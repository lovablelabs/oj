use std::sync::Arc;

use indexmap::map::Entry;
use rustc_hash::FxHashSet;
use tsrs_ast::{self as ast, Kind, Node, NodeFactory, NodeList, SourceFile};
use tsrs_core::collections::OrderedMap;
use tsrs_core::context::Context;
use tsrs_core::stringutil;
use tsrs_core::{CompilerOptions, P, TextRange};
use tsrs_lsproto as lsproto;
use tsrs_printer::{self as printer, EmitContext};
use tsrs_scanner as scanner;

use super::delete::positions_are_on_same_line;
use super::trackerimpl::{has_comments_before_line_break, need_semicolon_between};
use crate::astnav;
use crate::format;
use crate::lsconv::Converters;
use crate::lsutil::FormatCodeSettings;
use crate::spanmap::{self, Feature};

// tracker.go:21
#[derive(Clone, Debug, Default)]
pub struct NodeOptions {
    // Text to be inserted before the new node
    pub(crate) prefix: String,

    // Text to be inserted after the new node
    pub(crate) suffix: String,

    // Text of inserted node will be formatted with this indentation, otherwise indentation will be inferred from the old node
    pub(crate) indentation: Option<i32>,

    // Text of inserted node will be formatted with this delta, otherwise delta will be inferred from the new node kind
    pub(crate) delta: Option<i32>,

    pub(crate) leading_trivia_option: LeadingTriviaOption,
    pub(crate) trailing_trivia_option: TrailingTriviaOption,
    pub(crate) joiner: String,
}

// tracker.go:40
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LeadingTriviaOption {
    #[default]
    None = 0,
    Exclude = 1,
    IncludeAll = 2,
    JSDoc = 3,
    StartLine = 4,
}

// tracker.go:50
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TrailingTriviaOption {
    #[default]
    None = 0,
    Exclude = 1,
    ExcludeWhitespace = 2,
    Include = 3,
}

// tracker.go:58
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackerEditKind {
    Text = 1,
    ReplaceWithSingleNode = 3,
    ReplaceWithMultipleNodes = 4,
}

// tracker.go:67
#[derive(Clone, Debug)]
pub struct TrackerEdit {
    pub(crate) kind: TrackerEditKind,
    pub(crate) text_range: TextRange,

    pub(crate) new_text: String, // kind == text

    pub(crate) node: Option<P<Node>>, // single
    pub(crate) nodes: Vec<P<Node>>,   // multiple
    pub(crate) options: NodeOptions,
}

// tracker.go:78
pub struct NodesInsertedAtStartState {
    node: P<Node>,
    source_file: P<SourceFile>,
}

// tracker.go:83
pub struct Tracker {
    // initialized with
    pub(crate) format_settings: FormatCodeSettings,
    pub(crate) new_line: String,
    pub(crate) converters: Arc<Converters>,
    pub(crate) ctx: Context,
    pub(crate) emit_context: P<EmitContext>,

    pub(crate) node_factory: NodeFactory,

    // Go `collections.MultiMap` (random iteration order; the edits are sorted per file afterwards).
    pub(crate) changes: OrderedMap<P<SourceFile>, Vec<TrackerEdit>>,
    deleted_nodes: Vec<DeletedNode>,
    nodes_with_insertions_at_start: OrderedMap<P<Node>, NodesInsertedAtStartState>,

    // unmappableFiles collects the files for which an edit could not be represented within a single
    // verbatim span of the original text. GetChanges drops their edits so a partial, corrupting change is
    // never emitted for a content-mapped file.
    pub(crate) unmappable_files: FxHashSet<String>,
}

impl std::ops::Deref for Tracker {
    type Target = NodeFactory;
    fn deref(&self) -> &NodeFactory {
        &self.node_factory
    }
}

// tracker.go:107
#[derive(Clone, Copy)]
pub struct DeletedNode {
    source_file: P<SourceFile>,
    node: P<Node>,
}

// tracker.go:112
pub(crate) fn new_tracker(
    ctx: &Context,
    compiler_options: &CompilerOptions,
    format_options: FormatCodeSettings,
    converters: Arc<Converters>,
) -> Tracker {
    let emit_context = printer::new_emit_context();
    let new_line = compiler_options.new_line.get_new_line_character().to_string();
    let ctx = with_format_code_settings(ctx, format_options.clone(), &new_line); // !!! formatSettings in context?
    Tracker {
        node_factory: emit_context.factory.as_node_factory().clone(),
        emit_context,
        changes: OrderedMap::default(),
        deleted_nodes: Vec::new(),
        ctx,
        converters,
        format_settings: format_options,
        new_line,
        nodes_with_insertions_at_start: OrderedMap::default(),
        unmappable_files: FxHashSet::default(),
    }
}

// Go `format.WithFormatCodeSettings(ctx, ...)`: the formatter settings travel in the request context as a
// `format::FormatContext` value.
fn with_format_code_settings(
    ctx: &Context,
    options: FormatCodeSettings,
    new_line: &str,
) -> Context {
    let base = format_context(ctx);
    ctx.with_value(format::with_format_code_settings(&base, options, new_line))
}

pub(crate) fn format_context(ctx: &Context) -> format::FormatContext {
    ctx.value::<format::FormatContext>().cloned().unwrap_or_default()
}

impl Tracker {
    fn add_change(&mut self, source_file: P<SourceFile>, edit: TrackerEdit) {
        self.changes.entry(source_file).or_default().push(edit);
    }

    // tracker.go:134
    // GetChanges returns the accumulated text edits grouped by file name. Any file whose edits could not be
    // faithfully mapped back onto content-mapped original text is omitted from the returned map, and its name
    // is included in the returned slice. Dropping the whole file (rather than the individual edit) keeps a
    // logical change atomic, and returning the result inline means a caller cannot forget to check it or
    // accidentally emit a partial, corrupting change.
    // Note: after calling this, the Tracker object must be discarded!
    pub(crate) fn get_changes(
        &mut self,
    ) -> (OrderedMap<String, Vec<lsproto::TextEdit>>, Vec<String>) {
        self.finish_delete_declarations();
        self.finish_nodes_with_insertions_at_start();
        let mut changes = self.get_text_changes_from_changes();
        // !!! changes for new files
        if self.unmappable_files.is_empty() {
            return (changes, Vec::new());
        }
        let mut unmappable = Vec::with_capacity(self.unmappable_files.len());
        #[expect(
            clippy::iter_over_hash_type,
            reason = "removals of distinct keys commute and the names are sorted after; Go ranges the set too"
        )]
        for file_name in &self.unmappable_files {
            changes.shift_remove(file_name);
            unmappable.push(file_name.clone());
        }
        unmappable.sort();
        (changes, unmappable)
    }

    // tracker.go:154
    // fromLSPEditRange converts an LSP range to a source file range. For a content-mapped file, an original
    // range may have several projections; this selects the one belonging to sourceFile. A range with no exact
    // projection cannot be written back and marks the file unmappable.
    #[expect(
        clippy::wrong_self_convention,
        reason = "The method name and mutable receiver match the upstream Go operation."
    )]
    fn from_lsp_edit_range(
        &mut self,
        source_file: P<SourceFile>,
        lsproto_range: lsproto::Range,
    ) -> TextRange {
        let spans = self.converters.from_lsp_range_for_source_file(
            source_file,
            lsproto_range,
            Feature::All,
        );
        if let Some(result) = spans.iter().find_map(|span| {
            if span.fidelity.is_exact() && span.script == source_file {
                Some(span.span)
            } else {
                None
            }
        }) {
            return result;
        }
        self.unmappable_files.insert(source_file.original_file_name().to_string());
        if !spans.is_empty() {
            return spans[0].span;
        }
        TextRange::new(0, 0)
    }

    // tracker.go:172
    // toLSPEditRange converts a source file range to an LSP range. For a content-mapped file, the range is
    // mapped back to the original document. If it does not fall entirely within a single verbatim span, the
    // edit cannot be represented safely: the file is recorded so GetChanges drops its edits, and a best-effort
    // range is returned so the accumulated edits stay well-formed.
    #[expect(
        clippy::wrong_self_convention,
        reason = "The method name and mutable receiver match the upstream Go operation."
    )]
    pub(crate) fn to_lsp_edit_range(
        &mut self,
        source_file: P<SourceFile>,
        text_range: TextRange,
    ) -> lsproto::Range {
        let (r, fidelity) = self.converters.to_lsp_range(&source_file, text_range);
        if !fidelity.is_exact() {
            // The range does not map into a single verbatim span, so the edit cannot be represented safely in
            // the original text. Record the file so GetChanges drops its edits, keeping the best-effort range so
            // the accumulated edits stay well-formed.
            self.unmappable_files.insert(source_file.original_file_name().to_string());
        }
        r
    }

    // tracker.go:183
    pub(crate) fn replace_node(
        &mut self,
        source_file: P<SourceFile>,
        old_node: P<Node>,
        new_node: P<Node>,
        options: Option<NodeOptions>,
    ) {
        let options = options.unwrap_or_else(|| {
            // defaults to `useNonAdjustedPositions`
            NodeOptions {
                leading_trivia_option: LeadingTriviaOption::Exclude,
                trailing_trivia_option: TrailingTriviaOption::Exclude,
                ..Default::default()
            }
        });
        let range = self.get_adjusted_range(
            source_file,
            old_node,
            old_node,
            options.leading_trivia_option,
            options.trailing_trivia_option,
        );
        self.replace_range(source_file, range, new_node, options);
    }

    // tracker.go:194
    pub(crate) fn replace_node_with_nodes(
        &mut self,
        source_file: P<SourceFile>,
        old_node: P<Node>,
        new_nodes: Vec<P<Node>>,
        options: Option<NodeOptions>,
    ) {
        let options = options.unwrap_or_else(|| NodeOptions {
            leading_trivia_option: LeadingTriviaOption::Exclude,
            trailing_trivia_option: TrailingTriviaOption::Exclude,
            ..Default::default()
        });
        let range = self.get_adjusted_range(
            source_file,
            old_node,
            old_node,
            options.leading_trivia_option,
            options.trailing_trivia_option,
        );
        self.replace_range_with_nodes(source_file, range, new_nodes, options);
    }

    // tracker.go:205
    // ReplaceRange replaces textRange in sourceFile with newNode.
    fn replace_range(
        &mut self,
        source_file: P<SourceFile>,
        text_range: TextRange,
        new_node: P<Node>,
        options: NodeOptions,
    ) {
        self.add_change(
            source_file,
            TrackerEdit {
                kind: TrackerEditKind::ReplaceWithSingleNode,
                text_range,
                options,
                node: Some(new_node),
                nodes: Vec::new(),
                new_text: String::new(),
            },
        );
    }

    // tracker.go:211
    // ReplaceRangeWithText replaces an LSP range with text. For a content-mapped file, the LSP range is in the
    // original document; text may be placed at any exact projection because it does not need formatting context.
    pub(crate) fn replace_range_with_text(
        &mut self,
        source_file: P<SourceFile>,
        lsproto_range: lsproto::Range,
        text: &str,
    ) {
        let range = self.from_lsp_edit_range(source_file, lsproto_range);
        self.replace_text_range_with_text(source_file, range, text);
    }

    // tracker.go:217
    // ReplaceTextRangeWithText replaces textRange in sourceFile with text. For a content-mapped file, GetChanges
    // maps the range back to the original document and drops the file if the edit cannot be represented there.
    pub(crate) fn replace_text_range_with_text(
        &mut self,
        source_file: P<SourceFile>,
        text_range: TextRange,
        text: &str,
    ) {
        self.add_change(
            source_file,
            TrackerEdit {
                kind: TrackerEditKind::Text,
                text_range,
                new_text: text.to_string(),
                node: None,
                nodes: Vec::new(),
                options: NodeOptions::default(),
            },
        );
    }

    // tracker.go:222
    // ReplaceRangeWithNodes replaces textRange in sourceFile with newNodes.
    fn replace_range_with_nodes(
        &mut self,
        source_file: P<SourceFile>,
        text_range: TextRange,
        new_nodes: Vec<P<Node>>,
        options: NodeOptions,
    ) {
        if new_nodes.len() == 1 {
            self.replace_range(source_file, text_range, new_nodes[0], options);
            return;
        }
        self.add_change(
            source_file,
            TrackerEdit {
                kind: TrackerEditKind::ReplaceWithMultipleNodes,
                text_range,
                nodes: new_nodes,
                options,
                node: None,
                new_text: String::new(),
            },
        );
    }

    // tracker.go:231
    // insertTextAt inserts text at an offset in sourceFile.
    fn insert_text_at(&mut self, source_file: P<SourceFile>, pos: u32, text: &str) {
        self.replace_text_range_with_text(source_file, TextRange::new(pos, pos), text);
    }

    // tracker.go:236
    // InsertText inserts text at an LSP position.
    pub(crate) fn insert_text(
        &mut self,
        source_file: P<SourceFile>,
        pos: lsproto::Position,
        text: &str,
    ) {
        self.replace_range_with_text(source_file, lsproto::Range { start: pos, end: pos }, text);
    }

    // tracker.go:240
    pub(crate) fn insert_node_at(
        &mut self,
        source_file: P<SourceFile>,
        pos: u32,
        new_node: P<Node>,
        options: NodeOptions,
    ) {
        self.replace_range(source_file, TextRange::new(pos, pos), new_node, options);
    }

    // tracker.go:244
    fn insert_nodes_at(
        &mut self,
        source_file: P<SourceFile>,
        pos: u32,
        new_nodes: Vec<P<Node>>,
        options: NodeOptions,
    ) {
        self.replace_range_with_nodes(source_file, TextRange::new(pos, pos), new_nodes, options);
    }

    // tracker.go:248
    pub(crate) fn insert_node_after(
        &mut self,
        source_file: P<SourceFile>,
        after: P<Node>,
        new_node: P<Node>,
    ) {
        let end_position = self.end_pos_for_insert_node_after(source_file, after, new_node);
        let options = self.get_insert_node_after_options(source_file, after);
        self.insert_node_at(source_file, end_position, new_node, options);
    }

    // tracker.go:253
    pub(crate) fn insert_nodes_after(
        &mut self,
        source_file: P<SourceFile>,
        after: P<Node>,
        new_nodes: Vec<P<Node>>,
    ) {
        let end_position = self.end_pos_for_insert_node_after(source_file, after, new_nodes[0]);
        let options = self.get_insert_node_after_options(source_file, after);
        self.insert_nodes_at(source_file, end_position, new_nodes, options);
    }

    // tracker.go:258
    pub(crate) fn insert_node_before(
        &mut self,
        source_file: P<SourceFile>,
        before: P<Node>,
        new_node: P<Node>,
        blank_line_between: bool,
        leading_trivia_option: LeadingTriviaOption,
    ) {
        let pos =
            self.get_adjusted_start_position(source_file, before, leading_trivia_option, false);
        let options = self.get_options_for_insert_node_before(before, new_node, blank_line_between);
        self.insert_node_at(source_file, pos, new_node, options);
    }

    // tracker.go:265
    // TryInsertTypeAnnotation inserts a type annotation after the appropriate position on a node
    // (after the close paren for function-like, after the name/exclamation/question for variable-like).
    // Returns true if successful.
    pub(crate) fn try_insert_type_annotation(
        &mut self,
        source_file: P<SourceFile>,
        node: P<Node>,
        type_node: P<Node>,
    ) -> bool {
        let end_node: Option<P<Node>> = if ast::is_function_like(Some(node)) {
            let mut e = astnav::find_child_of_kind(node, Kind::CloseParenToken, source_file);
            if e.is_none() {
                if !ast::is_arrow_function(node) {
                    return false;
                }
                // If no `)`, is an arrow function `x => x`, so use the end of the first parameter
                let params = node.parameters();
                if params.is_empty() {
                    return false;
                }
                e = Some(params[0]);
            }
            e
        } else {
            let mut e = match node.kind() {
                Kind::VariableDeclaration => node.as_variable_declaration().exclamation_token(),
                Kind::PropertySignature => node.as_property_signature_declaration().postfix_token(),
                Kind::PropertyDeclaration => node.as_property_declaration().postfix_token(),
                Kind::Parameter => node.as_parameter_declaration().question_token(),
                _ => None,
            };
            if e.is_none() {
                e = node.name();
            }
            e
        };
        let Some(end_node) = end_node else {
            return false;
        };
        self.insert_node_at(
            source_file,
            end_node.end().expect_source("parsed nodes have source positions"),
            type_node,
            NodeOptions { prefix: ": ".to_string(), ..Default::default() },
        );
        true
    }

    // tracker.go:304
    // ParenthesizeArrowParameters wraps the parameters of a paren-less arrow function in `(` and `)`.
    // This is a no-op if the arrow function already has parens.
    pub(crate) fn parenthesize_arrow_parameters(
        &mut self,
        source_file: P<SourceFile>,
        arrow_func: P<Node>,
    ) {
        if astnav::find_child_of_kind(arrow_func, Kind::CloseParenToken, source_file).is_some() {
            return;
        }
        let params = arrow_func.parameters();
        if params.is_empty() {
            return;
        }
        let first_param = params[0];
        let last_param = params[params.len() - 1];
        let start_pos = astnav::get_start_of_node(first_param, source_file, false);
        self.insert_text_at(source_file, start_pos, "(");
        self.insert_text_at(
            source_file,
            last_param.end().expect_source("parsed parameters have source positions"),
            ")",
        );
    }

    // tracker.go:320
    // InsertModifierBefore inserts a modifier token (like 'type') before a node with a trailing space.
    pub(crate) fn insert_modifier_before(
        &mut self,
        source_file: P<SourceFile>,
        modifier: Kind,
        before: P<Node>,
    ) {
        let pos = astnav::get_start_of_node(before, source_file, false);
        let token = self.new_token(modifier);
        token.set_loc(TextRange::new(pos, pos));
        token.set_parent(before.parent());
        self.insert_node_at(
            source_file,
            pos,
            token,
            NodeOptions { suffix: " ".to_string(), ..Default::default() },
        );
    }

    // tracker.go:330
    // Delete queues a node for deletion with smart handling of list items, imports, etc.
    // The actual deletion happens in finishDeleteDeclarations during GetChanges.
    pub(crate) fn delete(&mut self, source_file: P<SourceFile>, node: P<Node>) {
        self.deleted_nodes.push(DeletedNode { source_file, node });
    }

    // tracker.go:335
    // DeleteRange deletes a text range from the source file.
    pub(crate) fn delete_range(&mut self, source_file: P<SourceFile>, text_range: TextRange) {
        self.replace_text_range_with_text(source_file, text_range, "");
    }

    // tracker.go:341
    // DeleteNode deletes a node immediately with specified trivia options.
    // Stop! Consider using Delete instead, which has logic for deleting nodes from delimited lists.

    // tracker.go:346
    // DeleteNodeRange deletes a range of nodes with specified trivia options.
    pub(crate) fn delete_node_range(
        &mut self,
        source_file: P<SourceFile>,
        start_node: P<Node>,
        end_node: P<Node>,
        leading_trivia: LeadingTriviaOption,
        trailing_trivia: TrailingTriviaOption,
    ) {
        let start_position =
            self.get_adjusted_start_position(source_file, start_node, leading_trivia, false);
        let end_position = Self::get_adjusted_end_position(source_file, end_node, trailing_trivia);
        self.replace_text_range_with_text(
            source_file,
            TextRange::new(start_position, end_position),
            "",
        );
    }

    // tracker.go:353
    // finishDeleteDeclarations processes all queued deletions with smart handling for lists and trailing commas.
    fn finish_delete_declarations(&mut self) {
        // Go map (random iteration order); the edits are sorted afterwards.
        let mut deleted_nodes_in_lists: OrderedMap<P<Node>, bool> = OrderedMap::default();

        let deleted_nodes = self.deleted_nodes.clone();
        for deleted in &deleted_nodes {
            // Skip if this node is contained within another deleted node
            let is_contained = deleted_nodes.iter().any(|other| {
                other.source_file == deleted.source_file
                    && other.node != deleted.node
                    && range_contains_range_exclusive(other.node, deleted.node)
            });
            if is_contained {
                continue;
            }

            super::delete::delete_declaration(
                self,
                &mut deleted_nodes_in_lists,
                deleted.source_file,
                deleted.node,
            );
        }

        // Handle trailing commas for last elements in lists
        let nodes: Vec<P<Node>> = deleted_nodes_in_lists.keys().copied().collect();
        for node in nodes {
            let source_file = ast::get_source_file_of_node(node).unwrap();
            let Some(list) = format::get_containing_list(node, source_file) else {
                continue;
            };
            if node != list.nodes()[list.nodes().len() - 1] {
                continue;
            }

            let last_non_deleted_index = (0..list.nodes().len() - 1).rev().find(|&index| {
                !deleted_nodes_in_lists.get(&list.nodes()[index]).copied().unwrap_or(false)
            });

            if let Some(last_non_deleted_index) = last_non_deleted_index {
                let start = list.nodes()[last_non_deleted_index]
                    .end()
                    .expect_source("parsed nodes have source positions");
                let end = self.start_position_to_delete_node_in_list(
                    source_file,
                    list.nodes()[last_non_deleted_index + 1],
                );
                self.replace_text_range_with_text(source_file, TextRange::new(start, end), "");
            }
        }
    }

    // tracker.go:397
    fn end_pos_for_insert_node_after(
        &mut self,
        source_file: P<SourceFile>,
        after: P<Node>,
        new_node: P<Node>,
    ) -> u32 {
        let after_end = after.end().expect_source("parsed nodes have source positions");
        if need_semicolon_between(after, new_node)
            && source_file.text().as_bytes()[after_end as usize - 1] != b';'
        {
            // check if previous statement ends with semicolon
            // if not - insert semicolon to preserve the code from changing the meaning due to ASI
            let end_pos = after_end;
            let semicolon = self.new_token(Kind::SemicolonToken);
            semicolon.set_loc(TextRange::new(after_end, after_end));
            semicolon.set_parent(after.parent());
            self.replace_range(
                source_file,
                TextRange::new(end_pos, end_pos),
                semicolon,
                NodeOptions::default(),
            );
        }
        Self::get_adjusted_end_position(source_file, after, TrailingTriviaOption::None)
    }

    // tracker.go:420
    /**
     * This function should be used to insert nodes in lists when nodes don't carry separators as the part of the node range,
     * i.e. arguments in arguments lists, parameters in parameter lists etc.
     * Note that separators are part of the node in statements and class elements.
     */
    pub(crate) fn insert_node_in_list_after(
        &mut self,
        source_file: P<SourceFile>,
        after: P<Node>,
        new_node: P<Node>,
        containing_list: Option<P<NodeList>>,
    ) {
        let containing_list =
            containing_list.or_else(|| format::get_containing_list(after, source_file));
        let Some(containing_list) = containing_list else {
            // Debug.fail("node is not a list element")
            return;
        };
        let Some(index) = containing_list.nodes().iter().position(|&n| n == after) else {
            return;
        };
        let end = after.end().expect_source("parsed nodes have source positions");
        if index != containing_list.nodes().len() - 1 {
            // any element except the last one
            // use next sibling as an anchor
            let next_token = astnav::get_token_at_position(source_file, end);
            if is_separator(after, Some(next_token)) {
                // for list
                // a, b, c
                // create change for adding 'e' after 'a' as
                // - find start of next element after a (it is b)
                // - use next element start as start and end position in final change
                // - build text of change by formatting the text of node + whitespace trivia of b

                // in multiline case it will work as
                //   a,
                //   b,
                //   c,
                // result - '*' denotes leading trivia that will be inserted after new text (displayed as '#')
                //   a,
                //   insertedtext<separator>#
                // ###b,
                //   c,
                let next_node = containing_list.nodes()[index + 1];
                let start_pos = scanner::skip_trivia_ex(
                    source_file.text(),
                    next_node.pos().expect_source("parsed nodes have source positions"),
                    Some(&scanner::SkipTriviaOptions {
                        stop_after_line_break: false,
                        stop_at_comments: true,
                        ..Default::default()
                    }),
                );

                // write separator and leading trivia of the next element as suffix
                let suffix = format!(
                    "{}{}",
                    scanner::token_to_string(next_token.kind()),
                    &source_file.text()[next_token
                        .end()
                        .expect_source("parsed tokens have source positions")
                        as usize..start_pos as usize],
                );
                self.insert_nodes_at(
                    source_file,
                    start_pos,
                    vec![new_node],
                    NodeOptions { suffix, ..Default::default() },
                );
            }
            return;
        }

        let after_start = astnav::get_start_of_node(after, source_file, false);
        let after_start_line_position =
            format::get_line_start_position_for_position(after_start, source_file);

        // insert element after the last element in the list that has more than one item
        // pick the element preceding the after element to:
        // - pick the separator
        // - determine if list is a multiline
        let mut multiline_list = false;

        // if list has only one element then we'll format is as multiline if node has comment in trailing trivia, or as singleline otherwise
        // i.e. var x = 1 // this is x
        //     | new element will be inserted at this position
        let mut separator = Kind::CommaToken; // SyntaxKind.CommaToken | SyntaxKind.SemicolonToken
        if containing_list.nodes().len() != 1 {
            // otherwise, if list has more than one element, pick separator from the list
            let token_before_insert_position = astnav::find_preceding_token(
                source_file,
                after.pos().expect_source("parsed nodes have source positions"),
            );
            separator = if is_separator(after, token_before_insert_position) {
                token_before_insert_position.unwrap().kind()
            } else {
                Kind::CommaToken
            };
            // determine if list is multiline by checking lines of after element and element that precedes it.
            let after_minus_one_start_line_position = format::get_line_start_position_for_position(
                astnav::get_start_of_node(containing_list.nodes()[index - 1], source_file, false),
                source_file,
            );
            multiline_list = after_minus_one_start_line_position != after_start_line_position;
        }
        if has_comments_before_line_break(source_file.text(), end)
            || !positions_are_on_same_line(
                containing_list.loc.get().pos().expect_source("parsed lists have source positions"),
                containing_list.loc.get().end().expect_source("parsed lists have source positions"),
                source_file,
            )
        {
            // in this case we'll always treat containing list as multiline
            multiline_list = true;
        }
        if multiline_list {
            // insert separator immediately following the 'after' node to preserve comments in trailing trivia
            let separator_token = self.new_token(separator);
            let separator_string = scanner::token_to_string(separator);
            separator_token.set_loc(TextRange::new(
                end,
                end + u32::try_from(separator_string.len()).expect("separator length exceeds u32"),
            ));
            separator_token.set_parent(after.parent());
            let end_pos = end;
            self.replace_range(
                source_file,
                TextRange::new(end_pos, end_pos),
                separator_token,
                NodeOptions::default(),
            );
            // use the same indentation as 'after' item
            let indentation = format::find_first_non_whitespace_column(
                after_start_line_position,
                after_start,
                source_file,
                &self.format_settings,
            );
            // insert element before the line break on the line that contains 'after' element
            let mut insert_pos = scanner::skip_trivia_ex(
                source_file.text(),
                end,
                Some(&scanner::SkipTriviaOptions {
                    stop_after_line_break: true,
                    stop_at_comments: false,
                    ..Default::default()
                }),
            );
            // find position before "\n" or "\r\n"
            while insert_pos != end
                && stringutil::is_line_break(u32::from(
                    source_file.text().as_bytes()[insert_pos as usize - 1],
                ))
            {
                insert_pos -= 1;
            }
            let insert_ls_pos = insert_pos;
            let prefix = self.new_line.clone();
            self.replace_range(
                source_file,
                TextRange::new(insert_ls_pos, insert_ls_pos),
                new_node,
                NodeOptions { indentation: Some(indentation), prefix, ..Default::default() },
            );
        } else {
            let separator_string = scanner::token_to_string(separator);
            let end_pos = end;
            self.replace_range(
                source_file,
                TextRange::new(end_pos, end_pos),
                new_node,
                NodeOptions { prefix: format!("{separator_string} "), ..Default::default() },
            );
        }
    }

    // tracker.go:522
    // InsertImportSpecifierAtIndex inserts a new import specifier at the specified index in a NamedImports list
    pub(crate) fn insert_import_specifier_at_index(
        &mut self,
        source_file: P<SourceFile>,
        new_specifier: P<Node>,
        named_imports: P<Node>,
        index: usize,
    ) {
        let elements = named_imports.as_named_imports().elements.nodes();

        let mut prev_specifier: Option<P<Node>> = None;
        if index > 0 && index - 1 < elements.len() {
            prev_specifier = Some(elements[index - 1]);
        }
        if let Some(prev_specifier) = prev_specifier {
            self.insert_node_in_list_after(source_file, prev_specifier, new_specifier, None);
        } else {
            let blank = !positions_are_on_same_line(
                astnav::get_start_of_node(elements[0], source_file, false),
                astnav::get_start_of_node(
                    named_imports.parent().unwrap().parent().unwrap(),
                    source_file,
                    false,
                ),
                source_file,
            );
            self.insert_node_before(
                source_file,
                elements[0],
                new_specifier,
                blank,
                LeadingTriviaOption::None,
            );
        }
    }

    // tracker.go:543
    pub(crate) fn insert_at_top_of_file(
        &mut self,
        source_file: P<SourceFile>,
        insert: &[P<Node>],
        blank_line_between: bool,
    ) {
        if insert.is_empty() {
            return;
        }

        let mut pos = Self::get_insertion_position_at_source_file_top(source_file);
        let mut original_pos = pos;
        // A content mapper may synthesize a header. Advance to the first writable segment so the insertion
        // maps exactly to the original file, and use its original position when deciding leading trivia.
        if let Some(span_map) = crate::lsconv::Script::span_map(&source_file) {
            for segment in span_map.segments() {
                if segment.kind != spanmap::Kind::VERBATIM || segment.virtual_end <= pos {
                    continue;
                }
                if segment.virtual_start > pos {
                    pos = segment.virtual_start;
                }
                original_pos = segment.original_start + pos - segment.virtual_start;
                break;
            }
        }
        let mut options = NodeOptions::default();
        if original_pos != 0 {
            options.prefix.clone_from(&self.new_line);
        }
        let text = source_file.text();
        if text.is_empty() || !stringutil::is_line_break(u32::from(text.as_bytes()[pos as usize])) {
            options.suffix.clone_from(&self.new_line);
        }
        if blank_line_between {
            options.suffix.push_str(&self.new_line);
        }

        if insert.len() == 1 {
            self.insert_node_at(source_file, pos, insert[0], options);
        } else {
            self.insert_nodes_at(source_file, pos, insert.to_vec(), options);
        }
    }

    // tracker.go:582
    pub(crate) fn insert_member_at_start(
        &mut self,
        source_file: P<SourceFile>,
        node: P<Node>,
        new_element: P<Node>,
    ) {
        self.insert_node_at_start_worker(source_file, node, new_element);
    }

    // tracker.go:586
    fn insert_node_at_start_worker(
        &mut self,
        source_file: P<SourceFile>,
        node: P<Node>,
        new_element: P<Node>,
    ) {
        let indentation = self
            .try_compute_indentation_from_existing_members(source_file, node)
            .unwrap_or_else(|| self.try_compute_indentation_for_new_member(source_file, node));

        let Some(members) = get_members_or_properties(node) else {
            return;
        };

        let options = self.get_insert_node_at_start_insert_options(source_file, node, indentation);
        self.insert_node_at(
            source_file,
            members.loc.get().pos().expect_source("parsed member lists have source positions"),
            new_element,
            options,
        );
    }

    // tracker.go:600
    fn try_compute_indentation_for_new_member(
        &self,
        source_file: P<SourceFile>,
        node: P<Node>,
    ) -> i32 {
        let node_start = astnav::get_start_of_node(node, source_file, false);
        let line_start = format::get_line_start_position_for_position(node_start, source_file);

        let mut tab_size = self.format_settings.tab_size;
        if tab_size <= 0 {
            tab_size = 4;
        }

        let mut indent_size = self.format_settings.indent_size;
        if indent_size <= 0 {
            indent_size = 4;
        }
        find_indentation_column(source_file.text(), line_start, node_start, tab_size).unwrap_or(0)
            + indent_size
    }

    // tracker.go:616
    fn try_compute_indentation_from_existing_members(
        &self,
        source_file: P<SourceFile>,
        node: P<Node>,
    ) -> Option<i32> {
        let members = get_members_or_properties(node)?;

        let mut indentation = None;
        let text = source_file.text();
        let mut tab_size = self.format_settings.tab_size;
        let mut last = node;

        if tab_size <= 0 {
            tab_size = 4;
        }

        for &member in members.nodes() {
            if printer::range_start_positions_are_on_same_line(
                last.loc(),
                member.loc(),
                source_file,
            ) {
                return None;
            }

            let member_start = astnav::get_start_of_node(member, source_file, false);
            let line_start =
                format::get_line_start_position_for_position(member_start, source_file);
            let column = find_indentation_column(text, line_start, member_start, tab_size)?;

            if let Some(indentation) = indentation {
                if indentation != column {
                    return None;
                }
                last = member;
                continue;
            }

            indentation = Some(column);
            last = member;
        }

        indentation
    }

    // tracker.go:661
    fn get_insert_node_after_options(
        &self,
        source_file: P<SourceFile>,
        node: P<Node>,
    ) -> NodeOptions {
        let new_line_char = self.new_line.clone();
        let mut options = match node.kind() {
            Kind::Parameter => {
                // default opts
                NodeOptions::default()
            }
            Kind::ClassDeclaration | Kind::ModuleDeclaration => NodeOptions {
                prefix: new_line_char.clone(),
                suffix: new_line_char,
                ..Default::default()
            },

            Kind::VariableDeclaration | Kind::StringLiteral | Kind::Identifier => {
                NodeOptions { prefix: ", ".to_string(), ..Default::default() }
            }

            Kind::PropertyAssignment => {
                NodeOptions { suffix: format!(",{new_line_char}"), ..Default::default() }
            }

            Kind::ExportKeyword => NodeOptions { prefix: " ".to_string(), ..Default::default() },

            _ => {
                // Else we haven't handled this kind of node yet -- add it
                assert!(
                    ast::is_statement(node) || ast::is_class_or_type_element(node),
                    "unimplemented node type {:?} in changeTracker.getInsertNodeAfterOptions",
                    node.kind()
                );
                NodeOptions { suffix: new_line_char, ..Default::default() }
            }
        };
        if node.end() == source_file.as_node().end() && ast::is_statement(node) {
            options.prefix = format!("{}{}", self.new_line, options.prefix);
        }

        options
    }

    // tracker.go:694
    fn get_options_for_insert_node_before(
        &self,
        before: P<Node>,
        inserted: P<Node>,
        blank_line_between: bool,
    ) -> NodeOptions {
        if ast::is_statement(before) || ast::is_class_or_type_element(before) {
            if blank_line_between {
                return NodeOptions {
                    suffix: format!("{}{}", self.new_line, self.new_line),
                    ..Default::default()
                };
            }
            return NodeOptions { suffix: self.new_line.clone(), ..Default::default() };
        } else if before.kind() == Kind::VariableDeclaration {
            // insert `x = 1, ` into `const x = 1, y = 2;
            return NodeOptions { suffix: ", ".to_string(), ..Default::default() };
        } else if before.kind() == Kind::Parameter {
            if inserted.kind() == Kind::Parameter {
                return NodeOptions { suffix: ", ".to_string(), ..Default::default() };
            }
            return NodeOptions::default();
        } else if (before.kind() == Kind::StringLiteral
            && before.parent().is_some_and(|p| p.kind() == Kind::ImportDeclaration))
            || before.kind() == Kind::NamedImports
        {
            return NodeOptions { suffix: ", ".to_string(), ..Default::default() };
        } else if before.kind() == Kind::ImportSpecifier {
            let mut suffix = ",".to_string();
            if blank_line_between {
                suffix.push_str(&self.new_line);
            } else {
                suffix.push(' ');
            }
            return NodeOptions { suffix, ..Default::default() };
        }
        // We haven't handled this kind of node yet -- add it
        panic!(
            "unimplemented node type {:?} in changeTracker.getOptionsForInsertNodeBefore",
            before.kind()
        );
    }

    // tracker.go:723
    fn get_insert_node_at_start_insert_options(
        &mut self,
        source_file: P<SourceFile>,
        node: P<Node>,
        indentation: i32,
    ) -> NodeOptions {
        let has_previous_insertion = match self.nodes_with_insertions_at_start.entry(node) {
            Entry::Occupied(_) => true,
            Entry::Vacant(entry) => {
                entry.insert(NodesInsertedAtStartState { node, source_file });
                false
            }
        };

        let members = get_members_or_properties(node);
        let is_object_literal = ast::is_object_literal_expression(node);
        let is_json = ast::is_json_source_file(source_file);

        let has_members = members.is_some_and(|m| !m.nodes().is_empty());

        let insert_trailing_comma = is_object_literal && (has_members || !is_json);
        let insert_leading_comma =
            is_object_literal && is_json && !has_members && has_previous_insertion;

        let mut suffix = String::new();
        if insert_trailing_comma {
            suffix = ",".to_string();
        } else if ast::is_interface_declaration(node) && !has_members {
            suffix = ";".to_string();
        }

        let mut prefix = self.new_line.clone();
        if insert_leading_comma {
            prefix = format!(",{prefix}");
        }

        NodeOptions { indentation: Some(indentation), prefix, suffix, ..Default::default() }
    }

    // tracker.go:758
    fn finish_nodes_with_insertions_at_start(&mut self) {
        let states: Vec<(P<Node>, P<SourceFile>)> =
            self.nodes_with_insertions_at_start.values().map(|s| (s.node, s.source_file)).collect();
        for (node, source_file) in states {
            let Some(open_brace) =
                astnav::find_child_of_kind(node, Kind::OpenBraceToken, source_file)
            else {
                continue;
            };

            let Some(close_brace) =
                astnav::find_child_of_kind(node, Kind::CloseBraceToken, source_file)
            else {
                continue;
            };

            let members = get_members_or_properties(node);
            let is_empty = members.is_none_or(|m| m.nodes().is_empty());
            let open_brace_end =
                open_brace.end().expect_source("parsed tokens have source positions");
            let close_brace_end =
                close_brace.end().expect_source("parsed tokens have source positions");
            let is_single_line =
                positions_are_on_same_line(open_brace_end, close_brace_end, source_file);

            if is_empty && is_single_line && open_brace_end != close_brace_end - 1 {
                self.delete_range(source_file, TextRange::new(open_brace_end, close_brace_end - 1));
            }

            if is_single_line {
                let nl = self.new_line.clone();
                self.insert_text_at(source_file, close_brace_end - 1, &nl);
            }
        }
    }
}

// tracker.go:788
fn get_members_or_properties(node: P<Node>) -> Option<P<NodeList>> {
    if ast::is_object_literal_expression(node) {
        return Some(node.property_list());
    }
    node.member_list()
}

// tracker.go:795
fn range_contains_range_exclusive(outer: P<Node>, inner: P<Node>) -> bool {
    outer.pos() < inner.pos() && inner.end() < outer.end()
}

// tracker.go:799
pub(crate) fn is_separator(node: P<Node>, candidate: Option<P<Node>>) -> bool {
    let Some(candidate) = candidate else {
        return false;
    };
    let Some(parent) = node.parent() else {
        return false;
    };
    candidate.kind() == Kind::CommaToken
        || (candidate.kind() == Kind::SemicolonToken
            && parent.kind() == Kind::ObjectLiteralExpression)
}

// tracker.go:803
fn find_indentation_column(
    text: &str,
    line_start: u32,
    member_start: u32,
    tab_size: i32,
) -> Option<i32> {
    let mut column = 0;

    let bytes = text.as_bytes();
    let mut i = line_start;
    while i < member_start && (i as usize) < bytes.len() {
        let ch = u32::from(bytes[i as usize]);

        if stringutil::is_line_break(ch) {
            return None;
        }
        if stringutil::is_white_space_single_line(ch) {
            column = advance_indentation_column(column, ch, tab_size);
            i += 1;
            continue;
        }
        return Some(column);
    }

    Some(column)
}

// tracker.go:822
fn advance_indentation_column(column: i32, ch: stringutil::Rune, tab_size: i32) -> i32 {
    if ch == '\t' as stringutil::Rune {
        return column + tab_size - (column % tab_size);
    }
    column + 1
}
