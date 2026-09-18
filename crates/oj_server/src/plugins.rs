// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use oj_resolver::OjResolver;

pub const PLUGIN_HOST_JS: &str = include_str!("assets/plugin-host.mjs");
pub const VITE_EXTRACT_JS: &str = include_str!("assets/vite-extract.mjs");

/// The host's `getServeInfo` report: how requests are served.
#[derive(Debug, Default, Clone, Copy)]
pub struct ServeInfo {
    /// Loopback port of the configureServer middleware stack, when any plugin
    /// registered a middleware.
    pub middleware_port: Option<u16>,
    /// Real runner-backed Vite DevEnvironments were built (the Environment-API
    /// path): documents are served by the plugin middleware.
    pub runner_environments: bool,
}

impl ServeInfo {
    /// The `{ middlewarePort, runnerEnvironments }` shape, shared by the host's
    /// `getServeInfo` RPC reply and its `{ ojServeInfo: ... }` stdout push.
    fn from_json(v: &serde_json::Value) -> ServeInfo {
        ServeInfo {
            middleware_port: v
                .get("middlewarePort")
                .and_then(|p| p.as_u64())
                .and_then(|p| u16::try_from(p).ok()),
            runner_environments: v
                .get("runnerEnvironments")
                .and_then(|b| b.as_bool())
                .unwrap_or(false),
        }
    }
}

#[derive(Debug)]
pub struct EmittedFile {
    pub file_name: String,
    pub source: String,
}

/// A chunk a plugin asked oj to emit via `this.emitFile({ type: "chunk" })`.
#[derive(Debug, Clone)]
pub struct ChunkEmit {
    pub ref_id: String,
    pub id: String,
    pub name: Option<String>,
    pub file_name: Option<String>,
}

impl ChunkEmit {
    fn from_value(m: &serde_json::Value) -> Option<Self> {
        Some(Self {
            ref_id: m.get("referenceId")?.as_str()?.to_string(),
            id: m.get("id")?.as_str()?.to_string(),
            name: m.get("name").and_then(|x| x.as_str()).map(str::to_string),
            file_name: m.get("fileName").and_then(|x| x.as_str()).map(str::to_string),
        })
    }
}

#[inline]
pub fn plugins_file(root: &Path) -> Option<std::path::PathBuf> {
    ["oj.plugins.mjs", "oj.plugins.js"]
        .into_iter()
        .map(|f| root.join(f))
        .find(|p| p.is_file())
}

pub enum PluginSource {
    OjPlugins(std::path::PathBuf),
    ViteConfig(std::path::PathBuf),
}

static VITE_CONFIG_OVERRIDE: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();

pub fn set_vite_config_override(path: std::path::PathBuf) {
    let _ = VITE_CONFIG_OVERRIDE.set(path);
}

#[inline]
pub fn vite_config_file(root: &Path) -> Option<std::path::PathBuf> {
    if let Some(p) = VITE_CONFIG_OVERRIDE.get() {
        return p.is_file().then(|| p.clone());
    }
    // Vite's DEFAULT_CONFIG_FILES order (constants.ts): the first that exists
    // wins, so a root with several config files picks the same one Vite does.
    [
        "vite.config.js",
        "vite.config.mjs",
        "vite.config.ts",
        "vite.config.cjs",
        "vite.config.mts",
        "vite.config.cts",
    ]
    .into_iter()
    .map(|f| root.join(f))
    .find(|p| p.is_file())
}

#[inline]
pub fn plugin_source(root: &Path) -> Option<PluginSource> {
    if VITE_CONFIG_OVERRIDE.get().is_some() {
        return vite_config_file(root).map(PluginSource::ViteConfig);
    }
    if let Some(p) = plugins_file(root) {
        return Some(PluginSource::OjPlugins(p));
    }
    vite_config_file(root).map(PluginSource::ViteConfig)
}

#[derive(Debug, Default)]
pub struct ViteValues {
    pub base: Option<String>,
    /// `publicDir`: a path, or `false` (no public directory).
    pub public_dir: Option<oj_config::BoolOrString>,
    pub port: Option<u16>,
    pub host: Option<String>,
    pub hmr_disabled: bool,
    pub fs_allow: Option<Vec<String>>,
    pub fs_strict: Option<bool>,
    pub define: Option<serde_json::Map<String, serde_json::Value>>,
    pub alias: Option<serde_json::Map<String, serde_json::Value>>,
    pub headers: Option<serde_json::Map<String, serde_json::Value>>,
    pub rollup_options: Option<serde_json::Value>,
    pub assets_inline_limit: Option<u64>,
    pub proxy: Option<serde_json::Value>,
    pub dedupe: Option<Vec<String>>,
    pub optimize_deps: Option<serde_json::Value>,
    /// The `build` block as the extractor normalized it (`outDir`, `sourcemap`,
    /// `minify`, `cssCodeSplit`, `target`, `ssr`); see `extractBuild` in
    /// vite-extract.mjs for the shapes it admits.
    pub build: Option<serde_json::Value>,
    /// `oxc.jsx` as normalized by the extractor (`{ jsx: { runtime, importSource,
    /// pragma, pragmaFrag } }`), and the `esbuild.jsx*` fields for older configs.
    pub oxc: Option<serde_json::Value>,
    pub esbuild: Option<serde_json::Value>,
    /// `ssr` block as normalized by the extractor (`noExternal`/`external`
    /// lists of names, globs or `{ regex }`, or `true`; `target`).
    pub ssr: Option<serde_json::Value>,
    /// A `mode` the config file itself names (resolved only when the CLI gave none).
    pub mode: Option<String>,
    /// `resolve.{extensions,mainFields,conditions,preserveSymlinks}`.
    pub resolve: Option<serde_json::Value>,
    /// The RAW config file's own top-level `resolve` block (the resolved one
    /// above carries Vite's client-environment conditions); consulted by the
    /// Node SSR consumers when the ssr environment is runner-backed.
    pub raw_resolve: Option<serde_json::Value>,
    /// `server.{strictPort,open}` normalized to booleans (`cors` is its own field).
    pub server_flags: Option<serde_json::Value>,
    /// `css.preprocessorOptions.<lang>.additionalData` (string form).
    pub css: Option<serde_json::Value>,
    pub env_prefix: Option<Vec<String>>,
    pub env_dir: Option<String>,
    /// `server.cors` (bool or options object) and `server.allowedHosts` (true or list).
    pub cors: Option<serde_json::Value>,
    pub allowed_hosts: Option<serde_json::Value>,
    /// `preview.*` (port, host, strictPort, open, cors, allowedHosts, headers, proxy).
    pub preview: Option<serde_json::Value>,
    /// `appType` (`spa` | `mpa` | `custom`).
    pub app_type: Option<String>,
    /// `html` block (`cspNonce`).
    pub html: Option<serde_json::Value>,
}

/// How long the config extraction may run before it is terminated. The
/// extractor runs real plugin code (config hooks), so 60 s is generous
/// headroom for a cold first run; `OJ_EXTRACT_TIMEOUT=<seconds>` raises it for
/// configs that legitimately take longer. Unbounded was worse: a config hook
/// that opened a socket or timer used to be able to wedge boot forever (Vite
/// has no bound here, but Vite is also not waiting on a separate evaluation).
fn extraction_timeout() -> std::time::Duration {
    extraction_timeout_from(std::env::var("OJ_EXTRACT_TIMEOUT").ok().as_deref())
}

fn extraction_timeout_from(raw: Option<&str>) -> std::time::Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(60);
    std::time::Duration::from_secs(secs)
}

/// The oj executable one-shot engine jobs run in, set once by the binary's
/// main. A native addon can take the whole process down when it is
/// re-initialized after the engine that first loaded it was torn down —
/// napi-rs before 3.10 corrupts its process-global state then (Node segfaults
/// the same way when a second worker_thread requires such an addon after the
/// first worker exited), and vite 8 apps load rolldown's binding once per
/// engine. A child process per job gives each one-shot engine its own address
/// space, as the pre-embedded node sidecars had.
static ENGINE_JOB_EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn engine_jobs_via_subprocess(exe: PathBuf) {
    let _ = ENGINE_JOB_EXE.set(exe);
}

/// Runs one export of an oj-owned module on a short-lived JS engine and
/// returns its JSON result: in a child `oj engine-job` process when the
/// binary registered itself via [`engine_jobs_via_subprocess`] (see
/// [`ENGINE_JOB_EXE`] for why), in-process otherwise (library consumers,
/// tests).
pub(crate) fn run_engine_job(
    root: &Path,
    module: &Path,
    export: &str,
    payload: serde_json::Value,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, oj_js::EngineError> {
    match ENGINE_JOB_EXE.get() {
        Some(exe) => run_engine_job_subprocess(exe, root, module, export, &payload, timeout),
        None => run_engine_job_in_process(root, module, export, payload, timeout),
    }
}

/// The in-process engine job: one isolate per run preserves the freshness
/// the one-shot subprocesses had (module caches, env dance and run-once plugin
/// guards die with the engine, a hook-started watcher or interval cannot
/// outlive it), and boot's parallel extractions each own their engine thread.
/// The call blocks the current thread, as the bounded subprocess wait did;
/// from inside a tokio runtime it blocks on a scoped helper thread instead so
/// no runtime worker is parked inside another `block_on`.
pub fn run_engine_job_in_process(
    root: &Path,
    module: &Path,
    export: &str,
    payload: serde_json::Value,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, oj_js::EngineError> {
    let run = move || {
        let mut config = oj_js::EngineConfig::new(root);
        config.default_deadline = Some(timeout);
        config.code_cache_dir = Some(crate::engine_code_cache_dir(root));
        let engine = oj_js::JsEngine::spawn(config)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| oj_js::EngineError::Boot(e.to_string()))?;
        rt.block_on(engine.call(module.to_string_lossy().into_owned(), export, vec![payload]))
        // Dropping the engine here joins its thread: pending JS work (timers,
        // watchers a config hook started) is discarded with the isolate, the
        // in-process equivalent of the old `process.exit(0)`.
    };
    if tokio::runtime::Handle::try_current().is_ok() {
        std::thread::scope(|s| {
            s.spawn(run)
                .join()
                .expect("engine job thread must not panic")
        })
    } else {
        run()
    }
}

/// The `--result` file contents of an `oj engine-job` run: the job's outcome,
/// encoded so the parent can rebuild the exact [`oj_js::EngineError`]. A file,
/// not stdout, so job code that prints cannot corrupt the channel.
pub fn engine_job_envelope(
    outcome: &Result<serde_json::Value, oj_js::EngineError>,
) -> serde_json::Value {
    match outcome {
        Ok(value) => serde_json::json!({ "ok": true, "value": value }),
        Err(e) => {
            let (kind, message) = match e {
                oj_js::EngineError::Boot(m) => ("boot", m.clone()),
                oj_js::EngineError::Js(m) => ("js", m.clone()),
                oj_js::EngineError::MemoryLimit => ("memory", String::new()),
                oj_js::EngineError::Deadline => ("deadline", String::new()),
                oj_js::EngineError::Closed => ("closed", String::new()),
            };
            serde_json::json!({ "ok": false, "kind": kind, "message": message })
        }
    }
}

fn engine_job_outcome(envelope: serde_json::Value) -> Result<serde_json::Value, oj_js::EngineError> {
    if envelope.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        return Ok(envelope.get("value").cloned().unwrap_or(serde_json::Value::Null));
    }
    let message = envelope
        .get("message")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    Err(match envelope.get("kind").and_then(|v| v.as_str()) {
        Some("js") => oj_js::EngineError::Js(message),
        Some("memory") => oj_js::EngineError::MemoryLimit,
        Some("deadline") => oj_js::EngineError::Deadline,
        Some("closed") => oj_js::EngineError::Closed,
        _ => oj_js::EngineError::Boot(message),
    })
}

fn run_engine_job_subprocess(
    exe: &Path,
    root: &Path,
    module: &Path,
    export: &str,
    payload: &serde_json::Value,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, oj_js::EngineError> {
    static JOB_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = JOB_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let result_file =
        std::env::temp_dir().join(format!("oj-engine-job-{}-{seq}.json", std::process::id()));
    let boot = |m: String| oj_js::EngineError::Boot(m);
    let mut child = std::process::Command::new(exe)
        .arg("engine-job")
        .arg(module)
        .arg("--root")
        .arg(root)
        .arg("--export")
        .arg(export)
        .arg("--timeout-secs")
        .arg(timeout.as_secs().max(1).to_string())
        .arg("--result")
        .arg(&result_file)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| boot(format!("could not run the engine job child: {e}")))?;
    {
        use std::io::Write;
        let mut stdin = child.stdin.take().expect("piped stdin");
        let sent = serde_json::to_string(payload)
            .map_err(|e| boot(format!("engine job payload: {e}")))
            .and_then(|json| {
                stdin
                    .write_all(json.as_bytes())
                    .map_err(|e| boot(format!("engine job payload: {e}")))
            });
        if let Err(e) = sent {
            let _ = child.kill();
            let _ = child.wait();
            return Err(e);
        }
        // Dropping closes the pipe; the child's read_to_string completes.
    }
    // The child's engine enforces `timeout` itself; the grace covers process
    // start and result writing, then a wedged child is killed like the old
    // bounded subprocess wait did.
    let deadline = std::time::Instant::now() + timeout + std::time::Duration::from_secs(15);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(&result_file);
                return Err(oj_js::EngineError::Deadline);
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(25)),
            Err(e) => {
                let _ = std::fs::remove_file(&result_file);
                return Err(boot(format!("could not wait for the engine job child: {e}")));
            }
        }
    };
    let envelope = std::fs::read_to_string(&result_file);
    let _ = std::fs::remove_file(&result_file);
    if !status.success() {
        // A crash here is an addon or engine taking the child down; the child
        // dying alone (instead of the caller) is this seam's whole point.
        return Err(boot(format!("the engine job child died: {status}")));
    }
    let envelope = envelope.map_err(|e| boot(format!("engine job result: {e}")))?;
    let envelope: serde_json::Value =
        serde_json::from_str(&envelope).map_err(|e| boot(format!("engine job result: {e}")))?;
    engine_job_outcome(envelope)
}

/// Evaluate the app's `vite.config` for `command` ("serve" | "build") and `mode`.
/// A config exported as a function (`defineConfig(({ command, mode }) => ...)`)
/// branches on both, so a build must be extracted as a build: evaluating it as
/// `serve`/`development` silently picks the dev branch of `base`, `define`,
/// `build.outDir` and friends in production output.
pub fn extract_vite_values(root: &Path, command: &str, mode: &str) -> Option<ViteValues> {
    extract_vite_values_with(root, command, mode, true)
}

/// `mode_explicit`: false when `mode` is only the command's default (no CLI
/// `--mode`), which lets a `mode` named in the config file win, as in Vite.
fn extract_vite_values_with(
    root: &Path,
    command: &str,
    mode: &str,
    mode_explicit: bool,
) -> Option<ViteValues> {
    if plugins_file(root).is_some() {
        return None;
    }
    // The cache is keyed per (config, command, mode); a default-mode evaluation
    // can differ from an explicit one, so it gets its own key.
    let mode_key = if mode_explicit {
        mode.to_string()
    } else {
        format!("{mode}@default")
    };
    let mode_key = mode_key.as_str();
    let vite = vite_config_file(root)?;
    let store = extraction_store(root);
    if let Some(hit) = store.lookup(&vite, command, mode_key) {
        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&hit.output) {
            print_extraction_stderr(&hit.stderr);
            let _ = CONFIG_DEPS.set(hit.deps);
            crate::boot_phase("vite-extract cache hit");
            return Some(parse_vite_values(&json));
        }
    }
    let cache = oj_cache::cache_root(root);
    let _ = std::fs::create_dir_all(&cache);
    // Several extractions run concurrently at boot (route tree, server-fn
    // resolver, config values), so the script lands via rename: a plain write
    // truncates it under a concurrent engine's import.
    static EXTRACT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = EXTRACT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let script = cache.join("oj-vite-extract.mjs");
    if std::fs::read(&script).ok().as_deref() != Some(VITE_EXTRACT_JS.as_bytes()) {
        let tmp = cache.join(format!("oj-vite-extract-{}-{seq}.tmp.mjs", std::process::id()));
        std::fs::write(&tmp, VITE_EXTRACT_JS).ok()?;
        std::fs::rename(&tmp, &script).ok()?;
    }
    // Bounded: the config's plugin code runs inside the engine and must never
    // wedge boot forever.
    let timeout = extraction_timeout();
    let payload = serde_json::json!({
        "vite": vite.to_string_lossy(),
        "root": root.to_string_lossy(),
        "command": command,
        "mode": mode,
        "modeKind": if mode_explicit { "explicit" } else { "default" },
        "cacheDir": cache.to_string_lossy(),
    });
    // The result is the call's RETURN VALUE: config code that prints (route
    // generators, banners) cannot corrupt the result channel, which the old
    // subprocess had to dodge with a temp result file next to argv.
    let json = match run_engine_job(root, &script, "extract", payload, timeout) {
        Ok(json) => json,
        Err(oj_js::EngineError::Deadline) => {
            eprintln!(
                "oj: extracting {}: the config evaluation did not finish within {}s and was killed (raise OJ_EXTRACT_TIMEOUT for slower configs)",
                vite.display(),
                timeout.as_secs()
            );
            return None;
        }
        Err(e) => {
            eprintln!("oj: extracting {}: {e}", vite.display());
            return None;
        }
    };
    // Everything the evaluation wrote to stderr (Vite's notices, oj's "not
    // applied" warnings, plugin prints), captured inside the engine.
    let stderr = json
        .get("__stderr")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    print_extraction_stderr(&stderr);
    // The extractor reports a config that failed to evaluate as `__ok: false`
    // (having put the cause in the stderr transcript above). That is not a
    // config with no values, so never parse it into an empty ViteValues: return
    // None and let the caller decide whether a present-but-broken vite.config
    // is an error.
    if json.get("__ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    // Stored once, under the same (config, command, mode_key) the lookup above
    // uses: a default-mode evaluation must not also masquerade as the explicit
    // `--mode <same>` entry, whose evaluation can differ.
    let deps: Vec<PathBuf> = json
        .get("__deps")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|d| d.as_str().map(PathBuf::from))
                .collect()
        })
        .unwrap_or_default();
    let _ = CONFIG_DEPS.set(deps.clone());
    if extraction_deps_truncated(&json) {
        // The read recorder hit its cap: `__deps` is an incomplete stamp of
        // the evaluation's inputs, so a cached entry could survive an edit to
        // an unrecorded file. Serve the result, never cache it.
        eprintln!(
            "oj: extracting {}: the config evaluation read more config-shaped files than the recorder tracks; result not cached",
            vite.display()
        );
    } else {
        // The stderr transcript is stored in its own field (replayed by the
        // lookup above), not inside the cached output.
        let mut stored = json.clone();
        if let Some(obj) = stored.as_object_mut() {
            obj.remove("__stderr");
        }
        store.store(&vite, command, mode_key, &deps, &stored.to_string(), &stderr);
    }
    EXTRACTION_RAN_FRESH.store(true, std::sync::atomic::Ordering::Relaxed);
    crate::boot_phase("vite-extract cache miss (engine ran)");
    Some(parse_vite_values(&json))
}

/// Whether any config extraction in this process ran the engine (a cache
/// miss): the config's observable inputs changed, so caches derived from the
/// evaluated config — the deps pre-seed stamp — must not serve either.
static EXTRACTION_RAN_FRESH: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn extraction_ran_fresh() -> bool {
    EXTRACTION_RAN_FRESH.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the extractor's read recorder overflowed (`__depsTruncated`): the
/// dep list is then honest-but-incomplete and the extraction must not be
/// cached under it.
fn extraction_deps_truncated(json: &serde_json::Value) -> bool {
    json.get("__depsTruncated").and_then(|v| v.as_bool()) == Some(true)
}

/// What the config extractor wrote to stderr (Vite's own notices and oj's
/// "not applied" warnings), printed once per process. The config is loaded
/// several times in a dev session (the Start route tree, server-fn resolver and
/// client bundle each adopt it, and again after a rebuild), each replaying the
/// cached stderr; Vite prints its config warnings once at startup.
fn print_extraction_stderr(stderr: &str) {
    let fresh = unseen_extraction_lines(stderr);
    if !fresh.is_empty() {
        eprint!("{fresh}");
    }
}

fn unseen_extraction_lines(stderr: &str) -> String {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> = std::sync::Mutex::new(None);
    let mut guard = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = guard.get_or_insert_with(std::collections::HashSet::new);
    let mut out = String::new();
    for line in stderr.lines() {
        if line.trim().is_empty() || seen.insert(line.to_string()) {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// The extraction cache, keyed on everything that can change the verdict: oj's
/// version, the extraction engine ("deno" marks the in-process engine — a
/// cache written by a node-subprocess-era oj, or any future engine change,
/// must never serve, whatever the script hash happens to be), the extraction
/// script itself and the observable environment.
fn extraction_store(root: &Path) -> oj_cache::config_extract::ConfigExtractStore {
    oj_cache::config_extract::ConfigExtractStore::new(
        root,
        &format!(
            "{}:deno:{}:{}",
            env!("CARGO_PKG_VERSION"),
            blake3::hash(VITE_EXTRACT_JS.as_bytes()).to_hex(),
            extraction_env_hash(std::env::vars())
        ),
    )
}

/// The part of the process environment a vite.config can observe while it
/// evaluates (`process.env.VITE_*` and `NODE_ENV`), hashed into the extraction
/// cache key so an env change re-evaluates the config instead of serving the
/// values computed under the old one.
pub fn extraction_env_hash(vars: impl Iterator<Item = (String, String)>) -> String {
    let mut relevant: Vec<(String, String)> = vars
        .filter(|(k, _)| k == "NODE_ENV" || k.starts_with("VITE_"))
        .collect();
    relevant.sort();
    let mut hasher = blake3::Hasher::new();
    for (k, v) in relevant {
        hasher.update(k.as_bytes());
        hasher.update(&[b'=']);
        hasher.update(v.as_bytes());
        hasher.update(&[0]);
    }
    hasher.finalize().to_hex().to_string()
}

/// The files the config file imported, as the extractor reported them (Vite's
/// `configFileDependencies`): the dev server restarts when one changes.
static CONFIG_DEPS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();

pub fn config_dependencies() -> &'static [PathBuf] {
    CONFIG_DEPS.get().map(Vec::as_slice).unwrap_or(&[])
}

#[inline]
fn parse_vite_values(json: &serde_json::Value) -> ViteValues {
    ViteValues {
        base: json
            .get("base")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        public_dir: match json.get("publicDir") {
            Some(serde_json::Value::String(s)) => Some(oj_config::BoolOrString::Str(s.clone())),
            Some(serde_json::Value::Bool(false)) => Some(oj_config::BoolOrString::Bool(false)),
            _ => None,
        },
        port: json.get("port").and_then(|v| v.as_u64()).map(|p| p as u16),
        host: json
            .get("host")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        hmr_disabled: json.get("hmr").and_then(|v| v.as_bool()) == Some(false),
        fs_allow: json.get("fsAllow").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        }),
        fs_strict: json.get("fsStrict").and_then(|v| v.as_bool()),
        define: json.get("define").and_then(|v| v.as_object()).cloned(),
        alias: json.get("alias").and_then(|v| v.as_object()).cloned(),
        headers: json.get("headers").and_then(|v| v.as_object()).cloned(),
        rollup_options: json.get("rollupOptions").filter(|v| !v.is_null()).cloned(),
        assets_inline_limit: json.get("assetsInlineLimit").and_then(|v| v.as_u64()),
        proxy: json.get("proxy").filter(|v| !v.is_null()).cloned(),
        dedupe: json.get("dedupe").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        }),
        optimize_deps: json.get("optimizeDeps").filter(|v| !v.is_null()).cloned(),
        build: json.get("build").filter(|v| !v.is_null()).cloned(),
        oxc: json.get("oxc").filter(|v| !v.is_null()).cloned(),
        esbuild: json.get("esbuild").filter(|v| !v.is_null()).cloned(),
        ssr: json.get("ssr").filter(|v| !v.is_null()).cloned(),
        mode: json.get("mode").and_then(|v| v.as_str()).map(str::to_string),
        resolve: json.get("resolve").filter(|v| !v.is_null()).cloned(),
        raw_resolve: json.get("rawResolve").filter(|v| !v.is_null()).cloned(),
        server_flags: json.get("serverFlags").filter(|v| !v.is_null()).cloned(),
        css: json.get("css").filter(|v| !v.is_null()).cloned(),
        env_prefix: json.get("envPrefix").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        }),
        env_dir: json.get("envDir").and_then(|v| v.as_str()).map(str::to_string),
        cors: json.get("cors").filter(|v| !v.is_null()).cloned(),
        allowed_hosts: json.get("allowedHosts").filter(|v| !v.is_null()).cloned(),
        preview: json.get("preview").filter(|v| !v.is_null()).cloned(),
        app_type: json.get("appType").and_then(|v| v.as_str()).map(str::to_string),
        html: json.get("html").filter(|v| !v.is_null()).cloned(),
    }
}

#[inline]
pub fn adopt_vite_config_values(
    config: &mut oj_config::OjConfig,
    root: &Path,
    command: &str,
    mode: &str,
) -> Result<(), String> {
    let Some(v) = extract_vite_values(root, command, mode) else {
        // No vite.config is fine: nothing to adopt. A vite.config that exists but
        // failed to evaluate is not: Vite fails hard here ("failed to load config
        // from ..."), and silently carrying on would build or serve with defaults
        // the app never asked for. An explicit oj.plugins file takes precedence over
        // vite.config (the extractor skips it then), so only the vite path is an
        // error. The extractor has already printed the underlying cause to stderr.
        if let Some(named) = VITE_CONFIG_OVERRIDE.get() {
            if !named.is_file() {
                return Err(format!(
                    "failed to load config from {}: --config names a file that does not exist",
                    named.display()
                ));
            }
        }
        if plugins_file(root).is_none() {
            if let Some(path) = vite_config_file(root) {
                return Err(format!("failed to load config from {}", path.display()));
            }
        }
        return Ok(());
    };
    merge_vite_values(config, v);
    Ok(())
}

/// Like `adopt_vite_config_values`, for a `mode` that is only the command's
/// default: the config file's own `mode` (if any) is honored and lands in
/// `config.mode` so the caller can reload under it.
pub fn adopt_vite_config_values_default_mode(
    config: &mut oj_config::OjConfig,
    root: &Path,
    command: &str,
    mode: &str,
) -> Result<(), String> {
    let Some(v) = extract_vite_values_with(root, command, mode, false) else {
        // Same rule as `adopt_vite_config_values`: a present vite.config that
        // failed to evaluate is an error, a missing one is nothing to adopt.
        if let Some(named) = VITE_CONFIG_OVERRIDE.get() {
            if !named.is_file() {
                return Err(format!(
                    "failed to load config from {}: --config names a file that does not exist",
                    named.display()
                ));
            }
        }
        if plugins_file(root).is_none() {
            if let Some(path) = vite_config_file(root) {
                return Err(format!("failed to load config from {}", path.display()));
            }
        }
        return Ok(());
    };
    merge_vite_values(config, v);
    Ok(())
}

fn merge_vite_values(config: &mut oj_config::OjConfig, v: ViteValues) {
    if config.base.is_none() {
        config.base = v.base;
    }
    if config.public_dir.is_none() {
        config.public_dir = v.public_dir;
    }
    if let Some(vdef) = v.define {
        let def = config.define.get_or_insert_with(Default::default);
        for (k, val) in vdef {
            def.entry(k).or_insert(val);
        }
    }
    if v.hmr_disabled {
        let sc = config.server.get_or_insert_with(Default::default);
        if sc.hmr.is_none() {
            sc.hmr = Some(oj_config::HmrConfig::Toggle(false));
        }
    }
    if v.port.is_some()
        || v.host.is_some()
        || v.headers.is_some()
        || v.fs_allow.is_some()
        || v.fs_strict.is_some()
    {
        let sc = config.server.get_or_insert_with(Default::default);
        if sc.port.is_none() {
            sc.port = v.port;
        }
        if sc.host.is_none() {
            sc.host = v.host;
        }
        if sc.fs.is_none() {
            if v.fs_allow.is_some() || v.fs_strict.is_some() {
                sc.fs = Some(oj_config::FsConfig {
                    allow: v.fs_allow,
                    strict: v.fs_strict,
                    deny: None,
                });
            }
        }
        if sc.headers.is_none() {
            if let Some(vheaders) = v.headers {
                let map = vheaders
                    .into_iter()
                    .filter_map(|(k, val)| val.as_str().map(|s| (k, s.to_string())))
                    .collect::<std::collections::BTreeMap<_, _>>();
                if !map.is_empty() {
                    sc.headers = Some(map);
                }
            }
        }
    }
    if let Some(valias) = v.alias {
        if !valias.is_empty() {
            let rc = config.resolve.get_or_insert_with(Default::default);
            let map = rc.alias.get_or_insert_with(Default::default);
            for (find, replacement) in valias {
                if let Some(s) = replacement.as_str() {
                    map.entry(find).or_insert_with(|| s.to_string());
                }
            }
        }
    }
    if let Some(ro) = v.rollup_options {
        let build = config.build.get_or_insert_with(Default::default);
        if build.rollup_options.is_none() && build.rolldown_options.is_none() {
            build.rollup_options = Some(ro);
        }
    }
    if let Some(limit) = v.assets_inline_limit {
        let build = config.build.get_or_insert_with(Default::default);
        build.assets_inline_limit.get_or_insert(limit);
    }
    if let Some(proxy) = v.proxy {
        let sc = config.server.get_or_insert_with(Default::default);
        if sc.proxy.is_none() {
            if let Ok(map) = serde_json::from_value::<
                std::collections::BTreeMap<String, oj_config::ProxyEntry>,
            >(proxy)
            {
                if !map.is_empty() {
                    sc.proxy = Some(map);
                }
            }
        }
    }
    if let Some(dedupe) = v.dedupe {
        if !dedupe.is_empty() {
            let rc = config.resolve.get_or_insert_with(Default::default);
            rc.dedupe.get_or_insert(dedupe);
        }
    }
    if let Some(od) = v.optimize_deps {
        if config.optimize_deps.is_none() {
            if let Ok(parsed) = serde_json::from_value::<oj_config::OptimizeDepsConfig>(od) {
                config.optimize_deps = Some(parsed);
            }
        }
    }
    if let Some(vb) = v.build.as_ref().and_then(|b| b.as_object()) {
        let build = config.build.get_or_insert_with(Default::default);
        let str_of = |k: &str| vb.get(k).and_then(|v| v.as_str()).map(str::to_string);
        let bool_of = |k: &str| vb.get(k).and_then(|v| v.as_bool());
        if build.out_dir.is_none() {
            build.out_dir = str_of("outDir");
        }
        let bool_or_str = |k: &str| match vb.get(k) {
            Some(serde_json::Value::Bool(b)) => Some(oj_config::BoolOrString::Bool(*b)),
            Some(serde_json::Value::String(s)) => Some(oj_config::BoolOrString::Str(s.clone())),
            _ => None,
        };
        if build.sourcemap.is_none() {
            build.sourcemap = bool_or_str("sourcemap");
        }
        if build.minify.is_none() {
            build.minify = bool_or_str("minify");
        }
        if build.css_code_split.is_none() {
            build.css_code_split = bool_of("cssCodeSplit");
        }
        if build.target.is_none() {
            build.target = match vb.get("target") {
                Some(serde_json::Value::String(s)) => Some(oj_config::StringOrList::One(s.clone())),
                Some(serde_json::Value::Array(a)) => Some(oj_config::StringOrList::Many(
                    a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                )),
                _ => None,
            };
        }
        if build.empty_out_dir.is_none() {
            build.empty_out_dir = bool_of("emptyOutDir");
        }
        if build.module_preload.is_none() {
            build.module_preload = vb.get("modulePreload").filter(|v| !v.is_null()).cloned();
        }
        if build.ssr.is_none() {
            build.ssr = bool_or_str("ssr");
        }
        if build.copy_public_dir.is_none() {
            build.copy_public_dir = bool_of("copyPublicDir");
        }
        if build.ssr_manifest.is_none() {
            build.ssr_manifest = bool_or_str("ssrManifest");
        }
        if build.manifest.is_none() {
            build.manifest = bool_or_str("manifest");
        }
        if build.css_minify.is_none() {
            build.css_minify = bool_or_str("cssMinify");
        }
        if build.assets_dir.is_none() {
            build.assets_dir = str_of("assetsDir");
        }
        if build.report_compressed_size.is_none() {
            build.report_compressed_size = bool_of("reportCompressedSize");
        }
        if build.chunk_size_warning_limit.is_none() {
            build.chunk_size_warning_limit = vb.get("chunkSizeWarningLimit").and_then(|v| v.as_f64());
        }
        if build.write.is_none() {
            build.write = bool_of("write");
        }
        for (key, slot) in [
            ("watch", &mut build.watch),
            ("license", &mut build.license),
            ("commonjsOptions", &mut build.commonjs_options),
        ] {
            if slot.is_none() {
                *slot = vb.get(key).filter(|v| !v.is_null()).cloned();
            }
        }
        if build.css_target.is_none() {
            build.css_target = match vb.get("cssTarget") {
                Some(serde_json::Value::String(s)) => Some(oj_config::StringOrList::One(s.clone())),
                Some(serde_json::Value::Array(a)) => Some(oj_config::StringOrList::Many(
                    a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
                )),
                _ => None,
            };
        }
        if build.lib.is_none() {
            build.lib = vb
                .get("lib")
                .cloned()
                .and_then(|l| serde_json::from_value::<oj_config::LibConfig>(l).ok());
        }
    }
    if v.cors.is_some() || v.allowed_hosts.is_some() {
        let sc = config.server.get_or_insert_with(Default::default);
        if sc.cors.is_none() {
            sc.cors = v.cors.and_then(|c| serde_json::from_value(c).ok());
        }
        if sc.allowed_hosts.is_none() {
            sc.allowed_hosts = v.allowed_hosts.and_then(|a| serde_json::from_value(a).ok());
        }
    }
    if let Some(preview) = v.preview {
        if let Ok(parsed) = serde_json::from_value::<oj_config::PreviewConfig>(preview) {
            let pc = config.preview.get_or_insert_with(Default::default);
            pc.port = pc.port.or(parsed.port);
            pc.host = pc.host.take().or(parsed.host);
            pc.strict_port = pc.strict_port.or(parsed.strict_port);
            pc.open = pc.open.take().or(parsed.open);
            pc.cors = pc.cors.take().or(parsed.cors);
            pc.allowed_hosts = pc.allowed_hosts.take().or(parsed.allowed_hosts);
            pc.headers = pc.headers.take().or(parsed.headers);
            pc.proxy = pc.proxy.take().or(parsed.proxy);
        }
    }
    if config.app_type.is_none() {
        config.app_type = v.app_type;
    }
    if config.oxc.is_none() {
        config.oxc = v.oxc;
    }
    if config.html.is_none() {
        config.html = v.html.and_then(|h| serde_json::from_value(h).ok());
    }
    if config.esbuild.is_none() {
        config.esbuild = v.esbuild;
    }
    // The ssr block merges PER-KEY, not whole-block: an oj.config.json that
    // sets one ssr key (say noExternal) must not drop the extractor's other
    // keys — above all `runnerBacked`, which ONLY extraction produces and every
    // consumer of the worker path reads, so it is always adopted. `resolve`
    // recurses ONE level deeper for the same reason: an oj-side
    // `ssr.resolve.externalConditions` must not drop the extractor's other
    // resolve sub-keys (the workerd sugar's `conditions` above all).
    match (config.ssr.as_mut(), v.ssr) {
        (None, vssr) => config.ssr = vssr,
        (Some(existing), Some(vssr)) => {
            if !existing.is_object() {
                // The oj-side ssr value is not an object: there is nothing to
                // merge per-key into, and dropping the extractor block here
                // would break the "runnerBacked is always adopted" contract.
                eprintln!(
                    "oj: config: the ssr block in oj's config is not an object; the vite.config ssr block is used"
                );
                *existing = vssr;
            } else if let (Some(obj), Some(vobj)) = (existing.as_object_mut(), vssr.as_object()) {
                for (k, val) in vobj {
                    if k == "runnerBacked" || !obj.contains_key(k) {
                        obj.insert(k.clone(), val.clone());
                    } else if k == "resolve" {
                        let Some(vsub) = val.as_object() else { continue };
                        if obj.get(k).is_some_and(serde_json::Value::is_object) {
                            let eobj = obj
                                .get_mut(k)
                                .and_then(serde_json::Value::as_object_mut)
                                .expect("checked is_object above");
                            for (sk, sval) in vsub {
                                if !eobj.contains_key(sk) {
                                    eobj.insert(sk.clone(), sval.clone());
                                }
                            }
                        } else {
                            // Nothing to merge into: adopting the extractor's
                            // block beats silently dropping the sugar's
                            // conditions.
                            eprintln!(
                                "oj: config: ssr.resolve in oj's config is not an object; the vite.config ssr.resolve block is used"
                            );
                            obj.insert(k.clone(), val.clone());
                        }
                    }
                }
            }
        }
        (Some(_), None) => {}
    }
    if config.mode.is_none() {
        config.mode = v.mode;
    }
    if let Some(vr) = v.resolve.as_ref().and_then(|r| r.as_object()) {
        let rc = config.resolve.get_or_insert_with(Default::default);
        let list = |k: &str| {
            vr.get(k).and_then(|x| x.as_array()).map(|a| {
                a.iter()
                    .filter_map(|s| s.as_str().map(str::to_string))
                    .collect::<Vec<_>>()
            })
        };
        if rc.extensions.is_none() {
            rc.extensions = list("extensions");
        }
        if rc.main_fields.is_none() {
            rc.main_fields = list("mainFields");
        }
        if rc.conditions.is_none() {
            rc.conditions = list("conditions");
        }
        if rc.external_conditions.is_none() {
            rc.external_conditions = list("externalConditions");
        }
        if rc.preserve_symlinks.is_none() {
            rc.preserve_symlinks = vr.get("preserveSymlinks").and_then(|b| b.as_bool());
        }
    }
    if config.raw_resolve.is_none() {
        config.raw_resolve = v
            .raw_resolve
            .and_then(|r| serde_json::from_value::<oj_config::ResolveConfig>(r).ok());
    }
    if let Some(sf) = v.server_flags.as_ref().and_then(|s| s.as_object()) {
        if config.app_type.is_none() {
            config.app_type = sf.get("appType").and_then(|a| a.as_str()).map(str::to_string);
        }
        let sc = config.server.get_or_insert_with(Default::default);
        if sc.strict_port.is_none() {
            sc.strict_port = sf.get("strictPort").and_then(|b| b.as_bool());
        }
        if sc.open.is_none() {
            sc.open = sf.get("open").and_then(|b| b.as_bool());
        }
        if sc.hmr.is_none() {
            sc.hmr = sf
                .get("hmr")
                .and_then(|h| serde_json::from_value::<oj_config::HmrOptions>(h.clone()).ok())
                .map(oj_config::HmrConfig::Options);
        }
        if sc.watch.is_none() {
            sc.watch = sf
                .get("watch")
                .and_then(|w| serde_json::from_value::<oj_config::WatchConfig>(w.clone()).ok());
        }
        if let Some(strict) = sf.get("fsStrict").and_then(|b| b.as_bool()) {
            let fs = sc.fs.get_or_insert_with(Default::default);
            if fs.strict.is_none() {
                fs.strict = Some(strict);
            }
        }
        if sf.get("skipWebSocketTokenCheck").and_then(|b| b.as_bool()) == Some(true) {
            let legacy = config.legacy.get_or_insert_with(Default::default);
            if legacy.skip_web_socket_token_check.is_none() {
                legacy.skip_web_socket_token_check = Some(true);
            }
        }
    }
    if let Some(css) = v.css.as_ref() {
        if config.css.is_none() {
            // The whole block (preprocessorOptions, devSourcemap, modules).
            config.css = serde_json::from_value::<oj_config::CssConfig>(css.clone()).ok();
        } else if let Some(po) = css.get("preprocessorOptions").and_then(|p| p.as_object()) {
            let cfg = config.css.as_mut().unwrap();
            let map = cfg.preprocessor_options.get_or_insert_with(Default::default);
            for (lang, opts) in po {
                let Some(data) = opts.get("additionalData").and_then(|d| d.as_str()) else {
                    continue;
                };
                let entry = map.entry(lang.clone()).or_default();
                if entry.additional_data.is_none() {
                    entry.additional_data = Some(data.to_string());
                }
            }
        }
    }
    if config.env_prefix.is_none() {
        if let Some(p) = v.env_prefix.filter(|p| !p.is_empty()) {
            config.env_prefix = Some(oj_config::StringOrList::Many(p));
        }
    }
    if config.env_dir.is_none() {
        config.env_dir = v.env_dir;
    }
}

pub struct PluginHost {
    /// The embedded engine hosting plugin-host.mjs. In an Option so
    /// `declare_gone`/`shutdown` can take + abandon it explicitly (background
    /// tasks hold Arc clones of the host, so dropping the caller's Arc alone
    /// must never decide the engine's fate). Abandoning drops the job channel
    /// and detaches the isolate thread; a thread wedged in NATIVE code (a napi
    /// call) leaks with its isolate — the accepted cost of the
    /// in-process host, where a kill used to reclaim it (JS-only wedges are
    /// interrupted by `terminate_execution` and unwind cleanly).
    engine: Mutex<Option<std::sync::Arc<oj_js::JsEngine>>>,
    /// The plugin-host.mjs path on disk — the module every hook call targets.
    host_module: String,
    ws_out: Mutex<Option<tokio::sync::broadcast::Sender<String>>>,
    /// `{ ojServer: { action, ... } }` pushes from the host: a plugin invalidating
    /// a module via server.moduleGraph, or server.restart().
    server_events: Mutex<Option<tokio::sync::mpsc::UnboundedSender<serde_json::Value>>>,
    /// The host's `{ ojServeInfo: ... }` control push: None until the host's
    /// top-level init completes. Subscribers see the info whenever the host
    /// eventually comes up, however slow the boot, and can activate the
    /// middleware path late instead of silently degrading to the SSR runner.
    serve_info_push: tokio::sync::watch::Sender<Option<ServeInfo>>,
    /// Whether the host finished its top-level init: flipped by the serve-info
    /// push, the `{ ojInit }` push, or the first hook reply (the host's hook
    /// entry point only runs after every top-level await, so any reply proves
    /// init completed). Hook calls are gated on this — see `call`.
    initialized: tokio::sync::watch::Sender<bool>,
    /// The host is gone (its engine thread exited, its init failed hard, or
    /// the transport belt declared it wedged): fail calls fast instead of
    /// waiting out the init deadline or the per-call timeout. A watch so a
    /// waiter (`host_gone_wait`) can select on the death instead of polling.
    host_gone: tokio::sync::watch::Sender<bool>,
    /// When the host's CURRENT generation was spawned (reset by a revive);
    /// the init deadline is measured from here, so boot RPCs share one
    /// deadline instead of stacking a fresh one each.
    spawned: Mutex<tokio::time::Instant>,
    /// Per-spawn init-wait policy: how long a call may wait for the host's
    /// top-level init. The boot/serve host takes the long init deadline (boot
    /// correctness depends on its snapshot RPCs), shared across calls and
    /// measured from spawn; a lazily spawned host (the SSR environment host,
    /// spawned on the first SSR request) takes the short per-call bound,
    /// measured from EACH call's own start (see `lazy`), so a wedged init
    /// degrades like a slow hook instead of freezing the watcher thread and
    /// browser-facing SSR transforms.
    init_wait: std::time::Duration,
    /// Whether this host was lazily spawned: its init wait is then anchored to
    /// each call's own start rather than to the spawn instant — a
    /// spawn-anchored short bound gave calls arriving after `spawn +
    /// init_wait` during a still-pending init a zero-length window (instant
    /// failure), where pre-init-gate semantics gave every call its own
    /// per-call timeout. Every pre-init call gets its own full window: an
    /// earlier call's expired window is evidence of a slow boot, not a wedge,
    /// so it never fails a later call early (see `call`).
    lazy: bool,
    /// Wedge EVIDENCE, not a call gate: flips true when a pre-init call's
    /// full init window elapsed with init still pending, or when the stall
    /// monitor saw a full RPC-scale window pass with no init milestone (see
    /// `init_progress_seen`), and back false the moment init progresses (an
    /// `initialized` flip or a milestone). Calls never consult
    /// it — time alone must not fail a call that a landing init would have
    /// served — but waiters gating separate work on the host's health (the
    /// Start prewarm hold) select on it, alongside `host_gone`, instead of
    /// running their own flat timers against a healthy slow boot.
    init_failed: tokio::sync::watch::Sender<bool>,
    /// The env knob named when the init wait elapses (matches `init_wait`).
    init_knob: &'static str,
    /// Count of `{ ojResyncDone }` pushes: the host sends one when an enqueued
    /// worker-environment resync actually EXECUTES (its /__oj_invalidate ack
    /// only means "enqueued"). A counter, not a flag: a waiter compares
    /// against the value it saw before enqueueing, so a completion landing
    /// before the wait starts is never missed, and one push may answer
    /// several coalesced enqueues.
    resync_done: tokio::sync::watch::Sender<u64>,
    /// Count of `{ ojInitProgress }` pushes: the host reports real milestones
    /// through its top-level init (script start, plugins loaded, each
    /// config-phase hook). The stall monitor (see `spawn_with_policy`)
    /// measures its wedge window from the LAST milestone, so a healthy slow
    /// boot that keeps progressing is never called wedged, while a host gone
    /// silent for a full RPC-scale window pre-init is — evidence a caller's
    /// own window cannot provide on the boot host, whose per-call windows
    /// equal the whole init deadline.
    init_progress_seen: tokio::sync::watch::Sender<u64>,
    /// The per-call RPC timeout, snapshotted at spawn (`plugin_rpc_timeout`;
    /// the env knob cannot change mid-process). Tests override it to exercise
    /// the transport belts without racing the env other tests read.
    rpc_wait: std::time::Duration,
    /// Last "still initializing" progress line, so concurrent init-gated calls
    /// print one line per interval, not one each.
    init_progress: Mutex<std::time::Instant>,
    /// Everything a respawn needs to boot a fresh engine generation: the
    /// composed boot seed (pluginsPath/initialJson/cacheRoot), the app root
    /// the engine and its resolver are built over, and the stall monitor's
    /// window. Snapshotted at spawn; a respawn boots the same host the same
    /// way, only on a fresh isolate.
    boot: BootContext,
    /// The revive budget and the engine GENERATION, one lock so a death
    /// report and a concurrent revive serialize: a report carries the
    /// generation of the engine it is about, and a report about a replaced
    /// engine is stale — ignoring it is what keeps the belt of an old,
    /// abandoned call from killing the freshly revived host.
    revive: Mutex<ReviveState>,
    /// Set by `shutdown()`: the host was retired on purpose and must never be
    /// revived (the push dispatcher's channel-close latches `host_gone` on a
    /// clean shutdown exactly like on a death).
    shut_down: std::sync::atomic::AtomicBool,
    /// A weak self-handle so `&self` methods (the call path's revive) can
    /// hand the background tasks of a fresh generation their `Arc`s.
    self_ref: std::sync::OnceLock<std::sync::Weak<PluginHost>>,
}

/// See [`PluginHost::boot`].
struct BootContext {
    boot_seed: String,
    root: PathBuf,
    stall_wait: std::time::Duration,
    /// The engine heap cap every generation is spawned with (see
    /// `plugin_host_memory_mb`).
    memory_limit_bytes: usize,
}

/// See [`PluginHost::revive`].
struct ReviveState {
    /// The live engine's generation; bumped by each revive.
    generation: u64,
    /// Respawns consumed. A LIFETIME budget, deliberately never reset by a
    /// successful boot: a wedge that recurs every generation would otherwise
    /// respawn forever, and each natively wedged generation leaks a detached
    /// isolate thread — past the budget the host stays gone and the outer
    /// supervisor (a dev-server restart) owns recovery, as it always did.
    attempts: u32,
    /// When the last revive ran, spacing attempts out so a burst of calls
    /// against a recurring wedge cannot burn the whole budget at once.
    last: Option<std::time::Instant>,
    /// Native addons already pending unsafe re-registration when THIS host
    /// died (snapshotted by `declare_gone`, before the abandon). An addon
    /// that shows up pending only AFTER the death was held by the dead
    /// generation itself — the successor runs the same plugins and will
    /// re-register it, so the revive gate refuses on exactly those (see
    /// `try_revive`); addons some other engine's teardown orphaned are not
    /// this host's to reload and never block it.
    pending_before: std::collections::HashSet<PathBuf>,
}

/// Respawns per host lifetime (see `ReviveState::attempts`).
const PLUGIN_HOST_RESPAWN_LIMIT: u32 = 3;
/// Minimum spacing between respawns (see `ReviveState::last`).
const PLUGIN_HOST_RESPAWN_SPACING: std::time::Duration = std::time::Duration::from_secs(5);

/// The plugin-host engine's heap cap: Node parity. The process host was a
/// `node` child, so a deployment's `NODE_OPTIONS --max-old-space-size` capped
/// it (and Node's ~4GB default did otherwise), and V8 exhaustion crashed it
/// into a supervisor restart. The embedded engine takes the same cap from the
/// same places — `OJ_PLUGIN_MEMORY_MB` first, then the inherited
/// `NODE_OPTIONS` flag, then 4096MB — but degrades gracefully: the near-limit
/// callback fails the running call with MemoryLimit, the host is declared
/// gone, and the next call revives it on a fresh heap. Uncapped (the previous
/// behavior), a heap blow-up ends in a GC storm the transport belt can only
/// read as a native wedge — or in V8's fatal OOM, which aborts the whole
/// dev-server process.
fn plugin_host_memory_mb() -> usize {
    if let Some(mb) = std::env::var("OJ_PLUGIN_MEMORY_MB")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|m| *m > 0)
    {
        return mb;
    }
    std::env::var("NODE_OPTIONS")
        .ok()
        .and_then(|opts| max_old_space_mb(&opts))
        .unwrap_or(4096)
}

/// The last `--max-old-space-size` in a NODE_OPTIONS value (last wins, like
/// Node; Node also accepts the underscore spelling), so the heap cap a
/// deployment already sets for its Node processes carries over to the
/// embedded engine unchanged.
fn max_old_space_mb(node_options: &str) -> Option<usize> {
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

/// This process's resident set size in MB, best effort, for the host-death
/// diagnostics: Linux reads the live value from /proc; the other unixes fall
/// back to getrusage's PEAK (close enough for a grown process, which is what
/// a wedge diagnostic is looking at).
fn process_rss_mb() -> Option<u64> {
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
/// info), answered SYNCHRONOUSLY on the engine's isolate thread through the
/// `__oj_rpc` bridge — both handlers are plain resolver/fs/compile work, so
/// the old request/reply plumbing (ids, a pending map, bounded stdin writes)
/// has no in-process counterpart at all.
fn ctx_rpc(
    method: &str,
    args: &[serde_json::Value],
    resolver: &OjResolver,
    root: &Path,
) -> Result<serde_json::Value, String> {
    match method {
        "resolve" => {
            let source = args.first().and_then(|v| v.as_str()).unwrap_or("");
            let importer = args.get(1).and_then(|v| v.as_str()).unwrap_or("");
            let dir = if importer.is_empty() {
                root.to_path_buf()
            } else {
                Path::new(importer)
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.to_path_buf())
            };
            Ok(match resolver.resolve(&dir, source) {
                Ok(p) => serde_json::Value::String(p.display().to_string()),
                Err(_) => serde_json::Value::Null,
            })
        }
        "moduleInfo" => {
            let id = args.first().and_then(|v| v.as_str()).unwrap_or("");
            let path = Path::new(id);
            match std::fs::read_to_string(path) {
                Ok(src) => {
                    let dir = path
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| root.to_path_buf());
                    let (code, imports) = match oj_compiler::compile(
                        path,
                        &src,
                        &oj_compiler::CompileOptions::prod(),
                    ) {
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
                Err(_) => Ok(serde_json::Value::Null),
            }
        }
        other => Err(format!("unknown ctx method: {other}")),
    }
}

/// How long one plugin hook may run before oj gives up on it. Vite has no
/// hook timeout at all; oj's default of 20 s keeps a hung plugin from wedging
/// the server, and `OJ_PLUGIN_TIMEOUT=<seconds>` raises it for plugins that
/// legitimately take longer (a large first-run codegen, a cold type check).
pub fn plugin_rpc_timeout() -> std::time::Duration {
    plugin_rpc_timeout_from(std::env::var("OJ_PLUGIN_TIMEOUT").ok().as_deref())
}

fn plugin_rpc_timeout_from(raw: Option<&str>) -> std::time::Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(20);
    std::time::Duration::from_secs(secs)
}

/// How long the plugin host may take to finish its top-level init (loading the
/// config, config/configResolved/configureServer, a Miniflare boot) before an
/// RPC waiting on it gives up. The host answers RPCs only after init, so this
/// gates `call` instead of racing the per-call timeout against a slow boot;
/// Vite has no bound at all here (its startup simply awaits the hooks).
/// `OJ_PLUGIN_INIT_TIMEOUT=<seconds>` adjusts it.
pub fn plugin_init_timeout() -> std::time::Duration {
    plugin_init_timeout_from(std::env::var("OJ_PLUGIN_INIT_TIMEOUT").ok().as_deref())
}

fn plugin_init_timeout_from(raw: Option<&str>) -> std::time::Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(300);
    std::time::Duration::from_secs(secs)
}

impl std::fmt::Debug for PluginHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PluginHost")
    }
}

/// The old spawn's `kill_on_drop`, in-process: a host dropped without an
/// explicit shutdown abandons its engine instead of letting the last
/// `Arc<JsEngine>` drop JOIN a thread that may be parked in a never-settling
/// init forever.
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

/// The init deadline one `call` waits out while the host is uninitialized: a
/// boot host shares the spawn-anchored deadline (`spawned + init_wait`), a
/// lazy host anchors `init_wait` to the CALL's own start so a call arriving
/// long after spawn still gets a full window (init landing releases it early).
fn call_init_deadline(
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

/// The init-wait policy per spawn kind (see `PluginHost::init_wait`): a boot
/// host gets the long init deadline, a lazily spawned host the short per-call
/// bound — each named after the env knob that adjusts it.
fn init_wait_policy(lazy: bool) -> (std::time::Duration, &'static str) {
    if lazy {
        (plugin_rpc_timeout(), "OJ_PLUGIN_TIMEOUT")
    } else {
        (plugin_init_timeout(), "OJ_PLUGIN_INIT_TIMEOUT")
    }
}

/// Per-spawn timeout overrides, for tests that must exercise the init-wait,
/// stall-monitor and transport-belt semantics without racing the env-var
/// knobs other tests read. Production spawns pass the default (env-derived).
#[derive(Default)]
struct SpawnTimeouts {
    init_wait: Option<std::time::Duration>,
    /// The stall monitor's no-progress window (defaults to the RPC timeout).
    stall: Option<std::time::Duration>,
    /// The per-call RPC timeout (`PluginHost::rpc_wait`).
    rpc: Option<std::time::Duration>,
    /// The engine heap cap (defaults to `plugin_host_memory_mb`).
    memory: Option<usize>,
}

impl PluginHost {
    /// Spawn a boot-time host: calls wait out the full init deadline
    /// (`OJ_PLUGIN_INIT_TIMEOUT`), because boot correctness depends on its
    /// snapshot RPCs (config defines, hook gates, serve info).
    pub async fn spawn(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_with_policy(root, plugins_file, config_json, false, SpawnTimeouts::default())
            .await
    }

    /// Spawn a lazily created host (the SSR environment host, created on the
    /// first SSR request): calls bound their init wait by the ordinary per-call
    /// timeout (`OJ_PLUGIN_TIMEOUT`), so a wedged init cannot freeze the single
    /// watcher thread or browser-facing SSR transforms for the long deadline.
    pub async fn spawn_lazy(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_with_policy(root, plugins_file, config_json, true, SpawnTimeouts::default())
            .await
    }

    /// Test-only lazy spawn with an explicit init wait, so the latch semantics
    /// can be exercised without racing the env-var knobs other tests read.
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
        )
        .await
    }

    /// Test-only spawn with every timeout explicit (see `SpawnTimeouts`).
    #[cfg(test)]
    async fn spawn_with_timeouts(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        lazy: bool,
        timeouts: SpawnTimeouts,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_with_policy(root, plugins_file, config_json, lazy, timeouts).await
    }

    async fn spawn_with_policy(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
        lazy: bool,
        timeouts: SpawnTimeouts,
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
            ws_out: Mutex::new(None),
            server_events: Mutex::new(None),
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
            },
            revive: Mutex::new(ReviveState {
                generation: 0,
                attempts: 0,
                last: None,
                pending_before: std::collections::HashSet::new(),
            }),
            shut_down: std::sync::atomic::AtomicBool::new(false),
            self_ref: std::sync::OnceLock::new(),
        });
        let _ = host.self_ref.set(std::sync::Arc::downgrade(&host));
        Self::ignite(&host, 0).map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(host)
    }

    /// Boot one engine GENERATION onto `host`: write the host module, spawn
    /// the engine with its push/RPC hooks, install it, and start the three
    /// per-generation tasks (boot, push dispatcher, init stall monitor). The
    /// initial spawn and every revive run this same path; each task carries
    /// its generation so a death it reports about a since-replaced engine is
    /// ignored (see `declare_gone`). `generation` is the generation this boot
    /// is FOR: if another revive superseded it before the engine could be
    /// installed, the just-spawned engine is abandoned instead of installed —
    /// two racing ignites must never leave a live loser behind (a zombie
    /// isolate with its own middleware server).
    fn ignite(host: &std::sync::Arc<PluginHost>, generation: u64) -> Result<(), String> {
        let root = host.boot.root.clone();
        let script = PathBuf::from(&host.host_module);
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Written atomically (tmp + rename): several hosts spawn concurrently
        // in one process (boot + lazy SSR + per-environment build hosts), and
        // a plain truncating write could hand a sibling engine's import a
        // half-written module.
        if std::fs::read(&script).ok().as_deref() != Some(PLUGIN_HOST_JS.as_bytes()) {
            let tmp = script.with_extension(format!("tmp-{}.mjs", std::process::id()));
            std::fs::write(&tmp, PLUGIN_HOST_JS).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &script).map_err(|e| e.to_string())?;
        }

        // The engine's push channel replaces the sidecar's control-plane
        // stdout: pushes arrive as values on `post_rx`, so nothing a plugin
        // prints can splice into the protocol and the whole control-token /
        // ACK / re-push machinery has no in-process counterpart.
        let (post_tx, mut post_rx) = tokio::sync::mpsc::unbounded_channel();
        let resolver = std::sync::Arc::new(OjResolver::new(&root));
        let rpc_handler: oj_js::RpcHandler = {
            let resolver = std::sync::Arc::clone(&resolver);
            let root = root.clone();
            std::sync::Arc::new(move |method, args| ctx_rpc(method, args, &resolver, &root))
        };
        let mut engine_config = oj_js::EngineConfig::new(&root);
        engine_config.code_cache_dir = Some(crate::engine_code_cache_dir(&root));
        engine_config.memory_limit_bytes = Some(host.boot.memory_limit_bytes);
        let engine = oj_js::JsEngine::spawn_with_hooks(
            engine_config,
            oj_js::EngineHooks {
                post: post_tx,
                rpc: Some(rpc_handler),
            },
        )
        .map_err(|e| format!("cannot start the embedded plugin host: {e}"))?;
        let engine = std::sync::Arc::new(engine);
        {
            let revive = host.revive.lock().unwrap();
            // A shutdown racing this boot must not gain a live engine it can
            // no longer take (its engine take may have run before this
            // install), and a newer revive must not gain a live loser.
            if revive.generation != generation
                || host.shut_down.load(std::sync::atomic::Ordering::SeqCst)
            {
                drop(revive);
                engine.abandon();
                return Err("superseded by a newer respawn or a shutdown".into());
            }
            *host.engine.lock().unwrap() = Some(std::sync::Arc::clone(&engine));
        }

        // The BOOT task: seed the host's identity (what used to be argv and
        // spawn env) as a global, then trigger the module's top-level init by
        // calling a trivial export. The init call takes no deadline — the
        // Rust-side watches (init gate, stall monitor) own boot patience —
        // and a top-level throw (the old "host process died on boot") fails
        // it, printing the cause and declaring the host gone.
        let boot_ref = std::sync::Arc::clone(host);
        let boot_engine = std::sync::Arc::clone(&engine);
        let boot_seed = host.boot.boot_seed.clone();
        let host_module = host.host_module.clone();
        tokio::spawn(async move {
            let prelude = format!("globalThis.__ojPluginHost = {boot_seed};");
            if let Err(e) = boot_engine
                .eval_with_deadline(oj_js::EvalInput::Source(prelude), None)
                .await
            {
                boot_ref
                    .declare_gone(&format!("plugin host boot prelude failed: {e}"), generation);
                return;
            }
            match boot_engine
                .call_with_deadline(host_module, "ojHostReady", Vec::new(), None)
                .await
            {
                // The `{ ojInit }` push already flipped `initialized`; the
                // reply is only the error path's carrier.
                Ok(_) => {}
                Err(oj_js::EngineError::Closed) => {}
                Err(e) => {
                    boot_ref.declare_gone(
                        &format!("plugin host failed to initialize: {e}"),
                        generation,
                    );
                }
            }
        });

        // The PUSH DISPATCHER: the engine-channel successor of the stdout
        // reader task. Same control pushes, minus the parsing: values arrive
        // whole, hook replies come back on their own call futures, and the
        // reverse ctx-RPC is answered synchronously inside the engine.
        let reader_ref = std::sync::Arc::clone(host);
        tokio::spawn(async move {
            while let Some(msg) = post_rx.recv().await {
                // A push queued by a since-replaced engine is history, not
                // state: a stale `ojInit` landing after a revive reset the
                // watches would open the call gate before the NEW engine even
                // evaluated its module (racing the boot prelude). Drain and
                // drop everything from a superseded generation.
                if reader_ref.revive.lock().unwrap().generation != generation {
                    continue;
                }
                if let Some(info) = msg.get("ojServeInfo") {
                    reader_ref
                        .serve_info_push
                        .send_replace(Some(ServeInfo::from_json(info)));
                    let _ = reader_ref.initialized.send_replace(true);
                    let _ = reader_ref.init_failed.send_replace(false);
                    continue;
                }
                if msg.get("ojInit").is_some() {
                    // The host's unconditional init-complete signal, sent in
                    // BOTH modes: build mode has no ojServeInfo push, so
                    // without this the gate would only release on the first
                    // reply — a hanging first hook would wait out the whole
                    // init deadline blamed on initialization.
                    let _ = reader_ref.initialized.send_replace(true);
                    let _ = reader_ref.init_failed.send_replace(false);
                    continue;
                }
                if msg.get("ojResyncDone").is_some() {
                    // An enqueued worker-environment resync actually ran (the
                    // invalidate queue drained to it); see resync_done.
                    reader_ref.resync_done.send_modify(|c| *c += 1);
                    continue;
                }
                if msg.get("ojInitProgress").is_some() {
                    // A real top-level init milestone: the boot is
                    // progressing, so standing wedge evidence is stale and
                    // the stall monitor re-arms (see init_progress_seen).
                    reader_ref.init_progress_seen.send_modify(|c| *c += 1);
                    let _ = reader_ref.init_failed.send_replace(false);
                    continue;
                }
                if let Some(ev) = msg.get("ojServer") {
                    let tx = reader_ref.server_events.lock().unwrap().clone();
                    if let Some(tx) = tx {
                        let _ = tx.send(ev.clone());
                    }
                    continue;
                }
                if let Some(ws) = msg.get("ojWs") {
                    let tx = reader_ref.ws_out.lock().unwrap().clone();
                    if let Some(tx) = tx {
                        let payload = match ws.get("event").and_then(|e| e.as_str()) {
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
                        };
                        if !payload.is_empty() {
                            let _ = tx.send(payload);
                        }
                    }
                    continue;
                }
            }
            // The push channel closed: the engine thread exited (a clean
            // shutdown, or the unwind after an abandon's terminate). Fail
            // every future call fast instead of letting an init-gated call
            // wait out the whole init deadline on a dead engine; in-flight
            // calls select on this same watch. Generation-guarded: an
            // abandoned engine's thread can exit AFTER a revive replaced it,
            // and its close must not kill the fresh generation.
            let revive = reader_ref.revive.lock().unwrap();
            if revive.generation == generation {
                drop(revive);
                let _ = reader_ref.host_gone.send_replace(true);
            }
        });

        // The init STALL MONITOR: wedge evidence independent of any caller's
        // window. The boot host's per-call init windows equal the whole init
        // deadline, so no call ever burns an RPC-scale window on it — without
        // this, a wedged-but-alive host held evidence-gated waiters (the
        // Start prewarm hold) for the full deadline. The host reports real
        // milestones (`{ ojInitProgress }`) through its top-level init; a
        // full RPC-scale window with NO milestone and init still pending
        // flips `init_failed`, and any progress — a milestone, or init
        // itself — clears it. A healthy slow boot that keeps hitting
        // milestones therefore holds waiters however long it takes, while a
        // host gone silent releases them at the ~RPC scale. Evidence only:
        // calls never consult it (see `call`).
        let monitor_ref = std::sync::Arc::clone(host);
        let stall_wait = host.boot.stall_wait;
        tokio::spawn(async move {
            let mut init_rx = monitor_ref.initialized.subscribe();
            let mut gone_rx = monitor_ref.host_gone.subscribe();
            let mut prog_rx = monitor_ref.init_progress_seen.subscribe();
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
                    let _ = monitor_ref.init_failed.send_replace(true);
                    // The window is spent: re-arm only on new progress (or
                    // exit on init/death) instead of spinning on a past
                    // deadline. The reader clears the evidence on progress.
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
        Ok(())
    }

    /// Whether a dead host may still come back: budget left, spacing not the
    /// question here (a waiter asks "is recovery possible at all"), and never
    /// after an on-purpose `shutdown`.
    pub fn can_revive(&self) -> bool {
        !self.shut_down.load(std::sync::atomic::Ordering::SeqCst)
            && self.revive.lock().unwrap().attempts < PLUGIN_HOST_RESPAWN_LIMIT
    }

    /// Revive a dead host with a fresh engine generation, on demand from the
    /// next call: reset the per-generation watches, bump the generation, and
    /// re-run the same boot `ignite` ran at spawn. Bounded by a lifetime
    /// budget and a minimum spacing (see `ReviveState`); a shutdown host is
    /// never revived. Returns whether the host is now (or already was) live.
    fn try_revive(&self) -> bool {
        if self.shut_down.load(std::sync::atomic::Ordering::SeqCst) {
            return false;
        }
        let Some(host) = self.self_ref.get().and_then(std::sync::Weak::upgrade) else {
            return false;
        };
        let mut revive = self.revive.lock().unwrap();
        if !*self.host_gone.borrow() {
            // A concurrent caller already revived it while we waited on the
            // lock (or the death report was stale): nothing to do.
            return true;
        }
        if revive.attempts >= PLUGIN_HOST_RESPAWN_LIMIT {
            return false;
        }
        if let Some(last) = revive.last {
            if last.elapsed() < PLUGIN_HOST_RESPAWN_SPACING {
                // Too soon after the previous attempt: fail this call fast
                // instead of stacking engines against a recurring wedge.
                return false;
            }
        }
        // A respawn re-loads the app's plugins, and with them the native
        // addons the DEAD generation held. Re-registering an addon whose
        // every runtime is gone runs its init against dangling process-global
        // state — a pre-3.10 napi-rs addon (vite 8.0.16's rolldown pin)
        // crashes the whole dev server on it, which is strictly worse than
        // the dead host this would heal. Refuse for exactly the addons this
        // host's own teardown orphaned (pending now, not pending at its
        // death); an addon another engine still holds live re-registers as
        // the ordinary concurrent case and never blocks, and neither does an
        // orphan this host never loaded. No attempt is consumed, and the
        // spacing clock throttles the re-check and the message.
        let orphaned = oj_js::addons_pending_unsafe_reregistration();
        if let Some(addon) = orphaned.iter().find(|p| !revive.pending_before.contains(*p)) {
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
        eprintln!(
            "oj: respawning the plugin host (attempt {} of {PLUGIN_HOST_RESPAWN_LIMIT})",
            revive.attempts
        );
        // Reset the per-generation state BEFORE the new engine can push:
        // init pending again, evidence cleared, the stale serve info dropped
        // (the old middleware port died with its engine; subscribers activate
        // again on the fresh generation's push).
        let _ = self.initialized.send_replace(false);
        let _ = self.init_failed.send_replace(false);
        self.serve_info_push.send_replace(None);
        *self.spawned.lock().unwrap() = tokio::time::Instant::now();
        let generation = revive.generation;
        drop(revive);
        match Self::ignite(&host, generation) {
            Ok(()) => {
                // Live again: lift the death flag last, so a caller either
                // sees a dead host or a fully re-armed one.
                let _ = self.host_gone.send_replace(false);
                true
            }
            Err(e) => {
                eprintln!("oj: plugin host respawn failed: {e}");
                false
            }
        }
    }

    async fn call(&self, hook: &str, args: &[&str]) -> Result<Option<String>, String> {
        if *self.host_gone.borrow() && !self.try_revive() {
            return Err("plugin host exited".into());
        }
        // The host answers hooks only after its top-level init completes (the
        // entry point runs past every top-level await), so a call during a
        // slow boot must wait for init — bounded by this spawn's init-wait
        // policy (the long spawn-anchored deadline on a boot host, the short
        // per-call window on a lazy one) — instead of racing its own per-call
        // timeout against the boot and permanently snapshotting wrong
        // defaults. Fast boots are untouched: initialized flips with the
        // serve-info push, the ojInit signal, or the first reply, all
        // preceding any wait here.
        //
        // The gate runs BEFORE anything reaches the engine. A wedged host is
        // mid-init on the isolate thread, so a job submitted now would only
        // queue behind the wedge and rot; worse, its failure would be blamed
        // on the hook. A pre-init call therefore submits nothing: it waits on
        // the init watch and either proceeds (init flipped: the module is
        // evaluated and serving) or fails at its window with no job sent.
        //
        // Per-call windows, deliberately with NO time-based fail-fast latch:
        // a previous call's expired window is evidence only of a slow boot,
        // not a wedge, so a later call must still get its own full window —
        // a healthy 30 s init serves a call arriving at 21 s the moment init
        // lands, where a latch would fail it milliseconds short. Time alone
        // never fails a call early: only host death (host_gone) fails fast.
        // A truly wedged host costs each caller one window (degrading like a
        // slow hook) with zero jobs submitted; `init_failed` still records the
        // expired-window evidence — cleared whenever init progresses — for
        // waiters that select on wedge evidence (the Start prewarm hold).
        let mut init_rx = self.initialized.subscribe();
        if !*init_rx.borrow_and_update() {
            let deadline = call_init_deadline(
                self.lazy,
                *self.spawned.lock().unwrap(),
                self.init_wait,
                tokio::time::Instant::now(),
            );
            let mut host_gone_rx = self.host_gone.subscribe();
            // A death flipped between the top-of-call check and this
            // subscribe is already "seen" by the receiver (changed() would
            // never fire for it): consult the value once after subscribing.
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
                            break;
                        }
                    }
                    changed = host_gone_rx.changed() => {
                        if changed.is_err() || *host_gone_rx.borrow() {
                            return Err("plugin host exited".into());
                        }
                    }
                    _ = tokio::time::sleep_until(deadline) => {
                        // A full window elapsed with init still pending:
                        // wedge EVIDENCE for selecting waiters (never a gate
                        // for later calls — see above).
                        let _ = self.init_failed.send_replace(true);
                        return Err(format!(
                            "plugin host still initializing after {}s running {hook} (raise {} for slower boots)",
                            self.init_wait.as_secs(),
                            self.init_knob,
                        ));
                    }
                    _ = progress.tick() => {
                        // One line per interval across concurrent waiters.
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
                }
            }
        }
        // Initialized: the module is evaluated and its hook entry point is
        // callable. The engine call carries the per-call deadline itself
        // (`OJ_PLUGIN_TIMEOUT`), and a deadline failure costs ONE call with
        // the host answering everyone else — exactly as the old host's reply
        // timeout did — whether the hook's promise never settles (abandoned
        // by the scheduler's expiry tick), it parked over budget behind
        // slower work, or it wedged in synchronous JS (interrupted by the
        // call's watchdog; the process host lost the WHOLE host to that
        // shape). The BELT past it is a second full window with NO reply of
        // any kind: everything above answers at the deadline while the
        // scheduler is alive, so total silence means the isolate thread is
        // blocked in NATIVE code (a napi call — the old "host stopped
        // draining its stdin" evidence) — declare the host gone.
        let deadline = tokio::time::Instant::now() + self.rpc_wait;
        // The generation this call runs against, read before the engine so a
        // racing revive makes the pair stale (belt ignored), never mismatched
        // the other way (a stale generation blaming a fresh engine).
        let generation = self.revive.lock().unwrap().generation;
        let engine = self
            .engine
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| "plugin host exited".to_string())?;
        let call = engine.call_with_deadline(
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
        let mut host_gone_rx = self.host_gone.subscribe();
        let result = tokio::select! {
            biased;
            r = &mut call => r,
            _ = async {
                while !*host_gone_rx.borrow_and_update() {
                    if host_gone_rx.changed().await.is_err() {
                        break;
                    }
                }
            } => return Err("plugin host exited".into()),
            _ = tokio::time::sleep_until(deadline + self.rpc_wait) => {
                let msg = format!(
                    "plugin host unresponsive for {}s running {hook} (the engine stopped scheduling)",
                    2 * self.rpc_wait.as_secs()
                );
                self.declare_gone(&msg, generation);
                return Err(msg);
            }
        };
        match result {
            Ok(value) => {
                // Any reply proves the host's top-level init completed: the
                // hook entry point only exists past every top-level await.
                // Generation-guarded: a reply from a since-replaced engine
                // proves the OLD generation's init, and must not open the
                // gate for the new one still booting.
                if self.revive.lock().unwrap().generation == generation {
                    let _ = self.initialized.send_replace(true);
                    let _ = self.init_failed.send_replace(false);
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
                // A near-limit heap is a property of the isolate, not of this
                // call: keeping the engine would keep serving off a heap that
                // can only thrash (and the unwind doubled its cap). Declare
                // it gone; the next call revives a fresh one.
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
                let _ = self.initialized.send_replace(true);
                let _ = self.init_failed.send_replace(false);
                Err(e)
            }
        }
    }

    /// Treat the host as dead NOW (a wedged isolate, a failed boot): abandon
    /// the engine — terminate_execution interrupts a running JS job so a
    /// wedged synchronous hook unwinds and the thread exits on its closed
    /// channel; a job blocked in NATIVE code cannot be interrupted and leaks
    /// the detached thread with its isolate (the documented cost of the
    /// in-process host) — and flip `host_gone` so every in-flight and future
    /// call fails fast, the same state the push dispatcher's channel-closed
    /// path reaches — no longer terminal: the next call may revive the host
    /// with a fresh engine generation (see `try_revive`). `generation` is the
    /// generation of the engine the report is ABOUT: a report outliving a
    /// revive (an old call's belt, an abandoned engine's late exit) is stale
    /// and must not kill the replacement.
    fn declare_gone(&self, why: &str, generation: u64) {
        let mut revive = self.revive.lock().unwrap();
        if revive.generation != generation {
            return;
        }
        // Snapshot the addons ALREADY orphaned before this death, so the
        // revive gate can tell "held by the dead generation" (pending only
        // after its teardown) from "someone else's" (see `pending_before`).
        revive.pending_before = oj_js::addons_pending_unsafe_reregistration()
            .into_iter()
            .collect();
        let rss = process_rss_mb()
            .map(|m| format!(" (process rss {m}MB)"))
            .unwrap_or_default();
        eprintln!("oj: {why}; treating the plugin host as gone{rss}");
        if let Some(engine) = self.engine.lock().unwrap().take() {
            engine.abandon();
        }
        drop(revive);
        let _ = self.host_gone.send_replace(true);
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

    /// The shared init deadline, measured from the host's spawn (this host's
    /// init-wait policy, so a lazily spawned host reports its short bound).
    /// A caller gating separate work on the host's initialization (the Start
    /// prewarm waiting for serve info) anchors to THIS deadline instead of
    /// starting a fresh full period of its own.
    pub fn init_deadline_at(&self) -> tokio::time::Instant {
        *self.spawned.lock().unwrap() + self.init_wait
    }

    /// Live updates of the host-gone flag (the engine exited or was
    /// abandoned). For waiters selecting on wedge evidence without holding the
    /// `Arc<PluginHost>` (the Start prewarm hold).
    pub fn host_gone_updates(&self) -> tokio::sync::watch::Receiver<bool> {
        self.host_gone.subscribe()
    }

    /// Live updates of the init-failure evidence: true while some pre-init
    /// call burned its full init window with init still pending, false again
    /// the moment init progresses. Evidence for waiters gating separate work
    /// on the host's health (the Start prewarm hold selects on it, with
    /// `host_gone_updates` and `init_deadline_at`, instead of a flat timer a
    /// healthy slow boot would trip) — never a per-call gate.
    pub fn init_failure_updates(&self) -> tokio::sync::watch::Receiver<bool> {
        self.init_failed.subscribe()
    }

    /// Live updates of the resync-executed counter (`{ ojResyncDone }` pushes).
    /// A caller enqueueing a resync snapshots the value FIRST, then waits for
    /// it to move past that baseline: the /__oj_invalidate ack only means
    /// "enqueued", and claiming "resynced" off the ack would log success over
    /// a queue that never drained.
    pub fn resync_done_updates(&self) -> tokio::sync::watch::Receiver<u64> {
        self.resync_done.subscribe()
    }

    /// Resolves when the host is gone (its engine exited or was abandoned). Lets a
    /// task holding an `Arc<PluginHost>` — which keeps every channel sender
    /// alive, so `changed().is_err()` can never observe the death — wait on
    /// the host dying instead of pinning it forever.
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
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => {
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
                let chunks = v
                    .get("emittedChunks")
                    .and_then(|c| c.as_array())
                    .map(|a| a.iter().filter_map(ChunkEmit::from_value).collect())
                    .unwrap_or_default();
                Ok((out, str_array("watchFiles"), str_array("maps"), chunks))
            }
            Err(_) => Ok((raw, Vec::new(), Vec::new(), Vec::new())),
        }
    }

    pub async fn seed_chunk_names(&self, map_json: &str) -> Result<Option<String>, String> {
        self.call("seedChunkNames", &[map_json]).await
    }

    #[inline]
    pub async fn has_module_parsed(&self) -> bool {
        matches!(self.call("hasModuleParsed", &[]).await, Ok(Some(s)) if s == "true")
    }

    #[inline]
    pub async fn module_parsed(&self, id: &str) -> Result<(), String> {
        self.call("replayModuleParsed", &[id]).await.map(|_| ())
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

    /// `ctx_json` is Vite's IndexHtmlTransformContext for the page (`path`,
    /// `filename`, and `originalUrl` in dev or `bundle` / `chunk` in a build);
    /// the host adds the dev server. A throwing hook is an `Err`, as in Vite,
    /// where it fails the request or the build.
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
        let chunks = serde_json::from_str::<serde_json::Value>(&raw)
            .ok()
            .and_then(|v| {
                v.get("emittedChunks")
                    .and_then(|c| c.as_array())
                    .map(|a| a.iter().filter_map(ChunkEmit::from_value).collect())
            })
            .unwrap_or_default();
        Ok(chunks)
    }

    /// `buildEnd(error?)`: Rollup passes the error that failed the build, so
    /// plugins see a failed build too (`None` for a successful one).
    #[inline]
    pub async fn build_end(&self, error: Option<&str>) -> Result<(), String> {
        match error {
            Some(e) => self.call("buildEnd", &[e]).await.map(|_| ()),
            None => self.call("buildEnd", &[]).await.map(|_| ()),
        }
    }

    #[inline]
    pub async fn render_start(&self) -> Result<(), String> {
        self.call("renderStart", &[]).await.map(|_| ())
    }

    #[inline]
    pub async fn watch_change(&self, file: &str, event: &str) -> Result<(), String> {
        self.call("watchChange", &[file, event]).await.map(|_| ())
    }

    #[inline]
    pub async fn close_bundle(&self) -> Result<(), String> {
        self.call("closeBundle", &[]).await.map(|_| ())
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
        matches!(self.call("hasGenerateBundle", &[]).await, Ok(Some(s)) if s == "true")
    }

    #[inline]
    pub async fn generate_bundle(
        &self,
        bundle_json: &str,
        is_write: bool,
    ) -> Result<Option<String>, String> {
        self.call(
            "generateBundle",
            &[bundle_json, if is_write { "true" } else { "false" }],
        )
        .await
    }

    #[inline]
    pub async fn has_render_chunk(&self) -> bool {
        matches!(self.call("hasRenderChunk", &[]).await, Ok(Some(s)) if s == "true")
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
        matches!(self.call("hasWriteBundle", &[]).await, Ok(Some(s)) if s == "true")
    }

    #[inline]
    pub async fn write_bundle(&self, bundle_json: &str, is_write: bool) -> Result<(), String> {
        self.call(
            "writeBundle",
            &[bundle_json, if is_write { "true" } else { "false" }],
        )
        .await
        .map(|_| ())
    }

    /// How the host serves requests: the loopback port of its configureServer
    /// middleware stack (when any plugin registered one), and whether it built
    /// real runner-backed Vite DevEnvironments (documents are then served by
    /// the plugin middleware, not the Node SSR runner). The host pushes this
    /// the moment its init completes, and RPCs are init-gated (see `call`), so
    /// a value that already arrived is returned without a round trip and the
    /// push is preferred at any point; only a host that blew the init deadline
    /// yields the default — the caller can then watch `serve_info_updates` for
    /// the late push instead of degrading silently.
    pub async fn serve_info(&self) -> ServeInfo {
        if let Some(info) = *self.serve_info_push.borrow() {
            return info;
        }
        let rpc = self.call("getServeInfo", &[]).await;
        // The push may have landed while the RPC ran (or failed); it is the
        // definitive value.
        if let Some(info) = *self.serve_info_push.borrow() {
            return info;
        }
        let Some(v) = rpc
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        else {
            return ServeInfo::default();
        };
        ServeInfo::from_json(&v)
    }

    /// Subscribe to the host's `{ ojServeInfo }` push: `None` until the host's
    /// top-level init completes, then the definitive `ServeInfo` — however slow
    /// the boot. Lets the caller activate the plugin-middleware path late when
    /// the boot-time `serve_info` timed out.
    pub fn serve_info_updates(&self) -> tokio::sync::watch::Receiver<Option<ServeInfo>> {
        self.serve_info_push.subscribe()
    }

    /// Number of plugins still active after oj filters out the ones it
    /// reimplements natively (the React family). Defaults to 1 on RPC failure so
    /// an uncertain host is kept, never dropped by mistake.
    pub async fn plugin_count(&self) -> usize {
        self.call("getPluginCount", &[])
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
    }

    /// Env mutations made by plugin `config()` hooks in the host's shadowed
    /// environment (e.g.
    /// a plugin flipping a VITE_* flag). Empty on RPC failure.
    /// `define` entries the plugins' `config()` hooks contributed, as
    /// `(key, js expression)` pairs (a string value is the expression itself,
    /// anything else its JSON), so they reach oj's compile the way Vite's merged
    /// `config.define` does.
    pub async fn config_defines(&self) -> Vec<(String, String)> {
        let Ok(Some(raw)) = self.call("getPluginConfig", &[]).await else {
            return Vec::new();
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) else {
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
        self.call("getEnvDelta", &[])
            .await
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    /// Whether any active plugin has a `transform` hook. Defaults to true on RPC
    /// failure so the per-module transform pass is never skipped by mistake.
    pub async fn has_transform(&self) -> bool {
        self.call("getHasTransform", &[])
            .await
            .ok()
            .flatten()
            .map(|s| s == "true")
            .unwrap_or(true)
    }

    /// Whether any active plugin has a `load` hook. Vite runs `load` hooks before
    /// the filesystem read, so a plugin can replace an on-disk file's contents; oj
    /// gates that load-first pass on this so apps with no `load` hook pay nothing.
    /// Defaults to false on RPC failure (the fs read alone is always correct).
    pub async fn has_load(&self) -> bool {
        self.call("getHasLoad", &[])
            .await
            .ok()
            .flatten()
            .map(|s| s == "true")
            .unwrap_or(false)
    }

    /// The `filter.code` include patterns of every object-form transform hook, as
    /// regex source strings. oj gates dependency transforms on these so it only
    /// hands a dep to the transform RPC when a transform's own filter wants it.
    pub async fn dep_transform_filters(&self) -> Vec<String> {
        let Ok(Some(raw)) = self.call("getDepTransformFilters", &[]).await else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
    }

    /// The `filter.id` include patterns of every object-form `load` hook, as regex
    /// source strings. A dependency module is offered to plugin `load` only when
    /// its path matches one, so deps cost no RPC unless a plugin asked for them.
    pub async fn dep_load_filters(&self) -> Vec<String> {
        let Ok(Some(raw)) = self.call("getDepLoadFilters", &[]).await else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
    }

    /// The `filter.id` include patterns of every object-form `resolveId` hook, as
    /// regex source strings. A relative or absolute import matching one is offered
    /// to the plugins' resolveId before oj's own resolver (Vite runs plugin
    /// resolveId first for every id; oj gates the non-bare ones on a declared
    /// filter so unfiltered plugins cost no RPC per import).
    pub async fn resolve_id_filters(&self) -> Vec<String> {
        let Ok(Some(raw)) = self.call("getResolveIdFilters", &[]).await else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
    }

    /// Which HMR hooks any active plugin defines: (watchChange, handleHotUpdate).
    /// Defaults to (true, true) on RPC or parse failure so an HMR RPC is never
    /// skipped by mistake.
    pub async fn hmr_hooks(&self) -> (bool, bool) {
        let raw = match self.call("getHmrHooks", &[]).await {
            Ok(Some(s)) => s,
            _ => return (true, true),
        };
        match serde_json::from_str::<serde_json::Value>(&raw) {
            Ok(v) => (
                v.get("watchChange")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(true),
                v.get("handleHotUpdate")
                    .and_then(|b| b.as_bool())
                    .unwrap_or(true),
            ),
            Err(_) => (true, true),
        }
    }

    /// Retire the host's engine now (used when the host has no active
    /// plugins). Abandon, not drop: dropping a JsEngine joins its thread, and
    /// nobody retiring an idle host should block on V8 teardown — the thread
    /// exits by itself on the closed channel, and the push dispatcher's
    /// channel-closed path then latches `host_gone`.
    pub fn shutdown(&self) {
        // Retired on purpose: never revived. The revive lock orders this
        // against an in-flight ignite — the flag lands either before its
        // install guard runs (the fresh engine is abandoned there) or after
        // the install (the take below reaches that engine) — so a shutdown
        // racing a revive can never leave a live engine behind.
        self.shut_down.store(true, std::sync::atomic::Ordering::SeqCst);
        let _revive = self.revive.lock().unwrap();
        if let Some(engine) = self.engine.lock().unwrap().take() {
            engine.abandon();
        }
    }

    pub fn set_server_events_sender(
        &self,
        tx: tokio::sync::mpsc::UnboundedSender<serde_json::Value>,
    ) {
        *self.server_events.lock().unwrap() = Some(tx);
    }

    pub fn set_ws_sender(&self, tx: tokio::sync::broadcast::Sender<String>) {
        *self.ws_out.lock().unwrap() = Some(tx);
    }

    #[inline]
    pub async fn ws_message(&self, event: &str, data: &str) -> Result<(), String> {
        self.call("wsMessage", &[event, data]).await.map(|_| ())
    }

    /// An HMR client connected: the host fires `server.ws.on("connection")`
    /// listeners (Vite's ws server emits one per accepted socket).
    #[inline]
    pub async fn ws_connection(&self) -> Result<(), String> {
        self.call("wsConnection", &[]).await.map(|_| ())
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
        let Some(json) = self.call("getPluginCss", &[]).await.ok().flatten() else {
            return Vec::new();
        };
        serde_json::from_str::<serde_json::Value>(&json)
            .ok()
            .and_then(|v| {
                v.as_array().map(|a| {
                    a.iter()
                        .filter_map(|e| {
                            let css = e.get("css")?.as_str()?.to_string();
                            let id = e.get("id").and_then(|x| x.as_str()).unwrap_or("").to_string();
                            Some((id, css))
                        })
                        .collect()
                })
            })
            .unwrap_or_default()
    }
}


#[cfg(test)]
mod vite_values_tests {
    use super::*;

    #[test]
    fn finds_commonjs_vite_config_formats() {
        for extension in ["cjs", "cts"] {
            let root = std::env::temp_dir().join(format!(
                "oj-config-format-{}-{extension}",
                std::process::id()
            ));
            std::fs::create_dir_all(&root).unwrap();
            let path = root.join(format!("vite.config.{extension}"));
            std::fs::write(&path, "module.exports = {};").unwrap();
            assert_eq!(vite_config_file(&root), Some(path));
            std::fs::remove_dir_all(&root).unwrap();
        }
    }

    #[test]
    fn plugin_rpc_timeout_defaults_and_reads_env_seconds() {
        assert_eq!(plugin_rpc_timeout_from(None).as_secs(), 20);
        assert_eq!(plugin_rpc_timeout_from(Some("90")).as_secs(), 90);
        assert_eq!(plugin_rpc_timeout_from(Some(" 5 ")).as_secs(), 5);
        // Garbage and zero fall back to the default rather than disabling the guard.
        assert_eq!(plugin_rpc_timeout_from(Some("soon")).as_secs(), 20);
        assert_eq!(plugin_rpc_timeout_from(Some("0")).as_secs(), 20);
    }

    #[test]
    fn plugin_init_timeout_defaults_and_reads_env_seconds() {
        assert_eq!(plugin_init_timeout_from(None).as_secs(), 300);
        assert_eq!(plugin_init_timeout_from(Some("2")).as_secs(), 2);
        assert_eq!(plugin_init_timeout_from(Some("soon")).as_secs(), 300);
        assert_eq!(plugin_init_timeout_from(Some("0")).as_secs(), 300);
    }

    #[test]
    fn extraction_timeout_defaults_and_reads_env_seconds() {
        assert_eq!(extraction_timeout_from(None).as_secs(), 60);
        assert_eq!(extraction_timeout_from(Some("120")).as_secs(), 120);
        assert_eq!(extraction_timeout_from(Some("junk")).as_secs(), 60);
        assert_eq!(extraction_timeout_from(Some("0")).as_secs(), 60);
    }

    // The per-spawn init-wait policy: a boot host waits out the long init
    // deadline (boot correctness depends on its snapshot RPCs), a lazily
    // spawned host (the SSR environment host) only the short per-call bound,
    // so a wedged init cannot freeze the watcher thread for the long deadline.
    #[test]
    fn init_wait_policy_is_long_for_boot_hosts_and_short_for_lazy_ones() {
        let (boot_wait, boot_knob) = init_wait_policy(false);
        assert_eq!(boot_wait, plugin_init_timeout());
        assert_eq!(boot_knob, "OJ_PLUGIN_INIT_TIMEOUT");
        let (lazy_wait, lazy_knob) = init_wait_policy(true);
        assert_eq!(lazy_wait, plugin_rpc_timeout());
        assert_eq!(lazy_knob, "OJ_PLUGIN_TIMEOUT");
    }

    // A lazy host's init gate is per-call: a call arriving AFTER spawn +
    // init_wait (init still pending) gets its own full window from its own
    // start, never the spawn-anchored deadline's zero-length remainder. A boot
    // host keeps the shared spawn-anchored deadline.
    #[test]
    fn lazy_call_past_the_spawn_deadline_gets_its_own_init_window() {
        let wait = std::time::Duration::from_secs(20);
        let spawned = tokio::time::Instant::now();
        // A call 40 s after spawn, with the 20 s window long since elapsed.
        let now = spawned + std::time::Duration::from_secs(40);
        let lazy = call_init_deadline(true, spawned, wait, now);
        assert_eq!(lazy, now + wait, "the lazy window anchors to the call's own start");
        let boot = call_init_deadline(false, spawned, wait, now);
        assert_eq!(boot, spawned + wait, "the boot deadline stays shared and spawn-anchored");
        assert!(boot <= now, "sanity: the boot deadline has elapsed for this call");
    }

    // An oj.config.json that sets one ssr key (noExternal) must not drop the
    // extractor's verdict: the ssr block merges per-key, and `runnerBacked` —
    // which only extraction produces — is always adopted.
    #[test]
    fn merge_fills_ssr_per_key_and_always_adopts_runner_backed() {
        let mut config = oj_config::OjConfig::default();
        config.ssr = Some(serde_json::json!({ "noExternal": true }));
        let v = ViteValues {
            ssr: Some(serde_json::json!({
                "noExternal": ["from-vite"],
                "target": "webworker",
                "runnerBacked": true,
                "resolve": { "conditions": ["workerd"] }
            })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let ssr = config.ssr.as_ref().unwrap();
        assert_eq!(ssr["noExternal"], serde_json::json!(true), "the oj config's key wins");
        assert_eq!(ssr["target"], "webworker", "extractor keys fill where oj lacks them");
        assert_eq!(ssr["resolve"]["conditions"][0], "workerd");
        assert!(oj_config::ssr_runner_backed(&config), "the verdict survives an oj-side ssr key");

        // runnerBacked is always the extractor's, even against a (stale)
        // oj-side value: only extraction produces it.
        let mut config = oj_config::OjConfig::default();
        config.ssr = Some(serde_json::json!({ "runnerBacked": false }));
        let v = ViteValues {
            ssr: Some(serde_json::json!({ "runnerBacked": true })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert!(oj_config::ssr_runner_backed(&config));
    }

    // The ssr merge recurses one level into `resolve`: an oj-side
    // ssr.resolve.externalConditions must not drop the extractor's other
    // resolve sub-keys (the workerd sugar's `conditions` above all).
    #[test]
    fn merge_recurses_one_level_into_ssr_resolve() {
        let mut config = oj_config::OjConfig::default();
        config.ssr = Some(serde_json::json!({ "resolve": { "externalConditions": ["oj-ext"] } }));
        let v = ViteValues {
            ssr: Some(serde_json::json!({
                "runnerBacked": true,
                "resolve": { "conditions": ["workerd"], "externalConditions": ["never-adopted"] }
            })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let ssr = config.ssr.as_ref().unwrap();
        assert_eq!(
            ssr["resolve"]["externalConditions"],
            serde_json::json!(["oj-ext"]),
            "the oj config's sub-key wins"
        );
        assert_eq!(
            ssr["resolve"]["conditions"],
            serde_json::json!(["workerd"]),
            "the extractor's other resolve sub-keys fill in"
        );
        assert!(oj_config::ssr_runner_backed(&config));
    }

    // A non-object oj-side ssr value (or ssr.resolve) cannot be merged
    // per-key: the extractor block is adopted (with a warning) so the
    // "runnerBacked is always adopted" contract holds.
    #[test]
    fn merge_adopts_extractor_ssr_when_the_oj_side_is_not_an_object() {
        let mut config = oj_config::OjConfig::default();
        config.ssr = Some(serde_json::json!("bogus"));
        let v = ViteValues {
            ssr: Some(serde_json::json!({ "runnerBacked": true, "target": "webworker" })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert!(
            oj_config::ssr_runner_backed(&config),
            "the contract holds against a non-object oj-side ssr"
        );
        assert_eq!(config.ssr.as_ref().unwrap()["target"], "webworker");

        // Same one level down: a non-object ssr.resolve adopts the
        // extractor's resolve block instead of silently dropping the sugar.
        let mut config = oj_config::OjConfig::default();
        config.ssr = Some(serde_json::json!({ "resolve": "bogus" }));
        let v = ViteValues {
            ssr: Some(serde_json::json!({
                "runnerBacked": true,
                "resolve": { "conditions": ["workerd"] }
            })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let ssr = config.ssr.as_ref().unwrap();
        assert_eq!(ssr["resolve"]["conditions"], serde_json::json!(["workerd"]));
        assert!(oj_config::ssr_runner_backed(&config));
    }

    // Per-call init windows with NO time-based fail-fast: an earlier call's
    // expired window is slow-boot evidence, not a wedge, so a later pre-init
    // call still waits its OWN full window and is served the moment a healthy
    // (merely slow) init lands. The expired window flips the init-failure
    // EVIDENCE watch for selecting waiters (the Start prewarm hold), and init
    // progressing clears it.
    /// Spawn a healthy lazy host over a trivial plugins file and prove it
    /// serves a call — the shared setup of the revive tests.
    async fn spawn_live_host(tag: &str) -> (PathBuf, std::sync::Arc<PluginHost>) {
        let root = std::env::temp_dir().join(format!("oj-revive-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(&plugins, "export default [];\n").unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();
        let host = PluginHost::spawn_lazy_with_wait(
            &root,
            &plugins,
            &config,
            std::time::Duration::from_secs(30),
        )
        .await
        .expect("the embedded engine spawns");
        host.resolve_id("x", "").await.expect("a live host serves");
        (root, host)
    }

    // A wedge is no longer terminal: the next call revives the host with a
    // fresh engine generation through the same boot path, and serves.
    #[tokio::test]
    async fn a_gone_host_is_revived_by_the_next_call() {
        let (root, host) = spawn_live_host("basic").await;
        let generation = host.revive.lock().unwrap().generation;
        host.declare_gone("test wedge", generation);
        assert!(*host.host_gone.borrow(), "the death latched");

        host.resolve_id("x", "")
            .await
            .expect("the next call revives the host and serves");
        let revive = host.revive.lock().unwrap();
        assert_eq!(revive.generation, generation + 1, "a fresh engine generation");
        assert_eq!(revive.attempts, 1, "one respawn consumed");
        drop(revive);
        assert!(!*host.host_gone.borrow(), "the host is live again");
        let _ = std::fs::remove_dir_all(&root);
    }

    // A death report about a replaced engine (an old call's transport belt
    // firing after a revive) is stale and must not kill the new generation.
    #[tokio::test]
    async fn a_stale_death_report_does_not_kill_a_revived_host() {
        let (root, host) = spawn_live_host("stale").await;
        let generation = host.revive.lock().unwrap().generation;
        host.declare_gone("test wedge", generation);
        host.resolve_id("x", "").await.expect("revived");

        host.declare_gone("stale report about the old engine", generation);
        assert!(
            !*host.host_gone.borrow(),
            "a stale-generation report is ignored"
        );
        host.resolve_id("x", "").await.expect("still serving");
        let _ = std::fs::remove_dir_all(&root);
    }

    // shutdown() retires the host on purpose: never revived.
    #[tokio::test]
    async fn a_shutdown_host_is_never_revived() {
        let (root, host) = spawn_live_host("shutdown").await;
        host.shutdown();
        host.host_gone_wait().await;
        let err = host.resolve_id("x", "").await.expect_err("stays dead");
        assert!(err.contains("plugin host exited"), "{err}");
        assert_eq!(host.revive.lock().unwrap().attempts, 0, "no respawn burned");
        let _ = std::fs::remove_dir_all(&root);
    }

    // The engine's heap cap mirrors the cap the process host inherited as a
    // node child: a deployment's NODE_OPTIONS --max-old-space-size (last
    // occurrence wins, underscore spelling accepted, like Node), with
    // OJ_PLUGIN_MEMORY_MB above it and 4096 beneath.
    #[test]
    fn node_options_heap_cap_parses_like_node() {
        assert_eq!(max_old_space_mb("--max-old-space-size=8192"), Some(8192));
        assert_eq!(
            max_old_space_mb("--dns-result-order=ipv4first --max-old-space-size=3072 --expose-gc"),
            Some(3072)
        );
        assert_eq!(max_old_space_mb("--max_old_space_size=2048"), Some(2048));
        assert_eq!(
            max_old_space_mb("--max-old-space-size=1024 --max-old-space-size=512"),
            Some(512)
        );
        assert_eq!(max_old_space_mb("--max-old-space-size=zero"), None);
        assert_eq!(max_old_space_mb("--max-semi-space-size=64"), None);
        assert_eq!(max_old_space_mb(""), None);
    }

    // A heap blow-up is a property of the isolate, not of one call: the
    // near-limit callback fails the running hook with MemoryLimit, the host
    // is declared gone (instead of serving on from a heap that can only
    // thrash, with a doubled cap), and the next call revives it on a fresh
    // heap. This is the graceful version of the process host's Node OOM
    // crash + supervisor restart.
    #[tokio::test]
    async fn a_memory_blowup_declares_the_host_gone_and_the_next_call_revives_it() {
        let root = std::env::temp_dir().join(format!("oj-revive-oom-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(
            &plugins,
            r#"const hog = [];
export default [{
  name: "heap-hog",
  load(id) {
    if (id.includes("__hog__")) {
      for (;;) hog.push(new Array(1024 * 1024).fill(Math.random()));
    }
    return null;
  },
}];
"#,
        )
        .unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();
        let host = PluginHost::spawn_with_timeouts(
            &root,
            &plugins,
            &config,
            true,
            SpawnTimeouts {
                init_wait: Some(std::time::Duration::from_secs(30)),
                rpc: Some(std::time::Duration::from_secs(20)),
                memory: Some(128 * 1024 * 1024),
                ..Default::default()
            },
        )
        .await
        .expect("the embedded engine spawns");
        host.load("warmup").await.expect("healthy host answers");

        let err = host
            .load("__hog__")
            .await
            .expect_err("the heap cap fails the allocating hook");
        assert!(
            err.contains("memory limit") && err.contains("OJ_PLUGIN_MEMORY_MB"),
            "the failure names the cap and its knob: {err}"
        );
        assert!(*host.host_gone.borrow(), "a blown heap retires the engine");

        host.load("after")
            .await
            .expect("the next call revives the host on a fresh heap");
        assert_eq!(host.revive.lock().unwrap().attempts, 1, "one respawn consumed");
        let _ = std::fs::remove_dir_all(&root);
    }

    // The budget is a LIFETIME cap: past it the host stays gone (the outer
    // supervisor owns recovery), and attempts are spaced so a burst of calls
    // against a recurring wedge cannot stack engines.
    #[tokio::test]
    async fn the_respawn_budget_is_finite_and_spaced() {
        let (root, host) = spawn_live_host("budget").await;
        for round in 0..PLUGIN_HOST_RESPAWN_LIMIT {
            let generation = host.revive.lock().unwrap().generation;
            host.declare_gone("recurring test wedge", generation);
            // Immediately after a previous revive the spacing rejects the
            // attempt; backdate the clock instead of sleeping it out.
            if round > 0 {
                let err = host.resolve_id("x", "").await.expect_err("spacing rejects");
                assert!(err.contains("plugin host exited"), "{err}");
                host.revive.lock().unwrap().last =
                    Some(std::time::Instant::now() - PLUGIN_HOST_RESPAWN_SPACING);
            }
            host.resolve_id("x", "").await.expect("revives within budget");
        }
        let generation = host.revive.lock().unwrap().generation;
        host.declare_gone("one wedge too many", generation);
        host.revive.lock().unwrap().last =
            Some(std::time::Instant::now() - PLUGIN_HOST_RESPAWN_SPACING);
        let err = host.resolve_id("x", "").await.expect_err("budget spent");
        assert!(err.contains("plugin host exited"), "{err}");
        assert!(!host.can_revive(), "no revive left for waiters to hold on");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn pre_init_calls_keep_their_own_window_after_an_earlier_one_expired() {
        let root = std::env::temp_dir().join(format!("oj-lazy-window-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // A plugins file whose top-level init is slow but healthy.
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(
            &plugins,
            "await new Promise((r) => setTimeout(r, 2500));\nexport default [];\n",
        )
        .unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();
        let host = match PluginHost::spawn_lazy_with_wait(
            &root,
            &plugins,
            &config,
            std::time::Duration::from_secs(1),
        )
        .await
        {
            Ok(h) => h,
            Err(e) => panic!("the embedded engine spawns: {e}"),
        };
        let mut evidence = host.init_failure_updates();
        assert!(!*evidence.borrow_and_update(), "no evidence before a window expires");

        // First call: waits its full per-call window (init is live), then
        // fails on the window — flipping the evidence watch.
        let t0 = std::time::Instant::now();
        let first = host.resolve_id("x", "").await;
        let first_err = first.expect_err("init outlives the first call's window");
        assert!(first_err.contains("still initializing"), "{first_err}");
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(900),
            "the first call waits its full window, got {:?}",
            t0.elapsed()
        );
        assert!(*evidence.borrow_and_update(), "the expired window is wedge evidence");

        // Later calls: each keeps its OWN full window (never the removed
        // fail-fast), so one of them is served the moment init lands. Every
        // failure on the way is a full-window "still initializing", and a
        // failing call burned at least most of a window rather than failing
        // in milliseconds off a latch.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let t = std::time::Instant::now();
            match host.resolve_id("x", "").await {
                Ok(_) => break,
                Err(e) => {
                    assert!(e.contains("still initializing"), "never a latched fail-fast: {e}");
                    assert!(
                        t.elapsed() >= std::time::Duration::from_millis(900),
                        "a pre-init call after an expired window still gets its own window, got {:?}",
                        t.elapsed()
                    );
                    assert!(
                        std::time::Instant::now() < deadline,
                        "a late init never served a waiting call: {e}"
                    );
                }
            }
        }
        assert!(
            !*evidence.borrow_and_update(),
            "init progressing clears the wedge evidence"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // The submit-before-gate hazard, pinned: a wedged init holds the isolate
    // thread, so a job submitted pre-init would only queue behind the wedge
    // and rot — every later call serialized behind it. Pre-init calls must
    // submit NOTHING: even with huge arguments, concurrent calls each fail at
    // their own window ("still initializing"), proving no call sat queued on
    // the wedged engine.
    #[tokio::test]
    async fn wedged_host_pre_init_calls_fail_at_their_window_without_submitting_jobs() {
        let root = std::env::temp_dir().join(format!("oj-wedged-stdin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Init never completes: the host module's top level never settles.
        // (The interval keeps the event loop alive — the same shape that
        // wedged the old node child.)
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(
            &plugins,
            "setInterval(() => {}, 1000);\nawait new Promise(() => {});\nexport default [];\n",
        )
        .unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();
        let host = match PluginHost::spawn_lazy_with_wait(
            &root,
            &plugins,
            &config,
            std::time::Duration::from_secs(1),
        )
        .await
        {
            Ok(h) => h,
            Err(e) => panic!("the embedded engine spawns: {e}"),
        };
        // Far past any OS pipe buffer, the old transport's wedge trigger: the
        // write-first path would have blocked here instead of gating on init.
        let big = "x".repeat(2 * 1024 * 1024);
        let t0 = std::time::Instant::now();
        let (a, b) = tokio::join!(host.resolve_id(&big, ""), host.resolve_id(&big, ""));
        for res in [a, b] {
            let err = res.expect_err("a wedged host fails pre-init calls at their window");
            assert!(err.contains("still initializing"), "{err}");
        }
        let elapsed = t0.elapsed();
        assert!(
            elapsed >= std::time::Duration::from_millis(900),
            "each call waits its window, got {elapsed:?}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "concurrent windows, not calls serialized behind a wedged engine: {elapsed:?}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // The stall monitor is the REAL evidence flip site for the boot host: its
    // per-call init windows equal the whole init deadline, so no call ever
    // burns an RPC-scale window on it. A wedged host (milestones stop, init
    // never lands) must flip the init-failure evidence at the stall window —
    // with NO call in flight — and a merely slow host must flip it and then
    // have init clear it, so evidence-gated waiters (the Start prewarm hold)
    // release on wedges at the ~RPC scale while healthy boots re-hold.
    #[tokio::test]
    async fn boot_host_stall_monitor_flips_evidence_without_a_call_and_init_clears_it() {
        let root = std::env::temp_dir().join(format!("oj-boot-stall-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();

        // A host that wedges forever during its plugins-module evaluation:
        // after the "start" milestone, no progress ever again.
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(
            &plugins,
            "setInterval(() => {}, 1000);\nawait new Promise(() => {});\nexport default [];\n",
        )
        .unwrap();
        let host = match PluginHost::spawn_with_timeouts(
            &root,
            &plugins,
            &config,
            false,
            SpawnTimeouts {
                init_wait: Some(std::time::Duration::from_secs(120)),
                stall: Some(std::time::Duration::from_secs(1)),
                ..Default::default()
            },
        )
        .await
        {
            Ok(h) => h,
            Err(e) => panic!("the embedded engine spawns: {e}"),
        };
        let mut evidence = host.init_failure_updates();
        let flipped = tokio::time::timeout(
            std::time::Duration::from_secs(20),
            evidence.wait_for(|v| *v),
        )
        .await;
        assert!(
            flipped.is_ok() && flipped.unwrap().is_ok(),
            "the stall monitor flips the evidence at the ~RPC scale, no call needed"
        );
        assert!(!host.is_initialized(), "the wedge never initialized");
        host.shutdown();

        // A merely SLOW boot: the stall flips the evidence (its one silent
        // stage outlives the window), then init lands and clears it.
        std::fs::write(
            &plugins,
            "await new Promise((r) => setTimeout(r, 2000));\nexport default [];\n",
        )
        .unwrap();
        let host = match PluginHost::spawn_with_timeouts(
            &root,
            &plugins,
            &config,
            false,
            SpawnTimeouts {
                init_wait: Some(std::time::Duration::from_secs(120)),
                stall: Some(std::time::Duration::from_millis(500)),
                ..Default::default()
            },
        )
        .await
        {
            Ok(h) => h,
            Err(e) => panic!("the embedded engine spawns: {e}"),
        };
        let mut evidence = host.init_failure_updates();
        assert!(tokio::time::timeout(
            std::time::Duration::from_secs(20),
            evidence.wait_for(|v| *v),
        )
        .await
        .is_ok_and(|r| r.is_ok()));
        // Init progressing clears the evidence (a milestone or init itself).
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(20),
                evidence.wait_for(|v| !*v),
            )
            .await
            .is_ok_and(|r| r.is_ok()),
            "init progress clears stall evidence"
        );
        let mut init = host.initialized_updates();
        assert!(tokio::time::timeout(
            std::time::Duration::from_secs(20),
            init.wait_for(|v| *v),
        )
        .await
        .is_ok_and(|r| r.is_ok()));
        host.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    // A hook that wedges the isolate in SYNCHRONOUS JS (an infinite loop —
    // the shape that used to block the node host's event loop until its
    // stdin filled and the whole host was declared gone) is now interrupted
    // by the per-call watchdog at the deadline: it fails ONE call and the
    // host survives — strictly better than the process host, where this
    // wedge cost the whole host.
    #[tokio::test]
    async fn synchronously_wedged_hook_is_terminated_at_its_deadline_and_the_host_survives() {
        let root = std::env::temp_dir().join(format!("oj-wedged-sync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(
            &plugins,
            r#"export default [{
  name: "blocker",
  load(id) {
    if (id.includes("__block__")) {
      for (;;) {}
    }
    return null;
  },
}];
"#,
        )
        .unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();
        let host = PluginHost::spawn_with_timeouts(
            &root,
            &plugins,
            &config,
            true,
            SpawnTimeouts {
                init_wait: Some(std::time::Duration::from_secs(30)),
                rpc: Some(std::time::Duration::from_secs(1)),
                ..Default::default()
            },
        )
        .await
        .expect("the embedded engine spawns");
        // Prove init landed (post-init transport is what is under test).
        host.load("warmup").await.expect("healthy host answers");

        let t0 = std::time::Instant::now();
        let err = host
            .load("__block__")
            .await
            .expect_err("a synchronous wedge fails at its own deadline");
        assert!(
            err.contains("timed out") && err.contains("OJ_PLUGIN_TIMEOUT"),
            "one call fails on its timeout, the host is kept: {err}"
        );
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(10),
            "bounded: {:?}",
            t0.elapsed()
        );
        // The wedge was terminated, not the host: later calls succeed.
        host.load("after")
            .await
            .expect("the host survives a terminated synchronous wedge");
        host.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    // The transport belt: a hook that blocks the isolate thread in NATIVE
    // code (a blocking child wait here — the same category as a wedged napi
    // call; note `Atomics.wait` does NOT qualify, V8's terminate interrupts
    // it) cannot be interrupted by the watchdog and stops the engine's
    // scheduler entirely, so no reply of any kind — not even the deadline
    // expiry — can land. The belt (a second full window past the per-call
    // deadline) declares the host GONE — and the NEXT call revives it on a
    // fresh engine generation, while calls landing inside the respawn
    // spacing still fail fast instead of each burning a window on a wedged
    // engine. The blocked thread itself leaks (detached) until the block
    // ends: the documented cost of the in-process host.
    #[tokio::test]
    async fn natively_blocked_hook_declares_the_host_gone_and_the_next_call_revives_it() {
        let root = std::env::temp_dir().join(format!("oj-wedged-native-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(
            &plugins,
            r#"import { execSync } from "node:child_process";
export default [{
  name: "native-blocker",
  load(id) {
    if (id.includes("__block__")) {
      execSync("sleep 60");
    }
    return null;
  },
}];
"#,
        )
        .unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();
        let host = PluginHost::spawn_with_timeouts(
            &root,
            &plugins,
            &config,
            true,
            SpawnTimeouts {
                init_wait: Some(std::time::Duration::from_secs(30)),
                rpc: Some(std::time::Duration::from_secs(1)),
                ..Default::default()
            },
        )
        .await
        .expect("the embedded engine spawns");
        host.load("warmup").await.expect("healthy host answers");

        let t0 = std::time::Instant::now();
        let err = host
            .load("__block__")
            .await
            .expect_err("a native block must fail at the belt");
        assert!(
            err.contains("unresponsive") || err.contains("exited"),
            "the belt names the wedge: {err}"
        );
        assert!(
            t0.elapsed() < std::time::Duration::from_secs(10),
            "bounded, not a blocked transport: {:?}",
            t0.elapsed()
        );
        // The host is gone — but not terminally: the next call revives it on
        // a fresh engine generation (the wedge was per-id) and serves.
        host.load("after")
            .await
            .expect("the next call revives the host and serves");
        assert_eq!(host.revive.lock().unwrap().attempts, 1, "one respawn consumed");
        // A second death inside the respawn spacing fails fast without a
        // window: attempts are spaced so a burst of calls against a
        // recurring wedge cannot stack engines.
        let generation = host.revive.lock().unwrap().generation;
        host.declare_gone("second test wedge", generation);
        let t1 = std::time::Instant::now();
        let err = host
            .load("again")
            .await
            .expect_err("inside the spacing the host stays gone");
        assert!(err.contains("exited"), "{err}");
        assert!(
            t1.elapsed() < std::time::Duration::from_millis(500),
            "fail-fast on a declared-gone host: {:?}",
            t1.elapsed()
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // The other half of the belt distinction: a hook that merely never
    // SETTLES (a hung promise — the isolate itself stays healthy) fails only
    // that one call, at the per-call deadline, and the host keeps serving
    // everyone else — the old "reply timed out, host kept" semantics. Only
    // total scheduler silence (the test above) declares the host gone.
    #[tokio::test]
    async fn hung_hook_promise_fails_only_that_call() {
        let root = std::env::temp_dir().join(format!("oj-hung-hook-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(
            &plugins,
            r#"export default [{
  name: "hanger",
  load(id) {
    if (id.includes("__hang__")) return new Promise(() => {});
    return null;
  },
}];
"#,
        )
        .unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();
        let host = PluginHost::spawn_with_timeouts(
            &root,
            &plugins,
            &config,
            true,
            SpawnTimeouts {
                init_wait: Some(std::time::Duration::from_secs(30)),
                rpc: Some(std::time::Duration::from_secs(1)),
                ..Default::default()
            },
        )
        .await
        .expect("the embedded engine spawns");
        host.load("warmup").await.expect("healthy host answers");

        // A concurrent healthy call proves the hang costs nobody else their
        // window while the hung one waits out its own deadline.
        let racer = std::sync::Arc::clone(&host);
        let healthy = tokio::spawn(async move { racer.load("alongside").await });
        let t0 = std::time::Instant::now();
        let err = host
            .load("__hang__")
            .await
            .expect_err("a never-settling hook fails at its own deadline");
        assert!(
            err.contains("timed out") && err.contains("OJ_PLUGIN_TIMEOUT"),
            "names the per-call timeout, not a wedge: {err}"
        );
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(900)
                && t0.elapsed() < std::time::Duration::from_secs(2),
            "fails at the deadline, before the belt: {:?}",
            t0.elapsed()
        );
        healthy
            .await
            .unwrap()
            .expect("a concurrent call is untouched by the hang");

        // The host was NOT declared gone: later calls succeed.
        host.load("after")
            .await
            .expect("the host survives an abandoned hook promise");
        host.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    // Process isolation: the in-process host shares oj's process, so a
    // plugin's env writes land in a private shadow (visible to the plugins
    // and to getEnvDelta) and NEVER in oj's real environment. The host's cwd
    // IS the app root (a real chdir at boot, like the old spawn's
    // current_dir — relative fs paths in hooks depend on it), and a plugin's
    // own process.chdir is contained to the shadow afterwards.
    #[tokio::test]
    async fn plugin_env_writes_and_cwd_stay_inside_the_host_shadow() {
        let root = std::env::temp_dir().join(format!("oj-env-shadow-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(
            &plugins,
            "process.env.OJ_HOST_CWD_BEFORE = process.cwd();\n\
             process.chdir(\"/\");\n\
             process.env.OJ_HOST_CWD_AFTER = process.cwd();\n\
             export default [];\n",
        )
        .unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();
        let host = PluginHost::spawn_lazy(&root, &plugins, &config)
            .await
            .expect("the embedded engine spawns");
        let delta = host.env_delta().await;
        let before = delta
            .get("OJ_HOST_CWD_BEFORE")
            .expect("the write is visible in the host's own delta");
        let canonical_root = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
        assert!(
            *before == canonical_root.display().to_string()
                || *before == root.display().to_string(),
            "the host's cwd is the app root: {before}"
        );
        assert_eq!(
            delta.get("OJ_HOST_CWD_AFTER").map(String::as_str),
            Some("/"),
            "a plugin's chdir moves the host's SHADOW cwd"
        );
        assert!(
            std::env::var("OJ_HOST_CWD_BEFORE").is_err() && std::env::var("OJ_HOST_CWD_AFTER").is_err(),
            "a plugin's env write must never reach oj's real environment"
        );
        assert_ne!(
            std::env::current_dir().unwrap(),
            std::path::PathBuf::from("/"),
            "a plugin's chdir must not move oj's real cwd"
        );
        host.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    // Vite's ordering guarantee: buildStart completes before any serving hook
    // runs, so an object-form plugin that computes closure state in
    // buildStart() and reads it in load() (the i18n-barrel shape) never sees
    // a load first. The host is spawned lazily and NEVER told to buildStart —
    // the gate at the hook entry must run it — and the first loads arrive
    // concurrently, which is exactly the race the in-process host had: an
    // engine call job reaching load() while buildStart had not settled read
    // `plan` as undefined and 500ed the module.
    #[tokio::test]
    async fn build_start_settles_before_any_load_even_under_concurrent_first_calls() {
        let root = std::env::temp_dir().join(format!("oj-buildstart-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(
            &plugins,
            r#"let plan;
export default [{
  name: "closure-state",
  buildStart() {
    // Slow on purpose: an ungated load lands well inside this window.
    return new Promise((resolve) => setTimeout(() => {
      plan = { groups: "compiled-groups" };
      resolve();
    }, 300));
  },
  load(id) {
    if (id === "\0closure-barrel") return `export default ${JSON.stringify(plan.groups)};`;
    return null;
  },
}];
"#,
        )
        .unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();
        let host = PluginHost::spawn_lazy(&root, &plugins, &config)
            .await
            .expect("the embedded engine spawns");
        let (a, b, c, d) = tokio::join!(
            host.load("\u{0}closure-barrel"),
            host.load("\u{0}closure-barrel"),
            host.load("\u{0}closure-barrel"),
            host.load("\u{0}closure-barrel"),
        );
        for (i, r) in [a, b, c, d].into_iter().enumerate() {
            let code = r
                .unwrap_or_else(|e| panic!("first load #{i} must not race buildStart: {e}"))
                .expect("the plugin claims the id");
            assert!(
                code.contains("compiled-groups"),
                "load #{i} must serve the buildStart-computed state: {code}"
            );
        }
        host.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    // The push channel end to end: a configureServer middleware makes the
    // host bring up its loopback middleware server and push { ojServeInfo }
    // with the port; a server.ws.send lands on the ws broadcast; a
    // server.restart() lands on the server-events channel. All of it arrives
    // as engine-channel values with no framing in between.
    #[tokio::test]
    async fn push_channel_delivers_serve_info_ws_and_server_events() {
        let root = std::env::temp_dir().join(format!("oj-push-dispatch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let plugins = root.join("oj.plugins.mjs");
        std::fs::write(
            &plugins,
            r#"export default [{
  name: "pusher",
  configureServer(server) {
    server.middlewares.use((req, res, next) => next());
    server.ws.send("oj:probe", { n: 7 });
    server.restart();
  },
}];
"#,
        )
        .unwrap();
        let config = serde_json::json!({
            "config": { "root": root.display().to_string() },
            "env": { "command": "serve", "mode": "development" },
        })
        .to_string();
        let host = PluginHost::spawn_lazy(&root, &plugins, &config)
            .await
            .expect("the embedded engine spawns");
        let (ws_tx, mut ws_rx) = tokio::sync::broadcast::channel(16);
        host.set_ws_sender(ws_tx);
        let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel();
        host.set_server_events_sender(ev_tx);

        let mut serve_info = host.serve_info_updates();
        let pushed = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            serve_info.wait_for(Option::is_some),
        )
        .await
        .expect("the serve-info push arrives")
        .expect("watch alive");
        let info = pushed.expect("serve info present");
        assert!(
            info.middleware_port.is_some(),
            "a registered middleware reports its loopback port"
        );
        drop(pushed);

        // The senders were installed before init began, and configureServer
        // (where the plugin pushed both) runs before the serve-info push that
        // released the wait above — so both deliveries are already in.
        let payload = tokio::time::timeout(std::time::Duration::from_secs(10), ws_rx.recv())
            .await
            .expect("the ws push arrives")
            .expect("broadcast alive");
        assert!(payload.contains("oj:probe") && payload.contains("custom"), "{payload}");
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), ev_rx.recv())
            .await
            .expect("the server event arrives")
            .expect("channel alive");
        assert_eq!(ev.get("action").and_then(|a| a.as_str()), Some("restart"));
        host.shutdown();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn extraction_deps_truncated_gates_only_on_the_flag() {
        let t = serde_json::json!({ "__ok": true, "__depsTruncated": true });
        assert!(extraction_deps_truncated(&t));
        assert!(!extraction_deps_truncated(&serde_json::json!({ "__ok": true })));
        assert!(!extraction_deps_truncated(
            &serde_json::json!({ "__depsTruncated": "yes" })
        ));
    }

    #[test]
    fn extraction_env_hash_tracks_vite_vars_and_node_env_only() {
        let base = || {
            vec![
                ("PATH".to_string(), "/bin".to_string()),
                ("VITE_API".to_string(), "a".to_string()),
                ("NODE_ENV".to_string(), "development".to_string()),
            ]
        };
        let h0 = extraction_env_hash(base().into_iter());
        // Order-independent.
        let mut rev = base();
        rev.reverse();
        assert_eq!(h0, extraction_env_hash(rev.into_iter()));
        // Unrelated variables do not churn the key.
        let mut plus = base();
        plus.push(("TERM".to_string(), "xterm".to_string()));
        assert_eq!(h0, extraction_env_hash(plus.into_iter()));
        // A VITE_* or NODE_ENV change does.
        let mut vite = base();
        vite[1].1 = "b".to_string();
        assert_ne!(h0, extraction_env_hash(vite.into_iter()));
        let mut node = base();
        node[2].1 = "production".to_string();
        assert_ne!(h0, extraction_env_hash(node.into_iter()));
    }

    // Vite (constants.ts DEFAULT_CONFIG_FILES): js, mjs, ts, cjs, mts, cts; with
    // both a .ts and a .js present, Vite loads the .js.
    #[test]
    fn config_discovery_precedence_matches_vite() {
        let root = std::env::temp_dir().join(format!("oj-config-precedence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let order = ["js", "mjs", "ts", "cjs", "mts", "cts"];
        for ext in order.iter().rev() {
            std::fs::write(root.join(format!("vite.config.{ext}")), "export default {};").unwrap();
        }
        for ext in order {
            assert_eq!(
                vite_config_file(&root),
                Some(root.join(format!("vite.config.{ext}"))),
                "with every later format present, .{ext} wins"
            );
            std::fs::remove_file(root.join(format!("vite.config.{ext}"))).unwrap();
        }
        assert_eq!(vite_config_file(&root), None);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn parse_reads_all_fields() {
        let json = serde_json::json!({
            "base": "/app/",
            "publicDir": "/abs/shared/public",
            "port": 3010,
            "host": "0.0.0.0",
            "define": { "__X__": "1" },
            "alias": { "@": "/src" },
            "headers": { "x-a": "b" }
        });
        let v = parse_vite_values(&json);
        assert_eq!(v.base.as_deref(), Some("/app/"));
        assert_eq!(v.public_dir, Some("/abs/shared/public".into()));
        assert_eq!(v.port, Some(3010));
        assert_eq!(v.host.as_deref(), Some("0.0.0.0"));
        assert!(v.define.unwrap().contains_key("__X__"));
        assert!(v.alias.unwrap().contains_key("@"));
        assert!(v.headers.unwrap().contains_key("x-a"));
    }

    #[test]
    fn parse_tolerates_nulls_and_missing() {
        let v = parse_vite_values(&serde_json::json!({ "base": null, "port": null }));
        assert!(v.base.is_none());
        assert!(v.public_dir.is_none());
        assert!(v.port.is_none());
        assert!(v.define.is_none());
    }

    #[test]
    fn merge_adopts_only_unset_fields() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            base: Some("/vite-base/".into()),
            public_dir: Some("shared/public".into()),
            port: Some(3010),
            host: Some("localhost".into()),
            hmr_disabled: false,
            fs_allow: None,
            fs_strict: None,
            define: None,
            alias: None,
            headers: None,
            rollup_options: None,
            assets_inline_limit: None,
            proxy: None,
            dedupe: None,
            optimize_deps: None,
            build: None,
            oxc: None,
            esbuild: None,
            ssr: None,
            mode: None,
            resolve: None,
            raw_resolve: None,
            server_flags: None,
            css: None,
            env_prefix: None,
            env_dir: None,
            cors: None,
            allowed_hosts: None,
            preview: None,
            app_type: None,
            html: None,
        };
        merge_vite_values(&mut config, v);
        assert_eq!(config.base.as_deref(), Some("/vite-base/"));
        assert_eq!(config.public_dir, Some("shared/public".into()));
        assert_eq!(config.server.unwrap().port, Some(3010));
    }

    #[test]
    fn merge_never_overrides_config() {
        let mut config = oj_config::OjConfig::default();
        config.base = Some("/oj-base/".into());
        config.public_dir = Some("my-public".into());
        let v = ViteValues {
            base: Some("/vite-base/".into()),
            public_dir: Some("shared/public".into()),
            port: None,
            host: None,
            hmr_disabled: false,
            fs_allow: None,
            fs_strict: None,
            define: None,
            alias: None,
            headers: None,
            rollup_options: None,
            assets_inline_limit: None,
            proxy: None,
            dedupe: None,
            optimize_deps: None,
            build: None,
            oxc: None,
            esbuild: None,
            ssr: None,
            mode: None,
            resolve: None,
            raw_resolve: None,
            server_flags: None,
            css: None,
            env_prefix: None,
            env_dir: None,
            cors: None,
            allowed_hosts: None,
            preview: None,
            app_type: None,
            html: None,
        };
        merge_vite_values(&mut config, v);
        assert_eq!(config.base.as_deref(), Some("/oj-base/"));
        assert_eq!(config.public_dir, Some("my-public".into()));
    }

    #[test]
    fn merge_adopts_server_fs_strict() {
        // `server.fs.strict: false` in a vite config reaches oj's FsConfig (Vite
        // skips the allow check entirely when strict is off) even with no allow list.
        let v = parse_vite_values(&serde_json::json!({ "fsStrict": false }));
        assert_eq!(v.fs_strict, Some(false));
        let mut config = oj_config::OjConfig::default();
        merge_vite_values(&mut config, v);
        let fs = config.server.unwrap().fs.unwrap();
        assert_eq!(fs.strict, Some(false));
        assert!(fs.allow.is_none());

        // Alongside an allow list both land; an oj-side fs config still wins.
        let v = parse_vite_values(&serde_json::json!({ "fsStrict": true, "fsAllow": ["../shared"] }));
        let mut config = oj_config::OjConfig::default();
        merge_vite_values(&mut config, v);
        let fs = config.server.unwrap().fs.unwrap();
        assert_eq!(fs.strict, Some(true));
        assert_eq!(fs.allow.as_deref(), Some(&["../shared".to_string()][..]));
        let absent = parse_vite_values(&serde_json::json!({}));
        assert_eq!(absent.fs_strict, None);
    }

    #[test]
    fn merge_adopts_proxy() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            proxy: Some(serde_json::json!({
                "/api": "http://localhost:3000",
                "/ws": { "target": "http://localhost:4000", "changeOrigin": true }
            })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let proxy = config.server.unwrap().proxy.unwrap();
        assert_eq!(proxy.get("/api").unwrap().target(), "http://localhost:3000");
        assert_eq!(proxy.get("/ws").unwrap().target(), "http://localhost:4000");
        assert!(proxy.get("/ws").unwrap().change_origin());
    }

    #[test]
    fn merge_adopts_rollup_options() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            rollup_options: Some(
                serde_json::json!({ "output": { "entryFileNames": "x/[name].js" } }),
            ),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let ro = oj_config::rolldown_options(&config).unwrap();
        assert_eq!(
            ro.pointer("/output/entryFileNames")
                .and_then(|v| v.as_str()),
            Some("x/[name].js")
        );
    }

    #[test]
    fn parse_reads_build_block() {
        let v = parse_vite_values(&serde_json::json!({
            "build": { "outDir": "out", "sourcemap": true, "minify": false,
                       "cssCodeSplit": false, "target": "es2020", "ssr": "src/entry-server.ts" }
        }));
        let b = v.build.unwrap();
        assert_eq!(b["outDir"], "out");
        assert_eq!(b["sourcemap"], true);
        assert_eq!(b["ssr"], "src/entry-server.ts");
        assert!(parse_vite_values(&serde_json::json!({ "build": null })).build.is_none());
    }

    #[test]
    fn merge_adopts_build_fields_only_when_unset() {
        let mut config = oj_config::OjConfig::default();
        config.build = Some(oj_config::BuildConfig {
            out_dir: Some("oj-out".into()),
            ..Default::default()
        });
        let v = ViteValues {
            build: Some(serde_json::json!({
                "outDir": "vite-out", "sourcemap": true, "minify": false,
                "cssCodeSplit": false, "target": "es2020", "ssr": "src/server.ts"
            })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let b = config.build.unwrap();
        assert_eq!(b.out_dir.as_deref(), Some("oj-out"), "oj.config wins");
        assert_eq!(b.sourcemap, Some(oj_config::BoolOrString::Bool(true)));
        assert_eq!(b.minify, Some(oj_config::BoolOrString::Bool(false)));
        assert_eq!(b.css_code_split, Some(false));
        assert_eq!(b.target.as_ref().map(|t| t.to_vec()), Some(vec!["es2020".to_string()]));
        assert_eq!(b.ssr, Some(oj_config::BoolOrString::Str("src/server.ts".into())));
    }

    #[test]
    fn merge_adopts_ssr_block_and_ssr_manifest() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            build: Some(serde_json::json!({ "ssr": true, "ssrManifest": true })),
            ssr: Some(serde_json::json!({ "noExternal": ["ui-kit"], "target": "webworker" })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(oj_config::ssr_manifest_name(&config).as_deref(), Some(".vite/ssr-manifest.json"));
        let e = oj_config::ssr_externals(&config);
        assert!(e.webworker() && !e.is_external_pkg("ui-kit"));
    }

    #[test]
    fn merge_adopts_vite_string_variants_and_empty_out_dir() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            build: Some(serde_json::json!({
                "sourcemap": "hidden", "minify": "terser", "target": ["es2020", "safari14"],
                "emptyOutDir": false
            })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(oj_config::build_sourcemap(&config), oj_config::Sourcemap::Hidden);
        assert!(oj_config::build_minify(&config));
        assert_eq!(oj_config::build_targets(&config), vec!["es2020", "safari14"]);
        assert_eq!(config.build.unwrap().empty_out_dir, Some(false));
    }

    #[test]
    fn merge_ignores_build_values_of_the_wrong_shape() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            build: Some(serde_json::json!({ "outDir": 3, "sourcemap": 7, "target": {"x": 1} })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let b = config.build.unwrap();
        assert!(b.out_dir.is_none());
        assert!(b.sourcemap.is_none());
        assert!(b.target.is_none());
    }

    #[test]
    fn merge_adopts_jsx_blocks_when_unset() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            oxc: Some(serde_json::json!({ "jsx": { "importSource": "@emotion/react" } })),
            esbuild: Some(serde_json::json!({ "jsxFactory": "h" })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let s = oj_config::jsx_settings(&config);
        assert_eq!(s.import_source.as_deref(), Some("@emotion/react"));
        assert_eq!(s.pragma.as_deref(), Some("h"));

        let mut config = oj_config::OjConfig::default();
        config.oxc = Some(serde_json::json!({ "jsx": { "importSource": "preact" } }));
        let v = ViteValues {
            oxc: Some(serde_json::json!({ "jsx": { "importSource": "@emotion/react" } })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(oj_config::jsx_settings(&config).import_source.as_deref(), Some("preact"), "oj.config wins");
    }

    #[test]
    fn merge_adopts_ssr_block_when_unset() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            ssr: Some(serde_json::json!({ "noExternal": ["lodash-es", { "regex": "^@acme/" }], "external": ["sharp"] })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let r = oj_config::ssr_externals(&config);
        assert!(r.is_no_external("lodash-es"));
        assert!(r.is_no_external("@acme/ui"));
        assert_eq!(r.is_external("sharp", true), Some(true));
    }

    #[test]
    fn merge_adopts_resolve_server_css_env_and_mode() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            mode: Some("staging".into()),
            resolve: Some(serde_json::json!({
                "extensions": [".ts", ".js"], "mainFields": ["module"],
                "conditions": ["custom"], "externalConditions": ["custom-ext"],
                "preserveSymlinks": true
            })),
            server_flags: Some(serde_json::json!({ "strictPort": true, "open": true })),
            css: Some(serde_json::json!({ "preprocessorOptions": { "scss": { "additionalData": "@use 'x';" } } })),
            env_prefix: Some(vec!["VITE_".into(), "APP_".into()]),
            env_dir: Some("env".into()),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(config.mode.as_deref(), Some("staging"));
        let rc = config.resolve.as_ref().unwrap();
        assert_eq!(rc.extensions.as_deref(), Some(&[".ts".to_string(), ".js".to_string()][..]));
        assert_eq!(rc.main_fields.as_deref(), Some(&["module".to_string()][..]));
        assert_eq!(rc.conditions.as_deref(), Some(&["custom".to_string()][..]));
        assert_eq!(rc.external_conditions.as_deref(), Some(&["custom-ext".to_string()][..]));
        assert_eq!(rc.preserve_symlinks, Some(true));
        let sc = config.server.as_ref().unwrap();
        assert_eq!(sc.strict_port, Some(true));
        assert_eq!(sc.open, Some(true));
        let scss = &config.css.as_ref().unwrap().preprocessor_options.as_ref().unwrap()["scss"];
        assert_eq!(scss.additional_data.as_deref(), Some("@use 'x';"));
        assert_eq!(oj_config::env_prefixes(&config), vec!["VITE_".to_string(), "APP_".to_string()]);
        assert_eq!(config.env_dir.as_deref(), Some("env"));

        // oj.config values win.
        let mut config = oj_config::OjConfig::default();
        config.mode = Some("qa".into());
        config.env_dir = Some("cfg".into());
        merge_vite_values(&mut config, ViteValues { mode: Some("staging".into()), env_dir: Some("env".into()), ..Default::default() });
        assert_eq!(config.mode.as_deref(), Some("qa"));
        assert_eq!(config.env_dir.as_deref(), Some("cfg"));
    }

    #[test]
    fn merge_adopts_cors_and_allowed_hosts() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            cors: Some(serde_json::json!({ "origin": ["http://a.test"], "credentials": true })),
            allowed_hosts: Some(serde_json::json!([".corp.example"])),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        let sc = config.server.unwrap();
        assert!(matches!(sc.cors, Some(oj_config::CorsConfig::Options(ref o)) if o.credentials == Some(true)));
        assert!(matches!(sc.allowed_hosts, Some(oj_config::AllowedHosts::List(ref l)) if l == &vec![".corp.example".to_string()]));
        let mut config = oj_config::OjConfig::default();
        merge_vite_values(&mut config, ViteValues { cors: Some(serde_json::json!(false)), allowed_hosts: Some(serde_json::json!(true)), ..Default::default() });
        let sc = config.server.unwrap();
        assert!(matches!(sc.cors, Some(oj_config::CorsConfig::Toggle(false))));
        assert!(matches!(sc.allowed_hosts, Some(oj_config::AllowedHosts::All(true))));
    }

    #[test]
    fn merge_adopts_css_preprocessor_options() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            css: Some(serde_json::json!({ "preprocessorOptions": { "scss": { "additionalData": "$b: red;", "loadPaths": ["styles"] } } })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(oj_config::css_additional_data(&config, "scss").as_deref(), Some("$b: red;"));
        assert_eq!(oj_config::css_load_paths(&config, "scss"), vec!["styles".to_string()]);
    }

    #[test]
    fn extraction_stderr_lines_print_once_per_process() {
        let first = unseen_extraction_lines("oj: vite.config: worker config is not applied\nsome plugin notice\n");
        assert_eq!(first, "oj: vite.config: worker config is not applied\nsome plugin notice\n");
        let again = unseen_extraction_lines("oj: vite.config: worker config is not applied\nsome plugin notice\nnew line\n");
        assert_eq!(again, "new line\n", "only lines not printed before in this process come back");
        assert_eq!(unseen_extraction_lines(""), "");
    }
}

// The extraction contract through the REAL in-process engine: these boot a V8
// isolate per case, so they are serialized on one lock (and any test that
// touches the extraction env knobs must hold it while they are set).
#[cfg(test)]
mod engine_extraction_tests {
    use super::*;

    static ENGINE_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        ENGINE_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn app(config: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"fx","version":"1.0.0","type":"module"}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join("vite.config.mjs"), config).unwrap();
        dir
    }

    #[test]
    fn extraction_returns_values_and_caches_them() {
        let _g = lock();
        let dir = app(r#"export default { base: "/app/", server: { port: 5199 } };"#);
        let root = dir.path();
        let v = extract_vite_values_with(root, "serve", "development", true)
            .expect("a valid config extracts");
        assert_eq!(v.base.as_deref(), Some("/app/"));
        assert_eq!(v.port, Some(5199));

        // The run was cached under the engine-marked version tag, stderr field
        // and all, and the cached output parses back to the same values.
        let hit = extraction_store(root)
            .lookup(&root.join("vite.config.mjs"), "serve", "development")
            .expect("the extraction is cached");
        let json: serde_json::Value = serde_json::from_str(&hit.output).unwrap();
        assert_eq!(json["base"], "/app/");
        assert!(
            json.get("__stderr").is_none(),
            "the transcript is stored in its own field, not inside the output"
        );
        // The env files Vite would load for this mode are stamped, absent or
        // not, so creating one later is a cache miss.
        assert!(
            hit.deps.iter().any(|d| d.ends_with(".env.development")),
            "mode env files are stamped: {:?}",
            hit.deps
        );
    }

    #[test]
    fn a_broken_config_is_none_not_an_empty_config() {
        let _g = lock();
        let dir = app("throw new Error('config exploded');\nexport default {};");
        let root = dir.path();
        assert!(
            extract_vite_values_with(root, "serve", "development", true).is_none(),
            "a config that fails to evaluate must never parse as empty values"
        );
        assert!(
            extraction_store(root)
                .lookup(&root.join("vite.config.mjs"), "serve", "development")
                .is_none(),
            "a failed extraction is never cached"
        );
        // ...and the adopt seam surfaces it as the load error Vite gives.
        let mut config = oj_config::OjConfig::default();
        let err = adopt_vite_config_values(&mut config, root, "serve", "development")
            .expect_err("a present-but-broken vite.config is an error");
        assert!(err.contains("failed to load config"), "{err}");
    }

    #[test]
    fn config_imports_are_recorded_as_deps() {
        let _g = lock();
        let dir = app(
            r#"import { base } from "./base.config.mjs";
export default { base };"#,
        );
        let root = dir.path();
        std::fs::write(root.join("base.config.mjs"), "export const base = \"/dep/\";\n").unwrap();
        let v = extract_vite_values_with(root, "serve", "development", true).unwrap();
        assert_eq!(v.base.as_deref(), Some("/dep/"));
        let hit = extraction_store(root)
            .lookup(&root.join("vite.config.mjs"), "serve", "development")
            .expect("cached");
        assert!(
            hit.deps.iter().any(|d| d.ends_with("base.config.mjs")),
            "the config's own imports invalidate the cache: {:?}",
            hit.deps
        );
    }

    #[test]
    fn truncated_observed_reads_serve_but_never_cache() {
        let _g = lock();
        // Cap the read recorder at one path; the config reads two .json files
        // (through the fs default object, the surface the recorder wraps), so
        // the dep stamp is incomplete and the result must not be cached.
        std::env::set_var("OJ_OBSERVED_READS_MAX", "1");
        let dir = app(
            r#"import fs from "node:fs";
const a = JSON.parse(fs.readFileSync(new URL("./a.json", import.meta.url), "utf8"));
const b = JSON.parse(fs.readFileSync(new URL("./b.json", import.meta.url), "utf8"));
export default { base: a.base + b.base };"#,
        );
        let root = dir.path();
        std::fs::write(root.join("a.json"), r#"{"base":"/a"}"#).unwrap();
        std::fs::write(root.join("b.json"), r#"{"base":"/b"}"#).unwrap();
        let result = extract_vite_values_with(root, "serve", "development", true);
        std::env::remove_var("OJ_OBSERVED_READS_MAX");
        let v = result.expect("the result is still served");
        assert_eq!(v.base.as_deref(), Some("/a/b"));
        assert!(
            extraction_store(root)
                .lookup(&root.join("vite.config.mjs"), "serve", "development")
                .is_none(),
            "an extraction with a truncated dep stamp must not be cached"
        );
    }

    #[test]
    fn a_config_that_never_finishes_is_terminated_at_the_deadline() {
        let _g = lock();
        std::env::set_var("OJ_EXTRACT_TIMEOUT", "2");
        let dir = app("await new Promise(() => {});\nexport default {};");
        let root = dir.path();
        let started = std::time::Instant::now();
        let result = extract_vite_values_with(root, "serve", "development", true);
        std::env::remove_var("OJ_EXTRACT_TIMEOUT");
        assert!(result.is_none(), "a wedged config evaluation is a failure");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "the deadline must end the wait, not the config's leisure"
        );
    }

    #[test]
    fn a_hook_started_interval_does_not_outlive_the_extraction() {
        let _g = lock();
        // The in-process equivalent of the old one-shot subprocess's
        // process.exit(0): a config that leaves timers behind (the TanStack
        // route generator shape) must not stall the caller, and the engine
        // dies with them at drop.
        let dir = app("setInterval(() => {}, 1000);\nexport default { base: \"/live/\" };");
        let root = dir.path();
        let started = std::time::Instant::now();
        let v = extract_vite_values_with(root, "serve", "development", true).unwrap();
        assert_eq!(v.base.as_deref(), Some("/live/"));
        assert!(
            started.elapsed() < std::time::Duration::from_secs(30),
            "a live timer must not hold the extraction open"
        );
    }

    #[test]
    fn stderr_prints_from_config_code_travel_in_the_transcript() {
        let _g = lock();
        let dir = app(
            r#"console.error("plugin says hi");
process.stderr.write("direct stderr write\n");
console.log("stdout is swallowed");
export default { base: "/loud/" };"#,
        );
        let root = dir.path();
        let v = extract_vite_values_with(root, "serve", "development", true).unwrap();
        assert_eq!(v.base.as_deref(), Some("/loud/"));
        let hit = extraction_store(root)
            .lookup(&root.join("vite.config.mjs"), "serve", "development")
            .expect("cached");
        assert!(hit.stderr.contains("plugin says hi"), "{}", hit.stderr);
        assert!(hit.stderr.contains("direct stderr write"), "{}", hit.stderr);
        assert!(
            !hit.stderr.contains("stdout is swallowed"),
            "stdout prints are dropped, as the old subprocess capture dropped them: {}",
            hit.stderr
        );
    }

    #[test]
    fn config_env_writes_do_not_leak_into_the_oj_process() {
        let _g = lock();
        // The old subprocess kept env mutations to itself; the in-process
        // engine must shadow process.env the same way (Vite's own NODE_ENV
        // dance runs on every extraction).
        let dir = app(
            r#"process.env.OJ_EXTRACT_LEAK_PROBE = "leaked";
export default { base: "/env/" };"#,
        );
        let root = dir.path();
        let v = extract_vite_values_with(root, "serve", "development", true).unwrap();
        assert_eq!(v.base.as_deref(), Some("/env/"));
        assert!(
            std::env::var("OJ_EXTRACT_LEAK_PROBE").is_err(),
            "a config's env write must die with its extraction"
        );
    }

    // The TS-config fallback (no vite installed): the extractor bundles the
    // config with the app's esbuild — a child process spawned from inside the
    // engine — writes the bundle next to its own script and imports it.
    // Skips quietly where the start-app fixture has no node_modules.
    #[test]
    fn a_ts_config_without_vite_loads_through_the_esbuild_fallback() {
        let _g = lock();
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let esbuild = repo.join("e2e/fixtures/start-app/node_modules/esbuild");
        if !esbuild.exists() {
            eprintln!("skipping: fixture esbuild not installed");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("package.json"),
            r#"{"name":"fx","version":"1.0.0","type":"module","dependencies":{"esbuild":"*"}}"#,
        )
        .unwrap();
        std::fs::create_dir_all(root.join("node_modules")).unwrap();
        std::os::unix::fs::symlink(&esbuild, root.join("node_modules/esbuild")).unwrap();
        let scoped = repo.join("e2e/fixtures/start-app/node_modules/@esbuild");
        if scoped.exists() {
            std::os::unix::fs::symlink(&scoped, root.join("node_modules/@esbuild")).unwrap();
        }
        std::fs::write(root.join("shared.ts"), "export const port: number = 5321;\n").unwrap();
        std::fs::write(
            root.join("vite.config.ts"),
            "import { port } from \"./shared\";\nexport default { base: \"/ts/\" as const, server: { port } };\n",
        )
        .unwrap();
        let v = extract_vite_values_with(root, "serve", "development", true)
            .expect("the TS config loads through the esbuild fallback");
        assert_eq!(v.base.as_deref(), Some("/ts/"));
        assert_eq!(v.port, Some(5321));
        // The bundle's metafile names the config's imports as deps.
        let hit = extraction_store(root)
            .lookup(&root.join("vite.config.ts"), "serve", "development")
            .expect("cached");
        assert!(
            hit.deps.iter().any(|d| d.ends_with("shared.ts")),
            "esbuild metafile inputs are stamped: {:?}",
            hit.deps
        );
    }

    // The phase's crux, checked at the exact seam production uses: a module
    // running on the engine spawns a real child process (as esbuild's JS API
    // spawns its Go service) and reads it back.
    #[test]
    fn engine_jobs_can_spawn_child_processes() {
        let _g = lock();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"name":"fx","version":"1.0.0","type":"module"}"#,
        )
        .unwrap();
        std::fs::write(
            dir.path().join("spawn-job.mjs"),
            r#"import { execFileSync, spawn } from "node:child_process";
export async function run() {
  const sync = execFileSync("/bin/echo", ["sync-child"], { encoding: "utf8" }).trim();
  const child = spawn("/bin/echo", ["piped-child"]);
  let piped = "";
  child.stdout.on("data", (d) => { piped += d.toString(); });
  const code = await new Promise((resolve) => child.on("close", resolve));
  return { sync, piped: piped.trim(), code };
}
"#,
        )
        .unwrap();
        if !std::path::Path::new("/bin/echo").exists() {
            return; // not a unix-y machine: the seam under test cannot run
        }
        let out = run_engine_job(
            dir.path(),
            &dir.path().join("spawn-job.mjs"),
            "run",
            serde_json::json!({}),
            std::time::Duration::from_secs(30),
        )
        .expect("child_process must work under the embedded engine");
        assert_eq!(out["sync"], "sync-child");
        assert_eq!(out["piped"], "piped-child");
        assert_eq!(out["code"], 0);
    }

    // The engine-job child talks to its parent through this envelope; a
    // variant that does not survive the round trip would turn a child's
    // deadline or JS error into a generic boot failure.
    #[test]
    fn engine_job_envelope_round_trips_every_outcome() {
        let outcomes: Vec<Result<serde_json::Value, oj_js::EngineError>> = vec![
            Ok(serde_json::json!({ "a": [1, "two"] })),
            Err(oj_js::EngineError::Boot("no engine".into())),
            Err(oj_js::EngineError::Js("TypeError: boom".into())),
            Err(oj_js::EngineError::MemoryLimit),
            Err(oj_js::EngineError::Deadline),
            Err(oj_js::EngineError::Closed),
        ];
        for outcome in outcomes {
            let back = engine_job_outcome(engine_job_envelope(&outcome));
            match (&outcome, &back) {
                (Ok(a), Ok(b)) => assert_eq!(a, b),
                (Err(a), Err(b)) => assert_eq!(a.to_string(), b.to_string()),
                _ => panic!("outcome {outcome:?} came back as {back:?}"),
            }
        }
    }
}
