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
use util::*;
mod plugin_routes;
use plugin_routes::*;
mod ui;
pub use ui::*;
mod client_js;
use client_js::*;
mod assets_serve;
use assets_serve::*;
mod rewrite;
use rewrite::*;
mod deps;
use deps::*;
mod module_pipeline;
pub use module_pipeline::*;
mod memory_cache;
use memory_cache::*;
mod serve;
use serve::*;
mod crawl;
use crawl::*;
mod debug;
use debug::*;
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
use css_engine::CssEngine;
use middleware::*;
use oj_graph::{HmrDecision, ModuleGraph};
use oj_resolver::OjResolver;
use plugins::PluginHost;
pub use preview::*;
pub use proxy::*;
use sidecar::is_tailwind_css;
use tokio::sync::broadcast;

const CLIENT_JS: &str = include_str!("assets/client.js");
pub const OJ_ROUTES_JS: &str = include_str!("assets/oj-routes.js");
const SERVER_FN_JS: &str = include_str!("assets/server-fn.js");
const LINGUI_MACRO_SHIM_JS: &str = include_str!("assets/lingui-macro-shim.mjs");
const REFRESH_RUNTIME_JS: &str = include_str!("assets/refresh-runtime.js");
const REFRESH_PREAMBLE_JS: &str = include_str!("assets/refresh-preamble.js");
// Vite's DEFAULT_EXTENSIONS probe order (js before ts) so the extensionless
// quick path agrees with the resolver; .cts/.svelte are compilable, not probed.
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

/// Persistent V8 code-cache dir for embedded engines, keyed by the V8 ABI and
/// NOT the oj version: per-entry source hashes already invalidate changed scripts.
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

type Defines = Vec<(String, String)>;

/// Deduplicate a define list keeping the LAST occurrence of each key (later
/// layers override earlier ones, as in Vite's define merge).
fn dedup_defines_last_wins(defines: Defines) -> Defines {
    let mut index: HashMap<String, usize> = HashMap::with_capacity(defines.len());
    let mut out: Defines = Vec::with_capacity(defines.len());
    for (k, v) in defines {
        match index.get(&k) {
            Some(&i) => out[i].1 = v,
            None => {
                index.insert(k.clone(), out.len());
                out.push((k, v));
            }
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
    /// `require()` specifiers of a directly-served CommonJS dep (Vite's getConditions).
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
    /// rustls client configs for proxied `wss://` targets, built once per server:
    /// index 0 verifies, index 1 is `secure: false`.
    proxy_tls: [std::sync::OnceLock<Result<std::sync::Arc<rustls::ClientConfig>, String>>; 2],
    virtual_modules: std::collections::BTreeMap<String, String>,
    /// The keys of `virtual_modules`, shared with per-request rewrite closures
    /// (the map is fixed at boot; cloning the keyset per compile was waste).
    virtual_ids: Arc<std::collections::BTreeSet<String>>,
    /// See `canonicalize_memo`.
    canon_memo: Mutex<std::collections::HashMap<String, PathBuf>>,
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
    /// The CLI's `--config` file (resolved against root), for the lazily
    /// spawned SSR host's plugin-source pick.
    config_file: Option<PathBuf>,
    /// The engines the debug GC endpoint fans over (plugin hosts, CSS, SSR,
    /// Start, addon keeper); every long-lived spawn joins it.
    engine_registry: oj_js::EngineRegistry,
    plugins: Option<std::sync::Arc<PluginHost>>,
    plugin_serve: Arc<PluginServe>,
    plugins_ssr: tokio::sync::OnceCell<Option<std::sync::Arc<PluginHost>>>,
    /// Watcher events the lazily spawned SSR host could not take yet, plus
    /// the dispatch-order lock; see [`SsrWatchQueue`].
    ssr_watch: Arc<SsrWatchQueue>,
    ssr_plugin_config: String,
    plugin_watched: Arc<Mutex<std::collections::HashSet<PathBuf>>>,
    /// Into the watcher thread's inbox (`WatchMsg`): directories of files
    /// served from outside the root (`ensure_watched_file`).
    watch_tx: std::sync::mpsc::Sender<WatchMsg>,
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
    /// An error frame broadcast while no client was connected is kept and
    /// delivered to the next client (Vite's ws `bufferedError`).
    buffered_error: Mutex<Option<String>>,
    /// Modules whose last transform failed on an unresolvable relative import
    /// (Vite's `_hasResolveFailedErrorModules`): a file appearing on disk re-processes them.
    resolve_failed: Mutex<std::collections::HashSet<String>>,
    /// `assets/client.js` with the `server.hmr` options and the socket token
    /// filled in (Vite's clientInjections), rendered once at startup.
    client_js: Bytes,
    /// Validator for `/@oj/client.js` (fixed per process).
    client_js_etag: String,
    /// Per module url, the `import.meta.glob` patterns it expands (absolute): a file
    /// created or deleted under one recompiles + hot-updates the module (Vite parity).
    glob_importers: Mutex<HashMap<String, Vec<glob::Pattern>>>,
    /// Vite's `appType` (`spa` | `mpa` | `custom`): whether an unmatched
    /// navigation falls back to `index.html`, and whether html is served at all.
    app_type: String,
    /// Vite's `server.watch.ignored` as compiled globs (each pattern as written and
    /// rooted at the project); a matching change is dropped before HMR or restart handling.
    watch_ignored: Vec<glob::Pattern>,
    /// Per-process secret a browser page must present as `?token=` to open the HMR
    /// socket (Vite's `webSocketToken`); non-browser clients (no `Origin`) connect freely.
    ws_token: String,
    ws_token_check: bool,
}

/// Live plugin-host serve state (configureServer middleware port + whether runner-backed
/// DevEnvironments serve the documents), read per request: a host whose init outlives the
/// boot deadlines fills it late via [`spawn_late_plugin_serve`].
#[derive(Default)]
pub struct PluginServe {
    /// One packed snapshot so every reader gets (port, runner_environments) from the same
    /// write: low 16 bits = middleware loopback port (0 = none), bit 16 = runner envs.
    state: std::sync::atomic::AtomicU32,
    /// The activation handler: runs synchronously inside `set` BEFORE a late activation
    /// becomes visible, so no request can observe the flipped mode while the catch-up is unarmed.
    on_activate: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Whether a LATE activation happened. Set before the handler runs, so a late
    /// registrar can still catch an activation that beat it (see `set_on_activate`).
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
        // The boot fill is not an activation (no reader existed before this
        // value), so it must not count as `late_activated`.
        let s = Self::default();
        s.state
            .store(Self::pack(info), std::sync::atomic::Ordering::SeqCst);
        s
    }
    fn set(&self, info: &plugins::ServeInfo) {
        let packed = Self::pack(info);
        // A late activation runs the handler first: a reader seeing the new mode finds
        // the catch-up armed. The flag precedes the handler for late registrars.
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
    /// Register the activation handler (see `on_activate`). At most one; a registration
    /// after activation is never called, so late callers must check `activated_late` after.
    pub fn set_on_activate(&self, hook: Box<dyn Fn() + Send + Sync>) {
        *self
            .on_activate
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(hook);
    }
    /// Whether a late (post-boot) activation already happened: the check for
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
    /// Runner-backed Vite DevEnvironments serve the documents (the Environment-API
    /// path): the Start path may keep its SSR runner cold.
    pub fn runner_environments(&self) -> bool {
        self.state.load(std::sync::atomic::Ordering::SeqCst) & RUNNER_ENVS_BIT != 0
    }
}

/// Flips [`PluginServe`] on the host's late `{ ojServeInfo }` push: `set` runs the
/// activation handler, then one catch-up resync full-reloads runner-backed environments
/// for edits missed while down. Survives revives; exits only when the host is gone for good.
fn spawn_late_plugin_serve(plugin_serve: Arc<PluginServe>, host: Arc<PluginHost>) {
    tokio::spawn(async move {
        let mut updates = host.serve_info_updates();
        let mut gone = host.host_gone_updates();
        let mut warned = false;
        // Last applied info: a spurious wake re-applies nothing, while a revived
        // generation's new info re-activates (the revive resets the watch to None first).
        let mut applied: Option<(Option<u16>, bool)> = None;
        // Permanent death makes no watch change of its own, so a slow re-check
        // bounds how long the task can pin the host after it.
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
                    catch_up_resync(&host, p).await;
                }
            }
            // A death ends the task only when no revive is left. This task's own Arc keeps
            // the sender alive, so `updates.changed()` alone can never observe the death.
            if *gone.borrow_and_update() && !host.can_revive() {
                if applied.is_none() {
                    eprintln!("oj: warning: the plugin host exited before initializing; plugin-served routes will not activate");
                } else {
                    eprintln!("oj: warning: the plugin host exited with no respawns left; plugin-served routes are down until the dev server restarts");
                }
                return;
            }
            let await_init = !warned && applied.is_none();
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
                _ = tokio::time::sleep_until(host.init_deadline_at()), if await_init => {
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
    });
}

/// The catch-up resync after a (re)activation on port `p`. The ack means only
/// "enqueued"; "resynced" is claimed only on the host's completion push, baseline
/// snapshotted BEFORE the enqueue.
async fn catch_up_resync(host: &Arc<PluginHost>, p: u16) {
    let mut done = host.resync_done_updates();
    let baseline = *done.borrow_and_update();
    if !resync_plugin_mw_with_retry(p).await {
        eprintln!(
            "oj: warning: the worker environment resync was not acknowledged; edits made while the plugin middleware was down may be stale until the next edit or a restart"
        );
        return;
    }
    println!("  plugin middleware: worker environment resync enqueued");
    let bound = plugins::plugin_rpc_timeout();
    let enqueued_at = std::time::Instant::now();
    if await_resync_completion(&mut done, baseline, bound).await {
        println!("  plugin middleware: worker environments resynced (full reload)");
        return;
    }
    eprintln!(
        "oj: warning: the worker environment resync was enqueued but did not complete within {}s (invalidate queue stuck?); edits made while the plugin middleware was down may be stale until the next edit or a restart",
        bound.as_secs()
    );
    // Keep the receiver alive so a LATE drain is reported with its
    // true delay; the gone watch ends the wait (this Arc pins the sender).
    let host = Arc::clone(host);
    tokio::spawn(async move {
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
    /// Sender for the `/__ws` broadcast: the channel the editor reads
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
                // Plugin hosts' stub httpServer emits "listening" on this (Vite parity:
                // the socket really accepts, address() reports the real bind).
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

/// Inputs of the client `import.meta.env` / `process.env.NODE_ENV` defines. Rebuilt
/// after plugin-host boot with the config()-hook env delta.
struct ClientDefines<'a> {
    env: Vec<(String, String)>,
    prefixes: Vec<String>,
    config: &'a oj_config::OjConfig,
    mode: &'a str,
}

impl ClientDefines<'_> {
    fn has_prefix(&self, key: &str) -> bool {
        self.prefixes.iter().any(|p| key.starts_with(p.as_str()))
    }

    fn build(&self, extra: &std::collections::BTreeMap<String, String>) -> Defines {
        let prefixes: Vec<&str> = self.prefixes.iter().map(String::as_str).collect();
        let merged = oj_env::with_process_env(
            self.env.clone(),
            std::env::vars().chain(extra.iter().map(|(k, v)| (k.clone(), v.clone()))),
            &prefixes,
        );
        // Vite defines process.env.NODE_ENV in dev too (nodeEnv = NODE_ENV || mode);
        // DEV/PROD follow it: `NODE_ENV=production vite dev` is PROD.
        let node_env = oj_env::resolve_node_env(
            std::env::var("NODE_ENV").ok().as_deref(),
            &self.env,
            "development",
        );
        let mut defines = oj_env::import_meta_env_defines(
            &merged,
            self.mode,
            node_env != "production",
            self.config.base.as_deref().unwrap_or("/"),
            &prefixes,
        );
        defines.extend(oj_config::config_defines(self.config));
        defines.extend(oj_config::environment_defines(self.config, "client"));
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
        // The oxc replacer refuses duplicate keys outright (silently disabling
        // every define), so dedupe keeping the last occurrence.
        dedup_defines_last_wins(defines)
    }
}

/// What installing a client define list yields for the server state.
struct InstalledDefines {
    html_env: std::collections::BTreeMap<String, String>,
    /// Salts the persistent cache: a cached compile embeds the defines.
    digest: String,
}

fn install_client_defines(defines: Defines) -> InstalledDefines {
    let html_env = oj_env::html_env_map(&defines);
    let mut hasher = blake3::Hasher::new();
    for (k, v) in &defines {
        hasher.update(k.as_bytes());
        hasher.update(&[0]);
        hasher.update(v.as_bytes());
        hasher.update(&[0]);
    }
    let digest = hasher.finalize().to_hex().to_string();
    oj_compiler::set_import_meta_env(defines);
    InstalledDefines { html_env, digest }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PluginsFormat {
    Oj,
    Vite,
}

impl PluginsFormat {
    fn as_str(self) -> &'static str {
        match self {
            PluginsFormat::Oj => "oj",
            PluginsFormat::Vite => "vite",
        }
    }
}

struct PluginFile {
    path: PathBuf,
    format: PluginsFormat,
    label: String,
}

fn plugin_file(root: &Path, config_file: Option<&Path>) -> Option<PluginFile> {
    match plugins::plugin_source(root, config_file)? {
        plugins::PluginSource::OjPlugins(p) => Some(PluginFile {
            label: p.file_name().unwrap().to_string_lossy().into_owned(),
            path: p,
            format: PluginsFormat::Oj,
        }),
        plugins::PluginSource::ViteConfig(p) => Some(PluginFile {
            path: p,
            format: PluginsFormat::Vite,
            label: "vite.config".to_string(),
        }),
    }
}

/// Handed to buildEnvironments and vite.resolveConfig verbatim: the deps
/// pre-seed child must resolve with the SAME string or cache hashes cannot match.
const HOST_ENV_MODE: &str = "dev";

/// The plugin host spawn payloads for the client and ssr environments.
struct PluginHostPayloads {
    client: String,
    ssr: String,
}

fn plugin_host_payloads(
    root: &Path,
    config: &oj_config::OjConfig,
    server_cfg: &oj_config::ServerConfig,
    dev_mode: &str,
    port: u16,
    format: PluginsFormat,
    is_start: bool,
) -> PluginHostPayloads {
    let mut cfg = serde_json::json!({
        "config": {
            "root": root.display().to_string(),
            "base": config.base.clone().unwrap_or_else(|| "/".into()),
            "mode": dev_mode,
            "command": "serve",
            "define": config.define,
            // For an oj-config-format app this is the only place the host learns
            // `server.proxy` ({from,to} rewrites cross; a FUNCTION rewrite rides the
            // host's own loaded config). strictPort etc: plugins read oj's real values.
            "server": {
                "port": port,
                "strictPort": oj_config::server_strict_port(config),
                "host": server_cfg.host,
                "proxy": server_cfg.proxy,
            },
            // `{}` rather than null: the host deep-merges this over the user's resolved
            // config, and null would erase its environments (and per-environment define).
            "environments": config.environments.clone().unwrap_or_default(),
        },
        "env": { "command": "serve", "mode": dev_mode },
        "environment": { "name": "client", "mode": HOST_ENV_MODE },
        "pluginsFormat": format.as_str(),
        // Optimizer include snapshot from Vite's prior _metadata.json: BOTH the preseed
        // child and buildEnvironments fold it into optimizeDeps.include so the seeded
        // configHash matches and a cold boot never re-optimizes in-host (server.restart()).
        "preseedIncludePath": oj_cache::cache_root(root).join("preseed-include.json").to_string_lossy(),
        "ojStartMode": is_start,
    });
    if format == PluginsFormat::Vite {
        // The extractor's verdict rides the spawn payload: TRUE is authoritative, FALSE
        // falls through to the host's own declaration check; omitted for oj plugin files.
        cfg["runnerBacked"] = serde_json::json!(oj_config::ssr_runner_backed(config));
    }
    let client = cfg.to_string();
    cfg["environment"]["name"] = serde_json::json!("ssr");
    PluginHostPayloads {
        client,
        ssr: cfg.to_string(),
    }
}

/// Spawn the client plugin host. Nothing left after native filtering = an idle
/// process on the hot path: drop it (dropping the Arc kills it). EXCEPT with
/// `server.proxy`: a function rewrite/configure/bypass needs a JS home.
async fn spawn_plugin_host(
    root: &Path,
    file: &PluginFile,
    payload: &str,
    registry: &oj_js::EngineRegistry,
    keep_for_proxy: bool,
    is_start: bool,
) -> anyhow::Result<Option<Arc<PluginHost>>> {
    let label = &file.label;
    let host = match PluginHost::spawn(root, &file.path, payload, Some(registry.clone())).await {
        Ok(host) => host,
        Err(e) => {
            eprintln!("oj: plugin host failed to start: {e}");
            return Ok(None);
        }
    };
    let plugin_count = host.plugin_count().await;
    if plugin_count == 0 && !keep_for_proxy {
        host.shutdown();
        println!("  plugins: {label} (none active after native filtering; served natively)");
        return Ok(None);
    }
    if plugin_count == 0 {
        // Kept only to host `server.proxy` in the middleware stack.
        println!("  plugins: {label} (none active; host kept for server.proxy)");
        return Ok(Some(host));
    }
    println!("  plugins: {label}");
    if !is_start {
        // Vite awaits the client buildStart while initing the
        // server; a rejection fails startup rather than serving.
        if let Err(e) = host.build_start().await {
            host.shutdown();
            anyhow::bail!("plugin buildStart failed:\n{e}");
        }
    }
    Ok(Some(host))
}

/// Fold config()-hook env mutations (e.g. a plugin flipping a VITE_* flag) and the
/// `define` entries the hooks returned (Vite merges them into config.define; the
/// plugin's value wins) into the client defines before any module compiles.
async fn fold_plugin_defines(
    host: &PluginHost,
    defines: &ClientDefines<'_>,
) -> Option<InstalledDefines> {
    let prefixed: std::collections::BTreeMap<String, String> = host
        .env_delta()
        .await
        .into_iter()
        .filter(|(k, _)| defines.has_prefix(k))
        .collect();
    let plugin_defines = host.config_defines().await;
    if prefixed.is_empty() && plugin_defines.is_empty() {
        return None;
    }
    let overridden: std::collections::HashSet<&str> =
        plugin_defines.iter().map(|(k, _)| k.as_str()).collect();
    let mut merged = defines.build(&prefixed);
    merged.retain(|(k, _)| !overridden.contains(k.as_str()));
    merged.extend(plugin_defines.iter().cloned());
    Some(install_client_defines(merged))
}

/// Which plugin hooks exist, so per-module and per-save RPCs nothing consumes
/// are skipped (the tagger case: no transform, no HMR hooks).
#[derive(Default)]
struct PluginCaps {
    use_module_parsed: bool,
    have_transform: bool,
    have_load: bool,
    dep_transform_res: Vec<regex::Regex>,
    dep_load_res: Vec<regex::Regex>,
    resolve_id_res: Vec<regex::Regex>,
    watch_change: bool,
    hot_update: bool,
}

fn compile_filters(patterns: &[String]) -> Vec<regex::Regex> {
    patterns
        .iter()
        .filter_map(|s| regex::Regex::new(s).ok())
        .collect()
}

impl PluginCaps {
    async fn probe(host: &PluginHost) -> Self {
        let use_module_parsed = host.has_module_parsed().await;
        let have_transform = host.has_transform().await;
        let have_load = host.has_load().await;
        let dep_transform_res = compile_filters(&host.dep_transform_filters().await);
        let dep_load_res = compile_filters(&host.dep_load_filters().await);
        let resolve_id_res = compile_filters(&host.resolve_id_filters().await);
        // Prime the per-plugin filter plan (hook_wants_* gates read it live): a filtered
        // hook's RPC is skipped for app modules its filter can never claim.
        let _ = host.build_hook_plan().await;
        let (watch_change, hot_update) = host.hmr_hooks().await;
        PluginCaps {
            use_module_parsed,
            have_transform,
            have_load,
            dep_transform_res,
            dep_load_res,
            resolve_id_res,
            watch_change,
            hot_update,
        }
    }
}

fn hmr_gate_of(server_cfg: &oj_config::ServerConfig) -> Option<Arc<HmrGate>> {
    let env_on = |name: &str| matches!(std::env::var(name).as_deref(), Ok("1") | Ok("true"));
    let enabled =
        server_cfg.hmr_gate == Some(true) || env_on("OJ_HMR_GATE") || env_on("LOVABLE_DEV_SERVER");
    if !enabled {
        return None;
    }
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
}

struct HmrSetup {
    enabled: bool,
    ws_path: String,
    ws_token: String,
    ws_token_check: bool,
    client_js: String,
}

fn hmr_setup(
    config: &oj_config::OjConfig,
    server_cfg: &oj_config::ServerConfig,
    gated: bool,
) -> HmrSetup {
    let enabled = server_cfg
        .hmr
        .as_ref()
        .map(|h| !h.is_disabled())
        .unwrap_or(true);
    if !enabled {
        println!("  hmr: disabled (server.hmr: false)");
    }
    let options = match &server_cfg.hmr {
        Some(oj_config::HmrConfig::Options(o)) => Some(o.clone()),
        _ => None,
    };
    if options
        .as_ref()
        .is_some_and(|o| o.port.is_some() && o.client_port.is_none())
    {
        println!(
            "  hmr.port is not applied (the socket shares the dev server port); set hmr.clientPort for the port the browser dials"
        );
    }
    let ws_path = hmr_socket_path(options.as_ref());
    let ws_token = new_ws_token();
    // In gated mode an external editor attaches from a browser page, so the token
    // is not demanded there (as with Vite's legacy.skipWebSocketTokenCheck).
    let ws_token_check = !gated
        && config
            .legacy
            .as_ref()
            .and_then(|l| l.skip_web_socket_token_check)
            != Some(true);
    let client_js = render_client_js(CLIENT_JS, options.as_ref(), &ws_path, &ws_token);
    HmrSetup {
        enabled,
        ws_path,
        ws_token,
        ws_token_check,
        client_js,
    }
}

fn client_resolver(root: &Path, config: &oj_config::OjConfig) -> OjResolver {
    OjResolver::with_settings(
        root,
        oj_resolver::ResolveSettings {
            conditions: oj_config::resolve_conditions(config, "client"),
            alias: oj_config::resolve_alias(config, "client"),
            dedupe: oj_config::resolve_dedupe(config),
            extensions: oj_config::resolve_extensions(config),
            main_fields: oj_config::resolve_main_fields(config),
            preserve_symlinks: oj_config::resolve_preserve_symlinks(config),
            server: false,
        },
    )
}

/// Feeds the unbundled Node SSR path. Conditions never cross runtimes: a
/// runner-backed ssr environment's list describes workerd, so this Node
/// consumer takes Vite's Node server defaults instead.
fn ssr_resolver(root: &Path, config: &oj_config::OjConfig) -> OjResolver {
    OjResolver::with_settings(
        root,
        oj_resolver::ResolveSettings {
            conditions: if oj_config::ssr_runner_backed(config) {
                oj_config::node_server_conditions(config, true)
            } else {
                oj_config::resolve_conditions(config, "ssr")
            },
            alias: oj_config::resolve_alias(config, "ssr"),
            dedupe: oj_config::resolve_dedupe(config),
            extensions: oj_config::resolve_extensions(config),
            main_fields: oj_config::resolve_main_fields(config),
            preserve_symlinks: oj_config::resolve_preserve_symlinks(config),
            // Vite's server environment: no `browser` main field or remap.
            server: true,
        },
    )
}

/// Vite: `allow: raw?.fs?.allow ?? [searchForWorkspaceRoot(root)]`. The workspace
/// root is the DEFAULT, not an addition: a user allow list replaces it.
fn fs_allow_list(
    root: &Path,
    server_cfg: &oj_config::ServerConfig,
) -> std::collections::HashSet<PathBuf> {
    let roots: Vec<PathBuf> = match server_cfg.fs.as_ref().and_then(|f| f.allow.as_ref()) {
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
        None => vec![workspace_root(root)],
    };
    let mut set = std::collections::HashSet::new();
    for r in roots {
        insert_allow_root(&mut set, r);
    }
    set
}

fn optimized_deps(
    root: &Path,
    config: &oj_config::OjConfig,
    dev_mode: &str,
) -> optimize::OptimizedDeps {
    let (include, exclude, entries) = oj_config::optimize_deps_lists(config);
    optimize::OptimizedDeps::prepare(
        root,
        env!("CARGO_PKG_VERSION"),
        optimize::OptimizeInput {
            no_discovery: config.optimize_deps.as_ref().and_then(|o| o.no_discovery),
            include,
            exclude,
            entries,
            dedupe: oj_config::resolve_dedupe(config),
            alias: oj_config::resolve_alias(config, "client"),
            force: oj_config::optimize_deps_force(config),
            bundler_options: oj_config::optimize_deps_bundler_options(config),
            conditions: oj_config::resolve_conditions(config, "client"),
            main_fields: optimize::optimizer_main_fields(config),
            extensions: oj_config::resolve_extensions(config)
                .unwrap_or_else(oj_resolver::default_extensions),
            preserve_symlinks: oj_config::resolve_preserve_symlinks(config),
            mode: dev_mode.to_string(),
            needs_interop: oj_config::optimize_deps_needs_interop(config),
        },
    )
}

/// Plugin server events, the cache writer thread and the file watcher.
fn spawn_state_tasks(
    state: &Arc<ServerState>,
    mut write_rx: tokio::sync::mpsc::Receiver<(String, Arc<CachedModule>)>,
    watch_rx: std::sync::mpsc::Receiver<WatchMsg>,
) {
    if let Some(host) = &state.plugins {
        host.set_ws_sender(state.reload_tx.clone());
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        host.set_server_events_sender(ev_tx);
        let st = Arc::clone(state);
        tokio::spawn(async move {
            while let Some(ev) = ev_rx.recv().await {
                handle_plugin_server_event(&st, &ev).await;
            }
        });
    }
    {
        let state = Arc::clone(state);
        std::thread::spawn(move || {
            while let Some((key, module)) = write_rx.blocking_recv() {
                state.cache.put(&key, &module);
            }
        });
    }
    spawn_watcher(Arc::clone(state), watch_rx);
}

/// `server.warmup`: precompile the client files.
fn spawn_warmup(state: &Arc<ServerState>, config: &oj_config::OjConfig) {
    let (client_files, ssr_files) = oj_config::server_warmup_files(config);
    if client_files.is_empty() && ssr_files.is_empty() {
        return;
    }
    let state = Arc::clone(state);
    tokio::spawn(async move {
        for file in warmup_paths(&state.root, &client_files) {
            let url = url_of(&state.root, &file);
            if let Err(error) = ensure_module(&state, &file, &url).await {
                eprintln!("oj: warmup {url}: {error}");
            }
        }
        // SSR dev compiles are not cached (each /@ssr-module re-transforms); the
        // durable warm-up is spawning the SSR plugin host off the first request.
        if !ssr_files.is_empty() {
            if let Some(host) = ssr_plugin_host(&state).await {
                let _ = host.build_hook_plan().await;
            }
        }
    });
}

fn build_router(
    state: Arc<ServerState>,
    server_cfg: &oj_config::ServerConfig,
    hmr_ws_path: &str,
) -> Router {
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
        // OJ_DEBUG_MEM=1: force a full V8 collection in every live engine so memory
        // probes measure retained heap (issue #202). 404 unless enabled.
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
        app = app.route(hmr_ws_path, get(ws_upgrade));
    }
    // Layer order is reversed at request time (last added runs first). Vite's
    // sequence is cors, host validation, proxy: proxied requests bypass neither gate.
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
    let extra_headers: Vec<(header::HeaderName, header::HeaderValue)> = server_cfg
        .headers
        .as_ref()
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
    app.with_state(state)
}

/// Load the serve config and adopt the vite.config's values.
fn load_dev_config(
    root: &Path,
    config_file: Option<&Path>,
    dev_mode: &str,
) -> anyhow::Result<oj_config::OjConfig> {
    let mut config =
        oj_config::load_with(root, "serve", dev_mode).map_err(|e| anyhow::anyhow!("{e}"))?;
    plugins::adopt_vite_config_values(&mut config, root, config_file, "serve", dev_mode)
        .map_err(|e| anyhow::anyhow!(e))?;
    boot_phase("vite config values adopted");
    // Feed optimizeDeps.include/exclude/needsInterop into partial bundling so
    // the same vite.config field that drives Vite's dep pre-bundle drives oj's.
    let (include, exclude, _entries) = oj_config::optimize_deps_lists(&config);
    pkg_bundle::configure(
        include,
        exclude,
        oj_config::optimize_deps_needs_interop(&config),
    );
    Ok(config)
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

    /// On-disk module cache: experimental, off by default (--enable-cache /
    /// OJ_ENABLE_CACHE=1); --no-cache / OJ_NO_CACHE=1 forces it off.
    fn persistent_cache_enabled(&self) -> bool {
        let env_flag = |k: &str| std::env::var(k).is_ok_and(|v| !v.is_empty() && v != "0");
        let enabled = self.enable_cache || env_flag("OJ_ENABLE_CACHE");
        let forced_off = self.no_cache || env_flag("OJ_NO_CACHE");
        enabled && !forced_off
    }

    fn dev_mode(&self) -> String {
        self.mode
            .clone()
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "development".to_string())
    }

    pub async fn build_app(self) -> anyhow::Result<BuiltApp> {
        let root = self
            .root
            .canonicalize()
            .with_context(|| format!("app root not found: {}", self.root.display()))?;

        // The CLI's --config, resolved against the app root; threaded to every
        // config read instead of parked in a global.
        let config_file: Option<PathBuf> = self.config.as_ref().map(|cfg| {
            if cfg.is_absolute() {
                cfg.clone()
            } else {
                root.join(cfg)
            }
        });

        boot_phase("build_app begin");
        prepare_cache_root(&root);
        let dev_mode = self.dev_mode();
        let config = load_dev_config(&root, config_file.as_deref(), &dev_mode)?;

        let env_dir = config
            .env_dir
            .as_deref()
            .map(|d| root.join(d))
            .unwrap_or_else(|| root.clone());
        let client_defines = ClientDefines {
            env: oj_env::load(&env_dir, &dev_mode),
            prefixes: oj_config::env_prefixes(&config),
            config: &config,
            mode: &dev_mode,
        };
        let mut defines =
            install_client_defines(client_defines.build(&std::collections::BTreeMap::new()));
        // `environments.ssr.define` layers over the shared define for SSR compiles only
        // (Vite's per-environment define): a per-side key must not leak across.
        oj_compiler::set_import_meta_env_ssr(dedup_defines_last_wins(
            oj_config::environment_defines(&config, "ssr"),
        ));

        // BEFORE any engine boots: plugin-hook children register here (own process
        // group each) so restarts and shutdown can kill whole plugin-spawned trees.
        deno_process::oj_hook::set(child_groups::register, child_groups::unregister);
        let server_cfg = config.server.clone().unwrap_or_default();
        let port = self.port.or(server_cfg.port).unwrap_or(5199);
        let strict_port = oj_config::server_strict_port(&config);
        let persistent_cache = self.persistent_cache_enabled();
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

        // TanStack Start owns its module graph and SSR; oj runs the plugin host only
        // for configureServer middleware, in start mode so lifecycle hooks are tolerated.
        let is_start = is_tanstack_start_app(&root);
        let plugin_file = plugin_file(&root, config_file.as_deref());
        let payloads = plugin_host_payloads(
            &root,
            &config,
            &server_cfg,
            &dev_mode,
            port,
            plugin_file.as_ref().map_or(PluginsFormat::Oj, |f| f.format),
            is_start,
        );
        // Optimizer quarantine: a cold deps cache would run Vite's dep optimizer (rolldown,
        // process-scoped native retention) inside oj; pre-seed in a one-shot child so the
        // host finds the caches warm and never builds in-process (see preseed.rs).
        if plugin_file
            .as_ref()
            .is_some_and(|f| f.format == PluginsFormat::Vite)
            && oj_config::ssr_runner_backed(&config)
        {
            preseed::preseed_server_deps(&root, HOST_ENV_MODE).await;
        }
        boot_phase("plugin host spawning");
        let engine_registry = oj_js::EngineRegistry::new();
        let plugin_host = match &plugin_file {
            Some(file) => {
                let keep_for_proxy = server_cfg.proxy.as_ref().is_some_and(|p| !p.is_empty());
                spawn_plugin_host(
                    &root,
                    file,
                    &payloads.client,
                    &engine_registry,
                    keep_for_proxy,
                    is_start,
                )
                .await?
            }
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
            // No middleware port yet: the host's init may outlive the boot deadlines.
            // Activate on its late serve-info push; never degrade silently.
            spawn_late_plugin_serve(Arc::clone(&plugin_serve), Arc::clone(host));
        }
        let mut caps = PluginCaps::default();
        if let Some(host) = &plugin_host {
            if let Some(folded) = fold_plugin_defines(host, &client_defines).await {
                defines = folded;
            }
            caps = PluginCaps::probe(host).await;
        }

        let jsx = jsx_config_of(&config);
        let jsx_overrides = match &plugin_host {
            Some(host) => {
                resolve_jsx_overrides(host, &root, jsx.import_source.as_deref().unwrap_or("react"))
                    .await
            }
            None => std::collections::BTreeMap::new(),
        };

        let hmr_gate = hmr_gate_of(&server_cfg);
        let hmr = hmr_setup(&config, &server_cfg, hmr_gate.is_some());
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
        let (write_tx, write_rx) = tokio::sync::mpsc::channel::<(String, Arc<CachedModule>)>(65536);
        let (watch_tx, watch_rx) = std::sync::mpsc::channel::<WatchMsg>();
        let public_dir = oj_config::public_dir(&config, &root);
        let client_resolver = Arc::new(client_resolver(&root, &config));
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
            config_file: config_file.clone(),
            engine_registry: engine_registry.clone(),
            root: root.clone(),
            public_dir,
            reload_tx: reload_tx.clone(),
            graph: Mutex::new(ModuleGraph::new()),
            require_resolver: Arc::new(client_resolver.require_variant()),
            resolver: client_resolver,
            ssr_resolver: Arc::new(ssr_resolver(&root, &config)),
            cache: PersistentCache::new(oj_cache::cache_root(&root), env!("CARGO_PKG_VERSION"))
                .with_salt_extra(&defines.digest)
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
            fs_allow: Arc::new(Mutex::new(fs_allow_list(&root, &server_cfg))),
            fs_strict,
            fs_deny: compile_fs_deny(&oj_config::server_fs_deny(&config)),
            dir_cache: Arc::new(Mutex::new(DirCache::new())),
            cache_writes: write_tx,
            preload_snapshot: load_graph_snapshot(&root),
            proxy,
            proxy_regex,
            http: reqwest::Client::new(),
            http_insecure: std::sync::OnceLock::new(),
            proxy_tls: [std::sync::OnceLock::new(), std::sync::OnceLock::new()],
            virtual_ids: Arc::new(
                config
                    .virtual_modules
                    .iter()
                    .flat_map(|m| m.keys().cloned())
                    .collect(),
            ),
            canon_memo: Mutex::new(std::collections::HashMap::new()),
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
            hmr_enabled: hmr.enabled,
            plugins: plugin_host.clone(),
            plugin_serve: Arc::clone(&plugin_serve),
            plugins_ssr: tokio::sync::OnceCell::new(),
            ssr_watch: Arc::new(SsrWatchQueue::default()),
            ssr_plugin_config: payloads.ssr,
            plugin_watched: Arc::new(Mutex::new(std::collections::HashSet::new())),
            watch_tx,
            plugins_use_module_parsed: caps.use_module_parsed,
            plugins_have_transform: caps.have_transform,
            plugins_have_load: caps.have_load,
            dep_transform_res: caps.dep_transform_res,
            dep_load_res: caps.dep_load_res,
            resolve_id_res: caps.resolve_id_res,
            plugins_watch_change: caps.watch_change,
            plugins_hot_update: caps.hot_update,
            html_env: defines.html_env,
            parsed_fired: Mutex::new(std::collections::HashSet::new()),
            rt: tokio::runtime::Handle::current(),
            base: config.base.clone().filter(|b| b != "/"),
            buffered_error: Mutex::new(None),
            resolve_failed: Mutex::new(std::collections::HashSet::new()),
            client_js_etag: format!(
                "\"{}\"",
                &blake3::hash(hmr.client_js.as_bytes()).to_hex()[..16]
            ),
            client_js: Bytes::from(hmr.client_js),
            glob_importers: Mutex::new(HashMap::new()),
            app_type,
            watch_ignored,
            ws_token: hmr.ws_token,
            ws_token_check: hmr.ws_token_check,
            optimized: Arc::new(optimized_deps(&root, &config, &dev_mode)),
        });
        pkg_bundle::set_version(state.optimized.version());
        spawn_state_tasks(&state, write_rx, watch_rx);
        spawn_warmup(&state, &config);
        if self.lazy {
            // Lazy mode (Vite's default): compile on demand. Mark the crawl "done" so
            // preload injection and chunk assembly never block on a crawl that will not run.
            let _ = crawl_tx.send(true);
        } else {
            spawn_crawl(Arc::clone(&state), crawl_tx);
        }

        let proxy_prefixes: Vec<String> = state.proxy.iter().map(|(p, _)| p.clone()).collect();
        let hmr_gate = state.hmr_gate.as_ref().map(|_| HmrGateHandle {
            state: Arc::clone(&state),
        });
        let ssr = SsrBridge {
            state: Arc::clone(&state),
        };
        let router = build_router(state, &server_cfg, &hmr.ws_path);

        Ok(BuiltApp {
            router,
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
    // Patterns are root-relative (Vite's warmup semantics). Exclusions match the
    // ROOT-RELATIVE walked path, so './' spellings never split positive vs negative.
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

/// Vite parity: close runs `buildEnd` then `closeBundle`. oj has no graceful drain, so
/// the hooks run on the signal (bounded) and the process exits with the shell's code.
async fn close_plugins_on_shutdown(host: Option<Arc<PluginHost>>) {
    #[cfg(unix)]
    let code = {
        use tokio::signal::unix::{signal, SignalKind};
        // SIGHUP too: own-group children no longer share the terminal's session, so a
        // closed terminal/SSH drop must be forwarded like ^C or plugin runtimes orphan.
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
    // the signal that ends oj never reaches them; forward it.
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
mod adapter_tests;
#[cfg(test)]
mod tests;
