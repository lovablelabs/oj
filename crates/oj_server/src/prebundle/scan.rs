// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite 8's scanImports (optimizer/scan.ts) on rolldown's scan: crawl the app
//! from its entries, record every bare import that resolves into
//! node_modules (or is listed in `include`) and stop there, keep crawling
//! into linked packages, externalize everything that is not JS. The include
//! list rides along as one virtual entry, so every pre-bundled dep resolves
//! through the resolver the bundle uses (Vite's
//! addManuallyIncludedOptimizeDeps). Every import resolves through the app's
//! plugins first, with `scan: true`, as Vite's scan goes through its plugin
//! container, and `optimizeDeps.rolldownOptions.plugins` join the crawl.

use std::borrow::Cow;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rolldown::{BundlerBuilder, InputItem};
use rolldown_common::{ModuleType, ResolvedExternal};
use rolldown_plugin::__inner::{Pluginable, SharedPluginable};
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookResolveIdArgs, HookResolveIdOutput,
    HookResolveIdReturn, HookTransformArgs, HookTransformOutput, HookTransformReturn, HookUsage,
    Plugin, PluginContext, PluginContextResolveOptions, SharedLoadPluginContext,
    SharedTransformPluginContext,
};
use tokio::sync::OnceCell;

use super::{
    clean_url, in_node_modules, is_bare, is_js_type, is_optimizable_entry, module_list_contains,
};
use super::{npm_package_name, Prebundle, Scanned};
use crate::plugins::PluginHost;

const INCLUDE_ID: &str = "\0oj-scan-include";
const HTML_SCRIPT_ID: &str = "\0oj-html-script:";

/// Vite's scan-time externals beyond the asset list: what it never crawls.
fn scan_external(id: &str) -> bool {
    let file = clean_url(id);
    [".json", ".json5", ".wasm"]
        .iter()
        .any(|e| file.ends_with(e))
}

/// `?worker`, `?sharedworker`, `?raw`, `?url`: queries the dev server owns.
fn special_query(id: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r"[?&](?:worker|sharedworker|raw|url)\b").unwrap()
    });
    RE.is_match(id)
}

pub(super) fn external_url(id: &str) -> bool {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?i)^(?:[a-z]+:)?//").unwrap());
    RE.is_match(id) || id.starts_with("data:")
}

/// Vite's esbuildDepPlugin / rolldownDepPlugin external types: styles,
/// preprocessors, wasm, single-file-component types and the known asset
/// types are never bundled into a dep; oj's own pipelines serve them.
pub(super) fn is_external_type(id: &str) -> bool {
    const TYPES: &[&str] = &[
        "css",
        "scss",
        "sass",
        "less",
        "styl",
        "stylus",
        "pcss",
        "postcss",
        "wasm",
        "vue",
        "svelte",
        "astro",
        "imba",
        "marko",
        "apng",
        "png",
        "jpg",
        "jpeg",
        "jfif",
        "pjpeg",
        "pjp",
        "gif",
        "svg",
        "ico",
        "webp",
        "avif",
        "bmp",
        "cur",
        "jxl",
        "woff",
        "woff2",
        "ttf",
        "otf",
        "eot",
        "mp4",
        "webm",
        "ogg",
        "mp3",
        "wav",
        "flac",
        "aac",
        "opus",
        "mov",
        "m4a",
        "vtt",
        "webmanifest",
        "pdf",
        "txt",
    ];
    let path = id.split('?').next().unwrap_or(id);
    path.rsplit_once('.').is_some_and(|(_, ext)| {
        !ext.contains('/') && TYPES.iter().any(|t| ext.eq_ignore_ascii_case(t))
    })
}

fn externalize(id: &str) -> HookResolveIdReturn {
    Ok(Some(HookResolveIdOutput {
        id: id.into(),
        external: Some(ResolvedExternal::Bool(true)),
        ..Default::default()
    }))
}

/// How many scan calls the plugin host runs at once. Rolldown issues a whole
/// crawl's resolves together, and each would start a plugin-chain run in the
/// host's one isolate: a dev request's hook would share every event-loop turn
/// with the whole crawl's work, and wait it out (issue #332). Bounded, each
/// turn the request takes shares the isolate with at most this many scan
/// calls; enough of them overlap that a plugin awaiting its own work
/// (`this.resolve`, fs) keeps the crawl near its unbounded pace. The cost of
/// a slow unit scales with it: a 6ms resolve chain delays a request ~100ms.
/// Per scan; the optimizer runs one at a time.
pub(super) const SCAN_HOST_TURNS: usize = 16;

/// The plugin host as the scan calls it: through [`SCAN_HOST_TURNS`] turns.
#[derive(Debug)]
struct ScanHost {
    host: Arc<PluginHost>,
    turns: tokio::sync::Semaphore,
}

impl ScanHost {
    async fn resolve_id(
        &self,
        source: &str,
        importer: &str,
    ) -> Result<Option<crate::plugins::ResolvedId>, String> {
        let _turn = self
            .turns
            .acquire()
            .await
            .expect("the scan never closes its turns");
        self.host.scan_resolve_id(source, importer).await
    }

    async fn plugin_hook(
        &self,
        hook: &str,
        args: &serde_json::Value,
    ) -> Result<Option<serde_json::Value>, String> {
        let _turn = self
            .turns
            .acquire()
            .await
            .expect("the scan never closes its turns");
        self.host.scan_plugin_hook(hook, args).await
    }
}

impl Prebundle {
    /// The deps the app imports, each with its bundle entry file. With
    /// `optimizeDeps.noDiscovery` only the include list is resolved. A scan
    /// that stops early (a syntax error in an app module) keeps what it found.
    pub async fn scan(self: &Arc<Self>, host: Option<&Arc<PluginHost>>) -> Scanned {
        let mut input: Vec<String> = if self.auto_discover {
            self.entries
                .iter()
                .map(|e| e.to_string_lossy().into_owned())
                .collect()
        } else {
            Vec::new()
        };
        let includes: Vec<String> = self
            .include
            .iter()
            .filter(|i| !i.contains('>'))
            .cloned()
            .collect();
        if !includes.is_empty() {
            input.push(INCLUDE_ID.to_string());
        }
        if input.is_empty() {
            return Vec::new();
        }
        let state = Arc::new(ScanState {
            pre: Arc::clone(self),
            host: host.map(|host| {
                Arc::new(ScanHost {
                    host: Arc::clone(host),
                    turns: tokio::sync::Semaphore::new(SCAN_HOST_TURNS),
                })
            }),
            includes,
            found: Mutex::default(),
            resolved: Mutex::default(),
            html_scripts: Mutex::default(),
        });
        let mut plugins: Vec<SharedPluginable> = Vec::new();
        if let Some(host) = &state.host {
            match host.host.scan_plugin_hooks().await {
                Ok(Some(hooks)) => plugins.push(Pluginable::new_shared(HostScanPlugins {
                    host: Arc::clone(host),
                    hooks,
                })),
                Ok(None) => {}
                Err(e) => eprintln!(
                    "oj: optimizeDeps.rolldownOptions.plugins unavailable to the scan ({e})"
                ),
            }
        }
        plugins.push(Pluginable::new_shared(DepScanPlugin(Arc::clone(&state))));
        let mut options = self.options.bundler_options(true);
        options.input = Some(
            input
                .into_iter()
                .map(|import| InputItem { name: None, import })
                .collect(),
        );
        options.cwd = Some(self.root.clone());
        // oj compiles JSX in plain .js app files; the scan must parse them.
        options
            .module_types
            .get_or_insert_default()
            .entry(".js".to_string())
            .or_insert(ModuleType::Jsx);
        let run = async {
            let mut bundler = BundlerBuilder::default()
                .with_options(options)
                .with_plugins(plugins)
                .build()
                .map_err(|e| first_message(&e.into_vec()))?;
            let out = bundler
                .scan()
                .await
                .map_err(|e| first_message(&e.into_vec()));
            let _ = bundler.close().await;
            out
        };
        if let Err(e) = crate::rolldown_runtime::on_rolldown(&self.rolldown, run).await {
            eprintln!("oj: dependency scan stopped early ({e})");
        }
        let found = state
            .found
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        found
    }
}

/// The first error's first line; for a failed load (whose first line only
/// names the module) its cause's too.
fn first_message(errs: &[rolldown_error::BuildDiagnostic]) -> String {
    let Some(first) = errs.first() else {
        return String::new();
    };
    let text = first.to_diagnostic().to_string();
    let keep = if crate::load_error::failed_load(first).is_some() {
        2
    } else {
        1
    };
    text.lines().take(keep).collect::<Vec<_>>().join(": ")
}

struct ScanState {
    pre: Arc<Prebundle>,
    host: Option<Arc<ScanHost>>,
    includes: Vec<String>,
    found: Mutex<Scanned>,
    /// Keyed on id and importer file (a plugin may resolve the same id
    /// differently per importer). The pending resolution is shared, so a
    /// duplicate arriving mid-flight never runs the plugin chain again.
    resolved: Mutex<HashMap<String, Arc<OnceCell<Option<String>>>>>,
    html_scripts: Mutex<HashMap<String, String>>,
}

impl ScanState {
    fn found(&self, id: &str) -> bool {
        self.found
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(d, _)| d == id)
    }

    /// The file `id` resolves to from `importer`: the app's plugins first (a
    /// plugin's virtual or external answer is not crawled), then rolldown's
    /// resolver with the aliases applied.
    async fn resolve(
        &self,
        ctx: &PluginContext,
        id: &str,
        importer: Option<&str>,
    ) -> Option<String> {
        let include_importer;
        let from = match importer {
            Some(INCLUDE_ID) | None => {
                include_importer = self
                    .pre
                    .root
                    .join("__oj_include__.js")
                    .to_string_lossy()
                    .into_owned();
                include_importer.as_str()
            }
            Some(i) => i,
        };
        let key = format!("{id}\0{from}");
        let cell = Arc::clone(
            self.resolved
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .entry(key)
                .or_default(),
        );
        cell.get_or_init(|| async {
            if let Some(host) = &self.host {
                if let Ok(Some(r)) = host.resolve_id(id, from).await {
                    let abs = Path::new(clean_url(&r.id)).is_absolute();
                    return (!r.is_external() && abs).then_some(r.id);
                }
            }
            let target = self.pre.alias.apply(id).unwrap_or_else(|| id.to_string());
            match ctx
                .resolve(
                    &target,
                    Some(from),
                    Some(PluginContextResolveOptions::default()),
                )
                .await
            {
                Ok(Ok(r)) if matches!(r.external, ResolvedExternal::Bool(false)) => {
                    Some(r.id.to_string())
                }
                _ => None,
            }
        })
        .await
        .clone()
    }

    /// Vite's htmlTypeOnLoadCallback: each `<script type="module">` of an
    /// html entry becomes an import of its src, an inline one a virtual
    /// module of its own (variable names may repeat between scripts).
    fn html_to_js(&self, file: &str) -> String {
        static COMMENT: std::sync::LazyLock<regex::Regex> =
            std::sync::LazyLock::new(|| regex::Regex::new(r"(?s)<!--.*?-->").unwrap());
        static SCRIPT: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(
                r#"(?is)(<script(?:\s+[a-z_:][-\w:]*(?:\s*=\s*(?:"[^"]*"|'[^']*'|[^"'<>=\s]+))?)*\s*>)(.*?)</script>"#,
            )
            .unwrap()
        });
        static SRC: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r#"(?i)\bsrc\s*=\s*(?:"([^"]+)"|'([^']+)'|([^\s'">]+))"#).unwrap()
        });
        static TYPE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(r#"(?i)\btype\s*=\s*(?:"([^"]+)"|'([^']+)'|([^\s'">]+))"#).unwrap()
        });
        let attr = |re: &regex::Regex, tag: &str| {
            re.captures(tag).and_then(|c| {
                c.iter()
                    .skip(1)
                    .flatten()
                    .next()
                    .map(|m| m.as_str().to_string())
            })
        };
        let Ok(raw) = std::fs::read_to_string(file) else {
            return String::new();
        };
        let raw = COMMENT.replace_all(&raw, "<!---->");
        let mut js = String::new();
        let mut n = 0;
        for caps in SCRIPT.captures_iter(&raw) {
            let tag = &caps[1];
            if attr(&TYPE, tag).as_deref() != Some("module") {
                continue;
            }
            if let Some(src) = attr(&SRC, tag) {
                js.push_str(&format!("import {};\n", serde_json::Value::String(src)));
            } else if !caps[2].trim().is_empty() {
                let key = format!("{HTML_SCRIPT_ID}{file}?id={n}");
                n += 1;
                js.push_str(&format!(
                    "export * from {};\n",
                    serde_json::Value::String(key.clone())
                ));
                self.html_scripts
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(key, caps[2].to_string());
            }
        }
        js.push_str("\nexport default {}");
        js
    }
}

#[derive(Debug)]
struct DepScanPlugin(Arc<ScanState>);

impl std::fmt::Debug for ScanState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScanState")
            .field("pre", &self.pre)
            .finish_non_exhaustive()
    }
}

impl Plugin for DepScanPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:dep-scan")
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load | HookUsage::Transform
    }

    async fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        let state = &self.0;
        let pre = &state.pre;
        let id = args.specifier;
        if id == INCLUDE_ID || id.starts_with(HTML_SCRIPT_ID) {
            return Ok(Some(HookResolveIdOutput::from_id(id)));
        }
        let Some(mut importer) = args.importer else {
            return Ok(None);
        };
        // An inline html script imports relative to its html file.
        let html_importer;
        if let Some(rest) = importer.strip_prefix(HTML_SCRIPT_ID) {
            html_importer = clean_url(rest).to_string();
            importer = &html_importer;
        }
        if external_url(id) || special_query(id) {
            return externalize(id);
        }
        // Aliased or not, as in Vite's scan: an alias into node_modules
        // records the dep under its bare name, one into the app's source is
        // crawled like any source file.
        if is_bare(id) {
            if id.contains('?') || module_list_contains(&pre.exclude, id) || state.found(id) {
                return externalize(id);
            }
            // resolve.dedupe from the root: the recorded file becomes the
            // bundle entry, so a deduped dep first seen from a linked package
            // must still pin the root copy.
            let deduped = npm_package_name(id).is_some_and(|p| pre.dedupe.contains(p));
            let from = if deduped { INCLUDE_ID } else { importer };
            let Some(resolved) = state.resolve(ctx, id, Some(from)).await else {
                return externalize(id);
            };
            if !Path::new(&resolved).is_absolute() || resolved.contains('\0') {
                return externalize(id);
            }
            // Queries stripped: the recorded file is a bundle entry, a path
            // rolldown must open.
            let file = clean_url(&resolved);
            if in_node_modules(&resolved) || pre.include.iter().any(|i| i == id) {
                if is_optimizable_entry(file) {
                    let mut found = state.found.lock().unwrap_or_else(|e| e.into_inner());
                    if !found.iter().any(|(d, _)| d == id) {
                        found.push((id.to_string(), PathBuf::from(file)));
                    }
                }
                return externalize(id);
            }
            return if is_js_type(file) {
                Ok(Some(HookResolveIdOutput::from_id(file)))
            } else {
                externalize(id)
            };
        }
        if is_external_type(id) || scan_external(id) {
            return externalize(id);
        }
        // `<script src="/main.tsx">`: a dev-server-rooted URL, the file under
        // the root.
        if id.starts_with('/') {
            let abs = pre.root.join(clean_url(id).trim_start_matches('/'));
            if abs.exists() {
                let s = abs.to_string_lossy();
                return if is_js_type(&s) || s.ends_with(".html") {
                    Ok(Some(HookResolveIdOutput::from_id(s.into_owned())))
                } else {
                    externalize(id)
                };
            }
        }
        match state.resolve(ctx, id, Some(importer)).await {
            Some(r) if Path::new(&r).is_absolute() && is_js_type(&r) => {
                Ok(Some(HookResolveIdOutput::from_id(clean_url(&r))))
            }
            _ => externalize(id),
        }
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        let state = &self.0;
        let (code, module_type) = if args.id == INCLUDE_ID {
            let code = state
                .includes
                .iter()
                .map(|i| format!("import {};", serde_json::Value::String(i.clone())))
                .collect::<Vec<_>>()
                .join("\n");
            (code, ModuleType::Js)
        } else if args.id.starts_with(HTML_SCRIPT_ID) {
            let scripts = state.html_scripts.lock().unwrap_or_else(|e| e.into_inner());
            (
                scripts.get(args.id).cloned().unwrap_or_default(),
                ModuleType::Jsx,
            )
        } else if clean_url(args.id).ends_with(".html") {
            (state.html_to_js(clean_url(args.id)), ModuleType::Js)
        } else {
            return Ok(None);
        };
        Ok(Some(HookLoadOutput {
            code: code.into(),
            module_type: Some(module_type),
            ..Default::default()
        }))
    }

    /// Vite's vite:dep-scan:transform:js-glob and its html script path: the
    /// glob calls expanded, so the crawl follows every file they match.
    async fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> HookTransformReturn {
        if !args.code.contains("import.meta.glob") {
            return Ok(None);
        }
        let html = args.id.strip_prefix(HTML_SCRIPT_ID);
        let file = clean_url(html.unwrap_or(args.id));
        if html.is_none() && !is_js_type(file) {
            return Ok(None);
        }
        let mut scopes = Vec::new();
        match oj_compiler::meta_glob::transform_glob(
            args.code,
            Path::new(file),
            &self.0.pre.root,
            &|id| self.0.pre.alias.apply(id),
            &mut scopes,
        ) {
            Ok(code) if code != args.code.as_str() => Ok(Some(HookTransformOutput {
                code: Some(code),
                ..Default::default()
            })),
            _ => Ok(None),
        }
    }
}

/// Which hooks `optimizeDeps.rolldownOptions.plugins` implement.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ScanPluginHooks {
    pub resolve_id: bool,
    pub load: bool,
    pub transform: bool,
}

/// `optimizeDeps.rolldownOptions.plugins`, run in the plugin host ahead of
/// the scan's own plugin (Vite puts them first in the scan's plugin list).
#[derive(Debug)]
struct HostScanPlugins {
    host: Arc<ScanHost>,
    hooks: ScanPluginHooks,
}

impl Plugin for HostScanPlugins {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:dep-scan-config-plugins")
    }

    fn register_hook_usage(&self) -> HookUsage {
        let mut usage = HookUsage::empty();
        if self.hooks.resolve_id {
            usage |= HookUsage::ResolveId;
        }
        if self.hooks.load {
            usage |= HookUsage::Load;
        }
        if self.hooks.transform {
            usage |= HookUsage::Transform;
        }
        usage
    }

    async fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> HookResolveIdReturn {
        let r = self
            .host
            .plugin_hook(
                "resolveId",
                &serde_json::json!([
                    args.specifier,
                    args.importer,
                    { "isEntry": args.is_entry, "kind": args.kind.to_string() }
                ]),
            )
            .await
            .map_err(anyhow::Error::msg)?;
        let Some(r) = r.as_ref().and_then(crate::plugins::ResolvedId::from_json) else {
            return Ok(None);
        };
        Ok(Some(HookResolveIdOutput {
            external: r.is_external().then_some(ResolvedExternal::Bool(true)),
            id: r.id.into(),
            ..Default::default()
        }))
    }

    async fn load(&self, _ctx: SharedLoadPluginContext, args: &HookLoadArgs<'_>) -> HookLoadReturn {
        let r = self
            .host
            .plugin_hook("load", &serde_json::json!([args.id]))
            .await
            .map_err(|e| {
                crate::load_error::load_failed(self.name(), args.id, anyhow::Error::msg(e))
            })?;
        let Some(code) = r
            .as_ref()
            .and_then(|r| r.get("code"))
            .and_then(|c| c.as_str())
        else {
            return Ok(None);
        };
        let module_type = r
            .as_ref()
            .and_then(|r| r.get("moduleType"))
            .and_then(|t| t.as_str())
            .map(ModuleType::from_str_with_fallback);
        Ok(Some(HookLoadOutput {
            code: code.into(),
            module_type,
            ..Default::default()
        }))
    }

    async fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> HookTransformReturn {
        let r = self
            .host
            .plugin_hook(
                "transform",
                &serde_json::json!([args.code.as_str(), args.id]),
            )
            .await
            .map_err(anyhow::Error::msg)?;
        Ok(r.as_ref()
            .and_then(|r| r.get("code"))
            .and_then(|c| c.as_str())
            .map(|code| HookTransformOutput {
                code: Some(code.to_string()),
                ..Default::default()
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::first_message;

    #[test]
    fn a_failed_load_stops_the_scan_naming_module_and_cause() {
        let e = crate::load_error::load_failed(
            "oj:dep-scan-config-plugins".into(),
            "/app/src/a.ts",
            anyhow::anyhow!("[plugin:x] load exploded"),
        );
        let d = e.downcast::<rolldown_error::BuildDiagnostic>().unwrap();
        assert_eq!(
            first_message(&[d]),
            "[oj:dep-scan-config-plugins] Could not load /app/src/a.ts: [plugin:x] load exploded"
        );
    }
}
