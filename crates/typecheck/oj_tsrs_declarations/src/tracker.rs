use crate::*;

// Non-function declarations in tracker.go (hand-ported in types.rs):
//   type SymbolTrackerImpl (tracker.go:12)
//   type SymbolTrackerSharedState (tracker.go:239)

impl SymbolTrackerImpl {
    // tracker.go:27
    // PopErrorFallbackNode implements checker.SymbolTracker.
    pub(crate) fn pop_error_fallback_node(&self) {
        let mut fallback_stack = self.fallback_stack.borrow_mut();
        let len = fallback_stack.len();
        fallback_stack.truncate(len - 1);
    }

    // tracker.go:32
    // PushErrorFallbackNode implements checker.SymbolTracker.
    pub(crate) fn push_error_fallback_node(&self, node: Option<P<Node>>) {
        self.fallback_stack.borrow_mut().push(node);
    }

    // tracker.go:37
    // ReportCyclicStructureError implements checker.SymbolTracker.
    pub(crate) fn report_cyclic_structure_error(&self) {
        let location = self.error_location();
        if let Some(location) = location {
            self.state.add_diagnostic(create_diagnostic_for_node(location, &diagnostics::THE_INFERRED_TYPE_OF_0_REFERENCES_A_TYPE_WITH_A_CYCLIC_STRUCTURE_WHICH_CANNOT_BE_TRIVIALLY_SERIALIZED_A_TYPE_ANNOTATION_IS_NECESSARY, &[&self.error_declaration_name_with_fallback()]));
        }
    }

    // tracker.go:45
    // ReportInaccessibleThisError implements checker.SymbolTracker.
    pub(crate) fn report_inaccessible_this_error(&self) {
        let location = self.error_location();
        if let Some(location) = location {
            self.state.add_diagnostic(create_diagnostic_for_node(location, &diagnostics::THE_INFERRED_TYPE_OF_0_REFERENCES_AN_INACCESSIBLE_1_TYPE_A_TYPE_ANNOTATION_IS_NECESSARY, &[&self.error_declaration_name_with_fallback(), &"this"]));
        }
    }

    // tracker.go:53
    // ReportInaccessibleUniqueSymbolError implements checker.SymbolTracker.
    pub(crate) fn report_inaccessible_unique_symbol_error(&self) {
        let location = self.error_location();
        if let Some(location) = location {
            self.state.add_diagnostic(create_diagnostic_for_node(location, &diagnostics::THE_INFERRED_TYPE_OF_0_REFERENCES_AN_INACCESSIBLE_1_TYPE_A_TYPE_ANNOTATION_IS_NECESSARY, &[&self.error_declaration_name_with_fallback(), &"unique symbol"]));
        }
    }

    // tracker.go:60
    fn is_bound_expando(&self, c: &mut Checker, node: P<Node>) -> bool {
        if !(ast::is_expando_property_declaration(node)
            && ast::is_property_access_expression(node.as_binary_expression().left))
        {
            return false;
        }
        // Match transformExpandoAssignment: only an assignment rooted at an identifier (`f.x = ...`) can bind an expando
        // property; `this.x = ...`, `super.x = ...`, `f().x = ...` and the like have no referenced declaration.
        let ns = ast::get_leftmost_access_expression(node.as_binary_expression().left);
        if !ast::is_identifier(ns) {
            return false;
        }
        let Some(ref_) = self.resolver.get_referenced_value_declaration_unsafe(c, ns) else {
            return false;
        };
        self.resolver.is_expando_function_declaration_unsafe(c, ref_)
    }

    // tracker.go:77
    fn is_child_of_bound_expando(&self, c: &mut Checker, node: P<Node>) -> bool {
        ast::find_ancestor_or_quit(node, |n| {
            if ast::is_source_file(n) || ast::is_block(n) {
                return FindAncestorResult::Quit;
            }
            ast::to_find_ancestor_result(self.is_bound_expando(c, n))
        })
        .is_some()
    }

    // tracker.go:87
    // ReportInferenceFallback implements checker.SymbolTracker.
    pub(crate) fn report_inference_fallback(&self, c: &mut Checker, node: P<Node>) {
        if !self.state.isolated_declarations {
            return;
        }
        if ast::get_source_file_of_node(node) != self.state.current_source_file.get() {
            return; // Nested error on a declaration in another file - ignore, will be reemitted if file is in the output file set
        }
        if self.state.resolver.is_expando_function_declaration_unsafe(c, node) {
            // within a node builder call that should already lock the checker, use the unsafe call
            let report_expando_function_errors =
                Rc::clone(self.state.report_expando_function_errors.get().unwrap());
            report_expando_function_errors(c, node);
        }
        if !self.is_child_of_bound_expando(c, node) {
            // expando props get an error when their host is visited by the above, this prevents a follow-on error on a non-inferrable expression
            self.state.add_diagnostic((self.get_isolated_declaration_error)(c, node));
        }
    }

    // tracker.go:103
    // ReportLikelyUnsafeImportRequiredError implements checker.SymbolTracker.
    pub(crate) fn report_likely_unsafe_import_required_error(
        &self,
        specifier: &str,
        symbol_name: &str,
    ) {
        let location = self.error_location();
        if let Some(location) = location {
            if symbol_name.is_empty() {
                self.state.add_diagnostic(create_diagnostic_for_node(location, &diagnostics::THE_INFERRED_TYPE_OF_0_CANNOT_BE_NAMED_WITHOUT_A_REFERENCE_TO_1_THIS_IS_LIKELY_NOT_PORTABLE_A_TYPE_ANNOTATION_IS_NECESSARY, &[&self.error_declaration_name_with_fallback(), &specifier]));
            } else {
                self.state.add_diagnostic(create_diagnostic_for_node(location, &diagnostics::THE_INFERRED_TYPE_OF_0_CANNOT_BE_NAMED_WITHOUT_A_REFERENCE_TO_2_FROM_1_THIS_IS_LIKELY_NOT_PORTABLE_A_TYPE_ANNOTATION_IS_NECESSARY, &[&self.error_declaration_name_with_fallback(), &specifier, &symbol_name]));
            }
        }
    }

    // tracker.go:115
    // ReportNonSerializableProperty implements checker.SymbolTracker.
    pub(crate) fn report_non_serializable_property(&self, property_name: &str) {
        let location = self.error_location();
        if let Some(location) = location {
            self.state.add_diagnostic(create_diagnostic_for_node(location, &diagnostics::THE_TYPE_OF_THIS_NODE_CANNOT_BE_SERIALIZED_BECAUSE_ITS_PROPERTY_0_CANNOT_BE_SERIALIZED, &[&property_name]));
        }
    }

    // tracker.go:123
    // ReportNonlocalAugmentation implements checker.SymbolTracker.
    pub(crate) fn report_nonlocal_augmentation(
        &self,
        containing_file: P<SourceFile>,
        parent_symbol: P<Symbol>,
        augmenting_symbol: P<Symbol>,
    ) {
        let primary_declaration = parent_symbol
            .declarations()
            .iter()
            .copied()
            .find(|&d| ast::get_source_file_of_node(d) == Some(containing_file));
        let augmenting_declarations: Vec<P<Node>> = augmenting_symbol
            .declarations()
            .iter()
            .copied()
            .filter(|&d| ast::get_source_file_of_node(d) != Some(containing_file))
            .collect();
        if let Some(primary_declaration) = primary_declaration
            && !augmenting_declarations.is_empty()
        {
            for augmentations in augmenting_declarations {
                let diag = create_diagnostic_for_node(augmentations, &diagnostics::DECLARATION_AUGMENTS_DECLARATION_IN_ANOTHER_FILE_THIS_CANNOT_BE_SERIALIZED, &[]);
                let related = create_diagnostic_for_node(primary_declaration, &diagnostics::THIS_IS_THE_DECLARATION_BEING_AUGMENTED_CONSIDER_MOVING_THE_AUGMENTING_DECLARATION_INTO_THE_SAME_FILE, &[]);
                diag.add_related_info(related);
                self.state.add_diagnostic(diag);
            }
        }
    }

    // tracker.go:137
    // ReportPrivateInBaseOfClassExpression implements checker.SymbolTracker.
    pub(crate) fn report_private_in_base_of_class_expression(&self, property_name: &str) {
        let location = self.error_location();
        if let Some(location) = location {
            let diag = create_diagnostic_for_node(location, &diagnostics::PROPERTY_0_OF_EXPORTED_ANONYMOUS_CLASS_TYPE_MAY_NOT_BE_PRIVATE_OR_PROTECTED, &[&property_name]);
            if ast::is_variable_declaration(location.parent().unwrap()) {
                let related = create_diagnostic_for_node(
                    location,
                    &diagnostics::ADD_A_TYPE_ANNOTATION_TO_THE_VARIABLE_0,
                    &[&self.error_declaration_name_with_fallback()],
                );
                diag.add_related_info(related);
            }
            self.state.add_diagnostic(diag);
        }
    }

    // tracker.go:150
    // ReportTruncationError implements checker.SymbolTracker.
    pub(crate) fn report_truncation_error(&self) {
        let location = self.error_location();
        if let Some(location) = location {
            self.state.add_diagnostic(create_diagnostic_for_node(location, &diagnostics::THE_INFERRED_TYPE_OF_THIS_NODE_EXCEEDS_THE_MAXIMUM_LENGTH_THE_COMPILER_WILL_SERIALIZE_AN_EXPLICIT_TYPE_ANNOTATION_IS_NEEDED, &[]));
        }
    }

    // tracker.go:157
    fn error_fallback_node(&self) -> Option<P<Node>> {
        let fallback_stack = self.fallback_stack.borrow();
        if !fallback_stack.is_empty() {
            return fallback_stack[fallback_stack.len() - 1];
        }
        None
    }

    // tracker.go:164
    fn error_location(&self) -> Option<P<Node>> {
        let mut location = self.state.error_name_node.get();
        if location.is_none() {
            location = self.error_fallback_node();
        }
        location
    }

    // tracker.go:172
    fn error_declaration_name_with_fallback(&self) -> String {
        if self.state.error_name_node.get().is_some() {
            return scanner::declaration_name_to_string(self.state.error_name_node.get());
        }
        if self.error_fallback_node().is_some()
            && ast::get_name_of_declaration(self.error_fallback_node()).is_some()
        {
            return scanner::declaration_name_to_string(ast::get_name_of_declaration(
                self.error_fallback_node(),
            ));
        }
        if self.error_fallback_node().is_some()
            && ast::is_export_assignment(self.error_fallback_node().unwrap())
        {
            if self.error_fallback_node().unwrap().as_export_assignment().is_export_equals {
                return "export=".to_string();
            }
            return "default".to_string();
        }
        "(Missing)".to_string() // same fallback declarationNameToString uses when node is zero-width (ie, nameless)
    }

    // tracker.go:189
    // TrackSymbol implements checker.SymbolTracker.
    pub(crate) fn track_symbol(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
    ) -> bool {
        if symbol.flags().intersects(SymbolFlags::TypeParameter) {
            return false;
        }
        // When watching for a class expression symbol, record its usage without
        // reporting accessibility errors — the caller will handle visibility by
        // wrapping the class in a namespace.
        if self.watched_class_symbol.get() == Some(symbol) {
            self.class_symbol_tracked.set(true);
            return false;
        }

        self.handle_symbol_accessibility_error(&self.resolver.is_symbol_accessible(
            c,
            symbol,
            enclosing_declaration,
            meaning,
            true, /*shouldComputeAliasToMarkVisible*/
        ))
    }

    // tracker.go:204
    pub(crate) fn handle_symbol_accessibility_error(
        &self,
        symbol_accessibility_result: &SymbolAccessibilityResult,
    ) -> bool {
        if symbol_accessibility_result.accessibility == SymbolAccessibility::Accessible {
            // Add aliases back onto the possible imports list if they're not there so we can try them again with updated visibility info
            if !symbol_accessibility_result.aliases_to_make_visible.is_empty() {
                for &ref_ in &symbol_accessibility_result.aliases_to_make_visible {
                    // core.AppendIfUnique
                    let mut late_marked_statements = self.state.late_marked_statements.borrow_mut();
                    if !late_marked_statements.contains(&ref_) {
                        late_marked_statements.push(ref_);
                    }
                }
            }
            // TODO: Do all these accessibility checks inside/after the first pass in the checker when declarations are enabled, if possible

            // The checker should issue errors on unresolvable names, skip the declaration emit error for using a private/unreachable name for those
        } else if symbol_accessibility_result.accessibility != SymbolAccessibility::NotResolved {
            // Report error
            let get_symbol_accessibility_diagnostic =
                self.state.get_symbol_accessibility_diagnostic.borrow().clone();
            let error_info = get_symbol_accessibility_diagnostic(symbol_accessibility_result);
            if let Some(info) = error_info {
                let mut diag_node = symbol_accessibility_result.error_node;
                if diag_node.is_none() {
                    diag_node = info.error_node;
                }
                if let Some(type_name) = info.type_name {
                    self.state.add_diagnostic(create_diagnostic_for_node(
                        diag_node,
                        info.diagnostic_message,
                        &[
                            &scanner::get_text_of_node(type_name),
                            &symbol_accessibility_result.error_symbol_name,
                            &symbol_accessibility_result.error_module_name,
                        ],
                    ));
                } else {
                    self.state.add_diagnostic(create_diagnostic_for_node(
                        diag_node,
                        info.diagnostic_message,
                        &[
                            &symbol_accessibility_result.error_symbol_name,
                            &symbol_accessibility_result.error_module_name,
                        ],
                    ));
                }
                return true;
            }
        }
        false
    }
}

// tracker.go:235
// SIG: node is `impl Into<Option<P<Node>>>` (was `P<Node>`): Go passes a possibly-nil node (handleSymbolAccessibilityError's
// diagNode) and NewDiagnosticForNode accepts nil; existing `P<Node>` callers compile unchanged.
pub fn create_diagnostic_for_node(
    node: impl Into<Option<P<Node>>>,
    message: &'static Message,
    args: &[&dyn Display],
) -> P<Diagnostic> {
    checker::new_diagnostic_for_node(node.into(), Some(message), args)
}

impl SymbolTrackerSharedState {
    // tracker.go:251
    pub(crate) fn add_diagnostic(&self, diag: P<Diagnostic>) {
        self.diagnostics.borrow_mut().push(diag);
    }
}

// tracker.go:255
pub fn new_symbol_tracker(
    resolver: Resolver,
    state: P<SymbolTrackerSharedState>,
) -> P<SymbolTrackerImpl> {
    P::new(SymbolTrackerImpl {
        resolver,
        state,
        fallback_stack: RefCell::new(Vec::new()),
        watched_class_symbol: Cell::new(None),
        class_symbol_tracked: Cell::new(false),
        get_isolated_declaration_error: create_get_isolated_declaration_errors(resolver),
    })
}
