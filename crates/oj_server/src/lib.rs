// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use axum::{
    body::{Body, Bytes},
    extract::{ws::Message, FromRequestParts, Query, State, WebSocketUpgrade},
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
    Router,
};
use oj_cache::{CachedModule, PersistentCache};

pub mod css_engine;
pub mod optimize;
pub mod pkg_bundle;
pub mod pkg_rolldown;
pub mod plugins;
mod preseed;
pub use preseed::PACKAGE_MANAGER_LOCKFILES;
pub mod sidecar;
pub mod svgr;
mod util;
pub use util::*;
mod plugin_routes;
pub use plugin_routes::*;
mod ui;
pub use ui::*;
mod client_js;
pub use client_js::*;
mod assets_serve;
pub use assets_serve::*;
mod rewrite;
pub use rewrite::*;
mod deps;
pub use deps::*;
mod module_pipeline;
pub use module_pipeline::*;
mod memory_cache;
pub use memory_cache::*;
mod serve;
pub use serve::*;
mod crawl;
pub use crawl::*;
mod debug;
pub use debug::*;
mod css_serve;
pub use css_serve::*;
mod plugin_mw;
pub use plugin_mw::*;
mod hmr;
pub use hmr::*;
mod watch;
pub use watch::*;
mod html;
pub use html::*;
mod ssr;
pub use ssr::*;
mod middleware;
mod preview;
mod proxy;
use middleware::*;
pub use preview::*;
pub use proxy::*;
use css_engine::CssEngine;
use oj_graph::{HmrDecision, ModuleGraph};
use oj_resolver::OjResolver;
use plugins::PluginHost;
use sidecar::is_tailwind_css;
use tokio::sync::broadcast;



const CLIENT_JS: &str = include_str!("assets/client.js");
pub const OJ_ROUTES_JS: &str = include_str!("assets/oj-routes.js");
const SERVER_FN_JS: &str = include_str!("assets/server-fn.js");
const LINGUI_MACRO_SHIM_JS: &str = include_str!("assets/lingui-macro-shim.mjs");
const REFRESH_RUNTIME_JS: &str = include_str!("assets/refresh-runtime.js");
const REFRESH_PREAMBLE_JS: &str = include_str!("assets/refresh-preamble.js");
// Probed in Vite's DEFAULT_EXTENSIONS order (js before ts, .mts included) so the
// extensionless quick path agrees with the resolver; .cts/.svelte trail as
// compilable-but-not-default-probed.
const COMPILABLE: &[&str] = &["mjs", "js", "mts", "ts", "jsx", "tsx", "cts", "svelte"];

const START_ASSETS: &[(&str, &str)] = &[
    (
        "injected-head-scripts.ts",
        include_str!("assets/start/injected-head-scripts.ts"),
    ),
    (
        "resolve-pkg.mjs",
        include_str!("assets/start/resolve-pkg.mjs"),
    ),
    (
        "rolldown-assets.mjs",
        include_str!("assets/start/rolldown-assets.mjs"),
    ),
    (
        "vite-plugin-bridge.mjs",
        include_str!("assets/start/vite-plugin-bridge.mjs"),
    ),
    (
        "glob-transform.mjs",
        include_str!("assets/start/glob-transform.mjs"),
    ),
    ("cf-server.mjs", include_str!("assets/start/cf-server.mjs")),
    (
        "cf-workers.mjs",
        include_str!("assets/start/cf-workers.mjs"),
    ),
    (
        "cf-server-worker.mjs",
        include_str!("assets/start/cf-server-worker.mjs"),
    ),
    ("cf-build.mjs", include_str!("assets/start/cf-build.mjs")),
    ("generate.mjs", include_str!("assets/start/generate.mjs")),
    (
        "gen-resolver.mjs",
        include_str!("assets/start/gen-resolver.mjs"),
    ),
    ("fn-stubs.mjs", include_str!("assets/start/fn-stubs.mjs")),
    (
        "bundle-client.mjs",
        include_str!("assets/start/bundle-client.mjs"),
    ),
    ("build.mjs", include_str!("assets/start/build.mjs")),
    (
        "live-reload.js",
        include_str!("assets/start/live-reload.js"),
    ),
    (
        "server-entry.tsx",
        include_str!("assets/start/server-entry.tsx"),
    ),
    (
        "client-entry.tsx",
        include_str!("assets/start/client-entry.tsx"),
    ),
    (
        "start-entry.ts",
        include_str!("assets/start/start-entry.ts"),
    ),
    (
        "plugin-adapters.ts",
        include_str!("assets/start/plugin-adapters.ts"),
    ),
    ("manifest.ts", include_str!("assets/start/manifest.ts")),
    (
        "manifest-dev.ts",
        include_str!("assets/start/manifest-dev.ts"),
    ),
];


/// The persistent V8 code-cache directory for embedded engines
/// (`oj_js::EngineConfig::code_cache_dir`): compiled bytecode for the app's
/// toolchain, reused across engine spawns and one-shot children — the
/// engine-side analog of NODE_COMPILE_CACHE. Keyed by the engine's ABI
/// (`oj_js::engine_abi_key`, the V8 version), NOT the oj version: per-entry
/// source hashes already invalidate changed scripts, so an oj release keeps
/// the warm cache instead of cold-starting every engine, and only a V8
/// upgrade (whose bytecode the new V8 would reject anyway) rotates the
/// directory.
pub fn engine_code_cache_dir(root: &Path) -> PathBuf {
    oj_cache::cache_root(root)
        .join("code-cache")
        .join(oj_js::engine_abi_key())
}

pub fn node_compile_cache(root: &Path) -> std::ffi::OsString {
    std::env::var_os("NODE_COMPILE_CACHE")
        .unwrap_or_else(|| oj_cache::cache_root(root).join("v8").into_os_string())
}

pub fn node_compile_cache_opt_in(root: &Path) -> Option<std::ffi::OsString> {
    let v = std::env::var_os("OJ_V8_COMPILE_CACHE")?;
    if v.is_empty() || v == "0" {
        return None;
    }
    Some(node_compile_cache(root))
}

pub fn prepare_cache_root(root: &Path) {
    let dir = oj_cache::cache_base(root);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        let _ = std::fs::write(gitignore, "*\n");
    }
    oj_cache::heal_legacy_layout(root);
    let _ = std::fs::create_dir_all(oj_cache::cache_root(root));
    // Vite-style boot hygiene (cleanupDepsCacheStaleDirs), off the boot
    // path: sweep torn-write tmp leftovers past the 24h age threshold.
    let current = engine_code_cache_dir(root);
    std::thread::spawn(move || {
        oj_js::code_cache::FsCodeCache::new(current)
            .sweep_stale_tmp(oj_js::code_cache::STALE_TMP_MAX_AGE);
    });
}

pub fn write_start_assets(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    for (name, content) in START_ASSETS {
        std::fs::write(dir.join(name), content)?;
    }
    Ok(())
}

pub fn is_tanstack_start_app(root: &Path) -> bool {
    root.join("src/routes").is_dir()
        && std::fs::read_to_string(root.join("package.json"))
            .map(|s| s.contains("@tanstack/react-start"))
            .unwrap_or(false)
}

/// Deduplicate a define list keeping the LAST occurrence of each key (later
/// layers override earlier ones, as in Vite's define merge).
fn dedup_defines_last_wins(defines: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::with_capacity(defines.len());
    for (k, v) in defines {
        if let Some(slot) = out.iter_mut().find(|(ek, _)| *ek == k) {
            slot.1 = v;
        } else {
            out.push((k, v));
        }
    }
    out
}

pub struct DevServer {
    pub root: PathBuf,
    pub port: Option<u16>,
    pub host: Option<String>,
    pub config: Option<PathBuf>,
    /// Enable the experimental on-disk module cache (off by default).
    pub enable_cache: bool,
    /// Force the on-disk module cache off even if enabled.
    pub no_cache: bool,
    /// Skip the eager graph crawl; compile modules on demand (Vite's default).
    pub lazy: bool,
    /// Vite's `--mode` for `serve` (default `development`): selects `.env.<mode>`,
    /// `import.meta.env.MODE`, and the mode plugin `config` hooks see.
    pub mode: Option<String>,
}

struct ServerState {
    root: PathBuf,
    /// Vite's `publicDir`; None when the config disables it (`publicDir: false`).
    public_dir: Option<PathBuf>,
    persistent_cache: bool,
    reload_tx: broadcast::Sender<String>,
    graph: Mutex<ModuleGraph>,
    resolver: Arc<OjResolver>,
    /// `resolver` with the `require` condition in place of `import`, for the
    /// `require()` specifiers of a directly-served CommonJS dep (Vite parity:
    /// getConditions pushes `require` when resolving for a requirer).
    require_resolver: Arc<OjResolver>,
    ssr_resolver: Arc<OjResolver>,
    cache: PersistentCache,
    memory: Mutex<MemoryCache>,
    mtime_keys: Mutex<HashMap<String, (std::time::SystemTime, u64, String)>>,
    compile_locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    crawl_done: tokio::sync::watch::Receiver<bool>,
    fs_allow: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
    /// `server.fs.strict` (default true). When false the allow list is not
    /// consulted for `/@fs/` paths; the deny list always is, as in Vite.
    fs_strict: bool,
    fs_deny: Vec<(glob::Pattern, bool)>,
    dir_cache: Arc<Mutex<DirCache>>,
    cache_writes: tokio::sync::mpsc::Sender<(String, Arc<CachedModule>)>,
    tailwind: tokio::sync::OnceCell<std::sync::Arc<CssEngine>>,
    preprocess: tokio::sync::OnceCell<std::sync::Arc<CssEngine>>,
    svelte: tokio::sync::OnceCell<std::sync::Arc<CssEngine>>,
    tailwind_urls: Mutex<std::collections::HashSet<String>>,
    has_postcss: bool,
    scss_additional_data: Option<String>,
    sass_additional_data: Option<String>,
    css_config: Option<oj_config::CssConfig>,
    /// `html.cspNonce`: stamped on every served page's script/style/link tags.
    csp_nonce: Option<String>,
    /// `resolve.alias`, root and public dir as seen by `@import`/`@use`/`url()`
    /// specifiers inside stylesheets (Vite's CSS resolvers).
    css_resolve: oj_css::CssResolveConfig,
    preload_snapshot: Vec<String>,
    proxy: Vec<(String, oj_config::ProxyEntry)>,
    /// Per `proxy` entry: the compiled pattern of a `^` (regex) context, `None`
    /// for a plain prefix. Built once so request matching does not recompile.
    proxy_regex: Vec<Option<regex::Regex>>,
    http: reqwest::Client,
    /// For `server.proxy` entries with `secure: false`: accepts any certificate.
    /// Built on first use, so projects that never opt in pay nothing.
    http_insecure: std::sync::OnceLock<reqwest::Client>,
    /// rustls client configs for proxied `wss://` targets, built once per server
    /// (the platform verifier loads the trust store; sessions resume across
    /// reconnects): index 0 verifies, index 1 is `secure: false`.
    proxy_tls: [std::sync::OnceLock<Result<std::sync::Arc<rustls::ClientConfig>, String>>; 2],
    virtual_modules: std::collections::BTreeMap<String, String>,
    jsx_overrides: std::collections::BTreeMap<String, String>,
    jsx: oj_compiler::JsxConfig,
    host_policy: HostPolicy,
    hmr_gate: Option<Arc<HmrGate>>,
    /// Fires when the HMR gate flushes, so the Start server releases the page
    /// reload it held (the plain path's held changes go through `decide` instead).
    gate_flush_tx: broadcast::Sender<()>,
    /// Process start, epoch milliseconds: the gate status `startedAt` the editor
    /// compares across dev server restarts.
    started_at_ms: u64,
    hmr_enabled: bool,
    plugins: Option<std::sync::Arc<PluginHost>>,
    plugin_serve: Arc<PluginServe>,
    plugins_ssr: tokio::sync::OnceCell<Option<std::sync::Arc<PluginHost>>>,
    /// Watcher events the lazily spawned SSR host could not take yet, plus
    /// the dispatch-order lock — see [`SsrWatchQueue`].
    ssr_watch: Arc<SsrWatchQueue>,
    ssr_plugin_config: String,
    plugin_watched: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
    plugins_use_module_parsed: bool,
    plugins_have_transform: bool,
    plugins_have_load: bool,
    // A dep is transformed only when its source matches one of these (the plugins'
    // own transform `filter.code` patterns); app source always goes through.
    dep_transform_res: Vec<regex::Regex>,
    // A dep goes through plugin `load` only when its path matches one of these
    // (the plugins' own object-form load `filter.id` patterns).
    dep_load_res: Vec<regex::Regex>,
    // A relative or absolute import is offered to plugin `resolveId` before oj's
    // resolver when it matches one of these (object-form resolveId `filter.id`).
    resolve_id_res: Vec<regex::Regex>,
    plugins_watch_change: bool,
    plugins_hot_update: bool,
    html_env: std::collections::BTreeMap<String, String>,
    parsed_fired: Mutex<std::collections::HashSet<String>>,
    rt: tokio::runtime::Handle,
    base: Option<String>,
    optimized: Arc<optimize::OptimizedDeps>,
    /// An error frame broadcast while no client was connected (a compile error
    /// hit by the page's first module requests, before its socket opened) is
    /// kept and delivered to the next client, like Vite's ws `bufferedError`.
    buffered_error: Mutex<Option<String>>,
    /// Modules whose last transform failed on an unresolvable relative import
    /// (Vite's `_hasResolveFailedErrorModules`): a file appearing on disk
    /// re-processes them so the overlay clears once the missing file exists.
    resolve_failed: Mutex<std::collections::HashSet<String>>,
    /// `assets/client.js` with the `server.hmr` options and the socket token
    /// filled in (Vite's clientInjections), rendered once at startup.
    client_js: Bytes,
    /// Validator for `/@oj/client.js` (fixed per process).
    client_js_etag: String,
    /// Per module url, the `import.meta.glob` patterns it expands (absolute):
    /// a file created or deleted under one changes the expansion, so the module
    /// is recompiled and hot updated (Vite's importMetaGlob hotUpdate).
    glob_importers: Mutex<HashMap<String, Vec<glob::Pattern>>>,
    /// Vite's `appType` (`spa` | `mpa` | `custom`): whether an unmatched
    /// navigation falls back to `index.html`, and whether html is served at all.
    app_type: String,
    /// Vite's `server.watch.ignored` as compiled globs (each pattern both as
    /// written and rooted at the project); a change matching one is dropped
    /// before HMR or restart handling.
    watch_ignored: Vec<glob::Pattern>,
    /// Per-process secret a browser page must present as `?token=` to open the
    /// HMR socket (Vite's `webSocketToken`): another origin's page cannot read
    /// update frames or push invalidations. Non-browser clients (no `Origin`)
    /// connect freely, as in Vite.
    ws_token: String,
    ws_token_check: bool,
}

/// Live view of how the plugin host serves requests: the configureServer
/// middleware port and whether runner-backed Vite DevEnvironments serve the
/// documents. Boot fills it from the host's initial serve info; a host whose
/// init outlives the boot deadlines fills it late — the host pushes
/// `{ ojServeInfo }` when ready and [`spawn_late_plugin_serve`] flips this —
/// so the request paths read it per request instead of snapshotting it at boot.
#[derive(Default)]
pub struct PluginServe {
    /// One packed snapshot, so every reader gets (port, runner_environments)
    /// from the same write: low 16 bits = the middleware's loopback port
    /// (0 = none yet), bit 16 = runner environments serve the documents.
    state: std::sync::atomic::AtomicU32,
    /// The activation handler: runs synchronously inside `set` BEFORE a late
    /// activation becomes visible to readers (start_dev marks its fallback
    /// runner dirty here), so no request can observe the flipped mode while
    /// the catch-up is still unarmed.
    on_activate: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Whether a LATE activation happened (a `set` flipping no-middleware to
    /// middleware after the boot fill). Set before the handler runs, so a
    /// caller registering its handler late can catch an activation that beat
    /// the registration by checking this afterwards (see `set_on_activate`).
    late_activated: std::sync::atomic::AtomicBool,
}

const RUNNER_ENVS_BIT: u32 = 1 << 16;

impl std::fmt::Debug for PluginServe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginServe")
            .field("mw_port", &self.mw_port())
            .field("runner_environments", &self.runner_environments())
            .finish()
    }
}

impl PluginServe {
    fn pack(info: &plugins::ServeInfo) -> u32 {
        let port = info.middleware_port.map(u32::from).unwrap_or(0);
        if port != 0 && info.runner_environments {
            port | RUNNER_ENVS_BIT
        } else {
            port
        }
    }
    fn from_info(info: &plugins::ServeInfo) -> Self {
        // The boot fill: not an activation (no reader existed before this
        // value), so it must not count as `late_activated` — a caller's
        // post-registration catch-up check is only for post-boot flips.
        let s = Self::default();
        s.state
            .store(Self::pack(info), std::sync::atomic::Ordering::SeqCst);
        s
    }
    fn set(&self, info: &plugins::ServeInfo) {
        let packed = Self::pack(info);
        // A late activation (no middleware -> middleware up) runs the handler
        // first: a reader that sees the new mode finds the catch-up armed. The
        // flag is set before the handler, so a handler registered a moment too
        // late is caught by the registrar's `activated_late` check instead.
        if packed & 0xFFFF != 0 && self.mw_port().is_none() {
            self.late_activated
                .store(true, std::sync::atomic::Ordering::SeqCst);
            if let Some(hook) = self
                .on_activate
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
            {
                hook();
            }
        }
        self.state
            .store(packed, std::sync::atomic::Ordering::SeqCst);
    }
    /// Register the activation handler (see `on_activate`). At most one; a
    /// registration after activation is never called, so callers registering
    /// late must check `activated_late` afterwards and run their catch-up
    /// inline when it is set.
    pub fn set_on_activate(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *self
            .on_activate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }
    /// Whether a late (post-boot) activation already happened — the check for
    /// a caller whose `set_on_activate` may have lost the race with it.
    pub fn activated_late(&self) -> bool {
        self.late_activated
            .load(std::sync::atomic::Ordering::SeqCst)
    }
    /// The configureServer middleware's loopback port, when it is up.
    pub fn mw_port(&self) -> Option<u16> {
        match self.state.load(std::sync::atomic::Ordering::SeqCst) & 0xFFFF {
            0 => None,
            p => u16::try_from(p).ok(),
        }
    }
    /// Runner-backed Vite DevEnvironments serve the documents (the
    /// Environment-API path, today the Cloudflare plugin): the Start path may
    /// keep its SSR runner cold.
    pub fn runner_environments(&self) -> bool {
        self.state.load(std::sync::atomic::Ordering::SeqCst) & RUNNER_ENVS_BIT != 0
    }
}

/// Waits for the plugin host's late `{ ojServeInfo }` push and flips the shared
/// [`PluginServe`] when it arrives, so a host whose init outlives the boot
/// deadlines still activates the middleware path. Activation is a transition,
/// not just a flag flip: `PluginServe::set` runs the registered activation
/// handler first (start_dev re-arms its fallback runner there), and this task
/// then sends one catch-up resync that full-reloads every runner-backed
/// environment — covering all edits missed while the path was down. A host
/// that never finishes initializing within the init deadline gets a loud
/// warning instead of degrading silently.
///
/// The task outlives activations: a host revived after a wedge boots a fresh
/// middleware server on a fresh port and pushes new serve info (the revive
/// resets the watch to None first), and this same task re-points the
/// forwarding and re-runs the catch-up resync for the edits the dead window
/// swallowed. It exits only when the host is gone with no revives left.
fn spawn_late_plugin_serve(plugin_serve: Arc<PluginServe>, host: Arc<PluginHost>) {
    tokio::spawn(async move {
        let mut updates = host.serve_info_updates();
        let mut gone = host.host_gone_updates();
        let mut warned = false;
        // What was last applied, so a spurious wake re-applies nothing while
        // a revived generation's genuinely new info re-activates. (A revive
        // that lands on the SAME port is applied too when the None reset was
        // observed; an unobserved reset with an identical port only costs the
        // catch-up resync — forwarding already points at the new server.)
        let mut applied: Option<(Option<u16>, bool)> = None;
        // Permanent death makes no watch change of its own (the final failed
        // revive is silent), so a slow re-check bounds how long the task can
        // pin the host after it.
        let mut recheck = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        );
        loop {
            let info = *updates.borrow_and_update();
            if info.is_none() {
                // The revive reset: whatever the next generation pushes is a
                // fresh activation, even on a port equal to the last one.
                applied = None;
            }
            let key = info.map(|i| (i.middleware_port, i.runner_environments));
            if let Some(info) = info.filter(|_| key != applied) {
                applied = key;
                plugin_serve.set(&info);
                if let Some(p) = plugin_serve.mw_port() {
                    println!(
                        "  plugin middleware: forwarding unmatched requests to :{p} (host came up after boot)"
                    );
                    // The catch-up: edits made while the path was down were
                    // never invalidated into the worker environments. The ack
                    // means only "enqueued" (the host answers on enqueue so a
                    // busy queue can't time the client out); "resynced" is
                    // claimed only on the host's completion push — baseline
                    // snapshotted BEFORE the enqueue so a fast completion is
                    // never missed.
                    let mut done = host.resync_done_updates();
                    let baseline = *done.borrow_and_update();
                    if resync_plugin_mw_with_retry(p).await {
                        println!("  plugin middleware: worker environment resync enqueued");
                        let bound = plugins::plugin_rpc_timeout();
                        let enqueued_at = std::time::Instant::now();
                        if await_resync_completion(&mut done, baseline, bound).await {
                            println!(
                                "  plugin middleware: worker environments resynced (full reload)"
                            );
                        } else {
                            eprintln!(
                                "oj: warning: the worker environment resync was enqueued but did not complete within {}s (invalidate queue stuck?); edits made while the plugin middleware was down may be stale until the next edit or a restart",
                                bound.as_secs()
                            );
                            // The warning is bounded, the queue is not: keep
                            // the receiver alive so a resync that drains LATE
                            // is reported with its true delay instead of the
                            // warning reading as permanent staleness. The
                            // host dying ends the wait (this task's Arc pins
                            // the sender, so changed() alone can never see
                            // the death).
                            let host = std::sync::Arc::clone(&host);
                            tokio::spawn(async move {
                                let mut done = done;
                                loop {
                                    if *done.borrow_and_update() > baseline {
                                        println!(
                                            "  plugin middleware: worker environments resynced late, {}s after the enqueue (full reload)",
                                            enqueued_at.elapsed().as_secs()
                                        );
                                        return;
                                    }
                                    tokio::select! {
                                        changed = done.changed() => { if changed.is_err() { return; } }
                                        _ = host.host_gone_wait() => return,
                                    }
                                }
                            });
                        }
                    } else {
                        eprintln!(
                            "oj: warning: the worker environment resync was not acknowledged; edits made while the plugin middleware was down may be stale until the next edit or a restart"
                        );
                    }
                }
            }
            // A death only ends the task when no revive is left; a revivable
            // host keeps it watching for the next generation's push. This
            // task's own Arc<PluginHost> keeps the push channel's sender
            // alive, so `updates.changed()` alone can never observe the host
            // dying — the gone watch and the slow re-check bound the wait.
            if *gone.borrow_and_update() && !host.can_revive() {
                if applied.is_none() {
                    eprintln!("oj: warning: the plugin host exited before initializing; plugin-served routes will not activate");
                } else {
                    eprintln!("oj: warning: the plugin host exited with no respawns left; plugin-served routes are down until the dev server restarts");
                }
                return;
            }
            if warned || applied.is_some() {
                tokio::select! {
                    changed = updates.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    changed = gone.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    _ = recheck.tick() => {}
                }
            } else {
                tokio::select! {
                    changed = updates.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    changed = gone.changed() => {
                        if changed.is_err() {
                            return;
                        }
                    }
                    _ = recheck.tick() => {}
                    _ = tokio::time::sleep_until(host.init_deadline_at()) => {
                        if !host.is_initialized() {
                            eprintln!(
                                "oj: warning: the plugin host did not finish initializing within {}s; plugin-served routes are inactive until it does",
                                plugins::plugin_init_timeout().as_secs()
                            );
                        }
                        warned = true;
                    }
                }
            }
        }
    });
}

pub struct BuiltApp {
    pub router: Router,
    pub host: std::net::IpAddr,
    pub port: u16,
    pub strict_port: bool,
    pub proxy_prefixes: Vec<String>,
    /// Live plugin-middleware state (port + runner environments), shared with
    /// the router's state; a slow plugin host activates it after boot.
    pub plugin_serve: Arc<PluginServe>,
    pub root: PathBuf,
    pub started: Instant,
    /// Sender for the `/__ws` broadcast — the channel the editor reads
    /// HMR + narration frames from. The start path pushes narration here.
    pub reload_tx: broadcast::Sender<String>,
    /// The client plugin host, for the shutdown hooks (buildEnd, closeBundle).
    pub plugin_host: Option<Arc<PluginHost>>,
    /// `server.open`: launch the browser once bound.
    pub open: bool,
    /// The HMR gate (the editor-driven hold), when enabled: the Start
    /// server holds its page reload behind it like the plain path holds updates.
    pub hmr_gate: Option<HmrGateHandle>,
    /// The SSR resolve/load pipeline, for the in-process SSR module runner.
    pub ssr: SsrBridge,
}

pub async fn bind_dev_listener(
    host: std::net::IpAddr,
    preferred: u16,
    strict: bool,
) -> anyhow::Result<(tokio::net::TcpListener, u16)> {
    for port in preferred..=u16::MAX {
        let addr = SocketAddr::from((host, port));
        match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => {
                let local = listener.local_addr();
                let bound = local.as_ref().map(|a| a.port()).unwrap_or(port);
                let interface = local
                    .as_ref()
                    .map(|a| a.ip().to_string())
                    .unwrap_or_else(|_| host.to_string());
                // The plugin hosts' stub httpServer emits "listening" on this
                // (Vite parity: the event means the socket really accepts,
                // and address() reports the real bind).
                plugins::dev_listener_bound(bound, &interface);
                return Ok((listener, bound));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
                if strict {
                    return Err(anyhow::anyhow!("Port {port} is already in use"));
                }
                eprintln!("Port {port} is in use, trying another one...");
            }
            Err(e) => return Err(e).with_context(|| format!("cannot bind {addr}")),
        }
    }
    Err(anyhow::anyhow!(
        "no available port found between {preferred} and 65535"
    ))
}

impl DevServer {
    pub async fn run(self) -> anyhow::Result<()> {
        let built = self.build_app().await?;
        let (listener, port) = bind_dev_listener(built.host, built.port, built.strict_port).await?;
        println!("  {} dev server", oj_brand());
        println!("  root: {}", built.root.display());
        let url = format!("http://localhost:{}/", port);
        println!("  {}", link(&url, &cell(&url)));
        if !built.proxy_prefixes.is_empty() {
            println!("  proxy: {}", built.proxy_prefixes.join(", "));
        }
        println!("  ready in {:?}", built.started.elapsed());
        if built.plugin_host.is_some() {
            tokio::spawn(close_plugins_on_shutdown(built.plugin_host.clone()));
        }
        if built.open {
            open_browser(&url);
        }
        axum::serve(listener, built.router).await?;
        Ok(())
    }

    pub async fn build_app(self) -> anyhow::Result<BuiltApp> {
        let root = self
            .root
            .canonicalize()
            .with_context(|| format!("app root not found: {}", self.root.display()))?;

        if let Some(cfg) = &self.config {
            let cfg = if cfg.is_absolute() {
                cfg.clone()
            } else {
                root.join(cfg)
            };
            plugins::set_vite_config_override(cfg);
        }

        boot_phase("build_app begin");
        prepare_cache_root(&root);
        let dev_mode = self
            .mode
            .clone()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "development".to_string());
        let mut config =
            oj_config::load_with(&root, "serve", &dev_mode).map_err(|e| anyhow::anyhow!("{e}"))?;
        plugins::adopt_vite_config_values(&mut config, &root, "serve", &dev_mode)
            .map_err(|e| anyhow::anyhow!(e))?;
        boot_phase("vite config values adopted");

        // Feed optimizeDeps.include/exclude/needsInterop into partial bundling so
        // the same vite.config field that drives Vite's dep pre-bundle drives oj's.
        {
            let (include, exclude, _entries) = oj_config::optimize_deps_lists(&config);
            pkg_bundle::configure(
                include,
                exclude,
                oj_config::optimize_deps_needs_interop(&config),
            );
        }

        let env_prefixes = oj_config::env_prefixes(&config);
        let env_prefix_refs: Vec<&str> = env_prefixes.iter().map(String::as_str).collect();
        let env_dir = config
            .env_dir
            .as_deref()
            .map(|d| root.join(d))
            .unwrap_or_else(|| root.clone());
        let env = oj_env::load(&env_dir, &dev_mode);
        // Rebuilt after plugin-host boot with the config()-hook env delta, so
        // it stays a closure over the same inputs rather than a one-shot block.
        let build_env_defines = |extra: &std::collections::BTreeMap<String, String>| {
            let merged = oj_env::with_process_env(
                env.clone(),
                std::env::vars().chain(extra.iter().map(|(k, v)| (k.clone(), v.clone()))),
                &env_prefix_refs,
            );
            // Vite defines process.env.NODE_ENV in dev too (nodeEnv = NODE_ENV || mode);
            // without it, library code that reads it throws a ReferenceError in dev.
            // DEV/PROD follow it as well: `NODE_ENV=production vite dev` is PROD.
            let node_env = oj_env::resolve_node_env(
                std::env::var("NODE_ENV").ok().as_deref(),
                &env,
                "development",
            );
            let mut defines = oj_env::import_meta_env_defines(
                &merged,
                &dev_mode,
                node_env != "production",
                config.base.as_deref().unwrap_or("/"),
                &env_prefix_refs,
            );
            defines.extend(oj_config::config_defines(&config));
            defines.extend(oj_config::environment_defines(&config, "client"));
            let node_env_json =
                serde_json::to_string(&node_env).unwrap_or_else(|_| "\"development\"".into());
            for key in [
                "process.env.NODE_ENV",
                "global.process.env.NODE_ENV",
                "globalThis.process.env.NODE_ENV",
            ] {
                if !defines.iter().any(|(k, _)| k == key) {
                    defines.push((key.to_string(), node_env_json.clone()));
                }
            }
            // Later entries win, and the oxc replacer refuses duplicate keys
            // outright (which would silently disable every define), so the
            // list is deduped keeping the last occurrence.
            dedup_defines_last_wins(defines)
        };
        let digest_defines = |defines: &[(String, String)]| {
            let mut hasher = blake3::Hasher::new();
            for (k, v) in defines {
                hasher.update(k.as_bytes());
                hasher.update(&[0]);
                hasher.update(v.as_bytes());
                hasher.update(&[0]);
            }
            hasher.finalize().to_hex().to_string()
        };
        let defines = build_env_defines(&std::collections::BTreeMap::new());
        let mut html_env = oj_env::html_env_map(&defines);
        let mut env_defines_digest = digest_defines(&defines);
        oj_compiler::set_import_meta_env(defines);
        // `environments.ssr.define` layers over the shared define for SSR
        // compiles only (Vite's per-environment define): kept out of the
        // client list so a key defined differently per side (a
        // "client"/"server" marker) does not leak across.
        oj_compiler::set_import_meta_env_ssr(dedup_defines_last_wins(
            oj_config::environment_defines(&config, "ssr"),
        ));

        // BEFORE any engine boots: children spawned by plugin hooks register
        // here (forked deno_process spawn hook, own process group each) so
        // restarts and shutdown can kill whole plugin-spawned trees.
        deno_process::oj_hook::set(child_groups::register, child_groups::unregister);
        let server_cfg = config.server.clone().unwrap_or_default();
        let port = self.port.or(server_cfg.port).unwrap_or(5199);
        let strict_port = oj_config::server_strict_port(&config);
        // The on-disk module cache is experimental and off by default. Opt in
        // with `oj dev --enable-cache` (or OJ_ENABLE_CACHE=1); `--no-cache`
        // (or OJ_NO_CACHE=1) forces it off even when otherwise enabled.
        let env_flag = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty() && v != "0");
        let cache_enabled = self.enable_cache || env_flag("OJ_ENABLE_CACHE");
        let cache_forced_off = self.no_cache || env_flag("OJ_NO_CACHE");
        let persistent_cache = cache_enabled && !cache_forced_off;
        let host = resolve_host(self.host.as_deref().or(server_cfg.host.as_deref()));
        let proxy: Vec<(String, oj_config::ProxyEntry)> = server_cfg
            .proxy
            .clone()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let proxy_regex: Vec<Option<regex::Regex>> = proxy
            .iter()
            .map(|(ctx, _)| proxy_context_regex(ctx))
            .collect();

        // TanStack Start owns its module graph and SSR; oj runs the plugin host
        // only to host configureServer middleware (the editor dev-server bridge),
        // in start mode so the framework plugins' lifecycle hooks are tolerated.
        let is_start = is_tanstack_start_app(&root);
        let plugin_src = plugins::plugin_source(&root);
        let (plugins_path, plugins_format, plugins_label) = match plugin_src {
            Some(plugins::PluginSource::OjPlugins(p)) => {
                let label = p.file_name().unwrap().to_string_lossy().into_owned();
                (Some(p), "oj", label)
            }
            Some(plugins::PluginSource::ViteConfig(p)) => {
                (Some(p), "vite", "vite.config".to_string())
            }
            None => (None, "oj", String::new()),
        };

        // The environment.mode the host hands buildEnvironments, which passes
        // it to vite.resolveConfig verbatim: the deps pre-seed child must
        // resolve with the SAME string or its cache hashes cannot match.
        let host_env_mode = "dev";
        let mut plugin_cfg = serde_json::json!({
            "config": {
                "root": root.display().to_string(),
                "base": config.base.clone().unwrap_or_else(|| "/".into()),
                "mode": dev_mode,
                "command": "serve",
                "define": config.define,
                // `proxy` too: for an oj-config-format app (no vite.config the
                // host can load) this is the only place the host learns the
                // app's `server.proxy`, so the single Node proxy can cover it.
                // The {from,to} rewrite form crosses fine; a FUNCTION rewrite
                // (vite-format only) rides the host's own loaded config instead.
                // `strictPort` too: the resolved config plugins read carries
                // oj's real values, not Vite's defaults.
                "server": { "port": port, "strictPort": strict_port, "host": server_cfg.host, "proxy": server_cfg.proxy },
                // `{}` rather than null when the config has none: the host deep-merges
                // this over the user's Vite-resolved config, and a null would erase
                // its environments (and their per-environment `define`).
                "environments": config.environments.clone().unwrap_or_default(),
            },
            "env": { "command": "serve", "mode": dev_mode },
            "environment": { "name": "client", "mode": host_env_mode },
            "pluginsFormat": plugins_format,
            // Per-environment optimizer include extension, snapshotted by the
            // preseed child from Vite's own prior _metadata.json (deps a
            // plugin injects at runtime, e.g. Cloudflare's unenv polyfills,
            // land there on commit). BOTH the child and buildEnvironments
            // fold this same snapshot into optimizeDeps.include so the
            // seeded metadata's configHash matches — the next cold boot then
            // never re-optimizes in-host (which ended in server.restart()).
            "preseedIncludePath": oj_cache::cache_root(&root).join("preseed-include.json").to_string_lossy(),
            "ojStartMode": is_start,
        });
        if plugins_format == "vite" {
            // The extractor already evaluated the config (boot fails hard when
            // a present vite.config does not extract); its verdict rides the
            // spawn payload. The host treats TRUE as authoritative and
            // sufficient (a host-side hook failure cannot lose the path), while
            // FALSE falls through to the host's own declaration check — a
            // degraded or stale verdict can then never silently disable the
            // worker path the host itself can see declared. Omitted for oj
            // plugin files, where no extraction ran.
            plugin_cfg["runnerBacked"] = serde_json::json!(oj_config::ssr_runner_backed(&config));
        }
        let plugin_config = plugin_cfg.to_string();
        plugin_cfg["environment"]["name"] = serde_json::json!("ssr");
        let ssr_plugin_config = plugin_cfg.to_string();
        // Optimizer quarantine: a runner-backed config boots the app's real
        // Vite DevEnvironments inside the in-process plugin host, and a cold
        // deps cache then runs Vite's dep optimizer — a rolldown build whose
        // native retention is process-scoped — inside oj. Pre-seed the caches
        // in a one-shot child first, so the host finds them warm and never
        // builds in-process (see preseed.rs for the gate and the known gaps).
        if plugins_path.is_some()
            && plugins_format == "vite"
            && oj_config::ssr_runner_backed(&config)
        {
            preseed::preseed_server_deps(&root, host_env_mode).await;
        }
        boot_phase("plugin host spawning");
        let plugin_host = match plugins_path {
            Some(file) => match PluginHost::spawn(&root, &file, &plugin_config).await {
                Ok(host) => {
                    // Every remaining plugin may be one oj reimplements natively
                    // (e.g. @vitejs/plugin-react -> oj does JSX/refresh in oxc). If
                    // nothing is left after that filtering, the host is an idle
                    // Node process sitting on the per-request/HMR path -- drop it
                    // and serve natively. Dropping the Arc kills the process.
                    // EXCEPT when `server.proxy` is configured: the single proxy
                    // lives in the host's middleware stack, and a FUNCTION rewrite
                    // (or `configure`/`bypass`) has no other place to run — keep
                    // the already-spawned host so the proxy always has a Node home
                    // instead of the Rust fallback silently forwarding unstripped.
                    let keep_for_proxy = server_cfg.proxy.as_ref().is_some_and(|p| !p.is_empty());
                    let plugin_count = host.plugin_count().await;
                    if plugin_count == 0 && !keep_for_proxy {
                        host.shutdown();
                        println!("  plugins: {plugins_label} (none active after native filtering; served natively)");
                        None
                    } else if plugin_count == 0 {
                        // Kept only to host the single `server.proxy` (no plugins
                        // to build): the middleware stack runs the proxy so a
                        // function rewrite / configure / bypass has a Node home.
                        println!(
                            "  plugins: {plugins_label} (none active; host kept for server.proxy)"
                        );
                        Some(host)
                    } else {
                        println!("  plugins: {plugins_label}");
                        if !is_start {
                            // Vite awaits the client buildStart while initing the
                            // server; a rejection fails startup rather than serving.
                            if let Err(e) = host.build_start().await {
                                host.shutdown();
                                anyhow::bail!("plugin buildStart failed:\n{e}");
                            }
                        }
                        Some(host)
                    }
                }
                Err(e) => {
                    eprintln!("oj: plugin host failed to start: {e}");
                    None
                }
            },
            None => None,
        };
        boot_phase("plugin host ready");
        let serve_info = match &plugin_host {
            Some(host) => host.serve_info().await,
            None => plugins::ServeInfo::default(),
        };
        let plugin_serve = Arc::new(PluginServe::from_info(&serve_info));
        if let Some(p) = plugin_serve.mw_port() {
            println!("  plugin middleware: forwarding unmatched requests to :{p}");
        } else if let Some(host) = &plugin_host {
            // No middleware port yet: either no plugin registered one, or the
            // host's init outlived the boot deadlines (many plugins, Miniflare
            // inside configureServer). The host pushes its serve info when its
            // init completes — activate the middleware path then, catch the
            // worker environments up, and never degrade silently: a host that
            // never finishes initializing gets a loud warning.
            spawn_late_plugin_serve(Arc::clone(&plugin_serve), Arc::clone(host));
        }
        if let Some(host) = &plugin_host {
            // Fold config()-hook env mutations (e.g. a plugin flipping a VITE_*
            // flag) into the client defines before any module compiles.
            let prefixed: std::collections::BTreeMap<String, String> = host
                .env_delta()
                .await
                .into_iter()
                .filter(|(k, _)| env_prefix_refs.iter().any(|p| k.starts_with(p)))
                .collect();
            // Likewise `define` entries the plugins' config() hooks returned
            // (Vite merges them into config.define; the plugin's value wins).
            let plugin_defines = host.config_defines().await;
            if !prefixed.is_empty() || !plugin_defines.is_empty() {
                let mut defines = build_env_defines(&prefixed);
                defines.retain(|(k, _)| !plugin_defines.iter().any(|(pk, _)| pk == k));
                defines.extend(plugin_defines);
                html_env = oj_env::html_env_map(&defines);
                env_defines_digest = digest_defines(&defines);
                oj_compiler::set_import_meta_env(defines);
            }
        }
        let plugins_use_module_parsed = match &plugin_host {
            Some(host) => host.has_module_parsed().await,
            None => false,
        };
        // The tagger (and other jsx-override/configureServer plugins) have no
        // transform hook, so the per-module transform RPC is a wasted full-source
        // stdio round-trip; skip it when nothing consumes it.
        let plugins_have_transform = match &plugin_host {
            Some(host) => host.has_transform().await,
            None => false,
        };
        let plugins_have_load = match &plugin_host {
            Some(host) => host.has_load().await,
            None => false,
        };
        let dep_transform_res: Vec<regex::Regex> = match &plugin_host {
            Some(host) => host
                .dep_transform_filters()
                .await
                .iter()
                .filter_map(|s| regex::Regex::new(s).ok())
                .collect(),
            None => Vec::new(),
        };
        let dep_load_res: Vec<regex::Regex> = match &plugin_host {
            Some(host) => host
                .dep_load_filters()
                .await
                .iter()
                .filter_map(|s| regex::Regex::new(s).ok())
                .collect(),
            None => Vec::new(),
        };
        let resolve_id_res: Vec<regex::Regex> = match &plugin_host {
            Some(host) => host
                .resolve_id_filters()
                .await
                .iter()
                .filter_map(|s| regex::Regex::new(s).ok())
                .collect(),
            None => Vec::new(),
        };
        // Prime the per-plugin filter plan (the hook_wants_* gates read it
        // live from the host): what the coarse has_* flags above cannot
        // express, so a filtered hook's RPC is skipped for the app modules
        // its filter can never claim.
        if let Some(host) = &plugin_host {
            let _ = host.build_hook_plan().await;
        }
        // Same idea for HMR: a host without watchChange/handleHotUpdate hooks (the
        // tagger case) doesn't need those per-save stdio round-trips.
        let (plugins_watch_change, plugins_hot_update) = match &plugin_host {
            Some(host) => host.hmr_hooks().await,
            None => (false, false),
        };

        let jsx = jsx_config_of(&config);
        let jsx_overrides = match &plugin_host {
            Some(host) => {
                resolve_jsx_overrides(host, &root, jsx.import_source.as_deref().unwrap_or("react"))
                    .await
            }
            None => std::collections::BTreeMap::new(),
        };

        let hmr_gate = {
            let env_on =
                |name: &str| matches!(std::env::var(name).as_deref(), Ok("1") | Ok("true"));
            let enabled = server_cfg.hmr_gate == Some(true)
                || env_on("OJ_HMR_GATE")
                || env_on("LOVABLE_DEV_SERVER");
            if enabled {
                let full_reload = std::env::var("OJ_HMR_FULL_RELOAD")
                    .or_else(|_| std::env::var("LOVABLE_HMR_FULL_RELOAD"))
                    .as_deref()
                    != Ok("false");
                println!(
                    "  hmr gate: on ({})",
                    if full_reload {
                        "full-reload"
                    } else {
                        "granular"
                    }
                );
                Some(Arc::new(HmrGate {
                    full_reload,
                    max_hold: Duration::from_millis(240_000),
                    inner: Mutex::new(GateInner::default()),
                    held_reload: std::sync::atomic::AtomicBool::new(false),
                }))
            } else {
                None
            }
        };

        let hmr_enabled = server_cfg
            .hmr
            .as_ref()
            .map(|h| !h.is_disabled())
            .unwrap_or(true);
        if !hmr_enabled {
            println!("  hmr: disabled (server.hmr: false)");
        }
        let hmr_options = match &server_cfg.hmr {
            Some(oj_config::HmrConfig::Options(o)) => Some(o.clone()),
            _ => None,
        };
        if hmr_options
            .as_ref()
            .is_some_and(|o| o.port.is_some() && o.client_port.is_none())
        {
            println!(
                "  hmr.port is not applied (the socket shares the dev server port); set hmr.clientPort for the port the browser dials"
            );
        }
        let hmr_ws_path = hmr_socket_path(hmr_options.as_ref());
        let ws_token = new_ws_token();
        // An external editor attaches to the socket from a browser page in gated
        // mode, so the token is not demanded there (as with Vite's
        // legacy.skipWebSocketTokenCheck).
        let ws_token_check = hmr_gate.is_none()
            && config
                .legacy
                .as_ref()
                .and_then(|l| l.skip_web_socket_token_check)
                != Some(true);
        let client_js = render_client_js(CLIENT_JS, hmr_options.as_ref(), &hmr_ws_path, &ws_token);
        let app_type = config.app_type.clone().unwrap_or_else(|| "spa".to_string());
        if app_type != "spa" {
            println!("  appType: {app_type}");
        }
        let fs_strict = server_cfg.fs.as_ref().and_then(|f| f.strict) != Some(false);
        if !fs_strict {
            println!("  server.fs.strict: false (files outside the allow list are served)");
        }
        let watch_ignored = watch_ignored_patterns(
            &root,
            server_cfg
                .watch
                .as_ref()
                .and_then(|w| w.ignored.as_deref())
                .unwrap_or(&[]),
        );
        let open = server_cfg.open == Some(true);

        let started = Instant::now();
        let (reload_tx, _) = broadcast::channel::<String>(64);
        let (crawl_tx, crawl_rx) = tokio::sync::watch::channel(false);
        let (write_tx, mut write_rx) =
            tokio::sync::mpsc::channel::<(String, Arc<CachedModule>)>(65536);
        let public_dir = oj_config::public_dir(&config, &root);
        let client_resolver = Arc::new(OjResolver::with_settings(
            &root,
            oj_resolver::ResolveSettings {
                conditions: oj_config::resolve_conditions(&config, "client"),
                alias: oj_config::resolve_alias(&config, "client"),
                dedupe: oj_config::resolve_dedupe(&config),
                extensions: oj_config::resolve_extensions(&config),
                main_fields: oj_config::resolve_main_fields(&config),
                preserve_symlinks: oj_config::resolve_preserve_symlinks(&config),
                server: false,
            },
        ));
        let css_resolve = oj_css::CssResolveConfig {
            root: root.clone(),
            public_dir: public_dir.clone().unwrap_or_default(),
            alias: oj_config::resolve_alias(&config, "client"),
            // Dev lowers to build.cssTarget too (Vite's lightningcss options
            // are resolved once from it); dev output is never minified.
            targets: oj_config::build_css_targets(&config),
            minify: false,
            modules: css_modules_options(&config),
        };
        let state = Arc::new(ServerState {
            persistent_cache,
            root: root.clone(),
            public_dir,
            reload_tx: reload_tx.clone(),
            graph: Mutex::new(ModuleGraph::new()),
            require_resolver: Arc::new(client_resolver.require_variant()),
            resolver: client_resolver,
            ssr_resolver: Arc::new(OjResolver::with_settings(
                &root,
                oj_resolver::ResolveSettings {
                    // This resolver feeds the unbundled Node SSR path.
                    // Conditions never cross runtimes: a runner-backed ssr
                    // environment's list (browser + workerd from the
                    // Cloudflare plugin, via the ssr.resolve sugar) describes
                    // workerd, so this Node consumer takes Vite's Node server
                    // defaults instead; otherwise the environment's own list
                    // applies verbatim, as under Vite.
                    conditions: if oj_config::ssr_runner_backed(&config) {
                        oj_config::node_server_conditions(&config, true)
                    } else {
                        oj_config::resolve_conditions(&config, "ssr")
                    },
                    alias: oj_config::resolve_alias(&config, "ssr"),
                    dedupe: oj_config::resolve_dedupe(&config),
                    extensions: oj_config::resolve_extensions(&config),
                    main_fields: oj_config::resolve_main_fields(&config),
                    preserve_symlinks: oj_config::resolve_preserve_symlinks(&config),
                    // Vite's server environment: no `browser` main field or remap.
                    server: true,
                },
            )),
            cache: PersistentCache::new(oj_cache::cache_root(&root), env!("CARGO_PKG_VERSION"))
                .with_salt_extra(&env_defines_digest)
                // A cached compile embeds the JSX runtime import; a changed
                // importSource/runtime must not serve the old module.
                .with_salt_extra(&format!("jsx={jsx:?}")),
            memory: Mutex::new(MemoryCache::new(memory_cache_budget())),
            mtime_keys: Mutex::new(HashMap::new()),
            compile_locks: Mutex::new(HashMap::new()),
            crawl_done: crawl_rx,
            tailwind: tokio::sync::OnceCell::new(),
            preprocess: tokio::sync::OnceCell::new(),
            svelte: tokio::sync::OnceCell::new(),
            tailwind_urls: Mutex::new(std::collections::HashSet::new()),
            has_postcss: has_postcss_config(&root),
            scss_additional_data: oj_config::css_additional_data(&config, "scss"),
            sass_additional_data: oj_config::css_additional_data(&config, "sass"),
            css_config: config.css.clone(),
            css_resolve,
            csp_nonce: oj_config::html_csp_nonce(&config),
            fs_allow: Arc::new(Mutex::new({
                // Vite: `allow: raw?.fs?.allow ?? [searchForWorkspaceRoot(root)]`. The
                // workspace root is the DEFAULT, not an addition: a user allow list
                // replaces it (so it can narrow serving), and without one workspace
                // packages (shared UI, fonts) are served without per-package entries.
                match server_cfg.fs.as_ref().and_then(|f| f.allow.as_ref()) {
                    Some(allow) => allow
                        .iter()
                        .map(|p| {
                            let pb = PathBuf::from(p);
                            if pb.is_absolute() {
                                pb
                            } else {
                                root.join(&pb)
                            }
                        })
                        .collect(),
                    None => std::iter::once(workspace_root(&root)).collect(),
                }
            })),
            fs_strict: server_cfg
                .fs
                .as_ref()
                .and_then(|f| f.strict)
                .unwrap_or(true),
            fs_deny: compile_fs_deny(&oj_config::server_fs_deny(&config)),
            dir_cache: Arc::new(Mutex::new(DirCache::new())),
            cache_writes: write_tx,
            preload_snapshot: load_graph_snapshot(&root),
            proxy,
            proxy_regex,
            http: reqwest::Client::new(),
            http_insecure: std::sync::OnceLock::new(),
            proxy_tls: [std::sync::OnceLock::new(), std::sync::OnceLock::new()],
            virtual_modules: config.virtual_modules.clone().unwrap_or_default(),
            jsx_overrides,
            jsx,
            host_policy: HostPolicy::from_config(&server_cfg, self.host.as_deref()),
            hmr_gate,
            gate_flush_tx: broadcast::channel::<()>(16).0,
            started_at_ms: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            hmr_enabled,
            plugins: plugin_host.clone(),
            plugin_serve: Arc::clone(&plugin_serve),
            plugins_ssr: tokio::sync::OnceCell::new(),
            ssr_watch: Arc::new(SsrWatchQueue::default()),
            ssr_plugin_config,
            plugin_watched: Arc::new(Mutex::new(std::collections::HashSet::new())),
            plugins_use_module_parsed,
            plugins_have_transform,
            plugins_have_load,
            dep_transform_res,
            dep_load_res,
            resolve_id_res,
            plugins_watch_change,
            plugins_hot_update,
            html_env,
            parsed_fired: Mutex::new(std::collections::HashSet::new()),
            rt: tokio::runtime::Handle::current(),
            base: config.base.clone().filter(|b| b != "/"),
            buffered_error: Mutex::new(None),
            resolve_failed: Mutex::new(std::collections::HashSet::new()),
            client_js_etag: format!("\"{}\"", &blake3::hash(client_js.as_bytes()).to_hex()[..16]),
            client_js: Bytes::from(client_js),
            glob_importers: Mutex::new(HashMap::new()),
            app_type,
            watch_ignored,
            ws_token,
            ws_token_check,
            optimized: Arc::new({
                let (include, exclude, entries) = oj_config::optimize_deps_lists(&config);
                optimize::OptimizedDeps::prepare(
                    &root,
                    env!("CARGO_PKG_VERSION"),
                    optimize::OptimizeInput {
                        no_discovery: config.optimize_deps.as_ref().and_then(|o| o.no_discovery),
                        include,
                        exclude,
                        entries,
                        dedupe: oj_config::resolve_dedupe(&config),
                        alias: oj_config::resolve_alias(&config, "client"),
                        force: oj_config::optimize_deps_force(&config),
                        bundler_options: oj_config::optimize_deps_bundler_options(&config),
                        conditions: oj_config::resolve_conditions(&config, "client"),
                        main_fields: optimize::optimizer_main_fields(&config),
                        extensions: oj_config::resolve_extensions(&config)
                            .unwrap_or_else(oj_resolver::default_extensions),
                        preserve_symlinks: oj_config::resolve_preserve_symlinks(&config),
                        mode: dev_mode.clone(),
                        needs_interop: oj_config::optimize_deps_needs_interop(&config),
                    },
                )
            }),
        });
        pkg_bundle::set_version(state.optimized.version());
        if let Some(host) = &state.plugins {
            host.set_ws_sender(state.reload_tx.clone());
            let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
            host.set_server_events_sender(ev_tx);
            let st = Arc::clone(&state);
            tokio::spawn(async move {
                while let Some(ev) = ev_rx.recv().await {
                    handle_plugin_server_event(&st, &ev).await;
                }
            });
        }
        {
            let state = Arc::clone(&state);
            std::thread::spawn(move || {
                while let Some((key, module)) = write_rx.blocking_recv() {
                    state.cache.put(&key, &module);
                }
            });
        }
        spawn_watcher(Arc::clone(&state));
        let (client_files, ssr_files) = oj_config::server_warmup_files(&config);
        if !client_files.is_empty() || !ssr_files.is_empty() {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                for file in warmup_paths(&state.root, &client_files) {
                    let url = url_of(&state.root, &file);
                    if let Err(error) = ensure_module(&state, &file, &url).await {
                        eprintln!("oj: warmup {url}: {error}");
                    }
                }
                // SSR dev compiles are not cached anywhere (each /@ssr-module
                // request re-transforms), so per-file warmup requests would be
                // thrown away. The durable warm-up is the SSR plugin host
                // itself: spawning it (and priming its hook plan) here moves
                // the multi-second sidecar boot off the first real request.
                if !ssr_files.is_empty() {
                    if let Some(host) = ssr_plugin_host(&state).await {
                        let _ = host.build_hook_plan().await;
                    }
                }
            });
        }
        if self.lazy {
            // Lazy mode (Vite's default): no eager graph crawl. Modules are
            // compiled on demand as the browser requests them, so the first
            // paint only pays for the first route's modules instead of the whole
            // graph up front. Mark the crawl "done" immediately so preload
            // injection and chunk assembly never block waiting for a crawl that
            // will not run; the module graph still fills in per request.
            let _ = crawl_tx.send(true);
        } else {
            spawn_crawl(Arc::clone(&state), crawl_tx);
        }

        let mut app = Router::new()
            .route("/@oj/client.js", get(serve_client_js))
            .route(
                "/@oj/refresh-runtime.js",
                get(|| async { js(REFRESH_RUNTIME_JS) }),
            )
            .route(
                "/@oj/refresh-preamble.js",
                get(|| async { js(REFRESH_PREAMBLE_JS) }),
            )
            .route("/@oj/routes.js", get(serve_oj_routes))
            // OJ_DEBUG_MEM=1: force a full V8 collection in every live
            // engine, so memory probes measure retained heap instead of
            // whatever V8 has not bothered to collect yet (issue #202's
            // GC-before-measuring point, symmetric with probing a Node
            // server through its inspector). 404 unless enabled.
            .route("/@oj/debug/gc", get(debug_gc))
            .route("/@oj/debug/mem", get(debug_mem_stats))
            .route("/@oj/server-fn.js", get(|| async { js(SERVER_FN_JS) }))
            .route(
                "/@oj/lingui-macro-shim.js",
                get(|| async { js(LINGUI_MACRO_SHIM_JS) }),
            )
            .route("/@ssr-resolve", get(ssr_resolve))
            .route("/@ssr-module", get(ssr_module))
            .route("/__ws", get(ws_upgrade))
            .route("/__hmr_flush", post(hmr_flush))
            .route("/__hmr_gate", get(hmr_gate_status))
            .fallback(serve_fallback);
        // `server.hmr.path`: the client dials this path instead of /__ws (Vite
        // serves its socket at base + hmr.path).
        if hmr_ws_path != "/__ws" && hmr_ws_path != "/" && !hmr_ws_path.starts_with("/@oj/") {
            app = app.route(&hmr_ws_path, get(ws_upgrade));
        }
        // Layer order is reversed at request time (the last layer added runs
        // first). Vite's middleware sequence is cors, then host validation,
        // then proxy: proxied requests must not bypass either gate.
        if !state.proxy.is_empty() {
            app = app.layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                proxy_middleware,
            ));
        }
        app = app.layer(axum::middleware::from_fn_with_state(
            Arc::clone(&state),
            vite_hmr_upgrade,
        ));
        let extra_headers: Vec<(header::HeaderName, header::HeaderValue)> = config
            .server
            .as_ref()
            .and_then(|s| s.headers.as_ref())
            .map(|h| {
                h.iter()
                    .filter_map(|(k, v)| Some((k.parse().ok()?, v.parse().ok()?)))
                    .collect()
            })
            .unwrap_or_default();
        if !extra_headers.is_empty() {
            app = app.layer(axum::middleware::from_fn_with_state(
                Arc::new(extra_headers),
                apply_dev_headers,
            ));
        }
        if !state.host_policy.allow_all {
            app = app.layer(axum::middleware::from_fn_with_state(
                Arc::clone(&state),
                host_check_middleware,
            ));
        }
        if let Some(cors) = CorsPolicy::from_config(server_cfg.cors.as_ref()) {
            app = app.layer(axum::middleware::from_fn_with_state(
                Arc::new(cors),
                cors_middleware,
            ));
        }
        let proxy_prefixes: Vec<String> = state.proxy.iter().map(|(p, _)| p.clone()).collect();
        let hmr_gate = state.hmr_gate.as_ref().map(|_| HmrGateHandle {
            state: Arc::clone(&state),
        });
        let ssr = SsrBridge {
            state: Arc::clone(&state),
        };
        let app = app.with_state(state);

        Ok(BuiltApp {
            router: app,
            host,
            port,
            strict_port,
            proxy_prefixes,
            plugin_serve,
            root,
            started,
            reload_tx,
            plugin_host,
            open,
            hmr_gate,
            ssr,
        })
    }
}

pub fn warmup_paths(root: &Path, patterns: &[String]) -> Vec<PathBuf> {
    // Patterns are root-relative (Vite's warmup semantics). Exclusions match
    // against the ROOT-RELATIVE path of each walked file, so a './' spelled in
    // either side (or a root containing glob metacharacters) can never make a
    // positive and a negative pattern disagree about the same file.
    let normalize = |p: &str| p.trim_start_matches("./").to_string();
    let mut files = std::collections::BTreeSet::new();
    let mut excluded = Vec::new();
    for pattern in patterns {
        let (negative, pattern) = pattern
            .strip_prefix('!')
            .map(|p| (true, p))
            .unwrap_or((false, pattern.as_str()));
        let pattern = normalize(pattern);
        if negative {
            if let Ok(pattern) = glob::Pattern::new(&pattern) {
                excluded.push(pattern);
            }
        } else {
            let walk = format!(
                "{}/{}",
                glob::Pattern::escape(&root.to_string_lossy()),
                pattern
            );
            if let Ok(matches) = glob::glob(&walk) {
                files.extend(matches.flatten().filter(|file| file.is_file()));
            }
        }
    }
    files
        .into_iter()
        .filter(|file| {
            let rel = file.strip_prefix(root).unwrap_or(file);
            !excluded.iter().any(|p| p.matches_path(rel))
        })
        .collect()
}

/// Vite's dev server close runs the plugin container's `buildEnd` then
/// `closeBundle` (pluginContainer.close), so plugins that hold resources or
/// write summaries on shutdown get to. oj has no graceful drain (HMR sockets
/// would hold it open), so the hooks run on the signal and the process exits
/// with the shell's conventional code; a hung plugin is cut off after a bound.
async fn close_plugins_on_shutdown(host: Option<Arc<PluginHost>>) {
    #[cfg(unix)]
    let code = {
        use tokio::signal::unix::{signal, SignalKind};
        // SIGHUP too: own-group children no longer share the terminal's
        // session, so a closed terminal/SSH drop must be forwarded like ^C
        // or every plugin runtime is orphaned.
        match (
            signal(SignalKind::terminate()),
            signal(SignalKind::hangup()),
        ) {
            (Ok(mut term), Ok(mut hup)) => tokio::select! {
                _ = tokio::signal::ctrl_c() => 130,
                _ = term.recv() => 143,
                _ = hup.recv() => 129,
            },
            _ => {
                let _ = tokio::signal::ctrl_c().await;
                130
            }
        }
    };
    #[cfg(not(unix))]
    let code = {
        let _ = tokio::signal::ctrl_c().await;
        130
    };
    if let Some(host) = host {
        let _ = tokio::time::timeout(Duration::from_secs(5), async {
            if let Err(e) = host.build_end(None).await {
                eprintln!("oj: plugin buildEnd on close failed: {e}");
            }
            if let Err(e) = host.close_bundle().await {
                eprintln!("oj: plugin closeBundle on close failed: {e}");
            }
        })
        .await;
    }
    // Own-group children no longer sit in the terminal's foreground group, so
    // the signal that ends oj never reaches them — forward it.
    child_groups::kill_all();
    std::process::exit(code);
}



pub fn resolve_host(host: Option<&str>) -> std::net::IpAddr {
    match host {
        Some("true") | Some("0.0.0.0") | Some("::") | Some("[::]") => [0, 0, 0, 0].into(),
        Some("localhost") | None => [127, 0, 0, 1].into(),
        Some(h) => h.parse().unwrap_or([127, 0, 0, 1].into()),
    }
}



async fn serve_oj_routes(State(state): State<Arc<ServerState>>) -> Response {
    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir_cache = Arc::clone(&state.dir_cache);
    let synthetic = root.join("oj-routes.tsx");
    let compile_opts = dev_compile_opts(&state);
    let compiled = tokio::task::spawn_blocking(move || {
        let dir = root.clone();
        let mut rewrite =
            |s: &str| rewrite_specifier(&root, &dir, &resolver, &fs_allow, &dir_cache, s, true);
        oj_compiler::compile_module(&synthetic, OJ_ROUTES_JS, &compile_opts, Some(&mut rewrite))
            .map(|o| o.code_with_inline_map())
            .map_err(|e| format!("{e}"))
    })
    .await;
    match compiled {
        Ok(Ok(code)) => (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            code,
        )
            .into_response(),
        Ok(Err(e)) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj: routes manifest: {e}"),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("compile task failed: {e}"),
        )
            .into_response(),
    }
}

fn dev_compile_opts(state: &ServerState) -> oj_compiler::CompileOptions {
    let mut opts = oj_compiler::CompileOptions::dev();
    opts.jsx = state.jsx.clone();
    opts
}

/// The compiler's JSX settings for a config (`oxc.jsx` / `esbuild.jsx*`).
pub fn jsx_config_of(config: &oj_config::OjConfig) -> oj_compiler::JsxConfig {
    let s = oj_config::jsx_settings(config);
    oj_compiler::JsxConfig {
        runtime: s.runtime,
        import_source: s.import_source,
        pragma: s.pragma,
        pragma_frag: s.pragma_frag,
    }
}

async fn resolve_jsx_overrides(
    host: &PluginHost,
    root: &Path,
    import_source: &str,
) -> std::collections::BTreeMap<String, String> {
    let mut overrides = std::collections::BTreeMap::new();
    let importer = root.join("index.html");
    let importer = importer.to_string_lossy();
    for spec in [
        format!("{import_source}/jsx-dev-runtime"),
        format!("{import_source}/jsx-runtime"),
    ] {
        if let Ok(Some(id)) = host.resolve_id(&spec, &importer).await {
            if id != spec {
                overrides.insert(spec, id);
            }
        }
    }
    overrides
}



#[cfg(test)]
mod tests;
#[cfg(test)]
mod adapter_tests;
