// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use std::borrow::Cow;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context};
use oj_server::plugins::PluginHost;
use rolldown::{
    BundlerBuilder, BundlerOptions, InputItem, OutputFormat, RawMinifyOptions, SourceMapType,
};
use rolldown_plugin::__inner::SharedPluginable;
use rolldown_plugin::{
    HookLoadArgs, HookLoadOutput, HookLoadReturn, HookRenderChunkArgs, HookRenderChunkOutput,
    HookRenderChunkReturn, HookResolveIdArgs, HookResolveIdOutput, HookResolveIdReturn,
    HookTransformArgs, HookTransformOutput, HookTransformOutputMap, HookTransformReturn, Plugin,
    PluginContext, SharedLoadPluginContext, SharedTransformPluginContext,
};

/// Both config loaders replace a function-valued rollup option with this marker
/// (the config is evaluated in a sandbox / a subprocess and travels as JSON, so
/// the function itself cannot reach the bundler). oj warns instead of silently
/// building without the option.
const FN_MARKER: &str = "__oj_fn__";

/// rolldown's batched diagnostics rendered the way Vite prints them (message,
/// file and frame), instead of a `Debug` dump.
macro_rules! rolldown_failure {
    ($($label:tt)*) => {
        |errs| {
            let detail = errs
                .into_vec()
                .iter()
                .map(|e| e.to_diagnostic().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::anyhow!("{}:\n{detail}", format!($($label)*))
        }
    };
}

fn warn_fn_option(key: &str, hint: &str) {
    eprintln!(
        "oj build: (!) build.rollupOptions.{key} is a function, which oj cannot run (the \
         config is serialized before the build); the option is ignored. {hint}"
    );
}

fn is_fn_marker(v: &serde_json::Value) -> bool {
    v.as_str() == Some(FN_MARKER)
}

fn ro_output(ro: Option<&serde_json::Value>) -> Option<&serde_json::Value> {
    let output = ro?.get("output")?;
    if output.is_array() {
        output.get(0)
    } else {
        Some(output)
    }
}

fn ro_output_str(ro: Option<&serde_json::Value>, key: &str) -> Option<String> {
    let v = ro_output(ro)?.get(key)?;
    if is_fn_marker(v) {
        warn_fn_option(
            &format!("output.{key}"),
            "Use a pattern string such as \"assets/[name]-[hash].js\".",
        );
        return None;
    }
    v.as_str().map(String::from)
}

fn ro_external(ro: Option<&serde_json::Value>) -> Vec<String> {
    match ro.and_then(|v| v.get("external")) {
        Some(serde_json::Value::String(s)) if s == FN_MARKER => {
            warn_fn_option("external", "List the external specifiers as strings.");
            Vec::new()
        }
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

/// The `rollupOptions.output` settings Vite spreads into rolldown's output
/// options and oj forwards for its (single) output: format, banner/footer/
/// intro/outro strings, globals and paths maps, umd/iife `name` and `extend`,
/// `inlineDynamicImports` / `codeSplitting: false`.
#[derive(Debug, Default)]
struct OutputOverrides {
    format: Option<OutputFormat>,
    banner: Option<String>,
    footer: Option<String>,
    intro: Option<String>,
    outro: Option<String>,
    globals: Option<std::collections::HashMap<String, String>>,
    paths: Option<std::collections::HashMap<String, String>>,
    name: Option<String>,
    extend: Option<bool>,
    inline_dynamic_imports: bool,
}

fn ro_output_overrides(ro: Option<&serde_json::Value>) -> OutputOverrides {
    let mut ov = OutputOverrides::default();
    if let Some(arr) = ro.and_then(|r| r.get("output")).and_then(|o| o.as_array()) {
        if arr.len() > 1 {
            eprintln!(
                "oj build: (!) build.rollupOptions.output lists {} outputs; oj builds the first one only.",
                arr.len()
            );
        }
    }
    let Some(out) = ro_output(ro) else { return ov };
    if out.get("file").is_some_and(|v| !v.is_null()) {
        eprintln!(
            "oj build: (!) build.rollupOptions.output.file is not supported (Vite rejects it too);              use build.outDir and output.entryFileNames."
        );
    }
    if out.get("sourcemap").is_some_and(|v| !v.is_null()) {
        eprintln!("oj build: (!) build.rollupOptions.output.sourcemap is ignored; use build.sourcemap instead (as in Vite).");
    }
    if out.get("plugins").is_some_and(|v| !v.is_null()) {
        eprintln!("oj build: (!) build.rollupOptions.output.plugins is not supported; output plugins are ignored.");
    }
    ov.format = match out.get("format").and_then(|v| v.as_str()) {
        None | Some("es") | Some("esm") | Some("module") => None,
        Some("cjs") | Some("commonjs") => Some(OutputFormat::Cjs),
        Some("umd") => Some(OutputFormat::Umd),
        Some("iife") => Some(OutputFormat::Iife),
        Some(other) => {
            eprintln!("oj build: (!) build.rollupOptions.output.format \"{other}\" is not supported; building es.");
            None
        }
    };
    let addon = |key: &str| -> Option<String> {
        let v = out.get(key)?;
        if is_fn_marker(v) {
            warn_fn_option(&format!("output.{key}"), "Use a string.");
            return None;
        }
        v.as_str().map(String::from)
    };
    ov.banner = addon("banner");
    ov.footer = addon("footer");
    ov.intro = addon("intro");
    ov.outro = addon("outro");
    let str_map = |key: &str| -> Option<std::collections::HashMap<String, String>> {
        let v = out.get(key)?;
        if is_fn_marker(v) {
            warn_fn_option(
                &format!("output.{key}"),
                "Use an object of specifier -> value.",
            );
            return None;
        }
        let obj = v.as_object()?;
        Some(
            obj.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect(),
        )
    };
    ov.globals = str_map("globals");
    ov.paths = str_map("paths");
    ov.name = out.get("name").and_then(|v| v.as_str()).map(String::from);
    ov.extend = out.get("extend").and_then(|v| v.as_bool());
    ov.inline_dynamic_imports = out.get("inlineDynamicImports").and_then(|v| v.as_bool())
        == Some(true)
        || out.get("codeSplitting").and_then(|v| v.as_bool()) == Some(false)
        || matches!(
            ov.format,
            Some(OutputFormat::Umd) | Some(OutputFormat::Iife)
        );
    ov
}

/// `rollupOptions.treeshake: false` turns rolldown's tree shaking off.
fn ro_treeshake(ro: Option<&serde_json::Value>) -> rolldown_common::TreeshakeOptions {
    match ro.and_then(|r| r.get("treeshake")) {
        Some(serde_json::Value::Bool(false)) => rolldown_common::TreeshakeOptions::Boolean(false),
        _ => rolldown_common::TreeshakeOptions::default(),
    }
}

fn addon_option(s: Option<String>) -> Option<rolldown_common::AddonOutputOption> {
    s.map(|s| rolldown_common::AddonOutputOption::String(Some(s)))
}

#[derive(Debug)]
struct OjCssPlugin {
    collected: Arc<Mutex<Vec<(String, String)>>>,
    root: PathBuf,
    has_postcss: bool,
    client: bool,
    inline_limit: u64,
    css_code_split: bool,
    // The user plugin host, so CSS sources pass through the plugin `transform`
    // chain (e.g. UnoCSS's directive transformer resolving `@apply`) before oj
    // preprocesses/compiles them, matching Vite where CSS is a real module.
    host: Option<Arc<PluginHost>>,
    css_transform_enabled: Arc<tokio::sync::OnceCell<bool>>,
    /// `css.preprocessorOptions` (Sass additionalData/loadPaths, Less/Stylus
    /// options), applied the same way the dev server applies them.
    css: Option<oj_config::CssConfig>,
    /// `resolve.alias`, root and public dir for specifiers inside stylesheets.
    resolve: oj_css::CssResolveConfig,
    /// Inline `<script type="module">` bodies keyed by their html-proxy id.
    html_inline: Arc<Mutex<std::collections::HashMap<String, String>>>,
    /// What a `?worker&inline` bundle inherits from the app build (Vite's
    /// `bundleWorkerEntry` builds the worker under the app's resolved config).
    /// None: a bare browser bundle (server and library builds).
    worker: Option<Arc<WorkerBundleOpts>>,
    /// Vite's resolved `build.cssMinify` (defaults to `build.minify`).
    /// Source ids of `?worker` / `?worker&url` entries emitted as chunks, so the
    /// document-only build helpers can skip chunks that only a worker loads.
    worker_entries: Arc<Mutex<std::collections::HashSet<String>>>,
}

/// The app build settings an inline worker bundle shares with its importer:
/// alias/conditions (`rolldown_resolve`), target and JSX (`transform_options`),
/// the client `define` map and minification.
#[derive(Debug)]
struct WorkerBundleOpts {
    config: oj_config::OjConfig,
    is_production: bool,
    define: Vec<(String, String)>,
    minify: bool,
}

/// What `@import`/`@use`/`url()` specifiers inside stylesheets resolve through,
/// as Vite's CSS resolvers do: the environment's `resolve.alias`, the root and
/// the public directory.
fn css_resolve_of(
    root: &Path,
    config: &oj_config::OjConfig,
    env: &str,
) -> oj_css::CssResolveConfig {
    oj_css::CssResolveConfig {
        root: root.to_path_buf(),
        public_dir: oj_config::public_dir(config, root).unwrap_or_default(),
        alias: oj_config::resolve_alias(config, env),
        targets: oj_config::build_css_targets(config),
        minify: oj_config::build_css_minify(config, env == "ssr"),
        modules: oj_server::css_modules_options(config),
    }
}

fn assets_inline_limit_of(config: &oj_config::OjConfig) -> u64 {
    config
        .build
        .as_ref()
        .and_then(|b| b.assets_inline_limit)
        .unwrap_or(4096)
}

/// Vite's `build.cssCodeSplit` defaults to true: each chunk gets its own CSS.
fn css_code_split_of(config: &oj_config::OjConfig) -> bool {
    config
        .build
        .as_ref()
        .and_then(|b| b.css_code_split)
        .unwrap_or(true)
}

fn re_escape(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '/' => out.push_str(r"[\\/]"),
            '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '^' | '$' => {
                out.push('\\');
                out.push(c);
            }
            _ => out.push(c),
        }
    }
    out
}

/// Chunking from `output.advancedChunks` (rolldown's native form; wins when
/// present) or the Rollup `output.manualChunks` object. A manualChunks value is a
/// module id: a bare package name matches that package under `node_modules`, a
/// `./`-relative or absolute path matches that source file (Rollup semantics).
/// The function forms cannot cross the JSON boundary and produce a warning.
fn manual_chunks(
    ro: Option<&serde_json::Value>,
    root: &Path,
) -> Option<rolldown_common::CodeSplittingMode> {
    let output = ro_output(ro)?;
    if let Some(adv) = output.get("advancedChunks") {
        if let Some(mode) = advanced_chunks(adv) {
            return Some(mode);
        }
    }
    let mc = output.get("manualChunks")?;
    if is_fn_marker(mc) {
        warn_fn_option(
            "output.manualChunks",
            "Use the object form ({ vendor: [\"react\"] }) or rolldown's output.advancedChunks groups.",
        );
        return None;
    }
    let map = mc.as_object()?;
    let mut groups = Vec::new();
    let mut priority = map.len() as u32;
    for (name, tokens) in map {
        let Some(arr) = tokens.as_array() else {
            continue;
        };
        let mut packages: Vec<String> = Vec::new();
        let mut paths: Vec<String> = Vec::new();
        for t in arr.iter().filter_map(|t| t.as_str()) {
            if t.starts_with("./") || t.starts_with("../") || Path::new(t).is_absolute() {
                let abs = if Path::new(t).is_absolute() {
                    PathBuf::from(t)
                } else {
                    root.join(t)
                };
                let abs = abs.canonicalize().unwrap_or(abs);
                paths.push(re_escape(&abs.to_string_lossy()));
            } else {
                packages.push(re_escape(t));
            }
        }
        let mut alts: Vec<String> = Vec::new();
        if !packages.is_empty() {
            alts.push(format!(
                r"[\\/]node_modules[\\/]({})([\\/]|$)",
                packages.join("|")
            ));
        }
        if !paths.is_empty() {
            alts.push(format!("^({})$", paths.join("|")));
        }
        if alts.is_empty() {
            continue;
        }
        let pattern = alts.join("|");
        let Ok(test) = rolldown_utils::js_regex::HybridRegex::new(&pattern) else {
            continue;
        };
        groups.push(rolldown_common::MatchGroup {
            name: rolldown_common::MatchGroupName::Static(name.clone()),
            test: Some(rolldown_common::MatchGroupTest::Regex(test)),
            priority: Some(priority),
            ..Default::default()
        });
        priority = priority.saturating_sub(1);
    }
    if groups.is_empty() {
        return None;
    }
    Some(rolldown_common::CodeSplittingMode::Advanced(
        rolldown_common::ManualCodeSplittingOptions {
            groups: Some(groups),
            ..Default::default()
        },
    ))
}

/// rolldown `output.advancedChunks` from its JSON form. A group `test` is a
/// regex source (a RegExp literal arrives as `{ __oj_regex__ }`, a string is
/// taken as a regex like rolldown's own deserializer does); function tests and
/// names are warned about and skipped.
fn advanced_chunks(adv: &serde_json::Value) -> Option<rolldown_common::CodeSplittingMode> {
    let obj = adv.as_object()?;
    let f64_of = |k: &str| obj.get(k).and_then(|v| v.as_f64());
    let u32_of = |k: &str| obj.get(k).and_then(|v| v.as_u64()).map(|n| n as u32);
    let mut groups = Vec::new();
    for g in obj
        .get("groups")
        .and_then(|g| g.as_array())
        .into_iter()
        .flatten()
    {
        let Some(g) = g.as_object() else { continue };
        let name = match g.get("name") {
            Some(serde_json::Value::String(s)) if s == FN_MARKER => {
                warn_fn_option("output.advancedChunks.groups[].name", "Use a static name.");
                continue;
            }
            Some(serde_json::Value::String(s)) => s.clone(),
            _ => continue,
        };
        let test = match g.get("test") {
            None | Some(serde_json::Value::Null) => None,
            Some(serde_json::Value::String(s)) if s == FN_MARKER => {
                warn_fn_option(
                    "output.advancedChunks.groups[].test",
                    "Use a RegExp or string.",
                );
                continue;
            }
            Some(serde_json::Value::String(s)) => Some(s.clone()),
            Some(serde_json::Value::Object(o)) => o
                .get("__oj_regex__")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            _ => None,
        };
        let test = match test {
            Some(src) => match rolldown_utils::js_regex::HybridRegex::new(&src) {
                Ok(re) => Some(rolldown_common::MatchGroupTest::Regex(re)),
                Err(_) => {
                    eprintln!("oj build: (!) advancedChunks group {name}: invalid test regex {src:?}; skipped");
                    continue;
                }
            },
            None => None,
        };
        let gf = |k: &str| g.get(k).and_then(|v| v.as_f64());
        let gu = |k: &str| g.get(k).and_then(|v| v.as_u64()).map(|n| n as u32);
        groups.push(rolldown_common::MatchGroup {
            name: rolldown_common::MatchGroupName::Static(name),
            test,
            priority: gu("priority"),
            min_size: gf("minSize"),
            max_size: gf("maxSize"),
            min_share_count: gu("minShareCount"),
            min_module_size: gf("minModuleSize"),
            max_module_size: gf("maxModuleSize"),
            include_dependencies_recursively: g
                .get("includeDependenciesRecursively")
                .and_then(|v| v.as_bool()),
            ..Default::default()
        });
    }
    if groups.is_empty() {
        return None;
    }
    Some(rolldown_common::CodeSplittingMode::Advanced(
        rolldown_common::ManualCodeSplittingOptions {
            groups: Some(groups),
            min_share_count: u32_of("minShareCount"),
            min_size: f64_of("minSize"),
            max_size: f64_of("maxSize"),
            min_module_size: f64_of("minModuleSize"),
            max_module_size: f64_of("maxModuleSize"),
            include_dependencies_recursively: obj
                .get("includeDependenciesRecursively")
                .and_then(|v| v.as_bool()),
        },
    ))
}

/// rolldown's oxc transform settings: `build.target`, and the JSX runtime /
/// importSource / pragmas from `oxc.jsx` or `esbuild.jsx*` (what plugin-react's
/// `jsxImportSource` becomes). `development` follows NODE_ENV as in Vite, so a
/// development-mode build uses `jsx-dev-runtime`.
fn transform_options(
    config: &oj_config::OjConfig,
    is_production: bool,
) -> Option<rolldown_common::BundlerTransformOptions> {
    let jsx = oj_config::jsx_settings(config);
    let classic = jsx.runtime.as_deref() == Some("classic");
    Some(rolldown_common::BundlerTransformOptions {
        target: Some(rolldown_common::Either::Right(oj_config::build_targets(
            config,
        ))),
        // `runtime` stays None unless configured: rolldown only merges the
        // tsconfig `compilerOptions.jsx`/`jsxFactory`/`jsxImportSource` into the
        // transform when no runtime is set, and a project configured that way
        // must keep building as before.
        jsx: Some(rolldown_common::Either::Right(
            rolldown_common::JsxOptions {
                runtime: jsx.runtime.clone(),
                development: Some(!is_production),
                import_source: if classic { None } else { jsx.import_source },
                pragma: if classic { jsx.pragma } else { None },
                pragma_frag: if classic { jsx.pragma_frag } else { None },
                ..Default::default()
            },
        )),
        ..Default::default()
    })
}

/// The three spellings Vite's define plugin replaces for NODE_ENV.
fn node_env_defines(node_env: &str) -> Vec<(String, String)> {
    let json = serde_json::Value::String(node_env.to_string()).to_string();
    [
        "process.env.NODE_ENV",
        "global.process.env.NODE_ENV",
        "globalThis.process.env.NODE_ENV",
    ]
    .iter()
    .map(|k| (k.to_string(), json.clone()))
    .collect()
}

/// Vite's define plugin for a browser bundle (`keepProcessEnv: false`, the
/// default for a client consumer and the SSR webworker target): `process.env`
/// itself (and its `global.`/`globalThis.` spellings) is defined to `{}`, so
/// `process.env.SOMETHING` evaluates to `undefined` instead of throwing a
/// ReferenceError, and NODE_ENV to the resolved mode. A lib build gets neither.
fn process_env_defines(node_env: &str) -> Vec<(String, String)> {
    let mut pairs: Vec<(String, String)> = [
        "process.env",
        "global.process.env",
        "globalThis.process.env",
    ]
    .iter()
    .map(|k| (k.to_string(), "{}".to_string()))
    .collect();
    pairs.extend(node_env_defines(node_env));
    pairs
}

/// Vite maps `build.minify: false` to rolldown's `'dce-only'`: no mangling or
/// whitespace removal, but dead code is still dropped (rolldown's `false` would
/// keep it).
fn rolldown_minify(on: bool) -> RawMinifyOptions {
    if on {
        RawMinifyOptions::Bool(true)
    } else {
        RawMinifyOptions::DeadCodeEliminationOnly
    }
}

/// Vite's define plugin sets `import.meta.hot` to `undefined` in every build,
/// so HMR-only code is dead and minifies away.
fn import_meta_hot_define() -> (String, String) {
    ("import.meta.hot".to_string(), "undefined".to_string())
}

fn shell_node_env() -> Option<String> {
    std::env::var("NODE_ENV").ok().filter(|v| !v.is_empty())
}

/// `.env` files are read from `envDir` (default the root) and only `envPrefix`
/// variables (default `VITE_`) are exposed, as in dev; the build used to
/// hardcode both.
fn env_dir_of(root: &Path, config: &oj_config::OjConfig) -> PathBuf {
    match config.env_dir.as_deref() {
        Some(d) => root.join(d),
        None => root.to_path_buf(),
    }
}

fn env_prefixes_of(config: &oj_config::OjConfig) -> Vec<String> {
    oj_config::env_prefixes(config)
}

fn sourcemap_type(s: oj_config::Sourcemap) -> Option<SourceMapType> {
    match s {
        oj_config::Sourcemap::Off => None,
        oj_config::Sourcemap::File => Some(SourceMapType::File),
        oj_config::Sourcemap::Inline => Some(SourceMapType::Inline),
        oj_config::Sourcemap::Hidden => Some(SourceMapType::Hidden),
    }
}

/// `environments.<env>.build.sourcemap` (boolean) overrides the top-level setting.
fn env_sourcemap(
    config: &oj_config::OjConfig,
    env: &str,
    default: oj_config::Sourcemap,
) -> Option<SourceMapType> {
    match oj_config::environment_build_bool(config, env, "sourcemap") {
        Some(true) => Some(SourceMapType::File),
        Some(false) => None,
        None => sourcemap_type(default),
    }
}

/// Vite's `emptyOutDir` rule (watch.ts `resolveEmptyOutDir`, prepareOutDir.ts):
/// unset means "empty it when it is inside the project root"; an outDir outside
/// root is left alone with a warning unless `build.emptyOutDir` / `--emptyOutDir`
/// says so. A `.git` directory inside outDir always survives.
fn canonical_or_self(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// Vite's `resolveEmptyOutDir`: an explicit setting wins; otherwise only an outDir
/// strictly inside root is emptied. An outDir equal to root is NOT inside (Vite
/// tests `startsWith(root + "/")`); treating it as inside emptied the whole
/// project, sources included. An outside outDir is left alone with a warning.
/// `warn` lets a second caller (the failed-build cleanup) reuse the decision
/// without repeating the message.
fn out_dir_emptiable(root: &Path, out_dir: &Path, empty: Option<bool>, warn: bool) -> bool {
    if let Some(b) = empty {
        return b;
    }
    let (r, o) = (canonical_or_self(root), canonical_or_self(out_dir));
    let inside = o != r && o.starts_with(&r);
    if !inside && warn && out_dir.exists() {
        eprintln!(
            "oj build: (!) outDir {} is not inside project root and will not be emptied.\n\
             Use --emptyOutDir to override.",
            out_dir.display()
        );
    }
    inside
}

/// Empty a directory the way Vite's `emptyDir(outDir, [".git"])` does: every entry
/// goes except a `.git` directory. Shared by the build-start emptying and the
/// failed-build cleanup, so both respect the same containment decision.
fn empty_dir_keep_git(out_dir: &Path) -> anyhow::Result<()> {
    if !out_dir.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(out_dir)? {
        let entry = entry?;
        if entry.file_name() == ".git" {
            continue;
        }
        let p = entry.path();
        if entry.file_type()?.is_dir() {
            fs::remove_dir_all(&p)?;
        } else {
            fs::remove_file(&p)?;
        }
    }
    Ok(())
}

fn prepare_out_dir(root: &Path, out_dir: &Path, empty: Option<bool>) -> anyhow::Result<()> {
    if out_dir_emptiable(root, out_dir, empty, true) {
        // Emptying an outDir that IS the project root deletes the sources the
        // build reads, so it can never be what anyone meant, even with an explicit
        // emptyOutDir. Refuse rather than destroy the project (Vite would proceed).
        if canonical_or_self(out_dir) == canonical_or_self(root) {
            bail!(
                "build.outDir {} is the project root; refusing to empty it. Point outDir at a subdirectory.",
                out_dir.display()
            );
        }
        empty_dir_keep_git(out_dir)?;
    }
    fs::create_dir_all(out_dir)?;
    Ok(())
}

fn is_build_asset(id: &str) -> bool {
    oj_compiler::assets::is_asset_url(id)
}

fn asset_mime(ext: &str) -> &'static str {
    oj_compiler::assets::asset_mime(ext)
}

fn b64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            T[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            T[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

fn export_default_url(url: &str) -> String {
    format!(
        "export default {};",
        serde_json::Value::String(url.to_string())
    )
}

/// Vite's `svgToDataURL` (asset.ts): an SVG with `<text`, `<foreignObject` or
/// nested quotes is base64 (any rewrite would be unsafe); otherwise the markup
/// is trimmed, inter-tag whitespace dropped, double quotes turned single, and
/// `% # < >` plus whitespace runs percent-encoded (`%20` keeps srcset valid).
/// A backslash is encoded too, so the url is safe inside a `"…"` CSS string.
fn svg_data_url(text: &str) -> String {
    if text.contains("<text") || text.contains("<foreignObject") || has_nested_quotes(text) {
        return format!("data:image/svg+xml;base64,{}", b64(text.as_bytes()));
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut pending_ws = false;
    for ch in text.trim().chars() {
        if ch.is_whitespace() {
            pending_ws = true;
            continue;
        }
        if pending_ws {
            // `>   <` collapses to `><`; any other whitespace run is one `%20`.
            if !(ch == '<' && out.ends_with("%3e")) {
                out.push_str("%20");
            }
            pending_ws = false;
        }
        match ch {
            '%' => out.push_str("%25"),
            '#' => out.push_str("%23"),
            '<' => out.push_str("%3c"),
            '>' => out.push_str("%3e"),
            '\\' => out.push_str("%5C"),
            '"' => out.push('\''),
            other => out.push(other),
        }
    }
    format!("data:image/svg+xml,{out}")
}

/// Vite's `nestedQuotesRE`: a double-quoted run containing a single quote, or
/// the reverse, which the quote swap would corrupt.
fn has_nested_quotes(text: &str) -> bool {
    for (open, inner) in [('"', '\''), ('\'', '"')] {
        let mut rest = text;
        while let Some(start) = rest.find(open) {
            let after = &rest[start + 1..];
            let Some(end) = after.find(open) else { break };
            if after[..end].contains(inner) {
                return true;
            }
            rest = &after[end + 1..];
        }
    }
    false
}

/// Emit an asset file, or inline it as a data URL when it is under
/// `assetsInlineLimit`; `no_inline` (`?no-inline`, Vite's asset plugin) always
/// emits.
fn emit_or_inline(
    ctx: &rolldown_plugin::SharedLoadPluginContext,
    root: &Path,
    file: &str,
    bytes: Vec<u8>,
    inline_limit: u64,
    no_inline: bool,
) -> anyhow::Result<String> {
    let path = std::path::Path::new(file);
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    // Vite's shouldInline: strictly below the limit; `?no-inline` always emits.
    let inline = !no_inline && (bytes.len() as u64) < inline_limit;
    if inline && ext != "svg" {
        return Ok(export_default_url(&format!(
            "data:{};base64,{}",
            asset_mime(ext),
            b64(&bytes)
        )));
    }
    if inline && ext == "svg" {
        let text = String::from_utf8_lossy(&bytes);
        return Ok(export_default_url(&svg_data_url(&text)));
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("asset")
        .to_string();
    // Vite emits the asset with its root-relative path as `originalFileName`,
    // which keys its manifest row and links it to the importing chunk.
    let original = Path::new(file)
        .strip_prefix(root)
        .ok()
        .map(|p| p.to_string_lossy().replace('\\', "/"));
    let reference = ctx
        .emit_file(
            rolldown_common::EmittedAsset {
                name: Some(name),
                original_file_name: original,
                source: rolldown_common::StrOrBytes::Bytes(bytes),
                ..Default::default()
            },
            None,
            None,
        )
        .map_err(|e| anyhow::anyhow!(e))?;
    Ok(format!(
        "export default import.meta.ROLLUP_FILE_URL_{reference};"
    ))
}

fn is_stylesheet_path(path: &str) -> bool {
    path.ends_with(".css")
        || oj_css::is_sass(path)
        || oj_server::sidecar::is_less(path)
        || oj_server::sidecar::is_stylus(path)
}

/// The build's stylesheet pipeline for one file: plugin transforms on the raw
/// source, Sass/Less/Stylus, Tailwind/PostCSS, then lightningcss (minified).
/// Shared by plain CSS imports, `?url` (emitted as a compiled `.css` asset) and
/// `?inline` (the compiled text), so every form of importing a stylesheet ships
/// the same CSS, as in Vite.
#[allow(clippy::too_many_arguments)]
async fn compile_stylesheet(
    root: &Path,
    host: &Option<Arc<oj_server::plugins::PluginHost>>,
    css_transform_enabled: &tokio::sync::OnceCell<bool>,
    has_postcss: bool,
    css: &Option<oj_config::CssConfig>,
    resolve: &oj_css::CssResolveConfig,
    path: &str,
    id: &str,
) -> anyhow::Result<oj_css::CssOutput> {
    let cfg = oj_config::OjConfig {
        css: css.clone(),
        ..Default::default()
    };
    let is_less = oj_server::sidecar::is_less(path);
    let is_stylus = oj_server::sidecar::is_stylus(path);
    let mut source =
        std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
    // Run the plugin transform chain on the raw CSS source first, so directive
    // transformers (UnoCSS `@apply`/`@unocss-include`, etc.) resolve before oj
    // preprocesses and compiles it.
    if let Some(host) = host {
        let on = *css_transform_enabled
            .get_or_init(|| async { host.has_transform().await })
            .await;
        if on {
            if let Ok((out, _, _, _)) = host.transform(&source, id, "{}").await {
                source = out;
            }
        }
    }
    if oj_css::is_sass(path) {
        let lang = if path.ends_with(".sass") {
            "sass"
        } else {
            "scss"
        };
        let data = oj_config::css_additional_data(&cfg, lang);
        let load_paths: Vec<PathBuf> = oj_config::css_load_paths(&cfg, lang)
            .into_iter()
            .map(|p| root.join(p))
            .collect();
        let dir = std::path::Path::new(path).parent();
        source = oj_css::compile_sass_opts(
            &source,
            &oj_css::SassOptions {
                load_dir: dir,
                additional_data: data.as_deref(),
                load_paths: &load_paths,
                resolve: resolve.as_ref(),
            },
        )
        .map_err(|e| anyhow::anyhow!(e))?;
    } else if is_less || is_stylus {
        let lang = if is_less { "less" } else { "stylus" };
        if let Some(data) = oj_config::css_additional_data(&cfg, lang).filter(|d| !d.is_empty()) {
            source = format!("{data}\n{source}");
        }
        let opts = oj_config::css_preprocessor_json(&cfg, lang);
        source = preprocess_via_engine(root, std::path::Path::new(path), &source, opts).await?;
    }
    // Plain `@import`s are inlined (postcss-import parity) so the concatenated
    // chunk stylesheet does not carry imports that 404 from `assets/`. It runs
    // before PostCSS, as postcss-import is the first plugin of Vite's chain, so
    // imported rules go through the user's plugins too. A Tailwind stylesheet
    // resolves its own imports in the tailwind engine (as the Tailwind Vite
    // plugin does).
    let tailwind = oj_server::sidecar::is_tailwind_css(&source);
    if !tailwind {
        source =
            oj_css::inline_imports_with(&source, std::path::Path::new(path), &resolve.as_ref())
                .map_err(|e| anyhow::anyhow!(e))?;
    }
    // PostCSS/Tailwind run on the preprocessor OUTPUT (Vite orders them the same
    // way), on the source as transformed so far rather than re-read from disk.
    if tailwind || has_postcss {
        source = expand_css_via_engine(root, std::path::Path::new(path), &source).await?;
    }
    let css_id = match std::path::Path::new(path).strip_prefix(root) {
        Ok(rel) => format!("/{}", rel.display()),
        Err(_) => path.to_string(),
    };
    oj_css::compile_css_with(&css_id, &source, &resolve.as_ref()).map_err(|e| anyhow::anyhow!(e))
}

/// Split `spec?query` into the file and a normalized oj asset query, for Vite's
/// import queries in any combination/order: `?worker`, `?worker&inline`,
/// `?worker&url`, `?sharedworker...`, and the single `?url`/`?raw`/`?inline`/
/// `?init`/`?react`. Other queries (`?v=1`, `?tsr-split=x`) are not oj's.
fn split_asset_query(spec: &str) -> Option<(String, String)> {
    let (base, query) = spec.split_once('?')?;
    let params: Vec<&str> = query.split('&').filter(|p| !p.is_empty()).collect();
    let has = |k: &str| params.contains(&k);
    let worker_kind = if has("worker") {
        Some("worker")
    } else if has("sharedworker") {
        Some("sharedworker")
    } else {
        None
    };
    if let Some(kind) = worker_kind {
        let mut q = kind.to_string();
        if has("inline") {
            q.push_str("&inline");
        } else if has("url") {
            q.push_str("&url");
        }
        return Some((base.to_string(), q));
    }
    for kind in ["url", "init", "raw", "inline", "react", "no-inline"] {
        if params.len() == 1 && has(kind) {
            return Some((base.to_string(), kind.to_string()));
        }
    }
    None
}

/// `(file, ctor, inline, url)` for a resolved worker id (`x.ts?worker&inline`).
fn worker_id_parts(id: &str) -> Option<(&str, &'static str, bool, bool)> {
    let (file, query) = id.split_once('?')?;
    let params: Vec<&str> = query.split('&').collect();
    let ctor = if params.contains(&"worker") {
        "Worker"
    } else if params.contains(&"sharedworker") {
        "SharedWorker"
    } else {
        return None;
    };
    Some((
        file,
        ctor,
        params.contains(&"inline"),
        params.contains(&"url"),
    ))
}

/// Bundle a worker entry to a single ESM string (dynamic imports inlined) for
/// `?worker&inline`, so the worker ships inside the importing chunk as in Vite.
/// The worker is bundled the way Vite's `bundleWorkerEntry` does it: under the
/// app's resolve (alias, conditions), transform (target, JSX), define and minify
/// settings, with oj's own asset/CSS/worker plugin so `?url`, stylesheets and
/// nested workers inside the worker behave as in the main bundle. `nested` is
/// the plugin for that inner bundle, built by the caller from its own fields.
async fn bundle_worker_inline(
    root: &Path,
    file: &str,
    opts: Option<&Arc<WorkerBundleOpts>>,
    nested: OjCssPlugin,
) -> anyhow::Result<String> {
    let plugins: Vec<SharedPluginable> = vec![Arc::new(nested)];
    let mut bundler = BundlerBuilder::default()
        .with_plugins(plugins)
        .with_options(BundlerOptions {
            input: Some(vec![InputItem {
                name: Some("worker".to_string()),
                import: file.to_string(),
            }]),
            cwd: Some(root.to_path_buf()),
            format: Some(OutputFormat::Esm),
            platform: Some(rolldown::Platform::Browser),
            code_splitting: Some(rolldown_common::CodeSplittingMode::Bool(false)),
            resolve: opts.and_then(|o| rolldown_resolve(root, &o.config, "client")),
            transform: opts.and_then(|o| transform_options(&o.config, o.is_production)),
            define: opts.map(|o| o.define.iter().cloned().collect()),
            minify: Some(rolldown_minify(opts.is_none_or(|o| o.minify))),
            ..Default::default()
        })
        .build()
        .map_err(rolldown_failure!("inline worker init failed"))?;
    let out = bundler
        .generate()
        .await
        .map_err(rolldown_failure!("inline worker build failed for {file}"))?;
    let mut code = String::new();
    for asset in &out.assets {
        if let rolldown_common::Output::Chunk(chunk) = asset {
            code.push_str(&chunk.code);
        }
    }
    Ok(code)
}

/// The `import.meta.glob` / variable dynamic import / `new URL(.., import.meta.url)`
/// expansions oj applies in the transform hook. `None` when the module comes out
/// byte-identical: the dynamic-import and new-URL expanders return a copy of their
/// input when nothing matched (a plain `import("./x")`, a bare `import.meta.url`),
/// and handing that copy to rolldown as `Some(code)` makes it push
/// `SourcemapChainElement::Omitted`, which empties the module's mappings when
/// `build.sourcemap` is on.
fn expand_import_meta(id: &str, code: &str) -> Option<String> {
    let has_glob = code.contains("import.meta.glob");
    // Vite's dynamicImportVarsOptions.exclude defaults to node_modules.
    let has_dynamic = code.contains("import(") && !id.contains("/node_modules/");
    let has_new_url = code.contains("import.meta.url");
    if !has_glob && !has_dynamic && !has_new_url {
        return None;
    }
    let path = Path::new(id);
    // At least one expander runs past the gate above, so plain Strings and one
    // final changed-check state the invariant (a Cow's borrowed arm here would
    // be unreachable and could mask a future edit that makes it reachable).
    let mut expanded = code.to_string();
    if has_glob {
        expanded = oj_compiler::glob::expand_source(&expanded, path);
    }
    if has_dynamic {
        expanded = oj_compiler::glob::expand_dynamic_import_vars_source(&expanded, path);
    }
    if has_new_url {
        expanded = oj_compiler::glob::expand_new_url_asset_source(&expanded, path);
    }
    if expanded != code {
        Some(expanded)
    } else {
        None
    }
}

impl Plugin for OjCssPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:build")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        rolldown_plugin::HookUsage::ResolveId
            | rolldown_plugin::HookUsage::Load
            | rolldown_plugin::HookUsage::Transform
    }

    fn resolve_id(
        &self,
        ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl std::future::Future<Output = HookResolveIdReturn> + Send {
        let is_routes = args.specifier == "virtual:oj-routes";
        let routes_id = self
            .root
            .join("oj-routes.tsx")
            .to_string_lossy()
            .into_owned();
        let asset_query = split_asset_query(args.specifier);
        let importer = args.importer.map(str::to_string);
        let html_proxy = args.specifier.contains("?html-proxy&index=").then(|| {
            let spec = args.specifier.strip_prefix("./").unwrap_or(args.specifier);
            if Path::new(spec).is_absolute() {
                spec.to_string()
            } else {
                self.root.join(spec).to_string_lossy().into_owned()
            }
        });
        let ctx = ctx.clone();
        async move {
            if is_routes {
                return Ok(Some(HookResolveIdOutput::from_id(routes_id)));
            }
            if let Some(id) = html_proxy {
                return Ok(Some(HookResolveIdOutput::from_id(id)));
            }
            if let Some((base, query)) = asset_query {
                if let Ok(Ok(resolved)) = ctx.resolve(&base, importer.as_deref(), None).await {
                    let id = format!("{}?{query}", resolved.id.as_str());
                    return Ok(Some(HookResolveIdOutput::from_id(id)));
                }
            }
            Ok(None)
        }
    }

    fn transform(
        &self,
        _ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        // Borrow, do not copy: `Pluginable::call_transform` boxes this future for
        // the lifetime of `args`, so the source is probed in place.
        let id: &str = args.id;
        let code: &str = args.code;
        async move {
            Ok(
                expand_import_meta(id, code).map(|code| rolldown_plugin::HookTransformOutput {
                    code: Some(code),
                    ..Default::default()
                }),
            )
        }
    }

    fn load(
        &self,
        ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        let id = args.id.to_string();
        let collected = Arc::clone(&self.collected);
        let root = self.root.clone();
        let routes_id = root.join("oj-routes.tsx").to_string_lossy().into_owned();
        let client = self.client;
        let inline_limit = self.inline_limit;
        let worker_entries = Arc::clone(&self.worker_entries);
        let host = self.host.clone();
        let css_transform_enabled = Arc::clone(&self.css_transform_enabled);
        let self_has_postcss = self.has_postcss;
        let css_cfg = self.css.clone();
        let css_resolve = self.resolve.clone();
        let html_inline = Arc::clone(&self.html_inline);
        let worker = self.worker.clone();
        async move {
            if id.contains("?html-proxy&index=") {
                if let Some(body) = html_inline.lock().unwrap().get(&id).cloned() {
                    return Ok(Some(rolldown_plugin::HookLoadOutput {
                        code: arcstr::ArcStr::from(body),
                        module_type: Some(rolldown_common::ModuleType::Js),
                        ..Default::default()
                    }));
                }
            }
            if let Some(file) = id.strip_suffix("?url") {
                if is_stylesheet_path(file) {
                    // `import u from './a.scss?url'`: the URL of the COMPILED
                    // stylesheet, emitted as a `.css` asset (Vite css.ts), not the
                    // raw preprocessor source.
                    let out = compile_stylesheet(
                        &root,
                        &host,
                        &css_transform_enabled,
                        self_has_postcss,
                        &css_cfg,
                        &css_resolve,
                        file,
                        &id,
                    )
                    .await?;
                    let stem = std::path::Path::new(file)
                        .file_stem()
                        .and_then(|n| n.to_str())
                        .unwrap_or("style");
                    let reference = ctx
                        .emit_file(
                            rolldown_common::EmittedAsset {
                                name: Some(format!("{stem}.css")),
                                source: rolldown_common::StrOrBytes::Str(out.css),
                                ..Default::default()
                            },
                            None,
                            None,
                        )
                        .map_err(|e| anyhow::anyhow!(e))?;
                    return Ok(Some(rolldown_plugin::HookLoadOutput {
                        code: arcstr::ArcStr::from(format!(
                            "export default import.meta.ROLLUP_FILE_URL_{reference};"
                        )),
                        module_type: Some(rolldown_common::ModuleType::Js),
                        ..Default::default()
                    }));
                }
                let bytes =
                    std::fs::read(file).map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
                let code = emit_or_inline(&ctx, &root, file, bytes, inline_limit, false)?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(code),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some(file) = id.strip_suffix("?no-inline") {
                // `?no-inline`: the asset's URL, emitted as a file whatever its size.
                let bytes =
                    std::fs::read(file).map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
                let code = emit_or_inline(&ctx, &root, file, bytes, inline_limit, true)?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(code),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            // A plain `.svg` (no query) goes through the plugin transform chain
            // (vite-plugin-svgr) so it becomes a React component in the prod build
            // too, matching dev and Vite. Run the transform on the raw svg; if a
            // plugin componentizes it, emit JSX. An svg no plugin matches leaves the
            // markup unchanged and falls through to the asset pipeline below. The
            // transform hook skips `.svg` (it already ran here) to avoid running it
            // twice on the componentized output.
            if !id.contains('?') && id.ends_with(".svg") {
                if let Some(host) = &host {
                    let svg = std::fs::read_to_string(&id)
                        .map_err(|e| anyhow::anyhow!("cannot read {id}: {e}"))?;
                    if let Ok((out, _, _, _)) = host.transform(&svg, &id, "{}").await {
                        if out != svg && !out.trim_start().starts_with('<') {
                            return Ok(Some(rolldown_plugin::HookLoadOutput {
                                code: arcstr::ArcStr::from(out),
                                module_type: Some(rolldown_common::ModuleType::Jsx),
                                ..Default::default()
                            }));
                        }
                    }
                }
            }
            if !id.contains('?') && is_build_asset(&id) {
                let bytes =
                    std::fs::read(&id).map_err(|e| anyhow::anyhow!("cannot read {id}: {e}"))?;
                let code = emit_or_inline(&ctx, &root, &id, bytes, inline_limit, false)?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(code),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some(file) = id.strip_suffix("?init") {
                let bytes =
                    std::fs::read(file).map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
                let name = std::path::Path::new(file)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("asset")
                    .to_string();
                let reference = ctx
                    .emit_file(
                        rolldown_common::EmittedAsset {
                            name: Some(name),
                            source: rolldown_common::StrOrBytes::Bytes(bytes),
                            ..Default::default()
                        },
                        None,
                        None,
                    )
                    .map_err(|e| anyhow::anyhow!(e))?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(format!(
                        "const u = import.meta.ROLLUP_FILE_URL_{reference};\nexport default (imports = {{}}) => {{ const inst = (r) => r.instance; const fb = () => fetch(u).then((r) => r.arrayBuffer()).then((b) => WebAssembly.instantiate(b, imports)).then(inst); return WebAssembly.instantiateStreaming ? WebAssembly.instantiateStreaming(fetch(u), imports).then(inst).catch(fb) : fb(); }};"
                    )),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some(file) = id.strip_suffix("?raw") {
                let text = std::fs::read_to_string(file)
                    .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(format!(
                        "export default {};",
                        serde_json::Value::String(text)
                    )),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some(file) = id.strip_suffix("?inline") {
                if is_stylesheet_path(file) {
                    // `import css from './a.css?inline'` is the compiled CSS text,
                    // as in dev and Vite, not a base64 data URI of the source.
                    let out = compile_stylesheet(
                        &root,
                        &host,
                        &css_transform_enabled,
                        self_has_postcss,
                        &css_cfg,
                        &css_resolve,
                        file,
                        &id,
                    )
                    .await?;
                    return Ok(Some(rolldown_plugin::HookLoadOutput {
                        code: arcstr::ArcStr::from(format!(
                            "export default {};",
                            serde_json::Value::String(out.css)
                        )),
                        module_type: Some(rolldown_common::ModuleType::Js),
                        ..Default::default()
                    }));
                }
                let bytes =
                    std::fs::read(file).map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
                let ext = std::path::Path::new(file)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("");
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(format!(
                        "export default \"data:{};base64,{}\";",
                        asset_mime(ext),
                        b64(&bytes)
                    )),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some(file) = id.strip_suffix("?react") {
                let svg = std::fs::read_to_string(file)
                    .map_err(|e| anyhow::anyhow!("cannot read {file}: {e}"))?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(oj_server::svgr::svg_to_component(&svg)),
                    module_type: Some(rolldown_common::ModuleType::Jsx),
                    ..Default::default()
                }));
            }
            if oj_server::sidecar::is_svelte(&id) {
                let js = svelte_via_engine(&root, std::path::Path::new(&id)).await?;
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(js),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if let Some((file, ctor, inline, url)) = worker_id_parts(&id) {
                if inline {
                    // `?worker&inline`: the worker's bundled code travels inside
                    // this chunk and starts from a Blob URL (data: URL fallback).
                    let nested = OjCssPlugin {
                        collected: Arc::new(Mutex::new(Vec::new())),
                        root: root.clone(),
                        has_postcss: self_has_postcss,
                        client: true,
                        inline_limit,
                        css_code_split: false,
                        host: host.clone(),
                        css_transform_enabled: Arc::clone(&css_transform_enabled),
                        css: css_cfg.clone(),
                        html_inline: Arc::new(Mutex::new(std::collections::HashMap::new())),
                        worker: worker.clone(),
                        resolve: css_resolve.clone(),
                        worker_entries: Arc::clone(&worker_entries),
                    };
                    let code = bundle_worker_inline(&root, file, worker.as_ref(), nested).await?;
                    let literal = serde_json::Value::String(code).to_string();
                    return Ok(Some(rolldown_plugin::HookLoadOutput {
                        code: arcstr::ArcStr::from(format!(
                            "const __oj_worker_code = {literal};\nexport default function (options) {{ const opts = Object.assign({{ type: \"module\" }}, options); const blob = typeof Blob !== \"undefined\" && new Blob([__oj_worker_code], {{ type: \"text/javascript;charset=utf-8\" }}); const url = blob ? URL.createObjectURL(blob) : \"data:text/javascript;charset=utf-8,\" + encodeURIComponent(__oj_worker_code); try {{ return new {ctor}(url, opts); }} finally {{ if (blob) setTimeout(() => URL.revokeObjectURL(url), 0); }} }};"
                        )),
                        module_type: Some(rolldown_common::ModuleType::Js),
                        ..Default::default()
                    }));
                }
                let stem = std::path::Path::new(file)
                    .file_stem()
                    .and_then(|n| n.to_str())
                    .unwrap_or("worker")
                    .to_string();
                worker_entries.lock().unwrap().insert(file.to_string());
                let reference = ctx
                    .emit_chunk(rolldown_common::EmittedChunk {
                        id: file.to_string(),
                        name: Some(stem.into()),
                        preserve_entry_signatures: Some(
                            rolldown_common::PreserveEntrySignatures::False,
                        ),
                        ..Default::default()
                    })
                    .map_err(|e| anyhow::anyhow!(e))?;
                let code = if url {
                    // `?worker&url` (also what `new Worker(new URL(...))` becomes):
                    // the chunk's URL as a string.
                    format!("export default import.meta.ROLLUP_FILE_URL_{reference};")
                } else {
                    format!(
                        "export default function (options) {{ return new {ctor}(import.meta.ROLLUP_FILE_URL_{reference}, Object.assign({{ type: \"module\" }}, options)); }};"
                    )
                };
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(code),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            let path = id.split('?').next().unwrap_or(&id);
            if client && is_server_module_path(path) {
                let source = std::fs::read_to_string(path)
                    .map_err(|e| anyhow::anyhow!("cannot read {path}: {e}"))?;
                let url = match std::path::Path::new(path).strip_prefix(&root) {
                    Ok(rel) => format!("/{}", rel.display()),
                    Err(_) => path.to_string(),
                };
                let stub = server_fn_prod_stub(
                    &oj_compiler::exports(&source, std::path::Path::new(path)),
                    &url,
                );
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(stub),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if path == routes_id {
                return Ok(Some(rolldown_plugin::HookLoadOutput {
                    code: arcstr::ArcStr::from(oj_server::OJ_ROUTES_JS),
                    module_type: Some(rolldown_common::ModuleType::Js),
                    ..Default::default()
                }));
            }
            if !is_stylesheet_path(path) {
                return Ok(None);
            }
            let output = compile_stylesheet(
                &root,
                &host,
                &css_transform_enabled,
                self_has_postcss,
                &css_cfg,
                &css_resolve,
                path,
                &id,
            )
            .await?;
            // A CSS module: the class map as default plus named exports per
            // identifier-safe class (Vite's dataToEsm with namedExports).
            let js = match &output.exports {
                Some(exports) => oj_css::css_modules_esm(exports),
                None => "export default void 0;".to_string(),
            };
            collected
                .lock()
                .unwrap()
                .push((path.to_string(), output.css));
            // When code-splitting CSS, keep the stub in its chunk so `module_ids`
            // maps each stylesheet to the chunk that imported it; without this the
            // unused-default stub is tree-shaken and the mapping is lost.
            let side_effects = self
                .css_code_split
                .then_some(rolldown_common::side_effects::HookSideEffects::NoTreeshake);
            Ok(Some(rolldown_plugin::HookLoadOutput {
                code: arcstr::ArcStr::from(js),
                module_type: Some(rolldown_common::ModuleType::Js),
                side_effects,
                ..Default::default()
            }))
        }
    }
}

// The build's CSS engines: one lazily spawned in-process JS engine per kind,
// shared by every stylesheet of the build (including nested worker bundles).
// An `oj build` process works on a single app root, so caching per kind is
// caching per (root, kind).
static BUILD_TAILWIND: tokio::sync::OnceCell<Arc<oj_server::css_engine::CssEngine>> =
    tokio::sync::OnceCell::const_new();
static BUILD_PREPROCESS: tokio::sync::OnceCell<Arc<oj_server::css_engine::CssEngine>> =
    tokio::sync::OnceCell::const_new();
static BUILD_SVELTE: tokio::sync::OnceCell<Arc<oj_server::css_engine::CssEngine>> =
    tokio::sync::OnceCell::const_new();

/// Tailwind / PostCSS over `css` (the file's content as processed so far, not
/// re-read from disk).
async fn expand_css_via_engine(root: &Path, css_file: &Path, css: &str) -> anyhow::Result<String> {
    let engine = BUILD_TAILWIND
        .get_or_try_init(|| {
            oj_server::css_engine::CssEngine::tailwind(root, oj_server::css_engine::BUILD_DEADLINE)
        })
        .await?;
    engine
        .compile_path(css, css_file, serde_json::Value::Null, false)
        .await
        .map_err(|e| anyhow::anyhow!("tailwind/postcss failed for {}: {e}", css_file.display()))
}

async fn svelte_via_engine(root: &Path, file: &Path) -> anyhow::Result<String> {
    let engine = BUILD_SVELTE
        .get_or_try_init(|| {
            oj_server::css_engine::CssEngine::svelte(root, oj_server::css_engine::BUILD_DEADLINE)
        })
        .await?;
    let source = fs::read_to_string(file)?;
    engine
        .compile_path(&source, file, serde_json::Value::Null, false)
        .await
        .map_err(|e| anyhow::anyhow!("svelte compile failed for {}: {e}", file.display()))
}

async fn preprocess_via_engine(
    root: &Path,
    css_file: &Path,
    css: &str,
    options: serde_json::Value,
) -> anyhow::Result<String> {
    let engine = BUILD_PREPROCESS
        .get_or_try_init(|| {
            oj_server::css_engine::CssEngine::preprocess(
                root,
                oj_server::css_engine::BUILD_DEADLINE,
            )
        })
        .await?;
    engine
        .compile_path(css, css_file, options, false)
        .await
        .map_err(|e| anyhow::anyhow!("css preprocess failed for {}: {e}", css_file.display()))
}

fn rolldown_resolve(
    root: &Path,
    config: &oj_config::OjConfig,
    env: &str,
) -> Option<rolldown_common::ResolveOptions> {
    let alias = oj_config::resolve_alias(config, env);
    let alias: Vec<(String, Vec<Option<String>>)> = alias
        .into_iter()
        .map(|(find, replacement)| {
            let target = if replacement.starts_with('.') {
                root.join(&replacement).to_string_lossy().into_owned()
            } else {
                replacement
            };
            (find, vec![Some(target)])
        })
        .collect();
    // The same `resolve.*` the dev resolver honors, so a module that resolves in
    // dev (a custom extension, a mainFields order, an exports condition) also
    // resolves in the build. Only user-set values are forwarded; rolldown keeps
    // its own defaults for the rest.
    let extensions = oj_config::resolve_extensions(config);
    // Vite's pkg.main fallback (entryPoint ||= data.main): rolldown walks the
    // list verbatim, so a Vite-shaped main-less list must get "main" appended
    // here too, or an import that resolves in dev fails in the build.
    let main_fields = oj_config::resolve_main_fields(config).map(oj_resolver::with_main_fallback);
    // Conditions are always explicit: Vite resolves a build with `production`
    // (its `development|production` default), and the dev server passes the
    // same list, so a dep with a `development` export never differs between
    // dev and build. Same source selection as every other Node-executing
    // consumer: a runner-backed ssr environment's list describes the plugin's
    // runtime (workerd), and this generic server bundle executes under Node,
    // so it takes Vite's Node server semantics instead of the foreign list —
    // unless `ssr.target: "webworker"` says this ssr bundle itself targets a
    // worker runtime: it never executes under Node then, and Node conditions
    // would pick the wrong entries, so the environment's own list stands.
    let condition_names = Some(
        if env == "ssr"
            && oj_config::ssr_runner_backed(config)
            && !oj_config::ssr_externals(config).webworker()
        {
            oj_config::node_server_conditions(config, false)
        } else {
            oj_config::resolve_conditions_for(config, env, false)
        },
    );
    let symlinks = config
        .resolve
        .as_ref()
        .and_then(|r| r.preserve_symlinks)
        .map(|preserve| !preserve);
    Some(rolldown_common::ResolveOptions {
        alias: (!alias.is_empty()).then_some(alias),
        extensions,
        main_fields,
        condition_names,
        symlinks,
        // `.js` -> `.ts` remap for every fs path (aliases included), as in dev.
        extension_alias: Some(oj_resolver::default_extension_alias()),
        ..Default::default()
    })
}

fn is_server_module_path(path: &str) -> bool {
    [".server.ts", ".server.tsx", ".server.js", ".server.jsx"]
        .iter()
        .any(|s| path.ends_with(s))
}

fn server_fn_prod_stub(exports: &[String], url: &str) -> String {
    let mut out = String::from(
        "const __ojCall = (m, n, a) => fetch(\"/__oj_fn\", { method: \"POST\", \
         headers: { \"content-type\": \"application/json\" }, \
         body: JSON.stringify({ module: m, name: n, args: a }) })\
         .then((r) => { if (!r.ok) throw new Error(\"oj server fn \" + n + \": \" + r.status); return r.json(); });\n",
    );
    for name in exports {
        if name == "default" {
            out.push_str(&format!(
                "export default (...a) => __ojCall({url:?}, \"default\", a);\n"
            ));
        } else {
            out.push_str(&format!(
                "export const {name} = (...a) => __ojCall({url:?}, {name:?}, a);\n"
            ));
        }
    }
    out
}

fn copy_public_dir(src: &Path, dest: &Path) -> anyhow::Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            fs::create_dir_all(&to)?;
            copy_public_dir(&from, &to)?;
        } else if !to.exists() {
            if let Some(parent) = to.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

// Shared state for chunks/pages a plugin emits during the build. Held by both
// OjUserPlugin (which fills it inside hooks) and build() (which reads the
// collected HTML pages afterward to render them).
#[derive(Debug)]
struct EmitState {
    root: PathBuf,
    // oj chunk ref id -> rolldown ref id, resolved to the hashed name later.
    chunk_refs: std::sync::Mutex<std::collections::HashMap<String, String>>,
    // oj ref id -> an already-resolved output name (emitted `.html` pages, whose
    // output path is known at emit time rather than via rolldown).
    direct_names: std::sync::Mutex<std::collections::HashMap<String, String>>,
    // `.html` pages a plugin emitted as chunks, to render after the build.
    html_docs: std::sync::Mutex<Vec<HtmlDoc>>,
}

impl EmitState {
    fn new(root: PathBuf) -> Self {
        Self {
            root,
            chunk_refs: std::sync::Mutex::new(std::collections::HashMap::new()),
            direct_names: std::sync::Mutex::new(std::collections::HashMap::new()),
            html_docs: std::sync::Mutex::new(Vec::new()),
        }
    }
}

#[derive(Debug)]
struct OjUserPlugin {
    host: Arc<PluginHost>,
    render_chunk_enabled: Arc<tokio::sync::OnceCell<bool>>,
    emit: Arc<EmitState>,
    // Vite/rolldown push hook filters into the bundler so a filtered hook never
    // runs for a module it does not claim; the host's live plan gates the
    // isolate RPCs the same way. It only over-approximates (the host
    // re-filters per plugin), so a gated-out call is one no plugin would have
    // acted on. This construction-time snapshot only decides HookUsage bits;
    // per-call gates read the live plan, which a respawned host resets.
    gate: oj_server::plugins::BuildHookPlan,
    // OJ_DEBUG_HOOK_GATE=1: count gated-out RPCs and report at closeBundle,
    // so a test can assert the gate actually skipped something.
    gate_debug: bool,
    skipped_resolve: std::sync::atomic::AtomicU64,
    skipped_load: std::sync::atomic::AtomicU64,
    skipped_transform: std::sync::atomic::AtomicU64,
}

impl OjUserPlugin {
    fn new(
        host: Arc<PluginHost>,
        emit: Arc<EmitState>,
        gate: oj_server::plugins::BuildHookPlan,
    ) -> Self {
        Self {
            host,
            render_chunk_enabled: Arc::new(tokio::sync::OnceCell::new()),
            emit,
            gate,
            gate_debug: oj_server::plugins::hook_gate_debug(),
            skipped_resolve: Default::default(),
            skipped_load: Default::default(),
            skipped_transform: Default::default(),
        }
    }
}

// The rolldown module type a plugin-loaded id should parse as, from its
// extension (query stripped). Defaults to Js, matching Rollup/Vite treating an
// extensionless virtual id as JavaScript.
fn module_type_for_id(id: &str) -> rolldown_common::ModuleType {
    use rolldown_common::ModuleType;
    let path = id.split(['?', '#']).next().unwrap_or(id);
    match Path::new(path).extension().and_then(|e| e.to_str()) {
        Some("jsx") => ModuleType::Jsx,
        Some("tsx") => ModuleType::Tsx,
        Some("ts" | "mts" | "cts") => ModuleType::Ts,
        Some("json") => ModuleType::Json,
        _ => ModuleType::Js,
    }
}

// Forward chunks a plugin emitted (in buildStart or transform) to rolldown as
// build roots. A `.html` id is not a JS module: its `<script type=module>` are
// emitted as JS entries instead and the page is queued for rendering, with the
// ref id resolving to the page's output path.
fn forward_emitted_chunks(
    ctx: &PluginContext,
    chunks: &[oj_server::plugins::ChunkEmit],
    emit: &EmitState,
) {
    if chunks.is_empty() {
        return;
    }
    for c in chunks {
        if c.id.ends_with(".html") {
            forward_emitted_html(ctx, c, emit);
            continue;
        }
        let emitted = rolldown_common::EmittedChunk {
            id: c.id.clone(),
            name: c.name.clone().map(Into::into),
            file_name: c.file_name.clone().map(Into::into),
            ..Default::default()
        };
        if let Ok(rd_ref) = ctx.emit_chunk(emitted) {
            emit.chunk_refs
                .lock()
                .unwrap()
                .insert(c.ref_id.clone(), rd_ref.to_string());
        }
    }
}

fn forward_emitted_html(ctx: &PluginContext, c: &oj_server::plugins::ChunkEmit, emit: &EmitState) {
    let abs = if Path::new(&c.id).is_absolute() {
        PathBuf::from(&c.id)
    } else {
        emit.root.join(&c.id)
    };
    let Ok(content) = fs::read_to_string(&abs) else {
        return;
    };
    let html_dir = abs.parent().unwrap_or(&emit.root).to_path_buf();
    let out_rel = abs
        .strip_prefix(&emit.root)
        .map(|p| p.display().to_string().replace('\\', "/"))
        .unwrap_or_else(|_| {
            abs.file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| c.id.clone())
        });
    let scripts: Vec<HtmlScript> = module_script_srcs(&content)
        .into_iter()
        .map(|src| {
            let sabs = resolve_html_ref(&src, &html_dir, &emit.root);
            let emitted = rolldown_common::EmittedChunk {
                id: sabs.display().to_string(),
                name: sabs.file_stem().and_then(|s| s.to_str()).map(Into::into),
                ..Default::default()
            };
            let _ = ctx.emit_chunk(emitted);
            HtmlScript { src, abs: sabs }
        })
        .collect();
    emit.direct_names
        .lock()
        .unwrap()
        .insert(c.ref_id.clone(), out_rel.clone());
    emit.html_docs.lock().unwrap().push(HtmlDoc {
        out_rel,
        src_html: content,
        scripts,
        dir: emit.root.clone(),
    });
}

impl Plugin for OjUserPlugin {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("oj:user-plugins")
    }

    fn register_hook_usage(&self) -> rolldown_plugin::HookUsage {
        let mut usage = rolldown_plugin::HookUsage::BuildStart
            | rolldown_plugin::HookUsage::GenerateBundle
            | rolldown_plugin::HookUsage::RenderChunk
            | rolldown_plugin::HookUsage::WriteBundle
            | rolldown_plugin::HookUsage::RenderStart
            | rolldown_plugin::HookUsage::CloseBundle;
        // When no user plugin has the hook at all, rolldown never needs to call
        // the Rust hook either.
        if self.gate.resolve_id.present {
            usage |= rolldown_plugin::HookUsage::ResolveId;
        }
        if self.gate.load.present {
            usage |= rolldown_plugin::HookUsage::Load;
        }
        if self.gate.transform.present {
            usage |= rolldown_plugin::HookUsage::Transform;
        }
        usage
    }

    async fn build_start(
        &self,
        ctx: &PluginContext,
        _args: &rolldown_plugin::HookBuildStartArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        // Vite/rolldown: a rejecting buildStart fails the build.
        match self.host.build_start().await {
            Ok(chunks) => forward_emitted_chunks(ctx, &chunks, &self.emit),
            Err(e) => return Err(anyhow::anyhow!("plugin buildStart failed:\n{e}")),
        }
        Ok(())
    }

    fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl std::future::Future<Output = HookResolveIdReturn> + Send {
        let pass = self.host.hook_wants_resolve_id(args.specifier);
        if !pass && self.gate_debug {
            self.skipped_resolve
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let host = Arc::clone(&self.host);
        let spec = args.specifier.to_string();
        let importer = args.importer.unwrap_or("").to_string();
        async move {
            if !pass {
                return Ok(None);
            }
            host.resolve_id(&spec, &importer)
                .await
                .map(|r| r.map(HookResolveIdOutput::from_id))
                .map_err(|e| anyhow::anyhow!("plugin resolveId failed for {spec}:\n{e}"))
        }
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl std::future::Future<Output = HookLoadReturn> + Send {
        let pass = self.host.hook_wants_load(args.id);
        if !pass && self.gate_debug {
            self.skipped_load
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let host = Arc::clone(&self.host);
        let id = args.id.to_string();
        async move {
            if !pass {
                return Ok(None);
            }
            // A throwing plugin `load`/`transform` fails the build, as in Vite;
            // bundling the raw source instead would ship wrong output silently.
            host.load(&id)
                .await
                .map_err(|e| anyhow::anyhow!("plugin load failed for {id}:\n{e}"))
                .map(|loaded| {
                    loaded.map(|code| {
                        let path = id.split(['?', '#']).next().unwrap_or(&id);
                        if path.ends_with(".css") {
                            // A plugin-served virtual CSS module (e.g. UnoCSS's layer
                            // placeholder): its real CSS is routed through the
                            // `vite:css-post` shim, so keep an empty side-effect stub
                            // in the graph (so the plugin's build hook still sees the
                            // module) rather than parsing CSS as JS.
                            return HookLoadOutput {
                                code: arcstr::ArcStr::from("export {};\n"),
                                module_type: Some(rolldown_common::ModuleType::Js),
                                side_effects: Some(
                                    rolldown_common::side_effects::HookSideEffects::NoTreeshake,
                                ),
                                ..Default::default()
                            };
                        }
                        HookLoadOutput {
                            code: arcstr::ArcStr::from(code),
                            // Infer the type from the id so a plugin-served virtual
                            // module keeps its JSX/TS semantics (e.g. unplugin-icons'
                            // `~icons/*.jsx`); a hardcoded Js made oxc parse the JSX
                            // as plain JS and fail.
                            module_type: Some(module_type_for_id(&id)),
                            ..Default::default()
                        }
                    })
                })
        }
    }

    fn transform(
        &self,
        ctx: SharedTransformPluginContext,
        args: &HookTransformArgs<'_>,
    ) -> impl std::future::Future<Output = HookTransformReturn> + Send {
        let pass = self.host.hook_wants_transform(args.id, args.code.as_str());
        if !pass && self.gate_debug {
            self.skipped_transform
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        let host = Arc::clone(&self.host);
        let emit = Arc::clone(&self.emit);
        let code: &str = args.code;
        let id: &str = args.id;
        async move {
            if !pass {
                return Ok(None);
            }
            // A `.svg` is componentized (or left as an asset) in the load hook, where
            // the plugin transform chain already ran on the raw markup. Re-running it
            // here would feed svgr its own component output (or an `export default`
            // asset stub) and corrupt it, so skip svg ids.
            if id.split('?').next().unwrap_or(id).ends_with(".svg") {
                return Ok(None);
            }
            match host.transform(code, id, "{}").await {
                Ok((out, _, _, chunks)) => {
                    forward_emitted_chunks(&ctx.inner, &chunks, &emit);
                    if out != code {
                        Ok(Some(HookTransformOutput {
                            code: Some(out),
                            ..Default::default()
                        }))
                    } else {
                        Ok(None)
                    }
                }
                Err(e) => Err(anyhow::anyhow!("plugin transform failed for {id}:\n{e}")),
            }
        }
    }

    async fn generate_bundle(
        &self,
        ctx: &PluginContext,
        args: &mut rolldown_plugin::HookGenerateBundleArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        // Resolve the final hashed name of every chunk the plugin emitted and
        // seed them into the sidecar so `this.getFileName(refId)` (read here in
        // generateBundle) returns the real output filename.
        let refs: Vec<(String, String)> = self
            .emit
            .chunk_refs
            .lock()
            .unwrap()
            .iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        let direct: Vec<(String, String)> = self
            .emit
            .direct_names
            .lock()
            .unwrap()
            .iter()
            .map(|(a, b)| (a.clone(), b.clone()))
            .collect();
        if !refs.is_empty() || !direct.is_empty() {
            let mut map = serde_json::Map::new();
            for (oj_ref, rd_ref) in refs {
                if let Ok(name) = ctx.get_file_name(&rd_ref) {
                    map.insert(oj_ref, serde_json::Value::String(name.to_string()));
                }
            }
            // Emitted `.html` pages resolve directly to their output path.
            for (oj_ref, name) in direct {
                map.insert(oj_ref, serde_json::Value::String(name));
            }
            if !map.is_empty() {
                let _ = self
                    .host
                    .seed_chunk_names(&serde_json::Value::Object(map).to_string())
                    .await;
            }
        }
        if !self.host.has_generate_bundle().await {
            return Ok(());
        }
        let bundle_json = serialize_bundle_with_vite_manifest(args.bundle, &self.emit.root);
        let mutated = self
            .host
            .generate_bundle(&bundle_json, args.is_write)
            .await
            .map_err(|e| anyhow::anyhow!("plugin generateBundle failed:\n{e}"))?;
        if let Some(mutated) = mutated {
            apply_bundle_mutations(args.bundle, &mutated);
        }
        Ok(())
    }

    fn render_chunk(
        &self,
        _ctx: &PluginContext,
        args: &HookRenderChunkArgs<'_>,
    ) -> impl std::future::Future<Output = HookRenderChunkReturn> + Send {
        let host = Arc::clone(&self.host);
        let enabled = Arc::clone(&self.render_chunk_enabled);
        let code = Arc::clone(&args.code);
        // Once the first call learned no plugin has renderChunk, later chunks
        // skip the JSON serialization too, not just the RPC.
        let chunk_json = if enabled.get() == Some(&false) {
            String::new()
        } else {
            serialize_rendered_chunk(&args.chunk)
        };
        async move {
            let on = *enabled
                .get_or_init(|| async { host.has_render_chunk().await })
                .await;
            if !on {
                return Ok(None);
            }
            match host.render_chunk(&code, &chunk_json).await {
                Ok(Some(out)) if out != *code => Ok(Some(HookRenderChunkOutput {
                    code: out,
                    map: HookTransformOutputMap::Null,
                })),
                _ => Ok(None),
            }
        }
    }

    async fn write_bundle(
        &self,
        _ctx: &PluginContext,
        args: &mut rolldown_plugin::HookWriteBundleArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        if !self.host.has_write_bundle().await {
            return Ok(());
        }
        let bundle_json = serialize_bundle(args.bundle);
        self.host
            .write_bundle(&bundle_json, true)
            .await
            .map_err(|e| anyhow::anyhow!("plugin writeBundle failed:\n{e}"))
    }

    async fn render_start(
        &self,
        _ctx: &PluginContext,
        _args: &rolldown_plugin::HookRenderStartArgs<'_>,
    ) -> rolldown_plugin::HookNoopReturn {
        self.host
            .render_start()
            .await
            .map_err(|e| anyhow::anyhow!("plugin renderStart failed:\n{e}"))
    }

    async fn close_bundle(
        &self,
        _ctx: &PluginContext,
        _args: Option<&rolldown_plugin::HookCloseBundleArgs<'_>>,
    ) -> rolldown_plugin::HookNoopReturn {
        if self.gate_debug {
            use std::sync::atomic::Ordering::Relaxed;
            eprintln!(
                "oj: hook gate skipped resolveId={} load={} transform={}",
                self.skipped_resolve.load(Relaxed),
                self.skipped_load.load(Relaxed),
                self.skipped_transform.load(Relaxed),
            );
        }
        self.host
            .close_bundle()
            .await
            .map_err(|e| anyhow::anyhow!("plugin closeBundle failed:\n{e}"))
    }
}

fn serialize_rendered_chunk(chunk: &rolldown_common::RollupRenderedChunk) -> String {
    // `chunk.modules` (keyed by module id) is read by renderChunk hooks such as
    // UnoCSS's `unocss:global:build:generate` to find their virtual layer module.
    let mut modules = serde_json::Map::new();
    for (id, rm) in chunk.modules.keys.iter().zip(chunk.modules.values.iter()) {
        modules.insert(
            id.as_str().to_string(),
            serde_json::json!({
                "renderedExports": rm
                    .rendered_exports
                    .iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>(),
            }),
        );
    }
    serde_json::json!({
        "type": "chunk",
        "fileName": chunk.filename.to_string(),
        "name": chunk.name.to_string(),
        "isEntry": chunk.is_entry,
        "isDynamicEntry": chunk.is_dynamic_entry,
        "facadeModuleId": chunk.facade_module_id.as_ref().map(|f| f.as_str().to_string()),
        "moduleIds": chunk.module_ids.iter().map(|m| m.as_str().to_string()).collect::<Vec<_>>(),
        "modules": serde_json::Value::Object(modules),
        "imports": chunk.imports.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
        "dynamicImports": chunk.dynamic_imports.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
        "exports": chunk.exports.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
    })
    .to_string()
}

fn serialize_bundle(bundle: &[rolldown_common::Output]) -> String {
    use rolldown_common::{Output, StrOrBytes};
    let mut map = serde_json::Map::new();
    for out in bundle {
        match out {
            Output::Chunk(c) => {
                // Rollup's `chunk.modules` is `{ [id]: RenderedModule }`; plugins
                // like @crxjs walk its keys (module ids) and read renderedExports.
                let mut modules = serde_json::Map::new();
                for (id, rm) in c.modules.keys.iter().zip(c.modules.values.iter()) {
                    modules.insert(
                        id.as_str().to_string(),
                        serde_json::json!({
                            "renderedExports": rm
                                .rendered_exports
                                .iter()
                                .map(|e| e.to_string())
                                .collect::<Vec<_>>(),
                        }),
                    );
                }
                map.insert(
                    c.filename.to_string(),
                    serde_json::json!({
                        "type": "chunk",
                        "fileName": c.filename.to_string(),
                        "name": c.name.to_string(),
                        "isEntry": c.is_entry,
                        "isDynamicEntry": c.is_dynamic_entry,
                        "facadeModuleId": c.facade_module_id.as_ref().map(|f| f.as_str().to_string()),
                        "moduleIds": c.module_ids.iter().map(|m| m.as_str().to_string()).collect::<Vec<_>>(),
                        "modules": serde_json::Value::Object(modules),
                        "imports": c.imports.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
                        "dynamicImports": c.dynamic_imports.iter().map(|i| i.to_string()).collect::<Vec<_>>(),
                        "exports": c.exports.iter().map(|e| e.to_string()).collect::<Vec<_>>(),
                        "code": c.code,
                        "map": serde_json::Value::Null,
                        "sourcemapFileName": c.sourcemap_filename,
                    }),
                );
            }
            Output::Asset(a) => {
                let source = match &a.source {
                    StrOrBytes::Str(s) => Some(s.as_str()),
                    StrOrBytes::Bytes(_) => None,
                };
                map.insert(
                    a.filename.to_string(),
                    serde_json::json!({
                        "type": "asset",
                        "fileName": a.filename.to_string(),
                        "name": a.names.first(),
                        "names": a.names,
                        "source": source,
                    }),
                );
            }
        }
    }
    serde_json::Value::Object(map).to_string()
}

fn apply_bundle_mutations(bundle: &mut Vec<rolldown_common::Output>, json: &str) {
    use rolldown_common::{Output, StrOrBytes};
    let Ok(map) = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(json) else {
        return;
    };
    // A plugin can delete a bundle entry (e.g. @crxjs removes its manifest JS
    // chunk and the input stub): any output whose filename no longer appears in
    // the returned bundle was removed.
    bundle.retain(|out| {
        let key = match out {
            Output::Chunk(c) => c.filename.as_str(),
            Output::Asset(a) => a.filename.as_str(),
        };
        map.contains_key(key)
    });
    for out in bundle.iter_mut() {
        match out {
            Output::Chunk(c) => {
                let Some(entry) = map.get(c.filename.as_str()) else {
                    continue;
                };
                if let Some(code) = entry.get("code").and_then(|x| x.as_str()) {
                    if c.code != code {
                        Arc::make_mut(c).code = code.to_string();
                    }
                }
                // A plugin can rename an output by mutating its `fileName`
                // (the map key stays the original name).
                if let Some(fname) = entry.get("fileName").and_then(|x| x.as_str()) {
                    if c.filename.as_str() != fname {
                        Arc::make_mut(c).filename = fname.into();
                    }
                }
            }
            Output::Asset(a) => {
                let Some(entry) = map.get(a.filename.as_str()) else {
                    continue;
                };
                if let Some(src) = entry.get("source").and_then(|x| x.as_str()) {
                    if a.source.as_bytes() != src.as_bytes() {
                        Arc::make_mut(a).source = StrOrBytes::Str(src.to_string());
                    }
                }
                if let Some(fname) = entry.get("fileName").and_then(|x| x.as_str()) {
                    if a.filename.as_str() != fname {
                        Arc::make_mut(a).filename = fname.into();
                    }
                }
            }
        }
    }
}

async fn user_plugin_host(
    root: &Path,
    base: &str,
    define: &serde_json::Value,
    environments: &serde_json::Value,
    env_name: &str,
    mode: &str,
) -> Option<Arc<PluginHost>> {
    let (file, plugins_format, label) = match oj_server::plugins::plugin_source(root)? {
        oj_server::plugins::PluginSource::OjPlugins(p) => {
            let label = p.file_name().unwrap().to_string_lossy().into_owned();
            (p, "oj", label)
        }
        oj_server::plugins::PluginSource::ViteConfig(p) => (p, "vite", "vite.config".to_string()),
    };
    let config = serde_json::json!({
        "config": { "root": root.display().to_string(), "base": base, "mode": mode, "command": "build", "define": define, "environments": environments },
        "env": { "command": "build", "mode": mode },
        "environment": { "name": env_name, "mode": "build" },
        "pluginsFormat": plugins_format,
    })
    .to_string();
    match PluginHost::spawn(root, &file, &config).await {
        Ok(host) => {
            println!("oj build ({env_name}): plugins from {label}");
            Some(host)
        }
        Err(e) => {
            eprintln!("oj build ({env_name}): plugin host failed to start: {e}");
            None
        }
    }
}

// Vite (build.ts) closes the bundle in a `finally`, so a failed build still
// reaches every plugin's buildEnd (with the error) and closeBundle. rolldown
// runs them itself only when the scan fails; a failure past that (render,
// write) would skip them, so the host is told directly. Its closeBundle is
// idempotent, so the rolldown-driven call and this one do not double up.
async fn fail_with_plugin_hooks(
    err: anyhow::Error,
    host: &Option<Arc<PluginHost>>,
) -> anyhow::Error {
    if let Some(h) = host {
        let message = format!("{err:#}");
        if let Err(e) = h.build_end(Some(&message)).await {
            eprintln!("oj build: plugin buildEnd (after failure) failed:\n{e}");
        }
        if let Err(e) = h.close_bundle().await {
            eprintln!("oj build: plugin closeBundle (after failure) failed:\n{e}");
        }
    }
    err
}

// `define` entries the plugins' config() hooks returned; Vite merges them into
// config.define, so they join the build's define map (plugin value wins).
async fn plugin_config_defines(host: &Option<Arc<PluginHost>>) -> Vec<(String, String)> {
    match host {
        Some(h) => h.config_defines().await,
        None => Vec::new(),
    }
}

/// `oj build` command-line options that override the config file, as Vite's
/// CLI merges its `build.*` flags and `--base` over the resolved config.
#[derive(Debug, Default, Clone)]
pub struct CliOptions {
    pub out: Option<PathBuf>,
    pub ssr: Option<String>,
    pub empty_out_dir: bool,
    pub base: Option<String>,
    pub assets_dir: Option<String>,
    pub assets_inline_limit: Option<u64>,
    pub target: Option<String>,
    /// `--sourcemap [true|false|inline|hidden]`
    pub sourcemap: Option<String>,
    /// `--minify [true|false|oxc|esbuild|terser]`
    pub minify: Option<String>,
    /// `--manifest [name]`
    pub manifest: Option<String>,
    /// `--ssrManifest [name]`
    pub ssr_manifest: Option<String>,
    pub watch: bool,
}

static CLI_OPTIONS: std::sync::OnceLock<CliOptions> = std::sync::OnceLock::new();

/// Vite's cac turns `--flag` into `true` and `--flag false` into `false`; a
/// bare `--manifest` selects the default file name.
fn cli_bool_or_str(v: &str) -> oj_config::BoolOrString {
    match v {
        "true" => oj_config::BoolOrString::Bool(true),
        "false" => oj_config::BoolOrString::Bool(false),
        s => oj_config::BoolOrString::Str(s.to_string()),
    }
}

/// Layer the CLI options over a loaded config. Every build stage that reloads
/// the config (SSR, client entry, server functions) applies the same layer, so
/// `--base` and friends reach each of them.
fn apply_cli_options(config: &mut oj_config::OjConfig) {
    if let Some(cli) = CLI_OPTIONS.get() {
        apply_cli_options_from(config, cli);
    }
}

fn apply_cli_options_from(config: &mut oj_config::OjConfig, cli: &CliOptions) {
    if let Some(base) = &cli.base {
        config.base = Some(base.clone());
    }
    let has_build_flags = cli.out.is_some()
        || cli.assets_dir.is_some()
        || cli.assets_inline_limit.is_some()
        || cli.target.is_some()
        || cli.sourcemap.is_some()
        || cli.minify.is_some()
        || cli.manifest.is_some()
        || cli.ssr_manifest.is_some()
        || cli.empty_out_dir;
    if !has_build_flags {
        return;
    }
    let b = config.build.get_or_insert_with(Default::default);
    if let Some(out) = &cli.out {
        b.out_dir = Some(out.display().to_string());
    }
    if let Some(d) = &cli.assets_dir {
        b.assets_dir = Some(d.clone());
    }
    if let Some(n) = cli.assets_inline_limit {
        b.assets_inline_limit = Some(n);
    }
    if let Some(t) = &cli.target {
        b.target = Some(oj_config::StringOrList::One(t.clone()));
    }
    if let Some(s) = &cli.sourcemap {
        b.sourcemap = Some(cli_bool_or_str(s));
    }
    if let Some(m) = &cli.minify {
        b.minify = Some(cli_bool_or_str(m));
    }
    if let Some(m) = &cli.manifest {
        b.manifest = Some(cli_bool_or_str(m));
    }
    if let Some(m) = &cli.ssr_manifest {
        b.ssr_manifest = Some(cli_bool_or_str(m));
    }
    if cli.empty_out_dir {
        b.empty_out_dir = Some(true);
    }
}

/// `build.*` options oj accepts for compatibility but cannot honor; each is
/// named once so a config relying on it does not fail silently.
fn warn_unsupported_build_options(config: &oj_config::OjConfig) {
    let Some(b) = config.build.as_ref() else {
        return;
    };
    let warn = |what: &str, hint: &str| {
        eprintln!("oj build: (!) build.{what} is not supported by oj and is ignored. {hint}");
    };
    if b.write == Some(false) {
        warn("write: false", "oj always writes the bundle to outDir.");
    }
    if b.watch
        .as_ref()
        .is_some_and(|w| !w.is_null() && *w != serde_json::Value::Bool(false))
    {
        warn(
            "watch",
            "Run `oj build` again after changes, or use `oj dev`.",
        );
    }
    if b.license
        .as_ref()
        .is_some_and(|l| *l != serde_json::Value::Bool(false))
    {
        warn(
            "license",
            "No LICENSE file is emitted for the bundled dependencies.",
        );
    }
    if b.commonjs_options.is_some() {
        warn(
            "commonjsOptions",
            "rolldown handles CommonJS dependencies natively without configuration.",
        );
    }
}

pub async fn build(root: PathBuf, cli_mode: Option<&str>, cli: CliOptions) -> anyhow::Result<()> {
    let root = root
        .canonicalize()
        .with_context(|| format!("app root not found: {}", root.display()))?;
    if cli.watch {
        bail!("oj build --watch is not supported yet; run `oj build` again after changes, or use `oj dev`");
    }
    let ssr = cli.ssr.clone();
    let _ = CLI_OPTIONS.set(cli);

    // Vite: `mode = inlineConfig.mode || config.mode || "production"`; when the
    // config file itself names a mode and the CLI did not, the config is loaded
    // again under that mode (its function form and `.env.<mode>` depend on it).
    let mut mode_owned = cli_mode.unwrap_or("production").to_string();
    let mut config =
        oj_config::load_with(&root, "build", &mode_owned).map_err(|e| anyhow::anyhow!("{e}"))?;
    if cli_mode.is_none() {
        oj_server::plugins::adopt_vite_config_values_default_mode(
            &mut config,
            &root,
            "build",
            &mode_owned,
        )
        .map_err(|e| anyhow::anyhow!(e))?;
        if let Some(m) = config.mode.clone().filter(|m| *m != mode_owned) {
            mode_owned = m;
            config = oj_config::load_with(&root, "build", &mode_owned)
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            oj_server::plugins::adopt_vite_config_values(&mut config, &root, "build", &mode_owned)
                .map_err(|e| anyhow::anyhow!(e))?;
        }
    } else {
        oj_server::plugins::adopt_vite_config_values(&mut config, &root, "build", &mode_owned)
            .map_err(|e| anyhow::anyhow!(e))?;
    }
    apply_cli_options(&mut config);
    warn_unsupported_build_options(&config);
    let mode: &str = &mode_owned;
    let build_cfg = config.build.clone().unwrap_or_default();
    let ro_opts = oj_config::rolldown_options(&config);
    let out = build_cfg
        .out_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("dist"));
    let out_dir = if out.is_absolute() {
        out
    } else {
        root.join(&out)
    };
    let minify = oj_config::build_minify(&config);
    let sourcemap = oj_config::build_sourcemap(&config);
    let empty_out_dir = build_cfg.empty_out_dir;
    let loaded_env = oj_env::load(&env_dir_of(&root, &config), mode);
    let env_prefixes = env_prefixes_of(&config);
    let env_prefix_refs: Vec<&str> = env_prefixes.iter().map(String::as_str).collect();
    let node_env = oj_env::resolve_node_env(shell_node_env().as_deref(), &loaded_env, "production");
    let is_production = node_env == "production";

    let config_ssr_entry =
        oj_config::build_ssr_entry(&config).map_err(|e| anyhow::anyhow!("{e}"))?;
    if let Some(entry) = ssr.or(config_ssr_entry) {
        return build_ssr_app(
            &root,
            &out_dir,
            &entry,
            mode,
            minify,
            sourcemap,
            build_cfg.prerender.clone(),
            empty_out_dir,
        )
        .await;
    }

    if let Some(lib) = build_cfg.lib.clone() {
        return build_library(&root, &out_dir, &config, lib, mode, minify, sourcemap).await;
    }

    let base = normalize_base(config.base.as_deref().unwrap_or("/"));
    // Vite's default file names live under `build.assetsDir` (`assets/[name]-[hash].js`,
    // `assets/[name]-[hash][extname]`); an explicit `output.*FileNames` wins.
    let assets_dir = oj_config::build_assets_dir(&config);
    let asset_names_pattern = ro_output_str(ro_opts, "assetFileNames")
        .unwrap_or_else(|| oj_config::assets_dir_path(&assets_dir, "[name]-[hash][extname]"));

    prepare_out_dir(&root, &out_dir, empty_out_dir)?;

    let started = Instant::now();

    // Vite resolves the build entry as `rolldownOptions.input || index.html`,
    // where input is string | string[] | Record<name, path>. Each `.html`
    // entry is processed as a page (its `<script type=module>` become JS
    // inputs and the page is re-emitted); everything else is a direct entry.
    let ro_input = ro_opts
        .and_then(|r| r.get("input"))
        .filter(|v| !v.is_null());
    let named_inputs: Vec<(String, String)> = match ro_input {
        Some(v) => normalize_input_entries(v),
        None => vec![("index".to_string(), "index.html".to_string())],
    };
    if named_inputs.is_empty() {
        bail!("build.rollupOptions.input resolved to no entries");
    }

    let mut html_docs: Vec<HtmlDoc> = Vec::new();
    let html_inline: Arc<Mutex<std::collections::HashMap<String, String>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let mut inputs: Vec<InputItem> = Vec::new();
    let mut seen_imports: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut had_html = false;
    let push_input = |abs: &Path,
                      name: String,
                      inputs: &mut Vec<InputItem>,
                      seen: &mut std::collections::HashSet<String>| {
        let rel = abs.strip_prefix(&root).unwrap_or(abs);
        let import = format!("./{}", rel.display().to_string().replace('\\', "/"));
        if seen.insert(import.clone()) {
            inputs.push(InputItem {
                name: Some(name),
                import,
            });
        }
    };
    for (name, rel) in &named_inputs {
        let rel_path = Path::new(rel);
        let abs = if rel_path.is_absolute() {
            rel_path.to_path_buf()
        } else {
            root.join(rel)
        };
        if rel.ends_with(".html") {
            let content = match fs::read_to_string(&abs) {
                Ok(c) => c,
                Err(_) if ro_input.is_none() => bail!("no index.html in {}", root.display()),
                Err(e) => {
                    return Err(anyhow::Error::from(e))
                        .with_context(|| format!("build input HTML not found: {}", abs.display()));
                }
            };
            had_html = true;
            let html_dir = abs.parent().unwrap_or(&root).to_path_buf();
            let (content, inline) = externalize_inline_scripts(&content, &abs);
            for (_, id, body) in &inline {
                html_inline.lock().unwrap().insert(id.clone(), body.clone());
            }
            let scripts: Vec<HtmlScript> = module_script_srcs(&content)
                .into_iter()
                .map(|src| {
                    let abs = match inline.iter().find(|(p, _, _)| *p == src) {
                        Some((_, id, _)) => PathBuf::from(id),
                        None => resolve_html_ref(&src, &html_dir, &root),
                    };
                    HtmlScript { src, abs }
                })
                .collect();
            for s in &scripts {
                let stem = s
                    .abs
                    .file_stem()
                    .and_then(|x| x.to_str())
                    .unwrap_or("entry")
                    .to_string();
                push_input(&s.abs, stem, &mut inputs, &mut seen_imports);
            }
            let out_rel = if rel_path.is_absolute() {
                abs.strip_prefix(&root)
                    .unwrap_or(&abs)
                    .display()
                    .to_string()
                    .replace('\\', "/")
            } else {
                rel.replace('\\', "/")
            };
            html_docs.push(HtmlDoc {
                out_rel,
                src_html: content,
                scripts,
                dir: html_dir,
            });
        } else {
            if !abs.exists() {
                bail!("build input not found: {}", abs.display());
            }
            push_input(&abs, name.clone(), &mut inputs, &mut seen_imports);
        }
    }
    if inputs.is_empty() {
        if had_html {
            bail!("index.html has no <script type=\"module\" src=...> entry");
        }
        bail!("build.rollupOptions.input resolved to no entries");
    }

    let collected_css: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let worker_entries: Arc<Mutex<std::collections::HashSet<String>>> =
        Arc::new(Mutex::new(std::collections::HashSet::new()));
    let css_split = css_code_split_of(&config);
    let plugin_host = user_plugin_host(
        &root,
        &base,
        &serde_json::json!(config.define),
        &serde_json::json!(config.environments),
        "client",
        mode,
    )
    .await;
    let plugin_defines = plugin_config_defines(&plugin_host).await;
    let emit = Arc::new(EmitState::new(root.to_path_buf()));
    let mut oj_plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(host) = &plugin_host {
        oj_plugins.push(Arc::new(OjUserPlugin::new(
            Arc::clone(host),
            Arc::clone(&emit),
            host.build_hook_plan().await,
        )));
    }
    let client_minify =
        oj_config::environment_build_bool(&config, "client", "minify").unwrap_or(minify);
    let client_define: Vec<(String, String)> = {
        let env = oj_env::with_process_env(loaded_env.clone(), std::env::vars(), &env_prefix_refs);
        let mut pairs: Vec<(String, String)> = process_env_defines(&node_env);
        pairs.push(import_meta_hot_define());
        pairs.extend(oj_env::import_meta_env_defines(
            &env,
            mode,
            !is_production,
            &base,
            &env_prefix_refs,
        ));
        pairs.extend(oj_config::config_defines(&config));
        pairs.extend(oj_config::environment_defines(&config, "client"));
        pairs.extend(plugin_defines);
        pairs
    };
    oj_plugins.push(Arc::new(OjCssPlugin {
        collected: Arc::clone(&collected_css),
        root: root.to_path_buf(),
        has_postcss: oj_server::has_postcss_config(&root),
        inline_limit: assets_inline_limit_of(&config),
        client: true,
        css_code_split: css_split,
        resolve: css_resolve_of(&root, &config, "client"),
        host: plugin_host.clone(),
        css_transform_enabled: Arc::new(tokio::sync::OnceCell::new()),
        css: config.css.clone(),
        html_inline: Arc::clone(&html_inline),
        worker: Some(Arc::new(WorkerBundleOpts {
            config: config.clone(),
            is_production,
            define: client_define.clone(),
            minify: client_minify,
        })),
        worker_entries: Arc::clone(&worker_entries),
    }));
    let out_ov = ro_output_overrides(ro_opts);
    // Vite's build import analysis (preload wrapping, polyfill) only applies to
    // es output; umd/iife/cjs bundles cannot use `import.meta.url`.
    let es_output = out_ov.format.is_none();
    let out_banner = out_ov.banner.clone();
    let mut bundler = BundlerBuilder::default()
        .with_plugins(oj_plugins)
        .with_options(BundlerOptions {
            input: Some(inputs),
            transform: transform_options(&config, is_production),
            code_splitting: if out_ov.inline_dynamic_imports {
                Some(rolldown_common::CodeSplittingMode::Bool(false))
            } else {
                manual_chunks(ro_opts, &root)
            },
            format: out_ov.format,
            banner: addon_option(out_ov.banner),
            footer: addon_option(out_ov.footer),
            intro: addon_option(out_ov.intro),
            outro: addon_option(out_ov.outro),
            globals: out_ov
                .globals
                .map(|g| rolldown_common::GlobalsOutputOption::FxHashMap(g.into_iter().collect())),
            paths: out_ov
                .paths
                .map(|p| rolldown_common::PathsOutputOption::FxHashMap(p.into_iter().collect())),
            name: out_ov.name,
            extend: out_ov.extend,
            treeshake: ro_treeshake(ro_opts),
            // Vite sets `preserveEntrySignatures: false` for app builds; oj keeps
            // rolldown's exports-only default because plugin-emitted chunks (crx
            // pages, scripts) rely on their entry exports surviving.
            cwd: Some(root.clone()),
            dir: Some(out_dir.display().to_string()),
            resolve: rolldown_resolve(&root, &config, "client"),
            entry_filenames: Some(
                ro_output_str(ro_opts, "entryFileNames")
                    .unwrap_or_else(|| oj_config::assets_dir_path(&assets_dir, "[name]-[hash].js"))
                    .into(),
            ),
            chunk_filenames: Some(
                ro_output_str(ro_opts, "chunkFileNames")
                    .unwrap_or_else(|| oj_config::assets_dir_path(&assets_dir, "[name]-[hash].js"))
                    .into(),
            ),
            asset_filenames: Some(asset_names_pattern.clone().into()),
            external: {
                let ext = ro_external(ro_opts);
                (!ext.is_empty()).then(|| rolldown::IsExternal::from(ext))
            },
            minify: Some(rolldown_minify(client_minify)),
            sourcemap: env_sourcemap(&config, "client", sourcemap),
            define: Some(client_define.into_iter().collect()),
            ..Default::default()
        })
        .build()
        .map_err(rolldown_failure!("rolldown init failed"))?;

    let output = match bundler.write().await {
        Ok(output) => output,
        Err(errs) => {
            let detail = errs
                .into_vec()
                .iter()
                .map(|e| e.to_diagnostic().to_string())
                .collect::<Vec<_>>()
                .join("\n");
            return Err(fail_with_plugin_hooks(
                anyhow::anyhow!("build failed:\n{detail}"),
                &plugin_host,
            )
            .await);
        }
    };
    bundler
        .close()
        .await
        .map_err(|errs| anyhow::anyhow!("close failed:\n{errs:?}"))?;

    let unresolved: Vec<String> = output
        .warnings
        .iter()
        .filter(|warning| warning.kind().to_string() == "UNRESOLVED_IMPORT")
        .map(|warning| warning.to_string())
        .collect();
    if !unresolved.is_empty() {
        // Vite fails the build on an unresolved import and writes nothing. rolldown
        // has already written the bundle by the time its diagnostics are readable, so
        // drop the partial output so a failed build never leaves a broken bundle
        // behind. Only under the same containment rule that emptied the directory at
        // build start: an outDir outside root (or the user's emptyOutDir: false) was
        // never oj's to clear, and a failed build must not start clearing it.
        if out_dir_emptiable(&root, &out_dir, empty_out_dir, false) {
            let _ = empty_dir_keep_git(&out_dir);
            // Nothing written means no directory either (like Vite); remove_dir is
            // non-recursive, so a kept .git simply leaves it in place.
            let _ = fs::remove_dir(&out_dir);
        }
        bail!(
            "build failed: unresolved imports:\n{}\n\nThis is most likely unintended because it can break your application at runtime.\nIf you do want to externalize a module explicitly, add it to `build.rollupOptions.external`.",
            unresolved.join("\n")
        );
    }

    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_end(None).await {
            bail!("plugin buildEnd failed:\n{e}");
        }
        // CSS a plugin routed through the vite:css-post shim (e.g. UnoCSS's
        // generated utilities) joins the build's stylesheet output.
        for (id, css) in host.get_plugin_css().await {
            let src = if id.is_empty() {
                root.join("__plugin_css__").display().to_string()
            } else {
                id.split(['?', '#']).next().unwrap_or(&id).to_string()
            };
            collected_css.lock().unwrap().push((src, css));
        }
        match host.emitted_files().await {
            Ok(files) => {
                for file in files {
                    let dest = out_dir.join(&file.file_name);
                    if let Some(parent) = dest.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(&dest, file.source.as_bytes())?;
                }
            }
            Err(e) => eprintln!("oj build: plugin emitFile collection failed: {e}"),
        }
    }

    for warning in &output.warnings {
        if format!("{warning:?}").contains("SOURCEMAP_BROKEN") {
            continue;
        }
        eprintln!("oj build warning: {warning:?}");
    }

    // %VITE_*% / import.meta.env substitution (Vite's htmlEnvHook), applied to
    // each page before the plugin transformIndexHtml below.
    let html_env = {
        let env = oj_env::with_process_env(loaded_env.clone(), std::env::vars(), &env_prefix_refs);
        let mut defines =
            oj_env::import_meta_env_defines(&env, mode, !is_production, &base, &env_prefix_refs);
        defines.extend(oj_config::config_defines(&config));
        oj_env::html_env_map(&defines)
    };
    let mut emitted: Vec<(String, usize)> = Vec::new();
    let has_postcss = oj_server::has_postcss_config(&root);
    let public_dir = oj_config::public_dir(&config, &root);
    let link_css_transform_enabled: tokio::sync::OnceCell<bool> = tokio::sync::OnceCell::new();
    let link_css_resolve = css_resolve_of(&root, &config, "client");
    let css_asset_opts = CssAssetOpts {
        inline_limit: assets_inline_limit_of(&config),
        asset_names: Some(asset_names_pattern.as_str()),
        resolve: link_css_resolve.as_ref(),
    };
    let mut manifest_entries: Vec<ManifestEntry> = Vec::new();
    let mut manifest_assets: Vec<ManifestAsset> = Vec::new();
    let mut imports_map: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    let mut entry_files: Vec<String> = Vec::new();
    let mut facade_to_file: std::collections::HashMap<PathBuf, String> =
        std::collections::HashMap::new();
    for asset in &output.assets {
        if let rolldown_common::Output::Chunk(chunk) = asset {
            let filename = chunk.filename.to_string();
            emitted.push((filename.clone(), chunk.code.len()));
            imports_map.insert(
                filename.clone(),
                chunk.imports.iter().map(|i| i.to_string()).collect(),
            );
            // Entry chunks are keyed by their source path; shared/non-entry
            // chunks by `_<filename>` (Vite's manifest shape).
            let src = if chunk.is_entry {
                chunk.facade_module_id.as_ref().and_then(|f| {
                    Path::new(f.as_ref())
                        .strip_prefix(&root)
                        .ok()
                        .map(|p| p.to_string_lossy().replace('\\', "/"))
                })
            } else {
                None
            };
            let key = src.clone().unwrap_or_else(|| {
                format!(
                    "_{}",
                    Path::new(&filename)
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or(&filename)
                )
            });
            manifest_entries.push(ManifestEntry {
                key,
                name: chunk.name.to_string(),
                file: filename.clone(),
                src,
                is_entry: chunk.is_entry,
                is_dynamic_entry: chunk.is_dynamic_entry,
                imports: chunk.imports.iter().map(|i| i.to_string()).collect(),
                dynamic_imports: chunk
                    .dynamic_imports
                    .iter()
                    .map(|i| i.to_string())
                    .collect(),
                css: Vec::new(),
                assets: Vec::new(),
            });
            if chunk.is_entry {
                entry_files.push(filename.clone());
                if let Some(facade) = &chunk.facade_module_id {
                    facade_to_file.insert(PathBuf::from(facade.as_ref()), filename.clone());
                }
            }
        } else if let rolldown_common::Output::Asset(asset) = asset {
            emitted.push((asset.filename.to_string(), asset.source.as_bytes().len()));
        }
    }

    // Non-split CSS is one combined stylesheet linked from every page.
    let combined_css_name: Option<String> = if !css_split {
        let mut css_entries = collected_css.lock().unwrap().clone();
        if css_entries.is_empty() {
            None
        } else {
            css_entries.sort();
            let sources = css_entries.clone();
            let mut seen_assets: std::collections::HashMap<PathBuf, String> =
                std::collections::HashMap::new();
            let combined: String = css_entries
                .into_iter()
                .map(|(src, css)| {
                    let dir = Path::new(&src)
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| root.to_path_buf());
                    rebase_css_urls(
                        &css,
                        &dir,
                        &out_dir,
                        &base,
                        css_asset_opts,
                        &mut emitted,
                        &mut seen_assets,
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            let hash = content_hash(combined.as_bytes());
            let css_name = render_asset_name(css_asset_opts.asset_names, "style", &hash, "css");
            if let Some(parent) = out_dir.join(&css_name).parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(out_dir.join(&css_name), &combined)?;
            emitted.push((css_name.clone(), combined.len()));
            for entry in &mut manifest_entries {
                if entry.is_entry {
                    entry.css.push(css_name.clone());
                }
            }
            for (src, _) in &sources {
                push_css_manifest_row(&mut manifest_assets, &root, src, &css_name);
            }
            Some(css_name)
        }
    } else {
        None
    };
    // Split CSS is emitted once for the whole bundle; each page then links the
    // stylesheets of its own entry chunks (a page-relative href when the base is
    // relative).
    let chunk_css: Vec<(String, String)> = if css_split {
        emit_split_css_files(
            &output,
            &collected_css,
            &out_dir,
            &root,
            &base,
            css_asset_opts,
            &mut emitted,
            &mut manifest_entries,
            &mut manifest_assets,
        )?
    } else {
        Vec::new()
    };
    let mut all_sync_chunks: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if let Some(name) = oj_config::ssr_manifest_name(&config) {
        let manifest = ssr_manifest(
            &output,
            &root,
            &base,
            &chunk_css,
            &combined_css_name,
            &imports_map,
        );
        let dest = out_dir.join(&name);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&dest, serde_json::to_string_pretty(&manifest)?)?;
    }

    // Pages a plugin emitted as `.html` chunks during the build are rendered
    // here alongside the statically-resolved input pages.
    {
        let mut emitted_pages = emit.html_docs.lock().unwrap();
        html_docs.append(&mut emitted_pages);
    }

    // Page script entries across all documents: the only chunks that get the
    // modulepreload polyfill (a worker or plugin-emitted entry has no document).
    let mut page_entry_files: Vec<String> = Vec::new();
    // Vite's build transformIndexHtml ctx (html.ts) carries the output bundle
    // and the page's entry chunk; serialized once for every page, and not at
    // all when no plugin has the hook (the bundle JSON carries every chunk's
    // full code).
    let html_hook_on = match &plugin_host {
        Some(host) => host.has_transform_index_html().await,
        None => false,
    };
    let html_bundle: Option<serde_json::Value> = if html_hook_on {
        serde_json::from_str(&serialize_bundle(&output.assets)).ok()
    } else {
        None
    };
    for doc in &html_docs {
        let mut rewritten_html = oj_env::replace_html_env(&doc.src_html, &html_env);
        let page_base = page_base(&base, &doc.out_rel);
        // Rewrite each `<script src>` to its hashed output chunk and collect
        // this page's own entry chunks (for modulepreload and split CSS).
        let mut doc_entry_files: Vec<String> = Vec::new();
        for s in &doc.scripts {
            if let Some(file) = facade_to_file.get(&s.abs) {
                let out_src = with_base(file, &page_base);
                rewritten_html = rewritten_html.replace(&s.src, &out_src);
                rewritten_html = add_script_crossorigin(&rewritten_html, &out_src);
                doc_entry_files.push(file.clone());
            }
        }
        page_entry_files.extend(doc_entry_files.iter().cloned());
        for entry in &doc_entry_files {
            all_sync_chunks.insert(entry.clone());
            all_sync_chunks.extend(transitive_imports(entry, &imports_map));
        }

        // Inject <link rel="modulepreload"> for this page's transitively
        // imported chunks so the browser fetches them in parallel.
        let mut preloads: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        for entry in &doc_entry_files {
            for dep in transitive_imports(entry, &imports_map) {
                preloads.insert(dep);
            }
        }
        if !preloads.is_empty() && oj_config::module_preload_links(&config) {
            let links = preloads
                .iter()
                .map(|f| {
                    format!(
                        "<link rel=\"modulepreload\" href=\"{}\" crossorigin />",
                        with_base(f, &page_base)
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            rewritten_html = insert_before_head(&rewritten_html, &links);
        }

        // `<link rel="stylesheet" href>` to a local file (relative or root-absolute)
        // goes through the same pipeline as an imported stylesheet (Sass/PostCSS/
        // Tailwind, lightningcss, url() assets) and is emitted hashed, as in Vite.
        {
            let mut seen_link_assets: std::collections::HashMap<PathBuf, String> =
                std::collections::HashMap::new();
            for href in stylesheet_hrefs(&doc.src_html) {
                let src = resolve_html_ref(&href, &doc.dir, &root);
                if !src.is_file() {
                    continue;
                }
                let src_str = src.to_string_lossy().into_owned();
                let out = compile_stylesheet(
                    &root,
                    &plugin_host,
                    &link_css_transform_enabled,
                    has_postcss,
                    &config.css,
                    &link_css_resolve,
                    &src_str,
                    &src_str,
                )
                .await
                .with_context(|| format!("stylesheet {href} linked from {}", doc.out_rel))?;
                let dir = src
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.clone());
                let css = rebase_css_urls(
                    &out.css,
                    &dir,
                    &out_dir,
                    &base,
                    css_asset_opts,
                    &mut emitted,
                    &mut seen_link_assets,
                );
                let hash = content_hash(css.as_bytes());
                let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("style");
                let name = render_asset_name(
                    css_asset_opts.asset_names,
                    &sanitize_asset_name(stem),
                    &hash,
                    "css",
                );
                let dest = out_dir.join(&name);
                if let Some(parent) = dest.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&dest, &css)?;
                emitted.push((name.clone(), css.len()));
                push_css_manifest_row(&mut manifest_assets, &root, &src_str, &name);
                rewritten_html =
                    rewrite_link_hrefs(&rewritten_html, &href, &with_base(&name, &base));
            }
        }

        // Every other asset the page references by attribute (img src/srcset,
        // link icon/manifest href, video/audio/source/track, meta og:image...)
        // is emitted hashed or inlined, and a publicDir reference gets the base
        // prefix, as Vite's build-html plugin does.
        {
            let mut seen_html_assets: std::collections::HashMap<PathBuf, String> =
                std::collections::HashMap::new();
            rewritten_html =
                rewrite_html_asset_attrs(&rewritten_html, |tag, tag_name, _attr, value, srcset| {
                    let mut one = |url: &str| {
                        html_asset_url(
                            tag,
                            tag_name,
                            url,
                            &doc.dir,
                            &root,
                            public_dir.as_deref(),
                            &out_dir,
                            &page_base,
                            css_asset_opts,
                            &mut emitted,
                            &mut seen_html_assets,
                        )
                    };
                    if srcset {
                        rewrite_srcset(value, one)
                    } else {
                        one(value)
                    }
                });
        }

        if let Some(css_name) = &combined_css_name {
            let link = format!(
                "<link rel=\"stylesheet\" href=\"{}\" crossorigin />",
                with_base(css_name, &page_base)
            );
            rewritten_html = insert_before_head(&rewritten_html, &link);
        } else if css_split {
            let links = split_css_links(&chunk_css, &doc_entry_files, &imports_map, &page_base);
            if !links.is_empty() {
                rewritten_html = insert_before_head(&rewritten_html, &links);
            }
        }

        if let Some(host) = plugin_host.as_ref().filter(|_| html_hook_on) {
            // Vite (html.ts): `{ path: "/" + relative path, filename: <source
            // html>, bundle, chunk }`, and a throwing hook fails the build.
            let page_file = doc
                .dir
                .join(Path::new(&doc.out_rel).file_name().unwrap_or_default());
            let chunk = html_bundle
                .as_ref()
                .zip(doc_entry_files.first())
                .and_then(|(b, f)| b.get(f.as_str()).cloned())
                .unwrap_or(serde_json::Value::Null);
            let ctx = serde_json::json!({
                "path": format!("/{}", doc.out_rel),
                "filename": page_file.display().to_string(),
                "bundle": html_bundle.clone().unwrap_or(serde_json::Value::Object(Default::default())),
                "chunk": chunk,
            })
            .to_string();
            rewritten_html = host
                .transform_index_html(&rewritten_html, &ctx)
                .await
                .map_err(|e| {
                    anyhow::anyhow!("plugin transformIndexHtml failed for {}:\n{e}", doc.out_rel)
                })?;
        }
        if let Some(nonce) = oj_config::html_csp_nonce(&config) {
            rewritten_html = oj_server::inject_csp_nonce(&rewritten_html, &nonce);
        }
        let dest = out_dir.join(&doc.out_rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        emitted.push((doc.out_rel.clone(), rewritten_html.len()));
        fs::write(&dest, rewritten_html)?;
    }

    // Vite's build import analysis: wrap dynamic imports in `__vitePreload` so a
    // lazy chunk's own dependencies and stylesheets load in parallel (and its
    // CSS is applied before it runs), inject the modulepreload polyfill into
    // page entries, and give async-only chunks a stylesheet fallback.
    if es_output {
        apply_preload_helper(
            &output,
            &out_dir,
            &base,
            &imports_map,
            &chunk_css,
            &all_sync_chunks,
            &page_entry_files,
            oj_config::module_preload_polyfill(&config),
            out_banner.as_deref(),
            &worker_entries.lock().unwrap().clone(),
        )?;
    }

    // Vite writes the manifest only when `build.manifest` is set (a name or `true`).
    if let Some(name) = oj_config::build_manifest_name(&config) {
        manifest_asset_rows(&output.assets, &mut manifest_entries, &mut manifest_assets);
        let dest = out_dir.join(&name);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(
            &dest,
            serde_json::to_string_pretty(&build_manifest(&manifest_entries, &manifest_assets))?,
        )?;
    }

    if let (true, Some(public_dir)) = (build_cfg.copy_public_dir.unwrap_or(true), &public_dir) {
        copy_public_dir(public_dir, &out_dir)?;
    }

    println!(
        "{} build: {} in {:?}",
        oj_server::oj_brand(),
        out_dir.display(),
        started.elapsed()
    );
    report_build_output(
        &out_dir,
        &mut emitted,
        oj_config::build_report_compressed_size(&config),
    );
    // Vite's reporter: with minification on and no lib, a client chunk over
    // `build.chunkSizeWarningLimit` kB gets the code-splitting hint.
    if client_minify {
        let limit = oj_config::build_chunk_size_warning_limit(&config);
        let large = output.assets.iter().any(|a| match a {
            rolldown_common::Output::Chunk(c) => (c.code.len() as f64) / 1000.0 > limit,
            _ => false,
        });
        if large {
            eprintln!("{}", chunk_size_warning(limit));
        }
    }
    Ok(())
}

/// The per-file report, largest first, with Vite's gzip column
/// (`build.reportCompressedSize`, default on). Source maps are listed by size
/// only, as in Vite.
fn report_build_output(out_dir: &Path, emitted: &mut [(String, usize)], report_gzip: bool) {
    emitted.sort_by_key(|e| std::cmp::Reverse(e.1));
    let width = emitted.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    for (name, bytes) in emitted.iter() {
        let gzip = if report_gzip && !name.ends_with(".map") {
            fs::read(out_dir.join(name))
                .ok()
                .map(|data| format!("  │ gzip: {:>8}", human_bytes(gzip_size(&data))))
                .unwrap_or_default()
        } else {
            String::new()
        };
        println!("  {:>9}  {name:<width$}{gzip}", human_bytes(*bytes));
    }
}

fn gzip_size(data: &[u8]) -> usize {
    use std::io::Write;
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    if enc.write_all(data).is_err() {
        return 0;
    }
    enc.finish().map(|v| v.len()).unwrap_or(0)
}

/// Vite's chunk size warning text (reporter.ts).
fn chunk_size_warning(limit_kb: f64) -> String {
    format!(
        "\n(!) Some chunks are larger than {limit_kb} kB after minification. Consider:\n\
         - Using dynamic import() to code-split the application\n\
         - Use build.rollupOptions.output.manualChunks to improve chunking: https://rollupjs.org/configuration-options/#output-manualchunks\n\
         - Adjust chunk size limit for this warning via build.chunkSizeWarningLimit."
    )
}

fn human_bytes(bytes: usize) -> String {
    if bytes >= 1_048_576 {
        format!("{:.1}MB", bytes as f64 / 1_048_576.0)
    } else if bytes >= 1024 {
        format!("{:.1}kB", bytes as f64 / 1024.0)
    } else {
        format!("{bytes}B")
    }
}

fn module_script_srcs(html: &str) -> Vec<String> {
    scan_attrs(html, "<script", "src")
        .into_iter()
        .filter(|src| oj_server::html_entry_src(src).is_some())
        .collect()
}

#[derive(Debug)]
struct HtmlScript {
    src: String,
    abs: PathBuf,
}

#[derive(Debug)]
struct HtmlDoc {
    out_rel: String,
    src_html: String,
    scripts: Vec<HtmlScript>,
    /// Directory of the source page; relative `<link href>`s resolve against it.
    dir: PathBuf,
}

/// Inline `<script type="module">…</script>` blocks (no `src`): the byte range
/// of the whole element and its body.
fn inline_module_scripts(html: &str) -> Vec<(usize, usize, String)> {
    let mut out = Vec::new();
    for (start, _) in html.match_indices("<script") {
        let Some(tag_end) = html[start..].find('>') else {
            continue;
        };
        let tag = &html[start..start + tag_end];
        // Attribute values compare quote-agnostically (`type='module'`,
        // `type=module`), as an HTML parser sees them.
        if !html_attr(tag, "type").is_some_and(|t| t.eq_ignore_ascii_case("module"))
            || html_attr(tag, "src").is_some()
            || tag.ends_with('/')
        {
            continue;
        }
        let body_start = start + tag_end + 1;
        let Some(close) = html[body_start..].find("</script>") else {
            continue;
        };
        let body = html[body_start..body_start + close].to_string();
        if body.trim().is_empty() {
            continue;
        }
        out.push((start, body_start + close + "</script>".len(), body));
    }
    out
}

/// Vite's html-proxy: each inline module script becomes its own entry
/// (`<page>?html-proxy&index=N.js`, whose relative imports resolve against the
/// page's directory) and the tag is rewritten to a `src` placeholder that the
/// post-bundle HTML rewrite replaces with the hashed chunk. Returns the rewritten
/// html and `(placeholder src, virtual id, body)` per script.
fn externalize_inline_scripts(
    html: &str,
    html_abs: &Path,
) -> (String, Vec<(String, String, String)>) {
    let blocks = inline_module_scripts(html);
    if blocks.is_empty() {
        return (html.to_string(), Vec::new());
    }
    let mut out = html.to_string();
    let mut entries = Vec::new();
    for (n, (start, end, body)) in blocks.iter().enumerate().rev() {
        let placeholder = format!("/@oj-inline/{n}.js");
        let id = format!("{}?html-proxy&index={n}.js", html_abs.display());
        out.replace_range(
            *start..*end,
            &format!("<script type=\"module\" src=\"{placeholder}\"></script>"),
        );
        entries.push((placeholder, id, body.clone()));
    }
    entries.reverse();
    (out, entries)
}

/// `href`s of `<link rel="stylesheet">` tags that name a local file (relative or
/// root-absolute), in document order.
fn stylesheet_hrefs(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (start, _) in html.match_indices("<link") {
        let Some(end) = html[start..].find('>') else {
            continue;
        };
        let tag = &html[start..start + end];
        // Quoted or unquoted attributes, any case (html_attr is the shared parser).
        if !html_attr(tag, "rel").is_some_and(|v| v.eq_ignore_ascii_case("stylesheet")) {
            continue;
        }
        let Some(href) = html_attr(tag, "href") else {
            continue;
        };
        if href.is_empty()
            || href.starts_with("http://")
            || href.starts_with("https://")
            || href.starts_with("//")
            || href.starts_with("data:")
        {
            continue;
        }
        out.push(href.to_string());
    }
    out
}

fn normalize_input_entries(input: &serde_json::Value) -> Vec<(String, String)> {
    let name_of = |p: &str| {
        Path::new(p)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("index")
            .to_string()
    };
    match input {
        serde_json::Value::String(s) => vec![(name_of(s), s.clone())],
        serde_json::Value::Array(arr) => arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(|s| (name_of(s), s.to_string()))
            .collect(),
        serde_json::Value::Object(map) => map
            .iter()
            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
            .collect(),
        _ => Vec::new(),
    }
}

fn resolve_html_ref(src: &str, html_dir: &Path, root: &Path) -> PathBuf {
    let s = src.trim();
    let s = s.strip_prefix("./").unwrap_or(s);
    let joined = match s.strip_prefix('/') {
        Some(rest) => root.join(rest),
        None => html_dir.join(s),
    };
    // Lexically resolve `..` so a page-relative `../src/x.js` matches the
    // normalized facade id rolldown reports for the chunk (and its `<script>`
    // tag gets rewritten).
    let mut out = PathBuf::new();
    for c in joined.components() {
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

/// Vite emits every entry `<script type="module">` with `crossorigin` (module
/// scripts fetch in CORS mode anyway, and only a crossorigin tag can be
/// preloaded with `<link rel="preload">`); add it to the tag whose `src` is
/// `src` unless the page already set it.
fn add_script_crossorigin(html: &str, src: &str) -> String {
    let mut out = String::with_capacity(html.len() + 16);
    let mut rest = html;
    while let Some(start) = rest.find("<script") {
        let Some(end) = rest[start..].find('>') else {
            break;
        };
        let tag = &rest[start..start + end];
        let has_src = html_attr(tag, "src") == Some(src);
        out.push_str(&rest[..start + end]);
        if has_src && !has_bare_attr(tag, "crossorigin") && html_attr(tag, "crossorigin").is_none()
        {
            out.push_str(" crossorigin");
        }
        rest = &rest[start + end..];
    }
    out.push_str(rest);
    out
}

fn insert_before_head(html: &str, snippet: &str) -> String {
    match html.find("</head>") {
        Some(i) => format!("{}{}\n{}", &html[..i], snippet, &html[i..]),
        None => format!("{snippet}\n{html}"),
    }
}

/// Rewrite the `href` of every `<link>` whose value is `from`, keeping the tag's
/// own quoting (single, double or none), so a bundled stylesheet link is updated
/// however the page spelled it.
fn rewrite_link_hrefs(html: &str, from: &str, to: &str) -> String {
    let mut out = String::with_capacity(html.len() + to.len());
    let mut last = 0;
    for (start, _) in html.match_indices("<link") {
        if start < last {
            continue;
        }
        let Some(end) = html[start..].find('>') else {
            continue;
        };
        let tag = &html[start..start + end];
        let Some(value) = html_attr(tag, "href") else {
            continue;
        };
        if value != from {
            continue;
        }
        let offset = value.as_ptr() as usize - tag.as_ptr() as usize;
        out.push_str(&html[last..start + offset]);
        out.push_str(to);
        last = start + offset + value.len();
    }
    out.push_str(&html[last..]);
    out
}

/// Vite's `DEFAULT_HTML_ASSET_SOURCES` (assetSource.ts): per element, the
/// attributes holding one asset URL and the ones holding a srcset.
fn html_asset_attrs(tag_name: &str) -> Option<(&'static [&'static str], &'static [&'static str])> {
    Some(match tag_name {
        "audio" | "embed" | "input" | "track" => (&["src"], &[]),
        "img" => (&["src"], &["srcset"]),
        "image" | "use" => (&["href", "xlink:href"], &[]),
        "link" => (&["href"], &["imagesrcset"]),
        "object" => (&["data"], &[]),
        "source" => (&["src"], &["srcset"]),
        "video" => (&["src", "poster"], &[]),
        "meta" => (&["content"], &[]),
        _ => return None,
    })
}

/// Vite only treats `<meta content>` as an asset for these `name`/`property`
/// values (msapplication tiles, twitter:image, Open Graph media).
fn meta_content_is_asset(tag: &str) -> bool {
    const NAMES: [&str; 7] = [
        "msapplication-tileimage",
        "msapplication-square70x70logo",
        "msapplication-square150x150logo",
        "msapplication-wide310x150logo",
        "msapplication-square310x310logo",
        "msapplication-config",
        "twitter:image",
    ];
    const PROPERTIES: [&str; 7] = [
        "og:image",
        "og:image:url",
        "og:image:secure_url",
        "og:audio",
        "og:audio:secure_url",
        "og:video",
        "og:video:secure_url",
    ];
    let matches = |attr: &str, allowed: &[&str]| {
        html_attr(tag, attr)
            .map(|v| v.trim().to_ascii_lowercase())
            .is_some_and(|v| allowed.contains(&v.as_str()))
    };
    matches("name", &NAMES) || matches("property", &PROPERTIES)
}

/// A bare (valueless) attribute such as `vite-ignore`.
fn has_bare_attr(tag: &str, name: &str) -> bool {
    tag.split_ascii_whitespace().skip(1).any(|tok| {
        tok.eq_ignore_ascii_case(name)
            || tok
                .get(..name.len() + 1)
                .is_some_and(|p| p.eq_ignore_ascii_case(&format!("{name}=")))
    })
}

/// The `<link rel>` values Vite never inlines as a data URL (html.ts
/// `noInlineLinkRels`): a favicon or manifest must stay a real file.
fn link_rel_forbids_inline(tag: &str) -> bool {
    html_attr(tag, "rel").is_some_and(|rel| {
        rel.split_ascii_whitespace().any(|r| {
            matches!(
                r.to_ascii_lowercase().as_str(),
                "icon" | "apple-touch-icon" | "apple-touch-startup-image" | "manifest"
            )
        })
    })
}

/// Vite's `isCSSRequest`.
fn is_css_request(url: &str) -> bool {
    let clean = url.split(['?', '#']).next().unwrap_or(url);
    let ext = clean.rsplit_once('.').map(|(_, e)| e).unwrap_or("");
    matches!(
        ext,
        "css" | "less" | "sass" | "scss" | "styl" | "stylus" | "pcss" | "postcss" | "sss"
    )
}

/// Rewrite the value of each srcset candidate (`url descriptor, ...`) with `f`;
/// unchanged when no candidate changes.
fn rewrite_srcset(value: &str, mut f: impl FnMut(&str) -> Option<String>) -> Option<String> {
    let mut changed = false;
    let candidates: Vec<String> = value
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .map(|c| {
            let (url, descriptor) = match c.split_once(char::is_whitespace) {
                Some((u, d)) => (u, d.trim()),
                None => (c, ""),
            };
            let url = match f(url) {
                Some(u) => {
                    changed = true;
                    u
                }
                None => url.to_string(),
            };
            if descriptor.is_empty() {
                url
            } else {
                format!("{url} {descriptor}")
            }
        })
        .collect();
    changed.then(|| candidates.join(", "))
}

/// Vite's build-html asset pass (html.ts, `getNodeAssetAttributes`): every
/// element whose attributes reference an asset (img src/srcset, link href/
/// imagesrcset, video src/poster, source, audio, track, object data, use/image
/// href, allowed meta content) gets those values rewritten by `rewrite(tag,
/// tag_name, attr, value)`, keeping the page's own quoting. A `vite-ignore`
/// attribute skips the element and is removed.
fn rewrite_html_asset_attrs(
    html: &str,
    mut rewrite: impl FnMut(&str, &str, &str, &str, bool) -> Option<String>,
) -> String {
    let mut out = String::with_capacity(html.len());
    let mut last = 0;
    let bytes = html.as_bytes();
    for (start, _) in html.match_indices('<') {
        if start < last {
            continue;
        }
        let name_end = html[start + 1..]
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == ':' || c == '-'))
            .map(|i| start + 1 + i)
            .unwrap_or(html.len());
        if name_end == start + 1
            || name_end >= bytes.len()
            || !matches!(bytes[name_end], b' ' | b'\t' | b'\n' | b'\r' | b'/' | b'>')
        {
            continue;
        }
        let tag_name = html[start + 1..name_end].to_ascii_lowercase();
        let Some((src_attrs, srcset_attrs)) = html_asset_attrs(&tag_name) else {
            continue;
        };
        let Some(end) = html[start..].find('>') else {
            continue;
        };
        let tag = &html[start..start + end];
        // (offset within tag, length, replacement)
        let mut edits: Vec<(usize, usize, String)> = Vec::new();
        if has_bare_attr(tag, "vite-ignore") {
            if let Some(pos) = tag.to_ascii_lowercase().find("vite-ignore") {
                let mut cut_start = pos;
                while cut_start > 0 && tag.as_bytes()[cut_start - 1].is_ascii_whitespace() {
                    cut_start -= 1;
                }
                let cut_end = tag[pos..]
                    .find(char::is_whitespace)
                    .map(|i| pos + i)
                    .unwrap_or(tag.len());
                edits.push((cut_start, cut_end - cut_start, String::new()));
            }
        } else if tag_name != "meta" || meta_content_is_asset(tag) {
            for (attrs, srcset) in [(src_attrs, false), (srcset_attrs, true)] {
                for attr in attrs.iter() {
                    let Some(value) = html_attr(tag, attr) else {
                        continue;
                    };
                    if let Some(new) = rewrite(tag, &tag_name, attr, value, srcset) {
                        let offset = value.as_ptr() as usize - tag.as_ptr() as usize;
                        edits.push((offset, value.len(), new));
                    }
                }
            }
        }
        if edits.is_empty() {
            continue;
        }
        edits.sort_by_key(|e| e.0);
        out.push_str(&html[last..start]);
        let mut cursor = 0;
        for (offset, len, new) in edits {
            out.push_str(&tag[cursor..offset]);
            out.push_str(&new);
            cursor = offset + len;
        }
        out.push_str(&tag[cursor..]);
        last = start + end;
    }
    out.push_str(&html[last..]);
    out
}

/// Vite's `isExcludedUrl` for html asset references: fragments, data URLs and
/// external (`scheme://`, `//`) URLs are left alone.
fn is_excluded_html_url(url: &str) -> bool {
    url.is_empty()
        || url.starts_with('#')
        || url.starts_with("data:")
        || url.starts_with("//")
        || url.split_once("://").is_some_and(|(scheme, _)| {
            !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphabetic())
        })
}

/// Emit one html-referenced source asset (hashed under `assetFileNames`, or
/// inlined as a data URL when small and inlining is allowed) and return its URL
/// from the page. `seen` dedupes by resolved path across the page's references.
#[allow(clippy::too_many_arguments)]
fn emit_html_asset(
    abs: &Path,
    suffix: &str,
    out_dir: &Path,
    page_base: &str,
    opts: CssAssetOpts<'_>,
    no_inline: bool,
    emitted: &mut Vec<(String, usize)>,
    seen: &mut std::collections::HashMap<PathBuf, String>,
) -> Option<String> {
    let abs = abs.canonicalize().ok()?;
    if let Some(url) = seen.get(&abs) {
        return Some(format!("{url}{suffix}"));
    }
    let data = std::fs::read(&abs).ok()?;
    let ext = abs.extension().and_then(|s| s.to_str()).unwrap_or("");
    // Vite's shouldInline: never an .html, never a link icon/manifest, never an
    // svg addressed by fragment; otherwise anything under assetsInlineLimit.
    if !(no_inline
        || ext.eq_ignore_ascii_case("html")
        || (data.len() as u64) >= opts.inline_limit
        || (ext.eq_ignore_ascii_case("svg") && suffix.starts_with('#')))
    {
        if ext.eq_ignore_ascii_case("svg") {
            return Some(svg_data_url(&String::from_utf8_lossy(&data)));
        }
        return Some(format!("data:{};base64,{}", asset_mime(ext), b64(&data)));
    }
    let hash = content_hash(&data);
    let stem = abs.file_stem().and_then(|s| s.to_str()).unwrap_or("asset");
    let name = render_asset_name(opts.asset_names, &sanitize_asset_name(stem), &hash, ext);
    let dest = out_dir.join(&name);
    if let Some(p) = dest.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    std::fs::write(&dest, &data).ok()?;
    emitted.push((name.clone(), data.len()));
    let url = with_base(&name, page_base);
    seen.insert(abs, url.clone());
    Some(format!("{url}{suffix}"))
}

/// The new value for one html asset reference (html.ts asset attribute branch):
/// a `publicDir` file keeps its path under the base; a bundle-relative source
/// file is emitted hashed (or inlined); a stylesheet `<link>` is left to the
/// stylesheet pass; anything else (external, data:, missing) stays as written.
#[allow(clippy::too_many_arguments)]
fn html_asset_url(
    tag: &str,
    tag_name: &str,
    value: &str,
    html_dir: &Path,
    root: &Path,
    public_dir: Option<&Path>,
    out_dir: &Path,
    page_base: &str,
    opts: CssAssetOpts<'_>,
    emitted: &mut Vec<(String, usize)>,
    seen: &mut std::collections::HashMap<PathBuf, String>,
) -> Option<String> {
    if is_excluded_html_url(value) {
        return None;
    }
    let cut = value.find(['?', '#']).unwrap_or(value.len());
    let (clean, query) = value.split_at(cut);
    // Vite keeps only a `#fragment` on an emitted asset; the public path keeps its
    // query as written.
    let fragment = query.find('#').map(|i| &query[i..]).unwrap_or("");
    if let (Some(rest), Some(public_dir)) = (clean.strip_prefix('/'), public_dir) {
        if public_dir.join(rest).is_file() {
            return Some(format!("{}{query}", with_base(clean, page_base)));
        }
    }
    if tag_name == "link"
        && is_css_request(clean)
        && html_attr(tag, "media").is_none()
        && html_attr(tag, "disabled").is_none()
        && !has_bare_attr(tag, "disabled")
    {
        return None;
    }
    let abs = resolve_html_ref(clean, html_dir, root);
    if !abs.is_file() {
        return None;
    }
    let no_inline = tag_name == "link" && link_rel_forbids_inline(tag);
    emit_html_asset(
        &abs, fragment, out_dir, page_base, opts, no_inline, emitted, seen,
    )
}

use oj_compiler::html::html_attr;

fn scan_attrs(html: &str, tag_prefix: &str, attr_name: &str) -> Vec<String> {
    let mut values = Vec::new();
    for (start, _) in html.match_indices(tag_prefix) {
        let Some(end) = html[start..].find('>') else {
            continue;
        };
        let tag = &html[start..start + end];
        if tag_prefix == "<script"
            && !html_attr(tag, "type").is_some_and(|value| value.eq_ignore_ascii_case("module"))
        {
            continue;
        }
        if let Some(value) = html_attr(tag, attr_name) {
            values.push(value.to_string());
        }
    }
    values
}

pub(crate) async fn build_ssr(
    root: &Path,
    out_dir: &Path,
    entry: &str,
    mode: &str,
    sourcemap: oj_config::Sourcemap,
) -> anyhow::Result<()> {
    use rolldown::{IsExternal, Platform};

    let entry_import = if entry.starts_with('.') {
        entry.to_string()
    } else {
        format!("./{entry}")
    };
    let stem = Path::new(entry)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("server")
        .to_string();

    fs::create_dir_all(out_dir)?;
    let started = Instant::now();
    let collected_css: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

    let mut config =
        oj_config::load_with(root, "build", mode).map_err(|e| anyhow::anyhow!("{e}"))?;
    oj_server::plugins::adopt_vite_config_values(&mut config, root, "build", mode)
        .map_err(|e| anyhow::anyhow!(e))?;
    apply_cli_options(&mut config);
    let loaded_env = oj_env::load(&env_dir_of(root, &config), mode);
    let env_prefixes = env_prefixes_of(&config);
    let env_prefix_refs: Vec<&str> = env_prefixes.iter().map(String::as_str).collect();
    let node_env = oj_env::resolve_node_env(shell_node_env().as_deref(), &loaded_env, "production");
    let is_production = node_env == "production";
    let ssr_base = config.base.clone().unwrap_or_else(|| "/".into());
    let plugin_host = user_plugin_host(
        root,
        &ssr_base,
        &serde_json::json!(config.define),
        &serde_json::json!(config.environments),
        "ssr",
        mode,
    )
    .await;
    let plugin_defines = plugin_config_defines(&plugin_host).await;

    // Vite's SSR externalization: dependencies stay external unless
    // `ssr.noExternal` (or a webworker target) bundles them; `ssr.external` wins.
    let externals = Arc::new(oj_config::ssr_externals(&config));
    let external = IsExternal::Fn(Some(Arc::new({
        let externals = Arc::clone(&externals);
        let node_modules = root.join("node_modules");
        move |spec: &str, _importer, is_resolved: bool| {
            let ext = if is_resolved {
                // Fallback for dependencies reached some other way (a plugin
                // resolution, a nested node_modules); kept as their resolved path.
                spec.contains("node_modules")
                    && oj_config::SsrExternals::package_of_path(spec)
                        .is_none_or(|pkg| externals.is_external_pkg(pkg))
            } else {
                // Externalize a dependency by its bare specifier, as Vite does, so the
                // server bundle imports `react`, not an absolute path. Only a name that
                // is an installed package qualifies; an alias or a plugin virtual that
                // merely looks bare resolves and bundles as usual.
                let bare = !spec.starts_with('.')
                    && !spec.starts_with('/')
                    && !spec.starts_with('\0')
                    && !spec.contains(':');
                bare && {
                    let pkg = oj_config::SsrExternals::package_of(spec);
                    externals.is_external_pkg(pkg)
                        && node_modules.join(pkg).join("package.json").is_file()
                }
            };
            Box::pin(async move { Ok(ext) })
        }
    })));

    let emit = Arc::new(EmitState::new(root.to_path_buf()));
    let mut oj_plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(host) = &plugin_host {
        oj_plugins.push(Arc::new(OjUserPlugin::new(
            Arc::clone(host),
            Arc::clone(&emit),
            host.build_hook_plan().await,
        )));
    }
    oj_plugins.push(Arc::new(OjCssPlugin {
        collected: Arc::clone(&collected_css),
        root: root.to_path_buf(),
        has_postcss: oj_server::has_postcss_config(root),
        inline_limit: assets_inline_limit_of(&config),
        client: false,
        css_code_split: false,
        resolve: css_resolve_of(root, &config, "ssr"),
        host: plugin_host.clone(),
        css_transform_enabled: Arc::new(tokio::sync::OnceCell::new()),
        css: config.css.clone(),
        html_inline: Arc::new(Mutex::new(std::collections::HashMap::new())),
        worker: None,
        worker_entries: Arc::new(Mutex::new(std::collections::HashSet::new())),
    }));
    let mut bundler = BundlerBuilder::default()
        .with_plugins(oj_plugins)
        .with_options(BundlerOptions {
            input: Some(vec![InputItem {
                name: Some(stem.clone()),
                import: entry_import,
            }]),
            cwd: Some(root.to_path_buf()),
            dir: Some(out_dir.display().to_string()),
            resolve: rolldown_resolve(root, &config, "ssr"),
            transform: transform_options(&config, is_production),
            platform: Some(if externals.webworker() {
                Platform::Browser
            } else {
                Platform::Node
            }),
            external: Some(external),
            format: Some(OutputFormat::Esm),
            preserve_entry_signatures: Some(
                rolldown_common::PreserveEntrySignatures::AllowExtension,
            ),
            entry_filenames: Some(format!("{stem}.mjs").into()),
            chunk_filenames: Some(format!("{stem}-[hash].mjs").into()),
            minify: Some(rolldown_minify(
                oj_config::environment_build_bool(&config, "ssr", "minify").unwrap_or(false),
            )),
            sourcemap: env_sourcemap(&config, "ssr", sourcemap),
            define: Some({
                let env = oj_env::with_process_env(
                    loaded_env.clone(),
                    std::env::vars(),
                    &env_prefix_refs,
                );
                // `keepProcessEnv` defaults to true for a server consumer, so only
                // the webworker target gets Vite's `process.env` -> `{}` defines.
                let mut pairs = if externals.webworker() {
                    process_env_defines(&node_env)
                } else {
                    node_env_defines(&node_env)
                };
                pairs.push(import_meta_hot_define());
                pairs.extend(oj_env::import_meta_env_defines_with(
                    &env,
                    mode,
                    !is_production,
                    &ssr_base,
                    &env_prefix_refs,
                    true,
                ));
                pairs.extend(oj_config::config_defines(&config));
                pairs.extend(oj_config::environment_defines(&config, "ssr"));
                pairs.extend(plugin_defines);
                pairs.into_iter().collect()
            }),
            ..Default::default()
        })
        .build()
        .map_err(rolldown_failure!("rolldown init failed"))?;

    let output = match bundler
        .write()
        .await
        .map_err(rolldown_failure!("ssr build failed"))
    {
        Ok(output) => output,
        Err(err) => return Err(fail_with_plugin_hooks(err, &plugin_host).await),
    };
    bundler
        .close()
        .await
        .map_err(|errs| anyhow::anyhow!("ssr close failed:\n{errs:?}"))?;

    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_end(None).await {
            bail!("plugin buildEnd failed (ssr):\n{e}");
        }
    }

    let mut emitted: Vec<(String, usize)> = Vec::new();
    for asset in &output.assets {
        if let rolldown_common::Output::Chunk(c) = asset {
            emitted.push((c.filename.to_string(), c.code.len()));
        }
    }
    println!(
        "oj build (ssr): {} in {:?}",
        out_dir.display(),
        started.elapsed()
    );
    emitted.sort_by_key(|e| std::cmp::Reverse(e.1));
    for (name, bytes) in &emitted {
        println!("  {:>9}  {}", human_bytes(*bytes), name);
    }
    Ok(())
}

const OJ_SERVER_FNS_JS: &str = r#"const mods = import.meta.glob("./src/**/*.server.*");
const norm = (s) => String(s).replace(/^\.?\/+/, "");
export async function dispatch(url, name, args) {
  const want = norm(url);
  const key = Object.keys(mods).find((k) => norm(k) === want);
  if (!key) throw new Error("oj: no server module " + url);
  const m = await mods[key]();
  const fn = name === "default" ? m.default : m[name];
  if (typeof fn !== "function") throw new Error("oj: no server function " + name + " in " + url);
  return fn(...(Array.isArray(args) ? args : []));
}
"#;

fn has_server_modules(root: &Path) -> bool {
    fn walk(dir: &Path) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                if walk(&p) {
                    return true;
                }
            } else if p.file_name().and_then(|n| n.to_str()).is_some_and(|n| {
                [".server.ts", ".server.tsx", ".server.js", ".server.jsx"]
                    .iter()
                    .any(|s| n.ends_with(s))
            }) {
                return true;
            }
        }
        false
    }
    walk(&root.join("src"))
}

async fn build_server_fns(root: &Path, out_dir: &Path, mode: &str) -> anyhow::Result<()> {
    use rolldown::{IsExternal, Platform};
    let mut config =
        oj_config::load_with(root, "build", mode).map_err(|e| anyhow::anyhow!("{e}"))?;
    oj_server::plugins::adopt_vite_config_values(&mut config, root, "build", mode)
        .map_err(|e| anyhow::anyhow!(e))?;
    apply_cli_options(&mut config);
    let node_env = oj_env::resolve_node_env(
        shell_node_env().as_deref(),
        &oj_env::load(&env_dir_of(root, &config), mode),
        "production",
    );
    let is_production = node_env == "production";
    let entry_path = root.join("_oj_server_fns_entry.tsx");
    fs::write(&entry_path, OJ_SERVER_FNS_JS)?;
    let collected: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let ext_rule = Arc::new(oj_config::ssr_externals(&config));
    let external = IsExternal::Fn(Some(Arc::new(move |spec: &str, _i, resolved: bool| {
        let in_node_modules = resolved && spec.contains("node_modules");
        let ext = ext_rule.is_external(spec, in_node_modules).unwrap_or(false);
        Box::pin(async move { Ok(ext) })
    })));
    let result = async {
        let mut bundler = BundlerBuilder::default()
            .with_plugins(vec![Arc::new(OjCssPlugin {
                collected: Arc::clone(&collected),
                root: root.to_path_buf(),
                has_postcss: oj_server::has_postcss_config(root),
                inline_limit: 4096,
                client: false,
                resolve: css_resolve_of(root, &config, "ssr"),
                css_code_split: false,
                host: None,
                css_transform_enabled: Arc::new(tokio::sync::OnceCell::new()),
                css: config.css.clone(),
                html_inline: Arc::new(Mutex::new(std::collections::HashMap::new())),
                worker: None,
                worker_entries: Arc::new(Mutex::new(std::collections::HashSet::new())),
            })])
            .with_options(BundlerOptions {
                input: Some(vec![InputItem {
                    name: Some("_oj_server_fns".to_string()),
                    import: "./_oj_server_fns_entry.tsx".to_string(),
                }]),
                cwd: Some(root.to_path_buf()),
                dir: Some(out_dir.display().to_string()),
                platform: Some(Platform::Node),
                external: Some(external),
                format: Some(OutputFormat::Esm),
                entry_filenames: Some("_oj_server_fns.mjs".to_string().into()),
                chunk_filenames: Some("_oj_server_fns-[hash].mjs".to_string().into()),
                minify: Some(RawMinifyOptions::Bool(false)),
                transform: transform_options(&config, is_production),
                define: Some({
                    let mut pairs = node_env_defines(&node_env);
                    pairs.push(import_meta_hot_define());
                    pairs.push(("import.meta.env.SSR".to_string(), "true".to_string()));
                    pairs.into_iter().collect()
                }),
                ..Default::default()
            })
            .build()
            .map_err(rolldown_failure!("server-fns init failed"))?;
        bundler
            .write()
            .await
            .map_err(rolldown_failure!("server-fns build failed"))?;
        bundler
            .close()
            .await
            .map_err(|errs| anyhow::anyhow!("server-fns close failed:\n{errs:?}"))?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let _ = fs::remove_file(&entry_path);
    result
}

const PRERENDER_JS: &str = r#"import * as entry from "./entry-server.mjs";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const CLIENT_JS = "__CLIENT_JS__";
const CLIENT_CSS = "__CLIENT_CSS__";
const serialize = (d) => JSON.stringify(d ?? null).replace(/</g, "\\u003c");
// Injected by the build (the script runs on the embedded engine, not argv).
const paths = __PATHS__;
const root = dirname(fileURLToPath(import.meta.url));

// Called by the engine (`JsEngine::call` settles on the returned promise; an
// eval would instead wait out an event loop the server bundle keeps alive).
export async function run() {
  for (const url of paths) {
    const data = typeof entry.load === "function" ? await entry.load(url) : null;
    const routeHead = typeof entry.head === "function" ? String(await entry.head(url, data)) : "";
    const body = await renderFull(url, data);
    const html =
      '<!doctype html><html><head><meta charset="utf-8">' +
      routeHead +
      `<script>window.__OJ_DATA__=${serialize(data)}</script>` +
      (CLIENT_CSS ? `<link rel="stylesheet" href="${CLIENT_CSS}">` : "") +
      `<script type="module" src="${CLIENT_JS}"></script></head><body><div id="app">` +
      body +
      "</div></body></html>";
    const file = url === "/" ? "index.html" : join(url.replace(/^\/+/, ""), "index.html");
    const dest = join(root, file);
    await mkdir(dirname(dest), { recursive: true });
    await writeFile(dest, html);
    console.error(`oj prerender: ${url} -> ${file}`);
  }
  return true;
}

async function renderFull(url, data) {
  if (typeof entry.renderStream === "function") {
    const stream = await entry.renderStream(url, data);
    const reader = stream.getReader();
    const dec = new TextDecoder();
    let out = "";
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      out += dec.decode(value, { stream: true });
    }
    return out;
  }
  return await entry.render(url, data);
}
"#;

const SSR_PROD_SERVER: &str = r#"import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join, normalize } from "node:path";
import * as entry from "./entry-server.mjs";
import { dispatch as __ojDispatch } from "./_oj_server_fns.mjs";

const root = dirname(fileURLToPath(import.meta.url));
const PORT = process.env.PORT || 5180;
const CLIENT_JS = "__CLIENT_JS__";
const CLIENT_CSS = "__CLIENT_CSS__";
const TAIL = "</div></body></html>";
const TYPES = { ".js": "text/javascript", ".css": "text/css", ".map": "application/json" };
const serialize = (data) => JSON.stringify(data ?? null).replace(/</g, "\\u003c");
const readBody = (req) =>
  new Promise((resolve) => {
    let b = "";
    req.on("data", (c) => (b += c));
    req.on("end", () => resolve(b));
  });

createServer(async (req, res) => {
  const url = req.url.split("?")[0];
  if (req.method === "POST" && url === "/__oj_fn") {
    try {
      const { module, name, args } = JSON.parse(await readBody(req));
      const result = await __ojDispatch(module, name, args);
      res.writeHead(200, { "content-type": "application/json" });
      return void res.end(JSON.stringify(result ?? null));
    } catch (e) {
      res.writeHead(500, { "content-type": "text/plain" });
      return void res.end(String((e && e.stack) || e));
    }
  }
  if (url.startsWith("/assets/")) {
    const file = normalize(join(root, url));
    if (!file.startsWith(root)) return void res.writeHead(403).end();
    try {
      const buf = await readFile(file);
      res.writeHead(200, { "content-type": TYPES[file.slice(file.lastIndexOf("."))] || "application/octet-stream" });
      return void res.end(buf);
    } catch {
      return void res.writeHead(404).end();
    }
  }
  try {
    const wantsData = Boolean(req.headers["oj-loader"]);
    const load = () => (typeof entry.load === "function" ? entry.load(url) : null);
    if (req.method === "POST") {
      if (typeof entry.action === "function") await entry.action(url, await readBody(req));
      if (wantsData) {
        const body = serialize(await load());
        res.writeHead(200, { "content-type": "application/json" });
        return void res.end(body);
      }
      return void res.writeHead(303, { location: url }).end();
    }
    if (wantsData) {
      const body = serialize(await load());
      res.writeHead(200, { "content-type": "application/json" });
      return void res.end(body);
    }
    const data = await load();
    const json = serialize(data);
    const routeHead = typeof entry.head === "function" ? String(await entry.head(url, data)) : "";
    const HEAD =
      '<!doctype html><html><head><meta charset="utf-8">' +
      routeHead +
      `<script>window.__OJ_DATA__=${json}</script>` +
      (CLIENT_CSS ? `<link rel="stylesheet" href="${CLIENT_CSS}">` : "") +
      `<script type="module" src="${CLIENT_JS}"></script></head><body><div id="app">`;
    const stream = await entry.renderStream(url, data);
    res.writeHead(200, { "content-type": "text/html; charset=utf-8", "transfer-encoding": "chunked" });
    res.write(HEAD);
    const reader = stream.getReader();
    const dec = new TextDecoder();
    for (;;) {
      const { done, value } = await reader.read();
      if (done) break;
      res.write(dec.decode(value, { stream: true }));
    }
    res.write(TAIL);
    res.end();
  } catch (e) {
    res.writeHead(500, { "content-type": "text/html" }).end(`<pre>${String((e && e.stack) || e)}</pre>`);
  }
}).listen(PORT, () => console.log(`oj ssr server on http://localhost:${PORT}`));
"#;

const SSR_WORKER_ENTRY: &str = r#"import * as entry from "./entry-server.mjs";
import { dispatch as __ojDispatch } from "./_oj_server_fns.mjs";

const CLIENT_JS = "__CLIENT_JS__";
const CLIENT_CSS = "__CLIENT_CSS__";
const serialize = (d) => JSON.stringify(d ?? null).replace(/</g, "\\u003c");
const enc = new TextEncoder();

export default {
  async fetch(request) {
    const url = new URL(request.url).pathname;
    if (request.method === "POST" && url === "/__oj_fn") {
      try {
        const { module, name, args } = await request.json();
        return Response.json(await __ojDispatch(module, name, args));
      } catch (e) {
        return new Response(String((e && e.stack) || e), { status: 500 });
      }
    }
    const wantsData = Boolean(request.headers.get("oj-loader"));
    const load = () => (typeof entry.load === "function" ? entry.load(url) : null);
    if (request.method === "POST") {
      if (typeof entry.action === "function") await entry.action(url, await request.text());
      if (wantsData) return Response.json(await load());
      return new Response(null, { status: 303, headers: { location: url } });
    }
    if (wantsData) return Response.json(await load());
    const data = await load();
    const routeHead = typeof entry.head === "function" ? String(await entry.head(url, data)) : "";
    const HEAD =
      '<!doctype html><html><head><meta charset="utf-8">' +
      routeHead +
      `<script>window.__OJ_DATA__=${serialize(data)}</script>` +
      (CLIENT_CSS ? `<link rel="stylesheet" href="${CLIENT_CSS}">` : "") +
      `<script type="module" src="${CLIENT_JS}"></script></head><body><div id="app">`;
    const stream = await entry.renderStream(url, data);
    const body = new ReadableStream({
      async start(controller) {
        controller.enqueue(enc.encode(HEAD));
        const reader = stream.getReader();
        for (;;) {
          const { done, value } = await reader.read();
          if (done) break;
          controller.enqueue(value);
        }
        controller.enqueue(enc.encode("</div></body></html>"));
        controller.close();
      },
    });
    return new Response(body, { headers: { "content-type": "text/html; charset=utf-8" } });
  },
};
"#;

pub(crate) fn derive_client_entry(root: &Path, server_entry: &str) -> Option<String> {
    let file = Path::new(server_entry).file_name()?.to_str()?;
    if !file.contains("server") {
        return None;
    }
    let client_file = file.replace("server", "client");
    let client_rel = match Path::new(server_entry).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => {
            format!("{}/{}", dir.to_string_lossy(), client_file)
        }
        _ => client_file,
    };
    root.join(&client_rel).is_file().then_some(client_rel)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn build_ssr_app(
    root: &Path,
    out_dir: &Path,
    entry: &str,
    mode: &str,
    minify: bool,
    sourcemap: oj_config::Sourcemap,
    prerender: Option<Vec<String>>,
    empty_out_dir: Option<bool>,
) -> anyhow::Result<()> {
    prepare_out_dir(root, out_dir, empty_out_dir)?;

    build_ssr(root, out_dir, entry, mode, sourcemap).await?;

    let Some(client_entry) = derive_client_entry(root, entry) else {
        println!("oj build (ssr): server bundle only (no *-client sibling to hydrate)");
        return Ok(());
    };
    let (js, css) =
        build_client_entry(root, out_dir, &client_entry, mode, minify, sourcemap).await?;

    build_server_fns(root, out_dir, mode).await?;
    if has_server_modules(root) {
        println!(
            "  {:>9}  _oj_server_fns.mjs",
            human_bytes(OJ_SERVER_FNS_JS.len())
        );
    }

    let server = SSR_PROD_SERVER
        .replace("__CLIENT_JS__", &js)
        .replace("__CLIENT_CSS__", css.as_deref().unwrap_or(""));
    fs::write(out_dir.join("server.mjs"), server)?;
    println!("  {:>9}  server.mjs", human_bytes(SSR_PROD_SERVER.len()));

    let worker = SSR_WORKER_ENTRY
        .replace("__CLIENT_JS__", &js)
        .replace("__CLIENT_CSS__", css.as_deref().unwrap_or(""));
    fs::write(out_dir.join("worker.mjs"), worker)?;
    println!(
        "  {:>9}  worker.mjs (edge)",
        human_bytes(SSR_WORKER_ENTRY.len())
    );

    if let Some(paths) = prerender.filter(|p| !p.is_empty()) {
        let script = PRERENDER_JS
            .replace("__CLIENT_JS__", &js)
            .replace("__CLIENT_CSS__", css.as_deref().unwrap_or(""))
            .replace("__PATHS__", &serde_json::to_string(&paths)?);
        let script_path = out_dir.join("_oj_prerender.mjs");
        fs::write(&script_path, script)?;
        // Runs on the embedded engine: the built server bundle imports its
        // externals from node_modules via byonm, as it did under node.
        let mut engine_config = oj_js::EngineConfig::new(root);
        engine_config.code_cache_dir = Some(oj_server::engine_code_cache_dir(root));
        let engine = oj_js::JsEngine::spawn(engine_config)
            .map_err(|e| anyhow::anyhow!("prerender engine: {e}"))?;
        let result = engine
            .call(
                script_path.to_string_lossy().into_owned(),
                "run",
                Vec::new(),
            )
            .await;
        let _ = fs::remove_file(&script_path);
        if let Err(e) = result {
            bail!("prerender failed: {e}");
        }
    }
    println!("  run: node {}", out_dir.join("server.mjs").display());
    Ok(())
}

async fn build_client_entry(
    root: &Path,
    out_dir: &Path,
    entry: &str,
    mode: &str,
    minify: bool,
    sourcemap: oj_config::Sourcemap,
) -> anyhow::Result<(String, Option<String>)> {
    let entry_import = if entry.starts_with('.') {
        entry.to_string()
    } else {
        format!("./{entry}")
    };
    let stem = Path::new(entry)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("client")
        .to_string();
    let collected_css: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));

    let mut config =
        oj_config::load_with(root, "build", mode).map_err(|e| anyhow::anyhow!("{e}"))?;
    oj_server::plugins::adopt_vite_config_values(&mut config, root, "build", mode)
        .map_err(|e| anyhow::anyhow!(e))?;
    apply_cli_options(&mut config);
    let loaded_env = oj_env::load(&env_dir_of(root, &config), mode);
    let env_prefixes = env_prefixes_of(&config);
    let env_prefix_refs: Vec<&str> = env_prefixes.iter().map(String::as_str).collect();
    let node_env = oj_env::resolve_node_env(shell_node_env().as_deref(), &loaded_env, "production");
    let is_production = node_env == "production";
    let client_base = normalize_base(config.base.as_deref().unwrap_or("/"));
    let assets_dir = oj_config::build_assets_dir(&config);
    let plugin_host = user_plugin_host(
        root,
        &client_base,
        &serde_json::json!(config.define),
        &serde_json::json!(config.environments),
        "client",
        mode,
    )
    .await;
    let plugin_defines = plugin_config_defines(&plugin_host).await;
    let emit = Arc::new(EmitState::new(root.to_path_buf()));
    let mut oj_plugins: Vec<SharedPluginable> = Vec::new();
    if let Some(host) = &plugin_host {
        oj_plugins.push(Arc::new(OjUserPlugin::new(
            Arc::clone(host),
            Arc::clone(&emit),
            host.build_hook_plan().await,
        )));
    }
    oj_plugins.push(Arc::new(OjCssPlugin {
        collected: Arc::clone(&collected_css),
        root: root.to_path_buf(),
        has_postcss: oj_server::has_postcss_config(root),
        inline_limit: assets_inline_limit_of(&config),
        client: true,
        css_code_split: false,
        resolve: css_resolve_of(root, &config, "client"),
        host: plugin_host.clone(),
        css_transform_enabled: Arc::new(tokio::sync::OnceCell::new()),
        css: config.css.clone(),
        html_inline: Arc::new(Mutex::new(std::collections::HashMap::new())),
        worker: None,
        worker_entries: Arc::new(Mutex::new(std::collections::HashSet::new())),
    }));

    let mut bundler = BundlerBuilder::default()
        .with_plugins(oj_plugins)
        .with_options(BundlerOptions {
            input: Some(vec![InputItem {
                name: Some(stem),
                import: entry_import,
            }]),
            cwd: Some(root.to_path_buf()),
            dir: Some(out_dir.display().to_string()),
            resolve: rolldown_resolve(root, &config, "client"),
            transform: transform_options(&config, is_production),
            entry_filenames: Some(
                oj_config::assets_dir_path(&assets_dir, "[name]-[hash].js").into(),
            ),
            chunk_filenames: Some(
                oj_config::assets_dir_path(&assets_dir, "[name]-[hash].js").into(),
            ),
            minify: Some(rolldown_minify(
                oj_config::environment_build_bool(&config, "client", "minify").unwrap_or(minify),
            )),
            sourcemap: env_sourcemap(&config, "client", sourcemap),
            define: Some({
                let env = oj_env::with_process_env(
                    loaded_env.clone(),
                    std::env::vars(),
                    &env_prefix_refs,
                );
                let mut pairs = process_env_defines(&node_env);
                pairs.push(import_meta_hot_define());
                pairs.extend(oj_env::import_meta_env_defines(
                    &env,
                    mode,
                    !is_production,
                    &client_base,
                    &env_prefix_refs,
                ));
                pairs.extend(oj_config::config_defines(&config));
                pairs.extend(oj_config::environment_defines(&config, "client"));
                pairs.extend(plugin_defines);
                pairs.into_iter().collect()
            }),
            ..Default::default()
        })
        .build()
        .map_err(rolldown_failure!("rolldown init failed"))?;

    let output = match bundler
        .write()
        .await
        .map_err(rolldown_failure!("client build failed"))
    {
        Ok(output) => output,
        Err(err) => return Err(fail_with_plugin_hooks(err, &plugin_host).await),
    };
    bundler
        .close()
        .await
        .map_err(|errs| anyhow::anyhow!("client close failed:\n{errs:?}"))?;

    if let Some(host) = &plugin_host {
        if let Err(e) = host.build_end(None).await {
            bail!("plugin buildEnd failed (client):\n{e}");
        }
    }

    let mut js = None;
    for asset in &output.assets {
        if let rolldown_common::Output::Chunk(c) = asset {
            if c.is_entry {
                js = Some(with_base(&c.filename, &client_base));
            }
        }
    }
    let js = js.ok_or_else(|| anyhow::anyhow!("client build produced no entry chunk"))?;

    let mut css_entries = collected_css.lock().unwrap().clone();
    let css = if css_entries.is_empty() {
        None
    } else {
        css_entries.sort();
        let combined: String = css_entries
            .into_iter()
            .map(|(_, css)| css)
            .collect::<Vec<_>>()
            .join("\n");
        let hash = format!("{:016x}", {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            combined.hash(&mut h);
            h.finish()
        });
        let name = oj_config::assets_dir_path(&assets_dir, &format!("style-{}.css", &hash[..8]));
        if let Some(parent) = out_dir.join(&name).parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(out_dir.join(&name), combined)?;
        Some(with_base(&name, &client_base))
    };
    Ok((js, css))
}

async fn build_library(
    root: &Path,
    out_dir: &Path,
    config: &oj_config::OjConfig,
    lib: oj_config::LibConfig,
    mode: &str,
    minify: bool,
    sourcemap: oj_config::Sourcemap,
) -> anyhow::Result<()> {
    let loaded_env = oj_env::load(&env_dir_of(root, config), mode);
    let node_env = oj_env::resolve_node_env(shell_node_env().as_deref(), &loaded_env, "production");
    let is_production = node_env == "production";
    let env_prefixes = env_prefixes_of(config);
    let env_prefix_refs: Vec<&str> = env_prefixes.iter().map(String::as_str).collect();
    let import_of = |p: &str| {
        if p.starts_with('.') {
            p.to_string()
        } else {
            format!("./{p}")
        }
    };
    let stem_of = |p: &str| {
        Path::new(p)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("index")
            .to_string()
    };
    // Vite: a string entry is named by its stem, a list by each stem, an object
    // by its aliases (`resolveRolldownOptions`).
    let entries: Vec<(String, String)> = match &lib.entry {
        oj_config::LibEntry::One(p) => vec![(stem_of(p), import_of(p))],
        oj_config::LibEntry::Many(ps) => ps.iter().map(|p| (stem_of(p), import_of(p))).collect(),
        oj_config::LibEntry::Named(m) => m.iter().map(|(k, p)| (k.clone(), import_of(p))).collect(),
    };
    if entries.is_empty() {
        bail!("build.lib.entry resolved to no entries");
    }
    let multiple = entries.len() > 1;
    // Vite's `resolveLibFilename`: `fileName`, else the (unscoped) package.json
    // name for a single string entry, else the entry's own name; the extension
    // follows the package `type` (`resolveOutputJsExtension`).
    let pkg: Option<serde_json::Value> = fs::read_to_string(root.join("package.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok());
    let pkg_type_module = pkg
        .as_ref()
        .and_then(|p| p.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| t == "module");
    let pkg_name = pkg
        .as_ref()
        .and_then(|p| p.get("name"))
        .and_then(|n| n.as_str())
        .map(|n| {
            n.strip_prefix('@')
                .and_then(|s| s.split_once('/'))
                .map(|(_, rest)| rest)
                .unwrap_or(n)
                .to_string()
        });
    let single_string_entry = matches!(lib.entry, oj_config::LibEntry::One(_));
    let base_name: Option<String> = match (&lib.file_name, multiple) {
        (Some(f), false) => Some(f.clone()),
        (Some(_), true) => {
            eprintln!("oj build: (!) build.lib.fileName is ignored with multiple entries; each entry keeps its own name");
            None
        }
        (None, false) if single_string_entry && pkg.is_some() => Some(
            pkg_name
                .clone()
                .ok_or_else(|| anyhow::anyhow!("Name in package.json is required if option \"build.lib.fileName\" is not provided."))?,
        ),
        (None, false) => Some(entries[0].0.clone()),
        (None, true) => None,
    };
    let js_ext = |fmt: &str| -> &'static str {
        if pkg_type_module {
            if matches!(fmt, "cjs" | "umd") {
                "cjs"
            } else {
                "js"
            }
        } else if matches!(fmt, "es" | "esm") {
            "mjs"
        } else {
            "js"
        }
    };
    // Vite's `resolveBuildOutputs`: default formats are es+umd for one entry and
    // es+cjs for several; umd/iife need a single entry and `build.lib.name`.
    let formats = lib.formats.clone().unwrap_or_else(|| {
        if multiple {
            vec!["es".into(), "cjs".into()]
        } else {
            vec!["es".into(), "umd".into()]
        }
    });
    for fmt in &formats {
        if lib_format(fmt).is_none() {
            bail!("unknown lib format: {fmt} (es, cjs, umd, iife)");
        }
        if matches!(fmt.as_str(), "umd" | "iife") {
            if multiple {
                bail!("Multiple entry points are not supported when output formats include \"umd\" or \"iife\".");
            }
            if lib.name.is_none() {
                bail!("Option \"build.lib.name\" is required when output formats include \"umd\" or \"iife\".");
            }
        }
    }

    prepare_out_dir(
        root,
        out_dir,
        config.build.as_ref().and_then(|b| b.empty_out_dir),
    )?;
    let started = Instant::now();
    let collected_css: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let mut emitted: Vec<(String, usize)> = Vec::new();

    for fmt in &formats {
        let format = match fmt.as_str() {
            "es" | "esm" => OutputFormat::Esm,
            "cjs" => OutputFormat::Cjs,
            "umd" => OutputFormat::Umd,
            _ => OutputFormat::Iife,
        };
        let ext = js_ext(fmt);
        // `name.js` / `name.cjs` for es and cjs, `name.umd.js` / `name.iife.js`
        // otherwise (Vite `resolveLibFilename`).
        let name_pattern = base_name.clone().unwrap_or_else(|| "[name]".to_string());
        let entry_file = if matches!(fmt.as_str(), "es" | "esm" | "cjs") {
            format!("{name_pattern}.{ext}")
        } else {
            format!("{name_pattern}.{fmt}.{ext}")
        };
        // An ES library keeps its whitespace so `/* @__PURE__ */` annotations
        // survive for the consumer's tree-shaking (Vite `codegen: false`).
        let minify_opts = if minify && matches!(fmt.as_str(), "es" | "esm") {
            RawMinifyOptions::Object(rolldown_common::RawMinifyOptionsDetailed {
                mangle: Some(rolldown_common::RawMangleOptions::default()),
                compress: Some(rolldown_common::RawCompressOptions::default()),
                remove_whitespace: false,
            })
        } else {
            rolldown_minify(minify)
        };

        let mut bundler = BundlerBuilder::default()
            .with_plugins(vec![Arc::new(OjCssPlugin {
                collected: Arc::clone(&collected_css),
                root: root.to_path_buf(),
                has_postcss: oj_server::has_postcss_config(root),
                // Vite inlines every asset of a library build.
                inline_limit: u64::MAX,
                client: true,
                resolve: css_resolve_of(root, config, "client"),
                css_code_split: false,
                host: None,
                css_transform_enabled: Arc::new(tokio::sync::OnceCell::new()),
                css: config.css.clone(),
                html_inline: Arc::new(Mutex::new(std::collections::HashMap::new())),
                worker: None,
                worker_entries: Arc::new(Mutex::new(std::collections::HashSet::new())),
            })])
            .with_options(BundlerOptions {
                input: Some(
                    entries
                        .iter()
                        .map(|(name, import)| InputItem {
                            name: Some(name.clone()),
                            import: import.clone(),
                        })
                        .collect(),
                ),
                cwd: Some(root.to_path_buf()),
                dir: Some(out_dir.display().to_string()),
                resolve: rolldown_resolve(root, config, "client"),
                format: Some(format),
                name: lib.name.clone(),
                preserve_entry_signatures: Some(rolldown_common::PreserveEntrySignatures::Strict),
                entry_filenames: Some(entry_file.into()),
                chunk_filenames: Some(format!("[name]-[hash].{ext}").into()),
                asset_filenames: Some("[name].[ext]".to_string().into()),
                code_splitting: matches!(fmt.as_str(), "umd" | "iife")
                    .then_some(rolldown_common::CodeSplittingMode::Bool(false)),
                minify: Some(minify_opts),
                sourcemap: sourcemap_type(sourcemap),
                transform: transform_options(config, is_production),
                // Vite's define plugin adds no `process.env` defines for a library
                // (rolldown's own browser-platform NODE_ENV default still applies,
                // as under Vite); import.meta.env and the user's `define` do.
                define: Some({
                    let env = oj_env::with_process_env(
                        loaded_env.clone(),
                        std::env::vars(),
                        &env_prefix_refs,
                    );
                    let mut pairs = oj_env::import_meta_env_defines(
                        &env,
                        mode,
                        !is_production,
                        "/",
                        &env_prefix_refs,
                    );
                    pairs.push(import_meta_hot_define());
                    pairs.extend(oj_config::config_defines(config));
                    pairs.extend(oj_config::environment_defines(config, "client"));
                    pairs.into_iter().collect()
                }),
                ..Default::default()
            })
            .build()
            .map_err(rolldown_failure!("rolldown init failed"))?;

        let output = bundler
            .write()
            .await
            .map_err(rolldown_failure!("lib build ({fmt}) failed"))?;
        for asset in &output.assets {
            if let rolldown_common::Output::Chunk(c) = asset {
                emitted.push((c.filename.to_string(), c.code.len()));
            }
        }
    }

    let css_entries = collected_css.lock().unwrap().clone();
    if !css_entries.is_empty() {
        let combined: String = css_entries
            .into_iter()
            .map(|(_, css)| css)
            .collect::<Vec<_>>()
            .join("\n");
        // Vite: `build.lib.cssFileName`, else the library's file name / package name.
        let css_stem = lib
            .css_file_name
            .clone()
            .or_else(|| base_name.clone())
            .or_else(|| pkg_name.clone())
            .unwrap_or_else(|| "style".to_string());
        let css_name = format!("{css_stem}.css");
        fs::write(out_dir.join(&css_name), &combined)?;
        emitted.push((css_name, combined.len()));
    }

    println!(
        "oj build (library): {} in {:?}",
        out_dir.display(),
        started.elapsed()
    );
    emitted.sort_by_key(|e| std::cmp::Reverse(e.1));
    emitted.dedup();
    for (name, bytes) in &emitted {
        println!("  {:>9}  {}", human_bytes(*bytes), name);
    }
    Ok(())
}

/// The library formats Vite accepts; `true` when the format needs `build.lib.name`.
fn lib_format(fmt: &str) -> Option<bool> {
    match fmt {
        "es" | "esm" | "cjs" => Some(false),
        "umd" | "iife" => Some(true),
        _ => None,
    }
}

/// Vite's `base`: `""` and `"./"` mean a relative base (URLs are emitted relative
/// to the page / stylesheet / chunk that references them, so the build can be
/// served from any path or from `file://`); anything else is an absolute prefix.
fn normalize_base(base: &str) -> String {
    if base.is_empty() || base == "./" {
        return "./".to_string();
    }
    let mut b = base.to_string();
    if !b.starts_with('/') {
        b.insert(0, '/');
    }
    if !b.ends_with('/') {
        b.push('/');
    }
    b
}

fn is_relative_base(base: &str) -> bool {
    base == "./"
}

/// A public URL for an output file from a root-level page (or any consumer that
/// sits at the outDir root).
fn with_base(filename: &str, base: &str) -> String {
    format!("{base}{}", filename.trim_start_matches('/'))
}

/// The base to use from a page at `out_rel` (e.g. `nested/index.html`): with a
/// relative base that is `../` per directory level, otherwise the absolute base.
fn page_base(base: &str, out_rel: &str) -> String {
    if !is_relative_base(base) {
        return base.to_string();
    }
    let depth = out_rel.trim_start_matches('/').matches('/').count();
    if depth == 0 {
        "./".to_string()
    } else {
        "../".repeat(depth)
    }
}

/// URL for an emitted asset as referenced from an emitted stylesheet: relative
/// bases become a path relative to where the stylesheets land (the
/// `assetFileNames` directory, `assets/` by default), absolute bases the public path.
fn css_asset_url(name: &str, base: &str, asset_names: Option<&str>) -> String {
    if is_relative_base(base) {
        let css_file = render_asset_name(asset_names, "style", "00000000", "css");
        relative_chunk_path(&css_file, name)
    } else {
        with_base(name, base)
    }
}

/// `to` relative to the directory of `from` (both outDir-relative, `/`-separated).
fn relative_chunk_path(from: &str, to: &str) -> String {
    let from_dir: Vec<&str> = from
        .rsplit_once('/')
        .map(|(d, _)| d.split('/').collect())
        .unwrap_or_default();
    let to_parts: Vec<&str> = to.split('/').collect();
    let common = from_dir
        .iter()
        .zip(to_parts.iter())
        .take_while(|(a, b)| a == b)
        .count();
    let mut out = String::new();
    for _ in common..from_dir.len() {
        out.push_str("../");
    }
    if out.is_empty() {
        out.push_str("./");
    }
    out.push_str(&to_parts[common..].join("/"));
    out
}

/// All chunks reachable from `entry` via static imports (excludes `entry`).
fn transitive_imports(
    entry: &str,
    map: &std::collections::HashMap<String, Vec<String>>,
) -> std::collections::BTreeSet<String> {
    let mut seen = std::collections::BTreeSet::new();
    let mut stack: Vec<String> = map.get(entry).cloned().unwrap_or_default();
    while let Some(f) = stack.pop() {
        if seen.insert(f.clone()) {
            if let Some(deps) = map.get(&f) {
                stack.extend(deps.iter().cloned());
            }
        }
    }
    seen
}

fn sanitize_asset_name(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn content_hash(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex()[..16].to_string()
}

/// Emit a CSS-referenced asset (font/image) under a content hash and return its
/// base-prefixed URL, or None to leave the `url()` untouched (data:/absolute/external).
/// How files oj writes itself (CSS url() assets, chunk stylesheets) are named
/// and when they are inlined: `build.assetsInlineLimit` and
/// `build.rollupOptions.output.assetFileNames`.
#[derive(Debug, Clone, Copy)]
struct CssAssetOpts<'a> {
    inline_limit: u64,
    asset_names: Option<&'a str>,
    /// Alias and root-absolute resolution for `url()` specifiers.
    resolve: oj_css::CssResolve<'a>,
}

/// Render Vite/rolldown `assetFileNames` placeholders (`[name]`, `[hash]`,
/// `[ext]`, `[extname]`); the default is Vite's `assets/[name]-[hash][extname]`.
fn render_asset_name(pattern: Option<&str>, name: &str, hash: &str, ext: &str) -> String {
    let pattern = pattern.unwrap_or("assets/[name]-[hash][extname]");
    let extname = if ext.is_empty() {
        String::new()
    } else {
        format!(".{ext}")
    };
    pattern
        .replace("[name]", name)
        .replace("[hash]", &hash[..hash.len().min(8)])
        .replace("[extname]", &extname)
        .replace("[ext]", ext)
}

fn emit_css_url(
    inner: &str,
    css_dir: &Path,
    out_dir: &Path,
    base: &str,
    opts: CssAssetOpts<'_>,
    emitted: &mut Vec<(String, usize)>,
    seen: &mut std::collections::HashMap<PathBuf, String>,
) -> Option<String> {
    if inner.is_empty()
        || inner.starts_with("data:")
        || inner.starts_with("http://")
        || inner.starts_with("https://")
        || inner.starts_with("//")
        || inner.starts_with('#')
    {
        return None;
    }
    let cut = inner.find(['?', '#']).unwrap_or(inner.len());
    let (clean, suffix) = inner.split_at(cut);
    // Vite's url resolver: a public-dir file keeps its url; an alias or a
    // root-absolute path names a file that is emitted like a relative one.
    let abs = if let Some(aliased) = opts.resolve.alias_path(clean) {
        aliased
    } else if let Some(rel) = clean.strip_prefix('/') {
        if rel.is_empty() || opts.resolve.public_file(clean).is_some() {
            return None;
        }
        opts.resolve.root_path(clean)?
    } else {
        css_dir.join(clean)
    };
    let abs = abs.canonicalize().ok()?;
    if let Some(url) = seen.get(&abs) {
        return Some(format!("{url}{suffix}"));
    }
    let data = std::fs::read(&abs).ok()?;
    let ext = abs.extension().and_then(|s| s.to_str()).unwrap_or("");
    // Small assets referenced from CSS are inlined as in Vite (assetsInlineLimit),
    // except an svg with a fragment, which must stay a file to keep its `#id`.
    if (data.len() as u64) < opts.inline_limit && !suffix.starts_with('#') {
        if ext.eq_ignore_ascii_case("svg") {
            return Some(svg_data_url(&String::from_utf8_lossy(&data)));
        }
        return Some(format!("data:{};base64,{}", asset_mime(ext), b64(&data)));
    }
    let hash = content_hash(&data);
    let stem = abs.file_stem().and_then(|s| s.to_str()).unwrap_or("asset");
    let name = render_asset_name(opts.asset_names, &sanitize_asset_name(stem), &hash, ext);
    let dest = out_dir.join(&name);
    if let Some(p) = dest.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    std::fs::write(&dest, &data).ok()?;
    emitted.push((name.clone(), data.len()));
    let url = css_asset_url(&name, base, opts.asset_names);
    seen.insert(abs, url.clone());
    Some(format!("{url}{suffix}"))
}

/// Rewrite relative `url()` refs in one stylesheet to point at emitted,
/// content-hashed assets, since the stylesheet is concatenated into
/// `/assets/style-*.css` where the original relative paths would 404.
fn rebase_css_urls(
    css: &str,
    css_dir: &Path,
    out_dir: &Path,
    base: &str,
    opts: CssAssetOpts<'_>,
    emitted: &mut Vec<(String, usize)>,
    seen: &mut std::collections::HashMap<PathBuf, String>,
) -> String {
    let mut out = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(pos) = rest.find("url(") {
        out.push_str(&rest[..pos]);
        let after = &rest[pos + 4..];
        let Some(close) = after.find(')') else {
            out.push_str("url(");
            rest = after;
            continue;
        };
        let inner_raw = &after[..close];
        let inner = inner_raw
            .trim()
            .trim_matches(|c| c == '"' || c == '\'')
            .trim();
        match emit_css_url(inner, css_dir, out_dir, base, opts, emitted, seen) {
            Some(url) => {
                out.push_str("url(\"");
                out.push_str(&url);
                out.push_str("\")");
            }
            None => {
                out.push_str("url(");
                out.push_str(inner_raw);
                out.push(')');
            }
        }
        rest = &after[close + 1..];
    }
    out.push_str(rest);
    out
}

/// `build.cssCodeSplit`: emit one stylesheet per chunk that imports CSS and
/// return `(chunk filename, css filename)` pairs. Which pages link which
/// stylesheet render-blocking is decided per page by `split_css_links`; chunks
/// only reached through dynamic imports get their CSS from `__vitePreload` (and
/// a self-injecting fallback, see `apply_preload_helper`).
#[allow(clippy::too_many_arguments)]
fn emit_split_css_files(
    output: &rolldown::BundleOutput,
    collected_css: &Arc<Mutex<Vec<(String, String)>>>,
    out_dir: &Path,
    root: &Path,
    base: &str,
    opts: CssAssetOpts<'_>,
    emitted: &mut Vec<(String, usize)>,
    manifest_entries: &mut [ManifestEntry],
    manifest_assets: &mut Vec<ManifestAsset>,
) -> anyhow::Result<Vec<(String, String)>> {
    let css_map: std::collections::HashMap<String, String> =
        collected_css.lock().unwrap().iter().cloned().collect();
    let mut chunk_css: Vec<(String, String)> = Vec::new();
    if css_map.is_empty() {
        return Ok(chunk_css);
    }
    let mut seen_assets: std::collections::HashMap<PathBuf, String> =
        std::collections::HashMap::new();
    for asset in &output.assets {
        let rolldown_common::Output::Chunk(chunk) = asset else {
            continue;
        };
        let mut css = String::new();
        let mut sources: Vec<String> = Vec::new();
        for module_id in &chunk.modules.keys {
            let mid = module_id.to_string();
            if let Some(src) = css_map.get(&mid) {
                sources.push(mid.clone());
                let dir = Path::new(&mid)
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| root.to_path_buf());
                if !css.is_empty() {
                    css.push('\n');
                }
                css.push_str(&rebase_css_urls(
                    src,
                    &dir,
                    out_dir,
                    base,
                    opts,
                    emitted,
                    &mut seen_assets,
                ));
            }
        }
        if css.is_empty() {
            continue;
        }
        let hash = content_hash(css.as_bytes());
        let css_name = render_asset_name(
            opts.asset_names,
            &sanitize_asset_name(&chunk.name),
            &hash,
            "css",
        );
        if let Some(parent) = out_dir.join(&css_name).parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(out_dir.join(&css_name), &css)?;
        emitted.push((css_name.clone(), css.len()));
        for entry in manifest_entries.iter_mut() {
            if entry.file == chunk.filename.as_str() {
                entry.css.push(css_name.clone());
            }
        }
        // Vite keys the stylesheet's manifest row by each source file it was
        // built from (the asset's `originalFileNames`).
        for src in &sources {
            push_css_manifest_row(manifest_assets, root, src, &css_name);
        }
        chunk_css.push((chunk.filename.to_string(), css_name));
    }
    Ok(chunk_css)
}

/// Render-blocking `<link rel=stylesheet>` tags for a page: the CSS of its entry
/// chunks and of every chunk they reach through static imports (no FOUC).
fn split_css_links(
    chunk_css: &[(String, String)],
    entry_files: &[String],
    imports_map: &std::collections::HashMap<String, Vec<String>>,
    page_base: &str,
) -> String {
    let mut sync_chunks: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for entry in entry_files {
        sync_chunks.insert(entry.clone());
        sync_chunks.extend(transitive_imports(entry, imports_map));
    }
    let mut linked: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut links = String::new();
    for (chunk_file, css_name) in chunk_css {
        if sync_chunks.contains(chunk_file) && linked.insert(css_name.clone()) {
            if !links.is_empty() {
                links.push('\n');
            }
            links.push_str(&format!(
                "<link rel=\"stylesheet\" href=\"{}\" crossorigin />",
                with_base(css_name, page_base)
            ));
        }
    }
    links
}

/// Vite's `__vitePreload` helper (importAnalysisBuild.ts), ES2015 so it survives
/// any `build.target`. `__vite__assetsURL` is generated per base: a relative base
/// resolves deps against the importing chunk, an absolute base is prefixed.
const PRELOAD_HELPER_JS: &str = r#"const __vite__scriptRel=(function(){var r=typeof document!=="undefined"&&document.createElement("link").relList;return r&&r.supports&&r.supports("modulepreload")?"modulepreload":"preload"})();const __vite__seen={};function __vitePreload(baseModule,deps,importerUrl){var promise=Promise.resolve();if(deps&&deps.length>0){var links=document.getElementsByTagName("link");var cspNonceMeta=document.querySelector("meta[property=csp-nonce]");var cspNonce=cspNonceMeta&&(cspNonceMeta.nonce||cspNonceMeta.getAttribute("nonce"));var allSettled=function(ps){return Promise.all(ps.map(function(p){return Promise.resolve(p).then(function(value){return{status:"fulfilled",value:value}},function(reason){return{status:"rejected",reason:reason}})}))};promise=allSettled(deps.map(function(dep){dep=new URL(__vite__assetsURL(dep,importerUrl),import.meta.url).href;if(dep in __vite__seen)return;__vite__seen[dep]=true;var isCss=dep.endsWith(".css");for(var i=links.length-1;i>=0;i--){var l=links[i];if(l.href===dep&&(!isCss||l.rel==="stylesheet"))return}var link=document.createElement("link");link.rel=isCss?"stylesheet":__vite__scriptRel;if(!isCss){link.as="script"}link.crossOrigin="";link.href=dep;if(cspNonce)link.setAttribute("nonce",cspNonce);document.head.appendChild(link);if(isCss){return new Promise(function(res,rej){link.addEventListener("load",res);link.addEventListener("error",function(){rej(new Error("Unable to preload CSS for "+dep))})})}}))}function handlePreloadError(err){var e=new Event("vite:preloadError",{cancelable:true});e.payload=err;window.dispatchEvent(e);if(!e.defaultPrevented)throw err}return promise.then(function(res){for(var i=0;i<(res||[]).length;i++){var item=res[i];if(item.status!=="rejected")continue;handlePreloadError(item.reason)}return baseModule().catch(handlePreloadError)})}"#;

/// Vite's `vite/modulepreload-polyfill` (fetches `<link rel=modulepreload>` on
/// browsers without native support), injected into page entry chunks.
const MODULEPRELOAD_POLYFILL_JS: &str = r#"(function(){var relList=document.createElement("link").relList;if(relList&&relList.supports&&relList.supports("modulepreload"))return;for(var i=0,ls=document.querySelectorAll('link[rel="modulepreload"]');i<ls.length;i++)processPreload(ls[i]);new MutationObserver(function(mutations){for(var m=0;m<mutations.length;m++){var mutation=mutations[m];if(mutation.type!=="childList")continue;for(var n=0;n<mutation.addedNodes.length;n++){var node=mutation.addedNodes[n];if(node.tagName==="LINK"&&node.rel==="modulepreload")processPreload(node)}}}).observe(document,{childList:true,subtree:true});function getFetchOpts(link){var fetchOpts={};if(link.integrity)fetchOpts.integrity=link.integrity;if(link.referrerPolicy)fetchOpts.referrerPolicy=link.referrerPolicy;if(link.crossOrigin==="use-credentials")fetchOpts.credentials="include";else if(link.crossOrigin==="anonymous")fetchOpts.credentials="omit";else fetchOpts.credentials="same-origin";return fetchOpts}function processPreload(link){if(link.ep)return;link.ep=true;var fetchOpts=getFetchOpts(link);fetch(link.href,fetchOpts)}})();"#;

/// Wrap each `import("./chunk.js")` in a chunk with `__vitePreload`, passing the
/// dependency list Vite would (the lazy chunk, its statically imported chunks
/// and their stylesheets, minus what the importer already has), prepend the
/// modulepreload polyfill to page entries, and give async-only chunks that own
/// CSS a self-injecting fallback link. Everything is prepended on the chunk's
/// first line so existing sourcemap line numbers stay valid.
#[allow(clippy::too_many_arguments)]
fn apply_preload_helper(
    output: &rolldown::BundleOutput,
    out_dir: &Path,
    base: &str,
    imports_map: &std::collections::HashMap<String, Vec<String>>,
    chunk_css: &[(String, String)],
    sync_chunks: &std::collections::BTreeSet<String>,
    page_entries: &[String],
    polyfill: bool,
    banner: Option<&str>,
    worker_entries: &std::collections::HashSet<String>,
) -> anyhow::Result<()> {
    let css_of: std::collections::HashMap<&str, &str> = chunk_css
        .iter()
        .map(|(c, css)| (c.as_str(), css.as_str()))
        .collect();
    let relative = is_relative_base(base);
    // Chunks only a worker ever loads run without a `document`: no preload
    // wrapping, no stylesheet self-injection (Vite builds workers without its
    // import analysis, `isWorker`).
    let worker_only = {
        let mut entries: Vec<(String, bool)> = Vec::new();
        let mut dynamic_map: std::collections::HashMap<String, Vec<String>> =
            std::collections::HashMap::new();
        for asset in &output.assets {
            if let rolldown_common::Output::Chunk(chunk) = asset {
                let file = chunk.filename.to_string();
                dynamic_map.insert(
                    file.clone(),
                    chunk
                        .dynamic_imports
                        .iter()
                        .map(|d| d.to_string())
                        .collect(),
                );
                if chunk.is_entry {
                    let is_worker = chunk
                        .facade_module_id
                        .as_ref()
                        .is_some_and(|f| worker_entries.contains(f.as_ref() as &str));
                    entries.push((file, is_worker));
                }
            }
        }
        worker_only_chunks(&entries, imports_map, &dynamic_map)
    };
    for asset in &output.assets {
        let rolldown_common::Output::Chunk(chunk) = asset else {
            continue;
        };
        let file = chunk.filename.to_string();
        if worker_only.contains(&file) {
            continue;
        }
        let path = out_dir.join(&file);
        let Ok(mut code) = fs::read_to_string(&path) else {
            continue;
        };
        let mut prefix = String::new();

        if polyfill && page_entries.iter().any(|e| e == &file) {
            prefix.push_str(MODULEPRELOAD_POLYFILL_JS);
        }

        let mut dep_list: Vec<String> = Vec::new();
        let mut wrapped = false;
        if !chunk.dynamic_imports.is_empty() {
            let own: std::collections::BTreeSet<String> = std::iter::once(file.clone())
                .chain(transitive_imports(&file, imports_map))
                .collect();
            for target in &chunk.dynamic_imports {
                let target = target.to_string();
                let rel = relative_chunk_path(&file, &target);
                let mut deps: Vec<String> = Vec::new();
                let closure: Vec<String> = std::iter::once(target.clone())
                    .chain(transitive_imports(&target, imports_map))
                    .collect();
                for c in &closure {
                    if own.contains(c) {
                        continue;
                    }
                    deps.push(c.clone());
                    if let Some(css) = css_of.get(c.as_str()) {
                        deps.push(css.to_string());
                    }
                }
                // Only the chunk itself: `import()` fetches it anyway.
                if deps.len() <= 1 {
                    deps.clear();
                }
                let mut idx: Vec<String> = Vec::new();
                for d in deps {
                    let d = if relative {
                        relative_chunk_path(&file, &d)
                    } else {
                        d
                    };
                    let i = match dep_list.iter().position(|x| x == &d) {
                        Some(i) => i,
                        None => {
                            dep_list.push(d);
                            dep_list.len() - 1
                        }
                    };
                    idx.push(i.to_string());
                }
                // rolldown's minifier may emit the specifier in any quote style.
                for quote in ['"', '\'', '`'] {
                    let needle = format!("import({quote}{rel}{quote})");
                    if code.contains(&needle) {
                        let replacement = format!(
                            "__vitePreload(function(){{return import({quote}{rel}{quote})}},__vite__mapDeps([{}]),import.meta.url)",
                            idx.join(",")
                        );
                        code = code.replace(&needle, &replacement);
                        wrapped = true;
                    }
                }
            }
        }
        if wrapped {
            let assets_url = if relative {
                "function(dep,importerUrl){return new URL(dep,importerUrl).href}".to_string()
            } else {
                format!(
                    "function(dep){{return {}+dep}}",
                    serde_json::Value::String(base.to_string())
                )
            };
            prefix.push_str(&format!(
                "const __vite__mapDeps=(i,m=__vite__mapDeps,d=(m.f||(m.f={})))=>i.map(i=>d[i]);const __vite__assetsURL={assets_url};{PRELOAD_HELPER_JS}",
                serde_json::Value::Array(
                    dep_list
                        .iter()
                        .map(|d| serde_json::Value::String(d.clone()))
                        .collect()
                )
            ));
        }

        // Async-only chunks with CSS: a fallback for dynamic imports the rewrite
        // above could not see (variable specifiers), idempotent against preload.
        if !sync_chunks.contains(&file) {
            if let Some(css_name) = css_of.get(file.as_str()) {
                let href = serde_json::Value::String(if relative {
                    relative_chunk_path(&file, css_name)
                } else {
                    with_base(css_name, base)
                });
                // Compare resolved URLs (`link.href` is absolute), so a link the
                // preload helper or the page already added is recognized.
                prefix.push_str(&format!(
                    "(function(){{var u=new URL({href},import.meta.url).href;var ls=document.getElementsByTagName('link');for(var i=0;i<ls.length;i++){{if(ls[i].rel==='stylesheet'&&ls[i].href===u)return}}var l=document.createElement('link');l.rel='stylesheet';l.href=u;document.head.appendChild(l)}})();"
                ));
            }
        }

        if !prefix.is_empty() {
            // `output.banner` stays the first thing in the file (a license
            // header, a `"use client"` directive) ahead of oj's helpers.
            let (head, body) = match banner {
                Some(b) if !b.is_empty() && code.starts_with(b) => (b, &code[b.len()..]),
                _ => ("", code.as_str()),
            };
            fs::write(&path, format!("{head}{prefix}{body}"))?;
        }
    }
    Ok(())
}

/// The chunks reachable (statically or dynamically) from worker entries but from
/// no other entry.
fn worker_only_chunks(
    entries: &[(String, bool)],
    imports_map: &std::collections::HashMap<String, Vec<String>>,
    dynamic_map: &std::collections::HashMap<String, Vec<String>>,
) -> std::collections::BTreeSet<String> {
    let closure = |roots: &[&String]| -> std::collections::BTreeSet<String> {
        let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
        let mut stack: Vec<String> = roots.iter().map(|r| (*r).clone()).collect();
        while let Some(f) = stack.pop() {
            if !seen.insert(f.clone()) {
                continue;
            }
            for m in [imports_map, dynamic_map] {
                if let Some(deps) = m.get(&f) {
                    stack.extend(deps.iter().cloned());
                }
            }
        }
        seen
    };
    let workers: Vec<&String> = entries.iter().filter(|(_, w)| *w).map(|(f, _)| f).collect();
    if workers.is_empty() {
        return std::collections::BTreeSet::new();
    }
    let others: Vec<&String> = entries
        .iter()
        .filter(|(_, w)| !*w)
        .map(|(f, _)| f)
        .collect();
    let from_workers = closure(&workers);
    let from_others = closure(&others);
    from_workers.difference(&from_others).cloned().collect()
}

/// Vite's `ssr-manifest.json` (ssrManifestPlugin): for every source module in the
/// client bundle, the public URLs a server renderer should preload when that
/// module renders: the module's chunk (non-entry chunks only; entries are already
/// in the HTML) and that chunk's stylesheet. Keys are root-relative paths.
fn ssr_manifest(
    output: &rolldown::BundleOutput,
    root: &Path,
    base: &str,
    chunk_css: &[(String, String)],
    combined_css: &Option<String>,
    imports_map: &std::collections::HashMap<String, Vec<String>>,
) -> serde_json::Map<String, serde_json::Value> {
    let css_of: std::collections::HashMap<&str, &str> = chunk_css
        .iter()
        .map(|(c, css)| (c.as_str(), css.as_str()))
        .collect();
    let imported_assets = chunk_imported_assets(&output.assets);
    let mut manifest = serde_json::Map::new();
    for asset in &output.assets {
        let rolldown_common::Output::Chunk(chunk) = asset else {
            continue;
        };
        let file = chunk.filename.to_string();
        // Vite's css deps map: each dynamically imported chunk is keyed by its
        // file name with the stylesheets it and its static imports bring in, so
        // an SSR renderer can preload the CSS of a lazy route.
        for target in &chunk.dynamic_imports {
            let target = target.to_string();
            let mut deps: Vec<serde_json::Value> = Vec::new();
            let mut push_css = |f: &str| {
                if f != file {
                    if let Some(css) = css_of.get(f) {
                        let url: serde_json::Value = with_base(css, base).into();
                        if !deps.contains(&url) {
                            deps.push(url);
                        }
                    }
                }
            };
            push_css(&target);
            for dep in transitive_imports(&target, imports_map) {
                push_css(&dep);
            }
            let key = target.rsplit('/').next().unwrap_or(&target).to_string();
            manifest.insert(key, serde_json::Value::Array(deps));
        }
        let mut urls: Vec<serde_json::Value> = Vec::new();
        if !chunk.is_entry {
            urls.push(with_base(&file, base).into());
            if let Some(css) = css_of.get(file.as_str()) {
                urls.push(with_base(css, base).into());
            }
        } else if let Some(css) = combined_css {
            urls.push(with_base(css, base).into());
        }
        // importedAssets are listed for entry and non-entry chunks alike (Vite).
        if let Some(assets) = imported_assets.get(file.as_str()) {
            for a in assets {
                urls.push(with_base(a, base).into());
            }
        }
        for module_id in &chunk.modules.keys {
            let mid = module_id.to_string();
            let Ok(rel) = Path::new(&mid).strip_prefix(root) else {
                continue;
            };
            let key = rel.to_string_lossy().replace('\\', "/");
            let entry = manifest
                .entry(key)
                .or_insert_with(|| serde_json::Value::Array(Vec::new()));
            if let Some(list) = entry.as_array_mut() {
                for u in &urls {
                    if !list.contains(u) {
                        list.push(u.clone());
                    }
                }
            }
        }
    }
    manifest
}

struct ManifestEntry {
    /// Manifest key: the entry's `src` path, or `_<filename>` for a shared chunk.
    key: String,
    name: String,
    file: String,
    src: Option<String>,
    is_entry: bool,
    is_dynamic_entry: bool,
    /// Statically-imported chunk filenames (remapped to manifest keys on output).
    imports: Vec<String>,
    dynamic_imports: Vec<String>,
    css: Vec<String>,
    /// Non-CSS asset files imported by this chunk (Vite's `importedAssets`).
    assets: Vec<String>,
}

/// A manifest row for an emitted asset (Vite: keyed by its original file name
/// relative to root, or `_<basename>` when unknown).
struct ManifestAsset {
    key: String,
    file: String,
    name: Option<String>,
}

/// A manifest row for an emitted stylesheet, keyed by its root-relative source
/// path (`_<basename>` for CSS that has no file, e.g. plugin-generated).
fn push_css_manifest_row(rows: &mut Vec<ManifestAsset>, root: &Path, src: &str, css_name: &str) {
    let src = src.split(['?', '#']).next().unwrap_or(src);
    let key = match Path::new(src).strip_prefix(root) {
        Ok(rel) => rel.to_string_lossy().replace('\\', "/"),
        Err(_) => format!(
            "_{}",
            Path::new(css_name)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(css_name)
        ),
    };
    if rows.iter().any(|r| r.key == key) {
        return;
    }
    rows.push(ManifestAsset {
        key,
        file: css_name.to_string(),
        name: Path::new(css_name)
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string),
    });
}

/// Vite's `viteMetadata.importedAssets` per chunk: an asset a module imported
/// (`import url from './big.png'`) was emitted with the module's root-relative
/// path as its original file name, so the chunk containing that module
/// imported it. Keyed by chunk file name.
fn chunk_imported_assets(
    bundle: &[rolldown_common::Output],
) -> std::collections::HashMap<String, Vec<String>> {
    use rolldown_common::Output;
    let mut by_original: Vec<(&str, String)> = Vec::new();
    for out in bundle {
        let Output::Asset(asset) = out else { continue };
        if asset.names.is_empty() {
            continue;
        }
        if let Some(orig) = asset.original_file_names.first() {
            by_original.push((orig.as_str(), asset.filename.to_string()));
        }
    }
    let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    if by_original.is_empty() {
        return map;
    }
    for out in bundle {
        let Output::Chunk(chunk) = out else { continue };
        let mut assets: Vec<String> = Vec::new();
        for id in &chunk.module_ids {
            let id = id.as_ref();
            let rel = id.split(['?', '#']).next().unwrap_or(id);
            for (orig, file) in &by_original {
                if rel.ends_with(orig) && !assets.contains(file) {
                    assets.push(file.clone());
                }
            }
        }
        if !assets.is_empty() {
            map.insert(chunk.filename.to_string(), assets);
        }
    }
    map
}

/// Asset rows for the manifest, and each chunk's `assets` list (Vite keys an
/// asset row by its original file name, `_<basename>` when unknown).
fn manifest_asset_rows(
    bundle: &[rolldown_common::Output],
    entries: &mut [ManifestEntry],
    rows: &mut Vec<ManifestAsset>,
) {
    use rolldown_common::Output;
    for out in bundle {
        let Output::Asset(asset) = out else { continue };
        if asset.names.is_empty() {
            continue;
        }
        let file = asset.filename.to_string();
        let key = match asset.original_file_names.first() {
            Some(orig) => orig.clone(),
            None => format!(
                "_{}",
                Path::new(&file)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&file)
            ),
        };
        // A JS chunk generated from the same source keeps the key (Vite).
        if entries.iter().any(|e| e.key == key) || rows.iter().any(|r| r.key == key) {
            continue;
        }
        rows.push(ManifestAsset {
            key,
            file,
            name: asset.names.first().cloned(),
        });
    }
    for (file, assets) in chunk_imported_assets(bundle) {
        if let Some(entry) = entries.iter_mut().find(|e| e.file == file) {
            entry.assets = assets;
        }
    }
}

// Build Vite-manifest entries directly from a bundle (chunks only; CSS is added
// later in the output stage). Used to expose `.vite/manifest.json` inside the
// generateBundle bundle for plugins that read it (e.g. @crxjs).
fn manifest_entries_from_bundle(
    bundle: &[rolldown_common::Output],
    root: &Path,
) -> Vec<ManifestEntry> {
    use rolldown_common::Output;
    let mut entries = Vec::new();
    for out in bundle {
        let Output::Chunk(chunk) = out else { continue };
        let filename = chunk.filename.to_string();
        let src = if chunk.is_entry {
            chunk.facade_module_id.as_ref().and_then(|f| {
                Path::new(f.as_ref())
                    .strip_prefix(root)
                    .ok()
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
            })
        } else {
            None
        };
        let key = src.clone().unwrap_or_else(|| {
            format!(
                "_{}",
                Path::new(&filename)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&filename)
            )
        });
        entries.push(ManifestEntry {
            key,
            name: chunk.name.to_string(),
            file: filename,
            src,
            is_entry: chunk.is_entry,
            is_dynamic_entry: chunk.is_dynamic_entry,
            imports: chunk.imports.iter().map(|i| i.to_string()).collect(),
            dynamic_imports: chunk
                .dynamic_imports
                .iter()
                .map(|i| i.to_string())
                .collect(),
            css: Vec::new(),
            assets: Vec::new(),
        });
    }
    entries
}

// serialize_bundle plus a synthetic `.vite/manifest.json` asset, so a plugin's
// generateBundle can read the Vite build manifest from the bundle the way it
// would under Vite (with build.manifest enabled).
fn serialize_bundle_with_vite_manifest(bundle: &[rolldown_common::Output], root: &Path) -> String {
    let base = serialize_bundle(bundle);
    let mut entries = manifest_entries_from_bundle(bundle, root);
    let mut assets = Vec::new();
    manifest_asset_rows(bundle, &mut entries, &mut assets);
    let manifest = build_manifest(&entries, &assets).to_string();
    let mut val: serde_json::Value =
        serde_json::from_str(&base).unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = val.as_object_mut() {
        obj.insert(
            ".vite/manifest.json".to_string(),
            serde_json::json!({
                "type": "asset",
                "fileName": ".vite/manifest.json",
                "name": "manifest.json",
                "source": manifest,
            }),
        );
    }
    val.to_string()
}

fn build_manifest(entries: &[ManifestEntry], assets: &[ManifestAsset]) -> serde_json::Value {
    // Vite's manifest references imports/dynamicImports by manifest KEY, not by
    // output filename, so build a filename -> key map first.
    let file_to_key: std::collections::HashMap<&str, &str> = entries
        .iter()
        .map(|e| (e.file.as_str(), e.key.as_str()))
        .collect();
    let remap = |files: &[String]| -> Vec<serde_json::Value> {
        files
            .iter()
            .filter_map(|f| {
                file_to_key
                    .get(f.as_str())
                    .map(|k| serde_json::Value::from(*k))
            })
            .collect()
    };
    let mut map = serde_json::Map::new();
    for e in entries {
        let mut row = serde_json::Map::new();
        row.insert("file".into(), e.file.clone().into());
        row.insert("name".into(), e.name.clone().into());
        if let Some(src) = &e.src {
            row.insert("src".into(), src.clone().into());
        }
        if e.is_entry {
            row.insert("isEntry".into(), true.into());
        }
        if e.is_dynamic_entry {
            row.insert("isDynamicEntry".into(), true.into());
        }
        let imports = remap(&e.imports);
        if !imports.is_empty() {
            row.insert("imports".into(), imports.into());
        }
        let dyn_imports = remap(&e.dynamic_imports);
        if !dyn_imports.is_empty() {
            row.insert("dynamicImports".into(), dyn_imports.into());
        }
        if !e.css.is_empty() {
            row.insert("css".into(), e.css.clone().into());
        }
        if !e.assets.is_empty() {
            row.insert("assets".into(), e.assets.clone().into());
        }
        map.insert(e.key.clone(), serde_json::Value::Object(row));
    }
    for a in assets {
        let mut row = serde_json::Map::new();
        row.insert("file".into(), a.file.clone().into());
        row.insert("src".into(), a.key.clone().into());
        if let Some(name) = &a.name {
            row.insert("name".into(), name.clone().into());
        }
        map.insert(a.key.clone(), serde_json::Value::Object(row));
    }
    serde_json::Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        // pid-keyed and cleaned before use, never after: a later run reclaims
        // the dir, and nothing deletes a directory the process cwd (moved by
        // an in-process host's boot chdir) might still point at.
        let dir = std::env::temp_dir().join(format!("oj-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    // The generic SSR prod bundle executes under Node, so a runner-backed ssr
    // environment's condition list (workerd-shaped, the plugin runtime's) must
    // not steer it: same source selection as the dev loader and the unbundled
    // SSR resolver — runner-backed takes Vite's Node server semantics, a plain
    // config's list passes verbatim.
    #[test]
    fn ssr_rolldown_conditions_swap_to_node_semantics_when_runner_backed() {
        let from = |json: &str| -> oj_config::OjConfig { serde_json::from_str(json).unwrap() };
        let root = Path::new("/some/app");

        let runner_backed = from(
            r#"{ "ssr": { "runnerBacked": true, "resolve": {
                "conditions": ["workerd", "worker", "module", "browser", "development|production"] } } }"#,
        );
        let conditions = rolldown_resolve(root, &runner_backed, "ssr")
            .unwrap()
            .condition_names
            .unwrap();
        assert_eq!(
            conditions,
            oj_config::node_server_conditions(&runner_backed, false)
        );
        assert!(conditions.contains(&"node".to_string()));
        assert!(!conditions.contains(&"workerd".to_string()));
        assert!(!conditions.contains(&"browser".to_string()));

        // Not runner-backed: the user's ssr list passes verbatim (prod-mapped).
        let plain = from(
            r#"{ "ssr": { "resolve": { "conditions": ["custom", "development|production"] } } }"#,
        );
        let conditions = rolldown_resolve(root, &plain, "ssr")
            .unwrap()
            .condition_names
            .unwrap();
        assert!(conditions.contains(&"custom".to_string()));
        assert!(conditions.contains(&"production".to_string()));
        assert!(!conditions.contains(&"node".to_string()));

        // The client environment never takes the swap.
        let conditions = rolldown_resolve(root, &runner_backed, "client")
            .unwrap()
            .condition_names
            .unwrap();
        assert!(conditions.contains(&"browser".to_string()));
        assert!(!conditions.contains(&"node".to_string()));

        // `ssr.target: "webworker"`: the ssr bundle itself targets a worker
        // runtime and never executes under Node, so even runner-backed it
        // keeps the environment's own conditions instead of the Node swap.
        let worker_target = from(
            r#"{ "ssr": { "runnerBacked": true, "target": "webworker", "resolve": {
                "conditions": ["workerd", "worker", "module", "browser", "development|production"] } } }"#,
        );
        let conditions = rolldown_resolve(root, &worker_target, "ssr")
            .unwrap()
            .condition_names
            .unwrap();
        assert!(conditions.contains(&"workerd".to_string()));
        assert!(!conditions.contains(&"node".to_string()));

        // ... and a node (default) target still swaps.
        let node_target = from(
            r#"{ "ssr": { "runnerBacked": true, "target": "node", "resolve": {
                "conditions": ["workerd", "development|production"] } } }"#,
        );
        let conditions = rolldown_resolve(root, &node_target, "ssr")
            .unwrap()
            .condition_names
            .unwrap();
        assert_eq!(
            conditions,
            oj_config::node_server_conditions(&node_target, false)
        );
        assert!(!conditions.contains(&"workerd".to_string()));
    }

    // Vite's pkg.main fallback at the build seam: dev's OjResolver appends
    // "main" to an adopted Vite-shaped (main-less) mainFields list, so the
    // rolldown build must resolve with the same appended list — or the same
    // import resolves in dev and fails in the build.
    #[test]
    fn rolldown_main_fields_carry_vites_main_fallback() {
        let from = |json: &str| -> oj_config::OjConfig { serde_json::from_str(json).unwrap() };
        let root = Path::new("/some/app");
        let vite_shaped = from(
            r#"{ "resolve": { "mainFields": ["browser", "module", "jsnext:main", "jsnext"] } }"#,
        );
        let fields = rolldown_resolve(root, &vite_shaped, "client")
            .unwrap()
            .main_fields
            .unwrap();
        assert_eq!(
            fields,
            ["browser", "module", "jsnext:main", "jsnext", "main"].map(String::from)
        );
        // A list already naming main is untouched (no duplicate, no reorder).
        let named = from(r#"{ "resolve": { "mainFields": ["main", "module"] } }"#);
        let fields = rolldown_resolve(root, &named, "client")
            .unwrap()
            .main_fields
            .unwrap();
        assert_eq!(fields, ["main", "module"].map(String::from));
        // No user list: rolldown keeps its own defaults (None passes through).
        assert!(rolldown_resolve(root, &from("{}"), "client")
            .unwrap()
            .main_fields
            .is_none());
    }

    #[test]
    fn out_dir_emptiable_follows_vite_and_never_counts_root_as_inside() {
        let root = scratch("emptiable-root");
        let outside = scratch("emptiable-outside");
        let inside = root.join("dist");
        fs::create_dir_all(&inside).unwrap();
        // Vite's default: inside root is emptied ...
        assert!(out_dir_emptiable(&root, &inside, None, false));
        // ... an outDir equal to root is NOT inside (startsWith(root + "/")) ...
        assert!(!out_dir_emptiable(&root, &root, None, false));
        // ... and an outside one is left alone.
        assert!(!out_dir_emptiable(&root, &outside, None, false));
        // An explicit setting wins either way.
        assert!(out_dir_emptiable(&root, &outside, Some(true), false));
        assert!(!out_dir_emptiable(&root, &inside, Some(false), false));
    }

    #[test]
    fn prepare_out_dir_refuses_to_empty_the_project_root_but_still_empties_dist() {
        let root = scratch("prepare-root");
        fs::write(root.join("index.html"), "<html></html>").unwrap();
        fs::write(root.join("main.js"), "export {};").unwrap();

        // outDir == root with an explicit emptyOutDir would delete the sources the
        // build reads; refuse instead of destroying the project.
        let err = prepare_out_dir(&root, &root, Some(true))
            .unwrap_err()
            .to_string();
        assert!(err.contains("project root"), "{err}");
        assert!(root.join("index.html").exists() && root.join("main.js").exists());

        // With Vite's default rule an equal path is simply not emptied.
        prepare_out_dir(&root, &root, None).unwrap();
        assert!(root.join("index.html").exists() && root.join("main.js").exists());

        // A real inside outDir is still emptied, keeping only .git.
        let dist = root.join("dist");
        fs::create_dir_all(dist.join(".git")).unwrap();
        fs::write(dist.join("stale.js"), "old").unwrap();
        prepare_out_dir(&root, &dist, None).unwrap();
        assert!(!dist.join("stale.js").exists());
        assert!(dist.join(".git").is_dir());
    }

    #[tokio::test]
    async fn module_preload_configuration_controls_production_links() {
        for (index, (config_name, config, expected)) in [
            (
                "oj.config.json",
                r#"{"build":{"modulePreload":false}}"#,
                false,
            ),
            (
                "vite.config.mjs",
                "export default { build: { modulePreload: false } };",
                false,
            ),
            (
                "oj.config.json",
                r#"{"build":{"modulePreload":{"polyfill":false}}}"#,
                true,
            ),
            ("oj.config.json", "{}", true),
        ]
        .into_iter()
        .enumerate()
        {
            let suffix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("oj-module-preload-{suffix}-{index}"));
            fs::create_dir_all(&root).unwrap();
            fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
            fs::write(root.join(config_name), config).unwrap();
            fs::write(
                root.join("index.html"),
                r#"<html><head></head><body>
                    <script type="module" src="/first.js"></script>
                    <script type="module" src="/second.js"></script>
                </body></html>"#,
            )
            .unwrap();
            fs::write(
                root.join("first.js"),
                r#"import { shared } from "./shared.js"; window.first = shared;"#,
            )
            .unwrap();
            fs::write(
                root.join("second.js"),
                r#"import { shared } from "./shared.js"; window.second = shared;"#,
            )
            .unwrap();
            fs::write(root.join("shared.js"), r#"export const shared = "ready";"#).unwrap();

            build(root.clone(), Some("production"), CliOptions::default())
                .await
                .expect("synthetic shared-chunk fixture should build");

            let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
            assert_eq!(
                html.contains("rel=\"modulepreload\""),
                expected,
                "{config_name}: {config}\n{html}"
            );
        }
    }

    #[test]
    fn expand_import_meta_reports_only_real_changes() {
        // No marker at all.
        assert_eq!(
            expand_import_meta("/app/src/a.ts", "export const a = 1;\n"),
            None
        );
        // A plain string-literal dynamic import is left alone by the expander,
        // so the hook must not report it as changed (the sourcemap regression).
        assert_eq!(
            expand_import_meta(
                "/app/src/r.tsx",
                "const P = lazy(() => import(\"./pages/P\"));\n"
            ),
            None
        );
        // node_modules is excluded from dynamic-import-vars expansion.
        assert_eq!(
            expand_import_meta(
                "/app/node_modules/x/index.js",
                "export const c = (n) => import(`./c/${n}.js`);\n"
            ),
            None
        );

        // A variable dynamic import that matches files really changes.
        let dir = scratch("expand-import-meta");
        fs::create_dir_all(dir.join("locales")).unwrap();
        fs::write(dir.join("locales/en.json"), "{}").unwrap();
        let id = dir.join("main.js").to_string_lossy().into_owned();
        let out = expand_import_meta(&id, "const load = (l) => import(`./locales/${l}.json`);\n")
            .expect("matching variable dynamic import expands");
        assert!(
            out.contains(r#"case "./locales/en.json": return import("./locales/en.json")"#),
            "{out}"
        );
        assert!(
            out.contains("${l}"),
            "original template kept as runtime arg: {out}"
        );
        assert!(out.contains("Unknown variable dynamic import"), "{out}");
        fs::remove_dir_all(&dir).unwrap();

        // `new URL(..., import.meta.url)` really changes.
        let out = expand_import_meta(
            "/app/src/main.js",
            "const w = new Worker(new URL(\"./w.ts\", import.meta.url), { type: \"module\" });\nconst s = new SharedWorker(new URL(\"./w.ts\", import.meta.url));\n",
        )
        .expect("new URL asset expands");
        assert!(
            out.contains("import __oj_worker_0 from \"./w.ts?worker&url\""),
            "{out}"
        );
        assert!(
            out.contains("import __oj_worker_1 from \"./w.ts?sharedworker&url\""),
            "{out}"
        );
        assert!(
            out.contains("new Worker(__oj_worker_0, { type: \"module\" })"),
            "{out}"
        );
        assert!(out.contains("new SharedWorker(__oj_worker_1)"), "{out}");
        assert!(
            !out.contains("w.ts?url"),
            "the worker entry is not a plain asset: {out}"
        );
    }

    // The transform hook must answer `None` for a module it did not change: a
    // `Some(code)` with the default map makes rolldown push
    // `SourcemapChainElement::Omitted`, which empties that module's mappings, so
    // every route-split file with a plain `import("./x")` silently lost its
    // sourcemap when `build.sourcemap` was on.
    #[tokio::test]
    async fn production_sourcemap_keeps_mappings_for_plain_dynamic_import_modules() {
        let root = scratch("smap-dynimp");
        fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        fs::write(
            root.join("oj.config.json"),
            r#"{"build":{"sourcemap":true,"minify":false}}"#,
        )
        .unwrap();
        fs::write(
            root.join("index.html"),
            r#"<html><head></head><body><script type="module" src="/src/main.js"></script></body></html>"#,
        )
        .unwrap();
        fs::create_dir_all(root.join("src/pages")).unwrap();
        fs::write(
            root.join("src/pages/about.js"),
            "export const name = \"about\";\n",
        )
        .unwrap();
        fs::write(
            root.join("src/main.js"),
            "export function load() {\n  return import(\"./pages/about.js\");\n}\nwindow.__LOAD = load;\n",
        )
        .unwrap();

        build(root.clone(), Some("production"), CliOptions::default())
            .await
            .expect("plain dynamic import fixture should build");

        let map_path = fs::read_dir(root.join("dist/assets"))
            .unwrap()
            .filter_map(|e| e.ok().map(|e| e.path()))
            .find(|p| {
                let name = p.file_name().unwrap().to_string_lossy();
                name.starts_with("main-") && name.ends_with(".map")
            })
            .expect("main-*.map emitted");
        let map: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(&map_path).unwrap()).unwrap();
        let sources: Vec<&str> = map["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|s| s.as_str())
            .collect();
        // Load-bearing: a multi-module chunk would carry sibling mappings anyway,
        // so check the dynamic-import module itself is still mapped.
        assert!(
            sources.iter().any(|s| s.ends_with("src/main.js")),
            "src/main.js missing from {}: {sources:?}",
            map_path.display()
        );
        assert!(
            !map["mappings"].as_str().unwrap_or("").is_empty(),
            "empty mappings in {}",
            map_path.display()
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn module_type_for_id_infers_from_extension() {
        use rolldown_common::ModuleType;
        // Vite derives the transform lang from the id's extension (query
        // stripped); oj mirrors it for plugin-loaded/virtual modules.
        assert!(matches!(
            module_type_for_id("~icons/ph/x.jsx"),
            ModuleType::Jsx
        ));
        assert!(matches!(
            module_type_for_id("/v/comp.tsx?used"),
            ModuleType::Tsx
        ));
        assert!(matches!(
            module_type_for_id("virtual:mod.ts"),
            ModuleType::Ts
        ));
        assert!(matches!(module_type_for_id("a.mts"), ModuleType::Ts));
        assert!(matches!(module_type_for_id("a.cts"), ModuleType::Ts));
        assert!(matches!(
            module_type_for_id("data.json#x"),
            ModuleType::Json
        ));
        assert!(matches!(module_type_for_id("mod.mjs"), ModuleType::Js));
        assert!(matches!(module_type_for_id("plain.js"), ModuleType::Js));
        // Extensionless virtual ids default to JavaScript.
        assert!(matches!(
            module_type_for_id("\0virtual:store"),
            ModuleType::Js
        ));
    }

    #[test]
    fn normalize_input_entries_covers_string_array_object() {
        assert_eq!(
            normalize_input_entries(&serde_json::json!("index.html")),
            vec![("index".to_string(), "index.html".to_string())]
        );
        assert_eq!(
            normalize_input_entries(&serde_json::json!("src/pages/options/index.html")),
            vec![(
                "index".to_string(),
                "src/pages/options/index.html".to_string()
            )]
        );
        assert_eq!(
            normalize_input_entries(&serde_json::json!(["a.html", "b.html"])),
            vec![
                ("a".to_string(), "a.html".to_string()),
                ("b".to_string(), "b.html".to_string()),
            ]
        );
        let obj = normalize_input_entries(&serde_json::json!({
            "admin": "src/admin.html",
            "app": "src/app.html",
        }));
        assert!(obj.contains(&("admin".to_string(), "src/admin.html".to_string())));
        assert!(obj.contains(&("app".to_string(), "src/app.html".to_string())));
        assert_eq!(obj.len(), 2);
    }

    #[test]
    fn resolve_html_ref_is_relative_to_the_page_or_root() {
        let root = Path::new("/proj");
        let page_dir = Path::new("/proj/src/pages/options");
        // Root-absolute refs resolve against the project root.
        assert_eq!(
            resolve_html_ref("/src/main.tsx", page_dir, root),
            PathBuf::from("/proj/src/main.tsx")
        );
        // Relative refs resolve against the HTML file's own directory.
        assert_eq!(
            resolve_html_ref("./index.tsx", page_dir, root),
            PathBuf::from("/proj/src/pages/options/index.tsx")
        );
        // `..` is resolved lexically so the path matches rolldown's facade id.
        assert_eq!(
            resolve_html_ref("../../main.tsx", page_dir, root),
            PathBuf::from("/proj/src/main.tsx")
        );
        assert_eq!(
            resolve_html_ref("main.tsx", page_dir, root),
            PathBuf::from("/proj/src/pages/options/main.tsx")
        );
    }

    #[test]
    fn insert_before_head_places_snippet_or_prepends() {
        assert_eq!(
            insert_before_head("<head></head><body></body>", "<x>"),
            "<head><x>\n</head><body></body>"
        );
        assert_eq!(
            insert_before_head("<body></body>", "<x>"),
            "<x>\n<body></body>"
        );
    }

    #[tokio::test]
    async fn copy_public_dir_configuration_controls_production_assets() {
        for (index, (config_name, config, copied)) in [
            (
                "oj.config.json",
                r#"{"build":{"copyPublicDir":false}}"#,
                false,
            ),
            (
                "vite.config.mjs",
                "export default { build: { copyPublicDir: false } };",
                false,
            ),
            (
                "oj.config.json",
                r#"{"build":{"copyPublicDir":true}}"#,
                true,
            ),
            ("oj.config.json", "{}", true),
        ]
        .into_iter()
        .enumerate()
        {
            let suffix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!("oj-copy-public-dir-{suffix}-{index}"));
            fs::create_dir_all(root.join("public")).unwrap();
            fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
            fs::write(root.join(config_name), config).unwrap();
            fs::write(
                root.join("index.html"),
                r#"<html><body><script type="module" src="/main.js"></script></body></html>"#,
            )
            .unwrap();
            fs::write(root.join("main.js"), "window.ready = true;").unwrap();
            fs::write(root.join("public/asset.txt"), "public asset").unwrap();

            build(root.clone(), Some("production"), CliOptions::default())
                .await
                .expect("synthetic public-directory fixture should build");

            assert_eq!(
                root.join("dist/asset.txt").is_file(),
                copied,
                "{config_name}: {config}"
            );
            assert!(root.join("dist/index.html").is_file());
        }
    }

    /// Vite's define plugin: a browser bundle gets `process.env` -> `{}` (so an
    /// arbitrary `process.env.X` is `undefined`, not a ReferenceError) and
    /// NODE_ENV inlined; a server bundle keeps `process.env` (keepProcessEnv).
    #[tokio::test]
    async fn client_build_defines_process_env_to_an_empty_object_and_ssr_keeps_it() {
        let root = scratch("process-env-define");
        fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        fs::write(
            root.join("index.html"),
            r#"<html><body><script type="module" src="/main.js"></script></body></html>"#,
        )
        .unwrap();
        fs::write(
            root.join("main.js"),
            "export const probe = [process.env.SOME_FLAG, globalThis.process.env.OTHER, process.env.NODE_ENV];\n\
             window.probe = probe;\n",
        )
        .unwrap();
        fs::write(
            root.join("server.js"),
            "export const flag = process.env.SOME_FLAG;\n",
        )
        .unwrap();

        build(root.clone(), Some("production"), CliOptions::default())
            .await
            .expect("client build");
        let mut client = String::new();
        for entry in fs::read_dir(root.join("dist/assets")).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().is_some_and(|e| e == "js") {
                client.push_str(&fs::read_to_string(p).unwrap());
            }
        }
        assert!(
            !client.contains("process.env"),
            "client bundle still reads process.env:\n{client}"
        );
        assert!(client.contains("\"production\"") || client.contains("`production`"));

        build_ssr(
            &root,
            &root.join("dist-ssr"),
            "server.js",
            "production",
            oj_config::Sourcemap::Off,
        )
        .await
        .expect("ssr build");
        let server = fs::read_to_string(root.join("dist-ssr/server.mjs")).unwrap();
        assert!(
            server.contains("process.env.SOME_FLAG"),
            "ssr bundle must keep process.env:\n{server}"
        );
    }

    /// Vite's build-html asset pass: attribute references to source assets are
    /// emitted hashed (or inlined under assetsInlineLimit, never for a link
    /// icon/manifest or a fragment svg), publicDir references get the base
    /// prefix, srcset candidates are handled one by one, and external,
    /// missing and `vite-ignore` references stay as written.
    #[tokio::test]
    async fn html_asset_attributes_are_rewritten_like_vite() {
        let root = scratch("html-asset-attrs");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join("public")).unwrap();
        fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        fs::write(
            root.join("vite.config.mjs"),
            "export default { base: '/app/' };",
        )
        .unwrap();
        fs::write(root.join("src/big.png"), vec![7u8; 9000]).unwrap();
        fs::write(root.join("src/big-2x.png"), vec![8u8; 9000]).unwrap();
        fs::write(root.join("src/tiny.png"), vec![9u8; 40]).unwrap();
        fs::write(
            root.join("src/icon.svg"),
            "<svg xmlns=\"http://www.w3.org/2000/svg\"><rect id=\"r\"/></svg>",
        )
        .unwrap();
        fs::write(root.join("public/logo.png"), vec![1u8; 100]).unwrap();
        fs::write(root.join("src/main.js"), "console.log(1);").unwrap();
        fs::write(
            root.join("index.html"),
            "<html><head>\n\
             <link rel=\"icon\" href=\"./src/icon.svg\">\n\
             <link rel=\"manifest\" href=\"/manifest.webmanifest\">\n\
             <meta property=\"og:image\" content=\"/src/big.png\">\n\
             <meta name=\"description\" content=\"/src/big.png\">\n\
             </head><body>\n\
             <img src=\"./src/big.png\" alt=\"a\">\n\
             <img src='src/tiny.png' srcset=\"./src/big.png 1x, ./src/big-2x.png 2x\">\n\
             <img src=\"/logo.png\">\n\
             <img src=\"https://example.com/x.png\">\n\
             <img vite-ignore src=\"./src/big.png\">\n\
             <video poster=\"src/big.png\" src=\"/missing.mp4\"></video>\n\
             <svg><use href=\"./src/icon.svg#r\"></use></svg>\n\
             <script type=\"module\" src=\"/src/main.js\"></script></body></html>",
        )
        .unwrap();
        fs::write(root.join("public/manifest.webmanifest"), "{}").unwrap();

        build(root.clone(), Some("production"), CliOptions::default())
            .await
            .expect("html asset fixture should build");
        let html = fs::read_to_string(root.join("dist/index.html")).unwrap();
        let big = fs::read_dir(root.join("dist/assets"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .find(|n| n.starts_with("big-") && !n.starts_with("big-2x") && n.ends_with(".png"))
            .expect("big.png emitted hashed");
        let big_url = format!("/app/assets/{big}");
        assert!(
            html.contains(&format!("<img src=\"{big_url}\" alt=\"a\">")),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "<meta property=\"og:image\" content=\"{big_url}\">"
            )),
            "{html}"
        );
        assert!(
            html.contains("<meta name=\"description\" content=\"/src/big.png\">"),
            "{html}"
        );
        assert!(
            html.contains(&format!(
                "<video poster=\"{big_url}\" src=\"/missing.mp4\">"
            )),
            "{html}"
        );
        assert!(
            html.contains(&format!("srcset=\"{big_url} 1x, /app/assets/big-2x-")),
            "{html}"
        );
        // small: inlined, keeping the page's single quotes
        assert!(html.contains("<img src='data:image/png;base64,"), "{html}");
        // link icon is never inlined, and its fragment use shares the same file
        assert!(
            html.contains("<link rel=\"icon\" href=\"/app/assets/icon-"),
            "{html}"
        );
        assert!(
            html.contains("<use href=\"/app/assets/icon-") && html.contains(".svg#r\">"),
            "{html}"
        );
        // publicDir references get the base prefix
        assert!(
            html.contains("<link rel=\"manifest\" href=\"/app/manifest.webmanifest\">"),
            "{html}"
        );
        assert!(html.contains("<img src=\"/app/logo.png\">"), "{html}");
        // left alone: external, vite-ignore (attribute dropped), missing file
        assert!(
            html.contains("<img src=\"https://example.com/x.png\">"),
            "{html}"
        );
        assert!(html.contains("<img src=\"./src/big.png\">"), "{html}");
        assert!(!html.contains("vite-ignore"), "{html}");
    }

    /// `build.lib` from vite.config: Vite's entry forms, default formats
    /// (es+umd for one entry, es+cjs for several), file names from the package
    /// name and `type`, and `cssFileName`.
    #[tokio::test]
    async fn vite_config_build_lib_is_honored_with_vite_naming() {
        // One string entry in a `type: module` package named `@scope/my-lib`.
        let root = scratch("vite-lib-single");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("package.json"),
            r#"{"name":"@scope/my-lib","type":"module"}"#,
        )
        .unwrap();
        fs::write(
            root.join("src/index.ts"),
            "import './style.css';\nexport const add = (a: number, b: number) => a + b;\n",
        )
        .unwrap();
        fs::write(root.join("src/style.css"), ".a{color:red}").unwrap();
        fs::write(
            root.join("vite.config.mjs"),
            "export default { build: { lib: { entry: 'src/index.ts', name: 'MyLib' } } };",
        )
        .unwrap();
        build(root.clone(), Some("production"), CliOptions::default())
            .await
            .expect("vite.config build.lib should build");
        assert!(
            root.join("dist/my-lib.js").is_file(),
            "es output named after the unscoped package"
        );
        assert!(
            root.join("dist/my-lib.umd.cjs").is_file(),
            "umd is a default format; .cjs under type module"
        );
        assert!(root.join("dist/my-lib.css").is_file());
        assert!(!root.join("dist/index.js").exists());

        // Several aliased entries in a commonjs package: es+cjs, per-entry names.
        let root = scratch("vite-lib-multi");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("package.json"), r#"{"name":"multi"}"#).unwrap();
        fs::write(root.join("src/a.ts"), "export const a = 1;\n").unwrap();
        fs::write(root.join("src/b.ts"), "export const b = 2;\n").unwrap();
        fs::write(
            root.join("vite.config.mjs"),
            "export default { build: { lib: { entry: { main: 'src/a.ts', extra: 'src/b.ts' }, cssFileName: 'theme' } } };",
        )
        .unwrap();
        build(root.clone(), Some("production"), CliOptions::default())
            .await
            .expect("multi-entry lib should build");
        for f in ["main.mjs", "extra.mjs", "main.js", "extra.js"] {
            assert!(root.join("dist").join(f).is_file(), "missing {f}");
        }
        assert!(
            !root.join("dist/main.umd.js").exists(),
            "umd is not a default with several entries"
        );

        // umd without a name is Vite's error, not a silent es-only build.
        let root = scratch("vite-lib-noname");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(root.join("package.json"), r#"{"name":"noname"}"#).unwrap();
        fs::write(root.join("src/index.ts"), "export const a = 1;\n").unwrap();
        fs::write(
            root.join("vite.config.mjs"),
            "export default { build: { lib: { entry: 'src/index.ts', formats: ['umd'] } } };",
        )
        .unwrap();
        let err = build(root.clone(), Some("production"), CliOptions::default())
            .await
            .expect_err("umd needs build.lib.name");
        assert!(err.to_string().contains("build.lib.name"), "{err}");
    }

    /// Vite's `bundleWorkerEntry`: a `?worker&inline` bundle is built under the
    /// app config, so an aliased import, `define` and the client `process.env`
    /// defines resolve the same way inside the worker as in the main bundle.
    #[tokio::test]
    async fn inline_worker_is_bundled_with_the_app_config() {
        let root = scratch("worker-inline-config");
        fs::create_dir_all(root.join("src/lib")).unwrap();
        fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        fs::write(
            root.join("vite.config.mjs"),
            "import path from 'node:path';\n\
             export default { resolve: { alias: { '@': path.resolve(import.meta.dirname, 'src') } }, define: { __APP_VERSION__: '\"1.2.3\"' } };\n",
        )
        .unwrap();
        fs::write(
            root.join("src/lib/util.js"),
            "export const greet = (n) => 'hi ' + n;\n",
        )
        .unwrap();
        fs::write(
            root.join("src/w.js"),
            "import { greet } from '@/lib/util';\n\
             self.postMessage(greet(__APP_VERSION__) + ':' + process.env.NODE_ENV + ':' + String(process.env.NOPE));\n",
        )
        .unwrap();
        fs::write(
            root.join("src/main.js"),
            "import W from './w.js?worker&inline';\nnew W().onmessage = (e) => { document.body.textContent = e.data; };\n",
        )
        .unwrap();
        fs::write(
            root.join("index.html"),
            r#"<html><body><script type="module" src="/src/main.js"></script></body></html>"#,
        )
        .unwrap();
        build(root.clone(), Some("production"), CliOptions::default())
            .await
            .expect("inline worker fixture should build");
        let mut code = String::new();
        for entry in fs::read_dir(root.join("dist/assets")).unwrap() {
            let p = entry.unwrap().path();
            if p.extension().is_some_and(|e| e == "js") {
                code.push_str(&fs::read_to_string(p).unwrap());
            }
        }
        assert!(
            !code.contains("@/lib"),
            "alias not resolved inside the inline worker:\n{code}"
        );
        assert!(
            !code.contains("__APP_VERSION__"),
            "define not applied inside the inline worker:\n{code}"
        );
        assert!(code.contains("1.2.3") && code.contains("hi "), "{code}");
        assert!(
            !code.contains("process.env"),
            "process.env defines missing inside the inline worker:\n{code}"
        );
    }

    /// The plugin host of an SSR (and client-entry) build is told the real mode,
    /// so a plugin's `configResolved(config).mode` under `--mode staging` is
    /// "staging" as in Vite, not a hardcoded "production".
    #[tokio::test]
    async fn ssr_build_plugins_see_the_real_mode() {
        let root = scratch("ssr-plugin-mode");
        fs::write(root.join("package.json"), r#"{"type":"module"}"#).unwrap();
        fs::write(
            root.join("vite.config.mjs"),
            "import fs from 'node:fs';\n\
             export default { plugins: [{ name: 'mode-probe', configResolved(c) { fs.writeFileSync(new URL('./mode.txt', import.meta.url), c.mode); } }] };\n",
        )
        .unwrap();
        fs::write(
            root.join("server.js"),
            "export const render = () => 'ok';\n",
        )
        .unwrap();
        build_ssr(
            &root,
            &root.join("dist-ssr"),
            "server.js",
            "staging",
            oj_config::Sourcemap::Off,
        )
        .await
        .expect("ssr build");
        let seen = fs::read_to_string(root.join("mode.txt")).expect("plugin configResolved ran");
        assert_eq!(seen.trim(), "staging");
    }

    #[test]
    fn manifest_matches_vite_shape() {
        let mk = |key: &str,
                  name: &str,
                  file: &str,
                  src: Option<&str>,
                  is_entry: bool,
                  is_dyn: bool,
                  imports: Vec<&str>,
                  dyn_imports: Vec<&str>,
                  css: Vec<&str>| ManifestEntry {
            key: key.into(),
            name: name.into(),
            file: file.into(),
            src: src.map(str::to_string),
            is_entry,
            is_dynamic_entry: is_dyn,
            imports: imports.into_iter().map(str::to_string).collect(),
            dynamic_imports: dyn_imports.into_iter().map(str::to_string).collect(),
            css: css.into_iter().map(str::to_string).collect(),
            assets: vec![],
        };
        let m = build_manifest(
            &[
                mk(
                    "src/main.tsx",
                    "main",
                    "assets/main-abc123.js",
                    Some("src/main.tsx"),
                    true,
                    false,
                    vec!["assets/vendor-def456.js"],
                    vec!["assets/lazy-xyz.js"],
                    vec!["assets/style-99.css"],
                ),
                mk(
                    "_vendor-def456.js",
                    "vendor",
                    "assets/vendor-def456.js",
                    None,
                    false,
                    false,
                    vec![],
                    vec![],
                    vec![],
                ),
                mk(
                    "_lazy-xyz.js",
                    "lazy",
                    "assets/lazy-xyz.js",
                    None,
                    false,
                    true,
                    vec![],
                    vec![],
                    vec![],
                ),
            ],
            &[],
        );
        let row = &m["src/main.tsx"];
        assert_eq!(row["file"], "assets/main-abc123.js");
        assert_eq!(row["name"], "main");
        assert_eq!(row["src"], "src/main.tsx");
        assert_eq!(row["isEntry"], true);
        // imports/dynamicImports reference manifest KEYS, not output filenames.
        assert_eq!(row["imports"][0], "_vendor-def456.js");
        assert_eq!(row["dynamicImports"][0], "_lazy-xyz.js");
        assert_eq!(row["css"][0], "assets/style-99.css");
        // Non-entry chunk is present, keyed by _<filename>, with no src.
        assert_eq!(m["_vendor-def456.js"]["file"], "assets/vendor-def456.js");
        assert!(m["_vendor-def456.js"].get("src").is_none());
        // Dynamic entry is flagged.
        assert_eq!(m["_lazy-xyz.js"]["isDynamicEntry"], true);
    }

    #[test]
    fn manifest_asset_rows_are_keyed_by_source_and_chunks_list_assets() {
        let root = scratch("manifest-css-rows");
        let mut rows: Vec<ManifestAsset> = Vec::new();
        let src = root.join("src/style.css").display().to_string();
        push_css_manifest_row(&mut rows, &root, &src, "assets/main-abc.css");
        // The same source twice (two chunks sharing a stylesheet) is one row.
        push_css_manifest_row(
            &mut rows,
            &root,
            &format!("{src}?inline"),
            "assets/main-abc.css",
        );
        // CSS without a file under root falls back to `_<basename>`.
        push_css_manifest_row(
            &mut rows,
            &root,
            "/elsewhere/__plugin_css__",
            "assets/gen-1.css",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].key, "src/style.css");
        assert_eq!(rows[1].key, "_gen-1.css");
        let m = build_manifest(
            &[ManifestEntry {
                key: "src/main.ts".into(),
                name: "main".into(),
                file: "assets/main-1.js".into(),
                src: Some("src/main.ts".into()),
                is_entry: true,
                is_dynamic_entry: false,
                imports: vec![],
                dynamic_imports: vec![],
                css: vec!["assets/main-abc.css".into()],
                assets: vec!["assets/big-9.png".into()],
            }],
            &rows,
        );
        assert_eq!(m["src/main.ts"]["assets"][0], "assets/big-9.png");
        assert_eq!(m["src/style.css"]["file"], "assets/main-abc.css");
        assert_eq!(m["src/style.css"]["src"], "src/style.css");
        assert_eq!(m["src/style.css"]["name"], "main-abc.css");
        assert!(m["src/style.css"].get("isEntry").is_none());
        assert_eq!(m["_gen-1.css"]["file"], "assets/gen-1.css");
    }

    #[test]
    fn cli_options_layer_over_the_loaded_config() {
        // Vite's cac: a bare `--manifest` is `true`, `--minify false` is `false`,
        // anything else stays a string (`--sourcemap inline`, `--manifest m.json`).
        assert!(matches!(
            cli_bool_or_str("true"),
            oj_config::BoolOrString::Bool(true)
        ));
        assert!(matches!(
            cli_bool_or_str("false"),
            oj_config::BoolOrString::Bool(false)
        ));
        assert!(
            matches!(cli_bool_or_str("inline"), oj_config::BoolOrString::Str(s) if s == "inline")
        );
        let cli = CliOptions {
            out: Some(PathBuf::from("build-out")),
            base: Some("/app/".into()),
            assets_dir: Some("static".into()),
            assets_inline_limit: Some(10),
            sourcemap: Some("hidden".into()),
            minify: Some("false".into()),
            manifest: Some("true".into()),
            ssr_manifest: Some("ssr.json".into()),
            empty_out_dir: true,
            ..Default::default()
        };
        let mut config = oj_config::OjConfig {
            base: Some("/from-config/".into()),
            build: Some(oj_config::BuildConfig {
                out_dir: Some("dist".into()),
                minify: Some(oj_config::BoolOrString::Bool(true)),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_cli_options_from(&mut config, &cli);
        assert_eq!(config.base.as_deref(), Some("/app/"));
        let b = config.build.as_ref().unwrap();
        assert_eq!(b.out_dir.as_deref(), Some("build-out"));
        assert_eq!(b.assets_inline_limit, Some(10));
        assert_eq!(b.empty_out_dir, Some(true));
        assert!(
            !oj_config::build_minify(&config),
            "--minify false wins over the config's true"
        );
        assert!(
            !oj_config::build_css_minify(&config, false),
            "cssMinify follows minify"
        );
        assert_eq!(
            oj_config::build_sourcemap(&config),
            oj_config::Sourcemap::Hidden
        );
        assert_eq!(oj_config::build_assets_dir(&config), "static");
        assert_eq!(
            oj_config::build_manifest_name(&config).as_deref(),
            Some(".vite/manifest.json")
        );
        assert_eq!(
            oj_config::ssr_manifest_name(&config).as_deref(),
            Some("ssr.json")
        );
    }

    #[test]
    fn rollup_output_options_are_forwarded() {
        let ro = serde_json::json!({
            "treeshake": false,
            "output": {
                "format": "iife", "name": "App", "extend": true,
                "banner": "/* b */", "footer": "/* f */", "intro": "i;", "outro": "o;",
                "globals": { "react": "React" }, "paths": { "react": "https://esm.sh/react" },
                "entryFileNames": "__oj_fn__"
            }
        });
        let ov = ro_output_overrides(Some(&ro));
        assert!(matches!(ov.format, Some(OutputFormat::Iife)));
        assert_eq!(ov.name.as_deref(), Some("App"));
        assert_eq!(ov.extend, Some(true));
        assert_eq!(ov.banner.as_deref(), Some("/* b */"));
        assert_eq!(ov.footer.as_deref(), Some("/* f */"));
        assert_eq!(ov.intro.as_deref(), Some("i;"));
        assert_eq!(ov.outro.as_deref(), Some("o;"));
        assert_eq!(ov.globals.as_ref().unwrap()["react"], "React");
        assert_eq!(ov.paths.as_ref().unwrap()["react"], "https://esm.sh/react");
        assert!(
            ov.inline_dynamic_imports,
            "iife implies a single chunk (Vite: codeSplitting false)"
        );
        assert!(matches!(
            ro_treeshake(Some(&ro)),
            rolldown_common::TreeshakeOptions::Boolean(false)
        ));

        // es stays rolldown's default; `inlineDynamicImports` and `codeSplitting: false` both fold chunks.
        let es = serde_json::json!({ "output": [{ "format": "es", "inlineDynamicImports": true }, { "format": "cjs" }] });
        let ov = ro_output_overrides(Some(&es));
        assert!(ov.format.is_none());
        assert!(ov.inline_dynamic_imports);
        let cs = serde_json::json!({ "output": { "codeSplitting": false } });
        assert!(ro_output_overrides(Some(&cs)).inline_dynamic_imports);
        assert!(!ro_output_overrides(None).inline_dynamic_imports);
        assert!(matches!(
            ro_treeshake(None),
            rolldown_common::TreeshakeOptions::Option(_)
        ));
    }

    #[test]
    fn entry_scripts_get_crossorigin_once() {
        let html = r#"<script type="module" src="/assets/main-1.js"></script><script src="/other.js"></script><script type="module" crossorigin src="/assets/main-1.js"></script>"#;
        let out = add_script_crossorigin(html, "/assets/main-1.js");
        assert_eq!(
            out,
            r#"<script type="module" src="/assets/main-1.js" crossorigin></script><script src="/other.js"></script><script type="module" crossorigin src="/assets/main-1.js"></script>"#
        );
        // Untouched when no tag has that src.
        assert_eq!(add_script_crossorigin(html, "/nope.js"), html);
    }

    #[test]
    fn reporter_gzip_and_chunk_warning_follow_vite() {
        let out = scratch("reporter");
        let big = "x".repeat(20_000);
        fs::write(out.join("a.js"), &big).unwrap();
        assert!(
            gzip_size(big.as_bytes()) < 200,
            "gzip of a repetitive file is small"
        );
        assert_eq!(
            gzip_size(b""),
            20,
            "an empty gzip stream is the 20-byte header"
        );
        let mut emitted = vec![("a.js".to_string(), 20_000), ("b.js".to_string(), 30)];
        report_build_output(&out, &mut emitted, true);
        assert_eq!(emitted[0].0, "a.js", "largest first");
        let w = chunk_size_warning(500.0);
        assert!(w.contains("larger than 500 kB after minification"));
        assert!(w.contains("build.chunkSizeWarningLimit"));
    }

    #[test]
    fn worker_only_chunks_exclude_anything_a_page_entry_reaches() {
        let entries = vec![("main.js".to_string(), false), ("w.js".to_string(), true)];
        let imports: std::collections::HashMap<String, Vec<String>> = [
            ("main.js".to_string(), vec!["shared.js".to_string()]),
            (
                "w.js".to_string(),
                vec!["shared.js".to_string(), "wlib.js".to_string()],
            ),
        ]
        .into_iter()
        .collect();
        let dynamics: std::collections::HashMap<String, Vec<String>> =
            [("w.js".to_string(), vec!["wlazy.js".to_string()])]
                .into_iter()
                .collect();
        let only = worker_only_chunks(&entries, &imports, &dynamics);
        assert_eq!(
            only.into_iter().collect::<Vec<_>>(),
            vec![
                "w.js".to_string(),
                "wlazy.js".to_string(),
                "wlib.js".to_string()
            ],
            "shared.js is reachable from the page, so it keeps the document helpers"
        );
        assert!(
            worker_only_chunks(&[("main.js".to_string(), false)], &imports, &dynamics).is_empty()
        );
    }

    #[test]
    fn minify_false_still_eliminates_dead_code() {
        assert!(matches!(
            rolldown_minify(true),
            RawMinifyOptions::Bool(true)
        ));
        assert!(matches!(
            rolldown_minify(false),
            RawMinifyOptions::DeadCodeEliminationOnly
        ));
        assert_eq!(
            import_meta_hot_define(),
            ("import.meta.hot".to_string(), "undefined".to_string())
        );
    }

    #[test]
    fn non_entry_omits_isentry_and_empty_fields() {
        let m = build_manifest(
            &[ManifestEntry {
                key: "_chunk-1.js".into(),
                name: "chunk".into(),
                file: "assets/chunk-1.js".into(),
                src: None,
                is_entry: false,
                is_dynamic_entry: false,
                imports: vec![],
                dynamic_imports: vec![],
                css: vec![],
                assets: vec![],
            }],
            &[],
        );
        let row = m["_chunk-1.js"].as_object().unwrap();
        assert!(!row.contains_key("assets"));
        assert!(!row.contains_key("isEntry"));
        assert!(!row.contains_key("isDynamicEntry"));
        assert!(!row.contains_key("imports"));
        assert!(!row.contains_key("dynamicImports"));
        assert!(!row.contains_key("css"));
        assert!(!row.contains_key("src"));
    }

    #[test]
    fn lib_format_mapping() {
        assert_eq!(lib_format("es"), Some(false));
        assert_eq!(lib_format("esm"), Some(false));
        assert_eq!(lib_format("cjs"), Some(false));
        assert_eq!(lib_format("umd"), Some(true));
        assert_eq!(lib_format("iife"), Some(true));
        assert_eq!(lib_format("amd"), None);
    }

    #[test]
    fn base_normalization_and_application() {
        assert_eq!(normalize_base("/"), "/");
        assert_eq!(normalize_base(""), "./");
        assert_eq!(normalize_base("./"), "./");
        assert_eq!(normalize_base("app"), "/app/");
        assert_eq!(normalize_base("/app"), "/app/");
        assert_eq!(normalize_base("/app/"), "/app/");
        assert_eq!(with_base("assets/x-h.js", "/"), "/assets/x-h.js");
        assert_eq!(with_base("assets/x-h.js", "/app/"), "/app/assets/x-h.js");
        assert_eq!(with_base("assets/x-h.js", "./"), "./assets/x-h.js");
    }

    #[test]
    fn relative_base_is_computed_per_page_stylesheet_and_chunk() {
        assert_eq!(page_base("/app/", "nested/index.html"), "/app/");
        assert_eq!(page_base("./", "index.html"), "./");
        assert_eq!(page_base("./", "nested/index.html"), "../");
        assert_eq!(page_base("./", "a/b/index.html"), "../../");
        assert_eq!(css_asset_url("assets/img-h.png", "./", None), "./img-h.png");
        assert_eq!(
            css_asset_url("assets/img-h.png", "/app/", None),
            "/app/assets/img-h.png"
        );
        assert_eq!(
            css_asset_url(
                "static/img-h.png",
                "./",
                Some("static/[name]-[hash][extname]")
            ),
            "./img-h.png"
        );
        assert_eq!(
            css_asset_url("assets/img-h.png", "./", Some("css/[name][extname]")),
            "../assets/img-h.png"
        );
        assert_eq!(
            relative_chunk_path("assets/main-h.js", "assets/lazy-h.js"),
            "./lazy-h.js"
        );
        assert_eq!(
            relative_chunk_path("main-h.js", "assets/lazy-h.js"),
            "./assets/lazy-h.js"
        );
        assert_eq!(
            relative_chunk_path("assets/a/main-h.js", "assets/lazy-h.js"),
            "../lazy-h.js"
        );
    }

    #[test]
    fn module_script_srcs_only_module_type_absolute() {
        let html = r#"<script type="module" src="/src/main.tsx"></script>
                      <script src="/legacy.js"></script>
                      <script type="module" src="https://cdn/x.js"></script>"#;
        let srcs = module_script_srcs(html);
        assert_eq!(srcs, vec!["/src/main.tsx"]);
    }

    #[test]
    fn module_script_srcs_accepts_relative_entries() {
        let html = r#"<script type="module" src="src/index.tsx"></script>"#;
        assert_eq!(module_script_srcs(html), vec!["src/index.tsx"]);
        let html2 = r#"<script type="module" src="./app/main.ts"></script>"#;
        assert_eq!(module_script_srcs(html2), vec!["./app/main.ts"]);
    }

    #[test]
    fn is_server_module_path_matches_server_suffixes() {
        for yes in [
            "api.server.ts",
            "a/b/auth.server.tsx",
            "x.server.js",
            "y.server.jsx",
        ] {
            assert!(
                is_server_module_path(yes),
                "{yes} should be a server module"
            );
        }
        for no in [
            "api.ts",
            "server.ts",
            "api.server.css",
            "a.serverx.ts",
            "note.server.md",
        ] {
            assert!(
                !is_server_module_path(no),
                "{no} should not be a server module"
            );
        }
    }

    #[test]
    fn server_fn_prod_stub_emits_an_rpc_per_export() {
        let out = server_fn_prod_stub(&["getUser".into(), "default".into()], "/api.server.ts");
        assert!(
            out.contains("const __ojCall ="),
            "the fetch helper is inlined: {out}"
        );
        assert!(
            out.contains(
                r#"export const getUser = (...a) => __ojCall("/api.server.ts", "getUser", a);"#
            ),
            "named export stub: {out}"
        );
        assert!(
            out.contains(r#"export default (...a) => __ojCall("/api.server.ts", "default", a);"#),
            "default export stub: {out}"
        );
        let empty = server_fn_prod_stub(&[], "/x.server.ts");
        assert!(empty.contains("__ojCall"));
        assert!(
            !empty.contains("export "),
            "no exports means no stubs: {empty}"
        );
    }

    /// Whatever an inlined asset's bytes are, the module oj emits for it has to
    /// parse. The url is derived from a file oj did not write.
    fn parses(code: &str) -> bool {
        oj_compiler::compile(
            std::path::Path::new("/verify.mjs"),
            code,
            &oj_compiler::CompileOptions::prod(),
        )
        .is_ok()
    }

    #[test]
    fn an_inlined_svg_with_crlf_endings_emits_parseable_javascript() {
        // git hands a Windows checkout CRLF, and a raw carriage return cannot
        // appear in a JavaScript string literal.
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\">\r\n  <rect/>\r\n</svg>\r\n";
        let url = svg_data_url(svg);
        assert!(!url.contains('\r'), "carriage return survived: {url}");
        assert!(!url.contains('\n'), "line feed survived: {url}");
        assert!(url.starts_with("data:image/svg+xml,"), "{url}");
        assert!(url.contains("%3crect/%3e"), "content lost: {url}");
        assert!(parses(&export_default_url(&url)), "{url}");
    }

    #[test]
    fn svg_data_urls_encode_everything_that_would_break_the_url_or_the_literal() {
        let cases = [
            ("<svg/>", "%3csvg/%3e"),
            ("<svg a=\"b\"/>", "%3csvg%20a='b'/%3e"),
            ("<svg>100%</svg>", "%3csvg%3e100%25%3c/svg%3e"),
            ("<svg fill=\"#fff\"/>", "%3csvg%20fill='%23fff'/%3e"),
            ("<svg>a\\b</svg>", "%3csvg%3ea%5Cb%3c/svg%3e"),
            // Vite: whitespace between tags is dropped, other runs become one %20.
            (
                "  <svg>\n  <rect   x=\"1\"/>\n</svg>  ",
                "%3csvg%3e%3crect%20x='1'/%3e%3c/svg%3e",
            ),
        ];
        for (svg, expected_tail) in cases {
            let url = svg_data_url(svg);
            assert_eq!(url, format!("data:image/svg+xml,{expected_tail}"));
            assert!(parses(&export_default_url(&url)), "{url}");
        }
        // Line and paragraph separators are string-literal terminators too.
        for terminator in ["\u{2028}", "\u{2029}", "\r", "\n", "\r\n"] {
            let url = svg_data_url(&format!("<svg>{terminator}</svg>"));
            assert_eq!(url, "data:image/svg+xml,%3csvg%3e%3c/svg%3e");
            assert!(parses(&export_default_url(&url)));
        }
        // Text, foreignObject and nested quotes cannot be rewritten safely: base64 (Vite).
        for svg in [
            "<svg><text>hi</text></svg>",
            "<svg><foreignObject/></svg>",
            "<svg title=\"it's\"/>",
            "<svg title='say \"hi\"'/>",
        ] {
            let url = svg_data_url(svg);
            assert!(
                url.starts_with("data:image/svg+xml;base64,"),
                "{svg} -> {url}"
            );
            assert_eq!(
                url,
                format!("data:image/svg+xml;base64,{}", b64(svg.as_bytes()))
            );
        }
        assert!(
            !has_nested_quotes("<svg a=\"b\" c='d'/>"),
            "separate quote styles are fine"
        );
    }

    #[test]
    fn a_default_export_of_a_url_is_always_a_valid_module() {
        for url in [
            "data:image/png;base64,AAAA",
            "/assets/a-0123.png",
            "",
            "a\"b",
            "a\\b",
            "a\nb",
            "a\rb",
            "a\u{2028}b",
            "a\0b",
            "café/🚀.png",
        ] {
            let module = export_default_url(url);
            assert!(parses(&module), "{url:?} -> {module}");
        }
    }

    #[test]
    fn every_asset_extension_has_a_real_mime_type() {
        // `is_build_asset` decides what gets inlined; `asset_mime` decides what
        // the browser is told it is. A gap between them inlines a file the page
        // then refuses to render.
        for ext in [
            "png", "jpg", "jpeg", "gif", "webp", "avif", "ico", "bmp", "svg", "woff", "woff2",
            "ttf", "otf", "eot", "mp4", "webm", "mov", "mp3", "wav", "ogg",
        ] {
            assert!(
                is_build_asset(&format!("/src/a.{ext}")),
                "{ext} not an asset"
            );
            assert_ne!(
                asset_mime(ext),
                "application/octet-stream",
                "{ext} has no mime type"
            );
        }
        // A query does not change the classification.
        assert!(is_build_asset("/src/a.png?url"));
        // Same list as the dev server, case-insensitive like Vite.
        assert!(is_build_asset("/src/photo.JPG"));
        assert!(is_build_asset("/src/doc.pdf"));
        assert!(is_build_asset("/src/site.webmanifest"));
        assert_eq!(asset_mime("PNG"), "image/png");
        assert!(!is_build_asset("/src/a.tsx"));
        assert!(!is_build_asset("/src/a.png.tsx"));
        assert!(!is_build_asset(""));
        // An unknown extension falls back rather than guessing.
        assert_eq!(asset_mime("xyz"), "application/octet-stream");
    }

    #[test]
    fn base64_matches_the_reference_vectors() {
        // RFC 4648 section 10, plus the padding boundaries.
        assert_eq!(b64(b""), "");
        assert_eq!(b64(b"f"), "Zg==");
        assert_eq!(b64(b"fo"), "Zm8=");
        assert_eq!(b64(b"foo"), "Zm9v");
        assert_eq!(b64(b"foob"), "Zm9vYg==");
        assert_eq!(b64(b"fooba"), "Zm9vYmE=");
        assert_eq!(b64(b"foobar"), "Zm9vYmFy");
        // High bytes must not sign-extend.
        assert_eq!(b64(&[0xff, 0xff, 0xff]), "////");
        assert_eq!(b64(&[0x00, 0x00, 0x00]), "AAAA");
        assert_eq!(b64(&[0xfb, 0xff, 0xbf]), "+/+/");
        // Length is always a multiple of four.
        for len in 0..64 {
            let bytes: Vec<u8> = (0..len).map(|i| i as u8).collect();
            assert_eq!(b64(&bytes).len() % 4, 0, "len {len}");
        }
    }

    #[test]
    fn asset_queries_split_in_any_combination() {
        assert_eq!(
            split_asset_query("./w.ts?worker"),
            Some(("./w.ts".into(), "worker".into()))
        );
        assert_eq!(
            split_asset_query("./w.ts?worker&inline"),
            Some(("./w.ts".into(), "worker&inline".into()))
        );
        assert_eq!(
            split_asset_query("./w.ts?inline&worker"),
            Some(("./w.ts".into(), "worker&inline".into()))
        );
        assert_eq!(
            split_asset_query("./w.ts?url&sharedworker"),
            Some(("./w.ts".into(), "sharedworker&url".into()))
        );
        assert_eq!(
            split_asset_query("./a.png?url"),
            Some(("./a.png".into(), "url".into()))
        );
        assert_eq!(
            split_asset_query("./a.png?url&v=1"),
            None,
            "foreign params are not oj queries"
        );
        assert_eq!(split_asset_query("./a.png?v=1"), None);
        assert_eq!(split_asset_query("./a.png"), None);
        assert_eq!(
            worker_id_parts("/x/w.ts?worker&inline"),
            Some(("/x/w.ts", "Worker", true, false))
        );
        assert_eq!(
            worker_id_parts("/x/w.ts?sharedworker&url"),
            Some(("/x/w.ts", "SharedWorker", false, true))
        );
        assert_eq!(worker_id_parts("/x/w.ts?url"), None);
    }

    #[test]
    fn asset_file_names_render_vite_placeholders() {
        assert_eq!(
            render_asset_name(None, "bg", "0123456789abcdef", "png"),
            "assets/bg-01234567.png"
        );
        assert_eq!(
            render_asset_name(
                Some("static/[name].[hash][extname]"),
                "bg",
                "0123456789abcdef",
                "png"
            ),
            "static/bg.01234567.png"
        );
        assert_eq!(
            render_asset_name(
                Some("[ext]/[name]-[hash].[ext]"),
                "app",
                "abcdef0123456789",
                "css"
            ),
            "css/app-abcdef01.css"
        );
        assert_eq!(render_asset_name(None, "x", "abcd", ""), "assets/x-abcd");
    }

    #[test]
    fn content_hashes_are_stable_and_content_addressed() {
        // Pinned: filenames derived from this end up in caching headers, so the
        // value must not drift with the toolchain that built oj.
        assert_eq!(content_hash(b""), "af1349b9f5f9a1a6");
        assert_eq!(content_hash(b"oj"), "b74f11b8dbbdd6b4");
        assert_eq!(content_hash(b"oj").len(), 16);
        assert!(content_hash(b"oj").chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(content_hash(b"a"), content_hash(b"b"));
        // A one-bit difference is a different name.
        assert_ne!(content_hash(&[0x00]), content_hash(&[0x01]));
    }

    #[test]
    fn transitive_imports_terminates_on_a_cycle() {
        let mut map = std::collections::HashMap::new();
        map.insert("a".to_string(), vec!["b".to_string()]);
        map.insert("b".to_string(), vec!["c".to_string(), "a".to_string()]);
        map.insert("c".to_string(), vec!["b".to_string()]);
        let reachable = transitive_imports("a", &map);
        assert_eq!(
            reachable.into_iter().collect::<Vec<_>>(),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
        // An unknown entry reaches nothing.
        assert!(transitive_imports("missing", &map).is_empty());
        // A self-import terminates.
        let mut selfish = std::collections::HashMap::new();
        selfish.insert("a".to_string(), vec!["a".to_string()]);
        assert_eq!(
            transitive_imports("a", &selfish)
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["a".to_string()]
        );
    }

    #[test]
    fn asset_names_are_sanitized_without_collapsing_to_nothing() {
        assert_eq!(sanitize_asset_name("logo.png"), "logo.png");
        assert_eq!(sanitize_asset_name("my logo.png"), "my_logo.png");
        assert_eq!(sanitize_asset_name("a/b.png"), "a_b.png");
        assert_eq!(sanitize_asset_name("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_asset_name("a\0b.png"), "a_b.png");
        assert!(!sanitize_asset_name("café.png").contains('é'));
        assert!(sanitize_asset_name("").is_empty());
    }

    #[test]
    fn human_bytes_scales_by_threshold() {
        assert_eq!(human_bytes(0), "0B");
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1023), "1023B");
        assert_eq!(human_bytes(1024), "1.0kB");
        assert_eq!(human_bytes(1536), "1.5kB");
        assert_eq!(human_bytes(1_048_575), "1024.0kB");
        assert_eq!(human_bytes(1_048_576), "1.0MB");
        assert_eq!(human_bytes(3_145_728), "3.0MB");
    }

    #[test]
    fn rewrite_link_hrefs_keeps_the_pages_quoting() {
        let html = "<link rel='stylesheet' href='/site.css'><link rel=stylesheet href=/site.css><link rel=\"stylesheet\" href = \"/site.css\"><link href=\"/other.css\">";
        let out = rewrite_link_hrefs(html, "/site.css", "/assets/site-abc.css");
        assert_eq!(
            out,
            "<link rel='stylesheet' href='/assets/site-abc.css'><link rel=stylesheet href=/assets/site-abc.css><link rel=\"stylesheet\" href = \"/assets/site-abc.css\"><link href=\"/other.css\">"
        );
        assert_eq!(
            rewrite_link_hrefs("<p>no links</p>", "/a", "/b"),
            "<p>no links</p>"
        );
    }

    #[test]
    fn stylesheet_hrefs_take_local_stylesheet_links_only() {
        let html = r#"<html><head>
          <link rel="stylesheet" href="/src/base.css">
          <link rel="stylesheet" href="./src/theme.scss">
          <link rel="stylesheet" href="https://cdn.test/x.css">
          <link rel="icon" href="favicon.ico">
          <link rel="modulepreload" href="/assets/chunk.js">
        </head></html>"#;
        assert_eq!(
            stylesheet_hrefs(html),
            vec!["/src/base.css".to_string(), "./src/theme.scss".to_string()]
        );
    }

    #[test]
    fn inline_module_scripts_become_html_proxy_entries() {
        let html = "<html><body>\n<script type=\"module\">import { x } from \"./src/x.js\"; x();</script>\n<script type=\"module\" src=\"/src/main.js\"></script>\n<script>var legacy = 1;</script>\n<script type=\"module\">console.log(2)</script></body></html>";
        let blocks = inline_module_scripts(html);
        assert_eq!(blocks.len(), 2);
        assert!(blocks[0].2.contains("import { x }"));
        let (out, entries) = externalize_inline_scripts(html, Path::new("/app/index.html"));
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "/@oj-inline/0.js");
        assert_eq!(entries[0].1, "/app/index.html?html-proxy&index=0.js");
        assert_eq!(entries[1].1, "/app/index.html?html-proxy&index=1.js");
        assert!(
            out.contains("<script type=\"module\" src=\"/@oj-inline/0.js\"></script>"),
            "{out}"
        );
        assert!(
            out.contains("<script type=\"module\" src=\"/@oj-inline/1.js\"></script>"),
            "{out}"
        );
        assert!(
            !out.contains("console.log(2)"),
            "inline body removed: {out}"
        );
        assert!(
            out.contains("var legacy = 1;"),
            "classic scripts untouched: {out}"
        );
        assert!(
            out.contains("src=\"/src/main.js\""),
            "external module kept: {out}"
        );
        // The placeholder is picked up as a module entry src.
        assert!(module_script_srcs(&out).contains(&"/@oj-inline/0.js".to_string()));
    }

    #[test]
    fn inline_module_scripts_match_any_attribute_quoting() {
        let html = "<script type='module'>import a from './a.ts'</script>\n\
                    <script type=module>import b from './b.ts'</script>\n\
                    <script type = \"MODULE\" data-src='/x.js'>import c from './c.ts'</script>\n\
                    <script type='module' src='/src/main.ts'></script>\n\
                    <script type='text/javascript'>var d = 1;</script>";
        let blocks = inline_module_scripts(html);
        assert_eq!(blocks.len(), 3, "{blocks:?}");
        assert!(blocks[0].2.contains("'./a.ts'"));
        assert!(blocks[1].2.contains("'./b.ts'"));
        assert!(blocks[2].2.contains("'./c.ts'"));
        let (out, entries) = externalize_inline_scripts(html, Path::new("/app/index.html"));
        assert_eq!(entries.len(), 3);
        assert!(!out.contains("./a.ts") && !out.contains("./c.ts"), "{out}");
        assert!(
            out.contains("src='/src/main.ts'"),
            "external module kept: {out}"
        );
        assert!(
            out.contains("var d = 1;"),
            "classic scripts untouched: {out}"
        );
    }
}
