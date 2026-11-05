// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! TanStack's dead-code pass, `deadCodeElimination(ast, refIdents)` from
//! babel-dead-code-elimination, which Start's compiler and the router's code
//! splitter run after their edits with the identifiers referenced before
//! them (`findReferencedIdentifiers`) as candidates. A module-level binding
//! that was used and no longer is goes, repeatedly, so whatever only it used
//! goes with it:
//!
//! - a binding is dead when nothing outside the dead bindings uses it
//!   (`findRemovableBindings`): an unused one, but also a self-recursive
//!   helper, or a group of helpers that only call each other;
//! - candidates are the bindings referenced before the edit and not already
//!   dead then, so code that was dead to begin with stays;
//! - a reassigned binding is never dropped (`constantViolations`);
//! - imports lose their dead specifiers (all of them: the declaration goes),
//!   declarators bound to a dead name go, a destructuring pattern loses its
//!   dead names (an object pattern with a rest element keeps its other
//!   properties) and goes once empty, function declarations go;
//! - exported declarations stay (Babel counts the export as a reference).
//!
//! Edits keep the module's line count.

use std::collections::{HashMap, HashSet};

use oxc_allocator::Allocator;
use oxc_ast::ast::{
    BindingIdentifier, BindingPattern, Declaration, ExportDefaultDeclarationKind,
    ImportDeclarationSpecifier, Program, Statement, VariableDeclaration,
};
use oxc_parser::Parser;
use oxc_semantic::{Semantic, SemanticBuilder};
use oxc_span::{GetSpan, SourceType, Span};

/// `after` with what only the edit's removed code used gone, the candidates
/// being the module-level bindings `before` used.
pub(crate) fn drop_dead_bindings(before: &str, mut after: String, path: &str) -> String {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, before, source_type(path)).parse();
    if parsed.fatal_error {
        return after;
    }
    let semantic = SemanticBuilder::new().build(&parsed.program).semantic;
    let graph = Graph::of(&parsed.program, &semantic);
    let candidates: HashSet<String> = graph
        .bindings
        .iter()
        .enumerate()
        .filter(|(i, b)| b.referenced && !graph.dead.contains(i))
        .map(|(_, b)| b.name.clone())
        .collect();
    if candidates.is_empty() {
        return after;
    }
    while let Some(next) = drop_pass(&after, path, &candidates) {
        after = next;
    }
    after
}

fn source_type(path: &str) -> SourceType {
    SourceType::from_path(path.split('?').next().unwrap_or(path))
        .unwrap_or_else(|_| SourceType::tsx())
}

/// A module-level binding the pass may drop.
struct Binding {
    name: String,
    /// The node its references are attributed to (Babel's `binding.path`):
    /// the import specifier, the declarator, the function.
    container: Span,
    /// Exported, reassigned or otherwise never dropped.
    pinned: bool,
    /// Any reference at all (`binding.referenced`).
    referenced: bool,
    /// Where it is referenced.
    refs: Vec<Span>,
}

/// The module's droppable bindings and which of them are dead.
struct Graph {
    bindings: Vec<Binding>,
    dead: HashSet<usize>,
}

impl Graph {
    fn of(program: &Program, semantic: &Semantic) -> Graph {
        let scoping = semantic.scoping();
        let spans = reference_spans(program);
        let mut bindings = Vec::new();
        let mut add = |id: &BindingIdentifier, container: Span, exported: bool| {
            let Some(symbol) = id.symbol_id.get() else {
                return;
            };
            let mut refs = Vec::new();
            let mut written = false;
            for &rid in scoping.get_resolved_reference_ids(symbol) {
                let reference = scoping.get_reference(rid);
                if reference.is_write() {
                    written = true;
                }
                // A reference with no identifier node (an export specifier's
                // local) sits outside every binding: it keeps the binding.
                refs.push(
                    spans
                        .get(&rid)
                        .copied()
                        .unwrap_or(Span::new(u32::MAX, u32::MAX)),
                );
            }
            bindings.push(Binding {
                name: id.name.to_string(),
                container,
                pinned: exported || written,
                referenced: !refs.is_empty() || exported,
                refs,
            });
        };
        for statement in &program.body {
            match statement {
                Statement::ImportDeclaration(import) if !import.import_kind.is_type() => {
                    for s in import.specifiers.iter().flatten() {
                        add(s.local(), s.span(), false);
                    }
                }
                Statement::VariableDeclaration(var) => declarators(var, false, &mut add),
                Statement::FunctionDeclaration(f) => {
                    if let Some(id) = &f.id {
                        add(id, f.span, false);
                    }
                }
                Statement::ExportDeclaration(e) => match &e.declaration {
                    Declaration::VariableDeclaration(var) => declarators(var, true, &mut add),
                    Declaration::FunctionDeclaration(f) => {
                        if let Some(id) = &f.id {
                            add(id, f.span, true);
                        }
                    }
                    _ => {}
                },
                Statement::ExportDefaultDeclaration(e) => {
                    if let ExportDefaultDeclarationKind::FunctionDeclaration(f) = &e.declaration {
                        if let Some(id) = &f.id {
                            add(id, f.span, true);
                        }
                    }
                }
                _ => {}
            }
        }

        // findRemovableBindings: a reference inside another binding's node is
        // an edge from that binding; one anywhere else keeps it alive, as do
        // the pinned ones; what nothing alive reaches is dead.
        let container_of = |span: Span| -> Vec<usize> {
            bindings
                .iter()
                .enumerate()
                .filter(|(_, b)| b.container.start <= span.start && span.end <= b.container.end)
                .map(|(i, _)| i)
                .collect()
        };
        let mut alive = vec![false; bindings.len()];
        let mut users: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut queue = Vec::new();
        for (i, b) in bindings.iter().enumerate() {
            if b.pinned {
                alive[i] = true;
                queue.push(i);
                continue;
            }
            for &r in &b.refs {
                let within = container_of(r);
                if within.is_empty() {
                    if !alive[i] {
                        alive[i] = true;
                        queue.push(i);
                    }
                } else {
                    for c in within {
                        users.entry(c).or_default().push(i);
                    }
                }
            }
        }
        while let Some(c) = queue.pop() {
            for &target in users.get(&c).map(Vec::as_slice).unwrap_or_default() {
                if !alive[target] {
                    alive[target] = true;
                    queue.push(target);
                }
            }
        }
        let dead = (0..bindings.len()).filter(|i| !alive[*i]).collect();
        Graph { bindings, dead }
    }

    fn is_dead(&self, name: &str) -> bool {
        self.bindings
            .iter()
            .enumerate()
            .any(|(i, b)| b.name == name && self.dead.contains(&i))
    }
}

/// Every identifier reference's span, by reference id.
fn reference_spans(program: &Program) -> HashMap<oxc_syntax::reference::ReferenceId, Span> {
    use oxc_ast::ast::IdentifierReference;
    use oxc_ast_visit::Visit;
    struct Collect(HashMap<oxc_syntax::reference::ReferenceId, Span>);
    impl<'a> Visit<'a> for Collect {
        fn visit_identifier_reference(&mut self, it: &IdentifierReference<'a>) {
            if let Some(id) = it.reference_id.get() {
                self.0.insert(id, it.span);
            }
        }
    }
    let mut c = Collect(HashMap::new());
    c.visit_program(program);
    c.0
}

fn declarators(
    var: &VariableDeclaration,
    exported: bool,
    add: &mut impl FnMut(&BindingIdentifier, Span, bool),
) {
    for d in &var.declarations {
        for id in pattern_names(&d.id) {
            add(id, d.span, exported);
        }
    }
}

/// The names a binding pattern declares (Babel's `findVariables`).
fn pattern_names<'p, 'a>(pattern: &'p BindingPattern<'a>) -> Vec<&'p BindingIdentifier<'a>> {
    let mut out = Vec::new();
    fn walk<'p, 'a>(p: &'p BindingPattern<'a>, out: &mut Vec<&'p BindingIdentifier<'a>>) {
        match p {
            BindingPattern::BindingIdentifier(id) => out.push(id),
            BindingPattern::ObjectPattern(o) => {
                for prop in &o.properties {
                    walk(&prop.value, out);
                }
                if let Some(rest) = &o.rest {
                    walk(&rest.argument, out);
                }
            }
            BindingPattern::ArrayPattern(a) => {
                for el in a.elements.iter().flatten() {
                    walk(el, out);
                }
                if let Some(rest) = &a.rest {
                    walk(&rest.argument, out);
                }
            }
            BindingPattern::AssignmentPattern(a) => walk(&a.left, out),
        }
    }
    walk(pattern, &mut out);
    out
}

/// One pass: `None` when nothing is dead.
fn drop_pass(code: &str, path: &str, candidates: &HashSet<String>) -> Option<String> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, code, source_type(path)).parse();
    if parsed.fatal_error {
        return None;
    }
    let semantic = SemanticBuilder::new().build(&parsed.program).semantic;
    let graph = Graph::of(&parsed.program, &semantic);
    let dead = |id: &BindingIdentifier| {
        candidates.contains(id.name.as_str()) && graph.is_dead(id.name.as_str())
    };
    let text = |span: Span| &code[span.start as usize..span.end as usize];
    let mut edits: Vec<(usize, usize, String)> = Vec::new();
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
                let replacement = if kept.is_empty() {
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
                edits.push((start, end, replacement));
            }
            Statement::VariableDeclaration(var) => {
                let mut changed = false;
                let mut kept = Vec::new();
                for d in &var.declarations {
                    match rebuild(&d.id, code, &dead) {
                        Rebuilt::Same => kept.push(text(d.span).to_string()),
                        Rebuilt::Gone => changed = true,
                        Rebuilt::Changed(id) => {
                            changed = true;
                            kept.push(format!(
                                "{id}{}",
                                &code[d.id.span().end as usize..d.span.end as usize]
                            ));
                        }
                    }
                }
                if !changed {
                    continue;
                }
                let replacement = if kept.is_empty() {
                    String::new()
                } else {
                    let kind = &code[start..var.declarations[0].span.start as usize];
                    let semi = if text(span).ends_with(';') { ";" } else { "" };
                    format!("{kind}{}{semi}", kept.join(", "))
                };
                edits.push((start, end, replacement));
            }
            Statement::FunctionDeclaration(f) if f.id.as_ref().is_some_and(dead) => {
                edits.push((start, end, String::new()));
            }
            _ => {}
        }
    }
    if edits.is_empty() {
        return None;
    }
    let mut out = String::with_capacity(code.len());
    let mut last = 0;
    for (start, end, replacement) in edits {
        out.push_str(&code[last..start]);
        let lost = code[start..end].matches('\n').count();
        let kept = replacement.matches('\n').count();
        out.push_str(&replacement);
        out.push_str(&"\n".repeat(lost.saturating_sub(kept)));
        last = end;
    }
    out.push_str(&code[last..]);
    (out != code).then_some(out)
}

enum Rebuilt {
    Same,
    Gone,
    Changed(String),
}

/// A declarator's pattern without its dead names: Babel's VariableDeclarator
/// visitor (and its ObjectPattern/ArrayPattern cleanup of emptied patterns).
fn rebuild(
    pattern: &BindingPattern,
    code: &str,
    dead: &impl Fn(&BindingIdentifier) -> bool,
) -> Rebuilt {
    let text = |span: Span| &code[span.start as usize..span.end as usize];
    match pattern {
        BindingPattern::BindingIdentifier(id) => {
            if dead(id) {
                Rebuilt::Gone
            } else {
                Rebuilt::Same
            }
        }
        BindingPattern::AssignmentPattern(a) => match rebuild(&a.left, code, dead) {
            Rebuilt::Same => Rebuilt::Same,
            Rebuilt::Gone => Rebuilt::Gone,
            Rebuilt::Changed(left) => Rebuilt::Changed(format!(
                "{left}{}",
                &code[a.left.span().end as usize..a.span.end as usize]
            )),
        },
        BindingPattern::ObjectPattern(o) => {
            let mut parts = Vec::new();
            let mut changed = false;
            // A property can only go when no rest element follows it: the rest
            // would gather it instead (Babel's shouldBeRemoved).
            let has_rest = o.rest.is_some();
            for prop in &o.properties {
                let whole = text(prop.span).to_string();
                if has_rest {
                    parts.push(whole);
                    continue;
                }
                match rebuild(&prop.value, code, dead) {
                    Rebuilt::Same => parts.push(whole),
                    Rebuilt::Gone => changed = true,
                    Rebuilt::Changed(value) => {
                        changed = true;
                        parts.push(format!(
                            "{}{value}{}",
                            &code[prop.span.start as usize..prop.value.span().start as usize],
                            &code[prop.value.span().end as usize..prop.span.end as usize]
                        ));
                    }
                }
            }
            if let Some(rest) = &o.rest {
                match rebuild(&rest.argument, code, dead) {
                    Rebuilt::Gone => changed = true,
                    _ => parts.push(text(rest.span).to_string()),
                }
            }
            if !changed {
                Rebuilt::Same
            } else if parts.is_empty() {
                Rebuilt::Gone
            } else {
                Rebuilt::Changed(format!("{{ {} }}", parts.join(", ")))
            }
        }
        BindingPattern::ArrayPattern(a) => {
            let mut parts = Vec::new();
            let mut changed = false;
            for el in &a.elements {
                match el {
                    None => parts.push(String::new()),
                    Some(p) => match rebuild(p, code, dead) {
                        Rebuilt::Same => parts.push(text(p.span()).to_string()),
                        Rebuilt::Gone => {
                            changed = true;
                            parts.push(String::new());
                        }
                        Rebuilt::Changed(t) => {
                            changed = true;
                            parts.push(t);
                        }
                    },
                }
            }
            let mut rest = None;
            if let Some(r) = &a.rest {
                match rebuild(&r.argument, code, dead) {
                    Rebuilt::Gone => changed = true,
                    _ => rest = Some(text(r.span).to_string()),
                }
            }
            if !changed {
                return Rebuilt::Same;
            }
            if rest.is_none() && parts.iter().all(String::is_empty) {
                return Rebuilt::Gone;
            }
            // Trailing holes carry no binding.
            while parts.last().is_some_and(String::is_empty) && rest.is_none() {
                parts.pop();
            }
            if let Some(r) = rest {
                parts.push(r);
            }
            Rebuilt::Changed(format!("[{}]", parts.join(", ")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `before` with `remove` cut out (the edit), then the pass.
    fn dce(before: &str, remove: &str) -> String {
        assert!(before.contains(remove), "{remove} not in {before}");
        let after = before.replacen(remove, "", 1);
        let out = drop_dead_bindings(before, after, "src/x.ts");
        let allocator = Allocator::default();
        let parsed = Parser::new(&allocator, &out, SourceType::ts()).parse();
        assert!(
            parsed.diagnostics.is_empty(),
            "{:?}\n{out}",
            parsed.diagnostics
        );
        assert_eq!(out.lines().count(), before.lines().count(), "lines:\n{out}");
        out
    }

    #[test]
    fn what_only_the_removed_code_used_goes_transitively() {
        let before = "import { getRequest } from \"@tanstack/react-start/server\";\n\
                      import { keep } from \"./keep\";\n\
                      const header = () => getRequest()?.headers.get(\"x\");\n\
                      function meta() { return header(); }\n\
                      export const a = keep(() => meta());\n";
        let out = dce(before, "() => meta()");
        for gone in ["getRequest", "header", "meta"] {
            assert!(!out.contains(gone), "{gone} left in:\n{out}");
        }
        assert!(out.contains("import { keep } from \"./keep\";"), "{out}");
    }

    // findRemovableBindings: a self-recursive helper the removed code was the
    // only user of goes, and the import only it used goes with it.
    #[test]
    fn a_self_recursive_helper_goes_with_its_import() {
        let before = "import { getRequest } from \"@tanstack/react-start/server\";\n\
                      function hdr(n: string, d = 0): string | null { return d > 2 ? null : getRequest()?.headers.get(n) ?? hdr(n, d + 1); }\n\
                      export const f = wrap(() => hdr(\"x\"));\n";
        let out = dce(before, "() => hdr(\"x\")");
        assert!(!out.contains("hdr") && !out.contains("getRequest"), "{out}");
    }

    #[test]
    fn helpers_that_only_call_each_other_go_together() {
        let before = "import { getRequest } from \"@tanstack/react-start/server\";\n\
                      const a = (n: number): unknown => (n ? b(n - 1) : null);\n\
                      const b = (n: number): unknown => a(n) ?? getRequest();\n\
                      export const f = wrap(() => a(3));\n";
        let out = dce(before, "() => a(3)");
        for gone in ["const a", "const b", "getRequest"] {
            assert!(!out.contains(gone), "{gone} left in:\n{out}");
        }
    }

    // A cycle something live still uses stays.
    #[test]
    fn a_cycle_a_live_binding_uses_stays() {
        let before = "const a = (n: number): unknown => (n ? b(n - 1) : null);\n\
                      const b = (n: number): unknown => a(n);\n\
                      export const live = () => b(1);\n\
                      export const f = wrap(() => a(3));\n";
        let out = dce(before, "() => a(3)");
        assert!(out.contains("const a") && out.contains("const b"), "{out}");
    }

    #[test]
    fn destructured_names_go_and_an_emptied_pattern_takes_its_declarator() {
        let before = "import * as S from \"@tanstack/react-start/server\";\n\
                      import { cfg } from \"./cfg\";\n\
                      const { getRequest } = S;\n\
                      const { host, port } = cfg;\n\
                      const [first, second] = list();\n\
                      export const f = wrap(() => getRequest() + port + first);\n\
                      export const g = host + second;\n";
        let out = dce(before, "() => getRequest() + port + first");
        assert!(!out.contains("getRequest") && !out.contains(" S;"), "{out}");
        assert!(!out.contains("@tanstack/react-start/server"), "{out}");
        assert!(out.contains("const { host } = cfg;"), "{out}");
        assert!(out.contains("const [, second] = list();"), "{out}");
    }

    // Babel's rest rule: with a rest element, the other properties stay (the
    // rest would gather them); a dead rest itself goes.
    #[test]
    fn a_rest_element_keeps_the_properties_before_it() {
        let before = "const { a, ...others } = obj;\n\
                      const { b, ...gone } = obj;\n\
                      export const f = wrap(() => a + gone.x);\n\
                      export const g = others + b;\n";
        let out = dce(before, "() => a + gone.x");
        assert!(out.contains("const { a, ...others } = obj;"), "{out}");
        assert!(out.contains("const { b } = obj;"), "{out}");
    }

    #[test]
    fn exports_reassigned_and_already_dead_bindings_stay() {
        let before = "import { side } from \"./side\";\n\
                      import \"./effect\";\n\
                      import { unusedFromStart } from \"./unused\";\n\
                      export function exported() { return 1; }\n\
                      let counter = 0;\n\
                      function bump() { counter = counter + 1; }\n\
                      function deadAlready() { return 2; }\n\
                      export const f = wrap(() => exported() + side() + counter);\n\
                      bump();\n";
        let out = dce(before, "() => exported() + side() + counter");
        assert!(out.contains("export function exported()"), "{out}");
        assert!(out.contains("let counter = 0;"), "reassigned: {out}");
        assert!(out.contains("import \"./effect\";"), "{out}");
        assert!(
            out.contains("unusedFromStart"),
            "dead before the edit: {out}"
        );
        assert!(
            out.contains("function deadAlready"),
            "dead before the edit: {out}"
        );
        assert!(!out.contains("import { side }"), "{out}");
    }

    #[test]
    fn an_import_keeps_its_live_specifiers() {
        let before = "import def, { a, b as c } from \"./m\";\n\
                      export const f = wrap(() => a + def);\n\
                      export const g = c;\n";
        let out = dce(before, "() => a + def");
        assert!(out.contains("import { b as c } from \"./m\";"), "{out}");
    }
}
