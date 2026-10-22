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
//! through a namespace import. Edits keep the module's line count.

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, CallExpression, Expression, ImportDeclarationSpecifier, ModuleExportName, Program,
    Statement,
};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

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
/// has none. `path` picks the parser's language.
pub fn rewrite_env_fns(code: &str, path: &str, env: Env) -> Option<String> {
    if !FACTORIES.iter().any(|(name, _)| code.contains(name)) {
        return None;
    }
    let mut out = code.to_string();
    let mut changed = false;
    // A factory call inside a kept function is compiled on the next pass,
    // once the call around it is gone.
    for _ in 0..16 {
        let Some(next) = rewrite_outermost(&out, path, env) else {
            break;
        };
        out = next;
        changed = true;
    }
    changed.then_some(out)
}

fn rewrite_outermost(code: &str, path: &str, env: Env) -> Option<String> {
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path).unwrap_or_else(|_| SourceType::tsx());
    let parsed = Parser::new(&allocator, code, source_type).parse();
    if parsed.fatal_error {
        return None;
    }
    let bindings = Bindings::of(&parsed.program);
    if bindings.is_empty() {
        return None;
    }
    let mut finder = Finder {
        bindings: &bindings,
        env,
        code,
        edits: Vec::new(),
    };
    finder.visit_program(&parsed.program);
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
        return None;
    }
    let mut out = String::with_capacity(code.len());
    let mut last = 0;
    for Edit { start, end, text } in outermost {
        out.push_str(&code[last..start]);
        let lost = code[start..end].matches('\n').count();
        let kept = text.matches('\n').count();
        out.push_str(&text);
        out.push_str(&"\n".repeat(lost.saturating_sub(kept)));
        last = end;
    }
    out.push_str(&code[last..]);
    Some(out)
}

/// The module's local names for the factories: imported names and namespace
/// imports of `@tanstack/*` modules.
#[derive(Default)]
struct Bindings {
    names: Vec<(String, Factory)>,
    namespaces: Vec<String>,
}

impl Bindings {
    fn of(program: &Program) -> Bindings {
        let mut out = Bindings::default();
        for statement in &program.body {
            let Statement::ImportDeclaration(import) = statement else {
                continue;
            };
            if !import.source.value.starts_with("@tanstack/") {
                continue;
            }
            for specifier in import.specifiers.iter().flatten() {
                match specifier {
                    ImportDeclarationSpecifier::ImportSpecifier(s) => {
                        let imported = match &s.imported {
                            ModuleExportName::IdentifierName(id) => id.name.as_str(),
                            ModuleExportName::IdentifierReference(id) => id.name.as_str(),
                            ModuleExportName::StringLiteral(lit) => lit.value.as_str(),
                        };
                        if let Some((_, factory)) = FACTORIES.iter().find(|(n, _)| *n == imported) {
                            out.names.push((s.local.name.to_string(), *factory));
                        }
                    }
                    ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                        out.namespaces.push(s.local.name.to_string());
                    }
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
}

/// An isomorphic chain's implementations, as source spans.
#[derive(Default)]
struct Chain {
    server: Option<(usize, usize)>,
    client: Option<(usize, usize)>,
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
        let arg = first_arg(call)?;
        if side == "server" {
            chain.server = Some(arg);
        } else {
            chain.client = Some(arg);
        }
        Some(chain)
    }

    fn arg_text(&self, (start, end): (usize, usize)) -> String {
        format!("({})", &self.code[start..end])
    }
}

/// The first argument's span, when it is a plain expression.
fn first_arg(call: &CallExpression) -> Option<(usize, usize)> {
    match call.arguments.first()? {
        Argument::SpreadElement(_) => None,
        arg => {
            let span = arg.span();
            Some((span.start as usize, span.end as usize))
        }
    }
}

impl<'a> Visit<'a> for Finder<'_> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        let span = call.span;
        let (start, end) = (span.start as usize, span.end as usize);
        if let Some(chain) = self.chain(call) {
            let side = match self.env {
                Env::Server => chain.server,
                Env::Client => chain.client,
            };
            let text = side.map_or_else(|| "(() => {})".to_string(), |s| self.arg_text(s));
            self.edits.push(Edit { start, end, text });
        } else if let Some(factory @ (Factory::ServerOnly | Factory::ClientOnly)) =
            self.bindings.factory(&call.callee)
        {
            let (target, name) = match factory {
                Factory::ServerOnly => (Env::Server, "Server"),
                _ => (Env::Client, "Client"),
            };
            if let Some(arg) = first_arg(call) {
                let text = if self.env == target {
                    self.arg_text(arg)
                } else {
                    let side = name.to_lowercase();
                    format!(
                        "(() => {{ throw new Error({:?}); }})",
                        format!("create{name}OnlyFn() functions can only be called on the {side}!")
                    )
                };
                self.edits.push(Edit { start, end, text });
            }
        }
        walk::walk_call_expression(self, call);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const IMPORT: &str = "import { createIsomorphicFn, createServerOnlyFn, createClientOnlyFn } from \"@tanstack/react-start\";\n";

    fn rewrite(body: &str, env: Env) -> String {
        rewrite_env_fns(&format!("{IMPORT}{body}"), "a.ts", env)
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
        let client = rewrite_env_fns(code, "a.js", Env::Client).unwrap();
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
        assert_eq!(rewrite_env_fns(code, "a.ts", Env::Client), None);
        assert_eq!(rewrite_env_fns("const x = 1;\n", "a.ts", Env::Client), None);
    }

    #[test]
    fn a_replacement_keeps_precedence() {
        let out = rewrite("createServerOnlyFn(() => 1).call(null);\n", Env::Server);
        assert!(out.contains("(() => 1).call(null);"), "{out}");
    }
}
