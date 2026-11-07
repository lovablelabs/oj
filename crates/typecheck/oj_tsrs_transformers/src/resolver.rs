use crate::*;

/// Go `printer.EmitResolver` as the declaration transformer sees it: the checker's `*EmitResolver` behind the
/// interface.
///
/// Rust methods on `tsrs_checker::EmitResolver` take the checker (`&self, c: &mut Checker`); this handle
/// supplies it from a `CheckerSlot` that the caller of the transform lends the file's checker to
/// (`slot.lend(c, || tx.transform_source_file(file))`, see `tsrs_compiler` `get_declaration_diagnostics`).
///
/// The Go exported methods that take `checkerMu` are the methods here without a checker argument: each
/// borrows the checker with `slot.with` for the duration of the call, just as Go holds the lock. The Go
/// methods that must be called with the lock already held (`...Unsafe`, `IsSymbolAccessible`,
/// `GetPropertiesOfContainerFunction`, used by the symbol tracker from inside node-builder calls) take `c`
/// explicitly. `lock(|c| ..)` is the same borrow for code that calls a non-locking method outside a
/// node-builder call.
#[derive(Clone, Copy)]
pub struct Resolver {
    pub r: P<EmitResolver>,
    slot: P<CheckerSlot>,
}

impl Resolver {
    pub fn new(r: P<EmitResolver>, slot: P<CheckerSlot>) -> Resolver {
        Resolver { r, slot }
    }

    /// Borrows the lent checker (Go: `checkerMu.Lock()` around the call).
    pub fn lock<R>(self, f: impl FnOnce(&mut Checker) -> R) -> R {
        self.slot.with(f)
    }

    // Locking methods (Go exported methods that take `checkerMu`).

    pub fn is_declaration_visible(self, node: P<Node>) -> bool {
        self.lock(|c| self.r.is_declaration_visible_exported(c, node))
    }

    pub fn is_entity_name_visible(
        self,
        entity_name: P<Node>,
        enclosing_declaration: Option<P<Node>>,
    ) -> SymbolAccessibilityResult {
        self.lock(|c| self.r.is_entity_name_visible_exported(c, entity_name, enclosing_declaration))
    }

    pub fn is_late_bound(self, node: Option<P<Node>>) -> bool {
        self.lock(|c| self.r.is_late_bound(c, node))
    }

    pub fn is_implementation_of_overload(self, node: P<Node>) -> bool {
        self.lock(|c| self.r.is_implementation_of_overload(c, node))
    }

    pub fn is_definitely_reference_to_global_symbol_object(self, node: P<Node>) -> bool {
        self.lock(|c| self.r.is_definitely_reference_to_global_symbol_object(c, node))
    }

    pub fn precalculate_declaration_emit_visibility(self, file: P<SourceFile>) {
        self.lock(|c| self.r.precalculate_declaration_emit_visibility(c, file));
    }

    pub fn is_import_required_by_augmentation(self, decl: P<Node>) -> bool {
        self.lock(|c| self.r.is_import_required_by_augmentation(c, decl))
    }

    pub fn is_literal_const_declaration(self, node: P<Node>) -> bool {
        self.lock(|c| self.r.is_literal_const_declaration(c, node))
    }

    pub fn is_expando_function_declaration(self, node: P<Node>) -> bool {
        self.lock(|c| self.r.is_expando_function_declaration(c, node))
    }

    pub fn is_optional_parameter(self, node: P<Node>) -> bool {
        self.lock(|c| self.r.is_optional_parameter_exported(c, node))
    }

    pub fn requires_adding_implicit_undefined(
        self,
        declaration: P<Node>,
        symbol: Option<P<Symbol>>,
        enclosing_declaration: Option<P<Node>>,
    ) -> bool {
        self.lock(|c| {
            self.r.requires_adding_implicit_undefined_exported(
                c,
                declaration,
                symbol,
                enclosing_declaration,
            )
        })
    }

    pub fn get_enum_member_value(self, node: P<Node>) -> checker::evaluator::Result {
        self.lock(|c| self.r.get_enum_member_value(c, node))
    }

    pub fn get_referenced_value_declaration(self, node: P<Node>) -> Option<P<Node>> {
        self.lock(|c| self.r.get_referenced_value_declaration(c, node))
    }

    pub fn get_referenced_member_value_declaration(self, node: P<Node>) -> Option<P<Node>> {
        self.lock(|c| self.r.get_referenced_member_value_declaration(c, node))
    }

    pub fn get_element_access_expression_name(self, expression: P<Node>) -> String {
        self.lock(|c| self.r.get_element_access_expression_name(c, expression))
    }

    pub fn is_name_resolvable(self, location: Option<P<Node>>, name: &str) -> bool {
        self.lock(|c| self.r.is_name_resolvable(c, location, name))
    }

    pub fn is_this_property_assignment_declaration_redundant(self, node: Option<P<Node>>) -> bool {
        self.lock(|c| self.r.is_this_property_assignment_declaration_redundant(c, node))
    }

    pub fn get_effective_declaration_flags(
        self,
        node: P<Node>,
        flags: ModifierFlags,
    ) -> ModifierFlags {
        self.lock(|c| self.r.get_effective_declaration_flags(c, node, flags))
    }

    pub fn create_return_type_of_signature_declaration(
        self,
        signature_declaration: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: nodebuilder::Flags,
        internal_flags: nodebuilder::InternalFlags,
        tracker: &'static dyn SymbolTracker,
    ) -> Option<P<Node>> {
        self.lock(|c| {
            self.r.create_return_type_of_signature_declaration(
                c,
                signature_declaration,
                enclosing_declaration,
                flags,
                internal_flags,
                tracker,
            )
        })
    }

    pub fn create_type_parameters_of_signature_declaration(
        self,
        signature_declaration: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: nodebuilder::Flags,
        internal_flags: nodebuilder::InternalFlags,
        tracker: &'static dyn SymbolTracker,
    ) -> Option<Vec<P<Node>>> {
        self.lock(|c| {
            self.r.create_type_parameters_of_signature_declaration(
                c,
                signature_declaration,
                enclosing_declaration,
                flags,
                internal_flags,
                tracker,
            )
        })
    }

    pub fn create_type_of_declaration(
        self,
        declaration: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: nodebuilder::Flags,
        internal_flags: nodebuilder::InternalFlags,
        tracker: &'static dyn SymbolTracker,
    ) -> Option<P<Node>> {
        self.lock(|c| {
            self.r.create_type_of_declaration(
                c,
                declaration,
                enclosing_declaration,
                flags,
                internal_flags,
                tracker,
            )
        })
    }

    pub fn create_literal_const_value(
        self,
        node: P<Node>,
        tracker: &'static dyn SymbolTracker,
    ) -> Option<P<Node>> {
        self.lock(|c| self.r.create_literal_const_value(c, node, tracker))
    }

    pub fn create_type_of_expression(
        self,
        expression: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: nodebuilder::Flags,
        internal_flags: nodebuilder::InternalFlags,
        tracker: &'static dyn SymbolTracker,
    ) -> Option<P<Node>> {
        self.lock(|c| {
            self.r.create_type_of_expression(
                c,
                expression,
                enclosing_declaration,
                flags,
                internal_flags,
                tracker,
            )
        })
    }

    pub fn create_late_bound_index_signatures(
        self,
        container: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: nodebuilder::Flags,
        internal_flags: nodebuilder::InternalFlags,
        tracker: &'static dyn SymbolTracker,
    ) -> Vec<P<Node>> {
        self.lock(|c| {
            self.r.create_late_bound_index_signatures(
                c,
                container,
                enclosing_declaration,
                flags,
                internal_flags,
                tracker,
            )
        })
    }

    pub fn try_js_type_node_to_type_node(
        self,
        type_node: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: nodebuilder::Flags,
        internal_flags: nodebuilder::InternalFlags,
        tracker: &'static dyn SymbolTracker,
    ) -> Option<P<Node>> {
        self.lock(|c| {
            self.r.try_js_type_node_to_type_node(
                c,
                type_node,
                enclosing_declaration,
                flags,
                internal_flags,
                tracker,
            )
        })
    }

    // The rest of Go's `printer.EmitResolver` (used by the script transformers).

    pub(crate) fn is_referenced_alias_declaration(self, node: P<Node>) -> bool {
        self.lock(|c| self.r.is_referenced_alias_declaration(c, node))
    }

    pub(crate) fn is_value_alias_declaration(self, node: P<Node>) -> bool {
        self.lock(|c| self.r.is_value_alias_declaration(c, node))
    }

    pub(crate) fn is_top_level_value_import_equals_with_entity_name(self, node: P<Node>) -> bool {
        self.lock(|c| self.r.is_top_level_value_import_equals_with_entity_name(c, node))
    }

    pub(crate) fn mark_linked_references_recursively(self, file: P<SourceFile>) {
        self.lock(|c| self.r.mark_linked_references_recursively(c, Some(file)));
    }

    pub(crate) fn get_external_module_file_from_declaration(
        self,
        node: P<Node>,
    ) -> Option<P<SourceFile>> {
        self.lock(|c| self.r.get_external_module_file_from_declaration(c, node))
    }

    pub(crate) fn get_type_reference_serialization_kind(
        self,
        name: P<Node>,
        serial_scope: Option<P<Node>>,
    ) -> checker::TypeReferenceSerializationKind {
        self.lock(|c| self.r.get_type_reference_serialization_kind(c, Some(name), serial_scope))
    }

    pub(crate) fn get_constant_value(self, node: P<Node>) -> Option<checker::LiteralValue> {
        self.lock(|c| self.r.get_constant_value(c, node))
    }

    pub(crate) fn get_jsx_factory_entity(self, location: P<Node>) -> Option<P<Node>> {
        self.lock(|c| self.r.get_jsx_factory_entity(c, location))
    }

    pub(crate) fn get_jsx_fragment_factory_entity(self, location: P<Node>) -> Option<P<Node>> {
        self.lock(|c| self.r.get_jsx_fragment_factory_entity(c, location))
    }

    pub(crate) fn set_referenced_import_declaration(self, node: P<Node>, ref_: P<Node>) {
        self.lock(|c| self.r.set_referenced_import_declaration(c, node, ref_));
    }

    // binder.ReferenceResolver (embedded in Go's EmitResolver).

    pub(crate) fn get_referenced_export_container(
        self,
        node: P<Node>,
        prefix_locals: bool,
    ) -> Option<P<Node>> {
        self.lock(|c| self.r.get_referenced_export_container(c, node, prefix_locals))
    }

    fn get_referenced_import_declaration(self, node: P<Node>) -> Option<P<Node>> {
        self.lock(|c| self.r.get_referenced_import_declaration(c, node))
    }

    fn get_referenced_value_declarations(self, node: P<Node>) -> Vec<P<Node>> {
        self.lock(|c| self.r.get_referenced_value_declarations(c, node))
    }

    // Non-locking methods (called with the checker already borrowed, from symbol tracker callbacks).

    pub fn is_symbol_accessible(
        self,
        c: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
        should_compute_alias_to_mark_visible: bool,
    ) -> SymbolAccessibilityResult {
        self.r.is_symbol_accessible(
            c,
            Some(symbol),
            enclosing_declaration,
            meaning,
            should_compute_alias_to_mark_visible,
        )
    }

    pub fn get_referenced_value_declaration_unsafe(
        self,
        c: &mut Checker,
        node: P<Node>,
    ) -> Option<P<Node>> {
        self.r.get_referenced_value_declaration_unsafe(c, node)
    }

    pub fn is_expando_function_declaration_unsafe(self, c: &mut Checker, node: P<Node>) -> bool {
        self.r.is_expando_function_declaration_unsafe(c, node)
    }

    pub fn requires_adding_implicit_undefined_unsafe(
        self,
        c: &mut Checker,
        declaration: P<Node>,
        symbol: Option<P<Symbol>>,
        enclosing_declaration: Option<P<Node>>,
    ) -> bool {
        self.r.requires_adding_implicit_undefined_unsafe(
            c,
            declaration,
            symbol,
            enclosing_declaration,
        )
    }

    pub fn get_properties_of_container_function(
        self,
        c: &mut Checker,
        node: Option<P<Node>>,
    ) -> Vec<P<Symbol>> {
        self.r.get_properties_of_container_function(c, node)
    }
}

/// Go `binder.ReferenceResolver` as `getScriptTransformers` picks it: the emit resolver (which embeds the
/// interface), or a plain binder `ReferenceResolver` without hooks.
#[derive(Clone, Copy)]
pub enum ReferenceResolverRef {
    Emit(Resolver),
    Plain(P<tsrs_binder::ReferenceResolver<()>>),
}

impl ReferenceResolverRef {
    pub(crate) fn get_referenced_export_container(
        self,
        node: P<Node>,
        prefix_locals: bool,
    ) -> Option<P<Node>> {
        match self {
            ReferenceResolverRef::Emit(r) => r.get_referenced_export_container(node, prefix_locals),
            ReferenceResolverRef::Plain(r) => {
                r.get_referenced_export_container(&mut (), node, prefix_locals)
            }
        }
    }

    pub(crate) fn get_referenced_import_declaration(self, node: P<Node>) -> Option<P<Node>> {
        match self {
            ReferenceResolverRef::Emit(r) => r.get_referenced_import_declaration(node),
            ReferenceResolverRef::Plain(r) => r.get_referenced_import_declaration(&mut (), node),
        }
    }

    pub(crate) fn get_referenced_value_declaration(self, node: P<Node>) -> Option<P<Node>> {
        match self {
            ReferenceResolverRef::Emit(r) => r.get_referenced_value_declaration(node),
            ReferenceResolverRef::Plain(r) => r.get_referenced_value_declaration(&mut (), node),
        }
    }

    pub(crate) fn get_referenced_value_declarations(self, node: P<Node>) -> Vec<P<Node>> {
        match self {
            ReferenceResolverRef::Emit(r) => r.get_referenced_value_declarations(node),
            ReferenceResolverRef::Plain(r) => r.get_referenced_value_declarations(&mut (), node),
        }
    }
}
