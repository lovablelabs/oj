use crate::*;

// transform.go lines 1340–2327.

fn optimistic() -> printer::AutoGenerateOptions {
    printer::AutoGenerateOptions {
        flags: printer::GeneratedIdentifierFlags::Optimistic,
        ..Default::default()
    }
}

impl DeclarationTransformer {
    // transform.go:1350
    pub(crate) fn transform_common_js_export(
        &self,
        input: P<Node>,
        name: P<Node>,
    ) -> Option<P<Node>> {
        let res = self.transform_common_js_export_worker(input, name)?;
        Some(self.wrap_in_cjs_export_namespace(res))
    }

    // transform.go:1358
    fn transform_common_js_export_worker(&self, input: P<Node>, name: P<Node>) -> Option<P<Node>> {
        let f = self.factory();
        let mut name_text = "";
        if ast::is_identifier(name) || ast::is_string_literal(name) {
            name_text = name.text();
        }
        if self.witnessed_cjs_exports.borrow().has(&name_text.to_string()) && !name_text.is_empty()
        {
            return None; // Already emitted this export name
        }
        self.witnessed_cjs_exports.borrow_mut().add(name_text.to_string());
        self.result_has_external_module_indicator.set(true);
        self.result_has_scope_marker.set(true);
        // only transform cjs exports to shorthand at the top-level of a source file, otherwise we uniformly emit nested exports with a type annotation
        if is_common_js_alias_export(input)
            && ast::is_expression_statement(input.parent().unwrap())
            && ast::is_source_file(input.parent().unwrap().parent().unwrap())
        {
            // export { name }
            // export { source as name }
            return Some(self.transform_binary_expression_to_export_declaration(input, name));
        }

        // Check if the RHS is a class expression - emit as a class declaration instead of a typed variable
        if ast::is_binary_expression(input) {
            let rhs = unwrap_parenthesized_expression(input.as_binary_expression().right());
            if ast::is_class_expression(rhs) {
                let class_expr_name = rhs.name();
                let has_expr_name = class_expr_name.is_some_and(|n| !n.text().is_empty());

                if has_expr_name {
                    let class_expr_name = class_expr_name.unwrap();
                    // Set up TrackSymbol watch to detect if the class expression's own
                    // symbol is referenced during member type serialization.
                    self.tracker.watched_class_symbol.set(rhs.symbol());
                    self.tracker.class_symbol_tracked.set(false);

                    let result = (|| -> P<Node> {
                        // Serialize class members using the class expression name, which
                        // triggers TrackSymbol for any self-referential member types.
                        let class_name = f.new_identifier(class_expr_name.text());
                        let class_mods = vec![f.new_modifier(Kind::ExportKeyword)];
                        let mut class_decl = self.transform_class_expression_to_declaration(
                            rhs,
                            class_name,
                            f.new_modifier_list(class_mods),
                        );
                        self.preserve_js_doc(class_decl, input);

                        // Determine if namespace isolation is needed:
                        // - The class expression name differs from the export name, OR
                        // - The class's own symbol was used in a member's serialized type
                        let names_differ =
                            !ast::is_identifier(name) || class_expr_name.text() != name.text();
                        let needs_isolation =
                            names_differ || self.tracker.class_symbol_tracked.get();

                        if needs_isolation {
                            let ns_name = f.new_unique_name_ex("_ns", optimistic());
                            let mut ns_mods = Vec::new();
                            if self.needs_declare.get() {
                                ns_mods.push(f.new_modifier(Kind::DeclareKeyword));
                            }
                            let ns_decl = f.new_module_declaration(
                                Some(f.new_modifier_list(ns_mods)),
                                Kind::NamespaceKeyword,
                                ns_name,
                                None,
                                Some(f.new_module_block(f.new_node_list(vec![class_decl]))),
                            );

                            let mut alias_base = String::from("_exported");
                            let name_text = name.text();
                            if ast::is_identifier(name)
                                && scanner::is_identifier_text(
                                    &format!("_{name_text}"),
                                    tsrs_core::LanguageVariant::Standard,
                                )
                            {
                                alias_base = format!("_{name_text}");
                            }
                            let import_alias = f.new_unique_name_ex(&alias_base, optimistic());
                            let qualified_name = f.new_qualified_name(ns_name, class_name);
                            let import_decl = f.new_import_equals_declaration(
                                None,
                                false,
                                import_alias,
                                qualified_name,
                            );

                            let export_specifier =
                                f.new_export_specifier(false, Some(import_alias), name);
                            let export_decl = f.new_export_declaration(
                                None,
                                false,
                                Some(f.new_named_exports(f.new_node_list(vec![export_specifier]))),
                                None,
                                None,
                            );
                            self.remove_all_comments(export_decl);

                            return f.new_syntax_list(alloc_vec(vec![
                                ns_decl,
                                import_decl,
                                export_decl,
                            ]));
                        }

                        // No isolation needed: names match and no self-references.
                        // Update modifiers to include declare if needed.
                        let mut mods = Vec::new();
                        mods.push(f.new_modifier(Kind::ExportKeyword));
                        if self.needs_declare.get() {
                            mods.push(f.new_modifier(Kind::DeclareKeyword));
                        }
                        let cd = class_decl.as_class_declaration();
                        class_decl = f.update_class_declaration(
                            class_decl,
                            Some(f.new_modifier_list(mods)),
                            cd.name(),
                            cd.type_parameters(),
                            cd.heritage_clauses(),
                            cd.members(),
                        );
                        class_decl
                    })();

                    // Go: deferred reset of the TrackSymbol watch.
                    self.tracker.watched_class_symbol.set(None);
                    self.tracker.class_symbol_tracked.set(false);
                    return Some(result);
                }
                let mut mods = Vec::new();
                mods.push(f.new_modifier(Kind::ExportKeyword));
                if self.needs_declare.get() {
                    mods.push(f.new_modifier(Kind::DeclareKeyword));
                }
                let mut class_name = name;
                if !ast::is_identifier(class_name) {
                    class_name = f.new_unique_name_ex("_class", optimistic());
                }
                let class_decl = self.transform_class_expression_to_declaration(
                    rhs,
                    class_name,
                    f.new_modifier_list(mods),
                );
                self.preserve_js_doc(class_decl, input);
                if !ast::is_identifier(name) {
                    // Non-identifier name: emit class declaration + named export
                    let export_decl =
                        f.new_export_declaration(
                            None,
                            false,
                            Some(f.new_named_exports(f.new_node_list(vec![
                                f.new_export_specifier(false, Some(class_name), name),
                            ]))),
                            None,
                            None,
                        );
                    self.remove_all_comments(export_decl);
                    return Some(f.new_syntax_list(alloc_vec(vec![class_decl, export_decl])));
                }
                return Some(class_decl);
            }
        }

        if ast::is_identifier(name) {
            if name.text() == "default" {
                // const _default: Type; export default _default;
                let new_id = f.new_unique_name_ex("_default", optimistic());
                *self.state.get_symbol_accessibility_diagnostic.borrow_mut() = Rc::new(
                    move |_: &SymbolAccessibilityResult| {
                        Some(P::new(SymbolAccessibilityDiagnostic {
                        diagnostic_message: &diagnostics::DEFAULT_EXPORT_OF_THE_MODULE_HAS_OR_IS_USING_PRIVATE_NAME_0,
                        error_node: Some(input),
                        type_name: None,
                    }))
                    },
                );
                self.tracker.push_error_fallback_node(Some(input));
                let type_ = self.ensure_type(input, false);
                let var_decl = f.new_variable_declaration(new_id, None, type_, None);
                self.tracker.pop_error_fallback_node();
                let mod_list = if self.needs_declare.get() {
                    f.new_modifier_list(vec![f.new_modifier(Kind::DeclareKeyword)])
                } else {
                    f.new_modifier_list(vec![])
                };
                let statement = f.new_variable_statement(
                    Some(mod_list),
                    f.new_variable_declaration_list(
                        f.new_node_list(vec![var_decl]),
                        NodeFlags::Const,
                    ),
                );

                let assignment = f.new_export_assignment(input.modifiers(), false, None, new_id);
                // Remove comments from the export declaration and copy them onto the synthetic _default declaration
                self.preserve_js_doc(statement, input);
                self.remove_all_comments(assignment);
                return Some(f.new_syntax_list(alloc_vec(vec![statement, assignment])));
            } else if self.resolver.get_referenced_value_declaration(name) == Some(input)
                || self.resolver.get_referenced_value_declaration(name).is_none()
            {
                // only inline to a export var if the `name` lookup points at this assignment or nothing - if it points at something else, we must use a temp name
                // export var name: Type
                self.tracker.push_error_fallback_node(Some(input));
                let type_ = self.ensure_type(input, false);
                let var_decl = f.new_variable_declaration(name, None, type_, None);
                self.tracker.pop_error_fallback_node();
                let mod_list = if self.needs_declare.get() {
                    f.new_modifier_list(vec![
                        f.new_modifier(Kind::ExportKeyword),
                        f.new_modifier(Kind::DeclareKeyword),
                    ])
                } else {
                    f.new_modifier_list(vec![f.new_modifier(Kind::ExportKeyword)])
                };
                return Some(f.new_variable_statement(
                    Some(mod_list),
                    f.new_variable_declaration_list(
                        f.new_node_list(vec![var_decl]),
                        NodeFlags::None,
                    ),
                ));
            }
        }
        // const _exported: Type; export {_exported as "name"};
        let new_id = f.new_unique_name_ex("_exported", optimistic());
        *self.state.get_symbol_accessibility_diagnostic.borrow_mut() =
            Rc::new(move |_: &SymbolAccessibilityResult| {
                Some(P::new(SymbolAccessibilityDiagnostic {
                    diagnostic_message:
                        &diagnostics::DEFAULT_EXPORT_OF_THE_MODULE_HAS_OR_IS_USING_PRIVATE_NAME_0,
                    error_node: Some(input),
                    type_name: None,
                }))
            });
        self.tracker.push_error_fallback_node(Some(input));
        let type_ = self.ensure_type(input, false);
        let var_decl = f.new_variable_declaration(new_id, None, type_, None);
        self.tracker.pop_error_fallback_node();
        let mod_list = if self.needs_declare.get() {
            f.new_modifier_list(vec![f.new_modifier(Kind::DeclareKeyword)])
        } else {
            f.new_modifier_list(vec![])
        };
        let statement = f.new_variable_statement(
            Some(mod_list),
            f.new_variable_declaration_list(f.new_node_list(vec![var_decl]), NodeFlags::Const),
        );

        let assignment = f.new_export_declaration(
            None,
            false,
            Some(f.new_named_exports(f.new_node_list(vec![f.new_export_specifier(
                false,
                Some(new_id),
                name,
            )]))),
            None,
            None,
        );
        // Remove comments from the export declaration and copy them onto the synthetic _default declaration
        self.preserve_js_doc(statement, input);
        self.remove_all_comments(assignment);
        Some(f.new_syntax_list(alloc_vec(vec![statement, assignment])))
    }

    // transform.go:1543
    fn wrap_in_cjs_export_namespace(&self, content: P<Node>) -> P<Node> {
        let Some(cjs_export_assignment_name) = self.cjs_export_assignment_name.get() else {
            return content;
        };
        let f = self.factory();
        // Reuse the same name node so unique names resolve consistently with the class/export
        let ns_name = cjs_export_assignment_name;
        let mut members: &'static [P<Node>] = if content.kind() == Kind::SyntaxList {
            content.as_syntax_list().children
        } else {
            alloc_vec(vec![content])
        };
        let mut ns_mods = Vec::new();
        if self.needs_declare.get() {
            ns_mods.push(f.new_modifier(Kind::DeclareKeyword));
        }
        (members, _) = self.declare_stripping_visitor().visit_slice(members);
        f.new_module_declaration(
            Some(f.new_modifier_list(ns_mods)),
            Kind::NamespaceKeyword,
            ns_name,
            None,
            Some(f.new_module_block(f.new_node_list_from_slice(members))),
        )
    }
}

// transform.go:1569
pub fn is_common_js_alias_export(node: P<Node>) -> bool {
    if ast::is_binary_expression(node)
        && ast::is_identifier(node.as_binary_expression().right())
        && let Some(symbol) = node.symbol()
        && symbol.declarations().len() == 1
    {
        return true;
    }
    false
}

impl DeclarationTransformer {
    // transform.go:1581
    // transformClassExpressionToDeclaration converts a class expression into a class declaration
    // for use in CJS export declarations (e.g., exports.K = class K {} or module.exports = class Thing {}).
    // This delegates to the shared buildClassMembers helper to stay in sync with transformClassDeclaration.
    pub(crate) fn transform_class_expression_to_declaration(
        &self,
        class_expr: P<Node>,
        class_name: P<Node>,
        modifiers: P<ModifierList>,
    ) -> P<Node> {
        let previous_enclosing_declaration = self.enclosing_declaration.get();
        self.enclosing_declaration.set(Some(class_expr));
        let previous_in_class_expression_declaration = self.in_class_expression_declaration.get();
        self.in_class_expression_declaration.set(true);

        let mut extra_members = Vec::new();
        if ast::is_in_js_file(class_expr) {
            extra_members = self.collect_this_property_assignments(class_expr);
        }
        let members = self.build_class_members(class_expr, &extra_members);
        let type_parameters =
            self.ensure_type_params(class_expr, class_expr.as_class_expression().type_parameters());
        let heritage_clauses =
            self.visitor().visit_nodes(class_expr.as_class_expression().heritage_clauses());

        let result = self.factory().new_class_declaration(
            Some(modifiers),
            Some(class_name),
            type_parameters,
            heritage_clauses,
            members,
        );
        // Go: deferred restore.
        self.enclosing_declaration.set(previous_enclosing_declaration);
        self.in_class_expression_declaration.set(previous_in_class_expression_declaration);
        result
    }

    // transform.go:1608
    pub(crate) fn rewrite_module_specifier(
        &self,
        parent: P<Node>,
        input: Option<P<Node>>,
    ) -> Option<P<Node>> {
        let input = input?;
        self.result_has_external_module_indicator.set(
            self.result_has_external_module_indicator.get()
                || (parent.kind() != Kind::ModuleDeclaration && parent.kind() != Kind::ImportType),
        );
        if ast::is_string_literal(input)
            && self.compiler_options.rewrite_relative_import_extensions.is_true()
        {
            let rewrites = self.host.content_mapper_extension_rewrites();
            let use_case_sensitive_file_names = self.host.use_case_sensitive_file_names();
            if !tspath::should_rewrite_extension(
                input.text(),
                &rewrites,
                use_case_sensitive_file_names,
            ) {
                return Some(input);
            }
            let (rewritten, changed) =
                tspath::rewrite_extension(input.text(), &rewrites, use_case_sensitive_file_names);
            if changed {
                let result = self.factory().new_string_literal(
                    alloc_str(&rewritten),
                    input.as_string_literal().token_flags(),
                );
                self.emit_context().set_original(result, input);
                self.emit_context().assign_comment_and_source_map_ranges(result, input);
                return Some(result);
            }
        }
        Some(input)
    }

    // transform.go:1616
    pub(crate) fn preserve_js_doc(&self, updated: P<Node>, original: P<Node>) {
        // Copy comment range from original to updated node so JSDoc comments are preserved
        self.emit_context().assign_comment_range(updated, original);
    }

    // transform.go:1621
    pub(crate) fn preserve_partial_js_doc(&self, updated: P<Node>, original: P<Node>) {
        if !original.flags().intersects(NodeFlags::Reparsed) {
            return;
        }
        let Some(&jsdoc) = original
            .eager_jsdoc(ast::get_source_file_of_node(original).map(tsrs_core::P::get))
            .first()
        else {
            return;
        };
        let description = scanner::get_text_of_jsdoc_comment(Some(jsdoc.as_jsdoc().comment));
        if description.is_empty() {
            return;
        }
        let comment = format!("*\n * {}\n ", description.replace('\n', "\n * "));
        self.emit_context().add_synthetic_leading_comment(
            updated,
            Kind::MultiLineCommentTrivia,
            &comment,
            true, /*hasTrailingNewLine*/
        );
    }

    // transform.go:1637
    pub(crate) fn remove_all_comments(&self, node: P<Node>) {
        self.emit_context().add_emit_flags(node, printer::EmitFlags::NoComments);
        // !!! TODO: Also remove synthetic trailing/leading comments added by transforms
        // emitNode.leadingComments = undefined;
        // emitNode.trailingComments = undefined;
    }

    // transform.go:1644
    pub(crate) fn ensure_type(&self, node: P<Node>, ignore_private: bool) -> Option<P<Node>> {
        if !ignore_private
            && self.resolver.get_effective_declaration_flags(
                self.emit_context().parse_node(Some(node)).unwrap(),
                ModifierFlags::Private,
            ) != ModifierFlags::None
        {
            // Private nodes emit no types (except private parameter properties, whose parameter types are actually visible)
            return None;
        }

        if self.should_print_with_initializer(node) {
            // Literal const declarations will have an initializer ensured rather than a type
            return None;
        }

        // Should be removed createTypeOfDeclaration will actually now reuse the existing annotation so there is no real need to duplicate type walking
        // Left in for now to minimize diff during syntactic type node builder refactor
        if !ast::is_export_assignment(node)
            && !ast::is_binding_element(node)
            && node.type_node().is_some()
            && (!ast::is_parameter_declaration(node)
                || !self.resolver.requires_adding_implicit_undefined(
                    node,
                    None,
                    self.enclosing_declaration.get(),
                ))
        {
            if self.state.current_source_file.get().unwrap().is_js() {
                // JS types have a heap of constructs we can't directly emit into .d.ts files; the node builder contains logic to remap those where possible, so we invoke it here
                // In strada we always built js declarations symbolically, so all js type nodes went through this postprocessing
                let mut js_flags = DECLARATION_EMIT_NODE_BUILDER_FLAGS;
                if self.in_class_expression_declaration.get() {
                    js_flags &= !nodebuilder::Flags::WriteClassExpressionAsTypeLiteral;
                }
                let res = self.resolver.try_js_type_node_to_type_node(
                    node.type_node().unwrap(),
                    self.enclosing_declaration.get(),
                    js_flags,
                    DECLARATION_EMIT_INTERNAL_NODE_BUILDER_FLAGS,
                    self.tracker.get(),
                );
                if res.is_some() {
                    return res;
                }
                // otherwise, fall back to full serialization
            } else {
                return self.visit_fn(node.type_node());
            }
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

        let mut flags = DECLARATION_EMIT_NODE_BUILDER_FLAGS;
        if self.in_class_expression_declaration.get() {
            flags &= !nodebuilder::Flags::WriteClassExpressionAsTypeLiteral;
        }
        let type_node = if ast::has_inferred_type(node) {
            self.resolver.create_type_of_declaration(
                node,
                self.enclosing_declaration.get(),
                flags,
                DECLARATION_EMIT_INTERNAL_NODE_BUILDER_FLAGS,
                self.tracker.get(),
            )
        } else if ast::is_function_like(node) {
            self.resolver.create_return_type_of_signature_declaration(
                node,
                self.enclosing_declaration.get(),
                flags,
                DECLARATION_EMIT_INTERNAL_NODE_BUILDER_FLAGS,
                self.tracker.get(),
            )
        } else {
            panic!("Unhandled node kind in ensureType: {:?}", node.kind());
        };

        self.state.error_name_node.set(old_error_name_node);
        if let Some(old_diag) = old_diag {
            *self.state.get_symbol_accessibility_diagnostic.borrow_mut() = old_diag;
        }
        if type_node.is_none() {
            return Some(self.factory().new_keyword_type_node(Kind::AnyKeyword));
        }
        type_node
    }

    // transform.go:1708
    pub(crate) fn should_print_with_initializer(&self, node: P<Node>) -> bool {
        can_have_literal_initializer(self.resolver, node)
            && node.initializer().is_some()
            && self.resolver.is_literal_const_declaration(
                self.emit_context().most_original(Some(node)).unwrap(),
            )
    }

    // transform.go:1712
    pub(crate) fn check_entity_name_visibility(
        &self,
        entity_name: P<Node>,
        enclosing_declaration: Option<P<Node>>,
    ) {
        let visibility_result =
            self.resolver.is_entity_name_visible(entity_name, enclosing_declaration);
        self.tracker.handle_symbol_accessibility_error(&visibility_result);
    }

    // transform.go:1718
    // Transforms the direct child of a source file into zero or more replacement statements
    pub(crate) fn transform_top_level_declaration(&self, input: P<Node>) -> Option<P<Node>> {
        if !self.state.late_marked_statements.borrow().is_empty() {
            // Remove duplicates of the current statement from the deferred work queue (this was done via orderedRemoveItem in strada - why? to ensure the same backing array? microop?)
            self.state.late_marked_statements.borrow_mut().retain(|&node| node != input);
        }
        if self.should_strip_internal(Some(input)) {
            return None;
        }
        if input.kind() == Kind::ImportEqualsDeclaration {
            return self.transform_import_equals_declaration(input);
        }
        if input.kind() == Kind::ImportDeclaration || input.kind() == Kind::JSImportDeclaration {
            let res = self.transform_import_declaration(input);
            if let Some(res) = res
                && res.kind() != Kind::ImportDeclaration
            {
                // Go: `res := res.Clone(tx.Factory()); res.Kind = ast.KindImportDeclaration`
                return Some(ast::clone_as_import_declaration(
                    res,
                    self.factory().as_node_factory(),
                ));
            }
            return res;
        }
        if ast::is_declaration(input)
            && is_declaration_and_not_visible(self.emit_context(), self.resolver, input)
        {
            return None;
        }

        // !!! TODO: JSDoc support
        // if (isJSDocImportTag(input)) return;

        // Elide implementation signatures from overload sets
        if ast::is_function_like(input) && self.resolver.is_implementation_of_overload(input) {
            return None;
        }
        let original = self.emit_context().most_original(Some(input)).unwrap();
        let id = ast::get_node_id(original);
        let is_expando_host = self.expando_hosts.borrow().contains_key(&id);
        let has_deferred_expando_assignments =
            self.deferred_expando_assignments.borrow().contains_key(&id);
        if is_expando_host || has_deferred_expando_assignments {
            return self.create_full_expando_block(id);
        }

        let previous_enclosing_declaration = self.enclosing_declaration.get();
        if is_enclosing_declaration(input) {
            self.enclosing_declaration.set(Some(input));
        }

        let can_produce_diagnostic = can_produce_diagnostics(input);
        let old_diag = self.state.get_symbol_accessibility_diagnostic.borrow().clone();
        let old_name = self.state.error_name_node.get();
        if can_produce_diagnostic {
            *self.state.get_symbol_accessibility_diagnostic.borrow_mut() =
                create_get_symbol_accessibility_diagnostic_for_node(input);
        }
        let save_needs_declare = self.needs_declare.get();

        let result = match input.kind() {
            Kind::TypeAliasDeclaration | Kind::JSTypeAliasDeclaration => {
                Some(self.transform_type_alias_declaration(input))
            }
            Kind::InterfaceDeclaration => Some(self.transform_interface_declaration(input)),
            Kind::FunctionDeclaration => Some(self.transform_function_declaration(input)),
            Kind::ModuleDeclaration => Some(self.transform_module_declaration(input)),
            Kind::ClassDeclaration => Some(self.transform_class_declaration(input)),
            Kind::VariableStatement => self.transform_variable_statement(input),
            Kind::EnumDeclaration => Some(self.transform_enum_declaration(input)),
            // Anything left unhandled is an error, so this should be unreachable
            _ => panic!("Unhandled top-level node in declaration emit: {:?}", input.kind()),
        };

        self.enclosing_declaration.set(previous_enclosing_declaration);
        *self.state.get_symbol_accessibility_diagnostic.borrow_mut() = old_diag;
        self.needs_declare.set(save_needs_declare);
        self.state.error_name_node.set(old_name);
        result
    }

    // transform.go:1798
    fn transform_type_alias_declaration(&self, input: P<Node>) -> P<Node> {
        self.needs_declare.set(false);
        let mut type_node = self.visit_fn(input.type_node());
        if type_node.is_none() {
            // stripInternal can elide an @internal type RHS; fall back like index signatures.
            type_node = Some(self.factory().new_keyword_type_node(Kind::AnyKeyword));
        }
        self.factory().update_type_alias_declaration(
            input,
            self.ensure_modifiers(input),
            input.name().unwrap(),
            self.visitor().visit_nodes(input.type_parameter_list()),
            type_node,
        )
    }

    // transform.go:1809
    fn transform_interface_declaration(&self, input: P<Node>) -> P<Node> {
        let decl = input.as_interface_declaration();
        self.factory().update_interface_declaration(
            input,
            self.ensure_modifiers(input),
            decl.name,
            self.visitor().visit_nodes(decl.type_parameters),
            self.visitor().visit_nodes(decl.heritage_clauses),
            self.visitor().visit_nodes(Some(decl.members)).unwrap(),
        )
    }

    // transform.go:1820
    fn transform_function_declaration(&self, input: P<Node>) -> P<Node> {
        if self.resolver.is_expando_function_declaration(input) {
            self.resolver
                .lock(|c| (self.state.report_expando_function_errors.get().unwrap())(c, input));
        }
        let modifiers = self.ensure_modifiers(input);
        let type_parameters = self.ensure_type_params(input, input.type_parameter_list());
        self.factory().update_function_declaration(
            input,
            modifiers,
            None,
            input.name(),
            type_parameters,
            Some(self.update_param_list(input, input.parameter_list().unwrap())),
            self.ensure_type(input, false),
            None, /*fullSignature*/
            None,
        )
    }

    // transform.go:1837
    fn transform_module_declaration(&self, input: P<Node>) -> P<Node> {
        // !!! TODO: module declarations are now parsed into nested module objects with export modifiers
        // It'd be good to collapse those back in the declaration output, but the AST can't represent the
        // `namespace a.b.c` shape for the printer (without using invalid identifier names).
        let f = self.factory();
        let mods = self.ensure_modifiers(input);
        let save_needs_declare = self.needs_declare.get();
        self.needs_declare.set(false);
        let inner = input.body();
        let mut keyword = input.as_module_declaration().keyword;
        if keyword != Kind::GlobalKeyword
            && (input.name().is_none() || !ast::is_string_literal(input.name().unwrap()))
        {
            keyword = Kind::NamespaceKeyword;
        }
        let attributes = self.visit_fn(input.attributes());

        if let Some(inner) = inner.filter(|inner| inner.kind() == Kind::ModuleBlock) {
            let old_needs_scope_fix = self.needs_scope_fix_marker.get();
            let old_has_scope_fix = self.result_has_scope_marker.get();
            self.result_has_scope_marker.set(false);
            self.needs_scope_fix_marker.set(false);
            let statements = self.visitor().visit_nodes(inner.statement_list()).unwrap();
            let mut late_statements =
                self.transform_and_replace_late_painted_statements(statements);
            if input.flags().intersects(NodeFlags::Ambient) {
                self.needs_scope_fix_marker.set(false); // If it was `declare`'d everything is implicitly exported already, ignore late printed "privates"
            }
            // With the final list of statements, there are 3 possibilities:
            // 1. There's an export assignment or export declaration in the namespace - do nothing
            // 2. Everything is exported and there are no export assignments or export declarations - strip all export modifiers
            // 3. Some things are exported, some are not, and there's no marker - add an empty marker
            if !ast::is_global_scope_augmentation(input)
                && !self.result_has_scope_marker.get()
                && !has_scope_marker(Some(late_statements))
            {
                if self.needs_scope_fix_marker.get() {
                    let mut nodes = late_statements.nodes().to_vec();
                    nodes.push(create_empty_exports(f.as_node_factory()));
                    late_statements = f.new_node_list(nodes);
                } else {
                    late_statements =
                        self.export_stripping_visitor().visit_nodes(Some(late_statements)).unwrap();
                }
            }

            let body = f.update_module_block(inner, late_statements);
            self.needs_declare.set(save_needs_declare);
            self.needs_scope_fix_marker.set(old_needs_scope_fix);
            self.result_has_scope_marker.set(old_has_scope_fix);

            return f.update_module_declaration(
                input,
                mods,
                keyword,
                input.name().unwrap(),
                attributes,
                Some(body),
            );
        }
        if let Some(inner) = inner {
            // trigger visit. ignore result (is deferred, so is just inner unless elided)
            self.visit_fn(Some(inner));
            // eagerly transform nested namespaces (the nesting doesn't need any elision or painting done)
            let original = self.emit_context().most_original(Some(inner)).unwrap();
            let id = ast::get_node_id(original);
            let body = self.late_statement_replacement_map.borrow_mut().remove(&id).flatten();
            return f.update_module_declaration(
                input,
                mods,
                keyword,
                input.name().unwrap(),
                attributes,
                body,
            );
        }
        f.update_module_declaration(input, mods, keyword, input.name().unwrap(), attributes, None)
    }

    // transform.go:1914
    pub(crate) fn strip_export_modifiers(&self, statement: P<Node>) -> P<Node> {
        let parse_node = self.emit_context().parse_node(Some(statement));
        if ast::is_import_equals_declaration(statement)
            || parse_node.is_some_and(|parse_node| {
                self.resolver.get_effective_declaration_flags(parse_node, ModifierFlags::Default)
                    != ModifierFlags::None
            })
            || !ast::can_have_modifiers(statement)
        {
            // `export import` statements should remain as-is, as imports are _not_ implicitly exported in an ambient namespace
            // Likewise, `export default` classes and the like and just be `default`, so we preserve their `export` modifiers, too
            return statement;
        }

        let old_flags = ast::get_combined_modifier_flags(statement);
        if !old_flags.intersects(ModifierFlags::Export) {
            return statement;
        }
        let new_flags = old_flags & (ModifierFlags::All ^ ModifierFlags::Export);
        let modifiers = ast::create_modifiers_from_modifier_flags(new_flags, |k| {
            self.factory().new_modifier(k)
        });
        ast::replace_modifiers(
            self.factory().as_node_factory(),
            statement,
            Some(self.factory().new_modifier_list(modifiers)),
        )
    }

    // transform.go:1937
    // buildClassMembers builds the member list for a class-like node (ClassDeclaration or ClassExpression).
    // It handles parameter properties, private identifiers, late-bound index signatures, and visited members.
    // Extra members (e.g., this-property assignments from JS files) can be passed via extraMembers.
    fn build_class_members(&self, class_node: P<Node>, extra_members: &[P<Node>]) -> P<NodeList> {
        let f = self.factory();
        let ctor = ast::get_first_constructor_with_body(class_node);
        let mut parameter_properties = Vec::new();
        if let Some(ctor) = ctor {
            let old_diag = self.state.get_symbol_accessibility_diagnostic.borrow().clone();
            for &param in ctor.parameters() {
                if !ast::has_syntactic_modifier(param, ModifierFlags::ParameterPropertyModifier)
                    || self.should_strip_internal(Some(param))
                {
                    continue;
                }
                *self.state.get_symbol_accessibility_diagnostic.borrow_mut() =
                    create_get_symbol_accessibility_diagnostic_for_node(param);
                if param.name().unwrap().kind() == Kind::Identifier {
                    let updated = f.new_property_declaration(
                        self.ensure_modifiers(param),
                        param.name().unwrap(),
                        param.question_token(),
                        self.ensure_type(param, false),
                        self.ensure_no_initializer(param),
                    );
                    self.preserve_js_doc(updated, param);
                    parameter_properties.push(updated);
                } else {
                    // Pattern - this is currently an error, but we emit declarations for it somewhat correctly
                    parameter_properties
                        .extend(self.walk_binding_pattern(param.name().unwrap(), param));
                }
            }
            *self.state.get_symbol_accessibility_diagnostic.borrow_mut() = old_diag;
        }

        // When the class has at least one private identifier, create a unique constant identifier to retain the nominal typing behavior
        // Prevents other classes with the same public members from being used in place of the current class
        let members = class_node.class_like_data().unwrap().members();
        let mut private_identifier = None;
        if members
            .nodes()
            .iter()
            .any(|member| member.name().is_some_and(ast::is_private_identifier))
        {
            private_identifier = Some(f.new_property_declaration(
                None,
                f.new_private_identifier("#private"),
                None,
                None,
                None,
            ));
        }

        let late_indexes = self.resolver.create_late_bound_index_signatures(
            class_node,
            self.enclosing_declaration.get(),
            DECLARATION_EMIT_NODE_BUILDER_FLAGS,
            DECLARATION_EMIT_INTERNAL_NODE_BUILDER_FLAGS,
            self.tracker.get(),
        );

        let mut member_nodes = Vec::with_capacity(members.nodes().len());
        if let Some(private_identifier) = private_identifier {
            member_nodes.push(private_identifier);
        }
        member_nodes.extend(late_indexes);
        member_nodes.extend(parameter_properties);
        member_nodes.extend_from_slice(extra_members);
        let visit_result = self.visitor().visit_nodes(Some(members));
        if let Some(visit_result) = visit_result.filter(|r| !r.nodes().is_empty()) {
            member_nodes.extend_from_slice(visit_result.nodes());
        }
        f.new_node_list(member_nodes)
    }

    // transform.go:1997
    fn transform_class_declaration(&self, input: P<Node>) -> P<Node> {
        let previous_enclosing_declaration = self.enclosing_declaration.get();
        self.enclosing_declaration.set(Some(input));

        self.state.error_name_node.set(input.name());
        self.tracker.push_error_fallback_node(Some(input));

        let result = (|| -> P<Node> {
            let f = self.factory();
            let decl = input.as_class_declaration();
            let modifiers = self.ensure_modifiers(input);
            let type_parameters = self.ensure_type_params(input, decl.type_parameters());

            // Collect this.x property assignments from constructors and static blocks in JS files
            let mut extra_members = Vec::new();
            if ast::is_in_js_file(input) {
                extra_members = self.collect_this_property_assignments(input);
            }

            let members = self.build_class_members(input, &extra_members);

            let extends_clause = ast::get_class_extends_heritage_element(input);

            if let Some(extends_clause) = extends_clause {
                let extends_expression =
                    extends_clause.as_expression_with_type_arguments().expression;
                if !ast::is_entity_name_expression(extends_expression)
                    && extends_expression.kind() != Kind::NullKeyword
                {
                    self.resolver.lock(|c| {
                        self.tracker.report_inference_fallback(c, extends_expression);
                    }); // Add an isolated declarations error on this extends clause
                    let mut old_id = "default";
                    if ast::node_is_present(decl.name())
                        && ast::is_identifier(decl.name().unwrap())
                        && !decl.name().unwrap().text().is_empty()
                    {
                        old_id = decl.name().unwrap().text();
                    }
                    let new_id = f.new_unique_name_ex(&format!("{old_id}_base"), optimistic());
                    let type_name = decl.name();
                    *self.state.get_symbol_accessibility_diagnostic.borrow_mut() = Rc::new(
                        move |_: &SymbolAccessibilityResult| {
                            Some(P::new(SymbolAccessibilityDiagnostic {
                            diagnostic_message: &diagnostics::X_EXTENDS_CLAUSE_OF_EXPORTED_CLASS_0_HAS_OR_IS_USING_PRIVATE_NAME_1,
                            error_node: Some(extends_clause),
                            type_name,
                        }))
                        },
                    );

                    let var_decl = f.new_variable_declaration(
                        new_id,
                        None,
                        self.resolver.create_type_of_expression(
                            extends_clause.expression().unwrap(),
                            Some(input),
                            DECLARATION_EMIT_NODE_BUILDER_FLAGS,
                            DECLARATION_EMIT_INTERNAL_NODE_BUILDER_FLAGS,
                            self.tracker.get(),
                        ),
                        None,
                    );
                    let mut mods = None;
                    if self.needs_declare.get() {
                        mods =
                            Some(f.new_modifier_list(vec![f.new_modifier(Kind::DeclareKeyword)]));
                    }
                    let statement = f.new_variable_statement(
                        mods,
                        f.new_variable_declaration_list(
                            f.new_node_list(vec![var_decl]),
                            NodeFlags::Const,
                        ),
                    );
                    let heritage_clause = extends_clause.parent().unwrap();
                    let new_heritage_clause = f.update_heritage_clause(
                        heritage_clause,
                        heritage_clause.as_heritage_clause().token,
                        f.new_node_list(vec![f.update_expression_with_type_arguments(
                            extends_clause,
                            new_id,
                            self.visitor().visit_nodes(
                                extends_clause.as_expression_with_type_arguments().type_arguments(),
                            ),
                        )]),
                    );
                    let retained_heritage_clauses =
                        self.visitor().visit_nodes(decl.heritage_clauses()); // should just be `implements`
                    let mut heritage_list = vec![new_heritage_clause];
                    if let Some(retained_heritage_clauses) =
                        retained_heritage_clauses.filter(|r| !r.nodes().is_empty())
                    {
                        heritage_list.extend_from_slice(retained_heritage_clauses.nodes());
                    }
                    let heritage_clauses = f.new_node_list(heritage_list);

                    return f.new_syntax_list(alloc_vec(vec![
                        statement,
                        f.update_class_declaration(
                            input,
                            modifiers,
                            decl.name(),
                            type_parameters,
                            Some(heritage_clauses),
                            members,
                        ),
                    ]));
                }
            }

            f.update_class_declaration(
                input,
                modifiers,
                decl.name(),
                type_parameters,
                self.visitor().visit_nodes(decl.heritage_clauses()),
                members,
            )
        })();

        // Go: deferred `PopErrorFallbackNode` and enclosing-declaration restore (LIFO).
        self.tracker.pop_error_fallback_node();
        self.enclosing_declaration.set(previous_enclosing_declaration);
        result
    }

    // transform.go:2091
    pub(crate) fn visit_this_property_assignments(&self, node: P<Node>) -> Option<P<Node>> {
        let mut is_static = false;
        let this_container = ast::get_this_container(node, false, false);
        let Some(this_target) = this_container.parent() else {
            return None; // thisContainer was source file, can't have expando-this
        };
        if ast::has_static_modifier(this_container)
            || ast::is_class_static_block_declaration(this_container)
        {
            is_static = true;
        }
        if Some(this_target) != self.enclosing_declaration.get() {
            return None; // stop searching within new `this` contexts
        }
        'case_block: {
            if ast::get_assignment_declaration_kind(node) == JSDeclarationKind::ThisProperty {
                let f = self.factory();
                let name = ast::get_name_of_declaration(node);
                let base = self.resolver.get_referenced_member_value_declaration(node);
                let key = get_this_property_assignment_key(name, node, is_static);
                if base.is_none() || self.seen_properties.borrow().has(&key) {
                    break 'case_block;
                }
                self.seen_properties.borrow_mut().add(key);
                let mut name = name.unwrap();

                // problem: this prop might be overriding a prop from a base type. The checker has special bails for override compat comparisons for binary expression properties,
                // but what we transform to won't - so we either need to match the base type (for example, if it's a getter/setter) or emit nothing
                // See `checkKindsOfPropertyMemberOverrides` in the checker for what we're trying to satisfy here
                let heritage_clauses = this_target.class_like_data().unwrap().heritage_clauses();
                if heritage_clauses.is_some_and(|h| !h.nodes().is_empty())
                    && !is_class_extending_null(Some(this_target))
                {
                    // there is a base type any assignments might be "from"
                    self.resolver.lock(|c| self.tracker.report_inference_fallback(c, this_target)); // Add an isolated declarations error on this class - we can't know how to transform this prop into an assignment without referring to type information
                    if self.resolver.is_this_property_assignment_declaration_redundant(Some(node)) {
                        break 'case_block; // skip assignments whose member is already provided by an `extends` base type (an inherited accessor/method, or an identical inherited property)
                        // TODO: If the property has an explicit `@type` annotation, we should probably emit it (maybe with an `override` modifier) instead of skipping it
                    }
                }

                let mut modifier_flags = self.ensure_modifier_flags(node);
                if is_static {
                    modifier_flags |= ModifierFlags::Static;
                }
                let mods = if modifier_flags == ModifierFlags::None {
                    None
                } else {
                    Some(f.new_modifier_list(ast::create_modifiers_from_modifier_flags(
                        modifier_flags,
                        |kind| f.new_modifier(kind),
                    )))
                };
                if ast::has_dynamic_name(node) {
                    if !is_simple_inlineable_expression(name) {
                        break 'case_block; // Member either becomes an index signature or is a reassignment
                    }
                    self.check_name(node);
                    name = f.new_computed_property_name(name); // Convert `this[foo] = expr` to `[foo]: Type`
                }
                if ast::get_text_of_property_name(name) == "constructor" {
                    break 'case_block; // `constructor` is a builtin class member, not allowed to redeclare it
                }
                if ast::is_identifier(name)
                    && !scanner::is_identifier_text(
                        name.text(),
                        tsrs_core::LanguageVariant::Standard,
                    )
                {
                    name = f.new_string_literal_from_node(name);
                }
                let prop = f.new_property_declaration(
                    mods,
                    name,
                    None,
                    self.ensure_type(node, false),
                    None,
                );
                if ast::is_expression_statement(node.parent().unwrap()) {
                    self.preserve_js_doc(prop, node.parent().unwrap());
                }
                self.this_property_assignments_collected.borrow_mut().push(prop);
            }
        }
        self.this_property_visitor().visit_each_child(Some(node))
    }
}

// transform.go:2160
pub fn is_class_extending_null(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    let Some(extends_clause) = ast::get_heritage_clause(node, Kind::ExtendsKeyword) else {
        return false;
    };
    let types = extends_clause.as_heritage_clause().types();
    if types.nodes().len() != 1 {
        return false;
    }
    let expr = types.nodes()[0].as_expression_with_type_arguments().expression;
    expr.kind() == Kind::NullKeyword
}

impl DeclarationTransformer {
    // transform.go:2178
    // collectThisPropertyAssignments finds `this.x = expr` assignments in constructors, methods, and static blocks
    // of JS classes and synthesizes PropertyDeclaration nodes for each unique property name.
    fn collect_this_property_assignments(&self, class_node: P<Node>) -> Vec<P<Node>> {
        let members = class_node.class_like_data().unwrap().members();
        let mut seen = Set::new();
        // Pre-populate seen with existing direct member nodes to avoid duplicates
        seen.m.extend(members.nodes().iter().filter_map(|&member| {
            if member.name().is_some() {
                let is_static = ast::is_static(member);
                Some(get_this_property_assignment_key(member.name(), member, is_static))
            } else {
                None
            }
        }));
        *self.seen_properties.borrow_mut() = seen;
        *self.this_property_assignments_collected.borrow_mut() = Vec::new();

        for &n in members.nodes() {
            self.this_property_visitor().visit_each_child(Some(n));
        }
        // Go: the result is read before the deferred `seenProperties.Clear()` and `thisPropertyAssignmentsCollected = nil`.
        let result = std::mem::take(&mut *self.this_property_assignments_collected.borrow_mut());
        self.seen_properties.borrow_mut().clear();
        result
    }

    // transform.go:2201
    fn walk_binding_pattern(&self, pattern: P<Node>, param: P<Node>) -> Vec<P<Node>> {
        let mut elems = Vec::new();
        elems =
            pattern.as_binding_pattern().elements.nodes().iter().fold(elems, |mut elems, &elem| {
                if ast::is_omitted_expression(elem) {
                    return elems;
                }
                if ast::is_binding_pattern(elem.name().unwrap()) {
                    elems.extend(self.walk_binding_pattern(elem.name().unwrap(), param));
                    return elems;
                }
                elems.push(self.factory().new_property_declaration(
                    self.ensure_modifiers(param),
                    elem.name().unwrap(),
                    None, /*questionOrExclamationToken*/
                    self.ensure_type(elem, false),
                    None, /*initializer*/
                ));

                elems
            });
        elems
    }

    // transform.go:2222
    fn transform_variable_statement(&self, input: P<Node>) -> Option<P<Node>> {
        let f = self.factory();
        let declaration_list = input.as_variable_statement().declaration_list;
        let mut visible = false;
        for &decl in declaration_list.as_variable_declaration_list().declarations.nodes() {
            visible = get_binding_name_visible(self.resolver, decl);
            if visible {
                break;
            }
        }
        if !visible {
            return None;
        }

        let mut input_nodes: Vec<P<Node>> =
            declaration_list.as_variable_declaration_list().declarations.nodes().to_vec();
        let mut extra_imports: &'static [P<Node>] = &[];
        if self.state.current_source_file.get().unwrap().common_js_module_indicator().is_some() {
            let mut normal_declarations = Vec::new();
            let mut imports = Vec::new();
            (imports, normal_declarations) = input_nodes.iter().fold(
                (imports, normal_declarations),
                |(mut imports, mut normal_declarations), &n| {
                    if ast::is_variable_declaration_initialized_to_require(n) {
                        imports.push(n);
                    } else {
                        normal_declarations.push(n);
                    }

                    (imports, normal_declarations)
                },
            );
            input_nodes = normal_declarations;
            (extra_imports, _) = self.visitor().visit_slice(alloc_vec(imports));
        }

        let (nodes, _) = self.visitor().visit_slice(alloc_vec(input_nodes));
        if nodes.is_empty() {
            if !extra_imports.is_empty() {
                return Some(f.new_syntax_list(extra_imports));
            }
            return None;
        }
        let node_list = f.new_node_list_from_slice(nodes);

        let modifiers = self.ensure_modifiers(input);

        let decl_list;
        if ast::is_var_using(declaration_list) || ast::is_var_await_using(declaration_list) {
            decl_list = f.new_variable_declaration_list(node_list, NodeFlags::Const);
            self.emit_context().set_original(decl_list, declaration_list);
            self.emit_context().set_comment_range(decl_list, declaration_list.loc());
            decl_list.set_loc(declaration_list.loc());
        } else {
            decl_list = f.update_variable_declaration_list(
                declaration_list,
                node_list,
                declaration_list.flags(),
            );
        }
        let res = f.update_variable_statement(input, modifiers, decl_list);
        if !extra_imports.is_empty() {
            let mut list = extra_imports.to_vec();
            list.push(res);
            return Some(f.new_syntax_list(alloc_vec(list)));
        }
        Some(res)
    }

    // transform.go:2277
    fn transform_enum_declaration(&self, input: P<Node>) -> P<Node> {
        let f = self.factory();
        let decl = input.as_enum_declaration();
        let modifiers = self.ensure_modifiers(input);
        let mut members = Vec::new();
        for &m in decl.members.nodes() {
            if self.should_strip_internal(Some(m)) {
                continue;
            }

            // Rewrite enum values to their constants, if available
            let enum_value = self.resolver.get_enum_member_value(m);

            if self.state.isolated_declarations && m.initializer().is_some() && enum_value.has_external_references &&
                // This will be its own compiler error instead, so don't report.
                !ast::is_computed_property_name(m.name().unwrap())
            {
                self.state.add_diagnostic(create_diagnostic_for_node(m, &diagnostics::ENUM_MEMBER_INITIALIZERS_MUST_BE_COMPUTABLE_WITHOUT_REFERENCES_TO_EXTERNAL_SYMBOLS_WITH_ISOLATED_DECLARATIONS, &[]));
            }

            let new_initializer = match enum_value.value {
                Some(checker::LiteralValue::Number(value)) => {
                    if value.is_inf() {
                        if value.0 > 0.0 {
                            Some(f.new_identifier("Infinity"))
                        } else {
                            Some(f.new_prefix_unary_expression(
                                Kind::MinusToken,
                                f.new_identifier("Infinity"),
                            ))
                        }
                    } else if value.is_nan() {
                        Some(f.new_identifier("NaN"))
                    } else if value.0 >= 0.0 {
                        Some(f.new_numeric_literal(alloc_str(&value.string()), TokenFlags::None))
                    } else {
                        Some(f.new_prefix_unary_expression(
                            Kind::MinusToken,
                            f.new_numeric_literal(alloc_str(&(-value).string()), TokenFlags::None),
                        ))
                    }
                }
                Some(checker::LiteralValue::String(value)) => {
                    Some(f.new_string_literal(value, TokenFlags::None))
                }
                // nil
                _ => None,
            };
            let result = f.update_enum_member(m, m.name().unwrap(), new_initializer);
            self.preserve_js_doc(result, m);
            members.push(result);
        }
        f.update_enum_declaration(input, modifiers, decl.name, f.new_node_list(members))
    }
}
