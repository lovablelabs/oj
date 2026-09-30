use super::*;

// Rollup's contract, which the rest of this host follows: `resolveId` returning a
// path means that file IS the module, and a `load` returning nothing means read it
// from disk. Only the second half was implemented, so a plugin that maps a
// specifier to a path without also serving its bytes -- which is what a resolver
// plugin is -- got a 404 for every module it resolved correctly.
//
// Redirecting to the file's normal dep URL rather than reading it here keeps every
// downstream behaviour identical to any other dependency: the fs.allow check,
// partial bundling, and the specifier rewriting applied inside the served file.
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
    let url = dep_serve_url(resolved, &state.root);
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
    let virtual_ids: std::collections::BTreeSet<String> =
        state.virtual_modules.keys().cloned().collect();
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
            if let Some(url) =
                rewrite_specifier(&root, &root, &resolver, &fs_allow, &dir_cache, spec, true)
            {
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

/// Vite's browser-externalized module for a node builtin that reaches the client
/// graph (optimizer rolldownDepPlugin `browser-external` load): a Proxy whose
/// property reads console.warn `Module "fs" has been externalized for browser
/// compatibility. Cannot access "fs.readFileSync" in client code.` and yield
/// undefined, so the app still mounts and the developer learns which dep pulled
/// the builtin in. Skips the keys bundlers, interop helpers and devtools poke.
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

pub(crate) async fn serve_plugin_id(state: &Arc<ServerState>, spec: &str, importer: &str) -> Response {
    // A plugin may polyfill a node builtin (vite-plugin-node-polyfills), so the
    // host gets first refusal; with no host, or when no plugin claims it, the
    // builtin is browser-externalized like Vite does.
    let Some(host) = &state.plugins else {
        if is_node_builtin(spec) {
            return browser_external_stub(spec);
        }
        return (StatusCode::NOT_FOUND, "oj: no plugin host").into_response();
    };
    let id = match host.resolve_id(spec, importer).await {
        Ok(Some(id)) => id,
        Ok(None) => {
            // A relative / absolute import routed here for a plugin's resolveId
            // filter that then declined it: Vite's own resolver takes over, so
            // resolve it against the importer like the native path would have.
            if !is_bare_specifier(spec) {
                let (base, query) = spec.split_once('?').unwrap_or((spec, ""));
                let dir = Path::new(importer)
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| state.root.clone());
                if let Ok(abs) = state.resolver.resolve(&dir, base) {
                    if abs.is_file() {
                        state.fs_allow.lock().unwrap().insert(package_root(&abs));
                        let mut url = dep_serve_url(&abs, &state.root);
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
            // No plugin claimed the bare id the importer deferred here: this is
            // Vite's "Failed to resolve import" for that importer (500 + overlay
            // naming the import site), not a bare 404 the browser reports as a
            // generic module error.
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
            if let Some(u) =
                rewrite_specifier(&root, &root, &resolver, &fs_allow, &dir_cache, s, true)
            {
                return Some(u);
            }
            // A plugin-loaded virtual can import another plugin virtual (the i18n
            // message groups import their `virtual:i18n-facade/*` counterpart). Route
            // bare specifiers back through the plugin like the on-disk compile path
            // does, instead of leaving `virtual:...` for the browser to fetch and fail.
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

// A plugin can `load` a module whose id is neither an on-disk file nor a bare
// specifier: wyw-in-js/linaria appends `import "<abs>.wyw-in-js.css"` to each
// transformed module and serves that absolute-path id from its own `load` hook,
// keeping the extracted CSS in memory. On a disk miss, consult the plugin
// container (resolveId -> load) before giving up. CSS a plugin returns is
// wrapped as a style-injecting JS module, matching Vite's `vite:css` handling of
// a `.css` import reached from JS (so the browser gets text/javascript, not a
// text/css module script the strict MIME check rejects).
// Serve a `/@oj-pkg/<hex>` package bundle: one request covering a CommonJS
// package's whole internal file graph (oj-native partial bundling). A package
// that can't be bundled in v1 (ESM entry, unsupported files) falls back to the
// entry's normal per-file compiled output, served at this same URL so the
// importer's interop (which reads __cjs_exports) still resolves.
pub(crate) async fn serve_pkg_bundle(state: &Arc<ServerState>, path: &str, versioned: bool) -> Response {
    let js = |code: Bytes| {
        (
            [
                (header::CONTENT_TYPE, "text/javascript"),
                (header::CACHE_CONTROL, dep_cache_control(versioned)),
            ],
            code,
        )
            .into_response()
    };
    // A chunk emitted by a previous rolldown fallback (a code-split sibling, or
    // the entry re-served). These paths aren't decodable entry hexes, so they
    // must be checked before entry_from_url.
    if let Some(code) = pkg_rolldown::cached_chunk(path) {
        return js(code);
    }
    if let Some(code) = pkg_bundle::cached(path) {
        return js(code);
    }
    let Some(entry) = pkg_bundle::entry_from_url(path) else {
        return (StatusCode::NOT_FOUND, "oj: bad pkg bundle path").into_response();
    };
    let resolver = Arc::clone(&state.resolver);
    let root = state.root.clone();
    // Known-hard packages (e.g. object-inspect) produce a concatenator bundle
    // that builds but breaks at runtime, so they never bail into the fallback.
    // Force those straight through rolldown, bypassing the concatenator.
    if pkg_rolldown::enabled() && pkg_rolldown::is_forced(&entry) {
        if let Some(code) = pkg_rolldown::build(&entry, &state.root, Arc::clone(&resolver)).await {
            return js(code);
        }
    }
    let entry_owned = entry.clone();
    let build_resolver = Arc::clone(&resolver);
    let outcome = tokio::task::spawn_blocking(move || {
        pkg_bundle::build(&entry_owned, build_resolver.as_ref(), &root)
    })
    .await;
    match outcome {
        Ok(pkg_bundle::BundleOutcome::Bundle(code)) => {
            let code = Bytes::from(code);
            pkg_bundle::store(path, code.clone());
            js(code)
        }
        Ok(pkg_bundle::BundleOutcome::Fallback) => {
            // The concatenator bailed. Before serving per-file, try bundling this
            // one package with rolldown (the robust path, Vite-style), if enabled.
            if pkg_rolldown::enabled() {
                if let Some(code) =
                    pkg_rolldown::build(&entry, &state.root, Arc::clone(&resolver)).await
                {
                    return js(code);
                }
                if std::env::var("OJ_PB_DEBUG").is_ok_and(|v| !v.is_empty() && v != "0") {
                    eprintln!("oj[pb] rolldown fallback failed, serving per-file: {path}");
                }
            }
            let url = url_of(&state.root, &entry);
            match ensure_module(state, &entry, &url).await {
                Ok((_, module)) => js(module.code.clone().into()),
                Err(e) => (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {e}")).into_response(),
            }
        }
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("oj: pkg bundle task failed: {e}"),
        )
            .into_response(),
    }
}

pub(crate) async fn serve_plugin_load_fallback(state: &Arc<ServerState>, uri: &Uri) -> Option<Response> {
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
            if let Some(u) =
                rewrite_specifier(&root, &root, &resolver, &fs_allow, &dir_cache, s, true)
            {
                return Some(u);
            }
            // A plugin-loaded virtual can import another plugin virtual (the i18n
            // message groups import their `virtual:i18n-facade/*` counterpart). Route
            // bare specifiers back through the plugin like the on-disk compile path
            // does, instead of leaving `virtual:...` for the browser to fetch and fail.
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

// The lingui macro entrypoints. Their transform is done by @lingui/swc-plugin
// (an SWC WASM plugin oj cannot run); left untransformed they drag the babel
// macro toolchain into the browser. oj serves a runtime identity shim instead.
pub(crate) fn is_lingui_macro_specifier(spec: &str) -> bool {
    matches!(
        spec,
        "@lingui/macro" | "@lingui/core/macro" | "@lingui/react/macro"
    )
}

// Whether a resolved dependency file is CommonJS (no ESM syntax), i.e. it will
// be served through wrap_cjs with `default` = module.exports. Used to decide
// whether a bare `import { x } from "cjs-dep"` needs importer-side interop
// (rewriting the named import to a property read off the default), since a CJS
// dep whose named exports are assigned at runtime (e.g. file-saver's `saveAs`)
// exposes no static ESM named bindings. Cached: a file's module kind is stable.
// A node_modules JS-family file that partial bundling should try to collapse
// into one `/@oj-pkg` bundle, whether it's CommonJS or ESM. (`.css`/`.json`/asset
// deps stay per-file; the builder itself falls back if a JS package can't be
// bundled safely.)
pub(crate) fn is_bundleable_dep_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("js" | "cjs" | "jsx" | "mjs")
    )
}

pub(crate) fn is_cjs_dep_file(path: &Path) -> bool {
    static CACHE: std::sync::OnceLock<Mutex<HashMap<PathBuf, bool>>> = std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(&v) = cache.lock().unwrap().get(path) {
        return v;
    }
    // Only JavaScript files can be CommonJS. A `.css`/`.json`/asset dep has no
    // ES-module syntax either, but it is not CJS — treating it as one routes it
    // to the CJS interop / package-bundle path and serves raw CSS as JS.
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
