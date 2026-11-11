//! Port of tsgolint's `internal/rules/no_misused_promises/no_misused_promises.go`.

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{self as ast, Kind, Node, Symbol};
use tsrs_checker::{self as checker, ContextFlags, SignatureKind, Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::{Diagnostic, LabeledRange, Range, RuleMessage};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::{
    get_function_head_loc, get_heritage_clauses, is_array_method_call_with_predicate,
    is_rest_parameter_declaration, is_thenable_type, union_type_parts,
};

struct ChecksVoidReturn {
    arguments: bool,
    attributes: bool,
    inherited_methods: bool,
    properties: bool,
    returns: bool,
    variables: bool,
}

impl Default for ChecksVoidReturn {
    fn default() -> Self {
        Self {
            arguments: true,
            attributes: true,
            inherited_methods: true,
            properties: true,
            returns: true,
            variables: true,
        }
    }
}

impl ChecksVoidReturn {
    fn from_object(object: &tsrs_core::json::JsonMap) -> Self {
        let boolean = |name: &str| match object.get(name) {
            None | Some(Value::Null) => Ok(true),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(()),
        };
        let parse = || {
            Ok::<_, ()>(Self {
                arguments: boolean("arguments")?,
                attributes: boolean("attributes")?,
                inherited_methods: boolean("inheritedMethods")?,
                properties: boolean("properties")?,
                returns: boolean("returns")?,
                variables: boolean("variables")?,
            })
        };
        parse().unwrap_or_default()
    }
}

pub struct NoMisusedPromises {
    checks_conditionals: bool,
    checks_spreads: bool,
    void_return: Option<ChecksVoidReturn>,
    checked_nodes: FxHashSet<P<Node>>,
}

#[derive(Clone, Copy)]
enum MessageId {
    Conditional,
    Predicate,
    Spread,
    VoidReturnArgument,
    VoidReturnAttribute,
    VoidReturnProperty,
    VoidReturnReturnValue,
    VoidReturnVariable,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description) = match self {
            Self::Conditional => {
                ("conditional", "Expected non-Promise value in a boolean conditional.")
            }
            Self::Predicate => ("predicate", "Expected a non-Promise value to be returned."),
            Self::Spread => ("spread", "Expected a non-Promise value to be spread in an object."),
            Self::VoidReturnArgument => (
                "voidReturnArgument",
                "Promise returned in function argument where a void return was expected.",
            ),
            Self::VoidReturnAttribute => (
                "voidReturnAttribute",
                "Promise-returning function provided to attribute where a void return was expected.",
            ),
            Self::VoidReturnProperty => (
                "voidReturnProperty",
                "Promise-returning function provided to property where a void return was expected.",
            ),
            Self::VoidReturnReturnValue => (
                "voidReturnReturnValue",
                "Promise-returning function provided to return value where a void return was expected.",
            ),
            Self::VoidReturnVariable => (
                "voidReturnVariable",
                "Promise-returning function provided to variable where a void return was expected.",
            ),
        };
        RuleMessage { id: id.into(), description: description.into(), ..Default::default() }
    }
}

#[derive(Clone, Copy)]
struct VoidExpectation {
    declaration: P<Node>,
    ty: P<Type>,
}

fn local_expectation_for_declaration(
    context: &RuleContext<'_>,
    declaration: Option<P<Node>>,
    ty: P<Type>,
) -> Option<VoidExpectation> {
    declaration
        .filter(|&node| ast::get_source_file_of_node(node) == Some(context.source_file))
        .map(|declaration| VoidExpectation { declaration, ty })
}

fn local_expectation_for_symbol(
    context: &RuleContext<'_>,
    symbol: Option<P<Symbol>>,
    ty: P<Type>,
) -> Option<VoidExpectation> {
    let symbol = symbol?;
    symbol
        .value_declaration()
        .into_iter()
        .chain(symbol.declarations().iter().copied())
        .find_map(|node| local_expectation_for_declaration(context, Some(node), ty))
}

fn promise_range(context: &RuleContext<'_>, node: P<Node>) -> Range {
    let node = ast::skip_parentheses(node);
    if ast::is_function_like(node) {
        if let Some(annotation) = node.type_node() {
            return trim_node_range(context.source_file, annotation);
        }
        if node.kind() == Kind::ArrowFunction
            && let Some(arrow) = node.as_arrow_function().equals_greater_than_token
        {
            return trim_node_range(context.source_file, arrow);
        }
        return get_function_head_loc(context.source_file, node);
    }
    trim_node_range(context.source_file, node)
}

fn build_diagnostic(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    range: Range,
    message: RuleMessage,
    expectation: Option<VoidExpectation>,
) -> Diagnostic {
    let ty = context.checker.get_type_at_location(node);
    let description = if ast::is_function_like(ast::skip_parentheses(node))
        || !context.checker.get_signatures_of_type(ty, SignatureKind::Call).is_empty()
    {
        "This callback"
    } else {
        "This expression"
    };
    let mut diagnostic = Diagnostic {
        range: Some(range),
        message,
        labeled_ranges: vec![LabeledRange {
            label: format!(
                "{description} has type `{}`.",
                context.checker.type_to_string_exported(ty)
            ),
            range: promise_range(context, node),
        }],
        ..Default::default()
    };
    if let Some(expectation) = expectation {
        let declaration = expectation.declaration;
        diagnostic.labeled_ranges.push(LabeledRange {
            label: format!(
                "This context accepts a `void`-returning callback through type `{}`.",
                context.checker.type_to_string_exported(expectation.ty)
            ),
            range: trim_node_range(
                context.source_file,
                declaration.type_node().or_else(|| declaration.name()).unwrap_or(declaration),
            ),
        });
    }
    diagnostic
}

fn report_node(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    message: RuleMessage,
    expectation: Option<VoidExpectation>,
) {
    let range = promise_range(context, node);
    let diagnostic = build_diagnostic(context, node, range, message, expectation);
    context.report_diagnostic(diagnostic);
}

fn local_property_expectation(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    ty: P<Type>,
) -> Option<VoidExpectation> {
    let name = node.name()?;
    let parent = node.parent().filter(|parent| parent.kind() == Kind::ObjectLiteralExpression)?;
    let object_type = context.checker.get_contextual_type(parent, ContextFlags::None)?;
    let symbol = context.checker.get_property_of_type(object_type, name.text());
    local_expectation_for_symbol(context, symbol, ty)
}

#[derive(Default)]
struct VoidArguments {
    thenable_indices: Vec<usize>,
    void_indices: Vec<usize>,
    expectations: FxHashMap<usize, VoidExpectation>,
}

fn any_signature_is_thenable_type(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    t: P<Type>,
) -> bool {
    context.checker.get_signatures_of_type(t, SignatureKind::Call).iter().any(|&signature| {
        let t = context.checker.get_return_type_of_signature(signature);
        is_thenable_type(context.checker, node, t)
    })
}

fn returns_thenable(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let t = context.checker.get_type_at_location(node);
    let apparent = context.checker.get_apparent_type(t);
    union_type_parts(&apparent)
        .iter()
        .any(|&part| any_signature_is_thenable_type(context, node, part))
}

fn is_function_param(context: &mut RuleContext<'_>, parameter: P<Symbol>, node: P<Node>) -> bool {
    let Some(t) = context.checker.get_type_of_symbol_at_location(parameter, Some(node)) else {
        return false;
    };
    let t = context.checker.get_apparent_type(t);
    union_type_parts(&t)
        .iter()
        .any(|&part| !context.checker.get_signatures_of_type(part, SignatureKind::Call).is_empty())
}

fn is_always_thenable(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let t = context.checker.get_type_at_location(node);
    let apparent = context.checker.get_apparent_type(t);
    for &part in union_type_parts(&apparent) {
        let Some(then) = context.checker.get_property_of_type(part, "then") else {
            return false;
        };
        let then_type = context
            .checker
            .get_type_of_symbol_at_location(then, Some(node))
            .expect("then property type");
        let mut has_thenable_signature = false;
        for &part in union_type_parts(&then_type) {
            for &signature in context.checker.get_signatures_of_type(part, SignatureKind::Call) {
                if signature
                    .parameters()
                    .first()
                    .is_some_and(|&parameter| is_function_param(context, parameter, node))
                {
                    has_thenable_signature = true;
                }
            }
            if has_thenable_signature {
                break;
            }
        }
        if !has_thenable_signature {
            return false;
        }
    }
    true
}

fn get_member_if_exists(
    context: &mut RuleContext<'_>,
    t: P<Type>,
    name: &str,
) -> Option<P<Symbol>> {
    if let Some(member) =
        t.symbol().and_then(|symbol| symbol.members()).and_then(|members| members.lookup(name))
    {
        return Some(member);
    }
    context.checker.get_property_of_type(t, name)
}

fn visit_function_type_parts(
    context: &mut RuleContext<'_>,
    t: P<Type>,
    visit: &mut impl FnMut(&mut RuleContext<'_>, P<Type>) -> bool,
) -> bool {
    let t = context.checker.get_reduced_apparent_type(t);
    if t.flags().intersects(TypeFlags::Union) {
        for &part in t.as_union_type().types() {
            if !visit_function_type_parts(context, part, visit) {
                return false;
            }
        }
        return true;
    }
    visit(context, t)
}

fn is_void_returning_function_type(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    t: P<Type>,
) -> bool {
    let mut had_void_return = false;
    if !visit_function_type_parts(context, t, &mut |context, part| {
        for &signature in context.checker.get_signatures_of_type(part, SignatureKind::Call) {
            let return_type = context.checker.get_return_type_of_signature(signature);
            if is_thenable_type(context.checker, node, return_type) {
                return false;
            }
            had_void_return |= return_type.flags().intersects(TypeFlags::Void);
        }
        true
    }) {
        return false;
    }
    had_void_return
}

fn is_thenable_returning_function_type(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    t: P<Type>,
) -> bool {
    !visit_function_type_parts(context, t, &mut |context, part| {
        !any_signature_is_thenable_type(context, node, part)
    })
}

fn check_thenable_or_void_argument(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    t: P<Type>,
    index: usize,
    arguments: &mut VoidArguments,
    declaration: Option<P<Node>>,
    from_contextual_type: bool,
) {
    let callee = node.expression().unwrap();
    if is_thenable_returning_function_type(context, callee, t) {
        arguments.thenable_indices.push(index);
    } else if is_void_returning_function_type(context, callee, t)
        && !arguments.thenable_indices.contains(&index)
    {
        arguments.void_indices.push(index);
        if let Some(expectation) = local_expectation_for_declaration(context, declaration, t)
            && (!arguments.expectations.contains_key(&index) || !from_contextual_type)
        {
            arguments.expectations.insert(index, expectation);
        }
    }
    let contextual = context.checker.get_contextual_type_for_argument_at_index(node, index);
    if contextual != t {
        check_thenable_or_void_argument(
            context,
            node,
            contextual,
            index,
            arguments,
            declaration,
            true,
        );
    }
}

fn void_function_arguments(context: &mut RuleContext<'_>, node: P<Node>) -> VoidArguments {
    if node.arguments().is_empty() {
        return VoidArguments::default();
    }
    let mut result = VoidArguments::default();
    let callee = node.expression().unwrap();
    let t = context.checker.get_type_at_location(callee);
    for &part in union_type_parts(&t) {
        let kind = if node.kind() == Kind::CallExpression {
            SignatureKind::Call
        } else {
            SignatureKind::Construct
        };
        for &signature in context.checker.get_signatures_of_type(part, kind) {
            for (index, &parameter) in signature.parameters().iter().enumerate() {
                if index >= node.arguments().len() {
                    break;
                }
                let t = context
                    .checker
                    .get_type_of_symbol_at_location(parameter, Some(callee))
                    .expect("parameter type");
                if parameter.value_declaration().is_some_and(is_rest_parameter_declaration) {
                    if context.checker.is_array_type(t) {
                        let element = context.checker.get_type_arguments(t)[0];
                        for index in index..node.arguments().len() {
                            check_thenable_or_void_argument(
                                context,
                                node,
                                element,
                                index,
                                &mut result,
                                parameter.value_declaration(),
                                false,
                            );
                        }
                    } else if checker::is_tuple_type_exported(t) {
                        let arguments = context.checker.get_type_arguments(t);
                        for position in index..node.arguments().len().min(index + arguments.len()) {
                            check_thenable_or_void_argument(
                                context,
                                node,
                                arguments[position - index],
                                position,
                                &mut result,
                                parameter.value_declaration(),
                                false,
                            );
                        }
                    }
                } else {
                    check_thenable_or_void_argument(
                        context,
                        node,
                        t,
                        index,
                        &mut result,
                        parameter.value_declaration(),
                        false,
                    );
                }
            }
        }
    }
    for index in result.thenable_indices.iter().copied() {
        if let Some(position) = result.void_indices.iter().position(|&candidate| candidate == index)
        {
            result.void_indices.remove(position);
            result.expectations.remove(&index);
        }
    }
    result
}

fn is_possibly_function_type(node: P<Node>) -> bool {
    match node.kind() {
        Kind::ConditionalType
        | Kind::ConstructorType
        | Kind::FunctionType
        | Kind::ImportType
        | Kind::IndexedAccessType
        | Kind::InferType
        | Kind::IntersectionType
        | Kind::QualifiedName
        | Kind::ThisType
        | Kind::TypeOperator
        | Kind::TypeQuery
        | Kind::TypeReference
        | Kind::UnionType => true,
        Kind::TypeLiteral => node
            .as_type_literal_node()
            .members
            .nodes()
            .iter()
            .any(|node| matches!(node.kind(), Kind::CallSignature | Kind::ConstructSignature)),
        _ => false,
    }
}

impl NoMisusedPromises {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("no-misused-promises: options must be an object".into()),
        };
        let boolean = |name: &str| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(true),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("no-misused-promises: {name} must be a boolean")),
        };
        let void_return = match object.and_then(|object| object.get("checksVoidReturn")) {
            None | Some(Value::Null | Value::Bool(true)) => Some(ChecksVoidReturn::default()),
            Some(Value::Object(object)) => Some(ChecksVoidReturn::from_object(object)),
            _ => None,
        };
        Ok(Self {
            checks_conditionals: boolean("checksConditionals")?,
            checks_spreads: boolean("checksSpreads")?,
            void_return,
            checked_nodes: FxHashSet::default(),
        })
    }

    fn check_conditional(
        &mut self,
        context: &mut RuleContext<'_>,
        node: Option<P<Node>>,
        is_test: bool,
    ) {
        let Some(node) = node else {
            return;
        };
        if ast::is_assignment_expression(node, false) || !self.checked_nodes.insert(node) {
            return;
        }
        let node = ast::skip_parentheses(node);
        if node.kind() == Kind::BinaryExpression && ast::is_logical_expression(node) {
            let expression = node.as_binary_expression();
            if expression.operator_token.kind() != Kind::QuestionQuestionToken || is_test {
                self.check_conditional(context, Some(expression.left), is_test);
            }
            if is_test {
                self.check_conditional(context, Some(expression.right()), is_test);
            }
            return;
        }
        if is_always_thenable(context, node) {
            report_node(context, node, MessageId::Conditional.message(), None);
        }
    }

    fn check_array_predicates(context: &mut RuleContext<'_>, node: P<Node>) {
        let Some(parent) = node.parent().filter(|parent| parent.kind() == Kind::CallExpression)
        else {
            return;
        };
        let Some(&callback) = parent.arguments().first() else {
            return;
        };
        if is_array_method_call_with_predicate(context.checker, parent)
            && returns_thenable(context, callback)
        {
            report_node(context, callback, MessageId::Predicate.message(), None);
        }
    }

    fn check_heritage_type_for_member_returning_void(
        context: &mut RuleContext<'_>,
        node: P<Node>,
        heritage: P<Type>,
        name: &str,
    ) {
        let Some(member) = get_member_if_exists(context, heritage, name) else {
            return;
        };
        let t = context
            .checker
            .get_type_of_symbol_at_location(member, Some(node))
            .expect("heritage member type");
        if !is_void_returning_function_type(context, node, t) {
            return;
        }
        let name = context.checker.type_to_string_exported(heritage);
        let expectation = local_expectation_for_symbol(context, Some(member), t);
        report_node(
            context,
            node,
            RuleMessage {
                id: "voidReturnInheritedMethod".into(),
                description: format!(
                    "Promise-returning method provided where a void return was expected by extended/implemented type '{name}'."
                ),
                ..Default::default()
            },
            expectation,
        );
    }

    fn check_class_like_or_interface_node(context: &mut RuleContext<'_>, node: P<Node>) {
        let Some(clauses) = get_heritage_clauses(node) else {
            return;
        };
        if clauses.nodes().is_empty() {
            return;
        }
        let mut heritage_types = None;
        for &member in node.members() {
            let Some(name) = member.name() else {
                continue;
            };
            if !matches!(
                name.kind(),
                Kind::Identifier
                    | Kind::PrivateIdentifier
                    | Kind::StringLiteral
                    | Kind::NumericLiteral
                    | Kind::BigIntLiteral
            ) || ast::is_static(member)
                || !returns_thenable(context, member)
            {
                continue;
            }
            let heritage_types = heritage_types.get_or_insert_with(|| {
                clauses
                    .nodes()
                    .iter()
                    .flat_map(|clause| clause.as_heritage_clause().types().nodes().iter())
                    .map(|&node| context.checker.get_type_at_location(node))
                    .collect::<Vec<_>>()
            });
            for &heritage in heritage_types.iter() {
                Self::check_heritage_type_for_member_returning_void(
                    context,
                    member,
                    heritage,
                    name.text(),
                );
            }
        }
    }

    fn check_jsx_attribute(context: &mut RuleContext<'_>, node: P<Node>) {
        let Some(initializer) =
            node.initializer().filter(|initializer| initializer.kind() == Kind::JsxExpression)
        else {
            return;
        };
        let Some(expression) = initializer.expression() else {
            return;
        };
        if let Some(t) = context.checker.get_contextual_type(initializer, ContextFlags::None)
            && is_void_returning_function_type(context, initializer, t)
            && returns_thenable(context, expression)
        {
            let expectation = node
                .parent()
                .and_then(|parent| context.checker.get_contextual_type(parent, ContextFlags::None))
                .and_then(|ty| {
                    context.checker.get_property_of_type(ty, node.name().unwrap().text())
                })
                .and_then(|symbol| local_expectation_for_symbol(context, Some(symbol), t));
            report_node(context, expression, MessageId::VoidReturnAttribute.message(), expectation);
        }
    }

    fn check_arguments(context: &mut RuleContext<'_>, node: P<Node>) {
        let void_arguments = void_function_arguments(context, node);
        if void_arguments.void_indices.is_empty() {
            return;
        }
        for (index, &argument) in node.arguments().iter().enumerate() {
            if void_arguments.void_indices.contains(&index) && returns_thenable(context, argument) {
                report_node(
                    context,
                    argument,
                    MessageId::VoidReturnArgument.message(),
                    void_arguments.expectations.get(&index).copied(),
                );
            }
        }
    }

    fn check_property(context: &mut RuleContext<'_>, node: P<Node>) {
        match node.kind() {
            Kind::PropertyAssignment => {
                let initializer = node.initializer().unwrap();
                if let Some(t) =
                    context.checker.get_contextual_type(initializer, ContextFlags::None)
                    && is_void_returning_function_type(context, initializer, t)
                    && returns_thenable(context, initializer)
                {
                    let expectation = local_property_expectation(context, node, t);
                    report_node(
                        context,
                        initializer,
                        MessageId::VoidReturnProperty.message(),
                        expectation,
                    );
                }
            }
            Kind::ShorthandPropertyAssignment => {
                let name = node.name().unwrap();
                if let Some(t) = context.checker.get_contextual_type(name, ContextFlags::None)
                    && is_void_returning_function_type(context, name, t)
                    && returns_thenable(context, name)
                {
                    let expectation = local_property_expectation(context, node, t);
                    report_node(
                        context,
                        name,
                        MessageId::VoidReturnProperty.message(),
                        expectation,
                    );
                }
            }
            Kind::MethodDeclaration => {
                let name = node.name().unwrap();
                if name.kind() == Kind::ComputedPropertyName {
                    return;
                }
                let Some(object) =
                    node.parent().filter(|parent| parent.kind() == Kind::ObjectLiteralExpression)
                else {
                    return;
                };
                if !returns_thenable(context, node) {
                    return;
                }
                let Some(object_type) =
                    context.checker.get_contextual_type(object, ContextFlags::None)
                else {
                    return;
                };
                let Some(property) = context.checker.get_property_of_type(object_type, name.text())
                else {
                    return;
                };
                let contextual = context
                    .checker
                    .get_type_of_symbol_at_location(property, Some(name))
                    .expect("contextual property type");
                if is_void_returning_function_type(context, name, contextual) {
                    let expectation =
                        local_expectation_for_symbol(context, Some(property), contextual);
                    report_node(
                        context,
                        node,
                        MessageId::VoidReturnProperty.message(),
                        expectation,
                    );
                }
            }
            _ => unreachable!("no-misused-promises property"),
        }
    }

    fn check_return_statement(context: &mut RuleContext<'_>, node: P<Node>) {
        let Some(expression) = node.expression() else {
            return;
        };
        let mut parent = node.parent();
        while let Some(function) = parent {
            if ast::is_function_like(function) {
                if function
                    .type_node()
                    .is_some_and(|annotation| !is_possibly_function_type(annotation))
                {
                    return;
                }
                break;
            }
            parent = function.parent();
        }
        if let Some(t) = context.checker.get_contextual_type(expression, ContextFlags::None)
            && is_void_returning_function_type(context, expression, t)
            && returns_thenable(context, expression)
        {
            let mut expectation = None;
            if let Some(function) = parent {
                if function.type_node().is_some() {
                    expectation = Some(VoidExpectation { declaration: function, ty: t });
                } else if let Some(function_type) =
                    context.checker.get_contextual_type(function, ContextFlags::None)
                {
                    for &signature in
                        context.checker.get_signatures_of_type(function_type, SignatureKind::Call)
                    {
                        expectation =
                            local_expectation_for_declaration(context, signature.declaration(), t);
                        if expectation.is_some() {
                            break;
                        }
                    }
                }
            }
            report_node(
                context,
                expression,
                MessageId::VoidReturnReturnValue.message(),
                expectation,
            );
        }
    }

    fn check_assignment(context: &mut RuleContext<'_>, node: P<Node>) {
        let expression = node.as_binary_expression();
        let t = context.checker.get_type_at_location(expression.left);
        if is_void_returning_function_type(context, expression.left, t)
            && returns_thenable(context, expression.right())
        {
            let symbol =
                context.checker.get_symbol_at_location_exported(expression.left).or_else(|| {
                    let name = expression.left.name()?;
                    context.checker.get_symbol_at_location_exported(name)
                });
            let expectation = local_expectation_for_symbol(context, symbol, t);
            report_node(
                context,
                expression.right(),
                MessageId::VoidReturnVariable.message(),
                expectation,
            );
        }
    }

    fn check_variable_declaration(context: &mut RuleContext<'_>, node: P<Node>) {
        let (Some(initializer), Some(annotation)) = (node.initializer(), node.type_node()) else {
            return;
        };
        if !is_possibly_function_type(annotation) {
            return;
        }
        let t = context.checker.get_type_at_location(node.name().unwrap());
        if is_void_returning_function_type(context, initializer, t)
            && returns_thenable(context, initializer)
        {
            report_node(
                context,
                initializer,
                MessageId::VoidReturnVariable.message(),
                Some(VoidExpectation { declaration: node, ty: t }),
            );
        }
    }
}

impl Rule for NoMisusedPromises {
    fn listens_to(&self, listener: Listener) -> bool {
        let Listener::Enter(kind) = listener else {
            return false;
        };
        if kind == Kind::BinaryExpression {
            return true;
        }
        if self.checks_conditionals
            && matches!(
                kind,
                Kind::PropertyAccessExpression
                    | Kind::ElementAccessExpression
                    | Kind::PrefixUnaryExpression
                    | Kind::ConditionalExpression
                    | Kind::ForStatement
                    | Kind::DoStatement
                    | Kind::WhileStatement
                    | Kind::IfStatement
            )
        {
            return true;
        }
        if self.checks_spreads && matches!(kind, Kind::SpreadElement | Kind::SpreadAssignment) {
            return true;
        }
        self.void_return.as_ref().is_some_and(|options| match kind {
            Kind::CallExpression | Kind::NewExpression => options.arguments,
            Kind::JsxAttribute => options.attributes,
            Kind::ClassDeclaration | Kind::ClassExpression | Kind::InterfaceDeclaration => {
                options.inherited_methods
            }
            Kind::PropertyAssignment
            | Kind::MethodDeclaration
            | Kind::ShorthandPropertyAssignment => options.properties,
            Kind::ReturnStatement => options.returns,
            Kind::VariableDeclaration => options.variables,
            _ => false,
        })
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match node.kind() {
            Kind::BinaryExpression => {
                if self.checks_conditionals && ast::is_logical_or_coalescing_binary_expression(node)
                {
                    self.check_conditional(context, Some(node), false);
                }
                if self.void_return.as_ref().is_some_and(|options| options.variables)
                    && ast::is_assignment_expression(node, false)
                {
                    Self::check_assignment(context, node);
                }
            }
            Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
                Self::check_array_predicates(context, node);
            }
            Kind::PrefixUnaryExpression => {
                let expression = node.as_prefix_unary_expression();
                if expression.operator == Kind::ExclamationToken {
                    self.check_conditional(context, Some(expression.operand), true);
                }
            }
            Kind::ConditionalExpression => self.check_conditional(
                context,
                Some(node.as_conditional_expression().condition),
                true,
            ),
            Kind::ForStatement => {
                self.check_conditional(context, node.as_for_statement().condition(), true);
            }
            Kind::DoStatement | Kind::WhileStatement | Kind::IfStatement => {
                self.check_conditional(context, node.expression(), true);
            }
            Kind::CallExpression | Kind::NewExpression => Self::check_arguments(context, node),
            Kind::JsxAttribute => Self::check_jsx_attribute(context, node),
            Kind::ClassDeclaration | Kind::ClassExpression | Kind::InterfaceDeclaration => {
                Self::check_class_like_or_interface_node(context, node);
            }
            Kind::PropertyAssignment
            | Kind::MethodDeclaration
            | Kind::ShorthandPropertyAssignment => Self::check_property(context, node),
            Kind::ReturnStatement => Self::check_return_statement(context, node),
            Kind::VariableDeclaration => Self::check_variable_declaration(context, node),
            Kind::SpreadElement | Kind::SpreadAssignment => {
                let expression = node.expression().unwrap();
                let t = context.checker.get_type_at_location(expression);
                if is_thenable_type(context.checker, expression, t) {
                    let range = Range {
                        pos: trim_node_range(context.source_file, node).pos,
                        end: trim_node_range(context.source_file, expression).pos,
                    };
                    let diagnostic = build_diagnostic(
                        context,
                        expression,
                        range,
                        MessageId::Spread.message(),
                        None,
                    );
                    context.report_diagnostic(diagnostic);
                }
            }
            _ => unreachable!("no-misused-promises listener"),
        }
    }
}
