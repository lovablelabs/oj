use tsrs_ast::{Kind, Node};
use tsrs_checker::{ObjectFlags, SignatureKind, TypeFlags};
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils::union_type_parts;

pub struct NoGeneratedEmptyObjectType;

fn check_node(node: P<Node>, context: &mut RuleContext<'_>) {
    let ty = context.checker.get_type_at_location(node);
    for &part in union_type_parts(&ty) {
        if part.flags().intersects(TypeFlags::Object)
            && !part.object_flags().intersects(ObjectFlags::ClassOrInterface)
            && context.checker.get_properties_of_type(part).is_empty()
            && context.checker.get_index_infos_of_type(part).is_empty()
            && context.checker.get_signatures_of_type(part, SignatureKind::Call).is_empty()
            && context.checker.get_signatures_of_type(part, SignatureKind::Construct).is_empty()
            && context.checker.is_type_assignable_to(context.checker.number_type, part)
            && context.checker.is_type_assignable_to(context.checker.string_type, part)
        {
            context.report_node(node, RuleMessage {
                id: "noGeneratedEmptyObjectType".into(),
                description: "This type resolves to `{}`, the empty object type. This was likely not intentional.".into(),
                ..Default::default()
            });
            return;
        }
    }
}

impl Rule for NoGeneratedEmptyObjectType {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::IntersectionType | Kind::TypeReference))
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        if node.kind() == Kind::TypeReference {
            let mut parent = node.parent().unwrap();
            while parent.kind() == Kind::ParenthesizedType {
                parent = parent.parent().unwrap();
            }
            if node.as_type_reference_node().type_arguments().is_none()
                || parent.kind() == Kind::IntersectionType
            {
                return;
            }
        }
        check_node(node, context);
    }
}
