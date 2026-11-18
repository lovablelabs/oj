// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    ExportAllDeclaration, ExportFromDeclaration, Expression, ImportDeclaration,
    ImportDeclarationSpecifier, ImportExpression, ModuleExportName, Statement,
};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_span::{SourceType, Span};

/// Cheap pre-gate for `rewrite_cjs_interop` on hot paths: the bare specifiers
/// this source may import, scanned without a parse (Vite's economy: its lexer
/// finds the imports, the full parse runs only on the ones being rewritten).
/// The caller probes each candidate against its interop mapping and skips the
/// parse when none maps. Tolerates everything legal between keyword and
/// specifier: Unicode whitespace, comments, and nested parens on `import(`.
/// Over-collection is harmless (the probe returns None).
pub fn bare_import_specifiers(source: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let bytes = source.as_bytes();
    for kw in ["from", "import"] {
        let mut at = 0;
        while let Some(pos) = source[at..].find(kw) {
            let i = at + pos;
            at = i + kw.len();
            if !is_keyword_start(bytes, i) {
                continue;
            }
            let s = skip_trivia(source, at, kw == "import");
            let Some(spec) = quoted_at(source, s) else {
                continue;
            };
            if !spec.is_empty()
                && !spec.starts_with('.')
                && !spec.starts_with('/')
                && !spec.contains('\\')
                && !out.iter().any(|o| o == spec)
            {
                out.push(spec.to_string());
            }
        }
    }
    out
}

/// A keyword match at `i`, not the tail of an identifier.
fn is_keyword_start(bytes: &[u8], i: usize) -> bool {
    i == 0
        || !(bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_' || bytes[i - 1] == b'$')
}

/// Skips whitespace and comments (and, when `parens`, any '(' runs) from byte
/// offset `i`; returns the offset of the next significant char.
fn skip_trivia(source: &str, mut i: usize, parens: bool) -> usize {
    loop {
        let rest = &source[i..];
        let Some(c) = rest.chars().next() else {
            return i;
        };
        if c.is_whitespace() {
            i += c.len_utf8();
        } else if rest.starts_with("//") {
            i += rest.find('\n').unwrap_or(rest.len());
        } else if let Some(body) = rest.strip_prefix("/*") {
            i += body.find("*/").map(|p| p + 4).unwrap_or(rest.len());
        } else if parens && c == '(' {
            i += 1;
        } else {
            return i;
        }
    }
}

/// The contents of a '...' or "..." string starting at byte offset `s`, or
/// of a template literal without a substitution or an escape (a literal
/// specifier to Vite's import analysis too).
fn quoted_at(source: &str, s: usize) -> Option<&str> {
    let quote = match source.as_bytes().get(s)? {
        b'"' => '"',
        b'\'' => '\'',
        b'`' => '`',
        _ => return None,
    };
    let body = &source[s + 1..];
    let text = body.find(quote).map(|end| &body[..end])?;
    (quote != '`' || !(text.contains("${") || text.contains('\\'))).then_some(text)
}

/// How Node reads the importing file, which decides Vite's interop of a
/// CommonJS dep flagged `__esModule` (importAnalysis.ts, `isNodeMode` and
/// `isNodeModeForDynamicImport`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImporterFormat {
    /// Vite's `isFilePathESM`: an `.mjs`/`.mts` file, or one under a nearest
    /// package.json with `"type": "module"`. A namespace import is then
    /// module.exports beside `default`, flagged or not.
    pub esm: bool,
    /// Vite's `isFilePathFormatExplicit`: also a `.cjs`/`.cts` file or one
    /// under `"type": "commonjs"`. The same, for a dynamic import.
    pub explicit: bool,
}

impl ImporterFormat {
    /// An ES module by its own extension or package.
    pub const ESM: Self = Self {
        esm: true,
        explicit: true,
    };
}

/// What serves a specifier that needs interop, which decides how the
/// CommonJS value is read off it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InteropKind {
    /// A pre-bundle, whose `default` export is module.exports: what Vite's
    /// optimizer writes and its rewrite imports (`import m from "url"`).
    PreBundle,
    /// A module oj serves itself (a CommonJS file wrapped file by file, a
    /// named-export facade), which carries module.exports as
    /// `__cjs_exports`. Vite has no such module.
    Served,
    /// The stub a Node builtin is externalized to. Vite's rewrite touches it
    /// only for a static import with named specifiers (importAnalysis.ts,
    /// the `browserExternalId` branch).
    BrowserExternal,
}

/// Where a specifier that needs interop is served.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InteropTarget {
    pub url: String,
    pub kind: InteropKind,
}

impl InteropTarget {
    pub fn new(url: impl Into<String>, kind: InteropKind) -> Self {
        Self {
            url: url.into(),
            kind,
        }
    }
}

pub fn rewrite_cjs_interop(
    source: &str,
    path: &Path,
    interop: &dyn Fn(&str) -> Option<InteropTarget>,
) -> Option<String> {
    rewrite_cjs_interop_logged(source, path, ImporterFormat::ESM, interop, &mut |_| {})
}

/// Like `rewrite_cjs_interop`, with a sink for the dev warnings the rewrite
/// cannot fix (Vite logs the same class from `transformCjsImport`).
pub fn rewrite_cjs_interop_logged(
    source: &str,
    path: &Path,
    format: ImporterFormat,
    interop: &dyn Fn(&str) -> Option<InteropTarget>,
    warn: &mut dyn FnMut(String),
) -> Option<String> {
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.fatal_error {
        return None;
    }

    let mut rewriter = InteropRewriter {
        interop,
        warn,
        path,
        source,
        format,
        edits: Vec::new(),
        hoisted: String::new(),
        idx: 0,
        needs_helper: false,
    };
    for stmt in &parsed.program.body {
        rewriter.statement(stmt);
    }
    let InteropRewriter {
        mut edits,
        hoisted,
        needs_helper,
        ..
    } = rewriter;

    // `import("cjs-dep")`: Vite wraps the promise so the awaited namespace reads
    // like the static-import interop (module.exports on `default`, its
    // properties as named members); otherwise `(await import("dep")).foo` reads
    // off the raw ESM wrapper namespace and is undefined.
    let mut dyn_edits = DynamicImportInterop {
        interop,
        source,
        node_mode: format.explicit,
        edits: Vec::new(),
        needs_helper: false,
    };
    dyn_edits.visit_program(&parsed.program);
    let needs_helper = dyn_edits.needs_helper || needs_helper;
    edits.extend(dyn_edits.edits);

    if edits.is_empty() {
        return None;
    }
    let mut result = apply_edits(source, edits);
    // Vite puts the bindings at the start of the file (interopNamedImports,
    // `appendLeft(getFileStartIndex(source))`: past a hashbang line, else
    // byte 0), so a function declared in the module and called above the
    // import statement finds them initialised. The same place here (see
    // `hoist_index` for the one exception), with no line added, the helper
    // first as the bindings call it. Every edit starts at or after it, so
    // its offset in the result is its offset in the source.
    let head = if needs_helper {
        format!("{DYN_INTEROP_HELPER}{hoisted}")
    } else {
        hoisted
    };
    result.insert_str(hoist_index(source, &parsed.program), &head);
    Some(result)
}

/// Where the hoisted bindings go: Vite's file start, unless a comment
/// before the module's first statement holds a JSX pragma (`@jsx`,
/// `@jsxImportSource`, `@jsxRuntime`, `@jsxFrag`). Vite's import analysis
/// runs after the JSX transform; here the transform runs on this pass's
/// output and reads a pragma from the comments before the first statement
/// (directives are not statements to it), so the bindings go right after the
/// last such comment: after `*/`, or at the start of the line after a `//`
/// comment.
fn hoist_index(source: &str, program: &oxc_ast::ast::Program) -> usize {
    let start = file_start_index(source);
    let Some(first) = program.body.first() else {
        return start;
    };
    let first = oxc_span::GetSpan::span(first).start as usize;
    program
        .comments
        .iter()
        .map(|comment| {
            (
                comment,
                comment.span.start as usize,
                comment.span.end as usize,
            )
        })
        .take_while(|(_, from, _)| *from < first)
        .filter(|(_, from, to)| *from >= start && source[*from..*to].contains("@jsx"))
        .last()
        .map_or(start, |(comment, _, to)| {
            if comment.is_line() {
                source[to..]
                    .find('\n')
                    .map_or(source.len(), |nl| to + nl + 1)
            } else {
                to
            }
        })
}

/// Vite's `getFileStartIndex`: the length of a leading hashbang line with
/// its line terminator (`hashbangRE`: `\r\n`, `\r`, `\n`, U+2028 or U+2029,
/// or none at the end of the file), else 0.
fn file_start_index(source: &str) -> usize {
    if !source.starts_with("#!") {
        return 0;
    }
    let is_terminator = |c: char| matches!(c, '\r' | '\n' | '\u{2028}' | '\u{2029}');
    match source.char_indices().find(|(_, c)| is_terminator(*c)) {
        None => source.len(),
        Some((at, '\r')) if source[at + 1..].starts_with('\n') => at + 2,
        Some((at, c)) => at + c.len_utf8(),
    }
}

struct Edit {
    start: usize,
    end: usize,
    text: String,
}

/// Applies non-overlapping edits back to front so earlier offsets stay valid.
fn apply_edits(source: &str, mut edits: Vec<Edit>) -> String {
    edits.sort_by_key(|e| std::cmp::Reverse(e.start));
    let mut result = source.to_string();
    for e in edits {
        result.replace_range(e.start..e.end, &e.text);
    }
    result
}

/// The line breaks of a replaced span, appended to its one-line replacement
/// so every later line keeps its number (Vite's `getLineBreaks`).
fn line_breaks(text: &str) -> String {
    "\n".repeat(text.matches('\n').count())
}

/// The top-level statement pass of `rewrite_cjs_interop_logged`. `idx`
/// numbers the `__ojns{n}` / `__ojcjs{n}` bindings in source order. A
/// statement is replaced in place by its import alone; the bindings read off
/// it collect in `hoisted`.
struct InteropRewriter<'r> {
    interop: &'r dyn Fn(&str) -> Option<InteropTarget>,
    warn: &'r mut dyn FnMut(String),
    path: &'r Path,
    source: &'r str,
    format: ImporterFormat,
    edits: Vec<Edit>,
    hoisted: String,
    idx: usize,
    needs_helper: bool,
}

impl InteropRewriter<'_> {
    fn next_idx(&mut self) -> usize {
        let n = self.idx;
        self.idx += 1;
        n
    }

    /// Replaces the statement at `span` with `text` on one line.
    fn push(&mut self, span: Span, text: String) {
        let (start, end) = (span.start as usize, span.end as usize);
        let text = text + &line_breaks(&self.source[start..end]);
        self.edits.push(Edit { start, end, text });
    }

    fn statement(&mut self, stmt: &Statement) {
        match stmt {
            Statement::ImportDeclaration(decl) => self.import(decl),
            Statement::ExportFromDeclaration(decl) => self.export_from(decl),
            Statement::ExportAllDeclaration(decl) => self.export_all(decl),
            _ => {}
        }
    }

    /// Vite's default import of a dep that needs interop (transformCjsImport):
    /// module.exports for an importer that is an ES module to Node, else the
    /// `default` of one flagged `__esModule`.
    fn default_of(&self, cjs: &str) -> String {
        if self.format.esm {
            cjs.to_string()
        } else {
            format!("(!{cjs}.__esModule ? {cjs} : {cjs}.default)")
        }
    }

    /// Replaces the statement at `span` with the import of `target` that
    /// binds `__ojcjs{n}`, the CommonJS value the hoisted bindings read.
    fn import_value(&mut self, span: Span, target: &InteropTarget, n: usize) {
        let url = json_str(&target.url);
        let (line, value) = import_of(target.kind, n, &url);
        self.hoisted.push_str(&value);
        self.push(span, line);
    }

    fn import(&mut self, decl: &ImportDeclaration) {
        if decl.import_kind.is_type() {
            return;
        }
        let Some(target) = (self.interop)(crate::lit_str(decl.source.value)) else {
            return;
        };
        let all: &[ImportDeclarationSpecifier] = decl
            .specifiers
            .as_ref()
            .map_or(&[], |specs| specs.as_slice());
        // An inline `type` specifier is erased by the TypeScript transform,
        // which runs after this pass (Vite's import analysis runs after it
        // and never sees one). A declaration of nothing else only has its
        // URL rewritten, for that transform to drop or keep as it would.
        let is_type = |spec: &&ImportDeclarationSpecifier| matches!(spec, ImportDeclarationSpecifier::ImportSpecifier(s) if s.import_kind.is_type());
        if !all.is_empty() && all.iter().all(|spec| is_type(&spec)) {
            self.next_idx();
            self.push(decl.source.span, json_str(&target.url));
            return;
        }
        let specs: Vec<&ImportDeclarationSpecifier> =
            all.iter().filter(|spec| !is_type(spec)).collect();
        let named = specs
            .iter()
            .any(|s| matches!(s, ImportDeclarationSpecifier::ImportSpecifier(_)));
        // Vite only rewrites the URL of a builtin's stub unless the statement
        // has named specifiers.
        if target.kind == InteropKind::BrowserExternal && !named {
            self.next_idx();
            self.push(decl.source.span, json_str(&target.url));
            return;
        }
        let n = self.next_idx();
        // Vite's `!node.specifiers.length`: `import "dep"`, `import {} from`.
        if specs.is_empty() {
            self.push(decl.span, format!("import {};", json_str(&target.url)));
            return;
        }
        let cjs = format!("__ojcjs{n}");
        self.import_value(decl.span, &target, n);
        let mut out = String::new();
        let mut names: Vec<String> = Vec::new();
        for spec in specs {
            match spec {
                ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                    out.push_str(&format!(
                        "const {} = {};",
                        s.local.name,
                        self.default_of(&cjs)
                    ));
                }
                // Vite's transformCjsImport: the namespace of a CommonJS dep
                // is module.exports spread beside `default`, so a name the
                // dep only assigns at runtime (a UMD wrapper) is a member.
                ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                    self.needs_helper = true;
                    out.push_str(&format!(
                        "const {} = __oj_dyn_interop({cjs}, {});",
                        s.local.name,
                        u8::from(self.format.esm)
                    ));
                }
                ImportDeclarationSpecifier::ImportSpecifier(s) => {
                    let imported = export_name(&s.imported);
                    if imported == "default" {
                        out.push_str(&format!(
                            "const {} = {};",
                            s.local.name,
                            self.default_of(&cjs)
                        ));
                    } else {
                        names.push(format!("{}: {}", json_key(&imported), s.local.name));
                    }
                }
            }
        }
        if !names.is_empty() {
            out.push_str(&format!("const {{ {} }} = {cjs};", names.join(", ")));
        }
        self.hoisted.push_str(&out);
    }

    /// `export { a, b as c } from "cjs"` and `export { default as X } from
    /// "cjs"`; without this the named bindings are undefined (the dep is a
    /// default-only ESM module).
    fn export_from(&mut self, decl: &ExportFromDeclaration) {
        if decl.export_kind.is_type() {
            return;
        }
        let Some(target) = (self.interop)(crate::lit_str(decl.source.value)) else {
            return;
        };
        let n = self.next_idx();
        // Vite's `!node.specifiers.length`: `export {} from "dep"`.
        if !decl.specifiers.iter().any(|s| !s.export_kind.is_type()) {
            self.push(decl.span, format!("import {};", json_str(&target.url)));
            return;
        }
        let cjs = format!("__ojcjs{n}");
        self.import_value(decl.span, &target, n);
        let mut out = String::new();
        let value_specs = decl.specifiers.iter().filter(|s| !s.export_kind.is_type());
        for (tmp, spec) in value_specs.enumerate() {
            let local = export_name(&spec.local);
            let value = if local == "default" {
                self.default_of(&cjs)
            } else {
                format!("{cjs}[{}]", json_str(&local))
            };
            let exported = json_key(&export_name(&spec.exported));
            let t = format!("__ojex{n}_{tmp}");
            out.push_str(&format!(
                "const {t} = {value};export {{ {t} as {exported} }};"
            ));
        }
        self.hoisted.push_str(&out);
    }

    /// `export * as ns from "cjs"` and `export * from "cjs"`: Vite's
    /// transformCjsImport returns nothing for an ExportAllDeclaration, so
    /// only the URL is rewritten. Of a pre-bundle that is its ES namespace:
    /// `ns` is its lone `default`, and the bare form re-exports nothing,
    /// which Vite warns of.
    ///
    /// A module oj serves itself has no counterpart in Vite: its ES
    /// namespace carries oj's own `__cjs_exports`, so `ns` is built from
    /// module.exports as a namespace import's is, and the bare form is left
    /// to re-export the names the wrapper could read.
    fn export_all(&mut self, decl: &ExportAllDeclaration) {
        if decl.export_kind.is_type() {
            return;
        }
        let Some(target) = (self.interop)(crate::lit_str(decl.source.value)) else {
            return;
        };
        if target.kind == InteropKind::BrowserExternal {
            self.next_idx();
            self.push(decl.source.span, json_str(&target.url));
            return;
        }
        let Some(exported) = &decl.exported else {
            let spec = decl.source.value.display().to_string();
            // The statement as Vite quotes it: up to its specifier.
            let statement = &self.source[decl.span.start as usize..decl.source.span.end as usize];
            if target.kind == InteropKind::PreBundle {
                // Vite's warning, in its words.
                (self.warn)(format!(
                    "Unable to interop `{statement}` in {}, this may lose module exports. Please export \"{spec}\" as ESM or use named exports instead, e.g. `export {{ A, B }} from \"{spec}\"`",
                    self.path.display(),
                ));
                self.next_idx();
                self.push(decl.source.span, json_str(&target.url));
            } else {
                (self.warn)(format!(
                    "`{statement}` in {} re-exports only the names oj could read off \"{spec}\"; exports it assigns at runtime are lost through a bare star re-export: use named exports, or pre-bundle the dep (optimizeDeps.include)",
                    self.path.display(),
                ));
            }
            return;
        };
        let n = self.next_idx();
        if target.kind == InteropKind::PreBundle {
            self.push(decl.source.span, json_str(&target.url));
            return;
        }
        self.needs_helper = true;
        self.import_value(decl.span, &target, n);
        self.hoisted.push_str(&format!(
            "const __ojex{n} = __oj_dyn_interop(__ojcjs{n}, {});export {{ __ojex{n} as {} }};",
            u8::from(self.format.esm),
            json_key(&export_name(exported)),
        ));
    }
}

/// The import of `url` that stands where the statement was, and the hoisted
/// binding of `__ojcjs{n}`, the CommonJS value (what `require()` returns).
///
/// A pre-bundle's is its `default` export, imported as Vite imports it: a
/// pre-bundle without one fails to link, and one whose `default` is `null`
/// or `undefined` is read as that. A module oj serves itself is imported as
/// a namespace and carries the value as `__cjs_exports`; one flagged for
/// interop that turns out to be an ES module is read off its `default`, or
/// as it is when it has none.
fn import_of(kind: InteropKind, n: usize, url: &str) -> (String, String) {
    match kind {
        InteropKind::PreBundle => (format!("import __ojcjs{n} from {url};"), String::new()),
        InteropKind::Served | InteropKind::BrowserExternal => (
            format!("import * as __ojns{n} from {url};"),
            format!(
                "const __ojcjs{n} = {};",
                served_value(&format!("__ojns{n}"))
            ),
        ),
    }
}

/// The CommonJS value of the namespace `ns` of a module oj serves itself.
fn served_value(ns: &str) -> String {
    format!(
        "(\"__cjs_exports\" in {ns} ? {ns}.__cjs_exports : {ns}.default !== undefined ? {ns}.default : {ns})"
    )
}

/// Vite's `interopHelper` (importAnalysis), for a namespace or dynamic
/// import of a dependency that needs interop: its CommonJS value `m` becomes
/// `default` and that value's own enumerable properties the other members
/// (none for an array or a primitive). With `n` unset (the importer is not
/// an ES module to Node, see [`ImporterFormat`]) a value flagged `__esModule`
/// is the namespace as it is, so its `default` is the package's own.
///
/// The helper sits in the module's own scope, where a top-level binding may
/// be called `Array` (`import { Array } from "effect"`), so it reaches
/// `isArray` through a literal's constructor and names no global.
const DYN_INTEROP_HELPER: &str = "const __oj_dyn_interop = (m, n) => n || !m?.__esModule ? { ...((typeof m === \"object\" && ![].constructor.isArray(m)) || typeof m === \"function\" ? m : {}), default: m } : m;";

struct DynamicImportInterop<'i> {
    interop: &'i dyn Fn(&str) -> Option<InteropTarget>,
    source: &'i str,
    /// Vite's `isNodeModeForDynamicImport`.
    node_mode: bool,
    edits: Vec<Edit>,
    needs_helper: bool,
}

impl<'a> Visit<'a> for DynamicImportInterop<'_> {
    fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
        // A template literal without a substitution is a literal to Vite
        // (importAnalysis.ts, `templateLiteralRE`).
        let spec = match &it.source {
            Expression::StringLiteral(lit) => Some(crate::lit_str(lit.value).to_string()),
            Expression::TemplateLiteral(tpl) if tpl.expressions.is_empty() => tpl
                .quasis
                .first()
                .and_then(|q| q.value.cooked)
                .map(|cooked| crate::lit_str(cooked).to_string()),
            _ => None,
        };
        let target = spec.and_then(|spec: String| (self.interop)(&spec));
        // Vite rewrites only the URL of a builtin's dynamic import.
        if let Some(target) = target
            .as_ref()
            .filter(|target| target.kind == InteropKind::BrowserExternal)
        {
            let span = oxc_span::GetSpan::span(&it.source);
            self.edits.push(Edit {
                start: span.start as usize,
                end: span.end as usize,
                text: json_str(&target.url),
            });
            return;
        }
        if let Some(target) = target {
            let (start, end) = (it.span.start as usize, it.span.end as usize);
            let value = match target.kind {
                InteropKind::PreBundle => "m.default".to_string(),
                _ => served_value("m"),
            };
            self.needs_helper = true;
            self.edits.push(Edit {
                start,
                end,
                text: format!(
                    "import({}).then((m) => __oj_dyn_interop({value}, {})){}",
                    json_str(&target.url),
                    u8::from(self.node_mode),
                    line_breaks(&self.source[start..end])
                ),
            });
            return;
        }
        walk::walk_import_expression(self, it);
    }
}

fn export_name(n: &ModuleExportName) -> String {
    match n {
        ModuleExportName::IdentifierName(i) => i.name.to_string(),
        ModuleExportName::IdentifierReference(i) => i.name.to_string(),
        ModuleExportName::StringLiteral(l) => crate::lit_str(l.value).to_string(),
    }
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_key(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_' || c == '$')
            .unwrap_or(false)
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        s.to_string()
    } else {
        json_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URL: &str = "/@oj-deps/cjs-dep.mjs";

    fn target(kind: InteropKind) -> impl Fn(&str) -> Option<InteropTarget> {
        move |spec: &str| (spec == "cjs-dep").then(|| InteropTarget::new(URL, kind))
    }

    fn run_as(src: &str, kind: InteropKind, format: ImporterFormat) -> Option<String> {
        rewrite_cjs_interop_logged(src, Path::new("m.js"), format, &target(kind), &mut |_| {})
    }

    /// A pre-bundle imported from an ES module.
    fn run(src: &str) -> String {
        run_as(src, InteropKind::PreBundle, ImporterFormat::ESM).unwrap()
    }

    fn served(src: &str) -> String {
        run_as(src, InteropKind::Served, ImporterFormat::ESM).unwrap()
    }

    const NOT_ESM: ImporterFormat = ImporterFormat {
        esm: false,
        explicit: false,
    };

    #[test]
    fn dynamic_import_of_a_cjs_dep_is_wrapped_with_the_namespace_interop() {
        let src = "const m = await import(\"cjs-dep\");\nconst n = import(\"esm-dep\");\nimport(`tpl`);\nimport(`cjs-${x}`);\n";
        let out = run(src);
        assert!(out.starts_with("const __oj_dyn_interop = "), "{out}");
        assert!(
            out.contains(&format!(
                "import(\"{URL}\").then((m) => __oj_dyn_interop(m.default, 1))"
            )),
            "{out}"
        );
        assert!(
            out.contains("import(\"esm-dep\")"),
            "untouched non-interop import: {out}"
        );
        assert!(
            out.contains("import(`tpl`)") && out.contains("import(`cjs-${x}`)"),
            "other template specifiers are left alone: {out}"
        );
        // Nothing to interop: no rewrite, no helper.
        assert!(rewrite_cjs_interop(src, Path::new("a.ts"), &|_| None).is_none());
    }

    /// Vite's `templateLiteralRE`: a template literal without a substitution
    /// names its module as a string literal does.
    #[test]
    fn a_template_literal_without_a_substitution_is_a_literal_specifier() {
        let out = run("const m = import(`cjs-dep`);\n");
        assert!(
            out.contains(&format!(
                "import(\"{URL}\").then((m) => __oj_dyn_interop(m.default, 1))"
            )),
            "{out}"
        );
    }

    /// Vite's `import m from "url"`: the pre-bundle's default export is
    /// module.exports, and a pre-bundle without one does not link.
    #[test]
    fn a_pre_bundle_is_imported_by_its_default_export() {
        let out = run(r#"import Foo from "cjs-dep";"#);
        assert_eq!(
            out,
            format!("const Foo = __ojcjs0;import __ojcjs0 from \"{URL}\";")
        );
    }

    /// A module oj serves itself carries module.exports as `__cjs_exports`.
    #[test]
    fn a_served_module_is_imported_as_a_namespace_and_read_off_cjs_exports() {
        let out = served(r#"import Foo, { a } from "cjs-dep";"#);
        assert!(
            out.starts_with(
                "const __ojcjs0 = (\"__cjs_exports\" in __ojns0 ? __ojns0.__cjs_exports : __ojns0.default !== undefined ? __ojns0.default : __ojns0);const Foo = __ojcjs0;const { a: a } = __ojcjs0;"
            ),
            "{out}"
        );
        assert!(
            out.ends_with(&format!("import * as __ojns0 from \"{URL}\";")),
            "{out}"
        );
        let out = served("const m = import(\"cjs-dep\");");
        assert!(
            out.contains(".then((m) => __oj_dyn_interop((\"__cjs_exports\" in m ? m.__cjs_exports : m.default !== undefined ? m.default : m), 1))"),
            "{out}"
        );
    }

    /// Vite's `!node.specifiers.length`.
    #[test]
    fn an_import_or_re_export_of_nothing_is_a_side_effect_import() {
        for src in [
            r#"import {} from "cjs-dep";"#,
            r#"export {} from "cjs-dep";"#,
        ] {
            assert_eq!(run(src), format!("import \"{URL}\";"), "{src}");
        }
    }

    /// The TypeScript transform runs after this pass and erases type-only
    /// imports; Vite's import analysis runs after it and never sees them.
    #[test]
    fn type_only_specifiers_are_left_to_the_typescript_transform() {
        let run = |src: &str| {
            rewrite_cjs_interop(src, Path::new("m.ts"), &target(InteropKind::PreBundle))
        };
        // Nothing but types: only the URL, no binding, no default import.
        assert_eq!(
            run(r#"import { type T, type U as V } from "cjs-dep";"#),
            Some(format!("import {{ type T, type U as V }} from \"{URL}\";"))
        );
        assert_eq!(run(r#"import type { T } from "cjs-dep";"#), None);
        assert_eq!(run(r#"export type { T } from "cjs-dep";"#), None);
        // A value beside a type: the value alone is bound.
        let out = run(r#"import d, { type T, a } from "cjs-dep";"#).unwrap();
        assert_eq!(
            out,
            format!(
                "const d = __ojcjs0;const {{ a: a }} = __ojcjs0;import __ojcjs0 from \"{URL}\";"
            )
        );
        // A value-syntax import that TypeScript would elide for being used
        // only as a type is not told apart here: it stays a binding, and
        // the import of the dep stays with it.
        let out = run("import { T } from \"cjs-dep\";\nlet x: T;\n").unwrap();
        assert!(out.starts_with("const { T: T } = __ojcjs0;"), "{out}");
    }

    /// The forms of a module oj serves itself read the same value as a
    /// pre-bundle's: the default by the importer, the namespace by the
    /// helper.
    #[test]
    fn a_served_module_follows_the_importer_and_the_helper_too() {
        let out = run_as(
            r#"import d, * as ns from "cjs-dep";"#,
            InteropKind::Served,
            NOT_ESM,
        )
        .unwrap();
        assert!(
            out.contains("const d = (!__ojcjs0.__esModule ? __ojcjs0 : __ojcjs0.default);"),
            "{out}"
        );
        assert!(
            out.contains("const ns = __oj_dyn_interop(__ojcjs0, 0);"),
            "{out}"
        );
        assert!(out.starts_with("const __oj_dyn_interop = "), "{out}");
    }

    #[test]
    fn named_imports_destructure_from_module_exports() {
        let out = run(r#"import { a, b as c } from "cjs-dep";"#);
        assert!(out.contains("const { a: a, b: c } = __ojcjs0;"), "{out}");
    }

    #[test]
    fn mixed_default_and_named() {
        let out = run(r#"import D, { x } from "cjs-dep";"#);
        assert!(out.contains("const D = __ojcjs0;"), "{out}");
        assert!(out.contains("const { x: x } = __ojcjs0;"), "{out}");
    }

    #[test]
    fn namespace_import_binds_module_exports() {
        let out = run(r#"import * as ns from "cjs-dep";"#);
        assert!(
            out.starts_with("const __oj_dyn_interop = "),
            "helper prepended: {out}"
        );
        assert!(
            out.contains("const ns = __oj_dyn_interop(__ojcjs0, 1);"),
            "{out}"
        );
    }

    /// Vite's `isNodeMode` for the namespace and `isNodeModeForDynamicImport`
    /// for `import()`; `export * as` of a pre-bundle is not rewritten.
    #[test]
    fn the_importer_format_sets_the_helper_flag_per_form() {
        let src = "import * as ns from \"cjs-dep\";\nexport * as re from \"cjs-dep\";\nconst m = import(\"cjs-dep\");\n";
        let run = |esm, explicit| {
            run_as(
                src,
                InteropKind::PreBundle,
                ImporterFormat { esm, explicit },
            )
            .unwrap()
        };
        let dynamic =
            |n: u8| format!("import(\"{URL}\").then((m) => __oj_dyn_interop(m.default, {n}))");
        // No `type` in the package: neither form is in Node mode.
        let out = run(false, false);
        assert!(
            out.contains("const ns = __oj_dyn_interop(__ojcjs0, 0);"),
            "{out}"
        );
        assert!(
            out.contains(&format!("export * as re from \"{URL}\";")),
            "{out}"
        );
        assert!(out.contains(&dynamic(0)), "{out}");
        // `"type": "commonjs"`: explicit, so the dynamic form alone is.
        let out = run(false, true);
        assert!(
            out.contains("const ns = __oj_dyn_interop(__ojcjs0, 0);"),
            "{out}"
        );
        assert!(out.contains(&dynamic(1)), "{out}");
        let out = run(true, true);
        assert!(
            out.contains("const ns = __oj_dyn_interop(__ojcjs0, 1);"),
            "{out}"
        );
        assert!(out.contains(&dynamic(1)), "{out}");
    }

    /// The helper names no global a module could shadow, and adds no line.
    #[test]
    fn interop_helper_names_no_shadowable_global() {
        for global in ["Object.", "Array.", "Reflect.", "globalThis"] {
            assert!(
                !DYN_INTEROP_HELPER.contains(global),
                "{global} in {DYN_INTEROP_HELPER}"
            );
        }
        assert!(!DYN_INTEROP_HELPER.contains('\n'));
    }

    #[test]
    fn side_effect_import_just_rewrites_specifier() {
        let out = run(r#"import "cjs-dep";"#);
        assert_eq!(out, format!("import \"{URL}\";"));
    }

    #[test]
    fn non_interop_and_type_imports_untouched() {
        assert!(run_as(
            r#"import x from "other";"#,
            InteropKind::PreBundle,
            ImporterFormat::ESM
        )
        .is_none());
        assert!(rewrite_cjs_interop(
            r#"import type T from "cjs-dep";"#,
            Path::new("m.ts"),
            &target(InteropKind::PreBundle)
        )
        .is_none());
    }

    #[test]
    fn string_named_import() {
        let out = run(r#"import { "weird-name" as w } from "cjs-dep";"#);
        assert!(
            out.contains(r#"const { "weird-name": w } = __ojcjs0;"#),
            "{out}"
        );
    }

    #[test]
    fn reexport_named_from_cjs() {
        let out = run(r#"export { a, b as c } from "cjs-dep";"#);
        assert!(
            out.ends_with(&format!("import __ojcjs0 from \"{URL}\";")),
            "{out}"
        );
        assert!(
            out.contains(r#"const __ojex0_0 = __ojcjs0["a"];export { __ojex0_0 as a };"#),
            "{out}"
        );
        assert!(
            out.contains(r#"const __ojex0_1 = __ojcjs0["b"];export { __ojex0_1 as c };"#),
            "{out}"
        );
    }

    #[test]
    fn reexport_from_non_interop_untouched() {
        assert!(run_as(
            r#"export { a } from "other";"#,
            InteropKind::PreBundle,
            ImporterFormat::ESM
        )
        .is_none());
    }

    #[test]
    fn local_export_without_source_untouched() {
        // `export { foo }` with no `from` is a local re-export, not a CJS dep.
        assert!(run_as(
            r#"const foo = 1; export { foo };"#,
            InteropKind::PreBundle,
            ImporterFormat::ESM
        )
        .is_none());
    }

    /// Vite's transformCjsImport returns nothing for `export * as`, so only
    /// the URL is rewritten: no helper, no binding.
    #[test]
    fn export_star_as_of_a_pre_bundle_only_rewrites_the_url() {
        let out = run("export * as geo from\n  \"cjs-dep\";\nmarker();\n");
        assert_eq!(
            out,
            format!("export * as geo from\n  \"{URL}\";\nmarker();\n")
        );
    }

    /// The ES namespace of a module oj serves itself carries `__cjs_exports`
    /// and names Vite's pre-bundle would not: `ns` is built from
    /// module.exports, as a namespace import's is.
    #[test]
    fn export_star_as_of_a_served_module_is_built_from_module_exports() {
        let out = served(r#"export * as geo from "cjs-dep";"#);
        assert!(out.starts_with("const __oj_dyn_interop = "), "{out}");
        assert!(
            out.contains(
                "const __ojex0 = __oj_dyn_interop(__ojcjs0, 1);export { __ojex0 as geo };"
            ),
            "{out}"
        );
        assert!(
            out.ends_with(&format!("import * as __ojns0 from \"{URL}\";")),
            "{out}"
        );
    }

    /// The default member in each of its forms, by the importer (Vite's
    /// transformCjsImport: `isNodeMode ? m : !m.__esModule ? m : m.default`).
    #[test]
    fn default_forms_follow_the_importer_format() {
        let run = |src: &str, format| run_as(src, InteropKind::PreBundle, format).unwrap();
        let own = "(!__ojcjs0.__esModule ? __ojcjs0 : __ojcjs0.default)";
        for (src, binding) in [
            (r#"import d from "cjs-dep";"#, "const d = "),
            (r#"import d, { a } from "cjs-dep";"#, "const d = "),
            (r#"import { default as d } from "cjs-dep";"#, "const d = "),
            (
                r#"export { default } from "cjs-dep";"#,
                "const __ojex0_0 = ",
            ),
            (
                r#"export { default as d } from "cjs-dep";"#,
                "const __ojex0_0 = ",
            ),
        ] {
            let out = run(src, ImporterFormat::ESM);
            assert!(out.contains(&format!("{binding}__ojcjs0;")), "{src}: {out}");
            let out = run(src, NOT_ESM);
            assert!(out.contains(&format!("{binding}{own};")), "{src}: {out}");
        }
        // A named member is read off module.exports either way.
        let out = run(r#"import d, { a } from "cjs-dep";"#, NOT_ESM);
        assert!(out.contains("const { a: a } = __ojcjs0;"), "{out}");
        let out = run(r#"export { a as default } from "cjs-dep";"#, NOT_ESM);
        assert!(out.contains(r#"const __ojex0_0 = __ojcjs0["a"];"#), "{out}");
    }

    /// Vite's `getFileStartIndex`: the bindings go at byte 0, or right after
    /// a hashbang line, in source order after the helper; every statement
    /// keeps its line.
    #[test]
    fn bindings_are_hoisted_to_the_file_start_and_lines_keep_their_numbers() {
        let src = "#!/usr/bin/env node\n\"use client\";\n// leading\ninit();\nimport {\n  a,\n  b as c,\n} from \"cjs-dep\";\nimport * as ns from \"cjs-dep\";\nconst m = await import(\n  \"cjs-dep\"\n);\nexport {\n  x,\n} from \"cjs-dep\";\nfunction init() { return ns.x; }\nmarker();\n";
        let out = run(src);
        let (before, after): (Vec<&str>, Vec<&str>) =
            (src.lines().collect(), out.lines().collect());
        assert_eq!(before.len(), after.len(), "{out}");
        // Untouched lines are where they were.
        for (i, line) in before.iter().enumerate() {
            if ["#!", "// leading", "init();", "function init", "marker()"]
                .iter()
                .any(|p| line.starts_with(p))
            {
                assert_eq!(after[i], *line, "line {}: {out}", i + 1);
            }
        }
        let head = after[1];
        assert!(head.ends_with("\"use client\";"), "{head}");
        let at = |needle: &str| {
            head.find(needle)
                .unwrap_or_else(|| panic!("{needle} in {head}"))
        };
        assert_eq!(at("const __oj_dyn_interop = "), 0);
        assert!(
            at("const { a: a, b: c } = __ojcjs0;")
                < at("const ns = __oj_dyn_interop(__ojcjs1, 1);")
        );
        assert!(at("const ns = ") < at("export { __ojex2_0 as x };"));
        assert!(at("export { __ojex2_0 as x };") < at("\"use client\";"));
        assert_eq!(after[4], format!("import __ojcjs0 from \"{URL}\";"));
        assert_eq!(after[8], format!("import __ojcjs1 from \"{URL}\";"));
        assert!(after[9].starts_with("const m = await import("), "{out}");
        assert_eq!(after[12], format!("import __ojcjs2 from \"{URL}\";"));
    }

    /// Byte 0 whatever the file starts with: a comment, a directive, a byte
    /// order mark, a decorator.
    #[test]
    fn the_bindings_start_the_file_whatever_it_starts_with() {
        let import = "import d from \"cjs-dep\";\n";
        let head = "const d = __ojcjs0;";
        for start in [
            "/** a comment */\n",
            "// @ts-nocheck\n",
            "\"use client\";\n",
            "'use strict'\n",
            "\u{feff}",
            "@dec export class X {}\n",
            "@dec\nexport default class {}\n",
        ] {
            let src = format!("{start}{import}");
            let out =
                rewrite_cjs_interop(&src, Path::new("m.tsx"), &target(InteropKind::PreBundle))
                    .unwrap_or_else(|| panic!("{src}"));
            assert_eq!(
                out,
                format!("{head}{start}import __ojcjs0 from \"{URL}\";\n"),
                "{src}"
            );
        }
    }

    /// The JSX transform, which runs on this pass's output, reads a pragma
    /// only from the comments before the first token: the bindings follow
    /// the last comment that holds one.
    #[test]
    fn the_bindings_follow_a_leading_jsx_pragma_comment() {
        let import = "import d from \"cjs-dep\";\n";
        let head = "const d = __ojcjs0;";
        let rewritten = format!("import __ojcjs0 from \"{URL}\";\n");
        for (before, after) in [
            ("/** @jsxImportSource custom */", "\n"),
            ("// license\n/* @jsx h */", " /* @other */\n"),
            ("// @jsxRuntime classic\n", "// later\n"),
            ("#!/usr/bin/env node\n/** @jsxFrag F */", "\n"),
            // After a directive prologue, a byte order mark, another
            // comment, all three.
            ("\"use client\";\n/** @jsxImportSource custom */", "\n"),
            ("\u{feff}/** @jsxImportSource custom */", "\n"),
            ("/* license */\n'use strict'\n// @jsx h\n", ""),
            (
                "#!/usr/bin/env node\n\"use client\";\n// note\n/** @jsxImportSource custom */",
                "\n",
            ),
        ] {
            let src = format!("{before}{after}{import}");
            let out =
                rewrite_cjs_interop(&src, Path::new("m.tsx"), &target(InteropKind::PreBundle))
                    .unwrap_or_else(|| panic!("{src}"));
            assert_eq!(out, format!("{before}{head}{after}{rewritten}"), "{src}");
        }
        // A pragma after the first statement is no pragma: byte 0.
        let src = format!("{import}/** @jsxImportSource custom */\n");
        let out = run(&src);
        assert!(out.starts_with(head), "{out}");
    }

    /// Vite's `hashbangRE`: any line terminator ends the hashbang line, and
    /// one that ends the file is the whole file.
    #[test]
    fn the_file_starts_after_the_hashbang_line_and_its_terminator() {
        for (source, at) in [
            ("#!/bin/node\nx", 12),
            ("#!/bin/node\r\nx", 13),
            ("#!/bin/node\rx", 12),
            ("#!/bin/node\u{2028}x", 14),
            ("#!/bin/node\u{2029}x", 14),
            ("#!/bin/node", 11),
            ("x\n#!", 0),
            ("", 0),
        ] {
            assert_eq!(file_start_index(source), at, "{source:?}");
        }
    }

    #[test]
    fn the_bindings_follow_a_hashbang_line() {
        let out = run("#!/usr/bin/env node\nconst m = await import(\"cjs-dep\");\n");
        assert!(
            out.starts_with("#!/usr/bin/env node\nconst __oj_dyn_interop"),
            "{out}"
        );
    }

    /// Vite rewrites the URL of a bare star re-export to the pre-bundle,
    /// whose only export is `default`, so nothing is re-exported, and warns.
    #[test]
    fn bare_export_star_of_a_pre_bundle_rewrites_the_url_and_warns() {
        let mut warnings = Vec::new();
        let out = rewrite_cjs_interop_logged(
            r#"export * from "cjs-dep";"#,
            Path::new("/app/m.js"),
            ImporterFormat::ESM,
            &target(InteropKind::PreBundle),
            &mut |w| warnings.push(w),
        );
        assert_eq!(out, Some(format!("export * from \"{URL}\";")));
        assert_eq!(
            warnings,
            ["Unable to interop `export * from \"cjs-dep\"` in /app/m.js, this may lose module exports. Please export \"cjs-dep\" as ESM or use named exports instead, e.g. `export { A, B } from \"cjs-dep\"`"]
        );
        // No warning when the source is not an interop candidate.
        warnings.clear();
        let out = rewrite_cjs_interop_logged(
            r#"export * from "./sib.js";"#,
            Path::new("m.js"),
            ImporterFormat::ESM,
            &target(InteropKind::PreBundle),
            &mut |w| warnings.push(w),
        );
        assert!(out.is_none() && warnings.is_empty());
    }

    /// A module oj serves itself re-exports the names its wrapper could
    /// read: left to the plain rewrite, with the warning.
    #[test]
    fn bare_export_star_of_a_served_module_is_left_alone_and_warns() {
        let mut warnings = Vec::new();
        let out = rewrite_cjs_interop_logged(
            r#"export * from "cjs-dep";"#,
            Path::new("m.js"),
            ImporterFormat::ESM,
            &target(InteropKind::Served),
            &mut |w| warnings.push(w),
        );
        assert!(out.is_none());
        assert_eq!(
            warnings,
            ["`export * from \"cjs-dep\"` in m.js re-exports only the names oj could read off \"cjs-dep\"; exports it assigns at runtime are lost through a bare star re-export: use named exports, or pre-bundle the dep (optimizeDeps.include)"]
        );
    }

    /// Vite's `browserExternalId` branch: a builtin's stub is interopped for
    /// a static import or re-export with named specifiers, and only has its
    /// URL rewritten (by the pass after this one) otherwise.
    #[test]
    fn a_builtin_stub_is_interopped_only_for_named_static_imports() {
        let run = |src: &str| run_as(src, InteropKind::BrowserExternal, ImporterFormat::ESM);
        for src in [
            r#"import fs from "cjs-dep";"#,
            r#"import * as fs from "cjs-dep";"#,
            r#"import "cjs-dep";"#,
            r#"const fs = await import("cjs-dep");"#,
            r#"export * as fs from "cjs-dep";"#,
            r#"export * from "cjs-dep";"#,
        ] {
            assert_eq!(
                run(src),
                Some(src.replace("\"cjs-dep\"", &format!("\"{URL}\""))),
                "{src}"
            );
        }
        for src in [
            r#"import { readFile } from "cjs-dep";"#,
            r#"import fs, { readFile } from "cjs-dep";"#,
            r#"export { readFile } from "cjs-dep";"#,
        ] {
            let out = run(src).unwrap_or_else(|| panic!("{src}"));
            assert!(
                out.ends_with(&format!("import * as __ojns0 from \"{URL}\";")),
                "{src}: {out}"
            );
            // The names are read off the stub's value, hoisted.
            assert!(
                out.starts_with("const __ojcjs0 = (\"__cjs_exports\" in __ojns0 ? "),
                "{src}: {out}"
            );
            assert!(
                out.contains("const { readFile: readFile } = __ojcjs0;")
                    || out.contains(r#"const __ojex0_0 = __ojcjs0["readFile"];"#),
                "{src}: {out}"
            );
        }
    }

    #[test]
    fn bare_import_specifiers_finds_every_candidate_position() {
        for (src, want) in [
            (r#"import { a } from "dep";"#, "dep"),
            ("import x from 'dep';", "dep"),
            (r#"export { a } from "dep";"#, "dep"),
            (r#"export * from "dep";"#, "dep"),
            (r#"export * as ns from "dep";"#, "dep"),
            (r#"import "dep";"#, "dep"),
            (r#"const m = await import("dep");"#, "dep"),
            ("import(  'dep')", "dep"),
            // minified: no space between keyword and quote
            (r#"import{a}from"dep";"#, "dep"),
            (r#"import x from"node:path";"#, "node:path"),
            // legal trivia between the keyword and the specifier
            (
                r#"import(/* webpackChunkName: "geo" */ "geodesiclib")"#,
                "geodesiclib",
            ),
            ("import x from // eol\n 'dep';", "dep"),
            (r#"import(("dep"))"#, "dep"),
            ("import x from\u{00a0}'dep';", "dep"),
            // a template literal without a substitution is a literal
            ("import(`dep`)", "dep"),
        ] {
            let specs = bare_import_specifiers(src);
            assert!(
                specs.iter().any(|s| s == want),
                "expected {want} in {specs:?} for: {src}"
            );
        }
        // dedup across positions
        assert_eq!(
            bare_import_specifiers(r#"import a from "dep"; import b from "dep";"#),
            vec!["dep".to_string()]
        );
    }

    #[test]
    fn bare_import_specifiers_skips_relative_only_modules() {
        for src in [
            r#"import { a } from "./sib.js";"#,
            r#"import x from "../up.js";"#,
            r#"export * from "/abs.js";"#,
            r#"import("./dyn.js")"#,
            "const platform_from = \"linux\";",
            r#"const s = "written from ./here";"#,
            "export const a = 1;",
            "",
            // a template with a substitution or an escape names no module
            "import(`dep-${x}`)",
            "import(`de\\x70`)",
        ] {
            assert!(
                bare_import_specifiers(src).is_empty(),
                "expected none: {src}"
            );
        }
    }
}
