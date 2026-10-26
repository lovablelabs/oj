use super::*;
use std::sync::Arc;

/// The host's own plugin container: the environment it boots in.
pub(crate) const MAIN_CONTAINER: u32 = 0;
/// The first id of the containers [`PluginHost::open_environment_container`]
/// opens; worker containers count up from 1 below it.
const ENVIRONMENT_CONTAINERS: u32 = 1 << 20;

/// A handle on one plugin container of a plugin host: the host's own
/// (MAIN_CONTAINER), or a worker bundle's ([`PluginHost::open_worker_container`]).
/// Every container lives in the one engine and realm the host runs.
pub struct PluginHost {
    pub(crate) core: std::sync::Arc<HostCore>,
    /// The container this handle's hook calls address.
    pub(crate) container: u32,
    /// A worker container's engine generation: a revived engine numbers its
    /// containers afresh, so the handle answers nothing past it. `None` for
    /// the host's own container, which every generation re-creates.
    generation: Option<u64>,
    /// Live hook plan: starts fail-open, overwritten only by a successful fetch,
    /// reverts to fail-open on engine respawn; a failed fetch is never cached.
    hook_plan: std::sync::RwLock<BuildHookPlan>,
    hook_plan_fetched: std::sync::atomic::AtomicBool,
    hook_plan_prime_started: std::sync::atomic::AtomicBool,
    /// Bumped by every reset: a fetch that began before one (a revive's
    /// refetch racing an environment switch) answered for the old plugin set
    /// and must not land.
    hook_plan_epoch: std::sync::atomic::AtomicU64,
    /// Whether this container's plugins have `watchChange` / `hotUpdate`,
    /// fetched with the hook plan; true until fetched (an HMR dispatch is
    /// never skipped by mistake).
    watch_change: std::sync::atomic::AtomicBool,
    hot_update: std::sync::atomic::AtomicBool,
    /// The environment containers declared at spawn (the host's own handle
    /// only): opened by the host's own init, before its configureServer.
    declared: std::sync::OnceLock<Vec<(String, std::sync::Arc<PluginHost>)>>,
}

/// A plugin host's engine and its lifecycle (boot, pushes, revive), shared by
/// every container on it.
pub(crate) struct HostCore {
    /// Engine hosting plugin-host.mjs; an Option so `declare_gone`/`shutdown` can
    /// take + abandon it. A thread wedged in NATIVE code leaks with its isolate.
    engine: Mutex<Option<std::sync::Arc<oj_js::JsEngine>>>,
    /// The plugin-host.mjs path on disk, the module every hook call targets.
    host_module: String,
    /// Wired once at server boot (the reader clones per push, lock-free).
    ws_out: std::sync::OnceLock<tokio::sync::broadcast::Sender<String>>,
    /// `{ ojServer: { action, ... } }` pushes from the host: a plugin invalidating
    /// a module via server.moduleGraph, or server.restart(). Wired once.
    server_events: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<serde_json::Value>>,
    /// The host's async reverse RPC (`{ rpc, method, args, container }`
    /// pushes), routed to the asking container's handler, which answers with
    /// [`PluginHost::rpc_reply`].
    rpc_routes: Mutex<std::collections::HashMap<u32, tokio::sync::mpsc::UnboundedSender<HostRpc>>>,
    /// Host's `{ ojServeInfo }` push: None until top-level init completes; late
    /// subscribers can still activate the middleware path.
    serve_info_push: tokio::sync::watch::Sender<Option<ServeInfo>>,
    /// Host finished top-level init: flipped by the serve-info push, `{ ojInit }`,
    /// or the first hook reply. Hook calls gate on this; see `call`.
    initialized: tokio::sync::watch::Sender<bool>,
    /// Host is gone (engine thread exited, init failed, or declared wedged): fail
    /// calls fast. A watch so waiters select on the death instead of polling.
    pub(crate) host_gone: tokio::sync::watch::Sender<bool>,
    /// When the CURRENT generation spawned (reset by a revive); the init warn
    /// bound reporting tools consult is measured from here.
    spawned: Mutex<tokio::time::Instant>,
    /// Wedge EVIDENCE, not a call gate: true when a full stall window passed
    /// with no milestone, false again on progress. Calls never consult it; waiters do.
    init_failed: tokio::sync::watch::Sender<bool>,
    /// Count of `{ ojResyncDone }` pushes (a resync EXECUTED; the ack only means
    /// enqueued). A counter so a completion landing before the wait starts is never missed.
    resync_done: tokio::sync::watch::Sender<u64>,
    /// Count of `{ ojInitProgress }` pushes (init milestones). The stall monitor
    /// measures its wedge window from the LAST milestone, so a slow but progressing boot is never wedged.
    init_progress_seen: tokio::sync::watch::Sender<u64>,
    /// How often a call waiting on a running hook probes the engine for
    /// liveness (and logs progress); never a budget the hook must finish
    /// within. Tests compress it.
    probe_wait: std::time::Duration,
    /// Last "still initializing" progress line, so concurrent init-gated calls
    /// print one line per interval, not one each.
    init_progress: Mutex<std::time::Instant>,
    /// Everything a respawn needs to boot a fresh generation (boot seed, root,
    /// stall window), snapshotted at spawn: a respawn boots the same host on a fresh isolate.
    boot: BootContext,
    /// Revive budget and engine GENERATION under one lock: a death report about
    /// a replaced engine is stale and must not kill the freshly revived host.
    pub(crate) revive: Mutex<ReviveState>,
    /// Set by `shutdown()`: retired on purpose, must never be revived.
    shut_down: std::sync::atomic::AtomicBool,
    /// Weak self-handle so `&self` methods can hand a fresh generation's tasks their `Arc`s.
    self_ref: std::sync::OnceLock<std::sync::Weak<HostCore>>,
    /// The handle on the host's own container, whose hook plan a respawn resets.
    main: std::sync::OnceLock<std::sync::Weak<PluginHost>>,
    /// The environment containers opened on this host, as each boot reopens
    /// them (their specs ride the boot seed), with the handles whose hook
    /// plans a respawn resets.
    pub(crate) environments: Mutex<Vec<(serde_json::Value, std::sync::Weak<PluginHost>)>>,
    next_environment: std::sync::atomic::AtomicU32,
}

/// The dev listener's bound (port, interface): hosts booting after the bind
/// read it in `ignite`; hosts already up are announced via [`LIVE_HOSTS`].
pub(crate) static DEV_LISTENER: Mutex<Option<(u16, String)>> = Mutex::new(None);
/// Every spawned host, weakly: the bind-time announce must reach hosts booted
/// before the listener existed (the normal boot order).
pub(crate) static LIVE_HOSTS: Mutex<Vec<std::sync::Weak<HostCore>>> = Mutex::new(Vec::new());

/// Dev listener bound: remember for hosts yet to boot, tell every live one.
/// The stub `httpServer` emits "listening" only on this signal (Vite parity: the socket really accepts).
pub fn dev_listener_bound(port: u16, interface: &str) {
    *DEV_LISTENER.lock().unwrap() = Some((port, interface.to_string()));
    LIVE_HOSTS.lock().unwrap().retain(|w| match w.upgrade() {
        Some(host) => {
            host.announce_dev_listener(port, interface);
            true
        }
        None => false,
    });
}

/// An engine started ahead of its host (see [`PluginHost::prewarm`]).
pub struct HostPrewarm {
    engine: std::sync::Arc<oj_js::JsEngine>,
    post_rx: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
    import_meta_env: Arc<std::sync::OnceLock<Arc<oj_compiler::ImportMetaEnv>>>,
    bundles: BundleGraphs,
    registry: Option<oj_js::EngineRegistry>,
}

fn new_host_engine(
    root: &Path,
    memory_limit_bytes: usize,
    registry: Option<oj_js::EngineRegistry>,
    import_meta_env: Arc<std::sync::OnceLock<Arc<oj_compiler::ImportMetaEnv>>>,
    bundles: BundleGraphs,
    post_tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
) -> Result<oj_js::JsEngine, String> {
    let root = root.to_path_buf();
    let resolver = std::sync::Arc::new(OjResolver::new(&root));
    let preseeds = Preseeds {
        runtime: tokio::runtime::Handle::try_current().ok(),
        ..Preseeds::default()
    };
    let rpc_handler: oj_js::RpcHandler = {
        let root = root.clone();
        Box::new(move |method, args| {
            bundle_rpc(method, args, &bundles)
                .or_else(|| preseeds.rpc(method, args, &root))
                .unwrap_or_else(|| ctx_rpc(method, args, &resolver, &root, import_meta_env.get()))
        })
    };
    let mut engine_config = oj_js::EngineConfig::new(&root);
    engine_config.code_cache_dir = Some(crate::engine_code_cache_dir(&root));
    engine_config.memory_limit_bytes = Some(memory_limit_bytes);
    engine_config.registry = registry;
    oj_js::JsEngine::spawn(
        engine_config,
        None,
        Some(oj_js::EngineHooks {
            post: post_tx,
            rpc: Some(rpc_handler),
        }),
    )
    .map_err(|e| format!("cannot start the embedded plugin host: {e}"))
}

/// Writes the boot module the host's engine calls before the host module.
fn ensure_boot_module(root: &Path) -> Result<String, String> {
    let dir = oj_cache::cache_root(root);
    ensure_asset(&dir, "vite-config-node.mjs", VITE_CONFIG_NODE_JS).map_err(|e| e.to_string())?;
    ensure_asset(&dir, "plugin-host-boot.mjs", PLUGIN_HOST_BOOT_JS).map_err(|e| e.to_string())?;
    Ok(dir
        .join("plugin-host-boot.mjs")
        .to_string_lossy()
        .into_owned())
}

/// See [`HostCore::boot`].
pub(crate) struct BootContext {
    boot_seed: serde_json::Value,
    root: PathBuf,
    stall_wait: std::time::Duration,
    /// Heap cap every generation is spawned with (see `plugin_host_memory_mb`).
    memory_limit_bytes: usize,
    /// Rides into every generation's engine (and the addon keeper), so the
    /// debug GC fan-out keeps reaching this host across respawns.
    registry: Option<oj_js::EngineRegistry>,
    /// The dev server's defines, for `moduleInfo` compiles; set once after spawn.
    import_meta_env: Arc<std::sync::OnceLock<Arc<oj_compiler::ImportMetaEnv>>>,
    /// The bundle each container's hooks run inside (see [`BundleGraph`]).
    bundles: BundleGraphs,
}

/// See [`HostCore::revive`].
pub(crate) struct ReviveState {
    /// The live engine's generation; bumped by each revive.
    pub(crate) generation: u64,
    /// Respawns consumed: a LIFETIME budget, never reset by a successful boot
    /// (a recurring wedge would respawn forever, leaking a wedged isolate thread each time).
    pub(crate) attempts: u32,
    /// When the last revive ran: spacing so a burst cannot burn the whole budget at once.
    pub(crate) last: Option<std::time::Instant>,
    /// Addons pending unsafe re-registration when THIS host died (snapshotted
    /// before the abandon): the revive gate refuses only on ones the dead generation itself orphaned.
    pub(crate) pending_before: std::collections::HashSet<PathBuf>,
    /// This generation's death was declared: only the first report may snapshot
    /// `pending_before` (a later one would fold in the dead generation's own orphans). Reset by each revive.
    pub(crate) reported: bool,
}

/// First report for the live generation wins; stale-generation reports and
/// duplicates are dropped. Serialized by the caller holding the `revive` lock.
pub(crate) fn first_death_report(revive: &mut ReviveState, generation: u64) -> bool {
    if revive.generation != generation || revive.reported {
        return false;
    }
    revive.reported = true;
    true
}

/// Respawns per host lifetime (see `ReviveState::attempts`).
pub(crate) const PLUGIN_HOST_RESPAWN_LIMIT: u32 = 3;
/// Minimum spacing between respawns (see `ReviveState::last`).
pub(crate) const PLUGIN_HOST_RESPAWN_SPACING: std::time::Duration =
    std::time::Duration::from_secs(5);

/// Plugin-host heap cap, Node parity: `OJ_PLUGIN_MEMORY_MB`, then NODE_OPTIONS
/// `--max-old-space-size`, then 4096MB. Near-limit fails the call with MemoryLimit and revives on a fresh heap.
pub(crate) fn plugin_host_memory_mb() -> usize {
    let knobs = &oj_env::get().knobs;
    if let Some(mb) = knobs.plugin_memory_mb {
        return mb;
    }
    knobs
        .node_options
        .as_deref()
        .and_then(max_old_space_mb)
        .unwrap_or(4096)
}

/// The last `--max-old-space-size` in NODE_OPTIONS (last wins, like Node;
/// underscore spelling accepted too).
pub(crate) fn max_old_space_mb(node_options: &str) -> Option<usize> {
    let mut found = None;
    for token in node_options.split_whitespace() {
        let norm = token.replace('_', "-");
        if let Some(v) = norm.strip_prefix("--max-old-space-size=") {
            if let Some(mb) = v.parse::<usize>().ok().filter(|m| *m > 0) {
                found = Some(mb);
            }
        }
    }
    found
}

/// Process-wide native-addon KEEPER: a hidden engine pre-registers live addons before
/// a dying host's teardown orphans them, so a respawn re-registers into the safe concurrent-envs case.
pub(crate) static ADDON_KEEPER: Mutex<Option<std::sync::Arc<oj_js::JsEngine>>> = Mutex::new(None);

/// Keeper budget, deliberately UNDER `PLUGIN_HOST_RESPAWN_SPACING`: by the time
/// a revive is allowed the keeper has either registered or given up, never in between.
pub(crate) const ADDON_KEEPER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(4);

/// Load `addons` into the keeper engine (spawned on first use), each best-effort:
/// one addon failing to require must not cost the others their keeper.
pub(crate) async fn keep_addons_alive(
    root: &Path,
    addons: &[PathBuf],
    registry: Option<oj_js::EngineRegistry>,
) -> Result<(), String> {
    let engine = {
        let mut keeper = ADDON_KEEPER.lock().unwrap();
        match &*keeper {
            Some(engine) => std::sync::Arc::clone(engine),
            None => {
                let engine = std::sync::Arc::new({
                    let mut config = oj_js::EngineConfig::new(root);
                    config.registry = registry;
                    oj_js::JsEngine::spawn(config, None, None)
                        .map_err(|e| format!("keeper engine failed to spawn: {e}"))?
                });
                *keeper = Some(std::sync::Arc::clone(&engine));
                engine
            }
        }
    };
    // Re-filter at load time: an unrelated engine dying since the snapshot
    // orphans its addons, and requiring one would itself be the dangerous re-registration.
    let live: std::collections::HashSet<PathBuf> = oj_js::addons_with_live_registrations()
        .into_iter()
        .collect();
    let paths = serde_json::to_string(
        &addons
            .iter()
            .filter(|p| live.contains(*p))
            .map(|p| p.to_string_lossy())
            .collect::<Vec<_>>(),
    )
    .map_err(|e| e.to_string())?;
    // The anchor only seats resolution; it does not need to exist.
    let anchor = serde_json::to_string(&root.join("package.json").to_string_lossy())
        .map_err(|e| e.to_string())?;
    let script = format!(
        r#"import {{ createRequire }} from "node:module";
const req = createRequire({anchor});
globalThis.__ojAddonKeeper ??= [];
for (const p of {paths}) {{
    try {{ globalThis.__ojAddonKeeper.push(req(p)); }} catch {{}}
}}
"#
    );
    engine
        .eval(
            oj_js::EvalInput::Source(script),
            Some(ADDON_KEEPER_DEADLINE),
        )
        .await
        .map(|_| ())
        .map_err(|e| format!("keeper load failed: {e}"))
}

/// Process RSS in MB, best effort, for host-death diagnostics: /proc on Linux,
/// getrusage's PEAK elsewhere.
pub(crate) fn process_rss_mb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        let line = status.lines().find(|l| l.starts_with("VmRSS:"))?;
        let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
        return Some(kb / 1024);
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::zeroed();
        if unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) } != 0 {
            return None;
        }
        let max_rss = unsafe { usage.assume_init() }.ru_maxrss.max(0) as u64;
        // macOS reports bytes; the BSDs report kilobytes.
        if cfg!(target_os = "macos") {
            return Some(max_rss / (1024 * 1024));
        }
        return Some(max_rss / 1024);
    }
    #[allow(unreachable_code)]
    None
}

/// One async reverse RPC from the host, for work that calls back into it.
#[derive(Debug)]
pub struct HostRpc {
    pub id: u64,
    pub method: String,
    pub args: Vec<serde_json::Value>,
}

/// The host's reverse ctx-RPC (`this.resolve` fallbacks, `this.load` module
/// info), answered SYNCHRONOUSLY on the isolate thread via the `__oj_rpc` bridge.
pub(crate) fn ctx_rpc(
    method: &str,
    args: &[serde_json::Value],
    resolver: &OjResolver,
    root: &Path,
    env: Option<&Arc<oj_compiler::ImportMetaEnv>>,
) -> Result<serde_json::Value, String> {
    let arg = |i: usize| args.get(i).and_then(|v| v.as_str()).unwrap_or("");
    let dir_of = |path: &Path| {
        path.parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| root.to_path_buf())
    };
    match method {
        "resolve" => {
            let (source, importer) = (arg(0), arg(1));
            let dir = if importer.is_empty() {
                root.to_path_buf()
            } else {
                dir_of(Path::new(importer))
            };
            Ok(match resolver.resolve(&dir, source) {
                Ok(p) => serde_json::Value::String(p.display().to_string()),
                Err(_) => serde_json::Value::Null,
            })
        }
        "moduleInfo" => {
            let id = arg(0);
            let path = Path::new(id);
            let Ok(src) = std::fs::read_to_string(path) else {
                return Ok(serde_json::Value::Null);
            };
            let dir = dir_of(path);
            let opts = oj_compiler::CompileOptions {
                env: env.cloned(),
                ..oj_compiler::CompileOptions::prod()
            };
            let (code, imports) = match oj_compiler::compile(path, &src, &opts) {
                Ok(out) => (out.code, out.imports),
                Err(_) => (src, Vec::new()),
            };
            let imported_ids: Vec<String> = imports
                .iter()
                .map(|spec| {
                    resolver
                        .resolve(&dir, spec)
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|_| spec.clone())
                })
                .collect();
            Ok(serde_json::json!({ "id": id, "code": code, "importedIds": imported_ids }))
        }
        other => Err(format!("unknown ctx method: {other}")),
    }
}

/// The deps pre-seeds of the dev server setup's registrations (plugin-host.mjs
/// `seedBootRegistrations`): asked for and polled over the synchronous
/// bridge, which a host still in its top-level init can use (a hook reply
/// cannot reach it then, and the setup may be what waits on the seed).
#[derive(Default)]
struct Preseeds {
    runs: Arc<std::sync::Mutex<std::collections::HashMap<u64, Option<bool>>>>,
    next: std::sync::atomic::AtomicU64,
    /// The runtime the engine was made on: the bridge runs on the isolate's thread.
    runtime: Option<tokio::runtime::Handle>,
}

impl Preseeds {
    /// `preseedRegistered(mode, registrations)` starts one and returns its
    /// id; `preseedRegisteredDone(id)` is null while it runs, then whether
    /// the child committed. None for any other method.
    fn rpc(
        &self,
        method: &str,
        args: &[serde_json::Value],
        root: &Path,
    ) -> Option<Result<serde_json::Value, String>> {
        let arg = |i: usize| {
            args.get(i)
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        };
        match method {
            "preseedRegistered" => {
                let Some(runtime) = &self.runtime else {
                    return Some(Ok(serde_json::Value::Null));
                };
                let id = self.next.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                self.runs.lock().unwrap().insert(id, None);
                let (runs, root, mode, registered) =
                    (Arc::clone(&self.runs), root.to_path_buf(), arg(0), arg(1));
                runtime.spawn(async move {
                    let seeded = crate::preseed::seed_registered(&root, &mode, &registered).await;
                    runs.lock().unwrap().insert(id, Some(seeded));
                });
                Some(Ok(serde_json::json!(id)))
            }
            "preseedRegisteredDone" => {
                let id = args.first().and_then(|v| v.as_u64()).unwrap_or(u64::MAX);
                let mut runs = self.runs.lock().unwrap();
                Some(Ok(match runs.get(&id) {
                    Some(Some(seeded)) => {
                        let seeded = *seeded;
                        runs.remove(&id);
                        serde_json::Value::Bool(seeded)
                    }
                    Some(None) => serde_json::Value::Null,
                    None => serde_json::Value::Bool(false),
                }))
            }
            _ => None,
        }
    }
}

/// The bundle a plugin container's hooks run inside. Its `this.getModuleInfo`
/// and `this.getModuleIds` read this graph as Rollup's plugin context reads
/// its own: every module the bundler has met is in it, whether or not a JS
/// hook ran for the module (the hook gate skips most).
pub trait BundleGraph: Send + Sync {
    /// Rollup's ModuleInfo fields but `code`, as JSON, or None for a module
    /// the bundle has not met.
    fn module_info(&self, id: &str) -> Option<serde_json::Value>;
    /// The module's code as plugins read it, once loaded.
    fn module_code(&self, id: &str) -> Option<String>;
    fn module_ids(&self) -> Vec<String>;
}

/// Each container's bundle, by container id.
pub(crate) type BundleGraphs =
    Arc<std::sync::Mutex<std::collections::HashMap<u32, Arc<dyn BundleGraph>>>>;

/// `bundleModuleInfo`, `bundleModuleCode` and `bundleModuleIds` (the asking
/// container first), answered SYNCHRONOUSLY on the isolate thread from that
/// container's bundle; null while it has none. None for any other method.
fn bundle_rpc(
    method: &str,
    args: &[serde_json::Value],
    bundles: &BundleGraphs,
) -> Option<Result<serde_json::Value, String>> {
    if !matches!(
        method,
        "bundleModuleInfo" | "bundleModuleCode" | "bundleModuleIds"
    ) {
        return None;
    }
    let container = args.first().and_then(|v| v.as_u64()).unwrap_or(0) as u32;
    let graph = bundles.lock().unwrap().get(&container).cloned();
    let Some(graph) = graph else {
        return Some(Ok(serde_json::Value::Null));
    };
    let id = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
    Some(Ok(match method {
        "bundleModuleInfo" => graph.module_info(id).unwrap_or_default(),
        "bundleModuleCode" => graph
            .module_code(id)
            .map_or(serde_json::Value::Null, serde_json::Value::String),
        _ => serde_json::json!(graph.module_ids()),
    }))
}

/// How often a waiting call probes a running hook for engine liveness, and
/// the stall monitor's default no-milestone window. Vite runs plugin work
/// unbounded, so elapsed time alone never fails anything here: only process
/// death, or a probe met with total engine silence, does.
pub const PROBE_WINDOW: std::time::Duration = std::time::Duration::from_secs(20);

/// When reporting tools start calling an uninitialized boot loud (the boot
/// warning, the Start prewarm hold's outer bound). Never a call gate: calls
/// wait init out however long it takes, exactly as a Vite request can only
/// exist after `initServer` resolved.
pub(crate) const INIT_WARN: std::time::Duration = std::time::Duration::from_secs(300);

/// A `{ ojWs }` push as the HMR payload to broadcast: a custom event when it
/// names one, else the raw object. Empty means nothing to send.
fn ws_push_payload(ws: &serde_json::Value) -> String {
    match ws.get("event").and_then(|e| e.as_str()) {
        Some(event) => serde_json::json!({
            "type": "custom",
            "event": event,
            "data": ws.get("data").cloned().unwrap_or(serde_json::Value::Null),
        })
        .to_string(),
        None => ws
            .get("data")
            .filter(|d| d.is_object())
            .map(|d| d.to_string())
            .unwrap_or_default(),
    }
}

/// Abandon rides a Drop guard: a cancelled keeper task must still abandon,
/// or the engine Arc's drop would JOIN a possibly-wedged thread.
struct AbandonOnDrop(Option<std::sync::Arc<oj_js::JsEngine>>);

impl Drop for AbandonOnDrop {
    fn drop(&mut self) {
        if let Some(engine) = self.0.take() {
            engine.abandon();
        }
    }
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginHost")
    }
}

impl std::fmt::Debug for HostCore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("HostCore")
    }
}

/// A host dropped without an explicit shutdown abandons its engine: the last
/// `Arc<JsEngine>` drop must not JOIN a thread parked in a never-settling init.
impl Drop for HostCore {
    fn drop(&mut self) {
        if let Some(engine) = self
            .engine
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            engine.abandon();
        }
    }
}

/// Per-spawn window overrides for tests, compressing the probe and stall
/// windows without racing env state. Production spawns pass the default.
#[derive(Default)]
pub(crate) struct SpawnTimeouts {
    /// The stall monitor's no-progress window (defaults to the probe window).
    pub(crate) stall: Option<std::time::Duration>,
    /// The liveness probe interval (`PluginHost::probe_wait`).
    pub(crate) probe: Option<std::time::Duration>,
    /// The engine heap cap (defaults to `plugin_host_memory_mb`).
    pub(crate) memory: Option<usize>,
}

impl PluginHost {
    /// Spawn a boot-time host.
    pub async fn spawn(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        registry: Option<oj_js::EngineRegistry>,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_with_policy(
            root,
            plugins_file,
            config_json,
            SpawnTimeouts::default(),
            registry,
        )
        .await
    }

    /// Test-only spawn with the probe and stall windows explicit (see `SpawnTimeouts`).
    #[cfg(test)]
    pub(crate) async fn spawn_with_timeouts(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        timeouts: SpawnTimeouts,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_with_policy(root, plugins_file, config_json, timeouts, None).await
    }

    /// [`PluginHost::spawn`] onto the engine a [`PluginHost::prewarm`] started.
    pub async fn spawn_prewarmed(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        prewarm: HostPrewarm,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_dev(root, plugins_file, config_json, None, Some(prewarm), &[]).await
    }

    /// The dev server's host: on a prewarmed engine when there is one, with
    /// `environments` (name, leave out the reimplemented framework plugins)
    /// declared up front, so its own init opens them before its
    /// configureServer runs, as Vite's createServer creates and inits every
    /// environment first. [`PluginHost::environment`] hands them out.
    pub async fn spawn_dev(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        registry: Option<oj_js::EngineRegistry>,
        prewarm: Option<HostPrewarm>,
        environments: &[(&str, bool)],
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        let registry = prewarm
            .as_ref()
            .map(|p| p.registry.clone())
            .unwrap_or(registry);
        Self::spawn_inner(
            root,
            plugins_file,
            config_json,
            SpawnTimeouts::default(),
            registry,
            prewarm,
            environments,
        )
        .await
    }

    /// A container [`PluginHost::spawn_dev`] declared.
    pub fn environment(&self, name: &str) -> Option<std::sync::Arc<PluginHost>> {
        self.declared
            .get()?
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, h)| std::sync::Arc::clone(h))
    }

    /// The host's engine, started before its config payload exists: the
    /// engine boot and the import of the app's vite (most of a host's start)
    /// need none of it, so the dev server runs them alongside the config
    /// extraction instead of after it. The engine is configured exactly as
    /// [`PluginHost::spawn`] would; its first generation adopts it.
    pub fn prewarm(root: &Path, registry: Option<oj_js::EngineRegistry>) -> Option<HostPrewarm> {
        let boot_module = ensure_boot_module(root).ok()?;
        let (post_tx, post_rx) = tokio::sync::mpsc::unbounded_channel();
        let import_meta_env: Arc<std::sync::OnceLock<Arc<oj_compiler::ImportMetaEnv>>> =
            Arc::default();
        let bundles = BundleGraphs::default();
        let engine = new_host_engine(
            root,
            plugin_host_memory_mb() * 1024 * 1024,
            registry.clone(),
            Arc::clone(&import_meta_env),
            Arc::clone(&bundles),
            post_tx,
        )
        .ok()?;
        let engine = std::sync::Arc::new(engine);
        let anchor = serde_json::json!(root.join("package.json"));
        let warming = std::sync::Arc::clone(&engine);
        tokio::spawn(async move {
            let _ = warming
                .call(boot_module, "warmVite", vec![anchor], None)
                .await;
        });
        Some(HostPrewarm {
            engine,
            post_rx,
            import_meta_env,
            bundles,
            registry,
        })
    }

    async fn spawn_with_policy(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        timeouts: SpawnTimeouts,
        registry: Option<oj_js::EngineRegistry>,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_inner(
            root,
            plugins_file,
            config_json,
            timeouts,
            registry,
            None,
            &[],
        )
        .await
    }

    async fn spawn_inner(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        timeouts: SpawnTimeouts,
        registry: Option<oj_js::EngineRegistry>,
        mut prewarm: Option<HostPrewarm>,
        environments: &[(&str, bool)],
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        let script = oj_cache::cache_root(root).join("plugin-host.mjs");

        let probe_wait = timeouts.probe.unwrap_or(PROBE_WINDOW);
        let stall_wait = timeouts.stall.unwrap_or(probe_wait);
        let boot_seed = serde_json::json!({
            "pluginsPath": plugins_file,
            "initialJson": config_json,
            "cacheRoot": oj_cache::cache_root(root),
        });
        let core = std::sync::Arc::new(HostCore {
            engine: Mutex::new(None),
            host_module: script.to_string_lossy().into_owned(),
            ws_out: std::sync::OnceLock::new(),
            server_events: std::sync::OnceLock::new(),
            rpc_routes: Mutex::new(std::collections::HashMap::new()),
            serve_info_push: tokio::sync::watch::channel(None).0,
            initialized: tokio::sync::watch::channel(false).0,
            host_gone: tokio::sync::watch::channel(false).0,
            spawned: Mutex::new(tokio::time::Instant::now()),
            init_failed: tokio::sync::watch::channel(false).0,
            resync_done: tokio::sync::watch::channel(0).0,
            init_progress_seen: tokio::sync::watch::channel(0).0,
            probe_wait,
            init_progress: Mutex::new(std::time::Instant::now()),
            boot: BootContext {
                boot_seed,
                root: root.to_path_buf(),
                stall_wait,
                memory_limit_bytes: timeouts
                    .memory
                    .unwrap_or_else(|| plugin_host_memory_mb() * 1024 * 1024),
                registry,
                import_meta_env: prewarm
                    .as_ref()
                    .map(|p| Arc::clone(&p.import_meta_env))
                    .unwrap_or_default(),
                bundles: prewarm
                    .as_ref()
                    .map(|p| Arc::clone(&p.bundles))
                    .unwrap_or_default(),
            },
            revive: Mutex::new(ReviveState {
                generation: 0,
                attempts: 0,
                last: None,
                pending_before: std::collections::HashSet::new(),
                reported: false,
            }),
            shut_down: std::sync::atomic::AtomicBool::new(false),
            self_ref: std::sync::OnceLock::new(),
            main: std::sync::OnceLock::new(),
            environments: Mutex::new(Vec::new()),
            next_environment: std::sync::atomic::AtomicU32::new(ENVIRONMENT_CONTAINERS),
        });
        let _ = core.self_ref.set(std::sync::Arc::downgrade(&core));
        let host = std::sync::Arc::new(PluginHost::over(
            std::sync::Arc::clone(&core),
            MAIN_CONTAINER,
            None,
        ));
        let _ = core.main.set(std::sync::Arc::downgrade(&host));
        let declared = environments
            .iter()
            .map(|(name, leave_out)| {
                let (spec, handle) = core.declare_environment(name, *leave_out, false);
                core.environments
                    .lock()
                    .unwrap()
                    .push((spec, std::sync::Arc::downgrade(&handle)));
                (name.to_string(), handle)
            })
            .collect();
        let _ = host.declared.set(declared);
        LIVE_HOSTS
            .lock()
            .unwrap()
            .push(std::sync::Arc::downgrade(&core));
        let warm = prewarm.take().map(|p| (p.engine, p.post_rx));
        HostCore::ignite_with(&core, 0, warm).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(host)
    }

    pub async fn transform(
        &self,
        code: &str,
        id: &str,
        resolved: &str,
    ) -> Result<HostTransform, String> {
        let raw = self.call("transform", &[code, id, resolved]).await?;
        Ok(HostTransform::parse(raw, code))
    }

    /// One slot of a stylesheet's transform chain, around Vite's core CSS
    /// plugins: `"pre"` (the source, before vite:css compiles it), `"normal"`
    /// (the compiled CSS) or `"post"` (the JS module vite:css-post makes).
    pub async fn transform_phase(
        &self,
        code: &str,
        id: &str,
        phase: &str,
    ) -> Result<HostTransform, String> {
        let raw = self
            .call("transformPhase", &[code, id, "{}", phase])
            .await?;
        Ok(HostTransform::parse(raw, code))
    }

    pub async fn seed_chunk_names(&self, map_json: &str) -> Result<Option<String>, String> {
        self.call("seedChunkNames", &[map_json]).await
    }

    #[inline]
    pub async fn has_module_parsed(&self) -> bool {
        self.call_flag("hasModuleParsed", false).await
    }

    #[inline]
    pub async fn module_parsed(&self, id: &str) -> Result<(), String> {
        self.call_unit("replayModuleParsed", &[id]).await
    }

    /// A Start build's module_parsed: rolldown's ModuleInfo as JSON (`id`,
    /// `code`, `importedIds`, `dynamicallyImportedIds`, `importers`,
    /// `dynamicImporters`, `exports`, `isEntry`).
    #[inline]
    pub async fn bundle_module_parsed(&self, info_json: &str) -> Result<(), String> {
        self.call_unit("bundleModuleParsed", &[info_json]).await
    }

    /// Moves a Start build's host to the next environment's build, on the
    /// same plugin instances (Vite's `builder.sharedPlugins`).
    /// The next environment brings its own plugin set (`applyToEnvironment`),
    /// so the hook gates take that set's filters, as rolldown takes each
    /// environment's plugins' filters in Vite.
    #[inline]
    pub async fn switch_environment(&self, name: &str) -> Result<(), String> {
        self.call_unit("switchEnvironment", &[name]).await?;
        self.reset_hook_plan();
        self.ensure_hook_plan().await;
        Ok(())
    }

    /// Vite's `builder.buildApp`: the plugins' buildApp hooks, which reach
    /// oj's environment builds through the `buildEnvironments` reverse RPC.
    /// `built_json` names the environments oj builds (name -> `{ outDir, input }`).
    #[inline]
    pub async fn build_app(&self, built_json: &str) -> Result<(), String> {
        self.call_unit("buildApp", &[built_json]).await
    }

    #[inline]
    pub async fn resolve_id(&self, source: &str, importer: &str) -> Result<Option<String>, String> {
        self.call("resolveId", &[source, importer]).await
    }

    /// One phase of the resolveId chain for a Start bundle host: `"pre"` (the
    /// hooks Vite runs before its core resolver), `"post"` (the rest), or
    /// `"all"`. The winning result whole, `external` included.
    pub async fn resolve_id_phase(
        &self,
        source: &str,
        importer: &str,
        phase: &str,
        is_entry: bool,
    ) -> Result<Option<ResolvedId>, String> {
        let entry = if is_entry { "true" } else { "false" };
        let raw = self
            .call("resolveIdPhase", &[source, importer, phase, entry])
            .await?;
        match raw {
            None => Ok(None),
            Some(raw) => serde_json::from_str::<serde_json::Value>(&raw)
                .ok()
                .and_then(|v| ResolvedId::from_json(&v))
                .map(Some)
                .ok_or_else(|| format!("resolveId result for {source}: {raw}")),
        }
    }

    /// The live config values a Start bundle reads (plugin-host.mjs
    /// `startFacts`): aliases, externals, chunk policy, the Cloudflare Worker
    /// environment and dev-server flags, functions as `{ "$fn": n }` handles.
    pub async fn start_facts(&self) -> Result<serde_json::Value, String> {
        let raw = self.call("startFacts", &[]).await?.unwrap_or_default();
        serde_json::from_str(&raw).map_err(|e| format!("start facts: {e}"))
    }

    /// Calls the config function behind a `{ "$fn": n }` handle with JSON
    /// arguments; its JSON result.
    pub async fn call_start_fn(
        &self,
        handle: u64,
        args: &serde_json::Value,
    ) -> Result<serde_json::Value, String> {
        let raw = self
            .call("callStartFn", &[&handle.to_string(), &args.to_string()])
            .await?
            .unwrap_or_default();
        serde_json::from_str(&raw).map_err(|e| format!("config function #{handle}: {e}"))
    }

    /// A codeSplitting group function over a batch of module ids (one result
    /// per id). `infos` (module infos, as JSON) carries the ones this
    /// chunking `generation` has not sent yet. `tolerant`: a throw comes back
    /// as `{ "$threw": message }` for its id instead of failing the batch.
    pub async fn call_start_fn_batch(
        &self,
        handle: u64,
        ids: &[String],
        generation: u64,
        infos: Option<&str>,
        tolerant: bool,
    ) -> Result<Vec<serde_json::Value>, String> {
        let ids = serde_json::to_string(ids).map_err(|e| e.to_string())?;
        let raw = self
            .call(
                "callStartFnBatch",
                &[
                    &handle.to_string(),
                    &ids,
                    &generation.to_string(),
                    infos.unwrap_or(""),
                    if tolerant { "true" } else { "false" },
                ],
            )
            .await?
            .unwrap_or_default();
        serde_json::from_str(&raw).map_err(|e| format!("codeSplitting function #{handle}: {e}"))
    }

    /// The resolveId chain as the dep scan runs it (Vite's
    /// `pluginContainer.resolveId(..., { scan: true })`): the winning result
    /// whole, `external` included.
    pub(crate) async fn scan_resolve_id(
        &self,
        source: &str,
        importer: &str,
    ) -> Result<Option<ResolvedId>, String> {
        let raw = self.call("scanResolveId", &[source, importer]).await?;
        Ok(raw
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
            .and_then(|v| ResolvedId::from_json(&v)))
    }

    /// Which hooks the live `optimizeDeps.rolldownOptions.plugins` implement;
    /// `None` when the config lists none.
    pub(crate) async fn scan_plugin_hooks(
        &self,
    ) -> Result<Option<crate::prebundle::ScanPluginHooks>, String> {
        let Some(raw) = self.call("scanPluginHooks", &[]).await? else {
            return Ok(None);
        };
        let v: serde_json::Value =
            serde_json::from_str(&raw).map_err(|e| format!("scan plugin hooks: {e}"))?;
        let has = |k: &str| v.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
        Ok(Some(crate::prebundle::ScanPluginHooks {
            resolve_id: has("resolveId"),
            load: has("load"),
            transform: has("transform"),
        }))
    }

    /// One hook of `optimizeDeps.rolldownOptions.plugins` over JSON
    /// arguments: resolveId and load return the first plugin's answer,
    /// transform the code after every plugin; null when none answered.
    pub(crate) async fn scan_plugin_hook(
        &self,
        hook: &str,
        args: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, String> {
        let raw = self
            .call("scanPluginHook", &[hook, &args.to_string()])
            .await?;
        raw.map(|raw| serde_json::from_str(&raw).map_err(|e| format!("scan plugin {hook}: {e}")))
            .transpose()
    }

    #[inline]
    pub async fn load(&self, id: &str) -> Result<Option<String>, String> {
        self.call("load", &[id]).await
    }

    /// The load hooks Vite runs before its core plugins (`enforce: "pre"`,
    /// `order: "pre"`) for an id a Start bundle's own loader would claim.
    #[inline]
    pub async fn load_pre(&self, id: &str) -> Result<Option<String>, String> {
        self.call("loadPhase", &[id, "pre"]).await
    }

    #[inline]
    pub async fn handle_hot_update(
        &self,
        file: &str,
        timestamp: u64,
        change_type: &str,
        modules_json: &str,
    ) -> Result<Option<String>, String> {
        self.call(
            "handleHotUpdate",
            &[file, &timestamp.to_string(), change_type, modules_json],
        )
        .await
    }

    /// `ctx_json` is Vite's IndexHtmlTransformContext; the host adds the dev
    /// server. A throwing hook is an `Err`, as in Vite.
    #[inline]
    pub async fn transform_index_html(&self, html: &str, ctx_json: &str) -> Result<String, String> {
        Ok(self
            .call("transformIndexHtml", &[html, ctx_json])
            .await?
            .unwrap_or_else(|| html.to_string()))
    }

    /// One group of a build's transformIndexHtml hooks, as Vite's
    /// vite:build-html runs them: `"pre"` (the `order: "pre"` ones, on the
    /// page source before its scripts are read) or `"post"` (the rest, once
    /// the bundle is written).
    pub async fn transform_index_html_phase(
        &self,
        html: &str,
        ctx_json: &str,
        phase: &str,
    ) -> Result<String, String> {
        Ok(self
            .call("transformIndexHtmlPhase", &[html, ctx_json, phase])
            .await?
            .unwrap_or_else(|| html.to_string()))
    }

    #[inline]
    pub async fn build_start(&self) -> Result<Vec<ChunkEmit>, String> {
        let Some(raw) = self.call("buildStart", &[]).await? else {
            return Ok(Vec::new());
        };
        Ok(serde_json::from_str::<serde_json::Value>(&raw)
            .map(|v| emitted_chunks(&v))
            .unwrap_or_default())
    }

    /// `buildEnd(error?)`: Rollup passes the error that failed the build, so
    /// plugins see a failed build too (`None` for a successful one).
    #[inline]
    pub async fn build_end(&self, error: Option<&str>) -> Result<(), String> {
        match error {
            Some(e) => self.call_unit("buildEnd", &[e]).await,
            None => self.call_unit("buildEnd", &[]).await,
        }
    }

    #[inline]
    pub async fn render_start(&self) -> Result<(), String> {
        self.call_unit("renderStart", &[]).await
    }

    #[inline]
    pub async fn watch_change(&self, file: &str, event: &str) -> Result<(), String> {
        self.call_unit("watchChange", &[file, event]).await
    }

    #[inline]
    pub async fn close_bundle(&self) -> Result<(), String> {
        self.call_unit("closeBundle", &[]).await
    }

    /// Vite's environment close: buildEnd, then closeBundle with what
    /// buildEnd threw (the close fails with it).
    pub async fn close_environment(&self) -> Result<(), String> {
        self.call_unit("closeEnvironment", &[]).await
    }

    /// The dev server's `close()`, which plugins wrap to dispose what `configureServer` started.
    pub async fn close_dev_server(&self) -> Result<(), String> {
        self.call_unit("closeDevServer", &[]).await
    }

    #[inline]
    pub async fn watch_files(&self) -> Result<Vec<String>, String> {
        let Some(json) = self.call("getWatchFiles", &[]).await? else {
            return Ok(Vec::new());
        };
        serde_json::from_str(&json).map_err(|e| e.to_string())
    }

    #[inline]
    pub async fn has_generate_bundle(&self) -> bool {
        self.call_flag("hasGenerateBundle", false).await
    }

    /// Fails open: only an explicit "false" skips the hook, so a wedged host
    /// still fails the build at the RPC instead of shipping untransformed HTML.
    #[inline]
    pub async fn has_transform_index_html(&self) -> bool {
        !matches!(self.call("hasTransformIndexHtml", &[]).await, Ok(Some(s)) if s == "false")
    }

    /// One bounded per-host fetch of the hook filter plan, so gates run on real
    /// filters from the first dispatch; a slow or wedged host degrades to fail-open.
    pub async fn prime_hook_plan(self: &std::sync::Arc<Self>) {
        use std::sync::atomic::Ordering;
        if self.hook_plan_fetched.load(Ordering::Acquire) {
            return;
        }
        if self.hook_plan_prime_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let fetch = self.ensure_hook_plan();
        if tokio::time::timeout(std::time::Duration::from_millis(500), fetch)
            .await
            .is_err()
        {
            // Keep trying off the request path; gates stay fail-open until
            // the plan lands.
            let host = std::sync::Arc::clone(self);
            tokio::spawn(async move { host.ensure_hook_plan().await });
        }
    }

    pub async fn build_hook_plan(&self) -> BuildHookPlan {
        self.ensure_hook_plan().await;
        self.hook_plan.read().unwrap().clone()
    }

    /// Dispatch the runner environments' `server.warmup` transforms
    /// (fire-and-forget, like Vite's warmup.ts); replies once dispatched.
    /// Returns `{environments, started}`, None when nothing was warmable.
    pub async fn warm_environments(&self) -> Result<Option<String>, String> {
        self.call("warmEnvironments", &[]).await
    }

    async fn ensure_hook_plan(&self) {
        use std::sync::atomic::Ordering;
        if self.hook_plan_fetched.load(Ordering::Acquire) {
            return;
        }
        let epoch = self.hook_plan_epoch.load(Ordering::Acquire);
        // Only a successful fetch is cached; a failure leaves the fail-open
        // default in place to be retried on the next call.
        if let Some(v) = self.call_json("getBuildHookPlan").await {
            let plan = BuildHookPlan {
                transform: HookFilterPlan::from_json(v.get("transform")),
                load: HookFilterPlan::from_json(v.get("load")),
                resolve_id: HookFilterPlan::from_json(v.get("resolveId")),
            };
            let flag = |key: &str| v.get(key).and_then(|b| b.as_bool()).unwrap_or(true);
            let mut slot = self.hook_plan.write().unwrap();
            if self.hook_plan_epoch.load(Ordering::Acquire) == epoch {
                *slot = plan;
                self.watch_change
                    .store(flag("watchChange"), Ordering::Release);
                self.hot_update.store(flag("hotUpdate"), Ordering::Release);
                self.hook_plan_fetched.store(true, Ordering::Release);
            }
        }
    }

    /// Whether this container's plugins take `watchChange` (fetched with
    /// the hook plan, refetched on a revive; true until then).
    pub fn wants_watch_change(&self) -> bool {
        self.watch_change.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Whether this container's plugins take `hotUpdate` (see
    /// [`PluginHost::wants_watch_change`]).
    pub fn wants_hot_update(&self) -> bool {
        self.hot_update.load(std::sync::atomic::Ordering::Acquire)
    }

    #[inline]
    pub fn hook_wants_transform(&self, id: &str, code: &str) -> bool {
        self.hook_plan
            .read()
            .unwrap()
            .transform
            .wants(id, Some(code))
    }

    #[inline]
    pub fn hook_wants_load(&self, id: &str) -> bool {
        self.hook_plan.read().unwrap().load.wants(id, None)
    }

    #[inline]
    pub fn hook_wants_resolve_id(&self, spec: &str) -> bool {
        self.hook_plan.read().unwrap().resolve_id.wants(spec, None)
    }

    #[inline]
    pub async fn generate_bundle(
        &self,
        bundle_json: &str,
        is_write: bool,
    ) -> Result<Option<String>, String> {
        self.call("generateBundle", &[bundle_json, bool_arg(is_write)])
            .await
    }

    #[inline]
    pub async fn has_render_chunk(&self) -> bool {
        self.call_flag("hasRenderChunk", false).await
    }

    pub async fn render_chunk(
        &self,
        code: &str,
        chunk_json: &str,
    ) -> Result<HostTransform, String> {
        let raw = self.call("renderChunk", &[code, chunk_json]).await?;
        Ok(HostTransform::parse(raw, code))
    }

    #[inline]
    pub async fn has_write_bundle(&self) -> bool {
        self.call_flag("hasWriteBundle", false).await
    }

    #[inline]
    pub async fn write_bundle(&self, bundle_json: &str, is_write: bool) -> Result<(), String> {
        self.call_unit("writeBundle", &[bundle_json, bool_arg(is_write)])
            .await
    }

    /// How the host serves requests. An already-pushed value returns without a round
    /// trip; only a host that blew the init deadline yields the default (watch `serve_info_updates` for the late push).
    pub async fn serve_info(&self) -> ServeInfo {
        if let Some(info) = *self.core.serve_info_push.borrow() {
            return info;
        }
        let rpc = self.call("getServeInfo", &[]).await;
        // The push may have landed while the RPC ran; it is the definitive value.
        if let Some(info) = *self.core.serve_info_push.borrow() {
            return info;
        }
        rpc.ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
            .map(|v| ServeInfo::from_json(&v))
            .unwrap_or_default()
    }

    /// Subscribe to `{ ojServeInfo }`: `None` until init completes; lets the
    /// caller activate the plugin-middleware path late.
    pub fn serve_info_updates(&self) -> tokio::sync::watch::Receiver<Option<ServeInfo>> {
        self.core.serve_info_push.subscribe()
    }

    /// Plugins still active after oj filters out natively reimplemented ones.
    /// Defaults to 1 on RPC failure so an uncertain host is never dropped.
    pub async fn plugin_count(&self) -> usize {
        self.call_ok("getPluginCount")
            .await
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
    }

    /// The active plugins' names, in order (Vite keys its dep pre-bundle on
    /// them). Empty on RPC failure.
    pub async fn plugin_names(&self) -> Vec<String> {
        self.call_json("getPluginNames")
            .await
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default()
    }

    /// `define` entries plugin `config()` hooks contributed, as (key, js
    /// expression) pairs, reaching oj's compile like Vite's merged `config.define`. Empty on RPC failure.
    pub async fn config_defines(&self) -> Vec<(String, String)> {
        let Some(v) = self.call_json("getPluginConfig").await else {
            return Vec::new();
        };
        v.get("define")
            .and_then(|d| d.as_object())
            .map(|d| {
                d.iter()
                    .map(|(k, v)| {
                        let expr = match v {
                            serde_json::Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        (k.clone(), expr)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    pub async fn env_delta(&self) -> std::collections::BTreeMap<String, String> {
        self.call_ok("getEnvDelta")
            .await
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Whether any active plugin has a `transform` hook. Defaults to true on RPC
    /// failure so the per-module transform pass is never skipped by mistake.
    pub async fn has_transform(&self) -> bool {
        self.call_flag("getHasTransform", true).await
    }

    /// Whether any active plugin has a `resolveId` hook: only one can answer a
    /// bare import oj's resolver cannot. Defaults to true on RPC failure, so a
    /// plugin-served bare id is never failed by mistake.
    pub async fn has_resolve_id(&self) -> bool {
        self.call_flag("getHasResolveId", true).await
    }

    /// Whether any active plugin has a `load` hook (Vite runs load before the fs
    /// read). Defaults to false on RPC failure (the fs read alone is always correct).
    pub async fn has_load(&self) -> bool {
        self.call_flag("getHasLoad", false).await
    }

    /// `filter.code` include patterns of every object-form transform hook;
    /// dependency transforms are gated on these.
    pub async fn dep_transform_filters(&self) -> Vec<HookFilter> {
        self.call_ok("getDepTransformFilters")
            .await
            .map(|s| HookFilter::list_from_json(&s))
            .unwrap_or_default()
    }

    /// `filter.id` include patterns of every object-form `load` hook: deps cost
    /// no RPC unless a plugin asked for them.
    pub async fn dep_load_filters(&self) -> Vec<HookFilter> {
        self.call_ok("getDepLoadFilters")
            .await
            .map(|s| HookFilter::list_from_json(&s))
            .unwrap_or_default()
    }

    /// `filter.id` include patterns of every object-form `resolveId` hook: a
    /// non-bare import is offered to plugin resolveId only when it matches one.
    pub async fn resolve_id_filters(&self) -> Vec<HookFilter> {
        self.call_ok("getResolveIdFilters")
            .await
            .map(|s| HookFilter::list_from_json(&s))
            .unwrap_or_default()
    }

    /// Where this container's async reverse RPCs go (each container answers
    /// its own: a worker bundle's `this.resolve` ends in that bundle).
    pub fn set_rpc_request_sender(&self, tx: tokio::sync::mpsc::UnboundedSender<HostRpc>) {
        if self.is_stale() {
            return;
        }
        self.core
            .rpc_routes
            .lock()
            .unwrap()
            .insert(self.container, tx);
    }

    #[inline]
    pub async fn ws_message(&self, event: &str, data: &str) -> Result<(), String> {
        self.call_unit("wsMessage", &[event, data]).await
    }

    /// An HMR client connected: the host fires `server.ws.on("connection")`
    /// listeners (Vite's ws server emits one per accepted socket).
    #[inline]
    pub async fn ws_connection(&self) -> Result<(), String> {
        self.call_unit("wsConnection", &[]).await
    }

    #[inline]
    pub async fn emitted_files(&self) -> Result<Vec<EmittedFile>, String> {
        let Some(json) = self.call("getEmittedFiles", &[]).await? else {
            return Ok(Vec::new());
        };
        let arr: Vec<serde_json::Value> = serde_json::from_str(&json).map_err(|e| e.to_string())?;
        Ok(arr
            .into_iter()
            .filter_map(|v| {
                Some(EmittedFile {
                    file_name: v.get("fileName")?.as_str()?.to_string(),
                    source: v.get("source")?.as_str()?.to_string(),
                })
            })
            .collect())
    }

    /// CSS that plugins (e.g. UnoCSS) routed through oj's `vite:css-post` shim.
    /// Returned as `(source_id, css)` pairs.
    pub async fn get_plugin_css(&self) -> Vec<(String, String)> {
        self.call_json("getPluginCss")
            .await
            .and_then(|v| {
                v.as_array().map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            let css = e.get("css")?.as_str()?.to_string();
                            let id = e
                                .get("id")
                                .and_then(|x| x.as_str())
                                .unwrap_or("")
                                .to_string();
                            Some((id, css))
                        })
                        .collect()
                })
            })
            .unwrap_or_default()
    }

    /// One hook call into this handle's container.
    async fn call(&self, hook: &str, args: &[&str]) -> Result<Option<String>, String> {
        if self.is_stale() {
            return Err(format!(
                "plugin container {} died with its plugin host's engine",
                self.container
            ));
        }
        self.core.call(self.container, hook, args).await
    }

    /// A worker container from an engine since revived: its id may name
    /// another bundle's container in the new one.
    fn is_stale(&self) -> bool {
        self.generation
            .is_some_and(|g| g != self.core.revive.lock().unwrap().generation)
    }

    async fn call_unit(&self, hook: &str, args: &[&str]) -> Result<(), String> {
        self.call(hook, args).await.map(|_| ())
    }

    /// A no-arg query, with any failure or empty reply as `None`.
    async fn call_ok(&self, hook: &str) -> Option<String> {
        self.call(hook, &[]).await.ok().flatten()
    }

    /// A no-arg query parsed as JSON; `None` on failure or bad JSON.
    async fn call_json(&self, hook: &str) -> Option<serde_json::Value> {
        serde_json::from_str(&self.call_ok(hook).await?).ok()
    }

    /// A no-arg `"true"` query; `default` when the call fails or returns nothing.
    async fn call_flag(&self, hook: &str, default: bool) -> bool {
        match self.call(hook, &[]).await {
            Ok(Some(s)) => s == "true",
            _ => default,
        }
    }

    /// A handle on `container` of `core`, its hook plan fail-open until fetched.
    fn over(core: std::sync::Arc<HostCore>, container: u32, generation: Option<u64>) -> PluginHost {
        PluginHost {
            core,
            container,
            generation,
            hook_plan: std::sync::RwLock::new(BuildHookPlan::fail_open()),
            hook_plan_fetched: std::sync::atomic::AtomicBool::new(false),
            hook_plan_prime_started: std::sync::atomic::AtomicBool::new(false),
            hook_plan_epoch: std::sync::atomic::AtomicU64::new(0),
            watch_change: std::sync::atomic::AtomicBool::new(true),
            hot_update: std::sync::atomic::AtomicBool::new(true),
            declared: std::sync::OnceLock::new(),
        }
    }

    /// A worker entry's bundle (Vite's bundleWorkerEntry): a container of its
    /// own on this host's engine, over `worker.plugins` called afresh, in the
    /// realm of the app's plugins so the two share module state as under
    /// Vite. Its resolved config is `isWorker` with `chain`, the bundle chain.
    /// Close it with [`PluginHost::close_container`] once the bundle is done.
    pub async fn open_worker_container(&self, chain: &[String]) -> Result<PluginHost, String> {
        let chain = serde_json::to_string(chain).map_err(|e| e.to_string())?;
        let id = self
            .core
            .call(self.container, "openWorkerContainer", &[&chain])
            .await?
            .and_then(|id| id.parse::<u32>().ok())
            .ok_or("the plugin host opened no worker container")?;
        let generation = self.core.revive.lock().unwrap().generation;
        let worker = PluginHost::over(std::sync::Arc::clone(&self.core), id, Some(generation));
        // Gated by `worker.plugins`' own filters, not the app's.
        worker.ensure_hook_plan().await;
        Ok(worker)
    }

    /// Another Vite environment on this host's plugin instances (Vite's
    /// createServer gives every environment a container over the one plugin
    /// array): `name`'s, with the framework plugins oj reimplements left out
    /// when `leave_out`, its `this.resolve` ending in a bundle's own
    /// resolution when `bundle`. One of the host's own environment is that
    /// environment (one buildStart). A respawned engine reopens it.
    pub async fn open_environment_container(
        &self,
        name: &str,
        leave_out: bool,
        bundle: bool,
    ) -> Result<std::sync::Arc<PluginHost>, String> {
        let (spec, handle) = self.core.declare_environment(name, leave_out, bundle);
        let id = handle.container;
        // Recorded first: a revive racing the open reopens it from the seed.
        self.core
            .environments
            .lock()
            .unwrap()
            .push((spec.clone(), std::sync::Arc::downgrade(&handle)));
        let open = || async {
            self.core
                .call(
                    MAIN_CONTAINER,
                    "openEnvironmentContainer",
                    &[&spec.to_string()],
                )
                .await
        };
        let generation = || self.core.revive.lock().unwrap().generation;
        let mut tried = generation();
        let mut opened = open().await;
        // An engine that died meanwhile comes back with the container (its
        // seed lists it): a later call revives it, and the open adopts it,
        // also when another caller revived it first. A revive held back
        // (respawn spacing, an addon that cannot be re-registered) fails the
        // call at once, so the retries wait between calls, within a bound.
        let deadline = tokio::time::Instant::now() + 3 * PLUGIN_HOST_RESPAWN_SPACING;
        while opened.is_err() && tokio::time::Instant::now() < deadline {
            if generation() == tried {
                if !(*self.core.host_gone.borrow() && self.core.can_revive()) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
            tried = generation();
            opened = open().await;
        }
        if let Err(e) = opened {
            self.core
                .environments
                .lock()
                .unwrap()
                .retain(|(s, _)| s["id"] != id);
            // A live engine that opened it anyway (from a seed) drops it.
            if !*self.core.host_gone.borrow() {
                let _ = self.core.call(id, "closeContainer", &["environment"]).await;
            }
            return Err(e);
        }
        handle.ensure_hook_plan().await;
        Ok(handle)
    }

    /// How many plugins the host's config lists before any environment
    /// drops one (`applyToEnvironment`): a host whose client runs none may
    /// still run some elsewhere. Defaults to 1 on RPC failure.
    pub async fn top_level_plugin_count(&self) -> usize {
        self.call_ok("getTopLevelPluginCount")
            .await
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
    }

    /// Drops this handle's worker container from the host (its plugins answer
    /// no more hooks); the host's own and its environment containers are never closed.
    pub async fn close_container(&self) -> Result<(), String> {
        // A stale handle's container died with its engine, and its id may
        // be a live bundle's now.
        if self.generation.is_none() || self.is_stale() {
            return Ok(());
        }
        self.core.rpc_routes.lock().unwrap().remove(&self.container);
        self.core
            .boot
            .bundles
            .lock()
            .unwrap()
            .remove(&self.container);
        self.call_unit("closeContainer", &[]).await
    }

    /// The bundle this container's hooks run inside from now on (a bundle's
    /// buildStart): its `this.getModuleInfo` and `this.getModuleIds` read
    /// that bundle's graph.
    pub fn set_bundle_graph(&self, graph: Arc<dyn BundleGraph>) {
        if self.is_stale() {
            return;
        }
        self.core
            .boot
            .bundles
            .lock()
            .unwrap()
            .insert(self.container, graph);
    }

    /// Fail-open gates until the plan is fetched again (a respawned engine).
    fn reset_hook_plan(&self) {
        use std::sync::atomic::Ordering;
        let mut slot = self.hook_plan.write().unwrap();
        self.hook_plan_epoch.fetch_add(1, Ordering::AcqRel);
        *slot = BuildHookPlan::fail_open();
        self.watch_change.store(true, Ordering::Release);
        self.hot_update.store(true, Ordering::Release);
        drop(slot);
        self.hook_plan_fetched.store(false, Ordering::Release);
        self.hook_plan_prime_started.store(false, Ordering::Release);
    }

    /// Whether the host finished its top-level init (the serve-info push, or
    /// any RPC reply, whichever came first).
    pub fn is_initialized(&self) -> bool {
        self.core.is_initialized()
    }

    /// Live updates of the initialized flag, for waiters keyed on the init
    /// transition itself (the watcher's SSR catch-up replay).
    pub fn initialized_updates(&self) -> tokio::sync::watch::Receiver<bool> {
        self.core.initialized_updates()
    }

    /// When an uninitialized boot turns loud, measured from spawn.
    pub fn init_warn_at(&self) -> tokio::time::Instant {
        self.core.init_warn_at()
    }

    /// Live updates of the host-gone flag.
    pub fn host_gone_updates(&self) -> tokio::sync::watch::Receiver<bool> {
        self.core.host_gone_updates()
    }

    /// Init-failure evidence updates.
    pub fn init_failure_updates(&self) -> tokio::sync::watch::Receiver<bool> {
        self.core.init_failure_updates()
    }

    /// Resync-executed counter updates.
    pub fn resync_done_updates(&self) -> tokio::sync::watch::Receiver<u64> {
        self.core.resync_done_updates()
    }

    /// Resolves when the host is gone.
    pub(crate) async fn host_gone_wait(&self) {
        self.core.host_gone_wait().await
    }

    /// Whether a dead host may still come back.
    pub fn can_revive(&self) -> bool {
        self.core.can_revive()
    }

    /// Retire the engine now, with every container on it.
    pub fn shutdown(&self) {
        self.core.shutdown()
    }

    pub fn set_server_events_sender(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    ) {
        self.core.set_server_events_sender(tx)
    }

    /// The dev server's defines for `moduleInfo` compiles (`oj build` leaves it unset).
    pub fn set_import_meta_env(&self, env: Arc<oj_compiler::ImportMetaEnv>) {
        self.core.set_import_meta_env(env)
    }

    pub fn set_ws_sender(&self, tx: tokio::sync::broadcast::Sender<String>) {
        self.core.set_ws_sender(tx)
    }

    /// Answers the host's async reverse RPC `id`.
    pub fn rpc_reply(&self, id: u64, result: Result<serde_json::Value, String>) {
        self.core.rpc_reply(id, result)
    }
}

impl HostCore {
    /// A fresh environment container's spec and handle (not yet recorded).
    fn declare_environment(
        self: &std::sync::Arc<Self>,
        name: &str,
        leave_out: bool,
        bundle: bool,
    ) -> (serde_json::Value, std::sync::Arc<PluginHost>) {
        let id = self
            .next_environment
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        let spec =
            serde_json::json!({ "id": id, "name": name, "leaveOut": leave_out, "bundle": bundle });
        let handle = std::sync::Arc::new(PluginHost::over(std::sync::Arc::clone(self), id, None));
        (spec, handle)
    }

    /// The host's own container, while a handle on it lives.
    fn main_container(&self) -> Option<std::sync::Arc<PluginHost>> {
        self.main.get().and_then(std::sync::Weak::upgrade)
    }

    /// The live handles on the containers every generation re-creates.
    fn durable_containers(&self) -> Vec<std::sync::Arc<PluginHost>> {
        let mut out: Vec<_> = self.main_container().into_iter().collect();
        out.extend(
            self.environments
                .lock()
                .unwrap()
                .iter()
                .filter_map(|(_, h)| h.upgrade()),
        );
        out
    }

    /// Boot one engine GENERATION onto `host` (initial spawn and every revive). A
    /// superseded `generation` abandons the just-spawned engine: racing ignites must never leave a live loser.
    fn ignite(host: &std::sync::Arc<HostCore>, generation: u64) -> Result<(), String> {
        Self::ignite_with(host, generation, None)
    }

    fn ignite_with(
        host: &std::sync::Arc<HostCore>,
        generation: u64,
        warm: Option<(
            std::sync::Arc<oj_js::JsEngine>,
            tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        )>,
    ) -> Result<(), String> {
        host.write_host_module()?;
        // Pushes arrive as values on `post_rx`, so nothing a plugin prints can
        // splice into the protocol.
        let (engine, post_rx) = match warm {
            Some(warm) => warm,
            None => {
                let (post_tx, post_rx) = tokio::sync::mpsc::unbounded_channel();
                (std::sync::Arc::new(host.spawn_engine(post_tx)?), post_rx)
            }
        };
        {
            let revive = host.revive.lock().unwrap();
            // A shutdown racing this boot must not gain a live engine it can no
            // longer take, and a newer revive must not gain a live loser.
            if revive.generation != generation
                || host.shut_down.load(std::sync::atomic::Ordering::SeqCst)
            {
                drop(revive);
                engine.abandon();
                return Err("superseded by a newer respawn or a shutdown".into());
            }
            *host.engine.lock().unwrap() = Some(std::sync::Arc::clone(&engine));
        }
        Self::spawn_boot_task(host, engine, generation);
        Self::spawn_push_dispatcher(host, post_rx, generation);
        Self::spawn_stall_monitor(host);
        Ok(())
    }

    /// Written atomically (tmp + rename): hosts spawn concurrently, and a
    /// truncating write could hand a sibling engine a half-written module.
    fn write_host_module(&self) -> Result<(), String> {
        let script = PathBuf::from(&self.host_module);
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let _ = ensure_asset(
            oj_cache::cache_root(&self.boot.root).as_path(),
            "discovered-deps.mjs",
            DISCOVERED_DEPS_JS,
        );
        ensure_asset(
            oj_cache::cache_root(&self.boot.root).as_path(),
            "vite-config-node.mjs",
            VITE_CONFIG_NODE_JS,
        )
        .map_err(|e| e.to_string())?;
        ensure_asset(
            oj_cache::cache_root(&self.boot.root).as_path(),
            "native-plugins.mjs",
            NATIVE_PLUGINS_JS,
        )
        .map_err(|e| e.to_string())?;
        ensure_asset(
            oj_cache::cache_root(&self.boot.root).as_path(),
            "vite-html.mjs",
            VITE_HTML_JS,
        )
        .map_err(|e| e.to_string())?;
        super::ensure_glob_assets(oj_cache::cache_root(&self.boot.root).as_path())
            .map_err(|e| e.to_string())?;
        if std::fs::read(&script).ok().as_deref() != Some(PLUGIN_HOST_JS.as_bytes()) {
            let tmp = script.with_extension(format!("tmp-{}.mjs", std::process::id()));
            std::fs::write(&tmp, PLUGIN_HOST_JS).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &script).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// A fresh engine whose pushes arrive on `post_tx` and whose reverse ctx-RPC
    /// is answered by [`ctx_rpc`].
    fn spawn_engine(
        &self,
        post_tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    ) -> Result<oj_js::JsEngine, String> {
        new_host_engine(
            &self.boot.root,
            self.boot.memory_limit_bytes,
            self.boot.registry.clone(),
            Arc::clone(&self.boot.import_meta_env),
            Arc::clone(&self.boot.bundles),
            post_tx,
        )
    }

    /// BOOT task: seed the host's identity, then trigger top-level init via a trivial
    /// export. No deadline (the Rust-side watches own boot patience); a top-level throw declares the host gone.
    fn spawn_boot_task(
        host: &std::sync::Arc<HostCore>,
        engine: std::sync::Arc<oj_js::JsEngine>,
        generation: u64,
    ) {
        let host = std::sync::Arc::clone(host);
        let mut seed = host.boot.boot_seed.clone();
        seed["containers"] = serde_json::Value::Array(
            host.environments
                .lock()
                .unwrap()
                .iter()
                .map(|(spec, _)| spec.clone())
                .collect(),
        );
        let host_module = host.host_module.clone();
        tokio::spawn(async move {
            let seeded = match ensure_boot_module(&host.boot.root) {
                Ok(boot_module) => engine
                    .call(boot_module, "seed", vec![seed], None)
                    .await
                    .map_err(|e| e.to_string()),
                Err(e) => Err(e),
            };
            if let Err(e) = seeded {
                host.declare_gone(&format!("plugin host boot prelude failed: {e}"), generation);
                return;
            }
            match engine
                .call(host_module, "ojHostReady", Vec::new(), None)
                .await
            {
                // The `{ ojInit }` push already flipped `initialized`; the reply is
                // the error path's carrier. Deliver a bind-time announce this host missed.
                Ok(_) => {
                    let listener = DEV_LISTENER.lock().unwrap().clone();
                    if let Some((port, interface)) = listener {
                        host.announce_dev_listener(port, &interface);
                    }
                }
                Err(oj_js::EngineError::Closed) => {}
                Err(e) => {
                    host.declare_gone(
                        &format!("plugin host failed to initialize: {e}"),
                        generation,
                    );
                }
            }
        });
    }

    /// The PUSH DISPATCHER: control pushes arrive whole on the engine channel;
    /// hook replies come back on their own call futures.
    fn spawn_push_dispatcher(
        host: &std::sync::Arc<HostCore>,
        mut post_rx: tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
        generation: u64,
    ) {
        let host = std::sync::Arc::clone(host);
        tokio::spawn(async move {
            while let Some(msg) = post_rx.recv().await {
                // Drain and drop pushes from a superseded or declared-dead generation:
                // a stale `ojInit` would open the call gate before the NEW engine evaluated its module.
                if *host.host_gone.borrow() || host.revive.lock().unwrap().generation != generation
                {
                    continue;
                }
                host.dispatch_push(&msg);
            }
            // Push channel closed = engine thread exited: fail future calls fast.
            // Generation-guarded: an abandoned engine's late exit must not kill the fresh generation.
            let revive = host.revive.lock().unwrap();
            if revive.generation == generation {
                drop(revive);
                let _ = host.host_gone.send_replace(true);
            }
        });
    }

    fn dispatch_push(self: &std::sync::Arc<Self>, msg: &serde_json::Value) {
        if let Some(id) = msg.get("rpc").and_then(|v| v.as_u64()) {
            let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
            let args = match msg.get("args") {
                Some(serde_json::Value::Array(args)) => args.clone(),
                _ => Vec::new(),
            };
            let rpc = HostRpc {
                id,
                method: method.to_string(),
                args,
            };
            // The container whose bundle asks answers it. Without one to
            // answer (`oj build`), fail the call: an unanswered one would
            // leave its hook awaiting forever.
            let container = msg
                .get("container")
                .and_then(|c| c.as_u64())
                .and_then(|c| u32::try_from(c).ok())
                .unwrap_or(MAIN_CONTAINER);
            let route = self.rpc_routes.lock().unwrap().get(&container).cloned();
            match route {
                Some(tx) => {
                    if let Err(e) = tx.send(rpc) {
                        self.rpc_reply(id, Err(format!("no handler for {}", e.0.method)));
                    }
                }
                None => self.rpc_reply(id, Err(format!("no handler for {method}"))),
            }
        } else if let Some(info) = msg.get("ojServeInfo") {
            self.serve_info_push
                .send_replace(Some(ServeInfo::from_json(info)));
            self.mark_initialized();
        } else if msg.get("ojInit").is_some() {
            // Unconditional init-complete, sent in BOTH modes: build mode
            // has no ojServeInfo push, and the gate must not wait for the first reply.
            self.mark_initialized();
        } else if msg.get("ojResyncDone").is_some() {
            // An enqueued resync actually ran; see resync_done.
            self.resync_done.send_modify(|c| *c += 1);
        } else if msg.get("ojInitProgress").is_some() {
            // Real init milestone: wedge evidence is stale, the stall monitor re-arms.
            self.init_progress_seen.send_modify(|c| *c += 1);
            let _ = self.init_failed.send_replace(false);
        } else if let Some(ev) = msg.get("ojServer") {
            if let Some(tx) = self.server_events.get() {
                let _ = tx.send(ev.clone());
            }
        } else if let Some(ws) = msg.get("ojWs") {
            if let Some(tx) = self.ws_out.get() {
                let payload = ws_push_payload(ws);
                if !payload.is_empty() {
                    let _ = tx.send(payload);
                }
            }
        }
    }

    /// Init completed (or a reply proved it): open the gate, clear the evidence.
    fn mark_initialized(&self) {
        let _ = self.initialized.send_replace(true);
        let _ = self.init_failed.send_replace(false);
    }

    /// Init STALL MONITOR, wedge evidence independent of any caller's window: a
    /// full RPC-scale window with NO milestone flips `init_failed`, progress clears it. Calls never consult it.
    fn spawn_stall_monitor(host: &std::sync::Arc<HostCore>) {
        let host = std::sync::Arc::clone(host);
        let stall_wait = host.boot.stall_wait;
        tokio::spawn(async move {
            let mut init_rx = host.initialized.subscribe();
            let mut gone_rx = host.host_gone.subscribe();
            let mut prog_rx = host.init_progress_seen.subscribe();
            loop {
                if *init_rx.borrow_and_update() || *gone_rx.borrow_and_update() {
                    return;
                }
                let _ = prog_rx.borrow_and_update();
                let deadline = tokio::time::Instant::now() + stall_wait;
                let mut stalled = false;
                tokio::select! {
                    biased;
                    changed = init_rx.changed() => { if changed.is_err() { return; } }
                    changed = gone_rx.changed() => { if changed.is_err() { return; } }
                    changed = prog_rx.changed() => { if changed.is_err() { return; } }
                    _ = tokio::time::sleep_until(deadline) => { stalled = true; }
                }
                if stalled {
                    let _ = host.init_failed.send_replace(true);
                    // Window spent: re-arm only on new progress; the reader clears the evidence.
                    loop {
                        tokio::select! {
                            biased;
                            changed = init_rx.changed() => {
                                if changed.is_err() || *init_rx.borrow() { return; }
                            }
                            changed = gone_rx.changed() => {
                                if changed.is_err() || *gone_rx.borrow() { return; }
                            }
                            changed = prog_rx.changed() => {
                                if changed.is_err() { return; }
                                break;
                            }
                        }
                    }
                }
            }
        });
    }

    /// Whether a dead host may still come back: budget left, and never after an
    /// on-purpose `shutdown`.
    pub fn can_revive(&self) -> bool {
        !self.shut_down.load(std::sync::atomic::Ordering::SeqCst)
            && self.revive.lock().unwrap().attempts < PLUGIN_HOST_RESPAWN_LIMIT
    }

    /// Revive a dead host with a fresh generation, bounded by lifetime budget and
    /// spacing (see `ReviveState`). Returns whether the host is now (or already was) live.
    fn try_revive(&self) -> bool {
        if self.shut_down.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        let Some(host) = self.self_ref.get().and_then(std::sync::Weak::upgrade) else {
            return false;
        };
        let mut revive = self.revive.lock().unwrap();
        if !*self.host_gone.borrow() {
            // A concurrent caller already revived it (or the report was stale).
            return true;
        }
        if revive.attempts >= PLUGIN_HOST_RESPAWN_LIMIT {
            return false;
        }
        if let Some(last) = revive.last {
            if last.elapsed() < PLUGIN_HOST_RESPAWN_SPACING {
                // Too soon: fail fast instead of stacking engines against a recurring wedge.
                return false;
            }
        }
        // Re-registering an addon whose every runtime is gone can crash the process
        // (napi-rs pre-3.10): refuse only for addons this host's own teardown orphaned; no attempt is consumed.
        let orphaned = oj_js::addons_pending_unsafe_reregistration();
        if let Some(addon) = orphaned
            .iter()
            .find(|p| !revive.pending_before.contains(*p))
        {
            revive.last = Some(std::time::Instant::now());
            eprintln!(
                "oj: not respawning the plugin host: native addon {} was torn down with it and re-registering can crash (napi-rs before 3.10); restart the dev server to recover",
                addon.display()
            );
            oj_diag::emit(
                oj_diag::Event::new(
                    oj_diag::Kind::HostExhausted,
                    format!(
                        "not respawning the plugin host: native addon {} was torn down with it",
                        addon.display()
                    ),
                )
                .source(oj_diag::Source::Host),
            );
            return false;
        }
        revive.attempts += 1;
        revive.last = Some(std::time::Instant::now());
        revive.generation += 1;
        revive.reported = false;
        eprintln!(
            "oj: respawning the plugin host (attempt {} of {PLUGIN_HOST_RESPAWN_LIMIT})",
            revive.attempts
        );
        oj_diag::emit(
            oj_diag::Event::new(
                oj_diag::Kind::HostRespawn,
                format!(
                    "respawning the plugin host (attempt {} of {PLUGIN_HOST_RESPAWN_LIMIT})",
                    revive.attempts
                ),
            )
            .source(oj_diag::Source::Host)
            .warn(),
        );
        self.reset_generation_state();
        let generation = revive.generation;
        drop(revive);
        match Self::ignite(&host, generation) {
            Ok(()) => {
                // Lift the death flag last: a caller sees a dead host or a fully re-armed one.
                let _ = self.host_gone.send_replace(false);
                if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                    for handle in self.durable_containers() {
                        runtime.spawn(async move { handle.ensure_hook_plan().await });
                    }
                }
                true
            }
            Err(e) => {
                eprintln!("oj: plugin host respawn failed: {e}");
                oj_diag::emit(
                    oj_diag::Event::new(
                        oj_diag::Kind::HostRespawn,
                        format!("plugin host respawn failed: {e}"),
                    )
                    .source(oj_diag::Source::Host),
                );
                false
            }
        }
    }

    /// Reset per-generation state BEFORE the new engine can push: init pending
    /// again, evidence cleared, stale serve info dropped. The fresh engine
    /// re-evaluates the plugins file: hook plan fail-open now, the refetch restores precise gating.
    fn reset_generation_state(&self) {
        let _ = self.initialized.send_replace(false);
        let _ = self.init_failed.send_replace(false);
        self.serve_info_push.send_replace(None);
        // Worker containers died with the old engine, their routes with them;
        // the host's own and its environment containers come back and refetch.
        self.rpc_routes.lock().unwrap().retain(|&container, _| {
            container == MAIN_CONTAINER || container >= ENVIRONMENT_CONTAINERS
        });
        for handle in self.durable_containers() {
            handle.reset_hook_plan();
        }
        *self.spawned.lock().unwrap() = tokio::time::Instant::now();
    }

    /// Fire-and-forget `serverListening` delivery (see [`dev_listener_bound`]):
    /// a failure only means this host's plugins never see "listening".
    fn announce_dev_listener(self: &std::sync::Arc<Self>, port: u16, interface: &str) {
        let host = std::sync::Arc::clone(self);
        let interface = interface.to_string();
        tokio::spawn(async move {
            let _ = host
                .call(
                    MAIN_CONTAINER,
                    "serverListening",
                    &[&port.to_string(), &interface],
                )
                .await;
        });
    }

    /// One hook call into `container`.
    async fn call(
        &self,
        container: u32,
        hook: &str,
        args: &[&str],
    ) -> Result<Option<String>, String> {
        if *self.host_gone.borrow() && !self.try_revive() {
            return Err("plugin host exited".into());
        }
        self.wait_for_init().await?;
        self.run_hook(container, hook, args).await
    }

    /// Init gate, BEFORE anything reaches the engine: the host answers hooks
    /// only after top-level init, and a job submitted to a wedged mid-init
    /// isolate would queue behind the wedge and be blamed on the hook.
    /// Deliberately UNBOUNDED, like a Vite request (which can only exist
    /// after `initServer` resolved): elapsed time is a slow boot, not a
    /// wedge, so it only logs; the stall monitor owns wedge evidence, and
    /// only host death (host_gone) fails the wait.
    async fn wait_for_init(&self) -> Result<(), String> {
        let mut init_rx = self.initialized.subscribe();
        if *init_rx.borrow_and_update() {
            return Ok(());
        }
        let mut host_gone_rx = self.host_gone.subscribe();
        // A death flipped before this subscribe is already "seen" (changed()
        // never fires for it): consult the value once after subscribing.
        if *host_gone_rx.borrow_and_update() {
            return Err("plugin host exited".into());
        }
        let mut progress = tokio::time::interval_at(
            tokio::time::Instant::now() + std::time::Duration::from_secs(30),
            std::time::Duration::from_secs(30),
        );
        loop {
            tokio::select! {
                biased;
                changed = init_rx.changed() => {
                    if changed.is_err() || *init_rx.borrow() {
                        return Ok(());
                    }
                }
                changed = host_gone_rx.changed() => {
                    if changed.is_err() || *host_gone_rx.borrow() {
                        return Err("plugin host exited".into());
                    }
                }
                _ = progress.tick() => self.log_init_progress(),
            }
        }
    }

    /// One "still initializing" line per interval across concurrent waiters.
    fn log_init_progress(&self) {
        let elapsed = self.spawned.lock().unwrap().elapsed().as_secs();
        let mut last = self
            .init_progress
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if last.elapsed().as_secs() >= 29 {
            *last = std::time::Instant::now();
            eprintln!("oj: plugin host still initializing ({elapsed}s)…");
        }
    }

    /// Runs one hook with NO deadline of its own: Vite runs plugin work
    /// unbounded, so elapsed time alone never fails a call. Liveness is
    /// PROBED instead: each probe window a no-op export is called, and a
    /// parked event loop answers it instantly however long the hook has been
    /// awaiting (that just logs progress). A probe met with total silence
    /// for two windows means the isolate thread is wedged — blocked in
    /// native code, or spinning in synchronous JS — so declare the host
    /// gone; the next call revives a fresh one.
    async fn run_hook(
        &self,
        container: u32,
        hook: &str,
        args: &[&str],
    ) -> Result<Option<String>, String> {
        // Generation read before the engine: a racing revive makes the pair stale
        // (probe verdict ignored), never a stale generation blaming a fresh engine.
        let generation = self.revive.lock().unwrap().generation;
        let engine = self
            .engine
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "plugin host exited".to_string())?;
        let call = engine.call(
            self.host_module.clone(),
            "ojRun",
            vec![
                serde_json::Value::String(hook.to_string()),
                serde_json::Value::Array(
                    args.iter()
                        .map(|a| serde_json::Value::String((*a).to_string()))
                        .collect(),
                ),
                serde_json::Value::from(container),
            ],
            None,
        );
        tokio::pin!(call);
        let started = tokio::time::Instant::now();
        let mut probe = tokio::time::interval_at(started + self.probe_wait, self.probe_wait);
        probe.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                r = &mut call => return self.finish_call(hook, generation, r),
                _ = self.host_gone_wait() => return Err("plugin host exited".into()),
                _ = probe.tick() => {
                    // The probe is deliberately UNGUARDED (no engine deadline):
                    // an armed watchdog terminates whatever executes on the
                    // isolate, which could be someone else's healthy work.
                    let ping = engine.call(self.host_module.clone(), "ojPing", Vec::new(), None);
                    match tokio::time::timeout(2 * self.probe_wait, ping).await {
                        Ok(Err(oj_js::EngineError::Closed)) => {
                            return Err("plugin host exited".into());
                        }
                        Ok(_) => {
                            eprintln!(
                                "oj: {hook} has been running for {}s (the plugin host is responsive)…",
                                started.elapsed().as_secs()
                            );
                        }
                        Err(_) => {
                            let msg = format!(
                                "plugin host unresponsive for {}s running {hook} (the engine stopped scheduling)",
                                (2 * self.probe_wait).as_secs()
                            );
                            self.declare_gone(&msg, generation);
                            return Err(msg);
                        }
                    }
                }
            }
        }
    }

    /// Maps one engine reply to the hook result, latching init or death as it proves.
    fn finish_call(
        &self,
        hook: &str,
        generation: u64,
        result: Result<serde_json::Value, oj_js::EngineError>,
    ) -> Result<Option<String>, String> {
        match result {
            Ok(value) => {
                // Any reply proves top-level init completed. Generation-guarded: a
                // reply from a replaced engine must not open the gate for the new one.
                if self.revive.lock().unwrap().generation == generation {
                    self.mark_initialized();
                }
                Ok(match value {
                    serde_json::Value::Null => None,
                    serde_json::Value::String(s) => Some(s),
                    other => Some(other.to_string()),
                })
            }
            // Defensive: hook calls carry no deadline, so this arm is only
            // reachable through an engine-internal termination.
            Err(oj_js::EngineError::Deadline) => Err(format!(
                "plugin host abandoned {hook} (execution terminated)"
            )),
            Err(oj_js::EngineError::Closed) => Err("plugin host exited".into()),
            Err(oj_js::EngineError::MemoryLimit) => {
                // A near-limit heap is a property of the isolate, not this call:
                // declare it gone, the next call revives a fresh one.
                let msg = format!(
                    "plugin host exceeded its memory limit ({}MB; raise OJ_PLUGIN_MEMORY_MB) running {hook}",
                    self.boot.memory_limit_bytes / (1024 * 1024)
                );
                self.declare_gone(&msg, generation);
                Err(msg)
            }
            Err(oj_js::EngineError::Boot(e)) => Err(e),
            Err(oj_js::EngineError::Js(e)) => {
                // A throwing hook still proves the host is up and serving.
                self.mark_initialized();
                Err(e)
            }
        }
    }

    /// Treat the host as dead NOW: abandon the engine and flip `host_gone`; the next
    /// call may revive it. A report about a since-replaced generation is stale and must not kill the replacement.
    pub(crate) fn declare_gone(&self, why: &str, generation: u64) {
        let mut revive = self.revive.lock().unwrap();
        if !first_death_report(&mut revive, generation) {
            return;
        }
        // Snapshot addons ALREADY orphaned before this death, so the revive gate
        // can tell the dead generation's from someone else's (see `pending_before`).
        revive.pending_before = oj_js::addons_pending_unsafe_reregistration()
            .into_iter()
            .collect();
        let rss = process_rss_mb()
            .map(|m| format!(" (process rss {m}MB)"))
            .unwrap_or_default();
        eprintln!("oj: {why}; treating the plugin host as gone{rss}");
        oj_diag::emit(
            oj_diag::Event::new(
                oj_diag::Kind::HostGone,
                format!("{why}; treating the plugin host as gone{rss}"),
            )
            .source(oj_diag::Source::Host),
        );
        if let Some(engine) = self.engine.lock().unwrap().take() {
            self.retire_engine(engine, &mut revive);
        }
        drop(revive);
        let _ = self.host_gone.send_replace(true);
    }

    /// KEEPER sequence: register live addons BEFORE the abandon (the open job
    /// channel keeps their env alive until then); the spacing stamp defers the first revive past the keeper.
    fn retire_engine(&self, engine: std::sync::Arc<oj_js::JsEngine>, revive: &mut ReviveState) {
        let addons = oj_js::addons_with_live_registrations();
        if addons.is_empty() {
            engine.abandon();
            return;
        }
        revive.last = Some(std::time::Instant::now());
        let root = self.boot.root.clone();
        let registry = self.boot.registry.clone();
        let guard = AbandonOnDrop(Some(engine));
        tokio::spawn(async move {
            let _guard = guard;
            if let Err(e) = keep_addons_alive(&root, &addons, registry).await {
                eprintln!(
                    "oj: native-addon keeper unavailable ({e}); a plugin host respawn that would re-register an orphaned addon will be refused"
                );
            }
        });
    }

    /// Whether the host finished its top-level init (the serve-info push, or
    /// any RPC reply, whichever came first).
    pub fn is_initialized(&self) -> bool {
        *self.initialized.borrow()
    }

    /// Live updates of the initialized flag, for waiters keyed on the init
    /// transition itself (the watcher's SSR catch-up replay).
    pub fn initialized_updates(&self) -> tokio::sync::watch::Receiver<bool> {
        self.initialized.subscribe()
    }

    /// When an uninitialized boot turns loud, measured from spawn: the boot
    /// warning and the Start prewarm hold anchor to THIS instant instead of
    /// starting fresh periods. Reporting only — calls never gate on it.
    pub fn init_warn_at(&self) -> tokio::time::Instant {
        *self.spawned.lock().unwrap() + INIT_WARN
    }

    /// Live updates of the host-gone flag, for waiters selecting on wedge
    /// evidence without holding the `Arc<PluginHost>`.
    pub fn host_gone_updates(&self) -> tokio::sync::watch::Receiver<bool> {
        self.host_gone.subscribe()
    }

    /// Init-failure evidence updates: true while a pre-init call burned its full
    /// window, false again on progress. For health waiters, never a per-call gate.
    pub fn init_failure_updates(&self) -> tokio::sync::watch::Receiver<bool> {
        self.init_failed.subscribe()
    }

    /// Resync-executed counter updates: snapshot FIRST, then wait for it to move
    /// past the baseline (the /__oj_invalidate ack only means "enqueued").
    pub fn resync_done_updates(&self) -> tokio::sync::watch::Receiver<u64> {
        self.resync_done.subscribe()
    }

    /// Resolves when the host is gone: a task holding an `Arc<PluginHost>` keeps
    /// every sender alive, so `changed().is_err()` can never observe the death.
    pub(crate) async fn host_gone_wait(&self) {
        let mut rx = self.host_gone.subscribe();
        while !*rx.borrow_and_update() {
            if rx.changed().await.is_err() {
                return;
            }
        }
    }

    /// Retire the engine now. Abandon, not drop: dropping joins the thread; the
    /// push dispatcher's channel-closed path then latches `host_gone`.
    pub(crate) fn shutdown(&self) {
        // Retired on purpose: never revived. The revive lock orders this against
        // an in-flight ignite, so a racing revive can never leave a live engine behind.
        self.shut_down
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _revive = self.revive.lock().unwrap();
        if let Some(engine) = self.engine.lock().unwrap().take() {
            engine.abandon();
        }
    }

    pub fn set_server_events_sender(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    ) {
        let _ = self.server_events.set(tx);
    }

    /// The dev server's defines for `moduleInfo` compiles (`oj build` leaves it unset).
    pub fn set_import_meta_env(&self, env: Arc<oj_compiler::ImportMetaEnv>) {
        let _ = self.boot.import_meta_env.set(env);
    }

    /// Answers the host's async reverse RPC `id`.
    pub fn rpc_reply(
        self: &std::sync::Arc<Self>,
        id: u64,
        result: Result<serde_json::Value, String>,
    ) {
        let reply = match result {
            Ok(result) => serde_json::json!({ "result": result }),
            Err(error) => serde_json::json!({ "error": error }),
        }
        .to_string();
        let host = std::sync::Arc::clone(self);
        tokio::spawn(async move {
            let _ = host
                .call(MAIN_CONTAINER, "rpcReply", &[&id.to_string(), &reply])
                .await;
        });
    }

    pub fn set_ws_sender(&self, tx: tokio::sync::broadcast::Sender<String>) {
        let _ = self.ws_out.set(tx);
    }
}

fn bool_arg(b: bool) -> &'static str {
    if b {
        "true"
    } else {
        "false"
    }
}

/// The `emittedChunks` of a transform or buildStart reply.
fn emitted_chunks(v: &serde_json::Value) -> Vec<ChunkEmit> {
    v.get("emittedChunks")
        .and_then(|c| c.as_array())
        .map(|a| a.iter().filter_map(ChunkEmit::from_value).collect())
        .unwrap_or_default()
}

/// What a plugin chain (transform or renderChunk) made of a module or chunk.
#[derive(Debug, Default)]
pub struct HostTransform {
    pub code: String,
    pub watch_files: Vec<String>,
    /// The maps the plugins returned, in chain order.
    pub maps: Vec<String>,
    /// The plugin that returned each of `maps`.
    pub map_plugins: Vec<String>,
    /// The first plugin that returned code without a map, which breaks the
    /// chain as in Rollup (`map: null` does not).
    pub map_broken: Option<String>,
    pub chunks: Vec<ChunkEmit>,
}

impl HostTransform {
    /// A reply that is not the host's JSON is the code itself; none leaves
    /// `code` as it was.
    fn parse(raw: Option<String>, code: &str) -> Self {
        let Some(raw) = raw else {
            return Self {
                code: code.to_string(),
                ..Self::default()
            };
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return Self {
                code: raw,
                ..Self::default()
            };
        };
        let str_array = |key: &str| {
            v.get(key)
                .and_then(|w| w.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        Self {
            code: v
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or(code)
                .to_string(),
            watch_files: str_array("watchFiles"),
            maps: str_array("maps"),
            map_plugins: str_array("mapPlugins"),
            map_broken: v
                .get("mapBroken")
                .and_then(|n| n.as_str())
                .map(str::to_string),
            chunks: emitted_chunks(&v),
        }
    }
}
