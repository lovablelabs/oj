use super::*;

/// `server.watch.ignored` as globs (chokidar matches absolute paths): a relative
/// pattern is kept as written and also rooted at the project.
pub(crate) fn watch_ignored_patterns(root: &Path, ignored: &[String]) -> Vec<glob::Pattern> {
    let mut out = Vec::new();
    for raw in ignored {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        if let Ok(p) = glob::Pattern::new(raw) {
            out.push(p);
        }
        if !raw.starts_with('/') && !raw.starts_with("**") {
            let rooted = root.join(raw).to_string_lossy().replace('\\', "/");
            if let Ok(p) = glob::Pattern::new(&rooted) {
                out.push(p);
            }
        }
    }
    out
}

pub(crate) fn is_watch_ignored(patterns: &[glob::Pattern], root: &Path, path: &Path) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let opts = glob::MatchOptions {
        require_literal_separator: true,
        ..Default::default()
    };
    let rel = path.strip_prefix(root).ok();
    patterns.iter().any(|p| {
        p.matches_path_with(path, opts) || rel.is_some_and(|r| p.matches_path_with(r, opts))
    })
}

/// A file the config imported (Vite's `configFileDependencies`): its change
/// restarts the server too; node_modules packages are left out, as in Vite.
pub(crate) fn is_config_dependency(path: &Path) -> bool {
    let deps = plugins::config_dependencies();
    if deps.is_empty() {
        return false;
    }
    let real = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    deps.iter().any(|d| {
        !d.components().any(|c| c.as_os_str() == "node_modules")
            && (d == path || std::fs::canonicalize(d).is_ok_and(|r| r == real))
    })
}

/// The config and env files oj reads once at startup, so a change to one
/// restarts the server: the mode's env files in `env_dir` (Vite's
/// getEnvFilesForMode), every config flavor at the root, and `--config`.
/// Only these paths: the same names elsewhere (a sibling package's `.env`, a
/// nested `postcss.config.js`) are files like any other, as in Vite, whose
/// restart check is `file === configFile` or one of the mode's env files.
pub(crate) fn restart_files(
    root: &Path,
    env_dir: &Path,
    mode: &str,
    config_file: Option<&Path>,
) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = oj_env::env_files(env_dir, mode).into();
    for base in [
        "vite.config",
        "oj.config",
        "postcss.config",
        "tailwind.config",
    ] {
        for ext in ["ts", "js", "mjs", "cjs", "mts", "cts", "json"] {
            files.push(root.join(format!("{base}.{ext}")));
        }
    }
    files.extend(config_file.map(Path::to_path_buf));
    // The watcher reports normalized paths; an `envDir: "../env"` must match.
    files.iter().map(|f| normalize(f)).collect()
}

/// True when `path` is one of [`restart_files`].
pub(crate) fn is_restart_trigger(files: &[PathBuf], path: &Path) -> bool {
    files.iter().any(|f| f == path)
}

/// Tsconfig files whose change re-runs the tsconfig-aware transform; the name
/// pattern covers `extends` bases like tsconfig.base.json (Vite parity).
pub(crate) fn is_tsconfig_file(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        n == "tsconfig.json" || (n.starts_with("tsconfig.") && n.ends_with(".json"))
    })
}

/// Registry of spawned children, SIGKILLed and reaped before the restart
/// re-exec so the fresh image inherits no survivors or zombies.
///
/// The registry is also persisted (one file per oj process, owner and children
/// identified by pid + kernel start time): a SIGKILLed oj runs no handler, so
/// the NEXT boot sweeps the predecessor's children instead. Vite leaks workerd
/// the same way on SIGKILL; the persisted-registry pattern is the Bazel/Gradle
/// daemon answer, pid files with an identity check.
pub(crate) mod child_groups {
    use std::path::{Path, PathBuf};
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    struct Child {
        pid: u32,
        /// Leads its own process group (killed as a group).
        own_group: bool,
        /// Kernel start time at registration; a recycled pid never matches.
        start: Option<u64>,
    }

    static CHILDREN: Mutex<Vec<Child>> = Mutex::new(Vec::new());
    /// This process's registry file, set once by `init_registry`.
    static REGISTRY: OnceLock<PathBuf> = OnceLock::new();

    pub fn register(pid: u32, own_group: bool) {
        let start = proc_start_time(pid);
        let mut children = CHILDREN.lock().unwrap();
        children.push(Child {
            pid,
            own_group,
            start,
        });
        persist(&children);
    }

    /// Retiring a reaped (or fork-owned) child keeps a later sweep from ever
    /// aiming at a recycled pid.
    pub fn unregister(pid: u32) {
        let mut children = CHILDREN.lock().unwrap();
        children.retain(|c| c.pid != pid);
        persist(&children);
    }

    /// Kernel start time of a live process, the half of the (pid, start)
    /// identity that pid recycling cannot forge.
    #[cfg(target_os = "linux")]
    pub(crate) fn proc_start_time(pid: u32) -> Option<u64> {
        // starttime is field 22 of /proc/<pid>/stat; comm may contain spaces,
        // so fields count from after its closing paren (state is field 3).
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let after_comm = stat.rsplit_once(')')?.1;
        after_comm.split_whitespace().nth(19)?.parse().ok()
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn proc_start_time(pid: u32) -> Option<u64> {
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        let size = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        let got = unsafe {
            libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDTBSDINFO,
                0,
                &mut info as *mut _ as *mut libc::c_void,
                size,
            )
        };
        (got == size).then(|| info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec)
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    pub(crate) fn proc_start_time(_pid: u32) -> Option<u64> {
        None
    }

    fn snapshot(children: &[Child]) -> String {
        let own = std::process::id();
        let kids: Vec<serde_json::Value> = children
            .iter()
            .map(
                |c| serde_json::json!({ "pid": c.pid, "own_group": c.own_group, "start": c.start }),
            )
            .collect();
        serde_json::json!({
            "owner": { "pid": own, "start": proc_start_time(own) },
            "children": kids,
        })
        .to_string()
    }

    /// Rewrite this process's registry file (write-then-rename, so a crash
    /// mid-write never leaves a half-parsed file for the next boot).
    fn persist(children: &[Child]) {
        let Some(file) = REGISTRY.get() else { return };
        let tmp = file.with_extension("json.tmp");
        if std::fs::write(&tmp, snapshot(children)).is_ok() {
            let _ = std::fs::rename(&tmp, file);
        }
    }

    /// Point the registry at `<cache>/children`, sweep every file a dead oj
    /// left behind (killing its still-identified children), and write ours.
    /// Must run before any engine can spawn.
    pub fn init_registry(dir: &Path) {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let reaped = sweep_stale(dir);
        if reaped > 0 {
            eprintln!("oj: reaped {reaped} orphaned child process(es) from a previous run");
        }
        let _ = REGISTRY.set(dir.join(format!("{}.json", std::process::id())));
        persist(&CHILDREN.lock().unwrap());
    }

    /// Kill the identity-verified children of every dead owner in `dir` and
    /// drop their files. A live owner's file (another oj on the same cache
    /// dir) is left alone.
    pub fn sweep_stale(dir: &Path) -> usize {
        let mut reaped = 0usize;
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        let own_file = format!("{}.json", std::process::id());
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json")
                || path.file_name().and_then(|n| n.to_str()) == Some(own_file.as_str())
            {
                continue;
            }
            let parsed: Option<serde_json::Value> = std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok());
            let Some(doc) = parsed else {
                // Unreadable registry: nothing identifiable to kill; drop it.
                let _ = std::fs::remove_file(&path);
                continue;
            };
            if owner_alive(&doc["owner"]) {
                continue;
            }
            for child in doc["children"].as_array().map(Vec::as_slice).unwrap_or(&[]) {
                let (Some(pid), Some(start)) = (child["pid"].as_u64(), child["start"].as_u64())
                else {
                    continue;
                };
                // Never signal without identity: we were not this child's
                // parent, so a recycled pid is a live risk, not a TOCTOU note.
                if proc_start_time(pid as u32) != Some(start) {
                    continue;
                }
                reaped += kill_verified(pid as u32, child["own_group"].as_bool().unwrap_or(false));
            }
            let _ = std::fs::remove_file(&path);
        }
        reaped
    }

    /// The recorded owner still runs iff its (pid, start) identity matches;
    /// without a recorded start, plain existence is the best test left.
    fn owner_alive(owner: &serde_json::Value) -> bool {
        let Some(pid) = owner["pid"].as_u64() else {
            return false;
        };
        match owner["start"].as_u64() {
            Some(start) => proc_start_time(pid as u32) == Some(start),
            #[cfg(unix)]
            None => unsafe { libc::kill(pid as i32, 0) == 0 },
            #[cfg(not(unix))]
            None => false,
        }
    }

    #[cfg(unix)]
    fn kill_verified(pid: u32, own_group: bool) -> usize {
        let pid_i = pid as i32;
        let target = if own_group {
            if unsafe { libc::getpgid(pid_i) } != pid_i {
                return 0;
            }
            -pid_i
        } else {
            pid_i
        };
        usize::from(unsafe { libc::kill(target, libc::SIGKILL) } == 0)
    }

    #[cfg(not(unix))]
    fn kill_verified(_pid: u32, _own_group: bool) -> usize {
        0
    }

    /// SIGKILL every registered child (its whole group when it leads one) and
    /// reap. Drains in rounds so a child spawned mid-sweep is taken next round.
    pub fn kill_all() -> usize {
        let mut killed = 0usize;
        #[cfg(unix)]
        for _round in 0..3 {
            let children: Vec<Child> = std::mem::take(&mut *CHILDREN.lock().unwrap());
            if children.is_empty() {
                break;
            }
            for &Child { pid, own_group, .. } in &children {
                let pid_i = pid as i32;
                // Verify before signaling (second lock against pid recycling):
                // an own-group child must still lead its group (getpgid == pid),
                // a direct child must still exist; a recycled pid leading its
                // own new group remains a theoretical TOCTOU.
                let target = if own_group {
                    if unsafe { libc::getpgid(pid_i) } != pid_i {
                        continue;
                    }
                    -pid_i
                } else {
                    if unsafe { libc::kill(pid_i, 0) } != 0 {
                        continue;
                    }
                    pid_i
                };
                if unsafe { libc::kill(target, libc::SIGKILL) } == 0 {
                    killed += 1;
                }
            }
            // Reap OUR direct children only, each with a small bound; a
            // waitpid(-1) sweep stalled on unrelated live children.
            for child in &children {
                let deadline = Instant::now() + Duration::from_millis(200);
                loop {
                    let r = unsafe {
                        libc::waitpid(child.pid as i32, std::ptr::null_mut(), libc::WNOHANG)
                    };
                    if r != 0 || Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        // Every kill_all caller is terminal (shutdown, the engine's exit path,
        // the restart exec): a drained registry drops its file instead of
        // leaving an empty husk for the next boot to unlink. A straggler
        // registered mid-sweep keeps the file so the next boot can reap it.
        let children = CHILDREN.lock().unwrap();
        if children.is_empty() {
            if let Some(file) = REGISTRY.get() {
                let _ = std::fs::remove_file(file);
            }
        } else {
            persist(&children);
        }
        killed
    }
}

/// `reason` names what asked for the exec (a config/env change, a plugin's
/// `server.restart()`): the log line and the diagnostics record say which,
/// since the exec keeps the PID and a supervisor sees nothing else.
/// Registers a child spawned outside oj_server (Start's client bundler) so a
/// restart or shutdown kills it with the rest.
pub fn register_child(pid: u32) {
    child_groups::register(pid, false);
}

pub fn unregister_child(pid: u32) {
    child_groups::unregister(pid);
}

pub fn restart_process(reason: &str) -> ! {
    if let Some(hook) = RESTART_HOOK.get() {
        hook(reason);
    }
    eprintln!("{} {reason} — restarting dev server", oj_brand());
    oj_diag::emit(
        oj_diag::Event::new(
            oj_diag::Kind::Restart,
            format!("{reason}; restarting dev server"),
        )
        .warn(),
    );
    let killed = child_groups::kill_all();
    if killed > 0 {
        eprintln!("oj: restart killed {killed} child process(es)");
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("oj"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    // The next boot gets the env oj was launched with, not whatever the live
    // environment became: anything that wrote to it mid-session (an engine's
    // Deno.env, a native addon's setenv) would otherwise look like shell env
    // there, outranking `.env` files and re-keying the Start bundle cache.
    let launch_env = oj_env::get().vars_os();
    // In-process plugin hosts chdir the process to their app root; restart from
    // the launch dir so relative CLI args (`oj dev ./web`) resolve correctly.
    let launch_dir = STARTUP_CWD.get().filter(|d| d.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&args).env_clear().envs(launch_env);
        if let Some(dir) = launch_dir {
            cmd.current_dir(dir);
        }
        let err = cmd.exec();
        eprintln!("oj: restart failed: {err}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    {
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&args).env_clear().envs(launch_env);
        if let Some(dir) = launch_dir {
            cmd.current_dir(dir);
        }
        let code = cmd.status().ok().and_then(|s| s.code()).unwrap_or(0);
        std::process::exit(code);
    }
}

static RESTART_HOOK: std::sync::OnceLock<fn(&str) -> !> = std::sync::OnceLock::new();

/// Takes every restart in place of the exec, for a process hosting dev
/// servers it must outlive (oj's integration tests): an exec there would
/// replace the whole process. Set once; later calls are ignored.
pub fn set_restart_hook(hook: fn(&str) -> !) {
    let _ = RESTART_HOOK.set(hook);
}

/// The directory oj was launched from, pinned by `capture_startup_cwd` before
/// any engine boots (each in-process host really chdirs to its app root).
pub(crate) static STARTUP_CWD: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Call once at process start, before any plugin host or engine exists.
pub fn capture_startup_cwd() {
    if let Ok(dir) = std::env::current_dir() {
        let _ = STARTUP_CWD.set(dir);
    }
}

/// A watcher subscribed to changes only, as Zed's is. inotify reports every
/// open and close of a watched path; none is a change, and chokidar's
/// fs.watch never sees them. With the mask they never reach the queue (on the
/// other backends notify filters them): a file someone keeps reading (an
/// editor, an indexer, a poller) otherwise flooded the batch loops, whose
/// settle window restarts on every event, so an edit landing in that stream
/// was never handed off, and reads alone could overflow the queue.
fn new_watcher<F: notify::EventHandler>(handler: F) -> notify::Result<notify::RecommendedWatcher> {
    use notify::Watcher;
    notify::RecommendedWatcher::new(
        handler,
        notify::Config::default().with_event_kinds(notify::EventKindMask::CORE),
    )
}

/// The watcher thread's inbox: filesystem notifications, and directories to
/// add because an outside-root file was served (`ensure_watched_file`). One
/// channel, so the thread owns the watcher outright with no lock.
pub(crate) enum WatchMsg {
    Fs(notify::Result<notify::Event>),
    Dir(PathBuf),
    /// Stop watching (Vite's `server.close()` closes its watcher); acked once
    /// the watcher is dropped, so no event can follow.
    Close(std::sync::mpsc::Sender<()>),
}

/// The dev watcher's raw events, for servers layered on oj_server (the Start
/// server's worker invalidation and rebundle). One watcher feeds both, as
/// Vite's single chokidar instance feeds every environment: a second, narrower
/// watcher once left edits outside `src/` reaching the browser but never the
/// worker. Published from the notify callback, ahead of the debounce and of
/// the watcher thread's plugin-hook RPCs, which block while a plugin host
/// initializes.
#[derive(Clone, Default)]
pub struct WatchFeed(pub(crate) Arc<Mutex<Vec<std::sync::mpsc::Sender<notify::Event>>>>);

impl WatchFeed {
    /// Every event from now on, minus excluded paths (`server.watch.ignored`
    /// and the cache dir); dropping the receiver unsubscribes.
    pub fn subscribe(&self) -> std::sync::mpsc::Receiver<notify::Event> {
        let (tx, rx) = std::sync::mpsc::channel();
        self.0.lock().unwrap().push(tx);
        rx
    }

    pub(crate) fn publish(
        &self,
        ev: &notify::Event,
        ignored: &[glob::Pattern],
        root: &Path,
        cache_base: &Path,
    ) {
        let mut subscribers = self.0.lock().unwrap();
        if subscribers.is_empty() {
            return;
        }
        let mut ev = ev.clone();
        ev.paths
            .retain(|p| !watch_excluded(ignored, root, cache_base, p));
        if ev.paths.is_empty() {
            return;
        }
        subscribers.retain(|tx| tx.send(ev.clone()).is_ok());
    }
}

/// What every consumer of watcher events skips: `server.watch.ignored` plus
/// the resolved cache dir, which Vite puts in chokidar's `ignored` -- the
/// name-based skip in `is_unwatched_dir` misses an `OJ_CACHE_DIR` inside the
/// root, whose writes would echo every compile back into the watcher -- plus
/// Vite's own config-loader temp.
pub(crate) fn watch_excluded(
    ignored: &[glob::Pattern],
    root: &Path,
    cache_base: &Path,
    path: &Path,
) -> bool {
    path.starts_with(cache_base)
        || path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(is_config_timestamp_temp)
        || is_watch_ignored(ignored, root, path)
}

/// Vite's `loadConfigFromBundledFile` temp (`<config>.timestamp-<ms>-<hash>.mjs`).
/// oj loads configs as Node's Vite does (assets/vite-config-node.mjs), so it
/// lands in `node_modules/.vite-temp/`; Vite still writes it beside the config
/// when no node_modules exists. Under Start, whose rebundle loads the config,
/// a temp the watch saw was a rebuild loop.
fn is_config_timestamp_temp(name: &str) -> bool {
    name.ends_with(".mjs")
        && name.find(".timestamp-").is_some_and(|i| {
            name.as_bytes()
                .get(i + ".timestamp-".len())
                .is_some_and(u8::is_ascii_digit)
        })
}

/// Vite's ensureWatchedFile: a served file OUTSIDE the root is not covered by
/// the root watch, so its directory goes to the watcher thread (the directory,
/// not the file: editors save by rename-replace, which strands an inode watch).
/// node_modules stays unwatched, as Vite's chokidar `ignored` does.
pub(crate) fn ensure_watched_file(state: &ServerState, file: &Path) {
    if file.starts_with(&state.root) || file.components().any(|c| c.as_os_str() == "node_modules") {
        return;
    }
    if let Some(dir) = file.parent() {
        let _ = state.watch_tx.send(WatchMsg::Dir(dir.to_path_buf()));
    }
}

/// Recorded only on success, so a directory that does not exist yet (a plugin
/// watch file created later) is retried instead of skipped forever.
fn watch_served_dir(
    watchers: &mut Watchers,
    watched: &mut std::collections::HashSet<PathBuf>,
    dir: PathBuf,
) {
    if !watched.contains(&dir)
        && watchers
            .watch(&dir, notify::RecursiveMode::NonRecursive)
            .is_ok()
    {
        watched.insert(dir);
    }
}

type EventSink = Arc<dyn Fn(notify::Result<notify::Event>) + Send + Sync>;

/// chokidar's fsevents handler caps its streams at 128.
const MAX_OWN_STREAMS: usize = 128;

/// The root watcher, plus a watcher per directory watched after boot on
/// macOS. Vite's chokidar opens an FSEvents stream per watched path, so a new
/// watch never touches the running streams; notify's FSEvents watcher restarts
/// its one stream on every `watch`, dropping the events that land meanwhile
/// (root edits included). An inotify or Windows watch restarts nothing, so
/// there the root watcher takes them all.
struct Watchers {
    root: notify::RecommendedWatcher,
    sink: EventSink,
    own: std::collections::HashMap<PathBuf, notify::RecommendedWatcher>,
}

impl Watchers {
    fn watch(&mut self, path: &Path, mode: notify::RecursiveMode) -> notify::Result<()> {
        use notify::Watcher;
        if !cfg!(target_os = "macos") || self.own.len() >= MAX_OWN_STREAMS {
            return self.root.watch(path, mode);
        }
        if self.own.contains_key(path) {
            return Ok(());
        }
        let sink = Arc::clone(&self.sink);
        let mut watcher = new_watcher(move |ev| sink(ev))?;
        watcher.watch(path, mode)?;
        self.own.insert(path.to_path_buf(), watcher);
        Ok(())
    }
}

/// Top-level entries never watched: recursively watching node_modules/.oj-cache/dist/.git
/// floods inotify with self-inflicted events (.oj-cache is rewritten on every compile).
fn is_unwatched_dir(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some("node_modules" | ".oj-cache" | "dist" | ".git")
    )
}

/// Watch each top-level root entry except the unwatched dirs, plus the root
/// itself non-recursively: new top-level entries are born under it, and the
/// per-entry watches only cover what existed at boot (chokidar watches the
/// root, so Vite sees new children). Falls back to a recursive root watch
/// only if nothing else could be watched.
fn watch_root(
    watcher: &mut notify::RecommendedWatcher,
    root: &Path,
    cache_base: &Path,
) -> notify::Result<()> {
    use notify::{RecursiveMode, Watcher};
    let root_watched = watcher.watch(root, RecursiveMode::NonRecursive).is_ok();
    let mut watched_any = root_watched;
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if is_unwatched_dir(&entry.file_name()) || entry.path() == cache_base {
                continue;
            }
            let path = entry.path();
            let mode = if path.is_dir() {
                RecursiveMode::Recursive
            } else if !root_watched || entry.file_type().is_ok_and(|t| t.is_symlink()) {
                // A plain top-level file is already covered by the root watch;
                // a second watch would deliver each edit twice (ContentChanges
                // only drops metadata noise). A symlinked entry still needs its
                // own watch: it follows to the target inode, whose edits never
                // touch the root directory entry.
                RecursiveMode::NonRecursive
            } else {
                continue;
            };
            if watcher.watch(&path, mode).is_ok() {
                watched_any = true;
            }
        }
    }
    if watched_any {
        Ok(())
    } else {
        watcher.watch(root, RecursiveMode::Recursive)
    }
}

/// The directories an event brought into existence (created, or moved into
/// place) under the root, as chokidar raises `addDir` for both: a dir RENAMED
/// in (a staged tree moved into place) arrives as `Modify(Name)`, not
/// `Create`. Rename-From paths and plain file renames fall through the
/// `is_dir` check. Never one Vite's chokidar ignores (`**/node_modules/**`,
/// `**/.git/**`, the cache dir, `server.watch.ignored`), nor a top-level dir
/// the root watch leaves out (`dist`, `.oj-cache`): a fresh install's
/// `node_modules` is one event, and walking it would report every package
/// file as a create.
pub(crate) fn created_dirs(
    ignored: &[glob::Pattern],
    root: &Path,
    cache_base: &Path,
    ev: &notify::Event,
) -> Vec<PathBuf> {
    if !matches!(
        ev.kind,
        notify::EventKind::Create(_)
            | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
    ) {
        return Vec::new();
    }
    ev.paths
        .iter()
        .filter(|path| watched_subtree(ignored, root, cache_base, path) && path.is_dir())
        .cloned()
        .collect()
}

/// A path under the root the root watch covers: not inside what Vite's
/// chokidar ignores (`**/node_modules/**`, `**/.git/**`, the cache dir,
/// `server.watch.ignored`) nor a top-level dir the root watch leaves out.
fn watched_subtree(ignored: &[glob::Pattern], root: &Path, cache_base: &Path, path: &Path) -> bool {
    let Ok(rel) = path.strip_prefix(root) else {
        return false;
    };
    let mut parts = rel.components().map(|c| c.as_os_str());
    !parts.clone().any(|c| c == "node_modules" || c == ".git")
        && !parts.next().is_some_and(is_unwatched_dir)
        && !watch_excluded(ignored, root, cache_base, path)
}

/// A directory created after boot: a top-level one, caught by the root's
/// non-recursive watch, is watched recursively (re-watching is idempotent,
/// notify keys watches by path); a deeper one is already under a recursive
/// watch. Either way the files it already holds are reported as creates
/// (chokidar emits `add` per file it finds in a new dir), since writes
/// landing before the watch, or moved in with the dir, were never seen.
/// Returns those files.
fn adopt_created_dirs(
    watchers: &mut Watchers,
    ignored: &[glob::Pattern],
    root: &Path,
    cache_base: &Path,
    ev: &notify::Event,
) -> Vec<PathBuf> {
    let mut found = Vec::new();
    for path in created_dirs(ignored, root, cache_base, ev) {
        if path.parent() == Some(root)
            && watchers
                .watch(&path, notify::RecursiveMode::Recursive)
                .is_err()
        {
            continue;
        }
        scan_files(&path, &mut found);
    }
    found
}

/// The files under a new directory, minus what Vite's chokidar ignores at
/// any depth (`node_modules`, `.git`). Symlinked directories are not entered:
/// one pointing back up the tree would never end.
pub(crate) fn scan_files(dir: &Path, out: &mut Vec<PathBuf>) {
    scan_tree(dir, out, &mut Vec::new());
}

/// `scan_files`, also collecting the directories it entered.
fn scan_tree(dir: &Path, out: &mut Vec<PathBuf>, dirs: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => {
                if !matches!(entry.file_name().to_str(), Some("node_modules" | ".git")) {
                    dirs.push(path.clone());
                    scan_tree(&path, out, dirs);
                }
            }
            Ok(t) if t.is_symlink() && path.is_dir() => {}
            Ok(_) => out.push(path),
            Err(_) => {}
        }
    }
}

/// Every file the root watch covers, as chokidar's initial scan records
/// them: the root's own files and each watched top-level dir's (not the
/// unwatched dirs, the cache dir, or what `scan_files` skips), spelled under
/// the root's real path. Vite's chokidar calls a later event for one of
/// these a change, never an add.
pub fn watched_files(root: &Path) -> std::collections::HashSet<PathBuf> {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let cache_base = {
        let base = oj_cache::cache_base(&root);
        std::fs::canonicalize(&base).unwrap_or(base)
    };
    let mut files = Vec::new();
    scan_watched(&root, &cache_base, &mut files, &mut Vec::new());
    files.into_iter().collect()
}

/// `watched_files` and the directories holding them, spelled under `root`
/// as given.
fn scan_watched(root: &Path, cache_base: &Path, files: &mut Vec<PathBuf>, dirs: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let path = entry.path();
        if is_unwatched_dir(&entry.file_name()) || path == cache_base {
            continue;
        }
        if path.is_dir() {
            dirs.push(path.clone());
            scan_tree(&path, files, dirs);
        } else {
            files.push(path);
        }
    }
}

/// The watcher's snapshot: each file and directory the root watch covers,
/// with its stamp when last seen.
pub(crate) type Snapshot = std::collections::HashMap<PathBuf, Stamp>;

/// The boot snapshot, minus excluded paths, and minus anything stamped after
/// the scan began: a file created while it ran has a Create queued that must
/// still read as an add.
pub(crate) fn boot_snapshot(ignored: &[glob::Pattern], root: &Path, cache_base: &Path) -> Snapshot {
    let began = SystemTime::now();
    let (mut files, mut dirs) = (Vec::new(), Vec::new());
    scan_watched(root, cache_base, &mut files, &mut dirs);
    files
        .into_iter()
        .chain(dirs)
        .filter(|p| !watch_excluded(ignored, root, cache_base, p))
        .filter_map(|p| Some((p.clone(), stamp(&p)?)))
        .filter(|(_, s)| s.mtime < began)
        .collect()
}

/// The subtrees a rescan event says the watcher lost track of, as Zed reads
/// them: an inotify queue overflow carries no path and means the whole root;
/// FSEvents' MustScanSubDirs names a subtree, at times an ancestor of the root
/// (the whole root again). Never one the root watch leaves out.
pub(crate) fn rescan_roots(
    ignored: &[glob::Pattern],
    root: &Path,
    cache_base: &Path,
    ev: &notify::Event,
) -> Vec<PathBuf> {
    if !ev.need_rescan() {
        return Vec::new();
    }
    if ev.paths.is_empty() {
        return vec![root.to_path_buf()];
    }
    ev.paths
        .iter()
        .filter_map(|p| {
            if root.starts_with(p) {
                Some(root.to_path_buf())
            } else if watched_subtree(ignored, root, cache_base, p) {
                Some(p.clone())
            } else {
                None
            }
        })
        .collect()
}

/// A batch's rescans collapsed, as Zed does: one per subtree, none under
/// another.
pub(crate) fn collapse_rescans(mut roots: Vec<PathBuf>) -> Vec<PathBuf> {
    roots.sort();
    roots.dedup();
    let mut kept: Vec<PathBuf> = Vec::new();
    for r in roots {
        if !kept.iter().any(|k| r.starts_with(k)) {
            kept.retain(|k| !k.starts_with(&r));
            kept.push(r);
        }
    }
    kept
}

/// What a rescan of `roots` finds against the watcher's snapshot (`known`),
/// as Zed's rescan diffs its entries: files on disk it never knew are adds,
/// known paths gone from disk are removals, known files whose stamp moved
/// are edits, and directories it never knew (or knew as another inode or
/// birth) are new. Untouched files stay silent, or oj would push an HMR update for every
/// module.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Rescanned {
    pub(crate) added: Vec<PathBuf>,
    pub(crate) changed: Vec<PathBuf>,
    pub(crate) removed: Vec<PathBuf>,
    pub(crate) new_dirs: Vec<PathBuf>,
}

pub(crate) fn rescan_changes(
    roots: &[PathBuf],
    ignored: &[glob::Pattern],
    root: &Path,
    cache_base: &Path,
    known: &Snapshot,
) -> Rescanned {
    let mut out = Rescanned::default();
    let excluded = |p: &Path| watch_excluded(ignored, root, cache_base, p);
    for r in roots {
        let (mut files, mut dirs) = (Vec::new(), Vec::new());
        if r == root {
            scan_watched(root, cache_base, &mut files, &mut dirs);
        } else {
            dirs.push(r.clone());
            scan_tree(r, &mut files, &mut dirs);
        }
        for d in dirs.into_iter().filter(|d| !excluded(d)) {
            let Some(now) = stamp(&d) else { continue };
            if !known.get(&d).is_some_and(|seen| seen.same_dir(&now)) {
                out.new_dirs.push(d);
            }
        }
        for f in files.into_iter().filter(|f| !excluded(f)) {
            match known.get(&f) {
                None => out.added.push(f),
                Some(seen) if stamp(&f) != Some(*seen) => out.changed.push(f),
                Some(_) => {}
            }
        }
    }
    out.removed = known
        .keys()
        .filter(|p| roots.iter().any(|r| p.starts_with(r)) && !p.exists())
        .cloned()
        .collect();
    out.new_dirs = collapse_rescans(out.new_dirs);
    for list in [&mut out.added, &mut out.changed, &mut out.removed] {
        list.sort();
    }
    out
}

/// What a snapshot entry compares, as Zed's entries do: mtime, inode, and
/// size, plus birth time where the filesystem keeps one, since a path
/// deleted and recreated can get the freed inode back. A file renamed onto
/// a path keeps its old mtime but brings its own inode; a metadata-only
/// change (chmod, xattrs, a new hard link) moves none of them, as the live
/// path ignores it too. A directory compares by inode and birth alone
/// (`same_dir`): its mtime moves with every child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Stamp {
    pub(crate) mtime: SystemTime,
    pub(crate) ino: u64,
    pub(crate) len: u64,
    pub(crate) born: Option<SystemTime>,
}

impl Stamp {
    fn same_dir(&self, other: &Stamp) -> bool {
        self.ino == other.ino && self.born == other.born
    }
}

pub(crate) fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    #[cfg(unix)]
    let ino = std::os::unix::fs::MetadataExt::ino(&meta);
    #[cfg(not(unix))]
    let ino = 0;
    Some(Stamp {
        mtime: meta.modified().unwrap_or(UNIX_EPOCH),
        ino,
        len: if meta.is_dir() { 0 } else { meta.len() },
        born: meta.created().ok(),
    })
}

/// One debounced set of changes.
#[derive(Default)]
struct Batch {
    paths: std::collections::HashSet<PathBuf>,
    /// Paths the watcher saw come into existence: plugins get "create" for
    /// those, "update" for edits, "delete" for removals.
    created: std::collections::HashSet<PathBuf>,
    /// Subtrees the watcher lost events under (`rescan_roots`), and why.
    rescans: Vec<PathBuf>,
    rescan_reason: Option<String>,
}

impl Batch {
    /// True when `ev` changed any path.
    fn add(&mut self, changes: &mut ContentChanges, ev: &notify::Event) -> bool {
        let changed = changes.changed_paths(ev);
        if matches!(ev.kind, notify::EventKind::Create(_)) {
            self.created.extend(changed.iter().cloned());
        }
        let any = !changed.is_empty();
        self.paths.extend(changed);
        any
    }
}

/// Act on a debounced batch: restart on config changes, full reload on
/// tsconfig changes, otherwise gate or dispatch HMR. The HMR awaits the
/// plugin host (its watch files, `handleHotUpdate`), which can take as long
/// as host init or a slow hook, so each batch's runs as its own task, as
/// Vite handles each chokidar event on its own: one parked batch never holds
/// another back, a `.env` restart or another file's update included.
fn handle_batch(
    state: &Arc<ServerState>,
    paths: &[PathBuf],
    created: &std::collections::HashSet<PathBuf>,
) {
    // Config/.env can't be hot-applied (read once at startup): restart, as
    // Vite does, naming the file relative to the launch dir as Vite's
    // "<file> changed, restarting server" does. Vite restarts from
    // handleHMRUpdate, so not with `server.hmr: false`.
    if let Some(file) = paths
        .iter()
        .filter(|_| state.hmr_enabled)
        .find(|p| is_restart_trigger(&state.restart_files, p) || is_config_dependency(p))
    {
        let shown = STARTUP_CWD
            .get()
            .and_then(|cwd| pathdiff::diff_paths(file, cwd))
            .unwrap_or_else(|| file.clone());
        restart_process(&format!("{} changed", shown.display()));
    }
    // Vite's reloadOnTsconfigChange: clear caches, full reload. The compile
    // key folds class-field semantics in, so stale persistent-cache entries
    // become unreachable once discovery is cleared.
    if paths.iter().any(|p| is_tsconfig_file(p)) {
        oj_compiler::tsconfig::clear_cache();
        state.mtime_keys.lock().unwrap().clear();
        state.memory.lock().unwrap().clear();
        let _ = state
            .reload_tx
            .send(full_reload_frame("tsconfig change", None, None));
    }
    // Vite's watcher, per change: plugin watchChange, then the module graph
    // invalidation, then HMR, which `server.hmr: false` skips and the editor
    // gate holds until its flush. The first two run either way. A Tailwind
    // sheet is dropped at once, as it carries no plugin state; so are the
    // resolvers' cached misses, which Vite's resolver never keeps.
    if !paths.is_empty() {
        state.tailwind_sheets.invalidate();
        crate::hmr::drop_resolver_misses(state, paths, created);
    }
    let dispatch = state.hmr_enabled
        && !state
            .hmr_gate
            .as_ref()
            .is_some_and(|gate| gate.hold(state, paths, false));
    let (state, paths, created) = (Arc::clone(state), paths.to_vec(), created.clone());
    state.rt.clone().spawn(async move {
        run_watch_change(&state, &paths, &created).await;
        invalidate_changed(&state, &paths);
        if !dispatch {
            return;
        }
        let messages = decide(&state, &paths, &created).await;
        if messages.is_empty() {
            return;
        }
        state.dir_cache.lock().unwrap().clear();
        for message in messages {
            let _ = state.reload_tx.send(message);
        }
    });
}

/// Vite's `moduleGraph.onFileChange` for each edited file: one still on disk
/// and already in the graph (a first save by rename arrives as a create, a
/// `change` to chokidar). A hard-invalidated module drops its mtime fast-path
/// key, so its next request recomputes the cache key, which carries the new
/// invalidation stamp.
fn invalidate_changed(state: &ServerState, paths: &[PathBuf]) {
    for path in paths {
        if !path.exists() {
            continue;
        }
        let url = url_of(&state.root, path);
        let hard = state
            .graph
            .lock()
            .unwrap()
            .invalidate_on_change(Path::new(&url), |m| !is_style_url(&m.to_string_lossy()));
        let mut keys = state.mtime_keys.lock().unwrap();
        for module in hard {
            keys.remove(&module.display().to_string());
        }
    }
}

pub(crate) fn spawn_watcher(
    state: Arc<ServerState>,
    rx: std::sync::mpsc::Receiver<WatchMsg>,
    ready: tokio::sync::watch::Sender<bool>,
) {
    std::thread::spawn(move || {
        use std::sync::mpsc::RecvTimeoutError;

        let tx = state.watch_tx.clone();
        let feed = state.watch_feed.clone();
        // Canonicalized so a relative or symlinked `OJ_CACHE_DIR` still
        // matches the absolute paths the watcher reports.
        let cache_base = {
            let base = oj_cache::cache_base(&state.root);
            std::fs::canonicalize(&base).unwrap_or(base)
        };
        let (ignored, root, cache) = (
            state.watch_ignored.clone(),
            state.root.clone(),
            cache_base.clone(),
        );
        let sink: EventSink = Arc::new(move |ev: notify::Result<notify::Event>| {
            if let Ok(ev) = &ev {
                feed.publish(ev, &ignored, &root, &cache);
            }
            let _ = tx.send(WatchMsg::Fs(ev));
        });
        let root_sink = Arc::clone(&sink);
        let mut watchers = match new_watcher(move |ev| root_sink(ev)) {
            Ok(root) => Watchers {
                root,
                sink,
                own: Default::default(),
            },
            Err(err) => {
                eprintln!("oj: file watcher failed to start: {err}");
                return;
            }
        };
        let mut served_dirs: std::collections::HashSet<PathBuf> = Default::default();
        if let Err(err) = watch_root(&mut watchers.root, &state.root, &cache_base) {
            eprintln!("oj: cannot watch {}: {err}", state.root.display());
            return;
        }
        let _ = ready.send(true);

        let debounce_ms: u64 = 10;
        // The paths the watcher knows, chokidar's watched set, with each
        // one's stamp when last seen: the boot scan plus every path a batch
        // named. A Create for a known path is an edit (chokidar: add once,
        // change after; FSEvents keeps a file's "created" flag on later
        // events for a while), and a rescan diffs against it, as Zed's does
        // against its entries. Scanned after `ready`: events queue meanwhile.
        let mut seen_paths = boot_snapshot(&state.watch_ignored, &state.root, &cache_base);
        let mut changes = ContentChanges::new();
        loop {
            let first = match rx.recv() {
                Ok(WatchMsg::Fs(Ok(ev))) => ev,
                Ok(WatchMsg::Fs(Err(_))) => continue,
                Ok(WatchMsg::Dir(dir)) => {
                    watch_served_dir(&mut watchers, &mut served_dirs, dir);
                    continue;
                }
                Ok(WatchMsg::Close(ack)) => {
                    drop(watchers);
                    let _ = ack.send(());
                    return;
                }
                Err(_) => break,
            };
            let mut batch = Batch::default();
            ingest_event(
                &state,
                &mut watchers,
                &cache_base,
                &mut changes,
                &mut batch,
                &first,
            );
            if batch.paths.is_empty() && batch.rescans.is_empty() {
                continue;
            }
            // Only a change the batch keeps restarts the quiet window: ignored
            // writes arriving faster than it would otherwise hold it open.
            let debounce = Duration::from_millis(debounce_ms);
            let mut quiet_at = std::time::Instant::now() + debounce;
            loop {
                let left = quiet_at.saturating_duration_since(std::time::Instant::now());
                match rx.recv_timeout(left) {
                    Ok(WatchMsg::Fs(Ok(ev))) => {
                        if ingest_event(
                            &state,
                            &mut watchers,
                            &cache_base,
                            &mut changes,
                            &mut batch,
                            &ev,
                        ) {
                            quiet_at = std::time::Instant::now() + debounce;
                        }
                    }
                    Ok(WatchMsg::Fs(Err(_))) => {}
                    Ok(WatchMsg::Dir(dir)) => {
                        watch_served_dir(&mut watchers, &mut served_dirs, dir)
                    }
                    Ok(WatchMsg::Close(ack)) => {
                        drop(watchers);
                        let _ = ack.send(());
                        return;
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            if !batch.rescans.is_empty() {
                rescan(
                    &state,
                    &mut watchers,
                    &cache_base,
                    &mut changes,
                    &mut batch,
                    &seen_paths,
                );
            }
            let Batch {
                paths, mut created, ..
            } = batch;
            let paths: Vec<PathBuf> = paths.into_iter().collect();
            created.retain(|p| !seen_paths.contains_key(p));
            // chokidar reports `add` again for a path after its `unlink`, so a
            // deleted path is unseen: recreated, it is a create, not an edit.
            for p in &paths {
                match stamp(p) {
                    Some(at) => seen_paths.insert(p.clone(), at),
                    None => seen_paths.remove(p),
                };
            }
            handle_batch(&state, &paths, &created);
        }
    });
}

/// One raw watcher event into the current batch: a created dir is adopted
/// first, and its existing files enter this same batch (and the feed,
/// which only hears the notify callback) as a synthesized create.
fn ingest_event(
    state: &ServerState,
    watchers: &mut Watchers,
    cache_base: &Path,
    changes: &mut ContentChanges,
    batch: &mut Batch,
    ev: &notify::Event,
) -> bool {
    let rescans = rescan_roots(&state.watch_ignored, &state.root, cache_base, ev);
    if !rescans.is_empty() {
        batch.rescans.extend(rescans);
        if let Some(info) = ev.info() {
            batch.rescan_reason.get_or_insert_with(|| info.to_string());
        }
        return true;
    }
    let mut kept = false;
    let adopted = adopt_created_dirs(watchers, &state.watch_ignored, &state.root, cache_base, ev);
    if !adopted.is_empty() {
        let mut synth =
            notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::Any));
        synth.paths = adopted;
        state
            .watch_feed
            .publish(&synth, &state.watch_ignored, &state.root, cache_base);
        kept |= add_watched(state, cache_base, changes, batch, &synth);
    }
    add_watched(state, cache_base, changes, batch, ev) || kept
}

/// The watcher lost events (an inotify queue overflow, FSEvents'
/// MustScanSubDirs): as Zed does, scan the subtrees against the snapshot,
/// re-arm the watches a directory born meanwhile never got, and hand what
/// changed to the batch and the feed as synthesized events.
///
/// Only new directories are re-armed, never the whole tree: notify's `watch`
/// walks on the thread that drains the queue, so re-walking a large tree
/// holds the queue undrained for the whole walk, which is how an overflow
/// raised the next one while the watch also subscribed to opens. Files a new
/// directory gained between the scan and its watch are picked up by scanning
/// it again after.
fn rescan(
    state: &ServerState,
    watchers: &mut Watchers,
    cache_base: &Path,
    changes: &mut ContentChanges,
    batch: &mut Batch,
    known: &Snapshot,
) {
    let roots = collapse_rescans(std::mem::take(&mut batch.rescans));
    let reason = batch.rescan_reason.take();
    for r in &roots {
        match &reason {
            Some(why) => eprintln!(
                "oj: file watcher lost events ({why}), rescanning {}",
                r.display()
            ),
            None => eprintln!("oj: file watcher lost events, rescanning {}", r.display()),
        }
    }
    let mut found = rescan_changes(&roots, &state.watch_ignored, &state.root, cache_base, known);
    if cfg!(target_os = "linux") {
        for d in &found.new_dirs {
            if let Err(err) = watchers.watch(d, notify::RecursiveMode::Recursive) {
                eprintln!("oj: cannot watch {}: {err}", d.display());
            }
            let mut late = Vec::new();
            scan_files(d, &mut late);
            found
                .added
                .extend(late.into_iter().filter(|f| !known.contains_key(f)));
        }
        found.added.sort();
        found.added.dedup();
    }
    for (kind, paths) in [
        (
            notify::EventKind::Create(notify::event::CreateKind::Any),
            found.added,
        ),
        (
            notify::EventKind::Modify(notify::event::ModifyKind::Data(
                notify::event::DataChange::Any,
            )),
            found.changed,
        ),
        (
            notify::EventKind::Remove(notify::event::RemoveKind::Any),
            found.removed,
        ),
    ] {
        if paths.is_empty() {
            continue;
        }
        let mut synth = notify::Event::new(kind);
        synth.paths = paths;
        state
            .watch_feed
            .publish(&synth, &state.watch_ignored, &state.root, cache_base);
        add_watched(state, cache_base, changes, batch, &synth);
    }
}

/// `ev` into the batch minus its excluded paths; true when it changed a
/// path the batch keeps.
fn add_watched(
    state: &ServerState,
    cache_base: &Path,
    changes: &mut ContentChanges,
    batch: &mut Batch,
    ev: &notify::Event,
) -> bool {
    let excluded = |p: &PathBuf| watch_excluded(&state.watch_ignored, &state.root, cache_base, p);
    if !ev.paths.iter().any(excluded) {
        return batch.add(changes, ev);
    }
    let mut kept = ev.clone();
    kept.paths.retain(|p| !excluded(p));
    !kept.paths.is_empty() && batch.add(changes, &kept)
}
