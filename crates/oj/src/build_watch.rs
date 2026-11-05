// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! `oj build --watch` (the "full rebuild" strategy).
//!
//! Vite 8 drives rolldown's native watcher (`rolldown.watch()`, the NAPI
//! `BindingWatcher`). That API is not published in the `rolldown` Rust crate
//! oj depends on, so `oj` watches the project with `notify` and re-runs its
//! one-shot build on every debounced change. That is slower than rolldown's
//! incremental watcher but reuses the whole build pipeline (config reload,
//! plugin host, split CSS, HTML rewriting, manifests) unchanged, so output is
//! always a complete, correct bundle — and, like Vite, `outDir` is emptied and
//! rewritten on each rebuild.
//!
//! Only the plain `oj build` pipeline is wired to this: `--ssr`/lib go through
//! the same `build()` entry point and are covered too, while TanStack Start
//! (`start_dev::start_build`) rejects `--watch` for now.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Vite's `build.watch` subset oj honors. Rollup's other watcher options
/// (`include`/`exclude`, `skipWrite`, `clearScreen`) are not wired yet.
#[derive(Debug, Clone)]
pub struct WatchOptions {
    /// chokidar `ignored` globs (`build.watch.chokidar.ignored`).
    pub ignored: Vec<String>,
    /// chokidar `usePolling`: a polling watcher instead of native events.
    pub use_polling: bool,
    /// chokidar `interval` (polling only), default 100ms.
    pub poll_interval: Duration,
    /// Rollup `build.watch.buildDelay`, used as the debounce window.
    pub debounce: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            ignored: Vec::new(),
            use_polling: false,
            poll_interval: Duration::from_millis(100),
            debounce: Duration::from_millis(50),
        }
    }
}

/// Resolve whether watch is on and with which options. `--watch` forces it on;
/// otherwise the config's `build.watch` decides (`null`/`false` = off, `true`
/// or an object = on).
pub fn resolve(cli_watch: bool, config_watch: Option<&serde_json::Value>) -> Option<WatchOptions> {
    let config_enabled =
        config_watch.is_some_and(|v| !v.is_null() && *v != serde_json::Value::Bool(false));
    if !cli_watch && !config_enabled {
        return None;
    }
    let mut opts = WatchOptions::default();
    if let Some(v) = config_watch {
        apply_json(&mut opts, v);
    }
    Some(opts)
}

fn apply_json(opts: &mut WatchOptions, v: &serde_json::Value) {
    let Some(obj) = v.as_object() else {
        return;
    };
    if let Some(ms) = obj.get("buildDelay").and_then(serde_json::Value::as_u64) {
        opts.debounce = Duration::from_millis(ms);
    }
    let Some(c) = obj.get("chokidar").and_then(serde_json::Value::as_object) else {
        return;
    };
    if let Some(ignored) = c.get("ignored") {
        opts.ignored = string_or_list(ignored);
    }
    if let Some(poll) = c.get("usePolling").and_then(serde_json::Value::as_bool) {
        opts.use_polling = poll;
    }
    if let Some(interval) = c.get("interval").and_then(serde_json::Value::as_u64) {
        opts.poll_interval = Duration::from_millis(interval);
    }
}

fn string_or_list(v: &serde_json::Value) -> Vec<String> {
    match v {
        serde_json::Value::String(s) => vec![s.clone()],
        serde_json::Value::Array(a) => a
            .iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// chokidar matches `ignored` against absolute paths; a relative pattern is
/// kept as written and also rooted at the project, as the dev watcher does.
fn compile_ignored(root: &Path, ignored: &[String]) -> Vec<glob::Pattern> {
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
            // A bare directory name means "ignore that dir anywhere", as
            // chokidar treats it: its contents are never watched. Without this
            // `/app/src/local/x.ts` would not match the pattern `local`.
            if !raw.contains('/') && !raw.contains(['*', '?', '[', '{']) {
                if let Ok(p) = glob::Pattern::new(&format!("{rooted}/**")) {
                    out.push(p);
                }
                if let Ok(p) = glob::Pattern::new(&format!("**/{raw}/**")) {
                    out.push(p);
                }
            }
        }
    }
    out
}

fn is_user_ignored(patterns: &[glob::Pattern], root: &Path, path: &Path) -> bool {
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

/// Every path the build must not react to: the resolved outDir, chokidar's
/// fixed skips (Vite's `resolveChokidarOptions`), and the user's `ignored`.
fn is_ignored(root: &Path, out_dir: &Path, patterns: &[glob::Pattern], path: &Path) -> bool {
    if path.starts_with(out_dir) {
        return true;
    }
    if path.components().any(|c| {
        matches!(
            c.as_os_str().to_str(),
            Some("node_modules" | ".oj-cache" | ".git" | "test-results")
        )
    }) {
        return true;
    }
    is_user_ignored(patterns, root, path)
}

/// Top-level entries never watched: recursively watching node_modules/.oj-cache/
/// .git floods inotify with self-inflicted events, exactly as in the dev watcher.
fn is_unwatched_dir(name: &std::ffi::OsStr) -> bool {
    matches!(
        name.to_str(),
        Some("node_modules" | ".oj-cache" | ".git" | "test-results")
    )
}

/// Watch each top-level root entry except the unwatched dirs, plus the root
/// itself non-recursively so new top-level entries are seen. Falls back to a
/// recursive root watch only if nothing else could be watched. Mirrors the
/// dev watcher's `watch_root`; duplicated here so this path is independent of
/// the server's state.
fn watch_root<W: notify::Watcher>(
    watcher: &mut W,
    root: &Path,
    out_dir: &Path,
) -> notify::Result<()> {
    use notify::RecursiveMode;
    let root_watched = watcher.watch(root, RecursiveMode::NonRecursive).is_ok();
    let mut watched_any = root_watched;
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            // outDir itself, or a top-level dir that contains it (skipping the
            // ancestor keeps an outDir nested in a source dir from echoing).
            if path == out_dir || out_dir.starts_with(&path) {
                continue;
            }
            if is_unwatched_dir(&entry.file_name()) {
                continue;
            }
            let mode = if path.is_dir() {
                RecursiveMode::Recursive
            } else if !root_watched || entry.file_type().is_ok_and(|t| t.is_symlink()) {
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

/// Run `rebuild` once per debounced batch of file changes, forever. `rebuild`
/// is expected to be a full one-shot build; its errors are logged and the loop
/// keeps watching (Vite's watch mode never exits on a failed rebuild).
pub async fn run<F, Fut>(
    root: PathBuf,
    out_dir: PathBuf,
    opts: WatchOptions,
    mut rebuild: F,
) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let patterns = compile_ignored(&root, &opts.ignored);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<Vec<PathBuf>>();
    let (handler_root, handler_out, handler_patterns) =
        (root.clone(), out_dir.clone(), patterns.clone());
    let handler = move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else {
            return;
        };
        if matches!(ev.kind, notify::EventKind::Access(_)) {
            return;
        }
        let changed: Vec<PathBuf> = ev
            .paths
            .into_iter()
            .filter(|p| !is_ignored(&handler_root, &handler_out, &handler_patterns, p))
            .collect();
        if !changed.is_empty() {
            let _ = tx.send(changed);
        }
    };
    if opts.use_polling {
        let mut watcher = notify::PollWatcher::new(
            handler,
            notify::Config::default().with_poll_interval(opts.poll_interval),
        )?;
        watch_loop(&mut watcher, &root, &out_dir, &opts, rx, &mut rebuild).await
    } else {
        let mut watcher = notify::recommended_watcher(handler)?;
        watch_loop(&mut watcher, &root, &out_dir, &opts, rx, &mut rebuild).await
    }
}

async fn watch_loop<W, F, Fut>(
    watcher: &mut W,
    root: &Path,
    out_dir: &Path,
    opts: &WatchOptions,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<Vec<PathBuf>>,
    rebuild: &mut F,
) -> anyhow::Result<()>
where
    W: notify::Watcher,
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    watch_root(watcher, root, out_dir)?;
    eprintln!("oj: watching for file changes...");
    loop {
        if rx.recv().await.is_none() {
            return Ok(());
        }
        // Settle: wait until no event has arrived for `debounce`, so one save
        // (or a burst from an editor) triggers a single rebuild. Events that
        // land during the rebuild stay queued and trigger the next one.
        loop {
            match tokio::time::timeout(opts.debounce, rx.recv()).await {
                Ok(Some(_)) => continue,
                Ok(None) => return Ok(()),
                Err(_) => break,
            }
        }
        let started = std::time::Instant::now();
        match rebuild().await {
            Ok(()) => eprintln!("oj: built in {}ms.", started.elapsed().as_millis()),
            Err(e) => eprintln!("oj error during rebuild: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_honors_cli_and_config_and_parses_the_chokidar_subset() {
        // Neither side asks for watch.
        assert!(resolve(false, None).is_none());
        assert!(resolve(false, Some(&serde_json::Value::Bool(false))).is_none());
        assert!(resolve(false, Some(&serde_json::Value::Null)).is_none());
        // `--watch` forces it on with defaults.
        assert_eq!(
            resolve(true, None).unwrap().debounce,
            Duration::from_millis(50)
        );
        // The config's `{}`/`true` enables it, and options are read.
        let cfg = serde_json::json!({
            "buildDelay": 25,
            "chokidar": { "ignored": ["**/*.log", "tmp"], "usePolling": true, "interval": 7 }
        });
        let opts = resolve(false, Some(&cfg)).unwrap();
        assert_eq!(opts.debounce, Duration::from_millis(25));
        assert_eq!(opts.poll_interval, Duration::from_millis(7));
        assert!(opts.use_polling);
        assert_eq!(
            opts.ignored,
            vec!["**/*.log".to_string(), "tmp".to_string()]
        );
        // A non-object value (true) just enables the defaults.
        let opts = resolve(false, Some(&serde_json::Value::Bool(true))).unwrap();
        assert!(!opts.use_polling && opts.ignored.is_empty());
    }

    #[test]
    fn ignored_paths_cover_out_dir_fixed_names_and_user_globs() {
        let root = Path::new("/app");
        let out_dir = Path::new("/app/dist");
        let patterns = compile_ignored(root, &["**/*.snap".to_string(), "local".to_string()]);
        // outDir and chokidar's fixed skips.
        assert!(is_ignored(
            root,
            out_dir,
            &patterns,
            Path::new("/app/dist/assets/x.js")
        ));
        assert!(is_ignored(
            root,
            out_dir,
            &patterns,
            Path::new("/app/node_modules/x/index.js")
        ));
        assert!(is_ignored(
            root,
            out_dir,
            &patterns,
            Path::new("/app/.oj-cache/c")
        ));
        assert!(is_ignored(
            root,
            out_dir,
            &patterns,
            Path::new("/app/.git/HEAD")
        ));
        // The user's globs, both root-relative and rooted-at-the-project forms.
        assert!(is_ignored(
            root,
            out_dir,
            &patterns,
            Path::new("/app/src/a.snap")
        ));
        assert!(is_ignored(
            root,
            out_dir,
            &patterns,
            Path::new("/app/local/x.ts")
        ));
        assert!(is_ignored(
            root,
            out_dir,
            &patterns,
            Path::new("/app/src/local/x.ts")
        ));
        // A normal source file is not ignored.
        assert!(!is_ignored(
            root,
            out_dir,
            &patterns,
            Path::new("/app/src/main.ts")
        ));
    }
}
