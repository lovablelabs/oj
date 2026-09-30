use super::*;

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
    pub(crate) state: Arc<ServerState>,
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

pub(crate) async fn ssr_resolve_inner(
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

pub(crate) async fn ssr_resolve(
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

pub(crate) fn js_response_json(v: serde_json::Value) -> Response {
    ([(header::CONTENT_TYPE, "application/json")], v.to_string()).into_response()
}

pub(crate) fn module_read_allowed(
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

pub(crate) fn ssr_module_allowed(state: &ServerState, path: &Path) -> bool {
    let allow = state.fs_allow.lock().unwrap().clone();
    module_read_allowed(&state.root, &allow, path)
}

pub(crate) async fn ssr_module_inner(
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
pub(crate) async fn ssr_transform_source(
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
pub(crate) fn ssr_compile_source(
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
        Ok(out) => Ok(with_inline_map(out.code, out.map_json)),
        Err(e) => Err(SsrModuleError::Failed(format!("{e}"))),
    }
}

pub(crate) async fn ssr_module(
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

pub(crate) async fn ssr_plugin_host(state: &Arc<ServerState>) -> Option<std::sync::Arc<PluginHost>> {
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
pub(crate) struct SsrWatchQueue {
    pub(crate) backlog: Mutex<Vec<(String, String)>>,
    order: tokio::sync::Mutex<()>,
    /// One line for the first skip, not one per save.
    logged: std::sync::atomic::AtomicBool,
}

/// Records a watcher event the pre-init lazy SSR host cannot take yet (the
/// watcher's fast-skip; see [`SsrWatchQueue`]): deduped by file, the latest
/// change type winning — a delete after an update is what the replay must
/// report.
pub(crate) fn note_ssr_watch_skip(queue: &SsrWatchQueue, file: &str, change_type: &str) {
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
pub(crate) async fn replay_ssr_watch_backlog(host: &PluginHost, queue: &SsrWatchQueue) {
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
pub(crate) fn spawn_ssr_watch_catch_up(host: std::sync::Arc<PluginHost>, queue: Arc<SsrWatchQueue>) {
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

pub(crate) fn sass_additional_data_for(state: &ServerState, url: &str) -> Option<String> {
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
pub(crate) fn sass_load_paths_for(state: &ServerState, url: &str) -> Vec<PathBuf> {
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

pub(crate) fn ssr_css_module(root: &Path, path: &Path, source: &str) -> Result<String, String> {
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
