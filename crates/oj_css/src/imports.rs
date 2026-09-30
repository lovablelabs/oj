//! `@import` inlining and resolution, as Vite's postcss-import step does it.

use std::path::{Path, PathBuf};

use crate::path::{is_external, node_modules_load_paths, read_package_json, strip_query, with_ext};
use crate::rebase::rebase_to_dir;
use crate::resolve::CssResolve;

const IMPORT: &str = "@import";

/// Inline plain `@import`s so the stylesheet carries the imported rules.
/// Relative imports resolve against the importer, bare ones through
/// `node_modules` (`style` / `main` entry). `layer(...)`, `supports(...)` and
/// external imports stay as written; inlined files' urls are rebased to
/// `file`'s directory.
pub fn inline_imports(source: &str, file: &Path) -> Result<String, String> {
    inline_imports_with(source, file, &CssResolve::default())
}

/// `inline_imports` with aliases and root-absolute specifiers.
pub fn inline_imports_with(
    source: &str,
    file: &Path,
    resolve: &CssResolve<'_>,
) -> Result<String, String> {
    inline_imports_collecting(source, file, resolve, &mut Vec::new())
}

/// `inline_imports_with`, appending every inlined file to `deps` (Vite's HMR
/// dependencies for the sheet).
pub fn inline_imports_collecting(
    source: &str,
    file: &Path,
    resolve: &CssResolve<'_>,
    deps: &mut Vec<PathBuf>,
) -> Result<String, String> {
    let mut inliner = Inliner {
        stack: vec![file.to_path_buf()],
        resolve,
        deps,
    };
    let out = inliner.inline(source, file)?;
    Ok(if out.contains(IMPORT) {
        hoist_imports(&out)
    } else {
        out
    })
}

const MAX_DEPTH: usize = 32;

struct Inliner<'a, 'r> {
    /// Files being inlined, for cycle detection.
    stack: Vec<PathBuf>,
    resolve: &'a CssResolve<'r>,
    deps: &'a mut Vec<PathBuf>,
}

impl Inliner<'_, '_> {
    fn inline(&mut self, source: &str, file: &Path) -> Result<String, String> {
        if !source.contains(IMPORT) || self.stack.len() > MAX_DEPTH {
            return Ok(source.to_string());
        }
        let dir = file.parent().unwrap_or(Path::new("."));
        let mut out = String::with_capacity(source.len());
        let mut rest = source;
        while let Some(pos) = rest.find(IMPORT) {
            let (before, at) = rest.split_at(pos);
            out.push_str(before);
            let import = at_statement_start(before)
                .then(|| PlainImport::parse(&at[IMPORT.len()..]))
                .flatten();
            let Some(import) = import else {
                out.push_str(IMPORT);
                rest = &at[IMPORT.len()..];
                continue;
            };
            let statement = &at[..IMPORT.len() + import.len];
            rest = &at[statement.len()..];
            let target = resolve_css_import_with(&import.spec, dir, self.resolve)
                .filter(|t| !self.stack.contains(t));
            let Some(target) = target else {
                out.push_str(statement);
                continue;
            };
            let child = std::fs::read_to_string(&target).map_err(|e| {
                format!(
                    "cannot read @import {} ({}): {e}",
                    import.spec,
                    target.display()
                )
            })?;
            self.deps.push(target.clone());
            self.stack.push(target.clone());
            let child = self.inline(&child, &target)?;
            self.stack.pop();
            let child_dir = target.parent().unwrap_or(Path::new("."));
            let rebased = rebase_to_dir(&child, &target, child_dir, dir, self.resolve)?;
            match import.media {
                // `@import "x" print;` becomes `@media print { ... }`.
                Some(media) => {
                    out.push_str(&format!("@media {media} {{\n{rebased}\n}}\n"));
                }
                None => {
                    out.push_str(&rebased);
                    out.push('\n');
                }
            }
        }
        out.push_str(rest);
        Ok(out)
    }
}

/// Whether an `@import` after `before` starts a statement (and is not inside
/// a string or a rule).
fn at_statement_start(before: &str) -> bool {
    before
        .trim_end()
        .chars()
        .next_back()
        .is_none_or(|c| matches!(c, ';' | '}' | '{' | '/'))
}

/// Move every `@import` left after inlining to the top (after `@charset`), as
/// CSS requires imports to precede all other rules.
fn hoist_imports(css: &str) -> String {
    let mut imports: Vec<&str> = Vec::new();
    let mut body = String::with_capacity(css.len());
    let mut rest = css;
    while let Some(pos) = rest.find(IMPORT) {
        let (before, at) = rest.split_at(pos);
        body.push_str(before);
        match at_statement_start(before).then(|| at.find(';')).flatten() {
            Some(end) => {
                imports.push(&at[..=end]);
                rest = &at[end + 1..];
            }
            None => {
                body.push_str(IMPORT);
                rest = &at[IMPORT.len()..];
            }
        }
    }
    body.push_str(rest);
    if imports.is_empty() {
        return body;
    }
    let mut out = String::with_capacity(css.len());
    let mut body_rest = body.as_str();
    if let Some(after) = body_rest.trim_start().strip_prefix("@charset") {
        if let Some(end) = after.find(';') {
            let stmt_end = body_rest.len() - after.len() + end + 1;
            out.push_str(&body_rest[..stmt_end]);
            out.push('\n');
            body_rest = &body_rest[stmt_end..];
        }
    }
    for imp in imports {
        out.push_str(imp.trim_start());
        out.push('\n');
    }
    out.push_str(body_rest);
    out
}

/// An inlinable `@import`: `"x"`, `'x'`, `url(x)` or `url("x")`, optionally
/// with a media query.
#[derive(Debug, PartialEq, Eq)]
struct PlainImport {
    spec: String,
    /// Bytes after `@import` through the closing `;`.
    len: usize,
    media: Option<String>,
}

impl PlainImport {
    /// None for `layer(...)` / `supports(...)` imports and external urls,
    /// which stay for the browser.
    fn parse(after: &str) -> Option<PlainImport> {
        let trimmed = after.trim_start();
        let ws = after.len() - trimmed.len();
        let (spec, used) = if let Some(inner) = trimmed.strip_prefix("url(") {
            let close = inner.find(')')?;
            let raw = inner[..close].trim().trim_matches(['"', '\'']);
            (raw, "url(".len() + close + 1)
        } else {
            let quote = trimmed.chars().next().filter(|&q| q == '"' || q == '\'')?;
            let close = trimmed[1..].find(quote)?;
            (&trimmed[1..1 + close], close + 2)
        };
        let tail = &trimmed[used..];
        let semi = tail.find(';')?;
        let cond = &tail[..semi];
        if cond.contains(['{', '}']) {
            return None;
        }
        let cond = cond.trim();
        if cond.contains("layer(") || cond.contains("supports(") || cond.starts_with("layer") {
            return None;
        }
        if spec.is_empty() || is_external(spec) {
            return None;
        }
        Some(PlainImport {
            spec: spec.to_string(),
            len: ws + used + semi + 1,
            media: (!cond.is_empty()).then(|| cond.to_string()),
        })
    }
}

/// postcss-import's order: relative to the importer, then a node_modules
/// package (its `style` / `main` entry, or a file inside it); `~` accepted.
pub fn resolve_css_import(spec: &str, dir: &Path) -> Option<PathBuf> {
    resolve_css_import_with(spec, dir, &CssResolve::default())
}

/// `resolve_css_import` after Vite's alias and root-absolute steps. A path
/// alias resolves there, a package alias continues as a bare specifier; `/x`
/// is the public file, else a file under the root, else a real absolute path.
pub fn resolve_css_import_with(
    spec: &str,
    dir: &Path,
    resolve: &CssResolve<'_>,
) -> Option<PathBuf> {
    let spec = strip_query(spec);
    let aliased = resolve.alias_spec(spec);
    let spec = match &aliased {
        Some(a) if Path::new(a).is_absolute() => return first_css_file(Path::new(a)),
        Some(a) => a.as_str(),
        None if spec.starts_with('/') => {
            return resolve
                .public_file(spec)
                .or_else(|| resolve.root_path(spec).and_then(|p| first_css_file(&p)))
                .or_else(|| first_css_file(Path::new(spec)));
        }
        None => spec,
    };
    let spec = spec.strip_prefix('~').unwrap_or(spec);
    if spec.is_empty() {
        return None;
    }
    if let Some(local) = first_css_file(&dir.join(spec)) {
        return Some(local);
    }
    if spec.starts_with("./") || spec.starts_with("../") {
        return None;
    }
    let (pkg, rest) = split_package_specifier(spec)?;
    for nm in node_modules_load_paths(dir) {
        let pkg_dir = nm.join(pkg);
        if !pkg_dir.is_dir() {
            continue;
        }
        if rest.is_empty() {
            return package_css_entry(&pkg_dir);
        }
        if let Some(file) = first_css_file(&pkg_dir.join(rest)) {
            return Some(file);
        }
    }
    None
}

/// A bare package import: the `style` / `main` entry when it is CSS, else
/// `index.css`.
fn package_css_entry(pkg_dir: &Path) -> Option<PathBuf> {
    let from_fields = read_package_json(pkg_dir).and_then(|json| {
        ["style", "main"]
            .iter()
            .filter_map(|field| json.get(field)?.as_str())
            .flat_map(|entry| css_file_candidates(&pkg_dir.join(entry)))
            .find(|c| c.is_file() && c.extension().is_some_and(|e| e == "css"))
    });
    from_fields.or_else(|| {
        let idx = pkg_dir.join("index.css");
        idx.is_file().then_some(idx)
    })
}

fn first_css_file(base: &Path) -> Option<PathBuf> {
    css_file_candidates(base).into_iter().find(|c| c.is_file())
}

/// `base`, `base.css` (when extensionless), `base/index.css`.
fn css_file_candidates(base: &Path) -> Vec<PathBuf> {
    let mut v = vec![base.to_path_buf()];
    if base.extension().is_none() {
        v.push(with_ext(base, "css"));
    }
    v.push(base.join("index.css"));
    v
}

/// `@scope/name/rest` -> (`@scope/name`, `rest`); `name/rest` -> (`name`, `rest`).
fn split_package_specifier(spec: &str) -> Option<(&str, &str)> {
    let name_end = if spec.starts_with('@') {
        let scope = spec.find('/')?;
        spec[scope + 1..]
            .find('/')
            .map_or(spec.len(), |i| scope + 1 + i)
    } else {
        spec.find('/').unwrap_or(spec.len())
    };
    let (pkg, rest) = spec.split_at(name_end);
    Some((pkg, rest.strip_prefix('/').unwrap_or(rest)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::relative_path;
    use crate::*;

    #[test]
    fn inlines_relative_and_package_imports_and_rebases_urls() {
        let base = std::env::temp_dir().join(format!("oj-css-imp-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("src/base")).unwrap();
        std::fs::create_dir_all(base.join("node_modules/normalize-fake")).unwrap();
        std::fs::write(base.join("src/vars.css"), ":root { --x: 1; }\n").unwrap();
        std::fs::write(
            base.join("src/base/reset.css"),
            "@import \"../vars.css\";\n.reset { background: url(./dot.png); }\n",
        )
        .unwrap();
        std::fs::write(
            base.join("node_modules/normalize-fake/package.json"),
            r#"{"name":"normalize-fake","style":"n.css"}"#,
        )
        .unwrap();
        std::fs::write(
            base.join("node_modules/normalize-fake/n.css"),
            ".norm { margin: 0; }\n",
        )
        .unwrap();
        std::fs::write(base.join("src/print.css"), ".print { display: none; }\n").unwrap();
        let app = base.join("src/app.css");
        let src = "@import \"./base/reset.css\";\n@import 'normalize-fake';\n@import url(https://cdn.test/x.css);\n@import \"./print.css\" print;\n@import \"./missing.css\" screen;\n.app { color: red; }\n";
        let out = inline_imports(src, &app).unwrap();
        assert!(out.contains("--x: 1"), "nested import inlined: {out}");
        assert!(out.contains(".reset"), "relative import inlined: {out}");
        assert!(
            out.contains("url(\"./base/dot.png\")") || out.contains("url(./base/dot.png)"),
            "url rebased to the entry dir: {out}"
        );
        assert!(out.contains(".norm"), "package style entry inlined: {out}");
        assert!(
            out.contains("@import url(https://cdn.test/x.css);"),
            "external import kept: {out}"
        );
        assert!(
            out.contains("@media print {\n.print { display: none; }"),
            "media import inlined as @media: {out}"
        );
        assert!(
            out.contains("@import \"./missing.css\" screen;"),
            "unresolvable media import kept: {out}"
        );
        // Kept imports are hoisted above the inlined rules (CSS requires it).
        let first_rule = out.find('{').unwrap();
        assert!(
            out.rfind("@import").unwrap() < first_rule,
            "imports hoisted first: {out}"
        );
        assert!(
            !out.contains("@import \"./base/reset.css\""),
            "inlined import removed: {out}"
        );
        assert!(
            out.contains(".app { color: red; }"),
            "own rules kept verbatim: {out}"
        );
        // Compiles and, in dev, rebases against the served url of the entry.
        let compiled = compile_css_rebased("/src/app.css", &out, true).unwrap();
        assert!(
            compiled.css.contains("/src/base/dot.png"),
            "{}",
            compiled.css
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // The collecting variants report every file the compile pulled in — the
    // HMR watch edges Vite gets from postcss-import deps / sass includedFiles.
    #[test]
    fn collecting_variants_report_dependency_files() {
        let base = std::env::temp_dir().join(format!("oj-css-deps-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("part.css"), ".p { color: red; }").unwrap();
        std::fs::write(base.join("nested.css"), ".n {}").unwrap();
        std::fs::write(base.join("mid.css"), "@import \"./nested.css\";\n.m {}").unwrap();
        let mut deps = Vec::new();
        inline_imports_collecting(
            "@import \"./part.css\";\n@import \"./mid.css\";\n.a {}",
            &base.join("a.css"),
            &CssResolve::default(),
            &mut deps,
        )
        .unwrap();
        for f in ["part.css", "mid.css", "nested.css"] {
            assert!(deps.contains(&base.join(f)), "{f} missing from {deps:?}");
        }

        std::fs::write(base.join("_dep.scss"), "$c: blue;").unwrap();
        let mut sass_deps = Vec::new();
        let out = compile_sass_collecting(
            "@use \"./dep\" as d;\n.s { color: d.$c; }",
            &SassOptions {
                load_dir: Some(&base),
                ..SassOptions::default()
            },
            &mut sass_deps,
        )
        .unwrap();
        assert!(out.contains("blue"), "{out}");
        assert!(
            sass_deps.contains(&base.join("_dep.scss")),
            "the partial is the dep: {sass_deps:?}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn import_inlining_survives_cycles_and_missing_files() {
        let base = std::env::temp_dir().join(format!("oj-css-cyc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(base.join("a.css"), "@import \"./b.css\";\n.a{}").unwrap();
        std::fs::write(base.join("b.css"), "@import \"./a.css\";\n.b{}").unwrap();
        let out = inline_imports("@import \"./b.css\";\n.a{}", &base.join("a.css")).unwrap();
        assert!(out.contains(".b{}") && out.contains(".a{}"), "{out}");
        assert!(
            out.contains("@import \"./a.css\";"),
            "the cycle edge stays as written: {out}"
        );
        let missing = inline_imports("@import \"./nope.css\";\n.a{}", &base.join("a.css")).unwrap();
        assert!(
            missing.contains("@import \"./nope.css\";"),
            "unresolvable import left alone: {missing}"
        );
        assert_eq!(
            relative_path(Path::new("/p/src"), Path::new("/p/src/base/dot.png")),
            "./base/dot.png"
        );
        assert_eq!(
            relative_path(Path::new("/p/src/base"), Path::new("/p/vars.css")),
            "../../vars.css"
        );
        assert_eq!(
            split_package_specifier("@acme/tokens/src/x.css"),
            Some(("@acme/tokens", "src/x.css"))
        );
        assert_eq!(
            split_package_specifier("normalize.css"),
            Some(("normalize.css", ""))
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn imports_resolve_through_alias_root_and_public_dir() {
        let base = std::env::temp_dir().join(format!("oj-css-alias-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("src/ui")).unwrap();
        std::fs::create_dir_all(base.join("public")).unwrap();
        std::fs::write(base.join("src/vars.css"), ":root { --v: 1; }\n").unwrap();
        std::fs::write(
            base.join("src/ui/theme.css"),
            ".theme { color: red; background: url(@/img.png); }\n",
        )
        .unwrap();
        std::fs::write(base.join("public/vendor.css"), ".vendor { margin: 0; }\n").unwrap();
        // A same-named file under the root must lose to the public one.
        std::fs::write(base.join("vendor.css"), ".wrong { margin: 1px; }\n").unwrap();
        let alias = vec![("@".to_string(), "./src".to_string())];
        let public = base.join("public");
        let r = CssResolve {
            root: Some(&base),
            public_dir: Some(&public),
            alias: &alias,
            ..CssResolve::default()
        };
        let dir = base.join("src/ui");
        assert_eq!(
            resolve_css_import_with("@/vars.css", &dir, &r),
            Some(base.join("src/vars.css"))
        );
        assert_eq!(
            resolve_css_import_with("@/vars", &dir, &r),
            Some(base.join("src/vars.css")),
            "extension probed"
        );
        assert_eq!(
            resolve_css_import_with("/src/vars.css", &dir, &r),
            Some(base.join("src/vars.css"))
        );
        assert_eq!(
            resolve_css_import_with("/vendor.css", &dir, &r),
            Some(base.join("public/vendor.css")),
            "public dir wins"
        );
        assert_eq!(resolve_css_import_with("/nope.css", &dir, &r), None);
        // Without a root the same specs stay unresolved (kept as written).
        assert_eq!(resolve_css_import("@/vars.css", &dir), None);
        assert_eq!(resolve_css_import("/src/vars.css", &dir), None);

        let entry = base.join("src/ui/app.css");
        let src = "@import \"@/vars.css\";\n@import \"/src/ui/theme.css\";\n@import '/vendor.css';\n.app { color: blue; }\n";
        let out = inline_imports_with(src, &entry, &r).unwrap();
        assert!(out.contains("--v: 1"), "aliased import inlined: {out}");
        assert!(
            out.contains(".theme"),
            "root-absolute import inlined: {out}"
        );
        assert!(
            out.contains(".vendor") && !out.contains(".wrong"),
            "public import inlined: {out}"
        );
        assert!(
            out.contains("url(@/img.png)") || out.contains("url(\"@/img.png\")"),
            "aliased url inside an inlined file is not rebased as relative: {out}"
        );
        assert!(!out.contains("@import"), "{out}");
        // The dev compile then turns the aliased url into the served path.
        let compiled = compile_css_dev("/src/ui/app.css", &out, false, &r)
            .unwrap()
            .css;
        assert!(compiled.contains("/src/img.png"), "{compiled}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn imports_after_a_comment_are_hoisted() {
        let out = hoist_imports(".a{}\n/* c */\n@import url(https://x.test/a.css);\n");
        assert!(
            out.starts_with("@import url(https://x.test/a.css);"),
            "{out}"
        );
    }
}
