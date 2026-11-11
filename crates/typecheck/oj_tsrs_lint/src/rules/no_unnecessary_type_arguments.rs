//! Port of tsgolint's `internal/rules/no_unnecessary_type_arguments/no_unnecessary_type_arguments.go`.

use rustc_hash::FxHashSet;
use tsrs_ast::{Kind, Node, NodeList, Symbol, SymbolFlags};
use tsrs_checker::{CheckMode, ObjectFlags, SignatureKind, Type};
use tsrs_core::{P, goslices};

use crate::protocol::{Fix, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext};

pub struct NoUnnecessaryTypeArguments;

fn is_type_context_declaration(node: P<Node>) -> bool {
    matches!(node.kind(), Kind::TypeAliasDeclaration | Kind::InterfaceDeclaration)
}

fn is_in_type_context(node: P<Node>) -> bool {
    node.kind() == Kind::TypeReference
        || node.parent().is_some_and(|parent| {
            matches!(parent.kind(), Kind::InterfaceDeclaration | Kind::TypeReference)
                || (parent.kind() == Kind::HeritageClause
                    && parent.as_heritage_clause().token == Kind::ImplementsKeyword)
        })
}

fn type_parameters_from_type(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    name: P<Node>,
) -> &'static [P<Node>] {
    let Some(mut symbol) = context.checker.get_symbol_at_location_exported(name) else {
        return &[];
    };
    if symbol.flags().intersects(SymbolFlags::Alias) {
        let (resolved, found) = context.checker.resolve_alias_exported(Some(symbol));
        if !found {
            return &[];
        }
        let Some(resolved) = resolved else {
            return &[];
        };
        symbol = resolved;
    }
    let mut declarations = symbol.declarations().to_vec();
    let in_type_context = is_in_type_context(node);
    goslices::sort_func(&mut declarations, |a, b| {
        let (a, b) = if in_type_context { (*a, *b) } else { (*b, *a) };
        i32::from(is_type_context_declaration(b)).cmp(&i32::from(is_type_context_declaration(a)))
    });
    for declaration in declarations {
        if matches!(
            declaration.kind(),
            Kind::TypeAliasDeclaration
                | Kind::InterfaceDeclaration
                | Kind::ClassDeclaration
                | Kind::ClassExpression
        ) {
            return declaration.type_parameters();
        }
        if declaration.kind() == Kind::VariableDeclaration {
            let ty = context.checker.get_type_of_symbol_exported(symbol);
            let signatures = context.checker.get_signatures_of_type(ty, SignatureKind::Construct);
            if let Some(declaration) =
                signatures.first().and_then(|signature| signature.declaration())
            {
                return declaration.type_parameters();
            }
        }
    }
    &[]
}

fn type_parameters_from_call(context: &mut RuleContext<'_>, node: P<Node>) -> &'static [P<Node>] {
    let signature = context.checker.get_resolved_signature(node, None, CheckMode::Normal);
    if let Some(declaration) = signature.declaration() {
        let parameters = declaration.type_parameters();
        if !parameters.is_empty() {
            return parameters;
        }
    }
    if node.kind() == Kind::NewExpression {
        return type_parameters_from_type(context, node, node.as_new_expression().expression);
    }
    &[]
}

fn references_type_parameter(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    symbols: &FxHashSet<P<Symbol>>,
) -> bool {
    if node.kind() == Kind::Identifier
        && context
            .checker
            .get_symbol_at_location_exported(node)
            .is_some_and(|symbol| symbols.contains(&symbol))
    {
        return true;
    }
    node.for_each_child(&mut |child| references_type_parameter(context, child, symbols))
}

fn constructor_arguments_can_infer(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    parameters: &[P<Node>],
) -> bool {
    if node.kind() != Kind::NewExpression || node.arguments().is_empty() {
        return false;
    }
    let symbols: FxHashSet<_> = parameters
        .iter()
        .filter_map(|parameter| {
            let name = parameter.name()?;
            context.checker.get_symbol_at_location_exported(name)
        })
        .collect();
    if symbols.is_empty() {
        return false;
    }
    let signature = context.checker.get_resolved_signature(node, None, CheckMode::Normal);
    let Some(declaration) =
        signature.declaration().filter(|declaration| declaration.function_like_data().is_some())
    else {
        return false;
    };
    declaration.parameters().iter().any(|parameter| {
        parameter.type_node().is_some_and(|ty| references_type_parameter(context, ty, &symbols))
    })
}

fn type_for_comparison(
    context: &mut RuleContext<'_>,
    ty: P<Type>,
) -> (P<Type>, &'static [P<Type>]) {
    if ty.object_flags().intersects(ObjectFlags::Reference) {
        (ty.target().expect("reference type target"), context.checker.get_type_arguments(ty))
    } else {
        (ty, &[])
    }
}

fn check_arguments(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    arguments: Option<P<NodeList>>,
    parameters: &[P<Node>],
) {
    let Some(arguments) = arguments else {
        return;
    };
    let Some(last_index) = arguments.nodes().len().checked_sub(1) else {
        return;
    };
    let Some(parameter) = parameters.get(last_index) else {
        return;
    };
    let Some(default_node) = parameter.as_type_parameter_declaration().default_type() else {
        return;
    };
    let argument = arguments.nodes()[last_index];
    let default_type = context.checker.get_type_at_location(default_node);
    let argument_type = context.checker.get_type_at_location(argument);
    if default_type != argument_type {
        let default = type_for_comparison(context, default_type);
        let argument = type_for_comparison(context, argument_type);
        if default != argument {
            return;
        }
    }
    if last_index == 0 && constructor_arguments_can_infer(context, node, parameters) {
        return;
    }
    let file = context.source_file;
    context.report_node_with_fixes(
        argument,
        RuleMessage {
            id: "unnecessaryTypeParameter".into(),
            description: "This is the default value for this type parameter, so it can be omitted."
                .into(),
            ..Default::default()
        },
        || {
            let range = if last_index == 0 {
                Range {
                    pos: arguments.pos().expect_source("parsed type arguments") - 1,
                    end: tsrs_scanner::get_range_of_token_at_position(
                        file,
                        arguments.end().expect_source("parsed type arguments"),
                    )
                    .source_end()
                    .unwrap(),
                }
            } else {
                Range {
                    pos: arguments.nodes()[last_index - 1]
                        .end()
                        .expect_source("parsed type argument"),
                    end: argument.end().expect_source("parsed type argument"),
                }
            };
            vec![Fix { range, text: String::new() }]
        },
    );
}

impl Rule for NoUnnecessaryTypeArguments {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::ExpressionWithTypeArguments
                    | Kind::TypeReference
                    | Kind::CallExpression
                    | Kind::NewExpression
                    | Kind::TaggedTemplateExpression
                    | Kind::JsxOpeningElement
                    | Kind::JsxSelfClosingElement
            )
        )
    }
    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let (arguments, parameters) = match listener {
            Listener::Enter(Kind::ExpressionWithTypeArguments) => {
                let expression = node.as_expression_with_type_arguments();
                (
                    expression.type_arguments(),
                    type_parameters_from_type(context, node, expression.expression),
                )
            }
            Listener::Enter(Kind::TypeReference) => {
                let reference = node.as_type_reference_node();
                (
                    reference.type_arguments(),
                    type_parameters_from_type(context, node, reference.type_name),
                )
            }
            Listener::Enter(Kind::CallExpression) => (
                node.as_call_expression().type_arguments(),
                type_parameters_from_call(context, node),
            ),
            Listener::Enter(Kind::NewExpression) => (
                node.as_new_expression().type_arguments(),
                type_parameters_from_call(context, node),
            ),
            Listener::Enter(Kind::TaggedTemplateExpression) => (
                node.as_tagged_template_expression().type_arguments(),
                type_parameters_from_call(context, node),
            ),
            Listener::Enter(Kind::JsxOpeningElement) => (
                node.as_jsx_opening_element().type_arguments(),
                type_parameters_from_call(context, node),
            ),
            Listener::Enter(Kind::JsxSelfClosingElement) => (
                node.as_jsx_self_closing_element().type_arguments(),
                type_parameters_from_call(context, node),
            ),
            _ => unreachable!("no-unnecessary-type-arguments listener"),
        };
        check_arguments(context, node, arguments, parameters);
    }
}
