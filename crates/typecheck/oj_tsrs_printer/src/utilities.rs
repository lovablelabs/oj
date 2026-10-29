use tsrs_ast::*;
use tsrs_core::stringutil;
use tsrs_core::*;
use tsrs_scanner as scanner;

use crate::*;

bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    #[expect(non_camel_case_types, reason = "Keep the upstream bitflags type name.")]
    pub struct getLiteralTextFlags: u32 {
        const None = 0;
        const NeverAsciiEscape = 1 << 0;
        const JsxAttributeEscape = 1 << 1;
        const TerminateUnterminatedLiterals = 1 << 2;
        const AllowNumericSeparator = 1 << 3;
        const InternalSymbolName = 1 << 4; // Port-only: see `EmitFlags::InternalSymbolName`.
    }
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum QuoteChar {
    SingleQuote = '\'' as u32,
    DoubleQuote = '"' as u32,
    Backtick = '`' as u32,
}

fn jsx_escaped_chars_map(ch: stringutil::Rune) -> Option<&'static str> {
    match ch {
        0x22 => Some("&quot;"),
        0x27 => Some("&apos;"),
        _ => None,
    }
}

fn escaped_chars_map(ch: stringutil::Rune) -> Option<&'static str> {
    match ch {
        0x09 => Some("\\t"),
        0x0B => Some("\\v"),
        0x0C => Some("\\f"),
        0x08 => Some("\\b"),
        0x0D => Some("\\r"),
        0x0A => Some("\\n"),
        0x5C => Some("\\\\"),
        0x22 => Some("\\\""),
        0x27 => Some("\\'"),
        0x60 => Some("\\`"),
        0x24 => Some("\\$"),       // when quoteChar == '`'
        0x2028 => Some("\\u2028"), // lineSeparator
        0x2029 => Some("\\u2029"), // paragraphSeparator
        0x0085 => Some("\\u0085"), // nextLine
        _ => None,
    }
}

fn encode_jsx_character_entity(b: &mut String, char_code: stringutil::Rune) {
    let hex_char_code = format!("{char_code:X}");
    b.push_str("&#x");
    b.push_str(&hex_char_code);
    b.push(';');
}

fn encode_utf16_escape_sequence(b: &mut String, char_code: stringutil::Rune) {
    let hex_char_code = format!("{char_code:X}");
    b.push_str("\\u");
    b.extend((hex_char_code.len()..4).map(|_| '0'));
    b.push_str(&hex_char_code);
}

// Based heavily on the abstract 'Quote'/'QuoteJSONString' operation from ECMA-262 (24.3.2.2),
// but augmented for a few select characters (e.g. lineSeparator, paragraphSeparator, nextLine)
// Note that this doesn't actually wrap the input in double quotes.
fn escape_string_worker(
    s: &str,
    quote_char: QuoteChar,
    flags: getLiteralTextFlags,
    b: &mut String,
) {
    let bytes = s.as_bytes();
    let mut pos = 0;
    let mut i = 0;
    while i < bytes.len() {
        let (mut ch, mut size) = if i == 0
            && flags.intersects(getLiteralTextFlags::InternalSymbolName)
            && bytes[0] == INTERNAL_SYMBOL_NAME_PREFIX_BYTE
        {
            // An internal symbol name the node builder put into a string literal (`(typeof E)["�missing"]`):
            // Go's prefix is the invalid byte 0xFE, which it decodes as a stray RuneError. Any other U+007F is user
            // text (`"\u{7F}\b"`), which Go leaves unescaped.
            (0xFFFD, 1)
        } else {
            stringutil::decode_js_string_rune_bytes(&bytes[i..])
        };

        let mut escape = false;
        if (0xD800..=0xDFFF).contains(&ch) {
            escape = true;
        } else if ch == 0xFFFD && size == 1 {
            // A stray byte that is not valid UTF-8 (for example, a fragment of a
            // surrogate sentinel left behind by code that sliced the string by
            // byte). Escape it as the Unicode replacement character so the output
            // is always well-formed rather than containing raw invalid bytes.
            escape = true;
        }

        // This consists of the first 19 unprintable ASCII characters, canonical escapes, lineSeparator,
        // paragraphSeparator, and nextLine. The latter three are just desirable to suppress new lines in
        // the language service. These characters should be escaped when printing, and if any characters are added,
        // `escapedCharsMap` and/or `jsxEscapedCharsMap` must be updated. Note that this *does not* include the 'delete'
        // character. There is no reason for this other than that JSON.stringify does not handle it either.
        if ch == '\\' as stringutil::Rune {
            if !flags.intersects(getLiteralTextFlags::JsxAttributeEscape) {
                escape = true;
            }
        } else if ch == '$' as stringutil::Rune {
            if quote_char == QuoteChar::Backtick && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
                escape = true;
            }
        } else if ch == quote_char as stringutil::Rune
            || ch == 0x2028
            || ch == 0x2029
            || ch == 0x0085
            || ch == '\r' as stringutil::Rune
        {
            escape = true;
        } else if ch == '\n' as stringutil::Rune {
            if quote_char != QuoteChar::Backtick {
                // Template strings preserve simple LF newlines, still encode CRLF (or CR).
                escape = true;
            }
        } else if ch <= 0x1f
            || !flags.intersects(getLiteralTextFlags::NeverAsciiEscape) && ch > 0x7f
        {
            escape = true;
        }

        if escape {
            if pos < i {
                // Write string up to this point
                b.push_str(&s[pos..i]);
            }

            if flags.intersects(getLiteralTextFlags::JsxAttributeEscape) {
                if ch == 0 {
                    b.push_str("&#0;");
                } else if let Some(m) = jsx_escaped_chars_map(ch) {
                    b.push_str(m);
                } else {
                    encode_jsx_character_entity(b, ch);
                }
            } else if ch == '\r' as stringutil::Rune
                && quote_char == QuoteChar::Backtick
                && i + 1 < bytes.len()
                && bytes[i + 1] == b'\n'
            {
                // Template strings preserve simple LF newlines, but still must escape CRLF. Left alone, the
                // above cases for `\r` and `\n` would inadvertently escape CRLF as two independent characters.
                size += 1;
                b.push_str("\\r\\n");
            } else if ch > 0xffff {
                // encode as surrogate pair
                ch -= 0x10000;
                encode_utf16_escape_sequence(b, ((ch & 0b1111_1111_1100_0000_0000) >> 10) + 0xD800);
                encode_utf16_escape_sequence(b, (ch & 0b0000_0000_0011_1111_1111) + 0xDC00);
            } else if (0xD800..=0xDFFF).contains(&ch) {
                encode_utf16_escape_sequence(b, ch);
            } else if ch == 0 {
                if i + 1 < bytes.len() && stringutil::is_digit(u32::from(bytes[i + 1])) {
                    // If the null character is followed by digits, print as a hex escape to prevent the result from
                    // parsing as an octal (which is forbidden in strict mode)
                    b.push_str("\\x00");
                } else {
                    // Otherwise, keep printing a literal \0 for the null character
                    b.push_str("\\0");
                }
            } else if let Some(m) = escaped_chars_map(ch) {
                b.push_str(m);
            } else {
                encode_utf16_escape_sequence(b, ch);
            }
            pos = i + size;
        }

        i += size;
    }

    if pos < i {
        b.push_str(&s[pos..]);
    }
}

pub fn escape_string(s: &str, quote_char: QuoteChar) -> String {
    let mut b = String::with_capacity(s.len() + 2);
    escape_string_worker(s, quote_char, getLiteralTextFlags::NeverAsciiEscape, &mut b);
    b
}

pub fn escape_non_ascii_string(s: &str, quote_char: QuoteChar) -> String {
    let mut b = String::with_capacity(s.len() + 2);
    escape_string_worker(s, quote_char, getLiteralTextFlags::None, &mut b);
    b
}

pub fn escape_jsx_attribute_string(s: &str, quote_char: QuoteChar) -> String {
    let mut b = String::with_capacity(s.len() + 2);
    escape_string_worker(
        s,
        quote_char,
        getLiteralTextFlags::JsxAttributeEscape | getLiteralTextFlags::NeverAsciiEscape,
        &mut b,
    );
    b
}

fn can_use_original_text(node: P<Node>, flags: getLiteralTextFlags) -> bool {
    // A synthetic node has no original text, nor does a node without a parent as we would be unable to find the
    // containing SourceFile. We also cannot use the original text if the literal was unterminated and the caller has
    // requested proper termination of unterminated literals
    if node_is_synthesized(node)
        || node.parent().is_none()
        || flags.intersects(getLiteralTextFlags::TerminateUnterminatedLiterals)
            && is_unterminated_literal(node)
    {
        return false;
    }

    if node.kind() == Kind::NumericLiteral {
        let token_flags = node.as_numeric_literal().token_flags();
        // For a numeric literal, we cannot use the original text if the original text was an invalid literal
        if token_flags.intersects(TokenFlags::IsInvalid) {
            return false;
        }
        // We also cannot use the original text if the literal contains numeric separators, but numeric separators
        // are not permitted
        if token_flags.intersects(TokenFlags::ContainsSeparator) {
            return flags.intersects(getLiteralTextFlags::AllowNumericSeparator);
        }
    }

    // Finally, we do not use the original text of a BigInt literal
    // TODO(rbuckton): The reason as to why we do not use the original text for bigints is not mentioned in the
    // original compiler source. It could be that this is no longer necessary, in which case bigint literals should
    // use the same code path as numeric literals, above
    node.kind() != Kind::BigIntLiteral
}

pub(crate) fn get_literal_text(
    node: P<Node>,
    source_file: Option<P<SourceFile>>,
    flags: getLiteralTextFlags,
) -> String {
    // If we don't need to downlevel and we can reach the original source text using
    // the node's parent reference, then simply get the text as it was originally written.
    if let Some(source_file) = source_file
        && can_use_original_text(node, flags)
    {
        return scanner::get_source_text_of_node_from_source_file(
            source_file,
            node,
            false, /*includeTrivia*/
        );
    }

    // If we can't reach the original source text, use the canonical form if it's a number,
    // or a (possibly escaped) quoted form of the original text if it's string-like.
    match node.kind() {
        Kind::StringLiteral => {
            let quote_char =
                if node.as_string_literal().token_flags().intersects(TokenFlags::SingleQuote) {
                    QuoteChar::SingleQuote
                } else {
                    QuoteChar::DoubleQuote
                };

            let text = node.text();

            // Write leading quote character
            let mut b = String::with_capacity(text.len() + 2);
            b.push(quote_char as u8 as char);

            // Write text
            escape_string_worker(text, quote_char, flags, &mut b);

            // Write trailing quote character
            b.push(quote_char as u8 as char);
            b
        }

        Kind::NoSubstitutionTemplateLiteral
        | Kind::TemplateHead
        | Kind::TemplateMiddle
        | Kind::TemplateTail => {
            // If a NoSubstitutionTemplateLiteral appears to have a substitution in it, the original text
            // had to include a backslash: `not \${a} substitution`.
            let text = node.text();
            let raw_text = node.template_literal_like_data().unwrap().raw_text;
            let raw = !raw_text.is_empty() || text.is_empty();

            let text_len = if raw { raw_text.len() } else { text.len() };

            // Write leading quote character
            let mut b = String::new();
            match node.kind() {
                Kind::NoSubstitutionTemplateLiteral => {
                    b.reserve(2 + text_len);
                    b.push('`');
                }
                Kind::TemplateHead => {
                    b.reserve(3 + text_len);
                    b.push('`');
                }
                Kind::TemplateMiddle => {
                    b.reserve(3 + text_len);
                    b.push('}');
                }
                Kind::TemplateTail => {
                    b.reserve(2 + text_len);
                    b.push('}');
                }
                _ => {}
            }

            // Write text
            if !raw_text.is_empty() || text.is_empty() {
                // If rawText is set, it is expected to be valid.
                b.push_str(raw_text);
            } else {
                escape_string_worker(text, QuoteChar::Backtick, flags, &mut b);
            }

            // Write trailing quote character
            match node.kind() {
                Kind::TemplateHead | Kind::TemplateMiddle => b.push_str("${"),
                Kind::NoSubstitutionTemplateLiteral | Kind::TemplateTail => b.push('`'),
                _ => {}
            }
            b
        }

        Kind::NumericLiteral | Kind::BigIntLiteral => node.text().to_string(),

        Kind::RegularExpressionLiteral => {
            if flags.intersects(getLiteralTextFlags::TerminateUnterminatedLiterals)
                && is_unterminated_literal(node)
            {
                let text = node.text();
                let mut b;
                if !text.is_empty() && text.as_bytes()[text.len() - 1] == b'\\' {
                    b = String::with_capacity(2 + text.len());
                    b.push_str(text);
                    b.push_str(" /");
                } else {
                    b = String::with_capacity(1 + text.len());
                    b.push_str(text);
                    b.push('/');
                }
                return b;
            }
            node.text().to_string()
        }

        _ => panic!("Unsupported LiteralLikeNode"),
    }
}

pub fn range_is_on_single_line(r: TextRange, source_file: P<SourceFile>) -> bool {
    range_start_is_on_same_line_as_range_end(r, r, source_file)
}

pub fn range_start_positions_are_on_same_line(
    range1: TextRange,
    range2: TextRange,
    source_file: P<SourceFile>,
) -> bool {
    positions_are_on_same_line(
        get_start_position_of_range(range1, source_file, false /*includeComments*/),
        get_start_position_of_range(range2, source_file, false /*includeComments*/),
        source_file,
    )
}

pub(crate) fn range_end_positions_are_on_same_line(
    range1: TextRange,
    range2: TextRange,
    source_file: P<SourceFile>,
) -> bool {
    positions_are_on_same_line(range1.end(), range2.end(), source_file)
}

fn range_start_is_on_same_line_as_range_end(
    range1: TextRange,
    range2: TextRange,
    source_file: P<SourceFile>,
) -> bool {
    positions_are_on_same_line(
        get_start_position_of_range(range1, source_file, false /*includeComments*/),
        range2.end(),
        source_file,
    )
}

pub(crate) fn range_end_is_on_same_line_as_range_start(
    range1: TextRange,
    range2: TextRange,
    source_file: P<SourceFile>,
) -> bool {
    positions_are_on_same_line(
        range1.end(),
        get_start_position_of_range(range2, source_file, false /*includeComments*/),
        source_file,
    )
}

fn get_start_position_of_range(
    r: TextRange,
    source_file: P<SourceFile>,
    include_comments: bool,
) -> TextPos {
    if position_is_synthesized(r.pos()) {
        return TextPos::Synthetic;
    }
    scanner::skip_trivia_ex(
        source_file.text(),
        r.pos().expect_source("source ranges have source positions"),
        Some(&scanner::SkipTriviaOptions {
            stop_at_comments: include_comments,
            ..Default::default()
        }),
    )
    .into()
}

pub fn positions_are_on_same_line(
    pos1: TextPos,
    pos2: TextPos,
    source_file: P<SourceFile>,
) -> bool {
    get_lines_between_positions(source_file, pos1, pos2).is_same()
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LineDifference {
    Before(u32),
    Same,
    After(u32),
}

impl LineDifference {
    pub fn is_same(self) -> bool {
        self == Self::Same
    }

    pub(crate) fn forward_count(self) -> usize {
        match self {
            Self::After(count) => count as usize,
            Self::Before(_) | Self::Same => 0,
        }
    }
}

pub fn get_lines_between_positions(
    source_file: P<SourceFile>,
    pos1: TextPos,
    pos2: TextPos,
) -> LineDifference {
    if pos1 == pos2 {
        return LineDifference::Same;
    }
    let Some((pos1, pos2)) = pos1.source_offset().zip(pos2.source_offset()) else {
        return LineDifference::Same;
    };
    let line_starts = scanner::get_ecma_line_starts(&*source_file);
    let lower = if pos1 < pos2 { pos1 } else { pos2 };
    let is_negative = lower == pos2;
    let upper = if is_negative { pos1 } else { pos2 };
    let lower_line = scanner::compute_line_of_position(line_starts, lower);
    let upper_line =
        lower_line + scanner::compute_line_of_position(&line_starts[lower_line..], upper);
    let difference = u32::try_from(upper_line - lower_line).expect("line difference exceeds u32");
    if difference == 0 {
        LineDifference::Same
    } else if is_negative {
        LineDifference::Before(difference)
    } else {
        LineDifference::After(difference)
    }
}

pub(crate) fn get_lines_between_range_end_and_range_start(
    range1: TextRange,
    range2: TextRange,
    source_file: P<SourceFile>,
    include_second_range_comments: bool,
) -> LineDifference {
    let range2_start =
        get_start_position_of_range(range2, source_file, include_second_range_comments);
    get_lines_between_positions(source_file, range1.end(), range2_start)
}

pub(crate) fn get_lines_between_position_and_preceding_non_whitespace_character(
    pos: TextPos,
    stop_pos: TextPos,
    source_file: P<SourceFile>,
    include_comments: bool,
) -> LineDifference {
    let pos = pos.expect_source("source ranges have source positions");
    let stop_pos = stop_pos.expect_source("source ranges have source positions");
    let start_pos = scanner::skip_trivia_ex(
        source_file.text(),
        pos,
        Some(&scanner::SkipTriviaOptions {
            stop_at_comments: include_comments,
            ..Default::default()
        }),
    );
    let prev_pos = get_previous_non_whitespace_position(start_pos, stop_pos, source_file);
    get_lines_between_positions(source_file, prev_pos.unwrap_or(stop_pos).into(), start_pos.into())
}

pub(crate) fn get_lines_between_position_and_next_non_whitespace_character(
    pos: TextPos,
    stop_pos: TextPos,
    source_file: P<SourceFile>,
    include_comments: bool,
) -> LineDifference {
    let pos = pos.expect_source("source ranges have source positions");
    let stop_pos = stop_pos.expect_source("source ranges have source positions");
    let next_pos = scanner::skip_trivia_ex(
        source_file.text(),
        pos,
        Some(&scanner::SkipTriviaOptions {
            stop_at_comments: include_comments,
            ..Default::default()
        }),
    );
    get_lines_between_positions(source_file, pos.into(), stop_pos.min(next_pos).into())
}

fn get_previous_non_whitespace_position(
    pos: u32,
    stop_pos: u32,
    source_file: P<SourceFile>,
) -> Option<u32> {
    let mut pos = pos;
    while pos >= stop_pos {
        if !stringutil::is_white_space_like(u32::from(source_file.text().as_bytes()[pos as usize]))
        {
            return Some(pos);
        }
        if pos == 0 {
            break;
        }
        pos -= 1;
    }
    None
}

pub(crate) fn sibling_node_positions_are_comparable(
    emit_context: P<EmitContext>,
    previous_node: P<Node>,
    next_node: P<Node>,
) -> bool {
    if next_node.pos() < previous_node.end() {
        return false;
    }

    let previous_node = emit_context.most_original(Some(previous_node)).unwrap();
    let next_node = emit_context.most_original(Some(next_node)).unwrap();
    let parent = previous_node.parent();
    if parent.is_none() || parent != next_node.parent() {
        return false;
    }

    let parent_node_array = get_containing_node_array(previous_node);
    if let Some(parent_node_array) = parent_node_array {
        let prev_node_index = parent_node_array.nodes().iter().position(|n| *n == previous_node);
        return match prev_node_index {
            Some(prev_node_index) => {
                parent_node_array.nodes().iter().position(|n| *n == next_node)
                    == Some(prev_node_index + 1)
            }
            None => false,
        };
    }

    false
}

fn modifier_node_list(modifiers: P<ModifierList>) -> P<NodeList> {
    P::from_static(&modifiers.get().list)
}

fn get_containing_node_array(node: P<Node>) -> Option<P<NodeList>> {
    let parent = node.parent()?;

    match node.kind() {
        Kind::TypeParameter => {
            if is_function_like(parent)
                || is_class_like(parent)
                || is_interface_declaration(parent)
                || is_type_or_js_type_alias_declaration(parent)
            {
                return parent.type_parameter_list();
            } else if is_infer_type_node(parent) {
                // infer type nodes have no associated type parameter list
            } else {
                panic!("Unexpected TypeParameter parent: {:?}", parent.kind());
            }
        }

        Kind::Parameter => return parent.function_like_data().unwrap().parameters(),
        Kind::TemplateLiteralTypeSpan => {
            return Some(parent.as_template_literal_type_node().template_spans);
        }
        Kind::TemplateSpan => return Some(parent.as_template_expression().template_spans),
        Kind::Decorator => {
            if can_have_decorators(parent)
                && let Some(modifiers) = parent.modifiers()
            {
                return Some(modifier_node_list(modifiers));
            }
            return None;
        }
        Kind::HeritageClause => {
            if is_class_like(parent) {
                return parent.class_like_data().unwrap().heritage_clauses();
            }
            return parent.as_interface_declaration().heritage_clauses;
        }
        _ => {}
    }

    // TODO(rbuckton)
    // if ast.IsJSDocTag(node) {
    //     if ast.IsJSDocTypeLiteral(node.parent) {
    // 		return nil
    // 	 }
    // 	 return node.parent.tags
    // }

    match parent.kind() {
        Kind::TypeLiteral | Kind::InterfaceDeclaration => {
            if is_type_element(node) {
                return parent.member_list();
            }
        }
        Kind::UnionType => return Some(parent.as_union_type_node().types()),
        Kind::IntersectionType => return Some(parent.as_intersection_type_node().types()),
        Kind::ArrayLiteralExpression
        | Kind::TupleType
        | Kind::NamedImports
        | Kind::NamedExports => return Some(parent.element_list()),
        Kind::ObjectLiteralExpression | Kind::JsxAttributes => return Some(parent.property_list()),
        Kind::CallExpression => {
            let p = parent.as_call_expression();
            if is_type_node(node) {
                return p.type_arguments();
            } else if node != p.expression {
                return Some(p.arguments);
            }
        }
        Kind::NewExpression => {
            let p = parent.as_new_expression();
            if is_type_node(node) {
                return p.type_arguments();
            } else if node != p.expression {
                return p.arguments;
            }
        }
        Kind::JsxElement | Kind::JsxFragment => {
            if is_jsx_child(node) {
                return Some(parent.children());
            }
        }
        Kind::JsxOpeningElement | Kind::JsxSelfClosingElement => {
            if is_type_node(node) {
                return parent.type_argument_list();
            }
        }
        Kind::Block | Kind::ModuleBlock | Kind::CaseClause | Kind::DefaultClause => {
            return parent.statement_list();
        }
        Kind::CaseBlock => return Some(parent.as_case_block().clauses),
        Kind::ClassDeclaration | Kind::ClassExpression => {
            if is_class_element(node) {
                return parent.member_list();
            }
        }
        Kind::EnumDeclaration => {
            if is_enum_member(node) {
                return parent.member_list();
            }
        }
        Kind::SourceFile if is_statement(node) => {
            return parent.statement_list();
        }
        _ => {}
    }

    if is_modifier(node)
        && let Some(modifiers) = parent.modifiers()
    {
        return Some(modifier_node_list(modifiers));
    }

    None
}

fn can_have_decorators(node: P<Node>) -> bool {
    matches!(
        node.kind(),
        Kind::Parameter
            | Kind::PropertyDeclaration
            | Kind::MethodDeclaration
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::ClassExpression
            | Kind::ClassDeclaration
    )
}

pub(crate) fn original_nodes_have_same_parent(
    emit_context: P<EmitContext>,
    node_a: P<Node>,
    node_b: P<Node>,
) -> bool {
    let node_a = emit_context.most_original(Some(node_a)).unwrap();
    if node_a.parent().is_some() {
        // For performance, do not call `MostOriginal` for `nodeB` if `nodeA` doesn't even
        // have a parent node.
        let node_b = emit_context.most_original(Some(node_b)).unwrap();
        return node_a.parent() == node_b.parent();
    }
    false
}

/// Go `tryGetEnd`: the values `greatestEnd` accepts (nil-able nodes, node lists, modifier lists, text ranges).
pub trait TryGetEnd {
    fn try_get_end(&self) -> Option<TextPos>;
}

impl TryGetEnd for P<Node> {
    fn try_get_end(&self) -> Option<TextPos> {
        Some(self.end())
    }
}

impl TryGetEnd for Option<P<Node>> {
    fn try_get_end(&self) -> Option<TextPos> {
        self.map(|n| n.end())
    }
}

impl TryGetEnd for Option<P<NodeList>> {
    fn try_get_end(&self) -> Option<TextPos> {
        self.map(|n| n.end())
    }
}

impl TryGetEnd for Option<P<ModifierList>> {
    fn try_get_end(&self) -> Option<TextPos> {
        self.map(|n| n.end())
    }
}

impl TryGetEnd for TextRange {
    fn try_get_end(&self) -> Option<TextPos> {
        Some(self.end())
    }
}

pub(crate) fn greatest_end(end: TextPos, nodes: &[&dyn TryGetEnd]) -> TextPos {
    let mut end = end;
    for node in nodes.iter().rev() {
        if let Some(node_end) = node.try_get_end()
            && end < node_end
        {
            end = node_end;
        }
    }
    end
}

pub(crate) fn skip_synthesized_parentheses(node: P<Node>) -> P<Node> {
    let mut node = node;
    while node.kind() == Kind::ParenthesizedExpression && node_is_synthesized(node) {
        node = node.expression().unwrap();
    }
    node
}

pub(crate) fn is_new_expression_without_arguments(node: P<Node>) -> bool {
    node.kind() == Kind::NewExpression && node.argument_list().is_none()
}

pub(crate) fn is_binary_operation(node: P<Node>, token: Kind) -> bool {
    let node = skip_partially_emitted_expressions(node);
    node.kind() == Kind::BinaryExpression
        && node.as_binary_expression().operator_token.kind() == token
}

pub(crate) fn mixing_binary_operators_requires_parentheses(a: Kind, b: Kind) -> bool {
    if a == Kind::QuestionQuestionToken {
        return b == Kind::AmpersandAmpersandToken || b == Kind::BarBarToken;
    }
    if b == Kind::QuestionQuestionToken {
        return a == Kind::AmpersandAmpersandToken || a == Kind::BarBarToken;
    }
    false
}

pub(crate) fn is_immediately_invoked_function_expression_or_arrow_function(node: P<Node>) -> bool {
    let node = skip_partially_emitted_expressions(node);
    if !is_call_expression(node) {
        return false;
    }
    let node = skip_partially_emitted_expressions(node.expression().unwrap());
    is_function_expression(node) || is_arrow_function(node)
}

fn has_leading_hash(text: &str) -> bool {
    !text.is_empty() && text.as_bytes()[0] == b'#'
}

pub(crate) fn remove_leading_hash(text: &str) -> &str {
    if has_leading_hash(text) { &text[1..] } else { text }
}

pub(crate) fn ensure_leading_hash(text: &str) -> String {
    if has_leading_hash(text) { text.to_string() } else { format!("#{text}") }
}

pub(crate) fn format_generated_name(
    private_name: bool,
    prefix: &str,
    base: &str,
    suffix: &str,
) -> String {
    let name = format!(
        "{}{}{}",
        remove_leading_hash(prefix),
        remove_leading_hash(base),
        remove_leading_hash(suffix)
    );
    if private_name {
        return ensure_leading_hash(&name);
    }
    name
}

fn is_ascii_word_character(ch: stringutil::Rune) -> bool {
    stringutil::is_ascii_letter(ch) || stringutil::is_digit(ch) || ch == '_' as stringutil::Rune
}

pub(crate) fn make_identifier_from_module_name(module_name: &str) -> String {
    let module_name = tspath::get_base_file_name(module_name);
    let bytes = module_name.as_bytes();
    let mut builder = String::new();
    let mut start = 0;
    let mut pos = 0;
    while pos < bytes.len() {
        let ch = u32::from(bytes[pos]);
        if pos == 0 && stringutil::is_digit(ch) {
            builder.push('_');
        } else if !is_ascii_word_character(ch) {
            if start < pos {
                builder.push_str(&module_name[start..pos]);
            }
            builder.push('_');
            start = pos + 1;
        }
        pos += 1;
    }
    if start < pos {
        builder.push_str(&module_name[start..pos]);
    }
    builder
}

// utilities.go:706
pub(crate) fn find_span_end_with_emit_context<T: Copy>(
    c: &EmitContext,
    array: &[T],
    test: impl Fn(&EmitContext, T) -> bool,
    start: usize,
) -> usize {
    let mut i = start;
    i += (i..array.len()).take_while(|&index| test(c, array[index])).count();
    i
}

// utilities.go:714
pub(crate) fn find_span_end<T: Copy>(array: &[T], test: impl Fn(T) -> bool, start: usize) -> usize {
    let mut i = start;
    i += (i..array.len()).take_while(|&index| test(array[index])).count();
    i
}

fn skip_white_space_single_line(text: &str, pos: &mut usize) {
    while *pos < text.len() {
        let (ch, size) = stringutil::decode_rune(&text.as_bytes()[*pos..]);
        if !stringutil::is_white_space_single_line(ch) {
            break;
        }
        *pos += size;
    }
}

fn match_white_space_single_line(text: &str, pos: &mut usize) -> bool {
    let start_pos = *pos;
    skip_white_space_single_line(text, pos);
    *pos != start_pos
}

fn match_rune(text: &str, pos: &mut usize, expected: stringutil::Rune) -> bool {
    let (ch, size) = stringutil::decode_rune(&text.as_bytes()[*pos..]);
    if ch == expected {
        *pos += size;
        return true;
    }
    false
}

fn match_string(text: &str, pos: &mut usize, expected: &str) -> bool {
    let mut text_pos = *pos;
    let mut expected_pos = 0;
    while expected_pos < expected.len() {
        if text_pos >= text.len() {
            return false;
        }

        let (expected_rune, expected_size) =
            stringutil::decode_rune(&expected.as_bytes()[expected_pos..]);
        if !match_rune(text, &mut text_pos, expected_rune) {
            return false;
        }

        expected_pos += expected_size;
    }

    *pos = text_pos;
    true
}

fn match_quoted_string(text: &str, pos: &mut usize) -> bool {
    let mut text_pos = *pos;

    let quote_char = if match_rune(text, &mut text_pos, '\'' as stringutil::Rune) {
        '\'' as stringutil::Rune
    } else if match_rune(text, &mut text_pos, '"' as stringutil::Rune) {
        '"' as stringutil::Rune
    } else {
        return false;
    };
    while text_pos < text.len() {
        let (ch, size) = stringutil::decode_rune(&text.as_bytes()[text_pos..]);
        text_pos += size;
        if ch == quote_char {
            *pos = text_pos;
            return true;
        }
    }
    false
}

// /// <reference path="..." />
// /// <reference types="..." />
// /// <reference lib="..." />
// /// <reference no-default-lib="..." />
// /// <amd-dependency path="..." />
// /// <amd-module />
pub fn is_recognized_triple_slash_comment(text: &str, comment_range: CommentRange) -> bool {
    let bytes = text.as_bytes();
    if comment_range.kind == Kind::SingleLineCommentTrivia
        && comment_range.text_range.len().is_some_and(|len| len > 2)
        && bytes[comment_range.pos() as usize + 1] == b'/'
        && bytes[comment_range.pos() as usize + 2] == b'/'
    {
        let text = &text[comment_range.pos() as usize + 3..comment_range.end() as usize];
        let mut pos = 0;
        skip_white_space_single_line(text, &mut pos);
        if !match_rune(text, &mut pos, '<' as stringutil::Rune) {
            return false;
        }
        if match_string(text, &mut pos, "reference") {
            if !match_white_space_single_line(text, &mut pos) {
                return false;
            }
            if !match_string(text, &mut pos, "path")
                && !match_string(text, &mut pos, "types")
                && !match_string(text, &mut pos, "lib")
                && !match_string(text, &mut pos, "no-default-lib")
            {
                return false;
            }
            skip_white_space_single_line(text, &mut pos);
            if !match_rune(text, &mut pos, '=' as stringutil::Rune) {
                return false;
            }
            skip_white_space_single_line(text, &mut pos);
            if !match_quoted_string(text, &mut pos) {
                return false;
            }
        } else if match_string(text, &mut pos, "amd-dependency") {
            if !match_white_space_single_line(text, &mut pos) {
                return false;
            }
            if !match_string(text, &mut pos, "path") {
                return false;
            }
            skip_white_space_single_line(text, &mut pos);
            if !match_rune(text, &mut pos, '=' as stringutil::Rune) {
                return false;
            }
            skip_white_space_single_line(text, &mut pos);
            if !match_quoted_string(text, &mut pos) {
                return false;
            }
        } else if match_string(text, &mut pos, "amd-module") {
            skip_white_space_single_line(text, &mut pos);
        } else {
            return false;
        }
        return text[pos..].contains("/>");
    }

    false
}

pub(crate) fn is_jsdoc_like_text(text: &str, comment: CommentRange) -> bool {
    let bytes = text.as_bytes();
    comment.kind == Kind::MultiLineCommentTrivia
        && comment.text_range.len().is_some_and(|len| len >= 5)
        && bytes[comment.pos() as usize + 2] == b'*'
        && bytes[comment.pos() as usize + 3] != b'/'
}

pub fn is_pinned_comment(text: &str, comment: CommentRange) -> bool {
    comment.kind == Kind::MultiLineCommentTrivia
        && comment.text_range.len().is_some_and(|len| len > 5)
        && text.as_bytes()[comment.pos() as usize + 2] == b'!'
}

pub(crate) fn calculate_indent(text: &str, pos: u32, end: u32) -> i32 {
    let mut pos = pos as usize;
    let end = end as usize;
    let mut current_line_indent = 0;
    let indent_size = get_default_indent_size() as i32;
    while pos < end {
        let (ch, size) = stringutil::decode_rune(&text.as_bytes()[pos..]);
        if !stringutil::is_white_space_single_line(ch) {
            break;
        }
        if ch == '\t' as stringutil::Rune {
            // Tabs = TabSize = indent size and go to next tabStop
            current_line_indent += indent_size - (current_line_indent % indent_size);
        } else {
            // Single space
            current_line_indent += 1;
        }
        pos += size;
    }

    current_line_indent
}

// utilities.go:886
// lineCharacterCache provides cached line/character lookups for a source file,
// optimized for monotonically increasing positions (e.g., during source map emit).
//
// When positions increase within the same line, only the delta between the last
// position and the new position needs to be scanned for UTF-16 code unit counts,
// turning what would be O(n²) into O(n) for long lines.
//
// Character offsets are measured in UTF-16 code units per the source map specification.
#[derive(Clone, Copy)]
pub struct LineCharacterCache {
    line_map: &'static [u32],
    text: &'static str,
    cached_line: usize,
    cached_pos: u32,
    cached_char: UTF16Offset,
    has_cached: bool,
}

// utilities.go:903
pub(crate) fn new_line_character_cache(
    source: &'static dyn tsrs_sourcemap::Source,
) -> LineCharacterCache {
    LineCharacterCache {
        line_map: source.ecma_line_map(),
        text: source.text(),
        cached_line: 0,
        cached_pos: 0,
        cached_char: 0,
        has_cached: false,
    }
}

impl LineCharacterCache {
    // utilities.go:912
    // getLineAndCharacter returns the 0-based line number and UTF-16 code unit
    // offset from the start of that line for the given byte position.
    pub(crate) fn get_line_and_character(&mut self, pos: u32) -> (u32, UTF16Offset) {
        let line = scanner::compute_line_of_position(self.line_map, pos);
        let line_start = self.line_map[line];
        // When pos is beyond the source text (e.g., for error-recovery tokens like
        // missing closing braces), we can't slice past the text end. Compute the
        // UTF-16 length up to EOF and add the remaining byte offset arithmetically,
        // matching TypeScript's computeLineAndCharacterOfPosition which uses
        // arithmetic (position - lineStarts[lineNumber]) and handles this implicitly.
        let end_pos = pos.min(self.text.len() as u32);
        let mut character =
            if self.has_cached && line == self.cached_line && end_pos >= self.cached_pos {
                // Incremental: only count UTF-16 code units from the last cached position.
                self.cached_char + utf16_len(&self.text[self.cached_pos as usize..end_pos as usize])
            } else if self.has_cached
                && line == self.cached_line
                && end_pos - line_start > self.cached_pos - end_pos
            {
                // tsrs-only: a position before the cached one on the same line (the printer maps a node's start after its
                // children's ends) is nearer to the cached position than to the line start; count back from there.
                self.cached_char - utf16_len(&self.text[end_pos as usize..self.cached_pos as usize])
            } else {
                // Full computation from line start.
                utf16_len(&self.text[line_start as usize..end_pos as usize])
            };
        let cached_char = character;
        character += pos - end_pos;
        self.cached_line = line;
        self.cached_pos = end_pos;
        self.cached_char = cached_char;
        self.has_cached = true;
        (line as u32, character)
    }
}
