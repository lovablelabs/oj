//! Port of tsgolint's `internal/rules/no_useless_default_assignment/no_useless_default_assignment.go`.

use tsrs_ast::{self as ast, Kind, Node, SymbolFlags};
use tsrs_checker::{ElementFlags, SignatureKind, Type, TypeFlags};
use tsrs_core::P;

use crate::protocol::{Diagnostic, Fix, LabeledRange, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils;

pub struct NoUselessDefaultAssignment;

#[derive(Clone, Copy)]
enum MessageId {
    NoStrictNullCheck,
    PreferOptionalSyntax,
    UselessDefaultAssignment,
    RemoveDefaultAssignment,
    UseOptionalSyntax,
    UselessUndefined,
}

impl MessageId {
    fn message(self, assignment: &str, type_text: &str) -> RuleMessage {
        let (id, description, help) = match self {
            Self::NoStrictNullCheck => ("noStrictNullCheck", "This rule requires the `strictNullChecks` compiler option to be turned on to function correctly.".into(), ""),
            Self::PreferOptionalSyntax => ("preferOptionalSyntax", "Using `= undefined` to make a parameter optional adds unnecessary runtime logic.".into(), ""),
            Self::UselessDefaultAssignment if type_text.is_empty() => ("uselessDefaultAssignment", format!("Default value is useless because the {assignment} is not nullish. This default assignment will never be used."), "Remove the default assignment"),
            Self::UselessDefaultAssignment => ("uselessDefaultAssignment", format!("Default value is useless because the {assignment} has type `{type_text}` (not nullish). This default assignment will never be used."), "Remove the default assignment"),
            Self::RemoveDefaultAssignment => ("removeDefaultAssignment", "Remove the default value.".into(), ""),
            Self::UseOptionalSyntax => ("useOptionalSyntax", "Use the `?` optional syntax instead.".into(), ""),
            Self::UselessUndefined => {
                let plural = match assignment { "property" => "properties".into(), "parameter" => "parameters".into(), _ => format!("{assignment}s") };
                ("uselessUndefined", format!("Default value is useless because it is undefined. Optional {plural} are already undefined by default."), "Remove the default assignment")
            }
        };
        RuleMessage { id: id.into(), description, help: help.into() }
    }
}

fn can_be_undefined(ty: P<Type>) -> bool {
    ty.flags().intersects(TypeFlags::Any | TypeFlags::Unknown)
        || utils::union_type_parts(&ty)
            .iter()
            .any(|part| part.flags().intersects(TypeFlags::Undefined))
}

fn property_name(mut node: P<Node>) -> Option<&'static str> {
    if node.kind() == Kind::ComputedPropertyName {
        node = node.as_computed_property_name().expression;
    }
    if node.kind() == Kind::Identifier
        || ast::is_literal_expression(node)
        || node.kind() == Kind::NoSubstitutionTemplateLiteral
    {
        Some(node.text())
    } else {
        None
    }
}

fn array_element_type(context: &mut RuleContext<'_>, ty: P<Type>, index: usize) -> Option<P<Type>> {
    if ty.is_tuple_type()
        && let Some(&argument) = context.checker.get_type_arguments(ty).get(index)
    {
        return Some(argument);
    }
    context.checker.get_number_index_type(ty)
}

fn property_in_all_branches(expression: P<Node>, name: &str) -> bool {
    let expression = ast::skip_parentheses(expression);
    match expression.kind() {
        Kind::ObjectLiteralExpression => expression.properties().iter().any(|property| {
            property.kind() != Kind::SpreadAssignment
                && property.name().and_then(property_name) == Some(name)
        }),
        Kind::ConditionalExpression => {
            let conditional = expression.as_conditional_expression();
            property_in_all_branches(conditional.when_true, name)
                && property_in_all_branches(conditional.when_false, name)
        }
        _ => false,
    }
}

fn binding_element_type(context: &mut RuleContext<'_>, element: P<Node>) -> Option<P<Type>> {
    if element.kind() != Kind::BindingElement {
        return None;
    }
    let pattern = element.parent()?;
    match pattern.kind() {
        Kind::ObjectBindingPattern => {
            let ty = pattern_source_type(context, pattern)?;
            let binding = element.as_binding_element();
            let name = property_name(binding.property_name().or_else(|| binding.name())?)?;
            let symbol = context.checker.get_property_of_type(ty, name)?;
            if symbol.flags().intersects(SymbolFlags::Optional) {
                let parent =
                    pattern.parent().filter(|parent| parent.kind() == Kind::VariableDeclaration)?;
                if !property_in_all_branches(parent.initializer()?, name) {
                    return None;
                }
            }
            context.checker.get_type_of_symbol_at_location(symbol, Some(element))
        }
        Kind::ArrayBindingPattern => {
            let ty = pattern_source_type(context, pattern)?;
            let index = pattern.elements().iter().position(|&node| node == element)?;
            array_element_type(context, ty, index)
        }
        _ => None,
    }
}

fn pattern_source_type(context: &mut RuleContext<'_>, pattern: P<Node>) -> Option<P<Type>> {
    let parent = pattern.parent()?;
    match parent.kind() {
        Kind::VariableDeclaration if parent.initializer().is_some() => {
            Some(context.checker.get_type_at_location(parent.initializer().unwrap()))
        }
        Kind::Parameter => {
            let function = parent.parent().filter(|&parent| ast::is_function_like(parent))?;
            let signature = context.checker.get_signature_from_declaration_exported(function);
            let mut index =
                function.parameters().iter().position(|&parameter| parameter == parent)?;
            if signature.this_parameter().is_some()
                && function
                    .parameters()
                    .first()
                    .and_then(|parameter| parameter.name())
                    .is_some_and(|name| name.kind() == Kind::Identifier && name.text() == "this")
            {
                index = index.checked_sub(1)?;
            }
            signature
                .parameters()
                .get(index)
                .map(|&symbol| context.checker.get_type_of_symbol_exported(symbol))
        }
        Kind::BindingElement => binding_element_type(context, parent),
        Kind::ArrayBindingPattern => {
            let ty = pattern_source_type(context, parent)?;
            let index = parent.elements().iter().position(|&node| node == pattern)?;
            array_element_type(context, ty, index)
        }
        _ => None,
    }
}

fn remove_default(node: P<Node>) -> Fix {
    let target = if node.kind() == Kind::Parameter {
        node.type_node().or_else(|| node.name())
    } else if node.kind() == Kind::BindingElement {
        node.name()
    } else {
        None
    };
    Fix {
        range: Range {
            pos: target
                .map_or_else(|| node.pos(), |target| target.end())
                .expect_source("parsed default assignment"),
            end: node.end().expect_source("parsed default assignment"),
        },
        text: String::new(),
    }
}

fn simple_type(ty: P<Type>) -> bool {
    !ty.flags().intersects(TypeFlags::Union | TypeFlags::Intersection)
        && (ty.flags().intersects(
            TypeFlags::StringLike
                | TypeFlags::NumberLike
                | TypeFlags::BooleanLike
                | TypeFlags::BigIntLike
                | TypeFlags::ESSymbolLike,
        ) || ty.symbol().is_some_and(|symbol| !symbol.name().is_empty()))
}

fn report_default(context: &mut RuleContext<'_>, node: P<Node>, assignment: &str, ty: P<Type>) {
    let Some(initializer) = node.initializer() else {
        return;
    };
    let type_text =
        if simple_type(ty) { context.checker.type_to_string_exported(ty) } else { String::new() };
    let range = trim_node_range(context.source_file, initializer);
    let mut diagnostic = Diagnostic {
        range: Some(range),
        message: MessageId::UselessDefaultAssignment.message(assignment, &type_text),
        labeled_ranges: vec![LabeledRange { label: "Default value".into(), range }],
        ..Default::default()
    };
    if !type_text.is_empty() {
        let target = if node.kind() == Kind::Parameter {
            node.type_node().or_else(|| node.name())
        } else {
            node.name()
        };
        if let Some(target) = target {
            diagnostic.labeled_ranges.push(LabeledRange {
                label: format!("{assignment} type `{type_text}` is not nullish"),
                range: trim_node_range(context.source_file, target),
            });
        }
    }
    if context.options.fix_suggestions {
        diagnostic.suggestions.push(Suggestion {
            message: MessageId::RemoveDefaultAssignment.message("", ""),
            fixes: vec![remove_default(node)],
        });
    }
    context.report_diagnostic(diagnostic);
}

fn report_undefined(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    assignment: &str,
    optional: bool,
) {
    let Some(initializer) = node.initializer() else {
        return;
    };
    let file = context.source_file;
    context.report_node_with_suggestions(
        initializer,
        if optional { MessageId::PreferOptionalSyntax } else { MessageId::UselessUndefined }
            .message(assignment, ""),
        || {
            let mut fixes = vec![remove_default(node)];
            if optional
                && let Some(name) = node.name().filter(|name| name.kind() == Kind::Identifier)
            {
                let end = trim_node_range(file, name).end;
                fixes.push(Fix { range: Range { pos: end, end }, text: "?".into() });
            }
            vec![Suggestion {
                message: if optional {
                    MessageId::UseOptionalSyntax
                } else {
                    MessageId::RemoveDefaultAssignment
                }
                .message("", ""),
                fixes,
            }]
        },
    );
}

fn check_function_parameter(context: &mut RuleContext<'_>, node: P<Node>) {
    let Some(parent) = node
        .parent()
        .filter(|parent| matches!(parent.kind(), Kind::ArrowFunction | Kind::FunctionExpression))
    else {
        return;
    };
    let Some(index) = parent.parameters().iter().position(|&parameter| parameter == node) else {
        return;
    };
    let Some(contextual) =
        context.checker.get_contextual_type(parent, tsrs_checker::ContextFlags::None)
    else {
        return;
    };
    let signatures = context.checker.get_signatures_of_type(contextual, SignatureKind::Call);
    let Some(&signature) =
        signatures.first().filter(|signature| signature.declaration() != Some(parent))
    else {
        return;
    };
    let Some(&symbol) = signature.parameters().get(index) else {
        return;
    };
    if symbol.value_declaration().is_some_and(|declaration| {
        declaration.kind() == Kind::Parameter
            && declaration.as_parameter_declaration().dot_dot_dot_token().is_some()
    }) {
        return;
    }
    if !symbol.flags().intersects(SymbolFlags::Optional) {
        let ty = context.checker.get_type_of_symbol_exported(symbol);
        if !ty.flags().intersects(TypeFlags::TypeParameter) && !can_be_undefined(ty) {
            report_default(context, node, "parameter", ty);
        }
    }
}

fn check_assignment(context: &mut RuleContext<'_>, node: P<Node>) {
    let Some(initializer) = node.initializer() else {
        return;
    };
    let initializer = ast::skip_parentheses(initializer);
    if initializer.kind() == Kind::Identifier && initializer.text() == "undefined" {
        if node.kind() == Kind::Parameter {
            let optional = node.type_node().is_some_and(|type_node| {
                let ty = context.checker.get_type_from_type_node_exported(type_node);
                can_be_undefined(ty)
            });
            report_undefined(context, node, "parameter", optional);
        } else {
            report_undefined(context, node, "property", false);
        }
        return;
    }
    if node.kind() == Kind::Parameter {
        check_function_parameter(context, node);
        return;
    }
    if node.kind() != Kind::BindingElement {
        return;
    }
    let Some(parent) = node.parent() else {
        return;
    };
    match parent.kind() {
        Kind::ObjectBindingPattern => {
            if let Some(ty) = binding_element_type(context, node)
                && !can_be_undefined(ty)
            {
                report_default(context, node, "property", ty);
            }
        }
        Kind::ArrayBindingPattern => {
            let Some(ty) = pattern_source_type(context, parent).filter(|ty| ty.is_tuple_type())
            else {
                return;
            };
            let arguments = context.checker.get_type_arguments(ty);
            let Some(index) = parent.elements().iter().position(|&element| element == node) else {
                return;
            };
            if index >= arguments.len() {
                return;
            }
            let target = ty.target().unwrap().as_tuple_type();
            if index >= target.min_length() {
                return;
            }
            let mut element_types = &arguments[index..=index];
            if index >= target.fixed_length() {
                let mut end = target.fixed_length();
                let mut position = end;
                let element_infos = target.element_infos();
                while end < arguments.len() && position <= index {
                    if element_infos[end].tuple_element_flags().intersects(ElementFlags::Required) {
                        position += 1;
                    }
                    end += 1;
                }
                element_types = &arguments[target.fixed_length()..end];
            }
            if !element_types.iter().copied().any(can_be_undefined) {
                let element_type = context.checker.get_union_type(element_types);
                report_default(context, node, "property", element_type);
            }
        }
        _ => {}
    }
}

impl Rule for NoUselessDefaultAssignment {
    fn initialize(&mut self, context: &mut RuleContext<'_>) {
        let options = context.program.options();
        if !options.get_strict_option_value(options.strict_null_checks) {
            context.report_range(
                Range { pos: 0, end: 0 },
                MessageId::NoStrictNullCheck.message("", ""),
            );
        }
    }
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(listener, Listener::Enter(Kind::BindingElement | Kind::Parameter))
    }
    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        check_assignment(context, node);
    }
}
