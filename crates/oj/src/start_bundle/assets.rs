// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's asset, stylesheet and worker import conventions in a Start bundle
//! (vite:asset, vite:css, vite:worker): `?raw`, `?url`, `?inline`,
//! `?no-inline`, `?worker` / `?sharedworker` (with `&url` or `&inline`), bare
//! font/image/media imports and stylesheet imports. A module is classified by
//! its id at load, as Vite's load hooks key on it, so whichever plugin
//! answered the import the id says what it is.
//!
//! The dev bundle points at the files on the dev server (`/@oj-start/fs/...`,
//! the worker's own URL on the dev pipeline); a build emits them through one
//! content-hash [`Emitter`] shared by the client, server and worker bundles,
//! so a server render and the browser agree on every `/assets/<hash>` URL.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rolldown_common::ModuleType;
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, Plugin, SharedLoadPluginContext,
};

use super::StartConfig;

/// What an asset import is, from its query and extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AssetTag {
    Raw,
    Url,
    Inline,
    Css,
    Worker { shared: bool, form: WorkerForm },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WorkerForm {
    /// `?worker`: a constructor wrapper.
    Module,
    /// `?worker&url`: the worker's URL.
    Url,
    /// `?worker&inline` (bundled environments): the bundle inside the importer.
    Inline,
}

/// Whether `query` (`?a&b=c`) carries the flag `name`, Vite's
/// `(\?|&)name(?:&|$)` (an `=value` makes it another parameter).
fn has_flag(query: &str, name: &str) -> bool {
    query
        .trim_start_matches('?')
        .split('&')
        .any(|part| part == name)
}

/// Vite's inlineRE (`[?&]inline\b`): also `?inline=...` and `?inline-x`.
fn has_inline(query: &str) -> bool {
    query.trim_start_matches('?').split('&').any(|part| {
        part.strip_prefix("inline").is_some_and(|rest| {
            !rest
                .chars()
                .next()
                .is_some_and(|c| c.is_alphanumeric() || c == '_')
        })
    })
}

const ASSET_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "avif", "ico", "woff", "woff2", "ttf", "otf", "eot",
    "mp4", "webm", "wasm",
];
const STYLE_EXTS: &[&str] = &["css", "less", "sass", "scss", "styl", "stylus"];

fn ext_of(path: &str) -> &str {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
}

pub(crate) fn is_style_path(path: &str) -> bool {
    STYLE_EXTS.contains(&ext_of(path))
}

/// A CSS module (`x.module.css`, any compiled language).
pub(crate) fn is_css_module(path: &str) -> bool {
    let name = Path::new(path)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");
    is_style_path(path) && name.ends_with(".module")
}

/// The asset an import (a specifier, or a resolved `/abs/x.png?url` id) is,
/// by its query and extension; `None` when it is no asset import. `dev`: the
/// unbundled dev pipeline ignores `&inline` on a worker, as Vite's does.
pub(crate) fn asset_tag(source: &str, dev: bool) -> Option<AssetTag> {
    if source.starts_with('\0') {
        return None;
    }
    let (path, query) = match source.find('?') {
        Some(i) => (&source[..i], &source[i..]),
        None => (source, ""),
    };
    let worker = has_flag(query, "worker");
    if worker || has_flag(query, "sharedworker") {
        // A bundled environment checks `&inline` before `&url`.
        let form = if !dev && has_inline(query) {
            WorkerForm::Inline
        } else if has_flag(query, "url") {
            WorkerForm::Url
        } else {
            WorkerForm::Module
        };
        return Some(AssetTag::Worker {
            shared: !worker,
            form,
        });
    }
    if has_flag(query, "raw") {
        return Some(AssetTag::Raw);
    }
    // Vite's shouldInline: `no-inline` wins, then `inline` (even with `url`).
    if has_flag(query, "no-inline") {
        return Some(AssetTag::Url);
    }
    if has_inline(query) {
        return Some(AssetTag::Inline);
    }
    if has_flag(query, "url") || ASSET_EXTS.contains(&ext_of(path).to_ascii_lowercase().as_str()) {
        return Some(AssetTag::Url);
    }
    is_style_path(path).then_some(AssetTag::Css)
}

/// The lazily started CSS engines a bundle compiles stylesheets on.
#[derive(Default)]
pub(crate) struct StyleEngines {
    preprocess: tokio::sync::OnceCell<Arc<oj_server::css_engine::CssEngine>>,
    tailwind: tokio::sync::OnceCell<Arc<oj_server::css_engine::CssEngine>>,
}

impl StyleEngines {
    async fn preprocess(
        &self,
        root: &Path,
    ) -> Result<Arc<oj_server::css_engine::CssEngine>, String> {
        self.preprocess
            .get_or_try_init(|| oj_server::css_engine::CssEngine::preprocess(root, None))
            .await
            .cloned()
            .map_err(|e| e.to_string())
    }

    async fn tailwind(&self, root: &Path) -> Result<Arc<oj_server::css_engine::CssEngine>, String> {
        self.tailwind
            .get_or_try_init(|| oj_server::css_engine::CssEngine::tailwind(root, None))
            .await
            .cloned()
            .map_err(|e| e.to_string())
    }
}

/// The stylesheet pipeline the dev server's SSR renders from (preprocessor,
/// CSS-module scoping, lightningcss), so class names agree across the server
/// render, the client and the build.
pub(crate) struct Styles {
    root: PathBuf,
    css: Option<oj_config::CssConfig>,
    modules: oj_css::CssModulesOptions,
    engines: StyleEngines,
    /// `build.cssMinify` per environment and `build.cssTarget`.
    minify_client: bool,
    minify_ssr: bool,
    targets: Vec<String>,
}

impl Styles {
    pub fn new(start: &StartConfig) -> Styles {
        Styles {
            root: start.root.clone(),
            css: start.config.css.clone(),
            modules: oj_server::css_modules_options(&start.config),
            engines: StyleEngines::default(),
            minify_client: oj_config::build_css_minify(&start.config, false),
            minify_ssr: oj_config::build_css_minify(&start.config, true),
            targets: oj_config::build_css_targets(&start.config),
        }
    }

    fn inputs(&self) -> oj_server::StyleInputs<'_> {
        oj_server::StyleInputs {
            root: &self.root,
            css: self.css.as_ref(),
            resolve: oj_css::CssResolve {
                root: Some(&self.root),
                modules: &self.modules,
                ..oj_css::CssResolve::default()
            },
        }
    }

    /// A CSS module's or a preprocessor file's compile: its JS (the class
    /// map) and its CSS.
    pub async fn compile(&self, path: &Path) -> Result<oj_server::StyleModule, String> {
        let source =
            std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let engine = if oj_server::needs_preprocess_engine(path) {
            Some(self.engines.preprocess(&self.root).await?)
        } else {
            None
        };
        oj_server::compile_style_file(&self.inputs(), path, source, engine.as_deref()).await
    }

    /// A dev `?inline` sheet: the module the dev server renders for SSR.
    pub async fn dev_inline(&self, path: &Path) -> Result<String, String> {
        let source =
            std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let engine = if oj_server::needs_preprocess_engine(path) {
            Some(self.engines.preprocess(&self.root).await?)
        } else {
            None
        };
        let root = self.root.clone();
        let engines = &self.engines;
        oj_server::start_dev_inline_style(
            &self.inputs(),
            path,
            source,
            engine.as_deref(),
            async || engines.tailwind(&root).await,
        )
        .await
    }

    /// Tailwind on a sheet that uses it (the substring rule the Start build
    /// always applied), else the sheet as is.
    async fn tailwind(&self, path: &Path, css: String) -> Result<String, String> {
        if !needs_css_compile(&css) {
            return Ok(css);
        }
        self.engines
            .tailwind(&self.root)
            .await?
            .compile_path(&css, path, serde_json::Value::Null, false)
            .await
    }

    /// Vite's build minifyCSS (lightningcss for `build.cssTarget`).
    fn minify(&self, path: &Path, css: &str) -> Result<String, String> {
        let lightning = self
            .css
            .as_ref()
            .and_then(|c| c.lightningcss.as_ref())
            .map(oj_css::LightningOptions::from_json)
            .unwrap_or_default();
        oj_css::minify_css(&path.to_string_lossy(), css, &self.targets, &lightning)
    }
}

/// Whether a sheet needs Tailwind (`@tailwind`, `@apply`, `@plugin`, an
/// `@import "tailwindcss"`).
pub(crate) fn needs_css_compile(src: &str) -> bool {
    src.contains("tailwindcss")
        || src.contains("@tailwind")
        || src.contains("@plugin")
        || src.contains("@apply")
}

/// A worker entry's bundle: the entry chunk's code and the file name the
/// bundler gave it (the chunks it loads are written already).
#[derive(Clone, Debug)]
pub(crate) struct WorkerBundle {
    pub code: String,
    pub file_name: String,
}

/// Bundles a worker entry on its own (Vite's bundleWorkerEntry).
pub(crate) type WorkerBundler = Arc<
    dyn Fn(
            PathBuf,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<WorkerBundle, String>> + Send>,
        > + Send
        + Sync,
>;

/// The build's content-hash emitter: every asset, stylesheet and worker the
/// client, server and worker bundles import is written once into
/// `<client>/assets/<name>-<hash><ext>`, at the same URL for all of them.
pub(crate) struct Emitter {
    assets_dir: PathBuf,
    base: String,
    styles: Arc<Styles>,
    written: Mutex<std::collections::HashSet<String>>,
    /// Stylesheets mid-emit (a sheet's own `url()` naming itself must not recurse).
    emitting: Mutex<std::collections::HashSet<PathBuf>>,
    /// The emitted client stylesheets, in emit order: the root route's links.
    css_urls: Mutex<Vec<String>>,
    worker_bundler: Mutex<Option<WorkerBundler>>,
    /// `worker.format` is "es": workers start as modules, not classic scripts.
    worker_module: std::sync::atomic::AtomicBool,
    worker_codes: Mutex<HashMap<PathBuf, Arc<tokio::sync::OnceCell<WorkerBundle>>>>,
    worker_urls: Mutex<HashMap<PathBuf, Arc<tokio::sync::OnceCell<String>>>>,
    /// The first worker bundle that failed: rolldown reports a failed load as
    /// "plugin threw an error", and the worker's own error is what Vite shows.
    worker_error: Mutex<Option<String>>,
}

impl Emitter {
    pub fn new(client_dir: &Path, base: &str, styles: Arc<Styles>) -> Emitter {
        Emitter {
            assets_dir: client_dir.join("assets"),
            base: base.to_string(),
            styles,
            written: Mutex::new(Default::default()),
            emitting: Mutex::new(Default::default()),
            css_urls: Mutex::new(Vec::new()),
            worker_bundler: Mutex::new(None),
            worker_module: std::sync::atomic::AtomicBool::new(false),
            worker_codes: Mutex::new(HashMap::new()),
            worker_urls: Mutex::new(HashMap::new()),
            worker_error: Mutex::new(None),
        }
    }

    /// The build's worker bundler; `module` when `worker.format` is "es".
    pub fn set_worker_bundler(&self, bundler: WorkerBundler, module: bool) {
        *self.worker_bundler.lock().unwrap() = Some(bundler);
        self.worker_module
            .store(module, std::sync::atomic::Ordering::SeqCst);
    }

    /// Why a worker bundle failed, when one did.
    pub fn worker_error(&self) -> Option<String> {
        self.worker_error.lock().unwrap().clone()
    }

    fn worker_is_module(&self) -> bool {
        self.worker_module.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn css_urls(&self) -> Vec<String> {
        self.css_urls.lock().unwrap().clone()
    }

    /// Writes `bytes` as the file at `path` names it, content-hashed; its URL.
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<String, String> {
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let stem: String = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default()
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '_' || c == '.' || c == '-' {
                    c
                } else {
                    '_'
                }
            })
            .collect();
        let hash = &blake3::hash(bytes).to_hex()[..8];
        let name = format!("{stem}-{hash}{ext}");
        if self.written.lock().unwrap().insert(name.clone()) {
            std::fs::create_dir_all(&self.assets_dir).map_err(|e| e.to_string())?;
            std::fs::write(self.assets_dir.join(&name), bytes).map_err(|e| e.to_string())?;
        }
        Ok(format!("{}assets/{name}", self.base))
    }

    /// Emits the file at `path`: a stylesheet compiled with its `url()`s
    /// emitted, anything else as its bytes.
    pub fn emit<'a>(
        &'a self,
        path: &'a Path,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, String>> + Send + 'a>>
    {
        Box::pin(async move {
            let reentrant = self.emitting.lock().unwrap().contains(path);
            if path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("css"))
                && !reentrant
            {
                let css = std::fs::read_to_string(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                return self.css(path, css).await;
            }
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            self.write(path, &bytes)
        })
    }

    /// A stylesheet's text emitted as the file at `path` (a CSS module's
    /// scoped sheet is not the file's bytes): Tailwind when it uses it, its
    /// `url()`s emitted, minified under the client's cssMinify (ending in a
    /// newline, as Vite's finalizeCss), then hashed and recorded.
    pub async fn css(&self, path: &Path, css: String) -> Result<String, String> {
        self.emitting.lock().unwrap().insert(path.to_path_buf());
        let result = async {
            let css = self.styles.tailwind(path, css).await?;
            let css = self
                .rewrite_urls(&css, path.parent().unwrap_or(path))
                .await?;
            let css = if self.styles.minify_client {
                format!("{}\n", self.styles.minify(path, &css)?)
            } else {
                css
            };
            // A compiled .scss/.less sheet ships as .css.
            let url = self.write(&path.with_extension("css"), css.as_bytes())?;
            let mut urls = self.css_urls.lock().unwrap();
            if !urls.contains(&url) {
                urls.push(url.clone());
            }
            Ok(url)
        }
        .await;
        self.emitting.lock().unwrap().remove(path);
        result
    }

    /// A `?inline` sheet's text: compiled and its `url()`s emitted as `css`
    /// does, but returned, minified under the importing environment's
    /// cssMinify with no trailing newline.
    pub async fn inline_css(
        &self,
        path: &Path,
        css: String,
        server: bool,
    ) -> Result<String, String> {
        let css = self.styles.tailwind(path, css).await?;
        let css = self
            .rewrite_urls(&css, path.parent().unwrap_or(path))
            .await?;
        let minify = if server {
            self.styles.minify_ssr
        } else {
            self.styles.minify_client
        };
        if minify {
            self.styles.minify(path, &css)
        } else {
            Ok(css)
        }
    }

    /// Every relative `url(...)` of a sheet in `dir` emitted and pointed at
    /// its URL; `data:`, `http(s):`, `//`, `#` and root-absolute ones, and
    /// missing files, stay as written.
    async fn rewrite_urls(&self, css: &str, dir: &Path) -> Result<String, String> {
        let mut out = String::with_capacity(css.len());
        let mut last = 0;
        for site in css_urls(css) {
            out.push_str(&css[last..site.start]);
            last = site.end;
            let target = site.url.trim();
            let keep = target.starts_with("data:")
                || target.starts_with("http:")
                || target.starts_with("https:")
                || target.starts_with("//")
                || target.starts_with('#')
                || target.starts_with('/');
            let clean = target.split(['?', '#']).next().unwrap_or(target);
            let abs = dir.join(clean);
            if keep || !abs.is_file() {
                out.push_str(&css[site.start..site.end]);
                continue;
            }
            let suffix = &target[clean.len()..];
            let url = self.emit(&normalize(&abs)).await?;
            out.push_str(&format!(
                "url({})",
                serde_json::Value::String(format!("{url}{suffix}"))
            ));
        }
        out.push_str(&css[last..]);
        Ok(out)
    }

    /// A worker entry's bundle, bundled once per file.
    pub async fn worker_code(&self, path: &Path) -> Result<WorkerBundle, String> {
        let cell = Arc::clone(
            self.worker_codes
                .lock()
                .unwrap()
                .entry(path.to_path_buf())
                .or_default(),
        );
        let bundler = self.worker_bundler.lock().unwrap().clone();
        let result = cell
            .get_or_try_init(|| async move {
                let bundler = bundler
                    .ok_or_else(|| format!("oj: no worker bundler for {}", path.display()))?;
                bundler(path.to_path_buf()).await
            })
            .await
            .cloned();
        if let Err(e) = &result {
            self.worker_error
                .lock()
                .unwrap()
                .get_or_insert_with(|| e.clone());
        }
        result
    }

    /// The emitted URL of a worker entry's bundle.
    pub async fn worker(&self, path: &Path) -> Result<String, String> {
        let cell = Arc::clone(
            self.worker_urls
                .lock()
                .unwrap()
                .entry(path.to_path_buf())
                .or_default(),
        );
        cell.get_or_try_init(|| async move {
            let bundle = self.worker_code(path).await?;
            let assets = self.assets_dir.parent().unwrap_or(&self.assets_dir);
            let dest = assets.join(&bundle.file_name);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&dest, bundle.code.as_bytes()).map_err(|e| e.to_string())?;
            Ok(format!("{}{}", self.base, bundle.file_name))
        })
        .await
        .cloned()
    }
}

/// A `url(...)` in a stylesheet: the whole span and the URL inside (quotes
/// stripped), Vite's `url\(\s*(['"]?)([^'")]+)\1\s*\)`.
struct CssUrl<'a> {
    start: usize,
    end: usize,
    url: &'a str,
}

fn css_urls(css: &str) -> Vec<CssUrl<'_>> {
    let mut out = Vec::new();
    let b = css.as_bytes();
    let mut from = 0;
    while let Some(i) = css[from..].find("url(") {
        let start = from + i;
        from = start + 4;
        let mut j = from;
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        let quote = match b.get(j) {
            Some(q @ (b'\'' | b'"')) => {
                j += 1;
                Some(*q)
            }
            _ => None,
        };
        let url_start = j;
        while j < b.len() && !matches!(b[j], b'\'' | b'"' | b')') {
            j += 1;
        }
        if j == url_start || j >= b.len() {
            continue;
        }
        let url_end = j;
        match quote {
            Some(q) if b[j] == q => j += 1,
            Some(_) => continue,
            None if b[j] != b')' => continue,
            None => {}
        }
        while j < b.len() && b[j].is_ascii_whitespace() {
            j += 1;
        }
        if b.get(j) != Some(&b')') {
            continue;
        }
        out.push(CssUrl {
            start,
            end: j + 1,
            url: &css[url_start..url_end],
        });
        from = j + 1;
    }
    out
}

fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// A file as a `data:` URI (`?inline` of a non-stylesheet).
fn data_uri(path: &Path) -> Result<String, String> {
    use base64::Engine;
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mime = match ext_of(&path.to_string_lossy())
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        _ => "application/octet-stream",
    };
    Ok(format!(
        "data:{mime};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(bytes)
    ))
}

/// The options a worker wrapper starts its worker with: a module only when
/// `worker.format` is "es" (Vite's worker load; unbundled dev always serves
/// modules).
fn worker_type_option(module: bool) -> &'static str {
    if module {
        "{ type: \"module\", name: options?.name }"
    } else {
        "{ name: options?.name }"
    }
}

/// Vite's `?worker&inline` in a bundled environment: the bundled worker in
/// the importer as a string, started from a Blob URL that revokes itself
/// (through `self.location` in a classic worker), falling back to a data:
/// URL; a SharedWorker always takes the data: URL (a blob URL would make
/// separate instances).
fn inline_worker_module(shared: bool, code: &str, module: bool) -> String {
    let js_content = format!(
        "const jsContent = {};",
        serde_json::Value::String(code.to_string())
    );
    let options = worker_type_option(module);
    let revoke = if module {
        "'URL.revokeObjectURL(import.meta.url);'"
    } else {
        "'(self.URL || self.webkitURL).revokeObjectURL(self.location.href);'"
    };
    if !shared {
        return format!(
            r#"{js_content}
const blob = typeof self !== "undefined" && self.Blob && new Blob([{revoke}, jsContent], {{ type: "text/javascript;charset=utf-8" }});
export default function WorkerWrapper(options) {{
  let objURL;
  try {{
    objURL = blob && (self.URL || self.webkitURL).createObjectURL(blob);
    if (!objURL) throw '';
    const worker = new Worker(objURL, {options});
    worker.addEventListener("error", () => {{
      (self.URL || self.webkitURL).revokeObjectURL(objURL);
    }});
    return worker;
  }} catch (e) {{
    return new Worker('data:text/javascript;charset=utf-8,' + encodeURIComponent(jsContent), {options});
  }}
}}
"#
        );
    }
    format!(
        r#"{js_content}
export default function WorkerWrapper(options) {{
  return new SharedWorker('data:text/javascript;charset=utf-8,' + encodeURIComponent(jsContent), {options});
}}
"#
    )
}

/// The dev bundle's cache keys for compiled sheets: a long-lived dev engine
/// caches modules by id, so a sheet a style input feeds (a partial, the
/// Tailwind config) must restart it (`compiled_sheets` says whether any
/// sheet was compiled at all).
#[derive(Default)]
pub(crate) struct DevStyleState {
    pub compiled_sheets: std::sync::atomic::AtomicBool,
}

/// Where the asset modules point.
pub(crate) enum AssetMode {
    /// The dev server's files: `/@oj-start/fs/<abs>`, workers on the dev
    /// pipeline. `css_urls` collects the linked stylesheets.
    Dev {
        css_urls: Arc<Mutex<Vec<String>>>,
        state: Arc<DevStyleState>,
    },
    /// The build's emitter; `server` for the server bundle (which links no
    /// stylesheet and minifies `?inline` sheets under ssr's cssMinify).
    Build { emitter: Arc<Emitter>, server: bool },
}

pub(crate) const DEV_FS_BASE: &str = "/@oj-start/fs";

pub(crate) struct StartAssetsPlugin {
    root: PathBuf,
    styles: Arc<Styles>,
    mode: AssetMode,
}

impl std::fmt::Debug for StartAssetsPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StartAssetsPlugin")
    }
}

impl StartAssetsPlugin {
    pub fn new(root: &Path, styles: Arc<Styles>, mode: AssetMode) -> Self {
        Self {
            root: root.to_path_buf(),
            styles,
            mode,
        }
    }

    async fn url_for(&self, path: &Path) -> Result<String, String> {
        match &self.mode {
            AssetMode::Dev { .. } => Ok(format!("{DEV_FS_BASE}{}", path.display())),
            AssetMode::Build { emitter, .. } => emitter.emit(path).await,
        }
    }

    /// A worker's URL: the dev pipeline's (root-relative, `/@fs` outside the
    /// root), or the build's emitted bundle.
    async fn worker_url_for(&self, path: &Path) -> Result<String, String> {
        match &self.mode {
            AssetMode::Dev { .. } => Ok(match path.strip_prefix(&self.root) {
                Ok(rel) => format!("/{}", rel.to_string_lossy().replace('\\', "/")),
                Err(_) => format!("/@fs{}", path.display()),
            }),
            AssetMode::Build { emitter, .. } => emitter.worker(path).await,
        }
    }

    async fn module(&self, id: &str, path: &Path) -> Result<Option<String>, String> {
        let json = |s: &str| serde_json::Value::String(s.to_string()).to_string();
        let dev = matches!(self.mode, AssetMode::Dev { .. });
        let Some(tag) = asset_tag(id, dev) else {
            // A bare `.svg` (or `?react` one) no plugin componentized: its URL.
            if path.extension().is_some_and(|e| e == "svg") {
                return Ok(Some(format!(
                    "export default {};",
                    json(&self.url_for(path).await?)
                )));
            }
            return Ok(None);
        };
        let path_str = path.to_string_lossy();
        Ok(Some(match tag {
            AssetTag::Raw => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                format!("export default {};", json(&text))
            }
            AssetTag::Url => format!("export default {};", json(&self.url_for(path).await?)),
            AssetTag::Inline if is_style_path(&path_str) => match &self.mode {
                AssetMode::Dev { state, .. } => {
                    state
                        .compiled_sheets
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    self.styles.dev_inline(path).await?
                }
                AssetMode::Build { emitter, server } => {
                    let compiled = self.styles.compile(path).await?;
                    // A build gives a CSS module's class map even inlined.
                    if is_css_module(&path_str) {
                        compiled.code
                    } else {
                        let css = emitter.inline_css(path, compiled.css, *server).await?;
                        format!("export default {};", json(&css))
                    }
                }
            },
            AssetTag::Inline => format!("export default {};", json(&data_uri(path)?)),
            AssetTag::Worker { shared, form } => {
                let ctor = if shared { "SharedWorker" } else { "Worker" };
                let module = match &self.mode {
                    AssetMode::Dev { .. } => true,
                    AssetMode::Build { emitter, .. } => emitter.worker_is_module(),
                };
                match (form, &self.mode) {
                    (WorkerForm::Inline, AssetMode::Build { emitter, .. }) => {
                        inline_worker_module(shared, &emitter.worker_code(path).await?.code, module)
                    }
                    (WorkerForm::Url, _) => {
                        format!("export default {};", json(&self.worker_url_for(path).await?))
                    }
                    _ => format!(
                        "export default function WorkerWrapper(options) {{ return new {ctor}({}, {}); }}",
                        json(&self.worker_url_for(path).await?),
                        worker_type_option(module)
                    ),
                }
            }
            AssetTag::Css => {
                // A CSS module or a preprocessor file compiles on oj's CSS
                // pipeline; a plain `.css` is linked as written (a build
                // emits it compiled).
                let compiled = if is_css_module(&path_str) || !path_str.ends_with(".css") {
                    if let AssetMode::Dev { state, .. } = &self.mode {
                        state
                            .compiled_sheets
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    Some(self.styles.compile(path).await?)
                } else {
                    None
                };
                match &self.mode {
                    AssetMode::Dev { css_urls, .. } => {
                        let href = self.url_for(path).await?;
                        let mut urls = css_urls.lock().unwrap();
                        if !urls.contains(&href) {
                            urls.push(href);
                        }
                    }
                    AssetMode::Build {
                        emitter,
                        server: false,
                    } => match &compiled {
                        Some(c) => {
                            emitter.css(path, c.css.clone()).await?;
                        }
                        None => {
                            emitter.emit(path).await?;
                        }
                    },
                    AssetMode::Build { server: true, .. } => {}
                }
                compiled.map_or_else(|| "export default {};".to_string(), |c| c.code)
            }
        }))
    }
}

impl Plugin for StartAssetsPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:start-assets")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::Load
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        let id = args.id;
        if id.starts_with('\0') {
            return Ok(None);
        }
        let path = Path::new(id.split('?').next().unwrap_or(id));
        if !path.is_absolute() {
            return Ok(None);
        }
        let code = self.module(id, path).await.map_err(|e| {
            oj_server::plugins::load_failed("oj:start-assets", anyhow::anyhow!("{e}"))
        })?;
        Ok(code.map(|code| HookLoadOutput {
            code: arcstr::ArcStr::from(code),
            module_type: Some(ModuleType::Js),
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn asset_imports_classify_like_vites_query_regexes() {
        use AssetTag::*;
        let cases: &[(&str, bool, Option<AssetTag>)] = &[
            ("/a/x.png", false, Some(Url)),
            ("/a/x.PNG", false, Some(Url)),
            ("/a/x.svg", false, None),
            ("/a/x.css", false, Some(Css)),
            ("/a/x.module.scss", false, Some(Css)),
            ("/a/x.ts?raw", false, Some(Raw)),
            ("/a/x.css?url", false, Some(Url)),
            ("/a/x.css?inline", false, Some(Inline)),
            ("/a/x.png?url&no-inline", false, Some(Url)),
            ("/a/x.png?no-inline&inline", false, Some(Url)),
            ("/a/x.png?url&inline", false, Some(Inline)),
            ("/a/x.ts?rawx", false, None),
            ("/a/x.ts?raw=1", false, None),
            ("\0/a/x.png", false, None),
        ];
        for (id, dev, want) in cases {
            assert_eq!(asset_tag(id, *dev), *want, "{id}");
        }
        assert_eq!(
            asset_tag("/w.ts?worker&inline", false),
            Some(Worker {
                shared: false,
                form: WorkerForm::Inline
            })
        );
        assert_eq!(
            asset_tag("/w.ts?worker&inline", true),
            Some(Worker {
                shared: false,
                form: WorkerForm::Module
            }),
            "unbundled dev ignores &inline"
        );
        assert_eq!(
            asset_tag("/w.ts?sharedworker&url", true),
            Some(Worker {
                shared: true,
                form: WorkerForm::Url
            })
        );
        assert_eq!(
            asset_tag("/w.ts?url&worker", false),
            Some(Worker {
                shared: false,
                form: WorkerForm::Url
            }),
            "any order"
        );
    }

    #[test]
    fn css_modules_and_style_paths() {
        assert!(is_css_module("/a/b.module.css"));
        assert!(is_css_module("/a/b.module.scss"));
        assert!(!is_css_module("/a/b.css"));
        assert!(is_style_path("/a/b.styl"));
        assert!(!is_style_path("/a/b.ts"));
    }

    #[test]
    fn css_url_sites_parse_like_vites_regex() {
        let css = r#"a{background:url( "./a.png" )} b{src:url(./f.woff2?v=1#x)} c{x:url('d.svg')} e{y:url()} f{url("unclosed)}"#;
        let urls: Vec<&str> = css_urls(css).iter().map(|u| u.url).collect();
        assert_eq!(urls, ["./a.png", "./f.woff2?v=1#x", "d.svg"]);
    }

    #[test]
    fn the_inline_worker_wrapper_matches_vites() {
        let module = inline_worker_module(false, "self.onmessage=()=>{}", true);
        assert!(
            module.starts_with("const jsContent = \"self.onmessage=()=>{}\";"),
            "{module}"
        );
        assert!(module.contains("URL.revokeObjectURL(import.meta.url);"));
        assert!(module.contains("new Worker(objURL, { type: \"module\", name: options?.name })"));
        // An iife worker (Vite's default format) is a classic script.
        let classic = inline_worker_module(false, "x", false);
        assert!(
            classic.contains("'(self.URL || self.webkitURL).revokeObjectURL(self.location.href);'"),
            "{classic}"
        );
        assert!(classic.contains("new Worker(objURL, { name: options?.name })"));
        assert!(!classic.contains("type: \"module\""));
        let shared = inline_worker_module(true, "x", true);
        assert!(
            shared.contains("new SharedWorker('data:text/javascript;charset=utf-8,'"),
            "{shared}"
        );
        assert!(!shared.contains("Blob"));
    }

    #[test]
    fn tailwind_runs_only_on_sheets_that_use_it() {
        assert!(needs_css_compile("@import \"tailwindcss\";"));
        assert!(needs_css_compile(".a { @apply p-4; }"));
        assert!(!needs_css_compile(".a { color: red }"));
    }
}
