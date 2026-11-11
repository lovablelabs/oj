//! Port of tsgolint's `internal/rules/no_unsafe_argument/no_unsafe_argument.go`.

use tsrs_ast::{Kind, Node};
use tsrs_checker::{CheckMode, ElementFlags, Type, TypeFlags};
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils;

pub struct NoUnsafeArgument;

enum RestType {
    Single(Option<P<Type>>),
    Tuple { index: usize, arguments: &'static [P<Type>] },
}

struct FunctionSignature {
    consumed_arguments: bool,
    parameter_index: usize,
    parameter_types: Vec<Option<P<Type>>>,
    rest_type: Option<RestType>,
}

impl FunctionSignature {
    fn new(context: &mut RuleContext<'_>, node: P<Node>) -> Self {
        let signature = context.checker.get_resolved_signature(node, None, CheckMode::Normal);
        let mut parameter_types = Vec::new();
        let mut rest_type = None;
        for (index, &parameter) in signature.parameters().iter().enumerate() {
            let ty = context.checker.get_type_of_symbol_at_location(parameter, Some(node));
            if parameter.declarations().first().is_some_and(|declaration| {
                declaration.kind() == Kind::Parameter
                    && declaration.as_parameter_declaration().dot_dot_dot_token().is_some()
            }) {
                rest_type = Some(match ty {
                    Some(ty) if context.checker.is_array_type(ty) => {
                        RestType::Single(Some(context.checker.get_type_arguments(ty)[0]))
                    }
                    Some(ty) if ty.is_tuple_type() => {
                        RestType::Tuple { index, arguments: context.checker.get_type_arguments(ty) }
                    }
                    _ => RestType::Single(ty),
                });
                break;
            }
            parameter_types.push(ty);
        }
        Self { consumed_arguments: false, parameter_index: 0, parameter_types, rest_type }
    }

    fn next_parameter_type(&mut self) -> Option<P<Type>> {
        let index = self.parameter_index;
        self.parameter_index += 1;
        if index >= self.parameter_types.len() || self.consumed_arguments {
            return match self.rest_type.as_ref()? {
                RestType::Single(ty) => *ty,
                RestType::Tuple { index: rest_index, arguments } => {
                    if self.consumed_arguments {
                        return arguments.last().copied();
                    }
                    arguments.get(index - rest_index).or_else(|| arguments.last()).copied()
                }
            };
        }
        self.parameter_types[index]
    }
}

fn describe_type(context: &mut RuleContext<'_>, ty: P<Type>) -> String {
    if utils::is_intrinsic_error_type(ty) {
        "error typed".into()
    } else {
        context.checker.type_to_string_exported(ty)
    }
}

fn describe_spread_type(context: &mut RuleContext<'_>, ty: P<Type>) -> String {
    if context.checker.is_array_type(ty)
        && utils::is_intrinsic_error_type(context.checker.get_type_arguments(ty)[0])
    {
        return "error".into();
    }
    describe_type(context, ty)
}

#[derive(Clone, Copy)]
#[expect(
    clippy::enum_variant_names,
    reason = "The variant names preserve the upstream tsgolint message ids."
)]
enum MessageId {
    UnsafeArgument,
    UnsafeArraySpread,
    UnsafeSpread,
    UnsafeTupleSpread,
}

impl MessageId {
    fn message(self, sender: &str, receiver: &str) -> RuleMessage {
        let (id, description) = match self {
            Self::UnsafeArgument => (
                "unsafeArgument",
                format!(
                    "Unsafe argument of type {sender} assigned to a parameter of type {receiver}."
                ),
            ),
            Self::UnsafeArraySpread => {
                ("unsafeArraySpread", format!("Unsafe spread of an {sender} array type."))
            }
            Self::UnsafeSpread => ("unsafeSpread", format!("Unsafe spread of an {sender} type.")),
            Self::UnsafeTupleSpread => (
                "unsafeTupleSpread",
                format!(
                    "Unsafe spread of a tuple type. The argument is {sender} and is assigned to a parameter of type {receiver}."
                ),
            ),
        };
        RuleMessage { id: id.into(), description, ..Default::default() }
    }
}

fn check_arguments(
    context: &mut RuleContext<'_>,
    arguments: &[P<Node>],
    callee: P<Node>,
    node: P<Node>,
) {
    if arguments.is_empty()
        || context.checker.get_type_at_location(callee).flags().intersects(TypeFlags::Any)
    {
        return;
    }
    let mut signature = FunctionSignature::new(context, node);
    if node.kind() == Kind::TaggedTemplateExpression {
        signature.next_parameter_type();
    }
    for &argument in arguments {
        if argument.kind() == Kind::SpreadElement {
            let spread_type = context.checker.get_type_at_location(argument.expression().unwrap());
            if spread_type.flags().intersects(TypeFlags::Any) {
                let sender = describe_type(context, spread_type);
                context.report_node(argument, MessageId::UnsafeSpread.message(&sender, ""));
            } else if context.checker.is_array_type(spread_type)
                && context.checker.get_type_arguments(spread_type)[0]
                    .flags()
                    .intersects(TypeFlags::Any)
            {
                let sender = describe_spread_type(context, spread_type);
                context.report_node(argument, MessageId::UnsafeArraySpread.message(&sender, ""));
            } else if spread_type.is_tuple_type() {
                for &tuple_type in context.checker.get_type_arguments(spread_type) {
                    let Some(parameter_type) = signature.next_parameter_type() else {
                        continue;
                    };
                    if utils::is_unsafe_assignment(
                        tuple_type,
                        parameter_type,
                        context.checker,
                        None,
                    )
                    .is_some()
                    {
                        let sender = if utils::is_intrinsic_error_type(tuple_type) {
                            "error typed".into()
                        } else {
                            format!(
                                "of type {}",
                                context.checker.type_to_string_exported(tuple_type)
                            )
                        };
                        let receiver = describe_type(context, parameter_type);
                        context.report_node(
                            argument,
                            MessageId::UnsafeTupleSpread.message(&sender, &receiver),
                        );
                    }
                }
                if spread_type
                    .target()
                    .expect("tuple reference target")
                    .as_tuple_type()
                    .combined_flags()
                    .intersects(ElementFlags::Variable)
                {
                    signature.consumed_arguments = true;
                }
            }
        } else {
            let Some(parameter_type) = signature.next_parameter_type() else {
                continue;
            };
            let argument_type = context.checker.get_type_at_location(argument);
            if utils::is_unsafe_assignment(
                argument_type,
                parameter_type,
                context.checker,
                Some(argument),
            )
            .is_some()
            {
                let sender = describe_type(context, argument_type);
                let receiver = describe_type(context, parameter_type);
                context
                    .report_node(argument, MessageId::UnsafeArgument.message(&sender, &receiver));
            }
        }
    }
}

impl Rule for NoUnsafeArgument {
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
            Listener::Enter(Kind::CallExpression | Kind::NewExpression) => {
                check_arguments(context, node.arguments(), node.expression().unwrap(), node);
            }
            Listener::Enter(Kind::TaggedTemplateExpression) => {
                let tagged = node.as_tagged_template_expression();
                if tagged.template.kind() == Kind::TemplateExpression {
                    let arguments = tagged
                        .template
                        .as_template_expression()
                        .template_spans
                        .nodes()
                        .iter()
                        .map(|span| span.expression().unwrap())
                        .collect::<Vec<_>>();
                    check_arguments(context, &arguments, tagged.tag, node);
                }
            }
            _ => unreachable!("no-unsafe-argument listener"),
        }
    }
}
