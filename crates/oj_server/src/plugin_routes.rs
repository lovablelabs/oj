use super::*;

// Rollup contract: a resolveId path IS the module, an empty `load` means read
// from disk. Redirect to the normal dep URL so fs.allow/bundling/rewriting apply.
pub(crate) fn serve_resolved_from_disk(state: &Arc<ServerState>, id: &str) -> Option<Response> {
    let resolved = Path::new(id);
    if !resolved.is_absolute() || !resolved.is_file() {
        return None;
    }
    state
        .fs_allow
        .lock()
        .unwrap()
        .insert(package_root(resolved));
    let url = url_of(&state.root, resolved);
    Some(Redirect::temporary(&url).into_response())
}

pub(crate) async fn serve_plugin_resolve(state: &Arc<ServerState>, id: &str) -> Response {
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
    let virtual_ids = Arc::clone(&state.virtual_ids);
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
            if let Some(url) = rewrite_specifier(
                &crate::rewrite::RewriteCtx {
                    root: &root,
                    dir: &root,
                    resolver: &resolver,
                    fs_allow: &fs_allow,
                    dir_cache: &dir_cache,
                },
                spec,
                true,
            ) {
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
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        browser_external_stub_source(spec),
    )
        .into_response()
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
        Ok(None) => {
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
            if let Some(u) = rewrite_specifier(
                &crate::rewrite::RewriteCtx {
                    root: &root,
                    dir: &root,
                    resolver: &resolver,
                    fs_allow: &fs_allow,
                    dir_cache: &dir_cache,
                },
                s,
                true,
            ) {
                return Some(u);
            }
            // A plugin virtual can import another plugin virtual: route bare
            // specifiers back through the plugin, never `virtual:...` to the browser.
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
            if let Some(u) = rewrite_specifier(
                &crate::rewrite::RewriteCtx {
                    root: &root,
                    dir: &root,
                    resolver: &resolver,
                    fs_allow: &fs_allow,
                    dir_cache: &dir_cache,
                },
                s,
                true,
            ) {
                return Some(u);
            }
            // A plugin virtual can import another plugin virtual: route bare
            // specifiers back through the plugin, never `virtual:...` to the browser.
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

pub(crate) fn is_cjs_dep_file(path: &Path) -> bool {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<PathBuf, bool>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(&v) = cache.lock().unwrap().get(path) {
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
    cache.lock().unwrap().insert(path.to_path_buf(), v);
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
