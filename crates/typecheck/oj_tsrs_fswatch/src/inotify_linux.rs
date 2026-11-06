// inotify_linux.go: inotify backend (Linux).
//
// One inotify instance, a poll loop on its own thread (Go: goroutine) that also watches a wake-up pipe for
// shutdown. Each watched directory (every subdirectory in recursive mode) gets one watch descriptor; events are
// routed to the subscriptions registered for that descriptor. Go protects `subscriptions` with watcherBase.mu;
// here it has its own mutex, always taken after watcherBase.mu where Go holds that lock (subscribe and closeWatch
// run under it via watch_add_many / watch_remove; handleEvent and closeFDs take it first).

use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::walkdir_unix::walk_dir;
use crate::watcher::{
    DirWatch, DwKey, ERR_OVERFLOW, Error, PlatformWatcher, WatcherBase, WatcherImpl,
    err_watched_directory_removed,
};

const INOTIFY_MASK: u32 = libc::IN_CREATE
    | libc::IN_DELETE
    | libc::IN_DELETE_SELF
    | libc::IN_MODIFY
    | libc::IN_MOVE_SELF
    | libc::IN_MOVED_FROM
    | libc::IN_MOVED_TO
    | libc::IN_DONT_FOLLOW
    | libc::IN_ONLYDIR
    | libc::IN_EXCL_UNLINK;
const INOTIFY_BUFFER_SIZE: usize = 8192;

#[derive(Clone)]
struct InotifySubscription {
    path: String,
    watch_path: String,
    dir_watch: Arc<DirWatch>,
}

pub struct InotifyBackend {
    base: WatcherBase,

    pipe_read_fd: AtomicI32,
    pipe_write_fd: AtomicI32,
    inotify: AtomicI32,
    subscriptions: Mutex<FxHashMap<i32, Vec<InotifySubscription>>>, // multimap<wd, sub>
    ended: Mutex<bool>,
    ended_cv: Condvar,
}

// inotify_linux.go:66 (init)
pub fn init(mut w: PlatformWatcher) -> PlatformWatcher {
    w.factory = Some(|| Arc::new(new_inotify_backend()) as Arc<dyn WatcherImpl>);
    w
}

// inotify_linux.go:70
fn new_inotify_backend() -> InotifyBackend {
    InotifyBackend {
        base: WatcherBase::default(),
        pipe_read_fd: AtomicI32::new(-1),
        pipe_write_fd: AtomicI32::new(-1),
        inotify: AtomicI32::new(-1),
        subscriptions: Mutex::new(FxHashMap::default()),
        ended: Mutex::new(false),
        ended_cv: Condvar::new(),
    }
}

fn close_fd(fd: i32) {
    // SAFETY: closing an fd this backend opened.
    unsafe { libc::close(fd) };
}

fn last_errno() -> i32 {
    std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
}

impl InotifyBackend {
    fn run_loop(&self) -> Result<(), Error> {
        let mut pipe_fds = [-1i32; 2];
        // SAFETY: `pipe_fds` has room for two fds.
        if unsafe { libc::pipe2(pipe_fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) } < 0 {
            return Err(Error::new(format!(
                "unable to open pipe: {}",
                std::io::Error::last_os_error()
            )));
        }
        self.pipe_read_fd.store(pipe_fds[0], Ordering::SeqCst);
        self.pipe_write_fd.store(pipe_fds[1], Ordering::SeqCst);
        // SAFETY: plain syscall.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(Error::new(format!(
                "unable to initialize inotify: {}",
                std::io::Error::last_os_error()
            )));
        }
        self.inotify.store(fd, Ordering::SeqCst);

        let mut pollfds = [
            libc::pollfd { fd: pipe_fds[0], events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd, events: libc::POLLIN, revents: 0 },
        ];

        self.base.notify_started();

        let mut buf = vec![0u8; INOTIFY_BUFFER_SIZE];
        loop {
            // SAFETY: `pollfds` holds two valid entries.
            let r = unsafe { libc::poll(pollfds.as_mut_ptr(), 2, 500) };
            if r < 0 {
                if last_errno() == libc::EINTR {
                    continue;
                }
                return Err(Error::new(format!(
                    "unable to poll: {}",
                    std::io::Error::last_os_error()
                )));
            }
            if pollfds[0].revents != 0 {
                break;
            }
            if pollfds[1].revents != 0 {
                self.handle_events(&mut buf)?;
            }
        }
        Ok(())
    }

    // inotify_linux.go:136
    fn close_fds(&self) {
        let _base = self.base.mu.lock().unwrap();
        for fd in [&self.pipe_read_fd, &self.pipe_write_fd, &self.inotify] {
            let old = fd.swap(-1, Ordering::SeqCst);
            if old >= 0 {
                close_fd(old);
            }
        }
    }

    // inotify_linux.go:205
    fn watch_dir(
        &self,
        subs: &mut FxHashMap<i32, Vec<InotifySubscription>>,
        w: &Arc<DirWatch>,
        path: &str,
        watch_path: &str,
    ) -> Result<i32, Error> {
        let cpath =
            std::ffi::CString::new(watch_path).map_err(|_| Error::from_errno(libc::EINVAL))?;
        // SAFETY: `cpath` is NUL-terminated and outlives the call.
        let wd = unsafe {
            libc::inotify_add_watch(
                self.inotify.load(Ordering::SeqCst),
                cpath.as_ptr(),
                watch_mask(),
            )
        };
        if wd < 0 {
            return Err(Error::from_errno(last_errno()));
        }
        let sub = InotifySubscription {
            path: path.to_string(),
            watch_path: watch_path.to_string(),
            dir_watch: Arc::clone(w),
        };
        subs.entry(wd).or_default().push(sub);
        Ok(wd)
    }

    // inotify_linux.go:218
    fn handle_events(&self, buf: &mut [u8]) -> Result<(), Error> {
        let mut watchers_touched: Vec<Arc<DirWatch>> = Vec::new();
        let header = std::mem::size_of::<libc::inotify_event>();

        loop {
            // SAFETY: `buf` is a writable buffer of `buf.len()` bytes.
            let n = unsafe {
                libc::read(
                    self.inotify.load(Ordering::SeqCst),
                    buf.as_mut_ptr().cast::<libc::c_void>(),
                    buf.len(),
                )
            };
            if n < 0 {
                let errno = last_errno();
                if errno == libc::EAGAIN || errno == libc::EWOULDBLOCK {
                    break;
                }
                return Err(Error::new(format!(
                    "Error reading from inotify: {}",
                    std::io::Error::from_raw_os_error(errno)
                )));
            }
            if n == 0 {
                break;
            }
            let n = n as usize;
            let mut offset = 0;
            while offset < n {
                // SAFETY: the kernel writes whole `inotify_event` records; `offset` is at a record start.
                let ev: libc::inotify_event = unsafe {
                    std::ptr::read_unaligned(buf.as_ptr().add(offset).cast::<libc::inotify_event>())
                };
                let record_size = header + ev.len as usize;
                let mut name = String::new();
                if ev.len > 0 {
                    let mut name_bytes = &buf[offset + header..offset + record_size];
                    if let Some(i) = name_bytes.iter().position(|&c| c == 0) {
                        name_bytes = &name_bytes[..i];
                    }
                    name = tsrs_core::utf8::from_utf8_lossy(name_bytes).into_owned();
                }

                if ev.mask & libc::IN_Q_OVERFLOW != 0 {
                    let _base = self.base.mu.lock().unwrap();
                    let subs = self.subscriptions.lock().unwrap();
                    #[expect(
                        clippy::iter_over_hash_type,
                        reason = "sets the same ErrOverflow on every watch; `touched` only drives coalesced notifies; Go ranges the map too"
                    )]
                    for list in subs.values() {
                        for sub in list {
                            sub.dir_watch.events.set_error(ERR_OVERFLOW.into());
                            touch(&mut watchers_touched, &sub.dir_watch);
                        }
                    }
                    offset += record_size;
                    continue;
                }

                self.handle_event(&ev, &name, &mut watchers_touched);
                offset += record_size;
            }
        }
        for w in watchers_touched {
            w.notify();
        }
        Ok(())
    }

    // inotify_linux.go:278
    fn handle_event(&self, ev: &libc::inotify_event, name: &str, touched: &mut Vec<Arc<DirWatch>>) {
        let _base = self.base.mu.lock().unwrap();
        let mut subs = self.subscriptions.lock().unwrap();
        let list = subs.get(&ev.wd).cloned().unwrap_or_default();
        for s in &list {
            if self.handle_subscription(&mut subs, ev, name, s) {
                touch(touched, &s.dir_watch);
            }
        }
    }

    // inotify_linux.go:289
    fn handle_subscription(
        &self,
        subs: &mut FxHashMap<i32, Vec<InotifySubscription>>,
        ev: &libc::inotify_event,
        name: &str,
        sub: &InotifySubscription,
    ) -> bool {
        let w = &sub.dir_watch;
        let mut path = sub.path.clone();
        let mut watch_path = sub.watch_path.clone();
        let is_dir = ev.mask & libc::IN_ISDIR != 0;
        if !name.is_empty() {
            path = format!("{path}/{name}");
            watch_path = format!("{watch_path}/{name}");
        }

        // tsrs-only (`set_write_settling`): a close has no event of its own; it releases the callbacks.
        if ev.mask & libc::IN_CLOSE_WRITE != 0 {
            w.write_finished(&path);
            return false;
        }
        if ev.mask & (libc::IN_CREATE | libc::IN_MOVED_TO) != 0 {
            w.events.create(&path);
            if ev.mask & libc::IN_CREATE != 0
                && !is_dir
                && crate::watcher::write_settling()
                && is_regular_file(&watch_path)
            {
                w.write_started(&path);
            }
            if is_dir && w.recursive {
                let mut dirs = Vec::new();
                let _ = walk_dir(&watch_path, true, &mut |p, p_is_dir| {
                    if p_is_dir {
                        dirs.push(p.to_string());
                    }
                    Ok(())
                });
                for p in dirs {
                    let _ = self.watch_dir(subs, w, &w.display_path(&p), &p);
                }
            }
        } else if ev.mask & libc::IN_MODIFY != 0 {
            w.events.update(&path);
            if crate::watcher::write_settling() {
                w.write_started(&path);
            }
        } else if ev.mask
            & (libc::IN_DELETE | libc::IN_DELETE_SELF | libc::IN_MOVED_FROM | libc::IN_MOVE_SELF)
            != 0
        {
            let is_self_event = ev.mask & (libc::IN_DELETE_SELF | libc::IN_MOVE_SELF) != 0;
            if is_self_event && path != w.dir {
                return false;
            }
            if is_self_event || is_dir {
                let wds: Vec<i32> = subs.keys().copied().collect();
                for wd in wds {
                    let list = subs.get_mut(&wd).unwrap();
                    list.retain(|s| {
                        !(s.path == path
                            || (s.path.len() > path.len()
                                && s.path.as_bytes()[path.len()] == b'/'
                                && s.path.starts_with(&path)))
                    });
                    if list.is_empty() {
                        // SAFETY: removing a watch descriptor of our own inotify instance.
                        unsafe { libc::inotify_rm_watch(self.inotify.load(Ordering::SeqCst), wd) };
                        subs.remove(&wd);
                    }
                }
            }
            w.events.remove(&path);
            if is_self_event && path == w.dir {
                w.events.set_error(err_watched_directory_removed());
            }
        }
        true
    }
}

// tsrs-only: with write settling (`set_write_settling`), also the closes of files opened for writing.
fn watch_mask() -> u32 {
    if crate::watcher::write_settling() {
        INOTIFY_MASK | libc::IN_CLOSE_WRITE
    } else {
        INOTIFY_MASK
    }
}

// A created entry that a writer may still be writing: a regular file (not a directory, symlink or device, which get
// no close event).
#[expect(
    clippy::filetype_is_file,
    reason = "only a regular file is written and closed; the other types are what it excludes"
)]
fn is_regular_file(path: &str) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_file())
}

fn touch(touched: &mut Vec<Arc<DirWatch>>, w: &Arc<DirWatch>) {
    if !touched.iter().any(|t| Arc::ptr_eq(t, w)) {
        touched.push(Arc::clone(w));
    }
}

impl WatcherImpl for InotifyBackend {
    fn base(&self) -> &WatcherBase {
        &self.base
    }

    // inotify_linux.go:84
    fn start(self: Arc<Self>) -> Result<(), Error> {
        let result = self.run_loop();
        self.close_fds();
        *self.ended.lock().unwrap() = true;
        self.ended_cv.notify_all();
        result
    }

    // inotify_linux.go:154
    fn shutdown(&self) {
        let fd = self.pipe_write_fd.load(Ordering::SeqCst);
        if fd < 0 {
            return;
        }
        // SAFETY: writing one byte from a live buffer to our own pipe.
        unsafe { libc::write(fd, b"X".as_ptr().cast::<libc::c_void>(), 1) };
        let ended = self.ended.lock().unwrap();
        let _ended = self.ended_cv.wait_while(ended, |e| !*e).unwrap();
    }

    // inotify_linux.go:168
    fn subscribe(&self, w: &Arc<DirWatch>) -> Result<(), Error> {
        let mut subs = self.subscriptions.lock().unwrap();
        if !w.recursive {
            if let Err(err) = self.watch_dir(&mut subs, w, &w.dir, &w.physical_dir) {
                return Err(Error::wrap(
                    format!("inotify_add_watch on '{}' failed: {err}", w.dir),
                    &err,
                ));
            }
            return Ok(());
        }
        let result = walk_dir(&w.physical_dir, true, &mut |watch_path, is_dir| {
            if !is_dir {
                return Ok(());
            }
            let path = w.display_path(watch_path);
            if let Err(err) = self.watch_dir(&mut subs, w, &path, watch_path) {
                return Err(Error::wrap(
                    format!("inotify_add_watch on '{path}' failed: {err}"),
                    &err,
                ));
            }
            Ok(())
        });
        if let Err(err) = result {
            drop(subs);
            let _ = self.close_watch(w);
            return Err(err);
        }
        Ok(())
    }

    // inotify_linux.go:399
    fn close_watch(&self, w: &Arc<DirWatch>) -> Result<(), Error> {
        let mut subs = self.subscriptions.lock().unwrap();
        let key = DwKey(Arc::clone(w));
        let mut first_err = None;
        let wds: Vec<i32> = subs.keys().copied().collect();
        for wd in wds {
            let list = subs.get_mut(&wd).unwrap();
            let before = list.len();
            list.retain(|s| DwKey(Arc::clone(&s.dir_watch)) != key);
            if list.len() == before {
                continue;
            }
            if list.is_empty() {
                // SAFETY: removing a watch descriptor of our own inotify instance.
                if unsafe { libc::inotify_rm_watch(self.inotify.load(Ordering::SeqCst), wd) } < 0
                    && first_err.is_none()
                {
                    let err = Error::from_errno(last_errno());
                    first_err =
                        Some(Error::wrap(format!("unable to remove dirWatch: {err}"), &err));
                }
                subs.remove(&wd);
            }
        }
        first_err.map_or(Ok(()), Err)
    }
}
