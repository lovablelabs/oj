use tsrs_ast::{self as ast, CommentRange, Kind, Node, NodeFactory};
use tsrs_core::{LanguageVariant, P, ScriptKind};
use tsrs_scanner as scanner;

pub fn get_language_variant(script_kind: ScriptKind) -> LanguageVariant {
    match script_kind {
        ScriptKind::TSX | ScriptKind::JSX | ScriptKind::JS | ScriptKind::JSON => {
            // .tsx and .jsx files are treated as jsx language variant.
            LanguageVariant::JSX
        }
        _ => LanguageVariant::Standard,
    }
}

pub fn token_is_identifier_or_keyword(token: Kind) -> bool {
    token >= Kind::Identifier
}

pub fn token_is_identifier_or_keyword_or_greater_than(token: Kind) -> bool {
    token == Kind::GreaterThanToken || token_is_identifier_or_keyword(token)
}

pub fn get_jsdoc_comment_ranges(
    _f: &mut NodeFactory,
    comment_ranges: &[CommentRange],
    node: P<Node>,
    text: &'static str,
) -> Vec<CommentRange> {
    let mut comment_ranges = comment_ranges.to_vec();
    append_jsdoc_comment_ranges(&mut comment_ranges, node, text);
    comment_ranges
}

/// `get_jsdoc_comment_ranges` into the caller's vector (the parser keeps one between nodes), which it filters as a
/// whole like the slice `get_jsdoc_comment_ranges` starts from.
pub fn append_jsdoc_comment_ranges(
    comment_ranges: &mut Vec<CommentRange>,
    node: P<Node>,
    text: &'static str,
) {
    let node_pos = node.pos().expect_source("parsed nodes have source positions");
    let node_end = node.end().expect_source("parsed nodes have source positions");
    match node.kind() {
        Kind::Parameter
        | Kind::TypeParameter
        | Kind::FunctionExpression
        | Kind::ArrowFunction
        | Kind::ParenthesizedExpression
        | Kind::VariableDeclaration
        | Kind::ExportSpecifier => {
            comment_ranges.extend(scanner::get_trailing_comment_ranges(text, node_pos));
            comment_ranges.extend(scanner::get_leading_comment_ranges(text, node_pos));
        }
        _ => {
            comment_ranges.extend(scanner::get_leading_comment_ranges(text, node_pos));
        }
    }
    // Keep if the comment starts with '/**' but not if it is '/**/'
    let bytes = text.as_bytes();
    comment_ranges.retain(|comment| {
        let comment_start = comment.pos();
        let comment_len = comment.end() - comment_start;
        !(comment.end() > node_end
            || comment_len < 4
            || bytes[(comment_start + 1) as usize] != b'*'
            || bytes[(comment_start + 2) as usize] != b'*'
            || bytes[(comment_start + 3) as usize] == b'/')
    });
}

pub fn is_keyword_or_punctuation(token: Kind) -> bool {
    ast::is_keyword_kind(token) || ast::is_punctuation_kind(token)
}

pub fn is_jsdoc_like_text(text: &str) -> bool {
    let text = text.as_bytes();
    text.len() >= 4 && text[1] == b'*' && text[2] == b'*' && text[3] != b'/'
}
