#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

use std::rc::Rc;

use tsrs_ast::{self as ast, Kind, Node, NodeFlags, NodeList, NodeVisitor, SourceFile};
use tsrs_core::P;
use tsrs_scanner as scanner;

use crate::astnav::{self, VisitNodeFn, VisitNodesFn};

// children.go:13
// Replaces last(node.getChildren(sourceFile))
pub(crate) fn get_last_child(node: P<Node>, source_file: P<SourceFile>) -> Option<P<Node>> {
    let last_child_node = get_last_visited_child(node, source_file);
    if ast::is_jsdoc_single_comment_node(node) && last_child_node.is_none() {
        return None;
    }
    let token_start_pos = match last_child_node {
        Some(c) => c.end().expect_source("parsed children have source positions"),
        None => node.pos().expect_source("parsed nodes have source positions"),
    };
    let mut last_token: Option<P<Node>> = None;
    let mut scanner = scanner::get_scanner_for_source_file(source_file, token_start_pos);
    let mut start_pos = token_start_pos;
    while start_pos < node.end().expect_source("parsed nodes have source positions") {
        let token_kind = scanner.token();
        let token_full_start = scanner.token_full_start();
        let token_end = scanner.token_end();
        last_token = Some(source_file.get_or_create_token(
            token_kind,
            token_full_start,
            token_end,
            node,
            scanner.token_flags(),
        ));
        start_pos = token_end;
        scanner.scan();
    }
    if last_token.is_some() { last_token } else { last_child_node }
}

// children.go:37
pub(crate) fn get_last_token(node: Option<P<Node>>, source_file: P<SourceFile>) -> Option<P<Node>> {
    let node = node?;

    if ast::is_token_kind(node.kind()) || ast::is_identifier(node) {
        return None;
    }

    assert_has_real_position(node);

    let last_child = get_last_child(node, source_file)?;

    if last_child.kind() < Kind::FIRST_NODE {
        Some(last_child)
    } else {
        get_last_token(Some(last_child), source_file)
    }
}

// children.go:62
// Gets the last visited child of the given node.
// NOTE: This doesn't include unvisited tokens; for this, use `getLastChild` or `getLastToken`.
pub(crate) fn get_last_visited_child(node: P<Node>, source_file: P<SourceFile>) -> Option<P<Node>> {
    let last_child: Rc<std::cell::Cell<Option<P<Node>>>> = Rc::new(std::cell::Cell::new(None));

    let visit_node: VisitNodeFn = {
        let last_child = Rc::clone(&last_child);
        Rc::new(move |n: Option<P<Node>>, _: &mut NodeVisitor| {
            if let Some(nd) = n
                && !nd.flags().intersects(NodeFlags::Reparsed)
            {
                last_child.set(Some(nd));
            }
            n
        })
    };
    let visit_node_list: VisitNodesFn = {
        let last_child = Rc::clone(&last_child);
        Rc::new(move |node_list: Option<P<NodeList>>, _: &mut NodeVisitor| {
            if let Some(list) = node_list
                && let Some(v) = list.nodes().iter().rev().into_iter().find_map(|&v| {
                    if v.flags().intersects(NodeFlags::Reparsed) { None } else { Some(v) }
                })
            {
                last_child.set(Some(v));
            }
            node_list
        })
    };

    astnav::visit_each_child_and_jsdoc(node, source_file, Some(visit_node), Some(visit_node_list));
    last_child.get()
}

// children.go:87
pub(crate) fn get_first_token(node: P<Node>, source_file: P<SourceFile>) -> Option<P<Node>> {
    if ast::is_identifier(node) || ast::is_token_kind(node.kind()) {
        return None;
    }
    assert_has_real_position(node);
    let mut first_child: Option<P<Node>> = None;
    node.for_each_child(&mut |n| {
        // Go tests `node.Flags` here (not `n.Flags`); kept as is.
        if node.flags().intersects(NodeFlags::Reparsed) {
            return false;
        }
        first_child = Some(n);
        true
    });

    let token_end_position = match first_child {
        Some(c) => c.pos().expect_source("parsed children have source positions"),
        None => node.end().expect_source("parsed nodes have source positions"),
    };
    let scanner = scanner::get_scanner_for_source_file(
        source_file,
        node.pos().expect_source("parsed nodes have source positions"),
    );
    let mut first_token: Option<P<Node>> = None;
    if node.pos().expect_source("parsed nodes have source positions") < token_end_position {
        let token_kind = scanner.token();
        let token_full_start = scanner.token_full_start();
        let token_end = scanner.token_end();
        first_token = Some(source_file.get_or_create_token(
            token_kind,
            token_full_start,
            token_end,
            node,
            scanner.token_flags(),
        ));
    }

    if first_token.is_some() {
        return first_token;
    }
    let first_child = first_child?;
    if first_child.kind() < Kind::FIRST_NODE {
        return Some(first_child);
    }
    get_first_token(first_child, source_file)
}

// children.go:128
fn assert_has_real_position(node: P<Node>) {
    assert!(
        !(ast::position_is_synthesized(node.pos()) || ast::position_is_synthesized(node.end())),
        "Node must have a real position for this operation."
    );
}
