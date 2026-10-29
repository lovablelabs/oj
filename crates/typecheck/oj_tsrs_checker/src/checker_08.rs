use crate::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::fmt::Display;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

// Non-function declarations in checker.go:15239-17432 (hand-ported by checker-foundation):
//   type ExportCollision (checker.go:16464)
//   type ExportCollisionTable (checker.go:16469)

impl Checker {
    // checker.go:15239
    pub(crate) fn get_target_of_alias_like_expression(
        &mut self,
        expression: P<Node>,
    ) -> Option<P<Symbol>> {
        if ast::is_class_expression(expression) {
            return self.check_expression_cached(expression).symbol();
        }
        if !ast::is_entity_name(expression) && !ast::is_entity_name_expression(expression) {
            return None;
        }
        let alias_like = self.resolve_entity_name(
            expression,
            SymbolFlags::Value | SymbolFlags::Type | SymbolFlags::Namespace,
            true, /*ignoreErrors*/
            true, /*dontResolveAlias*/
            None, /*location*/
        );
        if alias_like.is_some() {
            return alias_like;
        }
        self.check_expression_cached(expression);
        self.get_resolved_symbol_or_nil(expression)
    }

    // checker.go:15254
    fn get_target_of_namespace_export_declaration(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        let parent = node.parent().unwrap();
        if ast::can_have_symbol(parent) {
            let resolved = parent
                .symbol()
                .map(|s| self.resolve_external_module_symbol(s, true /*dontResolveAlias*/));
            self.mark_symbol_of_alias_declaration_if_type_only(Some(node), None);
            return resolved;
        }
        None
    }

    // checker.go:15263
    fn get_target_of_access_expression(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        let parent = node.parent().unwrap();
        if ast::is_binary_expression(parent) {
            let expr = parent.as_binary_expression();
            if expr.left() == node && expr.operator_token().kind() == Kind::EqualsToken {
                return self.get_target_of_alias_like_expression(expr.right());
            }
        }
        None
    }

    // checker.go:15273
    pub(crate) fn get_module_specifier_for_import_or_export(node: P<Node>) -> Option<P<Node>> {
        match node.kind() {
            Kind::ImportEqualsDeclaration => {
                let module_reference = node.as_import_equals_declaration().module_reference();
                if ast::is_external_module_reference(module_reference) {
                    module_reference.expression()
                } else {
                    None
                }
            }
            Kind::NamespaceImport | Kind::ExportSpecifier => {
                get_module_specifier_from_node(node.parent().unwrap().parent().unwrap())
            }
            Kind::ImportSpecifier => get_module_specifier_from_node(
                node.parent().unwrap().parent().unwrap().parent().unwrap(),
            ),
            Kind::ImportClause | Kind::NamespaceExport => {
                get_module_specifier_from_node(node.parent().unwrap())
            }
            _ => panic!("Unhandled case in getModuleSpecifierForImportOrExport"),
        }
    }
}

// checker.go:15295
pub(crate) fn get_module_specifier_from_node(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::ImportDeclaration | Kind::JSImportDeclaration | Kind::ExportDeclaration => {
            node.module_specifier()
        }
        _ => panic!("Unhandled case in getModuleSpecifierFromNode: {}", node.kind_string()),
    }
}

impl Checker {
    /**
     * Marks a symbol as type-only if its declaration is syntactically type-only.
     * If it is not itself marked type-only, but resolves to a type-only alias
     * somewhere in its resolution chain, save a reference to the type-only alias declaration
     * so the alias _not_ marked type-only can be identified as _transitively_ type-only.
     *
     * This function is called on each alias declaration that could be type-only or resolve to
     * another type-only alias during `resolveAlias`, so that later, when an alias is used in a
     * JS-emitting expression, we can quickly determine if that symbol is effectively type-only
     * and issue an error if so.
     */
    // checker.go:15326
    pub(crate) fn mark_symbol_of_alias_declaration_if_type_only(
        &mut self,
        alias_declaration: Option<P<Node>>,
        export_star_declaration: Option<P<Node>>,
    ) -> bool {
        let alias_declaration = match alias_declaration {
            Some(d) if ast::is_declaration_node(d) => d,
            _ => return false,
        };
        // If the declaration itself is type-only, mark it and return. No need to check what it resolves to.
        let source_symbol = self.get_symbol_of_declaration(alias_declaration).unwrap();
        let links = self.alias_symbol_links.get(source_symbol);
        if links.type_only_declaration.get().is_none()
            && ast::is_type_only_import_or_export_declaration(alias_declaration)
        {
            links.type_only_declaration.set(Some(alias_declaration));
            return true;
        }
        if links.type_only_declaration.get().is_none() && export_star_declaration.is_some() {
            links.type_only_declaration.set(export_star_declaration);
            return true;
        }
        links.type_only_declaration.get().is_some()
    }

    // checker.go:15344
    pub(crate) fn resolve_external_module_name(
        &mut self,
        location: P<Node>,
        module_reference_expression: P<Node>,
        ignore_errors: bool,
        import_attributes_type: Option<P<Type>>,
    ) -> Option<P<Symbol>> {
        let error_message = self
            .get_cannot_resolve_module_name_error_for_specific_module(module_reference_expression)
            .unwrap_or(&diagnostics::CANNOT_FIND_MODULE_0_OR_ITS_CORRESPONDING_TYPE_DECLARATIONS);
        let ignore_errors = ignore_errors || self.compiler_options.no_check.is_true();
        self.resolve_external_module_name_worker(
            Some(location),
            Some(module_reference_expression),
            if ignore_errors { None } else { Some(error_message) },
            ignore_errors,
            false, /*isForAugmentation*/
            import_attributes_type,
        )
    }

    // checker.go:15353
    fn get_cannot_resolve_module_name_error_for_specific_module(
        &self,
        module_name: P<Node>,
    ) -> Option<&'static Message> {
        if ast::is_string_literal(module_name) && node_core_modules().contains(module_name.text()) {
            if self.compiler_options.uses_wildcard_types() {
                return Some(&diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_NODE_TRY_NPM_I_SAVE_DEV_TYPES_SLASHNODE);
            }
            return Some(&diagnostics::CANNOT_FIND_NAME_0_DO_YOU_NEED_TO_INSTALL_TYPE_DEFINITIONS_FOR_NODE_TRY_NPM_I_SAVE_DEV_TYPES_SLASHNODE_AND_THEN_ADD_NODE_TO_THE_TYPES_FIELD_IN_YOUR_TSCONFIG);
        }
        None
    }

    // checker.go:15365
    pub(crate) fn resolve_external_module_name_worker(
        &mut self,
        location: Option<P<Node>>,
        module_reference_expression: Option<P<Node>>,
        module_not_found_error: Option<&'static Message>,
        ignore_errors: bool,
        is_for_augmentation: bool,
        import_attributes_type: Option<P<Type>>,
    ) -> Option<P<Symbol>> {
        if let Some(module_reference_expression) = module_reference_expression
            && ast::is_string_literal_like(module_reference_expression)
        {
            if module_reference_expression.parent().is_some_and(ast::is_source_phase_import) {
                return None;
            }
            return self.resolve_external_module(
                location,
                module_reference_expression.text(),
                module_not_found_error,
                if ignore_errors { None } else { Some(module_reference_expression) },
                is_for_augmentation,
                import_attributes_type,
            );
        }
        None
    }

    // checker.go:15372
    pub(crate) fn get_external_module_file_from_declaration(
        &mut self,
        declaration: P<Node>,
    ) -> Option<P<SourceFile>> {
        let mut specifier: Option<P<Node>> = None;
        if declaration.kind() == Kind::ModuleDeclaration {
            if ast::is_string_literal(declaration.name().unwrap()) {
                specifier = declaration.name();
            }
        } else {
            specifier = ast::get_external_module_name(declaration);
        }
        let mut import_attributes_type: Option<P<Type>> = None;
        if ast::has_import_attributes(declaration) {
            import_attributes_type =
                self.get_type_from_import_attributes(ast::get_import_attributes(declaration));
        }
        // This is only used by emit and type printing, after checking has already reported any
        // resolution errors for this specifier. Resolve with ignoreErrors so that these queries
        // don't add new diagnostics (e.g. an implicit-any-module suggestion) as a side effect.
        let module_symbol = self.resolve_external_module_name_worker(
            specifier,
            specifier,
            None,  /*moduleNotFoundError*/
            true,  /*ignoreErrors*/
            false, /*isForAugmentation*/
            import_attributes_type,
        )?;
        let decl = ast::get_declaration_of_kind(module_symbol, Kind::SourceFile)?;
        Some(decl.as_source_file_p())
    }

    // tsrs-only: the program's lookup turns the name into a path (normalized, a new string, lowercased where the file
    // system ignores case) and hashes it, per import of every resolution; its tables do not change while checking.
    fn get_source_file_for_resolved_module(
        &mut self,
        file_name: &'static str,
    ) -> Option<P<SourceFile>> {
        let key = (file_name.as_ptr() as usize, file_name.len());
        *self
            .resolved_module_source_files
            .entry(key)
            .or_insert_with(|| self.program.get_source_file_for_resolved_module(file_name))
    }

    // checker.go:15399
    pub(crate) fn resolve_external_module(
        &mut self,
        location: Option<P<Node>>,
        module_reference: &str,
        module_not_found_error: Option<&'static Message>,
        error_node: Option<P<Node>>,
        is_for_augmentation: bool,
        import_attributes_type: Option<P<Type>>,
    ) -> Option<P<Symbol>> {
        if error_node.is_some() && module_reference.starts_with("@types/") {
            let without_at_type_prefix = &module_reference["@types/".len()..];
            self.error(error_node, &diagnostics::CANNOT_IMPORT_TYPE_DECLARATION_FILES_CONSIDER_IMPORTING_0_INSTEAD_OF_1, &[&without_at_type_prefix, &module_reference]);
        }
        let import_attributes_type = import_attributes_type.unwrap_or(self.empty_object_type);

        let ambient_module =
            self.try_find_ambient_module(module_reference, true /*withAugmentations*/);
        if ambient_module.is_some() {
            return self.try_resolve_pattern_ambient_module(
                ambient_module,
                module_reference,
                import_attributes_type,
            );
        }

        let location = location.unwrap();
        let importing_source_file = ast::get_source_file_of_node(location).unwrap();
        let mut context_specifier: Option<P<Node>> = None;

        if ast::is_string_literal_like(location)
            || location.parent().is_some()
                && ast::is_module_declaration(location.parent().unwrap())
                && location.parent().unwrap().as_module_declaration().name() == location
        {
            context_specifier = Some(location);
        } else if ast::is_module_declaration(location) {
            context_specifier = Some(location.as_module_declaration().name());
        } else if ast::is_literal_import_type_node(location) {
            context_specifier =
                Some(location.as_import_type_node().argument.as_literal_type_node().literal);
        } else if ast::is_variable_declaration_initialized_to_bare_or_accessed_require(location) {
            context_specifier = ast::get_module_specifier_of_bare_or_accessed_require(location);
        } else {
            let mut ancestor = ast::find_ancestor(location, ast::is_import_call);
            if let Some(a) = ancestor {
                context_specifier = Some(a.arguments()[0]);
            }

            if ancestor.is_none() {
                ancestor = ast::find_ancestor(
                    location,
                    ast::is_import_declaration_or_js_import_declaration,
                );
                if let Some(a) = ancestor {
                    context_specifier = a.module_specifier();
                }
            }
            if ancestor.is_none() {
                ancestor = ast::find_ancestor(location, ast::is_export_declaration);
                if let Some(a) = ancestor {
                    context_specifier = a.module_specifier();
                }
            }
            if ancestor.is_none() {
                ancestor = ast::find_ancestor(location, ast::is_import_equals_declaration);
                if let Some(a) = ancestor {
                    let module_refrence = a.as_import_equals_declaration().module_reference();
                    if module_refrence.kind() == Kind::ExternalModuleReference {
                        context_specifier = module_refrence.expression();
                    }
                }
            }
        }

        let mode: ResolutionMode = match context_specifier {
            Some(cs) if ast::is_string_literal_like(cs) => {
                self.program.get_mode_for_usage_location(importing_source_file, cs)
            }
            _ => self.program.get_default_resolution_mode_for_file(importing_source_file),
        };

        let resolved_module =
            self.program.get_resolved_module(importing_source_file, module_reference, mode);
        let is_resolved = resolved_module.is_some_and(|r| r.is_resolved());

        let mut resolution_diagnostic: Option<&'static Message> = None;
        if error_node.is_some() && is_resolved {
            resolution_diagnostic = tsrs_module::get_resolution_diagnostic(
                &self.compiler_options,
                &resolved_module.unwrap(),
                importing_source_file,
            );
        }

        let mut source_file: Option<P<SourceFile>> = None;
        if is_resolved
            && (resolution_diagnostic.is_none()
                || std::ptr::eq(
                    resolution_diagnostic.unwrap(),
                    &raw const diagnostics::MODULE_0_WAS_RESOLVED_TO_1_BUT_JSX_IS_NOT_SET,
                ))
        {
            source_file = self
                .get_source_file_for_resolved_module(resolved_module.unwrap().resolved_file_name);
        }

        if let Some(source_file) = source_file {
            let resolved_module = resolved_module.unwrap();
            // If there's a resolutionDiagnostic we need to report it even if a sourceFile is found.
            if let Some(resolution_diagnostic) = resolution_diagnostic {
                self.error(
                    error_node,
                    resolution_diagnostic,
                    &[&module_reference, &resolved_module.resolved_file_name],
                );
            }

            if error_node.is_some() && !resolved_module.is_custom_resolution {
                let rewrites = self
                    .program
                    .as_module_specifier_generation_host()
                    .content_mapper_extension_rewrites();
                let case_sensitive = self.program.use_case_sensitive_file_names();
                let (_, mapped_specifier) =
                    tspath::rewrite_extension(module_reference, &rewrites, case_sensitive);
                let check_declaration_rewrite = self.compiler_options.get_emit_declarations()
                    && mapped_specifier
                    && tspath::path_is_relative(module_reference)
                    && !importing_source_file.is_declaration_file();
                if resolved_module.resolved_using_ts_extension
                    && tspath::is_declaration_file_name(module_reference)
                {
                    if ast::find_ancestor(location, ast::is_emittable_import).is_some() {
                        let ts_extension = tspath::try_extract_ts_extension(module_reference);
                        assert!(
                            !ts_extension.is_empty(),
                            "should be able to extract TS extension from string that passes IsDeclarationFileName"
                        );
                        let suggested =
                            self.get_suggested_import_source(module_reference, ts_extension, mode);
                        self.error(error_node, &diagnostics::A_DECLARATION_FILE_CANNOT_BE_IMPORTED_WITHOUT_IMPORT_TYPE_DID_YOU_MEAN_TO_IMPORT_AN_IMPLEMENTATION_FILE_0_INSTEAD, &[&suggested]);
                    }
                } else if resolved_module.resolved_using_ts_extension
                    && !self
                        .compiler_options
                        .allow_importing_ts_extensions_from(importing_source_file.file_name())
                {
                    if ast::find_ancestor(location, ast::is_emittable_import).is_some() {
                        let mut ts_extension = tspath::try_extract_ts_extension(module_reference);
                        if ts_extension.is_empty() {
                            // Fallback: do a best-effort extraction using strings.Contains.
                            // This handles cases where a wildcard pattern matches a TS extension that's
                            // not at the end of the module specifier, e.g., "#/foo.ts.omg" through "#/*.omg": "./src/*"
                            if let Some(ext) = tspath::SUPPORTED_TS_EXTENSIONS_FLAT
                                .iter()
                                .find(|&&ext| module_reference.contains(ext))
                            {
                                ts_extension = ext;
                            }
                        }
                        assert!(
                            !ts_extension.is_empty(),
                            "should be able to extract TS extension from string when resolvedUsingTsExtension is true"
                        );
                        self.error(error_node, &diagnostics::AN_IMPORT_PATH_CAN_ONLY_END_WITH_A_0_EXTENSION_WHEN_ALLOW_IMPORTING_TS_EXTENSIONS_IS_ENABLED, &[&ts_extension]);
                    }
                } else if self.compiler_options.rewrite_relative_import_extensions.is_true()
                    && (!location.flags().intersects(NodeFlags::Ambient)
                        || check_declaration_rewrite)
                    && !tspath::is_declaration_file_name(module_reference)
                    && ((!ast::is_literal_import_type_node(location)
                        && !ast::is_part_of_type_only_import_or_export_declaration(location))
                        || check_declaration_rewrite)
                {
                    let should_rewrite = tspath::should_rewrite_extension(
                        module_reference,
                        &rewrites,
                        case_sensitive,
                    );
                    let mapped_source = !source_file.content_mapper().is_empty()
                        && tspath::rewrite_extension(
                            source_file.file_name(),
                            &rewrites,
                            case_sensitive,
                        )
                        .1;
                    let resolved_using_rewritable_extension = resolved_module
                        .resolved_using_ts_extension
                        || (resolved_module.resolved_using_extra_extensions
                            && !source_file.content_mapper().is_empty()
                            && mapped_source);
                    if !resolved_using_rewritable_extension && should_rewrite {
                        let relative_to_source_file = tspath::get_relative_path_from_file(
                            &tspath::get_normalized_absolute_path(
                                importing_source_file.file_name(),
                                &self.program.get_current_directory(),
                            ),
                            resolved_module.resolved_file_name,
                            &tspath::ComparePathsOptions {
                                use_case_sensitive_file_names: self
                                    .program
                                    .use_case_sensitive_file_names(),
                                current_directory: self.program.get_current_directory(),
                            },
                        );
                        self.error(error_node, &diagnostics::THIS_RELATIVE_IMPORT_PATH_IS_UNSAFE_TO_REWRITE_BECAUSE_IT_LOOKS_LIKE_A_FILE_NAME_BUT_ACTUALLY_RESOLVES_TO_0, &[&relative_to_source_file]);
                    } else if resolved_using_rewritable_extension
                        && !should_rewrite
                        && (mapped_source
                            || self.program.source_file_may_be_emitted(source_file, false))
                    {
                        let ext = if mapped_source {
                            tspath::get_extension_rewrite(
                                source_file.file_name(),
                                &rewrites,
                                case_sensitive,
                            )
                            .expect("mapped source must have an extension rewrite")
                            .source
                            .clone()
                        } else {
                            tspath::get_any_extension_from_path(module_reference, &[], false)
                        };
                        self.error(error_node, &diagnostics::THIS_IMPORT_USES_A_0_EXTENSION_TO_RESOLVE_TO_AN_INPUT_TYPE_SCRIPT_FILE_BUT_WILL_NOT_BE_REWRITTEN_DURING_EMIT_BECAUSE_IT_IS_NOT_A_RELATIVE_PATH, &[&ext]);
                    } else if resolved_using_rewritable_extension
                        && should_rewrite
                        && let Some(redirect) =
                            self.program.get_redirect_for_resolution(source_file)
                    {
                        let own_root_dir = self.program.common_source_directory();
                        let other_root_dir = redirect.common_source_directory();

                        let compare_options = tspath::ComparePathsOptions {
                            use_case_sensitive_file_names: self
                                .program
                                .use_case_sensitive_file_names(),
                            current_directory: self.program.get_current_directory(),
                        };

                        let root_dir_path = tspath::get_relative_path_from_directory(
                            &own_root_dir,
                            &other_root_dir,
                            &compare_options,
                        );

                        // Get outDir paths, defaulting to root directories if not specified
                        let mut own_out_dir = self.compiler_options.out_dir.clone();
                        if own_out_dir.is_empty() {
                            own_out_dir = own_root_dir;
                        }
                        let mut other_out_dir =
                            redirect.compiler_options().unwrap().out_dir.clone();
                        if other_out_dir.is_empty() {
                            other_out_dir = other_root_dir;
                        }
                        let out_dir_path = tspath::get_relative_path_from_directory(
                            &own_out_dir,
                            &other_out_dir,
                            &compare_options,
                        );

                        if root_dir_path != out_dir_path {
                            self.error(error_node, &diagnostics::THIS_IMPORT_PATH_IS_UNSAFE_TO_REWRITE_BECAUSE_IT_RESOLVES_TO_ANOTHER_PROJECT_AND_THE_RELATIVE_PATH_BETWEEN_THE_PROJECTS_OUTPUT_FILES_IS_NOT_THE_SAME_AS_THE_RELATIVE_PATH_BETWEEN_ITS_INPUT_FILES, &[]);
                        }
                    }
                }
            }

            if let Some(source_file_symbol) = source_file.symbol() {
                if let Some(error_node) = error_node {
                    if resolved_module.is_external_library_import
                        && !resolution_extension_is_ts_or_json(resolved_module.extension)
                    {
                        self.error_on_implicit_any_module(
                            false, /*isError*/
                            error_node,
                            mode,
                            resolved_module,
                            module_reference,
                        );
                    }
                    if self.module_kind == ModuleKind::NODE16
                        || self.module_kind == ModuleKind::NODE18
                    {
                        let is_sync_import = self
                            .program
                            .get_default_resolution_mode_for_file(importing_source_file)
                            == ModuleKind::COMMON_JS
                            && ast::find_ancestor(location, ast::is_import_call).is_none()
                            || ast::find_ancestor(location, ast::is_import_equals_declaration)
                                .is_some();
                        let override_host =
                            ast::find_ancestor(location, ast::is_resolution_mode_override_host);
                        if is_sync_import
                            && self.program.get_default_resolution_mode_for_file(source_file)
                                == ModuleKind::ES_NEXT
                            && !ast::has_resolution_mode_override(override_host)
                        {
                            if ast::find_ancestor_kind(location, Kind::ImportEqualsDeclaration)
                                .is_some()
                            {
                                // ImportEquals in an ESM file resolving to another ESM file
                                self.error(Some(error_node), &diagnostics::MODULE_0_CANNOT_BE_IMPORTED_USING_THIS_CONSTRUCT_THE_SPECIFIER_ONLY_RESOLVES_TO_AN_ES_MODULE_WHICH_CANNOT_BE_IMPORTED_WITH_REQUIRE_USE_AN_ECMA_SCRIPT_IMPORT_INSTEAD, &[&module_reference]);
                            } else {
                                // CJS file resolving to an ESM file
                                let mut diagnostic_details: Option<P<Diagnostic>> = None;
                                let ext = tspath::try_get_extension_from_path(
                                    importing_source_file.file_name(),
                                );
                                if ext == tspath::EXTENSION_TS
                                    || ext == tspath::EXTENSION_JS
                                    || ext == tspath::EXTENSION_TSX
                                    || ext == tspath::EXTENSION_JSX
                                {
                                    diagnostic_details = Some(self.create_mode_mismatch_details(
                                        importing_source_file,
                                        error_node,
                                    ));
                                }

                                let message: &'static Message = if override_host.is_some_and(|h| {
                                    h.kind() == Kind::ImportDeclaration
                                        && h.import_clause().is_some()
                                        && h.import_clause().unwrap().is_type_only()
                                }) {
                                    &diagnostics::TYPE_ONLY_IMPORT_OF_AN_ECMA_SCRIPT_MODULE_FROM_A_COMMON_JS_MODULE_MUST_HAVE_A_RESOLUTION_MODE_ATTRIBUTE
                                } else if override_host
                                    .is_some_and(|h| h.kind() == Kind::ImportType)
                                {
                                    &diagnostics::TYPE_IMPORT_OF_AN_ECMA_SCRIPT_MODULE_FROM_A_COMMON_JS_MODULE_MUST_HAVE_A_RESOLUTION_MODE_ATTRIBUTE
                                } else {
                                    &diagnostics::THE_CURRENT_FILE_IS_A_COMMON_JS_MODULE_WHOSE_IMPORTS_WILL_PRODUCE_REQUIRE_CALLS_HOWEVER_THE_REFERENCED_FILE_IS_AN_ECMA_SCRIPT_MODULE_AND_CANNOT_BE_IMPORTED_WITH_REQUIRE_CONSIDER_WRITING_A_DYNAMIC_IMPORT_0_CALL_INSTEAD
                                };

                                self.add_diagnostic(new_diagnostic_chain_for_node(
                                    diagnostic_details,
                                    error_node,
                                    Some(message),
                                    &[&module_reference],
                                ));
                            }
                        }
                    }
                }
                let merged = self.get_merged_symbol(source_file_symbol);
                return self.try_resolve_pattern_ambient_module(
                    Some(merged),
                    module_reference,
                    import_attributes_type,
                );
            }
            let pattern_ambient_module = self.try_resolve_pattern_ambient_module(
                None, /*resolvedSymbol*/
                module_reference,
                import_attributes_type,
            );
            if pattern_ambient_module.is_some() {
                return pattern_ambient_module;
            }
            if let Some(error_node) = error_node
                && module_not_found_error.is_some()
                && !is_side_effect_import(error_node)
            {
                self.error(
                    Some(error_node),
                    &diagnostics::FILE_0_IS_NOT_A_MODULE,
                    &[&resolved_module.resolved_file_name],
                );
            }
            return None;
        }

        let pattern_ambient_module = self.try_resolve_pattern_ambient_module(
            None, /*resolvedSymbol*/
            module_reference,
            import_attributes_type,
        );
        if pattern_ambient_module.is_some() {
            return pattern_ambient_module;
        }

        let error_node = error_node?;

        if is_resolved && !resolution_extension_is_ts_or_json(resolved_module.unwrap().extension) && resolution_diagnostic.is_none()
            || resolution_diagnostic.is_some_and(|d| std::ptr::eq(d, &raw const diagnostics::COULD_NOT_FIND_A_DECLARATION_FILE_FOR_MODULE_0_1_IMPLICITLY_HAS_AN_ANY_TYPE))
        {
            let resolved_module = resolved_module.unwrap();
            if is_for_augmentation {
                self.error(Some(error_node), &diagnostics::INVALID_MODULE_NAME_IN_AUGMENTATION_MODULE_0_RESOLVES_TO_AN_UNTYPED_MODULE_AT_1_WHICH_CANNOT_BE_AUGMENTED, &[&module_reference, &resolved_module.resolved_file_name]);
            } else {
                let is_error = self.no_implicit_any && module_not_found_error.is_some();
                self.error_on_implicit_any_module(is_error, error_node, mode, resolved_module, module_reference);
            }
            return None;
        }

        if let Some(module_not_found_error) = module_not_found_error {
            // See if this was possibly a projectReference redirect
            if is_resolved {
                let resolved_module = resolved_module.unwrap();
                let path = tspath::to_path(
                    resolved_module.resolved_file_name,
                    &self.program.get_current_directory(),
                    self.program.use_case_sensitive_file_names(),
                );
                let redirect = self.program.get_project_reference_from_source(&path);
                if let Some(redirect) = redirect
                    && !redirect.output_dts.is_empty()
                {
                    self.error(
                        Some(error_node),
                        &diagnostics::OUTPUT_FILE_0_HAS_NOT_BEEN_BUILT_FROM_SOURCE_FILE_1,
                        &[&redirect.output_dts, &resolved_module.resolved_file_name],
                    );
                    return None;
                }
            }

            if let Some(resolution_diagnostic) = resolution_diagnostic {
                self.error(
                    Some(error_node),
                    resolution_diagnostic,
                    &[&module_reference, &resolved_module.unwrap().resolved_file_name],
                );
            } else {
                let is_extensionless_relative_path_import =
                    tspath::path_is_relative(module_reference)
                        && !tspath::has_extension(module_reference);
                let resolution_is_node16_or_next = self.module_resolution_kind
                    == ModuleResolutionKind::NODE16
                    || self.module_resolution_kind == ModuleResolutionKind::NODE_NEXT;
                if !self.compiler_options.get_resolve_json_module()
                    && tspath::file_extension_is(module_reference, tspath::EXTENSION_JSON)
                {
                    self.error(Some(error_node), &diagnostics::CANNOT_FIND_MODULE_0_CONSIDER_USING_RESOLVE_JSON_MODULE_TO_IMPORT_MODULE_WITH_JSON_EXTENSION, &[&module_reference]);
                } else if mode == RESOLUTION_MODE_ESM
                    && resolution_is_node16_or_next
                    && is_extensionless_relative_path_import
                {
                    let suggested_ext =
                        if tspath::has_trailing_directory_separator(module_reference) {
                            String::new()
                        } else {
                            let absolute_ref = tspath::get_normalized_absolute_path(
                                module_reference,
                                &tspath::get_directory_path(importing_source_file.file_name()),
                            );
                            self.get_suggested_import_extension(&absolute_ref)
                        };
                    if suggested_ext.is_empty() {
                        self.error(Some(error_node), &diagnostics::RELATIVE_IMPORT_PATHS_NEED_EXPLICIT_FILE_EXTENSIONS_IN_ECMA_SCRIPT_IMPORTS_WHEN_MODULE_RESOLUTION_IS_NODE16_OR_NODENEXT_CONSIDER_ADDING_AN_EXTENSION_TO_THE_IMPORT_PATH, &[]);
                    } else {
                        let suggestion = format!("{module_reference}{suggested_ext}");
                        self.error(Some(error_node), &diagnostics::RELATIVE_IMPORT_PATHS_NEED_EXPLICIT_FILE_EXTENSIONS_IN_ECMA_SCRIPT_IMPORTS_WHEN_MODULE_RESOLUTION_IS_NODE16_OR_NODENEXT_DID_YOU_MEAN_0, &[&suggestion]);
                    }
                } else if resolved_module.is_some_and(|r| !r.alternate_result.is_empty()) {
                    let error_info = self.create_module_not_found_chain(
                        resolved_module.unwrap(),
                        error_node,
                        module_reference,
                        mode,
                        module_reference,
                    );
                    self.add_diagnostic(new_diagnostic_chain_for_node(
                        Some(error_info),
                        error_node,
                        Some(module_not_found_error),
                        &[&module_reference],
                    ));
                } else {
                    self.error(Some(error_node), module_not_found_error, &[&module_reference]);
                }
            }
        }

        None
    }

    // Resolves the module reference to a pattern ambient module, if one exists.
    // If a resolved symbol from regular module resolution exists and we have an empty import attributes type,
    // we prefer the resolved symbol.
    // checker.go:15696
    fn try_resolve_pattern_ambient_module(
        &mut self,
        resolved_symbol: Option<P<Symbol>>,
        module_reference: &str,
        import_attributes_type: P<Type>,
    ) -> Option<P<Symbol>> {
        if self.is_empty_object_type(import_attributes_type) && resolved_symbol.is_some() {
            return resolved_symbol;
        }
        if !self.pattern_ambient_modules.is_empty() {
            let pattern_ambient_modules = self.pattern_ambient_modules.clone();
            let mut candidates: Vec<P<ast::PatternAmbientModule>> = Vec::new();
            candidates.extend(pattern_ambient_modules.into_iter().filter(|v| {
                let module_attributes_type = self.get_type_of_module_import_attributes(v.symbol);
                v.pattern.matches(module_reference)
                    && self.is_type_assignable_to(import_attributes_type, module_attributes_type)
            }));

            if !candidates.is_empty() {
                let augmentation = self
                    .pattern_ambient_module_augmentations
                    .and_then(|t| t.lookup(module_reference));
                let augmentation_target = self
                    .pattern_ambient_module_augmentation_targets
                    .and_then(|t| t.lookup(module_reference));

                if candidates.len() == 1 {
                    let merged_candidate = self.get_merged_symbol(candidates[0].symbol);
                    if let Some(augmentation) = augmentation
                        && augmentation_target == Some(merged_candidate)
                    {
                        return Some(self.get_merged_symbol(augmentation));
                    }
                    return Some(merged_candidate);
                }

                let mut best_type_candidates: Vec<P<ast::PatternAmbientModule>> = Vec::new();
                best_type_candidates.extend(candidates.iter().enumerate().filter_map(
                    |(i, candidate)| {
                        let candidate_type =
                            self.get_type_of_module_import_attributes(candidate.symbol);
                        let has_better_candidate =
                            candidates.iter().enumerate().any(|(j, other)| {
                                let other_type =
                                    self.get_type_of_module_import_attributes(other.symbol);
                                i != j
                                    && self.is_type_strict_subtype_of(other_type, candidate_type)
                                    && !self.is_type_identical_to(other_type, candidate_type)
                            });
                        (!has_better_candidate).then_some(*candidate)
                    },
                ));
                if best_type_candidates.len() == 1 {
                    let merged_candidate = self.get_merged_symbol(best_type_candidates[0].symbol);
                    if let Some(augmentation) = augmentation
                        && augmentation_target == Some(merged_candidate)
                    {
                        return Some(self.get_merged_symbol(augmentation));
                    }
                    return Some(merged_candidate);
                }
                let pattern = find_best_pattern_match(
                    &best_type_candidates,
                    |v| &v.pattern,
                    module_reference,
                )
                .unwrap();
                let merged_candidate = self.get_merged_symbol(pattern.symbol);
                if let Some(augmentation) = augmentation
                    && augmentation_target == Some(merged_candidate)
                {
                    return Some(self.get_merged_symbol(augmentation));
                }
                return Some(merged_candidate);
            }
        }
        resolved_symbol
    }
}

// checker.go:15748
fn resolution_extension_is_ts_or_json(ext: &str) -> bool {
    tspath::extension_is_ts(ext) || ext == tspath::EXTENSION_JSON
}

impl Checker {
    // checker.go:15752
    fn get_suggested_import_source(
        &self,
        module_reference: &str,
        ts_extension: &str,
        mode: ModuleKind,
    ) -> String {
        let import_source_without_extension =
            tspath::remove_extension(module_reference, ts_extension);

        // Direct users to import source with .js extension if outputting an ES module.
        // @see https://github.com/microsoft/TypeScript/issues/42151
        if self.module_kind.is_non_node_esm() || mode == ModuleKind::ES_NEXT {
            let prefer_ts = tspath::is_declaration_file_name(module_reference)
                && self.compiler_options.get_allow_importing_ts_extensions();
            let ext = if ts_extension == tspath::EXTENSION_MTS
                || ts_extension == tspath::EXTENSION_DMTS
            {
                if prefer_ts { ".mts" } else { ".mjs" }
            } else if ts_extension == tspath::EXTENSION_CTS
                || ts_extension == tspath::EXTENSION_DCTS
            {
                if prefer_ts { ".cts" } else { ".cjs" }
            } else if prefer_ts {
                ".ts"
            } else {
                ".js"
            };

            return format!("{import_source_without_extension}{ext}");
        }

        import_source_without_extension.to_string()
    }

    // checker.go:15775
    fn get_suggested_import_extension(&self, extensionless_import_path: &str) -> String {
        let exists = |c: &Checker, ext: &str| {
            c.program.file_exists(&format!("{extensionless_import_path}{ext}"))
        };
        let result = if exists(self, ".mts") {
            ".mjs"
        } else if exists(self, ".ts") {
            ".js"
        } else if exists(self, ".cts") {
            ".cjs"
        } else if exists(self, ".mjs") {
            ".mjs"
        } else if exists(self, ".js") {
            ".js"
        } else if exists(self, ".cjs") {
            ".cjs"
        } else if exists(self, ".tsx") {
            if self.compiler_options.jsx == JsxEmit::PRESERVE { ".jsx" } else { ".js" }
        } else if exists(self, ".jsx") {
            ".jsx"
        } else if exists(self, ".json") {
            ".json"
        } else {
            ""
        };
        result.to_string()
    }

    // checker.go:15799
    fn error_on_implicit_any_module(
        &mut self,
        is_error: bool,
        error_node: P<Node>,
        mode: ModuleKind,
        resolved_module: P<ResolvedModule>,
        module_reference: &str,
    ) {
        if is_side_effect_import(error_node) {
            return;
        }

        let mut error_info: Option<P<Diagnostic>> = None;
        if !tspath::is_external_module_name_relative(module_reference)
            && !resolved_module.package_id.name.is_empty()
        {
            error_info = Some(self.create_module_not_found_chain(
                resolved_module,
                error_node,
                module_reference,
                mode,
                resolved_module.package_id.name,
            ));
        }
        self.add_error_or_suggestion(
            is_error,
            new_diagnostic_chain_for_node(
                error_info,
                error_node,
                Some(&diagnostics::COULD_NOT_FIND_A_DECLARATION_FILE_FOR_MODULE_0_1_IMPLICITLY_HAS_AN_ANY_TYPE),
                &[&module_reference, &resolved_module.resolved_file_name],
            ),
        );
    }

    // checker.go:15820
    fn create_module_not_found_chain(
        &self,
        resolved_module: P<ResolvedModule>,
        error_node: P<Node>,
        module_reference: &str,
        mode: ModuleKind,
        package_name: &str,
    ) -> P<Diagnostic> {
        let _ = resolved_module;
        // Store the original packageName for repopulateInfo before any modifications
        let mut stored_package_name = package_name;
        if stored_package_name == module_reference {
            stored_package_name = "";
        }

        let details = crate::create_module_not_found_chain(
            self.program,
            ast::get_source_file_of_node(error_node).unwrap(),
            module_reference,
            mode,
            package_name,
        );
        let args: Vec<&dyn Display> = details.args.iter().map(|a| a as &dyn Display).collect();
        let result = new_diagnostic_for_node(Some(error_node), Some(details.message), &args);
        result.set_repopulate_info(RepopulateDiagnosticInfo {
            kind: RepopulateDiagnosticKind::ModuleNotFound,
            module_reference: module_reference.to_string(),
            mode,
            package_name: stored_package_name.to_string(),
        });
        result
    }

    // checker.go:15838
    fn create_mode_mismatch_details(
        &self,
        source_file: P<SourceFile>,
        error_node: P<Node>,
    ) -> P<Diagnostic> {
        let details = crate::create_mode_mismatch_details(self.program, source_file);
        let args: Vec<&dyn Display> = details.args.iter().map(|a| a as &dyn Display).collect();
        let result = new_diagnostic_for_node(Some(error_node), Some(details.message), &args);
        result.set_repopulate_info(RepopulateDiagnosticInfo {
            kind: RepopulateDiagnosticKind::ModeMismatch,
            module_reference: String::new(),
            mode: ModuleKind::NONE,
            package_name: String::new(),
        });
        result
    }

    // checker.go:15847
    pub(crate) fn try_find_ambient_module(
        &mut self,
        module_name: &str,
        with_augmentations: bool,
    ) -> Option<P<Symbol>> {
        if tspath::is_external_module_name_relative(module_name) {
            return None;
        }
        let globals = self.globals;
        let symbol =
            self.get_symbol(Some(globals), &format!("\"{module_name}\""), SymbolFlags::ValueModule);
        // merged symbol is module declaration symbol combined with all augmentations
        if with_augmentations {
            return symbol.map(|s| self.get_merged_symbol(s));
        }
        symbol
    }

    // checker.go:15859
    pub fn get_ambient_modules(&mut self) -> Vec<P<Symbol>> {
        if !self.ambient_modules_once {
            self.ambient_modules_once = true;
            let mut seen: FxHashSet<P<Symbol>> = FxHashSet::default();
            for (sym, global) in self.globals.entries() {
                if ast::is_ambient_module_symbol_name(sym) {
                    self.ambient_modules.push(global);
                    seen.insert(global);
                }
            }
            let pattern_ambient_modules = self.pattern_ambient_modules.clone();
            for module in pattern_ambient_modules {
                let symbol = self.get_merged_symbol(module.symbol);
                if seen.insert(symbol) {
                    self.ambient_modules.push(symbol);
                }
            }
        }
        self.ambient_modules.clone()
    }

    // Go accepts and returns nil (nil in, nil out); callers with an Option map over it.
    // checker.go:15879
    pub(crate) fn resolve_external_module_symbol(
        &mut self,
        module_symbol: P<Symbol>,
        dont_resolve_alias: bool,
    ) -> P<Symbol> {
        let export_equals_symbol =
            module_symbol.exports().and_then(|e| e.lookup(INTERNAL_SYMBOL_NAME_EXPORT_EQUALS));
        let export_equals =
            export_equals_symbol.map(|s| self.resolve_symbol_ex(s, dont_resolve_alias));
        if let Some(export_equals) = export_equals {
            return self.get_merged_symbol(export_equals);
        }
        module_symbol
    }

    // Resolves the given external module symbol, possibly removing call and construct signatures or creating a
    // wrapper module with a synthetic default.
    // checker.go:15891
    pub(crate) fn resolve_es_module_symbol(
        &mut self,
        module_symbol: P<Symbol>,
        node: P<Node>,
        module_specifier: P<Node>,
    ) -> P<Symbol> {
        let mut symbol =
            self.resolve_external_module_symbol(module_symbol, true /*dontResolveAlias*/);
        if ast::is_non_local_alias(
            symbol,
            SymbolFlags::Value | SymbolFlags::Type | SymbolFlags::Namespace,
        ) {
            // When the module has an export= with a pure alias, we transitively resolve and propagate any typeOnlyDeclaration
            let source = self.get_symbol_of_declaration(node).unwrap();
            let resolved = self.resolve_indirection_alias(source, symbol);
            symbol = self.get_merged_symbol(resolved);
        }
        {
            let reference_parent = module_specifier.parent().unwrap();
            let mut namespace_import: Option<P<Node>> = None;
            if ast::is_import_declaration(reference_parent) {
                namespace_import = ast::get_namespace_declaration_node(reference_parent);
            }
            if namespace_import.is_some() || ast::is_import_call(reference_parent) {
                let reference = if ast::is_import_call(reference_parent) {
                    reference_parent.arguments()[0]
                } else {
                    reference_parent.module_specifier().unwrap()
                };
                let typ = self.get_type_of_symbol(symbol);
                let import_attributes_type =
                    self.get_import_attributes_type_for_module_specifier(reference);
                let default_only_type = self.get_type_with_synthetic_default_only(
                    Some(typ),
                    symbol,
                    module_symbol,
                    reference,
                    import_attributes_type,
                );
                if let Some(default_only_type) = default_only_type {
                    return self.clone_type_as_module_type(
                        symbol,
                        default_only_type,
                        reference_parent,
                    );
                }

                let target_file =
                    module_symbol.declarations().iter().copied().find(|d| ast::is_source_file(*d));
                let usage_mode = self.get_emit_syntax_for_module_specifier_expression(reference);
                let mut export_module_dot_exports_symbol: Option<P<Symbol>> = None;
                if let Some(namespace_import) = namespace_import
                    && let Some(target_file) = target_file
                    && ModuleKind::NODE20 <= self.module_kind
                    && self.module_kind <= ModuleKind::NODE_NEXT
                    && usage_mode == ModuleKind::COMMON_JS
                    && self.program.get_implied_node_format_for_emit(target_file.as_source_file_p())
                        == ModuleKind::ES_NEXT
                {
                    export_module_dot_exports_symbol = self.get_export_of_module(
                        symbol,
                        INTERNAL_SYMBOL_NAME_MODULE_EXPORTS,
                        namespace_import,
                        true, /*dontResolveAlias*/
                    );
                }
                if let Some(export_module_dot_exports_symbol) = export_module_dot_exports_symbol {
                    if self.has_signatures(typ) {
                        return self.clone_type_as_module_type(
                            export_module_dot_exports_symbol,
                            typ,
                            reference_parent,
                        );
                    }
                    return export_module_dot_exports_symbol;
                }

                let is_esm_cjs_ref = target_file.is_some()
                    && is_esm_format_import_importing_commonjs_format_file(
                        usage_mode,
                        self.program.get_implied_node_format_for_emit(
                            target_file.unwrap().as_source_file_p(),
                        ),
                    );
                if self.has_signatures(typ)
                    || self
                        .get_property_of_type_ex(
                            typ,
                            INTERNAL_SYMBOL_NAME_DEFAULT,
                            true,  /*skipObjectFunctionPropertyAugment*/
                            false, /*includeTypeOnlyMembers*/
                        )
                        .is_some()
                    || is_esm_cjs_ref
                {
                    let module_type = if typ.flags().intersects(TypeFlags::StructuredType) {
                        self.get_type_with_synthetic_default_import_type(
                            typ,
                            symbol,
                            module_symbol,
                            reference,
                        )
                    } else {
                        self.create_default_property_wrapper_for_module(
                            symbol,
                            symbol.parent(),
                            None,
                        )
                    };
                    return self.clone_type_as_module_type(symbol, module_type, reference_parent);
                }
            }
        }
        symbol
    }

    // checker.go:15947
    fn has_signatures(&mut self, t: P<Type>) -> bool {
        !self.get_signatures_of_structured_type(t, SignatureKind::Call).is_empty()
            || !self.get_signatures_of_structured_type(t, SignatureKind::Construct).is_empty()
    }
}

// checker.go:15951
fn is_esm_format_import_importing_commonjs_format_file(
    usage_mode: ModuleKind,
    target_mode: ModuleKind,
) -> bool {
    usage_mode == ModuleKind::ES_NEXT && target_mode == ModuleKind::COMMON_JS
}

impl Checker {
    // checker.go:15955
    pub(crate) fn get_type_with_synthetic_default_only(
        &mut self,
        t: Option<P<Type>>,
        symbol: P<Symbol>,
        original_symbol: P<Symbol>,
        module_specifier: P<Node>,
        import_attributes_type: Option<P<Type>>,
    ) -> Option<P<Type>> {
        let has_default_only =
            self.is_only_importable_as_default(module_specifier, None, import_attributes_type);
        if has_default_only
            && let Some(t) = t
            && !self.is_error_type(t)
        {
            let key = CachedTypeKey { kind: CachedTypeKind::DefaultOnlyType, type_id: t.id };
            if let Some(cached) = self.cached_types.get(&key) {
                return Some(*cached);
            }
            let result = self.create_default_property_wrapper_for_module(
                symbol,
                Some(original_symbol),
                None,
            );
            self.cached_types.insert(key, result);
            return Some(result);
        }
        None
    }

    // checker.go:15969
    pub(crate) fn get_type_with_synthetic_default_import_type(
        &mut self,
        t: P<Type>,
        symbol: P<Symbol>,
        original_symbol: P<Symbol>,
        module_specifier: P<Node>,
    ) -> P<Type> {
        if !self.is_error_type(t) {
            let key = CachedTypeKey { kind: CachedTypeKind::SyntheticType, type_id: t.id };
            if let Some(cached) = self.cached_types.get(&key) {
                return *cached;
            }
            let file =
                original_symbol.declarations().iter().copied().find(|d| ast::is_source_file(*d));
            let has_synthetic_default = self.can_have_synthetic_default(
                file,
                original_symbol,
                false, /*dontResolveAlias*/
                module_specifier,
            );
            let synthetic_type;
            if has_synthetic_default {
                let anonymous_symbol =
                    self.new_symbol(SymbolFlags::TypeLiteral, INTERNAL_SYMBOL_NAME_TYPE);
                anonymous_symbol.set_declarations_static(original_symbol.declarations());
                let default_containing_object = self.create_default_property_wrapper_for_module(
                    symbol,
                    Some(original_symbol),
                    Some(anonymous_symbol),
                );
                self.value_symbol_links
                    .get(anonymous_symbol)
                    .resolved_type
                    .set(Some(default_containing_object));
                if self.is_valid_spread_type(t) {
                    synthetic_type = self.get_spread_type(
                        t,
                        default_containing_object,
                        Some(anonymous_symbol),
                        ObjectFlags::None,
                        false, /*readonly*/
                    );
                } else {
                    synthetic_type = default_containing_object;
                }
            } else {
                synthetic_type = t;
            }
            self.cached_types.insert(key, synthetic_type);
            return synthetic_type;
        }
        t
    }

    // checker.go:15997
    pub(crate) fn is_common_js_require(&mut self, node: P<Node>) -> bool {
        if !ast::is_require_call(node, true /*requireStringLiteralLikeArgument*/) {
            return false;
        }
        let expression = node.expression().unwrap();
        assert!(ast::is_identifier(expression), "Expected identifier for require call");
        // Make sure require is not a local function
        let resolved_require = self.resolve_name(
            Some(expression),
            expression.text(),
            SymbolFlags::Value,
            None,  /*nameNotFoundMessage*/
            true,  /*isUse*/
            false, /*excludeGlobals*/
        );
        if resolved_require == Some(self.require_symbol) {
            return true;
        }
        // project includes symbol named 'require' - make sure that it is ambient and local non-alias
        let resolved_require = match resolved_require {
            Some(r) if !r.flags().intersects(SymbolFlags::Alias) => r,
            _ => return false,
        };

        let target_declaration_kind = if resolved_require.flags().intersects(SymbolFlags::Function)
        {
            Kind::FunctionDeclaration
        } else if resolved_require.flags().intersects(SymbolFlags::Variable) {
            Kind::VariableDeclaration
        } else {
            Kind::Unknown
        };
        if target_declaration_kind != Kind::Unknown {
            let decl = ast::get_declaration_of_kind(resolved_require, target_declaration_kind);
            // function/variable declaration should be ambient
            return decl.is_some_and(|d| d.flags().intersects(NodeFlags::Ambient));
        }
        false
    }

    // checker.go:16030
    fn create_default_property_wrapper_for_module(
        &mut self,
        symbol: P<Symbol>,
        original_symbol: Option<P<Symbol>>,
        anonymous_symbol: Option<P<Symbol>>,
    ) -> P<Type> {
        let member_table = SymbolTable::new();
        let new_symbol = self.new_symbol(SymbolFlags::Alias, INTERNAL_SYMBOL_NAME_DEFAULT);
        new_symbol.set_parent(original_symbol);
        let name_type = self.get_string_literal_type("default");
        self.value_symbol_links.get(new_symbol).set_name_type(Some(name_type));
        let alias_target = self.resolve_symbol(symbol);
        self.alias_symbol_links.get(new_symbol).alias_target.set(Some(alias_target));
        member_table.set(INTERNAL_SYMBOL_NAME_DEFAULT, new_symbol);
        let mut anonymous_symbol = anonymous_symbol;
        if anonymous_symbol.is_none()
            && let Some(original_symbol) = original_symbol
        {
            let s = self.new_symbol(SymbolFlags::ObjectLiteral, INTERNAL_SYMBOL_NAME_OBJECT);
            s.set_declarations_static(original_symbol.declarations());
            anonymous_symbol = Some(s);
        }
        self.new_anonymous_type(anonymous_symbol, Some(member_table), &[], &[], &[])
    }

    // checker.go:16044
    fn clone_type_as_module_type(
        &mut self,
        symbol: P<Symbol>,
        module_type: P<Type>,
        reference_parent: P<Node>,
    ) -> P<Symbol> {
        let result = self.new_symbol(symbol.flags(), symbol.name());
        result.set_declarations_static(symbol.declarations());
        result.set_value_declaration(symbol.value_declaration());
        result.set_members(symbol.members().map(|m| m.clone_table()));
        result.set_exports(symbol.exports().map(|e| e.clone_table()));
        result.set_parent(symbol.parent());
        let links = self.export_type_links.get(result);
        links.target.set(Some(symbol));
        links.originating_import.set(Some(reference_parent));
        let resolved_module_type = self.resolve_structured_type_members(module_type);
        let t = self.new_anonymous_type(
            Some(result),
            resolved_module_type.members(),
            &[],
            &[],
            resolved_module_type.index_infos(),
        );
        self.value_symbol_links.get(result).resolved_type.set(Some(t));
        result
    }

    // checker.go:16059
    pub(crate) fn get_target_of_alias_declaration(
        &mut self,
        node: Option<P<Node>>,
    ) -> Option<P<Symbol>> {
        let node = node?;
        match node.kind() {
            Kind::ImportEqualsDeclaration | Kind::VariableDeclaration => {
                self.get_target_of_import_equals_declaration(node)
            }
            Kind::ImportClause => self.get_target_of_import_clause(node),
            Kind::NamespaceImport => self.get_target_of_namespace_import(node),
            Kind::NamespaceExport => self.get_target_of_namespace_export(node),
            Kind::ImportSpecifier | Kind::BindingElement => {
                self.get_target_of_import_specifier(node)
            }
            Kind::ExportSpecifier => self.get_target_of_export_specifier(
                node,
                SymbolFlags::Value | SymbolFlags::Type | SymbolFlags::Namespace,
                true, /*dontRecursivelyResolve*/
            ),
            Kind::ExportAssignment => self.get_target_of_export_assignment(node),
            Kind::BinaryExpression => self.get_target_of_binary_expression(node),
            Kind::NamespaceExportDeclaration => {
                self.get_target_of_namespace_export_declaration(node)
            }
            Kind::ShorthandPropertyAssignment => self.resolve_entity_name(
                node.as_shorthand_property_assignment().name(),
                SymbolFlags::Value | SymbolFlags::Type | SymbolFlags::Namespace,
                true, /*ignoreErrors*/
                true, /*dontRecursivelyResolve*/
                None, /*location*/
            ),
            Kind::PropertyAssignment => {
                self.get_target_of_alias_like_expression(node.initializer().unwrap())
            }
            Kind::ElementAccessExpression | Kind::PropertyAccessExpression => {
                self.get_target_of_access_expression(node)
            }
            _ => panic!("Unhandled case in getTargetOfAliasDeclaration: {}", node.kind_string()),
        }
    }

    /**
     * Resolves a qualified name and any involved aliases.
     */
    // checker.go:16095
    pub(crate) fn resolve_entity_name(
        &mut self,
        name: P<Node>,
        meaning: SymbolFlags,
        ignore_errors: bool,
        dont_resolve_alias: bool,
        location: Option<P<Node>>,
    ) -> Option<P<Symbol>> {
        if ast::node_is_missing(name) {
            return None;
        }
        let mut symbol: Option<P<Symbol>>;
        match name.kind() {
            Kind::Identifier => {
                let mut message: Option<&'static Message> = None;
                if !ignore_errors {
                    if meaning == SymbolFlags::Namespace || ast::node_is_synthesized(name) {
                        message = Some(&diagnostics::CANNOT_FIND_NAMESPACE_0);
                    } else {
                        message = Some(self.get_cannot_find_name_diagnostic_for_name(
                            ast::get_first_identifier(name),
                        ));
                    }
                }
                let resolve_location = location.unwrap_or(name);
                if meaning == SymbolFlags::Namespace {
                    symbol = self
                        .resolve_name(
                            Some(resolve_location),
                            name.text(),
                            meaning,
                            None,
                            true,  /*isUse*/
                            false, /*excludeGlobals*/
                        )
                        .map(|s| self.get_merged_symbol(s));
                    if symbol.is_none() {
                        let alias = self
                            .resolve_name(
                                Some(resolve_location),
                                name.text(),
                                SymbolFlags::Alias,
                                None,
                                true,  /*isUse*/
                                false, /*excludeGlobals*/
                            )
                            .map(|s| self.get_merged_symbol(s));
                        if let Some(alias) = alias
                            && alias.name() == INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
                        {
                            // resolve typedefs exported from commonjs, stored on the module symbol
                            symbol = alias.parent();
                        }
                    }
                    if symbol.is_none() && message.is_some() {
                        self.resolve_name(
                            Some(resolve_location),
                            name.text(),
                            meaning,
                            message,
                            true,  /*isUse*/
                            false, /*excludeGlobals*/
                        );
                    }
                } else {
                    symbol = self
                        .resolve_name(
                            Some(resolve_location),
                            name.text(),
                            meaning,
                            message,
                            true,  /*isUse*/
                            false, /*excludeGlobals*/
                        )
                        .map(|s| self.get_merged_symbol(s));
                }
            }
            Kind::QualifiedName => {
                let qualified = name.as_qualified_name();
                symbol = self.resolve_qualified_name(
                    name,
                    qualified.left(),
                    qualified.right(),
                    meaning,
                    ignore_errors,
                    location,
                );
            }
            Kind::PropertyAccessExpression => {
                let access = name.as_property_access_expression();
                symbol = self.resolve_qualified_name(
                    name,
                    access.expression(),
                    access.name(),
                    meaning,
                    ignore_errors,
                    location,
                );
            }
            _ => panic!("Unknown entity name kind"),
        }
        if let Some(mut s) = symbol
            && s != self.unknown_symbol
        {
            if !ast::node_is_synthesized(name)
                && ast::is_entity_name(name)
                && (s.flags().intersects(SymbolFlags::Alias)
                    || name.parent().is_some_and(|p| p.kind() == Kind::ExportAssignment))
            {
                self.mark_symbol_of_alias_declaration_if_type_only(
                    get_alias_declaration_from_name(name),
                    None,
                );
            }
            // We know a symbol with the given meaning exists along the alias chain, so resolve until we find it.
            while !s.flags().intersects(meaning)
                && !dont_resolve_alias
                && s.flags().intersects(SymbolFlags::Alias)
            {
                s = self.resolve_alias(s);
            }
            symbol = Some(s);
        }
        symbol
    }

    // checker.go:16151
    fn resolve_qualified_name(
        &mut self,
        name: P<Node>,
        left: P<Node>,
        right: P<Node>,
        meaning: SymbolFlags,
        ignore_errors: bool,
        location: Option<P<Node>>,
    ) -> Option<P<Symbol>> {
        let namespace = self.resolve_entity_name(
            left,
            SymbolFlags::Namespace,
            ignore_errors,
            false, /*dontResolveAlias*/
            location,
        );
        let mut namespace = match namespace {
            Some(n) if !ast::node_is_missing(right) => n,
            _ => return None,
        };
        if namespace == self.unknown_symbol {
            return Some(namespace);
        }
        if let Some(value_declaration) = namespace.value_declaration()
            && ast::is_in_js_file(value_declaration)
            && self.compiler_options.get_module_resolution_kind() != ModuleResolutionKind::BUNDLER
            && ast::is_variable_declaration(value_declaration)
            && value_declaration.initializer().is_some()
            && self.is_common_js_require(value_declaration.initializer().unwrap())
        {
            let module_name = value_declaration.initializer().unwrap().arguments()[0];
            let module_sym = self.resolve_external_module_name(
                module_name,
                module_name,
                false, /*ignoreErrors*/
                None,
            );
            if let Some(module_sym) = module_sym {
                let resolved_module_symbol = self
                    .resolve_external_module_symbol(module_sym, false /*dontResolveAlias*/);
                namespace = resolved_module_symbol;
            }
        }
        let text = right.text();
        let exports = self.get_exports_of_symbol(namespace);
        let mut symbol = self.get_symbol(exports, text, meaning).map(|s| self.get_merged_symbol(s));
        if symbol.is_none() && namespace.flags().intersects(SymbolFlags::Alias) {
            // `namespace` can be resolved further if there was a symbol merge with a re-export
            let resolved = self.resolve_alias(namespace);
            let exports = self.get_exports_of_symbol(resolved);
            symbol = self.get_symbol(exports, text, meaning).map(|s| self.get_merged_symbol(s));
        }
        if symbol.is_none() && !ignore_errors {
            let namespace_name =
                self.get_fully_qualified_name(namespace, None /*containingLocation*/);
            let declaration_name = tsrs_scanner::declaration_name_to_string(Some(right));
            let suggestion_for_nonexistent_module =
                self.get_suggested_symbol_for_nonexistent_module(right, namespace);
            if let Some(suggestion) = suggestion_for_nonexistent_module {
                let suggestion_name = self.symbol_to_string(suggestion);
                self.error(
                    Some(right),
                    &diagnostics::X_0_HAS_NO_EXPORTED_MEMBER_NAMED_1_DID_YOU_MEAN_2,
                    &[&namespace_name, &declaration_name, &suggestion_name],
                );
                return None;
            }
            let mut containing_qualified_name: Option<P<Node>> = None;
            if ast::is_qualified_name(name) {
                containing_qualified_name = Some(get_containing_qualified_name_node(name));
            }
            let can_suggest_typeof = self.global_object_type != self.unassigned_type
                && meaning.intersects(SymbolFlags::Type)
                && containing_qualified_name
                    .is_some_and(|n| !ast::is_type_of_expression(n.parent().unwrap()))
                && self
                    .try_get_qualified_name_as_value(containing_qualified_name.unwrap())
                    .is_some();
            if can_suggest_typeof {
                let containing_qualified_name = containing_qualified_name.unwrap();
                self.error(Some(containing_qualified_name), &diagnostics::X_0_REFERS_TO_A_VALUE_BUT_IS_BEING_USED_AS_A_TYPE_HERE_DID_YOU_MEAN_TYPEOF_0, &[&crate::entity_name_to_string(containing_qualified_name)]);
                return None;
            }
            if meaning.intersects(SymbolFlags::Namespace) {
                let parent = name.parent().unwrap();
                if ast::is_qualified_name(parent) {
                    let exports = self.get_exports_of_symbol(namespace);
                    let exported_type_symbol = self
                        .get_symbol(exports, text, SymbolFlags::Type)
                        .map(|s| self.get_merged_symbol(s));
                    if let Some(exported_type_symbol) = exported_type_symbol {
                        let qualified = parent.as_qualified_name();
                        let symbol_name = self.symbol_to_string(exported_type_symbol);
                        self.error(Some(qualified.right()), &diagnostics::CANNOT_ACCESS_0_1_BECAUSE_0_IS_A_TYPE_BUT_NOT_A_NAMESPACE_DID_YOU_MEAN_TO_RETRIEVE_THE_TYPE_OF_THE_PROPERTY_1_IN_0_WITH_0_1, &[&symbol_name, &qualified.right().text()]);
                        return None;
                    }
                }
            }
            self.error(
                Some(right),
                &diagnostics::NAMESPACE_0_HAS_NO_EXPORTED_MEMBER_1,
                &[&namespace_name, &declaration_name],
            );
        }
        symbol
    }

    // checker.go:16214
    fn try_get_qualified_name_as_value(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        let id = ast::get_first_identifier(node);
        let mut symbol = self.resolve_name(
            Some(id),
            id.text(),
            SymbolFlags::Value,
            None,  /*nameNotFoundMessage*/
            true,  /*isUse*/
            false, /*excludeGlobals*/
        )?;
        let mut n = id;
        while ast::is_qualified_name(n.parent().unwrap()) {
            let t = self.get_type_of_symbol(symbol);
            symbol = self
                .get_property_of_type(t, n.parent().unwrap().as_qualified_name().right().text())?;
            n = n.parent().unwrap();
        }
        Some(symbol)
    }

    // checker.go:16232
    pub(crate) fn get_suggested_symbol_for_nonexistent_module(
        &mut self,
        name: P<Node>,
        target_module: P<Symbol>,
    ) -> Option<P<Symbol>> {
        let exports = self.get_exports_of_module(target_module).values();
        self.get_spelling_suggestion_for_name(name.text(), &exports, SymbolFlags::ModuleMember)
    }

    // checker.go:16236
    pub(crate) fn get_fully_qualified_name(
        &mut self,
        symbol: P<Symbol>,
        containing_location: Option<P<Node>>,
    ) -> String {
        if let Some(parent) = symbol.parent() {
            let parent_name = self.get_fully_qualified_name(parent, containing_location);
            return parent_name + "." + &self.symbol_to_string(symbol);
        }
        self.symbol_to_string_ex(
            symbol,
            containing_location,
            SymbolFlags::All,
            SymbolFormatFlags::DoNotIncludeSymbolChain | SymbolFormatFlags::AllowAnyNodeKind,
        )
    }

    // checker.go:16243
    pub(crate) fn get_exports_of_symbol(&mut self, symbol: P<Symbol>) -> Option<P<SymbolTable>> {
        if symbol.flags().intersects(SymbolFlags::LateBindingContainer) {
            return self.get_resolved_members_or_exports_of_symbol(
                symbol,
                MembersOrExportsResolutionKind::ResolvedExports,
            );
        }
        if symbol.flags().intersects(SymbolFlags::Module) {
            return Some(self.get_exports_of_module(symbol));
        }
        symbol.exports()
    }

    // returns Option (Go returns a nil table when there are neither early nor late symbols; nil also
    // means "not yet resolved" in the links, so it must not be replaced by an empty table).
    // checker.go:16253
    pub(crate) fn get_resolved_members_or_exports_of_symbol(
        &mut self,
        symbol: P<Symbol>,
        resolution_kind: MembersOrExportsResolutionKind,
    ) -> Option<P<SymbolTable>> {
        let links = self.members_and_exports_links.get(symbol);
        let kind = resolution_kind as usize;
        if links[kind].get().is_none() {
            let is_static = resolution_kind == MembersOrExportsResolutionKind::ResolvedExports;
            let mut early_symbols = symbol.exports();
            if !is_static {
                early_symbols = symbol.members();
            } else if symbol.flags().intersects(SymbolFlags::Module) {
                early_symbols = Some(self.get_exports_of_module_worker(Some(symbol)).0);
            }
            links[kind].set(early_symbols);
            // fill in any as-yet-unresolved late-bound members.
            let mut late_symbols: Option<P<SymbolTable>> = None;
            let declarations = symbol.declarations();
            for &decl in declarations {
                for member in get_members_of_declaration(decl) {
                    if is_static == ast::has_static_modifier(member) {
                        if self.has_late_bindable_name(member) {
                            let late = *late_symbols.get_or_insert_with(SymbolTable::new);
                            self.late_bind_member(symbol, early_symbols, late, member);
                        } else if self.has_late_bindable_index_signature(member) {
                            let late = *late_symbols.get_or_insert_with(SymbolTable::new);
                            self.late_bind_index_signature(symbol, early_symbols, late, member /* as LateBoundDeclaration | LateBoundBinaryExpressionDeclaration */);
                        }
                    }
                }
            }
            if is_static
                && let Some(assignment_symbol) = symbol
                    .exports()
                    .and_then(|e| e.lookup(INTERNAL_SYMBOL_NAME_ASSIGNMENT_DECLARATION))
            {
                let declarations = assignment_symbol.declarations();
                for &member in declarations {
                    if self.has_late_bindable_name(member) {
                        let late = *late_symbols.get_or_insert_with(SymbolTable::new);
                        self.late_bind_member(symbol, early_symbols, late, member);
                    }
                }
            }
            let combined = self.combine_symbol_tables(early_symbols, late_symbols);
            links[kind].set(combined);
        }
        links[kind].get()
    }

    // Performs late-binding of a dynamic member. This performs the same function for
    // late-bound members that `declareSymbol` in binder.ts performs for early-bound
    // members.
    //
    // If a symbol is a dynamic name from a computed property, we perform an additional "late"
    // binding phase to attempt to resolve the name for the symbol from the type of the computed
    // property's expression. If the type of the expression is a string-literal, numeric-literal,
    // or unique symbol type, we can use that type as the name of the symbol.
    //
    // For example, given:
    //
    //	const x = Symbol();
    //
    //	interface I {
    //	  [x]: number;
    //	}
    //
    // The binder gives the property `[x]: number` a special symbol with the name "__computed".
    // In the late-binding phase we can type-check the expression `x` and see that it has a
    // unique symbol type which we can then use as the name of the member. This allows users
    // to define custom symbols that can be used in the members of an object type.
    //
    // @param parent The containing symbol for the member.
    // @param earlySymbols The early-bound symbols of the parent.
    // @param lateSymbols The late-bound symbols of the parent.
    // @param decl The member to bind.
    // checker.go:16328
    fn late_bind_member(
        &mut self,
        parent: P<Symbol>,
        early_symbols: Option<P<SymbolTable>>,
        late_symbols: P<SymbolTable>,
        decl: P<Node>,
    ) -> Option<P<Symbol>> {
        assert!(decl.symbol().is_some(), "The member is expected to have a symbol.");
        let decl_symbol = decl.symbol().unwrap();
        let links = self.symbol_node_links.get(decl);
        if links.resolved_symbol.get().is_none() {
            // In the event we attempt to resolve the late-bound name of this member recursively,
            // fall back to the early-bound name of this member.
            links.resolved_symbol.set(Some(decl_symbol));
            let decl_name = if ast::is_binary_expression(decl) {
                decl.as_binary_expression().left()
            } else {
                decl.name().unwrap()
            };
            let t = if ast::is_element_access_expression(decl_name) {
                self.check_expression_cached(
                    decl_name.as_element_access_expression().argument_expression(),
                )
            } else {
                self.check_computed_property_name(decl_name)
            };
            if is_type_usable_as_property_name(t) {
                let member_name = get_property_name_from_type(t);
                let symbol_flags = decl_symbol.flags();
                // Get or add a late-bound symbol for the member. This allows us to merge late-bound accessor declarations.
                let mut late_symbol = if let Some(s) = late_symbols.lookup(&member_name) {
                    s
                } else {
                    let s = self.new_symbol_ex(
                        SymbolFlags::None,
                        alloc_str(&member_name),
                        CheckFlags::Late,
                    );
                    late_symbols.set(s.name(), s);
                    s
                };
                // Report an error if there's a symbol declaration with the same name and conflicting flags.
                let early_symbol = early_symbols.and_then(|e| e.lookup(&member_name));
                if late_symbol.flags().intersects(get_excluded_symbol_flags(symbol_flags)) {
                    // If we have an existing early-bound member, combine its declarations so that we can
                    // report an error at each declaration.
                    let declarations: Vec<P<Node>> = if let Some(early_symbol) = early_symbol {
                        let mut v = early_symbol.declarations().to_vec();
                        v.extend(late_symbol.declarations().iter().copied());
                        v
                    } else {
                        late_symbol.declarations().to_vec()
                    };
                    let mut name = member_name.clone();
                    if t.flags().intersects(TypeFlags::UniqueESSymbol) {
                        name = tsrs_scanner::declaration_name_to_string(Some(decl_name)).into();
                    }
                    for d in declarations {
                        self.error(
                            Some(ast::get_name_of_declaration(d).unwrap_or(d)),
                            &diagnostics::DUPLICATE_IDENTIFIER_0,
                            &[&name],
                        );
                    }
                    self.error(Some(decl_name), &diagnostics::DUPLICATE_IDENTIFIER_0, &[&name]);
                    if late_symbol.flags().intersects(SymbolFlags::Accessor)
                        && late_symbol.flags() & SymbolFlags::Accessor
                            != symbol_flags & SymbolFlags::Accessor
                    {
                        late_symbol.flags.set(late_symbol.flags() | SymbolFlags::Accessor);
                    }
                    late_symbol = self.new_symbol_ex(
                        SymbolFlags::None,
                        alloc_str(&member_name),
                        CheckFlags::Late,
                    );
                }
                self.value_symbol_links.get(late_symbol).set_name_type(Some(t));
                self.add_declaration_to_late_bound_symbol(late_symbol, decl, symbol_flags);
                if late_symbol.parent().is_none() {
                    late_symbol.set_parent(Some(parent));
                }
                links.resolved_symbol.set(Some(late_symbol));
            }
        }
        links.resolved_symbol.get()
    }

    // checker.go:16391
    fn late_bind_index_signature(
        &mut self,
        parent: P<Symbol>,
        early_symbols: Option<P<SymbolTable>>,
        late_symbols: P<SymbolTable>,
        decl: P<Node>,
    ) {
        let _ = parent;
        // First, late bind the index symbol itself, if needed
        let index_symbol = if let Some(s) = late_symbols.lookup(INTERNAL_SYMBOL_NAME_INDEX) {
            s
        } else {
            let early = early_symbols.and_then(|e| e.lookup(INTERNAL_SYMBOL_NAME_INDEX));
            let index_symbol = match early {
                None => self.new_symbol_ex(
                    SymbolFlags::None,
                    INTERNAL_SYMBOL_NAME_INDEX,
                    CheckFlags::Late,
                ),
                Some(early) => {
                    let s = self.clone_symbol(early);
                    s.set_check_flags(s.check_flags() | CheckFlags::Late);
                    s
                }
            };
            late_symbols.set(INTERNAL_SYMBOL_NAME_INDEX, index_symbol);
            index_symbol
        };
        // Then just add the computed name as a late bound declaration
        // (note: unlike `addDeclarationToLateBoundSymbol` we do not set up a `.lateSymbol` on `decl`'s links,
        // since that would point at an index symbol and not a single property symbol, like most consumers would expect)
        if index_symbol.declarations().is_empty()
            || !decl.symbol().unwrap().flags().intersects(SymbolFlags::ReplaceableByMethod)
        {
            index_symbol.append_declarations(&[decl]);
        }
    }
}

// checker.go:16412
fn is_not_replacable_by_method(decl: P<Node>) -> bool {
    !decl.symbol().unwrap().flags().intersects(SymbolFlags::ReplaceableByMethod)
}

impl Checker {
    // Adds a declaration to a late-bound dynamic member. This performs the same function for
    // late-bound members that `addDeclarationToSymbol` in binder.ts performs for early-bound
    // members.
    // checker.go:16419
    fn add_declaration_to_late_bound_symbol(
        &mut self,
        symbol: P<Symbol>,
        member: P<Node>,
        symbol_flags: SymbolFlags,
    ) {
        assert!(symbol.check_flags().intersects(CheckFlags::Late), "Expected a late-bound symbol.");
        let member_symbol = member.symbol().unwrap();
        self.late_bound_links.get(member_symbol).late_symbol.set(Some(symbol));
        if symbol.declarations().is_empty()
            || !member_symbol.flags().intersects(SymbolFlags::ReplaceableByMethod)
        {
            symbol.flags.set(symbol.flags() | symbol_flags);
            symbol.append_declarations(&[member]);
        } else if symbol.flags().intersects(SymbolFlags::ReplaceableByMethod)
            && member_symbol.flags().intersects(SymbolFlags::Method)
        {
            // Remove all replacable-by-method members, along with their flags.
            let mut declarations: Vec<P<Node>> = symbol
                .declarations()
                .iter()
                .copied()
                .filter(|d| is_not_replacable_by_method(*d))
                .collect();
            declarations.push(member);
            symbol.set_declarations(&declarations);
            let old_flags = symbol.flags();
            symbol.flags.set(SymbolFlags::Transient);
            let declarations = symbol.declarations();
            for d in declarations {
                symbol.flags.set(symbol.flags() | d.symbol().unwrap().flags());
            }
            if old_flags.intersects(SymbolFlags::Accessor) {
                symbol.flags.set(symbol.flags() | SymbolFlags::Accessor);
            }
        }
        if symbol_flags.intersects(SymbolFlags::Value) {
            tsrs_binder::set_value_declaration(symbol, member);
        }
    }

    /**
     * Gets a `SymbolTable` containing both the early- and late-bound members of a symbol.
     *
     * For a description of late-binding, see `lateBindMember`.
     */
    // checker.go:16447
    pub(crate) fn get_members_of_symbol(&mut self, symbol: P<Symbol>) -> Option<P<SymbolTable>> {
        if symbol.flags().intersects(SymbolFlags::LateBindingContainer) {
            return self.get_resolved_members_or_exports_of_symbol(
                symbol,
                MembersOrExportsResolutionKind::ResolvedMembers,
            );
        }
        symbol.members()
    }

    // checker.go:16454
    pub(crate) fn get_exports_of_module(&mut self, module_symbol: P<Symbol>) -> P<SymbolTable> {
        let links = self.module_symbol_links.get(module_symbol);
        if links.resolved_exports.get().is_none() {
            // A nested computation of the same table (export * cycles) caches its result, which this one then replaces.
            self.alias_cache_blockers += 1;
            let (exports, type_only_export_star_map) =
                self.get_exports_of_module_worker(Some(module_symbol));
            links.resolved_exports.set(Some(exports));
            links.type_only_export_star_map.assign(type_only_export_star_map);
            self.alias_cache_blockers -= 1;
        }
        links.resolved_exports.get().unwrap()
    }

    // checker.go:16471
    fn get_exports_of_module_worker(
        &mut self,
        module_symbol: Option<P<Symbol>>,
    ) -> (P<SymbolTable>, FxHashMap<String, P<Node>>) {
        struct VisitState {
            visited_symbols: Vec<P<Symbol>>,
            non_type_only_names: collections::Set<String>,
            type_only_export_star_map: Option<FxHashMap<String, P<Node>>>,
        }
        // The ES6 spec permits export * declarations in a module to circularly reference the module itself. For example,
        // module 'a' can 'export * from "b"' and 'b' can 'export * from "a"' without error.
        // Only the outermost table escapes this traversal. Own temporary tables so their buffers are dropped
        // after merging, instead of retaining them in the arena.
        fn visit(
            c: &mut Checker,
            st: &mut VisitState,
            symbol: Option<P<Symbol>>,
            export_star: Option<P<Node>>,
            is_type_only: bool,
        ) -> Option<SymbolTable> {
            if !is_type_only && let Some(symbol) = symbol {
                // Add non-type-only names before checking if we've visited this module,
                // because we might have visited it via an 'export type *', and visiting
                // again with 'export *' will override the type-onlyness of its exports.
                if let Some(exports) = symbol.exports() {
                    st.non_type_only_names
                        .m
                        .extend(exports.keys().into_iter().map(std::string::ToString::to_string));
                }
            }
            let symbol = symbol?;
            let symbol_exports = symbol.exports()?;
            if st.visited_symbols.contains(&symbol) {
                return None;
            }
            st.visited_symbols.push(symbol);
            let symbols = symbol_exports.clone_value();
            // All export * declarations are collected in an __export symbol by the binder
            let export_stars = symbol_exports.lookup(INTERNAL_SYMBOL_NAME_EXPORT_STAR);
            if let Some(export_stars) = export_stars {
                let nested_symbols = SymbolTable::default();
                let mut lookup_table: ExportCollisionTable = FxHashMap::default();
                let declarations = export_stars.declarations();
                for &node in declarations {
                    let import_attributes_type =
                        c.get_type_from_import_attributes(ast::get_import_attributes(node));
                    let resolved_module = c.resolve_external_module_name(
                        node,
                        node.module_specifier().unwrap(),
                        false, /*ignoreErrors*/
                        import_attributes_type,
                    );
                    let exported_symbols = visit(
                        c,
                        st,
                        resolved_module,
                        Some(node),
                        is_type_only || node.is_type_only(),
                    );
                    c.extend_export_symbols(
                        &nested_symbols,
                        exported_symbols.as_ref(),
                        Some(&mut lookup_table),
                        Some(node),
                    );
                }
                #[expect(
                    clippy::iter_over_hash_type,
                    reason = "Go ranges over the map too; the loop only adds diagnostics (each names its own export), which the collection sorts on read"
                )]
                for (id, s) in &lookup_table {
                    // It's not an error if the file with multiple `export *`s with duplicate names exports a member with that name itself
                    if id == INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
                        || s.exports_with_duplicate.is_empty()
                        || symbols.lookup(id).is_some()
                    {
                        continue;
                    }
                    for &node in &s.exports_with_duplicate {
                        c.add_diagnostic(create_diagnostic_for_node(Some(node), &diagnostics::MODULE_0_HAS_ALREADY_EXPORTED_A_MEMBER_NAMED_1_CONSIDER_EXPLICITLY_RE_EXPORTING_TO_RESOLVE_THE_AMBIGUITY, &[&s.specifier_text, id]));
                    }
                }
                c.extend_export_symbols(&symbols, Some(&nested_symbols), None, None);
            }
            if let Some(export_star) = export_star
                && export_star.is_type_only()
            {
                let m = st.type_only_export_star_map.get_or_insert_with(FxHashMap::default);
                m.extend(symbols.keys().into_iter().map(|name| (name.to_string(), export_star)));
            }
            Some(symbols)
        }

        let mut st = VisitState {
            visited_symbols: Vec::new(),
            non_type_only_names: collections::new_set_with_size_hint(
                module_symbol.and_then(|m| m.exports()).map_or(0, |e| e.len()),
            ),
            type_only_export_star_map: None,
        };
        let mut original_module: Option<P<Symbol>> = None;
        if let Some(module_symbol) = module_symbol {
            let export_equals =
                module_symbol.exports().and_then(|e| e.lookup(INTERNAL_SYMBOL_NAME_EXPORT_EQUALS));
            if export_equals
                .map(|s| self.resolve_symbol_ex(s, false /*dontResolveAlias*/))
                .is_some()
            {
                original_module = Some(module_symbol);
            }
        }
        // A module defined by an 'export=' consists of one export that needs to be resolved
        let module_symbol = module_symbol
            .map(|m| self.resolve_external_module_symbol(m, false /*dontResolveAlias*/));
        let exports = visit(self, &mut st, module_symbol, None, false).unwrap_or_default();
        // A CommonJS module defined by an 'export=' might also export typedefs, stored on the original module
        if let Some(original_module) = original_module {
            let original_exports = original_module.exports().unwrap();
            if original_exports.len() > 1 {
                for symbol in original_exports.values() {
                    if symbol.name() == INTERNAL_SYMBOL_NAME_EXPORT_EQUALS
                        || symbol.name() == INTERNAL_SYMBOL_NAME_EXPORT_STAR
                    {
                        continue;
                    }
                    let flags = self.get_symbol_flags(symbol);
                    if flags.intersects(SymbolFlags::Type | SymbolFlags::Namespace)
                        && !flags.intersects(SymbolFlags::Value)
                        && exports.lookup(symbol.name()).is_none()
                    {
                        exports.set(symbol.name(), symbol);
                    }
                }
            }
        }
        let mut type_only_export_star_map = st.type_only_export_star_map.unwrap_or_default();
        #[expect(clippy::iter_over_hash_type, reason = "only removes keys from another map")]
        for name in st.non_type_only_names.keys() {
            type_only_export_star_map.remove(name);
        }
        (P::new(exports), type_only_export_star_map)
    }

    /**
     * Extends one symbol table with another while collecting information on name collisions for error message generation into the `lookupTable` argument
     * Not passing `lookupTable` and `exportNode` disables this collection, and just extends the tables
     */
    // checker.go:16558
    fn extend_export_symbols(
        &mut self,
        target: &SymbolTable,
        source: Option<&SymbolTable>,
        lookup_table: Option<&mut ExportCollisionTable>,
        export_node: Option<P<Node>>,
    ) {
        let Some(source) = source else { return };
        let mut lookup_table = lookup_table;
        for (id, source_symbol) in source.entries() {
            if id == INTERNAL_SYMBOL_NAME_DEFAULT {
                continue;
            }
            let target_symbol = target.lookup(id);
            match target_symbol {
                None => {
                    target.set(id, source_symbol);
                    if let (Some(lookup_table), Some(export_node)) =
                        (lookup_table.as_deref_mut(), export_node)
                    {
                        lookup_table.insert(
                            id.to_string(),
                            ExportCollision {
                                specifier_text: tsrs_scanner::get_text_of_node(
                                    export_node.module_specifier().unwrap(),
                                ),
                                exports_with_duplicate: Vec::new(),
                            },
                        );
                    }
                }
                Some(target_symbol) => {
                    if let Some(lookup_table) = lookup_table.as_deref_mut()
                        && let Some(export_node) = export_node
                        && self.resolve_symbol(target_symbol) != self.resolve_symbol(source_symbol)
                    {
                        let s = lookup_table.get_mut(id).unwrap();
                        s.exports_with_duplicate.push(export_node);
                    }
                }
            }
        }
    }

    // checker.go:16578
    pub fn resolve_alias_exported(
        &mut self,
        symbol: Option<P<Symbol>>,
    ) -> (Option<P<Symbol>>, bool) {
        let Some(symbol) = symbol else {
            return (None, false);
        };
        let resolved = self.resolve_alias(symbol);
        (Some(resolved), resolved != self.unknown_symbol)
    }

    // Resolve an alias symbol to the first target symbol in the resolution chain that includes some other
    // meaning. Pure aliases are eagerly resolved and any type-only markers are back-propagated to the original
    // symbol. The function panics if the argument is not a symbol with an alias meaning.
    // checker.go:16589
    pub(crate) fn resolve_alias(&mut self, symbol: P<Symbol>) -> P<Symbol> {
        assert!(symbol.flags().intersects(SymbolFlags::Alias), "Should only get alias here");
        let links = self.alias_symbol_links.get(symbol);
        if links.alias_target.get().is_none() {
            // Counted until the target is final (the circularity report below prints with the provisional target).
            self.alias_cache_blockers += 1;
            if !self.push_type_resolution(symbol.into(), TypeSystemPropertyName::AliasTarget) {
                self.alias_cache_blockers -= 1;
                return self.unknown_symbol;
            }
            let Some(node) = Self::get_declaration_of_alias_symbol(symbol) else {
                panic!(
                    "Unexpected nil in resolveAlias for symbol: {}",
                    self.symbol_to_string(symbol)
                );
            };
            let mut target = self.get_target_of_alias_declaration(Some(node));
            if ast::is_non_local_alias(
                target,
                SymbolFlags::Value | SymbolFlags::Type | SymbolFlags::Namespace,
            ) {
                // When the target is a pure alias, we transitively resolve and propagate any typeOnlyDeclaration
                target = Some(self.resolve_indirection_alias(symbol, target.unwrap()));
            }
            links.alias_target.set(Some(target.unwrap_or(self.unknown_symbol)));
            if !self.pop_type_resolution() {
                let name = self.symbol_to_string(symbol);
                self.error(
                    Some(node),
                    &diagnostics::CIRCULAR_DEFINITION_OF_IMPORT_ALIAS_0,
                    &[&name],
                );
                links.alias_target.set(Some(self.unknown_symbol));
            }
            self.alias_cache_blockers -= 1;
        }
        links.alias_target.get().unwrap()
    }

    // checker.go:16616
    fn resolve_indirection_alias(&mut self, source: P<Symbol>, target: P<Symbol>) -> P<Symbol> {
        let resolved = self.resolve_alias(target);
        let result = self.get_merged_symbol(resolved);
        let target_links = self.alias_symbol_links.get(target);
        if let Some(type_only_declaration) = target_links.type_only_declaration.get() {
            let source_links = self.alias_symbol_links.get(source);
            if source_links.type_only_declaration.get().is_none() {
                source_links.type_only_declaration.set(Some(type_only_declaration));
            }
        }
        result
    }

    // checker.go:16626
    pub(crate) fn try_resolve_alias(&mut self, symbol: P<Symbol>) -> Option<P<Symbol>> {
        let links = self.alias_symbol_links.get(symbol);
        if links.alias_target.get().is_some()
            || self
                .find_resolution_cycle_start_index(
                    symbol.into(),
                    TypeSystemPropertyName::AliasTarget,
                )
                .is_none()
        {
            return Some(self.resolve_alias(symbol));
        }
        None
    }

    // checker.go:16634
    pub(crate) fn resolve_alias_with_deprecation_check(
        &mut self,
        symbol: P<Symbol>,
        location: P<Node>,
    ) -> P<Symbol> {
        if !symbol.flags().intersects(SymbolFlags::Alias)
            || self.is_deprecated_symbol(symbol)
            || Self::get_declaration_of_alias_symbol(symbol).is_none()
        {
            return symbol;
        }
        let mut symbol = symbol;
        let target_symbol = self.resolve_alias(symbol);
        if target_symbol == self.unknown_symbol {
            return target_symbol;
        }
        while symbol.flags().intersects(SymbolFlags::Alias) {
            let target = self.get_immediate_aliased_symbol(symbol);
            if let Some(target) = target {
                if target == target_symbol {
                    break;
                }
                if !target.declarations().is_empty() {
                    if self.is_deprecated_symbol(target) {
                        let declarations = target.declarations();
                        self.add_deprecated_suggestion(location, declarations, target.name());
                        break;
                    }
                    if symbol == target_symbol {
                        break;
                    }
                    symbol = target;
                }
            } else {
                break;
            }
        }
        target_symbol
    }

    /**
     * Gets combined flags of a `symbol` and all alias targets it resolves to. `resolveAlias`
     * is typically recursive over chains of aliases, but stops mid-chain if an alias is merged
     * with another exported symbol, e.g.
     * ```ts
     * // a.ts
     * export const a = 0;
     * // b.ts
     * export { a } from "./a";
     * export type a = number;
     * // c.ts
     * import { a } from "./b";
     * ```
     * Calling `resolveAlias` on the `a` in c.ts would stop at the merged symbol exported
     * from b.ts, even though there is still more alias to resolve. Consequently, if we were
     * trying to determine if the `a` in c.ts has a value meaning, looking at the flags on
     * the local symbol and on the symbol returned by `resolveAlias` is not enough.
     * @returns SymbolFlags.All if `symbol` is an alias that ultimately resolves to `unknown`;
     * combined flags of all alias targets otherwise.
     */
    // checker.go:16686
    pub fn get_symbol_flags(&mut self, symbol: P<Symbol>) -> SymbolFlags {
        self.get_symbol_flags_ex(
            symbol, false, /*excludeTypeOnlyMeanings*/
            false, /*excludeLocalMeanings*/
        )
    }

    // checker.go:16690
    pub(crate) fn get_symbol_flags_ex(
        &mut self,
        symbol: P<Symbol>,
        exclude_type_only_meanings: bool,
        exclude_local_meanings: bool,
    ) -> SymbolFlags {
        let mut seen_symbols: collections::Set<P<Symbol>> = collections::Set::new();
        let mut symbol = symbol;
        let mut flags = SymbolFlags::None;
        if !exclude_local_meanings {
            flags = symbol.flags();
        }
        while symbol.flags().intersects(SymbolFlags::Alias) {
            if exclude_type_only_meanings && self.get_type_only_alias_declaration(symbol).is_some()
            {
                break;
            }
            let resolved = self.resolve_alias(symbol);
            let target =
                self.get_export_symbol_of_value_symbol_if_exported(Some(resolved)).unwrap();
            if target == self.unknown_symbol {
                return SymbolFlags::All;
            }
            if target.flags().intersects(SymbolFlags::Alias) {
                // Optimization - try to avoid creating or adding to `seenSymbols` if possible
                if target == symbol || seen_symbols.has(&target) {
                    break;
                }
                if seen_symbols.is_empty() {
                    seen_symbols.add(symbol);
                }
                seen_symbols.add(target);
            }
            flags |= target.flags();
            symbol = target;
        }
        flags
    }

    // checker.go:16720
    pub(crate) fn get_declaration_of_alias_symbol(symbol: P<Symbol>) -> Option<P<Node>> {
        symbol.declarations().iter().rev().copied().find(|d| ast::is_alias_symbol_declaration(*d))
    }

    // checker.go:16724
    fn get_type_of_symbol_with_deferred_type(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.value_symbol_links.get(symbol);
        if links.resolved_type.get().is_none() {
            let deferred = self.deferred_symbol_links.get(symbol);
            let t = if deferred.parent.get().unwrap().flags().intersects(TypeFlags::Union) {
                self.get_union_type(deferred.constituents.get())
            } else {
                self.get_intersection_type(deferred.constituents.get())
            };
            links.resolved_type.set(Some(t));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:16737
    fn get_write_type_of_symbol_with_deferred_type(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.value_symbol_links.get(symbol);
        if links.write_type().is_none() {
            let deferred = self.deferred_symbol_links.get(symbol);
            let t = if deferred.write_constituents.get().is_empty() {
                self.get_type_of_symbol_with_deferred_type(symbol)
            } else if deferred.parent.get().unwrap().flags().intersects(TypeFlags::Union) {
                self.get_union_type(deferred.write_constituents.get())
            } else {
                self.get_intersection_type(deferred.write_constituents.get())
            };
            links.set_write_type(Some(t));
        }
        links.write_type().unwrap()
    }

    // Distinct write types come only from set accessors, but synthetic union and intersection
    // properties deriving from set accessors will either pre-compute or defer the union or
    // intersection of the writeTypes of their constituents.
    // checker.go:16757
    pub(crate) fn get_write_type_of_symbol(&mut self, symbol: P<Symbol>) -> Option<P<Type>> {
        let check_flags = symbol.check_flags();
        if check_flags.intersects(CheckFlags::SyntheticProperty) {
            if check_flags.intersects(CheckFlags::DeferredType) {
                return Some(self.get_write_type_of_symbol_with_deferred_type(symbol));
            }
            let links = self.value_symbol_links.get(symbol);
            return links.write_type().or(links.resolved_type.get());
        }
        if symbol.flags().intersects(SymbolFlags::Property) {
            let t = self.get_type_of_symbol(symbol);
            return Some(
                self.remove_missing_type(t, symbol.flags().intersects(SymbolFlags::Optional)),
            );
        }
        if symbol.flags().intersects(SymbolFlags::Accessor) {
            if check_flags.intersects(CheckFlags::Instantiated) {
                return Some(self.get_write_type_of_instantiated_symbol(symbol));
            }
            return Some(self.get_write_type_of_accessors(symbol));
        }
        Some(self.get_type_of_symbol(symbol))
    }

    // checker.go:16777
    pub fn get_type_of_symbol_at_location(
        &mut self,
        symbol: P<Symbol>,
        location: Option<P<Node>>,
    ) -> Option<P<Type>> {
        let symbol = self.get_export_symbol_of_value_symbol_if_exported(Some(symbol)).unwrap();
        if let Some(mut location) = location {
            // If we have an identifier or a property access at the given location, if the location is
            // an dotted name expression, and if the location is not an assignment target, obtain the type
            // of the expression (which will reflect control flow analysis). If the expression indeed
            // resolved to the given symbol, return the narrowed type.
            if (ast::is_identifier(location) || ast::is_private_identifier(location))
                && !(ast::is_jsx_tag_name(location)
                    || ast::is_jsx_attribute(location.parent().unwrap())
                    || ast::is_jsx_namespaced_name(location.parent().unwrap()))
            {
                if ast::is_right_side_of_qualified_name_or_property_access(location) {
                    location = location.parent().unwrap();
                }
                if ast::is_expression_node(location)
                    && (!ast::is_assignment_target(location) || ast::is_write_access(location))
                {
                    let t = if ast::is_write_access(location)
                        && location.kind() == Kind::PropertyAccessExpression
                    {
                        self.check_property_access_expression(
                            location,
                            CheckMode::Normal,
                            true, /*writeOnly*/
                        )
                    } else {
                        self.get_type_of_expression(location)
                    };
                    let resolved_symbol =
                        self.symbol_node_links.get(location).resolved_symbol.get();
                    if self.get_export_symbol_of_value_symbol_if_exported(resolved_symbol)
                        == Some(symbol)
                    {
                        return Some(self.remove_optional_type_marker(t));
                    }
                }
            }
            if ast::is_declaration_name(location)
                && ast::is_set_accessor_declaration(location.parent().unwrap())
                && Self::get_annotated_accessor_type_node(location.parent()).is_some()
            {
                return Some(
                    self.get_write_type_of_accessors(location.parent().unwrap().symbol().unwrap()),
                );
            }
            // The location isn't a reference to the given symbol, meaning we're being asked
            // a hypothetical question of what type the symbol would have if there was a reference
            // to it at the given location. Since we have no control flow information for the
            // hypothetical reference (control flow information is created and attached by the
            // binder), we simply return the declared type of the symbol.
            if is_right_side_of_access_expression(location)
                && ast::is_write_access(location.parent().unwrap())
            {
                return self.get_write_type_of_symbol(symbol);
            }
        }
        Some(self.get_non_missing_type_of_symbol(symbol))
    }

    // checker.go:16816
    pub fn get_type_of_symbol(&mut self, symbol: P<Symbol>) -> P<Type> {
        let check_flags = symbol.check_flags();
        // The cached type of an instantiated symbol or a variable / parameter / property (the two branches below that
        // return `resolved_type` when it is set), read here so that this hit returns without a frame.
        let value_symbol = !check_flags.intersects(CheckFlags::DeferredType)
            && (check_flags.intersects(CheckFlags::Instantiated)
                || !check_flags.intersects(CheckFlags::Mapped | CheckFlags::ReverseMapped)
                    && !symbol.flags().intersects(SymbolFlags::Accessor)
                    && symbol.flags().intersects(SymbolFlags::Variable | SymbolFlags::Property));
        if value_symbol && let Some(t) = self.resolved_type_of_value_symbol(symbol) {
            return t;
        }
        if check_flags.intersects(CheckFlags::DeferredType) {
            return self.get_type_of_symbol_with_deferred_type(symbol);
        }
        if check_flags.intersects(CheckFlags::Instantiated) {
            return self.get_type_of_instantiated_symbol(symbol);
        }
        if check_flags.intersects(CheckFlags::Mapped) {
            return self.get_type_of_mapped_symbol(symbol);
        }
        if check_flags.intersects(CheckFlags::ReverseMapped) {
            return self.get_type_of_reverse_mapped_symbol(symbol);
        }
        let flags = symbol.flags();
        if flags.intersects(SymbolFlags::Accessor) {
            return self.get_type_of_accessors(symbol);
        }
        if flags.intersects(SymbolFlags::Variable | SymbolFlags::Property) {
            return self.get_type_of_variable_or_parameter_or_property(symbol);
        }
        if flags.intersects(
            SymbolFlags::Function
                | SymbolFlags::Method
                | SymbolFlags::Class
                | SymbolFlags::Enum
                | SymbolFlags::ValueModule,
        ) {
            return self.get_type_of_func_class_enum_module(symbol);
        }
        if flags.intersects(SymbolFlags::EnumMember) {
            return self.get_type_of_enum_member(symbol);
        }
        if flags.intersects(SymbolFlags::Alias) {
            return self.get_type_of_alias(symbol);
        }
        self.error_type
    }

    // checker.go:16847
    pub(crate) fn get_non_missing_type_of_symbol(&mut self, symbol: P<Symbol>) -> P<Type> {
        let t = self.get_type_of_symbol(symbol);
        self.remove_missing_type(t, symbol.flags().intersects(SymbolFlags::Optional))
    }

    /// The cached answer of `get_type_of_instantiated_symbol` / `get_type_of_variable_or_parameter_or_property`, read
    /// without creating links or assigning an id, so that `get_type_of_symbol` returns it without a frame (the
    /// functions that compute it are out of line).
    #[inline]
    fn resolved_type_of_value_symbol(&self, symbol: P<Symbol>) -> Option<P<Type>> {
        self.value_symbol_links.try_get_if_id_assigned(symbol)?.resolved_type.get()
    }

    /// What `get_type_of_symbol(symbol)` returns, without side effects, when that is a type it already resolved
    /// for a value symbol (the fast path at its top); None otherwise.
    pub(crate) fn cached_type_of_value_symbol(&self, symbol: P<Symbol>) -> Option<P<Type>> {
        let check_flags = symbol.check_flags();
        let value_symbol = !check_flags.intersects(CheckFlags::DeferredType)
            && (check_flags.intersects(CheckFlags::Instantiated)
                || !check_flags.intersects(CheckFlags::Mapped | CheckFlags::ReverseMapped)
                    && !symbol.flags().intersects(SymbolFlags::Accessor)
                    && symbol.flags().intersects(SymbolFlags::Variable | SymbolFlags::Property));
        if value_symbol { self.resolved_type_of_value_symbol(symbol) } else { None }
    }

    // checker.go:16851
    #[inline(never)]
    fn get_type_of_instantiated_symbol(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.value_symbol_links.get(symbol);
        if links.resolved_type.get().is_none() {
            #[cfg(feature = "site-counts")]
            if let Some(loc) = self.inst_symbol_sites.remove(&symbol) {
                crate::sitecount::hit_at("inst-symbol-type-resolved", "", loc);
            }
            let target_type = self.get_type_of_symbol(links.target().unwrap());
            let t = self.instantiate_type(target_type, links.mapper());
            links.resolved_type.set(Some(t));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:16859
    fn get_write_type_of_instantiated_symbol(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.value_symbol_links.get(symbol);
        if links.write_type().is_none() {
            let target_type = self.get_write_type_of_symbol(links.target().unwrap()).unwrap();
            let t = self.instantiate_type(target_type, links.mapper());
            links.set_write_type(Some(t));
        }
        links.write_type().unwrap()
    }

    // checker.go:16867
    #[inline(never)]
    fn get_type_of_variable_or_parameter_or_property(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.value_symbol_links.get(symbol);
        if links.resolved_type.get().is_none() {
            let t = self.get_type_of_variable_or_parameter_or_property_worker(symbol);
            // For a contextually typed parameter it is possible that a type has already
            // been assigned (in assignTypeToParameterAndFixTypeParameters), and we want
            // to preserve this type. In fact, we need to _prefer_ that type, but it won't
            // be assigned until contextual typing is complete, so we need to defer in
            // cases where contextual typing may take place.
            if links.resolved_type.get().is_none()
                && !self.is_parameter_of_context_sensitive_signature(symbol)
            {
                links.resolved_type.set(Some(t));
            }
            return t;
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:16887
    fn is_parameter_of_context_sensitive_signature(&mut self, symbol: P<Symbol>) -> bool {
        let Some(mut decl) = symbol.value_declaration() else {
            return false;
        };
        if ast::is_binding_element(decl) {
            decl = ast::walk_up_binding_elements_and_patterns(decl);
        }
        if ast::is_parameter_declaration(decl) {
            return self
                .is_context_sensitive_function_or_object_literal_method(decl.parent().unwrap());
        }
        false
    }

    // checker.go:16901
    fn get_type_of_variable_or_parameter_or_property_worker(
        &mut self,
        symbol: P<Symbol>,
    ) -> P<Type> {
        // Handle prototype property
        if symbol.flags().intersects(SymbolFlags::Prototype) {
            return self.get_type_of_prototype_property(symbol);
        }
        // CommonsJS require and module both have type any.
        if symbol == self.require_symbol {
            return self.any_type;
        }
        assert!(symbol.value_declaration().is_some());
        let declaration = symbol.value_declaration().unwrap();
        if ast::is_source_file(declaration)
            && ast::is_json_source_file(declaration.as_source_file_p())
        {
            let statements = declaration.statements();
            if statements.is_empty() {
                return self.empty_object_type;
            }
            let t = self.check_expression(statements[0].expression().unwrap());
            let t = self.get_widened_literal_type(t);
            return self.get_widened_type(t);
        }
        // Handle variable, parameter or property
        if !self.push_type_resolution(symbol.into(), TypeSystemPropertyName::Type) {
            return self.report_circularity_error(symbol);
        }
        if symbol.flags().intersects(SymbolFlags::ModuleExports) {
            if symbol.name() == "exports" {
                let module_symbol = symbol.value_declaration().unwrap().symbol().unwrap();
                let resolved = self
                    .resolve_external_module_symbol(module_symbol, false /*dontResolveAlias*/);
                return self.get_type_of_symbol(resolved);
            }
            return self.new_anonymous_type(Some(symbol), symbol.members(), &[], &[], &[]);
        }
        let result = match declaration.kind() {
            Kind::Parameter
            | Kind::PropertyDeclaration
            | Kind::PropertySignature
            | Kind::VariableDeclaration
            | Kind::BindingElement => {
                let report_errors = !self.is_parameter_of_context_sensitive_signature(symbol);
                self.get_widened_type_for_variable_like_declaration(declaration, report_errors) // only report diagnostics for context-insensitive parameters - context-sensitive ones may have their type fixed to something else
            }
            Kind::PropertyAssignment => {
                self.check_property_assignment(declaration, CheckMode::Normal)
            }
            Kind::ShorthandPropertyAssignment => self.check_shorthand_property_assignment(
                declaration,
                true, /*inDestructuringPattern*/
                CheckMode::Normal,
            ),
            Kind::MethodDeclaration => {
                self.check_object_literal_method(declaration, CheckMode::Normal)
            }
            Kind::ExportAssignment => {
                if let Some(type_node) = declaration.type_node() {
                    self.get_type_from_type_node(type_node)
                } else {
                    let span = self.census_begin(crate::workcensus::Cat::ExportAssign, || {
                        crate::workcensus::CKey::Node(declaration)
                    });
                    let t = self.check_expression_cached(declaration.expression().unwrap());
                    if let Some(timing) = self.census_end(span) {
                        let expr_kind =
                            ast::skip_parentheses(declaration.expression().unwrap()).kind() as u64;
                        self.census.as_mut().unwrap().record(
                            crate::workcensus::Cat::ExportAssign,
                            crate::workcensus::CKey::Node(declaration),
                            timing,
                            expr_kind,
                            0,
                            0,
                        );
                    }
                    self.widen_type_for_variable_like_declaration(
                        Some(t),
                        declaration,
                        false, /*reportErrors*/
                    )
                }
            }
            Kind::BinaryExpression | Kind::CallExpression => {
                self.get_widened_type_for_assignment_declaration(symbol)
            }
            Kind::JsxAttribute => self.check_jsx_attribute(declaration, CheckMode::Normal),
            Kind::EnumMember => self.get_type_of_enum_member(symbol),
            _ => panic!(
                "Unhandled case in getTypeOfVariableOrParameterOrPropertyWorker: {}",
                declaration.kind_string()
            ),
        };
        if !self.pop_type_resolution() {
            return self.report_circularity_error(symbol);
        }
        result
    }

    // Return the type associated with a variable, parameter, or property declaration. In the simple case this is the type
    // specified in a type annotation or inferred from an initializer. However, in the case of a destructuring declaration it
    // is a bit more involved. For example:
    //
    //	var [x, s = ""] = [1, "one"];
    //
    // Here, the array literal [1, "one"] is contextually typed by the type [any, string], which is the implied type of the
    // binding pattern [x, s = ""]. Because the contextual type is a tuple type, the resulting type of [1, "one"] is the
    // tuple type [number, string]. Thus, the type inferred for 'x' is number and the type inferred for 's' is string.
    // checker.go:16970
    pub(crate) fn get_widened_type_for_variable_like_declaration(
        &mut self,
        declaration: P<Node>,
        report_errors: bool,
    ) -> P<Type> {
        let t = self.get_type_for_variable_like_declaration(
            declaration,
            true, /*includeOptionality*/
            CheckMode::Normal,
        );
        self.widen_type_for_variable_like_declaration(t, declaration, report_errors)
    }

    // Return the inferred type for a variable, parameter, or property declaration
    // checker.go:16975
    pub(crate) fn get_type_for_variable_like_declaration(
        &mut self,
        declaration: P<Node>,
        include_optionality: bool,
        check_mode: CheckMode,
    ) -> Option<P<Type>> {
        // A variable declared in a for..in statement is of type string, or of type keyof T when the
        // right hand expression is of a type parameter type.
        if ast::is_variable_declaration(declaration) {
            let grand_parent = declaration.parent().unwrap().parent().unwrap();
            match grand_parent.kind() {
                Kind::ForInStatement => {
                    let t = self.check_expression_ex(
                        grand_parent.expression().unwrap(),
                        check_mode, /*checkMode*/
                    );
                    let t = self.get_non_nullable_type_if_needed(t);
                    let index_type = self.get_index_type(t);
                    if index_type.flags().intersects(TypeFlags::TypeParameter | TypeFlags::Index) {
                        return Some(self.get_extract_string_type(index_type));
                    }
                    return Some(self.string_type);
                }
                Kind::ForOfStatement => {
                    // checkRightHandSideOfForOf will return undefined if the for-of expression type was
                    // missing properties/signatures required to get its iteratedType (like
                    // [Symbol.iterator] or next). This may be because we accessed properties from anyType,
                    // or it may have led to an error inside getElementTypeOfIterable.
                    return Some(self.check_right_hand_side_of_for_of(grand_parent));
                }
                _ => {}
            }
        } else if ast::is_binding_element(declaration) {
            return self.get_type_for_binding_element(declaration);
        }
        let is_property = ast::is_property_declaration(declaration)
            && !ast::has_accessor_modifier(declaration)
            || ast::is_property_signature_declaration(declaration);
        let is_optional = include_optionality && is_optional_declaration(declaration);
        // Use type from type annotation if one is present
        let declared_type = self.try_get_type_from_type_node(declaration);
        if ast::is_catch_clause_variable_declaration_or_binding_element(declaration) {
            if let Some(declared_type) = declared_type {
                // If the catch clause is explicitly annotated with any or unknown, accept it, otherwise error.
                if declared_type.flags().intersects(TypeFlags::AnyOrUnknown) {
                    return Some(declared_type);
                }
                return Some(self.error_type);
            }
            // If the catch clause is not explicitly annotated, treat it as though it were explicitly
            // annotated with unknown or any, depending on useUnknownInCatchVariables.
            if self.use_unknown_in_catch_variables {
                return Some(self.unknown_type);
            }
            return Some(self.any_type);
        }
        if let Some(declared_type) = declared_type {
            return Some(self.add_optionality_ex(declared_type, is_property, is_optional));
        }
        if self.no_implicit_any
            && ast::is_variable_declaration(declaration)
            && !ast::is_binding_pattern(declaration.name().unwrap())
            && !self
                .get_combined_modifier_flags_cached(declaration)
                .intersects(ModifierFlags::Export)
            && !declaration.flags().intersects(NodeFlags::Ambient)
        {
            // If --noImplicitAny is on or the declaration is in a Javascript file,
            // use control flow tracked 'any' type for non-ambient, non-exported var or let variables with no
            // initializer or a 'null' or 'undefined' initializer.
            let initializer = declaration.initializer();
            if !self.get_combined_node_flags_cached(declaration).intersects(NodeFlags::Constant)
                && (initializer.is_none() || self.is_null_or_undefined(initializer.unwrap()))
            {
                return Some(self.auto_type);
            }
            // Use control flow tracked 'any[]' type for non-ambient, non-exported variables with an empty array
            // literal initializer.
            if initializer.is_some_and(is_empty_array_literal) {
                return Some(self.auto_array_type);
            }
        }
        if ast::is_parameter_declaration(declaration) {
            let Some(declaration_symbol) = declaration.symbol() else {
                // parameters of function types defined in JSDoc in TS files don't have symbols
                return None;
            };
            let fn_ = declaration.parent().unwrap();
            // For a parameter of a set accessor, use the type of the get accessor if one is present
            if ast::is_set_accessor_declaration(fn_) && self.has_bindable_name(fn_) {
                let parent_symbol =
                    self.get_symbol_of_declaration(declaration.parent().unwrap()).unwrap();
                let getter = ast::get_declaration_of_kind(parent_symbol, Kind::GetAccessor);
                if let Some(getter) = getter {
                    let getter_signature = self.get_signature_from_declaration(getter);
                    let this_parameter = Self::get_accessor_this_parameter(fn_);
                    if let Some(this_parameter) = this_parameter
                        && declaration == this_parameter
                    {
                        // Use the type from the *getter*
                        assert!(this_parameter.type_node().is_none());
                        return Some(
                            self.get_type_of_symbol(getter_signature.this_parameter().unwrap()),
                        );
                    }
                    return Some(self.get_return_type_of_signature(getter_signature));
                }
            }
            if let Some(t) = self.get_parameter_type_of_full_signature(fn_, declaration) {
                return Some(t);
            }
            // Use contextual parameter type if one is available
            let t = if declaration_symbol.name() == INTERNAL_SYMBOL_NAME_THIS {
                self.get_contextual_this_parameter_type(fn_)
            } else {
                self.get_contextually_typed_parameter_type(declaration)
            };
            if let Some(t) = t {
                return Some(self.add_optionality_ex(t, false /*isProperty*/, is_optional));
            }
        }
        // Use the type of the initializer expression if one is present and the declaration is
        // not a parameter of a contextually typed function
        if declaration.initializer().is_some() {
            let initializer_type = self.check_declaration_initializer(
                declaration,
                check_mode,
                None, /*contextualType*/
            );
            let t = self.widen_type_inferred_from_initializer(declaration, initializer_type);
            return Some(self.add_optionality_ex(t, is_property, is_optional));
        }
        if self.no_implicit_any && ast::is_property_declaration(declaration) {
            // We have a property declaration with no type annotation or initializer, in noImplicitAny mode or a .js file.
            // Use control flow analysis of this.xxx assignments in the constructor or static block to determine the type of the property.
            if !ast::has_static_modifier(declaration) {
                let constructor = ast::find_constructor_declaration(declaration.parent().unwrap());
                let t = if let Some(constructor) = constructor {
                    self.get_flow_type_in_constructor(declaration.symbol().unwrap(), constructor)
                } else if declaration.modifier_flags().intersects(ModifierFlags::Ambient) {
                    self.get_type_of_property_in_base_class(declaration.symbol().unwrap())
                } else {
                    None
                };
                let t = t?;
                return Some(self.add_optionality_ex(t, true /*isProperty*/, is_optional));
            }
            let static_blocks: Vec<P<Node>> = declaration
                .parent()
                .unwrap()
                .members()
                .iter()
                .copied()
                .filter(|m| ast::is_class_static_block_declaration(*m))
                .collect();
            let t = if !static_blocks.is_empty() {
                self.get_flow_type_in_static_blocks(declaration.symbol().unwrap(), &static_blocks)
            } else if declaration.modifier_flags().intersects(ModifierFlags::Ambient) {
                self.get_type_of_property_in_base_class(declaration.symbol().unwrap())
            } else {
                None
            };
            let t = t?;
            return Some(self.add_optionality_ex(t, true /*isProperty*/, is_optional));
        }
        if ast::is_jsx_attribute(declaration) {
            // if JSX attribute doesn't have initializer, by default the attribute will have boolean value of true.
            // I.e <Elem attr /> is sugar for <Elem attr={true} />
            return Some(self.true_type);
        }
        // If the declaration specifies a binding pattern and is not a parameter of a contextually
        // typed function, use the type implied by the binding pattern
        if ast::is_binding_pattern(declaration.name().unwrap()) {
            return Some(self.get_type_from_binding_pattern(
                declaration.name().unwrap(),
                false, /*includePatternInType*/
                true,  /*reportErrors*/
            ));
        }
        // No type specified and nothing can be inferred
        None
    }

    // checker.go:17120
    pub(crate) fn check_declaration_initializer(
        &mut self,
        declaration: P<Node>,
        check_mode: CheckMode,
        contextual_type: Option<P<Type>>,
    ) -> P<Type> {
        let initializer = declaration.initializer().unwrap();
        let census_span =
            self.census_begin(crate::workcensus::Cat::VarInit, || crate::workcensus::CKey::None);
        let quick = self.get_quick_type_of_expression(initializer);
        let quick_used = quick.is_some() as u64;
        let t = match quick {
            Some(t) => t,
            None => {
                if let Some(contextual_type) = contextual_type {
                    self.check_expression_with_contextual_type(
                        initializer,
                        contextual_type,
                        None, /*inferenceContext*/
                        check_mode,
                    )
                } else {
                    self.check_expression_cached_ex(initializer, check_mode)
                }
            }
        };
        if let Some(timing) = self.census_end(census_span) {
            self.census.as_mut().unwrap().record(
                crate::workcensus::Cat::VarInit,
                crate::workcensus::CKey::None,
                timing,
                quick_used,
                0,
                0,
            );
        }
        if ast::is_parameter_declaration(ast::get_root_declaration(declaration)) {
            let name = declaration.name().unwrap();
            match name.kind() {
                Kind::ObjectBindingPattern => {
                    if is_object_literal_type(t) {
                        return self.pad_object_literal_type(t, name);
                    }
                }
                Kind::ArrayBindingPattern if is_tuple_type(t) => {
                    return self.pad_tuple_type(t, name);
                }
                _ => {}
            }
        }
        t
    }

    // checker.go:17146
    fn pad_object_literal_type(&mut self, t: P<Type>, pattern: P<Node>) -> P<Type> {
        let mut missing_elements: Vec<P<Node>> = Vec::new();
        missing_elements.extend(pattern.elements().iter().filter_map(|&e| {
            if has_dot_dot_dot_token(e) {
                return None;
            }
            let name = self.get_property_name_from_binding_element(e);
            if name != INTERNAL_SYMBOL_NAME_MISSING && self.get_property_of_type(t, &name).is_none()
            {
                Some(e)
            } else {
                None
            }
        }));
        if missing_elements.is_empty() {
            return t;
        }
        let members = SymbolTable::new();
        for prop in self.get_properties_of_object_type(t).iter().copied() {
            members.set(prop.name(), prop);
        }
        for e in missing_elements {
            let name = self.get_property_name_from_binding_element(e);
            let symbol =
                self.new_symbol(SymbolFlags::Property | SymbolFlags::Optional, alloc_str(&name));
            let resolved_type = self.get_type_from_binding_element(
                e, false, /*includePatternInType*/
                true,  /*reportErrors*/
            );
            self.value_symbol_links.get(symbol).resolved_type.set(Some(resolved_type));
            members.set(symbol.name(), symbol);
        }
        let index_infos = self.get_index_infos_of_type(t);
        let result = self.new_anonymous_type(t.symbol(), Some(members), &[], &[], index_infos);
        result.object_flags.set(t.object_flags_lazy() & !ObjectFlags::WeakTypeMemo);
        result
    }

    // checker.go:17174
    fn get_property_name_from_binding_element(&mut self, e: P<Node>) -> String {
        let expr_type =
            self.get_literal_type_from_property_name(e.property_name_or_name().unwrap());
        if is_type_usable_as_property_name(expr_type) {
            return get_property_name_from_type(expr_type).into_owned();
        }
        INTERNAL_SYMBOL_NAME_MISSING.to_string()
    }

    // checker.go:17182
    fn pad_tuple_type(&mut self, t: P<Type>, pattern: P<Node>) -> P<Type> {
        let pattern_elements = pattern.elements();
        if t.target_tuple_type().combined_flags.get().intersects(ElementFlags::Variable)
            || Self::get_type_reference_arity(t) >= pattern_elements.len()
        {
            return t;
        }
        let mut element_types = self.get_element_types(t).to_vec();
        let mut element_infos: Vec<TupleElementInfo> =
            t.target_tuple_type().element_infos.get().to_vec();
        let mut i = Self::get_type_reference_arity(t);
        while i < pattern_elements.len() {
            let e = pattern_elements[i];
            if i < pattern_elements.len() - 1
                || !(ast::is_binding_element(e) && has_dot_dot_dot_token(e))
            {
                let mut element_type = self.any_type;
                if !ast::is_omitted_expression(e) && Self::has_default_value(e) {
                    element_type = self.get_type_from_binding_element(
                        e, false, /*includePatternInType*/
                        false, /*reportErrors*/
                    );
                }
                element_types.push(element_type);
                element_infos.push(TupleElementInfo {
                    flags: ElementFlags::Optional,
                    labeled_declaration: None,
                });
                if !ast::is_omitted_expression(e) && !Self::has_default_value(e) {
                    let any_type = self.any_type;
                    self.report_implicit_any(e, any_type, WideningKind::Normal);
                }
            }
            i += 1;
        }
        let readonly = t.target_tuple_type().readonly.get();
        self.create_tuple_type_ex(&element_types, &element_infos, readonly)
    }

    // checker.go:17206
    pub(crate) fn widen_type_inferred_from_initializer(
        &mut self,
        declaration: P<Node>,
        t: P<Type>,
    ) -> P<Type> {
        let widened = self.get_widened_literal_type_for_initializer(declaration, t);
        if ast::is_in_js_file(declaration) {
            if self.is_empty_literal_type(widened) {
                let any_type = self.any_type;
                self.report_implicit_any(declaration, any_type, WideningKind::Normal);
                return self.any_type;
            }
            if self.is_empty_array_literal_type(widened) {
                let any_array_type = self.any_array_type;
                self.report_implicit_any(declaration, any_array_type, WideningKind::Normal);
                return self.any_array_type;
            }
        }
        widened
    }

    // checker.go:17221
    pub(crate) fn get_widened_literal_type_for_initializer(
        &mut self,
        declaration: P<Node>,
        t: P<Type>,
    ) -> P<Type> {
        if self.get_combined_node_flags_cached(declaration).intersects(NodeFlags::Constant)
            || is_declaration_readonly(declaration)
        {
            return t;
        }
        self.get_widened_literal_type(t)
    }

    // checker.go:17228
    fn get_type_of_func_class_enum_module(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.value_symbol_links.get(symbol);
        if links.resolved_type.get().is_none() {
            let t = self.get_type_of_func_class_enum_module_worker(symbol);
            links.resolved_type.set(Some(t));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:17236
    fn get_type_of_func_class_enum_module_worker(&mut self, symbol: P<Symbol>) -> P<Type> {
        if symbol.flags().intersects(SymbolFlags::Module)
            && is_shorthand_ambient_module_symbol(symbol)
        {
            return self.any_type;
        } else if symbol.flags().intersects(SymbolFlags::ValueModule)
            && symbol.value_declaration().is_some_and(|d| {
                ast::is_source_file(d) && d.as_source_file().common_js_module_indicator().is_some()
            })
        {
            let resolved_module =
                self.resolve_external_module_symbol(symbol, false /*dontResolveAlias*/);
            if resolved_module != symbol {
                return self.get_type_of_symbol(resolved_module);
            }
        }
        let t = self.new_object_type(ObjectFlags::Anonymous, Some(symbol));
        if symbol.flags().intersects(SymbolFlags::Class) {
            let base_type_variable = self.get_base_type_variable_of_class(symbol);
            if let Some(base_type_variable) = base_type_variable {
                return self.get_intersection_type(&[t, base_type_variable]);
            }
            return t;
        }
        if self.strict_null_checks && symbol.flags().intersects(SymbolFlags::Optional) {
            return self.get_optional_type(t, true /*isProperty*/);
        }
        t
    }

    // checker.go:17260
    pub(crate) fn get_base_type_variable_of_class(&mut self, symbol: P<Symbol>) -> Option<P<Type>> {
        let declared_type = self.get_declared_type_of_class_or_interface(symbol);
        let base_constructor_type = self.get_base_constructor_type_of_class(declared_type);
        if base_constructor_type.flags().intersects(TypeFlags::TypeVariable) {
            return Some(base_constructor_type);
        } else if base_constructor_type.flags().intersects(TypeFlags::Intersection) {
            return base_constructor_type
                .types()
                .iter()
                .copied()
                .find(|t| t.flags().intersects(TypeFlags::TypeVariable));
        }
        None
    }

    /**
     * The base constructor of a class can resolve to
     * * undefinedType if the class has no extends clause,
     * * errorType if an error occurred during resolution of the extends expression,
     * * nullType if the extends expression is the null value,
     * * anyType if the extends expression has type any, or
     * * an object type with at least one construct signature.
     */
    // checker.go:17281
    pub(crate) fn get_base_constructor_type_of_class(&mut self, t: P<Type>) -> P<Type> {
        let data = t.as_interface_type();
        if let Some(resolved) = data.resolved_base_constructor_type.get() {
            return resolved;
        }
        let Some(base_type_node) = get_base_type_node_of_class(t) else {
            data.resolved_base_constructor_type.set(Some(self.undefined_type));
            return data.resolved_base_constructor_type.get().unwrap();
        };
        if !self.push_type_resolution(t.into(), TypeSystemPropertyName::ResolvedBaseConstructorType)
        {
            return self.error_type;
        }
        let base_constructor_type = self.check_expression(base_type_node.expression().unwrap());
        if base_constructor_type.flags().intersects(TypeFlags::Object | TypeFlags::Intersection) {
            // Resolving the members of a class requires us to resolve the base class of that class.
            // We force resolution here such that we catch circularities now.
            self.resolve_structured_type_members(base_constructor_type);
        }
        if !self.pop_type_resolution() {
            let t_symbol = t.symbol().unwrap();
            let name = self.symbol_to_string(t_symbol);
            self.error(
                t_symbol.value_declaration(),
                &diagnostics::X_0_IS_REFERENCED_DIRECTLY_OR_INDIRECTLY_IN_ITS_OWN_BASE_EXPRESSION,
                &[&name],
            );
            if data.resolved_base_constructor_type.get().is_none() {
                data.resolved_base_constructor_type.set(Some(self.error_type));
            }
            return data.resolved_base_constructor_type.get().unwrap();
        }
        if !base_constructor_type.flags().intersects(TypeFlags::Any)
            && base_constructor_type != self.null_widening_type
            && !self.is_constructor_type(base_constructor_type)
        {
            let type_name = self.type_to_string(base_constructor_type, None);
            let err = self.error(
                base_type_node.expression(),
                &diagnostics::TYPE_0_IS_NOT_A_CONSTRUCTOR_FUNCTION_TYPE,
                &[&type_name],
            );
            if base_constructor_type.flags().intersects(TypeFlags::TypeParameter) {
                let constraint = self.get_constraint_from_type_parameter(base_constructor_type);
                let mut ctor_return = self.unknown_type;
                if let Some(constraint) = constraint {
                    let ctor_sigs =
                        self.get_signatures_of_type(constraint, SignatureKind::Construct);
                    if !ctor_sigs.is_empty() {
                        ctor_return = self.get_return_type_of_signature(ctor_sigs[0]);
                    }
                }
                let base_symbol = base_constructor_type.symbol().unwrap();
                if !base_symbol.declarations().is_empty() {
                    let first_declaration = base_symbol.declarations()[0];
                    let symbol_name = self.symbol_to_string(base_symbol);
                    let return_name = self.type_to_string(ctor_return, None);
                    err.add_related_info(create_diagnostic_for_node(Some(first_declaration), &diagnostics::DID_YOU_MEAN_FOR_0_TO_BE_CONSTRAINED_TO_TYPE_NEW_ARGS_COLON_ANY_1, &[&symbol_name, &return_name]));
                }
            }
            if data.resolved_base_constructor_type.get().is_none() {
                data.resolved_base_constructor_type.set(Some(self.error_type));
            }
            return data.resolved_base_constructor_type.get().unwrap();
        }
        if data.resolved_base_constructor_type.get().is_none() {
            data.resolved_base_constructor_type.set(Some(base_constructor_type));
        }
        data.resolved_base_constructor_type.get().unwrap()
    }

    // checker.go:17333
    pub(crate) fn is_function_type(&mut self, t: P<Type>) -> bool {
        t.flags().intersects(TypeFlags::Object)
            && !self.get_signatures_of_type(t, SignatureKind::Call).is_empty()
    }

    // checker.go:17337
    pub(crate) fn is_constructor_type(&mut self, t: P<Type>) -> bool {
        if !self.get_signatures_of_type(t, SignatureKind::Construct).is_empty() {
            return true;
        }
        if t.flags().intersects(TypeFlags::TypeVariable) {
            let constraint = self.get_base_constraint_of_type(t);
            return constraint.is_some_and(|c| self.is_mixin_constructor_type(c));
        }
        false
    }

    // A type is a mixin constructor if it has a single construct signature taking no type parameters and a single
    // rest parameter of type any[].
    // checker.go:17350
    pub(crate) fn is_mixin_constructor_type(&mut self, t: P<Type>) -> bool {
        let signatures = self.get_signatures_of_type(t, SignatureKind::Construct);
        if signatures.len() == 1 {
            let s = signatures[0];
            if s.type_parameters().is_empty()
                && s.parameters.get().len() == 1
                && signature_has_rest_parameter(s)
            {
                let param_type = self.get_type_of_parameter(s.parameters.get()[0]);
                return is_type_any(Some(param_type))
                    || self.get_element_type_of_array_type(param_type) == Some(self.any_type);
            }
        }
        false
    }
}

// checker.go:17362
pub(crate) fn signature_has_rest_parameter(sig: P<Signature>) -> bool {
    sig.flags().intersects(SignatureFlags::HasRestParameter)
}

impl Checker {
    // checker.go:17366
    pub(crate) fn get_type_of_parameter(&mut self, symbol: P<Symbol>) -> P<Type> {
        let declaration = symbol.value_declaration();
        let t = self.get_type_of_symbol(symbol);
        self.add_optionality_ex(
            t,
            false,
            declaration.is_some_and(|d| d.initializer().is_some() || is_optional_declaration(d)),
        )
    }

    // checker.go:17371
    pub(crate) fn get_constraint_of_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        let flags = t.flags();
        if flags.intersects(TypeFlags::TypeParameter) {
            return self.get_constraint_of_type_parameter(t);
        } else if flags.intersects(TypeFlags::IndexedAccess) {
            return self.get_constraint_of_indexed_access(t);
        } else if flags.intersects(TypeFlags::Conditional) {
            return self.get_constraint_of_conditional_type(t);
        }
        self.get_base_constraint_of_type(t)
    }

    // checker.go:17383
    pub(crate) fn get_constraint_of_type_parameter(
        &mut self,
        type_parameter: P<Type>,
    ) -> Option<P<Type>> {
        if self.has_non_circular_base_constraint(type_parameter) {
            return self.get_constraint_from_type_parameter(type_parameter);
        }
        None
    }

    // checker.go:17390
    pub(crate) fn has_non_circular_base_constraint(&mut self, t: P<Type>) -> bool {
        self.get_resolved_base_constraint(t, crate::checker_13::BaseConstraintStack::default())
            != self.circular_constraint_type
    }

    // This is a worker function. Use getConstraintOfTypeParameter which guards against circular constraints
    // checker.go:17395
    pub(crate) fn get_constraint_from_type_parameter(&mut self, t: P<Type>) -> Option<P<Type>> {
        if !t.flags().intersects(TypeFlags::TypeParameter) {
            return None;
        }

        let tp = t.as_type_parameter();
        if tp.constraint.get().is_none() {
            let mut constraint: Option<P<Type>>;
            if let Some(target) = tp.target.get() {
                let target_constraint = self.get_constraint_of_type_parameter(target);
                constraint = target_constraint.map(|tc| self.instantiate_type(tc, tp.mapper.get()));
            } else {
                let constraint_declaration = Self::get_constraint_declaration(t);
                if let Some(constraint_declaration) = constraint_declaration {
                    let mut c = self.get_type_from_type_node(constraint_declaration);
                    if c.flags().intersects(TypeFlags::Any) && !self.is_error_type(c) {
                        // use stringNumberSymbolType as the base constraint for mapped type key constraints (unknown isn;t assignable to that, but `any` was),
                        // use unknown otherwise
                        if ast::is_mapped_type_node(
                            constraint_declaration.parent().unwrap().parent().unwrap(),
                        ) {
                            c = self.string_number_symbol_type;
                        } else {
                            c = self.unknown_type;
                        }
                    }
                    constraint = Some(c);
                } else {
                    constraint = self.get_inferred_type_parameter_constraint(t, false);
                }
            }
            if constraint.is_none() {
                constraint = Some(self.no_constraint_type);
            }
            tp.constraint.set(constraint);
        }
        if tp.constraint.get() != Some(self.no_constraint_type) {
            return tp.constraint.get();
        }
        None
    }
}
