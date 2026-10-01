use crate::compile::{HotAccept, ImportRewriter};
use oxc_allocator::Allocator;
use oxc_ast::ast::{Program, StringLiteral};

/// Find `import.meta.hot` references and lex the `accept` calls, rewriting the
/// dependency specifiers they name with the same rewriter as the imports so the
/// client can match them against the update's `acceptedPath`.
pub(crate) fn lex_hot_accept<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    rewriter: &mut Option<&mut ImportRewriter>,
) -> Option<HotAccept> {
    use oxc_ast::ast::{
        Argument, ArrayExpressionElement, CallExpression, Expression, StaticMemberExpression,
    };
    use oxc_ast_visit::{walk_mut, VisitMut};

    fn is_import_meta_hot(e: &Expression) -> bool {
        match e {
            Expression::StaticMemberExpression(m) => {
                m.property.name == "hot" && matches!(&m.object, Expression::ImportMeta(_))
            }
            Expression::ParenthesizedExpression(p) => is_import_meta_hot(&p.expression),
            _ => false,
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
            if let Some(new_spec) = (self.rewriter)(lit.value.as_str()) {
                lit.value = self.allocator.alloc_str(&new_spec).into();
                lit.raw = None;
            }
            lit.value.to_string()
        }
    }
    impl<'a> VisitMut<'a> for Lexer<'a, '_> {
        fn visit_static_member_expression(&mut self, it: &mut StaticMemberExpression<'a>) {
            if it.property.name == "hot" && matches!(&it.object, Expression::ImportMeta(_)) {
                self.uses_hot = true;
            }
            walk_mut::walk_static_member_expression(self, it);
        }
        fn visit_call_expression(&mut self, call: &mut CallExpression<'a>) {
            let is_accept = match &call.callee {
                Expression::StaticMemberExpression(m) => {
                    (m.property.name == "accept" || m.property.name == "acceptExports")
                        && is_import_meta_hot(&m.object)
                }
                _ => false,
            };
            if is_accept {
                self.uses_hot = true;
                let is_exports = matches!(&call.callee, Expression::StaticMemberExpression(m) if m.property.name == "acceptExports");
                match call.arguments.first_mut() {
                    None => self.accept.self_accepting = true,
                    // acceptExports("name" | [names], cb): a PARTIAL accept
                    // (Vite's acceptedHmrExports), not a self-accept — an
                    // importer using an unlisted export propagates past it.
                    Some(Argument::StringLiteral(lit)) if is_exports => {
                        let name = lit.value.to_string();
                        self.accept
                            .accepted_exports
                            .get_or_insert_with(Vec::new)
                            .push(name);
                    }
                    Some(Argument::ArrayExpression(arr)) if is_exports => {
                        let names = self.accept.accepted_exports.get_or_insert_with(Vec::new);
                        for el in arr.elements.iter() {
                            if let ArrayExpressionElement::StringLiteral(lit) = el {
                                names.push(lit.value.to_string());
                            }
                        }
                    }
                    Some(Argument::StringLiteral(lit)) if !is_exports => {
                        let dep = self.rewrite(lit);
                        self.accept.deps.push(dep);
                    }
                    Some(Argument::ArrayExpression(arr)) if !is_exports => {
                        for el in arr.elements.iter_mut() {
                            if let ArrayExpressionElement::StringLiteral(lit) = el {
                                let dep = self.rewrite(lit);
                                self.accept.deps.push(dep);
                            }
                        }
                    }
                    // accept(cb) or anything dynamic
                    Some(_) => self.accept.self_accepting = true,
                }
            }
            walk_mut::walk_call_expression(self, call);
        }
    }

    let mut noop = |_: &str| None;
    let accept = match rewriter.as_deref_mut() {
        Some(rw) => {
            let mut lexer = Lexer {
                allocator,
                rewriter: rw,
                uses_hot: false,
                accept: HotAccept::default(),
            };
            lexer.visit_program(program);
            (lexer.uses_hot, lexer.accept)
        }
        None => {
            let mut lexer = Lexer {
                allocator,
                rewriter: &mut noop,
                uses_hot: false,
                accept: HotAccept::default(),
            };
            lexer.visit_program(program);
            (lexer.uses_hot, lexer.accept)
        }
    };
    let (uses_hot, mut accept) = accept;
    if !uses_hot {
        return None;
    }
    accept.deps.sort();
    accept.deps.dedup();
    Some(accept)
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
