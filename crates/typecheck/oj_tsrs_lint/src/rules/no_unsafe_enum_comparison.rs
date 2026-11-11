//! Port of tsgolint's `internal/rules/no_unsafe_enum_comparison`.

use std::fmt::Write;

use rustc_hash::FxHashSet;
use tsrs_ast::{self as ast, Kind, Node, SourceFile, Symbol, SymbolFlags};
use tsrs_checker::{LiteralValue, Type, TypeFlags};
use tsrs_core::{LanguageVariant, P, jsnum};

use crate::protocol::{Diagnostic, LabeledRange, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, replace_node, trim_node_range};
use crate::utils;

pub struct NoUnsafeEnumComparison;

enum StaticValue {
    String(String),
    Number(jsnum::Number),
}

impl StaticValue {
    fn as_string(&self) -> String {
        match self {
            Self::String(value) => value.clone(),
            Self::Number(value) => value.to_string(),
        }
    }
    fn matches(&self, value: LiteralValue) -> bool {
        match (self, value) {
            (Self::String(left), LiteralValue::String(right)) => left == right,
            (Self::Number(left), LiteralValue::Number(right)) => *left == right,
            _ => false,
        }
    }
}

fn static_value(node: P<Node>) -> Option<StaticValue> {
    let node = ast::skip_parentheses(node);
    match node.kind() {
        Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral => {
            Some(StaticValue::String(node.text().into()))
        }
        Kind::NumericLiteral => Some(StaticValue::Number(jsnum::from_string(node.text()))),
        Kind::PrefixUnaryExpression => {
            let prefix = node.as_prefix_unary_expression();
            let StaticValue::Number(value) = static_value(prefix.operand)? else {
                return None;
            };
            match prefix.operator {
                Kind::MinusToken => Some(StaticValue::Number(-value)),
                Kind::PlusToken => Some(StaticValue::Number(value)),
                _ => None,
            }
        }
        Kind::AsExpression
        | Kind::TypeAssertionExpression
        | Kind::NonNullExpression
        | Kind::SatisfiesExpression => static_value(node.expression().unwrap()),
        Kind::BinaryExpression => {
            let binary = node.as_binary_expression();
            if binary.operator_token.kind() != Kind::PlusToken {
                return None;
            }
            let left = static_value(binary.left)?;
            let right = static_value(binary.right())?;
            match (left, right) {
                (StaticValue::Number(left), StaticValue::Number(right)) => {
                    Some(StaticValue::Number(left + right))
                }
                (left, right) => Some(StaticValue::String(left.as_string() + &right.as_string())),
            }
        }
        _ => None,
    }
}

fn quote_single(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('\'');
    for character in value.chars() {
        match character {
            '\t' => output.push_str("\\t"),
            '\u{000B}' => output.push_str("\\v"),
            '\u{000C}' => output.push_str("\\f"),
            '\u{0008}' => output.push_str("\\b"),
            '\r' => output.push_str("\\r"),
            '\n' => output.push_str("\\n"),
            '\\' => output.push_str("\\\\"),
            '\'' => output.push_str("\\'"),
            '\u{2028}' | '\u{2029}' | '\u{0085}' | '\u{0000}'..='\u{001F}' => {
                write!(output, "\\u{:04X}", u32::from(character)).unwrap();
            }
            _ => output.push(character),
        }
    }
    output.push('\'');
    output
}

fn member_suffix(file: P<SourceFile>, member: P<Node>) -> String {
    let Some(name) = member.name() else {
        return String::new();
    };
    match name.kind() {
        Kind::Identifier => format!(".{}", name.text()),
        Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral => {
            if tsrs_scanner::is_identifier_text(name.text(), LanguageVariant::Standard) {
                format!(".{}", name.text())
            } else {
                format!("[{}]", quote_single(name.text()))
            }
        }
        Kind::ComputedPropertyName => {
            let range = trim_node_range(file, name.expression().unwrap());
            format!("[{}]", &file.text()[range.pos as usize..range.end as usize])
        }
        _ => String::new(),
    }
}

fn symbol_matches_enum(
    context: &mut RuleContext<'_>,
    symbol: P<Symbol>,
    enum_symbol: P<Symbol>,
) -> bool {
    if symbol == enum_symbol || context.checker.get_export_symbol_of_symbol(symbol) == enum_symbol {
        return true;
    }
    if symbol.flags().intersects(SymbolFlags::Alias) {
        let alias = context.checker.get_aliased_symbol(symbol);
        return alias == enum_symbol
            || context.checker.get_export_symbol_of_symbol(alias) == enum_symbol;
    }
    false
}

fn qualifier_from_type(
    context: &mut RuleContext<'_>,
    at: P<Node>,
    enum_type: P<Type>,
    declaration: P<Node>,
) -> String {
    let qualifier = context.checker.type_to_string_exported(enum_type);
    if !qualifier
        .split('.')
        .all(|part| tsrs_scanner::is_identifier_text(part, LanguageVariant::Standard))
    {
        return String::new();
    }
    if qualifier.contains('.') {
        return qualifier;
    }
    let Some(symbol) =
        context.checker.get_symbol_at_location_exported(declaration.as_enum_declaration().name())
    else {
        return String::new();
    };
    for scope_symbol in context.checker.get_symbols_in_scope_exported(at, SymbolFlags::Value) {
        if scope_symbol.name() == qualifier && symbol_matches_enum(context, scope_symbol, symbol) {
            return qualifier;
        }
    }
    String::new()
}

fn symbol_matches_declaration(
    context: &mut RuleContext<'_>,
    symbol: Option<P<Symbol>>,
    declaration: P<Node>,
) -> bool {
    let Some(mut symbol) = symbol else {
        return false;
    };
    if symbol.flags().intersects(SymbolFlags::Alias) {
        symbol = context.checker.get_aliased_symbol(symbol);
    }
    let symbol = context.checker.get_export_symbol_of_symbol(symbol);
    symbol.value_declaration().is_some_and(|member| {
        member.kind() == Kind::EnumMember && member.parent() == Some(declaration)
    })
}

fn qualifier_from_access(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    declaration: P<Node>,
) -> String {
    let node = ast::skip_parentheses(node);
    let expression = match node.kind() {
        Kind::PropertyAccessExpression => {
            let access = node.as_property_access_expression();
            let symbol = context.checker.get_symbol_at_location_exported(access.name);
            if !symbol_matches_declaration(context, symbol, declaration) {
                return String::new();
            }
            access.expression
        }
        Kind::ElementAccessExpression => {
            let access = node.as_element_access_expression();
            let symbol = context.checker.get_symbol_at_location_exported(node);
            if !symbol_matches_declaration(context, symbol, declaration) {
                let symbol =
                    context.checker.get_symbol_at_location_exported(access.argument_expression);
                if !symbol_matches_declaration(context, symbol, declaration) {
                    return String::new();
                }
            }
            access.expression
        }
        _ => return String::new(),
    };
    let range = trim_node_range(context.source_file, expression);
    context.source_file.text()[range.pos as usize..range.end as usize].into()
}

fn enum_key(
    context: &mut RuleContext<'_>,
    at: P<Node>,
    enum_side: P<Node>,
    literals: &[P<Type>],
    value: &StaticValue,
) -> String {
    for &literal in literals {
        let Some(member) = literal
            .symbol()
            .and_then(|symbol| symbol.value_declaration())
            .filter(|member| member.kind() == Kind::EnumMember)
        else {
            continue;
        };
        if !context
            .checker
            .get_constant_value(member)
            .is_some_and(|constant| value.matches(constant))
        {
            continue;
        }
        let suffix = member_suffix(context.source_file, member);
        if suffix.is_empty() {
            continue;
        }
        let declaration = member.parent().expect("enum declaration");
        let mut qualifier = qualifier_from_access(context, enum_side, declaration);
        if qualifier.is_empty() {
            let enum_type = context.checker.get_type_at_location(declaration);
            qualifier = qualifier_from_type(context, at, enum_type, declaration);
        }
        if qualifier.is_empty() {
            continue;
        }
        return qualifier + &suffix;
    }
    String::new()
}

fn suggestions(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    left: P<Type>,
    right: P<Type>,
) -> Vec<Suggestion> {
    let binary = node.as_binary_expression();
    for (value_node, enum_node, enum_type) in
        [(binary.right(), binary.left, left), (binary.left, binary.right(), right)]
    {
        if let Some(value) = static_value(value_node) {
            let key =
                enum_key(context, node, enum_node, &utils::get_enum_literals(enum_type), &value);
            if !key.is_empty() {
                return vec![Suggestion {
                    message: RuleMessage {
                        id: "replaceValueWithEnum".into(),
                        description: "Replace with an enum value comparison.".into(),
                        ..Default::default()
                    },
                    fixes: vec![replace_node(context.source_file, value_node, key)],
                }];
            }
        }
    }
    Vec::new()
}

fn primitive_like(ty: P<Type>, flags: TypeFlags) -> bool {
    utils::union_type_parts(&ty).iter().all(|part| {
        if part.flags().intersects(TypeFlags::Intersection) {
            part.types().iter().any(|part| part.flags().intersects(flags))
        } else {
            part.flags().intersects(flags)
        }
    })
}

fn violates(left: &[P<Type>], right: P<Type>) -> bool {
    let number = primitive_like(right, TypeFlags::Number | TypeFlags::NumberLike);
    let string = primitive_like(right, TypeFlags::String | TypeFlags::StringLike);
    if !number && !string {
        return false;
    }
    left.iter().any(|part| {
        part.flags().intersects(TypeFlags::EnumLike)
            && if part.flags().intersects(TypeFlags::NumberLiteral) { number } else { string }
    })
}

fn mismatched(context: &mut RuleContext<'_>, left: P<Type>, right: P<Type>) -> bool {
    let left_enums = utils::get_enum_types(context.checker, left);
    let right_enums: FxHashSet<_> =
        utils::get_enum_types(context.checker, right).into_iter().collect();
    if (left_enums.is_empty() && right_enums.is_empty())
        || left_enums.iter().any(|ty| right_enums.contains(ty))
    {
        return false;
    }
    let left_parts = utils::union_type_parts(&left);
    let right_parts = utils::union_type_parts(&right);
    if left_parts.iter().any(|part| right_parts.contains(part)) {
        return false;
    }
    violates(left_parts, right) || violates(right_parts, left)
}

fn report_comparison(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    left: P<Node>,
    right: P<Node>,
    left_type: P<Type>,
    right_type: P<Type>,
    case: bool,
) {
    let (id, description, help, left_label, right_label) = if case {
        (
            "mismatchedCase",
            "The case statement does not have a shared enum type with the switch predicate.",
            "Compare against a member of the same enum as the switch value, or normalize both sides to the same primitive representation first.",
            "Switch value",
            "Case value",
        )
    } else {
        (
            "mismatchedCondition",
            "The two values in this comparison do not have a shared enum type.",
            "Compare enum values to members of the same enum, or convert both sides to the same primitive type before comparing them.",
            "Left operand",
            "Right operand",
        )
    };
    let mut diagnostic = Diagnostic {
        range: Some(trim_node_range(context.source_file, node)),
        message: RuleMessage { id: id.into(), description: description.into(), help: help.into() },
        labeled_ranges: vec![
            LabeledRange {
                label: format!(
                    "{left_label}: {}",
                    context.checker.type_to_string_exported(left_type)
                ),
                range: trim_node_range(context.source_file, left),
            },
            LabeledRange {
                label: format!(
                    "{right_label}: {}",
                    context.checker.type_to_string_exported(right_type)
                ),
                range: trim_node_range(context.source_file, right),
            },
        ],
        ..Default::default()
    };
    if !case && context.options.fix_suggestions {
        diagnostic.suggestions = suggestions(context, node, left_type, right_type);
    }
    context.report_diagnostic(diagnostic);
}

impl Rule for NoUnsafeEnumComparison {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::BinaryExpression | Kind::CaseClause))
    }
    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let (left, right, case) = match listener {
            Listener::Enter(Kind::BinaryExpression) => {
                let binary = node.as_binary_expression();
                if !matches!(
                    binary.operator_token.kind(),
                    Kind::LessThanToken
                        | Kind::LessThanEqualsToken
                        | Kind::GreaterThanToken
                        | Kind::GreaterThanEqualsToken
                        | Kind::EqualsEqualsToken
                        | Kind::EqualsEqualsEqualsToken
                        | Kind::ExclamationEqualsToken
                        | Kind::ExclamationEqualsEqualsToken
                ) {
                    return;
                }
                (binary.left, binary.right(), false)
            }
            Listener::Enter(Kind::CaseClause) => (
                node.parent()
                    .and_then(|parent| parent.parent())
                    .and_then(|parent| parent.expression())
                    .expect("switch predicate"),
                node.expression().expect("case expression"),
                true,
            ),
            _ => unreachable!("no-unsafe-enum-comparison listener"),
        };
        let left_type = context.checker.get_type_at_location(left);
        let right_type = context.checker.get_type_at_location(right);
        if mismatched(context, left_type, right_type) {
            report_comparison(context, node, left, right, left_type, right_type, case);
        }
    }
}
