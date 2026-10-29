use tsrs_ast::{Kind, Node, NodeList, SourceFile};
use tsrs_core::{P, TextRange};
use tsrs_scanner as scanner;

use crate::astnav;

// util.go:10
pub(crate) fn range_is_on_one_line(node: TextRange, file: P<SourceFile>) -> bool {
    let start_line = scanner::get_ecma_line_of_position(
        file.get(),
        node.pos().expect_source("formatting ranges have source positions"),
    );
    let end_line = scanner::get_ecma_line_of_position(
        file.get(),
        node.end().expect_source("formatting ranges have source positions"),
    );
    start_line == end_line
}

// util.go:16
pub(crate) fn get_open_token_for_list(node: P<Node>, list: P<NodeList>) -> Kind {
    match node.kind() {
        Kind::Constructor
        | Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::MethodDeclaration
        | Kind::MethodSignature
        | Kind::ArrowFunction
        | Kind::CallSignature
        | Kind::ConstructSignature
        | Kind::FunctionType
        | Kind::ConstructorType
        | Kind::GetAccessor
        | Kind::SetAccessor => {
            if node.type_parameter_list() == Some(list) {
                return Kind::LessThanToken;
            } else if node.parameter_list() == Some(list) {
                return Kind::OpenParenToken;
            }
        }
        Kind::CallExpression | Kind::NewExpression => {
            if node.type_argument_list() == Some(list) {
                return Kind::LessThanToken;
            } else if node.argument_list() == Some(list) {
                return Kind::OpenParenToken;
            }
        }
        Kind::ClassDeclaration
        | Kind::ClassExpression
        | Kind::InterfaceDeclaration
        | Kind::TypeAliasDeclaration => {
            if node.type_parameter_list() == Some(list) {
                return Kind::LessThanToken;
            }
        }
        Kind::TypeReference
        | Kind::TaggedTemplateExpression
        | Kind::TypeQuery
        | Kind::ExpressionWithTypeArguments
        | Kind::ImportType => {
            if node.type_argument_list() == Some(list) {
                return Kind::LessThanToken;
            }
        }
        Kind::TypeLiteral => return Kind::OpenBraceToken,
        _ => {}
    }

    Kind::Unknown
}

// util.go:63
pub(crate) fn get_close_token_for_open_token(kind: Kind) -> Kind {
    // TODO: matches strada - seems like it could handle more pairs of braces, though? [] notably missing
    match kind {
        Kind::OpenParenToken => Kind::CloseParenToken,
        Kind::LessThanToken => Kind::GreaterThanToken,
        Kind::OpenBraceToken => Kind::CloseBraceToken,
        _ => Kind::Unknown,
    }
}

// util.go:76
pub fn get_line_start_position_for_position(position: u32, source_file: P<SourceFile>) -> u32 {
    let line_starts = scanner::get_ecma_line_starts(source_file.get());
    let line = scanner::get_ecma_line_of_position(source_file.get(), position);
    line_starts[line]
}

// util.go:86
/*
 * Validating `expectedTokenKind` ensures the token was typed in the context we expect (eg: not a comment).
 * @param expectedTokenKind The kind of the last token constituting the desired parent node.
 */
pub(crate) fn find_immediately_preceding_token_of_kind(
    end: u32,
    expected_token_kind: Kind,
    source_file: P<SourceFile>,
) -> Option<P<Node>> {
    let preceding_token = astnav::find_preceding_token(source_file, end)?;
    if preceding_token.kind() != expected_token_kind
        || preceding_token.end().source_offset() != Some(end)
    {
        return None;
    }
    Some(preceding_token)
}

// util.go:107
/*
 * Finds the highest node enclosing `node` at the same list level as `node`
 * and whose end does not exceed `node.end`.
 *
 * Consider typing the following
 * ```
 * let x = 1;
 * while (true) {
 * }
 * ```
 * Upon typing the closing curly, we want to format the entire `while`-statement, but not the preceding
 * variable declaration.
 */
pub(crate) fn find_outermost_node_within_list_level(node: Option<P<Node>>) -> Option<P<Node>> {
    let mut current = node;
    while let Some(c) = current {
        let Some(parent) = c.parent() else {
            break;
        };
        if parent.end() != node.unwrap().end() || is_list_element(parent, c) {
            break;
        }
        current = Some(parent);
    }

    current
}

// util.go:121
// Returns true if node is a element in some list in parent
// i.e. parent is class declaration with the list of members and node is one of members.
fn is_list_element(parent: P<Node>, node: P<Node>) -> bool {
    match parent.kind() {
        Kind::ClassDeclaration | Kind::InterfaceDeclaration => {
            node.loc().contained_by(parent.member_list().unwrap().loc.get())
        }
        Kind::ModuleDeclaration => {
            let body = parent.body();
            body.is_some_and(|body| {
                body.kind() == Kind::ModuleBlock
                    && node.loc().contained_by(body.statement_list().unwrap().loc.get())
            })
        }
        Kind::SourceFile | Kind::Block | Kind::ModuleBlock => {
            node.loc().contained_by(parent.statement_list().unwrap().loc.get())
        }
        Kind::CatchClause => node
            .loc()
            .contained_by(parent.as_catch_clause().block.statement_list().unwrap().loc.get()),
        _ => false,
    }
}

// util.go:137
pub(crate) fn is_member_list_element(parent: P<Node>, node: P<Node>) -> bool {
    match parent.kind() {
        Kind::ClassDeclaration
        | Kind::ClassExpression
        | Kind::InterfaceDeclaration
        | Kind::EnumDeclaration
        | Kind::TypeLiteral
        | Kind::MappedType => node.loc().contained_by(parent.member_list().unwrap().loc.get()),
        _ => false,
    }
}
