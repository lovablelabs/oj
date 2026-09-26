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
pub mod sidecar;
pub mod svgr;
use css_engine::CssEngine;
use oj_graph::{HmrDecision, ModuleGraph};
use oj_resolver::OjResolver;
use plugins::PluginHost;
use sidecar::is_tailwind_css;
use tokio::sync::broadcast;

#[inline]
pub fn cobalt(s: &str) -> String {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal() {
        format!("\x1b[1;38;2;42;51;212m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

#[inline]
pub fn cell(s: &str) -> String {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal() {
        format!("\x1b[48;2;255;255;255m\x1b[1;38;2;42;51;212m {s} \x1b[0m")
    } else {
        s.to_string()
    }
}

#[inline]
pub fn oj_brand() -> String {
    cell("oj")
}

pub fn link(url: &str, text: &str) -> String {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal() {
        format!("\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\")
    } else {
        text.to_string()
    }
}

fn oj_tag() -> String {
    use std::io::IsTerminal;
    if std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal() {
        format!("{} ", oj_brand())
    } else {
        "oj:".to_string()
    }
}

fn bytes_to_string(bytes: Vec<u8>) -> std::io::Result<String> {
    match simdutf8::basic::from_utf8(&bytes) {
        Ok(_) => Ok(unsafe { String::from_utf8_unchecked(bytes) }),
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "stream did not contain valid UTF-8",
        )),
    }
}

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

pub fn boot_phase(label: &str) {
    if std::env::var_os("OJ_BOOT_PHASES").is_none() {
        return;
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    eprintln!("[oj-phase] {ms} {label}");
}

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
                "server": { "port": port, "host": server_cfg.host, "proxy": server_cfg.proxy },
                // `{}` rather than null when the config has none: the host deep-merges
                // this over the user's Vite-resolved config, and a null would erase
                // its environments (and their per-environment `define`).
                "environments": config.environments.clone().unwrap_or_default(),
            },
            "env": { "command": "serve", "mode": dev_mode },
            "environment": { "name": "client", "mode": host_env_mode },
            "pluginsFormat": plugins_format,
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

fn warmup_paths(root: &Path, patterns: &[String]) -> Vec<PathBuf> {
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
        match signal(SignalKind::terminate()) {
            Ok(mut term) => tokio::select! {
                _ = tokio::signal::ctrl_c() => 130,
                _ = term.recv() => 143,
            },
            Err(_) => {
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
    std::process::exit(code);
}

/// Vite's `server.open`: launch the system browser at the served url once the
/// listener is bound. `BROWSER=none` disables it and any other `BROWSER` value
/// names the command to run (the `open` package's convention Vite follows).
pub fn open_browser(url: &str) {
    let browser = std::env::var("BROWSER")
        .ok()
        .filter(|b| !b.trim().is_empty());
    if browser.as_deref() == Some("none") {
        return;
    }
    let mut cmd = match browser {
        Some(b) => std::process::Command::new(b),
        None if cfg!(target_os = "macos") => std::process::Command::new("open"),
        None if cfg!(target_os = "windows") => {
            let mut c = std::process::Command::new("cmd");
            c.args(["/C", "start", ""]);
            c
        }
        None => std::process::Command::new("xdg-open"),
    };
    cmd.arg(url)
        // The launcher takes a URL, not a path, and the process cwd may be a
        // since-deleted app root (host boot chdir), which fails the spawn.
        .current_dir(std::env::temp_dir())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if let Err(err) = cmd.spawn() {
        eprintln!("oj: could not open the browser: {err}");
    }
}

/// `server.watch.ignored` entries as globs. Vite hands them to chokidar, which
/// matches absolute paths; a relative pattern is kept as written (for
/// root-relative matching) and rooted at the project (for absolute paths).
fn watch_ignored_patterns(root: &Path, ignored: &[String]) -> Vec<glob::Pattern> {
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

fn is_watch_ignored(patterns: &[glob::Pattern], root: &Path, path: &Path) -> bool {
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
fn is_config_dependency(path: &Path) -> bool {
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

fn js(body: impl IntoResponse) -> Response {
    ([(header::CONTENT_TYPE, "text/javascript")], body).into_response()
}

async fn serve_client_js(State(state): State<Arc<ServerState>>, headers: HeaderMap) -> Response {
    // Browsers refetch /@oj/client.js on every reload; the body is fixed for
    // the server's lifetime, so a matching validator saves the transfer.
    cached_js_response(
        &headers,
        state.client_js_etag.clone(),
        state.client_js.clone(),
    )
}

/// A cached, immutable-for-this-process JS body: 304 on a matching validator,
/// otherwise the bytes verbatim (the header tuple overwrites the
/// `application/octet-stream` that `Bytes` would set).
fn cached_js_response(headers: &HeaderMap, etag: String, body: Bytes) -> Response {
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        == Some(etag.as_str())
    {
        return (
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "text/javascript".to_string()),
            (header::CACHE_CONTROL, "no-cache".to_string()),
            (header::ETAG, etag),
        ],
        body,
    )
        .into_response()
}

/// The path the HMR socket is served at: `server.hmr.path` (made absolute) or
/// oj's `/__ws`.
fn hmr_socket_path(hmr: Option<&oj_config::HmrOptions>) -> String {
    match hmr
        .and_then(|h| h.path.as_deref())
        .map(str::trim)
        .filter(|p| !p.is_empty())
    {
        Some(p) if p.starts_with('/') => p.to_string(),
        Some(p) => format!("/{p}"),
        None => "/__ws".to_string(),
    }
}

/// Fill the client's `__HMR_*__` / `__WS_TOKEN__` placeholders the way Vite's
/// clientInjections does: JSON literals, `null` where the config is silent so
/// the client falls back to the page's own location.
fn render_client_js(
    template: &str,
    hmr: Option<&oj_config::HmrOptions>,
    ws_path: &str,
    token: &str,
) -> String {
    let lit = |v: serde_json::Value| v.to_string();
    let protocol = hmr.and_then(|h| h.protocol.clone());
    let hostname = hmr.and_then(|h| h.host.clone());
    // Vite: `ws.clientPort -> ws.port -> the page's port`; oj's socket shares
    // the dev server port, so only clientPort (the browser-facing port behind a
    // proxy) moves the dial.
    let port = hmr.and_then(|h| h.client_port);
    let overlay = hmr.and_then(|h| h.overlay).unwrap_or(true);
    template
        .replace("__HMR_PROTOCOL__", &lit(protocol.into()))
        .replace("__HMR_HOSTNAME__", &lit(hostname.into()))
        .replace("__HMR_PORT__", &lit(port.into()))
        .replace("__HMR_PATH__", &lit(ws_path.into()))
        .replace("__HMR_ENABLE_OVERLAY__", &lit(overlay.into()))
        .replace("__WS_TOKEN__", &lit(token.into()))
}

/// A fresh random token for this process (Vite: `crypto.randomBytes(9)`, as
/// base64url). rustls' provider RNG is already linked; a hash of process-unique
/// state is the fallback if it ever fails.
pub(crate) fn new_ws_token() -> String {
    let mut bytes = [0u8; 16];
    let filled = rustls::crypto::aws_lc_rs::default_provider()
        .secure_random
        .fill(&mut bytes)
        .is_ok();
    if !filled {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let seed = format!("{}:{nanos}:{:p}", std::process::id(), &bytes);
        bytes.copy_from_slice(&blake3::hash(seed.as_bytes()).as_bytes()[..16]);
    }
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Vite's ws `shouldHandle`: a request carrying `Origin` comes from a browser
/// and must present the token (`hasValidToken`); requests without one are
/// allowed, since a client that can send them can already make plain HTTP
/// requests to the server. `vite-ping` never carries data and is exempt.
fn ws_token_rejected(check: bool, token: &str, headers: &HeaderMap, query: Option<&str>) -> bool {
    if !check || !headers.contains_key(header::ORIGIN) {
        return false;
    }
    !query.is_some_and(|q| {
        q.split('&')
            .any(|kv| kv.strip_prefix("token=") == Some(token))
    })
}

// An SSR module carries its source map inline: the runner maps stack frames
// through it back to the original file (Vite's ssrFixStacktrace).
fn with_inline_map(code: String, map_data_url: Option<String>) -> String {
    match map_data_url {
        Some(map) => format!("{code}\n//# sourceMappingURL={map}\n"),
        None => code,
    }
}

/// How the SSR pipeline resolved an import: a module it serves (through
/// `/@ssr-module` or [`SsrBridge::load_module`]), or an external the runner
/// imports from node_modules with its own Node resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SsrResolution {
    Module(String),
    External(String),
}

/// Why an SSR module could not be served.
#[derive(Debug)]
pub enum SsrModuleError {
    Forbidden(String),
    NotFound(String),
    Failed(String),
}

impl std::fmt::Display for SsrModuleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SsrModuleError::Forbidden(m)
            | SsrModuleError::NotFound(m)
            | SsrModuleError::Failed(m) => f.write_str(m),
        }
    }
}

/// In-process access to the SSR resolve/load pipeline behind `/@ssr-resolve`
/// and `/@ssr-module`, for the embedded SSR module runner. Both entry points
/// share one implementation, so the HTTP endpoints and the runner can never
/// drift apart.
#[derive(Clone)]
pub struct SsrBridge {
    state: Arc<ServerState>,
}

impl SsrBridge {
    pub async fn resolve(&self, importer: &str, spec: &str) -> Result<SsrResolution, String> {
        ssr_resolve_inner(&self.state, importer, spec).await
    }

    /// The module's transformed code (plugins + dev-ssr compile), with its
    /// source map inline.
    pub async fn load_module(&self, id: &str) -> Result<String, SsrModuleError> {
        ssr_module_inner(&self.state, id, false).await
    }

    /// Resolution for the in-process Start module host: unlike [`resolve`],
    /// a hit inside node_modules keeps its RESOLVED path, so the host can
    /// hand the engine the exact file the Vite-style resolver picked
    /// (mainFields, extension probing, exports conditions) instead of
    /// re-resolving the bare specifier with plain Node semantics.
    pub async fn resolve_start(
        &self,
        importer: &str,
        spec: &str,
    ) -> Result<StartResolution, String> {
        let state = &self.state;
        let importer_dir = Path::new(importer).parent().unwrap_or(&state.root);
        match state.ssr_resolver.resolve(importer_dir, spec) {
            Ok(p) => {
                if p.to_string_lossy().contains("/node_modules/") {
                    Ok(StartResolution::Dependency(p))
                } else {
                    Ok(StartResolution::Module(p.to_string_lossy().into_owned()))
                }
            }
            Err(e) => {
                if let Some(host) = ssr_plugin_host(state).await {
                    if let Ok(Some(id)) = host.resolve_id(spec, importer).await {
                        return Ok(StartResolution::Module(id));
                    }
                }
                if !spec.starts_with('.') && !spec.starts_with('/') {
                    return Ok(StartResolution::Bare(spec.to_string()));
                }
                Err(format!("cannot resolve {spec}: {}", e.reason))
            }
        }
    }

    /// The plugin-transform + compile tail on a source the caller pre-read
    /// (or pre-rewrote); `from_plugin` marks virtual content. `run_plugins:
    /// false` skips the plugin transform chain (the caller already ran it,
    /// e.g. an mdx compile) and only applies the dev-ssr compile.
    pub async fn transform_module(
        &self,
        id: &str,
        source: String,
        from_plugin: bool,
        run_plugins: bool,
    ) -> Result<String, SsrModuleError> {
        if run_plugins {
            ssr_transform_source(&self.state, id, source, from_plugin, false).await
        } else {
            ssr_compile_source(&self.state, id, source, from_plugin, false)
        }
    }

    /// The lazily spawned ssr-environment plugin host, when the app has one.
    pub async fn plugin_host(&self) -> Option<std::sync::Arc<PluginHost>> {
        ssr_plugin_host(&self.state).await
    }

    /// Whether the SSR pipeline may serve this module path (root, allow-list,
    /// node_modules), the same rule `load_module` enforces.
    pub fn module_allowed(&self, path: &Path) -> bool {
        ssr_module_allowed(&self.state, path)
    }
}

/// How the Start module host's resolution landed: a module the pipeline serves
/// (an app fs path or a plugin virtual id), a resolved dependency file inside
/// node_modules, or a bare specifier left to Node semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartResolution {
    Module(String),
    Dependency(PathBuf),
    Bare(String),
}

async fn ssr_resolve_inner(
    state: &Arc<ServerState>,
    importer: &str,
    spec: &str,
) -> Result<SsrResolution, String> {
    let importer_dir = Path::new(importer).parent().unwrap_or(&state.root);
    match state.ssr_resolver.resolve(importer_dir, spec) {
        Ok(p) => {
            let s = p.to_string_lossy();
            if s.contains("/node_modules/") {
                Ok(SsrResolution::External(spec.to_string()))
            } else {
                Ok(SsrResolution::Module(s.into_owned()))
            }
        }
        Err(e) => {
            if let Some(host) = ssr_plugin_host(state).await {
                if let Ok(Some(id)) = host.resolve_id(spec, importer).await {
                    return Ok(SsrResolution::Module(id));
                }
            }
            if !spec.starts_with('.') && !spec.starts_with('/') {
                return Ok(SsrResolution::External(spec.to_string()));
            }
            Err(format!("cannot resolve {spec}: {}", e.reason))
        }
    }
}

async fn ssr_resolve(
    State(state): State<Arc<ServerState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let (Some(importer), Some(spec)) = (q.get("importer"), q.get("spec")) else {
        return (StatusCode::BAD_REQUEST, "importer and spec required").into_response();
    };
    match ssr_resolve_inner(&state, importer, spec).await {
        Ok(SsrResolution::Module(id)) => js_response_json(serde_json::json!({ "id": id })),
        Ok(SsrResolution::External(spec)) => {
            js_response_json(serde_json::json!({ "external": true, "spec": spec }))
        }
        Err(e) => (StatusCode::NOT_FOUND, e).into_response(),
    }
}

fn js_response_json(v: serde_json::Value) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

fn module_read_allowed(
    root: &Path,
    allow: &std::collections::HashSet<PathBuf>,
    path: &Path,
) -> bool {
    let Ok(candidate) = std::fs::canonicalize(path) else {
        return true;
    };
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if candidate.starts_with(real(root)) {
        return true;
    }
    if candidate
        .components()
        .any(|c| c.as_os_str() == "node_modules")
    {
        return true;
    }
    allow
        .iter()
        .any(|allowed| candidate.starts_with(real(allowed)))
}

fn ssr_module_allowed(state: &ServerState, path: &Path) -> bool {
    let allow = state.fs_allow.lock().unwrap().clone();
    module_read_allowed(&state.root, &allow, path)
}

async fn ssr_module_inner(
    state: &Arc<ServerState>,
    id: &str,
    runner: bool,
) -> Result<String, SsrModuleError> {
    let path = PathBuf::from(id);
    if !ssr_module_allowed(state, &path) {
        return Err(SsrModuleError::Forbidden(
            "oj: module not allow-listed".into(),
        ));
    }
    let (source, from_plugin) = match std::fs::read(&path).and_then(bytes_to_string) {
        Ok(s) => (s, false),
        Err(read_err) => match ssr_plugin_host(state).await {
            Some(host) if host.hook_wants_load(id) => match host.load(id).await {
                Ok(Some(code)) => (code, true),
                _ => return Err(SsrModuleError::NotFound(format!("{id}: {read_err}"))),
            },
            Some(_) | None => return Err(SsrModuleError::NotFound(format!("{id}: {read_err}"))),
        },
    };
    let ext = path.extension().and_then(|e| e.to_str());
    if !from_plugin && ext.is_some_and(is_style_ext) {
        let source = if is_preprocessor(id) {
            run_preprocess_engine(state, id, &source, serde_json::Value::Null)
                .await
                .map_err(SsrModuleError::Failed)?
        } else {
            source
        };
        return ssr_css_module(&state.root, &path, &source).map_err(SsrModuleError::Failed);
    }
    if !from_plugin && ext == Some("json") {
        return oj_compiler::json::to_esm(&source, id)
            .map_err(|e| SsrModuleError::Failed(format!("{e}")));
    }
    ssr_transform_source(state, id, source, from_plugin, runner).await
}

/// The plugin-transform + dev-ssr-compile tail of the SSR module pipeline, on
/// a source the caller already has (the fs read, a plugin `load()` override,
/// or a pre-rewritten Start module).
async fn ssr_transform_source(
    state: &Arc<ServerState>,
    id: &str,
    source: String,
    from_plugin: bool,
    runner: bool,
) -> Result<String, SsrModuleError> {
    let source = match ssr_plugin_host(state).await {
        // The same per-plugin filter gate the client path runs: an SSR module
        // no plugin's filter can claim skips the isolate RPC (and the import
        // pre-resolution that only exists to feed it).
        Some(host) if host.hook_wants_transform(id, &source) => {
            let resolved =
                resolved_imports_json(&state.resolver, &state.fs_allow, &source, Path::new(id));
            match host.transform(&source, id, &resolved).await {
                Ok((code, _, _, _)) => code,
                Err(e) => {
                    return Err(SsrModuleError::Failed(format!(
                        "oj: plugin transform error for {id}:\n{e}"
                    )));
                }
            }
        }
        Some(_) => {
            if plugins::hook_gate_debug() {
                eprintln!("oj: hook gate skipped ssr transform for {id}");
            }
            source
        }
        None => source,
    };
    ssr_compile_source(state, id, source, from_plugin, runner)
}

/// The dev-ssr compile alone (no plugin transforms): TS/JSX strip, define,
/// import.meta.env/glob, refresh off, SSR true.
fn ssr_compile_source(
    state: &Arc<ServerState>,
    id: &str,
    source: String,
    from_plugin: bool,
    runner: bool,
) -> Result<String, SsrModuleError> {
    let compile_path: PathBuf = if from_plugin {
        PathBuf::from("virtual.tsx")
    } else {
        PathBuf::from(id)
    };
    // Dev SSR modules compile as dev + ssr (Vite's importAnalysis injects
    // `SSR: true` and the dev env), so `import.meta.env.SSR` is true and
    // `DEV`/`MODE` match the client; Fast Refresh stays off on the server.
    let mut opts = dev_compile_opts(state);
    opts.refresh = false;
    opts.ssr = true;
    if runner {
        return match oj_compiler::ssr::ssr_transform_module_with_map(&compile_path, &source, &opts)
        {
            Ok((code, map)) => Ok(with_inline_map(code, map)),
            Err(e) => Err(SsrModuleError::Failed(format!("{e}"))),
        };
    }
    match oj_compiler::compile(&compile_path, &source, &opts) {
        Ok(out) => Ok(with_inline_map(out.code, out.map_data_url)),
        Err(e) => Err(SsrModuleError::Failed(format!("{e}"))),
    }
}

async fn ssr_module(
    State(state): State<Arc<ServerState>>,
    Query(q): Query<HashMap<String, String>>,
) -> Response {
    let Some(id) = q.get("id") else {
        return (StatusCode::BAD_REQUEST, "id required").into_response();
    };
    let runner = q.get("runner").map(|v| v == "1").unwrap_or(false);
    match ssr_module_inner(&state, id, runner).await {
        Ok(code) => js(code),
        Err(SsrModuleError::Forbidden(m)) => (StatusCode::FORBIDDEN, m).into_response(),
        Err(SsrModuleError::NotFound(m)) => (StatusCode::NOT_FOUND, m).into_response(),
        Err(SsrModuleError::Failed(m)) => (StatusCode::INTERNAL_SERVER_ERROR, m).into_response(),
    }
}

async fn ssr_plugin_host(state: &Arc<ServerState>) -> Option<std::sync::Arc<PluginHost>> {
    let host = state
        .plugins_ssr
        .get_or_init(|| async {
            let file = match plugins::plugin_source(&state.root)? {
                plugins::PluginSource::OjPlugins(p) | plugins::PluginSource::ViteConfig(p) => p,
            };
            // Lazy spawn (first SSR request): the short init-wait policy, so a
            // wedged init cannot block the watcher thread's watchChange /
            // hotUpdate dispatch or SSR transforms for the long init deadline.
            match PluginHost::spawn_lazy(&state.root, &file, &state.ssr_plugin_config).await {
                Ok(host) => {
                    eprintln!("oj ssr: plugins (ssr environment) from {}", file.display());
                    // The catch-up half of the watcher's pre-init fast-skip:
                    // events skipped while this host initializes replay at
                    // its init (see SsrWatchQueue).
                    spawn_ssr_watch_catch_up(
                        std::sync::Arc::clone(&host),
                        Arc::clone(&state.ssr_watch),
                    );
                    // The ssr environment's per-environment define
                    // (`environments.ssr.define` from the resolved config,
                    // plus config()-hook deltas) layers over the shared
                    // define for SSR compiles.
                    let ssr_defines = host.config_defines().await;
                    if !ssr_defines.is_empty() {
                        oj_compiler::merge_import_meta_env_ssr(ssr_defines);
                    }
                    Some(host)
                }
                Err(e) => {
                    eprintln!("oj ssr: plugin host failed to start: {e}");
                    None
                }
            }
        })
        .await
        .clone();
    // Every consumer acquires the host here (the SSR gates and the Start
    // bridge alike), so this is the one seam where the plan can be primed
    // before any gated dispatch; a no-op once the plan is live.
    if let Some(h) = &host {
        h.prime_hook_plan().await;
    }
    host
}

/// Watcher events (file, change type) the lazily spawned SSR host could not
/// take yet: the watcher fast-skips a pre-init host — dispatching would
/// serially burn a full per-call init window per hook per save on a wedged
/// init — and queues the event here for a watchChange catch-up replay at the
/// host's init (see `spawn_ssr_watch_catch_up`). The backlog dedups by file
/// (the latest change type wins), so it is bounded by the files edited during
/// the window. `order` serializes EVERY watchChange dispatch toward that host
/// — the catch-up replay and the watcher's live post-init dispatch alike, the
/// live path draining the backlog first — so a stale queued event can never
/// land after a newer live event for the same file.
#[derive(Default)]
struct SsrWatchQueue {
    backlog: Mutex<Vec<(String, String)>>,
    order: tokio::sync::Mutex<()>,
    /// One line for the first skip, not one per save.
    logged: std::sync::atomic::AtomicBool,
}

/// Records a watcher event the pre-init lazy SSR host cannot take yet (the
/// watcher's fast-skip; see [`SsrWatchQueue`]): deduped by file, the latest
/// change type winning — a delete after an update is what the replay must
/// report.
fn note_ssr_watch_skip(queue: &SsrWatchQueue, file: &str, change_type: &str) {
    {
        let mut b = queue.backlog.lock().unwrap();
        if let Some(entry) = b.iter_mut().find(|(f, _)| f == file) {
            entry.1 = change_type.to_string();
        } else {
            b.push((file.to_string(), change_type.to_string()));
        }
    }
    if !queue.logged.swap(true, std::sync::atomic::Ordering::SeqCst) {
        println!(
            "oj: ssr plugin host still initializing; queuing file changes for a catch-up replay at its init"
        );
    }
}

/// Replays the skipped events as `watchChange` toward the (now initialized)
/// SSR host — the invalidation notice its plugins missed while pre-init; the
/// late hotUpdate half is deliberately not replayed (its result steers no
/// client update on the ssr environment, and the client updates already
/// happened through oj's own pipeline). The whole replay holds the queue's
/// order lock: a live dispatcher flushing the backlog before its own newer
/// event blocks here until an in-flight replay has fully drained, so queued
/// (older) events always reach the host before live (newer) ones. Loops: a
/// skip racing the drain lands in a later batch instead of being lost.
async fn replay_ssr_watch_backlog(host: &PluginHost, queue: &SsrWatchQueue) {
    let _order = queue.order.lock().await;
    loop {
        let batch: Vec<(String, String)> = {
            let mut b = queue.backlog.lock().unwrap();
            b.drain(..).collect()
        };
        if batch.is_empty() {
            return;
        }
        println!(
            "oj: ssr plugin host caught up: replaying {} file change(s) missed during its init",
            batch.len()
        );
        for (file, ev) in batch {
            if let Err(e) = host.watch_change(&file, &ev).await {
                eprintln!("oj: watchChange (ssr catch-up) failed for {file}: {e}");
            }
        }
    }
}

/// Waits for the lazy SSR host's init and replays the watcher backlog then; a
/// host that dies pre-init keeps the wait alive while it may still be revived
/// (a revive resets `initialized` and the fresh generation's init replays the
/// backlog), and only a permanent death — no respawns left — releases it.
fn spawn_ssr_watch_catch_up(host: std::sync::Arc<PluginHost>, queue: Arc<SsrWatchQueue>) {
    tokio::spawn(async move {
        let mut init = host.initialized_updates();
        let mut gone = host.host_gone_updates();
        loop {
            if *init.borrow_and_update() {
                break;
            }
            if *gone.borrow_and_update() && !host.can_revive() {
                return;
            }
            tokio::select! {
                changed = init.changed() => { if changed.is_err() { return; } }
                changed = gone.changed() => { if changed.is_err() { return; } }
                // Permanent death makes no watch change of its own (the final
                // failed revive is silent): re-check slowly.
                _ = tokio::time::sleep(std::time::Duration::from_secs(60)) => {}
            }
        }
        replay_ssr_watch_backlog(&host, &queue).await;
    });
}

fn sass_additional_data_for(state: &ServerState, url: &str) -> Option<String> {
    if !oj_css::is_sass(url) {
        return None;
    }
    let indented = url.split('?').next().unwrap_or(url).ends_with(".sass");
    if indented {
        state.sass_additional_data.clone()
    } else {
        state.scss_additional_data.clone()
    }
}

/// `css.preprocessorOptions.<scss|sass>.loadPaths` / `includePaths`, resolved
/// against the app root.
fn sass_load_paths_for(state: &ServerState, url: &str) -> Vec<PathBuf> {
    if !oj_css::is_sass(url) {
        return Vec::new();
    }
    let Some(css) = &state.css_config else {
        return Vec::new();
    };
    let cfg = oj_config::OjConfig {
        css: Some(css.clone()),
        ..Default::default()
    };
    let lang = if url.split('?').next().unwrap_or(url).ends_with(".sass") {
        "sass"
    } else {
        "scss"
    };
    oj_config::css_load_paths(&cfg, lang)
        .into_iter()
        .map(|p| state.root.join(p))
        .collect()
}

fn ssr_css_module(root: &Path, path: &Path, source: &str) -> Result<String, String> {
    let css_src = if oj_css::is_sass(&path.to_string_lossy()) {
        oj_css::compile_sass(source, path.parent())?
    } else {
        source.to_string()
    };
    let css_id = match path.strip_prefix(root) {
        Ok(rel) => format!("/{}", rel.display()),
        Err(_) => path.to_string_lossy().to_string(),
    };
    let output = oj_css::compile_css(&css_id, &css_src, true)?;
    Ok(match output.exports {
        Some(exports) => oj_css::css_modules_esm(&exports),
        None => "export default {};".to_string(),
    })
}

/// Vite's `css.modules` options as the CSS compiler applies them (dev and build).
pub fn css_modules_options(config: &oj_config::OjConfig) -> oj_css::CssModulesOptions {
    let m = oj_config::css_modules(config);
    oj_css::CssModulesOptions {
        locals_convention: m.locals_convention,
        generate_scoped_name: m.generate_scoped_name,
        global_scope: m.global_scope,
        global_module_paths: m.global_module_paths,
    }
}

pub fn resolve_host(host: Option<&str>) -> std::net::IpAddr {
    match host {
        Some("true") | Some("0.0.0.0") | Some("::") | Some("[::]") => [0, 0, 0, 0].into(),
        Some("localhost") | None => [127, 0, 0, 1].into(),
        Some(h) => h.parse().unwrap_or([127, 0, 0, 1].into()),
    }
}

/// What `oj preview` serves with: Vite's resolved preview options (each one
/// `preview.x ?? server.x`, the port aside) plus the build's base and assetsDir.
#[derive(Debug, Default, Clone)]
pub struct PreviewOptions {
    pub dir: PathBuf,
    pub port: u16,
    pub base: String,
    pub headers: Vec<(String, String)>,
    pub host: Option<String>,
    pub strict_port: bool,
    /// `preview.open`: `Some(path)` opens `url + path` once listening.
    pub open: Option<String>,
    pub cors: Option<oj_config::CorsConfig>,
    pub allowed_hosts: Option<oj_config::AllowedHosts>,
    /// `appType: "spa"` falls back to index.html for unknown paths; `mpa`/`custom` 404.
    pub spa_fallback: bool,
    /// `build.assetsDir`: hashed files under it are immutable.
    pub assets_dir: String,
}

/// The static preview server's per-request state.
struct PreviewState {
    dir: PathBuf,
    base: String,
    headers: Vec<(header::HeaderName, header::HeaderValue)>,
    spa_fallback: bool,
    assets_prefix: String,
}

async fn preview_host_check(
    State(policy): State<Arc<HostPolicy>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(raw) = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
    {
        let host = HostPolicy::host_header_name(raw);
        if !policy.hostname_allowed(host) {
            return (StatusCode::FORBIDDEN, HostPolicy::reject_message(host)).into_response();
        }
    }
    next.run(req).await
}

pub async fn preview(opts: PreviewOptions) -> anyhow::Result<()> {
    let dir = opts.dir.canonicalize().with_context(|| {
        format!(
            "build dir not found: {} (run `oj build` first)",
            opts.dir.display()
        )
    })?;
    let headers: Vec<(header::HeaderName, header::HeaderValue)> = opts
        .headers
        .iter()
        .filter_map(|(k, v)| Some((k.parse().ok()?, v.parse().ok()?)))
        .collect();
    let assets_dir = opts.assets_dir.trim_matches('/');
    let state = Arc::new(PreviewState {
        dir: dir.clone(),
        base: opts.base.clone(),
        headers,
        spa_fallback: opts.spa_fallback,
        assets_prefix: if assets_dir.is_empty() {
            "assets/".to_string()
        } else {
            format!("{assets_dir}/")
        },
    });
    let mut app = Router::new().fallback(get(preview_serve)).with_state(state);
    // Vite's preview stack: cors (unless `false`), then the Host check against
    // DNS rebinding (unless `allowedHosts: true`), then static files.
    if let Some(cors) = CorsPolicy::from_config(opts.cors.as_ref()) {
        app = app.layer(axum::middleware::from_fn_with_state(
            Arc::new(cors),
            cors_middleware,
        ));
    }
    let host_policy = HostPolicy::from_config(
        &oj_config::ServerConfig {
            allowed_hosts: opts.allowed_hosts.clone(),
            host: opts.host.clone(),
            ..Default::default()
        },
        None,
    );
    if !host_policy.allow_all {
        app = app.layer(axum::middleware::from_fn_with_state(
            Arc::new(host_policy),
            preview_host_check,
        ));
    }
    let (listener, port) = bind_dev_listener(
        resolve_host(opts.host.as_deref()),
        opts.port,
        opts.strict_port,
    )
    .await?;
    println!("  {} preview", oj_brand());
    println!("  serving: {}", dir.display());
    let url = format!("http://localhost:{port}{}", opts.base);
    println!("  {}", link(&url, &cell(&url)));
    if let Some(path) = &opts.open {
        let target = if path.starts_with("http://") || path.starts_with("https://") {
            path.clone()
        } else {
            format!(
                "{}{}",
                url.trim_end_matches('/'),
                if path.starts_with('/') {
                    path.clone()
                } else {
                    format!("/{path}")
                }
            )
        };
        open_browser(&target);
    }
    axum::serve(listener, app).await?;
    Ok(())
}

fn preview_rel(path: &str, base: &str) -> Option<String> {
    let trimmed = path
        .strip_prefix(base.trim_end_matches('/'))
        .unwrap_or(path);
    let rel = urldecode(trimmed.trim_start_matches('/'));
    if rel.split('/').any(|seg| seg == "..") {
        return None;
    }
    Some(if rel.is_empty() {
        "index.html".to_string()
    } else {
        rel
    })
}

/// Vite's html fallback for an extensionless preview request: a page's own
/// `index.html` (`/nested/` and `/nested`), then `<path>.html`, then the SPA
/// root `index.html`. Multi-page builds put pages in subdirectories, so serving
/// the root page for `/nested/` would load the wrong page (and, with a relative
/// base, break its `./assets/` URLs).
fn preview_html_fallback(dir: &Path, rel: &str, spa: bool) -> Option<PathBuf> {
    let rel = rel.trim_end_matches('/');
    if !rel.is_empty() {
        let dir_index = dir.join(rel).join("index.html");
        if dir_index.is_file() {
            return Some(dir_index);
        }
        let sibling = dir.join(format!("{rel}.html"));
        if sibling.is_file() {
            return Some(sibling);
        }
    }
    (spa || rel.is_empty() || rel == "index.html").then(|| dir.join("index.html"))
}

async fn preview_serve(State(state): State<Arc<PreviewState>>, uri: Uri) -> Response {
    let PreviewState {
        dir,
        base,
        headers: extra_headers,
        spa_fallback,
        assets_prefix,
    } = &*state;
    let Some(rel) = preview_rel(uri.path(), base) else {
        return (StatusCode::FORBIDDEN, "oj: path traversal denied").into_response();
    };
    let file = dir.join(&rel);
    let ext = Path::new(&rel)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");

    let (target, ctype) = if file.is_file() {
        (file, content_type(ext))
    } else if ext.is_empty() {
        // Vite's htmlFallbackMiddleware: `/x` -> `/x.html` or `/x/index.html`;
        // only `appType: "spa"` then falls back to the root index.html.
        match preview_html_fallback(dir, &rel, *spa_fallback) {
            Some(target) => (target, "text/html; charset=utf-8"),
            None => {
                return (StatusCode::NOT_FOUND, format!("oj: not found: {rel}")).into_response()
            }
        }
    } else {
        return (StatusCode::NOT_FOUND, format!("oj: not found: {rel}")).into_response();
    };

    // Build assets carry a content hash in their name, so they can be cached
    // forever; HTML is unhashed and must revalidate.
    let cache_control = if rel.starts_with(assets_prefix.as_str()) {
        "public, max-age=31536000, immutable"
    } else if ctype.starts_with("text/html") {
        "no-cache"
    } else {
        ""
    };

    match tokio::fs::read(&target).await {
        Ok(bytes) => {
            let mut resp = ([(header::CONTENT_TYPE, ctype)], bytes).into_response();
            let h = resp.headers_mut();
            if !cache_control.is_empty() {
                h.insert(
                    header::CACHE_CONTROL,
                    header::HeaderValue::from_static(cache_control),
                );
            }
            for (name, value) in extra_headers {
                h.insert(name.clone(), value.clone());
            }
            resp
        }
        Err(_) => (StatusCode::NOT_FOUND, "oj: not found").into_response(),
    }
}

/// A `lovable:boot-progress` custom HMR frame (editor boot narration). Shape
/// mirrors web/shared/lib/preview/bootProgress.ts; ssrModules + clientModules
/// are required non-negative ints or the editor drops the frame.
pub fn boot_progress_frame(
    ssr_modules: usize,
    client_modules: usize,
    client_idle_ms: Option<u64>,
) -> String {
    serde_json::json!({
        "type": "custom",
        "event": "lovable:boot-progress",
        "data": {
            "ssrModules": ssr_modules,
            "clientModules": client_modules,
            "ssrIdleMs": serde_json::Value::Null,
            "clientIdleMs": client_idle_ms,
            "buildError": serde_json::Value::Null,
        },
    })
    .to_string()
}

/// A `lovable:update-progress` custom HMR frame (editor prompt / steady-state
/// narration). Shape mirrors web/shared/lib/preview/updateProgress.ts; batch is
/// monotonic and trigger is one of "flush" | "watch" | "restart".
pub fn update_progress_frame(
    batch: u64,
    trigger: &str,
    ssr_modules: usize,
    client_modules: usize,
    idle_ms: Option<u64>,
    done: bool,
) -> String {
    serde_json::json!({
        "type": "custom",
        "event": "lovable:update-progress",
        "data": {
            "batch": batch,
            "trigger": trigger,
            "ssrModules": ssr_modules,
            "clientModules": client_modules,
            "idleMs": idle_ms,
            "done": done,
            "buildError": serde_json::Value::Null,
        },
    })
    .to_string()
}

async fn ws_upgrade(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    uri: Uri,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Some(resp) = state.host_policy.reject_ws_origin(&headers) {
        return resp;
    }
    if ws_token_rejected(state.ws_token_check, &state.ws_token, &headers, uri.query()) {
        return (
            StatusCode::UNAUTHORIZED,
            "oj: websocket token missing or invalid",
        )
            .into_response();
    }
    hmr_socket(upgrade, state, false)
}

fn vite_ws_subprotocol(h: &HeaderMap) -> Option<&'static str> {
    let is_ws = h
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("websocket"))
        .unwrap_or(false);
    if !is_ws {
        return None;
    }
    let raw = h.get(header::SEC_WEBSOCKET_PROTOCOL)?.to_str().ok()?;
    raw.split(',').map(|p| p.trim()).find_map(|p| match p {
        "vite-hmr" => Some("vite-hmr"),
        "vite-ping" => Some("vite-ping"),
        _ => None,
    })
}

async fn vite_hmr_upgrade(
    State(state): State<Arc<ServerState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let Some(proto) = vite_ws_subprotocol(req.headers()) else {
        return next.run(req).await;
    };
    if let Some(resp) = state.host_policy.reject_ws_origin(req.headers()) {
        return resp;
    }
    if proto != "vite-ping"
        && ws_token_rejected(
            state.ws_token_check,
            &state.ws_token,
            req.headers(),
            req.uri().query(),
        )
    {
        return (
            StatusCode::UNAUTHORIZED,
            "oj: websocket token missing or invalid",
        )
            .into_response();
    }
    let (mut parts, body) = req.into_parts();
    match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(upgrade) if proto == "vite-ping" => {
            upgrade
                .protocols(["vite-ping"])
                .on_upgrade(|mut socket| async move {
                    let _ = socket.send(Message::Close(None)).await;
                })
        }
        Ok(upgrade) => hmr_socket(upgrade, state, true),
        Err(_) => {
            next.run(axum::extract::Request::from_parts(parts, body))
                .await
        }
    }
}

fn hmr_socket(upgrade: WebSocketUpgrade, state: Arc<ServerState>, vite: bool) -> Response {
    let upgrade = if vite {
        upgrade.protocols(["vite-hmr"])
    } else {
        upgrade
    };
    upgrade.on_upgrade(move |mut socket| async move {
        let mut rx = state.reload_tx.subscribe();
        if vite {
            let connected = serde_json::json!({ "type": "connected" }).to_string();
            let _ = socket.send(Message::Text(connected.into())).await;
        }
        // Vite's ws server emits "connection" per accepted client; plugins that
        // push initial state from `server.ws.on("connection")` need the event.
        if let Some(host) = state.plugins.clone() {
            tokio::spawn(async move {
                let _ = host.ws_connection().await;
            });
        }
        let buffered = state.buffered_error.lock().unwrap().take();
        if let Some(frame) = buffered {
            let _ = socket.send(Message::Text(frame.into())).await;
        }
        if state.hmr_gate.is_some() {
            let mode = serde_json::json!({
                "type": "custom",
                "event": "lovable:dev-server-mode",
                "data": { "mode": "classic" },
            })
            .to_string();
            let _ = socket.send(Message::Text(mode.into())).await;
            let boot = boot_progress_frame(0, state.preload_snapshot.len(), Some(0));
            let _ = socket.send(Message::Text(boot.into())).await;
        }
        loop {
            tokio::select! {
                msg = rx.recv() => match msg {
                    Ok(text) => {
                        if socket.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                },
                incoming = socket.recv() => match incoming {
                    None | Some(Err(_)) => break,
                    Some(Ok(Message::Text(text))) => handle_client_message(&state, &text),
                    Some(Ok(_)) => {}
                },
            }
        }
    })
}

/// Vite's `server.allowedHosts` (middlewares/hostCheck.ts, server/ws.ts): a
/// request whose `Host` names something other than localhost, an IP literal, the
/// configured host, or an allowed host is refused with 403 so a malicious page
/// cannot reach the dev server through DNS rebinding. WebSocket upgrades apply
/// the same rule to `Origin`.
#[derive(Debug, Clone, Default)]
struct HostPolicy {
    allow_all: bool,
    allowed: Vec<String>,
}

impl HostPolicy {
    fn from_config(server: &oj_config::ServerConfig, cli_host: Option<&str>) -> Self {
        let mut allowed = Vec::new();
        match &server.allowed_hosts {
            Some(oj_config::AllowedHosts::All(true)) => {
                return Self {
                    allow_all: true,
                    allowed,
                }
            }
            Some(oj_config::AllowedHosts::List(list)) => {
                allowed.extend(list.iter().map(|h| h.to_ascii_lowercase()))
            }
            _ => {}
        }
        // A specific hostname the server was asked to bind to is allowed too.
        if let Some(h) = cli_host.or(server.host.as_deref()) {
            if !matches!(h, "true" | "0.0.0.0" | "::" | "[::]" | "localhost")
                && h.parse::<std::net::IpAddr>().is_err()
            {
                allowed.push(h.to_ascii_lowercase());
            }
        }
        Self {
            allow_all: false,
            allowed,
        }
    }

    fn hostname_allowed(&self, hostname: &str) -> bool {
        if self.allow_all {
            return true;
        }
        let host = hostname
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .to_ascii_lowercase();
        if host.is_empty()
            || host == "localhost"
            || host.ends_with(".localhost")
            || host.parse::<std::net::IpAddr>().is_ok()
        {
            return true;
        }
        self.allowed.iter().any(|a| {
            if let Some(domain) = a.strip_prefix('.') {
                host == domain || host.ends_with(a.as_str())
            } else {
                host == *a
            }
        })
    }

    /// The hostname of a `Host` header value (`example.com:5173`, `[::1]:5173`).
    fn host_header_name(value: &str) -> &str {
        let v = value.trim();
        if let Some(rest) = v.strip_prefix('[') {
            return rest.split(']').next().unwrap_or(rest);
        }
        v.rsplit_once(':').map(|(h, _)| h).unwrap_or(v)
    }

    fn reject_message(host: &str) -> String {
        format!(
            "Blocked request. This host ({host}) is not allowed.\nTo allow this host, add \"{host}\" to `server.allowedHosts` in your config."
        )
    }

    fn reject_ws_origin(&self, headers: &HeaderMap) -> Option<Response> {
        let origin = headers.get(header::ORIGIN)?.to_str().ok()?;
        let host = origin
            .split("://")
            .nth(1)
            .map(|rest| rest.split('/').next().unwrap_or(rest))
            .map(Self::host_header_name)
            .unwrap_or("");
        if self.hostname_allowed(host) {
            return None;
        }
        Some((StatusCode::FORBIDDEN, Self::reject_message(host)).into_response())
    }
}

async fn host_check_middleware(
    State(state): State<Arc<ServerState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if let Some(raw) = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
    {
        let host = HostPolicy::host_header_name(raw);
        if !state.host_policy.hostname_allowed(host) {
            return (StatusCode::FORBIDDEN, HostPolicy::reject_message(host)).into_response();
        }
    }
    next.run(req).await
}

/// Vite's `server.cors` (the `cors` package behind it). Unset: only localhost
/// origins (Vite's `defaultAllowedOrigins`); `true`: reflect any origin;
/// `false`: no CORS headers; an object: exact origins, methods, headers,
/// credentials, max-age.
#[derive(Debug, Clone)]
struct CorsPolicy {
    origin: CorsOrigin,
    methods: String,
    allowed_headers: Option<String>,
    credentials: bool,
    max_age: Option<u64>,
}

#[derive(Debug, Clone)]
enum CorsOrigin {
    Any,
    LocalhostDefault,
    List(Vec<String>),
}

impl CorsPolicy {
    fn from_config(cfg: Option<&oj_config::CorsConfig>) -> Option<Self> {
        let default_methods = "GET,HEAD,PUT,PATCH,POST,DELETE".to_string();
        let list_or_str = |v: &serde_json::Value| -> Option<String> {
            match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Array(a) => Some(
                    a.iter()
                        .filter_map(|x| x.as_str())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
                _ => None,
            }
        };
        match cfg {
            Some(oj_config::CorsConfig::Toggle(false)) => None,
            Some(oj_config::CorsConfig::Toggle(true)) => Some(Self {
                origin: CorsOrigin::Any,
                methods: default_methods,
                allowed_headers: None,
                credentials: false,
                max_age: None,
            }),
            Some(oj_config::CorsConfig::Options(o)) => {
                let origin = match &o.origin {
                    Some(serde_json::Value::Bool(true)) | Some(serde_json::Value::String(_))
                        if o.origin.as_ref().and_then(|v| v.as_str()) == Some("*") =>
                    {
                        CorsOrigin::Any
                    }
                    Some(serde_json::Value::Bool(true)) => CorsOrigin::Any,
                    Some(serde_json::Value::Bool(false)) => return None,
                    Some(serde_json::Value::String(s)) => CorsOrigin::List(vec![s.clone()]),
                    Some(serde_json::Value::Array(a)) => CorsOrigin::List(
                        a.iter()
                            .filter_map(|x| x.as_str().map(str::to_string))
                            .collect(),
                    ),
                    _ => CorsOrigin::LocalhostDefault,
                };
                Some(Self {
                    origin,
                    methods: o
                        .methods
                        .as_ref()
                        .and_then(list_or_str)
                        .unwrap_or(default_methods),
                    allowed_headers: o.allowed_headers.as_ref().and_then(list_or_str),
                    credentials: o.credentials.unwrap_or(false),
                    max_age: o.max_age,
                })
            }
            None => Some(Self {
                origin: CorsOrigin::LocalhostDefault,
                methods: default_methods,
                allowed_headers: None,
                credentials: false,
                max_age: None,
            }),
        }
    }

    fn allows(&self, origin: &str) -> bool {
        match &self.origin {
            CorsOrigin::Any => true,
            CorsOrigin::List(list) => list.iter().any(|o| o == origin),
            CorsOrigin::LocalhostDefault => is_localhost_origin(origin),
        }
    }
}

/// Vite's `defaultAllowedOrigins`:
/// `/^https?:\/\/(?:(?:[^:]+\.)?localhost|127\.0\.0\.1|\[::1\])(?::\d+)?$/`.
fn is_localhost_origin(origin: &str) -> bool {
    let rest = match origin
        .strip_prefix("https://")
        .or_else(|| origin.strip_prefix("http://"))
    {
        Some(r) => r,
        None => return false,
    };
    let (host, port) = if let Some(r) = rest.strip_prefix("[::1]") {
        ("[::1]", r)
    } else {
        rest.rsplit_once(':').unwrap_or((rest, ""))
    };
    let port_ok = port.is_empty()
        || port
            .strip_prefix(':')
            .unwrap_or(port)
            .chars()
            .all(|c| c.is_ascii_digit())
            && !port.strip_prefix(':').unwrap_or(port).is_empty();
    if !port_ok {
        return false;
    }
    host == "localhost"
        || host == "127.0.0.1"
        || host == "[::1]"
        || (host.ends_with(".localhost") && !host[..host.len() - ".localhost".len()].contains(':'))
}

async fn cors_middleware(
    State(policy): State<Arc<CorsPolicy>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let allowed = origin.as_deref().is_some_and(|o| policy.allows(o));
    let preflight = req.method() == axum::http::Method::OPTIONS
        && req
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    let mut resp = if preflight && allowed {
        let mut r = StatusCode::NO_CONTENT.into_response();
        let h = r.headers_mut();
        if let Ok(v) = policy.methods.parse() {
            h.insert(header::ACCESS_CONTROL_ALLOW_METHODS, v);
        }
        let requested = req
            .headers()
            .get(header::ACCESS_CONTROL_REQUEST_HEADERS)
            .cloned();
        match (&policy.allowed_headers, requested) {
            (Some(list), _) => {
                if let Ok(v) = list.parse() {
                    h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v);
                }
            }
            (None, Some(v)) => {
                h.insert(header::ACCESS_CONTROL_ALLOW_HEADERS, v);
                h.append(
                    header::VARY,
                    header::HeaderValue::from_static("Access-Control-Request-Headers"),
                );
            }
            (None, None) => {}
        }
        if let Some(age) = policy.max_age {
            if let Ok(v) = age.to_string().parse() {
                h.insert(header::ACCESS_CONTROL_MAX_AGE, v);
            }
        }
        h.insert(
            header::CONTENT_LENGTH,
            header::HeaderValue::from_static("0"),
        );
        r
    } else {
        next.run(req).await
    };
    if allowed {
        let h = resp.headers_mut();
        if let Some(v) = origin.as_deref().and_then(|o| o.parse().ok()) {
            h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, v);
        }
        h.append(header::VARY, header::HeaderValue::from_static("Origin"));
        if policy.credentials {
            h.insert(
                header::ACCESS_CONTROL_ALLOW_CREDENTIALS,
                header::HeaderValue::from_static("true"),
            );
        }
    }
    resp
}

async fn apply_dev_headers(
    State(headers): State<Arc<Vec<(header::HeaderName, header::HeaderValue)>>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    for (name, value) in headers.iter() {
        h.insert(name.clone(), value.clone());
    }
    resp
}

/// The upstream url for a matched proxy entry: the target joined with the
/// (rewritten) request path and the original query. Applies the `{from,to}`
/// string rewrite form only; the FUNCTION rewrite form is honored by the Node
/// proxy (the browser path delegates there when a plugin host is running).
/// Delegate a request to the single Vite-shaped proxy in the plugin host's
/// middleware stack: stream the body there, stream the reply back. If the host's
/// proxy declined (`x-oj-fallthrough` — a `bypass` that returned a string and
/// fell through, or a JS-RegExp non-match on an uncompilable context), serve the
/// possibly-rewritten url through normal oj routing instead of returning the
/// host's 404. The re-routed request carries no body (it was streamed to the
/// host): this covers the realistic GET bypass rewrite; a non-GET fallthrough
/// re-route loses its body.
async fn delegate_to_node_proxy(
    next: axum::middleware::Next,
    port: u16,
    req: axum::extract::Request,
) -> Response {
    let (parts, body) = req.into_parts();
    let method = parts.method.as_str().to_string();
    let pq = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| parts.uri.path().to_string());
    match proxy_to_loopback_streaming(port, &method, &pq, &parts.headers, Some(body)).await {
        Ok(resp) => {
            if resp.headers().contains_key("x-oj-fallthrough") {
                let mut parts = parts;
                // Re-route only to an origin-form path (starts with a single
                // `/`): a `bypass` returning an absolute or protocol-relative
                // URL must never be fed to native routing — treat that as
                // no-rewrite and serve the original path.
                if let Some(uri) = resp
                    .headers()
                    .get("x-oj-rewritten-url")
                    .and_then(|v| v.to_str().ok())
                    .filter(|s| s.starts_with('/') && !s.starts_with("//"))
                    .and_then(|s| s.parse::<Uri>().ok())
                {
                    parts.uri = uri;
                }
                let rreq = axum::extract::Request::from_parts(parts, Body::empty());
                return next.run(rreq).await;
            }
            resp
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("oj proxy delegation to plugin host failed: {e}"),
        )
            .into_response(),
    }
}

fn proxy_target(entry: &oj_config::ProxyEntry, path: &str, query: Option<&str>) -> String {
    let mut fwd_path = path.to_string();
    if let Some((from, to)) = entry.rewrite() {
        if let Some(stripped) = from.strip_prefix('^') {
            if let Some(rest) = fwd_path.strip_prefix(stripped) {
                fwd_path = format!("{to}{rest}");
            }
        } else {
            fwd_path = fwd_path.replacen(from, to, 1);
        }
    }
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    format!(
        "{}{}{}",
        entry.target().trim_end_matches('/'),
        fwd_path,
        query
    )
}

async fn proxy_middleware(
    State(state): State<Arc<ServerState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let path = req.uri().path().to_string();
    // Vite matches contexts against `req.url`, path and query together, so a
    // `^` regex context can key off a query parameter.
    let url = match req.uri().query() {
        Some(q) => format!("{path}?{q}"),
        None => path.clone(),
    };
    let matched = select_proxy(&state.proxy, &state.proxy_regex, &url);
    let Some((prefix, entry)) = matched else {
        // No context matched. A `^`-context oj's regex engine could not compile
        // (a JS RegExp using lookaround/backreference/alternation) never matches
        // on this browser-facing path and is served natively — oj's pre-existing
        // behavior, with a loud one-time warning at startup (proxy_context_regex).
        // The worker path still proxies such a context (the Node proxy holds the
        // full JS RegExp); full JS-regex-context support for the browser path is
        // a deferred follow-up rather than an unsound approximation here.
        return next.run(req).await;
    };
    let prefix = prefix.to_string();
    let entry = entry.clone();

    // A WebSocket upgrade on a `ws: true` entry is tunneled message by message
    // by the Rust proxy: the inbound listener owns the browser's upgrade, and
    // the worker's outbound fetch is never a ws upgrade, so the single Node
    // proxy need not handle upgrades — nothing regresses.
    if entry.ws() && is_websocket_upgrade(req.headers()) {
        let target = proxy_target(&entry, &path, req.uri().query());
        return proxy_websocket(state, req, Some(&entry), &target).await;
    }

    // `ws: false` (Vite's default): the upgrade belongs to the shared
    // httpServer's `upgrade` listeners, so relay it to the plugin middleware
    // server as a real upgrade. vite-hmr/vite-ping stay with oj's own
    // endpoint (this middleware wraps outside vite_hmr_upgrade).
    if is_websocket_upgrade(req.headers()) && vite_ws_subprotocol(req.headers()).is_none() {
        if let Some(port) = state.plugin_serve.mw_port() {
            return relay_upgrade_to_plugin_middleware(state, req, port).await;
        }
    }

    // The single, Vite-shaped proxy lives in the plugin host's middleware stack.
    // Whenever a plugin host is running, delegate the matched request there so
    // the browser path and the worker's outbound fetch share ONE proxy, with the
    // app's real config (function `rewrite`, `configure`, `bypass`) intact. The
    // ORIGINAL unstripped path is forwarded; the Node proxy re-matches and
    // rewrites it. The request body streams through, so uploads are not capped;
    // a `bypass` that returned a string (host proxy falls through) is served via
    // normal routing at the rewritten url rather than returning the host's 404.
    if let Some(port) = state.plugin_serve.mw_port() {
        return delegate_to_node_proxy(next, port, req).await;
    }

    // Fallback: no plugin host (a plain `oj dev` app with `server.proxy`, or the
    // brief boot window before the middleware port is known). The Rust proxy
    // forwards directly, applying the `{from,to}` string rewrite form.
    let target = proxy_target(&entry, &path, req.uri().query());
    let method = req.method().clone();
    let req_headers = req.headers().clone();
    // Small bodies are buffered so their Content-Length is preserved; a chunked
    // or large body streams through, so uploads are not capped by a buffer.
    let content_length = req_headers
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let chunked = req_headers.contains_key(header::TRANSFER_ENCODING);
    let stream_request = chunked || content_length.is_some_and(|n| n > 1024 * 1024);
    let body: reqwest::Body = if stream_request {
        reqwest::Body::wrap_stream(req.into_body().into_data_stream())
    } else {
        match axum::body::to_bytes(req.into_body(), 1024 * 1024).await {
            Ok(b) => reqwest::Body::from(b),
            Err(e) => {
                return (StatusCode::BAD_GATEWAY, format!("oj proxy: body read: {e}"))
                    .into_response()
            }
        }
    };

    let client = if entry.secure() {
        &state.http
    } else {
        state.http_insecure.get_or_init(|| {
            reqwest::Client::builder()
                .danger_accept_invalid_certs(true)
                .build()
                .expect("reqwest client")
        })
    };
    let mut out = client.request(method, &target).body(body);
    for (name, value) in req_headers.iter() {
        if entry.change_origin() && name == header::HOST {
            continue;
        }
        out = out.header(name, value);
    }

    match out.send().await {
        Ok(resp) => {
            let status = resp.status();
            let headers = resp.headers().clone();
            // Stream the upstream body: server-sent events and long-polling
            // responses reach the browser chunk by chunk instead of at the end.
            let mut response = Response::new(Body::from_stream(resp.bytes_stream()));
            *response.status_mut() = status;
            for (name, value) in headers.iter() {
                if name == header::TRANSFER_ENCODING || name == header::CONTENT_LENGTH {
                    continue;
                }
                response.headers_mut().append(name, value.clone());
            }
            response
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            format!("oj proxy to {prefix} failed: {e}"),
        )
            .into_response(),
    }
}

/// The compiled pattern of a `server.proxy` context that starts with `^` (Vite:
/// `new RegExp(context)`); `None` for a plain prefix context. A pattern that
/// does not compile is reported once and then behaves as a prefix, which for a
/// `^...` string never matches, the same as Vite throwing at startup would leave
/// it unreachable.
fn proxy_context_regex(context: &str) -> Option<regex::Regex> {
    if !context.starts_with('^') {
        return None;
    }
    match regex::Regex::new(context) {
        Ok(re) => Some(re),
        Err(err) => {
            eprintln!(
                "oj: warning: server.proxy context {context:?} uses a regex oj cannot compile \
                 ({err}) — likely a JS RegExp lookahead/backreference/alternation. Browser \
                 requests to it are matched by literal prefix and may not proxy (the worker path \
                 still proxies it); please file an issue if you need full JS-regex proxy contexts."
            );
            None
        }
    }
}

/// Vite's `doesProxyContextMatchUrl`: a `^` context is a regex tested against
/// the request url (path plus query), any other context is a path prefix.
pub fn proxy_context_matches(context: &str, url: &str) -> bool {
    if let Some(re) = proxy_context_regex(context) {
        return re.is_match(url);
    }
    url.starts_with(context)
}

/// The proxy entry for a request url. Vite takes the first matching context in
/// config order; oj's config is a sorted map, so the most specific plain prefix
/// wins and a regex context applies when no prefix matches.
fn select_proxy<'a>(
    entries: &'a [(String, oj_config::ProxyEntry)],
    regexes: &[Option<regex::Regex>],
    url: &str,
) -> Option<(&'a str, &'a oj_config::ProxyEntry)> {
    let prefix = entries
        .iter()
        .zip(regexes)
        .filter(|((ctx, _), re)| re.is_none() && url.starts_with(ctx.as_str()))
        .max_by_key(|((ctx, _), _)| ctx.len())
        .map(|((ctx, entry), _)| (ctx.as_str(), entry));
    prefix.or_else(|| {
        entries
            .iter()
            .zip(regexes)
            .find(|(_, re)| re.as_ref().is_some_and(|re| re.is_match(url)))
            .map(|((ctx, entry), _)| (ctx.as_str(), entry))
    })
}

pub fn is_websocket_upgrade(h: &HeaderMap) -> bool {
    h.get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"))
}

/// The upstream WebSocket url for a proxy target (`http://` targets become `ws://`).
/// The http(s) origin of a ws(s) target url, for `rewriteWsOrigin`.
fn ws_target_origin(url: &str) -> String {
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) => (s, r),
        None => return url.to_string(),
    };
    let authority = rest.split('/').next().unwrap_or(rest);
    let scheme = match scheme {
        "wss" => "https",
        "ws" => "http",
        other => other,
    };
    format!("{scheme}://{authority}")
}

fn ws_target_url(target: &str) -> String {
    if let Some(rest) = target.strip_prefix("https://") {
        format!("wss://{rest}")
    } else if let Some(rest) = target.strip_prefix("http://") {
        format!("ws://{rest}")
    } else {
        target.to_string()
    }
}

/// Request headers that travel to the upstream WebSocket. The handshake headers
/// are regenerated by the client library (duplicates would fail its handshake).
fn ws_forwardable_header(name: &header::HeaderName) -> bool {
    !matches!(
        *name,
        header::HOST
            | header::CONNECTION
            | header::UPGRADE
            | header::SEC_WEBSOCKET_KEY
            | header::SEC_WEBSOCKET_VERSION
            | header::SEC_WEBSOCKET_ACCEPT
            | header::SEC_WEBSOCKET_EXTENSIONS
            | header::CONTENT_LENGTH
            | header::TRANSFER_ENCODING
    )
}

impl ServerState {
    /// The rustls config for a proxied `wss://` target, built once: the platform
    /// trust store (what reqwest verifies with for the HTTP side), or no
    /// certificate check at all when the proxy entry says `secure: false`
    /// (http-proxy's `secure`), for self-signed dev backends.
    fn proxy_tls_config(
        &self,
        secure: bool,
    ) -> Result<std::sync::Arc<rustls::ClientConfig>, String> {
        self.proxy_tls[usize::from(!secure)]
            .get_or_init(|| proxy_tls_config(secure).map(std::sync::Arc::new))
            .clone()
    }
}

fn proxy_tls_config(secure: bool) -> Result<rustls::ClientConfig, String> {
    use rustls_platform_verifier::BuilderVerifierExt;
    let provider = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(std::sync::Arc::clone(&provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?;
    if secure {
        return Ok(builder
            .with_platform_verifier()
            .map_err(|e| e.to_string())?
            .with_no_client_auth());
    }
    Ok(builder
        .dangerous()
        .with_custom_certificate_verifier(std::sync::Arc::new(AcceptAnyCertificate(provider)))
        .with_no_client_auth())
}

/// `secure: false`: the certificate is not checked; signatures still are, so the
/// connection is at least the one the server we reached is speaking on.
#[derive(Debug)]
struct AcceptAnyCertificate(std::sync::Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.0.signature_verification_algorithms,
        )
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// A browser upgrade no Rust endpoint claims: hand it to the plugin
/// middleware server as a real upgrade, so configureServer `upgrade`
/// listeners fire like on Vite's shared httpServer.
async fn relay_upgrade_to_plugin_middleware(
    state: Arc<ServerState>,
    req: axum::extract::Request,
    port: u16,
) -> Response {
    let raw = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str().to_string())
        .unwrap_or_else(|| req.uri().path().to_string());
    let target = format!("http://127.0.0.1:{port}{raw}");
    proxy_websocket(state, req, None, &target).await
}

/// `server.proxy` with `ws: true`: accept the browser's WebSocket, open one to
/// the target with the same path, query, subprotocols and cookies, and relay
/// messages both ways until either side closes (Vite: http-proxy `ws`).
async fn proxy_websocket(
    state: Arc<ServerState>,
    req: axum::extract::Request,
    entry: Option<&oj_config::ProxyEntry>,
    target: &str,
) -> Response {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite as ts;

    let (mut parts, _body) = req.into_parts();
    let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(u) => u,
        Err(rejection) => return rejection.into_response(),
    };
    let url = ws_target_url(target);
    // tungstenite takes a ready-made request as-is: the handshake headers have
    // to be present (it generates them only for a bare url), then the browser's
    // remaining headers (cookies, origin, subprotocols) ride along.
    let target_uri: axum::http::Uri = match url.parse() {
        Ok(u) => u,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("oj proxy: bad websocket target {url}: {e}"),
            )
                .into_response()
        }
    };
    // host:port without any userinfo, the same authority the dial below uses.
    let target_host = match (target_uri.host(), target_uri.port_u16()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => String::new(),
    };
    // http-proxy keeps the browser's Host unless `changeOrigin` asks for the
    // target's; `rewriteWsOrigin` (Vite) swaps the Origin for the target's origin.
    let host: String = if entry.is_some_and(|e| e.change_origin()) {
        target_host.clone()
    } else {
        parts
            .headers
            .get(header::HOST)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .unwrap_or(target_host)
    };
    let mut builder = axum::http::Request::builder()
        .uri(url.as_str())
        .header(header::HOST, host)
        .header(header::CONNECTION, "Upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_VERSION, "13")
        .header(
            header::SEC_WEBSOCKET_KEY,
            ts::handshake::client::generate_key(),
        );
    for (name, value) in parts.headers.iter() {
        if !ws_forwardable_header(name) {
            continue;
        }
        if *name == header::ORIGIN && entry.is_some_and(|e| e.rewrite_ws_origin()) {
            builder = builder.header(name, ws_target_origin(&url));
            continue;
        }
        builder = builder.header(name, value);
    }
    let upstream_req = match builder.body(()) {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("oj proxy: ws request: {e}"),
            )
                .into_response()
        }
    };
    // `wss://` targets: tungstenite dials TCP and TLS itself, given a rustls
    // config (system trust store, or any certificate when the entry says
    // `secure: false`), as http-proxy does for a `wss:` target.
    let connector = if url.starts_with("wss://") {
        match state.proxy_tls_config(entry.is_none_or(|e| e.secure())) {
            Ok(cfg) => Some(tokio_tungstenite::Connector::Rustls(cfg)),
            Err(e) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    format!("oj proxy: tls config: {e}"),
                )
                    .into_response()
            }
        }
    } else {
        Some(tokio_tungstenite::Connector::Plain)
    };
    let connect =
        tokio_tungstenite::connect_async_tls_with_config(upstream_req, None, false, connector);
    // No oj-side deadline, matching Vite: its plugin `upgrade` listeners
    // share the http server, so an upgrade nobody claims simply dangles until
    // the CLIENT gives up — the client owns the patience. Here the dangle is
    // this pending dial (the middleware server accepted TCP but no listener
    // answered the handshake); hyper aborts this future when the client
    // disconnects, so the dangling cost is one socket and one parked task per
    // waiting client, the same surface Vite exposes.
    let connected = connect.await;
    let (upstream, upstream_resp) = match connected {
        Ok(pair) => pair,
        Err(e) => {
            return (
                StatusCode::BAD_GATEWAY,
                format!("oj proxy: websocket to {url} failed: {e}"),
            )
                .into_response()
        }
    };
    let selected = upstream_resp
        .headers()
        .get(header::SEC_WEBSOCKET_PROTOCOL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let upgrade = match selected {
        Some(p) => upgrade.protocols([p]),
        None => upgrade,
    };

    fn to_ts(m: Message) -> ts::Message {
        match m {
            Message::Text(t) => ts::Message::Text(ts::Utf8Bytes::from(t.as_str())),
            Message::Binary(b) => ts::Message::Binary(b),
            Message::Ping(b) => ts::Message::Ping(b),
            Message::Pong(b) => ts::Message::Pong(b),
            Message::Close(c) => ts::Message::Close(c.map(|c| ts::protocol::CloseFrame {
                code: ts::protocol::frame::coding::CloseCode::from(c.code),
                reason: ts::Utf8Bytes::from(c.reason.as_str()),
            })),
        }
    }
    fn from_ts(m: ts::Message) -> Option<Message> {
        Some(match m {
            ts::Message::Text(t) => Message::Text(t.as_str().into()),
            ts::Message::Binary(b) => Message::Binary(b),
            ts::Message::Ping(b) => Message::Ping(b),
            ts::Message::Pong(b) => Message::Pong(b),
            ts::Message::Close(c) => Message::Close(c.map(|c| axum::extract::ws::CloseFrame {
                code: c.code.into(),
                reason: c.reason.as_str().into(),
            })),
            ts::Message::Frame(_) => return None,
        })
    }

    upgrade.on_upgrade(move |client| async move {
        let (mut up_tx, mut up_rx) = upstream.split();
        let (mut cl_tx, mut cl_rx) = client.split();
        let client_to_upstream = async {
            while let Some(Ok(m)) = cl_rx.next().await {
                if up_tx.send(to_ts(m)).await.is_err() {
                    break;
                }
            }
            let _ = up_tx.close().await;
        };
        let upstream_to_client = async {
            while let Some(Ok(m)) = up_rx.next().await {
                if let Some(m) = from_ts(m) {
                    if cl_tx.send(m).await.is_err() {
                        break;
                    }
                }
            }
            let _ = cl_tx.close().await;
        };
        tokio::join!(client_to_upstream, upstream_to_client);
    })
}

/// `url`: the page's request path (Vite's ctx.path); `file`: the html on disk
/// (ctx.filename). A throwing transformIndexHtml fails the request with the
/// plugin error (Vite's indexHtml middleware lets it reach the error
/// middleware) instead of serving the untransformed page.
async fn serve_html(state: &ServerState, bytes: Vec<u8>, url: &str, file: &Path) -> Response {
    let mut raw = String::from_utf8_lossy(&bytes).into_owned();
    // %VITE_*% / import.meta.env substitution (Vite's htmlEnvHook), a pre-hook
    // before any plugin transformIndexHtml.
    raw = oj_env::replace_html_env(&raw, &state.html_env);
    if let Some(host) = &state.plugins {
        let ctx = serde_json::json!({
            "path": url,
            "filename": file.display().to_string(),
            "originalUrl": url,
        })
        .to_string();
        match host.transform_index_html(&raw, &ctx).await {
            Ok(out) => raw = out,
            Err(e) => {
                eprintln!("oj: transformIndexHtml failed for {url}: {e}");
                return (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
                    format!("oj: transformIndexHtml failed for {url}\n{e}"),
                )
                    .into_response();
            }
        }
    }
    let mut html = inject_module_preloads(inject_dev_scripts(raw), state);
    if let Some(nonce) = &state.csp_nonce {
        html = inject_csp_nonce(&html, nonce);
    }
    ([(header::CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()
}

/// Vite's `html.cspNonce` (injectNonceAttributeTagHook + injectCspNonceMetaTagHook):
/// every `<script>`, `<style>` and stylesheet/modulepreload/preload `<link>`
/// without a `nonce` gets `nonce="<nonce>"`, and `<head>` gets a
/// `<meta property="csp-nonce" nonce="<nonce>">` the runtime reads back.
pub fn inject_csp_nonce(html: &str, nonce: &str) -> String {
    let mut out = String::with_capacity(html.len() + 256);
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        let (before, at) = rest.split_at(lt);
        out.push_str(before);
        let name_end = at[1..]
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '-')
            .map(|i| i + 1)
            .unwrap_or(at.len());
        let name = at[1..name_end].to_ascii_lowercase();
        let Some(gt) = at.find('>') else {
            out.push_str(at);
            return out;
        };
        let tag = &at[..gt];
        let wants = match name.as_str() {
            "script" | "style" => true,
            "link" => html_tag_attr(tag, "rel").is_some_and(|rel| {
                rel.split_whitespace().any(|r| {
                    matches!(
                        r.to_ascii_lowercase().as_str(),
                        "stylesheet" | "modulepreload" | "preload"
                    )
                })
            }),
            _ => false,
        };
        if wants && html_tag_attr(tag, "nonce").is_none() {
            let body = tag.trim_end_matches('/').trim_end();
            let self_closing = tag.trim_end().ends_with('/');
            out.push_str(body);
            out.push_str(&format!(" nonce=\"{nonce}\""));
            out.push_str(if self_closing { " />" } else { ">" });
        } else {
            out.push_str(&at[..=gt]);
        }
        rest = &at[gt + 1..];
        // Skip raw text content so a `<` inside a script or style body is not
        // read as a tag.
        if matches!(name.as_str(), "script" | "style") && !tag.trim_end().ends_with('/') {
            let close = format!("</{name}");
            if let Some(end) = rest.to_ascii_lowercase().find(&close) {
                out.push_str(&rest[..end]);
                rest = &rest[end..];
            }
        }
    }
    out.push_str(rest);
    if !out.contains("property=\"csp-nonce\"") {
        let meta = format!("<meta property=\"csp-nonce\" nonce=\"{nonce}\">");
        out = match out.find("<head>") {
            Some(i) => {
                let at = i + "<head>".len();
                format!("{}\n{meta}{}", &out[..at], &out[at..])
            }
            None => format!("{meta}\n{out}"),
        };
    }
    out
}

async fn serve_index_html(state: &ServerState) -> Response {
    match tokio::fs::read(state.root.join("index.html")).await {
        Ok(bytes) => serve_html(state, bytes, "/index.html", &state.root.join("index.html")).await,
        Err(_) => (StatusCode::NOT_FOUND, "oj: index.html not found").into_response(),
    }
}

/// The html file Vite's htmlFallback middleware rewrites an unmatched path to:
/// a trailing slash asks for that directory's `index.html`, anything else for
/// the `.html` sibling. An explicit `.html` request is left alone (it either
/// exists, and was found already, or is a 404).
fn html_fallback_candidate(rel: &str) -> Option<String> {
    if rel.is_empty() || rel.ends_with(".html") {
        return None;
    }
    if rel.ends_with('/') {
        Some(format!("{rel}index.html"))
    } else {
        Some(format!("{rel}.html"))
    }
}

/// Vite's htmlFallback only acts on requests that accept html: no `Accept`, an
/// empty one, or one naming `text/html` or `*/*`.
fn accepts_html_fallback(headers: &HeaderMap) -> bool {
    match headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()) {
        None => true,
        Some(a) => a.is_empty() || a.contains("text/html") || a.contains("*/*"),
    }
}

fn is_spa_navigation(rel: &str, headers: &HeaderMap) -> bool {
    if rel.starts_with('@')
        || rel.starts_with("__")
        || rel.starts_with("src/")
        || rel.starts_with("node_modules/")
    {
        return false;
    }
    let last = rel.rsplit('/').next().unwrap_or("");
    let no_extension = !last.contains('.');
    let accepts_html = headers
        .get(header::ACCEPT)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|a| a.contains("text/html"));
    no_extension || accepts_html
}

async fn serve_fallback(
    State(state): State<Arc<ServerState>>,
    req: axum::extract::Request,
) -> Response {
    // A WebSocket upgrade nothing above claimed (oj's own ws endpoints and
    // `ws: true` proxies run earlier): plugin `upgrade` listeners get it.
    if is_websocket_upgrade(req.headers()) && vite_ws_subprotocol(req.headers()).is_none() {
        if let Some(port) = state.plugin_serve.mw_port() {
            return relay_upgrade_to_plugin_middleware(state, req, port).await;
        }
    }
    if req.method() == Method::GET {
        let headers = req.headers().clone();
        let uri = req.uri().clone();
        return serve_path(State(state), headers, uri).await;
    }
    let method = req.method().clone();
    let uri = req.uri().clone();
    let headers = req.headers().clone();
    let body = axum::body::to_bytes(req.into_body(), usize::MAX)
        .await
        .unwrap_or_default()
        .to_vec();
    forward_to_plugin_middleware(&state, &method, &uri, &headers, body)
        .await
        .unwrap_or_else(|| (StatusCode::NOT_FOUND, "oj: not found").into_response())
}

async fn forward_to_plugin_middleware(
    state: &ServerState,
    method: &Method,
    uri: &Uri,
    headers: &HeaderMap,
    body: Vec<u8>,
) -> Option<Response> {
    let port = state.plugin_serve.mw_port()?;
    let pq = uri
        .path_and_query()
        .map(|p| p.as_str())
        .unwrap_or(uri.path());
    let target = format!("http://127.0.0.1:{port}{pq}");
    let rmethod = reqwest::Method::from_bytes(method.as_str().as_bytes()).ok()?;
    let mut out = state.http.request(rmethod, &target);
    for (name, value) in headers.iter() {
        if name == header::HOST {
            continue;
        }
        out = out.header(name, value);
    }
    if !body.is_empty() {
        out = out.body(body);
    }
    let resp = out.send().await.ok()?;
    if resp.headers().contains_key("x-oj-fallthrough") {
        return None;
    }
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    // Stream the worker/plugin response through instead of buffering it. TanStack
    // Start streams its dehydrated data (queryStream + deferred promises) into the
    // HTML; buffering with resp.bytes() withholds the whole document until the SSR
    // stream closes, so the client's hydration never sees it progressively. Vite
    // pipes the worker Response body through (Readable.fromWeb); this is the same.
    let mut response = Response::new(Body::from_stream(resp.bytes_stream()));
    *response.status_mut() = status;
    for (name, value) in resp_headers.iter() {
        if name == header::TRANSFER_ENCODING || name == header::CONTENT_LENGTH {
            continue;
        }
        response.headers_mut().append(name, value.clone());
    }
    Some(response)
}

// Forward a GET to a plugin's configureServer middleware; returns None when the
// middleware falls through (x-oj-fallthrough), so the caller can fall back to
// SSR. Used by the TanStack start path, where GET requests are otherwise
// SSR'd and would never reach editor endpoints (the dev-server bridge).
// Tell a plugin's configureServer middleware server that source files changed,
// so it can invalidate the DevEnvironments' module graphs and send targeted
// HMR updates (the Cloudflare-plugin HMR path). Each change carries Vite's
// watcher event type: "update" | "create" | "delete". Fire-and-forget.
pub async fn notify_plugin_mw_invalidate(port: u16, changes: &[(String, &'static str)]) {
    let client = plugin_mw_client();
    let changes: Vec<serde_json::Value> = changes
        .iter()
        .map(|(path, kind)| serde_json::json!({ "path": path, "type": kind }))
        .collect();
    let body = serde_json::json!({ "changes": changes }).to_string();
    let _ = client
        .post(format!("http://127.0.0.1:{port}/__oj_invalidate"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await;
}

fn plugin_mw_client() -> &'static reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    // The endpoint answers only after the plugin hotUpdate hooks ran, and the
    // settled-batch call blocks the watcher thread: a hung hook must not
    // freeze rebuilds for the session, so the request is bounded.
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default()
    })
}

/// Tell the plugin middleware to resynchronize: invalidate every runner-backed
/// DevEnvironment's whole module graph and send a full-reload, bypassing the
/// per-change dedup. Sent on late activation, covering every edit made while
/// the middleware path was down (the watcher had no port to invalidate).
/// Returns whether the middleware acknowledged the ENQUEUE: the host answers
/// the moment the resync is on its serialized invalidate queue (guaranteed to
/// run after everything already queued), so the ACK is fast even behind a
/// slow queue. 202-style semantics: the ACK means only "enqueued", never
/// "ran" — the host pushes `{ ojResyncDone }` when the resync EXECUTES, and
/// the caller claims "resynced" only on that completion signal
/// ([`await_resync_completion`]); a queue that never drains warns instead of
/// logging success.
pub async fn notify_plugin_mw_resync(port: u16) -> bool {
    match plugin_mw_client()
        .post(format!("http://127.0.0.1:{port}/__oj_invalidate"))
        .header(header::CONTENT_TYPE, "application/json")
        .body(r#"{"resync":true}"#)
        .send()
        .await
    {
        Ok(resp) => resp.status().is_success(),
        Err(_) => false,
    }
}

/// [`notify_plugin_mw_resync`] with a few backed-off retries: the resync races
/// the host's middleware server settling in, and a transient failure must not
/// leave the degraded window's edits silently stale. The host ACKs on enqueue
/// and coalesces duplicates (a pending resync absorbs them), so retrying is
/// safe — it can never stack full-reloads — and a client timeout means only
/// "enqueue unconfirmed", which the next attempt settles either way. `false`
/// after the last attempt — the caller then warns instead of logging success.
pub async fn resync_plugin_mw_with_retry(port: u16) -> bool {
    for delay in [
        std::time::Duration::ZERO,
        std::time::Duration::from_millis(250),
        std::time::Duration::from_millis(750),
    ] {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        if notify_plugin_mw_resync(port).await {
            return true;
        }
    }
    false
}

/// Waits for the host's resync-executed signal — the `{ ojResyncDone }`
/// counter moving past the `baseline` snapshotted BEFORE the enqueue (so a
/// completion racing ahead of this wait is never missed, and one push may
/// answer several coalesced enqueues) — bounded, so a stuck invalidate queue
/// turns into a caller warning rather than an eternal wait or a false
/// "resynced" claim off the enqueue ack.
pub async fn await_resync_completion(
    done: &mut tokio::sync::watch::Receiver<u64>,
    baseline: u64,
    bound: std::time::Duration,
) -> bool {
    tokio::time::timeout(bound, async {
        loop {
            if *done.borrow_and_update() > baseline {
                return true;
            }
            if done.changed().await.is_err() {
                return false;
            }
        }
    })
    .await
    .unwrap_or(false)
}

// Method+body version of forward_get_to_plugin_mw, for the /_serverFn/ path so
// server functions reach a Cloudflare plugin's worker (Miniflare) like documents
// do, instead of running in the Node runner without the real runtime/bindings.
/// Proxy one request to a loopback HTTP service (the plugin middleware server or
/// the Start SSR runner) and stream its response back. Bodies are passed as
/// bytes, so binary uploads and responses survive; the original `Host` travels
/// as `x-oj-host` (see `loopback_request_headers`) so the service can build the
/// app's own absolute URLs.
/// Stream the response through instead of buffering it: TanStack Start streams
/// its dehydrated data (queryStream + deferred promises) into the HTML, and
/// buffering withholds the whole document until the SSR stream closes, so the
/// client's hydration never sees it progressively. Vite pipes the Response body
/// through (Readable.fromWeb); this is the same.
pub async fn proxy_to_loopback(
    port: u16,
    method: &str,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Option<Vec<u8>>,
) -> Result<Response, String> {
    proxy_to_loopback_streaming(port, method, path_and_query, headers, body.map(Body::from)).await
}

/// The headers a request forwarded to a loopback service (the plugin middleware
/// server, the Start SSR runner) carries. hyper writes the loopback `Host`
/// itself, so the browser's `Host` travels as `x-oj-host` and the service
/// rebuilds `Host` from it. Only the first `Host` is taken: Node discards
/// duplicate `Host` headers and keeps the first, so a joined value would never
/// reach an app under Vite. A proxy's own `x-forwarded-host` passes through
/// untouched, as under Vite where the app reads it next to the dev server's
/// `Host`; sending it as `x-forwarded-host` too made Node join the two into
/// `proxy-host, localhost:port`, which no URL parser accepts. An incoming
/// `x-oj-host` is dropped so a client cannot spoof it.
pub fn loopback_request_headers(
    headers: &HeaderMap,
) -> Vec<(header::HeaderName, header::HeaderValue)> {
    let mut out = Vec::with_capacity(headers.len() + 1);
    if let Some(host) = headers.get(header::HOST) {
        out.push((header::HeaderName::from_static("x-oj-host"), host.clone()));
    }
    for (name, value) in headers.iter() {
        if name == header::HOST || name.as_str() == "x-oj-host" {
            continue;
        }
        out.push((name.clone(), value.clone()));
    }
    out
}

/// `proxy_to_loopback` with the request body streamed through as it arrives
/// (Vite pipes `req` into the app; an upload is never held whole in memory).
pub async fn proxy_to_loopback_streaming(
    port: u16,
    method: &str,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Option<Body>,
) -> Result<Response, String> {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    let client = CLIENT.get_or_init(reqwest::Client::new);
    let target = format!("http://127.0.0.1:{port}{path_and_query}");
    let m = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
    let mut out = client.request(m, &target);
    for (name, value) in loopback_request_headers(headers) {
        out = out.header(name, value);
    }
    if let Some(b) = body {
        out = out.body(reqwest::Body::wrap_stream(b.into_data_stream()));
    }
    let resp = out.send().await.map_err(|e| e.to_string())?;
    let status = resp.status();
    let resp_headers = resp.headers().clone();
    let mut response = Response::new(Body::from_stream(resp.bytes_stream()));
    *response.status_mut() = status;
    for (name, value) in resp_headers.iter() {
        if name == header::TRANSFER_ENCODING || name == header::CONTENT_LENGTH {
            continue;
        }
        response.headers_mut().append(name, value.clone());
    }
    Ok(response)
}

pub async fn forward_to_plugin_mw(
    port: u16,
    method: &str,
    path_and_query: &str,
    headers: &HeaderMap,
    body: Option<Vec<u8>>,
) -> Option<Response> {
    let response = proxy_to_loopback(port, method, path_and_query, headers, body)
        .await
        .ok()?;
    if response.headers().contains_key("x-oj-fallthrough") {
        return None;
    }
    Some(response)
}

pub async fn forward_get_to_plugin_mw(
    port: u16,
    path_and_query: &str,
    headers: &HeaderMap,
) -> Option<Response> {
    forward_to_plugin_mw(port, "GET", path_and_query, headers, None).await
}

async fn serve_path(
    State(state): State<Arc<ServerState>>,
    headers: HeaderMap,
    uri: Uri,
) -> Response {
    let path = state
        .base
        .as_deref()
        .and_then(|b| uri.path().strip_prefix(b.trim_end_matches('/')))
        .unwrap_or_else(|| uri.path());
    let decoded = urldecode(path.trim_start_matches('/'));
    let rel = if decoded.is_empty() {
        "index.html"
    } else {
        decoded.as_str()
    };

    if let Some(name) = uri.path().strip_prefix("/@oj-deps/") {
        if name.contains('/') || name.contains("..") {
            return (StatusCode::FORBIDDEN, "oj: bad optimized dep path").into_response();
        }
        state.optimized.ready().await;
        return match tokio::fs::read(state.optimized.dir().join(name)).await {
            Ok(bytes) => dep_response(&headers, has_version_query(uri.query()), bytes),
            Err(_) => (
                StatusCode::NOT_FOUND,
                format!("oj: no optimized dep {name}"),
            )
                .into_response(),
        };
    }

    if let Some(hex) = uri.path().strip_prefix(OPTIONAL_PEER_PREFIX) {
        return match optional_peer_dep_stub(hex) {
            Some(code) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                code,
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, "oj: bad optional peer id").into_response(),
        };
    }

    if uri.path() == "/@oj-empty" {
        return (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            "// oj: browser-externalized module (package maps it to false)\nexport default {};\nexport const __cjs_exports = {};\n",
        )
            .into_response();
    }

    if uri.path().starts_with(pkg_bundle::PKG_PREFIX) {
        return serve_pkg_bundle(&state, uri.path(), has_version_query(uri.query())).await;
    }

    if let Some(id) = uri.path().strip_prefix("/@virtual/") {
        return match state.virtual_modules.get(id) {
            Some(code) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                code.clone(),
            )
                .into_response(),
            None => (StatusCode::NOT_FOUND, format!("oj: no virtual module {id}")).into_response(),
        };
    }

    if let Some(hex) = uri.path().strip_prefix("/@presolve/") {
        let id = hex_decode(hex).unwrap_or_default();
        return serve_plugin_resolve(&state, &id).await;
    }

    if let Some(seg) = uri.path().strip_prefix("/@id/") {
        let spec = decode_at_id(seg);
        let importer = uri
            .query()
            .and_then(|q| q.strip_prefix("importer="))
            .map(decode_at_id)
            .unwrap_or_default();
        return serve_plugin_id(&state, &spec, &importer).await;
    }

    let file = if let Some(abs) = uri.path().strip_prefix("/@fs") {
        match fs_gate(&state, &PathBuf::from(urldecode(abs))) {
            Some(real) => real,
            None => {
                return (StatusCode::FORBIDDEN, "oj: /@fs path not allow-listed").into_response();
            }
        }
    } else {
        match locate(&state.root, state.public_dir.as_deref(), rel) {
            Some(file) => file,
            None => {
                if let Some(resp) =
                    forward_to_plugin_middleware(&state, &Method::GET, &uri, &headers, Vec::new())
                        .await
                {
                    return resp;
                }
                if let Some(resp) = serve_plugin_load_fallback(&state, &uri).await {
                    return resp;
                }
                // Vite's htmlFallback: `/dir/` serves `dir/index.html` and `/page`
                // serves `page.html` when they exist; only appType `spa` then
                // falls back to the root index.html, and `custom` serves no html
                // of its own.
                if state.app_type != "custom" && accepts_html_fallback(&headers) {
                    if let Some(page) = html_fallback_candidate(rel)
                        .and_then(|c| locate(&state.root, state.public_dir.as_deref(), &c))
                    {
                        return match tokio::fs::read(&page).await {
                            Ok(bytes) => serve_html(&state, bytes, &format!("/{rel}"), &page).await,
                            Err(_) => (StatusCode::NOT_FOUND, format!("oj: no such file: /{rel}"))
                                .into_response(),
                        };
                    }
                }
                if state.app_type == "spa" && is_spa_navigation(rel, &headers) {
                    return serve_index_html(&state).await;
                }
                return (StatusCode::NOT_FOUND, format!("oj: no such file: /{rel}"))
                    .into_response();
            }
        }
    };

    if path_is_denied(&file, &state.root, &state.fs_deny) {
        return (StatusCode::FORBIDDEN, "oj: path denied by server.fs.deny").into_response();
    }

    let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
    // A publicDir file is served verbatim, never through the compile pipeline
    // (Vite's servePublicMiddleware runs before transform): a public service
    // worker or vendored script keeps its bytes. Only an explicit asset query
    // (`?url`, `?raw`, `?inline`) still yields a module.
    let in_public = state
        .public_dir
        .as_deref()
        .is_some_and(|p| file.starts_with(p));
    if let Some(kind) = query_asset_kind(uri.query()) {
        // A public file's url is its path under the public dir (`/logo.svg`,
        // Vite's checkPublicFile), not `/public/logo.svg`.
        let url = match state.public_dir.as_deref().filter(|_| in_public) {
            Some(p) => format!("/{}", file.strip_prefix(p).unwrap_or(&file).display()),
            None => url_of(&state.root, &file),
        };
        let js = if kind == "inline" && is_style_ext(ext) {
            inline_css_module(&state, &file, &url).await
        } else {
            asset_module(&file, &url, kind).await
        };
        return match js {
            Ok(js) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                js,
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
        };
    }
    if is_style_ext(ext) && !in_public {
        let url = url_of(&state.root, &file);
        let q = uri.query();
        let direct = q.is_some_and(|q| q.split('&').any(|kv| kv == "direct"));
        let import_query = q.is_some_and(|q| q.split('&').any(|kv| kv == "import"));
        if direct || (wants_raw_resource(&headers) && !import_query) {
            // `<link href>`, `fetch()` of a `?url` stylesheet, or `?direct`: the
            // compiled CSS text (Sass/Less/PostCSS/Tailwind applied), not the
            // preprocessor source and not the JS wrapper.
            return serve_css_direct(&state, &file, &url).await;
        }
        return serve_css_wrapper(&state, &file, &url).await;
    }
    if in_public {
        return serve_static_file(&file, ext).await;
    }
    if COMPILABLE.contains(&ext) {
        let url = url_of(&state.root, &file);
        return serve_compiled(&state, &file, &url, uri.query(), &headers).await;
    }
    if ext == "svg"
        && uri
            .query()
            .is_some_and(|q| q.split('&').any(|kv| kv == "react"))
    {
        let url = format!("{}?react", url_of(&state.root, &file));
        return serve_compiled(&state, &file, &url, None, &headers).await;
    }
    // A plain `.svg` import (no `?react`, no `?url`) goes through the compile path so
    // a configured `vite-plugin-svgr` can turn it into a React component (this app
    // sets svgrOptions.exportType "default" + an include list, so every matching svg
    // is imported as `import Icon from "./x.svg"`). serve_compiled runs the svgr
    // transform; svgs the plugin does not match fall back to a URL asset there. A raw
    // browser request (the Accept header wants the image) still serves the file.
    if ext.eq_ignore_ascii_case("svg")
        && query_asset_kind(uri.query()).is_none()
        && !wants_raw_resource(&headers)
    {
        let url = url_of(&state.root, &file);
        if state.plugins_have_transform {
            return serve_compiled(&state, &file, &url, uri.query(), &headers).await;
        }
        // No plugin can componentize it: a module import of an svg (relative,
        // aliased or root-absolute) is its URL, like any other asset in Vite.
        return match asset_module(&file, &url, "url").await {
            Ok(js) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                js,
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
        };
    }
    if ext == "json" {
        let url = url_of(&state.root, &file);
        return serve_compiled(&state, &file, &url, uri.query(), &headers).await;
    }
    if is_importable_asset_ext(ext)
        && query_asset_kind(uri.query()).is_none()
        && wants_module_import(&headers, uri.query())
    {
        let url = url_of(&state.root, &file);
        return match asset_module(&file, &url, "url").await {
            Ok(js) => (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                js,
            )
                .into_response(),
            Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
        };
    }

    match tokio::fs::read(&file).await {
        Ok(bytes) if ext == "html" => {
            serve_html(&state, bytes, &url_of(&state.root, &file), &file).await
        }
        Ok(bytes) if ext == "css" => {
            let source = String::from_utf8_lossy(&bytes).into_owned();
            if is_tailwind_css(&source) {
                let url = url_of(&state.root, &file);
                return match compile_tailwind(&state, &url, &source).await {
                    Ok(css) => (
                        [
                            (header::CONTENT_TYPE, "text/css"),
                            (header::CACHE_CONTROL, "no-cache"),
                        ],
                        css,
                    )
                        .into_response(),
                    Err(err) => {
                        send_error(&state, &err);
                        (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response()
                    }
                };
            }
            ([(header::CONTENT_TYPE, "text/css")], source).into_response()
        }
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type(ext).parse().unwrap());
            response
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj: read error: {err}"),
        )
            .into_response(),
    }
}

/// The file's bytes as-is with its content type (publicDir files).
async fn serve_static_file(file: &Path, ext: &str) -> Response {
    match tokio::fs::read(file).await {
        Ok(bytes) => {
            let mut response = Response::new(Body::from(bytes));
            response
                .headers_mut()
                .insert(header::CONTENT_TYPE, content_type(ext).parse().unwrap());
            response
        }
        Err(err) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj: read error: {err}"),
        )
            .into_response(),
    }
}

fn inject_dev_scripts(html: String) -> String {
    let tags = "<script type=\"module\" src=\"/@oj/refresh-preamble.js\"></script>\n\
                <script type=\"module\" src=\"/@oj/client.js\"></script>";
    match html.find("<head>") {
        Some(idx) => {
            let insert_at = idx + "<head>".len();
            format!("{}\n{}{}", &html[..insert_at], tags, &html[insert_at..])
        }
        None => format!("{tags}\n{html}"),
    }
}

async fn serve_compiled(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
    query: Option<&str>,
    headers: &HeaderMap,
) -> Response {
    // Key and transform per full url incl. query: the same file yields different
    // modules per query (TanStack router `?tsr-shared`/`?tsr-split` variants), so
    // the query must reach ensure_module's cache key and the plugin transform id.
    // The HMR cache-buster `t=<timestamp>` is not part of the module's identity,
    // though: it is stripped so a re-fetched module keys (and registers in the
    // graph) as itself, and freshness comes from the graph's HMR stamps instead.
    let base_url = url;
    let url_with_query = match query {
        Some(q) => strip_hmr_timestamp(&format!("{url}?{q}")),
        None => url.to_string(),
    };
    let url = url_with_query.as_str();
    let (key, module) = match ensure_module(state, file, url).await {
        Ok(pair) => pair,
        Err(err) => {
            send_error(state, &err);
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response();
        }
    };

    let etag = format!("\"{key}\"");
    if query.is_none() {
        if let Some(inm) = headers
            .get(header::IF_NONE_MATCH)
            .and_then(|v| v.to_str().ok())
        {
            if inm == etag {
                return (
                    StatusCode::NOT_MODIFIED,
                    [
                        (header::ETAG, etag),
                        (header::CACHE_CONTROL, "no-cache".to_string()),
                    ],
                )
                    .into_response();
            }
        }
    }

    let mut body = if module.kind == "svelte" {
        format!("{}{}", svelte_hot_glue(url), module.code)
    } else {
        module.code.clone()
    };
    if module.kind != "svelte" {
        let ctx_predefined = module.hot.is_some();
        if ctx_predefined {
            // The module reads import.meta.hot itself: define the context before
            // its body runs. The refresh glue below then REUSES it (its own
            // import would re-declare `__oj_createHotContext` — a SyntaxError
            // that kills the module).
            let full = match query {
                Some(q) if !q.is_empty() => format!("{base_url}?{q}"),
                _ => base_url.to_string(),
            };
            body = format!("{}{}", svelte_hot_glue(&strip_hmr_timestamp(&full)), body);
        }
        body.push_str(&hot_glue(
            base_url,
            query,
            module.is_boundary,
            ctx_predefined,
        ));
    }
    if let Some(map_url) = &module.map_data_url {
        body.push_str(&format!("\n//# sourceMappingURL={map_url}\n"));
    }

    (
        [
            (header::CONTENT_TYPE, "text/javascript".to_string()),
            (header::CACHE_CONTROL, "no-cache".to_string()),
            (header::ETAG, etag),
        ],
        body,
    )
        .into_response()
}

async fn ensure_module(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
) -> Result<(String, Arc<CachedModule>), String> {
    let react_svg = file.extension().and_then(|e| e.to_str()) == Some("svg")
        && url
            .split_once('?')
            .is_some_and(|(_, q)| q.split('&').any(|kv| kv == "react"));
    let is_svelte = file.extension().and_then(|e| e.to_str()) == Some("svelte");
    // A plain `.svg` (no explicit `?url`/`?raw`) reaches here when a transform
    // plugin might componentize it (vite-plugin-svgr). Route it through the transform
    // pipeline instead of short-circuiting to a URL asset; the svgr transform decides
    // per its own include filter, and an svg it does not match falls back to a URL
    // asset after the transform runs.
    let svgr_candidate = !react_svg
        && state.plugins_have_transform
        && file.extension().and_then(|e| e.to_str()) == Some("svg")
        && query_asset_kind(url.split_once('?').map(|(_, q)| q)).is_none();

    if !react_svg && !svgr_candidate && is_asset_path(file) {
        let clean = url.split('?').next().unwrap_or(url);
        let default = format!(
            "export default {};\n",
            serde_json::Value::String(clean.to_string())
        );
        let module = Arc::new(CachedModule {
            is_boundary: false,
            hot: None,
            kind: String::new(),
            code: default,
            map_data_url: None,
            imports: Vec::new(),
            require_map: Vec::new(),
            css_exports: Vec::new(),
            fs_allow: Vec::new(),
            watch_files: Vec::new(),
        });
        register_in_graph(state, url, &module);
        return Ok((String::new(), module));
    }

    let stamp = match tokio::fs::metadata(file).await {
        Ok(meta) => meta.modified().ok().map(|mtime| (mtime, meta.len())),
        Err(_) => None,
    };
    if let Some((mtime, size)) = stamp {
        let cached_key = state
            .mtime_keys
            .lock()
            .unwrap()
            .get(url)
            .filter(|(t, s, _)| *t == mtime && *s == size)
            .map(|(_, _, k)| k.clone());
        if let Some(key) = cached_key {
            if let Some(module) = memory_get(state, url, &key) {
                register_in_graph(state, url, &module);
                return Ok((key, module));
            }
        }
    }

    let is_dep_early = is_dep_module(url, file);

    // Vite runs plugin `load` hooks before the filesystem read (its fs read is the
    // last-resort `vite:load-fallback` plugin), so a plugin can replace an on-disk
    // file's contents. The i18n-dev plugin relies on this: its `load` collapses the
    // generated 8k-line message barrel (`_index.js`) into a handful of grouped
    // virtual modules, so the browser fetches a few groups instead of thousands of
    // individual re-exported files. oj mirrors the ordering here: give a matching
    // plugin `load` the first say, fall back to the disk read when none loads. It is
    // gated to app source (deps in node_modules never need it) and reached only on a
    // cold module (the mtime cache above short-circuits warm ones), so it adds no
    // per-request RPC on the warm path, and nothing at all when no plugin has `load`.
    // An `optimizeDeps.exclude`d package is one Vite never pre-bundles, so its
    // files go through every plugin's `load`/`transform` there like app source;
    // the other deps stand in for Vite's pre-bundled ones, which no plugin sees.
    let dep_wants_load = is_dep_early
        && (pkg_bundle::is_excluded(file) || {
            let path = file.to_string_lossy();
            state.dep_load_res.iter().any(|re| re.is_match(&path))
        });
    let plugin_loaded = if state.plugins_have_load && (!is_dep_early || dep_wants_load) {
        match &state.plugins {
            Some(host) => {
                let load_id = match url.split_once('?') {
                    Some((_, q)) => format!("{}?{}", file.display(), q),
                    None => file.to_string_lossy().into_owned(),
                };
                if !host.hook_wants_load(&load_id) {
                    if plugins::hook_gate_debug() {
                        eprintln!("oj: hook gate skipped load for {load_id}");
                    }
                    None
                } else {
                    // A throwing `load` fails the module like Vite (500 + overlay),
                    // rather than silently reading the disk file it meant to replace.
                    host.load(&load_id)
                        .await
                        .map_err(|e| format!("plugin load error for {url}:\n{e}"))?
                }
            }
            None => None,
        }
    } else {
        None
    };
    let source = match plugin_loaded {
        Some(code) => code,
        None => bytes_to_string(
            tokio::fs::read(file)
                .await
                .map_err(|err| format!("read error for {url}: {err}"))?,
        )
        .map_err(|err| format!("read error for {url}: {err}"))?,
    };
    if source.contains("import.meta.glob") {
        let patterns: Vec<glob::Pattern> = oj_compiler::glob::glob_patterns(&source, file)
            .iter()
            .filter_map(|p| glob::Pattern::new(p).ok())
            .collect();
        let clean = url.split('?').next().unwrap_or(url).to_string();
        let mut globs = state.glob_importers.lock().unwrap();
        if patterns.is_empty() {
            globs.remove(&clean);
        } else {
            globs.insert(clean, patterns);
        }
    }
    if file.extension().and_then(|e| e.to_str()) == Some("css") && is_tailwind_css(&source) {
        let css = compile_tailwind(state, url, &source).await?;
        let module = Arc::new(CachedModule {
            is_boundary: true,
            hot: None,
            kind: "css".into(),
            code: css,
            map_data_url: None,
            imports: Vec::new(),
            require_map: Vec::new(),
            css_exports: Vec::new(),
            fs_allow: Vec::new(),
            watch_files: Vec::new(),
        });
        register_in_graph(state, url, &module);
        return Ok((String::new(), module));
    }

    let is_server = is_server_module(file) && !is_dep_early;

    let mode = if is_server { "server" } else { "dev" };
    // Fold the newest HMR stamp among this module's imports into the key: after a
    // dependency updates, the (unchanged) importer must recompile so its import of
    // that dependency carries the new `?t=`, or the browser keeps the stale one.
    let imports_stamp = state
        .graph
        .lock()
        .unwrap()
        .imports_timestamp(Path::new(url));
    let mode_key = if imports_stamp > 0 {
        format!("{mode}@{imports_stamp}")
    } else {
        mode.to_string()
    };
    let key = state.cache.key(source.as_bytes(), url, &mode_key);
    if let Some((mtime, size)) = stamp {
        state
            .mtime_keys
            .lock()
            .unwrap()
            .insert(url.to_string(), (mtime, size, key.clone()));
    }

    if let Some(module) = memory_get(state, url, &key) {
        register_in_graph(state, url, &module);
        return Ok((key, module));
    }

    let lock = {
        let mut locks = state.compile_locks.lock().unwrap();
        Arc::clone(locks.entry(url.to_string()).or_default())
    };
    let _guard = lock.lock().await;

    if let Some(module) = memory_get(state, url, &key) {
        register_in_graph(state, url, &module);
        return Ok((key, module));
    }
    // The persistent (cross-restart) cache holds post-plugin-transform code. A
    // transform can append an import to a plugin-served *virtual* whose content
    // lives only in the plugin's in-memory state: wyw-in-js records each module's
    // extracted CSS in a `cssLookup` its `load` hook serves, and the cached code
    // still `import`s that `.wyw-in-js.css` id. On a warm start the transform
    // never re-runs, so that map is empty and the import 404s. Detect it
    // precisely — a cached module whose imports include a filesystem path with no
    // file on disk depends on such a virtual — and re-run the transform for just
    // those. Modules whose imports are all real files (svgr on disk, plain
    // source, deps) keep the fast persistent cache (Vite has no cross-restart
    // transform cache at all; this preserves oj's where it is sound).
    if let Some(module) = state
        .persistent_cache
        .then(|| state.cache.get(&key))
        .flatten()
    {
        let module = Arc::new(module);
        let needs_retransform = state.plugins_have_transform
            && !is_dep_early
            && imports_a_plugin_virtual(&module.imports, &state.root, &state.dir_cache);
        if !needs_retransform {
            memory_put(state, url, &key, &module);
            register_in_graph(state, url, &module);
            replay_module_parsed(state, file, &key, is_dep_early, is_server).await;
            return Ok((key, module));
        }
    }

    if is_server {
        let code = server_fn_stub(&oj_compiler::exports(&source, file), url);
        let module = Arc::new(CachedModule {
            is_boundary: false,
            hot: None,
            kind: String::new(),
            code,
            map_data_url: None,
            imports: Vec::new(),
            require_map: Vec::new(),
            css_exports: Vec::new(),
            fs_allow: Vec::new(),
            watch_files: Vec::new(),
        });
        if state.persistent_cache {
            let _ = state
                .cache_writes
                .try_send((key.clone(), Arc::clone(&module)));
        }
        memory_put(state, url, &key, &module);
        register_in_graph(state, url, &module);
        return Ok((key, module));
    }

    let is_dep = is_dep_module(url, file);
    let mut plugin_watch_files: Vec<String> = Vec::new();
    let mut plugin_maps: Vec<String> = Vec::new();
    let dep_wants_transform = is_dep
        && (pkg_bundle::is_excluded(file)
            || state
                .dep_transform_res
                .iter()
                .any(|re| re.is_match(&source)));
    let source = match &state.plugins {
        Some(host) if state.plugins_have_transform && (!is_dep || dep_wants_transform) => {
            // Pass the id WITH its query (e.g. `?tsr-shared=1`), like Vite: the router
            // code-splitter emits a different variant per query, keyed off the id.
            let transform_id = match url.split_once('?') {
                Some((_, q)) => format!("{}?{}", file.display(), q),
                None => file.to_string_lossy().into_owned(),
            };
            if !host.hook_wants_transform(&transform_id, &source) {
                if plugins::hook_gate_debug() {
                    eprintln!("oj: hook gate skipped transform for {transform_id}");
                }
                source
            } else {
                let resolved =
                    resolved_imports_json(&state.resolver, &state.fs_allow, &source, file);
                match host.transform(&source, &transform_id, &resolved).await {
                    Ok((code, watches, maps, _)) => {
                        plugin_watch_files = watches;
                        plugin_maps = maps;
                        code
                    }
                    // Vite fails the request with the plugin's error (code frame in the
                    // overlay); serving the untransformed source would ship wrong code.
                    Err(e) => {
                        return Err(format!(
                            "plugin transform error for {}:\n{e}",
                            file.display()
                        ));
                    }
                }
            }
        }
        _ => source,
    };

    let source = if is_preprocessor(url) {
        // css.preprocessorOptions.<less|stylus>: `additionalData` is prepended,
        // everything else goes to the preprocessor as its options (Vite parity).
        let lang = if sidecar::is_less(url) {
            "less"
        } else {
            "stylus"
        };
        let cfg = state.css_config.clone().map(|c| oj_config::OjConfig {
            css: Some(c),
            ..Default::default()
        });
        let (data, opts) = match &cfg {
            Some(c) => (
                oj_config::css_additional_data(c, lang),
                oj_config::css_preprocessor_json(c, lang),
            ),
            None => (None, serde_json::Value::Null),
        };
        let with_data = match data {
            Some(d) if !d.is_empty() => format!("{d}\n{source}"),
            _ => source,
        };
        run_preprocess_engine(state, url, &with_data, opts)
            .await
            .map_err(|e| format!("css preprocess error for {url}: {e}"))?
    } else {
        source
    };

    // PostCSS runs on the preprocessor OUTPUT (Vite orders Sass before PostCSS),
    // so a Sass file is compiled here first when a PostCSS config applies; the
    // compile step below then skips Sass for it.
    let mut sass_precompiled = false;
    let source = if state.has_postcss && oj_css::is_sass(url) {
        let data = sass_additional_data_for(state, url);
        let load_paths = sass_load_paths_for(state, url);
        let dir = file.parent().map(Path::to_path_buf);
        let src = source.clone();
        let css_resolve = state.css_resolve.clone();
        let compiled = tokio::task::spawn_blocking(move || {
            oj_css::compile_sass_opts(
                &src,
                &oj_css::SassOptions {
                    load_dir: dir.as_deref(),
                    additional_data: data.as_deref(),
                    load_paths: &load_paths,
                    resolve: css_resolve.as_ref(),
                },
            )
        })
        .await
        .map_err(|e| format!("sass compile task failed for {url}: {e}"))??;
        sass_precompiled = true;
        compiled
    } else {
        source
    };
    let css_like = sass_precompiled
        || is_preprocessor(url)
        || file.extension().and_then(|e| e.to_str()) == Some("css");
    let mut imports_inlined = false;
    let source = if state.has_postcss && css_like {
        // postcss-import is the first plugin of Vite's PostCSS chain, so the
        // rules of an @imported stylesheet go through the user's plugins too:
        // inline before the PostCSS pass, not after it.
        let source = oj_css::inline_imports_with(&source, file, &state.css_resolve.as_ref())?;
        imports_inlined = true;
        match run_css_engine(state, url, &source).await {
            Ok(out) => out,
            Err(e) => {
                eprintln!("oj: postcss failed for {url}: {e}");
                source
            }
        }
    } else {
        source
    };

    let source = if react_svg {
        svgr::svg_to_component(&source)
    } else {
        source
    };
    // The svgr plugin transform (if any) has run by now. If a plain `.svg` candidate
    // is still raw markup, the plugin did not match it (not in its include list), so
    // serve it as a URL asset like Vite; otherwise it is now component code compiled
    // as `.svg.tsx` below.
    let svgr_componentized = svgr_candidate && !source.trim_start().starts_with('<');
    if svgr_candidate && !svgr_componentized {
        let clean = url.split('?').next().unwrap_or(url);
        let module = Arc::new(CachedModule {
            is_boundary: false,
            hot: None,
            kind: String::new(),
            code: format!(
                "export default {};\n",
                serde_json::Value::String(clean.to_string())
            ),
            map_data_url: None,
            imports: Vec::new(),
            require_map: Vec::new(),
            css_exports: Vec::new(),
            fs_allow: Vec::new(),
            watch_files: Vec::new(),
        });
        register_in_graph(state, url, &module);
        return Ok((String::new(), module));
    }
    let source = if is_svelte {
        run_svelte_engine(state, url, &source)
            .await
            .map_err(|e| format!("svelte compile error for {url}: {e}"))?
    } else {
        source
    };

    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let require_resolver = Arc::clone(&state.require_resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir_cache = Arc::clone(&state.dir_cache);
    let virtual_ids: std::collections::BTreeSet<String> =
        state.virtual_modules.keys().cloned().collect();
    let jsx_overrides = state.jsx_overrides.clone();
    let jsx_config = state.jsx.clone();
    let dir = file.parent().map(Path::to_path_buf).unwrap_or_default();
    let file_owned = if react_svg || svgr_componentized {
        file.with_extension("svg.tsx")
    } else if is_svelte {
        file.with_extension("svelte.js")
    } else {
        file.to_path_buf()
    };
    let url_owned = url.to_string();
    let sass_data = sass_additional_data_for(state, &url_owned);
    let sass_load_paths = sass_load_paths_for(state, &url_owned);
    let css_resolve = state.css_resolve.clone();
    let css_dev_sourcemap = state
        .css_config
        .as_ref()
        .and_then(|c| c.dev_sourcemap)
        .unwrap_or(false);
    let hmr_state = Arc::clone(state);
    let plugin_fallback = state.plugins.is_some();
    let svgr_active = state.plugins_have_transform;
    let resolve_id_res = if plugin_fallback {
        state.resolve_id_res.clone()
    } else {
        Vec::new()
    };
    let importer_abs = file.to_string_lossy().into_owned();
    let ext = file.extension().and_then(|e| e.to_str());
    let is_css = ext.is_some_and(is_style_ext);
    let is_json = ext == Some("json");
    let dep_map = if is_css || is_json {
        Arc::new(optimize::DepMap::new())
    } else {
        state.optimized.ready().await
    };
    let compiled = tokio::task::spawn_blocking(move || -> Result<CachedModule, String> {
        if is_json {
            let code = oj_compiler::json::to_esm(&source, &url_owned)
                .map_err(|err| format!("compile error:\n{err}"))?;
            return Ok(CachedModule {
                is_boundary: false,
                hot: None,
                kind: String::new(),
                code,
                map_data_url: None,
                imports: Vec::new(),
                require_map: Vec::new(),
                css_exports: Vec::new(),
                fs_allow: Vec::new(),
                watch_files: Vec::new(),
            });
        }
        if is_css {
            let resolve = css_resolve.as_ref();
            let css_src = if oj_css::is_sass(&url_owned) && !sass_precompiled {
                oj_css::compile_sass_opts(
                    &source,
                    &oj_css::SassOptions {
                        load_dir: Some(&dir),
                        additional_data: sass_data.as_deref(),
                        load_paths: &sass_load_paths,
                        resolve,
                    },
                )?
            } else {
                source.clone()
            };
            // Plain `@import`s are inlined (postcss-import parity) so the injected
            // stylesheet does not @import a bare specifier or a wrong-relative url.
            let css_src = if imports_inlined {
                css_src
            } else {
                oj_css::inline_imports_with(&css_src, &file_owned, &resolve)?
            };
            let output =
                oj_css::compile_css_dev(&url_owned, &css_src, css_dev_sourcemap, &resolve)?;
            // A CSS module exports its class map, which changes on edit, so it
            // cannot self-accept (Vite's css-analysis): the update climbs to the
            // importing component, whose re-import fetches the new exports.
            let is_css_module = output.exports.is_some();
            return Ok(CachedModule {
                is_boundary: !is_css_module,
                hot: None,
                kind: "css".into(),
                code: output.css,
                map_data_url: None,
                imports: Vec::new(),
                require_map: Vec::new(),
                css_exports: output.exports.unwrap_or_default(),
                fs_allow: Vec::new(),
                watch_files: Vec::new(),
            });
        }
        // The first relative import nothing on disk satisfies. Vite's import
        // analysis fails the transform for it ("Failed to resolve import ...");
        // shipping the specifier unchanged would only surface as a 404 in the
        // browser, with no overlay and no recovery when the file is created.
        let unresolved: std::cell::RefCell<Option<String>> = std::cell::RefCell::new(None);
        let rewrite_with = |spec: &str, resolver: &OjResolver| {
            if spec == "virtual:oj-routes" {
                return Some("/@oj/routes.js".to_string());
            }
            if virtual_ids.contains(spec) {
                return Some(format!("/@virtual/{spec}"));
            }
            // Vite runs the plugins' resolveId before its own resolver for every
            // import. A relative / absolute import a plugin's resolveId filter
            // claims goes to the plugins first (`./icon.svg?react` remaps); the
            // /@id/ route falls back to the disk resolver when they decline.
            if !is_bare_specifier(spec) && resolve_id_res.iter().any(|re| re.is_match(spec)) {
                return Some(format!(
                    "/@id/{}?importer={}",
                    hex_encode(spec),
                    hex_encode(&importer_abs)
                ));
            }
            if let Some(id) = jsx_overrides.get(spec) {
                return Some(format!("/@presolve/{}", hex_encode(id)));
            }
            if let Some(meta) = dep_map.get(spec) {
                if !meta.needs_interop {
                    return Some(meta.url.clone());
                }
            }
            if let Some(url) =
                rewrite_specifier(&root, &dir, resolver, &fs_allow, &dir_cache, spec, true)
            {
                // `.svg` resolves to `<url>?url` (asset). When a transform plugin is
                // active (vite-plugin-svgr), leave the svg unmarked instead so it
                // routes through the compile path and svgr can componentize it, as
                // Vite does (it marks svg imports `?import`, not `?url`); an svg svgr
                // does not match falls back to a URL asset there.
                if svgr_active {
                    if let Some(base) = url.strip_suffix(".svg?url") {
                        return Some(format!("{base}.svg"));
                    }
                }
                // Vite's importAnalysis appends `?t=<lastHMRTimestamp>` to an import
                // of a module an HMR update invalidated, so the re-fetched importer
                // loads the dependency's new version instead of the browser's cached
                // instance (only boundaries are named in the update itself).
                let stamp = hmr_state
                    .graph
                    .lock()
                    .unwrap()
                    .hmr_timestamp(Path::new(url.split('?').next().unwrap_or(&url)));
                return Some(stamp_import_url(&url, stamp));
            }
            if plugin_fallback && is_bare_specifier(spec) {
                return Some(format!(
                    "/@id/{}?importer={}",
                    hex_encode(spec),
                    hex_encode(&importer_abs)
                ));
            }
            // A bare specifier no plugin can claim (there is no plugin fallback
            // here) fails the same way: Vite's importAnalysis errors for it
            // instead of shipping the bare name for the browser to reject. SSR
            // keeps Vite's `if (ssr) return [url, null]`: Node reports it.
            if !is_dep
                && !is_server
                && (relative_import_missing(&dir, resolver, spec)
                    || bare_import_unresolved(&dir, resolver, spec))
            {
                unresolved
                    .borrow_mut()
                    .get_or_insert_with(|| spec.to_string());
            }
            None
        };
        let mut rewrite = |spec: &str| rewrite_with(spec, &resolver);
        // The import specifiers whose named imports must read off the CJS
        // value (module.exports) instead of linking as ESM bindings, shared
        // by app source and served ESM deps (an ESM dep importing an
        // unbundled UMD sibling, the proj4 -> geographiclib-geodesic shape,
        // strict-links the same way app source does).
        let cjs_interop_url = |spec: &str| {
            // node builtins are browser-externalized to a stub with no
            // named exports; interop so `import { X } from "node:..."`
            // reads X off it (undefined) instead of failing to link.
            if is_node_builtin(spec) {
                return Some(format!("/@id/{}", hex_encode(spec)));
            }
            // lingui macro entrypoints go to the shim (which has real
            // named exports), never through default-access interop.
            if is_lingui_macro_specifier(spec) {
                return None;
            }
            if let Some(m) = dep_map.get(spec).filter(|m| m.needs_interop) {
                return Some(m.url.clone());
            }
            // A directly-served bare CJS dep (not pre-bundled): rewrite
            // `import { x } from "dep"` to read x off the default export,
            // so runtime-assigned CJS exports resolve. Vite pre-bundles
            // these; oj interops at the importer instead. Restricted to
            // node_modules so aliased app source (`~/x`, `@/x`, which
            // is_bare_specifier also matches) is never treated as a dep.
            if is_bare_specifier(spec) && dep_map.get(spec).is_none() {
                if let Ok(resolved) = resolver.resolve(&dir, spec) {
                    let in_node_modules = resolved
                        .components()
                        .any(|c| c.as_os_str() == "node_modules");
                    // optimizeDeps.needsInterop forces the interop
                    // rewrite even when static analysis reads the dep
                    // as ESM (its real exports only appear at runtime).
                    if in_node_modules
                        && (is_cjs_dep_file(&resolved)
                            || pkg_bundle::needs_forced_interop(&resolved))
                    {
                        fs_allow.lock().unwrap().insert(package_root(&resolved));
                        // With partial bundling on this is the /@oj-pkg
                        // bundle URL, which exports __cjs_exports too, so
                        // the destructured interop still reads names off it.
                        return Some(dep_serve_url(&resolved, &root));
                    }
                }
            }
            None
        };
        let output = if is_dep {
            if oj_compiler::cjs::has_module_syntax_pub(&file_owned, &source) {
                // Gated on a cheap bare-import scan: deps overwhelmingly
                // import their own relative files, and the rewrite would
                // otherwise add a second full parse to every served ESM dep.
                // The scan finds the bare specifiers without a parse; the
                // rewrite (a full parse) runs only when one of them actually
                // maps to an interop URL, so a dep file whose bare imports
                // are all ESM peers (react, tslib) skips it entirely.
                let dep_interop = if oj_compiler::interop::bare_import_specifiers(&source)
                    .iter()
                    .any(|spec| cjs_interop_url(spec).is_some())
                {
                    oj_compiler::interop::rewrite_cjs_interop_logged(
                        &source,
                        &file_owned,
                        &cjs_interop_url,
                        &mut warn_interop_once,
                    )
                } else {
                    None
                };
                let dep_src = dep_interop.as_deref().unwrap_or(&source);
                oj_compiler::cjs::compile_dep(&file_owned, &url_owned, dep_src, &mut rewrite)
            } else {
                let dep_interop = interop_node_builtins(&source, &file_owned);
                let dep_src = dep_interop.as_deref().unwrap_or(&source);
                // A CommonJS dep's `require()`s resolve with the `require`
                // condition (Vite's getConditions for a requirer), so a dual
                // package hands it its CJS build (`module.exports = fn`), not
                // the ESM one the interop would wrap as `{ default: fn }`.
                oj_compiler::cjs::compile_dep(
                    &file_owned,
                    &url_owned,
                    dep_src,
                    &mut |spec: &str| rewrite_with(spec, &require_resolver),
                )
            }
        } else {
            let interopped = oj_compiler::interop::rewrite_cjs_interop_logged(
                &source,
                &file_owned,
                &cjs_interop_url,
                &mut warn_interop_once,
            );
            let mut opts = if is_svelte {
                oj_compiler::CompileOptions {
                    dev: true,
                    refresh: false,
                    sourcemap: true,
                    ssr: false,
                    jsx: oj_compiler::JsxConfig::default(),
                }
            } else {
                oj_compiler::CompileOptions::dev()
            };
            opts.jsx = jsx_config;
            oj_compiler::compile_module_with_maps(
                &file_owned,
                interopped.as_deref().unwrap_or(&source),
                &opts,
                Some(&mut rewrite),
                &plugin_maps,
            )
        }
        .map_err(|err| format!("compile error:\n{err}"))?;
        if let Some(spec) = unresolved.borrow().as_ref() {
            return Err(unresolved_import_error(&root, &file_owned, &source, spec));
        }
        Ok(CachedModule {
            is_boundary: is_svelte || (!is_dep && output.has_refresh_registrations()),
            hot: output.hot_accept.map(|h| oj_cache::HotMeta {
                self_accept: h.self_accepting,
                deps: h.deps,
            }),
            code: output.code,
            map_data_url: output.map_data_url,
            fs_allow: fs_allow_from(&output.imports),
            watch_files: Vec::new(),
            imports: output.imports,
            kind: if is_svelte {
                "svelte".into()
            } else {
                String::new()
            },
            require_map: Vec::new(),
            css_exports: Vec::new(),
        })
    })
    .await;

    let module = match compiled {
        Ok(Ok(mut module)) => {
            module.watch_files = plugin_watch_files;
            Arc::new(module)
        }
        Ok(Err(err)) => {
            if is_unresolved_import_error(&err) {
                let clean = url.split('?').next().unwrap_or(url).to_string();
                state.resolve_failed.lock().unwrap().insert(clean);
                // Vite clears the importer's isSelfAccepting here (#9534) so the
                // update a later `create` triggers climbs to a boundary the page
                // did load rather than stopping at a module it never evaluated.
                state
                    .graph
                    .lock()
                    .unwrap()
                    .set_self_accepting(Path::new(url), false);
            }
            return Err(err);
        }
        Err(join_err) => return Err(format!("compiler task failed: {join_err}")),
    };
    if state.persistent_cache {
        let _ = state
            .cache_writes
            .try_send((key.clone(), Arc::clone(&module)));
    }
    memory_put(state, url, &key, &module);
    register_in_graph(state, url, &module);
    state
        .resolve_failed
        .lock()
        .unwrap()
        .remove(url.split('?').next().unwrap_or(url));
    if state.plugins_use_module_parsed && !is_dep && !is_server {
        state.parsed_fired.lock().unwrap().insert(key.clone());
    }
    Ok((key, module))
}

async fn replay_module_parsed(
    state: &Arc<ServerState>,
    file: &Path,
    key: &str,
    is_dep: bool,
    is_server: bool,
) {
    if !state.plugins_use_module_parsed || is_dep || is_server {
        return;
    }
    if !state.parsed_fired.lock().unwrap().insert(key.to_string()) {
        return;
    }
    if let Some(host) = &state.plugins {
        let _ = host.module_parsed(&file.to_string_lossy()).await;
    }
}

// Rough fixed cost of one cache entry beyond its module payload: the MemoryEntry
// struct, the HashMap bucket, and the String headers for the url + key.
const MEMORY_ENTRY_OVERHEAD: usize = 160;

struct MemoryEntry {
    key: String,
    module: Arc<CachedModule>,
    bytes: usize,
    seq: u64,
}

struct MemoryCache {
    map: HashMap<String, MemoryEntry>,
    total: usize,
    budget: usize,
    seq: u64,
}

impl MemoryCache {
    fn new(budget: usize) -> Self {
        MemoryCache {
            map: HashMap::new(),
            total: 0,
            budget,
            seq: 0,
        }
    }

    fn get(&mut self, url: &str, key: &str) -> Option<Arc<CachedModule>> {
        self.seq += 1;
        let seq = self.seq;
        let entry = self.map.get_mut(url)?;
        if entry.key != key {
            return None;
        }
        entry.seq = seq;
        Some(Arc::clone(&entry.module))
    }

    fn remove(&mut self, url: &str) {
        if let Some(old) = self.map.remove(url) {
            self.total -= old.bytes;
        }
    }

    fn clear(&mut self) {
        self.map.clear();
        self.total = 0;
    }

    fn put(&mut self, url: &str, key: &str, module: &Arc<CachedModule>) {
        let bytes = module_weight(module) + url.len() + key.len() + MEMORY_ENTRY_OVERHEAD;
        self.seq += 1;
        let seq = self.seq;
        let entry = MemoryEntry {
            key: key.to_string(),
            module: Arc::clone(module),
            bytes,
            seq,
        };
        if let Some(old) = self.map.insert(url.to_string(), entry) {
            self.total -= old.bytes;
        }
        self.total += bytes;
        self.evict();
    }

    // Evict in one sorted pass down to a low-water mark (90% of budget) so the
    // hot get()/put() paths stay cheap and eviction runs rarely, not per-put.
    fn evict(&mut self) {
        if self.total <= self.budget {
            return;
        }
        let low = self.budget - self.budget / 10;
        let mut order: Vec<(u64, String)> = self
            .map
            .iter()
            .map(|(url, e)| (e.seq, url.clone()))
            .collect();
        order.sort_unstable_by_key(|(seq, _)| *seq);
        for (_, url) in order {
            if self.total <= low || self.map.len() <= 1 {
                break;
            }
            if let Some(e) = self.map.remove(&url) {
                self.total -= e.bytes;
            }
        }
    }
}

fn module_weight(module: &CachedModule) -> usize {
    fn strs(v: &[String]) -> usize {
        v.iter()
            .map(|s| s.len() + std::mem::size_of::<String>())
            .sum::<usize>()
    }
    fn pairs(v: &[(String, String)]) -> usize {
        v.iter()
            .map(|(a, b)| a.len() + b.len() + 2 * std::mem::size_of::<String>())
            .sum::<usize>()
    }
    module.code.len()
        + module.map_data_url.as_ref().map_or(0, String::len)
        + module.kind.len()
        + strs(&module.imports)
        + strs(&module.fs_allow)
        + strs(&module.watch_files)
        + pairs(&module.require_map)
        + pairs(&module.css_exports)
}

fn memory_cache_budget() -> usize {
    if let Some(mb) = std::env::var("OJ_MEMORY_CACHE_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
    {
        return if mb == 0 {
            usize::MAX
        } else {
            mb.saturating_mul(1024 * 1024)
        };
    }
    // No explicit budget: scale to the container memory limit (density) if one is
    // visible, else a sane fixed default for a dev machine.
    let ceil = 256 * 1024 * 1024;
    let floor = 32 * 1024 * 1024;
    match detect_memory_limit() {
        Some(limit) => (limit / 8).clamp(floor, ceil),
        None => 128 * 1024 * 1024,
    }
}

// The cgroup memory ceiling (v2 then v1); None off-container or when unlimited.
fn detect_memory_limit() -> Option<usize> {
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory.max") {
        let t = s.trim();
        if t != "max" {
            if let Ok(v) = t.parse::<u64>() {
                return Some(v as usize);
            }
        }
    }
    if let Ok(s) = std::fs::read_to_string("/sys/fs/cgroup/memory/memory.limit_in_bytes") {
        if let Ok(v) = s.trim().parse::<u64>() {
            if v < (1u64 << 62) {
                return Some(v as usize);
            }
        }
    }
    None
}

fn memory_get(state: &ServerState, url: &str, key: &str) -> Option<Arc<CachedModule>> {
    state.memory.lock().unwrap().get(url, key)
}

fn memory_put(state: &ServerState, url: &str, key: &str, module: &Arc<CachedModule>) {
    state.memory.lock().unwrap().put(url, key, module);
}

fn package_root(path: &Path) -> PathBuf {
    let mut dir = path.parent();
    while let Some(d) = dir {
        if d.join("package.json").is_file() {
            return d.to_path_buf();
        }
        dir = d.parent();
    }
    path.parent().unwrap_or(path).to_path_buf()
}

fn fs_allow_from(imports: &[String]) -> Vec<String> {
    imports
        .iter()
        .filter_map(|i| i.split('?').next().unwrap_or(i).strip_prefix("/@fs"))
        .map(|p| package_root(Path::new(p)).display().to_string())
        .collect()
}

fn register_in_graph(state: &ServerState, url: &str, module: &CachedModule) {
    if !module.fs_allow.is_empty() {
        let mut allow = state.fs_allow.lock().unwrap();
        for p in &module.fs_allow {
            allow.insert(PathBuf::from(p));
        }
    }
    if !module.watch_files.is_empty() {
        let mut watched = state.plugin_watched.lock().unwrap();
        for p in &module.watch_files {
            watched.insert(PathBuf::from(p));
        }
    }
    let mut graph = state.graph.lock().unwrap();
    let local_imports: Vec<&Path> = module
        .imports
        .iter()
        .filter(|s| s.starts_with('/') && !s.starts_with("/@oj/") && !is_worker_query(s))
        .map(|s| Path::new(s.split('?').next().unwrap_or(s)))
        .collect();
    let pruned = graph.set_imports(Path::new(url), &local_imports);
    if !pruned.is_empty() {
        // Dependencies this module dropped that nothing imports any more: the
        // client runs their `hot.prune` callbacks (a stylesheet removes its
        // <style>), and they are stamped so a later re-import re-runs them, as
        // Vite's handlePrunedModules does after importAnalysis.
        graph.stamp_pruned(&pruned, now_millis() as u64);
        let paths: Vec<String> = pruned.iter().map(|p| p.display().to_string()).collect();
        println!("oj: prune {paths:?}");
        let _ = state
            .reload_tx
            .send(serde_json::json!({ "type": "prune", "paths": paths }).to_string());
    }
    let hot = module.hot.as_ref();
    graph.set_self_accepting(
        Path::new(url),
        module.is_boundary || hot.is_some_and(|h| h.self_accept),
    );
    let accepted: Vec<PathBuf> = hot
        .map(|h| {
            h.deps
                .iter()
                .map(|d| PathBuf::from(d.split('?').next().unwrap_or(d)))
                .collect()
        })
        .unwrap_or_default();
    graph.set_accepted_deps(Path::new(url), &accepted);
}

/// The compiled stylesheet as `text/css` (Vite's `?direct` / raw `<link>` request).
async fn serve_css_direct(state: &Arc<ServerState>, file: &Path, url: &str) -> Response {
    match ensure_module(state, file, url).await {
        Ok((_, module)) => (
            [
                (header::CONTENT_TYPE, "text/css"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            module.code.clone(),
        )
            .into_response(),
        Err(err) => {
            send_error(state, &err);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response()
        }
    }
}

async fn serve_css_wrapper(state: &Arc<ServerState>, file: &Path, url: &str) -> Response {
    let (_, module) = match ensure_module(state, file, url).await {
        Ok(pair) => pair,
        Err(err) => {
            send_error(state, &err);
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response();
        }
    };
    // A CSS module exports its class map as the default plus a named export
    // per identifier-safe class (Vite's dataToEsm with namedExports).
    // A module file compiled unscoped (css.modules global mode) still exports
    // its (empty) class map, as Vite does.
    let exports = if module.css_exports.is_empty() && !oj_css::is_css_module(url) {
        "export default void 0;\n".to_string()
    } else {
        oj_css::css_modules_esm(&module.css_exports)
    };
    // Plain stylesheets self-accept; a CSS module does not (its exports change
    // on edit), so the importing component is the boundary, as in Vite's css
    // plugin (`modulesCode || 'import.meta.hot.accept()'`).
    let accept = if module.is_boundary {
        "import.meta.hot.accept(() => {});\n"
    } else {
        ""
    };
    let body = format!(
        "import {{ createHotContext as __oj_hot, updateStyle as __oj_updateStyle, removeStyle as __oj_removeStyle }} from \"/@oj/client.js\";\n\
         import.meta.hot = __oj_hot({url:?});\n\
         __oj_updateStyle({url:?}, {css});\n\
         {exports}\
         {accept}\
         import.meta.hot.prune(() => __oj_removeStyle({url:?}));\n",
        css = serde_json::Value::String(module.code.clone()),
    );
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

pub fn has_postcss_config(root: &Path) -> bool {
    find_postcss_config(root).is_some()
}

/// The PostCSS config that applies to `root`, found the way postcss-load-config
/// does (what Vite uses): `postcss.config.{js,mjs,cjs,ts,mts,cts}`, `.postcssrc`,
/// `.postcssrc.{json,js,mjs,cjs,ts,mts,cts}` or a `package.json` with a
/// `postcss` key, searched from `root` up to the workspace root (nearest wins).
/// The tailwind engine module receives the path per request.
pub fn find_postcss_config(root: &Path) -> Option<PathBuf> {
    const NAMES: &[&str] = &[
        "postcss.config.js",
        "postcss.config.mjs",
        "postcss.config.cjs",
        "postcss.config.ts",
        "postcss.config.mts",
        "postcss.config.cts",
        ".postcssrc",
        ".postcssrc.json",
        ".postcssrc.js",
        ".postcssrc.mjs",
        ".postcssrc.cjs",
        ".postcssrc.ts",
        ".postcssrc.mts",
        ".postcssrc.cts",
    ];
    let stop = workspace_root(root);
    let mut dir = Some(root);
    while let Some(d) = dir {
        for name in NAMES {
            let p = d.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        let pkg = d.join("package.json");
        if let Ok(text) = std::fs::read_to_string(&pkg) {
            if serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .is_some_and(|v| v.get("postcss").is_some_and(|p| !p.is_null()))
            {
                return Some(pkg);
            }
        }
        if d == stop {
            break;
        }
        dir = d.parent();
    }
    None
}

async fn run_css_engine(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let engine = state
        .tailwind
        .get_or_try_init(|| CssEngine::tailwind(&state.root, css_engine::DEV_DEADLINE))
        .await
        .map_err(|e| e.to_string())?;
    engine.compile(source, url).await
}

fn is_preprocessor(url: &str) -> bool {
    sidecar::is_less(url) || sidecar::is_stylus(url)
}

// Whether a module served from node_modules or /@fs/ is a dependency (routed to
// the dep/CJS-interop path) rather than app/workspace source. A TS/JSX-extension
// file is always source and must be transpiled, even outside the root: monorepo
// packages reached through a resolve.alias are served via /@fs/ but are source.
/// A dependency module: JS under a `node_modules` directory (by url, or by the
/// real path an `/@fs/` url names). A linked workspace package realpaths outside
/// node_modules and is source, as in Vite's optimizer, so plugins and the
/// source compile path apply to it.
fn is_dep_module(url: &str, file: &Path) -> bool {
    let src_ext = matches!(
        file.extension().and_then(|e| e.to_str()),
        Some("ts" | "tsx" | "jsx" | "mts" | "cts")
    );
    !src_ext
        && (url.contains("/node_modules/")
            || (url.starts_with("/@fs/")
                && file.components().any(|c| c.as_os_str() == "node_modules")))
}

/// Stylesheet extensions (public: the in-process Start module host classifies
/// resolved paths with the same rule the dev server uses).
pub fn is_style_ext(ext: &str) -> bool {
    matches!(ext, "css" | "scss" | "sass" | "less" | "styl" | "stylus")
}

fn compile_fs_deny(user: &[String]) -> Vec<(glob::Pattern, bool)> {
    const DEFAULTS: &[&str] = &[".env", ".env.*", "*.crt", "*.pem", "**/.git/**"];
    DEFAULTS
        .iter()
        .map(|s| s.to_string())
        .chain(user.iter().cloned())
        .flat_map(|p| expand_braces(&p))
        .filter_map(|p| {
            let base_only = !p.contains('/');
            glob::Pattern::new(&p).ok().map(|pat| (pat, base_only))
        })
        .collect()
}

/// Expands `{a,b}` groups the way picomatch/Vite treat them; the `glob` crate
/// has no brace support, so `*.{key,pem}` (Vite's own default deny shape)
/// would otherwise compile to a literal that matches nothing.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_string()];
    };
    let Some(close) = pattern[open..].find('}').map(|i| open + i) else {
        return vec![pattern.to_string()];
    };
    let (head, rest) = (&pattern[..open], &pattern[close + 1..]);
    pattern[open + 1..close]
        .split(',')
        .flat_map(|alt| expand_braces(&format!("{head}{alt}{rest}")))
        .collect()
}

fn path_is_denied(file: &Path, root: &Path, deny: &[(glob::Pattern, bool)]) -> bool {
    if deny.is_empty() {
        return false;
    }
    let rel = file.strip_prefix(root).unwrap_or(file);
    let rel_str = rel.to_string_lossy().replace('\\', "/");
    let abs_str = file.to_string_lossy().replace('\\', "/");
    let base = file
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Match case-insensitively: a deny list is a security control, and on a
    // case-insensitive filesystem (macOS, Windows) `.ENV` opens the same bytes
    // as `.env`, so a case-sensitive glob would leak the file. Denying a
    // superset on case-sensitive filesystems is the safe direction. Other
    // options stay at glob's defaults, so only case behavior changes.
    let opts = glob::MatchOptions {
        case_sensitive: false,
        ..glob::MatchOptions::new()
    };
    for (pat, base_only) in deny {
        let hit = if *base_only {
            pat.matches_with(&base, opts)
        } else {
            pat.matches_with(&rel_str, opts) || pat.matches_with(&abs_str, opts)
        };
        if hit {
            return true;
        }
    }
    false
}

// The browser stamps every request with Sec-Fetch-Dest describing what it will
// do with the bytes: `style` (<link rel=stylesheet>), `image` (<img>), `font`
// (@font-face), media. Those want the raw resource. A JS `import` fetches the
// module with dest `script`/`empty`/worker, which wants the JS-module form: a
// style-injecting module for CSS, a URL-exporting module for an asset. Vite draws
// the same line; a `.css` reached from JS is served as JS, not text/css.
/// An asset url requested by a module import: Vite marks those `?import`, and a
/// browser sets `sec-fetch-dest: script` for an `import` of the url. Anything
/// else (a fetch(), curl, the Start proxy, an <img>) gets the file's bytes, as
/// Vite's static middleware serves them.
fn wants_module_import(headers: &HeaderMap, query: Option<&str>) -> bool {
    query.is_some_and(|q| q.split('&').any(|kv| kv == "import"))
        || headers.get("sec-fetch-dest").and_then(|v| v.to_str().ok()) == Some("script")
}

fn wants_raw_resource(headers: &HeaderMap) -> bool {
    matches!(
        headers.get("sec-fetch-dest").and_then(|v| v.to_str().ok()),
        Some("style" | "image" | "font" | "audio" | "video" | "track" | "object" | "embed")
    )
}

// Assets that, when imported from JS, resolve to a URL-exporting module (Vite's
// default asset handling, case-insensitive). svg is excluded here: it is routed
// through the compile path so vite-plugin-svgr can componentize it, falling back
// to a URL module there.
pub fn is_importable_asset_ext(ext: &str) -> bool {
    oj_compiler::assets::is_asset_ext(ext) && !ext.eq_ignore_ascii_case("svg")
}

// Node core modules. When one reaches the browser graph (usually via config-time
// tooling a dep drags along), Vite serves a browser-externalized stub rather than
// 404ing the whole module chain; oj does the same so the app still mounts. Pub:
// the Start module host also consults it, because on the SSR side a builtin
// outranks an installed polyfill package of the same name (Vite's fetchModule
// checks isBuiltin before resolving), exactly as Node itself behaves.
pub fn is_node_builtin(spec: &str) -> bool {
    // Vite's isNodeBuiltin: anything under the `node:` scheme is a builtin (this
    // covers node:sqlite, node:sea, node:test and whatever Node adds next); the
    // list below is `module.builtinModules` for the bare (scheme-less) names.
    if spec.starts_with("node:") {
        return true;
    }
    let base = spec.split('/').next().unwrap_or(spec);
    if base.starts_with("_http_") || base.starts_with("_stream_") || base.starts_with("_tls_") {
        return true;
    }
    matches!(
        base,
        "assert"
            | "async_hooks"
            | "buffer"
            | "child_process"
            | "cluster"
            | "console"
            | "constants"
            | "crypto"
            | "dgram"
            | "diagnostics_channel"
            | "dns"
            | "domain"
            | "events"
            | "fs"
            | "http"
            | "http2"
            | "https"
            | "inspector"
            | "module"
            | "net"
            | "os"
            | "path"
            | "perf_hooks"
            | "process"
            | "punycode"
            | "querystring"
            | "readline"
            | "repl"
            | "stream"
            | "string_decoder"
            | "sys"
            | "timers"
            | "tls"
            | "trace_events"
            | "tty"
            | "url"
            | "util"
            | "v8"
            | "vm"
            | "wasi"
            | "worker_threads"
            | "zlib"
    )
}

// Vite's searchForWorkspaceRoot: walk up from the app root and stop at the first
// workspace marker (pnpm-workspace.yaml / lerna.json, or a package.json with a
// `workspaces` field); otherwise searchForPackageRoot, the NEAREST ancestor with
// a package.json (the root itself, normally). `.git` is deliberately not a
// marker (Vite comments it out): a project nested somewhere inside a repository
// must not expose the whole repository over /@fs by default. oj seeds
// server.fs.allow with this, matching Vite's default.
fn workspace_root(root: &Path) -> PathBuf {
    let mut pkg_root: Option<PathBuf> = None;
    let mut dir = root;
    loop {
        if dir.join("pnpm-workspace.yaml").exists() || dir.join("lerna.json").exists() {
            return dir.to_path_buf();
        }
        if dir.join("package.json").exists() {
            if let Ok(txt) = std::fs::read_to_string(dir.join("package.json")) {
                if txt.contains("\"workspaces\"") {
                    return dir.to_path_buf();
                }
            }
            if pkg_root.is_none() {
                pkg_root = Some(dir.to_path_buf());
            }
        }
        match dir.parent() {
            Some(p) => dir = p,
            None => return pkg_root.unwrap_or_else(|| root.to_path_buf()),
        }
    }
}

// Rewrite `import { X } from "node:builtin"` to read X off the browser-externalized
// stub (undefined) instead of a native named import that fails to link, matching
// Vite's importAnalysis interop for browser-external modules. Returns None when the
// source imports no node builtins. Applied on every compile path so deps and app
// source interop consistently.
// Pre-resolve a module's static imports (same resolver ctx.resolve uses) into a
// {spec: id|null} JSON map, handed to the plugin transform so a plugin's per-import
// `this.resolve` is a local lookup instead of a host round-trip. This is what keeps
// import-protection's transform (a resolve per import) from being thousands of IPC
// round-trips per page.
fn resolved_imports_json(
    resolver: &OjResolver,
    fs_allow: &Mutex<std::collections::HashSet<PathBuf>>,
    source: &str,
    file: &Path,
) -> String {
    let dir = file.parent().unwrap_or(file);
    let mut map = serde_json::Map::new();
    for spec in oj_compiler::imports(source, file) {
        // Node builtins never resolve to a file (they're browser-externalized via
        // interop); skip them so the resolver doesn't log a "cannot resolve" warning
        // per app module. A plugin's this.resolve falls back to the host for these.
        if is_node_builtin(&spec) {
            continue;
        }
        let val = match resolver.resolve(dir, &spec) {
            Ok(p) => {
                // The map hands these ids to the plugin transform; if the transform
                // keeps the import, the browser fetches it from /@fs, so allow-list
                // its package root now (rewrite_specifier does the same on its path).
                if p.components().any(|c| c.as_os_str() == "node_modules") || !p.starts_with(dir) {
                    fs_allow.lock().unwrap().insert(package_root(&p));
                }
                serde_json::Value::String(p.display().to_string())
            }
            Err(_) => serde_json::Value::Null,
        };
        map.insert(spec, val);
    }
    serde_json::Value::Object(map).to_string()
}

/// A stable, per-file condition (the bare star re-export of a CJS dep) would
/// otherwise re-warn on every recompile: HMR invalidations, cache misses,
/// restarts. Once per distinct message, like Vite's deduping logger.
fn warn_interop_once(msg: String) {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let seen = SEEN.get_or_init(Default::default);
    if seen.lock().unwrap().insert(msg.clone()) {
        eprintln!("oj: warning: {msg}");
    }
}

fn interop_node_builtins(source: &str, file: &Path) -> Option<String> {
    if !source.contains("node:") {
        return None;
    }
    oj_compiler::interop::rewrite_cjs_interop(source, file, &|spec| {
        is_node_builtin(spec).then(|| format!("/@id/{}", hex_encode(spec)))
    })
}

fn is_style_url(url: &str) -> bool {
    let f = url.split('?').next().unwrap_or(url);
    std::path::Path::new(f)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(is_style_ext)
}

async fn run_preprocess_engine(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
    options: serde_json::Value,
) -> Result<String, String> {
    let engine = state
        .preprocess
        .get_or_try_init(|| CssEngine::preprocess(&state.root, css_engine::DEV_DEADLINE))
        .await
        .map_err(|e| e.to_string())?;
    engine.compile_with(source, url, options).await
}

async fn run_svelte_engine(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let engine = state
        .svelte
        .get_or_try_init(|| CssEngine::svelte(&state.root, css_engine::DEV_DEADLINE))
        .await
        .map_err(|e| e.to_string())?;
    engine.compile(source, url).await
}

async fn compile_tailwind(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let css = run_css_engine(state, url, source).await?;
    state.tailwind_urls.lock().unwrap().insert(url.to_string());
    Ok(css)
}

/// A plugin drove `server.moduleGraph.invalidateModule(...)` or `server.restart()`
/// from the host. Invalidation drops the module's compiled output (so its next
/// request recompiles, re-running plugin transforms) and, for a file in the app,
/// propagates an HMR update exactly as a change to that file would; a virtual id
/// only loses its cache, as in Vite (plugins push their own ws message then).
async fn handle_plugin_server_event(state: &Arc<ServerState>, ev: &serde_json::Value) {
    match ev.get("action").and_then(|a| a.as_str()) {
        Some("restart") => {
            println!("oj: plugin requested server.restart()");
            restart_process();
        }
        Some("invalidateAll") => {
            state.mtime_keys.lock().unwrap().clear();
            state.memory.lock().unwrap().clear();
            let _ = state
                .reload_tx
                .send(full_reload_frame("plugin invalidateAll", None, None));
        }
        Some("invalidate") => {
            let Some(id) = ev.get("id").and_then(|i| i.as_str()) else {
                return;
            };
            let clean = id.split('?').next().unwrap_or(id);
            let path = Path::new(clean);
            let url = if path.is_absolute() && path.starts_with(&state.root) {
                url_of(&state.root, path)
            } else if clean.starts_with('/') {
                clean.to_string()
            } else {
                return;
            };
            state.mtime_keys.lock().unwrap().remove(&url);
            state.memory.lock().unwrap().remove(&url);
            if path.is_file() && state.graph.lock().unwrap().contains(Path::new(&url)) {
                for message in decide(state, &[path.to_path_buf()], &Default::default()).await {
                    let _ = state.reload_tx.send(message);
                }
            }
        }
        _ => {}
    }
}

fn handle_client_message(state: &Arc<ServerState>, text: &str) {
    let Ok(msg) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    // Vite's client sends `import.meta.hot.invalidate()` as the custom event
    // `vite:invalidate` (`{path, message, firstInvalidatedBy}`).
    if msg["type"] == "custom" && msg["event"] == "vite:invalidate" {
        let body = &msg["data"];
        let Some(path) = body["path"].as_str() else {
            return;
        };
        let first_invalidated_by = body["firstInvalidatedBy"].as_str();
        let reply = {
            let timestamp = now_millis() as u64;
            let (dirty, targets) = {
                let mut graph = state.graph.lock().unwrap();
                // Only a self-accepting module an update touched, once per update
                // (Vite's lastHMRInvalidationReceived); its importers are stamped
                // so the boundary's re-fetch sees the invalidated module's version.
                let Some(dirty) = graph.accept_invalidation(Path::new(path), timestamp) else {
                    println!("oj: invalidate {path} ignored (no pending update)");
                    return;
                };
                (dirty, graph.update_targets_from_importers(Path::new(path)))
            };
            {
                let mut keys = state.mtime_keys.lock().unwrap();
                for d in &dirty {
                    keys.remove(&d.display().to_string());
                }
            }
            match targets {
                // The invalidate came back around to the module that started the
                // chain: no importer can hot update it, so reload (Vite's
                // 'circular import invalidate').
                Ok(targets)
                    if first_invalidated_by.is_some_and(|first| {
                        targets
                            .iter()
                            .any(|t| t.accepted.display().to_string() == first)
                    }) =>
                {
                    let reason = "circular import invalidate";
                    println!("oj: invalidate {path} -> full-reload ({reason})");
                    full_reload_frame(reason, None, None)
                }
                Ok(targets) => {
                    let boundaries: Vec<&Path> =
                        targets.iter().map(|t| t.boundary.as_path()).collect();
                    println!("oj: invalidate {path} -> update {boundaries:?}");
                    let first = first_invalidated_by.unwrap_or(path);
                    let updates: Vec<_> = targets
                        .iter()
                        .map(|t| update_entry_for(t, timestamp, Some(first)))
                        .collect();
                    serde_json::json!({ "type": "update", "updates": updates }).to_string()
                }
                Err(reason) => {
                    println!("oj: invalidate {path} -> full-reload ({reason})");
                    full_reload_frame(&reason, None, None)
                }
            }
        };
        let _ = state.reload_tx.send(reply);
    } else if msg["type"] == "custom" && msg["event"].is_string() {
        let _ = state.reload_tx.send(
            serde_json::json!({
                "type": "custom",
                "event": msg["event"],
                "data": msg["data"],
            })
            .to_string(),
        );
        if let Some(host) = state.plugins.clone() {
            let event = msg["event"].as_str().unwrap_or_default().to_string();
            let data = msg["data"].to_string();
            tokio::spawn(async move {
                let _ = host.ws_message(&event, &data).await;
            });
        }
    }
}

/// The id a module's hot context and Fast Refresh registration use: its url with
/// oj's HMR cache-busting `t=<timestamp>` removed but every other query kept, so a
/// `?tsr-shared=1` variant stays a distinct module while the id is stable across
/// updates. It must match the clean path the server names in its update messages;
/// a timestamped id would never match, so accept callbacks would never fire.
/// Mirrors Vite's `removeTimestampQuery`.
fn strip_hmr_timestamp(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    // oj's HMR timestamp is `now_millis()`: exactly 13 digits. Match only that,
    // as Vite's `timestampRE` (`/\bt=\d{13}&?\b/`) does, so a user's own short or
    // non-numeric `t=` query is never mistaken for it and stripped.
    let kept: Vec<&str> = query
        .split('&')
        .filter(|kv| {
            !(kv.starts_with("t=") && kv.len() == 15 && kv[2..].bytes().all(|b| b.is_ascii_digit()))
        })
        .collect();
    if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    }
}

/// Append the HMR timestamp to a served module url when its module has one. Only
/// modules served as JS are stamped (compilable sources, JSON, and stylesheet
/// wrappers, which Vite's importAnalysis also stamps so an edited data file or
/// CSS module is re-fetched rather than read from the browser's module cache):
/// asset (`?url`, `?raw`) and oj-internal (`/@oj-deps/`, `/@fs/`, `/@id/`, ...)
/// urls are left alone, since deps never take part in HMR.
fn stamp_import_url(url: &str, timestamp: u64) -> String {
    if timestamp == 0 || url.starts_with("/@") || !url.starts_with('/') {
        return url.to_string();
    }
    let (path, query) = url.split_once('?').unwrap_or((url, ""));
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    if !COMPILABLE.contains(&ext) && ext != "json" && !is_style_ext(ext) {
        return url.to_string();
    }
    if query.split('&').any(|kv| kv.starts_with("t=")) {
        return url.to_string();
    }
    if query.is_empty() {
        format!("{path}?t={timestamp}")
    } else {
        format!("{url}&t={timestamp}")
    }
}

/// `ctx_predefined`: the served body already carries the hot-context banner
/// (serve_compiled prepends it when the module reads `import.meta.hot`
/// itself). The glue must then REUSE `import.meta.hot` — as Vite's refresh
/// footer reuses the import-analysis banner — never re-import: an import
/// binding is a lexical declaration, and a second
/// `import {{ createHotContext as __oj_createHotContext }}` in the same
/// module scope is a SyntaxError that kills the whole module.
fn hot_glue(url: &str, query: Option<&str>, is_boundary: bool, ctx_predefined: bool) -> String {
    if !is_boundary {
        return String::new();
    }
    // serve_compiled keys modules per full url, so `url` usually already carries
    // its query and `query` repeats it; it can also arrive clean with the query
    // separate. Either way the self-import must name exactly the module being
    // served (keeping its `t=` so the browser dedupes to the running instance) and
    // must never re-append a query the url already has. Doing so once per edit
    // grew the url without bound (`?t=X?t=Y...`) until hyper answered 414.
    let self_specifier = match query {
        Some(q) if !q.is_empty() && !url.contains('?') => format!("{url}?{q}"),
        _ => url.to_string(),
    };
    let id = strip_hmr_timestamp(&self_specifier);
    let ctx = if ctx_predefined {
        String::new()
    } else {
        format!(
            "import {{ createHotContext as __oj_createHotContext }} from \"/@oj/client.js\";\nimport.meta.hot ??= __oj_createHotContext({id:?});\n"
        )
    };
    format!(
        r#"
{ctx}import * as RefreshRuntime from "/@oj/refresh-runtime.js";
import * as __oj_currentExports from {self_specifier:?};
if (import.meta.hot) {{
  if (!window.__oj_refresh_installed__) {{
    throw new Error("oj: Fast Refresh preamble missing; was index.html served by oj?");
  }}
  const currentExports = __oj_currentExports;
  RefreshRuntime.registerExportsForReactRefresh({id:?}, currentExports);
  import.meta.hot.accept((nextExports) => {{
    if (!nextExports) return;
    const invalidateMessage = RefreshRuntime.validateRefreshBoundaryAndEnqueueUpdate({id:?}, currentExports, nextExports);
    if (invalidateMessage) import.meta.hot.invalidate(invalidateMessage);
  }});
}}
function $RefreshReg$(type, id) {{ return RefreshRuntime.register(type, {id:?} + " " + id); }}
function $RefreshSig$() {{ return RefreshRuntime.createSignatureFunctionForTransform(); }}
"#
    )
}

/// Vite's `ErrorPayload` (`{type:'error', err:{message, stack, id, loc, frame, plugin}}`).
/// oj's messages are "title\n<file>:<line>:<col>...\nframe" style text; the file
/// location is lifted into `id`/`loc` so a Vite-protocol overlay shows it.
/// Broadcast an error frame to the connected clients, or hold it for the next
/// one when none is connected yet (Vite's ws server does the same: a page whose
/// first module request 500s has not opened its socket by then, and without the
/// buffered frame it would show a blank page instead of the overlay).
fn send_error(state: &ServerState, message: &str) {
    let frame = error_frame(message);
    if state.reload_tx.receiver_count() == 0 {
        *state.buffered_error.lock().unwrap() = Some(frame);
    } else {
        let _ = state.reload_tx.send(frame);
    }
}

fn error_frame(message: &str) -> String {
    let mut err = serde_json::json!({ "message": message, "stack": "", "plugin": "oj" });
    static LOC: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = LOC.get_or_init(|| {
        regex::Regex::new(r"([^\s():]+\.[A-Za-z0-9]+):(\d+)(?::(\d+))?").expect("loc regex")
    });
    if let Some(c) = re.captures(message) {
        err["id"] = serde_json::Value::String(c[1].to_string());
        let line = c[2].parse::<u64>().unwrap_or(0);
        let column = c
            .get(3)
            .and_then(|m| m.as_str().parse::<u64>().ok())
            .unwrap_or(0);
        err["loc"] = serde_json::json!({ "file": &c[1], "line": line, "column": column });
    }
    if let Some((_, frame)) = message.split_once('\n') {
        if !frame.trim().is_empty() {
            err["frame"] = serde_json::Value::String(frame.to_string());
        }
    }
    serde_json::json!({ "type": "error", "err": err }).to_string()
}

/// One entry of Vite's `UpdatePayload.updates`.
fn update_entry(kind: &str, path: &str, timestamp: u64) -> serde_json::Value {
    serde_json::json!({
        "type": kind,
        "path": path,
        "acceptedPath": path,
        "timestamp": timestamp,
    })
}

/// The `js-update` entry for a graph boundary: stylesheet boundaries are named by
/// their module wrapper (`?import`); `isWithinCircularImport` and
/// `firstInvalidatedBy` are carried like Vite's `Update` so the client can reset
/// the page when a re-import inside a cycle fails and escalate a repeated
/// `hot.invalidate` instead of looping.
fn update_entry_for(
    target: &oj_graph::UpdateTarget,
    timestamp: u64,
    first_invalidated_by: Option<&str>,
) -> serde_json::Value {
    let style = |p: &Path| {
        let p = p.display().to_string();
        if is_style_url(&p) {
            format!("{p}?import")
        } else {
            p
        }
    };
    let mut entry = update_entry("js-update", &style(&target.boundary), timestamp);
    entry["acceptedPath"] = serde_json::Value::String(style(&target.accepted));
    if target.within_circular_import {
        entry["isWithinCircularImport"] = serde_json::Value::Bool(true);
    }
    if let Some(first) = first_invalidated_by {
        entry["firstInvalidatedBy"] = serde_json::Value::String(first.to_string());
    }
    entry
}

/// Vite's `FullReloadPayload`: `path` is the edited page (`/about.html`) so the
/// client reloads only tabs showing it, or `*` for every page; `triggeredBy` is
/// the absolute file. oj's `reason` is kept for its own log and tooling.
fn full_reload_frame(reason: &str, page: Option<&str>, triggered_by: Option<&Path>) -> String {
    let mut frame = serde_json::json!({
        "type": "full-reload",
        "reason": reason,
        "path": page.unwrap_or("*"),
    });
    if let Some(file) = triggered_by {
        frame["triggeredBy"] = serde_json::Value::String(file.display().to_string());
    }
    frame.to_string()
}

fn svelte_hot_glue(url: &str) -> String {
    format!(
        "\nimport {{ createHotContext as __oj_createHotContext }} from \"/@oj/client.js\";\nimport.meta.hot = __oj_createHotContext({url:?});\n"
    )
}

type DirCache = std::collections::HashMap<
    PathBuf,
    std::sync::Arc<std::collections::HashMap<std::ffi::OsString, bool>>,
>;

// Whether any of a cached module's imports is a plugin-served virtual — a
// filesystem-path import (not an oj-internal `/@…` route or an external URL)
// with no file on disk. Such a module's transform produced in-memory state
// (e.g. wyw-in-js's extracted CSS) that a warm start would lose, so it must be
// re-transformed. Import URLs are either root-relative (`/src/x.ts`) or absolute
// (`/Users/…/x.wyw-in-js.css`); check both interpretations before concluding a
// path is virtual.
fn imports_a_plugin_virtual(imports: &[String], root: &Path, dir_cache: &Mutex<DirCache>) -> bool {
    imports.iter().any(|imp| {
        let p = imp.split('?').next().unwrap_or(imp);
        if !p.starts_with('/') || p.starts_with("/@") || p.contains("://") {
            return false;
        }
        let as_root = root.join(p.trim_start_matches('/'));
        if is_file_cached(dir_cache, &as_root) {
            return false;
        }
        let as_abs = Path::new(p);
        !is_file_cached(dir_cache, as_abs)
    })
}

fn is_file_cached(cache: &Mutex<DirCache>, path: &Path) -> bool {
    let (Some(dir), Some(name)) = (path.parent(), path.file_name()) else {
        return path.is_file();
    };
    if let Some(entries) = cache.lock().unwrap().get(dir) {
        return entries.get(name).copied().unwrap_or(false);
    }
    let mut map = std::collections::HashMap::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let is_file = match e.file_type() {
                Ok(ft) if ft.is_file() => true,
                Ok(ft) if ft.is_symlink() => e.path().is_file(),
                _ => false,
            };
            map.insert(e.file_name(), is_file);
        }
    }
    let arc = std::sync::Arc::new(map);
    let result = arc.get(name).copied().unwrap_or(false);
    cache.lock().unwrap().insert(dir.to_path_buf(), arc);
    result
}

// Partial bundling (oj-native per-package dep bundling) is opt-in for now.
fn partial_bundle_enabled() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var("OJ_PARTIAL_BUNDLE").is_ok_and(|v| !v.is_empty() && v != "0"))
}

// The URL oj serves a resolved bare dependency from. With partial bundling on, a
// CommonJS package under node_modules is served as a single `/@oj-pkg` bundle
// (keyed by its entry path, so every reference to the same entry — the app's
// import and other packages' requires — collapses to one shared bundle);
// everything else keeps its normal per-file URL. Both the importer-interop and
// specifier-rewrite paths route through here so a dep never gets two URLs.
fn dep_serve_url(resolved: &Path, root: &Path) -> String {
    if partial_bundle_enabled()
        && resolved.components().any(|c| c.as_os_str() == "node_modules")
        && is_bundleable_dep_file(resolved)
        // optimizeDeps.exclude: serve this package per-file, never bundled.
        && !pkg_bundle::is_excluded(resolved)
    {
        return pkg_bundle::bundle_url_for(resolved);
    }
    url_of(root, resolved)
}

/// Vite's DEP_VERSION_RE: the request carries a `v=` query, i.e. it was reached
/// through a versioned dep URL and may be cached forever.
fn has_version_query(query: Option<&str>) -> bool {
    query.is_some_and(|q| q.split('&').any(|kv| kv.starts_with("v=")))
}

/// Vite transform middleware: an optimized dep is `max-age=31536000,immutable`
/// (its URL changes with the prebundle hash), everything else `no-cache`.
fn dep_cache_control(versioned: bool) -> &'static str {
    if versioned {
        "max-age=31536000,immutable"
    } else {
        "no-cache"
    }
}

/// A prebundled dep response: strong ETag over the bytes with a 304 on a matching
/// If-None-Match (Vite's send() does the same for every transformed module), and
/// the immutable cache policy when the URL is versioned.
fn dep_response(headers: &HeaderMap, versioned: bool, bytes: Vec<u8>) -> Response {
    let etag = format!("\"{}\"", &blake3::hash(&bytes).to_hex()[..16]);
    let cache_control = dep_cache_control(versioned).to_string();
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|inm| inm.split(',').any(|t| t.trim() == etag))
    {
        return (
            StatusCode::NOT_MODIFIED,
            [(header::ETAG, etag), (header::CACHE_CONTROL, cache_control)],
        )
            .into_response();
    }
    (
        [
            (header::CONTENT_TYPE, "text/javascript".to_string()),
            (header::CACHE_CONTROL, cache_control),
            (header::ETAG, etag),
        ],
        bytes,
    )
        .into_response()
}

pub(crate) const OPTIONAL_PEER_PREFIX: &str = "/@oj-optional-peer/";

/// Vite's optionalPeerDepId (resolve.ts tryNodeResolve): a bare import that does
/// not resolve, made from inside a dependency (never from the app root), whose
/// nearest package.json lists the package under `peerDependencies` with
/// `peerDependenciesMeta[pkg].optional`, resolves to a stub module id carrying
/// the peer and the parent names. The stub errors only when evaluated.
pub(crate) fn optional_peer_dep_url(root: &Path, dir: &Path, spec: &str) -> Option<String> {
    if !is_bare_specifier(spec) || spec.is_empty() || is_node_builtin(spec) || spec.contains('\0') {
        return None;
    }
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if dir == root || !dir.components().any(|c| c.as_os_str() == "node_modules") {
        return None;
    }
    let pkg_name = {
        let mut it = spec.split('/');
        let first = it.next()?;
        if first.starts_with('@') {
            format!("{first}/{}", it.next()?)
        } else {
            first.to_string()
        }
    };
    // findNearestMainPackageData: the closest package.json with a `name`.
    let mut cur: Option<&Path> = Some(dir.as_path());
    while let Some(d) = cur {
        if let Ok(txt) = std::fs::read_to_string(d.join("package.json")) {
            if let Ok(pkg) = serde_json::from_str::<serde_json::Value>(&txt) {
                if let Some(parent) = pkg.get("name").and_then(|n| n.as_str()) {
                    let declared = pkg
                        .get("peerDependencies")
                        .and_then(|p| p.get(&pkg_name))
                        .is_some();
                    let optional = pkg
                        .get("peerDependenciesMeta")
                        .and_then(|m| m.get(&pkg_name))
                        .and_then(|m| m.get("optional"))
                        .and_then(|o| o.as_bool())
                        .unwrap_or(false);
                    if declared && optional {
                        return Some(format!(
                            "{OPTIONAL_PEER_PREFIX}{}",
                            hex_encode(&format!("{spec}\n{parent}"))
                        ));
                    }
                    return None;
                }
            }
        }
        if d.file_name().is_some_and(|n| n == "node_modules") {
            return None;
        }
        cur = d.parent();
    }
    None
}

/// The module `/@oj-optional-peer/<hex>` serves (Vite's optional peer stub in
/// rolldownDepPlugin): evaluating it throws `Could not resolve "peer" imported by
/// "parent". Is it installed?`, so the failure names both packages instead of the
/// browser's generic unresolved-specifier error for the whole importer chain.
fn optional_peer_dep_stub(hex: &str) -> Option<String> {
    let decoded = hex_decode(hex)?;
    let (peer, parent) = decoded.split_once('\n')?;
    let peer_js = serde_json::Value::String(peer.to_string());
    let parent_js = serde_json::Value::String(parent.to_string());
    Some(format!(
        "// oj: optional peer dependency {peer_js} of {parent_js} is not installed\n\
         export default {{}};\n\
         export const __cjs_exports = {{}};\n\
         throw new Error(`Could not resolve \"${{{peer_js}}}\" imported by \"${{{parent_js}}}\". Is it installed?`);\n"
    ))
}

fn rewrite_specifier(
    root: &Path,
    dir: &Path,
    resolver: &OjResolver,
    fs_allow: &Mutex<std::collections::HashSet<PathBuf>>,
    dir_cache: &Mutex<DirCache>,
    spec: &str,
    css_import_marker: bool,
) -> Option<String> {
    if spec.starts_with('/') {
        // A root-relative URL (/src/x) is already servable. But a plugin can emit an
        // absolute filesystem path under root -- TanStack's dev client entry, and the
        // router code-splitter's `?tsr-shared=1` imports -- so rewrite that to its
        // root-relative URL (preserving any query), as Vite's import analysis does.
        let (base, query) = match spec.split_once('?') {
            Some((b, q)) => (b, Some(q)),
            None => (spec, None),
        };
        let p = Path::new(base);
        if is_file_cached(dir_cache, p) {
            let url = if p.starts_with(root) {
                url_of(root, p)
            } else {
                // An absolute path OUTSIDE root (a plugin pointing into a sibling
                // monorepo package): serve it through /@fs like Vite's FS_PREFIX
                // and allow its package so the fs guard lets it through.
                fs_allow.lock().unwrap().insert(package_root(p));
                dep_serve_url(p, root)
            };
            return Some(match query {
                Some(q) => format!("{url}?{q}"),
                None => url,
            });
        }
        return None;
    }
    if spec.contains("://") {
        return None;
    }

    if is_lingui_macro_specifier(spec) {
        warn_lingui_macro_shim_once();
        return Some("/@oj/lingui-macro-shim.js".to_string());
    }

    if let Some((base, query)) = spec.split_once('?') {
        if matches!(
            query,
            "url" | "raw" | "inline" | "worker" | "sharedworker" | "init" | "react" | "no-inline"
        ) {
            let resolved = rewrite_specifier(root, dir, resolver, fs_allow, dir_cache, base, false)
                .or_else(|| {
                    resolver.resolve(dir, base).ok().map(|p| {
                        fs_allow.lock().unwrap().insert(package_root(&p));
                        url_of(root, &p)
                    })
                })?;
            return Some(format!("{resolved}?{query}"));
        }
    }

    if spec.starts_with("./") || spec.starts_with("../") {
        let mut joined = normalize(&dir.join(spec));
        if !is_file_cached(dir_cache, &joined) {
            if let Some(ext) = joined.extension().and_then(|e| e.to_str()) {
                if ext == "js" || ext == "jsx" {
                    for cand in ["ts", "tsx"] {
                        let alt = joined.with_extension(cand);
                        if is_file_cached(dir_cache, &alt) {
                            joined = alt;
                            break;
                        }
                    }
                }
            }
        }
        let quick = if is_file_cached(dir_cache, &joined) {
            Some(joined)
        } else if joined.extension().is_none() {
            COMPILABLE
                .iter()
                .map(|ext| joined.with_extension(ext))
                .find(|c| is_file_cached(dir_cache, c))
        } else {
            None
        };
        if let Some(p) = quick {
            let url = url_of(root, &p);
            if css_import_marker && is_style_url(&url) {
                return Some(format!("{url}?import"));
            }
            if css_import_marker && is_asset_path(&p) {
                return Some(format!("{url}?url"));
            }
            return Some(url);
        }
    }

    match resolver.resolve(dir, spec) {
        // A node_modules dependency routes through `dep_serve_url` even when it
        // sits under the app root (the common layout), so partial bundling can
        // collapse it. `dep_serve_url` returns the plain per-file URL when partial
        // bundling is off or the file isn't bundleable, so this is a no-op then.
        Ok(resolved)
            if resolved
                .components()
                .any(|c| c.as_os_str() == "node_modules") =>
        {
            fs_allow.lock().unwrap().insert(package_root(&resolved));
            Some(dep_serve_url(&resolved, root))
        }
        Ok(resolved) if resolved.starts_with(root) => {
            // An alias (`@/assets/logo.svg`) or root-absolute import of a style
            // or asset gets the same `?import` / `?url` marks a relative one does.
            let url = url_of(root, &resolved);
            if css_import_marker && is_style_url(&url) {
                return Some(format!("{url}?import"));
            }
            if css_import_marker && is_asset_path(&resolved) {
                return Some(format!("{url}?url"));
            }
            Some(url)
        }
        Ok(resolved) => {
            fs_allow.lock().unwrap().insert(package_root(&resolved));
            Some(dep_serve_url(&resolved, root))
        }
        Err(err) if err.ignored => {
            // The package maps this specifier to `false` for the browser (a
            // package.json `browser` field, e.g. typescript's `fs`/`crypto`/
            // `source-map-support`). Vite serves an empty module here; do the
            // same so the importing dep still loads instead of 404ing.
            Some("/@oj-empty".to_string())
        }
        Err(err) => {
            // A dependency's missing OPTIONAL peer (peerDependenciesMeta.optional)
            // resolves to a module that errors when evaluated, naming both sides,
            // instead of the bare specifier failing the whole importer chain at
            // link time (Vite's optionalPeerDepId).
            if let Some(url) = optional_peer_dep_url(root, dir, spec) {
                return Some(url);
            }
            // Plugin-provided ids (virtual: modules, \0-prefixed) are expected
            // to miss the on-disk resolver; the caller's plugin fallback serves
            // them, so a "cannot resolve" line here is just misleading noise.
            let plugin_virtual = spec.starts_with("virtual:") || spec.starts_with('\0');
            if !(spec.starts_with("./")
                || spec.starts_with("../")
                || plugin_virtual
                || is_node_builtin(spec))
            {
                eprintln!("oj: cannot resolve '{spec}': {err}");
            }
            None
        }
    }
}

fn url_of(root: &Path, file: &Path) -> String {
    match file.strip_prefix(root) {
        Ok(rel) => format!("/{}", rel.display()),
        Err(_) => format!("/@fs{}", file.display()),
    }
}

/// A `./` or `../` import that neither exists on disk nor resolves (with the
/// configured extensions and index files). The query is dropped first: an
/// unknown query on an existing file is a legitimate specifier the browser
/// requests as-is, not a missing module.
fn relative_import_missing(dir: &Path, resolver: &OjResolver, spec: &str) -> bool {
    if !(spec.starts_with("./") || spec.starts_with("../")) {
        return false;
    }
    let base = spec.split('?').next().unwrap_or(spec);
    !normalize(&dir.join(base)).is_file() && resolver.resolve(dir, base).is_err_and(|e| !e.ignored)
}

/// A bare import (`some-pkg`, `@scope/pkg/sub`) the resolver cannot find and that
/// nothing else answers: not a node builtin (browser-externalized), not a
/// plugin-style virtual id (`virtual:`, `\0`), not a URL or data: import, and not
/// a package that maps the id to `false` (served empty). Vite's importAnalysis
/// fails the importer for these.
fn bare_import_unresolved(dir: &Path, resolver: &OjResolver, spec: &str) -> bool {
    if !is_bare_specifier(spec)
        || spec.is_empty()
        || spec.starts_with("virtual:")
        || spec.starts_with('\0')
        || spec.starts_with("data:")
        || is_node_builtin(spec)
        || is_lingui_macro_specifier(spec)
    {
        return false;
    }
    let base = spec.split('?').next().unwrap_or(spec);
    resolver.resolve(dir, base).is_err_and(|e| !e.ignored)
}

const UNRESOLVED_IMPORT_MARK: &str = "Failed to resolve import \"";

fn is_unresolved_import_error(err: &str) -> bool {
    err.contains(UNRESOLVED_IMPORT_MARK)
}

/// Vite's import-analysis error for a missing import ("Failed to resolve import
/// "./x" from "src/a.tsx". Does the file exist?"), in oj's `title\nfile:line:col
/// message\nframe` shape so the overlay lifts the location out of it. The
/// position is where the specifier is quoted in the (plugin-transformed) source.
fn unresolved_import_error(root: &Path, file: &Path, source: &str, spec: &str) -> String {
    let rel = file
        .strip_prefix(root)
        .unwrap_or(file)
        .display()
        .to_string();
    let quoted = ['"', '\'', '`']
        .iter()
        .find_map(|q| source.find(&format!("{q}{spec}{q}")).map(|p| p + 1));
    let (line, col) = match quoted {
        Some(pos) => {
            let before = &source[..pos];
            let line = before.matches('\n').count() + 1;
            let col = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
            (line, col)
        }
        None => (1, 1),
    };
    let frame = source.lines().nth(line - 1).unwrap_or("");
    format!(
        "compile error:\n{rel}:{line}:{col} {UNRESOLVED_IMPORT_MARK}{spec}\" from \"{rel}\". Does the file exist?\n{line:>4} | {frame}\n"
    )
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn locate(root: &Path, public_dir: Option<&Path>, rel: &str) -> Option<PathBuf> {
    if rel.split('/').any(|seg| seg == "..") {
        return None;
    }
    let base = root.join(rel);
    if base.is_file() {
        return Some(base);
    }
    if base.extension().is_none() {
        for ext in COMPILABLE {
            let candidate = base.with_extension(ext);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    let public = public_dir?.join(rel);
    if public.is_file() {
        return Some(public);
    }
    None
}

fn is_worker_query(url: &str) -> bool {
    match url.split_once('?') {
        Some((_, q)) => q
            .split('&')
            .any(|kv| kv == "worker" || kv == "sharedworker"),
        None => false,
    }
}

fn query_asset_kind(query: Option<&str>) -> Option<&'static str> {
    let q = query?;
    for kind in ["url", "raw", "worker", "sharedworker", "inline", "init"] {
        if q.split('&').any(|kv| kv == kind) {
            return Some(kind);
        }
    }
    // `?no-inline` only differs from `?url` in the build (never inlined).
    if q.split('&').any(|kv| kv == "no-inline") {
        return Some("url");
    }
    None
}

async fn asset_module(file: &Path, url: &str, kind: &str) -> Result<String, String> {
    let clean_url = url.split('?').next().unwrap_or(url);
    match kind {
        "url" => {
            // A stylesheet's URL must serve its COMPILED css (Sass, PostCSS, ...)
            // as text; `?direct` is the plain-CSS request, as in Vite.
            let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
            if is_style_ext(ext) {
                return Ok(format!("export default {:?};\n", format!("{clean_url}?direct")));
            }
            Ok(format!("export default {clean_url:?};\n"))
        }
        "raw" => {
            let text = tokio::fs::read_to_string(file)
                .await
                .map_err(|e| format!("read {}: {e}", file.display()))?;
            Ok(format!("export default {};\n", serde_json::Value::String(text)))
        }
        "inline" => {
            // A stylesheet's `?inline` goes through `inline_css_module` instead.
            let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
            let bytes = tokio::fs::read(file).await.map_err(|e| format!("read: {e}"))?;
            let mime = content_type(ext).split(';').next().unwrap_or("application/octet-stream");
            let data_uri = format!("data:{mime};base64,{}", base64_encode(&bytes));
            Ok(format!("export default {data_uri:?};\n"))
        }
        "worker" | "sharedworker" => {
            let ctor = if kind == "sharedworker" { "SharedWorker" } else { "Worker" };
            Ok(format!(
                "export default function () {{ return new {ctor}({clean_url:?}, {{ type: \"module\" }}); }}\n"
            ))
        }
        "init" => Ok(format!(
            "export default (imports = {{}}) => {{\n  const url = {clean_url:?};\n  const inst = (r) => r.instance;\n  const fallback = () => fetch(url).then((r) => r.arrayBuffer()).then((b) => WebAssembly.instantiate(b, imports)).then(inst);\n  if (WebAssembly.instantiateStreaming) {{\n    return WebAssembly.instantiateStreaming(fetch(url), imports).then(inst).catch(fallback);\n  }}\n  return fallback();\n}};\n"
        )),
        _ => Err(format!("unknown asset query: {kind}")),
    }
}

/// `import css from "./x.css?inline"`: the compiled stylesheet as a string. It
/// is the output of the same pipeline a plain stylesheet import runs (plugin
/// transforms, Sass/Less/Stylus with additionalData and loadPaths, PostCSS or
/// Tailwind, @import inlining, url() rebasing), as Vite's `?inline` is the css
/// of the same transform; a CSS module inlines its css, not its class map.
async fn inline_css_module(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
) -> Result<String, String> {
    let clean = url.split('?').next().unwrap_or(url);
    let (_, module) = Box::pin(ensure_module(state, file, clean)).await?;
    Ok(format!(
        "export default {};\n",
        serde_json::Value::String(module.code.clone())
    ))
}

fn base64_encode(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn is_server_module(file: &Path) -> bool {
    file.file_name()
        .and_then(|n| n.to_str())
        .map(|n| {
            [".server.ts", ".server.tsx", ".server.js", ".server.jsx"]
                .iter()
                .any(|s| n.ends_with(s))
        })
        .unwrap_or(false)
}

fn server_fn_stub(exports: &[String], url: &str) -> String {
    let mut out = String::from("import { __ojServerCall } from \"/@oj/server-fn.js\";\n");
    for name in exports {
        if name == "default" {
            out.push_str(&format!(
                "export default (...a) => __ojServerCall({url:?}, \"default\", a);\n"
            ));
        } else {
            out.push_str(&format!(
                "export const {name} = (...a) => __ojServerCall({url:?}, {name:?}, a);\n"
            ));
        }
    }
    out
}

fn debug_mem() -> bool {
    static ON: std::sync::LazyLock<bool> = std::sync::LazyLock::new(|| {
        std::env::var("OJ_DEBUG_MEM").is_ok_and(|v| !v.is_empty() && v != "0")
    });
    *ON
}

async fn debug_gc(headers: axum::http::HeaderMap) -> Response {
    if !debug_mem() {
        return (axum::http::StatusCode::NOT_FOUND, "").into_response();
    }
    // Probes (curl, the bench harness) send no Origin header; a hostile web
    // page's cross-origin fetch always does. GC-hammering from a browser tab
    // is the only remote vector this debug surface opens, so close it.
    if headers.contains_key(axum::http::header::ORIGIN) {
        return (axum::http::StatusCode::FORBIDDEN, "").into_response();
    }
    // Fans out through oj_js's engine registry (plugin hosts, SSR, Start,
    // CSS, addon-keeper alike) and returns only after each counted engine
    // ACKNOWLEDGED running its collection — a barrier, not a request, so a
    // probe reading RSS right after this response sees post-GC numbers.
    let collected = tokio::task::spawn_blocking(|| {
        oj_js::collect_all_garbage(std::time::Duration::from_secs(10))
    })
    .await
    .unwrap_or(0);
    (
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        format!("{{\"collected\":{collected}}}"),
    )
        .into_response()
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

// Rollup's contract, which the rest of this host follows: `resolveId` returning a
// path means that file IS the module, and a `load` returning nothing means read it
// from disk. Only the second half was implemented, so a plugin that maps a
// specifier to a path without also serving its bytes -- which is what a resolver
// plugin is -- got a 404 for every module it resolved correctly.
//
// Redirecting to the file's normal dep URL rather than reading it here keeps every
// downstream behaviour identical to any other dependency: the fs.allow check,
// partial bundling, and the specifier rewriting applied inside the served file.
fn serve_resolved_from_disk(state: &Arc<ServerState>, id: &str) -> Option<Response> {
    let resolved = Path::new(id);
    if !resolved.is_absolute() || !resolved.is_file() {
        return None;
    }
    state
        .fs_allow
        .lock()
        .unwrap()
        .insert(package_root(resolved));
    let url = dep_serve_url(resolved, &state.root);
    Some(Redirect::temporary(&url).into_response())
}

async fn serve_plugin_resolve(state: &Arc<ServerState>, id: &str) -> Response {
    let Some(host) = &state.plugins else {
        return (StatusCode::NOT_FOUND, "oj: no plugin host").into_response();
    };
    let source = match host.load(id).await {
        Ok(Some(src)) => src,
        Ok(None) => {
            if let Some(response) = serve_resolved_from_disk(state, id) {
                return response;
            }
            return (StatusCode::NOT_FOUND, format!("oj: no plugin loaded {id}")).into_response();
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let dep_map = state.optimized.ready().await;
    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir_cache = Arc::clone(&state.dir_cache);
    let virtual_ids: std::collections::BTreeSet<String> =
        state.virtual_modules.keys().cloned().collect();
    let plugin_fallback = state.plugins.is_some();
    let importer_abs = format!("\0{id}");
    let compile_opts = dev_compile_opts(state);
    let compiled = tokio::task::spawn_blocking(move || {
        let mut rewrite = |spec: &str| {
            if virtual_ids.contains(spec) {
                return Some(format!("/@virtual/{spec}"));
            }
            if let Some(meta) = dep_map.get(spec) {
                if !meta.needs_interop {
                    return Some(meta.url.clone());
                }
            }
            if let Some(url) =
                rewrite_specifier(&root, &root, &resolver, &fs_allow, &dir_cache, spec, true)
            {
                return Some(url);
            }
            if plugin_fallback && is_bare_specifier(spec) {
                return Some(format!(
                    "/@id/{}?importer={}",
                    hex_encode(spec),
                    hex_encode(&importer_abs)
                ));
            }
            None
        };
        let source = interop_node_builtins(&source, Path::new("plugin.tsx")).unwrap_or(source);
        oj_compiler::compile_module(
            Path::new("plugin.tsx"),
            &source,
            &compile_opts,
            Some(&mut rewrite),
        )
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
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("compile task failed: {e}"),
        )
            .into_response(),
    }
}

/// Vite's browser-externalized module for a node builtin that reaches the client
/// graph (optimizer rolldownDepPlugin `browser-external` load): a Proxy whose
/// property reads console.warn `Module "fs" has been externalized for browser
/// compatibility. Cannot access "fs.readFileSync" in client code.` and yield
/// undefined, so the app still mounts and the developer learns which dep pulled
/// the builtin in. Skips the keys bundlers, interop helpers and devtools poke.
fn browser_external_stub_source(spec: &str) -> String {
    let id = serde_json::Value::String(spec.to_string());
    format!(
        "// oj: browser-externalized node builtin {id}\n\
         const __oj_ext = Object.create(new Proxy({{}}, {{\n\
         \x20 get(_, key) {{\n\
         \x20   if (typeof key === \"string\" && key !== \"__esModule\" && key !== \"__proto__\" && key !== \"constructor\" && key !== \"splice\" && key !== \"then\") {{\n\
         \x20     console.warn(`Module \"${{{id}}}\" has been externalized for browser compatibility. Cannot access \"${{{id}}}.${{key}}\" in client code. See https://vite.dev/guide/troubleshooting.html#module-externalized-for-browser-compatibility for more details.`);\n\
         \x20   }}\n\
         \x20 }}\n\
         }}));\n\
         export default __oj_ext;\n\
         export const __cjs_exports = __oj_ext;\n"
    )
}

fn browser_external_stub(spec: &str) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        browser_external_stub_source(spec),
    )
        .into_response()
}

async fn serve_plugin_id(state: &Arc<ServerState>, spec: &str, importer: &str) -> Response {
    // A plugin may polyfill a node builtin (vite-plugin-node-polyfills), so the
    // host gets first refusal; with no host, or when no plugin claims it, the
    // builtin is browser-externalized like Vite does.
    let Some(host) = &state.plugins else {
        if is_node_builtin(spec) {
            return browser_external_stub(spec);
        }
        return (StatusCode::NOT_FOUND, "oj: no plugin host").into_response();
    };
    let id = match host.resolve_id(spec, importer).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            // A relative / absolute import routed here for a plugin's resolveId
            // filter that then declined it: Vite's own resolver takes over, so
            // resolve it against the importer like the native path would have.
            if !is_bare_specifier(spec) {
                let (base, query) = spec.split_once('?').unwrap_or((spec, ""));
                let dir = Path::new(importer)
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| state.root.clone());
                if let Ok(abs) = state.resolver.resolve(&dir, base) {
                    if abs.is_file() {
                        state.fs_allow.lock().unwrap().insert(package_root(&abs));
                        let mut url = dep_serve_url(&abs, &state.root);
                        if !query.is_empty() {
                            url.push('?');
                            url.push_str(query);
                        }
                        return Redirect::temporary(&url).into_response();
                    }
                }
            }
            if is_node_builtin(spec) {
                return browser_external_stub(spec);
            }
            // No plugin claimed the bare id the importer deferred here: this is
            // Vite's "Failed to resolve import" for that importer (500 + overlay
            // naming the import site), not a bare 404 the browser reports as a
            // generic module error.
            if is_bare_specifier(spec) && !importer.is_empty() {
                let importer_file = Path::new(importer);
                let source = std::fs::read_to_string(importer_file).unwrap_or_default();
                let err = unresolved_import_error(&state.root, importer_file, &source, spec);
                send_error(state, &err);
                return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response();
            }
            return (
                StatusCode::NOT_FOUND,
                format!("oj: no plugin resolved {spec}"),
            )
                .into_response();
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let source = match host.load(&id).await {
        Ok(Some(src)) => src,
        Ok(None) => {
            if let Some(response) = serve_resolved_from_disk(state, &id) {
                return response;
            }
            return (StatusCode::NOT_FOUND, format!("oj: no plugin loaded {id}")).into_response();
        }
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir_cache = Arc::clone(&state.dir_cache);
    let importer_id = id.clone();
    let compile_opts = dev_compile_opts(state);
    let compiled = tokio::task::spawn_blocking(move || {
        let mut rewrite = |s: &str| {
            if let Some(u) =
                rewrite_specifier(&root, &root, &resolver, &fs_allow, &dir_cache, s, true)
            {
                return Some(u);
            }
            // A plugin-loaded virtual can import another plugin virtual (the i18n
            // message groups import their `virtual:i18n-facade/*` counterpart). Route
            // bare specifiers back through the plugin like the on-disk compile path
            // does, instead of leaving `virtual:...` for the browser to fetch and fail.
            if is_bare_specifier(s) {
                return Some(format!(
                    "/@id/{}?importer={}",
                    hex_encode(s),
                    hex_encode(&importer_id)
                ));
            }
            None
        };
        let source = interop_node_builtins(&source, Path::new("plugin.tsx")).unwrap_or(source);
        oj_compiler::compile_module(
            Path::new("plugin.tsx"),
            &source,
            &compile_opts,
            Some(&mut rewrite),
        )
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
        Ok(Err(e)) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("compile task failed: {e}"),
        )
            .into_response(),
    }
}

// A plugin can `load` a module whose id is neither an on-disk file nor a bare
// specifier: wyw-in-js/linaria appends `import "<abs>.wyw-in-js.css"` to each
// transformed module and serves that absolute-path id from its own `load` hook,
// keeping the extracted CSS in memory. On a disk miss, consult the plugin
// container (resolveId -> load) before giving up. CSS a plugin returns is
// wrapped as a style-injecting JS module, matching Vite's `vite:css` handling of
// a `.css` import reached from JS (so the browser gets text/javascript, not a
// text/css module script the strict MIME check rejects).
// Serve a `/@oj-pkg/<hex>` package bundle: one request covering a CommonJS
// package's whole internal file graph (oj-native partial bundling). A package
// that can't be bundled in v1 (ESM entry, unsupported files) falls back to the
// entry's normal per-file compiled output, served at this same URL so the
// importer's interop (which reads __cjs_exports) still resolves.
async fn serve_pkg_bundle(state: &Arc<ServerState>, path: &str, versioned: bool) -> Response {
    let js = |code: Bytes| {
        (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, dep_cache_control(versioned)),
            ],
            code,
        )
            .into_response()
    };
    // A chunk emitted by a previous rolldown fallback (a code-split sibling, or
    // the entry re-served). These paths aren't decodable entry hexes, so they
    // must be checked before entry_from_url.
    if let Some(code) = pkg_rolldown::cached_chunk(path) {
        return js(code);
    }
    if let Some(code) = pkg_bundle::cached(path) {
        return js(code);
    }
    let Some(entry) = pkg_bundle::entry_from_url(path) else {
        return (StatusCode::NOT_FOUND, "oj: bad pkg bundle path").into_response();
    };
    let resolver = Arc::clone(&state.resolver);
    let root = state.root.clone();
    // Known-hard packages (e.g. object-inspect) produce a concatenator bundle
    // that builds but breaks at runtime, so they never bail into the fallback.
    // Force those straight through rolldown, bypassing the concatenator.
    if pkg_rolldown::enabled() && pkg_rolldown::is_forced(&entry) {
        if let Some(code) = pkg_rolldown::build(&entry, &state.root, Arc::clone(&resolver)).await {
            return js(code);
        }
    }
    let entry_owned = entry.clone();
    let build_resolver = Arc::clone(&resolver);
    let outcome = tokio::task::spawn_blocking(move || {
        pkg_bundle::build(&entry_owned, build_resolver.as_ref(), &root)
    })
    .await;
    match outcome {
        Ok(pkg_bundle::BundleOutcome::Bundle(code)) => {
            let code = Bytes::from(code);
            pkg_bundle::store(path, code.clone());
            js(code)
        }
        Ok(pkg_bundle::BundleOutcome::Fallback) => {
            // The concatenator bailed. Before serving per-file, try bundling this
            // one package with rolldown (the robust path, Vite-style), if enabled.
            if pkg_rolldown::enabled() {
                if let Some(code) =
                    pkg_rolldown::build(&entry, &state.root, Arc::clone(&resolver)).await
                {
                    return js(code);
                }
                if std::env::var("OJ_PB_DEBUG").is_ok_and(|v| !v.is_empty() && v != "0") {
                    eprintln!("oj[pb] rolldown fallback failed, serving per-file: {path}");
                }
            }
            let url = url_of(&state.root, &entry);
            match ensure_module(state, &entry, &url).await {
                Ok((_, module)) => js(module.code.clone().into()),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj: pkg bundle task failed: {e}"),
        )
            .into_response(),
    }
}

async fn serve_plugin_load_fallback(state: &Arc<ServerState>, uri: &Uri) -> Option<Response> {
    let host = state.plugins.as_ref()?;
    let spec = uri.path().to_string();
    let id = match host.resolve_id(&spec, "").await {
        Ok(Some(id)) => id,
        _ => return None,
    };
    let source = match host.load(&id).await {
        Ok(Some(src)) => src,
        _ => return None,
    };
    let id_path = id.split('?').next().unwrap_or(&id);
    let is_css = Path::new(id_path)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(is_style_ext);
    if is_css {
        let url = id_path.to_string();
        let body = format!(
            "import {{ createHotContext as __oj_hot, updateStyle as __oj_updateStyle }} from \"/@oj/client.js\";\n\
             import.meta.hot = __oj_hot({url:?});\n\
             __oj_updateStyle({url:?}, {css});\n\
             export default void 0;\n\
             import.meta.hot.accept(() => {{}});\n",
            css = serde_json::Value::String(source),
        );
        return Some(
            (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                body,
            )
                .into_response(),
        );
    }
    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir_cache = Arc::clone(&state.dir_cache);
    let importer_id = id.clone();
    let compile_opts = dev_compile_opts(state);
    let compiled = tokio::task::spawn_blocking(move || {
        let mut rewrite = |s: &str| {
            if let Some(u) =
                rewrite_specifier(&root, &root, &resolver, &fs_allow, &dir_cache, s, true)
            {
                return Some(u);
            }
            // A plugin-loaded virtual can import another plugin virtual (the i18n
            // message groups import their `virtual:i18n-facade/*` counterpart). Route
            // bare specifiers back through the plugin like the on-disk compile path
            // does, instead of leaving `virtual:...` for the browser to fetch and fail.
            if is_bare_specifier(s) {
                return Some(format!(
                    "/@id/{}?importer={}",
                    hex_encode(s),
                    hex_encode(&importer_id)
                ));
            }
            None
        };
        let source = interop_node_builtins(&source, Path::new("plugin.tsx")).unwrap_or(source);
        oj_compiler::compile_module(
            Path::new("plugin.tsx"),
            &source,
            &compile_opts,
            Some(&mut rewrite),
        )
        .map(|o| o.code_with_inline_map())
        .map_err(|e| format!("{e}"))
    })
    .await;
    match compiled {
        Ok(Ok(code)) => Some(
            (
                [
                    (header::CONTENT_TYPE, "text/javascript"),
                    (header::CACHE_CONTROL, "no-cache"),
                ],
                code,
            )
                .into_response(),
        ),
        _ => None,
    }
}

fn is_bare_specifier(spec: &str) -> bool {
    !spec.starts_with('.') && !spec.starts_with('/') && !spec.contains("://")
}

// The lingui macro entrypoints. Their transform is done by @lingui/swc-plugin
// (an SWC WASM plugin oj cannot run); left untransformed they drag the babel
// macro toolchain into the browser. oj serves a runtime identity shim instead.
fn is_lingui_macro_specifier(spec: &str) -> bool {
    matches!(
        spec,
        "@lingui/macro" | "@lingui/core/macro" | "@lingui/react/macro"
    )
}

// Whether a resolved dependency file is CommonJS (no ESM syntax), i.e. it will
// be served through wrap_cjs with `default` = module.exports. Used to decide
// whether a bare `import { x } from "cjs-dep"` needs importer-side interop
// (rewriting the named import to a property read off the default), since a CJS
// dep whose named exports are assigned at runtime (e.g. file-saver's `saveAs`)
// exposes no static ESM named bindings. Cached: a file's module kind is stable.
// A node_modules JS-family file that partial bundling should try to collapse
// into one `/@oj-pkg` bundle, whether it's CommonJS or ESM. (`.css`/`.json`/asset
// deps stay per-file; the builder itself falls back if a JS package can't be
// bundled safely.)
fn is_bundleable_dep_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("js" | "cjs" | "jsx" | "mjs")
    )
}

fn is_cjs_dep_file(path: &Path) -> bool {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<PathBuf, bool>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(&v) = cache.lock().unwrap().get(path) {
        return v;
    }
    // Only JavaScript files can be CommonJS. A `.css`/`.json`/asset dep has no
    // ES-module syntax either, but it is not CJS — treating it as one routes it
    // to the CJS interop / package-bundle path and serves raw CSS as JS.
    let is_js = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("js" | "cjs" | "jsx")
    );
    let v = is_js
        && match std::fs::read_to_string(path) {
            Ok(src) => !oj_compiler::cjs::has_module_syntax_pub(path, &src),
            Err(_) => false,
        };
    cache.lock().unwrap().insert(path.to_path_buf(), v);
    v
}

fn warn_lingui_macro_shim_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "oj: @lingui/*/macro is served by a runtime identity shim (i18n renders \
             source strings, no catalog lookup). oj cannot run @lingui/swc-plugin, \
             which is what normally compiles these macros."
        );
    });
}

fn hex_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() * 2);
    for b in s.bytes() {
        out.push(char::from_digit((b >> 4) as u32, 16).unwrap());
        out.push(char::from_digit((b & 0xf) as u32, 16).unwrap());
    }
    out
}

fn hex_decode(s: &str) -> Option<String> {
    let bytes = s.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    String::from_utf8(out).ok()
}

// oj hex-encodes its own /@id/ links, but a Vite plugin's client entry ships a
// raw /@id/<id> URL (Vite's convention: \0 shown as __x00__). Decode hex when the
// segment is valid hex, else fall back to the raw id so both forms resolve.
fn decode_at_id(seg: &str) -> String {
    if let Some(s) = hex_decode(seg) {
        return s;
    }
    urldecode(seg).replace("__x00__", "\0")
}

fn is_asset_ext(ext: &str) -> bool {
    // A plain `.wasm` import is served as a URL module (Vite asks for `?init`).
    oj_compiler::assets::is_asset_ext(ext) || ext.eq_ignore_ascii_case("wasm")
}

fn is_asset_path(file: &Path) -> bool {
    file.extension()
        .and_then(|e| e.to_str())
        .map(is_asset_ext)
        .unwrap_or(false)
}

fn content_type(ext: &str) -> &'static str {
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" | "mjs" | "cjs" => "text/javascript",
        "css" => "text/css",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "wasm" => "application/wasm",
        "woff2" => "font/woff2",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "txt" | "map2" => "text/plain; charset=utf-8",
        // Any other known asset type (case-insensitive), else octet-stream.
        other => oj_compiler::assets::asset_mime(other),
    }
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
}

/// The href a graph module is preloaded under: the exact URL its importer names,
/// so the preload and the import share one cache entry. A stylesheet is the
/// `?import` module; an optimized dep or package bundle carries the same
/// `?v=<version>` its import URLs do (the graph keys them without the query).
fn preload_href(path: &str, version: &str) -> String {
    if is_style_url(path) {
        format!("{path}?import")
    } else if !version.is_empty()
        && (path.starts_with("/@oj-deps/") || path.starts_with(pkg_bundle::PKG_PREFIX))
    {
        format!("{path}?v={version}")
    } else {
        path.to_string()
    }
}

fn inject_module_preloads(html: String, state: &ServerState) -> String {
    let paths: Vec<String> = if *state.crawl_done.borrow() {
        state
            .graph
            .lock()
            .unwrap()
            .module_paths()
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    } else {
        state.preload_snapshot.clone()
    };
    if paths.is_empty() {
        return html;
    }
    let version = state.optimized.version();
    let links: String = paths
        .iter()
        .map(|p| {
            format!(
                "<link rel=\"modulepreload\" href=\"{}\" />\n",
                preload_href(p, version)
            )
        })
        .collect();
    match html.find("</head>") {
        Some(idx) => format!("{}{links}{}", &html[..idx], &html[idx..]),
        None => format!("{html}\n{links}"),
    }
}

// The single gate for serving an absolute (`/@fs`) path. Decide on the
// canonical target so neither `..` traversal nor a symlink can escape an
// allow-listed root (component-wise `starts_with` on a raw path does not
// collapse `..`, and does not follow symlinks): require the real path to be
// inside a root and not denied. A path that cannot be canonicalized (missing,
// or a broken symlink) is refused. On success, return the ORIGINAL candidate,
// not the canonical path, so a caller running with `preserveSymlinks` keeps the
// module identity it asked for; both resolve to the same bytes.
fn fs_gate(state: &ServerState, candidate: &Path) -> Option<PathBuf> {
    let real = std::fs::canonicalize(candidate).ok()?;
    // Vite's isFileLoadingAllowed: `server.fs.strict: false` skips the allow
    // list entirely (the deny list below still applies).
    let allowed = !state.fs_strict || {
        let allow = state.fs_allow.lock().unwrap();
        allow.iter().any(|root| {
            // Fast path: roots are normally already canonical (the resolver
            // realpaths them). Fall back to canonicalizing the root so a
            // symlinked or /var-vs-/private/var root still matches.
            real.starts_with(root)
                || std::fs::canonicalize(root)
                    .map(|r| real.starts_with(r))
                    .unwrap_or(false)
        })
    };
    if !allowed || path_is_denied(&real, &state.root, &state.fs_deny) {
        return None;
    }
    Some(candidate.to_path_buf())
}

fn urldecode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(h), Some(l)) = (hex_nibble(bytes[i + 1]), hex_nibble(bytes[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn html_entry_src(src: &str) -> Option<String> {
    let s = src.trim();
    if s.is_empty()
        || s.starts_with("http://")
        || s.starts_with("https://")
        || s.starts_with("//")
        || s.starts_with("data:")
    {
        return None;
    }
    let s = s.strip_prefix("./").unwrap_or(s);
    Some(if s.starts_with('/') {
        s.to_string()
    } else {
        format!("/{s}")
    })
}

fn html_entries(root: &Path) -> Vec<String> {
    let Ok(html) = std::fs::read_to_string(root.join("index.html")) else {
        return Vec::new();
    };
    let mut entries = Vec::new();
    for tag_start in html.match_indices("<script").map(|(i, _)| i) {
        let Some(tag_end) = html[tag_start..].find('>') else {
            continue;
        };
        let tag = &html[tag_start..tag_start + tag_end];
        if !html_tag_attr(tag, "type").is_some_and(|t| t.eq_ignore_ascii_case("module")) {
            continue;
        }
        if let Some(entry) = html_tag_attr(tag, "src").and_then(html_entry_src) {
            entries.push(entry);
        }
    }
    entries
}

/// The value of attribute `name` in an opening tag (`<script type=...`),
/// whether double-quoted, single-quoted or unquoted, with optional spaces
/// around `=`; attribute names match case-insensitively and as whole words
/// (`data-src` is not `src`).
fn html_tag_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut i = tag.find(char::is_whitespace)?;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let start = i;
        while i < bytes.len()
            && !bytes[i].is_ascii_whitespace()
            && bytes[i] != b'='
            && bytes[i] != b'/'
        {
            i += 1;
        }
        if i == start {
            i += 1;
            continue;
        }
        let attr = &tag[start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let (vs, ve) = if matches!(bytes[i], b'"' | b'\'') {
            let q = bytes[i];
            let s = i + 1;
            i = s;
            while i < bytes.len() && bytes[i] != q {
                i += 1;
            }
            let e = i;
            i += usize::from(i < bytes.len());
            (s, e)
        } else {
            let s = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            (s, i)
        };
        if attr.eq_ignore_ascii_case(name) {
            return Some(&tag[vs..ve]);
        }
    }
    None
}

fn spawn_crawl(state: Arc<ServerState>, done_tx: tokio::sync::watch::Sender<bool>) {
    tokio::spawn(async move {
        let started = Instant::now();
        let mut visited: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut queue: Vec<String> = html_entries(&state.root);
        let mut tasks = tokio::task::JoinSet::new();

        loop {
            for url in queue.drain(..) {
                if !visited.insert(url.clone()) {
                    continue;
                }
                let file = if let Some(abs) = url.strip_prefix("/@fs") {
                    let f = PathBuf::from(abs);
                    let ok = {
                        let a = state.fs_allow.lock().unwrap();
                        a.iter().any(|r| f.starts_with(r))
                    };
                    if !ok {
                        continue;
                    }
                    f
                } else {
                    let rel = url.trim_start_matches('/').to_string();
                    match locate(&state.root, state.public_dir.as_deref(), &rel) {
                        Some(f) => f,
                        None => continue,
                    }
                };
                let ext = file.extension().and_then(|e| e.to_str()).unwrap_or("");
                if !(COMPILABLE.contains(&ext) || is_style_ext(ext) || ext == "json") {
                    continue;
                }
                let state = Arc::clone(&state);
                tasks.spawn(async move {
                    match ensure_module(&state, &file, &url).await {
                        Ok((_, module)) => module.imports.clone(),
                        Err(err) => {
                            eprintln!("oj: crawl: {err}");
                            Vec::new()
                        }
                    }
                });
            }
            match tasks.join_next().await {
                None => break,
                Some(imports) => {
                    for import in imports.unwrap_or_default() {
                        let import = import.split('?').next().unwrap_or(&import).to_string();
                        if import.starts_with('/')
                            && !import.starts_with("/@oj/")
                            && !visited.contains(&import)
                        {
                            queue.push(import);
                        }
                    }
                }
            }
        }

        let paths = state.graph.lock().unwrap().module_paths();
        println!(
            "{} eager graph ready: {} modules in {:?}",
            oj_tag(),
            paths.len(),
            started.elapsed()
        );
        save_graph_snapshot(&state.root, &paths);
        let _ = done_tx.send(true);
    });
}

fn snapshot_path(root: &Path) -> PathBuf {
    oj_cache::cache_root(root).join("graph-snapshot.json")
}

fn load_graph_snapshot(root: &Path) -> Vec<String> {
    std::fs::read(snapshot_path(root))
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

fn save_graph_snapshot(root: &Path, paths: &[PathBuf]) {
    let urls: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
    let path = snapshot_path(root);
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(path, serde_json::to_vec(&urls).unwrap_or_default());
}

struct HmrGate {
    full_reload: bool,
    max_hold: Duration,
    inner: Mutex<GateInner>,
    /// A page reload the Start server is holding for the flush (the editor's
    /// gate plugin's `heldReload`: a bundled dev server's reload is one event,
    /// not a set of hot updates).
    held_reload: std::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct GateInner {
    pending: std::collections::BTreeMap<PathBuf, std::collections::BTreeSet<String>>,
    generation: u64,
}

fn gate_relevant(path: &Path) -> bool {
    !path.components().any(|c| {
        let c = c.as_os_str();
        c == "node_modules" || c == ".oj-cache" || c == "dist"
    })
}

impl HmrGate {
    fn hold(&self, state: &Arc<ServerState>, paths: &[PathBuf]) -> bool {
        let relevant: Vec<&PathBuf> = paths.iter().filter(|p| gate_relevant(p)).collect();
        if relevant.is_empty() {
            return false;
        }
        let mut inner = self.inner.lock().unwrap();
        let was_empty = inner.pending.is_empty();
        for p in relevant {
            inner
                .pending
                .entry(p.clone())
                .or_default()
                .insert("change".to_string());
        }
        if was_empty {
            inner.generation += 1;
            let generation = inner.generation;
            let state = Arc::clone(state);
            let max_hold = self.max_hold;
            let rt = state.rt.clone();
            rt.spawn(async move {
                tokio::time::sleep(max_hold).await;
                if let Some(gate) = &state.hmr_gate {
                    let expired = {
                        let g = gate.inner.lock().unwrap();
                        g.generation == generation && !g.pending.is_empty()
                    };
                    if expired {
                        gate.flush(&state).await;
                    }
                }
            });
        }
        true
    }

    async fn flush(&self, state: &Arc<ServerState>) -> (Vec<String>, usize) {
        let entries: Vec<(PathBuf, std::collections::BTreeSet<String>)> = {
            let mut inner = self.inner.lock().unwrap();
            inner.generation += 1;
            std::mem::take(&mut inner.pending).into_iter().collect()
        };
        let files: Vec<String> = entries
            .iter()
            .map(|(p, _)| p.display().to_string())
            .collect();
        let count = entries.len();
        let held_reload = self
            .held_reload
            .swap(false, std::sync::atomic::Ordering::SeqCst);
        if held_reload || !entries.is_empty() {
            let _ = state.gate_flush_tx.send(());
        }
        if !entries.is_empty() {
            state.dir_cache.lock().unwrap().clear();
            if self.full_reload {
                let _ = state
                    .reload_tx
                    .send(full_reload_frame("hmr-flush", None, None));
            } else {
                let paths: Vec<PathBuf> = entries.into_iter().map(|(p, _)| p).collect();
                let sref: &ServerState = state;
                for message in decide(sref, &paths, &Default::default()).await {
                    let _ = state.reload_tx.send(message);
                }
            }
        }
        (files, count)
    }

    fn mode(&self) -> &'static str {
        if self.full_reload {
            "full-reload"
        } else {
            "granular"
        }
    }

    fn status(&self, state: &ServerState) -> serde_json::Value {
        let inner = self.inner.lock().unwrap();
        let mut pending = serde_json::Map::new();
        for (p, events) in &inner.pending {
            pending.insert(
                p.display().to_string(),
                serde_json::json!(events.iter().collect::<Vec<_>>()),
            );
        }
        let held_reload = self.held_reload.load(std::sync::atomic::Ordering::SeqCst);
        serde_json::json!({
            "enabled": true,
            "pending": pending,
            "count": inner.pending.len(),
            "mode": self.mode(),
            "heldReload": held_reload,
            "startedAt": state.started_at_ms,
        })
    }
}

/// The HMR gate as the Start server sees it: hold the page reload a rebuild
/// would send until the editor flushes (`POST /__hmr_flush`) or the hold cap
/// releases it, like the editor's Vite gate plugin holds a bundled dev server's
/// `full-reload`. Without it every write under `src/` reloaded the preview at
/// once, gate or not.
#[derive(Clone)]
pub struct HmrGateHandle {
    state: Arc<ServerState>,
}

impl HmrGateHandle {
    /// Record the changed paths and hold the reload. False when nothing relevant
    /// changed (build output, caches), in which case the caller reloads now.
    pub fn hold_reload(&self, paths: &[PathBuf]) -> bool {
        let Some(gate) = &self.state.hmr_gate else {
            return false;
        };
        if !gate.hold(&self.state, paths) {
            return false;
        }
        gate.held_reload
            .store(true, std::sync::atomic::Ordering::SeqCst);
        true
    }

    /// Fires once per flush that released something.
    pub fn subscribe_flush(&self) -> broadcast::Receiver<()> {
        self.state.gate_flush_tx.subscribe()
    }
}

async fn hmr_flush(State(state): State<Arc<ServerState>>) -> Response {
    let Some(gate) = &state.hmr_gate else {
        return js_response_json(
            serde_json::json!({ "flushed": [], "count": 0, "mode": "disabled" }),
        );
    };
    let held_reload = gate.held_reload.load(std::sync::atomic::Ordering::SeqCst);
    let (files, count) = gate.flush(&state).await;
    js_response_json(
        serde_json::json!({ "flushed": files, "count": count, "mode": gate.mode(), "reload": held_reload || count > 0 }),
    )
}

async fn hmr_gate_status(State(state): State<Arc<ServerState>>) -> Response {
    match &state.hmr_gate {
        Some(gate) => js_response_json(gate.status(&state)),
        None => js_response_json(serde_json::json!({ "enabled": false })),
    }
}

/// Which paths of a watcher event count as a content change, by chokidar's
/// rule: the data changed, or the modification time moved since this watcher
/// last saw the file. An attribute-only event whose mtime is unchanged (or the
/// first such event for a path, with nothing to compare against) is not a
/// change. On Linux the first read of a file after it was written updates its
/// atime under relatime, and inotify reports that as an attribute change, so a
/// rebuild that reads every source file looked like an edit of every source
/// file and triggered another rebuild, until the atimes settled.
pub struct ContentChanges {
    mtimes: std::collections::HashMap<PathBuf, std::time::SystemTime>,
}

impl Default for ContentChanges {
    fn default() -> Self {
        Self::new()
    }
}

impl ContentChanges {
    pub fn new() -> Self {
        Self {
            mtimes: std::collections::HashMap::new(),
        }
    }

    pub fn changed_paths(&mut self, ev: &notify::Event) -> Vec<PathBuf> {
        match &ev.kind {
            notify::EventKind::Access(_) => Vec::new(),
            notify::EventKind::Modify(notify::event::ModifyKind::Metadata(_)) => ev
                .paths
                .iter()
                .filter(|p| self.mtime_moved(p))
                .cloned()
                .collect(),
            _ => {
                for p in &ev.paths {
                    if let Ok(mtime) = std::fs::metadata(p).and_then(|m| m.modified()) {
                        self.mtimes.insert(p.clone(), mtime);
                    }
                }
                ev.paths.clone()
            }
        }
    }

    fn mtime_moved(&mut self, p: &Path) -> bool {
        let Ok(mtime) = std::fs::metadata(p).and_then(|m| m.modified()) else {
            self.mtimes.remove(p);
            return true;
        };
        match self.mtimes.insert(p.to_path_buf(), mtime) {
            Some(prev) => prev != mtime,
            // No baseline to compare against (there is no initial scan): a
            // fresh mtime is a touch/utimes change that must count once, an old
            // one is the relatime atime noise this filter exists to ignore.
            None => mtime
                .elapsed()
                .map(|age| age < std::time::Duration::from_secs(10))
                .unwrap_or(true),
        }
    }
}

/// True for config / env files whose change requires a full server restart
/// (they are read once at startup and cannot be hot-applied).
fn is_restart_trigger(path: &Path) -> bool {
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

/// Re-exec the current binary with the same arguments so a fresh process
/// re-reads config and .env. Rust sets CLOEXEC on the listening socket, so the
/// dev port is released as the image is replaced. Does not return on success.
fn restart_process() -> ! {
    eprintln!("{} config/env changed — restarting dev server", oj_brand());
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
static STARTUP_CWD: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Call once at process start, before any plugin host or engine exists.
pub fn capture_startup_cwd() {
    if let Ok(dir) = std::env::current_dir() {
        let _ = STARTUP_CWD.set(dir);
    }
}

fn spawn_watcher(state: Arc<ServerState>) {
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

// Async because it is reached both from the watcher thread (via block_on) and
// from the async /__hmr_flush handler; using block_on here panicked ("runtime
// within a runtime") when the gate flushed on an async worker thread.
fn parse_hmr_filter(raw: &str) -> Option<Vec<PathBuf>> {
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    if v.get("action")?.as_str()? != "filter" {
        return None;
    }
    let arr = v.get("modules")?.as_array()?;
    Some(
        arr.iter()
            .filter_map(|m| m.as_str().map(PathBuf::from))
            .collect(),
    )
}

/// `created`: the paths the watcher reported as newly created among `paths`
/// (the rest are edits, or removals when the file is gone).
async fn decide(
    state: &ServerState,
    paths: &[PathBuf],
    created: &std::collections::HashSet<PathBuf>,
) -> Vec<String> {
    if !state.hmr_enabled {
        return Vec::new();
    }
    let mut messages: Vec<String> = Vec::new();
    let mut updates: Vec<serde_json::Value> = Vec::new();

    let plugin_watched: std::collections::HashSet<PathBuf> = {
        let mut raw: Vec<String> = match &state.plugins {
            Some(host) => host.watch_files().await.unwrap_or_default(),
            None => Vec::new(),
        };
        raw.extend(
            state
                .plugin_watched
                .lock()
                .unwrap()
                .iter()
                .map(|p| p.to_string_lossy().into_owned()),
        );
        raw.into_iter()
            .map(|p| std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(p)))
            .collect()
    };

    let source_changed = paths.iter().any(|p| {
        !p.components().any(|c| {
            let c = c.as_os_str();
            c == "node_modules" || c == ".oj-cache" || c == "dist"
        }) && p
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| COMPILABLE.contains(&e))
    });
    if source_changed {
        let timestamp = now_millis() as u64;
        for url in state.tailwind_urls.lock().unwrap().iter() {
            updates.push(update_entry("css-update", url, timestamp));
        }
    }

    // A file appearing on disk may be the one a module failed to import: Vite
    // adds `_hasResolveFailedErrorModules` to a `create` event's module set so
    // those importers are re-processed (hmr.ts). notify does not tell a create
    // from a modify reliably, so a file that exists but is not in the graph is
    // taken as new; the importers then go through the loop as changed files.
    let mut paths: Vec<PathBuf> = paths.to_vec();
    let new_file = paths.iter().any(|p| {
        p.is_file()
            && !state
                .graph
                .lock()
                .unwrap()
                .contains(Path::new(&url_of(&state.root, p)))
    });
    if new_file {
        // The resolver caches misses too: without this a created `./dir/index.ts`
        // or extension-probed file stays "not found" for the importer's retry.
        state.resolver.clear_cache();
        state.ssr_resolver.clear_cache();
        let failed: Vec<String> = state.resolve_failed.lock().unwrap().drain().collect();
        for url in failed {
            paths.push(state.root.join(url.trim_start_matches('/')));
        }
    }
    // A file created or deleted under an `import.meta.glob` pattern changes
    // what the importer expands to: recompile and update the importer as if it
    // had been edited (Vite's importMetaGlob hotUpdate on create/delete).
    let glob_importers: Vec<String> = {
        let globs = state.glob_importers.lock().unwrap();
        if globs.is_empty() {
            Vec::new()
        } else {
            let graph = state.graph.lock().unwrap();
            // `*` stops at `/`, as the directory walk expanding the glob does.
            let opts = glob::MatchOptions {
                require_literal_separator: true,
                ..Default::default()
            };
            let mut hit: Vec<String> = Vec::new();
            for p in &paths {
                let known = graph.contains(Path::new(&url_of(&state.root, p)));
                let added_or_removed = !p.exists() || !known;
                if !added_or_removed {
                    continue;
                }
                for (importer, patterns) in globs.iter() {
                    if patterns.iter().any(|pat| pat.matches_path_with(p, opts))
                        && !hit.contains(importer)
                    {
                        hit.push(importer.clone());
                    }
                }
            }
            hit
        }
    };
    for importer in glob_importers {
        println!("oj: glob importer {importer} re-expanded");
        state.mtime_keys.lock().unwrap().remove(&importer);
        state.memory.lock().unwrap().remove(&importer);
        let file = state.root.join(importer.trim_start_matches('/'));
        if !paths.contains(&file) {
            paths.push(file);
        }
    }

    for path in &paths {
        // Vite's default watch ignores: `**/node_modules/**` and `**/.git/**`
        // at any depth (a nested package's node_modules included).
        if path.components().any(|c| {
            let c = c.as_os_str();
            c == "node_modules" || c == ".oj-cache" || c == "dist" || c == ".git"
        }) {
            continue;
        }

        if let Some(host) = &state.plugins {
            let file = path.display().to_string();
            // Vite's watcher hands plugins the change kind (hmr.ts HotUpdateOptions
            // type): a new file is "create" (chokidar add), an edit "update", and
            // a removed file still reaches watchChange / hotUpdate as "delete".
            let change_type = if !path.exists() {
                "delete"
            } else if created.contains(path)
                && !state
                    .graph
                    .lock()
                    .unwrap()
                    .contains(Path::new(&url_of(&state.root, path)))
            {
                // Newly created and never served: a file the graph already holds
                // was only rewritten (editors that replace files on save).
                "create"
            } else {
                "update"
            };
            // The ssr environment's plugin instances (Vite dispatches hotUpdate
            // and watchChange to every environment) when that host is up.
            let ssr_host = state.plugins_ssr.get().and_then(|h| h.clone());
            // Pre-init fast-skip: this dispatch path is serial, and each hook
            // toward a still-initializing lazy host would await a full
            // per-call init window — on a wedged init that froze every save's
            // HMR for 2× the window, forever. Skip both hooks and queue a
            // watchChange catch-up instead (replayed at the host's init by
            // spawn_ssr_watch_catch_up); a healthy slow boot still gets
            // post-init events normally. The re-check after queuing closes
            // the race where init lands between the decision and the push —
            // the catch-up task may have already drained.
            let ssr_host = match ssr_host {
                Some(ssr)
                    if !ssr.is_initialized()
                        && (state.plugins_watch_change || state.plugins_hot_update) =>
                {
                    note_ssr_watch_skip(&state.ssr_watch, &file, change_type);
                    if ssr.is_initialized() {
                        replay_ssr_watch_backlog(&ssr, &state.ssr_watch).await;
                    }
                    None
                }
                Some(ssr) if state.plugins_watch_change || state.plugins_hot_update => {
                    // Initialized: flush any queued catch-up events FIRST —
                    // under the queue's order lock, blocking while the
                    // catch-up task is mid-replay — so a stale queued
                    // watchChange can never land after this newer live event
                    // for the same file. Empty-queue cost is one lock check.
                    replay_ssr_watch_backlog(&ssr, &state.ssr_watch).await;
                    Some(ssr)
                }
                other => other,
            };
            if state.plugins_watch_change {
                if let Err(e) = host.watch_change(&file, change_type).await {
                    eprintln!("oj: watchChange failed for {file}: {e}");
                }
                if let Some(ssr) = &ssr_host {
                    if let Err(e) = ssr.watch_change(&file, change_type).await {
                        eprintln!("oj: watchChange (ssr) failed for {file}: {e}");
                    }
                }
            }
            if state.plugins_hot_update {
                let ts = now_millis() as u64;
                let hmr_url = url_of(&state.root, path);
                let modules_json = {
                    let g = state.graph.lock().unwrap();
                    match g.node(Path::new(&hmr_url)) {
                        Some(n) => serde_json::json!([{
                            "url": hmr_url,
                            "id": hmr_url,
                            "isSelfAccepting": n.is_self_accepting,
                            "importers": n
                                .importers
                                .iter()
                                .map(|p| p.display().to_string())
                                .collect::<Vec<_>>(),
                        }])
                        .to_string(),
                        None => "[]".to_string(),
                    }
                };
                if let Some(ssr) = &ssr_host {
                    // Its result steers no client update (the ssr environment has
                    // no browser); only a throwing hook is worth reporting.
                    if let Err(e) = ssr
                        .handle_hot_update(&file, ts, change_type, &modules_json)
                        .await
                    {
                        eprintln!("oj: hotUpdate (ssr) failed for {file}: {e}");
                    }
                }
                match host
                    .handle_hot_update(&file, ts, change_type, &modules_json)
                    .await
                {
                    // Vite (hmr.ts): a throwing hotUpdate is logged and sent to
                    // the client as an error payload (the overlay), and no update
                    // is dispatched for that file.
                    Err(e) => {
                        eprintln!("oj: hotUpdate failed for {file}: {e}");
                        messages.push(error_frame(&e));
                        continue;
                    }
                    Ok(Some(d)) if d == "skip" => {
                        println!("oj: change {file} -> HMR suppressed by plugin");
                        continue;
                    }
                    Ok(Some(d)) if d == "full-reload" => {
                        println!("oj: change {file} -> full-reload (plugin)");
                        messages.push(full_reload_frame("plugin", None, Some(path)));
                        return messages;
                    }
                    Ok(Some(d)) => {
                        if let Some(seeds) = parse_hmr_filter(&d) {
                            let seed_refs: Vec<&Path> =
                                seeds.iter().map(PathBuf::as_path).collect();
                            let decision =
                                state.graph.lock().unwrap().propagate_from_seeds(&seed_refs);
                            match decision {
                                HmrDecision::Update { boundaries } => {
                                    println!(
                                        "oj: change {file} -> plugin-filtered update {boundaries:?}"
                                    );
                                    let timestamp = now_millis() as u64;
                                    updates.extend(boundaries.iter().map(|b| {
                                        let mut p = format!("{}", b.display());
                                        if is_style_url(&p) {
                                            p.push_str("?import");
                                        }
                                        update_entry("js-update", &p, timestamp)
                                    }));
                                    continue;
                                }
                                HmrDecision::FullReload { reason } => {
                                    println!("oj: change {file} -> full-reload ({reason})");
                                    messages.push(full_reload_frame(&reason, None, Some(path)));
                                    return messages;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        if !plugin_watched.is_empty() {
            let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
            if plugin_watched.contains(&canon) {
                println!(
                    "oj: change {} -> full-reload (plugin watch)",
                    path.display()
                );
                messages.push(full_reload_frame("plugin-watch", None, Some(path)));
                return messages;
            }
        }

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if is_style_ext(ext) {
            let url = url_of(&state.root, path);
            // A stylesheet nothing imports is loaded by a `<link>` (serving it
            // compiled registers it in the graph, with no importers): swap the
            // link rather than dispatching a JS update it has no handler for.
            let link_loaded = {
                let g = state.graph.lock().unwrap();
                match g.node(Path::new(&url)) {
                    None => true,
                    Some(n) => n.importers.is_empty(),
                }
            };
            if link_loaded {
                println!("oj: change {url} -> css-update");
                updates.push(update_entry("css-update", &url, now_millis() as u64));
                continue;
            }
        }
        if ext == "html" {
            // Vite names the edited page (`path: '/about.html'`) so only the tab
            // showing it reloads; the reason keeps the absolute path oj's own
            // tooling reads.
            let page = url_of(&state.root, path);
            println!("oj: change {} -> full-reload", path.display());
            messages.push(full_reload_frame(
                &path.display().to_string(),
                Some(&page),
                Some(path),
            ));
            return messages;
        }
        if !(COMPILABLE.contains(&ext) || is_style_ext(ext) || ext == "json") {
            continue;
        }

        let url = url_of(&state.root, path);
        if !state.graph.lock().unwrap().contains(Path::new(&url)) {
            continue;
        }
        let targets = state.graph.lock().unwrap().update_targets(Path::new(&url));
        match targets {
            Ok(targets) => {
                let boundaries: Vec<&Path> = targets.iter().map(|t| t.boundary.as_path()).collect();
                println!("oj: change {url} -> update {boundaries:?}");
                let timestamp = now_millis() as u64;
                // Stamp the invalidated chain so re-fetched importers point at the
                // new versions of their (unchanged-on-disk) dependencies, and drop
                // those importers' mtime fast-path keys so they recompile with the
                // stamps rather than serving the cached code.
                let dirty = state
                    .graph
                    .lock()
                    .unwrap()
                    .stamp_update(Path::new(&url), timestamp);
                {
                    let mut keys = state.mtime_keys.lock().unwrap();
                    for d in &dirty {
                        keys.remove(&d.display().to_string());
                    }
                }
                if targets.is_empty() {
                    println!("oj: change {url} -> no update (nothing loaded imports it)");
                }
                updates.extend(targets.iter().map(|t| update_entry_for(t, timestamp, None)));
            }
            Err(reason) => {
                println!("oj: change {url} -> full-reload ({reason})");
                messages.push(full_reload_frame(&reason, None, Some(path)));
                return messages;
            }
        }
    }

    if !updates.is_empty() {
        messages.push(serde_json::json!({ "type": "update", "updates": updates }).to_string());
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    // The cached body reaches the wire verbatim and the header tuple
    // *overwrites* the `application/octet-stream` that `Bytes` sets (insert,
    // not append), so content-type stays a single text/javascript.
    #[tokio::test]
    async fn cached_js_response_serves_bytes_verbatim_with_text_javascript() {
        let body = Bytes::from("console.log(1)".to_string());
        let resp = cached_js_response(&HeaderMap::new(), "\"e\"".to_string(), body.clone());
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/javascript");
        assert_eq!(
            resp.headers().get_all(header::CONTENT_TYPE).iter().count(),
            1
        );
        assert_eq!(resp.headers()[header::CACHE_CONTROL], "no-cache");
        assert_eq!(resp.headers()[header::ETAG], "\"e\"");
        let got = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(got, body);
    }

    #[tokio::test]
    async fn cached_js_response_304_on_matching_etag_has_empty_body() {
        let mut headers = HeaderMap::new();
        headers.insert(header::IF_NONE_MATCH, "\"e\"".parse().unwrap());
        let resp = cached_js_response(&headers, "\"e\"".to_string(), Bytes::from_static(b"x"));
        assert_eq!(resp.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(resp.headers()[header::ETAG], "\"e\"");
        let got = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert!(got.is_empty());
    }

    // The watcher's pre-init fast-skip toward the lazy SSR host: recording a
    // skipped event is instant (the watcher path makes NO RPC toward a
    // still-initializing host — dispatching used to serially burn a full
    // per-call init window per hook per save on a wedged init), the backlog
    // dedups by file keeping the latest change type, and once the host
    // initializes the catch-up task replays the backlog as watchChange.
    #[tokio::test]
    async fn ssr_watch_skip_is_instant_and_replays_at_the_hosts_init() {
        let root = std::env::temp_dir().join(format!("oj-ssr-watch-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();

        // A wedged lazy host: skips must complete in milliseconds anyway.
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(
            &plugins,
            "setInterval(() => {}, 1000);\nawait new Promise(() => {});\nexport default [];\n",
        )
        .unwrap();
        let wedged = match plugins::PluginHost::spawn_lazy_with_wait(
            &root,
            &plugins,
            &config,
            std::time::Duration::from_secs(5),
        )
        .await
        {
            Ok(h) => h,
            Err(_) => return, // no node on this machine
        };
        let queue = Arc::new(SsrWatchQueue::default());
        assert!(!wedged.is_initialized());
        let t0 = std::time::Instant::now();
        // What decide() does per save while the host is pre-init.
        note_ssr_watch_skip(&queue, "/app/a.ts", "update");
        note_ssr_watch_skip(&queue, "/app/a.ts", "delete");
        note_ssr_watch_skip(&queue, "/app/b.ts", "update");
        assert!(
            t0.elapsed() < std::time::Duration::from_millis(200),
            "a skipped dispatch never waits on the host: {:?}",
            t0.elapsed()
        );
        assert_eq!(
            *queue.backlog.lock().unwrap(),
            vec![
                ("/app/a.ts".to_string(), "delete".to_string()),
                ("/app/b.ts".to_string(), "update".to_string()),
            ],
            "deduped by file, latest change type wins"
        );
        wedged.shutdown();

        // A healthy slow host: the catch-up task replays the backlog at init.
        std::fs::write(
            &plugins,
            "await new Promise((r) => setTimeout(r, 1000));\nexport default [];\n",
        )
        .unwrap();
        let host = match plugins::PluginHost::spawn_lazy_with_wait(
            &root,
            &plugins,
            &config,
            std::time::Duration::from_secs(30),
        )
        .await
        {
            Ok(h) => h,
            Err(_) => return,
        };
        spawn_ssr_watch_catch_up(std::sync::Arc::clone(&host), Arc::clone(&queue));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !queue.backlog.lock().unwrap().is_empty() {
            assert!(
                std::time::Instant::now() < deadline,
                "the backlog must drain once the host initializes"
            );
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(host.is_initialized(), "the replay only runs post-init");
        host.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    // The ordering guarantee: every dispatch toward the SSR host serializes
    // on the queue's order lock with the backlog drained first, so a queued
    // (older) watchChange always reaches the host BEFORE a live (newer) event
    // for the same file — even when the live dispatch races the catch-up
    // task mid-replay. The host-side plugin logs arrivals; the log's last
    // line must be the live event.
    #[tokio::test]
    async fn ssr_watch_catch_up_events_land_before_a_racing_live_event() {
        let root = std::env::temp_dir().join(format!("oj-ssr-watch-order-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let log = root.join("watch.log");
        // A slow-boot host whose watchChange hook is slow and logs arrivals:
        // the live dispatch below races the catch-up task mid-replay.
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(
            &plugins,
            format!(
                r#"import {{ appendFileSync }} from "node:fs";
await new Promise((r) => setTimeout(r, 800));
export default [{{
  name: "watch-logger",
  async watchChange(id, change) {{
    await new Promise((r) => setTimeout(r, 150));
    appendFileSync({log:?}, id + "|" + ((change && change.event) || "?") + "\n");
  }},
}}];
"#,
                log = log.display().to_string(),
            ),
        )
        .unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();
        let host = match plugins::PluginHost::spawn_lazy_with_wait(
            &root,
            &plugins,
            &config,
            std::time::Duration::from_secs(30),
        )
        .await
        {
            Ok(h) => h,
            Err(_) => return, // no node on this machine
        };
        let queue = Arc::new(SsrWatchQueue::default());
        note_ssr_watch_skip(&queue, "/app/a.ts", "update");
        note_ssr_watch_skip(&queue, "/app/b.ts", "update");
        spawn_ssr_watch_catch_up(std::sync::Arc::clone(&host), Arc::clone(&queue));
        // Wait out init, then dispatch a LIVE event while the catch-up task
        // is (very likely) mid-replay — the order lock, not luck, is what
        // guarantees the outcome under every interleaving.
        let mut init = host.initialized_updates();
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(20), init.wait_for(|v| *v),)
                .await
                .is_ok_and(|r| r.is_ok())
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // What decide() does for a live post-init event: flush, then send.
        replay_ssr_watch_backlog(&host, &queue).await;
        host.watch_change("/app/a.ts", "live")
            .await
            .expect("live dispatch reaches the host");
        let lines = std::fs::read_to_string(&log).unwrap_or_default();
        let lines: Vec<&str> = lines.lines().collect();
        assert_eq!(
            lines.last().copied(),
            Some("/app/a.ts|live"),
            "the live (newer) event lands LAST: {lines:?}"
        );
        assert!(
            lines.contains(&"/app/a.ts|update") && lines.contains(&"/app/b.ts|update"),
            "both queued events were replayed before it: {lines:?}"
        );
        host.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    // The late-activation resync ENQUEUE is confirmed only on an ACK:
    // transient failures are retried with backoff, and a middleware that never
    // acknowledges makes the helper report failure (the caller then warns
    // about stale edits) instead of logging progress over a resync that never
    // reached the queue. (Execution is a separate signal — see
    // resync_completion_is_claimed_on_the_done_signal_never_on_the_ack.)
    #[tokio::test]
    async fn resync_retries_until_the_middleware_acks() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let mut n = 0u32;
            loop {
                let Ok((mut sock, _)) = listener.accept().await else {
                    return;
                };
                n += 1;
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                // The first two attempts fail; the third is the real ACK.
                let resp = if n < 3 {
                    "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                } else {
                    "HTTP/1.1 204 No Content\r\nconnection: close\r\n\r\n"
                };
                let _ = sock.write_all(resp.as_bytes()).await;
            }
        });
        assert!(!notify_plugin_mw_resync(port).await, "a 500 is not an ACK");
        assert!(
            resync_plugin_mw_with_retry(port).await,
            "the retry loop must reach the eventual ACK"
        );
    }

    #[tokio::test]
    async fn resync_reports_failure_when_nothing_acks() {
        // A port with nothing listening: every attempt is refused.
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        assert!(!resync_plugin_mw_with_retry(port).await);
    }

    // The ack is 202-style ("enqueued"); "resynced" is claimed only when the
    // host's { ojResyncDone } completion signal moves the counter past the
    // pre-enqueue baseline. A busy queue completes late (still claimed), a
    // completion racing ahead of the wait is not missed (baseline semantics),
    // and a stuck queue times out into the caller's warning path.
    #[tokio::test]
    async fn resync_completion_is_claimed_on_the_done_signal_never_on_the_ack() {
        let (tx, mut rx) = tokio::sync::watch::channel(0u64);
        // Busy queue: the completion lands after the wait began.
        let baseline = *rx.borrow_and_update();
        let signal = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            tx.send_modify(|c| *c += 1);
            tx
        });
        assert!(
            await_resync_completion(&mut rx, baseline, std::time::Duration::from_secs(5)).await,
            "a late completion is still claimed"
        );
        let tx = signal.await.unwrap();

        // Fast queue: the completion already landed before the wait started —
        // the pre-enqueue baseline catches it.
        let baseline = *rx.borrow_and_update();
        tx.send_modify(|c| *c += 1);
        assert!(
            await_resync_completion(&mut rx, baseline, std::time::Duration::from_secs(5)).await,
            "a completion racing ahead of the wait is never missed"
        );

        // Stuck queue: no signal within the bound — the caller warns instead
        // of logging success off the enqueue ack.
        let baseline = *rx.borrow_and_update();
        assert!(
            !await_resync_completion(&mut rx, baseline, std::time::Duration::from_millis(50)).await,
            "a stuck queue must not be reported as resynced"
        );
    }

    // One packed word: every reader sees (port, runner_environments) from the
    // same write, and a late activation runs the handler BEFORE the flip is
    // visible — no reader can observe the new mode with the catch-up unarmed.
    #[test]
    fn plugin_serve_packs_one_snapshot_and_arms_before_the_flip() {
        let info = |port: Option<u16>, runner: bool| plugins::ServeInfo {
            middleware_port: port,
            runner_environments: runner,
        };
        let serve = PluginServe::default();
        assert_eq!(serve.mw_port(), None);
        assert!(!serve.runner_environments());

        // The handler observes the pre-flip state: set() runs it first.
        let seen = Arc::new(Mutex::new(None::<(Option<u16>, bool)>));
        {
            let serve = Arc::new(PluginServe::default());
            let inner = Arc::clone(&serve);
            let sink = Arc::clone(&seen);
            serve.set_on_activate(Box::new(move || {
                *sink.lock().unwrap() = Some((inner.mw_port(), inner.runner_environments()));
            }));
            assert!(!serve.activated_late());
            serve.set(&info(Some(4001), true));
            assert_eq!(
                *seen.lock().unwrap(),
                Some((None, false)),
                "the activation handler must run before readers can see the flip"
            );
            assert_eq!(serve.mw_port(), Some(4001));
            assert!(serve.runner_environments());
            // A registrar that lost the race to this activation can catch up.
            assert!(serve.activated_late());
            // A repeat set with the same info is not a second activation.
            *seen.lock().unwrap() = None;
            serve.set(&info(Some(4001), true));
            assert_eq!(*seen.lock().unwrap(), None);
        }

        // runner_environments only counts with a middleware port.
        let no_port = PluginServe::from_info(&info(None, true));
        assert_eq!(no_port.mw_port(), None);
        assert!(!no_port.runner_environments());
        let no_runner = PluginServe::from_info(&info(Some(4002), false));
        assert_eq!(no_runner.mw_port(), Some(4002));
        assert!(!no_runner.runner_environments());
        // The boot fill is not a late activation: a caller's post-registration
        // catch-up must not fire (and reload the runner) on every normal boot.
        assert!(!no_runner.activated_late());
    }

    #[test]
    fn dep_transform_gate_matches_only_marker_sources() {
        // The plugins' own transform code-filter patterns (getDepTransformFilters).
        let res: Vec<regex::Regex> = [
            r"\bcreateServerFn\b|\.\s*handler\s*\(",
            "createIsomorphicFn",
        ]
        .iter()
        .map(|s| regex::Regex::new(s).unwrap())
        .collect();
        let wants = |src: &str| res.iter().any(|re| re.is_match(src));
        assert!(wants(
            "export const f = createIsomorphicFn().client(() => 1)"
        ));
        assert!(wants("const x = createServerFn()"));
        assert!(wants("route.handler ( () => {} )"));
        assert!(!wants("export const x = 1;"));
        assert!(!wants(
            "import { getStartContext } from '@tanstack/start-storage-context'"
        ));
    }

    #[test]
    fn decode_at_id_handles_hex_and_raw_vite_ids() {
        assert_eq!(decode_at_id(&hex_encode("virtual:x")), "virtual:x");
        assert_eq!(
            decode_at_id("virtual:tanstack-start-dev-client-entry"),
            "virtual:tanstack-start-dev-client-entry"
        );
        assert_eq!(decode_at_id("__x00__virtual:foo"), "\0virtual:foo");
    }

    #[test]
    fn parse_hmr_filter_reads_filter_module_urls() {
        let seeds =
            parse_hmr_filter(r#"{"action":"filter","modules":["/src/a.tsx","/src/b.tsx"]}"#)
                .unwrap();
        assert_eq!(
            seeds,
            vec![PathBuf::from("/src/a.tsx"), PathBuf::from("/src/b.tsx")]
        );
    }

    #[test]
    fn parse_hmr_filter_ignores_non_filter_payloads() {
        assert!(parse_hmr_filter("skip").is_none());
        assert!(parse_hmr_filter("full-reload").is_none());
        assert!(parse_hmr_filter(r#"{"action":"reload"}"#).is_none());
        assert!(parse_hmr_filter(r#"{"action":"filter"}"#).is_none());
        assert!(parse_hmr_filter("not json at all").is_none());
    }

    #[tokio::test]
    async fn bind_dev_listener_increments_unless_strict() {
        use std::net::{IpAddr, Ipv4Addr};
        let host = IpAddr::V4(Ipv4Addr::LOCALHOST);
        // Hold an ephemeral port so the preferred one is busy.
        let occupied = tokio::net::TcpListener::bind((host, 0)).await.unwrap();
        let taken = occupied.local_addr().unwrap().port();

        // strict: a busy preferred port is a hard error, never moved.
        assert!(
            bind_dev_listener(host, taken, true).await.is_err(),
            "strict must reject a busy port",
        );

        // non-strict (Vite default): hop to the next free port.
        let (listener, port) = bind_dev_listener(host, taken, false).await.unwrap();
        assert_ne!(port, taken, "non-strict must pick a different port");
        assert_eq!(listener.local_addr().unwrap().port(), port);
    }

    // A stylesheet's `?inline` (compiled css string through the full pipeline,
    // not a data URI) is covered end to end by e2e/css-vite-parity.mjs, since
    // it runs the server's compile path.

    #[tokio::test]
    async fn non_css_inline_stays_a_data_uri() {
        let dir = tempfile::tempdir().unwrap();
        let png = dir.path().join("pixel.png");
        std::fs::write(&png, [0u8, 1, 2, 3]).unwrap();
        let out = asset_module(&png, "/pixel.png?inline", "inline")
            .await
            .unwrap();
        assert!(
            out.contains("data:"),
            "binary asset stays a data URI: {out}"
        );
    }

    #[test]
    fn postcss_config_is_found_like_postcss_load_config() {
        let base = std::env::temp_dir().join(format!("oj-postcss-find-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let app = base.join("packages/web");
        std::fs::create_dir_all(&app).unwrap();
        // Workspace root marker: Vite's searchForWorkspaceRoot is the stopDir of
        // its postcss-load-config search (`.git` is not a marker in Vite).
        std::fs::write(base.join("pnpm-workspace.yaml"), "packages: ['packages/*']").unwrap();
        assert!(find_postcss_config(&app).is_none());
        // A config at the workspace root applies to the package.
        std::fs::write(base.join(".postcssrc.json"), r#"{"plugins":{}}"#).unwrap();
        assert_eq!(
            find_postcss_config(&app),
            Some(base.join(".postcssrc.json"))
        );
        // package.json#postcss in the package itself wins (nearest first).
        std::fs::write(
            app.join("package.json"),
            r#"{"name":"web","postcss":{"plugins":{}}}"#,
        )
        .unwrap();
        assert_eq!(find_postcss_config(&app), Some(app.join("package.json")));
        // ...and a config file in the package beats its package.json key.
        std::fs::write(app.join("postcss.config.ts"), "export default {}").unwrap();
        assert_eq!(
            find_postcss_config(&app),
            Some(app.join("postcss.config.ts"))
        );
        // A package.json without the key does not count.
        std::fs::remove_file(app.join("postcss.config.ts")).unwrap();
        std::fs::write(app.join("package.json"), r#"{"name":"web"}"#).unwrap();
        assert_eq!(
            find_postcss_config(&app),
            Some(base.join(".postcssrc.json"))
        );
        // Nothing above the workspace root is consulted.
        std::fs::remove_file(base.join(".postcssrc.json")).unwrap();
        assert!(find_postcss_config(&app).is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn fs_deny_blocks_dotenv_and_git_by_default() {
        // No user config: Vite's default deny set still protects secrets.
        let root = Path::new("/proj");
        let deny = compile_fs_deny(&[]);
        assert!(path_is_denied(&root.join(".env"), root, &deny));
        assert!(path_is_denied(&root.join(".env.local"), root, &deny));
        assert!(path_is_denied(&root.join("certs/server.pem"), root, &deny));
        assert!(path_is_denied(&root.join(".git/config"), root, &deny));
        assert!(path_is_denied(
            &root.join("packages/app/.git/HEAD"),
            root,
            &deny
        ));
        // Ordinary source is served.
        assert!(!path_is_denied(&root.join("src/main.tsx"), root, &deny));
        assert!(!path_is_denied(&root.join("src/env.ts"), root, &deny));
    }

    #[test]
    fn fs_deny_honors_user_patterns() {
        let root = Path::new("/proj");
        let deny = compile_fs_deny(&["secrets/**".to_string(), "*.key".to_string()]);
        assert!(path_is_denied(&root.join("secrets/token.txt"), root, &deny));
        assert!(path_is_denied(&root.join("id_rsa.key"), root, &deny));
        assert!(!path_is_denied(&root.join("public/logo.svg"), root, &deny));
    }

    #[test]
    fn fs_deny_is_case_insensitive() {
        // On a case-insensitive filesystem `.ENV` opens the same bytes as
        // `.env`, so a case-sensitive deny glob would leak it. Every case
        // variant of a denied name (base-name and path patterns) must be denied.
        let root = Path::new("/proj");
        let deny = compile_fs_deny(&["secrets/**".to_string(), "*.key".to_string()]);
        assert!(path_is_denied(&root.join(".ENV"), root, &deny));
        assert!(path_is_denied(&root.join(".Env.Production"), root, &deny));
        assert!(path_is_denied(&root.join("certs/SERVER.PEM"), root, &deny));
        assert!(path_is_denied(&root.join(".GIT/config"), root, &deny));
        assert!(path_is_denied(&root.join("Secrets/token.txt"), root, &deny));
        assert!(path_is_denied(&root.join("id_rsa.KEY"), root, &deny));
    }

    #[test]
    fn fs_deny_expands_brace_groups() {
        // Vite's own default deny list is brace-form (*.{crt,pem,key,...});
        // the glob crate has no brace support, so without expansion those
        // patterns match nothing and denied files get served.
        let root = Path::new("/proj");
        let deny = compile_fs_deny(&[
            "*.{key,p12,pfx}".to_string(),
            "secrets/{a,b}/**".to_string(),
        ]);
        assert!(path_is_denied(&root.join("server.key"), root, &deny));
        assert!(path_is_denied(&root.join("bundle.p12"), root, &deny));
        assert!(path_is_denied(&root.join("cert.pfx"), root, &deny));
        assert!(path_is_denied(&root.join("secrets/a/token"), root, &deny));
        assert!(path_is_denied(&root.join("secrets/b/token"), root, &deny));
        assert!(!path_is_denied(&root.join("secrets/c/token"), root, &deny));
        assert!(!path_is_denied(&root.join("server.kee"), root, &deny));
    }

    #[test]
    fn warmup_paths_matches_exclusions_root_relative() {
        // A './'-spelled exclusion and a bare positive pattern must agree
        // about the same file, and a root containing glob metacharacters must
        // not break the walk (the root is escaped, patterns are relative).
        let dir = std::env::temp_dir().join(format!("oj-warmup-[x]-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src/generated")).unwrap();
        std::fs::write(dir.join("src/a.js"), "a").unwrap();
        std::fs::write(dir.join("src/generated/b.js"), "b").unwrap();
        let picked = warmup_paths(
            &dir,
            &[
                "src/**/*.js".to_string(),
                "!./src/generated/*.js".to_string(),
            ],
        );
        assert_eq!(picked.len(), 1, "exclusion must apply: {picked:?}");
        assert!(picked[0].ends_with("src/a.js"));
    }

    #[test]
    fn bare_specifier_classification_routes_plugin_virtuals_to_the_fallback() {
        // Plugin virtuals (`virtual:pwa-register`, \0-prefixed ids) count as
        // bare, so the plugin-host fallback (/@id/) resolves them; relative and
        // absolute-URL specifiers do not.
        assert!(is_bare_specifier("react"));
        assert!(is_bare_specifier("virtual:pwa-register"));
        assert!(is_bare_specifier("\0oj-virtual"));
        assert!(!is_bare_specifier("./local"));
        assert!(!is_bare_specifier("../up"));
        assert!(!is_bare_specifier("/abs"));
        assert!(!is_bare_specifier("https://cdn/x.js"));
    }

    #[test]
    fn sec_fetch_dest_decides_raw_vs_module_form() {
        // A `<link>`/`<img>`/@font-face request wants the raw resource; a JS
        // `import` (script/empty dest) wants the JS-module form. This is how a
        // `.css` reached from JS is served as JS, not a text/css module script.
        let raw = |d: &str| {
            let mut h = HeaderMap::new();
            h.insert("sec-fetch-dest", d.parse().unwrap());
            wants_raw_resource(&h)
        };
        assert!(raw("style"));
        assert!(raw("image"));
        assert!(raw("font"));
        assert!(!raw("script"));
        assert!(!raw("empty"));
        // No header (older browsers / non-browser clients): default to the
        // module form so JS imports keep working.
        assert!(!wants_raw_resource(&HeaderMap::new()));
    }

    #[test]
    fn imports_a_plugin_virtual_flags_only_missing_fs_paths() {
        // A cached module is re-transformed on warm start only if it imports a
        // plugin-served virtual: a filesystem-path import with no file on disk.
        // Real files (source, svgr's .svg) and oj-internal /@ routes keep the
        // fast persistent cache.
        let root = std::env::temp_dir().join(format!("oj-ipv-test-{}", std::process::id()));
        let src = root.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("real.ts"), "export {}\n").unwrap();
        let dc = Mutex::new(DirCache::new());
        let r = root.as_path();

        // All-real imports -> keep cache.
        assert!(!imports_a_plugin_virtual(
            &["/src/real.ts".to_string()],
            r,
            &dc
        ));
        // oj-internal routes and external urls are never plugin-state virtuals.
        assert!(!imports_a_plugin_virtual(
            &[
                "/@id/abc".to_string(),
                "/@virtual/x".to_string(),
                "https://cdn/x.js".to_string()
            ],
            r,
            &dc,
        ));
        // A missing absolute path (wyw's .wyw-in-js.css) -> re-transform.
        assert!(imports_a_plugin_virtual(
            &[
                "/src/real.ts".to_string(),
                "/Users/nope/x.wyw-in-js.css".to_string()
            ],
            r,
            &dc,
        ));
        // A missing root-relative path -> re-transform.
        assert!(imports_a_plugin_virtual(
            &["/src/gone.css".to_string()],
            r,
            &dc
        ));
        // Query strings are stripped before the on-disk check.
        assert!(!imports_a_plugin_virtual(
            &["/src/real.ts?import".to_string()],
            r,
            &dc
        ));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn lingui_macro_specifiers_are_matched_exactly() {
        assert!(is_lingui_macro_specifier("@lingui/macro"));
        assert!(is_lingui_macro_specifier("@lingui/core/macro"));
        assert!(is_lingui_macro_specifier("@lingui/react/macro"));
        // The runtime packages (not the macro entrypoints) must NOT be shimmed.
        assert!(!is_lingui_macro_specifier("@lingui/core"));
        assert!(!is_lingui_macro_specifier("@lingui/react"));
        assert!(!is_lingui_macro_specifier("@lingui/macro/extra"));
    }

    #[test]
    fn node_builtins_are_recognized_for_stubbing() {
        assert!(is_node_builtin("fs"));
        assert!(is_node_builtin("node:fs"));
        assert!(is_node_builtin("fs/promises"));
        assert!(is_node_builtin("crypto"));
        assert!(is_node_builtin("perf_hooks"));
        // Vite's isNodeBuiltin: every `node:` id is a builtin, including the
        // scheme-only modules and ones newer than any hardcoded list.
        assert!(is_node_builtin("node:sqlite"));
        assert!(is_node_builtin("node:sea"));
        assert!(is_node_builtin("node:test"));
        assert!(is_node_builtin("node:whatever-node-adds-next"));
        assert!(is_node_builtin("_http_common"));
        // Not builtins: real packages and app specifiers. The scheme-only names
        // are not builtins when bare, exactly like Node.
        assert!(!is_node_builtin("sqlite"));
        assert!(!is_node_builtin("test"));
        assert!(!is_node_builtin("source-map-support"));
        assert!(!is_node_builtin("react"));
        assert!(!is_node_builtin("./local"));
    }

    #[test]
    fn workspace_root_matches_vite_search_for_workspace_root() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().canonicalize().unwrap();
        // A git repository is NOT a workspace marker (Vite comments `.git` out):
        // the default fs.allow for a project nested in a repo stays the project.
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(repo.join("package.json"), "{}").unwrap();
        let app = repo.join("apps/web");
        std::fs::create_dir_all(&app).unwrap();
        std::fs::write(app.join("package.json"), r#"{"name":"web"}"#).unwrap();
        assert_eq!(
            workspace_root(&app),
            app,
            "nearest package.json, not the repo"
        );

        // A pnpm workspace marker above it widens the root to the workspace.
        std::fs::write(repo.join("pnpm-workspace.yaml"), "packages: ['apps/*']").unwrap();
        assert_eq!(workspace_root(&app), repo);
        std::fs::remove_file(repo.join("pnpm-workspace.yaml")).unwrap();

        // ...as does a root package.json with a `workspaces` field.
        std::fs::write(repo.join("package.json"), r#"{"workspaces":["apps/*"]}"#).unwrap();
        assert_eq!(workspace_root(&app), repo);

        // No package.json anywhere: the app root itself.
        let bare = repo.join("bare");
        std::fs::create_dir_all(&bare).unwrap();
        std::fs::write(repo.join("package.json"), "{}").unwrap();
        assert_eq!(
            workspace_root(&bare),
            repo,
            "nearest ancestor with a package.json"
        );
    }

    #[test]
    fn browser_external_stub_warns_like_vite_on_property_access() {
        let src = browser_external_stub_source("node:fs");
        assert!(src.contains("has been externalized for browser compatibility"));
        assert!(src.contains("Cannot access \"${\"node:fs\"}.${key}\" in client code"));
        // Interop reads names off __cjs_exports and probes __esModule first; the
        // probe must stay silent and the module must still export both shapes.
        assert!(src.contains("key !== \"__esModule\""));
        assert!(src.contains("export default __oj_ext"));
        assert!(src.contains("export const __cjs_exports = __oj_ext"));
        // The stub is JS-safe for any id (quotes escaped through JSON).
        let quoted = browser_external_stub_source("a\"b");
        assert!(quoted.contains("\"a\\\"b\""));
    }

    #[test]
    fn importable_asset_exts_exclude_code_and_svg() {
        assert!(is_importable_asset_ext("webp"));
        assert!(is_importable_asset_ext("png"));
        assert!(is_importable_asset_ext("woff2"));
        // Vite's list is case-insensitive and includes documents and more media.
        assert!(is_importable_asset_ext("PNG"));
        assert!(is_importable_asset_ext("pdf"));
        assert!(is_importable_asset_ext("flac"));
        assert!(is_asset_ext("Jpg"));
        assert_eq!(content_type("JPG"), "image/jpeg");
        // svg is routed to vite-plugin-svgr, not URL-exported here.
        assert!(!is_importable_asset_ext("svg"));
        assert!(!is_importable_asset_ext("SVG"));
        assert!(!is_importable_asset_ext("css"));
        assert!(!is_importable_asset_ext("js"));
    }

    #[test]
    fn ts_source_outside_root_is_not_treated_as_a_dep() {
        // A monorepo package reached through a resolve.alias is served via /@fs/
        // but is TS/JSX source that must be transpiled, not dep/CJS-interop'd.
        let fs = Path::new("/repo/packages/ui/src/Button.tsx");
        assert!(!is_dep_module("/@fs/repo/packages/ui/src/Button.tsx", fs));
        let ts = Path::new("/repo/packages/ui/src/index.ts");
        assert!(!is_dep_module("/@fs/repo/packages/ui/src/index.ts", ts));
        // A real dependency (.js/.mjs, or anything under node_modules that is not
        // TS/JSX source) stays on the dep path.
        let dep = Path::new("/app/node_modules/react/index.js");
        assert!(is_dep_module("/node_modules/react/index.js", dep));
        // A linked workspace package (realpath outside node_modules) is source,
        // not a dep: plugins and the source compile path apply (Vite treats
        // linked packages the same way).
        let fs_js = Path::new("/repo/packages/ui/dist/index.mjs");
        assert!(!is_dep_module(
            "/@fs/repo/packages/ui/dist/index.mjs",
            fs_js
        ));
        let fs_dep = Path::new("/repo/node_modules/.pnpm/x@1/node_modules/x/index.js");
        assert!(is_dep_module(
            "/@fs/repo/node_modules/.pnpm/x@1/node_modules/x/index.js",
            fs_dep
        ));
        // App-local source (not node_modules, not /@fs/) is never a dep.
        let local = Path::new("/app/src/App.tsx");
        assert!(!is_dep_module("/src/App.tsx", local));
    }

    #[test]
    fn html_injection_puts_preamble_first_in_head() {
        let out = inject_dev_scripts("<html><head><title>x</title></head></html>".into());
        let preamble = out.find("refresh-preamble").unwrap();
        let title = out.find("<title>").unwrap();
        assert!(preamble < title);
    }

    #[test]
    fn glue_only_added_for_boundary_modules() {
        assert!(hot_glue("/src/util.ts", None, false, false).is_empty());
        let glue = hot_glue("/src/App.tsx", Some("t=1700000000000"), true, false);
        assert!(glue.contains(r#"createHotContext("/src/App.tsx")"#));
        assert!(
            glue.contains(r#"from "/src/App.tsx?t=1700000000000""#),
            "{glue}"
        );
        assert!(glue.contains("validateRefreshBoundaryAndEnqueueUpdate"));
        assert!(glue.contains("function $RefreshReg$"));
    }

    // A module that reads import.meta.hot itself gets the hot-context banner
    // PREPENDED (svelte_hot_glue); the appended refresh glue must then reuse
    // that context, never re-import it — an import binding is a lexical
    // declaration, and a second `__oj_createHotContext` in the same module
    // scope is a SyntaxError that kills the module (seen on TanStack route
    // files, whose router plugin injects its own import.meta.hot handler).
    #[test]
    fn glue_reuses_a_predefined_hot_context_instead_of_redeclaring_it() {
        let banner = svelte_hot_glue("/src/routes/a.tsx");
        let glue = hot_glue("/src/routes/a.tsx", Some("t=1700000000000"), true, true);
        assert!(!glue.contains("__oj_createHotContext"), "{glue}");
        assert!(glue.contains("registerExportsForReactRefresh"), "{glue}");
        let combined = format!("{banner}{glue}");
        assert_eq!(
            combined
                .matches("createHotContext as __oj_createHotContext")
                .count(),
            1,
            "exactly one declaration per module scope: {combined}"
        );
    }

    #[test]
    fn glue_never_doubles_a_query_the_url_already_carries() {
        // serve_compiled keys per full url, so the real call hands hot_glue a url
        // that already has its query AND the same query again. The self-import
        // must not grow (`?t=X?t=X` grew per edit until hyper answered 414) and
        // the hot-context id must stay the clean path the server sends updates for.
        let glue = hot_glue(
            "/src/App.tsx?t=1700000000000",
            Some("t=1700000000000"),
            true,
            false,
        );
        assert!(
            glue.contains(r#"createHotContext("/src/App.tsx")"#),
            "{glue}"
        );
        assert!(
            glue.contains(r#"from "/src/App.tsx?t=1700000000000""#),
            "{glue}"
        );
        assert!(!glue.contains("?t=1700000000000?t=1700000000000"), "{glue}");
        assert!(
            glue.contains(r#"registerExportsForReactRefresh("/src/App.tsx","#),
            "{glue}"
        );
        // Only the hmr timestamp is stripped from the id; a semantic query that
        // makes a distinct module (router `?tsr-shared=1`) is kept in both.
        let v = hot_glue(
            "/src/r.tsx?tsr-shared=1&t=1700000000000",
            Some("tsr-shared=1&t=1700000000000"),
            true,
            false,
        );
        assert!(
            v.contains(r#"createHotContext("/src/r.tsx?tsr-shared=1")"#),
            "{v}"
        );
        assert!(
            v.contains(r#"from "/src/r.tsx?tsr-shared=1&t=1700000000000""#),
            "{v}"
        );
    }

    #[test]
    fn restart_triggers_include_every_config_flavor() {
        for f in [
            "oj.config.ts",
            "oj.config.json",
            "vite.config.mts",
            ".env",
            ".env.staging",
            "postcss.config.cjs",
        ] {
            assert!(is_restart_trigger(Path::new(f)), "{f}");
        }
        for f in ["src/main.ts", "package.json", "config.json", "env.ts"] {
            assert!(!is_restart_trigger(Path::new(f)), "{f}");
        }
    }

    #[test]
    fn error_frame_is_a_vite_error_payload() {
        let f: serde_json::Value = serde_json::from_str(&error_frame(
            "compile error:\nsrc/App.tsx:3:7 Unexpected token\n  | <div>",
        ))
        .unwrap();
        assert_eq!(f["type"], "error");
        assert_eq!(
            f["err"]["message"],
            "compile error:\nsrc/App.tsx:3:7 Unexpected token\n  | <div>"
        );
        assert_eq!(f["err"]["id"], "src/App.tsx");
        assert_eq!(f["err"]["loc"]["line"], 3);
        assert_eq!(f["err"]["loc"]["column"], 7);
        assert!(f["err"]["frame"]
            .as_str()
            .unwrap()
            .contains("Unexpected token"));
        let plain: serde_json::Value = serde_json::from_str(&error_frame("boom")).unwrap();
        assert!(plain["err"]["id"].is_null() && plain["err"]["frame"].is_null());
        let u = update_entry("css-update", "/a.css", 5);
        assert_eq!(u["acceptedPath"], "/a.css");
        assert_eq!(u["type"], "css-update");
    }

    #[test]
    fn ws_proxy_target_and_header_rules() {
        assert_eq!(
            ws_target_url("http://localhost:4000"),
            "ws://localhost:4000"
        );
        assert_eq!(ws_target_url("https://api.test"), "wss://api.test");
        assert_eq!(
            ws_target_origin("wss://api.test:8443/socket?x=1"),
            "https://api.test:8443"
        );
        assert_eq!(
            ws_target_origin("ws://localhost:3000"),
            "http://localhost:3000"
        );
        assert_eq!(ws_target_url("ws://x:1"), "ws://x:1");
        assert!(ws_forwardable_header(&header::COOKIE));
        assert!(ws_forwardable_header(&header::SEC_WEBSOCKET_PROTOCOL));
        assert!(ws_forwardable_header(&header::ORIGIN));
        for h in [
            header::HOST,
            header::CONNECTION,
            header::UPGRADE,
            header::SEC_WEBSOCKET_KEY,
            header::SEC_WEBSOCKET_VERSION,
            header::SEC_WEBSOCKET_EXTENSIONS,
        ] {
            assert!(!ws_forwardable_header(&h), "{h}");
        }
        let mut h = HeaderMap::new();
        h.insert(header::UPGRADE, "WebSocket".parse().unwrap());
        assert!(is_websocket_upgrade(&h));
        assert!(!is_websocket_upgrade(&HeaderMap::new()));
    }

    #[test]
    fn proxy_tls_configs_build_for_both_secure_settings() {
        assert!(proxy_tls_config(false).is_ok(), "accept-any config");
        assert!(proxy_tls_config(true).is_ok(), "platform verifier config");
    }

    #[test]
    fn localhost_origin_default_matches_vite_regex() {
        for ok in [
            "http://localhost",
            "http://localhost:5173",
            "https://app.localhost:3000",
            "http://127.0.0.1:8080",
            "http://[::1]:5173",
        ] {
            assert!(is_localhost_origin(ok), "{ok}");
        }
        for bad in [
            "http://evil.com",
            "http://localhost.evil.com",
            "http://127.0.0.1.nip.io",
            "ftp://localhost",
            "http://localhost:abc",
        ] {
            assert!(!is_localhost_origin(bad), "{bad}");
        }
    }

    #[test]
    fn host_policy_allows_localhost_ips_and_configured_hosts_only() {
        let server = oj_config::ServerConfig {
            allowed_hosts: Some(oj_config::AllowedHosts::List(vec![
                "app.test".into(),
                ".corp.example".into(),
            ])),
            ..Default::default()
        };
        let p = HostPolicy::from_config(&server, Some("dev.local"));
        for ok in [
            "localhost",
            "sub.localhost",
            "127.0.0.1",
            "[::1]",
            "10.0.0.5",
            "app.test",
            "APP.TEST",
            "corp.example",
            "x.corp.example",
            "dev.local",
        ] {
            assert!(p.hostname_allowed(ok), "{ok}");
        }
        for bad in ["evil.com", "notcorp.example", "app.test.evil", ""] {
            assert_eq!(p.hostname_allowed(bad), bad.is_empty(), "{bad}");
        }
        assert_eq!(
            HostPolicy::host_header_name("example.com:5173"),
            "example.com"
        );
        assert_eq!(HostPolicy::host_header_name("[::1]:5173"), "::1");
        assert_eq!(HostPolicy::host_header_name("example.com"), "example.com");
        let all = HostPolicy::from_config(
            &oj_config::ServerConfig {
                allowed_hosts: Some(oj_config::AllowedHosts::All(true)),
                ..Default::default()
            },
            None,
        );
        assert!(all.allow_all && all.hostname_allowed("evil.com"));
    }

    #[test]
    fn cors_policy_forms() {
        let default = CorsPolicy::from_config(None).unwrap();
        assert!(default.allows("http://localhost:5173") && !default.allows("http://evil.com"));
        assert!(CorsPolicy::from_config(Some(&oj_config::CorsConfig::Toggle(false))).is_none());
        let any = CorsPolicy::from_config(Some(&oj_config::CorsConfig::Toggle(true))).unwrap();
        assert!(any.allows("http://evil.com"));
        let opts: oj_config::CorsOptions = serde_json::from_value(serde_json::json!({
            "origin": ["http://a.test", "http://b.test"], "credentials": true, "methods": ["GET", "POST"], "maxAge": 60
        }))
        .unwrap();
        let list =
            CorsPolicy::from_config(Some(&oj_config::CorsConfig::Options(Box::new(opts)))).unwrap();
        assert!(list.allows("http://a.test") && !list.allows("http://localhost:5173"));
        assert!(list.credentials && list.methods == "GET,POST" && list.max_age == Some(60));
    }

    #[test]
    fn asset_requests_are_modules_only_for_imports() {
        let mut h = HeaderMap::new();
        assert!(!wants_module_import(&h, None), "a bare fetch gets the file");
        assert!(wants_module_import(&h, Some("import")));
        assert!(wants_module_import(&h, Some("t=1&import")));
        h.insert("sec-fetch-dest", "empty".parse().unwrap());
        assert!(!wants_module_import(&h, None));
        h.insert("sec-fetch-dest", "image".parse().unwrap());
        assert!(!wants_module_import(&h, None));
        h.insert("sec-fetch-dest", "script".parse().unwrap());
        assert!(wants_module_import(&h, None), "a module import of the url");
    }

    #[test]
    fn watch_ignored_globs_match_relative_and_absolute_paths() {
        let root = Path::new("/app");
        let pats = watch_ignored_patterns(
            root,
            &[
                "**/generated/**".to_string(),
                "docs/*.md".to_string(),
                "/tmp/out/**".to_string(),
            ],
        );
        assert!(is_watch_ignored(
            &pats,
            root,
            Path::new("/app/src/generated/x.ts")
        ));
        assert!(
            is_watch_ignored(&pats, root, Path::new("/app/docs/intro.md")),
            "root-relative pattern"
        );
        assert!(
            !is_watch_ignored(&pats, root, Path::new("/app/docs/deep/intro.md")),
            "* stops at /"
        );
        assert!(
            is_watch_ignored(&pats, root, Path::new("/tmp/out/a/b.js")),
            "absolute pattern"
        );
        assert!(!is_watch_ignored(
            &pats,
            root,
            Path::new("/app/src/main.ts")
        ));
        assert!(!is_watch_ignored(
            &[],
            root,
            Path::new("/app/src/generated/x.ts")
        ));
    }

    #[test]
    fn html_fallback_rewrites_like_vite() {
        assert_eq!(
            html_fallback_candidate("nested/").as_deref(),
            Some("nested/index.html")
        );
        assert_eq!(
            html_fallback_candidate("about").as_deref(),
            Some("about.html")
        );
        assert_eq!(
            html_fallback_candidate("docs/intro").as_deref(),
            Some("docs/intro.html")
        );
        assert_eq!(
            html_fallback_candidate("about.html"),
            None,
            "an explicit html request is not rewritten"
        );
        assert_eq!(html_fallback_candidate(""), None);
        let mut h = HeaderMap::new();
        assert!(accepts_html_fallback(&h), "no Accept is */*");
        h.insert(
            header::ACCEPT,
            "text/html,application/xhtml+xml".parse().unwrap(),
        );
        assert!(accepts_html_fallback(&h));
        h.insert(header::ACCEPT, "*/*".parse().unwrap());
        assert!(accepts_html_fallback(&h));
        h.insert(header::ACCEPT, "application/json".parse().unwrap());
        assert!(
            !accepts_html_fallback(&h),
            "an API-style request never gets html"
        );
    }

    #[test]
    fn client_js_is_rendered_from_server_hmr_options() {
        let tpl = "a=__HMR_PROTOCOL__;b=__HMR_HOSTNAME__;c=__HMR_PORT__;d=__HMR_PATH__;e=__HMR_ENABLE_OVERLAY__;f=__WS_TOKEN__;";
        assert_eq!(hmr_socket_path(None), "/__ws");
        assert_eq!(
            render_client_js(tpl, None, "/__ws", "tok"),
            r#"a=null;b=null;c=null;d="/__ws";e=true;f="tok";"#
        );
        let opts = oj_config::HmrOptions {
            path: Some("hmr".into()),
            port: Some(24678),
            client_port: Some(443),
            host: Some("app.test".into()),
            protocol: Some("wss".into()),
            overlay: Some(false),
            timeout: None,
        };
        let path = hmr_socket_path(Some(&opts));
        assert_eq!(path, "/hmr", "a relative hmr.path is made absolute");
        assert_eq!(
            render_client_js(tpl, Some(&opts), &path, "tok"),
            r#"a="wss";b="app.test";c=443;d="/hmr";e=false;f="tok";"#,
            "clientPort, not port, is what the browser dials"
        );
        let real = render_client_js(CLIENT_JS, None, "/__ws", "tok");
        assert!(
            !real.contains("__HMR_") && !real.contains("__WS_TOKEN__"),
            "no placeholder left"
        );
    }

    #[test]
    fn ws_token_is_demanded_only_from_browser_upgrades() {
        let mut h = HeaderMap::new();
        assert!(
            !ws_token_rejected(true, "t0k", &h, None),
            "no Origin: not a browser"
        );
        h.insert(header::ORIGIN, "http://localhost:5199".parse().unwrap());
        assert!(ws_token_rejected(true, "t0k", &h, None));
        assert!(ws_token_rejected(true, "t0k", &h, Some("token=nope")));
        assert!(!ws_token_rejected(true, "t0k", &h, Some("token=t0k")));
        assert!(!ws_token_rejected(true, "t0k", &h, Some("a=1&token=t0k")));
        assert!(
            !ws_token_rejected(false, "t0k", &h, None),
            "legacy.skipWebSocketTokenCheck"
        );
        let token = new_ws_token();
        assert_eq!(token.len(), 32);
        assert!(token.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(token, new_ws_token());
    }

    #[test]
    fn stamp_import_url_marks_only_compilable_module_urls() {
        assert_eq!(stamp_import_url("/src/utils.ts", 5), "/src/utils.ts?t=5");
        assert_eq!(
            stamp_import_url("/src/r.tsx?tsr-shared=1", 5),
            "/src/r.tsx?tsr-shared=1&t=5"
        );
        assert_eq!(
            stamp_import_url("/src/utils.ts", 0),
            "/src/utils.ts",
            "unstamped module"
        );
        assert_eq!(
            stamp_import_url("/src/utils.ts?t=3", 5),
            "/src/utils.ts?t=3",
            "never doubled"
        );
        assert_eq!(
            stamp_import_url("/src/data.json", 5),
            "/src/data.json?t=5",
            "json is a module"
        );
        assert_eq!(
            stamp_import_url("/src/a.css?import", 5),
            "/src/a.css?import&t=5",
            "a CSS module's importer must fetch its new class exports"
        );
        assert_eq!(
            stamp_import_url("/src/a.css?inline", 5),
            "/src/a.css?inline&t=5"
        );
        assert_eq!(stamp_import_url("/logo.svg?url", 5), "/logo.svg?url");
        assert_eq!(
            stamp_import_url("/@oj-deps/react.js", 5),
            "/@oj-deps/react.js"
        );
        assert_eq!(
            stamp_import_url("/@fs/x/node_modules/a/index.js", 5),
            "/@fs/x/node_modules/a/index.js"
        );
    }

    #[test]
    fn proxy_regex_contexts_match_path_and_query_like_vite() {
        let entries: Vec<(String, oj_config::ProxyEntry)> = [
            ("/api", "http://a"),
            ("/api/v2", "http://a2"),
            ("^/re/.*", "http://re"),
            ("^/search\\?q=", "http://q"),
            ("^(", "http://bad"),
        ]
        .iter()
        .map(|(c, t)| (c.to_string(), oj_config::ProxyEntry::Target(t.to_string())))
        .collect();
        let regexes: Vec<Option<regex::Regex>> = entries
            .iter()
            .map(|(c, _)| proxy_context_regex(c))
            .collect();
        assert!(regexes[0].is_none() && regexes[2].is_some());
        assert!(
            regexes[4].is_none(),
            "an invalid pattern degrades to a never-matching prefix"
        );
        let pick =
            |url: &str| select_proxy(&entries, &regexes, url).map(|(_, e)| e.target().to_string());
        assert_eq!(pick("/api/x"), Some("http://a".into()));
        assert_eq!(
            pick("/api/v2/x"),
            Some("http://a2".into()),
            "longest prefix wins"
        );
        assert_eq!(pick("/re/anything?x=1"), Some("http://re".into()));
        assert_eq!(
            pick("/search?q=oj"),
            Some("http://q".into()),
            "regex sees the query"
        );
        assert_eq!(pick("/search"), None);
        assert_eq!(pick("/other"), None);
        assert!(proxy_context_matches("^/re/.*", "/re/x"));
        assert!(
            !proxy_context_matches("^/re/.*", "/api/re/x"),
            "anchored, not a substring"
        );
        assert!(proxy_context_matches("/api", "/api/x?y=1"));
    }

    #[test]
    fn unresolved_import_error_points_at_the_specifier() {
        let root = Path::new("/app");
        let file = Path::new("/app/src/App.tsx");
        let source = "import { useState } from \"react\";\nimport { Later } from './Later';\n";
        let err = unresolved_import_error(root, file, source, "./Later");
        assert!(is_unresolved_import_error(&err));
        assert!(
            err.contains("src/App.tsx:2:24 Failed to resolve import \"./Later\" from \"src/App.tsx\". Does the file exist?"),
            "{err}"
        );
        assert!(
            err.contains("   2 | import { Later } from './Later';"),
            "{err}"
        );
        // The overlay's ErrorPayload lifts the location out of the message.
        let frame: serde_json::Value = serde_json::from_str(&error_frame(&err)).unwrap();
        assert_eq!(frame["err"]["id"], "src/App.tsx");
        assert_eq!(frame["err"]["loc"]["line"], 2);
        assert_eq!(frame["err"]["loc"]["column"], 24);
        // A specifier not found verbatim (plugin-rewritten source) still errors.
        let err = unresolved_import_error(root, file, "export {};\n", "./gone");
        assert!(
            err.contains("src/App.tsx:1:1 Failed to resolve import \"./gone\""),
            "{err}"
        );
        assert!(!is_unresolved_import_error(
            "compile error:\nparse error in x.tsx"
        ));
    }

    #[test]
    fn preload_hints_name_the_exact_import_url() {
        assert_eq!(preload_href("/src/main.tsx", "abcd1234"), "/src/main.tsx");
        assert_eq!(preload_href("/src/a.css", "abcd1234"), "/src/a.css?import");
        // Optimized deps and package bundles preload under their versioned URL,
        // so the preload and the later import hit one immutable cache entry.
        assert_eq!(
            preload_href("/@oj-deps/react.mjs", "abcd1234"),
            "/@oj-deps/react.mjs?v=abcd1234"
        );
        assert_eq!(
            preload_href("/@oj-pkg/00ff", "abcd1234"),
            "/@oj-pkg/00ff?v=abcd1234"
        );
        assert_eq!(
            preload_href("/@oj-deps/react.mjs", ""),
            "/@oj-deps/react.mjs",
            "no version, no query"
        );
        assert_eq!(
            preload_href("/@id/6e6f6465", "abcd1234"),
            "/@id/6e6f6465",
            "stubs are unversioned"
        );
    }

    #[test]
    fn optional_peer_dep_resolves_to_a_lazy_error_stub() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let parent = root.join("node_modules/devtools-hook");
        std::fs::create_dir_all(parent.join("lib")).unwrap();
        std::fs::write(
            parent.join("package.json"),
            r#"{"name":"devtools-hook","peerDependencies":{"react":">=18","@scope/opt":"*","required-peer":"*"},
                "peerDependenciesMeta":{"react":{"optional":false},"@scope/opt":{"optional":true}}}"#,
        )
        .unwrap();
        // The lookup runs from the importing file's directory inside the package.
        let from = parent.join("lib");
        let url = optional_peer_dep_url(&root, &from, "@scope/opt/sub").expect("optional peer");
        assert!(url.starts_with(OPTIONAL_PEER_PREFIX), "{url}");
        let stub = optional_peer_dep_stub(url.strip_prefix(OPTIONAL_PEER_PREFIX).unwrap()).unwrap();
        assert!(stub.contains("Could not resolve \"${\"@scope/opt/sub\"}\" imported by \"${\"devtools-hook\"}\". Is it installed?"), "{stub}");
        assert!(
            stub.contains("throw new Error("),
            "errors when evaluated: {stub}"
        );
        // A declared but non-optional peer, an undeclared package, a builtin, and
        // an import from the app root itself all fall through to the normal error.
        assert!(
            optional_peer_dep_url(&root, &from, "react").is_none(),
            "optional: false"
        );
        assert!(
            optional_peer_dep_url(&root, &from, "required-peer").is_none(),
            "no meta"
        );
        assert!(optional_peer_dep_url(&root, &from, "unknown-pkg").is_none());
        assert!(optional_peer_dep_url(&root, &from, "node:fs").is_none());
        assert!(optional_peer_dep_url(&root, &from, "./local").is_none());
        std::fs::write(root.join("package.json"), r#"{"name":"app","peerDependencies":{"x":"*"},"peerDependenciesMeta":{"x":{"optional":true}}}"#).unwrap();
        assert!(
            optional_peer_dep_url(&root, &root, "x").is_none(),
            "root has no peer deps (Vite: basedir !== root)"
        );
        assert!(optional_peer_dep_stub("zz").is_none(), "malformed id");
    }

    #[test]
    fn bare_import_unresolved_only_for_packages_nothing_answers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("node_modules/real")).unwrap();
        std::fs::write(
            root.join("node_modules/real/package.json"),
            r#"{"name":"real","main":"index.js"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("node_modules/real/index.js"),
            "module.exports = 1;\n",
        )
        .unwrap();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"app","browser":{"mapped-off":false}}"#,
        )
        .unwrap();
        let resolver = OjResolver::new(root);
        // Vite's importAnalysis fails the importer for a package that is not
        // installed (typo, missing install), subpaths included.
        assert!(bare_import_unresolved(root, &resolver, "not-installed-pkg"));
        assert!(bare_import_unresolved(
            root,
            &resolver,
            "@scope/not-installed/sub"
        ));
        assert!(
            bare_import_unresolved(root, &resolver, "not-installed-pkg?url"),
            "query dropped"
        );
        // Everything something else answers is not an error here.
        assert!(
            !bare_import_unresolved(root, &resolver, "real"),
            "installed"
        );
        assert!(
            !bare_import_unresolved(root, &resolver, "node:fs"),
            "builtin stub"
        );
        assert!(
            !bare_import_unresolved(root, &resolver, "fs"),
            "builtin stub"
        );
        assert!(
            !bare_import_unresolved(root, &resolver, "virtual:thing"),
            "plugin virtual"
        );
        assert!(
            !bare_import_unresolved(root, &resolver, "\0resolved"),
            "plugin resolved id"
        );
        assert!(
            !bare_import_unresolved(root, &resolver, "data:text/javascript,export{}"),
            "data url"
        );
        assert!(
            !bare_import_unresolved(root, &resolver, "https://cdn.example/x.js"),
            "external url"
        );
        assert!(
            !bare_import_unresolved(root, &resolver, "./nope"),
            "relative is the other check"
        );
        assert!(
            !bare_import_unresolved(root, &resolver, "/src/nope.ts"),
            "root-absolute"
        );
        assert!(
            !bare_import_unresolved(root, &resolver, "@lingui/macro"),
            "shimmed macro entry"
        );
    }

    #[test]
    fn relative_import_missing_only_for_unresolvable_relative_specifiers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/widgets")).unwrap();
        std::fs::write(root.join("src/util.ts"), "export const u = 1;\n").unwrap();
        std::fs::write(root.join("src/widgets/index.ts"), "export const w = 1;\n").unwrap();
        let resolver = OjResolver::new(root);
        let src = root.join("src");
        assert!(relative_import_missing(&src, &resolver, "./nope"));
        assert!(relative_import_missing(&src, &resolver, "../nope.js"));
        assert!(
            !relative_import_missing(&src, &resolver, "./util"),
            "extension probing"
        );
        assert!(!relative_import_missing(&src, &resolver, "./util.ts"));
        assert!(
            !relative_import_missing(&src, &resolver, "./widgets"),
            "directory index"
        );
        assert!(
            !relative_import_missing(&src, &resolver, "./util.ts?worker&inline"),
            "query dropped"
        );
        assert!(
            !relative_import_missing(&src, &resolver, "react"),
            "bare specifiers are not ours"
        );
        assert!(
            !relative_import_missing(&src, &resolver, "/src/nope.ts"),
            "root-absolute is not ours"
        );
        // A miss cached by the resolver clears once the file exists.
        assert!(relative_import_missing(&src, &resolver, "./later"));
        std::fs::write(root.join("src/later.ts"), "export {};\n").unwrap();
        resolver.clear_cache();
        assert!(!relative_import_missing(&src, &resolver, "./later"));
    }

    #[test]
    fn strip_hmr_timestamp_removes_only_the_t_param() {
        assert_eq!(strip_hmr_timestamp("/src/App.tsx"), "/src/App.tsx");
        assert_eq!(
            strip_hmr_timestamp("/src/App.tsx?t=1700000000000"),
            "/src/App.tsx"
        );
        assert_eq!(
            strip_hmr_timestamp("/a.tsx?tsr-shared=1&t=1700000000000"),
            "/a.tsx?tsr-shared=1"
        );
        assert_eq!(
            strip_hmr_timestamp("/a.tsx?t=1700000000000&tsr-shared=1"),
            "/a.tsx?tsr-shared=1"
        );
        // Only a 13-digit millisecond timestamp is oj's `t=` (Vite's timestampRE is
        // /\bt=\d{13}&?\b/). A short numeric or non-numeric `t=` is a user's own
        // query and must be kept.
        assert_eq!(strip_hmr_timestamp("/a.tsx?t=9"), "/a.tsx?t=9");
        assert_eq!(strip_hmr_timestamp("/a.tsx?t=123"), "/a.tsx?t=123");
        assert_eq!(strip_hmr_timestamp("/a.tsx?t=abc"), "/a.tsx?t=abc");
        assert_eq!(strip_hmr_timestamp("/a.tsx?type=x"), "/a.tsx?type=x");
    }

    #[test]
    fn resolve_host_maps_wildcards_to_all_interfaces() {
        let any: std::net::IpAddr = [0, 0, 0, 0].into();
        let local: std::net::IpAddr = [127, 0, 0, 1].into();
        assert_eq!(resolve_host(Some("true")), any);
        assert_eq!(resolve_host(Some("0.0.0.0")), any);
        assert_eq!(resolve_host(Some("::")), any);
        assert_eq!(resolve_host(Some("[::]")), any);
        assert_eq!(resolve_host(Some("localhost")), local);
        assert_eq!(resolve_host(None), local);
        let lan: std::net::IpAddr = [192, 168, 1, 5].into();
        assert_eq!(resolve_host(Some("192.168.1.5")), lan);
        assert_eq!(resolve_host(Some("bogus")), local);
    }

    #[test]
    fn normalize_resolves_parent_components() {
        assert_eq!(
            normalize(Path::new("/a/b/../c/./d.ts")),
            PathBuf::from("/a/c/d.ts")
        );
    }

    #[test]
    fn preview_html_fallback_prefers_page_index_then_sibling_then_root() {
        let dir = std::env::temp_dir().join(format!("oj-preview-fb-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("nested")).unwrap();
        std::fs::write(dir.join("index.html"), "root").unwrap();
        std::fs::write(dir.join("nested/index.html"), "nested").unwrap();
        std::fs::write(dir.join("about.html"), "about").unwrap();
        assert_eq!(
            preview_html_fallback(&dir, "nested/", true),
            Some(dir.join("nested/index.html"))
        );
        assert_eq!(
            preview_html_fallback(&dir, "nested", true),
            Some(dir.join("nested/index.html"))
        );
        assert_eq!(
            preview_html_fallback(&dir, "about", true),
            Some(dir.join("about.html"))
        );
        assert_eq!(
            preview_html_fallback(&dir, "missing/route", true),
            Some(dir.join("index.html"))
        );
        assert_eq!(
            preview_html_fallback(&dir, "", true),
            Some(dir.join("index.html"))
        );
        // appType mpa: `/x.html` and `/x/index.html` still resolve, nothing falls back to the root page.
        assert_eq!(
            preview_html_fallback(&dir, "about", false),
            Some(dir.join("about.html"))
        );
        assert_eq!(
            preview_html_fallback(&dir, "nested", false),
            Some(dir.join("nested/index.html"))
        );
        assert_eq!(preview_html_fallback(&dir, "missing/route", false), None);
        assert_eq!(
            preview_html_fallback(&dir, "", false),
            Some(dir.join("index.html"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preview_rel_maps_base_and_guards_traversal() {
        assert_eq!(preview_rel("/", "/").as_deref(), Some("index.html"));
        assert_eq!(
            preview_rel("/assets/x.js", "/").as_deref(),
            Some("assets/x.js")
        );
        assert_eq!(
            preview_rel("/app/assets/x.js", "/app/").as_deref(),
            Some("assets/x.js")
        );
        assert_eq!(preview_rel("/app/", "/app/").as_deref(), Some("index.html"));
        assert_eq!(preview_rel("/../etc/passwd", "/"), None);
    }

    #[test]
    fn html_entry_src_normalizes_relative_and_excludes_external() {
        assert_eq!(
            html_entry_src("src/index.tsx").as_deref(),
            Some("/src/index.tsx")
        );
        assert_eq!(
            html_entry_src("./src/index.tsx").as_deref(),
            Some("/src/index.tsx")
        );
        assert_eq!(
            html_entry_src("/src/index.tsx").as_deref(),
            Some("/src/index.tsx")
        );
        assert_eq!(html_entry_src("https://cdn/x.js"), None);
        assert_eq!(html_entry_src("//cdn/x.js"), None);
        assert_eq!(html_entry_src("data:text/js,1"), None);
    }

    #[test]
    fn csp_nonce_stamps_scripts_styles_and_preload_links_once() {
        let html = "<html><head>\
            <link rel=\"stylesheet\" href=\"/a.css\">\
            <link rel=\"icon\" href=\"/i.png\">\
            <link rel=\"modulepreload\" href=\"/m.js\" />\
            <style>.a{color:red}</style>\
            <script nonce=\"keep\">if (1 < 2) {}</script>\
            </head><body><script type='module' src='/main.js'></script><p>a < b</p></body></html>";
        let out = inject_csp_nonce(html, "n0nce");
        assert!(
            out.contains("<link rel=\"stylesheet\" href=\"/a.css\" nonce=\"n0nce\">"),
            "{out}"
        );
        assert!(
            out.contains("<link rel=\"icon\" href=\"/i.png\">"),
            "non-preload links untouched: {out}"
        );
        assert!(
            out.contains("<link rel=\"modulepreload\" href=\"/m.js\" nonce=\"n0nce\" />"),
            "{out}"
        );
        assert!(
            out.contains("<style nonce=\"n0nce\">.a{color:red}</style>"),
            "{out}"
        );
        assert!(
            out.contains("<script nonce=\"keep\">if (1 < 2) {}</script>"),
            "existing nonce kept: {out}"
        );
        assert!(
            out.contains("<script type='module' src='/main.js' nonce=\"n0nce\">"),
            "{out}"
        );
        assert!(
            out.contains("<head>\n<meta property=\"csp-nonce\" nonce=\"n0nce\">"),
            "{out}"
        );
        assert!(out.contains("<p>a < b</p>"), "{out}");
        assert_eq!(out.matches("csp-nonce").count(), 1);
        // Idempotent: a second pass adds nothing.
        assert_eq!(inject_csp_nonce(&out, "n0nce"), out);
    }

    #[test]
    fn html_entries_read_module_scripts_with_any_quoting() {
        let root =
            std::env::temp_dir().join(format!("oj-html-entries-quoting-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("index.html"),
            "<html><body>\
             <script type='module' src='/src/a.ts'></script>\
             <script type=module src=src/b.ts></script>\
             <script type = \"module\" data-src=\"/ignored.js\" src = \"./src/c.ts\"></script>\
             <script src=\"/legacy.js\"></script>\
             <script type=\"module\">inline()</script>\
             </body></html>",
        )
        .unwrap();
        assert_eq!(
            html_entries(&root),
            vec![
                "/src/a.ts".to_string(),
                "/src/b.ts".to_string(),
                "/src/c.ts".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn spa_navigation_falls_back_only_for_routes() {
        let html = {
            let mut h = HeaderMap::new();
            h.insert(
                header::ACCEPT,
                "text/html,application/xhtml+xml".parse().unwrap(),
            );
            h
        };
        let empty = HeaderMap::new();
        assert!(is_spa_navigation("dashboard", &empty));
        assert!(is_spa_navigation("users/123/edit", &empty));
        assert!(is_spa_navigation("report.v2", &html));
        assert!(!is_spa_navigation("missing.png", &empty));
        assert!(!is_spa_navigation("assets/app.js", &empty));
        assert!(!is_spa_navigation("@vite/client", &html));
        assert!(!is_spa_navigation("src/does-not-exist.tsx", &html));
        assert!(!is_spa_navigation("node_modules/react/missing.js", &html));
    }
}

#[cfg(test)]
mod adapter_tests {
    use super::*;

    fn tmp(label: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("oj-srv-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn write_start_assets_writes_every_module_the_start_host_aliases() {
        let dir = tmp("assets");
        write_start_assets(&dir).unwrap();
        for name in [
            "injected-head-scripts.ts",
            "manifest-dev.ts",
            "server-entry.tsx",
            "start-entry.ts",
            "cf-server.mjs",
            "cf-workers.mjs",
        ] {
            assert!(dir.join(name).is_file(), "{name} was not written");
        }
    }

    #[test]
    fn is_tanstack_start_app_requires_routes_and_dep() {
        let base = tmp("ts");
        let app = base.join("app");
        std::fs::create_dir_all(app.join("src").join("routes")).unwrap();
        std::fs::write(
            app.join("package.json"),
            r#"{"dependencies":{"react":"19"}}"#,
        )
        .unwrap();
        assert!(!is_tanstack_start_app(&app));
        std::fs::write(
            app.join("package.json"),
            r#"{"dependencies":{"@tanstack/react-start":"1"}}"#,
        )
        .unwrap();
        assert!(is_tanstack_start_app(&app));
        let app2 = base.join("app2");
        std::fs::create_dir_all(app2.join("src")).unwrap();
        std::fs::write(
            app2.join("package.json"),
            r#"{"dependencies":{"@tanstack/react-start":"1"}}"#,
        )
        .unwrap();
        assert!(!is_tanstack_start_app(&app2));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_ssr_module_endpoint_only_reads_the_project_and_its_dependencies() {
        let base = tmp("ssr-allow");
        let root = base.join("app");
        let outside = base.join("elsewhere");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/react")).unwrap();
        std::fs::create_dir_all(outside.join("secrets")).unwrap();
        std::fs::create_dir_all(base.join("linked/node_modules/dep")).unwrap();
        std::fs::create_dir_all(base.join("allowed")).unwrap();
        std::fs::write(root.join("src/App.tsx"), "x").unwrap();
        std::fs::write(root.join("node_modules/react/index.js"), "x").unwrap();
        std::fs::write(outside.join("secrets/id_rsa"), "x").unwrap();
        std::fs::write(base.join("linked/node_modules/dep/index.js"), "x").unwrap();
        std::fs::write(base.join("allowed/shared.ts"), "x").unwrap();

        let mut allow = std::collections::HashSet::new();
        allow.insert(base.join("allowed"));

        // The project, its dependencies, and what `server.fs.allow` named.
        for ok in [
            root.join("src/App.tsx"),
            root.join("node_modules/react/index.js"),
            base.join("linked/node_modules/dep/index.js"),
            base.join("allowed/shared.ts"),
        ] {
            assert!(module_read_allowed(&root, &allow, &ok), "{ok:?} denied");
        }

        // Everything else, however it is spelled.
        for denied in [
            outside.join("secrets/id_rsa"),
            root.join("../elsewhere/secrets/id_rsa"),
            root.join("src/../../elsewhere/secrets/id_rsa"),
        ] {
            assert!(
                !module_read_allowed(&root, &allow, &denied),
                "{denied:?} allowed"
            );
        }

        // A virtual id is not a filesystem read: the plugin host is the only
        // thing that can resolve it, so it passes through.
        for virtual_id in ["virtual:oj-routes", "\0virtual:x", "plugin:generated"] {
            assert!(module_read_allowed(&root, &allow, Path::new(virtual_id)));
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_out_of_the_project_does_not_widen_what_can_be_read() {
        let base = tmp("ssr-symlink");
        let root = base.join("app");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(base.join("secrets")).unwrap();
        std::fs::write(base.join("secrets/id_rsa"), "x").unwrap();
        let link = root.join("src/escape.ts");
        if std::os::unix::fs::symlink(base.join("secrets/id_rsa"), &link).is_ok() {
            assert!(
                !module_read_allowed(&root, &std::collections::HashSet::new(), &link),
                "a symlink inside the project must not expose its target"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn locate_prefers_root_then_public_dir() {
        let base = tmp("locate");
        let root = base.join("root");
        let public = base.join("shared-public");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(public.join("img")).unwrap();
        std::fs::write(root.join("src").join("App.tsx"), "x").unwrap();
        std::fs::write(public.join("img").join("logo.webp"), "y").unwrap();

        assert_eq!(
            locate(&root, Some(&public), "src/App"),
            Some(root.join("src/App.tsx"))
        );
        assert_eq!(
            locate(&root, Some(&public), "img/logo.webp"),
            Some(public.join("img/logo.webp"))
        );
        assert_eq!(locate(&root, Some(&public), "img/missing.webp"), None);
        assert_eq!(locate(&root, Some(&public), "../secret"), None);
        // `publicDir: false`: only the root is searched.
        assert_eq!(locate(&root, None, "img/logo.webp"), None);
        assert_eq!(
            locate(&root, None, "src/App"),
            Some(root.join("src/App.tsx"))
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // --- adverse: request paths are attacker-shaped strings ---

    #[test]
    fn urldecode_handles_every_malformed_escape() {
        assert_eq!(urldecode("plain/path.tsx"), "plain/path.tsx");
        assert_eq!(urldecode("a%20b"), "a b");
        assert_eq!(urldecode("a%2Fb"), "a/b");
        assert_eq!(urldecode("caf%C3%A9.css"), "café.css");
        assert_eq!(urldecode("100%25.css"), "100%.css");
        // Truncated and non-hex escapes are left alone rather than dropped.
        assert_eq!(urldecode("a%"), "a%");
        assert_eq!(urldecode("a%2"), "a%2");
        assert_eq!(urldecode("a%zz"), "a%zz");
        assert_eq!(urldecode("a%%20"), "a% ");
        assert_eq!(urldecode(""), "");
        // A single pass only: an encoded escape stays encoded once decoded.
        assert_eq!(urldecode("a%2520b"), "a%20b");
        // Invalid UTF-8 becomes replacement characters instead of panicking.
        assert!(!urldecode("%ff%fe").is_empty());
    }

    #[test]
    fn locate_rejects_traversal_in_every_spelling_after_decoding() {
        let base = tmp("traversal");
        let root = base.join("root");
        let public = base.join("public");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&public).unwrap();
        std::fs::write(base.join("secret.txt"), "s3cret").unwrap();
        std::fs::write(root.join("src").join("App.tsx"), "x").unwrap();

        for hostile in [
            "../secret.txt",
            "src/../../secret.txt",
            "./../secret.txt",
            "src/../../../etc/passwd",
        ] {
            assert_eq!(locate(&root, Some(&public), hostile), None, "{hostile}");
            // ...and through the decoding the request handler applies first.
            let encoded = hostile.replace("..", "%2e%2e");
            assert_eq!(
                locate(&root, Some(&public), &urldecode(&encoded)),
                None,
                "{encoded}"
            );
        }
        // A name that merely contains dots is not traversal.
        std::fs::write(root.join("src").join("..dotted.tsx"), "x").unwrap();
        assert!(locate(&root, Some(&public), "src/..dotted.tsx").is_some());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn locate_finds_files_whose_names_need_encoding() {
        let base = tmp("encoded-names");
        let root = base.join("root");
        let public = base.join("public");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(&public).unwrap();
        for name in ["Cool Button.tsx", "café.css", "100%.css", "a+b.tsx"] {
            std::fs::write(root.join("src").join(name), "x").unwrap();
        }

        // What a browser actually requests for each of those files.
        for (requested, name) in [
            ("src/Cool%20Button.tsx", "Cool Button.tsx"),
            ("src/caf%C3%A9.css", "café.css"),
            ("src/100%25.css", "100%.css"),
            ("src/a+b.tsx", "a+b.tsx"),
        ] {
            let decoded = urldecode(requested);
            assert_eq!(
                locate(&root, Some(&public), &decoded),
                Some(root.join("src").join(name)),
                "{requested}"
            );
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn preview_rel_decodes_before_guarding_traversal() {
        assert_eq!(
            preview_rel("/assets/Cool%20Button.js", "/").as_deref(),
            Some("assets/Cool Button.js")
        );
        assert_eq!(preview_rel("/%2e%2e/etc/passwd", "/"), None);
        assert_eq!(preview_rel("/a/%2e%2e/%2e%2e/etc/passwd", "/"), None);
        assert_eq!(preview_rel("/%2E%2E/etc/passwd", "/"), None);
        // Not traversal: `..` inside a segment.
        assert_eq!(preview_rel("/a..b/x.js", "/").as_deref(), Some("a..b/x.js"));
    }

    #[test]
    fn html_entry_src_rejects_everything_that_is_not_a_local_path() {
        for external in [
            "",
            "   ",
            "http://cdn.example/x.js",
            "https://cdn.example/x.js",
            "//cdn.example/x.js",
            "data:text/javascript,alert(1)",
        ] {
            assert_eq!(html_entry_src(external), None, "{external:?}");
        }
        assert_eq!(
            html_entry_src("./src/main.tsx").as_deref(),
            Some("/src/main.tsx")
        );
        assert_eq!(
            html_entry_src("src/main.tsx").as_deref(),
            Some("/src/main.tsx")
        );
        assert_eq!(
            html_entry_src("/src/main.tsx").as_deref(),
            Some("/src/main.tsx")
        );
        assert_eq!(
            html_entry_src("  src/main.tsx  ").as_deref(),
            Some("/src/main.tsx")
        );
        // A traversal in an entry src stays a path; `locate` is what refuses it.
        assert_eq!(
            html_entry_src("../../etc/passwd").as_deref(),
            Some("/../../etc/passwd")
        );
    }

    #[test]
    fn gate_relevance_ignores_generated_directories() {
        assert!(gate_relevant(Path::new("/app/src/App.tsx")));
        assert!(!gate_relevant(Path::new(
            "/app/node_modules/react/index.js"
        )));
        assert!(!gate_relevant(Path::new("/app/.oj-cache/ab/cd.json")));
        assert!(!gate_relevant(Path::new("/app/dist/assets/x.js")));
        // Only a whole path component counts.
        assert!(gate_relevant(Path::new("/app/src/dist-helper.ts")));
        assert!(gate_relevant(Path::new(
            "/app/src/my-node_modules-thing.ts"
        )));
    }

    #[test]
    fn query_classification_only_matches_whole_flags() {
        assert!(is_worker_query("/w.ts?worker"));
        assert!(is_worker_query("/w.ts?sharedworker"));
        assert!(!is_worker_query("/w.ts?workerish"));
        assert!(!is_worker_query("/w.ts?x=worker"));
        assert!(!is_worker_query("/w.ts"));
        assert!(!is_worker_query(""));
    }

    #[test]
    fn worker_beats_inline_in_asset_kind_classification() {
        // ?worker&inline must classify as a worker (inline is a modifier), not as
        // a generic inline asset that would 404 as a base64 data URI of the file.
        assert_eq!(query_asset_kind(Some("worker&inline")), Some("worker"));
        assert_eq!(
            query_asset_kind(Some("sharedworker&inline")),
            Some("sharedworker")
        );
        assert_eq!(query_asset_kind(Some("inline")), Some("inline"));
        assert_eq!(query_asset_kind(Some("worker")), Some("worker"));
    }

    #[test]
    fn is_spa_navigation_rules() {
        let empty = HeaderMap::new();
        assert!(is_spa_navigation("dashboard", &empty));
        assert!(is_spa_navigation("projects/abc", &empty));
        assert!(!is_spa_navigation("main.js", &empty));
        assert!(!is_spa_navigation("@vite/client", &empty));
        assert!(!is_spa_navigation("src/App.tsx", &empty));
        assert!(!is_spa_navigation("node_modules/react/index.js", &empty));
        let mut html = HeaderMap::new();
        html.insert(header::ACCEPT, "text/html,*/*".parse().unwrap());
        assert!(is_spa_navigation("some.thing", &html));
    }

    #[test]
    fn loopback_headers_carry_host_as_x_oj_host_and_pass_forwarded_host_through() {
        let mut h = HeaderMap::new();
        h.insert(header::HOST, "localhost:8080".parse().unwrap());
        h.append("x-forwarded-host", "app.example.com".parse().unwrap());
        h.append("x-forwarded-host", "edge.example.com".parse().unwrap());
        h.insert("x-oj-host", "spoofed.example.com".parse().unwrap());
        h.insert(header::ACCEPT, "text/html".parse().unwrap());
        let out = loopback_request_headers(&h);
        let get = |n: &str| {
            out.iter()
                .filter(|(k, _)| k.as_str() == n)
                .map(|(_, v)| v.to_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(get("x-oj-host"), ["localhost:8080"]);
        assert_eq!(
            get("x-forwarded-host"),
            ["app.example.com", "edge.example.com"]
        );
        assert!(
            get("host").is_empty(),
            "hyper writes the loopback Host itself"
        );
        assert_eq!(get("accept"), ["text/html"]);
        assert!(loopback_request_headers(&HeaderMap::new()).is_empty());
    }

    #[test]
    fn content_changes_ignore_attribute_only_events_unless_the_mtime_moved() {
        use notify::event::{AccessKind, DataChange, MetadataKind, ModifyKind};
        let dir = std::env::temp_dir().join(format!("oj-content-changes-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("a.ts");
        std::fs::write(&file, "export const a = 1;").unwrap();
        let meta = || {
            notify::Event::new(notify::EventKind::Modify(ModifyKind::Metadata(
                MetadataKind::Any,
            )))
            .add_path(file.clone())
        };
        let data = || {
            notify::Event::new(notify::EventKind::Modify(ModifyKind::Data(
                DataChange::Content,
            )))
            .add_path(file.clone())
        };
        let access = || {
            notify::Event::new(notify::EventKind::Access(AccessKind::Read)).add_path(file.clone())
        };
        // A never-seen file with an OLD mtime: the relatime atime storm shape.
        let old_mtime = std::time::SystemTime::now() - std::time::Duration::from_secs(3600);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(old_mtime)
            .unwrap();
        let mut changes = ContentChanges::new();
        assert!(
            changes.changed_paths(&meta()).is_empty(),
            "an atime update on a never-seen old file is not a change"
        );
        assert!(
            changes.changed_paths(&meta()).is_empty(),
            "nor is a repeat with the same mtime"
        );
        assert!(changes.changed_paths(&access()).is_empty());
        // A never-seen file whose mtime is fresh: a touch, which must count once.
        let touched = dir.join("touched.ts");
        std::fs::write(&touched, "export const t = 1;").unwrap();
        let meta_touched = notify::Event::new(notify::EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Any,
        )))
        .add_path(touched.clone());
        assert_eq!(
            changes.changed_paths(&meta_touched),
            vec![touched.clone()],
            "a touch on a never-seen file is a change"
        );
        let meta_touched = notify::Event::new(notify::EventKind::Modify(ModifyKind::Metadata(
            MetadataKind::Any,
        )))
        .add_path(touched.clone());
        assert!(
            changes.changed_paths(&meta_touched).is_empty(),
            "and is then the baseline"
        );
        assert_eq!(
            changes.changed_paths(&data()),
            vec![file.clone()],
            "a data change always counts"
        );
        assert!(
            changes.changed_paths(&meta()).is_empty(),
            "the mtime the data change recorded has not moved"
        );
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert_eq!(
            changes.changed_paths(&meta()),
            vec![file.clone()],
            "a moved mtime (touch) is a change, as in chokidar"
        );
        assert!(
            changes.changed_paths(&meta()).is_empty(),
            "and is then the new baseline"
        );
        std::fs::remove_file(&file).unwrap();
        assert_eq!(
            changes.changed_paths(&meta()),
            vec![file.clone()],
            "a vanished file is a change"
        );
        let removed =
            notify::Event::new(notify::EventKind::Remove(notify::event::RemoveKind::File))
                .add_path(file.clone());
        assert_eq!(changes.changed_paths(&removed), vec![file.clone()]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
