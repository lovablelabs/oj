//! Port of tsgolint's `internal/rules/no_unsafe_type_assertion/no_unsafe_type_assertion.go`.

use tsrs_ast::{Kind, Node};
use tsrs_checker::{ObjectFlags, Type, TypeFlags};
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils;

pub struct NoUnsafeTypeAssertion;

fn any_type_name(ty: P<Type>) -> &'static str {
    if utils::is_intrinsic_error_type(ty) { "error typed" } else { "`any`" }
}

#[derive(Clone, Copy)]
#[expect(
    clippy::enum_variant_names,
    reason = "The variant names preserve the upstream tsgolint message ids."
)]
enum MessageId {
    UnsafeOfAnyTypeAssertion,
    UnsafeToAnyTypeAssertion,
    UnsafeToUnconstrainedTypeAssertion,
    UnsafeTypeAssertion,
    UnsafeTypeAssertionAssignableToConstraint,
}

impl MessageId {
    fn message(self, ty: &str) -> RuleMessage {
        let (id, description) = match self {
            Self::UnsafeOfAnyTypeAssertion => {
                ("unsafeOfAnyTypeAssertion", format!("Unsafe assertion from {ty} detected."))
            }
            Self::UnsafeToAnyTypeAssertion => {
                ("unsafeToAnyTypeAssertion", format!("Unsafe assertion to {ty} detected."))
            }
            Self::UnsafeToUnconstrainedTypeAssertion => (
                "unsafeToUnconstrainedTypeAssertion",
                format!(
                    "Unsafe type assertion: '{ty}' could be instantiated with an arbitrary type which could be unrelated to the original type."
                ),
            ),
            Self::UnsafeTypeAssertion => (
                "unsafeTypeAssertion",
                format!(
                    "Unsafe type assertion: type '{ty}' is more narrow than the original type."
                ),
            ),
            Self::UnsafeTypeAssertionAssignableToConstraint => (
                "unsafeTypeAssertionAssignableToConstraint",
                format!(
                    "Unsafe type assertion: the original type is assignable to the constraint of type '{ty}', but '{ty}' could be instantiated with a different subtype of its constraint."
                ),
            ),
        };
        RuleMessage { id: id.into(), description, ..Default::default() }
    }

    fn guidance(self) -> &'static str {
        match self {
            Self::UnsafeOfAnyTypeAssertion => {
                "Use a type guard to check the value before asserting its type."
            }
            Self::UnsafeToAnyTypeAssertion => {
                "Keep the original type or use `unknown` and narrow the value before use."
            }
            _ => "Use a type guard to narrow the value before asserting a more specific type.",
        }
    }
}

fn report(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    original: P<Type>,
    asserted: P<Type>,
    id: MessageId,
    ty: &str,
) {
    let render = |checker: &mut tsrs_checker::Checker, ty| {
        if utils::is_intrinsic_error_type(ty) {
            Some("error".into())
        } else {
            String::from_utf8(checker.type_to_string_bytes_exported(ty)).ok()
        }
    };
    let Some(original) = render(context.checker, original) else {
        return;
    };
    let Some(asserted) = render(context.checker, asserted) else {
        return;
    };
    let guidance = id.guidance();
    let mut message = id.message(ty);
    message.help = format!(
        "The original expression has type `{original}`, and the assertion changes it to `{asserted}`. {guidance}"
    );
    context.report_node(node, message);
}

impl Rule for NoUnsafeTypeAssertion {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::AsExpression | Kind::TypeAssertionExpression))
    }
    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let expression = node.expression().unwrap();
        let annotation = node.type_node().unwrap();
        let expression_type = context.checker.get_type_at_location(expression);
        let asserted_type = context.checker.get_type_at_location(annotation);
        if expression_type == asserted_type {
            return;
        }
        if asserted_type.flags().intersects(TypeFlags::Any)
            && expression_type.flags().intersects(TypeFlags::Unknown)
        {
            report(
                context,
                node,
                expression_type,
                asserted_type,
                MessageId::UnsafeToAnyTypeAssertion,
                "`any`",
            );
            return;
        }
        if let Some((_, sender)) = utils::is_unsafe_assignment(
            expression_type,
            asserted_type,
            context.checker,
            Some(expression),
        ) {
            report(
                context,
                node,
                expression_type,
                asserted_type,
                MessageId::UnsafeOfAnyTypeAssertion,
                any_type_name(sender),
            );
            return;
        }
        if let Some((_, sender)) = utils::is_unsafe_assignment(
            asserted_type,
            expression_type,
            context.checker,
            Some(annotation),
        ) {
            report(
                context,
                node,
                expression_type,
                asserted_type,
                MessageId::UnsafeToAnyTypeAssertion,
                any_type_name(sender),
            );
            return;
        }
        let widened_type = if expression_type.flags().intersects(TypeFlags::Object)
            && expression_type.object_flags().intersects(ObjectFlags::ObjectLiteral)
        {
            context.checker.get_widened_type(expression_type)
        } else {
            expression_type
        };
        if context.checker.is_type_assignable_to(widened_type, asserted_type) {
            return;
        }
        if asserted_type.flags().intersects(TypeFlags::TypeParameter) {
            let constraint = context.checker.get_base_constraint_of_type(asserted_type);
            let Some(constraint) = constraint else {
                let ty = context.checker.type_to_string_exported(asserted_type);
                report(
                    context,
                    node,
                    expression_type,
                    asserted_type,
                    MessageId::UnsafeToUnconstrainedTypeAssertion,
                    &ty,
                );
                return;
            };
            if context.checker.is_type_assignable_to(widened_type, constraint) {
                let ty = context.checker.type_to_string_exported(asserted_type);
                report(
                    context,
                    node,
                    expression_type,
                    asserted_type,
                    MessageId::UnsafeTypeAssertionAssignableToConstraint,
                    &ty,
                );
                return;
            }
        }
        let ty = context.checker.type_to_string_exported(asserted_type);
        report(context, node, expression_type, asserted_type, MessageId::UnsafeTypeAssertion, &ty);
    }
}
