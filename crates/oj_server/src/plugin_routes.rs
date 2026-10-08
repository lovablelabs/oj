use super::*;

// Rollup contract: a resolveId path IS the module, an empty `load` means read
// from disk. Redirect to the normal dep URL so fs.allow/bundling/rewriting apply.
pub(crate) fn serve_resolved_from_disk(state: &Arc<ServerState>, id: &str) -> Option<Response> {
    let resolved = Path::new(id);
    if !resolved.is_absolute() || !resolved.is_file() {
        return None;
    }
    // Raw AND canonical, like every other allow-root insert: `fs_gate` checks
    // the request's canonical path, so a raw-only entry for a symlinked
    // resolution (a linked package, a macOS /var tmpdir) still 403'd.
    allow_root(&state.fs_allow, package_root(resolved));
    let url = url_of(&state.root, resolved);
    Some(Redirect::temporary(&url).into_response())
}

/// A `text/javascript`, `no-cache` response.
fn js_response(body: String) -> Response {
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// Loads a resolved plugin id, falling back to the file on disk.
async fn load_plugin_id(state: &Arc<ServerState>, id: &str) -> Result<String, Response> {
    let Some(host) = &state.plugins else {
        return Err((StatusCode::NOT_FOUND, "oj: no plugin host").into_response());
    };
    match host.load(id).await {
        Ok(Some(src)) => Ok(src),
        Ok(None) => Err(serve_resolved_from_disk(state, id).unwrap_or_else(|| {
            (StatusCode::NOT_FOUND, format!("oj: no plugin loaded {id}")).into_response()
        })),
        Err(e) => Err((StatusCode::INTERNAL_SERVER_ERROR, e).into_response()),
    }
}

/// Compiles plugin-loaded source. Imports go through `pre` first, then the normal
/// specifier rewrite. A plugin virtual can import another plugin virtual: leftover
/// bare specifiers route back through the plugin, never `virtual:...` to the browser.
async fn compile_plugin_source(
    state: &Arc<ServerState>,
    source: String,
    importer: String,
    pre: impl Fn(&str) -> Option<String> + Send + 'static,
) -> Result<String, String> {
    let root = state.root.clone();
    let resolver = Arc::clone(&state.resolver);
    let fs_allow = Arc::clone(&state.fs_allow);
    let dir_cache = Arc::clone(&state.dir_cache);
    let compile_opts = dev_compile_opts(state);
    tokio::task::spawn_blocking(move || {
        let mut rewrite = |spec: &str| {
            if let Some(url) = pre(spec) {
                return Some(url);
            }
            if let Some(url) =
                rewrite_specifier(&root, &root, &resolver, &fs_allow, &dir_cache, spec, true)
            {
                return Some(url);
            }
            is_bare_specifier(spec).then(|| {
                format!(
                    "/@id/{}?importer={}",
                    hex_encode(spec),
                    hex_encode(&importer)
                )
            })
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
    .await
    .unwrap_or_else(|e| Err(format!("compile task failed: {e}")))
}

fn compiled_response(compiled: Result<String, String>) -> Response {
    match compiled {
        Ok(code) => js_response(code),
        Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

pub(crate) async fn serve_plugin_resolve(state: &Arc<ServerState>, id: &str) -> Response {
    let source = match load_plugin_id(state, id).await {
        Ok(src) => src,
        Err(response) => return response,
    };
    let dep_map = state.optimized.ready().await;
    let virtual_ids = Arc::clone(&state.virtual_ids);
    let pre = move |spec: &str| {
        if virtual_ids.contains(spec) {
            return Some(format!("/@virtual/{spec}"));
        }
        dep_map
            .get(spec)
            .filter(|meta| !meta.needs_interop)
            .map(|meta| meta.url.clone())
    };
    compiled_response(compile_plugin_source(state, source, format!("\0{id}"), pre).await)
}

/// Vite's browser-externalized node builtin: a Proxy whose property reads warn
/// and yield undefined, skipping keys bundlers/interop helpers/devtools poke.
pub(crate) fn browser_external_stub_source(spec: &str) -> String {
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

pub(crate) fn browser_external_stub(spec: &str) -> Response {
    js_response(browser_external_stub_source(spec))
}

pub(crate) async fn serve_plugin_id(
    state: &Arc<ServerState>,
    spec: &str,
    importer: &str,
) -> Response {
    // A plugin may polyfill a node builtin, so the host gets first refusal;
    // otherwise the builtin is browser-externalized like Vite.
    let Some(host) = &state.plugins else {
        if is_node_builtin(spec) {
            return browser_external_stub(spec);
        }
        return (StatusCode::NOT_FOUND, "oj: no plugin host").into_response();
    };
    let id = match host.resolve_id(spec, importer).await {
        Ok(Some(id)) => id,
        Ok(None) => return serve_unclaimed_id(state, spec, importer),
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };
    let source = match load_plugin_id(state, &id).await {
        Ok(src) => src,
        Err(response) => return response,
    };
    compiled_response(compile_plugin_source(state, source, id, |_| None).await)
}

/// No plugin's resolveId claimed `spec`.
fn serve_unclaimed_id(state: &Arc<ServerState>, spec: &str, importer: &str) -> Response {
    // The plugin's resolveId declined a relative/absolute import:
    // resolve it against the importer like the native path would.
    if !is_bare_specifier(spec) {
        let (base, query) = spec.split_once('?').unwrap_or((spec, ""));
        let dir = Path::new(importer)
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| state.root.clone());
        if let Ok(abs) = state.resolver.resolve(&dir, base) {
            if abs.is_file() {
                allow_root(&state.fs_allow, package_root(&abs));
                let mut url = url_of(&state.root, &abs);
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
    // No plugin claimed the deferred bare id: Vite's "Failed to resolve
    // import" (500 + overlay naming the import site), not a bare 404.
    if is_bare_specifier(spec) && !importer.is_empty() {
        let importer_file = Path::new(importer);
        let source = std::fs::read_to_string(importer_file).unwrap_or_default();
        let err = unresolved_import_error(&state.root, importer_file, &source, spec);
        send_error(state, &err);
        return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response();
    }
    (
        StatusCode::NOT_FOUND,
        format!("oj: no plugin resolved {spec}"),
    )
        .into_response()
}

pub(crate) async fn serve_plugin_load_fallback(
    state: &Arc<ServerState>,
    uri: &Uri,
) -> Option<Response> {
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
        return Some(js_response(body));
    }
    compile_plugin_source(state, source, id, |_| None)
        .await
        .ok()
        .map(js_response)
}

pub(crate) fn is_bare_specifier(spec: &str) -> bool {
    !spec.starts_with('.') && !spec.starts_with('/') && !spec.contains("://")
}

// lingui macro entrypoints are normally compiled by @lingui/swc-plugin, which
// oj cannot run; oj serves a runtime identity shim instead.
pub(crate) fn is_lingui_macro_specifier(spec: &str) -> bool {
    matches!(
        spec,
        "@lingui/macro" | "@lingui/core/macro" | "@lingui/react/macro"
    )
}

pub(crate) fn is_cjs_dep_file(memo: &Mutex<HashMap<PathBuf, bool>>, path: &Path) -> bool {
    if let Some(&v) = memo.lock().unwrap().get(path) {
        return v;
    }
    // Only JS files can be CJS: a css/json/asset dep has no ESM syntax either,
    // but routing it to CJS interop would serve raw CSS as JS.
    let is_js = matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("js" | "cjs" | "jsx")
    );
    let v = is_js
        && match std::fs::read_to_string(path) {
            Ok(src) => !oj_compiler::cjs::has_module_syntax_pub(path, &src),
            Err(_) => false,
        };
    memo.lock().unwrap().insert(path.to_path_buf(), v);
    v
}

pub(crate) fn warn_lingui_macro_shim_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        eprintln!(
            "oj: @lingui/*/macro is served by a runtime identity shim (i18n renders \
             source strings, no catalog lookup). oj cannot run @lingui/swc-plugin, \
             which is what normally compiles these macros."
        );
    });
}
