use tsrs_ast::{self as ast, FunctionFlags, Kind, Node, NodeList, SourceFile};
use tsrs_core::P;
use tsrs_scanner::{self as scanner, SkipTriviaOptions};

use crate::protocol::{Diagnostic, Fix, Range, RuleMessage, Suggestion};
use crate::rule::{Listener, Rule, RuleContext};
use crate::utils::{
    get_function_head_loc, get_well_known_symbol_property_of_type, is_thenable_type, type_recurser,
};

fn build_missing_await_message() -> RuleMessage {
    RuleMessage {
        id: "missingAwait".into(),
        description: "Function has no 'await' expression.".into(),
        help: String::new(),
    }
}

fn build_remove_async_message() -> RuleMessage {
    RuleMessage {
        id: "removeAsync".into(),
        description: "Remove 'async'.".into(),
        help: String::new(),
    }
}

struct ScopeInfo {
    has_await: bool,
    is_async_yield: bool,
    function_flags: FunctionFlags,
    upper: Option<Box<ScopeInfo>>,
}

fn is_method_like(node: P<Node>) -> bool {
    ast::is_method_or_accessor(node) || ast::is_constructor_declaration(node)
}

fn previous_sibling_is_method_like(list: Option<P<NodeList>>, node: P<Node>) -> bool {
    let Some(list) = list else {
        return false;
    };
    list.nodes()
        .iter()
        .position(|&member| member == node)
        .is_some_and(|i| i > 0 && is_method_like(list.nodes()[i - 1]))
}

fn needs_preceding_semicolon(source_file: P<SourceFile>, node: P<Node>) -> bool {
    let pos = scanner::get_token_pos_of_node(node, source_file, false) as usize;
    // Pinned tsgolint's ASI check treats trailing comment bytes as preceding syntax.
    let text = source_file.text().as_bytes();
    let Some(last_pos) =
        text[..pos].iter().rposition(|c| !matches!(c, b' ' | b'\t' | b'\n' | b'\r'))
    else {
        return false;
    };
    let ch = text[last_pos];
    if matches!(ch, b';' | b':' | b'{' | b',' | b'(' | b'[') {
        return false;
    }
    if last_pos >= 1 && matches!(&text[last_pos - 1..=last_pos], b"=>" | b"++" | b"--") {
        return false;
    }
    if ch == b'}'
        && let Some(parent) = node.parent()
    {
        let list = match parent.kind() {
            Kind::ClassDeclaration | Kind::ClassExpression | Kind::InterfaceDeclaration => {
                parent.member_list()
            }
            Kind::ObjectLiteralExpression => Some(parent.as_object_literal_expression().properties),
            _ => None,
        };
        if previous_sibling_is_method_like(list, node) {
            return false;
        }
    }
    true
}

fn is_type_reference_named(type_ref: &ast::TypeReferenceNode, name: &str) -> bool {
    let tn = type_ref.type_name;
    if tn.kind() == Kind::Identifier {
        return tn.text() == name;
    }
    if tn.kind() != Kind::QualifiedName {
        return false;
    }
    let qualified_name = tn.as_qualified_name();
    qualified_name.right.text() == name
        && qualified_name.left.kind() == Kind::Identifier
        && qualified_name.left.text() == "globalThis"
}

fn build_remove_async_fixes(
    source_file: P<SourceFile>,
    node: P<Node>,
    async_token: P<Node>,
    is_generator: bool,
) -> Vec<Fix> {
    let mut fixes = Vec::with_capacity(3);
    let async_start = scanner::get_token_pos_of_node(async_token, source_file, false);
    let async_end = async_token.end().expect_source("parsed async modifier");
    let remove_end = scanner::skip_trivia_ex(
        source_file.text(),
        async_end,
        Some(&SkipTriviaOptions { stop_at_comments: true, ..Default::default() }),
    );
    let next_token = scanner::scan_token_at_position(source_file, async_end);
    let add_semicolon = matches!(
        next_token,
        Kind::OpenParenToken
            | Kind::OpenBracketToken
            | Kind::NoSubstitutionTemplateLiteral
            | Kind::TemplateHead
    ) && (is_at_start_of_expression_statement(node) || is_method_like(node))
        && needs_preceding_semicolon(source_file, node);
    fixes.push(Fix {
        range: Range { pos: async_start, end: remove_end },
        text: if add_semicolon { ";".into() } else { String::new() },
    });
    let Some(return_type) = node.type_node().filter(|t| t.kind() == Kind::TypeReference) else {
        return fixes;
    };
    let type_ref = return_type.as_type_reference_node();
    let type_name = type_ref.type_name;
    let type_name_start = scanner::get_token_pos_of_node(type_name, source_file, false);
    if is_generator && is_type_reference_named(type_ref, "AsyncGenerator") {
        fixes.push(Fix {
            range: Range {
                pos: type_name_start,
                end: type_name.end().expect_source("parsed type name"),
            },
            text: "Generator".into(),
        });
    } else if !is_generator
        && is_type_reference_named(type_ref, "Promise")
        && let Some(args) = type_ref.type_arguments().filter(|args| !args.nodes().is_empty())
    {
        let loc = args.loc.get();
        let open_angle_pos = loc.source_pos().unwrap() - 1;
        let close_angle_pos = loc.source_end().unwrap();
        fixes.push(Fix {
            range: Range { pos: close_angle_pos, end: close_angle_pos + 1 },
            text: String::new(),
        });
        fixes.push(Fix {
            range: Range { pos: type_name_start, end: open_angle_pos + 1 },
            text: String::new(),
        });
    }
    fixes
}

fn is_at_start_of_expression_statement(node: P<Node>) -> bool {
    let start = node.pos();
    let mut ancestor = node.parent();
    while let Some(a) = ancestor.filter(|a| a.pos() == start) {
        if a.kind() == Kind::ExpressionStatement {
            return true;
        }
        ancestor = a.parent();
    }
    false
}

#[derive(Default)]
pub struct RequireAwait {
    current_scope: Option<Box<ScopeInfo>>,
}

impl RequireAwait {
    fn enter_function(&mut self, node: P<Node>) {
        let function_flags = if node.body().is_some_and(|body| {
            body.kind() != Kind::Block || !body.as_block().statements.nodes().is_empty()
        }) {
            ast::get_function_flags(Some(node))
        } else {
            FunctionFlags::Normal
        };
        self.current_scope = Some(Box::new(ScopeInfo {
            has_await: false,
            is_async_yield: false,
            function_flags,
            upper: self.current_scope.take(),
        }));
    }

    fn exit_function(&mut self, node: P<Node>, context: &mut RuleContext<'_>) {
        let scope = self.current_scope.take().expect("function scope");
        let is_async = scope.function_flags.intersects(FunctionFlags::Async);
        let is_gen = scope.function_flags.intersects(FunctionFlags::Generator);
        if is_async && !scope.has_await && !(is_gen && scope.is_async_yield) {
            let suggestions = if context.options.fix_suggestions {
                let async_token = node
                    .modifiers()
                    .unwrap()
                    .nodes()
                    .iter()
                    .copied()
                    .find(|m| m.kind() == Kind::AsyncKeyword)
                    .expect("async function modifier");
                vec![Suggestion {
                    message: build_remove_async_message(),
                    fixes: build_remove_async_fixes(context.source_file, node, async_token, is_gen),
                }]
            } else {
                Vec::new()
            };
            context.report_diagnostic(Diagnostic {
                range: Some(get_function_head_loc(context.source_file, node)),
                message: build_missing_await_message(),
                suggestions,
                ..Default::default()
            });
        }
        self.current_scope = scope.upper;
    }

    fn mark_as_has_await(&mut self) {
        if let Some(scope) = &mut self.current_scope {
            scope.has_await = true;
        }
    }

    fn on_arrow_function(&mut self, node: P<Node>, context: &mut RuleContext<'_>) {
        self.enter_function(node);
        if !self.current_scope.as_ref().unwrap().function_flags.intersects(FunctionFlags::Async) {
            return;
        }
        let body = ast::skip_parentheses(node.body().unwrap());
        if matches!(body.kind(), Kind::Block | Kind::AwaitExpression) {
            return;
        }
        let t = context.checker.get_type_at_location(body);
        if is_thenable_type(context.checker, body, t) {
            self.mark_as_has_await();
        }
    }

    fn on_yield_expression(&mut self, node: P<Node>, context: &mut RuleContext<'_>) {
        let Some(scope) = &mut self.current_scope else {
            return;
        };
        if scope.is_async_yield {
            return;
        }
        let Some(argument) = node.expression() else {
            return;
        };
        if !scope.function_flags.intersects(FunctionFlags::Generator)
            || ast::is_literal_expression(argument)
        {
            return;
        }
        let t = context.checker.get_type_at_location(argument);
        if node.as_yield_expression().asterisk_token.is_none() {
            if is_thenable_type(context.checker, argument, t) {
                scope.is_async_yield = true;
            }
            return;
        }
        if type_recurser(t, &mut |t| {
            get_well_known_symbol_property_of_type(context.checker, t, "asyncIterator").is_some()
        }) {
            scope.is_async_yield = true;
        }
    }

    fn on_return_statement(&mut self, node: P<Node>, context: &mut RuleContext<'_>) {
        let Some(scope) = &self.current_scope else {
            return;
        };
        if scope.has_await || !scope.function_flags.intersects(FunctionFlags::Async) {
            return;
        }
        if let Some(expr) = node.expression() {
            let t = context.checker.get_type_at_location(expr);
            if is_thenable_type(context.checker, expr, t) {
                self.mark_as_has_await();
            }
        }
    }
}

impl Rule for RequireAwait {
    fn listens_to(&self, listener: Listener) -> bool {
        matches!(
            listener,
            Listener::Enter(
                Kind::FunctionDeclaration
                    | Kind::MethodDeclaration
                    | Kind::Constructor
                    | Kind::GetAccessor
                    | Kind::SetAccessor
                    | Kind::FunctionExpression
                    | Kind::ArrowFunction
                    | Kind::AwaitExpression
                    | Kind::ForOfStatement
                    | Kind::VariableDeclarationList
                    | Kind::YieldExpression
                    | Kind::ReturnStatement
            ) | Listener::Exit(
                Kind::FunctionDeclaration
                    | Kind::MethodDeclaration
                    | Kind::Constructor
                    | Kind::GetAccessor
                    | Kind::SetAccessor
                    | Kind::FunctionExpression
                    | Kind::ArrowFunction
            )
        )
    }

    fn visit(&mut self, listener: Listener, node: P<Node>, context: &mut RuleContext<'_>) {
        match listener {
            Listener::Exit(_) => self.exit_function(node, context),
            Listener::Enter(Kind::ArrowFunction) => self.on_arrow_function(node, context),
            Listener::Enter(Kind::AwaitExpression) => self.mark_as_has_await(),
            Listener::Enter(Kind::ForOfStatement) => {
                if node.as_for_in_or_of_statement().await_modifier.is_some() {
                    self.mark_as_has_await();
                }
            }
            Listener::Enter(Kind::VariableDeclarationList) => {
                if ast::is_var_await_using(node) {
                    self.mark_as_has_await();
                }
            }
            Listener::Enter(Kind::YieldExpression) => self.on_yield_expression(node, context),
            Listener::Enter(Kind::ReturnStatement) => self.on_return_statement(node, context),
            Listener::Enter(_) => self.enter_function(node),
            _ => {}
        }
    }
}
