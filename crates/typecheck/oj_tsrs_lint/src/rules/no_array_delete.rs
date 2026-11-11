//! Port of tsgolint's `internal/rules/no_array_delete/no_array_delete.go`.

use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{Checker, Type, TypeFlags};
use tsrs_core::P;

use crate::protocol::{Diagnostic, Fix, LabeledRange, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::get_constrained_type_at_location;

pub struct NoArrayDelete;

fn is_underlying_type_array(checker: &mut Checker, ty: P<Type>) -> bool {
    if ty.flags().intersects(TypeFlags::Union) {
        ty.types().iter().all(|ty| checker.is_array_or_tuple_type(*ty))
    } else if ty.flags().intersects(TypeFlags::Intersection) {
        ty.types().iter().any(|ty| checker.is_array_or_tuple_type(*ty))
    } else {
        checker.is_array_or_tuple_type(ty)
    }
}

fn token_range(context: &RuleContext<'_>, pos: u32) -> Range {
    let range = tsrs_scanner::get_range_of_token_at_position(context.source_file, pos);
    Range { pos: range.source_pos().unwrap(), end: range.source_end().unwrap() }
}

impl Rule for NoArrayDelete {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::DeleteExpression)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let deleted = ast::skip_parentheses(node.as_delete_expression().expression());
        if deleted.kind() != Kind::ElementAccessExpression {
            return;
        }
        let element = deleted.as_element_access_expression();
        let ty = get_constrained_type_at_location(context.checker, element.expression);
        if !is_underlying_type_array(context.checker, ty) {
            return;
        }
        let delete_range = token_range(context, node.pos().expect_source("parsed lint node"));
        let array_range = trim_node_range(context.source_file, element.expression);
        let suggestions = if context.options.fix_suggestions {
            let argument_range = trim_node_range(context.source_file, element.argument_expression);
            vec![Suggestion {
                message: RuleMessage {
                    id: "useSplice".into(),
                    description: "Use `array.splice()` instead.".into(),
                    ..Default::default()
                },
                fixes: vec![
                    Fix { range: delete_range, text: String::new() },
                    Fix { range: token_range(context, array_range.end), text: ".splice(".into() },
                    Fix { range: token_range(context, argument_range.end), text: ", 1)".into() },
                ],
            }]
        } else {
            Vec::new()
        };
        context.report_diagnostic(Diagnostic {
            range: Some(delete_range),
            message: RuleMessage {
                id: "noArrayDelete".into(),
                description: "Using the `delete` operator with an array expression is unsafe."
                    .into(),
                ..Default::default()
            },
            labeled_ranges: vec![LabeledRange {
                label: "This expression evaluates to an array.".into(),
                range: array_range,
            }],
            suggestions,
            ..Default::default()
        });
    }
}
