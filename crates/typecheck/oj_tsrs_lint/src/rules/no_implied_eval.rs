//! Port of tsgolint's `internal/rules/no_implied_eval/no_implied_eval.go`.

use tsrs_ast::{self as ast, Kind, Node, SymbolFlags};
use tsrs_checker::SignatureKind;
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils::is_builtin_symbol_like;

const GLOBAL_CANDIDATES: [&str; 3] = ["global", "globalThis", "window"];
const EVAL_LIKE_FUNCTIONS: [&str; 4] = ["execScript", "setImmediate", "setInterval", "setTimeout"];

pub struct NoImpliedEval;

fn get_callee_name(node: P<Node>) -> &'static str {
    if node.kind() == Kind::Identifier {
        return node.text();
    }
    if ast::is_access_expression(node) {
        let expression = node.expression().unwrap();
        if expression.kind() == Kind::Identifier && GLOBAL_CANDIDATES.contains(&expression.text()) {
            if node.kind() == Kind::PropertyAccessExpression {
                let name = node.as_property_access_expression().name();
                if name.kind() == Kind::Identifier {
                    return name.text();
                }
            } else {
                let argument = node.as_element_access_expression().argument_expression;
                if argument.kind() == Kind::StringLiteral {
                    return argument.text();
                }
            }
        }
    }
    ""
}

fn is_function_type(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let t = context.checker.get_type_at_location(node);
    if t.symbol().is_some_and(|symbol| {
        symbol.flags().intersects(SymbolFlags::Function | SymbolFlags::Method)
    }) {
        return true;
    }
    if is_builtin_symbol_like(context.program, context.checker, t, &["Function"]) {
        return true;
    }
    !context.checker.get_signatures_of_type(t, SignatureKind::Call).is_empty()
}

fn is_bind(mut node: P<Node>) -> bool {
    if node.kind() == Kind::PropertyAccessExpression {
        node = node.as_property_access_expression().name();
    }
    node.kind() == Kind::Identifier && node.text() == "bind"
}

fn is_function(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    if ast::is_function_like(node) {
        return true;
    }
    if ast::is_literal_expression(node) {
        return false;
    }
    if node.kind() == Kind::CallExpression {
        return is_bind(node.as_call_expression().expression) || is_function_type(context, node);
    }
    is_function_type(context, node)
}

impl Rule for NoImpliedEval {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::CallExpression | Kind::NewExpression))
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let callee = node.expression().unwrap();
        let name = get_callee_name(callee);
        if name.is_empty() {
            return;
        }
        if name == "Function" {
            let t = context.checker.get_type_at_location(callee);
            let no_symbol = t.symbol().is_none();
            if no_symbol
                || is_builtin_symbol_like(
                    context.program,
                    context.checker,
                    t,
                    &["FunctionConstructor"],
                )
            {
                context.report_node(
                    node,
                    RuleMessage {
                        id: "noFunctionConstructor".into(),
                        description:
                            "Implied eval. Do not use the Function constructor to create functions."
                                .into(),
                        ..Default::default()
                    },
                );
                if !no_symbol {
                    return;
                }
            }
        }
        let Some(&handler) = node.arguments().first() else {
            return;
        };
        if EVAL_LIKE_FUNCTIONS.contains(&name) && !is_function(context, handler) {
            let symbol = context.checker.get_symbol_at_location_exported(callee);
            if !symbol.is_some_and(|symbol| {
                symbol.declarations().iter().any(|&declaration| {
                    ast::get_source_file_of_node(declaration) == Some(context.source_file)
                })
            }) {
                context.report_node(
                    handler,
                    RuleMessage {
                        id: "noImpliedEvalError".into(),
                        description: "Implied eval.".into(),
                        help: "Consider passing a function.".into(),
                    },
                );
            }
        }
    }
}
