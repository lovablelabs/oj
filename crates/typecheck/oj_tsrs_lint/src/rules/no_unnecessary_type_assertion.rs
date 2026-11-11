//! Port of tsgolint's `internal/rules/no_unnecessary_type_assertion/no_unnecessary_type_assertion.go`.

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{self as ast, CheckFlags, Kind, ModifierFlags, Node, NodeFlags, SymbolFlags};
use tsrs_checker::{
    self as checker, AliasArg, CheckMode, ContextFlags, ObjectFlags, Signature, SignatureKind,
    Type, TypeFacts, TypeFlags, UnionReduction,
};
use tsrs_core::{P, json::Value};

use crate::protocol::{Diagnostic, Fix, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, replace_node, trim_node_range};
use crate::utils;

pub struct NoUnnecessaryTypeAssertion {
    check_literal_const_assertions: bool,
    types_to_ignore: Vec<String>,
}

fn pos(node: P<Node>) -> u32 {
    node.pos().expect_source("parsed assertion node")
}
fn end(node: P<Node>) -> u32 {
    node.end().expect_source("parsed assertion node")
}

fn remove(range: Range) -> Fix {
    Fix { range, text: String::new() }
}
fn token_range(context: &RuleContext<'_>, at: u32) -> Range {
    let range = tsrs_scanner::get_range_of_token_at_position(context.source_file, at);
    Range { pos: range.source_pos().unwrap(), end: range.source_end().unwrap() }
}
fn is_literal(ty: P<Type>) -> bool {
    ty.flags().intersects(
        TypeFlags::StringLiteral
            | TypeFlags::NumberLiteral
            | TypeFlags::BigIntLiteral
            | TypeFlags::BooleanLiteral,
    )
}
fn is_const_assertion(node: P<Node>) -> bool {
    node.kind() == Kind::TypeReference && {
        let name = node.as_type_reference_node().type_name;
        name.kind() == Kind::Identifier && name.text() == "const"
    }
}
fn conceptually_literal(node: P<Node>) -> bool {
    let node = ast::skip_parentheses(node);
    matches!(
        node.kind(),
        Kind::ArrayLiteralExpression
            | Kind::ObjectLiteralExpression
            | Kind::ClassExpression
            | Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::JsxElement
            | Kind::JsxSelfClosingElement
            | Kind::JsxFragment
            | Kind::StringLiteral
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::RegularExpressionLiteral
            | Kind::NoSubstitutionTemplateLiteral
            | Kind::TrueKeyword
            | Kind::FalseKeyword
            | Kind::NullKeyword
            | Kind::TemplateExpression
    )
}
fn parent_through_parens(node: P<Node>) -> Option<P<Node>> {
    let mut parent = node.parent();
    while parent.is_some_and(|parent| parent.kind() == Kind::ParenthesizedExpression) {
        parent = parent.unwrap().parent();
    }
    parent
}
fn original_expression(node: P<Node>) -> P<Node> {
    let mut current = ast::skip_parentheses(node.expression().unwrap());
    while matches!(current.kind(), Kind::AsExpression | Kind::TypeAssertionExpression) {
        current = ast::skip_parentheses(current.expression().unwrap());
    }
    current
}
fn context_sensitive_call(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::CallExpression | Kind::NewExpression | Kind::TaggedTemplateExpression
    ) || (node.kind() == Kind::AwaitExpression
        && context_sensitive_call(ast::skip_parentheses(node.expression().unwrap())))
}
fn type_arguments(context: &mut RuleContext<'_>, ty: P<Type>) -> &'static [P<Type>] {
    if let Some(alias) = ty.alias()
        && !alias.type_arguments().is_empty()
    {
        return alias.type_arguments();
    }
    if !ty.object_flags().intersects(ObjectFlags::Reference) {
        return &[];
    }
    context.checker.get_type_arguments(ty)
}
fn type_contains(
    context: &mut RuleContext<'_>,
    ty: P<Type>,
    flags: TypeFlags,
    seen: &mut FxHashSet<P<Type>>,
    active_signatures: &mut FxHashSet<P<Signature>>,
) -> bool {
    if !seen.insert(ty) {
        return false;
    }
    if ty.flags().intersects(flags) {
        return true;
    }
    if ty.flags().intersects(TypeFlags::Union | TypeFlags::Intersection) {
        return ty
            .types()
            .iter()
            .any(|&part| type_contains(context, part, flags, seen, active_signatures));
    }
    for &argument in type_arguments(context, ty) {
        if type_contains(context, argument, flags, seen, active_signatures) {
            return true;
        }
    }
    for &signature in context.checker.get_signatures_of_type(ty, SignatureKind::Call) {
        let mut identity = signature;
        while let Some(target) = identity.target() {
            identity = target;
        }
        if !active_signatures.insert(identity) {
            return true;
        }
        for &parameter in signature.parameters() {
            let parameter_type = context.checker.get_type_of_symbol_exported(parameter);
            if type_contains(context, parameter_type, flags, seen, active_signatures) {
                return true;
            }
        }
        let return_type = context.checker.get_return_type_of_signature_exported(signature);
        if type_contains(context, return_type, flags, seen, active_signatures) {
            return true;
        }
        active_signatures.remove(&identity);
    }
    false
}
fn contains_flags(context: &mut RuleContext<'_>, ty: P<Type>, flags: TypeFlags) -> bool {
    type_contains(context, ty, flags, &mut FxHashSet::default(), &mut FxHashSet::default())
}
fn has_index_signature(context: &mut RuleContext<'_>, ty: P<Type>) -> bool {
    utils::union_type_parts(&ty)
        .iter()
        .any(|&part| !context.checker.get_index_infos_of_type_exported(part).is_empty())
}
fn same_properties(context: &mut RuleContext<'_>, uncast: P<Type>, cast: P<Type>) -> bool {
    let uncast_properties = context.checker.get_properties_of_type(uncast);
    let cast_properties = context.checker.get_properties_of_type(cast);
    if uncast_properties.len() != cast_properties.len() {
        return false;
    }
    let by_name: FxHashMap<_, _> =
        cast_properties.iter().map(|&property| (property.name(), property)).collect();
    uncast_properties.iter().all(|&property| {
        by_name.get(property.name()).is_some_and(|&cast_property| {
            context.checker.is_readonly_symbol_exported(property)
                == context.checker.is_readonly_symbol_exported(cast_property)
        })
    })
}
fn same_arguments(context: &mut RuleContext<'_>, uncast: P<Type>, cast: P<Type>) -> bool {
    type_arguments(context, uncast) == type_arguments(context, cast)
}
fn equivalent_ignoring_undefined(uncast: P<Type>, cast: P<Type>) -> bool {
    let uncast_parts: FxHashSet<_> = utils::union_type_parts(&uncast)
        .iter()
        .copied()
        .filter(|part| !part.flags().intersects(TypeFlags::Undefined))
        .collect();
    let mut count = 0;
    for &part in utils::union_type_parts(&cast) {
        if part.flags().intersects(TypeFlags::Undefined) {
            continue;
        }
        if !uncast_parts.contains(&part) {
            return false;
        }
        count += 1;
    }
    uncast_parts.len() == count
}
fn empty_object(context: &mut RuleContext<'_>, ty: P<Type>) -> bool {
    ty.flags().intersects(TypeFlags::NonPrimitive)
        || (context.checker.get_properties_of_type(ty).is_empty()
            && context.checker.get_signatures_of_type(ty, SignatureKind::Call).is_empty()
            && context.checker.get_signatures_of_type(ty, SignatureKind::Construct).is_empty()
            && context.checker.get_index_infos_of_type_exported(ty).is_empty())
}
fn phantom_arguments(context: &mut RuleContext<'_>, ty: P<Type>) -> bool {
    empty_object(context, ty) && !type_arguments(context, ty).is_empty()
}
fn receiver_of_write_access(node: P<Node>) -> bool {
    let mut current = parent_through_parens(node);
    while let Some(access) = current.filter(|&node| ast::is_access_expression(node)) {
        if ast::is_write_access(access) {
            return true;
        }
        current = parent_through_parens(access);
    }
    false
}
fn element_access_argument(mut node: P<Node>) -> bool {
    let mut parent = node.parent();
    while let Some(next) = parent.filter(|parent| parent.kind() == Kind::ParenthesizedExpression) {
        node = next;
        parent = next.parent();
    }
    parent.is_some_and(|parent| {
        parent.kind() == Kind::ElementAccessExpression
            && parent.as_element_access_expression().argument_expression() == node
    })
}
fn has_enum_type(ty: P<Type>) -> bool {
    ty.flags().intersects(TypeFlags::EnumLike)
        || ty.flags().intersects(TypeFlags::Union | TypeFlags::Intersection)
            && ty.types().iter().any(|&part| has_enum_type(part))
}
fn constrained_to(
    context: &mut RuleContext<'_>,
    source: P<Type>,
    target: P<Type>,
    seen: &mut FxHashSet<P<Type>>,
) -> bool {
    if source == target {
        return true;
    }
    if !seen.insert(source) {
        return false;
    }
    if source.flags().intersects(TypeFlags::TypeParameter) {
        return context
            .checker
            .get_constraint_of_type_parameter_exported(source)
            .is_some_and(|constraint| constrained_to(context, constraint, target, seen));
    }
    source.flags().intersects(TypeFlags::Intersection)
        && source.types().iter().any(|&part| constrained_to(context, part, target, seen))
}
fn generic_inference_parameter(
    context: &mut RuleContext<'_>,
    call: P<Node>,
    argument_index: usize,
    element_path: &[usize],
) -> bool {
    let mut signature = context.checker.get_resolved_signature(call, None, CheckMode::Normal);
    while let Some(target) = signature.target() {
        signature = target;
    }
    if signature.type_parameters().is_empty() || signature.parameters().is_empty() {
        return false;
    }
    let parameter_index = argument_index.min(signature.parameters().len() - 1);
    let parameter = signature.parameters()[parameter_index];
    let mut ty = context.checker.get_type_of_symbol_exported(parameter);
    if parameter.value_declaration().is_some_and(utils::is_rest_parameter_declaration) {
        let arguments = type_arguments(context, ty);
        if !arguments.is_empty() {
            ty = arguments[(argument_index - parameter_index).min(arguments.len() - 1)];
        }
    }
    for &index in element_path {
        let element = if checker::is_tuple_type_exported(ty) {
            let arguments = context.checker.get_type_arguments(ty);
            (!arguments.is_empty()).then(|| arguments[index.min(arguments.len() - 1)])
        } else {
            context.checker.get_index_type_of_type(ty, context.checker.number_type)
        };
        let Some(element) = element else {
            break;
        };
        ty = element;
    }
    contains_flags(context, ty, TypeFlags::TypeVariable | TypeFlags::Index)
}
fn nested_array_generic_argument(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let mut path = Vec::new();
    let mut child = node;
    for current in std::iter::successors(node.parent(), |node| node.parent()) {
        if matches!(current.kind(), Kind::FunctionExpression | Kind::ArrowFunction) {
            return false;
        }
        if current.kind() == Kind::ArrayLiteralExpression
            && let Some(index) =
                current.as_array_literal_expression().elements.nodes().iter().position(|&element| {
                    element == child
                        || ast::skip_parentheses(element) == ast::skip_parentheses(child)
                })
        {
            path.insert(0, index);
            let mut argument = current;
            let mut parent = parent_through_parens(argument);
            let mut offset = 0;
            if let Some(spread) = parent.filter(|parent| parent.kind() == Kind::SpreadElement) {
                offset = index;
                argument = spread;
                parent = parent_through_parens(argument);
            }
            if let Some(call) = parent.filter(|parent| {
                matches!(parent.kind(), Kind::CallExpression | Kind::NewExpression)
            }) {
                let type_arguments = if call.kind() == Kind::CallExpression {
                    call.as_call_expression().type_arguments()
                } else {
                    call.as_new_expression().type_arguments()
                };
                if type_arguments.is_some() {
                    return false;
                }
                if let Some(index) = call.arguments().iter().position(|&candidate| {
                    candidate == argument || ast::skip_parentheses(candidate) == argument
                }) {
                    let path =
                        if argument.kind() == Kind::SpreadElement { &path[1..] } else { &path[..] };
                    return generic_inference_parameter(context, call, index + offset, path);
                }
            }
        }
        child = current;
    }
    false
}
fn property_in_inferred_callback_return(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let Some(property) = parent_through_parens(node).filter(|parent| {
        parent.kind() == Kind::PropertyAssignment
            && parent.initializer().map(ast::skip_parentheses) == Some(node)
    }) else {
        return false;
    };
    let Some(object) =
        property.parent().filter(|node| node.kind() == Kind::ObjectLiteralExpression)
    else {
        return false;
    };
    parent_through_parens(object).is_some_and(|callback| {
        callback.kind() == Kind::ArrowFunction
            && callback.body().map(ast::skip_parentheses) == Some(object)
            && in_generic_context(context, node)
    })
}
fn destructuring_default(mut target: P<Node>, path: &[String]) -> Option<P<Node>> {
    for index in (0..path.len()).rev() {
        target = ast::skip_parentheses(target);
        let element = match target.kind() {
            Kind::ArrayLiteralExpression => *target
                .as_array_literal_expression()
                .elements
                .nodes()
                .get(path[index].parse::<usize>().ok()?)?,
            Kind::ObjectLiteralExpression => {
                let mut result = None;
                for &property in target.properties() {
                    if property.kind() == Kind::SpreadAssignment || property.name().is_none() {
                        continue;
                    }
                    let name = ast::try_get_text_of_property_name(property.name().unwrap())?;
                    if name == path[index] {
                        if result.is_some() {
                            return None;
                        }
                        result = Some(property);
                    }
                }
                result?
            }
            _ => return None,
        };
        if index == 0 {
            if element.kind() == Kind::ShorthandPropertyAssignment {
                return element.as_shorthand_property_assignment().object_assignment_initializer();
            }
            let element = if element.kind() == Kind::PropertyAssignment {
                element.initializer()?
            } else {
                element
            };
            return ast::is_assignment_expression(element, true)
                .then(|| element.as_binary_expression().right());
        }
        target = ast::get_target_of_binding_or_assignment_element(element)?;
    }
    None
}
fn logical_result_type(
    context: &mut RuleContext<'_>,
    operator: Kind,
    left: P<Type>,
    right: P<Type>,
) -> P<Type> {
    match operator {
        Kind::AmpersandAmpersandToken
            if context.checker.has_type_facts(left, TypeFacts::Truthy) =>
        {
            let source = if context
                .program
                .options()
                .get_strict_option_value(context.program.options().strict_null_checks)
            {
                left
            } else {
                context.checker.get_base_type_of_literal_type_exported(right)
            };
            let falsy = context.checker.extract_definitely_falsy_types(source);
            context.checker.get_union_type_ex(
                &[falsy, right],
                UnionReduction::Literal,
                AliasArg::None,
                None,
            )
        }
        Kind::BarBarToken if context.checker.has_type_facts(left, TypeFacts::Falsy) => {
            let truthy = context.checker.remove_definitely_falsy_types(left);
            let truthy = context.checker.get_non_nullable_type(truthy);
            context.checker.get_union_type_ex(
                &[truthy, right],
                UnionReduction::Subtype,
                AliasArg::None,
                None,
            )
        }
        Kind::QuestionQuestionToken
            if context.checker.has_type_facts(left, TypeFacts::EQUndefinedOrNull) =>
        {
            let nonnullable = context.checker.get_non_nullable_type(left);
            context.checker.get_union_type_ex(
                &[nonnullable, right],
                UnionReduction::Subtype,
                AliasArg::None,
                None,
            )
        }
        _ => left,
    }
}
fn in_narrowing_assignment(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    mut uncast: P<Type>,
    mut cast: P<Type>,
) -> bool {
    let mut in_literal = false;
    let mut path: Vec<String> = Vec::new();
    let mut path_known = true;
    for current in std::iter::successors(Some(node), |current| current.parent()) {
        let Some(parent) = current.parent() else {
            break;
        };
        if !in_literal
            && ast::is_logical_or_coalescing_binary_expression(parent)
            && parent.as_binary_expression().right() == current
        {
            let binary = parent.as_binary_expression();
            let left = context.checker.get_type_at_location(binary.left);
            uncast = logical_result_type(context, binary.operator_token.kind(), left, uncast);
            cast = logical_result_type(context, binary.operator_token.kind(), left, cast);
        }
        if path_known
            && parent.kind() == Kind::ConditionalExpression
            && parent.as_conditional_expression().condition != current
        {
            let conditional = parent.as_conditional_expression();
            let other = if conditional.when_true == current {
                conditional.when_false
            } else {
                conditional.when_true
            };
            let mut other_type = Some(context.checker.get_type_at_location(other));
            for name in path.iter().rev() {
                other_type = other_type.and_then(|ty| {
                    context.checker.get_type_of_property_of_type(ty, name).or_else(|| {
                        name.parse::<i64>().ok()?;
                        context.checker.get_index_type_of_type(ty, context.checker.number_type)
                    })
                });
            }
            if let Some(other) = other_type {
                uncast = context.checker.get_union_type_ex(
                    &[uncast, other],
                    UnionReduction::Subtype,
                    AliasArg::None,
                    None,
                );
                cast = context.checker.get_union_type_ex(
                    &[cast, other],
                    UnionReduction::Subtype,
                    AliasArg::None,
                    None,
                );
            }
        }
        if !in_literal
            && parent.kind() == Kind::BinaryExpression
            && parent.as_binary_expression().left == current
            && matches!(
                parent.as_binary_expression().operator_token.kind(),
                Kind::BarBarToken | Kind::QuestionQuestionToken
            )
        {
            let uncast_nonnullable = context.checker.get_non_nullable_type(uncast);
            let cast_nonnullable = context.checker.get_non_nullable_type(cast);
            if context.checker.is_type_identical_to(uncast_nonnullable, cast_nonnullable) {
                return false;
            }
        }
        if parent.kind() == Kind::ParenthesizedExpression
            || ast::is_logical_or_coalescing_binary_expression(parent)
            || parent.kind() == Kind::ConditionalExpression
                && parent.as_conditional_expression().condition != current
            || parent.kind() == Kind::BinaryExpression
                && parent.as_binary_expression().operator_token.kind() == Kind::CommaToken
                && parent.as_binary_expression().right() == current
        {
            continue;
        }
        if matches!(
            parent.kind(),
            Kind::ArrayLiteralExpression | Kind::ObjectLiteralExpression | Kind::SpreadAssignment
        ) || parent.kind() == Kind::PropertyAssignment && parent.initializer() == Some(current)
        {
            in_literal = true;
            match parent.kind() {
                Kind::PropertyAssignment => {
                    let name = parent.name().and_then(ast::try_get_text_of_property_name);
                    path_known &= name.is_some();
                    path.push(name.unwrap_or_default());
                }
                Kind::ArrayLiteralExpression => {
                    let elements = parent.as_array_literal_expression().elements.nodes();
                    let index = elements.iter().position(|&node| node == current);
                    path_known &= index.is_some_and(|index| {
                        !elements[..index].iter().any(|node| node.kind() == Kind::SpreadElement)
                    });
                    path.push(index.map_or_else(|| "-1".into(), |index| index.to_string()));
                }
                Kind::ObjectLiteralExpression if path_known && !path.is_empty() => {
                    let properties = parent.properties();
                    let index = properties.iter().position(|&node| node == current).unwrap();
                    for &property in &properties[index + 1..] {
                        if property.kind() == Kind::SpreadAssignment {
                            let spread = context
                                .checker
                                .get_type_at_location(property.expression().unwrap());
                            if let Some(overwriting) =
                                context.checker.get_property_of_type(spread, path.last().unwrap())
                                && !overwriting.flags().intersects(SymbolFlags::Optional)
                                && !overwriting.check_flags().intersects(CheckFlags::Partial)
                                && !checker::get_declaration_modifier_flags_from_symbol_exported(
                                    overwriting,
                                )
                                .intersects(ModifierFlags::Private | ModifierFlags::Protected)
                                && checker::Checker::is_spreadable_property(overwriting)
                            {
                                return false;
                            }
                            continue;
                        }
                        if property
                            .name()
                            .and_then(ast::try_get_text_of_property_name)
                            .is_some_and(|name| name == *path.last().unwrap())
                        {
                            return false;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }
        if !ast::is_assignment_expression(parent, true)
            || parent.as_binary_expression().right() != current
        {
            return false;
        }
        let left = ast::skip_parentheses(parent.as_binary_expression().left);
        let receiver = if in_literal {
            if !ast::is_assignment_pattern(left) {
                return false;
            }
            let receiver = context.checker.get_contextual_type(node, ContextFlags::None);
            if path_known && let Some(initializer) = destructuring_default(left, &path) {
                uncast = context.checker.get_type_with_default(uncast, Some(initializer));
                cast = context.checker.get_type_with_default(cast, Some(initializer));
            }
            receiver
        } else {
            Some(context.checker.get_type_at_location(left))
        };
        let Some(mut receiver) = receiver else {
            return false;
        };
        if let Some(constraint) = context.checker.get_base_constraint_of_type(receiver) {
            receiver = constraint;
        }
        if !receiver.flags().intersects(TypeFlags::Union) {
            return false;
        }
        let uncast_reduced = context.checker.get_assignment_reduced_type(receiver, uncast);
        let cast_reduced = context.checker.get_assignment_reduced_type(receiver, cast);
        return !context.checker.is_type_identical_to(uncast_reduced, cast_reduced);
    }
    false
}
fn type_unchanged(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    expression: P<Node>,
    uncast: P<Type>,
    cast: P<Type>,
) -> bool {
    let expression = ast::skip_parentheses(expression);
    if uncast == cast {
        return true;
    }
    if (has_enum_type(uncast) || has_enum_type(cast))
        && !context.checker.is_type_identical_to(uncast, cast)
    {
        return false;
    }
    if uncast.flags().intersects(TypeFlags::TypeParameter) && receiver_of_write_access(node) {
        return false;
    }
    if context.program.options().no_unchecked_indexed_access.is_true()
        && element_access_argument(node)
    {
        return false;
    }
    let type_node = ast::skip_type_parentheses(node.type_node().unwrap());
    if type_node.kind() == Kind::IntersectionType
        && contains_flags(context, cast, TypeFlags::TypeVariable | TypeFlags::Index)
    {
        return false;
    }
    if context.program.options().exact_optional_property_types.is_true()
        && uncast.flags().intersects(TypeFlags::Undefined)
        && cast.flags().intersects(TypeFlags::Undefined)
    {
        return equivalent_ignoring_undefined(uncast, cast);
    }
    if (uncast.flags().intersects(TypeFlags::NonPrimitive)
        && !cast.flags().intersects(TypeFlags::NonPrimitive))
        || (has_index_signature(context, uncast) != has_index_signature(context, cast))
    {
        return false;
    }
    if conceptually_literal(expression)
        && (expression.kind() != Kind::ObjectLiteralExpression
            || expression.properties().is_empty()
            || context.checker.get_properties_of_type(cast).iter().any(|&property| {
                let ty = context.checker.get_type_of_symbol_exported(property);
                is_literal(ty)
            }))
    {
        return false;
    }
    let uncast_arguments = type_arguments(context, uncast);
    let cast_arguments = type_arguments(context, cast);
    if [uncast, cast].iter().any(|ty| ty.flags().intersects(TypeFlags::Any))
        || uncast_arguments
            .iter()
            .chain(cast_arguments)
            .any(|ty| ty.flags().intersects(TypeFlags::Any))
    {
        return false;
    }
    if cast.flags().intersects(TypeFlags::Intersection)
        && !uncast.flags().intersects(TypeFlags::Intersection)
    {
        if contains_flags(context, uncast, TypeFlags::Any)
            || contains_flags(context, cast, TypeFlags::Any)
            || (contains_flags(context, cast, TypeFlags::TypeVariable | TypeFlags::Index)
                && !contains_flags(context, uncast, TypeFlags::TypeVariable | TypeFlags::Index))
        {
            return false;
        }
        let parts = cast.types();
        let other = parts.iter().copied().find(|&part| part != uncast);
        if uncast.flags().intersects(TypeFlags::TypeParameter)
            && parts.len() == 2
            && parts.contains(&uncast)
            && other.is_some_and(|other| {
                empty_object(context, other)
                    && !contains_flags(context, other, TypeFlags::TypeVariable | TypeFlags::Index)
            })
            && let Some(constraint) = context.checker.get_base_constraint_of_type(uncast)
            && !context.checker.is_nullable_type(constraint)
        {
            return true;
        }
        return false;
    }
    same_properties(context, uncast, cast)
        && uncast_arguments == cast_arguments
        && context.checker.is_type_assignable_to(uncast, cast)
        && context.checker.is_type_assignable_to(cast, uncast)
        && !contains_flags(context, uncast, TypeFlags::Any)
        && !contains_flags(context, cast, TypeFlags::Any)
        && (!contains_flags(context, cast, TypeFlags::TypeVariable | TypeFlags::Index)
            || contains_flags(context, uncast, TypeFlags::TypeVariable | TypeFlags::Index))
}
fn uncast_type(context: &mut RuleContext<'_>, node: P<Node>) -> P<Type> {
    let expression = ast::skip_parentheses(node.expression().unwrap());
    if expression.kind() == Kind::CallExpression {
        let callee = ast::skip_parentheses(expression.as_call_expression().expression);
        if matches!(callee.kind(), Kind::ArrowFunction | Kind::FunctionExpression) {
            let function_type = context.checker.get_type_at_location(callee);
            if let Some(&signature) =
                context.checker.get_signatures_of_type(function_type, SignatureKind::Call).first()
            {
                let return_type = context.checker.get_return_type_of_signature_exported(signature);
                if callee.type_node().is_none()
                    && return_type.flags().intersects(TypeFlags::Undefined)
                {
                    return context.checker.void_type;
                }
                return return_type;
            }
        }
    }
    if context_sensitive_call(expression) {
        return context.checker.get_context_free_type_of_expression(expression);
    }
    context.checker.get_type_at_location(expression)
}
fn argument_index(node: P<Node>) -> Option<usize> {
    let parent = parent_through_parens(node)?;
    if !matches!(parent.kind(), Kind::CallExpression | Kind::NewExpression) {
        return None;
    }
    parent
        .arguments()
        .iter()
        .position(|&argument| argument == node || ast::skip_parentheses(argument) == node)
}
fn has_generic_signature(context: &mut RuleContext<'_>, ty: P<Type>) -> bool {
    context
        .checker
        .get_signatures_of_type(ty, SignatureKind::Call)
        .iter()
        .any(|signature| !signature.type_parameters().is_empty())
}
fn is_generic_call_with_inferred_type_arguments(
    context: &mut RuleContext<'_>,
    mut expression: P<Node>,
) -> bool {
    loop {
        expression = ast::skip_parentheses(expression);
        match expression.kind() {
            Kind::CallExpression => {
                if !expression.type_arguments().is_empty() {
                    return false;
                }
                let callee = context.checker.get_type_at_location(expression.expression().unwrap());
                if !has_generic_signature(context, callee) {
                    return false;
                }
                let mut signature =
                    context.checker.get_resolved_signature(expression, None, CheckMode::Normal);
                while let Some(target) = signature.target() {
                    signature = target;
                }
                return !signature.type_parameters().is_empty();
            }
            Kind::AwaitExpression | Kind::NonNullExpression => {
                expression = expression.expression().unwrap();
            }
            Kind::BinaryExpression => {
                let binary = expression.as_binary_expression();
                if binary.operator_token.kind() != Kind::CommaToken {
                    return false;
                }
                expression = binary.right();
            }
            _ => return false,
        }
    }
}
fn generic_mismatch(context: &mut RuleContext<'_>, uncast: P<Type>, contextual: P<Type>) -> bool {
    context.checker.get_properties_of_type(contextual).iter().any(|&property| {
        let contextual_type = context.checker.get_type_of_symbol_exported(property);
        if !has_generic_signature(context, contextual_type) {
            return false;
        }
        let Some(uncast_property) = context.checker.get_property_of_type(uncast, property.name())
        else {
            return true;
        };
        let uncast_type = context.checker.get_type_of_symbol_exported(uncast_property);
        !has_generic_signature(context, uncast_type)
    })
}
fn argument_to_overloaded_function(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let Some(index) = argument_index(node) else {
        return false;
    };
    let parent = parent_through_parens(node).unwrap();
    let callee_type = context.checker.get_type_at_location(parent.expression().unwrap());
    let callee_type = context.checker.get_non_nullable_type(callee_type);
    let signatures = context.checker.get_signatures_of_type(callee_type, SignatureKind::Call);
    if signatures.len() <= 1 {
        return false;
    }
    let mut types = Vec::with_capacity(signatures.len());
    for &signature in signatures {
        let Some(&parameter) = signature.parameters().get(index) else {
            return true;
        };
        let mut ty = context.checker.get_type_of_symbol_exported(parameter);
        if parameter.value_declaration().is_some_and(|declaration| {
            declaration.kind() == Kind::Parameter
                && declaration.as_parameter_declaration().dot_dot_dot_token().is_some()
        }) && let Some(&first) = type_arguments(context, ty).first()
        {
            ty = first;
        }
        types.push(ty);
    }
    if types.iter().any(|&ty| ty != types[0]) {
        let uncast = context.checker.get_type_at_location(node.expression().unwrap());
        return types.iter().any(|&ty| !context.checker.is_type_assignable_to(uncast, ty));
    }
    false
}
fn in_generic_context(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let mut seen_function = false;
    for current in std::iter::successors(node.parent(), |current| current.parent()) {
        if current.kind() == Kind::FunctionDeclaration {
            return false;
        }
        if matches!(current.kind(), Kind::FunctionExpression | Kind::ArrowFunction) {
            if current.body().is_some_and(|body| body.kind() == Kind::Block) || seen_function {
                return false;
            }
            seen_function = true;
        }
        if matches!(current.kind(), Kind::CallExpression | Kind::NewExpression) {
            let arguments = if current.kind() == Kind::CallExpression {
                current.as_call_expression().type_arguments()
            } else {
                current.as_new_expression().type_arguments()
            };
            if arguments.is_some() {
                continue;
            }
            let callee = current.expression().unwrap();
            if current.kind() == Kind::CallExpression
                && matches!(
                    callee.kind(),
                    Kind::PropertyAccessExpression | Kind::ElementAccessExpression
                )
                && current
                    .arguments()
                    .iter()
                    .any(|&argument| ast::skip_parentheses(argument) == node)
            {
                continue;
            }
            let ty = context.checker.get_type_at_location(callee);
            if has_generic_signature(context, ty) {
                return true;
            }
        }
    }
    false
}
fn skip_parent_contextual_any(node: P<Node>) -> bool {
    parent_through_parens(node).is_some_and(|parent| {
        matches!(
            parent.kind(),
            Kind::AsExpression
                | Kind::TypeAssertionExpression
                | Kind::SpreadElement
                | Kind::SpreadAssignment
                | Kind::SatisfiesExpression
        )
    })
}
fn problematic_property_context(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let Some(parent) = parent_through_parens(node).filter(|parent| {
        parent.kind() == Kind::PropertyAssignment
            && parent.initializer().map(ast::skip_parentheses) == Some(node)
    }) else {
        return false;
    };
    let Some(object) =
        parent.parent().filter(|parent| parent.kind() == Kind::ObjectLiteralExpression)
    else {
        return false;
    };
    if context
        .checker
        .get_contextual_type(object, ContextFlags::None)
        .is_some_and(|ty| ty.flags().intersects(TypeFlags::Union))
    {
        let Some(contextual) = context.checker.get_contextual_type(node, ContextFlags::None) else {
            return true;
        };
        let nonnullable = context.checker.get_non_nullable_type(contextual);
        if nonnullable.flags().intersects(TypeFlags::Union) {
            return true;
        }
        let uncast = context.checker.get_type_at_location(node.expression().unwrap());
        return !context.checker.is_type_assignable_to(uncast, nonnullable);
    }
    parent_through_parens(object).is_some_and(|parent| {
        matches!(
            parent.kind(),
            Kind::AsExpression | Kind::TypeAssertionExpression | Kind::SatisfiesExpression
        ) || (parent.kind() == Kind::CallExpression
            && parent.parent().is_some_and(|parent| parent.kind() == Kind::SatisfiesExpression))
    })
}
fn skip_contextual_fallback(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    cast_any: bool,
    uncast: P<Type>,
    cast: P<Type>,
) -> bool {
    let parent = parent_through_parens(node);
    if in_narrowing_assignment(context, node, uncast, cast) {
        return true;
    }
    if cast_any {
        return parent.is_some_and(ast::is_logical_expression)
            || in_generic_context(context, node)
            || problematic_property_context(context, node);
    }
    if ast::skip_parentheses(node.expression().unwrap()).kind() == Kind::TemplateExpression {
        return true;
    }
    let destructuring = parent.is_some_and(|parent| {
        parent.kind() == Kind::VariableDeclaration
            && parent.initializer().map(ast::skip_parentheses) == Some(node)
            && parent.name().is_some_and(ast::is_binding_pattern)
    });
    let nonstatement_assignment = parent.is_some_and(|parent| {
        ast::is_assignment_expression(parent, false)
            && ast::skip_parentheses(parent.as_binary_expression().right()) == node
            && parent_through_parens(parent)
                .is_none_or(|parent| parent.kind() != Kind::ExpressionStatement)
    });
    let logical_assignment = parent.is_some_and(|parent| {
        parent.kind() == Kind::BinaryExpression
            && ast::skip_parentheses(parent.as_binary_expression().right()) == node
            && ast::is_logical_or_coalescing_assignment_operator(
                parent.as_binary_expression().operator_token.kind(),
            )
    });
    if skip_parent_contextual_any(node)
        || ast::skip_parentheses(node.expression().unwrap()).kind() == Kind::ArrayLiteralExpression
        || nested_array_generic_argument(context, node)
        || destructuring
        || problematic_property_context(context, node)
        || property_in_inferred_callback_return(context, node)
        || nonstatement_assignment
        || logical_assignment
        || argument_to_overloaded_function(context, node)
    {
        return true;
    }
    in_generic_context(context, node)
        && !conceptually_literal(original_expression(node))
        && parent.is_none_or(|parent| parent.kind() != Kind::PropertyAssignment)
}

fn assertion_fixes(context: &RuleContext<'_>, node: P<Node>) -> Vec<Fix> {
    if let Some(mut range) = jsdoc_assertion_range(context, node) {
        while range.end < pos(node.expression().unwrap())
            && utils::is_str_white_space(char::from(
                context.source_file.text().as_bytes()[range.end as usize],
            ))
        {
            range.end += 1;
        }
        return vec![remove(range)];
    }
    let expression = node.expression().unwrap();
    if node.kind() == Kind::AsExpression {
        let keyword = token_range(context, end(expression));
        let token_before_as =
            tsrs_scanner::get_trailing_comment_ranges(context.source_file.text(), end(expression))
                .take_while(|comment| comment.pos() < keyword.pos)
                .chain(
                    tsrs_scanner::get_leading_comment_ranges(
                        context.source_file.text(),
                        end(expression),
                    )
                    .take_while(|comment| comment.pos() < keyword.pos),
                )
                .map(|comment| comment.end())
                .max()
                .unwrap_or(end(expression));
        return vec![remove(Range { pos: token_before_as, end: end(node) })];
    }
    let range = angle_range(context, node);
    let mut scanner = tsrs_scanner::get_scanner_for_source_file(context.source_file, range.end);
    let first = scanner.token();
    let statement_first = if first == Kind::AsyncKeyword
        && scanner.scan() == Kind::FunctionKeyword
        && !scanner.has_preceding_line_break()
    {
        Kind::FunctionKeyword
    } else {
        first
    };
    let parens = utils::is_start_of_expression_statement_needing_parentheses(
        context.source_file,
        node,
        statement_first,
    ) || utils::is_start_of_arrow_function_body_needing_parentheses(
        context.source_file,
        node,
        first,
    );
    let mut fixes = Vec::new();
    if parens {
        let start = trim_node_range(context.source_file, node).pos;
        fixes.push(Fix { range: Range { pos: start, end: start }, text: "(".into() });
    }
    fixes.push(remove(range));
    if parens {
        fixes.push(Fix { range: Range { pos: end(node), end: end(node) }, text: ")".into() });
    }
    fixes
}
fn angle_range(context: &RuleContext<'_>, node: P<Node>) -> Range {
    let mut scanner = tsrs_scanner::get_scanner_for_source_file(context.source_file, pos(node));
    let start = scanner.token_range().source_pos().unwrap();
    scanner.reset_pos(end(node.type_node().unwrap()));
    scanner.scan();
    Range { pos: start, end: scanner.token_range().source_end().unwrap() }
}
fn contextual_diagnostic(range: Range) -> Diagnostic {
    Diagnostic {
        range: Some(range),
        message: RuleMessage { id: "contextuallyUnnecessary".into(), description: "This assertion is unnecessary since the receiver accepts the original type of the expression.".into(), ..Default::default() },
        ..Default::default()
    }
}
fn unnecessary_diagnostic(range: Range) -> Diagnostic {
    Diagnostic {
        range: Some(range),
        message: RuleMessage {
            id: "unnecessaryAssertion".into(),
            description:
                "This assertion is unnecessary since it does not change the type of the expression."
                    .into(),
            ..Default::default()
        },
        ..Default::default()
    }
}
fn report_with_fixes(
    context: &mut RuleContext<'_>,
    mut diagnostic: Diagnostic,
    fixes: impl FnOnce() -> Vec<Fix>,
) {
    if context.options.fix {
        diagnostic.fixes = fixes();
    }
    context.report_diagnostic(diagnostic);
}
fn jsdoc_assertion_range(context: &RuleContext<'_>, node: P<Node>) -> Option<Range> {
    if !ast::is_in_js_file(node) || pos(node.type_node()?) >= pos(node.expression()?) {
        return None;
    }
    let start = node
        .parent()
        .filter(|parent| parent.kind() == Kind::ParenthesizedExpression)
        .map_or(pos(node), pos);
    let before = &context.source_file.text()[start as usize..pos(node.expression()?) as usize];
    let (begin, end) = (before.rfind("/**")?, before.rfind("*/")?);
    (end >= begin).then_some(Range { pos: start + begin as u32, end: start + end as u32 + 2 })
}
fn assertion_range(context: &RuleContext<'_>, node: P<Node>) -> Range {
    jsdoc_assertion_range(context, node)
        .unwrap_or_else(|| trim_node_range(context.source_file, node))
}
fn report_unnecessary(context: &mut RuleContext<'_>, node: P<Node>) {
    let fixes = if context.options.fix { assertion_fixes(context, node) } else { Vec::new() };
    context.report_diagnostic(Diagnostic {
        fixes,
        ..unnecessary_diagnostic(assertion_range(context, node))
    });
}
fn report_double_assertion(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    contextual: Option<P<Type>>,
) -> bool {
    let inner = ast::skip_parentheses(node.expression().unwrap());
    if !matches!(inner.kind(), Kind::AsExpression | Kind::TypeAssertionExpression) {
        return false;
    }
    let original = original_expression(node);
    let original_type = context.checker.get_type_at_location(original);
    let cast_type = context.checker.get_type_at_location(node);
    let unnecessary = type_unchanged(context, node, inner, original_type, cast_type)
        && !cast_type.flags().intersects(TypeFlags::Any);
    let unrelated_parameters = original_type != cast_type
        && original_type.flags().intersects(TypeFlags::TypeParameter)
        && cast_type.flags().intersects(TypeFlags::TypeParameter)
        && !constrained_to(context, original_type, cast_type, &mut FxHashSet::default());
    let contextual_unnecessary = if !unnecessary && !unrelated_parameters {
        contextual.is_some_and(|contextual| {
            let intermediate = context.checker.get_type_at_location(inner);
            intermediate.flags().intersects(TypeFlags::Any | TypeFlags::Unknown)
                && context.checker.is_type_assignable_to(original_type, contextual)
        })
    } else {
        false
    };
    if !unnecessary && !contextual_unnecessary {
        return false;
    }
    let mut diagnostic = contextual_diagnostic(assertion_range(context, node));
    if unnecessary {
        diagnostic.message = unnecessary_diagnostic(assertion_range(context, node)).message;
    }
    let fixes = if context.options.fix {
        let range = trim_node_range(context.source_file, original);
        let mut text =
            context.source_file.text()[range.pos as usize..range.end as usize].to_string();
        if original.kind() == Kind::ObjectLiteralExpression
            && node.parent().is_some_and(|parent| {
                parent.kind() == Kind::ArrowFunction && parent.body() == Some(node)
            })
        {
            text = format!("({text})");
        }
        vec![replace_node(context.source_file, node, text)]
    } else {
        Vec::new()
    };
    context.report_diagnostic(Diagnostic { fixes, ..diagnostic });
    true
}
fn possibly_used_before_assigned(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let declaration = context
        .checker
        .get_symbol_at_location_exported(node)
        .and_then(|symbol| symbol.declarations().first().copied());
    let Some(declaration) = declaration else {
        return true;
    };
    let options = context.program.options();
    if !options.get_strict_option_value(options.strict_null_checks)
        || declaration.kind() != Kind::VariableDeclaration
    {
        return false;
    }
    if declaration.parent().is_some_and(|parent| {
        parent.kind() == Kind::VariableDeclarationList && parent.flags() == NodeFlags::None
    }) {
        let scope = ast::get_enclosing_block_scope_container(node);
        let mut parent_scope = ast::get_enclosing_block_scope_container(declaration);
        while let Some(current) = parent_scope {
            parent_scope = ast::get_enclosing_block_scope_container(current);
            if parent_scope.is_none() {
                break;
            }
            if parent_scope == scope {
                return true;
            }
        }
    }
    let decl = declaration.as_variable_declaration();
    if decl.initializer().is_none()
        && decl.exclamation_token().is_none()
        && let Some(type_node) = decl.type_()
    {
        let declared_type = context.checker.get_type_from_type_node_exported(type_node);
        let ty = utils::get_constrained_type_at_location(context.checker, node);
        let declared = declaration
            .parent()
            .filter(|parent| parent.kind() == Kind::VariableDeclarationList)
            .and_then(|parent| parent.parent())
            .is_some_and(|parent| {
                parent.kind() == Kind::VariableStatement
                    && parent.modifiers().is_some_and(|modifiers| {
                        modifiers
                            .nodes()
                            .iter()
                            .any(|modifier| modifier.kind() == Kind::DeclareKeyword)
                    })
            });
        if declared_type == ty && !declared {
            return true;
        }
    }
    false
}
fn nullable_for_nonnull(context: &mut RuleContext<'_>, ty: P<Type>) -> bool {
    context.checker.is_nullable_type(ty)
        || utils::union_type_parts(&ty).iter().any(|part| {
            part.flags().intersects(TypeFlags::Any | TypeFlags::Unknown | TypeFlags::Void)
        })
}

fn check_nonnull(context: &mut RuleContext<'_>, node: P<Node>) {
    let expression = ast::skip_parentheses(node.expression().unwrap());
    if parent_through_parens(node).is_some_and(|parent| ast::is_assignment_expression(parent, true))
    {
        if ast::skip_parentheses(parent_through_parens(node).unwrap().as_binary_expression().left)
            == node
        {
            let range = token_range(context, end(node.expression().unwrap()));
            report_with_fixes(
                context,
                contextual_diagnostic(assertion_range(context, node)),
                || vec![remove(range)],
            );
        }
        return;
    }
    let constrained = utils::get_constrained_type_at_location(context.checker, expression);
    let actual = context.checker.get_type_at_location(expression);
    let constrained_nullable = nullable_for_nonnull(context, constrained);
    let actual_nullable = nullable_for_nonnull(context, actual);
    if !constrained_nullable && !actual_nullable {
        if expression.kind() == Kind::Identifier
            && possibly_used_before_assigned(context, expression)
        {
            return;
        }
        let range = token_range(context, end(node.expression().unwrap()));
        let diagnostic = unnecessary_diagnostic(assertion_range(context, node));
        report_with_fixes(context, diagnostic, || vec![remove(range)]);
    } else {
        if constrained != actual {
            return;
        }
        let flags = utils::union_type_parts(&constrained)
            .iter()
            .fold(TypeFlags::None, |flags, part| flags | part.flags());
        if let Some(contextual) = utils::get_contextual_type(context.checker, node) {
            let contextual_flags = utils::union_type_parts(&contextual)
                .iter()
                .fold(TypeFlags::None, |flags, part| flags | part.flags());
            if flags.intersects(TypeFlags::Unknown)
                && !contextual_flags.intersects(TypeFlags::Unknown)
            {
                return;
            }
            if [TypeFlags::Undefined, TypeFlags::Null, TypeFlags::Void]
                .iter()
                .all(|&flag| !flags.intersects(flag) || contextual_flags.intersects(flag))
            {
                let range = token_range(context, end(node.expression().unwrap()));
                report_with_fixes(
                    context,
                    contextual_diagnostic(assertion_range(context, node)),
                    || vec![remove(range)],
                );
            }
        }
    }
}

impl NoUnnecessaryTypeAssertion {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("no-unnecessary-type-assertion: options must be an object".into()),
        };
        let check_literal_const_assertions =
            match object.and_then(|object| object.get("checkLiteralConstAssertions")) {
                None | Some(Value::Null) => false,
                Some(Value::Bool(value)) => *value,
                _ => return Err(
                    "no-unnecessary-type-assertion: checkLiteralConstAssertions must be a boolean"
                        .into(),
                ),
            };
        let types_to_ignore = match object.and_then(|object| object.get("typesToIgnore")) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(values)) => values
                .iter()
                .map(|value| match value {
                    Value::String(value) => Ok(value.clone()),
                    Value::Null => Ok(String::new()),
                    _ => {
                        Err("no-unnecessary-type-assertion: typesToIgnore must contain strings"
                            .into())
                    }
                })
                .collect::<Result<_, String>>()?,
            _ => {
                return Err("no-unnecessary-type-assertion: typesToIgnore must be an array".into());
            }
        };
        Ok(Self { check_literal_const_assertions, types_to_ignore })
    }

    fn check_assertion(&self, context: &mut RuleContext<'_>, node: P<Node>) {
        let type_node = ast::skip_type_parentheses(node.type_node().unwrap());
        let type_range = trim_node_range(context.source_file, type_node);
        let type_text =
            &context.source_file.text()[type_range.pos as usize..type_range.end as usize];
        if self.types_to_ignore.iter().any(|ignore| ignore == type_text) {
            return;
        }
        let cast = context.checker.get_type_at_location(node);
        let cast_literal = is_literal(cast);
        let const_assertion = is_const_assertion(type_node);
        if !self.check_literal_const_assertions && cast_literal && const_assertion {
            return;
        }
        let expression = node.expression().unwrap();
        let generic_call_with_inferred_type_arguments =
            is_generic_call_with_inferred_type_arguments(context, expression);
        let mut uncast = if generic_call_with_inferred_type_arguments {
            context.checker.get_type_at_location(expression)
        } else {
            uncast_type(context, node)
        };
        let expression_for_type = ast::skip_parentheses(expression);
        if uncast == cast
            && expression_for_type.kind() == Kind::Identifier
            && let Some(symbol) =
                context.checker.get_symbol_at_location_exported(expression_for_type)
        {
            let symbol_type = context.checker.get_type_of_symbol_exported(symbol);
            if symbol_type.flags().intersects(TypeFlags::Conditional) {
                uncast = symbol_type;
            }
        }
        let unchanged = type_unchanged(context, node, expression, uncast, cast);
        let same_inferred = if cast_literal {
            ast::skip_parentheses(expression).kind() != Kind::TemplateExpression
                && parent_through_parens(node).is_some_and(|parent| {
                    (parent.kind() == Kind::VariableDeclaration
                        && parent.parent().is_some_and(|parent| {
                            parent.kind() == Kind::VariableDeclarationList
                                && parent.flags().intersects(NodeFlags::Const)
                        }))
                        || (parent.kind() == Kind::PropertyDeclaration
                            && parent.modifier_flags().intersects(ModifierFlags::Readonly))
                })
        } else {
            !const_assertion
        };
        if unchanged && same_inferred && generic_call_with_inferred_type_arguments {
            context.report_diagnostic(Diagnostic {
                range: Some(assertion_range(context, node)),
                message: RuleMessage {
                    id: "contextuallyInferredTypeArguments".into(),
                    description: "The type arguments for this generic call may be inferred from the assertion. Specify them explicitly instead.".into(),
                    ..Default::default()
                },
                ..Default::default()
            });
            return;
        }
        if unchanged && same_inferred {
            report_unnecessary(context, node);
            return;
        }
        if generic_call_with_inferred_type_arguments {
            return;
        }
        let cast_any = cast.flags().intersects(TypeFlags::Any) && !skip_parent_contextual_any(node);
        let contextual = if skip_contextual_fallback(context, node, cast_any, uncast, cast) {
            None
        } else {
            context.checker.get_contextual_type(node, ContextFlags::None)
        };
        if let Some(contextual) = contextual {
            let contextual_any = contextual.flags().intersects(TypeFlags::Any);
            let any_check = (!contextual_any
                && !contains_flags(context, contextual, TypeFlags::Any))
                || (contextual_any
                    && argument_index(node).is_some()
                    && !contains_flags(context, cast, TypeFlags::Any));
            let original = ast::skip_parentheses(expression);
            let nullish_union = cast.flags().intersects(TypeFlags::Union)
                && (original.kind() == Kind::NullKeyword
                    || (original.kind() == Kind::Identifier && original.text() == "undefined"));
            if !const_assertion
                && !contains_flags(context, uncast, TypeFlags::Any)
                && any_check
                && !(in_generic_context(context, node)
                    && (phantom_arguments(context, uncast)
                        || phantom_arguments(context, contextual))
                    && !same_arguments(context, uncast, contextual))
                && (cast_any || !generic_mismatch(context, uncast, contextual))
                && (contextual_any || context.checker.is_type_assignable_to(uncast, contextual))
                && !nullish_union
            {
                let fixes =
                    if context.options.fix { assertion_fixes(context, node) } else { Vec::new() };
                context.report_diagnostic(Diagnostic {
                    fixes,
                    ..contextual_diagnostic(assertion_range(context, node))
                });
                return;
            }
        }
        report_double_assertion(context, node, contextual);
    }
}
impl Rule for NoUnnecessaryTypeAssertion {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::AsExpression | Kind::TypeAssertionExpression | Kind::NonNullExpression
            )
        )
    }
    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match listener {
            Listener::Enter(Kind::NonNullExpression) => check_nonnull(context, node),
            Listener::Enter(Kind::AsExpression | Kind::TypeAssertionExpression) => {
                self.check_assertion(context, node);
            }
            _ => unreachable!("no-unnecessary-type-assertion listener"),
        }
    }
}
