use super::*;

// commonjsmodule.go is split by line range: commonjsmodule.rs (1–559), commonjsmodule_2.rs (560–985),
// commonjsmodule_3.rs (986–1339), commonjsmodule_4.rs (1340–end).

// commonjsmodule.go:15
#[expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]
pub(crate) struct CommonJSModuleTransformer {
    pub(crate) base: Transformer,
    top_level_visitor: OnceCell<NodeVisitor>, // visits statements at top level of a module
    top_level_nested_visitor: OnceCell<NodeVisitor>, // visits nested statements at top level of a module
    discarded_value_visitor: OnceCell<NodeVisitor>, // visits expressions whose values would be discarded at runtime
    assignment_pattern_visitor: OnceCell<NodeVisitor>, // visits assignment patterns in a destructuring assignment
    pub(crate) compiler_options: P<CompilerOptions>,
    pub(crate) resolver: ReferenceResolverRef,
    pub(crate) get_emit_module_format_of_file: Rc<dyn Fn(P<SourceFile>) -> ModuleKind>,
    pub(crate) module_kind: ModuleKind,
    pub(crate) language_version: ScriptTarget,
    pub(crate) current_source_file: Cell<Option<P<SourceFile>>>,
    current_module_info: RefCell<Option<Rc<ExternalModuleInfo>>>,
    pub(crate) parent_node: Cell<Option<P<Node>>>, // used for ancestor tracking via pushNode/popNode to detect expression identifiers
    current_node: Cell<Option<P<Node>>>, // used for ancestor tracking via pushNode/popNode to detect expression identifiers
    pub(crate) content_mapper_extension_rewrites: Vec<tspath::ExtensionRewrite>,
    pub(crate) use_case_sensitive_file_names: bool,
}

// commonjsmodule.go:32
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_common_js_module_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
    let compiler_options = opts.compiler_options;
    let emit_context = opts.context;
    let tx = P::new(CommonJSModuleTransformer {
        base: Transformer::default(),
        top_level_visitor: OnceCell::new(),
        top_level_nested_visitor: OnceCell::new(),
        discarded_value_visitor: OnceCell::new(),
        assignment_pattern_visitor: OnceCell::new(),
        compiler_options,
        resolver: opts.resolver,
        get_emit_module_format_of_file: Rc::clone(&opts.get_emit_module_format_of_file),
        module_kind: compiler_options.get_emit_module_kind(),
        language_version: compiler_options.get_emit_script_target(),
        current_source_file: Cell::new(None),
        current_module_info: RefCell::new(None),
        parent_node: Cell::new(None),
        current_node: Cell::new(None),
        content_mapper_extension_rewrites: opts.content_mapper_extension_rewrites.clone(),
        use_case_sensitive_file_names: opts.use_case_sensitive_file_names,
    });
    let _ =
        tx.top_level_visitor.set(emit_context.new_node_visitor(Rc::new(
            move |_: &mut NodeVisitor, n: P<Node>| tx.visit_top_level(n),
        )));
    let _ = tx.top_level_nested_visitor.set(emit_context.new_node_visitor(Rc::new(
        move |_: &mut NodeVisitor, n: P<Node>| tx.visit_top_level_nested(n),
    )));
    let _ = tx.discarded_value_visitor.set(emit_context.new_node_visitor(Rc::new(
        move |_: &mut NodeVisitor, n: P<Node>| tx.visit_discarded_value(n),
    )));
    let _ = tx.assignment_pattern_visitor.set(emit_context.new_node_visitor(Rc::new(
        move |_: &mut NodeVisitor, n: P<Node>| tx.visit_assignment_pattern(n),
    )));
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(emit_context),
    ))
}

impl CommonJSModuleTransformer {
    pub(crate) fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    pub(crate) fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    pub(crate) fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    fn top_level_visitor(&self) -> NodeVisitor {
        self.top_level_visitor.get().unwrap().clone()
    }

    pub(crate) fn top_level_nested_visitor(&self) -> NodeVisitor {
        self.top_level_nested_visitor.get().unwrap().clone()
    }

    pub(crate) fn discarded_value_visitor(&self) -> NodeVisitor {
        self.discarded_value_visitor.get().unwrap().clone()
    }

    pub(crate) fn assignment_pattern_visitor(&self) -> NodeVisitor {
        self.assignment_pattern_visitor.get().unwrap().clone()
    }

    pub(crate) fn module_info(&self) -> Rc<ExternalModuleInfo> {
        self.current_module_info.borrow().clone().unwrap()
    }

    // Pushes a new child node onto the ancestor tracking stack, returning the grandparent node to be restored later via `popNode`.
    // commonjsmodule.go:46
    pub(crate) fn push_node(&self, node: P<Node>) -> Option<P<Node>> {
        let grandparent_node = self.parent_node.get();
        self.parent_node.set(self.current_node.get());
        self.current_node.set(Some(node));
        grandparent_node
    }

    // Pops the last child node off the ancestor tracking stack, restoring the grandparent node.
    // commonjsmodule.go:54
    pub(crate) fn pop_node(&self, grandparent_node: Option<P<Node>>) {
        self.current_node.set(self.parent_node.get());
        self.parent_node.set(grandparent_node);
    }

    // Visits a node at the top level of the source file.
    // commonjsmodule.go:60
    fn visit_top_level(&self, node: P<Node>) -> Option<P<Node>> {
        let grandparent_node = self.push_node(node);
        let result = match node.kind() {
            Kind::ImportDeclaration => self.visit_top_level_import_declaration(node),
            Kind::ImportEqualsDeclaration => self.visit_top_level_import_equals_declaration(node),
            Kind::ExportDeclaration => self.visit_top_level_export_declaration(node),
            Kind::ExportAssignment => self.visit_top_level_export_assignment(node),
            Kind::FunctionDeclaration => Some(self.visit_top_level_function_declaration(node)),
            Kind::ClassDeclaration => self.visit_top_level_class_declaration(node),
            Kind::VariableStatement => self.visit_top_level_variable_statement(node),
            _ => self.visit_top_level_nested_no_stack(node),
        };
        self.pop_node(grandparent_node);
        result
    }

    // Visits nested elements at the top-level of a module.
    // commonjsmodule.go:85
    fn visit_top_level_nested(&self, node: P<Node>) -> Option<P<Node>> {
        let grandparent_node = self.push_node(node);
        let result = self.visit_top_level_nested_no_stack(node);
        self.pop_node(grandparent_node);
        result
    }

    // Visits nested elements at the top-level of a module without ancestor tracking.
    // commonjsmodule.go:93
    fn visit_top_level_nested_no_stack(&self, node: P<Node>) -> Option<P<Node>> {
        match node.kind() {
            Kind::VariableStatement => self.visit_top_level_variable_statement(node),
            Kind::ForStatement => self.visit_top_level_nested_for_statement(node),
            Kind::ForInStatement | Kind::ForOfStatement => {
                Some(self.visit_top_level_nested_for_in_or_of_statement(node))
            }
            Kind::DoStatement => Some(self.visit_top_level_nested_do_statement(node)),
            Kind::WhileStatement => Some(self.visit_top_level_nested_while_statement(node)),
            Kind::LabeledStatement => Some(self.visit_top_level_nested_labeled_statement(node)),
            Kind::WithStatement => Some(self.visit_top_level_nested_with_statement(node)),
            Kind::IfStatement => Some(self.visit_top_level_nested_if_statement(node)),
            Kind::SwitchStatement => Some(self.visit_top_level_nested_switch_statement(node)),
            Kind::CaseBlock => self.visit_top_level_nested_case_block(node),
            Kind::CaseClause | Kind::DefaultClause => {
                Some(self.visit_top_level_nested_case_or_default_clause(node))
            }
            Kind::TryStatement => self.visit_top_level_nested_try_statement(node),
            Kind::CatchClause => Some(self.visit_top_level_nested_catch_clause(node)),
            Kind::Block => self.visit_top_level_nested_block(node),
            _ => self.visit_no_stack(node, false /*resultIsDiscarded*/),
        }
    }

    // Visits source elements that are not top-level or top-level nested statements.
    // commonjsmodule.go:131
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        let grandparent_node = self.push_node(node);
        let result = self.visit_no_stack(node, false /*resultIsDiscarded*/);
        self.pop_node(grandparent_node);
        result
    }

    // Visits source elements that are not top-level or top-level nested statements without ancestor tracking.
    // commonjsmodule.go:139
    pub(crate) fn visit_no_stack(
        &self,
        node: P<Node>,
        result_is_discarded: bool,
    ) -> Option<P<Node>> {
        // This visitor does not need to descend into the tree if there are no dynamic imports or identifiers in the subtree
        if !ast::is_source_file(node)
            && !node
                .subtree_facts()
                .intersects(SubtreeFacts::ContainsDynamicImport | SubtreeFacts::ContainsIdentifier)
        {
            return Some(node);
        }

        match node.kind() {
            Kind::SourceFile => Some(self.visit_source_file(node.as_source_file_p())),
            Kind::ForStatement => Some(self.visit_for_statement(node)),
            Kind::ForInStatement | Kind::ForOfStatement => {
                Some(self.visit_for_in_or_of_statement(node))
            }
            Kind::ExpressionStatement => self.visit_expression_statement(node),
            Kind::VoidExpression => self.visit_void_expression(node),
            Kind::ParenthesizedExpression => {
                Some(self.visit_parenthesized_expression(node, result_is_discarded))
            }
            Kind::PartiallyEmittedExpression => {
                Some(self.visit_partially_emitted_expression(node, result_is_discarded))
            }
            Kind::CallExpression => Some(self.visit_call_expression(node)),
            Kind::TaggedTemplateExpression => Some(self.visit_tagged_template_expression(node)),
            Kind::BinaryExpression => Some(self.visit_binary_expression(node, result_is_discarded)),
            Kind::PrefixUnaryExpression => {
                Some(self.visit_prefix_unary_expression(node, result_is_discarded))
            }
            Kind::PostfixUnaryExpression => {
                Some(self.visit_postfix_unary_expression(node, result_is_discarded))
            }
            Kind::ShorthandPropertyAssignment => {
                Some(self.visit_shorthand_property_assignment(node))
            }
            Kind::Identifier => Some(self.visit_identifier(node)),
            _ => self.visitor().visit_each_child(Some(node)),
        }
    }

    // Visits source elements whose value is discarded if they are expressions.
    // commonjsmodule.go:186
    fn visit_discarded_value(&self, node: P<Node>) -> Option<P<Node>> {
        let grandparent_node = self.push_node(node);
        let result = self.visit_no_stack(node, true /*resultIsDiscarded*/);
        self.pop_node(grandparent_node);
        result
    }

    // commonjsmodule.go:193
    fn visit_assignment_pattern(&self, node: P<Node>) -> Option<P<Node>> {
        let grandparent_node = self.push_node(node);
        let result = self.visit_assignment_pattern_no_stack(node);
        self.pop_node(grandparent_node);
        result
    }

    // commonjsmodule.go:200
    pub(crate) fn visit_assignment_pattern_no_stack(&self, node: P<Node>) -> Option<P<Node>> {
        match node.kind() {
            // AssignmentPattern
            Kind::ObjectLiteralExpression | Kind::ArrayLiteralExpression => {
                self.assignment_pattern_visitor().visit_each_child(Some(node))
            }

            // AssignmentProperty
            Kind::PropertyAssignment => Some(self.visit_assignment_property(node)),
            Kind::ShorthandPropertyAssignment => {
                Some(self.visit_shorthand_assignment_property(node))
            }

            // AssignmentRestProperty
            Kind::SpreadAssignment => Some(self.visit_assignment_rest_property(node)),

            // AssignmentRestElement
            Kind::SpreadElement => Some(self.visit_assignment_rest_element(node)),

            // AssignmentElement
            _ => {
                if ast::is_expression(node) {
                    return self.visit_assignment_element(node);
                }

                self.visit_no_stack(node, false /*resultIsDiscarded*/)
            }
        }
    }

    // commonjsmodule.go:231
    fn visit_source_file(&self, node: P<SourceFile>) -> P<Node> {
        if node.is_declaration_file.get()
            || !(ast::is_effective_external_module(node, &self.compiler_options)
                || node.as_node().subtree_facts().intersects(SubtreeFacts::ContainsDynamicImport))
        {
            return node.as_node();
        }

        self.current_source_file.set(Some(node));
        *self.current_module_info.borrow_mut() = Some(Rc::new(collect_external_module_info(
            node,
            self.compiler_options,
            self.emit_context(),
            self.resolver,
        )));
        let updated = self.transform_common_js_module(node);
        self.current_source_file.set(None);
        *self.current_module_info.borrow_mut() = None;
        updated
    }

    // commonjsmodule.go:245
    fn should_emit_underscore_underscore_es_module(&self) -> bool {
        let current_source_file = self.current_source_file.get().unwrap();
        if tspath::file_extension_is_one_of(
            current_source_file.file_name(),
            tspath::SUPPORTED_JS_EXTENSIONS_FLAT,
        ) && current_source_file.common_js_module_indicator.get().is_some()
            && current_source_file
                .external_module_indicator
                .get()
                .is_none_or(|i| i.kind() == Kind::SourceFile)
        {
            return false;
        }
        if self.module_info().export_equals.is_none()
            && ast::is_external_module(current_source_file)
        {
            return true;
        }
        false
    }

    // commonjsmodule.go:258
    fn create_underscore_underscore_es_module(&self) -> P<Node> {
        let f = self.factory();
        let statement = f.new_expression_statement(f.new_call_expression(
            f.new_property_access_expression(
                f.new_identifier("Object"),
                None, /*questionDotToken*/
                f.new_identifier("defineProperty"),
                NodeFlags::None,
            ),
            None, /*questionDotToken*/
            None, /*typeArguments*/
            f.new_node_list(vec![
                f.new_identifier("exports"),
                f.new_string_literal("__esModule", TokenFlags::None),
                f.new_object_literal_expression(
                    f.new_node_list(vec![f.new_property_assignment(
                        None, /*modifiers*/
                        f.new_identifier("value"),
                        None, /*postfixToken*/
                        None, /*typeNode*/
                        f.new_true_expression(),
                    )]),
                    false, /*multiLine*/
                ),
            ]),
            NodeFlags::None,
        ));
        self.emit_context().set_emit_flags(statement, EmitFlags::CustomPrologue);
        statement
    }

    // commonjsmodule.go:290
    fn transform_common_js_module(&self, node: P<SourceFile>) -> P<Node> {
        let f = self.factory();
        let emit_context = self.emit_context();
        emit_context.start_variable_environment();

        // emit standard prologue directives (e.g. "use strict")
        let (prologue, rest) = f.split_standard_prologue(node.statements.nodes());
        let mut statements: Vec<P<Node>> = prologue.to_vec();

        // emit custom prologues from other transformations
        let (custom, rest) = f.split_custom_prologue(rest);
        statements.extend_from_slice(self.top_level_visitor().visit_slice(custom).0);

        // emits `Object.defineProperty(exports, "__esModule", { value: true });` at the top of the file
        if self.should_emit_underscore_underscore_es_module() {
            statements.push(self.create_underscore_underscore_es_module());
        }

        let info = self.module_info();

        // initialize all exports to `undefined`, e.g.:
        //  exports.a = exports.b = void 0;
        if !info.exported_names.is_empty() {
            const CHUNK_SIZE: usize = 50;
            statements.extend(info.exported_names.chunks(CHUNK_SIZE).map(|chunk| {
                let mut right = f.new_void_zero_expression();
                for &next_id in chunk {
                    let left = if next_id.kind() == Kind::StringLiteral {
                        f.new_element_access_expression(
                            f.new_identifier("exports"),
                            None, /*questionDotToken*/
                            f.new_string_literal_from_node(next_id),
                            NodeFlags::None,
                        )
                    } else {
                        let name = next_id.clone_node(f);
                        emit_context
                            .set_emit_flags(name, EmitFlags::NoSourceMap | EmitFlags::NoComments);
                        f.new_property_access_expression(
                            f.new_identifier("exports"),
                            None, /*questionDotToken*/
                            name,
                            NodeFlags::None,
                        )
                    };
                    right = f.new_assignment_expression(left, right);
                }
                let statement = f.new_expression_statement(right);
                emit_context.add_emit_flags(statement, EmitFlags::CustomPrologue);
                statement
            }));
        }

        // initialize exports for function declarations, e.g.:
        //  exports.f = f;
        //  function f() {}
        // These are marked as custom prologue so they are ordered before the external helpers
        // import declaration (e.g., `const tslib_1 = require("tslib")`), matching TypeScript's emit order.
        let exported_functions_start = statements.len();
        for &func in &info.exported_functions {
            self.append_exports_of_class_or_function_declaration(&mut statements, func);
        }
        for &s in &statements[exported_functions_start..] {
            emit_context.add_emit_flags(s, EmitFlags::CustomPrologue);
        }

        // visit the remaining statements in the source file
        let (rest, _) = self.top_level_visitor().visit_slice(rest);
        statements.extend_from_slice(rest);

        // emit `module.exports = ...` if needd
        self.append_export_equals_if_needed(&mut statements);

        // merge temp variables into the statement list
        let statements = emit_context.end_and_merge_variable_environment(&statements);

        let statement_list = f.new_node_list(statements);
        statement_list.loc.set(node.statements.loc.get());
        let mut result = f
            .update_source_file(node.as_node(), statement_list, node.end_of_file_token)
            .as_source_file_p();
        emit_context.add_emit_helper(result.as_node(), &emit_context.read_emit_helpers());

        let external_helpers_import_declaration =
            create_external_helpers_import_declaration_if_needed(
                emit_context,
                result,
                &self.compiler_options,
                (self.get_emit_module_format_of_file)(node),
                false, /*hasExportStarsToExportValues*/
                false, /*hasImportStar*/
                false, /*hasImportDefault*/
            );
        if let Some(external_helpers_import_declaration) = external_helpers_import_declaration {
            let (prologue, rest) = f.split_standard_prologue(result.statements.nodes());
            let (custom, rest) = f.split_custom_prologue(rest);
            let mut statements: Vec<P<Node>> = prologue.to_vec();
            statements.extend_from_slice(custom);
            statements.extend(
                self.top_level_visitor().visit_node(Some(external_helpers_import_declaration)),
            );
            statements.extend_from_slice(rest);
            let statement_list = f.new_node_list(statements);
            statement_list.loc.set(result.statements.loc.get());
            result = f
                .update_source_file(result.as_node(), statement_list, node.end_of_file_token)
                .as_source_file_p();
        }

        result.as_node()
    }

    // Adds the down-level representation of `export=` to the statement list if one exists in the source file.
    //
    // - The `statements` parameter is a statement list to which the down-level export statements are to be appended.
    // commonjsmodule.go:386
    fn append_export_equals_if_needed(&self, statements: &mut Vec<P<Node>>) {
        if let Some(export_equals) = self.module_info().export_equals {
            let expression_result = self.visit_export_equals(export_equals);
            if let Some(expression_result) = expression_result {
                let f = self.factory();
                let statement = f.new_expression_statement(f.new_assignment_expression(
                    f.new_property_access_expression(
                        f.new_identifier("module"),
                        None, /*questionDotToken*/
                        f.new_identifier("exports"),
                        NodeFlags::None,
                    ),
                    expression_result,
                ));

                self.emit_context().assign_comment_and_source_map_ranges(statement, export_equals);
                self.emit_context().add_emit_flags(statement, EmitFlags::NoComments);
                statements.push(statement);
            }
        }
    }

    // commonjsmodule.go:410
    fn visit_export_equals(&self, node: P<Node>) -> Option<P<Node>> {
        let grandparent_node = self.push_node(node);
        let result = self.visitor().visit_node(node.expression());
        self.pop_node(grandparent_node);
        result
    }

    // Appends the exports of an ImportDeclaration to a statement list, returning the statement list.
    // commonjsmodule.go:420
    pub(crate) fn append_exports_of_import_declaration(
        &self,
        statements: &mut Vec<P<Node>>,
        decl: P<Node>,
    ) {
        if self.module_info().export_equals.is_some() {
            return;
        }

        let Some(import_clause) = decl.as_import_declaration().import_clause else {
            return;
        };

        let mut seen: Set<&'static str> = Set::default();
        if import_clause.name().is_some() {
            self.append_exports_of_declaration(
                statements,
                import_clause,
                Some(&mut seen),
                false, /*liveBinding*/
            );
        }

        if let Some(named_bindings) = import_clause.as_import_clause().named_bindings {
            match named_bindings.kind() {
                Kind::NamespaceImport => {
                    self.append_exports_of_declaration(
                        statements,
                        named_bindings,
                        Some(&mut seen),
                        false, /*liveBinding*/
                    );
                }
                Kind::NamedImports => {
                    for &import_binding in named_bindings.elements() {
                        self.append_exports_of_declaration(
                            statements,
                            import_binding,
                            Some(&mut seen),
                            true, /*liveBinding*/
                        );
                    }
                }
                _ => {}
            }
        }
    }

    // Appends the exports of a VariableStatement to a statement list, returning the statement list.
    // commonjsmodule.go:455
    pub(crate) fn append_exports_of_variable_statement(
        &self,
        statements: &mut Vec<P<Node>>,
        node: P<Node>,
    ) {
        self.append_exports_of_variable_declaration_list(
            statements,
            node.as_variable_statement().declaration_list,
            false, /*isForInOrOfInitializer*/
        );
    }

    // Appends the exports of a VariableDeclarationList to a statement list, returning the statement list.
    // commonjsmodule.go:463
    pub(crate) fn append_exports_of_variable_declaration_list(
        &self,
        statements: &mut Vec<P<Node>>,
        node: P<Node>,
        is_for_in_or_of_initializer: bool,
    ) {
        if self.module_info().export_equals.is_some() {
            return;
        }

        for &decl in node.as_variable_declaration_list().declarations.nodes() {
            self.append_exports_of_binding_element(statements, decl, is_for_in_or_of_initializer);
        }
    }

    // Appends the exports of a VariableDeclaration or BindingElement to a statement list, returning the statement list.
    // commonjsmodule.go:479
    fn append_exports_of_binding_element(
        &self,
        statements: &mut Vec<P<Node>>,
        decl: P<Node>, /*VariableDeclaration | BindingElement*/
        is_for_in_or_of_initializer: bool,
    ) {
        let Some(name) = decl.name() else {
            return;
        };
        if self.module_info().export_equals.is_some() {
            return;
        }

        if ast::is_binding_pattern(name) {
            for &element in name.elements() {
                if !ast::is_omitted_expression(element) {
                    self.append_exports_of_binding_element(
                        statements,
                        element,
                        is_for_in_or_of_initializer,
                    );
                }
            }
        } else if !is_generated_identifier(self.emit_context(), name)
            && (!ast::is_variable_declaration(decl)
                || decl.initializer().is_some()
                || is_for_in_or_of_initializer)
        {
            self.append_exports_of_declaration(
                statements, decl, None,  /*seen*/
                false, /*liveBinding*/
            );
        }
    }

    // Appends the exports of a ClassDeclaration or FunctionDeclaration to a statement list, returning the statement list.
    // commonjsmodule.go:502
    pub(crate) fn append_exports_of_class_or_function_declaration(
        &self,
        statements: &mut Vec<P<Node>>,
        decl: P<Node>,
    ) {
        if self.module_info().export_equals.is_some() {
            return;
        }

        let f = self.factory();
        let mut seen: Set<&'static str> = Set::default();
        if ast::has_syntactic_modifier(decl, ModifierFlags::Export) {
            let export_name = if ast::has_syntactic_modifier(decl, ModifierFlags::Default) {
                f.new_identifier("default")
            } else {
                f.get_declaration_name(decl)
            };

            let export_value = f.get_local_name(decl);
            self.append_export_statement(
                statements,
                &mut seen,
                export_name,
                export_value,
                Some(decl.loc()),
                false, /*allowComments*/
                false, /*liveBinding*/
            );
        }

        if decl.name().is_some() {
            self.append_exports_of_declaration(
                statements,
                decl,
                Some(&mut seen),
                false, /*liveBinding*/
            );
        }
    }

    // Appends the exports of a declaration to a statement list, returning the statement list.
    // commonjsmodule.go:531
    pub(crate) fn append_exports_of_declaration(
        &self,
        statements: &mut Vec<P<Node>>,
        decl: P<Node>,
        seen: Option<&mut Set<&'static str>>,
        live_binding: bool,
    ) {
        let info = self.module_info();
        if info.export_equals.is_some() {
            return;
        }

        let mut own_seen: Set<&'static str> = Set::default();
        let seen = match seen {
            Some(seen) => seen,
            None => &mut own_seen,
        };

        if let Some(name) = decl.name()
            && !info.export_specifiers.m.is_empty()
            && ast::is_identifier(name)
        {
            let name = self.factory().get_declaration_name(decl);
            let export_specifiers = info.export_specifiers.get(&name.text());
            if !export_specifiers.is_empty() {
                let export_value = self.visit_expression_identifier(name);
                for &export_specifier in export_specifiers {
                    let export_name = export_specifier.name().unwrap();
                    self.append_export_statement(
                        statements,
                        seen,
                        export_name,
                        export_value,
                        Some(export_name.loc()), /*location*/
                        false,                   /*allowComments*/
                        live_binding,
                    );
                }
            }
        }
    }
}
