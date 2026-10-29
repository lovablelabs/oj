use std::rc::Rc;

use tsrs_core::{P, undefined_text_range};

use crate::*;

// Ideally, this would get cached on the node factory so there's only ever one set of closures made per factory
fn get_deep_clone_visitor(f: NodeFactory, synthetic_location: bool) -> NodeVisitor {
    let visit: VisitFn = Rc::new(move |visitor: &mut NodeVisitor, node: P<Node>| {
        let visited = visitor.visit_each_child(Some(node)).unwrap();
        if visited != node {
            if synthetic_location {
                visited.set_loc(undefined_text_range());
            }
            return Some(visited);
        }
        let c = node.clone_node(&visitor.factory); // forcibly clone leaf nodes, which will then cascade new nodes/arrays upwards via `update` calls
        // In strada, `factory.cloneNode` was dynamic and did _not_ clone positions for any "special cases", meanwhile
        // Node.Clone in corsa reliably uses `Update` calls for all nodes and so copies locations by default.
        // Deep clones are done to copy a node across files, so here, we explicitly make the location range synthetic on all cloned nodes
        if synthetic_location {
            c.set_loc(undefined_text_range());
        }
        Some(c)
    });
    let visit_nodes: VisitNodesHook =
        Rc::new(move |nodes: Option<P<NodeList>>, v: &mut NodeVisitor| {
            let nodes = nodes?;
            let visited = v.visit_nodes(Some(nodes)).unwrap();
            let new_list = if visited == nodes { nodes.clone_list(&v.factory) } else { visited };
            if synthetic_location {
                new_list.loc.set(undefined_text_range());
                if nodes.has_trailing_comma() {
                    new_list.nodes()[new_list.nodes().len() - 1].mark_trailing_comma();
                }
            }
            Some(new_list)
        });
    let visit_modifiers: VisitModifiersHook =
        Rc::new(move |nodes: Option<P<ModifierList>>, v: &mut NodeVisitor| {
            let nodes = nodes?;
            let visited = v.visit_modifiers(Some(nodes)).unwrap();
            let new_list = if visited == nodes { nodes.clone_list(&v.factory) } else { visited };
            if synthetic_location {
                new_list.list.loc.set(undefined_text_range());
                if nodes.has_trailing_comma() {
                    let new_nodes = new_list.nodes();
                    new_nodes[new_nodes.len() - 1].mark_trailing_comma();
                }
            }
            Some(new_list)
        });
    new_node_visitor(
        Some(visit),
        Some(f),
        NodeVisitorHooks {
            visit_nodes: Some(visit_nodes),
            visit_modifiers: Some(visit_modifiers),
            ..Default::default()
        },
    )
}

// The visitor owns a handle to this factory (`NodeFactory::clone` shares hooks and counters).
impl NodeFactory {
    fn with_deep_clone_visitor<R>(
        &self,
        synthetic_location: bool,
        f: impl FnOnce(&mut NodeVisitor) -> R,
    ) -> R {
        let mut visitor = get_deep_clone_visitor(self.clone(), synthetic_location);
        f(&mut visitor)
    }

    pub fn deep_clone_node(&self, node: Option<P<Node>>) -> Option<P<Node>> {
        self.with_deep_clone_visitor(true /*syntheticLocation*/, |v| v.visit_node(node))
    }

    /// A reparse cloner (`deep_clone_reparse` and `deep_clone_reparse_modifiers`) whose visitor is built once.
    ///
    /// The four heap allocations of `get_deep_clone_visitor` (three hook closures and the visitor data) are paid
    /// per cloner, not per clone. The parser keeps one per file (notes/perf-reparse-cloner.md).
    pub fn reparse_cloner(&self) -> ReparseCloner {
        ReparseCloner(get_deep_clone_visitor(self.clone(), false /*syntheticLocation*/))
    }

    pub fn deep_clone_reparse(&self, node: Option<P<Node>>) -> Option<P<Node>> {
        self.reparse_cloner().clone_node(node)
    }
}

/// `NodeFactory::reparse_cloner`: one deep-clone visitor, reused for every reparse clone of a file.
pub struct ReparseCloner(NodeVisitor);

impl ReparseCloner {
    /// `deep_clone_reparse`: a copy of `node` with its source locations, parents set, flagged `Reparsed`.
    pub fn clone_node(&mut self, node: Option<P<Node>>) -> Option<P<Node>> {
        let node = self.0.visit_node(Some(node?)).unwrap();
        set_parent_in_children(node);
        node.set_flags(node.flags() | NodeFlags::Reparsed);
        Some(node)
    }

    /// `deep_clone_reparse_modifiers`.
    pub fn clone_modifiers(
        &mut self,
        modifiers: Option<P<ModifierList>>,
    ) -> Option<P<ModifierList>> {
        self.0.visit_modifiers(modifiers)
    }
}
