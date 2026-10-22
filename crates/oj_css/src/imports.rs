//! `@import` inlining and resolution, as Vite's postcss-import step does it.

use std::collections::HashSet;
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
        seen_files: HashSet::new(),
        seen_contents: HashSet::new(),
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
    /// Files being inlined: an import of one is a cycle, dropped as
    /// postcss-import drops it.
    stack: Vec<PathBuf>,
    /// postcss-import's `skipDuplicates`: a file, or the same content, already
    /// inlined under the same conditions is not inlined again.
    seen_files: HashSet<(PathBuf, String)>,
    seen_contents: HashSet<(String, String)>,
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
            let Some(target) = resolve_css_import_with(&import.spec, dir, self.resolve) else {
                out.push_str(statement);
                continue;
            };
            if self.stack.contains(&target) {
                continue;
            }
            let conditions = import.conditions_key();
            if !self.seen_files.insert((target.clone(), conditions.clone())) {
                continue;
            }
            let child = std::fs::read_to_string(&target).map_err(|e| {
                format!(
                    "cannot read @import {} ({}): {e}",
                    import.spec,
                    target.display()
                )
            })?;
            self.deps.push(target.clone());
            if !self.seen_contents.insert((child.clone(), conditions)) {
                continue;
            }
            self.stack.push(target.clone());
            let child = self.inline(&child, &target)?;
            self.stack.pop();
            let child_dir = target.parent().unwrap_or(Path::new("."));
            let rebased = rebase_to_dir(&child, child_dir, dir, self.resolve);
            out.push_str(&import.wrap(&rebased));
            out.push('\n');
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

/// The `@import` statements left after inlining, moved to the top as
/// postcss-import bundles: `@charset`, then the kept imports and the empty
/// `@layer a, b;` statements that open the sheet, in the order written, then
/// the rest. CSS requires imports to precede all other rules.
fn hoist_imports(css: &str) -> String {
    let head = split_head(css);
    let mut out = String::with_capacity(css.len());
    if let Some(charset) = head.charset {
        out.push_str(charset);
        out.push('\n');
    }
    for statement in &head.statements {
        out.push_str(statement);
        out.push('\n');
    }
    out.push_str(head.body.trim_start_matches('\n'));
    out
}

/// A sheet split as postcss-import's parseStyles orders it.
struct Head<'a> {
    charset: Option<&'a str>,
    /// The prelude's imports, empty `@layer` statements and comments, in
    /// order, then the imports after it.
    statements: Vec<&'a str>,
    body: String,
}

fn split_head(css: &str) -> Head<'_> {
    let mut charset = None;
    let mut statements: Vec<&str> = Vec::new();
    let mut at = 0;
    // The prelude: statements before the first rule keep their order.
    loop {
        let rest = &css[at..];
        let trimmed = rest.trim_start();
        let ws = rest.len() - trimmed.len();
        let len = if trimmed.starts_with("/*") {
            trimmed.find("*/").map(|end| end + 2)
        } else if starts_with_word(trimmed, "@charset")
            || starts_with_word(trimmed, IMPORT)
            || starts_with_word(trimmed, "@layer")
        {
            statement_end(trimmed).map(|end| end + 1)
        } else {
            None
        };
        let Some(len) = len else { break };
        let text = &trimmed[..len];
        if starts_with_word(text, "@charset") {
            charset.get_or_insert(text);
        } else {
            statements.push(text);
        }
        at += ws + len;
    }
    // Imports past it (from inlined files) follow the prelude's.
    let mut body = String::with_capacity(css.len() - at);
    let mut rest = &css[at..];
    while let Some(pos) = rest.find(IMPORT) {
        let (before, tail) = rest.split_at(pos);
        body.push_str(before);
        match at_statement_start(&body)
            .then(|| statement_end(tail))
            .flatten()
        {
            Some(end) => {
                statements.push(&tail[..=end]);
                rest = &tail[end + 1..];
            }
            None => {
                body.push_str(IMPORT);
                rest = &tail[IMPORT.len()..];
            }
        }
    }
    body.push_str(rest);
    Head {
        charset,
        statements,
        body,
    }
}

/// Whether `s` opens with the at-keyword `word`, not a longer name.
fn starts_with_word(s: &str, word: &str) -> bool {
    s.get(..word.len())
        .is_some_and(|w| w.eq_ignore_ascii_case(word))
        && s[word.len()..]
            .chars()
            .next()
            .is_none_or(|c| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
}

/// The contents of the string `s` opens with and the bytes through its
/// closing quote.
fn quoted(s: &str) -> Option<(&str, usize)> {
    let quote = s.chars().next().filter(|&q| q == '"' || q == '\'')?;
    let close = s[1..].find(quote)?;
    Some((&s[1..1 + close], close + 2))
}

/// The `;` ending the statement `s` starts, skipping strings and parens (a
/// Google Fonts url carries `;`s), as PostCSS's tokenizer does. None when a
/// block opens or closes first.
fn statement_end(s: &str) -> Option<usize> {
    let mut quote = None;
    let mut depth = 0usize;
    let mut chars = s.char_indices();
    while let Some((i, c)) = chars.next() {
        match (quote, c) {
            (Some(_), '\\') => {
                chars.next();
            }
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ';') if depth == 0 => return Some(i),
            (None, '{' | '}') => return None,
            _ => {}
        }
    }
    None
}

/// The specifier an `@import` names (`"x"`, `'x'`, `url(x)`, `url("x")`)
/// and the bytes of `after` (what follows `@import`) through it.
fn import_target(after: &str) -> Option<(&str, usize)> {
    let trimmed = after.trim_start();
    let ws = after.len() - trimmed.len();
    let (spec, used) = if let Some(inner) = trimmed.strip_prefix("url(") {
        let body = inner.trim_start();
        let lead = inner.len() - body.len();
        let (raw, close) = match quoted(body) {
            Some((raw, end)) => (raw, lead + end + body[end..].find(')')?),
            None => {
                let close = inner.find(')')?;
                (inner[..close].trim(), close)
            }
        };
        (raw, "url(".len() + close + 1)
    } else {
        quoted(trimmed)?
    };
    Some((spec, ws + used))
}

/// The `)` closing a group whose `(` precedes `s`.
fn close_paren(s: &str) -> Option<usize> {
    let mut depth = 1usize;
    let mut quote = None;
    for (i, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), _) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '"' | '\'') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// An inlinable `@import` with its conditions, as postcss-import parses them:
/// `layer` / `layer(x)`, then `supports(...)`, then a media query.
#[derive(Debug, PartialEq, Eq)]
struct PlainImport {
    spec: String,
    /// Bytes after `@import` through the closing `;`.
    len: usize,
    /// `Some("")` for an anonymous `layer`.
    layer: Option<String>,
    supports: Option<String>,
    media: Option<String>,
}

impl PlainImport {
    /// None for external urls, which stay for the browser.
    fn parse(after: &str) -> Option<PlainImport> {
        let (spec, used) = import_target(after)?;
        let tail = &after[used..];
        let semi = statement_end(tail)?;
        if spec.is_empty() || is_external(spec) {
            return None;
        }
        let mut cond = tail[..semi].trim();
        let mut layer = None;
        if let Some(inner) = strip_prefix_ci(cond, "layer(") {
            let close = close_paren(inner)?;
            layer = Some(inner[..close].trim().to_string());
            cond = inner[close + 1..].trim_start();
        } else if starts_with_word(cond, "layer") {
            layer = Some(String::new());
            cond = cond["layer".len()..].trim_start();
        }
        let mut supports = None;
        if let Some(inner) = strip_prefix_ci(cond, "supports(") {
            let close = close_paren(inner)?;
            supports = Some(inner[..close].trim().to_string());
            cond = inner[close + 1..].trim_start();
        }
        Some(PlainImport {
            spec: spec.to_string(),
            len: used + semi + 1,
            layer,
            supports,
            media: (!cond.is_empty()).then(|| cond.to_string()),
        })
    }

    /// The conditions as an import prelude (`layer(x) supports(y) print`),
    /// postcss-import's duplicate key and the tail a kept import inherits.
    fn conditions_key(&self) -> String {
        let mut parts = Vec::new();
        match self.layer.as_deref() {
            Some("") => parts.push("layer".to_string()),
            Some(layer) => parts.push(format!("layer({layer})")),
            None => {}
        }
        if let Some(supports) = &self.supports {
            parts.push(format!("supports({supports})"));
        }
        if let Some(media) = &self.media {
            parts.push(media.clone());
        }
        parts.join(" ")
    }

    /// An imported sheet under the import's conditions, as postcss-import's
    /// applyConditions does: its rules inside the blocks (media outermost,
    /// then supports, then layer), and the imports it keeps carrying the
    /// conditions themselves.
    fn wrap(&self, css: &str) -> String {
        let key = self.conditions_key();
        if key.is_empty() {
            return css.to_string();
        }
        let head = split_head(css);
        let mut out = String::with_capacity(css.len() + 64);
        let mut body = String::new();
        for statement in &head.statements {
            match kept_import_with(statement, &key) {
                Some(import) => {
                    out.push_str(&import);
                    out.push('\n');
                }
                None => {
                    body.push_str(statement);
                    body.push('\n');
                }
            }
        }
        body.push_str(head.body.trim_start_matches('\n'));
        let block = |head: String, body: String| format!("{head} {{\n{body}\n}}");
        let mut css = body;
        if let Some(layer) = &self.layer {
            css = block(format!("@layer {layer}").trim_end().to_string(), css);
        }
        if let Some(supports) = &self.supports {
            css = block(format!("@supports ({supports})"), css);
        }
        if let Some(media) = &self.media {
            css = block(format!("@media {media}"), css);
        }
        out.push_str(&css);
        out
    }
}

/// A kept `@import` statement with `conditions` appended, when it has none of
/// its own (nested conditions would need postcss-import's data: url form).
fn kept_import_with(statement: &str, conditions: &str) -> Option<String> {
    if !starts_with_word(statement, IMPORT) {
        return None;
    }
    let after = &statement[IMPORT.len()..];
    let (_, used) = import_target(after)?;
    let own = after[used..].trim_end_matches(';').trim();
    Some(if own.is_empty() {
        format!("{IMPORT}{} {conditions};", &after[..used])
    } else {
        statement.to_string()
    })
}

/// `s` without a leading `prefix`, matched ignoring ASCII case.
fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    s.get(..prefix.len())
        .filter(|p| p.eq_ignore_ascii_case(prefix))
        .map(|_| &s[prefix.len()..])
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
        if read_package_json(&pkg_dir)
            .is_some_and(|json| json.get("exports").is_some_and(|e| !e.is_null()))
        {
            return exports_css_entry(spec, dir, resolve.production);
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

/// A package with `exports` resolves through them alone, with Vite's CSS
/// resolver conditions (`style`, the mode, `import`, `default`).
fn exports_css_entry(spec: &str, dir: &Path, production: bool) -> Option<PathBuf> {
    let mode = if production {
        "production"
    } else {
        "development"
    };
    let resolver = oxc_resolver::Resolver::new(oxc_resolver::ResolveOptions {
        condition_names: ["style", mode, "import", "default"]
            .map(String::from)
            .to_vec(),
        extensions: vec![".css".to_string()],
        main_fields: vec!["style".to_string()],
        main_files: Vec::new(),
        ..oxc_resolver::ResolveOptions::default()
    });
    let path = resolver.resolve(dir, spec).ok()?.into_path_buf();
    // A JS entry is no stylesheet: Vite fails on it; oj keeps the import.
    is_style_ext(&path).then_some(path)
}

fn is_style_ext(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
        matches!(
            e,
            "css" | "scss" | "sass" | "less" | "styl" | "stylus" | "pcss" | "postcss" | "sss"
        )
    })
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
            !out.contains("@import"),
            "the cycle edge is dropped, as postcss-import drops it: {out}"
        );
        assert_eq!(out.matches(".b{}").count(), 1, "{out}");
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

    const FONTS: &str =
        "@import url('https://fonts.test/css2?family=X:wght@400;500;700&display=swap');";

    #[test]
    fn remote_import_with_semicolons_hoists_whole() {
        // An imported file's remote import goes first (postcss-import's
        // beforeBundle), whole despite the `;`s in its url.
        let base = scratch("fonts-hoist");
        std::fs::write(base.join("a.css"), format!("{FONTS}\n.a{{}}\n")).unwrap();
        let out = inline_imports("@import \"./a.css\";\n.b{}\n", &base.join("index.css")).unwrap();
        assert!(out.starts_with(&format!("{FONTS}\n")), "{out}");
        assert!(out.find(".a{}") < out.find(".b{}"), "{out}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn statement_end_skips_strings_and_parens() {
        assert_eq!(statement_end(" url('a;b') print;x"), Some(17));
        assert_eq!(statement_end(" \"a;b\";"), Some(6));
        assert_eq!(statement_end(" url(a;b);"), Some(9));
        assert_eq!(statement_end(" \"a\\\";\";"), Some(7));
        assert_eq!(statement_end(" x { y; }"), None);
    }

    #[test]
    fn relative_imports_after_a_remote_font_import_are_inlined() {
        let base = std::env::temp_dir().join(format!("oj-css-fonts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("styles")).unwrap();
        std::fs::write(base.join("styles/a.css"), ".from-a { color: red; }\n").unwrap();
        std::fs::write(base.join("styles/b.css"), ".from-b { color: blue; }\n").unwrap();
        let src = format!(
            "{FONTS}\n@import \"./styles/a.css\";\n@import './styles/b.css';\n\n.app {{ margin: 0; }}\n"
        );
        let out = inline_imports(&src, &base.join("index.css")).unwrap();
        assert!(out.starts_with(&format!("{FONTS}\n")), "{out}");
        assert!(!out.contains("./styles/"), "{out}");
        let css = compile_css_dev("/index.css", &out, false, &CssResolve::default())
            .unwrap()
            .css;
        for rule in [".from-a", ".from-b", ".app", "wght@400;500;700"] {
            assert!(css.contains(rule), "{rule} kept:\n{css}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    fn scratch(name: &str) -> PathBuf {
        let base = std::env::temp_dir().join(format!("oj-css-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn layer_supports_and_media_imports_are_inlined_in_place() {
        let base = scratch("conditions");
        std::fs::write(base.join("a.css"), ".a { color: red; }\n").unwrap();
        let src = "@layer base, components;\n@import \"./a.css\" layer(components) supports(display: grid) print;\n@import './a.css' layer;\n.own { color: blue; }\n";
        let out = inline_imports(src, &base.join("index.css")).unwrap();
        assert!(
            out.starts_with("@layer base, components;\n"),
            "layer order statement first: {out}"
        );
        assert!(
            out.contains("@media print {\n@supports (display: grid) {\n@layer components {\n.a { color: red; }"),
            "media outermost, then supports, then layer: {out}"
        );
        assert!(
            out.contains("@layer {\n.a { color: red; }"),
            "anonymous layer: {out}"
        );
        assert!(!out.contains("@import"), "{out}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn prelude_imports_and_layer_statements_keep_their_order() {
        // postcss-import's beforeBundle keeps the prelude in source order.
        let out = hoist_imports(&format!("{FONTS}\n@layer a, b;\n.x{{}}\n"));
        assert!(
            out.starts_with(&format!("{FONTS}\n@layer a, b;\n")),
            "{out}"
        );
        let remote = "@import url(https://x.test/b.css) layer(b);";
        let out = hoist_imports(&format!(
            "/* c */\n@layer a;\n{remote}\n@layer c;\n.x{{}}\n"
        ));
        assert!(
            out.starts_with(&format!("/* c */\n@layer a;\n{remote}\n@layer c;\n")),
            "{out}"
        );
    }

    #[test]
    fn duplicates_and_cycles_are_dropped_like_postcss_import() {
        let base = scratch("dupes");
        std::fs::write(base.join("a.css"), ".a{}\n").unwrap();
        std::fs::write(base.join("same.css"), ".a{}\n").unwrap();
        std::fs::write(base.join("self.css"), "@import \"./self.css\";\n.s{}\n").unwrap();
        let src = "@import \"./a.css\";\n@import \"./a.css\";\n@import \"./same.css\";\n@import \"./a.css\" print;\n@import \"./self.css\";\n";
        let out = inline_imports(src, &base.join("index.css")).unwrap();
        assert_eq!(
            out.matches(".a{}").count(),
            2,
            "once bare, once under print: {out}"
        );
        assert!(out.contains("@media print {\n.a{}"), "{out}");
        assert_eq!(out.matches(".s{}").count(), 1, "{out}");
        assert!(!out.contains("@import"), "{out}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn kept_imports_in_a_conditional_import_carry_its_conditions() {
        let base = scratch("kept-conditions");
        std::fs::write(base.join("fonts.css"), format!("{FONTS}\n.f{{}}\n")).unwrap();
        std::fs::write(
            base.join("own.css"),
            "@import url(https://x.test/o.css) screen;\n.o{}\n",
        )
        .unwrap();
        let src = "@import \"./fonts.css\" layer(f) print;\n@import \"./own.css\" print;\n";
        let out = inline_imports(src, &base.join("index.css")).unwrap();
        let kept = FONTS.trim_end_matches(';');
        assert!(
            out.starts_with(&format!("{kept} layer(f) print;\n")),
            "{out}"
        );
        assert!(
            out.contains("@import url(https://x.test/o.css) screen;"),
            "an import with its own conditions keeps them: {out}"
        );
        assert!(out.contains("@media print {\n@layer f {\n.f{}"), "{out}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn conditions_match_ignoring_case() {
        let base = scratch("ci");
        std::fs::write(base.join("a.css"), ".a{}\n").unwrap();
        let out = inline_imports(
            "@import \"./a.css\" LAYER(up) Supports(display: grid);\n",
            &base.join("index.css"),
        )
        .unwrap();
        assert!(
            out.contains("@supports (display: grid) {\n@layer up {\n.a{}"),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn package_exports_resolve_with_vite_css_conditions() {
        let base = scratch("exports");
        let pkg = base.join("node_modules/ui-kit");
        std::fs::create_dir_all(pkg.join("dist")).unwrap();
        std::fs::write(
            pkg.join("package.json"),
            r#"{"name":"ui-kit","style":"./wrong.css","exports":{".":{"style":"./dist/kit.css"},"./tailwind.css":{"style":{"production":"./dist/tw.min.css","development":"./dist/tw.css"}}}}"#,
        )
        .unwrap();
        for f in ["kit.css", "tw.css", "tw.min.css"] {
            std::fs::write(
                pkg.join("dist").join(f),
                format!(".{} {{}}\n", f.replace('.', "-")),
            )
            .unwrap();
        }
        std::fs::write(pkg.join("wrong.css"), ".wrong {}\n").unwrap();
        let dev = CssResolve::default();
        let prod = CssResolve {
            production: true,
            ..CssResolve::default()
        };
        let r = |spec: &str, resolve: &CssResolve<'_>| {
            resolve_css_import_with(spec, &base, resolve).map(|p| p.canonicalize().unwrap())
        };
        let dist = pkg.join("dist").canonicalize().unwrap();
        assert_eq!(
            r("ui-kit", &dev),
            Some(dist.join("kit.css")),
            "exports beat the style field"
        );
        assert_eq!(r("ui-kit/tailwind.css", &dev), Some(dist.join("tw.css")));
        assert_eq!(
            r("ui-kit/tailwind.css", &prod),
            Some(dist.join("tw.min.css"))
        );
        assert_eq!(
            r("ui-kit/wrong.css", &dev),
            None,
            "exports encapsulate the package"
        );
        // `exports: null` is no exports map (Vite checks `if (data.exports)`).
        let nul = base.join("node_modules/nul");
        std::fs::create_dir_all(&nul).unwrap();
        std::fs::write(
            nul.join("package.json"),
            r#"{"name":"nul","style":"s.css","exports":null}"#,
        )
        .unwrap();
        std::fs::write(nul.join("s.css"), ".s{}").unwrap();
        assert_eq!(
            r("nul", &dev),
            Some(nul.join("s.css").canonicalize().unwrap())
        );
        // A JS-only entry is no stylesheet to inline.
        let js = base.join("node_modules/jsonly");
        std::fs::create_dir_all(&js).unwrap();
        std::fs::write(
            js.join("package.json"),
            r#"{"name":"jsonly","style":"s.css","exports":{".":{"import":"./i.js"}}}"#,
        )
        .unwrap();
        std::fs::write(js.join("i.js"), "export default 1;").unwrap();
        assert_eq!(r("jsonly", &dev), None);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn inlined_tailwind_paths_rebase_to_the_inlining_sheet() {
        let base = scratch("tw-paths");
        std::fs::create_dir_all(base.join("sub")).unwrap();
        std::fs::write(
            base.join("sub/child.css"),
            "@plugin \"./plug.js\";\n@plugin \"./opts.js\" { color: blue; }\n@source '../content';\n@source \"!./skip\";\n@config \"tailwind.config.js\";\n@source inline(\"underline\");\n",
        )
        .unwrap();
        let out =
            inline_imports("@import \"./sub/child.css\";\n", &base.join("index.css")).unwrap();
        for want in [
            "@plugin \"./sub/plug.js\";",
            "@plugin \"./sub/opts.js\" { color: blue; }",
            "@source './content';",
            "@source \"!./sub/skip\";",
            "@config \"tailwind.config.js\";",
            "@source inline(\"underline\");",
        ] {
            assert!(out.contains(want), "{want}: {out}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }
}
