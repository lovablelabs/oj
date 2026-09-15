// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;

use axum::{
    extract::{
        ws::{Message, WebSocket},
        FromRequestParts, Request, State, WebSocketUpgrade,
    },
    http::{header, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use futures_util::SinkExt;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::broadcast;

struct Runner {
    stdin: ChildStdin,
    lines: Lines<BufReader<ChildStdout>>,
    _child: Child,
    /// The SSR runner's loopback HTTP port (requests go over HTTP so bodies stay
    /// binary, responses stream and requests run concurrently); None for the
    /// line-protocol services (css host).
    http_port: Option<u16>,
}

struct StartState {
    proxy_prefixes: Vec<String>,
    /// Live plugin-middleware state, shared with oj_server: the middleware port
    /// and whether worker environments serve the documents. A slow plugin host
    /// (many plugins, Miniflare) activates it after boot, so every read goes
    /// through it per request instead of a boot-time snapshot.
    plugin_serve: Arc<oj_server::PluginServe>,
    runner: Arc<tokio::sync::Mutex<Runner>>,
    bundle: std::sync::RwLock<Arc<oj_cache::start_bundle::PinnedBundle>>,
    verify: oj_cache::integrity::VerifyMode,
    live_reload: PathBuf,
    workspace_root: PathBuf,
    reload_tx: broadcast::Sender<()>,
    // The oj_server /__ws broadcast: the channel the editor reads boot +
    // update narration frames from (the start path's own /@oj-start/hmr socket
    // only drives the app iframe's live reload).
    ws_tx: broadcast::Sender<String>,
    css_host: Option<Arc<tokio::sync::Mutex<Runner>>>,
    /// The dev mode (`oj dev --mode`), handed to every node script respawn.
    mode: String,
    /// The HMR gate, when the editor drives it: rebuilds still happen at once, the
    /// page reload waits for the flush.
    gate: Option<oj_server::HmrGateHandle>,
    /// Set on a source change while the runner is lazy (worker environments
    /// serve the documents; the runner is only a fallback, kept cold); the next
    /// request that can reach the runner reloads it first (`ensure_runner_fresh`).
    /// Also set by the plugin-serve activation handler when the middleware comes
    /// up after boot, so edits from the degraded window (which reloaded the
    /// runner eagerly, or raced the flip and took neither path) never leave a
    /// stale fallback. In an Arc so the handler closure captures only the flag,
    /// not the whole state (PluginServe is state-held: that would cycle).
    runner_dirty: Arc<std::sync::atomic::AtomicBool>,
}

impl StartState {
    /// Cloudflare worker environments serve the documents; the runner is only a
    /// fallback, kept cold and reloaded lazily (see `runner_dirty`) instead of
    /// eagerly on every edit. Live: a slow plugin host flips this after boot.
    fn lazy_runner(&self) -> bool {
        self.plugin_serve.runner_environments()
    }
}

// What the watcher forwards and rebuilds on: not the generated route tree (its
// writes would loop the generator) and not bare directory events (Linux inotify
// emits a parent-dir event alongside the file write; Vite never hands
// directories to hotUpdate hooks either).
fn watch_relevant(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    !name.contains("routeTree.gen") && !p.is_dir()
}

// Vite's watcher distinguishes add/change/unlink; notify's batched events are
// classified at send time: gone from disk is a delete, seen as a Create in this
// batch is a create, anything else an update.
fn change_type(p: &Path, created: &std::collections::HashSet<PathBuf>) -> &'static str {
    if !p.exists() {
        "delete"
    } else if created.contains(p) {
        "create"
    } else {
        "update"
    }
}

// Static hint that the app uses @cloudflare/vite-plugin, readable before the
// plugin host boots (the definitive flag is the host's serve info, live on
// BuiltApp::plugin_serve). A plain text search of the config file: false for
// every non-Cloudflare app, so their boot path is untouched. Used only to gate
// the prewarm decision below.
fn config_mentions_cloudflare_plugin(root: &Path, config: &Option<PathBuf>) -> bool {
    let file = config
        .clone()
        .or_else(|| oj_server::plugins::vite_config_file(root));
    file.and_then(|f| std::fs::read_to_string(f).ok())
        .is_some_and(|s| s.contains("@cloudflare/vite-plugin"))
}

// A --config path is resolved against the app root, the way `build` resolves
// it, so `oj dev --config vite.config.mjs app` and `oj build --config ... app`
// mean the same file. An absolute path is already the answer.
fn config_path(root: &Path, config: Option<PathBuf>) -> Option<PathBuf> {
    config.map(|c| if c.is_absolute() { c } else { root.join(c) })
}

pub async fn start_dev(
    root: PathBuf,
    port: Option<u16>,
    host: Option<String>,
    config: Option<PathBuf>,
    mode: Option<String>,
) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("app root not found: {}: {e}", root.display()))?;
    // Vite's `--mode` for `serve`: selects `.env.<mode>`, import.meta.env.MODE and
    // the mode plugin hooks see, on the SSR loader and the client bundle alike.
    let mode = mode
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "development".to_string());

    // Same order `build` uses: pin the override before anything reads a config,
    // so the Rust side and the plugin host agree on which file is the config.
    let config = config_path(&root, config);
    let cf_hint = config_mentions_cloudflare_plugin(&root, &config);
    if let Some(cfg) = &config {
        oj_server::plugins::set_vite_config_override(cfg.clone());
    }
    let cache = oj_cache::cache_root(&root).join("start");
    oj_server::prepare_cache_root(&root);
    oj_server::write_start_assets(&cache)?;
    oj_server::plugins::prepare_ssr_bridge(&root);
    oj_server::boot_phase("start_dev begin");

    let built_task = tokio::spawn(
        oj_server::DevServer {
            root: root.clone(),
            port,
            bundle: false,
            host,
            config,
            enable_cache: false,
            no_cache: false,
            lazy: false,
            mode: Some(mode.clone()),
        }
        .build_app(),
    );

    let route_tree = {
        let (root, cache, mode) = (root.clone(), cache.clone(), mode.clone());
        tokio::task::spawn_blocking(move || generate_route_tree(&root, &cache, &mode))
    };
    let resolver = {
        let (root, cache, mode) = (root.clone(), cache.clone(), mode.clone());
        tokio::task::spawn_blocking(move || generate_server_fn_resolver(&root, &cache, &mode))
    };
    route_tree.await??;
    oj_server::boot_phase("route tree ready");
    let bundle = {
        let (root, cache, mode) = (root.clone(), cache.clone(), mode.clone());
        tokio::task::spawn_blocking(move || bundle_client_entry_cached(&root, &cache, &mode))
    };
    resolver.await??;
    oj_server::boot_phase("resolver ready");
    let runner = Arc::new(tokio::sync::Mutex::new(
        spawn_start_runner(&root, &cache, &mode).await?,
    ));
    oj_server::boot_phase("runner spawned");
    let (reload_tx, _) = broadcast::channel::<()>(16);
    // Whether the plugin's worker environments serve the documents is known
    // only once the plugin host announces its serve info (the `{ ojServeInfo }`
    // push, mirrored on the host's watch channel); the cf_hint keeps the
    // non-Cloudflare prewarm overlapping the build exactly as before, while a
    // Cloudflare config holds the prewarm until the serve info is KNOWN —
    // bounded, so a host that never comes up still gets a warm runner — and
    // skips it iff the worker environments render (warming the runner is
    // wasted CPU then).
    let (cf_tx, cf_rx) = tokio::sync::oneshot::channel::<Option<PrewarmHold>>();
    {
        let runner = Arc::clone(&runner);
        let reload_tx = reload_tx.clone();
        tokio::spawn(async move {
            if cf_hint {
                if let Ok(Some(mut hold)) = cf_rx.await {
                    // Held until the serve info is KNOWN or there is wedge
                    // EVIDENCE (host death, a burned init window, the host's
                    // own init deadline) — see hold_prewarm_for_serve_info.
                    // On an evidence release the runner is prewarmed anyway;
                    // a LATE serve-info activation still supersedes
                    // (prewarming and then discovering worker environments
                    // render is only the pre-PR waste).
                    if let Some(known) = hold_prewarm_for_serve_info(&mut hold).await {
                        if known.middleware_port.is_some() && known.runner_environments {
                            oj_server::boot_phase("prewarm skipped (worker environments)");
                            return;
                        }
                    }
                }
            }
            let _ = forward(&runner, "GET".into(), "/".into(), &header::HeaderMap::new(), None).await;
            oj_server::boot_phase("prewarm complete");
            if revalidate_runner(&runner).await {
                let _ = reload_tx.send(());
            }
            oj_server::boot_phase("revalidate complete");
        });
    }
    let (bundle_res, built_res) = tokio::join!(bundle, built_task);
    let pinned = bundle_res??;
    let built = built_res??;
    let _ = cf_tx.send(built.plugin_host.as_ref().map(|h| PrewarmHold {
        updates: h.serve_info_updates(),
        host_gone: h.host_gone_updates(),
        init_failed: h.init_failure_updates(),
        init_deadline: h.init_deadline_at(),
        confirm: oj_server::plugins::plugin_rpc_timeout(),
    }));
    oj_server::boot_phase("bundle+build joined");
    let css_host = if app_uses_tailwind(&root) {
        spawn_node_service(&root, &cache.join("css-host.mjs"), &mode)
            .await
            .ok()
            .map(|r| Arc::new(tokio::sync::Mutex::new(r)))
    } else {
        None
    };
    let state = Arc::new(StartState {
        proxy_prefixes: built.proxy_prefixes.clone(),
        plugin_serve: Arc::clone(&built.plugin_serve),
        runner,
        bundle: std::sync::RwLock::new(Arc::new(pinned)),
        verify: oj_cache::integrity::VerifyMode::from_env(),
        live_reload: cache.join("live-reload.js"),
        workspace_root: workspace_root(&root),
        reload_tx: reload_tx.clone(),
        ws_tx: built.reload_tx.clone(),
        gate: built.hmr_gate.clone(),
        css_host,
        mode: mode.clone(),
        runner_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    });

    // The activation handler: when the plugin middleware comes up after boot,
    // PluginServe::set runs this synchronously BEFORE readers can observe the
    // flipped mode, marking the fallback runner dirty. That closes the
    // two-time-read window (watcher stores runner_dirty only when lazy, the
    // rebundle worker reloads eagerly only when not lazy: an edit racing the
    // flip could take neither path) — the next request through the fallback
    // reloads the runner first. The worker environments get their own catch-up
    // (the full-reload resync) from oj_server's late-activation task.
    {
        let runner_dirty = Arc::clone(&state.runner_dirty);
        state.plugin_serve.set_on_activate(Box::new(move || {
            runner_dirty.store(true, std::sync::atomic::Ordering::SeqCst);
        }));
        // The hook registers after DevServer::build returned, and the
        // late-activation task is already running: an activation landing in
        // that window ran with the hook unarmed. The documented current-state
        // check re-arms once inline — only the dirty flag needs this catch-up,
        // the worker resync fires from the late-activation task itself,
        // independent of the hook. A normal boot-time fill is not "late" and
        // never triggers a spurious runner reload here.
        if state.plugin_serve.activated_late() {
            state
                .runner_dirty
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    // A gate flush (the editor's POST /__hmr_flush, or the hold cap) releases
    // the reload the watcher held; the rebuild itself already happened.
    if let Some(gate) = &state.gate {
        let mut flushes = gate.subscribe_flush();
        let reload_tx = state.reload_tx.clone();
        tokio::spawn(async move {
            while flushes.recv().await.is_ok() {
                let _ = reload_tx.send(());
            }
        });
    }
    spawn_start_watcher(root.clone(), cache.clone(), Arc::clone(&state));
    {
        let (root, mode) = (root.clone(), mode.clone());
        tokio::task::spawn_blocking(move || {
            start_bundle_store(&root, &mode).prune(oj_cache::start_bundle::DEFAULT_PRUNE_BUDGET_BYTES);
        });
    }

    let app = built.router.layer(axum::middleware::from_fn_with_state(
        Arc::clone(&state),
        start_route,
    ));

    let (listener, port) =
        oj_server::bind_dev_listener(built.host, built.port, built.strict_port).await?;
    oj_server::boot_phase("listening");
    {
        let root = root.clone();
        tokio::spawn(async move {
            let code = shutdown_signal().await;
            oj_server::plugins::cleanup_ssr_bridge(&root);
            std::process::exit(code);
        });
    }
    println!("  {} dev (tanstack start)", oj_server::oj_brand());
    let url = format!("http://localhost:{}/", port);
    println!("  {}", oj_server::link(&url, &oj_server::cell(&url)));
    axum::serve(listener, app).await?;
    Ok(())
}

async fn shutdown_signal() -> i32 {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = match signal(SignalKind::terminate()) {
            Ok(s) => s,
            Err(_) => return tokio::signal::ctrl_c().await.map(|_| 130).unwrap_or(130),
        };
        tokio::select! {
            _ = tokio::signal::ctrl_c() => 130,
            _ = term.recv() => 143,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        130
    }
}

async fn revalidate_runner(runner: &Arc<tokio::sync::Mutex<Runner>>) -> bool {
    let mut guard = runner.lock().await;
    if guard
        .stdin
        .write_all(b"{\"cmd\":\"revalidate\"}\n")
        .await
        .is_err()
        || guard.stdin.flush().await.is_err()
    {
        return false;
    }
    let Ok(Some(line)) = guard.lines.next_line().await else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&line)
        .ok()
        .and_then(|v| v.get("reloaded").and_then(|b| b.as_bool()))
        .unwrap_or(false)
}

/// On the Cloudflare path the runner is only a fallback: edits mark it dirty
/// instead of reloading it, and the reload happens here, before a request can
/// reach it (the document fallback, and anything the plugin middleware may pipe
/// upstream via x-oj-forward-to).
async fn ensure_runner_fresh(state: &StartState) {
    if state.lazy_runner()
        && state
            .runner_dirty
            .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        reload_runner(state).await;
    }
}

async fn reload_runner(state: &StartState) {
    let mut guard = state.runner.lock().await;
    if guard
        .stdin
        .write_all(b"{\"cmd\":\"reload\"}\n")
        .await
        .is_err()
    {
        return;
    }
    let _ = guard.stdin.flush().await;
    let _ = guard.lines.next_line().await;
}

fn list_route_files(root: &Path) -> std::collections::BTreeSet<PathBuf> {
    let mut out = std::collections::BTreeSet::new();
    fn walk(dir: &Path, out: &mut std::collections::BTreeSet<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "ts" || x == "tsx") {
                out.insert(p);
            }
        }
    }
    walk(&root.join("src").join("routes"), &mut out);
    out
}

fn workspace_root(app: &Path) -> PathBuf {
    let mut best = app.to_path_buf();
    let mut cur = app;
    while let Some(parent) = cur.parent() {
        if parent.join("node_modules").is_dir() {
            best = parent.to_path_buf();
        }
        cur = parent;
    }
    best
}

fn asset_mime(ext: &str) -> &'static str {
    match ext {
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript",
        "json" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "wasm" => "application/wasm",
        _ => "application/octet-stream",
    }
}

const RELOAD_CLIENT: &str = "<script type=\"module\" src=\"/@oj-start/live-reload.js\"></script>";

/// One rebundle run's inputs, handed from the watcher thread to the coalescing
/// rebundle worker. Batches landing while a rebundle is in flight merge into
/// the single pending run (paths union), so at most one run ever queues.
#[derive(Default)]
struct PendingRebundle {
    /// Every changed path of the merged batches (relevant and not): the HMR
    /// gate records the full set, exactly as the inline loop did.
    paths: std::collections::HashSet<PathBuf>,
    /// The settled worker-invalidate sends already in flight for the merged
    /// batches: this run's browser reload waits for them, so a reload never
    /// lands while the worker environments still hold stale modules.
    invalidates: Vec<tokio::task::JoinHandle<()>>,
    /// The newest merged batch number: its done-frame clears the editor pill.
    batch: u64,
}

/// Spawn one worker-environment invalidate for a batch's relevant paths, off
/// the watcher thread. Returns the send's handle (None: no plugin middleware,
/// or nothing relevant) so the settled call site can make the browser reload
/// wait on it; the changed-paths Vec is only built once the port guard passed,
/// so non-Cloudflare sessions pay nothing per event.
fn spawn_mw_invalidate(
    rt: &tokio::runtime::Handle,
    state: &StartState,
    paths: &std::collections::HashSet<PathBuf>,
    created: &std::collections::HashSet<PathBuf>,
) -> Option<tokio::task::JoinHandle<()>> {
    let port = state.plugin_serve.mw_port()?;
    let changed: Vec<(String, &'static str)> = paths
        .iter()
        .filter(|p| watch_relevant(p))
        .map(|p| (p.to_string_lossy().into_owned(), change_type(p, created)))
        .collect();
    if changed.is_empty() {
        return None;
    }
    Some(rt.spawn(async move {
        oj_server::notify_plugin_mw_invalidate(port, &changed).await;
    }))
}

/// The files the regen steps write, which the watcher deliberately never
/// forwards: routeTree.gen events are filtered (`watch_relevant`) to stop
/// generator-feedback rebuild loops, and the server-fn resolver lives in the
/// cache dir the watcher does not even see. The rebundle worker snapshots
/// their content hashes around a run and pushes the ones the run actually
/// rewrote to the worker environments itself.
fn regen_output_files(root: &Path, cache: &Path) -> [PathBuf; 2] {
    // The generator honors tsr.config.json's `generatedRouteTree` (generate.mjs
    // passes oj's default under the file, not over it): snapshot the tree the
    // app actually configured, defaulting to src/routeTree.gen.ts.
    let configured = std::fs::read_to_string(root.join("tsr.config.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            let rel = v.get("generatedRouteTree")?.as_str()?.to_owned();
            Some(root.join(rel.trim_start_matches("./")))
        });
    [
        configured.unwrap_or_else(|| root.join("src").join("routeTree.gen.ts")),
        cache.join("server-fn-resolver.mjs"),
    ]
}

/// Content hash for regen-output change detection (None: missing/unreadable).
fn file_content_hash(p: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(p).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    Some(h.finish())
}

/// The regen outputs whose content a run rewrote, as invalidate changes.
fn changed_regen_outputs(
    files: &[PathBuf],
    before: &[Option<u64>],
) -> Vec<(String, &'static str)> {
    files
        .iter()
        .zip(before)
        .filter(|(f, before)| file_content_hash(f) != **before)
        .map(|(f, _)| (f.to_string_lossy().into_owned(), "update"))
        .collect()
}

fn spawn_start_watcher(root: PathBuf, cache: PathBuf, state: Arc<StartState>) {
    let rt = tokio::runtime::Handle::current();
    let pending: Arc<std::sync::Mutex<Option<PendingRebundle>>> =
        Arc::new(std::sync::Mutex::new(None));
    let wake = Arc::new(tokio::sync::Notify::new());
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    rt.spawn(rebundle_worker(
        root.clone(),
        cache.clone(),
        Arc::clone(&state),
        Arc::clone(&pending),
        Arc::clone(&wake),
        Arc::clone(&shutdown),
    ));
    // Ends the rebundle worker on every watcher-thread exit path (watch error,
    // channel disconnect), so it does not idle forever holding the state.
    struct StopWorker(Arc<std::sync::atomic::AtomicBool>, Arc<tokio::sync::Notify>);
    impl Drop for StopWorker {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
            self.1.notify_one();
        }
    }
    let stop_worker = StopWorker(Arc::clone(&shutdown), Arc::clone(&wake));
    // The watcher thread only receives, classifies, settles and hands batches
    // off; it never blocks on a rebundle or an HTTP send (Vite's chokidar
    // callbacks run per event too). A second edit landing while the first
    // edit's rebundle is in flight is invalidated promptly instead of queueing
    // behind the bundler.
    std::thread::spawn(move || {
        use notify::{RecursiveMode, Watcher};
        use std::sync::mpsc::RecvTimeoutError;

        let _stop_worker = stop_worker;
        let (tx, rx) = std::sync::mpsc::channel();
        let mut watcher = match notify::recommended_watcher(tx) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("oj start: file watcher failed: {e}");
                return;
            }
        };
        let src = root.join("src");
        if let Err(e) = watcher.watch(&src, RecursiveMode::Recursive) {
            eprintln!("oj start: cannot watch {}: {e}", src.display());
            return;
        }
        let mut batch = 0u64;
        // Attribute-only events (the atime update a read causes on Linux) are
        // not changes: the client rebundle reads every source file, which
        // otherwise looked like an edit of every source file and rebuilt again.
        let mut changes = oj_server::ContentChanges::new();
        loop {
            let mut created: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            let mut paths: std::collections::HashSet<PathBuf> = match rx.recv() {
                Ok(Ok(ev)) => {
                    if matches!(ev.kind, notify::EventKind::Create(_)) {
                        created.extend(ev.paths.iter().cloned());
                    }
                    changes.changed_paths(&ev).into_iter().collect()
                }
                Ok(Err(_)) => continue,
                Err(_) => break,
            };
            // A request can arrive milliseconds after a save: invalidate the
            // worker environments on the first raw event, before the settle
            // window and the rebuild, so a fast follow-up document is not
            // rendered from stale modules. The settled batch is invalidated
            // again below; the host dedups by content identity, so the repeat
            // send only costs work when a write landed inside the settle
            // window and actually changed content. The OS watcher's own
            // delivery latency is the remaining, unclosable window.
            let _ = spawn_mw_invalidate(&rt, &state, &paths, &created);
            loop {
                match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(Ok(ev)) => {
                        if matches!(ev.kind, notify::EventKind::Create(_)) {
                            created.extend(ev.paths.iter().cloned());
                        }
                        paths.extend(changes.changed_paths(&ev));
                    }
                    Ok(Err(_)) => {}
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            if paths.is_empty() {
                continue;
            }
            // Rebuild only on a real source-file change. Ignore the generated
            // route tree and its atomic-write temp siblings, plus bare directory
            // events (Linux inotify emits a parent-dir event alongside the file
            // write, which would otherwise slip past a filename-only filter and
            // retrigger the generator on every reload).
            let relevant = paths.iter().any(|p| watch_relevant(p));
            if !relevant {
                continue;
            }
            // Narrate the compile batch to the editor: "Applying changes…" while
            // it rebuilds; the rebundle worker sends the done-frame so the pill
            // clears.
            batch += 1;
            let _ = state.ws_tx.send(oj_server::update_progress_frame(
                batch, "watch", 0, 0, None, false,
            ));
            // Invalidate the plugin's worker DevEnvironments so a Cloudflare
            // app re-renders with the changed modules (not stale SSR). Spawned
            // promptly per settled batch, never behind an in-flight rebundle;
            // the rebundle worker awaits it before that run's browser reload.
            let invalidate = spawn_mw_invalidate(&rt, &state, &paths, &created);
            if state.lazy_runner() {
                state
                    .runner_dirty
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            {
                let mut slot = pending
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let run = slot.get_or_insert_with(PendingRebundle::default);
                run.paths.extend(paths.iter().cloned());
                run.invalidates.extend(invalidate);
                run.batch = batch;
            }
            wake.notify_one();
        }
    });
}

/// The coalescing rebundle worker: route-tree/server-fn regeneration and the
/// client rebundle run here, off the watcher thread. Batches landing mid-run
/// merge into the one pending run that starts when the current one finishes
/// (never more than one queued); the browser reload still fires once per
/// completed rebundle, after that run's settled invalidates, as before.
async fn rebundle_worker(
    root: PathBuf,
    cache: PathBuf,
    state: Arc<StartState>,
    pending: Arc<std::sync::Mutex<Option<PendingRebundle>>>,
    wake: Arc<tokio::sync::Notify>,
    shutdown: Arc<std::sync::atomic::AtomicBool>,
) {
    let mut prev_routes = list_route_files(&root);
    loop {
        let run = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take();
        let Some(run) = run else {
            // A pending run left by the exiting watcher still completes; the
            // worker ends only once the queue is drained.
            if shutdown.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
            wake.notified().await;
            continue;
        };
        let paths: Vec<PathBuf> = run.paths.into_iter().collect();
        // Snapshot the regen outputs before the run: the ones the run rewrites
        // get their own worker invalidate below (the watcher never forwards
        // them, see regen_output_files).
        let regen_files = regen_output_files(&root, &cache);
        let regen_before: Vec<Option<u64>> =
            regen_files.iter().map(|p| file_content_hash(p)).collect();
        let client = {
            let (r, c, m) = (root.clone(), cache.clone(), state.mode.clone());
            let routes_prev = prev_routes.clone();
            let changed: Vec<PathBuf> = paths.clone();
            tokio::task::spawn_blocking(move || {
                let routes_now = list_route_files(&r);
                let routes_changed = routes_now != routes_prev;
                if routes_changed {
                    let _ = generate_route_tree(&r, &c, &m);
                }
                let server_fn_changed = changed.iter().any(|p| {
                    let is_ts = p.extension().is_some_and(|e| e == "ts" || e == "tsx");
                    is_ts
                        && (!p.exists()
                            || std::fs::read_to_string(p)
                                .is_ok_and(|s| s.contains("createServerFn")))
                });
                if routes_changed || server_fn_changed {
                    let _ = generate_server_fn_resolver(&r, &c, &m);
                }
                let pinned = if bundle_client_entry(&r, &c, &m).is_err() {
                    None
                } else {
                    match start_bundle_store(&r, &m).persist(&c) {
                        Some((_, pinned)) => Some(pinned),
                        None => oj_cache::start_bundle::PinnedBundle::from_build_dir(&c),
                    }
                };
                (routes_now, pinned)
            })
        };
        let side = async {
            // A reload onto stale worker modules would re-render old content:
            // this run's settled invalidates complete before the signal.
            for handle in run.invalidates {
                let _ = handle.await;
            }
        };
        let (client, _) = tokio::join!(client, side);
        if let Ok((routes_now, pinned)) = client {
            prev_routes = routes_now;
            if let Some(pinned) = pinned {
                *state.bundle.write().unwrap() = Arc::new(pinned);
            }
        }
        // Regenerated outputs are edits the watcher never forwards: push the
        // ones this run rewrote to the worker environments explicitly (the
        // routeTree.gen filter only guards the rebuild loop, not worker
        // invalidation), and re-arm the lazy runner — a fallback request that
        // consumed the dirty flag mid-regen reloaded onto the old generated
        // files. Both before this run's browser reload.
        let regen_changed = changed_regen_outputs(&regen_files, &regen_before);
        if !regen_changed.is_empty() {
            if state.lazy_runner() {
                state
                    .runner_dirty
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            if let Some(port) = state.plugin_serve.mw_port() {
                let names: Vec<&str> = regen_changed
                    .iter()
                    .filter_map(|(p, _)| Path::new(p).file_name()?.to_str())
                    .collect();
                println!(
                    "  oj start: regen outputs changed ({}), invalidating worker",
                    names.join(", ")
                );
                oj_server::notify_plugin_mw_invalidate(port, &regen_changed).await;
            }
        }
        // The non-lazy runner reloads strictly after the regen completed (its
        // respawn must import the new generated files, as the inline loop
        // guaranteed) and before the browser reload.
        if !state.lazy_runner() {
            reload_runner(&state).await;
        }
        let held = state.gate.as_ref().is_some_and(|g| g.hold_reload(&paths));
        if !held {
            let _ = state.reload_tx.send(());
        }
        let modules = client_module_count(&cache);
        let _ = state.ws_tx.send(oj_server::update_progress_frame(
            run.batch,
            "watch",
            0,
            modules,
            Some(0),
            true,
        ));
        if held {
            println!("  oj start: rebuilt, reload held for the editor's flush");
        } else {
            println!("  oj start: rebuilt, reloading");
        }
    }
}

/// Environment handed to the Start node scripts: the mode's `.env` vars with an
/// `envPrefix` (shell wins, as in Vite's loadEnv), the prefixes themselves
/// (`OJ_ENV_PREFIX`), the per-environment defines (`OJ_DEFINE_CLIENT` /
/// `OJ_DEFINE_SSR`, Vite's `environments.<name>.define`) and the JSX settings
/// from the config (`OJ_JSX`, consumed by `jsxTransformOptions` in
/// resolve-pkg.mjs).
fn start_script_env(root: &Path, command: &str, mode: &str) -> anyhow::Result<Vec<(String, String)>> {
    let mut config = oj_config::load(root).unwrap_or_default();
    oj_server::plugins::adopt_vite_config_values(&mut config, root, command, mode)
        .map_err(|e| anyhow::anyhow!(e))?;
    // `.env` files come from `envDir` and only `envPrefix` variables are exposed
    // (Vite's loadEnv), not the root and `VITE_` unconditionally.
    let env_dir = match config.env_dir.as_deref() {
        Some(d) => root.join(d),
        None => root.to_path_buf(),
    };
    let prefixes = oj_config::env_prefixes(&config);
    let mut vars: Vec<(String, String)> = oj_env::load(&env_dir, mode)
        .into_iter()
        .filter(|(k, _)| {
            prefixes.iter().any(|p| k.starts_with(p.as_str())) && std::env::var_os(k).is_none()
        })
        .collect();
    if prefixes != ["VITE_"] {
        vars.push((
            "OJ_ENV_PREFIX".into(),
            serde_json::to_string(&prefixes).unwrap_or_default(),
        ));
    }
    for (env_name, var) in [("client", "OJ_DEFINE_CLIENT"), ("ssr", "OJ_DEFINE_SSR")] {
        let defines: serde_json::Map<String, serde_json::Value> =
            oj_config::environment_defines(&config, env_name)
                .into_iter()
                .map(|(k, v)| (k, serde_json::Value::String(v)))
                .collect();
        if !defines.is_empty() {
            vars.push((var.into(), serde_json::Value::Object(defines).to_string()));
        }
    }
    let jsx = oj_config::jsx_settings(&config);
    if jsx != oj_config::JsxSettings::default() {
        vars.push(("OJ_JSX".into(), serde_json::to_string(&jsx).unwrap_or_default()));
    }
    // The config's `define` map (values are JS expressions), applied by the SSR
    // loader and the production bundles like Vite's define plugin.
    let defines: serde_json::Map<String, serde_json::Value> = oj_config::config_defines(&config)
        .into_iter()
        .map(|(k, v)| (k, serde_json::Value::String(v)))
        .collect();
    if !defines.is_empty() {
        vars.push(("OJ_DEFINE".into(), serde_json::Value::Object(defines).to_string()));
    }
    // The conditions the SSR loader adds to Node's export conditions, and the
    // externalConditions swap for externalized deps. Vite-shaped selection:
    // conditions never cross runtimes. When the ssr environment is
    // runner-backed (its code executes in the Cloudflare plugin's workerd
    // DevEnvironments), its condition list describes THAT runtime and never
    // steers the loader — the loader is oj's own Node fallback and takes
    // Vite's Node server semantics instead (DEFAULT_SERVER_CONDITIONS /
    // DEFAULT_EXTERNAL_CONDITIONS plus the user's RAW top-level lists).
    // Otherwise the user's list (environment → `ssr.resolve` sugar → top
    // level) passes verbatim — an explicit `browser` for happy-dom-style Node
    // SSR stays honored, as Vite honors user conditions — with the
    // `development|production` placeholder mapped for the command.
    let dev = command != "build";
    let map_dev = |list: Vec<String>| -> Vec<String> {
        let dev_prod = if dev { "development" } else { "production" };
        list.into_iter()
            .map(|c| if c == "development|production" { dev_prod.to_string() } else { c })
            .collect()
    };
    let runner_backed = oj_config::ssr_runner_backed(&config);
    let resolve_conditions = if runner_backed {
        Some(oj_config::node_server_conditions(&config, dev))
    } else {
        oj_config::user_resolve_conditions(&config, "ssr").map(&map_dev)
    };
    if let Some(conditions) = resolve_conditions.filter(|c| !c.is_empty()) {
        vars.push((
            "OJ_RESOLVE_CONDITIONS".into(),
            serde_json::to_string(&conditions).unwrap_or_default(),
        ));
    }
    let external_conditions = if runner_backed {
        Some(oj_config::node_server_external_conditions(&config, dev))
    } else {
        oj_config::user_external_conditions(&config, "ssr").map(&map_dev)
    };
    if let Some(conditions) = external_conditions.filter(|c| !c.is_empty()) {
        vars.push((
            "OJ_EXTERNAL_CONDITIONS".into(),
            serde_json::to_string(&conditions).unwrap_or_default(),
        ));
    }
    // `ssr.noExternal`/`external`, consumed by the SSR loader to transform (rather
    // than hand to Node) the dependencies Vite would bundle.
    let externals = oj_config::ssr_externals(&config);
    if externals != oj_config::SsrExternals::default() {
        vars.push((
            "OJ_SSR_EXTERNALS".into(),
            serde_json::to_string(&externals).unwrap_or_default(),
        ));
    }
    // The client bundle's export conditions: Vite derives its client environment's
    // conditions from `resolve.conditions` (DEFAULT_CLIENT_CONDITIONS plus the
    // user list, dev/prod by command), so the Start client bundle must too
    // instead of a hardcoded browser/module/import/development.
    vars.push((
        "OJ_CLIENT_CONDITIONS".into(),
        serde_json::to_string(&oj_config::resolve_conditions_for(
            &config,
            "client",
            command != "build",
        ))
        .unwrap_or_default(),
    ));
    if let Some(entry) = configured_start_server_entry(&config, root) {
        vars.push(("OJ_START_SERVER_ENTRY".into(), entry.to_string_lossy().into_owned()));
    }
    Ok(vars)
}

/// The app's own TanStack Start server entry (`tanstackStart({ server: { entry } })`),
/// when it configures one. Start's plugin publishes the resolved entry paths as
/// `resolve.alias` entries (`virtual:tanstack-start-server-entry` -> file), which
/// Vite's dev server imports as the SSR handler; oj reads the same alias. Only an
/// app file counts: unconfigured, the alias points at Start's own package entry,
/// which oj replaces with its runner entry as before.
fn configured_start_server_entry(config: &oj_config::OjConfig, root: &Path) -> Option<PathBuf> {
    let target = oj_config::resolve_alias(config, "ssr")
        .into_iter()
        .find(|(find, _)| find == "virtual:tanstack-start-server-entry")
        .map(|(_, replacement)| replacement)?;
    let path = PathBuf::from(target.split('?').next().unwrap_or(&target));
    let path = if path.is_absolute() { path } else { root.join(path) };
    let real = path.canonicalize().ok()?;
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if !real.starts_with(&root) || real.components().any(|c| c.as_os_str() == "node_modules") || !real.is_file() {
        return None;
    }
    Some(real)
}

pub async fn start_build(root: PathBuf, mode: &str, out: Option<PathBuf>) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("app root not found: {}: {e}", root.display()))?;
    let cache = oj_cache::cache_root(&root).join("start");
    oj_server::prepare_cache_root(&root);
    oj_server::write_start_assets(&cache)?;
    generate_route_tree(&root, &cache, "development")?;
    generate_server_fn_resolver(&root, &cache, "development")?;
    // The build options Vite resolves for a Start app too: `--out`/`build.outDir`,
    // `base`, `build.sourcemap`, `build.minify` (consumed by build.mjs).
    let mut config = oj_config::load_with(&root, "build", mode).unwrap_or_default();
    oj_server::plugins::adopt_vite_config_values(&mut config, &root, "build", mode)
        .map_err(|e| anyhow::anyhow!(e))?;
    let build_cfg = config.build.clone().unwrap_or_default();
    let prerender = build_cfg.prerender.clone().unwrap_or_default().join(",");
    let out = out
        .or_else(|| build_cfg.out_dir.as_ref().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("dist"));
    let out_dir = if out.is_absolute() { out } else { root.join(out) };
    if out_dir.canonicalize().ok().as_deref() == Some(root.as_path()) {
        anyhow::bail!(
            "build.outDir {} is the project root; refusing to empty it. Point outDir at a subdirectory.",
            out_dir.display()
        );
    }
    // Vite's `base`: "" and "./" are relative, anything else an absolute prefix.
    let base = match config.base.as_deref().unwrap_or("/") {
        "" | "./" => "./".to_string(),
        "/" => "/".to_string(),
        b => format!("/{}/", b.trim_matches('/')),
    };
    let sourcemap = match oj_config::build_sourcemap(&config) {
        oj_config::Sourcemap::Off => "false",
        oj_config::Sourcemap::File => "true",
        oj_config::Sourcemap::Inline => "inline",
        oj_config::Sourcemap::Hidden => "hidden",
    };
    let minify = oj_config::build_minify(&config);
    // Vite's rule: the shell's NODE_ENV wins, else `.env[.mode]` NODE_ENV=development
    // makes a development build, else production. build.mjs derives DEV/PROD and
    // process.env.NODE_ENV from what it receives here.
    let env_dir = match config.env_dir.as_deref() {
        Some(d) => root.join(d),
        None => root.to_path_buf(),
    };
    let node_env = oj_env::resolve_node_env(
        std::env::var("NODE_ENV").ok().filter(|v| !v.is_empty()).as_deref(),
        &oj_env::load(&env_dir, mode),
        "production",
    );
    let status = std::process::Command::new("node")
        .arg(cache.join("build.mjs"))
        .env("OJ_APP_ROOT", &root)
        .env("OJ_CACHE_ROOT", oj_cache::cache_root(&root))
        .env("NODE_ENV", &node_env)
        .env("OJ_MODE", mode)
        .envs(start_script_env(&root, "build", mode)?)
        .env("OJ_PRERENDER", &prerender)
        .env("OJ_OUT_DIR", &out_dir)
        .env("OJ_BASE", &base)
        .env("OJ_SOURCEMAP", sourcemap)
        .env("OJ_MINIFY", if minify { "true" } else { "false" })
        .env("NODE_COMPILE_CACHE", oj_server::node_compile_cache(&root))
        .current_dir(&root)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run production build (node): {e}"))?;
    if !status.success() {
        anyhow::bail!("production build failed");
    }
    println!(
        "  {} build (tanstack start) -> {}",
        oj_server::oj_brand(),
        out_dir.display()
    );
    // A Cloudflare build (the app uses @cloudflare/vite-plugin) has no Node server.
    if out_dir.join("server.mjs").exists() {
        println!("  run: node {}", out_dir.join("server.mjs").display());
    }
    Ok(())
}

fn codegen_store(
    root: &Path,
    cache: &Path,
    kind: &str,
    script: &str,
    marker: Option<&str>,
) -> oj_cache::start_codegen::StartCodegenStore {
    let mut extra = std::fs::read(cache.join(script)).unwrap_or_default();
    for name in [
        "tsr.config.json",
        "package.json",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lockb",
    ] {
        if let Ok(bytes) = std::fs::read(root.join(name)) {
            extra.extend_from_slice(name.as_bytes());
            extra.push(0);
            extra.extend_from_slice(&bytes);
            extra.push(0);
        }
    }
    oj_cache::start_codegen::StartCodegenStore::new(
        root,
        kind,
        env!("CARGO_PKG_VERSION"),
        &extra,
        marker,
    )
}

fn generate_route_tree(root: &Path, cache: &Path, mode: &str) -> anyhow::Result<()> {
    let store = codegen_store(root, cache, "route-tree", "generate.mjs", None);
    let dest = root.join("src").join("routeTree.gen.ts");
    let outputs = [("routeTree.gen.ts", dest.as_path())];
    let inputs: Vec<PathBuf> = list_route_files(root).into_iter().collect();
    match store.restore(&inputs, &outputs) {
        Ok(stats) => {
            println!(
                "  oj start: route tree restored from cache (key {}…, {} route file(s) verified in {}ms)",
                stats.key.get(..8).unwrap_or(&stats.key),
                stats.keyed_files,
                stats.elapsed_ms
            );
            return Ok(());
        }
        Err(miss) => println!("  oj start: route tree cache miss ({miss})"),
    }
    run_node(root, &cache.join("generate.mjs"), "route tree generation", mode)?;
    let inputs: Vec<PathBuf> = list_route_files(root).into_iter().collect();
    store.persist(&inputs, &outputs);
    Ok(())
}

fn generate_server_fn_resolver(root: &Path, cache: &Path, mode: &str) -> anyhow::Result<()> {
    let store = codegen_store(
        root,
        cache,
        "server-fn",
        "gen-resolver.mjs",
        Some("createServerFn"),
    );
    let dest = cache.join("server-fn-resolver.mjs");
    let outputs = [("server-fn-resolver.mjs", dest.as_path())];
    let inputs = list_src_ts_files(root);
    match store.restore(&inputs, &outputs) {
        Ok(stats) => {
            println!(
                "  oj start: server-fn resolver restored from cache (key {}…, {} server-fn file(s), {} of {} rehashed, {}ms)",
                stats.key.get(..8).unwrap_or(&stats.key),
                stats.keyed_files,
                stats.rehashed,
                inputs.len(),
                stats.elapsed_ms
            );
            return Ok(());
        }
        Err(miss) => println!("  oj start: server-fn resolver cache miss ({miss})"),
    }
    run_node(root, &cache.join("gen-resolver.mjs"), "server-fn resolver", mode)?;
    store.persist(&inputs, &outputs);
    Ok(())
}

fn list_src_ts_files(root: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "ts" || x == "tsx") {
                out.push(p);
            }
        }
    }
    walk(&root.join("src"), &mut out);
    out
}

fn bundle_client_entry(root: &Path, cache: &Path, mode: &str) -> anyhow::Result<()> {
    run_node(
        root,
        &cache.join("bundle-client.mjs"),
        "client entry bundling",
        mode,
    )
}

fn start_bundle_store(root: &Path, mode: &str) -> oj_cache::start_bundle::StartBundleStore {
    oj_cache::start_bundle::StartBundleStore::for_mode(
        root,
        env!("CARGO_PKG_VERSION"),
        oj_cache::integrity::VerifyMode::from_env(),
        mode,
    )
}

fn bundle_client_entry_cached(
    root: &Path,
    cache: &Path,
    mode: &str,
) -> anyhow::Result<oj_cache::start_bundle::PinnedBundle> {
    let store = start_bundle_store(root, mode);
    match store.restore(cache) {
        Ok((stats, pinned)) => {
            let rehashed = match stats.rehashed {
                0 => String::new(),
                n => format!(", {n} re-hashed"),
            };
            println!(
                "  oj start: client bundle restored from cache (key {}…, {} chunks, {} files verified in {}ms{rehashed})",
                stats.key.get(..8).unwrap_or(&stats.key),
                stats.chunks,
                stats.files,
                stats.elapsed_ms
            );
            return Ok(pinned);
        }
        Err(miss) => println!("  oj start: client bundle cache miss ({miss})"),
    }
    bundle_client_entry(root, cache, mode)?;
    if let Some((key, pinned)) = store.persist(cache) {
        println!(
            "  oj start: client bundle cached (key {}…, {} chunks)",
            key.get(..8).unwrap_or(&key),
            pinned.len()
        );
        return Ok(pinned);
    }
    oj_cache::start_bundle::PinnedBundle::from_build_dir(cache)
        .ok_or_else(|| anyhow::anyhow!("client chunk index missing after successful bundle"))
}

// Client module count written by bundle-client.mjs, for update/boot narration.
fn client_module_count(cache: &Path) -> usize {
    std::fs::read_to_string(cache.join("client-entry.modules"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Vite's rule for the dev server too: the shell's NODE_ENV wins (so
/// `NODE_ENV=production oj dev` is PROD, as in Vite and oj's generic server),
/// else `.env[.mode]` NODE_ENV=development, else development.
fn dev_node_env(root: &Path, mode: &str) -> String {
    oj_env::resolve_node_env(
        std::env::var("NODE_ENV").ok().as_deref(),
        &oj_env::load(root, mode),
        "development",
    )
}

fn run_node(root: &Path, script: &Path, what: &str, mode: &str) -> anyhow::Result<()> {
    let status = std::process::Command::new("node")
        .arg(script)
        .env("OJ_APP_ROOT", root)
        .env("OJ_CACHE_ROOT", oj_cache::cache_root(root))
        .env("NODE_ENV", dev_node_env(root, mode))
        .env("OJ_MODE", mode)
        .envs(start_script_env(root, "serve", mode)?)
        .env("NODE_COMPILE_CACHE", oj_server::node_compile_cache(root))
        .current_dir(root)
        .status()
        .map_err(|e| anyhow::anyhow!("could not run {what} (node): {e}"))?;
    if !status.success() {
        anyhow::bail!("{what} failed");
    }
    Ok(())
}

async fn spawn_start_runner(root: &Path, cache: &Path, mode: &str) -> anyhow::Result<Runner> {
    let mut runner = spawn_node_service(root, &cache.join("runner.mjs"), mode).await?;
    // The runner announces its loopback port as its first stdout line, before it
    // evaluates the app entry (requests wait for that inside the runner).
    let line = tokio::time::timeout(std::time::Duration::from_secs(120), runner.lines.next_line())
        .await
        .map_err(|_| anyhow::anyhow!("start runner did not announce its port"))?
        .map_err(|e| anyhow::anyhow!("start runner stdout: {e}"))?
        .ok_or_else(|| anyhow::anyhow!("start runner exited before announcing its port"))?;
    let port = serde_json::from_str::<serde_json::Value>(&line)
        .ok()
        .and_then(|v| v.get("port").and_then(|p| p.as_u64()))
        .ok_or_else(|| anyhow::anyhow!("start runner sent an unexpected first line: {line}"))?;
    runner.http_port = Some(port as u16);
    Ok(runner)
}

async fn spawn_node_service(root: &Path, script: &Path, mode: &str) -> anyhow::Result<Runner> {
    let mut cmd = tokio::process::Command::new("node");
    // The SSR loader inlines source maps into every transformed module; this
    // flag makes Node apply them to stack traces (original .tsx positions).
    cmd.arg("--enable-source-maps").arg(script)
        .env("OJ_APP_ROOT", root)
        .env("OJ_CACHE_ROOT", oj_cache::cache_root(root))
        .env("NODE_ENV", dev_node_env(root, mode))
        .env("OJ_MODE", mode)
        .envs(start_script_env(root, "serve", mode)?)
        .env(
            "OJ_SSR_BRIDGE_DIR",
            oj_server::plugins::ssr_bridge_dir(root),
        )
        .current_dir(root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true);
    if let Some(v8) = oj_server::node_compile_cache_opt_in(root) {
        cmd.env("NODE_COMPILE_CACHE", v8);
    }
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not spawn node service {}: {e}", script.display()))?;
    let stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    Ok(Runner {
        stdin,
        lines: BufReader::new(stdout).lines(),
        _child: child,
        http_port: None,
    })
}

fn app_uses_tailwind(root: &Path) -> bool {
    std::fs::read_to_string(root.join("package.json"))
        .map(|s| {
            s.contains("@tailwindcss/postcss")
                || s.contains("@tailwindcss/vite")
                || s.contains("\"tailwindcss\"")
        })
        .unwrap_or(false)
}

fn needs_css_compile(src: &str) -> bool {
    src.contains("tailwindcss")
        || src.contains("@tailwind")
        || src.contains("@plugin")
        || src.contains("@apply")
}

async fn compile_css(host: &Arc<tokio::sync::Mutex<Runner>>, path: &Path) -> Option<String> {
    let mut guard = host.lock().await;
    let req = serde_json::json!({ "path": path.to_string_lossy() });
    guard
        .stdin
        .write_all(format!("{req}\n").as_bytes())
        .await
        .ok()?;
    guard.stdin.flush().await.ok()?;
    let line = tokio::time::timeout(std::time::Duration::from_secs(30), guard.lines.next_line())
        .await
        .ok()?
        .ok()??;
    let v: serde_json::Value = serde_json::from_str(&line).ok()?;
    v.get("css").and_then(|c| c.as_str()).map(|s| s.to_owned())
}

#[derive(Debug, PartialEq)]
enum Route {
    Document,
    /// A GET whose last segment has an extension (`/robots.txt`,
    /// `/users/john.doe`): a static file or module if one exists, otherwise the
    /// SSR handler (a Start server route or a dotted route param), like Vite
    /// where every request nothing else owns reaches the app.
    StaticOrDocument,
    Api,
    Pass,
}

fn classify(req: &Request, proxy_prefixes: &[String]) -> Route {
    let path = req.uri().path();
    if path.starts_with("/@") || path.starts_with("/__") {
        return Route::Pass;
    }
    if proxy_prefixes.iter().any(|p| path.starts_with(p.as_str())) {
        return Route::Pass;
    }
    // A dotted non-GET (`POST /api/export.csv`, `PUT /files/a.b`) can only be
    // the app's: static files and modules are GET-only, so it goes straight
    // to the SSR handler like every other request nothing else owns.
    let last = path.rsplit('/').next().unwrap_or("");
    if last != "index.html" && last.contains('.') {
        return if *req.method() == Method::GET {
            Route::StaticOrDocument
        } else {
            Route::Api
        };
    }
    if proxy_prefixes.iter().any(|p| path.starts_with(p.as_str())) {
        return Route::Pass;
    }
    match *req.method() {
        Method::GET => Route::Document,
        _ => Route::Api,
    }
}

fn document_url(url: &str) -> String {
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p, format!("?{q}")),
        None => (url, String::new()),
    };
    let path = match path.strip_suffix("/index.html") {
        Some(prefix) => format!("{prefix}/"),
        None => path.to_string(),
    };
    format!("{path}{query}")
}

async fn start_route(State(state): State<Arc<StartState>>, req: Request, next: Next) -> Response {
    if req.uri().path() == "/@oj-start/hmr" {
        let (mut parts, _) = req.into_parts();
        return match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(ws) => {
                let rx = state.reload_tx.subscribe();
                ws.on_upgrade(move |socket| start_hmr_socket(socket, rx))
            }
            Err(e) => e.into_response(),
        };
    }
    if req.uri().path() == "/@oj-start/live-reload.js" {
        return serve_js(&state.live_reload, "live-reload client").await;
    }
    if let Some(rest) = req.uri().path().strip_prefix("/@oj-start/fs/") {
        return serve_fs_asset(&state, &format!("/{rest}")).await;
    }
    if let Some(name) = req.uri().path().strip_prefix("/@oj-start/") {
        return serve_client_chunk(&state, name).await;
    }
    if req.uri().path().starts_with("/_serverFn/") {
        return forward_with_body(&state, req).await;
    }
    match classify(&req, &state.proxy_prefixes) {
        Route::Document => {
            let raw = path_and_query(&req);
            forward_document(&state, raw, req.headers()).await
        }
        Route::StaticOrDocument => {
            let raw = path_and_query(&req);
            let headers = req.headers().clone();
            let resp = next.run(req).await;
            if resp.status() != StatusCode::NOT_FOUND {
                return resp;
            }
            forward_document(&state, raw, &headers).await
        }
        Route::Api => forward_with_body(&state, req).await,
        Route::Pass => next.run(req).await,
    }
}

async fn start_hmr_socket(mut socket: WebSocket, mut rx: broadcast::Receiver<()>) {
    loop {
        tokio::select! {
            reload = rx.recv() => {
                if reload.is_err() || socket.send(Message::Text("reload".into())).await.is_err() {
                    break;
                }
            }
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) => break,
                Some(Ok(Message::Close(_))) => {
                    let _ = socket.close().await;
                    break;
                }
                Some(Ok(_)) => {}
            },
        }
    }
}

fn path_and_query(req: &Request) -> String {
    req.uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string()
}

async fn forward_document(
    state: &Arc<StartState>,
    raw: String,
    headers: &header::HeaderMap,
) -> Response {
    // Editor plugins (dev-server bridge) register configureServer routes
    // with no path prefix, so a GET like /_sandbox/preview/viewers can only
    // be told from an app route by asking the middleware first; it returns
    // x-oj-fallthrough when it does not own the path, and then we SSR.
    if let Some(port) = state.plugin_serve.mw_port() {
        if let Some(resp) = oj_server::forward_get_to_plugin_mw(port, &raw, headers).await {
            // A worker-served document needs the live-reload client like the
            // runner-served ones, or edits never reload the page.
            return inject_reload_client(resp);
        }
    }
    ensure_runner_fresh(state).await;
    forward(&state.runner, "GET".into(), document_url(&raw), headers, None).await
}

async fn forward_with_body(state: &Arc<StartState>, req: Request) -> Response {
    // The middleware may pipe an unclaimed request on to the runner
    // (x-oj-forward-to), so start refreshing it — without blocking this
    // request, which the worker middleware typically serves itself.
    if state.lazy_runner()
        && state
            .runner_dirty
            .load(std::sync::atomic::Ordering::SeqCst)
    {
        let st = Arc::clone(state);
        tokio::spawn(async move { ensure_runner_fresh(&st).await });
    }
    let method = req.method().to_string();
    let url = req
        .uri()
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or("/")
        .to_string();
    let mut headers = req.headers().clone();
    // The body streams through as Vite pipes `req` into the app: no size cap,
    // never held in memory. A configureServer middleware may claim the request
    // first; since a streamed body cannot be replayed, the plugin host pipes
    // an unclaimed request on to the runner itself (x-oj-forward-to) instead
    // of falling through.
    let Some(port) = state.plugin_serve.mw_port() else {
        return forward(&state.runner, method, url, &headers, Some(req.into_body())).await;
    };
    if let Some(runner_port) = state.runner.lock().await.http_port {
        if let Ok(v) = header::HeaderValue::from_str(&runner_port.to_string()) {
            headers.insert("x-oj-forward-to", v);
        }
    }
    match oj_server::proxy_to_loopback_streaming(port, &method, &url, &headers, Some(req.into_body()))
        .await
    {
        Ok(resp) if resp.headers().contains_key("x-oj-fallthrough") => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "oj start: request fell through the plugin middleware without reaching the app",
        )
            .into_response(),
        Ok(resp) => inject_reload_client(resp),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj start: {e}")).into_response(),
    }
}

async fn serve_client_chunk(state: &StartState, name: &str) -> Response {
    let name = percent_decode(name);
    let bundle = state
        .bundle
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let Some(chunk) = bundle.chunk(&name) else {
        return (
            StatusCode::NOT_FOUND,
            format!("oj start: {name} is not in the client bundle manifest"),
        )
            .into_response();
    };
    let mode = match &chunk.hash {
        Some(_) => state.verify,
        None => oj_cache::integrity::VerifyMode::Standard,
    };
    let expected = oj_cache::integrity::ExpectedFile {
        size: chunk.size,
        hash: chunk.hash.clone().unwrap_or_default(),
    };
    let path = chunk.path.clone();
    let read = tokio::task::spawn_blocking(move || {
        oj_cache::integrity::verified_read(&path, &expected, mode)
    })
    .await;
    match read {
        Ok(Ok(bytes)) => {
            let ext = name.rsplit('.').next().unwrap_or("");
            (
                [
                    (header::CONTENT_TYPE, asset_mime(ext)),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                bytes,
            )
                .into_response()
        }
        Ok(Err(e)) => {
            eprintln!("oj start: chunk {name} failed verification ({e}); refusing to serve");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("oj start: chunk {name}: {e}"),
            )
                .into_response()
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "oj start: chunk read task failed".to_string(),
        )
            .into_response(),
    }
}

async fn serve_js(path: &Path, what: &str) -> Response {
    match tokio::fs::read(path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            bytes,
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj start: {what}: {e}"),
        )
            .into_response(),
    }
}

async fn serve_fs_asset(state: &StartState, abs: &str) -> Response {
    let decoded = percent_decode(abs);
    let path = PathBuf::from(&decoded);
    let canon = match tokio::fs::canonicalize(&path).await {
        Ok(c) => c,
        Err(e) => return (StatusCode::NOT_FOUND, format!("oj start: asset: {e}")).into_response(),
    };
    if !canon.starts_with(&state.workspace_root) {
        return (StatusCode::FORBIDDEN, "oj start: asset outside workspace").into_response();
    }
    if canon.extension().and_then(|e| e.to_str()) == Some("css") {
        if let Some(host) = &state.css_host {
            if let Ok(src) = tokio::fs::read_to_string(&canon).await {
                if needs_css_compile(&src) {
                    if let Some(css) = compile_css(host, &canon).await {
                        return (
                            [
                                (header::CONTENT_TYPE, "text/css; charset=utf-8"),
                                (header::CACHE_CONTROL, "no-cache"),
                            ],
                            css,
                        )
                            .into_response();
                    }
                }
            }
        }
    }
    match tokio::fs::read(&canon).await {
        Ok(bytes) => {
            let ext = canon.extension().and_then(|e| e.to_str()).unwrap_or("");
            (
                [
                    (header::CONTENT_TYPE, asset_mime(ext)),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                bytes,
            )
                .into_response()
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj start: asset: {e}"),
        )
            .into_response(),
    }
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(h << 4 | l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}


async fn forward(
    runner: &Arc<tokio::sync::Mutex<Runner>>,
    method: String,
    url: String,
    req_headers: &header::HeaderMap,
    body: Option<axum::body::Body>,
) -> Response {
    let port = match runner.lock().await.http_port {
        Some(p) => p,
        None => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "oj start: runner has no loopback port",
            )
                .into_response()
        }
    };
    match oj_server::proxy_to_loopback_streaming(port, &method, &url, req_headers, body).await {
        Ok(resp) => inject_reload_client(resp),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj start: {e}")).into_response(),
    }
}

/// Append the live-reload client to a streamed HTML document. The stream is
/// passed through untouched (TanStack streams its dehydrated data into the
/// page) and the script tag follows the final chunk; browsers parse a trailing
/// `<script>` after `</html>` into the body.
fn inject_reload_client(resp: Response) -> Response {
    use tokio_stream::StreamExt;
    let is_html = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|c| c.to_str().ok())
        .is_some_and(|c| c.contains("text/html"));
    // A compressed body (a worker may set content-encoding) cannot take a
    // plain-text tail without corrupting the stream.
    let encoded = resp
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|c| c.to_str().ok())
        .is_some_and(|c| !c.eq_ignore_ascii_case("identity"));
    if !is_html || encoded {
        return resp;
    }
    let (parts, body) = resp.into_parts();
    let tail = tokio_stream::once(Ok::<_, axum::Error>(axum::body::Bytes::from_static(
        RELOAD_CLIENT.as_bytes(),
    )));
    let stream = body.into_data_stream().chain(tail);
    Response::from_parts(parts, axum::body::Body::from_stream(stream))
}

/// The signals the Cloudflare prewarm hold selects on, captured from the boot
/// plugin host once the server build joins.
type ServeInfoUpdates = tokio::sync::watch::Receiver<Option<oj_server::plugins::ServeInfo>>;
struct PrewarmHold {
    updates: ServeInfoUpdates,
    host_gone: tokio::sync::watch::Receiver<bool>,
    init_failed: tokio::sync::watch::Receiver<bool>,
    init_deadline: tokio::time::Instant,
    /// How long an evidence release stays PENDING before the prewarm commits
    /// (the RPC-timeout scale, matching the stall monitor's window): the
    /// stall monitor's evidence is clearable — one silent >window step of a
    /// healthy boot flips it and the next milestone clears it — so a clear
    /// landing within this window re-arms the hold instead of the release
    /// being one-shot. Host death and the init deadline still release
    /// immediately.
    confirm: std::time::Duration,
}

/// Holds the Cloudflare prewarm until the plugin host's serve info is KNOWN,
/// releasing early only on wedge EVIDENCE: (a) the info arriving decides
/// (worker environments render → skip the prewarm; none → prewarm), (b) the
/// host dying (`host_gone`) releases immediately, while the init-failure
/// evidence watch — the host's stall monitor flipping it when a full
/// RPC-scale window passed with no init milestone, or a pre-init RPC having
/// burned its full init window — makes the release PENDING for one more
/// RPC-scale window (`PrewarmHold::confirm`): the evidence is clearable, so a
/// milestone landing within that window re-arms the hold instead of a single
/// silent step of a healthy boot one-shot-releasing a wasted prewarm, and
/// (c) the
/// host's OWN init deadline (spawn + `OJ_PLUGIN_INIT_TIMEOUT` — never a fresh
/// full period measured from here) is the outer bound. A merely slow, healthy
/// boot — elapsed time short of that deadline, with no evidence — keeps
/// holding: the earlier flat RPC-scale timer prewarmed (wasted CPU, competing
/// with the boot) on every healthy boot slower than one RPC window. `None`
/// means "not known: prewarm"; a LATE serve-info activation still supersedes.
async fn hold_prewarm_for_serve_info(
    hold: &mut PrewarmHold,
) -> Option<oj_server::plugins::ServeInfo> {
    loop {
        if let Some(info) = *hold.updates.borrow_and_update() {
            return Some(info);
        }
        if *hold.host_gone.borrow_and_update() {
            return None;
        }
        if *hold.init_failed.borrow_and_update() {
            // Init-failure evidence: a wedge — or one clearable silent step
            // of a healthy boot (the stall monitor flips on any full window
            // with no milestone; the next milestone clears it). The release
            // is therefore not one-shot: it stays PENDING for one more
            // RPC-scale window, and an evidence-clear landing within it
            // re-arms the hold (the prewarm has not started while we are
            // still in here). Serve info, host death and the init deadline
            // decide immediately as before.
            let confirm = std::cmp::min(
                hold.init_deadline,
                tokio::time::Instant::now() + hold.confirm,
            );
            loop {
                tokio::select! {
                    changed = hold.updates.changed() => {
                        if changed.is_err() {
                            return None;
                        }
                        if let Some(info) = *hold.updates.borrow_and_update() {
                            return Some(info);
                        }
                    }
                    changed = hold.host_gone.changed() => {
                        if changed.is_err() || *hold.host_gone.borrow_and_update() {
                            return None;
                        }
                    }
                    changed = hold.init_failed.changed() => {
                        if changed.is_err() {
                            return None;
                        }
                        if !*hold.init_failed.borrow_and_update() {
                            break; // cleared: a false positive — re-arm
                        }
                    }
                    _ = tokio::time::sleep_until(confirm) => {
                        return None; // still standing: release, prewarm
                    }
                }
            }
            continue;
        }
        tokio::select! {
            changed = hold.updates.changed() => {
                if changed.is_err() {
                    return None;
                }
            }
            changed = hold.host_gone.changed() => {
                if changed.is_err() {
                    return None;
                }
            }
            changed = hold.init_failed.changed() => {
                if changed.is_err() {
                    return None;
                }
            }
            _ = tokio::time::sleep_until(hold.init_deadline) => {
                return None;
            }
        }
    }
}

#[cfg(test)]
fn collect_headers(headers: &header::HeaderMap) -> Vec<(String, String)> {
    headers
        .iter()
        .filter_map(|(k, v)| Some((k.as_str().to_owned(), v.to_str().ok()?.to_owned())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message as ClientMessage};

    struct HmrTestServer {
        url: String,
        reload_tx: broadcast::Sender<()>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for HmrTestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    impl HmrTestServer {
        async fn start() -> Self {
            Self::start_with_gate(None).await
        }

        async fn start_with_gate(gate: Option<Arc<tokio::sync::Notify>>) -> Self {
            let (reload_tx, _) = broadcast::channel(16);
            let app = axum::Router::new()
                .route(
                    "/@oj-start/hmr",
                    axum::routing::get(
                        |ws: WebSocketUpgrade,
                         State((tx, gate)): State<(
                            broadcast::Sender<()>,
                            Option<Arc<tokio::sync::Notify>>,
                        )>| async move {
                            let rx = tx.subscribe();
                            ws.on_upgrade(move |socket| async move {
                                if let Some(gate) = gate {
                                    gate.notified().await;
                                }
                                start_hmr_socket(socket, rx).await;
                            })
                        },
                    ),
                )
                .with_state((reload_tx.clone(), gate));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}/@oj-start/hmr", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                url,
                reload_tx,
                task,
            }
        }

        async fn wait_for_no_subscribers(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while self.reload_tx.receiver_count() != 0 {
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("disconnected HMR client retained its reload subscription");
        }
    }

    #[tokio::test]
    async fn start_hmr_releases_closed_clients_without_a_reload() {
        let server = HmrTestServer::start().await;
        for _ in 0..5 {
            let (mut client, _) = connect_async(&server.url).await.unwrap();
            assert_eq!(server.reload_tx.receiver_count(), 1);
            client.close(None).await.unwrap();
            server.wait_for_no_subscribers().await;
            let response = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert!(matches!(response, ClientMessage::Close(_)));
        }
    }

    #[tokio::test]
    async fn start_hmr_completes_close_handshakes_while_reloads_arrive() {
        use tokio_tungstenite::tungstenite::protocol::{frame::coding::CloseCode, CloseFrame};

        let gate = Arc::new(tokio::sync::Notify::new());
        let server = HmrTestServer::start_with_gate(Some(gate.clone())).await;
        let close = CloseFrame {
            code: CloseCode::Normal,
            reason: "leaving the preview".into(),
        };
        for attempt in 0..64 {
            let (mut client, _) = connect_async(&server.url).await.unwrap();
            client.close(Some(close.clone())).await.unwrap();
            assert_eq!(server.reload_tx.send(()).unwrap(), 1);
            // Let the I/O driver observe the close frame before releasing the handler.
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            gate.notify_one();
            let reply = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    match client.next().await {
                        Some(Ok(ClientMessage::Close(frame))) => break frame,
                        Some(Ok(ClientMessage::Text(text))) => assert_eq!(text, "reload"),
                        other => panic!("close handshake failed on attempt {attempt}: {other:?}"),
                    }
                }
            })
            .await
            .expect("close handshake did not finish while a reload was pending");
            assert_eq!(reply, Some(close.clone()));
            server.wait_for_no_subscribers().await;
        }
    }

    #[tokio::test]
    async fn start_hmr_releases_dropped_clients_without_a_reload() {
        let server = HmrTestServer::start().await;
        for _ in 0..5 {
            let (client, _) = connect_async(&server.url).await.unwrap();
            assert_eq!(server.reload_tx.receiver_count(), 1);
            drop(client);
            server.wait_for_no_subscribers().await;
        }
    }

    #[tokio::test]
    async fn start_hmr_keeps_live_clients_subscribed_after_incoming_messages() {
        let server = HmrTestServer::start().await;
        let (mut client, _) = connect_async(&server.url).await.unwrap();
        client
            .send(ClientMessage::Text("ignored".into()))
            .await
            .unwrap();
        client
            .send(ClientMessage::Ping(vec![1, 2, 3].into()))
            .await
            .unwrap();
        let pong = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(pong, ClientMessage::Pong(vec![1, 2, 3].into()));
        for _ in 0..2 {
            assert_eq!(server.reload_tx.send(()).unwrap(), 1);
            let reload = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(reload, ClientMessage::Text("reload".into()));
        }
        client.close(None).await.unwrap();
        server.wait_for_no_subscribers().await;
    }

    fn tmp(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oj-startdev-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn req(method: &str, path: &str) -> Request {
        axum::http::Request::builder()
            .method(method)
            .uri(path)
            .body(axum::body::Body::empty())
            .unwrap()
    }

    fn prewarm_channels() -> (
        tokio::sync::watch::Sender<Option<oj_server::plugins::ServeInfo>>,
        tokio::sync::watch::Sender<bool>,
        tokio::sync::watch::Sender<bool>,
        PrewarmHold,
    ) {
        let (info_tx, updates) = tokio::sync::watch::channel(None);
        let (gone_tx, host_gone) = tokio::sync::watch::channel(false);
        let (fail_tx, init_failed) = tokio::sync::watch::channel(false);
        let hold = PrewarmHold {
            updates,
            host_gone,
            init_failed,
            init_deadline: tokio::time::Instant::now() + std::time::Duration::from_secs(300),
            confirm: std::time::Duration::from_secs(20),
        };
        (info_tx, gone_tx, fail_tx, hold)
    }

    // (a) The hold outlives any flat RPC-scale timer while the boot is merely
    // slow and healthy: the serve info arriving is what decides, even minutes
    // in — never a 20 s timer prewarming against a still-booting host.
    #[tokio::test(start_paused = true)]
    async fn prewarm_hold_waits_out_a_healthy_slow_boot_for_the_serve_info() {
        let (info_tx, _gone_tx, _fail_tx, mut hold) = prewarm_channels();
        let start = tokio::time::Instant::now();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(120)).await;
            let _ = info_tx.send(Some(oj_server::plugins::ServeInfo::default()));
        });
        let known = hold_prewarm_for_serve_info(&mut hold).await;
        assert!(known.is_some(), "the arriving serve info decides");
        let held = start.elapsed();
        assert!(
            held >= std::time::Duration::from_secs(120),
            "held past the old RPC-scale bound on a healthy slow boot: {held:?}"
        );
        assert!(
            held < std::time::Duration::from_secs(300),
            "released by the info, not the deadline: {held:?}"
        );
    }

    // (c) With nothing deciding and no wedge evidence, the hold's outer bound
    // is the host's OWN init deadline — the prewarm then proceeds anyway.
    #[tokio::test(start_paused = true)]
    async fn prewarm_hold_releases_at_the_init_deadline_when_nothing_decides() {
        let (_info_tx, _gone_tx, _fail_tx, mut hold) = prewarm_channels();
        let start = tokio::time::Instant::now();
        let known = hold_prewarm_for_serve_info(&mut hold).await;
        assert!(known.is_none(), "not known: the caller prewarms");
        assert!(
            start.elapsed() >= std::time::Duration::from_secs(300),
            "the outer bound is the init deadline, got {:?}",
            start.elapsed()
        );
    }

    // (b) Wedge evidence releases the hold: the host dying immediately, and
    // standing init-failure evidence after its pending-release confirmation
    // window (the evidence is clearable now, so the release is not one-shot).
    #[tokio::test(start_paused = true)]
    async fn prewarm_hold_releases_on_wedge_evidence() {
        let (_info_tx, _gone_tx, fail_tx, mut hold) = prewarm_channels();
        let start = tokio::time::Instant::now();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(40)).await;
            let _ = fail_tx.send(true);
            // The real sender lives in the PluginHost for the whole session:
            // dropping it here would read as the host vanishing.
            std::mem::forget(fail_tx);
        });
        assert!(hold_prewarm_for_serve_info(&mut hold).await.is_none());
        let held = start.elapsed();
        assert!(
            held >= std::time::Duration::from_secs(60) && held < std::time::Duration::from_secs(300),
            "standing evidence released the hold after its confirmation window: {held:?}"
        );

        let (_info_tx, gone_tx, _fail_tx, mut hold) = prewarm_channels();
        let start = tokio::time::Instant::now();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;
            let _ = gone_tx.send(true);
        });
        assert!(hold_prewarm_for_serve_info(&mut hold).await.is_none());
        assert!(
            start.elapsed() < std::time::Duration::from_secs(300),
            "the host dying released the hold: {:?}",
            start.elapsed()
        );
    }

    // (b') The evidence release is NOT one-shot: the stall monitor's evidence
    // clears on the next milestone, so a healthy boot with ONE silent step
    // longer than the stall window flips it and then clears it — the pending
    // release re-arms and the hold keeps holding for the serve info instead
    // of launching a wasted prewarm. Standing evidence still releases (the
    // test above); a second, later wedge still gets its own pending release.
    #[tokio::test(start_paused = true)]
    async fn prewarm_hold_re_arms_when_evidence_clears_before_the_confirm_window() {
        let (info_tx, _gone_tx, fail_tx, mut hold) = prewarm_channels();
        let start = tokio::time::Instant::now();
        tokio::spawn(async move {
            // One silent >window step: evidence at 30s, milestone clears it
            // at 35s (inside the 20s confirmation window).
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let _ = fail_tx.send(true);
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let _ = fail_tx.send(false);
            // The boot finishes much later: the serve info decides.
            tokio::time::sleep(std::time::Duration::from_secs(85)).await;
            let _ = info_tx.send(Some(oj_server::plugins::ServeInfo::default()));
            // As above: the real senders outlive the hold.
            std::mem::forget(fail_tx);
            std::mem::forget(info_tx);
        });
        let known = hold_prewarm_for_serve_info(&mut hold).await;
        assert!(known.is_some(), "the cleared evidence re-armed the hold; the info decided");
        let held = start.elapsed();
        assert!(
            held >= std::time::Duration::from_secs(120),
            "held through the false positive to the serve info: {held:?}"
        );
        assert!(
            held < std::time::Duration::from_secs(300),
            "released by the info, not the deadline: {held:?}"
        );
    }

    // The rebundle worker snapshots the regen outputs around a run and pushes
    // only the ones the run actually rewrote to the worker environments:
    // rewritten content and a newly created file count, an untouched file (or
    // one missing before and after) does not.
    #[test]
    fn changed_regen_outputs_detects_rewrites_and_creations() {
        let dir = tmp("regen-outputs");
        let tree = dir.join("routeTree.gen.ts");
        let resolver = dir.join("server-fn-resolver.mjs");
        let missing = dir.join("never-written.mjs");
        std::fs::write(&tree, "tree v1").unwrap();
        let files = [tree.clone(), resolver.clone(), missing.clone()];
        let before: Vec<Option<u64>> = files.iter().map(|p| file_content_hash(p)).collect();

        // Nothing rewritten: no changes, even for the still-missing files.
        assert!(changed_regen_outputs(&files, &before).is_empty());

        // A rewrite and a creation both count; the untouched missing file not.
        std::fs::write(&tree, "tree v2").unwrap();
        std::fs::write(&resolver, "resolver v1").unwrap();
        let changed = changed_regen_outputs(&files, &before);
        let paths: Vec<&str> = changed.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(paths, vec![tree.to_str().unwrap(), resolver.to_str().unwrap()]);
        assert!(changed.iter().all(|(_, t)| *t == "update"));

        // Same content written again: hashes match, no change detected.
        let before: Vec<Option<u64>> = files.iter().map(|p| file_content_hash(p)).collect();
        std::fs::write(&tree, "tree v2").unwrap();
        assert!(changed_regen_outputs(&files, &before).is_empty());
    }

    // Vite's loadEnv rule for the Start scripts: `.env.<mode>` vars with any
    // configured envPrefix (not just VITE_) pass, unprefixed ones never do; the
    // prefixes and the per-environment defines travel alongside.
    #[test]
    fn start_script_env_honors_env_prefix_and_environment_defines() {
        let root = tmp("script-env");
        std::fs::write(
            root.join("oj.config.json"),
            r#"{
              "envPrefix": ["VITE_", "APP_"],
              "resolve": { "conditions": ["custom", "development|production"] },
              "define": { "__SHARED__": "1" },
              "environments": {
                "client": { "define": { "__SIDE__": "\"client\"" } },
                "ssr": { "define": { "__SIDE__": "\"server\"", "__ONLY_SSR__": "true" } }
              }
            }"#,
        )
        .unwrap();
        std::fs::write(
            root.join(".env.staging"),
            "VITE_X=1\nAPP_Y=2\nSECRET_Z=3\n",
        )
        .unwrap();
        let vars = start_script_env(&root, "serve", "staging").unwrap();
        let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("VITE_X").as_deref(), Some("1"));
        assert_eq!(get("APP_Y").as_deref(), Some("2"));
        assert_eq!(get("SECRET_Z"), None);
        assert_eq!(get("OJ_ENV_PREFIX").as_deref(), Some(r#"["VITE_","APP_"]"#));
        assert_eq!(get("OJ_DEFINE").as_deref(), Some(r#"{"__SHARED__":"1"}"#));
        assert_eq!(get("OJ_DEFINE_CLIENT").as_deref(), Some(r#"{"__SIDE__":"\"client\""}"#));
        assert_eq!(get("OJ_RESOLVE_CONDITIONS").as_deref(), Some(r#"["custom","development"]"#));

        // Conditions never cross runtimes: on a runner-backed ssr environment
        // (the extractor's `ssr.runnerBacked`, set when a plugin declares a
        // dev-runtime environment, e.g. the Cloudflare plugin's workerd
        // DevEnvironments) the environment's own list —
        // conditions AND externalConditions — describes workerd and never
        // reaches the Node loader; Vite's Node server semantics apply instead
        // (DEFAULT_SERVER_CONDITIONS / DEFAULT_EXTERNAL_CONDITIONS plus the
        // user's RAW top-level lists). Without the flag a user list is honored
        // verbatim, as Vite honors user conditions.
        {
            let runner_backed = tmp("script-env-runner-backed");
            std::fs::write(
                runner_backed.join("oj.config.json"),
                r#"{ "ssr": { "runnerBacked": true, "resolve": { "conditions": ["workerd", "worker", "module", "browser", "development|production"], "externalConditions": ["workerd", "development|production"] } } }"#,
            )
            .unwrap();
            let vars = start_script_env(&runner_backed, "serve", "development").unwrap();
            let var = |k: &str| {
                vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
            };
            assert_eq!(
                var("OJ_RESOLVE_CONDITIONS").as_deref(),
                Some(r#"["module","node","development","import","default"]"#),
                "a runner-backed environment's workerd list must not steer the Node loader; node defaults apply"
            );
            assert_eq!(
                var("OJ_EXTERNAL_CONDITIONS").as_deref(),
                Some(r#"["node","module-sync"]"#),
                "the workerd externalConditions never cross either; Vite's DEFAULT_EXTERNAL_CONDITIONS apply"
            );
            let _ = std::fs::remove_dir_all(&runner_backed);

            // The user's RAW top-level lists (the extractor's `rawResolve`)
            // are user-authored and runtime-neutral: conditions join the node
            // defaults, externalConditions replace them, dev|prod mapped.
            let with_raw = tmp("script-env-runner-raw");
            std::fs::write(
                with_raw.join("oj.config.json"),
                r#"{ "ssr": { "runnerBacked": true, "resolve": { "conditions": ["workerd", "module", "browser"] } },
                     "rawResolve": { "conditions": ["custom", "module"], "externalConditions": ["custom-ext", "development|production"] },
                     "resolve": { "conditions": ["module", "browser", "development|production"] } }"#,
            )
            .unwrap();
            let vars = start_script_env(&with_raw, "serve", "development").unwrap();
            let var = |k: &str| {
                vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone())
            };
            assert_eq!(
                var("OJ_RESOLVE_CONDITIONS").as_deref(),
                Some(r#"["module","node","development","custom","import","default"]"#),
                "raw top-level user conditions join the node defaults; the resolved client list never does"
            );
            assert_eq!(
                var("OJ_EXTERNAL_CONDITIONS").as_deref(),
                Some(r#"["custom-ext","development"]"#),
                "raw top-level externalConditions replace the default, as a user list does in Vite"
            );
            let _ = std::fs::remove_dir_all(&with_raw);

            let plain_browser = tmp("script-env-browser-honored");
            std::fs::write(
                plain_browser.join("oj.config.json"),
                r#"{ "environments": { "ssr": { "resolve": { "conditions": ["browser", "module"] } } } }"#,
            )
            .unwrap();
            let vars = start_script_env(&plain_browser, "serve", "development").unwrap();
            let cond = vars
                .iter()
                .find(|(n, _)| n == "OJ_RESOLVE_CONDITIONS")
                .map(|(_, v)| v.clone());
            assert_eq!(
                cond.as_deref(),
                Some(r#"["browser","module"]"#),
                "a user browser condition on a non-runner-backed environment is honored verbatim"
            );

            // The extraction shape on a Cloudflare Start app: the resolved ssr
            // environment's workerd conditions arrive on the `ssr.resolve`
            // sugar with `runnerBacked` alongside, and the resolved top-level
            // list carries Vite's client defaults. Neither list may steer the
            // Node loader: node defaults win.
            let cf_shape = tmp("script-env-cf-shape");
            std::fs::write(
                cf_shape.join("oj.config.json"),
                r#"{ "ssr": { "runnerBacked": true, "noExternal": true, "target": "node", "resolve": { "conditions": ["workerd", "worker", "module", "browser", "development|production"] } },
                     "resolve": { "conditions": ["module", "browser", "development|production"] } }"#,
            )
            .unwrap();
            let vars = start_script_env(&cf_shape, "serve", "development").unwrap();
            let cond = vars
                .iter()
                .find(|(n, _)| n == "OJ_RESOLVE_CONDITIONS")
                .map(|(_, v)| v.clone());
            assert_eq!(
                cond.as_deref(),
                Some(r#"["module","node","development","import","default"]"#),
                "neither the workerd sugar nor the client top-level list crosses into the Node loader"
            );
            let _ = std::fs::remove_dir_all(&cf_shape);
        }
        let ssr: serde_json::Value = serde_json::from_str(&get("OJ_DEFINE_SSR").unwrap()).unwrap();
        assert_eq!(ssr["__SIDE__"], "\"server\"");
        assert_eq!(ssr["__ONLY_SSR__"], "true");

        // Defaults: VITE_ only, and no prefix/define vars at all.
        let plain = tmp("script-env-plain");
        std::fs::write(plain.join(".env"), "VITE_X=1\nAPP_Y=2\n").unwrap();
        let vars = start_script_env(&plain, "serve", "development").unwrap();
        let names: Vec<&str> = vars.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"VITE_X"));
        assert!(!names.contains(&"APP_Y"));
        assert!(!names.contains(&"OJ_ENV_PREFIX"));
        assert!(!names.contains(&"OJ_DEFINE_CLIENT"));
        assert!(!names.contains(&"OJ_DEFINE_SSR"));
        assert!(!names.contains(&"OJ_RESOLVE_CONDITIONS"));
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&plain);
    }

    // The static Cloudflare hint gates skipping the boot prewarm: true only
    // when the config file names @cloudflare/vite-plugin, so every other app
    // keeps the current boot path.
    #[test]
    fn cloudflare_hint_reads_the_config_file() {
        let root = tmp("cf-hint");
        assert!(!config_mentions_cloudflare_plugin(&root, &None));
        std::fs::write(
            root.join("vite.config.ts"),
            "import { cloudflare } from \"@cloudflare/vite-plugin\";\nexport default {};\n",
        )
        .unwrap();
        assert!(config_mentions_cloudflare_plugin(&root, &None));
        std::fs::write(root.join("other.config.ts"), "export default {};\n").unwrap();
        assert!(!config_mentions_cloudflare_plugin(
            &root,
            &Some(root.join("other.config.ts"))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    // The live-reload tail goes onto html responses only, and never onto a
    // compressed body (appending plain text would corrupt the encoding).
    #[test]
    fn reload_client_injection_gates_on_html_and_encoding() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let body_of = |resp: Response| {
            rt.block_on(async {
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
                String::from_utf8_lossy(&bytes).into_owned()
            })
        };
        let resp = |ct: &str, enc: Option<&str>| {
            let mut b = Response::builder().header(header::CONTENT_TYPE, ct);
            if let Some(e) = enc {
                b = b.header(header::CONTENT_ENCODING, e);
            }
            b.body(axum::body::Body::from("<html></html>")).unwrap()
        };
        assert!(body_of(inject_reload_client(resp("text/html", None))).contains(RELOAD_CLIENT));
        assert!(!body_of(inject_reload_client(resp("text/html", Some("gzip")))).contains(RELOAD_CLIENT));
        assert!(body_of(inject_reload_client(resp("text/html", Some("identity")))).contains(RELOAD_CLIENT));
        assert!(!body_of(inject_reload_client(resp("text/javascript", None))).contains(RELOAD_CLIENT));
    }

    // Vite's NODE_ENV rule for the dev server: the shell wins (so
    // `NODE_ENV=production oj dev` is PROD), else `.env[.mode]` may only pick
    // development, else development. The shell value is read live, so the
    // test only covers the two file-driven branches.
    #[test]
    fn dev_node_env_follows_env_files_when_shell_is_silent() {
        if std::env::var_os("NODE_ENV").is_some_and(|v| !v.is_empty()) {
            return;
        }
        let root = tmp("node-env");
        assert_eq!(dev_node_env(&root, "development"), "development");
        std::fs::write(root.join(".env.staging"), "NODE_ENV=development\n").unwrap();
        assert_eq!(dev_node_env(&root, "staging"), "development");
        std::fs::write(root.join(".env.prodlike"), "NODE_ENV=production\n").unwrap();
        // Only development is honored from a .env file (Vite warns and ignores).
        assert_eq!(dev_node_env(&root, "prodlike"), "development");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn start_script_env_carries_the_client_resolve_conditions() {
        let root = tmp("client-conds");
        std::fs::write(root.join("package.json"), r#"{"name":"app"}"#).unwrap();
        let conds = |command: &str| -> Vec<String> {
            let vars = start_script_env(&root, command, "development").unwrap();
            let raw = &vars.iter().find(|(k, _)| k == "OJ_CLIENT_CONDITIONS").expect("var").1;
            serde_json::from_str(raw).unwrap()
        };
        // Defaults: Vite's client set, development for dev and production for build.
        let dev = conds("serve");
        assert!(dev.iter().any(|c| c == "browser") && dev.iter().any(|c| c == "development"), "{dev:?}");
        let build = conds("build");
        assert!(build.iter().any(|c| c == "production") && !build.iter().any(|c| c == "development"), "{build:?}");
        // A user resolve.conditions list reaches the client bundle.
        std::fs::write(
            root.join("oj.config.json"),
            r#"{"environments":{"client":{"resolve":{"conditions":["custom","development|production"]}}}}"#,
        )
        .unwrap();
        let custom = conds("serve");
        assert!(custom.iter().any(|c| c == "custom"), "{custom:?}");
        assert!(custom.iter().any(|c| c == "development"), "placeholder mapped: {custom:?}");
        assert!(!custom.iter().any(|c| c == "browser"), "user list replaces the defaults: {custom:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn app_uses_tailwind_detects_postcss_vite_and_bare() {
        let base = tmp("tw");
        let cases = [
            (r#"{"devDependencies":{"@tailwindcss/postcss":"4"}}"#, true),
            (r#"{"devDependencies":{"@tailwindcss/vite":"4"}}"#, true),
            (r#"{"dependencies":{"tailwindcss":"3"}}"#, true),
            (r#"{"dependencies":{"react":"19"}}"#, false),
        ];
        for (i, (pkg, expected)) in cases.iter().enumerate() {
            let app = base.join(format!("app{i}"));
            std::fs::create_dir_all(&app).unwrap();
            std::fs::write(app.join("package.json"), pkg).unwrap();
            assert_eq!(app_uses_tailwind(&app), *expected, "case {i}: {pkg}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn document_url_rewrites_index_html_to_directory() {
        assert_eq!(document_url("/"), "/");
        assert_eq!(document_url("/index.html"), "/");
        assert_eq!(document_url("/guides/index.html"), "/guides/");
        assert_eq!(document_url("/a/b/index.html"), "/a/b/");
        assert_eq!(document_url("/index.html?tab=1"), "/?tab=1");
        assert_eq!(document_url("/a/index.html?x=y&z=1"), "/a/?x=y&z=1");
        assert_eq!(document_url("/about"), "/about");
        assert_eq!(document_url("/guides/foo"), "/guides/foo");
        assert_eq!(document_url("/notindex.html"), "/notindex.html");
    }

    #[test]
    fn percent_decode_handles_escapes_and_partials() {
        assert_eq!(percent_decode("/plain/path.css"), "/plain/path.css");
        assert_eq!(percent_decode("/a%20b"), "/a b");
        assert_eq!(percent_decode("%2Fx"), "/x");
        assert_eq!(percent_decode("%41%42"), "AB");
        assert_eq!(percent_decode("a%"), "a%");
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%zz"), "a%zz");
    }

    #[test]
    fn asset_mime_maps_types() {
        assert_eq!(asset_mime("css"), "text/css; charset=utf-8");
        assert_eq!(asset_mime("js"), "text/javascript");
        assert_eq!(asset_mime("svg"), "image/svg+xml");
        assert_eq!(asset_mime("webp"), "image/webp");
        assert_eq!(asset_mime("woff2"), "font/woff2");
        assert_eq!(asset_mime("json"), "application/json");
        assert_eq!(asset_mime("weirdext"), "application/octet-stream");
    }

    #[test]
    fn needs_css_compile_detects_tailwind_markers() {
        assert!(needs_css_compile("@import \"tailwindcss\";"));
        assert!(needs_css_compile("@tailwind base;"));
        assert!(needs_css_compile("@plugin \"@tailwindcss/typography\";"));
        assert!(needs_css_compile(".btn { @apply px-2; }"));
        assert!(!needs_css_compile(".a { color: red }"));
        assert!(!needs_css_compile("@font-face { src: url(x.woff2) }"));
    }

    #[test]
    fn workspace_root_finds_farthest_node_modules() {
        let base = tmp("ws");
        std::fs::create_dir_all(base.join("node_modules")).unwrap();
        std::fs::create_dir_all(base.join("web").join("src")).unwrap();
        std::fs::create_dir_all(base.join("web").join("node_modules")).unwrap();
        assert_eq!(workspace_root(&base.join("web")), base);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn workspace_root_defaults_to_app_without_ancestor_modules() {
        let base = tmp("ws2");
        let app = base.join("solo");
        std::fs::create_dir_all(&app).unwrap();
        assert_eq!(workspace_root(&app), app);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn app_uses_tailwind_reads_package_json() {
        let base = tmp("tw-pkg");
        std::fs::write(
            base.join("package.json"),
            r#"{"devDependencies":{"@tailwindcss/postcss":"4"}}"#,
        )
        .unwrap();
        assert!(app_uses_tailwind(&base));
        std::fs::write(
            base.join("package.json"),
            r#"{"dependencies":{"react":"19"}}"#,
        )
        .unwrap();
        assert!(!app_uses_tailwind(&base));
        let none = tmp("tw-none");
        assert!(!app_uses_tailwind(&none));
        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&none);
    }

    #[test]
    fn classify_documents_vs_passes() {
        let no_proxy: Vec<String> = vec![];
        assert!(matches!(
            classify(&req("GET", "/"), &no_proxy),
            Route::Document
        ));
        assert!(matches!(
            classify(&req("GET", "/about"), &no_proxy),
            Route::Document
        ));
        assert!(matches!(
            classify(&req("GET", "/index.html"), &no_proxy),
            Route::Document
        ));
        assert!(matches!(
            classify(&req("GET", "/guides/index.html"), &no_proxy),
            Route::Document
        ));
        // A dotted GET is served statically when a file/module owns it and
        // otherwise reaches the SSR handler (server routes, dotted params).
        assert!(matches!(
            classify(&req("GET", "/main.js"), &no_proxy),
            Route::StaticOrDocument
        ));
        assert!(matches!(
            classify(&req("GET", "/styles.css"), &no_proxy),
            Route::StaticOrDocument
        ));
        assert!(matches!(
            classify(&req("GET", "/robots.txt"), &no_proxy),
            Route::StaticOrDocument
        ));
        assert!(matches!(
            classify(&req("GET", "/users/john.doe"), &no_proxy),
            Route::StaticOrDocument
        ));
        assert!(matches!(
            classify(&req("GET", "/api/v1.2/x"), &vec!["/api".to_string()]),
            Route::Pass
        ));
        assert!(matches!(
            classify(&req("GET", "/@oj-start/hmr"), &no_proxy),
            Route::Pass
        ));
        assert!(matches!(
            classify(&req("GET", "/__health"), &no_proxy),
            Route::Pass
        ));
        assert!(matches!(
            classify(&req("POST", "/api/auth/session-v2"), &no_proxy),
            Route::Api
        ));
        assert!(matches!(
            classify(&req("PUT", "/api/thing"), &no_proxy),
            Route::Api
        ));
        assert!(matches!(
            classify(&req("DELETE", "/api/thing"), &no_proxy),
            Route::Api
        ));
        // Dotted non-GETs are the app's (Vite hands every unowned request to
        // it): a POST to a dotted server route or a PUT to a dotted path.
        assert!(matches!(
            classify(&req("POST", "/main.js"), &no_proxy),
            Route::Api
        ));
        assert!(matches!(
            classify(&req("POST", "/api/export.csv"), &no_proxy),
            Route::Api
        ));
        assert!(matches!(
            classify(&req("PUT", "/files/a.b"), &no_proxy),
            Route::Api
        ));
        let proxy = vec!["/api".to_string()];
        assert!(matches!(
            classify(&req("GET", "/api/users"), &proxy),
            Route::Pass
        ));
        assert!(matches!(
            classify(&req("POST", "/api/users"), &proxy),
            Route::Pass
        ));
    }

    #[test]
    fn list_route_files_collects_ts_tsx_recursively() {
        let root = tmp("routes");
        let routes = root.join("src").join("routes");
        std::fs::create_dir_all(routes.join("nested")).unwrap();
        std::fs::write(routes.join("index.tsx"), "").unwrap();
        std::fs::write(routes.join("about.ts"), "").unwrap();
        std::fs::write(routes.join("styles.css"), "").unwrap();
        std::fs::write(routes.join("data.json"), "").unwrap();
        std::fs::write(routes.join("nested").join("deep.tsx"), "").unwrap();
        let found = list_route_files(&root);
        let names: Vec<String> = found
            .iter()
            .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .collect();
        assert_eq!(found.len(), 3, "only ts/tsx counted: {names:?}");
        assert!(names.contains(&"index.tsx".to_string()));
        assert!(names.contains(&"about.ts".to_string()));
        assert!(
            names.contains(&"deep.tsx".to_string()),
            "recursion into subdirs: {names:?}"
        );
        assert!(!names
            .iter()
            .any(|n| n.ends_with(".css") || n.ends_with(".json")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_src_ts_files_walks_all_of_src() {
        let root = tmp("srcwalk");
        let src = root.join("src");
        std::fs::create_dir_all(src.join("routes")).unwrap();
        std::fs::create_dir_all(src.join("lib")).unwrap();
        std::fs::write(src.join("routes").join("index.tsx"), "").unwrap();
        std::fs::write(src.join("lib").join("fns.ts"), "").unwrap();
        std::fs::write(src.join("lib").join("types.d.ts"), "").unwrap();
        std::fs::write(src.join("styles.css"), "").unwrap();
        let found = list_src_ts_files(&root);
        assert_eq!(found.len(), 3, "ts/tsx (incl. .d.ts) only: {found:?}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn list_route_files_missing_dir_is_empty() {
        let root = tmp("noroutes");
        assert!(list_route_files(&root).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn collect_headers_serializes_utf8_and_skips_non_utf8() {
        let mut h = header::HeaderMap::new();
        h.insert(
            "content-type",
            header::HeaderValue::from_static("text/html"),
        );
        h.insert("x-custom", header::HeaderValue::from_static("hello"));
        h.insert(
            "x-bin",
            header::HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
        );
        let out = collect_headers(&h);
        assert!(
            out.contains(&("content-type".to_string(), "text/html".to_string())),
            "{out:?}"
        );
        assert!(
            out.contains(&("x-custom".to_string(), "hello".to_string())),
            "{out:?}"
        );
        assert!(
            !out.iter().any(|(k, _)| k == "x-bin"),
            "non-utf8 value dropped: {out:?}"
        );
    }

    #[test]
    fn a_relative_config_path_resolves_against_the_app_root() {
        let root = Path::new("/srv/app");
        assert_eq!(
            config_path(root, Some(PathBuf::from("build/vite.config.mjs"))),
            Some(PathBuf::from("/srv/app/build/vite.config.mjs")),
        );
        assert_eq!(
            config_path(root, Some(PathBuf::from("/out/vite.config.mjs"))),
            Some(PathBuf::from("/out/vite.config.mjs")),
        );
        assert_eq!(config_path(root, None), None);
    }

    #[test]
    fn configured_start_server_entry_reads_the_start_alias_for_app_files_only() {
        let root = std::env::temp_dir().join(format!("oj-start-entry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/@tanstack/react-start/dist")).unwrap();
        std::fs::write(root.join("src/ssr-entry.ts"), "export default {};").unwrap();
        std::fs::write(root.join("node_modules/@tanstack/react-start/dist/server-entry.js"), "").unwrap();
        let cfg = |target: &str| -> oj_config::OjConfig {
            serde_json::from_str(&format!(
                r#"{{"resolve":{{"alias":{{"virtual:tanstack-start-server-entry":"{}"}}}}}}"#,
                target.replace('\\', "/")
            ))
            .unwrap()
        };
        let app = root.join("src/ssr-entry.ts");
        assert_eq!(
            configured_start_server_entry(&cfg(&app.to_string_lossy()), &root),
            Some(app.canonicalize().unwrap()),
            "an app file named by the Start alias is the server entry"
        );
        assert_eq!(configured_start_server_entry(&cfg("src/ssr-entry.ts"), &root).is_some(), true, "root-relative works too");
        let pkg = root.join("node_modules/@tanstack/react-start/dist/server-entry.js");
        assert_eq!(configured_start_server_entry(&cfg(&pkg.to_string_lossy()), &root), None, "Start's own default entry is not an app entry");
        assert_eq!(configured_start_server_entry(&cfg("src/missing.ts"), &root), None);
        let none: oj_config::OjConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(configured_start_server_entry(&none, &root), None);
        let env_scoped: oj_config::OjConfig = serde_json::from_str(&format!(
            r#"{{"environments":{{"ssr":{{"resolve":{{"alias":{{"virtual:tanstack-start-server-entry":"{}"}}}}}}}}}}"#,
            app.to_string_lossy().replace('\\', "/")
        ))
        .unwrap();
        assert!(configured_start_server_entry(&env_scoped, &root).is_some(), "environments.ssr.resolve.alias counts");
        let _ = std::fs::remove_dir_all(&root);
    }
}
