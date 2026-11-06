//! CSS modules: scoping options, `composes`, `localsConvention` and the JS
//! module body, matching postcss-modules as Vite configures it.

use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use lightningcss::css_modules::{self, CssModuleExport, CssModuleExports, CssModuleReference};
use regex::Regex;

use crate::compile::{compile_css_depth, Mode};
use crate::generic_names::GenericNames;
use crate::path::{normalize, strip_query};
use crate::resolve::CssResolve;
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
        compiled(src)
            .is_some_and(|re| re.is_match(path) || abs.as_deref().is_some_and(|a| re.is_match(a)))
    })
}

/// `globalModulePaths` sources compiled once per process (None: invalid).
fn compiled(src: &str) -> Option<Regex> {
    static CACHE: LazyLock<Mutex<HashMap<String, Option<Regex>>>> = LazyLock::new(Default::default);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache
        .entry(src.to_string())
        .or_insert_with(|| Regex::new(src).ok())
        .clone()
}

/// What lightningcss appends to each scoped local, until
/// [`ScopedNames::rename`] gives it the name Vite would.
const DEFAULT_NAME_MARK: &str = "__oj_scoped__";

/// The lightningcss pattern for a module's locals: each one marked, to be
/// named by [`ScopedNames`].
pub(crate) fn scoped_name_pattern() -> css_modules::Pattern {
    css_modules::Pattern::parse(&format!("[local]{DEFAULT_NAME_MARK}")).expect("static pattern")
}

/// The names Vite gives a CSS module's locals: postcss-modules'
/// generateScopedNameDefault, or generic-names for a string
/// `generateScopedName` ([`GenericNames`]).
pub(crate) enum ScopedNames<'a> {
    /// `_${name}_${hash}_${line}`, the hash being string-hash over
    /// `hashPrefix + css` in base 36 (5 characters), the line that of the
    /// first `.${name}` in `css`. `css` is what Vite hands PostCSS: the sheet
    /// after the "pre" plugins and the preprocessor, before `@import`s inline.
    Default {
        css: &'a str,
        hash: String,
    },
    Pattern(GenericNames),
}

impl<'a> ScopedNames<'a> {
    /// `url` names the module (root-relative or absolute); `css` is the sheet
    /// the default names hash.
    pub(crate) fn new(css: &'a str, url: &str, resolve: &CssResolve<'_>) -> Self {
        let modules = resolve.modules;
        let hash_prefix = modules.hash_prefix.as_deref();
        match modules
            .generate_scoped_name
            .as_deref()
            .filter(|s| !s.is_empty())
        {
            Some(pattern) => {
                let root = resolve.root.unwrap_or(Path::new("/"));
                let context = modules.context.as_deref().unwrap_or(root);
                let path = strip_query(url);
                // A dev url outside the root names the file after `/@fs`.
                let file = match path.strip_prefix("/@fs") {
                    Some(abs) if abs.starts_with('/') => PathBuf::from(abs),
                    _ => match Path::new(path) {
                        p if p.is_absolute() && p.is_file() => p.to_path_buf(),
                        _ => root.join(path.trim_start_matches('/')),
                    },
                };
                Self::Pattern(GenericNames::new(pattern, &file, context, hash_prefix))
            }
            None => {
                let digest = string_hash(&format!("{}{css}", hash_prefix.unwrap_or("")));
                let mut hash = base36(digest);
                hash.truncate(5);
                Self::Default { css, hash }
            }
        }
    }

    pub(crate) fn name(&self, local: &str) -> String {
        match self {
            Self::Default { css, hash } => {
                // `css.substr(0, i).split(/[\r\n]/).length`: a missing name
                // (i = -1) takes nothing, so line 1; `\r\n` counts twice.
                let line = css.find(&format!(".{local}")).map_or(1, |i| {
                    css[..i]
                        .chars()
                        .filter(|c| matches!(c, '\r' | '\n'))
                        .count()
                        + 1
                });
                format!("_{local}_{hash}_{line}")
            }
            Self::Pattern(names) => names.name(local),
        }
    }

    /// A class map value (scoped names, space-separated) with its marked
    /// locals renamed.
    pub(crate) fn rename_value(&self, value: &str) -> String {
        value
            .split(' ')
            .map(|part| match part.strip_suffix(DEFAULT_NAME_MARK) {
                Some(local) => self.name(local),
                None => part.to_string(),
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Every marked local in the printed sheet renamed. A local stays escaped
    /// as lightningcss printed it. Only what postcss-modules scopes is
    /// renamed (a class, an id, a `@keyframes` name and an animation's
    /// reference to one); another custom ident lightningcss marked
    /// (`view-transition-name`, `@counter-style`) stays as written.
    pub(crate) fn rename(&self, text: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = text;
        let mut before = 0;
        while let Some(pos) = rest.find(DEFAULT_NAME_MARK) {
            let head = &rest[..pos];
            let start = ident_start(head);
            let token = &head[start..];
            out.push_str(&head[..start]);
            let local = css_unescape(token);
            let context = &text[..before + start];
            before += pos + DEFAULT_NAME_MARK.len();
            if scope_of(context).is_none() {
                out.push_str(token);
                rest = &rest[pos + DEFAULT_NAME_MARK.len()..];
                continue;
            }
            match self {
                // The token keeps its escapes; only the parts around it are
                // new.
                Self::Default { .. } => {
                    let named = self.name(&local);
                    out.push('_');
                    out.push_str(token);
                    out.push_str(&named[1 + local.len()..]);
                }
                // generic-names leaves only identifier characters.
                Self::Pattern(names) => out.push_str(&names.name(&local)),
            }
            rest = &rest[pos + DEFAULT_NAME_MARK.len()..];
        }
        out.push_str(rest);
        out
    }
}

/// The locals the printed (marked) sheet declares as a class, an id or a
/// `@keyframes` name: what postcss-modules exports.
pub(crate) fn declared_locals(text: &str) -> std::collections::HashSet<String> {
    let mut found = std::collections::HashSet::new();
    let mut at = 0;
    while let Some(pos) = text[at..].find(DEFAULT_NAME_MARK) {
        let end = at + pos;
        let start = ident_start(&text[..end]);
        if matches!(scope_of(&text[..start]), Some(Scope::Declared)) {
            found.insert(css_unescape(&text[start..end]));
        }
        at = end + DEFAULT_NAME_MARK.len();
    }
    found
}

#[derive(Debug, PartialEq)]
enum Scope {
    /// A class, an id or a `@keyframes` name.
    Declared,
    /// An `animation` or `animation-name` reference.
    Reference,
}

/// How postcss-modules treats an ident printed after `before`: scoped as a
/// declaration or a reference, or (None) left global.
fn scope_of(before: &str) -> Option<Scope> {
    if before.ends_with('.') || before.ends_with('#') {
        return Some(Scope::Declared);
    }
    let trimmed = before.trim_end();
    if trimmed.ends_with("@keyframes") || trimmed.ends_with("-keyframes") {
        return Some(Scope::Declared);
    }
    let decl = &before[before.rfind([';', '{']).map_or(0, |i| i + 1)..];
    let property = decl.split(':').next().unwrap_or("").trim();
    let unprefixed = ["-webkit-", "-moz-", "-o-", "-ms-"]
        .iter()
        .find_map(|p| property.strip_prefix(p))
        .unwrap_or(property);
    (decl.contains(':') && matches!(unprefixed, "animation" | "animation-name"))
        .then_some(Scope::Reference)
}

/// string-hash 1.1.3: djb2 over UTF-16 code units from the end, `hash * 33 ^
/// c` wrapping to 32 bits as JS's `^` does, read as unsigned.
pub(crate) fn string_hash(s: &str) -> u32 {
    let units: Vec<u16> = s.encode_utf16().collect();
    let mut hash: i32 = 5381;
    for &unit in units.iter().rev() {
        hash = hash.wrapping_mul(33) ^ i32::from(unit);
    }
    hash as u32
}

/// `n.toString(36)`.
fn base36(mut n: u32) -> String {
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    loop {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
        if n == 0 {
            break;
        }
    }
    out.reverse();
    String::from_utf8(out).expect("ascii digits")
}

/// Where the CSS identifier ending `head` starts: name characters, non-ASCII
/// ones and backslash escapes (`\:`, and hex ones like `\31 ` with the space
/// that ends them).
fn ident_start(head: &str) -> usize {
    let chars: Vec<(usize, char)> = head.char_indices().collect();
    let mut i = chars.len();
    while i > 0 {
        let c = chars[i - 1].1;
        let name_char = c.is_ascii_alphanumeric() || c == '_' || c == '-' || !c.is_ascii();
        if name_char {
            i -= 1;
            continue;
        }
        if i >= 2 && chars[i - 2].1 == '\\' {
            i -= 2;
            continue;
        }
        if c == ' ' {
            // `\` + 1-6 hex digits + the space.
            let mut j = i - 1;
            while j > 0 && i - 1 - j < 6 && chars[j - 1].1.is_ascii_hexdigit() {
                j -= 1;
            }
            if j < i - 1 && j > 0 && chars[j - 1].1 == '\\' {
                i = j - 1;
                continue;
            }
        }
        break;
    }
    chars.get(i).map_or(head.len(), |(at, _)| *at)
}

/// A CSS identifier's escapes undone: `\XX ` hex code points and `\c`.
fn css_unescape(token: &str) -> String {
    let mut out = String::with_capacity(token.len());
    let mut chars = token.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let mut hex = String::new();
        while hex.len() < 6 && chars.peek().is_some_and(char::is_ascii_hexdigit) {
            hex.push(chars.next().unwrap());
        }
        if hex.is_empty() {
            if let Some(next) = chars.next() {
                out.push(next);
            }
            continue;
        }
        if chars.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            chars.next();
        }
        out.extend(u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32));
    }
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
    let out =
        compile_css_depth(&dep_url, &source, None, Mode::default(), resolve, depth + 1).ok()?;
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

    // The build scopes a CSS module first when plugins run between the two
    // steps (Vite's "normal" slot); with nothing in between, the two steps
    // ship what the one-step compile does, and the same class map.
    #[test]
    fn scoping_then_compiling_matches_the_one_step_compile() {
        let url = "/src/Counter.module.css";
        let plain = ".button { padding: 1rem; display: flex; }\n.button:hover { opacity: 0.9; }\n:global(.app) .label { color: rebeccapurple; user-select: none; }\n";
        let composing = ".button { padding: 1rem; }\n.other { composes: button; color: red; }\n";
        for minify in [true, false] {
            let cfg = CssResolveConfig {
                minify,
                ..Default::default()
            };
            for src in [plain, composing] {
                let one = compile_css_with(url, src, &cfg.as_ref()).unwrap();
                let scoped = scope_css_module(url, src, src, &cfg.as_ref()).unwrap();
                assert!(!scoped.css.contains(".button"), "scoped: {}", scoped.css);
                assert_eq!(scoped.exports, one.exports, "minify {minify}: {src}");
                let two = compile_scoped_css_with(url, &scoped.css, &cfg.as_ref()).unwrap();
                assert_eq!(two.exports, None, "the scoped compile makes no class map");
                if src == plain {
                    assert_eq!(two.css, one.css, "minify {minify}");
                }
            }
        }
    }

    /// The dev server scopes a module, hands the sheet to its "normal"
    /// plugins, then compiles it with urls rebased: the names are still
    /// Vite's (recorded from Vite 8.3.5's dev server), in the map and the sheet.
    #[test]
    fn dev_scoping_then_compiling_keeps_vites_names() {
        let url = "/src/Counter.module.css";
        let src = ".button { color: red; }\n";
        let cfg = CssResolveConfig::default();
        let scoped = scope_css_module(url, src, src, &cfg.as_ref()).unwrap();
        assert_eq!(
            scoped.exports,
            Some(vec![("button".to_string(), "_button_hwshl_1".to_string())])
        );
        let out = crate::compile_scoped_css_dev(url, &scoped.css, false, &cfg.as_ref()).unwrap();
        assert_eq!(out.exports, None, "the scoped compile makes no class map");
        assert!(out.css.contains("._button_hwshl_1"), "{}", out.css);
        assert!(!out.css.contains(".button"), "{}", out.css);
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
        // generic-names takes any text: no `[local]` names every class the
        // same, `[nope]` included, as in Vite.
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
        assert_eq!(out.exports.unwrap()[0].1, "-nope-");
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

    // Vite's names for these sheets (postcss-modules' generateScopedNameDefault,
    // recorded from a Vite 8.3.5 build): `_${local}_${hash}_${line}`, the hash
    // over the sheet, the line of the first `.${local}` in it, so an id or a
    // keyframes name is line 1 and `\r\n` counts as two lines.
    #[test]
    fn default_scoped_names_are_vites() {
        let names = |url: &str, src: &str| {
            compile_css(url, src, false)
                .unwrap()
                .exports
                .expect("module exports")
        };
        let pairs = |list: &[(&str, &str)]| {
            let mut v: Vec<(String, String)> = list
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            v.sort();
            v
        };
        assert_eq!(
            names("/src/Counter.module.css", ".button { color: red; }\n"),
            pairs(&[("button", "_button_hwshl_1")])
        );
        assert_eq!(
            names(
                "/src/multi.module.css",
                ".a { color: red; }\n\n.b { color: blue; }\n.c { composes: a; }\n#main { color: green; }\n@keyframes spin { to { opacity: 1 } }\n.d { animation: spin 1s; }\n",
            ),
            pairs(&[
                ("a", "_a_hn2ce_1"),
                ("b", "_b_hn2ce_3"),
                ("c", "_c_hn2ce_4 _a_hn2ce_1"),
                ("main", "_main_hn2ce_1"),
                ("d", "_d_hn2ce_7"),
                ("spin", "_spin_hn2ce_1"),
            ])
        );
        assert_eq!(
            names(
                "/src/crlf.module.css",
                ".x {\r\n  color: red;\r\n}\r\n.y { color: blue; }\r\n",
            ),
            pairs(&[("x", "_x_1kb7v_1"), ("y", "_y_1kb7v_7")])
        );
        assert_eq!(
            names(
                "/src/prefix.module.css",
                ".btn-primary { color: red; }\n.btn { color: blue; }\n",
            ),
            pairs(&[
                ("btn", "_btn_n9bj6_1"),
                ("btn-primary", "_btn-primary_n9bj6_1")
            ])
        );
        assert_eq!(
            names(
                "/src/uni.module.css",
                ".h\u{e9}llo { color: red; }\n.ok { color: blue; }\n",
            ),
            pairs(&[("h\u{e9}llo", "_h\u{e9}llo_1vvgd_1"), ("ok", "_ok_1vvgd_2")])
        );
    }

    #[test]
    fn default_scoped_names_rename_the_sheet_too() {
        let out = compile_css(
            "/src/multi.module.css",
            ".a { color: red; }\n\n.b { color: blue; }\n.c { composes: a; }\n#main { color: green; }\n@keyframes spin { to { opacity: 1 } }\n.d { animation: spin 1s; }\n",
            true,
        )
        .unwrap();
        for name in [
            "._a_hn2ce_1",
            "._b_hn2ce_3",
            "#_main_hn2ce_1",
            "@keyframes _spin_hn2ce_1",
            "1s _spin_hn2ce_1",
        ] {
            assert!(out.css.contains(name), "{name} in {}", out.css);
        }
        assert!(!out.css.contains("__oj_scoped__"), "{}", out.css);
        // An escaped local keeps its escape around the new name.
        let out = compile_css(
            "/src/e.module.css",
            ".a\\:b { color: red; }\n.\\31 x { color: blue; }\n",
            false,
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
        assert!(get("a:b").starts_with("_a:b_"), "{exports:?}");
        assert!(
            out.css
                .contains(&format!(".{}", get("a:b").replace(':', "\\:"))),
            "{}",
            out.css
        );
        // A hex escape (a leading digit) and its closing space stay whole.
        assert!(
            get("1x").starts_with("_1x_") && get("1x").ends_with("_1"),
            "{exports:?}"
        );
        let tail = get("1x")["_1x".len()..].to_string();
        assert!(out.css.contains(&format!("._\\31 x{tail}")), "{}", out.css);
    }

    // `css.modules.hashPrefix` salts the hash, as postcss-modules'.
    #[test]
    fn hash_prefix_salts_the_default_names() {
        let cfg = with_modules(CssModulesOptions {
            hash_prefix: Some("salt".into()),
            ..Default::default()
        });
        let out = compile_css_with(
            "/src/Counter.module.css",
            ".button { color: red; }\n",
            &cfg.as_ref(),
        )
        .unwrap();
        assert_eq!(
            out.exports.unwrap(),
            vec![("button".to_string(), "_button_8ywyu_1".to_string())]
        );
    }

    /// A pattern's `[hash]` covers the file's path from postcss-modules'
    /// context, `process.cwd()`: Vite 8.3.5 names `.btn` in
    /// `app/src/a.module.css` `btn__vjDgO` run from `app`, `btn__3BinS` run
    /// from its parent. The file still resolves from the app root.
    #[test]
    fn pattern_names_hash_the_path_from_the_working_directory() {
        let name = |context: Option<&str>| {
            let cfg = CssResolveConfig {
                root: "/w/app".into(),
                modules: CssModulesOptions {
                    generate_scoped_name: Some("[local]__[hash:base64:5]".into()),
                    context: context.map(Into::into),
                    ..Default::default()
                },
                ..Default::default()
            };
            compile_css_with("/src/a.module.css", ".btn { color: red; }\n", &cfg.as_ref())
                .unwrap()
                .exports
                .unwrap()
        };
        let btn = |v: &str| vec![("btn".to_string(), v.to_string())];
        assert_eq!(name(Some("/w/app")), btn("btn__vjDgO"));
        assert_eq!(name(None), btn("btn__vjDgO"), "no context: the app root");
        assert_eq!(name(Some("/w")), btn("btn__3BinS"));
    }

    /// A module outside the root (a monorepo's shared package) is named from
    /// its own path in dev, where its url is `/@fs/<file>`, as in the build:
    /// Vite 8.3.5 names `.btn` in `mono/shared/b.module.css`, run from
    /// `mono/app`, `_-shared-b-module__btn--6zfUj` in both.
    #[test]
    fn pattern_names_of_a_module_outside_the_root_follow_its_file() {
        let cfg = CssResolveConfig {
            root: "/x/mono/app".into(),
            modules: CssModulesOptions {
                generate_scoped_name: Some("[path][name]__[local]--[hash:base64:5]".into()),
                context: Some("/x/mono/app".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let name = |url: &str| {
            compile_css_with(url, ".btn { color: red; }\n", &cfg.as_ref())
                .unwrap()
                .exports
                .unwrap()
        };
        let btn = vec![(
            "btn".to_string(),
            "_-shared-b-module__btn--6zfUj".to_string(),
        )];
        assert_eq!(name("/@fs/x/mono/shared/b.module.css"), btn, "dev");
    }

    /// postcss-modules scopes classes, ids, `@keyframes` and animation
    /// references, and leaves grid lines, `view-transition-name`,
    /// `@counter-style` and container names global (a grid line name used to
    /// fail the compile). The sheet and the map are what Vite 8.3.5 builds.
    #[test]
    fn only_what_postcss_modules_scopes_is_scoped() {
        let src = ".grid { display: grid; grid-template-columns: [a] 1fr [b]; grid-template-areas: \"head\"; }\n\
.y { grid-area: head; grid-column: a / b; }\n\
.hero { view-transition-name: hero; }\n\
@counter-style thumbs { system: cyclic; symbols: \"👍\"; suffix: \" \"; }\n\
.list { list-style: thumbs; }\n\
.card { container-name: card; container-type: inline-size; }\n\
@container card (min-width: 1px) { .y { color: red; } }\n\
@keyframes spin { to { opacity: 1; } }\n\
.s { animation: spin 1s; }\n";
        let cfg = CssResolveConfig {
            minify: true,
            ..Default::default()
        };
        let out = compile_css_with("/src/g.module.css", src, &cfg.as_ref()).unwrap();
        assert_eq!(
            out.css.trim_end(),
            "._grid_1skpg_1{grid-template-columns:[a]1fr[b];grid-template-areas:\"head\";display:grid}._y_1skpg_2{grid-area:head/a/head/b}._hero_1skpg_3{view-transition-name:hero}@counter-style thumbs{system:cyclic;symbols:\"👍\";suffix:\" \"}._list_1skpg_5{list-style:thumbs}._card_1skpg_6{container:card/inline-size}@container card (width>=1px){._y_1skpg_2{color:red}}@keyframes _spin_1skpg_1{to{opacity:1}}._s_1skpg_9{animation:1s _spin_1skpg_1}"
        );
        let mut keys: Vec<String> = out.exports.unwrap().into_iter().map(|(k, _)| k).collect();
        keys.sort();
        assert_eq!(keys, ["card", "grid", "hero", "list", "s", "spin", "y"]);
        // Scoped in two steps (dev and the build's normal slot), the same.
        let scoped = scope_css_module("/src/g.module.css", src, src, &cfg.as_ref()).unwrap();
        assert!(
            scoped.css.contains("view-transition-name: hero"),
            "{}",
            scoped.css
        );
        assert!(
            scoped.css.contains("animation: 1s _spin_1skpg_1"),
            "{}",
            scoped.css
        );
    }

    // string-hash 1.1.3, in JS: `(hash * 33) ^ str.charCodeAt(--i)` from
    // 5381, `>>> 0`.
    #[test]
    fn string_hash_matches_the_js_package() {
        assert_eq!(string_hash(""), 5381);
        assert_eq!(string_hash("a"), 177604);
        // Wraps past 32 bits and reads UTF-16 code units.
        assert_eq!(string_hash("hello world"), 2616892229);
        assert_eq!(string_hash("\u{1F600}"), 5191800);
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
}
