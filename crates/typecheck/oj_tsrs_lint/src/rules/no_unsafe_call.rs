//! Port of tsgolint's `internal/rules/no_unsafe_call/no_unsafe_call.go`.

use tsrs_ast::{Kind, Node};
use tsrs_checker::{SignatureKind, TypeFlags};
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils;

pub struct NoUnsafeCall;

#[derive(Clone, Copy)]
#[expect(
    clippy::enum_variant_names,
    reason = "The variant names preserve the upstream tsgolint message ids."
)]
enum MessageId {
    UnsafeCall,
    UnsafeCallThis,
    UnsafeNew,
    UnsafeTemplateTag,
}

impl MessageId {
    fn message(self, ty: &str) -> RuleMessage {
        let (id, description, help) = match self {
            Self::UnsafeCall => {
                ("unsafeCall", format!("Unsafe call of a(n) {ty} typed value."), "")
            }
            Self::UnsafeCallThis => (
                "unsafeCallThis",
                format!("Unsafe call of a(n) {ty} typed value. `this` is typed as {ty}.\n"),
                "You can try to fix this by turning on the `noImplicitThis` compiler option, or adding a `this` parameter to the function.",
            ),
            Self::UnsafeNew => {
                ("unsafeNew", format!("Unsafe construction of a(n) {ty} typed value."), "")
            }
            Self::UnsafeTemplateTag => {
                ("unsafeTemplateTag", format!("Unsafe use of a(n) {ty} typed template tag."), "")
            }
        };
        RuleMessage { id: id.into(), description, help: help.into() }
    }
}

fn check_call(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    reporting_node: P<Node>,
    mut id: MessageId,
    new_call: bool,
) {
    let ty = utils::get_constrained_type_at_location(context.checker, node);
    if ty.flags().intersects(TypeFlags::Any) {
        let options = context.program.options();
        if !options.get_strict_option_value(options.no_implicit_this)
            && let Some(this) = utils::get_this_expression(node)
            && utils::get_constrained_type_at_location(context.checker, this)
                .flags()
                .intersects(TypeFlags::Any)
        {
            id = MessageId::UnsafeCallThis;
        }
        let description = if utils::is_intrinsic_error_type(ty) { "`error` type" } else { "`any`" };
        context.report_node(reporting_node, id.message(description));
        return;
    }
    if utils::is_builtin_symbol_like(context.program, context.checker, ty, &["Function"]) {
        if !context.checker.get_signatures_of_type(ty, SignatureKind::Construct).is_empty() {
            return;
        }
        let signatures = context.checker.get_signatures_of_type(ty, SignatureKind::Call);
        if new_call {
            if signatures.iter().any(|&signature| {
                !context
                    .checker
                    .get_return_type_of_signature_exported(signature)
                    .flags()
                    .intersects(TypeFlags::Void)
            }) {
                return;
            }
        } else if !signatures.is_empty() {
            return;
        }
        context.report_node(reporting_node, id.message("`Function`"));
    }
}

impl Rule for NoUnsafeCall {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::CallExpression | Kind::NewExpression | Kind::TaggedTemplateExpression
            )
        )
    }
    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match listener {
            Listener::Enter(Kind::CallExpression) => {
                let callee = node.expression().unwrap();
                if callee.kind() != Kind::ImportKeyword {
                    check_call(context, callee, callee, MessageId::UnsafeCall, false);
                }
            }
            Listener::Enter(Kind::NewExpression) => {
                check_call(context, node.expression().unwrap(), node, MessageId::UnsafeNew, true);
            }
            Listener::Enter(Kind::TaggedTemplateExpression) => {
                let tag = node.as_tagged_template_expression().tag;
                check_call(context, tag, tag, MessageId::UnsafeTemplateTag, false);
            }
            _ => unreachable!("no-unsafe-call listener"),
        }
    }
}
