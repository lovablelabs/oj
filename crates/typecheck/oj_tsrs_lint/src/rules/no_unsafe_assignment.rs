//! Port of tsgolint's `internal/rules/no_unsafe_assignment/no_unsafe_assignment.go`.

use rustc_hash::FxHashMap;
use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{ContextFlags, Type, TypeFlags};
use tsrs_core::P;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils;

pub struct NoUnsafeAssignment;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Comparison {
    None,
    Basic,
    Contextual,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MessageId {
    AnyAssignment,
    AnyAssignmentThis,
    UnsafeArrayPattern,
    UnsafeArrayPatternFromTuple,
    UnsafeArraySpread,
}

impl MessageId {
    fn message(self, sender: P<Type>) -> RuleMessage {
        let sender = if utils::is_intrinsic_error_type(sender) { "error typed" } else { "any" };
        let (id, description) = match self {
            Self::AnyAssignment => {
                ("anyAssignment", format!("Unsafe assignment of an {sender} value."))
            }
            Self::AnyAssignmentThis => (
                "anyAssignmentThis",
                format!("Unsafe assignment of an {sender} value. `this` is typed as `any`."),
            ),
            Self::UnsafeArrayPattern => (
                "unsafeArrayPattern",
                format!("Unsafe array destructuring of an {sender} array value."),
            ),
            Self::UnsafeArrayPatternFromTuple => (
                "unsafeArrayPatternFromTuple",
                format!("Unsafe array destructuring of a tuple element with an {sender} value."),
            ),
            Self::UnsafeArraySpread => {
                ("unsafeArraySpread", format!("Unsafe spread of an {sender} value in an array."))
            }
        };
        RuleMessage {
            id: id.into(),
            description,
            help: if self == Self::AnyAssignmentThis {
                "Enable `noImplicitThis` or add an explicit `this` parameter to the function."
                    .into()
            } else {
                String::new()
            },
        }
    }
}

fn type_text(context: &mut RuleContext<'_>, ty: P<Type>) -> Option<String> {
    if utils::is_intrinsic_error_type(ty) {
        Some("error".into())
    } else {
        String::from_utf8(context.checker.type_to_string_bytes_exported(ty)).ok()
    }
}

fn assignment_message(
    context: &mut RuleContext<'_>,
    sender: P<Type>,
    receiver: P<Type>,
    inferred: bool,
    mut message: RuleMessage,
) -> Option<RuleMessage> {
    let sender = type_text(context, sender)?;
    let receiver = type_text(context, receiver)?;
    let help = if inferred {
        format!(
            "Assigned value has type `{sender}`, so the target is also inferred as `{receiver}`."
        )
    } else {
        format!("Assigned value has type `{sender}`; the target has type `{receiver}`.")
    };
    message.help = format!(
        "{help} {}",
        if message.help.is_empty() {
            "Use a safely typed value or `unknown`, and narrow it before use."
        } else {
            &message.help
        }
    );
    Some(message)
}

fn report_destructure(
    context: &mut RuleContext<'_>,
    receiver: P<Node>,
    sender: P<Type>,
    array: bool,
) {
    let Some(source) = type_text(context, sender) else {
        return;
    };
    let unsafe_type = if array { "any" } else { &source };
    let mut message =
        if array { MessageId::UnsafeArrayPattern } else { MessageId::UnsafeArrayPatternFromTuple }
            .message(sender);
    message.help = format!(
        "The destructured value has type `{source}`, and this binding receives `{unsafe_type}`. Use a safely typed source or narrow the value before destructuring it."
    );
    context.report_node(receiver, message);
}

fn skip_sender_check(node: P<Node>, comparison: Comparison) -> bool {
    let node = ast::skip_parentheses(node);
    match node.kind() {
        Kind::StringLiteral
        | Kind::NumericLiteral
        | Kind::BigIntLiteral
        | Kind::RegularExpressionLiteral
        | Kind::NoSubstitutionTemplateLiteral
        | Kind::TrueKeyword
        | Kind::FalseKeyword
        | Kind::NullKeyword
        | Kind::ArrowFunction
        | Kind::FunctionExpression
        | Kind::ClassExpression => true,
        Kind::PrefixUnaryExpression => {
            let expression = node.as_prefix_unary_expression();
            matches!(expression.operator, Kind::PlusToken | Kind::MinusToken)
                && ast::skip_parentheses(expression.operand).kind() == Kind::NumericLiteral
        }
        Kind::ObjectLiteralExpression => {
            !node.properties().iter().any(|property| property.kind() == Kind::SpreadAssignment)
        }
        Kind::ArrayLiteralExpression => {
            comparison == Comparison::None
                && !node.elements().iter().any(|element| element.kind() == Kind::SpreadElement)
        }
        _ => false,
    }
}

fn any_array(context: &mut RuleContext<'_>, ty: P<Type>) -> bool {
    context.checker.is_array_type(ty)
        && context.checker.get_type_arguments(ty)[0].flags().intersects(TypeFlags::Any)
}

fn check_nested_pattern(
    context: &mut RuleContext<'_>,
    receiver: P<Node>,
    sender: P<Type>,
    sender_node: P<Node>,
) -> bool {
    if sender.flags().intersects(TypeFlags::Any) {
        report_destructure(context, receiver, sender, false);
        return true;
    }
    match receiver.kind() {
        Kind::ArrayBindingPattern | Kind::ArrayLiteralExpression => {
            check_array_destructure(context, receiver, sender, sender_node)
        }
        Kind::ObjectBindingPattern | Kind::ObjectLiteralExpression => {
            check_object_destructure(context, receiver, sender, sender_node)
        }
        _ => false,
    }
}

fn check_object_property(
    context: &mut RuleContext<'_>,
    key: P<Node>,
    value: P<Node>,
    properties: &FxHashMap<&'static str, P<Type>>,
    sender_node: P<Node>,
) -> bool {
    let name = if key.kind() == Kind::ComputedPropertyName {
        let expression = key.expression().unwrap();
        if !ast::is_literal_expression(expression) {
            return false;
        }
        expression.text()
    } else {
        key.text()
    };
    let Some(&sender) = properties.get(name) else {
        return false;
    };
    check_nested_pattern(context, value, sender, sender_node)
}

fn check_object_destructure(
    context: &mut RuleContext<'_>,
    receiver: P<Node>,
    sender: P<Type>,
    sender_node: P<Node>,
) -> bool {
    let properties: FxHashMap<_, _> = context
        .checker
        .get_properties_of_type(sender)
        .iter()
        .filter_map(|&symbol| {
            context
                .checker
                .get_type_of_symbol_at_location(symbol, Some(sender_node))
                .map(|ty| (symbol.name(), ty))
        })
        .collect();
    let mut reported = false;
    match receiver.kind() {
        Kind::ObjectLiteralExpression => {
            for &property in receiver.properties() {
                let value = match property.kind() {
                    Kind::PropertyAssignment => property.initializer(),
                    Kind::ShorthandPropertyAssignment => property.name(),
                    _ => None,
                };
                if let Some(value) = value
                    && check_object_property(
                        context,
                        property.name().unwrap(),
                        value,
                        &properties,
                        sender_node,
                    )
                {
                    reported = true;
                }
            }
        }
        Kind::ObjectBindingPattern => {
            for &property in receiver.elements() {
                let binding = property.as_binding_element();
                if binding.dot_dot_dot_token().is_some() {
                    continue;
                }
                let name = binding.name().expect("object binding name");
                if check_object_property(
                    context,
                    binding.property_name().unwrap_or(name),
                    name,
                    &properties,
                    sender_node,
                ) {
                    reported = true;
                }
            }
        }
        _ => {}
    }
    reported
}

fn check_array_destructure(
    context: &mut RuleContext<'_>,
    receiver: P<Node>,
    sender: P<Type>,
    sender_node: P<Node>,
) -> bool {
    if any_array(context, sender) {
        report_destructure(context, receiver, sender, true);
        return false;
    }
    if !sender.is_tuple_type() {
        return true;
    }
    let tuple_elements = context.checker.get_type_arguments(sender);
    let mut reported = false;
    for (index, &element) in receiver.elements().iter().enumerate() {
        let value = match receiver.kind() {
            Kind::ArrayLiteralExpression => {
                if element.kind() == Kind::SpreadElement {
                    None
                } else {
                    Some(element)
                }
            }
            Kind::ArrayBindingPattern => {
                let binding = element.as_binding_element();
                if binding.dot_dot_dot_token().is_some() { None } else { binding.name() }
            }
            _ => None,
        };
        if let Some(value) = value
            && let Some(&sender) = tuple_elements.get(index)
            && check_nested_pattern(context, value, sender, sender_node)
        {
            reported = true;
        }
    }
    reported
}

fn receiver_type(
    context: &mut RuleContext<'_>,
    receiver: P<Node>,
    sender_node: P<Node>,
    sender: P<Type>,
    comparison: Comparison,
) -> P<Type> {
    if comparison == Comparison::Contextual {
        if (receiver.parent().is_none_or(|parent| parent.kind() != Kind::JsxAttribute)
            || !sender.flags().intersects(TypeFlags::Any))
            && let Some(ty) = context.checker.get_contextual_type(sender_node, ContextFlags::None)
        {
            return ty;
        }
        if let Some(ty) = utils::get_contextual_type(context.checker, receiver) {
            return ty;
        }
    }
    context.checker.get_type_at_location(receiver)
}

fn check_assignment(
    context: &mut RuleContext<'_>,
    receiver: P<Node>,
    sender_node: P<Node>,
    reporting_node: P<Node>,
    comparison: Comparison,
) -> bool {
    if skip_sender_check(sender_node, comparison) {
        return false;
    }
    let sender = context.checker.get_type_at_location(sender_node);
    if sender.flags().intersects(TypeFlags::Any) {
        let receiver_ty = receiver_type(context, receiver, sender_node, sender, comparison);
        if receiver_ty.flags().intersects(TypeFlags::Unknown) {
            return false;
        }
        let mut diagnostic_receiver = receiver_ty;
        if let Some(attribute) =
            receiver.parent().filter(|parent| parent.kind() == Kind::JsxAttribute)
        {
            if let Some(ty) = utils::get_contextual_type(context.checker, sender_node) {
                diagnostic_receiver = ty;
            }
            if diagnostic_receiver.flags().intersects(TypeFlags::Unknown)
                && let Some(attributes_type) = context.checker.get_apparent_type_of_contextual_type(
                    attribute.parent().unwrap(),
                    ContextFlags::None,
                )
                && let Some(ty) = context.checker.get_type_of_property_or_index_signature_of_type(
                    attributes_type,
                    receiver.text(),
                )
            {
                diagnostic_receiver = ty;
            }
        }
        let options = context.program.options();
        if !options.get_strict_option_value(options.no_implicit_this)
            && let Some(this) = utils::get_this_expression(sender_node)
            && utils::get_constrained_type_at_location(context.checker, this)
                .flags()
                .intersects(TypeFlags::Any)
        {
            let message = assignment_message(
                context,
                sender,
                diagnostic_receiver,
                comparison == Comparison::None,
                MessageId::AnyAssignmentThis.message(sender),
            );
            if let Some(message) = message {
                context.report_node(reporting_node, message);
            }
            return true;
        }
        let message = assignment_message(
            context,
            sender,
            diagnostic_receiver,
            comparison == Comparison::None,
            MessageId::AnyAssignment.message(sender),
        );
        if let Some(message) = message {
            context.report_node(reporting_node, message);
        }
        return true;
    }
    if comparison == Comparison::None || !tsrs_checker::is_non_deferred_type_reference(sender) {
        return false;
    }
    let receiver = receiver_type(context, receiver, sender_node, sender, comparison);
    let Some((receiver, sender)) =
        utils::is_unsafe_assignment(sender, receiver, context.checker, Some(sender_node))
    else {
        return false;
    };
    let message = assignment_message(
        context,
        sender,
        receiver,
        false,
        RuleMessage {
            id: "unsafeAssignment".into(),
            description: "Unsafe assignment between incompatible types.".into(),
            ..Default::default()
        },
    );
    if let Some(message) = message {
        context.report_node(reporting_node, message);
    }
    true
}

fn check_destructure(
    context: &mut RuleContext<'_>,
    receiver: P<Node>,
    sender_node: P<Node>,
    array: bool,
) -> bool {
    if array {
        if !matches!(receiver.kind(), Kind::ArrayBindingPattern | Kind::ArrayLiteralExpression) {
            return false;
        }
        let sender = context.checker.get_type_at_location(sender_node);
        check_array_destructure(context, receiver, sender, sender_node)
    } else {
        if !matches!(receiver.kind(), Kind::ObjectBindingPattern | Kind::ObjectLiteralExpression) {
            return false;
        }
        let sender = context.checker.get_type_at_location(sender_node);
        check_object_destructure(context, receiver, sender, sender_node)
    }
}

fn check_full_assignment(
    context: &mut RuleContext<'_>,
    receiver: Option<P<Node>>,
    sender: Option<P<Node>>,
    reporting: P<Node>,
    comparison: Comparison,
) {
    let (Some(receiver), Some(sender)) = (receiver, sender) else {
        return;
    };
    if !check_assignment(context, receiver, sender, reporting, comparison)
        && !check_destructure(context, receiver, sender, true)
    {
        check_destructure(context, receiver, sender, false);
    }
}

impl Rule for NoUnsafeAssignment {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::PropertyDeclaration
                    | Kind::BinaryExpression
                    | Kind::BindingElement
                    | Kind::Parameter
                    | Kind::ShorthandPropertyAssignment
                    | Kind::VariableDeclaration
                    | Kind::JsxAttribute
            ) | Listener::ExpressionEnter(
                Kind::ObjectLiteralExpression | Kind::ArrayLiteralExpression
            )
        )
    }
    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match listener {
            Listener::Enter(Kind::PropertyDeclaration) => {
                if let Some(initializer) = node.initializer() {
                    let comparison = if node.type_node().is_some() {
                        Comparison::Basic
                    } else {
                        Comparison::None
                    };
                    check_assignment(context, node.name().unwrap(), initializer, node, comparison);
                }
            }
            Listener::Enter(Kind::BinaryExpression) => {
                if ast::is_assignment_expression(node, true) {
                    let binary = node.as_binary_expression();
                    check_full_assignment(
                        context,
                        Some(binary.left),
                        Some(binary.right()),
                        node,
                        Comparison::Basic,
                    );
                }
            }
            Listener::Enter(Kind::BindingElement | Kind::Parameter) => check_full_assignment(
                context,
                node.name(),
                node.initializer(),
                node,
                Comparison::Basic,
            ),
            Listener::Enter(Kind::ShorthandPropertyAssignment) => check_full_assignment(
                context,
                node.name(),
                node.as_shorthand_property_assignment().object_assignment_initializer(),
                node,
                Comparison::Basic,
            ),
            Listener::Enter(Kind::VariableDeclaration) => {
                let comparison =
                    if node.type_node().is_some() { Comparison::Basic } else { Comparison::None };
                check_full_assignment(context, node.name(), node.initializer(), node, comparison);
            }
            Listener::ExpressionEnter(Kind::ObjectLiteralExpression) => {
                for &property in node.properties() {
                    let initializer = match property.kind() {
                        Kind::PropertyAssignment => property.initializer(),
                        Kind::ShorthandPropertyAssignment => property.name(),
                        _ => continue,
                    };
                    let Some(initializer) = initializer else {
                        return;
                    };
                    let initializer = ast::skip_parentheses(initializer);
                    if ast::is_assignment_expression(initializer, false) {
                        return;
                    }
                    check_assignment(
                        context,
                        property.name().unwrap(),
                        initializer,
                        property,
                        Comparison::Contextual,
                    );
                }
            }
            Listener::ExpressionEnter(Kind::ArrayLiteralExpression) => {
                for &element in node.elements() {
                    if element.kind() != Kind::SpreadElement {
                        continue;
                    }
                    let rest_type =
                        context.checker.get_type_at_location(element.expression().unwrap());
                    if rest_type.flags().intersects(TypeFlags::Any) || any_array(context, rest_type)
                    {
                        let Some(text) = type_text(context, rest_type) else {
                            continue;
                        };
                        let mut message = MessageId::UnsafeArraySpread.message(rest_type);
                        message.help = format!(
                            "The spread value has type `{text}`. Use a safely typed array or narrow the value before spreading it."
                        );
                        context.report_node(element, message);
                    }
                }
            }
            Listener::Enter(Kind::JsxAttribute) => {
                if let Some(initializer) = node
                    .initializer()
                    .filter(|initializer| initializer.kind() == Kind::JsxExpression)
                    && let Some(expression) = initializer.as_jsx_expression().expression
                {
                    check_assignment(
                        context,
                        node.name().unwrap(),
                        expression,
                        expression,
                        Comparison::Contextual,
                    );
                }
            }
            _ => unreachable!("no-unsafe-assignment listener"),
        }
    }
}
