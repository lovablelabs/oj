// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, Expression, NewExpression, ObjectPropertyKind, Program,
    PropertyKey, Statement,
};
use oxc_ast_visit::{walk, walk_mut, Visit, VisitMut};
use oxc_parser::Parser;
use oxc_span::SourceType;

pub fn expand_source(source: &str, path: &Path) -> String {
    if !source.contains("import.meta.glob") {
        return source.to_string();
    }
    let allocator = crate::pooled_allocator();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return source.to_string();
    }
    let mut program = parsed.program;
    let dir = path.parent().unwrap_or(path);
    expand(&allocator, dir, &mut program);
    oxc_codegen::Codegen::new().build(&program).code
}

/// The positive `import.meta.glob` patterns a module uses, resolved against its
/// directory the way `expand` resolves them (`<dir>/pages/*.tsx`). The dev
/// server's watcher keeps these per importer: a file created or deleted under
/// one of them changes what the glob expands to, so the importer is updated as
/// if it had been edited (Vite's importMetaGlob `hotUpdate`).
pub fn glob_patterns(source: &str, path: &Path) -> Vec<String> {
    if !source.contains("import.meta.glob") {
        return Vec::new();
    }
    let allocator = crate::pooled_allocator();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return Vec::new();
    }
    let dir = path.parent().unwrap_or(path);
    let mut collector = PatternCollector {
        dir,
        out: Vec::new(),
    };
    collector.visit_program(&parsed.program);
    collector.out
}

struct PatternCollector<'d> {
    dir: &'d Path,
    out: Vec<String>,
}

impl<'a> Visit<'a> for PatternCollector<'_> {
    fn visit_call_expression(&mut self, call: &oxc_ast::ast::CallExpression<'a>) {
        if is_import_meta_glob(call) {
            if let Some(patterns) = call.arguments.first().and_then(collect_patterns) {
                for pattern in patterns {
                    if !pattern.starts_with('!') {
                        // `dir/./pages/*` would not match `dir/pages/x` as a
                        // glob pattern: drop the current-dir segments.
                        let rel = pattern.strip_prefix("./").unwrap_or(&pattern);
                        let abs = self.dir.join(rel).to_string_lossy().replace('\\', "/");
                        self.out.push(abs.replace("/./", "/"));
                    }
                }
            }
        }
        walk::walk_call_expression(self, call);
    }
}

pub fn expand<'a>(allocator: &'a Allocator, dir: &Path, program: &mut Program<'a>) {
    let mut expander = GlobExpander {
        allocator,
        dir,
        hoisted: Vec::new(),
        uid: 0,
    };
    expander.visit_program(program);
    let hoisted = expander.hoisted;
    if hoisted.is_empty() {
        return;
    }
    let src: &str = allocator.alloc_str(&hoisted.join("\n"));
    let parsed = Parser::new(allocator, src, SourceType::mjs()).parse();
    let mut stmts: Vec<Statement> = parsed.program.body.into_iter().collect();
    stmts.reverse();
    for stmt in stmts {
        program.body.insert(0, stmt);
    }
}

struct GlobExpander<'a, 'd> {
    allocator: &'a Allocator,
    dir: &'d Path,
    hoisted: Vec<String>,
    uid: usize,
}

#[derive(Default)]
struct GlobOptions {
    eager: bool,
    import: Option<String>,
    query: String,
    /// Vite's `base`: globs resolve against, and keys are relative to, this
    /// directory (`./x` / `../x` from the importer; `/x` from the root is not
    /// resolvable here and is treated as importer-relative).
    base: Option<String>,
    /// Vite's `exhaustive`: also match dotfiles and `node_modules`.
    exhaustive: bool,
}

/// `to` as an import specifier relative to `from_dir` (`./x` or `../x`),
/// both lexical paths under the same root.
fn relative_spec(from_dir: &Path, to: &Path) -> String {
    let from: Vec<_> = from_dir.components().collect();
    let to_parts: Vec<_> = to.components().collect();
    let common = from.iter().zip(&to_parts).take_while(|(a, b)| a == b).count();
    let mut out = String::new();
    for _ in common..from.len() {
        out.push_str("../");
    }
    if out.is_empty() {
        out.push_str("./");
    }
    let rest: Vec<String> = to_parts[common..]
        .iter()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    out.push_str(&rest.join("/"));
    out
}

/// `dir/base` normalized lexically (`.` and `..` folded).
fn join_normalized(dir: &Path, base: &str) -> std::path::PathBuf {
    let mut out = dir.to_path_buf();
    for c in Path::new(base).components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir | std::path::Component::RootDir => {}
            other => out.push(other),
        }
    }
    out
}

impl<'a> GlobExpander<'a, '_> {
    fn parse_expr(&self, source: &str) -> Option<Expression<'a>> {
        let source: &'a str = self.allocator.alloc_str(source);
        let parsed = Parser::new(self.allocator, source, SourceType::mjs()).parse();
        match parsed.program.body.into_iter().next() {
            Some(Statement::ExpressionStatement(es)) => Some(es.unbox().expression),
            _ => None,
        }
    }

    fn build_replacement(&mut self, args: &[Argument<'a>]) -> Option<String> {
        let patterns = collect_patterns(args.first()?)?;
        let opts = args
            .get(1)
            .map(collect_options)
            .unwrap_or(Some(GlobOptions::default()))?;

        // With `base`, globs resolve against and keys are relative to the base
        // directory, while the import specifiers stay relative to the importer
        // (Vite's importMetaGlob resolvePaths).
        let base_dir = opts.base.as_deref().map(|b| join_normalized(self.dir, b));
        let glob_dir = base_dir.as_deref().unwrap_or(self.dir);
        let matches = glob_matches(glob_dir, &patterns, opts.exhaustive);
        let mut entries: Vec<String> = Vec::new();
        for key in &matches {
            let import_path = match &base_dir {
                Some(b) => relative_spec(self.dir, &join_normalized(b, key)),
                None => key.clone(),
            };
            let spec = format!("{import_path}{}", opts.query);
            if opts.eager {
                let ident = format!("__oj_glob_{}", self.uid);
                self.uid += 1;
                match &opts.import {
                    Some(name) if name != "*" => self
                        .hoisted
                        .push(format!("import {{ {name} as {ident} }} from {spec:?};")),
                    _ => self
                        .hoisted
                        .push(format!("import * as {ident} from {spec:?};")),
                }
                entries.push(format!("{key:?}: {ident}"));
            } else {
                let importer = match &opts.import {
                    Some(name) if name != "*" => {
                        format!("() => import({spec:?}).then((m) => m[{name:?}])")
                    }
                    _ => format!("() => import({spec:?})"),
                };
                entries.push(format!("{key:?}: {importer}"));
            }
        }
        Some(format!("({{{}}})", entries.join(", ")))
    }
}

impl<'a> VisitMut<'a> for GlobExpander<'a, '_> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        let replacement = match &*expr {
            Expression::CallExpression(call) if is_import_meta_glob(call) => {
                self.build_replacement(&call.arguments)
            }
            _ => None,
        };
        if let Some(replacement) = replacement {
            if let Some(new_expr) = self.parse_expr(&replacement) {
                *expr = new_expr;
                return;
            }
        }
        walk_mut::walk_expression(self, expr);
    }
}

pub fn expand_dynamic_import_vars<'a>(
    allocator: &'a Allocator,
    dir: &Path,
    program: &mut Program<'a>,
    source_text: &str,
) -> bool {
    let mut v = DynImportVars {
        allocator,
        dir,
        source: source_text,
        changed: false,
    };
    v.visit_program(program);
    v.changed
}

/// Rewrites `new URL("./asset", import.meta.url)` into a hoisted `?url` asset
/// import referenced in place, so the asset flows through oj's normal asset
/// pipeline (Vite's asset-import-meta-url). A template literal
/// (`new URL(\`./img/${name}.png\`, import.meta.url)`) becomes a lookup over
/// the files matching its glob, falling back to the original url; and
/// `new Worker(new URL("./w.ts", import.meta.url), ...)` becomes a
/// `?worker&url` import so the worker is bundled as its own chunk (Vite's
/// worker-import-meta-url). Returns whether anything changed.
pub fn expand_new_url_asset<'a>(
    allocator: &'a Allocator,
    dir: &Path,
    program: &mut Program<'a>,
    source: &str,
) -> bool {
    let mut v = NewUrlAsset {
        allocator,
        dir,
        source,
        hoisted: Vec::new(),
        uid: 0,
    };
    v.visit_program(program);
    if v.hoisted.is_empty() {
        return false;
    }
    let src: &'a str = allocator.alloc_str(&v.hoisted.join("\n"));
    let parsed = Parser::new(allocator, src, SourceType::mjs()).parse();
    let mut stmts: Vec<Statement> = parsed.program.body.into_iter().collect();
    stmts.reverse();
    for stmt in stmts {
        program.body.insert(0, stmt);
    }
    true
}

struct NewUrlAsset<'a, 'd, 's> {
    allocator: &'a Allocator,
    dir: &'d Path,
    source: &'s str,
    hoisted: Vec<String>,
    uid: usize,
}

enum UrlSpec {
    /// `new URL("./a.png", import.meta.url)`
    Literal(String),
    /// `new URL(\`./img/${n}.png\`, import.meta.url)`: the glob it implies and
    /// the template's source text.
    Template { pattern: String, arg: String },
}

impl<'a> NewUrlAsset<'a, '_, '_> {
    fn is_import_meta_url(arg: &Argument<'a>) -> bool {
        match arg.as_expression() {
            Some(Expression::StaticMemberExpression(m)) => {
                m.property.name == "url" && matches!(&m.object, Expression::ImportMeta(_))
            }
            _ => false,
        }
    }

    fn asset_spec(&self, n: &NewExpression<'a>) -> Option<UrlSpec> {
        let Expression::Identifier(id) = &n.callee else {
            return None;
        };
        if id.name != "URL" || n.arguments.len() != 2 || !Self::is_import_meta_url(&n.arguments[1]) {
            return None;
        }
        match n.arguments[0].as_expression()? {
            Expression::StringLiteral(spec) => {
                let spec = spec.value.as_str();
                (spec.starts_with("./") || spec.starts_with("../")).then(|| UrlSpec::Literal(spec.to_string()))
            }
            Expression::TemplateLiteral(tpl) if !tpl.expressions.is_empty() => {
                let mut pattern = String::new();
                for (i, q) in tpl.quasis.iter().enumerate() {
                    let piece = q.value.cooked.as_ref().map(|c| c.as_str()).unwrap_or(q.value.raw.as_str());
                    pattern.push_str(piece);
                    if i < tpl.expressions.len() {
                        pattern.push('*');
                    }
                }
                if !(pattern.starts_with("./") || pattern.starts_with("../")) {
                    return None;
                }
                let arg = self.source.get(tpl.span.start as usize..tpl.span.end as usize)?.to_string();
                Some(UrlSpec::Template { pattern, arg })
            }
            _ => None,
        }
    }

    /// `new Worker(new URL("./w.ts", import.meta.url), opts)` (or SharedWorker):
    /// the worker entry, to import as `?worker&url`.
    fn worker_url_spec(&self, n: &NewExpression<'a>) -> Option<(String, &'static str)> {
        let Expression::Identifier(id) = &n.callee else {
            return None;
        };
        let kind = match id.name.as_str() {
            "Worker" => "worker",
            "SharedWorker" => "sharedworker",
            _ => return None,
        };
        let Expression::NewExpression(url) = n.arguments.first()?.as_expression()? else {
            return None;
        };
        match self.asset_spec(url)? {
            UrlSpec::Literal(spec) => Some((spec, kind)),
            UrlSpec::Template { .. } => None,
        }
    }

    fn parse_expr(&self, rep: &str) -> Option<Expression<'a>> {
        let src: &'a str = self.allocator.alloc_str(rep);
        let parsed = Parser::new(self.allocator, src, SourceType::mjs()).parse();
        match parsed.program.body.into_iter().next() {
            Some(Statement::ExpressionStatement(es)) => Some(es.unbox().expression),
            _ => None,
        }
    }
}

impl<'a> VisitMut<'a> for NewUrlAsset<'a, '_, '_> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        if let Expression::NewExpression(n) = &mut *expr {
            if let Some((spec, kind)) = self.worker_url_spec(n) {
                let ident = format!("__oj_worker_{}", self.uid);
                self.uid += 1;
                self.hoisted
                    .push(format!("import {ident} from {:?};", format!("{spec}?{kind}&url")));
                if let Some(url_expr) = self.parse_expr(&ident) {
                    if let Some(first) = n.arguments.first_mut() {
                        *first = Argument::from(url_expr);
                    }
                }
                // The remaining arguments (worker options) are visited as usual.
                for arg in n.arguments.iter_mut().skip(1) {
                    if let Some(e) = arg.as_expression_mut() {
                        walk_mut::walk_expression(self, e);
                    }
                }
                return;
            }
        }
        if let Expression::NewExpression(n) = &*expr {
            match self.asset_spec(n) {
                Some(UrlSpec::Literal(spec)) => {
                    let ident = format!("__oj_url_{}", self.uid);
                    self.uid += 1;
                    self.hoisted
                        .push(format!("import {ident} from {:?};", format!("{spec}?url")));
                    if let Some(e) = self.parse_expr(&format!("new URL({ident}, import.meta.url)")) {
                        *expr = e;
                    }
                    return;
                }
                Some(UrlSpec::Template { pattern, arg }) => {
                    let mut matches = glob_matches(self.dir, &[pattern], true);
                    matches.sort();
                    if !matches.is_empty() {
                        let mut entries = Vec::new();
                        for key in &matches {
                            let ident = format!("__oj_url_{}", self.uid);
                            self.uid += 1;
                            self.hoisted
                                .push(format!("import {ident} from {:?};", format!("{key}?url")));
                            entries.push(format!("{key:?}: {ident}"));
                        }
                        // Unmatched at runtime: keep the original url, as Vite does.
                        let rep = format!(
                            "new URL((function (p) {{ var m = {{{}}}; return m[p] ?? p; }})({arg}), import.meta.url)",
                            entries.join(", ")
                        );
                        if let Some(e) = self.parse_expr(&rep) {
                            *expr = e;
                        }
                        return;
                    }
                }
                None => {}
            }
        }
        walk_mut::walk_expression(self, expr);
    }
}

pub fn expand_new_url_asset_source(source: &str, path: &Path) -> String {
    if !source.contains("import.meta.url") {
        return source.to_string();
    }
    let allocator = crate::pooled_allocator();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return source.to_string();
    }
    let mut program = parsed.program;
    let dir = path.parent().unwrap_or(path);
    if expand_new_url_asset(&allocator, dir, &mut program, source) {
        oxc_codegen::Codegen::new().build(&program).code
    } else {
        source.to_string()
    }
}

pub fn expand_dynamic_import_vars_source(source: &str, path: &Path) -> String {
    if !source.contains("import(") {
        return source.to_string();
    }
    let allocator = crate::pooled_allocator();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return source.to_string();
    }
    let mut program = parsed.program;
    let dir = path.parent().unwrap_or(path);
    if expand_dynamic_import_vars(&allocator, dir, &mut program, source) {
        oxc_codegen::Codegen::new().build(&program).code
    } else {
        source.to_string()
    }
}

struct DynImportVars<'a, 'd, 's> {
    allocator: &'a Allocator,
    dir: &'d Path,
    source: &'s str,
    changed: bool,
}

impl<'a> DynImportVars<'a, '_, '_> {
    fn build(&self, imp: &oxc_ast::ast::ImportExpression<'a>) -> Option<String> {
        let Expression::TemplateLiteral(tpl) = &imp.source else {
            return None;
        };
        if tpl.expressions.is_empty() {
            return None;
        }
        let mut pattern = String::new();
        for (i, q) in tpl.quasis.iter().enumerate() {
            let piece = q
                .value
                .cooked
                .as_ref()
                .map(|c| c.as_str())
                .unwrap_or(q.value.raw.as_str());
            pattern.push_str(piece);
            if i < tpl.expressions.len() {
                pattern.push('*');
            }
        }
        if !(pattern.starts_with("./") || pattern.starts_with("../")) {
            return None;
        }
        let mut matches = glob_matches(self.dir, &[pattern], true);
        matches.sort();
        if matches.is_empty() {
            return None;
        }
        let arg = self
            .source
            .get(tpl.span.start as usize..tpl.span.end as usize)?;
        let mut cases = String::new();
        for key in &matches {
            cases.push_str(&format!("case {key:?}: return import({key:?});"));
        }
        Some(format!(
            "(function (p) {{ switch (p) {{ {cases}default: return Promise.reject(new Error(\"Unknown variable dynamic import: \" + p)); }} }})({arg})"
        ))
    }
}

impl<'a> VisitMut<'a> for DynImportVars<'a, '_, '_> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        if let Expression::ImportExpression(imp) = &*expr {
            if let Some(rep) = self.build(imp) {
                let src: &'a str = self.allocator.alloc_str(&rep);
                let parsed = Parser::new(self.allocator, src, SourceType::mjs()).parse();
                if let Some(Statement::ExpressionStatement(es)) =
                    parsed.program.body.into_iter().next()
                {
                    *expr = es.unbox().expression;
                    self.changed = true;
                    return;
                }
            }
        }
        walk_mut::walk_expression(self, expr);
    }
}

fn is_import_meta_glob(call: &oxc_ast::ast::CallExpression) -> bool {
    let Expression::StaticMemberExpression(member) = &call.callee else {
        return false;
    };
    member.property.name == "glob" && matches!(member.object, Expression::ImportMeta(_))
}

fn collect_patterns(arg: &Argument) -> Option<Vec<String>> {
    match arg.as_expression()? {
        Expression::StringLiteral(s) => Some(vec![s.value.to_string()]),
        Expression::ArrayExpression(arr) => {
            let mut out = Vec::new();
            for el in &arr.elements {
                if let ArrayExpressionElement::StringLiteral(s) = el {
                    out.push(s.value.to_string());
                } else {
                    return None;
                }
            }
            Some(out)
        }
        _ => None,
    }
}

fn collect_options(arg: &Argument) -> Option<GlobOptions> {
    let Some(Expression::ObjectExpression(obj)) = arg.as_expression() else {
        return None;
    };
    let mut opts = GlobOptions::default();
    for prop in &obj.properties {
        let ObjectPropertyKind::ObjectProperty(p) = prop else {
            continue;
        };
        let key = match &p.key {
            PropertyKey::StaticIdentifier(id) => id.name.as_str(),
            PropertyKey::StringLiteral(s) => s.value.as_str(),
            _ => continue,
        };
        match key {
            "eager" => {
                if let Expression::BooleanLiteral(b) = &p.value {
                    opts.eager = b.value;
                }
            }
            "import" => {
                if let Expression::StringLiteral(s) = &p.value {
                    opts.import = Some(s.value.to_string());
                }
            }
            "query" => {
                if let Expression::StringLiteral(s) = &p.value {
                    let q = s.value.as_str();
                    opts.query = if q.starts_with('?') {
                        q.to_string()
                    } else {
                        format!("?{q}")
                    };
                }
            }
            "as" => {
                if let Expression::StringLiteral(s) = &p.value {
                    opts.query = format!("?{}", s.value);
                }
            }
            "base" => {
                if let Expression::StringLiteral(s) = &p.value {
                    opts.base = Some(s.value.to_string());
                }
            }
            "exhaustive" => {
                if let Expression::BooleanLiteral(b) = &p.value {
                    opts.exhaustive = b.value;
                }
            }
            _ => {}
        }
    }
    Some(opts)
}

/// Whether a matched path is hidden from a non-exhaustive glob, as Vite's
/// `dot: false` + `ignore: ['**/node_modules/**']` hide it: a `node_modules`
/// segment, or a dot-led segment the pattern did not spell out literally.
fn hidden_by_default(rel: &str, pattern: &str) -> bool {
    let pattern_dots: Vec<&str> = pattern
        .split('/')
        .filter(|seg| seg.starts_with('.') && *seg != "." && *seg != "..")
        .collect();
    rel.split('/').any(|seg| {
        // `.`/`..` are path steps, not dotfiles: a `../sibling/*` glob from a
        // nested importer must match (Vite's tinyglobby does).
        seg == "node_modules"
            || (seg.starts_with('.')
                && seg != "."
                && seg != ".."
                && !pattern_dots.iter().any(|p| glob::Pattern::new(p).is_ok_and(|g| g.matches(seg))))
    })
}

fn glob_matches(dir: &Path, patterns: &[String], exhaustive: bool) -> Vec<String> {
    let (negatives, positives): (Vec<&String>, Vec<&String>) =
        patterns.iter().partition(|p| p.starts_with('!'));
    let neg_patterns: Vec<glob::Pattern> = negatives
        .iter()
        .filter_map(|p| glob::Pattern::new(p.trim_start_matches('!').trim_start_matches("./")).ok())
        .collect();

    let mut keys = std::collections::BTreeSet::new();
    for pat in positives {
        let abs = dir.join(pat);
        let Ok(paths) = glob::glob(&abs.to_string_lossy()) else {
            continue;
        };
        for entry in paths.flatten() {
            if !entry.is_file() {
                continue;
            }
            let Ok(rel) = entry.strip_prefix(dir) else {
                continue;
            };
            let rel = rel.to_string_lossy().replace('\\', "/");
            if neg_patterns.iter().any(|n| n.matches(&rel)) {
                continue;
            }
            if !exhaustive && hidden_by_default(&rel, pat) {
                continue;
            }
            let key = if rel.starts_with('.') {
                rel
            } else {
                format!("./{rel}")
            };
            keys.insert(key);
        }
    }
    keys.into_iter().collect()
}

#[cfg(test)]
mod tests {
    fn tmp(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("oj-glob-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("img")).unwrap();
        std::fs::write(d.join("img/a.png"), b"a").unwrap();
        std::fs::write(d.join("img/b.png"), b"b").unwrap();
        std::fs::write(d.join("w.ts"), "self.onmessage = () => {};").unwrap();
        d
    }

    // A `../sibling/*` glob from a nested importer must match: the `..` path
    // step is not a dotfile (the hidden-by-default rule once swallowed it and
    // every parent-relative glob expanded to an empty map).
    #[test]
    fn parent_relative_globs_match() {
        let d = tmp("parent-rel");
        std::fs::create_dir_all(d.join("routes")).unwrap();
        let out = super::expand_source(
            "const m = import.meta.glob(\"../img/*.png\", { eager: true });",
            &d.join("routes/index.tsx"),
        );
        assert!(out.contains("../img/a.png"), "{out}");
        assert!(out.contains("../img/b.png"), "{out}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn glob_patterns_are_resolved_against_the_importer_dir() {
        let src = "const a = import.meta.glob('./pages/*.tsx');\n\
                   const b = import.meta.glob(['./img/**/*.png', '!./img/skip.png'], { eager: true });\n\
                   const c = import.meta.glob(dynamic);\n";
        let pats = glob_patterns(src, std::path::Path::new("/app/src/main.ts"));
        assert_eq!(pats, vec!["/app/src/pages/*.tsx", "/app/src/img/**/*.png"]);
        assert!(glob_patterns("export const x = 1;", std::path::Path::new("/app/x.ts")).is_empty());
        // The resolved pattern matches the files `expand` would list (with `*`
        // stopping at `/`, as the directory walk in glob_matches does).
        let p = glob::Pattern::new(&pats[0]).unwrap();
        let opts = glob::MatchOptions { require_literal_separator: true, ..Default::default() };
        assert!(p.matches_path_with(std::path::Path::new("/app/src/pages/About.tsx"), opts));
        assert!(!p.matches_path_with(std::path::Path::new("/app/src/pages/nested/Deep.tsx"), opts));
    }

    #[test]
    fn template_literal_new_url_expands_to_a_glob_lookup() {
        let d = tmp("tpl");
        let src = "const n = 'a';\nexport const u = new URL(`./img/${n}.png`, import.meta.url);\n";
        let out = expand_new_url_asset_source(src, &d.join("main.js"));
        assert!(out.contains("import __oj_url_0 from \"./img/a.png?url\""), "{out}");
        assert!(out.contains("import __oj_url_1 from \"./img/b.png?url\""), "{out}");
        assert!(out.contains("\"./img/a.png\": __oj_url_0"), "{out}");
        assert!(out.contains("m[p] ?? p"), "unmatched keys fall back to the literal url: {out}");
        assert!(out.contains("`./img/${n}.png`"), "the original template is the lookup key: {out}");
        // A template that matches nothing is left alone.
        let none = expand_new_url_asset_source("new URL(`./nope/${x}.png`, import.meta.url);", &d.join("main.js"));
        assert!(none.contains("new URL(`./nope/${x}.png`, import.meta.url)"), "{none}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn new_worker_with_import_meta_url_becomes_a_worker_url_import() {
        let d = tmp("worker");
        let src = "const w = new Worker(new URL(\"./w.ts\", import.meta.url), { type: \"module\" });\nconst s = new SharedWorker(new URL(\"./w.ts\", import.meta.url));\n";
        let out = expand_new_url_asset_source(src, &d.join("main.js"));
        assert!(out.contains("import __oj_worker_0 from \"./w.ts?worker&url\""), "{out}");
        assert!(out.contains("import __oj_worker_1 from \"./w.ts?sharedworker&url\""), "{out}");
        assert!(out.contains("new Worker(__oj_worker_0, { type: \"module\" })"), "{out}");
        assert!(out.contains("new SharedWorker(__oj_worker_1)"), "{out}");
        assert!(!out.contains("w.ts?url"), "the worker entry is not a plain asset: {out}");
        let _ = std::fs::remove_dir_all(&d);
    }
    use super::*;
    use oxc_codegen::Codegen;

    fn expand_source(dir: &Path, src: &str) -> String {
        let allocator = crate::pooled_allocator();
        let parsed = Parser::new(&allocator, src, SourceType::mjs()).parse();
        let mut program = parsed.program;
        expand(&allocator, dir, &mut program);
        Codegen::new().build(&program).code
    }

    fn fixture_dir(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("oj-glob-{}-{label}", std::process::id()));
        let sub = dir.join("locales");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("en.json"), "{}").unwrap();
        std::fs::write(sub.join("fr.json"), "{}").unwrap();
        std::fs::write(sub.join("_ignore.json"), "{}").unwrap();
        dir
    }

    #[test]
    fn lazy_glob_expands_to_import_map() {
        let dir = fixture_dir("lazy");
        let out = expand_source(&dir, "const m = import.meta.glob('./locales/*.json');\n");
        assert!(
            out.contains(r#""./locales/en.json": () => import("./locales/en.json")"#),
            "{out}"
        );
        assert!(out.contains("fr.json"), "{out}");
        assert!(
            !out.contains("import.meta.glob"),
            "call must be replaced: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eager_glob_hoists_star_imports() {
        let dir = fixture_dir("eager");
        let out = expand_source(
            &dir,
            "const m = import.meta.glob('./locales/*.json', { eager: true });\n",
        );
        assert!(out.contains("import * as __oj_glob_0"), "{out}");
        assert!(out.contains(r#""./locales/en.json": __oj_glob_"#), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_base_option_keys_relative_to_base_and_imports_relative_to_importer() {
        let dir = fixture_dir("base");
        std::fs::create_dir_all(dir.join("src/pages")).unwrap();
        // Importer in src/pages, base "../../locales" (a sibling of src).
        let out = expand_source(
            &dir.join("src/pages"),
            "const m = import.meta.glob('./*.json', { base: '../../locales' });\n",
        );
        assert!(
            out.contains(r#""./en.json": () => import("../../locales/en.json")"#),
            "key relative to base, import relative to importer: {out}"
        );
        // A base under the importer directory.
        let out = expand_source(&dir, "const m = import.meta.glob('./*.json', { base: './locales', eager: true });\n");
        assert!(out.contains(r#"import * as __oj_glob_0 from "./locales/_ignore.json""#), "{out}");
        assert!(out.contains(r#""./_ignore.json": __oj_glob_0"#), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn glob_hides_dotfiles_and_node_modules_unless_exhaustive() {
        let dir = fixture_dir("exhaustive");
        std::fs::write(dir.join("locales/.hidden.json"), "{}").unwrap();
        std::fs::create_dir_all(dir.join("locales/node_modules/dep")).unwrap();
        std::fs::write(dir.join("locales/node_modules/dep/x.json"), "{}").unwrap();
        let out = expand_source(&dir, "const m = import.meta.glob('./locales/**/*.json');\n");
        assert!(out.contains("en.json"), "{out}");
        assert!(!out.contains(".hidden.json"), "dotfiles hidden by default: {out}");
        assert!(!out.contains("node_modules"), "node_modules hidden by default: {out}");
        let out = expand_source(&dir, "const m = import.meta.glob('./locales/**/*.json', { exhaustive: true });\n");
        assert!(out.contains(".hidden.json"), "{out}");
        assert!(out.contains("node_modules/dep/x.json"), "{out}");
        // A dot-led segment spelled out in the pattern is not hidden.
        let out = expand_source(&dir, "const m = import.meta.glob('./locales/.hidden.json');\n");
        assert!(out.contains(".hidden.json"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn dynamic_import_var_expands_to_switch() {
        let dir = fixture_dir("dynvar");
        let out = expand_dynamic_import_vars_source(
            "const load = (l) => import(`./locales/${l}.json`);\n",
            &dir.join("main.js"),
        );
        assert!(
            out.contains(r#"case "./locales/en.json": return import("./locales/en.json")"#),
            "{out}"
        );
        assert!(out.contains("fr.json"), "{out}");
        assert!(
            out.contains("${l}"),
            "original template kept as runtime arg: {out}"
        );
        assert!(out.contains("Unknown variable dynamic import"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn plain_string_dynamic_import_untouched() {
        let dir = fixture_dir("dynplain");
        let out = expand_dynamic_import_vars_source(
            "const m = import(\"./locales/en.json\");\n",
            &dir.join("main.js"),
        );
        assert!(
            !out.contains("switch"),
            "string-literal import left alone: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn negative_pattern_and_query_and_import() {
        let dir = fixture_dir("neg");
        let out = expand_source(
            &dir,
            "const m = import.meta.glob(['./locales/*.json', '!./locales/_*.json'], { query: 'raw', import: 'default' });\n",
        );
        assert!(
            !out.contains("_ignore"),
            "negative pattern must exclude: {out}"
        );
        assert!(out.contains("?raw"), "query appended: {out}");
        assert!(
            out.contains(r#".then((m) => m["default"])"#),
            "import pick: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn non_literal_pattern_is_left_untouched() {
        let dir = fixture_dir("nonlit");
        let out = expand_source(&dir, "const p = 'x'; const m = import.meta.glob(p);\n");
        assert!(
            out.contains("import.meta.glob(p)"),
            "dynamic arg must be left as-is: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_glob_yields_empty_object() {
        let dir = fixture_dir("empty");
        let out = expand_source(&dir, "const m = import.meta.glob('./nope/*.json');\n");
        assert!(
            out.contains("= {}"),
            "no matches must expand to an empty object: {out}"
        );
        assert!(
            !out.contains("import.meta.glob"),
            "call must be replaced: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn recursive_double_star_matches_nested_files() {
        let dir = std::env::temp_dir().join(format!("oj-glob-{}-recursive", std::process::id()));
        std::fs::create_dir_all(dir.join("content").join("posts")).unwrap();
        std::fs::write(dir.join("content").join("top.md"), "").unwrap();
        std::fs::write(dir.join("content").join("posts").join("nested.md"), "").unwrap();
        let out = expand_source(&dir, "const m = import.meta.glob('./content/**/*.md');\n");
        assert!(
            out.contains("./content/posts/nested.md"),
            "recursive match missing: {out}"
        );
        assert!(
            out.contains("./content/top.md"),
            "top-level match missing: {out}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
