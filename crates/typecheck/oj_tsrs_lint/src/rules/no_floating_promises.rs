//! Port of tsgolint's `internal/rules/no_floating_promises/no_floating_promises.go`.

use tsrs_ast::{self as ast, Kind, Node, OperatorPrecedence, OperatorPrecedenceFlags, Symbol};
use tsrs_checker::{self as checker, SignatureKind, Type};
use tsrs_core::{P, json::Value};

use crate::protocol::{Diagnostic, Fix, LabeledRange, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::type_matches_specifier::{
    TypeOrValueSpecifier, type_matches_some_specifier, value_matches_some_specifier,
};
use crate::utils::{is_builtin_symbol_like, union_type_parts};

pub struct NoFloatingPromises {
    allow_for_known_safe_calls: Vec<TypeOrValueSpecifier>,
    allow_for_known_safe_promises: Vec<TypeOrValueSpecifier>,
    check_thenables: bool,
    ignore_iife: bool,
    ignore_void: bool,
}

struct UnhandledPromise {
    node: P<Node>,
    ty: P<Type>,
    promise_array: bool,
    non_function_handler: Option<P<Node>>,
}

#[derive(Clone, Copy)]
enum MessageId {
    Floating,
    FloatingFixAwait,
    FloatingFixVoid,
    FloatingPromiseArray,
    FloatingPromiseArrayVoid,
    FloatingUselessRejectionHandler,
    FloatingUselessRejectionHandlerVoid,
    FloatingVoid,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description, help) = match self {
            Self::Floating => (
                "floating",
                "Promises must be awaited, add await operator.",
                "The promise must end with a call to .catch, or end with a call to .then with a rejection handler.",
            ),
            Self::FloatingFixAwait => ("floatingFixAwait", "Add await operator.", ""),
            Self::FloatingFixVoid => ("floatingFixVoid", "Add void operator to ignore.", ""),
            Self::FloatingPromiseArray => (
                "floatingPromiseArray",
                "An array of Promises may be unintentional.",
                "Consider handling the promises' fulfillment or rejection with Promise.all or similar.",
            ),
            Self::FloatingPromiseArrayVoid => (
                "floatingPromiseArrayVoid",
                "An array of Promises may be unintentional.",
                "Consider handling the promises' fulfillment or rejection with Promise.all or similar, or explicitly marking the expression as ignored with the `void` operator.",
            ),
            Self::FloatingUselessRejectionHandler => (
                "floatingUselessRejectionHandler",
                "Promises must be awaited, add await operator.",
                "The promise must end with a call to .catch, or end with a call to .then with a rejection handler. A rejection handler that is not a function will be ignored.",
            ),
            Self::FloatingUselessRejectionHandlerVoid => (
                "floatingUselessRejectionHandlerVoid",
                "Promises must be awaited, add void operator to ignore.",
                "The promise must end with a call to .catch, or end with a call to .then with a rejection handler, or be explicitly marked as ignored with the `void` operator. A rejection handler that is not a function will be ignored.",
            ),
            Self::FloatingVoid => (
                "floatingVoid",
                "Promises must be awaited, add void operator to ignore.",
                "The promise must end with a call to .catch, or end with a call to .then with a rejection handler, or be explicitly marked as ignored with the `void` operator.",
            ),
        };
        RuleMessage { id: id.into(), description: description.into(), help: help.into() }
    }
}

fn is_higher_precedence_than_unary(node: P<Node>) -> bool {
    let operator = if node.kind() == Kind::BinaryExpression {
        node.as_binary_expression().operator_token.kind()
    } else {
        Kind::Unknown
    };
    ast::get_operator_precedence(node.kind(), operator, OperatorPrecedenceFlags::None)
        > OperatorPrecedence::Unary
}

fn insert_before(context: &RuleContext<'_>, node: P<Node>, text: &str) -> Fix {
    let pos = trim_node_range(context.source_file, node).pos;
    Fix { range: Range { pos, end: pos }, text: text.into() }
}

fn insert_after(node: P<Node>, text: &str) -> Fix {
    let end = node.end().expect_source("parsed lint node");
    Fix { range: Range { pos: end, end }, text: text.into() }
}

fn add_await(context: &RuleContext<'_>, expression: P<Node>, statement: P<Node>) -> Vec<Fix> {
    if expression.kind() == Kind::VoidExpression {
        let range = tsrs_scanner::get_range_of_token_at_position(
            context.source_file,
            expression.pos().expect_source("parsed lint node"),
        );
        return vec![Fix {
            range: Range { pos: range.source_pos().unwrap(), end: range.source_end().unwrap() },
            text: "await".into(),
        }];
    }
    if is_higher_precedence_than_unary(statement.expression().unwrap()) {
        return vec![insert_before(context, statement, "await ")];
    }
    vec![insert_before(context, statement, "await ("), insert_after(expression, ")")]
}

fn is_function_param(context: &mut RuleContext<'_>, parameter: P<Symbol>, node: P<Node>) -> bool {
    let t = context
        .checker
        .get_type_of_symbol_at_location(parameter, Some(node))
        .expect("signature parameter type");
    let t = context.checker.get_apparent_type(t);
    union_type_parts(&t)
        .iter()
        .any(|&part| !context.checker.get_signatures_of_type(part, SignatureKind::Call).is_empty())
}

fn is_async_iife(statement: P<Node>) -> bool {
    let expression = statement.expression().unwrap();
    if expression.kind() != Kind::CallExpression {
        return false;
    }
    let callee = ast::skip_parentheses(expression.as_call_expression().expression);
    matches!(callee.kind(), Kind::ArrowFunction | Kind::FunctionExpression)
}

fn is_valid_rejection_handler(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let t = context.checker.get_type_at_location(node);
    !context.checker.get_signatures_of_type(t, SignatureKind::Call).is_empty()
}

fn is_known_argument_at(arguments: &[P<Node>], index: usize) -> bool {
    arguments.len() > index
        && arguments[..=index].iter().all(|node| node.kind() != Kind::SpreadElement)
}

impl NoFloatingPromises {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("no-floating-promises: options must be an object".into()),
        };
        let boolean = |name: &str, default| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(default),
            Some(Value::Bool(value)) => Ok(*value),
            _ => Err(format!("no-floating-promises: {name} must be a boolean")),
        };
        let specifiers = |name: &str| match object.and_then(|object| object.get(name)) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(values)) => {
                values.iter().map(TypeOrValueSpecifier::new).collect::<Result<_, _>>()
            }
            _ => Err(format!("no-floating-promises: {name} must be an array")),
        };
        Ok(Self {
            allow_for_known_safe_calls: specifiers("allowForKnownSafeCalls")?,
            allow_for_known_safe_promises: specifiers("allowForKnownSafePromises")?,
            check_thenables: boolean("checkThenables", false)?,
            ignore_iife: boolean("ignoreIIFE", false)?,
            ignore_void: boolean("ignoreVoid", true)?,
        })
    }

    fn is_promise_like(&self, context: &mut RuleContext<'_>, node: P<Node>, t: P<Type>) -> bool {
        if type_matches_some_specifier(t, &self.allow_for_known_safe_promises, context.program) {
            return false;
        }
        let apparent = context.checker.get_apparent_type(t);
        let parts = union_type_parts(&apparent);
        if parts.iter().any(|&part| {
            is_builtin_symbol_like(context.program, context.checker, part, &["Promise"])
        }) {
            return true;
        }
        if !self.check_thenables {
            return false;
        }
        for &part in parts {
            let Some(then) = context.checker.get_property_of_type(part, "then") else {
                continue;
            };
            let then_type = context
                .checker
                .get_type_of_symbol_at_location(then, Some(node))
                .expect("then property type");
            for &part in union_type_parts(&then_type) {
                for &signature in context.checker.get_signatures_of_type(part, SignatureKind::Call)
                {
                    let parameters = signature.parameters();
                    if parameters.len() >= 2
                        && is_function_param(context, parameters[0], node)
                        && is_function_param(context, parameters[1], node)
                    {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn is_promise_array(&self, context: &mut RuleContext<'_>, node: P<Node>, t: P<Type>) -> bool {
        for &part in union_type_parts(&t) {
            let apparent = context.checker.get_apparent_type(part);
            if context.checker.is_array_type(apparent) {
                let element = context.checker.get_type_arguments(apparent)[0];
                if self.is_promise_like(context, node, element) {
                    return true;
                }
            }
            if checker::is_tuple_type_exported(apparent) {
                for &element in context.checker.get_type_arguments(apparent) {
                    if self.is_promise_like(context, node, element) {
                        return true;
                    }
                }
            }
        }
        false
    }

    fn is_known_safe_promise_return(&self, context: &mut RuleContext<'_>, node: P<Node>) -> bool {
        if self.allow_for_known_safe_calls.is_empty() || node.kind() != Kind::CallExpression {
            return false;
        }
        let callee = node.as_call_expression().expression;
        let t = context.checker.get_type_at_location(callee);
        value_matches_some_specifier(
            callee,
            &self.allow_for_known_safe_calls,
            context.program,
            context.checker,
        ) || type_matches_some_specifier(t, &self.allow_for_known_safe_calls, context.program)
    }

    fn is_unhandled_promise(
        &self,
        context: &mut RuleContext<'_>,
        node: P<Node>,
    ) -> Option<UnhandledPromise> {
        if ast::is_assignment_expression(node, false) {
            return None;
        }
        if ast::is_comma_expression(node) {
            let expression = node.as_binary_expression();
            return self
                .is_unhandled_promise(context, expression.left)
                .or_else(|| self.is_unhandled_promise(context, expression.right()));
        }
        if !self.ignore_void && node.kind() == Kind::VoidExpression {
            return self.is_unhandled_promise(context, node.expression().unwrap());
        }
        let ty = context.checker.get_type_at_location(node);
        let mut result =
            UnhandledPromise { node, ty, promise_array: false, non_function_handler: None };
        if self.is_promise_array(context, node, ty) {
            result.promise_array = true;
            return Some(result);
        }
        if node.kind() == Kind::AwaitExpression || !self.is_promise_like(context, node, ty) {
            return None;
        }
        if node.kind() == Kind::CallExpression {
            let call = node.as_call_expression();
            let callee = call.expression;
            if ast::is_access_expression(callee) {
                let (name, _) = context.checker.get_accessed_property_name(callee);
                let arguments = call.arguments.nodes();
                let handler = match name.as_ref() {
                    "catch" if !arguments.is_empty() => Some(0),
                    "then" if arguments.len() >= 2 => Some(1),
                    _ => None,
                };
                if let Some(index) = handler {
                    if !is_known_argument_at(arguments, index) {
                        return Some(result);
                    }
                    if is_valid_rejection_handler(context, arguments[index]) {
                        return None;
                    }
                    result.non_function_handler = Some(arguments[index]);
                    return Some(result);
                }
                if name == "finally" {
                    let mut result =
                        self.is_unhandled_promise(context, callee.expression().unwrap())?;
                    result.node = node;
                    result.ty = ty;
                    return Some(result);
                }
            }
            return Some(result);
        }
        if node.kind() == Kind::ConditionalExpression {
            let expression = node.as_conditional_expression();
            return self
                .is_unhandled_promise(context, expression.when_false)
                .or_else(|| self.is_unhandled_promise(context, expression.when_true));
        }
        if ast::is_logical_or_coalescing_binary_expression(node) {
            let expression = node.as_binary_expression();
            return self
                .is_unhandled_promise(context, expression.left)
                .or_else(|| self.is_unhandled_promise(context, expression.right()));
        }
        Some(result)
    }
}

impl Rule for NoFloatingPromises {
    fn listens_to(&self, listener: Listener) -> bool {
        listener == Listener::Enter(Kind::ExpressionStatement)
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        if self.ignore_iife && is_async_iife(node) {
            return;
        }
        let expression = ast::skip_parentheses(node.expression().unwrap());
        if self.is_known_safe_promise_return(context, expression) {
            return;
        }
        let Some(result) = self.is_unhandled_promise(context, expression) else {
            return;
        };
        let range = trim_node_range(context.source_file, result.node);
        let description = if result.promise_array {
            "This array contains Promises and"
        } else {
            "This unhandled promise-like value"
        };
        let Ok(type_name) =
            String::from_utf8(context.checker.type_to_string_bytes_exported(result.ty))
        else {
            return;
        };
        let mut labels =
            vec![LabeledRange { range, label: format!("{description} has type `{type_name}`.") }];
        if let Some(handler) = result.non_function_handler {
            let ty = context.checker.get_type_at_location(handler);
            let Ok(type_name) =
                String::from_utf8(context.checker.type_to_string_bytes_exported(ty))
            else {
                return;
            };
            labels.push(LabeledRange {
                range: trim_node_range(context.source_file, handler),
                label: format!(
                    "This rejection handler has type `{type_name}`, which is not callable."
                ),
            });
        }
        if result.promise_array {
            context.report_diagnostic(Diagnostic {
                range: Some(range),
                message: if self.ignore_void {
                    MessageId::FloatingPromiseArrayVoid
                } else {
                    MessageId::FloatingPromiseArray
                }
                .message(),
                labeled_ranges: labels,
                ..Default::default()
            });
            return;
        }
        let id = match (self.ignore_void, result.non_function_handler.is_some()) {
            (true, true) => MessageId::FloatingUselessRejectionHandlerVoid,
            (true, false) => MessageId::FloatingVoid,
            (false, true) => MessageId::FloatingUselessRejectionHandler,
            (false, false) => MessageId::Floating,
        };
        let suggestions = if context.options.fix_suggestions {
            let mut suggestions = Vec::new();
            if self.ignore_void {
                let fixes = if is_higher_precedence_than_unary(node.expression().unwrap()) {
                    vec![insert_before(context, node, "void ")]
                } else {
                    vec![insert_before(context, node, "void ("), insert_after(expression, ")")]
                };
                suggestions
                    .push(Suggestion { message: MessageId::FloatingFixVoid.message(), fixes });
            }
            suggestions.push(Suggestion {
                message: MessageId::FloatingFixAwait.message(),
                fixes: add_await(context, expression, node),
            });
            suggestions
        } else {
            Vec::new()
        };
        context.report_diagnostic(Diagnostic {
            range: Some(range),
            labeled_ranges: labels,
            message: id.message(),
            suggestions,
            ..Default::default()
        });
    }
}
