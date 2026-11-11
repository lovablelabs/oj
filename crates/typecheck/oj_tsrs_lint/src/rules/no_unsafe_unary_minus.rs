//! Port of tsgolint's `internal/rules/no_unsafe_unary_minus/no_unsafe_unary_minus.go`.

use tsrs_ast::{Kind, Node};
use tsrs_checker::TypeFlags;
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils;

fn build_unary_minus_message(ty: &str) -> RuleMessage {
    RuleMessage {
        id: "unaryMinus".into(),
        description: format!(
            "Argument of unary negation should be assignable to number | bigint but is {ty} instead."
        ),
        ..Default::default()
    }
}

pub struct NoUnsafeUnaryMinus;

impl Rule for NoUnsafeUnaryMinus {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::PrefixUnaryExpression)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let expression = node.as_prefix_unary_expression();
        if expression.operator != Kind::MinusToken {
            return;
        }
        let ty = utils::get_constrained_type_at_location(context.checker, expression.operand);
        for &part in utils::union_type_parts(&ty) {
            if !part.flags().intersects(
                TypeFlags::Any | TypeFlags::Never | TypeFlags::BigIntLike | TypeFlags::NumberLike,
            ) {
                let ty = context.checker.type_to_string_exported(part);
                context.report_node(node, build_unary_minus_message(&ty));
                break;
            }
        }
    }
}
