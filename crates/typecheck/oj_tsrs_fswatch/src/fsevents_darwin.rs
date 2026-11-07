// fsevents_darwin.go: macOS FSEvents backend (event processing and stream lifecycle). The FFI plumbing is in
// fsevents_darwin_ffi.rs (see the deviation note there: the C callback classifies events on the stream's GCD
// queue thread instead of handing a payload to a goroutine through a pipe).
//
// Event classification (fsEventsCallback):
//   Each batch delivers arrays of paths, flags, and event IDs. The flags
//   bitmask may combine multiple states (created + modified + renamed).
//   Pure removes emit EventDelete with no syscalls. Renames and
//   remove+create combos do one Lstat to check existence (the kernel
//   reports some deletions as renames). Everything else emits EventUpdate
//   with no syscalls.
//
// Overflow:
//   flagMustScanSubDirs → ErrOverflow with detail (user/kernel/too-many).
//
// Root deletion:
//   Detected in the callback; the logical watch is marked terminated and
//   receives ErrWatchTerminated. The shared stream remains active for other
//   watches until the owner closes or reconciles the terminated watch.

use rustc_hash::{FxHashMap, FxHashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::fsevents_darwin_ffi::{
    FsEventStream, FsEventsCallbackPayload, StreamCallback, StreamStartError,
    create_and_start_stream, fs_events_get_current_event_id, new_stream_callback, teardown_stream,
};
use crate::pathcompare::ComparisonPath;
use crate::watcher::{
    DirWatch, DwKey, ERR_OVERFLOW, Error, PlatformWatcher, WatcherBase, WatcherImpl,
    err_watched_directory_removed,
};

// ----- FSEvents flag bits (from FSEvents.h) ------------------------------

pub const FLAG_MUST_SCAN_SUB_DIRS: u32 = 0x0000_0001;
pub const FLAG_USER_DROPPED: u32 = 0x0000_0002;
pub const FLAG_KERNEL_DROPPED: u32 = 0x0000_0004;
const FLAG_HISTORY_DONE: u32 = 0x0000_0010;

pub const FLAG_ITEM_CREATED: u32 = 0x0000_0100;
const FLAG_ITEM_REMOVED: u32 = 0x0000_0200;
const FLAG_ITEM_RENAMED: u32 = 0x0000_0800;
pub const FLAG_ITEM_IS_FILE: u32 = 0x0001_0000;
const FLAG_ITEM_IS_DIR: u32 = 0x0002_0000;
const FLAG_ITEM_IS_SYMLINK: u32 = 0x0004_0000;
const FLAG_ITEM_IS_HARDLINK: u32 = 0x0010_0000;
const FLAG_ITEM_IS_LAST_HARDLINK: u32 = 0x0020_0000;
const FLAG_ITEM_CLONED: u32 = 0x0040_0000;

const IGNORED_FLAGS: u32 = FLAG_ITEM_IS_HARDLINK
    | FLAG_ITEM_IS_LAST_HARDLINK
    | FLAG_ITEM_IS_SYMLINK
    | FLAG_ITEM_IS_DIR
    | FLAG_ITEM_IS_FILE
    | FLAG_ITEM_CLONED;

#[derive(Default)]
pub struct FseventsState {
    terminated: AtomicBool,
}

pub struct FseventsStream {
    pub stream: Mutex<Option<FsEventStream>>,
    pub cb: Mutex<Option<Box<StreamCallback>>>,
}

// ----- the watcherImpl -------------------------------------------------------

pub struct FsEventsBackend {
    base: WatcherBase,
    pub mu: Mutex<FsEventsState>,
}

#[derive(Default)]
pub struct FsEventsState {
    watches: FxHashMap<DwKey, Arc<FseventsState>>,
    // tsrs-only (Go has one `streams` list for every root; see `fseventsMaxRootStreams`): the stream of each root
    // (physical directory) that has its own, and the roots past the limit with the streams they share.
    pub root_streams: FxHashMap<String, Arc<FseventsStream>>,
    pub shared_roots: FxHashSet<String>,
    pub shared_streams: Vec<Arc<FseventsStream>>,
}

// fsevents_darwin.go:161 (init)
pub(crate) fn init(mut w: PlatformWatcher) -> PlatformWatcher {
    w.factory = Some(|| Arc::new(new_fs_events_backend()) as Arc<dyn WatcherImpl>);
    w.sequence = Some(fs_events_get_current_event_id);
    w
}

// fsevents_darwin.go:166
pub fn new_fs_events_backend() -> FsEventsBackend {
    FsEventsBackend { base: WatcherBase::default(), mu: Mutex::new(FsEventsState::default()) }
}

// fsevents_darwin.go:179
// checkWatcher mirrors the helper of the same name.
fn check_watcher(w: &Arc<DirWatch>) -> Result<(), Error> {
    let md = std::fs::metadata(&w.physical_dir)
        .map_err(|e| Error::path_error("stat", &w.physical_dir, e.raw_os_error().unwrap_or(0)))?;
    if !md.is_dir() {
        return Err(Error::from_errno(libc::ENOTDIR));
    }
    Ok(())
}

fn err_fsevents_user_dropped() -> Error {
    Error::wrap(
        format!("events were dropped by the FSEvents client: {}", ERR_OVERFLOW.0),
        &ERR_OVERFLOW.into(),
    )
}
fn err_fsevents_kernel_dropped() -> Error {
    Error::wrap(
        format!("events were dropped by the kernel: {}", ERR_OVERFLOW.0),
        &ERR_OVERFLOW.into(),
    )
}
fn err_fsevents_too_many() -> Error {
    Error::wrap(format!("too many events: {}", ERR_OVERFLOW.0), &ERR_OVERFLOW.into())
}

fn stream_start_error(e: StreamStartError) -> Error {
    match e {
        StreamStartError::CFStringCreateNull => Error::new("CFStringCreate returned NULL"),
        StreamStartError::CFArrayCreateNull => Error::new("CFArrayCreate returned NULL"),
        StreamStartError::StreamCreateNull => Error::new("FSEventStreamCreate returned NULL"),
        StreamStartError::StreamStartFailed => Error::new("error starting FSEvents stream"),
    }
}

const FSEVENTS_PATHS_PER_STREAM: usize = 512;

// tsrs-only: each watched root gets its own FSEventStream, up to this many; the roots past it share streams, as every
// root does in Go. When fseventsd falls behind a stream it drops that stream's events and reports the drop
// (UserDropped | MustScanSubDirs) on every root of the stream, which the watch turns into an overflow on every
// callback of each root. With one stream for every root, a burst of writes under one root (a checkout or an install
// in a project) overflowed every other root too, though nothing had changed there: a language server watching a
// project and the package directories it resolves into a package store outside it logged one overflow per watched
// package directory for each drop. A drop on one root's stream reaches no other stream (a minimal C client with two
// streams, under a burst that dropped events on one, saw nothing on the other). A process can start 512 streams (the
// 513th fails to start), so this leaves room for the shared streams and anything else in the process using FSEvents.
// Adding or removing a watch also restarts only its root's stream instead of every root's.
pub const FSEVENTS_MAX_ROOT_STREAMS: usize = 256;

#[derive(Clone)]
pub struct FseventsWatchSnapshot {
    pub w: Arc<DirWatch>,
    pub state: Arc<FseventsState>,
}

impl FsEventsState {
    // fsevents_darwin.go:215
    fn active_watches_locked(&self) -> Vec<FseventsWatchSnapshot> {
        let mut watches = Vec::with_capacity(self.watches.len());
        // stream paths are sorted after; each watch's dispatch only touches that watch's own state; Go ranges the map too
        watches.extend(self.watches.iter().filter_map(|(w, state)| {
            if state.terminated.load(Ordering::SeqCst) {
                return None;
            }
            Some(FseventsWatchSnapshot { w: Arc::clone(&w.0), state: Arc::clone(state) })
        }));
        watches
    }
}

impl FsEventsBackend {
    // fsevents_darwin.go:226
    fn start_streams(watches: &[FseventsWatchSnapshot]) -> Result<Vec<Arc<FseventsStream>>, Error> {
        start_fs_events_streams(watches, &|paths, watches| Self::start_stream(paths, watches))
    }

    // tsrs-only (Go restarts one set of streams for every root; see `fseventsMaxRootStreams`): restarts the streams
    // of `roots`, the physical directories whose watches changed, from the current watches. The new streams start
    // before the old ones stop, so no root misses events in between. On an error nothing is replaced.
    fn restart_root_streams(&self, roots: &FxHashSet<String>) -> Result<(), Error> {
        let st = self.mu.lock().unwrap();
        let watches = st.active_watches_locked();
        let mut root_stream_count = st.root_streams.len();
        let mut shared_roots = st.shared_roots.clone();
        let mut shared_changed = false;
        let mut has_stream = FxHashSet::default();
        has_stream.extend(roots.iter().filter(|root| st.root_streams.contains_key(*root)).cloned());
        drop(st);

        let mut sorted_roots: Vec<&String> = roots.iter().collect();
        sorted_roots.sort();
        let mut started: Vec<(String, Option<Arc<FseventsStream>>)> =
            Vec::with_capacity(sorted_roots.len());
        let stop_started = |started: &[(String, Option<Arc<FseventsStream>>)]| {
            for (_, stream) in started {
                if let Some(stream) = stream {
                    stop_fs_events_stream(stream);
                }
            }
        };
        let mut watches_by_root: FxHashMap<&str, Vec<FseventsWatchSnapshot>> = FxHashMap::default();
        for watch in watches.iter().filter(|watch| roots.contains(&watch.w.physical_dir)) {
            watches_by_root.entry(watch.w.physical_dir.as_str()).or_default().push(watch.clone());
        }
        for root in sorted_roots {
            let root_watches = watches_by_root.remove(root.as_str()).unwrap_or_default();
            if has_stream.contains(root) {
                if root_watches.is_empty() {
                    root_stream_count -= 1;
                    started.push((root.clone(), None));
                    continue;
                }
            } else if root_watches.is_empty() {
                shared_changed |= shared_roots.remove(root);
                continue;
            } else if shared_roots.contains(root) || root_stream_count >= FSEVENTS_MAX_ROOT_STREAMS
            {
                shared_roots.insert(root.clone());
                shared_changed = true;
                continue;
            } else {
                root_stream_count += 1;
            }
            match Self::start_stream(std::slice::from_ref(root), &root_watches) {
                Ok(stream) => started.push((root.clone(), stream)),
                Err(err) => {
                    stop_started(&started);
                    return Err(err);
                }
            }
        }
        let shared_streams = if shared_changed {
            let shared_watches: Vec<FseventsWatchSnapshot> = watches
                .iter()
                .filter(|watch| shared_roots.contains(&watch.w.physical_dir))
                .cloned()
                .collect();
            match Self::start_streams(&shared_watches) {
                Ok(streams) => Some(streams),
                Err(err) => {
                    stop_started(&started);
                    return Err(err);
                }
            }
        } else {
            None
        };

        let mut st = self.mu.lock().unwrap();
        let mut old_streams = Vec::new();
        for (root, stream) in started {
            old_streams.extend(match stream {
                Some(stream) => st.root_streams.insert(root, stream),
                None => st.root_streams.remove(&root),
            });
        }
        if let Some(shared_streams) = shared_streams {
            old_streams.extend(std::mem::replace(&mut st.shared_streams, shared_streams));
            st.shared_roots = shared_roots;
        }
        drop(st);
        stop_fs_events_streams(&old_streams);
        Ok(())
    }

    // fsevents_darwin.go:278
    // startStream creates and starts one FSEventStream watching all supplied paths.
    fn start_stream(
        paths: &[String],
        watches: &[FseventsWatchSnapshot],
    ) -> Result<Option<Arc<FseventsStream>>, Error> {
        if paths.is_empty() {
            return Ok(None);
        }
        let Some(mut cb) = new_stream_callback(watches) else {
            return Err(stream_start_error(StreamStartError::StreamCreateNull));
        };
        let stream = create_and_start_stream(paths, &mut cb).map_err(stream_start_error)?;
        Ok(Some(Arc::new(FseventsStream {
            stream: Mutex::new(Some(stream)),
            cb: Mutex::new(Some(cb)),
        })))
    }
}

// fsevents_darwin.go:230
fn start_fs_events_streams(
    watches: &[FseventsWatchSnapshot],
    start_stream: &dyn Fn(
        &[String],
        &[FseventsWatchSnapshot],
    ) -> Result<Option<Arc<FseventsStream>>, Error>,
) -> Result<Vec<Arc<FseventsStream>>, Error> {
    if watches.is_empty() {
        return Ok(Vec::new());
    }
    let mut seen = FxHashSet::with_capacity_and_hasher(watches.len(), rustc_hash::FxBuildHasher);
    let mut paths = Vec::with_capacity(watches.len());
    paths.extend(watches.iter().filter_map(|watch| {
        let path = watch.w.physical_dir.clone();
        if !seen.insert(path.clone()) {
            return None;
        }
        Some(path)
    }));
    paths.sort();

    if let Ok(stream) = start_stream(&paths, watches) {
        return Ok(stream.into_iter().collect());
    }

    let mut streams = Vec::with_capacity(paths.len().div_ceil(FSEVENTS_PATHS_PER_STREAM));
    let mut remaining_paths = &paths[..];
    while !remaining_paths.is_empty() {
        let chunk_len = remaining_paths.len().min(FSEVENTS_PATHS_PER_STREAM);
        let chunk_paths = &remaining_paths[..chunk_len];
        match start_stream(chunk_paths, &watches_for_fs_events_paths(watches, chunk_paths)) {
            Ok(stream) => streams.extend(stream),
            Err(err) => {
                stop_fs_events_streams(&streams);
                return Err(err);
            }
        }
        remaining_paths = &remaining_paths[chunk_len..];
    }
    Ok(streams)
}

// fsevents_darwin.go:265
fn watches_for_fs_events_paths(
    watches: &[FseventsWatchSnapshot],
    paths: &[String],
) -> Vec<FseventsWatchSnapshot> {
    if paths.is_empty() {
        return Vec::new();
    }
    watches
        .iter()
        .filter(|watch| paths.binary_search(&watch.w.physical_dir).is_ok())
        .cloned()
        .collect()
}

// fsevents_darwin.go:367
// The atomic Swap gates teardown so concurrent or repeated calls are safe:
// only the goroutine that observes a non-zero stream performs the cleanup.
fn stop_fs_events_streams(streams: &[Arc<FseventsStream>]) {
    for stream in streams {
        stop_fs_events_stream(stream);
    }
}

// fsevents_darwin.go:373
fn stop_fs_events_stream(state: &FseventsStream) {
    let Some(stream) = state.stream.lock().unwrap().take() else {
        return;
    };
    let mut cb = state.cb.lock().unwrap().take();
    teardown_stream(stream, cb.as_mut());
}

impl WatcherImpl for FsEventsBackend {
    fn base(&self) -> &WatcherBase {
        &self.base
    }

    // fsevents_darwin.go:174
    fn start(self: Arc<Self>) -> Result<(), Error> {
        self.base.notify_started();
        Ok(())
    }

    // fsevents_darwin.go:387
    // subscribe mirrors `fsEventsBackend::subscribe`.
    fn subscribe(&self, w: &Arc<DirWatch>) -> Result<(), Error> {
        self.subscribe_many(std::slice::from_ref(w)).unwrap()
    }

    // fsevents_darwin.go:391
    fn subscribe_many(&self, watches_to_add: &[Arc<DirWatch>]) -> Option<Result<(), Error>> {
        if watches_to_add.is_empty() {
            return Some(Ok(()));
        }
        let mut states: Vec<(Arc<DirWatch>, Arc<FseventsState>)> =
            Vec::with_capacity(watches_to_add.len());
        states = match watches_to_add.iter().try_fold(states, |mut states, w| {
            check_watcher(w)?;
            states.push((Arc::clone(w), Arc::new(FseventsState::default())));
            Ok(states)
        }) {
            Ok(states) => states,
            Err(err) => return Some(Err(err)),
        };

        let mut st = self.mu.lock().unwrap();
        for (w, state) in &states {
            *w.state.lock().unwrap() = Some(Arc::<FseventsState>::clone(state));
            st.watches.insert(DwKey(Arc::clone(w)), Arc::clone(state));
        }
        drop(st);

        let mut roots = FxHashSet::default();
        roots.extend(states.iter().map(|(w, _)| w.physical_dir.clone()));
        if let Err(err) = self.restart_root_streams(&roots) {
            let mut st = self.mu.lock().unwrap();
            for (w, state) in &states {
                let key = DwKey(Arc::clone(w));
                if st.watches.get(&key).is_some_and(|s| Arc::ptr_eq(s, state)) {
                    st.watches.remove(&key);
                    *w.state.lock().unwrap() = None;
                }
            }
            return Some(Err(err));
        }
        Some(Ok(()))
    }

    // fsevents_darwin.go:437
    // closeWatch mirrors `fsEventsBackend::closeWatch`.
    fn close_watch(&self, w: &Arc<DirWatch>) -> Result<(), Error> {
        let state = w.state.lock().unwrap().take().and_then(|s| s.downcast::<FseventsState>().ok());
        let Some(state) = state else {
            return Ok(());
        };
        state.terminated.store(true, Ordering::SeqCst);

        let mut st = self.mu.lock().unwrap();
        st.watches.remove(&DwKey(Arc::clone(w)));
        drop(st);

        let mut roots = FxHashSet::default();
        roots.insert(w.physical_dir.clone());
        self.restart_root_streams(&roots)
    }
}

// fsevents_darwin.go:467
// fsEventsCallback processes a batch of FSEvents (on the stream's serial dispatch queue thread).
pub(crate) fn fs_events_callback(cb: &StreamCallback, payload: &FsEventsCallbackPayload) {
    if payload.is_empty() {
        return;
    }

    let num_events = payload.num_events;
    let watches = &cb.watches;
    let mut touched: Vec<Arc<DirWatch>> = Vec::new();
    let mut touched_set: FxHashSet<DwKey> = FxHashSet::default();
    let mut touch = |w: &Arc<DirWatch>| {
        if touched_set.insert(DwKey(Arc::clone(w))) {
            touched.push(Arc::clone(w));
        }
    };

    let mut first_file: Option<String> = None;
    let mut burst = false;
    for i in 0..num_events {
        let flag = payload.flag(i);
        let event_id = payload.id(i);
        let path = payload.path_nfc(i);
        if path.is_empty() {
            continue;
        }
        let mut comparison = ComparisonPath::new(&path);

        let is_removed = flag & FLAG_ITEM_REMOVED != 0;
        let is_renamed = flag & FLAG_ITEM_RENAMED != 0;
        let is_created = flag & FLAG_ITEM_CREATED != 0;
        let is_done = flag & FLAG_HISTORY_DONE != 0;

        if flag & FLAG_MUST_SCAN_SUB_DIRS != 0 {
            let overflow = if flag & FLAG_USER_DROPPED != 0 {
                err_fsevents_user_dropped()
            } else if flag & FLAG_KERNEL_DROPPED != 0 {
                err_fsevents_kernel_dropped()
            } else {
                err_fsevents_too_many()
            };
            for watch in watches {
                if watch.state.terminated.load(Ordering::SeqCst) {
                    continue;
                }
                if fsevents_overflow_matches_prepared(&watch.w, &mut comparison) {
                    watch.w.events.set_error(overflow.clone());
                    touch(&watch.w);
                }
            }
        }

        if is_done {
            break;
        }

        if flag & !IGNORED_FLAGS == 0 {
            continue;
        }

        let raw_path = &path;
        let mut path_exists = false;
        let mut path_exists_known = false;
        // tsrs-only (`set_write_settling`): FSEvents has no close event, but a save that truncates the file and
        // writes it later (a slow writer, or `ftruncate` and then `write`) is seen empty by the time its event
        // arrives (FSEvents delivers about 12 ms after the write). An empty file holds the callbacks like an
        // unclosed one on inotify, until an event shows it non-empty or removed, or the hold times out. A save that
        // is not empty when its event arrives is not delayed.
        let file_len =
            (crate::watcher::write_settling() && flag & FLAG_ITEM_IS_FILE != 0).then(|| {
                if is_removed && !is_created {
                    None
                } else {
                    std::fs::metadata(raw_path)
                        .ok()
                        .filter(std::fs::Metadata::is_file)
                        .map(|m| m.len())
                }
            });
        let being_written = file_len.map(|len| len == Some(0));
        // tsrs-only (`set_write_settling`): a batch that changed two files that still exist is a tool writing many
        // files (a formatter, a checkout, a code generator), and the rest of its writes are still on the way.
        // Starting at once (the debounce's leading edge) would build on the first few and run a second cycle for the
        // rest, so the batch counts as a fire instead: the callbacks wait for 50 ms of quiet. A save, which changes
        // one file (an atomic save's temporary file is gone by the time its event arrives), is not delayed.
        if let Some(Some(_)) = file_len
            && !burst
        {
            match &first_file {
                None => first_file = Some(path.clone()),
                Some(first) => burst = *first != path,
            }
        }

        for watch in watches {
            if watch.state.terminated.load(Ordering::SeqCst) {
                continue;
            }
            let w = &watch.w;
            let Some(display_path) = fsevents_display_path_prepared(w, &mut comparison) else {
                continue;
            };

            // Skip events for the watched directory itself unless it's been
            // removed. fseventsd reports a change on the watched dir when a
            // child is added or removed; subscribers observe changes *within*
            // the directory, not the dir's own metadata churn.
            // (A removal of the dir is still propagated because WatcherBase
            // relies on it to tear down the stream.)
            if display_path == w.dir && !is_removed && !is_renamed {
                continue;
            }
            match being_written {
                Some(true) => w.write_started(&display_path),
                Some(false) => w.write_finished(&display_path),
                None => {}
            }

            if is_removed && !is_created {
                if display_path == w.dir {
                    w.events.remove_watch_root_at(&display_path, event_id);
                } else {
                    w.events.remove_at(&display_path, event_id);
                }
                if w.terminate_callbacks_for_deleted_root(
                    &display_path,
                    event_id,
                    &err_watched_directory_removed(),
                ) {
                    touch(w);
                }
                if display_path == w.dir {
                    watch.state.terminated.store(true, Ordering::SeqCst);
                    w.events.set_error(err_watched_directory_removed());
                }
            } else if is_renamed || (is_removed && is_created) {
                if !path_exists_known {
                    path_exists = std::fs::symlink_metadata(raw_path).is_ok();
                    path_exists_known = true;
                }
                if path_exists {
                    if display_path == w.dir {
                        w.events.update_watch_root_at(&display_path, event_id);
                    } else {
                        w.events.update_at(&display_path, event_id);
                    }
                } else {
                    if display_path == w.dir {
                        w.events.remove_watch_root_at(&display_path, event_id);
                    } else {
                        w.events.remove_at(&display_path, event_id);
                    }
                    if w.terminate_callbacks_for_deleted_root(
                        &display_path,
                        event_id,
                        &err_watched_directory_removed(),
                    ) {
                        touch(w);
                    }
                    if display_path == w.dir {
                        watch.state.terminated.store(true, Ordering::SeqCst);
                        w.events.set_error(err_watched_directory_removed());
                    }
                }
            } else if display_path == w.dir {
                w.events.update_watch_root_at(&display_path, event_id);
            } else {
                w.events.update_at(&display_path, event_id);
            }
            touch(w);
        }
    }

    if burst && let Some(w) = touched.first() {
        w.hold_leading_edge();
    }
    for w in touched {
        w.notify();
    }
}

// fsevents_darwin.go:609
#[expect(
    dead_code,
    reason = "only Go tests call it (fsevents_darwin_shared_test.go), and those are not ported"
)]
fn fsevents_display_path(w: &DirWatch, raw_path: &str) -> Option<String> {
    let mut path = ComparisonPath::new(raw_path);
    fsevents_display_path_prepared(w, &mut path)
}

// fsevents_darwin.go:614
fn fsevents_display_path_prepared(w: &DirWatch, raw_path: &mut ComparisonPath) -> Option<String> {
    let physical = ComparisonPath {
        path: w.physical_dir.clone(),
        folded: w.physical_dir_fold.clone(),
        ready: !w.physical_dir_fold.is_empty(),
        cache: None,
    };
    if let Some(path) = w.comparer.rebase_prepared(raw_path, &physical, &w.dir) {
        return Some(path);
    }
    if w.physical_dir != w.dir {
        let logical = ComparisonPath {
            path: w.dir.clone(),
            folded: w.dir_fold.clone(),
            ready: !w.dir_fold.is_empty(),
            cache: None,
        };
        return w.comparer.rebase_prepared(raw_path, &logical, &w.dir);
    }
    None
}

// fsevents_darwin.go:626
#[expect(
    dead_code,
    reason = "only Go tests call it (fsevents_darwin_shared_test.go), and those are not ported"
)]
fn fsevents_overflow_matches(w: &DirWatch, raw_path: &str) -> bool {
    let mut path = ComparisonPath::new(raw_path);
    fsevents_overflow_matches_prepared(w, &mut path)
}

// fsevents_darwin.go:631
fn fsevents_overflow_matches_prepared(w: &DirWatch, raw_path: &mut ComparisonPath) -> bool {
    let mut physical = ComparisonPath {
        path: w.physical_dir.clone(),
        folded: w.physical_dir_fold.clone(),
        ready: !w.physical_dir_fold.is_empty(),
        cache: None,
    };
    if w.comparer.suffix_prepared(&physical, raw_path).is_some() {
        return true;
    }
    if w.comparer.suffix_prepared(raw_path, &mut physical).is_some() {
        return true;
    }
    if w.physical_dir != w.dir {
        let mut logical = ComparisonPath {
            path: w.dir.clone(),
            folded: w.dir_fold.clone(),
            ready: !w.dir_fold.is_empty(),
            cache: None,
        };
        if w.comparer.suffix_prepared(&logical, raw_path).is_some() {
            return true;
        }
        return w.comparer.suffix_prepared(raw_path, &mut logical).is_some();
    }
    false
}
