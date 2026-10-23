//! `@stylexjs/unplugin`'s Vite plugin over the native compiler: its option
//! split, its transform gate, its per-module rule store and stylesheet, and its
//! dev endpoints. oj claims the plugin by name and runs this in its place.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use serde_json::{Map, Value, json};

use crate::api::{FileContext, MapMode, transform_program, transform_source_in_with_map};
use crate::assemble::{AssembleConfig, RuleRegistry};
use crate::errors::ErrorCode;
use crate::module_resolution::MemoFs;
use crate::options::{CompilerOptions, ResolvedOptions};
use crate::rules::StylexRule;

pub const PLUGIN_NAME: &str = "@stylexjs/unplugin";
/// The dev stylesheet, served whole on every request.
pub const DEV_CSS_PATH: &str = "/virtual:stylex.css";
/// The `devMode: 'full'` runtime as a plain script.
pub const DEV_RUNTIME_PATH: &str = "/virtual:stylex.js";
pub const RUNTIME_ID: &str = "virtual:stylex:runtime";
pub const CSS_ONLY_ID: &str = "virtual:stylex:css-only";
/// The HMR event the runtimes refetch the stylesheet on.
pub const CSS_UPDATE_EVENT: &str = "stylex:css-update";

pub const RUNTIME_JS: &str = include_str!("assets/runtime.js");
pub const CSS_ONLY_JS: &str = include_str!("assets/css-only.js");
pub const DEV_RUNTIME_JS: &str = include_str!("assets/dev-runtime.js");

/// Options the plugin consumes itself; the rest go to the compiler.
const PLUGIN_KEYS: &[&str] = &[
    "dev",
    "unstable_moduleResolution",
    "babelConfig",
    "importSources",
    "useCSSLayers",
    "lightningcssOptions",
    "cssInjectionTarget",
    "externalPackages",
    "devPersistToDisk",
    "devMode",
    "treeshakeCompensation",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DevMode {
    /// Inject the stylesheet link and runtime into served HTML.
    Full,
    /// Serve the stylesheet; the app imports `virtual:stylex:css-only`.
    CssOnly,
    /// No dev endpoints, and no transform under `serve`.
    Off,
    /// Any other value: the plugin still compiles and serves its stylesheet,
    /// but injects nothing and serves neither runtime.
    Other,
}

pub struct StylexPlugin {
    options: ResolvedOptions,
    assemble: AssembleConfig,
    dev_mode: DevMode,
    lightningcss_options: Value,
    import_sources: Vec<String>,
    root: PathBuf,
    fs: MemoFs,
    registry: Mutex<RuleRegistry>,
    fingerprint: String,
    on_change: Mutex<Option<ChangeListener>>,
    warnings: Vec<String>,
}

/// Called after a recorded module changed the stylesheet.
pub type ChangeListener = std::sync::Arc<dyn Fn() + Send + Sync>;

impl std::fmt::Debug for StylexPlugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StylexPlugin")
            .field("dev_mode", &self.dev_mode)
            .field("fingerprint", &self.fingerprint)
            .finish_non_exhaustive()
    }
}

/// One string-level transform: the compiled code and its map, or `None` when
/// the module has no StyleX to compile.
pub struct SourceOutput {
    pub code: String,
    pub map: Option<String>,
    pub rules: Vec<StylexRule>,
}

impl StylexPlugin {
    /// `user`: the object given to `stylex.vite(...)`. `node_env_dev`: whether
    /// `NODE_ENV` was `development` when the config loaded, the plugin's `dev`
    /// default. `live_fs`: re-stat package manifests (dev) instead of freezing
    /// them for the process (build).
    pub fn new(
        user: &Value,
        root: &Path,
        node_env_dev: bool,
        live_fs: bool,
    ) -> Result<Self, String> {
        let empty = Map::new();
        let user = match user {
            Value::Object(map) => map,
            Value::Null => &empty,
            other => {
                return Err(format!(
                    "{PLUGIN_NAME} options must be an object, got {other}"
                ));
            }
        };
        let dev_mode = match user.get("devMode").and_then(Value::as_str) {
            None | Some("full") => DevMode::Full,
            Some("css-only") => DevMode::CssOnly,
            Some("off") => DevMode::Off,
            Some(_) => DevMode::Other,
        };
        let import_sources = user
            .get("importSources")
            .cloned()
            .unwrap_or_else(|| json!(["stylex", "@stylexjs/stylex"]));
        let mut compiler: Map<String, Value> = user
            .iter()
            .filter(|(k, _)| !PLUGIN_KEYS.contains(&k.as_str()))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        compiler.insert("importSources".into(), import_sources.clone());
        compiler.insert(
            "treeshakeCompensation".into(),
            user.get("treeshakeCompensation")
                .cloned()
                .unwrap_or(Value::Bool(true)),
        );
        compiler.insert(
            "dev".into(),
            user.get("dev")
                .cloned()
                .unwrap_or(Value::Bool(node_env_dev)),
        );
        compiler.insert(
            "unstable_moduleResolution".into(),
            user.get("unstable_moduleResolution")
                .cloned()
                .unwrap_or_else(
                    || json!({ "type": "commonJS", "rootDir": root.to_string_lossy() }),
                ),
        );
        // The babel plugin's setOptions reads the keys it knows, so an unknown
        // one is ignored, and a known one of the wrong type is logged and
        // left at its default (logAndDefault).
        let mut warnings = Vec::new();
        compiler.retain(|key, value| {
            let one = Value::Object(Map::from_iter([(key.clone(), value.clone())]));
            match CompilerOptions::from_json(&one).and_then(|o| o.resolve()) {
                Ok(_) => true,
                Err(e) if e.code == ErrorCode::UnknownOption => false,
                Err(e) => {
                    warnings.push(format!("[@stylexjs/babel-plugin] {}", e.message));
                    false
                }
            }
        });
        let compiler = Value::Object(compiler);
        let options = CompilerOptions::from_json(&compiler)
            .and_then(|o| o.resolve())
            .map_err(|e| format!("{PLUGIN_NAME}: {e}"))?;
        let mut sheet = Map::new();
        sheet.insert(
            "useLayers".into(),
            user.get("useCSSLayers")
                .cloned()
                .unwrap_or(Value::Bool(false)),
        );
        if let Some(v) = user.get("enableLTRRTLComments") {
            sheet.insert("enableLTRRTLComments".into(), v.clone());
        }
        let assemble = AssembleConfig::from_json(&Value::Object(sheet))
            .map_err(|e| format!("{PLUGIN_NAME}: {e}"))?;
        let fingerprint = blake3::hash(
            format!(
                "{}:{}:{compiler}",
                crate::cache::compiler_fingerprint(),
                options.cache_repr
            )
            .as_bytes(),
        )
        .to_hex()[..16]
            .to_string();
        Ok(Self {
            lightningcss_options: user
                .get("lightningcssOptions")
                .cloned()
                .unwrap_or(Value::Null),
            import_sources: source_names(&import_sources),
            options,
            assemble,
            dev_mode,
            root: root.to_path_buf(),
            fs: if live_fs {
                MemoFs::live()
            } else {
                MemoFs::snapshot()
            },
            registry: Mutex::new(RuleRegistry::new()),
            fingerprint,
            on_change: Mutex::new(None),
            warnings,
        })
    }

    pub fn dev_mode(&self) -> DevMode {
        self.dev_mode
    }

    /// What the option checks logged: values of the wrong type, left at
    /// their defaults.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// `lightningcssOptions`, spread into the stylesheet's lightningcss pass.
    pub fn lightningcss_options(&self) -> &Value {
        &self.lightningcss_options
    }

    /// The plugin's `load` for its virtual runtimes in the current dev mode
    /// (matched as it matches them, by `includes`).
    pub fn virtual_module(&self, id: &str) -> Option<&'static str> {
        match self.dev_mode {
            DevMode::Full if id.contains(RUNTIME_ID) => Some(RUNTIME_JS),
            DevMode::CssOnly if id.contains(CSS_ONLY_ID) => Some(CSS_ONLY_JS),
            _ => None,
        }
    }

    /// Changes whenever compiled output could: the compiler and its options.
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The plugin's `transformInclude` and `shouldHandle`: a JS-like id whose
    /// code imports one of the import sources.
    pub fn handles(&self, id: &str, code: &str) -> bool {
        is_js_like(id) && self.import_sources.iter().any(|s| imports_source(code, s))
    }

    /// Compiles `program` (the parse of `source`) in place; the module's rules.
    pub fn transform_program<'a>(
        &self,
        allocator: &'a Allocator,
        program: &mut Program<'a>,
        file: &Path,
        source: &str,
    ) -> Result<Vec<StylexRule>, String> {
        let ctx = self.context(file, source);
        transform_program(allocator, program, &ctx, &self.options, &self.fs)
            .map(|out| out.rules)
            .map_err(|e| format!("{PLUGIN_NAME}: {e}"))
    }

    /// The string-level transform bundlers run.
    pub fn transform_source(
        &self,
        file: &Path,
        source: &str,
    ) -> Result<Option<SourceOutput>, String> {
        let ctx = self.context(file, source);
        let allocator = Allocator::default();
        let out = transform_source_in_with_map(
            &allocator,
            &ctx,
            &self.options,
            &self.fs,
            MapMode::WithContent,
        )
        .map_err(|e| format!("{PLUGIN_NAME}: {e}"))?;
        Ok(out.filter(|o| o.modified).map(|o| SourceOutput {
            code: o.code,
            map: o.map,
            rules: o.rules,
        }))
    }

    /// Records a module's rules (none removes it); whether the sheet changed.
    pub fn set_module_rules(&self, id: &str, rules: Vec<StylexRule>) -> bool {
        // With runtime injection the modules insert their own styles, and the
        // plugin keeps no rules (so serves and emits no sheet).
        if self.options.runtime_injection.is_some() {
            return false;
        }
        let changed = {
            let mut registry = self.registry.lock().unwrap();
            let before = registry.generation();
            if rules.is_empty() {
                registry.remove_file(id);
            } else {
                registry.set_file_rules(id, rules);
            }
            registry.generation() != before
        };
        if changed {
            let listener = self.on_change.lock().unwrap().clone();
            if let Some(listener) = listener {
                listener();
            }
        }
        changed
    }

    /// Every recorded module's rules, by module id: what a cache that skips
    /// the transform must restore with [`Self::set_module_rules`].
    pub fn module_rules(&self) -> Vec<(String, Vec<StylexRule>)> {
        let registry = self.registry.lock().unwrap();
        registry
            .files()
            .map(|(id, rules)| (id.to_string(), rules.to_vec()))
            .collect()
    }

    /// Replaces the listener told about every stylesheet change.
    pub fn set_change_listener(&self, listener: ChangeListener) {
        *self.on_change.lock().unwrap() = Some(listener);
    }

    /// Bumped by every change [`Self::set_module_rules`] makes to the sheet.
    pub fn generation(&self) -> u64 {
        self.registry.lock().unwrap().generation()
    }

    /// `processStylexRules` over every recorded module, or `None` with none.
    pub fn stylesheet(&self) -> Result<Option<String>, String> {
        let mut registry = self.registry.lock().unwrap();
        if registry.file_count() == 0 {
            return Ok(None);
        }
        registry
            .emit(&self.assemble)
            .map(Some)
            .map_err(|e| format!("{PLUGIN_NAME}: {e}"))
    }

    fn context<'s>(&'s self, file: &'s Path, source: &'s str) -> FileContext<'s> {
        FileContext {
            filename: file,
            source_text: source,
            cwd: &self.root,
        }
    }
}

/// The plugin's `config` hook list: the app's dependencies whose manifests
/// depend on an import source's package (or `@stylexjs/stylex`), plus
/// `externalPackages`. Vite keeps them out of dependency pre-bundling, so the
/// transform reaches their modules. Found from the package.json nearest
/// `root`, each dependency where Node's `require.resolve("<dep>/package.json")`
/// would find it.
pub fn dependency_packages(user: &Value, root: &Path) -> Vec<String> {
    let mut found: Vec<String> = user
        .get("externalPackages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(str::to_string))
        .collect();
    let import_sources = user
        .get("importSources")
        .cloned()
        .unwrap_or_else(|| json!(["stylex", "@stylexjs/stylex"]));
    let mut targets: Vec<String> = source_names(&import_sources)
        .iter()
        .filter_map(|s| package_of(s))
        .collect();
    targets.push("@stylexjs/stylex".to_string());
    let Some(app) = root.ancestors().find(|d| d.join("package.json").is_file()) else {
        return found;
    };
    let Some(manifest) = read_json(&app.join("package.json")) else {
        return found;
    };
    let mut deps: Vec<&str> = Vec::new();
    for field in [
        "dependencies",
        "devDependencies",
        "peerDependencies",
        "optionalDependencies",
    ] {
        for name in manifest
            .get(field)
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|m| m.keys())
        {
            if !deps.contains(&name.as_str()) {
                deps.push(name);
            }
        }
    }
    for dep in deps {
        let Some(dep_dir) = app
            .ancestors()
            .map(|d| d.join("node_modules").join(dep))
            .find(|d| d.join("package.json").is_file())
        else {
            continue;
        };
        let Some(dep_manifest) = read_json(&dep_dir.join("package.json")) else {
            continue;
        };
        if !exports_package_json(dep_manifest.get("exports"), &dep_dir) {
            continue;
        }
        let depends = ["dependencies", "peerDependencies", "optionalDependencies"]
            .iter()
            .any(|field| {
                dep_manifest
                    .get(*field)
                    .and_then(Value::as_object)
                    .is_some_and(|m| m.keys().any(|k| targets.contains(k)))
            });
        if depends && !found.iter().any(|f| f == dep) {
            found.push(dep.to_string());
        }
    }
    found
}

fn read_json(path: &Path) -> Option<Value> {
    serde_json::from_slice(&std::fs::read(path).ok()?).ok()
}

/// The package a module specifier names (`@scope/name/sub` -> `@scope/name`);
/// none for a relative or absolute one.
fn package_of(source: &str) -> Option<String> {
    if source.is_empty() || source.starts_with('.') || source.starts_with('/') {
        return None;
    }
    let mut parts = source.split('/');
    let first = parts.next()?;
    if first.starts_with('@') {
        let name = parts.next().filter(|n| !n.is_empty())?;
        return Some(format!("{first}/{name}"));
    }
    Some(first.to_string())
}

/// Whether Node's `require.resolve("<pkg>/package.json")` gets past the
/// package's `exports`: no map, or a `./package.json` subpath (exact, or by a
/// `./*` / `./*.json` pattern) whose `require` target exists in `dir`.
fn exports_package_json(exports: Option<&Value>, dir: &Path) -> bool {
    let map = match exports {
        None | Some(Value::Null) => return true,
        Some(Value::Object(map)) if map.keys().any(|k| k.starts_with('.')) => map,
        // A string, an array or a conditions object maps "." alone.
        Some(_) => return false,
    };
    let (target, star) = if let Some(t) = map.get("./package.json") {
        (t, None)
    } else if let Some(t) = map.get("./*") {
        (t, Some("package.json"))
    } else if let Some(t) = map.get("./*.json") {
        (t, Some("package"))
    } else {
        return false;
    };
    require_target(target)
        .map(|t| match star {
            Some(sub) => t.replace('*', sub),
            None => t.to_string(),
        })
        .is_some_and(|t| dir.join(t.trim_start_matches("./")).is_file())
}

/// An exports target under `require`'s conditions (`require`, `node`,
/// `default`, in the map's order); `null` blocks.
fn require_target(target: &Value) -> Option<&str> {
    match target {
        Value::String(s) => Some(s),
        Value::Array(items) => items.iter().find_map(require_target),
        Value::Object(conditions) => conditions
            .iter()
            .filter(|(k, _)| matches!(k.as_str(), "require" | "node" | "default"))
            .find_map(|(_, v)| require_target(v)),
        _ => None,
    }
}

/// `importSources` entries as module names (`{ from, as }` keeps `from`).
fn source_names(sources: &Value) -> Vec<String> {
    sources
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| match s {
            Value::String(s) => Some(s.clone()),
            Value::Object(o) => o.get("from").and_then(Value::as_str).map(str::to_string),
            _ => None,
        })
        .collect()
}

/// The plugin's `/\.[cm]?[jt]sx?(\?|$)/`, anywhere in the id: a query can
/// carry the extension (`App.vue?vue&type=script&lang.ts`).
fn is_js_like(id: &str) -> bool {
    let bytes = id.as_bytes();
    bytes.iter().enumerate().any(|(dot, &b)| {
        if b != b'.' {
            return false;
        }
        let mut at = dot + 1;
        if matches!(bytes.get(at), Some(b'c' | b'm')) {
            at += 1;
        }
        if !matches!(bytes.get(at), Some(b'j' | b't')) || bytes.get(at + 1) != Some(&b's') {
            return false;
        }
        at += 2;
        if bytes.get(at) == Some(&b'x') && matches!(bytes.get(at + 1), None | Some(b'?')) {
            return true;
        }
        matches!(bytes.get(at), None | Some(b'?'))
    })
}

/// The plugin's `containsStylexImport`: `from 's'`, `import('s')`,
/// `require('s')`, or a line-leading `import 's'`, either quote.
fn imports_source(code: &str, source: &str) -> bool {
    for quote in ['\'', '"'] {
        let needle = format!("{quote}{source}{quote}");
        let mut at = 0;
        while let Some(found) = code[at..].find(&needle) {
            let start = at + found;
            at = start + needle.len();
            let before = code[..start].trim_end_matches([' ', '\t', '\n', '\r']);
            if before.ends_with("from") {
                return true;
            }
            if let Some(call) = before.strip_suffix('(') {
                let call = call.trim_end_matches([' ', '\t', '\n', '\r']);
                if call.ends_with("import") || call.ends_with("require") {
                    return true;
                }
            }
            if let Some(stmt) = before.strip_suffix("import") {
                let line = stmt.rsplit('\n').next().unwrap_or(stmt);
                if line.trim().is_empty() {
                    return true;
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "import * as stylex from '@stylexjs/stylex';\n\
                       export const styles = stylex.create({ root: { color: 'red' } });\n";

    fn plugin(user: Value) -> StylexPlugin {
        StylexPlugin::new(&user, Path::new("/app"), false, false).unwrap()
    }

    #[test]
    fn gate_is_the_plugins_include_and_import_scan() {
        let p = plugin(json!({}));
        assert!(p.handles("/app/src/a.tsx", SRC));
        assert!(p.handles("/app/src/a.mts?v=1", SRC));
        assert!(!p.handles("/app/src/a.css", SRC));
        assert!(p.handles("/app/src/App.vue?vue&type=script&setup=true&lang.ts", SRC));
        assert!(!p.handles("/app/src/a.json", SRC));
        assert!(!p.handles("/app/src/a.tsv", SRC));
        assert!(p.handles("/a.js", "const s = await import ( \"stylex\" );"));
        assert!(p.handles("/a.js", "const s = require('stylex');"));
        assert!(p.handles("/a.js", "  import '@stylexjs/stylex';"));
        assert!(!p.handles("/a.js", "const s = '@stylexjs/stylex';"));
        assert!(!p.handles("/a.js", "<div sx={x} />"));
        let custom = plugin(json!({ "importSources": [{ "from": "my-sx", "as": "css" }] }));
        assert!(custom.handles("/a.ts", "import { css } from 'my-sx';"));
        assert!(!custom.handles("/a.ts", SRC));
    }

    #[test]
    fn options_split_between_plugin_and_compiler() {
        // Plugin-only keys never reach the compiler (which rejects unknown keys).
        let p = plugin(json!({
            "devMode": "css-only",
            "useCSSLayers": true,
            "externalPackages": ["x"],
            "lightningcssOptions": {},
            "classNamePrefix": "q",
        }));
        assert_eq!(p.dev_mode(), DevMode::CssOnly);
        let out = p
            .transform_source(Path::new("/app/src/a.ts"), SRC)
            .unwrap()
            .unwrap();
        assert!(out.code.contains("\"q"), "{}", out.code);
        assert!(p.set_module_rules("/app/src/a.ts", out.rules));
        assert!(
            p.stylesheet()
                .unwrap()
                .unwrap()
                .contains("@layer priority1{")
        );
        // The plugin accepts any devMode string; an unknown one injects nothing.
        let other =
            StylexPlugin::new(&json!({ "devMode": "x" }), Path::new("/app"), false, false).unwrap();
        assert_eq!(other.dev_mode(), DevMode::Other);
        assert_eq!(other.virtual_module(RUNTIME_ID), None);
    }

    #[test]
    fn unknown_options_are_ignored_and_bad_values_defaulted() {
        let p = plugin(json!({
            "useRemForFontSize": true,
            "genConditionalClasses": true,
            "dev": "true",
            "classNamePrefix": "q",
        }));
        assert_eq!(p.warnings().len(), 1, "{:?}", p.warnings());
        assert!(
            p.warnings()[0].contains("options.dev"),
            "{:?}",
            p.warnings()
        );
        let out = p
            .transform_source(Path::new("/app/src/a.ts"), SRC)
            .unwrap()
            .unwrap();
        assert!(
            out.code.contains("\"q"),
            "the valid option still applies: {}",
            out.code
        );
        assert!(
            !out.code.contains("a__styles"),
            "dev defaulted to false: {}",
            out.code
        );
    }

    #[test]
    fn runtime_injection_collects_no_rules() {
        let p = plugin(json!({ "runtimeInjection": true }));
        let out = p
            .transform_source(Path::new("/app/src/a.ts"), SRC)
            .unwrap()
            .unwrap();
        assert!(!p.set_module_rules("/app/src/a.ts", out.rules));
        assert_eq!(p.stylesheet().unwrap(), None);
    }

    #[test]
    fn dev_follows_node_env_unless_set() {
        let dev = StylexPlugin::new(&json!({}), Path::new("/app"), true, false).unwrap();
        let out = dev
            .transform_source(Path::new("/app/src/a.ts"), SRC)
            .unwrap()
            .unwrap();
        assert!(out.code.contains("a__styles.root"), "{}", out.code);
        let prod =
            StylexPlugin::new(&json!({ "dev": false }), Path::new("/app"), true, false).unwrap();
        let out = prod
            .transform_source(Path::new("/app/src/a.ts"), SRC)
            .unwrap()
            .unwrap();
        assert!(!out.code.contains("a__styles.root"), "{}", out.code);
    }

    #[test]
    fn dependency_packages_are_the_apps_stylex_dependents() {
        let dir = std::env::temp_dir().join(format!("oj-stylex-deps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let write = |rel: &str, body: &str| {
            let path = dir.join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, body).unwrap();
        };
        write(
            "package.json",
            r#"{"dependencies":{"ui":"1","plain":"1","sealed":"1","peer-ui":"1","starred":"1","open":"1"},"devDependencies":{"custom":"1"}}"#,
        );
        write(
            "node_modules/ui/package.json",
            r#"{"dependencies":{"@stylexjs/stylex":"0.19.0"}}"#,
        );
        write(
            "node_modules/plain/package.json",
            r#"{"dependencies":{"react":"19"}}"#,
        );
        // An exports map hiding package.json: Node's resolve fails, so the
        // plugin never sees this one.
        write(
            "node_modules/sealed/package.json",
            r#"{"exports":{".":"./i.js"},"dependencies":{"@stylexjs/stylex":"0.19.0"}}"#,
        );
        write(
            "node_modules/peer-ui/package.json",
            r#"{"peerDependencies":{"@stylexjs/stylex":"*"}}"#,
        );
        // `./*` maps package.json to dist/package.json, which is not there:
        // Node's resolve fails. `./*` onto the root does expose it.
        write(
            "node_modules/starred/package.json",
            r#"{"exports":{".":"./i.js","./*":"./dist/*"},"dependencies":{"@stylexjs/stylex":"0.19.0"}}"#,
        );
        write(
            "node_modules/open/package.json",
            r#"{"exports":{".":"./i.js","./*":{"require":"./*"}},"dependencies":{"@stylexjs/stylex":"0.19.0"}}"#,
        );
        write(
            "node_modules/custom/package.json",
            r#"{"dependencies":{"my-sx":"1"}}"#,
        );
        let app = dir.join("src");
        std::fs::create_dir_all(&app).unwrap();

        assert_eq!(
            dependency_packages(&json!({}), &app),
            ["ui", "peer-ui", "open"]
        );
        assert_eq!(
            dependency_packages(
                &json!({ "importSources": [{ "from": "my-sx/sub", "as": "css" }], "externalPackages": ["extra"] }),
                &dir
            ),
            ["extra", "ui", "peer-ui", "open", "custom"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn registry_tracks_modules_and_reports_changes() {
        let p = plugin(json!({}));
        assert_eq!(p.stylesheet().unwrap(), None);
        let rules = p
            .transform_source(Path::new("/app/src/a.ts"), SRC)
            .unwrap()
            .unwrap()
            .rules;
        assert!(p.set_module_rules("/app/src/a.ts", rules.clone()));
        assert!(
            !p.set_module_rules("/app/src/a.ts", rules),
            "same rules, no change"
        );
        assert_eq!(p.stylesheet().unwrap().unwrap(), ".x1e2nbdu{color:red}");
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = std::sync::Arc::clone(&calls);
        p.set_change_listener(std::sync::Arc::new(move || {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        assert!(p.set_module_rules("/app/src/a.ts", Vec::new()));
        assert!(!p.set_module_rules("/app/src/a.ts", Vec::new()));
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "only a change notifies"
        );
        assert_eq!(p.stylesheet().unwrap(), None);
        // A module with no StyleX compiles to nothing.
        assert!(
            p.transform_source(Path::new("/app/src/b.ts"), "export const x = 1;\n")
                .unwrap()
                .is_none()
        );
    }
}
