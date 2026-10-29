use crate::*;

impl DeclarationTransformer {
    // transform.go:2328
    pub(crate) fn ensure_modifiers(&self, node: P<Node>) -> Option<P<ModifierList>> {
        let current_flags =
            ast::get_combined_modifier_flags(self.emit_context().parse_node(Some(node)).unwrap())
                & ModifierFlags::All;
        let new_flags = self.ensure_modifier_flags(node);
        if current_flags == new_flags {
            // Elide decorators
            let mods = node.modifiers();
            let Some(mods) = mods else {
                return mods;
            };
            if can_reuse_modifier_nodes(mods.nodes()) {
                return Some(self.factory().new_modifier_list(
                    mods.nodes().iter().copied().filter(|&m| ast::is_modifier(m)).collect(),
                ));
            }
        }
        let result = ast::create_modifiers_from_modifier_flags(new_flags, |k| {
            self.factory().new_modifier(k)
        });
        if result.is_empty() {
            return None;
        }
        Some(self.factory().new_modifier_list(result))
    }

    // transform.go:2348
    pub(crate) fn ensure_modifier_flags(&self, node: P<Node>) -> ModifierFlags {
        let mut mask = ModifierFlags::All
            ^ (ModifierFlags::Public | ModifierFlags::Async | ModifierFlags::Override); // No async and override modifiers in declaration files
        let mut additions = ModifierFlags::None;
        if self.needs_declare.get() && !is_always_type(node) {
            additions = ModifierFlags::Ambient;
        }
        let parent_is_file = node.parent().unwrap().kind() == Kind::SourceFile;
        if !parent_is_file {
            mask ^= ModifierFlags::Ambient;
            additions = ModifierFlags::None;
        }
        if ast::is_implicitly_exported_jsdoc_declaration(node) {
            additions |= ModifierFlags::Export;
        }
        mask_modifier_flags(node, mask, additions)
    }

    // transform.go:2365
    // SIG: `params` is Go `*ast.TypeParameterList`, nil when the declaration has no type parameters (VisitNodes(nil)
    // returns nil): `P<NodeList>` -> `Option<P<NodeList>>`.
    pub(crate) fn ensure_type_params(
        &self,
        node: P<Node>,
        params: Option<P<NodeList>>,
    ) -> Option<P<NodeList>> {
        if !self
            .resolver
            .get_effective_declaration_flags(
                self.emit_context().parse_node(Some(node)).unwrap(),
                ModifierFlags::Private,
            )
            .is_empty()
        {
            return None;
        }
        let mut type_parameters = self.visitor().visit_nodes(params);
        if type_parameters.is_some() {
            return type_parameters;
        }
        let old_error_name_node = self.state.error_name_node.get();
        self.state.error_name_node.set(node.name());
        let mut old_diag: Option<GetSymbolAccessibilityDiagnostic> = None;
        if !self.suppress_new_diagnostic_contexts.get() {
            old_diag = Some(self.state.get_symbol_accessibility_diagnostic.borrow().clone());
            if can_produce_diagnostics(node) {
                *self.state.get_symbol_accessibility_diagnostic.borrow_mut() =
                    create_get_symbol_accessibility_diagnostic_for_node(node);
            }
        }

        if let Some(data) = node.function_like_data()
            && data.full_signature.get().is_some()
            && let Some(nodes) = self.resolver.create_type_parameters_of_signature_declaration(
                node,
                self.enclosing_declaration.get(),
                DECLARATION_EMIT_NODE_BUILDER_FLAGS,
                DECLARATION_EMIT_INTERNAL_NODE_BUILDER_FLAGS,
                self.tracker.get(),
            )
        {
            type_parameters = Some(P::new(NodeList::new(node.loc(), alloc_vec(nodes))));
        }

        self.state.error_name_node.set(old_error_name_node);
        if !self.suppress_new_diagnostic_contexts.get() {
            *self.state.get_symbol_accessibility_diagnostic.borrow_mut() = old_diag.unwrap();
        }
        type_parameters
    }

    // transform.go:2399
    pub(crate) fn update_param_list(&self, node: P<Node>, params: P<NodeList>) -> P<NodeList> {
        if !self
            .resolver
            .get_effective_declaration_flags(
                self.emit_context().parse_node(Some(node)).unwrap(),
                ModifierFlags::Private,
            )
            .is_empty()
            || params.nodes().is_empty()
        {
            return self.factory().new_node_list(Vec::new());
        }
        let mut results = Vec::with_capacity(params.nodes().len());
        results.extend(params.nodes().iter().map(|&p| self.ensure_parameter(p)));
        self.factory().new_node_list(results)
    }

    // transform.go:2410
    pub(crate) fn ensure_parameter(&self, p: P<Node>) -> P<Node> {
        let old_diag = self.state.get_symbol_accessibility_diagnostic.borrow().clone();
        if !self.suppress_new_diagnostic_contexts.get() {
            *self.state.get_symbol_accessibility_diagnostic.borrow_mut() =
                create_get_symbol_accessibility_diagnostic_for_node(p);
        }
        let param = p.as_parameter_declaration();
        let mut question_token = None;
        if self.resolver.is_optional_parameter(p) {
            if param.question_token().is_some() {
                question_token = param.question_token();
            } else {
                question_token = Some(self.factory().new_token(Kind::QuestionToken));
            }
        }
        let result = self.factory().update_parameter_declaration(
            p,
            None,
            param.dot_dot_dot_token(),
            self.binding_name_visitor().visit_node(p.name()).unwrap(),
            question_token,
            self.ensure_type(p, true),
            self.ensure_no_initializer(p),
        );
        *self.state.get_symbol_accessibility_diagnostic.borrow_mut() = old_diag;
        result
    }

    // transform.go:2436
    pub(crate) fn ensure_no_initializer(&self, node: P<Node>) -> Option<P<Node>> {
        if self.should_print_with_initializer(node) {
            let unwrapped_initializer =
                unwrap_parenthesized_expression(node.initializer().unwrap());
            if !ast::is_primitive_literal_value(unwrapped_initializer, true) {
                self.resolver.lock(|c| self.tracker.report_inference_fallback(c, node));
            }
            return self.resolver.create_literal_const_value(
                self.emit_context().parse_node(Some(node)).unwrap(),
                self.tracker.get(),
            );
        }
        None
    }

    // transform.go:2447
    pub(crate) fn visit_binding_name(&self, node: P<Node>) -> P<Node> {
        match node.kind() {
            Kind::ArrayBindingPattern | Kind::ObjectBindingPattern => {
                self.binding_name_visitor().visit_each_child(Some(node)).unwrap()
            }
            Kind::BindingElement => {
                if let Some(property_name) = node.property_name()
                    && ast::is_computed_property_name(property_name)
                    && ast::is_entity_name_expression(property_name.expression().unwrap())
                {
                    self.check_entity_name_visibility(
                        property_name.expression().unwrap(),
                        self.enclosing_declaration.get(),
                    );
                }
                let elem = node.as_binding_element();
                self.factory().update_binding_element(
                    node,
                    elem.dot_dot_dot_token(),
                    node.property_name(),
                    self.binding_name_visitor().visit_node(node.name()),
                    None, /*initializer*/
                )
            }
            _ => node,
        }
    }

    // transform.go:2463
    pub(crate) fn transform_import_equals_declaration(&self, decl: P<Node>) -> Option<P<Node>> {
        if !self.resolver.is_declaration_visible(decl) {
            return None;
        }
        let import_equals = decl.as_import_equals_declaration();
        if import_equals.module_reference.kind() == Kind::ExternalModuleReference {
            // Rewrite external module names if necessary
            let specifier = ast::get_external_module_import_equals_declaration_expression(decl);
            Some(self.factory().update_import_equals_declaration(
                decl,
                decl.modifiers(),
                import_equals.is_type_only,
                import_equals.name,
                self.factory().update_external_module_reference(
                    import_equals.module_reference,
                    self.rewrite_module_specifier(decl, specifier).unwrap(),
                ),
            ))
        } else {
            let old_diag = self.state.get_symbol_accessibility_diagnostic.borrow().clone();
            *self.state.get_symbol_accessibility_diagnostic.borrow_mut() =
                create_get_symbol_accessibility_diagnostic_for_node(decl);
            self.check_entity_name_visibility(
                import_equals.module_reference,
                self.enclosing_declaration.get(),
            );
            *self.state.get_symbol_accessibility_diagnostic.borrow_mut() = old_diag;
            Some(decl)
        }
    }

    // transform.go:2486
    pub(crate) fn transform_import_declaration(&self, decl: P<Node>) -> Option<P<Node>> {
        let import_decl = decl.as_import_declaration();
        let Some(import_clause) = import_decl.import_clause else {
            // import "mod" - possibly needed for side effects? (global interface patches, module augmentations, etc)
            return Some(self.factory().update_import_declaration(
                decl,
                decl.modifiers(),
                import_decl.import_clause,
                self.rewrite_module_specifier(decl, Some(import_decl.module_specifier)).unwrap(),
                import_decl.attributes(),
            ));
        };
        let clause = import_clause.as_import_clause();
        let mut phase_modifier = clause.phase_modifier.get();
        if phase_modifier == Kind::DeferKeyword {
            phase_modifier = Kind::Unknown;
        }
        // The `importClause` visibility corresponds to the default's visibility.
        let mut visible_default_binding = None;
        if import_clause.name().is_some() && self.resolver.is_declaration_visible(import_clause) {
            visible_default_binding = import_clause.name();
        }
        let Some(named_bindings) = clause.named_bindings else {
            // No named bindings (either namespace or list), meaning the import is just default or should be elided
            visible_default_binding?;
            return Some(self.factory().update_import_declaration(
                decl,
                decl.modifiers(),
                Some(self.factory().update_import_clause(
                    import_clause,
                    phase_modifier,
                    visible_default_binding,
                    None, /*namedBindings*/
                )),
                self.rewrite_module_specifier(decl, Some(import_decl.module_specifier)).unwrap(),
                import_decl.attributes(),
            ));
        };
        if named_bindings.kind() == Kind::NamespaceImport {
            // Namespace import (optionally with visible default)
            let mut named_bindings_result = None;
            if self.resolver.is_declaration_visible(named_bindings) {
                named_bindings_result = Some(named_bindings);
            }
            if visible_default_binding.is_none() && named_bindings_result.is_none() {
                return None;
            }
            return Some(self.factory().update_import_declaration(
                decl,
                decl.modifiers(),
                Some(self.factory().update_import_clause(
                    import_clause,
                    phase_modifier,
                    visible_default_binding,
                    named_bindings_result,
                )),
                self.rewrite_module_specifier(decl, Some(import_decl.module_specifier)).unwrap(),
                import_decl.attributes(),
            ));
        }
        // Named imports (optionally with visible default)
        let binding_list: Vec<P<Node>> = named_bindings
            .elements()
            .iter()
            .copied()
            .filter(|&b| self.resolver.is_declaration_visible(b))
            .collect();
        if !binding_list.is_empty() || visible_default_binding.is_some() {
            let mut named_imports = None;
            if !binding_list.is_empty() {
                named_imports = Some(self.factory().update_named_imports(
                    named_bindings,
                    self.factory().new_node_list(binding_list),
                ));
            }
            return Some(self.factory().update_import_declaration(
                decl,
                decl.modifiers(),
                Some(self.factory().update_import_clause(
                    import_clause,
                    phase_modifier,
                    visible_default_binding,
                    named_imports,
                )),
                self.rewrite_module_specifier(decl, Some(import_decl.module_specifier)).unwrap(),
                import_decl.attributes(),
            ));
        }
        // Augmentation of export depends on import
        if self.resolver.is_import_required_by_augmentation(decl) {
            if self.state.isolated_declarations {
                self.state.add_diagnostic(create_diagnostic_for_node(decl, &diagnostics::DECLARATION_EMIT_FOR_THIS_FILE_REQUIRES_PRESERVING_THIS_IMPORT_FOR_AUGMENTATIONS_THIS_IS_NOT_SUPPORTED_WITH_ISOLATED_DECLARATIONS, &[]));
            }
            return Some(self.factory().update_import_declaration(
                decl,
                decl.modifiers(),
                None, /*importClause*/
                self.rewrite_module_specifier(decl, Some(import_decl.module_specifier)).unwrap(),
                import_decl.attributes(),
            ));
        }
        // Nothing visible
        None
    }

    // transform.go:2591
    pub(crate) fn transform_jsdoc_type_expression(&self, input: P<Node>) -> Option<P<Node>> {
        self.visit_fn(Some(input.as_jsdoc_type_expression().type_))
    }

    // transform.go:2595
    pub(crate) fn transform_jsdoc_type_literal(&self, input: P<Node>) -> P<Node> {
        let (members, _) =
            self.visitor().visit_slice(input.as_jsdoc_type_literal().jsdoc_property_tags);
        let replacement =
            self.factory().new_type_literal_node(self.factory().new_node_list_from_static(members));
        self.emit_context().set_original(replacement, input);
        replacement
    }

    // transform.go:2602
    pub(crate) fn transform_jsdoc_property_tag(&self, input: P<Node>) -> P<Node> {
        let tag = input.as_jsdoc_parameter_or_property_tag();
        let replacement = self.factory().new_property_signature_declaration(
            None,
            self.visit_fn(Some(tag.tag_name())).unwrap(),
            None,
            self.visit_fn(tag.type_expression),
            None,
        );
        self.emit_context().set_original(replacement, input);
        replacement
    }

    // transform.go:2614
    pub(crate) fn transform_jsdoc_all_type(&self, input: P<Node>) -> P<Node> {
        let replacement = self.factory().new_keyword_type_node(Kind::AnyKeyword);
        self.emit_context().set_original(replacement, input);
        replacement
    }

    // transform.go:2620
    pub(crate) fn transform_jsdoc_nullable_type(&self, input: P<Node>) -> P<Node> {
        let replacement = self.factory().new_union_type_node(self.factory().new_node_list(vec![
                self.visit_fn(Some(input.as_jsdoc_nullable_type().type_))
                    .unwrap(),
                self.factory().new_literal_type_node(
                    self.factory().new_keyword_expression(Kind::NullKeyword),
                ),
            ]));
        self.emit_context().set_original(replacement, input);
        replacement
    }

    // transform.go:2629
    pub(crate) fn transform_jsdoc_non_nullable_type(&self, input: P<Node>) -> Option<P<Node>> {
        self.visit_fn(Some(input.as_jsdoc_non_nullable_type().type_))
    }

    // transform.go:2633
    pub(crate) fn transform_jsdoc_variadic_type(&self, input: P<Node>) -> P<Node> {
        let replacement = self.factory().new_array_type_node(
            self.visit_fn(Some(input.as_jsdoc_variadic_type().type_)).unwrap(),
        );
        self.emit_context().set_original(replacement, input);
        replacement
    }

    // transform.go:2639
    pub(crate) fn transform_jsdoc_optional_type(&self, input: P<Node>) -> P<Node> {
        let replacement = self.factory().new_union_type_node(self.factory().new_node_list(vec![
            self.visit_fn(Some(input.as_jsdoc_optional_type().type_)).unwrap(),
            self.factory().new_keyword_type_node(Kind::UndefinedKeyword),
        ]));
        self.emit_context().set_original(replacement, input);
        replacement
    }

    // transform.go:2648
    fn get_name_expression_preferring_identifier(&self, name_expr: P<Node>) -> P<Node> {
        let mut name_expr = name_expr;
        if ast::is_numeric_literal(name_expr) {
            // Numeric property names are string properties in JS; convert to string literal
            name_expr = self.factory().new_string_literal(name_expr.text(), TokenFlags::None);
        }
        if ast::is_string_literal_like(name_expr)
            && scanner::is_identifier_text(name_expr.text(), tsrs_core::LanguageVariant::Standard)
        {
            let result = self.factory().new_identifier(name_expr.text()); // prefer non-string literal names where possible
            let kw_kind = scanner::identifier_to_keyword_kind(result);
            // keep keywords as strings, except `default`, which has special reformulations in the transformer
            if kw_kind == Kind::Unknown || kw_kind == Kind::DefaultKeyword {
                // fake this into a parse tree node so the reference resolver resolves the node via `resolveName`
                result.set_parent(name_expr.parent());
                result.set_flags(result.flags() & !NodeFlags::Synthesized);
                // intentionally leave Loc unset so the string isn't used as the text source of the identifier
                return result;
            }
        }
        name_expr
    }
}

// transform.go:2668
pub fn is_not_declare_modifier(mod_: P<Node>) -> bool {
    mod_.kind() != Kind::DeclareKeyword
}

impl DeclarationTransformer {
    // transform.go:2672
    pub(crate) fn strip_declare_modifiers(&self, node: P<Node>) -> P<Node> {
        let mods = node.modifiers();
        if let Some(mods) = mods {
            let flags = node.modifier_flags();
            if flags.intersects(ModifierFlags::Ambient) {
                let filtered: Vec<P<Node>> =
                    mods.nodes().iter().copied().filter(|&m| is_not_declare_modifier(m)).collect();
                node.as_mutable().set_modifiers(Some(self.factory().new_modifier_list(filtered)));
            }
        }
        node // no need to recur into children, only strip at top-level
    }

    // transform.go:2687
    pub(crate) fn visit_cjs_export_assignments(
        &self,
        expression: Option<P<Node>>,
    ) -> Option<P<Node>> {
        let expression = expression?;
        let (_, mut cleanup_diagnostic_context) = self.setup_diagnostic_context(expression);
        if ast::get_assignment_declaration_kind(expression) == JSDeclarationKind::ModuleExports
            && self.state.current_source_file.get().unwrap().common_js_module_indicator().is_some()
        {
            let result = Some(self.transform_export_assignment(
                expression.parent().unwrap(),
                expression,
                expression.as_binary_expression().right(),
                true, /*isExportEquals*/
            ));
            if let Some(result) = result {
                self.cjs_export_assignment.set(Some(result));
                self.result_has_scope_marker.set(true);
                self.result_has_external_module_indicator.set(true);
            }
        }
        let result = self.cjs_export_assignment_visitor().visit_each_child(Some(expression)); // recur through the whole tree, looking for module.exports=
        cleanup_diagnostic_context();
        result
    }

    // transform.go:2707
    pub(crate) fn visit_nested_expression(&self, expression: Option<P<Node>>) -> Option<P<Node>> {
        let expression = expression?;
        let (_, mut cleanup_diagnostic_context) = self.setup_diagnostic_context(expression);
        match ast::get_assignment_declaration_kind(expression) {
            JSDeclarationKind::Property => {
                self.transform_expando_assignment(expression);
            }
            JSDeclarationKind::ExportsProperty => {
                if self
                    .state
                    .current_source_file
                    .get()
                    .unwrap()
                    .common_js_module_indicator()
                    .is_some()
                {
                    let result = self.transform_common_js_export(
                        expression,
                        self.get_name_expression_preferring_identifier(
                            ast::get_element_or_property_access_name(
                                expression.as_binary_expression().left,
                            )
                            .unwrap(),
                        ),
                    );
                    if let Some(result) = result {
                        self.cjs_export_members.borrow_mut().push(result);
                    }
                }
            }
            JSDeclarationKind::ObjectDefinePropertyExports
                if self
                    .state
                    .current_source_file
                    .get()
                    .unwrap()
                    .common_js_module_indicator()
                    .is_some() =>
            {
                let result = self.transform_common_js_export(
                    expression,
                    self.get_name_expression_preferring_identifier(expression.arguments()[1]),
                );
                if let Some(result) = result {
                    self.cjs_export_members.borrow_mut().push(result);
                }
            }
            _ => {}
        }
        let result = self.expression_visitor().visit_each_child(Some(expression)); // recur through the whole tree, looking for special assignments
        cleanup_diagnostic_context();
        result
    }

    // transform.go:2734
    fn transform_expando_assignment(&self, node: P<Node>) {
        let left = node.as_binary_expression().left;

        let symbol = node.symbol();
        let Some(symbol) = symbol.filter(|s| s.flags().intersects(SymbolFlags::Assignment)) else {
            return;
        };

        let ns = ast::get_leftmost_access_expression(left);
        if ns.kind() != Kind::Identifier {
            return;
        }

        let Some(declaration) = self.resolver.get_referenced_value_declaration(ns) else {
            return;
        };

        if self.should_strip_internal(Some(declaration)) {
            return;
        }

        if ast::is_variable_declaration(declaration) && declaration.type_node().is_some() {
            return;
        }

        if ast::is_function_declaration(declaration)
            && declaration.function_like_data().unwrap().full_signature.get().is_some()
        {
            return;
        }

        if ast::is_variable_declaration(declaration)
            && !ast::is_function_like(declaration.initializer())
        {
            return; // We're going to add a type, no need to dupe members with a namespace
        }

        if declaration.symbol().is_none() {
            return;
        }
        let host = declaration.symbol().unwrap();

        let name = self.factory().new_identifier(ns.text());
        let property = self.try_get_property_name(left);
        if property.is_empty()
            || !scanner::is_identifier_text(&property, tsrs_core::LanguageVariant::Standard)
        {
            return;
        }

        let host_id = self.get_expando_host_id(declaration);

        if ast::is_declaration(declaration)
            && is_declaration_and_not_visible(self.emit_context(), self.resolver, declaration)
        {
            // The host isn't visible (yet) - printing the type of a visible declaration may still
            // late-mark it as visible (e.g. an exported variable whose type prints as `typeof host`),
            // so defer the assignment to be processed if and when that happens.
            self.deferred_expando_assignments.borrow_mut().entry(host_id).or_default().push(node);
            return;
        }

        if ast::is_function_declaration(declaration)
            && !should_emit_function_properties(declaration)
        {
            return;
        }

        self.transform_expando_host(name, declaration);

        let export_name = self.factory().new_identifier(alloc_str(&property));
        let mut local_name = self.try_get_name_of_assigned_expression(node);
        if local_name.is_none()
            && !self.resolver.is_name_resolvable(self.enclosing_declaration.get(), &property)
            && !ast::is_non_contextual_keyword(scanner::string_to_token(export_name.text()))
        {
            // use exportName as localName if there won't be any conflicts or keyword issues
            local_name = Some(export_name);
        }
        if local_name.is_none()
            || ast::is_non_contextual_keyword(scanner::string_to_token(local_name.unwrap().text()))
        {
            // fallback to a generated name if the localName doesn't exist or is a keyword
            local_name = Some(self.factory().new_generated_name_for_node(node));
        }
        let local_name = local_name.unwrap();

        let (_, mut cleanup_diagnostic_context) = self.setup_diagnostic_context(node);

        let preexisting_expando_has_export = self
            .expando_members
            .borrow()
            .get(&host_id)
            .is_some_and(|members| members.iter().any(|&m| ast::is_export_declaration(m)));

        if ast::is_identifier(node.as_binary_expression().right()) {
            if !preexisting_expando_has_export {
                self.add_export_modifier_to_expando_members(host_id);
            }
            // alias-like, emit an `export {name}` or `export {name as alias}`
            let result = self.transform_binary_expression_to_export_declaration(node, export_name);
            self.expando_members.borrow_mut().entry(host_id).or_default().push(result);
            cleanup_diagnostic_context();
            return;
        }

        let mut var_modifiers = None;

        if preexisting_expando_has_export {
            var_modifiers = Some(self.factory().new_modifier_list(
                ast::create_modifiers_from_modifier_flags(ModifierFlags::Export, |k| {
                    self.factory().new_modifier(k)
                }),
            ));
        }

        let synthesized_namespace = self.factory().new_module_declaration(
            None, /*modifiers*/
            Kind::NamespaceKeyword,
            name,
            None,
            Some(self.factory().new_module_block(self.factory().new_node_list(Vec::new()))),
        );
        synthesized_namespace.set_parent(self.enclosing_declaration.get());
        let declaration_data = synthesized_namespace.declaration_data().unwrap();
        declaration_data.set_symbol(Some(host));
        let container_data = synthesized_namespace.locals_container_data().unwrap();
        let locals = SymbolTable::new();
        container_data.set_locals(Some(locals));
        locals.set(local_name.text(), symbol);

        let old_enclosing = self.enclosing_declaration.get();
        self.enclosing_declaration.set(Some(synthesized_namespace));

        let mut statements = vec![self.factory().new_variable_statement(
            var_modifiers,
            self.factory().new_variable_declaration_list(
                self.factory().new_node_list(vec![self.factory().new_variable_declaration(
                    local_name,
                    None, /*exclamationToken*/
                    self.ensure_type(node, false),
                    None, /*initializer*/
                )]),
                NodeFlags::None,
            ),
        )];

        if local_name.text() != export_name.text() {
            let named_exports =
                self.factory().new_named_exports(self.factory().new_node_list(vec![
                    self.factory().new_export_specifier(
                        false, /*isTypeOnly*/
                        Some(local_name),
                        export_name,
                    ),
                ]));
            statements.push(self.factory().new_export_declaration(
                None,  /*modifiers*/
                false, /*isTypeOnly*/
                Some(named_exports),
                None, /*moduleSpecifier*/
                None, /*attributes*/
            ));
            if !preexisting_expando_has_export {
                // Done before adding statements to expando members to keep the initial variable statement, before we rename anything, private
                self.add_export_modifier_to_expando_members(host_id);
            }
        }
        self.expando_members.borrow_mut().entry(host_id).or_default().extend(statements);

        self.enclosing_declaration.set(old_enclosing);
        cleanup_diagnostic_context();
    }

    fn add_export_modifier_to_expando_members(&self, host_id: NodeId) {
        // Add an `export` modifier to all existing expando members so they remain exported after the `export {}` is added
        let members = self.expando_members.borrow().get(&host_id).cloned().unwrap_or_default();
        for decl in members {
            // only invoked when `self.expando_members` does not *yet* contain an `export` declaration, so no need to skip one here to prevent `export export {}`
            let modifier_flags = ModifierFlags::Export | ast::get_combined_modifier_flags(decl);
            decl.as_mutable().set_modifiers(Some(self.factory().new_modifier_list(
                ast::create_modifiers_from_modifier_flags(modifier_flags, |k| {
                    self.factory().new_modifier(k)
                }),
            )));
        }
    }

    // transform.go:2868
    fn get_expando_host_id(&self, declaration: P<Node>) -> NodeId {
        let root = if ast::is_variable_declaration(declaration) {
            declaration.parent().unwrap().parent().unwrap()
        } else {
            declaration
        };
        ast::get_node_id(self.emit_context().most_original(Some(root)).unwrap())
    }

    // transform.go:2874
    fn transform_expando_host(&self, name: P<Node>, declaration: P<Node>) {
        let root = if ast::is_variable_declaration(declaration) {
            declaration.parent().unwrap().parent().unwrap()
        } else {
            declaration
        };
        let id = self.get_expando_host_id(declaration);

        if self.expando_hosts.borrow().contains_key(&id) {
            return;
        }

        let save_needs_declare = self.needs_declare.get();
        self.needs_declare.set(true);

        let mut modifier_flags = self.ensure_modifier_flags(root);
        let default_export = modifier_flags.intersects(ModifierFlags::Export)
            && modifier_flags.intersects(ModifierFlags::Default);

        self.needs_declare.set(save_needs_declare);

        if default_export {
            modifier_flags |= ModifierFlags::Ambient;
            modifier_flags ^= ModifierFlags::Default;
            modifier_flags ^= ModifierFlags::Export;
        }

        let (_, mut cleanup_diagnostic_context) = self.setup_diagnostic_context(declaration);

        let modifiers = self.factory().new_modifier_list(
            ast::create_modifiers_from_modifier_flags(modifier_flags, |k| {
                self.factory().new_modifier(k)
            }),
        );
        let mut replacement: Vec<P<Node>> = Vec::new();

        if ast::is_function_declaration(declaration) {
            let (type_parameters, parameters, asterisk_token) =
                extract_expando_host_params(declaration);
            replacement.push(self.factory().update_function_declaration(
                declaration,
                Some(modifiers),
                asterisk_token,
                declaration.name(),
                self.ensure_type_params(declaration, type_parameters),
                Some(self.update_param_list(declaration, parameters.unwrap())),
                self.ensure_type(declaration, false),
                None, /*fullSignature*/
                None, /*body*/
            ));
        } else if ast::is_variable_declaration(declaration)
            && ast::is_function_expression_or_arrow_function(declaration.initializer().unwrap())
        {
            let fn_ = declaration.initializer().unwrap();
            let (type_parameters, parameters, asterisk_token) = extract_expando_host_params(fn_);
            replacement.push(self.factory().new_function_declaration(
                Some(modifiers),
                asterisk_token,
                Some(self.factory().new_identifier(name.text())),
                self.ensure_type_params(fn_, type_parameters),
                Some(self.update_param_list(fn_, parameters.unwrap())),
                self.ensure_type(fn_, false),
                None, /*fullSignature*/
                None, /*body*/
            ));
        } else {
            let result = self.transform_top_level_declaration(declaration);
            self.expando_hosts.borrow_mut().insert(id, result);
            cleanup_diagnostic_context();
            return;
        }

        let report_expando_function_errors =
            Rc::clone(self.state.report_expando_function_errors.get().unwrap());
        self.resolver.lock(|c| report_expando_function_errors(c, declaration));

        if default_export {
            if ast::is_source_file(declaration.parent().unwrap()) {
                self.result_has_external_module_indicator.set(true);
            }
            self.result_has_scope_marker.set(true);
            replacement.push(self.factory().new_export_assignment(
                None,  /*modifiers*/
                false, /*isExportEquals*/
                None,  /*typeNode*/
                name,
            ));
        }

        // store host result to be added to the output when it's actually visited
        self.expando_hosts
            .borrow_mut()
            .insert(id, Some(self.factory().new_syntax_list(alloc_vec(replacement))));
        let has_late_replacement = self.late_statement_replacement_map.borrow().contains_key(&id);
        if has_late_replacement {
            let block = self.create_full_expando_block(id);
            self.late_statement_replacement_map.borrow_mut().insert(id, block);
        }
        cleanup_diagnostic_context();
    }

    // transform.go:2931
    pub(crate) fn create_full_expando_block(&self, id: NodeId) -> Option<P<Node>> {
        // Process any expando assignments on this host that were skipped because it wasn't
        // visible when they were collected - if it's still not visible, they simply get
        // re-deferred, and are dropped if the host is never late-marked visible.
        let deferred = self.deferred_expando_assignments.borrow_mut().remove(&id);
        if let Some(deferred) = deferred {
            for assignment in deferred {
                self.transform_expando_assignment(assignment);
            }
        }
        let n = self.expando_hosts.borrow().get(&id).copied().flatten();
        let add_ons = self.expando_members.borrow().get(&id).cloned();
        if let Some(add_ons) = add_ons {
            let mut modifiers = None;
            let mut name = None;
            let mut host: Vec<P<Node>> = Vec::new();
            if let Some(n) = n.filter(|n| n.kind() == Kind::SyntaxList) {
                // find the first named syntax list element and use its' name & modifiers
                if let Some((c, c_name)) =
                    n.iter_children().into_iter().find_map(|c| c.name().map(|c_name| (c, c_name)))
                {
                    name = Some(c_name.clone_node(self.factory()));
                    if let Some(c_modifiers) = c.modifiers() {
                        modifiers = Some(c_modifiers.clone_list(self.factory().as_node_factory()));
                    }
                }
                host = n.as_syntax_list().children.to_vec();
            } else if let Some(n) = n {
                name = Some(n.name().unwrap().clone_node(self.factory()));
                if let Some(n_modifiers) = n.modifiers() {
                    modifiers = Some(n_modifiers.clone_list(self.factory().as_node_factory()));
                }
                host = vec![n];
            }
            if let Some(name) = name {
                let module_decl = self.factory().new_module_declaration(
                    modifiers,
                    Kind::NamespaceKeyword,
                    name,
                    None,
                    Some(self.factory().new_module_block(self.factory().new_node_list(add_ons))),
                );
                let mut members = host;
                members.push(module_decl);
                return Some(self.factory().new_syntax_list(alloc_vec(members)));
            }
        }
        n
    }
}

// transform.go:2980
pub fn extract_expando_host_params(
    node: P<Node>,
) -> (Option<P<NodeList>>, Option<P<NodeList>>, Option<P<Node>>) {
    match node.kind() {
        Kind::FunctionExpression => {
            let fn_ = node.as_function_expression();
            (fn_.type_parameters(), fn_.parameters(), fn_.asterisk_token())
        }
        Kind::ArrowFunction => {
            let fn_ = node.as_arrow_function();
            (fn_.type_parameters(), fn_.parameters(), fn_.asterisk_token())
        }
        _ => {
            let fn_ = node.as_function_declaration();
            (fn_.type_parameters(), fn_.parameters(), fn_.asterisk_token())
        }
    }
}

impl DeclarationTransformer {
    // transform.go:2994
    fn try_get_property_name(&self, node: P<Node>) -> String {
        if ast::is_element_access_expression(node) {
            return self.resolver.get_element_access_expression_name(node);
        }
        if ast::is_property_access_expression(node) {
            return node.name().unwrap().text().to_string();
        }
        String::new()
    }
}
