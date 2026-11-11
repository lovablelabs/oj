//! Port of tsgolint's `internal/rules/await_thenable/await_thenable.go`.

use tsrs_ast::{self as ast, Kind, Node};
use tsrs_checker::{Checker, ObjectFlags, Type, TypeFlags};
use tsrs_core::{P, json::Value};

use crate::protocol::{Diagnostic, Fix, LabeledRange, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext, trim_node_range};
use crate::utils::{self, TypeAwaitable, union_type_parts};

pub struct AwaitThenable {
    allow_mixed_promise_arrays: bool,
}

impl AwaitThenable {
    pub fn new(options: &Value) -> Result<Self, String> {
        let object = match options {
            Value::Null => None,
            Value::Object(object) => Some(object),
            _ => return Err("await-thenable: options must be an object".into()),
        };
        let allow_mixed_promise_arrays =
            match object.and_then(|object| object.get("allowMixedPromiseArrays")) {
                None | Some(Value::Null) => false,
                Some(Value::Bool(value)) => *value,
                _ => {
                    return Err("await-thenable: allowMixedPromiseArrays must be a boolean".into());
                }
            };
        Ok(Self { allow_mixed_promise_arrays })
    }
}

#[derive(Clone, Copy)]
enum MessageId {
    Await,
    RemoveAwait,
    ForAwaitOfNonAsyncIterable,
    ConvertToOrdinaryFor,
    AwaitUsingOfNonAsyncDisposable,
    InvalidPromiseAggregatorInput,
}

impl MessageId {
    fn message(self) -> RuleMessage {
        let (id, description, help) = match self {
            Self::Await => (
                "await",
                "Unexpected `await` of a non-Promise (non-\"Thenable\") value.",
                "Remove `await` if the value is synchronous, or change the expression to return a Promise or Thenable before awaiting it.",
            ),
            Self::RemoveAwait => ("removeAwait", "Remove unnecessary `await`.", ""),
            Self::ForAwaitOfNonAsyncIterable => (
                "forAwaitOfNonAsyncIterable",
                "Unexpected `for await...of` of a value that is not async iterable.",
                "Use `for...of` for synchronous iterables, or change the iterable to implement `Symbol.asyncIterator`.",
            ),
            Self::ConvertToOrdinaryFor => {
                ("convertToOrdinaryFor", "Convert to an ordinary `for...of` loop.", "")
            }
            Self::AwaitUsingOfNonAsyncDisposable => (
                "awaitUsingOfNonAsyncDisposable",
                "Unexpected `await using` of a value that is not async disposable.",
                "Use plain `using` for synchronous disposables, or change the value to implement `Symbol.asyncDispose`.",
            ),
            Self::InvalidPromiseAggregatorInput => (
                "invalidPromiseAggregatorInput",
                "Unexpected iterable of non-Promise (non-\"Thenable\") values passed to promise aggregator.",
                "Pass an iterable of Promise-like values, or wrap each synchronous value in `Promise.resolve(...)` before calling the aggregator.",
            ),
        };
        RuleMessage { id: id.into(), description: description.into(), help: help.into() }
    }

    fn label(self) -> &'static str {
        match self {
            Self::ForAwaitOfNonAsyncIterable => "This value is not async iterable",
            Self::AwaitUsingOfNonAsyncDisposable => "This value is not async disposable",
            _ => "This expression is not Promise-like",
        }
    }
}

fn diagnostic(range: Range, id: MessageId, label_range: Range) -> Diagnostic {
    Diagnostic {
        range: Some(range),
        message: id.message(),
        labeled_ranges: vec![LabeledRange { label: id.label().into(), range: label_range }],
        ..Default::default()
    }
}

fn token_range(context: &RuleContext<'_>, node: P<Node>) -> Range {
    let range = tsrs_scanner::get_range_of_token_at_position(
        context.source_file,
        node.pos().expect_source("parsed lint node"),
    );
    Range { pos: range.source_pos().unwrap(), end: range.source_end().unwrap() }
}

fn suggest(context: &RuleContext<'_>, diagnostic: &mut Diagnostic, id: MessageId, range: Range) {
    if context.options.fix_suggestions {
        diagnostic.suggestions.push(Suggestion {
            message: id.message(),
            fixes: vec![Fix { text: String::new(), range }],
        });
    }
}

fn needs_preceding_semicolon(context: &RuleContext<'_>, node: P<Node>) -> bool {
    let start = trim_node_range(context.source_file, node).pos;
    let mut current = Some(node);
    while let Some(statement) = current {
        if trim_node_range(context.source_file, statement).pos != start {
            break;
        }
        if statement.kind() == Kind::ExpressionStatement {
            let Some(parent) = statement.parent().filter(|parent| parent.can_have_statements())
            else {
                return false;
            };
            let statements = parent.statements();
            let Ok(index) = statements.binary_search_by_key(
                &statement.pos().expect_source("parsed statement"),
                |statement| statement.pos().expect_source("parsed statement"),
            ) else {
                return false;
            };
            if index == 0 {
                return false;
            }
            let previous = statements[index - 1];
            let end = previous.end().expect_source("parsed statement");
            return end > previous.pos().expect_source("parsed statement")
                && context.source_file.text().as_bytes()[end as usize - 1] != b';';
        }
        current = statement.parent();
    }
    false
}

fn await_suggestion_fixes(context: &RuleContext<'_>, node: P<Node>, argument: P<Node>) -> Vec<Fix> {
    let mut fixes = vec![Fix {
        text: String::new(),
        range: utils::get_await_token_removal_range(
            context.source_file,
            node.pos().expect_source("parsed await"),
        ),
    }];
    let mut scanner = tsrs_scanner::get_scanner_for_source_file(
        context.source_file,
        argument.pos().expect_source("parsed await argument"),
    );
    let first_token = scanner.token();
    let statement_start_token = if first_token == Kind::AsyncKeyword
        && scanner.scan() == Kind::FunctionKeyword
        && !scanner.has_preceding_line_break()
    {
        Kind::FunctionKeyword
    } else {
        first_token
    };
    let mut starts_with_parenthesis = first_token == Kind::OpenParenToken;
    if utils::is_start_of_arrow_function_body_needing_parentheses(
        context.source_file,
        node,
        first_token,
    ) || utils::is_start_of_expression_statement_needing_parentheses(
        context.source_file,
        node,
        statement_start_token,
    ) {
        let range = trim_node_range(context.source_file, argument);
        fixes.extend([
            Fix { text: "(".into(), range: Range { pos: range.pos, end: range.pos } },
            Fix { text: ")".into(), range: Range { pos: range.end, end: range.end } },
        ]);
        starts_with_parenthesis = true;
    }
    if starts_with_parenthesis && needs_preceding_semicolon(context, node) {
        fixes[0].text = ";".into();
    }
    fixes
}

impl Rule for AwaitThenable {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::AwaitExpression
                    | Kind::CallExpression
                    | Kind::ForOfStatement
                    | Kind::VariableDeclarationList
            )
        )
    }

    fn visit(&mut self, _: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match node.kind() {
            Kind::AwaitExpression => {
                let argument = node.as_await_expression().expression;
                let t = context.checker.get_type_at_location(argument);
                if utils::needs_to_be_awaited(context.checker, argument, t) == TypeAwaitable::Never
                {
                    let range = token_range(context, node);
                    let mut diagnostic = diagnostic(
                        range,
                        MessageId::Await,
                        trim_node_range(context.source_file, argument),
                    );
                    if context.options.fix_suggestions {
                        diagnostic.suggestions.push(Suggestion {
                            message: MessageId::RemoveAwait.message(),
                            fixes: await_suggestion_fixes(context, node, argument),
                        });
                    }
                    context.report_diagnostic(diagnostic);
                }
            }
            Kind::CallExpression => {
                let expression = node.as_call_expression();
                let Some(&argument) = expression.arguments.nodes().first() else {
                    return;
                };
                let callee = ast::skip_parentheses(expression.expression);
                if !ast::is_access_expression(callee) {
                    return;
                }
                let (name, ok) = context.checker.get_accessed_property_name(callee);
                if !ok || !["all", "allSettled", "any", "race"].contains(&name.as_ref()) {
                    return;
                }
                let t = utils::get_constrained_type_at_location(
                    context.checker,
                    callee.expression().unwrap(),
                );
                if !utils::is_builtin_symbol_like(
                    context.program,
                    context.checker,
                    t,
                    &["PromiseConstructor"],
                ) {
                    return;
                }
                if self.allow_mixed_promise_arrays
                    && has_maybe_awaitable_value(context.checker, argument)
                {
                    return;
                }
                if argument.kind() == Kind::ArrayLiteralExpression {
                    for &element in argument.elements() {
                        if element.kind() == Kind::OmittedExpression {
                            continue;
                        }
                        let t = utils::get_constrained_type_at_location(context.checker, element);
                        if is_always_non_awaitable_type(context.checker, element, t) {
                            let range = trim_node_range(context.source_file, element);
                            let mut diagnostic =
                                diagnostic(range, MessageId::InvalidPromiseAggregatorInput, range);
                            diagnostic.labeled_ranges.push(LabeledRange {
                                label: "Promise aggregator input".into(),
                                range: trim_node_range(context.source_file, argument),
                            });
                            context.report_diagnostic(diagnostic);
                        }
                    }
                    return;
                }
                let t = utils::get_constrained_type_at_location(context.checker, argument);
                if is_invalid_promise_aggregator_input(context.checker, argument, t) {
                    let range = trim_node_range(context.source_file, argument);
                    context.report_diagnostic(diagnostic(
                        range,
                        MessageId::InvalidPromiseAggregatorInput,
                        range,
                    ));
                }
            }
            Kind::ForOfStatement => {
                let statement = node.as_for_in_or_of_statement();
                let Some(await_modifier) = statement.await_modifier else {
                    return;
                };
                let t = context.checker.get_type_at_location(statement.expression);
                if t.flags().intersects(TypeFlags::Any) {
                    return;
                }
                if union_type_parts(&t).iter().any(|&part| {
                    utils::get_well_known_symbol_property_of_type(
                        context.checker,
                        part,
                        "asyncIterator",
                    )
                    .is_some()
                }) {
                    return;
                }
                let mut head = trim_node_range(context.source_file, node);
                head.end = statement.statement.pos().expect_source("parsed loop body");
                let mut diagnostic = diagnostic(
                    head,
                    MessageId::ForAwaitOfNonAsyncIterable,
                    trim_node_range(context.source_file, statement.expression),
                );
                suggest(
                    context,
                    &mut diagnostic,
                    MessageId::ConvertToOrdinaryFor,
                    utils::get_await_token_removal_range(
                        context.source_file,
                        await_modifier.pos().expect_source("parsed await modifier"),
                    ),
                );
                context.report_diagnostic(diagnostic);
            }
            Kind::VariableDeclarationList => {
                if !ast::is_var_await_using(node) {
                    return;
                }
                let declarations = node.as_variable_declaration_list().declarations.nodes();
                for &declaration in declarations {
                    let Some(initializer) = declaration.initializer() else {
                        continue;
                    };
                    let t = context.checker.get_type_at_location(initializer);
                    if t.flags().intersects(TypeFlags::Any) {
                        continue;
                    }
                    if union_type_parts(&t).iter().any(|&part| {
                        utils::get_well_known_symbol_property_of_type(
                            context.checker,
                            part,
                            "asyncDispose",
                        )
                        .is_some()
                    }) {
                        continue;
                    }
                    let range = trim_node_range(context.source_file, initializer);
                    let mut diagnostic =
                        diagnostic(range, MessageId::AwaitUsingOfNonAsyncDisposable, range);
                    if declarations.len() == 1 {
                        suggest(
                            context,
                            &mut diagnostic,
                            MessageId::RemoveAwait,
                            utils::get_await_token_removal_range(
                                context.source_file,
                                node.pos().expect_source("parsed await using"),
                            ),
                        );
                    }
                    context.report_diagnostic(diagnostic);
                }
            }
            _ => {}
        }
    }
}

fn is_invalid_promise_aggregator_input(checker: &mut Checker, node: P<Node>, t: P<Type>) -> bool {
    if !is_iterable(checker, t) {
        return false;
    }
    for &part in union_type_parts(&t) {
        for value_type in get_value_types_of_array_like(checker, part) {
            if contains_non_awaitable_type(checker, node, value_type) {
                return true;
            }
        }
    }
    false
}

fn has_maybe_awaitable_value(checker: &mut Checker, node: P<Node>) -> bool {
    let t = utils::get_constrained_type_at_location(checker, node);
    for &part in union_type_parts(&t) {
        for value_type in get_value_types_of_array_like(checker, part) {
            if !is_always_non_awaitable_type(checker, node, value_type) {
                return true;
            }
        }
    }
    false
}

fn get_value_types_of_array_like(checker: &mut Checker, t: P<Type>) -> Vec<P<Type>> {
    if t.is_tuple_type() {
        return checker.get_type_arguments(t).to_vec();
    }
    if let Some(index) = checker.get_index_type_of_type(t, checker.number_type) {
        return vec![index];
    }
    if !t.object_flags().intersects(ObjectFlags::Reference) {
        return Vec::new();
    }
    checker.get_type_arguments(t).first().copied().into_iter().collect()
}

fn is_always_non_awaitable_type(checker: &mut Checker, node: P<Node>, t: P<Type>) -> bool {
    union_type_parts(&t)
        .iter()
        .all(|&part| utils::needs_to_be_awaited(checker, node, part) == TypeAwaitable::Never)
}

fn contains_non_awaitable_type(checker: &mut Checker, node: P<Node>, t: P<Type>) -> bool {
    union_type_parts(&t)
        .iter()
        .any(|&part| utils::needs_to_be_awaited(checker, node, part) == TypeAwaitable::Never)
}

fn is_iterable(checker: &mut Checker, t: P<Type>) -> bool {
    union_type_parts(&t).iter().all(|&part| {
        utils::get_well_known_symbol_property_of_type(checker, part, "iterator").is_some()
    })
}
