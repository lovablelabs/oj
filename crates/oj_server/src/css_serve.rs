use super::*;
use crate::css_engine::CssDeps;

/// The compiled stylesheet as `text/css` (Vite's `?direct` / raw `<link>` request).
pub(crate) async fn serve_css_direct(state: &Arc<ServerState>, file: &Path, url: &str) -> Response {
    let url = format!("{url}?direct");
    let css = match ensure_module(state, file, &url).await {
        Ok((_, module)) => {
            let id = format!("{}?direct", file.display());
            css_post(state, file, &url, &id, module.code.clone()).await
        }
        Err(err) => Err(err),
    };
    match css {
        Ok(css) => (
            [
                (header::CONTENT_TYPE, "text/css"),
                (header::CACHE_CONTROL, "no-cache"),
            ],
            css,
        )
            .into_response(),
        Err(err) => {
            send_error(state, &err);
            (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response()
        }
    }
}

/// An inline `<style>` or `style` attribute the dev HTML transform lifted out
/// of a page, compiled as Vite's devHtmlHook runs it through the client plugin
/// container. `id` is its html-proxy url (`/index.html?html-proxy&direct&index=0.css`,
/// `\0/page?html-proxy&inline-css&style-attr&index=1.css` for a page with no
/// file). Plugin transforms see Vite's id for it: the page file plus the
/// query, or the `\0` id as is. A sheet then compiles like a `.css` file beside
/// its page; an attribute's declarations go through the plugin transforms and
/// the app's PostCSS plugins and get their urls rebased, as text, and their
/// quotes escaped as vite:css-post records them.
pub(crate) async fn compile_html_style(
    state: &Arc<ServerState>,
    id: &str,
    code: String,
) -> Result<String, String> {
    let (page, query) = id.split_once('?').unwrap_or((id, ""));
    let virtual_page = page.starts_with('\0');
    let page = page.trim_start_matches('\0');
    let page_file = if Path::new(page).starts_with(&state.root) {
        PathBuf::from(page)
    } else {
        state.root.join(page.trim_start_matches('/'))
    };
    let transform_id = if virtual_page {
        id.to_string()
    } else {
        format!("{}?{query}", page_file.display())
    };
    // A page with no file is `/@id/__x00__<page>` (Vite's url for its `\0`
    // id), a directory nothing is on disk under: its relative urls resolve
    // nowhere and stay as written, as under Vite.
    let url = if virtual_page {
        format!("/@id/__x00__{page}?{query}")
    } else {
        format!("{}?{query}", url_of(&state.root, &page_file))
    };
    if query.split('&').any(|kv| kv == "style-attr") {
        // Vite's PostCSS pass parses the declarations as a root of their own:
        // the app's plugins run first, the url rewrite last.
        let code = plugin_transform(state, &page_file, &transform_id, code, false)
            .await?
            .code;
        let code = if state.has_postcss {
            run_css_engine(state, &url, &code).await?
        } else {
            code
        };
        let css = oj_css::rewrite_style_attribute_dev(&url, &code, &state.css_resolve.as_ref());
        return Ok(css.replace('"', "&quot;"));
    }
    let file = PathBuf::from(format!("{}.css", page_file.display()));
    let (_, module) = ensure_module_as(state, &file, &url, Some(code), Some(&transform_id)).await?;
    css_post(state, &page_file, &url, &transform_id, module.code.clone()).await
}

/// A stylesheet's "post" plugins (enforce or order "post"), on what Vite's
/// vite:css-post serves in dev: the CSS itself for a `?direct` request, the
/// JS module for an import. The sheet's other slots ran in its compile.
pub(crate) async fn css_post(
    state: &ServerState,
    file: &Path,
    url: &str,
    id: &str,
    code: String,
) -> Result<String, String> {
    let input = {
        use std::hash::{Hash, Hasher};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        code.hash(&mut h);
        h.finish()
    };
    if let Some((_, out)) = state
        .css_post
        .lock()
        .unwrap()
        .get(id)
        .filter(|(at, _)| *at == input)
    {
        return Ok(out.clone());
    }
    let is_dep = is_dep_module(url, file);
    let t = plugin_transform_phase(state, file, id, code, is_dep, Some("post")).await?;
    if !t.watch_files.is_empty() {
        let mut watched = state.plugin_watched.lock().unwrap();
        for p in &t.watch_files {
            let p = PathBuf::from(p);
            ensure_watched_file(state, &p);
            watched.insert(p);
        }
    }
    state
        .css_post
        .lock()
        .unwrap()
        .insert(id.to_string(), (input, t.code.clone()));
    Ok(t.code)
}

pub(crate) async fn serve_css_wrapper(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
) -> Response {
    let (module, sheet) = match ensure_module(state, file, url).await.map(|(_, module)| {
        let sheet = module.code.clone();
        (module, sheet)
    }) {
        Ok(pair) => pair,
        Err(err) => {
            send_error(state, &err);
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response();
        }
    };
    // CSS module: class map as default plus a named export per identifier-safe
    // class (Vite's dataToEsm); an unscoped module file still exports its (empty) map.
    let exports = if module.css_exports.is_empty() && !oj_css::is_css_module(url) {
        "export default void 0;\n".to_string()
    } else {
        oj_css::css_modules_esm(&module.css_exports)
    };
    // Plain stylesheets self-accept; a CSS module does not (its exports change
    // on edit), so the importing component is the boundary (Vite's css plugin).
    let accept = if module.is_boundary {
        "import.meta.hot.accept(() => {});\n"
    } else {
        ""
    };
    let body = format!(
        "import {{ createHotContext as __oj_hot, updateStyle as __oj_updateStyle, removeStyle as __oj_removeStyle }} from {client:?};\n\
         import.meta.hot = __oj_hot({url:?});\n\
         __oj_updateStyle({url:?}, {css});\n\
         {exports}\
         {accept}\
         import.meta.hot.prune(() => __oj_removeStyle({url:?}));\n",
        css = serde_json::to_string(&sheet).unwrap(),
        client = state.oj_paths.client,
    );
    let body = match css_post(state, file, url, &file.to_string_lossy(), body).await {
        Ok(body) => body,
        Err(err) => {
            send_error(state, &err);
            return (StatusCode::INTERNAL_SERVER_ERROR, format!("oj: {err}")).into_response();
        }
    };
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

/// The PostCSS config applying to `root`, found the way postcss-load-config does:
/// config files or a package.json `postcss` key, from `root` up to the workspace root (nearest wins).
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
    let engine = lazy_engine(state, EngineKind::Tailwind).await?;
    engine.compile(source, url).await
}

pub(crate) fn is_preprocessor(url: &str) -> bool {
    sidecar::preprocess_lang(url).is_some()
}

/// A dependency module (dep/CJS-interop path): JS under node_modules, by url or
/// by the real path an /@fs/ url names. TS/JSX-extension files are always source,
/// even outside the root; a linked workspace package realpaths outside
/// node_modules and is source, as in Vite's optimizer.
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
    matches!(
        ext,
        "css" | "scss" | "sass" | "less" | "styl" | "stylus" | "pcss" | "postcss" | "sss"
    )
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
    let engine = lazy_engine(state, EngineKind::Preprocess).await?;
    engine.compile_with(source, url, options).await
}

pub(crate) async fn run_svelte_engine(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let engine = lazy_engine(state, EngineKind::Svelte).await?;
    engine.compile(source, url).await
}

/// Tailwind sheets as last compiled, by url. Vite keeps a CSS module's
/// transform in its module graph until a file it depends on changes; a
/// Tailwind sheet depends on every source it scans, so any watched change
/// moves the generation and the next request compiles again. Without it,
/// every request compiled: the eager crawl and the page's own fetch on a
/// cold load, and each reload after.
#[derive(Default)]
pub(crate) struct TailwindSheets {
    generation: std::sync::atomic::AtomicU64,
    sheets: Mutex<HashMap<String, Sheet>>,
}

struct Sheet {
    source: u64,
    generation: u64,
    css: Arc<str>,
}

impl TailwindSheets {
    /// A watched file changed.
    pub(crate) fn invalidate(&self) {
        self.generation
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn generation(&self) -> u64 {
        self.generation.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn get(&self, url: &str, source: u64) -> Option<Arc<str>> {
        let generation = self.generation();
        let sheets = self.sheets.lock().unwrap();
        let sheet = sheets.get(url)?;
        (sheet.source == source && sheet.generation == generation).then(|| Arc::clone(&sheet.css))
    }

    /// `generation` is the one read before the compile started: a change
    /// landing mid-compile leaves the sheet stale, never fresh.
    fn put(&self, url: &str, source: u64, generation: u64, css: Arc<str>) {
        self.sheets.lock().unwrap().insert(
            url.to_string(),
            Sheet {
                source,
                generation,
                css,
            },
        );
    }
}

fn source_digest(source: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    source.hash(&mut h);
    h.finish()
}

/// A Tailwind sheet compiled, with the files its `@import`s inlined. Under a
/// PostCSS config, Vite's compileCSS runs postcss-import ahead of the app's
/// plugins (Tailwind v3 resolves no `@import`); without one, Tailwind v4's
/// compiler resolves them itself.
pub(crate) async fn compile_tailwind(
    state: &Arc<ServerState>,
    file: &Path,
    url: &str,
    source: &str,
) -> Result<(String, Vec<PathBuf>), String> {
    // The sheet's own url, as the page links it and HMR updates it (not a
    // `?direct` or `?inline` request's).
    state
        .tailwind_urls
        .lock()
        .unwrap()
        .insert(sheet_url(url).to_string());
    let mut imported = Vec::new();
    let inlined;
    let source = if state.has_postcss {
        inlined = oj_css::inline_imports_collecting(
            source,
            file,
            &state.css_resolve.as_ref(),
            &mut imported,
        )?;
        inlined.as_str()
    } else {
        source
    };
    let css = compile_tailwind_sheet(state, url, source).await?;
    Ok((css, imported))
}

async fn compile_tailwind_sheet(
    state: &Arc<ServerState>,
    url: &str,
    source: &str,
) -> Result<String, String> {
    let digest = source_digest(source);
    let sheets = &state.tailwind_sheets;
    if let Some(css) = sheets.get(url, digest) {
        return Ok(css.to_string());
    }
    // One compile per sheet at a time: the crawl and the page ask together.
    let lock = {
        let mut locks = state.compile_locks.lock().unwrap();
        Arc::clone(locks.entry(format!("tailwind:{url}")).or_default())
    };
    let _guard = lock.lock().await;
    if let Some(css) = sheets.get(url, digest) {
        return Ok(css.to_string());
    }
    let generation = sheets.generation();
    let engine = lazy_engine(state, EngineKind::Tailwind).await?;
    let (css, deps) = engine.compile_tracked(source, url).await?;
    watch_css_deps(state, deps).await;
    let css: Arc<str> = css.into();
    sheets.put(url, digest, generation, Arc::clone(&css));
    Ok(css.to_string())
}

/// Vite's compileCSS hands a sheet's PostCSS deps to the watcher
/// (`addWatchFile`), so an edit to a scanned source outside the root still
/// moves the sheet's generation. The root watch covers the rest. As with
/// chokidar's `add`, a watch takes effect a moment after it is asked for.
async fn watch_css_deps(state: &Arc<ServerState>, deps: CssDeps) {
    let root = state.root.clone();
    let files = tokio::task::spawn_blocking(move || outside_root_deps(&root, deps))
        .await
        .unwrap_or_default();
    for file in files {
        ensure_watched_file(state, &file);
    }
}

/// The dep files outside `root`, one per directory (the watcher watches
/// directories); the root watch covers the rest.
fn outside_root_deps(root: &Path, deps: CssDeps) -> Vec<PathBuf> {
    let mut dirs = std::collections::HashSet::new();
    css_dep_files(deps, Some(root))
        .into_iter()
        .filter(|p| {
            !p.starts_with(root) && !p.components().any(|c| c.as_os_str() == "node_modules")
        })
        .filter(|p| p.parent().is_some_and(|d| dirs.insert(d.to_path_buf())))
        .collect()
}

/// A compile's deps as files, as Vite's compileCSS collects them: a
/// `dir-dependency` expands as its globSync does, files under `dir` matching
/// `glob` with no node_modules and no dotfiles. `prune` is a subtree left
/// out (already watched).
pub(crate) fn css_dep_files(deps: CssDeps, prune: Option<&Path>) -> Vec<PathBuf> {
    let mut out = deps.files;
    for (dir, glob) in deps.dirs {
        if prune.is_some_and(|p| dir.starts_with(p)) {
            continue;
        }
        let Ok(matcher) = globset::GlobBuilder::new(&glob)
            .literal_separator(true)
            .build()
            .map(|g| g.compile_matcher())
        else {
            continue;
        };
        let mut stack = vec![dir.clone()];
        while let Some(at) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&at) else {
                continue;
            };
            for entry in entries.flatten() {
                let name = entry.file_name();
                if name.to_string_lossy().starts_with('.') || name == "node_modules" {
                    continue;
                }
                let path = entry.path();
                if prune.is_some_and(|p| path == p) {
                    continue;
                }
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(path);
                } else if path
                    .strip_prefix(&dir)
                    .is_ok_and(|rel| matcher.is_match(rel))
                {
                    out.push(path);
                }
            }
        }
    }
    out
}

pub(crate) enum EngineKind {
    Tailwind,
    Preprocess,
    Svelte,
}

/// The dev engine of `kind`, booted on first use.
pub(crate) async fn lazy_engine(
    state: &ServerState,
    kind: EngineKind,
) -> Result<&Arc<CssEngine>, String> {
    let cell = match kind {
        EngineKind::Tailwind => &state.tailwind,
        EngineKind::Preprocess => &state.preprocess,
        EngineKind::Svelte => &state.svelte,
    };
    cell.get_or_try_init(|| async {
        let root = &state.root;
        let registry = Some(state.engine_registry.clone());
        match kind {
            EngineKind::Tailwind => CssEngine::tailwind(root, registry).await,
            EngineKind::Preprocess => CssEngine::preprocess(root, registry).await,
            EngineKind::Svelte => CssEngine::svelte(root, registry).await,
        }
    })
    .await
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tailwind_sheet_tests {
    use super::*;

    #[test]
    fn a_sheet_serves_until_its_source_or_a_watched_file_changes() {
        let sheets = TailwindSheets::default();
        let src = source_digest("@tailwind utilities;");
        assert!(sheets.get("/src/index.css", src).is_none());

        let generation = sheets.generation();
        sheets.put("/src/index.css", src, generation, "a{}".into());
        assert_eq!(sheets.get("/src/index.css", src).as_deref(), Some("a{}"));
        assert!(sheets.get("/src/other.css", src).is_none());
        assert!(sheets
            .get("/src/index.css", source_digest("@tailwind base;"))
            .is_none());

        sheets.invalidate();
        assert!(sheets.get("/src/index.css", src).is_none());
    }

    #[test]
    fn outside_root_deps_expand_dir_globs_and_skip_the_root() {
        let base = tempfile::tempdir().unwrap();
        let base = base.path().canonicalize().unwrap();
        let root = base.join("app");
        let pkg = base.join("packages/ui");
        for dir in [
            root.join("src"),
            pkg.join("src/deep"),
            pkg.join("node_modules/x"),
            pkg.join(".cache"),
        ] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for file in [
            root.join("src/App.tsx"),
            pkg.join("src/Button.tsx"),
            pkg.join("src/deep/Card.tsx"),
            pkg.join("src/notes.md"),
            pkg.join("node_modules/x/y.tsx"),
            pkg.join(".cache/z.tsx"),
            base.join("theme.css"),
        ] {
            std::fs::write(file, "").unwrap();
        }
        let deps = CssDeps {
            files: vec![base.join("theme.css"), root.join("src/App.tsx")],
            dirs: vec![
                (pkg.clone(), "**/*.{ts,tsx}".into()),
                (base.clone(), "app/**/*.tsx".into()),
                (root.join("src"), "**/*.tsx".into()),
            ],
        };
        let mut got = outside_root_deps(&root, deps);
        got.sort();
        assert_eq!(
            got,
            vec![
                pkg.join("src/Button.tsx"),
                pkg.join("src/deep/Card.tsx"),
                base.join("theme.css"),
            ]
        );
    }

    #[test]
    fn a_change_during_the_compile_leaves_its_sheet_stale() {
        let sheets = TailwindSheets::default();
        let src = source_digest("@tailwind utilities;");
        let started = sheets.generation();
        sheets.invalidate();
        sheets.put("/src/index.css", src, started, "a{}".into());
        assert!(sheets.get("/src/index.css", src).is_none());
    }
}

#[cfg(test)]
mod style_ext_tests {
    use super::*;

    // Vite's CSS_LANGS_RE: /\.(css|less|sass|scss|styl|stylus|pcss|postcss|sss)(?:$|\?)/.
    #[test]
    fn style_extensions_are_vites_css_langs() {
        for ext in [
            "css", "less", "sass", "scss", "styl", "stylus", "pcss", "postcss", "sss",
        ] {
            assert!(is_style_ext(ext), "{ext}");
        }
        for ext in ["js", "ts", "json", "svg", "html", "style", "ss"] {
            assert!(!is_style_ext(ext), "{ext}");
        }
        assert!(is_style_url("/src/a.pcss?inline"));
        assert!(is_style_url("/src/a.sss"));
    }
}
