// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Path, PathBuf};

mod schema;
pub use schema::*;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config parse error in {0}: {1}")]
    Parse(PathBuf, String),
    #[error("config evaluation error in {0}: {1}")]
    Eval(PathBuf, String),
    #[error("config schema error in {0}: {1}")]
    Schema(PathBuf, String),
}

const EVAL_TIME_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);
const EVAL_MEMORY_LIMIT: usize = 64 * 1024 * 1024;

const CANDIDATES: &[&str] = &[
    "oj.config.ts",
    "oj.config.mjs",
    "oj.config.js",
    "oj.config.json",
];

fn define_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// `build.sourcemap` resolved: Vite's `true` -> separate `.map` files, `"inline"`,
/// `"hidden"` (maps written, no `sourceMappingURL` comment), default off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sourcemap {
    Off,
    File,
    Inline,
    Hidden,
}

pub fn build_sourcemap(config: &OjConfig) -> Sourcemap {
    match config.build.as_ref().and_then(|b| b.sourcemap.as_ref()) {
        None | Some(BoolOrString::Bool(false)) => Sourcemap::Off,
        Some(BoolOrString::Bool(true)) => Sourcemap::File,
        Some(BoolOrString::Str(s)) => match s.as_str() {
            "inline" => Sourcemap::Inline,
            "hidden" => Sourcemap::Hidden,
            "false" => Sourcemap::Off,
            _ => Sourcemap::File,
        },
    }
}

/// `build.minify`: Vite's default is on; a minifier name (`"oxc"`, `"esbuild"`,
/// `"terser"`) selects the tool in Vite and just means "on" here.
pub fn build_minify(config: &OjConfig) -> bool {
    match config.build.as_ref().and_then(|b| b.minify.as_ref()) {
        None => true,
        Some(BoolOrString::Bool(b)) => *b,
        Some(BoolOrString::Str(s)) => s != "false",
    }
}

/// `css.modules` reduced to what oj applies (strings only: a function-valued
/// `localsConvention` / `generateScopedName` cannot cross from the config
/// and is reported by the extractor).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CssModulesSettings {
    pub locals_convention: Option<String>,
    pub generate_scoped_name: Option<String>,
    pub global_scope: bool,
    pub global_module_paths: Vec<String>,
}

pub fn css_modules(config: &OjConfig) -> CssModulesSettings {
    let Some(m) = config.css.as_ref().and_then(|c| c.modules.as_ref()) else {
        return CssModulesSettings::default();
    };
    let str_of = |v: &Option<serde_json::Value>| {
        v.as_ref()
            .and_then(|v| v.as_str())
            .filter(|s| *s != "__oj_fn__")
            .map(str::to_string)
    };
    CssModulesSettings {
        locals_convention: str_of(&m.locals_convention),
        generate_scoped_name: str_of(&m.generate_scoped_name),
        global_scope: m.scope_behaviour.as_deref() == Some("global"),
        global_module_paths: m
            .global_module_paths
            .iter()
            .flatten()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s.clone()),
                serde_json::Value::Object(o) => o.get("__oj_regex__").and_then(|r| r.as_str()).map(str::to_string),
                _ => None,
            })
            .collect(),
    }
}

/// `html.cspNonce`, when set and non-empty.
pub fn html_csp_nonce(config: &OjConfig) -> Option<String> {
    config
        .html
        .as_ref()
        .and_then(|h| h.csp_nonce.clone())
        .filter(|n| !n.is_empty())
}

/// The public directory, absolute: Vite's `publicDir` (default `<root>/public`),
/// or None when the config sets `publicDir: false`.
pub fn public_dir(config: &OjConfig, root: &Path) -> Option<PathBuf> {
    match config.public_dir.as_ref() {
        Some(BoolOrString::Bool(false)) => None,
        Some(BoolOrString::Str(s)) if !s.is_empty() => Some(root.join(s)),
        _ => Some(root.join("public")),
    }
}

/// `build.assetsDir`, normalized to a `/`-separated outDir-relative directory
/// with no surrounding slashes (Vite's default is `assets`; an empty string
/// puts hashed files at the outDir root).
pub fn build_assets_dir(config: &OjConfig) -> String {
    let raw = config
        .build
        .as_ref()
        .and_then(|b| b.assets_dir.as_deref())
        .unwrap_or("assets");
    raw.replace('\\', "/")
        .trim_start_matches("./")
        .trim_matches('/')
        .to_string()
}

/// `tail` under `build.assetsDir`, as Vite spells its default output patterns
/// (`path.posix.join(assetsDir, "[name]-[hash].js")`).
pub fn assets_dir_path(assets_dir: &str, tail: &str) -> String {
    if assets_dir.is_empty() {
        tail.to_string()
    } else {
        format!("{assets_dir}/{tail}")
    }
}

/// `build.manifest`: the manifest file name to write, if any (Vite writes none
/// by default; `true` means `.vite/manifest.json`).
pub fn build_manifest_name(config: &OjConfig) -> Option<String> {
    match config.build.as_ref().and_then(|b| b.manifest.as_ref()) {
        None | Some(BoolOrString::Bool(false)) => None,
        Some(BoolOrString::Bool(true)) => Some(".vite/manifest.json".to_string()),
        Some(BoolOrString::Str(s)) => match s.as_str() {
            "false" => None,
            "true" => Some(".vite/manifest.json".to_string()),
            _ => Some(s.clone()),
        },
    }
}

/// `build.reportCompressedSize` (Vite default true).
pub fn build_report_compressed_size(config: &OjConfig) -> bool {
    config
        .build
        .as_ref()
        .and_then(|b| b.report_compressed_size)
        .unwrap_or(true)
}

/// `build.chunkSizeWarningLimit` in kB (Vite default 500).
pub fn build_chunk_size_warning_limit(config: &OjConfig) -> f64 {
    config
        .build
        .as_ref()
        .and_then(|b| b.chunk_size_warning_limit)
        .unwrap_or(500.0)
}

/// Vite 8's `'baseline-widely-available'` default target (constants.ts).
pub const BASELINE_WIDELY_AVAILABLE: &[&str] =
    &["chrome111", "edge111", "firefox114", "safari16.4", "ios16.4"];
/// Vite's legacy `'modules'` target.
pub const MODULES_TARGET: &[&str] = &["es2020", "edge88", "firefox78", "chrome87", "safari14"];

/// `build.target` as the engine list oxc lowers to. Vite's named presets expand
/// to their browser lists; unset means Vite's default baseline (Vite lowers to
/// it by default too, oj used to emit `esnext`).
pub fn build_targets(config: &OjConfig) -> Vec<String> {
    let raw: Vec<String> = match config.build.as_ref().and_then(|b| b.target.as_ref()) {
        None => vec!["baseline-widely-available".into()],
        Some(t) => t.to_vec(),
    };
    let mut out = Vec::new();
    for t in raw {
        match t.as_str() {
            "baseline-widely-available" => {
                out.extend(BASELINE_WIDELY_AVAILABLE.iter().map(|s| s.to_string()))
            }
            "modules" => out.extend(MODULES_TARGET.iter().map(|s| s.to_string())),
            _ => out.push(t),
        }
    }
    out
}

/// `build.cssTarget` as an engine list for CSS lowering: Vite defaults it to
/// `build.target`, so unset means the same baseline JS lowers to. Named
/// presets expand as for `build_targets`.
pub fn build_css_targets(config: &OjConfig) -> Vec<String> {
    let Some(raw) = config.build.as_ref().and_then(|b| b.css_target.as_ref()) else {
        return build_targets(config);
    };
    let mut out = Vec::new();
    for t in raw.to_vec() {
        match t.as_str() {
            "baseline-widely-available" => {
                out.extend(BASELINE_WIDELY_AVAILABLE.iter().map(|s| s.to_string()))
            }
            "modules" => out.extend(MODULES_TARGET.iter().map(|s| s.to_string())),
            _ => out.push(t),
        }
    }
    out
}

/// `build.cssMinify` (Vite build.ts): unset follows `build.minify` for the
/// client and is on for a server (SSR) build; a minifier name means "on".
pub fn build_css_minify(config: &OjConfig, server: bool) -> bool {
    match config.build.as_ref().and_then(|b| b.css_minify.as_ref()) {
        None => server || build_minify(config),
        Some(BoolOrString::Bool(b)) => *b,
        Some(BoolOrString::Str(s)) => s != "false",
    }
}

/// Whether pages get `<link rel="modulepreload">` for their entry chunks' static
/// imports: on unless `build.modulePreload` is `false` (Vite's html plugin).
pub fn module_preload_links(config: &OjConfig) -> bool {
    !matches!(
        config.build.as_ref().and_then(|b| b.module_preload.as_ref()),
        Some(serde_json::Value::Bool(false))
    )
}

/// Whether page entries get Vite's modulepreload polyfill: on unless
/// `build.modulePreload` is `false` or `{ polyfill: false }`.
pub fn module_preload_polyfill(config: &OjConfig) -> bool {
    match config.build.as_ref().and_then(|b| b.module_preload.as_ref()) {
        Some(serde_json::Value::Bool(false)) => false,
        Some(serde_json::Value::Object(o)) => {
            o.get("polyfill").and_then(|v| v.as_bool()) != Some(false)
        }
        _ => true,
    }
}


/// The SSR entry `oj build` uses when none is given on the command line:
/// `build.ssr` as a path, or with `build.ssr: true` the `rollupOptions.input`
/// entry (Vite's contract).
pub fn build_ssr_entry(config: &OjConfig) -> Result<Option<String>, String> {
    match config.build.as_ref().and_then(|b| b.ssr.as_ref()) {
        None | Some(BoolOrString::Bool(false)) => Ok(None),
        Some(BoolOrString::Str(s)) => Ok(Some(s.clone())),
        Some(BoolOrString::Bool(true)) => {
            let input = rolldown_options(config).and_then(|ro| ro.get("input"));
            let entry = match input {
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                Some(serde_json::Value::Array(a)) => a.first().and_then(|v| v.as_str()).map(str::to_string),
                Some(serde_json::Value::Object(o)) => o.values().next().and_then(|v| v.as_str()).map(str::to_string),
                _ => None,
            };
            entry
                .map(Some)
                .ok_or_else(|| "build.ssr: true needs the SSR entry in build.rollupOptions.input".to_string())
        }
    }
}

/// `build.ssrManifest`: the manifest file name to write, if any.
pub fn ssr_manifest_name(config: &OjConfig) -> Option<String> {
    match config.build.as_ref().and_then(|b| b.ssr_manifest.as_ref()) {
        None | Some(BoolOrString::Bool(false)) => None,
        Some(BoolOrString::Bool(true)) => Some(".vite/ssr-manifest.json".to_string()),
        Some(BoolOrString::Str(s)) => match s.as_str() {
            "false" => None,
            "true" => Some(".vite/ssr-manifest.json".to_string()),
            _ => Some(s.clone()),
        },
    }
}

pub fn rolldown_options(config: &OjConfig) -> Option<&serde_json::Value> {
    let build = config.build.as_ref()?;
    build
        .rolldown_options
        .as_ref()
        .or(build.rollup_options.as_ref())
}

pub fn server_strict_port(config: &OjConfig) -> bool {
    config
        .server
        .as_ref()
        .and_then(|s| s.strict_port)
        .unwrap_or(false)
}

pub fn env_prefixes(config: &OjConfig) -> Vec<String> {
    config
        .env_prefix
        .as_ref()
        .map(|p| p.to_vec())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| vec!["VITE_".to_string()])
}

pub fn css_additional_data(config: &OjConfig, lang: &str) -> Option<String> {
    config
        .css
        .as_ref()
        .and_then(|c| c.preprocessor_options.as_ref())
        .and_then(|m| m.get(lang))
        .and_then(|e| e.additional_data.clone())
}

/// `css.preprocessorOptions.<lang>` minus `additionalData`, as JSON for the
/// preprocessor (Less/Stylus run in a node sidecar and take the object as-is).
pub fn css_preprocessor_json(config: &OjConfig, lang: &str) -> serde_json::Value {
    config
        .css
        .as_ref()
        .and_then(|c| c.preprocessor_options.as_ref())
        .and_then(|m| m.get(lang))
        .map(|e| serde_json::Value::Object(e.rest.iter().map(|(k, v)| (k.clone(), v.clone())).collect()))
        .unwrap_or(serde_json::Value::Null)
}

/// Sass `loadPaths` (Vite 5+) and the legacy `includePaths`, in order.
pub fn css_load_paths(config: &OjConfig, lang: &str) -> Vec<String> {
    let entry = config
        .css
        .as_ref()
        .and_then(|c| c.preprocessor_options.as_ref())
        .and_then(|m| m.get(lang));
    let mut out = Vec::new();
    if let Some(e) = entry {
        for key in ["loadPaths", "includePaths"] {
            if let Some(arr) = e.rest.get(key).and_then(|v| v.as_array()) {
                out.extend(arr.iter().filter_map(|v| v.as_str()).map(str::to_string));
            }
        }
    }
    out
}

pub fn server_fs_deny(config: &OjConfig) -> Vec<String> {
    config
        .server
        .as_ref()
        .and_then(|s| s.fs.as_ref())
        .and_then(|f| f.deny.as_ref())
        .cloned()
        .unwrap_or_default()
}

/// The JSX transform settings a config asks for, in Vite's precedence: `oxc.jsx`
/// (what `@vitejs/plugin-react` writes from `jsxRuntime`/`jsxImportSource`) first,
/// then the older `esbuild.jsx*` names (`jsx: "transform"` is the classic runtime,
/// `jsxFactory`/`jsxFragment` its pragmas). Unset fields mean oxc's defaults
/// (automatic runtime from `react`).
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JsxSettings {
    pub runtime: Option<String>,
    pub import_source: Option<String>,
    pub pragma: Option<String>,
    pub pragma_frag: Option<String>,
}

pub fn jsx_settings(config: &OjConfig) -> JsxSettings {
    fn str_of(obj: &serde_json::Map<String, serde_json::Value>, key: &str) -> Option<String> {
        obj.get(key).and_then(|v| v.as_str()).map(str::to_string)
    }
    let mut s = JsxSettings::default();
    if let Some(jsx) = config
        .oxc
        .as_ref()
        .and_then(|o| o.get("jsx"))
        .and_then(|j| j.as_object())
    {
        s.runtime = str_of(jsx, "runtime");
        s.import_source = str_of(jsx, "importSource");
        s.pragma = str_of(jsx, "pragma");
        s.pragma_frag = str_of(jsx, "pragmaFrag");
    }
    if let Some(es) = config.esbuild.as_ref().and_then(|e| e.as_object()) {
        if s.runtime.is_none() {
            s.runtime = match str_of(es, "jsx").as_deref() {
                Some("transform") => Some("classic".into()),
                Some("automatic") => Some("automatic".into()),
                _ => None,
            };
        }
        if s.import_source.is_none() {
            s.import_source = str_of(es, "jsxImportSource");
        }
        if s.pragma.is_none() {
            s.pragma = str_of(es, "jsxFactory");
        }
        if s.pragma_frag.is_none() {
            s.pragma_frag = str_of(es, "jsxFragment");
        }
    }
    s
}

/// Vite's `ssr.noExternal` / `ssr.external` / `ssr.target` as a matching rule
/// (external.ts): `external` names stay external; `noExternal` (`true`, or a
/// package name / glob / RegExp match on the import specifier or package name)
/// is bundled and transformed; everything else that resolves into
/// `node_modules` stays external.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SsrExternals {
    pub no_external_all: bool,
    /// Package names / globs (`@scope/*`) to bundle.
    pub no_external: Vec<String>,
    /// RegExp sources to bundle (from `noExternal: [/^@acme\//]`).
    pub no_external_regex: Vec<String>,
    /// `external: true` externalizes every dependency, even `noExternal` ones.
    pub external_all: bool,
    pub external: Vec<String>,
    pub target: Option<String>,
}

pub fn ssr_externals(config: &OjConfig) -> SsrExternals {
    let mut out = SsrExternals::default();
    let Some(ssr) = config.ssr.as_ref().and_then(|s| s.as_object()) else {
        return out;
    };
    fn entries(v: Option<&serde_json::Value>) -> (bool, Vec<String>, Vec<String>) {
        let mut names = Vec::new();
        let mut regexes = Vec::new();
        let items: Vec<&serde_json::Value> = match v {
            Some(serde_json::Value::Bool(true)) => return (true, names, regexes),
            Some(serde_json::Value::Array(a)) => a.iter().collect(),
            Some(other @ (serde_json::Value::String(_) | serde_json::Value::Object(_))) => vec![other],
            _ => Vec::new(),
        };
        for item in items {
            match item {
                serde_json::Value::String(s) => names.push(s.clone()),
                serde_json::Value::Object(o) => {
                    if let Some(src) = o.get("regex").and_then(|r| r.as_str()) {
                        regexes.push(src.to_string());
                    }
                }
                _ => {}
            }
        }
        (false, names, regexes)
    }
    let (all, names, regexes) = entries(ssr.get("noExternal"));
    out.no_external_all = all;
    out.no_external = names;
    out.no_external_regex = regexes;
    let (all, names, _) = entries(ssr.get("external"));
    out.external_all = all;
    out.external = names;
    out.target = ssr.get("target").and_then(|t| t.as_str()).map(str::to_string);
    out
}

/// The package name an import specifier or a `node_modules` path names
/// (`@scope/pkg` or `pkg`).
pub fn package_name_of(spec_or_path: &str) -> Option<String> {
    let rest = match spec_or_path.rfind("/node_modules/") {
        Some(i) => &spec_or_path[i + "/node_modules/".len()..],
        None => spec_or_path,
    };
    let rest = rest.split('?').next().unwrap_or(rest);
    let mut parts = rest.split('/');
    let first = parts.next().filter(|s| !s.is_empty())?;
    if first.starts_with('@') {
        let second = parts.next().filter(|s| !s.is_empty())?;
        Some(format!("{first}/{second}"))
    } else {
        Some(first.to_string())
    }
}

fn glob_matches(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == value;
    }
    // A `*` matches any run of characters (Vite's picomatch use on package
    // names is effectively this).
    let mut rest = value;
    let mut pieces = pattern.split('*').peekable();
    let first = pieces.next().unwrap_or("");
    if !rest.starts_with(first) {
        return false;
    }
    rest = &rest[first.len()..];
    let mut last_piece = "";
    while let Some(piece) = pieces.next() {
        if pieces.peek().is_none() {
            last_piece = piece;
            break;
        }
        match rest.find(piece) {
            Some(i) => rest = &rest[i + piece.len()..],
            None => return false,
        }
    }
    rest.ends_with(last_piece)
}

impl SsrExternals {
    /// The package name of a bare specifier (`@scope/pkg/sub` -> `@scope/pkg`).
    pub fn package_of(spec: &str) -> &str {
        let mut parts = spec.splitn(3, '/');
        let first = parts.next().unwrap_or(spec);
        if first.starts_with('@') {
            match parts.next() {
                Some(second) => &spec[..first.len() + 1 + second.len()],
                None => first,
            }
        } else {
            first
        }
    }

    /// The package name of a resolved `node_modules` path.
    pub fn package_of_path(path: &str) -> Option<&str> {
        let idx = path.rfind("node_modules/")?;
        Some(Self::package_of(&path[idx + "node_modules/".len()..]))
    }

    /// `ssr.target: "webworker"`: as in Vite, every dependency is bundled.
    pub fn webworker(&self) -> bool {
        self.target.as_deref() == Some("webworker")
    }

    /// Whether a dependency (by package name) stays external to an SSR bundle.
    /// `external` wins over `noExternal`; a webworker target bundles everything.
    pub fn is_external_pkg(&self, pkg: &str) -> bool {
        if self.external_all || self.external.iter().any(|e| e == pkg) {
            return true;
        }
        if self.webworker() {
            return false;
        }
        !self.is_no_external(pkg)
    }

    /// Whether `noExternal` claims this specifier / package (so it is bundled
    /// and transformed rather than left to Node).
    pub fn is_no_external(&self, spec: &str) -> bool {
        if self.no_external_all || self.webworker() {
            return true;
        }
        let pkg = package_name_of(spec);
        let candidates = [Some(spec.to_string()), pkg.clone()];
        for pat in &self.no_external {
            for c in candidates.iter().flatten() {
                if glob_matches(pat, c) || c.starts_with(&format!("{pat}/")) {
                    return true;
                }
            }
        }
        for src in &self.no_external_regex {
            if let Ok(re) = regex::Regex::new(src) {
                if candidates.iter().flatten().any(|c| re.is_match(c)) {
                    return true;
                }
            }
        }
        false
    }

    /// Vite's `createIsConfiguredAsExternal` decision for an SSR build: given
    /// the raw specifier, or a resolved path (`in_node_modules`), should the
    /// module stay external?
    pub fn is_external(&self, spec: &str, in_node_modules: bool) -> Option<bool> {
        if self.external_all {
            return Some(true);
        }
        let pkg = package_name_of(spec);
        if self
            .external
            .iter()
            .any(|e| Some(e) == pkg.as_ref() || e == spec)
        {
            return Some(true);
        }
        if self.is_no_external(spec) {
            return Some(false);
        }
        if in_node_modules {
            return Some(true);
        }
        None
    }
}

pub fn config_defines(config: &OjConfig) -> Vec<(String, String)> {
    config
        .define
        .as_ref()
        .map(|d| {
            d.iter()
                .map(|(k, v)| (k.clone(), define_value(v)))
                .collect()
        })
        .unwrap_or_default()
}

pub fn environment_build_bool(config: &OjConfig, env_name: &str, field: &str) -> Option<bool> {
    config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("build"))
        .and_then(|b| b.get(field))
        .and_then(|v| v.as_bool())
}

/// Export conditions for the dev server (Vite's `development` condition active).
pub fn resolve_conditions(config: &OjConfig, env_name: &str) -> Vec<String> {
    resolve_conditions_for(config, env_name, true)
}

/// The user's own `resolve.conditions` for an environment (its
/// `environments.<name>.resolve.conditions` first, then — for the ssr
/// environment — the `ssr.resolve` sugar, then the top-level list), verbatim,
/// or None when the config leaves the defaults in place. The ssr sugar must
/// win over the top-level list: the extractor publishes the resolved ssr
/// environment's conditions there (e.g. a Cloudflare workerd set), while the
/// resolved top-level list carries Vite's client defaults (`browser` et al),
/// which must never steer server-side resolution.
pub fn user_resolve_conditions(config: &OjConfig, env_name: &str) -> Option<Vec<String>> {
    let str_list = |c: &serde_json::Value| {
        c.as_array().map(|c| {
            c.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
    };
    config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("resolve"))
        .and_then(|r| r.get("conditions"))
        .and_then(&str_list)
        .or_else(|| {
            if env_name != "ssr" {
                return None;
            }
            config
                .ssr
                .as_ref()
                .and_then(|s| s.get("resolve"))
                .and_then(|r| r.get("conditions"))
                .and_then(&str_list)
        })
        .or_else(|| config.resolve.as_ref().and_then(|r| r.conditions.clone()))
}

/// The user's `resolve.externalConditions` for an environment (Vite: the
/// conditions externalized SSR deps resolve with, replacing — never merging —
/// the environment's `resolve.conditions`).
pub fn user_external_conditions(config: &OjConfig, env_name: &str) -> Option<Vec<String>> {
    let str_list = |c: &serde_json::Value| {
        c.as_array().map(|c| {
            c.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect::<Vec<_>>()
        })
    };
    // Most specific wins, as in Vite: the environment's list, then (for the ssr
    // environment) the `ssr.resolve` sugar, then the top-level `resolve` list.
    config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("resolve"))
        .and_then(|r| r.get("externalConditions"))
        .and_then(&str_list)
        .or_else(|| {
            if env_name != "ssr" {
                return None;
            }
            config
                .ssr
                .as_ref()
                .and_then(|s| s.get("resolve"))
                .and_then(|r| r.get("externalConditions"))
                .and_then(&str_list)
        })
        .or_else(|| config.resolve.as_ref().and_then(|r| r.external_conditions.clone()))
}

/// Export conditions for an environment, as Vite resolves them: the default set
/// is `browser`/`node`, `module`, and `development` or `production` (per `dev`),
/// plus `import` and `default`, which the resolver always matches. A user
/// `resolve.conditions` list replaces the defaults (Vite parity: no implicit
/// `module` or dev/prod) but Vite's `development|production` placeholder is
/// mapped to the active one, and `import`/`default` are always kept so a
/// dual-package `exports` map still resolves.
pub fn resolve_conditions_for(config: &OjConfig, env_name: &str, dev: bool) -> Vec<String> {
    let dev_prod = if dev { "development" } else { "production" };
    if let Some(user) = user_resolve_conditions(config, env_name) {
        let mut out: Vec<String> = Vec::new();
        for c in user {
            let c = if c == "development|production" { dev_prod.to_string() } else { c };
            if !out.contains(&c) {
                out.push(c);
            }
        }
        for always in ["import", "default"] {
            if !out.iter().any(|c| c == always) {
                out.push(always.to_string());
            }
        }
        return out;
    }
    let base = if env_name == "ssr" { "node" } else { "browser" };
    [base, "import", "module", dev_prod, "default"]
        .map(String::from)
        .to_vec()
}

/// Whether the ssr environment is "runner-backed": its modules execute in a
/// plugin-driven runtime (e.g. the Cloudflare plugin's workerd
/// DevEnvironments), not in oj's own Node SSR runner. The extractor decides it
/// the way Vite does — the raw config, or a plugin's `config` hook return
/// merged Vite-style, declares `environments.<name>.dev.createEnvironment`
/// (the same declaration plugin-host.mjs's buildEnvironments gate reads) —
/// and publishes it as `ssr.runnerBacked` (see detectSsrRunnerBacked in
/// vite-extract.mjs).
///
/// Vite-shaped rule: conditions never cross runtimes. An environment's
/// `resolve.conditions` steer resolution only for code executing in that
/// environment's own runtime, so when ssr is runner-backed its list describes
/// workerd and every Node-executing consumer takes Vite's Node server
/// semantics (`node_server_conditions`) instead.
pub fn ssr_runner_backed(config: &OjConfig) -> bool {
    config
        .ssr
        .as_ref()
        .and_then(|s| s.get("runnerBacked"))
        .and_then(|v| v.as_bool())
        == Some(true)
}

/// Conditions for a Node-executing SSR consumer (the Start loader, the
/// unbundled SSR resolver) when the ssr environment is runner-backed: Vite's
/// Node server semantics instead of the foreign runtime's list.
/// DEFAULT_SERVER_CONDITIONS (`module`, `node`, `development|production` —
/// vite 8.2.1 dist node.js) plus the user's RAW top-level `resolve.conditions`
/// (user-authored and runtime-neutral; the resolved top-level list is the
/// client environment's and never crosses), plus `import`/`default`, which the
/// resolver always matches.
pub fn node_server_conditions(config: &OjConfig, dev: bool) -> Vec<String> {
    let dev_prod = if dev { "development" } else { "production" };
    let mut out: Vec<String> = ["module", "node", dev_prod].map(String::from).to_vec();
    let user = config
        .raw_resolve
        .as_ref()
        .and_then(|r| r.conditions.clone())
        .unwrap_or_default();
    for c in user {
        let c = if c == "development|production" { dev_prod.to_string() } else { c };
        if !out.contains(&c) {
            out.push(c);
        }
    }
    for always in ["import", "default"] {
        if !out.iter().any(|c| c == always) {
            out.push(always.to_string());
        }
    }
    out
}

/// `externalConditions` for the same consumers: the user's RAW top-level
/// `resolve.externalConditions` when set (in Vite, top-level externalConditions
/// DO inherit into every environment and a user list replaces the default),
/// else Vite's DEFAULT_EXTERNAL_CONDITIONS (`node`, `module-sync`).
pub fn node_server_external_conditions(config: &OjConfig, dev: bool) -> Vec<String> {
    let dev_prod = if dev { "development" } else { "production" };
    match config.raw_resolve.as_ref().and_then(|r| r.external_conditions.clone()) {
        Some(user) => user
            .into_iter()
            .map(|c| if c == "development|production" { dev_prod.to_string() } else { c })
            .collect(),
        None => ["node", "module-sync"].map(String::from).to_vec(),
    }
}

pub fn resolve_dedupe(config: &OjConfig) -> Vec<String> {
    config
        .resolve
        .as_ref()
        .and_then(|r| r.dedupe.as_ref())
        .cloned()
        .unwrap_or_default()
}

pub fn resolve_extensions(config: &OjConfig) -> Option<Vec<String>> {
    config.resolve.as_ref().and_then(|r| r.extensions.clone())
}

pub fn resolve_main_fields(config: &OjConfig) -> Option<Vec<String>> {
    config.resolve.as_ref().and_then(|r| r.main_fields.clone())
}

pub fn resolve_preserve_symlinks(config: &OjConfig) -> bool {
    config
        .resolve
        .as_ref()
        .and_then(|r| r.preserve_symlinks)
        .unwrap_or(false)
}

pub fn optimize_deps_lists(config: &OjConfig) -> (Vec<String>, Vec<String>, Vec<String>) {
    let od = config.optimize_deps.as_ref();
    let take = |f: Option<&Vec<String>>| f.cloned().unwrap_or_default();
    (
        take(od.and_then(|o| o.include.as_ref())),
        take(od.and_then(|o| o.exclude.as_ref())),
        take(od.and_then(|o| o.entries.as_ref())),
    )
}

/// `optimizeDeps.needsInterop`: deps forced through CJS->ESM interop.
pub fn optimize_deps_needs_interop(config: &OjConfig) -> Vec<String> {
    config
        .optimize_deps
        .as_ref()
        .and_then(|o| o.needs_interop.as_ref())
        .cloned()
        .unwrap_or_default()
}

/// `optimizeDeps.force`: ignore any cached pre-bundle and rebuild.
pub fn optimize_deps_force(config: &OjConfig) -> bool {
    config
        .optimize_deps
        .as_ref()
        .and_then(|o| o.force)
        .unwrap_or(false)
}

/// `optimizeDeps.rolldownOptions` (Vite 8) falling back to `esbuildOptions`
/// (Vite <=7): opaque bundler options forwarded to oj's dep bundling.
pub fn optimize_deps_bundler_options(config: &OjConfig) -> Option<serde_json::Value> {
    let od = config.optimize_deps.as_ref()?;
    let Some(options) = od.rolldown_options.as_ref() else {
        return od.esbuild_options.clone();
    };
    // The optimizer uses esbuild. Translate the nested Rolldown transform and
    // resolver options instead of dropping them at the sidecar boundary.
    let mut out = options.as_object()?.clone();
    if let Some(transform) = options.get("transform").and_then(|v| v.as_object()) {
        for key in ["define", "target", "keepNames", "drop"] {
            if let Some(value) = transform.get(key) {
                out.insert(key.to_string(), value.clone());
            }
        }
        // rolldown's transform.jsx is often an object (the oxc shape); esbuild's
        // `jsx` is a string enum and rejects anything else, failing the whole
        // optimizer run. Only the string form can cross.
        if let Some(jsx) = transform.get("jsx").filter(|v| v.is_string()) {
            out.insert("jsx".to_string(), jsx.clone());
        }
    }
    if let Some(resolve) = options.get("resolve").and_then(|v| v.as_object()) {
        for (from, to) in [
            ("conditionNames", "conditions"),
            ("mainFields", "mainFields"),
            ("extensions", "resolveExtensions"),
            ("alias", "alias"),
        ] {
            if let Some(value) = resolve.get(from) {
                out.insert(to.to_string(), value.clone());
            }
        }
    }
    Some(serde_json::Value::Object(out))
}

/// `server.warmup.clientFiles` / `server.warmup.ssrFiles`: modules to compile
/// eagerly at startup so their first request is already warm.
pub fn server_warmup_files(config: &OjConfig) -> (Vec<String>, Vec<String>) {
    let w = config.server.as_ref().and_then(|s| s.warmup.as_ref());
    let take = |f: Option<&Vec<String>>| f.cloned().unwrap_or_default();
    (
        take(w.and_then(|w| w.client_files.as_ref())),
        take(w.and_then(|w| w.ssr_files.as_ref())),
    )
}

pub fn resolve_alias(config: &OjConfig, env_name: &str) -> Vec<(String, String)> {
    let mut merged: std::collections::BTreeMap<String, String> = config
        .resolve
        .as_ref()
        .and_then(|r| r.alias.as_ref())
        .map(|a| a.clone().into_iter().collect())
        .unwrap_or_default();
    if let Some(env_alias) = config
        .environments
        .as_ref()
        .and_then(|e| e.get(env_name))
        .and_then(|e| e.get("resolve"))
        .and_then(|r| r.get("alias"))
        .and_then(|a| a.as_object())
    {
        for (find, replacement) in env_alias {
            if let Some(s) = replacement.as_str() {
                merged.insert(find.clone(), s.to_string());
            }
        }
    }
    merged.into_iter().collect()
}

pub fn environment_defines(config: &OjConfig, env_name: &str) -> Vec<(String, String)> {
    config
        .environments
        .as_ref()
        .and_then(|envs| envs.get(env_name))
        .and_then(|env| env.get("define"))
        .and_then(|d| d.as_object())
        .map(|d| {
            d.iter()
                .map(|(k, v)| (k.clone(), define_value(v)))
                .collect()
        })
        .unwrap_or_default()
}

pub fn load(root: &Path) -> Result<OjConfig, ConfigError> {
    load_with(root, "serve", "development")
}

pub fn load_with(root: &Path, command: &str, mode: &str) -> Result<OjConfig, ConfigError> {
    let Some(path) = CANDIDATES
        .iter()
        .map(|c| root.join(c))
        .find(|p| p.is_file())
    else {
        return Ok(OjConfig::default());
    };
    let source = std::fs::read_to_string(&path)
        .map_err(|e| ConfigError::Parse(path.clone(), e.to_string()))?;

    let json = if path.extension().and_then(|e| e.to_str()) == Some("json") {
        source
    } else {
        evaluate(&path, &source, command, mode)?
    };

    let value: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| ConfigError::Schema(path.clone(), e.to_string()))?;
    match &value {
        serde_json::Value::Object(_) => {
            serde_json::from_value(value).map_err(|e| ConfigError::Schema(path, e.to_string()))
        }
        serde_json::Value::Null => Err(ConfigError::Eval(
            path,
            "no config was exported; a config file must `export default` an object".into(),
        )),
        other => Err(ConfigError::Schema(
            path,
            format!("expected an object, found {}", json_type_name(other)),
        )),
    }
}

fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "a boolean",
        serde_json::Value::Number(_) => "a number",
        serde_json::Value::String(_) => "a string",
        serde_json::Value::Array(_) => "an array",
        serde_json::Value::Object(_) => "an object",
    }
}

fn evaluate(path: &Path, source: &str, command: &str, mode: &str) -> Result<String, ConfigError> {
    let js = strip_types(path, source)?;
    let script = to_script(&js);

    let rt = rquickjs::Runtime::new()
        .map_err(|e| ConfigError::Eval(path.to_path_buf(), e.to_string()))?;
    rt.set_memory_limit(EVAL_MEMORY_LIMIT);
    let deadline = std::time::Instant::now() + EVAL_TIME_LIMIT;
    rt.set_interrupt_handler(Some(Box::new(move || std::time::Instant::now() >= deadline)));
    let ctx = rquickjs::Context::full(&rt)
        .map_err(|e| ConfigError::Eval(path.to_path_buf(), e.to_string()))?;

    ctx.with(|ctx| {
        let env_obj: String = std::env::vars()
            .map(|(k, v)| {
                format!(
                    "{}:{}",
                    serde_json::to_string(&k).unwrap(),
                    serde_json::to_string(&v).unwrap()
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let prelude = format!(
            "var defineConfig = function (x) {{ return x; }};\n\
             var process = {{ env: {{ {env_obj} }} }};\n\
             var globalThis = globalThis || this;\n"
        );
        let env_arg = format!(
            "{{ command: {}, mode: {}, isSsrBuild: false, isPreview: false }}",
            serde_json::to_string(command).unwrap(),
            serde_json::to_string(mode).unwrap()
        );
        let full = format!(
            "{prelude}{script}\n\
             var __ojC = globalThis.__ojConfig;\n\
             if (typeof __ojC === 'function') __ojC = __ojC({env_arg});\n\
             function __ojMark(o) {{\n\
               if (!o || typeof o !== 'object') return;\n\
               for (var k in o) {{\n\
                 var v = o[k];\n\
                 if (typeof v === 'function') o[k] = '__oj_fn__';\n\
                 else if (v instanceof RegExp) o[k] = {{ __oj_regex__: v.source }};\n\
                 else __ojMark(v);\n\
               }}\n\
             }}\n\
             __ojMark(__ojC && __ojC.build && (__ojC.build.rolldownOptions || __ojC.build.rollupOptions));\n\
             __ojMark(__ojC && __ojC.css && __ojC.css.modules);\n\
             JSON.stringify(__ojC ?? null)"
        );
        let result: rquickjs::Value = ctx.eval(full).map_err(|e| {
            let caught = ctx.catch();
            let mut detail = caught
                .as_exception()
                .map(|ex| ex.to_string())
                .unwrap_or_else(|| format!("{e}"));
            if std::time::Instant::now() >= deadline {
                detail = format!(
                    "evaluation exceeded the {}s limit; a config file must not \
                     block (no infinite loops, no blocking work)",
                    EVAL_TIME_LIMIT.as_secs()
                );
            }
            if detail.contains("is not defined") {
                detail.push_str(
                    "\nnote: oj.config is evaluated in a sandbox without module imports; \
                     if this file is a plugins array, put it in oj.plugins.mjs instead",
                );
            }
            ConfigError::Eval(path.to_path_buf(), detail)
        })?;
        result
            .get::<String>()
            .map_err(|e| ConfigError::Eval(path.to_path_buf(), e.to_string()))
    })
}

fn strip_types(path: &Path, source: &str) -> Result<String, ConfigError> {
    use oxc_allocator::Allocator;
    use oxc_codegen::Codegen;
    use oxc_parser::Parser;
    use oxc_semantic::SemanticBuilder;
    use oxc_span::SourceType;
    use oxc_transformer::{TransformOptions, Transformer};

    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::ts());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return Err(ConfigError::Parse(
            path.to_path_buf(),
            "syntax error".into(),
        ));
    }
    let mut program = parsed.program;
    let scoping = SemanticBuilder::new()
        .with_enum_eval(true)
        .build(&program)
        .semantic
        .into_scoping();
    let ret = Transformer::new(&allocator, path, &TransformOptions::default())
        .build_with_scoping(scoping, &mut program);
    if !ret.diagnostics.is_empty() {
        return Err(ConfigError::Parse(
            path.to_path_buf(),
            ret.diagnostics
                .iter()
                .map(|d| d.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    Ok(Codegen::new().build(&program).code)
}

fn to_script(js: &str) -> String {
    let mut out = String::with_capacity(js.len());
    for line in js.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("import ") || trimmed.starts_with("import{") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("export default ") {
            out.push_str("globalThis.__ojConfig = ");
            out.push_str(rest);
            out.push('\n');
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn external_conditions_fall_back_from_environment_to_ssr_sugar_to_top_level() {
        let from = |json: &str| -> OjConfig { serde_json::from_str(json).unwrap() };
        let env = from(
            r#"{ "environments": { "ssr": { "resolve": { "externalConditions": ["env"] } } },
                 "ssr": { "resolve": { "externalConditions": ["sugar"] } },
                 "resolve": { "externalConditions": ["top"] } }"#,
        );
        assert_eq!(super::user_external_conditions(&env, "ssr"), Some(vec!["env".to_string()]));
        let sugar = from(
            r#"{ "ssr": { "resolve": { "externalConditions": ["sugar"] } },
                 "resolve": { "externalConditions": ["top"] } }"#,
        );
        assert_eq!(super::user_external_conditions(&sugar, "ssr"), Some(vec!["sugar".to_string()]));
        // The ssr sugar names the ssr environment only.
        assert_eq!(super::user_external_conditions(&sugar, "worker"), Some(vec!["top".to_string()]));
        let top = from(r#"{ "resolve": { "externalConditions": ["top"] } }"#);
        assert_eq!(super::user_external_conditions(&top, "ssr"), Some(vec!["top".to_string()]));
        assert_eq!(super::user_external_conditions(&OjConfig::default(), "ssr"), None);
    }

    // `resolve.conditions` falls back the same way: environment, then the ssr
    // sugar, then the top-level list. The sugar carries the resolved ssr
    // environment's conditions (e.g. the Cloudflare plugin's workerd set),
    // while the resolved top-level list is Vite's client defaults (`browser`);
    // reading the top-level list for the ssr environment steered the Node SSR
    // loader into browser builds (`document is not defined`).
    #[test]
    fn resolve_conditions_fall_back_from_environment_to_ssr_sugar_to_top_level() {
        let from = |json: &str| -> OjConfig { serde_json::from_str(json).unwrap() };
        let env = from(
            r#"{ "environments": { "ssr": { "resolve": { "conditions": ["env"] } } },
                 "ssr": { "resolve": { "conditions": ["sugar"] } },
                 "resolve": { "conditions": ["top"] } }"#,
        );
        assert_eq!(super::user_resolve_conditions(&env, "ssr"), Some(vec!["env".to_string()]));
        let sugar = from(
            r#"{ "ssr": { "resolve": { "conditions": ["workerd", "worker", "module", "browser"] } },
                 "resolve": { "conditions": ["module", "browser", "development|production"] } }"#,
        );
        assert_eq!(
            super::user_resolve_conditions(&sugar, "ssr"),
            Some(vec![
                "workerd".to_string(),
                "worker".to_string(),
                "module".to_string(),
                "browser".to_string()
            ])
        );
        // The ssr sugar names the ssr environment only.
        assert_eq!(
            super::user_resolve_conditions(&sugar, "client"),
            Some(vec![
                "module".to_string(),
                "browser".to_string(),
                "development|production".to_string()
            ])
        );
        let top = from(r#"{ "resolve": { "conditions": ["top"] } }"#);
        assert_eq!(super::user_resolve_conditions(&top, "ssr"), Some(vec!["top".to_string()]));
        assert_eq!(super::user_resolve_conditions(&OjConfig::default(), "ssr"), None);
    }

    // Conditions never cross runtimes: `ssr.runnerBacked` (published by the
    // extractor from a structural signal — the raw config's
    // `environments.ssr.dev.createEnvironment` or the Cloudflare dev plugin in
    // the plugin list) tells every Node-executing consumer to take Vite's Node
    // server semantics instead of the runner environment's own list.
    #[test]
    fn ssr_runner_backed_reads_the_extractor_flag() {
        let from = |json: &str| -> OjConfig { serde_json::from_str(json).unwrap() };
        assert!(super::ssr_runner_backed(&from(r#"{ "ssr": { "runnerBacked": true } }"#)));
        assert!(!super::ssr_runner_backed(&from(r#"{ "ssr": { "runnerBacked": false } }"#)));
        assert!(!super::ssr_runner_backed(&from(r#"{ "ssr": { "noExternal": true } }"#)));
        assert!(!super::ssr_runner_backed(&OjConfig::default()));
    }

    // The exact composition the unbundled SSR path (oj_server's ssr_resolver)
    // uses on a runner-backed (workerd) config: the ssr environment's workerd
    // set (browser included) never crosses into the Node resolver — Vite's
    // DEFAULT_SERVER_CONDITIONS equivalents apply, plus the user's RAW
    // top-level extras, plus import/default.
    #[test]
    fn a_runner_backed_workerd_config_gets_node_server_conditions() {
        let list = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let config: OjConfig = serde_json::from_str(
            r#"{ "ssr": { "runnerBacked": true, "resolve": { "conditions": ["workerd", "worker", "module", "browser", "development|production"] } } }"#,
        )
        .unwrap();
        assert!(super::ssr_runner_backed(&config));
        assert_eq!(
            super::node_server_conditions(&config, true),
            list(&["module", "node", "development", "import", "default"])
        );
        assert_eq!(
            super::node_server_conditions(&config, false),
            list(&["module", "node", "production", "import", "default"])
        );
        // The default externalConditions mirror Vite's DEFAULT_EXTERNAL_CONDITIONS.
        assert_eq!(
            super::node_server_external_conditions(&config, true),
            list(&["node", "module-sync"])
        );

        // The user's RAW top-level resolve lists are the one user-authored,
        // runtime-neutral source: conditions join the Node defaults (deduped,
        // dev|prod mapped); externalConditions replace the default, as a user
        // list does in Vite.
        let with_user: OjConfig = serde_json::from_str(
            r#"{ "ssr": { "runnerBacked": true },
                 "rawResolve": { "conditions": ["custom", "module", "development|production"],
                                 "externalConditions": ["custom-ext", "development|production"] },
                 "resolve": { "conditions": ["module", "browser", "development|production"] } }"#,
        )
        .unwrap();
        assert_eq!(
            super::node_server_conditions(&with_user, true),
            list(&["module", "node", "development", "custom", "import", "default"])
        );
        assert_eq!(
            super::node_server_external_conditions(&with_user, true),
            list(&["custom-ext", "development"])
        );

        // NOT runner-backed: the environment chain passes verbatim — an
        // explicit user `browser` (happy-dom-style Node SSR) stays honored, as
        // Vite honors user conditions.
        let browser: OjConfig = serde_json::from_str(
            r#"{ "ssr": { "resolve": { "conditions": ["browser", "module"] } } }"#,
        )
        .unwrap();
        assert!(!super::ssr_runner_backed(&browser));
        assert_eq!(
            super::resolve_conditions(&browser, "ssr"),
            list(&["browser", "module", "import", "default"])
        );
    }

    /// A config may declare a TypeScript `enum`, and lowering one needs scoping
    /// built with `with_enum_eval`: without it the transform aborts the process
    /// instead of loading the config.
    #[test]
    fn a_config_that_declares_an_enum_loads() {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "oj-config-enum-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("oj.config.ts"),
            "enum Port { Dev = 5199 }\nexport default { server: { port: Port.Dev as number } };",
        )
        .unwrap();

        let config = super::load(&dir).expect("a config with an enum must load");
        assert_eq!(config.server.unwrap().port, Some(5199));
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;

    fn eval_config_in(label: &str, src: &str) -> OjConfig {
        let dir = std::env::temp_dir().join(format!("oj-cfg-{}-{label}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("oj.config.ts"), src).unwrap();
        let cfg = load(&dir).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        cfg
    }

    #[test]
    fn no_config_is_default() {
        let cfg = load(std::path::Path::new("/nonexistent-oj-root")).unwrap();
        assert!(cfg.server.is_none());
    }

    #[test]
    fn optimize_deps_and_dedupe_accessors() {
        let json = r#"{"resolve":{"dedupe":["react","react-dom"]},
            "optimizeDeps":{"include":["cjs-dep"],"exclude":["big-esm"],"entries":["src/main.tsx"]}}"#;
        let cfg: OjConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            resolve_dedupe(&cfg),
            vec!["react".to_string(), "react-dom".to_string()]
        );
        let (inc, exc, ent) = optimize_deps_lists(&cfg);
        assert_eq!(inc, vec!["cjs-dep".to_string()]);
        assert_eq!(exc, vec!["big-esm".to_string()]);
        assert_eq!(ent, vec!["src/main.tsx".to_string()]);
    }

    #[test]
    fn resolve_extensions_main_fields_and_preserve_symlinks_accessors() {
        let json = r#"{"resolve":{
            "extensions":[".vue",".ts",".js"],
            "mainFields":["main","module"],
            "preserveSymlinks":true}}"#;
        let cfg: OjConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            resolve_extensions(&cfg),
            Some(vec![
                ".vue".to_string(),
                ".ts".to_string(),
                ".js".to_string()
            ])
        );
        assert_eq!(
            resolve_main_fields(&cfg),
            Some(vec!["main".to_string(), "module".to_string()])
        );
        assert!(resolve_preserve_symlinks(&cfg));
        // Absent resolve.* leaves extensions/mainFields unset, symlinks followed.
        let empty: OjConfig = serde_json::from_str("{}").unwrap();
        assert!(resolve_extensions(&empty).is_none());
        assert!(resolve_main_fields(&empty).is_none());
        assert!(!resolve_preserve_symlinks(&empty));
    }

    #[test]
    fn env_prefix_accepts_string_or_array_and_defaults() {
        // A single string stays a one-element list (back-compat).
        let one: OjConfig = serde_json::from_str(r#"{"envPrefix":"PUBLIC_"}"#).unwrap();
        assert_eq!(env_prefixes(&one), vec!["PUBLIC_".to_string()]);
        // An array exposes every listed prefix.
        let many: OjConfig =
            serde_json::from_str(r#"{"envPrefix":["VITE_","PUBLIC_"]}"#).unwrap();
        assert_eq!(
            env_prefixes(&many),
            vec!["VITE_".to_string(), "PUBLIC_".to_string()]
        );
        // Absent (or empty) falls back to Vite's default VITE_.
        let none: OjConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(env_prefixes(&none), vec!["VITE_".to_string()]);
        let empty: OjConfig = serde_json::from_str(r#"{"envPrefix":[]}"#).unwrap();
        assert_eq!(env_prefixes(&empty), vec!["VITE_".to_string()]);
    }

    #[test]
    fn css_additional_data_accessor_reads_per_language() {
        let json = r#"{"css":{"preprocessorOptions":{
            "scss":{"additionalData":"@use 'src/vars' as *;"},
            "sass":{"additionalData":"$x: 1"}}}}"#;
        let cfg: OjConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            css_additional_data(&cfg, "scss").as_deref(),
            Some("@use 'src/vars' as *;")
        );
        assert_eq!(css_additional_data(&cfg, "sass").as_deref(), Some("$x: 1"));
        // A language without an entry, and an absent css config, are both None.
        assert!(css_additional_data(&cfg, "less").is_none());
        let empty: OjConfig = serde_json::from_str("{}").unwrap();
        assert!(css_additional_data(&empty, "scss").is_none());
    }

    #[test]
    fn server_fs_deny_accessor() {
        let json = r#"{"server":{"fs":{"deny":["secrets/**","*.key"]}}}"#;
        let cfg: OjConfig = serde_json::from_str(json).unwrap();
        assert_eq!(
            server_fs_deny(&cfg),
            vec!["secrets/**".to_string(), "*.key".to_string()]
        );
        // Absent server / fs / deny is an empty list (defaults applied elsewhere).
        let empty: OjConfig = serde_json::from_str("{}").unwrap();
        assert!(server_fs_deny(&empty).is_empty());
    }

    #[test]
    fn server_hmr_false_parses_as_disabled() {
        let off: OjConfig = serde_json::from_str(r#"{"server":{"hmr":false}}"#).unwrap();
        assert!(off.server.unwrap().hmr.unwrap().is_disabled());
        let on: OjConfig = serde_json::from_str(r#"{"server":{"hmr":true}}"#).unwrap();
        assert!(!on.server.unwrap().hmr.unwrap().is_disabled());
        let obj: OjConfig = serde_json::from_str(r#"{"server":{"hmr":{"overlay":false}}}"#).unwrap();
        assert!(!obj.server.unwrap().hmr.unwrap().is_disabled());
        let empty: OjConfig = serde_json::from_str(r#"{"server":{"port":3000}}"#).unwrap();
        assert!(empty.server.unwrap().hmr.is_none());
    }

    #[test]
    fn server_strict_port_accessor_defaults_false() {
        let on: OjConfig = serde_json::from_str(r#"{"server":{"strictPort":true}}"#).unwrap();
        assert!(server_strict_port(&on));
        // Vite's default is false (auto-increment) when unset.
        let off: OjConfig = serde_json::from_str(r#"{"server":{"port":3000}}"#).unwrap();
        assert!(!server_strict_port(&off));
        let empty: OjConfig = serde_json::from_str("{}").unwrap();
        assert!(!server_strict_port(&empty));
    }

    #[test]
    fn unknown_vite_keys_are_ignored_not_rejected() {
        // Vite never validates config-file keys; a config carrying options oj
        // doesn't model must load and keep its known fields, not hard-fail.
        let json = r#"{
            "base": "/app/",
            "resolve": { "dedupe": ["react"], "mainFields": ["module","browser"], "preserveSymlinks": true },
            "optimizeDeps": { "include": ["cjs-dep"], "esbuildOptions": { "target": "es2020" }, "needsInterop": ["x"] },
            "build": { "outDir": "out", "cssCodeSplit": false },
            "css": { "modules": {} },
            "worker": { "format": "es" },
            "logLevel": "silent"
        }"#;
        let cfg: OjConfig =
            serde_json::from_str(json).expect("unknown Vite keys must not fail config load");
        assert_eq!(cfg.base.as_deref(), Some("/app/"));
        assert_eq!(resolve_dedupe(&cfg), vec!["react".to_string()]);
        let (inc, _, _) = optimize_deps_lists(&cfg);
        assert_eq!(inc, vec!["cjs-dep".to_string()]);
    }

    #[test]
    fn optimize_deps_absent_is_empty() {
        let cfg: OjConfig = serde_json::from_str("{}").unwrap();
        assert!(resolve_dedupe(&cfg).is_empty());
        let (inc, exc, ent) = optimize_deps_lists(&cfg);
        assert!(inc.is_empty() && exc.is_empty() && ent.is_empty());
        assert!(optimize_deps_needs_interop(&cfg).is_empty());
        assert!(!optimize_deps_force(&cfg));
        assert!(optimize_deps_bundler_options(&cfg).is_none());
        assert_eq!(server_warmup_files(&cfg), (vec![], vec![]));
    }

    #[test]
    fn optimize_deps_full_surface_parses() {
        let json = r#"{
            "optimizeDeps": {
                "include": ["object-inspect", "@apollo/client"],
                "exclude": ["big-esm"],
                "entries": ["src/main.tsx"],
                "needsInterop": ["object-inspect"],
                "force": true,
                "rolldownOptions": { "define": { "X": "1" } }
            },
            "server": { "warmup": { "clientFiles": ["./src/App.tsx"], "ssrFiles": ["./src/entry-server.tsx"] } }
        }"#;
        let cfg: OjConfig = serde_json::from_str(json).unwrap();
        let (inc, exc, _) = optimize_deps_lists(&cfg);
        assert_eq!(inc, vec!["object-inspect".to_string(), "@apollo/client".to_string()]);
        assert_eq!(exc, vec!["big-esm".to_string()]);
        assert_eq!(optimize_deps_needs_interop(&cfg), vec!["object-inspect".to_string()]);
        assert!(optimize_deps_force(&cfg));
        assert!(optimize_deps_bundler_options(&cfg).unwrap().get("define").is_some());
        let (client, ssr) = server_warmup_files(&cfg);
        assert_eq!(client, vec!["./src/App.tsx".to_string()]);
        assert_eq!(ssr, vec!["./src/entry-server.tsx".to_string()]);
    }

    #[test]
    fn optimize_deps_bundler_options_prefers_rolldown_then_esbuild() {
        // esbuildOptions is honored when rolldownOptions is absent (Vite <=7 configs).
        let cfg: OjConfig = serde_json::from_str(
            r#"{"optimizeDeps":{"esbuildOptions":{"target":"es2020"}}}"#,
        )
        .unwrap();
        assert_eq!(
            optimize_deps_bundler_options(&cfg).unwrap().get("target").unwrap(),
            "es2020"
        );
    }

    #[test]
    fn rolldown_transform_jsx_crosses_only_as_a_string() {
        // esbuild's `jsx` is a string enum; the oxc object shape would fail
        // the whole optimizer run, so it must be dropped at translation.
        let obj: OjConfig = serde_json::from_str(
            r#"{"optimizeDeps":{"rolldownOptions":{"transform":{"jsx":{"runtime":"automatic"},"target":"es2020"}}}}"#,
        )
        .unwrap();
        let out = optimize_deps_bundler_options(&obj).unwrap();
        assert!(out.get("jsx").is_none(), "object jsx must not cross: {out}");
        assert_eq!(out.get("target").unwrap(), "es2020");
        let string: OjConfig = serde_json::from_str(
            r#"{"optimizeDeps":{"rolldownOptions":{"transform":{"jsx":"preserve"}}}}"#,
        )
        .unwrap();
        assert_eq!(
            optimize_deps_bundler_options(&string).unwrap().get("jsx").unwrap(),
            "preserve"
        );
    }

    #[test]
    fn evaluates_ts_config_with_types_and_define_config() {
        let cfg = eval_config_in(
            "define",
            "import { defineConfig } from \"oj\";\n\
             export default defineConfig({\n\
               server: { port: 3000, proxy: { \"/api\": \"http://localhost:8080\" } },\n\
               resolve: { alias: { \"@\": \"./src\" } as Record<string,string> },\n\
             });\n",
        );
        let server = cfg.server.unwrap();
        assert_eq!(server.port, Some(3000));
        assert_eq!(
            server.proxy.unwrap().get("/api").unwrap().target(),
            "http://localhost:8080"
        );
        assert_eq!(
            cfg.resolve.unwrap().alias.unwrap().get("@").unwrap(),
            "./src"
        );
    }

    #[test]
    fn function_config_receives_command_and_mode() {
        let src = "export default ({ command, mode }) => ({ base: command === \"build\" ? \"/prod/\" : \"/dev/\", define: { __M__: mode } });\n";
        let cfg = eval_config_in("fnform", src);
        assert_eq!(cfg.base.as_deref(), Some("/dev/"));
        let defines: std::collections::BTreeMap<_, _> = config_defines(&cfg).into_iter().collect();
        assert_eq!(defines.get("__M__").unwrap(), "development");

        let dir = std::env::temp_dir().join(format!("oj-cfg-fnbuild-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("oj.config.js"), src).unwrap();
        let cfg = load_with(&dir, "build", "production").unwrap();
        assert_eq!(cfg.base.as_deref(), Some("/prod/"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn undefined_reference_config_gives_plugins_hint() {
        let err = evaluate(
            std::path::Path::new("oj.config.mjs"),
            "export default [tailwindcss()];\n",
            "serve",
            "development",
        )
        .unwrap_err();
        let msg = format!("{err}");
        assert!(msg.contains("tailwindcss"), "{msg}");
        assert!(msg.contains("oj.plugins.mjs"), "{msg}");
    }

    #[test]
    fn defineconfig_function_form_works() {
        let cfg = eval_config_in(
            "definefn",
            "import { defineConfig } from \"oj\";\nexport default defineConfig(({ mode }) => ({ base: mode === \"development\" ? \"/dev/\" : \"/prod/\" }));\n",
        );
        assert_eq!(cfg.base.as_deref(), Some("/dev/"));
    }

    #[test]
    fn computed_values_and_process_env_work() {
        unsafe { std::env::set_var("OJ_TEST_PORT", "4321") };
        let cfg = eval_config_in(
            "computed",
            "export default { server: { port: Number(process.env.OJ_TEST_PORT), open: 1 > 0 } };\n",
        );
        let server = cfg.server.unwrap();
        assert_eq!(server.port, Some(4321));
        assert_eq!(server.open, Some(true));
    }

    #[test]
    fn default_config_resolver_fallbacks() {
        let s = |xs: &[&str]| xs.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let cfg = load(std::path::Path::new("/nonexistent-oj-root")).unwrap();
        assert!(config_defines(&cfg).is_empty());
        assert!(environment_defines(&cfg, "ssr").is_empty());
        assert!(resolve_alias(&cfg, "client").is_empty());
        assert_eq!(environment_build_bool(&cfg, "client", "minify"), None);
        assert_eq!(
            resolve_conditions(&cfg, "ssr"),
            s(&["node", "import", "module", "development", "default"])
        );
        assert_eq!(
            resolve_conditions(&cfg, "client"),
            s(&["browser", "import", "module", "development", "default"])
        );
        assert_eq!(
            resolve_conditions_for(&cfg, "client", false),
            s(&["browser", "import", "module", "production", "default"])
        );
    }

    #[test]
    fn per_environment_resolution_and_precedence() {
        let cfg = eval_config_in(
            "env-resolvers",
            "export default {\n\
               define: { __FLAG__: \"true\", __COUNT__: 3 },\n\
               resolve: { conditions: [\"custom\"], alias: { \"@\": \"/src\", \"old\": \"/legacy\" } },\n\
               environments: {\n\
                 ssr: {\n\
                   build: { minify: false },\n\
                   resolve: { conditions: [\"node-only\"], alias: { \"old\": \"/ssr-legacy\" } },\n\
                   define: { __SSR__: true },\n\
                 },\n\
               },\n\
             };\n",
        );
        let defines: std::collections::BTreeMap<_, _> = config_defines(&cfg).into_iter().collect();
        assert_eq!(defines.get("__FLAG__").unwrap(), "true");
        assert_eq!(defines.get("__COUNT__").unwrap(), "3");

        // A user list replaces the defaults (no implicit module/dev-prod, like
        // Vite) but import/default are always kept so exports maps still match.
        assert_eq!(
            resolve_conditions(&cfg, "ssr"),
            vec!["node-only".to_string(), "import".to_string(), "default".to_string()]
        );
        assert_eq!(
            resolve_conditions(&cfg, "client"),
            vec!["custom".to_string(), "import".to_string(), "default".to_string()]
        );

        assert_eq!(
            resolve_alias(&cfg, "ssr"),
            vec![
                ("@".to_string(), "/src".to_string()),
                ("old".to_string(), "/ssr-legacy".to_string())
            ]
        );
        assert_eq!(
            resolve_alias(&cfg, "client"),
            vec![
                ("@".to_string(), "/src".to_string()),
                ("old".to_string(), "/legacy".to_string())
            ]
        );

        assert_eq!(environment_build_bool(&cfg, "ssr", "minify"), Some(false));
        assert_eq!(environment_build_bool(&cfg, "ssr", "sourcemap"), None);
        let ssr_defines: std::collections::BTreeMap<_, _> =
            environment_defines(&cfg, "ssr").into_iter().collect();
        assert_eq!(ssr_defines.get("__SSR__").unwrap(), "true");
        assert!(environment_defines(&cfg, "client").is_empty());
    }

    #[test]
    fn json_config_loads_directly() {
        let dir = std::env::temp_dir().join(format!("oj-cfg-json-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("oj.config.json"),
            r#"{"appType":"mpa","base":"/app/"}"#,
        )
        .unwrap();
        let cfg = load(&dir).unwrap();
        assert_eq!(cfg.app_type.as_deref(), Some("mpa"));
        assert_eq!(cfg.base.as_deref(), Some("/app/"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod jsx_settings_tests {
    use super::*;

    #[test]
    fn oxc_jsx_wins_over_esbuild_and_esbuild_maps_its_names() {
        let mut c = OjConfig::default();
        c.esbuild = Some(serde_json::json!({ "jsx": "transform", "jsxImportSource": "preact", "jsxFactory": "h", "jsxFragment": "Fragment" }));
        let s = jsx_settings(&c);
        assert_eq!(s.runtime.as_deref(), Some("classic"));
        assert_eq!(s.import_source.as_deref(), Some("preact"));
        assert_eq!(s.pragma.as_deref(), Some("h"));
        assert_eq!(s.pragma_frag.as_deref(), Some("Fragment"));

        c.oxc = Some(serde_json::json!({ "jsx": { "runtime": "automatic", "importSource": "@emotion/react" } }));
        let s = jsx_settings(&c);
        assert_eq!(s.runtime.as_deref(), Some("automatic"));
        assert_eq!(s.import_source.as_deref(), Some("@emotion/react"));
        assert_eq!(s.pragma.as_deref(), Some("h"), "esbuild fills what oxc left unset");
    }

    #[test]
    fn oxc_false_and_missing_blocks_mean_defaults() {
        let mut c = OjConfig::default();
        assert_eq!(jsx_settings(&c), JsxSettings::default());
        c.oxc = Some(serde_json::Value::Bool(false));
        c.esbuild = Some(serde_json::Value::Bool(false));
        assert_eq!(jsx_settings(&c), JsxSettings::default());
    }

    #[test]
    fn user_conditions_map_vites_dev_prod_placeholder() {
        let mut cfg = OjConfig::default();
        cfg.resolve = Some(ResolveConfig {
            conditions: Some(vec![
                "custom".into(),
                "development|production".into(),
                "import".into(),
            ]),
            ..Default::default()
        });
        assert_eq!(
            resolve_conditions_for(&cfg, "client", true),
            ["custom", "development", "import", "default"].map(String::from).to_vec()
        );
        assert_eq!(
            resolve_conditions_for(&cfg, "client", false),
            ["custom", "production", "import", "default"].map(String::from).to_vec()
        );
    }
}

#[cfg(test)]
mod ssr_externals_tests {
    use super::*;

    fn cfg(v: serde_json::Value) -> OjConfig {
        let mut c = OjConfig::default();
        c.ssr = Some(v);
        c
    }

    #[test]
    fn no_external_names_globs_regexes_and_true() {
        let r = ssr_externals(&cfg(serde_json::json!({
            "noExternal": ["lodash-es", "@acme/*", { "regex": "^@tanstack/" }],
            "external": ["sharp"],
            "target": "node"
        })));
        assert!(r.is_no_external("lodash-es"));
        assert!(r.is_no_external("lodash-es/debounce"));
        assert!(r.is_no_external("@acme/ui"));
        assert!(r.is_no_external("/app/node_modules/@acme/ui/dist/index.js"));
        assert!(r.is_no_external("@tanstack/react-query"));
        assert!(!r.is_no_external("react"));
        assert_eq!(r.is_external("sharp", true), Some(true), "external wins");
        assert_eq!(r.is_external("/app/node_modules/sharp/lib/index.js", true), Some(true));
        assert_eq!(r.is_external("lodash-es", false), Some(false));
        assert_eq!(r.is_external("/app/node_modules/react/index.js", true), Some(true));
        assert_eq!(r.is_external("./local", false), None, "undecided until resolved");
        assert_eq!(r.target.as_deref(), Some("node"));

        let all = ssr_externals(&cfg(serde_json::json!({ "noExternal": true })));
        assert!(all.no_external_all);
        assert_eq!(all.is_external("/app/node_modules/react/index.js", true), Some(false));
        let ext_all = ssr_externals(&cfg(serde_json::json!({ "noExternal": true, "external": true })));
        assert_eq!(ext_all.is_external("react", false), Some(true));
        let none = ssr_externals(&OjConfig::default());
        assert_eq!(none.is_external("/app/node_modules/react/index.js", true), Some(true));
        assert_eq!(none.is_external("react", false), None);
    }

    #[test]
    fn package_names_from_specifiers_and_paths() {
        assert_eq!(package_name_of("react").as_deref(), Some("react"));
        assert_eq!(package_name_of("react/jsx-runtime").as_deref(), Some("react"));
        assert_eq!(package_name_of("@scope/pkg/sub?x").as_deref(), Some("@scope/pkg"));
        assert_eq!(package_name_of("/a/node_modules/x/node_modules/@s/p/i.js").as_deref(), Some("@s/p"));
        assert_eq!(package_name_of(""), None);
        assert_eq!(package_name_of("@scope"), None);
    }
}

#[cfg(test)]
mod build_option_defaults_tests {
    use super::*;

    fn cfg(json: &str) -> OjConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn sourcemap_accepts_bool_and_vite_strings() {
        assert_eq!(build_sourcemap(&cfg("{}")), Sourcemap::Off);
        assert_eq!(build_sourcemap(&cfg(r#"{"build":{"sourcemap":true}}"#)), Sourcemap::File);
        assert_eq!(build_sourcemap(&cfg(r#"{"build":{"sourcemap":false}}"#)), Sourcemap::Off);
        assert_eq!(build_sourcemap(&cfg(r#"{"build":{"sourcemap":"inline"}}"#)), Sourcemap::Inline);
        assert_eq!(build_sourcemap(&cfg(r#"{"build":{"sourcemap":"hidden"}}"#)), Sourcemap::Hidden);
    }

    #[test]
    fn minify_accepts_bool_and_minifier_names() {
        assert!(build_minify(&cfg("{}")));
        assert!(!build_minify(&cfg(r#"{"build":{"minify":false}}"#)));
        assert!(build_minify(&cfg(r#"{"build":{"minify":"terser"}}"#)));
        assert!(build_minify(&cfg(r#"{"build":{"minify":"esbuild","terserOptions":{"compress":{}}}}"#)));
    }

    #[test]
    fn target_expands_vite_presets_and_accepts_arrays() {
        assert_eq!(build_targets(&cfg("{}")), BASELINE_WIDELY_AVAILABLE);
        assert_eq!(build_targets(&cfg(r#"{"build":{"target":"es2015"}}"#)), vec!["es2015"]);
        assert_eq!(build_targets(&cfg(r#"{"build":{"target":"modules"}}"#)), MODULES_TARGET);
        assert_eq!(
            build_targets(&cfg(r#"{"build":{"target":["es2020","safari14"]}}"#)),
            vec!["es2020", "safari14"]
        );
    }

    #[test]
    fn module_preload_polyfill_defaults_on() {
        assert!(module_preload_polyfill(&cfg("{}")));
        assert!(!module_preload_polyfill(&cfg(r#"{"build":{"modulePreload":false}}"#)));
        assert!(!module_preload_polyfill(&cfg(r#"{"build":{"modulePreload":{"polyfill":false}}}"#)));
        assert!(module_preload_polyfill(&cfg(r#"{"build":{"modulePreload":{"polyfill":true}}}"#)));
        assert!(module_preload_links(&cfg("{}")));
        assert!(!module_preload_links(&cfg(r#"{"build":{"modulePreload":false}}"#)));
        assert!(module_preload_links(&cfg(r#"{"build":{"modulePreload":{"polyfill":false}}}"#)), "polyfill off still links");
    }

    #[test]
    fn empty_out_dir_parses() {
        assert_eq!(cfg(r#"{"build":{"emptyOutDir":false}}"#).build.unwrap().empty_out_dir, Some(false));
        assert_eq!(cfg("{}").build.and_then(|b| b.empty_out_dir), None);
    }
}

#[cfg(test)]
mod ssr_option_tests {
    use super::*;

    fn cfg(json: &str) -> OjConfig {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn package_names_from_specifiers_and_paths() {
        assert_eq!(SsrExternals::package_of("react"), "react");
        assert_eq!(SsrExternals::package_of("react-dom/server"), "react-dom");
        assert_eq!(SsrExternals::package_of("@scope/pkg/sub/x"), "@scope/pkg");
        assert_eq!(SsrExternals::package_of("@scope/pkg"), "@scope/pkg");
        assert_eq!(
            SsrExternals::package_of_path("/app/node_modules/.pnpm/x/node_modules/@scope/pkg/dist/i.js"),
            Some("@scope/pkg")
        );
        assert_eq!(SsrExternals::package_of_path("/app/src/x.js"), None);
    }

    #[test]
    fn ssr_externals_follow_vite_precedence() {
        let e = ssr_externals(&cfg("{}"));
        assert!(e.is_external_pkg("react"), "deps are external by default");
        let e = ssr_externals(&cfg(r#"{"ssr":{"noExternal":["ui-kit"],"external":["react"]}}"#));
        assert!(!e.is_external_pkg("ui-kit"));
        assert!(e.is_external_pkg("react"));
        assert!(e.is_external_pkg("lodash"));
        let e = ssr_externals(&cfg(r#"{"ssr":{"noExternal":true,"external":["react"]}}"#));
        assert!(!e.is_external_pkg("lodash"), "noExternal: true bundles everything");
        assert!(e.is_external_pkg("react"), "except explicit externals");
        let e = ssr_externals(&cfg(r#"{"ssr":{"noExternal":"single"}}"#));
        assert!(!e.is_external_pkg("single"));
        let e = ssr_externals(&cfg(r#"{"ssr":{"target":"webworker"}}"#));
        assert!(e.webworker() && !e.is_external_pkg("anything"));
    }

    #[test]
    fn build_ssr_true_takes_the_rollup_input() {
        assert_eq!(build_ssr_entry(&cfg("{}")), Ok(None));
        assert_eq!(build_ssr_entry(&cfg(r#"{"build":{"ssr":"src/s.ts"}}"#)), Ok(Some("src/s.ts".into())));
        assert_eq!(
            build_ssr_entry(&cfg(r#"{"build":{"ssr":true,"rollupOptions":{"input":"src/entry-server.ts"}}}"#)),
            Ok(Some("src/entry-server.ts".into()))
        );
        assert!(build_ssr_entry(&cfg(r#"{"build":{"ssr":true}}"#)).is_err());
        assert_eq!(build_ssr_entry(&cfg(r#"{"build":{"ssr":false}}"#)), Ok(None));
    }

    #[test]
    fn ssr_manifest_name_resolves() {
        assert_eq!(ssr_manifest_name(&cfg("{}")), None);
        assert_eq!(ssr_manifest_name(&cfg(r#"{"build":{"ssrManifest":true}}"#)), Some(".vite/ssr-manifest.json".into()));
        assert_eq!(ssr_manifest_name(&cfg(r#"{"build":{"ssrManifest":"m.json"}}"#)), Some("m.json".into()));
    }
}

#[cfg(test)]
mod preprocessor_options_tests {
    use super::*;

    #[test]
    fn preprocessor_options_keep_every_key_for_the_preprocessor() {
        let cfg: OjConfig = serde_json::from_str(
            r##"{"css":{"preprocessorOptions":{
                "scss":{"additionalData":"$x: 1;","loadPaths":["styles"],"includePaths":["legacy"]},
                "less":{"javascriptEnabled":true,"globalVars":{"brand":"#f00"},"paths":["less"]}}}}"##,
        )
        .unwrap();
        assert_eq!(css_additional_data(&cfg, "scss").as_deref(), Some("$x: 1;"));
        assert_eq!(css_load_paths(&cfg, "scss"), vec!["styles".to_string(), "legacy".to_string()]);
        let less = css_preprocessor_json(&cfg, "less");
        assert_eq!(less["javascriptEnabled"], true);
        assert_eq!(less["globalVars"]["brand"], "#f00");
        assert!(less.get("additionalData").is_none());
        assert!(css_preprocessor_json(&cfg, "stylus").is_null());
        assert!(css_load_paths(&cfg, "sass").is_empty());
    }
}

#[cfg(test)]
mod build_manifest_css_minify_tests {
    use super::*;

    #[test]
    fn manifest_css_minify_and_assets_dir_follow_vite_defaults() {
        let cfg = OjConfig::default();
        assert_eq!(build_manifest_name(&cfg), None, "Vite writes no manifest by default");
        assert!(build_css_minify(&cfg, false), "cssMinify defaults to minify (on)");
        assert_eq!(build_assets_dir(&cfg), "assets");
        assert!(build_report_compressed_size(&cfg));
        assert_eq!(build_chunk_size_warning_limit(&cfg), 500.0);

        let cfg: OjConfig = serde_json::from_str(
            r#"{"build":{"manifest":true,"minify":false,"assetsDir":"/static/","chunkSizeWarningLimit":1000,"reportCompressedSize":false}}"#,
        )
        .unwrap();
        assert_eq!(build_manifest_name(&cfg).as_deref(), Some(".vite/manifest.json"));
        assert!(!build_css_minify(&cfg, false), "cssMinify unset follows minify: false");
        assert_eq!(build_assets_dir(&cfg), "static");
        assert_eq!(build_chunk_size_warning_limit(&cfg), 1000.0);
        assert!(!build_report_compressed_size(&cfg));

        let cfg: OjConfig = serde_json::from_str(
            r#"{"build":{"manifest":"meta/m.json","minify":false,"cssMinify":"lightningcss","ssrManifest":"true"}}"#,
        )
        .unwrap();
        assert_eq!(build_manifest_name(&cfg).as_deref(), Some("meta/m.json"));
        assert!(build_css_minify(&cfg, false), "an explicit cssMinify is independent of minify");
        assert_eq!(ssr_manifest_name(&cfg).as_deref(), Some(".vite/ssr-manifest.json"));
    }
}

#[cfg(test)]
mod proxy_secure_tests {
    use super::*;

    #[test]
    fn proxy_secure_defaults_on_and_reads_false() {
        let cfg: OjConfig = serde_json::from_str(r#"{"server":{"proxy":{
            "/a": "https://a.test",
            "/b": { "target": "https://b.test", "ws": true },
            "/c": { "target": "https://c.test", "secure": false }
        }}}"#).unwrap();
        let proxy = cfg.server.unwrap().proxy.unwrap();
        assert!(proxy["/a"].secure());
        assert!(proxy["/b"].secure());
        assert!(!proxy["/c"].secure());
    }
}

#[cfg(test)]
mod public_dir_tests {
    use super::*;

    #[test]
    fn css_target_and_minify_default_to_the_js_settings() {
        let none: OjConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(build_css_targets(&none), build_targets(&none));
        assert!(build_css_minify(&none, false));
        let js_off: OjConfig = serde_json::from_str(r#"{"build":{"minify":false}}"#).unwrap();
        assert!(!build_css_minify(&js_off, false), "cssMinify follows build.minify");
        assert!(build_css_minify(&js_off, true), "server builds minify CSS by default");
        let explicit: OjConfig =
            serde_json::from_str(r#"{"build":{"minify":false,"cssMinify":"lightningcss","cssTarget":["chrome120","modules"]}}"#).unwrap();
        assert!(build_css_minify(&explicit, false));
        let t = build_css_targets(&explicit);
        assert_eq!(t[0], "chrome120");
        assert!(t.contains(&"safari14".to_string()), "modules preset expands: {t:?}");
        let off: OjConfig = serde_json::from_str(r#"{"build":{"cssMinify":false}}"#).unwrap();
        assert!(!build_css_minify(&off, false));
    }

    #[test]
    fn css_modules_settings_read_strings_and_regex_markers() {
        let cfg: OjConfig = serde_json::from_str(
            r#"{"css":{"modules":{
                "localsConvention":"camelCaseOnly",
                "generateScopedName":"__oj_fn__",
                "scopeBehaviour":"global",
                "globalModulePaths":[{"__oj_regex__":"global\\.css$"},"legacy"],
                "getJSON":"__oj_fn__"
            }}}"#,
        )
        .unwrap();
        let m = css_modules(&cfg);
        assert_eq!(m.locals_convention.as_deref(), Some("camelCaseOnly"));
        assert_eq!(m.generate_scoped_name, None, "function form is dropped");
        assert!(m.global_scope);
        assert_eq!(m.global_module_paths, vec!["global\\.css$".to_string(), "legacy".to_string()]);
        assert_eq!(css_modules(&serde_json::from_str("{}").unwrap()), CssModulesSettings::default());
    }

    #[test]
    fn public_dir_reads_path_default_and_false() {
        let root = Path::new("/app");
        let none: OjConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(public_dir(&none, root), Some(PathBuf::from("/app/public")));
        let custom: OjConfig = serde_json::from_str(r#"{"publicDir":"static"}"#).unwrap();
        assert_eq!(public_dir(&custom, root), Some(PathBuf::from("/app/static")));
        let off: OjConfig = serde_json::from_str(r#"{"publicDir":false}"#).unwrap();
        assert_eq!(public_dir(&off, root), None);
    }
}
