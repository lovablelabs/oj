use tsrs_ast::{self as ast, Kind, Node};
use tsrs_core::P;

use crate::protocol::{Fix, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils::{get_constrained_type_at_location, type_recurser};

pub struct PreferReduceTypeParameter;

fn build_prefer_type_parameter_message() -> RuleMessage {
    RuleMessage {
        id: "preferTypeParameter".into(),
        description:
            "Unnecessary assertion: Array#reduce accepts a type parameter for the default value."
                .into(),
        ..Default::default()
    }
}

impl Rule for PreferReduceTypeParameter {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::CallExpression)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let expr = node.as_call_expression();
        let Some(&second_arg) = expr.arguments().nodes().get(1) else {
            return;
        };
        if !matches!(second_arg.kind(), Kind::AsExpression | Kind::TypeAssertionExpression) {
            return;
        }

        let callee = expr.expression;
        if !ast::is_access_expression(callee) {
            return;
        }
        let (property_name, found) = context.checker.get_accessed_property_name(callee);
        if !found || property_name != "reduce" {
            return;
        }

        let assertion_expr = second_arg.expression().unwrap();
        let assertion_type = second_arg.type_node().unwrap();
        let initializer_type = context.checker.get_type_at_location(assertion_expr);
        let asserted_type = context.checker.get_type_at_location(assertion_type);
        if !context.checker.is_type_assignable_to(initializer_type, asserted_type) {
            return;
        }

        let callee_obj_type =
            get_constrained_type_at_location(context.checker, callee.expression().unwrap());
        if type_recurser(callee_obj_type, &mut |t| !context.checker.is_array_or_tuple_type(t)) {
            return;
        }

        let source_file = context.source_file;
        context.report_node_with_fixes(second_arg, build_prefer_type_parameter_message(), || {
            let assertion_type_end = assertion_type.end().expect_source("parsed lint type");
            let range = if second_arg.kind() == Kind::AsExpression {
                Range {
                    pos: assertion_expr.end().expect_source("parsed lint expression"),
                    end: assertion_type_end,
                }
            } else {
                Range {
                    pos: second_arg.pos().expect_source("parsed lint assertion"),
                    end: assertion_expr.pos().expect_source("parsed lint expression"),
                }
            };
            let mut fixes = vec![Fix { range, text: String::new() }];
            if expr.type_arguments().is_none() {
                let callee_end = callee.end().expect_source("parsed lint callee");
                let type_pos = assertion_type.pos().expect_source("parsed lint type");
                fixes.push(Fix {
                    range: Range { pos: callee_end, end: callee_end },
                    text: format!(
                        "<{}>",
                        &source_file.text()[type_pos as usize..assertion_type_end as usize]
                    ),
                });
            }
            fixes
        });
    }
}
