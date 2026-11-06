use super::*;

impl CommonJSModuleTransformer {
    // Appends the down-level representation of an export to a statement list, returning the statement list.
    // commonjsmodule.go:560
    pub(crate) fn append_export_statement(
        &self,
        statements: &mut Vec<P<Node>>,
        seen: &mut Set<&'static str>,
        export_name: P<Node>,
        expression: P<Node>,
        location: Option<TextRange>,
        allow_comments: bool,
        live_binding: bool,
    ) {
        if export_name.kind() != Kind::StringLiteral {
            if seen.has(&export_name.text()) {
                return;
            }
            seen.add(export_name.text());
        }
        statements.push(self.create_export_statement(
            export_name,
            expression,
            location,
            allow_comments,
            live_binding,
        ));
    }

    // Creates a call to the current file's export function to export a value.
    // commonjsmodule.go:577
    fn create_export_statement(
        &self,
        name: P<Node>,
        value: P<Node>,
        location: Option<TextRange>,
        allow_comments: bool,
        live_binding: bool,
    ) -> P<Node> {
        let emit_context = self.emit_context();
        let statement = self.factory().new_expression_statement(self.create_export_expression(
            name,
            value,
            None, /*location*/
            live_binding,
        ));
        if let Some(location) = location {
            emit_context.set_comment_range(statement, location);
        }
        emit_context.add_emit_flags(statement, EmitFlags::StartOnNewLine);
        if !allow_comments {
            emit_context.add_emit_flags(statement, EmitFlags::NoComments);
        }
        statement
    }

    // Creates a call to the current file's export function to export a value.
    // commonjsmodule.go:595
    pub(crate) fn create_export_expression(
        &self,
        name: P<Node>,
        value: P<Node>,
        location: Option<TextRange>,
        live_binding: bool,
    ) -> P<Node> {
        let f = self.factory();
        let expression = if live_binding {
            // For a live binding we emit a getter on `exports` that returns the value:
            //  Object.defineProperty(exports, "<name>", { enumerable: true, get: function () { return <value>; } });
            f.new_call_expression(
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
                    f.new_string_literal_from_node(name),
                    f.new_object_literal_expression(
                        f.new_node_list(vec![
                            f.new_property_assignment(
                                None, /*modifiers*/
                                f.new_identifier("enumerable"),
                                None, /*postfixToken*/
                                None, /*typeNode*/
                                f.new_true_expression(),
                            ),
                            f.new_property_assignment(
                                None, /*modifiers*/
                                f.new_identifier("get"),
                                None, /*postfixToken*/
                                None, /*typeNode*/
                                f.new_function_expression(
                                    None, /*modifiers*/
                                    None, /*asteriskToken*/
                                    None, /*name*/
                                    None, /*typeParameters*/
                                    Some(f.new_node_list(Vec::new())),
                                    None, /*type*/
                                    None, /*fullSignature*/
                                    Some(f.new_block(
                                        f.new_node_list(vec![f.new_return_statement(Some(value))]),
                                        false, /*multiLine*/
                                    )),
                                ),
                            ),
                        ]),
                        false, /*multiLine*/
                    ),
                ]),
                NodeFlags::None,
            )
        } else {
            // Otherwise, we emit a simple property assignment.
            let left = if name.kind() == Kind::StringLiteral {
                // emits:
                //  exports["<name>"] = <value>;
                f.new_element_access_expression(
                    f.new_identifier("exports"),
                    None, /*questionDotToken*/
                    f.new_string_literal_from_node(name),
                    NodeFlags::None,
                )
            } else {
                // emits:
                //  exports.<name> = <value>;
                f.new_property_access_expression(
                    f.new_identifier("exports"),
                    None, /*questionDotToken*/
                    name.clone_node(f),
                    NodeFlags::None,
                )
            };
            f.new_assignment_expression(left, value)
        };
        if let Some(location) = location {
            self.emit_context().set_comment_range(expression, location);
        }
        expression
    }

    // Creates a `require()` call to import an external module.
    // commonjsmodule.go:680
    fn create_require_call(
        &self,
        node: P<Node>, /*ImportDeclaration | ImportEqualsDeclaration | ExportDeclaration*/
    ) -> P<Node> {
        let f = self.factory();
        let mut args: Vec<P<Node>> = Vec::new();
        let module_name = get_external_module_name_literal(
            f,
            node,
            self.current_source_file.get(),
            None, /*host*/
            None, /*resolver*/
            &self.compiler_options,
        );
        if let Some(module_name) = module_name {
            args.push(
                rewrite_module_specifier(
                    self.emit_context(),
                    Some(module_name),
                    &self.compiler_options,
                    &self.content_mapper_extension_rewrites,
                    self.use_case_sensitive_file_names,
                )
                .unwrap(),
            );
        }
        f.new_call_expression(
            f.new_identifier("require"),
            None, /*questionDotToken*/
            None, /*typeArguments*/
            f.new_node_list(args),
            NodeFlags::None,
        )
    }

    // commonjsmodule.go:695
    fn get_helper_expression_for_export(&self, node: P<Node>, inner_expr: P<Node>) -> P<Node> {
        if get_export_needs_import_star_helper(node) {
            return self
                .visitor()
                .visit_node(Some(self.factory().new_import_star_helper(inner_expr)))
                .unwrap();
        }
        inner_expr
    }

    // commonjsmodule.go:702
    fn get_helper_expression_for_import(&self, node: P<Node>, inner_expr: P<Node>) -> P<Node> {
        if get_import_needs_import_star_helper(node) {
            return self
                .visitor()
                .visit_node(Some(self.factory().new_import_star_helper(inner_expr)))
                .unwrap();
        }
        if get_import_needs_import_default_helper(node) {
            return self
                .visitor()
                .visit_node(Some(self.factory().new_import_default_helper(inner_expr)))
                .unwrap();
        }
        inner_expr
    }

    // commonjsmodule.go:712
    pub(crate) fn visit_top_level_import_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let f = self.factory();
        let emit_context = self.emit_context();
        if node.as_import_declaration().import_clause.is_none() {
            // import "mod";
            let statement = f.new_expression_statement(self.create_require_call(node));
            emit_context.set_original(statement, node);
            emit_context.assign_comment_and_source_map_ranges(statement, node);
            return Some(statement);
        }

        let mut statements: Vec<P<Node>> = Vec::new();
        let mut variables: Vec<P<Node>> = Vec::new();
        let namespace_declaration = ast::get_namespace_declaration_node(node);
        if let Some(namespace_declaration) = namespace_declaration
            && !ast::is_default_import(node)
        {
            // import * as n from "mod";
            variables.push(f.new_variable_declaration(
                namespace_declaration.name().unwrap().clone_node(f),
                None, /*exclamationToken*/
                None, /*type*/
                Some(self.get_helper_expression_for_import(node, self.create_require_call(node))),
            ));
        } else {
            // import d from "mod";
            // import { x, y } from "mod";
            // import d, { x, y } from "mod";
            // import d, * as n from "mod";
            variables.push(f.new_variable_declaration(
                f.new_generated_name_for_node(node),
                None, /*exclamationToken*/
                None, /*type*/
                Some(self.get_helper_expression_for_import(node, self.create_require_call(node))),
            ));

            if let Some(namespace_declaration) = namespace_declaration
                && ast::is_default_import(node)
            {
                variables.push(f.new_variable_declaration(
                    namespace_declaration.name().unwrap().clone_node(f),
                    None, /*exclamationToken*/
                    None, /*type*/
                    Some(f.new_generated_name_for_node(node)),
                ));
            }
        }

        let var_statement = f.new_variable_statement(
            None, /*modifiers*/
            f.new_variable_declaration_list(f.new_node_list(variables), NodeFlags::Const),
        );

        emit_context.set_original(var_statement, node);
        emit_context.assign_comment_and_source_map_ranges(var_statement, node);
        statements.push(var_statement);
        self.append_exports_of_import_declaration(&mut statements, node);
        single_or_many(Some(statements), f)
    }

    // commonjsmodule.go:782
    pub(crate) fn visit_top_level_import_equals_declaration(
        &self,
        node: P<Node>,
    ) -> Option<P<Node>> {
        // import m = n;
        assert!(
            ast::is_external_module_import_equals_declaration(node),
            "import= for internal module references should be handled in an earlier transformer."
        );

        let f = self.factory();
        let emit_context = self.emit_context();
        let mut statements: Vec<P<Node>> = Vec::new();
        if ast::has_syntactic_modifier(node, ModifierFlags::Export) {
            // export import m = require("mod");
            let statement = f.new_expression_statement(self.create_export_expression(
                node.name().unwrap(),
                self.create_require_call(node),
                Some(node.loc()),
                false, /*liveBinding*/
            ));

            emit_context.set_original(statement, node);
            emit_context.assign_comment_and_source_map_ranges(statement, node);
            statements.push(statement);
        } else {
            // import m = require("mod");
            let statement = f.new_variable_statement(
                None, /*modifiers*/
                f.new_variable_declaration_list(
                    f.new_node_list(vec![f.new_variable_declaration(
                        node.name().unwrap().clone_node(f),
                        None, /*exclamationToken*/
                        None, /*typeNode*/
                        Some(self.create_require_call(node)),
                    )]),
                    NodeFlags::Const,
                ),
            );
            emit_context.set_original(statement, node);
            emit_context.assign_comment_and_source_map_ranges(statement, node);
            statements.push(statement);
        }

        self.append_exports_of_declaration(
            &mut statements,
            node,
            None,  /*seen*/
            false, /*liveBinding*/
        );
        single_or_many(Some(statements), f)
    }

    // commonjsmodule.go:829
    pub(crate) fn visit_top_level_export_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let n = node.as_export_declaration();
        n.module_specifier?;

        let f = self.factory();
        let emit_context = self.emit_context();
        let generated_name = f.new_generated_name_for_node(node);
        if let Some(export_clause) = n.export_clause {
            if ast::is_named_exports(export_clause) {
                // export { x, y } from "mod";
                let mut statements: Vec<P<Node>> = Vec::new();
                let var_statement = f.new_variable_statement(
                    None, /*modifiers*/
                    f.new_variable_declaration_list(
                        f.new_node_list(vec![f.new_variable_declaration(
                            generated_name,
                            None, /*exclamationToken*/
                            None, /*type*/
                            Some(self.create_require_call(node)),
                        )]),
                        NodeFlags::None,
                    ),
                );
                emit_context.set_original(var_statement, node);
                emit_context.assign_comment_and_source_map_ranges(var_statement, node);
                statements.push(var_statement);

                statements.extend(export_clause.elements().iter().map(|&specifier| {
                    let specifier_name = specifier.property_name_or_name().unwrap();
                    let export_needs_import_default =
                        ast::module_export_name_is_default(specifier_name);

                    let target = if export_needs_import_default {
                        f.new_import_default_helper(generated_name)
                    } else {
                        generated_name
                    };

                    let export_name = if ast::is_string_literal(specifier.name().unwrap()) {
                        f.new_string_literal_from_node(specifier.name().unwrap())
                    } else {
                        f.get_export_name(specifier)
                    };

                    let exported_value = if ast::is_string_literal(specifier_name) {
                        f.new_element_access_expression(
                            target,
                            None, /*questionDotToken*/
                            specifier_name,
                            NodeFlags::None,
                        )
                    } else {
                        f.new_property_access_expression(
                            target,
                            None, /*questionDotToken*/
                            specifier_name,
                            NodeFlags::None,
                        )
                    };
                    let statement = f.new_expression_statement(self.create_export_expression(
                        export_name,
                        exported_value,
                        None, /*location*/
                        true, /*liveBinding*/
                    ));
                    emit_context.set_original(statement, specifier);
                    emit_context.assign_comment_and_source_map_ranges(statement, specifier);
                    statement
                }));

                return single_or_many(Some(statements), f);
            }

            // export * as ns from "mod";
            // export * as default from "mod";
            let clause_name = export_clause.name().unwrap();
            let export_name = if ast::is_string_literal(clause_name) {
                f.new_string_literal_from_node(clause_name)
            } else {
                clause_name.clone_node(f)
            };
            let statement = f.new_expression_statement(self.create_export_expression(
                export_name,
                self.get_helper_expression_for_export(node, self.create_require_call(node)),
                None,  /*location*/
                false, /*liveBinding*/
            ));
            emit_context.set_original(statement, node);
            emit_context.assign_comment_and_source_map_ranges(statement, node);
            return Some(statement);
        }

        // export * from "mod";
        let statement = f.new_expression_statement(
            self.visitor()
                .visit_node(Some(f.new_export_star_helper(
                    self.create_require_call(node),
                    f.new_identifier("exports"),
                )))
                .unwrap(),
        );
        emit_context.set_original(statement, node);
        emit_context.assign_comment_and_source_map_ranges(statement, node);
        Some(statement)
    }

    // commonjsmodule.go:935
    pub(crate) fn visit_top_level_export_assignment(&self, node: P<Node>) -> Option<P<Node>> {
        if node.as_export_assignment().is_export_equals {
            return None;
        }

        Some(self.create_export_statement(
            self.factory().new_identifier("default"),
            self.visitor().visit_node(node.expression()).unwrap(),
            Some(node.loc()), /*location*/
            true,             /*allowComments*/
            false,            /*liveBinding*/
        ))
    }

    // commonjsmodule.go:949
    pub(crate) fn visit_top_level_function_declaration(&self, node: P<Node>) -> P<Node> {
        if ast::has_syntactic_modifier(node, ModifierFlags::Export) {
            let f = self.factory();
            let mut v = self.visitor();
            f.update_function_declaration(
                node,
                extract_modifiers(
                    self.emit_context(),
                    node.modifiers(),
                    !ModifierFlags::ExportDefault,
                ),
                node.as_function_declaration().asterisk_token(),
                Some(f.get_declaration_name(node)),
                None, /*typeParameters*/
                v.visit_nodes(node.parameter_list()),
                None, /*type*/
                None, /*fullSignature*/
                v.visit_node(node.body()),
            )
        } else {
            self.visitor().visit_each_child(Some(node)).unwrap()
        }
    }

    // commonjsmodule.go:968
    pub(crate) fn visit_top_level_class_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let f = self.factory();
        let mut v = self.visitor();
        let mut statements: Vec<P<Node>> = Vec::new();
        if ast::has_syntactic_modifier(node, ModifierFlags::Export) {
            statements.push(f.update_class_declaration(
                node,
                v.visit_modifiers(extract_modifiers(
                    self.emit_context(),
                    node.modifiers(),
                    !ModifierFlags::ExportDefault,
                )),
                Some(f.get_declaration_name(node)),
                None, /*typeParameters*/
                v.visit_nodes(node.as_class_declaration().heritage_clauses()),
                v.visit_nodes(node.member_list()).unwrap(),
            ));
        } else {
            statements.push(v.visit_each_child(Some(node)).unwrap());
        }
        self.append_exports_of_class_or_function_declaration(&mut statements, node);
        single_or_many(Some(statements), f)
    }
}
