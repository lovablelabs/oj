use super::*;

impl CommonJSModuleTransformer {
    // commonjsmodule.go:1340
    pub(crate) fn visit_for_statement(&self, node: P<Node>) -> P<Node> {
        let n = node.as_for_statement();
        self.factory().update_for_statement(
            node,
            self.discarded_value_visitor().visit_node(n.initializer),
            self.visitor().visit_node(n.condition),
            self.discarded_value_visitor().visit_node(n.incrementor),
            self.emit_context()
                .visit_iteration_body(Some(n.statement()), &mut self.visitor())
                .unwrap(),
        )
    }

    // commonjsmodule.go:1350
    pub(crate) fn visit_for_in_or_of_statement(&self, node: P<Node>) -> P<Node> {
        let n = node.as_for_in_or_of_statement();
        self.factory().update_for_in_or_of_statement(
            node,
            n.await_modifier,
            self.discarded_value_visitor().visit_node(Some(n.initializer)).unwrap(),
            self.visitor().visit_node(Some(n.expression)).unwrap(),
            self.emit_context()
                .visit_iteration_body(Some(n.statement()), &mut self.visitor())
                .unwrap(),
        )
    }

    // Visits an expression statement whose value will be discarded at runtime.
    // commonjsmodule.go:1361
    pub(crate) fn visit_expression_statement(&self, node: P<Node>) -> Option<P<Node>> {
        self.discarded_value_visitor().visit_each_child(Some(node))
    }

    // Visits a `void` expression whose value will be discarded at runtime.
    // commonjsmodule.go:1366
    pub(crate) fn visit_void_expression(&self, node: P<Node>) -> Option<P<Node>> {
        self.discarded_value_visitor().visit_each_child(Some(node))
    }

    // Visits a parenthesized expression whose value may be discarded at runtime.
    // commonjsmodule.go:1371
    pub(crate) fn visit_parenthesized_expression(
        &self,
        node: P<Node>,
        result_is_discarded: bool,
    ) -> P<Node> {
        let mut v =
            if result_is_discarded { self.discarded_value_visitor() } else { self.visitor() };
        let expression = v.visit_node(node.expression()).unwrap();
        self.factory().update_parenthesized_expression(node, expression)
    }

    // Visits a partially emitted expression whose value may be discarded at runtime.
    // commonjsmodule.go:1377
    pub(crate) fn visit_partially_emitted_expression(
        &self,
        node: P<Node>,
        result_is_discarded: bool,
    ) -> P<Node> {
        let mut v =
            if result_is_discarded { self.discarded_value_visitor() } else { self.visitor() };
        let expression = v.visit_node(node.expression()).unwrap();
        self.factory().update_partially_emitted_expression(node, expression)
    }

    // Visits a binary expression whose value may be discarded, or which might contain an assignment to an exported
    // identifier.
    // commonjsmodule.go:1384
    pub(crate) fn visit_binary_expression(
        &self,
        node: P<Node>,
        result_is_discarded: bool,
    ) -> P<Node> {
        if ast::is_destructuring_assignment(node) {
            return self.visit_destructuring_assignment(node, result_is_discarded);
        }

        if ast::is_assignment_expression(node, false /*excludeCompoundAssignment*/) {
            return self.visit_assignment_expression(node);
        }

        if ast::is_comma_expression(node) {
            return self.visit_comma_expression(node, result_is_discarded);
        }

        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // commonjsmodule.go:1400
    fn visit_assignment_expression(&self, node: P<Node>) -> P<Node> {
        // When we see an assignment expression whose left-hand side is an exported symbol,
        // we should ensure all exports of that symbol are updated with the correct value.
        //
        // - We do not transform generated identifiers unless they are file-level reserved names.
        // - We do not transform identifiers tagged with the LocalName flag.
        // - We only transform identifiers that are exported at the top level.
        let emit_context = self.emit_context();
        let left = node.as_binary_expression().left;
        if ast::is_identifier(left)
            && (!is_generated_identifier(emit_context, left)
                || is_file_level_reserved_generated_identifier(emit_context, left))
            && !is_local_name(emit_context, left)
        {
            let exported_names = self.get_exports(left);
            if !exported_names.is_empty() {
                // For each additional export of the declaration, apply an export assignment.
                let mut expression = self.visitor().visit_each_child(Some(node)).unwrap();
                expression =
                    exported_names.into_iter().fold(expression, |mut expression, export_name| {
                        expression = self.create_export_expression(
                            export_name,
                            expression,
                            Some(node.loc()), /*location*/
                            false,            /*liveBinding*/
                        );

                        expression
                    });
                return expression;
            }
        }

        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // Visits a destructuring assignment which might target an exported identifier.
    // commonjsmodule.go:1425
    pub(crate) fn visit_destructuring_assignment(
        &self,
        node: P<Node>,
        value_is_discarded: bool,
    ) -> P<Node> {
        if self.destructuring_needs_flattening(node.as_binary_expression().left) {
            let callback = |name: P<Node>, value: P<Node>, location: Option<&TextRange>| {
                self.create_all_export_expressions(name, value, location)
            };
            return flatten_destructuring_assignment(
                &self.base,
                node,
                !value_is_discarded, /*needsValue*/
                FlattenLevel::All,
                Some(&callback),
            );
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // destructuringNeedsFlattening checks whether a destructuring assignment target contains any
    // exported identifiers that need to be flattened into individual export assignments.
    // commonjsmodule.go:1440
    fn destructuring_needs_flattening(&self, node: P<Node>) -> bool {
        if ast::is_object_literal_expression(node) {
            if let Some(result) = node.properties().iter().find_map(|&elem| {
                match elem.kind() {
                    Kind::PropertyAssignment => {
                        if self.destructuring_needs_flattening(elem.initializer().unwrap()) {
                            return Some(true);
                        }
                    }
                    Kind::ShorthandPropertyAssignment => {
                        if self.destructuring_needs_flattening(elem.name().unwrap()) {
                            return Some(true);
                        }
                    }
                    Kind::SpreadAssignment => {
                        if self.destructuring_needs_flattening(elem.expression().unwrap()) {
                            return Some(true);
                        }
                    }
                    Kind::MethodDeclaration | Kind::GetAccessor | Kind::SetAccessor => {
                        return Some(false);
                    }
                    _ => {}
                }

                None
            }) {
                return result;
            }
        } else if ast::is_array_literal_expression(node) {
            if node.as_array_literal_expression().elements.nodes().iter().any(|&elem| {
                if ast::is_spread_element(elem) {
                    if self.destructuring_needs_flattening(elem.expression().unwrap()) {
                        return true;
                    }
                } else if self.destructuring_needs_flattening(elem) {
                    return true;
                }

                false
            }) {
                return true;
            }
        } else if ast::is_identifier(node) {
            let exported_names = self.get_exports(node);
            if is_export_name(self.emit_context(), node) {
                // The identifier is already wrapped to be an export reference; tolerate up to one
                // matching export.
                return exported_names.len() > 1;
            }
            if exported_names.is_empty() {
                return false;
            }
            // A single direct export whose export name matches the identifier text can be handled
            // natively: substitution will rewrite the identifier to `exports.X`, so no flattening
            // is needed. Re-aliased exports (where the export name differs from the local name) or
            // multi-exported names cannot be expressed natively in a destructuring assignment.
            if exported_names.len() == 1
                && self.is_direct_export(node)
                && exported_names[0].text() == node.text()
            {
                return false;
            }
            return true;
        }
        false
    }

    // createAllExportExpressions is the callback used during destructuring flattening to create
    // export expressions for each exported identifier binding.
    // commonjsmodule.go:1500
    fn create_all_export_expressions(
        &self,
        name: P<Node>,
        value: P<Node>,
        location: Option<&TextRange>,
    ) -> P<Node> {
        let f = self.factory();
        let emit_context = self.emit_context();
        let exported_names = self.get_exports(name);
        if !exported_names.is_empty() {
            // If the name is directly exported (i.e., `export let x`), assign to exports.name directly.
            // Otherwise, assign to the local binding first (i.e., `let x; export { x }`).
            let mut expression;
            if self.is_direct_export(name) {
                // Create exports.name = value to handle the direct export assignment,
                // since the Go port doesn't have an onSubstituteNode mechanism to rewrite identifiers.
                let export_name = name.clone_node(f);
                emit_context
                    .add_emit_flags(export_name, EmitFlags::NoComments | EmitFlags::NoSourceMap);
                let property_access = f.new_property_access_expression(
                    f.new_identifier("exports"),
                    None, /*questionDotToken*/
                    export_name,
                    NodeFlags::None,
                );
                emit_context.add_emit_flags(property_access, EmitFlags::NoComments);
                expression = f.new_assignment_expression(property_access, value);
                emit_context.assign_comment_and_source_map_ranges(expression, name);
            } else {
                expression = f.new_assignment_expression(name, value);
            }
            expression =
                exported_names.into_iter().fold(expression, |mut expression, export_name| {
                    expression = self.create_export_expression(
                        export_name,
                        expression,
                        location.copied(),
                        false, /*liveBinding*/
                    );

                    expression
                });
            return expression;
        }
        // If the identifier is directly exported but has no additional export aliases,
        // still write to exports.name.
        if self.is_direct_export(name) {
            let export_name = name.clone_node(f);
            emit_context
                .add_emit_flags(export_name, EmitFlags::NoComments | EmitFlags::NoSourceMap);
            let property_access = f.new_property_access_expression(
                f.new_identifier("exports"),
                None, /*questionDotToken*/
                export_name,
                NodeFlags::None,
            );
            emit_context.add_emit_flags(property_access, EmitFlags::NoComments);
            let result = f.new_assignment_expression(property_access, value);
            emit_context.assign_comment_and_source_map_ranges(result, name);
            return result;
        }
        f.new_assignment_expression(name, value)
    }

    // isDirectExport checks whether the identifier is directly exported from the source file
    // (e.g., `export let x` or `export function f()`), as opposed to being re-exported via
    // `export { x }` for a locally-declared variable.
    // commonjsmodule.go:1550
    fn is_direct_export(&self, name: P<Node>) -> bool {
        let export_container = self.resolver.get_referenced_export_container(
            self.emit_context().most_original(Some(name)).unwrap(),
            false, /*prefixLocals*/
        );
        export_container.is_some_and(ast::is_source_file)
    }

    // commonjsmodule.go:1555
    pub(crate) fn visit_assignment_property(&self, node: P<Node>) -> P<Node> {
        let mut v = self.visitor();
        self.factory().update_property_assignment(
            node,
            None, /*modifiers*/
            v.visit_node(node.name()).unwrap(),
            None, /*postfixToken*/
            None, /*typeNode*/
            self.assignment_pattern_visitor().visit_node(node.initializer()).unwrap(),
        )
    }

    // commonjsmodule.go:1566
    pub(crate) fn visit_shorthand_assignment_property(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let n = node.as_shorthand_property_assignment();
        let mut target = self.visit_destructuring_assignment_target_no_stack(node.name().unwrap());
        if ast::is_identifier(target) {
            return f.update_shorthand_property_assignment(
                node,
                None, /*modifiers*/
                target,
                None, /*postfixToken*/
                None, /*typeNode*/
                n.equals_token,
                self.visitor().visit_node(n.object_assignment_initializer()),
            );
        }
        if let Some(object_assignment_initializer) = n.object_assignment_initializer() {
            let equals_token = n.equals_token.unwrap_or_else(|| f.new_token(Kind::EqualsToken));
            target = f.new_binary_expression(
                None, /*modifiers*/
                target,
                None, /*typeNode*/
                equals_token,
                self.visitor().visit_node(Some(object_assignment_initializer)).unwrap(),
            );
        }
        let updated = f.new_property_assignment(
            None, /*modifiers*/
            node.name().unwrap(),
            None, /*postfixToken*/
            None, /*typeNode*/
            target,
        );
        self.emit_context().set_original(updated, node);
        self.emit_context().assign_comment_and_source_map_ranges(updated, node);
        updated
    }

    // commonjsmodule.go:1608
    pub(crate) fn visit_assignment_rest_property(&self, node: P<Node>) -> P<Node> {
        self.factory().update_spread_assignment(
            node,
            self.visit_destructuring_assignment_target(node.expression().unwrap()),
        )
    }

    // commonjsmodule.go:1615
    pub(crate) fn visit_assignment_rest_element(&self, node: P<Node>) -> P<Node> {
        self.factory().update_spread_element(
            node,
            self.visit_destructuring_assignment_target(node.expression().unwrap()),
        )
    }

    // commonjsmodule.go:1622
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    pub(crate) fn visit_assignment_element(&self, node: P<Node>) -> Option<P<Node>> {
        if ast::is_binary_expression(node) {
            let n = node.as_binary_expression();
            if n.operator_token.kind() == Kind::EqualsToken {
                let left = self.visit_destructuring_assignment_target(n.left);
                let right = self.visitor().visit_node(Some(n.right())).unwrap();
                return Some(self.factory().update_binary_expression(
                    node,
                    None, /*modifiers*/
                    left,
                    None, /*typeNode*/
                    n.operator_token,
                    right,
                ));
            }
        }

        Some(self.visit_destructuring_assignment_target_no_stack(node))
    }

    // commonjsmodule.go:1640
    fn visit_destructuring_assignment_target(&self, node: P<Node>) -> P<Node> {
        let grandparent_node = self.push_node(node);
        let result = match node.kind() {
            Kind::ObjectLiteralExpression | Kind::ArrayLiteralExpression => {
                self.visit_assignment_pattern_no_stack(node).unwrap()
            }
            _ => self.visit_destructuring_assignment_target_no_stack(node),
        };
        self.pop_node(grandparent_node);
        result
    }

    // commonjsmodule.go:1653
    fn visit_destructuring_assignment_target_no_stack(&self, node: P<Node>) -> P<Node> {
        let emit_context = self.emit_context();
        if ast::is_identifier(node)
            && (!is_generated_identifier(emit_context, node)
                || is_file_level_reserved_generated_identifier(emit_context, node))
            && !is_local_name(emit_context, node)
        {
            let f = self.factory();
            let mut expression = self.visit_expression_identifier(node);
            let exported_names = self.get_exports(node);
            if !exported_names.is_empty() {
                // transforms:
                //  var x;
                //  export { x }
                //  { x: x } = y
                // to:
                //  { x: { set value(v) { exports.x = x = v; } }.value } = y

                let value = f.new_unique_name_ex(
                    "value",
                    AutoGenerateOptions {
                        flags: GeneratedIdentifierFlags::Optimistic,
                        ..Default::default()
                    },
                );
                expression = f.new_assignment_expression(expression, value);

                expression =
                    exported_names.into_iter().fold(expression, |mut expression, export_name| {
                        expression = self.create_export_expression(
                            export_name,
                            expression,
                            None,  /*location*/
                            false, /*liveBinding*/
                        );

                        expression
                    });

                let statement = f.new_expression_statement(expression);
                let statement_list = f.new_node_list(vec![statement]);
                let param = f.new_parameter_declaration(
                    None, /*modifiers*/
                    None, /*dotDotDotToken*/
                    value, None, /*questionToken*/
                    None, /*type*/
                    None, /*initializer*/
                );
                let value_setter = f.new_set_accessor_declaration(
                    None, /*modifiers*/
                    f.new_identifier("value"),
                    None, /*typeParameters*/
                    Some(f.new_node_list(vec![param])),
                    None, /*returnType*/
                    None, /*fullSignature*/
                    Some(f.new_block(statement_list, false /*multiLine*/)),
                );
                let property_list = f.new_node_list(vec![value_setter]);
                expression =
                    f.new_object_literal_expression(property_list, false /*multiLine*/);
                expression = f.new_property_access_expression(
                    expression,
                    None, /*questionDotToken*/
                    f.new_identifier("value"),
                    NodeFlags::None,
                );
            }
            return expression;
        }

        self.visit_no_stack(node, false /*resultIsDiscarded*/).unwrap()
    }

    // Visits a comma expression whose left-hand value is always discard, and whose right-hand value may be discarded at runtime.
    // commonjsmodule.go:1702
    fn visit_comma_expression(&self, node: P<Node>, result_is_discarded: bool) -> P<Node> {
        let n = node.as_binary_expression();
        let left = self.discarded_value_visitor().visit_node(Some(n.left)).unwrap();
        let mut v =
            if result_is_discarded { self.discarded_value_visitor() } else { self.visitor() };
        let right = v.visit_node(Some(n.right())).unwrap();
        self.factory().update_binary_expression(
            node,
            None, /*modifiers*/
            left,
            None, /*typeNode*/
            n.operator_token,
            right,
        )
    }

    // Visits a prefix unary expression that might modify an exported identifier.
    // commonjsmodule.go:1709
    pub(crate) fn visit_prefix_unary_expression(
        &self,
        node: P<Node>,
        _result_is_discarded: bool,
    ) -> P<Node> {
        // When we see a prefix increment expression whose operand is an exported
        // symbol, we should ensure all exports of that symbol are updated with the correct
        // value.
        //
        // - We do not transform generated identifiers for any reason.
        // - We do not transform identifiers tagged with the LocalName flag.
        // - We do not transform identifiers that were originally the name of an enum or
        //   namespace due to how they are transformed in TypeScript.
        // - We only transform identifiers that are exported at the top level.
        let n = node.as_prefix_unary_expression();
        if (n.operator == Kind::PlusPlusToken || n.operator == Kind::MinusMinusToken)
            && ast::is_identifier(n.operand)
            && !is_local_name(self.emit_context(), n.operand)
        {
            let exported_names = self.get_exports(n.operand);
            if !exported_names.is_empty() {
                // given:
                //   var x = 0;
                //   export { x }
                //   ++x;
                // emits:
                //   var x = 0;
                //   exports.x = x;
                //   exports.x = ++x;
                // note:
                //   after the operation, `exports.x` will hold the value of `x` after the increment.

                let mut expression = self.factory().update_prefix_unary_expression(
                    node,
                    n.operator,
                    self.visitor().visit_node(Some(n.operand)).unwrap(),
                );
                for export_name in exported_names {
                    expression = self.create_export_expression(
                        export_name,
                        expression,
                        None,  /*location*/
                        false, /*liveBinding*/
                    );
                    self.emit_context().assign_comment_and_source_map_ranges(expression, node);
                }
                return expression;
            }
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // Visits a postfix unary expression that might modify an exported identifier.
    // commonjsmodule.go:1746
    pub(crate) fn visit_postfix_unary_expression(
        &self,
        node: P<Node>,
        result_is_discarded: bool,
    ) -> P<Node> {
        // When we see a postfix increment expression whose operand is an exported
        // symbol, we should ensure all exports of that symbol are updated with the correct
        // value.
        //
        // - We do not transform generated identifiers for any reason.
        // - We do not transform identifiers tagged with the LocalName flag.
        // - We do not transform identifiers that were originally the name of an enum or
        //   namespace due to how they are transformed in TypeScript.
        // - We only transform identifiers that are exported at the top level.
        let n = node.as_postfix_unary_expression();
        let emit_context = self.emit_context();
        if (n.operator == Kind::PlusPlusToken || n.operator == Kind::MinusMinusToken)
            && ast::is_identifier(n.operand)
            && !is_local_name(emit_context, n.operand)
        {
            let exported_names = self.get_exports(n.operand);
            if !exported_names.is_empty() {
                // given (value is discarded):
                //   var x = 0;
                //   export { x }
                //   x++;
                // emits:
                //   var x = 0, y;
                //   exports.x = x;
                //   exports.x = (x++, x);
                // note:
                //   after the operation, `exports.x` will hold the value of `x` after the increment.
                //
                // given (value is not discarded):
                //   var x = 0, y;
                //   export { x }
                //   y = x++;
                // emits:
                //   var _a;
                //   var x = 0, y;
                //   exports.x = x;
                //   y = (exports.x = (_a = x++, x), _a);
                // note:
                //   after the operation, `exports.x` will hold the value of `x` after the increment, while
                //   `y` will hold the value of `x` before the increment.

                let f = self.factory();
                let mut temp: Option<P<Node>> = None;
                let mut expression = f.update_postfix_unary_expression(
                    node,
                    self.visitor().visit_node(Some(n.operand)).unwrap(),
                    n.operator,
                );
                if !result_is_discarded {
                    let t = f.new_temp_variable();
                    emit_context.add_variable_declaration(t);
                    temp = Some(t);

                    expression = f.new_assignment_expression(t, expression);
                    emit_context.assign_comment_and_source_map_ranges(expression, node);
                }

                expression = f.new_comma_expression(expression, n.operand.clone_node(f));
                emit_context.assign_comment_and_source_map_ranges(expression, node);

                for export_name in exported_names {
                    expression = self.create_export_expression(
                        export_name,
                        expression,
                        None,  /*location*/
                        false, /*liveBinding*/
                    );
                    emit_context.assign_comment_and_source_map_ranges(expression, node);
                }

                if let Some(temp) = temp {
                    expression = f.new_comma_expression(expression, temp);
                    emit_context.assign_comment_and_source_map_ranges(expression, node);
                }

                return expression;
            }
        }

        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // Visits a call expression that might reference an imported symbol and thus require an indirect call, or that might
    // be an `import()` or `require()` call that may need to be rewritten.
    // commonjsmodule.go:1826
    pub(crate) fn visit_call_expression(&self, node: P<Node>) -> P<Node> {
        let mut needs_rewrite = false;
        if self.compiler_options.rewrite_relative_import_extensions.is_true()
            && (ast::is_import_call(node) && !node.arguments().is_empty()
                || ast::is_in_js_file(node)
                    && ast::is_require_call(node, false /*requireStringLiteralLikeArgument*/))
        {
            needs_rewrite = true;
        }
        if node.expression().unwrap().kind() == Kind::ImportKeyword
            && self.should_transform_import_call()
        {
            return self.visit_import_call_expression(node, needs_rewrite);
        }
        if needs_rewrite {
            return self.shim_or_rewrite_import_or_require_call(node);
        }
        let n = node.as_call_expression();
        let callee = node.expression().unwrap();
        if ast::is_identifier(callee) {
            // given:
            //   import { f } from "mod";
            //   f();
            // emits:
            //   const mod_1 = require("mod");
            //   (0, mod_1.f)();
            // note:
            //   the indirect call is applied by the printer by way of the `EFIndirectCall` emit flag.
            let expression = self.visit_expression_identifier(callee);
            let updated = self.factory().update_call_expression(
                node,
                expression,
                n.question_dot_token(),
                None, /*typeArguments*/
                self.visitor().visit_nodes(Some(n.arguments)).unwrap(),
                node.flags(),
            );
            if !ast::is_identifier(expression) && !is_helper_name(self.emit_context(), callee) {
                self.emit_context().add_emit_flags(updated, EmitFlags::IndirectCall);
            }
            return updated;
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // commonjsmodule.go:1868
    fn should_transform_import_call(&self) -> bool {
        let current_source_file = self.current_source_file.get().unwrap();
        ast::should_transform_import_call(
            current_source_file.file_name(),
            &self.compiler_options,
            (self.get_emit_module_format_of_file)(current_source_file),
        )
    }

    // commonjsmodule.go:1872
    fn visit_import_call_expression(&self, node: P<Node>, rewrite_or_shim: bool) -> P<Node> {
        if self.module_kind == ModuleKind::NONE && self.language_version >= ScriptTarget::ES2020 {
            return self.visitor().visit_each_child(Some(node)).unwrap();
        }

        let f = self.factory();
        let external_module_name = get_external_module_name_literal(
            f,
            node,
            self.current_source_file.get(),
            None, /*host*/
            None, /*resolver*/
            &self.compiler_options,
        );
        let first_argument = self.visitor().visit_node(node.arguments().first().copied());

        // Only use the external module name if it differs from the first argument. This allows us to preserve the quote style of the argument on output.
        let argument = if external_module_name.is_some()
            && first_argument.is_none_or(|a| {
                !ast::is_string_literal(a) || a.text() != external_module_name.unwrap().text()
            }) {
            external_module_name
        } else if first_argument.is_some() && rewrite_or_shim {
            let first_argument = first_argument.unwrap();
            if ast::is_string_literal(first_argument) {
                rewrite_module_specifier(
                    self.emit_context(),
                    Some(first_argument),
                    &self.compiler_options,
                    &self.content_mapper_extension_rewrites,
                    self.use_case_sensitive_file_names,
                )
            } else {
                Some(f.new_rewrite_relative_import_extensions_helper(
                    first_argument,
                    self.compiler_options.jsx == tsrs_core::JsxEmit::PRESERVE,
                    &self.content_mapper_extension_rewrites,
                    self.use_case_sensitive_file_names,
                ))
            }
        } else {
            first_argument
        };
        self.create_import_call_expression_common_js(argument)
    }

    // commonjsmodule.go:1898
    fn create_import_call_expression_common_js(&self, arg: Option<P<Node>>) -> P<Node> {
        // import(x)
        // emit as
        // Promise.resolve(`${x}`).then((s) => require(s)) /*CommonJS Require*/
        // We have to wrap require in then callback so that require is done in asynchronously
        // if we simply do require in resolve callback in Promise constructor. We will execute the loading immediately
        // If the arg is not inlineable, we have to evaluate and ToString() it in the current scope
        // Otherwise, we inline it in require() so that it's statically analyzable
        let f = self.factory();

        let need_sync_eval =
            arg.is_some_and(|arg| !super::utilities::is_simple_inlineable_expression(arg));

        let mut promise_resolve_arguments: Vec<P<Node>> = Vec::new();
        if need_sync_eval {
            promise_resolve_arguments = vec![f.new_template_expression(
                f.new_template_head("", "", TokenFlags::None),
                f.new_node_list(vec![f.new_template_span(
                    arg.unwrap(),
                    f.new_template_tail("", "", TokenFlags::None),
                )]),
            )];
        }
        let promise_resolve_call = f.new_call_expression(
            f.new_property_access_expression(
                f.new_identifier("Promise"),
                None, /*questionDotToken*/
                f.new_identifier("resolve"),
                NodeFlags::None,
            ),
            None, /*questionDotToken*/
            None, /*typeArguments*/
            f.new_node_list(promise_resolve_arguments),
            NodeFlags::None,
        );

        let mut require_arguments: Vec<P<Node>> = Vec::new();
        if need_sync_eval {
            require_arguments = vec![f.new_identifier("s")];
        } else if let Some(arg) = arg {
            require_arguments = vec![arg];
        }

        let require_call = f.new_import_star_helper(f.new_call_expression(
            f.new_identifier("require"),
            None, /*questionDotToken*/
            None, /*typeArguments*/
            f.new_node_list(require_arguments),
            NodeFlags::None,
        ));

        let mut parameters: Vec<P<Node>> = Vec::new();
        if need_sync_eval {
            parameters = vec![f.new_parameter_declaration(
                None, /*modifiers*/
                None, /*dotDotDotToken*/
                f.new_identifier("s"),
                None, /*questionToken*/
                None, /*type*/
                None, /*initializer*/
            )];
        }

        let function = f.new_arrow_function(
            None, /*modifiers*/
            None, /*typeParameters*/
            Some(f.new_node_list(parameters)),
            None,                                            /*type*/
            None,                                            /*fullSignature*/
            Some(f.new_token(Kind::EqualsGreaterThanToken)), /*equalsGreaterThanToken*/
            Some(require_call),
        );

        f.new_call_expression(
            f.new_property_access_expression(
                promise_resolve_call,
                None, /*questionDotToken*/
                f.new_identifier("then"),
                NodeFlags::None,
            ),
            None, /*questionDotToken*/
            None, /*typeArguments*/
            f.new_node_list(vec![function]),
            NodeFlags::None,
        )
    }

    // commonjsmodule.go:1993
    fn shim_or_rewrite_import_or_require_call(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let n = node.as_call_expression();
        let mut v = self.visitor();
        let expression = v.visit_node(node.expression()).unwrap();
        let mut arguments_list = n.arguments;
        if !n.arguments.nodes().is_empty() {
            let mut first_argument = v.visit_node(Some(n.arguments.nodes()[0])).unwrap();
            let first_argument_changed;
            if ast::is_string_literal_like(first_argument) {
                let rewritten = rewrite_module_specifier(
                    self.emit_context(),
                    Some(first_argument),
                    &self.compiler_options,
                    &self.content_mapper_extension_rewrites,
                    self.use_case_sensitive_file_names,
                )
                .unwrap();
                first_argument_changed = rewritten != first_argument;
                first_argument = rewritten;
            } else {
                first_argument = f.new_rewrite_relative_import_extensions_helper(
                    first_argument,
                    self.compiler_options.jsx == tsrs_core::JsxEmit::PRESERVE,
                    &self.content_mapper_extension_rewrites,
                    self.use_case_sensitive_file_names,
                );
                first_argument_changed = true;
            }

            let (rest, rest_changed) = v.visit_slice(&n.arguments.nodes()[1..]);
            if first_argument_changed || rest_changed {
                let mut arguments = vec![first_argument];
                arguments.extend_from_slice(rest);
                arguments_list = f.new_node_list(arguments);
                arguments_list.loc.set(n.arguments.loc.get());
            }
        }

        f.update_call_expression(
            node,
            expression,
            n.question_dot_token(),
            None, /*typeArguments*/
            arguments_list,
            node.flags(),
        )
    }

    // Visits a tagged template expression that might reference an imported symbol and thus require an indirect call.
    // commonjsmodule.go:2030
    pub(crate) fn visit_tagged_template_expression(&self, node: P<Node>) -> P<Node> {
        let n = node.as_tagged_template_expression();
        if ast::is_identifier(n.tag) {
            // given:
            //   import { f } from "mod";
            //   f``;
            // emits:
            //   const mod_1 = require("mod");
            //   (0, mod_1.f) ``;
            // note:
            //   the indirect call is applied by the printer by way of the `EFIndirectCall` emit flag.

            let expression = self.visit_expression_identifier(n.tag);
            let updated = self.factory().update_tagged_template_expression(
                node,
                expression,
                None, /*questionDotToken*/
                None, /*typeArguments*/
                self.visitor().visit_node(Some(n.template)).unwrap(),
                node.flags(),
            );
            if !ast::is_identifier(expression) && !is_helper_name(self.emit_context(), n.tag) {
                self.emit_context().add_emit_flags(updated, EmitFlags::IndirectCall);
            }
            return updated;
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // Visits a shorthand property assignment that might reference an imported or exported symbol.
    // commonjsmodule.go:2058
    pub(crate) fn visit_shorthand_property_assignment(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let n = node.as_shorthand_property_assignment();
        let name = node.name().unwrap();
        let exported_or_imported_name = self.visit_expression_identifier(name);
        if exported_or_imported_name != name {
            // A shorthand property with an assignment initializer is probably part of a
            // destructuring assignment
            let mut expression = exported_or_imported_name;
            if let Some(object_assignment_initializer) = n.object_assignment_initializer() {
                expression = f.new_assignment_expression(
                    expression,
                    self.visitor().visit_node(Some(object_assignment_initializer)).unwrap(),
                );
            }
            let assignment = f.new_property_assignment(
                None, /*modifiers*/
                name, None, /*postfixToken*/
                None, /*typeNode*/
                expression,
            );
            assignment.set_loc(node.loc());
            self.emit_context().assign_comment_and_source_map_ranges(assignment, node);
            return assignment;
        }
        f.update_shorthand_property_assignment(
            node,
            None, /*modifiers*/
            exported_or_imported_name,
            None, /*postfixToken*/
            None, /*typeNode*/
            n.equals_token,
            self.visitor().visit_node(n.object_assignment_initializer()),
        )
    }

    // Visits an identifier that, if it is in an expression position, might reference an imported or exported symbol.
    // commonjsmodule.go:2089
    pub(crate) fn visit_identifier(&self, node: P<Node>) -> P<Node> {
        if is_identifier_reference(node, self.parent_node.get().unwrap()) {
            return self.visit_expression_identifier(node);
        }
        node
    }

    // Visits an identifier in an expression position that might reference an imported or exported symbol.
    // commonjsmodule.go:2097
    pub(crate) fn visit_expression_identifier(&self, node: P<Node>) -> P<Node> {
        let emit_context = self.emit_context();
        let info = emit_context.get_auto_generate_info(Some(node));
        if info.is_none_or(|info| info.flags.has_allow_name_substitution())
            && !is_helper_name(emit_context, node)
            && !is_local_name(emit_context, node)
            && !is_declaration_name_of_enum_or_namespace(emit_context, node)
        {
            let f = self.factory();
            let original = emit_context.most_original(Some(node)).unwrap();
            let export_container = self
                .resolver
                .get_referenced_export_container(original, is_export_name(emit_context, node));
            if export_container.is_some_and(ast::is_source_file) {
                let reference = f.new_property_access_expression(
                    f.new_identifier("exports"),
                    None, /*questionDotToken*/
                    node.clone_node(f),
                    NodeFlags::None,
                );
                emit_context.assign_comment_and_source_map_ranges(reference, node);
                reference.set_loc(node.loc());
                return reference;
            }

            let import_declaration = self.resolver.get_referenced_import_declaration(original);
            if let Some(import_declaration) = import_declaration {
                if ast::is_import_clause(import_declaration) {
                    let reference = f.new_property_access_expression(
                        f.new_generated_name_for_node(import_declaration.parent().unwrap()),
                        None, /*questionDotToken*/
                        f.new_identifier("default"),
                        NodeFlags::None,
                    );
                    emit_context.assign_comment_and_source_map_ranges(reference, node);
                    reference.set_loc(node.loc());
                    return reference;
                }
                if ast::is_import_specifier(import_declaration) {
                    let name = import_declaration.property_name_or_name().unwrap();
                    let decl = ast::find_ancestor(import_declaration, ast::is_import_declaration);
                    let target = f.new_generated_name_for_node(decl.unwrap_or(import_declaration));
                    let reference = if ast::is_string_literal(name) {
                        f.new_element_access_expression(
                            target,
                            None, /*questionDotToken*/
                            f.new_string_literal_from_node(name),
                            NodeFlags::None,
                        )
                    } else {
                        let reference_name = name.clone_node(f);
                        emit_context.add_emit_flags(
                            reference_name,
                            EmitFlags::NoSourceMap | EmitFlags::NoComments,
                        );
                        f.new_property_access_expression(
                            target,
                            None, /*questionDotToken*/
                            reference_name,
                            NodeFlags::None,
                        )
                    };
                    emit_context.assign_comment_and_source_map_ranges(reference, node);
                    reference.set_loc(node.loc());
                    return reference;
                }
            }
        }
        node
    }

    // Gets the exported names of an identifier, if it is exported.
    // commonjsmodule.go:2156
    fn get_exports(&self, name: P<Node>) -> Vec<P<Node>> {
        let emit_context = self.emit_context();
        let info = self.module_info();
        if !is_generated_identifier(emit_context, name) {
            let original = emit_context.most_original(Some(name)).unwrap();
            if let Some(import_declaration) =
                self.resolver.get_referenced_import_declaration(original)
            {
                return info.exported_bindings.get(&import_declaration).to_vec();
            }

            // An exported namespace or enum may merge with an ambient declaration, which won't show up in .js emit, so
            // we analyze all value exports of a symbol.
            let mut bindings_set: FxHashSet<P<Node>> = FxHashSet::default();
            let mut bindings: Vec<P<Node>> = Vec::new();
            let declarations = self.resolver.get_referenced_value_declarations(original);
            for declaration in declarations {
                bindings.extend(info.exported_bindings.get(&declaration).iter().filter_map(
                    |&binding| {
                        if bindings_set.insert(binding) { Some(binding) } else { None }
                    },
                ));
            }
            return bindings;
        } else if is_file_level_reserved_generated_identifier(emit_context, name) {
            let export_specifiers = info.export_specifiers.get(&name.text());
            return export_specifiers.iter().map(|s| s.name().unwrap()).collect();
        }
        Vec::new()
    }
}
