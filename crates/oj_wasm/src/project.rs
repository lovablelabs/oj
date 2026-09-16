// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The in-browser build: an in-memory file map through the same per-file
//! pipeline the native dev server uses (`oj_compiler` for TS/JSX, `oj_css` for
//! CSS/Sass/CSS Modules), producing a set of ES modules addressed by stable
//! specifiers. The host page maps each specifier to a Blob url with an import
//! map, so module cycles cost nothing and no service worker is needed.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::LazyLock;

use oj_compiler::html::html_attr;
use oj_compiler::{compile_module, CompileOptions};
use oj_css::{compile_css, compile_sass, css_modules_esm, is_sass};
use regex::Regex;
use serde::Serialize;

/// Prefix that turns an in-memory absolute path into a bare import-map
/// specifier: `/src/App.tsx` is served as `@app/src/App.tsx`.
pub const MODULE_PREFIX: &str = "@app";

const JS_EXTS: &[&str] = &["tsx", "ts", "jsx", "js", "mjs", "mts", "cts"];
// The native resolver's lists, so `./foo` and a NodeNext `./x.js` pick exactly
// the file the dev server would.
static PROBE_EXTS: LazyLock<Vec<String>> = LazyLock::new(oj_resolver::default_extensions);
static EXT_ALIAS: LazyLock<Vec<(String, Vec<String>)>> = LazyLock::new(oj_resolver::default_extension_alias);

#[derive(Debug, Serialize)]
pub struct BuildResult {
    pub ok: bool,
    /// `index.html` with module scripts rewritten to import-map specifiers and
    /// stylesheet links inlined.
    pub html: String,
    pub modules: Vec<Module>,
    /// Bare specifiers left for the host page to map (react, react-dom/...).
    pub bare: Vec<String>,
    pub errors: Vec<BuildError>,
}

#[derive(Debug, Serialize)]
pub struct Module {
    pub id: String,
    pub code: String,
}

#[derive(Debug, Serialize)]
pub struct BuildError {
    pub path: String,
    pub message: String,
}

impl BuildResult {
    fn failed(path: &str, message: String) -> Self {
        BuildResult {
            ok: false,
            html: String::new(),
            modules: Vec::new(),
            bare: Vec::new(),
            errors: vec![BuildError { path: path.to_string(), message }],
        }
    }
}

/// Collapse `.` and `..` segments into a leading-slash path.
pub fn normalize_abs(path: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

fn dir_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(0) | None => "/",
        Some(i) => &path[..i],
    }
}

fn is_bare(spec: &str) -> bool {
    !(spec.starts_with("./") || spec.starts_with("../") || spec.starts_with('/'))
}

/// `https://...`, `data:...`: an absolute url the browser resolves itself; it
/// must neither be rewritten nor land in the bare set (mapping it to a CDN
/// would double-wrap a working url).
fn has_scheme(spec: &str) -> bool {
    spec.split_once(':').is_some_and(|(scheme, _)| {
        let mut chars = scheme.chars();
        chars.next().is_some_and(|c| c.is_ascii_alphabetic())
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    })
}

/// Resolve a relative or root-absolute specifier against the file map, probing
/// extensions, extension aliases (NodeNext `./x.js` -> `x.ts`), and directory
/// indexes with the native resolver's own lists.
pub fn resolve(files: &BTreeMap<String, String>, importer_dir: &str, spec: &str) -> Option<String> {
    let spec = spec.split(['?', '#']).next().unwrap_or(spec);
    let joined = if spec.starts_with('/') {
        normalize_abs(spec)
    } else {
        normalize_abs(&format!("{importer_dir}/{spec}"))
    };
    if files.contains_key(&joined) {
        return Some(joined);
    }
    for (ext, alternates) in EXT_ALIAS.iter() {
        if let Some(stem) = joined.strip_suffix(ext.as_str()) {
            for alt in alternates {
                let probe = format!("{stem}{alt}");
                if files.contains_key(&probe) {
                    return Some(probe);
                }
            }
        }
    }
    for ext in PROBE_EXTS.iter() {
        let probe = format!("{joined}{ext}");
        if files.contains_key(&probe) {
            return Some(probe);
        }
    }
    for ext in PROBE_EXTS.iter() {
        let probe = format!("{joined}/index{ext}");
        if files.contains_key(&probe) {
            return Some(probe);
        }
    }
    None
}

fn ext_of(path: &str) -> &str {
    Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("")
}

// Tags are matched whole (attributes read by the shared `html_attr` scanner);
// SCRIPT_RE also captures the body so an inline module script can be refused
// loudly instead of shipping imports that cannot resolve from about:srcdoc.
static SCRIPT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?is)<script\b[^>]*/\s*>|(<script\b[^>]*>)(.*?)</script\s*>").unwrap());
static LINK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)<link\b[^>]*/?>").unwrap());
static COMMENT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<!--.*?-->").unwrap());
static STYLE_CLOSE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</(style)").unwrap());
static CSS_IMPORT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)(?:^|;)\s*@import\b").unwrap());
static SASS_LOAD_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^\s*@(use|import|forward)\b").unwrap());
static SASS_COMMENT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)/\*.*?\*/|//[^\n]*").unwrap());
static COMPOSES_FROM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"\bcomposes\s*:[^;{}]*\bfrom\s+["']"#).unwrap());

/// `//cdn.example/x`: protocol-relative urls are external like scheme urls.
fn is_external(spec: &str) -> bool {
    has_scheme(spec) || spec.starts_with("//")
}

/// `html_attr` takes the text between `<` and `>`; with the bracket left on, a
/// bare (unquoted) value at the end of the tag would swallow it.
fn tag_inner(tag: &str) -> &str {
    tag.trim_end_matches('>').trim_end_matches('/')
}

/// A scan copy of `html` with every comment's bytes blanked to spaces (length
/// preserved, so match ranges map straight onto the original). Comment spans
/// are regex matches on the str, so the range edges sit on char boundaries and
/// an all-ASCII fill keeps the copy valid utf-8.
fn blank_comments(html: &str) -> String {
    let mut bytes = html.as_bytes().to_vec();
    for m in COMMENT_RE.find_iter(html) {
        bytes[m.range()].fill(b' ');
    }
    String::from_utf8(bytes).expect("ascii fill preserves utf-8")
}

/// Apply non-overlapping `(range, replacement)` edits (sorted by start) to the
/// original text.
fn splice(original: &str, edits: Vec<(std::ops::Range<usize>, String)>) -> String {
    let mut out = String::with_capacity(original.len());
    let mut cursor = 0;
    for (range, replacement) in edits {
        out.push_str(&original[cursor..range.start]);
        out.push_str(&replacement);
        cursor = range.end;
    }
    out.push_str(&original[cursor..]);
    out
}

/// Make css safe inside a `<style>` element: the only sequence that can end it
/// early is `</style`, and `<\/style` is byte-identical once css unescapes the
/// `\/` (an escaped `/`), so strings like `content: "</style>"` still render.
fn escape_style_text(css: &str) -> String {
    STYLE_CLOSE_RE.replace_all(css, "<\\/$1").into_owned()
}

fn module_id(path: &str) -> String {
    format!("{MODULE_PREFIX}{path}")
}

/// A JS module that installs `css` in a `<style>` tag keyed by module id, then
/// exports it (plus the scoped class map for CSS Modules).
fn css_to_js(id: &str, css: &str, exports: Option<&[(String, String)]>) -> String {
    let css_lit = serde_json::Value::String(css.to_string());
    let selector = serde_json::Value::String(format!("style[data-oj-id=\"{id}\"]"));
    let id_lit = serde_json::Value::String(id.to_string());
    let mut out = format!(
        "const css = {css_lit};\n\
         let el = document.querySelector({selector});\n\
         if (!el) {{\n\
           el = document.createElement(\"style\");\n\
           el.setAttribute(\"data-oj-id\", {id_lit});\n\
           document.head.appendChild(el);\n\
         }}\n\
         el.textContent = css;\n"
    );
    match exports {
        Some(pairs) => out.push_str(&css_modules_esm(pairs)),
        None => out.push_str("export default css;\n"),
    }
    out
}

fn compile_stylesheet(path: &str, source: &str) -> Result<(String, Option<Vec<(String, String)>>), String> {
    let id = module_id(path);
    // Cross-file `composes: x from "./other.css"` resolves through std::fs in
    // oj_css and would silently drop the composed classes on wasm32. Scoping
    // is judged the way oj_css judges it (`.module.` in the FILENAME).
    if oj_css::is_css_module(path) && COMPOSES_FROM_RE.is_match(source) {
        return Err(
            "css modules `composes: ... from` another file is not supported in the wasm playground yet".to_string(),
        );
    }
    let plain = if is_sass(path) {
        // grass resolves @use/@import through std::fs, which cannot see the
        // in-memory tree (and does nothing on wasm32); fail with a clear
        // message instead of a confusing native file-not-found. Comments are
        // stripped from the detection copy so a commented-out @use is fine.
        if SASS_LOAD_RE.is_match(&SASS_COMMENT_RE.replace_all(source, "")) {
            return Err(
                "sass @use/@import is not supported in the wasm playground yet; keep the stylesheet self-contained".to_string(),
            );
        }
        compile_sass(source, None)?
    } else {
        source.to_string()
    };
    // Module scoping keys off the url (`.module.` in the filename).
    let out = compile_css(&id, &plain, false)?;
    // Without the dev server's rebase pass an `@import` survives verbatim and
    // would resolve against the preview document, silently loading nothing;
    // fail loudly instead until the graph walks css imports too. Anchored to a
    // line start or a rule boundary so a string value like
    // `content: "@import"` cannot false-positive but `@charset ...;@import`
    // on one line is still caught.
    if CSS_IMPORT_RE.is_match(&out.css) {
        return Err("css @import is not supported in the wasm playground yet; inline the file or import it from a JS module".to_string());
    }
    Ok((out.css, out.exports))
}

pub fn build(files: &BTreeMap<String, String>) -> BuildResult {
    let Some(html) = files.get("/index.html") else {
        return BuildResult::failed("/index.html", "project has no /index.html".to_string());
    };

    let mut errors: Vec<BuildError> = Vec::new();
    let mut bare: BTreeSet<String> = BTreeSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    // The tags are matched against a scan copy whose comment interiors are
    // blanked (byte length preserved), so a commented-out tag never matches
    // while the delivered document keeps its comments verbatim. Matched spans
    // are then spliced into the ORIGINAL html by byte range.
    // <script type="module" src="..."> becomes an inline import of the module
    // specifier (a script src never goes through the import map; an inline
    // `import` does), and a local <link rel="stylesheet" href="..."> is
    // inlined. External urls in either tag are the browser's business and pass
    // through verbatim.
    let scan = blank_comments(html);
    let mut edits: Vec<(std::ops::Range<usize>, String)> = Vec::new();
    let mut saw_module_script = false;
    for caps in SCRIPT_RE.captures_iter(&scan) {
        let whole = caps.get(0).expect("group 0 always exists");
        // Group 1 is the open tag of a paired script; without it the whole
        // match is a self-closing <script ... />, which the playground honors
        // as the author's intent (a closed, empty tag).
        let (tag, body) = match caps.get(1) {
            Some(open) => (open.as_str(), caps.get(2).map_or("", |m| m.as_str())),
            None => (whole.as_str(), ""),
        };
        let tag = tag_inner(tag);
        let kind = html_attr(tag, "type");
        let src = html_attr(tag, "src");
        // The html spec matches the type value ASCII case-insensitively.
        if !kind.is_some_and(|k| k.eq_ignore_ascii_case("module")) {
            continue;
        }
        saw_module_script = true;
        let Some(src) = src else {
            if !body.trim().is_empty() {
                errors.push(BuildError {
                    path: "/index.html".to_string(),
                    message: "inline module scripts are not supported in the wasm playground yet; move the code to a file and reference it with src".to_string(),
                });
            }
            continue;
        };
        if is_external(src) {
            continue;
        }
        match resolve(files, "/", src) {
            Some(path) => {
                queue.push_back(path.clone());
                edits.push((
                    whole.range(),
                    format!(
                        "<script type=\"module\">import {};</script>",
                        serde_json::Value::String(module_id(&path))
                    ),
                ));
            }
            None => {
                errors.push(BuildError {
                    path: "/index.html".to_string(),
                    message: format!("script src {src} does not match any file"),
                });
            }
        }
    }
    if !saw_module_script {
        errors.push(BuildError {
            path: "/index.html".to_string(),
            message: "no <script type=\"module\" src=...> entry found in /index.html".to_string(),
        });
    }
    for m in LINK_RE.find_iter(&scan) {
        let tag = tag_inner(m.as_str());
        let (Some(rel), Some(href)) = (html_attr(tag, "rel"), html_attr(tag, "href")) else {
            continue;
        };
        if !rel.eq_ignore_ascii_case("stylesheet") || is_external(href) {
            continue;
        }
        let Some(path) = resolve(files, "/", href) else {
            errors.push(BuildError {
                path: "/index.html".to_string(),
                message: format!("stylesheet href {href} does not match any file"),
            });
            continue;
        };
        // A linked css module would inline scoped selectors with the class map
        // dropped on the floor: nothing in the page could reference them.
        if oj_css::is_css_module(&path) {
            errors.push(BuildError {
                path,
                message: "a css module cannot be linked from index.html; import it from a JS module to get the class map".to_string(),
            });
            continue;
        }
        match compile_stylesheet(&path, &files[&path]) {
            Ok((css, _)) => edits.push((
                m.range(),
                format!(
                    "<style data-oj-id={}>{}</style>",
                    serde_json::Value::String(module_id(&path)),
                    escape_style_text(&css),
                ),
            )),
            Err(message) => errors.push(BuildError { path, message }),
        }
    }
    edits.sort_by_key(|(range, _)| range.start);
    let out_html = splice(html, edits);

    let mut modules: Vec<Module> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    while let Some(path) = queue.pop_front() {
        if !seen.insert(path.clone()) {
            continue;
        }
        let source = &files[&path];
        let ext = ext_of(&path);

        if ext == "css" || is_sass(&path) {
            match compile_stylesheet(&path, source) {
                Ok((css, exports)) => modules.push(Module {
                    id: module_id(&path),
                    code: css_to_js(&module_id(&path), &css, exports.as_deref()),
                }),
                Err(message) => errors.push(BuildError { path, message }),
            }
            continue;
        }

        if ext == "json" {
            // The same module the native server serves: named exports for safe
            // identifier keys, JSON.parse fallback for the rest (__proto__
            // included), invalid json as an error with the file's path.
            match oj_compiler::json::to_esm(source, &path) {
                Ok(code) => modules.push(Module { id: module_id(&path), code }),
                Err(err) => errors.push(BuildError {
                    path: path.clone(),
                    message: err.to_string(),
                }),
            }
            continue;
        }

        if !JS_EXTS.contains(&ext) {
            errors.push(BuildError {
                path: path.clone(),
                message: format!("unsupported file type .{ext} in the wasm playground"),
            });
            continue;
        }

        let opts = CompileOptions {
            dev: true,
            refresh: false,
            sourcemap: true,
            ssr: false,
            jsx: Default::default(),
        };
        // import.meta.glob expands over the real filesystem inside the
        // compiler, which is empty on wasm32: it would silently become `({})`.
        // glob_patterns visits the AST, so a comment or string that merely
        // mentions the name does not trip the guard.
        if !oj_compiler::glob::glob_patterns(source, Path::new(&path)).is_empty() {
            errors.push(BuildError {
                path: path.clone(),
                message: "import.meta.glob is not supported in the wasm playground yet".to_string(),
            });
            continue;
        }

        let dir = dir_of(&path).to_string();
        let mut problems: Vec<String> = Vec::new();
        let mut deps: Vec<String> = Vec::new();
        let mut rewrite = |spec: &str| -> Option<String> {
            if is_external(spec) {
                return None;
            }
            // ?raw / ?url / ?inline / ?worker: the native server serves these
            // conventions; silently stripping the query (or minting a bare
            // esm.sh url with a second `?`) would hand back the wrong value,
            // so refuse loudly BEFORE the bare check.
            if let Some((_, query)) = spec.split_once('?') {
                problems.push(format!(
                    "import \"{spec}\": ?{query} imports are not supported in the wasm playground yet"
                ));
                return None;
            }
            // Pseudo-bare specifiers that can never be a package: the `@/`
            // alias convention and the playground's own module prefix.
            // Node subpath imports (package.json `imports`) need a package
            // manifest the playground does not have; classified bare they
            // would mint an esm.sh url whose `#...` is a fragment.
            if spec.starts_with('#') {
                problems.push(format!(
                    "import \"{spec}\": subpath imports (#...) are not supported in the playground; use a relative path"
                ));
                return None;
            }
            if spec.starts_with("@/") {
                problems.push(format!(
                    "import \"{spec}\": alias imports are not configured in the playground; use a relative path"
                ));
                return None;
            }
            if spec.starts_with("@app/") {
                problems.push(format!(
                    "import \"{spec}\": {MODULE_PREFIX}/ is the playground's internal prefix; import the file with a relative path"
                ));
                return None;
            }
            if is_bare(spec) {
                bare.insert(spec.to_string());
                return None;
            }
            match resolve(files, &dir, spec) {
                Some(target) => {
                    deps.push(target.clone());
                    Some(module_id(&target))
                }
                None => {
                    problems.push(format!("import \"{spec}\" does not match any file"));
                    None
                }
            }
        };
        match compile_module(Path::new(&path), source, &opts, Some(&mut rewrite)) {
            Ok(out) => {
                queue.extend(deps);
                for message in problems {
                    errors.push(BuildError { path: path.clone(), message });
                }
                modules.push(Module {
                    id: module_id(&path),
                    code: out.code_with_inline_map(),
                });
            }
            Err(err) => {
                errors.push(BuildError { path: path.clone(), message: err.to_string() });
            }
        }
    }

    BuildResult {
        ok: errors.is_empty(),
        html: out_html,
        modules,
        bare: bare.into_iter().collect(),
        errors,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> BTreeMap<String, String> {
        let mut files = BTreeMap::new();
        files.insert(
            "/index.html".to_string(),
            "<!doctype html><html><head><link rel=\"stylesheet\" href=\"/src/global.css\" /></head>\
             <body><div id=\"root\"></div><script type=\"module\" src=\"/src/main.tsx\"></script></body></html>"
                .to_string(),
        );
        files.insert(
            "/src/main.tsx".to_string(),
            "import { createRoot } from \"react-dom/client\";\nimport App from \"./App\";\nimport \"./style.css\";\n\
             createRoot(document.getElementById(\"root\")!).render(<App />);\n"
                .to_string(),
        );
        files.insert(
            "/src/App.tsx".to_string(),
            "import styles from \"./App.module.css\";\nexport default function App() {\n  return <h1 className={styles.title}>hi</h1>;\n}\n"
                .to_string(),
        );
        files.insert("/src/style.css".to_string(), "body { margin: 0; }".to_string());
        files.insert("/src/App.module.css".to_string(), ".title { color: rebeccapurple; }".to_string());
        files.insert("/src/global.css".to_string(), ":root { --x: 1; }".to_string());
        files
    }

    #[test]
    fn builds_the_demo_graph() {
        let result = build(&demo());
        assert!(result.ok, "errors: {:?}", result.errors);
        let ids: Vec<&str> = result.modules.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"@app/src/main.tsx"));
        assert!(ids.contains(&"@app/src/App.tsx"));
        assert!(ids.contains(&"@app/src/style.css"));
        assert!(ids.contains(&"@app/src/App.module.css"));
        assert!(!ids.contains(&"@app/src/global.css"), "linked css is inlined, not a module");
    }

    #[test]
    fn rewrites_imports_and_collects_bare() {
        let result = build(&demo());
        let main = result.modules.iter().find(|m| m.id == "@app/src/main.tsx").unwrap();
        assert!(main.code.contains("\"@app/src/App.tsx\""), "{}", main.code);
        assert!(main.code.contains("\"@app/src/style.css\""));
        assert!(result.bare.contains(&"react-dom/client".to_string()));
        assert!(result.bare.iter().any(|b| b.starts_with("react/jsx")), "{:?}", result.bare);
    }

    #[test]
    fn html_entry_becomes_inline_import_and_link_is_inlined() {
        let result = build(&demo());
        assert!(result.html.contains("import \"@app/src/main.tsx\";"), "{}", result.html);
        assert!(!result.html.contains("src=\"/src/main.tsx\""));
        assert!(result.html.contains("--x: 1"), "{}", result.html);
        assert!(!result.html.contains("<link"));
    }

    #[test]
    fn css_modules_export_scoped_names() {
        let result = build(&demo());
        let module = result.modules.iter().find(|m| m.id == "@app/src/App.module.css").unwrap();
        assert!(module.code.contains("export const title = "), "{}", module.code);
        assert!(module.code.contains("export default"));
    }

    #[test]
    fn missing_import_is_reported_not_fatal() {
        let mut files = demo();
        files.insert("/src/main.tsx".to_string(), "import \"./nope\";\nconsole.log(1);\n".to_string());
        let result = build(&files);
        assert!(!result.ok);
        assert!(result.errors.iter().any(|e| e.message.contains("./nope")));
        assert!(result.modules.iter().any(|m| m.id == "@app/src/main.tsx"));
    }

    #[test]
    fn html_attributes_match_in_any_order_and_quote_style() {
        let mut files = demo();
        files.insert(
            "/index.html".to_string(),
            "<html><head><link href='/src/global.css' rel='stylesheet'></head>\
             <body><div id=\"root\"></div><script src='/src/main.tsx' type='module'></script></body></html>"
                .to_string(),
        );
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
        assert!(result.html.contains("import \"@app/src/main.tsx\";"), "{}", result.html);
        assert!(result.html.contains("--x: 1"));
    }

    #[test]
    fn non_module_scripts_and_non_stylesheet_links_are_kept() {
        let mut files = demo();
        files.insert(
            "/index.html".to_string(),
            "<html><head><link rel=\"icon\" href=\"/favicon.png\" /></head>\
             <body><script src=\"/legacy.js\"></script><script type=\"module\" src=\"/src/main.tsx\"></script></body></html>"
                .to_string(),
        );
        let result = build(&files);
        assert!(result.html.contains("<link rel=\"icon\""), "{}", result.html);
        assert!(result.html.contains("<script src=\"/legacy.js\"></script>"), "{}", result.html);
        assert!(result.html.contains("import \"@app/src/main.tsx\";"));
    }

    #[test]
    fn css_at_import_is_a_loud_error() {
        let mut files = demo();
        files.insert("/src/style.css".to_string(), "@import \"./global.css\";\nbody { margin: 0 }".to_string());
        let result = build(&files);
        assert!(!result.ok);
        assert!(result.errors.iter().any(|e| e.message.contains("@import")), "{:?}", result.errors);
    }

    #[test]
    fn url_imports_stay_untouched_and_out_of_bare() {
        let mut files = demo();
        files.insert(
            "/src/main.tsx".to_string(),
            "import x from \"https://esm.sh/lodash-es\";\nconsole.log(x);\n".to_string(),
        );
        let result = build(&files);
        let main = result.modules.iter().find(|m| m.id == "@app/src/main.tsx").unwrap();
        assert!(main.code.contains("\"https://esm.sh/lodash-es\""), "{}", main.code);
        assert!(!result.bare.iter().any(|b| b.contains("https://")), "{:?}", result.bare);
    }

    #[test]
    fn json_proto_keys_go_through_json_parse_and_bad_json_errors() {
        let mut files = demo();
        files.insert("/src/main.tsx".to_string(), "import d from \"./data.json\";\nconsole.log(d);\n".to_string());
        files.insert("/src/data.json".to_string(), "{\"__proto__\": {\"a\": 1}}".to_string());
        let result = build(&files);
        let json = result.modules.iter().find(|m| m.id == "@app/src/data.json").unwrap();
        assert!(json.code.contains("JSON.parse("), "{}", json.code);
        assert!(json.code.contains("export default"), "{}", json.code);

        files.insert("/src/data.json".to_string(), "{oops}".to_string());
        let result = build(&files);
        assert!(!result.ok);
        assert!(result.errors.iter().any(|e| e.path == "/src/data.json"), "{:?}", result.errors);
    }

    #[test]
    fn inlined_css_cannot_close_the_style_tag() {
        let mut files = demo();
        files.insert("/src/global.css".to_string(), ".x::after { content: \"</StYlE>\" }".to_string());
        let result = build(&files);
        assert!(result.ok, "{:?}", result.errors);
        assert!(!result.html.to_lowercase().contains("content: \"</style"), "{}", result.html);
        assert!(result.html.contains("<\\/StYlE>"), "{}", result.html);
    }

    #[test]
    fn no_module_entry_is_an_error() {
        let mut files = demo();
        files.insert("/index.html".to_string(), "<html><body><p>static</p></body></html>".to_string());
        let result = build(&files);
        assert!(!result.ok);
        assert!(result.errors.iter().any(|e| e.message.contains("no <script")), "{:?}", result.errors);
    }

    #[test]
    fn data_attributes_do_not_shadow_real_ones() {
        let mut files = demo();
        files.insert(
            "/index.html".to_string(),
            "<html><body><script data-type=\"module\" src=\"/legacy.js\"></script>\
             <script type=\"module\" data-src=\"/nope.js\" src=\"/src/main.tsx\"></script></body></html>"
                .to_string(),
        );
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
        assert!(result.html.contains("data-type=\"module\" src=\"/legacy.js\""), "{}", result.html);
        assert!(result.html.contains("import \"@app/src/main.tsx\";"), "{}", result.html);
    }

    #[test]
    fn uppercase_tags_and_spaced_close_match() {
        let mut files = demo();
        files.insert(
            "/index.html".to_string(),
            "<html><head><LINK REL=\"stylesheet\" HREF=\"/src/global.css\"></head>\
             <body><SCRIPT TYPE=\"module\" SRC=\"/src/main.tsx\"></SCRIPT ></body></html>"
                .to_string(),
        );
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
        assert!(result.html.contains("import \"@app/src/main.tsx\";"), "{}", result.html);
        assert!(result.html.contains("--x: 1"), "{}", result.html);
    }

    #[test]
    fn at_import_in_a_string_is_not_an_error() {
        let mut files = demo();
        files.insert(
            "/src/style.css".to_string(),
            ".hint::after { content: \"use @import here\" }".to_string(),
        );
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
    }

    #[test]
    fn sass_use_is_a_loud_error() {
        let mut files = demo();
        files.insert("/src/main.tsx".to_string(), "import \"./app.scss\";\nconsole.log(1);\n".to_string());
        files.insert("/src/app.scss".to_string(), "@use \"./vars\";\nbody { color: $ink }".to_string());
        let result = build(&files);
        assert!(!result.ok);
        assert!(result.errors.iter().any(|e| e.message.contains("sass @use/@import")), "{:?}", result.errors);
    }

    #[test]
    fn query_imports_are_a_loud_error() {
        let mut files = demo();
        files.insert(
            "/src/main.tsx".to_string(),
            "import text from \"./style.css?raw\";\nconsole.log(text);\n".to_string(),
        );
        let result = build(&files);
        assert!(!result.ok);
        assert!(
            result.errors.iter().any(|e| e.message.contains("?raw imports are not supported")),
            "{:?}",
            result.errors
        );
    }

    #[test]
    fn probe_order_matches_vite_js_before_ts() {
        let mut files = demo();
        files.insert("/src/main.tsx".to_string(), "import \"./dual\";\n".to_string());
        files.insert("/src/dual.js".to_string(), "console.log(\"js\");\n".to_string());
        files.insert("/src/dual.ts".to_string(), "console.log(\"ts\");\n".to_string());
        let result = build(&files);
        let ids: Vec<&str> = result.modules.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"@app/src/dual.js"), "{ids:?}");
        assert!(!ids.contains(&"@app/src/dual.ts"), "{ids:?}");
    }

    #[test]
    fn external_urls_in_html_pass_through_without_errors() {
        let mut files = demo();
        files.insert(
            "/index.html".to_string(),
            "<html><head><link rel=\"stylesheet\" href=\"https://fonts.googleapis.com/css2?family=Inter\">\
             <script type=\"module\" src=\"//cdn.example/x.js\"></script></head>\
             <body><script type=\"module\" src=\"/src/main.tsx\"></script></body></html>"
                .to_string(),
        );
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
        assert!(result.html.contains("fonts.googleapis.com"), "{}", result.html);
        assert!(result.html.contains("//cdn.example/x.js"), "{}", result.html);
    }

    #[test]
    fn commented_out_tags_are_ignored() {
        let mut files = demo();
        files.insert(
            "/index.html".to_string(),
            "<html><body><!-- <script type=\"module\" src=\"/src/broken.tsx\"></script> -->\
             <script type=\"module\" src=\"/src/main.tsx\"></script></body></html>"
                .to_string(),
        );
        files.insert("/src/broken.tsx".to_string(), "not!valid syntax(((".to_string());
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
        assert!(!result.modules.iter().any(|m| m.id.contains("broken")));
    }

    #[test]
    fn inline_module_scripts_are_a_loud_error() {
        let mut files = demo();
        files.insert(
            "/index.html".to_string(),
            "<html><body><script type=\"module\">import \"/src/main.tsx\";</script></body></html>".to_string(),
        );
        let result = build(&files);
        assert!(!result.ok);
        assert!(result.errors.iter().any(|e| e.message.contains("inline module scripts")), "{:?}", result.errors);
    }

    #[test]
    fn type_and_rel_values_match_case_insensitively() {
        let mut files = demo();
        files.insert(
            "/index.html".to_string(),
            "<html><head><link rel=\"Stylesheet\" href=\"/src/global.css\"></head>\
             <body><script type=\"Module\" src=\"/src/main.tsx\"></script></body></html>"
                .to_string(),
        );
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
        assert!(result.html.contains("import \"@app/src/main.tsx\";"));
        assert!(result.html.contains("--x: 1"));
    }

    #[test]
    fn json_named_exports_match_the_native_module() {
        let mut files = demo();
        files.insert("/src/main.tsx".to_string(), "import { version } from \"./pkg.json\";\nconsole.log(version);\n".to_string());
        files.insert("/src/pkg.json".to_string(), "{\"version\": \"1.0.0\"}".to_string());
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
        let json = result.modules.iter().find(|m| m.id == "@app/src/pkg.json").unwrap();
        assert!(json.code.contains("export const version"), "{}", json.code);
    }

    #[test]
    fn extensionless_json_and_nodenext_js_to_ts_resolve() {
        let mut files = demo();
        files.insert(
            "/src/main.tsx".to_string(),
            "import config from \"./config\";\nimport { x } from \"./mod.js\";\nconsole.log(config, x);\n".to_string(),
        );
        files.insert("/src/config.json".to_string(), "{\"a\": 1}".to_string());
        files.insert("/src/mod.ts".to_string(), "export const x = 1;\n".to_string());
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
        let ids: Vec<&str> = result.modules.iter().map(|m| m.id.as_str()).collect();
        assert!(ids.contains(&"@app/src/config.json"), "{ids:?}");
        assert!(ids.contains(&"@app/src/mod.ts"), "{ids:?}");
    }

    #[test]
    fn bare_specifiers_with_queries_and_pseudo_bare_aliases_error_loudly() {
        let mut files = demo();
        files.insert(
            "/src/main.tsx".to_string(),
            "import \"swiper/css?inline\";\nimport \"@/components/ui/button\";\nimport \"@app/src/App.tsx\";\nconsole.log(1);\n"
                .to_string(),
        );
        let result = build(&files);
        assert!(!result.ok);
        assert!(result.errors.iter().any(|e| e.message.contains("?inline imports are not supported")), "{:?}", result.errors);
        assert!(result.errors.iter().any(|e| e.message.contains("alias imports are not configured")), "{:?}", result.errors);
        assert!(result.errors.iter().any(|e| e.message.contains("internal prefix")), "{:?}", result.errors);
        assert!(result.bare.is_empty(), "{:?}", result.bare);
    }

    #[test]
    fn import_meta_glob_is_a_loud_error() {
        let mut files = demo();
        files.insert(
            "/src/main.tsx".to_string(),
            "const pages = import.meta.glob(\"./*.tsx\");\nconsole.log(pages);\n".to_string(),
        );
        let result = build(&files);
        assert!(!result.ok);
        assert!(result.errors.iter().any(|e| e.message.contains("import.meta.glob")), "{:?}", result.errors);
    }

    #[test]
    fn composes_from_another_file_is_a_loud_error_and_commented_sass_use_is_fine() {
        let mut files = demo();
        files.insert(
            "/src/App.module.css".to_string(),
            ".title { composes: base from \"./base.module.css\"; color: red }".to_string(),
        );
        let result = build(&files);
        assert!(!result.ok);
        assert!(result.errors.iter().any(|e| e.message.contains("composes")), "{:?}", result.errors);

        let mut files = demo();
        files.insert("/src/main.tsx".to_string(), "import \"./app.scss\";\nconsole.log(1);\n".to_string());
        files.insert(
            "/src/app.scss".to_string(),
            "/*\n@use \"./vars\";\n*/\n// @import \"./other\";\nbody { color: teal }".to_string(),
        );
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
    }

    #[test]
    fn external_only_module_entry_is_not_an_error() {
        let mut files = demo();
        files.insert(
            "/index.html".to_string(),
            "<html><body><script type=\"module\" src=\"https://esm.sh/my-app\"></script></body></html>".to_string(),
        );
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
        assert!(result.html.contains("https://esm.sh/my-app"));
        assert!(result.modules.is_empty());
    }

    #[test]
    fn self_closing_module_script_is_an_entry_and_swallows_nothing() {
        let mut files = demo();
        files.insert(
            "/index.html".to_string(),
            "<html><body><script type=\"module\" src=\"/src/main.tsx\" />\n<div id=\"root\"></div>\n\
             <script src=\"/legacy.js\"></script></body></html>"
                .to_string(),
        );
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
        assert!(result.html.contains("import \"@app/src/main.tsx\";"), "{}", result.html);
        assert!(result.html.contains("<div id=\"root\">"), "{}", result.html);
        assert!(result.html.contains("<script src=\"/legacy.js\"></script>"), "{}", result.html);
    }

    #[test]
    fn comments_survive_into_the_delivered_html() {
        let mut files = demo();
        files.insert(
            "/index.html".to_string(),
            "<html><body><!-- keep me --><script type=\"module\" src=\"/src/main.tsx\"></script></body></html>"
                .to_string(),
        );
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
        assert!(result.html.contains("<!-- keep me -->"), "{}", result.html);
    }

    #[test]
    fn subpath_imports_error_loudly() {
        let mut files = demo();
        files.insert("/src/main.tsx".to_string(), "import { fmt } from \"#utils\";\nconsole.log(fmt);\n".to_string());
        let result = build(&files);
        assert!(!result.ok);
        assert!(result.errors.iter().any(|e| e.message.contains("subpath imports")), "{:?}", result.errors);
        assert!(result.bare.is_empty(), "{:?}", result.bare);
    }

    #[test]
    fn linked_css_module_is_a_loud_error() {
        let mut files = demo();
        files.insert(
            "/index.html".to_string(),
            "<html><head><link rel=\"stylesheet\" href=\"/src/App.module.css\"></head>\
             <body><script type=\"module\" src=\"/src/main.tsx\"></script></body></html>"
                .to_string(),
        );
        let result = build(&files);
        assert!(!result.ok);
        assert!(result.errors.iter().any(|e| e.message.contains("css module cannot be linked")), "{:?}", result.errors);
    }

    #[test]
    fn mentioning_import_meta_glob_in_a_comment_is_fine() {
        let mut files = demo();
        files.insert(
            "/src/main.tsx".to_string(),
            "// unlike import.meta.glob in Vite, this is fine\nconsole.log(\"import.meta.glob\");\n".to_string(),
        );
        let result = build(&files);
        assert!(result.ok, "errors: {:?}", result.errors);
    }

    #[test]
    fn missing_index_html_fails() {
        let result = build(&BTreeMap::new());
        assert!(!result.ok);
        assert_eq!(result.errors[0].path, "/index.html");
    }

    #[test]
    fn normalizes_dots() {
        assert_eq!(normalize_abs("/src/../a/./b"), "/a/b");
        assert_eq!(normalize_abs("src/a"), "/src/a");
    }
}
