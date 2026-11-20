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

/// What the transforms that run on this pass's output will do with the
/// module's imports, so the pass leaves them the imports Vite's import
/// analysis would see after them.
#[derive(Clone, Debug, Default)]
pub struct LaterTransforms {
    /// TypeScript's `verbatimModuleSyntax` (the transform's
    /// `only_remove_type_imports`): an import written as a value stays
    /// though no value refers to it.
    pub keep_value_imports: bool,
    /// The classic JSX runtime, whose transform refers to its factories.
    pub jsx_classic: bool,
    /// The classic element factory (`React.createElement` when None).
    pub jsx_pragma: Option<String>,
    /// The classic fragment (`React.Fragment` when None).
    pub jsx_pragma_frag: Option<String>,
}

pub fn rewrite_cjs_interop(
    source: &str,
    path: &Path,
    interop: &dyn Fn(&str) -> Option<InteropTarget>,
) -> Option<String> {
    rewrite_cjs_interop_logged(
        source,
        path,
        ImporterFormat::ESM,
        &LaterTransforms::default(),
        &|_| true,
        interop,
        &mut |_| {},
    )
}

/// Like `rewrite_cjs_interop`, with a sink for the dev warnings the rewrite
/// cannot fix (Vite logs the same class from `transformCjsImport`).
pub fn rewrite_cjs_interop_logged(
    source: &str,
    path: &Path,
    format: ImporterFormat,
    later: &LaterTransforms,
    // Whether a specifier could need interop, answered without resolving or
    // registering anything: a module none of whose imports could is spared
    // the symbol pass that decides which bindings stay.
    may_need_interop: &dyn Fn(&str) -> bool,
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
        may_need_interop,
        warn,
        path,
        source,
        program: &parsed.program,
        // Symbols are read only off a parse without a diagnostic: a file
        // with one is reported by the compile, and keeps its bindings here.
        elides: source_type.is_typescript()
            && !later.keep_value_imports
            && parsed.diagnostics.is_empty(),
        typescript: source_type.is_typescript(),
        later,
        scoping: std::cell::OnceCell::new(),
        jsx_use: std::cell::OnceCell::new(),
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
    // import statement finds them initialised. The same place here, with no
    // line added, the helper first as the bindings call it. Every edit
    // starts at or after it, so its offset in the result is its offset in
    // the source.
    //
    // Vite's import analysis runs after the JSX transform; here the
    // transform runs on this pass's output and reads the file's JSX pragmas
    // from the comments before its first statement, which the bindings now
    // are. So the pragmas the file's own leading comments hold are restated
    // in one comment ahead of the bindings.
    let mut head = String::new();
    if !hoisted.is_empty() || needs_helper {
        head.push_str(&jsx_pragmas(source, &parsed.program).restated());
    }
    if needs_helper {
        head.push_str(DYN_INTEROP_HELPER);
    }
    head.push_str(&hoisted);
    result.insert_str(file_start_index(source), &head);
    Some(result)
}

/// Whether [`rewrite_cjs_interop_logged`] could rewrite, or ask its `interop`
/// about, a module whose parse is `program`: one with a value import, a
/// re-export or a literal dynamic import of a specifier that may need
/// interop. Where none may, the rewrite leaves the module as it is and
/// resolves and registers nothing, so skipping it changes nothing.
pub(crate) fn may_rewrite(
    program: &oxc_ast::ast::Program,
    source: &str,
    may_need_interop: &dyn Fn(&str) -> bool,
) -> bool {
    if imports_may_need_interop(program, may_need_interop) {
        return true;
    }
    if !names_import_call(source) {
        return false;
    }
    struct DynamicImports<'f> {
        may_need_interop: &'f dyn Fn(&str) -> bool,
        found: bool,
    }
    impl<'a> Visit<'a> for DynamicImports<'_> {
        fn visit_import_expression(&mut self, it: &ImportExpression<'a>) {
            self.found = self.found
                || literal_specifier(&it.source).is_some_and(|spec| (self.may_need_interop)(&spec));
            if !self.found {
                walk::walk_import_expression(self, it);
            }
        }
    }
    let mut imports = DynamicImports {
        may_need_interop,
        found: false,
    };
    imports.visit_program(program);
    imports.found
}

/// The static half of [`may_rewrite`]: the module's value imports and
/// re-exports.
pub(crate) fn imports_may_need_interop(
    program: &oxc_ast::ast::Program,
    may_need_interop: &dyn Fn(&str) -> bool,
) -> bool {
    program.body.iter().any(|stmt| {
        let (is_type, source) = match stmt {
            Statement::ImportDeclaration(decl) => (decl.import_kind.is_type(), &decl.source),
            Statement::ExportFromDeclaration(decl) => (decl.export_kind.is_type(), &decl.source),
            Statement::ExportAllDeclaration(decl) => (decl.export_kind.is_type(), &decl.source),
            _ => return false,
        };
        !is_type && may_need_interop(crate::lit_str(source.value))
    })
}

/// Whether `source` holds `import` followed by `(`, whitespace and comments
/// between them aside: every dynamic import does.
fn names_import_call(source: &str) -> bool {
    const KEYWORD: &str = "import";
    memchr::memmem::find_iter(source.as_bytes(), KEYWORD).any(|at| {
        let next = skip_trivia(source, at + KEYWORD.len(), false);
        source.as_bytes().get(next) == Some(&b'(')
    })
}

/// A dynamic import's specifier when it is a literal. A template literal
/// without a substitution is one to Vite (importAnalysis.ts,
/// `templateLiteralRE`).
fn literal_specifier(source: &Expression) -> Option<String> {
    match source {
        Expression::StringLiteral(lit) => Some(crate::lit_str(lit.value).to_string()),
        Expression::TemplateLiteral(tpl) if tpl.expressions.is_empty() => tpl
            .quasis
            .first()
            .and_then(|q| q.value.cooked)
            .map(|cooked| crate::lit_str(cooked).to_string()),
        _ => None,
    }
}

/// The JSX pragmas of a module, as oxc's transform reads them
/// (oxc_transformer 0.158, `jsx/comments.rs`: `update_options_with_comments`
/// over the comments `Transformer::build_with_scoping` hands it): from the
/// comments that start before the first statement's span, in a JSX source
/// only, every pragma of every comment, the last of a kind winning.
#[derive(Debug, Default, PartialEq, Eq)]
struct JsxPragmas {
    /// `@jsx`
    jsx: Option<String>,
    /// `@jsxFrag`
    frag: Option<String>,
    /// `@jsxRuntime classic` (true) or `automatic` (false); any other value
    /// is ignored.
    classic: Option<bool>,
    /// `@jsxImportSource`
    import_source: Option<String>,
}

impl JsxPragmas {
    /// One comment holding these pragmas, or nothing when there is none.
    fn restated(&self) -> String {
        let mut parts = Vec::new();
        if let Some(classic) = self.classic {
            parts.push(format!(
                "@jsxRuntime {}",
                if classic { "classic" } else { "automatic" }
            ));
        }
        for (keyword, value) in [
            ("@jsx", &self.jsx),
            ("@jsxFrag", &self.frag),
            ("@jsxImportSource", &self.import_source),
        ] {
            if let Some(value) = value {
                parts.push(format!("{keyword} {value}"));
            }
        }
        if parts.is_empty() {
            String::new()
        } else {
            format!("/* {} */", parts.join(" "))
        }
    }
}

fn jsx_pragmas(source: &str, program: &oxc_ast::ast::Program) -> JsxPragmas {
    let mut pragmas = JsxPragmas::default();
    let Some(first) = program
        .body
        .first()
        .filter(|_| program.source_type.is_jsx())
    else {
        return pragmas;
    };
    let first = oxc_span::GetSpan::span(first).start;
    for comment in program
        .comments
        .iter()
        .take_while(|comment| comment.span.start < first)
    {
        let mut text = comment.content_span().source_text(source);
        while let Some((keyword, value, rest)) = find_jsx_pragma(text) {
            text = rest;
            // A value from a `//` comment that would close the comment the
            // pragmas are restated in is not restated, so the transform
            // never sees it: it is no pragma here either.
            if value.contains("*/") {
                continue;
            }
            match keyword {
                "" => pragmas.jsx = Some(value.to_string()),
                "Frag" => pragmas.frag = Some(value.to_string()),
                "ImportSource" => pragmas.import_source = Some(value.to_string()),
                _ => match value {
                    "classic" => pragmas.classic = Some(true),
                    "automatic" => pragmas.classic = Some(false),
                    _ => {}
                },
            }
        }
    }
    pragmas
}

/// oxc's `find_jsx_pragma` (oxc_transformer 0.158, `jsx/comments.rs`), which
/// is private there: the next pragma in a comment's text, as the rest of its
/// keyword after `@jsx` (`""`, `"Runtime"`, `"ImportSource"`, `"Frag"`), its
/// value, and the text after it to search again.
///
/// `@` counts only at the start of the text or after a space, a tab, a line
/// break or `*` (so `` `@jsx h` `` in backticks is no pragma). The keyword
/// ends at a space or a tab; an unknown keyword is skipped. Spaces and tabs
/// may follow, then the value runs to the next ASCII whitespace; a line
/// break right after the keyword leaves no value, and the search ends.
fn find_jsx_pragma(mut text: &str) -> Option<(&str, &str, &str)> {
    let keyword;
    loop {
        let at = text.find('@')?;
        let preceded =
            at == 0 || matches!(text.as_bytes()[at - 1], b' ' | b'\t' | b'\r' | b'\n' | b'*');
        if !preceded || !text[at..].starts_with("@jsx") {
            text = &text[at + 1..];
            continue;
        }
        text = &text[at + 4..];
        let space = text.bytes().position(|b| matches!(b, b' ' | b'\t'))?;
        let word = &text[..space];
        text = &text[space + 1..];
        if matches!(word, "" | "Runtime" | "ImportSource" | "Frag") {
            keyword = word;
            break;
        }
    }
    let text = text.trim_start_matches([' ', '\t']);
    if text.is_empty() {
        return None;
    }
    let is_space = |b: u8| matches!(b, b' ' | b'\t' | b'\r' | b'\n' | 0x0B | 0x0C);
    let (value, rest) = match text.bytes().position(is_space) {
        Some(end) => (&text[..end], &text[end + 1..]),
        None => (text, ""),
    };
    (!value.is_empty()).then_some((keyword, value, rest))
}

/// Vite's `getFileStartIndex`: the length of a leading hashbang line with
/// its line terminator (`hashbangRE`: `\r\n`, `\r`, `\n`, U+2028 or U+2029,
/// or none at the end of the file), else 0.
fn file_start_index(source: &str) -> usize {
    if !source.starts_with("#!") {
        return 0;
    }
    next_line_start(source, 0).unwrap_or(source.len())
}

fn is_line_terminator(c: char) -> bool {
    matches!(c, '\r' | '\n' | '\u{2028}' | '\u{2029}')
}

/// The offset after the first line terminator at or after `from` (`\r\n`
/// as one).
fn next_line_start(source: &str, from: usize) -> Option<usize> {
    let (at, c) = source[from..]
        .char_indices()
        .find(|(_, c)| is_line_terminator(*c))?;
    let at = from + at;
    Some(if c == '\r' && source[at + 1..].starts_with('\n') {
        at + 2
    } else {
        at + c.len_utf8()
    })
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

/// The identifiers the JSX transform will refer to in a module that has
/// JSX, though the source does not: oxc's TypeScript transform keeps an
/// import of one (`is_jsx_imports`, and the references its classic JSX
/// transform creates first).
///
/// - The root of the `@jsx` pragma a leading comment gives, else `React`,
///   when the module has an element; of `@jsxFrag`, else `React`, when it
///   has a fragment. oxc keeps these under either runtime.
/// - Under the classic runtime (the config's, or a `@jsxRuntime classic`
///   comment), the roots of the configured `pragma` and `pragmaFrag`: an
///   element calls the first, a fragment both.
fn jsx_identifiers(
    source: &str,
    program: &oxc_ast::ast::Program,
    later: &LaterTransforms,
) -> Vec<String> {
    #[derive(Default)]
    struct Jsx {
        element: bool,
        fragment: bool,
    }
    impl<'a> Visit<'a> for Jsx {
        fn visit_jsx_element(&mut self, it: &oxc_ast::ast::JSXElement<'a>) {
            self.element = true;
            walk::walk_jsx_element(self, it);
        }
        fn visit_jsx_fragment(&mut self, it: &oxc_ast::ast::JSXFragment<'a>) {
            self.fragment = true;
            walk::walk_jsx_fragment(self, it);
        }
    }
    let mut jsx = Jsx::default();
    jsx.visit_program(program);
    if !jsx.element && !jsx.fragment {
        return Vec::new();
    }
    let pragmas = jsx_pragmas(source, program);
    let root = |path: &str| path.split('.').next().unwrap_or(path).to_string();
    let (comment_pragma, comment_frag) = (pragmas.jsx, pragmas.frag);
    let classic = pragmas.classic.unwrap_or(later.jsx_classic);
    let mut names = Vec::new();
    if jsx.element {
        names.push(root(comment_pragma.as_deref().unwrap_or("React")));
    }
    if jsx.fragment {
        names.push(root(comment_frag.as_deref().unwrap_or("React")));
    }
    if classic {
        let factory = comment_pragma.as_deref().or(later.jsx_pragma.as_deref());
        names.extend(classic_pragma_root(factory));
        if jsx.fragment {
            let fragment = comment_frag.as_deref().or(later.jsx_pragma_frag.as_deref());
            names.extend(classic_pragma_root(fragment));
        }
    }
    names
}

/// The identifier oxc's classic JSX transform refers to for a `pragma` or
/// `pragmaFrag` (oxc_transformer 0.158, `jsx/jsx_impl.rs`, `Pragma::parse`):
/// the root of a dotted path of identifier names that is no reserved word;
/// none for `this.x` and `import.meta.x`; and `React`, the default's root,
/// for no value, an empty one, or one that does not parse (`1x`, `class`).
fn classic_pragma_root(pragma: Option<&str>) -> Option<String> {
    use oxc_syntax::{identifier::is_identifier_name, keyword::is_reserved_keyword};
    let default = || Some("React".to_string());
    let Some(pragma) = pragma.filter(|pragma| !pragma.is_empty()) else {
        return default();
    };
    let mut parts = pragma.split('.');
    let root = parts.next().unwrap_or(pragma);
    let tail: Vec<&str> = parts.collect();
    let tail_names = |tail: &[&str]| tail.iter().all(|part| is_identifier_name(part));
    match root {
        "this" if tail_names(&tail) => None,
        "import" if tail.first() == Some(&"meta") && tail_names(&tail[1..]) => None,
        "this" | "import" => default(),
        _ if is_reserved_keyword(root) || !is_identifier_name(root) || !tail_names(&tail) => {
            default()
        }
        _ => Some(root.to_string()),
    }
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
    may_need_interop: &'r dyn Fn(&str) -> bool,
    warn: &'r mut dyn FnMut(String),
    path: &'r Path,
    source: &'r str,
    program: &'r oxc_ast::ast::Program<'r>,
    /// A TypeScript source whose transform, which runs after this pass,
    /// removes the import bindings no value refers to.
    elides: bool,
    typescript: bool,
    later: &'r LaterTransforms,
    /// The module's symbols and references, built for the first import that
    /// asks.
    scoping: std::cell::OnceCell<oxc_semantic::Scoping>,
    /// The identifiers the JSX transform will refer to.
    jsx_use: std::cell::OnceCell<Vec<String>>,
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

    /// Whether the TypeScript transform keeps the import binding `local`:
    /// one a value refers to (oxc's `has_value_reference`: any reference
    /// that is not a type position), or one the JSX transform refers to
    /// (see [`jsx_identifiers`]). Always, where that transform removes no
    /// value import.
    fn keeps(&self, local: &oxc_ast::ast::BindingIdentifier) -> bool {
        if !self.elides {
            return true;
        }
        let scoping = self.scoping.get_or_init(|| {
            oxc_semantic::SemanticBuilder::new()
                .build(self.program)
                .semantic
                .into_scoping()
        });
        let Some(symbol) = local.symbol_id.get() else {
            return true;
        };
        // oxc's `has_value_reference`: an import redeclared as a value
        // (`import { Status } from "dep"; const Status = ...`, legal when
        // the import is only a type) is shadowed, and removed. The transform
        // decides after its own passes have changed some symbols' flags, so
        // an `enum` or an instantiated `namespace` declared with the
        // import's name can come out the other way there; either way such a
        // module declares the name twice at runtime and does not load.
        if (scoping.symbol_flags(symbol) - oxc_semantic::SymbolFlags::Import).is_value() {
            return false;
        }
        if self.has_value_reference(scoping, symbol) {
            return true;
        }
        self.jsx_use
            .get_or_init(|| jsx_identifiers(self.source, self.program, self.later))
            .iter()
            .any(|name| name == local.name.as_str())
    }

    /// Whether a reference outside a type position refers to `symbol`. The
    /// right-hand side of `import X = ns.Y` refers to `ns` only while `X`
    /// itself is referred to as a value: the transform removes an alias
    /// used as a type alone, and its reference with it.
    fn has_value_reference(
        &self,
        scoping: &oxc_semantic::Scoping,
        symbol: oxc_semantic::SymbolId,
    ) -> bool {
        use oxc_ast::ast::{TSModuleReference, TSTypeName};
        scoping
            .get_resolved_reference_ids(symbol)
            .iter()
            .any(|&id| {
                if scoping.get_reference(id).is_type() {
                    return false;
                }
                // The alias whose right-hand side starts with this reference.
                let alias = self.program.body.iter().find_map(|stmt| {
                    let Statement::TSImportEqualsDeclaration(decl) = stmt else {
                        return None;
                    };
                    let mut name = match &decl.module_reference {
                        TSModuleReference::IdentifierReference(ident) => {
                            return (ident.reference_id.get() == Some(id)).then_some(&decl.id);
                        }
                        TSModuleReference::QualifiedName(qualified) => &qualified.left,
                        TSModuleReference::ExternalModuleReference(_) => return None,
                    };
                    loop {
                        match name {
                            TSTypeName::IdentifierReference(ident) => {
                                return (ident.reference_id.get() == Some(id)).then_some(&decl.id);
                            }
                            TSTypeName::QualifiedName(qualified) => name = &qualified.left,
                            TSTypeName::ThisExpression(_) => return None,
                        }
                    }
                });
                match alias.and_then(|alias| alias.symbol_id.get()) {
                    Some(alias) => self.has_value_reference(scoping, alias),
                    None => true,
                }
            })
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
        if decl.import_kind.is_type() || !(self.may_need_interop)(crate::lit_str(decl.source.value))
        {
            return;
        }
        let all: &[ImportDeclarationSpecifier] = decl
            .specifiers
            .as_ref()
            .map_or(&[], |specs| specs.as_slice());
        // The TypeScript transform runs after this pass, Vite's import
        // analysis after it: an inline `type` specifier is erased, and so is
        // a binding no value refers to (`import { T } from "dep"`, `T` used
        // only as a type, or not at all) unless `verbatimModuleSyntax` keeps
        // what is written as a value.
        let is_type = |spec: &&ImportDeclarationSpecifier| matches!(spec, ImportDeclarationSpecifier::ImportSpecifier(s) if s.import_kind.is_type());
        let specs: Vec<&ImportDeclarationSpecifier> = all
            .iter()
            .filter(|spec| !(self.typescript && is_type(spec)) && self.keeps(spec.local()))
            .collect();
        // With no binding left the transform removes the declaration (it
        // keeps `import "dep"` under `verbatimModuleSyntax`). It is left as
        // written for the transform to remove, and the dep is not asked
        // about: an import the browser never makes registers nothing.
        let emptied = !all.is_empty() && specs.is_empty();
        if emptied && !self.later.keep_value_imports {
            return;
        }
        let Some(target) = (self.interop)(crate::lit_str(decl.source.value)) else {
            return;
        };
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
        let target = literal_specifier(&it.source).and_then(|spec| (self.interop)(&spec));
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
        rewrite_cjs_interop_logged(
            src,
            Path::new("m.js"),
            format,
            &LaterTransforms::default(),
            &|_| true,
            &target(kind),
            &mut |_| {},
        )
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

    fn run_ts(src: &str, file: &str, later: &LaterTransforms) -> Option<String> {
        rewrite_cjs_interop_logged(
            src,
            Path::new(file),
            ImporterFormat::ESM,
            later,
            &|_| true,
            &target(InteropKind::PreBundle),
            &mut |_| {},
        )
    }

    /// oxc's TypeScript transform, which runs after this pass, removes an
    /// import binding no value refers to and the declaration when none is
    /// left (`has_value_reference`); Vite's import analysis runs on what
    /// remains. A declaration that will be removed is left as written, for
    /// that transform to remove: nothing is spliced, and the dep is not
    /// asked about.
    #[test]
    fn a_declaration_typescript_removes_is_left_to_it() {
        let later = LaterTransforms::default();
        for src in [
            "import { type T, type U as V } from \"cjs-dep\";\n",
            "import type { T } from \"cjs-dep\";\n",
            "export type { T } from \"cjs-dep\";\n",
            "import { T } from \"cjs-dep\";\nlet x: T;\n",
            "import { T } from \"cjs-dep\";\nexport type { T };\n",
            "import d, { T } from \"cjs-dep\";\nlet x: T | typeof d;\n",
            "import * as ns from \"cjs-dep\";\nlet x: ns.T;\n",
            "import { unused } from \"cjs-dep\";\nlet x = 1;\n",
            // Shadowed in the only scope that uses the name.
            "import { a } from \"cjs-dep\";\nfunction f(a: number) { return a; }\n",
            // No statement is joined to the next: the text is as it was.
            "\"use client\"\nimport { FC } from \"cjs-dep\"\n(async () => {})()\n",
            "import { FC } from \"cjs-dep\"\n[1].map(String)\n",
            "import { FC } from \"cjs-dep\"\n`tpl`.length\n",
        ] {
            assert_eq!(run_ts(src, "m.ts", &later), None, "{src}");
        }
        // The dep of a removed declaration is not looked up.
        let asked = std::cell::Cell::new(0);
        let out = rewrite_cjs_interop_logged(
            "import { T } from \"cjs-dep\";\nlet x: T;\n",
            Path::new("m.ts"),
            ImporterFormat::ESM,
            &later,
            &|_| true,
            &|_| {
                asked.set(asked.get() + 1);
                Some(InteropTarget::new(URL, InteropKind::PreBundle))
            },
            &mut |_| {},
        );
        assert_eq!((out, asked.get()), (None, 0));
    }

    /// A binding a value refers to is read; one beside it that none does is
    /// not.
    #[test]
    fn only_the_bindings_a_value_refers_to_are_read() {
        let later = LaterTransforms::default();
        let import = format!("import __ojcjs0 from \"{URL}\";");
        for (src, bindings) in [
            (
                "import { a, T } from \"cjs-dep\";\nlet x: T = a;\n",
                "const { a: a } = __ojcjs0;",
            ),
            (
                "import d, { T } from \"cjs-dep\";\nlet x: T = d;\n",
                "const d = __ojcjs0;",
            ),
            (
                "import d, { type T, a } from \"cjs-dep\";\nd(a);\n",
                "const d = __ojcjs0;const { a: a } = __ojcjs0;",
            ),
            // A class is a value and a type.
            (
                "import { C } from \"cjs-dep\";\nlet x: C = new C();\n",
                "const { C: C } = __ojcjs0;",
            ),
            // Re-exported, exported as default, in a shorthand property.
            (
                "import { a } from \"cjs-dep\";\nexport { a };\n",
                "const { a: a } = __ojcjs0;",
            ),
            (
                "import a from \"cjs-dep\";\nexport default a;\n",
                "const a = __ojcjs0;",
            ),
            (
                "import { a } from \"cjs-dep\";\nexport const o = { a };\n",
                "const { a: a } = __ojcjs0;",
            ),
            // Shadowed in an inner scope, used in the outer one.
            (
                "import { a } from \"cjs-dep\";\nfunction f(a: number) { return a; }\nf(a);\n",
                "const { a: a } = __ojcjs0;",
            ),
        ] {
            let rest = src.split_once('\n').unwrap().1;
            assert_eq!(
                run_ts(src, "m.ts", &later),
                Some(format!("{bindings}{import}\n{rest}")),
                "{src}"
            );
        }
        // JavaScript elides nothing.
        assert_eq!(
            run_ts("import { unused } from \"cjs-dep\";\n", "m.js", &later),
            Some(format!("const {{ unused: unused }} = __ojcjs0;{import}\n"))
        );
        // A file with a parse diagnostic keeps its bindings: its symbols
        // are not read off a recovered tree.
        // (A top-level `return` is a diagnostic the parser recovers from.)
        let src = "import { unused } from \"cjs-dep\";\nreturn;\n";
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, src, SourceType::ts()).parse();
        assert!(!parsed.fatal_error && !parsed.diagnostics.is_empty());
        let out = run_ts(src, "m.ts", &later).expect("rewritten");
        assert!(
            out.starts_with("const { unused: unused } = __ojcjs0;"),
            "{out}"
        );
    }

    /// tsconfig `verbatimModuleSyntax` (the transform's
    /// `only_remove_type_imports`): what is written as a value stays, an
    /// inline `type` goes, and a declaration of only those becomes a
    /// side-effect import.
    #[test]
    fn verbatim_module_syntax_keeps_value_imports() {
        let later = LaterTransforms {
            keep_value_imports: true,
            ..Default::default()
        };
        let import = format!("import __ojcjs0 from \"{URL}\";");
        assert_eq!(
            run_ts(
                "import { T, type U } from \"cjs-dep\";\nlet x: T;\n",
                "m.ts",
                &later
            ),
            Some(format!("const {{ T: T }} = __ojcjs0;{import}\nlet x: T;\n"))
        );
        assert_eq!(
            run_ts("import { unused } from \"cjs-dep\";\n", "m.ts", &later),
            Some(format!("const {{ unused: unused }} = __ojcjs0;{import}\n"))
        );
        assert_eq!(
            run_ts("import { type T } from \"cjs-dep\";\n", "m.ts", &later),
            Some(format!("import \"{URL}\";\n"))
        );
        assert_eq!(
            run_ts("import type { T } from \"cjs-dep\";\n", "m.ts", &later),
            None
        );
    }

    /// The JSX transform refers to its factories though the source does
    /// not; oxc's TypeScript transform keeps an import of one
    /// (`is_jsx_imports`, and the reference its classic transform creates).
    #[test]
    fn a_jsx_factory_import_is_kept_as_oxc_keeps_it() {
        let automatic = LaterTransforms::default();
        let classic = LaterTransforms {
            jsx_classic: true,
            jsx_pragma: Some("createElement".into()),
            jsx_pragma_frag: Some("Fragment".into()),
            ..Default::default()
        };
        let kept = |src: &str, file: &str, later: &LaterTransforms, binding: &str| {
            let out = run_ts(src, file, later);
            assert!(
                out.as_deref().is_some_and(|out| out.contains(binding)),
                "{src}: {out:?}"
            );
        };
        // The config's classic factory and fragment, used by the JSX alone.
        let src = "import { createElement, Fragment } from \"cjs-dep\";\nexport const A = () => <><b>x</b></>;\n";
        kept(
            src,
            "m.tsx",
            &classic,
            "const { createElement: createElement, Fragment: Fragment } = __ojcjs0;",
        );
        // An element without a fragment refers to the factory alone.
        let out = run_ts(
            "import { createElement, Fragment } from \"cjs-dep\";\nexport const A = () => <b>x</b>;\n",
            "m.tsx",
            &classic,
        )
        .unwrap();
        assert!(
            out.starts_with("const { createElement: createElement } = __ojcjs0;"),
            "{out}"
        );
        // Under the automatic runtime those names are nothing special.
        assert_eq!(run_ts(src, "m.tsx", &automatic), None);
        // A comment's pragma, under either runtime.
        kept(
            "/** @jsx h */\nimport { h } from \"cjs-dep\";\nexport const el = <p />;\n",
            "m.tsx",
            &automatic,
            "const { h: h } = __ojcjs0;",
        );
        kept(
            "/** @jsxRuntime classic */\n/** @jsx m.h */\nimport m from \"cjs-dep\";\nexport const el = <p />;\n",
            "m.tsx",
            &automatic,
            "const m = __ojcjs0;",
        );
        // oxc keeps an unused `React` beside an element under the automatic
        // runtime too (its TypeScript pragma defaults to React.createElement).
        kept(
            "import React from \"cjs-dep\";\nexport const el = <p />;\n",
            "m.tsx",
            &automatic,
            "const React = __ojcjs0;",
        );
        // Without JSX in the module, or in a `.ts` file, there is no such rule.
        assert_eq!(
            run_ts(
                "import React from \"cjs-dep\";\nexport const n = 1;\n",
                "m.tsx",
                &classic
            ),
            None
        );
        assert_eq!(
            run_ts(
                "import { createElement } from \"cjs-dep\";\nexport const n = 1;\n",
                "m.ts",
                &classic
            ),
            None
        );
        // An import used as an element is a value use.
        kept(
            "import { Foo } from \"cjs-dep\";\nexport const el = <Foo />;\n",
            "m.tsx",
            &automatic,
            "const { Foo: Foo } = __ojcjs0;",
        );
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
                rewrite_cjs_interop(&src, Path::new("m.jsx"), &target(InteropKind::PreBundle))
                    .unwrap_or_else(|| panic!("{src}"));
            assert_eq!(
                out,
                format!("{head}{start}import __ojcjs0 from \"{URL}\";\n"),
                "{src}"
            );
        }
    }

    /// The JSX transform, which runs on this pass's output, reads the pragmas
    /// of the comments before the first statement: the bindings are that
    /// statement now, at Vite's position, so the file's pragmas are restated
    /// in a comment ahead of them.
    #[test]
    fn the_pragmas_of_a_jsx_file_are_restated_ahead_of_the_bindings() {
        let import = "import d from \"cjs-dep\";\nd;\n";
        let head = "const d = __ojcjs0;";
        let rewritten = format!("import __ojcjs0 from \"{URL}\";\nd;\n");
        for (before, restated) in [
            (
                "/** @jsxImportSource custom */\n",
                "/* @jsxImportSource custom */",
            ),
            ("// license\n/* @jsx h */ /* @other */\n", "/* @jsx h */"),
            (
                "// @jsxRuntime classic\n// later\n",
                "/* @jsxRuntime classic */",
            ),
            (
                "\"use client\";\n/** @jsxImportSource custom */\n",
                "/* @jsxImportSource custom */",
            ),
            (
                "\u{feff}/** @jsxImportSource custom */\n",
                "/* @jsxImportSource custom */",
            ),
            ("/* license */\n'use strict'\n// @jsx h\n", "/* @jsx h */"),
            // On the first statement's own line, after code on its line.
            ("'use strict'; /** @jsx h */ ", "/* @jsx h */"),
            ("/* c */ /** @jsx h */ ", "/* @jsx h */"),
            ("'use strict' /** @jsx h */\n", "/* @jsx h */"),
            // Several pragmas, in one comment and in several: all of them,
            // the last of a kind.
            (
                "/**\n * @jsxRuntime classic\n * @jsx createElement\n */\n",
                "/* @jsxRuntime classic @jsx createElement */",
            ),
            (
                "/** @jsxFrag F @jsx h */\n/** @jsx g */\n",
                "/* @jsx g @jsxFrag F */",
            ),
            // Between a decorator and its export: before the export's span,
            // so oxc reads it.
            ("@dec /** @jsx h */ export class X {}\n", "/* @jsx h */"),
            // No pragma: nothing is restated.
            ("/** a comment with `@jsx h` in backticks */\n", ""),
            ("/** @jsxUnknown x */\n", ""),
        ] {
            for file in ["m.jsx", "m.tsx"] {
                let src = format!("{before}{import}");
                let out =
                    rewrite_cjs_interop(&src, Path::new(file), &target(InteropKind::PreBundle))
                        .unwrap_or_else(|| panic!("{src}"));
                assert_eq!(
                    out,
                    format!("{restated}{head}{before}{rewritten}"),
                    "{file}: {src:?}"
                );
            }
        }
        // After a hashbang line, and never in a file that is not JSX.
        let out = run("#!/usr/bin/env node\n/** @jsx h */\nimport d from \"cjs-dep\";\n");
        assert!(
            out.starts_with(&format!("#!/usr/bin/env node\n{head}/** @jsx h */")),
            "{out}"
        );
        let src = "#!/usr/bin/env node\n/** @jsx h */\nimport d from \"cjs-dep\";\n";
        let out =
            rewrite_cjs_interop(src, Path::new("m.jsx"), &target(InteropKind::PreBundle)).unwrap();
        assert!(
            out.starts_with(&format!(
                "#!/usr/bin/env node\n/* @jsx h */{head}/** @jsx h */"
            )),
            "{out}"
        );
        // A pragma after the first statement is no pragma.
        let out = rewrite_cjs_interop(
            "import d from \"cjs-dep\";\n/** @jsx h */\nd;\n",
            Path::new("m.jsx"),
            &target(InteropKind::PreBundle),
        )
        .unwrap();
        assert!(out.starts_with(head), "{out}");
    }

    /// The import bindings of `"cjs-dep"` this pass reads in `src`.
    fn bindings_oj_keeps(src: &str, file: &str, later: &LaterTransforms) -> Vec<String> {
        let Some(out) = run_ts(src, file, later) else {
            return Vec::new();
        };
        let head = out.split("import __ojcjs").next().unwrap_or("");
        let mut names = Vec::new();
        for statement in head.split(';') {
            // Past the restated pragma comment, when there is one.
            let statement = statement.trim_start();
            let statement = match statement.strip_prefix("/*") {
                Some(comment) => comment.split_once("*/").map_or("", |(_, rest)| rest),
                None => statement,
            };
            let Some(rest) = statement.strip_prefix("const ") else {
                continue;
            };
            let Some((left, right)) = rest.split_once(" = ") else {
                continue;
            };
            if !right.contains("__ojcjs") {
                continue;
            }
            match left.strip_prefix('{') {
                Some(list) => names.extend(
                    list.trim_end_matches('}')
                        .split(',')
                        .filter_map(|pair| pair.split_once(": "))
                        .map(|(_, local)| local.trim().to_string()),
                ),
                None => names.push(left.trim().to_string()),
            }
        }
        names.sort();
        names
    }

    /// The import bindings of `"cjs-dep"` left in what the real transform
    /// (the compile, through its public entry) makes of `src`.
    fn bindings_oxc_keeps(src: &str, file: &str, opts: &crate::CompileOptions) -> Vec<String> {
        let out = crate::compile::compile(Path::new(file), src, opts)
            .unwrap_or_else(|e| panic!("{file} does not compile: {e:?}\n{src}"));
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, &out.code, SourceType::mjs()).parse();
        assert!(!parsed.fatal_error, "{}", out.code);
        let mut names = Vec::new();
        for stmt in &parsed.program.body {
            let Statement::ImportDeclaration(decl) = stmt else {
                continue;
            };
            if crate::lit_str(decl.source.value) != "cjs-dep" {
                continue;
            }
            for spec in decl.specifiers.iter().flatten() {
                names.push(spec.local().name.to_string());
            }
        }
        names.sort();
        names
    }

    /// The pass decides which import bindings the TypeScript and JSX
    /// transforms will leave, before they run. This holds it to what they
    /// do leave: every source, under every JSX configuration and both
    /// `verbatimModuleSyntax` settings, through the real compile.
    #[test]
    fn the_kept_bindings_are_the_ones_the_real_transform_keeps() {
        const SOURCES: &[(&str, &str)] = &[
            // Types and unused.
            ("m.ts", "import { T } from \"cjs-dep\";\nexport let x: T;\n"),
            ("m.ts", "import { type T, type U as V } from \"cjs-dep\";\nexport let x: T | V;\n"),
            ("m.ts", "import type { T } from \"cjs-dep\";\nexport let x: T;\n"),
            ("m.ts", "import { T } from \"cjs-dep\";\nexport type { T };\n"),
            ("m.ts", "import d, { T } from \"cjs-dep\";\nexport let x: T | typeof d;\n"),
            ("m.ts", "import * as ns from \"cjs-dep\";\nexport let x: ns.T;\n"),
            ("m.ts", "import { unused } from \"cjs-dep\";\nexport const x = 1;\n"),
            ("m.ts", "import d, * as ns from \"cjs-dep\";\nexport const x = d;\n"),
            // Values, beside types.
            ("m.ts", "import { a, T } from \"cjs-dep\";\nexport let x: T = a;\n"),
            ("m.ts", "import d, { type T, a } from \"cjs-dep\";\nexport const x = d(a);\n"),
            ("m.ts", "import { C } from \"cjs-dep\";\nexport let x: C = new C();\n"),
            ("m.ts", "import { a } from \"cjs-dep\";\nexport { a };\n"),
            ("m.ts", "import a from \"cjs-dep\";\nexport default a;\n"),
            ("m.ts", "import { a } from \"cjs-dep\";\nexport const o = { a };\n"),
            ("m.ts", "import { a as b } from \"cjs-dep\";\nexport const o = [b];\n"),
            // Shadowing.
            ("m.ts", "import { a } from \"cjs-dep\";\nexport function f(a: number) { return a; }\n"),
            ("m.ts", "import { a } from \"cjs-dep\";\nexport function f(a: number) { return a; }\nf(a);\n"),
            // Redeclared as a value: the import was only a type.
            // (`REDECLARED` marks them: see the skips below.)
            ("m.ts", "import { Status } from \"cjs-dep\";\nconst Status = { ok: 1 }; // REDECLARED\nexport const s: Status = Status as any;\n"),
            ("m.ts", "import { Kind } from \"cjs-dep\";\nfunction Kind() { return 1; } // REDECLARED\nexport const k = Kind();\n"),
            ("m.ts", "import { K } from \"cjs-dep\";\nclass K {} // REDECLARED\nexport const k = new K();\n"),
            // Redeclared as a type only, or as an ambient value.
            ("m.ts", "import { I } from \"cjs-dep\";\ninterface I { a: number }\nexport const i: I = { a: 1 };\n"),
            ("m.ts", "import { A } from \"cjs-dep\";\ntype A = number;\nexport const a: A = 1;\n"),
            ("m.ts", "import { D } from \"cjs-dep\";\ndeclare const D: number;\nexport const d = D;\n"),
            ("m.ts", "import { N } from \"cjs-dep\";\nnamespace N { export type T = number }\nexport const n: N.T = 1;\n"),
            // `import x = ns.y`.
            ("m.ts", "import * as ns from \"cjs-dep\";\nimport x = ns.y;\nexport const v = x;\n"),
            ("m.ts", "import * as ns from \"cjs-dep\";\nimport X = ns.Y;\nexport let v: X;\n"),
            // Another module extension.
            ("m.mts", "import { a, T } from \"cjs-dep\";\nexport let x: T = a;\n"),
            // A member tag is a value use; an intrinsic of the same name is not.
            ("m.tsx", "import * as ns from \"cjs-dep\";\nexport const el = <ns.X />;\n"),
            ("m.tsx", "import { a } from \"cjs-dep\";\nexport const el = <a />;\n"),
            // An invalid classic factory falls back to React.createElement.
            ("m.tsx", "/** @jsxRuntime classic @jsx 1x */\nimport React, { x } from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "/** @jsxRuntime classic @jsx class */\nimport React from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "/** @jsxRuntime classic @jsx this.h */\nimport React from \"cjs-dep\";\nexport const el = <p />;\n"),
            // JSX.
            ("m.tsx", "import { Foo } from \"cjs-dep\";\nexport const el = <Foo />;\n"),
            ("m.tsx", "import React from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "import React from \"cjs-dep\";\nexport const el = <><b /></>;\n"),
            ("m.tsx", "import React from \"cjs-dep\";\nexport const n = 1;\n"),
            ("m.tsx", "import { createElement, Fragment } from \"cjs-dep\";\nexport const A = () => <><b>x</b></>;\n"),
            ("m.tsx", "import { createElement, Fragment } from \"cjs-dep\";\nexport const A = () => <b>x</b>;\n"),
            ("m.tsx", "import { createElement } from \"cjs-dep\";\nexport const n = 1;\n"),
            ("m.ts", "import { createElement } from \"cjs-dep\";\nexport const n = 1;\n"),
            // Comment pragmas.
            ("m.tsx", "/** @jsx h */\nimport { h } from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "/** @jsx h */\nimport { h, Fragment } from \"cjs-dep\";\nexport const el = <><p /></>;\n"),
            ("m.tsx", "/** @jsx m.h */\nimport m from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "/**\n * @jsxRuntime classic\n * @jsx createElement\n */\nimport { createElement } from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "/** @jsxFrag F @jsx h */\nimport { h, F } from \"cjs-dep\";\nexport const el = <><p /></>;\n"),
            ("m.tsx", "/** @jsx a @jsx b */\nimport { a, b } from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "/** `@jsx h` */\nimport { h } from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "/** @jsxImportSource cjs-dep */\nimport { jsx } from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "// @jsx h\nimport { h } from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "/** @jsxRuntime classic */\nimport React, { h } from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "/** @jsxRuntime automatic */\nimport { createElement } from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "\"use client\";\n/** @jsx h */\nimport { h } from \"cjs-dep\";\nexport const el = <p />;\n"),
            ("m.tsx", "import { h } from \"cjs-dep\";\n/** @jsx h */\nexport const el = <p />;\n"),
        ];
        let configs: [(&str, crate::JsxConfig); 3] = [
            ("automatic", crate::JsxConfig::default()),
            (
                "classic, default factory",
                crate::JsxConfig {
                    runtime: Some("classic".into()),
                    ..Default::default()
                },
            ),
            (
                "classic, config factory",
                crate::JsxConfig {
                    runtime: Some("classic".into()),
                    pragma: Some("createElement".into()),
                    pragma_frag: Some("Fragment".into()),
                    ..Default::default()
                },
            ),
        ];
        let (mut cases, mut skipped) = (0, Vec::new());
        for (file, src) in SOURCES {
            for (label, jsx) in &configs {
                for verbatim in [false, true] {
                    let mut opts = crate::CompileOptions::prod();
                    opts.jsx = jsx.clone();
                    opts.verbatim_module_syntax = Some(verbatim);
                    opts.class_field_set_semantics = Some(false);
                    if crate::compile::compile(Path::new(file), src, &opts).is_err() {
                        skipped.push((*src, *label, verbatim));
                        continue;
                    }
                    let later = LaterTransforms {
                        keep_value_imports: verbatim,
                        jsx_classic: jsx.is_classic(),
                        jsx_pragma: jsx.pragma.clone(),
                        jsx_pragma_frag: jsx.pragma_frag.clone(),
                    };
                    assert_eq!(
                        bindings_oj_keeps(src, file, &later),
                        bindings_oxc_keeps(src, file, &opts),
                        "{file}, {label}, verbatimModuleSyntax {verbatim}:\n{src}"
                    );
                    cases += 1;
                }
            }
        }
        // The only combinations that do not compile: an import redeclared
        // as a value, under `verbatimModuleSyntax`, where the transform
        // keeps the import beside the module's own declaration and the
        // compile rejects the name declared twice. So that rule is held to
        // the transform with the option off alone.
        let expected: Vec<_> = SOURCES
            .iter()
            .filter(|(_, src)| src.contains("REDECLARED"))
            .flat_map(|(_, src)| configs.iter().map(move |(label, _)| (*src, *label, true)))
            .collect();
        assert_eq!(skipped, expected, "the combinations that do not compile");
        assert_eq!(cases, SOURCES.len() * configs.len() * 2 - expected.len());
        eprintln!("differential cases: {cases}, skipped: {}", skipped.len());
    }

    /// oxc's `Pragma::parse` for the classic runtime: which identifier the
    /// transform refers to.
    #[test]
    fn the_classic_factory_root_is_the_one_oxc_refers_to() {
        for (pragma, root) in [
            (None, Some("React")),
            (Some(""), Some("React")),
            (Some("h"), Some("h")),
            (Some("Preact.h"), Some("Preact")),
            (Some("a.b.c"), Some("a")),
            // Not a path of identifier names, or a reserved word: the default.
            (Some("1x"), Some("React")),
            (Some("class"), Some("React")),
            (Some("a.1b"), Some("React")),
            (Some("a*/b"), Some("React")),
            (Some("import.x"), Some("React")),
            // No identifier is referred to.
            (Some("this.h"), None),
            (Some("this"), None),
            (Some("import.meta.h"), None),
        ] {
            assert_eq!(classic_pragma_root(pragma).as_deref(), root, "{pragma:?}");
        }
    }

    /// A pragma value that would close the restating comment is neither
    /// restated nor used.
    #[test]
    fn a_pragma_value_that_closes_a_comment_is_no_pragma() {
        let src = "// @jsx a*/b\n// @jsxFrag F\nimport d from \"cjs-dep\";\nd;\n";
        let out =
            rewrite_cjs_interop(src, Path::new("m.jsx"), &target(InteropKind::PreBundle)).unwrap();
        assert!(
            out.starts_with("/* @jsxFrag F */const d = __ojcjs0;// @jsx a*/b"),
            "{out}"
        );
        assert_eq!(out.lines().count(), src.lines().count(), "no line is added");
    }

    /// oxc's `find_jsx_pragma`, case by case.
    #[test]
    fn the_pragma_scan_reads_what_oxc_reads() {
        let all = |mut text: &str| {
            let mut found = Vec::new();
            while let Some((keyword, value, rest)) = find_jsx_pragma(text) {
                found.push(format!("{keyword}={value}"));
                text = rest;
            }
            found
        };
        for (text, want) in [
            ("* @jsx h ", vec!["=h"]),
            (
                "*\n * @jsxRuntime classic\n * @jsx createElement\n ",
                vec!["Runtime=classic", "=createElement"],
            ),
            ("* @jsxFrag F @jsx h ", vec!["Frag=F", "=h"]),
            ("* @jsx a @jsx b ", vec!["=a", "=b"]),
            (
                "* @jsxImportSource @emotion/react ",
                vec!["ImportSource=@emotion/react"],
            ),
            (" @jsx\th", vec!["=h"]),
            ("@jsx h", vec!["=h"]),
            // Not preceded by whitespace or `*`.
            ("* `@jsx h` ", vec![]),
            ("* x@jsx h ", vec![]),
            // An unknown keyword is skipped, the search goes on.
            ("* @jsxUnknown x @jsx h ", vec!["=h"]),
            // The keyword ends at a space or a tab: a line break does not
            // end it, so `@jsx\nh` is the unknown keyword `\nh`... up to the
            // next space, and the search goes on from there.
            ("* @jsx\nh @jsxFrag F ", vec!["Frag=F"]),
            // No value before the end.
            ("* @jsx ", vec![]),
            ("* @jsx", vec![]),
        ] {
            assert_eq!(all(text), want, "{text:?}");
        }
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
            &LaterTransforms::default(),
            &|_| true,
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
            &LaterTransforms::default(),
            &|_| true,
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
            &LaterTransforms::default(),
            &|_| true,
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
