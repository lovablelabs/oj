// Port of Go `execute/watchmanager` (watchmanager.go, watchbackend.go): fswatch directory watches, event
// accumulation and cycle signaling, shared by the CLI watcher (watcher.rs) and the build orchestrator.
//
// Locking contract (Go's): hold `lock()` around a whole cycle; `reconcile_watches` takes the guard. The watch
// callbacks take `changed` only, except a terminated watch, which takes the state lock like Go's
// handleWatchTerminated.

use std::fmt::Write as _;
use std::sync::{Arc, Condvar, Mutex, MutexGuard};

use rustc_hash::FxHashMap;
use tsrs_core::tspath::{self, ComparePathsOptions};
use tsrs_fswatch::{
    ERR_OVERFLOW, ERR_WATCH_TERMINATED, Error, Event, EventKind, IgnoreFunc, WatchCallback,
};

use crate::tsc::SyncWriter;

/// Go `WatchBackend`: fswatch, or the test harness's mock.
pub trait WatchBackend: Send + Sync {
    fn watch_directories(
        &self,
        requests: Vec<WatchDirectoryRequest>,
    ) -> Result<Vec<Box<dyn tsrs_fswatch::Watch>>, Error>;
}

/// Go `watchmanager.WatchDirectoryRequest`.
pub struct WatchDirectoryRequest {
    pub dir: String,
    pub callback: WatchCallback,
    pub recursive: bool,
    pub ignore: Option<IgnoreFunc>,
}

/// Go `FSWatchBackend`.
pub struct FsWatchBackend(&'static dyn tsrs_fswatch::Watcher);

impl WatchBackend for FsWatchBackend {
    fn watch_directories(
        &self,
        requests: Vec<WatchDirectoryRequest>,
    ) -> Result<Vec<Box<dyn tsrs_fswatch::Watch>>, Error> {
        let requests = requests
            .into_iter()
            .map(|r| {
                let mut options = Vec::new();
                if r.recursive {
                    options.push(tsrs_fswatch::with_recursive());
                }
                if let Some(ignore) = r.ignore {
                    options.push(tsrs_fswatch::with_ignore(ignore));
                }
                tsrs_fswatch::WatchDirectoryRequest {
                    dir: r.dir,
                    callback: Some(r.callback),
                    options,
                }
            })
            .collect();
        self.0.watch_directories(requests)
    }
}

struct WatchedDir {
    closer: Option<Box<dyn tsrs_fswatch::Watch>>,
    recursive: bool,
    // Identity for handleWatchTerminated (Go compares *watchedDir pointers).
    id: u64,
}

/// The state Go guards with `WatchManager.mu`.
pub struct WatchState {
    backend: Option<Arc<dyn WatchBackend>>,
    watched_dirs: FxHashMap<String, WatchedDir>,
    next_id: u64,
}

#[derive(Default)]
struct ChangedState {
    paths: Option<FxHashMap<String, EventKind>>,
    overflow: bool,
}

pub struct WatchManager {
    mu: Mutex<WatchState>,
    changed: Mutex<ChangedState>,
    // Go's `doCycleCh` (buffered, capacity 1): a pending signal coalesces with later ones.
    do_cycle: Mutex<bool>,
    do_cycle_cv: Condvar,
    stopped: Mutex<bool>,

    /// Go `DebugLog` (`TS_WATCH_DEBUG`); set once, possibly after the manager is shared.
    debug_log: std::sync::OnceLock<SyncWriter>,
    warn_writer: SyncWriter,
    dir_exists: Box<dyn Fn(&str) -> bool + Send + Sync>,
}

impl WatchManager {
    /// Go `NewWatchManager`.
    pub(crate) fn new(
        warn_writer: SyncWriter,
        dir_exists: Box<dyn Fn(&str) -> bool + Send + Sync>,
    ) -> Arc<WatchManager> {
        Arc::new(WatchManager {
            mu: Mutex::new(WatchState {
                backend: None,
                watched_dirs: FxHashMap::default(),
                next_id: 0,
            }),
            changed: Mutex::new(ChangedState::default()),
            do_cycle: Mutex::new(false),
            do_cycle_cv: Condvar::new(),
            stopped: Mutex::new(false),
            debug_log: std::sync::OnceLock::new(),
            warn_writer,
            dir_exists,
        })
    }

    pub(crate) fn lock(&self) -> MutexGuard<'_, WatchState> {
        self.mu.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn changed(&self) -> MutexGuard<'_, ChangedState> {
        self.changed.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn set_debug_log(&self, log: SyncWriter) {
        let _ = self.debug_log.set(log);
    }

    pub(crate) fn debug_log(&self) -> Option<&SyncWriter> {
        self.debug_log.get()
    }

    pub(crate) fn debug(&self, msg: impl FnOnce() -> String) {
        if let Some(log) = self.debug_log.get() {
            log(&msg());
        }
    }

    pub(crate) fn set_backend(state: &mut WatchState, backend: Arc<dyn WatchBackend>) {
        state.backend = Some(backend);
    }

    /// Go `EnsureDefaultBackend`.
    pub(crate) fn ensure_default_backend(&self, state: &mut WatchState) {
        if state.backend.is_none() {
            // tsrs-only: a cycle starts when the saved files are closed, not at the first write (a save's truncation).
            tsrs_fswatch::set_write_settling(true);
            // tsrs-only: TSRS_WATCH_BACKEND=kqueue picks the fallback that macOS uses when FSEvents is unavailable,
            // for testing it.
            let fsw = match std::env::var("TSRS_WATCH_BACKEND").as_deref() {
                Ok("kqueue") if tsrs_fswatch::kqueue().available() => tsrs_fswatch::kqueue(),
                _ => tsrs_fswatch::default(),
            };
            state.backend = Some(Arc::new(FsWatchBackend(fsw)));
            self.debug(|| format!("[watch] using {} backend\n", fsw.name()));
        }
    }

    /// Go `DrainEvents`.
    pub(crate) fn drain_events(&self) -> (FxHashMap<String, EventKind>, bool) {
        let mut changed = self.changed();
        (changed.paths.take().unwrap_or_default(), std::mem::take(&mut changed.overflow))
    }

    /// tsrs-only: records `paths` as updated, as if their events had arrived, and starts a cycle.
    pub(crate) fn queue_changes(&self, paths: Vec<String>) {
        {
            let mut changed = self.changed();
            let map = changed.paths.get_or_insert_with(FxHashMap::default);
            for path in paths {
                map.insert(path, EventKind::UPDATE);
            }
        }
        self.signal_do_cycle();
    }

    /// Go `ForceOverflow`.
    pub(crate) fn force_overflow(&self) {
        self.changed().overflow = true;
    }

    fn signal_do_cycle(&self) {
        *self.do_cycle.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        self.do_cycle_cv.notify_one();
    }

    fn on_watch_events(&self, events: &[Event], err: Option<&Error>) {
        if let Some(err) = err {
            if err.is(ERR_OVERFLOW) {
                self.debug(|| "[watch] event overflow, triggering rebuild\n".to_string());
                self.changed().overflow = true;
                self.signal_do_cycle();
                return;
            }
            (self.warn_writer)(&format!("Warning: File watch error: {}\n", err.message()));
            return;
        }
        if events.is_empty() {
            return;
        }
        self.debug(|| {
            let mut s = format!("[watch] {} event(s): ", events.len());
            for (i, e) in events.iter().enumerate() {
                if i > 0 {
                    s.push_str(", ");
                }
                if i >= 5 {
                    let _ = write!(s, "... and {} more", events.len() - i);
                    break;
                }
                let _ = write!(s, "{} {}", e.kind, e.path);
            }
            s.push('\n');
            s
        });
        {
            let mut changed = self.changed();
            let paths = changed.paths.get_or_insert_with(|| {
                FxHashMap::with_capacity_and_hasher(events.len(), rustc_hash::FxBuildHasher)
            });
            for e in events {
                paths.insert(e.path.clone(), e.kind);
            }
        }
        self.signal_do_cycle();
    }

    fn handle_watch_terminated(&self, dir: &str, id: u64) {
        self.debug(|| format!("[watch] watch terminated: {dir}\n"));
        let stale = {
            let mut state = self.lock();
            match state.watched_dirs.get(dir) {
                Some(wd) if wd.id == id => state.watched_dirs.remove(dir).and_then(|wd| wd.closer),
                _ => None,
            }
        };
        if let Some(closer) = stale {
            let _ = closer.close();
        }
        self.changed().overflow = true;
        self.signal_do_cycle();
    }

    /// Go `CloseAllWatches`.
    fn close_all_watches(&self) {
        let closers: Vec<_> =
            self.lock().watched_dirs.drain().filter_map(|(_, wd)| wd.closer).collect();
        for c in closers {
            let _ = c.close();
        }
    }

    fn create_dir_watch_request(
        self: &Arc<Self>,
        dir: &str,
        id: u64,
        recursive: bool,
    ) -> WatchDirectoryRequest {
        let wm = Arc::downgrade(self);
        let watch_dir = dir.to_string();
        WatchDirectoryRequest {
            dir: dir.to_string(),
            recursive,
            ignore: Some(Arc::new(|p: &str| should_ignore_watch_path(p))),
            callback: Arc::new(move |events: Vec<Event>, err: Option<Error>| {
                let Some(wm) = wm.upgrade() else { return };
                if let Some(e) = &err
                    && e.is(ERR_WATCH_TERMINATED)
                {
                    wm.handle_watch_terminated(&watch_dir, id);
                    return;
                }
                wm.on_watch_events(&events, err.as_ref());
            }),
        }
    }

    /// Go `ResolveDesiredDirs`: each wanted directory, or its nearest existing ancestor (non-recursive).
    pub(crate) fn resolve_desired_dirs(
        &self,
        desired_dirs: &FxHashMap<String, bool>,
    ) -> FxHashMap<String, bool> {
        let mut resolved: FxHashMap<String, bool> =
            FxHashMap::with_capacity_and_hasher(desired_dirs.len(), rustc_hash::FxBuildHasher);
        #[expect(
            clippy::iter_over_hash_type,
            reason = "Go ranges over a map too; the result is a map merged with `||`"
        )]
        for (dir, &recursive) in desired_dirs {
            // Only directories on disk can be watched. The embedded libs (bundled:///libs) exist in the FS but not on disk.
            if !tspath::is_rooted_disk_path(dir) {
                self.debug(|| format!("[watch] not a disk path: {dir}\n"));
                continue;
            }
            let mut watch_dir = dir.clone();
            let mut watch_recursive = recursive;
            while !(self.dir_exists)(&watch_dir) {
                let parent = tspath::get_directory_path(&watch_dir);
                if parent == watch_dir {
                    break;
                }
                watch_dir = parent;
                watch_recursive = false; // ancestor fallbacks are always non-recursive
            }
            // CanWatchDirectory only guards against falling back to an ancestor that is too generic to watch
            // (/, /home, ...). A directory that exists and was asked for is watched at any depth.
            if !(self.dir_exists)(&watch_dir)
                || (watch_dir != *dir && !can_watch_directory(&watch_dir))
            {
                self.debug(|| format!("[watch] no watchable ancestor for {dir}\n"));
                continue;
            }
            if watch_dir != *dir {
                self.debug(|| format!("[watch] resolved {dir} to ancestor {watch_dir}\n"));
            }
            let entry = resolved.entry(watch_dir).or_insert(false);
            *entry = *entry || watch_recursive;
        }
        resolved
    }

    /// Go `ReconcileWatches`: open the missing watches, close the stale ones, recreate those whose recursion changed.
    pub(crate) fn reconcile_watches(
        self: &Arc<Self>,
        state: &mut WatchState,
        desired_dirs: &FxHashMap<String, bool>,
    ) -> Result<(), Error> {
        if state.backend.is_none() {
            return Ok(());
        }
        let mut additions: Vec<(String, bool)> = Vec::new();
        let mut changes: Vec<(String, bool)> = Vec::new();
        let stale: Vec<String> =
            state.watched_dirs.keys().filter(|d| !desired_dirs.contains_key(*d)).cloned().collect();
        for dir in stale {
            self.debug(|| format!("[watch] closing stale dir watch: {dir}\n"));
            if let Some(closer) = state.watched_dirs.remove(&dir).and_then(|wd| wd.closer) {
                let _ = closer.close();
            }
        }
        let mut desired: Vec<(&String, &bool)> = desired_dirs.iter().collect();
        // Go ranges a map; sorted here so backends see a stable order.
        desired.sort();
        for (dir, &recursive) in desired {
            match state.watched_dirs.get(dir) {
                None => {
                    self.debug(|| {
                        format!("[watch] watching directory {dir} (recursive={recursive})\n")
                    });
                    additions.push((dir.clone(), recursive));
                }
                Some(wd) if wd.recursive != recursive => {
                    self.debug(|| {
                        format!(
                            "[watch] recreating dir watch {dir} (recursive {}→{recursive})\n",
                            wd.recursive
                        )
                    });
                    if let Some(closer) = state.watched_dirs.remove(dir).and_then(|wd| wd.closer) {
                        let _ = closer.close();
                    }
                    changes.push((dir.clone(), recursive));
                }
                Some(_) => {}
            }
        }
        additions.extend(changes);
        self.create_dir_watches(state, additions)
    }

    fn create_dir_watches(
        self: &Arc<Self>,
        state: &mut WatchState,
        updates: Vec<(String, bool)>,
    ) -> Result<(), Error> {
        if updates.is_empty() {
            return Ok(());
        }
        let mut ids = Vec::with_capacity(updates.len());
        let mut requests = Vec::with_capacity(updates.len());
        for (dir, recursive) in &updates {
            state.next_id += 1;
            ids.push(state.next_id);
            requests.push(self.create_dir_watch_request(dir, state.next_id, *recursive));
        }
        let backend = Arc::clone(state.backend.as_ref().expect("checked by reconcile_watches"));
        match backend.watch_directories(requests) {
            Ok(closers) => {
                for (((dir, recursive), id), closer) in updates.into_iter().zip(ids).zip(closers) {
                    state
                        .watched_dirs
                        .insert(dir, WatchedDir { closer: Some(closer), recursive, id });
                }
                Ok(())
            }
            Err(err) => {
                for (dir, _) in &updates {
                    self.debug(|| {
                        format!("[watch] failed to watch directory {dir}: {}\n", err.message())
                    });
                }
                Err(err)
            }
        }
    }

    /// Go `IsPathUnderWatch`.
    pub(crate) fn is_path_under_watch(
        state: &WatchState,
        path: &str,
        opts: &ComparePathsOptions,
    ) -> bool {
        state.watched_dirs.keys().any(|dir| tspath::contains_path(dir, path, opts))
    }

    /// Go `RunLoop`: runs `do_cycle` whenever events were signaled, until `stop` (Go: the context is cancelled).
    pub(crate) fn run_loop(&self, mut do_cycle: impl FnMut()) {
        loop {
            {
                let mut pending =
                    self.do_cycle.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                while !*pending {
                    if *self.stopped.lock().unwrap_or_else(std::sync::PoisonError::into_inner) {
                        drop(pending);
                        self.close_all_watches();
                        return;
                    }
                    // No timeout: an idle watch must not wake up (Go selects on its channels too); `stop` notifies.
                    pending = self
                        .do_cycle_cv
                        .wait(pending)
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                }
                *pending = false;
            }
            do_cycle();
        }
    }
}

/// Go `DirWatchSet`: the directories to watch, answering "is this directory already covered" (present, or under a
/// recursive entry).
pub(crate) struct DirWatchSet {
    use_case_sensitive_file_names: bool,
    dirs: FxHashMap<String, bool>,
    names: FxHashMap<String, String>,
}

impl DirWatchSet {
    pub(crate) fn new(opts: &ComparePathsOptions) -> DirWatchSet {
        DirWatchSet {
            use_case_sensitive_file_names: opts.use_case_sensitive_file_names,
            dirs: FxHashMap::default(),
            names: FxHashMap::default(),
        }
    }

    fn canonical(&self, dir: &str) -> String {
        tspath::get_canonical_file_name(dir, self.use_case_sensitive_file_names)
    }

    pub(crate) fn set(&mut self, dir: &str, recursive: bool) {
        let key = self.canonical(dir);
        self.names.entry(key.clone()).or_insert_with(|| dir.to_string());
        let entry = self.dirs.entry(key).or_insert(false);
        *entry = *entry || recursive;
    }

    /// tsrs-only: whether `dir` itself is in the set (not only under a recursive entry).
    pub(crate) fn has(&self, dir: &str) -> bool {
        self.dirs.contains_key(&self.canonical(dir))
    }

    pub(crate) fn covered(&self, dir: &str) -> bool {
        let mut dir = self.canonical(dir);
        if self.dirs.contains_key(&dir) {
            return true;
        }
        let root_length = tspath::get_root_length(&dir);
        while dir.len() > root_length {
            dir = tspath::get_directory_path(&dir);
            if self.dirs.get(&dir).copied().unwrap_or(false) {
                return true;
            }
        }
        false
    }

    pub(crate) fn dirs(&self) -> FxHashMap<String, bool> {
        self.dirs.iter().map(|(key, &recursive)| (self.names[key].clone(), recursive)).collect()
    }
}

/// tsrs-only: whether `p` is a `node_modules` directory or lies in one.
pub(crate) fn is_in_node_modules(p: &tspath::Path) -> bool {
    let s = p.as_str();
    s.ends_with("/node_modules") || s.contains("/node_modules/")
}

/// Go `ShouldIgnoreWatchPath`.
fn should_ignore_watch_path(path: &str) -> bool {
    let p = tspath::normalize_slashes(path);
    p.ends_with("/.git")
        || p.contains("/.git/")
        || p.contains("/node_modules/.")
        || p.contains("/.#")
}

/// Go `CanWatchDirectory`.
pub(crate) fn can_watch_directory(dir: &str) -> bool {
    let components = tspath::get_path_components(dir, "");
    let length = components.len();
    if length <= 2 {
        return false;
    }
    let root_length = perceived_os_root_length_for_watching(&components);
    length > root_length + 1
}

/// Go `PerceivedOsRootLengthForWatching`.
fn perceived_os_root_length_for_watching(components: &[String]) -> usize {
    let length = components.len();
    if length <= 1 {
        return 1;
    }
    let root = components[0].as_bytes();
    let mut index_after_os_root = 1;
    let mut is_dos_style =
        root.len() >= 2 && tspath::is_volume_character(root[0]) && root[1] == b':';
    if root != b"/" && !is_dos_style && components.len() > 1 {
        let c1 = components[1].as_bytes();
        if c1.len() >= 2 && tspath::is_volume_character(c1[0]) && c1.ends_with(b"$") {
            if length == 2 {
                return 2;
            }
            index_after_os_root = 2;
            is_dos_style = true;
        }
    }
    if is_dos_style
        && (index_after_os_root >= length
            || !components[index_after_os_root].eq_ignore_ascii_case("users"))
    {
        return index_after_os_root;
    }
    if index_after_os_root < length
        && components[index_after_os_root].eq_ignore_ascii_case("workspaces")
    {
        return index_after_os_root + 1;
    }
    index_after_os_root + 2
}
