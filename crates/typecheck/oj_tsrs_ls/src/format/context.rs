#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

use std::cell::Cell;
use std::rc::Rc;

use tsrs_ast::{Kind, Node, SourceFile};
use tsrs_core::{P, TextRange, Tristate};
use tsrs_scanner as scanner;

use super::*;
use crate::astnav;
use crate::lsutil::FormatCodeSettings;

// context.go:11
pub(crate) struct FormattingContext {
    pub(crate) current_token_span: TextRangeWithKind,
    pub(crate) next_token_span: TextRangeWithKind,
    context_node: Option<P<Node>>,
    pub(crate) current_token_parent: Option<P<Node>>,
    pub(crate) next_token_parent: Option<P<Node>>,

    context_node_all_on_same_line: Cell<Tristate>,
    next_node_all_on_same_line: Cell<Tristate>,
    tokens_are_on_same_line: Cell<Tristate>,
    context_node_block_is_on_one_line: Cell<Tristate>,
    next_node_block_is_on_one_line: Cell<Tristate>,

    pub(crate) source_file: P<SourceFile>,
    pub(crate) formatting_request_kind: FormatRequestKind,
    pub(crate) options: Rc<FormatCodeSettings>,
}

// context.go:29
pub(crate) fn new_formatting_context(
    file: P<SourceFile>,
    kind: FormatRequestKind,
    options: Rc<FormatCodeSettings>,
) -> FormattingContext {
    FormattingContext {
        current_token_span: TextRangeWithKind::default(),
        next_token_span: TextRangeWithKind::default(),
        context_node: None,
        current_token_parent: None,
        next_token_parent: None,
        context_node_all_on_same_line: Cell::new(Tristate::Unknown),
        next_node_all_on_same_line: Cell::new(Tristate::Unknown),
        tokens_are_on_same_line: Cell::new(Tristate::Unknown),
        context_node_block_is_on_one_line: Cell::new(Tristate::Unknown),
        next_node_block_is_on_one_line: Cell::new(Tristate::Unknown),
        source_file: file,
        formatting_request_kind: kind,
        options,
    }
}

impl FormattingContext {
    // Go reads these fields directly; they are set by update_context before any rule predicate runs.
    pub(crate) fn context_node(&self) -> P<Node> {
        self.context_node.unwrap()
    }

    pub(crate) fn current_token_parent(&self) -> P<Node> {
        self.current_token_parent.unwrap()
    }

    pub(crate) fn next_token_parent(&self) -> P<Node> {
        self.next_token_parent.unwrap()
    }

    // context.go:38
    pub(crate) fn update_context(
        &mut self,
        cur: TextRangeWithKind,
        cur_parent: Option<P<Node>>,
        next: TextRangeWithKind,
        next_parent: Option<P<Node>>,
        common_parent: Option<P<Node>>,
    ) {
        assert!(cur_parent.is_some(), "nil current range node parent in update context");
        assert!(next_parent.is_some(), "nil next range node parent in update context");
        assert!(common_parent.is_some(), "nil common parent node in update context");
        self.current_token_span = cur;
        self.current_token_parent = cur_parent;
        self.next_token_span = next;
        self.next_token_parent = next_parent;
        self.context_node = common_parent;

        // drop cached results
        self.context_node_all_on_same_line.set(Tristate::Unknown);
        self.next_node_all_on_same_line.set(Tristate::Unknown);
        self.tokens_are_on_same_line.set(Tristate::Unknown);
        self.context_node_block_is_on_one_line.set(Tristate::Unknown);
        self.next_node_block_is_on_one_line.set(Tristate::Unknown);
    }

    // context.go:62
    fn range_is_on_one_line(&self, node: TextRange) -> Tristate {
        if range_is_on_one_line(node, self.source_file) {
            return Tristate::True;
        }
        Tristate::False
    }

    // context.go:69
    fn node_is_on_one_line(&self, node: P<Node>) -> Tristate {
        self.range_is_on_one_line(with_token_start(node, self.source_file))
    }

    // context.go:78
    fn block_is_on_one_line(&self, node: P<Node>) -> Tristate {
        let open_brace = astnav::find_child_of_kind(node, Kind::OpenBraceToken, self.source_file);
        let close_brace = astnav::find_child_of_kind(node, Kind::CloseBraceToken, self.source_file);
        if let (Some(open_brace), Some(close_brace)) = (open_brace, close_brace) {
            let close_brace_start =
                scanner::get_token_pos_of_node(close_brace, self.source_file, false);
            return self.range_is_on_one_line(TextRange::new(
                open_brace.end().expect_source("parsed tokens have source positions"),
                close_brace_start,
            ));
        }
        Tristate::False
    }

    // context.go:88
    pub(crate) fn context_node_all_on_same_line(&self) -> bool {
        if self.context_node_all_on_same_line.get() == Tristate::Unknown {
            self.context_node_all_on_same_line.set(self.node_is_on_one_line(self.context_node()));
        }
        self.context_node_all_on_same_line.get() == Tristate::True
    }

    // context.go:95
    pub(crate) fn next_node_all_on_same_line(&self) -> bool {
        if self.next_node_all_on_same_line.get() == Tristate::Unknown {
            self.next_node_all_on_same_line.set(self.node_is_on_one_line(self.next_token_parent()));
        }
        self.next_node_all_on_same_line.get() == Tristate::True
    }

    // context.go:102
    pub(crate) fn tokens_are_on_same_line(&self) -> bool {
        if self.tokens_are_on_same_line.get() == Tristate::Unknown {
            self.tokens_are_on_same_line.set(self.range_is_on_one_line(TextRange::from_text_pos(
                self.current_token_span.loc.pos(),
                self.next_token_span.loc.pos(),
            )));
        }
        self.tokens_are_on_same_line.get() == Tristate::True
    }

    // context.go:109
    pub(crate) fn context_node_block_is_on_one_line(&self) -> bool {
        if self.context_node_block_is_on_one_line.get() == Tristate::Unknown {
            self.context_node_block_is_on_one_line
                .set(self.block_is_on_one_line(self.context_node()));
        }
        self.context_node_block_is_on_one_line.get() == Tristate::True
    }

    // context.go:116
    pub(crate) fn next_node_block_is_on_one_line(&self) -> bool {
        if self.next_node_block_is_on_one_line.get() == Tristate::Unknown {
            self.next_node_block_is_on_one_line
                .set(self.block_is_on_one_line(self.next_token_parent()));
        }
        self.next_node_block_is_on_one_line.get() == Tristate::True
    }
}

// context.go:73
pub(crate) fn with_token_start(loc: P<Node>, file: P<SourceFile>) -> TextRange {
    let start_pos = scanner::get_token_pos_of_node(loc, file, false);
    TextRange::new(start_pos, loc.end().expect_source("parsed nodes have source positions"))
}
