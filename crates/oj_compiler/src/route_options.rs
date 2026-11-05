// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The route options TanStack Start's client code splitter deletes. Start's
//! router plugin runs `tanstack-router:code-splitter:compile-reference-file`
//! for the client with `deleteNodes: ["ssr", "server", "headers"]`:
//! `compileCodeSplitReferenceRoute` drops those keys from a route's options
//! object, then runs its dead-code pass, so a route's server handlers and
//! what only they imported never reach the browser.

use std::collections::HashSet;

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, BindingPattern, CallExpression, Declaration, Expression, ObjectExpression,
    ObjectPropertyKind, PropertyKey, Statement, VariableDeclarator,
};
use oxc_ast_visit::{walk, Visit};
use oxc_parser::Parser;
use oxc_span::{GetSpan, SourceType};

/// The route options the client never gets (Start's client `deleteNodes`).
pub const CLIENT_DELETED_ROUTE_OPTIONS: &[&str] = &["ssr", "server", "headers"];

/// The route factories the reference compile edits (`allCreateRouteFns`).
const CREATE_ROUTE_FNS: &[&str] = &[
    "createFileRoute",
    "createRootRoute",
    "createRootRouteWithContext",
];

/// The code splitter's `routeFactoryCallCodeFilter`: a module that names no
/// route factory call is never compiled.
pub fn names_a_route_factory(code: &str) -> bool {
    CREATE_ROUTE_FNS.iter().any(|f| {
        code.match_indices(f).any(|(at, _)| {
            let before = code[..at].chars().next_back();
            let boundary = !before.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
            let rest = code[at + f.len()..].trim_start();
            boundary
                && (rest.starts_with('(')
                    || (*f == "createRootRouteWithContext" && rest.starts_with('<')))
        })
    })
}

/// The route module with `deleted` keys gone from its route options and,
/// when one went, what only they used gone too (the reference compile's
/// `deadCodeElimination` with the identifiers referenced before its edits).
/// Line counts are kept. None when no key was deleted.
pub fn delete_route_options(code: &str, path: &str, deleted: &[&str]) -> Option<String> {
    if !names_a_route_factory(code) {
        return None;
    }
    let allocator = Allocator::default();
    let source_type = SourceType::from_path(path.split('?').next().unwrap_or(path))
        .unwrap_or_else(|_| SourceType::tsx());
    let parsed = Parser::new(&allocator, code, source_type).parse();
    if parsed.fatal_error {
        return None;
    }
    let mut finder = RouteOptions {
        program: &parsed.program.body,
        code,
        deleted,
        seen: HashSet::new(),
        edits: Vec::new(),
        stopped: false,
    };
    finder.visit_program(&parsed.program);
    let mut edits = finder.edits;
    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut out = code.to_string();
    for (start, end) in edits {
        let lines = code[start..end].matches('\n').count();
        out.replace_range(start..end, &"\n".repeat(lines));
    }
    Some(crate::env_fns::drop_dead_bindings(code, out, path))
}

/// The spans to delete for `options`' `deleted` keys: each property with
/// the comma after it (or, for the last one, the comma before it).
fn deleted_property_spans(
    options: &ObjectExpression<'_>,
    code: &str,
    deleted: &[&str],
) -> Vec<(usize, usize)> {
    let props = &options.properties;
    let mut out = Vec::new();
    for (i, prop) in props.iter().enumerate() {
        let ObjectPropertyKind::ObjectProperty(p) = prop else {
            continue;
        };
        // getObjectPropertyKeyName: a non-computed identifier or string key.
        let name = match &p.key {
            _ if p.computed => None,
            PropertyKey::StaticIdentifier(id) => Some(id.name.as_str()),
            PropertyKey::StringLiteral(lit) => lit.value.as_str(),
            _ => None,
        };
        if !name.is_some_and(|n| deleted.contains(&n)) {
            continue;
        }
        let start = p.span.start as usize;
        let end = p.span.end as usize;
        match props.get(i + 1) {
            Some(next) => out.push((start, next.span().start as usize)),
            None => {
                // The trailing comma, if any, goes with it.
                let tail = &code[end..options.span.end as usize];
                let comma = tail.find(',').filter(|c| tail[..*c].trim().is_empty());
                out.push((start, comma.map_or(end, |c| end + c + 1)));
            }
        }
    }
    out
}

/// Collects the deletions in the route options objects the reference
/// compile visits, in its order.
struct RouteOptions<'p, 'a> {
    program: &'p [Statement<'a>],
    code: &'p str,
    deleted: &'p [&'p str],
    /// Options objects already edited, by start offset.
    seen: HashSet<u32>,
    edits: Vec<(usize, usize)>,
    /// An unsplittable (root) route stops the traversal (`programPath.stop()`).
    stopped: bool,
}

fn factory_name<'b>(call: &'b CallExpression<'_>) -> Option<&'b str> {
    match &call.callee {
        Expression::Identifier(id) if CREATE_ROUTE_FNS.contains(&id.name.as_str()) => {
            Some(id.name.as_str())
        }
        _ => None,
    }
}

impl<'p, 'a> RouteOptions<'p, 'a> {
    /// `babelHandleReference` on the factory call's first argument, through
    /// `resolveIdentifier`: an object literal, or an identifier a module-level
    /// declarator binds to one.
    fn handle(&mut self, name: &str, arg: Option<&Argument<'_>>) {
        let spans = match arg {
            Some(Argument::ObjectExpression(o)) => self.edit(o),
            Some(Argument::Identifier(id)) => match self.bound_object(id.name.as_str()) {
                Some(o) => self.edit(o),
                None => return,
            },
            _ => return,
        };
        self.edits.extend(spans);
        if name != "createFileRoute" {
            self.stopped = true;
        }
    }

    fn edit(&mut self, options: &ObjectExpression<'_>) -> Vec<(usize, usize)> {
        if !self.seen.insert(options.span.start) {
            return Vec::new();
        }
        deleted_property_spans(options, self.code, self.deleted)
    }

    fn bound_object(&self, name: &str) -> Option<&'p ObjectExpression<'a>> {
        for statement in self.program {
            let var = match statement {
                Statement::VariableDeclaration(v) => v,
                Statement::ExportDeclaration(e) => match &e.declaration {
                    Declaration::VariableDeclaration(v) => v,
                    _ => continue,
                },
                _ => continue,
            };
            for d in &var.declarations {
                if matches!(&d.id, BindingPattern::BindingIdentifier(b) if b.name == name) {
                    if let Some(Expression::ObjectExpression(o)) = &d.init {
                        return Some(o);
                    }
                }
            }
        }
        None
    }
}

impl<'a> Visit<'a> for RouteOptions<'_, 'a> {
    fn visit_call_expression(&mut self, call: &CallExpression<'a>) {
        if self.stopped {
            return;
        }
        // `createFileRoute("/x")(options)`, `createRootRouteWithContext<T>()(options)`:
        // the factory call's parent is a call, whose first argument is them.
        if let Expression::CallExpression(inner) = &call.callee {
            if let Some(name) = factory_name(inner) {
                self.handle(name, call.arguments.first());
            }
        }
        walk::walk_call_expression(self, call);
    }

    fn visit_variable_declarator(&mut self, d: &VariableDeclarator<'a>) {
        // `const Route = createRootRoute(options)`: a declarator whose init is
        // the factory call itself.
        if !self.stopped {
            if let Some(Expression::CallExpression(call)) = &d.init {
                if let Some(name) = factory_name(call) {
                    self.handle(name, call.arguments.first());
                }
            }
        }
        walk::walk_variable_declarator(self, d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(code: &str) -> Option<String> {
        delete_route_options(code, "src/routes/x.tsx", CLIENT_DELETED_ROUTE_OPTIONS)
    }

    fn parses(code: &str) {
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, code, SourceType::tsx()).parse();
        assert!(
            parsed.diagnostics.is_empty(),
            "{:?}\n{code}",
            parsed.diagnostics
        );
    }

    // A server route (`whatsapp-webhook.ts` shape): its handlers, the helpers
    // only they call and the `**/server/**` imports only those use leave the
    // client; the route itself stays.
    #[test]
    fn a_server_routes_handlers_and_their_imports_leave_the_client() {
        let code = [
            "import { createFileRoute } from \"@tanstack/react-router\";",
            "import { createAdminClient } from \"@/server/shared/admin-client\";",
            "import { storeInboundMedia } from \"@/server/shared/whatsappMedia\";",
            "import { label } from \"./label\";",
            "function asRecord(v: unknown) { return v && typeof v === \"object\" ? v : {}; }",
            "async function handle(req: Request) {",
            "  const db = createAdminClient();",
            "  return storeInboundMedia(db, asRecord(await req.json()));",
            "}",
            "export const Route = createFileRoute(\"/api/public/hook\")({",
            "  server: {",
            "    handlers: { POST: async ({ request }) => handle(request) },",
            "  },",
            "  headers: () => ({ \"x-a\": \"1\" }),",
            "  \"ssr\": false,",
            "  component: () => label,",
            "});",
            "",
        ]
        .join("\n");
        let out = client(&code).expect("rewritten");
        for gone in [
            "@/server/shared/admin-client",
            "@/server/shared/whatsappMedia",
            "handlers",
            "asRecord",
            "x-a",
            "ssr",
        ] {
            assert!(!out.contains(gone), "{gone} left in:\n{out}");
        }
        for kept in [
            "import { createFileRoute } from \"@tanstack/react-router\";",
            "import { label } from \"./label\";",
            "component: () => label",
            "createFileRoute(\"/api/public/hook\")({",
        ] {
            assert!(out.contains(kept), "{kept} is gone:\n{out}");
        }
        assert_eq!(out.lines().count(), code.lines().count(), "{out}");
        parses(&out);
    }

    // The last option goes with its comma; a computed key and a nested
    // `server` stay (only the route options' own keys are deleted).
    #[test]
    fn only_the_route_options_own_keys_are_deleted() {
        let code = "export const Route = createFileRoute(\"/x\")({ component: C, [server]: 1, loader: () => ({ server: 2 }), server: { handlers: {} }, });\n";
        let out = client(code).expect("rewritten");
        assert!(out.contains("[server]: 1"), "{out}");
        assert!(out.contains("({ server: 2 })"), "{out}");
        assert!(!out.contains("handlers"), "{out}");
        parses(&out);
    }

    #[test]
    fn root_routes_and_bound_options_are_edited() {
        let code = [
            "import { createRootRouteWithContext } from \"@tanstack/react-router\";",
            "import { secret } from \"./secret.server\";",
            "export const Route = createRootRouteWithContext<{ a: 1 }>()({",
            "  server: { handlers: { GET: () => secret() } },",
            "  component: Root,",
            "});",
            "function Root() { return null; }",
            "",
        ]
        .join("\n");
        let out = client(&code).expect("root route");
        assert!(!out.contains("secret"), "{out}");
        assert!(out.contains("component: Root"), "{out}");
        parses(&out);

        let code = [
            "import { srv } from \"./srv\";",
            "const options = { server: { handlers: { GET: srv } }, component: C };",
            "export const Route = createFileRoute(\"/y\")(options);",
            "",
        ]
        .join("\n");
        let out = client(&code).expect("bound options");
        assert!(!out.contains("srv"), "{out}");
        assert!(out.contains("component: C"), "{out}");
        parses(&out);

        let code = "import { srv } from \"./srv\";\nexport const Route = createRootRoute({ server: srv });\n";
        let out = client(code).expect("createRootRoute init");
        assert!(!out.contains("srv"), "{out}");
        parses(&out);
    }

    #[test]
    fn a_module_without_a_route_factory_call_or_deleted_key_is_untouched() {
        assert_eq!(client("export const server = { handlers: {} };\n"), None);
        assert_eq!(
            client("export const Route = createFileRoute(\"/x\")({ component: C });\n"),
            None
        );
        assert!(!names_a_route_factory("mycreateFileRoute(1)"));
        assert!(names_a_route_factory("createRootRouteWithContext<T>()({})"));
    }
}
