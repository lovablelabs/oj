use crate::*;
use tsrs_ast as ast;

impl Checker {
    // exports.go:8
    pub fn get_string_type(&self) -> P<Type> {
        self.string_type
    }

    // exports.go:12
    pub fn get_number_type(&self) -> P<Type> {
        self.number_type
    }

    // exports.go:16
    pub fn get_boolean_type(&self) -> P<Type> {
        self.boolean_type
    }

    // exports.go:20
    pub fn get_void_type(&self) -> P<Type> {
        self.void_type
    }

    // exports.go:24
    pub fn get_undefined_type(&self) -> P<Type> {
        self.undefined_type
    }

    // exports.go:28
    pub fn get_null_type(&self) -> P<Type> {
        self.null_type
    }

    // exports.go:32
    pub fn get_any_type(&self) -> P<Type> {
        self.any_type
    }

    // exports.go:40
    pub fn get_never_type(&self) -> P<Type> {
        self.never_type
    }

    // exports.go:44
    pub fn get_unknown_type(&self) -> P<Type> {
        self.unknown_type
    }

    // exports.go:48
    pub fn get_big_int_type(&self) -> P<Type> {
        self.bigint_type
    }

    // exports.go:52
    pub fn get_es_symbol_type(&self) -> P<Type> {
        self.es_symbol_type
    }

    // exports.go:56
    pub fn get_non_primitive_type(&self) -> P<Type> {
        self.non_primitive_type
    }

    // exports.go:60
    pub fn get_base_type_of_literal_type_exported(&mut self, t: P<Type>) -> P<Type> {
        self.get_base_type_of_literal_type(t)
    }

    // exports.go:64
    pub fn get_unknown_symbol(&self) -> P<Symbol> {
        self.unknown_symbol
    }

    // exports.go:68
    pub fn get_undefined_symbol(&self) -> P<Symbol> {
        self.undefined_symbol
    }

    // exports.go:72
    pub fn get_arguments_symbol(&self) -> P<Symbol> {
        self.arguments_symbol
    }

    // exports.go:76
    pub fn get_unknown_signature(&self) -> P<Signature> {
        self.unknown_signature
    }

    // exports.go:80
    pub fn get_union_type_exported(&mut self, types: &[P<Type>]) -> P<Type> {
        self.get_union_type(types)
    }

    // exports.go:84
    pub fn get_name_type_of_symbol(&self, symbol: P<Symbol>) -> Option<P<Type>> {
        if let Some(links) = self.value_symbol_links.try_get(symbol) {
            return links.name_type();
        }
        None
    }
}

// exports.go:91
pub fn is_type_usable_as_property_name_exported(t: P<Type>) -> bool {
    is_type_usable_as_property_name(t)
}

// exports.go:95
pub fn get_property_name_from_type_exported(t: P<Type>) -> std::borrow::Cow<'static, str> {
    get_property_name_from_type(t)
}

impl Checker {
    // exports.go:99
    pub fn get_global_symbol_exported(
        &mut self,
        name: &str,
        meaning: SymbolFlags,
        diagnostic: Option<&'static Message>,
    ) -> Option<P<Symbol>> {
        self.get_global_symbol(name, meaning, diagnostic)
    }

    // exports.go:103
    pub fn get_merged_symbol_exported(&mut self, symbol: P<Symbol>) -> P<Symbol> {
        self.get_merged_symbol(symbol)
    }

    // exports.go:107
    pub fn get_symbol_of_node_exported(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        self.get_symbol_of_node(node)
    }

    // exports.go:111
    pub fn get_symbol_of_declaration_exported(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        self.get_symbol_of_declaration(node)
    }

    // exports.go:115
    pub fn get_parent_of_symbol_exported(&mut self, symbol: P<Symbol>) -> Option<P<Symbol>> {
        self.get_parent_of_symbol(symbol)
    }

    // exports.go:119
    pub fn try_find_ambient_module_exported(&mut self, module_name: &str) -> Option<P<Symbol>> {
        self.try_find_ambient_module(module_name, true /* withAugmentations */)
    }

    // exports.go:123
    pub fn get_immediate_aliased_symbol_exported(
        &mut self,
        symbol: P<Symbol>,
    ) -> Option<P<Symbol>> {
        self.get_immediate_aliased_symbol(symbol)
    }

    // exports.go:127
    pub fn get_target_symbol_exported(&mut self, symbol: P<Symbol>) -> Option<P<Symbol>> {
        self.get_target_symbol(symbol)
    }

    // exports.go:135
    pub fn resolve_external_module_name_exported(
        &mut self,
        module_specifier: P<Node>,
        import_attributes_type: Option<P<Type>>,
    ) -> Option<P<Symbol>> {
        self.resolve_external_module_name(
            module_specifier,
            module_specifier,
            true, /*ignoreErrors*/
            import_attributes_type,
        )
    }

    // exports.go:143
    pub fn get_type_from_type_node_exported(&mut self, node: P<Node>) -> P<Type> {
        self.get_type_from_type_node(node)
    }

    // exports.go:147
    pub fn is_array_like_type_exported(&mut self, t: P<Type>) -> bool {
        self.is_array_like_type(t)
    }

    // exports.go:151
    pub fn get_properties_of_type_exported(&mut self, t: P<Type>) -> &'static [P<Symbol>] {
        self.get_properties_of_type(t)
    }

    // exports.go:155
    pub fn get_property_of_type_exported(&mut self, t: P<Type>, name: &str) -> Option<P<Symbol>> {
        self.get_property_of_type(t, name)
    }

    // exports.go:159
    pub fn type_has_call_or_construct_signatures_exported(&mut self, t: P<Type>) -> bool {
        self.type_has_call_or_construct_signatures(t)
    }

    // exports.go:171
    // Checks if a property can be accessed in a location.
    // The location is given by the `node` parameter.
    // The node does not need to be a property access.
    // @param node location where to check property accessibility
    // @param isSuper whether to consider this a `super` property access, e.g. `super.foo`.
    // @param isWrite whether this is a write access, e.g. `++foo.x`.
    // @param containingType type where the property comes from.
    // @param property property symbol.
    pub fn is_property_accessible_exported(
        &mut self,
        node: P<Node>,
        is_super: bool,
        is_write: bool,
        containing_type: P<Type>,
        property: P<Symbol>,
    ) -> bool {
        self.is_property_accessible(node, is_super, is_write, containing_type, property)
    }

    // exports.go:175
    pub fn get_type_of_property_of_contextual_type_exported(
        &mut self,
        t: P<Type>,
        name: &str,
    ) -> Option<P<Type>> {
        self.get_type_of_property_of_contextual_type(t, name)
    }
}

// exports.go:179
pub fn get_declaration_modifier_flags_from_symbol_exported(s: P<Symbol>) -> ModifierFlags {
    get_declaration_modifier_flags_from_symbol(s)
}

impl Checker {
    // exports.go:183
    pub fn was_canceled(&self) -> bool {
        self.was_canceled
    }

    // exports.go:187
    pub fn get_signatures_of_type_exported(
        &mut self,
        t: P<Type>,
        kind: SignatureKind,
    ) -> &'static [P<Signature>] {
        self.get_signatures_of_type(t, kind)
    }

    // exports.go:191
    pub fn get_declared_type_of_symbol_exported(&mut self, symbol: P<Symbol>) -> P<Type> {
        self.get_declared_type_of_symbol(symbol)
    }

    // exports.go:195
    pub fn get_type_of_symbol_exported(&mut self, symbol: P<Symbol>) -> P<Type> {
        self.get_type_of_symbol(symbol)
    }

    // exports.go:199
    pub fn get_non_missing_type_of_symbol_exported(&mut self, symbol: P<Symbol>) -> P<Type> {
        self.get_non_missing_type_of_symbol(symbol)
    }

    // exports.go:203
    pub fn get_constraint_of_type_parameter_exported(
        &mut self,
        type_parameter: P<Type>,
    ) -> Option<P<Type>> {
        self.get_constraint_of_type_parameter(type_parameter)
    }

    // exports.go:207
    pub fn get_true_type_of_conditional_type(&mut self, t: P<Type>) -> P<Type> {
        self.get_true_type_from_conditional_type(t)
    }

    // exports.go:211
    pub fn get_false_type_of_conditional_type(&mut self, t: P<Type>) -> P<Type> {
        self.get_false_type_from_conditional_type(t)
    }

    // exports.go:215
    pub fn get_default_from_type_parameter_exported(
        &mut self,
        type_parameter: P<Type>,
    ) -> Option<P<Type>> {
        self.get_default_from_type_parameter(type_parameter)
    }

    // exports.go:223
    pub fn get_base_constraint_of_type_exported(&mut self, t: P<Type>) -> Option<P<Type>> {
        self.get_base_constraint_of_type(t)
    }

    // exports.go:227
    pub fn get_type_predicate_of_signature_exported(
        &mut self,
        sig: P<Signature>,
    ) -> Option<P<TypePredicate>> {
        self.get_type_predicate_of_signature(sig)
    }
}

// exports.go:231
pub fn is_tuple_type_exported(t: P<Type>) -> bool {
    is_tuple_type(t)
}

// exports.go:235
pub fn is_tuple_type_target(t: P<Type>) -> bool {
    is_tuple_type(t) && t.target() == Some(t)
}

impl Checker {
    // exports.go:239
    pub fn is_array_type_exported(&self, t: P<Type>) -> bool {
        self.is_array_type(t)
    }

    // exports.go:243
    pub fn is_readonly_symbol_exported(&mut self, symbol: P<Symbol>) -> bool {
        self.is_readonly_symbol(symbol)
    }

    // exports.go:247
    pub fn get_return_type_of_signature_exported(&mut self, sig: P<Signature>) -> P<Type> {
        self.get_return_type_of_signature(sig)
    }

    // exports.go:251
    pub fn has_effective_rest_parameter_exported(&mut self, signature: P<Signature>) -> bool {
        self.has_effective_rest_parameter(signature)
    }

    // exports.go:255
    pub fn get_local_type_parameters_of_class_or_interface_or_type_alias_exported(
        &mut self,
        symbol: P<Symbol>,
    ) -> Vec<P<Type>> {
        self.get_local_type_parameters_of_class_or_interface_or_type_alias(symbol)
    }

    // exports.go:259
    pub fn get_contextual_type_for_object_literal_element_exported(
        &mut self,
        element: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        self.get_contextual_type_for_object_literal_element(element, context_flags)
    }

    // exports.go:263
    pub fn type_predicate_to_string_exported(&mut self, t: P<TypePredicate>) -> String {
        self.type_predicate_to_string(t)
    }

    // exports.go:267
    pub fn get_expanded_parameters_exported(
        &mut self,
        signature: P<Signature>,
        skip_union_expanding: bool,
    ) -> Vec<Vec<P<Symbol>>> {
        self.get_expanded_parameters(signature, skip_union_expanding)
    }

    // exports.go:271
    pub fn get_resolved_signature_exported(&mut self, node: P<Node>) -> P<Signature> {
        self.get_resolved_signature(node, None, CheckMode::Normal)
    }

    // exports.go:276
    // Return the type of the given property in the given type, or nil if no such property exists
    pub fn get_type_of_property_of_type_exported(
        &mut self,
        t: P<Type>,
        name: &str,
    ) -> Option<P<Type>> {
        self.get_type_of_property_of_type(t, name)
    }

    // exports.go:280
    pub fn get_contextual_type_for_argument_at_index_exported(
        &mut self,
        node: P<Node>,
        arg_index: usize,
    ) -> P<Type> {
        self.get_contextual_type_for_argument_at_index(node, arg_index)
    }

    // exports.go:284
    pub fn get_awaited_type_exported(&mut self, t: P<Type>) -> Option<P<Type>> {
        self.get_awaited_type(t)
    }

    // exports.go:288
    pub fn get_index_signatures_at_location_exported(&mut self, node: P<Node>) -> Vec<P<Node>> {
        self.get_index_signatures_at_location(node)
    }

    // exports.go:292
    pub fn get_resolved_symbol_exported(&mut self, node: P<Node>) -> P<Symbol> {
        self.get_resolved_symbol(node)
    }

    // exports.go:296
    pub fn get_jsx_namespace_exported(&mut self, location: P<Node>) -> String {
        self.get_jsx_namespace(Some(location))
    }

    // exports.go:300
    pub(crate) fn get_jsx_fragment_factory(&mut self, location: P<Node>) -> String {
        let entity = self.get_jsx_fragment_factory_entity(Some(location));
        if let Some(entity) = entity {
            return ast::get_first_identifier(entity).text().to_string();
        }
        String::new()
    }

    // exports.go:308
    // Go's location is nullable (the Node API resolves names without a location); existing callers pass a node.
    pub fn resolve_name_exported(
        &mut self,
        name: &str,
        location: impl Into<Option<P<Node>>>,
        meaning: SymbolFlags,
        exclude_globals: bool,
    ) -> Option<P<Symbol>> {
        self.resolve_name(location.into(), name, meaning, None, true, exclude_globals)
    }

    // exports.go:312
    pub fn get_symbol_flags_exported(&mut self, symbol: P<Symbol>) -> SymbolFlags {
        self.get_symbol_flags(symbol)
    }

    // exports.go:316
    pub fn get_base_types_exported(&mut self, t: P<Type>) -> &'static [P<Type>] {
        self.get_base_types(t)
    }

    // exports.go:320
    pub fn get_apparent_type_exported(&mut self, t: P<Type>) -> P<Type> {
        self.get_apparent_type(t)
    }

    // exports.go:324
    pub fn get_reduced_type_exported(&mut self, t: P<Type>) -> P<Type> {
        self.get_reduced_type(t)
    }

    // exports.go:330
    // GetFullyQualifiedName returns the fully qualified name of a symbol, walking up
    // its parent chain (e.g. `"/path/to/module".Namespace.Name`).
    pub fn get_fully_qualified_name_exported(&mut self, symbol: P<Symbol>) -> String {
        self.get_fully_qualified_name(symbol, None /*containingLocation*/)
    }

    // exports.go:334
    pub fn get_base_constructor_type_of_class_exported(&mut self, t: P<Type>) -> P<Type> {
        self.get_base_constructor_type_of_class(t)
    }

    // exports.go:338
    pub fn get_member_override_modifier_status_exported(
        &mut self,
        node: P<Node>,
        member: P<Node>,
        member_symbol: Option<P<Symbol>>,
    ) -> MemberOverrideStatus {
        self.get_member_override_modifier_status(node, member, member_symbol)
    }

    // exports.go:342
    pub fn get_rest_type_of_signature_exported(&mut self, sig: P<Signature>) -> P<Type> {
        self.get_rest_type_of_signature(sig)
    }

    // exports.go:346
    pub fn get_type_arguments_exported(&mut self, t: P<Type>) -> &'static [P<Type>] {
        self.get_type_arguments(t)
    }

    // exports.go:350
    pub fn get_index_info_of_type_exported(
        &mut self,
        t: P<Type>,
        key_type: P<Type>,
    ) -> Option<P<IndexInfo>> {
        self.get_index_info_of_type(t, key_type)
    }

    // exports.go:358
    pub fn get_index_infos_of_type_exported(&mut self, t: P<Type>) -> &'static [P<IndexInfo>] {
        self.get_index_infos_of_type(t)
    }

    // exports.go:362
    pub fn is_context_sensitive_exported(&mut self, node: P<Node>) -> bool {
        self.is_context_sensitive(node)
    }

    // exports.go:366
    pub fn fill_missing_type_arguments_exported(
        &mut self,
        type_arguments: &[P<Type>],
        type_parameters: &[P<Type>],
        min_type_argument_count: usize,
        is_java_script_implicit_any: bool,
    ) -> Vec<P<Type>> {
        self.fill_missing_type_arguments(
            type_arguments,
            type_parameters,
            min_type_argument_count,
            is_java_script_implicit_any,
        )
        .into_owned()
    }

    // exports.go:374
    pub fn get_widened_literal_type_exported(&mut self, t: P<Type>) -> P<Type> {
        self.get_widened_literal_type(t)
    }

    // exports.go:378
    pub fn is_type_assignable_to_exported(&mut self, source: P<Type>, target: P<Type>) -> bool {
        self.is_type_assignable_to(source, target)
    }

    // exports.go:382
    pub fn get_union_type_ex_exported(
        &mut self,
        types: &[P<Type>],
        union_reduction: UnionReduction,
    ) -> P<Type> {
        self.get_union_type_ex(types, union_reduction, AliasArg::None, None)
    }

    // exports.go:386
    pub fn requires_adding_implicit_undefined(&mut self, node: P<Node>) -> bool {
        let enclosing_declaration = match ast::find_ancestor(node, ast::is_declaration) {
            Some(d) => d,
            None => ast::get_source_file_of_node(node).unwrap().as_node(),
        };
        let symbol = node.symbol();
        if symbol.is_none() {
            return false;
        }
        let r = self.get_emit_resolver();
        r.requires_adding_implicit_undefined_exported(
            self,
            node,
            symbol,
            Some(enclosing_declaration),
        )
    }

    // exports.go:398
    pub fn remove_missing_or_undefined_type_exported(&mut self, t: P<Type>) -> P<Type> {
        self.remove_missing_or_undefined_type(t)
    }

    // exports.go:402
    pub fn get_widened_type_exported(&mut self, t: P<Type>) -> P<Type> {
        self.get_widened_type(t)
    }

    // exports.go:406
    pub fn compare_symbols_exported(
        &mut self,
        s1: Option<P<Symbol>>,
        s2: Option<P<Symbol>>,
    ) -> std::cmp::Ordering {
        self.compare_symbols(s1, s2)
    }

    // Not in Go: Go's language service reads `objType.Properties()` of types whose members it has just resolved
    // (semantictokens.go:450). With lazy member tables an instantiated class or interface reference answers those
    // queries from its table and its resolved property list stays empty, so ask through the table.
    pub fn has_properties_of_structured_type_exported(&mut self, t: P<Type>) -> bool {
        self.has_properties_of_structured_type(t)
    }
}

// exports.go:410
pub fn is_distributed_type_parameter(t: P<Type>) -> bool {
    t.flags().intersects(TypeFlags::TypeParameter) && t.as_type_parameter().is_distributed.get()
}
