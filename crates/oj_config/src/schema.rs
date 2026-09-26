// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OjConfig {
    pub root: Option<String>,
    /// A default mode named by the config file itself (Vite's `mode` option);
    /// the CLI's `--mode` wins over it.
    pub mode: Option<String>,
    pub base: Option<String>,
    /// Vite's `publicDir`: a directory (default `public`), or `false` for no
    /// public directory at all. See `public_dir`.
    pub public_dir: Option<BoolOrString>,
    pub server: Option<ServerConfig>,
    pub resolve: Option<ResolveConfig>,
    /// The RAW config file's top-level `resolve` block (the extractor's
    /// `rawResolve`). The extracted `resolve` above comes from Vite's RESOLVED
    /// config, whose top-level conditions are the client environment's
    /// (browser-bearing) list; this one is the user-authored, runtime-neutral
    /// list the Node SSR consumers may add to their Node defaults when the ssr
    /// environment is runner-backed. See `node_server_conditions`.
    pub raw_resolve: Option<ResolveConfig>,
    pub css: Option<CssConfig>,
    pub define: Option<BTreeMap<String, serde_json::Value>>,
    pub env_prefix: Option<StringOrList>,
    pub env_dir: Option<String>,
    pub build: Option<BuildConfig>,
    pub preview: Option<PreviewConfig>,
    /// Vite's `appType`: `spa` (default, html fallback to index.html), `mpa`
    /// (no SPA fallback) or `custom`.
    pub app_type: Option<String>,
    pub virtual_modules: Option<BTreeMap<String, String>>,
    pub environments: Option<BTreeMap<String, serde_json::Value>>,
    pub optimize_deps: Option<OptimizeDepsConfig>,
    /// Vite's `oxc` block (`oxc.jsx.{runtime,importSource,pragma,pragmaFrag}`);
    /// kept opaque because Vite also admits `oxc: false`. See `jsx_settings`.
    pub oxc: Option<serde_json::Value>,
    /// Vite <=7's `esbuild` block (`jsx`, `jsxImportSource`, `jsxFactory`,
    /// `jsxFragment`); opaque for the same reason. See `jsx_settings`.
    pub esbuild: Option<serde_json::Value>,
    /// Vite's `ssr` block (`noExternal`, `external`, `target`); opaque because
    /// entries may be strings, globs, RegExps (extracted as `{ regex }`) or
    /// `true`. See `ssr_externals`.
    pub ssr: Option<serde_json::Value>,
    /// Vite's `html` block (`cspNonce`).
    pub html: Option<HtmlConfig>,
    pub legacy: Option<LegacyConfig>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HtmlConfig {
    /// `html.cspNonce`: the nonce every injected/served `<script>`, `<style>`
    /// and stylesheet/preload `<link>` carries, plus a `csp-nonce` meta tag.
    pub csp_nonce: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum BoolOrString {
    Bool(bool),
    Str(String),
}

impl From<&str> for BoolOrString {
    fn from(s: &str) -> Self {
        BoolOrString::Str(s.to_string())
    }
}

impl From<String> for BoolOrString {
    fn from(s: String) -> Self {
        BoolOrString::Str(s)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrList {
    One(String),
    Many(Vec<String>),
}

impl StringOrList {
    pub fn to_vec(&self) -> Vec<String> {
        match self {
            StringOrList::One(s) => vec![s.clone()],
            StringOrList::Many(v) => v.clone(),
        }
    }
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CssConfig {
    pub preprocessor_options: Option<BTreeMap<String, PreprocessorEntry>>,
    /// Vite's `css.devSourcemap`: inline source maps on dev-served CSS.
    pub dev_sourcemap: Option<bool>,
    /// Vite's `css.modules` (postcss-modules options); see `css_modules`.
    pub modules: Option<CssModulesConfig>,
}

/// Vite's `css.modules` as it crosses the config boundary: functions arrive
/// as the `"__oj_fn__"` marker, RegExps as `{ "__oj_regex__": source }`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CssModulesConfig {
    pub locals_convention: Option<serde_json::Value>,
    pub generate_scoped_name: Option<serde_json::Value>,
    pub scope_behaviour: Option<String>,
    pub global_module_paths: Option<Vec<serde_json::Value>>,
    pub export_globals: Option<bool>,
    pub hash_prefix: Option<String>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PreprocessorEntry {
    pub additional_data: Option<String>,
    /// Every other option, forwarded verbatim: Sass `loadPaths`/`includePaths`,
    /// Less `paths`/`javascriptEnabled`/`globalVars`/`modifyVars`, Stylus
    /// `paths`/`define`, ... (Vite passes preprocessorOptions straight through).
    #[serde(flatten)]
    pub rest: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct OptimizeDepsConfig {
    pub no_discovery: Option<bool>,
    pub include: Option<Vec<String>>,
    pub exclude: Option<Vec<String>>,
    pub entries: Option<Vec<String>>,
    /// Deps whose CommonJS exports the scanner can't see statically; forces
    /// CJS->ESM interop for them.
    pub needs_interop: Option<Vec<String>>,
    /// Ignore any cached pre-bundle and rebuild from scratch.
    pub force: Option<bool>,
    /// Opaque options forwarded to the underlying bundler (esbuild on Vite <=7).
    pub esbuild_options: Option<serde_json::Value>,
    /// Opaque options forwarded to the underlying bundler (rolldown on Vite 8).
    pub rolldown_options: Option<serde_json::Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct PreviewConfig {
    pub port: Option<u16>,
    pub host: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
    /// Vite's preview options inherit from `server` when unset (resolvePreviewOptions).
    pub strict_port: Option<bool>,
    /// `true` or a path to open in the browser once the server listens.
    pub open: Option<serde_json::Value>,
    pub cors: Option<CorsConfig>,
    pub allowed_hosts: Option<AllowedHosts>,
    /// Accepted for compatibility; the preview server does not proxy yet (warned about).
    pub proxy: Option<serde_json::Value>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ServerConfig {
    pub port: Option<u16>,
    pub host: Option<String>,
    pub strict_port: Option<bool>,
    pub open: Option<bool>,
    pub cors: Option<CorsConfig>,
    pub hmr: Option<HmrConfig>,
    pub hmr_gate: Option<bool>,
    pub allowed_hosts: Option<AllowedHosts>,
    pub headers: Option<BTreeMap<String, String>>,
    pub proxy: Option<BTreeMap<String, ProxyEntry>>,
    pub fs: Option<FsConfig>,
    pub warmup: Option<WarmupConfig>,
    pub watch: Option<WatchConfig>,
}

/// Vite's `server.watch`: `ignored` globs (strings; RegExp and function forms
/// cannot cross the config bridge) whose changes never reach HMR or restart.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WatchConfig {
    pub ignored: Option<Vec<String>>,
}

/// Vite's `server.cors`: `true` reflects any origin, `false` disables CORS, an
/// object carries cors-package options; unset means Vite's default (localhost
/// origins only).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum CorsConfig {
    Toggle(bool),
    Options(CorsOptions),
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CorsOptions {
    /// `true` (any), a string, or a list of exact origins. A RegExp cannot cross
    /// the config boundary; the extractor warns and this falls back to the default.
    pub origin: Option<serde_json::Value>,
    pub methods: Option<serde_json::Value>,
    pub allowed_headers: Option<serde_json::Value>,
    pub credentials: Option<bool>,
    pub max_age: Option<u64>,
}

/// Vite's `server.allowedHosts`: `true` allows any Host header; a list adds
/// hostnames (a leading `.` allows the domain and its subdomains).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum AllowedHosts {
    All(bool),
    List(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum HmrConfig {
    Toggle(bool),
    Options(HmrOptions),
}

impl HmrConfig {
    pub fn is_disabled(&self) -> bool {
        matches!(self, HmrConfig::Toggle(false))
    }
}

/// Vite's `server.hmr` object form. `path`, `host`, `protocol`, `clientPort`
/// and `overlay` reach the served client (Vite's clientInjections); `port` is
/// accepted but the socket shares the dev server's port, so only `clientPort`
/// changes what the browser dials.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct HmrOptions {
    pub path: Option<String>,
    pub port: Option<u16>,
    pub overlay: Option<bool>,
    pub host: Option<String>,
    pub client_port: Option<u16>,
    pub protocol: Option<String>,
    pub timeout: Option<u64>,
}

/// Vite's `legacy` block: `skipWebSocketTokenCheck` lets a browser page attach
/// to the HMR socket without the per-process `?token=`.
#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct LegacyConfig {
    pub skip_web_socket_token_check: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct WarmupConfig {
    /// Client modules to transform eagerly at startup so their first request is warm.
    pub client_files: Option<Vec<String>>,
    pub ssr_files: Option<Vec<String>>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct FsConfig {
    pub allow: Option<Vec<String>>,
    pub strict: Option<bool>,
    pub deny: Option<Vec<String>>,
}

// Serialize too: an oj-config-format app's `server.proxy` is forwarded to the
// plugin host (which hosts the single proxy) in the spawn payload; only a
// FUNCTION rewrite cannot cross, and the {from,to} form here does.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum ProxyEntry {
    Target(String),
    Options(ProxyOptions),
}

impl ProxyEntry {
    pub fn target(&self) -> &str {
        match self {
            ProxyEntry::Target(t) => t,
            ProxyEntry::Options(o) => &o.target,
        }
    }
    pub fn change_origin(&self) -> bool {
        matches!(self, ProxyEntry::Options(o) if o.change_origin.unwrap_or(false))
    }
    pub fn ws(&self) -> bool {
        matches!(self, ProxyEntry::Options(o) if o.ws.unwrap_or(false))
    }
    pub fn rewrite_ws_origin(&self) -> bool {
        matches!(self, ProxyEntry::Options(o) if o.rewrite_ws_origin.unwrap_or(false))
    }
    pub fn secure(&self) -> bool {
        match self {
            ProxyEntry::Options(o) => o.secure.unwrap_or(true),
            ProxyEntry::Target(_) => true,
        }
    }
    pub fn rewrite(&self) -> Option<(&str, &str)> {
        match self {
            ProxyEntry::Options(o) => o.rewrite.as_ref().map(|r| (r.from.as_str(), r.to.as_str())),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProxyOptions {
    pub target: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub change_origin: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ws: Option<bool>,
    /// Verify the target's TLS certificate (http-proxy's `secure`, default true);
    /// `false` accepts a self-signed dev backend.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub secure: Option<bool>,
    /// Vite's `rewriteWsOrigin`: on a WebSocket upgrade, replace the browser's
    /// `Origin` with the target's origin (for servers that check it).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewrite_ws_origin: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rewrite: Option<ProxyRewrite>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxyRewrite {
    pub from: String,
    pub to: String,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResolveConfig {
    pub alias: Option<BTreeMap<String, String>>,
    pub dedupe: Option<Vec<String>>,
    pub extensions: Option<Vec<String>>,
    pub main_fields: Option<Vec<String>>,
    pub conditions: Option<Vec<String>>,
    /// Vite's `resolve.externalConditions`: what externalized SSR deps resolve
    /// with instead of the environment's conditions.
    pub external_conditions: Option<Vec<String>>,
    pub preserve_symlinks: Option<bool>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct BuildConfig {
    pub out_dir: Option<String>,
    /// `build.target`: an esbuild/oxc target string or array, or Vite's
    /// `"baseline-widely-available"` / `"modules"` names (see `build_targets`).
    pub target: Option<StringOrList>,
    /// `true`/`false`, or a minifier name (`"oxc"`, `"esbuild"`, `"terser"`),
    /// which all mean "minify" (oj minifies with oxc).
    pub minify: Option<BoolOrString>,
    /// Vite's `build.cssTarget`: browser targets for CSS lowering; defaults to
    /// `build.target` (see `build_css_targets`).
    pub css_target: Option<StringOrList>,
    /// Vite's `build.cssMinify`: `true`/`false` or a minifier name (all mean
    /// "minify"); defaults to `build.minify` (see `build_css_minify`).
    pub css_minify: Option<BoolOrString>,
    /// `true`/`false`, or `"inline"` / `"hidden"` (see `build_sourcemap`).
    pub sourcemap: Option<BoolOrString>,
    /// Vite's `build.emptyOutDir`: unset means "empty when outDir is inside root".
    pub empty_out_dir: Option<bool>,
    /// Accepted for compatibility; oj minifies with oxc, so this is ignored.
    pub terser_options: Option<serde_json::Value>,
    /// Vite's `build.modulePreload`: `false`, or `{ polyfill: bool }` (the
    /// `resolveDependencies` function cannot be carried; it is ignored).
    pub module_preload: Option<serde_json::Value>,
    pub lib: Option<LibConfig>,
    /// SSR entry: a path, or `true` to use `rollupOptions.input` (Vite).
    pub ssr: Option<BoolOrString>,
    /// Vite's `build.ssrManifest`: `true` for `.vite/ssr-manifest.json`, or a file name.
    pub ssr_manifest: Option<BoolOrString>,
    pub prerender: Option<Vec<String>>,
    pub rollup_options: Option<serde_json::Value>,
    pub rolldown_options: Option<serde_json::Value>,
    pub assets_inline_limit: Option<u64>,
    pub css_code_split: Option<bool>,
    pub copy_public_dir: Option<bool>,
    /// Vite's `build.assetsDir` (default `assets`): where hashed chunks and
    /// assets go under outDir.
    pub assets_dir: Option<String>,
    /// Vite's `build.manifest`: `true` for `.vite/manifest.json`, or a file name.
    pub manifest: Option<BoolOrString>,
    /// Vite's `build.reportCompressedSize` (default true): gzip column in the report.
    pub report_compressed_size: Option<bool>,
    /// Vite's `build.chunkSizeWarningLimit` in kB (default 500).
    pub chunk_size_warning_limit: Option<f64>,
    /// Vite's `build.write`; only `true` is supported (oj writes the bundle to disk).
    pub write: Option<bool>,
    /// Vite's `build.watch`; not supported by oj (warned about, ignored).
    pub watch: Option<serde_json::Value>,
    /// Vite's `build.license`; not supported by oj (warned about, ignored).
    pub license: Option<serde_json::Value>,
    /// Vite's `build.commonjsOptions`; rolldown handles CJS natively (warned about, ignored).
    pub commonjs_options: Option<serde_json::Value>,
}

/// Vite's `build.lib.entry`: one path, a list (each named by its file stem), or
/// `{ alias: path }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum LibEntry {
    One(String),
    Many(Vec<String>),
    Named(std::collections::BTreeMap<String, String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LibConfig {
    pub entry: LibEntry,
    pub name: Option<String>,
    pub formats: Option<Vec<String>>,
    /// Output name without extension (Vite: defaults to the package.json name
    /// for a single entry, else each entry's own name).
    pub file_name: Option<String>,
    /// Name of the emitted stylesheet without extension (Vite `build.lib.cssFileName`).
    pub css_file_name: Option<String>,
}
