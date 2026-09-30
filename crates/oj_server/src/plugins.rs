// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use oj_resolver::OjResolver;

pub const PLUGIN_HOST_JS: &str = include_str!("assets/plugin-host.mjs");
/// Sibling module the plugin host and preseed optimizer child import as `./discovered-deps.mjs`.
pub const DISCOVERED_DEPS_JS: &str = include_str!("assets/discovered-deps.mjs");

/// Idempotent, atomic materialization of an embedded asset into `dir`.
pub(crate) fn ensure_asset(dir: &Path, name: &str, bytes: &str) -> std::io::Result<()> {
    let path = dir.join(name);
    if std::fs::read(&path).ok().as_deref() == Some(bytes.as_bytes()) {
        return Ok(());
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{name}.tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, &path)
}
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
            file_name: m
                .get("fileName")
                .and_then(|x| x.as_str())
                .map(str::to_string),
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
    // Vite's DEFAULT_CONFIG_FILES order: first existing wins.
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
    /// The `build` block as the extractor normalized it; see `extractBuild` in vite-extract.mjs.
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
    /// RAW config's top-level `resolve` (the resolved one carries client-env
    /// conditions); consulted by Node SSR consumers when ssr is runner-backed.
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

/// Bound on config extraction (default 60s, `OJ_EXTRACT_TIMEOUT` raises it):
/// a config hook that opens a socket or timer must not wedge boot forever.
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

/// The oj executable one-shot engine jobs run in, set once by the binary's main.
/// A child per job isolates native addons that crash on re-init (napi-rs before 3.10).
static ENGINE_JOB_EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

pub fn engine_jobs_via_subprocess(exe: PathBuf) {
    let _ = ENGINE_JOB_EXE.set(exe);
}

/// Spawner's pid for a one-shot child's parent-death reaper (see
/// `reap_on_parent_death` in the `oj` binary). Every spawner sets it.
pub const PARENT_PID_ENV: &str = "OJ_PARENT_PID";

/// Runs one export of an oj-owned module on a short-lived JS engine: in a child
/// `oj engine-job` process when [`engine_jobs_via_subprocess`] ran, in-process otherwise.
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

/// One isolate per run: caches and hook-started timers die with the engine.
/// Blocks; inside a tokio runtime it uses a scoped thread so no runtime worker parks in block_on.
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
        // Dropping the engine joins its thread; pending JS work dies with the isolate.
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

/// The `--result` file of an `oj engine-job` run, encoding the exact [`oj_js::EngineError`].
/// A file, not stdout, so job code that prints cannot corrupt the channel.
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

fn engine_job_outcome(
    envelope: serde_json::Value,
) -> Result<serde_json::Value, oj_js::EngineError> {
    if envelope.get("ok").and_then(|v| v.as_bool()) == Some(true) {
        return Ok(envelope
            .get("value")
            .cloned()
            .unwrap_or(serde_json::Value::Null));
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
    // Inheriting the parent's cwd fails the spawn with ENOENT when that dir was
    // deleted (hosts chdir at boot); a missing root falls back to the temp dir.
    let job_cwd = if root.is_dir() {
        root.to_path_buf()
    } else {
        std::env::temp_dir()
    };
    let mut child = std::process::Command::new(exe)
        .arg("engine-job")
        .current_dir(&job_cwd)
        .env("OJ_PARENT_PID", std::process::id().to_string())
        .arg(module)
        .arg("--root")
        .arg(root)
        .arg("--export")
        .arg(export)
        .arg("--timeout-secs")
        .arg(timeout.as_secs().max(1).to_string())
        .arg("--result")
        .arg(&result_file)
        .env(PARENT_PID_ENV, std::process::id().to_string())
        .stdin(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            boot(format!(
                "could not run the engine job child (cwd {}): {e}",
                job_cwd.display()
            ))
        })?;
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
    // The child enforces `timeout` itself; the grace covers spawn and result
    // writing, then a wedged child is killed.
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
                return Err(boot(format!(
                    "could not wait for the engine job child: {e}"
                )));
            }
        }
    };
    let envelope = std::fs::read_to_string(&result_file);
    let _ = std::fs::remove_file(&result_file);
    if !status.success() {
        // A crash means an addon or engine took the child down instead of the caller.
        return Err(boot(format!("the engine job child died: {status}")));
    }
    let envelope = envelope.map_err(|e| boot(format!("engine job result: {e}")))?;
    let envelope: serde_json::Value =
        serde_json::from_str(&envelope).map_err(|e| boot(format!("engine job result: {e}")))?;
    engine_job_outcome(envelope)
}

/// Evaluate the app's `vite.config` for `command` ("serve" | "build") and `mode`.
/// Function configs branch on both, so a build must be extracted as a build.
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
    // Keyed per (config, command, mode); a default-mode evaluation gets its own key.
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
    // Extractions run concurrently at boot, so the script lands via rename: a
    // plain write truncates it under a concurrent engine's import.
    static EXTRACT_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = EXTRACT_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let script = cache.join("oj-vite-extract.mjs");
    if std::fs::read(&script).ok().as_deref() != Some(VITE_EXTRACT_JS.as_bytes()) {
        let tmp = cache.join(format!(
            "oj-vite-extract-{}-{seq}.tmp.mjs",
            std::process::id()
        ));
        std::fs::write(&tmp, VITE_EXTRACT_JS).ok()?;
        std::fs::rename(&tmp, &script).ok()?;
    }
    // Bounded: config plugin code must never wedge boot forever.
    let timeout = extraction_timeout();
    let payload = serde_json::json!({
        "vite": vite.to_string_lossy(),
        "root": root.to_string_lossy(),
        "command": command,
        "mode": mode,
        "modeKind": if mode_explicit { "explicit" } else { "default" },
        "cacheDir": cache.to_string_lossy(),
    });
    // The result is the call's return value: config code that prints cannot corrupt it.
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
    // Everything the evaluation wrote to stderr, captured inside the engine.
    let stderr = json
        .get("__stderr")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    print_extraction_stderr(&stderr);
    // `__ok: false` means the config failed to evaluate (cause already in stderr).
    // Never parse that into an empty ViteValues; return None.
    if json.get("__ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    // Stored under the same (config, command, mode_key) the lookup uses; a
    // default-mode evaluation must not masquerade as the explicit-mode entry.
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
        // Read recorder hit its cap: `__deps` is incomplete, so serve the
        // result but never cache it.
        eprintln!(
            "oj: extracting {}: the config evaluation read more config-shaped files than the recorder tracks; result not cached",
            vite.display()
        );
    } else {
        // Stderr transcript lives in its own field, not inside the cached output.
        let mut stored = json.clone();
        if let Some(obj) = stored.as_object_mut() {
            obj.remove("__stderr");
        }
        store.store(
            &vite,
            command,
            mode_key,
            &deps,
            &stored.to_string(),
            &stderr,
        );
    }
    EXTRACTION_RAN_FRESH.store(true, std::sync::atomic::Ordering::Relaxed);
    crate::boot_phase("vite-extract cache miss (engine ran)");
    Some(parse_vite_values(&json))
}

/// Whether any config extraction ran the engine (cache miss): caches derived
/// from the evaluated config (the deps pre-seed stamp) must not serve either.
static EXTRACTION_RAN_FRESH: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn extraction_ran_fresh() -> bool {
    EXTRACTION_RAN_FRESH.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the extractor's read recorder overflowed (`__depsTruncated`): the
/// dep list is incomplete and the extraction must not be cached under it.
fn extraction_deps_truncated(json: &serde_json::Value) -> bool {
    json.get("__depsTruncated").and_then(|v| v.as_bool()) == Some(true)
}

/// Extractor stderr, printed once per process: the config is loaded several
/// times per dev session, each replaying the cached stderr; Vite prints once.
fn print_extraction_stderr(stderr: &str) {
    let fresh = unseen_extraction_lines(stderr);
    if !fresh.is_empty() {
        eprint!("{fresh}");
    }
}

fn unseen_extraction_lines(stderr: &str) -> String {
    static SEEN: std::sync::Mutex<Option<std::collections::HashSet<String>>> =
        std::sync::Mutex::new(None);
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

/// Extraction cache key: oj version, engine marker ("deno"), script hash and
/// observable env; entries from another engine era must never serve.
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

/// Env a vite.config can observe (`VITE_*`, `NODE_ENV`), hashed into the cache
/// key so an env change re-evaluates instead of serving stale values.
pub fn extraction_env_hash(vars: impl Iterator<Item = (String, String)>) -> String {
    let mut relevant: Vec<(String, String)> = vars
        .filter(|(k, _)| k == "NODE_ENV" || k.starts_with("VITE_"))
        .collect();
    relevant.sort();
    let mut hasher = blake3::Hasher::new();
    for (k, v) in relevant {
        hasher.update(k.as_bytes());
        hasher.update(b"=");
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
        mode: json
            .get("mode")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        resolve: json.get("resolve").filter(|v| !v.is_null()).cloned(),
        raw_resolve: json.get("rawResolve").filter(|v| !v.is_null()).cloned(),
        server_flags: json.get("serverFlags").filter(|v| !v.is_null()).cloned(),
        css: json.get("css").filter(|v| !v.is_null()).cloned(),
        env_prefix: json.get("envPrefix").and_then(|v| v.as_array()).map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        }),
        env_dir: json
            .get("envDir")
            .and_then(|v| v.as_str())
            .map(str::to_string),
        cors: json.get("cors").filter(|v| !v.is_null()).cloned(),
        allowed_hosts: json.get("allowedHosts").filter(|v| !v.is_null()).cloned(),
        preview: json.get("preview").filter(|v| !v.is_null()).cloned(),
        app_type: json
            .get("appType")
            .and_then(|v| v.as_str())
            .map(str::to_string),
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
        // No vite.config: nothing to adopt. A present-but-broken vite.config fails
        // hard like Vite; an oj.plugins file takes precedence (extractor skips it).
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

/// Like `adopt_vite_config_values`, for a default `mode`: the config file's own
/// `mode` is honored and lands in `config.mode` so the caller can reload under it.
pub fn adopt_vite_config_values_default_mode(
    config: &mut oj_config::OjConfig,
    root: &Path,
    command: &str,
    mode: &str,
) -> Result<(), String> {
    let Some(v) = extract_vite_values_with(root, command, mode, false) else {
        // Same rule: a present-but-broken vite.config is an error, a missing one is fine.
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
        if v.fs_allow.is_some() || v.fs_strict.is_some() {
            let fs = sc.fs.get_or_insert_with(Default::default);
            fs.allow = fs.allow.take().or(v.fs_allow);
            fs.strict = fs.strict.or(v.fs_strict);
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
        if let Ok(parsed) = serde_json::from_value::<oj_config::OptimizeDepsConfig>(od) {
            let o = config.optimize_deps.get_or_insert_with(Default::default);
            o.include = o.include.take().or(parsed.include);
            o.exclude = o.exclude.take().or(parsed.exclude);
            o.entries = o.entries.take().or(parsed.entries);
            o.needs_interop = o.needs_interop.take().or(parsed.needs_interop);
            o.force = o.force.or(parsed.force);
            o.no_discovery = o.no_discovery.or(parsed.no_discovery);
            o.esbuild_options = o.esbuild_options.take().or(parsed.esbuild_options);
            o.rolldown_options = o.rolldown_options.take().or(parsed.rolldown_options);
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
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect(),
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
            build.chunk_size_warning_limit =
                vb.get("chunkSizeWarningLimit").and_then(|v| v.as_f64());
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
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect(),
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
    // ssr merges PER-KEY: `runnerBacked` (only extraction produces it) is always
    // adopted; `resolve` recurses one level so oj sub-keys keep the extractor's others.
    match (config.ssr.as_mut(), v.ssr) {
        (None, vssr) => config.ssr = vssr,
        (Some(existing), Some(vssr)) => {
            if !existing.is_object() {
                // Nothing to merge per-key into; adopt the extractor block (keeps runnerBacked).
                eprintln!(
                    "oj: config: the ssr block in oj's config is not an object; the vite.config ssr block is used"
                );
                *existing = vssr;
            } else if let (Some(obj), Some(vobj)) = (existing.as_object_mut(), vssr.as_object()) {
                for (k, val) in vobj {
                    if k == "runnerBacked" || !obj.contains_key(k) {
                        obj.insert(k.clone(), val.clone());
                    } else if k == "resolve" {
                        let Some(vsub) = val.as_object() else {
                            continue;
                        };
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
                            // Nothing to merge into: adopt the extractor block
                            // over dropping the sugar's conditions.
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
            config.app_type = sf
                .get("appType")
                .and_then(|a| a.as_str())
                .map(str::to_string);
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
        if let Some(w) = sf
            .get("warmup")
            .and_then(|w| serde_json::from_value::<oj_config::WarmupConfig>(w.clone()).ok())
        {
            let warmup = sc.warmup.get_or_insert_with(Default::default);
            warmup.client_files = warmup.client_files.take().or(w.client_files);
            warmup.ssr_files = warmup.ssr_files.take().or(w.ssr_files);
        }
        if let Some(deny) = sf
            .get("fsDeny")
            .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
        {
            sc.fs
                .get_or_insert_with(Default::default)
                .deny
                .get_or_insert(deny);
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
        if let Some(cfg) = config.css.as_mut() {
            if let Some(po) = css.get("preprocessorOptions").and_then(|p| p.as_object()) {
                let map = cfg
                    .preprocessor_options
                    .get_or_insert_with(Default::default);
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
        } else {
            // The whole block (preprocessorOptions, devSourcemap, modules).
            config.css = serde_json::from_value::<oj_config::CssConfig>(css.clone()).ok();
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

/// One hook's build gate: presence, always-offered plugins, and include filters.
/// Only over-approximates: `wants` may say yes wrongly, never no for a module a plugin would claim.
#[derive(Debug, Default, Clone)]
pub struct HookFilterPlan {
    pub present: bool,
    pub unfiltered: bool,
    pub plugins: Vec<PluginFilter>,
}

#[derive(Debug, Clone)]
pub struct PluginFilter {
    pub id: Vec<regex::Regex>,
    pub code: Vec<regex::Regex>,
}

#[derive(Debug, Default, Clone)]
pub struct BuildHookPlan {
    pub transform: HookFilterPlan,
    pub load: HookFilterPlan,
    pub resolve_id: HookFilterPlan,
}

impl HookFilterPlan {
    fn fail_open() -> Self {
        Self {
            present: true,
            unfiltered: true,
            plugins: Vec::new(),
        }
    }

    fn from_json(v: Option<&serde_json::Value>) -> Self {
        let Some(v) = v else {
            return Self::fail_open();
        };
        let present = v.get("present").and_then(|b| b.as_bool()).unwrap_or(true);
        let mut unfiltered = v
            .get("unfiltered")
            .and_then(|b| b.as_bool())
            .unwrap_or(true);
        let mut plugins = Vec::new();
        for entry in v
            .get("plugins")
            .and_then(|p| p.as_array())
            .map(|a| a.as_slice())
            .unwrap_or(&[])
        {
            let compile = |key: &str| -> Option<Vec<regex::Regex>> {
                let mut out = Vec::new();
                for s in entry.get(key).and_then(|x| x.as_array())? {
                    // A JS regex the regex crate cannot compile (lookaround,
                    // backrefs) cannot gate; the plugin then always crosses.
                    out.push(regex::Regex::new(s.as_str()?).ok()?);
                }
                Some(out)
            };
            match (compile("id"), compile("code")) {
                (Some(id), Some(code)) if !id.is_empty() || !code.is_empty() => {
                    plugins.push(PluginFilter { id, code });
                }
                _ => unfiltered = true,
            }
        }
        Self {
            present,
            unfiltered,
            plugins,
        }
    }

    /// Whether any plugin's filter could claim this module; a code filter with
    /// no code available passes, keeping the gate an over-approximation.
    pub fn wants(&self, id: &str, code: Option<&str>) -> bool {
        if !self.present {
            return false;
        }
        if self.unfiltered {
            return true;
        }
        // The host matches slash-normalized ids, so normalize Windows paths
        // the same way or the gate under-matches.
        let id = if id.contains('\\') {
            std::borrow::Cow::Owned(id.replace('\\', "/"))
        } else {
            std::borrow::Cow::Borrowed(id)
        };
        let id = id.as_ref();
        self.plugins.iter().any(|p| {
            let id_ok = p.id.is_empty() || p.id.iter().any(|re| re.is_match(id));
            let code_ok = p.code.is_empty()
                || match code {
                    Some(c) => p.code.iter().any(|re| re.is_match(c)),
                    None => true,
                };
            id_ok && code_ok
        })
    }
}

/// OJ_DEBUG_HOOK_GATE=1: gates log/count skipped RPCs so a test can assert a
/// skip happened (output alone cannot, the host's filters produce identical bytes).
pub fn hook_gate_debug() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("OJ_DEBUG_HOOK_GATE").is_ok_and(|v| v == "1"));
    *ON
}

impl BuildHookPlan {
    pub fn fail_open() -> Self {
        Self {
            transform: HookFilterPlan::fail_open(),
            load: HookFilterPlan::fail_open(),
            resolve_id: HookFilterPlan::fail_open(),
        }
    }
}

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
    ws_out: Mutex<Option<tokio::sync::broadcast::Sender<String>>>,
    /// `{ ojServer: { action, ... } }` pushes from the host: a plugin invalidating
    /// a module via server.moduleGraph, or server.restart().
    server_events: Mutex<Option<tokio::sync::mpsc::UnboundedSender<serde_json::Value>>>,
    /// Host's `{ ojServeInfo }` push: None until top-level init completes; late
    /// subscribers can still activate the middleware path.
    serve_info_push: tokio::sync::watch::Sender<Option<ServeInfo>>,
    /// Host finished top-level init: flipped by the serve-info push, `{ ojInit }`,
    /// or the first hook reply. Hook calls gate on this; see `call`.
    initialized: tokio::sync::watch::Sender<bool>,
    /// Host is gone (engine thread exited, init failed, or declared wedged): fail
    /// calls fast. A watch so waiters select on the death instead of polling.
    host_gone: tokio::sync::watch::Sender<bool>,
    /// When the CURRENT generation spawned (reset by a revive); the init deadline
    /// is measured from here so boot RPCs share one deadline.
    spawned: Mutex<tokio::time::Instant>,
    /// How long a call may wait for top-level init: the boot host takes the long
    /// deadline measured from spawn, a lazy host the per-call bound from each call's start.
    init_wait: std::time::Duration,
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
    revive: Mutex<ReviveState>,
    /// Set by `shutdown()`: retired on purpose, must never be revived.
    shut_down: std::sync::atomic::AtomicBool,
    /// Weak self-handle so `&self` methods can hand a fresh generation's tasks their `Arc`s.
    self_ref: std::sync::OnceLock<std::sync::Weak<PluginHost>>,
}

/// The dev listener's bound (port, interface): hosts booting after the bind
/// read it in `ignite`; hosts already up are announced via [`LIVE_HOSTS`].
static DEV_LISTENER: Mutex<Option<(u16, String)>> = Mutex::new(None);
/// Every spawned host, weakly: the bind-time announce must reach hosts booted
/// before the listener existed (the normal boot order).
static LIVE_HOSTS: Mutex<Vec<std::sync::Weak<PluginHost>>> = Mutex::new(Vec::new());

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
struct BootContext {
    boot_seed: String,
    root: PathBuf,
    stall_wait: std::time::Duration,
    /// Heap cap every generation is spawned with (see `plugin_host_memory_mb`).
    memory_limit_bytes: usize,
}

/// See [`PluginHost::revive`].
struct ReviveState {
    /// The live engine's generation; bumped by each revive.
    generation: u64,
    /// Respawns consumed: a LIFETIME budget, never reset by a successful boot
    /// (a recurring wedge would respawn forever, leaking a wedged isolate thread each time).
    attempts: u32,
    /// When the last revive ran: spacing so a burst cannot burn the whole budget at once.
    last: Option<std::time::Instant>,
    /// Addons pending unsafe re-registration when THIS host died (snapshotted
    /// before the abandon): the revive gate refuses only on ones the dead generation itself orphaned.
    pending_before: std::collections::HashSet<PathBuf>,
    /// This generation's death was declared: only the first report may snapshot
    /// `pending_before` (a later one would fold in the dead generation's own orphans). Reset by each revive.
    reported: bool,
}

/// First report for the live generation wins; stale-generation reports and
/// duplicates are dropped. Serialized by the caller holding the `revive` lock.
fn first_death_report(revive: &mut ReviveState, generation: u64) -> bool {
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

/// The last `--max-old-space-size` in NODE_OPTIONS (last wins, like Node;
/// underscore spelling accepted too).
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

/// Process-wide native-addon KEEPER: a hidden engine pre-registers live addons before
/// a dying host's teardown orphans them, so a respawn re-registers into the safe concurrent-envs case.
static ADDON_KEEPER: Mutex<Option<std::sync::Arc<oj_js::JsEngine>>> = Mutex::new(None);

/// Keeper budget, deliberately UNDER `PLUGIN_HOST_RESPAWN_SPACING`: by the time
/// a revive is allowed the keeper has either registered or given up, never in between.
const ADDON_KEEPER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(4);

/// Load `addons` into the keeper engine (spawned on first use), each best-effort:
/// one addon failing to require must not cost the others their keeper.
async fn keep_addons_alive(root: &Path, addons: &[PathBuf]) -> Result<(), String> {
    let engine = {
        let mut keeper = ADDON_KEEPER.lock().unwrap();
        match &*keeper {
            Some(engine) => std::sync::Arc::clone(engine),
            None => {
                let engine = std::sync::Arc::new(
                    oj_js::JsEngine::spawn(oj_js::EngineConfig::new(root))
                        .map_err(|e| format!("keeper engine failed to spawn: {e}"))?,
                );
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
        .eval_with_deadline(
            oj_js::EvalInput::Source(script),
            Some(ADDON_KEEPER_DEADLINE),
        )
        .await
        .map(|_| ())
        .map_err(|e| format!("keeper load failed: {e}"))
}

/// Process RSS in MB, best effort, for host-death diagnostics: /proc on Linux,
/// getrusage's PEAK elsewhere.
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
/// info), answered SYNCHRONOUSLY on the isolate thread via the `__oj_rpc` bridge.
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

/// How long one plugin hook may run (default 20s, `OJ_PLUGIN_TIMEOUT` raises it);
/// Vite has no hook timeout at all.
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

/// How long the host may take to finish top-level init before a waiting RPC
/// gives up (`OJ_PLUGIN_INIT_TIMEOUT`); gates `call` instead of racing the per-call timeout.
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

/// Init-wait policy per spawn kind: boot host long deadline, lazy host short
/// per-call bound, each named after its env knob.
fn init_wait_policy(lazy: bool) -> (std::time::Duration, &'static str) {
    if lazy {
        (plugin_rpc_timeout(), "OJ_PLUGIN_TIMEOUT")
    } else {
        (plugin_init_timeout(), "OJ_PLUGIN_INIT_TIMEOUT")
    }
}

/// Per-spawn timeout overrides for tests, avoiding races on the env-var knobs
/// other tests read. Production spawns pass the default (env-derived).
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
    /// Spawn a boot-time host: calls wait out the full init deadline, because
    /// boot correctness depends on its snapshot RPCs.
    pub async fn spawn(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_with_policy(
            root,
            plugins_file,
            config_json,
            false,
            SpawnTimeouts::default(),
        )
        .await
    }

    /// Spawn a lazily created host (e.g. the SSR environment host): calls bound
    /// their init wait by the per-call timeout, so a wedged init cannot freeze the watcher thread.
    pub async fn spawn_lazy(
        root: &Path,
        plugins_file: &Path,
        config_json: &str,
    ) -> anyhow::Result<std::sync::Arc<PluginHost>> {
        Self::spawn_with_policy(
            root,
            plugins_file,
            config_json,
            true,
            SpawnTimeouts::default(),
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
            hook_plan: std::sync::RwLock::new(BuildHookPlan::fail_open()),
            hook_plan_fetched: std::sync::atomic::AtomicBool::new(false),
            hook_plan_prime_started: std::sync::atomic::AtomicBool::new(false),
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
        let root = host.boot.root.clone();
        let script = PathBuf::from(&host.host_module);
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Written atomically (tmp + rename): hosts spawn concurrently, and a
        // truncating write could hand a sibling engine a half-written module.
        let _ = ensure_asset(
            oj_cache::cache_root(&root).as_path(),
            "discovered-deps.mjs",
            DISCOVERED_DEPS_JS,
        );
        if std::fs::read(&script).ok().as_deref() != Some(PLUGIN_HOST_JS.as_bytes()) {
            let tmp = script.with_extension(format!("tmp-{}.mjs", std::process::id()));
            std::fs::write(&tmp, PLUGIN_HOST_JS).map_err(|e| e.to_string())?;
            std::fs::rename(&tmp, &script).map_err(|e| e.to_string())?;
        }

        // Pushes arrive as values on `post_rx`, so nothing a plugin prints can
        // splice into the protocol.
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

        // BOOT task: seed the host's identity, then trigger top-level init via a trivial
        // export. No deadline (the Rust-side watches own boot patience); a top-level throw declares the host gone.
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
                boot_ref.declare_gone(&format!("plugin host boot prelude failed: {e}"), generation);
                return;
            }
            match boot_engine
                .call_with_deadline(host_module, "ojHostReady", Vec::new(), None)
                .await
            {
                // The `{ ojInit }` push already flipped `initialized`; the reply is
                // the error path's carrier. Deliver a bind-time announce this host missed.
                Ok(_) => {
                    let listener = DEV_LISTENER.lock().unwrap().clone();
                    if let Some((port, interface)) = listener {
                        boot_ref.announce_dev_listener(port, &interface);
                    }
                }
                Err(oj_js::EngineError::Closed) => {}
                Err(e) => {
                    boot_ref.declare_gone(
                        &format!("plugin host failed to initialize: {e}"),
                        generation,
                    );
                }
            }
        });

        // The PUSH DISPATCHER: control pushes arrive whole on the engine channel;
        // hook replies come back on their own call futures.
        let reader_ref = std::sync::Arc::clone(host);
        tokio::spawn(async move {
            while let Some(msg) = post_rx.recv().await {
                // Drain and drop pushes from a superseded or declared-dead generation:
                // a stale `ojInit` would open the call gate before the NEW engine evaluated its module.
                if *reader_ref.host_gone.borrow()
                    || reader_ref.revive.lock().unwrap().generation != generation
                {
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
                    // Unconditional init-complete, sent in BOTH modes: build mode
                    // has no ojServeInfo push, and the gate must not wait for the first reply.
                    let _ = reader_ref.initialized.send_replace(true);
                    let _ = reader_ref.init_failed.send_replace(false);
                    continue;
                }
                if msg.get("ojResyncDone").is_some() {
                    // An enqueued resync actually ran; see resync_done.
                    reader_ref.resync_done.send_modify(|c| *c += 1);
                    continue;
                }
                if msg.get("ojInitProgress").is_some() {
                    // Real init milestone: wedge evidence is stale, the stall monitor re-arms.
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
            // Push channel closed = engine thread exited: fail future calls fast.
            // Generation-guarded: an abandoned engine's late exit must not kill the fresh generation.
            let revive = reader_ref.revive.lock().unwrap();
            if revive.generation == generation {
                drop(revive);
                let _ = reader_ref.host_gone.send_replace(true);
            }
        });

        // Init STALL MONITOR, wedge evidence independent of any caller's window: a
        // full RPC-scale window with NO milestone flips `init_failed`, progress clears it. Calls never consult it.
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
        Ok(())
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
        // Reset per-generation state BEFORE the new engine can push: init pending
        // again, evidence cleared, stale serve info dropped.
        let _ = self.initialized.send_replace(false);
        let _ = self.init_failed.send_replace(false);
        self.serve_info_push.send_replace(None);
        // The fresh engine re-evaluates the plugins file: fail-open now, the
        // refetch restores precise gating.
        *self.hook_plan.write().unwrap() = BuildHookPlan::fail_open();
        self.hook_plan_fetched
            .store(false, std::sync::atomic::Ordering::Release);
        self.hook_plan_prime_started
            .store(false, std::sync::atomic::Ordering::Release);
        *self.spawned.lock().unwrap() = tokio::time::Instant::now();
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
        // Init gate, BEFORE anything reaches the engine: the host answers hooks
        // only after top-level init, and a job submitted to a wedged mid-init
        // isolate would queue behind the wedge and be blamed on the hook.
        // Deliberately NO time-based fail-fast latch: an expired window is
        // evidence of a slow boot, not a wedge, so every call gets its own full
        // window; only host death (host_gone) fails fast.
        let mut init_rx = self.initialized.subscribe();
        if !*init_rx.borrow_and_update() {
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
                            break;
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
        // The engine call carries the per-call deadline itself, and a deadline
        // failure costs ONE call with the host answering everyone else. The BELT
        // past it is a second full window with NO reply of any kind: the scheduler
        // answers at the deadline while alive, so total silence means the isolate
        // thread is blocked in NATIVE code; declare the host gone.
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
                // Any reply proves top-level init completed. Generation-guarded: a
                // reply from a replaced engine must not open the gate for the new one.
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
                let _ = self.initialized.send_replace(true);
                let _ = self.init_failed.send_replace(false);
                Err(e)
            }
        }
    }

    /// Treat the host as dead NOW: abandon the engine and flip `host_gone`; the next
    /// call may revive it. A report about a since-replaced generation is stale and must not kill the replacement.
    fn declare_gone(&self, why: &str, generation: u64) {
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
            // KEEPER sequence: register live addons BEFORE the abandon (the open job
            // channel keeps their env alive until then); the spacing stamp defers the first revive past the keeper.
            let addons = oj_js::addons_with_live_registrations();
            if addons.is_empty() {
                engine.abandon();
            } else {
                revive.last = Some(std::time::Instant::now());
                let root = self.boot.root.clone();
                // Abandon rides a Drop guard: a cancelled task must still abandon,
                // or the engine Arc's drop would JOIN a possibly-wedged thread.
                struct AbandonOnDrop(Option<std::sync::Arc<oj_js::JsEngine>>);
                impl Drop for AbandonOnDrop {
                    fn drop(&mut self) {
                        if let Some(engine) = self.0.take() {
                            engine.abandon();
                        }
                    }
                }
                let guard = AbandonOnDrop(Some(engine));
                tokio::spawn(async move {
                    let _guard = guard;
                    if let Err(e) = keep_addons_alive(&root, &addons).await {
                        eprintln!(
                            "oj: native-addon keeper unavailable ({e}); a plugin host respawn that would re-register an orphaned addon will be refused"
                        );
                    }
                });
            }
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
        if let Ok(Some(raw)) = self.call("getBuildHookPlan", &[]).await {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&raw) {
                let plan = BuildHookPlan {
                    transform: HookFilterPlan::from_json(v.get("transform")),
                    load: HookFilterPlan::from_json(v.get("load")),
                    resolve_id: HookFilterPlan::from_json(v.get("resolveId")),
                };
                *self.hook_plan.write().unwrap() = plan;
                self.hook_plan_fetched.store(true, Ordering::Release);
            }
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
        let Some(v) = rpc
            .ok()
            .flatten()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        else {
            return ServeInfo::default();
        };
        ServeInfo::from_json(&v)
    }

    /// Subscribe to `{ ojServeInfo }`: `None` until init completes; lets the
    /// caller activate the plugin-middleware path late.
    pub fn serve_info_updates(&self) -> tokio::sync::watch::Receiver<Option<ServeInfo>> {
        self.serve_info_push.subscribe()
    }

    /// Plugins still active after oj filters out natively reimplemented ones.
    /// Defaults to 1 on RPC failure so an uncertain host is never dropped.
    pub async fn plugin_count(&self) -> usize {
        self.call("getPluginCount", &[])
            .await
            .ok()
            .flatten()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1)
    }

    /// `define` entries plugin `config()` hooks contributed, as (key, js
    /// expression) pairs, reaching oj's compile like Vite's merged `config.define`. Empty on RPC failure.
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

    /// Whether any active plugin has a `load` hook (Vite runs load before the fs
    /// read). Defaults to false on RPC failure (the fs read alone is always correct).
    pub async fn has_load(&self) -> bool {
        self.call("getHasLoad", &[])
            .await
            .ok()
            .flatten()
            .map(|s| s == "true")
            .unwrap_or(false)
    }

    /// `filter.code` include patterns of every object-form transform hook, as
    /// regex sources; dependency transforms are gated on these.
    pub async fn dep_transform_filters(&self) -> Vec<String> {
        let Ok(Some(raw)) = self.call("getDepTransformFilters", &[]).await else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
    }

    /// `filter.id` include patterns of every object-form `load` hook: deps cost
    /// no RPC unless a plugin asked for them.
    pub async fn dep_load_filters(&self) -> Vec<String> {
        let Ok(Some(raw)) = self.call("getDepLoadFilters", &[]).await else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
    }

    /// `filter.id` include patterns of every object-form `resolveId` hook: a
    /// non-bare import is offered to plugin resolveId only when it matches one.
    pub async fn resolve_id_filters(&self) -> Vec<String> {
        let Ok(Some(raw)) = self.call("getResolveIdFilters", &[]).await else {
            return Vec::new();
        };
        serde_json::from_str::<Vec<String>>(&raw).unwrap_or_default()
    }

    /// Which HMR hooks any active plugin defines: (watchChange, handleHotUpdate).
    /// Defaults to (true, true) on failure so an HMR RPC is never skipped by mistake.
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

    // Init-wait policy: a boot host waits out the long init deadline, a lazy one
    // only the short per-call bound.
    #[test]
    fn init_wait_policy_is_long_for_boot_hosts_and_short_for_lazy_ones() {
        let (boot_wait, boot_knob) = init_wait_policy(false);
        assert_eq!(boot_wait, plugin_init_timeout());
        assert_eq!(boot_knob, "OJ_PLUGIN_INIT_TIMEOUT");
        let (lazy_wait, lazy_knob) = init_wait_policy(true);
        assert_eq!(lazy_wait, plugin_rpc_timeout());
        assert_eq!(lazy_knob, "OJ_PLUGIN_TIMEOUT");
    }

    // A lazy host's init gate is per-call: a call past spawn + init_wait gets its
    // own full window; a boot host keeps the shared spawn-anchored deadline.
    #[test]
    fn lazy_call_past_the_spawn_deadline_gets_its_own_init_window() {
        let wait = std::time::Duration::from_secs(20);
        let spawned = tokio::time::Instant::now();
        // A call 40 s after spawn, with the 20 s window long since elapsed.
        let now = spawned + std::time::Duration::from_secs(40);
        let lazy = call_init_deadline(true, spawned, wait, now);
        assert_eq!(
            lazy,
            now + wait,
            "the lazy window anchors to the call's own start"
        );
        let boot = call_init_deadline(false, spawned, wait, now);
        assert_eq!(
            boot,
            spawned + wait,
            "the boot deadline stays shared and spawn-anchored"
        );
        assert!(
            boot <= now,
            "sanity: the boot deadline has elapsed for this call"
        );
    }

    // Only the FIRST report per generation declares the host gone; duplicates and
    // stale generations are dropped, a revived generation reports fresh.
    #[test]
    fn only_the_first_death_report_per_generation_declares_the_host_gone() {
        let mut revive = ReviveState {
            generation: 3,
            attempts: 0,
            last: None,
            pending_before: std::collections::HashSet::new(),
            reported: false,
        };
        assert!(first_death_report(&mut revive, 3), "the first report wins");
        assert!(
            !first_death_report(&mut revive, 3),
            "a concurrent belt's duplicate is dropped"
        );
        assert!(
            !first_death_report(&mut revive, 2),
            "a stale generation's report is dropped"
        );
        // A revive bumps the generation and re-arms reporting (try_revive).
        revive.generation += 1;
        revive.reported = false;
        assert!(
            !first_death_report(&mut revive, 3),
            "the dead generation stays declared"
        );
        assert!(
            first_death_report(&mut revive, 4),
            "the fresh generation reports its own death"
        );
        assert!(!first_death_report(&mut revive, 4), "once");
    }

    // An oj-side ssr key must not drop the extractor's verdict: per-key merge,
    // `runnerBacked` always adopted.
    #[test]
    fn merge_fills_ssr_per_key_and_always_adopts_runner_backed() {
        let mut config = oj_config::OjConfig {
            ssr: Some(serde_json::json!({ "noExternal": true })),
            ..Default::default()
        };
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
        assert_eq!(
            ssr["noExternal"],
            serde_json::json!(true),
            "the oj config's key wins"
        );
        assert_eq!(
            ssr["target"], "webworker",
            "extractor keys fill where oj lacks them"
        );
        assert_eq!(ssr["resolve"]["conditions"][0], "workerd");
        assert!(
            oj_config::ssr_runner_backed(&config),
            "the verdict survives an oj-side ssr key"
        );

        // runnerBacked is always the extractor's, even against a (stale)
        // oj-side value: only extraction produces it.
        let mut config = oj_config::OjConfig {
            ssr: Some(serde_json::json!({ "runnerBacked": false })),
            ..Default::default()
        };
        let v = ViteValues {
            ssr: Some(serde_json::json!({ "runnerBacked": true })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert!(oj_config::ssr_runner_backed(&config));
    }

    // The ssr merge recurses one level into `resolve`: oj-side sub-keys must not
    // drop the extractor's others (the workerd sugar's `conditions` above all).
    #[test]
    fn merge_recurses_one_level_into_ssr_resolve() {
        let mut config = oj_config::OjConfig {
            ssr: Some(serde_json::json!({ "resolve": { "externalConditions": ["oj-ext"] } })),
            ..Default::default()
        };
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

    // A non-object oj-side ssr (or ssr.resolve) cannot merge per-key: the
    // extractor block is adopted so the runnerBacked contract holds.
    #[test]
    fn merge_adopts_extractor_ssr_when_the_oj_side_is_not_an_object() {
        let mut config = oj_config::OjConfig {
            ssr: Some(serde_json::json!("bogus")),
            ..Default::default()
        };
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
        let mut config = oj_config::OjConfig {
            ssr: Some(serde_json::json!({ "resolve": "bogus" })),
            ..Default::default()
        };
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

    /// Spawn a healthy lazy host over a trivial plugins file and prove it serves
    /// a call: the shared setup of the revive tests.
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

    /// Rewind respawn spacing after an induced death: these tests target revive
    /// semantics, not keeper timing.
    fn clear_respawn_spacing(host: &PluginHost) {
        host.revive.lock().unwrap().last =
            Some(std::time::Instant::now() - PLUGIN_HOST_RESPAWN_SPACING);
    }

    // A wedge is no longer terminal: the next call revives the host with a
    // fresh engine generation through the same boot path, and serves.
    #[tokio::test]
    async fn a_gone_host_is_revived_by_the_next_call() {
        let (_root, host) = spawn_live_host("basic").await;
        let generation = host.revive.lock().unwrap().generation;
        host.declare_gone("test wedge", generation);
        clear_respawn_spacing(&host);
        assert!(*host.host_gone.borrow(), "the death latched");

        host.resolve_id("x", "")
            .await
            .expect("the next call revives the host and serves");
        let revive = host.revive.lock().unwrap();
        assert_eq!(
            revive.generation,
            generation + 1,
            "a fresh engine generation"
        );
        assert_eq!(revive.attempts, 1, "one respawn consumed");
        drop(revive);
        assert!(!*host.host_gone.borrow(), "the host is live again");
    }

    // A death report about a replaced engine (an old call's transport belt
    // firing after a revive) is stale and must not kill the new generation.
    #[tokio::test]
    async fn a_stale_death_report_does_not_kill_a_revived_host() {
        let (_root, host) = spawn_live_host("stale").await;
        let generation = host.revive.lock().unwrap().generation;
        host.declare_gone("test wedge", generation);
        clear_respawn_spacing(&host);
        host.resolve_id("x", "").await.expect("revived");

        host.declare_gone("stale report about the old engine", generation);
        assert!(
            !*host.host_gone.borrow(),
            "a stale-generation report is ignored"
        );
        host.resolve_id("x", "").await.expect("still serving");
    }

    // shutdown() retires the host on purpose: never revived.
    #[tokio::test]
    async fn a_shutdown_host_is_never_revived() {
        let (_root, host) = spawn_live_host("shutdown").await;
        host.shutdown();
        host.host_gone_wait().await;
        let err = host.resolve_id("x", "").await.expect_err("stays dead");
        assert!(err.contains("plugin host exited"), "{err}");
        assert_eq!(host.revive.lock().unwrap().attempts, 0, "no respawn burned");
    }

    // Keeper is best effort per addon: an unloadable path is skipped, the eval
    // succeeds, the keeper engine stays up for the next death.
    #[tokio::test]
    async fn addon_keeper_tolerates_unloadable_addons() {
        let root = std::env::temp_dir().join(format!("oj-keeper-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        keep_addons_alive(
            &root,
            &[
                PathBuf::from("/nonexistent/fake-binding.node"),
                PathBuf::from("/also/missing.node"),
            ],
        )
        .await
        .expect("the keeper load is best effort");
        assert!(
            ADDON_KEEPER.lock().unwrap().is_some(),
            "the keeper engine stays resident"
        );
    }

    // Heap cap mirrors NODE_OPTIONS --max-old-space-size (last wins, underscore
    // accepted, like Node), OJ_PLUGIN_MEMORY_MB above it, 4096 beneath.
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

    // A heap blow-up fails the running hook with MemoryLimit, declares the host
    // gone, and the next call revives it on a fresh heap.
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

        clear_respawn_spacing(&host);
        host.load("after")
            .await
            .expect("the next call revives the host on a fresh heap");
        assert_eq!(
            host.revive.lock().unwrap().attempts,
            1,
            "one respawn consumed"
        );
    }

    // Budget is a LIFETIME cap: past it the host stays gone, and attempts are
    // spaced so a burst cannot stack engines.
    #[tokio::test]
    async fn the_respawn_budget_is_finite_and_spaced() {
        let (_root, host) = spawn_live_host("budget").await;
        for round in 0..PLUGIN_HOST_RESPAWN_LIMIT {
            let generation = host.revive.lock().unwrap().generation;
            host.declare_gone("recurring test wedge", generation);
            if round == 0 {
                clear_respawn_spacing(&host);
            }
            // Immediately after a previous revive the spacing rejects the
            // attempt; backdate the clock instead of sleeping it out.
            if round > 0 {
                let err = host.resolve_id("x", "").await.expect_err("spacing rejects");
                assert!(err.contains("plugin host exited"), "{err}");
                host.revive.lock().unwrap().last =
                    Some(std::time::Instant::now() - PLUGIN_HOST_RESPAWN_SPACING);
            }
            host.resolve_id("x", "")
                .await
                .expect("revives within budget");
        }
        let generation = host.revive.lock().unwrap().generation;
        host.declare_gone("one wedge too many", generation);
        host.revive.lock().unwrap().last =
            Some(std::time::Instant::now() - PLUGIN_HOST_RESPAWN_SPACING);
        let err = host.resolve_id("x", "").await.expect_err("budget spent");
        assert!(err.contains("plugin host exited"), "{err}");
        assert!(!host.can_revive(), "no revive left for waiters to hold on");
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
        assert!(
            !*evidence.borrow_and_update(),
            "no evidence before a window expires"
        );

        // First call waits its full per-call window, then fails on the window,
        // flipping the evidence watch.
        let t0 = std::time::Instant::now();
        let first = host.resolve_id("x", "").await;
        let first_err = first.expect_err("init outlives the first call's window");
        assert!(first_err.contains("still initializing"), "{first_err}");
        assert!(
            t0.elapsed() >= std::time::Duration::from_millis(900),
            "the first call waits its full window, got {:?}",
            t0.elapsed()
        );
        assert!(
            *evidence.borrow_and_update(),
            "the expired window is wedge evidence"
        );

        // Later calls each keep their OWN full window (never the removed
        // fail-fast latch), so one is served the moment init lands.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let t = std::time::Instant::now();
            match host.resolve_id("x", "").await {
                Ok(_) => break,
                Err(e) => {
                    assert!(
                        e.contains("still initializing"),
                        "never a latched fail-fast: {e}"
                    );
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
    }

    // Submit-before-gate hazard, pinned: pre-init calls must submit NOTHING;
    // concurrent calls each fail at their own window, proving none queued on the wedged engine.
    #[tokio::test]
    async fn wedged_host_pre_init_calls_fail_at_their_window_without_submitting_jobs() {
        let root = std::env::temp_dir().join(format!("oj-wedged-stdin-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        // Init never completes; the interval keeps the event loop alive (the
        // shape that wedged the old node child).
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
    }

    // Stall monitor is the boot host's real evidence flip site: a wedge flips
    // evidence at the stall window with NO call in flight; a merely slow host flips then clears on init.
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
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(20), init.wait_for(|v| *v),)
                .await
                .is_ok_and(|r| r.is_ok())
        );
        host.shutdown();
    }

    // A hook wedging the isolate in SYNCHRONOUS JS is interrupted by the per-call
    // watchdog at the deadline: ONE call fails, the host survives.
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
    }

    // Transport belt: a hook blocked in NATIVE code stops the scheduler entirely,
    // so the belt declares the host GONE and the next call revives it; the blocked thread leaks until the block ends.
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
        // Gone but not terminally: the next call revives on a fresh generation.
        clear_respawn_spacing(&host);
        host.load("after")
            .await
            .expect("the next call revives the host and serves");
        assert_eq!(
            host.revive.lock().unwrap().attempts,
            1,
            "one respawn consumed"
        );
        // A second death inside the respawn spacing fails fast without a window.
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
    }

    // A hook that merely never SETTLES fails only that call at the per-call
    // deadline; only total scheduler silence declares the host gone.
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
    }

    // Plugin env writes land in a private shadow, NEVER in oj's real environment;
    // the host's cwd IS the app root and a plugin's chdir is contained to the shadow.
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
            std::env::var("OJ_HOST_CWD_BEFORE").is_err()
                && std::env::var("OJ_HOST_CWD_AFTER").is_err(),
            "a plugin's env write must never reach oj's real environment"
        );
        assert_ne!(
            std::env::current_dir().unwrap(),
            std::path::PathBuf::from("/"),
            "a plugin's chdir must not move oj's real cwd"
        );
        host.shutdown();
    }

    // Vite ordering: buildStart completes before any serving hook. The host is
    // never told to buildStart; the hook-entry gate must run it, even under concurrent first loads.
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
    }

    // Push channel end to end: { ojServeInfo } with the middleware port, ws send
    // on the broadcast, server.restart() on the server-events channel.
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

        // Senders were installed before init; configureServer ran before the
        // serve-info push, so both deliveries are already in.
        let payload = tokio::time::timeout(std::time::Duration::from_secs(10), ws_rx.recv())
            .await
            .expect("the ws push arrives")
            .expect("broadcast alive");
        assert!(
            payload.contains("oj:probe") && payload.contains("custom"),
            "{payload}"
        );
        let ev = tokio::time::timeout(std::time::Duration::from_secs(10), ev_rx.recv())
            .await
            .expect("the server event arrives")
            .expect("channel alive");
        assert_eq!(ev.get("action").and_then(|a| a.as_str()), Some("restart"));
        host.shutdown();
    }

    #[test]
    fn extraction_deps_truncated_gates_only_on_the_flag() {
        let t = serde_json::json!({ "__ok": true, "__depsTruncated": true });
        assert!(extraction_deps_truncated(&t));
        assert!(!extraction_deps_truncated(
            &serde_json::json!({ "__ok": true })
        ));
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
        let root =
            std::env::temp_dir().join(format!("oj-config-precedence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let order = ["js", "mjs", "ts", "cjs", "mts", "cts"];
        for ext in order.iter().rev() {
            std::fs::write(
                root.join(format!("vite.config.{ext}")),
                "export default {};",
            )
            .unwrap();
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
        let mut config = oj_config::OjConfig {
            base: Some("/oj-base/".into()),
            ..Default::default()
        };
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
    fn merge_fills_partial_server_and_optimizer_configuration() {
        let mut config: oj_config::OjConfig = serde_json::from_value(serde_json::json!({
            "server": { "fs": { "strict": false }, "warmup": { "clientFiles": ["own.ts"] } },
            "optimizeDeps": { "exclude": ["own-dep"] }
        }))
        .unwrap();
        merge_vite_values(
            &mut config,
            parse_vite_values(&serde_json::json!({
                "fsAllow": ["../shared"], "fsStrict": true,
                "serverFlags": { "fsDeny": ["**/*.private"], "warmup": {
                    "clientFiles": ["other.ts"], "ssrFiles": ["server.ts"] } },
                "optimizeDeps": { "include": ["dep"], "exclude": ["other-dep"], "noDiscovery": true,
                    "rolldownOptions": { "transform": { "define": { "FLAG": "true" }, "target": "es2015" } } }
            })),
        );
        let fs = config.server.as_ref().unwrap().fs.as_ref().unwrap();
        assert_eq!(fs.strict, Some(false));
        assert_eq!(fs.allow.as_ref().unwrap(), &["../shared"]);
        assert_eq!(fs.deny.as_ref().unwrap(), &["**/*.private"]);
        assert_eq!(
            oj_config::server_warmup_files(&config),
            (vec!["own.ts".into()], vec!["server.ts".into()])
        );
        let od = config.optimize_deps.as_ref().unwrap();
        assert_eq!(od.exclude.as_ref().unwrap(), &["own-dep"]);
        assert_eq!(od.include.as_ref().unwrap(), &["dep"]);
        assert_eq!(od.no_discovery, Some(true));
        let opts = oj_config::optimize_deps_bundler_options(&config).unwrap();
        assert_eq!(opts["define"]["FLAG"], "true");
        assert_eq!(opts["target"], "es2015");
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
        let v =
            parse_vite_values(&serde_json::json!({ "fsStrict": true, "fsAllow": ["../shared"] }));
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
        assert!(parse_vite_values(&serde_json::json!({ "build": null }))
            .build
            .is_none());
    }

    #[test]
    fn merge_adopts_build_fields_only_when_unset() {
        let mut config = oj_config::OjConfig {
            build: Some(oj_config::BuildConfig {
                out_dir: Some("oj-out".into()),
                ..Default::default()
            }),
            ..Default::default()
        };
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
        assert_eq!(
            b.target.as_ref().map(|t| t.to_vec()),
            Some(vec!["es2020".to_string()])
        );
        assert_eq!(
            b.ssr,
            Some(oj_config::BoolOrString::Str("src/server.ts".into()))
        );
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
        assert_eq!(
            oj_config::ssr_manifest_name(&config).as_deref(),
            Some(".vite/ssr-manifest.json")
        );
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
        assert_eq!(
            oj_config::build_sourcemap(&config),
            oj_config::Sourcemap::Hidden
        );
        assert!(oj_config::build_minify(&config));
        assert_eq!(
            oj_config::build_targets(&config),
            vec!["es2020", "safari14"]
        );
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

        let mut config = oj_config::OjConfig {
            oxc: Some(serde_json::json!({ "jsx": { "importSource": "preact" } })),
            ..Default::default()
        };
        let v = ViteValues {
            oxc: Some(serde_json::json!({ "jsx": { "importSource": "@emotion/react" } })),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(
            oj_config::jsx_settings(&config).import_source.as_deref(),
            Some("preact"),
            "oj.config wins"
        );
    }

    #[test]
    fn merge_adopts_ssr_block_when_unset() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            ssr: Some(
                serde_json::json!({ "noExternal": ["lodash-es", { "regex": "^@acme/" }], "external": ["sharp"] }),
            ),
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
            css: Some(
                serde_json::json!({ "preprocessorOptions": { "scss": { "additionalData": "@use 'x';" } } }),
            ),
            env_prefix: Some(vec!["VITE_".into(), "APP_".into()]),
            env_dir: Some("env".into()),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(config.mode.as_deref(), Some("staging"));
        let rc = config.resolve.as_ref().unwrap();
        assert_eq!(
            rc.extensions.as_deref(),
            Some(&[".ts".to_string(), ".js".to_string()][..])
        );
        assert_eq!(rc.main_fields.as_deref(), Some(&["module".to_string()][..]));
        assert_eq!(rc.conditions.as_deref(), Some(&["custom".to_string()][..]));
        assert_eq!(
            rc.external_conditions.as_deref(),
            Some(&["custom-ext".to_string()][..])
        );
        assert_eq!(rc.preserve_symlinks, Some(true));
        let sc = config.server.as_ref().unwrap();
        assert_eq!(sc.strict_port, Some(true));
        assert_eq!(sc.open, Some(true));
        let scss = &config
            .css
            .as_ref()
            .unwrap()
            .preprocessor_options
            .as_ref()
            .unwrap()["scss"];
        assert_eq!(scss.additional_data.as_deref(), Some("@use 'x';"));
        assert_eq!(
            oj_config::env_prefixes(&config),
            vec!["VITE_".to_string(), "APP_".to_string()]
        );
        assert_eq!(config.env_dir.as_deref(), Some("env"));

        // oj.config values win.
        let mut config = oj_config::OjConfig {
            mode: Some("qa".into()),
            ..Default::default()
        };
        config.env_dir = Some("cfg".into());
        merge_vite_values(
            &mut config,
            ViteValues {
                mode: Some("staging".into()),
                env_dir: Some("env".into()),
                ..Default::default()
            },
        );
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
        assert!(
            matches!(sc.cors, Some(oj_config::CorsConfig::Options(ref o)) if o.credentials == Some(true))
        );
        assert!(
            matches!(sc.allowed_hosts, Some(oj_config::AllowedHosts::List(ref l)) if l == &vec![".corp.example".to_string()])
        );
        let mut config = oj_config::OjConfig::default();
        merge_vite_values(
            &mut config,
            ViteValues {
                cors: Some(serde_json::json!(false)),
                allowed_hosts: Some(serde_json::json!(true)),
                ..Default::default()
            },
        );
        let sc = config.server.unwrap();
        assert!(matches!(
            sc.cors,
            Some(oj_config::CorsConfig::Toggle(false))
        ));
        assert!(matches!(
            sc.allowed_hosts,
            Some(oj_config::AllowedHosts::All(true))
        ));
    }

    #[test]
    fn merge_adopts_css_preprocessor_options() {
        let mut config = oj_config::OjConfig::default();
        let v = ViteValues {
            css: Some(
                serde_json::json!({ "preprocessorOptions": { "scss": { "additionalData": "$b: red;", "loadPaths": ["styles"] } } }),
            ),
            ..Default::default()
        };
        merge_vite_values(&mut config, v);
        assert_eq!(
            oj_config::css_additional_data(&config, "scss").as_deref(),
            Some("$b: red;")
        );
        assert_eq!(
            oj_config::css_load_paths(&config, "scss"),
            vec!["styles".to_string()]
        );
    }

    #[test]
    fn extraction_stderr_lines_print_once_per_process() {
        let first = unseen_extraction_lines(
            "oj: vite.config: worker config is not applied\nsome plugin notice\n",
        );
        assert_eq!(
            first,
            "oj: vite.config: worker config is not applied\nsome plugin notice\n"
        );
        let again = unseen_extraction_lines(
            "oj: vite.config: worker config is not applied\nsome plugin notice\nnew line\n",
        );
        assert_eq!(
            again, "new line\n",
            "only lines not printed before in this process come back"
        );
        assert_eq!(unseen_extraction_lines(""), "");
    }
}

// Extraction contract through the REAL in-process engine: one V8 isolate per
// case, serialized on one lock (env-knob tests must hold it too).
#[cfg(test)]
mod engine_extraction_tests {
    use super::*;

    static ENGINE_LOCK: Mutex<()> = Mutex::new(());

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        ENGINE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
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
        let dir = app(r#"import { base } from "./base.config.mjs";
export default { base };"#);
        let root = dir.path();
        std::fs::write(
            root.join("base.config.mjs"),
            "export const base = \"/dep/\";\n",
        )
        .unwrap();
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
        // Cap the recorder at one path; the config reads two .json files, so the
        // dep stamp is incomplete and the result must not be cached.
        std::env::set_var("OJ_OBSERVED_READS_MAX", "1");
        let dir = app(r#"import fs from "node:fs";
const a = JSON.parse(fs.readFileSync(new URL("./a.json", import.meta.url), "utf8"));
const b = JSON.parse(fs.readFileSync(new URL("./b.json", import.meta.url), "utf8"));
export default { base: a.base + b.base };"#);
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
        // A config that leaves timers behind (the TanStack route-generator shape)
        // must not stall the caller; the engine dies with them at drop.
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
        let dir = app(r#"console.error("plugin says hi");
process.stderr.write("direct stderr write\n");
console.log("stdout is swallowed");
export default { base: "/loud/" };"#);
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
        // The engine must shadow process.env (Vite's own NODE_ENV dance runs on
        // every extraction).
        let dir = app(r#"process.env.OJ_EXTRACT_LEAK_PROBE = "leaked";
export default { base: "/env/" };"#);
        let root = dir.path();
        let v = extract_vite_values_with(root, "serve", "development", true).unwrap();
        assert_eq!(v.base.as_deref(), Some("/env/"));
        assert!(
            std::env::var("OJ_EXTRACT_LEAK_PROBE").is_err(),
            "a config's env write must die with its extraction"
        );
    }

    // TS-config fallback (no vite installed): the extractor bundles the config
    // with the app's esbuild. Skips quietly when the fixture has no node_modules.
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
        std::fs::write(
            root.join("shared.ts"),
            "export const port: number = 5321;\n",
        )
        .unwrap();
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

    // Checked at the exact production seam: a module on the engine spawns a real
    // child process (as esbuild's JS API does) and reads it back.
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

    // The child talks through this envelope; a variant not surviving the round
    // trip would turn a child's deadline or JS error into a generic boot failure.
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

#[cfg(test)]
mod hook_plan_tests {
    use super::HookFilterPlan;

    fn plan(json: &str) -> HookFilterPlan {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        HookFilterPlan::from_json(Some(&v))
    }

    #[test]
    fn absent_hook_wants_nothing() {
        let p = plan(r#"{"present":false,"unfiltered":false,"plugins":[]}"#);
        assert!(!p.wants("/app/src/main.tsx", None));
    }

    #[test]
    fn unfiltered_hook_wants_everything() {
        let p = plan(r#"{"present":true,"unfiltered":true,"plugins":[]}"#);
        assert!(p.wants("anything", None));
    }

    #[test]
    fn id_filter_gates_by_module_id() {
        let p =
            plan(r#"{"present":true,"unfiltered":false,"plugins":[{"id":["\\.tsx$"],"code":[]}]}"#);
        assert!(p.wants("/app/src/App.tsx", None));
        assert!(!p.wants("/app/node_modules/react/index.js", None));
    }

    #[test]
    fn id_and_code_filters_of_one_plugin_both_apply() {
        let p = plan(
            r#"{"present":true,"unfiltered":false,"plugins":[{"id":["\\.js$"],"code":["import\\.meta\\.glob"]}]}"#,
        );
        assert!(p.wants("/a.js", Some("import.meta.glob(\"./x\")")));
        assert!(!p.wants("/a.js", Some("plain code")));
        assert!(!p.wants("/a.ts", Some("import.meta.glob(\"./x\")")));
        // No code available (load/resolveId shape): the code half must pass.
        assert!(p.wants("/a.js", None));
    }

    #[test]
    fn filters_union_across_plugins() {
        let p = plan(
            r#"{"present":true,"unfiltered":false,"plugins":[{"id":["\\.md$"],"code":[]},{"id":["\\.svg$"],"code":[]}]}"#,
        );
        assert!(p.wants("/doc.md", None));
        assert!(p.wants("/icon.svg", None));
        assert!(!p.wants("/main.ts", None));
    }

    #[test]
    fn case_insensitive_js_regex_carries_over() {
        let p = plan(
            r#"{"present":true,"unfiltered":false,"plugins":[{"id":["(?i)\\.SVG$"],"code":[]}]}"#,
        );
        assert!(p.wants("/icon.svg", None));
    }

    #[test]
    fn multiline_and_dotall_inline_flags_apply() {
        let p = plan(
            r#"{"present":true,"unfiltered":false,"plugins":[{"id":[],"code":["(?m)^import\\s"]}]}"#,
        );
        assert!(p.wants("/a.js", Some("// banner\nimport x from \"y\";")));
        let p =
            plan(r#"{"present":true,"unfiltered":false,"plugins":[{"id":[],"code":["(?s)a.b"]}]}"#);
        assert!(p.wants("/a.js", Some("a\nb")));
    }

    #[test]
    fn windows_ids_match_slash_normalized_like_the_host() {
        let p = plan(
            r#"{"present":true,"unfiltered":false,"plugins":[{"id":["/src/.*\\.tsx$"],"code":[]}]}"#,
        );
        assert!(p.wants(r"C:\app\src\App.tsx", None));
        assert!(!p.wants(r"C:\app\node_modules\x\index.js", None));
    }

    #[test]
    fn uncompilable_regex_fails_open_to_unfiltered() {
        // JS lookahead does not compile in the regex crate; the plan must then
        // treat that plugin as unfiltered rather than never offering it modules.
        let p = plan(
            r#"{"present":true,"unfiltered":false,"plugins":[{"id":["(?!never)x"],"code":[]}]}"#,
        );
        assert!(p.wants("/anything/at/all", None));
    }

    #[test]
    fn malformed_plan_fails_open() {
        let p = HookFilterPlan::from_json(None);
        assert!(p.wants("x", None));
        let p = plan(r#"{"plugins":"nope"}"#);
        assert!(p.wants("x", None));
    }

    #[test]
    fn empty_filter_entry_fails_open() {
        let p = plan(r#"{"present":true,"unfiltered":false,"plugins":[{"id":[],"code":[]}]}"#);
        assert!(p.wants("/anything", None));
    }
}
