use super::*;

struct ESModuleTransformer {
    base: Transformer,
    compiler_options: P<CompilerOptions>,
    get_emit_module_format_of_file: Rc<dyn Fn(P<SourceFile>) -> ModuleKind>,
    current_source_file: Cell<Option<P<SourceFile>>>,
    import_require_statements: RefCell<Option<ImportRequireStatements>>,
    content_mapper_extension_rewrites: Vec<tspath::ExtensionRewrite>,
    use_case_sensitive_file_names: bool,
}

#[derive(Clone)]
struct ImportRequireStatements {
    statements: Vec<P<Node>>,
    require_helper_name: P<Node>,
}

// esmodule.go:28
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_es_module_transformer(opts: &TransformOptions) -> Option<P<Transformer>> {
    let compiler_options = opts.compiler_options;
    let tx = P::new(ESModuleTransformer {
        base: Transformer::default(),
        compiler_options,
        get_emit_module_format_of_file: Rc::clone(&opts.get_emit_module_format_of_file),
        current_source_file: Cell::new(None),
        import_require_statements: RefCell::new(None),
        content_mapper_extension_rewrites: opts.content_mapper_extension_rewrites.clone(),
        use_case_sensitive_file_names: opts.use_case_sensitive_file_names,
    });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(opts.context),
    ))
}

impl ESModuleTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    // Visits source elements that are not top-level or top-level nested statements.
    // esmodule.go:35
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        match node.kind() {
            Kind::SourceFile => Some(self.visit_source_file(node.as_source_file_p())),
            Kind::ImportDeclaration => Some(self.visit_import_declaration(node)),
            Kind::ImportEqualsDeclaration => self.visit_import_equals_declaration(node),
            Kind::ExportAssignment => self.visit_export_assignment(node),
            Kind::ExportDeclaration => self.visit_export_declaration(node),
            Kind::CallExpression => Some(self.visit_call_expression(node)),
            _ => self.visitor().visit_each_child(Some(node)),
        }
    }

    // esmodule.go:55
    fn visit_source_file(&self, node: P<SourceFile>) -> P<Node> {
        if node.is_declaration_file.get()
            || !(ast::is_external_module(node) || self.compiler_options.get_isolated_modules())
        {
            return node.as_node();
        }

        self.current_source_file.set(Some(node));
        *self.import_require_statements.borrow_mut() = None;

        let f = self.factory();
        let emit_context = self.emit_context();
        let mut result =
            self.visitor().visit_each_child(Some(node.as_node())).unwrap().as_source_file_p();
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
        let import_require_statements = self.import_require_statements.borrow().clone();
        if external_helpers_import_declaration.is_some() || import_require_statements.is_some() {
            let (prologue, rest) = f.split_standard_prologue(result.statements.nodes());
            let (custom, rest) = f.split_custom_prologue(rest);
            let mut statements: Vec<P<Node>> = prologue.to_vec();
            statements.extend_from_slice(custom);
            if let Some(external_helpers_import_declaration) = external_helpers_import_declaration {
                // The helpers import must be visited so that `import x = require("tslib")`
                // (TypeScript-only syntax) is transformed to `const x = require("tslib")`
                // for CJS output files via visitImportEqualsDeclaration.
                statements
                    .extend(self.visitor().visit_node(Some(external_helpers_import_declaration)));
            }
            if let Some(import_require_statements) =
                self.import_require_statements.borrow().as_ref()
            {
                statements.extend_from_slice(&import_require_statements.statements);
            }
            statements.extend_from_slice(rest);
            let statement_list = f.new_node_list(statements);
            statement_list.loc.set(result.statements.loc.get());
            result = f
                .update_source_file(result.as_node(), statement_list, node.end_of_file_token)
                .as_source_file_p();
        }

        if ast::is_external_module(result)
            && self.compiler_options.get_emit_module_kind() != ModuleKind::PRESERVE
            && !result.statements.nodes().iter().any(|&s| ast::is_external_module_indicator(s))
        {
            let mut statements: Vec<P<Node>> = result.statements.nodes().to_vec();
            statements.push(create_empty_imports(f));
            let statement_list = f.new_node_list(statements);
            statement_list.loc.set(result.statements.loc.get());
            result = f
                .update_source_file(result.as_node(), statement_list, node.end_of_file_token)
                .as_source_file_p();
        }

        *self.import_require_statements.borrow_mut() = None;
        self.current_source_file.set(None);
        result.as_node()
    }

    // esmodule.go:105
    fn visit_import_declaration(&self, node: P<Node>) -> P<Node> {
        if !self.compiler_options.rewrite_relative_import_extensions.is_true() {
            return node;
        }
        let n = node.as_import_declaration();
        let updated_module_specifier = rewrite_module_specifier(
            self.emit_context(),
            Some(n.module_specifier),
            &self.compiler_options,
            &self.content_mapper_extension_rewrites,
            self.use_case_sensitive_file_names,
        );
        let mut v = self.visitor();
        self.factory().update_import_declaration(
            node,
            None, /*modifiers*/
            v.visit_node(n.import_clause),
            updated_module_specifier.unwrap(),
            v.visit_node(n.attributes()),
        )
    }

    // esmodule.go:119
    fn visit_import_equals_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        // Though an error in es2020 modules, in node-flavor es2020 modules, we can helpfully transform this to a synthetic `require` call
        // To give easy access to a synchronous `require` in node-flavor esm. We do the transform even in scenarios where we error, but `import.meta.url`
        // is available, just because the output is reasonable for a node-like runtime.
        if self.compiler_options.get_emit_module_kind() < ModuleKind::NODE16 {
            return None;
        }

        assert!(
            ast::is_external_module_import_equals_declaration(node),
            "import= for internal module references should be handled in an earlier transformer."
        );

        let f = self.factory();
        let var_statement = f.new_variable_statement(
            None, /*modifiers*/
            f.new_variable_declaration_list(
                f.new_node_list(vec![f.new_variable_declaration(
                    node.name().unwrap().clone_node(f),
                    None, /*exclamationToken*/
                    None, /*type*/
                    Some(self.create_require_call(node)),
                )]),
                NodeFlags::Const,
            ),
        );
        self.emit_context().set_original(var_statement, node);
        self.emit_context().assign_comment_and_source_map_ranges(var_statement, node);

        let mut statements: Vec<P<Node>> = Vec::new();
        statements.push(var_statement);
        self.append_exports_of_import_equals_declaration(&mut statements, node);
        single_or_many(Some(statements), f)
    }

    // esmodule.go:154
    fn append_exports_of_import_equals_declaration(
        &self,
        statements: &mut Vec<P<Node>>,
        node: P<Node>,
    ) {
        if ast::has_syntactic_modifier(node, ModifierFlags::Export) {
            let f = self.factory();
            statements.push(f.new_export_declaration(
                None,  /*modifiers*/
                false, /*isTypeOnly*/
                Some(f.new_named_exports(f.new_node_list(vec![f.new_export_specifier(
                    false, /*isTypeOnly*/
                    None,  /*propertyName*/
                    node.name().unwrap().clone_node(f),
                )]))),
                None, /*moduleSpecifier*/
                None, /*attributes*/
            ));
        }
    }

    // esmodule.go:176
    fn visit_export_assignment(&self, node: P<Node>) -> Option<P<Node>> {
        let n = node.as_export_assignment();
        if !n.is_export_equals {
            return self.visitor().visit_each_child(Some(node));
        }
        if self.compiler_options.get_emit_module_kind() != ModuleKind::PRESERVE {
            // Elide `export=` as it is not legal with --module ES6
            return None;
        }
        let f = self.factory();
        let statement = f.new_expression_statement(f.new_assignment_expression(
            f.new_property_access_expression(
                f.new_identifier("module"),
                None, /*questionDotToken*/
                f.new_identifier("exports"),
                NodeFlags::None,
            ),
            self.visitor().visit_node(Some(n.expression())).unwrap(),
        ));
        self.emit_context().set_original(statement, node);
        Some(statement)
    }

    // esmodule.go:200
    fn visit_export_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let n = node.as_export_declaration();
        let Some(module_specifier) = n.module_specifier else {
            return Some(node);
        };

        let f = self.factory();
        let mut v = self.visitor();
        let updated_module_specifier = rewrite_module_specifier(
            self.emit_context(),
            Some(module_specifier),
            &self.compiler_options,
            &self.content_mapper_extension_rewrites,
            self.use_case_sensitive_file_names,
        );
        if self.compiler_options.module > ModuleKind::ES2015
            || n.export_clause.is_none_or(|c| !ast::is_namespace_export(c))
        {
            // Either ill-formed or don't need to be transformed.
            return Some(f.update_export_declaration(
                node,
                None,  /*modifiers*/
                false, /*isTypeOnly*/
                n.export_clause,
                updated_module_specifier,
                v.visit_node(n.attributes()),
            ));
        }

        let export_clause = n.export_clause.unwrap();
        let old_identifier = export_clause.name().unwrap();
        let synth_name = f.new_generated_name_for_node(old_identifier);
        let import_decl = f.new_import_declaration(
            None, /*modifiers*/
            Some(f.new_import_clause(
                Kind::Unknown, /*phaseModifier*/
                None,          /*name*/
                Some(f.new_namespace_import(synth_name)),
            )),
            updated_module_specifier.unwrap(),
            v.visit_node(n.attributes()),
        );
        self.emit_context().set_original(import_decl, export_clause);

        let export_decl = if is_export_namespace_as_default_declaration(node) {
            f.new_export_assignment(
                None,  /*modifiers*/
                false, /*isExportEquals*/
                None,  /*typeNode*/
                synth_name,
            )
        } else {
            f.new_export_declaration(
                None,  /*modifiers*/
                false, /*isTypeOnly*/
                Some(f.new_named_exports(f.new_node_list(vec![f.new_export_specifier(
                    false, /*isTypeOnly*/
                    Some(synth_name),
                    old_identifier,
                )]))),
                None, /*moduleSpecifier*/
                None, /*attributes*/
            )
        };
        self.emit_context().set_original(export_decl, node);
        single_or_many(Some(vec![import_decl, export_decl]), f)
    }

    // esmodule.go:253
    fn visit_call_expression(&self, node: P<Node>) -> P<Node> {
        if self.compiler_options.rewrite_relative_import_extensions.is_true()
            && (ast::is_import_call(node) && !node.arguments().is_empty()
                || ast::is_in_js_file(node)
                    && ast::is_require_call(node, false /*requireStringLiteralLikeArgument*/))
        {
            return self.visit_import_or_require_call(node);
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // esmodule.go:263
    fn visit_import_or_require_call(&self, node: P<Node>) -> P<Node> {
        let n = node.as_call_expression();
        if n.arguments.nodes().is_empty() {
            return self.visitor().visit_each_child(Some(node)).unwrap();
        }

        let f = self.factory();
        let mut v = self.visitor();
        let expression = v.visit_node(node.expression()).unwrap();

        let first = n.arguments.nodes()[0];
        let argument = if ast::is_string_literal_like(first) {
            rewrite_module_specifier(
                self.emit_context(),
                Some(first),
                &self.compiler_options,
                &self.content_mapper_extension_rewrites,
                self.use_case_sensitive_file_names,
            )
            .unwrap()
        } else {
            f.new_rewrite_relative_import_extensions_helper(
                first,
                self.compiler_options.jsx == tsrs_core::JsxEmit::PRESERVE,
                &self.content_mapper_extension_rewrites,
                self.use_case_sensitive_file_names,
            )
        };

        let mut arguments: Vec<P<Node>> = Vec::new();
        arguments.push(argument);

        let rest = v.visit_slice(&n.arguments.nodes()[1..]).0;
        arguments.extend_from_slice(rest);

        let argument_list = f.new_node_list(arguments);
        argument_list.loc.set(n.arguments.loc.get());
        f.update_call_expression(
            node,
            expression,
            n.question_dot_token(),
            None, /*typeArguments*/
            argument_list,
            node.flags(),
        )
    }

    // esmodule.go:297
    fn create_require_call(
        &self,
        node: P<Node>, /*ImportDeclaration | ImportEqualsDeclaration | ExportDeclaration*/
    ) -> P<Node> {
        let f = self.factory();
        let emit_context = self.emit_context();
        let module_name = get_external_module_name_literal(
            f,
            node,
            self.current_source_file.get(),
            None, /*host*/
            None, /*emitResolver*/
            &self.compiler_options,
        );

        let mut args: Vec<P<Node>> = Vec::new();
        if let Some(module_name) = module_name {
            args.push(
                rewrite_module_specifier(
                    emit_context,
                    Some(module_name),
                    &self.compiler_options,
                    &self.content_mapper_extension_rewrites,
                    self.use_case_sensitive_file_names,
                )
                .unwrap(),
            );
        }

        if self.compiler_options.get_emit_module_kind() == ModuleKind::PRESERVE {
            return f.new_call_expression(
                f.new_identifier("require"),
                None, /*questionDotToken*/
                None, /*typeArguments*/
                f.new_node_list(args),
                NodeFlags::None,
            );
        }

        if self.import_require_statements.borrow().is_none() {
            let create_require_name = f.new_unique_name_ex(
                "_createRequire",
                AutoGenerateOptions {
                    flags: GeneratedIdentifierFlags::Optimistic
                        | GeneratedIdentifierFlags::FileLevel,
                    ..Default::default()
                },
            );
            let import_statement = f.new_import_declaration(
                None, /*modifiers*/
                Some(f.new_import_clause(
                    Kind::Unknown, /*phaseModifier*/
                    None,          /*name*/
                    Some(f.new_named_imports(f.new_node_list(vec![f.new_import_specifier(
                        false, /*isTypeOnly*/
                        Some(f.new_identifier("createRequire")),
                        create_require_name,
                    )]))),
                )),
                f.new_string_literal("module", TokenFlags::None),
                None, /*attributes*/
            );
            emit_context.add_emit_flags(import_statement, EmitFlags::CustomPrologue);

            let require_helper_name = f.new_unique_name_ex(
                "__require",
                AutoGenerateOptions {
                    flags: GeneratedIdentifierFlags::Optimistic
                        | GeneratedIdentifierFlags::FileLevel,
                    ..Default::default()
                },
            );
            let require_statement = f.new_variable_statement(
                None, /*modifiers*/
                f.new_variable_declaration_list(
                    f.new_node_list(vec![f.new_variable_declaration(
                        require_helper_name,
                        None, /*exclamationToken*/
                        None, /*type*/
                        Some(f.new_call_expression(
                            create_require_name.clone_node(f),
                            None, /*questionDotToken*/
                            None, /*typeArguments*/
                            f.new_node_list(vec![f.new_property_access_expression(
                                f.new_meta_property(Kind::ImportKeyword, f.new_identifier("meta")),
                                None, /*questionDotToken*/
                                f.new_identifier("url"),
                                NodeFlags::None,
                            )]),
                            NodeFlags::None,
                        )),
                    )]),
                    NodeFlags::Const,
                ),
            );
            emit_context.add_emit_flags(require_statement, EmitFlags::CustomPrologue);
            *self.import_require_statements.borrow_mut() = Some(ImportRequireStatements {
                statements: vec![import_statement, require_statement],
                require_helper_name,
            });
        }

        let require_helper_name =
            self.import_require_statements.borrow().as_ref().unwrap().require_helper_name;
        f.new_call_expression(
            require_helper_name.clone_node(f),
            None, /*questionDotToken*/
            None, /*typeArguments*/
            f.new_node_list(args),
            NodeFlags::None,
        )
    }
}
