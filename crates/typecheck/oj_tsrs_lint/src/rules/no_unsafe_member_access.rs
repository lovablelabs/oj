//! Port of tsgolint's `internal/rules/no_unsafe_member_access/no_unsafe_member_access.go`.

use rustc_hash::FxHashMap;
use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils;

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Unsafe,
    Safe,
    Chained,
}

pub struct NoUnsafeMemberAccess {
    allow_optional_chaining: bool,
    state_cache: FxHashMap<P<Node>, State>,
}

fn data_type(ty: P<Type>) -> &'static str {
    if utils::is_intrinsic_error_type(ty) { "`error` typed" } else { "`any`" }
}

impl NoUnsafeMemberAccess {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("no-unsafe-member-access: options must be an object".into()),
        };
        let allow_optional_chaining =
            match object.and_then(|object| object.get("allowOptionalChaining")) {
                None | Some(Value::Null) => false,
                Some(Value::Bool(value)) => *value,
                _ => {
                    return Err(
                        "no-unsafe-member-access: allowOptionalChaining must be a boolean".into()
                    );
                }
            };
        Ok(Self { allow_optional_chaining, state_cache: FxHashMap::default() })
    }

    fn check_member_expression(&mut self, node: P<Node>, context: &mut RuleContext<'_>) -> State {
        if self.allow_optional_chaining && node.question_dot_token().is_some() {
            self.state_cache.insert(node, State::Chained);
            return State::Chained;
        }
        if let Some(&state) = self.state_cache.get(&node) {
            return state;
        }
        let mut parent = node.parent().expect("member parent");
        while parent.kind() != Kind::SourceFile {
            if parent.kind() == Kind::HeritageClause {
                return State::Safe;
            }
            parent = parent.parent().expect("member ancestor");
        }
        let expression = node.expression().unwrap();
        if ast::is_access_expression(expression)
            && self.check_member_expression(expression, context) == State::Unsafe
        {
            self.state_cache.insert(node, State::Unsafe);
            return State::Unsafe;
        }
        let ty = context.checker.get_type_at_location(expression);
        let state = if ty.flags().intersects(TypeFlags::Any) { State::Unsafe } else { State::Safe };
        self.state_cache.insert(node, state);
        if state == State::Unsafe {
            let (property, prefix, suffix) = if node.kind() == Kind::PropertyAccessExpression {
                (node.name().unwrap(), ".", "")
            } else {
                (node.as_element_access_expression().argument_expression, "[", "]")
            };
            let range = trim_node_range(context.source_file, property);
            let property_name = format!(
                "{prefix}{}{suffix}",
                &context.source_file.text()[range.pos as usize..range.end as usize]
            );
            let options = context.program.options();
            if !options.get_strict_option_value(options.no_implicit_this)
                && let Some(this) = utils::get_this_expression(node)
                && utils::get_constrained_type_at_location(context.checker, this)
                    .flags()
                    .intersects(TypeFlags::Any)
            {
                context.report_node(property, RuleMessage {
                            id: "unsafeThisMemberExpression".into(),
                            description: format!("Unsafe member access {property_name} on an `any` value. `this` is typed as `any`."),
                            help: "You can try to fix this by turning on the `noImplicitThis` compiler option, or adding a `this` parameter to the function.".into(),
                        });
                return state;
            }
            context.report_node(
                property,
                RuleMessage {
                    id: "unsafeMemberExpression".into(),
                    description: format!(
                        "Unsafe member access {property_name} on an {} value.",
                        data_type(ty)
                    ),
                    ..Default::default()
                },
            );
        }
        state
    }
}

impl Rule for NoUnsafeMemberAccess {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(Kind::PropertyAccessExpression | Kind::ElementAccessExpression)
        )
    }
    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        self.check_member_expression(node, context);
        match listener {
            Listener::Enter(Kind::PropertyAccessExpression) => {}
            Listener::Enter(Kind::ElementAccessExpression) => {
                if self.allow_optional_chaining && node.question_dot_token().is_some() {
                    return;
                }
                let argument = node.as_element_access_expression().argument_expression;
                if ast::is_literal_expression(argument) {
                    return;
                }
                let operator = match argument.kind() {
                    Kind::PrefixUnaryExpression => argument.as_prefix_unary_expression().operator,
                    Kind::PostfixUnaryExpression => argument.as_postfix_unary_expression().operator,
                    _ => Kind::Unknown,
                };
                if matches!(operator, Kind::PlusPlusToken | Kind::MinusMinusToken) {
                    return;
                }
                let ty = context.checker.get_type_at_location(argument);
                if ty.flags().intersects(TypeFlags::Any) {
                    let range = trim_node_range(context.source_file, argument);
                    let property_name =
                        &context.source_file.text()[range.pos as usize..range.end as usize];
                    context.report_node(
                        argument,
                        RuleMessage {
                            id: "unsafeComputedMemberAccess".into(),
                            description: format!(
                                "Computed name [{property_name}] resolves to an {} value.",
                                data_type(ty)
                            ),
                            ..Default::default()
                        },
                    );
                }
            }
            _ => unreachable!("no-unsafe-member-access listener"),
        }
    }
}
