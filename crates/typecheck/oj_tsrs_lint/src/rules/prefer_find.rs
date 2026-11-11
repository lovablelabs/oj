use rustc_hash::FxHashSet;
use tsrs_ast::{self as ast, Kind, Node, NodeFlags, Symbol};
use tsrs_checker::{Checker, Type, TypeFlags, is_tuple_type_exported};
use tsrs_core::{P, jsnum};

use crate::protocol::{Fix, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, replace_node, trim_node_range};
use crate::utils;

pub struct PreferFind;

enum StaticValue {
    String(&'static str),
    Number(f64),
    Boolean(bool),
    Null,
    BigInt(jsnum::PseudoBigInt),
    Undefined,
    Symbol,
}

struct FilterExpressionData {
    filter_node: P<Node>,
    is_bracket_syntax_for_filter: bool,
}

fn parse_float(text: &str) -> Option<f64> {
    let bytes = text.as_bytes();
    if bytes.iter().enumerate().any(|(index, &byte)| {
        byte == b'_'
            && (index == 0
                || index + 1 == bytes.len()
                || !bytes[index - 1].is_ascii_digit()
                || !bytes[index + 1].is_ascii_digit())
    }) {
        return None;
    }
    let value = text.replace('_', "").parse::<f64>().ok()?;
    if value.is_infinite()
        && !matches!(
            text.to_ascii_lowercase().as_str(),
            "inf" | "+inf" | "-inf" | "infinity" | "+infinity" | "-infinity"
        )
    {
        return None;
    }
    Some(value)
}

fn to_number_from_string(text: &str) -> f64 {
    let text = text.trim();
    match text {
        "" => return 0.0,
        "Infinity" | "+Infinity" => return f64::INFINITY,
        "-Infinity" => return f64::NEG_INFINITY,
        _ => {}
    }
    let (sign, trimmed) =
        if text.starts_with(['-', '+']) { (&text[..1], &text[1..]) } else { ("", text) };
    let radix = if trimmed.starts_with("0x") || trimmed.starts_with("0X") {
        16
    } else if trimmed.starts_with("0b") || trimmed.starts_with("0B") {
        2
    } else if trimmed.starts_with("0o") || trimmed.starts_with("0O") {
        8
    } else {
        return parse_float(text).unwrap_or(f64::NAN);
    };
    let signed_digits = format!("{sign}{}", &trimmed[2..]);
    let (negative, digits) = if let Some(rest) = signed_digits.strip_prefix('-') {
        (true, rest)
    } else {
        (false, signed_digits.strip_prefix('+').unwrap_or(&signed_digits))
    };
    if digits.is_empty() {
        return f64::NAN;
    }
    if !digits.chars().all(|character| character.is_digit(radix)) {
        return f64::NAN;
    }
    let value = jsnum::from_string(&format!("{}{digits}", &trimmed[..2])).0;
    if negative { -value } else { value }
}

fn to_number(value: &StaticValue) -> Option<f64> {
    match value {
        StaticValue::Number(value) => Some(*value),
        StaticValue::String(value) => Some(to_number_from_string(value)),
        StaticValue::Boolean(value) => Some(f64::from(u8::from(*value))),
        StaticValue::Null => Some(0.0),
        StaticValue::Undefined => Some(f64::NAN),
        StaticValue::BigInt(_) | StaticValue::Symbol => None,
    }
}

fn is_treated_as_zero_by_array_at(value: &StaticValue) -> bool {
    to_number(value).is_some_and(|number| number.is_nan() || number.trunc() == 0.0)
}

fn is_treated_as_zero_by_member_access(value: &StaticValue) -> bool {
    match value {
        StaticValue::String(value) => *value == "0",
        StaticValue::Number(value) => *value == 0.0,
        StaticValue::BigInt(value) => value.sign() == jsnum::BigIntSign::Zero,
        _ => false,
    }
}

fn parse_big_int_literal(text: &str) -> Option<StaticValue> {
    let trimmed = text.trim();
    let trimmed = trimmed.strip_suffix('n').unwrap_or(trimmed).replace('_', "");
    let (negative, body) = if let Some(rest) = trimmed.strip_prefix('-') {
        (true, rest)
    } else {
        (false, trimmed.strip_prefix('+').unwrap_or(&trimmed))
    };
    let (radix, digits) = if body.starts_with("0x") || body.starts_with("0X") {
        (16, &body[2..])
    } else if body.starts_with("0b") || body.starts_with("0B") {
        (2, &body[2..])
    } else if body.starts_with("0o") || body.starts_with("0O") {
        (8, &body[2..])
    } else if body.len() > 1 && body.starts_with('0') {
        (8, &body[1..])
    } else {
        (10, body)
    };
    if digits.is_empty() || !digits.chars().all(|character| character.is_digit(radix)) {
        return None;
    }
    let canonical = if radix == 8 {
        jsnum::parse_pseudo_big_int(&format!("0o{digits}"))
    } else {
        jsnum::parse_pseudo_big_int(body)
    };
    Some(StaticValue::BigInt(jsnum::new_pseudo_big_int(&canonical, negative)))
}

fn get_const_initializer(symbol: P<Symbol>) -> Option<P<Node>> {
    let declaration =
        symbol.value_declaration().filter(|node| node.kind() == Kind::VariableDeclaration)?;
    let list = declaration.parent()?;
    if list.kind() != Kind::VariableDeclarationList || !list.flags().intersects(NodeFlags::Const) {
        return None;
    }
    declaration.initializer()
}

fn get_static_value(
    checker: &mut Checker,
    node: P<Node>,
    visited: &mut FxHashSet<P<Symbol>>,
) -> Option<StaticValue> {
    let node = ast::skip_parentheses(node);
    match node.kind() {
        Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral => {
            return Some(StaticValue::String(node.text()));
        }
        Kind::NumericLiteral => {
            return parse_float(&node.text().replace('_', "")).map(StaticValue::Number);
        }
        Kind::TrueKeyword => return Some(StaticValue::Boolean(true)),
        Kind::FalseKeyword => return Some(StaticValue::Boolean(false)),
        Kind::NullKeyword => return Some(StaticValue::Null),
        Kind::BigIntLiteral => return parse_big_int_literal(node.text()),
        Kind::Identifier => {
            match node.text() {
                "undefined" => return Some(StaticValue::Undefined),
                "NaN" => return Some(StaticValue::Number(f64::NAN)),
                "Infinity" => return Some(StaticValue::Number(f64::INFINITY)),
                _ => {}
            }
            let symbol = checker.get_symbol_at_location_exported(node)?;
            if visited.contains(&symbol) {
                return None;
            }
            let initializer = get_const_initializer(symbol)?;
            visited.insert(symbol);
            let result = get_static_value(checker, initializer, visited);
            visited.remove(&symbol);
            return result;
        }
        Kind::PrefixUnaryExpression => {
            let prefix = node.as_prefix_unary_expression();
            let operand = get_static_value(checker, prefix.operand, visited)?;
            match prefix.operator {
                Kind::MinusToken => {
                    if let StaticValue::BigInt(mut value) = operand {
                        value.negative = !value.negative && value.sign() != jsnum::BigIntSign::Zero;
                        return Some(StaticValue::BigInt(value));
                    }
                    return to_number(&operand).map(|number| StaticValue::Number(-number));
                }
                Kind::PlusToken => return to_number(&operand).map(StaticValue::Number),
                _ => {}
            }
        }
        Kind::AsExpression | Kind::TypeAssertionExpression | Kind::NonNullExpression => {
            return get_static_value(checker, node.expression()?, visited);
        }
        _ => {}
    }
    if node.kind() == Kind::CallExpression {
        let call = node.as_call_expression();
        if call.question_dot_token().is_some() {
            return None;
        }
        let callee = ast::skip_parentheses(call.expression);
        if callee.kind() == Kind::Identifier && callee.text() == "Symbol" {
            return Some(StaticValue::Symbol);
        }
        if callee.kind() == Kind::PropertyAccessExpression {
            let object = ast::skip_parentheses(callee.expression()?);
            if object.kind() == Kind::Identifier
                && object.text() == "Symbol"
                && callee.name()?.text() == "for"
            {
                return Some(StaticValue::Symbol);
            }
        }
    }
    None
}

fn is_static_member_access_of_value(checker: &mut Checker, node: P<Node>, value: &str) -> bool {
    let node = ast::skip_parentheses(node);
    match node.kind() {
        Kind::PropertyAccessExpression => node.name().is_some_and(|name| name.text() == value),
        Kind::ElementAccessExpression => {
            let argument = node.as_element_access_expression().argument_expression();
            matches!(get_static_value(checker, argument, &mut FxHashSet::default()), Some(StaticValue::String(name)) if name == value)
        }
        _ => false,
    }
}

fn is_arrayish(checker: &Checker, t: P<Type>) -> bool {
    let mut is_at_least_one_arrayish_component = false;
    for &union_part in utils::union_type_parts(&t) {
        if union_part.flags().intersects(TypeFlags::Null | TypeFlags::Undefined) {
            continue;
        }
        let intersection_parts = if union_part.flags().intersects(TypeFlags::Intersection) {
            union_part.types()
        } else {
            std::slice::from_ref(&union_part)
        };
        if !intersection_parts
            .iter()
            .all(|&part| checker.is_array_type(part) || is_tuple_type_exported(part))
        {
            return false;
        }
        is_at_least_one_arrayish_component = true;
    }
    is_at_least_one_arrayish_component
}

fn parse_array_filter_expressions(
    checker: &mut Checker,
    expression: P<Node>,
) -> Vec<FilterExpressionData> {
    let node = ast::skip_parentheses(expression);
    if ast::is_comma_expression(node) {
        return parse_array_filter_expressions(checker, node.as_binary_expression().right());
    }
    if node.kind() == Kind::ConditionalExpression {
        let conditional = node.as_conditional_expression();
        let mut consequent = parse_array_filter_expressions(checker, conditional.when_true);
        if consequent.is_empty() {
            return Vec::new();
        }
        let alternate = parse_array_filter_expressions(checker, conditional.when_false);
        if alternate.is_empty() {
            return Vec::new();
        }
        consequent.extend(alternate);
        return consequent;
    }
    if node.kind() != Kind::CallExpression {
        return Vec::new();
    }
    let call = node.as_call_expression();
    if call.question_dot_token().is_some() {
        return Vec::new();
    }
    let callee = ast::skip_parentheses(call.expression);
    if !matches!(callee.kind(), Kind::PropertyAccessExpression | Kind::ElementAccessExpression)
        || !is_static_member_access_of_value(checker, callee, "filter")
    {
        return Vec::new();
    }
    let object_type =
        utils::get_constrained_type_at_location(checker, callee.expression().unwrap());
    if !is_arrayish(checker, object_type) {
        return Vec::new();
    }
    vec![FilterExpressionData {
        filter_node: if callee.kind() == Kind::PropertyAccessExpression {
            callee.name().unwrap()
        } else {
            callee.as_element_access_expression().argument_expression()
        },
        is_bracket_syntax_for_filter: callee.kind() == Kind::ElementAccessExpression,
    }]
}

fn get_object_if_array_at_zero_expression(checker: &mut Checker, node: P<Node>) -> Option<P<Node>> {
    let call = node.as_call_expression();
    if call.arguments().nodes().len() != 1 || call.question_dot_token().is_some() {
        return None;
    }
    let callee = ast::skip_parentheses(call.expression);
    if !matches!(callee.kind(), Kind::PropertyAccessExpression | Kind::ElementAccessExpression)
        || !is_static_member_access_of_value(checker, callee, "at")
    {
        return None;
    }
    let value = get_static_value(checker, call.arguments().nodes()[0], &mut FxHashSet::default())?;
    if is_treated_as_zero_by_array_at(&value) { callee.expression() } else { None }
}

fn is_member_access_of_zero(checker: &mut Checker, node: P<Node>) -> bool {
    let access = node.as_element_access_expression();
    if access.question_dot_token().is_some() {
        return false;
    }
    get_static_value(checker, access.argument_expression(), &mut FxHashSet::default())
        .is_some_and(|value| is_treated_as_zero_by_member_access(&value))
}

fn find_array_element_access_start(
    context: &RuleContext<'_>,
    array_node: P<Node>,
    whole_expression: P<Node>,
) -> Option<u32> {
    let mut index =
        trim_node_range(context.source_file, ast::skip_parentheses(array_node)).end as usize;
    let end = whole_expression.end().expect_source("parsed lint node") as usize;
    let text = context.source_file.text();
    let bytes = text.as_bytes();
    while index < end {
        let character = text[index..].chars().next().unwrap();
        if character.is_whitespace() {
            index += character.len_utf8();
            continue;
        }
        if bytes[index] == b'/' && index + 1 < end {
            if bytes[index + 1] == b'/' {
                index += 2;
                while index < end && !matches!(bytes[index], b'\n' | b'\r') {
                    index += 1;
                }
                continue;
            }
            if bytes[index + 1] == b'*' {
                index += 2;
                while index + 1 < end && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                    index += 1;
                }
                if index + 1 < end {
                    index += 2;
                } else {
                    return None;
                }
                continue;
            }
        }
        if matches!(bytes[index], b'.' | b'[') {
            return Some(index as u32);
        }
        index += character.len_utf8();
    }
    None
}

fn report_prefer_find(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    array_node: P<Node>,
    filter_expressions: &[FilterExpressionData],
) {
    let Some(start) = find_array_element_access_start(context, array_node, node) else {
        return;
    };
    let source_file = context.source_file;
    context.report_node_with_suggestions(
        node,
        RuleMessage {
            id: "preferFind".into(),
            description: "Prefer .find(...) instead of .filter(...)[0].".into(),
            ..Default::default()
        },
        || {
            let mut fixes: Vec<_> = filter_expressions
                .iter()
                .map(|filter| {
                    replace_node(
                        source_file,
                        filter.filter_node,
                        if filter.is_bracket_syntax_for_filter { "\"find\"" } else { "find" }
                            .into(),
                    )
                })
                .collect();
            fixes.push(Fix {
                text: String::new(),
                range: Range { pos: start, end: node.end().expect_source("parsed lint node") },
            });
            vec![Suggestion {
                message: RuleMessage {
                    id: "preferFindSuggestion".into(),
                    description: "Use .find(...) instead of .filter(...)[0].".into(),
                    ..Default::default()
                },
                fixes,
            }]
        },
    );
}

impl Rule for PreferFind {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::CallExpression | Kind::ElementAccessExpression))
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let object = if node.kind() == Kind::CallExpression {
            get_object_if_array_at_zero_expression(context.checker, node)
        } else if is_member_access_of_zero(context.checker, node) {
            node.expression()
        } else {
            None
        };
        let Some(object) = object else {
            return;
        };
        let filters = parse_array_filter_expressions(context.checker, object);
        if !filters.is_empty() {
            report_prefer_find(context, node, object, &filters);
        }
    }
}
