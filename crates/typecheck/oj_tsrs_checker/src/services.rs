use crate::*;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;

impl Checker {
    // services.go:17
    pub fn get_symbols_in_scope_exported(
        &mut self,
        location: P<Node>,
        meaning: SymbolFlags,
    ) -> Vec<P<Symbol>> {
        self.get_symbols_in_scope(location, meaning)
    }

    // services.go:21
    fn get_symbols_in_scope(&mut self, location: P<Node>, meaning: SymbolFlags) -> Vec<P<Symbol>> {
        if location.flags().intersects(NodeFlags::InWithStatement) {
            // We cannot answer semantic questions within a with block, do not proceed any further
            return Vec::new();
        }

        let symbols = SymbolTable::new();
        let mut is_static_symbol = false;

        // Copy the given symbol into symbol tables if the symbol has the given meaning
        // and it doesn't already exists in the symbol table.
        let copy_symbol = |symbol: P<Symbol>, meaning: SymbolFlags| {
            if symbol.combined_local_and_export_symbol_flags().intersects(meaning) {
                let id = symbol.name();
                // We will copy all symbol regardless of its reserved name because
                // symbolsToArray will check whether the key is a reserved name and
                // it will not copy symbol with reserved name to the array
                if !symbols.has(id) {
                    symbols.set(id, symbol);
                }
            }
        };

        let copy_symbols = |source: Option<P<SymbolTable>>, meaning: SymbolFlags| {
            if !meaning.is_empty()
                && let Some(source) = source
            {
                for (_, symbol) in source.entries() {
                    copy_symbol(symbol, meaning);
                }
            }
        };

        let copy_locally_visible_export_symbols =
            |source: Option<P<SymbolTable>>, meaning: SymbolFlags| {
                if !meaning.is_empty()
                    && let Some(source) = source
                {
                    for (_, symbol) in source.entries() {
                        // Similar condition as in `resolveNameHelper`
                        if ast::get_declaration_of_kind(symbol, Kind::ExportSpecifier).is_none()
                            && ast::get_declaration_of_kind(symbol, Kind::NamespaceExport).is_none()
                            && symbol.name() != INTERNAL_SYMBOL_NAME_DEFAULT
                        {
                            copy_symbol(symbol, meaning);
                        }
                    }
                }
            };

        // populateSymbols
        let mut location = Some(location);
        let mut last_location: Option<P<Node>> = None;
        while let Some(loc) = location {
            if ast::is_module_declaration(loc)
                && loc.as_module_declaration().attributes.is_some()
                && last_location == loc.as_module_declaration().attributes
            {
                // Module declaration is not in scope inside its attributes.
                last_location = Some(loc);
                location = loc.parent();
                continue;
            }

            if can_have_locals(loc) && loc.locals().is_some() && !ast::is_global_source_file(loc) {
                copy_symbols(loc.locals(), meaning);
            }

            match loc.kind() {
                Kind::SourceFile | Kind::ModuleDeclaration => {
                    if loc.kind() == Kind::ModuleDeclaration
                        || ast::is_external_module(loc.as_source_file_p())
                    {
                        let exports = self.get_symbol_of_declaration(loc).unwrap().exports();
                        copy_locally_visible_export_symbols(
                            exports,
                            meaning & SymbolFlags::ModuleMember,
                        );
                    }
                }
                Kind::EnumDeclaration => {
                    let exports = self.get_symbol_of_declaration(loc).unwrap().exports();
                    copy_symbols(exports, meaning & SymbolFlags::EnumMember);
                }
                Kind::ClassExpression | Kind::ClassDeclaration | Kind::InterfaceDeclaration => {
                    if loc.kind() == Kind::ClassExpression {
                        let class_name = loc.name();
                        if class_name.is_some() {
                            copy_symbol(loc.symbol().unwrap(), meaning);
                        }
                    }
                    // this fall-through is necessary because we would like to handle
                    // type parameter inside class expression similar to how we handle it in classDeclaration and interface Declaration.

                    // If we didn't come from static member of class or interface,
                    // add the type parameters into the symbol table
                    // (type parameters of classDeclaration/classExpression and interface are in member property of the symbol.
                    // Note: that the memberFlags come from previous iteration.
                    if !is_static_symbol {
                        let symbol = self.get_symbol_of_declaration(loc).unwrap();
                        let members = self.get_members_of_symbol(symbol);
                        copy_symbols(members, meaning & SymbolFlags::Type);
                    }
                }
                Kind::FunctionExpression => {
                    let func_name = loc.name();
                    if func_name.is_some() {
                        copy_symbol(loc.symbol().unwrap(), meaning);
                    }
                }
                _ => {}
            }

            if introduces_arguments_exotic_object(loc) {
                copy_symbol(self.arguments_symbol, meaning);
            }

            is_static_symbol = ast::is_static(loc);
            last_location = Some(loc);
            location = loc.parent();
        }

        copy_symbols(Some(self.globals), meaning);

        symbols.delete(INTERNAL_SYMBOL_NAME_THIS); // Not a symbol, a keyword
        symbols_to_array(Some(symbols))
    }

    // services.go:132
    pub fn get_exports_of_module_exported(&mut self, symbol: P<Symbol>) -> Vec<P<Symbol>> {
        symbols_to_array(Some(self.get_exports_of_module(symbol)))
    }

    // services.go:165
    pub fn is_valid_property_access_exported(
        &mut self,
        node: P<Node>,
        property_name: &str,
    ) -> bool {
        self.is_valid_property_access(node, property_name)
    }

    // services.go:169
    fn is_valid_property_access(&mut self, node: P<Node>, property_name: &str) -> bool {
        match node.kind() {
            Kind::PropertyAccessExpression => {
                let expression = node.expression().unwrap();
                let t = self.check_expression(expression);
                let t = self.get_widened_type(t);
                self.is_valid_property_access_with_type(
                    node,
                    expression.kind() == Kind::SuperKeyword,
                    property_name,
                    t,
                )
            }
            Kind::QualifiedName => {
                let t = self.check_expression(node.as_qualified_name().left);
                let t = self.get_widened_type(t);
                self.is_valid_property_access_with_type(
                    node,
                    false, /*isSuper*/
                    property_name,
                    t,
                )
            }
            Kind::ImportType => {
                let t = self.get_type_from_type_node(node);
                self.is_valid_property_access_with_type(
                    node,
                    false, /*isSuper*/
                    property_name,
                    t,
                )
            }
            _ => panic!("Unexpected node kind in isValidPropertyAccess: {:?}", node.kind()),
        }
    }

    // services.go:181
    fn is_valid_property_access_with_type(
        &mut self,
        node: P<Node>,
        is_super: bool,
        property_name: &str,
        t: P<Type>,
    ) -> bool {
        // Short-circuiting for improved performance.
        if is_type_any(Some(t)) {
            return true;
        }

        let prop = self.get_property_of_type(t, property_name);
        match prop {
            Some(prop) => {
                self.is_property_accessible(node, is_super, false /*isWrite*/, t, prop)
            }
            None => false,
        }
    }

    // services.go:199
    // Checks if an existing property access is valid for completions purposes.
    // node: a property access-like node where we want to check if we can access a property.
    // This node does not need to be an access of the property we are checking.
    // e.g. in completions, this node will often be an incomplete property access node, as in `foo.`.
    // Besides providing a location (i.e. scope) used to check property accessibility, we use this node for
    // computing whether this is a `super` property access.
    // type: the type whose property we are checking.
    // property: the accessed property's symbol.
    pub fn is_valid_property_access_for_completions_exported(
        &mut self,
        node: P<Node>,
        t: P<Type>,
        property: P<Symbol>,
    ) -> bool {
        self.is_property_accessible(
            node,
            node.kind() == Kind::PropertyAccessExpression
                && node.expression().unwrap().kind() == Kind::SuperKeyword,
            false, /*isWrite*/
            t,
            property,
        )
        // Previously we validated the 'this' type of methods but this adversely affected performance. See #31377 for more context.
    }

    // services.go:210
    pub fn get_all_possible_properties_of_types(&mut self, types: &[P<Type>]) -> Vec<P<Symbol>> {
        let union_type = self.get_union_type(types);
        if !union_type.flags().intersects(TypeFlags::Union) {
            return self.get_augmented_properties_of_type(union_type);
        }

        let props = SymbolTable::new();
        for &member_type in types {
            let augmented_props = self.get_augmented_properties_of_type(member_type);
            for p in augmented_props {
                if !props.has(p.name()) {
                    let prop = self.create_union_or_intersection_property(
                        union_type,
                        p.name(),
                        false, /*skipObjectFunctionPropertyAugment*/
                    );
                    // May be undefined if the property is private
                    if let Some(prop) = prop {
                        props.set(p.name(), prop);
                    }
                }
            }
        }
        props.values()
    }

    // services.go:232
    pub fn is_unknown_symbol(&self, symbol: P<Symbol>) -> bool {
        symbol == self.unknown_symbol
    }

    // services.go:236
    pub fn is_undefined_symbol(&self, symbol: P<Symbol>) -> bool {
        symbol == self.undefined_symbol
    }

    // services.go:240
    pub fn is_arguments_symbol(&self, symbol: P<Symbol>) -> bool {
        symbol == self.arguments_symbol
    }

    // services.go:245
    // Originally from services.ts
    pub fn get_non_optional_type(&mut self, t: P<Type>) -> P<Type> {
        self.remove_optional_type_marker(t)
    }

    // services.go:249
    pub fn get_string_index_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        self.get_index_type_of_type(t, self.string_type)
    }

    // services.go:253
    pub fn get_number_index_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        self.get_index_type_of_type(t, self.number_type)
    }

    // services.go:257
    pub fn get_element_type_of_array_type_exported(&mut self, t: P<Type>) -> Option<P<Type>> {
        self.get_element_type_of_array_type(t)
    }

    // services.go:261
    pub fn get_call_signatures(&mut self, t: P<Type>) -> &'static [P<Signature>] {
        self.get_signatures_of_type(t, SignatureKind::Call)
    }

    // services.go:269
    pub fn get_apparent_properties(&mut self, t: P<Type>) -> Vec<P<Symbol>> {
        self.get_augmented_properties_of_type(t)
    }

    // services.go:273
    fn get_augmented_properties_of_type(&mut self, t: P<Type>) -> Vec<P<Symbol>> {
        let t = self.get_apparent_type(t);
        let props = self.get_properties_of_type(t);
        let props_by_name = create_symbol_table(props);
        let mut function_type = None;
        if !self.get_signatures_of_type(t, SignatureKind::Call).is_empty() {
            function_type = Some(self.global_callable_function_type);
        } else if !self.get_signatures_of_type(t, SignatureKind::Construct).is_empty() {
            function_type = Some(self.global_newable_function_type);
        }

        let props_by_name = props_by_name.unwrap_or_else(SymbolTable::new);
        if let Some(function_type) = function_type {
            for &p in self.get_properties_of_type(function_type) {
                if !props_by_name.has(p.name()) {
                    props_by_name.set(p.name(), p);
                }
            }
        }
        self.get_named_members(Some(props_by_name), None)
    }

    // services.go:296
    pub fn try_get_member_in_module_exports_and_properties(
        &mut self,
        member_name: &str,
        module_symbol: P<Symbol>,
    ) -> Option<P<Symbol>> {
        let symbol = self.try_get_member_in_module_exports(member_name, module_symbol);
        if symbol.is_some() {
            return symbol;
        }

        let export_equals =
            self.resolve_external_module_symbol(module_symbol, false /*dontResolveAlias*/);
        if export_equals == module_symbol {
            return None;
        }

        let t = self.get_type_of_symbol(export_equals);
        if self.should_treat_properties_of_external_module_as_exports(t) {
            return self.get_property_of_type(t, member_name);
        }
        None
    }

    // services.go:314
    pub fn try_get_member_in_module_exports(
        &mut self,
        member_name: &str,
        module_symbol: P<Symbol>,
    ) -> Option<P<Symbol>> {
        let symbol_table = self.get_exports_of_module(module_symbol);
        symbol_table.lookup(member_name)
    }

    // services.go:319
    fn should_treat_properties_of_external_module_as_exports(
        &self,
        resolved_external_module_type: P<Type>,
    ) -> bool {
        !resolved_external_module_type.flags().intersects(TypeFlags::Primitive)
            || resolved_external_module_type.object_flags().intersects(ObjectFlags::Class)
            // `isArrayOrTupleLikeType` is too expensive to use in this auto-imports hot path.
            || self.is_array_type(resolved_external_module_type)
            || is_tuple_type(resolved_external_module_type)
    }

    // services.go:327
    pub fn get_contextual_type_exported(
        &mut self,
        node: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        if context_flags.intersects(ContextFlags::IgnoreNodeInferences) {
            return self.run_with_inference_blocked_from_source_node(node, |c| {
                c.get_contextual_type(node, context_flags)
            });
        }
        self.get_contextual_type(node, context_flags)
    }

    // services.go:334
    fn run_with_inference_blocked_from_source_node<T>(
        &mut self,
        node: P<Node>,
        f: impl FnOnce(&mut Checker) -> T,
    ) -> T {
        let containing_call = ast::find_ancestor(node, ast::is_call_like_expression);
        if containing_call.is_some() {
            let mut to_mark_skip = Some(node);
            loop {
                self.skip_direct_inference_nodes.add(to_mark_skip.unwrap());
                to_mark_skip = to_mark_skip.unwrap().parent();
                if to_mark_skip.is_none() || to_mark_skip == containing_call {
                    break;
                }
            }
        }

        self.is_inference_partially_blocked = true;
        let result = self.run_without_resolved_signature_caching(node, f);
        self.is_inference_partially_blocked = false;

        self.skip_direct_inference_nodes.clear();
        result
    }
}

// services.go:355
pub fn get_resolved_signature_for_signature_help(
    node: P<Node>,
    argument_count: usize,
    c: &mut Checker,
) -> (Option<P<Signature>>, Vec<P<Signature>>) {
    c.run_without_resolved_signature_caching(node, |c| {
        c.get_resolved_signature_worker(node, CheckMode::IsForSignatureHelp, argument_count)
    })
}

impl Checker {
    // services.go:367
    fn run_without_resolved_signature_caching<T>(
        &mut self,
        node: P<Node>,
        f: impl FnOnce(&mut Checker) -> T,
    ) -> T {
        let mut ancestor_node =
            ast::find_ancestor(node, ast::is_call_like_or_function_like_expression);
        if ancestor_node.is_some() {
            let mut cached_resolved_signatures: Vec<(P<SignatureLinks>, Option<P<Signature>>)> =
                Vec::new();
            let mut cached_types: Vec<(P<ValueSymbolLinks>, Option<P<Type>>)> = Vec::new();
            while let Some(an) = ancestor_node {
                let signature_links = self.signature_links.get(an);
                cached_resolved_signatures
                    .push((signature_links, signature_links.resolved_signature.get()));
                signature_links.resolved_signature.set(None);
                if ast::is_function_expression_or_arrow_function(an) {
                    let symbol = self.get_symbol_of_declaration(an).unwrap();
                    let symbol_links = self.value_symbol_links.get(symbol);
                    let resolved_type = symbol_links.resolved_type.get();
                    cached_types.push((symbol_links, resolved_type));
                    symbol_links.resolved_type.set(None);
                }
                ancestor_node =
                    ast::find_ancestor(an.parent(), ast::is_call_like_or_function_like_expression);
            }
            let result = f(self);
            for (signature_links, resolved_signature) in cached_resolved_signatures {
                signature_links.resolved_signature.set(resolved_signature);
            }
            for (symbol_links, resolved_type) in cached_types {
                symbol_links.resolved_type.set(resolved_type);
            }
            return result;
        }
        f(self)
    }

    // services.go:396
    pub fn skip_alias(&mut self, symbol: P<Symbol>) -> P<Symbol> {
        if symbol.flags().intersects(SymbolFlags::Alias) {
            return self.get_aliased_symbol(symbol);
        }
        symbol
    }

    // services.go:403
    pub fn get_root_symbols(&mut self, symbol: P<Symbol>) -> Vec<P<Symbol>> {
        let roots = self.get_immediate_root_symbols(symbol);
        if roots.is_empty() {
            return vec![symbol];
        }
        let mut result = Vec::new();
        result.extend(roots.into_iter().flat_map(|root| self.get_root_symbols(root)));
        result
    }

    // services.go:415
    pub fn get_mapped_type_symbol_of_property(&self, symbol: P<Symbol>) -> Option<P<Symbol>> {
        if let Some(value_links) = self.value_symbol_links.try_get(symbol) {
            return value_links.containing_type().unwrap().symbol();
        }
        None
    }

    // services.go:422
    fn get_immediate_root_symbols(&mut self, symbol: P<Symbol>) -> Vec<P<Symbol>> {
        if symbol.check_flags().intersects(CheckFlags::Synthetic) {
            let types =
                self.value_symbol_links.get(symbol).containing_type().unwrap().types().to_vec();
            let mut result = Vec::new();
            result.extend(
                types.into_iter().filter_map(|t| self.get_property_of_type(t, symbol.name())),
            );
            return result;
        }
        if symbol.flags().intersects(SymbolFlags::Transient) {
            if self.spread_links.has(symbol) {
                let left_spread = self.spread_links.get(symbol).left_spread.get();
                let right_spread = self.spread_links.get(symbol).right_spread.get();
                if let Some(left_spread) = left_spread {
                    return vec![left_spread, right_spread.unwrap()];
                }
            }
            if self.mapped_symbol_links.has(symbol) {
                let synthetic_origin = self.mapped_symbol_links.get(symbol).synthetic_origin.get();
                if let Some(synthetic_origin) = synthetic_origin {
                    return vec![synthetic_origin];
                }
            }
            let target = self.try_get_target(symbol);
            if let Some(target) = target {
                return vec![target];
            }
        }
        Vec::new()
    }

    // services.go:453
    fn try_get_target(&mut self, symbol: P<Symbol>) -> Option<P<Symbol>> {
        let mut target = None;
        let mut next = Some(symbol);
        loop {
            let n = next.unwrap();
            if self.value_symbol_links.has(n) {
                next = self.value_symbol_links.get(n).target();
            } else if self.export_type_links.has(n) {
                next = self.export_type_links.get(n).target.get();
            } else {
                next = None;
            }
            if next.is_none() {
                break;
            }
            target = next;
        }
        target
    }

    // services.go:472
    pub fn get_export_symbol_of_symbol(&mut self, symbol: P<Symbol>) -> P<Symbol> {
        self.get_merged_symbol(symbol.export_symbol().unwrap_or(symbol))
    }

    // services.go:476
    pub fn get_export_specifier_local_target_symbol(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        // node should be ExportSpecifier | Identifier
        match node.kind() {
            Kind::ExportSpecifier => {
                if node.parent().unwrap().parent().unwrap().module_specifier().is_some() {
                    return self.get_external_module_member(
                        node.parent().unwrap().parent().unwrap(),
                        node,
                        false, /*dontResolveAlias*/
                    );
                }
                let name = node.property_name_or_name().unwrap();
                if name.kind() == Kind::StringLiteral {
                    // Skip for invalid syntax like this: export { "x" }
                    return None;
                }
                self.resolve_entity_name(
                    name,
                    SymbolFlags::Value
                        | SymbolFlags::Type
                        | SymbolFlags::Namespace
                        | SymbolFlags::Alias,
                    true, /*ignoreErrors*/
                    false,
                    None,
                )
            }
            Kind::Identifier => {
                self.resolve_entity_name(
                    node,
                    SymbolFlags::Value
                        | SymbolFlags::Type
                        | SymbolFlags::Namespace
                        | SymbolFlags::Alias,
                    true, /*ignoreErrors*/
                    false,
                    None,
                )
            }
            _ => panic!(
                "Unhandled case in getExportSpecifierLocalTargetSymbol, node should be ExportSpecifier | Identifier"
            ),
        }
    }

    // services.go:495
    pub fn get_shorthand_assignment_value_symbol(
        &mut self,
        location: Option<P<Node>>,
    ) -> Option<P<Symbol>> {
        if let Some(location) = location
            && location.kind() == Kind::ShorthandPropertyAssignment
        {
            return self.resolve_entity_name(
                location.name().unwrap(),
                SymbolFlags::Value | SymbolFlags::Alias,
                true, /*ignoreErrors*/
                false,
                None,
            );
        }
        None
    }

    // services.go:508
    /**
     * Get symbols that represent parameter-property-declaration as parameter and as property declaration
     * @param parameter a parameterDeclaration node
     * @param parameterName a name of the parameter to get the symbols for.
     * @return a tuple of two symbols
     */
    pub fn get_symbols_of_parameter_property_declaration(
        &mut self,
        parameter: P<Node>, /*ParameterPropertyDeclaration*/
        parameter_name: &str,
    ) -> (P<Symbol>, P<Symbol>) {
        let constructor_declaration = parameter.parent().unwrap();
        let class_declaration = parameter.parent().unwrap().parent().unwrap();

        let parameter_symbol =
            self.get_symbol(constructor_declaration.locals(), parameter_name, SymbolFlags::Value);
        let members = self.get_members_of_symbol(class_declaration.symbol().unwrap());
        let property_symbol = self.get_symbol(members, parameter_name, SymbolFlags::Value);

        if let (Some(parameter_symbol), Some(property_symbol)) = (parameter_symbol, property_symbol)
        {
            return (parameter_symbol, property_symbol);
        }

        panic!(
            "There should exist two symbols, one as property declaration and one as parameter declaration"
        );
    }

    // services.go:524
    // IsDeclarationUsed checks if an import declaration identifier is used in the source file.
    // This is primarily used for organizing imports to determine which imports can be removed.
    pub fn is_declaration_used(
        &mut self,
        source_file: P<SourceFile>,
        identifier: P<Node>,
        jsx_elements_present: bool,
        jsx_mode_needs_explicit_import: bool,
    ) -> bool {
        if jsx_elements_present && jsx_mode_needs_explicit_import {
            let jsx_namespace = self.get_jsx_namespace(Some(source_file.as_node()));
            let jsx_fragment_factory = self.get_jsx_fragment_factory(source_file.as_node());
            let identifier_text = identifier.text();
            if identifier_text == jsx_namespace {
                return true;
            }
            if !jsx_fragment_factory.is_empty() && identifier_text == jsx_fragment_factory {
                return true;
            }
        }

        let symbol = self.get_symbol_at_location_exported(identifier);
        let Some(symbol) = symbol else {
            return true;
        };

        self.is_symbol_referenced_in_file(source_file, identifier, symbol)
    }

    // services.go:552
    // IsSymbolReferencedInFile checks if a symbol is referenced in the source file (besides its definition).
    // This is used as a quick check for whether a symbol is used at all in a file.
    fn is_symbol_referenced_in_file(
        &mut self,
        source_file: P<SourceFile>,
        definition: P<Node>,
        symbol: P<Symbol>,
    ) -> bool {
        let identifier_text = definition.text();
        if get_possible_symbol_reference_nodes(
            source_file,
            identifier_text,
            Some(source_file.as_node()),
        )
        .into_iter()
        .any(|token| {
            if !ast::is_identifier(token) {
                return false;
            }
            if token == definition || token.text() != identifier_text {
                return false;
            }
            let ref_symbol = self.get_symbol_at_location_exported(token);
            if ref_symbol == Some(symbol) {
                return true;
            }
            if let Some(parent) = token.parent()
                && parent.kind() == Kind::ShorthandPropertyAssignment
            {
                let shorthand_symbol = self.get_shorthand_assignment_value_symbol(Some(parent));
                if shorthand_symbol == Some(symbol) {
                    return true;
                }
            }
            if let Some(parent) = token.parent()
                && ast::is_export_specifier(parent)
            {
                let local_symbol =
                    self.get_local_symbol_for_export_specifier(token, ref_symbol, parent);
                if local_symbol == Some(symbol) {
                    return true;
                }
            }

            false
        }) {
            return true;
        }
        false
    }

    // services.go:587
    // GetReferencesToSymbolInFile returns all identifier nodes in the file that reference the given symbol.
    pub fn get_references_to_symbol_in_file(
        &mut self,
        source_file: P<SourceFile>,
        symbol: P<Symbol>,
    ) -> Vec<P<Node>> {
        let identifier_text = symbol.name();
        let mut result = Vec::new();
        result.extend(
            get_possible_symbol_reference_nodes(
                source_file,
                identifier_text,
                Some(source_file.as_node()),
            )
            .into_iter()
            .filter(|&token| {
                if !ast::is_identifier(token) {
                    return false;
                }
                if token.text() != identifier_text {
                    return false;
                }
                let ref_symbol = self.get_symbol_at_location_exported(token);
                if ref_symbol == Some(symbol) {
                    return true;
                }
                if let Some(parent) = token.parent()
                    && parent.kind() == Kind::ShorthandPropertyAssignment
                {
                    let shorthand_symbol = self.get_shorthand_assignment_value_symbol(Some(parent));
                    if shorthand_symbol == Some(symbol) {
                        return true;
                    }
                }
                if let Some(parent) = token.parent() {
                    if ast::is_export_specifier(parent) {
                        let local_symbol =
                            self.get_local_symbol_for_export_specifier(token, ref_symbol, parent);
                        local_symbol == Some(symbol)
                    } else {
                        false
                    }
                } else {
                    false
                }
            }),
        );
        result
    }

    // services.go:624
    fn get_local_symbol_for_export_specifier(
        &mut self,
        reference_location: P<Node>,
        reference_symbol: Option<P<Symbol>>,
        export_specifier: P<Node>,
    ) -> Option<P<Symbol>> {
        if is_export_specifier_alias(reference_location, export_specifier)
            && let Some(symbol) = self.get_export_specifier_local_target_symbol(export_specifier)
        {
            return Some(symbol);
        }
        reference_symbol
    }
}

// services.go:633
pub fn is_export_specifier_alias(reference_location: P<Node>, export_specifier: P<Node>) -> bool {
    assert!(
        export_specifier.property_name() == Some(reference_location)
            || export_specifier.name() == Some(reference_location),
        "referenceLocation is not export specifier name or property name"
    );
    let property_name = export_specifier.property_name();
    if let Some(property_name) = property_name {
        // Given `export { foo as bar } [from "someModule"]`: It's an alias at `foo`, but at `bar` it's a new symbol.
        property_name == reference_location
    } else {
        // `export { foo } from "foo"` is a re-export.
        // `export { foo };` is not a re-export, it creates an alias for the local variable `foo`.
        export_specifier.parent().unwrap().parent().unwrap().module_specifier().is_none()
    }
}

// services.go:646
pub fn get_possible_symbol_reference_nodes(
    source_file: P<SourceFile>,
    symbol_name: &str,
    container: Option<P<Node>>,
) -> Vec<P<Node>> {
    map_non_nil(
        &get_possible_symbol_reference_positions(source_file, symbol_name, container),
        |&pos| {
            let reference_location = astnav::get_touching_property_name(source_file, pos);
            if reference_location != source_file.as_node() {
                return Some(reference_location);
            }
            None
        },
    )
}

// services.go:655
pub fn get_possible_symbol_reference_positions(
    source_file: P<SourceFile>,
    symbol_name: &str,
    container: Option<P<Node>>,
) -> Vec<u32> {
    let mut positions = Vec::new();

    // TODO: Cache symbol existence for files to save text search
    // Also, need to make this work for unicode escapes.

    // Be resilient in the face of a symbol with no name or zero length name
    if symbol_name.is_empty() {
        return positions;
    }

    let text = source_file.text();
    let bytes = text.as_bytes();
    let source_length = text.len();
    let symbol_name_length = symbol_name.len();

    let container = container.unwrap_or(source_file.as_node());

    let container_start =
        container.pos().expect_source("parsed containers have source positions") as usize;
    let Some(mut position) =
        tsrs_core::index_bytes(&bytes[container_start..], symbol_name.as_bytes())
    else {
        return positions;
    };
    let end_pos = container.end().expect_source("parsed containers have source positions") as usize;
    while position < end_pos {
        // We found a match.  Make sure it's not part of a larger word (i.e. the char
        // before and after it have to be a non-identifier char).
        let end_position = position + symbol_name_length;

        if (position == 0 || !tsrs_scanner::is_identifier_part(u32::from(bytes[position - 1])))
            && (end_position == source_length
                || !tsrs_scanner::is_identifier_part(u32::from(bytes[end_position])))
        {
            // Found a real match.  Keep searching.
            positions.push(position.try_into().expect("source position fits in u32"));
        }
        let start_index = position + symbol_name_length + 1;
        if start_index > text.len() {
            break;
        }
        if let Some(found_index) =
            tsrs_core::index_bytes(&bytes[start_index..], symbol_name.as_bytes())
        {
            position = start_index + found_index;
        } else {
            break;
        }
    }

    positions
}

// Go strings.Index; Go slices strings at byte offsets that need not be character boundaries.
impl Checker {
    // services.go:700
    pub fn get_type_argument_constraint_exported(&mut self, node: P<Node>) -> Option<P<Type>> {
        if !ast::is_type_node(node) {
            return None;
        }
        self.get_type_argument_constraint(node)
    }

    // services.go:708
    // getUninstantiatedSignatures gets generic signatures from the function's/constructor's type.
    fn get_uninstantiated_signatures(&mut self, node: P<Node>) -> &'static [P<Signature>] {
        match node.kind() {
            Kind::CallExpression | Kind::Decorator => {
                let t = self.get_type_of_expression(node.expression().unwrap());
                self.get_signatures_of_type(t, SignatureKind::Call)
            }
            Kind::NewExpression => {
                let t = self.get_type_of_expression(node.expression().unwrap());
                self.get_signatures_of_type(t, SignatureKind::Construct)
            }
            Kind::JsxSelfClosingElement | Kind::JsxOpeningElement => {
                if is_jsx_intrinsic_tag_name(node.tag_name()) {
                    return &[];
                }
                let t = self.get_type_of_expression(node.tag_name());
                self.get_signatures_of_type(t, SignatureKind::Call)
            }
            Kind::TaggedTemplateExpression => {
                let t = self.get_type_of_expression(node.as_tagged_template_expression().tag);
                self.get_signatures_of_type(t, SignatureKind::Call)
            }
            _ => &[],
        }
    }

    // services.go:727
    fn get_type_parameter_constraint_for_position_across_signatures(
        &mut self,
        signatures: &[P<Signature>],
        position: usize,
    ) -> P<Type> {
        let mut relevant_constraints = Vec::new();
        relevant_constraints.extend(signatures.iter().filter_map(|signature| {
            if position >= signature.type_parameters().len() {
                return None;
            }
            let relevant_type_parameter = signature.type_parameters()[position];

            self.get_constraint_of_type_parameter(relevant_type_parameter)
        }));
        self.get_union_type(&relevant_constraints)
    }

    // services.go:742
    fn get_type_argument_constraint(&mut self, node: P<Node>) -> Option<P<Type>> {
        let mut type_argument_position = None;
        let parent = node.parent().unwrap();
        if ast::has_type_arguments(parent) {
            let type_args = parent.type_arguments();
            if let Some((i, _arg)) = type_args
                .iter()
                .enumerate()
                .find_map(|(i, &arg)| if arg == node { Some((i, arg)) } else { None })
            {
                type_argument_position = Some(i);
            }
        }

        if let Some(type_argument_position) = type_argument_position {
            // The node could be a type argument of a call, a `new` expression, a decorator, an
            // instantiation expression, or a generic type instantiation.

            if ast::is_call_like_expression(parent) {
                let signatures = self.get_uninstantiated_signatures(parent);
                return Some(self.get_type_parameter_constraint_for_position_across_signatures(
                    signatures,
                    type_argument_position,
                ));
            }

            if ast::is_decorator(parent.parent().unwrap()) {
                let signatures = self.get_uninstantiated_signatures(parent.parent().unwrap());
                return Some(self.get_type_parameter_constraint_for_position_across_signatures(
                    signatures,
                    type_argument_position,
                ));
            }

            if ast::is_expression_with_type_arguments(parent)
                && ast::is_expression_statement(parent.parent().unwrap())
            {
                let uninstantiated_type = self.check_expression(parent.expression().unwrap());

                let call_signatures =
                    self.get_signatures_of_type(uninstantiated_type, SignatureKind::Call);
                let call_constraint = self
                    .get_type_parameter_constraint_for_position_across_signatures(
                        call_signatures,
                        type_argument_position,
                    );
                let construct_signatures =
                    self.get_signatures_of_type(uninstantiated_type, SignatureKind::Construct);
                let construct_constraint = self
                    .get_type_parameter_constraint_for_position_across_signatures(
                        construct_signatures,
                        type_argument_position,
                    );

                // An instantiation expression instantiates both call and construct signatures, so
                // if both exist type arguments must be assignable to both constraints.
                if construct_constraint.flags().intersects(TypeFlags::Never) {
                    return Some(call_constraint);
                }
                if call_constraint.flags().intersects(TypeFlags::Never) {
                    return Some(construct_constraint);
                }
                return Some(self.get_intersection_type(&[call_constraint, construct_constraint]));
            }

            if ast::is_type_reference_type(parent) {
                let type_parameters = self.get_type_parameters_for_type_reference_or_import(parent);
                if type_parameters.is_empty() {
                    return None;
                }
                if type_argument_position >= type_parameters.len() {
                    return None;
                }
                let relevant_type_parameter = type_parameters[type_argument_position];
                let constraint = self.get_constraint_of_type_parameter(relevant_type_parameter);
                if let Some(constraint) = constraint {
                    let type_arguments =
                        self.get_effective_type_arguments(parent, &type_parameters);
                    return Some(self.instantiate_type(
                        constraint,
                        Some(new_type_mapper(
                            alloc_vec(type_parameters),
                            alloc_vec(type_arguments),
                        )),
                    ));
                }
            }
        }
        None
    }

    // services.go:816
    pub fn is_type_invalid_due_to_union_discriminant(
        &mut self,
        contextual_type: P<Type>,
        obj: P<Node>,
    ) -> bool {
        let properties = obj.properties();
        properties.iter().any(|&property| {
            let mut name_type = None;
            let property_name = property.name();
            if let Some(property_name) = property_name {
                if ast::is_jsx_namespaced_name(property_name) {
                    name_type = Some(self.get_string_literal_type(property_name.text()));
                } else {
                    name_type = Some(self.get_literal_type_from_property_name(property_name));
                }
            }
            let mut name = std::borrow::Cow::Borrowed("");
            if let Some(name_type) = name_type
                && is_type_usable_as_property_name(name_type)
            {
                name = get_property_name_from_type(name_type);
            }
            let mut expected = None;
            if !name.is_empty() {
                expected = self.get_type_of_property_of_type(contextual_type, &name);
            }
            match expected {
                Some(expected) => {
                    if !is_literal_type(expected) {
                        return false;
                    }
                    let t = self.get_type_of_node(property);
                    !self.is_type_assignable_to(t, expected)
                }
                None => false,
            }
        })
    }

    // services.go:841
    // Unlike `getExportsOfModule`, this includes properties of an `export =` value.
    pub fn get_exports_and_properties_of_module(
        &mut self,
        module_symbol: P<Symbol>,
    ) -> Vec<P<Symbol>> {
        let mut exports = self.get_exports_of_module_as_array(module_symbol);
        let export_equals =
            self.resolve_external_module_symbol(module_symbol, false /*dontResolveAlias*/);
        if export_equals != module_symbol {
            let t = self.get_type_of_symbol(export_equals);
            if self.should_treat_properties_of_external_module_as_exports(t) {
                exports.extend_from_slice(self.get_properties_of_type(t));
            }
        }
        exports
    }

    // services.go:853
    fn get_exports_of_module_as_array(&mut self, module_symbol: P<Symbol>) -> Vec<P<Symbol>> {
        symbols_to_array(Some(self.get_exports_of_module(module_symbol)))
    }

    // services.go:858
    // Returns all the properties of the Jsx.IntrinsicElements interface.
    pub fn get_jsx_intrinsic_tag_names_at(&mut self, location: P<Node>) -> &'static [P<Symbol>] {
        let intrinsics = self.get_jsx_type(JSX_NAMES.intrinsic_elements, location);
        self.get_properties_of_type_exported(intrinsics)
    }

    // services.go:866
    pub fn get_contextual_type_for_jsx_attribute_exported(
        &mut self,
        attribute: P<Node>,
    ) -> Option<P<Type>> {
        self.get_contextual_type_for_jsx_attribute(attribute, ContextFlags::None)
    }

    // services.go:870
    pub fn get_constant_value(&mut self, node: P<Node>) -> Option<LiteralValue> {
        if node.kind() == Kind::EnumMember {
            return self.get_enum_member_value(node).value;
        }

        if self.symbol_node_links.get(node).resolved_symbol.get().is_none() {
            self.check_expression_cached(node); // ensure cached resolved symbol is set
        }
        let mut symbol = self.symbol_node_links.get(node).resolved_symbol.get();
        if symbol.is_none() && ast::is_entity_name_expression(node) {
            symbol = self.resolve_entity_name(
                node,
                SymbolFlags::Value,
                true,  /*ignoreErrors*/
                false, /*dontResolveAlias*/
                None,  /*location*/
            );
        }
        if let Some(symbol) = symbol
            && symbol.flags().intersects(SymbolFlags::EnumMember)
        {
            // inline property\index accesses only for const enums
            let member = symbol.value_declaration().unwrap();
            if ast::is_enum_const(member.parent().unwrap()) {
                return self.get_enum_member_value(member).value;
            }
        }

        None
    }

    // services.go:899
    fn get_resolved_signature_worker(
        &mut self,
        node: P<Node>,
        check_mode: CheckMode,
        argument_count: usize,
    ) -> (Option<P<Signature>>, Vec<P<Signature>>) {
        // Go: printer.NewEmitContext().ParseNode(node). A fresh EmitContext has no original-node links, so ParseNode
        // returns the node itself when it is a parse tree node and nil otherwise.
        let parsed_node = if ast::is_parse_tree_node(node) { Some(node) } else { None };
        self.apparent_argument_count = Some(argument_count);
        let mut candidates_out_array = Vec::new();
        let mut res = None;
        if let Some(parsed_node) = parsed_node {
            res = Some(self.get_resolved_signature(
                parsed_node,
                Some(&mut candidates_out_array),
                check_mode,
            ));
        }
        self.apparent_argument_count = None;
        (res, candidates_out_array)
    }

    // services.go:911
    pub fn get_candidate_signatures_for_string_literal_completions(
        &mut self,
        call: P<Node>,
        editing_argument: P<Node>,
    ) -> Vec<P<Signature>> {
        // first, get candidates when inference is blocked from the source node.
        let mut candidates =
            self.run_with_inference_blocked_from_source_node(editing_argument, |c| {
                let (_, blocked_inference_candidates) =
                    c.get_resolved_signature_worker(call, CheckMode::Normal, 0);
                blocked_inference_candidates
            });
        let candidates_set: FxHashSet<P<Signature>> = candidates.iter().copied().collect();

        // next, get candidates where the source node is considered for inference.
        let other_candidates = self.run_without_resolved_signature_caching(editing_argument, |c| {
            let (_, inference_candidates) =
                c.get_resolved_signature_worker(call, CheckMode::Normal, 0);
            inference_candidates
        });

        candidates.extend(
            other_candidates.into_iter().filter(|candidate| !candidates_set.contains(candidate)),
        );

        candidates
    }

    // services.go:936
    // GetTypeAtPosition returns the type of a parameter at a given index in a signature.
    pub fn get_type_at_position_exported(&mut self, s: P<Signature>, pos: usize) -> P<Type> {
        self.get_type_at_position(s, pos)
    }

    // services.go:940
    pub fn get_type_parameter_at_position(&mut self, s: P<Signature>, pos: usize) -> P<Type> {
        let t = self.get_type_at_position(s, pos);
        if t.is_index() && is_this_type_parameter(t.as_index_type().target().unwrap()) {
            let constraint = self.get_base_constraint_of_type(t.as_index_type().target().unwrap());
            if let Some(constraint) = constraint {
                return self.get_index_type(constraint);
            }
        }
        t
    }

    // services.go:953
    // GetContextualTypeForArrayLiteralAtPosition returns the contextual type for an element at the given position
    // in an array with the given contextual type.
    pub fn get_contextual_type_for_array_literal_at_position(
        &mut self,
        contextual_array_type: Option<P<Type>>,
        array_literal: P<Node>,
        position: u32,
    ) -> Option<P<Type>> {
        contextual_array_type?;
        let (mut first_spread_index, mut last_spread_index) = (None, None);
        let mut element_index = 0;
        let elements = array_literal.elements();
        for (i, &elem) in elements.iter().enumerate() {
            if elem.pos().expect_source("parsed nodes have source positions") < position {
                element_index += 1;
            }
            if ast::is_spread_element(elem) {
                if first_spread_index.is_none() {
                    first_spread_index = Some(i);
                }
                last_spread_index = Some(i);
            }
        }
        // The array may be incomplete, so we don't know its final length.
        self.get_contextual_type_for_element_expression(
            contextual_array_type,
            element_index,
            None, /*length*/
            first_spread_index,
            last_spread_index,
        )
    }
}

// services.go:981
pub static KNOWN_GENERIC_TYPE_NAMES: [&str; 20] = [
    "Array",
    "ArrayLike",
    "ReadonlyArray",
    "Promise",
    "PromiseLike",
    "Iterable",
    "IterableIterator",
    "AsyncIterable",
    "Set",
    "WeakSet",
    "ReadonlySet",
    "Map",
    "WeakMap",
    "ReadonlyMap",
    "Partial",
    "Required",
    "Readonly",
    "Pick",
    "Omit",
    "NonNullable",
];

// services.go:1004
pub fn is_known_generic_type_name(name: &str) -> bool {
    KNOWN_GENERIC_TYPE_NAMES.contains(&name)
}

impl Checker {
    // services.go:1009
    pub fn get_first_type_argument_from_known_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        if t.object_flags().intersects(ObjectFlags::Reference)
            && let Some(t_symbol) = t.symbol()
            && is_known_generic_type_name(t_symbol.name())
        {
            let symbol = self.get_global_symbol(t_symbol.name(), SymbolFlags::Type, None);
            if symbol.is_some() && symbol == t.target().unwrap().symbol() {
                return self.get_type_arguments(t).first().copied();
            }
        }
        if let Some(alias) = t.alias() {
            let alias_symbol = alias.symbol().unwrap();
            if is_known_generic_type_name(alias_symbol.name()) {
                let symbol = self.get_global_symbol(alias_symbol.name(), SymbolFlags::Type, None);
                if symbol == Some(alias_symbol) {
                    return alias.type_arguments().first().copied();
                }
            }
        }
        None
    }

    // services.go:1026
    // Gets all symbols for one property. Does not get symbols for every property.
    pub fn get_property_symbols_from_contextual_type(
        &mut self,
        node: P<Node>,
        contextual_type: P<Type>,
        union_symbol_ok: bool,
    ) -> Vec<P<Symbol>> {
        let name = ast::get_text_of_property_name(node.name().unwrap());
        if name.is_empty() {
            return Vec::new();
        }
        if !contextual_type.flags().intersects(TypeFlags::Union) {
            if let Some(symbol) = self.get_property_of_type(contextual_type, &name) {
                return vec![symbol];
            }
            return Vec::new();
        }
        let mut filtered_types = contextual_type.types().to_vec();
        let parent = node.parent().unwrap();
        if ast::is_object_literal_expression(parent) || ast::is_jsx_attributes(parent) {
            filtered_types.retain(|&t| !self.is_type_invalid_due_to_union_discriminant(t, parent));
        }
        let discriminated_property_symbols: Vec<P<Symbol>> =
            filtered_types.iter().filter_map(|&t| self.get_property_of_type(t, &name)).collect();
        if union_symbol_ok
            && (discriminated_property_symbols.is_empty()
                || discriminated_property_symbols.len() == contextual_type.types().len())
            && let Some(symbol) = self.get_property_of_type(contextual_type, &name)
        {
            return vec![symbol];
        }
        if filtered_types.is_empty() && discriminated_property_symbols.is_empty() {
            // Bad discriminant -- do again without discriminating
            return contextual_type
                .types()
                .iter()
                .filter_map(|&t| self.get_property_of_type(t, &name))
                .collect();
        }
        // by eliminating duplicates we might even end up with a single symbol
        // that helps with displaying better quick infos on properties of union types
        deduplicate(&discriminated_property_symbols).into_owned()
    }

    // services.go:1071
    // Gets the property symbol corresponding to the property in destructuring assignment
    // 'property1' from
    //
    //	for ( { property1: a } of elems) {
    //	}
    //
    // 'property1' at location 'a' from:
    //
    //	[a] = [ property1, property2 ]
    pub fn get_property_symbol_of_destructuring_assignment(
        &mut self,
        location: P<Node>,
    ) -> Option<P<Symbol>> {
        let grandparent = location.parent().unwrap().parent();
        if ast::is_array_literal_or_object_literal_destructuring_pattern(grandparent) {
            // Get the type of the object or array literal and then look for property of given name in the type
            if let Some(type_of_object_literal) =
                self.get_type_of_assignment_pattern(grandparent.unwrap())
            {
                return self.get_property_of_type(type_of_object_literal, location.text());
            }
        }
        None
    }

    // services.go:1090
    // Gets the type of object literal or array literal of destructuring assignment.
    // { a } from
    //
    //	for ( { a } of elems) {
    //	}
    //
    // [ a ] from
    //
    //	[a] = [ some array ...]
    fn get_type_of_assignment_pattern(&mut self, expr: P<Node>) -> Option<P<Type>> {
        let parent = expr.parent().unwrap();
        // If this is from "for of"
        //     for ( { a } of elems) {
        //     }
        if ast::is_for_of_statement(parent) {
            let iterated_type = self.check_right_hand_side_of_for_of(parent);
            return Some(self.check_destructuring_assignment(
                expr,
                iterated_type,
                CheckMode::Normal,
                false,
            ));
        }
        // If this is from "for" initializer
        //     for ({a } = elems[0];.....) { }
        if ast::is_binary_expression(parent) {
            let iterated_type =
                self.get_type_of_expression(parent.as_binary_expression().right.get());
            return Some(self.check_destructuring_assignment(
                expr,
                iterated_type,
                CheckMode::Normal,
                false,
            ));
        }
        // If this is from nested object binding pattern
        //     for ({ skills: { primary, secondary } } = multiRobot, i = 0; i < 1; i++) {
        if ast::is_property_assignment(parent) {
            let node = parent.parent().unwrap();
            let type_of_parent_object_literal =
                self.get_type_of_assignment_pattern(node).unwrap_or(self.error_type);
            let property_index = index_of_node(node.properties(), parent)?;
            return self.check_object_literal_destructuring_property_assignment(
                node,
                type_of_parent_object_literal,
                property_index,
                None,
                false,
            );
        }
        // Array literal assignment - array destructuring pattern
        let node = parent;
        //    [{ property1: p1, property2 }] = elems;
        let type_of_array_literal =
            self.get_type_of_assignment_pattern(node).unwrap_or(self.error_type);
        let element_type = self.check_iterated_type_or_element_type(
            IterationUse::Destructuring,
            type_of_array_literal,
            self.undefined_type,
            Some(parent),
        );
        let element_index = index_of_node(node.elements(), expr)?;
        self.check_array_literal_destructuring_element_assignment(
            node,
            type_of_array_literal,
            element_index,
            element_type,
            CheckMode::Normal,
        )
    }

    // services.go:1120
    pub fn get_signature_from_declaration_exported(&mut self, node: P<Node>) -> P<Signature> {
        self.get_signature_from_declaration(node)
    }

    // services.go:1125
    // IsLibSymbolForHoverVerbosity returns true if a symbol is declared in a lib file.
    pub(crate) fn is_lib_symbol_for_hover_verbosity(&self, symbol: Option<P<Symbol>>) -> bool {
        let Some(symbol) = symbol else {
            return false;
        };
        let declarations = symbol.declarations().to_vec();
        if declarations.into_iter().any(|decl| {
            let sf = ast::get_source_file_of_node(decl);
            if let Some(sf) = sf
                && self.program.is_source_file_default_library(sf.path())
            {
                return true;
            }

            false
        }) {
            return true;
        }
        false
    }

    // services.go:1140
    // IsLibTypeForHoverVerbosity returns true if a type is declared in a lib file.
    // Don't expand types like Array or Promise, instead treating them as opaque.
    pub fn is_lib_type_for_hover_verbosity(&self, t: P<Type>) -> bool {
        let symbol = if t.object_flags().intersects(ObjectFlags::Reference) {
            t.target().unwrap().symbol()
        } else {
            t.symbol()
        };
        if self.is_lib_symbol_for_hover_verbosity(symbol) {
            return true;
        }
        is_tuple_type(t)
    }
}
