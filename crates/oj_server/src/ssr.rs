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
            inline: Box::new(|file, url| state.asset_urls.inline_url("ssr", file, url)),
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
            RawResolution::Path(p) | RawResolution::Claimed(p)
                if p.to_string_lossy().contains("/node_modules/") =>
            {
                StartResolution::Dependency(p)
            }
            RawResolution::Path(p) | RawResolution::Claimed(p) => {
                StartResolution::Module(p.to_string_lossy().into_owned())
            }
            RawResolution::PluginId(id) => StartResolution::Module(id),
            RawResolution::ExternalId(id) => StartResolution::Bare(id),
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
    /// A `pre` plugin's file other than the ssr resolver's: kept, not re-resolved.
    Claimed(PathBuf),
    PluginId(String),
    /// A `pre` plugin's external answer, imported by its id.
    ExternalId(String),
    /// Unresolved bare specifier, left to the runtime's own resolution.
    Bare,
}

/// The ssr plugin host's `pre` resolveId hooks for a bare import, then the
/// ssr resolver, then the host's `resolveId`, then a bare specifier passthrough.
async fn ssr_resolve_raw(
    state: &Arc<ServerState>,
    importer: &str,
    spec: &str,
) -> Result<RawResolution, String> {
    // The StyleX plugin's virtual runtimes, kept as the id (its resolveId).
    if crate::stylex::virtual_module(state, spec).is_some() {
        return Ok(RawResolution::PluginId(spec.to_string()));
    }
    if is_bare_specifier(spec) {
        if let Some(claimed) = ssr_pre_resolve(state, importer, spec).await {
            return Ok(claimed);
        }
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

/// Whether a resolved dependency file may be left to Node: Vite's
/// canExternalizeFile, `path.extname` included, which reads a query as part of
/// the extension (`x.css?url` is `.css?url`, never JavaScript).
pub fn can_externalize_file(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path);
    // `path.extname` keeps the dot: `file.` has extension `.`.
    let ext = match name.rfind('.') {
        Some(i) if i > 0 => &name[i..],
        _ => "",
    };
    matches!(ext, "" | ".js" | ".mjs" | ".cjs")
}

/// Vite runs `pre` resolveId hooks before its own resolver in SSR as in the
/// browser. An answer naming the ssr resolver's own file is dropped, so a
/// passthrough changes nothing.
async fn ssr_pre_resolve(
    state: &Arc<ServerState>,
    importer: &str,
    spec: &str,
) -> Option<RawResolution> {
    // Vite externalizes before any plugin resolves (shouldExternalize), and
    // never analyzes an externalized dependency's own imports.
    let importer_pkg = oj_config::SsrExternals::package_of_path(importer);
    if importer_pkg.is_some_and(|pkg| state.ssr_externals.is_external_pkg(pkg)) {
        return None;
    }
    let importer_dir = Path::new(importer).parent().unwrap_or(&state.root);
    let native = state.ssr_resolver.resolve(importer_dir, spec).ok();
    let externalized = |p: &PathBuf| {
        !matches_alias(&state.alias_finds, spec)
            && ssr_externalizes(&state.ssr_externals, spec, &p.to_string_lossy())
    };
    if native.as_ref().is_some_and(externalized) {
        return None;
    }
    let host = ssr_plugin_host(state).await?;
    let aliased = apply_alias(&state.aliases, spec);
    let filters = state.ssr_pre_resolve_id_res.as_ref();
    if filters.is_some_and(|res| {
        !res.iter()
            .any(|re| re.is_match(spec) || re.is_match(&aliased))
    }) {
        return None;
    }
    let answer = match host.resolve_id_pre(spec, importer, Some(&aliased)).await {
        Ok(answer) => answer?,
        Err(e) => {
            eprintln!("oj ssr: pre resolveId of {spec} from {importer} failed: {e}");
            return None;
        }
    };
    // Vite's import analysis keeps only a remote URL as is; any other answer,
    // `external` or not, the specifier itself or a queried id, is a module
    // the runner fetches (fetchModule externalizes no `/@id/` URL).
    if is_external_url(&answer.id) {
        return Some(RawResolution::ExternalId(answer.id));
    }
    if answer.id == spec || answer.id.contains('?') {
        return Some(RawResolution::PluginId(answer.id));
    }
    let path = Path::new(&answer.id);
    if !(path.is_absolute() && path.is_file()) {
        return Some(RawResolution::PluginId(answer.id));
    }
    let canonical = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    if native.is_some_and(|native| canonical(&native) == canonical(path)) {
        return None;
    }
    Some(RawResolution::Claimed(path.to_path_buf()))
}

/// Whether the ssr environment leaves `spec`, resolved to `path`, to Node:
/// Vite's createIsExternal for a dev environment (external.ts). Only a bare
/// specifier ever is. An exact `ssr.external` id always is; otherwise
/// `ssr.noExternal` keeps the package in the pipeline unless `ssr.external`
/// names it, and the file must pass [`can_externalize_file`], from
/// node_modules unless the package is configured external.
pub(crate) fn ssr_externalizes(
    externals: &oj_config::SsrExternals,
    spec: &str,
    path: &str,
) -> bool {
    // createIsExternal: an exact `ssr.external` id is external unless it is
    // relative or absolute (`#db` included), before the bareImportRE check.
    if spec.contains('\0') || spec.starts_with('.') || Path::new(spec).is_absolute() {
        return false;
    }
    if externals.external.iter().any(|e| e == spec) {
        return true;
    }
    if !is_bare_import(spec) {
        return false;
    }
    let externalizable = |configured: bool| {
        (configured || path.contains("/node_modules/")) && can_externalize_file(path)
    };
    // getNpmPackageName: an `@scope` with no name has none, and no list applies.
    let pkg = oj_config::SsrExternals::package_of(spec);
    if pkg.starts_with('@') && !pkg.contains('/') {
        return externalizable(false);
    }
    let listed = externals.external.iter().any(|e| e == pkg);
    // noExternal matches the package name only, as Vite's noExternalFilter.
    if !listed && externals.is_no_external(pkg) {
        return false;
    }
    externalizable(listed || externals.external_all)
}

/// Vite's bareImportRE, `^(?![a-zA-Z]:)[\w@](?!.*:\/\/)`: an ASCII word
/// character or `@` first, no drive letter, no `://`.
pub(crate) fn is_bare_import(spec: &str) -> bool {
    let mut chars = spec.chars();
    let (first, second) = (chars.next(), chars.next());
    let drive = first.is_some_and(|c| c.is_ascii_alphabetic()) && second == Some(':');
    first.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_' || c == '@')
        && !drive
        && !spec.contains("://")
}

/// One `resolve.builtins` entry of the ssr environment: an exact id, or a
/// RegExp.
#[derive(Clone, Debug)]
pub(crate) enum Builtin {
    Id(String),
    Pattern(regex::Regex),
}

impl Builtin {
    /// createIsBuiltin: an exact id, or the RegExp tests the id.
    pub(crate) fn matches(&self, id: &str) -> bool {
        match self {
            Builtin::Id(b) => b == id,
            Builtin::Pattern(re) => re.is_match(id),
        }
    }

    /// The extracted `ssr.builtins` (strings, `{ regex, flags }`), when the
    /// config resolved them. None for a runner-backed ssr environment: its
    /// list describes that runtime (workerd's `cloudflare:*`), and oj's Node
    /// SSR consumer keeps Vite's Node defaults, as it does for conditions.
    pub(crate) fn ssr_list(config: &oj_config::OjConfig) -> Option<Vec<Builtin>> {
        if oj_config::ssr_runner_backed(config) {
            return None;
        }
        let items = config.ssr.as_ref()?.get("builtins")?.as_array()?;
        Some(
            items
                .iter()
                .filter_map(|b| match b {
                    serde_json::Value::String(s) => Some(Builtin::Id(s.clone())),
                    serde_json::Value::Object(o) => {
                        let source = o.get("regex")?.as_str()?;
                        let flags = o.get("flags").and_then(|f| f.as_str()).unwrap_or("");
                        match js_regex(source, flags) {
                            Ok(re) => Some(Builtin::Pattern(re)),
                            Err(e) => {
                                eprintln!(
                                    "oj: resolve.builtins /{source}/{flags} is not supported and is skipped: {e}"
                                );
                                None
                            }
                        }
                    }
                    _ => None,
                })
                .collect(),
        )
    }
}

/// A JS RegExp as `regexp.test` runs it: `i`, `m` and `s` as their inline
/// flags, `y` (sticky, from `lastIndex` 0) anchored at the start; `g`, `u`, `d`
/// and `v` change nothing a single test sees.
pub(crate) fn js_regex(source: &str, flags: &str) -> Result<regex::Regex, regex::Error> {
    let inline: String = flags
        .chars()
        .filter(|f| matches!(f, 'i' | 'm' | 's'))
        .collect();
    let inline = if inline.is_empty() {
        String::new()
    } else {
        format!("(?{inline})")
    };
    let source = if flags.contains('y') {
        format!("^(?:{source})")
    } else {
        source.to_string()
    };
    regex::Regex::new(&format!("{inline}{source}"))
}

/// A builtin of the ssr environment, which Vite externalizes before any
/// `ssr.noExternal` or resolution (createIsExternal, fetchModule): its default
/// `resolve.builtins`, nodeLikeBuiltins, matched as createIsBuiltin does — an
/// exact `module.builtinModules` name, or the `node:` / `bun:` namespace. An
/// installed polyfill of the same name (`events`, `buffer`) never wins.
pub(crate) fn is_ssr_builtin(spec: &str) -> bool {
    const SUBPATHS: [&str; 12] = [
        "assert/strict",
        "dns/promises",
        "fs/promises",
        "inspector/promises",
        "path/posix",
        "path/win32",
        "readline/promises",
        "stream/consumers",
        "stream/promises",
        "stream/web",
        "timers/promises",
        "util/types",
    ];
    if spec.starts_with("node:") || spec.starts_with("bun:") {
        return true;
    }
    crate::is_node_builtin(spec) && (!spec.contains('/') || SUBPATHS.contains(&spec))
}

/// Vite's checkPublicFile: the file a root-relative path names under the
/// public dir, only when it exists and stays inside it (`/../x` never does).
pub(crate) fn public_file(state: &ServerState, rel: &str) -> Option<PathBuf> {
    let public = state.public_dir.as_deref()?;
    let clean = rel.split(['?', '#']).next().unwrap_or(rel);
    let file = crate::rewrite::normalize(&public.join(clean.trim_start_matches('/')));
    // `/@ssr-module` is an HTTP route, so `server.fs.deny` holds here too.
    (file.starts_with(public)
        && file.is_file()
        && !crate::module_pipeline::path_is_denied(&file, &state.root, &state.fs_deny))
    .then_some(file)
}

/// Vite's alias pattern matcher (preAlias.ts `matches`) over the top-level
/// `resolve.alias` finds: the find itself or anything under it.
pub(crate) fn matches_alias(finds: &[String], spec: &str) -> bool {
    finds.iter().any(|find| {
        if find.ends_with('/') {
            spec.starts_with(find.as_str())
        } else {
            spec == find
                || spec
                    .strip_prefix(find.as_str())
                    .is_some_and(|r| r.starts_with('/'))
        }
    })
}

/// `spec` after the first alias that matches it (the find itself, or its
/// prefix up to `/`), as @rollup/plugin-alias replaces it.
pub(crate) fn apply_alias(aliases: &[(String, String)], spec: &str) -> String {
    for (find, replacement) in aliases {
        if find.ends_with('/') {
            if let Some(rest) = spec.strip_prefix(find.as_str()) {
                return format!("{replacement}{rest}");
            }
        } else if spec == find {
            return replacement.clone();
        } else if let Some(rest) = spec
            .strip_prefix(find.as_str())
            .filter(|r| r.starts_with('/'))
        {
            return format!("{replacement}{rest}");
        }
    }
    spec.to_string()
}

/// Vite's safeModulePaths: the file an import of code the pipeline served
/// resolved to may load, wherever it is. This route is HTTP, so the import
/// must be one the importer's own code makes, never just what a request names.
fn record_safe_module_path(state: &ServerState, importer: &str, spec: &str, resolved: &Path) {
    let s = resolved.to_string_lossy();
    let file = Path::new(s.split('?').next().unwrap_or(&s));
    let importer_file = Path::new(importer.split('?').next().unwrap_or(importer));
    let imported = state
        .ssr_imports
        .lock()
        .unwrap()
        .get(importer_file)
        .is_some_and(|specs| specs.contains(spec));
    if imported && file.is_absolute() {
        crate::util::allow_root(&state.fs_allow, file.to_path_buf());
    }
}

pub(crate) async fn ssr_resolve_inner(
    state: &Arc<ServerState>,
    importer: &str,
    spec: &str,
) -> Result<SsrResolution, String> {
    // importAnalysis gates both the builtin and the external check on
    // `!matchAlias`: an aliased specifier always goes through the pipeline.
    let aliased = matches_alias(&state.alias_finds, spec);
    if !aliased {
        let builtin = match &state.ssr_builtins {
            Some(list) => list.iter().any(|b| b.matches(spec)),
            None => is_ssr_builtin(spec),
        };
        if builtin {
            return Ok(SsrResolution::External(spec.to_string()));
        }
    }
    // `/logo.png` with no such file under the root is a public file, whose id
    // stays root-relative as in Vite (its asset load reads checkPublicFile).
    if let Some(rel) = spec.strip_prefix('/').filter(|r| !r.starts_with('/')) {
        let clean = rel.split(['?', '#']).next().unwrap_or(rel);
        if !state.root.join(clean).is_file() && public_file(state, rel).is_some() {
            return Ok(SsrResolution::Module(spec.to_string()));
        }
    }
    Ok(match ssr_resolve_raw(state, importer, spec).await? {
        // A stylesheet, JSON, raw TypeScript or `x.css?url` is served like
        // the app's own, as is a `noExternal` package's file, and an aliased
        // specifier.
        RawResolution::Path(p)
            if !aliased && ssr_externalizes(&state.ssr_externals, spec, &p.to_string_lossy()) =>
        {
            SsrResolution::External(spec.to_string())
        }
        // A plugin's file is a module wherever it lives: Vite decides
        // externalization on the specifier, before any plugin resolves.
        RawResolution::Path(p) | RawResolution::Claimed(p) => {
            record_safe_module_path(state, importer, spec, &p);
            SsrResolution::Module(p.to_string_lossy().into_owned())
        }
        RawResolution::PluginId(id) => SsrResolution::Module(id),
        RawResolution::ExternalId(id) => SsrResolution::External(id),
        // Vite's alias plugin rewrites the specifier before anything else:
        // Node gets the target (`{ "my-fs": "node:fs" }` hands it `node:fs`).
        RawResolution::Bare if aliased => {
            SsrResolution::External(apply_alias(&state.aliases, spec))
        }
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
    allow
        .iter()
        .any(|allowed| candidate.starts_with(real(allowed)))
}

/// Vite's isFileLoadingAllowed: everything under `server.fs.strict: false`,
/// else the allowed roots, `server.fs.deny` aside. Unlike Vite, the deny list
/// holds under `strict: false` too, as in oj's browser gate.
pub(crate) fn ssr_module_allowed(state: &ServerState, path: &Path) -> bool {
    let real = std::fs::canonicalize(path).ok();
    if [Some(path), real.as_deref()]
        .into_iter()
        .flatten()
        .any(|p| crate::module_pipeline::path_is_denied(p, &state.root, &state.fs_deny))
    {
        return false;
    }
    if !state.fs_strict {
        return true;
    }
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
    // Checked on the file the id names, query aside: `x?raw` reads `x`.
    let named = Path::new(id.split('?').next().unwrap_or(id));
    if !ssr_module_allowed(state, named) {
        return Err(SsrModuleError::Forbidden(
            "oj: module not allow-listed".into(),
        ));
    }
    if let Some(code) = ssr_query_asset(state, id, runner).await? {
        return Ok(code);
    }
    // A stylesheet's `?inline` is its compiled text (vite:css-post's inline
    // export, ahead of the server consumer's class map).
    if let Some((file, query)) = id.split_once('?') {
        let sheet = Path::new(file);
        let is_sheet = sheet
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(is_style_ext);
        if is_sheet && has_word_param(&format!("?{query}"), "inline") && sheet.is_file() {
            return ssr_inline_style(state, id, sheet)
                .await
                .map_err(SsrModuleError::Failed);
        }
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

/// `?url`, `?raw` and `?inline` on a file: vite:asset's load, which answers
/// them in every environment, so the server render names the URL the browser
/// imports. A plugin that loads the id itself goes first.
async fn ssr_query_asset(
    state: &Arc<ServerState>,
    id: &str,
    runner: bool,
) -> Result<Option<String>, SsrModuleError> {
    let (file, query) = match id.split_once('?') {
        Some(pair) => pair,
        // A plain import of a public asset (`/logo.png`): vite:asset's load
        // names it as it names `?url`.
        None if id.starts_with('/')
            && !Path::new(id).is_file()
            && public_file(state, id).is_some_and(|f| is_asset_path(&f)) =>
        {
            (id, "url")
        }
        // A plain import of any other asset (`./logo.svg`) likewise.
        None if is_asset_path(Path::new(id)) && Path::new(id).is_file() => (id, "url"),
        None => return Ok(None),
    };
    let kind = match query_asset_kind(Some(query)) {
        Some(kind @ ("url" | "raw" | "inline")) => kind,
        _ => return Ok(None),
    };
    // A root-relative id under the public dir (Vite's checkPublicFile) is a
    // public file; a file on disk, wherever it is, is named by its own path.
    let public =
        public_file(state, file).filter(|_| file.starts_with('/') && !Path::new(file).is_file());
    let path = public.as_deref().unwrap_or(Path::new(file));
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    // A stylesheet's `?inline` is its compiled text, not the file's bytes.
    if !path.is_file() || (kind == "inline" && is_style_ext(ext)) {
        return Ok(None);
    }
    // `?inline` on a file vite:asset does not load is the plain module.
    if kind == "inline" && !is_asset_path(path) {
        // Every parameter `[?&]inline\b` reads as `inline` goes (`inline=1`
        // too), so the plain id never asks for `?inline` again.
        let rest: Vec<&str> = query
            .split('&')
            .filter(|kv| !has_word_param(&format!("?{kv}"), "inline"))
            .collect();
        let plain = match rest.is_empty() {
            true => file.to_string(),
            false => format!("{file}?{}", rest.join("&")),
        };
        if plain == id {
            return Ok(None);
        }
        return Box::pin(ssr_module_inner(state, &plain, runner))
            .await
            .map(Some);
    }
    if public.is_none() && !ssr_module_allowed(state, path) {
        return Err(SsrModuleError::Forbidden(
            "oj: module not allow-listed".into(),
        ));
    }
    if let Some(host) = ssr_plugin_host(state).await {
        if host.hook_wants_load(id) {
            if let Ok(Some(code)) = host.load(id).await {
                return ssr_transform_source(state, id, code, true, runner)
                    .await
                    .map(Some);
            }
        }
    }
    // A plain svg a transform plugin may componentize (svgr) goes through
    // the transforms, as the client's does; left as markup, it is a URL.
    let plain_svg = !id.contains('?') && ext == "svg" && state.plugins_have_transform;
    if plain_svg {
        if let Some(code) = ssr_svg_component(state, id, path, runner).await? {
            return Ok(Some(code));
        }
    }
    let url = match public {
        Some(_) => format!("{}?{query}", file),
        None => format!("{}?{query}", url_of(&state.root, path)),
    };
    let code = asset_module(path, &url, kind, &state.asset_urls, "ssr")
        .await
        .map_err(SsrModuleError::Failed)?;
    // vite:asset's load result takes the environment's transforms, as every
    // load does.
    if let Some(host) = ssr_plugin_host(state).await {
        if host.hook_wants_transform(id, &code) {
            return ssr_transform_source(state, id, code, true, runner)
                .await
                .map(Some);
        }
    }
    // The module runner takes every module in its transformed form.
    if runner {
        return ssr_compile_source(state, id, code, true, true).map(Some);
    }
    Ok(Some(code))
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

/// A plain svg's module once a transform plugin made it code (svgr), compiled
/// for the ssr environment; None when the transforms leave markup.
async fn ssr_svg_component(
    state: &Arc<ServerState>,
    id: &str,
    path: &Path,
    runner: bool,
) -> Result<Option<String>, SsrModuleError> {
    let Some(host) = ssr_plugin_host(state).await else {
        return Ok(None);
    };
    let source = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| SsrModuleError::Failed(format!("read {id}: {e}")))?;
    if !host.hook_wants_transform(id, &source) {
        return Ok(None);
    }
    let resolver = host
        .own_environment_resolver("ssr")
        .unwrap_or_else(|| Arc::clone(&state.ssr_resolver));
    let resolved = resolved_imports_json(&resolver, &state.fs_allow, &source, path, None);
    let code = host
        .transform(&source, id, &resolved)
        .await
        .map_err(|e| SsrModuleError::Failed(format!("oj: plugin transform error for {id}:\n{e}")))?
        .code;
    if code.trim_start().starts_with('<') {
        return Ok(None);
    }
    ssr_compile_source(state, id, code, true, runner).map(Some)
}

/// A stylesheet's `?inline` module in the SSR environment: the text the
/// client's `?inline` compiles (vite:css's url rewrite, PostCSS and Tailwind
/// included; the sheet is the same in every environment) as the default
/// export, through the ssr environment's "post" slot.
async fn ssr_inline_style(
    state: &Arc<ServerState>,
    id: &str,
    path: &Path,
) -> Result<String, String> {
    let url = format!("{}?inline", url_of(&state.root, path));
    let (_, module) = Box::pin(crate::module_pipeline::ensure_module(state, path, &url)).await?;
    let code = format!(
        "export default {}",
        serde_json::Value::String(module.code.clone())
    );
    let host = ssr_plugin_host(state).await;
    let slots = host.as_deref().map(|host| StyleSlots { host, id });
    style_slot(slots, code, "post").await
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
                .own_environment_resolver("ssr")
                .unwrap_or_else(|| Arc::clone(&state.ssr_resolver));
            let resolved =
                resolved_imports_json(&resolver, &state.fs_allow, &source, Path::new(id), None);
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
    let served = PathBuf::from(id.split('?').next().unwrap_or(id));
    let specs = oj_compiler::imports(&source, &compile_path)
        .into_iter()
        .collect();
    state.ssr_imports.lock().unwrap().insert(served, specs);
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
        inline: Box::new(|file, url| state.asset_urls.inline_url("ssr", file, url)),
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
