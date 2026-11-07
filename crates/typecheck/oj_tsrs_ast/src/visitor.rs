use std::rc::Rc;

use tsrs_core::{P, alloc_slice, alloc_vec};

use crate::ast::{ModifierList, Node, NodeFactory, NodeList};
use crate::kind::Kind;

// NodeVisitor
//
// Go stores plain func values that close over the visitor itself. In Rust the callbacks are shared
// `Rc<dyn Fn>` values that receive the visitor back as an argument, so a callback may re-enter the
// visitor (e.g. `v.visit_each_child(node)` from inside `visit`). The visitor owns a factory handle;
// to run a visitor with an existing factory (Go passes the same `*NodeFactory`), pass `f.clone()`,
// which shares hooks and counters.

pub type VisitFn = Rc<dyn Fn(&mut NodeVisitor, P<Node>) -> Option<P<Node>>>;
pub type VisitNodeHook = Rc<dyn Fn(Option<P<Node>>, &mut NodeVisitor) -> Option<P<Node>>>;
pub type VisitNodesHook = Rc<dyn Fn(Option<P<NodeList>>, &mut NodeVisitor) -> Option<P<NodeList>>>;
pub type VisitModifiersHook =
    Rc<dyn Fn(Option<P<ModifierList>>, &mut NodeVisitor) -> Option<P<ModifierList>>>;

// A handle (one `Rc`): transformers hand out a fresh `&mut NodeVisitor` for every `visit_each_child` by cloning the
// one they keep, and the visitor itself is never modified after construction.
#[derive(Clone)]
pub struct NodeVisitor(Rc<NodeVisitorData>);

pub struct NodeVisitorData {
    pub visit: Option<VisitFn>,  // Required. The callback used to visit a node
    pub factory: NodeFactory, // Required. The NodeFactory used to produce new nodes when passed to VisitEachChild
    pub hooks: NodeVisitorHooks, // Hooks to be invoked when visiting a node
}

impl std::ops::Deref for NodeVisitor {
    type Target = NodeVisitorData;
    fn deref(&self) -> &NodeVisitorData {
        &self.0
    }
}

// These hooks are used to intercept the default behavior of the visitor
#[derive(Default, Clone)]
pub struct NodeVisitorHooks {
    pub visit_node: Option<VisitNodeHook>, // Overrides visiting a Node. Only invoked by the VisitEachChild method on a given Node subtype.
    pub visit_token: Option<VisitNodeHook>, // Overrides visiting a TokenNode. Only invoked by the VisitEachChild method on a given Node subtype.
    pub visit_nodes: Option<VisitNodesHook>, // Overrides visiting a NodeList. Only invoked by the VisitEachChild method on a given Node subtype.
    pub visit_modifiers: Option<VisitModifiersHook>, // Overrides visiting a ModifierList. Only invoked by the VisitEachChild method on a given Node subtype.
    pub visit_embedded_statement: Option<VisitNodeHook>, // Overrides visiting a Node when it is the embedded statement body of an iteration statement, `if` statement, or `with` statement.
    pub visit_iteration_body: Option<VisitNodeHook>, // Overrides visiting a Node when it is the embedded statement body of an iteration statement.
    pub visit_parameters: Option<VisitNodesHook>,    // Overrides visiting a ParameterList.
    pub visit_function_body: Option<VisitNodeHook>,  // Overrides visiting a function body.
    pub visit_top_level_statements: Option<VisitNodesHook>, // Overrides visiting a variable environment.
}

pub fn new_node_visitor(
    visit: Option<VisitFn>,
    factory: Option<NodeFactory>,
    hooks: NodeVisitorHooks,
) -> NodeVisitor {
    NodeVisitor(Rc::new(NodeVisitorData { visit, factory: factory.unwrap_or_default(), hooks }))
}

impl NodeVisitor {
    pub fn visit_source_file(&mut self, node: P<Node>) -> P<Node> {
        let visited = self.visit_node(Some(node)).unwrap();
        visited.as_source_file();
        visited
    }

    // Visits a Node, possibly returning a new Node in its place.
    //
    //   - If the input node is nil, then the output is nil.
    //   - If v.Visit is nil, then the output is the input.
    //   - If v.Visit returns nil, then the output is nil.
    //   - If v.Visit returns a SyntaxList Node, then the output is the only child of the SyntaxList Node.
    pub fn visit_node(&mut self, node: Option<P<Node>>) -> Option<P<Node>> {
        let node = node?;
        let Some(visit) = self.visit.clone() else {
            return Some(node);
        };
        let mut visited = visit(self, node);
        if let Some(v) = visited
            && v.kind() == Kind::SyntaxList
        {
            let nodes = v.as_syntax_list().children;
            assert!(nodes.len() == 1, "Expected only a single node to be written to output");
            visited = Some(nodes[0]);
            assert!(
                nodes[0].kind() != Kind::SyntaxList,
                "The result of visiting and lifting a Node may not be SyntaxList"
            );
        }
        visited
    }

    // Visits an embedded Statement (i.e., the single statement body of a loop, `if..else` branch, etc.), possibly returning a new Statement in its place.
    //
    //   - If the input node is nil, then the output is nil.
    //   - If v.Visit is nil, then the output is the input.
    //   - If v.Visit returns nil, then the output is nil.
    //   - If v.Visit returns a SyntaxList Node, then the output is either the only child of the SyntaxList Node, or a Block containing the nodes in the list.
    pub fn visit_embedded_statement(&mut self, node: Option<P<Node>>) -> Option<P<Node>> {
        let node = node?;
        let Some(visit) = self.visit.clone() else {
            return Some(node);
        };
        let visited = visit(self, node)?;
        Some(self.lift_to_block(Some(visited)))
    }

    // Visits a NodeList, possibly returning a new NodeList in its place.
    //
    //   - If the input NodeList is nil, the output is nil.
    //   - If v.Visit is nil, then the output is the input.
    //   - If v.Visit returns nil, the visited Node will be absent in the output.
    //   - If v.Visit returns a different Node than the input, a new NodeList will be generated and returned.
    //   - If v.Visit returns a SyntaxList Node, then the children of that node will be merged into the output and a new NodeList will be returned.
    //   - If this method returns a new NodeList for any reason, it will have the same Loc as the input NodeList.
    pub fn visit_nodes(&mut self, nodes: Option<P<NodeList>>) -> Option<P<NodeList>> {
        let nodes = nodes?;
        if self.visit.is_none() {
            return Some(nodes);
        }
        let (result, changed) = self.visit_slice(nodes.nodes());
        if changed {
            let list = self.factory.new_node_list_from_static(result);
            list.loc.set(nodes.loc.get());
            return Some(list);
        }
        Some(nodes)
    }

    // Visits a ModifierList, possibly returning a new ModifierList in its place.
    // Same contract as visit_nodes.
    pub fn visit_modifiers(&mut self, nodes: Option<P<ModifierList>>) -> Option<P<ModifierList>> {
        let nodes = nodes?;
        if self.visit.is_none() {
            return Some(nodes);
        }
        let (result, changed) = self.visit_slice(nodes.list.nodes());
        if changed {
            let list = self.factory.new_modifier_list(result.to_vec());
            list.list.loc.set(nodes.list.loc.get());
            return Some(list);
        }
        Some(nodes)
    }

    // Visits a slice of Nodes, returning the resulting slice and a value indicating whether the slice was changed.
    //
    //   - If v.Visit is nil, then the output is the input.
    //   - If v.Visit returns nil, the visited Node will be absent in the output.
    //   - If v.Visit returns a different Node than the input, a new slice will be generated and returned.
    //   - If v.Visit returns a SyntaxList Node, then the children of that node will be merged into the output and a new slice will be returned.
    pub fn visit_slice(&mut self, nodes: &'static [P<Node>]) -> (&'static [P<Node>], bool) {
        let Some(visit) = self.visit.clone() else {
            return (nodes, false);
        };
        let mut i = 0;
        while i < nodes.len() {
            let node = nodes[i];
            let mut visited = visit(self, node);
            if visited != Some(node) {
                let mut updated: Vec<P<Node>> = nodes[..i].to_vec();
                loop {
                    // finish prior loop
                    match visited {
                        None => {}
                        Some(v) if v.kind() == Kind::SyntaxList => {
                            updated.extend_from_slice(v.as_syntax_list().children);
                        }
                        Some(v) => updated.push(v),
                    }
                    i += 1;
                    // loop over remaining elements
                    if i >= nodes.len() {
                        break;
                    }
                    if let Some(visit) = self.visit.clone() {
                        visited = visit(self, nodes[i]);
                    } else {
                        updated.extend_from_slice(&nodes[i..]);
                        break;
                    }
                }
                return (alloc_vec(updated), true);
            }
            i += 1;
        }
        (nodes, false)
    }

    // Visits each child of a Node, possibly returning a new Node of the same kind in its place.
    pub fn visit_each_child(&mut self, node: Option<P<Node>>) -> Option<P<Node>> {
        let node = node?;
        if self.visit.is_none() {
            return Some(node);
        }
        Some(node.visit_each_child(self))
    }

    // Go's unexported helpers (visitNode, visitToken, ...), called from the generated visit_each_child.

    pub(crate) fn visit_node_hooked(&mut self, node: Option<P<Node>>) -> Option<P<Node>> {
        if let Some(hook) = self.hooks.visit_node.clone() {
            return hook(node, self);
        }
        self.visit_node(node)
    }

    pub(crate) fn visit_embedded_statement_hooked(
        &mut self,
        node: Option<P<Node>>,
    ) -> Option<P<Node>> {
        if let Some(hook) = self.hooks.visit_embedded_statement.clone() {
            return hook(node, self);
        }
        if let Some(hook) = self.hooks.visit_node.clone() {
            let visited = hook(node, self);
            return Some(self.lift_to_block(visited));
        }
        self.visit_embedded_statement(node)
    }

    pub(crate) fn visit_iteration_body_hooked(&mut self, node: Option<P<Node>>) -> Option<P<Node>> {
        if let Some(hook) = self.hooks.visit_iteration_body.clone() {
            return hook(node, self);
        }
        self.visit_embedded_statement_hooked(node)
    }

    pub(crate) fn visit_function_body_hooked(&mut self, node: Option<P<Node>>) -> Option<P<Node>> {
        if let Some(hook) = self.hooks.visit_function_body.clone() {
            return hook(node, self);
        }
        self.visit_node_hooked(node)
    }

    pub(crate) fn visit_token_hooked(&mut self, node: Option<P<Node>>) -> Option<P<Node>> {
        if let Some(hook) = self.hooks.visit_token.clone() {
            return hook(node, self);
        }
        self.visit_node(node)
    }

    pub(crate) fn visit_nodes_hooked(&mut self, nodes: Option<P<NodeList>>) -> Option<P<NodeList>> {
        if let Some(hook) = self.hooks.visit_nodes.clone() {
            return hook(nodes, self);
        }
        self.visit_nodes(nodes)
    }

    pub(crate) fn visit_modifiers_hooked(
        &mut self,
        nodes: Option<P<ModifierList>>,
    ) -> Option<P<ModifierList>> {
        if let Some(hook) = self.hooks.visit_modifiers.clone() {
            return hook(nodes, self);
        }
        self.visit_modifiers(nodes)
    }

    pub(crate) fn visit_parameters_hooked(
        &mut self,
        nodes: Option<P<NodeList>>,
    ) -> Option<P<NodeList>> {
        if let Some(hook) = self.hooks.visit_parameters.clone() {
            return hook(nodes, self);
        }
        self.visit_nodes_hooked(nodes)
    }

    pub(crate) fn visit_top_level_statements_hooked(
        &mut self,
        nodes: Option<P<NodeList>>,
    ) -> Option<P<NodeList>> {
        if let Some(hook) = self.hooks.visit_top_level_statements.clone() {
            return hook(nodes, self);
        }
        self.visit_nodes_hooked(nodes)
    }

    fn lift_to_block(&self, node: Option<P<Node>>) -> P<Node> {
        let nodes: Vec<P<Node>> = match node {
            Some(n) if n.kind() == Kind::SyntaxList => n.as_syntax_list().children.to_vec(),
            Some(n) => vec![n],
            None => Vec::new(),
        };
        let node = if nodes.len() == 1 {
            nodes[0]
        } else {
            let list = self.factory.new_node_list(nodes);
            self.factory.new_block(list, true /*multiLine*/)
        };
        assert!(
            node.kind() != Kind::SyntaxList,
            "The result of visiting and lifting a Node may not be SyntaxList"
        );
        node
    }
}

/// A child that the Go AST allows to be nil only by accident of construction (a "required" child) is
/// non-optional in the Rust AST.
///
/// Go's `VisitEachChild` stores whatever the visitor returns, so a visitor that returns nil for such a child
/// produces a node with a nil child. That happens only in traversals whose result is discarded (the
/// declaration transformer's side-effect visitors, e.g. `visitThisPropertyAssignments` returning nil outside
/// its `this` container), so the Rust visitor keeps the original child instead of failing.
pub fn required_child<T>(visited: Option<T>, original: T) -> T {
    visited.unwrap_or(original)
}

/// Go `core.SameMap` over a raw node slice: returns `nodes` itself when `f` maps every element to itself.
pub(crate) fn same_map_nodes(
    nodes: &'static [P<Node>],
    mut f: impl FnMut(P<Node>) -> P<Node>,
) -> &'static [P<Node>] {
    for (i, &node) in nodes.iter().enumerate() {
        let mapped = f(node);
        if mapped != node {
            let mut result: Vec<P<Node>> = Vec::with_capacity(nodes.len());
            result.extend_from_slice(&nodes[..i]);
            result.push(mapped);
            result.extend(nodes[i + 1..].iter().map(|&rest| f(rest)));
            return alloc_slice(&result);
        }
    }
    nodes
}
