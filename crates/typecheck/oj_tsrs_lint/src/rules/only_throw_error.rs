use tsrs_ast::{Kind, Node};
use tsrs_checker::TypeFlags;
use tsrs_core::{P, json::Value};

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::type_matches_specifier::{TypeOrValueSpecifier, type_matches_some_specifier};
use crate::utils;

pub struct OnlyThrowError {
    allow: Vec<TypeOrValueSpecifier>,
    allow_rethrowing: bool,
    allow_throwing_any: bool,
    allow_throwing_unknown: bool,
}

#[derive(Clone, Copy)]
enum MessageId {
    Object,
    Undef,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description) = match self {
            Self::Object => ("object", "Expected an error object to be thrown."),
            Self::Undef => ("undef", "Do not throw undefined."),
        };
        RuleMessage { id: id.into(), description: description.into(), help: String::new() }
    }
}

fn is_rethrown_error(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    if node.kind() != Kind::Identifier {
        return false;
    }
    let Some(declaration) = context
        .checker
        .get_symbol_at_location_exported(node)
        .and_then(|symbol| symbol.declarations().first().copied())
    else {
        return false;
    };
    let Some(function) = declaration.parent() else {
        return false;
    };
    if function.kind() == Kind::CatchClause {
        return true;
    }
    if declaration.kind() != Kind::Parameter
        || declaration.as_parameter_declaration().dot_dot_dot_token().is_some()
        || function.kind() != Kind::ArrowFunction
        || function.parameters().first() != Some(&declaration)
    {
        return false;
    }
    let Some(call) = function.parent().filter(|parent| parent.kind() == Kind::CallExpression)
    else {
        return false;
    };
    let call = call.as_call_expression();
    if call.expression.kind() != Kind::PropertyAccessExpression {
        return false;
    }
    let access = call.expression.as_property_access_expression();
    let arguments = call.arguments().nodes();
    let is_rejection_handler = match access.name().text() {
        "catch" => {
            arguments.first() == Some(&function) && arguments[0].kind() != Kind::SpreadElement
        }
        "then" => {
            arguments.get(1) == Some(&function)
                && arguments[0].kind() != Kind::SpreadElement
                && arguments[1].kind() != Kind::SpreadElement
        }
        _ => false,
    };
    if !is_rejection_handler {
        return false;
    }
    let object_type = context.checker.get_type_at_location(access.expression);
    utils::is_thenable_type(context.checker, access.expression, object_type)
}

impl OnlyThrowError {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("only-throw-error: options must be an object".into()),
        };
        let boolean = |name: &str| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(true),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("only-throw-error: {name} must be a boolean")),
        };
        let allow = match object.and_then(|object| object.get("allow")) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(values)) => {
                values.iter().map(TypeOrValueSpecifier::new).collect::<Result<_, _>>()?
            }
            _ => return Err("only-throw-error: allow must be an array".into()),
        };
        Ok(Self {
            allow,
            allow_rethrowing: boolean("allowRethrowing")?,
            allow_throwing_any: boolean("allowThrowingAny")?,
            allow_throwing_unknown: boolean("allowThrowingUnknown")?,
        })
    }
}

impl Rule for OnlyThrowError {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::ThrowStatement)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let expression = node.expression().expect("throw expression");
        let t = context.checker.get_type_at_location(expression);
        if type_matches_some_specifier(t, &self.allow, context.program) {
            return;
        }
        if t.flags().intersects(TypeFlags::Undefined) {
            context.report_node(node, MessageId::Undef.message());
            return;
        }
        if self.allow_throwing_any && t.flags().intersects(TypeFlags::Any) {
            return;
        }
        if self.allow_throwing_unknown && t.flags().intersects(TypeFlags::Unknown) {
            return;
        }
        if self.allow_rethrowing && is_rethrown_error(context, expression) {
            return;
        }
        if utils::is_builtin_symbol_like(context.program, context.checker, t, &["Error"]) {
            return;
        }
        context.report_node(expression, MessageId::Object.message());
    }
}
