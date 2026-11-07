use rustc_hash::{FxHashMap, FxHashSet};
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, LazyLock, Mutex};
use tsrs_core::tspath;

use crate::canonicalize::canonicalize_path;
use crate::debounce::Debounce;
use crate::event::{Event, EventKind, EventList};
use crate::pathcompare::{BytePathComparer, ComparisonCache, ComparisonPath};

// ----- errors -----------------------------------------------------------------
//
// Go's sentinel errors and `fmt.Errorf("%w")` chains -> `Error`: a message plus the sentinels it wraps (for
// `errors.Is`) and the errno it wraps, if any (`errors.Is(err, unix.ENOENT)`).

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Sentinel(pub &'static str);

#[derive(Clone, PartialEq, Eq, Debug)]
#[expect(
    clippy::error_impl_error,
    reason = "the public name mirrors Go's package-level error value used throughout the port"
)]
pub struct Error {
    message: String,
    wraps: Vec<Sentinel>,
    errno: Option<i32>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Error {
        Error { message: message.into(), wraps: Vec::new(), errno: None }
    }

    pub(crate) fn from_errno(errno: i32) -> Error {
        Error {
            message: std::io::Error::from_raw_os_error(errno).to_string(),
            wraps: Vec::new(),
            errno: Some(errno),
        }
    }

    #[cfg(unix)]
    #[expect(
        clippy::needless_pass_by_value,
        reason = "a conversion: callers pass it to `map_err`, which hands over the error by value"
    )]
    pub(crate) fn from_io(err: std::io::Error) -> Error {
        Error { message: err.to_string(), wraps: Vec::new(), errno: err.raw_os_error() }
    }

    // Go `&os.PathError{Op, Path, Err}`.
    pub(crate) fn path_error(op: &str, path: &str, errno: i32) -> Error {
        let inner = Error::from_errno(errno);
        Error {
            message: format!("{} {}: {}", op, path, inner.message),
            wraps: Vec::new(),
            errno: Some(errno),
        }
    }

    // `fmt.Errorf(format with one %w, inner)`: the message is `message`, the chain is inner's.
    pub fn wrap(message: impl Into<String>, inner: &Error) -> Error {
        Error { message: message.into(), wraps: inner.wraps.clone(), errno: inner.errno }
    }

    // `fmt.Errorf("%w: %w", a, b)` and similar: both chains.

    pub fn is(&self, s: Sentinel) -> bool {
        self.wraps.contains(&s)
    }

    pub fn is_errno(&self, errno: i32) -> bool {
        self.errno == Some(errno)
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<Sentinel> for Error {
    fn from(s: Sentinel) -> Error {
        Error { message: s.0.to_string(), wraps: vec![s], errno: None }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

static ERR_NIL_CALLBACK: Sentinel = Sentinel("fswatch: callback must not be nil");

// errRootPath is returned by WatchFile when the supplied path is a
// filesystem root with no parent directory to watch.
static ERR_ROOT_PATH: Sentinel = Sentinel("fswatch: cannot watch a root path");

// errNotAbsolute is returned by [Watcher.WatchDirectory] and
// [Watcher.WatchFile] when the supplied path is not absolute.
static ERR_NOT_ABSOLUTE: Sentinel = Sentinel("fswatch: path must be absolute");

// Returned when a requested watch may resolve to a network location.
pub static ERR_NETWORK_PATH: Sentinel = Sentinel("fswatch: network paths are not supported");

// ErrOverflow indicates that the kernel event queue overflowed and
// some filesystem changes were missed. The watch remains
// active; further events will continue to be delivered. Callers
// should treat this as a signal to rescan the watched directory.
pub static ERR_OVERFLOW: Sentinel = Sentinel("fswatch: event overflow; some changes were missed");

// ErrWatchTerminated indicates that the watch was terminated due to
// an unrecoverable error (e.g. the watched directory was deleted or
// the watch descriptor was revoked). No further events will be
// delivered. Call Close to release remaining state.
pub static ERR_WATCH_TERMINATED: Sentinel = Sentinel("fswatch: watch terminated");

// ErrUnavailable indicates that a requested watcher is not
// available on the current platform.
pub static ERR_UNAVAILABLE: Sentinel = Sentinel("fswatch: watcher not available on this platform");

// ErrFilesystemUnsupported indicates that the active watcher backend cannot
// operate on the target filesystem, even though the backend is available on
// the current platform.
static ERR_FILESYSTEM_UNSUPPORTED: Sentinel =
    Sentinel("fswatch: watcher backend unsupported on this filesystem");

// `fmt.Errorf("%w: watched directory removed", ErrWatchTerminated)`.
#[cfg(unix)]
pub(crate) fn err_watched_directory_removed() -> Error {
    Error::wrap(
        format!("{}: watched directory removed", ERR_WATCH_TERMINATED.0),
        &ERR_WATCH_TERMINATED.into(),
    )
}

// ----- public API ---------------------------------------------------------------

// WatchCallback receives batched filesystem events. Rapid changes
// are coalesced before delivery.
//
// For a given Watch, the callback is never invoked concurrently
// with itself. It runs on a library goroutine, not the caller's.
//
// When err is non-nil, use [errors.Is] to check for [ErrOverflow]
// (recoverable) or [ErrWatchTerminated] (terminal).
pub type WatchCallback = Arc<dyn Fn(Vec<Event>, Option<Error>) + Send + Sync>;

pub type IgnoreFunc = Arc<dyn Fn(&str) -> bool + Send + Sync>;

// Watcher represents a filesystem watching implementation.
// Use one of the constructor functions ([Inotify], [FSEvents], [Kqueue],
// [Windows]) to obtain a value, or [Default] for the platform default.
//
// All watchers exist on every platform. Subscribing with a watcher that
// is not supported on the current OS returns [ErrUnavailable].
pub trait Watcher: Send + Sync {
    // Name returns a stable identifier ("inotify", "fsevents", "kqueue",
    // "windows").
    fn name(&self) -> &str;
    // Available reports whether this watcher works on the current OS.
    fn available(&self) -> bool;
    // HasFastRecursiveBackend reports whether this watcher supports efficient
    // recursive watching without requiring a full userspace tree walk. This is
    // true for Windows (ReadDirectoryChangesW subtree mode) and macOS FSEvents
    // (inherently recursive), and false for all other backends.
    fn has_fast_recursive_backend(&self) -> bool;
    // WatchDirectory watches dir for changes, calling fn with batched
    // events. By default, only direct children are watched. Use
    // [WithRecursive] to watch the entire directory tree.
    // dir must be an absolute path to an existing directory. If dir is a
    // symlink or reparse point to a directory, the OS subscription follows
    // the target directory but delivered event paths remain rooted at dir.
    // Userspace recursive traversal does not follow symlinked descendant
    // directories.
    // Returns [ErrUnavailable] if the watcher is not supported on
    // the current platform.
    fn watch_directory(
        &self,
        dir: &str,
        f: WatchCallback,
        opts: Vec<WatchOption>,
    ) -> Result<Box<dyn Watch>, Error>;
    // WatchDirectories watches multiple directories as a batch. It has the
    // same semantics as calling [Watcher.WatchDirectory] for each request, but
    // lets backends arm the underlying OS watches once for the whole batch.
    // Returned watches are in the same order as requests.
    fn watch_directories(
        &self,
        requests: Vec<WatchDirectoryRequest>,
    ) -> Result<Vec<Box<dyn Watch>>, Error>;
    // WatchFile watches a single file for changes, calling fn with
    // batched events. path must be an absolute path. The file does not
    // need to exist at subscribe time; its creation will be reported.
    // The parent directory must exist.
    //
    // Multiple WatchFile calls for files in the same directory
    // share a single OS watch on the parent directory.
    //
    // If the parent directory is deleted, [ErrWatchTerminated] is
    // delivered and the watch is dead.
    fn watch_file(&self, path: &str, f: WatchCallback) -> Result<Box<dyn Watch>, Error>;
}

// WatchOption configures a watch. (Go: an interface with one implementation per option.)
#[derive(Clone)]
pub enum WatchOption {
    Ignore(IgnoreFunc),
    Recursive,
    // fileOption defers the file filter until the parent directory's comparer is
    // available, so WatchFile does not need a second filesystem query.
    File(String),
}

impl WatchOption {
    fn apply_watch_option(&self, opts: &mut WatchOptions) {
        match self {
            WatchOption::Ignore(f) => opts.ignore = Some(Arc::clone(f)),
            WatchOption::Recursive => opts.recursive = true,
            WatchOption::File(path) => opts.file.clone_from(path),
        }
    }
}

// WatchDirectoryRequest describes one directory subscription in a
// [Watcher.WatchDirectories] batch.
#[derive(Clone)]
pub struct WatchDirectoryRequest {
    pub dir: String,
    pub callback: Option<WatchCallback>,
    pub options: Vec<WatchOption>,
}

#[derive(Default)]
struct WatchOptions {
    ignore: Option<IgnoreFunc>,
    recursive: bool,
    file: String,
}

// watcher.go:164
// WithIgnore returns a [WatchOption] that filters events before delivery.
// If the function returns true for a path, events for that path are
// silently dropped. The filtering is per-subscriber; multiple watches
// on the same directory may have different ignore functions.
pub fn with_ignore(f: IgnoreFunc) -> WatchOption {
    WatchOption::Ignore(f)
}

// watcher.go:183
// WithRecursive returns a [WatchOption] that enables recursive watching
// of the entire directory tree. Without this option,
// [Watcher.WatchDirectory] watches only direct children of dir.
pub fn with_recursive() -> WatchOption {
    WatchOption::Recursive
}

// Watch represents a live watch. Close stops watching
// and releases resources. It is idempotent.
pub trait Watch: Send + Sync {
    fn close(&self) -> Result<(), Error>;
}

// Package-level watcher instances. Platform init() functions set the factory.
static INOTIFY_WATCHER: LazyLock<PlatformWatcher> = LazyLock::new(|| {
    let w = PlatformWatcher::new("inotify");
    #[cfg(target_os = "linux")]
    let w = crate::inotify_linux::init(w);
    w
});
static FSEVENTS_WATCHER: LazyLock<PlatformWatcher> = LazyLock::new(|| {
    let w = PlatformWatcher::new("fsevents");
    #[cfg(target_os = "macos")]
    let w = crate::fsevents_darwin::init(w);
    w
});
static KQUEUE_WATCHER: LazyLock<PlatformWatcher> = LazyLock::new(|| {
    let w = PlatformWatcher::new("kqueue");
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    ))]
    let w = crate::kqueue::init(w);
    w
});
// windows.go (ReadDirectoryChangesW) is not ported: Windows builds get an unavailable watcher.
static WINDOWS_WATCHER: LazyLock<PlatformWatcher> =
    LazyLock::new(|| PlatformWatcher::new("windows"));
// fanotify_linux.go is not ported: fanotify().Available() is false, so Linux uses inotify (Go's Default() falls
// back to inotify the same way on kernels without fanotify).
static FANOTIFY_WATCHER: LazyLock<PlatformWatcher> =
    LazyLock::new(|| PlatformWatcher::new("fanotify"));
static FANOTIFY_FALLBACK_WATCHER: LazyLock<FallbackWatcher> =
    LazyLock::new(|| FallbackWatcher { primary: &*FANOTIFY_WATCHER, secondary: &*INOTIFY_WATCHER });
static UNSUPPORTED_WATCHER: LazyLock<PlatformWatcher> =
    LazyLock::new(|| PlatformWatcher::new("unsupported"));

// watcher.go:203
// AllWatchers returns a fresh slice listing every watcher backend the package
// knows about. Use [Watcher.Available] to check which ones work on the current
// OS.
pub fn all_watchers() -> Vec<&'static dyn Watcher> {
    vec![
        &*INOTIFY_WATCHER,
        &*FSEVENTS_WATCHER,
        &*KQUEUE_WATCHER,
        &*WINDOWS_WATCHER,
        &*FANOTIFY_FALLBACK_WATCHER,
    ]
}

// watcher.go:214
// Inotify returns the inotify watcher (Linux and Android).
pub fn inotify() -> &'static dyn Watcher {
    &*INOTIFY_WATCHER
}

// watcher.go:217
// FSEvents returns the FSEvents watcher (macOS).
pub fn fsevents() -> &'static dyn Watcher {
    &*FSEVENTS_WATCHER
}

// watcher.go:220
// Kqueue returns the kqueue watcher (macOS, FreeBSD, and other BSDs).
pub fn kqueue() -> &'static dyn Watcher {
    &*KQUEUE_WATCHER
}

// watcher.go:223
// Windows returns the ReadDirectoryChangesW watcher (Windows).
fn windows() -> &'static dyn Watcher {
    &*WINDOWS_WATCHER
}

// watcher.go:227
// Fanotify returns the fanotify watcher (Linux, kernel ≥ 5.13). Directories on
// filesystems that don't support fanotify watches automatically use inotify instead.
fn fanotify() -> &'static dyn Watcher {
    &*FANOTIFY_FALLBACK_WATCHER
}

static WRITE_SETTLING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// tsrs-only, process-wide: on backends that report when a writer closes a file (inotify), callbacks wait
/// until every file being written has been closed, then fire at once.
///
/// Go fires the first event after a quiet period immediately, so a save is seen right after its truncation
/// and the file is read half-written; batching for a fixed time instead adds that time to every save.
/// `FSEvents` has no close event: there a file that is empty when its event arrives counts as being written
/// (a truncation whose data has not followed yet), until an event shows it non-empty. kqueue ignores the
/// setting. For the CLI's `--watch`, which turns it on before it watches anything.
pub fn set_write_settling(on: bool) {
    WRITE_SETTLING.store(on, std::sync::atomic::Ordering::Relaxed);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn write_settling() -> bool {
    // Relaxed: set before the first watch is made; the backend threads that read it took the locks that publish
    // that watch, which order the store before this load.
    WRITE_SETTLING.load(std::sync::atomic::Ordering::Relaxed)
}

// watcher.go:230
// Default returns the recommended watcher for the current OS.
pub fn default() -> &'static dyn Watcher {
    if cfg!(target_os = "linux") {
        if fanotify().available() {
            return fanotify();
        }
        inotify()
    } else if cfg!(target_os = "android") {
        inotify()
    } else if cfg!(target_os = "macos") {
        if fsevents().available() {
            return fsevents();
        }
        kqueue()
    } else if cfg!(target_os = "windows") {
        windows()
    } else if cfg!(any(
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        target_os = "dragonfly"
    )) {
        kqueue()
    } else {
        &*UNSUPPORTED_WATCHER
    }
}

// fallbackWatcher keeps the primary backend for supported filesystems while
// routing individual unsupported watches to the secondary backend.
pub struct FallbackWatcher {
    primary: &'static dyn Watcher,
    secondary: &'static dyn Watcher,
}

impl Watcher for FallbackWatcher {
    fn name(&self) -> &str {
        self.primary.name()
    }
    fn available(&self) -> bool {
        self.primary.available()
    }
    fn has_fast_recursive_backend(&self) -> bool {
        self.primary.has_fast_recursive_backend()
    }

    // watcher.go:263
    fn watch_directory(
        &self,
        dir: &str,
        f: WatchCallback,
        opts: Vec<WatchOption>,
    ) -> Result<Box<dyn Watch>, Error> {
        let mut watches = self.watch_directories(vec![WatchDirectoryRequest {
            dir: dir.to_string(),
            callback: Some(f),
            options: opts,
        }])?;
        Ok(watches.remove(0))
    }

    // watcher.go:275
    fn watch_directories(
        &self,
        requests: Vec<WatchDirectoryRequest>,
    ) -> Result<Vec<Box<dyn Watch>>, Error> {
        match self.primary.watch_directories(requests.clone()) {
            Ok(watches) => return Ok(watches),
            Err(err) if !err.is(ERR_FILESYSTEM_UNSUPPORTED) => return Err(err),
            Err(_) => {}
        }

        let mut watches: Vec<Box<dyn Watch>> = Vec::with_capacity(requests.len());
        for request in requests {
            let f = request.callback.clone().ok_or_else(|| Error::from(ERR_NIL_CALLBACK))?;
            let mut result =
                self.primary.watch_directory(&request.dir, Arc::clone(&f), request.options.clone());
            if matches!(&result, Err(err) if err.is(ERR_FILESYSTEM_UNSUPPORTED)) {
                result = self.secondary.watch_directory(&request.dir, f, request.options.clone());
            }
            match result {
                Ok(watch) => watches.push(watch),
                Err(err) => {
                    for watch in watches.iter().rev() {
                        let _ = watch.close();
                    }
                    return Err(Error::wrap(
                        format!("fswatch: failed to watch directory {:?}: {}", request.dir, err),
                        &err,
                    ));
                }
            }
        }
        Ok(watches)
    }

    // watcher.go:300
    fn watch_file(&self, path: &str, f: WatchCallback) -> Result<Box<dyn Watch>, Error> {
        let watch = self.primary.watch_file(path, Arc::clone(&f));
        if matches!(&watch, Err(err) if err.is(ERR_FILESYSTEM_UNSUPPORTED)) {
            return self.secondary.watch_file(path, f);
        }
        watch
    }
}

// Pointer-identity key for `*dirWatch` map keys.
#[derive(Clone)]
pub struct DwKey(pub(crate) Arc<DirWatch>);

impl PartialEq for DwKey {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for DwKey {}
impl std::hash::Hash for DwKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (Arc::as_ptr(&self.0) as usize).hash(state);
    }
}

pub type WatcherFactory = fn() -> Arc<dyn WatcherImpl>;

// watcher is the concrete implementation of [Watcher]. Each platform
// watcher is a package-level *watcher whose factory is set by the
// platform's init() function.
pub struct PlatformWatcher {
    pub(crate) name: &'static str,
    mu: Mutex<WatcherState>,
    pub(crate) factory: Option<WatcherFactory>, // nil if not available on this platform
    pub(crate) sequence: Option<fn() -> u64>,
}

#[derive(Default)]
struct WatcherState {
    impl_: Option<Arc<dyn WatcherImpl>>,
    dir_watches: FxHashMap<String, Arc<DirWatch>>,
    debounce: Option<Arc<Debounce>>, // lazily created in getOrCreateDirWatch
}

const RECURSIVE_CONSOLIDATE_THRESHOLD: usize = 10;

impl PlatformWatcher {
    pub fn new(name: &'static str) -> PlatformWatcher {
        PlatformWatcher {
            name,
            mu: Mutex::new(WatcherState::default()),
            factory: None,
            sequence: None,
        }
    }

    // watcher.go:340
    fn can_share_recursive_dir_watches(&self) -> bool {
        // TODO: Re-enable this for Windows once coalesced recursive watches have
        // more real-world bake time.
        self.name == "fsevents"
    }

    // watcher.go:346
    fn get_impl(&self) -> Result<Arc<dyn WatcherImpl>, Error> {
        let st = self.mu.lock().unwrap();
        if let Some(impl_) = &st.impl_ {
            return Ok(Arc::clone(impl_));
        }
        let factory = self.factory;
        drop(st);

        let Some(factory) = factory else {
            return Err(ERR_UNAVAILABLE.into());
        };

        let impl_ = factory();
        run(&impl_)?;

        let mut st = self.mu.lock().unwrap();
        if let Some(existing) = &st.impl_ {
            let existing = Arc::clone(existing);
            drop(st);
            impl_.shutdown();
            return Ok(existing);
        }
        st.impl_ = Some(Arc::clone(&impl_));
        Ok(impl_)
    }

    // watcher.go:377
    fn key_for_dir_watch(dir: &str, recursive: bool) -> String {
        if recursive {
            return format!("{dir}\x00recursive");
        }
        dir.to_string()
    }

    // watcher.go:384
    fn find_covering_recursive_watch_locked(
        st: &WatcherState,
        dir: &str,
        physical_dir: &str,
        comparer: BytePathComparer,
    ) -> Option<Arc<DirWatch>> {
        let mut best: Option<&Arc<DirWatch>> = None;
        #[expect(
            clippy::iter_over_hash_type,
            reason = "the longest covering dir is unique: candidates are prefixes of one path, one key per dir; Go ranges the map too"
        )]
        for dw in st.dir_watches.values() {
            if !dw.recursive
                || dw.comparer != comparer
                || !is_in_directory_or_self(&dw.dir, dir)
                || !is_in_directory_or_self(&dw.physical_dir, physical_dir)
            {
                continue;
            }
            if best.is_none_or(|b| dw.dir.len() > b.dir.len()) {
                best = Some(dw);
            }
        }
        best.cloned()
    }

    // watcher.go:397
    fn find_consolidation_dir_locked(
        &self,
        st: &WatcherState,
        dir: &str,
        physical_dir: &str,
    ) -> String {
        if !self.can_share_recursive_dir_watches() {
            return String::new();
        }
        let mut dir = dir.to_string();
        let mut parent = filepath_dir(&dir);
        while parent != dir && parent != "." {
            if filepath_dir(&parent) == parent {
                break;
            }
            let physical_parent = physical_dir_for(&parent);
            if !is_in_directory_or_self(&physical_parent, physical_dir) {
                return String::new();
            }
            // tsrs-only: Go counts the watches under `parent`; this counts the children of `parent` that hold one (the
            // new watch's among them), so consolidation joins siblings. Counting watches let one crowded subtree carry
            // any directory above it: a language server's watches on the package directories of a package store
            // under the home directory consolidated into the store, and the next watch elsewhere in the home
            // directory (the project) consolidated into the home directory itself, which then streamed every write
            // in it to the process, and each drop of events on that stream became an overflow on every watch.
            let mut children: FxHashSet<&str> = FxHashSet::default();
            children.insert(&dir);
            // only collects the distinct children; returns `parent` regardless of traversal order
            let enough = st
                .dir_watches
                .values()
                .filter(|dw| {
                    dw.dir != parent
                        && is_in_directory_or_self(&parent, &dw.dir)
                        && is_in_directory_or_self(&physical_parent, &dw.physical_dir)
                })
                .any(|dw| {
                    children.insert(child_on_path(&parent, &dw.dir));
                    children.len() >= RECURSIVE_CONSOLIDATE_THRESHOLD
                });
            if enough {
                return parent;
            }
            let next = filepath_dir(&parent);
            if next == parent {
                break;
            }
            dir = parent;
            parent = next;
        }
        String::new()
    }

    // watcher.go:429
    pub fn get_or_create_dir_watch(
        &self,
        dir: &str,
        physical_dir: &str,
        recursive: bool,
        comparer: BytePathComparer,
    ) -> Result<Arc<DirWatch>, Error> {
        let mut st = self.mu.lock().unwrap();
        if st.debounce.is_none() {
            st.debounce = Some(Debounce::new());
        }
        let (mut dir, mut physical_dir, mut recursive) =
            (dir.to_string(), physical_dir.to_string(), recursive);

        if self.can_share_recursive_dir_watches() {
            if let Some(dw) =
                Self::find_covering_recursive_watch_locked(&st, &dir, &physical_dir, comparer)
            {
                return Ok(dw);
            }
            let consolidation_dir = self.find_consolidation_dir_locked(&st, &dir, &physical_dir);
            if !consolidation_dir.is_empty() {
                let parent_comparer = self.path_comparer(&consolidation_dir)?;
                if parent_comparer == comparer {
                    dir = consolidation_dir;
                    physical_dir = physical_dir_for(&dir);
                    recursive = true;
                    if let Some(dw) = Self::find_covering_recursive_watch_locked(
                        &st,
                        &dir,
                        &physical_dir,
                        comparer,
                    ) {
                        return Ok(dw);
                    }
                }
            }
        }

        let key = Self::key_for_dir_watch(&dir, recursive);
        if let Some(dw) = st.dir_watches.get(&key) {
            return Ok(Arc::clone(dw));
        }
        let dw = DirWatch::new(
            dir,
            physical_dir,
            st.debounce.as_ref().unwrap(),
            comparer,
            self.sequence,
            recursive,
        );
        st.dir_watches.insert(key, Arc::clone(&dw));
        Ok(dw)
    }

    // watcher.go:471
    fn remove_dir_watch(&self, dw: &Arc<DirWatch>) {
        let mut st = self.mu.lock().unwrap();
        let key = Self::key_for_dir_watch(&dw.dir, dw.recursive);
        if let Some(existing) = st.dir_watches.get(&key)
            && Arc::ptr_eq(existing, dw)
        {
            st.dir_watches.remove(&key);
            dw.destroy_debounce();
        }
    }
}

impl Watcher for PlatformWatcher {
    fn name(&self) -> &str {
        self.name
    }

    fn available(&self) -> bool {
        self.factory.is_some()
    }

    // watcher.go:331
    // HasFastRecursiveBackend implements [Watcher.HasFastRecursiveBackend].
    fn has_fast_recursive_backend(&self) -> bool {
        matches!(self.name, "windows" | "fsevents")
    }

    // watcher.go:481
    fn watch_directory(
        &self,
        dir: &str,
        f: WatchCallback,
        opts: Vec<WatchOption>,
    ) -> Result<Box<dyn Watch>, Error> {
        let mut watches = self.watch_directories(vec![WatchDirectoryRequest {
            dir: dir.to_string(),
            callback: Some(f),
            options: opts,
        }])?;
        Ok(watches.remove(0))
    }

    // watcher.go:493
    fn watch_directories(
        &self,
        requests: Vec<WatchDirectoryRequest>,
    ) -> Result<Vec<Box<dyn Watch>>, Error> {
        struct PreparedWatch {
            dw: Arc<DirWatch>,
            id: u64,
        }

        if !self.available() {
            return Err(ERR_UNAVAILABLE.into());
        }
        if requests.is_empty() {
            return Ok(Vec::new());
        }

        let mut prepared: Vec<PreparedWatch> = Vec::with_capacity(requests.len());
        let mut unique_dir_watches: Vec<Arc<DirWatch>> = Vec::with_capacity(requests.len());
        let mut seen_dir_watches: FxHashSet<DwKey> =
            FxHashSet::with_capacity_and_hasher(requests.len(), rustc_hash::FxBuildHasher);
        let rollback = |prepared: &[PreparedWatch]| {
            for p in prepared.iter().rev() {
                p.dw.unwatch(p.id);
                p.dw.unref(self);
            }
        };

        for request in requests {
            let Some(f) = request.callback else {
                rollback(&prepared);
                return Err(ERR_NIL_CALLBACK.into());
            };
            if tspath::is_network_path(&request.dir) {
                rollback(&prepared);
                return Err(ERR_NETWORK_PATH.into());
            }
            let dir = filepath_clean(&request.dir);
            if !dir.starts_with('/') {
                rollback(&prepared);
                return Err(ERR_NOT_ABSOLUTE.into());
            }
            let dir = canonicalize_path(&dir);
            if self.can_share_recursive_dir_watches()
                && let Err(err) = validate_watch_directory(&dir)
            {
                rollback(&prepared);
                return Err(err);
            }
            let physical_dir = physical_dir_for(&dir);

            let mut sopts = WatchOptions::default();
            for o in &request.options {
                o.apply_watch_option(&mut sopts);
            }

            let comparer = match self.path_comparer(&dir) {
                Ok(c) => c,
                Err(err) => {
                    rollback(&prepared);
                    return Err(err);
                }
            };
            let dw = match self.get_or_create_dir_watch(
                &dir,
                &physical_dir,
                sopts.recursive,
                comparer,
            ) {
                Ok(dw) => dw,
                Err(err) => {
                    rollback(&prepared);
                    return Err(err);
                }
            };
            let id =
                dw.add_callback(&dir, &physical_dir, sopts.recursive, f, sopts.ignore, &sopts.file);
            prepared.push(PreparedWatch { dw: Arc::clone(&dw), id });
            if seen_dir_watches.insert(DwKey(Arc::clone(&dw))) {
                unique_dir_watches.push(dw);
            }
        }

        let impl_ = match self.get_impl() {
            Ok(impl_) => impl_,
            Err(err) => {
                rollback(&prepared);
                return Err(err);
            }
        };
        if let Err(err) = watch_add_many(&impl_, &unique_dir_watches) {
            rollback(&prepared);
            return Err(err);
        }

        let self_static: &'static PlatformWatcher = self.as_static();
        Ok(prepared
            .into_iter()
            .map(|p| {
                Box::new(WatchHandle {
                    mu: Mutex::new(false),
                    w: self_static,
                    dw: p.dw,
                    impl_: Arc::clone(&impl_),
                    id: p.id,
                }) as Box<dyn Watch>
            })
            .collect())
    }

    // watcher.go:589
    fn watch_file(&self, path: &str, f: WatchCallback) -> Result<Box<dyn Watch>, Error> {
        if !self.available() {
            return Err(ERR_UNAVAILABLE.into());
        }
        if tspath::is_network_path(path) {
            return Err(ERR_NETWORK_PATH.into());
        }
        let path = filepath_clean(path);
        if !path.starts_with('/') {
            return Err(ERR_NOT_ABSOLUTE.into());
        }
        let path = canonicalize_path(&path);
        let dir = filepath_dir(&path);
        if dir == path {
            return Err(ERR_ROOT_PATH.into());
        }

        self.watch_directory(&dir, f, vec![WatchOption::File(path)])
    }
}

impl PlatformWatcher {
    // Every `watcher` is one of the package-level statics.
    fn as_static(&self) -> &'static PlatformWatcher {
        if let Some(result) = ([
            &*INOTIFY_WATCHER,
            &*FSEVENTS_WATCHER,
            &*KQUEUE_WATCHER,
            &*WINDOWS_WATCHER,
            &*FANOTIFY_WATCHER,
            &*UNSUPPORTED_WATCHER,
        ])
        .into_iter()
        .find(|w| std::ptr::eq(*w, self))
        {
            return result;
        }
        unreachable!("fswatch: watcher is not a package-level instance")
    }
}

// watcher.go:578
fn validate_watch_directory(dir: &str) -> Result<(), Error> {
    let md = std::fs::metadata(dir)
        .map_err(|e| Error::path_error("stat", dir, e.raw_os_error().unwrap_or(0)))?;
    if !md.is_dir() {
        return Err(Error::from_errno(libc::ENOTDIR));
    }
    Ok(())
}

struct WatchHandle {
    mu: Mutex<bool>, // cancelled
    w: &'static PlatformWatcher,
    dw: Arc<DirWatch>,
    impl_: Arc<dyn WatcherImpl>,
    id: u64,
}

impl Watch for WatchHandle {
    // watcher.go:620
    fn close(&self) -> Result<(), Error> {
        let mut cancelled = self.mu.lock().unwrap();
        if *cancelled {
            return Ok(());
        }
        *cancelled = true;
        let last = self.dw.unwatch(self.id);
        if last {
            watch_remove(&self.impl_, &self.dw);
            self.dw.unref(self.w);
        }
        Ok(())
    }
}

// watcherImpl is the internal interface implemented by each platform watcher.
// Go embeds `watcherBase` (shared watch tracking and lifecycle) in each backend and dispatches through
// `self`; here `base()` returns the embedded value and the shared logic is the free functions below.
pub trait WatcherImpl: Send + Sync + 'static {
    fn base(&self) -> &WatcherBase;
    // Runs on its own thread (Go: a goroutine started by watcherBase.run) and returns when the backend stops.
    fn start(self: Arc<Self>) -> Result<(), Error>;
    fn shutdown(&self) {}
    fn subscribe(&self, w: &Arc<DirWatch>) -> Result<(), Error>;
    // Go: the optional `subscribeMany` interface; None = not implemented.
    fn subscribe_many(&self, _watches: &[Arc<DirWatch>]) -> Option<Result<(), Error>> {
        None
    }
    fn close_watch(&self, w: &Arc<DirWatch>) -> Result<(), Error>;
}

// watcherBase provides shared watch-tracking and lifecycle logic.
// Concrete backends embed it and override subscribe/closeWatch/start.
#[derive(Default)]
pub struct WatcherBase {
    pub(crate) mu: Mutex<WatcherBaseState>,
    started: Mutex<bool>,
    started_cv: Condvar,
}

#[derive(Default)]
pub struct WatcherBaseState {
    subscriptions: FxHashSet<DwKey>,
    start_err: Option<Error>,
}

impl WatcherBase {
    // watcher.go:669
    pub(crate) fn notify_started(&self) {
        let mut started = self.started.lock().unwrap();
        if !*started {
            *started = true;
            self.started_cv.notify_all();
        }
    }

    // watcher.go:701
    fn handle_start_error(&self, err: &Error) {
        let mut st = self.mu.lock().unwrap();
        st.start_err = Some(err.clone());
        let subs: Vec<Arc<DirWatch>> = st.subscriptions.iter().map(|k| Arc::clone(&k.0)).collect();
        drop(st);
        for w in subs {
            w.notify_error(err);
        }
        self.notify_started();
    }
}

// watcher.go:680
fn run(impl_: &Arc<dyn WatcherImpl>) -> Result<(), Error> {
    let starter = Arc::clone(impl_);
    std::thread::Builder::new()
        .name("fswatch-backend".to_string())
        .spawn(move || {
            let base_owner = Arc::clone(&starter);
            let result =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || starter.start()));
            match result {
                Ok(Ok(())) => {}
                Ok(Err(err)) => base_owner.base().handle_start_error(&err),
                Err(panic) => {
                    let msg = panic
                        .downcast_ref::<String>()
                        .cloned()
                        .or_else(|| {
                            panic.downcast_ref::<&str>().map(std::string::ToString::to_string)
                        })
                        .unwrap_or_default();
                    base_owner.base().handle_start_error(&Error::new(msg));
                }
            }
        })
        .expect("failed to spawn fswatch backend thread");
    let base = impl_.base();
    let started = base.started.lock().unwrap();
    let _started = base.started_cv.wait_while(started, |s| !*s).unwrap();
    base.mu.lock().unwrap().start_err.clone().map_or(Ok(()), Err)
}

// watcher.go:717
fn watch_add_many(impl_: &Arc<dyn WatcherImpl>, watches: &[Arc<DirWatch>]) -> Result<(), Error> {
    let mut st = impl_.base().mu.lock().unwrap();
    let to_add: Vec<Arc<DirWatch>> = watches
        .iter()
        .filter(|w| !st.subscriptions.contains(&DwKey(Arc::clone(*w))))
        .cloned()
        .collect();
    if to_add.is_empty() {
        return Ok(());
    }

    if let Some(result) = impl_.subscribe_many(&to_add) {
        result?;
        st.subscriptions.extend(to_add.into_iter().map(DwKey));
        return Ok(());
    }

    let mut added: Vec<Arc<DirWatch>> = Vec::with_capacity(to_add.len());
    for w in to_add {
        if let Err(err) = impl_.subscribe(&w) {
            for added_watch in &added {
                st.subscriptions.remove(&DwKey(Arc::clone(added_watch)));
                let _ = impl_.close_watch(added_watch);
            }
            return Err(err);
        }
        st.subscriptions.insert(DwKey(Arc::clone(&w)));
        added.push(w);
    }
    Ok(())
}

// watcher.go:764
fn watch_remove(impl_: &Arc<dyn WatcherImpl>, w: &Arc<DirWatch>) {
    let mut st = impl_.base().mu.lock().unwrap();
    if !st.subscriptions.remove(&DwKey(Arc::clone(w))) {
        return;
    }
    let _ = impl_.close_watch(w);
}

// ----- dirWatch: per-directory watch state -------------------------

#[derive(Clone)]
pub struct Callback {
    id: u64,
    dir: String,
    physical_dir: String,
    watch_dir: String,
    watch_physical_dir: String,
    recursive: bool,
    f: WatchCallback,
    ignore: Option<IgnoreFunc>,
    since_seq: u64,
    terminal: Option<Error>,
    delivered: bool,
    comparer: BytePathComparer,
    #[cfg(target_os = "macos")] // read only by terminate_callbacks_for_deleted_root
    dir_comparison: ComparisonPath<'static>,
    physical_comparison: ComparisonPath<'static>,
    file_comparison: ComparisonPath<'static>,
}

// dirWatch holds per-directory state: pending events, registered callbacks,
// and a reference to the shared debouncer. Each watched directory has one.
pub struct DirWatch {
    // dir is the caller-visible watch root used in delivered event paths.
    pub dir: String,
    // physicalDir is the path passed to OS watcher APIs. It differs from dir
    // when dir or an ancestor is a symlink or reparse point to a directory.
    pub(crate) physical_dir: String,
    pub(crate) recursive: bool,
    pub events: EventList,
    pub(crate) comparer: BytePathComparer,
    #[cfg(target_os = "macos")] // read only by the FSEvents backend
    pub(crate) dir_fold: String,
    #[cfg(target_os = "macos")] // read only by the FSEvents backend
    pub(crate) physical_dir_fold: String,

    // state stores per-directory platform-specific bookkeeping (fsevents, windows).
    #[cfg(target_os = "macos")] // windows.go is not ported
    pub(crate) state: Mutex<Option<Arc<dyn std::any::Any + Send + Sync>>>,
    // sequence returns a backend event sequence cutoff for new logical callbacks.
    sequence: Option<fn() -> u64>,

    mu: Mutex<DirWatchState>,
    key: usize,
}

struct DirWatchState {
    callbacks: Vec<Callback>,
    debounce: Option<Arc<Debounce>>,
    next_cb_id: u64,
}

static NEXT_DIR_WATCH_KEY: AtomicU64 = AtomicU64::new(1);

impl DirWatch {
    // watcher.go:834 (newDirWatch + setComparer + the fields getOrCreateDirWatch assigns before publishing it)
    pub fn new(
        dir: String,
        physical_dir: String,
        db: &Arc<Debounce>,
        comparer: BytePathComparer,
        sequence: Option<fn() -> u64>,
        recursive: bool,
    ) -> Arc<DirWatch> {
        #[cfg(target_os = "macos")]
        let dir_fold = comparer.prepare(&dir).folded;
        #[cfg(target_os = "macos")]
        let physical_dir_fold = if physical_dir == dir {
            dir_fold.clone()
        } else {
            comparer.prepare(&physical_dir).folded
        };
        let dw = Arc::new(DirWatch {
            dir,
            physical_dir,
            recursive,
            events: EventList::default(),
            comparer,
            #[cfg(target_os = "macos")]
            dir_fold,
            #[cfg(target_os = "macos")]
            physical_dir_fold,
            #[cfg(target_os = "macos")]
            state: Mutex::new(None),
            sequence,
            mu: Mutex::new(DirWatchState {
                callbacks: Vec::new(),
                debounce: Some(Arc::clone(db)),
                next_cb_id: 0,
            }),
            key: NEXT_DIR_WATCH_KEY.fetch_add(1, Ordering::Relaxed) as usize,
        });
        let target = Arc::clone(&dw);
        db.add(dw.key, Arc::new(move || target.trigger_callbacks()));
        dw
    }

    // watcher.go:866
    // displayPath maps a physical event path back under the caller-visible
    // watch root.
    #[cfg(unix)]
    pub(crate) fn display_path(&self, watch_path: &str) -> String {
        rebase_path(watch_path, &self.physical_dir, &self.dir)
    }

    // watcher.go:871
    // physicalPath maps a caller-visible path to the physical watched root.
    #[expect(dead_code, reason = "its only Go callers, in fanotify_linux.go, are not ported")]
    fn physical_path(&self, display_path: &str) -> String {
        rebase_path(display_path, &self.dir, &self.physical_dir)
    }

    // watcher.go:916
    fn destroy_debounce(&self) {
        let db = self.mu.lock().unwrap().debounce.take();
        if let Some(db) = db {
            db.remove(self.key);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    // tsrs-only (`set_write_settling`): see `debounce::write_started` / `write_finished`.
    pub(crate) fn write_started(&self, path: &str) {
        let db = self.mu.lock().unwrap().debounce.clone();
        if let Some(db) = db {
            db.write_started(path);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn write_finished(&self, path: &str) {
        let db = self.mu.lock().unwrap().debounce.clone();
        if let Some(db) = db {
            db.write_finished(path);
        }
    }

    #[cfg(target_os = "macos")]
    // tsrs-only: see `debounce::hold_leading_edge`.
    pub(crate) fn hold_leading_edge(&self) {
        let db = self.mu.lock().unwrap().debounce.clone();
        if let Some(db) = db {
            db.hold_leading_edge();
        }
    }

    // watcher.go:926
    #[cfg(unix)]
    pub(crate) fn notify(&self) {
        let st = self.mu.lock().unwrap();
        let has_pending_cbs = st.callbacks.iter().any(|cb| !cb.delivered);
        let has_terminal = st.callbacks.iter().any(|cb| cb.terminal.is_some() && !cb.delivered);
        let has_events = self.events.size() > 0;
        let has_error = self.events.has_error();
        let db = st.debounce.clone();
        drop(st);

        if has_pending_cbs
            && (has_events || has_error || has_terminal)
            && let Some(db) = db
        {
            db.trigger();
        }
    }

    // watcher.go:944
    fn notify_error(&self, err: &Error) {
        let cbs = std::mem::take(&mut self.mu.lock().unwrap().callbacks);
        for cb in cbs {
            (cb.f)(Vec::new(), Some(err.clone()));
        }
    }

    // watcher.go:954
    fn trigger_callbacks(&self) {
        let mut st = self.mu.lock().unwrap();
        let has_error = self.events.has_error();
        let has_events = self.events.size() > 0;
        let mut cbs: Vec<Callback> = Vec::with_capacity(st.callbacks.len());
        let mut has_terminal = false;
        for cb in &st.callbacks {
            if cb.delivered {
                continue;
            }
            if cb.terminal.is_some() {
                has_terminal = true;
            }
            cbs.push(cb.clone());
        }
        if cbs.is_empty() {
            if has_events || has_error {
                let _ = self.events.drain();
            }
            return;
        }
        if !has_events && !has_error && !has_terminal {
            return;
        }
        // tsrs-only: the entries are taken under the locks and each callback's events made outside them (Go makes every
        // callback's snapshot under both). A watch shared by a few hundred callbacks (the CLI watch's directories,
        // consolidated) and a burst of a few thousand events (an emit) spent ~190 ms here with both locks held,
        // stalling the backend's event thread and new watches.
        let (entries, err) = self.events.drain_entries();
        for cb in &cbs {
            if cb.terminal.is_none() {
                continue;
            }
            if let Some(c) = st.callbacks.iter_mut().find(|c| c.id == cb.id) {
                c.delivered = true;
            }
        }
        drop(st);

        let comparisons = Mutex::new(ComparisonCache::default());
        let index = (cbs.len() > 1 && entries.len() > 1).then(|| EventIndex::new(&entries, self));
        for cb in &cbs {
            let mut cb_events: Vec<Event> =
                match index.as_ref().and_then(|index| index.candidates(cb)) {
                    Some(candidates) => candidates
                        .into_iter()
                        .filter_map(|i| entries[i].1.event_since(&entries[i].0, cb.since_seq))
                        .collect(),
                    None => entries
                        .iter()
                        .filter_map(|(path, e)| e.event_since(path, cb.since_seq))
                        .collect(),
                };
            if cb.ignore.is_some()
                || !cb.recursive
                || cb.dir != self.dir
                || !cb.file_comparison.path.is_empty()
            {
                let mut filtered = Vec::with_capacity(cb_events.len());
                for e in cb_events {
                    let mut e = cb.map_event_cached(e, Some(&comparisons));
                    if !cb.file_comparison.path.is_empty() {
                        let mut path = ComparisonPath {
                            path: e.path.clone(),
                            cache: Some(&comparisons),
                            ..Default::default()
                        };
                        match cb.comparer.suffix_prepared(&cb.file_comparison, &mut path) {
                            Some(suffix) if suffix.is_empty() => {}
                            _ => continue,
                        }
                        e.path.clone_from(&cb.file_comparison.path);
                    }
                    if let Some(ignore) = &cb.ignore
                        && ignore(&e.path)
                    {
                        continue;
                    }
                    if cb.dir != self.dir
                        && !e.included_watch_root
                        && e.path == cb.dir
                        && e.kind == EventKind::UPDATE
                    {
                        continue;
                    }
                    if cb.recursive {
                        if cb.dir != self.dir && !is_in_directory_or_self(&cb.dir, &e.path) {
                            continue;
                        }
                    } else {
                        let is_shared_watch_root =
                            if cb.dir == self.dir { false } else { e.path == cb.dir };
                        if !is_direct_child(&cb.dir, &e.path) && !is_shared_watch_root {
                            continue;
                        }
                    }
                    filtered.push(e);
                }
                cb_events = filtered;
            }
            let cb_err = cb.terminal.clone().or_else(|| err.clone());
            if !cb_events.is_empty() || cb_err.is_some() {
                (cb.f)(cb_events, cb_err);
            }
        }
    }

    // watcher.go:1066
    #[cfg(target_os = "macos")] // only the FSEvents backend calls it
    pub(crate) fn terminate_callbacks_for_deleted_root(
        &self,
        path: &str,
        seq: u64,
        err: &Error,
    ) -> bool {
        let mut st = self.mu.lock().unwrap();
        let mut changed = false;
        let comparisons = Mutex::new(ComparisonCache::default());
        let deleted = ComparisonPath {
            path: path.to_string(),
            cache: Some(&comparisons),
            ..Default::default()
        };
        for cb in &mut st.callbacks {
            if cb.delivered || cb.terminal.is_some() || cb.since_seq >= seq {
                continue;
            }
            let physical_path = ComparisonPath {
                path: cb.event_physical_path(path),
                cache: Some(&comparisons),
                ..Default::default()
            };
            let (mut dir, mut physical) =
                (cb.dir_comparison.clone(), cb.physical_comparison.clone());
            let logical_match = cb.comparer.suffix_prepared(&deleted, &mut dir).is_some();
            let physical_match =
                cb.comparer.suffix_prepared(&physical_path, &mut physical).is_some();
            if logical_match || physical_match {
                cb.terminal = Some(err.clone());
                changed = true;
            }
        }
        changed
    }

    // watcher.go:1124
    #[expect(
        dead_code,
        reason = "only Go tests call it (watcher_test.go, fsevents_darwin_shared_test.go), and those are not ported"
    )]
    fn watch(
        &self,
        dir: &str,
        physical_dir: &str,
        recursive: bool,
        f: WatchCallback,
        ignore: Option<IgnoreFunc>,
    ) -> u64 {
        self.add_callback(dir, physical_dir, recursive, f, ignore, "")
    }

    // watcher.go:1128
    pub fn add_callback(
        &self,
        dir: &str,
        physical_dir: &str,
        recursive: bool,
        f: WatchCallback,
        ignore: Option<IgnoreFunc>,
        file: &str,
    ) -> u64 {
        let mut st = self.mu.lock().unwrap();
        st.next_cb_id += 1;
        let id = st.next_cb_id;
        let mut since_seq = self.events.sequence();
        if let Some(sequence) = self.sequence {
            since_seq = sequence();
        }
        st.callbacks.push(Callback {
            id,
            dir: dir.to_string(),
            physical_dir: physical_dir.to_string(),
            watch_dir: self.dir.clone(),
            watch_physical_dir: self.physical_dir.clone(),
            recursive,
            f,
            ignore,
            since_seq,
            terminal: None,
            delivered: false,
            comparer: self.comparer,
            #[cfg(target_os = "macos")]
            dir_comparison: self.comparer.prepare(dir),
            physical_comparison: self.comparer.prepare(physical_dir),
            file_comparison: self.comparer.prepare(file),
        });
        id
    }

    // watcher.go:1146
    fn unwatch(&self, id: u64) -> bool {
        let mut st = self.mu.lock().unwrap();
        if let Some(i) = st.callbacks.iter().position(|cb| cb.id == id) {
            st.callbacks.remove(i);
            return st.callbacks.is_empty();
        }
        false
    }

    // watcher.go:1158
    fn unref(self: &Arc<Self>, w: &PlatformWatcher) {
        let empty = self.mu.lock().unwrap().callbacks.is_empty();
        if empty {
            w.remove_dir_watch(self);
        }
    }

    pub fn trigger_callbacks_for_test(&self) {
        self.trigger_callbacks();
    }

    pub fn destroy_debounce_for_test(&self) {
        self.destroy_debounce();
    }
}

impl Callback {
    // watcher.go:1046
    #[expect(
        dead_code,
        reason = "only a Go test calls it (TestConsolidatedSymlinkChildMapsSharedLogicalPath), and it is not ported"
    )]
    fn map_event(&self, e: Event) -> Event {
        self.map_event_cached(e, None)
    }

    // watcher.go:1050
    fn map_event_cached(&self, mut e: Event, cache: Option<&Mutex<ComparisonCache>>) -> Event {
        if !self.physical_dir.is_empty()
            && (self.physical_dir != self.dir || self.comparer.ignore_case)
        {
            let mut physical_path = ComparisonPath {
                path: self.event_physical_path(&e.path),
                cache,
                ..Default::default()
            };
            let mut root = self.physical_comparison.clone();
            if root.path.is_empty() {
                root.path.clone_from(&self.physical_dir);
            }
            if let Some(path) = self.comparer.rebase_prepared(&mut physical_path, &root, &self.dir)
            {
                e.path = path;
            }
        }
        e
    }

    // watcher.go:1062
    fn event_physical_path(&self, path: &str) -> String {
        if !self.watch_physical_dir.is_empty()
            && !self.watch_dir.is_empty()
            && self.watch_physical_dir != self.watch_dir
            && is_in_directory_or_self(&self.watch_dir, path)
        {
            return rebase_path(path, &self.watch_dir, &self.watch_physical_dir);
        }
        path.to_string()
    }
}

// tsrs-only (`trigger_callbacks`): a dirWatch's pending events by parent directory, so that a non-recursive callback
// considers only the events in its own directory instead of all of them. Each event is keyed by its display path and
// by its physical path (the two spaces a callback's filter compares in), folded when the comparer ignores case; a
// non-ASCII path, which folding would compare through CoreFoundation, is a candidate for every callback. The callback's
// filter then runs on the candidates unchanged, so the keys only need to cover every event it can accept: a direct
// child of its directory, or its directory itself.
struct EventIndex {
    ignore_case: bool,
    by_parent: FxHashMap<String, Vec<usize>>,
    by_path: FxHashMap<String, Vec<usize>>,
    everyone: Vec<usize>,
}

impl EventIndex {
    fn new(entries: &[(String, crate::event::EventEntry)], w: &DirWatch) -> EventIndex {
        let mut index = EventIndex {
            ignore_case: w.comparer.ignore_case,
            by_parent: FxHashMap::default(),
            by_path: FxHashMap::default(),
            everyone: Vec::new(),
        };
        for (i, (path, _)) in entries.iter().enumerate() {
            let physical = if !w.physical_dir.is_empty()
                && w.physical_dir != w.dir
                && is_in_directory_or_self(&w.dir, path)
            {
                rebase_path(path, &w.dir, &w.physical_dir)
            } else {
                path.clone()
            };
            let (Some(display), Some(physical)) = (index.key(path), index.key(&physical)) else {
                index.everyone.push(i);
                continue;
            };
            for key in [display, physical] {
                let parent = filepath_dir(&key);
                let list = index.by_parent.entry(parent).or_default();
                if list.last() != Some(&i) {
                    list.push(i);
                }
                let list = index.by_path.entry(key).or_default();
                if list.last() != Some(&i) {
                    list.push(i);
                }
            }
        }
        index
    }

    fn key(&self, path: &str) -> Option<String> {
        if !self.ignore_case {
            return Some(path.to_string());
        }
        path.is_ascii().then(|| path.to_ascii_lowercase())
    }

    // The entries `cb` may accept, in entry order; None when it may accept any (a recursive callback, or a directory
    // the keys cannot express).
    fn candidates(&self, cb: &Callback) -> Option<Vec<usize>> {
        if cb.recursive {
            return None;
        }
        let mut out: Vec<usize> = self.everyone.clone();
        for dir in [&cb.dir, &cb.physical_dir] {
            if dir.is_empty() {
                continue;
            }
            let key = self.key(dir)?;
            out.extend(self.by_parent.get(&key).into_iter().flatten());
            out.extend(self.by_path.get(&key).into_iter().flatten());
        }
        out.sort_unstable();
        out.dedup();
        Some(out)
    }
}

// watcher.go:850
// physicalDirFor returns the physical path to watch for dir. If dir, or an
// ancestor of dir, is a symlink or reparse point, events are subscribed on its
// realpath while callbacks still use dir.
pub fn physical_dir_for(dir: &str) -> String {
    let Ok(realpath) = std::fs::canonicalize(dir) else {
        return dir.to_string();
    };
    let realpath = realpath.to_string_lossy().into_owned();
    if realpath == dir || tspath::is_network_path(&realpath) {
        return dir.to_string();
    }
    canonicalize_path(&filepath_clean(&realpath))
}

// watcher.go:878
// rebasePath replaces the from root in path with to, preserving any child
// suffix. Prefix matches must end at a path separator so sibling paths like
// "/foo2" are not rebased from "/foo".
pub fn rebase_path(path: &str, from: &str, to: &str) -> String {
    if from == to {
        return path.to_string();
    }
    if path == from {
        return to.to_string();
    }
    if !path.starts_with(from) {
        return path.to_string();
    }
    let suffix = &path[from.len()..];
    if from.ends_with('/') {
        return join_path_suffix(to, suffix);
    }
    if !suffix.starts_with('/') {
        return path.to_string();
    }
    join_path_suffix(to, suffix)
}

// watcher.go:898
pub fn join_path_suffix(root: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        return root.to_string();
    }
    if let Some(rest) = suffix.strip_prefix('/') {
        if root.ends_with('/') {
            return format!("{root}{rest}");
        }
        return format!("{root}{suffix}");
    }
    if root.ends_with('/') {
        return format!("{root}{suffix}");
    }
    format!("{root}/{suffix}")
}

// watcher.go:1087
pub fn is_in_directory_or_self(dir: &str, path: &str) -> bool {
    if dir.is_empty() {
        return false;
    }
    if path == dir {
        return true;
    }
    if !path.starts_with(dir) {
        return false;
    }
    let rest = &path[dir.len()..];
    if rest.is_empty() {
        return false;
    }
    if dir.ends_with('/') {
        return true;
    }
    rest.starts_with('/')
}

// tsrs-only (`find_consolidation_dir_locked`): the child of `dir` that `path`, a path strictly inside `dir`, is in
// or is.
fn child_on_path<'p>(dir: &str, path: &'p str) -> &'p str {
    let start = if dir.ends_with('/') { dir.len() } else { dir.len() + 1 };
    path[start..].find('/').map_or(path, |i| &path[..start + i])
}

// watcher.go:1108
// isDirectChild reports whether path is an immediate child of dir.
// Both paths must be absolute. Returns false for path == dir.
fn is_direct_child(dir: &str, path: &str) -> bool {
    if !path.starts_with(dir) {
        return false;
    }
    let rest = &path[dir.len()..];
    if rest.is_empty() {
        return false;
    }
    if !rest.starts_with('/') {
        return false;
    }
    let rest = &rest[1..];
    !rest.is_empty() && !rest.contains('/')
}

// Go `filepath.Clean` (Unix).
fn filepath_clean(path: &str) -> String {
    if path.is_empty() {
        return ".".to_string();
    }
    let rooted = path.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|p| *p != "..") {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            p => out.push(p),
        }
    }
    let joined = out.join("/");
    if rooted {
        format!("/{joined}")
    } else if joined.is_empty() {
        ".".to_string()
    } else {
        joined
    }
}

// Go `filepath.Dir` (Unix).
fn filepath_dir(path: &str) -> String {
    let i = path.rfind('/').map_or(0, |i| i + 1);
    filepath_clean(&path[..i])
}
