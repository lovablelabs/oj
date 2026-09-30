use super::*;

/// The compiled stylesheet as `text/css` (Vite's `?direct` / raw `<link>` request).
pub(crate) async fn serve_css_direct(state: &Arc<ServerState>, file: &Path, url: &str) -> Response {
    match ensure_module(state, file, url).await {
        Ok((_, module)) => (
            [
                (header::CONTENT_TYPE, "text/css"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            module.code.clone(),
        )
            .into_response(),
        Err(err) => {
            send_error(state, &err);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response()
        }
    }
}

pub(crate) async fn serve_css_wrapper(state: &Arc<ServerState>, file: &Path, url: &str) -> Response {
    let (_, module) = match ensure_module(state, file, url).await {
        Ok(pair) => pair,
        Err(err) => {
            send_error(state, &err);
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response();
        }
    };
    // A CSS module exports its class map as the default plus a named export
    // per identifier-safe class (Vite's dataToEsm with namedExports).
    // A module file compiled unscoped (css.modules global mode) still exports
    // its (empty) class map, as Vite does.
    let exports = if module.css_exports.is_empty() && !oj_css::is_css_module(url) {
        "export default void 0;\n".to_string()
    } else {
        oj_css::css_modules_esm(&module.css_exports)
    };
    // Plain stylesheets self-accept; a CSS module does not (its exports change
    // on edit), so the importing component is the boundary, as in Vite's css
    // plugin (`modulesCode || 'import.meta.hot.accept()'`).
    let accept = if module.is_boundary {
        "import.meta.hot.accept(() => {});\n"
    } else {
        ""
    };
    let body = format!(
        "import {{ createHotContext as __oj_hot, updateStyle as __oj_updateStyle, removeStyle as __oj_removeStyle }} from \"/@oj/client.js\";\n\
         import.meta.hot = __oj_hot({url:?});\n\
         __oj_updateStyle({url:?}, {css});\n\
         {exports}\
         {accept}\
         import.meta.hot.prune(() => __oj_removeStyle({url:?}));\n",
        css = serde_json::Value::String(module.code.clone()),
    );
    (
        [
            (header::CONTENT_TYPE, "text/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

pub fn has_postcss_config(root: &Path) -> bool {
    find_postcss_config(root).is_some()
}

/// The PostCSS config that applies to `root`, found the way postcss-load-config
/// does (what Vite uses): `postcss.config.{js,mjs,cjs,ts,mts,cts}`, `.postcssrc`,
/// `.postcssrc.{json,js,mjs,cjs,ts,mts,cts}` or a `package.json` with a
/// `postcss` key, searched from `root` up to the workspace root (nearest wins).
/// The tailwind engine module receives the path per request.
pub fn find_postcss_config(root: &Path) -> Option<PathBuf> {
    const NAMES: &[&str] = &[
        "postcss.config.js",
        "postcss.config.mjs",
        "postcss.config.cjs",
        "postcss.config.ts",
        "postcss.config.mts",
        "postcss.config.cts",
        ".postcssrc",
        ".postcssrc.json",
        ".postcssrc.js",
        ".postcssrc.mjs",
        ".postcssrc.cjs",
        ".postcssrc.ts",
        ".postcssrc.mts",
        ".postcssrc.cts",
    ];
    let stop = workspace_root(root);
    let mut dir = Some(root);
    while let Some(d) = dir {
        for name in NAMES {
            let p = d.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
        let pkg = d.join("package.json");
        if let Ok(text) = std::fs::read_to_string(&pkg) {
            if serde_json::from_str::<serde_json::Value>(&text)
                .ok()
                .is_some_and(|v| v.get("postcss").is_some_and(|p| !p.is_null()))
            {
                return Some(pkg);
            }
        }
        if d == stop {
            break;
        }
        dir = d.parent();
    }
    None
}

pub(crate) async fn run_css_engine(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let engine = state
        .tailwind
        .get_or_try_init(|| CssEngine::tailwind(&state.root, css_engine::DEV_DEADLINE))
        .await
        .map_err(|e| e.to_string())?;
    engine.compile(source, url).await
}

pub(crate) fn is_preprocessor(url: &str) -> bool {
    sidecar::is_less(url) || sidecar::is_stylus(url)
}

// Whether a module served from node_modules or /@fs/ is a dependency (routed to
// the dep/CJS-interop path) rather than app/workspace source. A TS/JSX-extension
// file is always source and must be transpiled, even outside the root: monorepo
// packages reached through a resolve.alias are served via /@fs/ but are source.
/// A dependency module: JS under a `node_modules` directory (by url, or by the
/// real path an `/@fs/` url names). A linked workspace package realpaths outside
/// node_modules and is source, as in Vite's optimizer, so plugins and the
/// source compile path apply to it.
pub(crate) fn is_dep_module(url: &str, file: &Path) -> bool {
    let src_ext = matches!(
        file.extension().and_then(|e| e.to_str()),
        Some("ts" | "tsx" | "jsx" | "mts" | "cts")
    );
    !src_ext
        && (url.contains("/node_modules/")
            || (url.starts_with("/@fs/")
                && file.components().any(|c| c.as_os_str() == "node_modules")))
}

/// Stylesheet extensions (public: the in-process Start module host classifies
/// resolved paths with the same rule the dev server uses).
pub fn is_style_ext(ext: &str) -> bool {
    matches!(ext, "css" | "scss" | "sass" | "less" | "styl" | "stylus")
}

pub(crate) fn is_style_url(url: &str) -> bool {
    let f = url.split('?').next().unwrap_or(url);
    std::path::Path::new(f)
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(is_style_ext)
}

pub(crate) async fn run_preprocess_engine(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
    options: serde_json::Value,
) -> Result<String, String> {
    let engine = state
        .preprocess
        .get_or_try_init(|| CssEngine::preprocess(&state.root, css_engine::DEV_DEADLINE))
        .await
        .map_err(|e| e.to_string())?;
    engine.compile_with(source, url, options).await
}

pub(crate) async fn run_svelte_engine(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let engine = state
        .svelte
        .get_or_try_init(|| CssEngine::svelte(&state.root, css_engine::DEV_DEADLINE))
        .await
        .map_err(|e| e.to_string())?;
    engine.compile(source, url).await
}

pub(crate) async fn compile_tailwind(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let css = run_css_engine(state, url, source).await?;
    state.tailwind_urls.lock().unwrap().insert(url.to_string());
    Ok(css)
}
