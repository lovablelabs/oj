#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

use tsrs_ast::{self as ast, Kind, Node};
use tsrs_core::{P, Tristate};
use tsrs_scanner as scanner;

use super::*;
use crate::astnav;
use crate::lsutil::{self, FormatCodeSettings, SemicolonPreference};

//
// Contexts
//

// rulecontext.go:18
pub(crate) type OptionSelector = fn(&FormatCodeSettings) -> Tristate;

// rulecontext.go:22
pub(crate) fn semicolon_option(options: &FormatCodeSettings) -> SemicolonPreference {
    options.semicolons
}

// rulecontext.go:26
pub(crate) fn insert_space_after_comma_delimiter_option(options: &FormatCodeSettings) -> Tristate {
    options.insert_space_after_comma_delimiter
}

// rulecontext.go:30
pub(crate) fn insert_space_after_semicolon_in_for_statements_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.insert_space_after_semicolon_in_for_statements
}

// rulecontext.go:34
pub(crate) fn insert_space_before_and_after_binary_operators_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.insert_space_before_and_after_binary_operators
}

// rulecontext.go:38
pub(crate) fn insert_space_after_constructor_option(options: &FormatCodeSettings) -> Tristate {
    options.insert_space_after_constructor
}

// rulecontext.go:42
pub(crate) fn insert_space_after_keywords_in_control_flow_statements_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.insert_space_after_keywords_in_control_flow_statements
}

// rulecontext.go:46
pub(crate) fn insert_space_after_function_keyword_for_anonymous_functions_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.insert_space_after_function_keyword_for_anonymous_functions
}

// rulecontext.go:50
pub(crate) fn insert_space_after_opening_and_before_closing_nonempty_parenthesis_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.insert_space_after_opening_and_before_closing_nonempty_parenthesis
}

// rulecontext.go:54
pub(crate) fn insert_space_after_opening_and_before_closing_nonempty_brackets_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.insert_space_after_opening_and_before_closing_nonempty_brackets
}

// rulecontext.go:58
pub(crate) fn insert_space_after_opening_and_before_closing_nonempty_braces_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.insert_space_after_opening_and_before_closing_nonempty_braces
}

// rulecontext.go:62
pub(crate) fn insert_space_after_opening_and_before_closing_empty_braces_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.insert_space_after_opening_and_before_closing_empty_braces
}

// rulecontext.go:66
pub(crate) fn insert_space_after_opening_and_before_closing_template_string_braces_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.insert_space_after_opening_and_before_closing_template_string_braces
}

// rulecontext.go:70
pub(crate) fn insert_space_after_opening_and_before_closing_jsx_expression_braces_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.insert_space_after_opening_and_before_closing_jsx_expression_braces
}

// rulecontext.go:74
pub(crate) fn insert_space_after_type_assertion_option(options: &FormatCodeSettings) -> Tristate {
    options.insert_space_after_type_assertion
}

// rulecontext.go:78
pub(crate) fn insert_space_before_function_parenthesis_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.insert_space_before_function_parenthesis
}

// rulecontext.go:82
pub(crate) fn place_open_brace_on_new_line_for_functions_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.place_open_brace_on_new_line_for_functions
}

// rulecontext.go:86
pub(crate) fn place_open_brace_on_new_line_for_control_blocks_option(
    options: &FormatCodeSettings,
) -> Tristate {
    options.place_open_brace_on_new_line_for_control_blocks
}

// rulecontext.go:90
pub(crate) fn insert_space_before_type_annotation_option(options: &FormatCodeSettings) -> Tristate {
    options.insert_space_before_type_annotation
}

// rulecontext.go:102
pub(crate) fn option_equals<T: PartialEq + Copy + Send + Sync + 'static>(
    option_name: fn(&FormatCodeSettings) -> T,
    option_value: T,
) -> ContextPredicate {
    Box::leak(Box::new(move |context: &FormattingContext| {
        option_name(&context.options) == option_value
    }))
}

// rulecontext.go:108
pub(crate) fn is_option_enabled(option_name: OptionSelector) -> ContextPredicate {
    Box::leak(Box::new(move |context: &FormattingContext| option_name(&context.options).is_true()))
}

// rulecontext.go:114
pub(crate) fn is_option_disabled(option_name: OptionSelector) -> ContextPredicate {
    Box::leak(Box::new(move |context: &FormattingContext| option_name(&context.options).is_false()))
}

// rulecontext.go:120
pub(crate) fn is_option_disabled_or_undefined(option_name: OptionSelector) -> ContextPredicate {
    Box::leak(Box::new(move |context: &FormattingContext| {
        option_name(&context.options).is_false_or_unknown()
    }))
}

// rulecontext.go:126
pub(crate) fn is_option_disabled_or_undefined_or_tokens_on_same_line(
    option_name: OptionSelector,
) -> ContextPredicate {
    Box::leak(Box::new(move |context: &FormattingContext| {
        option_name(&context.options).is_false_or_unknown() || context.tokens_are_on_same_line()
    }))
}

// rulecontext.go:132
pub(crate) fn is_option_enabled_or_undefined(option_name: OptionSelector) -> ContextPredicate {
    Box::leak(Box::new(move |context: &FormattingContext| {
        option_name(&context.options).is_true_or_unknown()
    }))
}

// rulecontext.go:138
pub(crate) fn is_for_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::ForStatement
}

// rulecontext.go:142
pub(crate) fn is_not_for_context(context: &FormattingContext) -> bool {
    !is_for_context(context)
}

// rulecontext.go:146
pub(crate) fn is_binary_op_context(context: &FormattingContext) -> bool {
    match context.context_node().kind() {
        Kind::BinaryExpression => {
            context.context_node().as_binary_expression().operator_token.kind() != Kind::CommaToken
        }
        Kind::ConditionalExpression
        | Kind::ConditionalType
        | Kind::AsExpression
        | Kind::ExportSpecifier
        | Kind::ImportSpecifier
        | Kind::TypePredicate
        | Kind::UnionType
        | Kind::IntersectionType
        | Kind::SatisfiesExpression => true,

        // equals in binding elements func foo([[x, y] = [1, 2]])
        // equals in type X = ...
        // equal in import a = module('a');
        // equal in export = 1
        // equal in let a = 0
        // equal in p = 0
        Kind::BindingElement
        | Kind::TypeAliasDeclaration
        | Kind::ImportEqualsDeclaration
        | Kind::ExportAssignment
        | Kind::VariableDeclaration
        | Kind::Parameter
        | Kind::EnumMember
        | Kind::PropertyDeclaration
        | Kind::PropertySignature => {
            context.current_token_span.kind == Kind::EqualsToken
                || context.next_token_span.kind == Kind::EqualsToken
        }
        // "in" keyword in for (let x in []) { }
        // "in" keyword in [P in keyof T] T[P]
        Kind::ForInStatement | Kind::TypeParameter => {
            context.current_token_span.kind == Kind::InKeyword
                || context.next_token_span.kind == Kind::InKeyword
                || context.current_token_span.kind == Kind::EqualsToken
                || context.next_token_span.kind == Kind::EqualsToken
        }
        // Technically, "of" is not a binary operator, but format it the same way as "in"
        Kind::ForOfStatement => {
            context.current_token_span.kind == Kind::OfKeyword
                || context.next_token_span.kind == Kind::OfKeyword
        }
        _ => false,
    }
}

// rulecontext.go:195
pub(crate) fn is_not_binary_op_context(context: &FormattingContext) -> bool {
    !is_binary_op_context(context)
}

// rulecontext.go:199
pub(crate) fn is_not_type_annotation_context(context: &FormattingContext) -> bool {
    !is_type_annotation_context(context)
}

// rulecontext.go:203
pub(crate) fn is_type_annotation_context(context: &FormattingContext) -> bool {
    let context_kind = context.context_node().kind();
    context_kind == Kind::PropertyDeclaration
        || context_kind == Kind::PropertySignature
        || context_kind == Kind::Parameter
        || context_kind == Kind::VariableDeclaration
        || ast::is_function_like_kind(context_kind)
}

// rulecontext.go:212
fn is_optional_property_context(context: &FormattingContext) -> bool {
    ast::is_property_declaration(context.context_node())
        && ast::has_question_token(context.context_node())
}

// rulecontext.go:216
pub(crate) fn is_non_optional_property_context(context: &FormattingContext) -> bool {
    !is_optional_property_context(context)
}

// rulecontext.go:220
pub(crate) fn is_conditional_operator_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::ConditionalExpression
        || context.context_node().kind() == Kind::ConditionalType
}

// rulecontext.go:225
pub(crate) fn is_same_line_token_or_before_block_context(context: &FormattingContext) -> bool {
    context.tokens_are_on_same_line() || is_before_block_context(context)
}

// rulecontext.go:229
pub(crate) fn is_brace_wrapped_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::ObjectBindingPattern
        || context.context_node().kind() == Kind::MappedType
        || is_single_line_block_context(context)
}

// rulecontext.go:236
// This check is done before an open brace in a control construct, a function, or a typescript block declaration
pub(crate) fn is_before_multiline_block_context(context: &FormattingContext) -> bool {
    is_before_block_context(context)
        && !(context.next_node_all_on_same_line() || context.next_node_block_is_on_one_line())
}

// rulecontext.go:240
pub(crate) fn is_multiline_block_context(context: &FormattingContext) -> bool {
    is_block_context(context)
        && !(context.context_node_all_on_same_line() || context.context_node_block_is_on_one_line())
}

// rulecontext.go:244
fn is_single_line_block_context(context: &FormattingContext) -> bool {
    is_block_context(context)
        && (context.context_node_all_on_same_line() || context.context_node_block_is_on_one_line())
}

// rulecontext.go:248
fn is_block_context(context: &FormattingContext) -> bool {
    node_is_block_context(context.context_node())
}

// rulecontext.go:252
pub(crate) fn is_before_block_context(context: &FormattingContext) -> bool {
    node_is_block_context(context.next_token_parent())
}

// rulecontext.go:257
// IMPORTANT!!! This method must return true ONLY for nodes with open and close braces as immediate children
fn node_is_block_context(node: P<Node>) -> bool {
    if node_is_type_script_decl_with_block_context(node) {
        // This means we are in a context that looks like a block to the user, but in the grammar is actually not a node (it's a class, module, enum, object type literal, etc).
        return true;
    }

    matches!(
        node.kind(),
        Kind::Block | Kind::CaseBlock | Kind::ObjectLiteralExpression | Kind::ModuleBlock
    )
}

// rulecontext.go:274
pub(crate) fn is_function_decl_context(context: &FormattingContext) -> bool {
    matches!(
        context.context_node().kind(),
        Kind::FunctionDeclaration
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            // case ast.KindMemberFunctionDeclaration:
            | Kind::GetAccessor
            | Kind::SetAccessor
            // case ast.KindMethodSignature:
            | Kind::CallSignature
            | Kind::FunctionExpression
            | Kind::Constructor
            | Kind::ArrowFunction
            // case ast.KindConstructorDeclaration:
            // case ast.KindSimpleArrowFunctionExpression:
            // case ast.KindParenthesizedArrowFunctionExpression:
            | Kind::InterfaceDeclaration // This one is not truly a function, but for formatting purposes, it acts just like one
    )
}

// rulecontext.go:300
pub(crate) fn is_not_function_decl_context(context: &FormattingContext) -> bool {
    !is_function_decl_context(context)
}

// rulecontext.go:304
pub(crate) fn is_function_declaration_or_function_expression_context(
    context: &FormattingContext,
) -> bool {
    context.context_node().kind() == Kind::FunctionDeclaration
        || context.context_node().kind() == Kind::FunctionExpression
}

// rulecontext.go:308
pub(crate) fn is_type_script_decl_with_block_context(context: &FormattingContext) -> bool {
    node_is_type_script_decl_with_block_context(context.context_node())
}

// rulecontext.go:312
fn node_is_type_script_decl_with_block_context(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::ClassDeclaration
            | Kind::ClassExpression
            | Kind::InterfaceDeclaration
            | Kind::EnumDeclaration
            | Kind::TypeLiteral
            | Kind::ModuleDeclaration
            | Kind::ExportDeclaration
            | Kind::NamedExports
            | Kind::ImportDeclaration
            | Kind::NamedImports
    )
}

// rulecontext.go:330
pub(crate) fn is_after_code_block_context(context: &FormattingContext) -> bool {
    match context.current_token_parent().kind() {
        Kind::ClassDeclaration
        | Kind::ModuleDeclaration
        | Kind::EnumDeclaration
        | Kind::CatchClause
        | Kind::ModuleBlock
        | Kind::SwitchStatement => return true,
        Kind::Block => {
            let block_parent = context.current_token_parent().parent();
            // In a codefix scenario, we can't rely on parents being set. So just always return true.
            if block_parent.is_none_or(|p| {
                p.kind() != Kind::ArrowFunction && p.kind() != Kind::FunctionExpression
            }) {
                return true;
            }
        }
        _ => {}
    }
    false
}

// rulecontext.go:349
pub(crate) fn is_control_decl_context(context: &FormattingContext) -> bool {
    matches!(
        context.context_node().kind(),
        Kind::IfStatement
            | Kind::SwitchStatement
            | Kind::ForStatement
            | Kind::ForInStatement
            | Kind::ForOfStatement
            | Kind::WhileStatement
            | Kind::TryStatement
            | Kind::DoStatement
            | Kind::WithStatement
            // TODO
            // case ast.KindElseClause:
            | Kind::CatchClause
    )
}

// rulecontext.go:371
pub(crate) fn is_object_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::ObjectLiteralExpression
}

// rulecontext.go:375
fn is_function_call_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::CallExpression
}

// rulecontext.go:379
fn is_new_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::NewExpression
}

// rulecontext.go:383
pub(crate) fn is_function_call_or_new_context(context: &FormattingContext) -> bool {
    is_function_call_context(context) || is_new_context(context)
}

// rulecontext.go:387
pub(crate) fn is_previous_token_not_comma(context: &FormattingContext) -> bool {
    context.current_token_span.kind != Kind::CommaToken
}

// rulecontext.go:391
pub(crate) fn is_next_token_not_close_bracket(context: &FormattingContext) -> bool {
    context.next_token_span.kind != Kind::CloseBracketToken
}

// rulecontext.go:395
pub(crate) fn is_next_token_not_close_paren(context: &FormattingContext) -> bool {
    context.next_token_span.kind != Kind::CloseParenToken
}

// rulecontext.go:399
pub(crate) fn is_arrow_function_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::ArrowFunction
}

// rulecontext.go:403
pub(crate) fn is_import_type_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::ImportType
}

// rulecontext.go:407
pub(crate) fn is_non_jsx_same_line_token_context(context: &FormattingContext) -> bool {
    context.tokens_are_on_same_line() && context.context_node().kind() != Kind::JsxText
}

// rulecontext.go:411
pub(crate) fn is_non_jsx_text_context(context: &FormattingContext) -> bool {
    context.context_node().kind() != Kind::JsxText
}

// rulecontext.go:415
pub(crate) fn is_non_jsx_element_or_fragment_context(context: &FormattingContext) -> bool {
    context.context_node().kind() != Kind::JsxElement
        && context.context_node().kind() != Kind::JsxFragment
}

// rulecontext.go:419
pub(crate) fn is_jsx_expression_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::JsxExpression
        || context.context_node().kind() == Kind::JsxSpreadAttribute
}

// rulecontext.go:423
pub(crate) fn is_next_token_parent_jsx_attribute(context: &FormattingContext) -> bool {
    context.next_token_parent().kind() == Kind::JsxAttribute
        || (context.next_token_parent().kind() == Kind::JsxNamespacedName
            && context.next_token_parent().parent().unwrap().kind() == Kind::JsxAttribute)
}

// rulecontext.go:427
pub(crate) fn is_jsx_attribute_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::JsxAttribute
}

// rulecontext.go:431
pub(crate) fn is_next_token_parent_not_jsx_namespaced_name(context: &FormattingContext) -> bool {
    context.next_token_parent().kind() != Kind::JsxNamespacedName
}

// rulecontext.go:435
pub(crate) fn is_next_token_parent_jsx_namespaced_name(context: &FormattingContext) -> bool {
    context.next_token_parent().kind() == Kind::JsxNamespacedName
}

// rulecontext.go:439
pub(crate) fn is_jsx_self_closing_element_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::JsxSelfClosingElement
}

// rulecontext.go:443
pub(crate) fn is_not_before_block_in_function_declaration_context(
    context: &FormattingContext,
) -> bool {
    !is_function_decl_context(context) && !is_before_block_context(context)
}

// rulecontext.go:447
pub(crate) fn is_end_of_decorator_context_on_same_line(context: &FormattingContext) -> bool {
    context.tokens_are_on_same_line()
        && ast::has_decorators(context.context_node())
        && node_is_in_decorator_context(Some(context.current_token_parent()))
        && !node_is_in_decorator_context(Some(context.next_token_parent()))
}

// rulecontext.go:454
fn node_is_in_decorator_context(mut node: Option<P<Node>>) -> bool {
    while let Some(n) = node {
        if !ast::is_expression(n) {
            break;
        }
        node = n.parent();
    }
    node.is_some_and(|n| n.kind() == Kind::Decorator)
}

// rulecontext.go:461
pub(crate) fn is_start_of_variable_declaration_list(context: &FormattingContext) -> bool {
    context.current_token_parent().kind() == Kind::VariableDeclarationList
        && scanner::get_token_pos_of_node(
            context.current_token_parent(),
            context.source_file,
            false,
        ) == context
            .current_token_span
            .loc
            .pos()
            .expect_source("formatter token spans have source positions")
}

// rulecontext.go:466
pub(crate) fn is_not_format_on_enter(context: &FormattingContext) -> bool {
    context.formatting_request_kind != FormatRequestKind::FormatOnEnter
}

// rulecontext.go:470
pub(crate) fn is_module_decl_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::ModuleDeclaration
}

// rulecontext.go:474
pub(crate) fn is_object_type_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::TypeLiteral // && context.contextNode.parent.Kind != ast.KindInterfaceDeclaration;
}

// rulecontext.go:478
pub(crate) fn is_constructor_signature_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::ConstructSignature
}

// rulecontext.go:482
fn is_type_argument_or_parameter_or_assertion(token: TextRangeWithKind, parent: P<Node>) -> bool {
    if token.kind != Kind::LessThanToken && token.kind != Kind::GreaterThanToken {
        return false;
    }
    matches!(
        parent.kind(),
        Kind::TypeReference
            | Kind::TypeAssertionExpression
            | Kind::TypeAliasDeclaration
            | Kind::ClassDeclaration
            | Kind::ClassExpression
            | Kind::InterfaceDeclaration
            | Kind::FunctionDeclaration
            | Kind::FunctionExpression
            | Kind::ArrowFunction
            | Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::CallSignature
            | Kind::ConstructSignature
            | Kind::CallExpression
            | Kind::NewExpression
            | Kind::ExpressionWithTypeArguments
    )
}

// rulecontext.go:509
pub(crate) fn is_type_argument_or_parameter_or_assertion_context(
    context: &FormattingContext,
) -> bool {
    is_type_argument_or_parameter_or_assertion(
        context.current_token_span,
        context.current_token_parent(),
    ) || is_type_argument_or_parameter_or_assertion(
        context.next_token_span,
        context.next_token_parent(),
    )
}

// rulecontext.go:514
pub(crate) fn is_type_assertion_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::TypeAssertionExpression
}

// rulecontext.go:518
pub(crate) fn is_non_type_assertion_context(context: &FormattingContext) -> bool {
    !is_type_assertion_context(context)
}

// rulecontext.go:522
pub(crate) fn is_void_op_context(context: &FormattingContext) -> bool {
    context.current_token_span.kind == Kind::VoidKeyword
        && context.current_token_parent().kind() == Kind::VoidExpression
}

// rulecontext.go:526
pub(crate) fn is_yield_or_yield_star_with_operand(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::YieldExpression
        && context.context_node().expression().is_some()
}

// rulecontext.go:530
pub(crate) fn is_non_null_assertion_context(context: &FormattingContext) -> bool {
    context.context_node().kind() == Kind::NonNullExpression
}

// rulecontext.go:534
pub(crate) fn is_not_statement_condition_context(context: &FormattingContext) -> bool {
    !is_statement_condition_context(context)
}

// rulecontext.go:538
fn is_statement_condition_context(context: &FormattingContext) -> bool {
    matches!(
        context.context_node().kind(),
        Kind::IfStatement
            | Kind::ForStatement
            | Kind::ForInStatement
            | Kind::ForOfStatement
            | Kind::DoStatement
            | Kind::WhileStatement
    )
}

// rulecontext.go:553
pub(crate) fn is_semicolon_deletion_context(context: &FormattingContext) -> bool {
    let mut next_token_kind = context.next_token_span.kind;
    let mut next_token_start = context
        .next_token_span
        .loc
        .pos()
        .expect_source("formatter token spans have source positions");
    if ast::is_trivia(next_token_kind) {
        let next_real_token = if context.next_token_parent == context.current_token_parent {
            // !!! TODO: very different from strada, but strada's logic here is wonky - find the first ancestor without a parent? that's just the source file.
            astnav::find_next_token(
                context.next_token_parent(),
                context.source_file.as_node(),
                context.source_file,
            )
        } else {
            lsutil::get_first_token(context.next_token_parent(), context.source_file)
        };

        let Some(next_real_token) = next_real_token else {
            return true;
        };
        next_token_kind = next_real_token.kind();
        next_token_start =
            scanner::get_token_pos_of_node(next_real_token, context.source_file, false);
    }

    let start_line = scanner::get_ecma_line_of_position(
        context.source_file.get(),
        context
            .current_token_span
            .loc
            .pos()
            .expect_source("formatter token spans have source positions"),
    );
    let end_line = scanner::get_ecma_line_of_position(context.source_file.get(), next_token_start);
    if start_line == end_line {
        return next_token_kind == Kind::CloseBraceToken || next_token_kind == Kind::EndOfFile;
    }

    if next_token_kind == Kind::SemicolonToken
        && context.current_token_span.kind == Kind::SemicolonToken
    {
        return true;
    }

    if next_token_kind == Kind::SemicolonClassElement || next_token_kind == Kind::SemicolonToken {
        return false;
    }

    if context.context_node().kind() == Kind::InterfaceDeclaration
        || context.context_node().kind() == Kind::TypeAliasDeclaration
    {
        // Can't remove semicolon after `foo`; it would parse as a method declaration:
        //
        // interface I {
        //   foo;
        //   () void
        // }
        return context.current_token_parent().kind() != Kind::PropertySignature
            || context.current_token_parent().type_node().is_some()
            || next_token_kind != Kind::OpenParenToken;
    }

    if ast::is_property_declaration(context.current_token_parent()) {
        return context.current_token_parent().initializer().is_none();
    }

    context.current_token_parent().kind() != Kind::ForStatement
        && context.current_token_parent().kind() != Kind::EmptyStatement
        && context.current_token_parent().kind() != Kind::SemicolonClassElement
        && next_token_kind != Kind::OpenBracketToken
        && next_token_kind != Kind::OpenParenToken
        && next_token_kind != Kind::PlusToken
        && next_token_kind != Kind::MinusToken
        && next_token_kind != Kind::SlashToken
        && next_token_kind != Kind::RegularExpressionLiteral
        && next_token_kind != Kind::CommaToken
        && next_token_kind != Kind::TemplateExpression
        && next_token_kind != Kind::TemplateHead
        && next_token_kind != Kind::NoSubstitutionTemplateLiteral
        && next_token_kind != Kind::DotToken
}

// rulecontext.go:621
pub(crate) fn is_semicolon_insertion_context(context: &FormattingContext) -> bool {
    lsutil::position_is_asi_candidate(
        context
            .current_token_span
            .loc
            .end()
            .expect_source("formatter token spans have source positions"),
        context.current_token_parent(),
        context.source_file,
    )
}

// rulecontext.go:625
pub(crate) fn is_not_property_access_on_integer_literal(context: &FormattingContext) -> bool {
    !ast::is_property_access_expression(context.context_node())
        || !ast::is_numeric_literal(context.context_node().expression().unwrap())
        || context.context_node().expression().unwrap().text().contains('.')
}
