use crate::*;
use tsrs_core::TextRange;

// objectrestspread.go:10
struct ObjectRestSpreadTransformer {
    base: Transformer,
    compiler_options: P<CompilerOptions>,

    in_exported_variable_statement: Cell<bool>,
    expression_result_is_unused: Cell<bool>,

    parameters_with_preceding_object_rest_or_spread: RefCell<Option<FxHashSet<P<Node>>>>,
}

impl ObjectRestSpreadTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    // objectrestspread.go:20
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsESObjectRestOrSpread)
            && self.parameters_with_preceding_object_rest_or_spread.borrow().is_none()
        {
            return Some(node);
        }
        // Save the expressionResultIsUnused flag set by the parent for this node,
        // then reset to false for children (the default). Specific cases below override as needed.
        let expression_result_is_unused = self.expression_result_is_unused.get();
        self.expression_result_is_unused.set(false);
        // Go: defer func() { ch.expressionResultIsUnused = expressionResultIsUnused }()
        let result = match node.kind() {
            Kind::SourceFile => Some(self.visit_source_file(node)),
            Kind::ObjectLiteralExpression => Some(self.visit_object_literal_expression(node)),
            Kind::BinaryExpression => {
                Some(self.visit_binary_expression(node, expression_result_is_unused))
            }
            Kind::ExpressionStatement => {
                self.expression_result_is_unused.set(true);
                self.visitor().visit_each_child(Some(node))
            }
            Kind::ParenthesizedExpression => {
                self.expression_result_is_unused.set(expression_result_is_unused);
                self.visitor().visit_each_child(Some(node))
            }
            Kind::ForOfStatement => Some(self.visit_for_oftatement(node)),
            Kind::VariableStatement => Some(self.visit_variable_statement(node)),
            Kind::VariableDeclaration => self.visit_variable_declaration(node),
            Kind::CatchClause => Some(self.visit_catch_clause(node)),
            Kind::Parameter => Some(self.visit_parameter(node)),
            Kind::Constructor => Some(self.visit_contructor_declaration(node)),
            Kind::GetAccessor => Some(self.visit_get_accessor_declaration(node)),
            Kind::SetAccessor => Some(self.visit_set_accessor_declaration(node)),
            Kind::MethodDeclaration => Some(self.visit_method_declaration(node)),
            Kind::FunctionDeclaration => Some(self.visit_function_declaration(node)),
            Kind::ArrowFunction => Some(self.visit_arrow_function(node)),
            Kind::FunctionExpression => Some(self.visit_function_expression(node)),
            _ => self.visitor().visit_each_child(Some(node)),
        };
        self.expression_result_is_unused.set(expression_result_is_unused);
        result
    }

    // objectrestspread.go:71
    fn visit_source_file(&self, node: P<Node>) -> P<Node> {
        let visited = self.visitor().visit_each_child(Some(node)).unwrap();
        self.emit_context().add_emit_helper(visited, &self.emit_context().read_emit_helpers());
        visited
    }

    // objectrestspread.go:77
    fn visit_parameter(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let p = node.as_parameter_declaration();
        let in_set = self
            .parameters_with_preceding_object_rest_or_spread
            .borrow()
            .as_ref()
            .is_some_and(|m| m.contains(&node));
        if in_set {
            let mut name = node.name().unwrap();
            if ast::is_binding_pattern(name) {
                name = f.new_generated_name_for_node(node);
            }
            return f.update_parameter_declaration(
                node,
                None,
                p.dot_dot_dot_token(),
                name,
                None,
                None,
                None,
            );
        }
        if node.subtree_facts().intersects(SubtreeFacts::ContainsObjectRestOrSpread) {
            // Binding patterns are converted into a generated name and are
            // evaluated inside the function body.
            return f.update_parameter_declaration(
                node,
                None,
                p.dot_dot_dot_token(),
                f.new_generated_name_for_node(node),
                None,
                None,
                self.visitor().visit_node(node.initializer()),
            );
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // objectrestspread.go:111
    fn collect_parameters_with_preceding_object_rest_or_spread(
        node: P<Node>,
    ) -> Option<FxHashSet<P<Node>>> {
        let mut result: Option<FxHashSet<P<Node>>> = None;
        for &parameter in node.parameters() {
            if let Some(result) = result.as_mut() {
                result.insert(parameter);
            } else if parameter.subtree_facts().intersects(SubtreeFacts::ContainsObjectRestOrSpread)
            {
                result = Some(FxHashSet::default());
            }
        }
        result
    }

    // objectrestspread.go:125 (Go `oldParamScope` is the saved map)
    fn enter_parameter_list_context(&self, node: P<Node>) -> Option<FxHashSet<P<Node>>> {
        let collected = Self::collect_parameters_with_preceding_object_rest_or_spread(node);
        self.parameters_with_preceding_object_rest_or_spread.replace(collected)
    }

    // objectrestspread.go:131
    fn exit_parameter_list_context(&self, scope: Option<FxHashSet<P<Node>>>) {
        *self.parameters_with_preceding_object_rest_or_spread.borrow_mut() = scope;
    }

    // objectrestspread.go:135
    fn visit_contructor_declaration(&self, node: P<Node>) -> P<Node> {
        let old = self.enter_parameter_list_context(node);
        let result = self.factory().update_constructor_declaration(
            node,
            node.modifiers(),
            None,
            self.visitor().visit_nodes(node.parameter_list()),
            None,
            None,
            self.transform_function_body(node),
        );
        self.exit_parameter_list_context(old);
        result
    }

    // objectrestspread.go:149
    fn visit_get_accessor_declaration(&self, node: P<Node>) -> P<Node> {
        let old = self.enter_parameter_list_context(node);
        let result = self.factory().update_get_accessor_declaration(
            node,
            node.modifiers(),
            self.visitor().visit_node(node.name()).unwrap(),
            None,
            self.visitor().visit_nodes(node.parameter_list()),
            None,
            None,
            self.transform_function_body(node),
        );
        self.exit_parameter_list_context(old);
        result
    }

    // objectrestspread.go:164
    fn visit_set_accessor_declaration(&self, node: P<Node>) -> P<Node> {
        let old = self.enter_parameter_list_context(node);
        let result = self.factory().update_set_accessor_declaration(
            node,
            node.modifiers(),
            self.visitor().visit_node(node.name()).unwrap(),
            None,
            self.visitor().visit_nodes(node.parameter_list()),
            None,
            None,
            self.transform_function_body(node),
        );
        self.exit_parameter_list_context(old);
        result
    }

    // objectrestspread.go:179
    fn visit_method_declaration(&self, node: P<Node>) -> P<Node> {
        let old = self.enter_parameter_list_context(node);
        let result = self.factory().update_method_declaration(
            node,
            node.modifiers(),
            node.as_method_declaration().body_base.asterisk_token,
            self.visitor().visit_node(node.name()).unwrap(),
            node.postfix_token(),
            None,
            self.visitor().visit_nodes(node.parameter_list()),
            None,
            None,
            self.transform_function_body(node),
        );
        self.exit_parameter_list_context(old);
        result
    }

    // objectrestspread.go:196
    fn visit_function_declaration(&self, node: P<Node>) -> P<Node> {
        let old = self.enter_parameter_list_context(node);
        let result = self.factory().update_function_declaration(
            node,
            node.modifiers(),
            node.as_function_declaration().body_base.asterisk_token,
            self.visitor().visit_node(node.name()),
            None,
            self.visitor().visit_nodes(node.parameter_list()),
            None,
            None,
            self.transform_function_body(node),
        );
        self.exit_parameter_list_context(old);
        result
    }

    // objectrestspread.go:212
    fn visit_arrow_function(&self, node: P<Node>) -> P<Node> {
        let old = self.enter_parameter_list_context(node);
        let result = self.factory().update_arrow_function(
            node,
            node.modifiers(),
            None,
            self.visitor().visit_nodes(node.parameter_list()),
            None,
            None,
            node.as_arrow_function().equals_greater_than_token,
            self.transform_function_body(node),
        );
        self.exit_parameter_list_context(old);
        result
    }

    // objectrestspread.go:227
    fn visit_function_expression(&self, node: P<Node>) -> P<Node> {
        let old = self.enter_parameter_list_context(node);
        let result = self.factory().update_function_expression(
            node,
            node.modifiers(),
            node.as_function_expression().body_base.asterisk_token,
            self.visitor().visit_node(node.name()),
            None,
            self.visitor().visit_nodes(node.parameter_list()),
            None,
            None,
            self.transform_function_body(node),
        );
        self.exit_parameter_list_context(old);
        result
    }

    // objectrestspread.go:243
    fn transform_function_body(&self, node: P<Node>) -> Option<P<Node>> {
        let f = self.factory();
        // EmitContext().VisitFunctionBody is not used here because this transformer needs to inject
        // object rest assignments between visiting the body and merging the variable environment.
        self.emit_context().start_variable_environment();
        let mut body = self.visitor().visit_node(node.body());
        let extras = self.emit_context().end_variable_environment();
        self.emit_context().start_variable_environment();
        let new_statements = self.collect_object_rest_assignments(node);
        let extras = self.emit_context().end_and_merge_variable_environment(&extras);
        if new_statements.is_empty() && extras.is_empty() {
            return body;
        }

        if body.is_none() {
            body = Some(f.new_block(f.new_node_list(vec![]), true));
        }
        let mut body = body.unwrap();
        let mut prefix: Vec<P<Node>> = Vec::new();
        let mut suffix: Vec<P<Node>> = Vec::new();
        if ast::is_block(body) {
            let mut custom = false;
            let statements = body.statements();
            for (i, &statement) in statements.iter().enumerate() {
                if !custom && ast::is_prologue_directive(statement) {
                    prefix.push(statement);
                } else if self
                    .emit_context()
                    .emit_flags(statement)
                    .intersects(EmitFlags::CustomPrologue)
                {
                    custom = true;
                    prefix.push(statement);
                } else {
                    suffix = statements[i..].to_vec();
                    break;
                }
            }
        } else {
            let ret = f.new_return_statement(Some(body));
            ret.set_loc(body.loc());
            let list = f.new_node_list(vec![]);
            list.loc.set(body.loc());
            body = f.new_block(list, true);
            suffix.push(ret);
        }

        let mut all = prefix;
        all.extend(extras);
        all.extend(new_statements);
        all.extend(suffix);
        let new_statement_list = f.new_node_list(all);
        new_statement_list.loc.set(body.statement_list().unwrap().loc.get());
        Some(f.update_block(body, new_statement_list, body.as_block().multi_line))
    }

    // objectrestspread.go:288
    fn collect_object_rest_assignments(&self, node: P<Node>) -> Vec<P<Node>> {
        let f = self.factory();
        let mut contains_preceding_object_rest_or_spread = false;
        let mut results: Vec<P<Node>> = Vec::new();
        for &parameter in node.parameters() {
            if contains_preceding_object_rest_or_spread {
                if ast::is_binding_pattern(parameter.name().unwrap()) {
                    // In cases where a binding pattern is simply '[]' or '{}',
                    // we usually don't want to emit a var declaration; however, in the presence
                    // of an initializer, we must emit that expression to preserve side effects.
                    if !parameter.name().unwrap().elements().is_empty() {
                        let declarations = flatten_destructuring_binding(
                            &self.base,
                            parameter,
                            Some(f.new_generated_name_for_node(parameter)),
                            FlattenLevel::All,
                            false,
                            false,
                        );
                        if let Some(declarations) = declarations {
                            let mut decls = vec![declarations];
                            if declarations.kind() == Kind::SyntaxList {
                                decls = declarations.as_syntax_list().children.to_vec();
                            }
                            let declaration_list = f.new_variable_declaration_list(
                                f.new_node_list(decls),
                                NodeFlags::None,
                            );
                            let statement = f.new_variable_statement(None, declaration_list);
                            self.emit_context()
                                .add_emit_flags(statement, EmitFlags::CustomPrologue);
                            results.push(statement);
                        }
                    } else if parameter.initializer().is_some() {
                        let name = f.new_generated_name_for_node(parameter);
                        let initializer =
                            self.visitor().visit_node(parameter.initializer()).unwrap();
                        let assignment = f.new_assignment_expression(name, initializer);
                        let statement = f.new_expression_statement(assignment);
                        self.emit_context().add_emit_flags(statement, EmitFlags::CustomPrologue);
                        results.push(statement);
                    }
                } else if parameter.initializer().is_some() {
                    // Converts a parameter initializer into a function body statement, i.e.:
                    //
                    //  function f(x = 1) { }
                    //
                    // becomes
                    //
                    //  function f(x) {
                    //    if (typeof x === "undefined") { x = 1; }
                    //  }
                    let name = parameter.name().unwrap().clone_node(f);
                    name.set_loc(parameter.name().unwrap().loc());
                    self.emit_context().add_emit_flags(name, EmitFlags::NoSourceMap);

                    let initializer = self.visitor().visit_node(parameter.initializer()).unwrap();
                    self.emit_context().add_emit_flags(
                        initializer,
                        EmitFlags::NoSourceMap | EmitFlags::NoComments,
                    );

                    let assignment = f.new_assignment_expression(name, initializer);
                    assignment.set_loc(parameter.loc());
                    self.emit_context().add_emit_flags(assignment, EmitFlags::NoComments);

                    let block = f.new_block(
                        f.new_node_list(vec![f.new_expression_statement(assignment)]),
                        false,
                    );
                    block.set_loc(parameter.loc());
                    self.emit_context().add_emit_flags(
                        block,
                        EmitFlags::SingleLine
                            | EmitFlags::NoTrailingSourceMap
                            | EmitFlags::NoTokenSourceMaps
                            | EmitFlags::NoComments,
                    );

                    let type_check = f.new_type_check(name.clone_node(f), "undefined");
                    let statement = f.new_if_statement(type_check, block, None);
                    statement.set_loc(parameter.loc());
                    self.emit_context().add_emit_flags(
                        statement,
                        EmitFlags::NoTokenSourceMaps
                            | EmitFlags::NoTrailingSourceMap
                            | EmitFlags::CustomPrologue
                            | EmitFlags::NoComments
                            | EmitFlags::StartOnNewLine,
                    );
                    results.push(statement);
                }
            } else if parameter.subtree_facts().intersects(SubtreeFacts::ContainsObjectRestOrSpread)
            {
                contains_preceding_object_rest_or_spread = true;
                let declarations = flatten_destructuring_binding(
                    &self.base,
                    parameter,
                    Some(f.new_generated_name_for_node(parameter)),
                    FlattenLevel::ObjectRest,
                    false,
                    true,
                );
                if let Some(declarations) = declarations {
                    let mut decls = vec![declarations];
                    if declarations.kind() == Kind::SyntaxList {
                        decls = declarations.as_syntax_list().children.to_vec();
                    }
                    let declaration_list =
                        f.new_variable_declaration_list(f.new_node_list(decls), NodeFlags::None);
                    let statement = f.new_variable_statement(None, declaration_list);
                    self.emit_context().add_emit_flags(statement, EmitFlags::CustomPrologue);
                    results.push(statement);
                }
            }
        }

        results
    }

    // objectrestspread.go:378
    fn visit_catch_clause(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let c = node.as_catch_clause();
        if let Some(variable_declaration) = c.variable_declaration
            && ast::is_binding_pattern(variable_declaration.name().unwrap())
            && variable_declaration
                .name()
                .unwrap()
                .subtree_facts()
                .intersects(SubtreeFacts::ContainsObjectRestOrSpread)
        {
            let name = f.new_generated_name_for_node(variable_declaration.name().unwrap());
            let updated_decl = f.update_variable_declaration(
                variable_declaration,
                variable_declaration.name().unwrap(),
                None,
                None,
                Some(name),
            );
            let visited_bindings = flatten_destructuring_binding(
                &self.base,
                updated_decl,
                None,
                FlattenLevel::ObjectRest,
                false,
                false,
            );
            let mut block = self.visitor().visit_node(Some(c.block)).unwrap();
            if let Some(visited_bindings) = visited_bindings {
                let decls: Vec<P<Node>> = if visited_bindings.kind() == Kind::SyntaxList {
                    visited_bindings.as_syntax_list().children.to_vec()
                } else {
                    vec![visited_bindings]
                };
                let new_statement = f.new_variable_statement(
                    None,
                    f.new_variable_declaration_list(f.new_node_list(decls), NodeFlags::None),
                );
                let mut statements = vec![new_statement];
                statements.extend_from_slice(block.statements());
                let statement_list = f.new_node_list(statements);
                statement_list.loc.set(block.statement_list().unwrap().loc.get());

                block = f.update_block(block, statement_list, block.as_block().multi_line);
            }
            return f.update_catch_clause(
                node,
                Some(f.update_variable_declaration(variable_declaration, name, None, None, None)),
                block,
            );
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // objectrestspread.go:412
    fn visit_variable_statement(&self, node: P<Node>) -> P<Node> {
        if ast::has_syntactic_modifier(node, ModifierFlags::Export) {
            let old_in_exported_variable_statement = self.in_exported_variable_statement.get();
            self.in_exported_variable_statement.set(true);
            let result = self.visitor().visit_each_child(Some(node)).unwrap();
            self.in_exported_variable_statement.set(old_in_exported_variable_statement);
            return result;
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // objectrestspread.go:423
    fn visit_variable_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        if self.in_exported_variable_statement.get() {
            self.in_exported_variable_statement.set(false);
            let result = self.visit_variable_declaration_worker(node, true);
            self.in_exported_variable_statement.set(true);
            return result;
        }
        self.visit_variable_declaration_worker(node, false)
    }

    // objectrestspread.go:433
    fn visit_variable_declaration_worker(&self, node: P<Node>, exported: bool) -> Option<P<Node>> {
        // If we are here it is because the name contains a binding pattern with a rest somewhere in it.
        if ast::is_binding_pattern(node.name().unwrap())
            && node.subtree_facts().intersects(SubtreeFacts::ContainsObjectRestOrSpread)
        {
            return flatten_destructuring_binding(
                &self.base,
                node,
                None,
                FlattenLevel::ObjectRest,
                exported,
                false,
            );
        }
        self.visitor().visit_each_child(Some(node))
    }

    // objectrestspread.go:445
    fn visit_for_oftatement(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let s = node.as_for_in_or_of_statement();
        if s.initializer.subtree_facts().intersects(SubtreeFacts::ContainsObjectRestOrSpread)
            || (ast::is_assignment_pattern(s.initializer)
                && ast::contains_object_rest_or_spread(s.initializer))
        {
            let initializer_without_parens = ast::skip_parentheses(s.initializer);
            if ast::is_variable_declaration_list(initializer_without_parens)
                || ast::is_assignment_pattern(initializer_without_parens)
            {
                let temp = f.new_temp_variable();
                let res = self.visitor().visit_node(Some(
                    f.create_for_of_binding_statement(initializer_without_parens, temp),
                ));
                let mut statements: Vec<P<Node>> = Vec::with_capacity(1);
                if let Some(res) = res {
                    statements.push(res);
                }
                // Go's `else if node.Statement != nil` always holds (the statement is never nil), so both locations are
                // always assigned.
                let body_location: TextRange;
                let statements_location: TextRange;
                if ast::is_block(s.statement) {
                    statements.extend(
                        s.statement.statements().iter().filter_map(|&statement| {
                            self.visitor().visit_each_child(Some(statement))
                        }),
                    );
                    body_location = s.statement.loc();
                    statements_location = s.statement.statement_list().unwrap().loc.get();
                } else {
                    statements.push(self.visitor().visit_each_child(Some(s.statement)).unwrap());
                    body_location = s.statement.loc();
                    statements_location = s.statement.loc();
                }

                let list = f.new_variable_declaration_list(
                    f.new_node_list(vec![f.new_variable_declaration(temp, None, None, None)]),
                    NodeFlags::Let,
                );
                list.set_loc(s.initializer.loc());

                let expr = self.visitor().visit_each_child(Some(s.expression)).unwrap();

                let statements_list = f.new_node_list(statements);
                statements_list.loc.set(statements_location);

                let block = f.new_block(statements_list, true);
                block.set_loc(body_location);

                return f.update_for_in_or_of_statement(node, s.await_modifier, list, expr, block);
            }
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // objectrestspread.go:498
    fn visit_binary_expression(&self, node: P<Node>, expression_result_is_unused: bool) -> P<Node> {
        let b = node.as_binary_expression();
        if ast::is_destructuring_assignment(node) && ast::contains_object_rest_or_spread(b.left) {
            return flatten_destructuring_assignment(
                &self.base,
                node,
                !expression_result_is_unused,
                FlattenLevel::ObjectRest,
                None,
            );
        }
        if b.operator_token.kind() == Kind::CommaToken {
            self.expression_result_is_unused.set(true);
            let left = self.visitor().visit_node(Some(b.left)).unwrap();
            self.expression_result_is_unused.set(expression_result_is_unused);
            let right = self.visitor().visit_node(Some(b.right.get())).unwrap();
            return self.factory().update_binary_expression(
                node,
                None,
                left,
                None,
                b.operator_token,
                right,
            );
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // objectrestspread.go:516
    fn visit_object_literal_expression(&self, node: P<Node>) -> P<Node> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsObjectRestOrSpread) {
            return self.visitor().visit_each_child(Some(node)).unwrap();
        }
        // spread elements emit like so:
        // non-spread elements are chunked together into object literals, and then all are passed to __assign:
        //     { a, ...o, b } => __assign(__assign({a}, o), {b});
        // If the first element is a spread element, then the first argument to __assign is {}:
        //     { ...o, a, b, ...o2 } => __assign(__assign(__assign({}, o), {a, b}), o2)
        //
        // We cannot call __assign with more than two elements, since any element could cause side effects. For
        // example:
        //      var k = { a: 1, b: 2 };
        //      var o = { a: 3, ...k, b: k.a++ };
        //      // expected: { a: 1, b: 1 }
        // If we translate the above to `__assign({ a: 3 }, k, { b: k.a++ })`, the `k.a++` will evaluate before
        // `k` is spread and we end up with `{ a: 2, b: 1 }`.
        //
        // This also occurs for spread elements, not just property assignments:
        //      var k = { a: 1, get b() { l = { z: 9 }; return 2; } };
        //      var l = { c: 3 };
        //      var o = { ...k, ...l };
        //      // expected: { a: 1, b: 2, z: 9 }
        // If we translate the above to `__assign({}, k, l)`, the `l` will evaluate before `k` is spread and we
        // end up with `{ a: 1, b: 2, c: 3 }`
        let f = self.factory();
        let mut objects = self
            .chunk_object_literal_elements(Some(node.as_object_literal_expression().properties));
        if !objects.is_empty() && objects[0].kind() != Kind::ObjectLiteralExpression {
            objects.insert(0, f.new_object_literal_expression(f.new_node_list(vec![]), false));
        }
        let mut expression = objects[0];
        if objects.len() > 1 {
            for (i, &obj) in objects.iter().enumerate() {
                if i == 0 {
                    continue;
                }
                expression = f.new_assign_helper(
                    vec![expression, obj],
                    self.compiler_options.get_emit_script_target(),
                );
            }
            return expression;
        }
        f.new_assign_helper(objects, self.compiler_options.get_emit_script_target())
    }

    // objectrestspread.go:559
    fn chunk_object_literal_elements(&self, list: Option<P<NodeList>>) -> Vec<P<Node>> {
        let Some(list) = list else {
            return Vec::new();
        };
        if list.nodes().is_empty() {
            return Vec::new();
        }
        let f = self.factory();
        let elements = list.nodes();
        let mut chunk_object: Vec<P<Node>> = Vec::new();
        let mut objects: Vec<P<Node>> = Vec::with_capacity(1);
        for &e in elements {
            if e.kind() == Kind::SpreadAssignment {
                if !chunk_object.is_empty() {
                    objects.push(f.new_object_literal_expression(
                        f.new_node_list(std::mem::take(&mut chunk_object)),
                        false,
                    ));
                }
                let target = e.expression().unwrap();
                objects.extend(self.visitor().visit_node(Some(target)));
            } else {
                let elem = if e.kind() == Kind::PropertyAssignment {
                    Some(f.new_property_assignment(
                        None,
                        e.name().unwrap(),
                        None,
                        None,
                        self.visitor().visit_node(e.initializer()).unwrap(),
                    ))
                } else {
                    self.visitor().visit_node(Some(e))
                };
                chunk_object.extend(elem);
            }
        }
        if !chunk_object.is_empty() {
            objects.push(f.new_object_literal_expression(f.new_node_list(chunk_object), false));
        }
        objects
    }
}

// objectrestspread.go:590
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_object_rest_spread_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let tx = P::new(ObjectRestSpreadTransformer {
        base: Transformer::default(),
        compiler_options: opt.compiler_options,
        in_exported_variable_statement: Cell::new(false),
        expression_result_is_unused: Cell::new(false),
        parameters_with_preceding_object_rest_or_spread: RefCell::new(None),
    });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(opt.context),
    ))
}
