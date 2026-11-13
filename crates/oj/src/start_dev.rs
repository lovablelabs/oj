// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

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
use oj_server::{hmr_ping, send_within, silent_deadline};
use tokio::sync::broadcast;

use crate::start_chunks::{respond, Chunk, ServedBundles};
use crate::start_host::{StartEngine, StartRequest, StartResponse};

struct StartState {
    /// The dev server's restarts, checked before they replace it.
    restarter: Arc<oj_server::Restarter>,
    /// The scripts' env (see `script_env`), resolved once at boot for the
    /// watch-reload codegen and rebundles.
    script_env: Arc<[(String, String)]>,
    /// Where the app's Start keeps its routes and generated route tree.
    routes: RoutePaths,
    /// The session's client bundler, shared by the boot bundle and rebundles.
    bundler: Arc<ClientBundler>,
    proxy_prefixes: Vec<String>,
    /// Vite's `base` without its trailing slash; empty at the root.
    base: String,
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
    /// The dev server's content index; the regen outputs' `Pushed` baseline
    /// is what the worker environments last saw.
    content: Arc<oj_server::ContentIndex>,
    /// The client bundle the pages load, and the ones before it whose chunks
    /// a page still open on an older entry imports (see [`ServedBundles`]).
    bundle: std::sync::RwLock<ServedBundles>,
    /// Gzip bodies of the live client chunks, by content hash.
    gzip: crate::start_chunks::GzipCache,
    live_reload: PathBuf,
    /// `server.hmr.overlay` (default true), filled into the live-reload client.
    hmr_overlay: bool,
    /// Vite's `html.cspNonce`: the nonce the live-reload client's tag carries
    /// when the document's own tags name none (see [`inject_reload_client`]),
    /// as the tags Vite injects into a page carry it.
    csp_nonce: Option<String>,
    /// How often a page socket sends [`PING_FRAME`] (`OJ_HMR_PING_MS`); none
    /// turns the keepalive and the client's watchdog off.
    hmr_ping: Option<Duration>,
    workspace_root: PathBuf,
    /// Vite's `publicDir` (`None` when disabled): its files answer ahead of
    /// the router and the SSR handler, see [`public_file`].
    public_dir: Option<PathBuf>,
    /// Vite's `server.headers`, which its sirv sets on a public file.
    dev_headers: Arc<Vec<(header::HeaderName, header::HeaderValue)>>,
    /// The pages' reloads, as counts (see start_reloads).
    reloads: Arc<crate::start_reloads::Epochs>,
    /// The client bundle's last failure as a `{type:"error", err}` frame,
    /// Vite's bundled dev `lastBuildError`: sent to every page when a bundle
    /// fails, replayed to each page that connects until one succeeds.
    build_error: std::sync::RwLock<Option<String>>,
    /// Frames for every open page, sent as they are: the build's failures,
    /// and the name of a chunk a page asked for that is gone (`chunk_gone`).
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
    /// The same globs by the module that expands each: a file created or
    /// deleted under one re-expands that module everywhere it is served (the
    /// SSR graph, the plugin's worker environments), as Vite's import-glob
    /// `hotUpdate` adds the importer to the update. The client engine finds
    /// them itself: `StartCodePlugin`'s transform registers each glob's base
    /// as the importer's watch file.
    client_glob_importers: std::sync::RwLock<Vec<(PathBuf, oj_compiler::glob::GlobMatcher)>>,
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
    /// Whether a gate flush releases at once or waits for the rebundle run
    /// in flight (see `FlushTiming`).
    flush_timing: Arc<crate::start_reloads::FlushTiming>,
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

/// The OS watcher's delivery latency: how long the boot's revalidation gives
/// an edit it found to reach the watcher's feed once the watcher has read
/// everything that got there before (the feed's backlog is waited out
/// separately, see `watcher_covers`).
const PREWARM_RELOAD_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// The most the boot's revalidation waits for the watcher to catch up with
/// its feed before deciding on its reload alone.
const PREWARM_RELOAD_WAIT: std::time::Duration = std::time::Duration::from_secs(60);

/// Whether a watcher run covers the reload for the modules the boot's
/// revalidation dropped. The watcher's feed was open before the engine
/// warmed, so the edit that made them stale is on it, behind whatever the
/// watcher thread has yet to read (its initial scan of the root, on a slow
/// disk, is seconds): asked too early, the watcher has seen nothing, and a
/// reload sent from here doubled the run's own, or passed the gate's hold
/// the event was about to take. So: wait until the watcher is caught up,
/// give the OS its delivery latency, and ask again while that holds.
async fn watcher_covers(timing: &crate::start_reloads::FlushTiming) -> bool {
    let deadline = tokio::time::Instant::now() + PREWARM_RELOAD_WAIT;
    loop {
        // Checked every turn: a feed that goes busy within each grace would
        // otherwise cycle here past the deadline.
        if tokio::time::Instant::now() >= deadline {
            return timing.runs_cover_reloads();
        }
        if !timing.watcher_caught_up() {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            continue;
        }
        tokio::time::sleep(PREWARM_RELOAD_GRACE).await;
        if timing.watcher_caught_up() {
            return timing.runs_cover_reloads();
        }
    }
}

pub async fn start_dev(
    root: PathBuf,
    port: Option<u16>,
    host: Option<String>,
    config: Option<PathBuf>,
    mode: Option<String>,
    typecheck: bool,
    rolldown: tokio::runtime::Handle,
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

    // The dev server's plugin host engine, before the config is first read
    // below and in the dev server build alongside: that first evaluation runs
    // on it, so the config and its hooks run once, as in Vite.
    oj_server::start_host_engine_early(&root, config_file.as_deref(), &mode, port, host.as_deref());
    // Before boot: the long start boot is where supervisor signals land, and
    // plugin children (workerd) spawn well before the server listens.
    let host_slot = std::sync::Arc::new(std::sync::OnceLock::new());
    tokio::spawn(oj_server::close_plugins_on_shutdown(host_slot.clone()));
    // The client bundle's plugins run on the dev server's host, as soon as it is up.
    let (dev_host_tx, dev_host_rx) = tokio::sync::watch::channel(None);
    let built_task = tokio::spawn({
        let dev_server = oj_server::DevServer {
            root: root.clone(),
            port,
            host,
            config: config_file.clone(),
            lazy: false,
            typecheck,
            mode: Some(mode.clone()),
            rolldown: rolldown.clone(),
        };
        async move {
            let built = dev_server
                .build_app_with(move |host| {
                    let _ = dev_host_tx.send(Some(host));
                })
                .await?;
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
    let (script_vars, routes) = {
        let inputs = script_inputs(oj_env::get(), &root, config_file.as_deref(), "serve", &mode)?;
        let vars: Arc<[(String, String)]> =
            script_vars(oj_env::get(), &root, &inputs.app_env).into();
        (vars, RoutePaths::of(&inputs.config, &root))
    };
    let route_tree = {
        let (root, cache, env) = (root.clone(), cache.clone(), Arc::clone(&script_vars));
        let routes = routes.clone();
        tokio::task::spawn_blocking(move || generate_route_tree(&root, &routes, &cache, &env))
    };
    route_tree.await??;
    oj_server::boot_phase("route tree ready");
    let resolver = {
        let (root, cache) = (root.clone(), cache.clone());
        tokio::task::spawn_blocking(move || generate_server_fn_resolver(&root, &cache, true))
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
            stylex: oj_server::stylex::plugin_for(&root, &inputs.config, "serve")
                .map_err(|e| anyhow::anyhow!(e))?,
            config: inputs.config,
            app_env: inputs.app_env,
            rolldown,
        };
        Arc::new(ClientBundler::new(start, config_file.clone()).on_dev_host(dev_host_rx))
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
    // The chunks dir holds the boot bundle's files and nothing older: a
    // restored bundle's chunks are linked in from the store, and an earlier
    // run's leftovers go (see `start_chunks::keep_chunk_files`).
    let chunks_dir = cache.join("client-chunks");
    crate::start_chunks::keep_chunk_files(&pinned, &chunks_dir);
    let (built, watch_rx) = built_res??;
    // The gate's hold decides when a change reaches the pages.
    let reloads = Arc::new(crate::start_reloads::Epochs::new(
        built
            .hmr_gate
            .clone()
            .map(|gate| -> crate::start_reloads::Held { Arc::new(move || gate.reload_is_held()) }),
    ));
    // When a gate flush takes effect against the rebundle runs, and whether
    // the boot's revalidation below still owes the pages a reload.
    let flush_timing = Arc::new(crate::start_reloads::FlushTiming::default());
    oj_server::boot_phase("bundle+build joined");
    // The in-process Start runner: an embedded engine whose module host runs
    // the dev server's SSR pipeline (StartHost); it needs the built app's
    // SsrBridge, so it comes up once the server build joins.
    let hmr_overlay;
    let (engine, public_dir, csp_nonce) = {
        let mut config = oj_config::load(&root).unwrap_or_default();
        oj_server::plugins::adopt_vite_config_values(
            &mut config,
            &root,
            config_file.as_deref(),
            "serve",
            &mode,
        )
        .map_err(|e| anyhow::anyhow!(e))?;
        hmr_overlay = match config.server.as_ref().and_then(|s| s.hmr.as_ref()) {
            Some(oj_config::HmrConfig::Options(o)) => o.overlay.unwrap_or(true),
            _ => true,
        };
        let entry = configured_start_server_entry(&config, &root)
            .unwrap_or_else(|| cache.join("server-entry.tsx"));
        let init_env = script_vars.to_vec();
        let public_dir = oj_config::public_dir(&config, &root);
        let engine = Arc::new(StartEngine::new(
            root.clone(),
            &cache,
            built.ssr.clone(),
            entry,
            oj_config::ssr_externals(&config),
            public_dir.clone(),
            crate::start_bundle::assets::client_inline_limit(&config),
            init_env,
        )?);
        (engine, public_dir, oj_config::html_csp_nonce(&config))
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
        let reloads = Arc::clone(&reloads);
        let flush_timing = Arc::clone(&flush_timing);
        let warm_host = built.plugin_host.clone();
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
                            // the whole cold graph. One real render through
                            // the serving path fills the runner's module
                            // cache instead, beside the app's own
                            // `server.warmup` files, which the host warms in
                            // their environments as Vite's warmup does.
                            oj_server::boot_phase("prewarm: engine skipped (worker environments)");
                            let transforms = async {
                                if let Some(host) = warm_host.as_deref() {
                                    // The host DISPATCHES the warms and replies
                                    // immediately (Vite's warmup is equally
                                    // fire-and-forget).
                                    match host.warm_environments().await {
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
            // The render read to its end, so the deferred content's modules
            // are warm too.
            if let Ok(resp) = engine
                .handle(engine_request(
                    "GET".into(),
                    "/".into(),
                    &header::HeaderMap::new(),
                    None,
                    false,
                ))
                .await
            {
                let _ = resp.body_bytes().await;
            }
            oj_server::boot_phase("prewarm complete");
            // Tests stretch the gap between the warm render and the
            // revalidation, to land an edit in it.
            if let Some(delay) = std::env::var("OJ_START_PREWARM_DELAY_MS")
                .ok()
                .and_then(|v| v.parse::<u64>().ok())
            {
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            }
            // Modules edited while the engine warmed are dropped; the pages
            // reload for them only when no watcher run does (an edit before
            // the watcher armed). An edit the watcher saw is rebuilt and
            // reloaded by its run: a reload from here too, after the gate's
            // flush consumed the hold (or parked, for the flush to publish
            // ahead of the run), reloaded the pages a second time, or onto
            // the bundle from before the edit. The watcher may not have
            // read that edit yet (`watcher_covers` waits for it to).
            if engine.revalidate().await {
                if watcher_covers(&flush_timing).await {
                    oj_server::boot_phase(
                        "revalidate: stale modules, reload left to the watcher's run",
                    );
                } else {
                    oj_server::boot_phase("revalidate: stale modules, reloading");
                    reloads.bump();
                }
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
    // The resolved gate (the knob or `server.hmrGate`). A full-reload gate
    // reloads on every release, so it drops the boot's engine at the next
    // rebundle; a granular one keeps hot updates and parks them for the
    // editor's flush.
    bundler.set_hot_enabled(hot_under_gate(
        built.hmr_gate.as_ref().map(|gate| gate.full_reload()),
    ));
    if let Some(gate) = built.hmr_gate.clone() {
        bundler
            .hot
            .set_gate(Arc::new(move || gate.reload_is_held()));
    }
    // The dev urls' prefix under `base` (see oj_server::start_dev_url_base).
    let base = oj_server::start_dev_url_base(bundler.base());
    let state = Arc::new(StartState {
        restarter: Arc::clone(&built.restarter),
        script_env: Arc::clone(&script_vars),
        routes: routes.clone(),
        bundler,
        proxy_prefixes: built.proxy_prefixes.clone(),
        base,
        plugin_serve: Arc::clone(&built.plugin_serve),
        engine,
        loopback_port,
        bundle: std::sync::RwLock::new(ServedBundles::new(pinned, chunks_dir)),
        gzip: Default::default(),
        live_reload: cache.join("live-reload.js"),
        hmr_overlay,
        csp_nonce,
        hmr_ping: hmr_ping(oj_env::get().knobs.hmr_ping_ms),
        workspace_root: workspace_root(&root),
        public_dir,
        dev_headers: Arc::clone(&built.dev_headers),
        reloads: Arc::clone(&reloads),
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
        content: {
            let content = Arc::clone(&built.content);
            for p in regen_output_files(&routes.tree, &cache) {
                content.settle(&p, oj_server::Baseline::Pushed);
            }
            content
        },
        client_closure: std::sync::RwLock::new(load_client_closure(&cache)),
        client_glob_scopes: std::sync::RwLock::new(load_client_glob_scopes(&cache)),
        client_glob_importers: std::sync::RwLock::new(load_client_glob_importers(&cache)),
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
        flush_timing,
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
    // the changes held for it. One that lands while a rebundle runs (the
    // editor flushes right after its writes) waits for that run: its end
    // releases everything once, onto its output (`FlushTiming`). Releasing at
    // the flush could publish a reload parked by something other than the
    // run (the boot's revalidation), onto the output from before it, and the
    // run's end would reload the pages a second time.
    if let Some(gate) = &state.gate {
        let mut flushes = gate.subscribe_flush();
        let weak = Arc::downgrade(&state);
        tokio::spawn(async move {
            // A lagged receiver missed flushes, not what they released: the
            // next publish covers them all.
            while !matches!(
                flushes.recv().await,
                Err(broadcast::error::RecvError::Closed)
            ) {
                let Some(state) = weak.upgrade() else { return };
                if state.flush_timing.flush_now() {
                    release_gate_flush(&state);
                } else if state.reloads.parked() {
                    // The run's end publishes the parked reload; the hot
                    // frames parked so far are moot under it.
                    state.bundler.hot.discard();
                } else {
                    // The frames parked so far go out now, ahead of the ones
                    // the run makes (the hold is consumed, so those pass
                    // straight through at its end): a later frame must not
                    // overtake an earlier one, or the page sees a gap.
                    state.bundler.hot.release();
                }
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

    // This server's routes run inside the dev server's cors and host gates,
    // as Vite's corsMiddleware and hostValidationMiddleware come before every
    // other middleware (a framework's SSR handler, its own module urls and
    // the public dir included): a request from a host that is not allowed
    // gets no document either.
    {
        let state = Arc::clone(&state);
        let hook: oj_server::RouteHook =
            Arc::new(move |req, next| Box::pin(start_route(State(Arc::clone(&state)), req, next)));
        let _ = built.route_hook.set(hook);
    }
    let app = built.router.clone();

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
    // Outermost, around this server's own routes too.
    let app = oj_server::otel::traced(
        app,
        oj_server::tls::url_scheme(built.https.as_ref()),
        built.base.as_deref(),
    );
    built.serve(listener, app).await?;
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
    state.flush_timing.rebundle_wanted();
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
        state.engine.reload(&[]).await;
    }
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

/// Where the app's Start keeps its routes and generated route tree: what
/// `tanstackStart({ srcDirectory, router })` resolves to, which wins over
/// tsr.config.json under Start (see `oj_config::start_route_tree`).
#[derive(Debug, Clone)]
struct RoutePaths {
    dir: PathBuf,
    tree: PathBuf,
}

impl RoutePaths {
    fn of(config: &oj_config::OjConfig, root: &Path) -> Self {
        RoutePaths {
            dir: oj_config::start_routes_dir(config, root),
            tree: oj_config::start_route_tree(config, root),
        }
    }
}

/// The route files under `dir` (the routes directory).
fn list_route_files(dir: &Path) -> std::collections::BTreeSet<PathBuf> {
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
    walk(dir, &mut out);
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

/// The content type of a served file: the kinds this server serves that
/// are no asset (a stylesheet with its charset, a script, JSON, wasm), and
/// every asset type from the table the dev server and the build share.
fn asset_mime(ext: &str) -> &'static str {
    match ext {
        "css" => "text/css; charset=utf-8",
        // Vite's sirv setHeaders: the TypeScript extensions are JavaScript
        // too, not the video/mp2t their registered type says.
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "mts" | "cts" | "tsx" => "text/javascript",
        "json" => "application/json",
        "wasm" => "application/wasm",
        _ => oj_compiler::assets::asset_mime(ext),
    }
}

/// A document's stamp: the reloads it has (`g`, see start_reloads) and,
/// with hot updates, the hot changes it has (`h`, see
/// `HotUpdates::behind`), read by the caller before the document waits for
/// its output. Both ride the live-reload client's url to its socket.
fn document_stamp(state: &StartState, hot_has: Option<u64>) -> String {
    let reloads = state.reloads.stamp();
    match hot_has {
        Some(h) => format!("{reloads}&h={h}"),
        None => reloads,
    }
}

/// The hot changes a document rendered now has: read before it waits for
/// output that carries them, so a change landing in between can only make
/// the page reload once more, never miss it.
fn hot_has_now(state: &StartState) -> Option<u64> {
    state
        .bundler
        .hot_enabled()
        .then(|| state.bundler.hot.changes())
}

/// The live-reload client's tag, under the dev `base` prefix, with the
/// document's stamp (see start_reloads) and the document's CSP nonce when it
/// has one (see [`inject_reload_client`]), written as Vite's
/// `injectNonceAttributeTagHook` writes it on the tags it injects.
fn reload_client(url_base: &str, stamp: &str, nonce: Option<&str>) -> String {
    // The stamp's query (`g`, then `&h=`) sits in an attribute: `&` escaped.
    let stamp = stamp.replace('&', "&amp;");
    let nonce = nonce.map_or(String::new(), |n| format!(" nonce=\"{n}\""));
    format!(
        "<script type=\"module\" src=\"{url_base}/@oj-start/live-reload.js?g={stamp}\"{nonce}></script>"
    )
}

/// The nonce a document's own `<script>` tags carry, read off the body as it
/// streams past. Start writes the router's `ssr.nonce` on every script tag it
/// renders (@tanstack/react-router's `Scripts` and `Asset`, and router-core's
/// `injectScript` in ssr/ssr-server.ts, which writes it single-quoted), so
/// the first script tag with one names the nonce the page's policy accepts.
/// A tag may straddle two chunks: the bytes from an unclosed `<` are kept for
/// the next one, dropped past [`ScriptNonceScan::CARRY_MAX`] so a stray `<`
/// in an inline script's text never grows the carry without end.
#[derive(Default)]
struct ScriptNonceScan {
    carry: Vec<u8>,
    found: Option<String>,
}

impl ScriptNonceScan {
    const CARRY_MAX: usize = 4096;

    fn feed(&mut self, chunk: &[u8]) {
        if self.found.is_some() {
            return;
        }
        let mut buf = std::mem::take(&mut self.carry);
        buf.extend_from_slice(chunk);
        let mut at = 0;
        while let Some(lt) = script_open(&buf, at) {
            let Some(gt) = tag_end(&buf[lt..]) else {
                self.keep(&buf[lt..]);
                return;
            };
            let tag = String::from_utf8_lossy(&buf[lt..lt + gt]);
            if let Some(nonce) = oj_server::html_tag_attr(&tag, "nonce").filter(|n| !n.is_empty()) {
                self.found = Some(nonce.to_string());
                return;
            }
            at = lt + gt + 1;
        }
        // A chunk ending inside a tag's name (`<scr`), or in any tag the next
        // chunk closes: from the last `<` no `>` follows.
        if let Some(lt) = buf[at..].iter().rposition(|&b| b == b'<') {
            let rest = &buf[at + lt..];
            if !rest.contains(&b'>') {
                self.keep(rest);
            }
        }
    }

    fn keep(&mut self, rest: &[u8]) {
        if rest.len() <= Self::CARRY_MAX {
            self.carry = rest.to_vec();
        }
    }

    fn nonce(&self) -> Option<&str> {
        self.found.as_deref()
    }
}

/// The offset of the next `<script` start tag at or after `from`: the name
/// in any case, ended by whitespace, `>` or `/`, or by the buffer (a tag the
/// next chunk continues). Every document passes through here whole when it
/// has no nonce, so the search goes from `<` to `<` rather than byte by byte.
fn script_open(buf: &[u8], from: usize) -> Option<usize> {
    const NAME: &[u8] = b"script";
    let mut i = from;
    while let Some(lt) = buf.get(i..)?.iter().position(|&b| b == b'<') {
        let name = i + lt + 1;
        let found = buf.get(name..name + NAME.len())?;
        if found.eq_ignore_ascii_case(NAME)
            && buf
                .get(name + NAME.len())
                .is_none_or(|b| b.is_ascii_whitespace() || matches!(b, b'>' | b'/'))
        {
            return Some(i + lt);
        }
        i = name;
    }
    None
}

/// The offset of the `>` that ends the start tag `tag` begins with: one
/// inside a quoted attribute value (`data-x="a>b"`) is the value's.
fn tag_end(tag: &[u8]) -> Option<usize> {
    let mut quote = None;
    for (i, &b) in tag.iter().enumerate() {
        match (quote, b) {
            (None, b'"' | b'\'') => quote = Some(b),
            (None, b'>') => return Some(i),
            (Some(q), _) if q == b => quote = None,
            _ => {}
        }
    }
    None
}

/// One rebundle run's inputs, handed from the watcher thread to the coalescing
/// rebundle worker. Batches landing while a rebundle is in flight merge into
/// the single pending run (paths union), so at most one run ever queues.
#[derive(Default)]
struct PendingRebundle {
    /// Every changed path of the merged batches (relevant and not), for the
    /// regen decisions; the HMR gate recorded them at the watcher event.
    paths: std::collections::HashSet<PathBuf>,
    /// The glob importers the batches' created and deleted files re-expand
    /// (`glob_importers_hit`): changed modules for the server graphs, though
    /// no file of theirs was written.
    globbed: std::collections::HashSet<PathBuf>,
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

/// Every glob call the last bundle expanded, by the module that expands it
/// (an entry's `importer`). A cache written before the importer was recorded
/// contributes nothing here: its globs still gate the rebundle.
fn load_client_glob_importers(cache: &Path) -> Vec<(PathBuf, oj_compiler::glob::GlobMatcher)> {
    let Some(entries) = std::fs::read_to_string(cache.join("glob-scopes.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Vec<serde_json::Value>>(&s).ok())
    else {
        return Vec::new();
    };
    entries
        .iter()
        .filter_map(|e| {
            let importer = PathBuf::from(e.get("importer")?.as_str()?);
            let call = oj_compiler::glob::GlobCall::from_json(e)?;
            Some((importer, call.matcher()))
        })
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

/// A file a bundled glob matched that is gone from disk: its importer's
/// expansion shrinks, the other way a file that is not an edit changes a
/// bundle. The bundler's engine has no module for a `?raw` or `?url` member
/// by its bare path, so the closure alone cannot tell; the glob can.
fn gone_from_glob(scopes: &[oj_compiler::glob::GlobMatcher], p: &Path) -> bool {
    if scopes.is_empty() || p.exists() {
        return false;
    }
    let file = p.to_string_lossy().replace('\\', "/");
    scopes.iter().any(|g| g.is_match(&file))
}

/// The modules whose `import.meta.glob` a created or deleted file of `paths`
/// matches: they re-expand, as plain dev's glob importers do and as Vite's
/// import-glob `hotUpdate` adds them to the update. A file that exists and
/// the bundle already read is an edit, which changes no expansion.
fn glob_importers_hit(
    importers: &[(PathBuf, oj_compiler::glob::GlobMatcher)],
    closure: &std::collections::HashSet<PathBuf>,
    paths: &std::collections::HashSet<PathBuf>,
) -> Vec<PathBuf> {
    if importers.is_empty() {
        return Vec::new();
    }
    let mut hit: Vec<PathBuf> = Vec::new();
    for p in paths {
        let known = closure.contains(p)
            || std::fs::canonicalize(p).is_ok_and(|real| closure.contains(&real));
        if p.exists() && known {
            continue;
        }
        let file = p.to_string_lossy().replace('\\', "/");
        for (importer, matcher) in importers {
            if matcher.is_match(&file) && !hit.contains(importer) {
                hit.push(importer.clone());
            }
        }
    }
    hit.sort();
    hit
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
    let left_out = crate::start_bundle::dev::left_out_of_closure(cache);
    load_glob_calls(scopes)
        .iter()
        .flat_map(oj_compiler::glob::GlobCall::files)
        .filter(|f| !left_out(f))
        .collect()
}

/// Whether the client bundle hot-updates, given the editor's gate (`Some`
/// with its full-reload mode) or none. Only a full-reload gate turns hot
/// updates off: its releases reload the pages anyway.
fn hot_under_gate(gate_full_reload: Option<bool>) -> bool {
    gate_full_reload != Some(true)
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
    paths
        .iter()
        .any(|p| added_to_glob(&scopes, &closure, p) || gone_from_glob(&scopes, p))
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
        } else if added_to_glob(&scopes, &closure, p) || gone_from_glob(&scopes, p) {
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
fn regen_output_files(tree: &Path, cache: &Path) -> [PathBuf; 2] {
    [tree.to_path_buf(), cache.join("server-fn-resolver.mjs")]
}

/// The regen outputs whose content moved past their `Pushed` baseline (what
/// the worker environments last saw), as invalidate changes; the baseline
/// takes the current content.
fn changed_regen_outputs(
    files: &[PathBuf],
    content: &oj_server::ContentIndex,
) -> Vec<(String, &'static str)> {
    use oj_server::Baseline::Pushed;
    files
        .iter()
        .filter(|f| {
            let last = content.baseline(f, Pushed);
            let current = content.settle(f, Pushed);
            match last {
                Some(last) => current != last,
                // Never seen (a config change moved the tree): only a file
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
        state.flush_timing.watcher_armed();
        loop {
            let mut created: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
            // The boot's revalidation asks whether this thread is caught up
            // with its feed (`FlushTiming::watcher_caught_up`): idle while
            // waiting on an empty one, not from the first event in hand.
            let first = match rx.try_recv() {
                Ok(ev) => Ok(ev),
                Err(std::sync::mpsc::TryRecvError::Empty) => {
                    state.flush_timing.watcher_idle(true);
                    rx.recv()
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
            };
            state.flush_timing.watcher_idle(false);
            let mut paths: std::collections::HashSet<PathBuf> = match first {
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
                state.restarter.request_unchecked("the start entry changed");
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
            state.flush_timing.watcher_change();
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
            // A file created or deleted under a module's `import.meta.glob`
            // re-expands that module: an edit of it as far as every server
            // graph is concerned, though its file was not written.
            let globbed: std::collections::HashSet<PathBuf> = {
                let importers = state
                    .client_glob_importers
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                let closure = state
                    .client_closure
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                glob_importers_hit(&importers, &closure, &paths)
                    .into_iter()
                    .collect()
            };
            for importer in &globbed {
                let shown = importer.strip_prefix(&watch_root).unwrap_or(importer);
                println!("  oj start: glob importer {} re-expanded", shown.display());
            }
            // Invalidate the plugin's worker DevEnvironments so a Cloudflare
            // app re-renders with the changed modules (not stale SSR). Spawned
            // promptly per settled batch, never behind an in-flight rebundle;
            // the rebundle worker awaits it before that run's browser reload.
            let invalidate = if globbed.is_empty() {
                spawn_mw_invalidate(&rt, &state, &paths, &created)
            } else {
                let with_importers: std::collections::HashSet<PathBuf> =
                    paths.union(&globbed).cloned().collect();
                spawn_mw_invalidate(&rt, &state, &with_importers, &created)
            };
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
                run.globbed.extend(globbed);
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
    let routes = state.routes.clone();
    let mut prev_routes = list_route_files(&routes.dir);
    // Each route file's server-only shape, for the edits that flip it.
    let shapes = {
        let dir = routes.dir.clone();
        let seeded = tokio::task::spawn_blocking(move || route_shapes(&list_route_files(&dir)))
            .await
            .unwrap_or_default();
        Arc::new(std::sync::Mutex::new(seeded))
    };
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
        // The server graphs' changed files: the batch's, and the glob
        // importers its created and deleted files re-expand.
        let served: Vec<PathBuf> = paths.iter().chain(run.globbed.iter()).cloned().collect();
        let regen_files = regen_output_files(&routes.tree, &cache);
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
            let shapes = Arc::clone(&shapes);
            let routes = routes.clone();
            // Every batch, bundled or not: a style edit the gate passes over
            // must still recompile at the next bundle.
            bundler.note_changes(&changed);
            tokio::task::spawn_blocking(move || {
                let routes_now = list_route_files(&routes.dir);
                let routes_changed = routes_now != routes_prev;
                // An edit that flips a route's only-`server` shape moves it in
                // or out of the client's tree: Start invalidates the tree and
                // reloads the client only then (its generator plugin's
                // afterTransform). Other edits leave the tree as it is.
                let shape_flipped = {
                    let mut known = shapes
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let mut flipped = false;
                    for p in changed.iter().filter(|p| routes_now.contains(*p)) {
                        if let Some(now) = route_shape(p) {
                            flipped |= known.insert(p.clone(), now) != Some(now);
                        }
                    }
                    flipped
                };
                let client_tree = c.join(crate::start_bundle::CLIENT_ROUTE_TREE);
                let client_tree_before = std::fs::read(&client_tree).ok();
                if routes_changed || shape_flipped {
                    let _ = generate_route_tree(&r, &routes, &c, &env);
                }
                let client_tree_changed = std::fs::read(&client_tree).ok() != client_tree_before;
                let mut inputs = inputs;
                if client_tree_changed {
                    inputs.push(routes.tree.clone());
                }
                // A batch that may have changed the resolver regenerates it;
                // one that only may have (a tsconfig, a module its scan
                // read, a removed file) counts as a change when the resolver
                // it gets is another. Counted on the trigger alone, a temp
                // file that came and went beside an edit reloaded the page
                // the edit would have patched in place.
                let trigger = server_fn_trigger(&r, &c, &changed);
                let resolver_changed = refresh_server_fn_resolver(&r, &c, trigger, routes_changed);
                // Vite's reloadOnTsconfigChange: a `tsconfig.json` clears
                // every environment's modules and reloads the page, whatever
                // it holds and whatever the resolver made of it.
                let server_fn_changed =
                    resolver_changed || changed.iter().any(|p| is_tsconfig_json(p));
                // A route-set change rewrites the generated tree, a bundled
                // input, even when the triggering paths missed the closure.
                // Nothing to bring current while no page runs the bundle; a
                // runner document that loaded it since (it marks the bundle
                // loaded before it looks for a deferral) still gets it.
                let wants = needs_bundle || routes_changed || client_tree_changed;
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
                    Some(Ok(false)) => match pinned_output(&bundler, &r, &c, &m) {
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
                        // The patch may have grown the graph (an import the
                        // edit added): the gate's inputs follow it.
                        load_inputs(&state, &cache);
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
        let regen_changed = changed_regen_outputs(&regen_files, &state.content);
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
                state.engine.reload(&served).await;
                true
            } else {
                state.engine.revalidate_for(&served).await
            }
        } else if no_invalidates_sent {
            // Lazy with the middleware down: the fallback runner is serving,
            // so its graph is the only reload signal left.
            state.engine.revalidate_for(&served).await
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
            worker_hit || engine_hit || served.iter().any(|p| state.ssr.graph_knows_file(p));
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
        // The hold was taken at the watcher event. A flush that came while this
        // run built released it, and the reload reaches the pages now, with
        // the fresh bundle; otherwise it waits for the flush.
        let held = reload && !state.reloads.bump();
        // A flush that landed during this run publishes now, with the run's
        // output in place (its hot frames went out, or were dropped, at the
        // flush): a reload this run published already covers everything;
        // otherwise the flush publishes what it would have at the time.
        let more = pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some();
        if state.flush_timing.run_ended(more) {
            if reload && !held {
                state.bundler.hot.discard();
            } else {
                release_gate_flush(&state);
            }
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

/// A gate flush taking effect: the parked reloads publish, and the parked
/// hot updates go out, or are dropped when a reload makes them moot. A
/// reload the flush publishes takes every page onto output that carries the
/// parked hot updates: patching a page that is about to unload would be a
/// patch fetch cut off by the reload. Otherwise they go out, in order.
fn release_gate_flush(state: &StartState) {
    if state.reloads.flushed() {
        state.bundler.hot.discard();
    } else {
        state.bundler.hot.release();
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
    bundler: &ClientBundler,
    root: &Path,
    cache: &Path,
    mode: &str,
) -> Option<oj_cache::start_bundle::PinnedBundle> {
    if !bundler.cacheable() {
        return oj_cache::start_bundle::PinnedBundle::from_build_dir(cache);
    }
    match keyed_bundle_store(bundler, root, mode).persist(cache) {
        Some((key, pinned)) => {
            save_stylex_rules(bundler, cache, &key);
            save_generated_sheets(bundler, cache, &key);
            Some(pinned)
        }
        None => oj_cache::start_bundle::PinnedBundle::from_build_dir(cache),
    }
}

/// The StyleX rules of the bundle just cached under `key`, saved beside it
/// so a restore of it serves a complete sheet.
/// Marks the bundle cached under `key` as holding a stylesheet a plugin
/// serves (UnoCSS's): its CSS comes from what the plugin collects as modules
/// transform, which a restored bundle never does, so it is rebuilt instead.
const GENERATED_SHEETS: &str = "client-generated-sheets.json";

fn save_generated_sheets(bundler: &ClientBundler, cache: &Path, key: &str) {
    let file = cache.join(GENERATED_SHEETS);
    let generated = bundler
        .bundle_modules()
        .iter()
        .any(|id| crate::start_bundle::host::is_served_stylesheet(id));
    if generated {
        let _ = std::fs::write(file, serde_json::json!({ "key": key }).to_string());
    } else {
        let _ = std::fs::remove_file(file);
    }
}

fn has_generated_sheets(cache: &Path, key: &str) -> bool {
    std::fs::read(cache.join(GENERATED_SHEETS))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
        .is_some_and(|v| v["key"] == key)
}

fn save_stylex_rules(bundler: &ClientBundler, cache: &Path, key: &str) {
    if let Some(plugin) = bundler.stylex() {
        crate::stylex_plugin::save_rules(plugin, cache, key, &bundler.bundle_modules());
    }
}

/// Serve a fresh client output, with the inputs it was built from. The
/// bundle it replaces stays served for the pages open on it; the oldest
/// retained bundles past the window are forgotten, and their chunk files
/// that no served bundle names any more leave the chunks dir.
fn install_bundle(state: &StartState, cache: &Path, pinned: oj_cache::start_bundle::PinnedBundle) {
    let (current, forgotten) = {
        let mut served = state
            .bundle
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let forgotten = served.install(pinned);
        (served.current(), forgotten)
    };
    // With the bundles unlocked: chunk requests are not held up by it.
    state.gzip.retain_hashes_of(&current);
    crate::start_chunks::remove_chunk_files(&cache.join("client-chunks"), &forgotten);
    load_inputs(state, cache);
}

/// Takes the bundle's input set as the bundler last wrote it (closure.json,
/// and glob-scopes.json as the glob scopes and as their importers): after a
/// full output, and after a hot update, whose patch can bring new modules
/// and new globs into the graph.
fn load_inputs(state: &StartState, cache: &Path) {
    *state
        .client_closure
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = load_client_closure(cache);
    *state
        .client_glob_scopes
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = load_client_glob_scopes(cache);
    *state
        .client_glob_importers
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = load_client_glob_importers(cache);
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
            pinned_output(&bundler, &r, &c, &m)
                .ok_or_else(|| anyhow::anyhow!("no client chunk index"))
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
            state.reloads.bump();
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

/// The `.env` vars of one command, resolved against the process snapshot:
/// [`script_vars`] without the app root, cache root and NODE_ENV.
#[cfg(test)]
fn start_script_env(
    root: &Path,
    config_file: Option<&Path>,
    command: &str,
    mode: &str,
) -> anyhow::Result<Vec<(String, String)>> {
    let env = oj_env::get();
    let inputs = script_inputs(env, root, config_file, command, mode)?;
    Ok(crate::start_bundle::dotenv_process_vars(
        env,
        &inputs.app_env,
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

/// The Start build's outDir (`--out`, else `build.outDir`, else `dist`),
/// emptied by Vite's emptyOutDir rule as in `oj build`: one outside the root
/// is left alone unless `--emptyOutDir`/`build.emptyOutDir` says otherwise,
/// and the root itself is never emptied.
fn prepare_start_out_dir(
    root: &Path,
    out: Option<PathBuf>,
    build_cfg: &oj_config::BuildConfig,
    empty_out_dir: bool,
) -> anyhow::Result<PathBuf> {
    let out = out
        .or_else(|| build_cfg.out_dir.as_ref().map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("dist"));
    let out_dir = if out.is_absolute() {
        out
    } else {
        root.join(out)
    };
    let empty = if empty_out_dir {
        Some(true)
    } else {
        build_cfg.empty_out_dir
    };
    crate::build::prepare_out_dir(root, &out_dir, empty)?;
    Ok(out_dir)
}

pub async fn start_build(
    root: PathBuf,
    config_file: Option<PathBuf>,
    mode: &str,
    out: Option<PathBuf>,
    empty_out_dir: bool,
    rolldown: tokio::runtime::Handle,
) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("app root not found: {}: {e}", root.display()))?;
    let cache = oj_cache::cache_root(&root).join("start");
    oj_server::prepare_cache_root(&root);
    oj_server::write_start_assets(&cache)?;
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
    // The route tree is generated under the build's own config and env, as
    // the router plugin does inside a Vite build: no second evaluation.
    {
        let gen_env = script_vars(oj_env::get(), &root, &app_env);
        let (r, c) = (root.clone(), cache.clone());
        let routes = RoutePaths::of(&config, &root);
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            generate_route_tree(&r, &routes, &c, &gen_env)?;
            generate_server_fn_resolver(&r, &c, false)
        })
        .await??;
    }
    let build_cfg = config.build.clone().unwrap_or_default();
    let out_dir = prepare_start_out_dir(&root, out, &build_cfg, empty_out_dir)?;
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
        stylex: oj_server::stylex::plugin_for(&root, &config, "build")
            .map_err(|e| anyhow::anyhow!(e))?,
        config,
        app_env,
        rolldown,
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
/// `env`: what the codegen script's `process.env` holds; the route tree's
/// generator runs app code (virtual route configs) that can read it.
fn codegen_store(
    root: &Path,
    cache: &Path,
    kind: &str,
    script: Option<&str>,
    marker: Option<&str>,
    env: &[(String, String)],
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
    let mut env: Vec<&(String, String)> = env.iter().collect();
    env.sort();
    for (k, v) in env {
        for part in [k, v] {
            extra.extend_from_slice(part.as_bytes());
            extra.push(0);
        }
    }
    let mut names: Vec<String> = [
        "tsr.config.json",
        "package.json",
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lockb",
    ]
    .map(String::from)
    .to_vec();
    // The server-fn finder resolves an import through the tsconfig `paths`.
    if kind.starts_with("server-fn") {
        names.extend(
            resolution_configs(root)
                .iter()
                .map(|p| {
                    p.strip_prefix(root)
                        .unwrap_or(p)
                        .to_string_lossy()
                        .into_owned()
                })
                .filter(|name| name != "package.json"),
        );
    }
    for name in &names {
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

/// Whether `path` is a server-only route as the generator sees it; None when
/// it cannot be read or parsed (mid-edit).
fn route_shape(path: &Path) -> Option<bool> {
    let code = std::fs::read_to_string(path).ok()?;
    oj_compiler::route_options::is_server_only_route(&code, &path.to_string_lossy())
}

fn route_shapes(
    routes: &std::collections::BTreeSet<PathBuf>,
) -> std::collections::HashMap<PathBuf, bool> {
    routes
        .iter()
        .filter_map(|p| Some((p.clone(), route_shape(p)?)))
        .collect()
}

fn generate_route_tree(
    root: &Path,
    routes: &RoutePaths,
    cache: &Path,
    script_env: &[(String, String)],
) -> anyhow::Result<()> {
    // The client's pruned tree (generate.mjs), beside the generated one.
    let client = cache.join(crate::start_bundle::CLIENT_ROUTE_TREE);
    let mut script_env = script_env.to_vec();
    script_env.push((
        "OJ_CLIENT_ROUTE_TREE".to_string(),
        client.to_string_lossy().into_owned(),
    ));
    // Start's paths, passed inline over tsr.config.json as Start passes them.
    for (name, path) in [
        ("OJ_ROUTES_DIRECTORY", &routes.dir),
        ("OJ_GENERATED_ROUTE_TREE", &routes.tree),
    ] {
        script_env.push((name.to_string(), path.to_string_lossy().into_owned()));
    }
    let script_env = script_env.as_slice();
    let store = codegen_store(
        root,
        cache,
        "route-tree",
        Some("generate.mjs"),
        None,
        script_env,
    );
    let dest = routes.tree.as_path();
    let outputs = [
        ("routeTree.gen.ts", dest),
        (crate::start_bundle::CLIENT_ROUTE_TREE, client.as_path()),
    ];
    let inputs: Vec<PathBuf> = list_route_files(&routes.dir).into_iter().collect();
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
    // A crawl that fails writes no client tree: the client then loads the
    // generated one, not a tree left by an earlier run.
    let _ = std::fs::remove_file(&client);
    run_script_process(
        root,
        &cache.join("generate.mjs"),
        script_env,
        "route tree generation",
    )?;
    let inputs: Vec<PathBuf> = list_route_files(&routes.dir).into_iter().collect();
    store.persist(&inputs, &outputs);
    Ok(())
}

/// Dev and build resolvers differ (`dev` adds Start's dev ids), so each is
/// cached under its own kind and written to its own file.
///
/// Every source file keys the resolver, not only the ones naming
/// `createServerFn`: a server function finished on an imported builder
/// (`authedFn.handler(..)`) sits in a file that does not name the factory,
/// and what the import resolves to depends on the modules between the two.
/// So do the tsconfig files, whose `paths` an import may be spelled through,
/// and the modules the last scan read that are not source files under
/// `src/` (a `.js` builder, a `shared/` directory an alias names).
fn generate_server_fn_resolver(root: &Path, cache: &Path, dev: bool) -> anyhow::Result<()> {
    server_fn_resolver(root, cache, dev).map(|_| ())
}

/// [`generate_server_fn_resolver`], answering whether the resolver came from
/// the cache.
fn server_fn_resolver(root: &Path, cache: &Path, dev: bool) -> anyhow::Result<bool> {
    let kind = if dev { "server-fn" } else { "server-fn-build" };
    let store = codegen_store(root, cache, kind, None, None, &[]);
    let file = crate::start_bundle::server_fn_resolver_file(dev);
    let dest = cache.join(file);
    let reads_file = crate::start_bundle::server_fn_reads_file(dev);
    let reads_dest = cache.join(reads_file);
    let outputs = [(file, dest.as_path()), (reads_file, reads_dest.as_path())];
    let inputs = server_fn_inputs(root, &reads_dest);
    match store.restore(&inputs, &outputs) {
        Ok(stats) => {
            println!(
                "  oj start: server-fn resolver restored from cache (key {}…, {} source file(s), {} of {} rehashed, {}ms)",
                stats.key.get(..8).unwrap_or(&stats.key),
                stats.keyed_files,
                stats.rehashed,
                inputs.len(),
                stats.elapsed_ms
            );
            // The reads file the key was made with may be another
            // resolver's (the app went from one state to another and back):
            // the restored one names what this resolver depends on, and it
            // is this resolver's only when it gives the same inputs.
            if same_files(&inputs, &server_fn_inputs(root, &reads_dest)) {
                return Ok(true);
            }
            println!("  oj start: server-fn resolver cache miss (a module it read changed)");
        }
        Err(miss) => println!("  oj start: server-fn resolver cache miss ({miss})"),
    }
    let count = crate::start_bundle::write_server_fn_resolver(root, cache, dev)
        .map_err(|e| anyhow::anyhow!("server-fn resolver: {e}"))?;
    eprintln!(
        "{}: server-fn resolver ({count} function(s))",
        oj_server::oj_brand()
    );
    // The scan just written names the modules this resolver depends on.
    store.persist(&server_fn_inputs(root, &reads_dest), &outputs);
    Ok(false)
}

fn same_files(a: &[PathBuf], b: &[PathBuf]) -> bool {
    let set = |files: &[PathBuf]| {
        files
            .iter()
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
    };
    set(a) == set(b)
}

/// A line of the reads file.
enum ScanLine<'a> {
    /// A module read, app-relative.
    Read(&'a str),
    /// A relative import asked for, app-relative as written.
    Asked(&'a str),
    /// An import that is not relative and that nothing answered.
    Unresolved { from: &'a str, specifier: &'a str },
}

fn scan_lines(reads: &str) -> impl Iterator<Item = ScanLine<'_>> {
    reads.lines().filter_map(|line| {
        if let Some(base) = line.strip_prefix(crate::start_bundle::SERVER_FN_ASKED) {
            Some(ScanLine::Asked(base))
        } else if let Some(import) = line.strip_prefix(crate::start_bundle::SERVER_FN_UNRESOLVED) {
            let (from, specifier) = import.split_once('\t')?;
            Some(ScanLine::Unresolved { from, specifier })
        } else {
            Some(ScanLine::Read(line))
        }
    })
}

/// The files keying the server-fn resolver: the source files under `src/`,
/// the modules the scan recorded in `reads_file` read wherever they are (a
/// `.js` or `.mts` module, one outside `src/`), and the file that answers
/// each import the scan asked for as of now: one created where the scan
/// found none, or one that takes another's place (a `barrel.js` beside the
/// `barrel.ts` read). With no reads file yet the key is one no resolver was
/// kept under, or one whose own reads file gives other inputs.
fn server_fn_inputs(root: &Path, reads_file: &Path) -> Vec<PathBuf> {
    let mut inputs = list_src_ts_files(root);
    let reads = std::fs::read_to_string(reads_file).unwrap_or_default();
    // Built for an unresolved import alone.
    let modules = oj_compiler::server_fns::FsModules::new(root);
    for line in scan_lines(&reads) {
        let rel = match line {
            ScanLine::Read(rel) => Some(rel.to_string()),
            ScanLine::Asked(base) => oj_compiler::server_fns::resolve_relative(root, base),
            ScanLine::Unresolved { from, specifier } => modules.resolve(from, specifier),
        };
        if let Some(file) = rel.map(|rel| root.join(rel)).filter(|f| f.is_file()) {
            inputs.push(file);
        }
    }
    let nearer = nearer_configs(root, &inputs);
    inputs.extend(nearer);
    inputs
}

/// The package.json, tsconfig.json and jsconfig.json in the directories
/// between `root` and each of `files`, the root's own left to the store's
/// key: an import of a module is resolved through the nearest of each (a
/// package's `imports` and `exports`, a nested project's `paths`), so a
/// resolver kept under a key without them came back as it was when one of
/// them had changed what an import names.
fn nearer_configs(root: &Path, files: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs = std::collections::BTreeSet::new();
    for file in files {
        let mut dir = file.parent();
        while let Some(d) = dir {
            if d == root || !d.starts_with(root) || !dirs.insert(d.to_path_buf()) {
                break;
            }
            dir = d.parent();
        }
    }
    dirs.iter()
        .flat_map(|dir| {
            ["package.json", "tsconfig.json", "jsconfig.json"].map(|name| dir.join(name))
        })
        .filter(|file| file.is_file())
        .collect()
}

/// What a batch of changed paths is to the server-fn resolver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerFnTrigger {
    /// Nothing of the resolver's.
    None,
    /// The resolver may have changed, and only what it then holds says
    /// whether it did: a removed source file, a module the last scan read
    /// for an imported builder or one that could answer an import it asked
    /// for, a file that makes an import nothing answered resolve, a file an
    /// import's resolution reads (a tsconfig, package.json). The same
    /// resolver again changes nothing for the page: a temp file that came
    /// and went, a tsconfig saved as it was.
    Scan,
    /// A source file that may declare a server function was edited: the
    /// server's copy of it changed with it, whatever the resolver holds.
    Declares,
}

/// Whether a batch of changed paths may change the server-fn resolver.
#[cfg(test)]
fn server_fn_inputs_changed(root: &Path, cache: &Path, changed: &[PathBuf]) -> bool {
    server_fn_trigger(root, cache, changed) != ServerFnTrigger::None
}

/// A file Vite's `reloadOnTsconfigChange` reloads the page for: one named
/// `tsconfig.json`, in whatever directory, and no other (a
/// `tsconfig.app.json`, a `jsconfig.json`, a file a tsconfig extends).
fn is_tsconfig_json(path: &Path) -> bool {
    path.file_name().is_some_and(|name| name == "tsconfig.json")
}

/// Regenerates the dev resolver for a batch that may have changed it (or
/// the routes), answering whether the server functions changed for the
/// page: always for a declaring edit, and for a scan when the resolver it
/// leaves is another, or could not be made (what it holds is then unknown).
fn refresh_server_fn_resolver(
    root: &Path,
    cache: &Path,
    trigger: ServerFnTrigger,
    routes_changed: bool,
) -> bool {
    if trigger == ServerFnTrigger::None && !routes_changed {
        return false;
    }
    let resolver = cache.join(crate::start_bundle::server_fn_resolver_file(true));
    let before = std::fs::read(&resolver).ok();
    let made = generate_server_fn_resolver(root, cache, true);
    match trigger {
        ServerFnTrigger::Declares => true,
        ServerFnTrigger::Scan => made.is_err() || std::fs::read(&resolver).ok() != before,
        ServerFnTrigger::None => false,
    }
}

/// What `changed` is to the server-fn resolver ([`ServerFnTrigger`]): the
/// strongest of what each path is.
fn server_fn_trigger(root: &Path, cache: &Path, changed: &[PathBuf]) -> ServerFnTrigger {
    let reads =
        std::fs::read_to_string(cache.join(crate::start_bundle::server_fn_reads_file(true)))
            .unwrap_or_default();
    // Walked only for a JSON file not named as a tsconfig is.
    let configs = std::cell::OnceCell::new();
    // The imports nothing answered, asked again: once for the batch, and
    // only when it holds a module.
    let now_resolved = std::cell::OnceCell::new();
    // The resolver as it stands, for the files it imports.
    let resolver = std::cell::OnceCell::new();
    let resolver = || {
        resolver.get_or_init(|| {
            std::fs::read_to_string(cache.join(crate::start_bundle::server_fn_resolver_file(true)))
                .unwrap_or_default()
        })
    };
    let spelled = Spelling::new(root);
    // The modules read by their real paths, for a changed path the watcher
    // names another way than the import did (through a symlinked directory
    // of the app, in the letter case the disk has).
    let real_reads = std::cell::OnceCell::new();
    let any_read = scan_lines(&reads).any(|line| matches!(line, ScanLine::Read(_)));
    let mut trigger = ServerFnTrigger::None;
    for p in changed {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let ext = p.extension().and_then(|e| e.to_str()).unwrap_or_default();
        let source = matches!(ext, "ts" | "tsx");
        let rel = spelled.relative(p);
        let is_read = || {
            scan_lines(&reads).any(|line| matches!(line, ScanLine::Read(read) if read == rel))
                || any_read
                    && is_module(ext)
                    && spelled.real(p).is_some_and(|real| {
                        real_reads
                            .get_or_init(|| {
                                scan_lines(&reads)
                                    .filter_map(|line| match line {
                                        ScanLine::Read(read) => {
                                            std::fs::canonicalize(root.join(read)).ok()
                                        }
                                        _ => None,
                                    })
                                    .collect::<std::collections::HashSet<_>>()
                            })
                            .contains(&real)
                    })
        };
        // A source file, or a module of another kind the scan read for a
        // builder (`authed.mts`, a `.js` one), that may declare one.
        if (source || is_module(ext) && is_read())
            && std::fs::read_to_string(p).is_ok_and(|s| oj_compiler::server_fns::may_declare(&s))
        {
            return ServerFnTrigger::Declares;
        }
        if trigger == ServerFnTrigger::Scan {
            continue;
        }
        // A JSON file a config names, present or deleted (one it extended):
        // a temp an editor's safe-write or GNU sed put beside the sources
        // and removed before the batch settled is named by nothing, and is
        // none of the resolver's.
        let config = is_resolution_config(name)
            || (ext == "json"
                && configs
                    .get_or_init(|| {
                        named_resolution_configs(root)
                            .iter()
                            .map(|config| spelled.relative(config))
                            .collect::<Vec<_>>()
                    })
                    .contains(&rel));
        let read = || {
            is_read()
                || scan_lines(&reads)
                    .any(|line| matches!(line, ScanLine::Asked(base) if answers_import(base, &rel)))
        };
        let resolves = || {
            is_module(ext)
                && *now_resolved.get_or_init(|| {
                    let modules = oj_compiler::server_fns::FsModules::new(root);
                    scan_lines(&reads).any(|line| match line {
                        ScanLine::Unresolved { from, specifier } => {
                            modules.resolve(from, specifier).is_some()
                        }
                        _ => false,
                    })
                })
        };
        // A removed source file may have declared one.
        let removed = || source && !p.exists();
        // A source file the resolver imports that declares none any more
        // (its last server function was taken out): the resolver still
        // names what it had.
        let imported = || {
            source
                && resolver().contains(&format!(
                    "import({})",
                    crate::start_bundle::server_fn_module(cache, &root.join(&rel))
                ))
        };
        // A directory: the watcher may name it alone for the files that
        // came or went with it (a rename, `rm -r`). One under `src/` may
        // hold source files; one that is gone held what the scan read or
        // the resolver imports under it.
        let directory = || {
            if p.is_dir() {
                rel == "src" || rel.starts_with("src/")
            } else if p.exists() {
                false
            } else {
                let under = format!("{rel}/");
                let module = crate::start_bundle::server_fn_module(cache, &root.join(&rel));
                let imports = format!("import({}/", module.trim_end_matches('"'));
                scan_lines(&reads)
                    .any(|line| matches!(line, ScanLine::Read(read) if read.starts_with(&under)))
                    || resolver().contains(&imports)
            }
        };
        if config || read() || resolves() || removed() || imported() || directory() {
            trigger = ServerFnTrigger::Scan;
        }
    }
    trigger
}

/// How the watcher's paths are matched with the reads file's: see
/// [`Spelling::relative`]. Real paths are asked for once per directory, so
/// a large batch (a checkout) costs one lookup for each directory it
/// touches.
struct Spelling<'a> {
    root: &'a Path,
    real_root: std::cell::OnceCell<Option<PathBuf>>,
    dirs: std::cell::RefCell<std::collections::HashMap<PathBuf, Option<PathBuf>>>,
}

impl<'a> Spelling<'a> {
    fn new(root: &'a Path) -> Self {
        Self {
            root,
            real_root: std::cell::OnceCell::new(),
            dirs: Default::default(),
        }
    }

    /// `path` by its real path: its directory's, since the file may be gone.
    fn real(&self, path: &Path) -> Option<PathBuf> {
        let (dir, name) = (path.parent()?, path.file_name()?);
        let mut dirs = self.dirs.borrow_mut();
        let real = match dirs.get(dir) {
            Some(real) => real.clone(),
            None => {
                let real = std::fs::canonicalize(dir).ok();
                dirs.insert(dir.to_path_buf(), real.clone());
                real
            }
        };
        Some(real?.join(name))
    }

    /// `path` as the reads file names a file: relative to the root with `/`
    /// between its parts, or whole when it is not under it. The watcher may
    /// name a file another way than the root is written (the root reached
    /// through a symlink, `/var` for `/private/var`, a Windows path with or
    /// without its `\\?\` prefix), so a path that is not under the root as
    /// written is tried again by its real path.
    fn relative(&self, path: &Path) -> String {
        let shown = |rel: &Path| rel.to_string_lossy().replace('\\', "/");
        if let Ok(rel) = path.strip_prefix(self.root) {
            return shown(rel);
        }
        let real_root = self
            .real_root
            .get_or_init(|| std::fs::canonicalize(self.root).ok());
        let under = |path: &Path| {
            path.strip_prefix(self.root)
                .ok()
                .or_else(|| path.strip_prefix(real_root.as_deref()?).ok())
                .map(shown)
        };
        self.real(path)
            .as_deref()
            .and_then(under)
            .or_else(|| under(path))
            .unwrap_or_else(|| shown(path))
    }
}

/// [`Spelling::relative`], for one path.
#[cfg(test)]
fn app_relative(root: &Path, path: &Path) -> String {
    Spelling::new(root).relative(path)
}

/// An extension an import of the app's may be answered by: Vite's
/// `DEFAULT_EXTENSIONS` less `.json`, which declares nothing, and the
/// CommonJS pair, which an import names in full (`./x.cjs`, answered by
/// `x.cts` too).
fn is_module(ext: &str) -> bool {
    matches!(
        ext,
        "mjs" | "js" | "mts" | "ts" | "jsx" | "tsx" | "cjs" | "cts"
    )
}

/// Whether the file at `rel` could answer a relative import written as
/// `base` (both app-relative): the file itself, or a module that is `base`
/// with an extension, a `.js` import's TypeScript source, or the
/// directory's index. A file of another kind beside it (`authed.json`,
/// `authed.css`) answers no import of `./authed`.
fn answers_import(base: &str, rel: &str) -> bool {
    if rel == base {
        return true;
    }
    let stem = |path: &str| -> String {
        let cut = path.rfind('.').filter(|dot| !path[*dot..].contains('/'));
        path[..cut.unwrap_or(path.len())].to_string()
    };
    let ext = rel
        .rsplit_once('.')
        .map_or("", |(_, ext)| ext)
        .trim_end_matches('/');
    let file = stem(rel);
    is_module(ext) && (file == base || file == format!("{base}/index") || file == stem(base))
}

/// A file name that decides how an import resolves: the tsconfig files
/// (`paths`) and package.json (`imports`).
fn is_resolution_config(name: &str) -> bool {
    name == "package.json"
        || name == "jsconfig.json"
        || (name.starts_with("tsconfig") && name.ends_with(".json"))
}

/// The tsconfig files an import of the app may resolve through, sorted: the
/// ones at the root and under `src/` (the config nearest the importer owns
/// it), and the files those name by a relative `extends` or a `references`
/// path, whatever they are called and wherever they are. A config extended
/// from a package is keyed by the lockfile.
fn resolution_configs(root: &Path) -> Vec<PathBuf> {
    resolution_configs_with(root, false)
}

/// [`resolution_configs`] and, besides, every file a config names by a
/// relative `extends` or `references` path whether or not it exists (as the
/// file itself, as `<name>.json`, as `<dir>/tsconfig.json`): the set a
/// deleted JSON file is checked against, since the file a config extended is
/// gone by then.
fn named_resolution_configs(root: &Path) -> Vec<PathBuf> {
    resolution_configs_with(root, true)
}

fn resolution_configs_with(root: &Path, with_missing: bool) -> Vec<PathBuf> {
    fn named(dir: &Path, deep: bool, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let p = e.path();
            let name = e.file_name();
            let name = name.to_string_lossy();
            if p.is_dir() {
                if deep && name != "node_modules" {
                    named(&p, deep, out);
                }
            } else if name != "package.json" && is_resolution_config(&name) {
                out.push(p);
            }
        }
    }
    let mut pending = Vec::new();
    let mut missing = Vec::new();
    named(root, false, &mut pending);
    named(&root.join("src"), true, &mut pending);
    static NAMED: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#""(?:extends|path)"\s*:\s*(?:\[\s*)?((?:"[^"]*"\s*,?\s*)+)"#).unwrap()
    });
    static QUOTED: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r#""([^"]*)""#).unwrap());
    let mut out: Vec<PathBuf> = Vec::new();
    while let Some(config) = pending.pop() {
        if out.contains(&config) {
            continue;
        }
        let text = std::fs::read_to_string(&config).unwrap_or_default();
        let dir = config.parent().unwrap_or(root).to_path_buf();
        out.push(config);
        for list in NAMED.captures_iter(&text) {
            for target in QUOTED.captures_iter(&list[1]) {
                let target = &target[1];
                if !target.starts_with('.') {
                    continue;
                }
                let path = crate::ssr_dev::normalize_path(&dir.join(target));
                let candidates = [
                    path.clone(),
                    PathBuf::from(format!("{}.json", path.display())),
                    path.join("tsconfig.json"),
                ];
                let found = candidates.iter().find(|candidate| candidate.is_file());
                if with_missing {
                    missing.extend(candidates.iter().filter(|c| !c.is_file()).cloned());
                }
                pending.extend(found.cloned());
            }
        }
    }
    if with_missing {
        for path in missing {
            if !out.contains(&path) {
                out.push(path);
            }
        }
    }
    out.sort();
    out
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

/// The client bundle's store, keyed by what the bundle depends on beyond its
/// closure: the route trees, the config and its imports, the defines its
/// code inlines, and whether it is built for hot updates (the Fast Refresh
/// plugins and the engine's runtime are in a hot bundle alone).
fn keyed_bundle_store(
    bundler: &ClientBundler,
    root: &Path,
    mode: &str,
) -> oj_cache::start_bundle::StartBundleStore {
    start_bundle_store(root, mode)
        .keyed_by_files(&bundler.cache_key_files())
        .keyed_by("defines", &bundler.cache_key_defines())
        .keyed_by("hot", if bundler.hot_bundle() { "1" } else { "0" })
}

fn bundle_client_entry_cached(
    bundler: &ClientBundler,
    cache: &Path,
    mode: &str,
) -> anyhow::Result<oj_cache::start_bundle::PinnedBundle> {
    let root = bundler.root();
    let _ = std::fs::remove_file(cache.join(CLIENT_ERROR_FILE));
    let store = keyed_bundle_store(bundler, root, mode);
    let stylex = bundler.stylex();
    match store.restore(cache, |scopes| restorable_glob_members(scopes, cache)) {
        // A restored bundle never ran the StyleX transform: its modules'
        // rules come from the sidecar saved with it, or it is rebuilt.
        Ok((stats, _))
            if stylex
                .is_some_and(|p| !crate::stylex_plugin::restore_rules(p, cache, &stats.key)) =>
        {
            println!("  oj start: client bundle cache miss (no StyleX rules saved for it)")
        }
        Ok((stats, _)) if has_generated_sheets(cache, &stats.key) => println!(
            "  oj start: client bundle cache miss (a plugin generates a stylesheet from its modules)"
        ),
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
    if !bundler.cacheable() {
        println!("  oj start: client bundle not cached (built without the app's plugin host)");
    } else if let Some((key, pinned)) = store.persist(cache) {
        save_stylex_rules(bundler, cache, &key);
        save_generated_sheets(bundler, cache, &key);
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
/// What the Start scripts (route codegen, the SSR runtime's `process.env`)
/// see: the app root and cache, NODE_ENV, and every var of the mode's `.env`
/// files, the shell winning, as TanStack Start's load-env plugin assigns them.
fn script_vars(env: &oj_env::Env, root: &Path, app_env: &oj_env::AppEnv) -> Vec<(String, String)> {
    let mut vars = vec![
        (
            "OJ_APP_ROOT".to_string(),
            root.to_string_lossy().into_owned(),
        ),
        (
            "OJ_CACHE_ROOT".to_string(),
            oj_cache::cache_root(root).to_string_lossy().into_owned(),
        ),
        ("NODE_ENV".to_string(), app_env.node_env().to_string()),
    ];
    vars.extend(crate::start_bundle::dotenv_process_vars(env, app_env));
    vars
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

#[cfg(test)]
fn classify(req: &Request, proxy_prefixes: &[String]) -> Route {
    classify_path(
        req.uri().path(),
        req.uri().path(),
        req.method(),
        proxy_prefixes,
    )
}

/// [`classify`] with `path` the request's path past the dev `base` (what
/// oj's own routes see) and `raw` the path as sent (what `server.proxy`
/// matches, as Vite's proxy runs ahead of its base middleware).
fn classify_path(path: &str, raw: &str, method: &Method, proxy_prefixes: &[String]) -> Route {
    // Only oj's own `/__` endpoints are the dev server's: Vite reserves no
    // `/__` prefix, so an app route or a plugin middleware page under it
    // goes through the middleware and SSR like any document.
    if path.starts_with("/@") || oj_server::is_dev_server_path(path) {
        return Route::Pass;
    }
    if proxy_prefixes.iter().any(|p| raw.starts_with(p.as_str())) {
        return Route::Pass;
    }
    // A dotted non-GET (`POST /api/export.csv`, `PUT /files/a.b`) can only be
    // the app's: static files and modules are GET-only, so it goes straight
    // to the SSR handler like every other request nothing else owns.
    let last = path.rsplit('/').next().unwrap_or("");
    if last != "index.html" && last.contains('.') {
        return if *method == Method::GET {
            Route::StaticOrDocument
        } else {
            Route::Api
        };
    }
    if proxy_prefixes.iter().any(|p| raw.starts_with(p.as_str())) {
        return Route::Pass;
    }
    match *method {
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

/// `path` with the dev `base` stripped (Vite's base middleware); a path
/// outside it is left as it is.
fn strip_base<'a>(base: &str, path: &'a str) -> &'a str {
    match path.strip_prefix(base) {
        Some(rest) if !base.is_empty() && rest.starts_with('/') => rest,
        _ => path,
    }
}

async fn start_route(State(state): State<Arc<StartState>>, req: Request, next: Next) -> Response {
    // oj's own urls answer under `base` too, as Vite's do; a document or a
    // server function is forwarded with its full url.
    let path = strip_base(&state.base, req.uri().path()).to_string();
    let path = path.as_str();
    if path == "/@oj-start/hmr" {
        let (mut parts, _) = req.into_parts();
        return match WebSocketUpgrade::from_request_parts(&mut parts, &()).await {
            Ok(ws) => {
                // The page's stamp (the changes its document had): a change it
                // missed while loading reloads it as soon as it connects.
                let epoch = state.reloads.page(parts.uri.query());
                // Subscribed before the replay read: an error landing between
                // the two arrives twice, never not at all.
                let errors = state.error_tx.subscribe();
                let replay = state
                    .build_error
                    .read()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .clone();
                // Hot updates reach a page unless a full-reload HMR gate
                // reloads every edit; a granular gate parks them for its flush
                // (HotUpdates::set_gate).
                let has = crate::start_hot::hot_change_in(parts.uri.query());
                let hot = state.bundler.hot_enabled().then(|| {
                    let weak = Arc::downgrade(&state);
                    HotPage {
                        has,
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
                let ping = state.hmr_ping;
                ws.protocols(["oj-hmr"]).on_upgrade(move |socket| {
                    start_hmr_socket(socket, epoch, errors, replay, hot, Some(events), ping)
                })
            }
            Err(e) => e.into_response(),
        };
    }
    if path == "/@oj-start/live-reload.js" {
        return match tokio::fs::read_to_string(&state.live_reload).await {
            Ok(js) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                render_live_reload(
                    &js,
                    state.hmr_overlay,
                    &state.base,
                    crate::start_reloads::stamp_in(req.uri().query()),
                    crate::start_hot::hot_change_in(req.uri().query()),
                    state.hmr_ping,
                ),
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
    if path == "/@oj-start/error-overlay.js" {
        let overlay = state.live_reload.with_file_name("error-overlay.js");
        return serve_js(&overlay, "error overlay").await;
    }
    if let Some(name) = path.strip_prefix(crate::start_hot::PATCH_PREFIX) {
        return state.bundler.hot.serve(name);
    }
    if let Some(rest) = path.strip_prefix("/@oj-start/fs/") {
        return serve_fs_asset(&state, &format!("/{rest}")).await;
    }
    if let Some(rest) = path.strip_prefix(crate::start_bundle::assets::DEV_ID_BASE) {
        let Some(id) = served_sheet_id(rest) else {
            return (
                StatusCode::FORBIDDEN,
                "oj start: not a plugin-served stylesheet",
            )
                .into_response();
        };
        return match css_body(&state, Path::new(&id)).await {
            Some(body) => css_response(body),
            None => (StatusCode::NOT_FOUND, "oj start: no such stylesheet").into_response(),
        };
    }
    if let Some(name) = path.strip_prefix("/@oj-start/") {
        return serve_client_chunk(&state, req.headers(), name).await;
    }
    if path.starts_with("/_serverFn/") {
        return forward_with_body(&state, req).await;
    }
    // A WebSocket upgrade is never a document: the inner app owns them all
    // (vite-hmr endpoint, `ws: true` proxies, plugin `upgrade` listeners).
    if oj_server::is_websocket_upgrade(req.headers()) {
        return next.run(req).await;
    }
    let route = classify_path(path, req.uri().path(), req.method(), &state.proxy_prefixes);
    // A file under `publicDir` answers before the router and the SSR handler
    // see the request, as Vite's servePublicMiddleware runs ahead of its html
    // middlewares for every appType: `/hub/index.html` is that file, not the
    // `/hub/` document (which the router would redirect or 404), and a
    // dotted or extensionless public path is never the app's 404. Not for a
    // proxied prefix (`Route::Pass`): Vite's proxy middleware comes before
    // its public one.
    if !matches!(route, Route::Pass) {
        if let Some(hit) = public_file(
            &state,
            req.uri().path(),
            path,
            req.uri().query(),
            req.method(),
        )
        .await
        {
            // A plugin's configureServer middleware comes before the public
            // one in Vite: where the plugin middleware is asked first (a
            // document), it still is.
            if matches!(route, Route::Document) && state.plugin_serve.mw_port().is_some() {
                let raw = path_and_query(&req);
                return forward_document(&state, raw, req.headers(), Some(hit)).await;
            }
            return public_response(&state, &hit, req.method(), req.headers()).await;
        }
    }
    match route {
        Route::Document => {
            let raw = path_and_query(&req);
            forward_document(&state, raw, req.headers(), None).await
        }
        Route::StaticOrDocument => {
            let raw = path_and_query(&req);
            let headers = req.headers().clone();
            let resp = next.run(req).await;
            if resp.status() != StatusCode::NOT_FOUND {
                return resp;
            }
            forward_document(&state, raw, &headers, None).await
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
    /// The hot changes the page says it has (`h` on its socket's url).
    has: Option<u64>,
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

/// Sent on connect to a page whose document is older than the published
/// changes (see start_reloads). A client that knows no such frame reloads.
const BEHIND_FRAME: &str = r#"{"type":"oj:behind"}"#;
/// [`BEHIND_FRAME`] for a hot change the page missed.
const HOT_BEHIND_FRAME: &str = r#"{"type":"oj:behind","hot":true}"#;
/// Sent to every page socket each [`StartState::hmr_ping`] while it is open.
/// The page's live-reload client treats any frame as proof the socket is
/// alive and replaces one that stays silent for longer than a few pings: a
/// connection dropped on the way (a proxy, a load balancer, a sleeping
/// laptop) without a close never fires the browser's `onclose`, so the page
/// would otherwise keep a dead socket, and miss every change, until a manual
/// refresh. The keepalive also keeps idle-timeout middleboxes from cutting it.
/// A text frame, as page scripts never see the protocol's own pings.
const PING_FRAME: &str = r#"{"type":"oj:ping"}"#;
/// The close reason a page socket gets when its page's end stopped
/// answering pings, as engine.io's "ping timeout" and Phoenix's "heartbeat
/// timeout".
const PING_TIMEOUT_REASON: &str = "oj: ping timeout";

async fn start_hmr_socket(
    mut socket: WebSocket,
    mut epoch: crate::start_reloads::PageEpoch,
    mut errors: broadcast::Receiver<String>,
    replay: Option<String>,
    mut hot: Option<HotPage>,
    mut events: Option<PageEvents>,
    ping: Option<Duration>,
) {
    use crate::start_hot::PageMessage;
    // Each tick also sends the protocol's own ping, which the browser's
    // network stack answers by itself: a page paused at a breakpoint or in
    // an alert() still answers, where an answer from its scripts (engine.io's
    // pong, Phoenix's heartbeat) would stop. So a socket that hears nothing
    // from the page's end for the deadline lost it on the way: close it, as
    // Socket.IO does a missed pong and Phoenix 60s without data. The page's
    // hot client then leaves the bundler (`page_gone` below) instead of
    // staying listed until TCP gives up, minutes later.
    let deadline = ping.map(silent_deadline);
    let mut heard = tokio::time::Instant::now();
    // The first ping one interval in: the page's deadline runs from its
    // connect, so it hears in time, and an answer from it starts this
    // end's clock afresh.
    let mut ping = oj_server::ping_ticks(ping);
    // A change went out while the page loaded, before this socket could
    // take it. Its own frame, so the client can tell a document that keeps
    // coming back behind (served from a cache) from a new change.
    if epoch.behind()
        && socket
            .send(Message::Text(BEHIND_FRAME.into()))
            .await
            .is_err()
    {
        return;
    }
    // The same for a hot change: a frame goes once, to the pages connected
    // then, so one that went out while this page loaded or while its socket
    // was down (subscribed above, so none is missed from here on) leaves it
    // behind for good unless it reloads now.
    if hot.as_ref().is_some_and(|h| h.bundler.hot.behind(h.has))
        && socket
            .send(Message::Text(HOT_BEHIND_FRAME.into()))
            .await
            .is_err()
    {
        return;
    }
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
            () = oj_server::ping_due(&mut ping) => {
                if deadline.is_some_and(|d| heard.elapsed() > d) {
                    // The page's end is gone: say why, but never wait on a dead
                    // connection to take it (engine.io tears its transport
                    // down the same way).
                    println!("  oj start: a page stopped answering pings; closed its socket");
                    let close = Message::Close(Some(axum::extract::ws::CloseFrame {
                        code: axum::extract::ws::close_code::NORMAL,
                        reason: PING_TIMEOUT_REASON.into(),
                    }));
                    let _ = tokio::time::timeout(Duration::from_secs(1), socket.send(close)).await;
                    break;
                }
                if !send_within(&mut socket, Message::Ping(Default::default()), deadline).await {
                    break;
                }
                if !send_within(&mut socket, Message::Text(PING_FRAME.into()), deadline).await {
                    break;
                }
            },
            custom = custom => match custom {
                Ok(text) if page_custom_frame(&text) => {
                    if !send_within(&mut socket, Message::Text(text.into()), deadline).await {
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
                    if !send_within(&mut socket, Message::Text(frame.into()), deadline).await {
                        break;
                    }
                }
                // A lagged receiver missed older failures; the newest one
                // still arrives (or the reload that clears them).
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            behind = epoch.fell_behind() => match behind {
                None => break,
                // A count, not a queue: however many changes went out, the
                // page reloads once onto the newest.
                Some(()) => {
                    if !send_within(&mut socket, Message::Text("reload".into()), deadline).await {
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
                if !send_within(&mut socket, Message::Text(text.into()), deadline).await {
                    break;
                }
            },
            incoming = socket.recv() => {
                // Any frame from the page's end shows it alive, the
                // browser's pong to our protocol ping included.
                if matches!(incoming, Some(Ok(_))) {
                    heard = tokio::time::Instant::now();
                }
                match incoming {
                    None | Some(Err(_)) => break,
                    // The page closed it: answer, but never wait long on it.
                    Some(Ok(Message::Close(_))) => {
                        let _ = tokio::time::timeout(Duration::from_secs(1), socket.close()).await;
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
                            // Said again on the same socket, the same page: no change.
                            Some(PageMessage::Client(id)) if page.as_deref() == Some(id.as_str()) => {}
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
                }
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

/// `public`: the `publicDir` file the url names, when there is one. It is
/// served instead of the document unless the plugin middleware answers the
/// url or rewrites it (Vite runs a plugin's configureServer middlewares
/// before its public one), and it does not wait for the client output.
async fn forward_document(
    state: &Arc<StartState>,
    raw: String,
    headers: &header::HeaderMap,
    mut public: Option<PublicHit>,
) -> Response {
    let hot_has = hot_has_now(state);
    // A document loads the client output: never one from before a hot update.
    if public.is_none() {
        state.client_output_fresh().await;
    }
    // Editor plugins (dev-server bridge) register configureServer routes
    // with no path prefix, so a GET like /_sandbox/preview/viewers can only
    // be told from an app route by asking the middleware first; it returns
    // x-oj-fallthrough when it does not own the path, and then we SSR.
    // The changes the document has, stamped before it renders: one that goes
    // out while it loads reaches its socket on connect.
    let mut raw = raw;
    if let Some(port) = state.plugin_serve.mw_port() {
        let stamp = document_stamp(state, hot_has);
        match oj_server::forward_get_to_plugin_mw(port, &raw, headers).await {
            oj_server::Forwarded::Answered(resp) => {
                if public.is_some() {
                    state.client_output_fresh().await;
                }
                // A worker-served document needs the live-reload client like the
                // runner-served ones, or edits never reload the page.
                return inject_reload_client(resp, &state.base, &stamp, state.csp_nonce.as_deref());
            }
            // A middleware rewrote `req.url` and passed the request on: the
            // document it named is the one to render, as Vite's own
            // middlewares after it would serve that url.
            oj_server::Forwarded::FellThrough {
                rewritten: Some(target),
            } => {
                raw = target
                    .path_and_query()
                    .map(|p| p.as_str().to_string())
                    .unwrap_or_else(|| target.path().to_string());
                // The url the middleware left is the one to serve: no longer
                // the public file's.
                if public.take().is_some() {
                    state.client_output_fresh().await;
                }
            }
            oj_server::Forwarded::FellThrough { rewritten: None } => {}
        }
    }
    if let Some(hit) = public {
        return public_response(state, &hit, &Method::GET, headers).await;
    }
    ensure_bundle_current(state).await;
    ensure_runner_fresh(state).await;
    forward(
        state,
        &document_stamp(state, hot_has),
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
    let stamp = document_stamp(state, hot_has_now(state));
    // A configureServer middleware may claim the request first; since a
    // streamed body cannot be replayed, the plugin host pipes an unclaimed
    // request on to the engine's loopback shim (x-oj-forward-to) instead of
    // falling through. Without a middleware the request body is read whole
    // and handed to the engine (the call takes it as one argument); the
    // response body streams back on either path.
    let Some(port) = state.plugin_serve.mw_port() else {
        let body = match read_body_capped(req.into_body(), start_max_body_bytes()).await {
            Ok(body) => body,
            Err(resp) => return resp,
        };
        return forward(state, &stamp, method, url, &headers, body).await;
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
        Ok(resp) => inject_reload_client(resp, &state.base, &stamp, state.csp_nonce.as_deref()),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj start: {e}")).into_response(),
    }
}

/// Tells the open pages that `name`, which one of them just asked for, is
/// not a chunk the dev server can serve any more: the one that asked reloads
/// (see [`crate::start_chunks::chunk_gone_frame`]). Said of any name the
/// served bundles do not hold, not only of ones they once held: a page left
/// open across a restart of the dev server names chunks this run never had,
/// and its socket is not reloaded on connect (a stamp from another run
/// counts as current, see start_reloads), so this is what brings it back. A
/// page that asks for a name nothing ever had reloads once for it and stays.
/// A source map is asked for by the browser's tools, not by a page.
fn chunk_gone(pages: &broadcast::Sender<String>, name: &str) {
    if !name.ends_with(".map") {
        let _ = pages.send(crate::start_chunks::chunk_gone_frame(name));
    }
}

async fn serve_client_chunk(
    state: &StartState,
    req: &axum::http::HeaderMap,
    name: &str,
) -> Response {
    let name = percent_decode(name);
    let served = state
        .bundle
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .chunk(&name);
    let Some(served) = served else {
        chunk_gone(&state.error_tx, &name);
        return (
            StatusCode::NOT_FOUND,
            format!("oj start: {name} is not in the client bundle manifest"),
        )
            .into_response();
    };
    let chunk = served.chunk;
    let pages = state.error_tx.clone();
    let expected = oj_cache::integrity::ExpectedFile {
        size: chunk.size,
        hash: chunk.hash.clone().unwrap_or_default(),
    };
    let path = chunk.path.clone();
    let what = name.clone();
    let retained = !served.current;
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
            // An earlier bundle's chunk file the store has swept (or a build
            // dir the next bundle rewrote): the page is too old to serve,
            // and that is a 404, not a server fault.
            Ok(Err(oj_cache::integrity::VerifyError::Io(e)))
                if retained && e.kind() == std::io::ErrorKind::NotFound =>
            {
                chunk_gone(&pages, &what);
                Err((
                    StatusCode::NOT_FOUND,
                    format!("oj start: {what} is from an earlier bundle and no longer on disk; reload the page"),
                )
                    .into_response())
            }
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
    let immutable = !served.entry;
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
fn render_live_reload(
    js: &str,
    overlay: bool,
    url_base: &str,
    stamp: Option<String>,
    hot_has: Option<u64>,
    ping: Option<Duration>,
) -> String {
    js.replace(
        "__HMR_ENABLE_OVERLAY__",
        if overlay { "true" } else { "false" },
    )
    .replace(
        "__OJ_DEV_BASE__",
        &serde_json::Value::String(url_base.to_string()).to_string(),
    )
    .replace(
        "__OJ_PAGE_STAMP__",
        &stamp.map_or_else(
            || "null".to_string(),
            |s| serde_json::Value::String(s).to_string(),
        ),
    )
    .replace(
        "__OJ_HOT_CHANGE__",
        &hot_has.map_or_else(|| "null".to_string(), |h| h.to_string()),
    )
    // The keepalive interval the client's watchdog counts on; 0 turns it off.
    .replace(
        "__OJ_PING_MS__",
        &ping.map_or(0, |p| p.as_millis()).to_string(),
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
/// The client environment's plugins see it as the sheet's own id, as under
/// TanStack Start's dev styles on Vite, which take a sheet's CSS from the
/// client's transform of it: "pre" transforms on the source, "normal" ones
/// on the sheet (the "post" ones see the module the client bundle imports).
async fn css_body(state: &StartState, file: &Path) -> Option<Result<String, String>> {
    // The client bundle ran this sheet through the plugins already.
    if let Some(css) = state.bundler.linked_sheet(file) {
        return Some(Ok(css));
    }
    let host = state.bundler.plugin_host().await;
    let id = file.display().to_string();
    let slots = host
        .as_deref()
        .map(|host| oj_server::StyleSlots { host, id: &id });
    let pipeline = oj_css::is_css_module(&file.to_string_lossy())
        || file.extension().and_then(|e| e.to_str()) != Some("css");
    let src = if pipeline {
        match state.ssr.style_sheet(file, slots).await {
            Ok(m) => m.css,
            Err(e) => return Some(Err(e)),
        }
    } else {
        let served = slots.is_some_and(|s| s.host.hook_wants_load(s.id));
        match oj_server::read_style_source(slots, file).await {
            // The plugin still loads what the bundle ran through the plugins.
            Ok(src) if !file.is_file() => match state.bundler.linked_served_sheet(file, &src) {
                Some(css) => return Some(Ok(css)),
                None => match oj_server::style_slot(slots, src, "pre").await {
                    Ok(src) => src,
                    Err(e) => return Some(Err(e)),
                },
            },
            Ok(src) => match oj_server::style_slot(slots, src, "pre").await {
                Ok(src) => src,
                Err(e) => return Some(Err(e)),
            },
            // A plugin's load failing is its error to show, as Vite's 500.
            Err(e) if served => return Some(Err(e)),
            Err(_) => return None,
        }
    };
    let compiled = match state.css_host.as_ref().filter(|_| needs_css_compile(&src)) {
        Some(host) => compile_css(host, &src, file).await,
        None => None,
    };
    if compiled.is_none() && !pipeline && slots.is_none() {
        return None;
    }
    let css = compiled.unwrap_or(src);
    Some(oj_server::style_slot(slots, css, "normal").await)
}

/// The id a `/@oj-start/id/` request names, when it is a sheet a plugin
/// serves with no file behind it. Files go through `/@oj-start/fs/` and its
/// workspace check, so this route never reads one.
fn served_sheet_id(rest: &str) -> Option<String> {
    let id = crate::start_bundle::assets::id_of_dev_url(&percent_decode(rest));
    let module_id = id.starts_with('\0') || id.starts_with('/');
    (module_id && crate::start_bundle::host::is_served_stylesheet(&id)).then_some(id)
}

fn css_response(body: Result<String, String>) -> Response {
    match body {
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
    if canon
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(oj_server::is_style_ext)
    {
        if let Some(body) = css_body(state, &canon).await {
            return css_response(body);
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

/// The file under `public_dir` that `path` names, or None when the request
/// is not for one. Vite's servePublicMiddleware: the exact path of a file
/// in the dir, decoded as `decodeURI` does (its `toFilePath`), with no
/// extension guessing and no `index.html` for a directory (sirv with
/// `extensions: []`), skipping oj's own `/@` urls (its `isInternalRequest`)
/// and the `?url` and `?import` requests whose module the transform
/// pipeline makes (`urlRE`, `isImportRequest`: `/logo.svg?url` is a module).
/// Only a path of plain segments names a file: nothing that could leave the
/// dir, on any platform.
fn public_file_path(
    public_dir: &Path,
    path: &str,
    query: Option<&str>,
    method: &Method,
) -> Option<PathBuf> {
    if *method != Method::GET && *method != Method::HEAD {
        return None;
    }
    if path.starts_with("/@") || oj_server::is_dev_server_path(path) {
        return None;
    }
    // Vite's urlRE is `(\?|&)url(?:&|$)`, its importQueryRE
    // `(\?|&)import=?(?:&|$)`.
    if query.is_some_and(|q| {
        q.split('&')
            .any(|flag| flag == "url" || flag == "import" || flag == "import=")
    }) {
        return None;
    }
    // A malformed escape leaves the path as it was written (Vite catches
    // decodeURI's error), which then names a file only literally.
    let decoded = if path.contains('%') {
        decode_uri(path).unwrap_or_else(|| path.to_string())
    } else {
        path.to_string()
    };
    let rel = decoded.strip_prefix('/')?;
    if rel.is_empty()
        || rel
            .split('/')
            .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return None;
    }
    // A drive, a root or a separator the platform reads inside a segment.
    let plain = Path::new(rel)
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !plain || rel.contains('\\') {
        return None;
    }
    Some(public_dir.join(rel))
}

/// JavaScript's `decodeURI`: every escape decoded but those of the reserved
/// characters (`; / ? : @ & = + $ , #`), which stay as written, so `%2F` is
/// never a path separator. None for what `decodeURI` throws on: an escape
/// that is not two hex digits, or bytes that are not UTF-8.
fn decode_uri(s: &str) -> Option<String> {
    const RESERVED: &[u8] = b";/?:@&=+$,#";
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'%' {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        let byte = hex_val(*bytes.get(i + 1)?)? << 4 | hex_val(*bytes.get(i + 2)?)?;
        if RESERVED.contains(&byte) {
            out.extend_from_slice(&bytes[i..i + 3]);
        } else {
            out.push(byte);
        }
        i += 3;
    }
    String::from_utf8(out).ok()
}

/// A `publicDir` file a request names: where it is and its metadata.
struct PublicHit {
    file: PathBuf,
    meta: std::fs::Metadata,
}

/// The `publicDir` file the request is for, if it names one: see
/// [`public_file_path`]. `raw` is the request's path, `path` the same past
/// the dev `base`. Vite's base middleware comes before its public one, so
/// with a `base` only a url under it names a public file. Vite matches the
/// url against the names in the dir, so a name in another case is no match
/// even where the file system would open it.
async fn public_file(
    state: &StartState,
    raw: &str,
    path: &str,
    query: Option<&str>,
    method: &Method,
) -> Option<PublicHit> {
    let public = state.public_dir.as_deref()?;
    if !state.base.is_empty() && raw.len() == path.len() {
        return None;
    }
    let file = public_file_path(public, path, query, method)?;
    let meta = tokio::fs::metadata(&file).await.ok()?;
    if !meta.is_file() || !named_as_written(public, &file).await {
        return None;
    }
    Some(PublicHit { file, meta })
}

/// Whether `file` (which exists, under `public`) is spelled as its name on
/// disk is. A case-insensitive file system opens `Hub/Index.html` for
/// `hub/index.html`; Vite's set of public file names does not hold it. A
/// name that resolves elsewhere altogether (a symlink) is the file asked for.
async fn named_as_written(public: &Path, file: &Path) -> bool {
    let (Ok(real_dir), Ok(real)) = (
        tokio::fs::canonicalize(public).await,
        tokio::fs::canonicalize(file).await,
    ) else {
        return true;
    };
    let (Ok(asked), Ok(found)) = (file.strip_prefix(public), real.strip_prefix(&real_dir)) else {
        return true;
    };
    let (asked, found) = (asked.to_string_lossy(), found.to_string_lossy());
    asked == found || asked.to_lowercase() != found.to_lowercase()
}

/// A `publicDir` file's response, as Vite's public middleware serves it:
/// the bytes as they are (never compiled, never given the live-reload
/// client), with sirv's type, validators and byte ranges
/// ([`oj_server::public_file_response`]) and `server.headers`, which its
/// sirv `setHeaders` puts on every file it serves.
async fn public_response(
    state: &StartState,
    hit: &PublicHit,
    method: &Method,
    headers: &header::HeaderMap,
) -> Response {
    let mut response = oj_server::public_file_response(&hit.file, &hit.meta, method, headers).await;
    for (name, value) in state.dev_headers.iter() {
        response.headers_mut().insert(name.clone(), value.clone());
    }
    response
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

/// The runner's reply as the HTTP response, its body streamed chunk by chunk
/// as the handler writes them (hyper flushes each one).
fn engine_response(resp: StartResponse) -> Response {
    let mut out = Response::new(axum::body::Body::from_stream(resp.body));
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

/// The request through the engine, the reply given the live-reload client
/// (with the document's stamp and the state's nonce, see
/// [`inject_reload_client`]).
async fn forward(
    state: &StartState,
    stamp: &str,
    method: String,
    url: String,
    req_headers: &header::HeaderMap,
    body: Option<Vec<u8>>,
) -> Response {
    match state
        .engine
        .handle(engine_request(method, url, req_headers, body, false))
        .await
    {
        Ok(resp) => inject_reload_client(
            engine_response(resp),
            &state.base,
            stamp,
            state.csp_nonce.as_deref(),
        ),
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
///
/// The tag carries a CSP nonce when the document has one: the nonce the
/// document's own script tags carry ([`ScriptNonceScan`]), which is the one
/// this request's policy header names (the app writes both from the same
/// request context), else `config_nonce` (Vite's `html.cspNonce`, which its
/// `injectNonceAttributeTagHook` in plugins/html.ts puts on the tags it
/// injects into a page, a static value the app's policy must name). Under a
/// strict policy (`script-src 'nonce-..' 'strict-dynamic'`) a tag without it
/// is refused; Vite's own client needs none there, since it arrives as an
/// import of the app's nonced client entry rather than as a tag of its own.
/// The app's tags are left as they are: Vite adds no nonce to a Start
/// document.
fn inject_reload_client(
    resp: Response,
    url_base: &str,
    stamp: &str,
    config_nonce: Option<&str>,
) -> Response {
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
    let tag = {
        let (url_base, stamp) = (url_base.to_string(), stamp.to_string());
        let config_nonce = config_nonce.map(str::to_string);
        move |document_nonce: Option<&str>| {
            reload_client(
                &url_base,
                &stamp,
                document_nonce.or(config_nonce.as_deref()),
            )
        }
    };
    // The tail is made once the body ended, when the scan has seen every tag.
    let stream = futures_util::stream::unfold(
        (
            body.into_data_stream(),
            ScriptNonceScan::default(),
            Some(tag),
        ),
        |(mut body, mut scan, tag)| async move {
            let tag = tag?;
            match body.next().await {
                Some(chunk) => {
                    if let Ok(bytes) = &chunk {
                        scan.feed(bytes);
                    }
                    Some((chunk, (body, scan, Some(tag))))
                }
                None => {
                    let tail = axum::body::Bytes::from(tag(scan.nonce()));
                    Some((Ok::<_, axum::Error>(tail), (body, scan, None)))
                }
            }
        },
    );
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
    fn start_build_out_dir_follows_vites_empty_out_dir_rule() {
        let base = tmp("start-out-dir");
        let root = base.join("app");
        let outside = base.join("shared");
        for dir in [root.join("dist"), outside.clone()] {
            std::fs::create_dir_all(dir.join(".git")).unwrap();
            std::fs::write(dir.join("keep.txt"), "x").unwrap();
        }
        let cfg = oj_config::BuildConfig::default();

        // Inside the root: emptied, `.git` kept.
        let dist = prepare_start_out_dir(&root, None, &cfg, false).unwrap();
        assert_eq!(dist, root.join("dist"));
        assert!(!dist.join("keep.txt").exists());
        assert!(dist.join(".git").is_dir());

        // Outside the root: left alone.
        prepare_start_out_dir(&root, Some(outside.clone()), &cfg, false).unwrap();
        assert!(
            outside.join("keep.txt").exists(),
            "an outside outDir was emptied"
        );

        // `--emptyOutDir` and `build.emptyOutDir: true` opt in.
        prepare_start_out_dir(&root, Some(outside.clone()), &cfg, true).unwrap();
        assert!(!outside.join("keep.txt").exists());
        std::fs::write(outside.join("keep.txt"), "x").unwrap();
        let opted = oj_config::BuildConfig {
            empty_out_dir: Some(true),
            out_dir: Some(outside.display().to_string()),
            ..Default::default()
        };
        prepare_start_out_dir(&root, None, &opted, false).unwrap();
        assert!(!outside.join("keep.txt").exists());

        // `build.emptyOutDir: false` keeps an inside outDir.
        std::fs::write(root.join("dist/keep.txt"), "x").unwrap();
        let kept = oj_config::BuildConfig {
            empty_out_dir: Some(false),
            ..Default::default()
        };
        prepare_start_out_dir(&root, None, &kept, false).unwrap();
        assert!(root.join("dist/keep.txt").exists());

        // The root itself is refused even when asked.
        std::fs::write(root.join("src.ts"), "x").unwrap();
        assert!(prepare_start_out_dir(&root, Some(root.clone()), &cfg, true).is_err());
        assert!(root.join("src.ts").exists());
    }

    #[test]
    fn only_a_full_reload_gate_turns_the_bundlers_hot_updates_off() {
        // No gate: edits apply in place.
        assert!(hot_under_gate(None));
        // A granular gate parks the hot updates for its flush instead of
        // reloading every edit.
        assert!(hot_under_gate(Some(false)));
        // A full-reload gate reloads on every release.
        assert!(!hot_under_gate(Some(true)));
        let bundler = ClientBundler::for_tests(Path::new("/app"));
        bundler.set_hot_enabled(true);
        // The bundle store keys a generation by the mode too: a development
        // client with hot updates on is a hot bundle, nothing else is.
        assert!(bundler.hot_bundle());
        bundler.set_hot_enabled(false);
        assert!(!bundler.hot_enabled());
        assert!(!bundler.hot_bundle());
        // A fresh bundler takes the gate from the config (the knob or the
        // app's `hmr-gate` plugin, which extraction records as
        // `server.hmrGate`), so the boot bundle and its cache key already
        // know the mode: with the default full-reload gate, not hot.
        let mut start = crate::start_bundle::dev::tests::start_at(Path::new("/app"));
        start
            .config
            .server
            .get_or_insert_with(Default::default)
            .hmr_gate = Some(true);
        let gated = ClientBundler::new(start, None);
        assert_eq!(
            gated.hot_enabled(),
            !oj_env::get().knobs.hmr_full_reload,
            "a config gate resolves as the server's own will"
        );
        // A bundle built without the dev server's plugin host is never cached.
        assert!(bundler.cacheable());
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
        let store =
            |kind: &str| codegen_store(&root, &cache, kind, Some("generate.mjs"), None, &[]);

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

    // `oj dev` and `oj build` generate the route tree under their own env
    // (development and production), which the generator's app code can read:
    // one's tree must not restore for the other.
    #[test]
    fn route_tree_cache_keys_on_the_generator_env() {
        let root = tmp("treeenv");
        std::fs::create_dir_all(root.join("src/routes")).unwrap();
        let cache = root.join("cache");
        std::fs::create_dir_all(&cache).unwrap();
        std::fs::write(root.join("src/routes/index.tsx"), "export const Route = 1;").unwrap();
        let tree = root.join("src/routeTree.gen.ts");
        std::fs::write(&tree, "// tree").unwrap();
        let inputs = vec![root.join("src/routes/index.tsx")];
        let outputs = [("routeTree.gen.ts", tree.as_path())];
        let env = |node_env: &str| vec![("NODE_ENV".to_string(), node_env.to_string())];
        let store = |env: &[(String, String)]| {
            codegen_store(&root, &cache, "route-tree", Some("generate.mjs"), None, env)
        };

        store(&env("development"))
            .persist(&inputs, &outputs)
            .unwrap();
        assert!(
            store(&env("production"))
                .restore(&inputs, &outputs)
                .is_err(),
            "a tree generated under development must not restore for a build"
        );
        assert!(store(&env("development"))
            .restore(&inputs, &outputs)
            .is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    struct HmrTestServer {
        url: String,
        reloads: Arc<crate::start_reloads::Epochs>,
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
        Arc<crate::start_reloads::Epochs>,
        broadcast::Sender<String>,
        Option<String>,
        Option<Arc<tokio::sync::Notify>>,
        broadcast::Sender<String>,
        ToPlugins,
    );

    impl HmrTestServer {
        async fn start() -> Self {
            Self::start_with(None, None, None, None).await
        }

        async fn start_with_gate(gate: Option<Arc<tokio::sync::Notify>>) -> Self {
            Self::start_with(gate, None, None, None).await
        }

        async fn start_with_ping(ping: Duration) -> Self {
            Self::start_with(None, None, Some(ping), None).await
        }

        /// Pages with a hot client, registering with `bundler`.
        async fn start_hot(ping: Duration, bundler: Arc<ClientBundler>) -> Self {
            Self::start_with(None, None, Some(ping), Some(bundler)).await
        }

        async fn start_with(
            gate: Option<Arc<tokio::sync::Notify>>,
            replay: Option<String>,
            ping: Option<Duration>,
            bundler: Option<Arc<ClientBundler>>,
        ) -> Self {
            let reloads = Arc::new(crate::start_reloads::Epochs::new(None));
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
            let app = axum::Router::new()
                .route(
                    "/@oj-start/hmr",
                    axum::routing::get(
                        move |ws: WebSocketUpgrade,
                              uri: axum::http::Uri,
                              State((tx, error_tx, replay, gate, ws_tx, sink)): State<
                            HmrTestState,
                        >| async move {
                            let epoch = tx.page(uri.query());
                            let errors = error_tx.subscribe();
                            let events = PageEvents {
                                frames: ws_tx.subscribe(),
                                to_plugins: sink,
                            };
                            let hot = bundler.as_ref().map(|bundler| HotPage {
                                bundler: Arc::clone(bundler),
                                frames: bundler.hot.subscribe(),
                                reload_needed: Arc::new(|| {}),
                                has: None,
                            });
                            ws.on_upgrade(move |socket| async move {
                                if let Some(gate) = gate {
                                    gate.notified().await;
                                }
                                start_hmr_socket(
                                    socket,
                                    epoch,
                                    errors,
                                    replay,
                                    hot,
                                    Some(events),
                                    ping,
                                )
                                .await;
                            })
                        },
                    ),
                )
                .with_state((
                    Arc::clone(&reloads),
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
                reloads,
                error_tx,
                ws_tx,
                to_plugins,
                task,
            }
        }

        async fn wait_for_no_subscribers(&self) {
            tokio::time::timeout(std::time::Duration::from_secs(2), async {
                while self.reloads.receivers() != 0 {
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
        // The protocol's pings and pongs are the socket's own (the client
        // answers a ping by itself as it reads); the frames are the page's.
        loop {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(2), client.next())
                .await
                .expect("a frame arrives")
                .unwrap()
                .unwrap();
            match msg {
                ClientMessage::Text(t) => return t.to_string(),
                ClientMessage::Ping(_) | ClientMessage::Pong(_) => {}
                other => panic!("expected a text frame, got {other:?}"),
            }
        }
    }

    /// An open page socket gets a keepalive frame each interval, with
    /// nothing else to send: the page's watchdog counts on hearing from the
    /// server, and a frame on the wire keeps idle-timeout proxies from
    /// cutting the socket.
    #[tokio::test]
    async fn an_idle_page_socket_gets_a_ping_each_interval() {
        let server = HmrTestServer::start_with_ping(Duration::from_millis(100)).await;
        let (mut client, _) = tokio_tungstenite::connect_async(&server.url).await.unwrap();
        let started = std::time::Instant::now();
        for _ in 0..3 {
            assert_eq!(next_text(&mut client).await, PING_FRAME);
        }
        // Three intervals, not a burst on connect.
        assert!(
            started.elapsed() >= Duration::from_millis(250),
            "three pings came in {:?}",
            started.elapsed()
        );
    }

    /// Pings go between the page's real frames, not instead of them.
    #[tokio::test]
    async fn pings_leave_the_pages_frames_alone() {
        let server = HmrTestServer::start_with_ping(Duration::from_millis(100)).await;
        let (mut client, _) = tokio_tungstenite::connect_async(&server.url).await.unwrap();
        assert_eq!(next_text(&mut client).await, PING_FRAME);
        while server.ws_tx.receiver_count() == 0 {
            tokio::task::yield_now().await;
        }
        let custom = r#"{"type":"custom","event":"app:hi","data":1}"#;
        server.ws_tx.send(custom.to_string()).unwrap();
        let mut seen = Vec::new();
        while seen.iter().all(|f| f != custom) {
            seen.push(next_text(&mut client).await);
        }
        assert!(
            seen.iter().all(|f| f == custom || f == PING_FRAME),
            "{seen:?}"
        );
    }

    /// A page that stops answering pings is gone, as far as its socket
    /// knows: the server closes the socket after the deadline instead of
    /// keeping it (and the page's hot client) until TCP gives up.
    #[tokio::test]
    async fn a_socket_whose_page_stops_answering_is_closed() {
        let ping = Duration::from_millis(100);
        let server = HmrTestServer::start_with_ping(ping).await;
        let (mut client, _) = tokio_tungstenite::connect_async(&server.url).await.unwrap();
        let started = std::time::Instant::now();
        // A client end that reads nothing answers no protocol ping: a page
        // whose connection is gone.
        tokio::time::sleep(silent_deadline(ping) + ping * 2).await;
        let closed = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match client.next().await {
                    Some(Ok(ClientMessage::Close(frame))) => return frame,
                    None | Some(Err(_)) => return None,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await;
        let frame = closed.expect("the silent page's socket stayed open");
        // It says why, as engine.io's "ping timeout". The close frame may
        // never be read: answering the pings queued while it read nothing,
        // the client writes to a socket the server already closed, and on
        // Linux the reset discards the unread close (macOS delivers it).
        if let Some(frame) = frame {
            assert_eq!(frame.reason.as_str(), PING_TIMEOUT_REASON);
        }
        assert!(
            started.elapsed() >= silent_deadline(ping),
            "closed before the deadline: {:?}",
            started.elapsed()
        );
        server.wait_for_no_subscribers().await;
    }

    /// One that answers stays, however long it is idle, with no word from
    /// its scripts: the protocol's pong, which a browser sends by itself
    /// (as a page paused at a breakpoint still does), is answer enough.
    #[tokio::test]
    async fn a_socket_whose_page_answers_stays_open() {
        let ping = Duration::from_millis(100);
        let server = HmrTestServer::start_with_ping(ping).await;
        let (mut client, _) = tokio_tungstenite::connect_async(&server.url).await.unwrap();
        let until = std::time::Instant::now() + silent_deadline(ping) * 4;
        let mut protocol_pings = 0;
        while std::time::Instant::now() < until {
            match tokio::time::timeout(Duration::from_secs(1), client.next()).await {
                Ok(Some(Ok(ClientMessage::Ping(_)))) => protocol_pings += 1,
                Ok(Some(Ok(ClientMessage::Text(t)))) => assert_eq!(t.as_str(), PING_FRAME),
                other => panic!("the answering page's socket ended: {other:?}"),
            }
        }
        assert!(protocol_pings >= 3, "{protocol_pings} protocol pings");
        assert_eq!(
            server.reloads.receivers(),
            1,
            "the answering page was dropped"
        );
    }

    /// A page whose socket died without a close opens a new one under the
    /// same runtime id, and the dead one's server end closes later, at its
    /// ping deadline. That close must leave the page registered: taken off,
    /// every later edit would leave it out and reload it, instead of
    /// patching it in place.
    #[tokio::test]
    async fn a_dead_sockets_late_close_leaves_its_page_on_the_engine() {
        let ping = Duration::from_millis(100);
        let bundler = Arc::new(ClientBundler::for_tests(Path::new("/app")));
        let server = HmrTestServer::start_hot(ping, Arc::clone(&bundler)).await;
        let hello = || ClientMessage::Text(r#"{"type":"oj:client","id":"c1"}"#.into());
        // The first socket announces the page, then goes silent (reads
        // nothing, so answers no ping).
        let (mut dead, _) = tokio_tungstenite::connect_async(&server.url).await.unwrap();
        dead.send(hello()).await.unwrap();
        // Its replacement announces the same page and keeps answering.
        let (mut live, _) = tokio_tungstenite::connect_async(&server.url).await.unwrap();
        live.send(hello()).await.unwrap();
        let until = std::time::Instant::now() + silent_deadline(ping) * 3;
        while std::time::Instant::now() < until {
            assert_eq!(next_text(&mut live).await, PING_FRAME);
        }
        // By now the server closed the silent one.
        let closed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match dead.next().await {
                    Some(Ok(ClientMessage::Close(_))) | None | Some(Err(_)) => return,
                    Some(Ok(_)) => {}
                }
            }
        })
        .await;
        assert!(closed.is_ok(), "the silent socket was never closed");
        // The bundler's own list (this test bundler runs no engine; the
        // engine hears of pages through the same count, in order).
        assert_eq!(
            bundler.pages(),
            vec!["c1".to_string()],
            "the dead socket's close took the page off with it"
        );
        // Its last socket closing takes it off.
        live.close(None).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !bundler.pages().is_empty() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the page stayed on after its last socket closed");
    }

    #[tokio::test]
    async fn a_socket_without_an_interval_sends_no_pings() {
        let server = HmrTestServer::start().await;
        let (mut client, _) = tokio_tungstenite::connect_async(&server.url).await.unwrap();
        let quiet = tokio::time::timeout(Duration::from_millis(300), client.next()).await;
        assert!(quiet.is_err(), "a frame arrived: {quiet:?}");
    }

    /// Vite's bundled dev error path: a page connecting while the client
    /// bundle is broken gets the stored failure first (lastBuildError replay),
    /// a later failure arrives as its own frame, and a reload still follows.
    #[tokio::test]
    async fn start_hmr_replays_the_build_error_then_forwards_new_ones() {
        let stored = r#"{"type":"error","err":{"message":"first"}}"#.to_string();
        let server = HmrTestServer::start_with(None, Some(stored.clone()), None, None).await;
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
        server.reloads.bump();
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
        let on = render_live_reload(
            &js,
            true,
            "/app",
            Some("1f-3".into()),
            Some(7),
            Some(Duration::from_millis(1500)),
        );
        let off = render_live_reload(&js, false, "", None, None, None);
        assert!(on.contains("const enableOverlay = true;"), "{on}");
        // The dev urls' prefix under `base`, filled in as a string.
        assert!(on.contains("const base = \"/app\";"), "{on}");
        assert!(off.contains("const base = \"\";"), "{off}");
        // The document's stamp, from the script's url.
        assert!(on.contains("const stamp = \"1f-3\";"), "{on}");
        assert!(off.contains("const stamp = null;"), "{off}");
        // The hot changes the document has, from the script's url too.
        assert!(on.contains("const docHas = 7;"), "{on}");
        assert!(off.contains("const docHas = null;"), "{off}");
        // The keepalive interval the watchdog counts on; none turns it off.
        assert!(on.contains("const pingMs = 1500;"), "{on}");
        assert!(off.contains("const pingMs = 0;"), "{off}");
        assert!(!on.contains("__OJ_PING_MS__"));
        assert!(!on.contains("__OJ_DEV_BASE__"));
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
            assert_eq!(server.reloads.receivers(), 1);
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
            // The closing client is still subscribed when the change goes out.
            assert_eq!(server.reloads.receivers(), 1);
            server.reloads.bump();
            // Let the I/O driver observe the close frame before releasing the handler.
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            gate.notify_one();
            let reply = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    match client.next().await {
                        Some(Ok(ClientMessage::Close(frame))) => break frame,
                        Some(Ok(ClientMessage::Text(text))) => {
                            assert!(text == "reload" || text == BEHIND_FRAME, "{text}")
                        }
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
            assert_eq!(server.reloads.receivers(), 1);
            drop(client);
            server.wait_for_no_subscribers().await;
        }
    }

    #[tokio::test]
    async fn start_hmr_reloads_a_busy_page_once_for_many_changes() {
        let gate = Arc::new(tokio::sync::Notify::new());
        let server = HmrTestServer::start_with_gate(Some(gate.clone())).await;
        let (mut client, _) = connect_async(&server.url).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while server.reloads.receivers() != 1 {
                tokio::time::sleep(std::time::Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("client subscription never registered");
        // The handler is held while the changes go out: a count, not a queue,
        // so none is lost and the page reloads once onto the newest (found
        // behind when the handler starts).
        for _ in 0..32 {
            assert!(server.reloads.bump());
        }
        gate.notify_one();
        assert_eq!(next_text(&mut client).await, BEHIND_FRAME);
        let more = tokio::time::timeout(std::time::Duration::from_millis(200), client.next()).await;
        assert!(more.is_err(), "the page reloaded more than once: {more:?}");
        client.close(None).await.unwrap();
        server.wait_for_no_subscribers().await;
    }

    /// A page still loading when a change went out had no socket to take it:
    /// its document's stamp says it is behind, and it reloads on connect. A
    /// page rendered after the change stays.
    #[tokio::test]
    async fn start_hmr_reloads_a_page_that_connects_behind() {
        let server = HmrTestServer::start().await;
        let rendered = server.reloads.stamp();
        server.reloads.bump();
        let (mut behind, _) = connect_async(format!("{}?g={rendered}", server.url))
            .await
            .unwrap();
        assert_eq!(next_text(&mut behind).await, BEHIND_FRAME);
        let current = server.reloads.stamp();
        let (mut fresh, _) = connect_async(format!("{}?g={current}", server.url))
            .await
            .unwrap();
        let quiet = tokio::time::timeout(std::time::Duration::from_millis(200), fresh.next()).await;
        assert!(
            quiet.is_err(),
            "a current page reloaded on connect: {quiet:?}"
        );
        server.reloads.bump();
        assert_eq!(next_text(&mut fresh).await, "reload");
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
            assert!(server.reloads.bump());
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
    // Vite's import-glob hotUpdate, the other way round and for the server
    // graphs: a file created or deleted where a module's glob looks changes
    // that module's expansion, so the module counts as changed although its
    // own file was not written (the client engine finds it through a watch
    // on the glob's directory, the server graphs through this list). A
    // member that exists and the bundle already read is an edit: nothing
    // re-expands for it. A deleted member the bundler has no module for by
    // its bare path (a `?raw` one) still gates the rebundle through its glob.
    #[test]
    fn a_glob_member_created_or_deleted_re_expands_its_importer() {
        let dir = tmp("glob-importers");
        let app = dir.join("app");
        let a = app.to_string_lossy().replace('\\', "/");
        let posts = app.join("src/posts.tsx");
        let icons = app.join("src/icons.tsx");
        std::fs::write(
            dir.join("glob-scopes.json"),
            serde_json::json!([
                {"importer": posts, "patterns": [format!("{a}/src/content/*.md")], "exhaustive": false, "caseSensitive": true},
                {"importer": icons, "patterns": [format!("{a}/src/icons/**/*.svg")], "exhaustive": false, "caseSensitive": true},
                // A cache an older oj wrote names no importer: it still gates.
                {"patterns": [format!("{a}/src/data/*.json")], "exhaustive": false, "caseSensitive": true},
            ])
            .to_string(),
        )
        .unwrap();
        let importers = load_client_glob_importers(&dir);
        assert_eq!(importers.len(), 2, "the entries naming an importer");
        let scopes = load_client_glob_scopes(&dir);
        assert_eq!(scopes.len(), 3, "every entry gates the rebundle");

        let file = |rel: &str| {
            let p = app.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, "x").unwrap();
            p
        };
        let read = file("src/content/read.md");
        let closure: std::collections::HashSet<PathBuf> =
            [std::fs::canonicalize(&read).unwrap()].into();
        let gone = app.join("src/content/gone.md");
        let created = file("src/content/new.md");
        let elsewhere = file("src/other/new.md");
        let data_gone = app.join("src/data/old.json");

        assert!(gone_from_glob(&scopes, &gone), "a deleted member");
        assert!(
            gone_from_glob(&scopes, &data_gone),
            "under a legacy entry too"
        );
        assert!(
            !gone_from_glob(&scopes, &read),
            "a member on disk is an edit"
        );
        assert!(!gone_from_glob(&scopes, &created));
        assert!(
            !gone_from_glob(&scopes, &app.join("src/other/gone.md")),
            "a deleted file no glob matches"
        );
        assert!(!gone_from_glob(&[], &gone));

        let hit = |paths: &[&PathBuf]| {
            glob_importers_hit(
                &importers,
                &closure,
                &paths.iter().map(|p| (*p).clone()).collect(),
            )
        };
        assert_eq!(
            hit(&[&created]),
            std::slice::from_ref(&posts),
            "a create re-expands"
        );
        assert_eq!(
            hit(&[&gone]),
            std::slice::from_ref(&posts),
            "a delete re-expands"
        );
        assert!(
            hit(&[&read]).is_empty(),
            "an edit of a member re-expands nothing"
        );
        assert!(hit(&[&elsewhere]).is_empty(), "a file no glob matches");
        assert_eq!(
            hit(&[&created, &gone, &app.join("src/icons/a.svg")]),
            [icons.clone(), posts.clone()],
            "each importer once, in order"
        );
        assert!(
            glob_importers_hit(&[], &closure, &[created.clone()].into()).is_empty(),
            "no importers recorded"
        );
    }

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
            assert_eq!(
                crate::start_bundle::dev::is_route_tree_gen(Path::new(name)),
                generated,
                "{name}"
            );
        }
    }

    // A cache dir reached through a symlink: the closure leaves its files out
    // by realpath, so a glob reaching that realpath has not grown either.
    #[cfg(unix)]
    #[test]
    fn restore_glob_members_skip_a_symlinked_cache_by_its_realpath() {
        let app = std::fs::canonicalize(tmp("restore-glob-link")).unwrap();
        std::fs::create_dir_all(app.join("src/lib")).unwrap();
        std::fs::create_dir_all(app.join("src/out/start")).unwrap();
        std::fs::create_dir_all(app.join(".oj")).unwrap();
        let member = app.join("src/lib/a.ts");
        std::fs::write(&member, "x").unwrap();
        std::fs::write(app.join("src/out/start/manifest.ts"), "x").unwrap();
        let cache = app.join(".oj/start");
        std::os::unix::fs::symlink(app.join("src/out/start"), &cache).unwrap();
        let scopes = app.join("glob-scopes.json");
        let a = app.to_string_lossy().replace('\\', "/");
        std::fs::write(
            &scopes,
            serde_json::json!([{"patterns": [format!("{a}/src/**/*.ts")], "exhaustive": true, "caseSensitive": true}])
                .to_string(),
        )
        .unwrap();
        assert_eq!(restorable_glob_members(&scopes, &cache), [member]);
        let _ = std::fs::remove_dir_all(&app);
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
        let seen = oj_server::ContentIndex::default();
        for p in &files {
            seen.settle(p, oj_server::Baseline::Pushed);
        }

        // Nothing rewritten: no changes, even for the still-missing files.
        assert!(changed_regen_outputs(&files, &seen).is_empty());

        // A rewrite and a creation both count; the untouched missing file not.
        std::fs::write(&tree, "tree v2").unwrap();
        std::fs::write(&resolver, "resolver v1").unwrap();
        let changed = changed_regen_outputs(&files, &seen);
        let paths: Vec<&str> = changed.iter().map(|(p, _)| p.as_str()).collect();
        assert_eq!(
            paths,
            vec![tree.to_str().unwrap(), resolver.to_str().unwrap()]
        );
        assert!(changed.iter().all(|(_, t)| *t == "update"));

        // Same content written again: hashes match, no change detected.
        std::fs::write(&tree, "tree v2").unwrap();
        assert!(changed_regen_outputs(&files, &seen).is_empty());

        // A change the worker HAS seen (recorded above) never re-pushes, and a
        // change landing between runs (someone else's write) is caught on the
        // next pass.
        std::fs::write(&tree, "tree v3").unwrap();
        let changed = changed_regen_outputs(&files, &seen);
        assert_eq!(changed.len(), 1);
        assert!(changed[0].0.ends_with("routeTree.gen.ts"));
    }

    // Vite's loadEnv rule for the Start scripts: `.env.<mode>` vars with any
    // configured envPrefix (not just VITE_) pass, unprefixed ones never do.
    #[test]
    fn start_script_env_carries_every_dotenv_var() {
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
        // Server code reads unprefixed secrets: envPrefix only gates what
        // reaches `import.meta.env` on the client.
        assert_eq!(get("SECRET_Z").as_deref(), Some("3"));

        let plain = tmp("script-env-plain");
        std::fs::write(plain.join(".env"), "VITE_X=1\nDATABASE_URL=postgres://x\n").unwrap();
        let vars = start_script_env(&plain, None, "serve", "development").unwrap();
        let names: Vec<&str> = vars.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["DATABASE_URL", "VITE_X"]);
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
        // Under the dev `base`, as Vite's urls are.
        let tag = reload_client("/app", "1f-3", None);
        assert!(
            tag.contains("src=\"/app/@oj-start/live-reload.js?g=1f-3\""),
            "{tag}"
        );
        assert!(!tag.contains("nonce"), "{tag}");
        // With the hot changes it has: the query's `&` escaped in the attribute.
        let hot = reload_client("/app", "1f-3&h=7", None);
        assert!(
            hot.contains("src=\"/app/@oj-start/live-reload.js?g=1f-3&amp;h=7\""),
            "{hot}"
        );
        let inject = |r| body_of(inject_reload_client(r, "/app", "1f-3", None));
        assert!(inject(resp("text/html", None)).contains(&tag));
        assert!(!inject(resp("text/html", Some("gzip"))).contains(&tag));
        assert!(inject(resp("text/html", Some("identity"))).contains(&tag));
        assert!(!inject(resp("text/javascript", None)).contains(&tag));
    }

    // The live-reload tag takes the document's CSP nonce: the one the
    // document's own script tags carry, found while the body streams (a tag
    // split across chunks included), else `html.cspNonce`; a document whose
    // tags have none gets a tag without one.
    #[test]
    fn reload_client_tag_carries_the_documents_nonce() {
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
        let streamed = |chunks: &[&str]| {
            let chunks: Vec<Result<axum::body::Bytes, axum::Error>> = chunks
                .iter()
                .map(|c| Ok(axum::body::Bytes::from(c.to_string())))
                .collect();
            Response::builder()
                .header(header::CONTENT_TYPE, "text/html")
                .body(axum::body::Body::from_stream(tokio_stream::iter(chunks)))
                .unwrap()
        };
        let tail = |html: &str| html[html.rfind("<script").unwrap()..].to_string();
        let inject = |chunks: &[&str], config: Option<&str>| {
            tail(&body_of(inject_reload_client(
                streamed(chunks),
                "",
                "1f-3",
                config,
            )))
        };
        // Start's `<Scripts>` writes the router's nonce double-quoted; its
        // `injectScript` writes it single-quoted.
        let page = [
            "<html><head><link rel=\"modulepreload\" href=\"/a.js\" nonce=\"n1\"></head>",
            "<body><script nonce=\"n1\" type=\"module\" src=\"/a.js\"></script></body></html>",
        ];
        assert!(inject(&page, None).contains(" nonce=\"n1\""));
        let single = ["<html><body><script nonce='n2'>x()</script></body></html>"];
        assert!(inject(&single, None).contains(" nonce=\"n2\""));
        // A tag the stream splits inside its name, and one split inside its
        // attributes, each as the chunks come.
        let split_name = [
            "<html><body><scr",
            "ipt nonce=\"n3\">x()</script></body></html>",
        ];
        assert!(
            inject(&split_name, None).contains(" nonce=\"n3\""),
            "{split_name:?}"
        );
        let split_attrs = [
            "<html><body><script type=\"module\" non",
            "ce=\"n4\" src=\"/a.js\"></script></body></html>",
        ];
        assert!(
            inject(&split_attrs, None).contains(" nonce=\"n4\""),
            "{split_attrs:?}"
        );
        // A `<script` inside text, with no `>` for a while, is no tag; a
        // document whose scripts carry no nonce gets a tag without one.
        let none = [
            "<html><body><script src=\"/a.js\"></script><p>a < b</p>",
            "</body></html>",
        ];
        assert!(!inject(&none, None).contains("nonce"), "{none:?}");
        // The document's nonce wins over the config's: the document's tags
        // name the nonce this request's policy accepts, the config's is a
        // static value for the tags Vite injects into a page. The config's
        // stands in when the document's tags carry none.
        let both = inject(&page, Some("cfg"));
        assert!(both.contains(" nonce=\"n1\""), "{both}");
        assert!(!both.contains("cfg"), "{both}");
        let configured = inject(&none, Some("cfg"));
        assert!(configured.contains(" nonce=\"cfg\""), "{configured}");
        // A `>` inside a quoted attribute value is the value's, not the
        // tag's end: the nonce after it is still read, whichever quote, and
        // when the stream splits the tag inside that value.
        let quoted =
            ["<html><body><script data-when=\"a>b\" nonce=\"n5\">x()</script></body></html>"];
        assert!(
            inject(&quoted, None).contains(" nonce=\"n5\""),
            "{quoted:?}"
        );
        let single_quoted =
            ["<html><body><script data-when='a>\"b' nonce='n6'>x()</script></body></html>"];
        assert!(
            inject(&single_quoted, None).contains(" nonce=\"n6\""),
            "{single_quoted:?}"
        );
        let split_value = [
            "<html><body><script data-when=\"a>",
            "b\" nonce=\"n7\">x()</script></body></html>",
        ];
        assert!(
            inject(&split_value, None).contains(" nonce=\"n7\""),
            "{split_value:?}"
        );
        // Tags and stray `<` before the script, and a chunk that ends on a
        // bare `<`, are passed over.
        let busy = [
            "<html><head><style>a<b{}</style><scripts></scripts></head><body><p>1 < 2 <",
            "b>x</b></p><script nonce=\"n8\">x()</script></body></html>",
        ];
        assert!(inject(&busy, None).contains(" nonce=\"n8\""), "{busy:?}");
        assert_eq!(script_open(b"<p><a><script>", 0), Some(6));
        assert_eq!(script_open(b"<p><a><script>", 7), None);
        assert_eq!(script_open(b"<scripts><ScRiPt ", 0), Some(9));
        assert_eq!(script_open(b"text <scr", 0), None);
        assert_eq!(tag_end(b"<script a=\"x>y\" b='z>'>rest>"), Some(22));
        assert_eq!(tag_end(b"<script a=\"x>y"), None);
        // The tag's name in any case; another tag starting with the name is
        // no script.
        let mut scan = ScriptNonceScan::default();
        scan.feed(b"<SCRIPT NONCE=\"up\">");
        assert_eq!(scan.nonce(), Some("up"));
        let mut scan = ScriptNonceScan::default();
        scan.feed(b"<scripts nonce=\"no\">");
        assert_eq!(scan.nonce(), None);
    }

    // Vite's NODE_ENV rule for the dev server: the shell wins (so
    // `NODE_ENV=production oj dev` is PROD), else `.env[.mode]` from `envDir`
    // may only pick development, else development.
    #[test]
    fn script_env_node_env_shell_wins_then_env_files() {
        let node_env = |env: &oj_env::Env, root: &Path, mode: &str| -> String {
            let inputs = script_inputs(env, root, None, "serve", mode).unwrap();
            let vars = script_vars(env, root, &inputs.app_env);
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
    fn dotenv_process_vars_skip_keys_the_shell_sets() {
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
        let vars = crate::start_bundle::dotenv_process_vars(&env, &app_env);
        assert_eq!(
            vars,
            vec![
                ("OTHER".to_string(), "x".to_string()),
                ("VITE_A".to_string(), "file".to_string()),
            ]
        );
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

    /// The id route serves only what a plugin serves: a `\0` id or a path
    /// with no file behind it, never a file on disk (inside the workspace or
    /// not) nor a relative id.
    #[test]
    fn the_id_route_serves_only_plugin_served_sheets() {
        let dir = std::env::temp_dir().join(format!("oj-id-route-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("secret.css");
        std::fs::write(&file, ".x{}").unwrap();
        assert_eq!(
            served_sheet_id("__x00__virtual:theme.css").as_deref(),
            Some("\0virtual:theme.css")
        );
        assert_eq!(served_sheet_id("/__uno.css").as_deref(), Some("/__uno.css"));
        assert_eq!(served_sheet_id(&file.display().to_string()[1..]), None);
        assert_eq!(served_sheet_id(&file.display().to_string()), None);
        assert_eq!(served_sheet_id("..%2Fsecret.css"), None);
        assert_eq!(served_sheet_id("__x00__virtual:data.js"), None);
        let _ = std::fs::remove_dir_all(&dir);
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
    fn public_file_path_is_the_exact_decoded_path_in_the_dir() {
        let public = Path::new("/app/public");
        let at =
            |path: &str, query: Option<&str>| public_file_path(public, path, query, &Method::GET);
        assert_eq!(
            at("/hub/index.html", None),
            Some(PathBuf::from("/app/public/hub/index.html"))
        );
        assert_eq!(
            at("/with%20space.txt", None),
            Some(PathBuf::from("/app/public/with space.txt"))
        );
        assert_eq!(
            at("/noext", Some("v=1")),
            Some(PathBuf::from("/app/public/noext"))
        );
        // No directory index and nothing for the root: Vite's public
        // middleware serves files only.
        assert_eq!(at("/hub/", None), None);
        assert_eq!(at("/", None), None);
        // The module requests the transform pipeline owns.
        assert_eq!(at("/logo.svg", Some("url")), None);
        assert_eq!(at("/logo.svg", Some("import")), None);
        assert_eq!(at("/logo.svg", Some("v=1&url")), None);
        // oj's own urls and a path that tries to leave the dir.
        assert_eq!(at("/@oj-start/x.js", None), None);
        assert_eq!(at("/@fs/etc/passwd", None), None);
        assert_eq!(at("/../package.json", None), None);
        assert_eq!(at("/a/%2e%2e/package.json", None), None);
        assert_eq!(at("//etc/passwd", None), None);
        assert_eq!(at("/a/./b.txt", None), None);
        assert_eq!(at("/a\\..\\b.txt", None), None);
        // Vite's importQueryRE takes `import=` too; `urlRE` only the bare flag.
        assert_eq!(at("/logo.svg", Some("import=")), None);
        assert_eq!(
            at("/logo.svg", Some("url=1")),
            Some(PathBuf::from("/app/public/logo.svg"))
        );
        // decodeURI leaves the reserved characters' escapes as written: an
        // encoded separator is part of a name, never a separator.
        assert_eq!(
            at("/a%2Fb.txt", None),
            Some(PathBuf::from("/app/public/a%2Fb.txt"))
        );
        assert_eq!(
            at("/caf%C3%A9.txt", None),
            Some(PathBuf::from("/app/public/café.txt"))
        );
        // What decodeURI throws on stays as written.
        assert_eq!(
            at("/100%.txt", None),
            Some(PathBuf::from("/app/public/100%.txt"))
        );
        assert_eq!(
            at("/bad%FFutf8.txt", None),
            Some(PathBuf::from("/app/public/bad%FFutf8.txt"))
        );
        assert_eq!(
            public_file_path(public, "/a.txt", None, &Method::POST),
            None
        );
        assert_eq!(
            public_file_path(public, "/a.txt", None, &Method::HEAD),
            Some(PathBuf::from("/app/public/a.txt"))
        );
    }

    #[test]
    fn decode_uri_is_javascripts() {
        assert_eq!(
            decode_uri("/with%20space.txt").as_deref(),
            Some("/with space.txt")
        );
        assert_eq!(decode_uri("/%E2%9C%93").as_deref(), Some("/✓"));
        // decodeURI("%2F%3F%23%26%3D%2B%24%2C%3B%3A%40") is its input.
        let reserved = "%2F%3F%23%26%3D%2B%24%2C%3B%3A%40";
        assert_eq!(decode_uri(reserved).as_deref(), Some(reserved));
        // URIError: malformed escapes and bytes that are not UTF-8.
        assert_eq!(decode_uri("%"), None);
        assert_eq!(decode_uri("a%2"), None);
        assert_eq!(decode_uri("a%zz"), None);
        assert_eq!(decode_uri("%FF"), None);
        assert_eq!(decode_uri("%C3"), None);
    }

    // Vite matches a url against the names in the public dir, so one that
    // differs only in case is no public file, whatever the file system opens.
    #[tokio::test]
    async fn a_public_file_is_named_as_it_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let public = dir.path().join("public");
        std::fs::create_dir_all(public.join("Hub")).unwrap();
        std::fs::write(public.join("Hub/Index.html"), "x").unwrap();
        assert!(named_as_written(&public, &public.join("Hub/Index.html")).await);
        // Only where the file system is case-insensitive does the other
        // spelling exist at all; there it is refused.
        let other = public.join("hub/index.html");
        if other.exists() {
            assert!(!named_as_written(&public, &other).await);
        }
        // A symlink names another file: the one asked for.
        #[cfg(unix)]
        {
            std::fs::write(dir.path().join("outside.txt"), "y").unwrap();
            std::os::unix::fs::symlink(dir.path().join("outside.txt"), public.join("link.txt"))
                .unwrap();
            std::os::unix::fs::symlink(public.join("Hub/Index.html"), public.join("alias.html"))
                .unwrap();
            assert!(named_as_written(&public, &public.join("link.txt")).await);
            assert!(named_as_written(&public, &public.join("alias.html")).await);
        }
    }

    #[test]
    fn asset_mime_maps_types() {
        assert_eq!(asset_mime("css"), "text/css; charset=utf-8");
        assert_eq!(asset_mime("js"), "text/javascript");
        assert_eq!(asset_mime("svg"), "image/svg+xml");
        assert_eq!(asset_mime("webp"), "image/webp");
        assert_eq!(asset_mime("woff2"), "font/woff2");
        assert_eq!(asset_mime("json"), "application/json");
        // The shared table answers for the other known asset types.
        assert_eq!(asset_mime("mp3"), "audio/mpeg");
        assert_eq!(asset_mime("pdf"), "application/pdf");
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
    fn oj_routes_classify_past_the_base_and_proxies_on_the_raw_path() {
        assert_eq!(strip_base("/app", "/app/@oj-start/hmr"), "/@oj-start/hmr");
        assert_eq!(strip_base("/app", "/application"), "/application");
        assert_eq!(strip_base("", "/@oj-start/hmr"), "/@oj-start/hmr");
        let get = Method::GET;
        let no_proxy: Vec<String> = vec![];
        assert!(matches!(
            classify_path("/@id/x", "/app/@id/x", &get, &no_proxy),
            Route::Pass
        ));
        assert!(matches!(
            classify_path("/about", "/app/about", &get, &no_proxy),
            Route::Document
        ));
        // Vite's proxy matches the url as sent, ahead of the base middleware.
        let proxy = vec!["/api".to_string()];
        assert!(matches!(
            classify_path("/api/x", "/app/api/x", &get, &proxy),
            Route::Document
        ));
        assert!(matches!(
            classify_path("/api/x", "/api/x", &get, &proxy),
            Route::Pass
        ));
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
        let found = list_route_files(&routes);
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

    /// An app whose server functions are finished on a builder another
    /// module exports, imported through a tsconfig `paths` alias: the file
    /// declaring them never names `createServerFn`.
    fn aliased_builder_app(label: &str) -> (PathBuf, PathBuf) {
        let root = tmp(label);
        let cache = root.join(".oj-cache");
        std::fs::create_dir_all(&cache).unwrap();
        for (path, code) in [
            (
                "tsconfig.json",
                "{ \"compilerOptions\": { \"baseUrl\": \".\", \"paths\": { \"@app/*\": [\"./src/*\"] } } }\n",
            ),
            ("package.json", "{ \"name\": \"app\", \"type\": \"module\" }\n"),
            (
                "src/lib/authed.ts",
                "import { createServerFn } from \"@tanstack/react-start\";\nexport const authedFn = createServerFn({ method: \"POST\" });\n",
            ),
            ("src/lib/barrel.ts", "export * from \"@app/lib/authed\";\n"),
            (
                "src/lib/aliased.functions.ts",
                "import { authedFn } from \"@app/lib/barrel\";\nexport const first = authedFn.handler(async () => 1);\n",
            ),
            ("src/lib/plain.ts", "export const plain = 1;\n"),
        ] {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, code).unwrap();
        }
        (root, cache)
    }

    fn resolver_maps(cache: &Path, name: &str) -> bool {
        let id = oj_compiler::server_fns::server_fn_id("src/lib/aliased.functions.ts", name);
        std::fs::read_to_string(cache.join(crate::start_bundle::server_fn_resolver_file(true)))
            .unwrap()
            .contains(&id)
    }

    // The resolver cached for an app is not restored once a file that keys it
    // changed, whether or not the file names `createServerFn`: a second
    // function added to a file whose builder is imported, the module
    // re-exporting the builder, and the tsconfig whose `paths` the import is
    // spelled through. Keyed on the files naming the factory alone, the old
    // resolver came back and the client called an id the server did not map.
    #[test]
    fn the_server_fn_resolver_is_regenerated_when_a_file_it_depends_on_changes() {
        let (root, cache) = aliased_builder_app("resolver-stale");
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(resolver_maps(&cache, "first"));
        assert!(!resolver_maps(&cache, "second"));

        // Unchanged: restored, the same content.
        let before = std::fs::read(cache.join("server-fn-resolver.mjs")).unwrap();
        assert!(server_fn_resolver(&root, &cache, true).unwrap());
        assert_eq!(
            std::fs::read(cache.join("server-fn-resolver.mjs")).unwrap(),
            before
        );

        // A second function in the file that does not name the factory.
        let fns = root.join("src/lib/aliased.functions.ts");
        let code = std::fs::read_to_string(&fns).unwrap();
        std::fs::write(
            &fns,
            format!("{code}export const second = authedFn.handler(async () => 2);\n"),
        )
        .unwrap();
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(resolver_maps(&cache, "first") && resolver_maps(&cache, "second"));

        // The barrel stops re-exporting the builder: no function is left.
        std::fs::write(
            root.join("src/lib/barrel.ts"),
            "export const nothing = 1;\n",
        )
        .unwrap();
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(!resolver_maps(&cache, "first") && !resolver_maps(&cache, "second"));
        std::fs::write(
            root.join("src/lib/barrel.ts"),
            "export * from \"@app/lib/authed\";\n",
        )
        .unwrap();
        // The files are those of two steps back: that resolver is restored.
        assert!(server_fn_resolver(&root, &cache, true).unwrap());
        assert!(resolver_maps(&cache, "first") && resolver_maps(&cache, "second"));

        // The alias stops naming the app's module.
        std::fs::write(
            root.join("tsconfig.json"),
            "{ \"compilerOptions\": { \"baseUrl\": \".\", \"paths\": { \"@other/*\": [\"./src/*\"] } } }\n",
        )
        .unwrap();
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(!resolver_maps(&cache, "first") && !resolver_maps(&cache, "second"));
        // And once more unchanged: restored.
        assert!(server_fn_resolver(&root, &cache, true).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn write_app(root: &Path, files: &[(&str, &str)]) {
        for (path, code) in files {
            let file = root.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, code).unwrap();
        }
    }

    fn resolver_text(cache: &Path) -> String {
        std::fs::read_to_string(cache.join(crate::start_bundle::server_fn_resolver_file(true)))
            .unwrap()
    }

    // A builder in a module that is not a `.ts`/`.tsx` file under `src/` (a
    // `.js` file of a `shared/` directory an alias names), reached through
    // `paths` a tsconfig takes from a file it extends: the module and the
    // extended file key the resolver and regenerate it in dev, as a source
    // file and the root tsconfig do. Neither was an input, so an edit to
    // either restored the resolver of before.
    #[test]
    fn the_resolver_is_keyed_on_a_module_outside_src_and_on_an_extended_tsconfig() {
        let root = tmp("resolver-outside-src");
        let cache = root.join(".oj-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let base = "{ \"compilerOptions\": { \"paths\": { \"@shared/*\": [\"../shared/*\"] } } }\n";
        let builder = "import { createServerFn } from \"@tanstack/react-start\";\nexport const authedFn = createServerFn({ method: \"POST\" });\n";
        write_app(
            &root,
            &[
                ("tsconfig.json", "{ \"extends\": \"./config/base\" }\n"),
                ("config/base.json", base),
                ("package.json", "{ \"name\": \"app\", \"type\": \"module\" }\n"),
                ("shared/authed.js", builder),
                (
                    "src/lib/fns.ts",
                    "import { authedFn } from \"@shared/authed\";\nexport const first = authedFn.handler(async () => 1);\n",
                ),
            ],
        );
        let id = oj_compiler::server_fns::server_fn_id("src/lib/fns.ts", "first");
        assert_eq!(
            resolution_configs(&root),
            [root.join("config/base.json"), root.join("tsconfig.json")]
        );
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(resolver_text(&cache).contains(&id));
        assert!(server_fn_resolver(&root, &cache, true).unwrap());
        let changed = |rel: &str| server_fn_inputs_changed(&root, &cache, &[root.join(rel)]);
        assert!(changed("shared/authed.js"));
        assert!(changed("config/base.json"));
        assert!(!changed("shared/other.js"));
        // An existing JSON file no tsconfig names.
        std::fs::write(root.join("config/other.json"), "{}\n").unwrap();
        assert!(!changed("config/other.json"));
        // A JSON file that came and went inside the batch (an editor's
        // safe-write temp, GNU sed's), gone once the batch settles: named by
        // no config, it is no input, wherever it was. It turned every edit
        // batched with such a temp into a resolver regeneration, and the hot
        // update into a rebuild.
        assert!(!changed("src/content/sedf1aa6r.json"));
        assert!(!changed("config/draft.json"));
        // The extended file deleted: an input still, the tsconfig names it.
        std::fs::remove_file(root.join("config/base.json")).unwrap();
        assert!(changed("config/base.json"));
        std::fs::write(root.join("config/base.json"), base).unwrap();

        // The module stops exporting a builder.
        std::fs::write(
            root.join("shared/authed.js"),
            "export const authedFn = 1;\n",
        )
        .unwrap();
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(!resolver_text(&cache).contains(&id));
        std::fs::write(root.join("shared/authed.js"), builder).unwrap();
        assert!(server_fn_resolver(&root, &cache, true).unwrap());
        assert!(resolver_text(&cache).contains(&id));

        // The extended file stops naming the directory.
        std::fs::write(
            root.join("config/base.json"),
            base.replace("@shared", "@other"),
        )
        .unwrap();
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(!resolver_text(&cache).contains(&id));
        let _ = std::fs::remove_dir_all(&root);
    }

    // A relative import no file answers is kept beside the resolver: the
    // file created there (here a `.js` barrel, not a source file the key
    // lists) regenerates the resolver in dev and at the next start. Only the
    // modules read were kept, so the handler on the builder the new barrel
    // exports stayed out of the resolver until a restart, or past one.
    #[test]
    fn a_file_created_where_an_import_found_none_regenerates_the_resolver() {
        let root = tmp("resolver-created");
        let cache = root.join(".oj-cache");
        std::fs::create_dir_all(&cache).unwrap();
        write_app(
            &root,
            &[
                ("package.json", "{ \"name\": \"app\", \"type\": \"module\" }\n"),
                (
                    "src/lib/authed.ts",
                    "import { createServerFn } from \"@tanstack/react-start\";\nexport const authedFn = createServerFn({ method: \"POST\" });\n",
                ),
                (
                    "src/lib/fns.ts",
                    "import { authedFn } from \"./barrel\";\nexport const first = authedFn.handler(async () => 1);\n",
                ),
            ],
        );
        let id = oj_compiler::server_fns::server_fn_id("src/lib/fns.ts", "first");
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(!resolver_text(&cache).contains(&id));
        let reads =
            std::fs::read_to_string(cache.join(crate::start_bundle::server_fn_reads_file(true)))
                .unwrap();
        assert_eq!(reads.lines().collect::<Vec<_>>(), ["?src/lib/barrel"]);
        let changed = |rel: &str| server_fn_inputs_changed(&root, &cache, &[root.join(rel)]);
        assert!(changed("src/lib/barrel.js"));
        assert!(changed("src/lib/barrel.tsx"));
        assert!(changed("src/lib/barrel/index.mts"));
        assert!(!changed("src/lib/barrels.js"));
        assert!(!changed("src/lib/barrel/other.js"));
        assert!(!changed("src/barrel.js"));
        assert!(server_fn_resolver(&root, &cache, true).unwrap());

        std::fs::write(
            root.join("src/lib/barrel.js"),
            "export * from \"./authed\";\n",
        )
        .unwrap();
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(resolver_text(&cache).contains(&id));
        assert!(server_fn_resolver(&root, &cache, true).unwrap());
        let _ = std::fs::remove_dir_all(&root);
    }

    // The reads file on disk is the last resolver's, which may not be the
    // one the key finds: `fns.ts` imports a barrel that is not there, is
    // edited, then is put back while the barrel is created (a branch switch).
    // The key made with the second resolver's reads file, which asked for no
    // barrel, is the first one's, whose resolver has no function; its own
    // reads file names the barrel, so it is not restored.
    #[test]
    fn a_resolver_is_not_restored_when_its_own_reads_file_gives_other_inputs() {
        let root = tmp("resolver-other-state");
        let cache = root.join(".oj-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let authed = "import { createServerFn } from \"@tanstack/react-start\";\nexport const authedFn = createServerFn({ method: \"POST\" });\n";
        let through_barrel = "import { authedFn } from \"./barrel\";\nexport const first = authedFn.handler(async () => 1);\n";
        write_app(
            &root,
            &[
                (
                    "package.json",
                    "{ \"name\": \"app\", \"type\": \"module\" }\n",
                ),
                ("src/lib/authed.ts", authed),
                ("src/lib/fns.ts", through_barrel),
            ],
        );
        let id = oj_compiler::server_fns::server_fn_id("src/lib/fns.ts", "first");
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(!resolver_text(&cache).contains(&id));

        std::fs::write(root.join("src/lib/fns.ts"), "export const nothing = 1;\n").unwrap();
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        let reads =
            std::fs::read_to_string(cache.join(crate::start_bundle::server_fn_reads_file(true)))
                .unwrap();
        assert_eq!(reads, "");

        std::fs::write(root.join("src/lib/fns.ts"), through_barrel).unwrap();
        std::fs::write(
            root.join("src/lib/barrel.js"),
            "export * from \"./authed\";\n",
        )
        .unwrap();
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(resolver_text(&cache).contains(&id));
        assert!(server_fn_resolver(&root, &cache, true).unwrap());
        assert!(resolver_text(&cache).contains(&id));
        let _ = std::fs::remove_dir_all(&root);
    }

    // An import through an alias that names no file yet, a file that takes
    // the place of the one read (`authed.js` is tried before `authed.ts`),
    // and a deleted file a tsconfig extends: each regenerates the resolver
    // in dev, and the first two key it at the next start. The alias's target
    // and the shadowing file are no `.ts` under `src/`, and neither was a
    // module the scan read.
    #[test]
    fn a_late_alias_target_a_shadowing_file_and_a_deleted_config_regenerate_the_resolver() {
        let root = tmp("resolver-late");
        let cache = root.join(".oj-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let builder = "import { createServerFn } from \"@tanstack/react-start\";\nexport const authedFn = createServerFn({ method: \"POST\" });\n";
        write_app(
            &root,
            &[
                ("tsconfig.json", "{ \"extends\": \"./config/base.json\" }\n"),
                (
                    "config/base.json",
                    "{ \"compilerOptions\": { \"paths\": { \"@shared/*\": [\"../shared/*\"] } } }\n",
                ),
                ("package.json", "{ \"name\": \"app\", \"type\": \"module\" }\n"),
                ("src/lib/authed.ts", "export const authedFn = 1;\n"),
                (
                    "src/lib/fns.ts",
                    "import { authedFn } from \"@shared/authed\";\nimport { authedFn as local } from \"./authed\";\nexport const first = authedFn.handler(async () => 1);\nexport const second = local.handler(async () => 2);\n",
                ),
            ],
        );
        let id = |name: &str| oj_compiler::server_fns::server_fn_id("src/lib/fns.ts", name);
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(!resolver_text(&cache).contains(&id("first")));
        assert!(!resolver_text(&cache).contains(&id("second")));
        let reads =
            std::fs::read_to_string(cache.join(crate::start_bundle::server_fn_reads_file(true)))
                .unwrap();
        assert_eq!(
            reads.lines().collect::<Vec<_>>(),
            [
                "src/lib/authed.ts",
                "?src/lib/authed",
                "!src/lib/fns.ts\t@shared/authed"
            ]
        );
        let changed = |rel: &str| server_fn_inputs_changed(&root, &cache, &[root.join(rel)]);
        // Nothing answers the alias yet, whatever module changed.
        assert!(!changed("shared/authed.js"));
        assert!(!changed("shared/other.js"));
        assert!(changed("src/lib/authed.js"));
        // The extended file, deleted: the tsconfig names it. A deleted JSON
        // nothing names (a temp an editor wrote and removed) is no input.
        let extended = std::fs::read(root.join("config/base.json")).unwrap();
        std::fs::remove_file(root.join("config/base.json")).unwrap();
        assert!(changed("config/base.json"));
        std::fs::write(root.join("config/base.json"), extended).unwrap();
        assert!(!changed("config/gone.json"));
        assert!(!changed("src/content/sed1a2b3c.json"));
        assert!(!changed("config/base.json.bak"));

        // The alias's target is created.
        std::fs::create_dir_all(root.join("shared")).unwrap();
        std::fs::write(root.join("shared/authed.js"), builder).unwrap();
        assert!(changed("shared/authed.js"));
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(resolver_text(&cache).contains(&id("first")));
        assert!(server_fn_resolver(&root, &cache, true).unwrap());

        // A `.js` file takes the place of the `.ts` one read.
        std::fs::write(root.join("src/lib/authed.js"), builder).unwrap();
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(resolver_text(&cache).contains(&id("second")));
        assert!(server_fn_resolver(&root, &cache, true).unwrap());
        std::fs::remove_file(root.join("src/lib/authed.js")).unwrap();
        assert!(!resolver_text(&cache).is_empty());
        server_fn_resolver(&root, &cache, true).unwrap();
        assert!(!resolver_text(&cache).contains(&id("second")));
        let _ = std::fs::remove_dir_all(&root);
    }

    // What a batch is to the resolver. An edit of a file that may declare a
    // server function changes the server's copy whatever the resolver
    // holds; everything else only may have changed the resolver, and counts
    // when the resolver it leaves is another: a `.ts` temp that came and
    // went, a tsconfig-named temp, a tsconfig saved as it was. Counted on
    // the trigger alone, each reloaded the page an edit beside it would
    // have patched in place.
    #[test]
    fn a_batch_is_a_declaring_edit_a_scan_or_nothing_to_the_resolver() {
        use ServerFnTrigger::{Declares, None, Scan};
        let (root, cache) = aliased_builder_app("resolver-trigger-kinds");
        generate_server_fn_resolver(&root, &cache, true).unwrap();
        let trigger = |rels: &[&str]| {
            let paths: Vec<PathBuf> = rels.iter().map(|rel| root.join(rel)).collect();
            server_fn_trigger(&root, &cache, &paths)
        };
        // Declares `.handler(..)`.
        assert_eq!(trigger(&["src/lib/aliased.functions.ts"]), Declares);
        // Names the factory: declares too, and is a module the scan read.
        assert_eq!(trigger(&["src/lib/authed.ts"]), Declares);
        // The barrel the scan read declares nothing.
        assert_eq!(trigger(&["src/lib/barrel.ts"]), Scan);
        // Temps that came and went, and config files as they are.
        assert_eq!(trigger(&["src/content/tmp123.ts"]), Scan);
        assert_eq!(trigger(&["src/content/tsconfig.tmp.json"]), Scan);
        assert_eq!(trigger(&["tsconfig.json"]), Scan);
        assert_eq!(trigger(&["package.json"]), Scan);
        // None of the resolver's.
        assert_eq!(trigger(&["src/lib/plain.ts"]), None);
        assert_eq!(trigger(&["src/content/sed1a2b3c.json"]), None);
        assert_eq!(trigger(&["src/styles.css"]), None);
        assert_eq!(trigger(&[]), None);
        // The strongest of a batch's paths, wherever it is in it.
        assert_eq!(
            trigger(&["src/lib/plain.ts", "tsconfig.json", "src/styles.css"]),
            Scan
        );
        assert_eq!(
            trigger(&["tsconfig.json", "src/lib/aliased.functions.ts"]),
            Declares
        );
        assert_eq!(
            trigger(&["src/lib/aliased.functions.ts", "src/content/tmp123.ts"]),
            Declares
        );

        // A scan that leaves the resolver as it was is no change of it: the
        // temp, the tsconfig saved again.
        let resolver = cache.join(crate::start_bundle::server_fn_resolver_file(true));
        let before = std::fs::read(&resolver).unwrap();
        let tsconfig = std::fs::read(root.join("tsconfig.json")).unwrap();
        std::fs::write(root.join("tsconfig.json"), &tsconfig).unwrap();
        generate_server_fn_resolver(&root, &cache, true).unwrap();
        assert_eq!(std::fs::read(&resolver).unwrap(), before);
        // One that changes what an import resolves to is.
        std::fs::write(
            root.join("tsconfig.json"),
            "{ \"compilerOptions\": { \"paths\": { \"@other/*\": [\"./src/*\"] } } }\n",
        )
        .unwrap();
        generate_server_fn_resolver(&root, &cache, true).unwrap();
        assert_ne!(std::fs::read(&resolver).unwrap(), before);
        let _ = std::fs::remove_dir_all(&root);
    }

    // A file beside an import the scan asked for answers it only when it is
    // a module: `authed.json` or `authed.css` beside `./authed` is no
    // answer, and regenerated the resolver for an edit of a data file.
    #[test]
    fn only_a_module_answers_an_import_the_scan_asked_for() {
        for answer in [
            "src/lib/authed",
            "src/lib/authed.ts",
            "src/lib/authed.tsx",
            "src/lib/authed.js",
            "src/lib/authed.mjs",
            "src/lib/authed.mts",
            "src/lib/authed.jsx",
            "src/lib/authed/index.ts",
            "src/lib/authed/index.js",
        ] {
            assert!(answers_import("src/lib/authed", answer), "{answer}");
        }
        for other in [
            "src/lib/authed.json",
            "src/lib/authed.css",
            "src/lib/authed.bak",
            "src/lib/authed.ts.tmp",
            "src/lib/authed/index.json",
            "src/lib/authed/other.ts",
            "src/lib/authedx.ts",
            "src/authed.ts",
        ] {
            assert!(!answers_import("src/lib/authed", other), "{other}");
        }
        // An import written with its extension: the file itself, or its
        // TypeScript source.
        assert!(answers_import("src/lib/authed.js", "src/lib/authed.js"));
        assert!(answers_import("src/lib/authed.js", "src/lib/authed.ts"));
        assert!(answers_import("src/lib/data.json", "src/lib/data.json"));
        assert!(!answers_import("src/lib/authed.js", "src/lib/authed.json"));
    }

    // A changed path is matched with what the reads file lists however the
    // watcher spells it: the root reached through a symlink (macOS names a
    // temp dir `/var/..` and `/private/var/..`), either way round, and a
    // file that is gone, whose directory still has a real path. Matched as
    // written, an edit of a module the scan read, or the removal of an
    // extended tsconfig, was nothing to the resolver.
    #[cfg(unix)]
    #[test]
    fn a_changed_path_is_matched_however_the_root_is_spelled() {
        let dir = tmp("resolver-spelled");
        let real = dir.canonicalize().unwrap().join("app");
        let link = dir.join("link");
        std::fs::create_dir_all(real.join("src/lib")).unwrap();
        std::fs::create_dir_all(real.join("config")).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();
        for (root, other) in [(&real, &link), (&link, &real)] {
            let rel = |path: &str| app_relative(root, &other.join(path));
            assert_eq!(rel("src/lib/barrel.ts"), "src/lib/barrel.ts");
            assert_eq!(rel("src/lib/gone.ts"), "src/lib/gone.ts", "no such file");
            assert_eq!(rel("tsconfig.json"), "tsconfig.json");
            assert_eq!(
                app_relative(root, &root.join("src/lib/barrel.ts")),
                "src/lib/barrel.ts"
            );
        }
        // Not under the root at all: whole, however it is tried.
        let outside = dir.canonicalize().unwrap().join("elsewhere/x.ts");
        assert_eq!(
            app_relative(&real, &outside),
            outside.to_string_lossy().replace('\\', "/")
        );
        assert_eq!(
            app_relative(&real, Path::new("relative/x.ts")),
            "relative/x.ts"
        );

        // Through the trigger: the app at its real path, the watcher naming
        // files through the link.
        let cache = real.join(".oj-cache");
        std::fs::create_dir_all(&cache).unwrap();
        write_app(
            &real,
            &[
                ("tsconfig.json", "{ \"extends\": \"./config/base.json\" }\n"),
                (
                    "config/base.json",
                    "{ \"compilerOptions\": { \"baseUrl\": \"..\", \"paths\": { \"@app/*\": [\"./src/*\"] } } }\n",
                ),
                ("package.json", "{ \"name\": \"app\", \"type\": \"module\" }\n"),
                (
                    "src/lib/authed.ts",
                    "import { createServerFn } from \"@tanstack/react-start\";\nexport const authedFn = createServerFn({ method: \"POST\" });\n",
                ),
                ("src/lib/barrel.ts", "export * from \"./authed\";\n"),
                (
                    "src/lib/fns.ts",
                    "import { authedFn } from \"./barrel\";\nexport const first = authedFn.handler(async () => 1);\n",
                ),
                ("src/lib/plain.ts", "export const plain = 1;\n"),
            ],
        );
        generate_server_fn_resolver(&real, &cache, true).unwrap();
        let through_link = |path: &str| server_fn_trigger(&real, &cache, &[link.join(path)]);
        assert_eq!(through_link("src/lib/barrel.ts"), ServerFnTrigger::Scan);
        assert_eq!(through_link("src/lib/barrel.js"), ServerFnTrigger::Scan);
        assert_eq!(through_link("config/base.json"), ServerFnTrigger::Scan);
        assert_eq!(through_link("src/lib/plain.ts"), ServerFnTrigger::None);
        assert_eq!(through_link("config/other.json"), ServerFnTrigger::None);
        std::fs::remove_file(real.join("config/base.json")).unwrap();
        assert_eq!(
            through_link("config/base.json"),
            ServerFnTrigger::Scan,
            "removed, and named by the tsconfig"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    const BUILDER: &str = "import { createServerFn } from \"@tanstack/react-start\";\nexport const authedFn = createServerFn({ method: \"POST\" });\n";

    fn trigger_of(root: &Path, cache: &Path, rels: &[&str]) -> ServerFnTrigger {
        let paths: Vec<PathBuf> = rels.iter().map(|rel| root.join(rel)).collect();
        server_fn_trigger(root, cache, &paths)
    }

    // Vite reloads the page for a file named `tsconfig.json` and no other.
    #[test]
    fn only_a_file_named_tsconfig_json_reloads_the_page_by_its_name() {
        for yes in [
            "tsconfig.json",
            "/app/tsconfig.json",
            "/app/src/nested/tsconfig.json",
        ] {
            assert!(is_tsconfig_json(Path::new(yes)), "{yes}");
        }
        for no in [
            "/app/tsconfig.app.json",
            "/app/tsconfig.tmp.json",
            "/app/jsconfig.json",
            "/app/config/base.json",
            "/app/tsconfig.json.bak",
            "/app/tsconfig.json/x.ts",
            "/app/package.json",
            "/app/src/tsconfig.ts",
        ] {
            assert!(!is_tsconfig_json(Path::new(no)), "{no}");
        }
    }

    // What the rebuild worker takes from a batch: a declaring edit always
    // changed the server functions, a scan did when the resolver it leaves
    // is another or could not be made, and a route change alone regenerates
    // without counting.
    #[test]
    fn the_worker_counts_a_scan_when_the_resolver_it_leaves_is_another() {
        use ServerFnTrigger::{Declares, None, Scan};
        let (root, cache) = aliased_builder_app("resolver-worker");
        generate_server_fn_resolver(&root, &cache, true).unwrap();
        let resolver = cache.join(crate::start_bundle::server_fn_resolver_file(true));
        let before = std::fs::read(&resolver).unwrap();
        assert!(!refresh_server_fn_resolver(&root, &cache, None, false));
        assert!(!refresh_server_fn_resolver(&root, &cache, None, true));
        assert!(!refresh_server_fn_resolver(&root, &cache, Scan, false));
        assert!(!refresh_server_fn_resolver(&root, &cache, Scan, true));
        assert!(refresh_server_fn_resolver(&root, &cache, Declares, false));
        assert_eq!(std::fs::read(&resolver).unwrap(), before);

        // The resolver is regenerated for a route change alone, uncounted.
        std::fs::remove_file(&resolver).unwrap();
        assert!(!refresh_server_fn_resolver(&root, &cache, None, true));
        assert_eq!(std::fs::read(&resolver).unwrap(), before);
        // No trigger and no route change: left alone.
        std::fs::remove_file(&resolver).unwrap();
        assert!(!refresh_server_fn_resolver(&root, &cache, None, false));
        assert!(!resolver.exists());
        // A scan that finds no resolver where it makes one.
        assert!(refresh_server_fn_resolver(&root, &cache, Scan, false));

        // A scan that leaves another resolver.
        std::fs::remove_file(root.join("src/lib/aliased.functions.ts")).unwrap();
        assert!(refresh_server_fn_resolver(&root, &cache, Scan, false));
        assert!(!resolver_maps(&cache, "first"));
        assert!(!refresh_server_fn_resolver(&root, &cache, Scan, false));

        // One that could not be made: what the page would call is unknown.
        std::fs::remove_dir_all(&cache).unwrap();
        std::fs::write(&cache, "not a directory").unwrap();
        assert!(generate_server_fn_resolver(&root, &cache, true).is_err());
        assert!(refresh_server_fn_resolver(&root, &cache, Scan, false));
        assert!(!refresh_server_fn_resolver(&root, &cache, None, true));
        let _ = std::fs::remove_dir_all(&root);
    }

    // A builder's module the scan read is a declaring edit whatever its
    // extension: `authed.mts` as `authed.ts`. Taken for a scan, an edit of
    // its middleware left the page on the old server function when the
    // resolver came out the same.
    #[test]
    fn a_builder_module_of_another_kind_is_a_declaring_edit() {
        use ServerFnTrigger::{Declares, None, Scan};
        let root = tmp("resolver-other-kinds");
        let cache = root.join(".oj-cache");
        std::fs::create_dir_all(&cache).unwrap();
        write_app(
            &root,
            &[
                ("package.json", "{ \"name\": \"app\", \"type\": \"module\" }\n"),
                ("src/lib/authed.mts", BUILDER),
                ("shared/second.js", BUILDER),
                // Names the factory, and nothing imports it.
                ("src/lib/unread.mts", BUILDER),
                (
                    "src/lib/fns.ts",
                    "import { authedFn } from \"./authed.mjs\";\nimport { authedFn as second } from \"../../shared/second\";\nimport { lateFn } from \"./late.cjs\";\nexport const first = authedFn.handler(async () => 1);\nexport const other = second.handler(async () => 2);\nexport const late = lateFn.handler(async () => 3);\n",
                ),
            ],
        );
        generate_server_fn_resolver(&root, &cache, true).unwrap();
        let trigger = |rels: &[&str]| trigger_of(&root, &cache, rels);
        assert_eq!(trigger(&["src/lib/authed.mts"]), Declares);
        assert_eq!(trigger(&["shared/second.js"]), Declares);
        assert_eq!(trigger(&["src/styles.css", "shared/second.js"]), Declares);
        assert_eq!(trigger(&["tsconfig.json", "src/lib/authed.mts"]), Declares);
        // Not one the scan read: oj compiles server functions in `.ts` and
        // `.tsx` files alone.
        assert_eq!(trigger(&["src/lib/unread.mts"]), None);
        // Read, and declaring nothing any more.
        std::fs::write(
            root.join("shared/second.js"),
            "export const authedFn = 1;\n",
        )
        .unwrap();
        assert_eq!(trigger(&["shared/second.js"]), Scan);

        // `./late.cjs` is answered by `late.cts`, as Vite's
        // `isPossibleTsOutput` has it, and by `late.cjs`.
        assert!(answers_import("src/lib/late.cjs", "src/lib/late.cts"));
        assert!(answers_import("src/lib/late.cjs", "src/lib/late.cjs"));
        assert!(!answers_import("src/lib/late.cjs", "src/lib/late.json"));
        assert!(!answers_import("src/lib/late.cjs", "src/lib/later.cts"));
        std::fs::write(root.join("src/lib/late.cts"), "export const lateFn = 1;\n").unwrap();
        assert_eq!(trigger(&["src/lib/late.cts"]), Scan);
        assert_eq!(trigger(&["src/lib/late.json"]), None);
        for ext in ["mjs", "js", "mts", "ts", "jsx", "tsx", "cjs", "cts"] {
            assert!(is_module(ext), "{ext}");
        }
        for ext in ["json", "css", "", "d", "map"] {
            assert!(!is_module(ext), "{ext}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // A source file whose last server function was taken out names no
    // factory and no handler any more, and the resolver still imports it:
    // the edit is a scan, and the resolver made again maps it no longer.
    #[test]
    fn a_file_the_resolver_imports_is_a_scan_once_it_declares_nothing() {
        use ServerFnTrigger::{Declares, None, Scan};
        let (root, cache) = aliased_builder_app("resolver-dead-entry");
        generate_server_fn_resolver(&root, &cache, true).unwrap();
        let trigger = |rels: &[&str]| trigger_of(&root, &cache, rels);
        assert_eq!(trigger(&["src/lib/aliased.functions.ts"]), Declares);
        std::fs::write(
            root.join("src/lib/aliased.functions.ts"),
            "export const first = 1;\n",
        )
        .unwrap();
        assert_eq!(trigger(&["src/lib/aliased.functions.ts"]), Scan);
        assert_eq!(trigger(&["src/lib/plain.ts"]), None);
        // A file whose name the imported one's starts with.
        std::fs::write(root.join("src/lib/aliased.ts"), "export const a = 1;\n").unwrap();
        assert_eq!(trigger(&["src/lib/aliased.ts"]), None);
        assert!(resolver_maps(&cache, "first"));
        assert!(refresh_server_fn_resolver(&root, &cache, Scan, false));
        assert!(!resolver_maps(&cache, "first"));
        // The resolver imports it no longer: an edit of it is nothing.
        assert_eq!(trigger(&["src/lib/aliased.functions.ts"]), None);
        let _ = std::fs::remove_dir_all(&root);
    }

    // A directory named alone for the files that came or went with it (a
    // rename, `rm -r`): one under `src/` may hold source files, and one
    // that is gone held what the resolver imports or the scan read under
    // it. Named so, the batch was nothing to the resolver, which kept
    // importing the old paths.
    #[test]
    fn a_directory_that_came_or_went_is_a_scan() {
        use ServerFnTrigger::{None, Scan};
        let root = tmp("resolver-directories");
        let cache = root.join(".oj-cache");
        std::fs::create_dir_all(&cache).unwrap();
        write_app(
            &root,
            &[
                ("package.json", "{ \"name\": \"app\", \"type\": \"module\" }\n"),
                ("shared/authed.js", BUILDER),
                (
                    "src/features/a/fns.ts",
                    "import { createServerFn } from \"@tanstack/react-start\";\nexport const first = createServerFn().handler(async () => 1);\n",
                ),
                (
                    "src/lib/fns.ts",
                    "import { authedFn } from \"../../shared/authed\";\nexport const second = authedFn.handler(async () => 2);\n",
                ),
                ("src/features/ab/plain.ts", "export const plain = 1;\n"),
                ("docs/notes.md", "notes\n"),
            ],
        );
        generate_server_fn_resolver(&root, &cache, true).unwrap();
        let trigger = |rels: &[&str]| trigger_of(&root, &cache, rels);
        // Directories as they stand: under `src/` or not.
        assert_eq!(trigger(&["src/features/a"]), Scan);
        assert_eq!(trigger(&["src"]), Scan);
        assert_eq!(trigger(&["docs"]), None);
        assert_eq!(trigger(&["shared"]), None);
        assert_eq!(trigger(&["srcs"]), None);

        std::fs::rename(root.join("src/features/a"), root.join("src/features/b")).unwrap();
        assert_eq!(
            trigger(&["src/features/a"]),
            Scan,
            "the resolver imports under it"
        );
        assert_eq!(trigger(&["src/features/b"]), Scan);
        assert!(refresh_server_fn_resolver(&root, &cache, Scan, false));
        assert!(resolver_text(&cache).contains("src/features/b/fns.ts"));
        assert!(!resolver_text(&cache).contains("src/features/a/fns.ts"));
        assert_eq!(
            trigger(&["src/features/a"]),
            None,
            "nothing is under it any more"
        );

        std::fs::remove_dir_all(root.join("src/features/ab")).unwrap();
        assert_eq!(
            trigger(&["src/features/ab"]),
            None,
            "held nothing of the resolver's"
        );
        std::fs::remove_dir_all(root.join("docs")).unwrap();
        assert_eq!(trigger(&["docs"]), None);
        std::fs::remove_dir_all(root.join("shared")).unwrap();
        assert_eq!(
            trigger(&["shared"]),
            Scan,
            "the scan read a module under it"
        );
        assert_eq!(trigger(&["share"]), None);
        assert!(refresh_server_fn_resolver(&root, &cache, Scan, false));
        let _ = std::fs::remove_dir_all(&root);
    }

    // The package.json and tsconfig nearest a module decide what its
    // imports name (`imports`, a nested project's `paths`), and only the
    // root's keyed the cached resolver: with `src/package.json` pointing
    // `#builder` at another module, the resolver came back from the cache as
    // it was.
    #[test]
    fn the_resolver_is_keyed_on_the_configs_nearest_the_modules_it_read() {
        let root = tmp("resolver-nearer-configs");
        let cache = root.join(".oj-cache");
        std::fs::create_dir_all(&cache).unwrap();
        let imports = |target: &str| {
            format!("{{ \"name\": \"inner\", \"imports\": {{ \"#builder\": \"./lib/{target}.ts\" }} }}\n")
        };
        write_app(
            &root,
            &[
                ("package.json", "{ \"name\": \"app\", \"type\": \"module\" }\n"),
                ("src/package.json", &imports("authed")),
                ("src/lib/authed.ts", BUILDER),
                ("src/lib/plain.ts", "export const authedFn = { handler() {} };\n"),
                (
                    "src/lib/fns.ts",
                    "import { authedFn } from \"#builder\";\nexport const first = authedFn.handler(async () => 1);\n",
                ),
                ("src/lib/deep/tsconfig.json", "{}\n"),
                ("src/lib/deep/x.ts", "export const x = 1;\n"),
                ("other/package.json", "{}\n"),
            ],
        );
        let maps = |cache: &Path| {
            resolver_text(cache).contains(&oj_compiler::server_fns::server_fn_id(
                "src/lib/fns.ts",
                "first",
            ))
        };
        let reads = cache.join(crate::start_bundle::server_fn_reads_file(true));
        assert!(!server_fn_resolver(&root, &cache, true).unwrap());
        assert!(maps(&cache));
        let inputs = server_fn_inputs(&root, &reads);
        assert!(inputs.contains(&root.join("src/package.json")));
        assert!(inputs.contains(&root.join("src/lib/deep/tsconfig.json")));
        assert!(
            !inputs.contains(&root.join("package.json")),
            "the store's own"
        );
        assert!(
            !inputs.contains(&root.join("other/package.json")),
            "above no module"
        );
        assert!(
            server_fn_resolver(&root, &cache, true).unwrap(),
            "unchanged: restored"
        );

        std::fs::write(root.join("src/package.json"), imports("plain")).unwrap();
        assert_eq!(
            trigger_of(&root, &cache, &["src/package.json"]),
            ServerFnTrigger::Scan
        );
        assert!(refresh_server_fn_resolver(
            &root,
            &cache,
            ServerFnTrigger::Scan,
            false
        ));
        assert!(!maps(&cache), "`#builder` names no builder now");
        std::fs::write(root.join("src/package.json"), imports("authed")).unwrap();
        assert!(refresh_server_fn_resolver(
            &root,
            &cache,
            ServerFnTrigger::Scan,
            false
        ));
        assert!(maps(&cache));

        assert_eq!(
            nearer_configs(&root, &[root.join("src/lib/deep/x.ts"), root.join("x.ts")]),
            [
                root.join("src/package.json"),
                root.join("src/lib/deep/tsconfig.json")
            ]
        );
        assert!(nearer_configs(&root, &[PathBuf::from("/elsewhere/a/x.ts")]).is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    // The scan names a module it read through a symlinked directory of the
    // app where the module really is, and one imported in another letter
    // case than the disk has (a case-insensitive volume) as the import
    // spelled it. The watcher's path for either, through the link or as the
    // disk has it, is the module read.
    #[cfg(unix)]
    #[test]
    fn a_module_read_is_matched_by_its_real_path() {
        use ServerFnTrigger::{None, Scan};
        let (root, _) = aliased_builder_app("resolver-real-paths");
        let root = root.canonicalize().unwrap();
        let cache = root.join(".oj-cache");
        write_app(
            &root,
            &[
                ("kept/barrel.ts", "export * from \"@app/lib/authed\";\n"),
                ("kept/plain.ts", "export const plain = 1;\n"),
                (
                    "src/lib/linked.functions.ts",
                    "import { authedFn } from \"@app/linked/barrel\";\nexport const viaLink = authedFn.handler(async () => 1);\n",
                ),
            ],
        );
        std::os::unix::fs::symlink(root.join("kept"), root.join("src/linked")).unwrap();
        generate_server_fn_resolver(&root, &cache, true).unwrap();
        let reads =
            std::fs::read_to_string(cache.join(crate::start_bundle::server_fn_reads_file(true)))
                .unwrap();
        assert!(
            reads.lines().any(|line| line == "kept/barrel.ts"),
            "{reads}"
        );
        let trigger = |rels: &[&str]| trigger_of(&root, &cache, rels);
        assert_eq!(trigger(&["src/linked/barrel.ts"]), Scan);
        assert_eq!(trigger(&["kept/barrel.ts"]), Scan, "where it really is");
        assert_eq!(trigger(&["kept/plain.ts"]), None);
        assert_eq!(trigger(&["kept/barrel.json"]), None);
        assert_eq!(trigger(&["kept/gone/barrel.js"]), None, "no such directory");

        // Another letter case, where the volume takes one for the other.
        if root.join("src/lib/BARREL.ts").exists() {
            std::fs::write(
                root.join("src/lib/cased.functions.ts"),
                "import { authedFn } from \"@app/lib/Barrel\";\nexport const cased = authedFn.handler(async () => 1);\n",
            )
            .unwrap();
            std::fs::write(
                root.join("src/lib/aliased.functions.ts"),
                "export const first = 1;\n",
            )
            .unwrap();
            generate_server_fn_resolver(&root, &cache, true).unwrap();
            let reads = std::fs::read_to_string(
                cache.join(crate::start_bundle::server_fn_reads_file(true)),
            )
            .unwrap();
            assert!(
                reads.lines().any(|line| line == "src/lib/Barrel.ts"),
                "{reads}"
            );
            assert!(
                !reads.lines().any(|line| line == "src/lib/barrel.ts"),
                "{reads}"
            );
            assert_eq!(trigger(&["src/lib/barrel.ts"]), Scan);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // Which edits regenerate the resolver in dev: a file that may declare a
    // server function, a module the last scan read for an imported builder
    // (the builder's module, the barrel between), a deleted source file and
    // the files an import's resolution reads. A module no chain reached does
    // not, nor a style.
    #[test]
    fn a_dev_edit_regenerates_the_resolver_when_its_scan_read_the_file() {
        let (root, cache) = aliased_builder_app("resolver-trigger");
        generate_server_fn_resolver(&root, &cache, true).unwrap();
        let reads =
            std::fs::read_to_string(cache.join(crate::start_bundle::server_fn_reads_file(true)))
                .unwrap();
        assert_eq!(
            reads.lines().collect::<Vec<_>>(),
            ["src/lib/authed.ts", "src/lib/barrel.ts"]
        );
        let changed = |rel: &str| server_fn_inputs_changed(&root, &cache, &[root.join(rel)]);
        assert!(changed("src/lib/aliased.functions.ts"));
        assert!(changed("src/lib/authed.ts"));
        assert!(changed("src/lib/barrel.ts"));
        assert!(changed("src/lib/deleted.ts"));
        assert!(changed("tsconfig.json"));
        assert!(changed("tsconfig.app.json"));
        assert!(changed("package.json"));
        assert!(changed("src/tsconfig.json"));
        assert!(!changed("src/lib/plain.ts"));
        assert!(!changed("src/styles.css"));
        std::fs::write(root.join("components.json"), "{}\n").unwrap();
        assert!(!changed("components.json"));
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
    }

    #[test]
    fn list_route_files_missing_dir_is_empty() {
        let root = tmp("noroutes");
        assert!(list_route_files(&root.join("src/routes")).is_empty());
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
        // Start publishes the alias at the top level; Vite 8 ignores an
        // environment's own resolve.alias.
        assert_eq!(
            configured_start_server_entry(&env_scoped, &root),
            None,
            "environments.ssr.resolve.alias does not count"
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
