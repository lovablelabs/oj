use tsrs_ast::{self as ast, Kind, ModifierFlags, Node, NodeFlags, OperatorPrecedenceFlags};
use tsrs_core::P;
use tsrs_core::json::Value;

use crate::protocol::{Fix, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::{self, TypeAwaitable};

#[derive(Clone, Copy)]
enum MessageId {
    DisallowedPromiseAwait,
    DisallowedPromiseAwaitSuggestion,
    NonPromiseAwait,
    RequiredPromiseAwait,
    RequiredPromiseAwaitSuggestion,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description) = match self {
            Self::DisallowedPromiseAwait => (
                "disallowedPromiseAwait",
                "Returning an awaited promise is not allowed in this context.",
            ),
            Self::DisallowedPromiseAwaitSuggestion => (
                "disallowedPromiseAwaitSuggestion",
                "Remove `await` before the expression. Use caution as this may impact control flow.",
            ),
            Self::NonPromiseAwait => (
                "nonPromiseAwait",
                "Returning an awaited value that is not a promise is not allowed.",
            ),
            Self::RequiredPromiseAwait => (
                "requiredPromiseAwait",
                "Returning an awaited promise is required in this context.",
            ),
            Self::RequiredPromiseAwaitSuggestion => (
                "requiredPromiseAwaitSuggestion",
                "Add `await` before the expression. Use caution as this may impact control flow.",
            ),
        };
        RuleMessage { id: id.into(), description: description.into(), help: String::new() }
    }
}

struct ScopeInfo {
    has_async: bool,
    owning_func: P<Node>,
    parent: Option<Box<ScopeInfo>>,
}

enum ContainingTryStatementBlock {
    Catch,
    Finally,
    Try,
}

enum WhetherToAwait {
    DontCare,
    Await,
    NoAwait,
}

enum ReturnAwaitOptions {
    Always,
    ErrorHandlingCorrectnessOnly,
    InTryCatch,
    Never,
}

fn get_whether_to_await(
    affects_error_handling: bool,
    option: &ReturnAwaitOptions,
) -> WhetherToAwait {
    match option {
        ReturnAwaitOptions::Always => WhetherToAwait::Await,
        ReturnAwaitOptions::ErrorHandlingCorrectnessOnly => {
            if affects_error_handling {
                WhetherToAwait::Await
            } else {
                WhetherToAwait::DontCare
            }
        }
        ReturnAwaitOptions::InTryCatch => {
            if affects_error_handling {
                WhetherToAwait::Await
            } else {
                WhetherToAwait::NoAwait
            }
        }
        ReturnAwaitOptions::Never => WhetherToAwait::NoAwait,
    }
}

pub struct ReturnAwait {
    options: ReturnAwaitOptions,
    scope: Option<Box<ScopeInfo>>,
}

impl ReturnAwait {
    pub fn new(options: &Value) -> Result<Self, String> {
        let options = match options {
            Value::Null => ReturnAwaitOptions::InTryCatch,
            Value::String(value) => match value.as_str() {
                "always" => ReturnAwaitOptions::Always,
                "error-handling-correctness-only" => {
                    ReturnAwaitOptions::ErrorHandlingCorrectnessOnly
                }
                "in-try-catch" => ReturnAwaitOptions::InTryCatch,
                "never" => ReturnAwaitOptions::Never,
                _ => return Err("return-await: invalid option".into()),
            },
            _ => return Err("return-await: options must be a string".into()),
        };
        Ok(Self { options, scope: None })
    }

    fn enter_function(&mut self, node: P<Node>) {
        self.scope = Some(Box::new(ScopeInfo {
            has_async: ast::has_syntactic_modifier(node, ModifierFlags::Async),
            owning_func: node,
            parent: self.scope.take(),
        }));
    }

    fn exit_function(&mut self) {
        self.scope = self.scope.take().expect("entered function scope").parent;
    }

    fn affects_explicit_resource_management(&self, node: P<Node>) -> bool {
        let scope = self.scope.as_ref().expect("async function scope");
        if scope.owning_func.body().is_none_or(|body| body.kind() != Kind::Block) {
            return false;
        }
        let mut declaration_scope = ast::get_enclosing_block_scope_container(node);
        while let Some(container) = declaration_scope {
            if let Some(locals) = container.locals() {
                for local in locals.values() {
                    if local.value_declaration().is_some_and(|declaration| {
                        declaration.kind() == Kind::VariableDeclaration
                            && declaration
                                .parent()
                                .expect("variable declaration list")
                                .flags()
                                .intersects(NodeFlags::Using)
                            && declaration.pos() < node.pos()
                    }) {
                        return true;
                    }
                }
            }
            if scope.owning_func == container {
                break;
            }
            declaration_scope = ast::get_enclosing_block_scope_container(container);
        }
        false
    }

    fn test(&self, node: P<Node>, context: &mut RuleContext<'_>) {
        let is_await = node.kind() == Kind::AwaitExpression;
        let child = if is_await { node.expression().unwrap() } else { node };
        let t = context.checker.get_type_at_location(child);
        let certainty = utils::needs_to_be_awaited(context.checker, node, t);
        if certainty != TypeAwaitable::Always {
            if is_await && certainty != TypeAwaitable::May {
                let fixes = remove_await_fix(node, context);
                context
                    .report_node_with_fixes(node, MessageId::NonPromiseAwait.message(), || fixes);
            }
            return;
        }
        let affects_error_handling = affects_explicit_error_handling(node)
            || self.affects_explicit_resource_management(node);
        let (id, suggestion_id, fixes) =
            match get_whether_to_await(affects_error_handling, &self.options) {
                WhetherToAwait::Await if !is_await => (
                    MessageId::RequiredPromiseAwait,
                    MessageId::RequiredPromiseAwaitSuggestion,
                    insert_await_fix(node, context),
                ),
                WhetherToAwait::NoAwait if is_await => (
                    MessageId::DisallowedPromiseAwait,
                    MessageId::DisallowedPromiseAwaitSuggestion,
                    remove_await_fix(node, context),
                ),
                _ => return,
            };
        if affects_error_handling {
            context.report_node_with_suggestions(node, id.message(), || {
                vec![Suggestion { message: suggestion_id.message(), fixes }]
            });
        } else {
            context.report_node_with_fixes(node, id.message(), || fixes);
        }
    }

    fn test_each_possibly_returned_node(&self, node: P<Node>, context: &mut RuleContext<'_>) {
        let node = ast::skip_parentheses(node);
        if node.kind() == Kind::ConditionalExpression {
            let expression = node.as_conditional_expression();
            self.test_each_possibly_returned_node(expression.when_false, context);
            self.test_each_possibly_returned_node(expression.when_true, context);
        } else {
            self.test(node, context);
        }
    }
}

fn find_containing_try_statement(node: P<Node>) -> Option<(ContainingTryStatementBlock, P<Node>)> {
    let mut child = node;
    let mut ancestor = node.parent();
    while let Some(current) = ancestor {
        if ast::is_function_like(current) {
            break;
        }
        if current.kind() != Kind::TryStatement {
            child = current;
            ancestor = current.parent();
            continue;
        }
        let statement = current.as_try_statement();
        let block = if child == statement.try_block {
            ContainingTryStatementBlock::Try
        } else if Some(child) == statement.catch_clause {
            ContainingTryStatementBlock::Catch
        } else if Some(child) == statement.finally_block {
            ContainingTryStatementBlock::Finally
        } else {
            ContainingTryStatementBlock::Catch
        };
        return Some((block, current));
    }
    None
}

fn affects_explicit_error_handling(mut node: P<Node>) -> bool {
    loop {
        let Some((block, statement)) = find_containing_try_statement(node) else {
            return false;
        };
        match block {
            ContainingTryStatementBlock::Catch => {
                if statement.as_try_statement().finally_block.is_some() {
                    return true;
                }
                node = statement;
            }
            ContainingTryStatementBlock::Finally => node = statement,
            ContainingTryStatementBlock::Try => return true,
        }
    }
}

fn remove_await_fix(node: P<Node>, context: &RuleContext<'_>) -> Vec<Fix> {
    let range = utils::get_await_token_removal_range(
        context.source_file,
        node.pos().expect_source("parsed await expression"),
    );
    let mut fixes = vec![Fix { text: String::new(), range }];
    let operand = node.expression().unwrap();
    let first_token = tsrs_scanner::scan_token_at_position(
        context.source_file,
        operand.pos().expect_source("parsed await operand"),
    );
    if utils::is_start_of_arrow_function_body_needing_parentheses(
        context.source_file,
        node,
        first_token,
    ) {
        let range = trim_node_range(context.source_file, operand);
        fixes.extend([
            Fix { text: "(".into(), range: Range { pos: range.pos, end: range.pos } },
            Fix { text: ")".into(), range: Range { pos: range.end, end: range.end } },
        ]);
    }
    fixes
}

fn insert_await_fix(node: P<Node>, context: &RuleContext<'_>) -> Vec<Fix> {
    let range = trim_node_range(context.source_file, node);
    let is_high_precedence = ast::get_expression_precedence(node)
        > ast::get_operator_precedence(
            Kind::AwaitExpression,
            Kind::Unknown,
            OperatorPrecedenceFlags::None,
        );
    let mut fixes = vec![Fix {
        text: if is_high_precedence { "await " } else { "await (" }.into(),
        range: Range { pos: range.pos, end: range.pos },
    }];
    if !is_high_precedence {
        fixes.push(Fix { text: ")".into(), range: Range { pos: range.end, end: range.end } });
    }
    fixes
}

impl Rule for ReturnAwait {
    fn listens_to(&self, listener: Listener) -> bool {
        match listener {
            Listener::Enter(Kind::ReturnStatement) => true,
            Listener::Enter(kind) | Listener::Exit(kind) => matches!(
                kind,
                Kind::ArrowFunction
                    | Kind::FunctionDeclaration
                    | Kind::FunctionExpression
                    | Kind::MethodDeclaration
                    | Kind::Constructor
                    | Kind::GetAccessor
                    | Kind::SetAccessor
            ),
            _ => false,
        }
    }

    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match listener {
            Listener::Enter(Kind::ReturnStatement) => {
                if self.scope.as_ref().is_some_and(|scope| scope.has_async)
                    && let Some(expression) = node.as_return_statement().expression()
                {
                    self.test_each_possibly_returned_node(expression, context);
                }
            }
            Listener::Enter(_) => self.enter_function(node),
            Listener::Exit(_) => {
                if node.kind() == Kind::ArrowFunction
                    && self.scope.as_ref().is_some_and(|scope| scope.has_async)
                {
                    let body = node.body().expect("arrow function body");
                    if body.kind() != Kind::Block {
                        self.test_each_possibly_returned_node(body, context);
                    }
                }
                self.exit_function();
            }
            _ => {}
        }
    }
}
