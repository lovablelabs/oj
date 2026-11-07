use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::*;
use tsrs_core::collections::OrderedSet;
use tsrs_core::*;

use crate::factory::{NodeFactory, new_node_factory_for_context};
use crate::*;

// Stores side-table information used during transformation that can be read by the printer to customize emit
//
// NOTE: EmitContext is not guaranteed to be thread-safe.
//
// Handled as `P<EmitContext>` (shared by the node builder, the transformers and the printers they feed, like Go's
// `*EmitContext`), so all state is interior-mutable. The environment scopes are `Rc<RefCell<varScope>>` (Go
// `*varScope`, mutated through `Peek()`); borrows are never held across a visitor call.
pub struct EmitContext {
    pub factory: NodeFactory, // Required. The NodeFactory to use to create new nodes
    auto_generate: RefCell<FxHashMap<P<Node>, AutoGenerateInfo>>,
    text_source: RefCell<FxHashMap<P<Node>, P<Node>>>,
    original: RefCell<FxHashMap<P<Node>, P<Node>>>,
    emit_nodes: RefCell<FxHashMap<P<Node>, EmitNode>>,
    assigned_name: RefCell<FxHashMap<P<Node>, P<Node>>>,
    class_this: RefCell<FxHashMap<P<Node>, P<Node>>>,
    var_scope_stack: RefCell<Stack<Rc<RefCell<VarScope>>>>,
    let_scope_stack: RefCell<Stack<Rc<RefCell<VarScope>>>>,
    emit_helpers: RefCell<OrderedSet<SP<EmitHelper>>>,
}

bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    #[expect(non_camel_case_types, reason = "Keep the upstream bitflags type name.")]
    pub(crate) struct environmentFlags: u32 {
        const None = 0;
        const InParameters = 1 << 0; // currently visiting a parameter list
        const VariablesHoistedInParameters = 1 << 1; // a temp variable was hoisted while visiting a parameter list
    }
}

#[derive(Default)]
pub struct VarScope {
    variables: Vec<P<Node>>,
    functions: Vec<P<Node>>,
    flags: environmentFlags,
    initialization_statements: Vec<P<Node>>,
}

pub fn new_emit_context() -> P<EmitContext> {
    new_emit_context_in(false)
}

/// Places temporary emit nodes in the current scratch region.
///
/// tsrs-only: an emit context whose nodes die with the thread's scratch region (one file's emit,
/// notes/mem-emit-regions.md): the context and every node its factory creates (also through a node builder
/// that uses it) are allocated there (`P::new_scratch`).
///
/// Without an entered scratch region it is `new_emit_context`.
pub fn new_scratch_emit_context() -> P<EmitContext> {
    new_emit_context_in(true)
}

fn new_emit_context_in(scratch: bool) -> P<EmitContext> {
    let c = P::new_in(
        scratch,
        EmitContext {
            factory: new_node_factory_for_context(scratch),
            auto_generate: RefCell::default(),
            text_source: RefCell::default(),
            original: RefCell::default(),
            emit_nodes: RefCell::default(),
            assigned_name: RefCell::default(),
            class_this: RefCell::default(),
            var_scope_stack: RefCell::default(),
            let_scope_stack: RefCell::default(),
            emit_helpers: RefCell::default(),
        },
    );
    c.factory.set_emit_context(c);
    c
}

// Go pools emit contexts; the release function resets the context like Go's does before returning it to the pool.

impl EmitContext {
    // Go returns the context to a pool; tsrs allocates a new one per use (in the never-freed arena, or in the thread's
    // scratch region for the scratch variants, freed with the region), so `reset` also releases the tables' memory
    // instead of keeping their capacity.
    pub fn reset(&self) {
        *self.auto_generate.borrow_mut() = rustc_hash::FxHashMap::default();
        *self.text_source.borrow_mut() = rustc_hash::FxHashMap::default();
        *self.original.borrow_mut() = rustc_hash::FxHashMap::default();
        *self.emit_nodes.borrow_mut() = rustc_hash::FxHashMap::default();
        *self.assigned_name.borrow_mut() = rustc_hash::FxHashMap::default();
        *self.class_this.borrow_mut() = rustc_hash::FxHashMap::default();
        *self.var_scope_stack.borrow_mut() = Stack::default();
        *self.let_scope_stack.borrow_mut() = Stack::default();
        *self.emit_helpers.borrow_mut() = tsrs_core::collections::OrderedSet::default();
    }

    // emitcontext.go:90
    // Creates a new NodeVisitor attached to this EmitContext
    pub fn new_node_visitor(&self, visit: VisitFn) -> NodeVisitor {
        // SAFETY: an EmitContext is only created by `new_emit_context_in` (`new_emit_context` /
        // `new_scratch_emit_context`), which allocates it with `P::new_in` in the process-lifetime arena or the
        // thread's scratch region; the visitor (and the `P<EmitContext>` its hooks capture) must not outlive that
        // region.
        let c: P<EmitContext> =
            P::from_static(unsafe { &*std::ptr::from_ref::<EmitContext>(self) });
        tsrs_ast::new_node_visitor(
            Some(visit),
            Some(self.factory.as_node_factory().clone()),
            NodeVisitorHooks {
                visit_parameters: Some(Rc::new(move |nodes, visitor| {
                    c.visit_parameters(nodes, visitor)
                })),
                visit_function_body: Some(Rc::new(move |node, visitor| {
                    c.visit_function_body(node, visitor)
                })),
                visit_iteration_body: Some(Rc::new(move |body, visitor| {
                    c.visit_iteration_body(body, visitor)
                })),
                visit_top_level_statements: Some(Rc::new(move |nodes, visitor| {
                    c.visit_variable_environment(nodes, visitor)
                })),
                visit_embedded_statement: Some(Rc::new(move |node, visitor| {
                    c.visit_embedded_statement(node, visitor)
                })),
                ..Default::default()
            },
        )
    }

    //
    // Environment tracking
    //

    // Starts a new VariableEnvironment used to track hoisted `var` statements and function declarations.
    //
    // see: https://tc39.es/ecma262/#table-additional-state-components-for-ecmascript-code-execution-contexts
    //
    // NOTE: This is the equivalent of `transformContext.startLexicalEnvironment` in Strada.
    pub fn start_variable_environment(&self) {
        self.var_scope_stack.borrow_mut().push(Rc::default());
        self.start_lexical_environment();
    }

    // Ends the current VariableEnvironment, returning a list of statements that should be emitted at the start of the current scope.
    //
    // NOTE: This is the equivalent of `transformContext.endLexicalEnvironment` in Strada.
    pub fn end_variable_environment(&self) -> Vec<P<Node>> {
        let scope = self.var_scope_stack.borrow_mut().pop();
        let (functions, variables, initialization_statements) = {
            let scope = scope.borrow();
            (
                scope.functions.clone(),
                scope.variables.clone(),
                scope.initialization_statements.clone(),
            )
        };
        let mut statements: Vec<P<Node>> = Vec::new();
        if !functions.is_empty() {
            statements = functions;
        }
        if !variables.is_empty() {
            let var_decl_list = self.factory.new_variable_declaration_list(
                self.factory.new_node_list(variables),
                NodeFlags::None,
            );
            let var_statement =
                self.factory.new_variable_statement(None /*modifiers*/, var_decl_list);
            self.set_emit_flags(var_statement, EmitFlags::CustomPrologue);
            statements.push(var_statement);
        }
        if !initialization_statements.is_empty() {
            statements.extend(initialization_statements);
        }
        statements.extend(self.end_lexical_environment());
        statements
    }

    // Invokes c.EndVariableEnvironment() and merges the results into `statements`
    pub fn end_and_merge_variable_environment_list(
        &self,
        statements: Option<P<NodeList>>,
    ) -> Option<P<NodeList>> {
        let nodes: &[P<Node>] = match statements {
            Some(statements) => statements.nodes(),
            None => &[],
        };

        let (result, changed) = self.end_and_merge_variable_environment_worker(nodes);
        if changed {
            let list = self.factory.new_node_list(result);
            // Go reads `statements.Loc` here, which panics when `statements` is nil.
            list.loc.set(statements.unwrap().loc.get());
            return Some(list);
        }

        statements
    }

    // Invokes c.EndVariableEnvironment() and merges the results into `statements`
    pub fn end_and_merge_variable_environment(&self, statements: &[P<Node>]) -> Vec<P<Node>> {
        let (result, _) = self.end_and_merge_variable_environment_worker(statements);
        result
    }

    // Go's unexported `endAndMergeVariableEnvironment` (renamed: collides with the exported method after snake-casing).
    fn end_and_merge_variable_environment_worker(
        &self,
        statements: &[P<Node>],
    ) -> (Vec<P<Node>>, bool) {
        let declarations = self.end_variable_environment();
        self.merge_environment_worker(statements, &declarations)
    }

    // Adds a `var` declaration to the current VariableEnvironment
    //
    // NOTE: This is the equivalent of `transformContext.hoistVariableDeclaration` in Strada.
    pub fn add_variable_declaration(&self, name: P<Node>) {
        let var_decl = self.factory.new_variable_declaration(
            name, None, /*exclamationToken*/
            None, /*typeNode*/
            None, /*initializer*/
        );
        self.set_emit_flags(var_decl, EmitFlags::NoNestedSourceMaps);
        let scope = Rc::clone(self.var_scope_stack.borrow().peek());
        let mut scope = scope.borrow_mut();
        scope.variables.push(var_decl);
        if scope.flags.intersects(environmentFlags::InParameters) {
            scope.flags |= environmentFlags::VariablesHoistedInParameters;
        }
    }

    // Adds a hoisted function declaration to the current VariableEnvironment
    //
    // NOTE: This is the equivalent of `transformContext.hoistFunctionDeclaration` in Strada.

    // Starts a new LexicalEnvironment used to track block-scoped `let`, `const`, and `using` declarations.
    //
    // see: https://tc39.es/ecma262/#table-additional-state-components-for-ecmascript-code-execution-contexts
    //
    // NOTE: This is the equivalent of `transformContext.startBlockScope` in Strada.
    // NOTE: This is *not* the same as `startLexicalEnvironment` in Strada as that method is incorrectly named.
    fn start_lexical_environment(&self) {
        self.let_scope_stack.borrow_mut().push(Rc::default());
    }

    // Ends the current EndLexicalEnvironment, returning a list of statements that should be emitted at the start of the current scope.
    //
    // NOTE: This is the equivalent of `transformContext.endLexicalEnvironment` in Strada.
    // NOTE: This is *not* the same as `endLexicalEnvironment` in Strada as that method is incorrectly named.
    fn end_lexical_environment(&self) -> Vec<P<Node>> {
        let scope = self.let_scope_stack.borrow_mut().pop();
        let variables = scope.borrow().variables.clone();
        let mut statements: Vec<P<Node>> = Vec::new();
        if !variables.is_empty() {
            let var_decl_list = self.factory.new_variable_declaration_list(
                self.factory.new_node_list(variables),
                NodeFlags::Let,
            );
            let var_statement =
                self.factory.new_variable_statement(None /*modifiers*/, var_decl_list);
            self.set_emit_flags(var_statement, EmitFlags::CustomPrologue);
            statements.push(var_statement);
        }
        statements
    }

    // Adds a `let` declaration to the current LexicalEnvironment.
    pub fn add_lexical_declaration(&self, name: P<Node>) {
        let var_decl = self.factory.new_variable_declaration(
            name, None, /*exclamationToken*/
            None, /*typeNode*/
            None, /*initializer*/
        );
        self.set_emit_flags(var_decl, EmitFlags::NoNestedSourceMaps);
        let scope = Rc::clone(self.let_scope_stack.borrow().peek());
        scope.borrow_mut().variables.push(var_decl);
    }

    // Merges declarations produced by c.EndVariableEnvironment() or c.EndLexicalEnvironment() into a statement list
    pub fn merge_environment_list(
        &self,
        statements: P<NodeList>,
        declarations: &[P<Node>],
    ) -> P<NodeList> {
        let (result, changed) = self.merge_environment_worker(statements.nodes(), declarations);
        if changed {
            let list = self.factory.new_node_list(result);
            list.loc.set(statements.loc.get());
            return list;
        }
        statements
    }

    // Merges declarations produced by c.EndVariableEnvironment() or c.EndLexicalEnvironment() into a slice of statements
    pub fn merge_environment(
        &self,
        statements: &[P<Node>],
        declarations: &[P<Node>],
    ) -> Vec<P<Node>> {
        let (result, _) = self.merge_environment_worker(statements, declarations);
        result
    }

    // Go's unexported `mergeEnvironment` (renamed: collides with the exported method after snake-casing).
    fn merge_environment_worker(
        &self,
        statements: &[P<Node>],
        declarations: &[P<Node>],
    ) -> (Vec<P<Node>>, bool) {
        if declarations.is_empty() {
            return (statements.to_vec(), false);
        }

        // When we merge new lexical statements into an existing statement list, we merge them in the following manner:
        //
        // Given:
        //
        // | Left                               | Right                               |
        // |------------------------------------|-------------------------------------|
        // | [standard prologues (left)]        | [standard prologues (right)]        |
        // | [hoisted functions (left)]         | [hoisted functions (right)]         |
        // | [hoisted variables (left)]         | [hoisted variables (right)]         |
        // | [lexical init statements (left)]   | [lexical init statements (right)]   |
        // | [other statements (left)]          |                                     |
        //
        // The resulting statement list will be:
        //
        // | Result                              |
        // |-------------------------------------|
        // | [standard prologues (right)]        |
        // | [standard prologues (left)]         |
        // | [hoisted functions (right)]         |
        // | [hoisted functions (left)]          |
        // | [hoisted variables (right)]         |
        // | [hoisted variables (left)]          |
        // | [lexical init statements (right)]   |
        // | [lexical init statements (left)]    |
        // | [other statements (left)]           |
        //
        // NOTE: It is expected that new lexical init statements must be evaluated before existing lexical init statements,
        // as the prior transformation may depend on the evaluation of the lexical init statements to be in the correct state.

        let mut changed = false;

        // find standard prologues on left in the following order: standard directives, hoisted functions, hoisted variables, other custom
        let left_standard_prologue_end = find_span_end(statements, is_prologue_directive, 0);
        let left_hoisted_functions_end = find_span_end_with_emit_context(
            self,
            statements,
            EmitContext::is_hoisted_function,
            left_standard_prologue_end,
        );
        let left_hoisted_variables_end = find_span_end_with_emit_context(
            self,
            statements,
            EmitContext::is_hoisted_variable_statement,
            left_hoisted_functions_end,
        );

        // find standard prologues on right in the following order: standard directives, hoisted functions, hoisted variables, other custom
        let right_standard_prologue_end = find_span_end(declarations, is_prologue_directive, 0);
        let right_hoisted_functions_end = find_span_end_with_emit_context(
            self,
            declarations,
            EmitContext::is_hoisted_function,
            right_standard_prologue_end,
        );
        let right_hoisted_variables_end = find_span_end_with_emit_context(
            self,
            declarations,
            EmitContext::is_hoisted_variable_statement,
            right_hoisted_functions_end,
        );
        let right_custom_prologue_end = find_span_end_with_emit_context(
            self,
            declarations,
            EmitContext::is_custom_prologue,
            right_hoisted_variables_end,
        );
        assert!(
            right_custom_prologue_end == declarations.len(),
            "Expected declarations to be valid standard or custom prologues"
        );

        let mut left: Vec<P<Node>> = statements.to_vec();

        // splice other custom prologues from right into left
        if right_custom_prologue_end > right_hoisted_variables_end {
            left = splice(
                &left,
                SpliceStart::FromStart(left_hoisted_variables_end),
                0,
                &declarations[right_hoisted_variables_end..right_custom_prologue_end],
            )
            .into_owned();
            changed = true;
        }

        // splice hoisted variables from right into left
        if right_hoisted_variables_end > right_hoisted_functions_end {
            left = splice(
                &left,
                SpliceStart::FromStart(left_hoisted_functions_end),
                0,
                &declarations[right_hoisted_functions_end..right_hoisted_variables_end],
            )
            .into_owned();
            changed = true;
        }

        // splice hoisted functions from right into left
        if right_hoisted_functions_end > right_standard_prologue_end {
            left = splice(
                &left,
                SpliceStart::FromStart(left_standard_prologue_end),
                0,
                &declarations[right_standard_prologue_end..right_hoisted_functions_end],
            )
            .into_owned();
            changed = true;
        }

        // splice standard prologues from right into left (that are not already in left)
        if right_standard_prologue_end > 0 {
            if left_standard_prologue_end == 0 {
                left = splice(
                    &left,
                    SpliceStart::FromStart(0),
                    0,
                    &declarations[..right_standard_prologue_end],
                )
                .into_owned();
                changed = true;
            } else {
                let mut left_prologues: FxHashSet<&'static str> = FxHashSet::default();
                left_prologues.extend(
                    statements[..left_standard_prologue_end]
                        .iter()
                        .map(|&left_prologue| left_prologue.expression().unwrap().text()),
                );
                for &right_prologue in declarations[..right_standard_prologue_end].iter().rev() {
                    if !left_prologues.contains(right_prologue.expression().unwrap().text()) {
                        left = concatenate(&[right_prologue], &left).into_owned();
                        changed = true;
                    }
                }
            }
        }

        (left, changed)
    }

    fn is_custom_prologue(&self, node: P<Node>) -> bool {
        self.emit_flags(node).intersects(EmitFlags::CustomPrologue)
    }

    fn is_hoisted_function(&self, node: P<Node>) -> bool {
        self.is_custom_prologue(node) && is_function_declaration(node)
    }

    fn is_hoisted_variable_statement(&self, node: P<Node>) -> bool {
        self.is_custom_prologue(node)
            && is_variable_statement(node)
            && every(
                node.as_variable_statement()
                    .declaration_list
                    .as_variable_declaration_list()
                    .declarations
                    .nodes(),
                |&n| is_hoisted_variable(n),
            )
    }

    #[expect(
        clippy::unused_self,
        reason = "This method implements the NodeFactory creation hook, whose callback receives the emit context."
    )]
    pub(crate) fn on_create(&self, node: P<Node>) {
        node.flags.set(node.flags.get() | NodeFlags::Synthesized);
    }

    pub(crate) fn on_update(&self, updated: P<Node>, original: P<Node>) {
        self.set_original(updated, original);
    }

    pub(crate) fn on_clone(&self, updated: P<Node>, original: P<Node>) {
        self.set_original(updated, original);
        if is_identifier(updated) || is_private_identifier(updated) {
            let auto_generate = self.auto_generate.borrow().get(&original).copied();
            if let Some(auto_generate) = auto_generate {
                let auto_generate_copy = auto_generate;
                self.auto_generate.borrow_mut().insert(updated, auto_generate_copy);
            }
        }
    }

    //
    // Name Generation
    //

    // Gets whether a given name has an associated AutoGenerateInfo entry.
    pub fn has_auto_generate_info(&self, node: Option<P<Node>>) -> bool {
        if let Some(node) = node {
            return self.auto_generate.borrow().contains_key(&node);
        }
        false
    }

    // Gets the associated AutoGenerateInfo entry for a given name.
    pub fn get_auto_generate_info(&self, name: Option<P<Node>>) -> Option<AutoGenerateInfo> {
        let name = name?;
        self.auto_generate.borrow().get(&name).copied()
    }

    pub(crate) fn set_auto_generate_info(&self, name: P<Node>, info: AutoGenerateInfo) {
        self.auto_generate.borrow_mut().insert(name, info);
    }

    // Walks the associated AutoGenerateInfo entries of a name to find the root Nopde from which the name should be generated.
    pub fn get_node_for_generated_name(&self, name: P<Node>) -> P<Node> {
        if let Some(auto_generate) = self.get_auto_generate_info(Some(name))
            && auto_generate.flags.is_node()
        {
            return self
                .get_node_for_generated_name_worker(auto_generate.node.unwrap(), auto_generate.id);
        }
        name
    }

    pub(crate) fn get_node_for_generated_name_worker(
        &self,
        node: P<Node>,
        auto_generate_id: AutoGenerateId,
    ) -> P<Node> {
        let mut node = node;
        let mut original = self.original(node);
        while let Some(o) = original {
            node = o;
            if is_member_name(node) {
                // if "node" is a different generated name (having a different "autoGenerateId"), use it and stop traversing.
                let auto_generate = self.get_auto_generate_info(Some(node));
                match auto_generate {
                    None => break,
                    Some(auto_generate) => {
                        if auto_generate.flags.is_node() && auto_generate.id != auto_generate_id {
                            break;
                        }
                        if auto_generate.flags.is_node() {
                            original = auto_generate.node;
                            continue;
                        }
                    }
                }
            }
            original = self.original(node);
        }
        node
    }

    //
    // Original Node Tracking
    //

    // Sets the original node for a given node.
    //
    // NOTE: This is the equivalent to `setOriginalNode` in Strada.
    pub fn set_original(&self, node: P<Node>, original: P<Node>) {
        self.set_original_ex(node, original, false);
    }

    pub fn unset_original(&self, node: P<Node>) {
        self.original.borrow_mut().remove(&node);
    }

    pub fn set_original_ex(&self, node: P<Node>, original: P<Node>, allow_overwrite: bool) {
        let existing = self.original.borrow().get(&node).copied();
        match existing {
            None => {
                self.original.borrow_mut().insert(node, original);
                let source = self.emit_nodes.borrow().get(&original).cloned();
                if let Some(emit_node) = source {
                    self.emit_nodes.borrow_mut().entry(node).or_default().copy_from(&emit_node);
                }
            }
            Some(existing) => {
                if !allow_overwrite && existing != original {
                    panic!("Original node already set.");
                } else if allow_overwrite {
                    self.original.borrow_mut().insert(node, original);
                }
            }
        }
    }

    // Gets the original node for a given node.
    //
    // NOTE: This is the equivalent to reading `node.original` in Strada.
    pub fn original(&self, node: P<Node>) -> Option<P<Node>> {
        self.original.borrow().get(&node).copied()
    }

    // Gets the most original node associated with this node by walking Original pointers.
    //
    // NOTE: This method is analogous to `getOriginalNode` in the old compiler, but the name has changed to avoid accidental
    // conflation with `SetOriginal`/`Original`
    pub fn most_original(&self, node: Option<P<Node>>) -> Option<P<Node>> {
        let mut node = node;
        if let Some(n) = node {
            let mut original = self.original(n);
            while let Some(o) = original {
                node = Some(o);
                original = self.original(o);
            }
        }
        node
    }

    // Gets the original parse tree node for a given node.
    //
    // NOTE: This is the equivalent to `getParseTreeNode` in Strada.
    pub fn parse_node(&self, node: Option<P<Node>>) -> Option<P<Node>> {
        let node = self.most_original(node)?;
        if is_parse_tree_node(node) {
            return Some(node);
        }
        None
    }

    pub fn is_file_level_unique_name(
        &self,
        source_file: P<SourceFile>,
        name: &str,
        has_global_name: Option<&dyn Fn(&str) -> bool>,
    ) -> bool {
        if let Some(has_global_name) = has_global_name
            && has_global_name(name)
        {
            return false;
        }
        let source_file = self.most_original(Some(source_file.as_node())).unwrap().as_source_file();
        !source_file.has_identifier(name)
    }

    //
    // Emit-related Data
    //

    pub fn emit_flags(&self, node: P<Node>) -> EmitFlags {
        if let Some(emit_node) = self.emit_nodes.borrow().get(&node) {
            return emit_node.emit_flags;
        }
        EmitFlags::None
    }

    pub fn set_emit_flags(&self, node: P<Node>, flags: EmitFlags) {
        self.emit_nodes.borrow_mut().entry(node).or_default().emit_flags = flags;
    }

    pub fn add_emit_flags(&self, node: P<Node>, flags: EmitFlags) {
        self.emit_nodes.borrow_mut().entry(node).or_default().emit_flags |= flags;
    }

    pub(crate) fn snippet_element(&self, node: P<Node>) -> Option<SnippetElement> {
        if let Some(emit_node) = self.emit_nodes.borrow().get(&node) {
            return emit_node.rare()?.snippet_element;
        }
        None
    }

    pub fn set_snippet_element(&self, node: P<Node>, snippet_element: SnippetElement) {
        self.emit_nodes.borrow_mut().entry(node).or_default().rare_mut().snippet_element =
            Some(snippet_element);
    }

    // Gets the range to use for a node when emitting comments.
    pub fn comment_range(&self, node: P<Node>) -> TextRange {
        if let Some(emit_node) = self.emit_nodes.borrow().get(&node)
            && emit_node.flags.intersects(emitNodeFlags::hasCommentRange)
        {
            return emit_node.comment_range;
        }
        node.loc()
    }

    // Sets the range to use for a node when emitting comments.
    pub fn set_comment_range(&self, node: P<Node>, loc: TextRange) {
        let mut emit_nodes = self.emit_nodes.borrow_mut();
        let emit_node = emit_nodes.entry(node).or_default();
        emit_node.comment_range = loc;
        emit_node.flags |= emitNodeFlags::hasCommentRange;
    }

    // Sets the range to use for a node when emitting comments.
    pub fn assign_comment_range(&self, to: P<Node>, from: P<Node>) {
        self.set_comment_range(to, self.comment_range(from));
    }

    // Gets the range to use for a node when emitting source maps.
    pub fn source_map_range(&self, node: P<Node>) -> TextRange {
        if let Some(emit_node) = self.emit_nodes.borrow().get(&node)
            && emit_node.flags.intersects(emitNodeFlags::hasSourceMapRange)
        {
            return emit_node.source_map_range;
        }
        node.loc()
    }

    // Sets the range to use for a node when emitting source maps.
    pub fn set_source_map_range(&self, node: P<Node>, loc: TextRange) {
        let mut emit_nodes = self.emit_nodes.borrow_mut();
        let emit_node = emit_nodes.entry(node).or_default();
        emit_node.source_map_range = loc;
        emit_node.flags |= emitNodeFlags::hasSourceMapRange;
    }

    // Sets the range to use for a node when emitting source maps.
    pub fn assign_source_map_range(&self, to: P<Node>, from: P<Node>) {
        self.set_source_map_range(to, self.source_map_range(from));
    }

    // Sets the range to use for a node when emitting comments and source maps.
    pub fn assign_comment_and_source_map_ranges(&self, to: P<Node>, from: P<Node>) {
        let comment_range = self.comment_range(from);
        let source_map_range = self.source_map_range(from);
        let mut emit_nodes = self.emit_nodes.borrow_mut();
        let emit_node = emit_nodes.entry(to).or_default();
        emit_node.comment_range = comment_range;
        emit_node.source_map_range = source_map_range;
        emit_node.flags |= emitNodeFlags::hasCommentRange | emitNodeFlags::hasSourceMapRange;
    }

    // Gets the range for a token of a node when emitting source maps.
    pub(crate) fn token_source_map_range(&self, node: P<Node>, kind: Kind) -> Option<TextRange> {
        if let Some(emit_node) = self.emit_nodes.borrow().get(&node)
            && let Some(ranges) = emit_node.rare().and_then(|r| r.token_source_map_ranges.as_ref())
            && let Some(loc) = ranges.get(&kind)
        {
            return Some(*loc);
        }
        None
    }

    // Sets the range for a token of a node when emitting source maps.

    pub fn assigned_name(&self, node: P<Node>) -> Option<P<Node>> {
        self.assigned_name.borrow().get(&node).copied()
    }

    pub fn text_source(&self, node: P<Node>) -> Option<P<Node>> {
        self.text_source.borrow().get(&node).copied()
    }

    pub(crate) fn set_text_source(&self, node: P<Node>, text_source_node: P<Node>) {
        self.text_source.borrow_mut().insert(node, text_source_node);
    }

    pub fn set_assigned_name(&self, node: P<Node>, name: P<Node>) {
        self.assigned_name.borrow_mut().insert(node, name);
    }

    pub fn class_this(&self, node: P<Node>) -> Option<P<Node>> {
        self.class_this.borrow().get(&node).copied()
    }

    pub fn set_class_this(&self, node: P<Node>, class_this: P<Node>) {
        self.class_this.borrow_mut().insert(node, class_this);
    }

    pub(crate) fn request_emit_helper(&self, helper: SP<EmitHelper>) {
        assert!(!helper.scoped, "Cannot request a scoped emit helper");
        for h in helper.dependencies {
            self.request_emit_helper(*h);
        }
        self.emit_helpers.borrow_mut().insert(helper);
    }

    pub fn read_emit_helpers(&self) -> Vec<SP<EmitHelper>> {
        let helpers: Vec<SP<EmitHelper>> = self.emit_helpers.borrow().iter().copied().collect();
        self.emit_helpers.borrow_mut().clear();
        helpers
    }

    pub fn add_emit_helper(&self, node: P<Node>, helper: &[SP<EmitHelper>]) {
        let mut emit_nodes = self.emit_nodes.borrow_mut();
        let emit_node = emit_nodes.entry(node).or_default();
        for h in helper {
            if !emit_node.rare().is_some_and(|r| r.helpers.contains(h)) {
                emit_node.rare_mut().helpers.push(*h);
            }
        }
    }

    pub fn get_emit_helpers(&self, node: P<Node>) -> Vec<SP<EmitHelper>> {
        if let Some(emit_node) = self.emit_nodes.borrow().get(&node) {
            return emit_node.rare().map_or_else(Vec::new, |r| r.helpers.clone());
        }
        Vec::new()
    }

    pub fn get_external_helpers_module_name(&self, node: P<SourceFile>) -> Option<P<Node>> {
        if let Some(parse_node) = self.parse_node(Some(node.as_node()))
            && let Some(emit_node) = self.emit_nodes.borrow().get(&parse_node)
        {
            return emit_node.rare()?.external_helpers_module_name;
        }
        None
    }

    pub fn set_external_helpers_module_name(&self, node: P<SourceFile>, name: P<Node>) {
        let parse_node = self.parse_node(Some(node.as_node()));
        let Some(parse_node) = parse_node else {
            panic!(
                "Node must be a parse tree node or have an Original pointer to a parse tree node."
            );
        };

        self.emit_nodes
            .borrow_mut()
            .entry(parse_node)
            .or_default()
            .rare_mut()
            .external_helpers_module_name = Some(name);
    }

    pub(crate) fn has_recorded_external_helpers(&self, node: P<SourceFile>) -> bool {
        if let Some(parse_node) = self.parse_node(Some(node.as_node())) {
            let emit_nodes = self.emit_nodes.borrow();
            let emit_node = emit_nodes.get(&parse_node);
            return emit_node.is_some_and(|emit_node| {
                emit_node.rare().is_some_and(|r| r.external_helpers_module_name.is_some())
                    || emit_node.emit_flags.intersects(EmitFlags::ExternalHelpers)
            });
        }
        false
    }

    pub fn is_call_to_helper(&self, first_segment: P<Node>, helper_name: &str) -> bool {
        is_call_expression(first_segment)
            && is_identifier(first_segment.expression().unwrap())
            && self
                .emit_flags(first_segment.expression().unwrap())
                .intersects(EmitFlags::HelperName)
            && first_segment.expression().unwrap().text() == helper_name
    }

    //
    // Visitor Hooks
    //

    fn visit_variable_environment(
        &self,
        nodes: Option<P<NodeList>>,
        visitor: &mut NodeVisitor,
    ) -> Option<P<NodeList>> {
        self.start_variable_environment();
        let visited = visitor.visit_nodes(nodes);
        self.end_and_merge_variable_environment_list(visited)
    }

    pub fn visit_parameters(
        &self,
        nodes: Option<P<NodeList>>,
        visitor: &mut NodeVisitor,
    ) -> Option<P<NodeList>> {
        self.start_variable_environment();
        let scope = Rc::clone(self.var_scope_stack.borrow().peek());
        let old_flags = scope.borrow().flags;
        scope.borrow_mut().flags |= environmentFlags::InParameters;
        let mut nodes = visitor.visit_nodes(nodes);

        // As of ES2015, any runtime execution of that occurs in for a parameter (such as evaluating an
        // initializer or a binding pattern), occurs in its own lexical scope. As a result, any expression
        // that we might transform that introduces a temporary variable would fail as the temporary variable
        // exists in a different lexical scope. To address this, we move any binding patterns and initializers
        // in a parameter list to the body if we detect a variable being hoisted while visiting a parameter list
        // when the emit target is greater than ES2015. (Which is now all targets.)
        let flags = scope.borrow().flags;
        if flags.intersects(environmentFlags::VariablesHoistedInParameters) {
            nodes = self.add_default_value_assignments_if_needed(nodes);
        }
        scope.borrow_mut().flags = old_flags;
        // !!! c.suspendVariableEnvironment()
        nodes
    }

    fn add_default_value_assignments_if_needed(
        &self,
        node_list: Option<P<NodeList>>,
    ) -> Option<P<NodeList>> {
        let Some(node_list) = node_list else {
            return node_list;
        };
        let mut result: Option<Vec<P<Node>>> = None;
        let nodes = node_list.nodes();
        for (i, &parameter) in nodes.iter().enumerate() {
            let updated = self.add_default_value_assignment_if_needed(parameter);
            if updated != parameter {
                result.get_or_insert_with(|| nodes.to_vec())[i] = updated;
            }
        }
        if let Some(result) = result {
            let res = self.factory.new_node_list(result);
            res.loc.set(node_list.loc.get());
            return Some(res);
        }
        Some(node_list)
    }

    fn add_default_value_assignment_if_needed(&self, parameter: P<Node>) -> P<Node> {
        let p = parameter.as_parameter_declaration();
        // A rest parameter cannot have a binding pattern or an initializer,
        // so let's just ignore it.
        if p.dot_dot_dot_token().is_some() {
            parameter
        } else if is_binding_pattern(p.name()) {
            self.add_default_value_assignment_for_binding_pattern(parameter)
        } else if let Some(initializer) = p.initializer() {
            self.add_default_value_assignment_for_initializer(parameter, p.name(), initializer)
        } else {
            parameter
        }
    }

    fn add_default_value_assignment_for_binding_pattern(&self, parameter: P<Node>) -> P<Node> {
        let p = parameter.as_parameter_declaration();
        let init_node = if let Some(initializer) = p.initializer() {
            self.factory.new_conditional_expression(
                self.factory.new_strict_equality_expression(
                    self.factory.new_generated_name_for_node(parameter),
                    self.factory.new_void_zero_expression(),
                ),
                self.factory.new_token(Kind::QuestionToken),
                initializer,
                self.factory.new_token(Kind::ColonToken),
                self.factory.new_generated_name_for_node(parameter),
            )
        } else {
            self.factory.new_generated_name_for_node(parameter)
        };
        self.add_initialization_statement(self.factory.new_variable_statement(
            None,
            self.factory.new_variable_declaration_list(
                self.factory.new_node_list(vec![self.factory.new_variable_declaration(
                    p.name(),
                    None,
                    p.type_(),
                    Some(init_node),
                )]),
                NodeFlags::None,
            ),
        ));
        self.factory.update_parameter_declaration(
            parameter,
            parameter.modifiers(),
            p.dot_dot_dot_token(),
            self.factory.new_generated_name_for_node(parameter),
            p.question_token(),
            p.type_(),
            None,
        )
    }

    fn add_default_value_assignment_for_initializer(
        &self,
        parameter: P<Node>,
        name: P<Node>,
        initializer: P<Node>,
    ) -> P<Node> {
        let p = parameter.as_parameter_declaration();
        self.add_emit_flags(initializer, EmitFlags::NoSourceMap | EmitFlags::NoComments);
        let name_clone = name.clone_node(&self.factory);
        self.add_emit_flags(name_clone, EmitFlags::NoSourceMap);
        let init_assignment = self.factory.new_assignment_expression(name_clone, initializer);
        init_assignment.set_loc(parameter.loc());
        self.add_emit_flags(init_assignment, EmitFlags::NoComments);
        let init_block = self.factory.new_block(
            self.factory
                .new_node_list(vec![self.factory.new_expression_statement(init_assignment)]),
            false,
        );
        init_block.set_loc(parameter.loc());
        self.add_emit_flags(
            init_block,
            EmitFlags::SingleLine
                | EmitFlags::NoTrailingSourceMap
                | EmitFlags::NoTokenSourceMaps
                | EmitFlags::NoComments,
        );
        self.add_initialization_statement(self.factory.new_if_statement(
            self.factory.new_type_check(name.clone_node(&self.factory), "undefined"),
            init_block,
            None,
        ));
        self.factory.update_parameter_declaration(
            parameter,
            parameter.modifiers(),
            p.dot_dot_dot_token(),
            p.name(),
            p.question_token(),
            p.type_(),
            None,
        )
    }

    pub fn add_initialization_statement(&self, node: P<Node>) {
        // Go's `Peek()` panics on an empty stack before the nil check below can fire.
        let scope = Rc::clone(self.var_scope_stack.borrow().peek());
        self.add_emit_flags(node, EmitFlags::CustomPrologue);
        scope.borrow_mut().initialization_statements.push(node);
    }

    pub fn convert_to_function_block(&self, node: P<Node>, multi_line: bool) -> P<Node> {
        if is_block(node) {
            return node;
        }
        let return_statement = self.factory.new_return_statement(Some(node));
        return_statement.set_loc(node.loc());
        let statements = self.factory.new_node_list(vec![return_statement]);
        statements.loc.set(node.loc());
        let block = self.factory.new_block(statements, multi_line);
        block.set_loc(node.loc());
        block
    }

    pub fn visit_function_body(
        &self,
        node: Option<P<Node>>,
        visitor: &mut NodeVisitor,
    ) -> Option<P<Node>> {
        // !!! c.resumeVariableEnvironment()
        let updated = visitor.visit_node(node);
        let declarations = self.end_variable_environment();
        if declarations.is_empty() {
            return updated;
        }

        let Some(updated) = updated else {
            return Some(
                self.factory
                    .new_block(self.factory.new_node_list(declarations), true /*multiLine*/),
            );
        };

        if !is_block(updated) {
            self.add_emit_flags(updated, EmitFlags::NoComments);
            let block = self.convert_to_function_block(updated, false /*multiLine*/);
            return Some(self.factory.update_block(
                block,
                self.merge_environment_list(block.statement_list().unwrap(), &declarations),
                block.as_block().multi_line,
            ));
        }

        Some(self.factory.update_block(
            updated,
            self.merge_environment_list(updated.statement_list().unwrap(), &declarations),
            updated.as_block().multi_line,
        ))
    }

    pub fn visit_iteration_body(
        &self,
        body: Option<P<Node>>,
        visitor: &mut NodeVisitor,
    ) -> Option<P<Node>> {
        body?;

        self.start_lexical_environment();
        let updated = self.visit_embedded_statement(body, visitor);
        let Some(updated) = updated else {
            panic!("Expected visitor to return a statement.");
        };

        let mut statements = self.end_lexical_environment();
        if !statements.is_empty() {
            if is_block(updated) {
                statements.extend_from_slice(updated.statements());
                let statements_list = self.factory.new_node_list(statements);
                statements_list.loc.set(updated.statement_list().unwrap().loc.get());
                return Some(self.factory.update_block(
                    updated,
                    statements_list,
                    updated.as_block().multi_line,
                ));
            }
            statements.push(updated);
            return Some(
                self.factory
                    .new_block(self.factory.new_node_list(statements), true /*multiLine*/),
            );
        }

        Some(updated)
    }

    fn visit_embedded_statement(
        &self,
        node: Option<P<Node>>,
        visitor: &mut NodeVisitor,
    ) -> Option<P<Node>> {
        let node = node?;
        let embedded_statement = visitor.visit_embedded_statement(Some(node));
        match embedded_statement {
            Some(embedded_statement) if !is_not_emitted_statement(embedded_statement) => {
                Some(embedded_statement)
            }
            _ => {
                let empty_statement = visitor.factory.new_empty_statement();
                empty_statement.set_loc(node.loc());
                self.set_original(empty_statement, node);
                self.assign_comment_range(empty_statement, node);
                Some(empty_statement)
            }
        }
    }

    pub fn set_synthetic_leading_comments(
        &self,
        node: P<Node>,
        comments: Vec<SynthesizedComment>,
    ) -> P<Node> {
        self.emit_nodes.borrow_mut().entry(node).or_default().rare_mut().leading_comments =
            comments;
        node
    }

    pub fn add_synthetic_leading_comment(
        &self,
        node: P<Node>,
        kind: Kind,
        text: &str,
        has_trailing_new_line: bool,
    ) -> P<Node> {
        self.emit_nodes.borrow_mut().entry(node).or_default().rare_mut().leading_comments.push(
            SynthesizedComment {
                kind,
                loc: TextRange::synthetic(),
                has_leading_new_line: false,
                has_trailing_new_line,
                text: text.to_string(),
            },
        );
        node
    }

    pub(crate) fn get_synthetic_leading_comments(&self, node: P<Node>) -> Vec<SynthesizedComment> {
        if let Some(emit_node) = self.emit_nodes.borrow().get(&node) {
            return emit_node.rare().map_or_else(Vec::new, |r| r.leading_comments.clone());
        }
        Vec::new()
    }

    pub fn set_synthetic_trailing_comments(
        &self,
        node: P<Node>,
        comments: Vec<SynthesizedComment>,
    ) -> P<Node> {
        self.emit_nodes.borrow_mut().entry(node).or_default().rare_mut().trailing_comments =
            comments;
        node
    }

    pub fn add_synthetic_trailing_comment(
        &self,
        node: P<Node>,
        kind: Kind,
        text: &str,
        has_trailing_new_line: bool,
    ) -> P<Node> {
        self.emit_nodes.borrow_mut().entry(node).or_default().rare_mut().trailing_comments.push(
            SynthesizedComment {
                kind,
                loc: TextRange::synthetic(),
                has_leading_new_line: false,
                has_trailing_new_line,
                text: text.to_string(),
            },
        );
        node
    }

    pub(crate) fn get_synthetic_trailing_comments(&self, node: P<Node>) -> Vec<SynthesizedComment> {
        if let Some(emit_node) = self.emit_nodes.borrow().get(&node) {
            return emit_node.rare().map_or_else(Vec::new, |r| r.trailing_comments.clone());
        }
        Vec::new()
    }

    // SetTypeNode stores the original type node on a name node when the type is erased,
    // so the emitter can use the type's position for comment preservation.
    pub fn set_type_node(&self, node: P<Node>, type_node: P<Node>) {
        self.emit_nodes.borrow_mut().entry(node).or_default().rare_mut().type_node =
            Some(type_node);
    }

    // GetTypeNode gets the type node stored on a name node by the type eraser.
    pub(crate) fn get_type_node(&self, node: P<Node>) -> Option<P<Node>> {
        if let Some(emit_node) = self.emit_nodes.borrow().get(&node) {
            return emit_node.rare()?.type_node;
        }
        None
    }

    pub fn new_not_emitted_statement(&self, node: P<Node>) -> P<Node> {
        let statement = self.factory.new_not_emitted_statement();
        statement.set_loc(node.loc());
        self.set_original(statement, node);
        self.assign_comment_range(statement, node);
        statement
    }
}

// emitcontext.go:362 (package-level; used by `EmitContext::is_hoisted_variable_statement`)
fn is_hoisted_variable(node: P<Node>) -> bool {
    is_identifier(node.name().unwrap()) && node.initializer().is_none()
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AutoGenerateOptions {
    pub flags: GeneratedIdentifierFlags,
    pub prefix: &'static str,
    pub suffix: &'static str,
}

static NEXT_AUTO_GENERATE_ID: AtomicU32 = AtomicU32::new(0);

pub(crate) fn next_auto_generate_id() -> AutoGenerateId {
    AutoGenerateId(NEXT_AUTO_GENERATE_ID.fetch_add(1, Ordering::SeqCst) + 1)
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct AutoGenerateId(pub(crate) u32);

#[derive(Clone, Copy, Debug)]
pub struct AutoGenerateInfo {
    pub flags: GeneratedIdentifierFlags, // Specifies whether to auto-generate the text for an identifier.
    pub(crate) id: AutoGenerateId, // Ensures unique generated identifiers get unique names, but clones get the same name.
    pub(crate) prefix: &'static str, // Optional prefix to apply to the start of the generated name
    pub(crate) suffix: &'static str, // Optional suffix to apply to the end of the generated name
    pub(crate) node: Option<P<Node>>, // For a GeneratedIdentifierFlagsNode, the node from which to generate an identifier
}

bitflags::bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    #[expect(non_camel_case_types, reason = "Keep the upstream bitflags type name.")]
    pub(crate) struct emitNodeFlags: u32 {
        const hasCommentRange = 1 << 0;
        const hasSourceMapRange = 1 << 1;
    }
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum SnippetKind {
    #[default]
    TabStop,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct SnippetElement {
    pub kind: SnippetKind,
    pub order: u32,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SynthesizedComment {
    pub(crate) kind: Kind,
    loc: TextRange,
    pub(crate) has_leading_new_line: bool,
    pub(crate) has_trailing_new_line: bool,
    pub(crate) text: String,
}

// The checker's node builder sets emit flags on hundreds of thousands of synthesized nodes and nothing else, so the
// other fields (emit helpers, synthesized comments, token ranges, ...) live in a tail allocated on the first write
// (`rare_mut`); reads of an absent tail see Go's zero values. 32 bytes per map entry instead of 160.
#[derive(Clone, Default)]
pub struct EmitNode {
    flags: emitNodeFlags,
    emit_flags: EmitFlags,
    comment_range: TextRange,
    source_map_range: TextRange,
    rare: Option<Box<EmitNodeRare>>,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<EmitNode>() == 32);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<EmitNode>() == 28);

#[derive(Clone, Default)]
struct EmitNodeRare {
    token_source_map_ranges: Option<FxHashMap<Kind, TextRange>>,
    helpers: Vec<SP<EmitHelper>>,
    external_helpers_module_name: Option<P<Node>>,
    leading_comments: Vec<SynthesizedComment>,
    trailing_comments: Vec<SynthesizedComment>,
    type_node: Option<P<Node>>,
    snippet_element: Option<SnippetElement>,
}

impl EmitNode {
    fn rare(&self) -> Option<&EmitNodeRare> {
        self.rare.as_deref()
    }

    fn rare_mut(&mut self) -> &mut EmitNodeRare {
        self.rare.get_or_insert_with(Default::default)
    }

    // NOTE: This method is not guaranteed to be thread-safe
    fn copy_from(&mut self, source: &EmitNode) {
        self.flags = source.flags;
        self.emit_flags = source.emit_flags;
        self.comment_range = source.comment_range;
        self.source_map_range = source.source_map_range;
        let src = source.rare();
        let token_source_map_ranges = src.and_then(|r| r.token_source_map_ranges.clone());
        let helpers = src.map_or_else(Vec::new, |r| r.helpers.clone());
        let external_helpers_module_name = src.and_then(|r| r.external_helpers_module_name);
        let snippet_element = src.and_then(|r| r.snippet_element);
        if self.rare.is_some()
            || token_source_map_ranges.is_some()
            || !helpers.is_empty()
            || external_helpers_module_name.is_some()
            || snippet_element.is_some()
        {
            let rare = self.rare_mut();
            rare.token_source_map_ranges = token_source_map_ranges;
            rare.helpers = helpers;
            rare.external_helpers_module_name = external_helpers_module_name;
            if let Some(snippet_element) = snippet_element {
                rare.snippet_element = Some(snippet_element);
            }
        }
    }
}
