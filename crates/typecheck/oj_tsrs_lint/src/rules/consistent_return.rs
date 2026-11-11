//! Port of tsgolint's `internal/rules/consistent_return/consistent_return.go`.

use rustc_hash::FxHashSet;

use tsrs_ast::{self as ast, FunctionFlags, Kind, Node, NodeFlags, SourceFile};
use tsrs_checker::{Checker, SignatureKind, Type, TypeFlags};
use tsrs_core::P;
use tsrs_core::json::Value;

use crate::protocol::RuleMessage;
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::{is_thenable_type, union_type_parts};

#[derive(Clone, Copy)]
enum MessageId {
    MissingReturnValue,
    UnexpectedReturnValue,
}

impl MessageId {
    fn message(self, function_name_with_kind: &str) -> RuleMessage {
        let (id, description) = match self {
            Self::MissingReturnValue => (
                "missingReturnValue",
                format!("{function_name_with_kind} expected a return value."),
            ),
            Self::UnexpectedReturnValue => (
                "unexpectedReturnValue",
                format!("{function_name_with_kind} expected no return value."),
            ),
        };
        RuleMessage { id: id.into(), description, help: String::new() }
    }
}

struct FunctionState {
    node: P<Node>,
    upper: Option<Box<FunctionState>>,
    has_return: bool,
    has_return_value: bool,
    has_mismatch: bool,
    allows_void_return: bool,
    function_name_with_kind: String,
    message_id: Option<MessageId>,
}

fn get_function_name_with_kind(source_file: P<SourceFile>, node: P<Node>) -> String {
    let prefix = if ast::get_function_flags(Some(node)).intersects(FunctionFlags::Async) {
        "Async function"
    } else {
        "Function"
    };
    let Some(name) = ast::get_name_of_declaration(node) else {
        return prefix.into();
    };
    let name_text = if name.kind() == Kind::Identifier {
        name.text()
    } else {
        let range = trim_node_range(source_file, name);
        source_file.text()[range.pos as usize..range.end as usize].trim()
    };
    if name_text.is_empty() { prefix.into() } else { format!("{prefix} '{name_text}'") }
}

fn is_thenable_type_with_void_value(
    checker: &mut Checker,
    node: P<Node>,
    t: P<Type>,
    visited: &mut FxHashSet<P<Type>>,
) -> bool {
    if !visited.insert(t) {
        return false;
    }
    if t.flags().intersects(TypeFlags::Void) {
        return true;
    }
    if t.flags().intersects(TypeFlags::Union | TypeFlags::Intersection) {
        return t
            .types()
            .iter()
            .any(|&part| is_thenable_type_with_void_value(checker, node, part, visited));
    }
    if !is_thenable_type(checker, node, t) {
        return false;
    }
    let Some(awaited) = checker.get_awaited_type(t) else {
        return false;
    };
    if awaited == t {
        return false;
    }
    is_thenable_type_with_void_value(checker, node, awaited, visited)
}

fn is_return_void_or_thenable_void(context: &mut RuleContext<'_>, node: P<Node>) -> bool {
    let t = context.checker.get_type_at_location(node);
    let signatures = context.checker.get_signatures_of_type(t, SignatureKind::Call);
    let is_async = ast::get_function_flags(Some(node)).intersects(FunctionFlags::Async);
    signatures.iter().any(|&signature| {
        let return_type = context.checker.get_return_type_of_signature(signature);
        if is_async {
            is_thenable_type_with_void_value(
                context.checker,
                node,
                return_type,
                &mut FxHashSet::default(),
            )
        } else {
            union_type_parts(&return_type)
                .iter()
                .any(|part| part.flags().intersects(TypeFlags::Void))
        }
    })
}

fn get_has_return_value(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    treat_undefined_as_unspecified: bool,
) -> bool {
    let Some(expression) = node.as_return_statement().expression() else {
        return false;
    };
    if !treat_undefined_as_unspecified {
        return true;
    }
    let t = context.checker.get_type_at_location(expression);
    if t.flags() == TypeFlags::Undefined {
        return false;
    }
    !(expression.kind() == Kind::VoidExpression
        || expression.kind() == Kind::Identifier && expression.text() == "undefined")
}

fn report_mismatched_return_statement(
    context: &mut RuleContext<'_>,
    node: P<Node>,
    state: &FunctionState,
) {
    if let Some(id) = state.message_id {
        context.report_node(node, id.message(&state.function_name_with_kind));
    }
}

pub struct ConsistentReturn {
    treat_undefined_as_unspecified: bool,
    current_function: Option<Box<FunctionState>>,
}

impl ConsistentReturn {
    pub fn new(options: &Value) -> Result<Self, String> {
        let treat_undefined_as_unspecified = match options {
            Value::Null => false,
            Value::Object(object) => match object.get("treatUndefinedAsUnspecified") {
                None | Some(Value::Null) => false,
                Some(Value::Bool(value)) => *value,
                _ => {
                    return Err(
                        "consistent-return: treatUndefinedAsUnspecified must be a boolean".into()
                    );
                }
            },
            _ => return Err("consistent-return: options must be an object".into()),
        };
        Ok(Self { treat_undefined_as_unspecified, current_function: None })
    }

    fn enter_function(&mut self, node: P<Node>, context: &mut RuleContext<'_>) {
        self.current_function = Some(Box::new(FunctionState {
            node,
            upper: self.current_function.take(),
            has_return: false,
            has_return_value: false,
            has_mismatch: false,
            allows_void_return: is_return_void_or_thenable_void(context, node),
            function_name_with_kind: get_function_name_with_kind(context.source_file, node),
            message_id: None,
        }));
    }

    fn exit_function(&mut self, context: &mut RuleContext<'_>) {
        if let Some(state) = self.current_function.take() {
            if !state.has_mismatch
                && state.has_return
                && state.has_return_value
                && !state.allows_void_return
                && state.node.flags().intersects(NodeFlags::HasImplicitReturn)
            {
                context.report_node(
                    state.node,
                    MessageId::MissingReturnValue.message(&state.function_name_with_kind),
                );
            }
            self.current_function = state.upper;
        }
    }

    fn on_return_statement(&mut self, node: P<Node>, context: &mut RuleContext<'_>) {
        let Some(current) = &mut self.current_function else {
            return;
        };
        if node.as_return_statement().expression().is_none() && current.allows_void_return {
            return;
        }
        let has_return_value =
            get_has_return_value(context, node, self.treat_undefined_as_unspecified);
        if !current.has_return {
            current.has_return = true;
            current.has_return_value = has_return_value;
            current.message_id = Some(if has_return_value {
                MessageId::MissingReturnValue
            } else {
                MessageId::UnexpectedReturnValue
            });
            return;
        }
        if current.has_return_value != has_return_value {
            current.has_mismatch = true;
            report_mismatched_return_statement(context, node, current);
        }
    }
}

impl Rule for ConsistentReturn {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::FunctionDeclaration
                    | Kind::FunctionExpression
                    | Kind::ArrowFunction
                    | Kind::MethodDeclaration
                    | Kind::Constructor
                    | Kind::GetAccessor
                    | Kind::SetAccessor
                    | Kind::ReturnStatement
            ) | Listener::Exit(
                Kind::FunctionDeclaration
                    | Kind::FunctionExpression
                    | Kind::ArrowFunction
                    | Kind::MethodDeclaration
                    | Kind::Constructor
                    | Kind::GetAccessor
                    | Kind::SetAccessor
            )
        )
    }

    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match listener {
            Listener::Enter(Kind::ReturnStatement) => self.on_return_statement(node, context),
            Listener::Enter(_) => self.enter_function(node, context),
            Listener::Exit(_) => self.exit_function(context),
            _ => {}
        }
    }
}
