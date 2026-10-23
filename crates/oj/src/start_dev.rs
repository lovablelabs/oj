// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};
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
use oj_server::needs_css_compile;
use tokio::sync::broadcast;

use crate::start_chunks::{respond, Chunk};
use crate::start_host::{StartEngine, StartRequest, StartResponse};

struct StartState {
    /// The scripts' env (see `script_env`), resolved once at boot for the
    /// watch-reload codegen and rebundles.
    script_env: Arc<[(String, String)]>,
    /// The session's client bundler, shared by the boot bundle and rebundles.
    bundler: Arc<ClientBundler>,
    proxy_prefixes: Vec<String>,
    /// Live plugin-middleware state, shared with oj_server: the middleware port
    /// and whether worker environments serve the documents. A slow plugin host
    /// (many plugins, Miniflare) activates it after boot, so every read goes
    /// through it per request instead of a boot-time snapshot.
    plugin_serve: Arc<oj_server::PluginServe>,
    /// The in-process Start runner (the embedded engine + module host).
    engine: Arc<StartEngine>,
    /// Loopback port for the plugin middleware's `x-oj-forward-to` pipe: a
    /// tiny HTTP shim that calls the engine, standing in for the node
    /// runner's loopback server.
    loopback_port: u16,
    /// Content hashes of the regen outputs as last pushed to the worker
    /// environments. Tracked in state (not per-run before/after snapshots):
    /// the framework's own router-generator plugin in the plugin host also
    /// rewrites the route tree on watchChange, and racing it per run would
    /// randomly hide a change it wrote first.
    regen_hashes: std::sync::Mutex<std::collections::HashMap<PathBuf, Option<u64>>>,
    bundle: std::sync::RwLock<Arc<oj_cache::start_bundle::PinnedBundle>>,
    /// Gzip bodies of the live client chunks, by content hash.
    gzip: crate::start_chunks::GzipCache,
    live_reload: PathBuf,
    /// `server.hmr.overlay` (default true), filled into the live-reload client.
    hmr_overlay: bool,
    workspace_root: PathBuf,
    reload_tx: broadcast::Sender<()>,
    /// The client bundle's last failure as a `{type:"error", err}` frame,
    /// Vite's bundled dev `lastBuildError`: sent to every page when a bundle
    /// fails, replayed to each page that connects until one succeeds.
    build_error: std::sync::RwLock<Option<String>>,
    error_tx: broadcast::Sender<String>,
    // The oj_server /__ws broadcast: the channel the editor reads boot +
    // update narration frames from (the start path's own /@oj-start/hmr socket
    // only drives the app iframe's live reload).
    ws_tx: broadcast::Sender<String>,
    /// A page's `import.meta.hot.send` (event, JSON data), to the plugins'
    /// `server.ws.on(event)` listeners.
    to_plugins: ToPlugins,
    css_host: Option<Arc<oj_server::css_engine::CssEngine>>,
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
    /// The last client bundle's complete input set (closure.json: every module
    /// id the bundler touched, plus watch files and config deps). A batch that
    /// misses it entirely cannot change the bundle, so the rebundle is skipped
    /// (rollup watch semantics; Vite's module graph plays this role). Empty
    /// means unknown, which always rebundles.
    client_closure: std::sync::RwLock<std::collections::HashSet<PathBuf>>,
    /// Every `import.meta.glob` the bundle expanded (glob-scopes.json). A
    /// new file one matches grows its expansion, so it rebundles; a new file
    /// anywhere else cannot change a bundle whose inputs all resolved,
    /// and is ignored -- Vite's add handling, which only re-expands the
    /// importers of the globs an added file matches (resolution shifts land
    /// when an importer changes or the bundle failed, there as here).
    client_glob_scopes: std::sync::RwLock<Vec<oj_compiler::glob::GlobMatcher>>,
    /// False after a failed rebundle: the closure may be a torso (the build
    /// stopped early), so every next batch rebundles until one succeeds.
    bundle_ok: std::sync::atomic::AtomicBool,
    /// A runner-rendered document handed a page the Start client bundle. The
    /// worker environments' documents never do: TanStack's own manifest
    /// answers there, and its script is the unbundled dev client entry, so
    /// those pages run the client module graph, as under Vite.
    bundle_loaded: std::sync::atomic::AtomicBool,
    /// Edits changed the bundle's inputs while no page loaded it, and no
    /// rebundle ran for them; the next runner document brings it current.
    bundle_deferred: std::sync::atomic::AtomicBool,
    /// True while the deferred bundle is building: a runner document waits
    /// for it rather than handing out the empty one.
    deferred_build: tokio::sync::watch::Sender<bool>,
    /// The coalescing rebundle worker's queue (at most one pending run) and
    /// its wake.
    rebundles: Arc<std::sync::Mutex<Option<PendingRebundle>>>,
    rebundle_wake: Arc<tokio::sync::Notify>,
    /// The dev server's SSR bridge, here for its module-graph lookup: files
    /// the generic pipeline serves (a dev worker script fetched by URL) are
    /// in no other graph the reload decision consults.
    ssr: oj_server::SsrBridge,
    /// Hot updates outrun the client output: a rebundle whose changes all
    /// went out as patches writes no output (on a large app that is most of
    /// the time between two saves), and the refresher brings it current once
    /// edits pause. A document waits for it (`HotUpdates::stale`), so a
    /// reload never loads the bundle from before an edit.
    ///
    /// Wakes the refresher; `urgent` skips its wait for edits to pause (a
    /// document or a page's reload waits on it).
    refresh_wake: tokio::sync::Notify,
    urgent: std::sync::atomic::AtomicBool,
    /// Refreshes started.
    refreshes: std::sync::atomic::AtomicU64,
    /// A page found no hot-update boundary: the refreshes started count it
    /// saw. The first refresh started after that reloads the pages once its
    /// output is current (Vite's reload-needed flush).
    reload_wanted: std::sync::Mutex<Option<u64>>,
}

impl StartState {
    fn page_needs_reload(&self) {
        let mark = self.refreshes.load(std::sync::atomic::Ordering::SeqCst);
        self.reload_wanted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get_or_insert(mark);
        self.refresh_now();
    }

    fn refresh_now(&self) {
        self.urgent.store(true, std::sync::atomic::Ordering::SeqCst);
        self.refresh_wake.notify_one();
    }

    /// Waits until the client output covers every hot update (bounded: a
    /// stuck refresh must not hold documents forever).
    async fn client_output_fresh(&self) {
        let mut stale = self.bundler.hot.stale();
        if !*stale.borrow() {
            return;
        }
        self.refresh_now();
        let _ =
            tokio::time::timeout(std::time::Duration::from_secs(60), stale.wait_for(|s| !*s)).await;
    }

    /// Cloudflare worker environments serve the documents; the runner is only a
    /// fallback, kept cold and reloaded lazily (see `runner_dirty`) instead of
    /// eagerly on every edit. Live: a slow plugin host flips this after boot.
    fn lazy_runner(&self) -> bool {
        self.plugin_serve.runner_environments()
    }

    /// Whether any page can be running the Start client bundle: with the
    /// worker environments serving the documents, only one the fallback
    /// runner rendered.
    fn pages_run_bundle(&self) -> bool {
        !self.lazy_runner() || self.bundle_loaded.load(std::sync::atomic::Ordering::SeqCst)
    }
}

// What the watcher forwards and rebuilds on: not the generated route tree (its
// writes would loop the generator) nor the generator's temp files under
// .tanstack/ (renamed into the tree), not declaration files (never runtime
// modules; generated message catalogs write them by the thousand), and not
// bare directory events (Linux inotify emits a parent-dir event alongside the
// file write; Vite never hands directories to hotUpdate hooks either).
fn watch_relevant(p: &Path) -> bool {
    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
    !name.contains("routeTree.gen")
        && !is_declaration_file(name)
        && !p.components().any(|c| c.as_os_str() == ".tanstack")
        && !p.is_dir()
}

fn is_declaration_file(name: &str) -> bool {
    [".d.ts", ".d.mts", ".d.cts"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

/// Paths whose change can flip which file `app_start_entry` resolves to: the
/// `src/start.*` ladder and any `package.json`, whose `imports` may map the
/// entry. Name-shaped matching over-approximates (a nested src dir, a dependency's package.json); the
/// re-probe a hit triggers is a handful of stats, and the server restarts
/// only when the resolved entry actually changed.
fn start_entry_candidate(p: &Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    if name == "package.json" {
        return true;
    }
    let under = |p: &Path, dir: &str| {
        p.parent()
            .and_then(|d| d.file_name())
            .is_some_and(|n| n == dir)
    };
    under(p, "src") && name.starts_with("start.")
}

// Vite's watcher distinguishes add/change/unlink; notify's batched events are
// classified at send time: gone from disk is a delete, an add this batch saw
// (`KnownFiles::adds`) is a create, anything else an update.
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
        .or_else(|| oj_server::plugins::vite_config_file(root, config.as_deref()));
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

    // Same order `build` uses: resolve --config against the root once; it is
    // threaded to every config read from here on.
    let config_file = config_path(&root, config);
    let cf_hint = config_mentions_cloudflare_plugin(&root, &config_file);
    let cache = oj_cache::cache_root(&root).join("start");
    oj_server::prepare_cache_root(&root);
    oj_server::write_start_assets(&cache)?;
    oj_server::boot_phase("start_dev begin");

    // Before boot: the long start boot is where supervisor signals land, and
    // plugin children (workerd) spawn well before the server listens.
    let host_slot = std::sync::Arc::new(std::sync::OnceLock::new());
    tokio::spawn(oj_server::close_plugins_on_shutdown(host_slot.clone()));
    let built_task = tokio::spawn({
        let dev_server = oj_server::DevServer {
            root: root.clone(),
            port,
            host,
            config: config_file.clone(),
            enable_cache: false,
            no_cache: false,
            lazy: false,
            mode: Some(mode.clone()),
        };
        async move {
            let built = dev_server.build_app().await?;
            // Subscribed the moment the server (and its watcher) exists, not
            // after the client bundle joins: an edit landing during the rest
            // of boot queues for the Start watcher instead of being lost.
            let watch_rx = built.watch_feed.subscribe();
            anyhow::Ok((built, watch_rx))
        }
    });

    // The two codegen steps run SERIALIZED on purpose, not as a lost
    // concurrency opportunity: the route-tree generator writes
    // src/routeTree.gen.ts and rewrites route files in place (the generator
    // keeps `createFileRoute` ids in sync), while the server-fn resolver reads
    // every .ts/.tsx under src — the generated tree included — and the
    // resolver's codegen cache hashes those same files as its key. Running
    // them concurrently would let the resolver scan half-written files and
    // persist a cache key over content that was still moving.
    // The scripts' env, resolved once and shared by boot and every rebundle.
    let script_vars: Arc<[(String, String)]> =
        script_env(oj_env::get(), &root, config_file.as_deref(), "serve", &mode)?.into();
    let route_tree = {
        let (root, cache, env) = (root.clone(), cache.clone(), Arc::clone(&script_vars));
        tokio::task::spawn_blocking(move || generate_route_tree(&root, &cache, &env))
    };
    route_tree.await??;
    oj_server::boot_phase("route tree ready");
    let resolver = {
        let (root, cache) = (root.clone(), cache.clone());
        tokio::task::spawn_blocking(move || generate_server_fn_resolver(&root, &cache))
    };
    resolver.await??;
    oj_server::boot_phase("resolver ready");
    let bundler = {
        let inputs = script_inputs(oj_env::get(), &root, config_file.as_deref(), "serve", &mode)?;
        let start = crate::start_bundle::StartConfig {
            base: crate::start_bundle::normalize_base(inputs.config.base.as_deref()),
            root: root.clone(),
            cache: cache.clone(),
            command: crate::start_bundle::Command::Serve,
            mode: mode.clone(),
            config: inputs.config,
            app_env: inputs.app_env,
        };
        Arc::new(ClientBundler::new(start, config_file.clone()))
    };
    // A Cloudflare app's documents come from the worker environments, which
    // render them from TanStack's own manifest: their pages load the dev
    // client entry, never the Start bundle. It is built when a runner
    // document first hands it out (`ensure_bundle_current`), so a session
    // whose worker serves every document never starts the bundler at all.
    let defer_bundle = cf_hint;
    let bundle = {
        let (bundler, cache, mode) = (Arc::clone(&bundler), cache.clone(), mode.clone());
        async move {
            if defer_bundle {
                oj_server::boot_phase("client bundle deferred (worker-rendered documents)");
                return Ok(Ok(None));
            }
            tokio::task::spawn_blocking(move || {
                bundle_client_entry_cached(&bundler, &cache, &mode).map(Some)
            })
            .await
        }
    };
    let (reload_tx, _) = broadcast::channel::<()>(16);
    let (bundle_res, built_res) = tokio::join!(bundle, built_task);
    // A build error in the app's code (client-error.json records it) keeps
    // the server up, as Vite's bundled dev does: pages render the overlay and
    // the next edit rebundles. Anything else (the bundler failing to load)
    // still aborts the boot.
    let (pinned, boot_error) = match bundle_res? {
        Ok(Some(pinned)) => (pinned, None),
        Ok(None) => (oj_cache::start_bundle::PinnedBundle::empty(), None),
        Err(e) => match client_error_frame(&cache) {
            Some(frame) => {
                eprintln!("  oj start: client bundle failed, serving the error overlay: {e:#}");
                (oj_cache::start_bundle::PinnedBundle::empty(), Some(frame))
            }
            None => return Err(e),
        },
    };
    let (built, watch_rx) = built_res??;
    oj_server::boot_phase("bundle+build joined");
    // The in-process Start runner: an embedded engine whose module host runs
    // the dev server's SSR pipeline (StartHost); it needs the built app's
    // SsrBridge, so it comes up once the server build joins.
    // The app's own server.warmup lists (Vite semantics, globs included), from
    // the adopted config; they join the route tree in the runner transform
    // warm below.
    let warm_patterns;
    let hmr_overlay;
    let engine = {
        let mut config = oj_config::load(&root).unwrap_or_default();
        oj_server::plugins::adopt_vite_config_values(
            &mut config,
            &root,
            config_file.as_deref(),
            "serve",
            &mode,
        )
        .map_err(|e| anyhow::anyhow!(e))?;
        warm_patterns = {
            let (client, ssr) = oj_config::server_warmup_files(&config);
            [client, ssr].concat()
        };
        hmr_overlay = match config.server.as_ref().and_then(|s| s.hmr.as_ref()) {
            Some(oj_config::HmrConfig::Options(o)) => o.overlay.unwrap_or(true),
            _ => true,
        };
        let entry = configured_start_server_entry(&config, &root)
            .unwrap_or_else(|| cache.join("server-entry.tsx"));
        let init_env = script_vars.to_vec();
        Arc::new(StartEngine::new(
            root.clone(),
            &cache,
            built.ssr.clone(),
            entry,
            oj_config::ssr_externals(&config),
            oj_config::public_dir(&config, &root),
            init_env,
        )?)
    };
    let loopback_port = spawn_engine_loopback(Arc::clone(&engine)).await?;
    oj_server::boot_phase("engine ready");
    // Whether the plugin's worker environments serve the documents is known
    // only once the plugin host announces its serve info (the `{ ojServeInfo }`
    // push, mirrored on the host's watch channel); a Cloudflare config holds
    // the prewarm until the serve info is KNOWN — bounded, so a host that
    // never comes up still gets a warm engine — and skips it iff the worker
    // environments render (warming the engine is wasted CPU then).
    let prewarm_hold = built.plugin_host.as_ref().map(|h| PrewarmHold {
        updates: h.serve_info_updates(),
        host_gone: h.host_gone_updates(),
        init_failed: h.init_failure_updates(),
        init_deadline: h.init_warn_at(),
        confirm: oj_server::plugins::PROBE_WINDOW,
    });
    // Filled once the router exists (BEFORE the listener binds): the
    // runner-environment warm below drives one render by calling the service
    // directly, Vite's work-before-listen shape — its monkey-patched
    // httpServer.listen runs every environment.listen (dep-optimizer init +
    // fire-and-forget warmupFiles) before the socket binds, so warming never
    // queues behind bind. oj's bind additionally waits on the client bundle,
    // which is exactly the wait this overlap removes. Calling the service
    // also sidesteps the bound-interface question a TCP self-request had
    // (::1 vs 127.0.0.1 vs a specific host): there is no socket to dial.
    let (app_tx, app_rx) = tokio::sync::oneshot::channel::<axum::Router>();
    // Sent once the prewarm knows oj's runner renders the documents: their
    // pages load the Start bundle, so a deferred one starts building then.
    let (runner_documents_tx, runner_documents_rx) = tokio::sync::oneshot::channel::<()>();
    {
        let engine = Arc::clone(&engine);
        let reload_tx = reload_tx.clone();
        let warm_host = built.plugin_host.clone();
        let warm_root = root.clone();
        tokio::spawn(async move {
            if cf_hint {
                if let Some(mut hold) = prewarm_hold {
                    // Held until the serve info is KNOWN or there is wedge
                    // EVIDENCE (host death, a burned init window, the host's
                    // own init deadline) — see hold_prewarm_for_serve_info.
                    if let Some(known) = hold_prewarm_for_serve_info(&mut hold).await {
                        if known.middleware_port.is_some() && known.runner_environments {
                            // Warming oj's OWN engine is wasted here (the
                            // worker environments render the documents), but
                            // skipping entirely left the first request to pay
                            // the whole cold graph. Two warms instead, like
                            // Vite's warmup but fed from the route tree:
                            // every route module's transform fires at once in
                            // the host (parallel across cores), while one
                            // real render through the serving path chases the
                            // warm transforms and fills the runner's module
                            // cache. transformRequest dedups the overlap.
                            oj_server::boot_phase("prewarm: engine skipped (worker environments)");
                            let transforms = async {
                                let urls = runner_warm_urls(&warm_root, &warm_patterns);
                                if urls.is_empty() {
                                    return;
                                }
                                if let Some(host) = warm_host.as_deref() {
                                    // The host DISPATCHES the warms and replies
                                    // immediately (Vite's warmup is equally
                                    // fire-and-forget); awaiting completion
                                    // would park a cold-graph warm behind one
                                    // RPC window.
                                    match host.warm_environments(&urls).await {
                                        Ok(Some(summary)) => oj_server::boot_phase(&format!(
                                            "runner transform warm dispatched ({summary})"
                                        )),
                                        Ok(None) => oj_server::boot_phase(
                                            "runner transform warm unavailable (nothing warmable)",
                                        ),
                                        Err(_) => oj_server::boot_phase(
                                            "runner transform warm unavailable",
                                        ),
                                    }
                                }
                            };
                            let render = async {
                                if let Ok(app) = app_rx.await {
                                    if runner_render_warm(app).await {
                                        oj_server::boot_phase("runner render warm complete");
                                    } else {
                                        oj_server::boot_phase(
                                            "runner render warm failed (best effort)",
                                        );
                                    }
                                }
                            };
                            tokio::join!(transforms, render);
                            return;
                        }
                    }
                }
            }
            let _ = runner_documents_tx.send(());
            let _ = engine
                .handle(engine_request(
                    "GET".into(),
                    "/".into(),
                    &header::HeaderMap::new(),
                    None,
                    false,
                ))
                .await;
            oj_server::boot_phase("prewarm complete");
            if engine.revalidate().await {
                let _ = reload_tx.send(());
            }
            oj_server::boot_phase("revalidate complete");
        });
    }
    let css_host = if app_uses_tailwind(&root) {
        oj_server::css_engine::CssEngine::tailwind(&root, Some(built.ssr.engine_registry()))
            .await
            .ok()
    } else {
        None
    };
    // The resolved gate (the knob or `server.hmrGate`): a gate on drops the
    // boot's engine at the next rebundle.
    bundler.set_hot_enabled(built.hmr_gate.is_none());
    let state = Arc::new(StartState {
        script_env: Arc::clone(&script_vars),
        bundler,
        proxy_prefixes: built.proxy_prefixes.clone(),
        plugin_serve: Arc::clone(&built.plugin_serve),
        engine,
        loopback_port,
        bundle: std::sync::RwLock::new(Arc::new(pinned)),
        gzip: Default::default(),
        live_reload: cache.join("live-reload.js"),
        hmr_overlay,
        workspace_root: workspace_root(&root),
        reload_tx: reload_tx.clone(),
        build_error: std::sync::RwLock::new(boot_error.clone()),
        error_tx: broadcast::channel(16).0,
        ws_tx: built.reload_tx.clone(),
        to_plugins: {
            let host = built.plugin_host.clone();
            Arc::new(move |event: &str, data: &str| {
                let Some(host) = host.clone() else { return };
                let (event, data) = (event.to_string(), data.to_string());
                tokio::spawn(async move {
                    let _ = host.ws_message(&event, &data).await;
                });
            })
        },
        gate: built.hmr_gate.clone(),
        css_host,
        mode: mode.clone(),
        runner_dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        regen_hashes: std::sync::Mutex::new(
            regen_output_files(&root, &cache)
                .into_iter()
                .map(|p| {
                    let h = file_content_hash(&p);
                    (p, h)
                })
                .collect(),
        ),
        client_closure: std::sync::RwLock::new(load_client_closure(&cache)),
        client_glob_scopes: std::sync::RwLock::new(load_client_glob_scopes(&cache)),
        // A failed boot bundle rebundles on every next batch until one succeeds.
        bundle_ok: std::sync::atomic::AtomicBool::new(boot_error.is_none()),
        bundle_loaded: std::sync::atomic::AtomicBool::new(false),
        bundle_deferred: std::sync::atomic::AtomicBool::new(defer_bundle),
        deferred_build: tokio::sync::watch::channel(false).0,
        rebundles: Arc::new(std::sync::Mutex::new(None)),
        rebundle_wake: Arc::new(tokio::sync::Notify::new()),
        ssr: built.ssr.clone(),
        refresh_wake: tokio::sync::Notify::new(),
        urgent: std::sync::atomic::AtomicBool::new(false),
        refreshes: std::sync::atomic::AtomicU64::new(0),
        reload_wanted: std::sync::Mutex::new(None),
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
    spawn_start_watcher(root.clone(), cache.clone(), Arc::clone(&state), watch_rx);
    // Once the serve info says oj's runner renders the documents (no worker
    // environment does, or the host never announced), their pages load the
    // Start bundle: build a deferred one then, as Vite's bundled dev builds
    // at startup, instead of making the first document wait for it.
    {
        let state = Arc::clone(&state);
        tokio::spawn(async move {
            if runner_documents_rx.await.is_ok() {
                start_deferred_bundle(&state);
            }
        });
    }
    {
        let (root, mode) = (root.clone(), mode.clone());
        tokio::task::spawn_blocking(move || {
            start_bundle_store(&root, &mode)
                .prune(oj_cache::start_bundle::DEFAULT_PRUNE_BUDGET_BYTES);
        });
    }

    let app = built
        .router
        .clone()
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            start_route,
        ));

    let _ = app_tx.send(app.clone());
    let (listener, port) = built.bind_listener().await?;
    oj_server::boot_phase("listening");
    if let Some(host) = built.plugin_host.clone() {
        let _ = host_slot.set(host);
    }
    println!("  {} dev (tanstack start)", oj_server::oj_brand());
    let url = format!(
        "{}://localhost:{}/",
        oj_server::tls::url_scheme(built.https.as_ref()),
        port
    );
    println!("  {}", oj_server::link(&url, &oj_server::cell(&url)));
    oj_server::tls::serve_router(listener, app, built.https.as_ref()).await?;
    Ok(())
}

/// Queues the bundle left unbuilt (a Cloudflare boot) or stale (edits while
/// no page loaded it, see `StartState::pages_run_bundle`), once: whoever
/// needs it first starts it, and `deferred_build` stays raised until it ran.
fn start_deferred_bundle(state: &Arc<StartState>) {
    if !state
        .bundle_deferred
        .swap(false, std::sync::atomic::Ordering::SeqCst)
    {
        return;
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    state.deferred_build.send_replace(true);
    {
        let mut slot = state
            .rebundles
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let run = slot.get_or_insert_with(PendingRebundle::default);
        run.force = true;
        run.done.push(tx);
    }
    state.rebundle_wake.notify_one();
    let state = Arc::clone(state);
    tokio::spawn(async move {
        let _ = rx.await;
        state.deferred_build.send_replace(false);
    });
}

/// A runner document loads the Start client bundle: it waits for a deferred
/// one, its own or one boot already started (bounded, like
/// `client_output_fresh`), and edits from then on rebundle it as they come.
async fn ensure_bundle_current(state: &Arc<StartState>) {
    state
        .bundle_loaded
        .store(true, std::sync::atomic::Ordering::SeqCst);
    start_deferred_bundle(state);
    let mut building = state.deferred_build.subscribe();
    let _ = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        building.wait_for(|b| !*b),
    )
    .await;
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
        state.engine.reload().await;
    }
}

/// The urls the runner-environment transform warm fires: every route module
/// plus the router entry, as root-relative urls (Vite's warmup.ts fileToUrl
/// shape), joined by the app's own `server.warmup` lists expanded with Vite's
/// glob semantics. The route tree is what the first render will walk, so
/// warming it is the whole graph minus dependencies the optimizer already
/// bundled.
fn runner_warm_urls(root: &Path, warmup_patterns: &[String]) -> Vec<String> {
    let mut files: std::collections::BTreeSet<PathBuf> = list_route_files(root);
    for entry in ["src/router.tsx", "src/router.ts"] {
        let p = root.join(entry);
        if p.is_file() {
            files.insert(p);
        }
    }
    files.extend(oj_server::warmup_paths(root, warmup_patterns));
    files
        .iter()
        .filter_map(|f| f.strip_prefix(root).ok())
        .map(|rel| format!("/{}", rel.to_string_lossy().replace('\\', "/")))
        .collect()
}

/// One real GET / through oj's own router service — the serving path a user
/// request takes (middleware forward, worker-environment render) minus the
/// socket, so the warm needs no bound listener and overlaps bind and the
/// client bundle (Vite's work-before-listen ordering: its patched
/// httpServer.listen runs environment.listen, and with it warmupFiles,
/// before the socket binds). Executing app server code with no request in
/// sight is deliberate: Vite's `server.open` does the same (its own GET of
/// the entry with `Accept: text/html`), and a sandbox boot has exactly one
/// app to serve. The streamed body is drained — the render only completes
/// when its document does — and the result says whether a response actually
/// came back so the caller can log honestly. Best effort with a generous
/// bound; a cold monorepo render is minutes on a contended sandbox.
async fn runner_render_warm(app: axum::Router) -> bool {
    use futures_util::StreamExt;
    use tower::util::ServiceExt;
    let warm = async {
        let Ok(request) = axum::http::Request::builder()
            .uri("/")
            // Pre-bind there is no port to put in the origin: an app
            // middleware building absolute URLs from Host sees a portless
            // localhost during the warm (the render's output is discarded, so
            // only the module caches it fills matter).
            .header(axum::http::header::HOST, "localhost")
            .header(axum::http::header::ACCEPT, "text/html")
            .body(axum::body::Body::empty())
        else {
            return false;
        };
        let Ok(response) = app.oneshot(request).await;
        let ok = response.status().is_success();
        // Drain the stream chunk by chunk and discard: the render only
        // completes when its document does, without buffering a whole
        // streamed SSR body just to throw it away.
        let mut body = response.into_body().into_data_stream();
        while let Some(chunk) = body.next().await {
            if chunk.is_err() {
                break;
            }
        }
        ok
    };
    matches!(
        tokio::time::timeout(std::time::Duration::from_secs(600), warm).await,
        Ok(true)
    )
}

/// The routes directory the app configured (tsr.config.json's
/// `routesDirectory`, the same file the route-tree generator merges its
/// defaults under) or the generator's default. Rust-side walkers must agree
/// with generate.mjs: a custom layout otherwise regenerates trees the watcher
/// never notices and warms nothing.
fn resolved_routes_dir(root: &Path) -> PathBuf {
    let configured = std::fs::read_to_string(root.join("tsr.config.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| {
            v.get("routesDirectory")
                .and_then(|d| d.as_str())
                .map(String::from)
        });
    let rel = configured.unwrap_or_else(|| "./src/routes".to_string());
    root.join(rel.trim_start_matches("./"))
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
    walk(&resolved_routes_dir(root), &mut out);
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
    /// Every changed path of the merged batches (relevant and not), for the
    /// regen decisions; the HMR gate recorded them at the watcher event.
    paths: std::collections::HashSet<PathBuf>,
    /// The worker-invalidate sends (early and settled) in flight for the
    /// merged batches: this run's browser reload waits for them, so a reload
    /// never lands while the worker environments still hold stale modules,
    /// and their replies say whether any worker graph actually served a
    /// changed module.
    invalidates: Vec<tokio::task::JoinHandle<bool>>,
    /// The newest merged batch number: its done-frame clears the editor pill.
    batch: u64,
    /// A runner document is about to hand pages the bundle that edits left
    /// stale (`bundle_deferred`): rebundle whatever the paths say.
    force: bool,
    /// Signalled once this run completes.
    done: Vec<tokio::sync::oneshot::Sender<()>>,
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
) -> Option<tokio::task::JoinHandle<bool>> {
    let port = state.plugin_serve.mw_port()?;
    let changed: Vec<(String, &'static str)> = paths
        .iter()
        .filter(|p| watch_relevant(p))
        .map(|p| (p.to_string_lossy().into_owned(), change_type(p, created)))
        .collect();
    if changed.is_empty() {
        return None;
    }
    Some(rt.spawn(async move { oj_server::notify_plugin_mw_invalidate(port, &changed).await }))
}

/// The last successful bundle's input set (closure.json, start_bundle::dev).
/// Empty on any read or parse failure: unknown always rebundles.
fn load_client_closure(cache: &Path) -> std::collections::HashSet<PathBuf> {
    std::fs::read_to_string(cache.join("closure.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<PathBuf>>(&s).ok())
        .map(|v| v.into_iter().collect())
        .unwrap_or_default()
}

/// Every glob call the last bundle expanded, with Vite's matcher for each.
fn load_client_glob_scopes(cache: &Path) -> Vec<oj_compiler::glob::GlobMatcher> {
    load_glob_calls(&cache.join("glob-scopes.json"))
        .iter()
        .map(oj_compiler::glob::GlobCall::matcher)
        .collect()
}

/// The glob calls a bundle expanded, as the client bundle writes them beside
/// the closure (Vite's resolved patterns and options, see
/// `oj_compiler::glob::GlobCall`). Missing or unreadable is an empty list:
/// the closure check still rebundles for every known input, and the
/// bundle-ok flag for every failed one.
fn load_glob_calls(file: &Path) -> Vec<oj_compiler::glob::GlobCall> {
    let Some(entries) = std::fs::read_to_string(file)
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(&s).ok())
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| match e.as_str() {
            // A cache an older oj wrote holds scan bases: anything under one.
            Some(base) => Some(oj_compiler::glob::GlobCall {
                patterns: vec![format!("{base}/**")],
                exhaustive: true,
                case_sensitive: true,
            }),
            None => oj_compiler::glob::GlobCall::from_json(e),
        })
        .collect()
}

/// A file a bundled glob matches but the bundle never read is one the glob's
/// expansion grows by (the one way a NEW file changes a bundle whose inputs
/// all resolved), however the watcher reported it: a file renamed into place
/// arrives as a rename, and chokidar calls any path it did not know an add.
/// Plain dev's glob importers use the same rule. The watcher spelling or its
/// real path, like the closure check.
fn added_to_glob(
    scopes: &[oj_compiler::glob::GlobMatcher],
    closure: &std::collections::HashSet<PathBuf>,
    p: &Path,
) -> bool {
    if scopes.is_empty() || !p.is_file() {
        return false;
    }
    let real = std::fs::canonicalize(p).ok();
    if closure.contains(p) || real.as_ref().is_some_and(|r| closure.contains(r)) {
        return false;
    }
    let matched = |p: &Path| {
        let file = p.to_string_lossy().replace('\\', "/");
        scopes.iter().any(|g| g.is_match(&file))
    };
    matched(p) || real.is_some_and(|r| matched(&r))
}

/// The files the watcher knows, as chokidar's watched set: the boot scan
/// (`oj_server::watched_files`) plus every file an event has named since,
/// keyed by real path. Chokidar calls an event for a path it knows a change,
/// whatever the OS reported (an atomic save's rename onto a live file, an
/// unlink and recreate inside its atomic window, FSEvents' sticky created
/// flag), and an add for one it did not: a file renamed into place, or one
/// inside a directory that appeared (the shared watcher's synthesized
/// creates).
struct KnownFiles(std::collections::HashSet<PathBuf>);

impl KnownFiles {
    fn key(p: &Path) -> PathBuf {
        std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
    }

    /// The paths `ev` adds: a create or a rename's target the watcher did
    /// not know. Every file `ev` names is known from here on.
    fn adds(&mut self, ev: &notify::Event) -> Vec<PathBuf> {
        let adding = matches!(
            ev.kind,
            notify::EventKind::Create(_)
                | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
        );
        ev.paths
            .iter()
            .filter(|p| p.is_file() && self.0.insert(Self::key(p)) && adding)
            .cloned()
            .collect()
    }

    /// A settled batch's paths gone from disk are unknown again, as chokidar
    /// drops a path on unlink: recreating one later is an add.
    fn forget_gone(&mut self, paths: &std::collections::HashSet<PathBuf>) {
        for p in paths.iter().filter(|p| !p.exists()) {
            self.0.remove(p);
            if let Some(real) = p.parent().and_then(|d| std::fs::canonicalize(d).ok()) {
                if let Some(name) = p.file_name() {
                    self.0.remove(&real.join(name));
                }
            }
        }
    }
}

/// The files a stored generation's globs expand to today that its closure
/// could hold: the client bundle leaves the generated route tree and its own
/// directory (`cache`) out of closure.json, so a glob reaching them has not
/// grown, or every restore of such an app would miss.
fn restorable_glob_members(scopes: &Path, cache: &Path) -> Vec<PathBuf> {
    load_glob_calls(scopes)
        .iter()
        .flat_map(oj_compiler::glob::GlobCall::files)
        .filter(|f| !f.starts_with(cache) && !is_route_tree_gen(f))
        .collect()
}

/// The client bundle's `routeTree.gen.[jt]sx?` closure filter.
fn is_route_tree_gen(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("routeTree.gen."))
        .is_some_and(|ext| matches!(ext, "js" | "jsx" | "ts" | "tsx"))
}

/// Whether a batch can change the client bundle: a path in the closure (the
/// watcher spelling, or its real path, since the bundler records resolved
/// paths), a new file a bundled glob matches, a failed previous bundle, or
/// no closure to consult. A new file anywhere else is ignored, as Vite
/// ignores adds its module graphs and glob importers never reach: a new
/// file that would shadow an existing import's resolution lands when that
/// importer changes or a bundle fails, there as here.
fn client_bundle_affected(state: &StartState, paths: &[PathBuf]) -> bool {
    if !state.bundle_ok.load(std::sync::atomic::Ordering::SeqCst) {
        return true;
    }
    let closure = state
        .client_closure
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if closure.is_empty() {
        return true;
    }
    if paths.iter().any(|p| {
        closure.contains(p) || std::fs::canonicalize(p).is_ok_and(|real| closure.contains(&real))
    }) {
        return true;
    }
    let scopes = state
        .client_glob_scopes
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    paths.iter().any(|p| added_to_glob(&scopes, &closure, p))
}

/// A batch's paths the client bundle reads: those in the closure (as it
/// spells them) and new files a bundled glob matches. Everything, when there
/// is no closure to consult. The bundler's engine rebuilds in full for one it
/// does not know as a module.
fn client_inputs(state: &StartState, paths: &[PathBuf]) -> Vec<PathBuf> {
    let closure = state
        .client_closure
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if closure.is_empty() || !state.bundle_ok.load(std::sync::atomic::Ordering::SeqCst) {
        return paths.to_vec();
    }
    let scopes = state
        .client_glob_scopes
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let mut inputs = Vec::new();
    for p in paths {
        if closure.contains(p) {
            inputs.push(p.clone());
        } else if let Some(real) = std::fs::canonicalize(p)
            .ok()
            .filter(|r| closure.contains(r))
        {
            inputs.push(real);
        } else if added_to_glob(&scopes, &closure, p) {
            inputs.push(p.clone());
        }
    }
    inputs
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

/// The regen outputs whose content moved past `seen` (the hashes last pushed
/// to the worker environments), as invalidate changes; `seen` is updated to
/// the current content.
fn changed_regen_outputs(
    files: &[PathBuf],
    seen: &mut std::collections::HashMap<PathBuf, Option<u64>>,
) -> Vec<(String, &'static str)> {
    files
        .iter()
        .filter(|f| {
            let current = file_content_hash(f);
            match seen.insert((*f).clone(), current) {
                Some(last) => current != last,
                // Never seen (a tsr.config change moved the tree): only a file
                // that exists is a change worth pushing.
                None => current.is_some(),
            }
        })
        .map(|f| (f.to_string_lossy().into_owned(), "update"))
        .collect()
}

fn spawn_start_watcher(
    root: PathBuf,
    cache: PathBuf,
    state: Arc<StartState>,
    rx: std::sync::mpsc::Receiver<notify::Event>,
) {
    let rt = tokio::runtime::Handle::current();
    let pending = Arc::clone(&state.rebundles);
    let wake = Arc::clone(&state.rebundle_wake);
    let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
    // The start entry's presence is a config-time input: upstream resolves
    // `#tanstack-start-entry` once in its plugin's `config` hook, and oj
    // probes it once per StartHost and per bundle run. A `src/start.*` that
    // appears or disappears mid-session would otherwise half-apply (the next
    // unrelated rebundle re-probes for the client while the SSR alias stays
    // fixed until restart), so a flip of the probed answer restarts the dev
    // server, as a config edit does. Edits to an existing entry are ordinary
    // module changes and never flip this.
    let watch_root = root.clone();
    let boot_entry = crate::start_host::app_start_entry(&watch_root);
    rt.spawn(refresh_worker(
        root.clone(),
        cache.clone(),
        Arc::clone(&state),
    ));
    rt.spawn(rebundle_worker(
        root,
        cache,
        Arc::clone(&state),
        Arc::clone(&pending),
        Arc::clone(&wake),
        Arc::clone(&shutdown),
    ));
    // Ends the rebundle worker when the watcher thread exits (the feed
    // disconnected), so it does not idle forever holding the state.
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
        use std::sync::mpsc::RecvTimeoutError;

        let _stop_worker = stop_worker;
        let mut batch = 0u64;
        // Attribute-only events (the atime update a read causes on Linux) are
        // not changes: the client rebundle reads every source file, which
        // otherwise looked like an edit of every source file and rebuilt again.
        let mut changes = oj_server::ContentChanges::new();
        // Scanned on this thread, so boot never waits on it: events queue on
        // the feed meanwhile. A file created while it runs can read as known,
        // a race chokidar's own initial scan has too.
        let mut known = KnownFiles(oj_server::watched_files(&watch_root));
        loop {
            let mut created: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            let mut paths: std::collections::HashSet<PathBuf> = match rx.recv() {
                Ok(ev) => {
                    created.extend(known.adds(&ev));
                    changes.changed_paths(&ev).into_iter().collect()
                }
                Err(_) => break,
            };
            // A request can arrive milliseconds after a save: invalidate the
            // worker environments on the first raw event, before the settle
            // window and the rebuild, so a fast follow-up document is not
            // rendered from stale modules. The settled batch is invalidated
            // again below; the host dedups by content identity, so the repeat
            // send only costs work when a write landed inside the settle
            // window and actually changed content. The OS watcher's own
            // delivery latency is the remaining, unclosable window. The
            // handle is kept: the host's reply says whether a worker graph
            // matched, and the dedup means the settled send alone can read
            // as a miss for a change this early send already carried.
            let early_invalidate = spawn_mw_invalidate(&rt, &state, &paths, &created);
            loop {
                match rx.recv_timeout(std::time::Duration::from_millis(50)) {
                    Ok(ev) => {
                        created.extend(known.adds(&ev));
                        paths.extend(changes.changed_paths(&ev));
                    }
                    Err(RecvTimeoutError::Timeout) => break,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            known.forget_gone(&paths);
            if paths.is_empty() {
                continue;
            }
            if paths.iter().any(|p| start_entry_candidate(p))
                && crate::start_host::app_start_entry(&watch_root) != boot_entry
            {
                oj_server::restart_process("the start entry changed");
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
            // The gate holds at the watcher event, as plain dev does
            // (watch.rs): the editor flushes right after its writes, usually
            // while the rebuild below still runs, and a hold taken only at
            // rebuild end would wait out the cap for a flush that already
            // happened, serving the previous bundle for minutes.
            if let Some(gate) = &state.gate {
                let batch_paths: Vec<PathBuf> = paths.iter().cloned().collect();
                gate.hold_reload(&batch_paths);
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
                run.invalidates.extend(early_invalidate);
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
        let regen_files = regen_output_files(&root, &cache);
        // The gate: a batch whose paths miss the last bundle's input closure
        // cannot change the client bundle (rollup watch semantics; Vite only
        // acts on files its module graphs know). Regen checks still run below
        // either way: a server fn can live in a file the client never
        // imports, since the resolver scans all of src.
        let needs_bundle = run.force || client_bundle_affected(&state, &paths);
        let inputs = client_inputs(&state, &paths);
        let pages_run_bundle = state.pages_run_bundle();
        let no_invalidates_sent = run.invalidates.is_empty();
        let client = {
            let (r, c, m) = (root.clone(), cache.clone(), state.mode.clone());
            let env = Arc::clone(&state.script_env);
            let bundler = Arc::clone(&state.bundler);
            let routes_prev = prev_routes.clone();
            let changed: Vec<PathBuf> = paths.clone();
            let generation = state.bundler.hot.generation();
            let deferral = Arc::clone(&state);
            // Every batch, bundled or not: a style edit the gate passes over
            // must still recompile at the next bundle.
            bundler.note_changes(&changed);
            tokio::task::spawn_blocking(move || {
                let routes_now = list_route_files(&r);
                let routes_changed = routes_now != routes_prev;
                if routes_changed {
                    let _ = generate_route_tree(&r, &c, &env);
                }
                let server_fn_changed = changed.iter().any(|p| {
                    let is_ts = p.extension().is_some_and(|e| e == "ts" || e == "tsx");
                    is_ts
                        && (!p.exists()
                            || std::fs::read_to_string(p)
                                .is_ok_and(|s| s.contains("createServerFn")))
                });
                if routes_changed || server_fn_changed {
                    let _ = generate_server_fn_resolver(&r, &c);
                }
                // A route-set change rewrites the generated tree, a bundled
                // input, even when the triggering paths missed the closure.
                // Nothing to bring current while no page runs the bundle; a
                // runner document that loaded it since (it marks the bundle
                // loaded before it looks for a deferral) still gets it.
                let wants = needs_bundle || routes_changed;
                let bundling = wants
                    && (pages_run_bundle || {
                        deferral
                            .bundle_deferred
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                        deferral
                            .bundle_loaded
                            .load(std::sync::atomic::Ordering::SeqCst)
                    });
                // Name what trips an actual rebundle: a feedback loop (a build
                // output echoing into the watcher) is invisible without the
                // paths, and only the first few are needed to see it. A batch
                // the gate passes over stays silent, as Vite stays silent for
                // events its graphs never reach.
                if bundling {
                    let mut named: Vec<&std::path::Path> = changed
                        .iter()
                        .filter(|p| watch_relevant(p))
                        .map(|p| p.as_path())
                        .collect();
                    named.sort();
                    let shown = named
                        .iter()
                        .take(3)
                        .map(|p| p.display().to_string())
                        .collect::<Vec<_>>()
                        .join(", ");
                    let more = named.len().saturating_sub(3);
                    if more > 0 {
                        println!("  oj start: rebuilding for {shown} (+{more} more)");
                    } else {
                        println!("  oj start: rebuilding for {shown}");
                    }
                }
                let outcome = match bundling.then(|| bundler.bundle(&inputs, false)) {
                    None => ClientRun::Skipped,
                    Some(Ok(true)) => ClientRun::Hot,
                    Some(Ok(false)) => match pinned_output(&r, &c, &m) {
                        Some(pinned) => ClientRun::Fresh(pinned),
                        None => ClientRun::Failed("the bundle left no output to serve".into()),
                    },
                    Some(Err(e)) => ClientRun::Failed(format!("{e:#}")),
                };
                (routes_now, server_fn_changed, bundling, outcome, generation)
            })
        };
        let side = async {
            // A reload onto stale worker modules would re-render old content:
            // this run's settled invalidates complete before the signal, and
            // their replies say whether any worker graph served a change (a
            // failed send reads as yes).
            let mut hit = false;
            for handle in run.invalidates {
                hit |= handle.await.unwrap_or(true);
            }
            hit
        };
        let (client, worker_hit) = tokio::join!(client, side);
        let (mut bundled, mut server_fn_changed) = (false, false);
        let mut failure: Option<String> = None;
        let mut hot = false;
        let mut fresh = false;
        if let Ok((routes_now, fn_changed, bundling, outcome, generation)) = client {
            prev_routes = routes_now;
            server_fn_changed = fn_changed;
            bundled = bundling;
            if bundling {
                if let ClientRun::Failed(reason) = &outcome {
                    failure = Some(reason.clone());
                }
                state
                    .bundle_ok
                    .store(failure.is_none(), std::sync::atomic::Ordering::SeqCst);
                let frame = failure.as_deref().map(|reason| {
                    client_error_frame(&cache).unwrap_or_else(|| bundler_failure_frame(reason))
                });
                if let Some(frame) = &frame {
                    let _ = state.error_tx.send(frame.clone());
                }
                *state
                    .build_error
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = frame;
                match outcome {
                    ClientRun::Fresh(pinned) => {
                        install_bundle(&state, &cache, pinned);
                        state
                            .bundle_deferred
                            .store(false, std::sync::atomic::Ordering::SeqCst);
                        state.bundler.hot.served_output_of(generation);
                        fresh = true;
                    }
                    ClientRun::Hot => {
                        hot = true;
                        state.refresh_wake.notify_one();
                    }
                    ClientRun::Skipped | ClientRun::Failed(_) => {}
                }
            }
        }
        // Regenerated outputs are edits the watcher never forwards: push the
        // ones whose content moved past what the worker environments last saw
        // (whether this run's regen or the framework plugin's own generator
        // rewrote them), and re-arm the lazy runner — a fallback request that
        // consumed the dirty flag mid-regen reloaded onto the old generated
        // files. Both before this run's browser reload.
        let regen_changed = {
            let mut seen = state
                .regen_hashes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            changed_regen_outputs(&regen_files, &mut seen)
        };
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
                let _ = oj_server::notify_plugin_mw_invalidate(port, &regen_changed).await;
            }
        }
        // The non-lazy runner reloads strictly after the regen completed (its
        // re-import must see the new generated files, as the inline loop
        // guaranteed) and before the browser reload. A batch that bundled or
        // regenerated reloads it whole, as before; one that did neither only
        // revalidates, which drops exactly the stale records (and answers
        // whether the engine served any of the changed files at all).
        let engine_hit = if !state.lazy_runner() {
            if bundled || server_fn_changed || !regen_changed.is_empty() {
                state.engine.reload().await;
                true
            } else {
                state.engine.revalidate_for(&paths).await
            }
        } else if no_invalidates_sent {
            // Lazy with the middleware down: the fallback runner is serving,
            // so its graph is the only reload signal left.
            state.engine.revalidate_for(&paths).await
        } else {
            false
        };
        // A page that asked for a reload gets it with any fresh output.
        let forced = fresh
            && state
                .reload_wanted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .is_some();
        let server_signals =
            worker_hit || engine_hit || paths.iter().any(|p| state.ssr.graph_knows_file(p));
        let unbundled = !pages_run_bundle && !bundled;
        let reload = reloads_browser(RunSignals {
            unbundled,
            forced,
            bundle_failed: failure.is_some(),
            server_fn_changed,
            regen_changed: !regen_changed.is_empty(),
            hot,
            bundled,
            server_signals,
        });
        // Tailwind generates rules from the sources it scans: an update
        // applied in place re-links the sheets instead of reloading. A sheet
        // the update imported for the first time is linked in the pages.
        let added = if hot {
            hot_styles_added(&cache)
        } else {
            Vec::new()
        };
        if hot && !reload && (state.css_host.is_some() || !added.is_empty()) {
            state
                .bundler
                .hot
                .refresh_styles(state.css_host.is_some(), &added);
        }
        // The hold was taken at the watcher event; by now the editor's flush
        // may have consumed it, in which case this run's reload goes out with
        // the fresh bundle instead of waiting for a flush that already came.
        let held = state.gate.as_ref().is_some_and(|g| g.reload_is_held());
        if reload && !held {
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
        for done in run.done {
            let _ = done.send(());
        }
        if let Some(reason) = &failure {
            eprintln!("  oj start: client bundle failed, the pages keep the last bundle: {reason}");
        } else if hot && !reload {
            println!("  oj start: hot updated");
        } else if unbundled && !reload {
            println!("  oj start: client updates left to the page's module graph, no rebundle");
        } else if !reload {
            println!("  oj start: change outside every served graph, no rebundle, no reload");
        } else if held {
            println!(
                "  oj start: {}, reload held for the editor's flush",
                if bundled { "rebuilt" } else { "updated" }
            );
        } else {
            println!(
                "  oj start: {}, reloading",
                if bundled { "rebuilt" } else { "updated" }
            );
        }
    }
}

/// What a rebundle run saw, for the browser reload decision.
#[derive(Clone, Copy, Default)]
struct RunSignals {
    /// No page runs the Start bundle: they run the client module graph.
    unbundled: bool,
    /// A page asked for a reload, and the run has fresh output for it.
    forced: bool,
    bundle_failed: bool,
    server_fn_changed: bool,
    regen_changed: bool,
    /// Every change went out to the pages as a hot update.
    hot: bool,
    bundled: bool,
    /// A worker environment, the engine or the generic pipeline served a
    /// changed module.
    server_signals: bool,
}

/// Reload the browser only when something it can observe changed: the
/// bundle, a regen output, a worker-served module, an engine-served one, or a
/// module the generic pipeline served (a dev worker script). A change outside
/// every graph (a README, a config of some other tool) reloads nothing, as in
/// Vite.
/// A failed bundle reloads nothing: the page keeps what it runs under the
/// overlay (Vite's bundled dev), and the fixed bundle reloads it. A bundle
/// whose changes all went out as hot updates reloads only for what the pages
/// cannot take in place (a server-fn or route-set change). The server render
/// of the same modules is fresh for the next document, as Vite leaves SSR to
/// the next request once client HMR applied the edit.
/// With no page on the bundle (worker-rendered documents), the pages run the
/// client module graph, whose own HMR takes each edit in place or reloads, as
/// in Vite; a worker environment's update never reaches the page there
/// either. Only the regen outputs, which the watcher never forwards to that
/// HMR, still reload.
fn reloads_browser(s: RunSignals) -> bool {
    if s.unbundled {
        return s.forced || s.regen_changed;
    }
    s.forced
        || !s.bundle_failed
            && (s.server_fn_changed || s.regen_changed || !s.hot && (s.bundled || s.server_signals))
}

/// What a rebundle did with the client.
enum ClientRun {
    /// The batch missed the client's inputs.
    Skipped,
    /// With the bundler's error.
    Failed(String),
    /// Every change went out as a hot update; the output is the refresher's.
    Hot,
    Fresh(oj_cache::start_bundle::PinnedBundle),
}

/// The output a successful bundle wrote, cached for the next boot.
fn pinned_output(
    root: &Path,
    cache: &Path,
    mode: &str,
) -> Option<oj_cache::start_bundle::PinnedBundle> {
    match start_bundle_store(root, mode).persist(cache) {
        Some((_, pinned)) => Some(pinned),
        None => oj_cache::start_bundle::PinnedBundle::from_build_dir(cache),
    }
}

/// Serve a fresh client output, with the inputs it was built from.
fn install_bundle(state: &StartState, cache: &Path, pinned: oj_cache::start_bundle::PinnedBundle) {
    let pinned = Arc::new(pinned);
    state.gzip.retain(|h| pinned.has_hash(h));
    *state.bundle.write().unwrap() = pinned;
    *state
        .client_closure
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = load_client_closure(cache);
    *state
        .client_glob_scopes
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = load_client_glob_scopes(cache);
}

/// How long edits must pause before the refresher writes the client output:
/// on a large app that takes about a second, and the engine queues the next
/// edit's patch behind it, so it waits out a typing cadence. A document or a
/// page's reload needs it at once and skips the wait.
const REFRESH_PAUSE: std::time::Duration = std::time::Duration::from_secs(2);

/// Brings the client output current after hot updates, once edits pause or
/// as soon as a document or a page's reload needs it.
async fn refresh_worker(root: PathBuf, cache: PathBuf, state: Arc<StartState>) {
    use std::sync::atomic::Ordering::SeqCst;
    loop {
        state.refresh_wake.notified().await;
        while !state.urgent.swap(false, SeqCst) {
            let patches = state.bundler.hot.generation();
            tokio::select! {
                _ = tokio::time::sleep(REFRESH_PAUSE) => {}
                _ = state.refresh_wake.notified() => continue,
            }
            if state.bundler.hot.generation() == patches {
                break;
            }
        }
        let generation = state.bundler.hot.generation();
        let started = state.refreshes.fetch_add(1, SeqCst) + 1;
        let (r, c, m) = (root.clone(), cache.clone(), state.mode.clone());
        let bundler = Arc::clone(&state.bundler);
        let pinned = tokio::task::spawn_blocking(move || {
            bundler.bundle(&[], true)?;
            pinned_output(&r, &c, &m).ok_or_else(|| anyhow::anyhow!("no client chunk index"))
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|r| r);
        match pinned {
            Ok(pinned) => install_bundle(&state, &cache, pinned),
            // The rebundle that fixes it reports the error to the pages;
            // documents stop waiting either way.
            Err(e) => println!("  oj start: client output refresh failed: {e}"),
        }
        state.bundler.hot.served_output_of(generation);
        let wanted = state
            .reload_wanted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take_if(|mark| *mark < started)
            .is_some();
        if wanted {
            let _ = state.reload_tx.send(());
        }
    }
}

/// The config and app env a Start command's scripts are built from, resolved
/// once per command.
struct ScriptInputs {
    config: oj_config::OjConfig,
    app_env: oj_env::AppEnv,
}

fn script_inputs(
    env: &oj_env::Env,
    root: &Path,
    config_file: Option<&Path>,
    command: &str,
    mode: &str,
) -> anyhow::Result<ScriptInputs> {
    let mut config = oj_config::load(root).unwrap_or_default();
    oj_server::plugins::adopt_vite_config_values(&mut config, root, config_file, command, mode)
        .map_err(|e| anyhow::anyhow!(e))?;
    // `.env` files come from `envDir` and only `envPrefix` variables are exposed
    // (Vite's loadEnv), not the root and `VITE_` unconditionally.
    let env_dir = match config.env_dir.as_deref() {
        Some(d) => root.join(d),
        None => root.to_path_buf(),
    };
    let app_env = oj_env::AppEnv::resolve(
        env,
        &env_dir,
        mode,
        oj_config::env_prefixes(&config),
        "development",
    );
    Ok(ScriptInputs { config, app_env })
}

/// The `.env` vars with an `envPrefix` that the shell does not already set
/// (the scripts inherit the shell's own value).
fn prefixed_dotenv_vars(env: &oj_env::Env, app_env: &oj_env::AppEnv) -> Vec<(String, String)> {
    app_env
        .loaded()
        .iter()
        .filter(|(k, _)| app_env.has_prefix(k) && env.var_os(k).is_none())
        .cloned()
        .collect()
}

/// The `.env` vars the Start scripts (route codegen, the SSR runtime's
/// `process.env`) see: the mode's vars with an `envPrefix`, the shell winning,
/// as in Vite's loadEnv.
fn start_script_vars(env: &oj_env::Env, inputs: &ScriptInputs) -> Vec<(String, String)> {
    prefixed_dotenv_vars(env, &inputs.app_env)
}

/// [`start_script_vars`] for one command, resolved against the process snapshot.
#[cfg(test)]
fn start_script_env(
    root: &Path,
    config_file: Option<&Path>,
    command: &str,
    mode: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let env = oj_env::get();
    Ok(start_script_vars(
        env,
        &script_inputs(env, root, config_file, command, mode)?,
    ))
}

/// The app's own TanStack Start server entry (`tanstackStart({ server: { entry } })`),
/// when it configures one. Start's plugin publishes the resolved entry paths as
/// `resolve.alias` entries (`virtual:tanstack-start-server-entry` -> file), which
/// Vite's dev server imports as the SSR handler; oj reads the same alias. Only an
/// app file counts: unconfigured, the alias points at Start's own package entry,
/// which oj replaces with its runner entry as before.
pub(crate) fn configured_start_server_entry(
    config: &oj_config::OjConfig,
    root: &Path,
) -> Option<PathBuf> {
    let target = oj_config::resolve_alias(config, "ssr")
        .into_iter()
        .find(|(find, _)| find == "virtual:tanstack-start-server-entry")
        .map(|(_, replacement)| replacement)?;
    let path = PathBuf::from(target.split('?').next().unwrap_or(&target));
    let path = if path.is_absolute() {
        path
    } else {
        root.join(path)
    };
    let real = path.canonicalize().ok()?;
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    if !real.starts_with(&root)
        || real.components().any(|c| c.as_os_str() == "node_modules")
        || !real.is_file()
    {
        return None;
    }
    Some(real)
}

pub async fn start_build(
    root: PathBuf,
    config_file: Option<PathBuf>,
    mode: &str,
    out: Option<PathBuf>,
) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("app root not found: {}: {e}", root.display()))?;
    let cache = oj_cache::cache_root(&root).join("start");
    oj_server::prepare_cache_root(&root);
    oj_server::write_start_assets(&cache)?;
    {
        let gen_env = script_env(
            oj_env::get(),
            &root,
            config_file.as_deref(),
            "serve",
            "development",
        )?;
        let (r, c) = (root.clone(), cache.clone());
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            generate_route_tree(&r, &c, &gen_env)?;
            generate_server_fn_resolver(&r, &c)
        })
        .await??;
    }
    // The build options Vite resolves for a Start app too: `--out`/`build.outDir`,
    // `base`, `build.sourcemap`, `build.minify`.
    let mut config = oj_config::load_with(&root, "build", mode).unwrap_or_default();
    oj_server::plugins::adopt_vite_config_values(
        &mut config,
        &root,
        config_file.as_deref(),
        "build",
        mode,
    )
    .map_err(|e| anyhow::anyhow!(e))?;
    let build_cfg = config.build.clone().unwrap_or_default();
    let out = out
        .or_else(|| build_cfg.out_dir.as_ref().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("dist"));
    let out_dir = if out.is_absolute() {
        out
    } else {
        root.join(out)
    };
    if out_dir.canonicalize().ok().as_deref() == Some(root.as_path()) {
        anyhow::bail!(
            "build.outDir {} is the project root; refusing to empty it. Point outDir at a subdirectory.",
            out_dir.display()
        );
    }
    // Vite's rule: the shell's NODE_ENV wins, else `.env[.mode]`
    // NODE_ENV=development makes a development build, else production. The
    // shell's NODE_ENV comes from the startup snapshot, so nothing an engine
    // writes process-wide can change the build's env rule.
    let env_dir = match config.env_dir.as_deref() {
        Some(d) => root.join(d),
        None => root.to_path_buf(),
    };
    let app_env = oj_env::AppEnv::resolve(
        oj_env::get(),
        &env_dir,
        mode,
        oj_config::env_prefixes(&config),
        "production",
    );
    let output = crate::start_bundle::prod::BuildOutput {
        out_dir: out_dir.clone(),
        sourcemap: oj_config::build_sourcemap(&config),
        minify: oj_config::build_minify(&config),
        prerender: build_cfg.prerender.clone().unwrap_or_default(),
    };
    let start = crate::start_bundle::StartConfig {
        base: crate::start_bundle::normalize_base(config.base.as_deref()),
        root: root.clone(),
        cache: cache.clone(),
        command: crate::start_bundle::Command::Build,
        mode: mode.to_string(),
        config,
        app_env,
    };
    crate::start_bundle::prod::build(start, config_file, output).await?;
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

/// `script`: the codegen script whose bytes key its outputs (the route tree's
/// generate.mjs); a generator built into oj is keyed by oj's version alone.
fn codegen_store(
    root: &Path,
    cache: &Path,
    kind: &str,
    script: Option<&str>,
    marker: Option<&str>,
) -> oj_cache::start_codegen::StartCodegenStore {
    let mut extra = script
        .and_then(|s| std::fs::read(cache.join(s)).ok())
        .unwrap_or_default();
    // The route tree's footer names the start entry, so which file (if any)
    // resolves keys the output; its content does not.
    if kind == "route-tree" {
        if let Some(entry) = crate::start_host::app_start_entry(root) {
            extra.extend_from_slice(entry.to_string_lossy().as_bytes());
        }
        extra.push(0);
    }
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

fn generate_route_tree(
    root: &Path,
    cache: &Path,
    script_env: &[(String, String)],
) -> anyhow::Result<()> {
    let store = codegen_store(root, cache, "route-tree", Some("generate.mjs"), None);
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
    run_script_process(
        root,
        &cache.join("generate.mjs"),
        script_env,
        "route tree generation",
    )?;
    let inputs: Vec<PathBuf> = list_route_files(root).into_iter().collect();
    store.persist(&inputs, &outputs);
    Ok(())
}

fn generate_server_fn_resolver(root: &Path, cache: &Path) -> anyhow::Result<()> {
    let store = codegen_store(root, cache, "server-fn", None, Some("createServerFn"));
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
    let count = crate::start_bundle::write_server_fn_resolver(root, cache)
        .map_err(|e| anyhow::anyhow!("server-fn resolver: {e}"))?;
    eprintln!(
        "{}: server-fn resolver ({count} function(s))",
        oj_server::oj_brand()
    );
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

/// Runs a Start codegen script in a fresh `oj start-script` child process
/// (the embedded engine, one process per run), replacing the long-lived
/// ScriptEngine the dev server briefly used. In-process, rolldown's binding
/// keeps native memory resident after every `build()` (its mimalloc holds
/// finished builds' pages, rolldown/rolldown#10985) on top of the dev server
/// itself; only process exit releases all of it, as the retired one-shot
/// `node` spawns did. The client bundle is the exception: it runs on one
/// long-lived [`ClientBundler`] for plugin state, which frees each build's
/// outputs and purges on free to stay near one build's footprint. The env
/// pairs travel on stdin (they used to be spawn env; argv would print values
/// in `ps`).
fn run_script_process(
    root: &Path,
    script: &Path,
    env: &[(String, String)],
    what: &str,
) -> anyhow::Result<()> {
    let exe = std::env::current_exe().map_err(|e| anyhow::anyhow!("oj executable path: {e}"))?;
    let mut child = std::process::Command::new(exe)
        .arg("start-script")
        .env("OJ_PARENT_PID", std::process::id().to_string())
        .arg(script)
        .arg("--root")
        .arg(root)
        .current_dir(root)
        .env(
            oj_server::plugins::PARENT_PID_ENV,
            std::process::id().to_string(),
        )
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| anyhow::anyhow!("could not run {what}: {e}"))?;
    oj_server::boot_phase(&format!("{what}: child {} spawned", child.id()));
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("piped stdin");
        stdin
            .write_all(serde_json::to_string(env)?.as_bytes())
            .map_err(|e| anyhow::anyhow!("could not send {what} env: {e}"))?;
        // Dropping closes the pipe; the child's read_to_string completes.
    }
    let status = child
        .wait()
        .map_err(|e| anyhow::anyhow!("could not wait for {what}: {e}"))?;
    oj_server::boot_phase(&format!("{what}: child exited ({status})"));
    if !status.success() {
        anyhow::bail!("{what} failed");
    }
    Ok(())
}

/// Where the client bundle records its failure (Vite's
/// prepareError shape); absent after a success.
const CLIENT_ERROR_FILE: &str = "client-error.json";
/// Where the client bundle records whether a rebundle's changes all reached
/// the pages as hot updates.
const CLIENT_HMR_FILE: &str = "client-hmr.json";

/// The client bundle's recorded failure as the `{type:"error", err}` frame
/// Vite's bundled dev sends its clients; None when no bundle failed.
fn client_error_frame(cache: &Path) -> Option<String> {
    let bytes = std::fs::read(cache.join(CLIENT_ERROR_FILE)).ok()?;
    let err: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    Some(serde_json::json!({ "type": "error", "err": err }).to_string())
}

/// The error frame for a client bundle that failed before the app's build
/// ran (rolldown failing to resolve or load, the bundler exiting), which
/// leaves no client-error.json: the overlay names the bundler's error.
fn bundler_failure_frame(reason: &str) -> String {
    // The bundler's reply is one line, its stack frames joined by spaces.
    let message = reason
        .find("    at ")
        .map_or(reason, |at| &reason[..at])
        .trim_end();
    serde_json::json!({ "type": "error", "err": { "message": message, "stack": reason } })
        .to_string()
}

/// A file a compiled stylesheet can read: a sheet in any language oj
/// compiles (a partial or a `composes` source included) or the PostCSS or
/// Tailwind config.
pub(crate) fn is_style_input(path: &Path) -> bool {
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if matches!(
        ext,
        "css" | "scss" | "sass" | "less" | "styl" | "stylus" | "pcss" | "postcss" | "sss"
    ) {
        return true;
    }
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    name.starts_with("postcss.config.")
        || name.starts_with(".postcssrc")
        || name.starts_with("tailwind.config.")
}

/// The dev session's client bundler (start_bundle::dev), in process on oj's
/// rolldown.
pub(crate) type ClientBundler = crate::start_bundle::dev::DevBundler;

/// The stylesheets the last bundle's hot updates imported for the first time
/// (client-hmr.json `css`).
fn hot_styles_added(cache: &Path) -> Vec<String> {
    std::fs::read_to_string(cache.join(CLIENT_HMR_FILE))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| serde_json::from_value(v.get("css")?.clone()).ok())
        .unwrap_or_default()
}

fn start_bundle_store(root: &Path, mode: &str) -> oj_cache::start_bundle::StartBundleStore {
    oj_cache::start_bundle::StartBundleStore::for_mode(
        root,
        env!("CARGO_PKG_VERSION"),
        oj_cache::integrity::VerifyMode::Standard,
        mode,
    )
}

fn bundle_client_entry_cached(
    bundler: &ClientBundler,
    cache: &Path,
    mode: &str,
) -> anyhow::Result<oj_cache::start_bundle::PinnedBundle> {
    let root = bundler.root();
    let _ = std::fs::remove_file(cache.join(CLIENT_ERROR_FILE));
    let store = start_bundle_store(root, mode);
    match store.restore(cache, |scopes| restorable_glob_members(scopes, cache)) {
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
    bundler.bundle(&[], false)?;
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

// Client module count written by the client bundle, for update/boot narration.
fn client_module_count(cache: &Path) -> usize {
    std::fs::read_to_string(cache.join("client-entry.modules"))
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(0)
}

/// Vite's rule for the dev server too: the shell's NODE_ENV wins (so
/// `NODE_ENV=production oj dev` is PROD, as in Vite and oj's generic server),
/// else `.env[.mode]` NODE_ENV=development, else development.
/// The environment the one-shot scripts receive as their `run(env)` argument:
/// the same values `node` used to get as spawn env. Resolved once per command;
/// `.env` edits restart the process, so it never goes stale.
fn script_env(
    env: &oj_env::Env,
    root: &Path,
    config_file: Option<&Path>,
    command: &str,
    mode: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let inputs = script_inputs(env, root, config_file, command, mode)?;
    let mut vars = vec![
        (
            "OJ_APP_ROOT".to_string(),
            root.to_string_lossy().into_owned(),
        ),
        (
            "OJ_CACHE_ROOT".to_string(),
            oj_cache::cache_root(root).to_string_lossy().into_owned(),
        ),
        (
            "NODE_ENV".to_string(),
            inputs.app_env.node_env().to_string(),
        ),
    ];
    vars.extend(start_script_vars(env, &inputs));
    Ok(vars)
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

async fn compile_css(
    host: &Arc<oj_server::css_engine::CssEngine>,
    source: &str,
    path: &Path,
) -> Option<String> {
    host.compile_path(source, path, serde_json::Value::Null, true)
        .await
        .ok()
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
    // Only oj's own `/__` endpoints are the dev server's: Vite reserves no
    // `/__` prefix, so an app route or a plugin middleware page under it
    // goes through the middleware and SSR like any document.
    if path.starts_with("/@") || oj_server::is_dev_server_path(path) {
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
                // Subscribed before the replay read: an error landing between
                // the two arrives twice, never not at all.
                let errors = state.error_tx.subscribe();
                let replay = state
                    .build_error
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                // Hot updates reach a page unless the HMR gate holds updates
                // for the editor's flush: then each edit reloads, as before.
                let hot = state.gate.is_none().then(|| {
                    let weak = Arc::downgrade(&state);
                    HotPage {
                        bundler: Arc::clone(&state.bundler),
                        frames: state.bundler.hot.subscribe(),
                        reload_needed: Arc::new(move || {
                            if let Some(state) = weak.upgrade() {
                                state.page_needs_reload();
                            }
                        }),
                    }
                });
                // Echo the `oj-hmr` subprotocol when a client offers it: the
                // positive identity signal for readiness probes, exactly as
                // Vite's `vite-hmr` echo is theirs. oj's own client offers no
                // subprotocol and is untouched (no match, no header).
                let events = PageEvents {
                    frames: state.ws_tx.subscribe(),
                    to_plugins: Arc::clone(&state.to_plugins),
                };
                ws.protocols(["oj-hmr"]).on_upgrade(move |socket| {
                    start_hmr_socket(socket, rx, errors, replay, hot, Some(events))
                })
            }
            Err(e) => e.into_response(),
        };
    }
    if req.uri().path() == "/@oj-start/live-reload.js" {
        return match tokio::fs::read_to_string(&state.live_reload).await {
            Ok(js) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                render_live_reload(&js, state.hmr_overlay),
            )
                .into_response(),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("oj start: live-reload client: {e}"),
            )
                .into_response(),
        };
    }
    // The overlay element the live-reload client shows build errors in,
    // written beside it by write_start_assets.
    if req.uri().path() == "/@oj-start/error-overlay.js" {
        let overlay = state.live_reload.with_file_name("error-overlay.js");
        return serve_js(&overlay, "error overlay").await;
    }
    if let Some(name) = req
        .uri()
        .path()
        .strip_prefix(crate::start_hot::PATCH_PREFIX)
    {
        return state.bundler.hot.serve(name);
    }
    if let Some(rest) = req.uri().path().strip_prefix("/@oj-start/fs/") {
        return serve_fs_asset(&state, &format!("/{rest}")).await;
    }
    if let Some(name) = req.uri().path().strip_prefix("/@oj-start/") {
        return serve_client_chunk(&state, req.headers(), name).await;
    }
    if req.uri().path().starts_with("/_serverFn/") {
        return forward_with_body(&state, req).await;
    }
    // A WebSocket upgrade is never a document: the inner app owns them all
    // (vite-hmr endpoint, `ws: true` proxies, plugin `upgrade` listeners).
    if oj_server::is_websocket_upgrade(req.headers()) {
        return next.run(req).await;
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

/// A page's end of the hot updates: its patches arrive here, and what it
/// says (its runtime id, a delivered patch, a reload it needs) goes to the
/// bundler.
struct HotPage {
    bundler: Arc<ClientBundler>,
    frames: broadcast::Receiver<Arc<crate::start_hot::Frame>>,
    reload_needed: Arc<dyn Fn() + Send + Sync>,
}

type ToPlugins = Arc<dyn Fn(&str, &str) + Send + Sync>;

/// The custom events Vite's ws server carries both ways: a plugin's
/// `server.ws.send` (on the editor channel) to the page, the page's
/// `import.meta.hot.send` to the plugins.
struct PageEvents {
    frames: broadcast::Receiver<String>,
    to_plugins: ToPlugins,
}

/// A plugin's frame on the editor channel the page should see: only custom
/// events (the channel also carries the plain dev path's own HMR frames).
fn page_custom_frame(text: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(text)
        .is_ok_and(|v| v["type"] == "custom" && v["event"].is_string())
}

async fn start_hmr_socket(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<()>,
    mut errors: broadcast::Receiver<String>,
    replay: Option<String>,
    mut hot: Option<HotPage>,
    mut events: Option<PageEvents>,
) {
    use crate::start_hot::PageMessage;
    if let Some(frame) = replay {
        if socket.send(Message::Text(frame.into())).await.is_err() {
            return;
        }
    }
    let mut page: Option<String> = None;
    loop {
        let frame = async {
            match hot.as_mut() {
                Some(h) => h.frames.recv().await,
                None => std::future::pending().await,
            }
        };
        let custom = async {
            match events.as_mut() {
                Some(e) => e.frames.recv().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            custom = custom => match custom {
                Ok(text) if page_custom_frame(&text) => {
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                // Events, not state: a lagged page skips them, as oj_server's
                // HMR socket does.
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => events = None,
            },
            error = errors.recv() => match error {
                Ok(frame) => {
                    if socket.send(Message::Text(frame.into())).await.is_err() {
                        break;
                    }
                }
                // A lagged receiver missed older failures; the newest one
                // still arrives (or the reload that clears them).
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            reload = rx.recv() => match reload {
                Err(broadcast::error::RecvError::Closed) => break,
                // A lagged receiver only missed reload pings; one reload
                // catches the client up (Vite's per-socket queue never drops
                // a reload, so neither may oj). The general HMR handler in
                // oj_server carries distinct frames and skips instead.
                Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => {
                    if socket.send(Message::Text("reload".into())).await.is_err() {
                        break;
                    }
                }
            },
            frame = frame => {
                let text = match frame {
                    Ok(f) if f.client.is_none() || f.client == page => f.text.clone(),
                    Ok(_) => continue,
                    // A missed patch leaves the page behind: reload it.
                    Err(broadcast::error::RecvError::Lagged(_)) => "reload".to_string(),
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if socket.send(Message::Text(text.into())).await.is_err() {
                    break;
                }
            },
            incoming = socket.recv() => match incoming {
                None | Some(Err(_)) => break,
                Some(Ok(Message::Close(_))) => {
                    let _ = socket.close().await;
                    break;
                }
                Some(Ok(Message::Text(text))) => {
                    let message = crate::start_hot::page_message(&text);
                    if let (Some(PageMessage::Custom { event, data }), Some(e)) = (&message, &events) {
                        (e.to_plugins)(event, data);
                        continue;
                    }
                    let Some(h) = hot.as_ref() else { continue };
                    match message {
                        Some(PageMessage::Client(id)) => {
                            if let Some(old) = page.replace(id.clone()) {
                                h.bundler.page_gone(&old);
                            }
                            h.bundler.page_connected(&id);
                        }
                        Some(PageMessage::Delivered(file)) => h.bundler.patch_delivered(&file),
                        Some(PageMessage::ReloadNeeded) => (h.reload_needed)(),
                        Some(PageMessage::Custom { .. }) | None => {}
                    }
                }
                Some(Ok(_)) => {}
            },
        }
    }
    if let (Some(h), Some(id)) = (hot.as_ref(), page) {
        h.bundler.page_gone(&id);
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
    // A document loads the client output: never one from before a hot update.
    state.client_output_fresh().await;
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
    ensure_bundle_current(state).await;
    ensure_runner_fresh(state).await;
    forward(
        &state.engine,
        "GET".into(),
        document_url(&raw),
        headers,
        None,
    )
    .await
}

/// The cap on a buffered request body (`OJ_START_MAX_BODY`, bytes). Bodies
/// reaching the engine are buffered whole because the call transport is
/// JSON-over-V8 (base64 in a string; op-streaming is the known follow-up), so
/// an unbounded body would balloon ~2.3x through base64 + JSON before the
/// handler sees it. The default is deliberately generous — the cap prevents
/// accidental OOM, it does not ration uploads.
fn start_max_body_bytes() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        oj_env::get()
            .knobs
            .start_max_body
            .unwrap_or(128 * 1024 * 1024)
    })
}

/// Reads a request body whole, bounded by `cap` ([`start_max_body_bytes`] at
/// the call sites). A body past the cap is a 413 naming the knob; a read
/// failure (a client aborting mid-upload) is a 400 — never an empty body
/// reaching the handler as if the client had sent one.
#[allow(clippy::result_large_err, reason = "the Err is the handler's response")]
async fn read_body_capped(body: axum::body::Body, cap: usize) -> Result<Option<Vec<u8>>, Response> {
    use tokio_stream::StreamExt;
    let mut stream = body.into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(frame) = stream.next().await {
        match frame {
            Ok(bytes) => {
                if buf.len().saturating_add(bytes.len()) > cap {
                    return Err((
                        StatusCode::PAYLOAD_TOO_LARGE,
                        format!(
                            "oj start: request body exceeds the {cap}-byte buffer cap; raise OJ_START_MAX_BODY to accept larger bodies"
                        ),
                    )
                        .into_response());
                }
                buf.extend_from_slice(&bytes);
            }
            Err(e) => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    format!("oj start: request body read failed: {e}"),
                )
                    .into_response())
            }
        }
    }
    Ok(if buf.is_empty() { None } else { Some(buf) })
}

async fn forward_with_body(state: &Arc<StartState>, req: Request) -> Response {
    // The middleware may pipe an unclaimed request on to the runner
    // (x-oj-forward-to), so start refreshing it — without blocking this
    // request, which the worker middleware typically serves itself.
    if state.lazy_runner() && state.runner_dirty.load(std::sync::atomic::Ordering::SeqCst) {
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
    // A configureServer middleware may claim the request first; since a
    // streamed body cannot be replayed, the plugin host pipes an unclaimed
    // request on to the engine's loopback shim (x-oj-forward-to) instead of
    // falling through. Without a middleware the body is read whole and handed
    // to the engine (the call transport is buffered).
    let Some(port) = state.plugin_serve.mw_port() else {
        let body = match read_body_capped(req.into_body(), start_max_body_bytes()).await {
            Ok(body) => body,
            Err(resp) => return resp,
        };
        return forward(&state.engine, method, url, &headers, body).await;
    };
    if let Ok(v) = header::HeaderValue::from_str(&state.loopback_port.to_string()) {
        headers.insert("x-oj-forward-to", v);
    }
    match oj_server::proxy_to_loopback_streaming(
        port,
        &method,
        &url,
        &headers,
        Some(req.into_body()),
    )
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

async fn serve_client_chunk(
    state: &StartState,
    req: &axum::http::HeaderMap,
    name: &str,
) -> Response {
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
    let expected = oj_cache::integrity::ExpectedFile {
        size: chunk.size,
        hash: chunk.hash.clone().unwrap_or_default(),
    };
    let path = chunk.path.clone();
    let what = name.clone();
    let read = move || async move {
        match tokio::task::spawn_blocking(move || {
            oj_cache::integrity::verified_read(
                &path,
                &expected,
                oj_cache::integrity::VerifyMode::Standard,
            )
        })
        .await
        {
            Ok(Ok(bytes)) => Ok(axum::body::Bytes::from(bytes)),
            Ok(Err(e)) => {
                eprintln!("oj start: chunk {what} failed verification ({e}); refusing to serve");
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("oj start: chunk {what}: {e}"),
                )
                    .into_response())
            }
            Err(_) => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                "oj start: chunk read task failed".to_string(),
            )
                .into_response()),
        }
    };
    let ext = name.rsplit('.').next().unwrap_or("");
    let mime = asset_mime(ext);
    // Every chunk but the entry is named by rolldown's `[name]-[hash]`.
    let immutable = !bundle.is_entry(&name);
    match chunk.hash.clone() {
        Some(hash) => {
            let chunk = Chunk {
                mime,
                hash: &hash,
                immutable,
            };
            respond(req, chunk, Some(&state.gzip), read).await
        }
        // A bundle the store could not persist carries no hash: read first,
        // hash the bytes, and keep no gzip body for it.
        None => {
            let bytes = match read().await {
                Ok(b) => b,
                Err(resp) => return resp,
            };
            let hash = blake3::hash(&bytes).to_hex().to_string();
            let chunk = Chunk {
                mime,
                hash: &hash,
                immutable,
            };
            respond(req, chunk, None, || async { Ok(bytes) }).await
        }
    }
}

/// The live-reload client with Vite's `__HMR_ENABLE_OVERLAY__` filled in.
fn render_live_reload(js: &str, overlay: bool) -> String {
    js.replace(
        "__HMR_ENABLE_OVERLAY__",
        if overlay { "true" } else { "false" },
    )
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

/// A stylesheet the dev client loads, when it is not served as written: a
/// CSS module or a preprocessor file is the sheet SSR compiles (preprocessed,
/// scoped under the names SSR renders and the bundle's class map carries),
/// and a Tailwind sheet is compiled after that, as the app's postcss plugins
/// run after Vite's preprocessors and postcss-modules.
async fn css_body(state: &StartState, file: &Path) -> Option<Result<String, String>> {
    let pipeline = oj_css::is_css_module(&file.to_string_lossy())
        || file.extension().and_then(|e| e.to_str()) != Some("css");
    let src = if pipeline {
        match state.ssr.style_sheet(file).await {
            Ok(m) => m.css,
            Err(e) => return Some(Err(e)),
        }
    } else {
        tokio::fs::read_to_string(file).await.ok()?
    };
    if let Some(host) = state.css_host.as_ref().filter(|_| needs_css_compile(&src)) {
        if let Some(css) = compile_css(host, &src, file).await {
            return Some(Ok(css));
        }
    }
    pipeline.then_some(Ok(src))
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
    if canon
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(oj_server::is_style_ext)
    {
        if let Some(body) = css_body(state, &canon).await {
            return match body {
                Ok(css) => (
                    [
                        (header::CONTENT_TYPE, "text/css; charset=utf-8"),
                        (header::CACHE_CONTROL, "no-cache"),
                    ],
                    css,
                )
                    .into_response(),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    format!("oj start: css: {e}"),
                )
                    .into_response(),
            };
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

/// Builds the engine request for one incoming request. The browser's Host is
/// taken from the `Host` header (first value, Node's rule); on the loopback
/// shim (`from_loopback`) the plugin middleware forwards the original Host as
/// `x-oj-host`, which wins there — and is dropped from a direct request so a
/// client cannot spoof it, exactly as `loopback_request_headers` did.
fn engine_request(
    method: String,
    url: String,
    req_headers: &header::HeaderMap,
    body: Option<Vec<u8>>,
    from_loopback: bool,
) -> StartRequest {
    let first_value = |name: &str| {
        req_headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string)
    };
    let host = from_loopback
        .then(|| first_value("x-oj-host"))
        .flatten()
        .or_else(|| first_value("host"))
        .unwrap_or_else(|| "localhost".to_string());
    let headers = req_headers
        .iter()
        .filter(|(name, _)| {
            let n = name.as_str();
            n != "host" && !oj_server::is_loopback_private(n)
        })
        .filter_map(|(name, value)| {
            Some((name.as_str().to_string(), value.to_str().ok()?.to_string()))
        })
        .collect();
    StartRequest {
        method,
        url,
        host,
        headers,
        body,
    }
}

fn engine_response(resp: StartResponse) -> Response {
    let mut out = Response::new(axum::body::Body::from(resp.body));
    *out.status_mut() =
        StatusCode::from_u16(resp.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let headers = out.headers_mut();
    for (name, value) in resp.headers {
        if let (Ok(n), Ok(v)) = (
            header::HeaderName::from_bytes(name.as_bytes()),
            header::HeaderValue::from_str(&value),
        ) {
            headers.append(n, v);
        }
    }
    for cookie in resp.set_cookies {
        if let Ok(v) = header::HeaderValue::from_str(&cookie) {
            headers.append(header::SET_COOKIE, v);
        }
    }
    out
}

async fn forward(
    engine: &StartEngine,
    method: String,
    url: String,
    req_headers: &header::HeaderMap,
    body: Option<Vec<u8>>,
) -> Response {
    match engine
        .handle(engine_request(method, url, req_headers, body, false))
        .await
    {
        Ok(resp) => inject_reload_client(engine_response(resp)),
        Err(e) => {
            // The runner's 500 page shape (Vite's errorMiddleware): message +
            // stack in an HTML page the overlay can read.
            let esc = |s: &str| {
                s.replace('&', "&amp;")
                    .replace('<', "&lt;")
                    .replace('>', "&gt;")
            };
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
                format!(
                    "<!DOCTYPE html>\n<html lang=\"en\"><head><meta charset=\"UTF-8\" /><title>Error</title></head>\n<body><h1>Internal Server Error</h1><pre>{}</pre></body></html>\n",
                    esc(&e)
                ),
            )
                .into_response()
        }
    }
}

/// The loopback shim standing in for the node runner's HTTP server: the
/// plugin middleware pipes unclaimed requests here (`x-oj-forward-to`), and
/// the shim calls the engine. Responses go back raw; the dev-server side
/// injects the reload client where needed.
async fn spawn_engine_loopback(engine: Arc<StartEngine>) -> anyhow::Result<u16> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let port = listener.local_addr()?.port();
    let app = axum::Router::new()
        .fallback(axum::routing::any(
            |State(engine): State<Arc<StartEngine>>, req: Request| async move {
                let method = req.method().to_string();
                let url = path_and_query(&req);
                let headers = req.headers().clone();
                let body = match read_body_capped(req.into_body(), start_max_body_bytes()).await {
                    Ok(body) => body,
                    Err(resp) => return resp,
                };
                match engine
                    .handle(engine_request(method, url, &headers, body, true))
                    .await
                {
                    Ok(resp) => engine_response(resp),
                    Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj start: {e}"))
                        .into_response(),
                }
            },
        ))
        .with_state(engine);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok(port)
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
    /// (the probe-window scale, matching the stall monitor's window): the
    /// stall monitor's evidence is clearable — one silent >window step of a
    /// healthy boot flips it and the next milestone clears it — so a clear
    /// landing within this window re-arms the hold instead of the release
    /// being one-shot. Host death and the init warn bound still release
    /// immediately.
    confirm: std::time::Duration,
}

/// Holds the Cloudflare prewarm until the plugin host's serve info is KNOWN,
/// releasing early only on wedge EVIDENCE: (a) the info arriving decides
/// (worker environments render → skip the prewarm; none → prewarm), (b) the
/// host dying (`host_gone`) releases immediately, while the init-failure
/// evidence watch — the host's stall monitor flipping it when a full probe
/// window passed with no init milestone — makes the release PENDING for one
/// more probe window (`PrewarmHold::confirm`): the evidence is clearable, so
/// a milestone landing within that window re-arms the hold instead of a
/// single silent step of a healthy boot one-shot-releasing a wasted prewarm,
/// and (c) the host's OWN init warn bound (spawn-anchored — never a fresh
/// full period measured from here) is the outer bound, so a boot slower than
/// anyone reasonably waits does not hold the warm engine hostage. A merely
/// slow, healthy boot — elapsed time short of that bound, with no evidence —
/// keeps holding: the earlier flat probe-scale timer prewarmed (wasted CPU,
/// competing with the boot) on every healthy boot slower than one window.
/// `None` means "not known: prewarm"; a LATE serve-info activation still supersedes.
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

    #[test]
    fn the_hmr_gate_turns_the_bundlers_hot_updates_off() {
        let bundler = ClientBundler::for_tests(Path::new("/app"));
        bundler.set_hot_enabled(true);
        assert!(bundler.hot_enabled());
        // A gated server: every edit reloads, so the bundle never goes hot.
        bundler.set_hot_enabled(false);
        assert!(!bundler.hot_enabled());
    }

    #[test]
    fn a_style_edit_moves_the_bundlers_style_epoch() {
        let bundler = ClientBundler::for_tests(Path::new("/app"));
        let epoch = || bundler.style_epoch();
        bundler.note_changes(&[PathBuf::from("/app/src/routes/about.tsx")]);
        assert_eq!(epoch(), 0, "a script edit keeps every style import cached");
        bundler.note_changes(&[
            PathBuf::from("/app/src/a.tsx"),
            PathBuf::from("/app/src/_vars.scss"),
        ]);
        assert_eq!(
            epoch(),
            1,
            "a partial edit recompiles the sheets that use it"
        );
        bundler.note_changes(&[PathBuf::from("/app/postcss.config.mjs")]);
        bundler.note_changes(&[PathBuf::from("/app/src/card.module.css")]);
        assert_eq!(epoch(), 3);
    }
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::{connect_async, tungstenite::Message as ClientMessage};

    // The watcher re-probes the start entry only for paths that can flip it:
    // the src/start.* ladder and any package.json (its `imports` may map the
    // entry). Ordinary sources never trigger the probe, so an edit to an existing src/start.ts stays an
    // ordinary rebundle (the probe's answer is unchanged either way).
    #[test]
    fn start_entry_candidates_are_the_probe_shapes() {
        let hit = |p: &str| start_entry_candidate(&PathBuf::from(p));
        assert!(hit("/app/src/start.ts"));
        assert!(hit("/app/src/start.jsx"));
        assert!(
            !hit("/app/src/start/index.ts"),
            "a directory index is not an entry"
        );
        assert!(hit("/app/package.json"));
        assert!(hit("/app/packages/dep/package.json"));
        assert!(!hit("/app/src/starter.ts"));
        assert!(!hit("/app/src/start-page.tsx"));
        assert!(
            !hit("/app/src/routes/start.ts"),
            "only directly under a src dir"
        );
        assert!(!hit("/app/src/main.tsx"));
        assert!(!hit("/app/other/start.ts"));
    }

    // The route tree's footer names the start entry, so a tree cached without
    // one must not restore once src/start.ts exists (and the reverse). Edits to
    // the entry's content keep the key. The server-fn resolver never reads it.
    #[test]
    fn route_tree_cache_keys_on_the_start_entry() {
        let root = std::env::temp_dir().join(format!("oj-treekey-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src/routes")).unwrap();
        let cache = root.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(root.join("src/routes/index.tsx"), "export const Route = 1;").unwrap();
        let tree = root.join("src/routeTree.gen.ts");
        std::fs::write(&tree, "// tree").unwrap();
        let inputs = vec![root.join("src/routes/index.tsx")];
        let outputs = [("routeTree.gen.ts", tree.as_path())];
        let store = |kind: &str| codegen_store(&root, &cache, kind, Some("generate.mjs"), None);

        store("route-tree").persist(&inputs, &outputs).unwrap();
        store("server-fn").persist(&inputs, &outputs).unwrap();
        assert!(store("route-tree").restore(&inputs, &outputs).is_ok());

        std::fs::write(root.join("src/start.ts"), "export const startInstance = 1;").unwrap();
        assert!(
            store("route-tree").restore(&inputs, &outputs).is_err(),
            "a created start entry re-keys the route tree"
        );
        assert!(
            store("server-fn").restore(&inputs, &outputs).is_ok(),
            "the server-fn resolver does not key on the start entry"
        );

        store("route-tree").persist(&inputs, &outputs).unwrap();
        std::fs::write(root.join("src/start.ts"), "export const startInstance = 2;").unwrap();
        assert!(
            store("route-tree").restore(&inputs, &outputs).is_ok(),
            "an edit to the entry keeps the key"
        );

        std::fs::remove_file(root.join("src/start.ts")).unwrap();
        assert!(
            store("route-tree").restore(&inputs, &outputs).is_ok(),
            "removing it restores the entry-less tree"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    struct HmrTestServer {
        url: String,
        reload_tx: broadcast::Sender<()>,
        error_tx: broadcast::Sender<String>,
        /// The editor channel a plugin's `server.ws.send` lands on.
        ws_tx: broadcast::Sender<String>,
        /// What pages sent the plugins (event, data).
        to_plugins: Arc<std::sync::Mutex<Vec<(String, String)>>>,
        task: tokio::task::JoinHandle<()>,
    }

    impl Drop for HmrTestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    type HmrTestState = (
        broadcast::Sender<()>,
        broadcast::Sender<String>,
        Option<String>,
        Option<Arc<tokio::sync::Notify>>,
        broadcast::Sender<String>,
        ToPlugins,
    );

    impl HmrTestServer {
        async fn start() -> Self {
            Self::start_with(None, None).await
        }

        async fn start_with_gate(gate: Option<Arc<tokio::sync::Notify>>) -> Self {
            Self::start_with(gate, None).await
        }

        async fn start_with(
            gate: Option<Arc<tokio::sync::Notify>>,
            replay: Option<String>,
        ) -> Self {
            let (reload_tx, _) = broadcast::channel(16);
            let (error_tx, _) = broadcast::channel(16);
            let (ws_tx, _) = broadcast::channel(16);
            let to_plugins = Arc::new(std::sync::Mutex::new(Vec::new()));
            let sink: ToPlugins = {
                let to_plugins = Arc::clone(&to_plugins);
                Arc::new(move |event: &str, data: &str| {
                    to_plugins
                        .lock()
                        .unwrap()
                        .push((event.to_string(), data.to_string()));
                })
            };
            let app =
                axum::Router::new()
                    .route(
                        "/@oj-start/hmr",
                        axum::routing::get(
                            |ws: WebSocketUpgrade,
                             State((tx, error_tx, replay, gate, ws_tx, sink)): State<
                                HmrTestState,
                            >| async move {
                                let rx = tx.subscribe();
                                let errors = error_tx.subscribe();
                                let events = PageEvents {
                                    frames: ws_tx.subscribe(),
                                    to_plugins: sink,
                                };
                                ws.on_upgrade(move |socket| async move {
                                    if let Some(gate) = gate {
                                        gate.notified().await;
                                    }
                                    start_hmr_socket(
                                        socket,
                                        rx,
                                        errors,
                                        replay,
                                        None,
                                        Some(events),
                                    )
                                    .await;
                                })
                            },
                        ),
                    )
                    .with_state((
                        reload_tx.clone(),
                        error_tx.clone(),
                        replay,
                        gate,
                        ws_tx.clone(),
                        sink,
                    ));
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("ws://{}/@oj-start/hmr", listener.local_addr().unwrap());
            let task = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            Self {
                url,
                reload_tx,
                error_tx,
                ws_tx,
                to_plugins,
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

    async fn next_text<S>(client: &mut S) -> String
    where
        S: futures_util::Stream<
                Item = Result<ClientMessage, tokio_tungstenite::tungstenite::Error>,
            > + Unpin,
    {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .expect("a frame arrives")
            .unwrap()
            .unwrap();
        match msg {
            ClientMessage::Text(t) => t.to_string(),
            other => panic!("expected a text frame, got {other:?}"),
        }
    }

    /// Vite's bundled dev error path: a page connecting while the client
    /// bundle is broken gets the stored failure first (lastBuildError replay),
    /// a later failure arrives as its own frame, and a reload still follows.
    #[tokio::test]
    async fn start_hmr_replays_the_build_error_then_forwards_new_ones() {
        let stored = r#"{"type":"error","err":{"message":"first"}}"#.to_string();
        let server = HmrTestServer::start_with(None, Some(stored.clone())).await;
        let (mut client, _) = connect_async(&server.url).await.unwrap();
        assert_eq!(
            next_text(&mut client).await,
            stored,
            "the stored error replays on connect"
        );
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while server.error_tx.receiver_count() != 1 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        let later = r#"{"type":"error","err":{"message":"second"}}"#.to_string();
        server.error_tx.send(later.clone()).unwrap();
        assert_eq!(next_text(&mut client).await, later);
        server.reload_tx.send(()).unwrap();
        assert_eq!(next_text(&mut client).await, "reload");
    }

    /// Vite's ws server carries custom events both ways: a plugin's
    /// `server.ws.send` reaches the page (only custom frames: the editor
    /// channel also carries the plain dev path's own HMR frames), and the
    /// page's `import.meta.hot.send` reaches `server.ws.on`. The HMR gate
    /// (no hot page here) leaves both on, as Vite's send never depends on it.
    #[tokio::test]
    async fn start_hmr_relays_custom_events_between_the_page_and_the_plugins() {
        let server = HmrTestServer::start().await;
        let (mut client, _) = connect_async(&server.url).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while server.ws_tx.receiver_count() != 1 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
        server
            .ws_tx
            .send(r#"{"type":"full-reload","path":"*"}"#.to_string())
            .unwrap();
        let custom = r#"{"type":"custom","event":"app:pong","data":{"n":2}}"#.to_string();
        server.ws_tx.send(custom.clone()).unwrap();
        assert_eq!(
            next_text(&mut client).await,
            custom,
            "the custom frame arrives, the full-reload frame before it never does"
        );

        client
            .send(ClientMessage::Text(
                r#"{"type":"custom","event":"app:ping","data":{"n":1}}"#.into(),
            ))
            .await
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while server.to_plugins.lock().unwrap().is_empty() {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the page's custom event reaches the plugins");
        assert_eq!(
            *server.to_plugins.lock().unwrap(),
            vec![("app:ping".to_string(), r#"{"n":1}"#.to_string())]
        );
    }

    #[test]
    fn live_reload_client_takes_the_hmr_overlay_setting_like_vites_client() {
        let dir = std::env::temp_dir().join(format!("oj-live-reload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        oj_server::write_start_assets(&dir).unwrap();
        let js = std::fs::read_to_string(dir.join("live-reload.js")).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let on = render_live_reload(&js, true);
        let off = render_live_reload(&js, false);
        assert!(on.contains("const enableOverlay = true;"), "{on}");
        assert!(off.contains("const enableOverlay = false;"), "{off}");
        assert!(!off.contains("__HMR_ENABLE_OVERLAY__"));
        // Off, an error frame is logged and the overlay never mounts.
        let show = &off[off.find("function showError").unwrap()..];
        let guard = show.find("if (!enableOverlay) return;").unwrap();
        assert!(guard < show.find("error-overlay.js").unwrap());
    }

    #[test]
    fn client_error_frame_wraps_the_recorded_failure_as_a_vite_error_payload() {
        let dir = std::env::temp_dir().join(format!("oj-client-error-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(client_error_frame(&dir), None, "no record, no frame");
        std::fs::write(
            dir.join(CLIENT_ERROR_FILE),
            r#"{"message":"boom","id":"/app/src/a.jsx","loc":{"line":1,"column":2}}"#,
        )
        .unwrap();
        let frame: serde_json::Value =
            serde_json::from_str(&client_error_frame(&dir).unwrap()).unwrap();
        assert_eq!(frame["type"], "error");
        assert_eq!(frame["err"]["message"], "boom");
        assert_eq!(frame["err"]["id"], "/app/src/a.jsx");
        assert_eq!(frame["err"]["loc"]["line"], 1);
        std::fs::write(dir.join(CLIENT_ERROR_FILE), "not json").unwrap();
        assert_eq!(client_error_frame(&dir), None, "a torn record is no frame");
    }

    #[test]
    fn a_bundler_that_fails_before_the_build_names_its_error_in_the_overlay() {
        let reason = "client entry bundling failed: Error: oj: cannot resolve 'rolldown' from /app     at resolveOjRolldown (file:///app/resolve-pkg.mjs:127:11)";
        let frame: serde_json::Value =
            serde_json::from_str(&bundler_failure_frame(reason)).unwrap();
        assert_eq!(frame["type"], "error");
        assert_eq!(
            frame["err"]["message"],
            "client entry bundling failed: Error: oj: cannot resolve 'rolldown' from /app"
        );
        assert_eq!(frame["err"]["stack"], reason);
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
    async fn start_hmr_reloads_lagged_clients_instead_of_dropping_them() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let server = HmrTestServer::start_with_gate(Some(gate.clone())).await;
        let (mut client, _) = connect_async(&server.url).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while server.reload_tx.receiver_count() != 1 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("client subscription never registered");
        // The handler is gated, so overflowing the 16-slot broadcast makes its
        // first recv return Lagged. A missed ping is still a pending reload.
        for _ in 0..32 {
            assert_eq!(server.reload_tx.send(()).unwrap(), 1);
        }
        gate.notify_one();
        let first = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
            .await
            .expect("lagged client received nothing")
            .unwrap()
            .unwrap();
        assert_eq!(
            first,
            ClientMessage::Text("reload".into()),
            "a lagged client must be reloaded, not dropped"
        );
        client.close(None).await.unwrap();
        server.wait_for_no_subscribers().await;
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
            held >= std::time::Duration::from_secs(60)
                && held < std::time::Duration::from_secs(300),
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
        assert!(
            known.is_some(),
            "the cleared evidence re-armed the hold; the info decided"
        );
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

    // The rebundle gate's input set: closure.json parses to the file set, and
    // anything unreadable reads as empty, which the gate treats as unknown
    // (always rebundle) — a torn or missing closure must never suppress one.
    #[test]
    fn client_closure_parses_and_defaults_to_empty() {
        let dir = tmp("closure-load");
        assert!(
            load_client_closure(&dir).is_empty(),
            "missing file is empty"
        );
        std::fs::write(dir.join("closure.json"), "{ not json").unwrap();
        assert!(load_client_closure(&dir).is_empty(), "torn file is empty");
        std::fs::write(
            dir.join("closure.json"),
            r#"["/app/src/a.tsx","/app/styles/app.css"]"#,
        )
        .unwrap();
        let set = load_client_closure(&dir);
        assert!(set.contains(Path::new("/app/src/a.tsx")));
        assert!(set.contains(Path::new("/app/styles/app.css")));
        assert_eq!(set.len(), 2);
    }

    // Vite's import-glob hotUpdate: an added file re-expands the importer of
    // each glob whose patterns match it (picomatch with `dot: !!exhaustive`,
    // node_modules ignored unless exhaustive) and changes nothing else. That
    // is the one way a NEW file changes a bundle whose inputs all resolved.
    // New is what the bundle never read, however the watcher reported it (a
    // file renamed into place is a rename event). The calls are
    // the glob expansion's output (glob-scopes.json), verbatim.
    #[test]
    fn new_files_rebundle_only_when_a_glob_matches_them() {
        let dir = tmp("glob-scopes-load");
        assert!(
            load_client_glob_scopes(&dir).is_empty(),
            "missing file is empty"
        );
        std::fs::write(dir.join("glob-scopes.json"), "{ not json").unwrap();
        assert!(
            load_client_glob_scopes(&dir).is_empty(),
            "torn file is empty"
        );
        // The app as the watcher spells it; the closure holds real paths.
        let app = dir.join("app");
        let a = app.to_string_lossy().replace('\\', "/");
        std::fs::write(
            dir.join("glob-scopes.json"),
            serde_json::json!([
                {"patterns": [format!("{a}/src/icons/**/*.{{svg,png}}"), format!("!{a}/src/icons/skip/**")], "exhaustive": false, "caseSensitive": false},
                {"patterns": [format!("{a}/src/modules/*/messages/*.json")], "exhaustive": false, "caseSensitive": true},
                {"patterns": [format!("{a}/src/routes/\\[id\\]/*.ts")], "exhaustive": false, "caseSensitive": true},
            ])
            .to_string(),
        )
        .unwrap();
        let scopes = load_client_glob_scopes(&dir);
        assert_eq!(scopes.len(), 3);

        let file = |rel: &str| {
            let p = app.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
            p
        };
        let bundled = file("src/modules/home/messages/en.json");
        let closure: std::collections::HashSet<PathBuf> =
            [std::fs::canonicalize(&bundled).unwrap()].into();
        for (rel, rebundles) in [
            ("src/modules/new/messages/fr.json", true),
            ("src/icons/deep/new.svg", true),
            ("src/icons/Deep/Logo.SVG", true),
            ("src/routes/[id]/edit.ts", true),
            // A member the bundle already read: an edit, not an add.
            ("src/modules/home/messages/en.json", false),
            // Under a glob's scan base, matched by none of its patterns.
            ("src/modules/dashboard/components/sedf1aa6r", false),
            ("src/modules/x/New.tsx", false),
            ("src/icons/skip/a.svg", false),
            ("src/icons/.cache/a.svg", false),
            ("src/icons/node_modules/a.svg", false),
            ("src/routes/i/edit.ts", false),
            (".wrangler/state/v3/trace-store/a.sqlite-wal", false),
        ] {
            assert_eq!(
                added_to_glob(&scopes, &closure, &file(rel)),
                rebundles,
                "{rel}"
            );
        }
        let gone = app.join("src/modules/gone/messages/de.json");
        assert!(
            !added_to_glob(&scopes, &closure, &gone),
            "a path no longer on disk was nothing a glob expands to"
        );
        assert!(!added_to_glob(
            &[],
            &closure,
            &file("src/modules/new/messages/fr.json")
        ));

        // A directory moved into place is one event, no add itself; the
        // shared watcher follows it with a create for each file inside.
        let mut known = KnownFiles(Default::default());
        let staged = dir.join("staged");
        std::fs::create_dir_all(staged.join("messages")).unwrap();
        std::fs::write(staged.join("messages/en.json"), "{}").unwrap();
        let moved = app.join("src/modules/moved");
        std::fs::rename(&staged, &moved).unwrap();
        let renamed = notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Name(notify::event::RenameMode::Any),
        ))
        .add_path(moved.clone());
        assert!(
            known.adds(&renamed).is_empty(),
            "a directory is no add itself"
        );
        let inside = moved.join("messages/en.json");
        let synth = notify::Event::new(notify::EventKind::Create(notify::event::CreateKind::Any))
            .add_path(inside.clone());
        assert_eq!(known.adds(&synth), std::slice::from_ref(&inside));
        assert!(added_to_glob(&scopes, &closure, &inside));
        // A file renamed into place is an add; its vanished source is not.
        let target = file("src/modules/renamed/messages/it.json");
        let rename = notify::Event::new(notify::EventKind::Modify(
            notify::event::ModifyKind::Name(notify::event::RenameMode::Both),
        ))
        .add_path(app.join("src/.it.json.tmp"))
        .add_path(target.clone());
        let adds = known.adds(&rename);
        assert_eq!(adds, std::slice::from_ref(&target));
        assert_eq!(change_type(&target, &adds.into_iter().collect()), "create");

        // A cache an older oj wrote: scan bases, matched as before.
        std::fs::write(
            dir.join("glob-scopes.json"),
            format!(r#"["{a}/src/content"]"#),
        )
        .unwrap();
        let legacy = load_client_glob_scopes(&dir);
        assert!(added_to_glob(
            &legacy,
            &closure,
            &file("src/content/a/b.txt")
        ));
        assert!(!added_to_glob(&legacy, &closure, &file("src/other.json")));
    }

    // Chokidar calls an event for a path in its watched set a change, and one
    // for a path it did not know an add, whatever the OS reported: an atomic
    // save renamed onto a live file is an update (a lazy glob member no
    // worker loaded must not re-expand its importer), a file renamed into
    // place is a create, and so is one recreated after its delete settled.
    #[test]
    fn renames_onto_known_files_are_changes_as_in_chokidar() {
        use notify::event::{CreateKind, DataChange, ModifyKind, RemoveKind, RenameMode};
        use notify::{Event, EventKind};
        let app = tmp("known-files");
        let file = |rel: &str| {
            let p = app.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
            p
        };
        let live = file("src/pages/live.tsx");
        file("node_modules/dep/index.js");
        let mut known = KnownFiles(oj_server::watched_files(&app));
        assert!(known.0.contains(&std::fs::canonicalize(&live).unwrap()));
        assert!(
            !known
                .0
                .iter()
                .any(|p| p.components().any(|c| c.as_os_str() == "node_modules")),
            "node_modules is never watched"
        );
        let ev = |kind, paths: &[&PathBuf]| {
            let mut ev = Event::new(kind);
            ev.paths = paths.iter().map(|p| (*p).clone()).collect();
            ev
        };
        let rename = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
        let create = EventKind::Create(CreateKind::File);

        // An editor's atomic save onto a file the scan saw.
        let staged = file("src/pages/.live.tsx.tmp");
        std::fs::rename(&staged, &live).unwrap();
        let adds = known.adds(&ev(rename, &[&staged, &live]));
        assert!(adds.is_empty(), "{adds:?}");
        assert_eq!(change_type(&live, &adds.into_iter().collect()), "update");
        // FSEvents' sticky created flag on a later write of the same file.
        assert!(known.adds(&ev(create, &[&live])).is_empty());
        // Unlinked and recreated inside one batch: chokidar's atomic change.
        std::fs::remove_file(&live).unwrap();
        assert!(known
            .adds(&ev(EventKind::Remove(RemoveKind::File), &[&live]))
            .is_empty());
        file("src/pages/live.tsx");
        assert!(known.adds(&ev(create, &[&live])).is_empty());
        known.forget_gone(&[live.clone()].into());
        assert!(
            known.adds(&ev(create, &[&live])).is_empty(),
            "still on disk"
        );

        // A new file renamed into place is an add, once.
        let fresh = app.join("src/pages/fresh.tsx");
        let staged = file("src/pages/.fresh.tsx.tmp");
        std::fs::rename(&staged, &fresh).unwrap();
        assert_eq!(
            known.adds(&ev(rename, &[&staged, &fresh])),
            std::slice::from_ref(&fresh)
        );
        let write = EventKind::Modify(ModifyKind::Data(DataChange::Content));
        assert!(known.adds(&ev(write, &[&fresh])).is_empty());
        // Deleted in a settled batch, then created again: an add.
        std::fs::remove_file(&fresh).unwrap();
        known.forget_gone(&[fresh.clone()].into());
        file("src/pages/fresh.tsx");
        assert_eq!(
            known.adds(&ev(create, &[&fresh])),
            std::slice::from_ref(&fresh)
        );
        // A file a write event named first is known: its later rename is a change.
        let edited = file("src/pages/edited.tsx");
        assert!(known.adds(&ev(write, &[&edited])).is_empty());
        assert!(known.adds(&ev(rename, &[&edited])).is_empty());
    }

    // The client bundle leaves the generated route tree and its own dir out
    // of closure.json: a glob reaching either has not grown, or a restore of
    // such an app would miss on every boot.
    #[test]
    fn restore_glob_members_skip_what_the_closure_leaves_out() {
        let app = tmp("restore-glob-members");
        let file = |rel: &str| {
            let p = app.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
            p
        };
        let member = file("src/lib/a.ts");
        file("src/routeTree.gen.ts");
        file("src/.oj/start/manifest.ts");
        let kept = file("src/routeTree.generated.ts");
        let cache = app.join("src/.oj/start");
        let scopes = app.join("glob-scopes.json");
        let a = app.to_string_lossy().replace('\\', "/");
        std::fs::write(
            &scopes,
            serde_json::json!([{"patterns": [format!("{a}/src/**/*.ts")], "exhaustive": true, "caseSensitive": true}])
                .to_string(),
        )
        .unwrap();
        let mut got = restorable_glob_members(&scopes, &cache);
        got.sort();
        assert_eq!(got, [member, kept]);
        for (name, generated) in [
            ("routeTree.gen.ts", true),
            ("routeTree.gen.jsx", true),
            ("routeTree.gen.d.ts", false),
            ("routeTree.ts", false),
        ] {
            assert_eq!(is_route_tree_gen(Path::new(name)), generated, "{name}");
        }
    }

    // The rebundle worker tracks the regen outputs' content against what the
    // worker environments last saw and pushes only real moves: rewritten
    // content and a newly created file count, an untouched file (or one
    // missing throughout) does not — regardless of WHO rewrote the file (this
    // run's regen or the framework plugin's own generator racing it).
    #[test]
    fn changed_regen_outputs_detects_rewrites_and_creations() {
        let dir = tmp("regen-outputs");
        let tree = dir.join("routeTree.gen.ts");
        let resolver = dir.join("server-fn-resolver.mjs");
        let missing = dir.join("never-written.mjs");
        std::fs::write(&tree, "tree v1").unwrap();
        let files = [tree.clone(), resolver.clone(), missing.clone()];
        let mut seen: std::collections::HashMap<PathBuf, Option<u64>> = files
            .iter()
            .map(|p| (p.clone(), file_content_hash(p)))
            .collect();

        // Nothing rewritten: no changes, even for the still-missing files.
        assert!(changed_regen_outputs(&files, &mut seen).is_empty());

        // A rewrite and a creation both count; the untouched missing file not.
        std::fs::write(&tree, "tree v2").unwrap();
        std::fs::write(&resolver, "resolver v1").unwrap();
        let changed = changed_regen_outputs(&files, &mut seen);
        let paths: Vec<&str> = changed.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            vec![tree.to_str().unwrap(), resolver.to_str().unwrap()]
        );
        assert!(changed.iter().all(|(_, t)| *t == "update"));

        // Same content written again: hashes match, no change detected.
        std::fs::write(&tree, "tree v2").unwrap();
        assert!(changed_regen_outputs(&files, &mut seen).is_empty());

        // A change the worker HAS seen (recorded above) never re-pushes, and a
        // change landing between runs (someone else's write) is caught on the
        // next pass.
        std::fs::write(&tree, "tree v3").unwrap();
        let changed = changed_regen_outputs(&files, &mut seen);
        assert_eq!(changed.len(), 1);
        assert!(changed[0].0.ends_with("routeTree.gen.ts"));
    }

    // Vite's loadEnv rule for the Start scripts: `.env.<mode>` vars with any
    // configured envPrefix (not just VITE_) pass, unprefixed ones never do.
    #[test]
    fn start_script_env_honors_env_prefix() {
        let root = tmp("script-env");
        std::fs::write(
            root.join("oj.config.json"),
            r#"{ "envPrefix": ["VITE_", "APP_"] }"#,
        )
        .unwrap();
        std::fs::write(root.join(".env.staging"), "VITE_X=1\nAPP_Y=2\nSECRET_Z=3\n").unwrap();
        let vars = start_script_env(&root, None, "serve", "staging").unwrap();
        let get = |k: &str| vars.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(get("VITE_X").as_deref(), Some("1"));
        assert_eq!(get("APP_Y").as_deref(), Some("2"));
        assert_eq!(get("SECRET_Z"), None);

        // Defaults: VITE_ only.
        let plain = tmp("script-env-plain");
        std::fs::write(plain.join(".env"), "VITE_X=1\nAPP_Y=2\n").unwrap();
        let vars = start_script_env(&plain, None, "serve", "development").unwrap();
        let names: Vec<&str> = vars.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["VITE_X"]);
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
                let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                    .await
                    .unwrap();
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
        assert!(
            !body_of(inject_reload_client(resp("text/html", Some("gzip")))).contains(RELOAD_CLIENT)
        );
        assert!(
            body_of(inject_reload_client(resp("text/html", Some("identity"))))
                .contains(RELOAD_CLIENT)
        );
        assert!(
            !body_of(inject_reload_client(resp("text/javascript", None))).contains(RELOAD_CLIENT)
        );
    }

    // Vite's NODE_ENV rule for the dev server: the shell wins (so
    // `NODE_ENV=production oj dev` is PROD), else `.env[.mode]` from `envDir`
    // may only pick development, else development.
    #[test]
    fn script_env_node_env_shell_wins_then_env_files() {
        let node_env = |env: &oj_env::Env, root: &Path, mode: &str| -> String {
            let vars = script_env(env, root, None, "serve", mode).unwrap();
            vars.into_iter()
                .find(|(k, _)| k == "NODE_ENV")
                .map(|(_, v)| v)
                .unwrap()
        };
        let silent = oj_env::Env::from_vars([("NODE_ENV", "")]);
        let root = tmp("node-env");
        assert_eq!(node_env(&silent, &root, "development"), "development");
        std::fs::write(root.join(".env.staging"), "NODE_ENV=development\n").unwrap();
        assert_eq!(node_env(&silent, &root, "staging"), "development");
        std::fs::write(root.join(".env.prodlike"), "NODE_ENV=production\n").unwrap();
        // Only development is honored from a .env file (Vite warns and ignores).
        assert_eq!(node_env(&silent, &root, "prodlike"), "development");
        let shell = oj_env::Env::from_vars([("NODE_ENV", "production")]);
        assert_eq!(node_env(&shell, &root, "staging"), "production");
        // The files come from `envDir`, like every other `.env` read.
        let with_dir = tmp("node-env-dir");
        std::fs::create_dir_all(with_dir.join("env")).unwrap();
        std::fs::write(with_dir.join("oj.config.json"), r#"{"envDir":"env"}"#).unwrap();
        std::fs::write(with_dir.join("env/.env.staging"), "NODE_ENV=development\n").unwrap();
        let prod_default = oj_env::Env::from_vars([("NODE_ENV", "")]);
        assert_eq!(node_env(&prod_default, &with_dir, "staging"), "development");
    }

    // A `.env` value reaches the scripts only when the shell lacks the key.
    #[test]
    fn prefixed_dotenv_vars_skip_keys_the_shell_sets() {
        let root = tmp("dotenv-shell");
        std::fs::write(root.join(".env"), "VITE_A=file\nVITE_B=file\nOTHER=x\n").unwrap();
        let env = oj_env::Env::from_vars([("VITE_B", "shell")]);
        let app_env = oj_env::AppEnv::resolve(
            &env,
            &root,
            "development",
            vec!["VITE_".into()],
            "development",
        );
        let vars = prefixed_dotenv_vars(&env, &app_env);
        assert_eq!(vars, vec![("VITE_A".to_string(), "file".to_string())]);
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
    }

    // Buffered request bodies are bounded, and a read failure surfaces as a
    // request failure: an aborted upload can never reach an action handler as
    // an EMPTY body, and a body past the cap is refused naming the knob.
    #[tokio::test]
    async fn body_reads_cap_at_the_knob_and_fail_on_stream_errors() {
        // Under the cap: buffered whole; empty stays None (the engine's shape).
        let body = axum::body::Body::from(vec![7u8; 1024]);
        assert_eq!(
            read_body_capped(body, 4096).await.unwrap().unwrap().len(),
            1024
        );
        assert!(read_body_capped(axum::body::Body::empty(), 4096)
            .await
            .unwrap()
            .is_none());

        // Past the cap: 413, and the message names OJ_START_MAX_BODY.
        let resp = read_body_capped(axum::body::Body::from(vec![7u8; 5000]), 4096)
            .await
            .expect_err("an oversized body is refused");
        assert_eq!(resp.status(), StatusCode::PAYLOAD_TOO_LARGE);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(
            String::from_utf8_lossy(&bytes).contains("OJ_START_MAX_BODY"),
            "the 413 names the knob"
        );

        // A mid-stream error (client abort) is a 400, never an empty body.
        let broken = axum::body::Body::from_stream(tokio_stream::iter(vec![
            Ok::<_, std::io::Error>(axum::body::Bytes::from_static(b"partial")),
            Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionReset,
                "client went away",
            )),
        ]));
        let resp = read_body_capped(broken, 4096)
            .await
            .expect_err("a read failure is a response, not an empty body");
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
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
    fn workspace_root_finds_farthest_node_modules() {
        let base = tmp("ws");
        std::fs::create_dir_all(base.join("node_modules")).unwrap();
        std::fs::create_dir_all(base.join("web").join("src")).unwrap();
        std::fs::create_dir_all(base.join("web").join("node_modules")).unwrap();
        assert_eq!(workspace_root(&base.join("web")), base);
    }

    #[test]
    fn workspace_root_defaults_to_app_without_ancestor_modules() {
        let base = tmp("ws2");
        let app = base.join("solo");
        std::fs::create_dir_all(&app).unwrap();
        assert_eq!(workspace_root(&app), app);
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
            classify(&req("GET", "/api/v1.2/x"), &["/api".to_string()]),
            Route::Pass
        ));
        assert!(matches!(
            classify(&req("GET", "/@oj-start/hmr"), &no_proxy),
            Route::Pass
        ));
        // `/__` is not reserved (Vite reserves no such prefix): an app route
        // or plugin page there is a document; only oj's own endpoints pass.
        assert!(matches!(
            classify(&req("GET", "/__health"), &no_proxy),
            Route::Document
        ));
        assert!(matches!(
            classify(&req("GET", "/__component/preview/Badge"), &no_proxy),
            Route::Document
        ));
        assert!(matches!(
            classify(&req("GET", "/__hmr_gate"), &no_proxy),
            Route::Pass
        ));
        assert!(matches!(
            classify(&req("POST", "/__hmr_flush"), &no_proxy),
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
    }

    #[test]
    fn list_route_files_missing_dir_is_empty() {
        let root = tmp("noroutes");
        assert!(list_route_files(&root).is_empty());
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
        std::fs::write(
            root.join("node_modules/@tanstack/react-start/dist/server-entry.js"),
            "",
        )
        .unwrap();
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
        assert!(
            configured_start_server_entry(&cfg("src/ssr-entry.ts"), &root).is_some(),
            "root-relative works too"
        );
        let pkg = root.join("node_modules/@tanstack/react-start/dist/server-entry.js");
        assert_eq!(
            configured_start_server_entry(&cfg(&pkg.to_string_lossy()), &root),
            None,
            "Start's own default entry is not an app entry"
        );
        assert_eq!(
            configured_start_server_entry(&cfg("src/missing.ts"), &root),
            None
        );
        let none: oj_config::OjConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(configured_start_server_entry(&none, &root), None);
        let env_scoped: oj_config::OjConfig = serde_json::from_str(&format!(
            r#"{{"environments":{{"ssr":{{"resolve":{{"alias":{{"virtual:tanstack-start-server-entry":"{}"}}}}}}}}}}"#,
            app.to_string_lossy().replace('\\', "/")
        ))
        .unwrap();
        assert!(
            configured_start_server_entry(&env_scoped, &root).is_some(),
            "environments.ssr.resolve.alias counts"
        );
    }

    // Vite: with the worker rendering the documents from TanStack's own
    // manifest, the page runs the client module graph, and an edit reaches it
    // as that graph's HMR update; the worker environment's own update never
    // reloads the page.
    #[test]
    fn worker_documents_leave_client_edits_to_the_page_hmr() {
        let unbundled = RunSignals {
            unbundled: true,
            ..Default::default()
        };
        for edit in [
            RunSignals {
                server_signals: true,
                ..unbundled
            },
            RunSignals {
                server_fn_changed: true,
                server_signals: true,
                ..unbundled
            },
        ] {
            assert!(
                !reloads_browser(edit),
                "an edit the page's own HMR applies reloaded it"
            );
        }
        // The generated route tree never reaches that HMR (the watcher drops
        // its writes), so a route-set change still reloads.
        assert!(reloads_browser(RunSignals {
            regen_changed: true,
            ..unbundled
        }));
    }

    #[test]
    fn bundle_pages_reload_for_what_no_hot_update_carried() {
        let base = RunSignals::default();
        assert!(!reloads_browser(RunSignals {
            hot: true,
            bundled: true,
            server_signals: true,
            ..base
        }));
        assert!(reloads_browser(RunSignals {
            bundled: true,
            ..base
        }));
        assert!(reloads_browser(RunSignals {
            server_signals: true,
            ..base
        }));
        assert!(reloads_browser(RunSignals {
            hot: true,
            server_fn_changed: true,
            ..base
        }));
        assert!(!reloads_browser(RunSignals {
            bundled: true,
            bundle_failed: true,
            ..base
        }));
        assert!(
            !reloads_browser(base),
            "a change outside every graph reloads nothing"
        );
    }
}
