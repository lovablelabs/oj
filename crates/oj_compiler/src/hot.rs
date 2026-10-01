use crate::compile::{HotAccept, ImportRewriter};
use crate::rewrite::rewrite_literal;
use oxc_allocator::Allocator;
use oxc_ast::ast::{
    Argument, ArrayExpressionElement, CallExpression, Expression, Program, StaticMemberExpression,
    StringLiteral,
};
use oxc_ast_visit::{walk_mut, VisitMut};

/// Find `import.meta.hot` references and lex the `accept` calls, rewriting the
/// dependency specifiers they name with the same rewriter as the imports so the
/// client can match them against the update's `acceptedPath`.
pub(crate) fn lex_hot_accept<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut Option<&mut ImportRewriter>,
) -> Option<HotAccept> {
    let mut noop = |_: &str| None;
    let rewriter: &mut ImportRewriter = match rewriter.as_deref_mut() {
        Some(rw) => rw,
        None => &mut noop,
    };
    let mut lexer = Lexer {
        allocator,
        rewriter,
        uses_hot: false,
        accept: HotAccept::default(),
    };
    lexer.visit_program(program);
    if !lexer.uses_hot {
        return None;
    }
    let mut accept = lexer.accept;
    accept.deps.sort();
    accept.deps.dedup();
    Some(accept)
}

fn is_import_meta_hot_member(m: &StaticMemberExpression) -> bool {
    m.property.name == "hot" && matches!(&m.object, Expression::ImportMeta(_))
}

fn is_import_meta_hot(e: &Expression) -> bool {
    match e {
        Expression::StaticMemberExpression(m) => is_import_meta_hot_member(m),
        Expression::ParenthesizedExpression(p) => is_import_meta_hot(&p.expression),
        _ => false,
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum AcceptKind {
    /// `accept()`, `accept(cb)`, `accept(dep | [deps], cb)`.
    Deps,
    /// `acceptExports(name | [names], cb)`.
    Exports,
}

fn accept_kind(callee: &Expression) -> Option<AcceptKind> {
    let Expression::StaticMemberExpression(m) = callee else {
        return None;
    };
    if !is_import_meta_hot(&m.object) {
        return None;
    }
    match m.property.name.as_str() {
        "accept" => Some(AcceptKind::Deps),
        "acceptExports" => Some(AcceptKind::Exports),
        _ => None,
    }
}

struct Lexer<'a, 'r> {
    allocator: &'a Allocator,
    rewriter: &'r mut ImportRewriter<'r>,
    uses_hot: bool,
    accept: HotAccept,
}

impl<'a> Lexer<'a, '_> {
    fn rewrite(&mut self, lit: &mut StringLiteral<'a>) -> String {
        rewrite_literal(self.allocator, lit, self.rewriter);
        lit.value.to_string()
    }

    fn record_accept(&mut self, kind: AcceptKind, first_arg: Option<&mut Argument<'a>>) {
        match (kind, first_arg) {
            (_, None) => self.accept.self_accepting = true,
            // acceptExports is a PARTIAL accept (Vite's acceptedHmrExports), not
            // a self-accept: an importer using an unlisted export propagates past it.
            (AcceptKind::Exports, Some(Argument::StringLiteral(lit))) => {
                let name = lit.value.to_string();
                self.accepted_exports().push(name);
            }
            (AcceptKind::Exports, Some(Argument::ArrayExpression(arr))) => {
                let names = self.accepted_exports();
                for el in arr.elements.iter() {
                    if let ArrayExpressionElement::StringLiteral(lit) = el {
                        names.push(lit.value.to_string());
                    }
                }
            }
            (AcceptKind::Deps, Some(Argument::StringLiteral(lit))) => {
                let dep = self.rewrite(lit);
                self.accept.deps.push(dep);
            }
            (AcceptKind::Deps, Some(Argument::ArrayExpression(arr))) => {
                for el in arr.elements.iter_mut() {
                    if let ArrayExpressionElement::StringLiteral(lit) = el {
                        let dep = self.rewrite(lit);
                        self.accept.deps.push(dep);
                    }
                }
            }
            // accept(cb) or anything dynamic
            (_, Some(_)) => self.accept.self_accepting = true,
        }
    }

    fn accepted_exports(&mut self) -> &mut Vec<String> {
        self.accept.accepted_exports.get_or_insert_with(Vec::new)
    }
}

impl<'a> VisitMut<'a> for Lexer<'a, '_> {
    fn visit_static_member_expression(&mut self, it: &mut StaticMemberExpression<'a>) {
        if is_import_meta_hot_member(it) {
            self.uses_hot = true;
        }
        walk_mut::walk_static_member_expression(self, it);
    }

    fn visit_call_expression(&mut self, call: &mut CallExpression<'a>) {
        if let Some(kind) = accept_kind(&call.callee) {
            self.uses_hot = true;
            self.record_accept(kind, call.arguments.first_mut());
        }
        walk_mut::walk_call_expression(self, call);
    }
}

#[cfg(test)]
mod tests {

    use std::path::Path;

    use crate::compile::{compile_module, CompileOptions, HotAccept};

    #[test]
    fn lexes_import_meta_hot_accept_forms_and_rewrites_dep_specifiers() {
        let mut rw = |spec: &str| spec.strip_prefix("./").map(|r| format!("/src/{r}"));
        let none = compile_module(
            Path::new("a.ts"),
            "export const x = 1;",
            &CompileOptions::dev(),
            Some(&mut rw),
        )
        .unwrap();
        assert_eq!(none.hot_accept, None);

        let self_accept = compile_module(
            Path::new("a.ts"),
            "export const x = 1;\nif (import.meta.hot) { import.meta.hot.accept(); }",
            &CompileOptions::dev(),
            Some(&mut rw),
        )
        .unwrap();
        assert_eq!(
            self_accept.hot_accept,
            Some(HotAccept {
                self_accepting: true,
                deps: vec![],
                accepted_exports: None
            })
        );

        let cb = compile_module(
            Path::new("a.ts"),
            "import.meta.hot?.accept((m) => m);",
            &CompileOptions::dev(),
            Some(&mut rw),
        )
        .unwrap();
        assert!(cb.hot_accept.unwrap().self_accepting);

        let deps = compile_module(
            Path::new("a.ts"),
            "import { v } from './util.js';\nimport.meta.hot.accept(['./util.js', './other.js'], ([u]) => u);\nimport.meta.hot.accept('./one.js', (m) => m);",
            &CompileOptions::dev(),
            Some(&mut rw),
        )
        .unwrap();
        let hot = deps.hot_accept.unwrap();
        assert!(!hot.self_accepting);
        assert_eq!(
            hot.deps,
            vec!["/src/one.js", "/src/other.js", "/src/util.js"]
        );
        assert!(
            deps.code.contains("\"/src/util.js\", \"/src/other.js\"")
                || deps.code.contains("\"/src/util.js\",\"/src/other.js\""),
            "{}",
            deps.code
        );
        assert!(deps.code.contains("\"/src/one.js\""), "{}", deps.code);

        let read_only = compile_module(
            Path::new("a.ts"),
            "console.log(import.meta.hot?.data);",
            &CompileOptions::dev(),
            Some(&mut rw),
        )
        .unwrap();
        assert_eq!(
            read_only.hot_accept,
            Some(HotAccept::default()),
            "referencing import.meta.hot needs a context even without accept"
        );

        // acceptExports is a PARTIAL accept (Vite's acceptedHmrExports), not a
        // self-accept: the graph gates propagation on the importers' bindings.
        let partial = compile_module(
            Path::new("a.ts"),
            "export const a = 1, b = 2;\nimport.meta.hot.acceptExports([\"a\"], (m) => m);\nimport.meta.hot.acceptExports(\"c\");",
            &CompileOptions::dev(),
            Some(&mut rw),
        )
        .unwrap();
        let hot = partial.hot_accept.unwrap();
        assert!(!hot.self_accepting);
        assert_eq!(hot.accepted_exports, Some(vec!["a".into(), "c".into()]));

        // Vite's promotion (importAnalysis): a list covering EVERY export the
        // module has makes it fully self-accepting, so even a namespace or
        // dynamic importer hot-swaps through it.
        let all = compile_module(
            Path::new("a.ts"),
            "export const a = 1;\nexport default 2;\nimport.meta.hot.acceptExports([\"a\", \"default\"], (m) => m);",
            &CompileOptions::dev(),
            Some(&mut rw),
        )
        .unwrap();
        let hot = all.hot_accept.unwrap();
        assert!(
            hot.self_accepting,
            "acceptExports covering all exports self-accepts"
        );
        assert_eq!(
            hot.accepted_exports,
            Some(vec!["a".into(), "default".into()])
        );
    }
}
