use crate::*;
use std::rc::Rc;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;

// Non-function declarations in nodecopy.go (hand-ported by printer-foundation in nodebuilder_types.rs):
//   type recoveryBoundary (nodecopy.go:98)
//   type originalRecoveryScopeState (nodecopy.go:116)
//   type wrappingTracker (nodecopy.go:134)

impl NodeBuilderImpl {
    // nodecopy.go:12
    pub(crate) fn reuse_node(&self, c: &mut Checker, node: P<Node>) -> Option<P<Node>> {
        self.try_reuse_existing_node_helper(c, node)
    }

    // nodecopy.go:20
    pub(crate) fn try_js_type_node_to_type_node(
        &self,
        c: &mut Checker,
        node: P<Node>,
    ) -> Option<P<Node>> {
        self.reuse_node(c, node)
    }

    // nodecopy.go:24
    pub(crate) fn reuse_name(
        &self,
        c: &mut Checker,
        node: P<Node>,
        is_method: bool,
    ) -> Option<P<Node>> {
        let res = self.reuse_node(c, node)?;

        let Some(text) = ast::try_get_text_of_property_name(res) else {
            return Some(res);
        };

        let kind = classify_property_name(&text, ast::is_string_literal(res), is_method);
        if ast::is_identifier(res) && kind == PropertyNameNodeKind::Identifier {
            return Some(res);
        }
        if ast::is_string_literal(res) && kind == PropertyNameNodeKind::StringLiteral {
            return Some(res);
        }

        let renamed = match kind {
            PropertyNameNodeKind::Identifier => self.new_identifier(c, &text, None),
            PropertyNameNodeKind::StringLiteral => {
                self.f.new_string_literal(self.f.alloc_text(&text), TokenFlags::None)
            }
            PropertyNameNodeKind::NumericLiteral => return Some(res),
        };
        self.e.set_original(renamed, res);
        Some(self.set_text_range(c, renamed, Some(res)))
    }

    // nodecopy.go:56
    pub(crate) fn reuse_type_node(&self, c: &mut Checker, node: P<Node>) -> Option<P<Node>> {
        let r = self.reuse_node(c, node);
        if r.is_some() {
            // After successful reuse during hover, probe the reused AST for expandable
            // type references so canIncreaseExpansionDepth is set even though
            // typeToTypeNode (and shouldExpandType) were never called.
            let ctx = self.ctx();
            if ctx.max_expansion_depth.get().is_some() && !ctx.can_increase_expansion_depth.get() {
                self.walk_node_for_expandability(c, Some(node));
            }
            return r;
        }
        self.ctx().tracker.get().unwrap().report_inference_fallback(c, node);
        let t = self.get_type_from_type_node(c, node, false);
        self.type_to_type_node(c, t)
    }

    // nodecopy.go:78
    // walkNodeForExpandability walks a reused AST node tree, calling checkTypeExpandability
    // on each type reference, type predicate, or import type node.
    // Short-circuits once canIncreaseExpansionDepth is set.
    fn walk_node_for_expandability(&self, c: &mut Checker, node: Option<P<Node>>) {
        let Some(node) = node else {
            return;
        };
        if self.ctx().can_increase_expansion_depth.get() {
            return;
        }
        // Check these explicitly so we look into type arguments wehther or not they are in the tree or not.
        if ast::is_type_reference_node(node)
            || ast::is_expression_with_type_arguments(node)
            || ast::is_type_predicate_node(node)
            || ast::is_import_type_node(node)
        {
            let t = self.get_type_from_type_node(c, node, false);
            if t.is_some() {
                self.check_type_expandability(c, t);
                if self.ctx().can_increase_expansion_depth.get() {
                    return;
                }
            }
        }
        node.for_each_child(&mut |child| {
            self.walk_node_for_expandability(c, Some(child));
            self.ctx().can_increase_expansion_depth.get()
        });
    }
}

impl RecoveryBoundary {
    // nodecopy.go:109
    fn mark_error(&self, f: Option<Box<dyn FnOnce()>>) {
        self.had_error.set(true);
        if let Some(f) = f {
            self.deferred_reports.borrow_mut().push(f);
        }
    }

    // nodecopy.go:122
    fn start_recovery_scope(&self) -> OriginalRecoveryScopeState {
        let tracked_symbols_top = self.ctx.tracked_symbols.borrow().len();
        let unreported_errors_top = self.deferred_reports.borrow().len();
        OriginalRecoveryScopeState {
            tracked_symbols_top,
            unreported_errors_top,
            had_error: self.had_error.get(),
        }
    }

    // nodecopy.go:128
    fn end_recovery_scope(&self, state: OriginalRecoveryScopeState) {
        self.had_error.set(state.had_error);
        self.ctx.tracked_symbols.borrow_mut().truncate(state.tracked_symbols_top);
        self.deferred_reports.borrow_mut().truncate(state.unreported_errors_top);
    }
}

impl WrappingTracker {
    // nodecopy.go:139
    pub(crate) fn pop_error_fallback_node(&self) {
        self.wrapped.pop_error_fallback_node();
    }

    // nodecopy.go:143
    pub(crate) fn push_error_fallback_node(&self, node: Option<P<Node>>) {
        self.wrapped.push_error_fallback_node(node);
    }

    // nodecopy.go:147
    pub(crate) fn report_cyclic_structure_error(&self) {
        let wrapped = self.wrapped;
        self.bound.mark_error(Some(Box::new(move || {
            wrapped.report_cyclic_structure_error();
        })));
    }

    // nodecopy.go:151
    pub(crate) fn report_inaccessible_this_error(&self) {
        let wrapped = self.wrapped;
        self.bound.mark_error(Some(Box::new(move || {
            wrapped.report_inaccessible_this_error();
        })));
    }

    // nodecopy.go:155
    pub(crate) fn report_inaccessible_unique_symbol_error(&self) {
        let wrapped = self.wrapped;
        self.bound.mark_error(Some(Box::new(move || {
            wrapped.report_inaccessible_unique_symbol_error();
        })));
    }

    // nodecopy.go:159
    pub(crate) fn report_inference_fallback(&self, c: &mut Checker, node: P<Node>) {
        self.wrapped.report_inference_fallback(c, node); // Should this also be deferred?
    }

    // nodecopy.go:163
    pub(crate) fn report_likely_unsafe_import_required_error(
        &self,
        specifier: &str,
        symbol_name: &str,
    ) {
        let wrapped = self.wrapped;
        let specifier = specifier.to_string();
        let symbol_name = symbol_name.to_string();
        self.bound.mark_error(Some(Box::new(move || {
            wrapped.report_likely_unsafe_import_required_error(&specifier, &symbol_name);
        })));
    }

    // nodecopy.go:167
    pub(crate) fn report_non_serializable_property(&self, property_name: &str) {
        let wrapped = self.wrapped;
        let property_name = property_name.to_string();
        self.bound.mark_error(Some(Box::new(move || {
            wrapped.report_non_serializable_property(&property_name);
        })));
    }

    // nodecopy.go:171
    pub(crate) fn report_nonlocal_augmentation(
        &self,
        containing_file: P<SourceFile>,
        parent_symbol: P<Symbol>,
        augmenting_symbol: P<Symbol>,
    ) {
        self.wrapped.report_nonlocal_augmentation(
            containing_file,
            parent_symbol,
            augmenting_symbol,
        ); // Should this also be deferred?
    }

    // nodecopy.go:175
    pub(crate) fn report_private_in_base_of_class_expression(&self, property_name: &str) {
        let wrapped = self.wrapped;
        let property_name = property_name.to_string();
        self.bound.mark_error(Some(Box::new(move || {
            wrapped.report_private_in_base_of_class_expression(&property_name);
        })));
    }

    // nodecopy.go:179
    pub(crate) fn report_truncation_error(&self) {
        self.wrapped.report_truncation_error(); // Should this also be deferred?
    }

    // nodecopy.go:183
    pub(crate) fn track_symbol(
        &self,
        _c: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
    ) -> bool {
        self.bound.tracked_symbols.borrow_mut().push(TrackedSymbolArgs {
            symbol,
            enclosing_declaration,
            meaning,
        });
        false
    }
}

// nodecopy.go:188
pub fn new_wrapping_tracker(
    inner: &'static dyn SymbolTracker,
    bound: P<RecoveryBoundary>,
) -> P<WrappingTracker> {
    P::new_scratch(WrappingTracker { wrapped: inner, bound })
}

impl NodeBuilderImpl {
    // nodecopy.go:195
    fn create_recovery_boundary(&self, c: &Checker) -> P<RecoveryBoundary> {
        c.check_not_canceled();
        let ctx = self.ctx();
        let bound = P::new_scratch(RecoveryBoundary {
            ctx,
            had_error: Cell::new(false),
            deferred_reports: RefCell::new(Vec::new()),
            old_tracker: ctx.tracker.get(),
            // Go keeps the old slice and sets `b.ctx.trackedSymbols = nil` below.
            old_tracked_symbols: RefCell::new(ctx.tracked_symbols.take()),
            tracked_symbols: RefCell::new(Vec::new()),
            old_encountered_error: ctx.encountered_error.get(),
            old_approximate_length: ctx.approximate_length.get(),
        });
        let new_tracker = new_symbol_tracker_impl(
            ctx,
            Some(new_wrapping_tracker(ctx.tracker.get().unwrap(), bound).get()),
        );
        ctx.tracker.set(Some(new_tracker.get()));
        bound
    }

    // nodecopy.go:204
    fn finalize_boundary(&self, c: &mut Checker, bound: P<RecoveryBoundary>) -> bool {
        let _ = c;
        let ctx = self.ctx();
        ctx.tracker.set(bound.old_tracker);
        *ctx.tracked_symbols.borrow_mut() = bound.old_tracked_symbols.take();
        ctx.encountered_error.set(bound.old_encountered_error);
        ctx.approximate_length.set(bound.old_approximate_length);

        let deferred_reports = bound.deferred_reports.take();
        for f in deferred_reports {
            f();
        }
        if bound.had_error.get() {
            return false;
        }
        let tracked_symbols = bound.tracked_symbols.borrow().clone();
        for a in tracked_symbols {
            ctx.tracker.get().unwrap().track_symbol(
                c,
                a.symbol,
                a.enclosing_declaration,
                a.meaning,
            );
        }
        true
    }

    // nodecopy.go:222
    pub(crate) fn try_reuse_existing_node_helper(
        &self,
        c: &mut Checker,
        existing: P<Node>,
    ) -> Option<P<Node>> {
        let bound = self.create_recovery_boundary(c);
        let mut v = get_existing_node_tree_visitor(c, self.as_p(), bound); // !!! TODO: Cache visitor and just reset bound+host builder? We try this for a *lot* of nodes.
        let transformed = self.checker_slot.lend(c, || v.visit_node(Some(existing)));
        if !self.finalize_boundary(c, bound) {
            return None;
        }
        let ctx = self.ctx();
        // Synthetic nodes have both positions set to -1 in Go, so their length is zero.
        let existing_length = existing.loc().len().unwrap_or(0) as usize;
        ctx.approximate_length.set(ctx.approximate_length.get() + existing_length);
        transformed
    }

    // nodecopy.go:234
    fn get_module_specifier_override(
        &self,
        c: &mut Checker,
        parent: P<Node>,
        lit: P<Node>,
    ) -> String {
        let ctx = self.ctx();
        if ctx.enclosing_file.get() != ast::get_source_file_of_node(lit) {
            let mut mode = RESOLUTION_MODE_NONE;
            if let Some(attributes) = parent.as_import_type_node().attributes {
                mode = ast::get_resolution_mode_override(Some(attributes), None)
                    .unwrap_or(RESOLUTION_MODE_NONE);
            }
            let mut name: &'static str = lit.text();
            let original_name = name;
            let node_symbol = Self::try_get_resolved_symbol_from_type_node(c, parent);
            let meaning = if parent.as_import_type_node().is_type_of {
                SymbolFlags::Value
            } else {
                SymbolFlags::Type
            };
            let mut parent_symbol: Option<P<Symbol>> = None;
            if let Some(node_symbol) = node_symbol
                && c.is_symbol_accessible(
                    Some(node_symbol),
                    ctx.enclosing_declaration.get(),
                    meaning,
                    false,
                )
                .accessibility
                    == SymbolAccessibility::Accessible
            {
                parent_symbol = Some(self.lookup_symbol_chain(c, node_symbol, meaning, true)[0]);
            }
            if let Some(parent_symbol) = parent_symbol.filter(|s| is_external_module_symbol(*s)) {
                name = self.get_specifier_for_module_symbol(c, parent_symbol, mode).specifier;
            } else {
                let target_file = c.get_external_module_file_from_declaration(parent);
                if let Some(target_file) = target_file {
                    name = self
                        .get_specifier_for_module_symbol(
                            c,
                            target_file.as_node().symbol().unwrap(),
                            mode,
                        )
                        .specifier;
                }
            }
            if !name.is_empty() && name.contains("/node_modules/") {
                ctx.encountered_error.set(true);
                ctx.tracker.get().unwrap().report_likely_unsafe_import_required_error(name, "");
            }
            if name != original_name {
                return name.to_string();
            }
        }
        String::new()
    }

    // nodecopy.go:270
    fn rewrite_module_specifier(&self, c: &mut Checker, parent: P<Node>, lit: P<Node>) -> P<Node> {
        let mut new_name = self.get_module_specifier_override(c, parent, lit);
        if new_name.is_empty() {
            let Some(rewritten) = self.rewrite_content_mapped_module_specifier(c, lit.text())
            else {
                return lit;
            };
            if rewritten == lit.text() {
                return lit;
            }
            new_name = rewritten;
        }
        let res = self.f.new_string_literal(self.f.alloc_text(&new_name), TokenFlags::None);
        self.e.set_original(res, lit);
        res
    }

    // nodecopy.go:280
    fn get_enclosing_declaration_ignoring_fake_scope(&self, c: &Checker) -> Option<P<Node>> {
        let _ = c;
        let mut enc = self.ctx().enclosing_declaration.get();
        while let Some(e) = enc {
            if self.links.get(e).fake_scope_for_signature_declaration.get().is_none() {
                break;
            }
            enc = e.parent();
        }
        enc
    }
}

// nodecopy.go:288
pub fn get_existing_node_tree_visitor(
    c: &Checker,
    b: P<NodeBuilderImpl>,
    bound: P<RecoveryBoundary>,
) -> NodeVisitor {
    // TODO: wrap all these closures into methods on an object so we can guarantee we reuse the same memory on each invocation by reusing/resetting the object
    // instead of re-closing-over all of these each time we need a visitor. In theory the compiler could handle this, but in practice closure inlining hasn't been reliable
    //
    // Rust: Go's nested closures are the methods of `ExistingNodeTree` below (they close over `b`, `bound` and the
    // visitor itself, which the Rust callbacks receive as `v`). The checker reaches the callbacks through
    // `b.checker_slot` (see `CheckerSlot`); the caller lends it around `v.visit_node(..)`.
    let _ = c;
    let t = ExistingNodeTree { b, bound };
    let slot = b.checker_slot;
    let non_local_node = Rc::new(Cell::new(true));
    let visit: VisitFn =
        Rc::new(move |v: &mut NodeVisitor, node: P<Node>| slot.with(|c| t.visit(c, v, node)));
    let visit_nodes: VisitNodesHook = {
        let non_local_node = Rc::clone(&non_local_node);
        Rc::new(move |nodes: Option<P<NodeList>>, v: &mut NodeVisitor| {
            let mut res = v.visit_nodes(nodes);
            if non_local_node.get()
                && let Some(r) = res
            {
                // Remove position data from node lists originating in other files
                let r = if Some(r) == nodes { nodes.unwrap().clone_list(&b.f) } else { r };
                r.loc.set(TextRange::synthetic());
                res = Some(r);
            }
            res
        })
    };
    let visit_node: VisitNodeHook = Rc::new(move |node: Option<P<Node>>, v: &mut NodeVisitor| {
        // Capture if the current node is in the current file so node lists knoww if they can keep positions or not
        let old_non_local_node = non_local_node.get();
        let enclosing_file = b.ctx().enclosing_file.get();
        non_local_node.set(
            enclosing_file.is_none()
                || enclosing_file != ast::get_source_file_of_node(b.e.most_original(node)),
        );
        let res = v.visit_node(node);
        non_local_node.set(old_non_local_node);
        res
    });
    new_node_visitor(
        Some(visit),
        Some(b.f.clone()),
        NodeVisitorHooks {
            visit_nodes: Some(visit_nodes),
            visit_node: Some(visit_node),
            ..Default::default()
        },
    )
}

/// The closures of `getExistingNodeTreeVisitor`. `v` is Go's `visitor` (the existing-node visitor itself); every
/// re-entry into it re-lends the checker (`visit_node`, `visit_nodes`, `visit_modifiers`, `visit_each_child`).
#[derive(Clone, Copy)]
struct ExistingNodeTree {
    b: P<NodeBuilderImpl>,
    bound: P<RecoveryBoundary>,
}

impl ExistingNodeTree {
    fn visit_node(
        self,
        c: &mut Checker,
        v: &mut NodeVisitor,
        node: Option<P<Node>>,
    ) -> Option<P<Node>> {
        self.b.checker_slot.lend(c, || v.visit_node(node))
    }

    fn visit_nodes(
        self,
        c: &mut Checker,
        v: &mut NodeVisitor,
        nodes: Option<P<NodeList>>,
    ) -> Option<P<NodeList>> {
        self.b.checker_slot.lend(c, || v.visit_nodes(nodes))
    }

    fn visit_modifiers(
        self,
        c: &mut Checker,
        v: &mut NodeVisitor,
        nodes: Option<P<ModifierList>>,
    ) -> Option<P<ModifierList>> {
        self.b.checker_slot.lend(c, || v.visit_modifiers(nodes))
    }

    fn visit_each_child(
        self,
        c: &mut Checker,
        v: &mut NodeVisitor,
        node: P<Node>,
    ) -> Option<P<Node>> {
        self.b.checker_slot.lend(c, || v.visit_each_child(Some(node)))
    }

    /// `b.setTextRange(range, location)` with non-nil arguments (never returns nil).
    fn set_text_range(self, c: &mut Checker, range_: P<Node>, location: P<Node>) -> P<Node> {
        self.b.set_text_range(c, range_, Some(location))
    }

    fn tracker(self) -> &'static dyn SymbolTracker {
        self.b.ctx().tracker.get().unwrap()
    }

    // note: also handles renaming type parameters renamed within the current context
    fn attach_symbol_to_leftmost_identifier(
        self,
        c: &mut Checker,
        leftmost: P<Node>,
        node: P<Node>,
        sym: Option<P<Symbol>>,
    ) -> P<Node> {
        let slot = self.b.checker_slot;
        let visitor_func: VisitFn = Rc::new(move |vis: &mut NodeVisitor, node: P<Node>| {
            slot.with(|c| Some(self.attach_symbol_visitor_func(c, vis, leftmost, sym, node)))
        });
        let mut vis = new_node_visitor(
            Some(visitor_func),
            Some(self.b.f.clone()),
            NodeVisitorHooks::default(),
        );
        self.attach_symbol_visitor_func(c, &mut vis, leftmost, sym, node)
    }

    // visitorFunc of attachSymbolToLeftmostIdentifier
    fn attach_symbol_visitor_func(
        self,
        c: &mut Checker,
        vis: &mut NodeVisitor,
        leftmost: P<Node>,
        sym: Option<P<Symbol>>,
        node: P<Node>,
    ) -> P<Node> {
        let b = self.b;
        if node == leftmost {
            let mut name: Option<P<Node>> = None;
            if let Some(sym) = sym {
                let type_ = c.get_declared_type_of_symbol(sym);
                if sym.flags().intersects(SymbolFlags::TypeParameter) {
                    name = Some(b.type_parameter_to_name(c, type_));
                }
            }
            let name = match name {
                Some(name) => name,
                None => b.new_identifier(c, node.text(), sym),
            };
            let name = self.set_text_range(c, name, node);
            b.e.add_emit_flags(name, EmitFlags::NoAsciiEscaping);
            return name;
        }
        let visited = b.checker_slot.lend(c, || node.visit_each_child(vis));
        self.set_text_range(c, visited, node)
    }

    fn track_existing_entity_name(
        self,
        c: &mut Checker,
        node: P<Node>,
        override_enclosing: Option<P<Node>>,
    ) -> (bool, P<Node>, Option<P<Symbol>>) {
        let b = self.b;
        let mut enclosing_declaration = b.ctx().enclosing_declaration.get();
        if override_enclosing.is_some() {
            enclosing_declaration = override_enclosing;
        }
        let mut introduces_error = false;
        let leftmost = ast::get_first_identifier(node);
        if ast::is_in_js_file(node)
            && (ast::is_exports_identifier(leftmost)
                || ast::is_module_exports_access_expression(leftmost.parent().unwrap())
                || (ast::is_qualified_name(leftmost.parent().unwrap())
                    && ast::is_module_identifier(
                        leftmost.parent().unwrap().as_qualified_name().left,
                    )
                    && ast::is_exports_identifier(
                        leftmost.parent().unwrap().as_qualified_name().right,
                    )))
        {
            introduces_error = true;
            return (
                introduces_error,
                self.set_text_range(c, b.f.deep_clone_node(Some(node)).unwrap(), node),
                None,
            );
        }
        let meaning = get_meaning_of_entity_name_reference(node);
        let mut sym: Option<P<Symbol>>;
        if ast::is_this_identifier(leftmost) {
            // `this` isn't a bindable identifier - skip resolution, find a relevant `this` symbol directly and avoid exhaustive scope traversal
            let this_container =
                checker::Checker::get_this_container(leftmost, false, false).unwrap();
            sym = c.get_symbol_of_declaration(this_container);
            if c.is_symbol_accessible(sym, Some(leftmost), meaning, false).accessibility
                != SymbolAccessibility::Accessible
            {
                introduces_error = true;
                self.tracker().report_inaccessible_this_error();
            }
            return (
                introduces_error,
                self.attach_symbol_to_leftmost_identifier(c, leftmost, node, sym),
                None,
            );
        }
        sym = c.resolve_entity_name(leftmost, meaning, true, true, None);
        if b.ctx().enclosing_declaration.get().is_some()
            && !sym.is_some_and(|s| s.flags().intersects(SymbolFlags::TypeParameter))
        {
            sym = c.get_export_symbol_of_value_symbol_if_exported(sym);
            // Some declarations may be transplanted to a new location.
            // When this happens we need to make sure that the name has the same meaning at both locations
            // We also check for the unknownSymbol because when we create a fake scope some parameters may actually not be usable
            // either because they are the expanded rest parameter,
            // or because they are the newly added parameters from the tuple, which might have different meanings in the original context
            let sym_at_location = c.resolve_entity_name(
                leftmost,
                meaning,
                true,
                true,
                b.ctx().enclosing_declaration.get(),
            );
            let unknown_symbol = c.unknown_symbol;
            if
            // Check for unusable parameters symbols
            sym_at_location == Some(unknown_symbol)
                // If the symbol is not found, but was not found in the original scope either we probably have an error, don't reuse the node
                || (sym_at_location.is_none() && sym.is_some())
                // If the symbol is found both in declaration scope and in current scope then it should point to the same reference
                || match (sym_at_location, sym) {
                    (Some(sym_at_location), Some(sym)) => {
                        let export_symbol = c.get_export_symbol_of_value_symbol_if_exported(Some(sym_at_location)).unwrap();
                        c.get_symbol_if_same_reference(export_symbol, sym).is_none()
                    }
                    _ => false,
                }
            {
                // In isolated declaration we will not do rest parameter expansion so there is no need to report on these.
                if sym_at_location != Some(unknown_symbol) {
                    self.tracker().report_inference_fallback(c, node);
                }
                introduces_error = true;
                return (
                    introduces_error,
                    self.set_text_range(c, b.f.deep_clone_node(Some(node)).unwrap(), node),
                    sym,
                );
            }
            sym = sym_at_location;
        }

        if let Some(sym) = sym {
            // If a parameter is resolvable in the current context it is also visible, so no need to go to symbol accesibility
            if sym.flags().intersects(SymbolFlags::FunctionScopedVariable)
                && let Some(value_declaration) = sym.value_declaration()
                && (ast::is_part_of_parameter_declaration(value_declaration)
                    || ast::is_jsdoc_parameter_tag(value_declaration))
            {
                return (
                    introduces_error,
                    self.attach_symbol_to_leftmost_identifier(c, leftmost, node, Some(sym)),
                    None,
                );
            }
            if !sym.flags().intersects(SymbolFlags::TypeParameter) /* Type parameters are visible in the current context if they are are resolvable */
                && !ast::is_declaration_name(node)
                && c.is_symbol_accessible(Some(sym), enclosing_declaration, meaning, false).accessibility != SymbolAccessibility::Accessible
            {
                self.tracker().report_inference_fallback(c, node);
                introduces_error = true;
            } else {
                self.tracker().track_symbol(c, sym, enclosing_declaration, meaning);
            }
            return (
                introduces_error,
                self.attach_symbol_to_leftmost_identifier(c, leftmost, node, Some(sym)),
                None,
            );
        }
        (
            introduces_error,
            self.set_text_range(c, b.f.deep_clone_node(Some(node)).unwrap(), node),
            None,
        )
    }

    fn try_visit_indexed_access(
        self,
        c: &mut Checker,
        v: &mut NodeVisitor,
        node: P<Node>,
    ) -> Option<P<Node>> {
        let result_object_type =
            self.try_visit_simple_type_node(c, v, node.as_indexed_access_type_node().object_type)?;
        let index_type =
            self.visit_node(c, v, Some(node.as_indexed_access_type_node().index_type)).unwrap();
        let updated =
            self.b.f.update_indexed_access_type_node(node, result_object_type, index_type);
        Some(self.set_text_range(c, updated, node))
    }

    fn try_visit_key_of(
        self,
        c: &mut Checker,
        v: &mut NodeVisitor,
        node: P<Node>,
    ) -> Option<P<Node>> {
        let to = node.as_type_operator_node();
        let t = self.try_visit_simple_type_node(c, v, to.type_)?;
        let updated = self.b.f.update_type_operator_node(node, to.operator, t);
        Some(self.set_text_range(c, updated, node))
    }

    fn try_visit_type_query(
        self,
        c: &mut Checker,
        v: &mut NodeVisitor,
        node: P<Node>,
    ) -> Option<P<Node>> {
        let expr_name_node = node.as_type_query_node().expr_name;
        let (introduces_error, expr_name, _) =
            self.track_existing_entity_name(c, expr_name_node, None);
        if !introduces_error {
            let type_arguments = self.visit_nodes(c, v, node.type_argument_list());
            let updated = self.b.f.update_type_query_node(node, expr_name, type_arguments);
            return Some(self.set_text_range(c, updated, node));
        }

        let type_arguments = self.visit_nodes(c, v, node.type_argument_list());
        let serialized_name = self.b.serialize_type_name(c, expr_name_node, true, type_arguments);
        if let Some(serialized_name) = serialized_name {
            return Some(self.set_text_range(c, serialized_name, expr_name_node));
        }
        None
    }

    fn try_visit_type_reference(
        self,
        c: &mut Checker,
        v: &mut NodeVisitor,
        node: P<Node>,
    ) -> Option<P<Node>> {
        let b = self.b;
        if ast::is_const_type_reference(node) {
            return None;
        }
        let s =
            nodebuilder_types::NodeBuilderImpl::try_get_resolved_symbol_from_type_node(c, node)?; // ???
        if s.flags().intersects(SymbolFlags::TypeParameter) {
            let declared_type = c.get_declared_type_of_symbol(s);
            if let Some(mapper) = b.ctx().mapper.get()
                && c.get_mapped_type(declared_type, mapper) != declared_type
            {
                return None; // refers to type parameter remapped by context (TODO improvement: just return the remapped param name?)
            }
        }
        let t = b.get_type_from_type_node(c, node, false).unwrap();
        if !nodebuilder_types::NodeBuilderImpl::can_reuse_existing_js_type_node(c, node, t) {
            // fallback to serialization for jsdoc types that have insufficient or incomplete type args, or are remapped by the checker in only jsdoc contexts
            // TODO: remappings like `promise` -> `Promise<any>` are static, we *could* statically remap the nodes, too. But that only matters for `isolatedDeclarations`
            // in JS, should we enable that.
            return None;
        }
        let type_name = node.as_type_reference_node().type_name;
        let (introduces_error, new_name, _) = self.track_existing_entity_name(c, type_name, None);
        if introduces_error {
            let type_arguments = self.visit_nodes(c, v, node.type_argument_list());
            let serialized_name = b.serialize_type_name(c, type_name, false, type_arguments);
            if let Some(serialized_name) = serialized_name {
                return Some(self.set_text_range(c, serialized_name, type_name));
            }
            None
        } else {
            let type_arguments = self.visit_nodes(c, v, node.type_argument_list());
            let updated = b.f.update_type_reference_node(node, new_name, type_arguments);
            Some(self.set_text_range(c, updated, node))
        }
    }

    fn try_visit_simple_type_node(
        self,
        c: &mut Checker,
        v: &mut NodeVisitor,
        node: P<Node>,
    ) -> Option<P<Node>> {
        let inner_node = ast::skip_parentheses(node);
        match inner_node.kind() {
            Kind::TypeReference => return self.try_visit_type_reference(c, v, inner_node),
            Kind::TypeQuery => return self.try_visit_type_query(c, v, inner_node),
            Kind::IndexedAccessType => return self.try_visit_indexed_access(c, v, inner_node),
            Kind::TypeOperator
                if inner_node.as_type_operator_node().operator == Kind::KeyOfKeyword =>
            {
                return self.try_visit_key_of(c, v, inner_node);
            }
            _ => {}
        }
        self.visit_node(c, v, Some(node))
    }

    fn visit_existing_node_tree_symbols_worker(
        self,
        c: &mut Checker,
        v: &mut NodeVisitor,
        node: P<Node>,
    ) -> Option<P<Node>> {
        let b = self.b;
        let bound = self.bound;
        let mut node = node;
        let factory = &b.f;
        // !!! TODO: the reparser *should* make all the jsdoc remapping logic here redundant,
        // assuming we only ever try to preserve reparsed nodes and never walk back to the jsdoc "originals"
        // accidentally.
        // Still, what can be ported of the logic is here, just in case.
        // Begin JSDoc handling
        if node.kind() == Kind::JSDocTypeExpression {
            // Unwrap JSDocTypeExpressions
            return self.visit_node(c, v, Some(node.as_jsdoc_type_expression().type_));
        }
        // !!! TODO: We don't _actually_ support jsdoc namepath types, emit `any` instead; verify we handle as gracefully as strada
        if node.kind() == Kind::JSDocAllType
        /* || node.Kind == ast.JSDocNamepathType */
        {
            return Some(factory.new_keyword_type_node(Kind::AnyKeyword));
        }
        // !!! TODO: verify JSDocUnknwonType is hopefully just parsed into `unknown` upfront; the kind no longer exists
        // if node.Kind == ast.KindJSDocUnknownType {
        // 	return factory.NewKeywordTypeNode(ast.KindUnknownKeyword)
        // }
        if node.kind() == Kind::JSDocNullableType {
            let union_members = vec![
                self.visit_node(c, v, Some(node.as_jsdoc_nullable_type().type_)).unwrap(),
                factory.new_literal_type_node(factory.new_keyword_expression(Kind::NullKeyword)),
            ];
            return Some(factory.new_union_type_node(factory.new_node_list(union_members)));
        }
        if node.kind() == Kind::JSDocOptionalType {
            let union_members = vec![
                self.visit_node(c, v, Some(node.as_jsdoc_optional_type().type_)).unwrap(),
                factory.new_keyword_type_node(Kind::UndefinedKeyword),
            ];
            return Some(factory.new_union_type_node(factory.new_node_list(union_members)));
        }
        if node.kind() == Kind::JSDocNonNullableType {
            // Unwrap
            return self.visit_node(c, v, Some(node.as_jsdoc_non_nullable_type().type_));
        }
        if node.kind() == Kind::JSDocVariadicType {
            // !!! TODO: verify this matches how jsdoc variadics are actually handled now?
            let element_type =
                self.visit_node(c, v, Some(node.as_jsdoc_variadic_type().type_)).unwrap();
            return Some(factory.new_array_type_node(element_type));
        }
        if node.kind() == Kind::JSDocTypeLiteral {
            let mut members: Vec<P<Node>> = Vec::new();
            members.extend(node.as_jsdoc_type_literal().jsdoc_property_tags.iter().filter_map(
                |&t| {
                    if t.kind() != Kind::JSDocPropertyTag && t.kind() != Kind::JSDocParameterTag {
                        return None;
                    }
                    let n = t.name().unwrap();
                    let target_name = if ast::is_identifier(n) {
                        n
                    } else {
                        n.as_qualified_name().right // !!! TODO: without typesystem backup, doing this cast unguarded seems really suspect, even though it is what strada does
                    };
                    let name = self.visit_node(c, v, Some(target_name));
                    let should_be_optional = t.as_jsdoc_parameter_or_property_tag().is_bracketed
                        || t.type_expression()
                            .is_some_and(|te| te.kind() == Kind::JSDocOptionalType);
                    let question = if should_be_optional {
                        Some(factory.new_token(Kind::QuestionToken))
                    } else {
                        None
                    };
                    let ty = self.visit_node(c, v, t.type_expression()); // !!! TODO: alternate lookup locations for the type? serialize on demand if it doesn't serialze? strada does something funky here.

                    Some(factory.new_property_signature_declaration(
                        None,
                        name.unwrap(),
                        question,
                        ty,
                        None,
                    ))
                },
            ));
            return Some(factory.new_type_literal_node(factory.new_node_list(members)));
        }
        // if (ast.IsExpressionWithTypeArguments(node) || ast.IsTypeReferenceNode(node)) && ast.IsJSDocIndexSignature(node) { /// !!! TODO: JSDocIndexSignature handling hasn't been ported - readd if it's readded
        // 	args := node.TypeArguments()
        // 	if len(args) != 2 {
        // 		return factory.NewKeywordTypeNode(ast.KindAnyKeyword) // shouldn't be flagged as a jsdoc index signature in the first place
        // 	}
        // 	return factory.NewTypeLiteralNode(factory.NewNodeList([]*ast.Node{
        // 		factory.NewIndexSignatureDeclaration(nil, factory.NewNodeList([]*ast.Node{
        // 			factory.NewParameterDeclaration(nil, nil, factory.NewIdentifier("x"), nil, visitor.VisitNode(args[0]), nil),
        // 		}), visitor.VisitNode(args[1])),
        // 	}))
        // }
        // if node.Kind == ast.KindJSDocFunctionType {} // !!! no longer exists
        // End JSDoc handling

        if ast::is_type_reference_node(node)
            && ast::is_identifier(node.as_type_reference_node().type_name)
            && node.as_type_reference_node().type_name.as_identifier().text() == ""
        {
            let replacement = factory.new_keyword_type_node(Kind::AnyKeyword);
            b.e.set_original(replacement, node);
            return Some(replacement);
        }
        if ast::is_this_type_node(node) {
            // TODO: strada never marks `this` type nodes as an error - it calls `canReuseTypeNode` on it, but that function always returns `true` for `this`
            // type nodes, which in turn fails to verify that the `this` context is the same between the source and target locations. The conservative thing is to
            // _never_ copy a `this`. We could improve this, but strada is *definitely* wrong and overbroad here. (note that we're inling uses of `canReuseTypeNode`
            // in corsa because of the unfurled host structure meaning we don't need to defer to a host object for functionality it needs)
            // bound.markError(nil) // conservative approach
            return Some(node);
        }
        if ast::is_type_parameter_declaration(node) {
            let (_, new_name, _) = self.track_existing_entity_name(c, node.name().unwrap(), None);
            let tp = node.as_type_parameter_declaration();
            let modifiers = self.visit_modifiers(c, v, node.modifiers());
            let constraint = self.visit_node(c, v, tp.constraint);
            let expression = self.visit_node(c, v, tp.expression);
            let default_type = self.visit_node(c, v, tp.default_type);
            return Some(factory.update_type_parameter_declaration(
                node,
                modifiers,
                new_name,
                constraint,
                expression,
                default_type,
            ));
        }
        if ast::is_indexed_access_type_node(node) {
            let result = self.try_visit_indexed_access(c, v, node);
            if result.is_some() {
                return result;
            }
            bound.mark_error(None);
            return Some(node);
        }
        if ast::is_type_reference_node(node) {
            let result = self.try_visit_type_reference(c, v, node);
            if result.is_some() {
                return result;
            }
            bound.mark_error(None);
            return Some(node);
        }
        if ast::is_type_query_node(node) {
            let result = self.try_visit_type_query(c, v, node);
            if result.is_some() {
                return result;
            }
            bound.mark_error(None);
            return Some(node);
        }
        if ast::is_type_operator_node(node) {
            let to = node.as_type_operator_node();
            if to.operator == Kind::UniqueKeyword && to.type_.kind() == Kind::SymbolKeyword {
                let non_fake_enclosing = b.get_enclosing_declaration_ignoring_fake_scope(c);
                let same_scope = ast::find_ancestor(node, |a| Some(a) == non_fake_enclosing);
                if same_scope.is_none() {
                    bound.mark_error(None);
                    return Some(node);
                }
            } else if to.operator == Kind::KeyOfKeyword {
                let result = self.try_visit_key_of(c, v, node);
                if result.is_some() {
                    return result;
                }
                bound.mark_error(None);
                return Some(node);
            }
        }
        if ast::is_literal_import_type_node(node) {
            let it = node.as_import_type_node();
            // assert keyword in imported attributes is deprecated, so we don't reuse types that contain it
            // Ex: import("pkg", { assert: {} }
            if it.attributes.is_some_and(|a| a.as_import_attributes().token == Kind::AssertKeyword)
            {
                bound.mark_error(None);
                return Some(node);
            }
            let t = b.get_type_from_type_node(c, node, true);
            if t.is_none() {
                bound.mark_error(None);
                return Some(node);
            }
            // if ast.IsInJSFile(node) {
            // 	// !!! TODO: invalidate node reuse if js fallback logic used in type param list/typeof lookup (but isn't this logic gone?)
            // 	// s := b.ch.symbolNodeLinks.Get(node).resolvedSymbol
            // }
            let original_spec = it.argument.as_literal_type_node().literal;
            let mut specifier = b.rewrite_module_specifier(c, node, original_spec);
            if original_spec == specifier {
                specifier = self.visit_node(c, v, Some(specifier)).unwrap(); // visit node if not replaced
            }
            let mut arg = it.argument;
            if specifier != original_spec {
                arg = factory.new_literal_type_node(specifier);
            }
            let attributes = self.visit_node(c, v, it.attributes);
            let qualifier = self.visit_node(c, v, it.qualifier);
            let type_arguments = self.visit_nodes(c, v, node.type_argument_list());
            return Some(factory.update_import_type_node(
                node,
                it.is_type_of,
                arg,
                attributes,
                qualifier,
                type_arguments,
            ));
        }
        if let Some(name) = node.name()
            && name.kind() == Kind::ComputedPropertyName
            && !c.has_late_bindable_name(node)
        {
            if !ast::has_dynamic_name(node) {
                // !!! TODO: This matches strada, but rather than recursing, this should probably fall down to later cases.
                // Take a `["field"]` property declaration - it still needs a `: any` appended to it
                return self.visit_each_child(c, v, node);
            }
            // !!! TODO: this condition matches strada, but it just seems wrong? Or at the very least extraordinarily approximate, and doesn't flag a builder error...
            let should_remove_declaration =
                !(b.ctx().internal_flags.get().intersects(InternalFlags::AllowUnresolvedNames)
                    && ast::is_entity_name_expression(name.as_computed_property_name().expression)
                    && c.check_computed_property_name(name).flags().intersects(TypeFlags::Any));
            if should_remove_declaration {
                return None;
            }
        }
        if (ast::is_function_like(node) && node.type_node().is_none())
            || (ast::is_property_declaration(node)
                && node.type_node().is_none()
                && node.initializer().is_none())
            || (ast::is_property_signature_declaration(node)
                && node.type_node().is_none()
                && node.initializer().is_none())
            || (ast::is_parameter_declaration(node)
                && node.type_node().is_none()
                && node.initializer().is_none())
        {
            let mut visited = self.visit_each_child(c, v, node).unwrap();
            if visited == node {
                visited = self.set_text_range(c, node.clone_node(factory), node);
            }
            node = visited;
            let new_type = factory.new_keyword_type_node(Kind::AnyKeyword);
            match node.kind() {
                Kind::PropertyDeclaration => {
                    return Some(factory.update_property_declaration(
                        node,
                        node.modifiers(),
                        node.name().unwrap(),
                        node.postfix_token(),
                        Some(new_type),
                        None,
                    ));
                }
                Kind::PropertySignature => {
                    return Some(factory.update_property_signature_declaration(
                        node,
                        node.modifiers(),
                        node.name().unwrap(),
                        node.postfix_token(),
                        Some(new_type),
                        None,
                    ));
                }
                Kind::Parameter => {
                    let pd = node.as_parameter_declaration();
                    return Some(factory.update_parameter_declaration(
                        node,
                        None,
                        pd.dot_dot_dot_token(),
                        node.name().unwrap(),
                        pd.question_token.get(),
                        Some(new_type),
                        None,
                    ));
                }
                Kind::MethodSignature => {
                    return Some(factory.update_method_signature_declaration(
                        node,
                        node.modifiers(),
                        node.name().unwrap(),
                        node.postfix_token(),
                        node.type_parameter_list(),
                        node.parameter_list(),
                        Some(new_type),
                    ));
                }
                Kind::CallSignature => {
                    return Some(factory.update_call_signature_declaration(
                        node,
                        node.type_parameter_list(),
                        node.parameter_list(),
                        Some(new_type),
                    ));
                }
                Kind::JSDocSignature => {
                    return Some(factory.update_jsdoc_signature(
                        node,
                        node.type_parameter_list(),
                        node.parameter_list(),
                        Some(new_type),
                    ));
                }
                Kind::ConstructSignature => {
                    return Some(factory.update_construct_signature_declaration(
                        node,
                        node.type_parameter_list(),
                        node.parameter_list(),
                        Some(new_type),
                    ));
                }
                Kind::IndexSignature => {
                    return Some(factory.update_index_signature_declaration(
                        node,
                        node.modifiers(),
                        node.parameter_list(),
                        Some(new_type),
                    ));
                }
                Kind::FunctionType => {
                    return Some(factory.update_function_type_node(
                        node,
                        node.type_parameter_list(),
                        node.parameter_list(),
                        Some(new_type),
                    ));
                }
                Kind::ConstructorType => {
                    return Some(factory.update_constructor_type_node(
                        node,
                        node.modifiers(),
                        node.type_parameter_list(),
                        node.parameter_list(),
                        Some(new_type),
                    ));
                }
                _ => {}
            }
        }
        if ast::is_computed_property_name(node)
            && ast::is_entity_name_expression(node.as_computed_property_name().expression)
        {
            let (introduces_error, result, _) = self.track_existing_entity_name(
                c,
                node.as_computed_property_name().expression,
                None,
            );
            if !introduces_error {
                return Some(factory.update_computed_property_name(node, result));
            }
            // !!! TODO: rewriting computed names based on evaluator/typecheck results?
            // strada's behavior seems hard to justify vs marking an error and moving on
            bound.mark_error(None);
            return self.visit_each_child(c, v, node);
        }
        if ast::is_type_predicate_node(node) {
            let tp = node.as_type_predicate_node();

            let parameter_name = if ast::is_identifier(tp.parameter_name) {
                let (introduces_error, result, _) =
                    self.track_existing_entity_name(c, tp.parameter_name, None);
                // Should not usually happen the only case is when a type predicate comes from a JSDoc type annotation with it's own parameter symbol definition.
                // /** @type {(v: unknown) => v is undefined} */
                // const isUndef = v => v === undefined;
                if introduces_error {
                    bound.mark_error(None);
                }
                result
            } else {
                tp.parameter_name.clone_node(factory)
            };
            let asserts_modifier = self.visit_node(c, v, tp.asserts_modifier);
            let type_ = self.visit_node(c, v, tp.type_);
            return Some(factory.update_type_predicate_node(
                node,
                asserts_modifier,
                parameter_name,
                type_,
            ));
        }
        if ast::is_conditional_type_node(node) {
            let ct = node.as_conditional_type_node();
            let check_type = self.visit_node(c, v, Some(ct.check_type)).unwrap();
            let infer_type_parameters = c.get_infer_type_parameters(node);
            let mut dispose =
                b.enter_new_scope(c, Some(node), &[], &infer_type_parameters, None, None);
            let extends_type = self.visit_node(c, v, Some(ct.extends_type)).unwrap();
            let true_type = self.visit_node(c, v, Some(ct.true_type)).unwrap();
            dispose(c);
            let false_type = self.visit_node(c, v, Some(ct.false_type)).unwrap();
            return Some(factory.update_conditional_type_node(
                node,
                check_type,
                extends_type,
                true_type,
                false_type,
            ));
        }

        // style applications
        if ast::is_tuple_type_node(node)
            || (!b.ctx().flags.get().intersects(Flags::MultilineObjectLiterals)
                && ast::is_type_literal_node(node))
            || ast::is_mapped_type_node(node)
        {
            // make tuples/types/mappedtypes single line
            let mut res = self.visit_each_child(c, v, node).unwrap();
            if res == node {
                res = res.clone_node(factory);
                res = self.set_text_range(c, res, node);
            }
            b.e.add_emit_flags(res, EmitFlags::SingleLine);
            return Some(res);
        }

        if ast::is_string_literal_like(node) {
            // Preserve the original characters of the literal (e.g. emojis) in declaration emit
            // rather than escaping them as ASCII Unicode escapes. Mirrors TypeScript's behavior
            // for synthesized string literal types in the node builder (checker.ts:6853).
            let cl = node.clone_node(&b.f);
            if ast::is_string_literal(node)
                && b.ctx().flags.get().intersects(Flags::UseSingleQuotesForStringLiteralType)
                && !node.as_string_literal().token_flags().intersects(TokenFlags::SingleQuote)
            {
                // set single quote on string literals
                let token_flags = &cl.as_string_literal().literal_like_node_base.token_flags;
                token_flags.set(token_flags.get() ^ TokenFlags::SingleQuote);
            }
            b.e.add_emit_flags(cl, EmitFlags::NoAsciiEscaping);
            return Some(cl);
        }

        self.visit_each_child(c, v, node)
    }

    // The visit callback of getExistingNodeTreeVisitor.
    fn visit(self, c: &mut Checker, v: &mut NodeVisitor, node: P<Node>) -> Option<P<Node>> {
        let b = self.b;
        let bound = self.bound;
        // If there was an error in a sibling node bail early, the result will be discarded anyway
        if bound.had_error.get() {
            return Some(node);
        }
        let recover_ = bound.start_recovery_scope();
        let introduces_new_scope = ast::is_function_like(node) || ast::is_mapped_type_node(node);
        let mut exit: Option<Box<dyn FnMut(&mut Checker)>> = None;
        if introduces_new_scope {
            let mut params: &'static [P<Symbol>] = &[];
            let mut type_params: Vec<P<Type>> = Vec::new();
            if ast::is_function_like(node) {
                let sig = c.get_signature_from_declaration(node);
                params = sig.parameters.get();
                type_params = sig.type_parameters().to_vec();
            } else if ast::is_conditional_type_node(node) {
                // !!! TODO: impossible in combination with the scope start check???
                type_params = c.get_infer_type_parameters(node);
            } else if ast::is_mapped_type_node(node) {
                let symbol =
                    c.get_symbol_of_declaration(node.as_mapped_type_node().type_parameter).unwrap();
                type_params = vec![c.get_declared_type_of_type_parameter(symbol)];
            }
            exit = Some(b.enter_new_scope(c, Some(node), params, &type_params, None, None));
        }
        let mut result = self.visit_existing_node_tree_symbols_worker(c, v, node);
        if let Some(mut exit) = exit {
            exit(c);
        }

        if result == Some(node) && !ast::node_is_synthesized(node) {
            result = b.f.deep_clone_node(Some(node)); // always clone a new node
        }

        // We want to clone the subtree, so when we mark it up with __pos and __end in quickfixes,
        //  we don't get odd behavior because of reused nodes. We also need to clone to _remove_
        //  the position information if the node comes from a different file than the one the node builder
        //  is set to build for (even though we are reusing the node structure, the position information
        //  would make the printer print invalid spans for literals and identifiers, and the formatter would
        //  choke on the mismatched positonal spans between a parent and an injected child from another file).
        result = result.map(|r| b.set_text_range(c, r, Some(node)));

        if bound.had_error.get() {
            if ast::is_type_node(node) && !ast::is_type_predicate_node(node) {
                bound.end_recovery_scope(recover_);
                // TODO: this fallback matches strada behavior, but it lacks any verification that the type from `node` actually matches
                // the type we'd expect at this traversal position within the parent type.
                let t = b.get_type_from_type_node(c, node, false);
                return b.type_to_type_node(c, t);
            }
            return Some(b.set_text_range(c, node.clone_node(&b.f), Some(node)));
        }

        result
    }
}
