use tsrs_ast::{self as ast, Kind, Node, SourceFile};
use tsrs_core::collections::OrderedMap;
use tsrs_core::stringutil;
use tsrs_core::{P, TextRange, apply_bulk_edits};
use tsrs_lsproto as lsproto;
use tsrs_printer as printer;
use tsrs_scanner as scanner;

use super::tracker::*;
use crate::astnav;
use crate::format;
use crate::lsutil::{self, FormatCodeSettings, SemicolonPreference};
use crate::spanmap::Feature;

impl Tracker {
    // trackerimpl.go:22
    pub(crate) fn get_text_changes_from_changes(
        &mut self,
    ) -> OrderedMap<String, Vec<lsproto::TextEdit>> {
        let mut changes: OrderedMap<String, Vec<lsproto::TextEdit>> = OrderedMap::default();
        // A content-mapped file can have several projections, each keyed separately in t.changes but
        // all sharing one original file. Their edits are collected together before being ordered and checked,
        // so duplicate edits are emitted once and conflicting edits are rejected regardless of map iteration
        // order.
        let mut projections: OrderedMap<String, usize> = OrderedMap::default();
        let all: Vec<(P<SourceFile>, Vec<TrackerEdit>)> =
            self.changes.iter().map(|(k, v)| (*k, v.clone())).collect();
        for (source_file, changes_in_file) in all {
            let file_name = source_file.original_file_name().to_string();
            if self.unmappable_files.contains(&file_name) {
                continue;
            }
            // For a content-mapped file, a mapper may reorder source text relative to the original document.
            // Convert first, then sort and check for overlap in the space where edits are actually applied.
            let mut text_changes = Vec::with_capacity(changes_in_file.len());
            text_changes.extend(changes_in_file.iter().map(|change| {
                // !!! targetSourceFile

                let new_text = self.compute_new_text(change, source_file, source_file);
                // span := createTextSpanFromRange(c.Range)
                // !!!
                // Filter out redundant changes.
                // if (span.length == newText.length && stringContainsAt(targetSourceFile.text, newText, span.start)) { return nil }

                let range = self.to_lsp_edit_range(source_file, change.text_range);
                lsproto::TextEdit { new_text, range }
            }));

            if !text_changes.is_empty() {
                changes.entry(file_name.clone()).or_default().extend(text_changes);
                *projections.entry(file_name).or_default() += 1;
            }
        }

        let file_names: Vec<String> = changes.keys().cloned().collect();
        for file_name in file_names {
            // Converting the edits above may have found that this file cannot be represented in its original
            // text. GetChanges drops it, so its order does not matter, and the best-effort ranges left behind
            // may overlap in ways the check below would refuse.
            if self.unmappable_files.contains(&file_name) {
                continue;
            }
            let projection_count = projections.get(&file_name).copied().unwrap_or(0);
            let text_changes = changes.get_mut(&file_name).unwrap();
            // order changes by start position
            // If the start position is the same, put the shorter range first, since an empty range (x, x) may precede (x, y) but not vice-versa.
            text_changes.sort_by(|a, b| lsproto::compare_ranges(a.range, b.range));
            if projection_count > 1 {
                let deduped = dedupe_identical_edits(std::mem::take(text_changes));
                *text_changes = deduped;
            }
            // verify that change intervals do not overlap, except possibly at end points.
            if let Some(pair) = text_changes
                .windows(2)
                .find(|pair| text_edits_conflict(&pair[0], &pair[1], projection_count > 1))
            {
                if projection_count > 1 {
                    // Conflicting projections cannot be applied to the original file.
                    self.unmappable_files.insert(file_name.clone());
                } else {
                    panic!("changes overlap: {:?} and {:?}", pair[0].range, pair[1].range);
                }
            }
        }
        changes
    }

    // trackerimpl.go:136
    fn compute_new_text(
        &mut self,
        change: &TrackerEdit,
        target_source_file: P<SourceFile>,
        source_file: P<SourceFile>,
    ) -> String {
        if change.kind == TrackerEditKind::Text {
            return change.new_text.clone();
        }

        let start = self.to_lsp_edit_range(source_file, change.text_range).start;
        let positions =
            self.converters.from_lsp_position_for_source_file(source_file, start, Feature::All);
        let mut result = String::new();
        let mut found = false;
        // The original range may have multiple verbatim copies; it is safe to lose their identity only when
        // formatting at every exact projection produces the same edit.
        for mapped in &positions {
            if !mapped.fidelity.is_exact() {
                continue;
            }
            let projection = mapped.script;
            let pos = mapped.position;

            let text = match change.kind {
                TrackerEditKind::ReplaceWithMultipleNodes => {
                    let mut joiner = change.options.joiner.clone();
                    if joiner.is_empty() {
                        joiner.clone_from(&self.new_line);
                    }
                    let parts: Vec<String> = change
                        .nodes
                        .iter()
                        .map(|&n| {
                            let t = self.get_formatted_text_of_node(
                                n,
                                target_source_file,
                                projection,
                                pos,
                                &change.options,
                            );
                            t.strip_suffix(self.new_line.as_str()).map(str::to_string).unwrap_or(t)
                        })
                        .collect();
                    parts.join(&joiner)
                }
                TrackerEditKind::ReplaceWithSingleNode => self.get_formatted_text_of_node(
                    change.node.unwrap(),
                    target_source_file,
                    projection,
                    pos,
                    &change.options,
                ),
                TrackerEditKind::Text => {
                    panic!("change kind {} should have been handled earlier", change.kind as i32)
                }
            };
            // Strip initial indentation if text will be inserted in the middle of the line.
            let mut no_indent: &str = &text;
            if !(change.options.indentation.is_some()
                || format::get_line_start_position_for_position(pos, projection) == pos)
            {
                no_indent = text.trim_start_matches(|c: char| c.is_whitespace());
            }
            let candidate = format!(
                "{}{}{}",
                change.options.prefix,
                no_indent,
                if no_indent.ends_with(change.options.suffix.as_str()) {
                    ""
                } else {
                    change.options.suffix.as_str()
                }
            );
            if found && candidate != result {
                self.unmappable_files.insert(source_file.original_file_name().to_string());
                return String::new();
            }
            result = candidate;
            found = true;
        }
        if !found {
            self.unmappable_files.insert(source_file.original_file_name().to_string());
        }
        self.reindent_inserted_lines(source_file, change, result)
    }

    // trackerimpl.go:212
    // reindentInsertedLines fixes the indentation of a line an insertion introduces into the document the
    // edit is applied to. The inserted text forms its own line when it ends in a newline, and that line has to
    // pick up the indentation of the line it is being spliced into. Where the indentation goes depends on
    // which side of the insertion point it already sits on:
    //
    //   - Inserting after a line's indentation (`\t\tfoo` with the point before `foo`) leaves the inserted text
    //     indented but pushes the rest of the line down bare, so the indentation is repeated after the text.
    //   - Inserting at the very start of a line leaves the existing text indented but puts the inserted text at
    //     column zero, so the indentation is emitted before the text.
    //
    // The indentation is read from the original text at the edit's original position, so it holds for a
    // content-mapped file whose projection is indented differently from the document the edit is applied to.
    // Edits carrying an explicit indentation option are left alone.
    fn reindent_inserted_lines(
        &self,
        source_file: P<SourceFile>,
        change: &TrackerEdit,
        text: String,
    ) -> String {
        if text.is_empty()
            || change.text_range.pos() != change.text_range.end()
            || change.options.indentation.is_some()
        {
            return text;
        }
        if !text.ends_with(self.new_line.as_str()) {
            return text;
        }
        let original = crate::lsconv::Script::original_text(&source_file);
        let Some(mut pos) = change.text_range.source_pos() else {
            return text;
        };
        if let Some(spans) = crate::lsconv::Script::span_map(&source_file) {
            let (mapped, fidelity) = spans.virtual_to_original_position(pos);
            if !fidelity.is_exact() {
                return text;
            }
            pos = mapped;
        }
        if pos as usize > original.len() {
            return text;
        }
        let pos = pos as usize;
        let line_start = original[..pos].rfind(['\r', '\n']).map_or(0, |i| i + 1);
        let before_point = &original[line_start..pos];
        if before_point.is_empty() {
            // At the start of a line: the existing text keeps its indentation, and the inserted line needs it —
            // but only when the formatter left the text at column zero. Where the formatter already indented it
            // (inserting into a multi-line list, say), that indentation is the correct one.
            if !leading_indentation(&text).is_empty() {
                return text;
            }
            return format!("{}{}", leading_indentation(&original[line_start..]), text);
        }
        if leading_indentation(before_point) != before_point {
            return text;
        }
        // Just past the indentation: the inserted line already has it, the text pushed down needs it back.
        format!("{text}{before_point}")
    }

    // trackerimpl.go:262
    fn get_formatted_text_of_node(
        &self,
        node_in: P<Node>,
        target_source_file: P<SourceFile>,
        source_file: P<SourceFile>,
        pos: u32,
        options: &NodeOptions,
    ) -> String {
        let (text, source_file_like) = self.get_nonformatted_text(node_in, target_source_file);
        // !!! if (validate) validate(node, text);
        let format_options =
            get_format_code_settings_for_writing(self.format_settings.clone(), target_source_file);

        let initial_indentation = match options.indentation {
            None => format::get_indentation(
                pos,
                source_file,
                &format_options,
                options.prefix == self.new_line
                    || format::get_line_start_position_for_position(pos, source_file) == pos,
            ),
            Some(indentation) => indentation,
        };

        let mut delta = 0;
        if let Some(d) = options.delta {
            delta = d;
        } else if format_options.indent_size != 0
            && format::should_indent_child_node(&format_options, node_in, None, None, false)
        {
            delta = format_options.indent_size;
        }

        let changes = format::format_node_given_indentation(
            &format_context(&self.ctx),
            source_file_like,
            source_file_like.as_source_file_p(),
            target_source_file.language_variant(),
            initial_indentation,
            delta,
        );
        apply_bulk_edits(&text, &changes)
    }

    // trackerimpl.go:295
    fn get_nonformatted_text(
        &self,
        node: P<Node>,
        source_file: P<SourceFile>,
    ) -> (String, P<Node>) {
        let (text, node_out) = printer::print_and_position_node(
            &self.node_factory,
            node,
            Some(source_file),
            &self.new_line,
            usize::try_from(self.format_settings.indent_size).unwrap_or(0),
            Some(self.emit_context),
        );
        let source_file_like = printer::create_synthetic_source_file(
            &self.node_factory,
            node_out,
            &text,
            ast::SourceFileParseOptions {
                file_name: source_file.file_name().to_string(),
                path: source_file.path().clone(),
                ..Default::default()
            },
        );
        (text, source_file_like.as_node())
    }

    // trackerimpl.go:308
    // method on the changeTracker because use of converters
    // GetAdjustedRange computes the adjusted range for a node in a source file, accounting for trivia.
    pub(crate) fn get_adjusted_range(
        &self,
        source_file: P<SourceFile>,
        start_node: P<Node>,
        end_node: P<Node>,
        leading_option: LeadingTriviaOption,
        trailing_option: TrailingTriviaOption,
    ) -> TextRange {
        TextRange::new(
            self.get_adjusted_start_position(source_file, start_node, leading_option, false),
            Self::get_adjusted_end_position(source_file, end_node, trailing_option),
        )
    }

    // trackerimpl.go:316
    // method on the changeTracker because use of converters
    pub(crate) fn get_adjusted_start_position(
        &self,
        source_file: P<SourceFile>,
        node: P<Node>,
        leading_option: LeadingTriviaOption,
        has_trailing_comment: bool,
    ) -> u32 {
        let text = source_file.text();
        if leading_option == LeadingTriviaOption::JSDoc {
            let mut f = self.node_factory.clone();
            let jsdoc_comments = tsrs_parser::get_jsdoc_comment_ranges(&mut f, &[], node, text);
            if !jsdoc_comments.is_empty() {
                return format::get_line_start_position_for_position(
                    jsdoc_comments[0].pos(),
                    source_file,
                );
            }
        }

        let start = astnav::get_start_of_node(node, source_file, false);
        let start_of_line_pos = format::get_line_start_position_for_position(start, source_file);

        match leading_option {
            LeadingTriviaOption::Exclude => return start,
            LeadingTriviaOption::StartLine => {
                if node.loc().contains_inclusive(start_of_line_pos.into()) {
                    return start_of_line_pos;
                }
                return start;
            }
            _ => {}
        }

        let full_start = node.pos().expect_source("parsed nodes have source positions");
        if full_start == start {
            return start;
        }
        let line_starts = source_file.ecma_line_map();
        let full_start_line_index = scanner::compute_line_of_position(line_starts, full_start);
        let full_start_line_pos = line_starts[full_start_line_index];
        if start_of_line_pos == full_start_line_pos {
            // full start and start of the node are on the same line
            //   a,     b;
            //    ^     ^
            //    |   start
            // fullstart
            // when b is replaced - we usually want to keep the leading trvia
            // when b is deleted - we delete it
            if leading_option == LeadingTriviaOption::IncludeAll {
                return full_start;
            }
            return start;
        }

        // if node has a trailing comments, use comment end position as the text has already been included.
        if has_trailing_comment {
            // Check first for leading comments as if the node is the first import, we want to exclude the trivia;
            // otherwise we get the trailing comments.
            let mut comments: Vec<ast::CommentRange> =
                scanner::get_leading_comment_ranges(text, full_start).collect();
            if comments.is_empty() {
                comments = scanner::get_trailing_comment_ranges(text, full_start).collect();
            }
            if !comments.is_empty() {
                return scanner::skip_trivia_ex(
                    text,
                    comments[0].end(),
                    Some(&scanner::SkipTriviaOptions {
                        stop_after_line_break: true,
                        stop_at_comments: true,
                        ..Default::default()
                    }),
                );
            }
        }

        // get start position of the line following the line that contains fullstart position
        // (but only if the fullstart isn't the very beginning of the file)
        let next_line_start = usize::from(full_start > 0);
        let mut adjusted_start_position = line_starts[full_start_line_index + next_line_start];
        // skip whitespaces/newlines
        adjusted_start_position = scanner::skip_trivia_ex(
            text,
            adjusted_start_position,
            Some(&scanner::SkipTriviaOptions { stop_at_comments: true, ..Default::default() }),
        );
        line_starts[scanner::compute_line_of_position(line_starts, adjusted_start_position)]
    }

    // trackerimpl.go:394
    // method on the changeTracker because of converters
    // Return the end position of a multiline comment if it is on another line.
    fn get_end_position_of_multiline_trailing_comment(
        source_file: P<SourceFile>,
        node: P<Node>,
        trailing_opt: TrailingTriviaOption,
    ) -> Option<u32> {
        if trailing_opt == TrailingTriviaOption::Include {
            // If the trailing comment is a multiline comment that extends to the next lines,
            // return the end of the comment and track it for the next nodes to adjust.
            let line_starts = source_file.ecma_line_map();
            let node_end = node.end().expect_source("parsed nodes have source positions");
            let node_end_line = scanner::compute_line_of_position(line_starts, node_end);
            if let Some(comment) =
                scanner::get_trailing_comment_ranges(source_file.text(), node_end)
                    // Ignore single-line comments and comments starting on subsequent lines.
                    .take_while(|comment| {
                        comment.kind != Kind::SingleLineCommentTrivia
                            && scanner::compute_line_of_position(line_starts, comment.pos())
                                <= node_end_line
                    })
                    .find(|comment| {
                        scanner::compute_line_of_position(line_starts, comment.end())
                            > node_end_line
                    })
            {
                return Some(scanner::skip_trivia_ex(
                    source_file.text(),
                    comment.end(),
                    Some(&scanner::SkipTriviaOptions {
                        stop_after_line_break: true,
                        stop_at_comments: true,
                        ..Default::default()
                    }),
                ));
            }
        }

        None
    }

    // trackerimpl.go:422
    // method on the changeTracker because of converters
    pub(crate) fn get_adjusted_end_position(
        source_file: P<SourceFile>,
        node: P<Node>,
        trailing_trivia_option: TrailingTriviaOption,
    ) -> u32 {
        let node_end = node.end().expect_source("parsed nodes have source positions");
        if trailing_trivia_option == TrailingTriviaOption::Exclude {
            return node_end;
        }
        let text = source_file.text();
        if trailing_trivia_option == TrailingTriviaOption::ExcludeWhitespace {
            let mut comments: Vec<ast::CommentRange> =
                scanner::get_trailing_comment_ranges(text, node_end).collect();
            comments.extend(scanner::get_leading_comment_ranges(text, node_end));
            if let Some(last) = comments.last() {
                let real_end = last.end();
                if real_end != 0 {
                    return real_end;
                }
            }
            return node_end;
        }

        if let Some(multiline_end_position) = Self::get_end_position_of_multiline_trailing_comment(
            source_file,
            node,
            trailing_trivia_option,
        ) {
            return multiline_end_position;
        }

        let new_end = scanner::skip_trivia_ex(
            text,
            node_end,
            Some(&scanner::SkipTriviaOptions { stop_after_line_break: true, ..Default::default() }),
        );

        if new_end != node_end
            && (trailing_trivia_option == TrailingTriviaOption::Include
                || stringutil::is_line_break(u32::from(text.as_bytes()[new_end as usize - 1])))
        {
            return new_end;
        }
        node_end
    }

    // trackerimpl.go:475
    pub(crate) fn get_insertion_position_at_source_file_top(source_file: P<SourceFile>) -> u32 {
        let mut last_prologue: Option<P<Node>> = None;
        for &node in source_file.statements.nodes() {
            if ast::is_prologue_directive(node) {
                last_prologue = Some(node);
            } else {
                break;
            }
        }

        let mut position = 0_u32;
        let text = source_file.text();
        let bytes = text.as_bytes();
        let advance_past_line_break = |position: &mut u32| {
            if *position as usize >= bytes.len() {
                return;
            }
            let ch = bytes[*position as usize];
            if stringutil::is_line_break(u32::from(ch)) {
                *position += 1;
                if (*position as usize) < bytes.len()
                    && ch == b'\r'
                    && bytes[*position as usize] == b'\n'
                {
                    *position += 1;
                }
            }
        };
        if let Some(last_prologue) = last_prologue {
            position = last_prologue.end().expect_source("parsed prologues have source positions");
            advance_past_line_break(&mut position);
            return position;
        }

        let shebang = scanner::get_shebang(text);
        if !shebang.is_empty() {
            position = u32::try_from(shebang.len()).expect("shebang length exceeds u32");
            advance_past_line_break(&mut position);
        }

        let ranges: Vec<ast::CommentRange> =
            scanner::get_leading_comment_ranges(text, position).collect();
        if ranges.is_empty() {
            return position;
        }
        // Find the first attached comment to the first node and add before it
        let mut last_comment: Option<ast::CommentRange> = None;
        let mut pinned_or_triple_slash = false;
        let mut first_node_line = None;

        let len_statements = source_file.statements.nodes().len();
        let line_map = source_file.ecma_line_map();
        for r in &ranges {
            if r.kind == Kind::MultiLineCommentTrivia {
                if printer::is_pinned_comment(text, *r) {
                    last_comment = Some(*r);
                    pinned_or_triple_slash = true;
                    continue;
                }
            } else if printer::is_recognized_triple_slash_comment(text, *r) {
                last_comment = Some(*r);
                pinned_or_triple_slash = true;
                continue;
            }

            if let Some(lc) = &last_comment {
                // Always insert after pinned or triple slash comments
                if pinned_or_triple_slash {
                    break;
                }

                // There was a blank line between the last comment and this comment.
                // This comment is not part of the copyright comments
                let comment_line = scanner::compute_line_of_position(line_map, r.pos());
                let last_comment_end_line = scanner::compute_line_of_position(line_map, lc.end());
                if comment_line >= last_comment_end_line + 2 {
                    break;
                }
            }

            if len_statements > 0 {
                if first_node_line.is_none() {
                    first_node_line = Some(scanner::compute_line_of_position(
                        line_map,
                        astnav::get_start_of_node(
                            source_file.statements.nodes()[0],
                            source_file,
                            false,
                        ),
                    ));
                }
                let comment_end_line = scanner::compute_line_of_position(line_map, r.end());
                if first_node_line.expect("a statement established its line") < comment_end_line + 2
                {
                    break;
                }
            }
            last_comment = Some(*r);
            pinned_or_triple_slash = false;
        }

        if let Some(lc) = last_comment {
            position = lc.end();
            advance_past_line_break(&mut position);
        }
        position
    }
}

// trackerimpl.go:85
// dedupeIdenticalEdits drops exact duplicates from a sorted slice of edits. When a mapper copies one span
// of the original into more than one projection, an edit computed against each projection describes the
// same change to the same original range; emitting it once per projection would apply it repeatedly.
fn dedupe_identical_edits(mut edits: Vec<lsproto::TextEdit>) -> Vec<lsproto::TextEdit> {
    edits.dedup_by(|a, b| a.range == b.range && a.new_text == b.new_text);
    edits
}

// trackerimpl.go:96
pub fn text_edits_conflict(
    a: &lsproto::TextEdit,
    b: &lsproto::TextEdit,
    multiple_projections: bool,
) -> bool {
    if lsproto::compare_positions(a.range.end, b.range.start).is_gt() {
        return true;
    }
    // Different insertions at the same position are ambiguous when they may come from different projections.
    multiple_projections
        && a.range.start == a.range.end
        && a.range == b.range
        && a.new_text != b.new_text
}

// trackerimpl.go:255
fn leading_indentation(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut end = 0;
    end += (end..bytes.len())
        .take_while(|&index| bytes[index] == b' ' || bytes[index] == b'\t')
        .count();
    &text[..end]
}

// trackerimpl.go:285
pub(crate) fn get_format_code_settings_for_writing(
    options: FormatCodeSettings,
    source_file: P<SourceFile>,
) -> FormatCodeSettings {
    let mut options = options;
    let should_auto_detect_semicolon_preference = options.semicolons == SemicolonPreference::Ignore;
    let should_remove_semicolons = options.semicolons == SemicolonPreference::Remove
        || should_auto_detect_semicolon_preference
            && !lsutil::probably_uses_semicolons(source_file);
    if should_remove_semicolons {
        options.semicolons = SemicolonPreference::Remove;
    }

    options
}

// trackerimpl.go:452
pub(crate) fn has_comments_before_line_break(text: &str, start: u32) -> bool {
    if let Some(result) = text[start as usize..].chars().into_iter().find_map(|ch| {
        if stringutil::is_white_space_single_line(ch as u32) { None } else { Some(ch == '/') }
    }) {
        return result;
    }
    false
}

// trackerimpl.go:461
pub(crate) fn need_semicolon_between(a: P<Node>, b: P<Node>) -> bool {
    (ast::is_property_signature_declaration(a) || ast::is_property_declaration(a))
        && ast::is_class_or_type_element(b)
        && b.name().unwrap().kind() == Kind::ComputedPropertyName
        || ast::is_statement_but_not_declaration(a) && ast::is_statement_but_not_declaration(b) // TODO: only if b would start with a `(` or `[`
}
