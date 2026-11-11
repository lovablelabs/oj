use rustc_hash::FxHashMap;
use tsrs_ast::{Kind, Node};
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils;

fn build_mismatch_message() -> RuleMessage {
    RuleMessage {
        id: "mismatch".into(),
        description: "`get()` type should be assignable to its equivalent `set()` type.".into(),
        ..Default::default()
    }
}

fn check_accessors_pair(getter: P<Node>, setter: P<Node>, context: &mut RuleContext<'_>) {
    let get_type = context.checker.get_type_at_location(getter);
    let set_type = context.checker.get_type_at_location(setter.parameters()[0]);
    if !context.checker.is_type_assignable_to(get_type, set_type) {
        context.report_node(getter.type_node().unwrap(), build_mismatch_message());
    }
}

pub struct RelatedGetterSetterPairs;

impl Rule for RelatedGetterSetterPairs {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::ClassDeclaration
                    | Kind::ClassExpression
                    | Kind::InterfaceDeclaration
                    | Kind::TypeLiteral
            )
        )
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let mut get_accessors = FxHashMap::default();
        let mut set_accessors = FxHashMap::default();
        for &member in node.members() {
            if member.kind() == Kind::GetAccessor {
                if member.type_node().is_none() {
                    continue;
                }
                let (name, _) =
                    utils::get_name_from_member(context.source_file, member.name().unwrap());
                if let Some(&setter) = set_accessors.get(&name) {
                    check_accessors_pair(member, setter, context);
                } else {
                    get_accessors.insert(name, member);
                }
            } else if member.kind() == Kind::SetAccessor {
                if member.parameters().len() != 1 {
                    continue;
                }
                let (name, _) =
                    utils::get_name_from_member(context.source_file, member.name().unwrap());
                if let Some(&getter) = get_accessors.get(&name) {
                    check_accessors_pair(getter, member, context);
                } else {
                    set_accessors.insert(name, member);
                }
            }
        }
    }
}
