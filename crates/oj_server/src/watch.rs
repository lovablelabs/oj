use super::*;

/// `server.watch.ignored` entries as globs. Vite hands them to chokidar, which
/// matches absolute paths; a relative pattern is kept as written (for
/// root-relative matching) and rooted at the project (for absolute paths).
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

/// A file the config imported (the extractor reports them, like Vite's
/// `configFileDependencies`): its change restarts the server too. Packages
/// under node_modules are left out, as in Vite.
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

/// True for tsconfig files whose change must re-run the tsconfig-aware
/// transform (Vite: any `/tsconfig.json` plus every .json its resolution
/// cache loaded; the name pattern covers the `extends` bases like
/// tsconfig.base.json without tracking cache membership).
pub(crate) fn is_tsconfig_file(path: &Path) -> bool {
    path.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
        n == "tsconfig.json" || (n.starts_with("tsconfig.") && n.ends_with(".json"))
    })
}

/// Re-exec the current binary with the same arguments so a fresh process
/// re-reads config and .env. Rust sets CLOEXEC on the listening socket, so the
/// dev port is released as the image is replaced. Does not return on success.
pub(crate) mod child_groups {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static CHILDREN: Mutex<Vec<(u32, bool)>> = Mutex::new(Vec::new());

    pub fn register(pid: u32, own_group: bool) {
        CHILDREN.lock().unwrap().push((pid, own_group));
    }

    /// The fork reports a child it reaped or now owns the kill for; retiring
    /// the entry here is what keeps a later sweep from ever aiming at a
    /// recycled pid.
    pub fn unregister(pid: u32) {
        CHILDREN.lock().unwrap().retain(|(p, _)| *p != pid);
    }

    /// SIGKILL every registered child (its whole group when it leads one) and
    /// reap the corpses, so an exec'd image inherits neither survivors nor
    /// zombies. Drains in rounds: a child spawned while the sweep runs lands
    /// in the emptied registry and is taken by the next round.
    pub fn kill_all() -> usize {
        let mut killed = 0usize;
        #[cfg(unix)]
        for _round in 0..3 {
            let children: Vec<(u32, bool)> = std::mem::take(&mut *CHILDREN.lock().unwrap());
            if children.is_empty() {
                break;
            }
            for (pid, own_group) in &children {
                let pid_i = *pid as i32;
                // Never trust a stale entry: an own-group child still leads
                // its group iff getpgid(pid) == pid, and a direct child must
                // still exist. (The fork retires reaped children, so stale
                // entries are rare; this is the second lock against pid
                // recycling. A recycled pid that happens to lead its own new
                // group remains a theoretical TOCTOU.)
                let target = if *own_group {
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
            // Reap OUR direct children only, each with a small bound — a
            // per-pid wait can never stall on some unrelated live child the
            // way a waitpid(-1) sweep did.
            for (pid, _) in &children {
                let deadline = Instant::now() + Duration::from_millis(200);
                loop {
                    let r =
                        unsafe { libc::waitpid(*pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
                    if r != 0 || Instant::now() >= deadline {
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
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
    // In-process plugin hosts chdir the whole process to their app root, so a
    // bare re-exec would resolve relative CLI args (`oj dev ./web`) against
    // the wrong directory. Restart from where oj was launched.
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

pub(crate) fn spawn_watcher(state: Arc<ServerState>) {
    std::thread::spawn(move || {
        use notify::{RecursiveMode, Watcher};

        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(tx) {
            Ok(w) => w,
            Err(err) => {
                eprintln!("oj: file watcher failed to start: {err}");
                return;
            }
        };
        // Watch each top-level entry except node_modules/.oj-cache/dist/.git
        // rather than the whole root: those dirs are huge and, in the case of
        // .oj-cache, rewritten by oj on every compile -- recursively watching
        // them floods the watcher (notably Linux inotify) with self-inflicted
        // events. Skipping them at watch time is more robust than filtering
        // after the fact.
        let ignore = |name: &std::ffi::OsStr| {
            matches!(
                name.to_str(),
                Some("node_modules" | ".oj-cache" | "dist" | ".git")
            )
        };
        let mut watched_any = false;
        if let Ok(entries) = std::fs::read_dir(&state.root) {
            for entry in entries.flatten() {
                if ignore(&entry.file_name()) {
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
        // Fall back to a recursive root watch only if nothing else could be
        // watched (e.g. an otherwise-empty root).
        if !watched_any {
            if let Err(err) = watcher.watch(&state.root, RecursiveMode::Recursive) {
                eprintln!("oj: cannot watch {}: {err}", state.root.display());
                return;
            }
        }

        use std::sync::mpsc::RecvTimeoutError;
        let debounce_ms: u64 = std::env::var("OJ_HMR_DEBOUNCE_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(10);
        // Paths this watcher has already reported once. FSEvents keeps a file's
        // "created" flag on later events for a while, so a Create for a path seen
        // before is an edit (chokidar tracks the same distinction by its own
        // state, emitting `add` once and `change` after).
        let mut seen_paths: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        let mut changes = ContentChanges::new();
        loop {
            let first = match rx.recv() {
                Ok(Ok(ev)) => ev,
                Ok(Err(_)) => continue,
                Err(_) => break,
            };
            let first_paths = changes.changed_paths(&first);
            if first_paths.is_empty() {
                continue;
            }
            // Which of the debounced paths the watcher saw come into existence:
            // Vite's watcher tells plugins "create" for those (hotUpdate /
            // watchChange type), "update" for edits and "delete" for removals.
            let mut created: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            if matches!(first.kind, notify::EventKind::Create(_)) {
                created.extend(first_paths.iter().cloned());
            }
            let mut paths: std::collections::HashSet<PathBuf> = first_paths.into_iter().collect();
            loop {
                match rx.recv_timeout(Duration::from_millis(debounce_ms)) {
                    Ok(Ok(ev)) => {
                        let changed = changes.changed_paths(&ev);
                        if matches!(ev.kind, notify::EventKind::Create(_)) {
                            created.extend(changed.iter().cloned());
                        }
                        paths.extend(changed);
                    }
                    Ok(Err(_)) => {}
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            let paths: Vec<PathBuf> = paths
                .into_iter()
                .filter(|p| !is_watch_ignored(&state.watch_ignored, &state.root, p))
                .collect();
            if paths.is_empty() {
                continue;
            }
            created.retain(|p| !seen_paths.contains(p));
            seen_paths.extend(paths.iter().cloned());
            // A config or .env change can't be hot-applied (config is read once at
            // startup), so restart the process to pick it up — matching Vite.
            if paths
                .iter()
                .any(|p| is_restart_trigger(p) || is_config_dependency(p))
            {
                restart_process();
            }
            // Vite's reloadOnTsconfigChange: a tsconfig change clears the
            // tsconfig cache, invalidates every module graph and forces a full
            // reload ("the nuclear option"). The compile key folds the
            // class-field semantics in, so cleared discovery alone makes stale
            // persistent-cache entries unreachable.
            if paths.iter().any(|p| is_tsconfig_file(p)) {
                oj_compiler::tsconfig::clear_cache();
                state.mtime_keys.lock().unwrap().clear();
                state.memory.lock().unwrap().clear();
                let _ = state
                    .reload_tx
                    .send(full_reload_frame("tsconfig change", None, None));
            }
            if !state.hmr_enabled {
                continue;
            }
            if let Some(gate) = &state.hmr_gate {
                if gate.hold(&state, &paths) {
                    continue;
                }
            }
            let messages = state.rt.block_on(decide(&state, &paths, &created));
            if messages.is_empty() {
                continue;
            }
            state.dir_cache.lock().unwrap().clear();
            for message in messages {
                let _ = state.reload_tx.send(message);
            }
        }
    });
}
