//! CSS modules: scoping options, `composes`, `localsConvention` and the JS
//! module body, matching postcss-modules as Vite configures it.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use lightningcss::css_modules::{self, CssModuleExport, CssModuleExports, CssModuleReference};

use crate::compile::{compile_css_depth, Mode};
use crate::path::{normalize, strip_query};
use crate::resolve::{CssModulesOptions, CssResolve};
use crate::sass::{compile_sass, is_sass};

pub fn is_css_module(url: &str) -> bool {
    let path = url.split('?').next().unwrap_or(url);
    path.rsplit('/')
        .next()
        .is_some_and(|f| f.contains(".module."))
}

/// The JS module for a class map, shaped like Vite's `dataToEsm(map, {
/// namedExports: true })`: the map is the default export and every legal
/// identifier is also a named export.
pub fn css_modules_esm(exports: &[(String, String)]) -> String {
    let mut out = String::new();
    let mut map = serde_json::Map::new();
    for (name, scoped) in exports {
        let value = serde_json::Value::String(scoped.clone());
        if is_legal_identifier(name) {
            out.push_str(&format!("export const {name} = {value};\n"));
        }
        map.insert(name.clone(), value);
    }
    out.push_str(&format!(
        "export default {};\n",
        serde_json::Value::Object(map)
    ));
    out
}

/// Reserved words and global builtins @rollup/pluginutils' makeLegalIdentifier
/// rejects.
#[rustfmt::skip]
static FORBIDDEN: LazyLock<HashSet<&str>> = LazyLock::new(|| HashSet::from([
    "break", "case", "class", "catch", "const", "continue", "debugger", "default", "delete",
    "do", "else", "export", "extends", "finally", "for", "function", "if", "import", "in",
    "instanceof", "let", "new", "return", "super", "switch", "this", "throw", "try",
    "typeof", "var", "void", "while", "with", "yield", "enum", "await", "implements",
    "package", "protected", "static", "interface", "private", "public", "arguments",
    "Infinity", "NaN", "undefined", "null", "true", "false", "eval", "uneval", "isFinite",
    "isNaN", "parseFloat", "parseInt", "decodeURI", "decodeURIComponent", "encodeURI",
    "encodeURIComponent", "escape", "unescape", "Object", "Function", "Boolean", "Symbol",
    "Error", "EvalError", "InternalError", "RangeError", "ReferenceError", "SyntaxError",
    "TypeError", "URIError", "Number", "Math", "Date", "String", "RegExp", "Array",
    "Int8Array", "Uint8Array", "Uint8ClampedArray", "Int16Array", "Uint16Array",
    "Int32Array", "Uint32Array", "Float32Array", "Float64Array", "Map", "Set", "WeakMap",
    "WeakSet", "SIMD", "ArrayBuffer", "DataView", "JSON", "Promise", "Generator",
    "GeneratorFunction", "Reflect", "Proxy", "Intl",
]));

/// `key === makeLegalIdentifier(key)`.
fn is_legal_identifier(name: &str) -> bool {
    let ident_char = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '$';
    name.chars().next().is_some_and(|c| !c.is_ascii_digit())
        && name.chars().all(ident_char)
        && !FORBIDDEN.contains(name)
}

/// Whether a `.module.css` file is scoped: not `scopeBehaviour: "global"` and
/// not matched by `globalModulePaths`.
pub(crate) fn module_is_scoped(url: &str, resolve: &CssResolve<'_>) -> bool {
    let modules = resolve.modules;
    if modules.global_scope {
        return false;
    }
    if modules.global_module_paths.is_empty() {
        return true;
    }
    let path = strip_query(url);
    let abs = module_file_path(url, resolve).map(|p| p.to_string_lossy().into_owned());
    !modules.global_module_paths.iter().any(|src| {
        regex::Regex::new(src)
            .is_ok_and(|re| re.is_match(path) || abs.as_deref().is_some_and(|a| re.is_match(a)))
    })
}

/// `generateScopedName` as a lightningcss pattern. postcss-modules tokens map
/// over (`[hash:base64:5]` -> `[hash]`, `[contenthash]` -> `[content-hash]`);
/// `[path]`, `[folder]` and `[ext]` have no equivalent and are dropped.
pub(crate) fn scoped_name_pattern(modules: &CssModulesOptions) -> css_modules::Pattern {
    const DEFAULT: &str = "[name]_[local]_[hash]";
    let default = || css_modules::Pattern::parse(DEFAULT).expect("static pattern");
    let Some(raw) = modules
        .generate_scoped_name
        .as_deref()
        .filter(|s| !s.is_empty())
    else {
        return default();
    };
    let translated = translate_scoped_name(raw);
    let parsed = if translated.contains("[local]") {
        css_modules::Pattern::parse(&translated).map_err(|e| format!("{e:?}"))
    } else {
        Err("no [local] placeholder".to_string())
    };
    parsed.unwrap_or_else(|e| {
        eprintln!("oj: css.modules.generateScopedName {raw:?} is not a supported pattern ({e}); using {DEFAULT}");
        default()
    })
}

fn translate_scoped_name(raw: &str) -> String {
    let mut out = String::new();
    let mut rest = raw;
    while let Some(start) = rest.find('[') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find(']') else {
            rest = &rest[start..];
            break;
        };
        let token = rest[start + 1..start + end].to_ascii_lowercase();
        if token.starts_with("contenthash") || token.starts_with("content-hash") {
            out.push_str("[content-hash]");
        } else if token.starts_with("hash") {
            out.push_str("[hash]");
        } else if token == "name" || token == "local" {
            out.push_str(&format!("[{token}]"));
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    out
}

/// The file a compile `url` names on disk: an absolute path as-is, else
/// root-relative.
fn module_file_path(url: &str, resolve: &CssResolve<'_>) -> Option<PathBuf> {
    let path = strip_query(url);
    let p = Path::new(path);
    if p.is_absolute() && p.is_file() {
        return Some(p.to_path_buf());
    }
    let joined = resolve.root?.join(path.trim_start_matches('/'));
    joined.is_file().then_some(joined)
}

/// lodash `camelCase`: words split on non-alphanumerics and lower-to-upper
/// transitions, the first lowercased, the rest capitalized.
fn camel_case(s: &str) -> String {
    let mut words: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut prev_lower = false;
    for c in s.chars() {
        let boundary = !c.is_alphanumeric() || (c.is_uppercase() && prev_lower);
        if boundary && !cur.is_empty() {
            words.push(std::mem::take(&mut cur));
        }
        if !c.is_alphanumeric() {
            prev_lower = false;
            continue;
        }
        prev_lower = c.is_lowercase() || c.is_ascii_digit();
        cur.push(c);
    }
    if !cur.is_empty() {
        words.push(cur);
    }
    let mut out = String::new();
    for (i, w) in words.iter().enumerate() {
        let lower = w.to_lowercase();
        let mut chars = lower.chars();
        match chars.next() {
            Some(first) if i > 0 => {
                out.extend(first.to_uppercase());
                out.push_str(chars.as_str());
            }
            _ => out.push_str(&lower),
        }
    }
    out
}

/// postcss-modules' `dashesCamelCase`: only `-x` runs become `X`.
fn dashes_camel_case(s: &str) -> String {
    let mut out = String::new();
    let mut upper_next = false;
    for c in s.chars() {
        if c == '-' {
            upper_next = true;
        } else if std::mem::take(&mut upper_next) {
            out.extend(c.to_uppercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `localsConvention` on an export map: `camelCase` / `dashes` add the
/// converted key beside the original, the `*Only` forms replace it. A later
/// key overwrites an earlier one, as postcss-modules assigns them.
fn apply_locals_convention(
    pairs: Vec<(String, String)>,
    convention: Option<&str>,
) -> Vec<(String, String)> {
    let (convert, only): (fn(&str) -> String, bool) = match convention {
        Some("camelCase") => (camel_case, false),
        Some("camelCaseOnly") => (camel_case, true),
        Some("dashes") => (dashes_camel_case, false),
        Some("dashesOnly") => (dashes_camel_case, true),
        _ => return pairs,
    };
    let mut out = BTreeMap::new();
    for (name, value) in pairs {
        let converted = convert(&name);
        if !only {
            out.insert(name, value.clone());
        }
        out.insert(converted, value);
    }
    out.into_iter().collect()
}

/// The export map with `composes` expanded as postcss-modules exports it: the
/// scoped name, then every composed class (locals transitively, globals as
/// written, `from "./other.css"` compiled from that file), sorted by name.
pub(crate) fn module_exports(
    map: CssModuleExports,
    url: &str,
    resolve: &CssResolve<'_>,
    depth: u8,
) -> Vec<(String, String)> {
    let composer = Composer {
        by_scoped: map.values().map(|e| (e.name.as_str(), e)).collect(),
        deps: RefCell::default(),
        url,
        resolve,
        depth,
    };
    let mut pairs: Vec<(String, String)> = map
        .iter()
        .map(|(name, export)| (name.clone(), composer.value(export, &mut Vec::new())))
        .collect();
    pairs.sort();
    apply_locals_convention(pairs, resolve.modules.locals_convention.as_deref())
}

struct Composer<'a> {
    by_scoped: HashMap<&'a str, &'a CssModuleExport>,
    /// Export maps of `from "..."` files by specifier, each compiled once.
    deps: RefCell<HashMap<&'a str, Option<HashMap<String, String>>>>,
    url: &'a str,
    resolve: &'a CssResolve<'a>,
    depth: u8,
}

impl<'a> Composer<'a> {
    fn value(&self, export: &'a CssModuleExport, seen: &mut Vec<&'a str>) -> String {
        let mut value = export.name.clone();
        for r in &export.composes {
            let part = match r {
                CssModuleReference::Local { name } => {
                    match self.by_scoped.get_key_value(name.as_str()) {
                        Some((&key, inner)) if !seen.contains(&key) => {
                            seen.push(key);
                            let v = self.value(inner, seen);
                            seen.pop();
                            v
                        }
                        _ => name.clone(),
                    }
                }
                CssModuleReference::Global { name } => name.clone(),
                CssModuleReference::Dependency { name, specifier } => {
                    let mut deps = self.deps.borrow_mut();
                    let exports = deps.entry(specifier).or_insert_with(|| {
                        dependency_exports(specifier, self.url, self.resolve, self.depth)
                    });
                    match exports.as_ref().and_then(|e| e.get(name)) {
                        Some(v) => v.clone(),
                        None => {
                            eprintln!(
                                "oj: css module {}: cannot resolve `composes: {name} from {specifier:?}`",
                                self.url
                            );
                            continue;
                        }
                    }
                }
            };
            if !part.is_empty() {
                value.push(' ');
                value.push_str(&part);
            }
        }
        value
    }
}

/// The export map of the module file a `composes: ... from "spec"` names
/// (relative, root-absolute or aliased), compiled with the same settings.
fn dependency_exports(
    spec: &str,
    url: &str,
    resolve: &CssResolve<'_>,
    depth: u8,
) -> Option<HashMap<String, String>> {
    if depth > 8 {
        return None;
    }
    let file = module_file_path(url, resolve)?;
    let dep = if let Some(p) = resolve.alias_path(spec) {
        p
    } else if let Some(rest) = spec.strip_prefix('/') {
        resolve.root?.join(rest)
    } else {
        file.parent()?.join(spec)
    };
    // Lexical only: the url (and so the `[hash]`) must match the spelling the
    // file gets when imported directly.
    let dep = normalize(&dep);
    let mut source = std::fs::read_to_string(&dep).ok()?;
    if is_sass(&dep.to_string_lossy()) {
        source = compile_sass(&source, dep.parent()).ok()?;
    }
    let dep_url = match resolve.root.and_then(|r| dep.strip_prefix(r).ok()) {
        Some(rel) => format!("/{}", rel.display()),
        None => dep.to_string_lossy().into_owned(),
    };
    let out = compile_css_depth(&dep_url, &source, Mode::default(), resolve, depth + 1).ok()?;
    Some(out.exports?.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::*;

    #[test]
    fn is_css_module_matches_only_the_filename() {
        assert!(is_css_module("/src/app.module.css"));
        assert!(is_css_module("app.module.scss"));
        assert!(is_css_module("/a/b.module.css?used"));
        assert!(!is_css_module("/src/styles.css"));
        assert!(!is_css_module("/module.styles/app.css"));
    }

    #[test]
    fn css_modules_scope_and_export_class_names() {
        let out = compile_css(
            "/src/Counter.module.css",
            ".button { padding: 1rem; } .button:hover { opacity: 0.9; }",
            false,
        )
        .unwrap();
        let exports = out.exports.expect("module exports");
        assert_eq!(exports.len(), 1);
        let (name, scoped) = &exports[0];
        assert_eq!(name, "button");
        assert_ne!(scoped, "button", "class must be scoped: {scoped}");
        assert!(out.css.contains(scoped.as_str()), "{}", out.css);
    }

    fn with_modules(modules: CssModulesOptions) -> CssResolveConfig {
        CssResolveConfig {
            modules,
            minify: true,
            ..Default::default()
        }
    }

    #[test]
    fn css_modules_locals_convention_shapes_the_export_map() {
        let src = ".my-class { color: red } .foo_bar { color: blue } .Plain { color: green }";
        let keys = |conv: &str| {
            let cfg = with_modules(CssModulesOptions {
                locals_convention: Some(conv.into()),
                ..Default::default()
            });
            compile_css_with("/src/a.module.css", src, &cfg.as_ref())
                .unwrap()
                .exports
                .unwrap()
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            keys("camelCase"),
            ["Plain", "fooBar", "foo_bar", "my-class", "myClass", "plain"]
        );
        assert_eq!(keys("camelCaseOnly"), ["fooBar", "myClass", "plain"]);
        assert_eq!(keys("dashes"), ["Plain", "foo_bar", "my-class", "myClass"]);
        assert_eq!(keys("dashesOnly"), ["Plain", "foo_bar", "myClass"]);
        // Converted keys carry the same scoped value as the original.
        let cfg = with_modules(CssModulesOptions {
            locals_convention: Some("camelCase".into()),
            ..Default::default()
        });
        let out = compile_css_with("/src/a.module.css", src, &cfg.as_ref())
            .unwrap()
            .exports
            .unwrap();
        let get = |k: &str| {
            out.iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(get("my-class"), get("myClass"));
    }

    #[test]
    fn css_modules_generate_scoped_name_pattern_is_honored() {
        let cfg = with_modules(CssModulesOptions {
            generate_scoped_name: Some("[local]__[hash:base64:5]".into()),
            ..Default::default()
        });
        let out = compile_css_with(
            "/src/Btn.module.css",
            ".button { color: red }",
            &cfg.as_ref(),
        )
        .unwrap();
        let (_, scoped) = &out.exports.unwrap()[0];
        assert!(scoped.starts_with("button__"), "{scoped}");
        assert!(!scoped.contains("Btn"), "{scoped}");
        let cfg = with_modules(CssModulesOptions {
            generate_scoped_name: Some("app-[name]-[local]".into()),
            ..Default::default()
        });
        let out = compile_css_with(
            "/src/Btn.module.css",
            ".button { color: red }",
            &cfg.as_ref(),
        )
        .unwrap();
        assert_eq!(out.exports.unwrap()[0].1, "app-Btn-module-button");
        // An unsupported pattern falls back to the default instead of failing.
        let cfg = with_modules(CssModulesOptions {
            generate_scoped_name: Some("[nope]".into()),
            ..Default::default()
        });
        let out = compile_css_with(
            "/src/Btn.module.css",
            ".button { color: red }",
            &cfg.as_ref(),
        )
        .unwrap();
        assert!(out.exports.unwrap()[0].1.starts_with("Btn-module_button_"));
    }

    #[test]
    fn css_modules_global_scope_and_global_module_paths_compile_unscoped() {
        let cfg = with_modules(CssModulesOptions {
            global_scope: true,
            ..Default::default()
        });
        let out =
            compile_css_with("/src/a.module.css", ".x { color: red }", &cfg.as_ref()).unwrap();
        assert_eq!(out.css, ".x{color:red}");
        assert_eq!(
            out.exports,
            Some(Vec::new()),
            "still a module to its importer, with no locals"
        );

        let cfg = with_modules(CssModulesOptions {
            global_module_paths: vec![r"global\.module\.css$".into()],
            ..Default::default()
        });
        let g = compile_css_with(
            "/src/theme.global.module.css",
            ".x { color: red }",
            &cfg.as_ref(),
        )
        .unwrap();
        assert_eq!(g.css, ".x{color:red}");
        assert_eq!(g.exports, Some(Vec::new()));
        let scoped =
            compile_css_with("/src/a.module.css", ".x { color: red }", &cfg.as_ref()).unwrap();
        assert_ne!(
            scoped.css, ".x{color:red}",
            "non-matching modules stay scoped"
        );
    }

    #[test]
    fn css_modules_composes_locals_globals_and_other_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/base.module.css"),
            ".base { padding: 1px } .more { composes: base; margin: 0 }",
        )
        .unwrap();
        let cfg = CssResolveConfig {
            root: dir.path().to_path_buf(),
            ..Default::default()
        };
        std::fs::write(
            dir.path().join("src/a.module.css"),
            ".a { color: red } .b { composes: a; color: blue } .c { composes: g from global; } .d { composes: more from \"./base.module.css\"; }",
        )
        .unwrap();
        let out = compile_css_with(
            "/src/a.module.css",
            &std::fs::read_to_string(dir.path().join("src/a.module.css")).unwrap(),
            &cfg.as_ref(),
        )
        .unwrap();
        let exports = out.exports.unwrap();
        let get = |k: &str| {
            exports
                .iter()
                .find(|(n, _)| n == k)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        assert_eq!(
            get("b"),
            format!("{} {}", get("b").split(' ').next().unwrap(), get("a"))
        );
        assert_eq!(get("c").split(' ').nth(1), Some("g"));
        let base = compile_css_with(
            "/src/base.module.css",
            &std::fs::read_to_string(dir.path().join("src/base.module.css")).unwrap(),
            &cfg.as_ref(),
        )
        .unwrap()
        .exports
        .unwrap();
        let base_more = base
            .iter()
            .find(|(n, _)| n == "more")
            .map(|(_, v)| v.clone())
            .unwrap();
        assert!(
            base_more.contains(' '),
            "more composes base transitively: {base_more}"
        );
        let d = get("d");
        assert!(
            d.ends_with(&base_more),
            "d = {d}, expected suffix {base_more}"
        );
    }

    #[test]
    fn css_modules_scoped_name_matches_ssr_loader() {
        // The retired node SSR loader (its JS cssModuleExports mirror)
        // recomputes these names in JS so server rendering agrees with the
        // class map the client is served; ssr-loader-css-modules.test.mjs pins
        // the same literals. If this assertion changes (lightningcss upgrade,
        // pattern change), the loader must be updated to match.
        let out = compile_css("/src/Counter.module.css", ".button { color: red; }", false).unwrap();
        let exports = out.exports.expect("module exports");
        assert_eq!(
            exports,
            vec![(
                "button".to_string(),
                "Counter-module_button_EjW_Uq".to_string()
            )]
        );
    }

    #[test]
    fn scoped_class_names_follow_the_name_local_hash_pattern() {
        // The scoped name shows up in devtools, in snapshots and in the exports
        // map, so its shape is part of the contract.
        let out = compile_css("/src/Counter.module.css", ".button { color: red }", false).unwrap();
        let (local, scoped) = out.exports.expect("exports").into_iter().next().unwrap();
        assert_eq!(local, "button");
        // `[name]` is the file stem with its dots flattened.
        let prefix = "Counter-module_button_";
        assert!(
            scoped.starts_with(prefix),
            "expected [name]_[local]_[hash], got {scoped}"
        );
        let hash = &scoped[prefix.len()..];
        assert!(!hash.is_empty(), "no hash in {scoped}");
        assert!(
            hash.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
            "hash is not a plain token: {scoped}"
        );
        // The hash is derived from the path, not the contents: editing a
        // stylesheet must not rename its classes, or every edit would invalidate
        // the markup that already references them.
        let after_edit = compile_css("/src/Counter.module.css", ".button { color: blue }", false)
            .unwrap()
            .exports
            .expect("exports")
            .remove(0)
            .1;
        assert_eq!(scoped, after_edit, "an edit must not rename a class");
    }

    #[test]
    fn css_modules_esm_names_legal_identifiers_and_keeps_the_whole_map_default() {
        let exports = vec![
            ("button".to_string(), "m_button_h".to_string()),
            ("my-class".to_string(), "m_my-class_h".to_string()),
            ("default".to_string(), "m_default_h".to_string()),
            ("class".to_string(), "m_class_h".to_string()),
            ("Map".to_string(), "m_Map_h".to_string()),
            ("_private".to_string(), "m__private_h".to_string()),
            ("$x".to_string(), "m_x_h".to_string()),
            ("1st".to_string(), "m_1st_h".to_string()),
        ];
        let js = css_modules_esm(&exports);
        assert!(js.contains("export const button = \"m_button_h\";"), "{js}");
        assert!(
            js.contains("export const _private = ") && js.contains("export const $x = "),
            "{js}"
        );
        for bad in ["my-class", "default", "class", "Map", "1st"] {
            assert!(
                !js.contains(&format!("export const {bad} ")),
                "{bad} must not be a named export: {js}"
            );
        }
        assert!(js.contains("export default {"), "{js}");
        for key in [
            "button", "my-class", "default", "class", "Map", "_private", "$x", "1st",
        ] {
            assert!(
                js.contains(&format!("\"{key}\":\"m_")),
                "{key} in the default map: {js}"
            );
        }
        assert_eq!(css_modules_esm(&[]), "export default {};\n");
    }

    #[test]
    fn locals_convention_never_emits_a_key_twice() {
        let pairs = vec![
            ("foo-bar".to_string(), "a".to_string()),
            ("fooBar".to_string(), "b".to_string()),
        ];
        let out = apply_locals_convention(pairs, Some("camelCase"));
        assert_eq!(
            out,
            [
                ("foo-bar".into(), "a".into()),
                ("fooBar".into(), "b".into())
            ]
        );
    }

    #[test]
    fn scoped_name_keeps_an_unterminated_bracket() {
        assert_eq!(translate_scoped_name("[local]_[hash"), "[local]_[hash");
        assert_eq!(translate_scoped_name("x-[path][local]"), "x-[local]");
    }
}
