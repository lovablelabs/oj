use tsrs_ast::{self as ast, CommentRange, Kind, Node, NodeList, SourceFile};
use tsrs_core::stringutil::{decode_rune, is_white_space_like, is_white_space_single_line};
use tsrs_core::{P, TextRange};
use tsrs_scanner as scanner;

use super::*;
use crate::astnav;
use crate::lsutil::{self, FormatCodeSettings, IndentStyle};

// indent.go:17
pub(crate) fn get_indentation_for_node(
    n: P<Node>,
    ignore_actual_indentation_range: Option<TextRange>,
    source_file: P<SourceFile>,
    options: &FormatCodeSettings,
) -> i32 {
    let (startline, startpos) = scanner::get_ecma_line_and_byte_offset_of_position(
        source_file.get(),
        scanner::get_token_pos_of_node(n, source_file, false),
    );
    get_indentation_for_node_worker(
        n,
        startline,
        startpos,
        ignore_actual_indentation_range,
        0, /*indentationDelta*/
        source_file,
        false, /*isNextChild*/
        options,
    )
}

// indent.go:24
// GetIndentation computes the expected indentation for a position in a source file.
// This is the Go port of SmartIndenter.getIndentation from TypeScript.
pub fn get_indentation(
    position: u32,
    source_file: P<SourceFile>,
    options: &FormatCodeSettings,
    assume_new_line_before_close_brace: bool,
) -> i32 {
    if position as usize > source_file.text().len() {
        return options.base_indent_size; // past EOF
    }

    // no indentation when the indent style is set to none,
    // so we can return fast
    if options.indent_style == IndentStyle::NONE {
        return 0;
    }

    let preceding_token = astnav::find_preceding_token_ex(
        source_file,
        position,
        None, /*startNode*/
        true, /*excludeJSDoc*/
    );

    let enclosing_comment_range =
        get_range_of_enclosing_comment(source_file, position, preceding_token);
    if let Some(enclosing_comment_range) = enclosing_comment_range
        && enclosing_comment_range.kind == Kind::MultiLineCommentTrivia
    {
        return get_comment_indent(source_file, position, options, &enclosing_comment_range);
    }

    let Some(preceding_token) = preceding_token else {
        return options.base_indent_size;
    };

    // no indentation in string/regex/template literals
    if is_string_or_regular_expression_or_template_literal(preceding_token.kind()) {
        let token_start = scanner::get_token_pos_of_node(preceding_token, source_file, false);
        if token_start <= position
            && position < preceding_token.end().expect_source("parsed tokens have source positions")
        {
            return 0;
        }
    }

    let line_at_position = scanner::get_ecma_line_of_position(source_file.get(), position);

    // indentation is first non-whitespace character in a previous line
    // for block indentation, we should look for a line which contains something that's not
    // whitespace.
    let current_token = astnav::get_token_at_position(source_file, position);
    // For object literals, we want indentation to work just like with blocks.
    // If the `{` starts in any position (even in the middle of a line), then
    // the following indentation should treat `{` as the start of that line (including leading whitespace).
    // ```
    //     const a: { x: undefined, y: undefined } = {}       // leading 4 whitespaces and { starts in the middle of line
    // ->
    //     const a: { x: undefined, y: undefined } = {
    //         x: undefined,
    //         y: undefined,
    //     }
    // ---------------------
    //     const a: {x : undefined, y: undefined } =
    //      {}
    // ->
    //     const a: { x: undefined, y: undefined } =
    //      {                                                  // leading 5 whitespaces and { starts at 6 column
    //          x: undefined,
    //          y: undefined,
    //      }
    // ```
    let is_object_literal = current_token.kind() == Kind::OpenBraceToken
        && current_token.parent().is_some_and(|p| p.kind() == Kind::ObjectLiteralExpression);
    if options.indent_style == IndentStyle::BLOCK || is_object_literal {
        return get_block_indent(source_file, position, options);
    }

    if preceding_token.kind() == Kind::CommaToken
        && preceding_token.parent().is_some_and(|p| p.kind() != Kind::BinaryExpression)
    {
        // previous token is comma that separates items in list - find the previous item and try to derive indentation from it
        if let Some(actual_indentation) =
            get_actual_indentation_for_list_item_before_comma(preceding_token, source_file, options)
        {
            return actual_indentation;
        }
    }

    let container_list = get_list_by_position(position, preceding_token.parent(), source_file);
    // use list position if the preceding token is before any list items
    if let Some(container_list) = container_list
        && !preceding_token.loc().contained_by(container_list.loc.get())
    {
        let use_the_same_base_indentation = current_token.parent().is_some_and(|p| {
            p.kind() == Kind::FunctionExpression || p.kind() == Kind::ArrowFunction
        });
        let mut indent_size = 0;
        if !use_the_same_base_indentation {
            indent_size = options.indent_size;
        }
        match get_actual_indentation_for_list_start_line(Some(container_list), source_file, options)
        {
            Some(indentation) => return indentation + indent_size,
            None => return indent_size,
        }
    }

    get_smart_indent(
        source_file,
        position,
        preceding_token,
        line_at_position,
        assume_new_line_before_close_brace,
        options,
    )
}

// indent.go:111
fn get_comment_indent(
    source_file: P<SourceFile>,
    position: u32,
    options: &FormatCodeSettings,
    enclosing_comment_range: &CommentRange,
) -> i32 {
    let previous_line = scanner::get_ecma_line_of_position(source_file.get(), position)
        .checked_sub(1)
        .expect("an enclosing comment has a preceding line");
    let comment_start_line =
        scanner::get_ecma_line_of_position(source_file.get(), enclosing_comment_range.pos());

    if previous_line <= comment_start_line {
        let line_starts = scanner::get_ecma_line_starts(source_file.get());
        return find_first_non_whitespace_column(
            line_starts[comment_start_line],
            position,
            source_file,
            options,
        );
    }

    let line_starts = scanner::get_ecma_line_starts(source_file.get());
    let start_position_of_line = line_starts[previous_line];
    let (character, column) = find_first_non_whitespace_character_and_column(
        start_position_of_line,
        position,
        source_file,
        options,
    );

    if column == 0 {
        return column;
    }

    let first_non_whitespace_character_code =
        source_file.text().as_bytes()[(start_position_of_line + character) as usize];
    if first_non_whitespace_character_code == b'*' {
        return column - 1;
    }
    column
}

// indent.go:137
fn get_leading_comment_ranges_of_node(
    node: P<Node>,
    file: P<SourceFile>,
) -> Option<scanner::CommentRangeIter> {
    if node.kind() == Kind::JsxText {
        return None;
    }
    Some(scanner::get_leading_comment_ranges(
        file.text(),
        node.pos().expect_source("parsed nodes have source positions"),
    ))
}

// indent.go:144
fn get_range_of_enclosing_comment(
    source_file: P<SourceFile>,
    position: u32,
    preceding_token: Option<P<Node>>,
) -> Option<CommentRange> {
    let mut token_at_position = astnav::get_token_at_position(source_file, position);
    let jsdoc = ast::find_ancestor(token_at_position, |n| n.is_jsdoc());
    if let Some(jsdoc) = jsdoc {
        token_at_position = jsdoc.parent().unwrap();
    }
    let token_start =
        astnav::get_start_of_node(token_at_position, source_file, false /*includeJSDoc*/);
    if token_start <= position
        && position < token_at_position.end().expect_source("parsed tokens have source positions")
    {
        return None;
    }

    // Between two consecutive tokens, all comments are either trailing on the former
    // or leading on the latter (and none are in both lists).
    let trailing_ranges_of_previous_token = preceding_token.map(|token| {
        scanner::get_trailing_comment_ranges(
            source_file.text(),
            token.end().expect_source("parsed tokens have source positions"),
        )
    });
    let leading_ranges_of_next_token =
        get_leading_comment_ranges_of_node(token_at_position, source_file);
    let comment_ranges = trailing_ranges_of_previous_token
        .into_iter()
        .flatten()
        .chain(leading_ranges_of_next_token.into_iter().flatten());
    if let Some(result) = comment_ranges.into_iter().find_map(|comment_range| {
        if comment_range.text_range.contains_exclusive(position.into())
            || position == comment_range.end()
                && (comment_range.kind == Kind::SingleLineCommentTrivia
                    || position as usize == source_file.text().len())
        {
            Some(Some(comment_range))
        } else {
            None
        }
    }) {
        return result;
    }
    None
}

// indent.go:177
fn get_block_indent(
    source_file: P<SourceFile>,
    position: u32,
    options: &FormatCodeSettings,
) -> i32 {
    // move backwards until we find a line with a non-whitespace character,
    // then find the first non-whitespace character for that line.
    let mut current = position;
    while current > 0 {
        let (ch, size) = decode_rune(&source_file.text().as_bytes()[current as usize..]);
        if !is_white_space_like(ch) {
            break;
        }
        current -= size as u32;
    }

    let line_start = get_line_start_position_for_position(current, source_file);
    find_first_non_whitespace_column(line_start, current, source_file, options)
}

// indent.go:193
fn get_actual_indentation_for_list_item_before_comma(
    comma_token: P<Node>,
    source_file: P<SourceFile>,
    options: &FormatCodeSettings,
) -> Option<i32> {
    // previous token is comma that separates items in list - find the previous item and try to derive indentation from it
    comma_token.parent()?;
    let containing_list = get_containing_list(comma_token, source_file)?;
    let comma_index = containing_list.nodes().iter().position(|&n| n == comma_token)?;
    if comma_index > 0 {
        return derive_actual_indentation_from_list(
            containing_list,
            comma_index - 1,
            source_file,
            options,
        );
    }
    None
}

// indent.go:209
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum NextTokenKind {
    Unknown = 0,
    OpenBrace = 1,
    CloseBrace = 2,
}

// indent.go:217
fn next_token_is_curly_brace_on_same_line_as_cursor(
    preceding_token: P<Node>,
    current: P<Node>,
    line_at_position: usize,
    source_file: P<SourceFile>,
) -> NextTokenKind {
    let Some(next_token) = astnav::find_next_token(preceding_token, current, source_file) else {
        return NextTokenKind::Unknown;
    };

    if next_token.kind() == Kind::OpenBraceToken {
        // open braces are always indented at the parent level
        return NextTokenKind::OpenBrace;
    } else if next_token.kind() == Kind::CloseBraceToken {
        // close braces are indented at the parent level if they are located on the same line with cursor
        let next_token_start_line = get_start_line_for_node(next_token, source_file);
        if line_at_position == next_token_start_line {
            return NextTokenKind::CloseBrace;
        }
        return NextTokenKind::Unknown;
    }

    NextTokenKind::Unknown
}

// indent.go:238
fn get_smart_indent(
    source_file: P<SourceFile>,
    position: u32,
    preceding_token: P<Node>,
    line_at_position: usize,
    assume_new_line_before_close_brace: bool,
    options: &FormatCodeSettings,
) -> i32 {
    // try to find node that can contribute to indentation and includes 'position' starting from 'precedingToken'
    // if such node is found - compute initial indentation for 'position' inside this node
    let mut previous: Option<P<Node>> = None;
    let mut current = Some(preceding_token);

    while let Some(cur) = current {
        if lsutil::position_belongs_to_node(cur, position, source_file)
            && should_indent_child_node(options, cur, previous, Some(source_file), true)
        {
            let (current_start_line, current_start_char) =
                get_start_line_and_character_for_node(cur, source_file);
            let ntk = next_token_is_curly_brace_on_same_line_as_cursor(
                preceding_token,
                cur,
                line_at_position,
                source_file,
            );
            let mut indentation_delta = 0;
            if ntk != NextTokenKind::Unknown {
                // handle cases when codefix is about to be inserted before the close brace
                if assume_new_line_before_close_brace && ntk == NextTokenKind::CloseBrace {
                    indentation_delta = options.indent_size;
                }
                // else 0
            } else if line_at_position != current_start_line {
                indentation_delta = options.indent_size;
            }
            return get_indentation_for_node_worker(
                cur,
                current_start_line,
                current_start_char,
                None,
                indentation_delta,
                source_file,
                true,
                options,
            );
        }

        // check if current node is a list item - if yes, take indentation from it
        // do not consider parent-child line sharing yet:
        // function foo(a
        //    | preceding node 'a' does share line with its parent but indentation is expected
        if let Some(actual_indentation) = get_actual_indentation_for_list_item(
            cur,
            source_file,
            options,
            true, /*listIndentsChild*/
        ) {
            return actual_indentation;
        }

        previous = current;
        current = cur.parent();
    }
    // no parent was found - return the base indentation of the SourceFile
    options.base_indent_size
}

// indent.go:279
fn get_indentation_for_node_worker(
    mut current: P<Node>,
    mut current_start_line: usize,
    mut current_start_character: u32,
    ignore_actual_indentation_range: Option<TextRange>,
    mut indentation_delta: i32,
    source_file: P<SourceFile>,
    is_next_child: bool,
    options: &FormatCodeSettings,
) -> i32 {
    let mut parent = current.parent();

    // Walk up the tree and collect indentation for parent-child node pairs. Indentation is not added if
    // * parent and child nodes start on the same line, or
    // * parent is an IfStatement and child starts on the same line as an 'else clause'.
    while let Some(par) = parent {
        let mut use_actual_indentation = true;
        if let Some(ignore_actual_indentation_range) = ignore_actual_indentation_range {
            let start = scanner::get_token_pos_of_node(current, source_file, false);
            use_actual_indentation = start
                < ignore_actual_indentation_range
                    .pos()
                    .expect_source("formatting ranges have source positions")
                || start
                    > ignore_actual_indentation_range
                        .end()
                        .expect_source("formatting ranges have source positions");
        }

        let (containing_list_or_parent_start_line, containing_list_or_parent_start_character) =
            get_containing_list_or_parent_start(par, current, source_file);
        let parent_and_child_share_line = containing_list_or_parent_start_line
            == current_start_line
            || child_starts_on_the_same_line_with_else_in_if_statement(
                par,
                current,
                current_start_line,
                source_file,
            );

        if use_actual_indentation {
            // check if current node is a list item - if yes, take indentation from it
            let mut first_list_child: Option<P<Node>> = None;
            let container_list = get_containing_list(current, source_file);
            if let Some(container_list) = container_list {
                first_list_child = tsrs_core::first_or_nil(container_list.nodes());
            }
            // A list indents its children if the children begin on a later line than the list itself:
            //
            // f1(               L0 - List start
            //   {               L1 - First child start: indented, along with all other children
            //     prop: 0
            //   },
            //   {
            //     prop: 1
            //   }
            // )
            //
            // f2({             L0 - List start and first child start: children are not indented.
            //   prop: 0             Object properties are indented only one level, because the list
            // }, {                  itself contributes nothing.
            //   prop: 1        L3 - The indentation of the second object literal is best understood by
            // })                    looking at the relationship between the list and *first* list item.
            let mut list_indents_child = false;
            if let Some(first_list_child) = first_list_child {
                let list_line = get_start_line_for_node(first_list_child, source_file);
                list_indents_child = list_line > containing_list_or_parent_start_line;
            }
            if let Some(actual_indentation) = get_actual_indentation_for_list_item(
                current,
                source_file,
                options,
                list_indents_child,
            ) {
                return actual_indentation + indentation_delta;
            }

            // try to fetch actual indentation for current node from source text
            let actual_indentation = get_actual_indentation_for_node(
                current,
                par,
                current_start_line,
                current_start_character,
                parent_and_child_share_line,
                source_file,
                options,
            );
            if let Some(actual_indentation) = actual_indentation {
                return actual_indentation + indentation_delta;
            }
        }

        // increase indentation if parent node wants its content to be indented and parent and child nodes don't start on the same line
        if should_indent_child_node(options, par, Some(current), Some(source_file), is_next_child)
            && !parent_and_child_share_line
        {
            indentation_delta += options.indent_size;
        }

        // In our AST, a call argument's `parent` is the call-expression, not the argument list.
        // We would like to increase indentation based on the relationship between an argument and its argument-list,
        // so we spoof the starting position of the (parent) call-expression to match the (non-parent) argument-list.
        // But, the spoofed start-value could then cause a problem when comparing the start position of the call-expression
        // to *its* parent (in the case of an iife, an expression statement), adding an extra level of indentation.
        //
        // Instead, when at an argument, we unspoof the starting position of the enclosing call expression
        // *after* applying indentation for the argument.

        let use_true_start = is_argument_and_start_line_overlaps_expression_being_called(
            par,
            current,
            current_start_line,
            source_file,
        );

        current = par;
        parent = current.parent();

        if use_true_start {
            (current_start_line, current_start_character) =
                scanner::get_ecma_line_and_byte_offset_of_position(
                    source_file.get(),
                    scanner::get_token_pos_of_node(current, source_file, false),
                );
        } else {
            current_start_line = containing_list_or_parent_start_line;
            current_start_character = containing_list_or_parent_start_character;
        }
    }

    indentation_delta + options.base_indent_size
}

// indent.go:378
fn get_actual_indentation_for_node(
    current: P<Node>,
    parent: P<Node>,
    current_line: usize,
    current_char: u32,
    parent_and_child_share_line: bool,
    source_file: P<SourceFile>,
    options: &FormatCodeSettings,
) -> Option<i32> {
    // actual indentation is used for statements\declarations if one of cases below is true:
    // - parent is SourceFile - by default immediate children of SourceFile are not indented except when user indents them manually
    // - parent and child are not on the same line
    let use_actual_indentation = (ast::is_declaration(current)
        || ast::is_statement_but_not_declaration(current))
        && (parent.kind() == Kind::SourceFile || !parent_and_child_share_line);

    if !use_actual_indentation {
        return None;
    }

    Some(find_column_for_first_non_whitespace_character_in_line(
        current_line,
        current_char,
        source_file,
        options,
    ))
}

// indent.go:391
fn is_argument_and_start_line_overlaps_expression_being_called(
    parent: P<Node>,
    child: P<Node>,
    child_start_line: usize,
    source_file: P<SourceFile>,
) -> bool {
    if !(ast::is_call_expression(parent) && parent.arguments().contains(&child)) {
        return false;
    }
    let expression_of_call_expression_end = parent
        .expression()
        .unwrap()
        .end()
        .expect_source("parsed expressions have source positions");
    let expression_of_call_expression_end_line =
        scanner::get_ecma_line_of_position(source_file.get(), expression_of_call_expression_end);
    expression_of_call_expression_end_line == child_start_line
}

// indent.go:400
fn get_actual_indentation_for_list_item(
    node: P<Node>,
    source_file: P<SourceFile>,
    options: &FormatCodeSettings,
    list_indents_child: bool,
) -> Option<i32> {
    if node.parent().is_some_and(|p| p.kind() == Kind::VariableDeclarationList) {
        // VariableDeclarationList has no wrapping tokens
        return None;
    }
    let containing_list = get_containing_list(node, source_file);
    if let Some(containing_list) = containing_list {
        if let Some(index) = containing_list.nodes().iter().position(|&element| element == node)
            && let Some(result) =
                derive_actual_indentation_from_list(containing_list, index, source_file, options)
        {
            return Some(result);
        }
        let mut delta = 0;
        if list_indents_child {
            delta = options.indent_size;
        }
        match get_actual_indentation_for_list_start_line(
            Some(containing_list),
            source_file,
            options,
        ) {
            Some(indentation) => return Some(indentation + delta),
            None => return Some(delta),
        }
    }
    None
}

// indent.go:427
fn get_actual_indentation_for_list_start_line(
    list: Option<P<NodeList>>,
    source_file: P<SourceFile>,
    options: &FormatCodeSettings,
) -> Option<i32> {
    let list = list?;
    let (line, char) = scanner::get_ecma_line_and_byte_offset_of_position(
        source_file.get(),
        list.loc.get().pos().expect_source("parsed lists have source positions"),
    );
    Some(find_column_for_first_non_whitespace_character_in_line(line, char, source_file, options))
}

// indent.go:435
fn derive_actual_indentation_from_list(
    list: P<NodeList>,
    index: usize,
    source_file: P<SourceFile>,
    options: &FormatCodeSettings,
) -> Option<i32> {
    assert!(index < list.nodes().len());

    let node = list.nodes()[index];

    // walk toward the start of the list starting from current node and check if the line is the same for all items.
    // if end line for item [i - 1] differs from the start line for item [i] - find column of the first non-whitespace character on the line of item [i]

    let (mut line, mut char) = get_start_line_and_character_for_node(node, source_file);

    for i in (0..=index).rev() {
        if list.nodes()[i].kind() == Kind::CommaToken {
            continue;
        }
        // skip list items that ends on the same line with the current list element
        let prev_end_line = scanner::get_ecma_line_of_position(
            source_file.get(),
            list.nodes()[i].end().expect_source("parsed nodes have source positions"),
        );
        if prev_end_line != line {
            return Some(find_column_for_first_non_whitespace_character_in_line(
                line,
                char,
                source_file,
                options,
            ));
        }

        (line, char) = get_start_line_and_character_for_node(list.nodes()[i], source_file);
    }
    None
}

// indent.go:460
fn find_column_for_first_non_whitespace_character_in_line(
    line: usize,
    char: u32,
    source_file: P<SourceFile>,
    options: &FormatCodeSettings,
) -> i32 {
    let line_start = scanner::get_ecma_position_of_line_and_byte_offset(source_file.get(), line, 0);
    find_first_non_whitespace_column(line_start, line_start + char, source_file, options)
}

// indent.go:465
pub(crate) fn find_first_non_whitespace_column(
    start_pos: u32,
    end_pos: u32,
    source_file: P<SourceFile>,
    options: &FormatCodeSettings,
) -> i32 {
    let (_, col) =
        find_first_non_whitespace_character_and_column(start_pos, end_pos, source_file, options);
    col
}

// indent.go:477
/*
* Character is the actual index of the character since the beginning of the line.
* Column - position of the character after expanding tabs to spaces.
* "0\t2$"
* value of 'character' for '$' is 3
* value of 'column' for '$' is 6 (assuming that tab size is 4)
 */
pub(crate) fn find_first_non_whitespace_character_and_column(
    start_pos: u32,
    end_pos: u32,
    source_file: P<SourceFile>,
    options: &FormatCodeSettings,
) -> (u32, i32) {
    let mut column = 0;
    let text = source_file.text().as_bytes();
    let mut pos = start_pos;
    while pos < end_pos {
        let (ch, size) = decode_rune(&text[pos as usize..]);
        if !is_white_space_single_line(ch) {
            break;
        }

        if ch == '\t' as u32 {
            if options.tab_size > 0 {
                column += options.tab_size + (column % options.tab_size);
            }
        } else {
            column += 1;
        }

        pos += size as u32;
    }
    (pos - start_pos, column)
}

// indent.go:500
pub(crate) fn child_starts_on_the_same_line_with_else_in_if_statement(
    parent: P<Node>,
    child: P<Node>,
    child_start_line: usize,
    source_file: P<SourceFile>,
) -> bool {
    if parent.kind() == Kind::IfStatement && parent.as_if_statement().else_statement == Some(child)
    {
        let else_keyword = astnav::find_preceding_token(
            source_file,
            child.pos().expect_source("parsed nodes have source positions"),
        );
        assert!(else_keyword.is_some());
        let else_keyword_start_line = get_start_line_for_node(else_keyword.unwrap(), source_file);
        return else_keyword_start_line == child_start_line;
    }
    false
}

// indent.go:510
fn get_start_line_and_character_for_node(n: P<Node>, source_file: P<SourceFile>) -> (usize, u32) {
    scanner::get_ecma_line_and_byte_offset_of_position(
        source_file.get(),
        scanner::get_token_pos_of_node(n, source_file, false),
    )
}

// indent.go:514
fn get_start_line_for_node(n: P<Node>, source_file: P<SourceFile>) -> usize {
    scanner::get_ecma_line_of_position(
        source_file.get(),
        scanner::get_token_pos_of_node(n, source_file, false),
    )
}

// indent.go:518
pub fn get_containing_list(node: P<Node>, source_file: P<SourceFile>) -> Option<P<NodeList>> {
    let parent = node.parent()?;
    get_list_by_range(
        scanner::get_token_pos_of_node(node, source_file, false),
        node.end().expect_source("parsed nodes have source positions"),
        parent,
        source_file,
    )
}

// indent.go:525
fn get_list_by_position(
    pos: u32,
    node: Option<P<Node>>,
    source_file: P<SourceFile>,
) -> Option<P<NodeList>> {
    let node = node?;
    get_list_by_range(pos, pos, node, source_file)
}

// indent.go:532
fn get_list_by_range(
    start: u32,
    end: u32,
    node: P<Node>,
    source_file: P<SourceFile>,
) -> Option<P<NodeList>> {
    let r = TextRange::new(start, end);
    match node.kind() {
        Kind::TypeReference => get_list(node.type_argument_list(), r, node, source_file),
        Kind::ObjectLiteralExpression => get_list(Some(node.property_list()), r, node, source_file),
        Kind::ArrayLiteralExpression
        | Kind::ObjectBindingPattern
        | Kind::ArrayBindingPattern
        | Kind::NamedImports
        | Kind::NamedExports => get_list(Some(node.element_list()), r, node, source_file),
        Kind::TypeLiteral => get_list(node.member_list(), r, node, source_file),
        Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::ArrowFunction
        | Kind::MethodDeclaration
        | Kind::MethodSignature
        | Kind::CallSignature
        | Kind::Constructor
        | Kind::ConstructorType
        | Kind::ConstructSignature => {
            let tpl = get_list(node.type_parameter_list(), r, node, source_file);
            if tpl.is_some() {
                return tpl;
            }
            get_list(node.parameter_list(), r, node, source_file)
        }
        Kind::GetAccessor => get_list(node.parameter_list(), r, node, source_file),
        Kind::ClassDeclaration
        | Kind::ClassExpression
        | Kind::InterfaceDeclaration
        | Kind::TypeAliasDeclaration
        | Kind::JSDocTemplateTag => get_list(node.type_parameter_list(), r, node, source_file),
        Kind::NewExpression | Kind::CallExpression => {
            let l = get_list(node.type_argument_list(), r, node, source_file);
            if l.is_some() {
                return l;
            }
            get_list(node.argument_list(), r, node, source_file)
        }
        Kind::VariableDeclarationList => {
            get_list(Some(node.as_variable_declaration_list().declarations), r, node, source_file)
        }
        _ => None, // TODO: should this be a panic? It isn't in strada.
    }
}

// indent.go:579
fn get_list(
    list: Option<P<NodeList>>,
    r: TextRange,
    node: P<Node>,
    source_file: P<SourceFile>,
) -> Option<P<NodeList>> {
    let list = list?;
    if r.contained_by(get_visual_list_range(node, list.loc.get(), source_file)) {
        return Some(list);
    }
    None
}

// indent.go:589
fn get_visual_list_range(_node: P<Node>, list: TextRange, source_file: P<SourceFile>) -> TextRange {
    // In strada, this relied on the services .getChildren method, which manifested synthetic token nodes
    // _however_, the logic boils down to "find the child with the matching span and adjust its start to the
    // previous (possibly token) child's end and its end to the token start of the following element" - basically
    // expanding the range to encompass all the neighboring non-token trivia
    // Now, we perform that logic with the scanner instead
    let list_start = list.pos().expect_source("parsed lists have source positions");
    let list_end = list.end().expect_source("parsed lists have source positions");
    let prior = astnav::find_preceding_token(source_file, list_start);
    let prior_end = match prior {
        None => list_start,
        Some(prior) => prior.end().expect_source("parsed tokens have source positions"),
    };
    // Find the token that starts at or after list.End() using the scanner
    let scan = scanner::get_scanner_for_source_file(source_file, list_end);
    let next_start = if scan.token() == Kind::EndOfFile { list_end } else { scan.token_start() };
    TextRange::new(prior_end, next_start)
}

// indent.go:613
fn get_containing_list_or_parent_start(
    parent: P<Node>,
    child: P<Node>,
    source_file: P<SourceFile>,
) -> (usize, u32) {
    let containing_list = get_containing_list(child, source_file);
    let start_pos = match containing_list {
        Some(containing_list) => {
            containing_list.loc.get().pos().expect_source("parsed lists have source positions")
        }
        None => scanner::get_token_pos_of_node(parent, source_file, false),
    };
    scanner::get_ecma_line_and_byte_offset_of_position(source_file.get(), start_pos)
}

// indent.go:624
fn is_control_flow_ending_statement(kind: Kind, parent_kind: Kind) -> bool {
    match kind {
        Kind::ReturnStatement
        | Kind::ThrowStatement
        | Kind::ContinueStatement
        | Kind::BreakStatement => parent_kind != Kind::Block,
        _ => false,
    }
}

// indent.go:637
/*
* True when the parent node should indent the given child by an explicit rule.
* @param isNextChild If true, we are judging indent of a hypothetical child *after* this one, not the current child.
 */
pub fn should_indent_child_node(
    settings: &FormatCodeSettings,
    parent: P<Node>,
    child: Option<P<Node>>,
    source_file: Option<P<SourceFile>>,
    is_next_child: bool,
) -> bool {
    node_will_indent_child(settings, parent, child, source_file, false)
        && !(is_next_child
            && child.is_some_and(|c| is_control_flow_ending_statement(c.kind(), parent.kind())))
}

// indent.go:646
pub(crate) fn node_will_indent_child(
    settings: &FormatCodeSettings,
    parent: P<Node>,
    child: Option<P<Node>>,
    source_file: Option<P<SourceFile>>,
    indent_by_default: bool,
) -> bool {
    let child_kind = match child {
        Some(c) => c.kind(),
        None => Kind::Unknown,
    };

    match parent.kind() {
        Kind::ExpressionStatement
        | Kind::ClassDeclaration
        | Kind::ClassExpression
        | Kind::InterfaceDeclaration
        | Kind::EnumDeclaration
        | Kind::TypeAliasDeclaration
        | Kind::ArrayLiteralExpression
        | Kind::Block
        | Kind::ModuleBlock
        | Kind::ObjectLiteralExpression
        | Kind::TypeLiteral
        | Kind::MappedType
        | Kind::TupleType
        | Kind::ParenthesizedExpression
        | Kind::PropertyAccessExpression
        | Kind::CallExpression
        | Kind::NewExpression
        | Kind::VariableStatement
        | Kind::ExportAssignment
        | Kind::ReturnStatement
        | Kind::ConditionalExpression
        | Kind::ArrayBindingPattern
        | Kind::ObjectBindingPattern
        | Kind::JsxOpeningElement
        | Kind::JsxOpeningFragment
        | Kind::JsxSelfClosingElement
        | Kind::JsxExpression
        | Kind::MethodSignature
        | Kind::CallSignature
        | Kind::ConstructSignature
        | Kind::Parameter
        | Kind::FunctionType
        | Kind::ConstructorType
        | Kind::ParenthesizedType
        | Kind::TaggedTemplateExpression
        | Kind::AwaitExpression
        | Kind::NamedExports
        | Kind::NamedImports
        | Kind::ExportSpecifier
        | Kind::ImportSpecifier
        | Kind::PropertyDeclaration
        | Kind::CaseClause
        | Kind::DefaultClause => return true,
        Kind::CaseBlock => return settings.indent_switch_case.is_true_or_unknown(),
        Kind::VariableDeclaration | Kind::PropertyAssignment | Kind::BinaryExpression => {
            if settings
                .indent_multi_line_object_literal_beginning_on_blank_line
                .is_false_or_unknown()
                && let Some(source_file) = source_file
                && child_kind == Kind::ObjectLiteralExpression
            {
                return range_is_on_one_line(child.unwrap().loc(), source_file);
            }
            if parent.kind() == Kind::BinaryExpression
                && let Some(source_file) = source_file
                && child_kind == Kind::JsxElement
            {
                let parent_start_line = scanner::get_ecma_line_of_position(
                    source_file.get(),
                    scanner::skip_trivia(
                        source_file.text(),
                        parent.pos().expect_source("parsed nodes have source positions"),
                    ),
                );
                let child_start_line = scanner::get_ecma_line_of_position(
                    source_file.get(),
                    scanner::skip_trivia(
                        source_file.text(),
                        child.unwrap().pos().expect_source("parsed nodes have source positions"),
                    ),
                );
                return parent_start_line != child_start_line;
            }
            if parent.kind() != Kind::BinaryExpression {
                return true;
            }
            return indent_by_default;
        }
        Kind::DoStatement
        | Kind::WhileStatement
        | Kind::ForInStatement
        | Kind::ForOfStatement
        | Kind::ForStatement
        | Kind::IfStatement
        | Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::MethodDeclaration
        | Kind::Constructor
        | Kind::GetAccessor
        | Kind::SetAccessor => return child_kind != Kind::Block,
        Kind::ArrowFunction => {
            if let Some(source_file) = source_file
                && child_kind == Kind::ParenthesizedExpression
            {
                return range_is_on_one_line(child.unwrap().loc(), source_file);
            }
            return child_kind != Kind::Block;
        }
        Kind::ExportDeclaration => return child_kind != Kind::NamedExports,
        Kind::ImportDeclaration => {
            return child_kind != Kind::ImportClause
                || child
                    .unwrap()
                    .as_import_clause()
                    .named_bindings
                    .is_some_and(|nb| nb.kind() != Kind::NamedImports);
        }
        Kind::JsxElement => return child_kind != Kind::JsxClosingElement,
        Kind::JsxFragment => return child_kind != Kind::JsxClosingFragment,
        Kind::IntersectionType | Kind::UnionType | Kind::SatisfiesExpression => {
            if child_kind == Kind::TypeLiteral
                || child_kind == Kind::TupleType
                || child_kind == Kind::MappedType
            {
                return false;
            }
            return indent_by_default;
        }
        Kind::TryStatement => {
            if child_kind == Kind::Block {
                return false;
            }
            return indent_by_default;
        }
        _ => {}
    }

    // No explicit rule for given nodes so the result will follow the default value argument
    indent_by_default
}

// indent.go:779
// A multiline conditional typically increases the indentation of its whenTrue and whenFalse children:
//
// condition
//
//	? whenTrue
//	: whenFalse;
//
// However, that indentation does not apply if the subexpressions themselves span multiple lines,
// applying their own indentation:
//
//	(() => {
//	  return complexCalculationForCondition();
//	})() ? {
//
//	  whenTrue: 'multiline object literal'
//	} : (
//
//	whenFalse('multiline parenthesized expression')
//
// );
//
// In these cases, we must discard the indentation increase that would otherwise be applied to the
// whenTrue and whenFalse children to avoid double-indenting their contents. To identify this scenario,
// we check for the whenTrue branch beginning on the line that the condition ends, and the whenFalse
// branch beginning on the line that the whenTrue branch ends.
pub(crate) fn child_is_unindented_branch_of_conditional_expression(
    parent: P<Node>,
    child: P<Node>,
    child_start_line: usize,
    source_file: P<SourceFile>,
) -> bool {
    if parent.kind() == Kind::ConditionalExpression
        && (child == parent.as_conditional_expression().when_true
            || child == parent.as_conditional_expression().when_false)
    {
        let condition_end_line = scanner::get_ecma_line_of_position(
            source_file.get(),
            parent
                .as_conditional_expression()
                .condition
                .end()
                .expect_source("parsed expressions have source positions"),
        );
        if child == parent.as_conditional_expression().when_true {
            return child_start_line == condition_end_line;
        }
        // On the whenFalse side, we have to look at the whenTrue side, because if that one was
        // indented, whenFalse must also be indented:
        //
        // const y = true
        //   ? 1 : (          L1: whenTrue indented because it's on a new line
        //     0              L2: indented two stops, one because whenTrue was indented
        //   );                   and one because of the parentheses spanning multiple lines
        let true_start_line =
            get_start_line_for_node(parent.as_conditional_expression().when_true, source_file);
        let true_end_line = scanner::get_ecma_line_of_position(
            source_file.get(),
            parent
                .as_conditional_expression()
                .when_true
                .end()
                .expect_source("parsed expressions have source positions"),
        );
        return condition_end_line == true_start_line && true_end_line == child_start_line;
    }
    false
}

// indent.go:800
pub(crate) fn argument_starts_on_same_line_as_previous_argument(
    parent: P<Node>,
    child: P<Node>,
    child_start_line: usize,
    source_file: P<SourceFile>,
) -> bool {
    if ast::is_call_expression(parent) || ast::is_new_expression(parent) {
        if parent.arguments().is_empty() {
            return false;
        }
        let Some(current_index) = parent.arguments().iter().position(|&argument| argument == child)
        else {
            // If it's not one of the arguments, don't look past this
            return false;
        };
        if current_index == 0 {
            return false; // Can't look at previous node if first
        }

        let previous_node = parent.arguments()[current_index - 1];
        let line_of_previous_node = scanner::get_ecma_line_of_position(
            source_file.get(),
            previous_node.end().expect_source("parsed nodes have source positions"),
        );
        if child_start_line == line_of_previous_node {
            return true;
        }
    }
    false
}
