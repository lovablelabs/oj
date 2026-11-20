// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A violation's message as Start's plugin writes it (`formatViolation`):
//! what denied the import, where the importer uses it, the chain of imports
//! from an entry, and the code around the use.
//!
//! Start reads the location from the transformed code it cached and maps it
//! back through a sourcemap. This reads the importer's file, so the column
//! is the source's own.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{Declaration, Expression, ImportDeclarationSpecifier, Program, Statement};
use oxc_ast::AstKind;
use oxc_ast_visit::Visit;
use oxc_parser::{ParseOptions, Parser};
use oxc_semantic::{Scoping, SemanticBuilder, SymbolId};
use oxc_span::{GetSpan, SourceType, Span};

use super::{relativize, Graph, ImportProtection, Side};

pub(super) enum Kind<'a> {
    /// The denied specifier pattern.
    Specifier(&'a str),
    /// The denied file pattern.
    File(&'a str),
    Marker,
}

pub(super) struct Violation<'a> {
    pub side: Side,
    pub kind: Kind<'a>,
    pub importer: &'a str,
    pub specifier: &'a str,
    pub resolved: Option<&'a str>,
}

/// A 1-based line and column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Loc {
    line: usize,
    column: usize,
}

struct Step {
    file: String,
    specifier: Option<String>,
    loc: Option<Loc>,
}

const CLIENT_SUGGESTIONS: [&str; 4] = [
    "Use createServerFn().handler(() => ...) to keep the logic on the server and call it from the client via an RPC bridge",
    "Use createServerOnlyFn(() => ...) to mark it as server-only (it will throw if accidentally called from the client)",
    "Use createIsomorphicFn().client(() => ...).server(() => ...) to provide separate client and server implementations",
    "Move the server-only import out of this file into a separate .server.ts module that is not imported by any client code",
];

const SERVER_SUGGESTIONS: [&str; 3] = [
    "Use createClientOnlyFn(() => ...) to mark it as client-only (returns undefined on the server)",
    "Use createIsomorphicFn().client(() => ...).server(() => ...) to provide separate client and server implementations",
    "Move the client-only import out of this file into a separate .client.ts module that is not imported by any server code",
];

/// Start's `formatViolation` over an enriched violation.
pub(super) fn message(ip: &ImportProtection, graph: &Graph, v: &Violation<'_>) -> String {
    let rel = |path: &str| relativize(path, &ip.root).to_string();
    let code = std::fs::read_to_string(v.importer).ok();
    let loc = code.as_deref().and_then(|code| {
        let module = Module::parse(code, Path::new(v.importer))?;
        candidates(v, &ip.root).iter().find_map(|source| {
            module
                .usage(source, v.side)
                .or_else(|| module.import_of(source))
        })
    });
    let snippet = code
        .as_deref()
        .zip(loc)
        .and_then(|(code, loc)| snippet(code, loc));

    let mut trace = trace(graph, v.importer, ip.max_trace_depth);
    for step in &mut trace {
        let Some(specifier) = &step.specifier else {
            continue;
        };
        step.loc = std::fs::read_to_string(&step.file)
            .ok()
            .and_then(|code| Module::parse(&code, Path::new(&step.file))?.import_of(specifier));
    }
    if let Some(last) = trace.last_mut() {
        last.specifier
            .get_or_insert_with(|| v.specifier.to_string());
        last.loc = last.loc.or(loc);
    }

    let at =
        |loc: Option<Loc>| loc.map_or_else(String::new, |l| format!(":{}:{}", l.line, l.column));
    let mut lines: Vec<String> = vec![
        String::new(),
        format!(
            "[import-protection] Import denied in {} environment",
            v.side.name()
        ),
        String::new(),
    ];
    lines.push(match v.kind {
        Kind::Specifier(pattern) => format!("  Denied by specifier pattern: {pattern}"),
        Kind::File(pattern) => format!("  Denied by file pattern: {pattern}"),
        Kind::Marker => {
            "  Denied by marker: module is restricted to the opposite environment".to_string()
        }
    });
    lines.push(format!("  Importer: {}{}", rel(v.importer), at(loc)));
    lines.push(format!("  Import: \"{}\"", rel(v.specifier)));
    if let Some(resolved) = v.resolved {
        lines.push(format!("  Resolved: {}", rel(resolved)));
    }
    if !trace.is_empty() {
        lines.push(String::new());
        lines.push("  Trace:".to_string());
        for (i, step) in trace.iter().enumerate() {
            let entry = if i == 0 { " (entry)" } else { "" };
            let import = step
                .specifier
                .as_ref()
                .map_or_else(String::new, |s| format!(" (import \"{}\")", rel(s)));
            lines.push(format!(
                "    {}. {}{}{entry}{import}",
                i + 1,
                rel(&step.file),
                at(step.loc)
            ));
        }
    }
    if let (Some(snippet), Some(loc)) = (&snippet, loc) {
        lines.push(String::new());
        lines.push("  Code:".to_string());
        lines.extend(snippet.iter().cloned());
        lines.push(String::new());
        lines.push(format!("  {}{}", rel(v.importer), at(Some(loc))));
    }
    lines.push(String::new());
    lines.push("  Suggestions:".to_string());
    match v.side {
        Side::Client => lines.extend(CLIENT_SUGGESTIONS.iter().map(|s| format!("    - {s}"))),
        Side::Server => {
            let text = snippet.as_ref().map_or_else(String::new, |s| s.join("\n"));
            if looks_like_jsx(&text) {
                lines.push("    - Wrap the JSX in <ClientOnly fallback={<Loading />}>...</ClientOnly> so it only renders in the browser after hydration".to_string());
            }
            lines.extend(SERVER_SUGGESTIONS.iter().map(|s| format!("    - {s}")));
        }
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Start's test for a snippet that shows JSX: `/<[A-Z]/`, or a `{...(...)...}`
/// on one line beside a `<`.
fn looks_like_jsx(text: &str) -> bool {
    let tag = text
        .as_bytes()
        .windows(2)
        .any(|w| w[0] == b'<' && w[1].is_ascii_uppercase());
    let call_in_braces = text.lines().any(|line| {
        let Some(open) = line.find('{') else {
            return false;
        };
        let Some(paren) = line[open..].find('(') else {
            return false;
        };
        let Some(close) = line[open + paren..].find(')') else {
            return false;
        };
        line[open + paren + close..].contains('}')
    });
    tag || (call_in_braces && text.contains('<'))
}

/// The ways the importer may have written the denied import, most likely
/// first. Start's `buildSourceCandidates` and
/// `addImporterRelativeSourceCandidates`.
fn candidates(v: &Violation<'_>, root: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push = |value: &str| {
        let bare = value.split('?').next().unwrap_or(value);
        for candidate in [value, bare, without_known_extension(bare)] {
            if !candidate.is_empty() && !out.iter().any(|c| c == candidate) {
                out.push(candidate.to_string());
            }
        }
    };
    push(v.specifier);
    if let Some(resolved) = v.resolved {
        push(resolved);
        let relative = relativize(resolved, root);
        push(relative);
        push(&format!("./{relative}"));
        push(&format!("/{relative}"));
        let from = Path::new(v.importer).parent().unwrap_or(Path::new(""));
        let written = relative_path(from, Path::new(resolved));
        if !written.is_empty() {
            if written.starts_with('.') {
                push(&written);
            } else {
                push(&format!("./{written}"));
            }
        }
    }
    out
}

fn without_known_extension(id: &str) -> &str {
    const KNOWN: [&str; 9] = [
        ".ts", ".tsx", ".mts", ".cts", ".js", ".jsx", ".mjs", ".cjs", ".json",
    ];
    let name_start = id.rfind('/').map_or(0, |i| i + 1);
    match id[name_start..].rfind('.') {
        // A leading dot names a dotfile, not an extension (Node's extname).
        Some(dot) if dot > 0 && KNOWN.contains(&&id[name_start + dot..]) => &id[..name_start + dot],
        _ => id,
    }
}

/// `to` as written from `from` (Node's `path.relative`), `/`-separated.
fn relative_path(from: &Path, to: &Path) -> String {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut parts: Vec<String> = vec!["..".to_string(); from.len() - shared];
    parts.extend(
        to[shared..]
            .iter()
            .map(|c| c.as_os_str().to_string_lossy().into_owned()),
    );
    parts.join("/")
}

/// Start's `buildTrace`: the shortest chain of imports from an entry (a
/// registered one, or a module nothing imports) down to `start`.
fn trace(graph: &Graph, start: &str, max_depth: usize) -> Vec<Step> {
    let mut visited: HashSet<&str> = HashSet::from([start]);
    let mut depth: HashMap<&str, usize> = HashMap::from([(start, 0)]);
    // Each importer's link down toward `start`: the module it imports there,
    // and the specifier it wrote.
    let mut down: HashMap<&str, (&str, &str)> = HashMap::new();
    let mut queue: VecDeque<&str> = VecDeque::from([start]);
    let mut root = start;
    while let Some(node) = queue.pop_front() {
        let importers = graph.reverse.get(node).filter(|list| !list.is_empty());
        if node != start && (graph.entries.contains(node) || importers.is_none()) {
            root = node;
            break;
        }
        if depth[node] >= max_depth {
            continue;
        }
        for (importer, specifier) in importers.into_iter().flatten() {
            if visited.insert(importer.as_str()) {
                depth.insert(importer, depth[node] + 1);
                down.insert(importer, (node, specifier));
                queue.push_back(importer);
            }
        }
    }
    let mut steps = Vec::new();
    let mut current = root;
    for _ in 0..=max_depth + 1 {
        let link = down.get(current);
        steps.push(Step {
            file: current.to_string(),
            specifier: link.map(|(_, specifier)| (*specifier).to_string()),
            loc: None,
        });
        match link {
            Some((next, _)) => current = next,
            None => break,
        }
    }
    steps
}

/// Start's `buildCodeSnippet`: the line at `loc` with two lines around it
/// and a caret under the column.
fn snippet(code: &str, loc: Loc) -> Option<Vec<String>> {
    let all: Vec<&str> = code
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect();
    if loc.line < 1 || loc.line > all.len() {
        return None;
    }
    let start = loc.line.saturating_sub(2).max(1);
    let end = (loc.line + 2).min(all.len());
    let gutter = end.to_string().len();
    let mut out = Vec::new();
    for number in start..=end {
        let marker = if number == loc.line { '>' } else { ' ' };
        out.push(format!(
            "  {marker} {number:>gutter$} | {}",
            all[number - 1]
        ));
        if number == loc.line && loc.column > 0 {
            out.push(format!(
                "    {} | {}^",
                " ".repeat(gutter),
                " ".repeat(loc.column - 1)
            ));
        }
    }
    Some(out)
}

/// One module's imports, read once.
struct Imports {
    /// Each import source with the offset of its string literal, in source
    /// order: static imports, re-exports and literal `import()`s.
    sources: Vec<(String, u32)>,
    /// The symbols each static import's source binds.
    bindings: Vec<(String, Vec<SymbolId>)>,
}

/// A module that parses. Each lookup parses it again: a violation is rare,
/// and nothing is kept between two.
struct Module<'a> {
    code: &'a str,
    path: &'a Path,
}

impl<'a> Module<'a> {
    fn parse(code: &'a str, path: &'a Path) -> Option<Module<'a>> {
        let allocator = Allocator::default();
        let parsed = parse(&allocator, code, path);
        (!parsed.fatal_error).then_some(Module { code, path })
    }

    fn with<T>(&self, read: impl FnOnce(&Program<'_>, &Scoping, &Imports) -> T) -> T {
        let allocator = Allocator::default();
        let program = parse(&allocator, self.code, self.path).program;
        let scoping = SemanticBuilder::new()
            .build(&program)
            .semantic
            .into_scoping();
        let imports = imports(&program);
        read(&program, &scoping, &imports)
    }

    /// Where the module first imports `source`: its string literal.
    fn import_of(&self, source: &str) -> Option<Loc> {
        self.with(|_, _, imports| {
            imports
                .sources
                .iter()
                .filter(|(s, _)| s == source)
                .map(|(_, offset)| *offset)
                .min()
                .map(|offset| loc_at(self.code, offset))
        })
    }

    /// Where the module first uses a binding it imports from `source`,
    /// outside the calls Start's compiler strips on `side`: a call, a `new`
    /// or a member access when there is one, else any reference. Start's
    /// `findUsagePosInAnalysis`.
    fn usage(&self, source: &str, side: Side) -> Option<Loc> {
        self.with(|program, scoping, imports| {
            let symbols: Vec<SymbolId> = imports
                .bindings
                .iter()
                .filter(|(s, _)| s == source)
                .flat_map(|(_, symbols)| symbols.iter().copied())
                .collect();
            if symbols.is_empty() {
                return None;
            }
            let mut usage = Usage {
                scoping,
                symbols: &symbols,
                side,
                stack: Vec::new(),
                preferred: None,
                any: None,
            };
            usage.visit_program(program);
            usage
                .preferred
                .or(usage.any)
                .map(|offset| loc_at(self.code, offset))
        })
    }
}

fn parse<'a>(allocator: &'a Allocator, code: &'a str, path: &Path) -> oxc_parser::ParserReturn<'a> {
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::tsx());
    // Babel, which Start parses with, has no node for a parenthesis.
    Parser::new(allocator, code, source_type)
        .with_options(ParseOptions {
            preserve_parens: false,
            ..ParseOptions::default()
        })
        .parse()
}

fn imports(program: &Program<'_>) -> Imports {
    let mut out = Imports {
        sources: Vec::new(),
        bindings: Vec::new(),
    };
    for statement in &program.body {
        match statement {
            Statement::ImportDeclaration(decl) => {
                let specifiers: &[ImportDeclarationSpecifier<'_>] = decl
                    .specifiers
                    .as_deref()
                    .map_or(&[], |list| list.as_slice());
                let all_types = !specifiers.is_empty()
                    && specifiers.iter().all(|s| {
                        matches!(s, ImportDeclarationSpecifier::ImportSpecifier(s) if s.import_kind.is_type())
                    });
                if decl.import_kind.is_type() || all_types {
                    continue;
                }
                out.sources
                    .push((text(&decl.source), decl.source.span.start));
                let symbols = specifiers
                    .iter()
                    .filter_map(|s| match s {
                        ImportDeclarationSpecifier::ImportSpecifier(s) => {
                            (!s.import_kind.is_type()).then(|| s.local.symbol_id.get())?
                        }
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                            s.local.symbol_id.get()
                        }
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                            s.local.symbol_id.get()
                        }
                    })
                    .collect();
                out.bindings.push((text(&decl.source), symbols));
            }
            Statement::ExportFromDeclaration(decl) => {
                let all_types = !decl.specifiers.is_empty()
                    && decl.specifiers.iter().all(|s| s.export_kind.is_type());
                if !decl.export_kind.is_type() && !all_types {
                    out.sources
                        .push((text(&decl.source), decl.source.span.start));
                }
            }
            Statement::ExportAllDeclaration(decl) if !decl.export_kind.is_type() => {
                out.sources
                    .push((text(&decl.source), decl.source.span.start));
            }
            _ => {}
        }
    }
    let mut dynamic = DynamicImports(&mut out.sources);
    dynamic.visit_program(program);
    out.sources.sort_by_key(|(_, offset)| *offset);
    out
}

struct DynamicImports<'s>(&'s mut Vec<(String, u32)>);

impl<'a> Visit<'a> for DynamicImports<'_> {
    fn visit_import_expression(&mut self, it: &oxc_ast::ast::ImportExpression<'a>) {
        if let Expression::StringLiteral(source) = &it.source {
            self.0.push((text(source), source.span.start));
        }
        oxc_ast_visit::walk::walk_import_expression(self, it);
    }
}

struct Usage<'a, 's> {
    scoping: &'s Scoping,
    symbols: &'s [SymbolId],
    side: Side,
    stack: Vec<AstKind<'a>>,
    preferred: Option<u32>,
    any: Option<u32>,
}

impl<'a> Usage<'a, '_> {
    /// Whether the reference sits in a function passed straight to a call
    /// Start's compiler removes from this side's code.
    fn in_stripped_call(&self) -> bool {
        self.stack.iter().enumerate().rev().any(|(i, kind)| {
            let function = match kind {
                AstKind::Function(f) => f.span,
                AstKind::ArrowFunctionExpression(f) => f.span,
                _ => return false,
            };
            let Some(AstKind::CallExpression(call)) = i.checked_sub(1).map(|p| self.stack[p])
            else {
                return false;
            };
            call.arguments.iter().any(|arg| arg.span() == function)
                && stripped_callee(&call.callee, self.side)
        })
    }
}

/// Start's `isCompilerSafeBoundaryCall`, for the callee.
fn stripped_callee(callee: &Expression<'_>, side: Side) -> bool {
    match callee {
        Expression::Identifier(id) => match side {
            Side::Client => id.name == "createServerOnlyFn",
            Side::Server => id.name == "createClientOnlyFn",
        },
        Expression::StaticMemberExpression(member) => {
            let root = callee_root(&member.object).unwrap_or("");
            match (side, member.property.name.as_str()) {
                (Side::Client, "handler") => root.ends_with("ServerFn"),
                (Side::Client, "server") => {
                    root == "createIsomorphicFn" || root.ends_with("Middleware")
                }
                (Side::Server, "client") => root == "createIsomorphicFn",
                _ => false,
            }
        }
        _ => false,
    }
}

fn callee_root<'a>(expression: &Expression<'a>) -> Option<&'a str> {
    match expression {
        Expression::Identifier(id) => Some(id.name.as_str()),
        Expression::CallExpression(call) => callee_root(&call.callee),
        Expression::StaticMemberExpression(member) => callee_root(&member.object),
        Expression::ComputedMemberExpression(member) => callee_root(&member.object),
        _ => None,
    }
}

impl<'a> Visit<'a> for Usage<'a, '_> {
    fn enter_node(&mut self, kind: AstKind<'a>) {
        self.stack.push(kind);
    }

    fn leave_node(&mut self, _: AstKind<'a>) {
        self.stack.pop();
    }

    fn visit_import_declaration(&mut self, _: &oxc_ast::ast::ImportDeclaration<'a>) {}

    fn visit_identifier_reference(&mut self, it: &oxc_ast::ast::IdentifierReference<'a>) {
        if self.preferred.is_some() {
            return;
        }
        let symbol = it
            .reference_id
            .get()
            .and_then(|id| self.scoping.get_reference(id).symbol_id());
        if !symbol.is_some_and(|s| self.symbols.contains(&s)) {
            return;
        }
        let parent = self.stack.last().copied();
        // A JSX tag name is no `Identifier` to Babel, so Start skips it.
        if matches!(
            parent,
            Some(
                AstKind::JSXOpeningElement(_)
                    | AstKind::JSXClosingElement(_)
                    | AstKind::JSXMemberExpression(_)
            )
        ) || self.in_stripped_call()
        {
            return;
        }
        let is = |span: Span| span == it.span;
        let preferred = match parent {
            Some(AstKind::CallExpression(call)) => is(call.callee.span()),
            Some(AstKind::NewExpression(new)) => is(new.callee.span()),
            Some(AstKind::StaticMemberExpression(member)) => is(member.object.span()),
            Some(AstKind::ComputedMemberExpression(member)) => is(member.object.span()),
            _ => false,
        };
        if preferred {
            self.preferred = Some(it.span.start);
        } else if self.any.is_none() {
            self.any = Some(it.span.start);
        }
    }
}

/// A string literal's value. A lone surrogate, which no module specifier
/// holds, reads as empty.
fn text(literal: &oxc_ast::ast::StringLiteral<'_>) -> String {
    literal.value.as_str().unwrap_or_default().to_string()
}

/// The 1-based line and column (in UTF-16 units, as Babel counts) of a byte
/// offset.
fn loc_at(code: &str, offset: u32) -> Loc {
    let before = &code[..(offset as usize).min(code.len())];
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    Loc {
        line: before.matches('\n').count() + 1,
        column: before[line_start..].encode_utf16().count() + 1,
    }
}

/// The names a module exports, `default` aside, sorted. Start's
/// `getNamedExports`. Empty for code that does not parse.
pub(super) fn named_exports(code: &str, path: &Path) -> Vec<String> {
    let allocator = Allocator::default();
    let parsed = parse(&allocator, code, path);
    if parsed.fatal_error {
        return Vec::new();
    }
    let mut names: Vec<String> = Vec::new();
    let mut specifiers = |list: &[oxc_ast::ast::ExportSpecifier<'_>]| {
        for specifier in list {
            if !specifier.export_kind.is_type() {
                names.push(specifier.exported.name().to_string());
            }
        }
    };
    let mut declared: Vec<String> = Vec::new();
    for statement in &parsed.program.body {
        match statement {
            Statement::ExportDeclaration(decl) => match &decl.declaration {
                Declaration::FunctionDeclaration(f) => {
                    declared.extend(f.id.as_ref().map(|id| id.name.to_string()));
                }
                Declaration::ClassDeclaration(c) => {
                    declared.extend(c.id.as_ref().map(|id| id.name.to_string()));
                }
                Declaration::VariableDeclaration(v) => {
                    for declarator in &v.declarations {
                        declared.extend(
                            declarator
                                .id
                                .get_binding_identifiers()
                                .iter()
                                .map(|id| id.name.to_string()),
                        );
                    }
                }
                // What TypeScript compiles to a value.
                Declaration::TSEnumDeclaration(e) if !e.declare => {
                    declared.push(e.id.name.to_string());
                }
                Declaration::TSNamespaceDeclaration(n) if !n.declare => {
                    declared.push(n.id.name.to_string());
                }
                _ => {}
            },
            Statement::ExportNamedDeclaration(decl) if !decl.export_kind.is_type() => {
                specifiers(&decl.specifiers);
            }
            // `export { type A, type B } from "x"` exports no value.
            Statement::ExportFromDeclaration(decl)
                if !decl.export_kind.is_type()
                    && !(!decl.specifiers.is_empty()
                        && decl.specifiers.iter().all(|s| s.export_kind.is_type())) =>
            {
                specifiers(&decl.specifiers);
            }
            _ => {}
        }
    }
    names.extend(declared);
    names.retain(|n| n != "default" && !n.is_empty());
    names.sort();
    names.dedup();
    names
}
