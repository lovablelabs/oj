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

/// Vite's noInlineRE (`[?&]no-inline\b`): also `?no-inline=...` and
/// `?no-inline-x`, as [`has_inline`] reads `inline`.
fn has_no_inline(query: &str) -> bool {
    oj_server::has_word_param(&format!("?{}", query.trim_start_matches('?')), "no-inline")
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

/// A file type whose bare import is its URL: Vite's `KNOWN_ASSET_TYPES`
/// (oj_compiler's copy, images, media, fonts, pdf and txt among them), plus
/// `.wasm`, which oj has always served by URL. An `.svg` is left to the
/// plugins first (svgr's `?react`, a componentizing plugin) and falls back to
/// its URL in [`StartAssetsPlugin::module`]. A type outside the list that
/// rolldown read as JavaScript failed the bundle with "stream did not
/// contain valid UTF-8" on the first binary byte.
fn is_asset_ext(ext: &str) -> bool {
    ext.eq_ignore_ascii_case("wasm")
        || (!ext.eq_ignore_ascii_case("svg") && oj_compiler::assets::is_asset_ext(ext))
}
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
pub(crate) fn asset_tag(root: &Path, source: &str, dev: bool) -> Option<AssetTag> {
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
    if has_no_inline(query) {
        return Some(AssetTag::Url);
    }
    if has_inline(query) {
        return Some(AssetTag::Inline);
    }
    if has_flag(query, "url")
        || is_asset_ext(ext_of(path))
        || oj_compiler::assets::is_included_asset(root, path)
    {
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
    /// The client's `build.assetsInlineLimit` (see [`client_inline_limit`]).
    pub(crate) fn inline_limit(&self) -> u64 {
        self.inline_limit
    }

    pub fn new(start: &StartConfig) -> Styles {
        Styles {
            root: start.root.clone(),
            css: start.config.css.clone(),
            inline_limit: client_inline_limit(&start.config),
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

/// Whether a sheet needs Tailwind: any of its directives, as the server's
/// own gate decides (a second list here fell behind it, and a sheet with a
/// `@reference` alone was emitted as written).
pub(crate) fn needs_css_compile(src: &str) -> bool {
    oj_server::needs_css_compile(src)
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
/// A chunk's stylesheet as Vite's css-post emits it in renderChunk: the
/// chunk's CSS modules' sheets concatenated in module order, named after the
/// chunk and hashed by its content. `pure`: the chunk holds nothing but plain
/// CSS (no exports, no JS), so generateBundle drops it and the chunks that
/// imported it take its sheet, as Vite drops a pure CSS chunk.
struct ChunkSheet {
    /// The chunk's file name as renderChunk sees it (rolldown's preliminary
    /// name, a hash placeholder in it until the bundle is written).
    chunk: String,
    /// The sheet's file, relative to the client's output directory.
    file: String,
    url: String,
    modules: Vec<PathBuf>,
    pure: bool,
}

/// `output.assetFileNames` for the sheets written here: a pattern, or the
/// app's function, called in the plugin host with the asset as Vite's
/// `PreRenderedAsset` describes it.
pub(crate) enum AssetNamer {
    Pattern(String),
    Host(Arc<oj_server::plugins::PluginHost>, u64),
}

/// Rolldown's sanitizeFileName for a `[name]`: the characters a file system
/// or a URL could take amiss become `_`; a `/` stays, so a chunk group's
/// `dir/name` keeps its directory.
fn sanitize_file_name(name: &str) -> String {
    name.chars()
        .map(|c| match c {
            '\u{0}'..='\u{1f}' | '\u{7f}' => '_',
            '"' | '#' | '$' | '%' | '&' | '*' | '+' | ',' | ':' | ';' | '<' | '=' | '>' | '?'
            | '[' | ']' | '^' | '`' | '{' | '|' | '}' => '_',
            c => c,
        })
        .collect()
}

/// The `[name]` of a chunk's stylesheet, as Vite's css-post names the asset:
/// an entry's (whose facade is not a stylesheet) is the chunk name's base
/// name with `.css` appended (`index`); any other chunk's is the chunk name
/// with its extension replaced by `.css` (`ensureFileExt`: `routeTree.gen`
/// becomes `routeTree`, `dir/name` keeps its directory).
pub(crate) fn chunk_sheet_stem(name: &str, is_entry: bool, facade_is_css: bool) -> String {
    if is_entry && !facade_is_css {
        return name.rsplit('/').next().unwrap_or(name).to_string();
    }
    let base_start = name.rfind('/').map_or(0, |i| i + 1);
    match name[base_start..].rfind('.') {
        // A leading dot is a hidden file's, not an extension's.
        Some(i) if i > 0 => name[..base_start + i].to_string(),
        _ => name.to_string(),
    }
}

pub(crate) struct Emitter {
    assets_dir: PathBuf,
    base: String,
    styles: Arc<Styles>,
    written: Mutex<std::collections::HashSet<String>>,
    /// Stylesheets mid-emit (a sheet's own `url()` naming itself must not recurse).
    emitting: Mutex<std::collections::HashSet<PathBuf>>,
    /// The client stylesheets the root route links, in link order.
    css_urls: Mutex<Vec<String>>,
    /// The finished sheet of each client CSS module (compiled, its `url()`s
    /// emitted, minified), waiting for the chunk that holds the module: as
    /// Vite's css-post keeps a module's CSS in `styles` until renderChunk
    /// emits the chunk's one sheet, so a sheet is named after its chunk.
    module_css: Mutex<HashMap<PathBuf, String>>,
    /// The chunks' stylesheets, as renderChunk wrote them.
    chunk_sheets: Mutex<Vec<ChunkSheet>>,
    /// The sheets a chunk took over from the pure CSS chunks it imported
    /// (generateBundle dropped them), by the chunk's preliminary file name.
    inherited: Mutex<HashMap<String, Vec<String>>>,
    /// The pure CSS chunks generateBundle dropped, by final file name, each
    /// with its sheets (its own and the ones it had taken over): a chunk
    /// that imported one with `import()` still names the file, and the
    /// preload helper answers for it as Vite's does (its sheets alone, the
    /// import itself replaced).
    removed_pure: Mutex<HashMap<String, Vec<String>>>,
    /// A plugin's CSS for a module no chunk holds (`plugin_css`), by module.
    standalone: Mutex<HashMap<PathBuf, String>>,
    /// `output.assetFileNames` for the sheets written here.
    asset_namer: Mutex<Option<AssetNamer>>,
    /// The environment's `build.cssMinify` (the client's, or ssr's for a
    /// server bundle that emits its assets).
    minify: bool,
    /// A server bundle's emitter (`build.emitAssets` on, as a Cloudflare
    /// Worker's is): its sheets and `url()` files go under its own output
    /// directory, as Vite's css-post and asset plugin write them there.
    server: bool,
    /// Every stylesheet emitted, linked or a `?url` asset, in emit order.
    emitted_css: Mutex<Vec<String>>,
    /// The assets written for a module (`?url` imports, workers, a sheet's
    /// `url()`s), by the importing module's id: Vite's
    /// `viteMetadata.importedAssets`, listed in the manifest's `assets`.
    module_assets: Mutex<HashMap<String, Vec<String>>>,
    /// Every asset written, with its source file: the manifest's asset rows
    /// (Vite emits them through rolldown; oj writes them itself).
    asset_rows: Mutex<Vec<(PathBuf, String)>>,
    /// The sheets a generateBundle hook replaced (see `rename_sheet`): by
    /// the file oj wrote, the file that took its place and its text. Shared
    /// with the server's emitter, whose `?url` import of the same sheet
    /// names the replacement, as under Vite.
    renamed: Arc<Mutex<HashMap<String, (String, String)>>>,
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
        let minify = styles.minify_client;
        Emitter {
            assets_dir: client_dir.join("assets"),
            base: base.to_string(),
            styles,
            written: Mutex::new(Default::default()),
            emitting: Mutex::new(Default::default()),
            css_urls: Mutex::new(Vec::new()),
            module_css: Mutex::new(HashMap::new()),
            chunk_sheets: Mutex::new(Vec::new()),
            inherited: Mutex::new(HashMap::new()),
            removed_pure: Mutex::new(HashMap::new()),
            standalone: Mutex::new(HashMap::new()),
            asset_namer: Mutex::new(None),
            minify,
            server: false,
            emitted_css: Mutex::new(Vec::new()),
            module_assets: Mutex::new(HashMap::new()),
            asset_rows: Mutex::new(Vec::new()),
            renamed: Arc::default(),
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

    /// A server bundle's own emitter, writing under `server_dir` and
    /// minifying under ssr's `cssMinify`, for an environment whose
    /// `build.emitAssets` is on.
    pub fn for_server(server_dir: &Path, base: &str, styles: Arc<Styles>) -> Emitter {
        let minify = styles.minify_ssr;
        Emitter {
            minify,
            server: true,
            ..Emitter::new(server_dir, base, styles)
        }
    }

    /// Whether a server bundle on this emitter writes its sheets.
    /// A written stylesheet a generateBundle hook edited in place: the file
    /// takes the hook's text under its name.
    pub fn overwrite_sheet(&self, file: &str, text: &str) {
        let out_dir = self.assets_dir.parent().unwrap_or(&self.assets_dir);
        let _ = std::fs::write(out_dir.join(file), text);
    }

    /// The sheets `client`'s generateBundle hooks replaced name their
    /// replacements here too (see `rename_sheet`).
    pub fn sharing_renames(mut self, client: &Emitter) -> Emitter {
        self.renamed = Arc::clone(&client.renamed);
        self
    }

    pub fn is_server(&self) -> bool {
        self.server
    }

    /// `output.assetFileNames` for the sheets written here.
    pub fn set_asset_namer(&self, namer: Option<AssetNamer>) {
        *self.asset_namer.lock().unwrap() = namer;
    }

    /// Every client stylesheet written so far, by the modules it holds: each
    /// CSS module and the file of its chunk's sheet (a plugin's standalone
    /// sheet by its module), relative to the client's output directory.
    pub fn sheet_files(&self) -> Vec<(PathBuf, String)> {
        let mut files: Vec<(PathBuf, String)> = Vec::new();
        for sheet in self.chunk_sheets.lock().unwrap().iter() {
            for module in &sheet.modules {
                files.push((module.clone(), sheet.file.clone()));
            }
        }
        for (source, url) in self.standalone.lock().unwrap().iter() {
            files.push((source.clone(), self.file_of(url)));
        }
        files.sort();
        files
    }

    /// The chunks' stylesheets for the bundle hooks and the manifest: by
    /// module, with what each chunk took over from the pure CSS chunks it
    /// imported.
    pub fn chunk_sheets(&self) -> crate::build::ChunkSheets {
        crate::build::ChunkSheets::new(self.sheet_files())
            .with_inherited(self.inherited.lock().unwrap().clone())
            .with_assets(
                self.module_assets.lock().unwrap().clone(),
                self.asset_rows.lock().unwrap().clone(),
            )
    }

    /// Every stylesheet written so far with its text (the chunks' sheets,
    /// the `?url` ones, a plugin's own), by file under the output directory,
    /// sorted: the assets the generateBundle hooks' bundle carries for them.
    pub fn sheet_assets(&self) -> Vec<(String, String)> {
        let out_dir = self.assets_dir.parent().unwrap_or(&self.assets_dir);
        let mut files = self.css_file_names();
        files.dedup();
        files
            .into_iter()
            .filter_map(|file| {
                let text = std::fs::read_to_string(out_dir.join(&file)).ok()?;
                Some((file, text))
            })
            .collect()
    }

    /// A written stylesheet a generateBundle hook replaced with an emit of
    /// its own (@stylexjs/unplugin re-emits the sheet it appended to under a
    /// new hash and drops the old one): every record of `old` names `new`,
    /// and the old file goes; the new one rolldown writes from the bundle.
    /// The hook's asset stands in the manifest (Vite lists it under
    /// `_<file>`, the source's own row gone with the asset it named).
    pub fn rename_sheet(&self, old: &str, new: &str, source: &str) {
        let out_dir = self.assets_dir.parent().unwrap_or(&self.assets_dir);
        self.renamed
            .lock()
            .unwrap()
            .insert(old.to_string(), (new.to_string(), source.to_string()));
        let old_url = format!("{}{old}", self.base);
        let new_url = format!("{}{new}", self.base);
        let rename_url = |url: &mut String| {
            if *url == old_url {
                *url = new_url.clone();
            }
        };
        let rename_file = |file: &mut String| {
            if file == old {
                *file = new.to_string();
            }
        };
        for sheet in self.chunk_sheets.lock().unwrap().iter_mut() {
            rename_file(&mut sheet.file);
            rename_url(&mut sheet.url);
        }
        for files in self.inherited.lock().unwrap().values_mut() {
            files.iter_mut().for_each(&rename_file);
        }
        for files in self.removed_pure.lock().unwrap().values_mut() {
            files.iter_mut().for_each(&rename_file);
        }
        for files in self.module_assets.lock().unwrap().values_mut() {
            files.iter_mut().for_each(&rename_file);
        }
        self.emitted_css
            .lock()
            .unwrap()
            .iter_mut()
            .for_each(&rename_url);
        self.standalone
            .lock()
            .unwrap()
            .values_mut()
            .for_each(&rename_url);
        self.css_urls
            .lock()
            .unwrap()
            .iter_mut()
            .for_each(&rename_url);
        self.asset_rows
            .lock()
            .unwrap()
            .retain(|(_, file)| file != old);
        if let Some(name) = new.rsplit('/').next() {
            self.written.lock().unwrap().insert(name.to_string());
        }
        let _ = std::fs::remove_file(out_dir.join(old));
    }

    fn file_of(&self, url: &str) -> String {
        let file = url.strip_prefix(self.base.as_str()).unwrap_or(url);
        file.trim_start_matches('/').to_string()
    }

    /// Links the sheets the page gets render-blocking, in the order Vite's
    /// css-post collects them: the written client bundle's entry chunk and
    /// what it reaches by static imports (dependencies first), each chunk's
    /// own sheet and those it took over; `every` links every chunk's sheet
    /// in that order instead (no Start manifest plugin lists a route's).
    /// A plugin's standalone sheets follow.
    pub fn link_sheets(&self, outputs: &[rolldown_common::Output], every: bool) {
        use rolldown_common::Output;
        let chunks: Vec<&rolldown_common::OutputChunk> = outputs
            .iter()
            .filter_map(|o| match o {
                Output::Chunk(c) => Some(&**c),
                Output::Asset(_) => None,
            })
            .collect();
        let imports: HashMap<String, Vec<String>> = chunks
            .iter()
            .map(|c| {
                (
                    c.filename.to_string(),
                    c.imports.iter().map(|i| i.to_string()).collect(),
                )
            })
            .collect();
        let entries: Vec<String> = chunks
            .iter()
            .filter(|c| every || c.is_entry)
            .map(|c| c.filename.to_string())
            .collect();
        let by_file: HashMap<&str, &rolldown_common::OutputChunk> =
            chunks.iter().map(|c| (c.filename.as_str(), *c)).collect();
        let sheets = self.chunk_sheets.lock().unwrap();
        let inherited = self.inherited.lock().unwrap();
        let mut urls: Vec<String> = Vec::new();
        let mut push = |url: String| {
            if !urls.contains(&url) {
                urls.push(url);
            }
        };
        for file in crate::build::ordered_sync_chunks(&entries, &imports) {
            let Some(chunk) = by_file.get(file.as_str()) else {
                continue;
            };
            let prelim = chunk.preliminary_filename.as_str();
            for file in inherited.get(prelim).into_iter().flatten() {
                push(format!("{}{file}", self.base));
            }
            if let Some(sheet) = sheets.iter().find(|s| s.chunk == prelim) {
                push(sheet.url.clone());
            }
        }
        let mut standalone: Vec<String> =
            self.standalone.lock().unwrap().values().cloned().collect();
        standalone.sort();
        for url in standalone {
            push(url);
        }
        *self.css_urls.lock().unwrap() = urls;
    }

    /// Each written chunk and the sheets it loads (its own, then those it
    /// took over), by the chunks' final file names: what a dynamic import's
    /// preload list names for the chunk.
    pub fn chunk_css(&self, outputs: &[rolldown_common::Output]) -> Vec<(String, String)> {
        let sheets = self.chunk_sheets.lock().unwrap();
        let inherited = self.inherited.lock().unwrap();
        let mut out = Vec::new();
        for o in outputs {
            let rolldown_common::Output::Chunk(chunk) = o else {
                continue;
            };
            let prelim = chunk.preliminary_filename.as_str();
            if let Some(sheet) = sheets.iter().find(|s| s.chunk == prelim) {
                out.push((chunk.filename.to_string(), sheet.file.clone()));
            }
            for file in inherited.get(prelim).into_iter().flatten() {
                out.push((chunk.filename.to_string(), file.clone()));
            }
        }
        out
    }

    /// The pattern a sheet named `name` (with its `.css`) is written under:
    /// Vite's default, the app's `assetFileNames`, or what its function
    /// answers for the asset.
    async fn asset_pattern(&self, name: &str, source: &str) -> Result<String, String> {
        let namer = match self.asset_namer.lock().unwrap().as_ref() {
            None => return Ok("assets/[name]-[hash][extname]".to_string()),
            Some(AssetNamer::Pattern(p)) => return Ok(p.clone()),
            Some(AssetNamer::Host(host, handle)) => (Arc::clone(host), *handle),
        };
        let (host, handle) = namer;
        let asset = serde_json::json!([{
            "type": "asset",
            "name": name,
            "names": [name],
            "originalFileName": null,
            "originalFileNames": [],
            "source": source,
        }]);
        let pattern = host.call_start_fn(handle, &asset).await?;
        pattern.as_str().map(str::to_string).ok_or_else(|| {
            format!("output.assetFileNames returned {pattern}, not a file name pattern")
        })
    }

    /// Vite's css-post renderChunk for `chunk`: its CSS modules' sheets as
    /// one file named after the chunk (the entry's after its base name,
    /// `index.css`; a group's `dir/name` keeps its directory), hashed by its
    /// content and written under `assetFileNames`. The sheet's file, or None
    /// for a chunk with no CSS. `js_empty`: the chunk rendered no JS, which
    /// with no exports and only plain CSS modules makes it a pure CSS chunk.
    pub async fn chunk_sheet(
        &self,
        chunk: &rolldown_common::RollupRenderedChunk,
        js_empty: bool,
    ) -> Result<Option<String>, String> {
        if let Some(file) = self.chunk_sheet_name(chunk.filename.as_str()) {
            return Ok(Some(file));
        }
        let mut css = String::new();
        let mut modules: Vec<PathBuf> = Vec::new();
        let mut pure = chunk.exports.is_empty();
        {
            let module_css = self.module_css.lock().unwrap();
            for id in &chunk.module_ids {
                let id: &str = id.as_ref();
                let path = Path::new(id.split(['?', '#']).next().unwrap_or(id));
                // vite:css-post reads a chunk's sheet off each module's own
                // id: a `?url`, `?inline` or `?raw` module of a sheet
                // carries none (the asset plugin, or an inlined string,
                // answered it), and a `?transform-only` one's compile went
                // into the `?url` asset, so it is passed over. oj keyed the
                // sheets by path, and a chunk holding `x.css?url` and
                // `x.css?transform-only` took the sheet of `x.css` twice
                // (Lovable's app-boot sheet carried Partners.css twice over).
                let query = id.split_once('?').map(|(_, q)| q).unwrap_or("");
                let has_param = |name: &str| query.split('&').any(|p| p == name);
                let transform_only = has_param("transform-only");
                let asset_module = ["url", "inline", "raw"].iter().any(|p| has_param(p));
                match module_css.get(path) {
                    Some(_) if transform_only => {}
                    Some(sheet) if !asset_module => {
                        // A CSS module's class map is JS the chunk keeps.
                        if is_css_module(&path.to_string_lossy()) {
                            pure = false;
                        }
                        // Vite minifies a chunk's sheet whole, so its modules'
                        // sheets meet with nothing between them; oj minified
                        // each on its own, with esbuild's trailing newline,
                        // and a sheet of several modules carried one newline
                        // per module (and a hash of its own for it).
                        if self.minify {
                            css.push_str(sheet.trim_end_matches('\n'));
                        } else {
                            css.push_str(sheet);
                        }
                        modules.push(path.to_path_buf());
                    }
                    _ if !(js_empty && !chunk.is_entry) => pure = false,
                    _ => {}
                }
            }
        }
        if modules.is_empty() {
            return Ok(None);
        }
        if self.minify && !css.is_empty() {
            css.push('\n');
        }
        let facade_is_css = chunk
            .facade_module_id
            .as_ref()
            .is_some_and(|f| is_style_path(f.as_str().split('?').next().unwrap_or("")));
        let stem = chunk_sheet_stem(chunk.name.as_str(), chunk.is_entry, facade_is_css);
        let pattern = self.asset_pattern(&format!("{stem}.css"), &css).await?;
        let file = crate::build::render_asset_name(
            Some(&pattern),
            &sanitize_file_name(&stem),
            &crate::build::css_asset_hash(&css),
            "css",
        );
        let out_dir = self.assets_dir.parent().unwrap_or(&self.assets_dir);
        let dest = out_dir.join(&file);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        if !dest.is_file() {
            std::fs::write(&dest, css.as_bytes())
                .map_err(|e| format!("{}: {e}", dest.display()))?;
        }
        let mut sheets = self.chunk_sheets.lock().unwrap();
        if let Some(sheet) = sheets.iter().find(|s| s.chunk == chunk.filename.as_str()) {
            return Ok(Some(sheet.file.clone()));
        }
        sheets.push(ChunkSheet {
            chunk: chunk.filename.to_string(),
            url: format!("{}{file}", self.base),
            file: file.clone(),
            modules,
            pure,
        });
        Ok(Some(file))
    }

    /// The sheet renderChunk wrote for the chunk with preliminary file name
    /// `chunk`, if any (vite:css-post's augmentChunkHash names it).
    pub fn chunk_sheet_name(&self, chunk: &str) -> Option<String> {
        self.chunk_sheets
            .lock()
            .unwrap()
            .iter()
            .find(|s| s.chunk == chunk)
            .map(|s| s.file.clone())
    }

    /// Vite's css-post generateBundle for the pure CSS chunks: each is
    /// dropped from `bundle` (its map with it), the chunks that imported it
    /// lose the import (the statement from their code, the file from their
    /// `imports`) and take over its sheet, which their preload lists and
    /// `viteMetadata.importedCss` then name.
    pub fn drop_pure_chunks(&self, bundle: &mut Vec<rolldown_common::Output>) {
        use rolldown_common::Output;
        let pure_prelims: std::collections::HashSet<String> = {
            let sheets = self.chunk_sheets.lock().unwrap();
            sheets
                .iter()
                .filter(|s| s.pure)
                .map(|s| s.chunk.clone())
                .collect()
        };
        if pure_prelims.is_empty() {
            return;
        }
        // Final file name -> preliminary, and the pure chunks' final names.
        let mut prelim_of: HashMap<String, String> = HashMap::new();
        let mut pure_files: Vec<String> = Vec::new();
        for o in bundle.iter() {
            if let Output::Chunk(c) = o {
                prelim_of.insert(c.filename.to_string(), c.preliminary_filename.clone());
                if pure_prelims.contains(&c.preliminary_filename) {
                    pure_files.push(c.filename.to_string());
                }
            }
        }
        if pure_files.is_empty() {
            return;
        }
        let basenames: Vec<String> = pure_files
            .iter()
            .map(|f| regex::escape(f.rsplit('/').next().unwrap_or(f)))
            .collect();
        let empty_import = regex::Regex::new(&format!(
            r#"\bimport\s*["'][^"']*(?:{})["'];"#,
            basenames.join("|")
        ))
        .expect("a file-name alternation is a valid pattern");
        let mut inherited = self.inherited.lock().unwrap();
        let sheets = self.chunk_sheets.lock().unwrap();
        {
            // What each dropped chunk brought: a chunk importing it with
            // `import()` keeps the statement, and the preload helper reads
            // its sheets off this record (Vite's removedPureCssFilesCache).
            let mut removed = self.removed_pure.lock().unwrap();
            for file in &pure_files {
                let Some(prelim) = prelim_of.get(file) else {
                    continue;
                };
                let mut css: Vec<String> = Vec::new();
                if let Some(sheet) = sheets.iter().find(|s| &s.chunk == prelim) {
                    css.push(sheet.file.clone());
                }
                for inherited_file in inherited.get(prelim).cloned().unwrap_or_default() {
                    if !css.contains(&inherited_file) {
                        css.push(inherited_file);
                    }
                }
                removed.insert(file.clone(), css);
            }
        }
        for o in bundle.iter_mut() {
            let Output::Chunk(chunk) = o else { continue };
            let names_one = |files: &[arcstr::ArcStr]| {
                files.iter().any(|i| pure_files.contains(&i.to_string()))
            };
            if !names_one(&chunk.imports) && !names_one(&chunk.dynamic_imports) {
                continue;
            }
            let chunk = Arc::make_mut(chunk);
            // The chunk's own `import()` of a dropped chunk stays in its
            // code for the preload helper to rewrite; the file leaves the
            // chunk's list, as the manifest's dynamicImports never names a
            // chunk the bundle lost.
            chunk
                .dynamic_imports
                .retain(|import| !pure_files.contains(&import.to_string()));
            let mut took: Vec<String> = Vec::new();
            chunk.imports.retain(|import| {
                let import = import.to_string();
                if !pure_files.contains(&import) {
                    return true;
                }
                if let Some(prelim) = prelim_of.get(&import) {
                    if let Some(sheet) = sheets.iter().find(|s| &s.chunk == prelim) {
                        took.push(sheet.file.clone());
                    }
                    for file in inherited.get(prelim).cloned().unwrap_or_default() {
                        took.push(file);
                    }
                }
                false
            });
            let mine = inherited
                .entry(chunk.preliminary_filename.clone())
                .or_default();
            for file in took {
                if !mine.contains(&file) {
                    mine.push(file);
                }
            }
            // Vite's css-post (getEmptyChunkReplacer) leaves a comment the
            // length of the statement in its place, so nothing after it
            // moves under the chunk's sourcemap.
            chunk.code = empty_import
                .replace_all(&chunk.code, |m: &regex::Captures| {
                    format!(
                        "/* empty css {}*/",
                        " ".repeat(m[0].len().saturating_sub(15))
                    )
                })
                .into_owned();
        }
        let pure_maps: Vec<String> = pure_files.iter().map(|f| format!("{f}.map")).collect();
        bundle.retain(|o| match o {
            Output::Chunk(c) => !pure_files.contains(&c.filename.to_string()),
            // A dropped chunk's sourcemap goes with it.
            Output::Asset(a) => !pure_maps.contains(&a.filename.to_string()),
        });
    }

    /// The pure CSS chunks [`Self::drop_pure_chunks`] dropped, by final file
    /// name, with their sheets.
    pub fn removed_pure_css(&self) -> HashMap<String, Vec<String>> {
        self.removed_pure.lock().unwrap().clone()
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
        // A sheet a generateBundle hook replaced (`rename_sheet`), asked for
        // again by the same text (the server bundle's `?url` import of it):
        // the replacement, written here too when this emitter has its own
        // output (a server emitting its assets), as Vite's server bundle
        // ends with the client's name for it.
        let renamed = self
            .renamed
            .lock()
            .unwrap()
            .get(&format!("assets/{name}"))
            .cloned();
        let (name, bytes) = match &renamed {
            Some((file, source)) => (
                file.strip_prefix("assets/").unwrap_or(file).to_string(),
                source.as_bytes(),
            ),
            None => (name, bytes),
        };
        self.asset_rows
            .lock()
            .unwrap()
            .push((path.to_path_buf(), format!("assets/{name}")));
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
        let css = if self.minify {
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
                return self.css_asset(path, css, None).await;
            }
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            self.write(path, &bytes)
        })
    }

    /// An asset imported by `id` (its query included) or named by a CSS
    /// `url()`: its data URL when Vite's build would inline it
    /// (`fileToBuiltUrl` → `shouldInline`: never with `?no-inline`, always
    /// with `?inline`, never an `.html` or an svg with a fragment, else under
    /// the client's `build.assetsInlineLimit` unless a Git LFS pointer), else
    /// emitted as [`Emitter::emit`] emits it. An svg inlines as
    /// `data:image/svg+xml,…` (svgToDataURL), anything else base64.
    pub async fn asset_url(&self, path: &Path, id: &str) -> Result<String, String> {
        let is_svg = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("svg"));
        let inline = if oj_server::has_word_param(id, "no-inline") {
            false
        } else if oj_server::has_word_param(id, "inline") {
            true
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("html"))
            || (is_svg && id.contains('#'))
        {
            false
        } else {
            let limit = self.styles.inline_limit();
            std::fs::metadata(path).is_ok_and(|m| m.len() < limit)
        };
        if inline {
            let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
            if !bytes.starts_with(b"version https://git-lfs.github.com") {
                return Ok(if is_svg {
                    oj_server::svg_to_data_url(&bytes)
                } else {
                    oj_server::asset_data_url(path, &bytes)
                });
            }
        }
        let url = self.emit(path).await?;
        self.note_module_asset(id, &url);
        Ok(url)
    }

    /// Records `url` (a written file's, not a data URL) as an asset of the
    /// module `id`.
    pub fn note_module_asset(&self, id: &str, url: &str) {
        if url.starts_with("data:") {
            return;
        }
        let file = self.file_of(url);
        let mut assets = self.module_assets.lock().unwrap();
        let files = assets.entry(id.to_string()).or_default();
        if !files.contains(&file) {
            files.push(file);
        }
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
            // Plain `@import`s are inlined, as Vite's postcss-import step
            // inlines them before anything else runs on a sheet, so the
            // emitted sheet carries no import that 404s from `assets/` (a
            // `?url` sheet of fonts importing a package's `@font-face`
            // sheet shipped the bare `@import` itself). A Tailwind sheet
            // resolves its own imports in the tailwind engine.
            let css = if needs_css_compile(&css) {
                css
            } else {
                oj_css::inline_imports_with(&css, path, &self.styles.inputs().resolve)?
            };
            let css = self.styles.tailwind(path, css).await?;
            let css = oj_server::style_slot(slots, css, "normal").await?;
            let css = self
                .rewrite_urls(&css, path.parent().unwrap_or(path))
                .await?;
            let css = if self.minify {
                format!("{}\n", self.styles.minify(path, &css)?)
            } else {
                css
            };
            if !link {
                // A compiled .scss/.less sheet ships as .css.
                let url = self.write(&path.with_extension("css"), css.as_bytes())?;
                let mut emitted = self.emitted_css.lock().unwrap();
                if !emitted.contains(&url) {
                    emitted.push(url.clone());
                }
                return Ok(url);
            }
            // A linked sheet waits for its chunk: renderChunk writes the
            // chunk's one sheet (`chunk_sheet`), named after the chunk.
            self.module_css
                .lock()
                .unwrap()
                .insert(path.to_path_buf(), css);
            Ok(String::new())
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
        let css = if self.minify {
            format!("{}\n", self.styles.minify(&path, css)?)
        } else {
            css.to_string()
        };
        // The module's chunk sheet, rewritten with the plugin's CSS in the
        // module's place; the file keeps the name it was hashed and linked
        // under, as Vite's asset keeps its name when a plugin's generateBundle
        // rewrites its source.
        let holder = {
            let sheets = self.chunk_sheets.lock().unwrap();
            sheets
                .iter()
                .find(|s| s.modules.contains(&path))
                .map(|s| (s.file.clone(), s.modules.clone()))
        };
        if let Some((file, modules)) = holder {
            let mut module_css = self.module_css.lock().unwrap();
            module_css.insert(path, css);
            let joined: String = modules
                .iter()
                .filter_map(|m| module_css.get(m))
                .cloned()
                .collect();
            let dest = self
                .assets_dir
                .parent()
                .unwrap_or(&self.assets_dir)
                .join(&file);
            return std::fs::write(&dest, joined).map_err(|e| format!("{}: {e}", dest.display()));
        }
        let url = self.write(&path.with_extension("css"), css.as_bytes())?;
        let old = self.standalone.lock().unwrap().insert(path, url.clone());
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
    #[cfg(test)]
    pub fn append_generated_css(&self, name: &str, css: &str) -> Result<(), String> {
        let names = self.css_file_names();
        let picked = oj_stylex::plugin::pick_css_asset(&names).map(|at| names[at].clone());
        self.append_generated_css_to(picked, name, css)
    }

    /// Every stylesheet the client emitted, by file name under the out dir
    /// (the chunks' sheets, the `?url` ones, a plugin's own), sorted: what
    /// the StyleX plugin's generateBundle picks its target from.
    pub fn css_file_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .chunk_sheets
            .lock()
            .unwrap()
            .iter()
            .map(|s| s.file.clone())
            .collect();
        for url in self.emitted_css.lock().unwrap().iter() {
            names.push(self.file_of(url));
        }
        for url in self.standalone.lock().unwrap().values() {
            names.push(self.file_of(url));
        }
        names.sort();
        names
    }

    /// [`Emitter::append_generated_css`] onto `picked` (a name from
    /// [`Emitter::css_file_names`]); the plugin's default pick is the first
    /// `index-<hash>.css` (the entry's), else the first `style-<hash>.css`,
    /// else the first sheet, and its `cssInjectionTarget` chooses before it.
    pub fn append_generated_css_to(
        &self,
        picked: Option<String>,
        name: &str,
        css: &str,
    ) -> Result<(), String> {
        let out_dir = self.assets_dir.parent().unwrap_or(&self.assets_dir);
        let file = match picked {
            Some(file) => out_dir.join(file),
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
            let url = self.asset_url(&normalize(&abs), target).await?;
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

/// A file as a `data:` URI (`?inline` of a non-stylesheet), Vite's
/// `assetToDataURL`: an SVG as `svgToDataURL` writes it, any other file
/// base64.
fn data_uri(path: &Path) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(oj_server::asset_data_url(path, &bytes))
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

/// The client's numeric `build.assetsInlineLimit` (its environment's own
/// first; Vite's default 4096), the limit Vite's `shouldInline` reads.
pub(crate) fn client_inline_limit(config: &oj_config::OjConfig) -> u64 {
    config
        .build
        .as_ref()
        .and_then(|b| {
            b.assets_inline_limit_envs
                .as_ref()
                .and_then(|envs| envs.get("client")?.as_u64())
                .or(b.assets_inline_limit)
        })
        .unwrap_or(4096)
}

/// Vite's `fileToDevUrl` for an `.svg`, the one type dev inlines on its own:
/// "if is svg and it's inlined in build, also inline it in dev to match the
/// behaviour in build due to quote handling differences". The data URL when
/// the build's `shouldInline` would say so (no `no-inline` on the id, no
/// `#` fragment, the file under `assetsInlineLimit`, not a Git LFS
/// placeholder); `None` for anything else, which stays the served url. `id`
/// is the import as written (with its query), `path` the file it names.
pub(crate) fn dev_svg_inline(path: &Path, id: &str, inline_limit: u64) -> Option<String> {
    if !ext_of(&path.to_string_lossy()).eq_ignore_ascii_case("svg") {
        return None;
    }
    let query = id.split_once('?').map_or("", |(_, q)| q);
    if has_no_inline(query) || id.contains('#') {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    const GIT_LFS_PREFIX: &[u8] = b"version https://git-lfs.github.com";
    if (bytes.len() as u64) >= inline_limit || bytes.starts_with(GIT_LFS_PREFIX) {
        return None;
    }
    Some(oj_server::svg_to_data_url(&bytes))
}

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

    /// An asset import's value: in dev, a small `.svg` is its data URL as
    /// Vite's `fileToDevUrl` inlines it ([`dev_svg_inline`]), anything else
    /// its served url; the build's emitter inlines or emits by the same rule.
    async fn asset_url_for(&self, path: &Path, id: &str) -> Result<String, String> {
        match &self.mode {
            AssetMode::Dev { .. } => {
                if let Some(data) = dev_svg_inline(path, id, self.styles.inline_limit()) {
                    return Ok(data);
                }
                self.url_for(path).await
            }
            AssetMode::Build { emitter, .. } => emitter.asset_url(path, id).await,
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
        let Some(tag) = asset_tag(&self.root, id.trim_start_matches('\0'), dev) else {
            // A bare `.svg` (or `?react` one) no plugin componentized: its
            // URL, or its data URL when small (Vite's dev inlines an svg the
            // build would).
            if path.extension().is_some_and(|e| e == "svg") {
                return Ok(Some(format!(
                    "export default {};",
                    json(&self.asset_url_for(path, id).await?)
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
            AssetTag::Url => format!(
                "export default {};",
                json(&self.asset_url_for(path, id).await?)
            ),
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
                            let worker_url = self.worker_url_for(path, worker_type).await?;
                            if let AssetMode::Build { emitter, .. } = &self.mode {
                                emitter.note_module_asset(id, &worker_url);
                            }
                            json(&worker_url)
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
                        worker_chain,
                    } => {
                        if worker_chain.is_empty() {
                            emitter.css(path, css(), slots).await?;
                        } else {
                            // Vite's worker build emits the worker's sheet as
                            // an asset the page does not link.
                            emitter.css_asset(path, css(), slots).await?;
                        }
                    }
                    AssetMode::Build {
                        emitter,
                        server: true,
                        ..
                    } => {
                        if emitter.is_server() {
                            // `build.emitAssets`: the server bundle writes
                            // its chunks' sheets too, as a Worker's does.
                            emitter.css(path, css(), slots).await?;
                        } else if slots.is_some() {
                            let css = self.styles.tailwind(path, css()).await?;
                            oj_server::style_slot(slots, css, "normal").await?;
                        }
                    }
                }
                // A plain sheet's module is empty in a build, as vite:css-post
                // leaves it (`code = ""`): a chunk of sheets alone is then a
                // pure CSS chunk, dropped with its sheet handed to the chunks
                // importing it, with `import()` as with `import`. Dev keeps
                // an object for the module's default.
                compiled.map_or_else(
                    || match &self.mode {
                        AssetMode::Build { .. } => String::new(),
                        AssetMode::Dev { .. } => "export default {};".to_string(),
                    },
                    |c| c.code,
                )
            }
        }))
    }
}

impl StartAssetsPlugin {
    /// The emitter whose bundle's chunks get stylesheets: the client's, or a
    /// server's that emits its assets (a worker's sheets are assets, as
    /// Vite's worker build emits them).
    fn client_emitter(&self) -> Option<&Arc<Emitter>> {
        match &self.mode {
            AssetMode::Build {
                emitter,
                server: false,
                worker_chain,
            } if worker_chain.is_empty() => Some(emitter),
            AssetMode::Build {
                emitter,
                server: true,
                ..
            } if emitter.is_server() => Some(emitter),
            _ => None,
        }
    }
}

impl Plugin for StartAssetsPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:start-assets")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::Load
            | rolldown_plugin::HookUsage::RenderChunk
            | rolldown_plugin::HookUsage::AugmentChunkHash
            | rolldown_plugin::HookUsage::GenerateBundle
    }

    /// vite:css-post's renderChunk for a client chunk: its stylesheet,
    /// named after it, written now so the bundle hooks see it.
    async fn render_chunk(
        &self,
        _ctx: &rolldown_plugin::PluginContext,
        args: &rolldown_plugin::HookRenderChunkArgs<'_>,
    ) -> rolldown_plugin::HookRenderChunkReturn {
        if let Some(emitter) = self.client_emitter() {
            emitter
                .chunk_sheet(&args.chunk, args.code.is_empty())
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        Ok(None)
    }

    /// vite:css-post's augmentChunkHash: a chunk's hash covers its sheet's
    /// name, so the chunk is renamed when only its CSS changes.
    async fn augment_chunk_hash(
        &self,
        _ctx: &rolldown_plugin::PluginContext,
        chunk: Arc<rolldown_common::RollupRenderedChunk>,
    ) -> rolldown_plugin::HookAugmentChunkHashReturn {
        Ok(self
            .client_emitter()
            .and_then(|emitter| emitter.chunk_sheet_name(chunk.filename.as_str())))
    }

    /// vite:css-post's generateBundle: the pure CSS chunks go, their
    /// importers take their sheets. After the plugins' own generateBundle
    /// (the host's), which see the chunks as Vite's user plugins do.
    async fn generate_bundle(
        &self,
        _ctx: &rolldown_plugin::PluginContext,
        args: &mut rolldown_plugin::HookGenerateBundleArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        if let Some(emitter) = self.client_emitter() {
            emitter.drop_pure_chunks(args.bundle);
        }
        Ok(())
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
            // An asset-like module, as Vite's plugins load one: its map is
            // empty, so it names no source in its chunk's sourcemap.
            map: crate::build::vite_asset_map(),
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
            // Vite's KNOWN_ASSET_TYPES beyond images: media, fonts, documents.
            ("/a/x.mp3", false, Some(Url)),
            ("/a/x.wav", false, Some(Url)),
            ("/a/x.ogg", false, Some(Url)),
            ("/a/x.MOV", false, Some(Url)),
            ("/a/x.eot", false, Some(Url)),
            ("/a/x.pdf", false, Some(Url)),
            ("/a/x.webmanifest", false, Some(Url)),
            ("/a/x.txt", false, Some(Url)),
            ("/a/x.wasm", false, Some(Url)),
            ("/a/x.txt?raw", false, Some(Raw)),
            ("/a/x.svg", false, None),
            ("/a/x.svg?react", false, None),
            ("/a/x.mdx", false, None),
            ("/a/x.css", false, Some(Css)),
            ("/a/x.module.scss", false, Some(Css)),
            ("/a/x.ts?raw", false, Some(Raw)),
            // `?raw` wins over the extension, as Vite's rawRE check comes
            // first: an SVG's text, not its URL or component.
            ("/a/x.svg?raw", false, Some(Raw)),
            ("/a/x.md?raw", true, Some(Raw)),
            ("/a/x.svg?inline", false, Some(Inline)),
            ("/a/x.css?url", false, Some(Url)),
            ("/a/x.css?inline", false, Some(Inline)),
            ("/a/x.png?url&no-inline", false, Some(Url)),
            ("/a/x.png?no-inline&inline", false, Some(Url)),
            // noInlineRE is word-bounded, as inlineRE is.
            ("/a/x.png?url&no-inline=1", false, Some(Url)),
            ("/a/x.png?no-inline-x&inline", false, Some(Url)),
            ("/a/x.svg?inline=1", false, Some(Inline)),
            ("/a/x.png?url&inline", false, Some(Inline)),
            ("/a/x.ts?rawx", false, None),
            ("/a/x.ts?raw=1", false, None),
            ("\0/a/x.png", false, None),
        ];
        for (id, dev, want) in cases {
            assert_eq!(asset_tag(Path::new("/"), id, *dev), *want, "{id}");
        }
        assert_eq!(
            asset_tag(Path::new("/"), "/w.ts?worker&inline", false),
            Some(Worker {
                shared: false,
                form: WorkerForm::Inline
            })
        );
        assert_eq!(
            asset_tag(Path::new("/"), "/w.ts?worker&inline", true),
            Some(Worker {
                shared: false,
                form: WorkerForm::Module
            }),
            "unbundled dev ignores &inline"
        );
        assert_eq!(
            asset_tag(Path::new("/"), "/w.ts?sharedworker&url", true),
            Some(Worker {
                shared: true,
                form: WorkerForm::Url
            })
        );
        assert_eq!(
            asset_tag(Path::new("/"), "/w.ts?url&worker", false),
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
    /// Vite's `fileToDevUrl` inlines an `.svg` in dev when the build's
    /// `shouldInline` would: under `build.assetsInlineLimit`, no `no-inline`
    /// on the id, no `#` fragment, not a Git LFS placeholder; the form is
    /// `svgToDataURL`'s. Anything else keeps its url, and only an svg is
    /// inlined on its own.
    #[test]
    fn dev_inlines_a_small_svg_as_vites_file_to_dev_url_does() {
        let dir = std::env::temp_dir().join(format!("oj-dev-svg-inline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let small = dir.join("dot.svg");
        std::fs::write(
            &small,
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><circle r=\"1\"/></svg>\n",
        )
        .unwrap();
        let big = dir.join("big.svg");
        std::fs::write(&big, format!("<svg>{}</svg>", "<g/>".repeat(2000))).unwrap();
        let lfs = dir.join("lfs.svg");
        std::fs::write(
            &lfs,
            "version https://git-lfs.github.com/spec/v1\noid sha256:0\n",
        )
        .unwrap();
        let png = dir.join("dot.png");
        std::fs::write(&png, b"png").unwrap();
        let id = |p: &std::path::Path, q: &str| format!("{}{q}", p.display());

        assert_eq!(
            dev_svg_inline(&small, &id(&small, ""), 4096).as_deref(),
            Some("data:image/svg+xml,%3csvg%20xmlns='http://www.w3.org/2000/svg'%3e%3ccircle%20r='1'/%3e%3c/svg%3e"),
            "a small svg, bare"
        );
        assert!(
            dev_svg_inline(&small, &id(&small, "?url"), 4096).is_some(),
            "`?url` too"
        );
        assert!(
            dev_svg_inline(&small, &id(&small, "?url&no-inline"), 4096).is_none(),
            "`no-inline` keeps the url"
        );
        assert!(
            dev_svg_inline(&small, &id(&small, "?url&no-inline=1"), 4096).is_none(),
            "`no-inline=1` too (Vite's noInlineRE is word-bounded)"
        );
        assert!(
            dev_svg_inline(&small, &id(&small, "#top"), 4096).is_none(),
            "a fragment keeps the url"
        );
        assert!(
            dev_svg_inline(&big, &id(&big, ""), 4096).is_none(),
            "over the limit"
        );
        assert!(
            dev_svg_inline(&big, &id(&big, ""), 1 << 20).is_some(),
            "under a raised limit"
        );
        let size = std::fs::metadata(&small).unwrap().len();
        assert!(
            dev_svg_inline(&small, &id(&small, ""), size).is_none(),
            "the limit is exclusive"
        );
        assert!(
            dev_svg_inline(&lfs, &id(&lfs, ""), 4096).is_none(),
            "a Git LFS placeholder"
        );
        assert!(
            dev_svg_inline(&png, &id(&png, ""), 4096).is_none(),
            "only an svg"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_inline_limit_is_the_clients_then_the_builds_then_vites_default() {
        let with = |build: Option<oj_config::BuildConfig>| oj_config::OjConfig {
            build,
            ..Default::default()
        };
        assert_eq!(client_inline_limit(&with(None)), 4096);
        assert_eq!(
            client_inline_limit(&with(Some(oj_config::BuildConfig {
                assets_inline_limit: Some(1000),
                ..Default::default()
            }))),
            1000
        );
        assert_eq!(
            client_inline_limit(&with(Some(oj_config::BuildConfig {
                assets_inline_limit: Some(1000),
                assets_inline_limit_envs: Some(serde_json::json!({ "client": 10, "ssr": 2000 })),
                ..Default::default()
            }))),
            10
        );
    }
}
