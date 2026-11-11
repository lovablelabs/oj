use tsrs_ast::{self as ast, FunctionFlags, Kind, Node};
use tsrs_checker::{ContextFlags, SignatureKind, Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils::{get_heritage_clauses, union_type_parts};

pub struct StrictVoidReturn {
    allowed_return_type_flags: TypeFlags,
}

#[derive(Clone, Copy)]
enum MessageId {
    AsyncFunc,
    NonVoidFunc,
    NonVoidReturn,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description) = match self {
            Self::AsyncFunc => {
                ("asyncFunc", "Async function used in a context where a void function is expected.")
            }
            Self::NonVoidFunc => (
                "nonVoidFunc",
                "Value-returning function used in a context where a void function is expected.",
            ),
            Self::NonVoidReturn => {
                ("nonVoidReturn", "Value returned in a context where a void return is expected.")
            }
        };
        RuleMessage { id: id.into(), description: description.into(), ..Default::default() }
    }
}

fn is_void_returning_function_type(context: &mut RuleContext<'_>, t: P<Type>) -> bool {
    let mut return_types = Vec::new();
    for &part in union_type_parts(&t) {
        for &signature in context.checker.get_signatures_of_type(part, SignatureKind::Call) {
            return_types.push(context.checker.get_return_type_of_signature(signature));
        }
    }
    !return_types.is_empty()
        && return_types.iter().all(|return_type| {
            union_type_parts(return_type)
                .iter()
                .all(|part| part.flags().intersects(TypeFlags::Void))
        })
}

fn is_nullish_or_any(t: P<Type>) -> bool {
    t.flags().intersects(
        TypeFlags::VoidLike
            | TypeFlags::Undefined
            | TypeFlags::Null
            | TypeFlags::Any
            | TypeFlags::Never,
    )
}

fn get_member_name(context: &mut RuleContext<'_>, name: Option<P<Node>>) -> String {
    let Some(name) = name else {
        return String::new();
    };
    if let Some(symbol) = context.checker.get_symbol_at_location_exported(name)
        && !symbol.name().is_empty()
    {
        return symbol.name().into();
    }
    match name.kind() {
        Kind::Identifier
        | Kind::PrivateIdentifier
        | Kind::StringLiteral
        | Kind::NumericLiteral
        | Kind::BigIntLiteral => name.text().into(),
        Kind::ComputedPropertyName => {
            let expression = name.as_computed_property_name().expression;
            match expression.kind() {
                Kind::Identifier
                | Kind::StringLiteral
                | Kind::NumericLiteral
                | Kind::BigIntLiteral => expression.text().into(),
                _ => String::new(),
            }
        }
        _ => String::new(),
    }
}

fn get_base_member_types(context: &mut RuleContext<'_>, member: P<Node>) -> Vec<P<Type>> {
    let Some(clauses) = member.parent().and_then(get_heritage_clauses) else {
        return Vec::new();
    };
    let Some(symbol) =
        member.name().and_then(|name| context.checker.get_symbol_at_location_exported(name))
    else {
        return Vec::new();
    };
    let mut types = Vec::new();
    for &clause in clauses.nodes() {
        for &heritage in clause.as_heritage_clause().types().nodes() {
            let heritage_type = context.checker.get_type_at_location(heritage);
            if let Some(property) =
                context.checker.get_property_of_type(heritage_type, symbol.name())
            {
                types.push(
                    context
                        .checker
                        .get_type_of_symbol_at_location(property, Some(member))
                        .expect("base member type"),
                );
            }
        }
    }
    types
}

impl StrictVoidReturn {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("strict-void-return: options must be an object".into()),
        };
        let allow_return_any = match object.and_then(|object| object.get("allowReturnAny")) {
            None | Some(Value::Null) => false,
            Some(Value::Bool(value)) => *value,
            _ => return Err("strict-void-return: allowReturnAny must be a boolean".into()),
        };
        let mut allowed_return_type_flags =
            TypeFlags::Void | TypeFlags::Never | TypeFlags::Undefined;
        if allow_return_any {
            allowed_return_type_flags |= TypeFlags::Any;
        }
        Ok(Self { allowed_return_type_flags })
    }

    fn is_allowed_type(&self, t: P<Type>) -> bool {
        union_type_parts(&t)
            .iter()
            .all(|part| part.flags().intersects(self.allowed_return_type_flags))
    }

    fn report_if_non_void_function(&self, context: &mut RuleContext<'_>, function: P<Node>) {
        let actual_type = context.checker.get_type_at_location(function);
        let actual_type = context.checker.get_apparent_type(actual_type);
        if context.checker.get_signatures_of_type(actual_type, SignatureKind::Call).iter().all(
            |&signature| {
                self.is_allowed_type(context.checker.get_return_type_of_signature(signature))
            },
        ) {
            return;
        }
        if !matches!(
            function.kind(),
            Kind::ArrowFunction | Kind::FunctionExpression | Kind::MethodDeclaration
        ) {
            context.report_node(function, MessageId::NonVoidFunc.message());
            return;
        }
        let flags = ast::get_function_flags(Some(function));
        if flags.intersects(FunctionFlags::Generator) {
            context.report_node(function, MessageId::NonVoidFunc.message());
            return;
        }
        if flags.intersects(FunctionFlags::Async) {
            context.report_node(function, MessageId::AsyncFunc.message());
            return;
        }
        if let Some(body) = function.body().filter(|body| body.kind() != Kind::Block) {
            context.report_node(body, MessageId::NonVoidReturn.message());
            return;
        }
        if let Some(annotation) =
            function.type_node().filter(|annotation| annotation.kind() != Kind::VoidKeyword)
        {
            context.report_node(annotation, MessageId::NonVoidFunc.message());
            return;
        }
        self.visit_returns(context, function, function);
    }

    fn visit_returns(&self, context: &mut RuleContext<'_>, node: P<Node>, function: P<Node>) {
        if node != function && ast::is_function_like(node) {
            return;
        }
        if node.kind() == Kind::ReturnStatement
            && let Some(expression) = node.expression()
        {
            let return_type = context.checker.get_type_at_location(expression);
            if !self.is_allowed_type(return_type) {
                context.report_node(node, MessageId::NonVoidReturn.message());
            }
        }
        node.for_each_child(&mut |child| {
            self.visit_returns(context, child, function);
            false
        });
    }

    fn check_expression_node(&self, context: &mut RuleContext<'_>, node: P<Node>) -> bool {
        if let Some(expected) = context.checker.get_contextual_type(node, ContextFlags::None)
            && is_void_returning_function_type(context, expected)
        {
            self.report_if_non_void_function(context, node);
            return true;
        }
        false
    }

    fn check_function_call_node(&self, context: &mut RuleContext<'_>, call: P<Node>) {
        let callee = call.expression().expect("call expression");
        let function_type = context.checker.get_type_at_location(callee);
        let mut signatures = Vec::new();
        let kind = if call.kind() == Kind::CallExpression {
            SignatureKind::Call
        } else {
            SignatureKind::Construct
        };
        for &part in union_type_parts(&function_type) {
            signatures.extend_from_slice(context.checker.get_signatures_of_type(part, kind));
        }
        for (index, &argument) in call.arguments().iter().enumerate() {
            if argument.kind() == Kind::SpreadElement {
                continue;
            }
            let mut expected_returns = Vec::new();
            for &signature in &signatures {
                let Some(&parameter) = signature.parameters().get(index) else {
                    continue;
                };
                let parameter_type = context
                    .checker
                    .get_type_of_symbol_at_location(parameter, Some(callee))
                    .expect("parameter type");
                for &part in union_type_parts(&parameter_type) {
                    for &parameter_signature in
                        context.checker.get_signatures_of_type(part, SignatureKind::Call)
                    {
                        expected_returns.push(
                            context.checker.get_return_type_of_signature(parameter_signature),
                        );
                    }
                }
            }
            let all_return_void = expected_returns.iter().all(|&t| {
                t.flags().intersects(TypeFlags::Void | TypeFlags::TypeParameter)
                    || is_nullish_or_any(t)
            });
            if (signatures.len() == 1 || all_return_void)
                && self.check_expression_node(context, argument)
            {
                continue;
            }
            if expected_returns.iter().any(|t| t.flags().intersects(TypeFlags::Void))
                && expected_returns.iter().all(|&t| is_nullish_or_any(t))
            {
                self.report_if_non_void_function(context, argument);
            }
        }
    }

    fn check_object_method_node(&self, context: &mut RuleContext<'_>, method: P<Node>) {
        if method.name().is_some_and(|name| name.kind() == Kind::ComputedPropertyName) {
            return;
        }
        let Some(object_type) = context
            .checker
            .get_contextual_type(method.parent().expect("method parent"), ContextFlags::None)
        else {
            return;
        };
        let name = get_member_name(context, method.name());
        if name.is_empty() {
            return;
        }
        let Some(property) = context.checker.get_property_of_type(object_type, &name) else {
            return;
        };
        let expected = context
            .checker
            .get_type_of_symbol_at_location(property, Some(method))
            .expect("contextual method type");
        if is_void_returning_function_type(context, expected) {
            self.report_if_non_void_function(context, method);
        }
    }

    fn check_class_method_node(&self, context: &mut RuleContext<'_>, method: P<Node>) {
        if method.body().is_none() {
            return;
        }
        if get_base_member_types(context, method)
            .into_iter()
            .any(|t| is_void_returning_function_type(context, t))
        {
            self.report_if_non_void_function(context, method);
        }
    }

    fn check_class_property_node(&self, context: &mut RuleContext<'_>, property: P<Node>) {
        for base in get_base_member_types(context, property) {
            if is_void_returning_function_type(context, base)
                && let Some(initializer) = property.initializer()
            {
                self.report_if_non_void_function(context, initializer);
                return;
            }
        }
        if let Some(initializer) = property.initializer() {
            self.check_expression_node(context, initializer);
        }
    }
}

impl Rule for StrictVoidReturn {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::ArrayLiteralExpression
                    | Kind::ArrowFunction
                    | Kind::BinaryExpression
                    | Kind::CallExpression
                    | Kind::NewExpression
                    | Kind::JsxAttribute
                    | Kind::MethodDeclaration
                    | Kind::PropertyDeclaration
                    | Kind::PropertyAssignment
                    | Kind::ShorthandPropertyAssignment
                    | Kind::ReturnStatement
                    | Kind::VariableDeclaration
            )
        )
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match node.kind() {
            Kind::ArrayLiteralExpression => {
                for &element in node.elements() {
                    if element.kind() != Kind::SpreadElement {
                        self.check_expression_node(context, element);
                    }
                }
            }
            Kind::ArrowFunction => {
                if let Some(body) = node.body().filter(|body| body.kind() != Kind::Block) {
                    self.check_expression_node(context, body);
                }
            }
            Kind::BinaryExpression => {
                if ast::is_assignment_expression(node, false) {
                    self.check_expression_node(context, node.as_binary_expression().right());
                }
            }
            Kind::CallExpression | Kind::NewExpression => {
                self.check_function_call_node(context, node);
            }
            Kind::JsxAttribute => {
                if let Some(initializer) = node
                    .initializer()
                    .filter(|initializer| initializer.kind() == Kind::JsxExpression)
                    && let Some(expression) = initializer
                        .expression()
                        .filter(|expression| expression.kind() != Kind::OmittedExpression)
                {
                    self.check_expression_node(context, expression);
                }
            }
            Kind::MethodDeclaration => {
                if node
                    .parent()
                    .is_some_and(|parent| parent.kind() == Kind::ObjectLiteralExpression)
                {
                    self.check_object_method_node(context, node);
                } else {
                    self.check_class_method_node(context, node);
                }
            }
            Kind::PropertyDeclaration => self.check_class_property_node(context, node),
            Kind::PropertyAssignment | Kind::VariableDeclaration => {
                if let Some(initializer) = node.initializer() {
                    self.check_expression_node(context, initializer);
                }
            }
            Kind::ShorthandPropertyAssignment => {
                self.check_expression_node(context, node.name().expect("shorthand name"));
            }
            Kind::ReturnStatement => {
                if let Some(expression) = node.expression() {
                    self.check_expression_node(context, expression);
                }
            }
            _ => unreachable!("strict-void-return listener"),
        }
    }
}
