use std::cell::RefCell;
use std::rc::Rc;

use tsrs_ast::{self as ast, Node, NodeList, NodeVisitorHooks};
use tsrs_core::P;

// Rust adapter for callbacks that cannot borrow the caller's traversal state: collect individual nodes and
// node lists in VisitEachChild order, then let the caller process them without changing the tree.
pub enum VisitedChild {
    Node(P<Node>),
    Nodes(P<NodeList>),
}

pub fn collect_visited_children(node: P<Node>) -> Vec<VisitedChild> {
    let items: Rc<RefCell<Vec<VisitedChild>>> = Rc::new(RefCell::new(Vec::new()));
    let visit_items = Rc::clone(&items);
    let nodes_items = Rc::clone(&items);
    let mut visitor = ast::new_node_visitor(
        Some(Rc::new(move |_: &mut ast::NodeVisitor, child: P<Node>| {
            visit_items.borrow_mut().push(VisitedChild::Node(child));
            Some(child)
        })),
        None,
        NodeVisitorHooks {
            visit_nodes: Some(Rc::new(
                move |nodes: Option<P<NodeList>>, _: &mut ast::NodeVisitor| {
                    if let Some(list) = nodes {
                        nodes_items.borrow_mut().push(VisitedChild::Nodes(list));
                    }
                    nodes
                },
            )),
            ..Default::default()
        },
    );
    node.visit_each_child(&mut visitor);
    items.take()
}
