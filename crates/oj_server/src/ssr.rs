use super::*;

/// How the SSR pipeline resolved an import: a module it serves, or an external
/// the runner imports from node_modules with its own Node resolution.
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

/// In-process access to the `/@ssr-resolve` / `/@ssr-module` pipeline for the
/// embedded runner; HTTP endpoints and runner share one implementation.
#[derive(Clone)]
pub struct SsrBridge {
    pub(crate) state: Arc<ServerState>,
}

impl SsrBridge {
    /// The dev server's engine registry, for hosts on the other side of the
    /// bridge to register their engines in (SSR, Start).
    pub fn engine_registry(&self) -> oj_js::EngineRegistry {
        self.state.engine_registry.clone()
    }

    /// Whether the generic pipeline ever served this file as a module (its
    /// module graph; a Start worker script fetched by URL lands here while
    /// staying outside both the client bundle and the Start engine's graph).
    pub fn graph_knows_file(&self, file: &Path) -> bool {
        let url = crate::rewrite::url_of(&self.state.root, file);
        self.state.graph.lock().unwrap().contains(Path::new(&url))
    }

    /// A style file as SSR compiles it (see [`compile_style_file`]): the
    /// sheet the Start dev server serves for it, after the "pre" transforms
    /// of `slots` (the client environment's, for a `<link>`).
    pub async fn style_sheet(
        &self,
        path: &Path,
        slots: Option<StyleSlots<'_>>,
    ) -> Result<StyleModule, String> {
        let source = read_style_source(slots, path).await?;
        let source = style_slot(slots, source, "pre").await?;
        style_file(&self.state, path, source).await
    }

    /// The prefix Start's dev urls take under the server's `base` (see
    /// [`crate::start_dev_url_base`]).
    pub fn start_url_base(&self) -> String {
        crate::start_dev_url_base(self.state.base.as_deref().unwrap_or("/"))
    }

    /// A stylesheet's `?inline` module under Start dev (see
    /// [`start_dev_inline_style`]), on the server's engines.
    /// The SSR environment's plugins see it as `<file>?inline`, in every slot.
    pub async fn start_dev_inline_style(&self, path: &Path) -> Result<String, String> {
        let state = &self.state;
        let host = ssr_plugin_host(state).await;
        let id = format!("{}?inline", path.display());
        let slots = host.as_deref().map(|host| StyleSlots { host, id: &id });
        let source = read_style_source(slots, path).await?;
        let engine = if needs_preprocess_engine(path) {
            Some(lazy_engine(state, EngineKind::Preprocess).await?.as_ref())
        } else {
            None
        };
        let inputs = StyleInputs {
            root: &state.root,
            css: state.css_config.as_ref(),
            resolve: state.css_resolve.as_ref(),
        };
        let tailwind = async || lazy_engine(state, EngineKind::Tailwind).await.cloned();
        let url_base = crate::start_dev_url_base(state.base.as_deref().unwrap_or("/"));
        let code =
            start_dev_inline_style(&inputs, path, source, engine, tailwind, &url_base, slots)
                .await?;
        style_slot(slots, code, "post").await
    }

    pub async fn resolve(&self, importer: &str, spec: &str) -> Result<SsrResolution, String> {
        ssr_resolve_inner(&self.state, importer, spec).await
    }

    /// The module's transformed code (plugins + dev-ssr compile), with its
    /// source map inline.
    pub async fn load_module(&self, id: &str) -> Result<String, SsrModuleError> {
        ssr_module_inner(&self.state, id, false).await
    }

    /// Like [`resolve`], but a node_modules hit keeps its RESOLVED path so the
    /// Start host uses the Vite-picked file, not a Node re-resolution.
    pub async fn resolve_start(
        &self,
        importer: &str,
        spec: &str,
    ) -> Result<StartResolution, String> {
        Ok(match ssr_resolve_raw(&self.state, importer, spec).await? {
            RawResolution::Path(p) if p.to_string_lossy().contains("/node_modules/") => {
                StartResolution::Dependency(p)
            }
            RawResolution::Path(p) => StartResolution::Module(p.to_string_lossy().into_owned()),
            RawResolution::PluginId(id) => StartResolution::Module(id),
            RawResolution::Bare => StartResolution::Bare(spec.to_string()),
        })
    }

    /// Plugin-transform + compile tail on a pre-read source; `run_plugins:
    /// false` skips the plugin chain and applies only the dev-ssr compile.
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

/// Start host resolution: a served module (fs path or virtual id), a resolved
/// node_modules file, or a bare specifier left to Node semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartResolution {
    Module(String),
    Dependency(PathBuf),
    Bare(String),
}

/// What the SSR resolve ladder found, before each caller classifies it.
enum RawResolution {
    Path(PathBuf),
    PluginId(String),
    /// Unresolved bare specifier, left to the runtime's own resolution.
    Bare,
}

/// The ssr resolver, then the ssr plugin host's `resolveId`, then a bare
/// specifier passthrough.
async fn ssr_resolve_raw(
    state: &Arc<ServerState>,
    importer: &str,
    spec: &str,
) -> Result<RawResolution, String> {
    // The StyleX plugin's virtual runtimes, kept as the id (its resolveId).
    if crate::stylex::virtual_module(state, spec).is_some() {
        return Ok(RawResolution::PluginId(spec.to_string()));
    }
    let importer_dir = Path::new(importer).parent().unwrap_or(&state.root);
    let err = match state.ssr_resolver.resolve(importer_dir, spec) {
        Ok(p) => return Ok(RawResolution::Path(p)),
        Err(e) => e,
    };
    if let Some(host) = ssr_plugin_host(state).await {
        if let Ok(Some(id)) = host.resolve_id(spec, importer).await {
            return Ok(RawResolution::PluginId(id));
        }
    }
    if !spec.starts_with('.') && !spec.starts_with('/') {
        return Ok(RawResolution::Bare);
    }
    Err(format!("cannot resolve {spec}: {}", err.reason))
}

pub(crate) async fn ssr_resolve_inner(
    state: &Arc<ServerState>,
    importer: &str,
    spec: &str,
) -> Result<SsrResolution, String> {
    Ok(match ssr_resolve_raw(state, importer, spec).await? {
        RawResolution::Path(p) => {
            let s = p.to_string_lossy();
            // A dependency stylesheet or JSON file is served like the app's
            // own: Vite only externalizes JavaScript (canExternalizeFile), and
            // the runtime would parse a stylesheet as a module.
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("");
            if s.contains("/node_modules/") && !is_style_ext(ext) && ext != "json" {
                SsrResolution::External(spec.to_string())
            } else {
                SsrResolution::Module(s.into_owned())
            }
        }
        RawResolution::PluginId(id) => SsrResolution::Module(id),
        RawResolution::Bare => SsrResolution::External(spec.to_string()),
    })
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
    // The StyleX plugin's virtual runtimes (its load), in every environment.
    if let Some(code) = crate::stylex::virtual_module(state, id) {
        return ssr_compile_source(state, id, code.to_string(), true, runner);
    }
    let path = PathBuf::from(id);
    if !ssr_module_allowed(state, &path) {
        return Err(SsrModuleError::Forbidden(
            "oj: module not allow-listed".into(),
        ));
    }
    let ext = path.extension().and_then(|e| e.to_str());
    // A sheet a plugin serves with no file behind it compiles like a file.
    let served = || async {
        ssr_plugin_host(state)
            .await
            .is_some_and(|host| host.hook_wants_load(id))
    };
    if ext.is_some_and(is_style_ext) && (path.is_file() || served().await) {
        let on_disk = path.is_file();
        return ssr_style_module(state, id, &path).await.map_err(|e| {
            // No file and no plugin serving it: missing, as before.
            if on_disk {
                SsrModuleError::Failed(e)
            } else {
                SsrModuleError::NotFound(e)
            }
        });
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
    if !from_plugin && ext == Some("json") {
        return oj_compiler::json::to_esm(&source, id)
            .map_err(|e| SsrModuleError::Failed(format!("{e}")));
    }
    ssr_transform_source(state, id, source, from_plugin, runner).await
}

/// A stylesheet as the SSR environment's module: its class map, through that
/// environment's plugin slots ("pre" on the source, "normal" on the sheet,
/// "post" on the module), as Vite's SSR runs vite:css and vite:css-post.
async fn ssr_style_module(
    state: &Arc<ServerState>,
    id: &str,
    path: &Path,
) -> Result<String, String> {
    let host = ssr_plugin_host(state).await;
    let slots = host.as_deref().map(|host| StyleSlots { host, id });
    let source = read_style_source(slots, path).await?;
    let source = style_slot(slots, source, "pre").await?;
    let module = style_file(state, path, source).await?;
    style_slot(slots, module.css, "normal").await?;
    style_slot(slots, module.code, "post").await
}

/// Plugin-transform + dev-ssr-compile tail of the SSR module pipeline, on a
/// source the caller already has.
pub(crate) async fn ssr_transform_source(
    state: &Arc<ServerState>,
    id: &str,
    source: String,
    from_plugin: bool,
    runner: bool,
) -> Result<String, SsrModuleError> {
    let host = ssr_plugin_host(state).await;
    // StyleX ahead of the app's transforms, as the client path runs it.
    let pre_stylex = match &host {
        Some(host) if host.hook_wants_transform(id, &source) => {
            crate::stylex::pre_transform(state, id, &source).map_err(SsrModuleError::Failed)?
        }
        _ => None,
    };
    let (source, stylex_rules) = match pre_stylex {
        Some(pre) => (pre.code, Some(pre.rules)),
        None => (source, None),
    };
    let source = match host {
        // Same filter gate as the client path: an unclaimed module skips the
        // isolate RPC and the import pre-resolution that only feeds it.
        Some(host) if host.hook_wants_transform(id, &source) => {
            // The ssr environment's plugin-facing resolver, as `this.resolve` there.
            let resolver = host
                .environment_resolver("ssr", true)
                .unwrap_or_else(|| Arc::clone(&state.ssr_resolver));
            let resolved =
                resolved_imports_json(&resolver, &state.fs_allow, &source, Path::new(id));
            match host.transform(&source, id, &resolved).await {
                Ok(t) => t.code,
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
    compile_ssr(state, id, source, from_plugin, runner, stylex_rules)
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
    compile_ssr(state, id, source, from_plugin, runner, None)
}

/// [`ssr_compile_source`]; `stylex_rules` when StyleX already compiled the
/// source (ahead of the app's plugins), which the compile then skips.
fn compile_ssr(
    state: &Arc<ServerState>,
    id: &str,
    source: String,
    from_plugin: bool,
    runner: bool,
    stylex_rules: Option<Vec<oj_stylex::rules::StylexRule>>,
) -> Result<String, SsrModuleError> {
    // A module a plugin `load`ed compiles under a synthetic path, but StyleX
    // derives its hashes from the file: it runs on the real id, as the
    // client's compile does.
    let (source, stylex_rules) = match stylex_rules {
        None if from_plugin => match crate::stylex::pre_transform(state, id, &source)
            .map_err(SsrModuleError::Failed)?
        {
            Some(pre) => (pre.code, Some(pre.rules)),
            None => (source, None),
        },
        rules => (source, rules),
    };
    let compile_path: PathBuf = if from_plugin {
        PathBuf::from("virtual.tsx")
    } else {
        PathBuf::from(id)
    };
    // Compile as dev + ssr so `import.meta.env.SSR` is true and `DEV`/`MODE`
    // match the client; Fast Refresh stays off on the server.
    let mut opts = dev_compile_opts(state);
    opts.refresh = false;
    opts.ssr = true;
    if stylex_rules.is_some() {
        opts.stylex = None;
    }
    let out = oj_compiler::compile(&compile_path, &source, &opts)
        .map_err(|e| SsrModuleError::Failed(format!("{e}")))?;
    // The plugin collects every environment's rules into the one sheet, by
    // module id: the file's path, or what a plugin `load`ed it as.
    crate::stylex::record_id(
        state,
        id,
        stylex_rules.as_deref().unwrap_or(&out.stylex_rules),
    );
    if !from_plugin {
        let file = id.split('?').next().unwrap_or(id);
        state
            .ssr_modules
            .lock()
            .unwrap()
            .insert(PathBuf::from(file));
    }
    let code = if runner {
        oj_compiler::ssr::ssr_transform(&out.code, &compile_path)
    } else {
        out.code
    };
    Ok(with_inline_map(code, out.map_json))
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

/// The ssr environment's container on the dev server's plugin host (opened
/// at boot): the same plugin instances as the client's (Vite's one plugin
/// array), the framework plugins oj reimplements left out under Start.
pub(crate) async fn ssr_plugin_host(
    state: &Arc<ServerState>,
) -> Option<std::sync::Arc<PluginHost>> {
    state.plugins_ssr.clone()
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

/// A style file compiled with the server's config (see [`compile_style_file`]),
/// booting the preprocess engine when the file needs it.
pub(crate) async fn style_file(
    state: &ServerState,
    path: &Path,
    source: String,
) -> Result<StyleModule, String> {
    let engine = if needs_preprocess_engine(path) {
        Some(lazy_engine(state, EngineKind::Preprocess).await?.as_ref())
    } else {
        None
    };
    let inputs = StyleInputs {
        root: &state.root,
        css: state.css_config.as_ref(),
        resolve: state.css_resolve.as_ref(),
    };
    compile_style_file(&inputs, path, source, engine).await
}

/// Vite's `css.lightningcss` as the build minify applies it.
pub fn css_lightning_options(config: &oj_config::OjConfig) -> oj_css::LightningOptions {
    config
        .css
        .as_ref()
        .and_then(|c| c.lightningcss.as_ref())
        .map(oj_css::LightningOptions::from_json)
        .unwrap_or_default()
}

/// Vite's `css.modules` options as the CSS compiler applies them (dev and build).
pub fn css_modules_options(config: &oj_config::OjConfig) -> oj_css::CssModulesOptions {
    let m = oj_config::css_modules(config);
    oj_css::CssModulesOptions {
        locals_convention: m.locals_convention,
        generate_scoped_name: m.generate_scoped_name,
        global_scope: m.global_scope,
        global_module_paths: m.global_module_paths,
        hash_prefix: m.hash_prefix,
        // postcss-modules' `process.cwd()`: where oj was started, as Vite.
        context: oj_env::get().cwd.clone(),
    }
}
