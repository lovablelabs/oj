use crate::*;
use rustc_hash::FxHashMap;
use tsrs_ast as ast;
use tsrs_ast::*;

// Non-function declarations in nodebuilder.go (hand-ported by printer-foundation in printer_types.rs / nodebuilder_types.rs):
//   type NodeBuilder (nodebuilder.go:10)
//   type VerbosityContext (nodebuilder.go:21)

impl NodeBuilder {
    // nodebuilder.go:29
    pub(crate) fn emit_context(&self) -> P<EmitContext> {
        self.impl_.e
    }

    // nodebuilder.go:33
    fn enter_context(
        &self,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) {
        let mut verbosity_level = None;
        let mut max_truncation_length = 0;
        if let Some(verbosity) = self.verbosity.get() {
            verbosity_level = Some(verbosity.level.get());
            max_truncation_length = verbosity.max_truncation_length.get();
        }
        self.ctx_stack.borrow_mut().push(self.impl_.ctx.get());
        // Per-call state, dead after `exit_context`: in the scratch region during emit (notes/mem-emit-regions.md).
        let ctx = P::new_scratch(NodeBuilderContext {
            tracker: Cell::new(tracker),
            flags: Cell::new(flags),
            internal_flags: Cell::new(internal_flags),
            max_expansion_depth: Cell::new(verbosity_level),
            max_truncation_length: Cell::new(max_truncation_length),
            enclosing_declaration: Cell::new(enclosing_declaration),
            enclosing_file: Cell::new(ast::get_source_file_of_node(enclosing_declaration)),
            ..NodeBuilderContext::new(self.host)
        });
        self.impl_.ctx.set(Some(ctx));
        let tracker = new_symbol_tracker_impl(ctx, tracker);
        ctx.tracker.set(Some(tracker.get()));
    }

    // propagateVerbosityOut copies expansion signals from the context to the VerbosityContext output.
    // nodebuilder.go:62
    fn propagate_verbosity_out(&self) {
        if let Some(verbosity) = self.verbosity.get() {
            // Only set to true, never clear — multiple calls share the same VerbosityContext
            if self.impl_.ctx().can_increase_expansion_depth.get() {
                verbosity.can_increase_verbosity.set(true);
            }
            if self.impl_.ctx().expansion_truncated.get() {
                verbosity.truncated.set(true);
            }
        }
    }

    // nodebuilder.go:74
    fn pop_context(&self) {
        let mut ctx_stack = self.ctx_stack.borrow_mut();
        match ctx_stack.pop() {
            None => self.impl_.ctx.set(None),
            Some(ctx) => self.impl_.ctx.set(ctx),
        }
    }

    // nodebuilder.go:84
    fn exit_context(&self, result: Option<P<Node>>) -> Option<P<Node>> {
        self.propagate_verbosity_out();
        self.exit_context_check();
        let encountered_error = self.impl_.ctx().encountered_error.get();
        self.pop_context();
        if encountered_error {
            return None;
        }
        result
    }

    // nodebuilder.go:94
    fn exit_context_slice(&self, result: Vec<P<Node>>) -> Vec<P<Node>> {
        self.propagate_verbosity_out();
        self.exit_context_check();
        let encountered_error = self.impl_.ctx().encountered_error.get();
        self.pop_context();
        if encountered_error {
            return Vec::new();
        }
        result
    }

    // nodebuilder.go:104
    fn exit_context_check(&self) {
        let ctx = self.impl_.ctx();
        if ctx.truncating.get() && ctx.flags.get().intersects(Flags::NoTruncation) {
            ctx.tracker.get().unwrap().report_truncation_error();
        }
    }

    // IndexInfoToIndexSignatureDeclaration implements NodeBuilderInterface.
    // nodebuilder.go:111
    pub fn index_info_to_index_signature_declaration(
        &self,
        c: &mut Checker,
        info: P<IndexInfo>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result = self.impl_.index_info_to_index_signature_declaration_helper(c, info, None);
        self.exit_context(Some(result))
    }

    // SerializeReturnTypeForSignature implements NodeBuilderInterface.
    // nodebuilder.go:117
    pub(crate) fn serialize_return_type_for_signature(
        &self,
        c: &mut Checker,
        signature_declaration: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let signature = c.get_signature_from_declaration(signature_declaration);
        let (_, mut cleanup) = self.impl_.enter_signature_scope(c, signature);
        let result = self.impl_.serialize_return_type_for_signature(c, signature, true);
        cleanup(c);
        self.exit_context(result)
    }

    // nodebuilder.go:126
    pub(crate) fn serialize_type_parameters_for_signature(
        &self,
        c: &mut Checker,
        signature_declaration: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Vec<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let symbol = c.get_symbol_of_declaration(signature_declaration);
        let type_params = self.symbol_to_type_parameter_declarations(
            c,
            symbol.unwrap(),
            enclosing_declaration,
            flags,
            internal_flags,
            tracker,
        );
        self.exit_context_slice(type_params)
    }

    // SerializeTypeForDeclaration implements NodeBuilderInterface.
    // nodebuilder.go:134
    pub(crate) fn serialize_type_for_declaration(
        &self,
        c: &mut Checker,
        declaration: P<Node>,
        symbol: Option<P<Symbol>>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result =
            self.impl_.serialize_type_for_declaration(c, Some(declaration), None, symbol, true);
        self.exit_context(Some(result))
    }

    // SerializeTypeForExpression implements NodeBuilderInterface.
    // nodebuilder.go:140
    pub(crate) fn serialize_type_for_expression(
        &self,
        c: &mut Checker,
        expr: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result = self.impl_.serialize_type_for_expression(c, expr);
        self.exit_context(result)
    }

    // SignatureToSignatureDeclaration implements NodeBuilderInterface.
    // nodebuilder.go:146
    pub fn signature_to_signature_declaration(
        &self,
        c: &mut Checker,
        signature: P<Signature>,
        kind: Kind,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result = self.impl_.signature_to_signature_declaration_helper(c, signature, kind, None);
        self.exit_context(Some(result))
    }

    // ExpandSymbolForHover produces declaration nodes for a symbol with verbosity level support.
    // nodebuilder.go:152
    pub(crate) fn expand_symbol_for_hover(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
    ) -> Vec<P<Node>> {
        self.enter_context(
            None,
            Flags::IgnoreErrors
                | Flags::MultilineObjectLiterals
                | Flags::UseAliasDefinedOutsideCurrentScope,
            InternalFlags::None,
            None,
        );

        // Push the declared type onto the type stack to prevent re-expansion.
        // We push a nil sentinel after the real type so that isTypeOnStack
        // (which skips the last element) still checks declaredType.
        let declared_type = c.get_declared_type_of_symbol(symbol);
        self.impl_.ctx().type_stack.borrow_mut().push(Some(declared_type));
        self.impl_.ctx().type_stack.borrow_mut().push(None);

        let nodes = self.impl_.expand_symbol_for_hover(c, symbol);

        {
            let ctx = self.impl_.ctx();
            let mut type_stack = ctx.type_stack.borrow_mut();
            let len = type_stack.len();
            type_stack.truncate(len - 2);
        }

        self.propagate_verbosity_out();

        // Simplify declarations by applying original modifiers
        let f = &self.impl_.f;
        let mut result = Vec::with_capacity(nodes.len());
        result.extend(nodes.into_iter().filter_map(|node| match node.kind() {
            Kind::ClassDeclaration => Some(simplify_class_declaration(f, node, symbol)),
            Kind::EnumDeclaration => {
                Some(simplify_modifiers(f, node, ast::is_enum_declaration, symbol))
            }
            Kind::InterfaceDeclaration => {
                if meaning.intersects(SymbolFlags::Interface) {
                    Some(simplify_modifiers(f, node, ast::is_interface_declaration, symbol))
                } else {
                    None
                }
            }
            Kind::ModuleDeclaration => {
                Some(simplify_modifiers(f, node, ast::is_module_declaration, symbol))
            }
            _ => None,
        }));

        self.exit_context_slice(result)
    }
}

// nodebuilder.go:188
pub fn simplify_class_declaration(
    f: &NodeFactory,
    class_decl: P<Node>,
    symbol: P<Symbol>,
) -> P<Node> {
    let mut class_decl = class_decl;
    let class_declarations: Vec<P<Node>> =
        symbol.declarations().iter().copied().filter(|&d| ast::is_class_like(d)).collect();
    let original_class_decl =
        if class_declarations.is_empty() { class_decl } else { class_declarations[0] };
    let modifiers =
        original_class_decl.modifier_flags() & !(ModifierFlags::Export | ModifierFlags::Ambient);
    let is_anonymous = ast::is_class_expression(original_class_decl);
    if is_anonymous {
        let cd = class_decl.as_class_declaration();
        class_decl = f.update_class_declaration(
            class_decl,
            class_decl.modifiers(),
            None,
            cd.type_parameters(),
            cd.heritage_clauses(),
            cd.members(),
        );
    }
    replace_modifiers(
        f,
        class_decl,
        Some(f.new_modifier_list(create_modifiers_from_modifier_flags(modifiers, |k| {
            f.new_modifier(k)
        }))),
    )
}

// nodebuilder.go:212
pub fn simplify_modifiers(
    f: &NodeFactory,
    new_decl: P<Node>,
    mut is_decl_kind: impl FnMut(P<Node>) -> bool,
    symbol: P<Symbol>,
) -> P<Node> {
    let decls: Vec<P<Node>> =
        symbol.declarations().iter().copied().filter(|&d| is_decl_kind(d)).collect();
    let decl_with_modifiers = if decls.is_empty() { new_decl } else { decls[0] };
    let modifiers =
        decl_with_modifiers.modifier_flags() & !(ModifierFlags::Export | ModifierFlags::Ambient);
    replace_modifiers(
        f,
        new_decl,
        Some(f.new_modifier_list(create_modifiers_from_modifier_flags(modifiers, |k| {
            f.new_modifier(k)
        }))),
    )
}

impl NodeBuilder {
    // SymbolToEntityName implements NodeBuilderInterface.
    // nodebuilder.go:225
    pub(crate) fn symbol_to_entity_name(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result = self.impl_.symbol_to_name(c, symbol, meaning, false);
        self.exit_context(Some(result))
    }

    // SymbolToExpression implements NodeBuilderInterface.
    // nodebuilder.go:231
    pub(crate) fn symbol_to_expression(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result = self.impl_.symbol_to_expression(c, symbol, meaning);
        self.exit_context(Some(result))
    }

    // SymbolToNode implements NodeBuilderInterface.
    // nodebuilder.go:237
    pub(crate) fn symbol_to_node(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result = self.impl_.symbol_to_node(c, symbol, meaning);
        self.exit_context(Some(result))
    }

    // SymbolToParameterDeclaration implements NodeBuilderInterface.
    // nodebuilder.go:243
    pub fn symbol_to_parameter_declaration(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        // Go has a value receiver here (the copy's ctxStack is pushed/popped); the net effect on the shared impl ctx is
        // the same as with a pointer receiver.
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result = self.impl_.symbol_to_parameter_declaration(c, symbol, false);
        self.exit_context(Some(result))
    }

    // SymbolToTypeParameterDeclarations implements NodeBuilderInterface.
    // nodebuilder.go:249
    fn symbol_to_type_parameter_declarations(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Vec<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result = self.impl_.symbol_to_type_parameter_declarations(c, symbol);
        self.exit_context_slice(result)
    }

    // TypeParameterToDeclaration implements NodeBuilderInterface.
    // nodebuilder.go:255
    pub fn type_parameter_to_declaration(
        &self,
        c: &mut Checker,
        parameter: P<Type>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result = self.impl_.type_parameter_to_declaration(c, parameter);
        self.exit_context(Some(result))
    }

    // TypePredicateToTypePredicateNode implements NodeBuilderInterface.
    // nodebuilder.go:261
    pub(crate) fn type_predicate_to_type_predicate_node(
        &self,
        c: &mut Checker,
        predicate: P<TypePredicate>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result = self.impl_.type_predicate_to_type_predicate_node(c, predicate);
        self.exit_context(Some(result))
    }

    // TypeToTypeNode implements NodeBuilderInterface.
    // nodebuilder.go:267
    pub fn type_to_type_node(
        &self,
        c: &mut Checker,
        typ: P<Type>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result = self.impl_.type_to_type_node(c, Some(typ));
        self.exit_context(result)
    }

    // nodebuilder.go:272
    pub(crate) fn try_js_type_node_to_type_node(
        &self,
        c: &mut Checker,
        node: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: Option<&'static dyn SymbolTracker>,
    ) -> Option<P<Node>> {
        self.enter_context(enclosing_declaration, flags, internal_flags, tracker);
        let result = self.impl_.try_js_type_node_to_type_node(c, node);
        self.exit_context(result)
    }
}

// nodebuilder.go:279
pub fn new_node_builder(ch: &Checker, e: P<EmitContext>) -> P<NodeBuilder> {
    new_node_builder_ex(ch, e, None /*idToSymbol*/)
}

// nodebuilder.go:283
#[expect(
    clippy::implicit_hasher,
    reason = "The API deliberately accepts only FxHashMap, as required by the workspace hashing policy."
)]
pub fn new_node_builder_ex(
    ch: &Checker,
    e: P<EmitContext>,
    id_to_symbol: Option<P<RefCell<FxHashMap<P<Node>, P<Symbol>>>>>,
) -> P<NodeBuilder> {
    let impl_ = new_node_builder_impl(ch, e, id_to_symbol);
    P::new_in(
        e.factory.is_scratch(),
        NodeBuilder {
            impl_,
            ctx_stack: RefCell::new(Vec::with_capacity(1)),
            host: ch.program.as_module_specifier_generation_host(),
            verbosity: Cell::new(None),
        },
    )
}

impl Checker {
    // nodebuilder.go:288
    pub(crate) fn get_node_builder(&mut self) -> (P<NodeBuilder>, Box<dyn FnMut(&mut Checker)>) {
        let release_nodes: Box<dyn FnMut(&mut Checker)> = Box::new(|c: &mut Checker| {
            c.type_to_string_nodebuilder.unwrap().emit_context().factory.release_arenas(); // Allow any allocated nodes to be freed if they're no longer in a cache
        });
        if let Some(node_builder) = self.type_to_string_nodebuilder {
            return (node_builder, release_nodes);
        }
        let node_builder = self.get_node_builder_ex(None /*idToSymbol*/);
        self.type_to_string_nodebuilder = Some(node_builder);
        (node_builder, release_nodes)
    }

    // nodebuilder.go:299
    pub(crate) fn get_node_builder_ex(
        &self,
        id_to_symbol: Option<P<RefCell<FxHashMap<P<Node>, P<Symbol>>>>>,
    ) -> P<NodeBuilder> {
        new_node_builder_ex(self, new_emit_context(), id_to_symbol)
    }
}
