use super::*;

/// The pending-statement state shared by the `commitPendingVariables` / `commitPendingExpressions` /
/// `pushVariable` / `pushExpression` closures of `visitTopLevelVariableStatement`.
struct PendingTopLevelVariables {
    statements: Vec<P<Node>>,
    variables: Vec<P<Node>>,
    expressions: Vec<P<Node>>,
    modifiers: Option<P<ModifierList>>,
}

impl CommonJSModuleTransformer {
    // commonjsmodule.go:986
    pub(crate) fn visit_top_level_variable_statement(&self, node: P<Node>) -> Option<P<Node>> {
        if !ast::has_syntactic_modifier(node, ModifierFlags::Export) {
            return self.visit_top_level_nested_variable_statement(node);
        }
        let f = self.factory();
        let emit_context = self.emit_context();
        let mut v = self.visitor();
        let declaration_list = node.as_variable_statement().declaration_list;
        // export var a = b;
        let mut st = PendingTopLevelVariables {
            statements: Vec::new(),
            variables: Vec::new(),
            expressions: Vec::new(),
            modifiers: None,
        };

        let commit_pending_variables = |st: &mut PendingTopLevelVariables| {
            if !st.variables.is_empty() {
                let variable_list = f.new_node_list(std::mem::take(&mut st.variables));
                let statement = f.update_variable_statement(
                    node,
                    st.modifiers,
                    f.update_variable_declaration_list(
                        declaration_list,
                        variable_list,
                        declaration_list.flags(),
                    ),
                );
                if !st.statements.is_empty() {
                    emit_context.add_emit_flags(statement, EmitFlags::NoComments);
                }
                st.statements.push(statement);
            }
        };

        let commit_pending_expressions = |st: &mut PendingTopLevelVariables| {
            if !st.expressions.is_empty() {
                let statement =
                    f.new_expression_statement(f.inline_expressions(&st.expressions).unwrap());
                st.expressions.clear();
                emit_context.assign_comment_and_source_map_ranges(statement, node);
                if !st.statements.is_empty() {
                    emit_context.add_emit_flags(statement, EmitFlags::NoComments);
                }
                st.statements.push(statement);
            }
        };

        let push_variable = |st: &mut PendingTopLevelVariables, variable: P<Node>| {
            commit_pending_expressions(st);
            st.variables.push(variable);
        };

        let push_expression = |st: &mut PendingTopLevelVariables, expression: P<Node>| {
            commit_pending_variables(st);
            st.expressions.push(expression);
        };

        // If we're exporting these variables, then these just become assignments to 'exports.x'.
        for &variable in declaration_list.as_variable_declaration_list().declarations.nodes() {
            let var = variable.as_variable_declaration();
            let name = variable.name().unwrap();
            let initializer = variable.initializer();

            if ast::is_identifier(name) && is_local_name(emit_context, name) {
                // A "local name" generally means a variable declaration that *shouldn't* be
                // converted to `exports.x = ...`, even if the declaration is exported. This
                // usually indicates a class or function declaration that was converted into
                // a variable declaration, as most references to the declaration will remain
                // untransformed (i.e., `new C` rather than `new exports.C`). In these cases,
                // an `export { x }` declaration will follow.

                if st.modifiers.is_none() {
                    st.modifiers = extract_modifiers(
                        emit_context,
                        node.modifiers(),
                        !ModifierFlags::ExportDefault,
                    );
                }

                let mut variable = variable;
                if let Some(initializer) = initializer {
                    variable = f.update_variable_declaration(
                        variable,
                        name,
                        None, /*exclamationToken*/
                        None, /*type*/
                        Some(self.create_export_expression(
                            name,
                            v.visit_node(Some(initializer)).unwrap(),
                            None,
                            false, /*liveBinding*/
                        )),
                    );
                }

                push_variable(&mut st, variable);
            } else if initializer.is_some_and(|i| {
                ast::is_arrow_function(i)
                    || ast::is_function_expression(i)
                    || ast::is_class_expression(i)
            }) && !ast::is_binding_pattern(name)
            {
                // preserve variable declarations for functions and classes to assign names

                push_variable(
                    &mut st,
                    f.new_variable_declaration(
                        name,
                        var.exclamation_token(),
                        var.type_(),
                        v.visit_node(initializer),
                    ),
                );

                let property_access = f.new_property_access_expression(
                    f.new_identifier("exports"),
                    None, /*questionDotToken*/
                    name,
                    NodeFlags::None,
                );
                emit_context.assign_comment_and_source_map_ranges(property_access, name);

                push_expression(
                    &mut st,
                    f.new_assignment_expression(property_access, name.clone_node(f)),
                );
            } else if ast::is_identifier(name) {
                if let Some(expression) = self.transform_initialized_variable(variable) {
                    push_expression(&mut st, v.visit_node(Some(expression)).unwrap());
                }
            } else if ast::is_binding_pattern(name) {
                // For binding patterns with export modifier, use flattenDestructuringAssignment
                // to decompose into individual export assignments
                if let Some(expression) = self.transform_initialized_variable(variable) {
                    push_expression(&mut st, expression);
                }
            } else {
                // For binding patterns, we can't do exports.{pattern} = value
                // Just emit the assignment and let appendExportsOfVariableStatement handle the exports
                if let Some(expression) =
                    convert_variable_declaration_to_assignment_expression(emit_context, variable)
                {
                    push_expression(&mut st, v.visit_node(Some(expression)).unwrap());
                }
            }
        }

        commit_pending_variables(&mut st);
        commit_pending_expressions(&mut st);
        let mut statements = st.statements;
        self.append_exports_of_variable_statement(&mut statements, node);
        // Go's `statements` slice stays nil when nothing was appended (SingleOrMany(nil) is nil, not an empty list).
        single_or_many(if statements.is_empty() { None } else { Some(statements) }, f)
    }

    // commonjsmodule.go:1110
    fn transform_initialized_variable(&self, node: P<Node>) -> Option<P<Node>> {
        let initializer = node.initializer()?;
        let name = node.name().unwrap();
        if ast::is_binding_pattern(name) {
            // Convert the binding pattern into an equivalent assignment expression and visit it
            // as a destructuring assignment. This preserves native destructuring (and therefore
            // iterator semantics for array patterns) whenever each leaf identifier can be
            // substituted to an export reference. Only when the destructuring would assign to
            // re-aliased or multi-exported names (where native destructuring cannot update all
            // targets) does `visitDestructuringAssignment` fall back to flattening.
            let assignment =
                convert_variable_declaration_to_assignment_expression(self.emit_context(), node)
                    .unwrap();
            let grandparent_node = self.push_node(assignment);
            let result =
                self.visit_destructuring_assignment(assignment, true /*valueIsDiscarded*/);
            self.pop_node(grandparent_node);
            return Some(result);
        }
        let f = self.factory();
        let property_access = f.new_property_access_expression(
            f.new_identifier("exports"),
            None, /*questionDotToken*/
            name,
            NodeFlags::None,
        );
        self.emit_context().assign_comment_and_source_map_ranges(property_access, name);
        Some(f.new_assignment_expression(property_access, initializer))
    }

    // Visits a top-level nested variable statement as it may contain `var` declarations that are hoisted and may still be
    // exported with `export {}`.
    // commonjsmodule.go:1138
    fn visit_top_level_nested_variable_statement(&self, node: P<Node>) -> Option<P<Node>> {
        let mut statements: Vec<P<Node>> = Vec::new();
        statements.extend(self.visitor().visit_each_child(Some(node)));
        self.append_exports_of_variable_statement(&mut statements, node);
        single_or_many(Some(statements), self.factory())
    }

    // Visits a top-level nested `for` statement as it may contain `var` declarations that are hoisted and may still be
    // exported with `export {}`.
    // commonjsmodule.go:1147
    pub(crate) fn visit_top_level_nested_for_statement(&self, node: P<Node>) -> Option<P<Node>> {
        let f = self.factory();
        let emit_context = self.emit_context();
        let n = node.as_for_statement();
        if let Some(initializer) = n.initializer
            && ast::is_variable_declaration_list(initializer)
            && !initializer.flags().intersects(NodeFlags::BlockScoped)
        {
            let mut export_statements: Vec<P<Node>> = Vec::new();
            self.append_exports_of_variable_declaration_list(
                &mut export_statements,
                initializer,
                false, /*isForInOrOfInitializer*/
            );
            if !export_statements.is_empty() {
                // given:
                //   export { x }
                //   for (var x = 0; ;) { }
                // emits:
                //   var x = 0;
                //   exports.x = x;
                //   for (; ;) { }

                let mut statements: Vec<P<Node>> = Vec::new();
                let var_decl_list =
                    self.discarded_value_visitor().visit_node(Some(initializer)).unwrap();
                let var_statement =
                    f.new_variable_statement(None /*modifiers*/, var_decl_list);
                statements.push(var_statement);
                statements.extend(export_statements);

                let condition = self.visitor().visit_node(n.condition);
                let incrementor = self.discarded_value_visitor().visit_node(n.incrementor);
                let body = emit_context
                    .visit_iteration_body(Some(n.statement()), &mut self.top_level_nested_visitor())
                    .unwrap();
                statements.push(f.update_for_statement(
                    node,
                    None, /*initializer*/
                    condition,
                    incrementor,
                    body,
                ));
                return single_or_many(Some(statements), f);
            }
        }
        Some(
            f.update_for_statement(
                node,
                self.discarded_value_visitor().visit_node(n.initializer),
                self.visitor().visit_node(n.condition),
                self.discarded_value_visitor().visit_node(n.incrementor),
                emit_context
                    .visit_iteration_body(Some(n.statement()), &mut self.top_level_nested_visitor())
                    .unwrap(),
            ),
        )
    }

    // Visits a top-level nested `for..in` or `for..of` statement as it may contain `var` declarations that are hoisted and
    // may still be exported with `export {}`.
    // commonjsmodule.go:1192
    pub(crate) fn visit_top_level_nested_for_in_or_of_statement(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let emit_context = self.emit_context();
        let n = node.as_for_in_or_of_statement();
        if ast::is_variable_declaration_list(n.initializer)
            && !n.initializer.flags().intersects(NodeFlags::BlockScoped)
        {
            let mut export_statements: Vec<P<Node>> = Vec::new();
            self.append_exports_of_variable_declaration_list(
                &mut export_statements,
                n.initializer,
                true, /*isForInOrOfInitializer*/
            );
            if !export_statements.is_empty() {
                // given:
                //   export { x }
                //   for (var x in y) {
                //     ...
                //   }
                // emits:
                //   for (var x in y) {
                //     exports.x = x;
                //     ...
                //   }

                let initializer =
                    self.discarded_value_visitor().visit_node(Some(n.initializer)).unwrap();
                let expression = self.visitor().visit_node(Some(n.expression)).unwrap();
                let mut body = emit_context
                    .visit_iteration_body(Some(n.statement()), &mut self.top_level_nested_visitor())
                    .unwrap();
                if ast::is_block(body) {
                    let block = body.as_block();
                    let mut body_statements = export_statements;
                    body_statements.extend_from_slice(block.statements.nodes());
                    let body_statement_list = f.new_node_list(body_statements);
                    body_statement_list.loc.set(block.statements.loc.get());
                    body = f.update_block(body, body_statement_list, block.multi_line);
                } else {
                    let mut body_statements = export_statements;
                    body_statements.push(body);
                    body = f.new_block(f.new_node_list(body_statements), true /*multiLine*/);
                }
                return f.update_for_in_or_of_statement(
                    node,
                    n.await_modifier,
                    initializer,
                    expression,
                    body,
                );
            }
        }
        f.update_for_in_or_of_statement(
            node,
            n.await_modifier,
            self.discarded_value_visitor().visit_node(Some(n.initializer)).unwrap(),
            self.visitor().visit_node(Some(n.expression)).unwrap(),
            emit_context
                .visit_iteration_body(Some(n.statement()), &mut self.top_level_nested_visitor())
                .unwrap(),
        )
    }

    // commonjsmodule.go:1240
    pub(crate) fn visit_top_level_nested_do_statement(&self, node: P<Node>) -> P<Node> {
        let n = node.as_do_statement();
        let statement = self
            .emit_context()
            .visit_iteration_body(Some(n.statement()), &mut self.top_level_nested_visitor())
            .unwrap();
        self.factory().update_do_statement(
            node,
            statement,
            self.visitor().visit_node(Some(n.expression)).unwrap(),
        )
    }

    // commonjsmodule.go:1250
    pub(crate) fn visit_top_level_nested_while_statement(&self, node: P<Node>) -> P<Node> {
        let n = node.as_while_statement();
        let expression = self.visitor().visit_node(Some(n.expression)).unwrap();
        self.factory().update_while_statement(
            node,
            expression,
            self.emit_context()
                .visit_iteration_body(Some(n.statement()), &mut self.top_level_nested_visitor())
                .unwrap(),
        )
    }

    // commonjsmodule.go:1260
    pub(crate) fn visit_top_level_nested_labeled_statement(&self, node: P<Node>) -> P<Node> {
        let n = node.as_labeled_statement();
        let statement = self
            .top_level_nested_visitor()
            .visit_embedded_statement(Some(n.statement()))
            .unwrap_or_else(|| self.factory().new_empty_statement());
        self.factory().update_labeled_statement(node, n.label, statement)
    }

    // commonjsmodule.go:1270
    pub(crate) fn visit_top_level_nested_with_statement(&self, node: P<Node>) -> P<Node> {
        let n = node.as_with_statement();
        let statement = self
            .top_level_nested_visitor()
            .visit_embedded_statement(Some(n.statement()))
            .unwrap_or_else(|| self.factory().new_empty_statement());
        let expression = self.visitor().visit_node(Some(n.expression)).unwrap();
        self.factory().update_with_statement(node, expression, statement)
    }

    // commonjsmodule.go:1280
    pub(crate) fn visit_top_level_nested_if_statement(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let n = node.as_if_statement();
        let expression = self.visitor().visit_node(Some(n.expression)).unwrap();
        let then_statement = self
            .top_level_nested_visitor()
            .visit_embedded_statement(Some(n.then_statement))
            .unwrap_or_else(|| f.new_block(f.new_node_list(Vec::new()), false /*multiLine*/));
        let else_statement =
            self.top_level_nested_visitor().visit_embedded_statement(n.else_statement);
        f.update_if_statement(node, expression, then_statement, else_statement)
    }

    // commonjsmodule.go:1292
    pub(crate) fn visit_top_level_nested_switch_statement(&self, node: P<Node>) -> P<Node> {
        let n = node.as_switch_statement();
        let expression = self.visitor().visit_node(Some(n.expression)).unwrap();
        self.factory().update_switch_statement(
            node,
            expression,
            self.top_level_nested_visitor().visit_node(Some(n.case_block)).unwrap(),
        )
    }

    // commonjsmodule.go:1302
    pub(crate) fn visit_top_level_nested_case_block(&self, node: P<Node>) -> Option<P<Node>> {
        self.top_level_nested_visitor().visit_each_child(Some(node))
    }

    // commonjsmodule.go:1308
    pub(crate) fn visit_top_level_nested_case_or_default_clause(&self, node: P<Node>) -> P<Node> {
        let n = node.as_case_or_default_clause();
        let expression = self.visitor().visit_node(n.expression);
        self.factory().update_case_or_default_clause(
            node,
            expression,
            self.top_level_nested_visitor().visit_nodes(Some(n.statements)).unwrap(),
        )
    }

    // commonjsmodule.go:1318
    pub(crate) fn visit_top_level_nested_try_statement(&self, node: P<Node>) -> Option<P<Node>> {
        self.top_level_nested_visitor().visit_each_child(Some(node))
    }

    // commonjsmodule.go:1324
    pub(crate) fn visit_top_level_nested_catch_clause(&self, node: P<Node>) -> P<Node> {
        let n = node.as_catch_clause();
        self.factory().update_catch_clause(
            node,
            n.variable_declaration,
            self.top_level_nested_visitor().visit_node(Some(n.block)).unwrap(),
        )
    }

    // commonjsmodule.go:1334
    pub(crate) fn visit_top_level_nested_block(&self, node: P<Node>) -> Option<P<Node>> {
        self.top_level_nested_visitor().visit_each_child(Some(node))
    }
}
