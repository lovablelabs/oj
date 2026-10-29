#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

use crate::*;

// Non-function declarations in transform.go:1-1339 (hand-ported in types.rs):
//   type ReferencedFilePair (transform.go:23)
//   type OutputPaths (transform.go:28)
//   type DeclarationEmitHost (transform.go:34)
//   type thisPropertyAssignmentKey (transform.go:46)
//   type DeclarationTransformer (transform.go:63)
//   const declarationEmitNodeBuilderFlags (transform.go:215)
//   const declarationEmitInternalNodeBuilderFlags (transform.go:223)

// transform.go:53
pub(crate) fn get_this_property_assignment_key(
    name: Option<P<Node>>,
    node: P<Node>,
    is_static: bool,
) -> ThisPropertyAssignmentKey {
    // Go dereferences `name` here (IsPrivateIdentifier(nil) panics).
    let is_private = ast::is_private_identifier(name.unwrap());
    if let Some(name) = name
        && !ast::is_dynamic_name(name)
        && let Some(name_text) = ast::try_get_text_of_property_name(name)
    {
        return ThisPropertyAssignmentKey { name: name_text, node: None, is_static, is_private };
    }
    ThisPropertyAssignmentKey { name: String::new(), node: Some(node), is_static, is_private }
}

// transform.go:103
// TODO: Convert to transformers.TransformerFactory signature to allow more automatic composition with other transforms
pub fn new_declaration_transformer(
    host: &'static dyn DeclarationEmitHost,
    resolver: Resolver,
    compiler_options: P<CompilerOptions>,
    declaration_file_path: &str,
) -> P<DeclarationTransformer> {
    let context = Some(resolver.r.emit_context());
    let state = P::new(SymbolTrackerSharedState {
        late_marked_statements: RefCell::new(Vec::new()),
        diagnostics: RefCell::new(Vec::new()),
        // Go's zero value is a nil func (calling it panics); throwDiagnostic panics the same way.
        get_symbol_accessibility_diagnostic: RefCell::new(Rc::new(
            |r: &SymbolAccessibilityResult| Some(throw_diagnostic(r)),
        )),
        error_name_node: Cell::new(None),
        isolated_declarations: compiler_options.isolated_declarations.is_true(),
        strip_internal: compiler_options.strip_internal.is_true(),
        current_source_file: Cell::new(None),
        resolver,
        report_expando_function_errors: OnceCell::new(),
    });
    let tracker = new_symbol_tracker(resolver, state);
    // TODO: Use new host GetOutputPathsFor method instead of passing in entrypoint paths (which will also better support bundled emit)
    let tx = P::new(DeclarationTransformer {
        base: Transformer::default(),
        host,
        compiler_options,
        tracker,
        state,
        resolver,
        declaration_file_path: declaration_file_path.to_string(),
        needs_declare: Cell::new(false),
        needs_scope_fix_marker: Cell::new(false),
        result_has_scope_marker: Cell::new(false),
        enclosing_declaration: Cell::new(None),
        result_has_external_module_indicator: Cell::new(false),
        suppress_new_diagnostic_contexts: Cell::new(false),
        witnessed_cjs_exports: RefCell::new(Set::new()),
        late_statement_replacement_map: RefCell::new(FxHashMap::default()),
        expando_hosts: RefCell::new(FxHashMap::default()),
        expando_members: RefCell::new(FxHashMap::default()),
        deferred_expando_assignments: RefCell::new(FxHashMap::default()),
        seen_properties: RefCell::new(Set::new()),
        this_property_assignments_collected: RefCell::new(Vec::new()),
        raw_referenced_files: RefCell::new(Vec::new()),
        raw_type_reference_directives: RefCell::new(Vec::new()),
        raw_lib_reference_directives: RefCell::new(Vec::new()),
        binding_name_visitor: OnceCell::new(),
        expression_visitor: OnceCell::new(),
        cjs_export_assignment_visitor: OnceCell::new(),
        export_stripping_visitor: OnceCell::new(),
        this_property_visitor: OnceCell::new(),
        cjs_export_assignment: Cell::new(None),
        cjs_export_members: RefCell::new(Vec::new()),
        cjs_export_assignment_name: Cell::new(None),
        declare_stripping_visitor: OnceCell::new(),
        in_class_expression_declaration: Cell::new(false),
    });
    let report_expando_function_errors: Rc<dyn Fn(&mut Checker, P<Node>)> = Rc::new(
        move |c: &mut Checker, node: P<Node>| {
            if !tx.state.isolated_declarations {
                return;
            }
            let props = resolver.get_properties_of_container_function(c, Some(node));
            for p in props {
                if ast::is_expando_property_declaration(p.value_declaration()) {
                    let mut error_target = p.value_declaration().unwrap();
                    if ast::is_binary_expression(error_target) {
                        error_target = error_target.as_binary_expression().left;
                    }
                    tx.state.add_diagnostic(create_diagnostic_for_node(error_target, &diagnostics::ASSIGNING_PROPERTIES_TO_FUNCTIONS_WITHOUT_DECLARING_THEM_IS_NOT_SUPPORTED_WITH_ISOLATED_DECLARATIONS_ADD_AN_EXPLICIT_DECLARATION_FOR_THE_PROPERTIES_ASSIGNED_TO_THIS_FUNCTION, &[]));
                }
            }
        },
    );
    let _ = tx.state.report_expando_function_errors.set(report_expando_function_errors);
    tx.get()
        .base
        .new_transformer(Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)), context);
    let _ = tx.binding_name_visitor.set(tx.emit_context().new_node_visitor(Rc::new(
        move |_: &mut NodeVisitor, n: P<Node>| Some(tx.visit_binding_name(n)),
    )));
    let _ = tx.expression_visitor.set(tx.emit_context().new_node_visitor(Rc::new(
        move |_: &mut NodeVisitor, n: P<Node>| tx.visit_nested_expression(Some(n)),
    )));
    let _ = tx.export_stripping_visitor.set(tx.emit_context().new_node_visitor(Rc::new(
        move |_: &mut NodeVisitor, n: P<Node>| Some(tx.strip_export_modifiers(n)),
    )));
    let _ = tx.this_property_visitor.set(tx.emit_context().new_node_visitor(Rc::new(
        move |_: &mut NodeVisitor, n: P<Node>| tx.visit_this_property_assignments(n),
    )));
    let _ = tx.cjs_export_assignment_visitor.set(tx.emit_context().new_node_visitor(Rc::new(
        move |_: &mut NodeVisitor, n: P<Node>| tx.visit_cjs_export_assignments(Some(n)),
    )));
    let _ = tx.declare_stripping_visitor.set(tx.emit_context().new_node_visitor(Rc::new(
        move |_: &mut NodeVisitor, n: P<Node>| Some(tx.strip_declare_modifiers(n)),
    )));
    tx
}

impl DeclarationTransformer {
    // transform.go:142
    pub fn get_diagnostics(&self) -> Vec<P<Diagnostic>> {
        self.state.diagnostics.borrow().clone()
    }

    // transform.go:146
    pub(crate) fn should_strip_internal(&self, node: Option<P<Node>>) -> bool {
        self.state.strip_internal
            && node.is_some()
            && self.is_internal_declaration(node, self.state.current_source_file.get().unwrap())
    }

    // transform.go:150
    fn is_internal_declaration(&self, node: Option<P<Node>>, source_file: P<SourceFile>) -> bool {
        let Some(node) = node else {
            return false;
        };
        let parse_tree_node = self.emit_context().most_original(Some(node)).unwrap();
        if !ast::is_parse_tree_node(parse_tree_node) {
            return false;
        }
        if parse_tree_node.kind() == Kind::Parameter {
            let params = parse_tree_node.parent().unwrap().parameters();
            let param_idx = params.iter().position(|p| *p == parse_tree_node);
            let mut previous_sibling: Option<P<Node>> = None;
            if let Some(param_idx) = param_idx
                && param_idx > 0
            {
                previous_sibling = Some(params[param_idx - 1]);
            }

            let text = source_file.text();
            let mut comment_ranges: Vec<CommentRange> = Vec::new();

            if let Some(previous_sibling) = previous_sibling {
                // to handle
                // ... parameters, /** @internal */
                // public param: string
                let trailing_pos = scanner::skip_trivia_ex(
                    text,
                    previous_sibling
                        .end()
                        .expect_source("parsed nodes have source positions")
                        .checked_add(1)
                        .expect("source position overflow"),
                    Some(&scanner::SkipTriviaOptions {
                        stop_at_comments: true,
                        ..Default::default()
                    }),
                );
                comment_ranges.extend(scanner::get_trailing_comment_ranges(text, trailing_pos));
                comment_ranges.extend(scanner::get_leading_comment_ranges(
                    text,
                    node.pos().expect_source("parsed nodes have source positions"),
                ));
            } else {
                let trailing_pos = scanner::skip_trivia_ex(
                    text,
                    node.pos().expect_source("parsed nodes have source positions"),
                    Some(&scanner::SkipTriviaOptions {
                        stop_at_comments: true,
                        ..Default::default()
                    }),
                );
                comment_ranges.extend(scanner::get_trailing_comment_ranges(text, trailing_pos));
            }

            if !comment_ranges.is_empty() {
                return has_internal_annotation(
                    comment_ranges[comment_ranges.len() - 1],
                    source_file,
                );
            }
            return false;
        }

        Self::get_leading_comment_ranges_of_node(parse_tree_node, source_file)
            .into_iter()
            .any(|comment_range| has_internal_annotation(comment_range, source_file))
    }

    // transform.go:203
    fn get_leading_comment_ranges_of_node(
        node: P<Node>,
        source_file: P<SourceFile>,
    ) -> Vec<CommentRange> {
        if node.kind() == Kind::JsxText {
            return Vec::new();
        }
        scanner::get_leading_comment_ranges(
            source_file.text(),
            node.pos().expect_source("parsed nodes have source positions"),
        )
        .collect()
    }
}

// transform.go:210
fn has_internal_annotation(comment_range: CommentRange, source_file: P<SourceFile>) -> bool {
    let comment = &source_file.text()[comment_range.pos() as usize..comment_range.end() as usize];
    comment.contains("@internal")
}

impl DeclarationTransformer {
    // transform.go:226
    // functions as both `visitDeclarationStatements` and `transformRoot`, utilitzing SyntaxList nodes
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        match node.kind() {
            Kind::SourceFile => Some(self.visit_source_file(node.as_source_file_p())),
            // statements we keep but do something to
            Kind::FunctionDeclaration
            | Kind::ModuleDeclaration
            | Kind::ImportEqualsDeclaration
            | Kind::InterfaceDeclaration
            | Kind::ClassDeclaration
            | Kind::JSTypeAliasDeclaration
            | Kind::TypeAliasDeclaration
            | Kind::EnumDeclaration
            | Kind::VariableStatement
            | Kind::ImportDeclaration
            | Kind::JSImportDeclaration
            | Kind::ExportDeclaration
            | Kind::ExportAssignment => self.visit_declaration_statements(node),
            // statements we elide
            Kind::BreakStatement
            | Kind::ContinueStatement
            | Kind::DebuggerStatement
            | Kind::DoStatement
            | Kind::EmptyStatement
            | Kind::ForInStatement
            | Kind::ForOfStatement
            | Kind::ForStatement
            | Kind::IfStatement
            | Kind::LabeledStatement
            | Kind::ReturnStatement
            | Kind::SwitchStatement
            | Kind::ThrowStatement
            | Kind::TryStatement
            | Kind::WhileStatement
            | Kind::WithStatement
            | Kind::NotEmittedStatement
            | Kind::Block
            | Kind::MissingDeclaration
            | Kind::ExpressionStatement => None,
            // parts of things, things we just visit children of
            _ => self.visit_declaration_subtree(node),
        }
    }
}

// transform.go:276
fn throw_diagnostic(result: &SymbolAccessibilityResult) -> P<SymbolAccessibilityDiagnostic> {
    let _ = result;
    panic!("Diagnostic emitted without context")
}

impl DeclarationTransformer {
    // transform.go:280
    fn visit_source_file(&self, node: P<SourceFile>) -> P<Node> {
        self.cjs_export_assignment_name.set(None);
        if node.is_declaration_file() {
            return node.as_node();
        }

        self.needs_declare.set(true);
        self.needs_scope_fix_marker.set(false);
        self.result_has_scope_marker.set(false);
        self.enclosing_declaration.set(Some(node.as_node()));
        *self.state.get_symbol_accessibility_diagnostic.borrow_mut() =
            Rc::new(|r: &SymbolAccessibilityResult| Some(throw_diagnostic(r)));
        self.result_has_external_module_indicator.set(false);
        self.suppress_new_diagnostic_contexts.set(false);
        *self.state.late_marked_statements.borrow_mut() = Vec::new();
        *self.late_statement_replacement_map.borrow_mut() = FxHashMap::default();
        *self.expando_hosts.borrow_mut() = FxHashMap::default();
        *self.expando_members.borrow_mut() = FxHashMap::default();
        *self.deferred_expando_assignments.borrow_mut() = FxHashMap::default();
        *self.raw_referenced_files.borrow_mut() = Vec::new();
        *self.raw_type_reference_directives.borrow_mut() = Vec::new();
        *self.raw_lib_reference_directives.borrow_mut() = Vec::new();
        self.witnessed_cjs_exports.borrow_mut().clear();
        self.state.current_source_file.set(Some(node));
        self.collect_file_references(node);
        self.resolver.precalculate_declaration_emit_visibility(
            self.emit_context().most_original(Some(node.as_node())).unwrap().as_source_file_p(),
        );
        let updated = self.transform_source_file(node);
        self.state.current_source_file.set(None);
        updated
    }

    // transform.go:310
    fn collect_file_references(&self, source_file: P<SourceFile>) {
        self.raw_referenced_files.borrow_mut().extend(
            source_file
                .referenced_files()
                .iter()
                .map(|ref_| ReferencedFilePair { file: source_file, ref_: *ref_ }),
        );
        self.raw_type_reference_directives
            .borrow_mut()
            .extend_from_slice(source_file.type_reference_directives());
        self.raw_lib_reference_directives
            .borrow_mut()
            .extend_from_slice(source_file.lib_reference_directives());
    }
}

// transform.go:316
fn node_or_syntax_list_children(node: P<Node>) -> Vec<P<Node>> {
    if ast::is_syntax_list(node) {
        return node.as_syntax_list().children.to_vec();
    }
    vec![node]
}

// transform.go:323
fn flatten_syntax_lists(nodes: &[P<Node>]) -> Vec<P<Node>> {
    nodes.iter().flat_map(|n| node_or_syntax_list_children(*n)).collect()
}

impl DeclarationTransformer {
    // transform.go:327
    fn append_cjs_exports(&self, combined_statements: P<NodeList>) -> P<NodeList> {
        let mut combined_statements = combined_statements;
        let mut result: Vec<P<Node>> = Vec::new();
        if let Some(cjs_export_assignment) = self.cjs_export_assignment.get() {
            result.push(cjs_export_assignment);
        }
        result.extend_from_slice(&self.cjs_export_members.borrow());
        result.extend_from_slice(combined_statements.nodes());
        let statement_nodes = flatten_syntax_lists(&result);
        if statement_nodes.len() != combined_statements.nodes().len() {
            combined_statements = self.factory().new_node_list(statement_nodes);
        }
        combined_statements
    }

    // transform.go:341
    fn transform_source_file(&self, node: P<SourceFile>) -> P<Node> {
        self.cjs_export_assignment.set(None);
        self.cjs_export_assignment_name.set(None);
        *self.cjs_export_members.borrow_mut() = Vec::new();
        self.cjs_export_assignment_visitor().visit_node(Some(node.as_node())); // collect nested module.exports= assignments
        self.expression_visitor().visit_node(Some(node.as_node())); // collect expando members (requires any export assignment be located in advance)
        let statements = self.visitor().visit_nodes(Some(node.statements)).unwrap();
        let mut combined_statements =
            self.transform_and_replace_late_painted_statements(statements);
        combined_statements = self.append_cjs_exports(combined_statements);
        combined_statements.loc.set(statements.loc.get()); // setTextRange
        if ast::is_external_or_common_js_module(node) {
            if ast::is_in_js_file(node.as_node()) {
                let export_equals = node
                    .symbol()
                    .and_then(|s| s.exports())
                    .and_then(|e| e.lookup(INTERNAL_SYMBOL_NAME_EXPORT_EQUALS));
                if let Some(export_equals) = export_equals {
                    let declarations = export_equals.declarations().to_vec();
                    if declarations.len() > 1 {
                        for &node in &declarations {
                            self.state.add_diagnostic(create_diagnostic_for_node(node, &diagnostics::MULTIPLE_MODULE_EXPORTS_ASSIGNMENTS_CANNOT_BE_SERIALIZED_FOR_DECLARATION_EMIT, &[]));
                        }
                    }
                }
            }
            if !self.result_has_external_module_indicator.get()
                || (self.needs_scope_fix_marker.get() && !self.result_has_scope_marker.get())
            {
                let marker = create_empty_exports(self.factory().as_node_factory());
                let mut new_list = combined_statements.nodes().to_vec();
                new_list.push(marker);
                let with_marker = self.factory().new_node_list(new_list);
                with_marker.loc.set(combined_statements.loc.get());
                combined_statements = with_marker;
            }
        }
        let output_file_path =
            tspath::get_directory_path(&tspath::normalize_slashes(&self.declaration_file_path));
        let result = self.factory().update_source_file(
            node.as_node(),
            combined_statements,
            node.end_of_file_token,
        );
        result.as_source_file().lib_reference_directives.set(alloc_vec(self.get_lib_references()));
        result
            .as_source_file()
            .type_reference_directives
            .set(alloc_vec(self.get_type_references()));
        result.as_source_file().is_declaration_file.set(true);
        result
            .as_source_file()
            .referenced_files
            .set(alloc_vec(self.get_referenced_files(&output_file_path)));
        // Go's deferred reset.
        self.cjs_export_assignment.set(None);
        self.cjs_export_assignment_name.set(None);
        *self.cjs_export_members.borrow_mut() = Vec::new();
        result
    }
}

// transform.go:382
pub(crate) fn create_empty_exports(factory: &NodeFactory) -> P<Node> {
    factory.new_export_declaration(
        None, /*isTypeOnly*/
        false,
        Some(factory.new_named_exports(factory.new_node_list(Vec::new()))),
        None,
        None,
    )
}

impl DeclarationTransformer {
    // transform.go:386
    pub(crate) fn transform_and_replace_late_painted_statements(
        &self,
        statements: P<NodeList>,
    ) -> P<NodeList> {
        // This is a `while` loop because `handleSymbolAccessibilityError` can see additional import aliases marked as visible during
        // error handling which must now be included in the output and themselves checked for errors.
        // For example:
        // ```
        // module A {
        //   export module Q {}
        //   import B = Q;
        //   import C = B;
        //   export import D = C;
        // }
        // ```
        // In such a scenario, only Q and D are initially visible, but we don't consider imports as private names - instead we say they if they are referenced they must
        // be recorded. So while checking D's visibility we mark C as visible, then we must check C which in turn marks B, completing the chain of
        // dependent imports and allowing a valid declaration file output. Today, this dependent alias marking only happens for internal import aliases.
        loop {
            let next = {
                let mut late_marked_statements = self.state.late_marked_statements.borrow_mut();
                if late_marked_statements.is_empty() {
                    break;
                }
                late_marked_statements.remove(0)
            };

            let save_needs_declare = self.needs_declare.get();
            self.needs_declare.set(next.parent().is_some_and(ast::is_source_file));

            let result = self.transform_top_level_declaration(next);

            self.needs_declare.set(save_needs_declare);
            let original = self.emit_context().most_original(Some(next)).unwrap();
            let id = ast::get_node_id(original);
            self.late_statement_replacement_map.borrow_mut().insert(id, result);
        }

        // And lastly, we need to get the final form of all those indetermine import declarations from before and add them to the output list
        // (and remove them from the set to examine for outter declarations)
        let mut results: Vec<P<Node>> = Vec::with_capacity(statements.nodes().len());
        for &statement in statements.nodes() {
            if !ast::is_late_visibility_painted_statement(statement) {
                results.push(statement);
                continue;
            }
            let original = self.emit_context().most_original(Some(statement)).unwrap();
            let id = ast::get_node_id(original);
            let entry = self.late_statement_replacement_map.borrow().get(&id).copied();
            let Some(replacement) = entry else {
                results.push(statement);
                continue; // not replaced
            };
            let Some(replacement) = replacement else {
                continue; // deleted
            };
            if replacement.kind() == Kind::SyntaxList {
                if !self.needs_scope_fix_marker.get()
                    || !self.result_has_external_module_indicator.get()
                {
                    for &elem in replacement.as_syntax_list().children {
                        if needs_scope_marker(elem) {
                            self.needs_scope_fix_marker.set(true);
                        }
                        if ast::is_source_file(statement.parent().unwrap())
                            && ast::is_external_module_indicator(elem)
                        {
                            self.result_has_external_module_indicator.set(true);
                        }
                    }
                }
                results.extend_from_slice(replacement.as_syntax_list().children);
            } else {
                if needs_scope_marker(replacement) {
                    self.needs_scope_fix_marker.set(true);
                }
                if ast::is_source_file(statement.parent().unwrap())
                    && ast::is_external_module_indicator(replacement)
                {
                    self.result_has_external_module_indicator.set(true);
                }
                results.push(replacement);
            }
        }

        self.factory().new_node_list(results)
    }

    // transform.go:464
    fn get_referenced_files(&self, output_file_path: &str) -> Vec<P<FileReference>> {
        let mut results: Vec<P<FileReference>> = Vec::new();
        // Handle path rewrites for triple slash ref comments
        let raw_referenced_files = self.raw_referenced_files.borrow().clone();
        results.extend(raw_referenced_files.into_iter().filter_map(|pair| {
            let source_file = pair.file;
            let ref_ = pair.ref_;

            if !ref_.preserve {
                return None;
            }

            let file = self.host.get_source_file_from_reference(source_file, ref_)?;

            let decl_file_name: String = if file.is_declaration_file() {
                file.file_name().to_string()
            } else {
                let paths = self.host.get_output_paths_for(file, true);
                // Try to use output path for referenced file, or output js path if that doesn't exist, or the input path if all else fails
                let mut name = paths.declaration_file_path().to_string();
                if name.is_empty() {
                    name = paths.js_file_path().to_string();
                }
                if name.is_empty() {
                    name = file.file_name().to_string();
                }
                name
            };
            // Should only be missing if the source file is missing a fileName (at which point we can't name a reference to it anyway)
            // TODO: Shouldn't this be a crash or assert instead of a silent continue?
            if decl_file_name.is_empty() {
                return None;
            }

            let file_name = tspath::get_relative_path_to_directory_or_url(
                output_file_path,
                &decl_file_name,
                false, // TODO: Probably unsafe to assume this isn't a URL, but that's what strada does
                &tspath::ComparePathsOptions {
                    current_directory: self.host.get_current_directory().to_string(),
                    use_case_sensitive_file_names: self.host.use_case_sensitive_file_names(),
                },
            );

            Some(P::new(FileReference {
                text_range: tsrs_core::TextRange::synthetic(),
                file_name,
                resolution_mode: ref_.resolution_mode,
                preserve: ref_.preserve,
            }))
        }));
        results
    }

    // transform.go:519
    fn get_lib_references(&self) -> Vec<P<FileReference>> {
        let mut result: Vec<P<FileReference>> = Vec::new();
        // clone retained references
        result.extend(self.raw_lib_reference_directives.borrow().iter().filter_map(|ref_| {
            if !ref_.preserve {
                return None;
            }
            Some(P::new(FileReference {
                text_range: tsrs_core::TextRange::synthetic(),
                file_name: ref_.file_name.clone(),
                resolution_mode: ref_.resolution_mode,
                preserve: ref_.preserve,
            }))
        }));
        result
    }

    // transform.go:535
    fn get_type_references(&self) -> Vec<P<FileReference>> {
        let mut result: Vec<P<FileReference>> = Vec::new();
        // clone retained references
        result.extend(self.raw_type_reference_directives.borrow().iter().filter_map(|ref_| {
            if !ref_.preserve {
                return None;
            }
            Some(P::new(FileReference {
                text_range: tsrs_core::TextRange::synthetic(),
                file_name: ref_.file_name.clone(),
                resolution_mode: ref_.resolution_mode,
                preserve: ref_.preserve,
            }))
        }));
        result
    }

    // transform.go:551
    pub(crate) fn setup_diagnostic_context(&self, input: P<Node>) -> (bool, Box<dyn FnMut() + '_>) {
        let can_produce_diagnostic = can_produce_diagnostics(input);
        let old_within_object_literal_type = self.suppress_new_diagnostic_contexts.get();
        let should_enter_suppress_new_diagnostics_context_context =
            (input.kind() == Kind::TypeLiteral || input.kind() == Kind::MappedType)
                && !(input.parent().unwrap().kind() == Kind::TypeAliasDeclaration
                    || input.parent().unwrap().kind() == Kind::JSTypeAliasDeclaration);

        let old_diag = self.state.get_symbol_accessibility_diagnostic.borrow().clone();
        if can_produce_diagnostic && !self.suppress_new_diagnostic_contexts.get() {
            *self.state.get_symbol_accessibility_diagnostic.borrow_mut() =
                create_get_symbol_accessibility_diagnostic_for_node(input);
        }
        let old_name = self.state.error_name_node.get();

        if should_enter_suppress_new_diagnostics_context_context {
            self.suppress_new_diagnostic_contexts.set(true);
        }

        (
            can_produce_diagnostic,
            Box::new(move || {
                *self.state.get_symbol_accessibility_diagnostic.borrow_mut() = Rc::clone(&old_diag);
                self.state.error_name_node.set(old_name);
                self.suppress_new_diagnostic_contexts.set(old_within_object_literal_type);
            }),
        )
    }

    // transform.go:573
    fn visit_declaration_subtree(&self, input: P<Node>) -> Option<P<Node>> {
        if self.should_strip_internal(Some(input)) {
            return None;
        }
        if ast::is_declaration(input) {
            if is_declaration_and_not_visible(self.emit_context(), self.resolver, input) {
                return None;
            }
            if ast::has_dynamic_name(input) {
                if self.state.isolated_declarations {
                    // Classes and object literals usually elide properties with computed names that are not of a literal type
                    // In isolated declarations TSC needs to error on these as we don't know the type in a DTE.
                    if !self.resolver.is_definitely_reference_to_global_symbol_object(
                        input.name().unwrap().expression().unwrap(),
                    ) {
                        if ast::is_class_declaration(input.parent().unwrap())
                            || ast::is_object_literal_expression(input.parent().unwrap())
                        {
                            self.state.add_diagnostic(create_diagnostic_for_node(input, &diagnostics::COMPUTED_PROPERTY_NAMES_ON_CLASS_OR_OBJECT_LITERALS_CANNOT_BE_INFERRED_WITH_ISOLATED_DECLARATIONS, &[]));
                            return None;
                        } else if (ast::is_interface_declaration(input.parent().unwrap())
                            || ast::is_type_literal_node(input.parent().unwrap()))
                            && !ast::is_entity_name_expression(
                                input.name().unwrap().expression().unwrap(),
                            )
                        {
                            // Type declarations just need to double-check that the input computed name is an entity name expression
                            self.state.add_diagnostic(create_diagnostic_for_node(input, &diagnostics::COMPUTED_PROPERTIES_MUST_BE_NUMBER_OR_STRING_LITERALS_VARIABLES_OR_DOTTED_EXPRESSIONS_WITH_ISOLATED_DECLARATIONS, &[]));
                            return None;
                        }
                    }
                } else if !self.resolver.is_late_bound(self.emit_context().parse_node(Some(input)))
                    || !ast::is_entity_name_expression(input.name().unwrap().expression().unwrap())
                {
                    return None;
                }
            }
        }

        // Elide implementation signatures from overload sets
        if ast::is_function_like(input) && self.resolver.is_implementation_of_overload(input) {
            return None;
        }

        if input.kind() == Kind::SemicolonClassElement {
            return None;
        }

        if ast::is_heritage_clause(input) {
            let types = input.as_heritage_clause().types().nodes();
            if types.is_empty() || (types.len() == 1 && ast::node_is_missing(types[0])) {
                return None;
            }
        }

        let previous_enclosing_declaration = self.enclosing_declaration.get();
        if is_enclosing_declaration(input) {
            self.enclosing_declaration.set(Some(input));
        }

        let (can_produce_diagnostic, mut cleanup_diagnostic_context) =
            self.setup_diagnostic_context(input);

        let result: Option<P<Node>> = match input.kind() {
            Kind::MappedType => Some(self.transform_mapped_type_node(input)),
            Kind::HeritageClause => self.transform_heritage_clause(input),
            Kind::MethodSignature => self.transform_method_signature_declaration(input),
            Kind::MethodDeclaration => self.transform_method_declaration(input),
            Kind::ConstructSignature => Some(self.transform_construct_signature_declaration(input)),
            Kind::Constructor => Some(self.transform_constructor_declaration(input)),
            Kind::GetAccessor => self.transform_get_accesor_declaration(input),
            Kind::SetAccessor => self.transform_set_accessor_declaration(input),
            Kind::PropertyDeclaration => self.transform_property_declaration(input),
            Kind::PropertySignature => self.transform_property_signature_declaration(input),
            Kind::CallSignature => Some(self.transform_call_signature_declaration(input)),
            Kind::IndexSignature => Some(self.transform_index_signature_declaration(input)),
            Kind::VariableDeclaration => self.transform_variable_declaration(input),
            Kind::TypeParameter => self.transform_type_parameter_declaration(input),
            Kind::ExpressionWithTypeArguments => {
                self.transform_expression_with_type_arguments(input)
            }
            Kind::TypeReference => self.transform_type_reference(input),
            Kind::ConditionalType => Some(self.transform_conditional_type_node(input)),
            Kind::FunctionType => Some(self.transform_function_type_node(input)),
            Kind::ConstructorType => Some(self.transform_constructor_type_node(input)),
            Kind::ImportType => Some(self.transform_import_type_node(input)),
            Kind::TypeQuery => {
                self.check_entity_name_visibility(
                    input.as_type_query_node().expr_name,
                    self.enclosing_declaration.get(),
                );
                self.visitor().visit_each_child(Some(input))
            }
            Kind::QualifiedName => {
                let right = input.as_qualified_name().right;
                if right.kind() == Kind::PrivateIdentifier {
                    self.state.add_diagnostic(create_diagnostic_for_node(input, &diagnostics::DECLARATION_EMIT_ELIDES_PRIVATE_MEMBERS_BUT_0_REFERS_TO_A_PRIVATE_MEMBER_WRITE_AN_EXPLICIT_TYPE_HERE, &[&right.text()]));
                }
                self.visitor().visit_each_child(Some(input))
            }
            Kind::TupleType => {
                let result = self.visitor().visit_each_child(Some(input));
                if let Some(result) = result
                    && transformers::is_original_node_single_line(self.emit_context(), Some(input))
                {
                    self.emit_context().add_emit_flags(result, printer::EmitFlags::SingleLine);
                }
                result
            }
            Kind::JSDocTypeExpression => self.transform_jsdoc_type_expression(input),
            Kind::JSDocTypeLiteral => Some(self.transform_jsdoc_type_literal(input)),
            Kind::JSDocPropertyTag => Some(self.transform_jsdoc_property_tag(input)),
            Kind::JSDocAllType => Some(self.transform_jsdoc_all_type(input)),
            Kind::JSDocNullableType => Some(self.transform_jsdoc_nullable_type(input)),
            Kind::JSDocNonNullableType => self.transform_jsdoc_non_nullable_type(input),
            Kind::JSDocOptionalType => Some(self.transform_jsdoc_optional_type(input)),
            Kind::JSDocVariadicType => Some(self.transform_jsdoc_variadic_type(input)),
            _ => self.visitor().visit_each_child(Some(input)),
        };

        if result.is_some() && can_produce_diagnostic && ast::has_dynamic_name(input) {
            self.check_name(input);
        }

        self.enclosing_declaration.set(previous_enclosing_declaration);
        // Go's deferred cleanupDiagnosticContext().
        cleanup_diagnostic_context();
        result
    }

    // transform.go:708
    pub(crate) fn check_name(&self, node: P<Node>) {
        let old_diag = self.state.get_symbol_accessibility_diagnostic.borrow().clone();
        if !self.suppress_new_diagnostic_contexts.get() {
            *self.state.get_symbol_accessibility_diagnostic.borrow_mut() =
                create_get_symbol_accessibility_diagnostic_for_node_name(node);
        }
        self.state.error_name_node.set(node.name());
        assert!(ast::has_dynamic_name(node)); // Should only be called with dynamic names
        let entity_name = node.name().unwrap().expression().unwrap();
        self.check_entity_name_visibility(entity_name, self.enclosing_declaration.get());
        if !self.suppress_new_diagnostic_contexts.get() {
            *self.state.get_symbol_accessibility_diagnostic.borrow_mut() = old_diag;
        }
        self.state.error_name_node.set(None);
    }

    // transform.go:723
    fn transform_mapped_type_node(&self, input: P<Node>) -> P<Node> {
        let mapped = input.as_mapped_type_node();
        // handle missing template type nodes, since the printer does not
        let type_node = match mapped.type_ {
            None => Some(self.factory().new_keyword_type_node(Kind::AnyKeyword)),
            Some(t) => self.visit_fn(Some(t)),
        };
        self.factory().update_mapped_type_node(
            input,
            mapped.readonly_token,
            // Go stores a nil child here; see ast::required_child.
            ast::required_child(self.visit_fn(Some(mapped.type_parameter)), mapped.type_parameter),
            self.visit_fn(mapped.name_type),
            mapped.question_token,
            type_node,
            None,
        )
    }

    // transform.go:742
    fn transform_heritage_clause(&self, clause: P<Node>) -> Option<P<Node>> {
        let heritage_clause = clause.as_heritage_clause();
        let types = heritage_clause.types();
        let retained_clauses: Vec<P<Node>> = types
            .nodes()
            .iter()
            .copied()
            .filter(|&t| {
                let name = ast::get_heritage_clause_element_name(t);
                ast::is_entity_name(name)
                    || ast::is_entity_name_expression(name)
                    || (heritage_clause.token == Kind::ExtendsKeyword
                        && ast::is_expression_with_type_arguments(t)
                        && t.expression().unwrap().kind() == Kind::NullKeyword)
            })
            .collect();
        if retained_clauses.is_empty() {
            return None; // elide empty clause
        }
        if retained_clauses.len() == types.nodes().len() {
            return self.visitor().visit_each_child(Some(clause));
        }
        Some(
            self.factory().update_heritage_clause(
                clause,
                heritage_clause.token,
                self.visitor()
                    .visit_nodes(Some(self.factory().new_node_list(retained_clauses)))
                    .unwrap(),
            ),
        )
    }

    // transform.go:761
    fn transform_import_type_node(&self, input: P<Node>) -> P<Node> {
        if !ast::is_literal_import_type_node(input) {
            return input;
        }
        let import_type = input.as_import_type_node();
        self.factory().update_import_type_node(
            input,
            import_type.is_type_of,
            self.factory().update_literal_type_node(
                import_type.argument,
                self.rewrite_module_specifier(
                    input,
                    Some(import_type.argument.as_literal_type_node().literal),
                )
                .unwrap(),
            ),
            import_type.attributes,
            import_type.qualifier,
            self.visitor().visit_nodes(input.type_argument_list()),
        )
    }

    // transform.go:778
    fn transform_constructor_type_node(&self, input: P<Node>) -> P<Node> {
        self.factory().update_constructor_type_node(
            input,
            self.ensure_modifiers(input),
            self.visitor().visit_nodes(input.type_parameter_list()),
            Some(self.update_param_list(input, input.parameter_list().unwrap())),
            self.visit_fn(input.type_node()),
        )
    }

    // transform.go:788
    fn transform_function_type_node(&self, input: P<Node>) -> P<Node> {
        self.factory().update_function_type_node(
            input,
            self.visitor().visit_nodes(input.type_parameter_list()),
            Some(self.update_param_list(input, input.parameter_list().unwrap())),
            self.visit_fn(input.type_node()),
        )
    }

    // transform.go:797
    fn transform_conditional_type_node(&self, input: P<Node>) -> P<Node> {
        let conditional = input.as_conditional_type_node();
        let check_type = self.visit_fn(Some(conditional.check_type));
        let extends_type = self.visit_fn(Some(conditional.extends_type));
        let old_enclosing_decl = self.enclosing_declaration.get();
        self.enclosing_declaration.set(Some(conditional.true_type));
        let true_type = self.visit_fn(Some(conditional.true_type));
        self.enclosing_declaration.set(old_enclosing_decl);
        let false_type = self.visit_fn(Some(conditional.false_type));

        // Go stores nil children here; see ast::required_child.
        self.factory().update_conditional_type_node(
            input,
            ast::required_child(check_type, conditional.check_type),
            ast::required_child(extends_type, conditional.extends_type),
            ast::required_child(true_type, conditional.true_type),
            ast::required_child(false_type, conditional.false_type),
        )
    }

    // transform.go:815
    fn transform_type_reference(&self, input: P<Node>) -> Option<P<Node>> {
        self.check_entity_name_visibility(
            input.as_type_reference_node().type_name,
            self.enclosing_declaration.get(),
        );
        self.visitor().visit_each_child(Some(input))
    }

    // transform.go:820
    fn transform_expression_with_type_arguments(&self, input: P<Node>) -> Option<P<Node>> {
        let expression = input.as_expression_with_type_arguments().expression;
        if ast::is_entity_name(expression) || ast::is_entity_name_expression(expression) {
            self.check_entity_name_visibility(expression, self.enclosing_declaration.get());
        }
        self.visitor().visit_each_child(Some(input))
    }

    // transform.go:827
    fn transform_type_parameter_declaration(&self, input: P<Node>) -> Option<P<Node>> {
        let type_parameter = input.as_type_parameter_declaration();
        if is_private_method_type_parameter(self.resolver, input)
            && (type_parameter.default_type.is_some() || type_parameter.constraint.is_some())
        {
            return Some(self.factory().update_type_parameter_declaration(
                input,
                input.modifiers(),
                type_parameter.name,
                None,
                type_parameter.expression,
                None,
            ));
        }
        self.visitor().visit_each_child(Some(input))
    }

    // transform.go:841
    fn transform_variable_declaration(&self, input: P<Node>) -> Option<P<Node>> {
        if self.state.current_source_file.get().unwrap().common_js_module_indicator().is_some()
            && ast::is_variable_declaration_initialized_to_require(input)
        {
            return self.transform_cjs_require_variable_declaration(input);
        }
        if ast::is_binding_pattern(input.name().unwrap())
            && has_any_binding_initializers(input.name().unwrap())
        {
            return self.recreate_binding_pattern(input.name().unwrap());
        }
        // Variable declaration types also suppress new diagnostic contexts, provided the contexts wouldn't be made for binding pattern types
        self.suppress_new_diagnostic_contexts.set(true);
        Some(self.factory().update_variable_declaration(
            input,
            self.binding_name_visitor().visit_node(input.name()).unwrap(),
            None,
            self.ensure_type(input, false),
            self.ensure_no_initializer(input),
        ))
    }
}

// transform.go:859
fn has_any_binding_initializers(binding_pattern: P<Node>) -> bool {
    if binding_pattern.as_binding_pattern().elements.nodes().iter().any(|&elem| {
        if !ast::is_binding_element(elem) {
            return false;
        }
        let e = elem.as_binding_element();
        if e.initializer().is_some() {
            return true;
        }
        if let Some(name) = e.name
            && ast::is_binding_pattern(name)
            && has_any_binding_initializers(name)
        {
            return true;
        }

        false
    }) {
        return true;
    }
    false
}

impl DeclarationTransformer {
    // transform.go:875
    fn transform_cjs_require_variable_declaration(&self, input: P<Node>) -> Option<P<Node>> {
        let specifier = self
            .rewrite_module_specifier(input, Some(input.initializer().unwrap().arguments()[0]))
            .unwrap();
        let name = input.name().unwrap();
        if ast::is_identifier(name) {
            // `const x = require("something")` -> `import x = require("something")`
            Some(self.factory().new_import_equals_declaration(
                None,
                false,
                name,
                self.factory().new_external_module_reference(specifier),
            ))
        } else if ast::is_array_binding_pattern(name) {
            // TODO: Is this actually reachable? should we error on this?
            None
        } else {
            // object binding pattern

            // `const {x, y: z} = require("something")` -> `import {x, y as z} from "something"`
            let mut import_specifiers: Vec<P<Node>> = Vec::new();
            import_specifiers.extend(name.as_binding_pattern().elements.nodes().iter().filter_map(
                |&elem| {
                    if !ast::is_identifier(elem.name().unwrap()) {
                        return None; // nested destructuring, bail
                    }
                    Some(self.factory().new_import_specifier(
                        false,
                        elem.property_name(),
                        elem.name().unwrap(),
                    ))
                },
            ));
            Some(
                self.factory().new_import_declaration(
                    None,
                    Some(
                        self.factory().new_import_clause(
                            Kind::Unknown,
                            None,
                            Some(self.factory().new_named_imports(
                                self.factory().new_node_list(import_specifiers),
                            )),
                        ),
                    ),
                    specifier,
                    None,
                ),
            )
        }
    }

    // transform.go:907
    fn recreate_binding_pattern(&self, input: P<Node>) -> Option<P<Node>> {
        let mut results: Vec<P<Node>> = Vec::new();
        results = input.as_binding_pattern().elements.nodes().iter().fold(
            results,
            |mut results, &elem| {
                let Some(result) = self.recreate_binding_element(elem) else {
                    return results;
                };
                if result.kind() == Kind::SyntaxList {
                    results.extend_from_slice(result.as_syntax_list().children);
                } else {
                    results.push(result);
                }

                results
            },
        );
        if results.is_empty() {
            return None;
        }
        if results.len() == 1 {
            return Some(results[0]);
        }
        Some(self.factory().new_syntax_list(alloc_vec(results)))
    }

    // transform.go:929
    fn recreate_binding_element(&self, e: P<Node>) -> Option<P<Node>> {
        let name = e.name()?;
        if !get_binding_name_visible(self.resolver, e) {
            return None;
        }
        if ast::is_binding_pattern(name) {
            return self.recreate_binding_pattern(name);
        }
        Some(self.factory().new_variable_declaration(
            name,
            None,
            self.ensure_type(e, false),
            None, // TODO: possible strada bug - not emitting const initialized binding pattern elements?
        ))
    }

    // transform.go:947
    fn transform_index_signature_declaration(&self, input: P<Node>) -> P<Node> {
        let t = match self.visit_fn(input.type_node()) {
            Some(t) => t,
            None => self.factory().new_keyword_type_node(Kind::AnyKeyword),
        };
        self.factory().update_index_signature_declaration(
            input,
            self.ensure_modifiers(input),
            Some(self.update_param_list(input, input.parameter_list().unwrap())),
            Some(t),
        )
    }

    // transform.go:960
    fn transform_call_signature_declaration(&self, input: P<Node>) -> P<Node> {
        self.factory().update_call_signature_declaration(
            input,
            self.ensure_type_params(input, input.type_parameter_list()),
            Some(self.update_param_list(input, input.parameter_list().unwrap())),
            self.ensure_type(input, false),
        )
    }

    // transform.go:969
    fn transform_property_signature_declaration(&self, input: P<Node>) -> Option<P<Node>> {
        if ast::is_private_identifier(input.name().unwrap()) {
            return None;
        }
        let result = self.factory().update_property_signature_declaration(
            input,
            self.ensure_modifiers(input),
            input.name().unwrap(),
            input.postfix_token(),
            self.ensure_type(input, false),
            self.ensure_no_initializer(input), // TODO: possible strada bug (fixed here) - const property signatures never initialized
        );
        self.preserve_partial_js_doc(result, input);
        Some(result)
    }

    // transform.go:985
    fn transform_property_declaration(&self, input: P<Node>) -> Option<P<Node>> {
        if ast::is_private_identifier(input.name().unwrap()) {
            return None;
        }
        // Remove definite assignment assertion (!) from declaration files
        let mut postfix_token = input.postfix_token();
        if postfix_token.is_some_and(|t| t.kind() == Kind::ExclamationToken) {
            postfix_token = None;
        }
        Some(self.factory().update_property_declaration(
            input,
            self.ensure_modifiers(input),
            input.name().unwrap(),
            postfix_token,
            self.ensure_type(input, false),
            self.ensure_no_initializer(input),
        ))
    }

    // transform.go:1004
    fn transform_set_accessor_declaration(&self, input: P<Node>) -> Option<P<Node>> {
        if ast::is_private_identifier(input.name().unwrap()) {
            return None;
        }

        Some(self.factory().update_set_accessor_declaration(
            input,
            self.ensure_modifiers(input),
            input.name().unwrap(),
            None, // accessors shouldn't have type params
            Some(self.update_accessor_param_list(
                input,
                self.resolver.get_effective_declaration_flags(
                    self.emit_context().parse_node(Some(input)).unwrap(),
                    ModifierFlags::Private,
                ) != ModifierFlags::None,
            )),
            None,
            None,
            None,
        ))
    }

    // transform.go:1021
    fn transform_get_accesor_declaration(&self, input: P<Node>) -> Option<P<Node>> {
        if ast::is_private_identifier(input.name().unwrap()) {
            return None;
        }
        Some(self.factory().update_get_accessor_declaration(
            input,
            self.ensure_modifiers(input),
            input.name().unwrap(),
            None, // accessors shouldn't have type params
            Some(self.update_accessor_param_list(
                input,
                self.resolver.get_effective_declaration_flags(
                    self.emit_context().parse_node(Some(input)).unwrap(),
                    ModifierFlags::Private,
                ) != ModifierFlags::None,
            )),
            self.ensure_type(input, false),
            None,
            None,
        ))
    }

    // transform.go:1037
    fn update_accessor_param_list(&self, input: P<Node>, is_private: bool) -> P<NodeList> {
        let mut new_params: Vec<P<Node>> = Vec::new();
        if !is_private && let Some(this_param) = ast::get_this_parameter(input) {
            new_params.push(self.ensure_parameter(this_param));
        }
        if ast::is_set_accessor_declaration(input) {
            let mut value_param: Option<P<Node>> = None;
            if !is_private {
                let parameters = input.parameters();
                if new_params.len() == 1 && parameters.len() >= 2 {
                    value_param = Some(self.ensure_parameter(parameters[1]));
                } else if new_params.is_empty() && !parameters.is_empty() {
                    value_param = Some(self.ensure_parameter(parameters[0]));
                }
            }
            let value_param = if let Some(value_param) = value_param {
                value_param
            } else {
                // When synthesizing a missing value parameter, emit `value: any` for non-private accessors to match TypeScript's declaration emit behavior.
                let mut t: Option<P<Node>> = None;
                if !is_private {
                    t = Some(self.factory().new_keyword_type_node(Kind::AnyKeyword));
                }
                self.factory().new_parameter_declaration(
                    None,
                    None,
                    self.factory().new_identifier("value"),
                    None,
                    t,
                    None,
                )
            };
            new_params.push(value_param);
        }
        self.factory().new_node_list(new_params)
    }

    // transform.go:1074
    fn transform_constructor_declaration(&self, input: P<Node>) -> P<Node> {
        // A constructor declaration may not have a type annotation
        self.factory().update_constructor_declaration(
            input,
            self.ensure_modifiers(input),
            None, // no type params
            Some(self.update_param_list(input, input.parameter_list().unwrap())),
            None, // no return type
            None,
            None,
        )
    }

    // transform.go:1087
    fn transform_construct_signature_declaration(&self, input: P<Node>) -> P<Node> {
        self.factory().update_construct_signature_declaration(
            input,
            self.ensure_type_params(input, input.type_parameter_list()),
            Some(self.update_param_list(input, input.parameter_list().unwrap())),
            self.ensure_type(input, false),
        )
    }

    // transform.go:1096
    fn omit_private_method_type(&self, input: P<Node>) -> Option<P<Node>> {
        if let Some(symbol) = input.symbol() {
            let first_declaration = symbol.declarations().first().copied();
            if first_declaration.is_some_and(|d| d != input) {
                return None;
            }
        }
        let result = if ast::is_method_signature_declaration(input) {
            self.factory().new_property_signature_declaration(
                self.ensure_modifiers(input),
                input.name().unwrap(),
                None, /*postfixToken*/
                None, /*typeNode*/
                None, /*initializer*/
            )
        } else {
            self.factory().new_property_declaration(
                self.ensure_modifiers(input),
                input.name().unwrap(),
                None, /*postfixToken*/
                None, /*typeNode*/
                None, /*initializer*/
            )
        };
        self.preserve_js_doc(result, input);
        Some(result)
    }

    // transform.go:1122
    fn transform_method_signature_declaration(&self, input: P<Node>) -> Option<P<Node>> {
        if self.resolver.get_effective_declaration_flags(
            self.emit_context().parse_node(Some(input)).unwrap(),
            ModifierFlags::Private,
        ) != ModifierFlags::None
        {
            self.omit_private_method_type(input)
        } else if ast::is_private_identifier(input.name().unwrap()) {
            None
        } else {
            Some(self.factory().update_method_signature_declaration(
                input,
                self.ensure_modifiers(input),
                input.name().unwrap(),
                input.postfix_token(),
                self.ensure_type_params(input, input.type_parameter_list()),
                Some(self.update_param_list(input, input.parameter_list().unwrap())),
                self.ensure_type(input, false),
            ))
        }
    }

    // transform.go:1140
    fn transform_method_declaration(&self, input: P<Node>) -> Option<P<Node>> {
        if self.resolver.get_effective_declaration_flags(
            self.emit_context().parse_node(Some(input)).unwrap(),
            ModifierFlags::Private,
        ) != ModifierFlags::None
        {
            self.omit_private_method_type(input)
        } else if ast::is_private_identifier(input.name().unwrap()) {
            None
        } else {
            Some(self.factory().update_method_declaration(
                input,
                self.ensure_modifiers(input),
                None,
                input.name().unwrap(),
                input.postfix_token(),
                self.ensure_type_params(input, input.type_parameter_list()),
                Some(self.update_param_list(input, input.parameter_list().unwrap())),
                self.ensure_type(input, false),
                None,
                None,
            ))
        }
    }

    // transform.go:1161
    fn visit_declaration_statements(&self, input: P<Node>) -> Option<P<Node>> {
        if self.should_strip_internal(Some(input)) {
            return None;
        }
        match input.kind() {
            Kind::ExportDeclaration => {
                if ast::is_source_file(input.parent().unwrap()) {
                    self.result_has_external_module_indicator.set(true);
                }
                self.result_has_scope_marker.set(true);
                let export_declaration = input.as_export_declaration();
                // Rewrite external module names if necessary
                let module_specifier =
                    self.rewrite_module_specifier(input, input.module_specifier());
                Some(self.factory().update_export_declaration(
                    input,
                    input.modifiers(),
                    input.is_type_only(),
                    export_declaration.export_clause,
                    module_specifier,
                    export_declaration.attributes,
                ))
            }
            Kind::ExportAssignment => Some(self.transform_export_assignment(
                input,
                input,
                input.expression().unwrap(),
                input.as_export_assignment().is_export_equals,
            )),
            _ => {
                let id = ast::get_node_id(self.emit_context().most_original(Some(input)).unwrap());
                let is_nil = self
                    .late_statement_replacement_map
                    .borrow()
                    .get(&id)
                    .copied()
                    .flatten()
                    .is_none();
                if is_nil {
                    // Don't actually transform yet; just leave as original node - will be elided/swapped by late pass
                    let result = self.transform_top_level_declaration(input);
                    self.late_statement_replacement_map.borrow_mut().insert(id, result);
                }
                Some(input)
            }
        }
    }

    // transform.go:1192
    pub(crate) fn try_get_name_of_assigned_expression(
        &self,
        unwrapped: P<Node>,
    ) -> Option<P<Node>> {
        let mut name_node: Option<P<Node>> = None;
        let mut name_text: &'static str = "";
        if !ast::is_property_access_expression(unwrapped) && unwrapped.name().is_some() {
            name_text = unwrapped.name().unwrap().text();
        } else if ast::is_identifier(unwrapped) {
            name_text = unwrapped.text();
        }
        if !name_text.is_empty() && name_text != "default" {
            if self.resolver.is_name_resolvable(self.enclosing_declaration.get(), name_text) {
                // create a unique name that shares the same text as its' base
                name_node = Some(self.factory().new_unique_name_ex(
                    name_text,
                    printer::AutoGenerateOptions {
                        flags: printer::GeneratedIdentifierFlags::Optimistic,
                        ..Default::default()
                    },
                ));
            } else {
                // use the node's name as-is, since it's not otherwise in-scope
                name_node = Some(self.factory().new_identifier(name_text));
            }
        }
        name_node
    }

    // transform.go:1212
    fn get_name_of_exported_assigned_expression(
        &self,
        unwrapped: P<Node>,
        is_export_equals: bool,
    ) -> P<Node> {
        let name_node = match self.try_get_name_of_assigned_expression(unwrapped) {
            Some(name_node) => name_node,
            None => {
                // fallback to a default name
                if is_export_equals
                    && ast::is_source_file_js(self.state.current_source_file.get().unwrap())
                {
                    // only JS files prefer to use `_exports` for export assignments - TS has always used `_default` for both `export=` and `export default`
                    self.factory().new_unique_name_ex(
                        "_exports",
                        printer::AutoGenerateOptions {
                            flags: printer::GeneratedIdentifierFlags::Optimistic,
                            ..Default::default()
                        },
                    )
                } else {
                    self.factory().new_unique_name_ex(
                        "_default",
                        printer::AutoGenerateOptions {
                            flags: printer::GeneratedIdentifierFlags::Optimistic,
                            ..Default::default()
                        },
                    )
                }
            }
        };
        self.cjs_export_assignment_name.set(Some(name_node));
        name_node
    }

    // transform.go:1227
    pub(crate) fn transform_export_assignment(
        &self,
        input: P<Node>,
        assignment: P<Node>,
        expression: P<Node>,
        is_export_equals: bool,
    ) -> P<Node> {
        if ast::is_source_file(input.parent().unwrap()) {
            self.result_has_external_module_indicator.set(true);
        }
        self.result_has_scope_marker.set(true);
        if ast::is_identifier(expression)
            && (ast::is_source_file(input.parent().unwrap())
                || ast::is_module_block(input.parent().unwrap()))
        {
            let export_assignment =
                self.factory().new_export_assignment(None, is_export_equals, None, expression);
            self.emit_context().assign_source_map_range(export_assignment, input);
            self.preserve_js_doc(export_assignment, input);
            return export_assignment;
        }

        *self.state.get_symbol_accessibility_diagnostic.borrow_mut() =
            Rc::new(move |_: &SymbolAccessibilityResult| {
                Some(P::new(SymbolAccessibilityDiagnostic {
                    diagnostic_message:
                        &diagnostics::DEFAULT_EXPORT_OF_THE_MODULE_HAS_OR_IS_USING_PRIVATE_NAME_0,
                    error_node: Some(input),
                    type_name: None,
                }))
            });
        self.tracker.push_error_fallback_node(Some(assignment));

        // Check if the expression is a class expression - emit as a class declaration + export assignment
        let unwrapped = ast::skip_outer_expressions(
            expression,
            OuterExpressionKinds::ExpressionTypePassthrough,
        );
        let new_id = self.get_name_of_exported_assigned_expression(unwrapped, is_export_equals);
        if ast::is_class_expression(unwrapped) {
            let mut mods: Vec<P<Node>> = Vec::new();
            if self.needs_declare.get() {
                mods.push(self.factory().new_modifier(Kind::DeclareKeyword));
            }
            let class_decl = self.transform_class_expression_to_declaration(
                unwrapped,
                new_id,
                self.factory().new_modifier_list(mods),
            );
            self.tracker.pop_error_fallback_node();
            self.preserve_js_doc(class_decl, input);
            // Reuse the same name node for the export so unique names resolve consistently
            let export_assignment =
                self.factory().new_export_assignment(None, is_export_equals, None, new_id);
            self.emit_context().assign_source_map_range(export_assignment, input);
            self.remove_all_comments(export_assignment);
            return self.factory().new_syntax_list(alloc_vec(vec![export_assignment, class_decl]));
        } else if ast::is_function_like(unwrapped) {
            // Promote function or arrow function expressions to a function declaration
            let mut mods: Vec<P<Node>> = Vec::new();
            if self.needs_declare.get() {
                mods.push(self.factory().new_modifier(Kind::DeclareKeyword));
            }
            let full_signature_type = assignment.type_node();
            let func_decl = self.transform_function_like_to_declaration(
                unwrapped,
                new_id,
                self.factory().new_modifier_list(mods),
                full_signature_type,
            );
            self.tracker.pop_error_fallback_node();
            self.preserve_js_doc(func_decl, input);
            // Reuse the same name node for the export so unique names resolve consistently
            let export_assignment =
                self.factory().new_export_assignment(None, is_export_equals, None, new_id);
            self.emit_context().assign_source_map_range(export_assignment, input);
            self.remove_all_comments(export_assignment);
            return self.factory().new_syntax_list(alloc_vec(vec![export_assignment, func_decl]));
        }

        // expression is non-identifier, create _default typed variable to reference
        self.cjs_export_assignment_name.set(Some(new_id));
        let mut type_: Option<P<Node>> = None;
        let mut initializer: Option<P<Node>> = None;
        if ast::is_primitive_literal_value(unwrap_parenthesized_expression(expression), true) {
            initializer = self.resolver.create_literal_const_value(
                self.emit_context().parse_node(Some(assignment)).unwrap(),
                self.tracker.get(),
            );
        }
        if initializer.is_none() {
            type_ = self.ensure_type(assignment, false);
        }
        let var_decl = self.factory().new_variable_declaration(new_id, None, type_, initializer);
        self.tracker.pop_error_fallback_node();
        let mod_list = if self.needs_declare.get() {
            self.factory()
                .new_modifier_list(vec![self.factory().new_modifier(Kind::DeclareKeyword)])
        } else {
            self.factory().new_modifier_list(Vec::new())
        };
        let statement = self.factory().new_variable_statement(
            Some(mod_list),
            self.factory().new_variable_declaration_list(
                self.factory().new_node_list(vec![var_decl]),
                NodeFlags::Const,
            ),
        );
        let export_assignment =
            self.factory().new_export_assignment(None, is_export_equals, None, new_id);
        self.emit_context().assign_source_map_range(export_assignment, input);
        // Remove comments from the export declaration and copy them onto the synthetic _default declaration
        self.preserve_js_doc(statement, input);
        self.factory().new_syntax_list(alloc_vec(vec![statement, export_assignment]))
    }

    // transform.go:1305
    fn transform_function_like_to_declaration(
        &self,
        unwrapped: P<Node>,
        func_name: P<Node>,
        mods: P<ModifierList>,
        full_signature_type: Option<P<Node>>,
    ) -> P<Node> {
        let d = unwrapped.function_like_data().unwrap();
        let mut sig = d.full_signature();
        if sig.is_none() {
            sig = full_signature_type;
        }
        if sig.is_none() {
            self.factory().new_function_declaration(
                Some(mods),
                None,
                Some(func_name),
                self.ensure_type_params(unwrapped, d.type_parameters()),
                Some(self.update_param_list(unwrapped, d.parameters().unwrap())),
                self.ensure_type(unwrapped, false),
                self.visitor().visit_node(sig),
                None,
            )
        } else {
            // If a full signature type node is present, emit as a variable statement to reuse it
            self.factory().new_variable_statement(
                Some(mods),
                self.factory().new_variable_declaration_list(
                    self.factory().new_node_list(vec![self.factory().new_variable_declaration(
                        func_name,
                        None,
                        self.visitor().visit_node(sig),
                        None,
                    )]),
                    NodeFlags::Const,
                ),
            )
        }
    }

    // transform.go:1331
    pub(crate) fn transform_binary_expression_to_export_declaration(
        &self,
        input: P<Node>,
        name: P<Node>,
    ) -> P<Node> {
        let mut property_name = Some(input.as_binary_expression().right());

        // track alias target so referenced declarations are included in the output
        self.tracker.handle_symbol_accessibility_error(
            &self
                .resolver
                .is_entity_name_visible(property_name.unwrap(), self.enclosing_declaration.get()),
        );

        if ast::is_identifier(name) && property_name.unwrap().text() == name.text() {
            property_name = None;
        }

        self.factory().new_export_declaration(
            None,
            false,
            Some(self.factory().new_named_exports(self.factory().new_node_list(vec![
                self.factory().new_export_specifier(false, property_name, name),
            ]))),
            None,
            None,
        )
    }
}
