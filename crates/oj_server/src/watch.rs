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

/// True for config / env files whose change requires a full server restart
/// (they are read once at startup and cannot be hot-applied).
pub(crate) fn is_restart_trigger(path: &Path) -> bool {
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name == ".env" || name.starts_with(".env.") {
        return true;
    }
    let stem_ext = |bases: &[&str], exts: &[&str]| {
        bases
            .iter()
            .any(|b| exts.iter().any(|e| name == format!("{b}.{e}")))
    };
    stem_ext(
        &[
            "vite.config",
            "oj.config",
            "postcss.config",
            "tailwind.config",
        ],
        &["ts", "js", "mjs", "cjs", "mts", "cts", "json"],
    )
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

pub(crate) fn restart_process() -> ! {
    eprintln!("{} config/env changed — restarting dev server", oj_brand());
    let killed = child_groups::kill_all();
    if killed > 0 {
        eprintln!("oj: restart killed {killed} child process(es)");
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("oj"));
    let args: Vec<String> = std::env::args().skip(1).collect();
    // In-process plugin hosts chdir the process to their app root; restart from
    // the launch dir so relative CLI args (`oj dev ./web`) resolve correctly.
    let launch_dir = STARTUP_CWD.get().filter(|d| d.is_dir());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new(&exe);
        cmd.args(&args);
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
        cmd.args(&args);
        if let Some(dir) = launch_dir {
            cmd.current_dir(dir);
        }
        let code = cmd.status().ok().and_then(|s| s.code()).unwrap_or(0);
        std::process::exit(code);
    }
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

/// The watcher thread's inbox: filesystem notifications, and directories to
/// add because an outside-root file was served (`ensure_watched_file`). One
/// channel, so the thread owns the watcher outright with no lock.
pub(crate) enum WatchMsg {
    Fs(notify::Result<notify::Event>),
    Dir(PathBuf),
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
    watcher: &mut notify::RecommendedWatcher,
    watched: &mut std::collections::HashSet<PathBuf>,
    dir: PathBuf,
) {
    use notify::{RecursiveMode, Watcher};
    if !watched.contains(&dir) && watcher.watch(&dir, RecursiveMode::NonRecursive).is_ok() {
        watched.insert(dir);
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

/// Watch each top-level root entry except the unwatched dirs; fall back to a
/// recursive root watch only if nothing else could be watched.
fn watch_root(watcher: &mut notify::RecommendedWatcher, root: &Path) -> notify::Result<()> {
    use notify::{RecursiveMode, Watcher};
    let mut watched_any = false;
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            if is_unwatched_dir(&entry.file_name()) {
                continue;
            }
            let path = entry.path();
            let mode = if path.is_dir() {
                RecursiveMode::Recursive
            } else {
                RecursiveMode::NonRecursive
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

/// One debounced set of changes.
#[derive(Default)]
struct Batch {
    paths: std::collections::HashSet<PathBuf>,
    /// Paths the watcher saw come into existence: plugins get "create" for
    /// those, "update" for edits, "delete" for removals.
    created: std::collections::HashSet<PathBuf>,
}

impl Batch {
    fn add(&mut self, changes: &mut ContentChanges, ev: &notify::Event) {
        let changed = changes.changed_paths(ev);
        if matches!(ev.kind, notify::EventKind::Create(_)) {
            self.created.extend(changed.iter().cloned());
        }
        self.paths.extend(changed);
    }
}

/// Act on a debounced batch: restart on config changes, full reload on
/// tsconfig changes, otherwise gate or dispatch HMR.
fn handle_batch(
    state: &Arc<ServerState>,
    paths: &[PathBuf],
    created: &std::collections::HashSet<PathBuf>,
) {
    // Config/.env can't be hot-applied (read once at startup): restart, as Vite does.
    if paths
        .iter()
        .any(|p| is_restart_trigger(p) || is_config_dependency(p))
    {
        restart_process();
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
    if !state.hmr_enabled {
        return;
    }
    if let Some(gate) = &state.hmr_gate {
        if gate.hold(state, paths) {
            return;
        }
    }
    let messages = state.rt.block_on(decide(state, paths, created));
    if messages.is_empty() {
        return;
    }
    state.dir_cache.lock().unwrap().clear();
    for message in messages {
        let _ = state.reload_tx.send(message);
    }
}

pub(crate) fn spawn_watcher(state: Arc<ServerState>, rx: std::sync::mpsc::Receiver<WatchMsg>) {
    std::thread::spawn(move || {
        use std::sync::mpsc::RecvTimeoutError;

        let tx = state.watch_tx.clone();
        let mut watcher = match notify::recommended_watcher(move |ev| {
            let _ = tx.send(WatchMsg::Fs(ev));
        }) {
            Ok(w) => w,
            Err(err) => {
                eprintln!("oj: file watcher failed to start: {err}");
                return;
            }
        };
        let mut served_dirs: std::collections::HashSet<PathBuf> = Default::default();
        if let Err(err) = watch_root(&mut watcher, &state.root) {
            eprintln!("oj: cannot watch {}: {err}", state.root.display());
            return;
        }

        let debounce_ms: u64 = oj_env::get().knobs.hmr_debounce_ms.unwrap_or(10);
        // FSEvents keeps a file's "created" flag on later events for a while, so
        // a Create for a path seen before is an edit (chokidar: add once, change after).
        let mut seen_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut changes = ContentChanges::new();
        loop {
            let first = match rx.recv() {
                Ok(WatchMsg::Fs(Ok(ev))) => ev,
                Ok(WatchMsg::Fs(Err(_))) => continue,
                Ok(WatchMsg::Dir(dir)) => {
                    watch_served_dir(&mut watcher, &mut served_dirs, dir);
                    continue;
                }
                Err(_) => break,
            };
            let mut batch = Batch::default();
            batch.add(&mut changes, &first);
            if batch.paths.is_empty() {
                continue;
            }
            loop {
                match rx.recv_timeout(Duration::from_millis(debounce_ms)) {
                    Ok(WatchMsg::Fs(Ok(ev))) => batch.add(&mut changes, &ev),
                    Ok(WatchMsg::Fs(Err(_))) => {}
                    Ok(WatchMsg::Dir(dir)) => watch_served_dir(&mut watcher, &mut served_dirs, dir),
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            let Batch { paths, mut created } = batch;
            let paths: Vec<PathBuf> = paths
                .into_iter()
                .filter(|p| !is_watch_ignored(&state.watch_ignored, &state.root, p))
                .collect();
            if paths.is_empty() {
                continue;
            }
            created.retain(|p| !seen_paths.contains(p));
            seen_paths.extend(paths.iter().cloned());
            handle_batch(&state, &paths, &created);
        }
    });
}
