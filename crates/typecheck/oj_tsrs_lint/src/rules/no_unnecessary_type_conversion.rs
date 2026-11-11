//! Port of tsgolint's `internal/rules/no_unnecessary_type_conversion/no_unnecessary_type_conversion.go`.

use tsrs_ast::{Kind, Node, SymbolFlags};
use tsrs_checker::{LiteralValue, Type, TypeFlags};
use tsrs_core::P;

use crate::protocol::{Diagnostic, Fix, LabeledRange, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, replace_node, trim_node_range};
use crate::utils::{self, union_type_parts};

pub struct NoUnnecessaryTypeConversion;

#[derive(Clone, Copy)]
enum MessageId {
    SuggestRemove,
    SuggestSatisfies,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description) = match self {
            Self::SuggestRemove => ("suggestRemove", "Remove the type conversion."),
            Self::SuggestSatisfies => {
                ("suggestSatisfies", "Instead, assert that the value satisfies the primitive type.")
            }
        };
        RuleMessage { id: id.into(), description: description.into(), ..Default::default() }
    }
}

fn does_underlying_type_match_flag(t: P<Type>, flag: TypeFlags) -> bool {
    union_type_parts(&t).iter().all(|part| part.flags().intersects(flag))
}

fn is_empty_string_literal(node: P<Node>) -> bool {
    node.kind() == Kind::StringLiteral && node.text().is_empty()
}

#[expect(
    clippy::float_cmp,
    reason = "Go tests integer literals by comparing the parsed value with math.Trunc(value)."
)]
fn is_all_number_literal_integers(t: P<Type>) -> bool {
    let parts = union_type_parts(&t);
    !parts.is_empty()
        && parts.iter().all(|part| {
            if !part.flags().intersects(TypeFlags::NumberLiteral) {
                return false;
            }
            let Some(LiteralValue::Number(value)) = part.as_literal_type().value() else {
                return false;
            };
            value.0.trunc() == value.0
        })
}

fn is_node_parenthesized(node: P<Node>) -> bool {
    node.parent().is_some_and(|parent| {
        parent.kind() == Kind::ParenthesizedExpression
            && parent.as_parenthesized_expression().expression() == node
    })
}

fn is_object_expression_in_one_line_return(node: P<Node>, inner: P<Node>) -> bool {
    node.parent()
        .is_some_and(|parent| parent.kind() == Kind::ArrowFunction && parent.body() == Some(node))
        && inner.kind() == Kind::ObjectLiteralExpression
}

fn is_weak_precedence_parent(node: P<Node>) -> bool {
    let Some(parent) = node.parent() else {
        return false;
    };
    match parent.kind() {
        Kind::PostfixUnaryExpression
        | Kind::PrefixUnaryExpression
        | Kind::BinaryExpression
        | Kind::ConditionalExpression
        | Kind::AwaitExpression => true,
        Kind::PropertyAccessExpression => parent.as_property_access_expression().expression == node,
        Kind::ElementAccessExpression => parent.as_element_access_expression().expression == node,
        Kind::CallExpression | Kind::NewExpression => parent.expression() == Some(node),
        Kind::TaggedTemplateExpression => parent.as_tagged_template_expression().tag == node,
        _ => false,
    }
}

fn get_node_text(context: &RuleContext<'_>, node: P<Node>) -> String {
    let range = trim_node_range(context.source_file, node);
    context.source_file.text()[range.pos as usize..range.end as usize].into()
}

fn build_wrapping_fix(
    context: &RuleContext<'_>,
    node: P<Node>,
    inner: &[P<Node>],
    primitive: Option<&str>,
) -> Fix {
    let inner_codes: Vec<_> = inner
        .iter()
        .map(|&inner| {
            let mut code = get_node_text(context, inner);
            if !utils::is_strong_precedence_node(inner)
                || is_object_expression_in_one_line_return(node, inner)
            {
                code = format!("({code})");
            }
            code
        })
        .collect();
    let Some(primitive) = primitive else {
        return replace_node(context.source_file, node, inner_codes.join(""));
    };
    let mut code = format!("{} satisfies {primitive}", inner_codes[0]);
    if is_weak_precedence_parent(node) && !is_node_parenthesized(node) {
        code = format!("({code})");
    }
    replace_node(context.source_file, node, code)
}

fn build_suggestions(
    context: &RuleContext<'_>,
    node: P<Node>,
    primitive: &str,
    inner: &[P<Node>],
) -> Vec<Suggestion> {
    vec![
        Suggestion {
            message: MessageId::SuggestRemove.message(),
            fixes: vec![build_wrapping_fix(context, node, inner, None)],
        },
        Suggestion {
            message: MessageId::SuggestSatisfies.message(),
            fixes: vec![build_wrapping_fix(context, node, inner, Some(primitive))],
        },
    ]
}

fn report_conversion(
    context: &mut RuleContext<'_>,
    conversion: Range,
    expression: Range,
    t: P<Type>,
    suggestions: Vec<Suggestion>,
) {
    let text = context.checker.type_to_string_exported(t);
    context.report_diagnostic(Diagnostic {
        range: Some(conversion),
        message: RuleMessage {
            id: "unnecessaryTypeConversion".into(),
            description:
                "This type conversion does not change the type or value of the expression.".into(),
            ..Default::default()
        },
        labeled_ranges: vec![LabeledRange {
            range: expression,
            label: format!("This expression already has type '{text}'."),
        }],
        suggestions,
        ..Default::default()
    });
}

fn report_with_suggestions(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    inner: P<Node>,
    t: P<Type>,
    conversion: Range,
    primitive: &str,
) {
    let suggestions = if context.options.fix_suggestions {
        build_suggestions(context, node, primitive, &[inner])
    } else {
        Vec::new()
    };
    report_conversion(
        context,
        conversion,
        trim_node_range(context.source_file, inner),
        t,
        suggestions,
    );
}

fn report_binary_plus(context: &mut RuleContext<'_>, node: P<Node>) {
    let binary = node.as_binary_expression();
    if is_empty_string_literal(binary.right()) {
        let t = utils::get_constrained_type_at_location(context.checker, binary.left);
        if does_underlying_type_match_flag(t, TypeFlags::StringLike) {
            report_with_suggestions(
                context,
                node,
                binary.left,
                t,
                Range {
                    pos: binary.left.end().expect_source("parsed lint node"),
                    end: node.end().expect_source("parsed lint node"),
                },
                "string",
            );
        }
        return;
    }
    if is_empty_string_literal(binary.left) {
        let t = utils::get_constrained_type_at_location(context.checker, binary.right());
        if does_underlying_type_match_flag(t, TypeFlags::StringLike) {
            report_with_suggestions(
                context,
                node,
                binary.right(),
                t,
                Range {
                    pos: trim_node_range(context.source_file, node).pos,
                    end: trim_node_range(context.source_file, binary.right()).pos,
                },
                "string",
            );
        }
    }
}

fn report_plus_equals(context: &mut RuleContext<'_>, node: P<Node>) {
    let binary = node.as_binary_expression();
    if !is_empty_string_literal(binary.right()) || binary.left.kind() != Kind::Identifier {
        return;
    }
    let t = utils::get_constrained_type_at_location(context.checker, binary.left);
    if !does_underlying_type_match_flag(t, TypeFlags::StringLike) {
        return;
    }
    let suggestions = if context.options.fix_suggestions {
        let mut remove_fix = build_wrapping_fix(context, node, &[binary.left], None);
        if let Some(parent) =
            node.parent().filter(|parent| parent.kind() == Kind::ExpressionStatement)
        {
            remove_fix = replace_node(context.source_file, parent, String::new());
        }
        vec![
            Suggestion { message: MessageId::SuggestRemove.message(), fixes: vec![remove_fix] },
            Suggestion {
                message: MessageId::SuggestSatisfies.message(),
                fixes: vec![build_wrapping_fix(context, node, &[binary.left], Some("string"))],
            },
        ]
    } else {
        Vec::new()
    };
    report_conversion(
        context,
        trim_node_range(context.source_file, node),
        trim_node_range(context.source_file, binary.left),
        t,
        suggestions,
    );
}

fn report_builtin_conversion_call(context: &mut RuleContext<'_>, node: P<Node>) {
    let call = node.as_call_expression();
    let arguments = call.arguments().nodes();
    if call.expression.kind() != Kind::Identifier || arguments.len() != 1 {
        return;
    }
    let argument = arguments[0];
    if argument.kind() == Kind::SpreadElement {
        return;
    }
    let (flag, builtins, primitive): (_, &[&str], _) = match call.expression.text() {
        "BigInt" => (TypeFlags::BigIntLike, &["BigInt", "BigIntConstructor"], "bigint"),
        "Boolean" => (TypeFlags::BooleanLike, &["Boolean", "BooleanConstructor"], "boolean"),
        "Number" => (TypeFlags::NumberLike, &["Number", "NumberConstructor"], "number"),
        "String" => (TypeFlags::StringLike, &["String", "StringConstructor"], "string"),
        _ => return,
    };
    let callee_type = context.checker.get_type_at_location(call.expression);
    if !utils::is_builtin_symbol_like(context.program, context.checker, callee_type, builtins) {
        return;
    }
    let t = utils::get_constrained_type_at_location(context.checker, argument);
    if !does_underlying_type_match_flag(t, flag) {
        return;
    }
    report_with_suggestions(
        context,
        node,
        argument,
        t,
        trim_node_range(context.source_file, call.expression),
        primitive,
    );
}

fn report_string_to_string_call(context: &mut RuleContext<'_>, node: P<Node>) {
    let call = node.as_call_expression();
    if call.expression.kind() != Kind::PropertyAccessExpression
        || !call.arguments().nodes().is_empty()
    {
        return;
    }
    let member = call.expression.as_property_access_expression();
    let name = call.expression.name().expect("property access name");
    if name.text() != "toString" {
        return;
    }
    let t = utils::get_constrained_type_at_location(context.checker, member.expression);
    if t.flags().intersects(TypeFlags::EnumLike)
        || t.symbol().is_some_and(|symbol| symbol.flags().intersects(SymbolFlags::EnumMember))
    {
        return;
    }
    if !does_underlying_type_match_flag(t, TypeFlags::StringLike) {
        return;
    }
    report_with_suggestions(
        context,
        node,
        member.expression,
        t,
        Range {
            pos: name.pos().expect_source("parsed lint node"),
            end: node.end().expect_source("parsed lint node"),
        },
        "string",
    );
}

fn report_unary_plus(context: &mut RuleContext<'_>, node: P<Node>) {
    let unary = node.as_prefix_unary_expression();
    if unary.operator != Kind::PlusToken {
        return;
    }
    let t = utils::get_constrained_type_at_location(context.checker, unary.operand);
    if !does_underlying_type_match_flag(t, TypeFlags::NumberLike) {
        return;
    }
    report_with_suggestions(
        context,
        node,
        unary.operand,
        t,
        Range {
            pos: trim_node_range(context.source_file, node).pos,
            end: trim_node_range(context.source_file, unary.operand).pos,
        },
        "number",
    );
}

fn report_double_unary(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    operator: Kind,
    primitive: &str,
) {
    let unary = node.as_prefix_unary_expression();
    if unary.operator != operator {
        return;
    }
    let Some(outer) = node.parent().filter(|parent| parent.kind() == Kind::PrefixUnaryExpression)
    else {
        return;
    };
    let parent = outer.as_prefix_unary_expression();
    if parent.operator != operator || parent.operand != node {
        return;
    }
    let t = utils::get_constrained_type_at_location(context.checker, unary.operand);
    if if operator == Kind::ExclamationToken {
        !does_underlying_type_match_flag(t, TypeFlags::BooleanLike)
    } else {
        !is_all_number_literal_integers(t)
    } {
        return;
    }
    report_with_suggestions(
        context,
        outer,
        unary.operand,
        t,
        Range {
            pos: trim_node_range(context.source_file, outer).pos,
            end: trim_node_range(context.source_file, node).pos + 1,
        },
        primitive,
    );
}

impl Rule for NoUnnecessaryTypeConversion {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::BinaryExpression | Kind::CallExpression | Kind::PrefixUnaryExpression
            )
        )
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match node.kind() {
            Kind::BinaryExpression => match node.as_binary_expression().operator_token.kind() {
                Kind::PlusToken => report_binary_plus(context, node),
                Kind::PlusEqualsToken => report_plus_equals(context, node),
                _ => {}
            },
            Kind::CallExpression => {
                report_builtin_conversion_call(context, node);
                report_string_to_string_call(context, node);
            }
            Kind::PrefixUnaryExpression => {
                report_unary_plus(context, node);
                report_double_unary(context, node, Kind::ExclamationToken, "boolean");
                report_double_unary(context, node, Kind::TildeToken, "number");
            }
            _ => unreachable!("no-unnecessary-type-conversion listener"),
        }
    }
}
