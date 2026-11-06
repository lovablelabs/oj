use tsrs_ast::{self as ast, Kind, Node, SourceFile};
use tsrs_core::P;
use tsrs_scanner as scanner;

use super::*;
use crate::astnav;

// completednode.go:12
// PositionBelongsToNode returns true if the position belongs to the node.
// Assumes `candidate.Pos() <= position` holds.
#[expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]
pub(crate) fn position_belongs_to_node(
    candidate: P<Node>,
    position: u32,
    file: P<SourceFile>,
) -> bool {
    let candidate_pos = candidate.pos().expect_source("parsed nodes have source positions");
    assert!(candidate_pos <= position, "Expected candidate.pos <= position");
    position < candidate.end().expect_source("parsed nodes have source positions")
        || !is_completed_node(Some(candidate), file)
}

// completednode.go:19
fn is_completed_node(n: Option<P<Node>>, source_file: P<SourceFile>) -> bool {
    let Some(n) = n else {
        return false;
    };
    if ast::node_is_missing(n) {
        return false;
    }

    match n.kind() {
        Kind::ClassDeclaration
        | Kind::InterfaceDeclaration
        | Kind::EnumDeclaration
        | Kind::ObjectLiteralExpression
        | Kind::ObjectBindingPattern
        | Kind::TypeLiteral
        | Kind::Block
        | Kind::ModuleBlock
        | Kind::CaseBlock
        | Kind::NamedImports
        | Kind::NamedExports => node_ends_with(n, Kind::CloseBraceToken, source_file),

        Kind::CatchClause => is_completed_node(Some(n.as_catch_clause().block), source_file),

        Kind::NewExpression
        | Kind::CallExpression
        | Kind::ParenthesizedExpression
        | Kind::ParenthesizedType => {
            if n.kind() == Kind::NewExpression && n.argument_list().is_none() {
                return true;
            }
            node_ends_with(n, Kind::CloseParenToken, source_file)
        }

        Kind::FunctionType | Kind::ConstructorType => is_completed_node(n.type_node(), source_file),

        Kind::Constructor
        | Kind::GetAccessor
        | Kind::SetAccessor
        | Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::MethodDeclaration
        | Kind::MethodSignature
        | Kind::ConstructSignature
        | Kind::CallSignature
        | Kind::ArrowFunction => {
            if n.body().is_some() {
                return is_completed_node(n.body(), source_file);
            }
            if n.type_node().is_some() {
                return is_completed_node(n.type_node(), source_file);
            }
            // Even though type parameters can be unclosed, we can get away with
            // having at least a closing paren.
            has_child_of_kind(n, Kind::CloseParenToken, source_file)
        }

        Kind::ModuleDeclaration => n.body().is_some() && is_completed_node(n.body(), source_file),

        Kind::IfStatement => {
            if n.as_if_statement().else_statement.is_some() {
                return is_completed_node(n.as_if_statement().else_statement, source_file);
            }
            is_completed_node(Some(n.as_if_statement().then_statement), source_file)
        }

        Kind::ExpressionStatement => {
            is_completed_node(n.expression(), source_file)
                || has_child_of_kind(n, Kind::SemicolonToken, source_file)
        }

        Kind::ArrayLiteralExpression
        | Kind::ArrayBindingPattern
        | Kind::ElementAccessExpression
        | Kind::ComputedPropertyName
        | Kind::TupleType => node_ends_with(n, Kind::CloseBracketToken, source_file),

        Kind::IndexSignature => {
            if n.as_index_signature_declaration().type_().is_some() {
                return is_completed_node(n.as_index_signature_declaration().type_(), source_file);
            }
            has_child_of_kind(n, Kind::CloseBracketToken, source_file)
        }

        // there is no such thing as terminator token for CaseClause/DefaultClause so for simplicity always consider them non-completed
        Kind::CaseClause | Kind::DefaultClause => false,

        Kind::ForStatement | Kind::ForInStatement | Kind::ForOfStatement | Kind::WhileStatement => {
            is_completed_node(Some(n.statement()), source_file)
        }
        Kind::DoStatement => {
            // rough approximation: if DoStatement has While keyword - then if node is completed is checking the presence of ')';
            if has_child_of_kind(n, Kind::WhileKeyword, source_file) {
                return node_ends_with(n, Kind::CloseParenToken, source_file);
            }
            is_completed_node(Some(n.statement()), source_file)
        }

        Kind::TypeQuery => is_completed_node(Some(n.as_type_query_node().expr_name), source_file),

        Kind::TypeOfExpression
        | Kind::DeleteExpression
        | Kind::VoidExpression
        | Kind::YieldExpression
        | Kind::SpreadElement => is_completed_node(n.expression(), source_file),

        Kind::TaggedTemplateExpression => {
            is_completed_node(Some(n.as_tagged_template_expression().template), source_file)
        }

        Kind::TemplateExpression => {
            let last_span =
                tsrs_core::last_or_nil(n.as_template_expression().template_spans.nodes());
            is_completed_node(last_span, source_file)
        }

        Kind::TemplateSpan => ast::node_is_present(n.as_template_span().literal),

        Kind::ExportDeclaration | Kind::ImportDeclaration => {
            ast::node_is_present(n.module_specifier())
        }

        Kind::PrefixUnaryExpression => {
            is_completed_node(Some(n.as_prefix_unary_expression().operand), source_file)
        }

        Kind::BinaryExpression => {
            is_completed_node(Some(n.as_binary_expression().right()), source_file)
        }

        Kind::ConditionalExpression => {
            is_completed_node(Some(n.as_conditional_expression().when_false), source_file)
        }

        _ => true,
    }
}

// completednode.go:162
// Checks if node ends with 'expectedLastToken'.
// If child at position 'length - 1' is 'SemicolonToken' it is skipped and 'expectedLastToken' is compared with child at position 'length - 2'.
fn node_ends_with(n: P<Node>, expected_last_token: Kind, source_file: P<SourceFile>) -> bool {
    let last_child_node = get_last_visited_child(n, source_file);
    let mut last_node_and_tokens: Vec<P<Node>> = Vec::new();
    let token_start_pos = match last_child_node {
        Some(c) => {
            last_node_and_tokens.push(c);
            c.end().expect_source("parsed nodes have source positions")
        }
        None => n.pos().expect_source("parsed nodes have source positions"),
    };
    let mut scanner = scanner::get_scanner_for_source_file(source_file, token_start_pos);
    let mut start_pos = token_start_pos;
    while start_pos < n.end().expect_source("parsed nodes have source positions") {
        let token_kind = scanner.token();
        let token_full_start = scanner.token_full_start();
        let token_end = scanner.token_end();
        let token = source_file.get_or_create_token(
            token_kind,
            token_full_start,
            token_end,
            n,
            scanner.token_flags(),
        );
        last_node_and_tokens.push(token);
        start_pos = token_end;
        scanner.scan();
    }
    let Some(&last_child) = last_node_and_tokens.last() else {
        return false;
    };
    if last_child.kind() == expected_last_token {
        return true;
    } else if last_child.kind() == Kind::SemicolonToken && last_node_and_tokens.len() > 1 {
        return last_node_and_tokens[last_node_and_tokens.len() - 2].kind() == expected_last_token;
    }
    false
}

// completednode.go:194
fn has_child_of_kind(containing_node: P<Node>, kind: Kind, source_file: P<SourceFile>) -> bool {
    astnav::find_child_of_kind(containing_node, kind, source_file).is_some()
}
