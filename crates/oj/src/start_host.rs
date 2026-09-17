// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The in-process TanStack Start runner: an embedded JS engine whose module
//! loading reproduces the semantics of the retired Node loader
//! (`registerHooks` + `loader.mjs`) on top of the dev server's SSR pipeline.
//!
//! Resolution and loading decisions live here, in the [`ModuleHost`]; what
//! genuinely stays JS (the `fetch` handler protocol) lives in the bootstrap
//! module. Compared to the node loader:
//! - TS/JSX stripping, `define`/`import.meta.env`, `import.meta.glob` and the
//!   plugin transform chain come from the same Rust pipeline the SSR runner
//!   uses (`SsrBridge::transform_module`), replacing rolldown `transformSync`
//!   + `glob-transform.mjs` + the FIFO plugin bridge.
//! - CSS-module class names come from the Rust side (`oj_css`), replacing the
//!   loader's SipHash mirror.
//! - CJS facades, directory-entry recovery and extension probing inside
//!   node_modules are the engine's byonm loading plus the Vite-style resolver
//!   (externals travel as the RESOLVED file, not the bare specifier).
//! - The on-disk transform cache keyed on the Node version is gone: the
//!   pipeline's own caches and the isolate's module map cover it.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use oj_js::EngineConfig;
use oj_js::HostFuture;
use oj_js::HostModule;
use oj_js::HostModuleType;
use oj_js::HostResolved;
use oj_js::JsEngine;
use oj_js::ModuleHost;
use oj_server::SsrBridge;
use oj_server::StartResolution;

use crate::ssr_host::{importer_id, versioned_id, VersionGraph};

const START_BOOTSTRAP_JS: &str = include_str!("assets/start-bootstrap.mjs");

/// A module id the resolver rejected with a scheme the runtime cannot load
/// (an un-aliased `cloudflare:*`): served as an empty module instead of
/// crashing the render, like the node loader's scheme net.
const STUB_PREFIX: &str = "oj-start-stub:";

/// Extensions probed for the Start-alias and root-relative ladders (Vite's
/// DEFAULT_EXTENSIONS as files and directory indexes, like the loader's EXTS).
const PROBE_EXTS: [&str; 7] = [".ts", ".tsx", ".mts", ".js", ".jsx", ".mjs", ".json"];

fn probe(base: &Path) -> Option<PathBuf> {
    if base.is_file() {
        return Some(base.to_path_buf());
    }
    let s = base.to_string_lossy();
    for ext in PROBE_EXTS {
        let p = PathBuf::from(format!("{s}{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    for ext in PROBE_EXTS {
        let p = base.join(format!("index{ext}"));
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Vite's `ssr.noExternal` matcher (names, `@scope/*` globs, RegExp sources,
/// `true`), applied to package names; `external` names always win.
pub(crate) struct NoExternal {
    rules: oj_config::SsrExternals,
    regexes: Vec<regex::Regex>,
}

impl NoExternal {
    fn new(rules: oj_config::SsrExternals) -> NoExternal {
        let regexes = rules
            .no_external_regex
            .iter()
            .filter_map(|src| regex::Regex::new(src).ok())
            .collect();
        NoExternal { rules, regexes }
    }

    fn matches_pkg(&self, pkg: &str) -> bool {
        if self.rules.external.iter().any(|e| e == pkg) || self.rules.external_all {
            return false;
        }
        if self.rules.no_external_all {
            return true;
        }
        if self.rules.no_external.iter().any(|p| glob_match(p, pkg)) {
            return true;
        }
        self.regexes.iter().any(|re| re.is_match(pkg))
    }

    pub(crate) fn matches_path(&self, path: &str) -> bool {
        match pkg_name_of_path(path) {
            Some(pkg) => self.matches_pkg(&pkg),
            None => false,
        }
    }
}

fn glob_match(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    let re = format!(
        "^{}$",
        pattern
            .split('*')
            .map(regex::escape)
            .collect::<Vec<_>>()
            .join(".*")
    );
    regex::Regex::new(&re)
        .map(|r| r.is_match(value))
        .unwrap_or(false)
}

/// Package name of a node_modules path (`.../node_modules/@scope/pkg/x` ->
/// `@scope/pkg`).
fn pkg_name_of_path(path: &str) -> Option<String> {
    let i = path.rfind("/node_modules/")?;
    let rest = &path[i + "/node_modules/".len()..];
    let mut parts = rest.split('/');
    let first = parts.next().filter(|p| !p.is_empty())?;
    if let Some(scope) = first.strip_prefix('@').map(|_| first) {
        let second = parts.next().filter(|p| !p.is_empty())?;
        return Some(format!("{scope}/{second}"));
    }
    Some(first.to_string())
}

/// Base64url without padding (Node's `Buffer.toString("base64url")`), the
/// server-function id encoding shared with gen-resolver.mjs and the client
/// bundle.
fn base64url(bytes: &[u8]) -> String {
    crate::ssr_host::base64(bytes)
        .trim_end_matches('=')
        .replace('+', "-")
        .replace('/', "_")
}

/// The server-side `createServerFn` rewrite the node loader applied
/// (loader-util.mjs `rewriteServerFns`): every
/// `const NAME = createServerFn(...).handler(` grows a sibling
/// `NAME_createServerFn_handler = createServerRpc(meta, ...)` export and the
/// handler's first argument, without adding lines (stack traces keep their
/// positions).
pub(crate) fn rewrite_server_fns(code: &str, rel: &str) -> String {
    if !code.contains("createServerFn") {
        return code.to_string();
    }
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(
            r"(^|[\n;])([ \t]*)((?:export\s+)?const\s+([A-Za-z_$][A-Za-z0-9_$]*)\s*=\s*createServerFn\b(?s:.)*?\.handler\s*\()",
        )
        .expect("server-fn rewrite regex")
    });
    let mut changed = false;
    let out = re.replace_all(code, |caps: &regex::Captures| {
        changed = true;
        let (pre, indent, decl, name) = (&caps[1], &caps[2], &caps[3], &caps[4]);
        let id = base64url(format!("{rel}#{name}").as_bytes());
        let meta = format!(
            "{{ id: {id:?}, name: {name:?}, filename: {rel:?} }}",
        );
        let rpc = format!(
            "export const {name}_createServerFn_handler = createServerRpc({meta}, (opts) => {name}.__executeServer(opts)); "
        );
        format!("{pre}{indent}{rpc}{decl}{name}_createServerFn_handler, ")
    });
    if !changed {
        return code.to_string();
    }
    format!("import {{ createServerRpc }} from \"@tanstack/react-start/server-rpc\"; {out}")
}

fn ext_of(path: &str) -> &str {
    let name = path.rsplit('/').next().unwrap_or(path);
    match name.rfind('.') {
        Some(i) if i > 0 => &name[i + 1..],
        _ => "",
    }
}

fn src_lang(path: &str) -> Option<&'static str> {
    match ext_of(path) {
        "ts" | "mts" | "cts" => Some("ts"),
        "tsx" => Some("tsx"),
        "jsx" => Some("jsx"),
        "js" | "mjs" => Some("js"),
        _ => None,
    }
}

/// The loader's `isCjsFile`: `.cjs` always, `.js` when the nearest
/// package.json is not `"type": "module"` and the file shows no ESM syntax.
fn is_cjs_file(path: &str) -> bool {
    if path.ends_with(".cjs") {
        return true;
    }
    if !path.ends_with(".js") {
        return false;
    }
    let mut dir = Path::new(path).parent();
    for _ in 0..40 {
        let Some(d) = dir else { break };
        let pj = d.join("package.json");
        if pj.is_file() {
            let is_module = std::fs::read_to_string(&pj)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
                .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(|t| t == "module"))
                .unwrap_or(false);
            if is_module {
                return false;
            }
            break;
        }
        dir = d.parent();
    }
    let Ok(src) = std::fs::read_to_string(path) else {
        return false;
    };
    !has_esm_syntax(&src)
}

fn has_esm_syntax(src: &str) -> bool {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    let re = RE.get_or_init(|| {
        regex::Regex::new(r"(?:^|[\n;])\s*export\s|(?:^|[\n;])\s*import\s[^(]")
            .expect("esm syntax regex")
    });
    re.is_match(src)
}

fn mime_of(path: &str) -> &'static str {
    match ext_of(path).to_ascii_lowercase().as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "ico" => "image/x-icon",
        "svg" => "image/svg+xml",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "ogg" => "audio/ogg",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "mov" => "video/quicktime",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "webmanifest" => "application/manifest+json",
        "wasm" => "application/wasm",
        "css" => "text/css",
        "json" => "application/json",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

fn json_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

/// Classify a resolved filesystem path into the module id the host serves: a
/// plain path (source modules, svg, json, mdx), or a path with an `?ojasset` /
/// `?react` tag for asset intents, styles and importable assets (resolve
/// first, classify second — Vite's ordering).
fn classify_id(abs: &str, intent: Option<&'static str>, svg_react: bool) -> String {
    if svg_react {
        return format!("{abs}?react");
    }
    if let Some(kind) = intent {
        return format!("{abs}?ojasset={kind}");
    }
    let ext = ext_of(abs).to_ascii_lowercase();
    if oj_server::is_style_ext(&ext) {
        return format!("{abs}?ojasset=css");
    }
    // wasm is asset-classified like the loader (URL export only).
    if oj_server::is_importable_asset_ext(&ext) || ext == "wasm" {
        return format!("{abs}?ojasset=url");
    }
    abs.to_string()
}

/// The intent query the loader recognized on a specifier.
fn split_intent(spec: &str) -> (String, Option<&'static str>, bool) {
    if let Some(clean) = spec.strip_suffix("?react") {
        if clean.ends_with(".svg") {
            return (clean.to_string(), None, true);
        }
    }
    for intent in ["raw", "url", "inline"] {
        if let Some(clean) = spec.strip_suffix(&format!("?{intent}")) {
            let tag: &'static str = match intent {
                "raw" => "raw",
                "url" => "url",
                _ => "inline",
            };
            return (clean.to_string(), Some(tag), false);
        }
    }
    (spec.to_string(), None, false)
}

/// The Start framework-seam aliases (the retired node loader's ALIASES map):
/// resolved targets under the app or the start cache dir. start-server-core
/// imports these by bare specifier and expects the bundler to answer; one the
/// host does not map fails every document request.
fn framework_aliases(root: &Path, cache: &Path) -> Vec<(String, PathBuf)> {
    vec![
        ("#tanstack-router-entry".into(), root.join("src/router")),
        ("#tanstack-start-entry".into(), cache.join("start-entry.ts")),
        (
            "#tanstack-start-plugin-adapters".into(),
            cache.join("plugin-adapters.ts"),
        ),
        (
            "#tanstack-start-server-fn-resolver".into(),
            cache.join("server-fn-resolver.mjs"),
        ),
        (
            "tanstack-start-manifest:v".into(),
            cache.join("manifest-dev.ts"),
        ),
        (
            "tanstack-start-injected-head-scripts:v".into(),
            cache.join("injected-head-scripts.ts"),
        ),
        (
            "@cloudflare/vite-plugin/server".into(),
            cache.join("cf-server.mjs"),
        ),
        // Vite externalizes `cloudflare:*` into workerd; here there is no
        // workerd, so the one module server fns actually use resolves to a
        // dev stub whose `env` comes from the wrangler vars. Other
        // cloudflare:* ids fall to the empty-module net in resolve.
        ("cloudflare:workers".into(), cache.join("cf-workers.mjs")),
        // Start's default server entry is oj's runner entry: an app
        // `server.entry` that wraps it composes the same way as in Vite.
        (
            "@tanstack/react-start/server-entry".into(),
            cache.join("server-entry.tsx"),
        ),
    ]
}

pub struct StartHost {
    bridge: SsrBridge,
    root: PathBuf,
    /// `publicDir` (adopted from the config); `None` when disabled.
    public_dir: Option<PathBuf>,
    /// The Start framework-seam aliases (the loader's ALIASES map): resolved
    /// targets under the app or the start cache dir.
    aliases: Vec<(String, PathBuf)>,
    no_external: NoExternal,
    graph: VersionGraph,
    /// buildStart for the ssr plugin container ran (once per session), so
    /// compile-on-startup plugins have their state before any load().
    plugins_started: tokio::sync::OnceCell<()>,
    warned_schemes: Mutex<HashSet<String>>,
}

impl StartHost {
    pub fn new(
        root: PathBuf,
        cache: &Path,
        bridge: SsrBridge,
        no_external: oj_config::SsrExternals,
        public_dir: Option<PathBuf>,
    ) -> Arc<StartHost> {
        let aliases = framework_aliases(&root, cache);
        Arc::new(StartHost {
            bridge,
            root,
            public_dir,
            aliases,
            no_external: NoExternal::new(no_external),
            graph: VersionGraph::default(),
            plugins_started: tokio::sync::OnceCell::new(),
            warned_schemes: Mutex::new(HashSet::new()),
        })
    }

    pub(crate) fn graph(&self) -> &VersionGraph {
        &self.graph
    }

    /// buildStart on the ssr plugin container, once, before its first use:
    /// Vite runs it before any module loads, and compile-on-startup plugins
    /// (i18n barrels) serve their state from load(). A throwing plugin is
    /// logged with attribution and skipped, never taking the dev server down.
    async fn ensure_plugins_started(&self) {
        self.plugins_started
            .get_or_init(|| async {
                let Some(host) = self.bridge.plugin_host().await else {
                    return;
                };
                if let Err(e) = host.build_start().await {
                    let name = e
                        .split_once("[plugin:")
                        .and_then(|(_, rest)| rest.split_once(']'))
                        .map(|(name, _)| name.to_string())
                        .unwrap_or_else(|| "unknown".into());
                    let msg = e.lines().next().unwrap_or(&e).to_string();
                    eprintln!("oj: plugin \"{name}\" buildStart failed (skipped): {msg}");
                }
            })
            .await;
    }

    fn alias_target(&self, spec: &str) -> Option<&Path> {
        self.aliases
            .iter()
            .find(|(find, _)| find == spec)
            .map(|(_, target)| target.as_path())
    }

    fn stub_specifier(&self, spec: &str) -> String {
        let scheme = spec.split(':').next().unwrap_or("").to_string();
        if self.warned_schemes.lock().unwrap().insert(scheme.clone()) {
            eprintln!(
                "oj: no SSR module for the '{scheme}:' scheme (e.g. {spec}); serving an empty module"
            );
        }
        self.graph.specifier_for(&format!("{STUB_PREFIX}{spec}"))
    }

    fn module_url(&self, id: &str, importer: String) -> HostResolved {
        HostResolved::Url(self.graph.edge_specifier(id, importer))
    }

    async fn resolve_inner(
        &self,
        importer: &str,
        spec: &str,
    ) -> Result<Option<HostResolved>, String> {
        let importer_id = importer_id(importer);

        // The framework-seam aliases first, from every importer (tanstack's
        // own dist imports `tanstack-start-manifest:v` and friends). For
        // `#`-prefixed keys the app's own package.json `imports` wins over the
        // convention, so the resolver gets the first try.
        if let Some(target) = self.alias_target(spec) {
            let bridge_first = spec.starts_with('#');
            if !bridge_first {
                if let Some(hit) = probe(target) {
                    return Ok(Some(
                        self.module_url(&hit.to_string_lossy(), importer_id),
                    ));
                }
            } else {
                if let Ok(StartResolution::Module(id)) =
                    self.bridge.resolve_start(&importer_id, spec).await
                {
                    return Ok(Some(self.module_url(&id, importer_id)));
                }
                if let Some(hit) = probe(target) {
                    return Ok(Some(
                        self.module_url(&hit.to_string_lossy(), importer_id),
                    ));
                }
            }
        }

        let is_plugin_shaped = spec.starts_with("virtual:") || spec.starts_with('\0');

        // node_modules internals resolve with plain Node semantics, as they
        // did under the node runner (only aliases and plugin ids cross).
        if importer_id.contains("/node_modules/") && !is_plugin_shaped {
            return Ok(Some(HostResolved::External(spec.to_string())));
        }

        let (clean, intent, svg_react) = split_intent(spec);

        // A /-prefixed id resolves against the project root (Vite's asSrc),
        // then against publicDir; a real absolute fs path falls through.
        if clean.starts_with('/') {
            let rel = clean.trim_start_matches('/');
            let public_hit = || {
                self.public_dir
                    .as_ref()
                    .and_then(|public| probe(&public.join(rel)))
            };
            if let Some(hit) = probe(&self.root.join(rel)).or_else(public_hit) {
                let id = classify_id(&hit.to_string_lossy(), intent, svg_react);
                return Ok(Some(self.module_url(&id, importer_id)));
            }
        }

        match self.bridge.resolve_start(&importer_id, &clean).await {
            Ok(StartResolution::Module(id)) => {
                if Path::new(&id).is_absolute() {
                    let tagged = classify_id(&id, intent, svg_react);
                    // A CommonJS file (a linked `file:` dependency, a .cjs
                    // module) is Node's to load: byonm gives it require
                    // semantics and lexed named exports, like the node
                    // loader's fall-through did.
                    if tagged == id && is_cjs_file(&id) {
                        if let Ok(u) = url::Url::from_file_path(&id) {
                            return Ok(Some(HostResolved::External(u.to_string())));
                        }
                    }
                    return Ok(Some(self.module_url(&tagged, importer_id)));
                }
                Ok(Some(self.module_url(&id, importer_id)))
            }
            Ok(StartResolution::Dependency(p)) => {
                let abs = p.to_string_lossy().into_owned();
                let ext = ext_of(&abs).to_ascii_lowercase();
                if svg_react
                    || intent.is_some()
                    || oj_server::is_style_ext(&ext)
                    || oj_server::is_importable_asset_ext(&ext)
                {
                    let id = classify_id(&abs, intent, svg_react);
                    return Ok(Some(self.module_url(&id, importer_id)));
                }
                // Hand the engine the exact file the Vite-style resolver
                // picked (mainFields, extension probing, exports conditions,
                // legacy directory entries); byonm loads it with Node
                // semantics (CJS translation included).
                match url::Url::from_file_path(&p) {
                    Ok(u) => Ok(Some(HostResolved::External(u.to_string()))),
                    Err(_) => Ok(Some(HostResolved::External(clean))),
                }
            }
            Ok(StartResolution::Bare(spec)) => {
                // A scheme Node's loader would reject (an un-aliased
                // `cloudflare:*`) becomes an empty module instead of taking
                // the server down; `node:` and bare npm names stay external.
                if let Ok(u) = url::Url::parse(&spec) {
                    if u.scheme().len() > 1 && u.scheme() != "node" {
                        return Ok(Some(HostResolved::Url(self.stub_specifier(&spec))));
                    }
                }
                Ok(Some(HostResolved::External(spec)))
            }
            Err(e) => Err(format!("{e} (imported from {importer_id})")),
        }
    }

    async fn load_inner(&self, specifier: &str) -> Result<Option<HostModule>, String> {
        // Plain file loads (byonm-resolved) the host still claims: json as
        // ESM with named exports, and noExternal dependencies through the
        // transform pipeline (define/env/glob, plugin transforms).
        let Some(id) = versioned_id(specifier) else {
            return self.load_unversioned(specifier).await;
        };
        self.ensure_plugins_started().await;

        let module = self.load_id(&id).await?;
        self.graph.record_loaded(&id);
        Ok(Some(module))
    }

    async fn load_unversioned(&self, specifier: &str) -> Result<Option<HostModule>, String> {
        let Ok(u) = url::Url::parse(specifier) else {
            return Ok(None);
        };
        if u.scheme() != "file" {
            return Ok(None);
        }
        let Ok(path) = u.to_file_path() else {
            return Ok(None);
        };
        let path_str = path.to_string_lossy().into_owned();
        if !path_str.contains("/node_modules/") {
            return Ok(None);
        }
        if path_str.ends_with(".json") {
            // Vite's json plugin (namedExports) for dependency json too.
            let code = self
                .bridge
                .load_module(&path_str)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(Some(HostModule {
                code,
                module_type: HostModuleType::JavaScript,
            }));
        }
        // `ssr.noExternal` dependencies run through the pipeline (Vite
        // bundles them through its plugins); everything else is byonm's.
        if !self.no_external.matches_path(&path_str) {
            return Ok(None);
        }
        if src_lang(&path_str).is_none() || is_cjs_file(&path_str) {
            return Ok(None);
        }
        self.ensure_plugins_started().await;
        let source =
            std::fs::read_to_string(&path).map_err(|e| format!("read {path_str}: {e}"))?;
        let code = self
            .bridge
            .transform_module(&path_str, source, false, true)
            .await
            .map_err(|e| e.to_string())?;
        Ok(Some(HostModule {
            code,
            module_type: HostModuleType::JavaScript,
        }))
    }

    async fn load_id(&self, id: &str) -> Result<HostModule, String> {
        let js = |code: String| HostModule {
            code,
            module_type: HostModuleType::JavaScript,
        };
        if let Some(spec) = id.strip_prefix(STUB_PREFIX) {
            let _ = spec;
            return Ok(js("export default {};".into()));
        }

        let (clean, query) = match id.split_once('?') {
            Some((c, q)) => (c.to_string(), Some(q.to_string())),
            None => (id.to_string(), None),
        };
        let is_path = Path::new(&clean).is_absolute();

        // svgr: the plugin owns .svg modules (bare and ?react); without a
        // claiming plugin the module is its served URL.
        if is_path && clean.ends_with(".svg") {
            let plugin_id = match &query {
                Some(q) => format!("{clean}?{q}"),
                None => clean.clone(),
            };
            let loaded = match self.bridge.plugin_host().await {
                Some(host) => host.load(&plugin_id).await.ok().flatten(),
                None => None,
            };
            let code = loaded
                .unwrap_or_else(|| format!("export default {};", json_str(&format!("/@oj-start/fs{clean}"))));
            return Ok(js(code));
        }

        if let Some(kind) = query.as_deref().and_then(|q| q.strip_prefix("ojasset=")) {
            return Ok(js(self.asset_module(&clean, kind).await?));
        }

        if !is_path {
            // A plugin virtual id: the pipeline's load() consults the plugin
            // host and compiles the result.
            return self
                .bridge
                .load_module(id)
                .await
                .map(js)
                .map_err(|e| e.to_string());
        }

        if clean.ends_with(".json") {
            return self
                .bridge
                .load_module(&clean)
                .await
                .map(js)
                .map_err(|e| e.to_string());
        }

        if clean.ends_with(".mdx") {
            let raw =
                std::fs::read_to_string(&clean).map_err(|e| format!("read {clean}: {e}"))?;
            let compiled = match self.bridge.plugin_host().await {
                Some(host) => host
                    .transform(&raw, &clean, "{}")
                    .await
                    .map(|(code, _, _, _)| code)
                    .map_err(|e| e.to_string())?,
                None => return Err(format!("no plugin compiles {clean}")),
            };
            // Compile the mdx output (jsx) with the pipeline's dev-ssr
            // options; `from_plugin` keeps the compile off the .mdx path.
            let code = self
                .bridge
                .transform_module(&clean, compiled, true, false)
                .await
                .map_err(|e| e.to_string())?;
            return Ok(js(code));
        }

        // Source modules: a plugin load() overrides the on-disk file (Vite
        // runs load before the fs read); app sources get the server-fn
        // rewrite; the pipeline does plugin transforms + compile.
        let in_deps = clean.contains("/node_modules/");
        let plugin_loaded = match self.bridge.plugin_host().await {
            Some(host) => host.load(&clean).await.ok().flatten(),
            None => None,
        };
        let from_plugin = plugin_loaded.is_some();
        let raw = match plugin_loaded {
            Some(code) => code,
            None => match std::fs::read_to_string(&clean) {
                Ok(s) => s,
                Err(_) => {
                    // Not readable and no plugin claim: the pipeline's own
                    // fallback (virtual content, precise errors).
                    return self
                        .bridge
                        .load_module(&clean)
                        .await
                        .map(js)
                        .map_err(|e| e.to_string());
                }
            },
        };
        let raw = if !in_deps {
            let rel = clean
                .strip_prefix(&format!("{}/", self.root.to_string_lossy()))
                .unwrap_or(&clean)
                .to_string();
            rewrite_server_fns(&raw, &rel)
        } else {
            raw
        };
        let code = self
            .bridge
            .transform_module(&clean, raw, from_plugin, true)
            .await
            .map_err(|e| e.to_string())?;
        Ok(js(code))
    }

    async fn asset_module(&self, path: &str, kind: &str) -> Result<String, String> {
        match kind {
            "raw" => {
                let text =
                    std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
                Ok(format!("export default {};", json_str(&text)))
            }
            "inline" => {
                let ext = ext_of(path).to_ascii_lowercase();
                if oj_server::is_style_ext(&ext) {
                    let text =
                        std::fs::read_to_string(path).map_err(|e| format!("read {path}: {e}"))?;
                    Ok(format!("export default {};", json_str(&text)))
                } else {
                    let bytes = std::fs::read(path).map_err(|e| format!("read {path}: {e}"))?;
                    let url = format!(
                        "data:{};base64,{}",
                        mime_of(path),
                        crate::ssr_host::base64(&bytes)
                    );
                    Ok(format!("export default {};", json_str(&url)))
                }
            }
            "css" => {
                // The pipeline serves css modules with the client's exact
                // scoped names (oj_css) and plain css as an empty module.
                self.bridge
                    .load_module(path)
                    .await
                    .map_err(|e| e.to_string())
            }
            // "url" and anything unrecognized: the served-URL module.
            _ => Ok(format!(
                "export default {};",
                json_str(&format!("/@oj-start/fs{path}"))
            )),
        }
    }
}

impl ModuleHost for StartHost {
    fn resolve<'a>(
        &'a self,
        importer: &'a str,
        specifier: &'a str,
    ) -> HostFuture<'a, Result<Option<HostResolved>, String>> {
        Box::pin(self.resolve_inner(importer, specifier))
    }

    fn load<'a>(
        &'a self,
        specifier: &'a str,
    ) -> HostFuture<'a, Result<Option<HostModule>, String>> {
        Box::pin(self.load_inner(specifier))
    }
}

/// One request into the app's fetch handler, as the bootstrap consumes it.
#[derive(Debug)]
pub struct StartRequest {
    pub method: String,
    pub url: String,
    pub host: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<Vec<u8>>,
}

/// The handler's reply: status/headers/set-cookies plus the whole body (the
/// stream is read to completion in the bootstrap, so deferred TanStack
/// content is present).
#[derive(Debug)]
pub struct StartResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub set_cookies: Vec<String>,
    pub body: Vec<u8>,
}

/// The Start runner handle `start_dev` calls per request. Wraps the engine so
/// a respawn (stale-instance reclaim after many reloads) is transparent:
/// requests share it behind a read lock, the respawn takes the write lock.
pub struct StartEngine {
    host: Arc<StartHost>,
    root: PathBuf,
    entry_id: String,
    bootstrap: String,
    init_env: Vec<(String, String)>,
    engine: tokio::sync::RwLock<EngineSlot>,
}

/// A spawned engine plus its one-time bootstrap init (the env priming runs
/// once per isolate; a respawn gets a fresh slot and re-primes).
struct EngineSlot {
    engine: JsEngine,
    inited: tokio::sync::OnceCell<()>,
}

impl EngineSlot {
    fn new(engine: JsEngine) -> EngineSlot {
        EngineSlot {
            engine,
            inited: tokio::sync::OnceCell::new(),
        }
    }
}

impl StartEngine {
    pub fn new(
        root: PathBuf,
        cache: &Path,
        bridge: SsrBridge,
        entry_abs: PathBuf,
        no_external: oj_config::SsrExternals,
        public_dir: Option<PathBuf>,
        init_env: Vec<(String, String)>,
    ) -> anyhow::Result<StartEngine> {
        let bootstrap = cache.join("start-bootstrap.mjs");
        std::fs::write(&bootstrap, START_BOOTSTRAP_JS)?;
        let host = StartHost::new(root.clone(), cache, bridge, no_external, public_dir);
        let engine = spawn_engine(&root, &host)?;
        Ok(StartEngine {
            host,
            root,
            entry_id: entry_abs.to_string_lossy().into_owned(),
            bootstrap: bootstrap.to_string_lossy().into_owned(),
            init_env,
            engine: tokio::sync::RwLock::new(EngineSlot::new(engine)),
        })
    }

    async fn init(&self, engine: &JsEngine) -> Result<(), String> {
        let env: serde_json::Map<String, serde_json::Value> = self
            .init_env
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        engine
            .call(
                self.bootstrap.clone(),
                "init",
                vec![serde_json::Value::Object(env)],
            )
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }

    /// One request through the handler; concurrent calls interleave on the
    /// isolate (the engine's call scheduler).
    pub async fn handle(&self, req: StartRequest) -> Result<StartResponse, String> {
        let slot = self.engine.read().await;
        slot.inited
            .get_or_try_init(|| self.init(&slot.engine))
            .await?;
        let engine = &slot.engine;
        let entry = self.host.graph().specifier_for(&self.entry_id);
        let payload = serde_json::json!({
            "method": req.method,
            "url": req.url,
            "host": req.host,
            "headers": req.headers,
            "bodyBase64": req.body.map(|b| crate::ssr_host::base64(&b)),
        });
        let value = engine
            .call(self.bootstrap.clone(), "handle", vec![entry.into(), payload])
            .await
            .map_err(|e| e.to_string())?;
        let status = value
            .get("status")
            .and_then(|s| s.as_u64())
            .unwrap_or(500) as u16;
        let pairs = |key: &str| -> Vec<(String, String)> {
            value
                .get(key)
                .and_then(|h| h.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|p| {
                            let k = p.get(0)?.as_str()?.to_string();
                            let v = p.get(1)?.as_str()?.to_string();
                            Some((k, v))
                        })
                        .collect()
                })
                .unwrap_or_default()
        };
        let set_cookies = value
            .get("setCookies")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let body = value
            .get("bodyBase64")
            .and_then(|b| b.as_str())
            .map(base64_decode)
            .unwrap_or_default();
        Ok(StartResponse {
            status,
            headers: pairs("headers"),
            set_cookies,
            body,
        })
    }

    /// The watcher's reload: drop changed modules (and their importers) and
    /// force the entry fresh, so the next request re-imports the new graph.
    pub async fn reload(&self) {
        self.host.graph().invalidate();
        self.host.graph().bump(&self.entry_id);
        self.maybe_respawn().await;
    }

    /// The boot revalidation: re-check the world; reports whether anything
    /// changed (the caller reloads the browser then).
    pub async fn revalidate(&self) -> bool {
        let dropped = self.host.graph().invalidate();
        if dropped > 0 {
            self.host.graph().bump(&self.entry_id);
        }
        self.maybe_respawn().await;
        dropped > 0
    }

    async fn maybe_respawn(&self) {
        if !self.host.graph().should_respawn() {
            return;
        }
        let mut engine = self.engine.write().await;
        if !self.host.graph().should_respawn() {
            return; // another reload already respawned
        }
        match spawn_engine(&self.root, &self.host) {
            Ok(fresh) => {
                *engine = EngineSlot::new(fresh);
                self.host.graph().reset_after_respawn();
                eprintln!("oj start: engine respawned to reclaim stale module instances");
            }
            Err(e) => eprintln!("oj start: engine respawn failed: {e}"),
        }
    }
}

/// The engine the Start one-shot scripts (route-tree generation, the
/// server-fn resolver, the client bundle, the production build) run on,
/// replacing per-run `node` spawns. No module host: the scripts import
/// rolldown's napi binding and the app's plugins with plain byonm semantics,
/// exactly as they did under node. Each script exports `run(env)`; the env
/// values that used to be spawn env travel as the argument.
pub struct ScriptEngine {
    engine: JsEngine,
    handle: tokio::runtime::Handle,
}

impl ScriptEngine {
    /// Must be created from inside the tokio runtime (the handle drives
    /// blocking callers); the engine itself is plain byonm.
    pub fn new(root: &Path) -> anyhow::Result<ScriptEngine> {
        Ok(ScriptEngine {
            engine: JsEngine::spawn(EngineConfig::new(root)).map_err(|e| anyhow::anyhow!("{e}"))?,
            handle: tokio::runtime::Handle::try_current()
                .map_err(|_| anyhow::anyhow!("ScriptEngine::new needs a tokio runtime"))?,
        })
    }

    /// Runs `export async function run(env)` of `script` to completion, from
    /// a blocking thread (the boot spawn_blocking tasks, the rebundle
    /// worker); async call sites use `run_async`.
    pub fn run(&self, script: &Path, env: &[(String, String)], what: &str) -> anyhow::Result<()> {
        // block_on panics on an async worker thread; blocking threads
        // (spawn_blocking) are fine even though they carry runtime context.
        self.handle
            .block_on(self.call(script, env))
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{what} failed: {e}"))
    }

    /// The async variant for async call sites (`oj build`).
    pub async fn run_async(
        &self,
        script: &Path,
        env: &[(String, String)],
        what: &str,
    ) -> anyhow::Result<()> {
        self.call(script, env)
            .await
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{what} failed: {e}"))
    }

    fn call(
        &self,
        script: &Path,
        env: &[(String, String)],
    ) -> impl std::future::Future<Output = Result<serde_json::Value, oj_js::EngineError>> + '_ {
        let env_obj: serde_json::Map<String, serde_json::Value> = env
            .iter()
            .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
            .collect();
        self.engine.call(
            script.to_string_lossy().into_owned(),
            "run",
            vec![serde_json::Value::Object(env_obj)],
        )
    }
}

fn spawn_engine(root: &Path, host: &Arc<StartHost>) -> Result<JsEngine, oj_js::EngineError> {
    JsEngine::spawn_with_host(
        EngineConfig::new(root),
        Arc::clone(host) as Arc<dyn ModuleHost>,
    )
}

fn base64_decode(s: &str) -> Vec<u8> {
    fn val(b: u8) -> Option<u32> {
        match b {
            b'A'..=b'Z' => Some((b - b'A') as u32),
            b'a'..=b'z' => Some((b - b'a' + 26) as u32),
            b'0'..=b'9' => Some((b - b'0' + 52) as u32),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes: Vec<u32> = s.bytes().filter_map(val).collect();
    let mut out = Vec::with_capacity(bytes.len() * 3 / 4);
    for chunk in bytes.chunks(4) {
        let mut n = 0u32;
        for (i, b) in chunk.iter().enumerate() {
            n |= b << (18 - 6 * i);
        }
        let count = match chunk.len() {
            4 => 3,
            3 => 2,
            2 => 1,
            _ => 0,
        };
        for i in 0..count {
            out.push(((n >> (16 - 8 * i)) & 0xff) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_fn_rewrite_matches_the_node_loader() {
        let code = "import { createServerFn } from \"@tanstack/react-start\";\nexport const getGreeting = createServerFn({ method: \"GET\" }).handler(async () => \"hi\");\n";
        let out = rewrite_server_fns(code, "src/server/data.ts");
        assert!(out.starts_with(
            "import { createServerRpc } from \"@tanstack/react-start/server-rpc\"; "
        ));
        let id = base64url(b"src/server/data.ts#getGreeting");
        assert!(
            out.contains(&format!(
                "export const getGreeting_createServerFn_handler = createServerRpc({{ id: \"{id}\", name: \"getGreeting\", filename: \"src/server/data.ts\" }}, (opts) => getGreeting.__executeServer(opts)); "
            )),
            "{out}"
        );
        assert!(out.contains(".handler(getGreeting_createServerFn_handler, async () => \"hi\")"));
        // Line-preserving: the rewrite adds no lines.
        assert_eq!(code.lines().count(), out.lines().count());

        // Untouched code passes through byte-identical.
        let plain = "export const x = 1;\n";
        assert_eq!(rewrite_server_fns(plain, "src/x.ts"), plain);
    }

    #[test]
    fn base64url_matches_node_buffer() {
        assert_eq!(base64url(b"src/a.ts#fn"), "c3JjL2EudHMjZm4");
        assert_eq!(base64url(&[0xff, 0xef, 0x01]), "_-8B");
    }

    #[test]
    fn base64_roundtrips() {
        for input in [b"".as_slice(), b"f", b"fo", b"foo", &[0xff, 0x00, 0x10, 0x88]] {
            assert_eq!(
                base64_decode(&crate::ssr_host::base64(input)),
                input,
                "{input:?}"
            );
        }
    }

    #[test]
    fn intent_queries_split_like_the_loader() {
        assert_eq!(
            split_intent("./logo.svg?react"),
            ("./logo.svg".into(), None, true)
        );
        assert_eq!(
            split_intent("../notes.txt?raw"),
            ("../notes.txt".into(), Some("raw"), false)
        );
        assert_eq!(
            split_intent("./hero.png?url"),
            ("./hero.png".into(), Some("url"), false)
        );
        assert_eq!(
            split_intent("./x.css?inline"),
            ("./x.css".into(), Some("inline"), false)
        );
        assert_eq!(split_intent("./x.tsx"), ("./x.tsx".into(), None, false));
        // ?react is only the svg component intent.
        assert_eq!(
            split_intent("./x.ts?react"),
            ("./x.ts?react".into(), None, false)
        );
    }

    #[test]
    fn classification_tags_assets_styles_and_sources() {
        assert_eq!(classify_id("/a/b.css", None, false), "/a/b.css?ojasset=css");
        assert_eq!(
            classify_id("/a/b.module.scss", None, false),
            "/a/b.module.scss?ojasset=css"
        );
        assert_eq!(
            classify_id("/a/hero.png", None, false),
            "/a/hero.png?ojasset=url"
        );
        assert_eq!(
            classify_id("/a/n.txt", Some("raw"), false),
            "/a/n.txt?ojasset=raw"
        );
        assert_eq!(classify_id("/a/l.svg", None, true), "/a/l.svg?react");
        assert_eq!(classify_id("/a/l.svg", None, false), "/a/l.svg");
        assert_eq!(classify_id("/a/x.tsx", None, false), "/a/x.tsx");
        assert_eq!(
            classify_id("/a/m.wasm", None, false),
            "/a/m.wasm?ojasset=url"
        );
    }

    #[test]
    fn no_external_matches_names_globs_and_regexes() {
        let rules = oj_config::SsrExternals {
            no_external: vec!["left-pad".into(), "@acme/*".into()],
            no_external_regex: vec!["^weird-".into()],
            external: vec!["@acme/keep-out".into()],
            ..Default::default()
        };
        let m = NoExternal::new(rules);
        assert!(m.matches_path("/x/node_modules/left-pad/index.js"));
        assert!(m.matches_path("/x/node_modules/@acme/ui/dist/i.js"));
        assert!(m.matches_path("/x/node_modules/weird-lib/i.js"));
        assert!(!m.matches_path("/x/node_modules/@acme/keep-out/i.js"));
        assert!(!m.matches_path("/x/node_modules/react/index.js"));
        assert!(!m.matches_path("/x/src/app.tsx"));
    }

    #[test]
    fn cjs_detection_matches_the_loader() {
        let dir = std::env::temp_dir().join(format!("oj-cjsdet-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("esm")).unwrap();
        std::fs::write(dir.join("package.json"), "{}").unwrap();
        std::fs::write(dir.join("esm/package.json"), r#"{"type":"module"}"#).unwrap();
        let cjs = dir.join("plain.js");
        std::fs::write(&cjs, "module.exports = { a: 1 };\n").unwrap();
        let esmish = dir.join("esmish.js");
        std::fs::write(&esmish, "export const a = 1;\n").unwrap();
        let typed = dir.join("esm/x.js");
        std::fs::write(&typed, "module.exports = 1;\n").unwrap();
        assert!(is_cjs_file(&cjs.to_string_lossy()));
        assert!(!is_cjs_file(&esmish.to_string_lossy()));
        assert!(!is_cjs_file(&typed.to_string_lossy()), "type:module wins");
        assert!(is_cjs_file("/whatever/x.cjs"));
        assert!(!is_cjs_file("/whatever/x.mjs"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The framework seam, from the consumer's side: start-server-core imports
    // these by bare specifier and expects the bundler to answer; one the host
    // does not map fails every document request. (The old node loader had the
    // same table; write_start_assets covers the targets existing.)
    #[test]
    fn the_start_host_maps_every_framework_virtual_module() {
        let root = Path::new("/app");
        let cache = Path::new("/app/.oj-cache/v1/start");
        let aliases = framework_aliases(root, cache);
        for spec in [
            "tanstack-start-manifest:v",
            "tanstack-start-injected-head-scripts:v",
            "#tanstack-router-entry",
            "#tanstack-start-entry",
            "#tanstack-start-plugin-adapters",
            "#tanstack-start-server-fn-resolver",
            "@cloudflare/vite-plugin/server",
            "cloudflare:workers",
            "@tanstack/react-start/server-entry",
        ] {
            assert!(
                aliases.iter().any(|(find, _)| find == spec),
                "the start host has no alias for {spec}"
            );
        }
    }

    #[test]
    fn pkg_names_parse_scoped_and_plain() {
        assert_eq!(
            pkg_name_of_path("/a/node_modules/react/index.js").as_deref(),
            Some("react")
        );
        assert_eq!(
            pkg_name_of_path("/a/node_modules/@acme/ui/dist/x.js").as_deref(),
            Some("@acme/ui")
        );
        assert_eq!(pkg_name_of_path("/a/src/x.ts"), None);
    }

}
