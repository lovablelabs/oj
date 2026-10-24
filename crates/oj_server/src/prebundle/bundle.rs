// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite's prepareRolldownOptimizerRun and rolldownDepPlugin: one rolldown
//! build with an ESM entry per dep plus shared chunks. One difference from
//! Vite: oj serves the bundle verbatim (Vite runs importAnalysis over it), so
//! whatever stays external is written as the URL oj serves it at instead of
//! a path or a bare name.

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rolldown::{BundlerBuilder, InputItem, OutputFormat};
use rolldown_common::{ImportKind, ModuleType, Output, ResolvedExternal, ResolvedId};
use rolldown_plugin::__inner::{Pluginable, SharedPluginable};
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookTransformArgs, HookTransformOutput, HookTransformReturn, HookUsage,
    Plugin, PluginContext, PluginContextResolveOptions, SharedLoadPluginContext,
    SharedTransformPluginContext,
};

use super::scan::is_external_type;
use super::{
    clean_url, in_node_modules, is_bare, is_js_type, module_list_contains, npm_package_name,
};
use super::{normalize, Plan, Prebuilt, Prebundle};

const BROWSER_EXTERNAL: &str = "\0oj-dep:browser-external:";
const OPTIONAL_PEER: &str = "\0oj-dep:optional-peer:";
const CJS_EXTERNAL_FACADE: &str = "\0oj-dep:cjs-external:";
const CONVERT_EXTERNAL: &str = "\0oj-dep:external-conversion:";
/// The specifier a conversion facade imports its external through.
const CONVERTED_PREFIX: &str = "oj-dep-external:";

fn json(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

fn external_url(url: String) -> HookResolveIdReturn {
    Ok(Some(HookResolveIdOutput {
        id: url.into(),
        external: Some(ResolvedExternal::Absolute),
        ..Default::default()
    }))
}

fn virtual_id(id: String) -> HookResolveIdReturn {
    Ok(Some(HookResolveIdOutput::from_id(id)))
}

fn resolve_options(kind: ImportKind) -> Option<PluginContextResolveOptions> {
    Some(PluginContextResolveOptions {
        import_kind: kind,
        skip_self: true,
        ..Default::default()
    })
}

/// The pieces of a module's static shape the interop decision reads.
struct Shape {
    has_module_syntax: bool,
    exports: Vec<String>,
}

/// `file` parsed as its extension says; `None` when it does not parse.
fn shape_of(file: &Path) -> Option<Shape> {
    let source = std::fs::read_to_string(file).ok()?;
    let allocator = oxc_allocator::Allocator::default();
    let source_type = oxc_span::SourceType::from_path(file).unwrap_or_default();
    let parsed = oxc_parser::Parser::new(&allocator, &source, source_type).parse();
    if parsed.fatal_error {
        return None;
    }
    let record = &parsed.module_record;
    let exports = record
        .local_export_entries
        .iter()
        .chain(record.indirect_export_entries.iter())
        .filter_map(|e| match &e.export_name {
            oxc_syntax::module_record::ExportExportName::Name(n) => Some(n.name.to_string()),
            oxc_syntax::module_record::ExportExportName::Default(_) => Some("default".to_string()),
            _ => None,
        })
        .collect();
    Some(Shape {
        has_module_syntax: record.has_module_syntax,
        exports,
    })
}

/// An entry with no ESM syntax: the bundle's own verdict for one
/// ([`needs_interop`]) before its output exists.
pub(super) fn is_common_js(file: &Path) -> bool {
    shape_of(file).is_some_and(|s| !s.has_module_syntax)
}

fn single_default(exports: &[String]) -> bool {
    exports.len() == 1 && exports[0] == "default"
}

/// Vite's needsInterop (optimizer/index.ts) minus the forced list: the entry
/// has no ESM syntax (CJS, UMD), or the bundle collapsed an ESM entry into a
/// lone default export (a peer require()d it).
fn needs_interop(facade: Option<&Path>, generated: &[String]) -> bool {
    match facade.filter(|f| f.is_absolute()).and_then(shape_of) {
        None => generated.is_empty() || single_default(generated),
        Some(shape) if !shape.has_module_syntax => true,
        Some(shape) => single_default(generated) && !single_default(&shape.exports),
    }
}

/// Vite's extractExportsData fallback: a dep entry that only parses as JSX
/// (JSX shipped in a `.js` file) switches the whole bundle to `.js: jsx`.
fn needs_jsx_loader(entries: &BTreeMap<String, PathBuf>) -> bool {
    let fails = |source: &str, jsx: bool| {
        let allocator = oxc_allocator::Allocator::default();
        let source_type = oxc_span::SourceType::mjs().with_jsx(jsx);
        let parsed = oxc_parser::Parser::new(&allocator, source, source_type).parse();
        parsed.fatal_error || parsed.diagnostics.has_errors()
    };
    entries.values().any(|file| {
        file.extension().is_some_and(|e| e == "js")
            && std::fs::read_to_string(file)
                .is_ok_and(|source| fails(&source, false) && !fails(&source, true))
    })
}

/// Vite's optionalPeerDepId: a bare import a dep's package.json lists as an
/// optional peer, and which is not installed, becomes a module that throws
/// when evaluated instead of failing the whole pre-bundle.
fn optional_peer_of(id: &str, importer: &str) -> Option<String> {
    let pkg_name = npm_package_name(id)?;
    if !in_node_modules(importer) {
        return None;
    }
    for dir in Path::new(importer).ancestors().skip(1) {
        let Ok(text) = std::fs::read_to_string(dir.join("package.json")) else {
            continue;
        };
        let meta: serde_json::Value = serde_json::from_str(&text).ok()?;
        // A nameless package.json (a `type` marker) is not the package.
        let Some(name) = meta.get("name").and_then(|n| n.as_str()) else {
            continue;
        };
        let optional = meta
            .pointer(&format!(
                "/peerDependenciesMeta/{}/optional",
                pkg_name.replace('~', "~0").replace('/', "~1")
            ))
            .and_then(|o| o.as_bool())
            .unwrap_or(false);
        return optional.then(|| format!("{pkg_name}:{name}"));
    }
    None
}

/// The non-JS file a resolution landed on, as Vite's resolveResult treats it:
/// an asset external, or (for `require()`) a conversion facade.
fn asset_or_module(pre: &Prebundle, r: ResolvedId, kind: ImportKind) -> HookResolveIdReturn {
    if !matches!(r.external, ResolvedExternal::Bool(false)) || !is_external_type(&r.id) {
        return Ok(Some(HookResolveIdOutput::from_resolved_id(r)));
    }
    let url = pre.url_of(Path::new(r.id.as_str()));
    if matches!(kind, ImportKind::Require) {
        return virtual_id(format!("{CONVERT_EXTERNAL}{url}"));
    }
    external_url(url)
}

/// Vite's non-JS externals: a style, asset or wasm import inside a dep stays
/// out of the bundle, pointed at the URL oj serves the file at.
#[derive(Debug)]
struct DepAssetsPlugin(Arc<Prebundle>);

impl Plugin for DepAssetsPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:dep-pre-bundle-assets")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        let id = args.specifier;
        if !is_external_type(id) {
            return Ok(None);
        }
        if let Some(url) = id.strip_prefix(CONVERTED_PREFIX) {
            return external_url(url.to_string());
        }
        let Some(importer) = args.importer else {
            return Ok(None);
        };
        let resolved = ctx
            .resolve(id, Some(importer), resolve_options(args.kind))
            .await?;
        let mut file = None;
        if let Ok(r) = resolved {
            if matches!(r.external, ResolvedExternal::Bool(false)) {
                if is_js_type(&r.id) {
                    return Ok(Some(HookResolveIdOutput::from_resolved_id(r)));
                }
                file = Some(PathBuf::from(r.id.as_str()));
            }
        }
        if file.is_none() && id.starts_with('.') {
            file = Path::new(importer).parent().map(|d| normalize(&d.join(id)));
        }
        let url = file.map_or_else(|| id.to_string(), |f| self.0.url_of(&f));
        // require() of an external stays a runtime require in rolldown's
        // output; Vite converts it to an import through a facade module.
        if matches!(args.kind, ImportKind::Require) {
            return virtual_id(format!("{CONVERT_EXTERNAL}{url}"));
        }
        external_url(url)
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        let Some(url) = args.id.strip_prefix(CONVERT_EXTERNAL) else {
            return Ok(None);
        };
        let spec = json(&format!("{CONVERTED_PREFIX}{url}"));
        let path = clean_url(url);
        let is_style = [
            ".css", ".scss", ".sass", ".less", ".styl", ".stylus", ".pcss", ".postcss",
        ]
        .iter()
        .any(|e| path.ends_with(e));
        let code = if is_style && !url.contains(".module.") {
            format!("import {spec};")
        } else {
            format!("export {{ default }} from {spec};\nexport * from {spec};")
        };
        Ok(Some(HookLoadOutput {
            code: code.into(),
            module_type: Some(ModuleType::Js),
            ..Default::default()
        }))
    }
}

/// Vite's bare-import handling for a dep bundle: excluded deps and node
/// builtins stay out, `resolve.dedupe` pins the root copy, a missing optional
/// peer throws only when used, and a dep's `new URL(.., import.meta.url)`
/// points at the URL oj serves the file at.
#[derive(Debug)]
struct DepBundlePlugin(Arc<Prebundle>);

impl Plugin for DepBundlePlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:dep-pre-bundle")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load | HookUsage::Transform
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        let pre = &self.0;
        let id = args.specifier;
        if !is_bare(id) {
            return Ok(None);
        }
        if let Some(url) = id.strip_prefix(CONVERTED_PREFIX) {
            return external_url(url.to_string());
        }
        let Some(importer) = args.importer else {
            return Ok(None);
        };
        if module_list_contains(&pre.exclude, id) {
            let url = match ctx
                .resolve(id, Some(importer), resolve_options(args.kind))
                .await?
            {
                Ok(r) if matches!(r.external, ResolvedExternal::Bool(false)) => {
                    pre.url_of(Path::new(r.id.as_str()))
                }
                _ => id.to_string(),
            };
            // An ESM bundle has no require(): Vite's cjs-external plugin
            // turns a require() of an excluded dep into an import.
            if matches!(args.kind, ImportKind::Require) {
                return virtual_id(format!("{CJS_EXTERNAL_FACADE}{url}"));
            }
            return external_url(url);
        }
        if let Some(aliased) = pre.alias.apply(id) {
            if let Ok(r) = ctx
                .resolve(&aliased, Some(importer), resolve_options(args.kind))
                .await?
            {
                return asset_or_module(pre, r, args.kind);
            }
        }
        // resolve.dedupe: from the project root wherever the importer sits
        // (Vite resolve.ts: dedupe -> basedir = root).
        let importer_dir = Path::new(importer).parent();
        let from_root = npm_package_name(id).is_some_and(|p| pre.dedupe.contains(p))
            && importer_dir.is_some_and(|d| d != pre.root && d != pre.real_root);
        let root_importer = pre.root.join("package.json").to_string_lossy().into_owned();
        let from = if from_root {
            root_importer.as_str()
        } else {
            importer
        };
        if let Ok(r) = ctx
            .resolve(id, Some(from), resolve_options(args.kind))
            .await?
        {
            return asset_or_module(pre, r, args.kind);
        }
        if crate::module_pipeline::is_node_builtin(id) {
            return virtual_id(format!("{BROWSER_EXTERNAL}{id}"));
        }
        if let Some(peer) = optional_peer_of(id, importer) {
            return virtual_id(format!("{OPTIONAL_PEER}{peer}"));
        }
        Ok(None)
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        let id = args.id;
        let code = if let Some(url) = id.strip_prefix(CJS_EXTERNAL_FACADE) {
            let spec = json(&format!("{CONVERTED_PREFIX}{url}"));
            format!("import * as m from {spec};\nmodule.exports = {{ ...m }};\n")
        } else if let Some(name) = id.strip_prefix(BROWSER_EXTERNAL) {
            let warning = json(&format!(
                "Module \"{name}\" has been externalized for browser compatibility. Cannot access \"{name}."
            ));
            format!(
                "module.exports = Object.create(new Proxy({{}}, {{\n  get(_, key) {{\n    if (key !== \"__esModule\" && key !== \"__proto__\" && key !== \"constructor\" && key !== \"splice\") {{\n      console.warn({warning} + String(key) + '\" in client code. See https://vite.dev/guide/troubleshooting.html#module-externalized-for-browser-compatibility for more details.');\n    }}\n  }}\n}}));\n"
            )
        } else if let Some(peer) = id.strip_prefix(OPTIONAL_PEER) {
            let (peer_dep, parent_dep) = peer.split_once(':').unwrap_or((peer, ""));
            let message = json(&format!(
                "Could not resolve \"{peer_dep}\" imported by \"{parent_dep}\". Is it installed?"
            ));
            format!("module.exports = {{}};throw new Error({message});")
        } else {
            return Ok(None);
        };
        Ok(Some(HookLoadOutput {
            code: code.into(),
            module_type: Some(ModuleType::Js),
            ..Default::default()
        }))
    }

    /// A dep's `new URL("./x.wasm", import.meta.url)` is relative to the
    /// dep's own file; the bundle lives elsewhere, so the URL becomes the
    /// one oj serves that file at (Vite rewrites it relative to its deps
    /// dir). Matched on literal-stripped code, as Vite matches on
    /// strip-literal output; `/* @vite-ignore */` skips a site.
    async fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> HookTransformReturn {
        static URL_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(
                r#"\bnew\s+URL\s*\(\s*('[^']+'|"[^"]+"|`[^`]+`)\s*,\s*import\.meta\.url\s*(?:,\s*)?\)"#,
            )
            .unwrap()
        });
        static IGNORE: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"/\*\s*@vite-ignore\s*\*/").unwrap());
        let code = args.code.as_str();
        if !code.contains("import.meta.url") {
            return Ok(None);
        }
        let dir = Path::new(clean_url(args.id))
            .parent()
            .unwrap_or(Path::new("/"));
        let masked = oj_compiler::meta_glob::strip_literals(code);
        let mut out = String::with_capacity(code.len());
        let mut last = 0;
        for caps in URL_RE.captures_iter(&masked) {
            let whole = caps.get(0).expect("match");
            let literal = caps.get(1).expect("url group");
            let raw = &code[literal.start()..literal.end()];
            if IGNORE.is_match(&code[whole.start()..literal.start()])
                || (raw.starts_with('`') && raw.contains("${"))
            {
                continue;
            }
            let url = &raw[1..raw.len() - 1];
            if url.starts_with("data:") || url.starts_with('/') || super::scan::external_url(url) {
                continue;
            }
            out.push_str(&code[last..whole.start()]);
            out.push_str(&format!(
                "new URL('' + {}, import.meta.url)",
                json(&self.0.url_of(&normalize(&dir.join(url))))
            ));
            last = whole.end();
        }
        if last == 0 {
            return Ok(None);
        }
        out.push_str(&code[last..]);
        Ok(Some(HookTransformOutput {
            code: Some(out),
            ..Default::default()
        }))
    }
}

/// What the bundle built for one entry name.
struct Built {
    file: String,
    cjs: bool,
}

impl Prebundle {
    /// Bundles the plan into the deps dir: the committed metadata per dep.
    /// The initial run starts from an empty dir; a rerun after serve-time
    /// discovery keeps the previous files, so a page that predates it still
    /// loads its chunks until the reload lands.
    pub async fn bundle(
        self: &Arc<Self>,
        plan: &Plan,
    ) -> Result<BTreeMap<String, Prebuilt>, String> {
        if !self.keep_existing {
            let _ = std::fs::remove_dir_all(&self.out_dir);
        }
        std::fs::create_dir_all(&self.out_dir)
            .map_err(|e| format!("{}: {e}", self.out_dir.display()))?;
        if plan.entries.is_empty() {
            return Ok(BTreeMap::new());
        }
        let built = self.run(&plan.entries).await?;
        let facades: Vec<String> = plan
            .names
            .iter()
            .filter(|(dep, name)| {
                self.facade_deps.contains(*dep) && built.get(*name).is_some_and(|b| b.cjs)
            })
            .map(|(dep, _)| dep.clone())
            .collect();
        let mut names = if facades.is_empty() {
            BTreeMap::new()
        } else {
            self.named_exports(facades).await
        };
        let mut out = BTreeMap::new();
        for (dep, name) in &plan.names {
            let Some(b) = built.get(name) else {
                continue;
            };
            let forced = self.needs_interop.contains(dep);
            let named = names.remove(dep).filter(|n| !n.is_empty());
            let (file, needs_interop) = match named {
                // A CJS react-family dep gets a named-export facade next to
                // its bundle, so modules that link it as ESM find its names.
                Some(names) => {
                    let facade = format!("{name}__oj_named.mjs");
                    let code = format!(
                        "import __m from \"./{}\";\nexport default __m;\nexport const __cjs_exports = __m;\nexport const {{ {} }} = __m;\n",
                        b.file,
                        names.join(", ")
                    );
                    std::fs::write(self.out_dir.join(&facade), code).map_err(|e| e.to_string())?;
                    (facade, forced)
                }
                None => (b.file.clone(), b.cjs || forced),
            };
            let file_hash = std::fs::read(self.out_dir.join(&file))
                .map(|bytes| blake3::hash(&bytes).to_hex()[..16].to_string())
                .unwrap_or_default();
            out.insert(
                dep.clone(),
                Prebuilt {
                    file,
                    needs_interop,
                    file_hash,
                },
            );
        }
        Ok(out)
    }

    async fn run(
        self: &Arc<Self>,
        entries: &BTreeMap<String, PathBuf>,
    ) -> Result<BTreeMap<String, Built>, String> {
        let mut options = self.options.bundler_options(false);
        options.input = Some(
            entries
                .iter()
                .map(|(name, file)| InputItem {
                    name: Some(name.clone()),
                    import: file.to_string_lossy().into_owned(),
                })
                .collect(),
        );
        options.cwd = Some(self.root.clone());
        options.dir = Some(self.out_dir.to_string_lossy().into_owned());
        options.format = Some(OutputFormat::Esm);
        options.entry_filenames = Some("[name].mjs".to_string().into());
        options.chunk_filenames = Some("[name]-[hash].mjs".to_string().into());
        let types = options.module_types.get_or_insert_default();
        // CSS is externalized at resolve; one that slips through must not
        // wake rolldown's own CSS handling, whose output nothing serves.
        types.entry(".css".to_string()).or_insert(ModuleType::Js);
        if needs_jsx_loader(entries) {
            types.insert(".js".to_string(), ModuleType::Jsx);
        }
        let plugins: Vec<SharedPluginable> = vec![
            Pluginable::new_shared(DepAssetsPlugin(Arc::clone(self))),
            Pluginable::new_shared(DepBundlePlugin(Arc::clone(self))),
        ];
        let mut bundler = BundlerBuilder::default()
            .with_options(options)
            .with_plugins(plugins)
            .build()
            .map_err(|e| diagnostics(&e.into_vec()))?;
        let written = bundler
            .write()
            .await
            .map_err(|e| diagnostics(&e.into_vec()));
        let _ = bundler.close().await;
        let mut built = BTreeMap::new();
        for output in written?.assets {
            let Output::Chunk(chunk) = output else {
                continue;
            };
            if !chunk.is_entry {
                continue;
            }
            let exports: Vec<String> = chunk.exports.iter().map(|e| e.to_string()).collect();
            let facade = chunk
                .facade_module_id
                .as_ref()
                .map(|f| Path::new(f.as_str()));
            built.insert(
                chunk.name.to_string(),
                Built {
                    file: chunk.filename.to_string(),
                    cjs: needs_interop(facade, &exports),
                },
            );
        }
        Ok(built)
    }

    /// The names a CommonJS dep's `module.exports` carries at runtime, read
    /// by requiring it in a short-lived engine: static analysis cannot see
    /// through React's `module.exports = require(env ? a : b)`.
    async fn named_exports(&self, deps: Vec<String>) -> BTreeMap<String, Vec<String>> {
        let cache = oj_cache::cache_root(&self.root);
        if crate::plugins::ensure_asset(&cache, "dep-names.mjs", DEP_NAMES_JS).is_err() {
            return BTreeMap::new();
        }
        let root = self.root.clone();
        let script = cache.join("dep-names.mjs");
        let payload = serde_json::json!({ "root": root.to_string_lossy(), "deps": deps });
        let job = tokio::task::spawn_blocking(move || {
            crate::plugins::run_engine_job(&root, &script, "namedExports", payload, None)
        });
        match job.await {
            Ok(Ok(v)) => serde_json::from_value(v).unwrap_or_default(),
            Ok(Err(e)) => {
                eprintln!("oj: reading the named exports of the CommonJS React deps failed: {e}");
                BTreeMap::new()
            }
            Err(_) => BTreeMap::new(),
        }
    }
}

const DEP_NAMES_JS: &str = include_str!("../assets/dep-names.mjs");

fn diagnostics(errs: &[rolldown_error::BuildDiagnostic]) -> String {
    errs.iter()
        .map(|e| e.to_diagnostic().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}
