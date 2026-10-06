use super::*;
use std::sync::Arc;

pub struct PluginHost {
    /// Engine hosting plugin-host.mjs; an Option so `declare_gone`/`shutdown` can
    /// take + abandon it. A thread wedged in NATIVE code leaks with its isolate.
    engine: Mutex<Option<std::sync::Arc<oj_js::JsEngine>>>,
    /// The plugin-host.mjs path on disk, the module every hook call targets.
    host_module: String,
    /// Live hook plan: starts fail-open, overwritten only by a successful fetch,
    /// reverts to fail-open on engine respawn; a failed fetch is never cached.
    hook_plan: std::sync::RwLock<BuildHookPlan>,
    hook_plan_fetched: std::sync::atomic::AtomicBool,
    hook_plan_prime_started: std::sync::atomic::AtomicBool,
    /// Wired once at server boot (the reader clones per push, lock-free).
    ws_out: std::sync::OnceLock<tokio::sync::broadcast::Sender<String>>,
    /// `{ ojServer: { action, ... } }` pushes from the host: a plugin invalidating
    /// a module via server.moduleGraph, or server.restart(). Wired once.
    server_events: std::sync::OnceLock<tokio::sync::mpsc::UnboundedSender<serde_json::Value>>,
    /// Host's `{ ojServeInfo }` push: None until top-level init completes; late
    /// subscribers can still activate the middleware path.
    serve_info_push: tokio::sync::watch::Sender<Option<ServeInfo>>,
    /// Host finished top-level init: flipped by the serve-info push, `{ ojInit }`,
    /// or the first hook reply. Hook calls gate on this; see `call`.
    initialized: tokio::sync::watch::Sender<bool>,
    /// Host is gone (engine thread exited, init failed, or declared wedged): fail
    /// calls fast. A watch so waiters select on the death instead of polling.
    pub(crate) host_gone: tokio::sync::watch::Sender<bool>,
    /// When the CURRENT generation spawned (reset by a revive); the init deadline
    /// is measured from here so boot RPCs share one deadline.
    spawned: Mutex<tokio::time::Instant>,
    /// How long a call may wait for top-level init: the boot host takes the long
    /// deadline measured from spawn, a lazy host the per-call bound from each call's start.
    pub(crate) init_wait: std::time::Duration,
    /// Lazily spawned: init wait anchors to each call's own start, so every
    /// pre-init call gets a full window and an expired one never fails a later call (see `call`).
    lazy: bool,
    /// Wedge EVIDENCE, not a call gate: true when a full init window or stall
    /// window passed with no milestone, false again on progress. Calls never consult it; waiters do.
    init_failed: tokio::sync::watch::Sender<bool>,
    /// The env knob named when the init wait elapses (matches `init_wait`).
    init_knob: &'static str,
    /// Count of `{ ojResyncDone }` pushes (a resync EXECUTED; the ack only means
    /// enqueued). A counter so a completion landing before the wait starts is never missed.
    resync_done: tokio::sync::watch::Sender<u64>,
    /// Count of `{ ojInitProgress }` pushes (init milestones). The stall monitor
    /// measures its wedge window from the LAST milestone, so a slow but progressing boot is never wedged.
    init_progress_seen: tokio::sync::watch::Sender<u64>,
    /// Per-call RPC timeout, snapshotted at spawn; tests override it to avoid
    /// racing the env knob other tests read.
    rpc_wait: std::time::Duration,
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
    self_ref: std::sync::OnceLock<std::sync::Weak<PluginHost>>,
}

/// The dev listener's bound (port, interface): hosts booting after the bind
/// read it in `ignite`; hosts already up are announced via [`LIVE_HOSTS`].
pub(crate) static DEV_LISTENER: Mutex<Option<(u16, String)>> = Mutex::new(None);
/// Every spawned host, weakly: the bind-time announce must reach hosts booted
/// before the listener existed (the normal boot order).
pub(crate) static LIVE_HOSTS: Mutex<Vec<std::sync::Weak<PluginHost>>> = Mutex::new(Vec::new());

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

/// See [`PluginHost::boot`].
pub(crate) struct BootContext {
    boot_seed: String,
    root: PathBuf,
    stall_wait: std::time::Duration,
    /// Heap cap every generation is spawned with (see `plugin_host_memory_mb`).
    memory_limit_bytes: usize,
    /// Rides into every generation's engine (and the addon keeper), so the
    /// debug GC fan-out keeps reaching this host across respawns.
    registry: Option<oj_js::EngineRegistry>,
    /// The dev server's defines, for `moduleInfo` compiles; set once after spawn.
    import_meta_env: Arc<std::sync::OnceLock<Arc<oj_compiler::ImportMetaEnv>>>,
}

/// See [`PluginHost::revive`].
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

/// How long one plugin hook may run (default 20s, `OJ_PLUGIN_TIMEOUT` raises it);
/// Vite has no hook timeout at all.
pub fn plugin_rpc_timeout() -> std::time::Duration {
    plugin_rpc_timeout_from(oj_env::get().knobs.plugin_timeout.as_deref())
}

pub(crate) fn plugin_rpc_timeout_from(raw: Option<&str>) -> std::time::Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(20);
    std::time::Duration::from_secs(secs)
}

/// How long the host may take to finish top-level init before a waiting RPC
/// gives up (`OJ_PLUGIN_INIT_TIMEOUT`); gates `call` instead of racing the per-call timeout.
pub fn plugin_init_timeout() -> std::time::Duration {
    plugin_init_timeout_from(oj_env::get().knobs.plugin_init_timeout.as_deref())
}

pub(crate) fn plugin_init_timeout_from(raw: Option<&str>) -> std::time::Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(300);
    std::time::Duration::from_secs(secs)
}

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

/// A host dropped without an explicit shutdown abandons its engine: the last
/// `Arc<JsEngine>` drop must not JOIN a thread parked in a never-settling init.
impl Drop for PluginHost {
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

/// Init deadline a pre-init `call` waits out: boot hosts share `spawned +
/// init_wait`, lazy hosts anchor to the CALL's own start (init landing releases early).
pub(crate) fn call_init_deadline(
    lazy: bool,
    spawned: tokio::time::Instant,
    init_wait: std::time::Duration,
    now: tokio::time::Instant,
) -> tokio::time::Instant {
    if lazy {
        now + init_wait
    } else {
        spawned + init_wait
    }
}

/// Init-wait policy per spawn kind: boot host long deadline, lazy host short
/// per-call bound, each named after its env knob.
pub(crate) fn init_wait_policy(lazy: bool) -> (std::time::Duration, &'static str) {
    if lazy {
        (plugin_rpc_timeout(), "OJ_PLUGIN_TIMEOUT")
    } else {
        (plugin_init_timeout(), "OJ_PLUGIN_INIT_TIMEOUT")
    }
}

/// Per-spawn timeout overrides for tests, avoiding races on the env-var knobs
/// other tests read. Production spawns pass the default (env-derived).
#[derive(Default)]
pub(crate) struct SpawnTimeouts {
    pub(crate) init_wait: Option<std::time::Duration>,
    /// The stall monitor's no-progress window (defaults to the RPC timeout).
    pub(crate) stall: Option<std::time::Duration>,
    /// The per-call RPC timeout (`PluginHost::rpc_wait`).
    pub(crate) rpc: Option<std::time::Duration>,
    /// The engine heap cap (defaults to `plugin_host_memory_mb`).
    pub(crate) memory: Option<usize>,
}

impl PluginHost {
    /// Spawn a boot-time host: calls wait out the full init deadline, because
    /// boot correctness depends on its snapshot RPCs.
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
            false,
            SpawnTimeouts::default(),
            registry,
        )
        .await
    }

    /// Spawn a lazily created host (e.g. the SSR environment host): calls bound
    /// their init wait by the per-call timeout, so a wedged init cannot freeze the watcher thread.
    pub async fn spawn_lazy(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        registry: Option<oj_js::EngineRegistry>,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_with_policy(
            root,
            plugins_file,
            config_json,
            true,
            SpawnTimeouts::default(),
            registry,
        )
        .await
    }

    /// Test-only lazy spawn with an explicit init wait.
    #[cfg(test)]
    pub(crate) async fn spawn_lazy_with_wait(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        init_wait: std::time::Duration,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_with_policy(
            root,
            plugins_file,
            config_json,
            true,
            SpawnTimeouts {
                init_wait: Some(init_wait),
                ..Default::default()
            },
            None,
        )
        .await
    }

    /// Test-only spawn with every timeout explicit (see `SpawnTimeouts`).
    #[cfg(test)]
    pub(crate) async fn spawn_with_timeouts(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        lazy: bool,
        timeouts: SpawnTimeouts,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_with_policy(root, plugins_file, config_json, lazy, timeouts, None).await
    }

    async fn spawn_with_policy(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        lazy: bool,
        timeouts: SpawnTimeouts,
        registry: Option<oj_js::EngineRegistry>,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        let script = oj_cache::cache_root(root).join("plugin-host.mjs");

        let (mut init_wait, init_knob) = init_wait_policy(lazy);
        if let Some(w) = timeouts.init_wait {
            init_wait = w;
        }
        let rpc_wait = timeouts.rpc.unwrap_or_else(plugin_rpc_timeout);
        let stall_wait = timeouts.stall.unwrap_or(rpc_wait);
        let boot_seed = serde_json::json!({
            "pluginsPath": plugins_file,
            "initialJson": config_json,
            "cacheRoot": oj_cache::cache_root(root),
        });
        let host = std::sync::Arc::new(PluginHost {
            engine: Mutex::new(None),
            host_module: script.to_string_lossy().into_owned(),
            hook_plan: std::sync::RwLock::new(BuildHookPlan::fail_open()),
            hook_plan_fetched: std::sync::atomic::AtomicBool::new(false),
            hook_plan_prime_started: std::sync::atomic::AtomicBool::new(false),
            ws_out: std::sync::OnceLock::new(),
            server_events: std::sync::OnceLock::new(),
            serve_info_push: tokio::sync::watch::channel(None).0,
            initialized: tokio::sync::watch::channel(false).0,
            host_gone: tokio::sync::watch::channel(false).0,
            spawned: Mutex::new(tokio::time::Instant::now()),
            init_wait,
            lazy,
            init_failed: tokio::sync::watch::channel(false).0,
            init_knob,
            resync_done: tokio::sync::watch::channel(0).0,
            init_progress_seen: tokio::sync::watch::channel(0).0,
            rpc_wait,
            init_progress: Mutex::new(std::time::Instant::now()),
            boot: BootContext {
                boot_seed: boot_seed.to_string(),
                root: root.to_path_buf(),
                stall_wait,
                memory_limit_bytes: timeouts
                    .memory
                    .unwrap_or_else(|| plugin_host_memory_mb() * 1024 * 1024),
                registry,
                import_meta_env: Arc::default(),
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
        });
        let _ = host.self_ref.set(std::sync::Arc::downgrade(&host));
        LIVE_HOSTS
            .lock()
            .unwrap()
            .push(std::sync::Arc::downgrade(&host));
        Self::ignite(&host, 0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(host)
    }

    /// Boot one engine GENERATION onto `host` (initial spawn and every revive). A
    /// superseded `generation` abandons the just-spawned engine: racing ignites must never leave a live loser.
    fn ignite(host: &std::sync::Arc<PluginHost>, generation: u64) -> Result<(), String> {
        host.write_host_module()?;
        // Pushes arrive as values on `post_rx`, so nothing a plugin prints can
        // splice into the protocol.
        let (post_tx, post_rx) = tokio::sync::mpsc::unbounded_channel();
        let engine = std::sync::Arc::new(host.spawn_engine(post_tx)?);
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
        let root = self.boot.root.clone();
        let resolver = std::sync::Arc::new(OjResolver::new(&root));
        let rpc_handler: oj_js::RpcHandler = {
            let root = root.clone();
            let env = Arc::clone(&self.boot.import_meta_env);
            Box::new(move |method, args| ctx_rpc(method, args, &resolver, &root, env.get()))
        };
        let mut engine_config = oj_js::EngineConfig::new(&root);
        engine_config.code_cache_dir = Some(crate::engine_code_cache_dir(&root));
        engine_config.memory_limit_bytes = Some(self.boot.memory_limit_bytes);
        engine_config.registry = self.boot.registry.clone();
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

    /// BOOT task: seed the host's identity, then trigger top-level init via a trivial
    /// export. No deadline (the Rust-side watches own boot patience); a top-level throw declares the host gone.
    fn spawn_boot_task(
        host: &std::sync::Arc<PluginHost>,
        engine: std::sync::Arc<oj_js::JsEngine>,
        generation: u64,
    ) {
        let host = std::sync::Arc::clone(host);
        let prelude = format!("globalThis.__ojPluginHost = {};", host.boot.boot_seed);
        let host_module = host.host_module.clone();
        tokio::spawn(async move {
            if let Err(e) = engine.eval(oj_js::EvalInput::Source(prelude), None).await {
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
        host: &std::sync::Arc<PluginHost>,
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

    fn dispatch_push(&self, msg: &serde_json::Value) {
        if let Some(info) = msg.get("ojServeInfo") {
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
    fn spawn_stall_monitor(host: &std::sync::Arc<PluginHost>) {
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
        self.reset_generation_state();
        let generation = revive.generation;
        drop(revive);
        match Self::ignite(&host, generation) {
            Ok(()) => {
                // Lift the death flag last: a caller sees a dead host or a fully re-armed one.
                let _ = self.host_gone.send_replace(false);
                if let Ok(handle) = tokio::runtime::Handle::try_current() {
                    let host = std::sync::Arc::clone(&host);
                    handle.spawn(async move { host.ensure_hook_plan().await });
                }
                true
            }
            Err(e) => {
                eprintln!("oj: plugin host respawn failed: {e}");
                false
            }
        }
    }

    /// Reset per-generation state BEFORE the new engine can push: init pending
    /// again, evidence cleared, stale serve info dropped. The fresh engine
    /// re-evaluates the plugins file: hook plan fail-open now, the refetch restores precise gating.
    fn reset_generation_state(&self) {
        use std::sync::atomic::Ordering;
        let _ = self.initialized.send_replace(false);
        let _ = self.init_failed.send_replace(false);
        self.serve_info_push.send_replace(None);
        *self.hook_plan.write().unwrap() = BuildHookPlan::fail_open();
        self.hook_plan_fetched.store(false, Ordering::Release);
        self.hook_plan_prime_started.store(false, Ordering::Release);
        *self.spawned.lock().unwrap() = tokio::time::Instant::now();
    }

    /// Fire-and-forget `serverListening` delivery (see [`dev_listener_bound`]):
    /// a failure only means this host's plugins never see "listening".
    fn announce_dev_listener(self: &std::sync::Arc<Self>, port: u16, interface: &str) {
        let host = std::sync::Arc::clone(self);
        let interface = interface.to_string();
        tokio::spawn(async move {
            let _ = host
                .call("serverListening", &[&port.to_string(), &interface])
                .await;
        });
    }

    async fn call(&self, hook: &str, args: &[&str]) -> Result<Option<String>, String> {
        if *self.host_gone.borrow() && !self.try_revive() {
            return Err("plugin host exited".into());
        }
        self.wait_for_init(hook).await?;
        self.run_hook(hook, args).await
    }

    /// Init gate, BEFORE anything reaches the engine: the host answers hooks
    /// only after top-level init, and a job submitted to a wedged mid-init
    /// isolate would queue behind the wedge and be blamed on the hook.
    /// Deliberately NO time-based fail-fast latch: an expired window is
    /// evidence of a slow boot, not a wedge, so every call gets its own full
    /// window; only host death (host_gone) fails fast.
    async fn wait_for_init(&self, hook: &str) -> Result<(), String> {
        let mut init_rx = self.initialized.subscribe();
        if *init_rx.borrow_and_update() {
            return Ok(());
        }
        let deadline = call_init_deadline(
            self.lazy,
            *self.spawned.lock().unwrap(),
            self.init_wait,
            tokio::time::Instant::now(),
        );
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
                // Deterministic when arms are simultaneously ready: an
                // init flip racing an elapsed deadline must win.
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
                _ = tokio::time::sleep_until(deadline) => {
                    // Full window elapsed with init pending: wedge EVIDENCE,
                    // never a gate for later calls.
                    let _ = self.init_failed.send_replace(true);
                    return Err(format!(
                        "plugin host still initializing after {}s running {hook} (raise {} for slower boots)",
                        self.init_wait.as_secs(),
                        self.init_knob,
                    ));
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

    /// The engine call carries the per-call deadline itself, and a deadline
    /// failure costs ONE call with the host answering everyone else. The BELT
    /// past it is a second full window with NO reply of any kind: the scheduler
    /// answers at the deadline while alive, so total silence means the isolate
    /// thread is blocked in NATIVE code; declare the host gone.
    async fn run_hook(&self, hook: &str, args: &[&str]) -> Result<Option<String>, String> {
        let deadline = tokio::time::Instant::now() + self.rpc_wait;
        // Generation read before the engine: a racing revive makes the pair stale
        // (belt ignored), never a stale generation blaming a fresh engine.
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
            ],
            Some(self.rpc_wait),
        );
        tokio::pin!(call);
        let result = tokio::select! {
            biased;
            r = &mut call => r,
            _ = self.host_gone_wait() => return Err("plugin host exited".into()),
            _ = tokio::time::sleep_until(deadline + self.rpc_wait) => {
                let msg = format!(
                    "plugin host unresponsive for {}s running {hook} (the engine stopped scheduling)",
                    2 * self.rpc_wait.as_secs()
                );
                self.declare_gone(&msg, generation);
                return Err(msg);
            }
        };
        self.finish_call(hook, generation, result)
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
            Err(oj_js::EngineError::Deadline) => Err(format!(
                "plugin host timed out after {}s running {hook} (raise OJ_PLUGIN_TIMEOUT for slow plugins)",
                self.rpc_wait.as_secs()
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

    /// Shared init deadline measured from spawn: callers gating separate work
    /// anchor to THIS deadline instead of starting a fresh full period.
    pub fn init_deadline_at(&self) -> tokio::time::Instant {
        *self.spawned.lock().unwrap() + self.init_wait
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

    pub async fn transform(
        &self,
        code: &str,
        id: &str,
        resolved: &str,
    ) -> Result<(String, Vec<String>, Vec<String>, Vec<ChunkEmit>), String> {
        let Some(raw) = self.call("transform", &[code, id, resolved]).await? else {
            return Ok((code.to_string(), Vec::new(), Vec::new(), Vec::new()));
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
            return Ok((raw, Vec::new(), Vec::new(), Vec::new()));
        };
        let out = v
            .get("code")
            .and_then(|c| c.as_str())
            .unwrap_or(code)
            .to_string();
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
        Ok((
            out,
            str_array("watchFiles"),
            str_array("maps"),
            emitted_chunks(&v),
        ))
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

    #[inline]
    pub async fn resolve_id(&self, source: &str, importer: &str) -> Result<Option<String>, String> {
        self.call("resolveId", &[source, importer]).await
    }

    #[inline]
    pub async fn load(&self, id: &str) -> Result<Option<String>, String> {
        self.call("load", &[id]).await
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

    /// Dispatch the app-Vite-side transform warm (fire-and-forget, like Vite's
    /// warmup.ts); replies once dispatched. Returns `{environments, started}`, None when nothing was warmable.
    pub async fn warm_environments(&self, urls: &[String]) -> Result<Option<String>, String> {
        let payload = serde_json::to_string(urls).map_err(|e| e.to_string())?;
        self.call("warmEnvironments", &[&payload]).await
    }

    async fn ensure_hook_plan(&self) {
        use std::sync::atomic::Ordering;
        if self.hook_plan_fetched.load(Ordering::Acquire) {
            return;
        }
        // Only a successful fetch is cached; a failure leaves the fail-open
        // default in place to be retried on the next call.
        if let Some(v) = self.call_json("getBuildHookPlan").await {
            let plan = BuildHookPlan {
                transform: HookFilterPlan::from_json(v.get("transform")),
                load: HookFilterPlan::from_json(v.get("load")),
                resolve_id: HookFilterPlan::from_json(v.get("resolveId")),
            };
            *self.hook_plan.write().unwrap() = plan;
            self.hook_plan_fetched.store(true, Ordering::Release);
        }
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
    ) -> Result<Option<String>, String> {
        self.call("renderChunk", &[code, chunk_json]).await
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
        if let Some(info) = *self.serve_info_push.borrow() {
            return info;
        }
        let rpc = self.call("getServeInfo", &[]).await;
        // The push may have landed while the RPC ran; it is the definitive value.
        if let Some(info) = *self.serve_info_push.borrow() {
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
        self.serve_info_push.subscribe()
    }

    /// Plugins still active after oj filters out natively reimplemented ones.
    /// Defaults to 1 on RPC failure so an uncertain host is never dropped.
    pub async fn plugin_count(&self) -> usize {
        self.call_ok("getPluginCount")
            .await
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
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

    /// Whether any active plugin has a `load` hook (Vite runs load before the fs
    /// read). Defaults to false on RPC failure (the fs read alone is always correct).
    pub async fn has_load(&self) -> bool {
        self.call_flag("getHasLoad", false).await
    }

    /// `filter.code` include patterns of every object-form transform hook, as
    /// regex sources; dependency transforms are gated on these.
    pub async fn dep_transform_filters(&self) -> Vec<String> {
        self.call_ok("getDepTransformFilters")
            .await
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// `filter.id` include patterns of every object-form `load` hook: deps cost
    /// no RPC unless a plugin asked for them.
    pub async fn dep_load_filters(&self) -> Vec<String> {
        self.call_ok("getDepLoadFilters")
            .await
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// `filter.id` include patterns of every object-form `resolveId` hook: a
    /// non-bare import is offered to plugin resolveId only when it matches one.
    pub async fn resolve_id_filters(&self) -> Vec<String> {
        self.call_ok("getResolveIdFilters")
            .await
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Which HMR hooks any active plugin defines: (watchChange, handleHotUpdate).
    /// Defaults to (true, true) on failure so an HMR RPC is never skipped by mistake.
    pub async fn hmr_hooks(&self) -> (bool, bool) {
        let Some(v) = self.call_json("getHmrHooks").await else {
            return (true, true);
        };
        let flag = |key: &str| v.get(key).and_then(|b| b.as_bool()).unwrap_or(true);
        (flag("watchChange"), flag("handleHotUpdate"))
    }

    /// Retire the engine now. Abandon, not drop: dropping joins the thread; the
    /// push dispatcher's channel-closed path then latches `host_gone`.
    pub fn shutdown(&self) {
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

    pub fn set_ws_sender(&self, tx: tokio::sync::broadcast::Sender<String>) {
        let _ = self.ws_out.set(tx);
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
