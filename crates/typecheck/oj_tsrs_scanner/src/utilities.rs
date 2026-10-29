use tsrs_ast as ast;
use tsrs_ast::{Kind, Node, NodeFlags, NodeList, SourceFile, TokenFlags};
use tsrs_core::{LanguageVariant, P, stringutil};

use crate::scanner::{
    decode_rune, is_identifier_part_ex, is_identifier_start, skip_trivia, text_to_keyword,
};

pub(crate) fn token_is_identifier_or_keyword(token: Kind) -> bool {
    token >= Kind::Identifier
}

pub fn identifier_to_keyword_kind(node: P<Node>) -> Kind {
    text_to_keyword(node.as_identifier().text())
}

pub fn get_source_text_of_node_from_source_file(
    source_file: P<SourceFile>,
    node: P<Node>,
    include_trivia: bool,
) -> String {
    get_text_of_node_from_source_text(source_file.text(), node, include_trivia)
}

pub fn is_jsdoc_type_expression_or_child(node: P<Node>) -> bool {
    if ast::is_jsdoc_type_expression(node) {
        return true;
    }
    if !node.flags().intersects(NodeFlags::JSDoc | NodeFlags::Reparsed) {
        return false;
    }
    let current = Some(node);
    std::iter::successors(current, |c| c.parent()).any(ast::is_type_node)
}

fn trim_left_white_space_like(s: &str) -> &str {
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let (ch, size) = decode_rune(&b[i..]);
        if !stringutil::is_white_space_like(ch) {
            break;
        }
        i += size;
    }
    &s[i..]
}

pub fn normalize_jsdoc_type_source_text(text: &str) -> String {
    let line_starts = tsrs_core::compute_ecma_line_starts(text);
    if line_starts.len() == 1 {
        return strip_leading_jsdoc_comment(text).to_string();
    }

    let mut result = String::with_capacity(text.len());
    let new_line = "\n";
    result = line_starts.iter().enumerate().fold(result, |mut result, (i, &line_start)| {
        if i > 0 {
            result.push_str(new_line);
        }
        let mut line_end = text.len();
        if i + 1 < line_starts.len() {
            line_end = line_starts[i + 1] as usize;
        }
        let line = text[line_start as usize..line_end]
            .trim_end_matches(|c: char| stringutil::is_line_break(c as u32));
        result.push_str(strip_leading_jsdoc_comment(line));

        result
    });
    result
}

fn strip_leading_jsdoc_comment(line: &str) -> &str {
    let mut line = trim_left_white_space_like(line);
    if !line.is_empty() && line.as_bytes()[0] == b'*' {
        line = &line[1..];
    }
    trim_left_white_space_like(line)
}

pub fn get_text_of_node_from_source_text(
    source_text: &str,
    node: P<Node>,
    include_trivia: bool,
) -> String {
    if ast::node_is_missing(Some(node)) {
        return String::new();
    }
    let mut pos = node.pos().expect_source("parsed nodes have source positions");
    if !include_trivia {
        pos = skip_trivia(source_text, pos);
    }
    let end = node.end().expect_source("parsed nodes have source positions");
    let mut text = source_text[pos as usize..end as usize].to_string();
    if is_jsdoc_type_expression_or_child(node) {
        text = normalize_jsdoc_type_source_text(&text);
    }
    if node.flags().intersects(NodeFlags::ReparserTransformedLiteral) {
        // This is similar to `getLiteralTextOfNode` in the printer, but without the context of an `emitContext` to provide overrides
        if ast::is_string_literal(node) {
            if node.as_string_literal().token_flags().intersects(TokenFlags::SingleQuote) {
                return format!("'{text}'");
            }
            return format!("\"{text}\"");
        } else if ast::is_identifier(node) {
            return node.text().to_string();
        }
        // Only the above node kinds are currently transformed into one another by the reparser, requiring the textual remapping.
        // (Any reamppings done by emit transforms are handled by `getLiteralTextOfNode` in the printer)
        // Fail on any other kinds.
        panic!("Unexpected reparser-transformed node kind: {:?}", node.kind());
    }
    text
}

pub fn get_text_of_node(node: P<Node>) -> String {
    get_source_text_of_node_from_source_file(
        ast::get_source_file_of_node(node).unwrap(),
        node,
        false, /*includeTrivia*/
    )
}

pub fn get_text_of_jsdoc_comment(comment: Option<P<NodeList>>) -> String {
    let Some(comment) = comment else {
        return String::new();
    };
    let mut b = String::new();
    b = comment.nodes().iter().fold(b, |mut b, &n| {
        match n.kind() {
            Kind::JSDocText => b.push_str(n.text()),
            Kind::JSDocLink | Kind::JSDocLinkCode | Kind::JSDocLinkPlain => {
                b.push_str(&get_text_of_node(n));
            }
            _ => {}
        }

        b
    });
    let trimmed_len = b.trim_end_matches(char::is_whitespace).len();
    b.truncate(trimmed_len);
    b
}

pub fn declaration_name_to_string(name: Option<P<Node>>) -> String {
    match name {
        Some(name) if name.pos() != name.end() => get_text_of_node(name),
        _ => "(Missing)".to_string(),
    }
}

pub fn is_identifier_text(name: &str, language_variant: LanguageVariant) -> bool {
    let b = name.as_bytes();
    let (ch, mut size) = decode_rune(b);
    if !is_identifier_start(ch) {
        return false;
    }
    let mut i = size;
    while i < b.len() {
        let (ch, s) = decode_rune(&b[i..]);
        size = s;
        if !is_identifier_part_ex(ch, language_variant) {
            return false;
        }
        i += size;
    }
    true
}

pub fn is_intrinsic_jsx_name(name: &str) -> bool {
    !name.is_empty()
        && (name.as_bytes()[0] >= b'a' && name.as_bytes()[0] <= b'z' || name.contains('-'))
}
