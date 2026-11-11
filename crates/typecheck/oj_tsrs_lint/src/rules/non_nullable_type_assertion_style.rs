use rustc_hash::FxHashSet;
use tsrs_ast::{self as ast, Kind, Node, OperatorPrecedence};
use tsrs_checker::{Checker, Type, TypeFlags};
use tsrs_core::P;

use crate::protocol::{Fix, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::union_type_parts;

pub struct NonNullableTypeAssertionStyle;

fn build_prefer_non_null_assertion_message() -> RuleMessage {
    RuleMessage {
        id: "preferNonNullAssertion".into(),
        description:
            "Use a ! assertion to more succinctly remove null and undefined from the type.".into(),
        ..Default::default()
    }
}

fn get_types_if_not_loose(checker: &mut Checker, node: P<Node>) -> Option<P<Type>> {
    let t = checker.get_type_at_location(node);
    if t.flags().intersects(TypeFlags::Any | TypeFlags::Unknown) { None } else { Some(t) }
}

fn could_be_nullable(checker: &mut Checker, mut t: P<Type>) -> bool {
    if t.flags().intersects(TypeFlags::TypeParameter) {
        let Some(constraint) = checker.get_base_constraint_of_type(t) else {
            return true;
        };
        t = constraint;
    }
    union_type_parts(&t).iter().any(|part| part.flags().intersects(TypeFlags::Nullable))
}

impl Rule for NonNullableTypeAssertionStyle {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::AsExpression | Kind::TypeAssertionExpression))
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        if ast::is_const_assertion(node) {
            return;
        }

        let expression = node.expression().unwrap();
        let Some(original_type) = get_types_if_not_loose(context.checker, expression) else {
            return;
        };
        let original_types = union_type_parts(&original_type);
        let type_annotation = node.type_node().unwrap();
        let Some(asserted_type) = get_types_if_not_loose(context.checker, type_annotation) else {
            return;
        };
        let asserted_types = union_type_parts(&asserted_type);

        let non_nullable_original_type: FxHashSet<_> = original_types
            .iter()
            .copied()
            .filter(|t| !t.flags().intersects(TypeFlags::Nullable))
            .collect();
        if non_nullable_original_type.len() == original_types.len() {
            return;
        }

        let mut asserted_types_set = FxHashSet::default();
        for &t in asserted_types {
            if could_be_nullable(context.checker, t) || !non_nullable_original_type.contains(&t) {
                return;
            }
            asserted_types_set.insert(t);
        }
        if non_nullable_original_type != asserted_types_set {
            return;
        }

        let higher_precedence_than_unary =
            ast::get_expression_precedence(expression) > OperatorPrecedence::Unary;
        let expression_end = expression.end().expect_source("parsed lint expression");
        let remove_range = if ast::is_assertion_expression(node) {
            Range { pos: expression_end, end: node.end().expect_source("parsed lint assertion") }
        } else {
            Range {
                pos: node.pos().expect_source("parsed lint assertion"),
                end: expression.pos().expect_source("parsed lint expression"),
            }
        };
        let mut fixes = vec![Fix { range: remove_range, text: String::new() }];
        if higher_precedence_than_unary {
            fixes.push(Fix {
                range: Range { pos: expression_end, end: expression_end },
                text: "!".into(),
            });
        } else {
            let expression_start = trim_node_range(context.source_file, expression).pos;
            fixes.push(Fix {
                range: Range { pos: expression_start, end: expression_start },
                text: "(".into(),
            });
            fixes.push(Fix {
                range: Range { pos: expression_end, end: expression_end },
                text: ")!".into(),
            });
        }
        context.report_node_with_suggestions(
            node,
            build_prefer_non_null_assertion_message(),
            || vec![Suggestion { message: build_prefer_non_null_assertion_message(), fixes }],
        );
    }
}
