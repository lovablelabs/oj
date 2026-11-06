// Port of ls/selectionranges.go.

use tsrs_ast::{self as ast, Kind, Node, NodeFactory, SourceFile};
use tsrs_core::context::Context;
use tsrs_core::{P, TextRange, alloc_slice};
use tsrs_lsproto as lsproto;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::languageservice::LanguageService;
use crate::spanmap::Feature;
use crate::utilities::get_children_from_non_jsdoc_node;
use crate::visitedchildren::{VisitedChild, collect_visited_children};

// selectionranges.go:14
const MAX_SELECTION_RANGE_DEPTH: usize = 1000;

// selectionranges.go:16
struct SelectionRangeBuilder {
    ranges: Vec<lsproto::Range>,
    // Go's `cap(b.ranges)`
    capacity: usize,
    oldest_index: usize,
}

// selectionranges.go:21
fn new_selection_range_builder(capacity: usize) -> SelectionRangeBuilder {
    SelectionRangeBuilder { ranges: Vec::with_capacity(capacity), capacity, oldest_index: 0 }
}

impl SelectionRangeBuilder {
    // selectionranges.go:27
    fn push(&mut self, selection_range: lsproto::Range) {
        if self.ranges.len() < self.capacity {
            self.ranges.push(selection_range);
            return;
        }

        self.ranges[self.oldest_index] = selection_range;
        self.oldest_index = (self.oldest_index + 1) % self.ranges.len();
    }

    // selectionranges.go:37
    fn build(&self, result: Option<lsproto::SelectionRange>) -> Option<lsproto::SelectionRange> {
        let mut result = result;
        for i in 0..self.ranges.len() {
            let index = (self.oldest_index + i) % self.ranges.len();
            result = Some(lsproto::SelectionRange {
                range: self.ranges[index],
                parent: result.map(Box::new),
            });
        }
        result
    }
}

impl LanguageService {
    // selectionranges.go:48
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language-service handlers retain the fallible interface used by LSP and API dispatch."
    )]
    pub fn provide_selection_ranges(
        &self,
        _ctx: &Context,
        params: &lsproto::SelectionRangeParams,
    ) -> Result<lsproto::SelectionRangeResponse, lsproto::Error> {
        let (_, source_file) = self.get_program_and_file(&params.text_document.uri);

        let mut results = Vec::with_capacity(params.positions.len());
        for &position in &params.positions {
            let positions = self.converters.from_lsp_position_for_source_file(
                source_file,
                position,
                Feature::SelectionRanges,
            );
            if positions.len() != 1 || !positions[0].fidelity.is_single_segment() {
                return Ok(lsproto::SelectionRangesOrNull::default());
            }
            if let Some(selection_range) =
                get_smart_selection_range(self, positions[0].script, positions[0].position)
            {
                results.push(selection_range);
            }
        }

        Ok(lsproto::SelectionRangesOrNull { selection_ranges: Some(results) })
    }
}

// selectionranges.go:68
fn get_selection_children(
    factory: &NodeFactory,
    node: P<Node>,
    source_file: P<SourceFile>,
) -> Vec<P<Node>> {
    if !ast::is_mapped_type_node(node) {
        return get_children_from_non_jsdoc_node(node, source_file);
    }

    let children = get_children_from_non_jsdoc_node(node, source_file);
    if children.len() < 2 {
        return children;
    }

    let open_brace_token = children[0];
    let close_brace_token = children[children.len() - 1];
    if open_brace_token.kind() != Kind::OpenBraceToken
        || close_brace_token.kind() != Kind::CloseBraceToken
    {
        return children;
    }

    let mapped_type = node.as_mapped_type_node();
    let children = &children[1..children.len() - 1];

    // Group `-/+readonly` and `-/+?`.
    let grouped_with_plus_minus_tokens = group_children(factory, children, |child| {
        Some(child) == mapped_type.readonly_token
            || child.kind() == Kind::ReadonlyKeyword
            || Some(child) == mapped_type.question_token
            || child.kind() == Kind::QuestionToken
    });

    // Group the type parameter with its surrounding brackets.
    let grouped_with_brackets = group_children(factory, &grouped_with_plus_minus_tokens, |child| {
        child.kind() == Kind::OpenBracketToken
            || child.kind() == Kind::TypeParameter
            || child.kind() == Kind::CloseBracketToken
    });

    // Go exposes the trailing semicolon directly, so keep it in the right-hand
    // group to produce the same effective selection tree as Strada.
    vec![
        open_brace_token,
        create_syntax_list(
            factory,
            &split_children(
                factory,
                &grouped_with_brackets,
                |child| child.kind() == Kind::ColonToken,
                false,
            ),
        ),
        close_brace_token,
    ]
}

// selectionranges.go:114
fn group_children(
    factory: &NodeFactory,
    children: &[P<Node>],
    group_on: impl Fn(P<Node>) -> bool,
) -> Vec<P<Node>> {
    let mut result = Vec::new();
    let mut group: Vec<P<Node>> = Vec::new();
    for &child in children {
        if group_on(child) {
            group.push(child);
        } else {
            if !group.is_empty() {
                result.push(create_syntax_list(factory, &group));
                group = Vec::new();
            }
            result.push(child);
        }
    }
    if !group.is_empty() {
        result.push(create_syntax_list(factory, &group));
    }
    result
}

// selectionranges.go:134
fn split_children(
    factory: &NodeFactory,
    children: &[P<Node>],
    pivot_on: impl Fn(P<Node>) -> bool,
    separate_trailing_semicolon: bool,
) -> Vec<P<Node>> {
    if children.len() < 2 {
        return children.to_vec();
    }

    let Some(split_token_index) = children.iter().position(|&child| pivot_on(child)) else {
        return children.to_vec();
    };

    let left_children = &children[..split_token_index];
    let split_token = children[split_token_index];
    let last_token = children[children.len() - 1];
    let separate_last_token =
        separate_trailing_semicolon && last_token.kind() == Kind::SemicolonToken;
    let mut right_end = children.len();
    if separate_last_token {
        right_end -= 1;
    }
    let right_children = &children[split_token_index + 1..right_end];

    let mut result = Vec::with_capacity(4);
    if !left_children.is_empty() {
        result.push(create_syntax_list(factory, left_children));
    }
    result.push(split_token);
    if !right_children.is_empty() {
        result.push(create_syntax_list(factory, right_children));
    }
    if separate_last_token {
        result.push(last_token);
    }
    result
}

// selectionranges.go:181
fn create_syntax_list(factory: &NodeFactory, children: &[P<Node>]) -> P<Node> {
    let list = factory.new_syntax_list(alloc_slice(children));
    list.set_loc(TextRange::from_text_pos(children[0].pos(), children[children.len() - 1].end()));
    list
}

// The closures of getSmartSelectionRange share `ranges` and `lastRange`.
struct SmartSelection<'a> {
    l: &'a LanguageService,
    source_file: P<SourceFile>,
    pos: u32,
    ranges: SelectionRangeBuilder,
    last_range: lsproto::Range,
}

impl SmartSelection<'_> {
    fn node_contains_position(&self, node: Option<P<Node>>) -> bool {
        let Some(node) = node else {
            return false;
        };
        let start =
            scanner::get_token_pos_of_node(node, self.source_file, true /*includeJSDoc*/);
        let end = node.end().expect_source("parsed nodes have source positions");
        start <= self.pos && self.pos < end
    }

    fn position_should_snap_to_node(&self, node: P<Node>) -> bool {
        let node_end = node.end().expect_source("parsed nodes have source positions");
        if self.pos < node_end {
            return true;
        }
        if node_end == self.pos {
            let touching_property_name =
                astnav::get_touching_property_name(self.source_file, self.pos);
            return touching_property_name.pos() < node.end();
        }
        false
    }

    fn push_selection_range(&mut self, start: u32, end: u32) {
        if start == end {
            return;
        }

        if !(start <= self.pos && self.pos <= end) {
            return;
        }

        let (lsp_range, fidelity) = self.l.converters.to_lsp_range_for_feature(
            &self.source_file,
            TextRange::new(start, end),
            Feature::SelectionRanges,
        );
        if fidelity.is_none() {
            return;
        }

        if self.last_range == lsp_range {
            return;
        }
        self.last_range = lsp_range;

        self.ranges.push(lsp_range);
    }

    fn push_selection_comment_range(&mut self, start: u32, end: u32) {
        self.push_selection_range(start, end);

        let mut comment_pos = start;
        let text = self.source_file.text().as_bytes();
        comment_pos += u32::try_from(
            (comment_pos..end)
                .take_while(|&index| (index as usize) < text.len() && text[index as usize] == b'/')
                .count(),
        )
        .expect("comment length exceeds u32");
        self.push_selection_range(comment_pos, end);
    }

    fn positions_are_on_same_line(&self, pos1: u32, pos2: u32) -> bool {
        if pos1 == pos2 {
            return true;
        }
        let line_starts = self.source_file.ecma_line_map();
        scanner::compute_line_of_position(line_starts, pos1)
            == scanner::compute_line_of_position(line_starts, pos2)
    }
}

// selectionranges.go:187 (shouldSkipNode)
fn should_skip_node(node: P<Node>, parent: Option<P<Node>>) -> bool {
    if ast::is_block(node) {
        return true;
    }

    if ast::is_template_span(node) || ast::is_template_head(node) || ast::is_template_tail(node) {
        return true;
    }

    if let Some(parent) = parent {
        if ast::is_variable_declaration_list(node) && ast::is_variable_statement(parent) {
            return true;
        }

        // Skip lone variable declarations
        if ast::is_variable_declaration(node) && ast::is_variable_declaration_list(parent) {
            let decl = parent.as_variable_declaration_list();
            if decl.declarations.nodes().len() == 1 {
                return true;
            }
        }
    }

    if ast::is_jsdoc_type_expression(node)
        || ast::is_jsdoc_signature(node)
        || ast::is_jsdoc_type_literal(node)
    {
        return true;
    }

    false
}

// selectionranges.go:186
fn get_smart_selection_range(
    l: &LanguageService,
    source_file: P<SourceFile>,
    pos: u32,
) -> Option<lsproto::SelectionRange> {
    let factory = NodeFactory::default();
    // Traversal discovers ranges from broadest to most specific, so retain the newest ranges nearest to the cursor
    let mut s = SmartSelection {
        l,
        source_file,
        pos,
        ranges: new_selection_range_builder(MAX_SELECTION_RANGE_DEPTH - 1),
        last_range: lsproto::Range::default(),
    };
    let mut root: Option<lsproto::SelectionRange> = None;
    if source_file.content_mapper().is_empty() {
        let (full_range, _) = l.converters.to_lsp_range(
            &source_file,
            TextRange::from_text_pos(source_file.as_node().pos(), source_file.as_node().end()),
        );
        root = Some(lsproto::SelectionRange { range: full_range, parent: None });
        s.last_range = full_range;
    }

    let mut current = Some(source_file.as_node());
    while let Some(cur) = current {
        let mut next: Option<P<Node>> = None;
        let parent = cur;

        let visit = |s: &mut SmartSelection, next: &mut Option<P<Node>>, node: P<Node>| {
            if next.is_some() {
                return;
            }
            let found_comment = scanner::get_trailing_comment_ranges(
                source_file.text(),
                node.end().expect_source("parsed nodes have source positions"),
            )
            .next();
            if let Some(found_comment) = found_comment
                && found_comment.kind == Kind::SingleLineCommentTrivia
            {
                s.push_selection_comment_range(
                    found_comment
                        .text_range
                        .pos()
                        .expect_source("comment ranges have source positions"),
                    found_comment
                        .text_range
                        .end()
                        .expect_source("comment ranges have source positions"),
                );
            }

            if s.node_contains_position(Some(node)) {
                // Add range for multi-line function bodies before skipping the block
                if ast::is_block(node) && ast::is_function_like_declaration(parent) {
                    let node_end = node.end().expect_source("parsed nodes have source positions");
                    if !s.positions_are_on_same_line(
                        astnav::get_start_of_node(node, source_file, false),
                        node_end,
                    ) {
                        let start = astnav::get_start_of_node(node, source_file, false);
                        s.push_selection_range(start, node_end);
                    }
                }

                // Synthesize a stop for '${ ... }' since '${' and '}' actually belong to siblings.
                if ast::is_template_span(parent) {
                    let template_span = parent.as_template_span();
                    // Start from just before the '${' and end after the '}'
                    // The '${' is 2 characters before the expression start
                    let span_start = node
                        .pos()
                        .expect_source("parsed template nodes have source positions")
                        .checked_sub(2)
                        .expect("template expression begins after `${`");
                    // The '}' is the first character of the template literal (middle or tail)
                    let span_end =
                        astnav::get_start_of_node(template_span.literal, source_file, false) + 1;
                    // Validate the positions are reasonable
                    let text = source_file.text();
                    if span_end as usize <= text.len() && span_start < span_end {
                        s.push_selection_range(span_start, span_end);
                    }
                }

                if !should_skip_node(node, Some(parent)) {
                    let start = astnav::get_start_of_node(node, source_file, false);
                    let end = node.end().expect_source("parsed nodes have source positions");
                    s.push_selection_range(start, end);

                    if ast::is_mapped_type_node(node) {
                        let mut selection_parent = node;
                        loop {
                            let mut selection_child: Option<P<Node>> = None;
                            for child in
                                get_selection_children(&factory, selection_parent, source_file)
                            {
                                let child_start = scanner::get_token_pos_of_node(
                                    child,
                                    source_file,
                                    true, /*includeJSDoc*/
                                );
                                if child_start > s.pos {
                                    break;
                                }
                                if s.position_should_snap_to_node(child) {
                                    s.push_selection_range(
                                        child_start,
                                        child
                                            .end()
                                            .expect_source("parsed nodes have source positions"),
                                    );
                                    selection_child = Some(child);
                                    break;
                                }
                            }
                            match selection_child {
                                Some(child) if ast::is_syntax_list(child) => {
                                    selection_parent = child;
                                }
                                _ => break,
                            }
                        }
                    }

                    // String literals should have a stop both inside and outside their quotes.
                    if ast::is_string_literal(node)
                        || node.kind() == Kind::TemplateExpression
                        || node.kind() == Kind::NoSubstitutionTemplateLiteral
                    {
                        // Only add inner content range if there's actually content (handles unterminated literals)
                        if start + 1 < end.saturating_sub(1) {
                            s.push_selection_range(start + 1, end - 1);
                        }
                    }
                }

                *next = Some(node);
            }
        };

        // Visit JSDoc nodes first if they exist
        for &jsdoc in cur.jsdoc(Some(source_file.get())) {
            visit(&mut s, &mut next, jsdoc);
        }

        for child in collect_visited_children(cur) {
            match child {
                VisitedChild::Node(node) => visit(&mut s, &mut next, node),
                VisitedChild::Nodes(nodes) => {
                    let nodes_slice = nodes.nodes();
                    if !nodes_slice.is_empty() {
                        let should_skip_list = ast::is_variable_declaration_list(parent)
                            || ast::is_template_expression(parent);

                        if !should_skip_list {
                            let start =
                                astnav::get_start_of_node(nodes_slice[0], source_file, false);
                            let end = nodes_slice[nodes_slice.len() - 1]
                                .end()
                                .expect_source("parsed nodes have source positions");

                            if start <= s.pos && s.pos < end {
                                s.push_selection_range(start, end);
                            }
                        }
                    }
                    for &node in nodes_slice {
                        visit(&mut s, &mut next, node);
                    }
                }
            }
        }
        current = next;
    }
    s.ranges.build(root)
}
