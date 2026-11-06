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
/// Vite's CSS_LANGS_RE.
const STYLE_EXTS: &[&str] = &[
    "css", "less", "sass", "scss", "styl", "stylus", "pcss", "postcss", "sss",
];

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

/// A sheet the dev bundle ran through the plugins (see `Styles::dev_sheets`).
struct DevSheet {
    stamp: u64,
    /// For a sheet a plugin serves, a hash of what its `load` returned: the
    /// sheet is current while the plugin returns the same text, whatever the
    /// stamp (UnoCSS's grows as modules transform, with no file edit).
    loaded: Option<u64>,
    css: String,
}

fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// The stylesheet pipeline the dev server's SSR renders from (preprocessor,
/// CSS-module scoping, lightningcss), so class names agree across the server
/// render, the client and the build.
pub(crate) struct Styles {
    root: PathBuf,
    css: Option<oj_config::CssConfig>,
    /// The client's numeric `build.assetsInlineLimit` (its environment's own
    /// first), for the urls a dev sheet names.
    inline_limit: u64,
    modules: oj_css::CssModulesOptions,
    engines: StyleEngines,
    /// `build.cssMinify` per environment and `build.cssTarget`.
    minify_client: bool,
    minify_ssr: bool,
    targets: Vec<String>,
    /// The dev urls' prefix under `base`.
    url_base: String,
    /// The sheets the dev client bundle ran through the plugins, after their
    /// "normal" transforms, stamped with the bundle that compiled them: a
    /// page's `<link>` serves one without running the plugins again until the
    /// next bundle or style edit, as Vite serves a sheet's import and its CSS
    /// from one transform.
    dev_sheets: Mutex<HashMap<PathBuf, DevSheet>>,
    dev_stamp: std::sync::atomic::AtomicU64,
}

impl Styles {
    pub fn new(start: &StartConfig) -> Styles {
        Styles {
            root: start.root.clone(),
            css: start.config.css.clone(),
            inline_limit: start
                .config
                .build
                .as_ref()
                .and_then(|b| {
                    b.assets_inline_limit_envs
                        .as_ref()
                        .and_then(|envs| envs.get("client")?.as_u64())
                        .or(b.assets_inline_limit)
                })
                .unwrap_or(4096),
            modules: oj_server::css_modules_options(&start.config),
            engines: StyleEngines::default(),
            minify_client: oj_config::build_css_minify(&start.config, false),
            minify_ssr: oj_config::build_css_minify(&start.config, true),
            targets: oj_config::build_css_targets(&start.config),
            url_base: start.dev_url_base(),
            dev_sheets: Mutex::new(HashMap::new()),
            dev_stamp: std::sync::atomic::AtomicU64::new(0),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// A dev bundle started, or a style input changed: every kept sheet may
    /// be stale.
    pub fn dev_sheets_stale(&self) {
        self.dev_stamp
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    /// The current stamp, read before a sheet's sources are, so an edit that
    /// lands mid-compile leaves what the compile keeps stale.
    pub(crate) fn dev_stamp(&self) -> u64 {
        self.dev_stamp.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// `loaded`: what a plugin's `load` returned for a sheet with no file.
    pub(crate) fn keep_dev_sheet(
        &self,
        path: &Path,
        css: String,
        stamp: u64,
        loaded: Option<&str>,
    ) {
        let sheet = DevSheet {
            stamp,
            loaded: loaded.map(text_hash),
            css,
        };
        self.dev_sheets
            .lock()
            .unwrap()
            .insert(path.to_path_buf(), sheet);
    }

    /// The sheet the current dev bundle compiled for the file at `path`, if
    /// it did.
    pub fn dev_sheet(&self, path: &Path) -> Option<String> {
        let stamp = self.dev_stamp.load(std::sync::atomic::Ordering::SeqCst);
        self.dev_sheets
            .lock()
            .unwrap()
            .get(path)
            .filter(|s| s.loaded.is_none() && s.stamp == stamp)
            .map(|s| s.css.clone())
    }

    /// The sheet the dev bundle compiled for a plugin-served `id`, if the
    /// plugin still loads the same text for it.
    pub fn dev_served_sheet(&self, id: &Path, loaded: &str) -> Option<String> {
        self.dev_sheets
            .lock()
            .unwrap()
            .get(id)
            .filter(|s| s.loaded == Some(text_hash(loaded)))
            .map(|s| s.css.clone())
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
            inline: Box::new(|file, url| {
                Ok(oj_server::inline_url_under(file, url, self.inline_limit))
            }),
        }
    }

    /// A CSS module's or a preprocessor file's compile, from its source
    /// (after the "pre" plugin transforms): its JS (the class map) and its CSS.
    pub async fn compile(
        &self,
        path: &Path,
        source: String,
    ) -> Result<oj_server::StyleModule, String> {
        let engine = if oj_server::needs_preprocess_engine(path) {
            Some(self.engines.preprocess(&self.root).await?)
        } else {
            None
        };
        oj_server::compile_style_file(&self.inputs(), path, source, engine.as_deref()).await
    }

    /// A dev `?inline` sheet: the module the dev server renders for SSR,
    /// through the "pre" and "normal" slots of `slots`.
    pub async fn dev_inline(
        &self,
        path: &Path,
        slots: Option<oj_server::StyleSlots<'_>>,
    ) -> Result<String, String> {
        let source = oj_server::read_style_source(slots, path).await?;
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
            &self.url_base,
            slots,
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

/// Bundles a worker entry on its own (Vite's bundleWorkerEntry), given the
/// entry and its bundle chain (the workers importing it, then the entry).
pub(crate) type WorkerBundler = Arc<
    dyn Fn(
            PathBuf,
            Vec<PathBuf>,
        ) -> std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<WorkerBundle, String>> + Send>,
        > + Send
        + Sync,
>;

/// Worker bundles in flight by entry and asking chain, each shared by the
/// loads that wait on it.
type WorkerBundling = HashMap<(PathBuf, Vec<PathBuf>), Arc<tokio::sync::OnceCell<WorkerBundle>>>;

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
    /// The URL each client stylesheet was emitted at, by its module.
    sheets: Mutex<HashMap<PathBuf, String>>,
    /// Every stylesheet emitted, linked or a `?url` asset, in emit order.
    emitted_css: Mutex<Vec<String>>,
    worker_bundler: Mutex<Option<WorkerBundler>>,
    /// `worker.format` is "es": workers start as modules, not classic scripts.
    worker_module: std::sync::atomic::AtomicBool,
    /// Finished worker bundles, by entry (Vite's worker output cache).
    worker_codes: Mutex<HashMap<PathBuf, WorkerBundle>>,
    /// Bundles in flight, by entry and the chain that asked: as in Vite, an
    /// unfinished bundle is never shared with another chain, which could be
    /// the one it waits on (a cycle between two workers the client imports).
    worker_bundling: Mutex<WorkerBundling>,
    /// Worker entries written to the output.
    worker_files: Mutex<std::collections::HashSet<String>>,
    /// The first worker bundle that failed: the parent bundle reports it as a
    /// failed load of the worker, and the worker's own error is what Vite shows.
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
            sheets: Mutex::new(HashMap::new()),
            emitted_css: Mutex::new(Vec::new()),
            worker_bundler: Mutex::new(None),
            worker_module: std::sync::atomic::AtomicBool::new(false),
            worker_codes: Mutex::new(HashMap::new()),
            worker_bundling: Mutex::new(HashMap::new()),
            worker_files: Mutex::new(Default::default()),
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

    /// Puts the linked sheets in the client bundle's execution order, the
    /// order Vite's css-post collects them in: they were linked as their
    /// loads finished, which rolldown runs concurrently.
    pub fn order_links(&self, outputs: &[rolldown_common::Output]) {
        let order = module_order(outputs);
        let sheets = self.sheets.lock().unwrap();
        let mut at: HashMap<&str, usize> = HashMap::new();
        for (path, url) in sheets.iter() {
            if let Some(&i) = path.to_str().and_then(|p| order.get(p)) {
                let slot = at.entry(url.as_str()).or_insert(i);
                *slot = (*slot).min(i);
            }
        }
        self.css_urls
            .lock()
            .unwrap()
            .sort_by_key(|url| at.get(url.as_str()).copied().unwrap_or(usize::MAX));
    }

    /// Writes `bytes` as the file at `path` names it, content-hashed; its URL.
    fn write(&self, path: &Path, bytes: &[u8]) -> Result<String, String> {
        let ext = path
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let stem: String = path
            .file_stem()
            .map(|s| s.to_string_lossy().trim_start_matches('\0').to_string())
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
        // A stylesheet hashes as Vite names one (with its hash marker).
        let hash = if ext == ".css" {
            crate::build::css_asset_hash(&String::from_utf8_lossy(bytes))
        } else {
            crate::build::content_hash(bytes)
        };
        let hash = &hash[..8];
        let name = format!("{stem}-{hash}{ext}");
        if self.written.lock().unwrap().insert(name.clone()) {
            std::fs::create_dir_all(&self.assets_dir).map_err(|e| e.to_string())?;
            std::fs::write(self.assets_dir.join(&name), bytes).map_err(|e| e.to_string())?;
        }
        Ok(format!("{}assets/{name}", self.base))
    }

    /// CSS a worker bundle's plugins routed through its `vite:css-post`:
    /// emitted beside the worker as Vite's worker build emits its chunk's
    /// sheet, minified under cssMinify, linked from nowhere.
    pub fn worker_css(&self, entry: &Path, css: &str) -> Result<String, String> {
        let path = entry.with_extension("css");
        let css = if self.styles.minify_client {
            format!("{}\n", self.styles.minify(&path, css)?)
        } else {
            css.to_string()
        };
        self.write(&path, css.as_bytes())
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
                return self.css(path, css, None).await;
            }
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            self.write(path, &bytes)
        })
    }

    /// A stylesheet's text emitted as the file at `path` (a CSS module's
    /// scoped sheet is not the file's bytes): Tailwind when it uses it, the
    /// "normal" plugin transforms of `slots`, its `url()`s emitted, minified
    /// under the client's cssMinify (ending in a newline, as Vite's
    /// finalizeCss), then hashed and recorded.
    pub async fn css(
        &self,
        path: &Path,
        css: String,
        slots: Option<oj_server::StyleSlots<'_>>,
    ) -> Result<String, String> {
        self.sheet(path, css, slots, true).await
    }

    /// [`Emitter::css`] for a `?url` import: the same file, which the page
    /// does not link (Vite emits it as an asset, apart from the chunk's CSS).
    pub async fn css_asset(
        &self,
        path: &Path,
        css: String,
        slots: Option<oj_server::StyleSlots<'_>>,
    ) -> Result<String, String> {
        self.sheet(path, css, slots, false).await
    }

    async fn sheet(
        &self,
        path: &Path,
        css: String,
        slots: Option<oj_server::StyleSlots<'_>>,
        link: bool,
    ) -> Result<String, String> {
        self.emitting.lock().unwrap().insert(path.to_path_buf());
        let result = async {
            let css = self.styles.tailwind(path, css).await?;
            let css = oj_server::style_slot(slots, css, "normal").await?;
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
            {
                let mut emitted = self.emitted_css.lock().unwrap();
                if !emitted.contains(&url) {
                    emitted.push(url.clone());
                }
            }
            if !link {
                return Ok(url);
            }
            let mut urls = self.css_urls.lock().unwrap();
            if !urls.contains(&url) {
                urls.push(url.clone());
            }
            self.sheets
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), url.clone());
            Ok(url)
        }
        .await;
        self.emitting.lock().unwrap().remove(path);
        result
    }

    /// CSS a plugin routed through the client's `vite:css-post` for module
    /// `id` (UnoCSS's generated layers): it takes the place of the sheet
    /// emitted for that module, as vite:css-post keeps one per module, and is
    /// minified and named like any client sheet. CSS for no emitted sheet is
    /// linked as a sheet of its own.
    pub fn plugin_css(&self, id: &str, css: &str) -> Result<(), String> {
        let path = match id.split(['?', '#']).next().unwrap_or(id) {
            "" => self.assets_dir.join("plugin.css"),
            clean => PathBuf::from(clean),
        };
        let css = if self.styles.minify_client {
            format!("{}\n", self.styles.minify(&path, css)?)
        } else {
            css.to_string()
        };
        let url = self.write(&path.with_extension("css"), css.as_bytes())?;
        let old = self
            .sheets
            .lock()
            .unwrap()
            .insert(path.clone(), url.clone());
        if let Some(old) = &old {
            for u in self.emitted_css.lock().unwrap().iter_mut() {
                if u == old {
                    *u = url.clone();
                }
            }
        }
        let mut urls = self.css_urls.lock().unwrap();
        match old.and_then(|old| urls.iter().position(|u| *u == old).map(|i| (i, old))) {
            Some((i, old)) if old != url => {
                urls[i] = url;
                // Nothing else links the placeholder sheet.
                if let Some((_, file)) = old.rsplit_once("assets/") {
                    let _ = std::fs::remove_file(self.assets_dir.join(file));
                }
            }
            Some(_) => {}
            None if !urls.contains(&url) => urls.push(url),
            None => {}
        }
        Ok(())
    }

    /// A plugin's late CSS (the StyleX sheet, complete only once every module
    /// compiled) appended to the first stylesheet the client emitted, as the
    /// plugin's generateBundle does (`current + '\n' + css`): the file keeps
    /// the name it was hashed and linked under. With no stylesheet it is the
    /// plugin's writeBundle fallback, `assets/<name>`, linked from nowhere.
    pub fn append_generated_css(&self, name: &str, css: &str) -> Result<(), String> {
        // The first CSS asset in the bundle, a `?url` one as well (the app
        // links that one itself).
        let first = self.emitted_css.lock().unwrap().first().cloned();
        let file = match first.as_deref().and_then(|u| u.rsplit_once("assets/")) {
            Some((_, file)) => self.assets_dir.join(file),
            None => {
                std::fs::create_dir_all(&self.assets_dir).map_err(|e| e.to_string())?;
                return std::fs::write(self.assets_dir.join(name), css).map_err(|e| e.to_string());
            }
        };
        let current =
            std::fs::read_to_string(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let joined = if current.is_empty() {
            css.to_string()
        } else {
            format!("{current}\n{css}")
        };
        std::fs::write(&file, joined).map_err(|e| format!("{}: {e}", file.display()))
    }

    /// A `?inline` sheet's text: compiled and its `url()`s emitted as `css`
    /// does, but returned, minified under the importing environment's
    /// cssMinify with no trailing newline.
    pub async fn inline_css(
        &self,
        path: &Path,
        css: String,
        server: bool,
        slots: Option<oj_server::StyleSlots<'_>>,
    ) -> Result<String, String> {
        let css = self.styles.tailwind(path, css).await?;
        let css = oj_server::style_slot(slots, css, "normal").await?;
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

    /// `chain` (the workers whose bundles import `path`, empty from the
    /// client or server) with `path` last; a worker importing itself through
    /// them fails, as in Vite, before it could wait on its own bundle.
    fn worker_chain(&self, path: &Path, chain: &[PathBuf]) -> Result<Vec<PathBuf>, String> {
        let mut new_chain = chain.to_vec();
        new_chain.push(path.to_path_buf());
        if !chain.iter().any(|p| p == path) {
            return Ok(new_chain);
        }
        let root = self.styles.root();
        let names: Vec<String> = new_chain
            .iter()
            .map(|p| {
                p.strip_prefix(root)
                    .unwrap_or(p)
                    .to_string_lossy()
                    .replace('\\', "/")
            })
            .collect();
        Err(format!(
            "Circular worker imports detected. Vite does not support it. Import chain: {}",
            names.join(" -> ")
        ))
    }

    /// A worker entry's bundle under `chain`: a finished one is reused, as
    /// Vite's worker output cache is.
    pub async fn worker_code(
        &self,
        path: &Path,
        chain: &[PathBuf],
    ) -> Result<WorkerBundle, String> {
        let new_chain = self.worker_chain(path, chain)?;
        if let Some(done) = self.worker_codes.lock().unwrap().get(path) {
            return Ok(done.clone());
        }
        let cell = Arc::clone(
            self.worker_bundling
                .lock()
                .unwrap()
                .entry((path.to_path_buf(), chain.to_vec()))
                .or_default(),
        );
        let bundler = self.worker_bundler.lock().unwrap().clone();
        let result = cell
            .get_or_try_init(|| async move {
                let bundler = bundler
                    .ok_or_else(|| format!("oj: no worker bundler for {}", path.display()))?;
                bundler(path.to_path_buf(), new_chain).await
            })
            .await
            .cloned();
        if let Ok(done) = &result {
            self.worker_codes
                .lock()
                .unwrap()
                .entry(path.to_path_buf())
                .or_insert_with(|| done.clone());
        }
        if let Err(e) = &result {
            self.worker_error
                .lock()
                .unwrap()
                .get_or_insert_with(|| e.clone());
        }
        result
    }

    /// The emitted URL of a worker entry's bundle.
    pub async fn worker(&self, path: &Path, chain: &[PathBuf]) -> Result<String, String> {
        let bundle = self.worker_code(path, chain).await?;
        if self
            .worker_files
            .lock()
            .unwrap()
            .insert(bundle.file_name.clone())
        {
            let assets = self.assets_dir.parent().unwrap_or(&self.assets_dir);
            let dest = assets.join(&bundle.file_name);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(&dest, bundle.code.as_bytes()).map_err(|e| e.to_string())?;
        }
        Ok(format!("{}{}", self.base, bundle.file_name))
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

/// A dev client bundle's linked stylesheets, each with the module that
/// linked it, in the order their loads finished.
#[derive(Default)]
pub(crate) struct LinkedSheets(Vec<(String, String)>);

impl LinkedSheets {
    fn link(&mut self, id: &str, href: String) {
        if !self.0.iter().any(|(_, h)| *h == href) {
            self.0.push((id.to_string(), href));
        }
    }

    /// The links in the order they were made.
    pub fn urls(&self) -> Vec<String> {
        self.0.iter().map(|(_, h)| h.clone()).collect()
    }

    /// The links in the bundle's execution order, as Vite's dev client
    /// injects sheets when their modules run; one no chunk holds goes last.
    pub fn in_bundle_order(&self, outputs: &[rolldown_common::Output]) -> Vec<String> {
        let order = module_order(outputs);
        let mut links: Vec<&(String, String)> = self.0.iter().collect();
        links.sort_by_key(|(id, _)| order.get(id.as_str()).copied().unwrap_or(usize::MAX));
        links.into_iter().map(|(_, h)| h.clone()).collect()
    }
}

/// Each module's place in a bundle's execution order, as Vite's
/// getCssFilesForChunk walks a chunk's sheets: the chunks a chunk imports
/// run before it, depth first from the entries, then the dynamic chunks in
/// the order they are imported; a chunk nothing reaches goes last. A chunk
/// lists its own modules in execution order. Keyed by id and by its path.
fn module_order(outputs: &[rolldown_common::Output]) -> HashMap<&str, usize> {
    use rolldown_common::OutputChunk;
    fn visit<'a>(
        chunk: &'a OutputChunk,
        by_name: &HashMap<&str, &'a OutputChunk>,
        seen: &mut std::collections::HashSet<&'a str>,
        ordered: &mut Vec<&'a OutputChunk>,
    ) {
        if !seen.insert(chunk.filename.as_str()) {
            return;
        }
        for name in &chunk.imports {
            if let Some(imported) = by_name.get(name.as_str()) {
                visit(imported, by_name, seen, ordered);
            }
        }
        ordered.push(chunk);
    }
    let chunks: Vec<&OutputChunk> = outputs
        .iter()
        .filter_map(|o| match o {
            rolldown_common::Output::Chunk(c) => Some(&**c),
            rolldown_common::Output::Asset(_) => None,
        })
        .collect();
    let by_name: HashMap<&str, &OutputChunk> =
        chunks.iter().map(|c| (c.filename.as_str(), *c)).collect();
    let (mut seen, mut ordered) = (Default::default(), Vec::new());
    for entry in chunks.iter().filter(|c| c.is_entry) {
        visit(entry, &by_name, &mut seen, &mut ordered);
    }
    let mut next = 0;
    while let Some(chunk) = ordered.get(next).copied() {
        next += 1;
        for name in &chunk.dynamic_imports {
            if let Some(imported) = by_name.get(name.as_str()) {
                visit(imported, &by_name, &mut seen, &mut ordered);
            }
        }
    }
    for chunk in &chunks {
        visit(chunk, &by_name, &mut seen, &mut ordered);
    }
    let mut order = HashMap::new();
    for (i, id) in ordered.iter().flat_map(|c| c.module_ids.iter()).enumerate() {
        let id = id.as_str();
        order.entry(id).or_insert(i);
        order
            .entry(id.split(['?', '#']).next().unwrap_or(id))
            .or_insert(i);
    }
    order
}

/// Where the asset modules point.
pub(crate) enum AssetMode {
    /// The dev server's files: `/@oj-start/fs/<abs>`, workers on the dev
    /// pipeline. `css_urls` collects the linked stylesheets.
    Dev {
        css_urls: Arc<Mutex<LinkedSheets>>,
        state: Arc<DevStyleState>,
        /// The dev urls' prefix under `base`.
        url_base: String,
    },
    /// The build's emitter; `server` for the server bundle (which links no
    /// stylesheet and minifies `?inline` sheets under ssr's cssMinify).
    /// `worker_chain` is a worker bundle's chain (empty for the client and
    /// server), the workers it imports bundling under it.
    Build {
        emitter: Arc<Emitter>,
        server: bool,
        worker_chain: Vec<PathBuf>,
    },
}

pub(crate) const DEV_FS_BASE: &str = "/@oj-start/fs";

/// Where the dev server serves a stylesheet a plugin serves with no file
/// behind it: its id in Vite's `/@id/` spelling (`\0` as `__x00__`).
pub(crate) const DEV_ID_BASE: &str = "/@oj-start/id/";

pub(crate) fn dev_id_url(url_base: &str, id: &str) -> String {
    let mut out = format!("{url_base}{DEV_ID_BASE}");
    for c in id.replace('\0', "__x00__").chars() {
        match c {
            '%' | '?' | '#' | ' ' | '"' | '<' | '>' => out.push_str(&format!("%{:02X}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// The id a [`dev_id_url`] names, from the part after [`DEV_ID_BASE`]
/// (percent-decoded already).
pub(crate) fn id_of_dev_url(rest: &str) -> String {
    rest.replace("__x00__", "\0")
}

pub(crate) struct StartAssetsPlugin {
    root: PathBuf,
    styles: Arc<Styles>,
    mode: AssetMode,
    /// The bundle's plugin host, whose "pre" and "normal" transforms a
    /// stylesheet runs through as it compiles ("post" ones run on the module
    /// this loads, as rolldown transforms it).
    plugins: Option<Arc<oj_server::plugins::PluginHost>>,
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
            plugins: None,
        }
    }

    pub fn with_plugins(mut self, host: Option<Arc<oj_server::plugins::PluginHost>>) -> Self {
        self.plugins = host;
        self
    }

    async fn url_for(&self, path: &Path) -> Result<String, String> {
        match &self.mode {
            AssetMode::Dev { url_base, .. } => {
                Ok(format!("{url_base}{DEV_FS_BASE}{}", path.display()))
            }
            AssetMode::Build { emitter, .. } => emitter.emit(path).await,
        }
    }

    /// A worker's URL: the dev pipeline's (root-relative, `/@fs` outside the
    /// root) with Vite's `worker_file&type=<worker_type>`, which that pipeline
    /// serves with the env prelude, or the build's emitted bundle.
    async fn worker_url_for(&self, path: &Path, worker_type: &str) -> Result<String, String> {
        match &self.mode {
            AssetMode::Dev { url_base, .. } => Ok(match path.strip_prefix(&self.root) {
                Ok(rel) => format!(
                    "{url_base}/{}?worker_file&type={worker_type}",
                    rel.to_string_lossy().replace('\\', "/")
                ),
                Err(_) => format!(
                    "{url_base}/@fs{}?worker_file&type={worker_type}",
                    path.display()
                ),
            }),
            AssetMode::Build {
                emitter,
                worker_chain,
                ..
            } => emitter.worker(path, worker_chain).await,
        }
    }

    async fn module(&self, id: &str, path: &Path) -> Result<Option<String>, String> {
        let json = |s: &str| serde_json::Value::String(s.to_string()).to_string();
        let dev = matches!(self.mode, AssetMode::Dev { .. });
        // A plugin-served sheet's `\0` id takes the tag of what it names.
        let Some(tag) = asset_tag(id.trim_start_matches('\0'), dev) else {
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
        let slots = self
            .plugins
            .as_deref()
            .map(|host| oj_server::StyleSlots { host, id });
        Ok(Some(match tag {
            AssetTag::Raw => {
                let text = std::fs::read_to_string(path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                format!("export default {};", json(&text))
            }
            // Vite's vite:css takes a built `?url` sheet as `?transform-only`:
            // compiled through the plugins under that id and emitted like a
            // linked sheet, its module importing that id for the "post" ones.
            AssetTag::Url if is_style_path(&path_str) && !dev => {
                let AssetMode::Build { emitter, .. } = &self.mode else {
                    unreachable!("a build mode")
                };
                let only = format!("{}?transform-only", path.display());
                let slots = self
                    .plugins
                    .as_deref()
                    .map(|host| oj_server::StyleSlots { host, id: &only });
                let source = oj_server::read_style_source(slots, path).await?;
                let source = oj_server::style_slot(slots, source, "pre").await?;
                let css = if is_css_module(&path_str) || !path_str.ends_with(".css") {
                    self.styles.compile(path, source).await?.css
                } else {
                    source
                };
                let url = emitter.css_asset(path, css, slots).await?;
                format!("import {};\nexport default {};", json(&only), json(&url))
            }
            AssetTag::Url => format!("export default {};", json(&self.url_for(path).await?)),
            AssetTag::Inline if is_style_path(&path_str) => match &self.mode {
                AssetMode::Dev { state, .. } => {
                    state
                        .compiled_sheets
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    self.styles.dev_inline(path, slots).await?
                }
                AssetMode::Build {
                    emitter, server, ..
                } => {
                    let source = oj_server::read_style_source(slots, path).await?;
                    let source = oj_server::style_slot(slots, source, "pre").await?;
                    let compiled = self.styles.compile(path, source).await?;
                    // A build gives a CSS module's class map even inlined.
                    if is_css_module(&path_str) {
                        oj_server::style_slot(slots, compiled.css, "normal").await?;
                        compiled.code
                    } else {
                        let css = emitter
                            .inline_css(path, compiled.css, *server, slots)
                            .await?;
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
                // Vite's vite:worker: a worker importing its own entry starts
                // itself again, at its own URL.
                let own = matches!(
                    &self.mode,
                    AssetMode::Build { worker_chain, .. } if worker_chain.last().is_some_and(|p| p == path)
                );
                match (form, &self.mode) {
                    (
                        WorkerForm::Inline,
                        AssetMode::Build {
                            emitter,
                            worker_chain,
                            ..
                        },
                    ) if !own => inline_worker_module(
                        shared,
                        &emitter.worker_code(path, worker_chain).await?.code,
                        module,
                    ),
                    _ => {
                        // `module`, as Vite's dev worker load names every
                        // `?worker` import, unless oj's rewrite of
                        // `new Worker(new URL(...), opts)` passed the options' type.
                        let worker_type = match form {
                            WorkerForm::Url => id
                                .split_once('?')
                                .map_or("", |(_, q)| q)
                                .split('&')
                                .find_map(|kv| kv.strip_prefix("worker_type="))
                                .filter(|t| matches!(*t, "classic" | "ignore"))
                                .unwrap_or("module"),
                            _ => "module",
                        };
                        let url = if own {
                            "self.location.href".to_string()
                        } else {
                            json(&self.worker_url_for(path, worker_type).await?)
                        };
                        if matches!(form, WorkerForm::Url) {
                            format!("export default {url};")
                        } else {
                            format!(
                                "export default function WorkerWrapper(options) {{ return new {ctor}({url}, {}); }}",
                                worker_type_option(module)
                            )
                        }
                    }
                }
            }
            AssetTag::Css => {
                // A CSS module or a preprocessor file compiles on oj's CSS
                // pipeline; a plain `.css` is linked as written (a build
                // emits it compiled).
                let emits = matches!(self.mode, AssetMode::Build { server: false, .. });
                let stamp = self.styles.dev_stamp();
                let source = oj_server::read_style_source(slots, path).await?;
                let on_disk = path.is_file();
                let loaded = (!on_disk).then(|| source.clone());
                // Only the client build ships this compile. A dev page links
                // the sheet, and that `?direct` request runs every slot; the
                // import (and the server build's) runs them too when a plugin
                // takes the sheet, as in Vite, sparing a compile when none does.
                let slots = slots.filter(|s| emits || s.host.hook_wants_transform(id, &source));
                let source = oj_server::style_slot(slots, source, "pre").await?;
                let compiled = if is_css_module(&path_str) || !path_str.ends_with(".css") {
                    if let AssetMode::Dev { state, .. } = &self.mode {
                        state
                            .compiled_sheets
                            .store(true, std::sync::atomic::Ordering::SeqCst);
                    }
                    Some(self.styles.compile(path, source.clone()).await?)
                } else {
                    None
                };
                let css = || {
                    compiled
                        .as_ref()
                        .map_or_else(|| source.clone(), |c| c.css.clone())
                };
                match &self.mode {
                    AssetMode::Dev {
                        css_urls, url_base, ..
                    } => {
                        if slots.is_some() {
                            let css = self.styles.tailwind(path, css()).await?;
                            let css = oj_server::style_slot(slots, css, "normal").await?;
                            self.styles
                                .keep_dev_sheet(path, css, stamp, loaded.as_deref());
                        }
                        let href = if path.is_file() {
                            self.url_for(path).await?
                        } else {
                            dev_id_url(url_base, id)
                        };
                        css_urls.lock().unwrap().link(id, href);
                    }
                    AssetMode::Build {
                        emitter,
                        server: false,
                        ..
                    } => {
                        emitter.css(path, css(), slots).await?;
                    }
                    AssetMode::Build { server: true, .. } => {
                        if slots.is_some() {
                            let css = self.styles.tailwind(path, css()).await?;
                            oj_server::style_slot(slots, css, "normal").await?;
                        }
                    }
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
        let path = Path::new(id.split('?').next().unwrap_or(id));
        // A `?url` sheet's compile, done by its `?url` module: what remains is
        // the module the "post" plugins see, empty as vite:css-post leaves it.
        if id.ends_with("?transform-only") && is_style_path(&path.to_string_lossy()) {
            return Ok(Some(HookLoadOutput {
                code: arcstr::ArcStr::from(""),
                module_type: Some(ModuleType::Js),
                side_effects: Some(rolldown_common::side_effects::HookSideEffects::NoTreeshake),
                ..Default::default()
            }));
        }
        let served = self.plugins.is_some() && super::host::is_served_stylesheet(id);
        if !served && (id.starts_with('\0') || !path.is_absolute()) {
            return Ok(None);
        }
        let code = self
            .module(id, path)
            .await
            .map_err(|e| oj_server::load_error::load_failed(self.name(), id, anyhow::anyhow!(e)))?;
        // A plain sheet stays in its chunk as well, as vite:css-post keeps
        // one: the chunk's modules then place its link.
        let plain = !id.contains('?') && is_style_path(id) && !is_css_module(id);
        Ok(code.map(|code| HookLoadOutput {
            code: arcstr::ArcStr::from(code),
            module_type: Some(ModuleType::Js),
            // A plugin-served sheet stays in its chunk, as Vite keeps CSS
            // modules (`no-treeshake`): UnoCSS finds its layers by the
            // chunk's modules.
            side_effects: (served || plain)
                .then_some(rolldown_common::side_effects::HookSideEffects::NoTreeshake),
            ..Default::default()
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plugin-served sheet's dev link, in Vite's `/@id/` spelling, names
    /// its id again.
    #[test]
    fn dev_id_urls_round_trip_the_id() {
        for id in [
            "\0virtual:theme.css",
            "/__uno.css",
            "\0virtual:a b.css?lang=css",
        ] {
            let url = dev_id_url("/base", id);
            assert!(url.starts_with("/base/@oj-start/id/"), "{url}");
            assert!(
                !url.contains('\0') && !url.contains('?') && !url.contains(' '),
                "{url}"
            );
            let rest = url.strip_prefix(&format!("/base{DEV_ID_BASE}")).unwrap();
            let decoded = rest.replace("%3F", "?").replace("%20", " ");
            assert_eq!(id_of_dev_url(&decoded), id);
        }
        assert_eq!(
            dev_id_url("", "\0virtual:theme.css"),
            "/@oj-start/id/__x00__virtual:theme.css"
        );
    }

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
