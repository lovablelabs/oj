//! Port of tsgolint's `internal/rules/no_for_in_array/no_for_in_array.go`.

use tsrs_ast::{Kind, Node};
use tsrs_checker::TypeFlags;
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils::{get_constrained_type_at_location, get_for_statement_head_loc, type_recurser};

pub struct NoForInArray;

impl Rule for NoForInArray {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::ForInStatement)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let t = get_constrained_type_at_location(
            context.checker,
            node.as_for_in_or_of_statement().expression,
        );
        let array_like = type_recurser(t, &mut |t| {
            if context.checker.get_index_type_of_type(t, context.checker.number_type).is_none() {
                return false;
            }
            let Some(length) = context.checker.get_property_of_type(t, "length") else {
                return false;
            };
            context.checker.get_type_of_symbol(length).flags().intersects(TypeFlags::NumberLike)
        });
        if array_like {
            context.report_range(get_for_statement_head_loc(context.source_file, node), RuleMessage {
                id: "forInViolation".into(),
                description: "For-in loops over arrays skips holes, returns indices as strings, and may visit the prototype chain or other enumerable properties.".into(),
                help: "Use a more robust iteration method such as for-of or array.forEach instead.".into(),
            });
        }
    }
}
