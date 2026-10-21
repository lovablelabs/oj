// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::{Component, Path, PathBuf};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, CallExpression, Expression, ImportExpression, NewExpression,
    ObjectPropertyKind, Program, PropertyKey, Statement, TemplateLiteral,
};
use oxc_ast_visit::{walk, walk_mut, Visit, VisitMut};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};

/// `root` is the app root (Vite's `config.root`) that `/` patterns resolve
/// against; see [`expand`].
pub fn expand_source(source: &str, path: &Path, root: Option<&Path>) -> String {
    if !source.contains("import.meta.glob") {
        return source.to_string();
    }
    rewrite_source(source, path, |allocator, _, program| {
        expand(allocator, path, root, program);
        true
    })
}

/// The `import.meta.glob` calls a module makes, resolved the way `expand`
/// resolves them (`<dir>/pages/*.tsx`, `<root>/src/*.ts`). The dev server's
/// watcher keeps their matchers per importer: a file created or deleted that
/// one matches changes what the glob expands to, so the importer is updated as
/// if it had been edited (Vite's importMetaGlob `hotUpdate`).
pub fn glob_patterns(source: &str, path: &Path, root: Option<&Path>) -> Vec<GlobCall> {
    if !source.contains("import.meta.glob") {
        return Vec::new();
    }
    let allocator = Allocator::default();
    let Some(program) = parse_program(&allocator, source, path) else {
        return Vec::new();
    };
    let mut collector = PatternCollector {
        dir: path.parent().unwrap_or(path),
        root,
        out: Vec::new(),
    };
    collector.visit_program(&program);
    collector.out
}

pub fn expand_new_url_asset_source(source: &str, path: &Path) -> String {
    if !source.contains("import.meta.url") {
        return source.to_string();
    }
    rewrite_source(source, path, |allocator, dir, program| {
        expand_new_url_asset(allocator, dir, program, source)
    })
}

pub fn expand_dynamic_import_vars_source(source: &str, path: &Path) -> String {
    if !source.contains("import(") {
        return source.to_string();
    }
    rewrite_source(source, path, |allocator, dir, program| {
        expand_dynamic_import_vars(allocator, dir, program, source)
    })
}

/// Parses `source` as the file at `path`, applies `rewrite` with the file's
/// directory, and prints the program when `rewrite` reports a change.
/// Unparsable or unchanged input comes back as is.
fn rewrite_source(
    source: &str,
    path: &Path,
    rewrite: impl for<'a> FnOnce(&'a Allocator, &Path, &mut Program<'a>) -> bool,
) -> String {
    let allocator = Allocator::default();
    let Some(mut program) = parse_program(&allocator, source, path) else {
        return source.to_string();
    };
    if rewrite(&allocator, path.parent().unwrap_or(path), &mut program) {
        oxc_codegen::Codegen::new().build(&program).code
    } else {
        source.to_string()
    }
}

fn parse_program<'a>(
    allocator: &'a Allocator,
    source: &'a str,
    path: &Path,
) -> Option<Program<'a>> {
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::mjs());
    let parsed = Parser::new(allocator, source, source_type).parse();
    (!parsed.fatal_error).then_some(parsed.program)
}

/// Parses a synthesized expression into the arena.
fn parse_expr<'a>(allocator: &'a Allocator, source: &str) -> Option<Expression<'a>> {
    let source: &'a str = allocator.alloc_str(source);
    let parsed = Parser::new(allocator, source, SourceType::mjs()).parse();
    match parsed.program.body.into_iter().next() {
        Some(Statement::ExpressionStatement(es)) => Some(es.unbox().expression),
        _ => None,
    }
}

/// Parses the synthesized import statements and puts them, in order, at the
/// top of `program`.
fn prepend_hoisted<'a>(allocator: &'a Allocator, program: &mut Program<'a>, hoisted: &[String]) {
    let src: &'a str = allocator.alloc_str(&hoisted.join("\n"));
    let parsed = Parser::new(allocator, src, SourceType::mjs()).parse();
    for (i, stmt) in parsed.program.body.into_iter().enumerate() {
        program.body.insert(i, stmt);
    }
}

fn is_relative(spec: &str) -> bool {
    spec.starts_with("./") || spec.starts_with("../")
}

/// The glob a relative template literal with substitutions implies, each
/// `${...}` becoming `*` (`./img/${n}.png` -> `./img/*.png`).
fn template_glob(tpl: &TemplateLiteral) -> Option<String> {
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
    is_relative(&pattern).then_some(pattern)
}

fn span_text(source: &str, span: Span) -> Option<&str> {
    source.get(span.start as usize..span.end as usize)
}

struct PatternCollector<'d> {
    dir: &'d Path,
    root: Option<&'d Path>,
    out: Vec<GlobCall>,
}

impl<'a> Visit<'a> for PatternCollector<'_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if is_import_meta_glob(call) {
            let patterns = call.arguments.first().and_then(collect_patterns);
            let opts = match call.arguments.get(1) {
                Some(arg) => collect_options(arg),
                None => Some(GlobOptions::default()),
            };
            if let Some(found) = patterns
                .zip(opts)
                .and_then(|(p, o)| GlobCall::resolve(&p, &o, self.dir, self.root))
            {
                self.out.push(found);
            }
        }
        walk::walk_call_expression(self, call);
    }
}

/// One `import.meta.glob` call as Vite matches files against it: its patterns
/// resolved to absolute globs (`!`-led for negatives, path prefixes
/// glob-escaped, Vite's globsResolved) and the options its matchers and walk
/// take. The Start client bundler writes the same shape (glob-scopes.json).
#[derive(Clone, Debug, PartialEq)]
pub struct GlobCall {
    pub patterns: Vec<String>,
    pub exhaustive: bool,
    pub case_sensitive: bool,
}

impl GlobCall {
    /// Vite's toAbsoluteGlob over a call's patterns, with `base` applied.
    /// `None` when `base` is root-led and no root is known.
    fn resolve(
        patterns: &[String],
        opts: &GlobOptions,
        dir: &Path,
        root: Option<&Path>,
    ) -> Option<GlobCall> {
        let base_dir = match opts.base.as_deref() {
            Some(b) => Some(match b.strip_prefix('/') {
                Some(rest) => join_normalized(&escaped(root?), rest),
                None => join_normalized(&escaped(dir), b),
            }),
            None => None,
        };
        let dir = base_dir.unwrap_or_else(|| escaped(dir));
        let root = root.map(escaped);
        Some(GlobCall {
            patterns: patterns
                .iter()
                .filter_map(|p| absolute_glob(p, &dir, root.as_deref()))
                .collect(),
            exhaustive: opts.exhaustive,
            case_sensitive: opts.case_sensitive,
        })
    }

    /// The shape glob-scopes.json holds (`{ patterns, exhaustive,
    /// caseSensitive }`); `None` when it is not one.
    pub fn from_json(v: &serde_json::Value) -> Option<GlobCall> {
        Some(GlobCall {
            patterns: v["patterns"]
                .as_array()?
                .iter()
                .map(|p| p.as_str().map(str::to_string))
                .collect::<Option<_>>()?,
            exhaustive: v["exhaustive"].as_bool().unwrap_or(false),
            case_sensitive: v["caseSensitive"].as_bool().unwrap_or(true),
        })
    }

    pub fn matcher(&self) -> GlobMatcher {
        GlobMatcher::new(self)
    }

    /// The files the call expands to, absolute and lexically normalized,
    /// sorted by path string as Vite sorts them: a walk from the patterns'
    /// common scan base (Vite's getCommonBase), kept where the matcher
    /// matches. Like tinyglobby with Vite's options, the walk enters neither
    /// `node_modules` nor a dot-led directory unless the call is exhaustive
    /// or a pattern spells a dot-led part.
    fn files(&self) -> Vec<PathBuf> {
        let matcher = self.matcher();
        let bases: Vec<String> = matcher.affirmed.iter().map(|a| a.base.clone()).collect();
        let Some(start) = common_base(&bases) else {
            return Vec::new();
        };
        let enter_hidden = self.exhaustive
            || matcher.affirmed.iter().any(|a| {
                a.pattern
                    .split('/')
                    .any(|seg| seg.starts_with('.') && seg != "." && seg != "..")
            });
        let mut found = Vec::new();
        let mut seen = std::collections::HashSet::new();
        walk_files(
            Path::new(&start),
            self.exhaustive,
            enter_hidden,
            &mut seen,
            &mut found,
        );
        let mut files = std::collections::BTreeMap::new();
        for file in found {
            let key = slash(&file);
            if matcher.is_match(&key) {
                files.insert(key, file);
            }
        }
        files.into_values().collect()
    }
}

/// A [`GlobCall`]'s matcher, as Vite's importMetaGlob builds one per call
/// (picomatch with `dot: !!exhaustive`, `nocase: !caseSensitive`,
/// `ignore: ['**/node_modules/**']` unless exhaustive, no extglobs): a file
/// matches when an affirmed pattern does and no negated one does. `*` and
/// `**` stop at a dot-led part the pattern does not spell, and a
/// `node_modules` part below the pattern's scan base is ignored.
pub struct GlobMatcher {
    affirmed: Vec<Affirmed>,
    negated: globset::GlobSet,
    exhaustive: bool,
}

struct Affirmed {
    glob: globset::GlobMatcher,
    /// pm.scan's base, unescaped: the literal directory the pattern starts in.
    base: String,
    pattern: String,
}

impl GlobMatcher {
    fn new(call: &GlobCall) -> GlobMatcher {
        let build = |p: &str| {
            globset::GlobBuilder::new(&picomatch_class_negation(p))
                .literal_separator(true)
                .backslash_escape(true)
                .empty_alternates(true)
                .case_insensitive(!call.case_sensitive)
                .build()
                .ok()
        };
        let mut affirmed = Vec::new();
        let mut negated = globset::GlobSetBuilder::new();
        for p in &call.patterns {
            match p.strip_prefix('!') {
                Some(neg) => {
                    if let Some(g) = build(neg) {
                        negated.add(g);
                    }
                }
                None => {
                    if let Some(g) = build(p) {
                        affirmed.push(Affirmed {
                            glob: g.compile_matcher(),
                            base: scan_base(p),
                            pattern: p.clone(),
                        });
                    }
                }
            }
        }
        GlobMatcher {
            affirmed,
            negated: negated
                .build()
                .unwrap_or_else(|_| globset::GlobSet::empty()),
            exhaustive: call.exhaustive,
        }
    }

    /// Whether `file` (absolute, `/`-separated) is in the call's expansion.
    pub fn is_match(&self, file: &str) -> bool {
        let affirmed = self.affirmed.is_empty()
            || self.affirmed.iter().any(|a| {
                a.glob.is_match(file)
                    && (self.exhaustive || {
                        let rel = file
                            .strip_prefix(a.base.as_str())
                            .and_then(|r| r.strip_prefix('/'))
                            .unwrap_or(file);
                        !hidden_by_default(rel, &a.pattern)
                    })
            });
        affirmed && !self.negated.is_match(file)
    }
}

/// picomatch reads `[^...]` as a negated class, globset only `[!...]`.
fn picomatch_class_negation(pattern: &str) -> std::borrow::Cow<'_, str> {
    if pattern.contains("[^") {
        std::borrow::Cow::Owned(pattern.replace("[^", "[!"))
    } else {
        std::borrow::Cow::Borrowed(pattern)
    }
}

/// pm.scan's base: the pattern up to the last `/` before its first unescaped
/// glob character, unescaped. A pattern with none names a file, and its
/// directory is the base (Vite's getCommonBase reads a dotted last part so).
fn scan_base(pattern: &str) -> String {
    let mut last_slash = None;
    let mut chars = pattern.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => {
                chars.next();
            }
            '*' | '?' | '[' | '{' => break,
            '/' => last_slash = Some(i),
            _ => {}
        }
    }
    unescape(last_slash.map_or("", |i| &pattern[..i]))
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// A literal path as a glob prefix: Vite's globSafePath (tinyglobby
/// escapePath), so a `[id]` or `(group)` directory matches itself.
fn escaped(path: &Path) -> PathBuf {
    let mut out = String::new();
    for c in slash(path).chars() {
        if matches!(
            c,
            '*' | '?' | '[' | ']' | '{' | '}' | '(' | ')' | '!' | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    PathBuf::from(out)
}

/// Vite's getCommonBase over the affirmed patterns' scan bases.
fn common_base(bases: &[String]) -> Option<String> {
    let first = bases.first()?;
    let parts: Vec<&str> = first.split('/').collect();
    let mut common = String::new();
    for i in 0..parts.len() {
        let candidate = parts[..=i].join("/");
        if bases
            .iter()
            .all(|b| *b == candidate || b.starts_with(&format!("{candidate}/")))
        {
            common = candidate;
        } else {
            break;
        }
    }
    Some(if common.is_empty() {
        "/".into()
    } else {
        common
    })
}

fn walk_files(
    dir: &Path,
    exhaustive: bool,
    enter_hidden: bool,
    seen: &mut std::collections::HashSet<PathBuf>,
    out: &mut Vec<PathBuf>,
) {
    // A symlinked directory is followed once (tinyglobby follows links).
    if !seen.insert(std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf())) {
        return;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !exhaustive && (name == "node_modules" || (name.starts_with('.') && !enter_hidden)) {
                continue;
            }
            walk_files(&path, exhaustive, enter_hidden, seen, out);
        } else if path.is_file() {
            out.push(path);
        }
    }
}

/// Expands every literal `import.meta.glob` in the module at `importer`, as
/// Vite's importMetaGlob does: `./` and `../` patterns resolve from the
/// importer's directory and `/` ones from `root` (`None`: no root is known,
/// and a `/` pattern matches nothing). Import specifiers are always relative
/// to the importer; keys are too when every pattern is relative, and
/// root-absolute (`/src/x.ts`) otherwise.
pub fn expand<'a>(
    allocator: &'a Allocator,
    importer: &Path,
    root: Option<&Path>,
    program: &mut Program<'a>,
) {
    let mut expander = GlobExpander {
        allocator,
        importer,
        dir: importer.parent().unwrap_or(importer),
        root,
        hoisted: Vec::new(),
        uid: 0,
    };
    expander.visit_program(program);
    if !expander.hoisted.is_empty() {
        prepend_hoisted(allocator, program, &expander.hoisted);
    }
}

struct GlobExpander<'a, 'd> {
    allocator: &'a Allocator,
    importer: &'d Path,
    dir: &'d Path,
    root: Option<&'d Path>,
    hoisted: Vec<String>,
    uid: usize,
}

struct GlobOptions {
    eager: bool,
    import: Option<String>,
    query: String,
    /// Vite's `base`: globs resolve against, and keys are relative to, this
    /// directory (`./x` / `../x` from the importer, `/x` from the root).
    base: Option<String>,
    /// Vite's `exhaustive`: also match dotfiles and `node_modules`.
    exhaustive: bool,
    /// Vite's `caseSensitive` (default true): false matches with nocase.
    case_sensitive: bool,
}

impl Default for GlobOptions {
    fn default() -> Self {
        GlobOptions {
            eager: false,
            import: None,
            query: String::new(),
            base: None,
            exhaustive: false,
            case_sensitive: true,
        }
    }
}

impl GlobOptions {
    /// The named export to pick, or None for the whole namespace (`"*"`).
    fn named_import(&self) -> Option<&str> {
        self.import.as_deref().filter(|name| *name != "*")
    }
}

/// `to` as an import specifier relative to `from_dir` (`./x` or `../x`),
/// both lexical paths under the same root.
fn relative_spec(from_dir: &Path, to: &Path) -> String {
    let from: Vec<_> = from_dir.components().collect();
    let to_parts: Vec<_> = to.components().collect();
    let common = from
        .iter()
        .zip(&to_parts)
        .take_while(|(a, b)| a == b)
        .count();
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

/// `dir/rel` normalized lexically (`.` and `..` folded); `rel` is relative.
fn join_normalized(dir: &Path, rel: &str) -> PathBuf {
    let mut out = normalize(dir);
    for c in Path::new(rel).components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
            other => out.push(other),
        }
    }
    out
}

/// `path` with `.` and `..` folded lexically.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in path.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Vite's toAbsoluteGlob: a `/` pattern joins the root, `./` and `../` ones
/// the directory they are relative to (the importer's, or `base`'s), and a
/// `**` one is matched from the root as it stands. `None` for a root-led
/// pattern without a root, or a form Vite hands to its resolver (an alias, a
/// bare package path), which this compiler has none of.
fn absolute_glob(pattern: &str, dir: &Path, root: Option<&Path>) -> Option<String> {
    let (pre, pattern) = match pattern.strip_prefix('!') {
        Some(p) => ("!", p),
        None => ("", pattern),
    };
    let abs = if let Some(rest) = pattern.strip_prefix('/') {
        join_normalized(root?, rest)
    } else if pattern.starts_with("**") {
        join_normalized(root?, pattern)
    } else if pattern.starts_with("./") || pattern.starts_with("../") {
        join_normalized(dir, pattern)
    } else {
        return None;
    };
    // Not `slash`: the prefix is already `/`-separated, and its backslashes
    // are glob escapes.
    Some(format!("{pre}{}", abs.to_string_lossy()))
}

/// Vite's resolvePaths key for `file` outside `base`: relative to the root,
/// `/`-led unless it climbs out of it.
fn root_key(root: &Path, file: &Path) -> String {
    let rel = relative_spec(root, file);
    match rel.strip_prefix("./") {
        Some(inside) => format!("/{inside}"),
        None => rel,
    }
}

impl<'a> GlobExpander<'a, '_> {
    fn build_replacement(&mut self, args: &[Argument<'a>]) -> Option<String> {
        let patterns = collect_patterns(args.first()?)?;
        let opts = match args.get(1) {
            Some(arg) => collect_options(arg)?,
            None => GlobOptions::default(),
        };

        // Vite's importMetaGlob resolvePaths: with `base`, globs resolve
        // against and keys are relative to the base directory; without it,
        // keys are importer-relative when every pattern is relative and
        // root-absolute otherwise. Import specifiers are importer-relative.
        let base_dir = match opts.base.as_deref() {
            Some(b) => Some(match b.strip_prefix('/') {
                Some(rest) => join_normalized(self.root?, rest),
                None => join_normalized(self.dir, b),
            }),
            None => None,
        };
        let is_relative = patterns
            .iter()
            .all(|p| p.starts_with('.') || p.starts_with('!'));
        let mut entries: Vec<String> = Vec::new();
        let files = GlobCall::resolve(&patterns, &opts, self.dir, self.root)?.files();
        let importer = normalize(self.importer);
        for file in files.into_iter().filter(|f| *f != importer) {
            let import_path = relative_spec(self.dir, &file);
            let key = match (&base_dir, self.root) {
                (Some(b), _) => relative_spec(b, &file),
                (None, _) if is_relative => import_path.clone(),
                (None, Some(root)) => root_key(root, &file),
                (None, None) => import_path.clone(),
            };
            let spec = format!("{import_path}{}", opts.query);
            let value = self.entry_value(&spec, &opts);
            entries.push(format!("{key:?}: {value}"));
        }
        Some(format!("({{{}}})", entries.join(", ")))
    }

    /// One map value: a hoisted import binding when eager, else a lazy loader.
    fn entry_value(&mut self, spec: &str, opts: &GlobOptions) -> String {
        let import = opts.named_import();
        if !opts.eager {
            return match import {
                Some(name) => format!("() => import({spec:?}).then((m) => m[{name:?}])"),
                None => format!("() => import({spec:?})"),
            };
        }
        let ident = format!("__oj_glob_{}", self.uid);
        self.uid += 1;
        self.hoisted.push(match import {
            Some(name) => format!("import {{ {name} as {ident} }} from {spec:?};"),
            None => format!("import * as {ident} from {spec:?};"),
        });
        ident
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
        if let Some(new_expr) = replacement.and_then(|r| parse_expr(self.allocator, &r)) {
            *expr = new_expr;
            return;
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
    prepend_hoisted(allocator, program, &v.hoisted);
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
        if id.name != "URL" || n.arguments.len() != 2 || !Self::is_import_meta_url(&n.arguments[1])
        {
            return None;
        }
        match n.arguments[0].as_expression()? {
            Expression::StringLiteral(spec) => {
                let spec = spec.value.as_str();
                is_relative(spec).then(|| UrlSpec::Literal(spec.to_string()))
            }
            Expression::TemplateLiteral(tpl) => {
                let pattern = template_glob(tpl)?;
                let arg = span_text(self.source, tpl.span)?.to_string();
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

    /// Hoists `import <ident> from "<spec>"` and returns the fresh binding.
    fn hoist_import(&mut self, prefix: &str, spec: String) -> String {
        let ident = format!("{prefix}_{}", self.uid);
        self.uid += 1;
        self.hoisted.push(format!("import {ident} from {spec:?};"));
        ident
    }

    /// Points a worker constructor's URL argument at a hoisted `?worker&url`
    /// import. Returns false when `n` is not such a constructor.
    fn rewrite_worker(&mut self, n: &mut NewExpression<'a>) -> bool {
        let Some((spec, kind)) = self.worker_url_spec(n) else {
            return false;
        };
        let ident = self.hoist_import("__oj_worker", format!("{spec}?{kind}&url"));
        if let Some(url_expr) = parse_expr(self.allocator, &ident) {
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
        true
    }

    /// Replacement source for an asset `new URL(...)`, hoisting its imports.
    /// None for a template that matches no file (left untouched).
    fn asset_replacement(&mut self, spec: UrlSpec) -> Option<String> {
        match spec {
            UrlSpec::Literal(spec) => {
                let ident = self.hoist_import("__oj_url", format!("{spec}?url"));
                Some(format!("new URL({ident}, import.meta.url)"))
            }
            UrlSpec::Template { pattern, arg } => {
                let matches = template_matches(self.dir, &pattern);
                if matches.is_empty() {
                    return None;
                }
                let entries: Vec<String> = matches
                    .iter()
                    .map(|key| {
                        let ident = self.hoist_import("__oj_url", format!("{key}?url"));
                        format!("{key:?}: {ident}")
                    })
                    .collect();
                // Unmatched at runtime: keep the original url, as Vite does.
                Some(format!(
                    "new URL((function (p) {{ var m = {{{}}}; return m[p] ?? p; }})({arg}), import.meta.url)",
                    entries.join(", ")
                ))
            }
        }
    }
}

impl<'a> VisitMut<'a> for NewUrlAsset<'a, '_, '_> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        if let Expression::NewExpression(n) = &mut *expr {
            if self.rewrite_worker(n) {
                return;
            }
        }
        let replacement = match &*expr {
            Expression::NewExpression(n) => self
                .asset_spec(n)
                .and_then(|spec| self.asset_replacement(spec)),
            _ => None,
        };
        if let Some(rep) = replacement {
            if let Some(e) = parse_expr(self.allocator, &rep) {
                *expr = e;
            }
            return;
        }
        walk_mut::walk_expression(self, expr);
    }
}

struct DynImportVars<'a, 'd, 's> {
    allocator: &'a Allocator,
    dir: &'d Path,
    source: &'s str,
    changed: bool,
}

impl<'a> DynImportVars<'a, '_, '_> {
    fn build(&self, imp: &ImportExpression<'a>) -> Option<String> {
        let Expression::TemplateLiteral(tpl) = &imp.source else {
            return None;
        };
        let pattern = template_glob(tpl)?;
        let matches = template_matches(self.dir, &pattern);
        if matches.is_empty() {
            return None;
        }
        let arg = span_text(self.source, tpl.span)?;
        let cases: String = matches
            .iter()
            .map(|key| format!("case {key:?}: return import({key:?});"))
            .collect();
        Some(format!(
            "(function (p) {{ switch (p) {{ {cases}default: return Promise.reject(new Error(\"Unknown variable dynamic import: \" + p)); }} }})({arg})"
        ))
    }
}

impl<'a> VisitMut<'a> for DynImportVars<'a, '_, '_> {
    fn visit_expression(&mut self, expr: &mut Expression<'a>) {
        if let Expression::ImportExpression(imp) = &*expr {
            if let Some(new_expr) = self.build(imp).and_then(|r| parse_expr(self.allocator, &r)) {
                *expr = new_expr;
                self.changed = true;
                return;
            }
        }
        walk_mut::walk_expression(self, expr);
    }
}

fn is_import_meta_glob(call: &CallExpression) -> bool {
    let Expression::StaticMemberExpression(member) = &call.callee else {
        return false;
    };
    member.property.name == "glob" && matches!(member.object, Expression::ImportMeta(_))
}

fn collect_patterns(arg: &Argument) -> Option<Vec<String>> {
    match arg.as_expression()? {
        Expression::StringLiteral(s) => Some(vec![s.value.to_string()]),
        Expression::ArrayExpression(arr) => arr
            .elements
            .iter()
            .map(|el| match el {
                ArrayExpressionElement::StringLiteral(s) => Some(s.value.to_string()),
                _ => None,
            })
            .collect(),
        _ => None,
    }
}

/// The literal `import.meta.glob` options; values of the wrong type are ignored.
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
        let string = match &p.value {
            Expression::StringLiteral(s) => Some(s.value.as_str()),
            _ => None,
        };
        let boolean = match &p.value {
            Expression::BooleanLiteral(b) => Some(b.value),
            _ => None,
        };
        match (key, string, boolean) {
            ("eager", _, Some(b)) => opts.eager = b,
            ("exhaustive", _, Some(b)) => opts.exhaustive = b,
            ("caseSensitive", _, Some(b)) => opts.case_sensitive = b,
            ("import", Some(s), _) => opts.import = Some(s.to_string()),
            ("base", Some(s), _) => opts.base = Some(s.to_string()),
            ("query", Some(q), _) if q.starts_with('?') => opts.query = q.to_string(),
            ("query", Some(q), _) => opts.query = format!("?{q}"),
            ("as", Some(s), _) => opts.query = format!("?{s}"),
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
                && !pattern_dots.iter().any(|p| {
                    globset::Glob::new(p).is_ok_and(|g| g.compile_matcher().is_match(seg))
                }))
    })
}

/// The files a relative template's glob matches (dynamic-import-vars and a
/// template `new URL`), as `dir`-relative keys spelled like the template
/// (`./x`, `../x`), which the runtime value is compared against verbatim.
fn template_matches(dir: &Path, pattern: &str) -> Vec<String> {
    let Ok(paths) = glob::glob(&dir.join(pattern).to_string_lossy()) else {
        return Vec::new();
    };
    let mut keys = std::collections::BTreeSet::new();
    for entry in paths.flatten() {
        if !entry.is_file() {
            continue;
        }
        let Ok(rel) = entry.strip_prefix(dir) else {
            continue;
        };
        let rel = slash(rel);
        keys.insert(if rel.starts_with('.') {
            rel
        } else {
            format!("./{rel}")
        });
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

    /// An app root with `files` written (empty contents).
    fn app(name: &str, files: &[&str]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("oj-glob-root-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for f in files {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, "export {};").unwrap();
        }
        root
    }

    // Vite's toAbsoluteGlob joins a `/` pattern to the root (not the
    // filesystem root), and resolvePaths keys a non-relative glob
    // root-absolute: generated server code that looks its one file up by
    // that key (`m['/src/x.ts']`) finds the loader. The import itself stays
    // importer-relative.
    #[test]
    fn a_root_absolute_glob_resolves_from_the_root_and_keys_root_absolute() {
        let root = app(
            "root-abs",
            &[
                "src/integrations/auth/factory.server.ts",
                "src/integrations/auth/middleware.ts",
            ],
        );
        let src = "export async function auth() {\n  \
                   const m = import.meta.glob('/src/integrations/auth/factory.server.ts');\n  \
                   return m['/src/integrations/auth/factory.server.ts'];\n}\n";
        let out = super::expand_source(
            src,
            &root.join("src/integrations/auth/middleware.ts"),
            Some(&root),
        );
        assert!(
            out.contains(
                r#""/src/integrations/auth/factory.server.ts": () => import("./factory.server.ts")"#
            ),
            "{out}"
        );
        assert!(!out.contains("import.meta.glob"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
    }

    // Vite's resolvePaths: keys are importer-relative only when EVERY pattern
    // is relative (`isRelative`); one `/` pattern keys them all root-absolute.
    #[test]
    fn mixed_relative_and_root_patterns_key_every_file_root_absolute() {
        let root = app(
            "root-mixed",
            &["src/a/one.ts", "src/b/two.ts", "src/index.ts"],
        );
        let out = super::expand_source(
            "const m = import.meta.glob(['./a/*.ts', '/src/b/*.ts']);",
            &root.join("src/index.ts"),
            Some(&root),
        );
        assert!(
            out.contains(r#""/src/a/one.ts": () => import("./a/one.ts")"#),
            "{out}"
        );
        assert!(
            out.contains(r#""/src/b/two.ts": () => import("./b/two.ts")"#),
            "{out}"
        );
        // All relative: importer-relative keys, as before.
        let out = super::expand_source(
            "const m = import.meta.glob('./a/*.ts');",
            &root.join("src/index.ts"),
            Some(&root),
        );
        assert!(
            out.contains(r#""./a/one.ts": () => import("./a/one.ts")"#),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Vite's toAbsoluteGlob: `base: '/x'` is the root's `x`; keys are relative
    // to the base, imports to the importer.
    #[test]
    fn a_root_led_base_resolves_from_the_root() {
        let root = app("root-base", &["src/pages/home.tsx", "src/app/main.ts"]);
        let out = super::expand_source(
            "const m = import.meta.glob('./*.tsx', { base: '/src/pages' });",
            &root.join("src/app/main.ts"),
            Some(&root),
        );
        assert!(
            out.contains(r#""./home.tsx": () => import("../pages/home.tsx")"#),
            "{out}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    // Vite's importMetaGlob filters `file !== id`: a module is never in its
    // own glob, relative or root-absolute.
    #[test]
    fn the_importer_is_left_out_of_its_own_glob() {
        let root = app("root-self", &["src/a.ts", "src/b.ts"]);
        for pattern in ["./*.ts", "/src/*.ts"] {
            let out = super::expand_source(
                &format!("const m = import.meta.glob('{pattern}');"),
                &root.join("src/a.ts"),
                Some(&root),
            );
            assert!(out.contains(r#"import("./b.ts")"#), "{pattern}: {out}");
            assert!(!out.contains(r#"import("./a.ts")"#), "{pattern}: {out}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // Negative `/` patterns resolve like positive ones (toAbsoluteGlob keeps
    // the `!`), and a root glob still hides node_modules and dot-led paths
    // unless `exhaustive` (tinyglobby's `dot: false` + node_modules ignore).
    #[test]
    fn root_globs_honor_negatives_and_hidden_paths() {
        let root = app(
            "root-neg",
            &[
                "src/x/keep.ts",
                "src/x/skip.ts",
                "src/x/.hidden/h.ts",
                "src/x/node_modules/n.ts",
                "src/main.ts",
            ],
        );
        let out = super::expand_source(
            "const m = import.meta.glob(['/src/x/**/*.ts', '!/src/x/skip.ts']);",
            &root.join("src/main.ts"),
            Some(&root),
        );
        assert!(out.contains(r#""/src/x/keep.ts""#), "{out}");
        for hidden in ["skip.ts", "h.ts", "n.ts"] {
            assert!(!out.contains(hidden), "{hidden}: {out}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    // No root known (a compile outside the dev server): a `/` pattern matches
    // nothing instead of globbing the filesystem root.
    #[test]
    fn without_a_root_a_root_pattern_matches_nothing() {
        let out = super::expand_source(
            "const m = import.meta.glob('/etc/*');",
            Path::new("/app/src/main.ts"),
            None,
        );
        assert!(out.contains("const m = {}"), "{out}");
    }

    // The watcher's patterns (Vite's importMetaGlob hotUpdate) resolve `/`
    // patterns from the root too, so a file created under one updates the
    // importer.
    #[test]
    fn glob_patterns_resolve_root_patterns_from_the_root() {
        let pats = glob_patterns(
            "const m = import.meta.glob(['/src/pages/*.tsx', './local/*.ts', '!/src/pages/x.tsx']);",
            Path::new("/app/src/main.ts"),
            Some(Path::new("/app")),
        );
        assert_eq!(
            pats,
            vec![GlobCall {
                patterns: vec![
                    "/app/src/pages/*.tsx".into(),
                    "/app/src/local/*.ts".into(),
                    "!/app/src/pages/x.tsx".into()
                ],
                exhaustive: false,
                case_sensitive: true,
            }]
        );
    }

    // What picomatch 4.0.5 answers for each case, with the matcher built the
    // way Vite 8.2.1's importMetaGlob builds one per call (noextglob,
    // `dot: !!exhaustive`, `nocase: !caseSensitive`, `**/node_modules/**`
    // ignored unless exhaustive, negatives subtracted).
    #[test]
    fn the_matcher_answers_what_vites_picomatch_matcher_answers() {
        let cases: &[(&[&str], &str, bool, bool, bool)] = &[
            (&["/a/src/*.json"], "/a/src/x.json", false, true, true),
            (&["/a/src/*.json"], "/a/src/d/x.json", false, true, false),
            (&["/a/c/**/x.json"], "/a/c/foox.json", false, true, false),
            (&["/a/c/**/x.json"], "/a/c/x.json", false, true, true),
            (&["/a/c/**/x.json"], "/a/c/d/e/x.json", false, true, true),
            (&["/a/x/[ab]/*.json"], "/a/x/a/m.json", false, true, true),
            (&["/a/x/[ab]/*.json"], "/a/x/c/m.json", false, true, false),
            (&["/a/x/[^a]/*.json"], "/a/x/b/m.json", false, true, true),
            (&["/a/x/[^a]/*.json"], "/a/x/a/m.json", false, true, false),
            (&["/a/x/{a,b}/*.json"], "/a/x/b/m.json", false, true, true),
            (&["/a/src/*.json"], "/a/src/.h.json", false, true, false),
            (&["/a/src/*.json"], "/a/src/.h.json", true, true, true),
            (
                &["/a/src/**/*.json"],
                "/a/src/.d/x.json",
                false,
                true,
                false,
            ),
            (&["/a/src/.d/*.json"], "/a/src/.d/x.json", false, true, true),
            (
                &["/a/src/**/*.js"],
                "/a/src/node_modules/p/i.js",
                false,
                true,
                false,
            ),
            (
                &["/a/src/**/*.js"],
                "/a/src/node_modules/p/i.js",
                true,
                true,
                true,
            ),
            (&["/a/src/Icon.svg"], "/a/src/icon.svg", false, false, true),
            (&["/a/src/Icon.svg"], "/a/src/icon.svg", false, true, false),
            (
                &["/a/src/*.ts", "!/a/src/skip.ts"],
                "/a/src/skip.ts",
                false,
                true,
                false,
            ),
            (
                &["/a/src/*.ts", "!/a/src/skip.ts"],
                "/a/src/keep.ts",
                false,
                true,
                true,
            ),
            (
                &["/a/src/*.ts", "!**/skip.ts"],
                "/a/src/skip.ts",
                false,
                true,
                false,
            ),
            (&[r"/a/\[id\]/*.ts"], "/a/[id]/x.ts", false, true, true),
            (&[r"/a/\(g\)/*.ts"], "/a/(g)/x.ts", false, true, true),
            (
                &["/a/.cfg/app/src/*.ts"],
                "/a/.cfg/app/src/x.ts",
                false,
                true,
                true,
            ),
        ];
        for (patterns, file, exhaustive, case_sensitive, want) in cases {
            let call = GlobCall {
                patterns: patterns.iter().map(|p| p.to_string()).collect(),
                exhaustive: *exhaustive,
                case_sensitive: *case_sensitive,
            };
            assert_eq!(
                call.matcher().is_match(file),
                *want,
                "{patterns:?} on {file}"
            );
        }
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
            None,
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
        let pats = glob_patterns(src, std::path::Path::new("/app/src/main.ts"), None);
        let patterns: Vec<&[String]> = pats.iter().map(|c| c.patterns.as_slice()).collect();
        assert_eq!(
            patterns,
            vec![
                &["/app/src/pages/*.tsx".to_string()][..],
                &[
                    "/app/src/img/**/*.png".to_string(),
                    "!/app/src/img/skip.png".to_string()
                ][..],
            ]
        );
        assert!(glob_patterns(
            "export const x = 1;",
            std::path::Path::new("/app/x.ts"),
            None
        )
        .is_empty());
        // The call's matcher matches the files `expand` would list (`*`
        // stops at `/`), and its negatives subtract.
        assert!(pats[0].matcher().is_match("/app/src/pages/About.tsx"));
        assert!(!pats[0].matcher().is_match("/app/src/pages/nested/Deep.tsx"));
        assert!(pats[1].matcher().is_match("/app/src/img/a/b.png"));
        assert!(!pats[1].matcher().is_match("/app/src/img/skip.png"));
    }

    #[test]
    fn template_literal_new_url_expands_to_a_glob_lookup() {
        let d = tmp("tpl");
        let src = "const n = 'a';\nexport const u = new URL(`./img/${n}.png`, import.meta.url);\n";
        let out = expand_new_url_asset_source(src, &d.join("main.js"));
        assert!(
            out.contains("import __oj_url_0 from \"./img/a.png?url\""),
            "{out}"
        );
        assert!(
            out.contains("import __oj_url_1 from \"./img/b.png?url\""),
            "{out}"
        );
        assert!(out.contains("\"./img/a.png\": __oj_url_0"), "{out}");
        assert!(
            out.contains("m[p] ?? p"),
            "unmatched keys fall back to the literal url: {out}"
        );
        assert!(
            out.contains("`./img/${n}.png`"),
            "the original template is the lookup key: {out}"
        );
        // A template that matches nothing is left alone.
        let none = expand_new_url_asset_source(
            "new URL(`./nope/${x}.png`, import.meta.url);",
            &d.join("main.js"),
        );
        assert!(
            none.contains("new URL(`./nope/${x}.png`, import.meta.url)"),
            "{none}"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn new_worker_with_import_meta_url_becomes_a_worker_url_import() {
        let d = tmp("worker");
        let src = "const w = new Worker(new URL(\"./w.ts\", import.meta.url), { type: \"module\" });\nconst s = new SharedWorker(new URL(\"./w.ts\", import.meta.url));\n";
        let out = expand_new_url_asset_source(src, &d.join("main.js"));
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
        let _ = std::fs::remove_dir_all(&d);
    }
    use super::*;
    use oxc_codegen::Codegen;

    fn expand_source(dir: &Path, src: &str) -> String {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, src, SourceType::mjs()).parse();
        let mut program = parsed.program;
        expand(&allocator, &dir.join("index.js"), None, &mut program);
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
        let out = expand_source(
            &dir,
            "const m = import.meta.glob('./*.json', { base: './locales', eager: true });\n",
        );
        assert!(
            out.contains(r#"import * as __oj_glob_0 from "./locales/_ignore.json""#),
            "{out}"
        );
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
        assert!(
            !out.contains(".hidden.json"),
            "dotfiles hidden by default: {out}"
        );
        assert!(
            !out.contains("node_modules"),
            "node_modules hidden by default: {out}"
        );
        let out = expand_source(
            &dir,
            "const m = import.meta.glob('./locales/**/*.json', { exhaustive: true });\n",
        );
        assert!(out.contains(".hidden.json"), "{out}");
        assert!(out.contains("node_modules/dep/x.json"), "{out}");
        // A dot-led segment spelled out in the pattern is not hidden.
        let out = expand_source(
            &dir,
            "const m = import.meta.glob('./locales/.hidden.json');\n",
        );
        assert!(out.contains(".hidden.json"), "{out}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // The dialect is picomatch's, as Vite expands it: braces and classes
    // expand, `**` spans whole parts only, `caseSensitive: false` folds
    // case, and the importer's own `[id]` directory is matched literally.
    #[test]
    fn globs_expand_in_picomatchs_dialect() {
        let root = app(
            "dialect",
            &[
                "x/a/m.json",
                "x/b/m.json",
                "x/c/m.json",
                "c/foox.json",
                "c/d/x.json",
                "img/Icon.svg",
                "routes/[id]/one.ts",
                "routes/[id]/two.ts",
            ],
        );
        let expand = |importer: &str, src: &str| {
            super::expand_source(src, &root.join(importer), Some(&root))
        };
        for pattern in ["./x/{a,b}/*.json", "./x/[ab]/*.json", "./x/[!c]/*.json"] {
            let out = expand(
                "main.ts",
                &format!("const m = import.meta.glob('{pattern}');"),
            );
            assert!(
                out.contains("./x/a/m.json") && out.contains("./x/b/m.json"),
                "{pattern}: {out}"
            );
            assert!(!out.contains("./x/c/m.json"), "{pattern}: {out}");
        }
        let out = expand("main.ts", "const m = import.meta.glob('./c/**/x.json');");
        assert!(
            out.contains("./c/d/x.json") && !out.contains("foox"),
            "{out}"
        );
        let out = expand(
            "main.ts",
            "const m = import.meta.glob('./img/icon.svg', { caseSensitive: false });",
        );
        assert!(out.contains("./img/Icon.svg"), "{out}");
        let out = expand(
            "routes/[id]/one.ts",
            "const m = import.meta.glob('./*.ts');",
        );
        assert!(out.contains("./two.ts"), "{out}");
        let _ = std::fs::remove_dir_all(&root);
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

    // Vite's toAbsoluteGlob leaves a `**` glob as it stands, matched from
    // the root, and keys it root-absolute (it is not a relative glob).
    #[test]
    fn a_double_star_glob_matches_from_the_root() {
        let root = app(
            "double-star",
            &[
                "src/main.ts",
                "src/docs/a.md",
                "docs/b.md",
                "node_modules/dep/c.md",
            ],
        );
        let importer = root.join("src/main.ts");
        let out = super::expand_source(
            "const m = import.meta.glob('**/*.md');\n",
            &importer,
            Some(&root),
        );
        assert!(
            out.contains(r#""/docs/b.md": () => import("../docs/b.md")"#),
            "{out}"
        );
        assert!(
            out.contains(r#""/src/docs/a.md": () => import("./docs/a.md")"#),
            "{out}"
        );
        assert!(
            !out.contains("node_modules"),
            "hidden unless exhaustive: {out}"
        );
        let none =
            super::expand_source("const m = import.meta.glob('**/*.md');\n", &importer, None);
        assert!(none.contains("const m = {}"), "no root, no matches: {none}");
    }

    // Vite sorts the matched paths as strings: `-` (0x2d) sorts before `/`.
    #[test]
    fn matches_sort_by_path_string_like_vite() {
        let root = app("sort", &["src/main.ts", "src/a/b.ts", "src/a-b/x.ts"]);
        let out = super::expand_source(
            "const m = import.meta.glob('/src/**/*.ts');\n",
            &root.join("src/main.ts"),
            Some(&root),
        );
        let dash = out.find("/src/a-b/x.ts").expect("a-b matched");
        let slash = out.find("/src/a/b.ts").expect("a/b matched");
        assert!(dash < slash, "a-b/x before a/b: {out}");
    }
}
