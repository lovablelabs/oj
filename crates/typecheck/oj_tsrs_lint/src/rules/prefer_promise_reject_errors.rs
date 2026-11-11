use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::TypeFlags;
use tsrs_core::{P, json::Value};

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext};
use crate::type_matches_specifier::{TypeOrValueSpecifier, type_matches_some_specifier};
use crate::utils;

pub struct PreferPromiseRejectErrors {
    allow: Vec<TypeOrValueSpecifier>,
    allow_empty_reject: bool,
    allow_throwing_any: bool,
    allow_throwing_unknown: bool,
}

fn build_reject_an_error_message() -> RuleMessage {
    RuleMessage {
        id: "rejectAnError".into(),
        description: "Expected the Promise rejection reason to be an Error.".into(),
        help: String::new(),
    }
}

impl PreferPromiseRejectErrors {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("prefer-promise-reject-errors: options must be an object".into()),
        };
        let boolean = |name: &str| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("prefer-promise-reject-errors: {name} must be a boolean")),
        };
        let allow = match object.and_then(|object| object.get("allow")) {
            None | Some(Value::Null) => Vec::new(),
            Some(Value::Array(values)) => {
                values.iter().map(TypeOrValueSpecifier::new).collect::<Result<_, _>>()?
            }
            _ => return Err("prefer-promise-reject-errors: allow must be an array".into()),
        };
        Ok(Self {
            allow,
            allow_empty_reject: boolean("allowEmptyReject")?,
            allow_throwing_any: boolean("allowThrowingAny")?,
            allow_throwing_unknown: boolean("allowThrowingUnknown")?,
        })
    }

    fn check_reject_call(&self, node: P<Node>, context: &mut RuleContext<'_>) {
        if let Some(&argument) = node.as_call_expression().arguments().nodes().first() {
            let t = context.checker.get_type_at_location(argument);
            if type_matches_some_specifier(t, &self.allow, context.program) {
                return;
            }
            if self.allow_throwing_any && t.flags().intersects(TypeFlags::Any) {
                return;
            }
            if self.allow_throwing_unknown && t.flags().intersects(TypeFlags::Unknown) {
                return;
            }
            if utils::is_builtin_symbol_like(context.program, context.checker, t, &["Error"])
                || utils::is_readonly_error_like(context.program, context.checker, t)
            {
                return;
            }
        } else if self.allow_empty_reject {
            return;
        }
        context.report_node(node, build_reject_an_error_message());
    }

    fn type_at_location_is_like_promise(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
        let t = context.checker.get_type_at_location(node);
        utils::is_builtin_symbol_like(context.program, context.checker, t, &["PromiseConstructor"])
            || utils::is_builtin_symbol_like(context.program, context.checker, t, &["Promise"])
    }
}

impl Rule for PreferPromiseRejectErrors {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::CallExpression)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        let callee = ast::skip_parentheses(node.as_call_expression().expression);
        if ast::is_access_expression(callee) {
            let (method_name, _) = context.checker.get_accessed_property_name(callee);
            if method_name == "reject"
                && Self::type_at_location_is_like_promise(
                    context,
                    callee.expression().expect("access expression receiver"),
                )
            {
                self.check_reject_call(node, context);
            }
        } else if callee.kind() == Kind::Identifier {
            let Some(param) = context
                .checker
                .get_symbol_at_location_exported(callee)
                .and_then(|symbol| symbol.value_declaration())
                .filter(|param| param.kind() == Kind::Parameter)
            else {
                return;
            };
            let Some(mut parent_node) = param.parent().filter(|parent| {
                matches!(parent.kind(), Kind::FunctionExpression | Kind::ArrowFunction)
            }) else {
                return;
            };
            if parent_node.parameters().get(1) != Some(&param) {
                return;
            }
            loop {
                let Some(parent) = parent_node.parent() else {
                    return;
                };
                parent_node = parent;
                if parent_node.kind() == Kind::ParenthesizedExpression {
                    continue;
                }
                if parent_node.kind() != Kind::NewExpression {
                    return;
                }
                break;
            }
            let t = context.checker.get_type_at_location(
                parent_node.expression().expect("new expression constructor"),
            );
            if !utils::is_builtin_symbol_like(
                context.program,
                context.checker,
                t,
                &["PromiseConstructor"],
            ) {
                return;
            }
            self.check_reject_call(node, context);
        }
    }
}
