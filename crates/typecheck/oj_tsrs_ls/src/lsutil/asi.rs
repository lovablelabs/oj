#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

use tsrs_ast::{self as ast, FindAncestorResult, Kind, Node, SourceFile};
use tsrs_core::P;
use tsrs_scanner as scanner;

use super::*;
use crate::astnav;

// asi.go:9
pub(crate) fn position_is_asi_candidate(pos: u32, context: P<Node>, file: P<SourceFile>) -> bool {
    let context_ancestor = ast::find_ancestor_or_quit(context, |ancestor| {
        if ancestor.end().source_offset() != Some(pos) {
            return FindAncestorResult::Quit;
        }

        ast::to_find_ancestor_result(syntax_may_be_asi_candidate(ancestor.kind()))
    });

    context_ancestor.is_some_and(|a| node_is_asi_candidate(a, file))
}

// asi.go:21
fn syntax_may_be_asi_candidate(kind: Kind) -> bool {
    syntax_requires_trailing_comma_or_semicolon_or_asi(kind)
        || syntax_requires_trailing_function_block_or_semicolon_or_asi(kind)
        || syntax_requires_trailing_module_block_or_semicolon_or_asi(kind)
        || syntax_requires_trailing_semicolon_or_asi(kind)
}

// asi.go:28
pub(crate) fn syntax_requires_trailing_comma_or_semicolon_or_asi(kind: Kind) -> bool {
    kind == Kind::CallSignature
        || kind == Kind::ConstructSignature
        || kind == Kind::IndexSignature
        || kind == Kind::PropertySignature
        || kind == Kind::MethodSignature
}

// asi.go:36
fn syntax_requires_trailing_function_block_or_semicolon_or_asi(kind: Kind) -> bool {
    kind == Kind::FunctionDeclaration
        || kind == Kind::Constructor
        || kind == Kind::MethodDeclaration
        || kind == Kind::GetAccessor
        || kind == Kind::SetAccessor
}

// asi.go:44
fn syntax_requires_trailing_module_block_or_semicolon_or_asi(kind: Kind) -> bool {
    kind == Kind::ModuleDeclaration
}

// asi.go:48
pub(crate) fn syntax_requires_trailing_semicolon_or_asi(kind: Kind) -> bool {
    kind == Kind::VariableStatement
        || kind == Kind::ExpressionStatement
        || kind == Kind::DoStatement
        || kind == Kind::ContinueStatement
        || kind == Kind::BreakStatement
        || kind == Kind::ReturnStatement
        || kind == Kind::ThrowStatement
        || kind == Kind::DebuggerStatement
        || kind == Kind::PropertyDeclaration
        || kind == Kind::TypeAliasDeclaration
        || kind == Kind::ImportDeclaration
        || kind == Kind::ImportEqualsDeclaration
        || kind == Kind::ExportDeclaration
        || kind == Kind::NamespaceExportDeclaration
        || kind == Kind::ExportAssignment
}

// asi.go:66
fn node_is_asi_candidate(node: P<Node>, file: P<SourceFile>) -> bool {
    let last_token = get_last_token(Some(node), file);
    if last_token.is_some_and(|t| t.kind() == Kind::SemicolonToken) {
        return false;
    }

    if syntax_requires_trailing_comma_or_semicolon_or_asi(node.kind()) {
        if last_token.is_some_and(|t| t.kind() == Kind::CommaToken) {
            return false;
        }
    } else if syntax_requires_trailing_module_block_or_semicolon_or_asi(node.kind()) {
        let last_child = get_last_child(node, file);
        if last_child.is_some_and(ast::is_module_block) {
            return false;
        }
    } else if syntax_requires_trailing_function_block_or_semicolon_or_asi(node.kind()) {
        let last_child = get_last_child(node, file);
        if last_child.is_some_and(ast::is_function_block) {
            return false;
        }
    } else if !syntax_requires_trailing_semicolon_or_asi(node.kind()) {
        return false;
    }

    // See comment in parser's `parseDoStatement`
    if node.kind() == Kind::DoStatement {
        return true;
    }

    let top_node = ast::find_ancestor(node, |ancestor| ancestor.parent().is_none()).unwrap();
    let next_token = astnav::find_next_token(node, top_node, file);
    let Some(next_token) = next_token.filter(|t| t.kind() != Kind::CloseBraceToken) else {
        return true;
    };

    let start_line = scanner::get_ecma_line_of_position(
        file.get(),
        node.end().expect_source("parsed nodes have source positions"),
    );
    let end_line = scanner::get_ecma_line_of_position(
        file.get(),
        astnav::get_start_of_node(next_token, file, false /*includeJSDoc*/),
    );
    start_line != end_line
}
