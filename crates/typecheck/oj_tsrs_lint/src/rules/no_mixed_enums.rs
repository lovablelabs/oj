//! Port of tsgolint's `internal/rules/no_mixed_enums/no_mixed_enums.go`.

use tsrs_ast::{Kind, Node};
use tsrs_checker::TypeFlags;
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};

#[derive(Clone, Copy, PartialEq, Eq)]
enum AllowedType {
    Number,
    String,
    Unknown,
}

fn get_member_type(context: &mut RuleContext<'_>, node: P<Node>) -> AllowedType {
    let Some(initializer) = node.as_enum_member().initializer() else {
        return AllowedType::Number;
    };
    match initializer.kind() {
        Kind::NumericLiteral => AllowedType::Number,
        Kind::StringLiteral => AllowedType::String,
        _ => {
            let t = context.checker.get_type_at_location(initializer);
            if t.flags().intersects(TypeFlags::StringLike) {
                AllowedType::String
            } else if t.flags().intersects(TypeFlags::NumberLike) {
                AllowedType::Number
            } else {
                AllowedType::Unknown
            }
        }
    }
}

pub struct NoMixedEnums;

impl Rule for NoMixedEnums {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::EnumDeclaration)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        if node.members().is_empty() {
            return;
        }
        let symbol = context
            .checker
            .get_symbol_at_location_exported(node.name().expect("enum name"))
            .expect("enum symbol");
        let declaration = symbol.declarations()[0];
        let desired_type = get_member_type(context, declaration.members()[0]);
        if desired_type == AllowedType::Unknown {
            return;
        }
        for &member in node.members() {
            let current_type = get_member_type(context, member);
            if current_type == AllowedType::Unknown {
                return;
            }
            if current_type != desired_type {
                context.report_node(
                    member.as_enum_member().initializer().unwrap_or(member),
                    RuleMessage {
                        id: "mixed".into(),
                        description: "Mixing number and string enums can be confusing.".into(),
                        ..Default::default()
                    },
                );
                return;
            }
        }
    }
}
