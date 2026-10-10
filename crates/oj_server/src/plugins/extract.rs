use super::*;

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
    /// `server.watch.ignored` string globs from the RESOLVED config: plugin
    /// `config` hooks add their own (vite-plugin-cloudflare ignores
    /// `**/.wrangler/**`, whose miniflare state writes would otherwise feed
    /// every request back into the watcher), and only the resolved list
    /// carries them.
    pub watch_ignored: Option<Vec<String>>,
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
pub(crate) fn extraction_timeout() -> std::time::Duration {
    extraction_timeout_from(oj_env::get().knobs.extract_timeout.as_deref())
}

pub(crate) fn extraction_timeout_from(raw: Option<&str>) -> std::time::Duration {
    let secs = raw
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .unwrap_or(60);
    std::time::Duration::from_secs(secs)
}

/// The oj executable one-shot engine jobs run in, set once by the binary's main.
/// A child per job isolates native addons that crash on re-init (napi-rs before 3.10).
pub(crate) static ENGINE_JOB_EXE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

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
        let engine = oj_js::JsEngine::spawn(config, None, None)?;
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| oj_js::EngineError::Boot(e.to_string()))?;
        rt.block_on(engine.call(
            module.to_string_lossy().into_owned(),
            export,
            vec![payload],
            None,
        ))
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

pub(crate) fn engine_job_outcome(
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

pub(crate) fn run_engine_job_subprocess(
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
pub fn extract_vite_values(
    root: &Path,
    config: Option<&Path>,
    command: &str,
    mode: &str,
) -> Option<ViteValues> {
    extract_vite_values_with(root, config, command, mode, true)
}

/// `mode_explicit`: false when `mode` is only the command's default (no CLI
/// `--mode`), which lets a `mode` named in the config file win, as in Vite.
pub(crate) fn extract_vite_values_with(
    root: &Path,
    config: Option<&Path>,
    command: &str,
    mode: &str,
    mode_explicit: bool,
) -> Option<ViteValues> {
    extract_vite_values_timed(
        root,
        config,
        command,
        mode,
        mode_explicit,
        extraction_timeout(),
    )
}

/// [`extract_vite_values_with`] bounded by `timeout` instead of `OJ_EXTRACT_TIMEOUT`.
pub(crate) fn extract_vite_values_timed(
    root: &Path,
    config: Option<&Path>,
    command: &str,
    mode: &str,
    mode_explicit: bool,
    timeout: std::time::Duration,
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
    let vite = vite_config_file(root, config)?;
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
    let script = materialize_extract_script(&cache)?;
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
    store_extraction(&store, &vite, command, mode_key, &json, &stderr);
    EXTRACTION_RAN_FRESH.store(true, std::sync::atomic::Ordering::Relaxed);
    crate::boot_phase("vite-extract cache miss (engine ran)");
    Some(parse_vite_values(&json))
}

/// Writes the extractor script into `cache`. Extractions run concurrently at
/// boot, so it lands via rename: a plain write truncates it under a concurrent
/// engine's import.
fn materialize_extract_script(cache: &Path) -> Option<PathBuf> {
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
    Some(script)
}

/// Records `__deps` and caches the result under the same (config, command,
/// mode_key) the lookup uses; a default-mode evaluation must not masquerade as
/// the explicit-mode entry.
fn store_extraction(
    store: &oj_cache::config_extract::ConfigExtractStore,
    vite: &Path,
    command: &str,
    mode_key: &str,
    json: &serde_json::Value,
    stderr: &str,
) {
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
    if extraction_deps_truncated(json) {
        // Read recorder hit its cap: `__deps` is incomplete, so serve the
        // result but never cache it.
        eprintln!(
            "oj: extracting {}: the config evaluation read more config-shaped files than the recorder tracks; result not cached",
            vite.display()
        );
        return;
    }
    // Stderr transcript lives in its own field, not inside the cached output.
    let mut stored = json.clone();
    if let Some(obj) = stored.as_object_mut() {
        obj.remove("__stderr");
    }
    store.store(vite, command, mode_key, &deps, &stored.to_string(), stderr);
}

/// Whether any config extraction ran the engine (cache miss): caches derived
/// from the evaluated config (the deps pre-seed stamp) must not serve either.
pub(crate) static EXTRACTION_RAN_FRESH: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn extraction_ran_fresh() -> bool {
    EXTRACTION_RAN_FRESH.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the extractor's read recorder overflowed (`__depsTruncated`): the
/// dep list is incomplete and the extraction must not be cached under it.
pub(crate) fn extraction_deps_truncated(json: &serde_json::Value) -> bool {
    json.get("__depsTruncated").and_then(|v| v.as_bool()) == Some(true)
}

/// Extractor stderr, printed once per process: the config is loaded several
/// times per dev session, each replaying the cached stderr; Vite prints once.
pub(crate) fn print_extraction_stderr(stderr: &str) {
    let fresh = unseen_extraction_lines(stderr);
    if !fresh.is_empty() {
        eprint!("{fresh}");
    }
}

pub(crate) fn unseen_extraction_lines(stderr: &str) -> String {
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
pub(crate) fn extraction_store(root: &Path) -> oj_cache::config_extract::ConfigExtractStore {
    oj_cache::config_extract::ConfigExtractStore::new(
        root,
        &format!(
            "{}:deno:{}:{}",
            env!("CARGO_PKG_VERSION"),
            blake3::hash(VITE_EXTRACT_JS.as_bytes()).to_hex(),
            extraction_env_hash(
                oj_env::get()
                    .vars()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
            )
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
pub(crate) static CONFIG_DEPS: std::sync::OnceLock<Vec<PathBuf>> = std::sync::OnceLock::new();

pub fn config_dependencies() -> &'static [PathBuf] {
    CONFIG_DEPS.get().map(Vec::as_slice).unwrap_or(&[])
}

fn str_field(json: &serde_json::Value, key: &str) -> Option<String> {
    json.get(key).and_then(|v| v.as_str()).map(str::to_string)
}

fn non_null(json: &serde_json::Value, key: &str) -> Option<serde_json::Value> {
    json.get(key).filter(|v| !v.is_null()).cloned()
}

/// The string entries of an array value (non-strings skipped).
fn string_list(v: &serde_json::Value) -> Option<Vec<String>> {
    v.as_array().map(|a| {
        a.iter()
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect()
    })
}

fn bool_or_string(v: Option<&serde_json::Value>) -> Option<oj_config::BoolOrString> {
    match v {
        Some(serde_json::Value::Bool(b)) => Some(oj_config::BoolOrString::Bool(*b)),
        Some(serde_json::Value::String(s)) => Some(oj_config::BoolOrString::Str(s.clone())),
        _ => None,
    }
}

fn string_or_list(v: Option<&serde_json::Value>) -> Option<oj_config::StringOrList> {
    match v {
        Some(serde_json::Value::String(s)) => Some(oj_config::StringOrList::One(s.clone())),
        Some(a @ serde_json::Value::Array(_)) => string_list(a).map(oj_config::StringOrList::Many),
        _ => None,
    }
}

#[inline]
pub(crate) fn parse_vite_values(json: &serde_json::Value) -> ViteValues {
    ViteValues {
        base: str_field(json, "base"),
        public_dir: match json.get("publicDir") {
            Some(serde_json::Value::String(s)) => Some(oj_config::BoolOrString::Str(s.clone())),
            Some(serde_json::Value::Bool(false)) => Some(oj_config::BoolOrString::Bool(false)),
            _ => None,
        },
        port: json.get("port").and_then(|v| v.as_u64()).map(|p| p as u16),
        host: str_field(json, "host"),
        hmr_disabled: json.get("hmr").and_then(|v| v.as_bool()) == Some(false),
        fs_allow: json.get("fsAllow").and_then(string_list),
        fs_strict: json.get("fsStrict").and_then(|v| v.as_bool()),
        watch_ignored: json.get("watchIgnored").and_then(string_list),
        define: json.get("define").and_then(|v| v.as_object()).cloned(),
        alias: json.get("alias").and_then(|v| v.as_object()).cloned(),
        headers: json.get("headers").and_then(|v| v.as_object()).cloned(),
        rollup_options: non_null(json, "rollupOptions"),
        assets_inline_limit: json.get("assetsInlineLimit").and_then(|v| v.as_u64()),
        proxy: non_null(json, "proxy"),
        dedupe: json.get("dedupe").and_then(string_list),
        optimize_deps: non_null(json, "optimizeDeps"),
        build: non_null(json, "build"),
        oxc: non_null(json, "oxc"),
        esbuild: non_null(json, "esbuild"),
        ssr: non_null(json, "ssr"),
        mode: str_field(json, "mode"),
        resolve: non_null(json, "resolve"),
        raw_resolve: non_null(json, "rawResolve"),
        server_flags: non_null(json, "serverFlags"),
        css: non_null(json, "css"),
        env_prefix: json.get("envPrefix").and_then(string_list),
        env_dir: str_field(json, "envDir"),
        cors: non_null(json, "cors"),
        allowed_hosts: non_null(json, "allowedHosts"),
        preview: non_null(json, "preview"),
        app_type: str_field(json, "appType"),
        html: non_null(json, "html"),
    }
}

#[inline]
pub fn adopt_vite_config_values(
    config: &mut oj_config::OjConfig,
    root: &Path,
    config_file: Option<&Path>,
    command: &str,
    mode: &str,
) -> Result<(), String> {
    let Some(v) = extract_vite_values(root, config_file, command, mode) else {
        return no_values_verdict(root, config_file);
    };
    merge_vite_values(config, v);
    Ok(())
}

/// Like `adopt_vite_config_values`, for a default `mode`: the config file's own
/// `mode` is honored and lands in `config.mode` so the caller can reload under it.
pub fn adopt_vite_config_values_default_mode(
    config: &mut oj_config::OjConfig,
    root: &Path,
    config_file: Option<&Path>,
    command: &str,
    mode: &str,
) -> Result<(), String> {
    let Some(v) = extract_vite_values_with(root, config_file, command, mode, false) else {
        return no_values_verdict(root, config_file);
    };
    merge_vite_values(config, v);
    Ok(())
}

/// No values extracted: no vite.config means nothing to adopt, but a
/// present-but-broken one fails hard like Vite. An oj.plugins file takes
/// precedence (the extractor skips it).
fn no_values_verdict(root: &Path, config_file: Option<&Path>) -> Result<(), String> {
    if let Some(named) = config_file {
        if !named.is_file() {
            return Err(format!(
                "failed to load config from {}: --config names a file that does not exist",
                named.display()
            ));
        }
    }
    if plugins_file(root).is_none() {
        if let Some(path) = vite_config_file(root, config_file) {
            return Err(format!("failed to load config from {}", path.display()));
        }
    }
    Ok(())
}

/// Fills every field oj's own config left unset from the vite.config values.
pub(crate) fn merge_vite_values(config: &mut oj_config::OjConfig, v: ViteValues) {
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
    merge_server_basics(
        config,
        ServerBasics {
            port: v.port,
            host: v.host,
            headers: v.headers,
            fs_allow: v.fs_allow,
            fs_strict: v.fs_strict,
        },
    );
    // Unioned rather than filled-if-unset: Vite's mergeConfig concatenates
    // `server.watch.ignored` arrays, and a user list in oj's own config must
    // not displace the plugin-added ignores (`**/.wrangler/**` et al).
    if let Some(vignored) = v.watch_ignored {
        if !vignored.is_empty() {
            let sc = config.server.get_or_insert_with(Default::default);
            let watch = sc.watch.get_or_insert_with(Default::default);
            let list = watch.ignored.get_or_insert_with(Default::default);
            for glob in vignored {
                if !list.contains(&glob) {
                    list.push(glob);
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
        merge_optimize_deps(config, od);
    }
    if let Some(vb) = v.build.as_ref().and_then(|b| b.as_object()) {
        merge_build_block(config.build.get_or_insert_with(Default::default), vb);
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
        merge_preview(config, preview);
    }
    // Before the server flags: a top-level `appType` wins over theirs.
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
    merge_ssr_block(config, v.ssr);
    if config.mode.is_none() {
        config.mode = v.mode;
    }
    if let Some(vr) = v.resolve.as_ref().and_then(|r| r.as_object()) {
        merge_resolve_block(config.resolve.get_or_insert_with(Default::default), vr);
    }
    if config.raw_resolve.is_none() {
        config.raw_resolve = v
            .raw_resolve
            .and_then(|r| serde_json::from_value::<oj_config::ResolveConfig>(r).ok());
    }
    if let Some(sf) = v.server_flags.as_ref().and_then(|s| s.as_object()) {
        merge_server_flags(config, sf);
    }
    if let Some(css) = v.css.as_ref() {
        merge_css(config, css);
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

/// The vite.config's `server.{port,host,headers,fs.allow,fs.strict}`.
struct ServerBasics {
    port: Option<u16>,
    host: Option<String>,
    headers: Option<serde_json::Map<String, serde_json::Value>>,
    fs_allow: Option<Vec<String>>,
    fs_strict: Option<bool>,
}

fn merge_server_basics(config: &mut oj_config::OjConfig, v: ServerBasics) {
    if v.port.is_none()
        && v.host.is_none()
        && v.headers.is_none()
        && v.fs_allow.is_none()
        && v.fs_strict.is_none()
    {
        return;
    }
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

fn merge_optimize_deps(config: &mut oj_config::OjConfig, od: serde_json::Value) {
    let Ok(parsed) = serde_json::from_value::<oj_config::OptimizeDepsConfig>(od) else {
        return;
    };
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

/// The extractor-normalized `build` block (see `extractBuild` in vite-extract.mjs).
fn merge_build_block(
    build: &mut oj_config::BuildConfig,
    vb: &serde_json::Map<String, serde_json::Value>,
) {
    let str_of = |k: &str| vb.get(k).and_then(|v| v.as_str()).map(str::to_string);
    let bool_of = |k: &str| vb.get(k).and_then(|v| v.as_bool());
    let bool_or_str = |k: &str| bool_or_string(vb.get(k));
    if build.out_dir.is_none() {
        build.out_dir = str_of("outDir");
    }
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
        build.target = string_or_list(vb.get("target"));
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
        build.css_target = string_or_list(vb.get("cssTarget"));
    }
    if build.lib.is_none() {
        build.lib = vb
            .get("lib")
            .cloned()
            .and_then(|l| serde_json::from_value::<oj_config::LibConfig>(l).ok());
    }
}

fn merge_preview(config: &mut oj_config::OjConfig, preview: serde_json::Value) {
    let Ok(parsed) = serde_json::from_value::<oj_config::PreviewConfig>(preview) else {
        return;
    };
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

/// ssr merges PER-KEY: `runnerBacked` (only extraction produces it) is always
/// adopted; `resolve` recurses one level so oj sub-keys keep the extractor's others.
fn merge_ssr_block(config: &mut oj_config::OjConfig, vssr: Option<serde_json::Value>) {
    let Some(vssr) = vssr else {
        return;
    };
    let Some(existing) = config.ssr.as_mut() else {
        config.ssr = Some(vssr);
        return;
    };
    if !existing.is_object() {
        // Nothing to merge per-key into; adopt the extractor block (keeps runnerBacked).
        eprintln!(
            "oj: config: the ssr block in oj's config is not an object; the vite.config ssr block is used"
        );
        *existing = vssr;
        return;
    }
    let (Some(obj), Some(vobj)) = (existing.as_object_mut(), vssr.as_object()) else {
        return;
    };
    for (k, val) in vobj {
        if k == "runnerBacked" || !obj.contains_key(k) {
            obj.insert(k.clone(), val.clone());
        } else if k == "resolve" {
            let Some(vsub) = val.as_object() else {
                continue;
            };
            if let Some(eobj) = obj.get_mut(k).and_then(serde_json::Value::as_object_mut) {
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

/// `resolve.{extensions,mainFields,conditions,externalConditions,preserveSymlinks}`.
fn merge_resolve_block(
    rc: &mut oj_config::ResolveConfig,
    vr: &serde_json::Map<String, serde_json::Value>,
) {
    let list = |k: &str| vr.get(k).and_then(string_list);
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

/// The extractor's `serverFlags`. Runs after the vite `fsStrict` value, so that
/// one wins and this is only a fallback.
fn merge_server_flags(
    config: &mut oj_config::OjConfig,
    sf: &serde_json::Map<String, serde_json::Value>,
) {
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
    if sc.forward_console.is_none() {
        sc.forward_console = sf.get("forwardConsole").cloned();
    }
    if sf.get("skipWebSocketTokenCheck").and_then(|b| b.as_bool()) == Some(true) {
        let legacy = config.legacy.get_or_insert_with(Default::default);
        if legacy.skip_web_socket_token_check.is_none() {
            legacy.skip_web_socket_token_check = Some(true);
        }
    }
}

/// With an oj-side css block only `preprocessorOptions.<lang>.additionalData`
/// fills in; otherwise the whole block (preprocessorOptions, devSourcemap,
/// modules) is adopted.
fn merge_css(config: &mut oj_config::OjConfig, css: &serde_json::Value) {
    let Some(cfg) = config.css.as_mut() else {
        config.css = serde_json::from_value::<oj_config::CssConfig>(css.clone()).ok();
        return;
    };
    let Some(po) = css.get("preprocessorOptions").and_then(|p| p.as_object()) else {
        return;
    };
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
