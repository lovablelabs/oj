// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Reads a literal vite.config in Rust, so a dev boot whose config is only
//! oj's native plugins plus plain values starts no JS engine at all.
//!
//! The accepted shape is narrow on purpose: imports of `vite`'s
//! `defineConfig`, `@vitejs/plugin-react`, `@stylexjs/unplugin` and `path`,
//! then `export default` of an object (or `defineConfig({...})`) holding
//! `plugins: [react(), stylex.vite({...})]`, `server.port`, `server.host`,
//! `resolve.alias` and the inert `logLevel` / `clearScreen`. Anything else
//! (another statement, a function config, a spread, an env read) returns None
//! and the config is evaluated on an engine as before.
//!
//! The output is what `vite-extract.mjs` emits for that config under the
//! exact Vite and plugin-react versions below (`vite8-react-serve.json`), with the
//! root-dependent and config-dependent fields filled in;
//! `crates/oj/tests/static_config_parity.rs` holds the two equal.

use std::path::{Component, Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, Expression, ImportDeclarationSpecifier, ModuleExportName,
    ObjectExpression, ObjectPropertyKind, PropertyKey, PropertyKind, Statement,
};
use oxc_parser::Parser;
use oxc_span::SourceType;
use serde_json::{Map, Value};

const TEMPLATE: &str = include_str!("../assets/vite8-react-serve.json");

/// The installs the template was taken from (and the parity test runs
/// against). Defaults move between minors (8.0's `fs.deny`, its
/// `build.rolldownOptions.platform`), so any other version goes to the engine.
const VITE: &str = "8.3.5";
const PLUGIN_REACT: &str = "6.1.2";
const STYLEX_UNPLUGIN: &str = "0.19.0";

/// The extraction output for `vite` (the app's vite.config), or None when the
/// config is not one this reader accepts or the installed toolchain is not
/// the one its template was taken from.
pub(crate) fn static_extraction(
    root: &Path,
    vite: &Path,
    command: &str,
    mode: &str,
) -> Option<Value> {
    let env = oj_env::get();
    if command != "serve" || mode != "development" {
        return None;
    }
    // A NODE_ENV the config would see changes Vite's resolution.
    if env
        .knobs
        .node_env
        .as_deref()
        .is_some_and(|v| v != "development")
    {
        return None;
    }
    // Vite appends these to `server.allowedHosts` (resolveServerOptions).
    if env
        .vars()
        .any(|(k, _)| k == "__VITE_ADDITIONAL_SERVER_ALLOWED_HOSTS")
    {
        return None;
    }
    if let Some(reason) = toolchain_mismatch(
        root,
        &[("vite", VITE), ("@vitejs/plugin-react", PLUGIN_REACT)],
    ) {
        crate::boot_phase(&format!("vite-extract static declined: {reason}"));
        return None;
    }
    let source = std::fs::read_to_string(vite).ok()?;
    // Vite bundles the config from its real path, so `__dirname` and
    // `import.meta.dirname` are a symlinked config's target directory.
    let Some(config) = read_config(&source, &vite.canonicalize().ok()?) else {
        crate::boot_phase("vite-extract static declined: config is not a literal shape");
        return None;
    };
    if config.stylex.is_some() {
        if let Some(reason) = toolchain_mismatch(root, &[("@stylexjs/unplugin", STYLEX_UNPLUGIN)]) {
            crate::boot_phase(&format!("vite-extract static declined: {reason}"));
            return None;
        }
    }

    let mut out: Value = serde_json::from_str(TEMPLATE).ok()?;
    let fields = out.as_object_mut()?;
    let root_text = root.to_string_lossy().into_owned();
    fields.insert("envDir".into(), Value::String(root_text.clone()));
    fields.insert(
        "publicDir".into(),
        Value::String(root.join("public").to_string_lossy().into_owned()),
    );
    fields.insert("fsAllow".into(), fs_allow(root)?);
    fields.insert("port".into(), Value::from(config.port.unwrap_or(5173)));
    if let Some(host) = &config.host {
        insert_host(fields, host)?;
    }
    fields
        .get_mut("serverFlags")?
        .as_object_mut()?
        .insert("forwardConsole".into(), forward_console(|key| env.var(key)));
    fields.insert("alias".into(), Value::Object(config.alias));
    fields.insert(
        "stylex".into(),
        config
            .stylex
            .map(|options| serde_json::json!({ "options": options }))
            .unwrap_or(Value::Null),
    );
    let _ = super::CONFIG_DEPS.set(vec![vite.to_path_buf()]);
    Some(out)
}

/// Vite's default `server.forwardConsole`: on when it runs under a coding agent.
fn forward_console<'a>(var: impl Fn(&str) -> Option<&'a str>) -> Value {
    if coding_agent(var) {
        serde_json::json!({ "unhandledErrors": true, "logLevels": ["error", "warn"] })
    } else {
        serde_json::json!({ "unhandledErrors": false, "logLevels": [] })
    }
}

/// std-env 4.3's agent detection as bundled in Vite 8. The engine child
/// inherits oj's stdout, so its TTY check sees the same terminal.
fn coding_agent<'a>(var: impl Fn(&str) -> Option<&'a str>) -> bool {
    use std::io::IsTerminal;
    let set = |key: &str| var(key).is_some_and(|v| !v.is_empty());
    let contains = |key: &str, needle: &str| var(key).is_some_and(|v| v.contains(needle));
    set("AI_AGENT")
        || [
            "CLAUDECODE",
            "CLAUDE_CODE",
            "REPL_ID",
            "GEMINI_CLI",
            "CODEX_SANDBOX",
            "CODEX_THREAD_ID",
            "OPENCODE",
            "AUGMENT_AGENT",
            "GOOSE_PROVIDER",
            "JUNIE_DATA",
            "JUNIE_SHIM_PATH",
            "COPILOT_AGENT",
            "COPILOT_CLI",
            "CURSOR_AGENT",
        ]
        .into_iter()
        .any(set)
        || contains("PATH", ".pi/agent")
        || contains("PATH", ".pi\\agent")
        || contains("EDITOR", "devin")
        || (contains("TERM_PROGRAM", "kiro") && !std::io::stdout().is_terminal())
}

#[derive(Default)]
struct LiteralConfig {
    /// `react()` is listed: plugin-react's config hook is part of the template.
    react: bool,
    port: Option<u16>,
    host: Option<String>,
    alias: Map<String, Value>,
    /// `stylex.vite(options)`'s options, when the plugin is listed.
    stylex: Option<Value>,
}

/// Local names the config's imports bound.
#[derive(Default)]
struct Bindings {
    define_config: Option<String>,
    react: Option<String>,
    stylex: Option<String>,
    path: Option<String>,
}

fn read_config(source: &str, file: &Path) -> Option<LiteralConfig> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(file).ok()?;
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.fatal_error || !parsed.diagnostics.is_empty() {
        return None;
    }
    let dir = file.parent()?;
    let mut bindings = Bindings::default();
    let mut config = None;
    for statement in &parsed.program.body {
        match statement {
            Statement::ImportDeclaration(import) => {
                if import.import_kind.is_type() {
                    continue;
                }
                bind_import(
                    &mut bindings,
                    import.source.value.as_str()?,
                    import.specifiers.as_deref()?,
                )?;
            }
            Statement::ExportDefaultDeclaration(export) if config.is_none() => {
                let value = export.declaration.as_expression()?;
                config = Some(read_object(
                    config_object(value, &bindings)?,
                    &bindings,
                    dir,
                )?);
            }
            _ => return None,
        }
    }
    config.filter(|config| config.react)
}

fn bind_import(
    bindings: &mut Bindings,
    source: &str,
    specifiers: &[ImportDeclarationSpecifier],
) -> Option<()> {
    let [specifier] = specifiers else {
        return None;
    };
    let slot = match (source, specifier) {
        ("vite", ImportDeclarationSpecifier::ImportSpecifier(s)) => {
            let ModuleExportName::IdentifierName(name) = &s.imported else {
                return None;
            };
            if name.name != "defineConfig" {
                return None;
            }
            &mut bindings.define_config
        }
        ("@vitejs/plugin-react", ImportDeclarationSpecifier::ImportDefaultSpecifier(_)) => {
            &mut bindings.react
        }
        ("@stylexjs/unplugin", ImportDeclarationSpecifier::ImportDefaultSpecifier(_)) => {
            &mut bindings.stylex
        }
        (
            "path" | "node:path",
            ImportDeclarationSpecifier::ImportDefaultSpecifier(_)
            | ImportDeclarationSpecifier::ImportNamespaceSpecifier(_),
        ) => &mut bindings.path,
        _ => return None,
    };
    if slot.is_some() {
        return None;
    }
    *slot = Some(specifier.local().name.to_string());
    Some(())
}

fn is_binding(expr: &Expression, name: &Option<String>) -> bool {
    matches!((expr, name), (Expression::Identifier(id), Some(n)) if id.name == n.as_str())
}

fn config_object<'a, 'b>(
    value: &'b Expression<'a>,
    bindings: &Bindings,
) -> Option<&'b ObjectExpression<'a>> {
    match value {
        Expression::ObjectExpression(obj) => Some(obj),
        Expression::CallExpression(call) if is_binding(&call.callee, &bindings.define_config) => {
            match call.arguments.as_slice() {
                [Argument::ObjectExpression(obj)] => Some(obj),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Each property of a plain object literal, by its static key. A repeated
/// key (JavaScript keeps the last) and `__proto__` (which sets the prototype
/// instead of a property) are declined.
fn properties<'a, 'b>(obj: &'b ObjectExpression<'a>) -> Option<Vec<(String, &'b Expression<'a>)>> {
    let mut seen = std::collections::HashSet::new();
    obj.properties
        .iter()
        .map(|prop| {
            let ObjectPropertyKind::ObjectProperty(p) = prop else {
                return None;
            };
            if p.kind != PropertyKind::Init || p.method || p.computed || p.shorthand {
                return None;
            }
            let key = match &p.key {
                PropertyKey::StaticIdentifier(id) => id.name.to_string(),
                PropertyKey::StringLiteral(s) => s.value.as_str()?.to_string(),
                _ => return None,
            };
            if key == "__proto__" || !seen.insert(key.clone()) {
                return None;
            }
            Some((key, &p.value))
        })
        .collect()
}

fn read_object(obj: &ObjectExpression, bindings: &Bindings, dir: &Path) -> Option<LiteralConfig> {
    let mut config = LiteralConfig::default();
    for (key, value) in properties(obj)? {
        match key.as_str() {
            "plugins" => read_plugins(value, bindings, &mut config)?,
            "server" => {
                let Expression::ObjectExpression(server) = value else {
                    return None;
                };
                for (key, value) in properties(server)? {
                    match (key.as_str(), value) {
                        ("port", Expression::NumericLiteral(n)) => {
                            if n.value.fract() != 0.0 || !(0.0..=65535.0).contains(&n.value) {
                                return None;
                            }
                            config.port = Some(n.value as u16);
                        }
                        ("host", Expression::StringLiteral(h)) => {
                            let host = h.value.as_str()?;
                            if host.is_empty() {
                                return None;
                            }
                            config.host = Some(host.to_string());
                        }
                        _ => return None,
                    }
                }
            }
            "resolve" => {
                let Expression::ObjectExpression(resolve) = value else {
                    return None;
                };
                for (key, value) in properties(resolve)? {
                    let ("alias", Expression::ObjectExpression(alias)) = (key.as_str(), value)
                    else {
                        return None;
                    };
                    for (find, replacement) in properties(alias)? {
                        let (find, target) =
                            normalize_alias(find, path_value(replacement, bindings, dir)?);
                        // Vite's resolved list keeps the first of a key, as
                        // vite-extract.mjs does.
                        config.alias.entry(find).or_insert(Value::String(target));
                    }
                }
            }
            "logLevel" if matches!(value, Expression::StringLiteral(_)) => {}
            "clearScreen" if matches!(value, Expression::BooleanLiteral(_)) => {}
            _ => return None,
        }
    }
    Some(config)
}

fn read_plugins(value: &Expression, bindings: &Bindings, config: &mut LiteralConfig) -> Option<()> {
    let Expression::ArrayExpression(list) = value else {
        return None;
    };
    for element in &list.elements {
        let ArrayExpressionElement::CallExpression(call) = element else {
            return None;
        };
        if is_binding(&call.callee, &bindings.react) && call.arguments.is_empty() && !config.react {
            config.react = true;
            continue;
        }
        let Expression::StaticMemberExpression(member) = &call.callee else {
            return None;
        };
        if !is_binding(&member.object, &bindings.stylex)
            || member.property.name != "vite"
            || config.stylex.is_some()
        {
            return None;
        }
        let options = match call.arguments.as_slice() {
            [] => Value::Object(Map::new()),
            [Argument::ObjectExpression(obj)] => json_object(obj)?,
            _ => return None,
        };
        config.stylex = Some(options);
    }
    config.react.then_some(())
}

/// A string `server.host` as Vite resolves it: `preview.host` inherits it,
/// and getAdditionalAllowedHosts adds it once for the server and once for
/// the preview to `server.allowedHosts` and `preview.allowedHosts`, which by
/// default are one shared array, so each push lands in both.
fn insert_host(fields: &mut Map<String, Value>, host: &str) -> Option<()> {
    let host = Value::String(host.to_string());
    let shared = Value::Array(vec![host.clone(); 4]);
    fields.insert("host".into(), host.clone());
    fields.insert("allowedHosts".into(), shared.clone());
    let preview = fields.get_mut("preview")?.as_object_mut()?;
    preview.insert("host".into(), host);
    preview.insert("allowedHosts".into(), shared);
    Some(())
}

/// A JSON-shaped literal: what `JSON.stringify` of it would carry.
fn json_literal(expr: &Expression) -> Option<Value> {
    Some(match expr {
        Expression::StringLiteral(s) => Value::String(s.value.as_str()?.to_string()),
        Expression::BooleanLiteral(b) => Value::Bool(b.value),
        Expression::NullLiteral(_) => Value::Null,
        Expression::NumericLiteral(n) => number(n.value)?,
        Expression::ArrayExpression(list) => Value::Array(
            list.elements
                .iter()
                .map(|e| json_literal(e.as_expression()?))
                .collect::<Option<_>>()?,
        ),
        Expression::ObjectExpression(obj) => json_object(obj)?,
        _ => return None,
    })
}

/// A number as `JSON.stringify` writes it: whole numbers without a fraction,
/// so they read back as integers.
fn number(value: f64) -> Option<Value> {
    const SAFE: f64 = 9_007_199_254_740_991.0;
    if value.fract() == 0.0 && value.abs() <= SAFE {
        return Some(Value::from(value as i64));
    }
    serde_json::Number::from_f64(value).map(Value::Number)
}

/// Vite's `normalizeSingleAlias`: a trailing `/` is dropped from both sides
/// when both have one.
fn normalize_alias(find: String, replacement: String) -> (String, String) {
    match (find.strip_suffix('/'), replacement.strip_suffix('/')) {
        (Some(f), Some(r)) => (f.to_string(), r.to_string()),
        _ => (find, replacement),
    }
}

fn json_object(obj: &ObjectExpression) -> Option<Value> {
    Some(Value::Object(
        properties(obj)?
            .into_iter()
            .map(|(k, v)| Some((k, json_literal(v)?)))
            .collect::<Option<_>>()?,
    ))
}

/// A string, or `path.resolve(dirname, "...")` against the config's directory.
fn path_value(expr: &Expression, bindings: &Bindings, dir: &Path) -> Option<String> {
    match expr {
        Expression::StringLiteral(s) => s.value.as_str().map(str::to_string),
        Expression::CallExpression(call) => {
            let Expression::StaticMemberExpression(callee) = &call.callee else {
                return None;
            };
            if !is_binding(&callee.object, &bindings.path) || callee.property.name != "resolve" {
                return None;
            }
            let mut out = PathBuf::new();
            for arg in &call.arguments {
                let segment = match arg.as_expression()? {
                    Expression::Identifier(id) if id.name == "__dirname" => dir.to_path_buf(),
                    Expression::StaticMemberExpression(m)
                        if m.property.name == "dirname"
                            && matches!(&m.object, Expression::ImportMeta(_)) =>
                    {
                        dir.to_path_buf()
                    }
                    Expression::StringLiteral(s) => PathBuf::from(s.value.as_str()?),
                    _ => return None,
                };
                out.push(segment);
            }
            // path.resolve falls back to the cwd for a relative result.
            out.is_absolute()
                .then(|| normalize(&out).to_string_lossy().into_owned())
        }
        _ => None,
    }
}

/// Node's `path.normalize` for an absolute path: `.` and `..` folded, no
/// trailing separator, no symlinks read.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Why the installed toolchain is not the one the template was taken from:
/// the first package in `pinned` whose installed version differs, or None.
fn toolchain_mismatch(root: &Path, pinned: &[(&str, &str)]) -> Option<String> {
    pinned.iter().find_map(|&(name, want)| {
        let found = installed_version(root, name);
        (found.as_deref() != Some(want)).then(|| {
            format!(
                "{name} {} installed, the template is for {want}",
                found.as_deref().unwrap_or("not")
            )
        })
    })
}

/// The version in `node_modules/<name>/package.json`, looked up from `root`
/// the way Node resolves a bare package.
fn installed_version(root: &Path, name: &str) -> Option<String> {
    root.ancestors().find_map(|dir| {
        let text =
            std::fs::read_to_string(dir.join("node_modules").join(name).join("package.json"))
                .ok()?;
        let json: Value = serde_json::from_str(&text).ok()?;
        json.get("version")?.as_str().map(str::to_string)
    })
}

/// Vite's default `server.fs.allow` (resolveServerOptions): the workspace
/// root, plus Vite's own client directory when the root does not hold it.
/// None for Yarn PnP and pnpm virtual-store layouts, which add more.
fn fs_allow(root: &Path) -> Option<Value> {
    let workspace = search_for_workspace_root(root);
    let package_root = search_for_package_root(root);
    if package_root.join(".pnp.cjs").exists()
        || package_root.join(".pnp.js").exists()
        || workspace.join("node_modules/.modules.yaml").exists()
    {
        return None;
    }
    let vite = root
        .ancestors()
        .map(|dir| dir.join("node_modules/vite"))
        .find(|dir| dir.join("package.json").is_file())?
        .canonicalize()
        .ok()?;
    let client = vite.join("dist/client");
    let mut allow = vec![workspace.to_string_lossy().into_owned()];
    let client = client.to_string_lossy().into_owned();
    if !allow.iter().any(|dir| is_parent_directory(dir, &client)) {
        allow.push(client);
    }
    Some(Value::Array(allow.into_iter().map(Value::String).collect()))
}

/// Vite's `isParentDirectory`, case-folded where the file system usually is.
fn is_parent_directory(dir: &str, file: &str) -> bool {
    let dir = if dir.ends_with('/') {
        dir.to_string()
    } else {
        format!("{dir}/")
    };
    file.starts_with(&dir)
        || (cfg!(any(target_os = "macos", windows))
            && file.to_lowercase().starts_with(&dir.to_lowercase()))
}

/// Vite's `searchForPackageRoot`: the nearest directory with a package.json.
fn search_for_package_root(current: &Path) -> PathBuf {
    current
        .ancestors()
        .find(|dir| dir.join("package.json").exists())
        .unwrap_or(current)
        .to_path_buf()
}

/// Vite's `searchForWorkspaceRoot` (server/searchRoot.ts).
fn search_for_workspace_root(current: &Path) -> PathBuf {
    let declares = |dir: &Path, file: &str, key: &str| {
        std::fs::read_to_string(dir.join(file))
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .is_some_and(|json| json.get(key).is_some_and(is_truthy))
    };
    current
        .ancestors()
        .find(|dir| {
            dir.join("pnpm-workspace.yaml").exists()
                || dir.join("lerna.json").exists()
                || declares(dir, "package.json", "workspaces")
                || declares(dir, "deno.json", "workspace")
                || declares(dir, "deno.jsonc", "workspace")
        })
        .map(Path::to_path_buf)
        .unwrap_or_else(|| search_for_package_root(current))
}

/// JavaScript truthiness of a parsed JSON value.
fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use super::toolchain_mismatch;

    fn install(root: &std::path::Path, name: &str, version: &str) {
        let dir = root.join("node_modules").join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("package.json"),
            format!(r#"{{"name":"{name}","version":"{version}"}}"#),
        )
        .unwrap();
    }

    /// A decline must say which package moved, or a patch release silently
    /// sends every app back to the engine.
    #[test]
    fn a_version_decline_names_the_package_and_both_versions() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        install(root, "vite", "8.3.5");
        install(root, "@vitejs/plugin-react", "6.1.3");
        let pinned = [
            ("vite", "8.3.5"),
            ("@vitejs/plugin-react", "6.1.2"),
            ("absent", "1.0.0"),
        ];
        assert_eq!(
            toolchain_mismatch(root, &pinned).as_deref(),
            Some("@vitejs/plugin-react 6.1.3 installed, the template is for 6.1.2")
        );
        assert_eq!(
            toolchain_mismatch(root, &pinned[2..]).as_deref(),
            Some("absent not installed, the template is for 1.0.0")
        );
        assert_eq!(toolchain_mismatch(root, &pinned[..1]), None);
    }
}
