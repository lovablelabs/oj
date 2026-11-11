//! Port of tsgolint's `internal/rules/no_unnecessary_condition/no_unnecessary_condition.go`.

use tsrs_ast::{self as ast, Kind, Node, SymbolFlags};
use tsrs_checker::{
    self as checker, AccessFlags, AliasArg, CheckMode, LiteralValue, MappedTypeModifiers,
    ObjectFlags, SignatureKind, Type, TypeData, TypeFlags, TypePredicateKind,
};
use tsrs_core::{P, json::Value};

use crate::protocol::{Diagnostic, Fix, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::{self, union_type_parts};

pub struct NoUnnecessaryCondition {
    loop_condition_mode: String,
    check_type_predicates: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum MessageId {
    AlwaysTruthy,
    AlwaysFalsy,
    Never,
    AlwaysTruthyFunc,
    AlwaysFalsyFunc,
    NeverNullish,
    NeverOptionalChain,
    NoStrictNullCheck,
    ComparisonBetweenLiteralTypes,
    TypeGuardAlreadyIsType,
    AlwaysNullish,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description) = match self {
            Self::AlwaysTruthy => {
                ("alwaysTruthy", "Unnecessary conditional, value is always truthy.")
            }
            Self::AlwaysFalsy => ("alwaysFalsy", "Unnecessary conditional, value is always falsy."),
            Self::Never => ("never", "Unnecessary conditional, value is `never`."),
            Self::AlwaysTruthyFunc => (
                "alwaysTruthyFunc",
                "This callback should return a conditional, but return is always truthy.",
            ),
            Self::AlwaysFalsyFunc => (
                "alwaysFalsyFunc",
                "This callback should return a conditional, but return is always falsy.",
            ),
            Self::NeverNullish => (
                "neverNullish",
                "Unnecessary conditional, expected left-hand side of `??` operator to be possibly null or undefined.",
            ),
            Self::NeverOptionalChain => {
                ("neverOptionalChain", "Unnecessary optional chain on a non-nullish value.")
            }
            Self::NoStrictNullCheck => (
                "noStrictNullCheck",
                "This rule requires the `strictNullChecks` compiler option to be turned on to function correctly.",
            ),
            Self::ComparisonBetweenLiteralTypes => {
                ("comparisonBetweenLiteralTypes", "Unnecessary comparison between literal values.")
            }
            Self::TypeGuardAlreadyIsType => (
                "typeGuardAlreadyIsType",
                "Type predicate is unnecessary as the parameter type already satisfies the predicate.",
            ),
            Self::AlwaysNullish => {
                ("alwaysNullish", "Unnecessary conditional, value is always nullish.")
            }
        };
        RuleMessage {
            id: id.into(),
            description: description.into(),
            help: if self == Self::NeverOptionalChain {
                "Remove the optional chain; the value cannot be `null` or `undefined`.".into()
            } else {
                String::new()
            },
        }
    }
}

fn truncate_type_name(bytes: Vec<u8>) -> Option<String> {
    let mut runes = Vec::new();
    let mut remaining = bytes.as_slice();
    while !remaining.is_empty() {
        match std::str::from_utf8(remaining) {
            Ok(text) => {
                runes.extend(text.chars());
                break;
            }
            Err(error) => {
                let (valid, invalid) = remaining.split_at(error.valid_up_to());
                runes.extend(std::str::from_utf8(valid).unwrap().chars());
                runes.push(char::REPLACEMENT_CHARACTER);
                remaining = &invalid[1..];
            }
        }
    }
    if runes.len() > 120 {
        Some(runes[..117].iter().chain(['.', '.', '.'].iter()).collect())
    } else {
        String::from_utf8(bytes).ok()
    }
}

fn type_name(context: &mut RuleContext<'_>, ty: P<Type>) -> Option<String> {
    truncate_type_name(context.checker.type_to_string_bytes_exported(ty))
}

fn typed_message(
    context: &mut RuleContext<'_>,
    ty: P<Type>,
    node: P<Node>,
    id: MessageId,
    returning: bool,
) -> Option<RuleMessage> {
    let mut result = id.message();
    if !matches!(
        ast::skip_parentheses(node).kind(),
        Kind::TrueKeyword
            | Kind::FalseKeyword
            | Kind::NullKeyword
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::StringLiteral
            | Kind::NoSubstitutionTemplateLiteral
    ) {
        let ty = type_name(context, ty)?;
        let help = if returning {
            format!("The callback returns type `{ty}`.")
        } else {
            format!("The checked value has type `{ty}`.")
        };
        result.help = if result.help.is_empty() { help } else { format!("{help} {}", result.help) };
    }
    Some(result)
}

fn comparison_message(
    context: &mut RuleContext<'_>,
    left: P<Type>,
    right: P<Type>,
    mut message: RuleMessage,
) -> Option<RuleMessage> {
    let left = type_name(context, left)?;
    let right = type_name(context, right)?;
    message.help =
        format!("The left side has type `{left}`, and the right side has type `{right}`.");
    Some(message)
}

fn guard_message(
    context: &mut RuleContext<'_>,
    value: P<Type>,
    predicate: P<Type>,
) -> Option<RuleMessage> {
    let value = type_name(context, value)?;
    let predicate = type_name(context, predicate)?;
    let mut result = MessageId::TypeGuardAlreadyIsType.message();
    result.help = format!(
        "Type `{value}` already satisfies predicate type `{predicate}`. Remove the redundant type check."
    );
    Some(result)
}

fn is_indeterminate_type(t: P<Type>) -> bool {
    t.flags().intersects(
        TypeFlags::Any | TypeFlags::Unknown | TypeFlags::TypeParameter | TypeFlags::Index,
    )
}

fn is_true_literal_type_value(t: P<Type>) -> Option<bool> {
    if !t.flags().intersects(TypeFlags::BooleanLiteral) {
        return None;
    }
    match t.data() {
        TypeData::Intrinsic(intrinsic) => match intrinsic.intrinsic_name() {
            "true" => Some(true),
            "false" => Some(false),
            _ => None,
        },
        TypeData::Literal(literal) => match literal.value() {
            Some(LiteralValue::Boolean(value)) => Some(value),
            _ => None,
        },
        _ => None,
    }
}

fn is_static_value(t: P<Type>) -> bool {
    let flags = t.flags();
    if flags.intersects(TypeFlags::BooleanLiteral) {
        return is_true_literal_type_value(t).is_some();
    }
    flags.intersects(TypeFlags::Undefined | TypeFlags::Null)
        || flags.intersects(
            TypeFlags::StringLiteral | TypeFlags::NumberLiteral | TypeFlags::BigIntLiteral,
        ) && matches!(t.data(), TypeData::Literal(_))
}

fn literal_string(t: P<Type>) -> Option<String> {
    match t.data() {
        TypeData::Literal(literal) => match literal.value() {
            Some(LiteralValue::String(value)) => Some(value.into()),
            Some(LiteralValue::Number(value)) => Some(value.to_string()),
            Some(LiteralValue::BigInt(value)) => Some(value.string()),
            Some(LiteralValue::Boolean(value)) => Some(value.to_string()),
            None => None,
        },
        _ => None,
    }
}

fn check_type_condition(t: P<Type>) -> (bool, bool) {
    let flags = t.flags();
    if flags.intersects(TypeFlags::Never) {
        return (false, true);
    }
    if flags.intersects(TypeFlags::IndexedAccess) {
        return (false, false);
    }
    if flags.intersects(TypeFlags::Union) {
        let mut all_truthy = true;
        let mut all_falsy = true;
        for &part in t.types() {
            let (truthy, falsy) = check_type_condition(part);
            all_truthy &= truthy;
            all_falsy &= falsy;
        }
        return (all_truthy, all_falsy);
    }
    if flags.intersects(TypeFlags::Intersection) {
        let mut all_truthy = true;
        for &part in t.types() {
            let (truthy, falsy) = check_type_condition(part);
            if falsy {
                return (false, true);
            }
            all_truthy &= truthy;
        }
        return (all_truthy, false);
    }
    if flags.intersects(TypeFlags::Null | TypeFlags::Undefined | TypeFlags::Void) {
        return (false, true);
    }
    if flags.intersects(
        TypeFlags::Object
            | TypeFlags::NonPrimitive
            | TypeFlags::ESSymbol
            | TypeFlags::UniqueESSymbol,
    ) {
        return (true, false);
    }
    if let Some(value) = is_true_literal_type_value(t) {
        return (value, !value);
    }
    if flags.intersects(TypeFlags::StringLiteral)
        && let Some(value) = literal_string(t)
    {
        let falsy = value.is_empty();
        return (!falsy, falsy);
    }
    if flags.intersects(TypeFlags::NumberLiteral)
        && let Some(value) = literal_string(t)
    {
        let falsy = value == "0" || value == "NaN";
        return (!falsy, falsy);
    }
    if flags.intersects(TypeFlags::BigIntLiteral)
        && let Some(value) = literal_string(t)
    {
        let falsy = value == "0" || value == "0n";
        return (!falsy, falsy);
    }
    (false, false)
}

fn is_nullish_type(t: P<Type>) -> bool {
    if t.flags().intersects(TypeFlags::Union) {
        return t.types().iter().any(|&part| is_nullish_type(part));
    }
    t.flags().intersects(TypeFlags::Null | TypeFlags::Undefined | TypeFlags::Void)
}

fn remove_nullish_from_type(t: P<Type>) -> Option<P<Type>> {
    if !t.flags().intersects(TypeFlags::Union) {
        return (!is_nullish_type(t)).then_some(t);
    }
    t.types().iter().copied().find(|&part| !is_nullish_type(part))
}

fn resolve_indexed_access_type(context: &mut RuleContext<'_>, t: P<Type>) -> P<Type> {
    if !t.flags().intersects(TypeFlags::IndexedAccess) {
        return t;
    }
    let access = t.as_indexed_access_type();
    let Some(mut object_type) = access.object_type() else {
        return t;
    };
    let Some(mut index_type) = access.index_type() else {
        return t;
    };
    object_type = context.checker.get_base_constraint_of_type(object_type).unwrap_or(object_type);
    index_type = context.checker.get_base_constraint_of_type(index_type).unwrap_or(index_type);
    if index_type.flags().intersects(TypeFlags::StringLiteral | TypeFlags::NumberLiteral)
        && let Some(name) = literal_string(index_type)
        && let Some(property) =
            context.checker.get_type_of_property_of_type_exported(object_type, &name)
    {
        return property;
    }
    let index_parts = union_type_parts(&index_type);
    if index_parts.iter().any(|part| part.flags().intersects(TypeFlags::StringLike))
        && let Some(index) = string_index_type(context, object_type)
    {
        return index;
    }
    if index_parts.iter().any(|part| part.flags().intersects(TypeFlags::NumberLike))
        && let Some(index) =
            context.checker.get_index_type_of_type(object_type, context.checker.number_type)
    {
        return index;
    }
    t
}

fn string_index_type(context: &mut RuleContext<'_>, t: P<Type>) -> Option<P<Type>> {
    context.checker.get_index_type_of_type(t, context.checker.string_type)
}

fn get_resolved_type(context: &mut RuleContext<'_>, node: P<Node>) -> Option<P<Type>> {
    let mut t = context.checker.get_type_at_location(node);
    if t.flags().intersects(TypeFlags::TypeParameter) {
        let constraint = context.checker.get_base_constraint_of_type(t)?;
        return Some(resolve_indexed_access_type(context, constraint));
    }
    if t.flags().intersects(TypeFlags::Conditional)
        && let Some(constraint) = context.checker.get_base_constraint_of_type(t)
    {
        t = constraint;
    }
    Some(resolve_indexed_access_type(context, t))
}

fn node_is_array_type(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    get_resolved_type(context, node).is_some_and(|t| {
        union_type_parts(&t).iter().any(|&part| context.checker.is_array_type(part))
    })
}

fn node_is_tuple_type(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    get_resolved_type(context, node).is_some_and(|t| {
        union_type_parts(&t).iter().any(|&part| checker::is_tuple_type_exported(part))
    })
}

fn is_array_index_expression(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let node = ast::skip_parentheses(node);
    if node.kind() != Kind::ElementAccessExpression {
        return false;
    }
    let access = node.as_element_access_expression();
    let argument = access.argument_expression();
    node_is_array_type(context, access.expression)
        || node_is_tuple_type(context, access.expression)
            && ast::skip_parentheses(argument).kind() != Kind::NumericLiteral
}

fn is_conditional_always_necessary(t: P<Type>) -> bool {
    union_type_parts(&t).iter().any(|part| {
        part.flags().intersects(
            TypeFlags::Any | TypeFlags::Unknown | TypeFlags::TypeVariable | TypeFlags::Conditional,
        )
    })
}

fn chain_expression(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::PropertyAccessExpression => Some(node.as_property_access_expression().expression),
        Kind::ElementAccessExpression => Some(node.as_element_access_expression().expression),
        Kind::CallExpression => Some(node.as_call_expression().expression),
        _ => None,
    }
}

fn has_question_dot(node: P<Node>) -> bool {
    match node.kind() {
        Kind::PropertyAccessExpression => {
            node.as_property_access_expression().question_dot_token().is_some()
        }
        Kind::ElementAccessExpression => {
            node.as_element_access_expression().question_dot_token().is_some()
        }
        Kind::CallExpression => node.as_call_expression().question_dot_token().is_some(),
        _ => false,
    }
}

fn has_optional_chain(node: P<Node>) -> bool {
    let node = ast::skip_parentheses(node);
    has_question_dot(node) || chain_expression(node).is_some_and(has_optional_chain)
}

fn get_property_name_from_literal_type(t: P<Type>) -> Option<String> {
    if t.flags().intersects(TypeFlags::StringLiteral | TypeFlags::NumberLiteral) {
        literal_string(t)
    } else {
        None
    }
}

fn is_nullable_property_type(
    context: &mut RuleContext<'_>,
    object: P<Type>,
    property: P<Type>,
) -> bool {
    if property.flags().intersects(TypeFlags::Union) {
        return property
            .types()
            .iter()
            .any(|&part| is_nullable_property_type(context, object, part));
    }
    if let Some(name) = get_property_name_from_literal_type(property)
        && let Some(t) = context.checker.get_type_of_property_of_type_exported(object, &name)
    {
        return is_nullish_type(t);
    }
    let name = utils::get_type_name(context.checker, property);
    context
        .checker
        .get_index_infos_of_type_exported(object)
        .iter()
        .any(|info| utils::get_type_name(context.checker, info.key_type()) == name)
}

fn is_nullable_element_access_expression(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let access = node.as_element_access_expression();
    let argument = access.argument_expression();
    let object = context.checker.get_type_at_location(access.expression);
    let property = context.checker.get_type_at_location(argument);
    is_nullable_property_type(context, object, property)
}

fn get_call_return_type(context: &mut RuleContext<'_>, node: P<Node>) -> Option<P<Type>> {
    if node.kind() != Kind::CallExpression {
        return None;
    }
    let call = node.as_call_expression();
    if call.question_dot_token().is_none()
        && !has_optional_chain(call.expression)
        && let Some(t) = get_resolved_type(context, node)
    {
        return Some(t);
    }
    let signature = context.checker.get_resolved_signature(node, None, CheckMode::Normal);
    Some(context.checker.get_return_type_of_signature(signature))
}

fn get_property_type_from_base(
    context: &mut RuleContext<'_>,
    base: P<Type>,
    node: P<Node>,
) -> Option<P<Type>> {
    if node.kind() != Kind::PropertyAccessExpression {
        return None;
    }
    let base = remove_nullish_from_type(base)?;
    let Some(name_node) = node.name() else {
        return Some(context.checker.get_type_at_location(node));
    };
    let name = ast::get_text_of_property_name(name_node);
    if name.is_empty() {
        return Some(context.checker.get_type_at_location(node));
    }
    if let Some(property) = context.checker.get_property_of_type(base, &name) {
        return Some(context.checker.get_type_of_symbol_exported(property));
    }
    let apparent = context.checker.get_apparent_type(base);
    if apparent != base
        && let Some(property) = context.checker.get_property_of_type(apparent, &name)
    {
        return Some(context.checker.get_type_of_symbol_exported(property));
    }
    let mut index = string_index_type(context, base);
    if index.is_none() {
        index = string_index_type(context, apparent);
    }
    if index.is_none() && base.object_flags().intersects(ObjectFlags::Mapped) {
        for &property in context.checker.get_properties_of_type(base) {
            if property.name() == name {
                return (!property.flags().intersects(SymbolFlags::Optional))
                    .then(|| context.checker.get_type_of_symbol_exported(property));
            }
        }
        return None;
    }
    if let Some(index) = index {
        if context.program.options().no_unchecked_indexed_access.is_true() {
            return None;
        }
        return Some(index);
    }
    Some(context.checker.get_type_at_location(node))
}

fn get_type_from_call_property(
    context: &mut RuleContext<'_>,
    call: P<Node>,
    access: P<Node>,
) -> Option<P<Type>> {
    let return_type = get_call_return_type(context, call)
        .unwrap_or_else(|| context.checker.get_type_at_location(call));
    let return_type = remove_nullish_from_type(return_type)?;
    if access.kind() == Kind::PropertyAccessExpression {
        get_property_type_from_base(context, return_type, access)
    } else {
        Some(context.checker.get_type_at_location(access))
    }
}

fn is_call_expression_nullable_origin_from_callee(
    context: &mut RuleContext<'_>,
    node: P<Node>,
) -> bool {
    let Some(t) = get_resolved_type(context, node.as_call_expression().expression) else {
        return false;
    };
    if !t.flags().intersects(TypeFlags::Union) {
        return false;
    }
    let own_nullable = t.types().iter().any(|&part| {
        context.checker.get_signatures_of_type(part, SignatureKind::Call).iter().any(|&signature| {
            is_nullish_type(context.checker.get_return_type_of_signature(signature))
        })
    });
    !own_nullable && is_nullish_type(t)
}

fn is_member_expression_nullable_origin_from_object(
    context: &mut RuleContext<'_>,
    node: P<Node>,
) -> bool {
    let node = ast::skip_parentheses(node);
    let (object, property_type, property_name) = match node.kind() {
        Kind::PropertyAccessExpression => {
            let Some(name) = node.name() else {
                return false;
            };
            (
                node.as_property_access_expression().expression,
                None,
                ast::get_text_of_property_name(name),
            )
        }
        Kind::ElementAccessExpression => {
            let access = node.as_element_access_expression();
            let argument = access.argument_expression();
            (access.expression, get_resolved_type(context, argument), String::new())
        }
        _ => return false,
    };
    let Some(t) = get_resolved_type(context, object) else {
        return false;
    };
    if !t.flags().intersects(TypeFlags::Union) {
        return false;
    }
    let mut own_nullable = false;
    for &part in t.types() {
        if is_nullish_type(part) {
            continue;
        }
        if node.kind() == Kind::ElementAccessExpression {
            if property_type
                .is_some_and(|property| is_nullable_property_type(context, part, property))
            {
                own_nullable = true;
                break;
            }
            continue;
        }
        if property_name.is_empty() {
            continue;
        }
        if let Some(property) =
            context.checker.get_type_of_property_of_type_exported(part, &property_name)
        {
            if is_nullish_type(property) {
                own_nullable = true;
                break;
            }
            continue;
        }
        for &info in context.checker.get_index_infos_of_type_exported(part) {
            if utils::get_type_name(context.checker, info.key_type()) != "string" {
                continue;
            }
            if context.program.options().no_unchecked_indexed_access.is_true()
                || is_nullish_type(info.value_type())
            {
                own_nullable = true;
                break;
            }
        }
        if own_nullable {
            break;
        }
    }
    !own_nullable && is_nullish_type(t)
}

fn is_optionable_expression(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let node = ast::skip_parentheses(node);
    let Some(t) = get_resolved_type(context, node) else {
        return false;
    };
    if is_conditional_always_necessary(t) {
        return true;
    }
    let own_nullable = match node.kind() {
        Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
            !is_member_expression_nullable_origin_from_object(context, node)
        }
        Kind::CallExpression => !is_call_expression_nullable_origin_from_callee(context, node),
        _ => true,
    };
    own_nullable && is_nullish_type(t)
}

fn is_safe_tuple_access(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let access = node.as_element_access_expression();
    let argument = access.argument_expression();
    let argument = ast::skip_parentheses(argument);
    argument.kind() == Kind::NumericLiteral
        && get_resolved_type(context, access.expression)
            .is_some_and(checker::is_tuple_type_exported)
}

fn has_unsafe_element_access(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let node = ast::skip_parentheses(node);
    if node.kind() == Kind::ElementAccessExpression
        && !has_question_dot(node)
        && !context.program.options().no_unchecked_indexed_access.is_true()
        && !is_safe_tuple_access(context, node)
    {
        return true;
    }
    chain_expression(node).is_some_and(|inner| has_unsafe_element_access(context, inner))
}

fn chained_element_type(
    context: &mut RuleContext<'_>,
    base: P<Type>,
    expression: P<Node>,
) -> (P<Type>, Option<Vec<u8>>) {
    let argument = expression.as_element_access_expression().argument_expression();
    let key_type = context.checker.get_type_at_location(argument);
    let mut names = Vec::new();
    let mut representative = None;
    for &part in union_type_parts(&key_type) {
        let Some(name) =
            literal_string(part).filter(|_| part.flags().intersects(TypeFlags::StringLiteral))
        else {
            return (context.checker.get_type_at_location(expression), None);
        };
        let Some(property) = context.checker.get_property_of_type(base, &name) else {
            return (context.checker.get_type_at_location(expression), None);
        };
        let ty = context.checker.get_type_of_symbol_exported(property);
        if is_nullish_type(ty) {
            return (context.checker.get_type_at_location(expression), None);
        }
        representative.get_or_insert(ty);
        let name = context.checker.type_to_string_bytes_exported(ty);
        if !names.contains(&name) {
            names.push(name);
        }
    }
    if let Some(ty) = representative {
        let name = names.join(&b" | "[..]);
        (ty, Some(name))
    } else {
        (context.checker.get_type_at_location(expression), None)
    }
}

fn check_optional_chain(context: &mut RuleContext<'_>, node: P<Node>) {
    if !has_question_dot(node) {
        return;
    }
    let expression = chain_expression(node).expect("optional chain expression");
    let skipped = ast::skip_parentheses(expression);
    if skipped.kind() == Kind::ElementAccessExpression
        && !context.program.options().no_unchecked_indexed_access.is_true()
        && !has_question_dot(skipped)
        && !is_safe_tuple_access(context, skipped)
    {
        return;
    }
    if has_question_dot(ast::skip_parentheses(expression))
        && has_unsafe_element_access(context, expression)
    {
        return;
    }
    if is_optionable_expression(context, expression) {
        return;
    }
    let chained = has_question_dot(expression);
    let mut effective_type_name = None;
    let mut expression_type = if chained {
        let base_expression = chain_expression(expression).expect("chained expression");
        let Some(base_type) = get_resolved_type(context, base_expression) else {
            return;
        };
        let Some(base_type) = remove_nullish_from_type(base_type) else {
            return;
        };
        match expression.kind() {
            Kind::PropertyAccessExpression => {
                if let Some(t) = get_property_type_from_base(context, base_type, expression) {
                    Some(t)
                } else {
                    let t = context.checker.get_type_at_location(expression);
                    if !context.program.options().no_unchecked_indexed_access.is_true()
                        && base_type.object_flags().intersects(ObjectFlags::Mapped)
                        && !checker::get_mapped_type_modifiers(base_type)
                            .intersects(MappedTypeModifiers::IncludeOptional)
                    {
                        remove_nullish_from_type(t)
                    } else {
                        Some(t)
                    }
                }
            }
            Kind::ElementAccessExpression => {
                let (ty, name) = chained_element_type(context, base_type, expression);
                effective_type_name = name;
                Some(ty)
            }
            Kind::CallExpression => get_call_return_type(context, expression)
                .or_else(|| get_resolved_type(context, expression)),
            _ => Some(context.checker.get_type_at_location(expression)),
        }
    } else if matches!(
        expression.kind(),
        Kind::PropertyAccessExpression | Kind::ElementAccessExpression
    ) {
        let inner = chain_expression(expression).unwrap();
        if inner.kind() == Kind::CallExpression {
            get_type_from_call_property(context, inner, expression)
        } else {
            get_resolved_type(context, expression)
        }
    } else if expression.kind() == Kind::CallExpression {
        get_call_return_type(context, expression).or_else(|| get_resolved_type(context, expression))
    } else {
        get_resolved_type(context, expression)
    };
    if expression.kind() == Kind::CallExpression
        && is_call_expression_nullable_origin_from_callee(context, expression)
    {
        expression_type = expression_type.and_then(remove_nullish_from_type);
    }
    let Some(expression_type) = expression_type else {
        return;
    };
    if expression.kind() == Kind::CallExpression
        && let Some(function_type) =
            get_resolved_type(context, expression.as_call_expression().expression)
    {
        for &part in union_type_parts(&function_type) {
            if part.flags().intersects(TypeFlags::Null | TypeFlags::Undefined | TypeFlags::Void) {
                continue;
            }
            if let Some(&signature) =
                context.checker.get_signatures_of_type(part, SignatureKind::Call).first()
                && is_nullish_type(context.checker.get_return_type_of_signature(signature))
            {
                return;
            }
        }
    }
    if is_indeterminate_type(expression_type)
        || expression_type.flags().intersects(TypeFlags::Union)
            && expression_type.types().iter().any(|&part| is_indeterminate_type(part))
    {
        return;
    }
    if !is_nullish_type(expression_type) {
        let message = if let Some(bytes) = effective_type_name {
            let Some(name) = truncate_type_name(bytes) else {
                return;
            };
            let mut result = MessageId::NeverOptionalChain.message();
            result.help = format!(
                "The checked value has type `{name}`. Remove the optional chain; the value cannot be `null` or `undefined`."
            );
            result
        } else {
            let Some(message) = typed_message(
                context,
                expression_type,
                expression,
                MessageId::NeverOptionalChain,
                false,
            ) else {
                return;
            };
            message
        };
        let token = match node.kind() {
            Kind::PropertyAccessExpression => {
                node.as_property_access_expression().question_dot_token()
            }
            Kind::ElementAccessExpression => {
                node.as_element_access_expression().question_dot_token()
            }
            Kind::CallExpression => node.as_call_expression().question_dot_token(),
            _ => None,
        }
        .expect("optional chain token");
        let suggestions = if context.options.fix_suggestions {
            vec![Suggestion {
                message: RuleMessage {
                    id: "suggestRemoveOptionalChain".into(),
                    description: "Remove unnecessary optional chain".into(),
                    ..Default::default()
                },
                fixes: vec![Fix {
                    range: trim_node_range(context.source_file, token),
                    text: if node.kind() == Kind::PropertyAccessExpression {
                        ".".into()
                    } else {
                        String::new()
                    },
                }],
            }]
        } else {
            Vec::new()
        };
        context.report_diagnostic(Diagnostic {
            range: Some(trim_node_range(context.source_file, node)),
            message,
            suggestions,
            ..Default::default()
        });
    }
}

fn is_nullable_member_expression(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let node = ast::skip_parentheses(node);
    match node.kind() {
        Kind::PropertyAccessExpression => {
            let Some(name_node) = node.name() else {
                return false;
            };
            let base = context
                .checker
                .get_type_at_location(node.as_property_access_expression().expression);
            let name = ast::get_text_of_property_name(name_node);
            if name.is_empty() {
                return false;
            }
            let mut symbol = context
                .checker
                .get_symbol_at_location_exported(name_node)
                .or_else(|| context.checker.get_property_of_type(base, &name));
            if symbol.is_none() {
                symbol = context
                    .checker
                    .get_properties_of_type(base)
                    .iter()
                    .copied()
                    .find(|property| property.name() == name);
            }
            symbol.is_some_and(|symbol| symbol.flags().intersects(SymbolFlags::Optional))
        }
        Kind::ElementAccessExpression => is_nullable_element_access_expression(context, node),
        _ => false,
    }
}

fn option_chain_contains_option_array_index(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let node = ast::skip_parentheses(node);
    let Some(lhs) = chain_expression(node) else {
        return false;
    };
    if has_question_dot(node) && is_array_index_expression(context, lhs) {
        return true;
    }
    if chain_expression(lhs).is_some() {
        option_chain_contains_option_array_index(context, lhs)
    } else {
        false
    }
}

fn is_assertion_argument(context: &mut RuleContext<'_>, argument: P<Node>, enabled: bool) -> bool {
    if !enabled {
        return false;
    }
    let Some(call) = argument.parent().filter(|parent| parent.kind() == Kind::CallExpression)
    else {
        return false;
    };
    let Some(index) = call
        .arguments()
        .iter()
        .take_while(|node| node.kind() != Kind::SpreadElement)
        .position(|&node| node == argument)
    else {
        return false;
    };
    let signature = context.checker.get_resolved_signature(call, None, CheckMode::Normal);
    context.checker.get_type_predicate_of_signature_exported(signature).is_some_and(|predicate| {
        predicate.kind() == TypePredicateKind::AssertsIdentifier
            && predicate.type_().is_none()
            && predicate.parameter_index() == Some(index)
    })
}

fn is_only_used_for_truthiness(
    context: &mut RuleContext<'_>,
    mut node: P<Node>,
    check_type_predicates: bool,
) -> bool {
    let mut parent = node.parent();
    while let Some(next) = parent.filter(|parent| parent.kind() == Kind::ParenthesizedExpression) {
        node = next;
        parent = next.parent();
    }
    let Some(parent) = parent else {
        return false;
    };
    match parent.kind() {
        Kind::ConditionalExpression => return parent.as_conditional_expression().condition == node,
        Kind::DoStatement | Kind::IfStatement | Kind::WhileStatement => {
            return parent.expression() == Some(node);
        }
        Kind::ForStatement => return parent.as_for_statement().condition() == Some(node),
        Kind::PrefixUnaryExpression => {
            return parent.as_prefix_unary_expression().operator == Kind::ExclamationToken;
        }
        Kind::CallExpression => return is_assertion_argument(context, node, check_type_predicates),
        _ => {}
    }
    if ast::is_logical_expression(parent) {
        let binary = parent.as_binary_expression();
        return binary.operator_token.kind() == Kind::AmpersandAmpersandToken
            && binary.left == node
            || is_only_used_for_truthiness(context, parent, check_type_predicates);
    }
    false
}

fn nullish_coalescing_type(
    context: &mut RuleContext<'_>,
    node: P<Node>,
) -> (Option<P<Type>>, bool) {
    let mut ty = get_resolved_type(context, node);
    let node = ast::skip_parentheses(node);
    if !ast::is_assignment_target(node) {
        return (ty, false);
    }
    if !context.program.options().no_unchecked_indexed_access.is_true() {
        if node.kind() == Kind::ElementAccessExpression {
            ty = ty.map(|ty| context.checker.get_flow_type_of_reference(node, ty));
        }
        return (ty, false);
    }
    let read_type = match node.kind() {
        Kind::PropertyAccessExpression => {
            let access = node.as_property_access_expression();
            let name = access.name();
            if name.kind() == Kind::PrivateIdentifier {
                return (ty, false);
            }
            let Some(base) = get_resolved_type(context, access.expression) else {
                return (ty, false);
            };
            let name = ast::get_text_of_property_name(name);
            if context.checker.get_property_of_type(base, &name).is_some() {
                return (ty, false);
            }
            context.checker.get_type_of_property_or_index_signature_of_type(base, &name)
        }
        Kind::ElementAccessExpression => {
            let access = node.as_element_access_expression();
            let (Some(base), Some(key)) = (
                get_resolved_type(context, access.expression),
                get_resolved_type(context, access.argument_expression()),
            ) else {
                return (ty, false);
            };
            context.checker.get_indexed_access_type_or_undefined(
                base,
                key,
                AccessFlags::ExpressionPosition,
                None,
                AliasArg::None,
            )
        }
        _ => None,
    };
    match read_type {
        Some(read_type) => {
            (Some(context.checker.get_flow_type_of_reference(node, read_type)), true)
        }
        None => (ty, false),
    }
}

fn has_possibly_non_nullish_indexed_read(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let node = ast::skip_parentheses(node);
    if node.kind() != Kind::ElementAccessExpression || !ast::is_assignment_target(node) {
        return false;
    }
    let access = node.as_element_access_expression();
    let (Some(base), Some(key)) = (
        get_resolved_type(context, access.expression),
        get_resolved_type(context, access.argument_expression()),
    ) else {
        return false;
    };
    if !key.flags().intersects(TypeFlags::Union) {
        return false;
    }
    let Some(read_type) = context.checker.get_indexed_access_type_or_undefined(
        base,
        key,
        AccessFlags::ExpressionPosition,
        None,
        AliasArg::None,
    ) else {
        return false;
    };
    let flow_type = context.checker.get_flow_type_of_reference(node, read_type);
    union_type_parts(&flow_type).iter().any(|part| {
        !part.flags().intersects(TypeFlags::Null | TypeFlags::Undefined | TypeFlags::Void)
    })
}

fn check_node(
    context: &mut RuleContext<'_>,
    expression: P<Node>,
    check_type_predicates: bool,
    unary_not: bool,
    report_node: Option<P<Node>>,
) {
    let report_node = report_node.unwrap_or(expression);
    let expression = ast::skip_parentheses(expression);
    if expression.kind() == Kind::PrefixUnaryExpression {
        let unary = expression.as_prefix_unary_expression();
        if unary.operator == Kind::ExclamationToken {
            check_node(
                context,
                unary.operand,
                check_type_predicates,
                !unary_not,
                Some(report_node),
            );
            return;
        }
    }
    if !context.program.options().no_unchecked_indexed_access.is_true()
        && is_array_index_expression(context, expression)
    {
        return;
    }
    if expression.kind() == Kind::BinaryExpression {
        let binary = expression.as_binary_expression();
        if matches!(binary.operator_token.kind(), Kind::AmpersandAmpersandToken | Kind::BarBarToken)
        {
            if is_only_used_for_truthiness(context, expression, check_type_predicates) {
                check_node(context, binary.right(), check_type_predicates, false, None);
            }
            return;
        }
    }
    let Some(mut t) = get_resolved_type(context, expression) else {
        return;
    };
    if is_conditional_always_necessary(t) {
        return;
    }
    if expression.kind() == Kind::ElementAccessExpression
        && t.flags().intersects(TypeFlags::IndexedAccess)
        && let Some(base) =
            get_resolved_type(context, expression.as_element_access_expression().expression)
        && let Some(index) = string_index_type(context, base)
    {
        t = index;
    }
    if t.flags().intersects(TypeFlags::Never) {
        let Some(message) = typed_message(context, t, expression, MessageId::Never, false) else {
            return;
        };
        context.report_node(report_node, message);
        return;
    }
    let (truthy, falsy) = check_type_condition(t);
    if falsy {
        let Some(message) = typed_message(
            context,
            t,
            expression,
            if unary_not { MessageId::AlwaysTruthy } else { MessageId::AlwaysFalsy },
            false,
        ) else {
            return;
        };
        context.report_node(report_node, message);
        return;
    }
    if truthy {
        let Some(message) = typed_message(
            context,
            t,
            expression,
            if unary_not { MessageId::AlwaysFalsy } else { MessageId::AlwaysTruthy },
            false,
        ) else {
            return;
        };
        context.report_node(report_node, message);
    }
}

fn check_node_for_nullish(context: &mut RuleContext<'_>, node: P<Node>) {
    let (ty, recovered_read_type) = nullish_coalescing_type(context, node);
    let Some(mut t) = ty else {
        return;
    };
    if is_conditional_always_necessary(t) {
        return;
    }
    if t.flags().intersects(TypeFlags::IndexedAccess) {
        t = resolve_indexed_access_type(context, t);
        if t.flags().intersects(TypeFlags::IndexedAccess) {
            return;
        }
    }
    if t.flags().intersects(TypeFlags::Never) {
        let Some(message) = typed_message(context, t, node, MessageId::Never, false) else {
            return;
        };
        context.report_node(node, message);
        return;
    }
    if t.flags().intersects(TypeFlags::Null | TypeFlags::Undefined | TypeFlags::Void)
        && !has_possibly_non_nullish_indexed_read(context, node)
    {
        let Some(message) = typed_message(context, t, node, MessageId::AlwaysNullish, false) else {
            return;
        };
        context.report_node(node, message);
        return;
    }
    if !is_nullish_type(t) && (recovered_read_type || !is_nullable_member_expression(context, node))
    {
        let node = ast::skip_parentheses(node);
        if context.program.options().no_unchecked_indexed_access.is_true()
            || !(is_array_index_expression(context, node)
                || has_optional_chain(node)
                    && option_chain_contains_option_array_index(context, node))
        {
            let Some(message) = typed_message(context, t, node, MessageId::NeverNullish, false)
            else {
                return;
            };
            context.report_node(node, message);
        }
    }
}

fn is_comparable(t: P<Type>, mut flags: TypeFlags, operator: Kind) -> bool {
    flags |=
        TypeFlags::Any | TypeFlags::Unknown | TypeFlags::TypeParameter | TypeFlags::TypeVariable;
    if matches!(operator, Kind::EqualsEqualsToken | Kind::ExclamationEqualsToken) {
        flags |= TypeFlags::Null | TypeFlags::Undefined | TypeFlags::Void;
    }
    let type_flags = t.flags();
    if type_flags.intersects(
        flags
            | TypeFlags::Conditional
            | TypeFlags::TypeVariable
            | TypeFlags::Substitution
            | TypeFlags::IncludesConstrainedTypeVariable,
    ) {
        return true;
    }
    if type_flags.intersects(TypeFlags::Union) {
        return t.types().iter().any(|&part| is_comparable(part, flags, operator));
    }
    if type_flags.intersects(TypeFlags::Intersection) {
        return !t.types().is_empty()
            && t.types().iter().all(|&part| is_comparable(part, flags, operator));
    }
    false
}

fn check_if_bool_expression_is_necessary_conditional(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    left: P<Node>,
    right: P<Node>,
    operator: Kind,
) {
    let Some(left_type) = get_resolved_type(context, left) else {
        return;
    };
    let Some(right_type) = get_resolved_type(context, right) else {
        return;
    };
    if is_static_value(left_type) && is_static_value(right_type) {
        let Some(message) = comparison_message(
            context,
            left_type,
            right_type,
            MessageId::ComparisonBetweenLiteralTypes.message(),
        ) else {
            return;
        };
        context.report_node(node, message);
        return;
    }
    let options = context.program.options();
    if !options.get_strict_option_value(options.strict_null_checks) {
        return;
    }
    let lf = left_type.flags();
    let rf = right_type.flags();
    if lf == TypeFlags::Undefined
        && !is_comparable(right_type, TypeFlags::Undefined | TypeFlags::Void, operator)
        || rf == TypeFlags::Undefined
            && !is_comparable(left_type, TypeFlags::Undefined | TypeFlags::Void, operator)
        || lf == TypeFlags::Null && !is_comparable(right_type, TypeFlags::Null, operator)
        || rf == TypeFlags::Null && !is_comparable(left_type, TypeFlags::Null, operator)
    {
        let Some(message) = comparison_message(context, left_type, right_type, RuleMessage { id: "noOverlapBooleanExpression".into(), description: "This condition will always return the same value since the types have no overlap.".into(), ..Default::default() }) else { return; };
        context.report_node(node, message);
    }
}

fn check_predicate_function(context: &mut RuleContext<'_>, node: P<Node>, check_guards: bool) {
    let function_type = context.checker.get_type_at_location(node);
    for &signature in context.checker.get_signatures_of_type(function_type, SignatureKind::Call) {
        if let Some(predicate) = context.checker.get_type_predicate_of_signature_exported(signature)
            && check_guards
            && let Some(index) = predicate.parameter_index()
            && let Some(&parameter) = signature.parameters().get(index)
        {
            let parameter_type = context.checker.get_type_of_symbol_exported(parameter);
            if matches!(predicate.kind(), TypePredicateKind::Identifier | TypePredicateKind::This)
                && let Some(predicate_type) = predicate.type_()
                && context.checker.is_type_assignable_to_exported(parameter_type, predicate_type)
            {
                let Some(message) = guard_message(context, parameter_type, predicate_type) else {
                    return;
                };
                let report = parameter
                    .value_declaration()
                    .filter(|&declaration| {
                        ast::get_source_file_of_node(declaration) == Some(context.source_file)
                    })
                    .unwrap_or(node);
                context.report_node(report, message);
                return;
            }
        }
        let mut return_type = context.checker.get_return_type_of_signature(signature);
        if return_type.flags().intersects(TypeFlags::TypeParameter) {
            return_type = context
                .checker
                .get_constraint_of_type_parameter_exported(return_type)
                .unwrap_or(return_type);
        }
        let (truthy, falsy) = check_type_condition(return_type);
        let literal = matches!(node.kind(), Kind::ArrowFunction | Kind::FunctionExpression);
        let report_node = if literal { node.body().unwrap_or(node) } else { node };
        if truthy || falsy {
            let id = match (truthy, literal) {
                (true, true) => MessageId::AlwaysTruthy,
                (true, false) => MessageId::AlwaysTruthyFunc,
                (false, true) => MessageId::AlwaysFalsy,
                (false, false) => MessageId::AlwaysFalsyFunc,
            };
            let Some(message) = typed_message(context, return_type, report_node, id, true) else {
                return;
            };
            context.report_node(report_node, message);
        }
    }
}

impl NoUnnecessaryCondition {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("no-unnecessary-condition: options must be an object".into()),
        };
        let loop_condition_mode =
            match object.and_then(|object| object.get("allowConstantLoopConditions")) {
                Some(Value::String(value)) => value.clone(),
                Some(Value::Bool(true)) => "always".into(),
                _ => "never".into(),
            };
        let check_type_predicates =
            match object.and_then(|object| object.get("checkTypePredicates")) {
                None | Some(Value::Null) => false,
                Some(Value::Bool(value)) => *value,
                _ => {
                    return Err(
                        "no-unnecessary-condition: checkTypePredicates must be a boolean".into()
                    );
                }
            };
        Ok(Self { loop_condition_mode, check_type_predicates })
    }

    fn check_loop(&self, context: &mut RuleContext<'_>, test: P<Node>) {
        let skipped = ast::skip_parentheses(test);
        if self.loop_condition_mode == "only-allowed-literals"
            && (matches!(skipped.kind(), Kind::TrueKeyword | Kind::FalseKeyword)
                || skipped.kind() == Kind::NumericLiteral && matches!(skipped.text(), "0" | "1"))
        {
            return;
        }
        if self.loop_condition_mode == "always"
            && (skipped.kind() == Kind::TrueKeyword
                || get_resolved_type(context, test)
                    .is_some_and(|t| is_true_literal_type_value(t) == Some(true)))
        {
            return;
        }
        check_node(context, test, self.check_type_predicates, false, None);
    }

    fn check_type_predicate_call_expression(&self, context: &mut RuleContext<'_>, node: P<Node>) {
        if !self.check_type_predicates {
            return;
        }
        let call = node.as_call_expression();
        let arguments = call.arguments().nodes();
        let arguments = &arguments[..arguments
            .iter()
            .position(|argument| argument.kind() == Kind::SpreadElement)
            .unwrap_or(arguments.len())];
        if arguments.is_empty() {
            return;
        }
        let signature = context.checker.get_resolved_signature(node, None, CheckMode::Normal);
        let Some(predicate) = context.checker.get_type_predicate_of_signature_exported(signature)
        else {
            return;
        };
        let Some(index) = predicate.parameter_index() else {
            return;
        };
        let Some(&argument) = arguments.get(index) else {
            return;
        };
        match predicate.kind() {
            TypePredicateKind::AssertsIdentifier => {
                if predicate.type_().is_none() {
                    check_node(context, argument, self.check_type_predicates, false, None);
                    return;
                }
                if Some(utils::get_constrained_type_at_location(context.checker, argument))
                    == predicate.type_()
                {
                    let ty = utils::get_constrained_type_at_location(context.checker, argument);
                    let Some(message) = guard_message(context, ty, predicate.type_().unwrap())
                    else {
                        return;
                    };
                    context.report_node(argument, message);
                }
            }
            TypePredicateKind::Identifier
                if Some(utils::get_constrained_type_at_location(context.checker, argument))
                    == predicate.type_() =>
            {
                let ty = utils::get_constrained_type_at_location(context.checker, argument);
                let Some(message) = guard_message(context, ty, predicate.type_().unwrap()) else {
                    return;
                };
                context.report_node(argument, message);
            }
            _ => {}
        }
    }
}

impl Rule for NoUnnecessaryCondition {
    fn initialize(&mut self, context: &mut RuleContext<'_>) {
        let options = context.program.options();
        if !options.get_strict_option_value(options.strict_null_checks) {
            context.report_range(Range { pos: 0, end: 0 }, MessageId::NoStrictNullCheck.message());
        }
    }

    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::IfStatement
                    | Kind::WhileStatement
                    | Kind::DoStatement
                    | Kind::ForStatement
                    | Kind::ConditionalExpression
                    | Kind::BinaryExpression
                    | Kind::PropertyAccessExpression
                    | Kind::ElementAccessExpression
                    | Kind::CallExpression
                    | Kind::CaseClause
            )
        )
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match node.kind() {
            Kind::IfStatement => {
                check_node(
                    context,
                    node.as_if_statement().expression,
                    self.check_type_predicates,
                    false,
                    None,
                );
            }
            Kind::WhileStatement => self.check_loop(context, node.as_while_statement().expression),
            Kind::DoStatement => self.check_loop(context, node.as_do_statement().expression),
            Kind::ForStatement => {
                if let Some(test) = node.as_for_statement().condition() {
                    self.check_loop(context, test);
                }
            }
            Kind::ConditionalExpression => check_node(
                context,
                node.as_conditional_expression().condition,
                self.check_type_predicates,
                false,
                None,
            ),
            Kind::BinaryExpression => {
                let binary = node.as_binary_expression();
                let operator = binary.operator_token.kind();
                match operator {
                    Kind::AmpersandAmpersandToken
                    | Kind::BarBarToken
                    | Kind::AmpersandAmpersandEqualsToken
                    | Kind::BarBarEqualsToken => {
                        check_node(context, binary.left, self.check_type_predicates, false, None);
                    }
                    Kind::QuestionQuestionToken | Kind::QuestionQuestionEqualsToken => {
                        check_node_for_nullish(context, binary.left);
                    }
                    Kind::LessThanToken
                    | Kind::GreaterThanToken
                    | Kind::LessThanEqualsToken
                    | Kind::GreaterThanEqualsToken
                    | Kind::EqualsEqualsToken
                    | Kind::EqualsEqualsEqualsToken
                    | Kind::ExclamationEqualsToken
                    | Kind::ExclamationEqualsEqualsToken => {
                        check_if_bool_expression_is_necessary_conditional(
                            context,
                            node,
                            binary.left,
                            binary.right(),
                            operator,
                        );
                    }
                    _ => {}
                }
            }
            Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
                check_optional_chain(context, node);
            }
            Kind::CallExpression => {
                check_optional_chain(context, node);
                if utils::is_array_method_call_with_predicate(context.checker, node)
                    && let Some(&argument) = node.as_call_expression().arguments().nodes().first()
                {
                    check_predicate_function(context, argument, self.check_type_predicates);
                }
                self.check_type_predicate_call_expression(context, node);
            }
            Kind::CaseClause => {
                if let Some(expression) = node.expression()
                    && let Some(switch) = node
                        .parent()
                        .and_then(|parent| parent.parent())
                        .filter(|parent| parent.kind() == Kind::SwitchStatement)
                {
                    check_if_bool_expression_is_necessary_conditional(
                        context,
                        expression,
                        switch.as_switch_statement().expression,
                        expression,
                        Kind::EqualsEqualsEqualsToken,
                    );
                }
            }
            _ => unreachable!("no-unnecessary-condition listener"),
        }
    }
}
