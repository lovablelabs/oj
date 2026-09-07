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

use oj_compiler::{compile_module, CompileOptions};
use oj_css::{compile_css, compile_sass, css_modules_esm, is_sass};
use regex::Regex;
use serde::Serialize;

/// Prefix that turns an in-memory absolute path into a bare import-map
/// specifier: `/src/App.tsx` is served as `@app/src/App.tsx`.
pub const MODULE_PREFIX: &str = "@app";

const JS_EXTS: &[&str] = &["tsx", "ts", "jsx", "js", "mjs"];
const PROBE_EXTS: &[&str] = &[".tsx", ".ts", ".jsx", ".js", ".mjs"];

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
/// script extensions and directory indexes the way the native resolver does.
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
    for ext in PROBE_EXTS {
        let probe = format!("{joined}{ext}");
        if files.contains_key(&probe) {
            return Some(probe);
        }
    }
    for ext in PROBE_EXTS {
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

static SCRIPT_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<script\b[^>]*>\s*</script>").unwrap());
static LINK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"<link\b[^>]*/?>").unwrap());
static TYPE_RE: LazyLock<Regex> = LazyLock::new(|| attr_re("type"));
static SRC_RE: LazyLock<Regex> = LazyLock::new(|| attr_re("src"));
static REL_RE: LazyLock<Regex> = LazyLock::new(|| attr_re("rel"));
static HREF_RE: LazyLock<Regex> = LazyLock::new(|| attr_re("href"));
static STYLE_CLOSE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)</(style)").unwrap());

fn attr_re(name: &str) -> Regex {
    Regex::new(&format!(r#"\b{name}\s*=\s*(?:["']([^"']*)["']|([^\s>"']+))"#)).unwrap()
}

/// The value of one html attribute in `tag`: quoted (either style) or bare.
fn attr(tag: &str, name: &str) -> Option<String> {
    let re: &Regex = match name {
        "type" => &TYPE_RE,
        "src" => &SRC_RE,
        "rel" => &REL_RE,
        "href" => &HREF_RE,
        _ => unreachable!("attr() is only called for the four known names"),
    };
    re.captures(tag)
        .and_then(|c| c.get(1).or_else(|| c.get(2)))
        .map(|m| m.as_str().to_string())
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
    let plain = if is_sass(path) { compile_sass(source, None)? } else { source.to_string() };
    // Module scoping keys off the url (`.module.` in the filename).
    let out = compile_css(&id, &plain, false)?;
    // Without the dev server's rebase pass an `@import` survives verbatim and
    // would resolve against the preview document, silently loading nothing;
    // fail loudly instead until the graph walks css imports too.
    if out.css.contains("@import") {
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

    // <script type="module" src="..."> becomes an inline import of the module
    // specifier (a script src never goes through the import map; an inline
    // `import` does), and <link rel="stylesheet" href="..."> is inlined. Tags
    // are matched whole and their attributes read separately, so attribute
    // order and quote style (double, single, none) don't matter.
    let out_html = SCRIPT_RE.replace_all(html, |caps: &regex::Captures| {
        let tag = &caps[0];
        let (Some(kind), Some(src)) = (attr(tag, "type"), attr(tag, "src")) else {
            return caps[0].to_string();
        };
        if kind != "module" {
            return caps[0].to_string();
        }
        match resolve(files, "/", &src) {
            Some(path) => {
                queue.push_back(path.clone());
                format!(
                    "<script type=\"module\">import {};</script>",
                    serde_json::Value::String(module_id(&path))
                )
            }
            None => {
                errors.push(BuildError {
                    path: "/index.html".to_string(),
                    message: format!("script src {src} does not match any file"),
                });
                caps[0].to_string()
            }
        }
    });
    if queue.is_empty() {
        errors.push(BuildError {
            path: "/index.html".to_string(),
            message: "no <script type=\"module\" src=...> entry found in /index.html".to_string(),
        });
    }
    let out_html = LINK_RE
        .replace_all(&out_html, |caps: &regex::Captures| {
            let tag = &caps[0];
            let (Some(rel), Some(href)) = (attr(tag, "rel"), attr(tag, "href")) else {
                return caps[0].to_string();
            };
            if rel != "stylesheet" {
                return caps[0].to_string();
            }
            let Some(path) = resolve(files, "/", &href) else {
                errors.push(BuildError {
                    path: "/index.html".to_string(),
                    message: format!("stylesheet href {href} does not match any file"),
                });
                return caps[0].to_string();
            };
            match compile_stylesheet(&path, &files[&path]) {
                Ok((css, _)) => format!(
                    "<style data-oj-id={}>{}</style>",
                    serde_json::Value::String(module_id(&path)),
                    escape_style_text(&css),
                ),
                Err(message) => {
                    errors.push(BuildError { path, message });
                    caps[0].to_string()
                }
            }
        })
        .into_owned();

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
            // Validate here so a bad edit surfaces in the error strip with its
            // path, and ship the text through JSON.parse: interpolating raw
            // JSON as an expression would let `__proto__` keys set the
            // prototype instead of a property.
            match serde_json::from_str::<serde_json::Value>(source) {
                Ok(_) => modules.push(Module {
                    id: module_id(&path),
                    code: format!(
                        "export default JSON.parse({});\n",
                        serde_json::Value::String(source.to_string())
                    ),
                }),
                Err(err) => errors.push(BuildError {
                    path: path.clone(),
                    message: format!("invalid json: {err}"),
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
        let dir = dir_of(&path).to_string();
        let mut missing: Vec<String> = Vec::new();
        let mut deps: Vec<String> = Vec::new();
        let mut rewrite = |spec: &str| -> Option<String> {
            if has_scheme(spec) {
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
                    missing.push(spec.to_string());
                    None
                }
            }
        };
        match compile_module(Path::new(&path), source, &opts, Some(&mut rewrite)) {
            Ok(out) => {
                drop(rewrite);
                queue.extend(deps);
                for spec in missing {
                    errors.push(BuildError {
                        path: path.clone(),
                        message: format!("import \"{spec}\" does not match any file"),
                    });
                }
                modules.push(Module {
                    id: module_id(&path),
                    code: out.code_with_inline_map(),
                });
            }
            Err(err) => {
                drop(rewrite);
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
    fn json_ships_through_json_parse_and_bad_json_errors() {
        let mut files = demo();
        files.insert("/src/main.tsx".to_string(), "import d from \"./data.json\";\nconsole.log(d);\n".to_string());
        files.insert("/src/data.json".to_string(), "{\"__proto__\": {\"a\": 1}}".to_string());
        let result = build(&files);
        let json = result.modules.iter().find(|m| m.id == "@app/src/data.json").unwrap();
        assert!(json.code.starts_with("export default JSON.parse("), "{}", json.code);

        files.insert("/src/data.json".to_string(), "{oops}".to_string());
        let result = build(&files);
        assert!(!result.ok);
        assert!(result.errors.iter().any(|e| e.path == "/src/data.json" && e.message.contains("invalid json")));
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
