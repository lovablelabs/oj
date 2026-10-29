use super::*;

// using.go:11
struct UsingDeclarationTransformer {
    base: Transformer,

    export_bindings: RefCell<Option<FxHashMap<&'static str, P<Node>>>>,
    export_binding_names: RefCell<Vec<&'static str>>,
    export_vars: RefCell<Vec<P<Node>>>,
    default_export_binding: Cell<Option<P<Node>>>,
    export_equals_binding: Cell<Option<P<Node>>>,
}

// using.go:21
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_using_declaration_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let tx = P::new(UsingDeclarationTransformer {
        base: Transformer::default(),
        export_bindings: RefCell::new(None),
        export_binding_names: RefCell::new(Vec::new()),
        export_vars: RefCell::new(Vec::new()),
        default_export_binding: Cell::new(None),
        export_equals_binding: Cell::new(None),
    });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| Some(tx.visit(n))),
        Some(opt.context),
    ))
}

// using.go:26
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum UsingKind {
    None,
    Sync,
    Async,
}

impl UsingDeclarationTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    // using.go:34
    fn visit(&self, node: P<Node>) -> P<Node> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsUsing) {
            return node;
        }

        match node.kind() {
            Kind::SourceFile => self.visit_source_file(node.as_source_file_p()),
            Kind::Block => self.visit_block(node),
            Kind::ForStatement => self.visit_for_statement(node),
            Kind::ForOfStatement => self.visit_for_of_statement(node),
            _ => self.visitor().visit_each_child(Some(node)).unwrap(),
        }
    }

    // using.go:54
    fn visit_source_file(&self, node: P<SourceFile>) -> P<Node> {
        if node.is_declaration_file.get() {
            return node.as_node();
        }

        let f = self.factory();
        let emit_context = self.emit_context();

        let using_kind = get_using_kind_of_statements(node.statements.nodes());
        let visited: P<Node> = if using_kind == UsingKind::None {
            self.visitor().visit_each_child(Some(node.as_node())).unwrap()
        } else {
            // Imports and exports must stay at the top level. This means we must hoist all imports, exports, and
            // top-level function declarations and bindings out of the `try` statements we generate (see using.go for
            // the worked example). We hoist bindings, functions, imports and exports to a new outer statement list
            // while moving all other statements in the source file into the `try` block, which is the same approach
            // we use for System module emit. Unlike System module emit, we attempt to preserve all statements prior
            // to the first top-level `using` to isolate the complexity of the transformed output to only where it is
            // necessary.
            emit_context.start_variable_environment();

            *self.export_bindings.borrow_mut() = Some(FxHashMap::default());
            self.export_vars.borrow_mut().clear();

            let (prologue, rest) = f.split_standard_prologue(node.statements.nodes());
            let mut top_level_statements: Vec<P<Node>> = Vec::new();
            top_level_statements.extend_from_slice(self.visitor().visit_slice(prologue).0);

            // Collect and transform any leading statements up to the first `using` or `await using`. This preserves
            // the original statement order much as is possible.

            let mut pos = 0;
            while pos < rest.len() {
                let statement = rest[pos];
                if get_using_kind(statement) != UsingKind::None {
                    if pos > 0 {
                        top_level_statements
                            .extend_from_slice(self.visitor().visit_slice(&rest[..pos]).0);
                    }
                    break;
                }
                pos += 1;
            }

            assert!(pos < rest.len(), "Should have encountered at least one 'using' statement.");

            // transform the rest of the body
            let env_binding = self.create_env_binding();
            let body_statements = self.transform_using_declarations(
                &rest[pos..],
                env_binding,
                Some(&mut top_level_statements),
            );

            // add `export {}` declarations for any hoisted bindings.
            let has_export_bindings =
                self.export_bindings.borrow().as_ref().is_some_and(|b| !b.is_empty());
            if has_export_bindings {
                let export_specifiers: Vec<P<Node>> = {
                    let bindings = self.export_bindings.borrow();
                    let bindings = bindings.as_ref().unwrap();
                    self.export_binding_names
                        .borrow()
                        .iter()
                        .map(|name| {
                            *bindings
                                .get(name)
                                .expect("Missing export binding for hoisted export name")
                        })
                        .collect()
                };
                top_level_statements.push(f.new_export_declaration(
                    None,  /*modifiers*/
                    false, /*isTypeOnly*/
                    Some(f.new_named_exports(f.new_node_list(export_specifiers))),
                    None, /*moduleSpecifier*/
                    None, /*attributes*/
                ));
            }

            top_level_statements.extend(emit_context.end_variable_environment());
            let export_vars = self.export_vars.borrow().clone();
            if !export_vars.is_empty() {
                top_level_statements.push(f.new_variable_statement(
                    Some(f.new_modifier_list(vec![f.new_modifier(Kind::ExportKeyword)])),
                    f.new_variable_declaration_list(f.new_node_list(export_vars), NodeFlags::Let),
                ));
            }
            top_level_statements.extend(self.create_downlevel_using_statements(
                body_statements,
                env_binding,
                using_kind == UsingKind::Async,
            ));

            if let Some(export_equals_binding) = self.export_equals_binding.get() {
                top_level_statements.push(f.new_export_assignment(
                    None, /*modifiers*/
                    true, /*isExportEquals*/
                    None, /*typeNode*/
                    export_equals_binding,
                ));
            }

            f.update_source_file(
                node.as_node(),
                f.new_node_list(top_level_statements),
                node.end_of_file_token,
            )
        };
        emit_context.add_emit_helper(visited, &emit_context.read_emit_helpers());
        self.export_vars.borrow_mut().clear();
        *self.export_bindings.borrow_mut() = None;
        self.export_binding_names.borrow_mut().clear();
        self.default_export_binding.set(None);
        self.export_equals_binding.set(None);
        visited
    }

    // using.go:192
    fn visit_block(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let block = node.as_block();
        let using_kind = get_using_kind_of_statements(block.statements.nodes());
        if using_kind != UsingKind::None {
            let (prologue, rest) = f.split_standard_prologue(block.statements.nodes());
            let env_binding = self.create_env_binding();
            let mut statements: Vec<P<Node>> = Vec::with_capacity(prologue.len() + 2);
            statements.extend_from_slice(self.visitor().visit_slice(prologue).0);
            let body = self.transform_using_declarations(
                rest,
                env_binding,
                None, /*topLevelStatements*/
            );
            statements.extend(self.create_downlevel_using_statements(
                body,
                env_binding,
                using_kind == UsingKind::Async,
            ));
            let statement_list = f.new_node_list(statements);
            statement_list.loc.set(block.statements.loc.get());
            return f.update_block(node, statement_list, block.multi_line);
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // using.go:211
    fn visit_for_statement(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let n = node.as_for_statement();
        if let Some(initializer) = n.initializer
            && is_using_variable_declaration_list(initializer)
        {
            // given:
            //
            //  for (using x = expr; cond; incr) { ... }
            //
            // produces a shallow transformation to:
            //
            //  {
            //    using x = expr;
            //    for (; cond; incr) { ... }
            //  }
            //
            // before handing the shallow transformation back to the visitor for an in-depth transformation.
            return self
                .visitor()
                .visit_node(Some(f.new_block(
                    f.new_node_list(vec![
                        f.new_variable_statement(None /*modifiers*/, initializer),
                        f.update_for_statement(
                            node,
                            None, /*initializer*/
                            n.condition,
                            n.incrementor,
                            n.statement(),
                        ),
                    ]),
                    false, /*multiLine*/
                )))
                .unwrap();
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // using.go:241
    fn visit_for_of_statement(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let n = node.as_for_in_or_of_statement();
        if is_using_variable_declaration_list(n.initializer) {
            // given:
            //
            //  for (using x of y) { ... }
            //
            // produces a shallow transformation to:
            //
            //  for (const x_1 of y) {
            //    using x = x;
            //    ...
            //  }
            //
            // before handing the shallow transformation back to the visitor for an in-depth transformation.
            let for_initializer = n.initializer;
            let for_decl = for_initializer
                .as_variable_declaration_list()
                .declarations
                .nodes()
                .first()
                .copied()
                .unwrap_or_else(|| {
                    f.new_variable_declaration(f.new_temp_variable(), None, None, None)
                });

            let is_await_using =
                get_using_kind_of_variable_declaration_list(for_initializer) == UsingKind::Async;
            let temp = f.new_generated_name_for_node(for_decl.name().unwrap());
            let using_var = f.update_variable_declaration(
                for_decl,
                for_decl.name().unwrap(),
                None, /*exclamationToken*/
                None, /*type*/
                Some(temp),
            );
            let using_var_list = f.new_variable_declaration_list(
                f.new_node_list(vec![using_var]),
                if is_await_using { NodeFlags::AwaitUsing } else { NodeFlags::Using },
            );
            let using_var_statement =
                f.new_variable_statement(None /*modifiers*/, using_var_list);
            let body = n.statement();
            let statement = if ast::is_block(body) {
                let mut statements: Vec<P<Node>> = Vec::with_capacity(body.statements().len() + 1);
                statements.push(using_var_statement);
                statements.extend_from_slice(body.statements());
                f.update_block(body, f.new_node_list(statements), body.as_block().multi_line)
            } else {
                f.new_block(
                    f.new_node_list(vec![using_var_statement, body]),
                    true, /*multiLine*/
                )
            };
            return self
                .visitor()
                .visit_node(Some(f.update_for_in_or_of_statement(
                    node,
                    n.await_modifier,
                    f.new_variable_declaration_list(
                        f.new_node_list(vec![f.new_variable_declaration(
                            temp, None, /*exclamationToken*/
                            None, /*type*/
                            None,
                        )]),
                        NodeFlags::Const,
                    ),
                    n.expression,
                    statement,
                )))
                .unwrap();
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // The `hoist` closure of transformUsingDeclarations (using.go:309).
    fn hoist(
        &self,
        node: P<Node>,
        top_level_statements: &mut Option<&mut Vec<P<Node>>>,
    ) -> Option<P<Node>> {
        let Some(top_level_statements) = top_level_statements else {
            return Some(node);
        };

        match node.kind() {
            Kind::ImportDeclaration
            | Kind::ImportEqualsDeclaration
            | Kind::ExportDeclaration
            | Kind::FunctionDeclaration => {
                Self::hoist_import_or_export_or_hoisted_declaration(node, top_level_statements);
                None
            }
            Kind::ExportAssignment => Some(self.hoist_export_assignment(node)),
            Kind::ClassDeclaration => Some(self.hoist_class_declaration(node)),
            Kind::VariableStatement => self.hoist_variable_statement(node),
            _ => Some(node),
        }
    }

    // using.go:306
    fn transform_using_declarations(
        &self,
        statements_in: &'static [P<Node>],
        env_binding: P<Node>,
        mut top_level_statements: Option<&mut Vec<P<Node>>>,
    ) -> Vec<P<Node>> {
        let f = self.factory();
        let emit_context = self.emit_context();
        let mut statements: Vec<P<Node>> = Vec::new();

        let hoist_or_append_node =
            |this: &Self,
             node: P<Node>,
             top: &mut Option<&mut Vec<P<Node>>>,
             statements: &mut Vec<P<Node>>| {
                if let Some(node) = this.hoist(node, top) {
                    statements.push(node);
                }
            };

        for &statement in statements_in {
            let using_kind = get_using_kind(statement);
            if using_kind != UsingKind::None {
                let var_statement = statement.as_variable_statement();
                let declaration_list = var_statement.declaration_list;
                let mut declarations: Vec<P<Node>> = Vec::new();
                for &declaration in
                    declaration_list.as_variable_declaration_list().declarations.nodes()
                {
                    if !ast::is_identifier(declaration.name().unwrap()) {
                        // Since binding patterns are a grammar error, we reset `declarations` so we don't process this as a `using`.
                        declarations.clear();
                        break;
                    }

                    // perform a shallow transform for any named evaluation
                    let mut declaration = declaration;
                    if is_named_evaluation(emit_context, declaration) {
                        declaration = transform_named_evaluation(
                            emit_context,
                            declaration,
                            false, /*ignoreEmptyStringLiteral*/
                            "",    /*assignedName*/
                        );
                    }

                    let initializer = self
                        .visitor()
                        .visit_node(declaration.initializer())
                        .unwrap_or_else(|| f.new_void_zero_expression());
                    declarations.push(f.update_variable_declaration(
                        declaration,
                        declaration.name().unwrap(),
                        None, /*exclamationToken*/
                        None, /*type*/
                        Some(f.new_add_disposable_resource_helper(
                            env_binding,
                            initializer,
                            using_kind == UsingKind::Async,
                        )),
                    ));
                }

                // Only replace the statement if it was valid.
                if !declarations.is_empty() {
                    let var_list = f.new_variable_declaration_list(
                        f.new_node_list(declarations),
                        NodeFlags::Const,
                    );
                    emit_context.set_original(var_list, declaration_list);
                    var_list.set_loc(declaration_list.loc());
                    hoist_or_append_node(
                        self,
                        f.update_variable_statement(statement, None /*modifiers*/, var_list),
                        &mut top_level_statements,
                        &mut statements,
                    );
                    continue;
                }
            }

            let result = self.visit(statement);
            if result.kind() == Kind::SyntaxList {
                for &node in result.as_syntax_list().children {
                    hoist_or_append_node(self, node, &mut top_level_statements, &mut statements);
                }
            } else {
                hoist_or_append_node(self, result, &mut top_level_statements, &mut statements);
            }
        }
        statements
    }

    // using.go:397
    fn hoist_import_or_export_or_hoisted_declaration(
        node: P<Node>,
        top_level_statements: &mut Vec<P<Node>>,
    ) {
        // NOTE: `node` has already been visited
        top_level_statements.push(node);
    }

    // using.go:402
    fn hoist_export_assignment(&self, node: P<Node>) -> P<Node> {
        if node.as_export_assignment().is_export_equals {
            self.hoist_export_equals(node)
        } else {
            self.hoist_export_default(node)
        }
    }

    // using.go:410
    fn hoist_export_default(&self, node: P<Node>) -> P<Node> {
        // NOTE: `node` has already been visited
        if self.default_export_binding.get().is_some() {
            // invalid case of multiple `export default` declarations. Don't assert here, just pass it through
            return node;
        }

        // given:
        //
        //   export default expr;
        //
        // produces:
        //
        //   // top level
        //   var default_1;
        //   export { default_1 as default };
        //
        //   // body
        //   default_1 = expr;

        let f = self.factory();
        let emit_context = self.emit_context();
        let default_export_binding = f.new_unique_name_ex(
            "_default",
            AutoGenerateOptions {
                flags: GeneratedIdentifierFlags::ReservedInNestedScopes
                    | GeneratedIdentifierFlags::FileLevel
                    | GeneratedIdentifierFlags::Optimistic,
                ..Default::default()
            },
        );
        self.default_export_binding.set(Some(default_export_binding));
        self.hoist_binding_identifier(
            default_export_binding,
            true, /*isExport*/
            Some(f.new_identifier("default")),
            Some(node),
        );

        // give a class or function expression an assigned name, if needed.
        let mut expression = node.expression().unwrap();
        let mut inner_expression =
            ast::skip_outer_expressions(expression, OuterExpressionKinds::All);
        if is_named_evaluation(emit_context, inner_expression) {
            inner_expression = transform_named_evaluation(
                emit_context,
                inner_expression,
                false, /*ignoreEmptyStringLiteral*/
                "default",
            );
            expression = f.restore_outer_expressions(
                Some(expression),
                inner_expression,
                OuterExpressionKinds::All,
            );
        }

        let assignment = f.new_assignment_expression(default_export_binding, expression);
        f.new_expression_statement(assignment)
    }

    // using.go:445
    fn hoist_export_equals(&self, node: P<Node>) -> P<Node> {
        // NOTE: `node` has already been visited
        if self.export_equals_binding.get().is_some() {
            // invalid case of multiple `export default` declarations. Don't assert here, just pass it through
            return node;
        }

        // given:
        //
        //   export = expr;
        //
        // produces:
        //
        //   // top level
        //   var default_1;
        //
        //   try {
        //       // body
        //       default_1 = expr;
        //   } ...
        //
        //   // top level suffix
        //   export = default_1;

        let f = self.factory();
        let export_equals_binding = f.new_unique_name_ex(
            "_default",
            AutoGenerateOptions {
                flags: GeneratedIdentifierFlags::ReservedInNestedScopes
                    | GeneratedIdentifierFlags::FileLevel
                    | GeneratedIdentifierFlags::Optimistic,
                ..Default::default()
            },
        );
        self.export_equals_binding.set(Some(export_equals_binding));
        self.emit_context().add_variable_declaration(export_equals_binding);

        // give a class or function expression an assigned name, if needed.
        let assignment =
            f.new_assignment_expression(export_equals_binding, node.expression().unwrap());
        f.new_expression_statement(assignment)
    }

    // using.go:477
    fn hoist_class_declaration(&self, node: P<Node>) -> P<Node> {
        // NOTE: `node` has already been visited
        if node.name().is_none() && self.default_export_binding.get().is_some() {
            // invalid case of multiple `export default` declarations. Don't assert here, just pass it through
            return node;
        }

        let f = self.factory();
        let emit_context = self.emit_context();
        let is_exported = ast::has_syntactic_modifier(node, ModifierFlags::Export);
        let is_default = ast::has_syntactic_modifier(node, ModifierFlags::Default);

        // When hoisting a class declaration at the top level of a file containing a top-level `using` statement, we
        // must first convert it to a class expression so that we can hoist the binding outside of the `try`.
        let mut expression = convert_class_declaration_to_class_expression(emit_context, node);
        if node.name().is_some() {
            // given:
            //
            //  using x = expr;
            //  class C {}
            //
            // produces:
            //
            //  var x, C;
            //  const env_1 = { ... };
            //  try {
            //    x = __addDisposableResource(env_1, expr, false);
            //    C = class {};
            //  }
            //  catch (e_1) {
            //    env_1.error = e_1;
            //    env_1.hasError = true;
            //  }
            //  finally {
            //    __disposeResources(env_1);
            //  }
            //
            // If the class is exported, we also produce an `export { C };`
            self.hoist_binding_identifier(
                f.get_local_name(node),
                is_exported && !is_default,
                None, /*exportAlias*/
                Some(node),
            );
            expression = f.new_assignment_expression(f.get_declaration_name(node), expression);
            emit_context.set_original(expression, node);
            emit_context.set_source_map_range(expression, node.loc());
            emit_context.set_comment_range(expression, node.loc());
            if is_named_evaluation(emit_context, expression) {
                expression = transform_named_evaluation(
                    emit_context,
                    expression,
                    false, /*ignoreEmptyStringLiteral*/
                    "",    /*assignedName*/
                );
            }
        }

        if is_default && self.default_export_binding.get().is_none() {
            // In the case of a default export, we create a temporary variable that we export as the default and then
            // assign to that variable (`export { default_1 as default }`, `default_1 = C = class {}`). Though we will
            // never reassign `default_1`, this most closely matches the specified runtime semantics.
            let default_export_binding = f.new_unique_name_ex(
                "_default",
                AutoGenerateOptions {
                    flags: GeneratedIdentifierFlags::ReservedInNestedScopes
                        | GeneratedIdentifierFlags::FileLevel
                        | GeneratedIdentifierFlags::Optimistic,
                    ..Default::default()
                },
            );
            self.default_export_binding.set(Some(default_export_binding));
            self.hoist_binding_identifier(
                default_export_binding,
                true, /*isExport*/
                Some(f.new_identifier("default")),
                Some(node),
            );
            expression = f.new_assignment_expression(default_export_binding, expression);
            emit_context.set_original(expression, node);
            if is_named_evaluation(emit_context, expression) {
                expression = transform_named_evaluation(
                    emit_context,
                    expression,
                    false, /*ignoreEmptyStringLiteral*/
                    "default",
                );
            }
        }

        f.new_expression_statement(expression)
    }

    // using.go:562
    fn hoist_variable_statement(&self, node: P<Node>) -> Option<P<Node>> {
        // NOTE: `node` has already been visited
        let f = self.factory();
        let emit_context = self.emit_context();
        let mut expressions: Vec<P<Node>> = Vec::new();
        let is_exported = ast::has_syntactic_modifier(node, ModifierFlags::Export);
        expressions.extend(
            node.as_variable_statement()
                .declaration_list
                .as_variable_declaration_list()
                .declarations
                .nodes()
                .iter()
                .filter_map(|&variable| {
                    self.hoist_binding_element(variable, is_exported, Some(variable));
                    if variable.initializer().is_some() {
                        Some(self.hoist_initialized_variable(variable))
                    } else {
                        None
                    }
                }),
        );
        if !expressions.is_empty() {
            let statement = f.new_expression_statement(f.inline_expressions(&expressions).unwrap());
            emit_context.set_original(statement, node);
            emit_context.set_comment_range(statement, node.loc());
            emit_context.set_source_map_range(statement, node.loc());
            return Some(statement);
        }
        None
    }

    // using.go:582
    fn hoist_initialized_variable(&self, node: P<Node>) -> P<Node> {
        // NOTE: `node` has already been visited
        let f = self.factory();
        let emit_context = self.emit_context();
        let Some(initializer) = node.initializer() else {
            panic!("Expected initializer");
        };
        let name = node.name().unwrap();
        let target = if ast::is_identifier(name) {
            let target = name.clone_node(f);
            emit_context.set_emit_flags(
                target,
                emit_context.emit_flags(target) & !(EmitFlags::LocalName | EmitFlags::ExportName),
            );
            target
        } else {
            convert_binding_pattern_to_assignment_pattern(emit_context, name)
        };

        let assignment = f.new_assignment_expression(target, initializer);
        emit_context.set_original(assignment, node);
        emit_context.set_comment_range(assignment, node.loc());
        emit_context.set_source_map_range(assignment, node.loc());
        assignment
    }

    // using.go:602
    fn hoist_binding_element(
        &self,
        node: P<Node>, /*VariableDeclaration|BindingElement*/
        is_exported_declaration: bool,
        original: Option<P<Node>>,
    ) {
        // NOTE: `node` has already been visited
        let name = node.name().unwrap();
        if ast::is_binding_pattern(name) {
            for &element in name.elements() {
                if element.name().is_some() {
                    self.hoist_binding_element(element, is_exported_declaration, original);
                }
            }
        } else {
            self.hoist_binding_identifier(
                name,
                is_exported_declaration,
                None, /*exportAlias*/
                original,
            );
        }
    }

    // using.go:615
    fn hoist_binding_identifier(
        &self,
        node: P<Node>,
        is_export: bool,
        export_alias: Option<P<Node>>,
        original: Option<P<Node>>,
    ) {
        // NOTE: `node` has already been visited
        let f = self.factory();
        let emit_context = self.emit_context();
        let mut name = node;
        if !is_generated_identifier(emit_context, node) {
            name = name.clone_node(f);
        }
        if is_export {
            if export_alias.is_none() && !is_local_name(emit_context, name) {
                let var_decl = f.new_variable_declaration(
                    name, None, /*exclamationToken*/
                    None, /*type*/
                    None, /*initializer*/
                );
                if let Some(original) = original {
                    emit_context.set_original(var_decl, original);
                }
                self.export_vars.borrow_mut().push(var_decl);
                return;
            }

            let (local_name, export_name) = match export_alias {
                Some(export_alias) => (Some(name), export_alias),
                None => (None, name),
            };
            let specifier =
                f.new_export_specifier(false /*isTypeOnly*/, local_name, export_name);
            if let Some(original) = original {
                emit_context.set_original(specifier, original);
            }
            let mut bindings = self.export_bindings.borrow_mut();
            let bindings = bindings.get_or_insert_with(FxHashMap::default);
            if !bindings.contains_key(name.text()) {
                self.export_binding_names.borrow_mut().push(name.text());
            }
            bindings.insert(name.text(), specifier);
        }
        emit_context.add_variable_declaration(name);
    }

    // using.go:654
    fn create_env_binding(&self) -> P<Node> {
        self.factory().new_unique_name("env")
    }

    // using.go:658
    fn create_downlevel_using_statements(
        &self,
        body_statements: Vec<P<Node>>,
        env_binding: P<Node>,
        async_: bool,
    ) -> Vec<P<Node>> {
        let f = self.factory();
        let mut statements: Vec<P<Node>> = Vec::with_capacity(2);

        // produces:
        //
        //  const env_1 = { stack: [], error: void 0, hasError: false };
        //
        // (Go passes a nil element list to NewArrayLiteralExpression; the Rust factory takes a list, so an empty
        // one stands in. Both print `[]`.)
        let env_object = f.new_object_literal_expression(
            f.new_node_list(vec![
                f.new_property_assignment(
                    None, /*modifiers*/
                    f.new_identifier("stack"),
                    None, /*postfixToken*/
                    None, /*typeNode*/
                    f.new_array_literal_expression(
                        f.new_node_list(Vec::new()),
                        false, /*multiLine*/
                    ),
                ),
                f.new_property_assignment(
                    None, /*modifiers*/
                    f.new_identifier("error"),
                    None, /*postfixToken*/
                    None, /*typeNode*/
                    f.new_void_zero_expression(),
                ),
                f.new_property_assignment(
                    None, /*modifiers*/
                    f.new_identifier("hasError"),
                    None, /*postfixToken*/
                    None, /*typeNode*/
                    f.new_false_expression(),
                ),
            ]),
            false, /*multiLine*/
        );
        let env_var = f.new_variable_declaration(
            env_binding,
            None, /*exclamationToken*/
            None, /*typeNode*/
            Some(env_object),
        );
        let env_var_list =
            f.new_variable_declaration_list(f.new_node_list(vec![env_var]), NodeFlags::Const);
        let env_var_statement = f.new_variable_statement(None /*modifiers*/, env_var_list);
        statements.push(env_var_statement);

        // when `async` is `false`, produces:
        //
        //  try {
        //    <bodyStatements>
        //  }
        //  catch (e_1) {
        //      env_1.error = e_1;
        //      env_1.hasError = true;
        //  }
        //  finally {
        //    __disposeResources(env_1);
        //  }

        // when `async` is `true`, produces:
        //
        //  try {
        //    <bodyStatements>
        //  }
        //  catch (e_1) {
        //      env_1.error = e_1;
        //      env_1.hasError = true;
        //  }
        //  finally {
        //    const result_1 = __disposeResources(env_1);
        //    if (result_1) {
        //      await result_1;
        //    }
        //  }

        // Unfortunately, it is necessary to use two properties to indicate an error because `throw undefined` is legal
        // JavaScript.
        let try_block = f.new_block(f.new_node_list(body_statements), true /*multiLine*/);
        let body_catch_binding = f.new_unique_name("e");
        let catch_clause = f.new_catch_clause(
            Some(f.new_variable_declaration(
                body_catch_binding,
                None, /*exclamationToken*/
                None, /*type*/
                None, /*initializer*/
            )),
            f.new_block(
                f.new_node_list(vec![
                    f.new_expression_statement(f.new_assignment_expression(
                        f.new_property_access_expression(
                            env_binding,
                            None,
                            f.new_identifier("error"),
                            NodeFlags::None,
                        ),
                        body_catch_binding,
                    )),
                    f.new_expression_statement(f.new_assignment_expression(
                        f.new_property_access_expression(
                            env_binding,
                            None,
                            f.new_identifier("hasError"),
                            NodeFlags::None,
                        ),
                        f.new_true_expression(),
                    )),
                ]),
                true, /*multiLine*/
            ),
        );

        let finally_block = if async_ {
            let result = f.new_unique_name("result");
            f.new_block(
                f.new_node_list(vec![
                    f.new_variable_statement(
                        None, /*modifiers*/
                        f.new_variable_declaration_list(
                            f.new_node_list(vec![f.new_variable_declaration(
                                result,
                                None, /*exclamationToken*/
                                None, /*type*/
                                Some(f.new_dispose_resources_helper(env_binding)),
                            )]),
                            NodeFlags::Const,
                        ),
                    ),
                    f.new_if_statement(
                        result,
                        f.new_expression_statement(f.new_await_expression(result)),
                        None, /*elseStatement*/
                    ),
                ]),
                true, /*multiLine*/
            )
        } else {
            f.new_block(
                f.new_node_list(vec![
                    f.new_expression_statement(f.new_dispose_resources_helper(env_binding)),
                ]),
                true, /*multiLine*/
            )
        };

        let try_statement = f.new_try_statement(try_block, Some(catch_clause), Some(finally_block));
        statements.push(try_statement);
        statements
    }
}

// using.go:761
fn is_using_variable_declaration_list(node: P<Node>) -> bool {
    ast::is_variable_declaration_list(node)
        && get_using_kind_of_variable_declaration_list(node) != UsingKind::None
}

// using.go:765
fn get_using_kind_of_variable_declaration_list(node: P<Node>) -> UsingKind {
    let flags = node.flags() & NodeFlags::BlockScoped;
    if flags == NodeFlags::AwaitUsing {
        UsingKind::Async
    } else if flags == NodeFlags::Using {
        UsingKind::Sync
    } else {
        UsingKind::None
    }
}

// using.go:776
fn get_using_kind_of_variable_statement(node: P<Node>) -> UsingKind {
    get_using_kind_of_variable_declaration_list(node.as_variable_statement().declaration_list)
}

// using.go:780
fn get_using_kind(statement: P<Node>) -> UsingKind {
    if ast::is_variable_statement(statement) {
        return get_using_kind_of_variable_statement(statement);
    }
    UsingKind::None
}

// using.go:787
fn get_using_kind_of_statements(statements: &[P<Node>]) -> UsingKind {
    let mut result = UsingKind::None;
    for &statement in statements {
        let using_kind = get_using_kind(statement);
        if using_kind == UsingKind::Async {
            return UsingKind::Async;
        }
        if using_kind > result {
            result = using_kind;
        }
    }
    result
}
