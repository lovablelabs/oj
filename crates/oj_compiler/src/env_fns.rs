// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TanStack Start's environment functions, compiled per environment the way
//! its start-compiler does (handleCreateIsomorphicFn, handleEnvOnly), so the
//! other side's code never reaches a bundle:
//!
//! - `createIsomorphicFn().server(s).client(c)` becomes this side's function,
//!   or a no-op when it has none;
//! - `createServerOnlyFn(f)` / `createClientOnlyFn(f)` become `f` on their
//!   side, and a function that throws on the other.
//!
//! A call counts when its factory is imported from a `@tanstack/*` module
//! (the framework packages re-export `@tanstack/start-fn-stubs`), by name or
//! through a namespace import, or by name from a module the caller found
//! re-exports TanStack's ([`foreign_factory_imports`], [`reexport_of`]), as
//! TanStack resolves an import through the modules that re-export it. Edits keep the module's line count. As
//! TanStack's compiler does after it, the bindings only the dropped side used
//! go too, so the other side's imports are not bundled.

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, BindingIdentifier, BindingPattern, CallExpression, Expression,
    ImportDeclarationSpecifier, ModuleExportName, Program, Statement,
};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_semantic::{Scoping, SemanticBuilder};
use oxc_span::{GetSpan, SourceType};
use std::collections::HashSet;

/// The environment a module is compiled for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Env {
    Client,
    Server,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Factory {
    Isomorphic,
    ServerOnly,
    ClientOnly,
}

const FACTORIES: [(&str, Factory); 3] = [
    ("createIsomorphicFn", Factory::Isomorphic),
    ("createServerOnlyFn", Factory::ServerOnly),
    ("createClientOnlyFn", Factory::ClientOnly),
];

/// `code` with its environment functions compiled for `env`; `None` when it
/// has none. `path` picks the parser's language. Like TanStack's compiler, it
/// fails when this side's implementation is not a plain expression
/// (`createIsomorphicFn().client(func) must be called with a function!`).
pub fn rewrite_env_fns(code: &str, path: &str, env: Env) -> Result<Option<String>, String> {
    rewrite_env_fns_with(code, path, env, &[])
}

/// [`rewrite_env_fns`], also counting the `(source, name)` imports in
/// `reexported` as TanStack's factories.
pub fn rewrite_env_fns_with(
    code: &str,
    path: &str,
    env: Env,
    reexported: &[(String, String)],
) -> Result<Option<String>, String> {
    if !has_factory_name(code) {
        return Ok(None);
    }
    let mut out = code.to_string();
    let mut changed = false;
    // A factory call inside a kept function is compiled on the next pass,
    // once the call around it is gone.
    for _ in 0..16 {
        let Some((next, nested)) = rewrite_outermost(&out, path, env, reexported)? else {
            break;
        };
        out = next;
        changed = true;
        if !nested {
            break;
        }
    }
    Ok(changed.then(|| drop_dead_bindings(code, out, path)))
}

fn has_factory_name(code: &str) -> bool {
    FACTORIES.iter().any(|(name, _)| code.contains(name))
}

/// The `(source, name)` imports of a factory name from outside `@tanstack/*`
/// (`import { createIsomorphicFn } from "~/lib/start"`): the ones whose module
/// re-exports TanStack's go to [`rewrite_env_fns_with`].
pub fn foreign_factory_imports(code: &str, path: &str) -> Vec<(String, String)> {
    if !has_factory_name(code) {
        return Vec::new();
    }
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, source_type(path)).parse();
    let mut out = Vec::new();
    for statement in &parsed.program.body {
        let Statement::ImportDeclaration(import) = statement else {
            continue;
        };
        if import.source.value.starts_with("@tanstack/") {
            continue;
        }
        for specifier in import.specifiers.iter().flatten() {
            if let ImportDeclarationSpecifier::ImportSpecifier(s) = specifier {
                let imported = export_name(&s.imported);
                if factory_named(imported).is_some() {
                    out.push((import.source.value.to_string(), imported.to_string()));
                }
            }
        }
    }
    out
}

/// Where a module's export `name` comes from, when it re-exports it.
#[derive(Debug, PartialEq, Eq)]
pub enum ReExport {
    /// A factory of a `@tanstack/*` module.
    TanStack,
    /// Export `name` of module `source`.
    From { source: String, name: String },
    /// One of these `export *` sources, if any has it.
    Stars(Vec<String>),
}

/// How `code` (a module) exports `name`: `export { x as name } from`, an
/// import it exports again, or `export *`; `None` when it defines it itself.
pub fn reexport_of(code: &str, path: &str, name: &str) -> Option<ReExport> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, source_type(path)).parse();
    let from = |source: &str, imported: &str| {
        if source.starts_with("@tanstack/") && factory_named(imported).is_some() {
            ReExport::TanStack
        } else {
            ReExport::From {
                source: source.to_string(),
                name: imported.to_string(),
            }
        }
    };
    let mut local = None;
    let mut stars = Vec::new();
    for statement in &parsed.program.body {
        match statement {
            Statement::ExportFromDeclaration(export) => {
                for s in &export.specifiers {
                    if export_name(&s.exported) == name {
                        return Some(from(&export.source.value, export_name(&s.local)));
                    }
                }
            }
            Statement::ExportNamedDeclaration(export) => {
                for s in &export.specifiers {
                    if export_name(&s.exported) == name {
                        local = Some(export_name(&s.local).to_string());
                    }
                }
            }
            Statement::ExportAllDeclaration(export) if export.exported.is_none() => {
                stars.push(export.source.value.to_string());
            }
            _ => {}
        }
    }
    if let Some(local) = local {
        for statement in &parsed.program.body {
            let Statement::ImportDeclaration(import) = statement else {
                continue;
            };
            for specifier in import.specifiers.iter().flatten() {
                if let ImportDeclarationSpecifier::ImportSpecifier(s) = specifier {
                    if s.local.name == local.as_str() {
                        return Some(from(&import.source.value, export_name(&s.imported)));
                    }
                }
            }
        }
        return None;
    }
    if stars.iter().any(|s| s.starts_with("@tanstack/")) && factory_named(name).is_some() {
        return Some(ReExport::TanStack);
    }
    (!stars.is_empty()).then_some(ReExport::Stars(stars))
}

fn export_name<'a>(name: &'a ModuleExportName) -> &'a str {
    match name {
        ModuleExportName::IdentifierName(id) => id.name.as_str(),
        ModuleExportName::IdentifierReference(id) => id.name.as_str(),
        ModuleExportName::StringLiteral(lit) => lit.value.as_str(),
    }
}

fn factory_named(name: &str) -> Option<Factory> {
    FACTORIES.iter().find(|(n, _)| *n == name).map(|(_, f)| *f)
}

fn rewrite_outermost(
    code: &str,
    path: &str,
    env: Env,
    reexported: &[(String, String)],
) -> Result<Option<(String, bool)>, String> {
    #[cfg(test)]
    tests::PASSES.with(|n| n.set(n.get() + 1));
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, source_type(path)).parse();
    if parsed.fatal_error {
        return Ok(None);
    }
    let bindings = Bindings::of(&parsed.program, reexported);
    if bindings.is_empty() {
        return Ok(None);
    }
    let mut finder = Finder {
        bindings: &bindings,
        env,
        code,
        edits: Vec::new(),
        kept: Vec::new(),
        error: None,
    };
    finder.visit_program(&parsed.program);
    if let Some(error) = finder.error {
        return Err(error);
    }
    // A call inside a kept implementation needs another pass.
    let nested = finder.edits.iter().any(|e| {
        finder
            .kept
            .iter()
            .any(|&(start, end)| start <= e.start && e.end <= end)
    });
    // Only the outermost: a chain's own prefix (`createIsomorphicFn().server(s)`
    // inside `.client(c)`) and calls inside its arguments are covered by it.
    let mut edits = finder.edits;
    edits.sort_by_key(|e| (e.start, std::cmp::Reverse(e.end)));
    let mut outermost: Vec<Edit> = Vec::new();
    for edit in edits {
        if outermost.last().is_some_and(|o| edit.start < o.end) {
            continue;
        }
        outermost.push(edit);
    }
    if outermost.is_empty() {
        return Ok(None);
    }
    Ok(Some((apply(code, outermost), nested)))
}

/// `code` with `edits` (sorted, disjoint) applied, each padded with the
/// newlines it removed.
fn apply(code: &str, edits: Vec<Edit>) -> String {
    let mut out = String::with_capacity(code.len());
    let mut last = 0;
    for Edit { start, end, text } in edits {
        out.push_str(&code[last..start]);
        let lost = code[start..end].matches('\n').count();
        let kept = text.matches('\n').count();
        out.push_str(&text);
        out.push_str(&"\n".repeat(lost.saturating_sub(kept)));
        last = end;
    }
    out.push_str(&code[last..]);
    out
}

/// TanStack's dead-code pass (babel-dead-code-elimination, with the bindings
/// referenced before the rewrite as its candidates): a module-level import,
/// variable or function that was used and no longer is goes, repeatedly, so
/// whatever only it used goes with it. Exported declarations stay.
fn drop_dead_bindings(before: &str, mut after: String, path: &str) -> String {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, before, source_type(path)).parse();
    if parsed.fatal_error {
        return after;
    }
    let scoping = SemanticBuilder::new()
        .build(&parsed.program)
        .semantic
        .into_scoping();
    let mut candidates = HashSet::new();
    for_each_binding(&parsed.program, &mut |id| {
        if references(&scoping, id) > 0 {
            candidates.insert(id.name.to_string());
        }
    });
    if candidates.is_empty() {
        return after;
    }
    while let Some(next) = drop_unreferenced(&after, path, &candidates) {
        after = next;
    }
    after
}

/// One pass of [`drop_dead_bindings`]: `None` when nothing is dead.
fn drop_unreferenced(code: &str, path: &str, candidates: &HashSet<String>) -> Option<String> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, source_type(path)).parse();
    if parsed.fatal_error {
        return None;
    }
    let scoping = SemanticBuilder::new()
        .build(&parsed.program)
        .semantic
        .into_scoping();
    let dead = |id: &BindingIdentifier| {
        candidates.contains(id.name.as_str()) && references(&scoping, id) == 0
    };
    let text = |span: oxc_span::Span| &code[span.start as usize..span.end as usize];
    let mut edits = Vec::new();
    for statement in &parsed.program.body {
        let span = statement.span();
        let (start, end) = (span.start as usize, span.end as usize);
        match statement {
            Statement::ImportDeclaration(import) if !import.import_kind.is_type() => {
                let Some(specifiers) = &import.specifiers else {
                    continue;
                };
                let kept: Vec<_> = specifiers.iter().filter(|s| !dead(s.local())).collect();
                if kept.len() == specifiers.len() {
                    continue;
                }
                let text = if kept.is_empty() {
                    String::new()
                } else {
                    let mut clauses = Vec::new();
                    let mut named = Vec::new();
                    for specifier in kept {
                        match specifier {
                            ImportDeclarationSpecifier::ImportSpecifier(_) => {
                                named.push(text(specifier.span()))
                            }
                            _ => clauses.push(text(specifier.span()).to_string()),
                        }
                    }
                    if !named.is_empty() {
                        clauses.push(format!("{{ {} }}", named.join(", ")));
                    }
                    let tail = &code[import.source.span.start as usize..end];
                    format!("import {} from {tail}", clauses.join(", "))
                };
                edits.push(Edit { start, end, text });
            }
            Statement::VariableDeclaration(var) => {
                let is_dead = |d: &&oxc_ast::ast::VariableDeclarator| matches!(&d.id, BindingPattern::BindingIdentifier(id) if dead(id));
                let kept: Vec<_> = var.declarations.iter().filter(|d| !is_dead(d)).collect();
                if kept.len() == var.declarations.len() {
                    continue;
                }
                let text = if kept.is_empty() {
                    String::new()
                } else {
                    let kind = &code[start..var.declarations[0].span.start as usize];
                    let list: Vec<_> = kept.iter().map(|d| text(d.span)).collect();
                    let semi = if text(span).ends_with(';') { ";" } else { "" };
                    format!("{kind}{}{semi}", list.join(", "))
                };
                edits.push(Edit { start, end, text });
            }
            Statement::FunctionDeclaration(f) if f.id.as_ref().is_some_and(dead) => {
                edits.push(Edit {
                    start,
                    end,
                    text: String::new(),
                });
            }
            _ => {}
        }
    }
    let out = apply(code, edits);
    (out != code).then_some(out)
}

/// The module-level bindings the dead-code pass may drop: imports, variables
/// bound to a plain name, and function declarations, not exported ones.
fn for_each_binding(program: &Program, f: &mut impl FnMut(&BindingIdentifier)) {
    for statement in &program.body {
        match statement {
            Statement::ImportDeclaration(import) => {
                for specifier in import.specifiers.iter().flatten() {
                    f(specifier.local());
                }
            }
            Statement::VariableDeclaration(var) => {
                for declarator in &var.declarations {
                    if let BindingPattern::BindingIdentifier(id) = &declarator.id {
                        f(id);
                    }
                }
            }
            Statement::FunctionDeclaration(func) => {
                if let Some(id) = &func.id {
                    f(id);
                }
            }
            _ => {}
        }
    }
}

fn references(scoping: &Scoping, id: &BindingIdentifier) -> usize {
    id.symbol_id
        .get()
        .map_or(1, |symbol| scoping.get_resolved_reference_ids(symbol).len())
}

fn source_type(path: &str) -> SourceType {
    SourceType::from_path(path).unwrap_or_else(|_| SourceType::tsx())
}

/// The module's local names for the factories: imported names and namespace
/// imports of `@tanstack/*` modules.
#[derive(Default)]
struct Bindings {
    names: Vec<(String, Factory)>,
    namespaces: Vec<String>,
}

impl Bindings {
    fn of(program: &Program, reexported: &[(String, String)]) -> Bindings {
        let mut out = Bindings::default();
        for statement in &program.body {
            let Statement::ImportDeclaration(import) = statement else {
                continue;
            };
            let source = import.source.value.as_str();
            let tanstack = source.starts_with("@tanstack/");
            for specifier in import.specifiers.iter().flatten() {
                match specifier {
                    ImportDeclarationSpecifier::ImportSpecifier(s) => {
                        let imported = export_name(&s.imported);
                        let counts = tanstack
                            || reexported
                                .iter()
                                .any(|(src, n)| src == source && n == imported);
                        if let Some(factory) = factory_named(imported).filter(|_| counts) {
                            out.names.push((s.local.name.to_string(), factory));
                        }
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) if tanstack => {
                        out.namespaces.push(s.local.name.to_string());
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(_) => {}
                    ImportDeclarationSpecifier::ImportDefaultSpecifier(_) => {}
                }
            }
        }
        out
    }

    fn is_empty(&self) -> bool {
        self.names.is_empty() && self.namespaces.is_empty()
    }

    /// The factory `callee` names, when it is one.
    fn factory(&self, callee: &Expression) -> Option<Factory> {
        match callee.without_parentheses() {
            Expression::Identifier(id) => self
                .names
                .iter()
                .find(|(local, _)| *local == id.name.as_str())
                .map(|(_, f)| *f),
            Expression::StaticMemberExpression(m) => {
                let Expression::Identifier(ns) = m.object.without_parentheses() else {
                    return None;
                };
                if !self.namespaces.iter().any(|n| *n == ns.name.as_str()) {
                    return None;
                }
                FACTORIES
                    .iter()
                    .find(|(name, _)| *name == m.property.name.as_str())
                    .map(|(_, f)| *f)
            }
            _ => None,
        }
    }
}

struct Edit {
    start: usize,
    end: usize,
    text: String,
}

struct Finder<'b> {
    bindings: &'b Bindings,
    env: Env,
    code: &'b str,
    edits: Vec<Edit>,
    /// The implementations kept as replacement text, as source spans.
    kept: Vec<(usize, usize)>,
    /// The first call TanStack's compiler rejects.
    error: Option<String>,
}

/// An isomorphic chain's implementations.
#[derive(Default)]
struct Chain {
    server: Option<Arg>,
    client: Option<Arg>,
}

/// A factory call's implementation argument.
#[derive(Clone, Copy)]
enum Arg {
    /// A plain expression, as a source span.
    Expr(usize, usize),
    /// Missing or spread: TanStack's "must be called with a function!".
    NotAFunction,
}

impl Finder<'_> {
    /// `call` as `createIsomorphicFn()` followed by `.server(..)` /
    /// `.client(..)` links; None when it is not one.
    fn chain(&self, call: &CallExpression) -> Option<Chain> {
        if self.bindings.factory(&call.callee) == Some(Factory::Isomorphic) {
            return Some(Chain::default());
        }
        let Expression::StaticMemberExpression(m) = call.callee.without_parentheses() else {
            return None;
        };
        let side = m.property.name.as_str();
        if side != "server" && side != "client" {
            return None;
        }
        let Expression::CallExpression(inner) = m.object.without_parentheses() else {
            return None;
        };
        let mut chain = self.chain(inner)?;
        let arg = first_arg(call);
        if side == "server" {
            chain.server = Some(arg);
        } else {
            chain.client = Some(arg);
        }
        Some(chain)
    }

    /// `arg` as replacement text, or `error` recorded when it is not one.
    fn arg_text(&mut self, arg: Arg, error: impl FnOnce() -> String) -> Option<String> {
        match arg {
            Arg::Expr(start, end) => {
                self.kept.push((start, end));
                Some(format!("({})", &self.code[start..end]))
            }
            Arg::NotAFunction => {
                self.error.get_or_insert_with(error);
                None
            }
        }
    }
}

fn first_arg(call: &CallExpression) -> Arg {
    match call.arguments.first() {
        None | Some(Argument::SpreadElement(_)) => Arg::NotAFunction,
        Some(arg) => {
            let span = arg.span();
            Arg::Expr(span.start as usize, span.end as usize)
        }
    }
}

impl<'a> Visit<'a> for Finder<'_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        let span = call.span;
        let (start, end) = (span.start as usize, span.end as usize);
        if let Some(chain) = self.chain(call) {
            let (side, name) = match self.env {
                Env::Server => (chain.server, "server"),
                Env::Client => (chain.client, "client"),
            };
            let text = match side {
                None => Some("(() => {})".to_string()),
                Some(arg) => self.arg_text(arg, || {
                    format!("createIsomorphicFn().{name}(func) must be called with a function!")
                }),
            };
            if let Some(text) = text {
                self.edits.push(Edit { start, end, text });
            }
        } else if let Some(factory @ (Factory::ServerOnly | Factory::ClientOnly)) =
            self.bindings.factory(&call.callee)
        {
            let (target, name) = match factory {
                Factory::ServerOnly => (Env::Server, "Server"),
                _ => (Env::Client, "Client"),
            };
            let text = if self.env == target {
                self.arg_text(first_arg(call), || {
                    format!("create{name}OnlyFn() must be called with a function!")
                })
            } else {
                let side = name.to_lowercase();
                Some(format!(
                    "(() => {{ throw new Error({:?}); }})",
                    format!("create{name}OnlyFn() functions can only be called on the {side}!")
                ))
            };
            if let Some(text) = text {
                self.edits.push(Edit { start, end, text });
            }
        }
        walk::walk_call_expression(self, call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    thread_local! {
        pub(super) static PASSES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    fn passes(f: impl FnOnce()) -> usize {
        PASSES.with(|n| n.set(0));
        f();
        PASSES.with(|n| n.get())
    }

    const IMPORT: &str = "import { createIsomorphicFn, createServerOnlyFn, createClientOnlyFn } from \"@tanstack/react-start\";\n";

    fn rewrite(body: &str, env: Env) -> String {
        rewrite_env_fns(&format!("{IMPORT}{body}"), "a.ts", env)
            .unwrap()
            .unwrap_or_else(|| panic!("rewritten: {body}"))
    }

    #[test]
    fn isomorphic_chains_keep_only_this_sides_function() {
        let body = "export const f = createIsomorphicFn()\n  .server(() => secretOnServer())\n  .client(() => window.__X__);\n";
        let client = rewrite(body, Env::Client);
        assert!(
            client.contains("export const f = (() => window.__X__)"),
            "{client}"
        );
        assert!(!client.contains("secretOnServer"), "{client}");
        let server = rewrite(body, Env::Server);
        assert!(server.contains("(() => secretOnServer())"), "{server}");
        assert!(!server.contains("window.__X__"), "{server}");
        for out in [&client, &server] {
            assert_eq!(
                out.lines().count(),
                format!("{IMPORT}{body}").lines().count(),
                "{out}"
            );
        }
    }

    #[test]
    fn a_missing_side_becomes_a_no_op() {
        let out = rewrite(
            "const f = createIsomorphicFn().server(() => db());\n",
            Env::Client,
        );
        assert!(out.contains("const f = (() => {});"), "{out}");
        assert!(!out.contains("db()"), "{out}");
    }

    #[test]
    fn env_only_functions_throw_on_the_other_side() {
        let body = "const s = createServerOnlyFn(() => readSecrets());\nconst c = createClientOnlyFn(() => localStorage.x);\n";
        let client = rewrite(body, Env::Client);
        assert!(!client.contains("readSecrets"), "{client}");
        assert!(
            client.contains("throw new Error(\"createServerOnlyFn() functions can only be called on the server!\")"),
            "{client}"
        );
        assert!(
            client.contains("const c = (() => localStorage.x);"),
            "{client}"
        );
        let server = rewrite(body, Env::Server);
        assert!(
            server.contains("const s = (() => readSecrets());"),
            "{server}"
        );
        assert!(
            server.contains("createClientOnlyFn() functions can only be called on the client!"),
            "{server}"
        );
    }

    #[test]
    fn aliases_namespaces_and_nested_calls_compile() {
        let code = "import { createIsomorphicFn as iso } from \"@tanstack/start-fn-stubs\";\nimport * as S from \"@tanstack/react-start\";\nexport const a = iso().client(() => S.createServerOnlyFn(() => inner())).server(() => 1);\nexport const b = S.createClientOnlyFn(() => 2);\n";
        let client = rewrite_env_fns(code, "a.js", Env::Client).unwrap().unwrap();
        assert!(
            client.contains("export const a = (() => (() => { throw new Error("),
            "{client}"
        );
        assert!(!client.contains("inner()"), "{client}");
        assert!(client.contains("export const b = (() => 2);"), "{client}");
    }

    #[test]
    fn calls_off_other_modules_and_shadow_free_code_stay() {
        let code = "import { createIsomorphicFn } from \"./mine\";\nconst f = createIsomorphicFn().server(() => 1);\n";
        assert_eq!(rewrite_env_fns(code, "a.ts", Env::Client), Ok(None));
        assert_eq!(
            rewrite_env_fns("const x = 1;\n", "a.ts", Env::Client),
            Ok(None)
        );
    }

    #[test]
    fn a_replacement_keeps_precedence() {
        let out = rewrite("createServerOnlyFn(() => 1).call(null);\n", Env::Server);
        assert!(out.contains("(() => 1).call(null);"), "{out}");
    }

    #[test]
    fn imports_only_the_dropped_side_used_go() {
        let code = "import { createIsomorphicFn } from \"@tanstack/react-start\";\nimport { db, other } from \"./db\";\nimport { unused } from \"./kept-as-it-was\";\nimport \"./side-effect\";\nconst query = () => db.q();\nfunction load() { return query(); }\nexport const who = createIsomorphicFn().server(() => load()).client(() => other);\n";
        let client = rewrite_env_fns(code, "a.js", Env::Client).unwrap().unwrap();
        for gone in ["db,", "db.q", "createIsomorphicFn", "query", "load"] {
            assert!(!client.contains(gone), "{gone}:\n{client}");
        }
        for kept in [
            "import { other } from \"./db\";",
            "import { unused } from \"./kept-as-it-was\";",
            "import \"./side-effect\";",
            "export const who = (() => other);",
        ] {
            assert!(client.contains(kept), "{kept}:\n{client}");
        }
        assert_eq!(client.lines().count(), code.lines().count(), "{client}");
        let server = rewrite_env_fns(code, "a.js", Env::Server).unwrap().unwrap();
        assert!(server.contains("import { db } from \"./db\";"), "{server}");
        assert!(server.contains("function load()"), "{server}");
    }

    #[test]
    fn exported_and_still_used_bindings_stay() {
        let code = "import { createServerOnlyFn } from \"@tanstack/react-start\";\nimport def, * as ns from \"./x\";\nconst a = () => ns.a, b = 1;\nconst c = () => def;\nexport { c };\nexport function helper() { return b; }\nexport const s = createServerOnlyFn(() => [a(), c(), helper()]);\n";
        let client = rewrite_env_fns(code, "a.ts", Env::Client).unwrap().unwrap();
        assert!(client.contains("const b = 1;"), "{client}");
        assert!(!client.contains("ns.a"), "{client}");
        assert!(client.contains("import def from \"./x\";"), "{client}");
        assert!(client.contains("const c = () => def;"), "{client}");
        assert!(client.contains("export function helper()"), "{client}");
    }

    #[test]
    fn a_factory_reexported_by_a_local_module_compiles() {
        let code = "import { createIsomorphicFn } from \"~/lib/start\";\nimport { createServerOnlyFn } from \"@tanstack/react-start\";\nexport const f = createIsomorphicFn().server(() => 1).client(() => 2);\n";
        assert_eq!(
            foreign_factory_imports(code, "a.ts"),
            vec![("~/lib/start".to_string(), "createIsomorphicFn".to_string())]
        );
        assert_eq!(rewrite_env_fns(code, "a.ts", Env::Client), Ok(None));
        let reexported = foreign_factory_imports(code, "a.ts");
        let out = rewrite_env_fns_with(code, "a.ts", Env::Client, &reexported)
            .unwrap()
            .unwrap();
        assert!(out.contains("export const f = (() => 2);"), "{out}");
    }

    #[test]
    fn reexports_are_traced_to_tanstack() {
        let name = "createIsomorphicFn";
        let of = |code: &str| reexport_of(code, "lib.ts", name);
        assert_eq!(
            of("export { createIsomorphicFn } from \"@tanstack/react-start\";"),
            Some(ReExport::TanStack)
        );
        assert_eq!(
            of("import { createIsomorphicFn as iso } from \"@tanstack/react-start\";\nexport { iso as createIsomorphicFn };"),
            Some(ReExport::TanStack)
        );
        assert_eq!(
            of("export * from \"@tanstack/react-start\";"),
            Some(ReExport::TanStack)
        );
        assert_eq!(
            of("export { iso as createIsomorphicFn } from \"./inner\";"),
            Some(ReExport::From {
                source: "./inner".to_string(),
                name: "iso".to_string()
            })
        );
        assert_eq!(
            of("export * from \"./a\";\nexport * from \"./b\";"),
            Some(ReExport::Stars(vec!["./a".to_string(), "./b".to_string()]))
        );
        assert_eq!(of("export const createIsomorphicFn = () => ({});"), None);
        assert_eq!(
            of("export { createIsomorphicFn as other } from \"@tanstack/react-start\";"),
            None
        );
    }

    #[test]
    fn this_sides_implementation_must_be_a_function_expression() {
        let err = |body: &str, env| rewrite_env_fns(&format!("{IMPORT}{body}"), "a.ts", env);
        let spread = "const f = createIsomorphicFn().server(() => 1).client(...impls);\n";
        assert_eq!(
            err(spread, Env::Client),
            Err("createIsomorphicFn().client(func) must be called with a function!".to_string())
        );
        // The other side's argument is never looked at.
        let server = err(spread, Env::Server).unwrap().unwrap();
        assert!(server.contains("const f = (() => 1);"), "{server}");
        assert_eq!(
            err("const s = createServerOnlyFn();\n", Env::Server),
            Err("createServerOnlyFn() must be called with a function!".to_string())
        );
        let client = err("const s = createServerOnlyFn();\n", Env::Client)
            .unwrap()
            .unwrap();
        assert!(
            client.contains("can only be called on the server!"),
            "{client}"
        );
    }

    #[test]
    fn only_a_call_inside_a_kept_implementation_takes_another_pass() {
        let flat = "const a = createIsomorphicFn().server(() => 1).client(() => createServerOnlyFn(() => 2));\nconst b = createClientOnlyFn(() => 3);\n";
        assert_eq!(passes(|| drop(rewrite(flat, Env::Server))), 1);
        let nested = "const a = createIsomorphicFn().client(() => 1).server(() => createServerOnlyFn(() => 2));\n";
        let mut out = String::new();
        assert_eq!(passes(|| out = rewrite(nested, Env::Server)), 2);
        assert!(out.contains("const a = (() => (() => 2));"), "{out}");
    }
}
