use tsrs_ast::{Kind, Node, SourceFile};
use tsrs_core::stringutil::{decode_rune, is_line_break, is_white_space_single_line};
use tsrs_core::{LanguageVariant, P, TextChange, TextRange};
use tsrs_scanner as scanner;

use super::*;
use crate::lsutil::{self, FormatCodeSettings};

// api.go:14
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[expect(
    clippy::enum_variant_names,
    reason = "The variant names preserve the upstream Go enum names."
)]
pub enum FormatRequestKind {
    FormatDocument,
    FormatSelection,
    FormatOnEnter,
    FormatOnSemicolon,
    FormatOnOpeningCurlyBrace,
    FormatOnClosingCurlyBrace,
}

// api.go:25
// Go keeps the formatter settings and the host newline as values of the request `context.Context`
// (`formatOptionsKey`, `formatNewlineKey`). Here the two values are bundled in this struct, which every Go `ctx`
// parameter of this package maps to; callers that hold a `tsrs_core::context::Context` (completions, the change
// tracker) store it there with `with_value`.
#[derive(Clone, Default)]
pub struct FormatContext {
    format_options: Option<FormatCodeSettings>,
    format_newline: Option<String>,
}

/// tsrs-only: the largest indent, tab and base indent size the formatter uses (`with_format_code_settings`), far
/// beyond any real indentation.
pub const MAX_INDENT_SIZE: i32 = 100;

// api.go:32
pub fn with_format_code_settings(
    ctx: &FormatContext,
    mut options: FormatCodeSettings,
    new_line: &str,
) -> FormatContext {
    // tsrs-only: the formatter builds each line's indentation as a string of that many columns, so a client's
    // `tabSize: 1e9` made every indented line a gigabyte (Go and tsrs grew until killed). Sizes past
    // MAX_INDENT_SIZE are clamped to it.
    let editor = &mut options.editor_settings;
    for size in [&mut editor.indent_size, &mut editor.tab_size, &mut editor.base_indent_size] {
        *size = (*size).min(MAX_INDENT_SIZE);
    }
    let mut ctx = ctx.clone();
    ctx.format_options = Some(options);
    ctx.format_newline = Some(new_line.to_string());
    // In strada, the rules map was both globally cached *and* cached into the context, for some reason. We skip that here and just use the global one.
    ctx
}

// api.go:39
pub(crate) fn get_format_code_settings_from_context(ctx: &FormatContext) -> FormatCodeSettings {
    if let Some(opt) = &ctx.format_options {
        return opt.clone();
    }
    lsutil::get_default_format_code_settings()
}

// api.go:46
pub(crate) fn get_new_line_or_default_from_context(ctx: &FormatContext) -> String {
    // TODO: Move into broader LS - more than just the formatter uses the newline editor setting/host new line
    let opt = get_format_code_settings_from_context(ctx);
    if !opt.new_line_character.is_empty() {
        return opt.new_line_character.clone();
    }
    // Go type-asserts the context value, which panics when it is absent.
    let host = ctx
        .format_newline
        .as_deref()
        .expect("interface conversion: interface {} is nil, not string");
    if !host.is_empty() {
        return host.to_string();
    }
    "\n".to_string()
}

// api.go:58
fn format_span(
    ctx: &FormatContext,
    span: TextRange,
    file: P<SourceFile>,
    kind: FormatRequestKind,
) -> Vec<TextChange> {
    // find the smallest node that fully wraps the range and compute the initial indentation for the node
    let enclosing_node = find_enclosing_node(span, file);
    let opts = get_format_code_settings_from_context(ctx);

    let mut worker = new_format_span_worker(
        ctx,
        span,
        enclosing_node,
        get_indentation_for_node(enclosing_node, Some(span), file, &opts),
        get_own_or_inherited_delta(enclosing_node, &opts, file),
        kind,
        prepare_range_contains_error_function(file.diagnostics(), span),
        file,
    );
    new_formatting_scanner(
        file.text(),
        file.language_variant(),
        get_scan_start_position(enclosing_node, span, file),
        span.end().expect_source("formatting spans have source positions"),
        &mut worker,
    )
}

// api.go:81
pub fn format_node_given_indentation(
    ctx: &FormatContext,
    node: P<Node>,
    file: P<SourceFile>,
    language_variant: LanguageVariant,
    initial_indentation: i32,
    delta: i32,
) -> Vec<TextChange> {
    let text_range = TextRange::from_text_pos(node.pos(), node.end());
    let mut worker = new_format_span_worker(
        ctx,
        text_range,
        node,
        initial_indentation,
        delta,
        FormatRequestKind::FormatSelection,
        Box::new(|_: TextRange| false), // assume that node does not have any errors
        file,
    );
    new_formatting_scanner(
        file.text(),
        language_variant,
        text_range.pos().expect_source("formatting ranges have source positions"),
        text_range.end().expect_source("formatting ranges have source positions"),
        &mut worker,
    )
}

// api.go:101
fn format_node_lines(
    ctx: &FormatContext,
    source_file: P<SourceFile>,
    node: Option<P<Node>>,
    request_kind: FormatRequestKind,
) -> Vec<TextChange> {
    let Some(node) = node else {
        return Vec::new();
    };
    let token_start = scanner::get_token_pos_of_node(node, source_file, false);
    let line_start = get_line_start_position_for_position(token_start, source_file);
    let span =
        TextRange::new(line_start, node.end().expect_source("parsed nodes have source positions"));
    format_span(ctx, span, source_file, request_kind)
}

// api.go:111
pub fn format_document(ctx: &FormatContext, source_file: P<SourceFile>) -> Vec<TextChange> {
    format_span(
        ctx,
        TextRange::new(
            0,
            source_file.as_node().end().expect_source("source files have source positions"),
        ),
        source_file,
        FormatRequestKind::FormatDocument,
    )
}

// api.go:115
pub fn format_selection(
    ctx: &FormatContext,
    source_file: P<SourceFile>,
    start: u32,
    end: u32,
) -> Vec<TextChange> {
    format_span(
        ctx,
        TextRange::new(get_line_start_position_for_position(start, source_file), end),
        source_file,
        FormatRequestKind::FormatSelection,
    )
}

// api.go:119
pub(crate) fn format_on_opening_curly(
    ctx: &FormatContext,
    source_file: P<SourceFile>,
    position: u32,
) -> Vec<TextChange> {
    let Some(opening_curly) =
        find_immediately_preceding_token_of_kind(position, Kind::OpenBraceToken, source_file)
    else {
        return Vec::new();
    };
    let curly_brace_range = opening_curly.parent();
    let outermost_node = find_outermost_node_within_list_level(curly_brace_range);
    /*
     * We limit the span to end at the opening curly to handle the case where
     * the brace matched to that just typed will be incorrect after further edits.
     * For example, we could type the opening curly for the following method
     * body without brace-matching activated:
     * ```
     * class C {
     *     foo()
     * }
     * ```
     * and we wouldn't want to move the closing brace.
     */
    let text_range = TextRange::new(
        get_line_start_position_for_position(
            scanner::get_token_pos_of_node(outermost_node.unwrap(), source_file, false),
            source_file,
        ),
        position,
    );
    format_span(ctx, text_range, source_file, FormatRequestKind::FormatOnOpeningCurlyBrace)
}

// api.go:142
pub(crate) fn format_on_closing_curly(
    ctx: &FormatContext,
    source_file: P<SourceFile>,
    position: u32,
) -> Vec<TextChange> {
    let preceding_token =
        find_immediately_preceding_token_of_kind(position, Kind::CloseBraceToken, source_file);
    format_node_lines(
        ctx,
        source_file,
        find_outermost_node_within_list_level(preceding_token),
        FormatRequestKind::FormatOnClosingCurlyBrace,
    )
}

// api.go:147
pub(crate) fn format_on_semicolon(
    ctx: &FormatContext,
    source_file: P<SourceFile>,
    position: u32,
) -> Vec<TextChange> {
    let semicolon =
        find_immediately_preceding_token_of_kind(position, Kind::SemicolonToken, source_file);
    format_node_lines(
        ctx,
        source_file,
        find_outermost_node_within_list_level(semicolon),
        FormatRequestKind::FormatOnSemicolon,
    )
}

// api.go:152
pub(crate) fn format_on_enter(
    ctx: &FormatContext,
    source_file: P<SourceFile>,
    position: u32,
) -> Vec<TextChange> {
    let line = scanner::get_ecma_line_of_position(source_file.get(), position);
    if line == 0 {
        return Vec::new();
    }
    // get start position for the previous line
    let start_pos = scanner::get_ecma_line_starts(source_file.get())[line - 1];
    // After the enter key, the cursor is now at a new line. The new line may or may not contain non-whitespace characters.
    // If the new line has only whitespaces, we won't want to format this line, because that would remove the indentation as
    // trailing whitespaces. So the end of the formatting span should be the later one between:
    //  1. the end of the previous line
    //  2. the last non-whitespace character in the current line
    let text = source_file.text().as_bytes();
    let mut end_of_format_span =
        scanner::get_ecma_end_line_position(source_file, line).unwrap_or(text.len() as u32);
    while end_of_format_span > start_pos {
        let (ch, s) = decode_rune(&text[end_of_format_span as usize..]);
        if s == 0 || is_white_space_single_line(ch) {
            // on multibyte character keep backing up
            end_of_format_span -= 1;
            continue;
        }
        break;
    }

    // if the character at the end of the span is a line break, we shouldn't include it, because it indicates we don't want to
    // touch the current line at all. Also, on some OSes the line break consists of two characters (\r\n), we should test if the
    // previous character before the end of format span is line break character as well.
    let (ch, _) = decode_rune(&text[end_of_format_span as usize..]);
    if is_line_break(ch) {
        end_of_format_span -= 1;
    }

    let span = TextRange::new(
        start_pos,
        // end value is exclusive so add 1 to the result
        end_of_format_span + 1,
    );

    format_span(ctx, span, source_file, FormatRequestKind::FormatOnEnter)
}
