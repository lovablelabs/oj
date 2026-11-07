// esdecorator.go lines 1231-end (class elements, expressions, descriptors, metadata).

use printer::EmitFlags;

use super::*;

// esdecorator.go:1232
#[derive(Default)]
pub struct PartialResult {
    pub modifiers: Option<P<ModifierList>>,
    pub referenced_name: Option<P<Node>>,
    pub name: Option<P<Node>>,
    pub initializers_name: Option<P<Node>>,
    pub extra_initializers_name: Option<P<Node>>,
    pub descriptor_name: Option<P<Node>>,
    pub this_arg: Option<P<Node>>,
}

// esdecorator.go:1242
pub type CreateDescriptorFunc =
    fn(&EsDecoratorTransformer, P<Node>, Option<P<ModifierList>>) -> P<Node>;

impl EsDecoratorTransformer {
    // esdecorator.go:1244
    fn partial_transform_class_element(
        &self,
        member: P<Node>,
        ci: Option<P<ClassInfo>>,
        create_descriptor: Option<CreateDescriptorFunc>,
    ) -> PartialResult {
        let f = self.base.factory();
        let ec = self.base.emit_context();

        let Some(ci) = ci else {
            let modifiers =
                self.modifier_visitor.get().unwrap().clone().visit_modifiers(member.modifiers());
            self.enter_name();
            let name = self.visit_property_name(member.name().unwrap());
            self.exit_name();
            return PartialResult { modifiers, name: Some(name), ..Default::default() };
        };

        // Member decorators require privileged access to private names. However, computed property
        // evaluation occurs interspersed with decorator evaluation. This means that if we encounter
        // a computed property name we must inline decorator evaluation.

        // Collect decorators for this member. Decorator expressions evaluate outside the class body,
        // so `this` should NOT be replaced with `_classThis`.
        let saved_class_this = self.class_this.get();
        self.class_this.set(None);
        let member_decorators = self.transform_all_decorators_of_declaration(&member.decorators());
        self.class_this.set(saved_class_this);
        let modifiers =
            self.modifier_visitor.get().unwrap().clone().visit_modifiers(member.modifiers());

        let mut result = PartialResult { modifiers, ..Default::default() };

        if !member_decorators.is_empty() {
            let member_decorators_name = self.create_helper_variable(member, "decorators");
            let member_decorators_array =
                f.new_array_literal_expression(f.new_node_list(member_decorators), false);
            let member_decorators_assignment =
                f.new_assignment_expression(member_decorators_name, member_decorators_array);
            let mi = P::new(MemberInfo::default());
            mi.member_decorators_name.set(Some(member_decorators_name));
            ci.member_infos.borrow_mut().insert(member, mi);
            self.pending_expressions
                .borrow_mut()
                .get_or_insert_with(Vec::new)
                .push(member_decorators_assignment);

            // 5. Static non-field (method/getter/setter/auto-accessor) element decorators are applied
            // 6. Non-static non-field (method/getter/setter/auto-accessor) element decorators are applied
            // 7. Static field (excl. auto-accessor) element decorators are applied
            // 8. Non-static field (excl. auto-accessor) element decorators are applied

            // Determine decorator kind
            let kind = if ast::is_get_accessor_declaration(member) {
                "getter"
            } else if ast::is_set_accessor_declaration(member) {
                "setter"
            } else if ast::is_method_declaration(member) {
                "method"
            } else if ast::is_auto_accessor_property_declaration(member) {
                "accessor"
            } else if ast::is_property_declaration(member) {
                "field"
            } else {
                panic!("Unexpected class element kind.");
            };

            // Determine the property name for the context
            let mut property_name_computed = false;
            let mut property_name_expr: Option<P<Node>> = None;
            let member_name = member.name();
            if let Some(name) =
                member_name.filter(|n| ast::is_identifier(*n) || ast::is_private_identifier(*n))
            {
                property_name_computed = false;
                property_name_expr = Some(name);
            } else if let Some(name) = member_name.filter(|n| ast::is_property_name_literal(*n)) {
                property_name_computed = true;
                property_name_expr = Some(f.new_string_literal_from_node(name));
            } else if let Some(name) = member_name.filter(|n| ast::is_computed_property_name(*n)) {
                let cpn_expression = name.as_computed_property_name().expression;
                if ast::is_property_name_literal(cpn_expression)
                    && !ast::is_identifier(cpn_expression)
                {
                    property_name_computed = true;
                    property_name_expr = Some(f.new_string_literal_from_node(cpn_expression));
                } else {
                    self.enter_name();
                    let (referenced_name, name) = self.visit_referenced_property_name(name);
                    result.referenced_name = referenced_name;
                    result.name = Some(name);
                    self.exit_name();
                    property_name_computed = true;
                    property_name_expr = result.referenced_name;
                }
            }

            let context_obj = f.new_es_decorate_class_element_context_object(
                kind,
                property_name_computed,
                property_name_expr.unwrap(),
                ast::is_static(member),
                member_name.is_some_and(ast::is_private_identifier),
                // 15.7.3 CreateDecoratorAccessObject (kind, name)
                // 2. If _kind_ is ~field~, ~method~, ~accessor~, or ~getter~, then ...
                ast::is_property_declaration(member)
                    || ast::is_get_accessor_declaration(member)
                    || ast::is_method_declaration(member),
                // 3. If _kind_ is ~field~, ~accessor~, or ~setter~, then ...
                ast::is_property_declaration(member) || ast::is_set_accessor_declaration(member),
                ci.metadata_reference.get().unwrap(),
            );

            if ast::is_method_or_accessor(member) {
                // produces (public elements):
                //   __esDecorate(this, null, _member_decorators, { kind: "method", name: "...", static: false, private: false, access: { ... } }, _instanceExtraInitializers);
                // produces (private elements):
                //   __esDecorate(this, _member_descriptor = { value() { ... } }, _member_decorators, { kind: "method", name: "...", static: false, private: true, access: { ... } }, _instanceExtraInitializers);
                let method_extra_initializers_name = if ast::is_static(member) {
                    ci.static_method_extra_initializers_name.get()
                } else {
                    ci.instance_method_extra_initializers_name.get()
                };
                assert!(
                    method_extra_initializers_name.is_some(),
                    "methodExtraInitializersName should be defined"
                );

                let descriptor_arg = if let (true, Some(create_descriptor)) = (
                    ast::is_private_identifier_class_element_declaration(member),
                    create_descriptor,
                ) {
                    // For private members, extract the method/accessor body into a descriptor object.
                    // Filter modifiers to only keep async.
                    let async_mods = self
                        .async_only_modifier_visitor
                        .get()
                        .unwrap()
                        .clone()
                        .visit_modifiers(result.modifiers);
                    let descriptor = create_descriptor(self, member, async_mods);
                    let descriptor_name = self.create_helper_variable(member, "descriptor");
                    mi.member_descriptor_name.set(Some(descriptor_name));
                    result.descriptor_name = Some(descriptor_name);
                    f.new_assignment_expression(descriptor_name, descriptor)
                } else {
                    f.new_token(Kind::NullKeyword)
                };

                let es_decorate_expr = f.new_es_decorate_helper(
                    f.new_this_expression(),
                    descriptor_arg,
                    member_decorators_name,
                    context_obj,
                    f.new_token(Kind::NullKeyword),
                    method_extra_initializers_name.unwrap(),
                );
                let es_decorate_statement = f.new_expression_statement(es_decorate_expr);
                ec.set_source_map_range(es_decorate_statement, move_range_past_decorators(member));
                Self::append_decoration_statement(ci, member, es_decorate_statement);
            } else if ast::is_property_declaration(member) {
                let initializers_name = self.create_helper_variable(member, "initializers");
                let extra_initializers_name =
                    self.create_helper_variable(member, "extraInitializers");
                mi.member_initializers_name.set(Some(initializers_name));
                mi.member_extra_initializers_name.set(Some(extra_initializers_name));
                result.initializers_name = Some(initializers_name);
                result.extra_initializers_name = Some(extra_initializers_name);
                if ast::is_static(member) {
                    result.this_arg = ci.class_this.get();
                }

                let ctor_arg = if ast::is_auto_accessor_property_declaration(member) {
                    f.new_this_expression()
                } else {
                    f.new_token(Kind::NullKeyword)
                };

                let descriptor_arg = if let (true, Some(create_descriptor)) = (
                    ast::is_private_identifier_class_element_declaration(member)
                        && ast::has_accessor_modifier(member),
                    create_descriptor,
                ) {
                    let descriptor = create_descriptor(self, member, None);
                    let descriptor_name = self.create_helper_variable(member, "descriptor");
                    mi.member_descriptor_name.set(Some(descriptor_name));
                    result.descriptor_name = Some(descriptor_name);
                    f.new_assignment_expression(descriptor_name, descriptor)
                } else {
                    f.new_token(Kind::NullKeyword)
                };

                // produces:
                //   __esDecorate(null, null, _member_decorators, { kind: "field", name: "...", static: false, private: ..., access: { ... } }, _instanceExtraInitializers);
                let es_decorate_expr = f.new_es_decorate_helper(
                    ctor_arg,
                    descriptor_arg,
                    member_decorators_name,
                    context_obj,
                    initializers_name,
                    extra_initializers_name,
                );
                let es_decorate_statement = f.new_expression_statement(es_decorate_expr);
                ec.set_source_map_range(es_decorate_statement, move_range_past_decorators(member));
                Self::append_decoration_statement(ci, member, es_decorate_statement);
            }
        }

        if result.name.is_none() {
            self.enter_name();
            result.name = Some(self.visit_property_name(member.name().unwrap()));
            self.exit_name();
        }

        if result.modifiers.is_none_or(|m| m.nodes().is_empty())
            && (ast::is_method_declaration(member) || ast::is_property_declaration(member))
        {
            // Don't emit leading comments on the name for methods and properties without modifiers, otherwise we
            // will end up printing duplicate comments.
            ec.set_emit_flags(result.name.unwrap(), EmitFlags::NoLeadingComments);
        }

        result
    }

    // esdecorator.go:1447
    // appendDecorationStatement appends an __esDecorate statement to the appropriate
    // decoration statement list on classInfo based on the member's kind and static-ness.
    fn append_decoration_statement(ci: P<ClassInfo>, member: P<Node>, stmt: P<Node>) {
        if ast::is_method_or_accessor(member) || ast::is_auto_accessor_property_declaration(member)
        {
            if ast::is_static(member) {
                ci.static_non_field_decoration_statements.borrow_mut().push(stmt);
            } else {
                ci.non_static_non_field_decoration_statements.borrow_mut().push(stmt);
            }
        } else if ast::is_property_declaration(member)
            && !ast::is_auto_accessor_property_declaration(member)
        {
            if ast::is_static(member) {
                ci.static_field_decoration_statements.borrow_mut().push(stmt);
            } else {
                ci.non_static_field_decoration_statements.borrow_mut().push(stmt);
            }
        } else {
            panic!("Unexpected class element kind.");
        }
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:1465
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    pub(crate) fn visit_method_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        self.enter_class_element(node);
        let result = self.partial_transform_class_element(
            node,
            self.class_info_stack.get(),
            Some(EsDecoratorTransformer::create_method_descriptor_object),
        );
        if let Some(descriptor_name) = result.descriptor_name {
            self.exit_class_element();
            return Some(self.finish_class_element(
                self.create_method_descriptor_forwarder(
                    result.modifiers,
                    result.name.unwrap(),
                    descriptor_name,
                ),
                node,
            ));
        }
        let parameters = self.base.visitor().visit_nodes(node.parameter_list());
        let body = self.base.visitor().visit_node(node.body());
        self.exit_class_element();
        let asterisk = node.as_method_declaration().asterisk_token();
        Some(self.finish_class_element(
            self.base.factory().update_method_declaration(
                node,
                result.modifiers,
                asterisk,
                result.name.unwrap(),
                None,
                None,
                parameters,
                None,
                None,
                body,
            ),
            node,
        ))
    }

    // esdecorator.go:1482
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    pub(crate) fn visit_get_accessor_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        self.enter_class_element(node);
        let result = self.partial_transform_class_element(
            node,
            self.class_info_stack.get(),
            Some(EsDecoratorTransformer::create_get_accessor_descriptor_object),
        );
        if let Some(descriptor_name) = result.descriptor_name {
            self.exit_class_element();
            return Some(self.finish_class_element(
                self.create_get_accessor_descriptor_forwarder(
                    result.modifiers,
                    result.name.unwrap(),
                    descriptor_name,
                ),
                node,
            ));
        }
        let parameters = self.base.visitor().visit_nodes(node.parameter_list());
        let body = self.base.visitor().visit_node(node.body());
        self.exit_class_element();
        Some(self.finish_class_element(
            self.base.factory().update_get_accessor_declaration(
                node,
                result.modifiers,
                result.name.unwrap(),
                None,
                parameters,
                None,
                None,
                body,
            ),
            node,
        ))
    }

    // esdecorator.go:1499
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    pub(crate) fn visit_set_accessor_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        self.enter_class_element(node);
        let result = self.partial_transform_class_element(
            node,
            self.class_info_stack.get(),
            Some(EsDecoratorTransformer::create_set_accessor_descriptor_object),
        );
        if let Some(descriptor_name) = result.descriptor_name {
            self.exit_class_element();
            return Some(self.finish_class_element(
                self.create_set_accessor_descriptor_forwarder(
                    result.modifiers,
                    result.name.unwrap(),
                    descriptor_name,
                ),
                node,
            ));
        }
        let parameters = self.base.visitor().visit_nodes(node.parameter_list());
        let body = self.base.visitor().visit_node(node.body());
        self.exit_class_element();
        Some(self.finish_class_element(
            self.base.factory().update_set_accessor_declaration(
                node,
                result.modifiers,
                result.name.unwrap(),
                None,
                parameters,
                None,
                None,
                body,
            ),
            node,
        ))
    }

    // esdecorator.go:1516
    pub(crate) fn visit_class_static_block_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        self.enter_class_element(node);
        let f = self.base.factory();
        let ec = self.base.emit_context();

        let mut result: P<Node>;
        if is_class_named_evaluation_helper_block(ec, node) {
            result = self.base.visitor().visit_each_child(Some(node)).unwrap();
            // Transfer AssignedName metadata to the new node so isClassNamedEvaluationHelperBlock
            // can still find it after visiting (visiting may create a new node when this->_classThis)
            if let Some(assigned_name) = ec.assigned_name(node)
                && result != node
            {
                ec.set_assigned_name(result, assigned_name);
            }
        } else if is_class_this_assignment_block(ec, node) {
            let saved_class_this = self.class_this.get();
            self.class_this.set(None);
            result = self.base.visitor().visit_each_child(Some(node)).unwrap();
            self.class_this.set(saved_class_this);
        } else {
            // Use a nested variable environment so temp vars generated during static block
            // content transformation (e.g., super access temps) stay scoped to the static block.
            ec.start_variable_environment();
            result = self.base.visitor().visit_each_child(Some(node)).unwrap();
            let var_statements = ec.end_variable_environment();
            if !var_statements.is_empty() {
                // Inject var declarations at the start of the static block's body
                let block_body = result.as_class_static_block_declaration().body;
                let mut new_stmts: Vec<P<Node>> = var_statements;
                new_stmts.extend_from_slice(block_body.statements());
                result = f.new_class_static_block_declaration(
                    None,
                    f.new_block(f.new_node_list(new_stmts), block_body.as_block().multi_line),
                );
            }
            if let Some(ci) = self.class_info_stack.get() {
                ci.has_static_initializers.set(true);
                if !ci.pending_static_initializers.borrow().is_empty() {
                    // If we tried to inject the pending initializers into the current block, we might run into
                    // variable name collisions due to sharing this blocks scope. To avoid this, we inject a new
                    // static block that contains the pending initializers that precedes this block.
                    let pending = std::mem::take(&mut *ci.pending_static_initializers.borrow_mut());
                    let mut stmts: Vec<P<Node>> = Vec::new();
                    stmts.extend(pending.into_iter().map(|init| {
                        let init_stmt = f.new_expression_statement(init);
                        ec.set_source_map_range(init_stmt, ec.source_map_range(init));
                        init_stmt
                    }));
                    let body = f.new_block(f.new_node_list(stmts), true);
                    let static_block = f.new_class_static_block_declaration(None, body);
                    // Return both the new static block and the original
                    self.exit_class_element();
                    return single_or_many(Some(vec![static_block, result]), f);
                }
            }
        }

        self.exit_class_element();
        Some(result)
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:1574
    pub(crate) fn visit_property_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        let ec = self.base.emit_context();
        let f = self.base.factory();
        let mut node = node;
        if is_named_evaluation_and(ec, node, Some(&is_anonymous_class_needing_assigned_name)) {
            node = transform_named_evaluation(
                ec,
                node,
                can_ignore_empty_string_literal_in_assigned_name(node.initializer()),
                "",
            );
        }

        self.enter_class_element(node);

        // TODO(rbuckton): We support decorating `declare x` fields with legacyDecorators, but we currently don't
        //                 support them with esDecorators. We need to consider whether we will support them in the
        //                 future, and how. For now, these should be elided by the `ts` transform.
        assert!(!ast::has_syntactic_modifier(node, ModifierFlags::Ambient), "Not yet implemented.");

        let create_descriptor: Option<CreateDescriptorFunc> = if ast::has_accessor_modifier(node) {
            Some(EsDecoratorTransformer::create_accessor_property_descriptor_object)
        } else {
            None
        };
        let result = self.partial_transform_class_element(
            node,
            self.class_info_stack.get(),
            create_descriptor,
        );

        ec.start_variable_environment();

        let mut initializer = self.base.visitor().visit_node(node.initializer());
        if let Some(initializers_name) = result.initializers_name {
            let this_arg = result.this_arg.unwrap_or_else(|| f.new_this_expression());
            let value = initializer.unwrap_or_else(|| f.new_void_zero_expression());
            initializer =
                Some(f.new_run_initializers_helper(this_arg, initializers_name, Some(value)));
        }

        if ast::is_static(node)
            && initializer.is_some()
            && let Some(ci) = self.class_info_stack.get()
        {
            ci.has_static_initializers.set(true);
        }

        let declarations = ec.end_variable_environment();
        if !declarations.is_empty() {
            let mut stmts = declarations;
            stmts.push(f.new_return_statement(initializer));
            initializer = Some(f.new_immediately_invoked_arrow_function(stmts));
        }

        if let Some(ci) = self.class_info_stack.get() {
            if ast::is_static(node) {
                initializer = self.inject_pending_initializers(ci, true, initializer);
                if let Some(extra) = result.extra_initializers_name {
                    let this_arg = ci.class_this.get().unwrap_or_else(|| f.new_this_expression());
                    ci.pending_static_initializers
                        .borrow_mut()
                        .push(f.new_run_initializers_helper(this_arg, extra, None));
                }
            } else {
                initializer = self.inject_pending_initializers(ci, false, initializer);
                if let Some(extra) = result.extra_initializers_name {
                    ci.pending_instance_initializers
                        .borrow_mut()
                        .push(f.new_run_initializers_helper(f.new_this_expression(), extra, None));
                }
            }
        }

        self.exit_class_element();

        if let (true, Some(descriptor_name)) =
            (ast::has_accessor_modifier(node), result.descriptor_name)
        {
            // given:
            //  accessor #x = 1;
            //
            // emits:
            //  static {
            //      _esDecorate(null, _private_x_descriptor = { get() { return this.#x_1; }, set(value) { this.#x_1 = value; } }, ...)
            //  }
            //  ...
            //  #x_1 = 1;
            //  get #x() { return _private_x_descriptor.get.call(this); }
            //  set #x(value) { _private_x_descriptor.set.call(this, value); }

            let comment_range = ec.comment_range(node);
            let source_map_range = ec.source_map_range(node);

            // Since we're creating two declarations where there was previously one, cache
            // the expression for any computed property names.
            let prop_name = node.name().unwrap();
            let mut getter_name = result.name.unwrap();
            let mut setter_name = result.name.unwrap();
            if ast::is_computed_property_name(prop_name)
                && !is_simple_inlineable_expression(prop_name.expression().unwrap())
            {
                if let Some(cache_assignment) =
                    find_computed_property_name_cache_assignment(ec, prop_name)
                {
                    getter_name = f.update_computed_property_name(
                        prop_name,
                        self.base.visitor().visit_node(prop_name.expression()).unwrap(),
                    );
                    setter_name = f.update_computed_property_name(
                        prop_name,
                        cache_assignment.as_binary_expression().left,
                    );
                } else {
                    let temp = f.new_temp_variable();
                    ec.set_source_map_range(temp, prop_name.expression().unwrap().loc());
                    ec.add_variable_declaration(temp);
                    let expression =
                        self.base.visitor().visit_node(prop_name.expression()).unwrap();
                    let assignment = f.new_assignment_expression(temp, expression);
                    ec.set_source_map_range(assignment, prop_name.expression().unwrap().loc());
                    getter_name = f.update_computed_property_name(prop_name, assignment);
                    setter_name = f.update_computed_property_name(prop_name, temp);
                }
            }

            let modifiers_without_accessor = self
                .accessor_stripping_modifier_visitor
                .get()
                .unwrap()
                .clone()
                .visit_modifiers(result.modifiers);

            let backing_field = create_accessor_property_backing_field(
                f,
                node,
                modifiers_without_accessor,
                initializer,
            );
            ec.set_original(backing_field, node);
            ec.set_emit_flags(backing_field, EmitFlags::NoComments);
            ec.set_source_map_range(backing_field, source_map_range);
            ec.set_source_map_range(
                backing_field.name().unwrap(),
                ec.source_map_range(node.name().unwrap()),
            );

            let getter = self.create_get_accessor_descriptor_forwarder(
                modifiers_without_accessor,
                getter_name,
                descriptor_name,
            );
            ec.set_original(getter, node);
            ec.set_comment_range(getter, comment_range);
            ec.set_source_map_range(getter, source_map_range);

            let setter = self.create_set_accessor_descriptor_forwarder(
                modifiers_without_accessor,
                setter_name,
                descriptor_name,
            );
            ec.set_original(setter, node);
            ec.set_emit_flags(setter, EmitFlags::NoComments);
            ec.set_source_map_range(setter, source_map_range);

            return single_or_many(Some(vec![backing_field, getter, setter]), f);
        }

        Some(self.finish_class_element(
            f.update_property_declaration(
                node,
                result.modifiers,
                result.name.unwrap(),
                None,
                None,
                initializer,
            ),
            node,
        ))
    }

    // esdecorator.go:1724
    pub(crate) fn visit_this_expression(&self, node: P<Node>) -> P<Node> {
        self.class_this.get().unwrap_or(node)
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:1731
    pub(crate) fn visit_call_expression(&self, node: P<Node>) -> P<Node> {
        let call = node.as_call_expression();
        if let (true, Some(class_this)) =
            (ast::is_super_property(call.expression), self.class_this.get())
        {
            let expression = self.base.visitor().visit_node(Some(call.expression)).unwrap();
            let arguments_list = self.base.visitor().visit_nodes(Some(call.arguments)).unwrap();
            let invocation = self.base.factory().new_function_call_call(
                expression,
                Some(class_this),
                arguments_list.nodes(),
            );
            self.base.emit_context().set_original(invocation, node);
            invocation.set_loc(node.loc());
            return invocation;
        }
        self.base.visitor().visit_each_child(Some(node)).unwrap()
    }

    // esdecorator.go:1744
    pub(crate) fn visit_tagged_template_expression(&self, node: P<Node>) -> P<Node> {
        let tte = node.as_tagged_template_expression();
        if let (true, Some(class_this)) = (ast::is_super_property(tte.tag), self.class_this.get()) {
            let f = self.base.factory();
            let tag = self.base.visitor().visit_node(Some(tte.tag)).unwrap();
            let bound_tag = f.new_function_bind_call(tag, class_this, &[]);
            self.base.emit_context().set_original(bound_tag, node);
            bound_tag.set_loc(node.loc());
            let template = self.base.visitor().visit_node(Some(tte.template)).unwrap();
            return f.update_tagged_template_expression(
                node,
                bound_tag,
                None,
                None,
                template,
                node.flags(),
            );
        }
        self.base.visitor().visit_each_child(Some(node)).unwrap()
    }

    // esdecorator.go:1757
    pub(crate) fn visit_property_access_expression(&self, node: P<Node>) -> P<Node> {
        let pa = node.as_property_access_expression();
        if let (true, true, Some(class_this), Some(class_super)) = (
            ast::is_super_property(node),
            ast::is_identifier(pa.name),
            self.class_this.get(),
            self.class_super.get(),
        ) {
            let f = self.base.factory();
            let property_name = f.new_string_literal_from_node(pa.name);
            let super_property = f.new_reflect_get_call(class_super, property_name, class_this);
            self.base.emit_context().set_original(super_property, pa.expression);
            super_property.set_loc(pa.expression.loc());
            return super_property;
        }
        self.base.visitor().visit_each_child(Some(node)).unwrap()
    }

    // esdecorator.go:1769
    pub(crate) fn visit_element_access_expression(&self, node: P<Node>) -> P<Node> {
        let ea = node.as_element_access_expression();
        if let (true, Some(class_this), Some(class_super)) =
            (ast::is_super_property(node), self.class_this.get(), self.class_super.get())
        {
            let property_name =
                self.base.visitor().visit_node(Some(ea.argument_expression)).unwrap();
            let super_property =
                self.base.factory().new_reflect_get_call(class_super, property_name, class_this);
            self.base.emit_context().set_original(super_property, ea.expression);
            super_property.set_loc(ea.expression.loc());
            return super_property;
        }
        self.base.visitor().visit_each_child(Some(node)).unwrap()
    }

    // esdecorator.go:1798
    pub(crate) fn visit_parameter_declaration(&self, node: P<Node>) -> P<Node> {
        let ec = self.base.emit_context();
        let mut param_node = node;
        if is_named_evaluation_and(ec, param_node, Some(&is_anonymous_class_needing_assigned_name))
        {
            param_node = transform_named_evaluation(
                ec,
                param_node,
                can_ignore_empty_string_literal_in_assigned_name(param_node.initializer()),
                "",
            );
        }

        let p = param_node.as_parameter_declaration();
        let updated = self.base.factory().update_parameter_declaration(
            param_node,
            None, // modifiers - strip all modifiers (including decorators)
            p.dot_dot_dot_token(),
            self.base.visitor().visit_node(Some(p.name)).unwrap(),
            None, // questionToken
            None, // type
            self.base.visitor().visit_node(p.initializer()),
        );
        if updated != param_node {
            // While we emit the source map for the node after skipping decorators and modifiers,
            // we need to emit the comments for the original range.
            ec.set_comment_range(updated, param_node.loc());
            let new_loc = move_range_past_modifiers(param_node);
            updated.set_loc(new_loc);
            ec.set_source_map_range(updated, new_loc);
            ec.set_emit_flags(updated.name().unwrap(), EmitFlags::NoTrailingSourceMap);
        }
        updated
    }

    // esdecorator.go:1870
    // visitNamedEvaluationSite replaces Strada's visitPropertyAssignment, visitVariableDeclaration,
    // and visitBindingElement, which all share the same logic.
    pub(crate) fn visit_named_evaluation_site(
        &self,
        node: P<Node>,
        class_expr: Option<P<Node>>,
    ) -> P<Node> {
        let ec = self.base.emit_context();
        let mut node = node;
        if is_named_evaluation_and(ec, node, Some(&is_anonymous_class_needing_assigned_name)) {
            node = transform_named_evaluation(
                ec,
                node,
                can_ignore_empty_string_literal_in_assigned_name(class_expr),
                "",
            );
        }
        self.base.visitor().visit_each_child(Some(node)).unwrap()
    }
}

// esdecorator.go:1877
pub fn is_anonymous_class_needing_assigned_name(node: P<Node>) -> bool {
    ast::is_class_expression(node) && node.name().is_none() && is_decorated_class_like(node)
}

// esdecorator.go:1885
// The IIFE produced for `(@dec class {})` will result in an assigned name of the form
// `var class_1 = class { };`, and thus the empty string cannot be ignored. However, The IIFE
// produced for `(class { @dec x; })` will not result in an assigned name since it
// transforms to `return class { };`, and thus the empty string *can* be ignored.
pub fn can_ignore_empty_string_literal_in_assigned_name(node: Option<P<Node>>) -> bool {
    let Some(node) = node else {
        return false;
    };
    let inner_expression = ast::skip_outer_expressions(node, OuterExpressionKinds::All);
    ast::is_class_expression(inner_expression)
        && inner_expression.name().is_none()
        && !ast::class_or_constructor_parameter_is_decorated(false, inner_expression)
}

impl EsDecoratorTransformer {
    // esdecorator.go:1893
    pub(crate) fn visit_for_statement(&self, node: P<Node>) -> P<Node> {
        let fs = node.as_for_statement();
        let discarded = || self.discarded_visitor.get().unwrap().clone();
        let initializer = discarded().visit_node(fs.initializer);
        let condition = self.base.visitor().visit_node(fs.condition);
        let incrementor = discarded().visit_node(fs.incrementor);
        let statement = self
            .base
            .emit_context()
            .visit_iteration_body(Some(fs.statement()), &mut self.base.visitor());
        self.base.factory().update_for_statement(
            node,
            initializer,
            condition,
            incrementor,
            statement.unwrap(),
        )
    }

    // esdecorator.go:1905
    pub(crate) fn visit_expression_statement(&self, node: P<Node>) -> P<Node> {
        self.discarded_visitor.get().unwrap().clone().visit_each_child(Some(node)).unwrap()
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:1909
    pub(crate) fn visit_binary_expression(&self, node: P<Node>, discarded: bool) -> P<Node> {
        let f = self.base.factory();
        let ec = self.base.emit_context();
        let bin = node.as_binary_expression();

        if ast::is_destructuring_assignment(node) {
            let left = self.visit_assignment_pattern(bin.left);
            let right = self.base.visitor().visit_node(Some(bin.right())).unwrap();
            return f.update_binary_expression(node, None, left, None, bin.operator_token, right);
        }

        if ast::is_assignment_expression(node, false) {
            // 13.15.2 RS: Evaluation (see esdecorator.go for the spec steps)
            if is_named_evaluation_and(ec, node, Some(&is_anonymous_class_needing_assigned_name)) {
                let node = transform_named_evaluation(
                    ec,
                    node,
                    can_ignore_empty_string_literal_in_assigned_name(Some(bin.right())),
                    "",
                );
                return self.base.visitor().visit_each_child(Some(node)).unwrap();
            }

            if let (true, Some(class_this), Some(class_super)) =
                (ast::is_super_property(bin.left), self.class_this.get(), self.class_super.get())
            {
                let mut setter_name: Option<P<Node>> = None;
                if ast::is_element_access_expression(bin.left) {
                    setter_name = self.base.visitor().visit_node(Some(
                        bin.left.as_element_access_expression().argument_expression,
                    ));
                } else if ast::is_property_access_expression(bin.left)
                    && ast::is_identifier(bin.left.as_property_access_expression().name)
                {
                    setter_name = Some(f.new_string_literal_from_node(
                        bin.left.as_property_access_expression().name,
                    ));
                }
                if let Some(mut setter_name) = setter_name {
                    // super.x = ...
                    // super.x += ...
                    // super[x] = ...
                    // super[x] += ...
                    let mut expression = self.base.visitor().visit_node(Some(bin.right())).unwrap();
                    if ast::is_compound_assignment(bin.operator_token.kind()) {
                        let mut getter_name = setter_name;
                        if !is_simple_inlineable_expression(setter_name) {
                            getter_name = f.new_temp_variable();
                            ec.add_variable_declaration(getter_name);
                            setter_name = f.new_assignment_expression(getter_name, setter_name);
                        }
                        let super_property_get =
                            f.new_reflect_get_call(class_super, getter_name, class_this);
                        ec.set_original(super_property_get, bin.left);
                        super_property_get.set_loc(bin.left.loc());
                        expression = f.as_node_factory().new_binary_expression(
                            None,
                            super_property_get,
                            None,
                            f.new_token(get_non_assignment_operator_for_compound_assignment(
                                bin.operator_token.kind(),
                            )),
                            expression,
                        );
                        expression.set_loc(node.loc());
                    }
                    let mut temp: Option<P<Node>> = None;
                    if !discarded {
                        let t = f.new_temp_variable();
                        ec.add_variable_declaration(t);
                        temp = Some(t);
                    }
                    if let Some(temp) = temp {
                        expression = f.new_assignment_expression(temp, expression);
                        expression.set_loc(node.loc());
                    }
                    expression =
                        f.new_reflect_set_call(class_super, setter_name, expression, class_this);
                    ec.set_original(expression, node);
                    expression.set_loc(node.loc());
                    if let Some(temp) = temp {
                        expression = f.new_comma_expression(expression, temp);
                        expression.set_loc(node.loc());
                    }
                    return expression;
                }
            }
        }

        if bin.operator_token.kind() == Kind::CommaToken {
            let left =
                self.discarded_visitor.get().unwrap().clone().visit_node(Some(bin.left)).unwrap();
            let right = if discarded {
                self.discarded_visitor.get().unwrap().clone().visit_node(Some(bin.right()))
            } else {
                self.base.visitor().visit_node(Some(bin.right()))
            };
            return f.update_binary_expression(
                node,
                None,
                left,
                None,
                bin.operator_token,
                right.unwrap(),
            );
        }

        self.base.visitor().visit_each_child(Some(node)).unwrap()
    }

    // esdecorator.go:2019
    pub(crate) fn visit_pre_or_postfix_unary_expression(
        &self,
        node: P<Node>,
        discarded: bool,
    ) -> P<Node> {
        let f = self.base.factory();
        let ec = self.base.emit_context();

        let (operator, operand_node) = if ast::is_prefix_unary_expression(node) {
            (node.as_prefix_unary_expression().operator, node.as_prefix_unary_expression().operand)
        } else {
            (
                node.as_postfix_unary_expression().operator,
                node.as_postfix_unary_expression().operand,
            )
        };

        if operator == Kind::PlusPlusToken || operator == Kind::MinusMinusToken {
            let operand = ast::skip_parentheses(operand_node);
            if let (true, Some(class_this), Some(class_super)) =
                (ast::is_super_property(operand), self.class_this.get(), self.class_super.get())
            {
                let mut setter_name: Option<P<Node>> = None;
                if ast::is_element_access_expression(operand) {
                    setter_name = self.base.visitor().visit_node(Some(
                        operand.as_element_access_expression().argument_expression,
                    ));
                } else if ast::is_property_access_expression(operand)
                    && ast::is_identifier(operand.as_property_access_expression().name)
                {
                    setter_name = Some(f.new_string_literal_from_node(
                        operand.as_property_access_expression().name,
                    ));
                }
                if let Some(mut setter_name) = setter_name {
                    let mut getter_name = setter_name;
                    if !is_simple_inlineable_expression(setter_name) {
                        getter_name = f.new_temp_variable();
                        ec.add_variable_declaration(getter_name);
                        setter_name = f.new_assignment_expression(getter_name, setter_name);
                    }

                    let mut expression =
                        f.new_reflect_get_call(class_super, getter_name, class_this);
                    ec.set_original(expression, node);
                    expression.set_loc(node.loc());

                    // If the result of this expression is discarded, we don't need to create an extra temp
                    // variable to hold the result (see esdecorator.go for the worked examples).
                    let mut temp: Option<P<Node>> = None;
                    if !discarded {
                        let t = f.new_temp_variable();
                        ec.add_variable_declaration(t);
                        temp = Some(t);
                    }

                    expression = expand_pre_or_postfix_increment_or_decrement_expression(
                        f, ec, node, expression, temp,
                    );

                    expression =
                        f.new_reflect_set_call(class_super, setter_name, expression, class_this);
                    ec.set_original(expression, node);
                    expression.set_loc(node.loc());

                    if let Some(temp) = temp {
                        expression = f.new_comma_expression(expression, temp);
                        expression.set_loc(node.loc());
                    }

                    return expression;
                }
            }
        }

        self.base.visitor().visit_each_child(Some(node)).unwrap()
    }

    // esdecorator.go:2099
    fn visit_referenced_property_name(&self, node: P<Node>) -> (Option<P<Node>>, P<Node>) {
        let f = self.base.factory();
        if ast::is_property_name_literal(node) || ast::is_private_identifier(node) {
            return (
                Some(f.new_string_literal_from_node(node)),
                self.base.visitor().visit_node(Some(node)).unwrap(),
            );
        }

        let cpn_expression = node.as_computed_property_name().expression;
        if ast::is_property_name_literal(cpn_expression) && !ast::is_identifier(cpn_expression) {
            return (
                Some(f.new_string_literal_from_node(cpn_expression)),
                self.base.visitor().visit_node(Some(node)).unwrap(),
            );
        }

        let referenced_name = f.new_generated_name_for_node(node);
        self.base.emit_context().add_variable_declaration(referenced_name);

        let key =
            f.new_prop_key_helper(self.base.visitor().visit_node(Some(cpn_expression)).unwrap());
        let assignment = f.new_assignment_expression(referenced_name, key);
        let updated_name =
            f.update_computed_property_name(node, self.inject_pending_expressions(assignment));
        (Some(referenced_name), updated_name)
    }

    // esdecorator.go:2118
    fn visit_property_name(&self, node: P<Node>) -> P<Node> {
        if ast::is_computed_property_name(node) {
            return self.visit_computed_property_name(node);
        }
        self.base.visitor().visit_node(Some(node)).unwrap()
    }

    // esdecorator.go:2125
    pub(crate) fn visit_computed_property_name(&self, node: P<Node>) -> P<Node> {
        let mut expression = self
            .base
            .visitor()
            .visit_node(Some(node.as_computed_property_name().expression))
            .unwrap();
        if !is_simple_inlineable_expression(expression) {
            expression = self.inject_pending_expressions(expression);
        }
        self.base.factory().update_computed_property_name(node, expression)
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:2134
    fn visit_destructuring_assignment_target(&self, node: P<Node>) -> P<Node> {
        if ast::is_object_literal_expression(node) || ast::is_array_literal_expression(node) {
            return self.visit_assignment_pattern(node);
        }

        if let (true, Some(class_this), Some(class_super)) =
            (ast::is_super_property(node), self.class_this.get(), self.class_super.get())
        {
            let f = self.base.factory();
            let ec = self.base.emit_context();
            let mut property_name: Option<P<Node>> = None;
            if ast::is_element_access_expression(node) {
                property_name = self
                    .base
                    .visitor()
                    .visit_node(Some(node.as_element_access_expression().argument_expression));
            } else if ast::is_property_access_expression(node)
                && ast::is_identifier(node.as_property_access_expression().name)
            {
                property_name =
                    Some(f.new_string_literal_from_node(node.as_property_access_expression().name));
            }
            if let Some(property_name) = property_name {
                let param_name = f.new_temp_variable();
                let expression = f.new_assignment_target_wrapper(
                    param_name,
                    f.new_reflect_set_call(class_super, property_name, param_name, class_this),
                );
                ec.set_original(expression, node);
                expression.set_loc(node.loc());
                return expression;
            }
        }

        self.base.visitor().visit_each_child(Some(node)).unwrap()
    }

    // esdecorator.go:2168
    fn visit_assignment_element(&self, node: P<Node>) -> P<Node> {
        // 13.15.5.5 RS: IteratorDestructuringAssignmentEvaluation (see esdecorator.go)
        if ast::is_assignment_expression(node, true /*excludeCompoundAssignment*/) {
            let f = self.base.factory();
            let mut node = node;
            if is_named_evaluation_and(
                self.base.emit_context(),
                node,
                Some(&is_anonymous_class_needing_assigned_name),
            ) {
                node = transform_named_evaluation(
                    self.base.emit_context(),
                    node,
                    can_ignore_empty_string_literal_in_assigned_name(Some(
                        node.as_binary_expression().right(),
                    )),
                    "",
                );
            }
            let bin = node.as_binary_expression();
            let assignment_target = self.visit_destructuring_assignment_target(bin.left);
            let initializer = self.base.visitor().visit_node(Some(bin.right())).unwrap();
            return f.update_binary_expression(
                node,
                None,
                assignment_target,
                None,
                bin.operator_token,
                initializer,
            );
        }
        self.visit_destructuring_assignment_target(node)
    }

    // esdecorator.go:2190
    fn visit_assignment_rest_element(&self, node: P<Node>) -> P<Node> {
        let se_expression = node.as_spread_element().expression;
        if ast::is_left_hand_side_expression(se_expression) {
            let expression = self.visit_destructuring_assignment_target(se_expression);
            return self.base.factory().update_spread_element(node, expression);
        }
        self.base.visitor().visit_each_child(Some(node)).unwrap()
    }

    // esdecorator.go:2200
    pub(crate) fn visit_array_assignment_element(&self, node: P<Node>) -> Option<P<Node>> {
        assert!(ast::is_expression(node));
        if ast::is_spread_element(node) {
            return Some(self.visit_assignment_rest_element(node));
        }
        if !ast::is_omitted_expression(node) {
            return Some(self.visit_assignment_element(node));
        }
        self.base.visitor().visit_each_child(Some(node))
    }

    // esdecorator.go:2211
    fn visit_assignment_property_node(&self, node: P<Node>) -> P<Node> {
        // AssignmentProperty : PropertyName `:` AssignmentElement (see esdecorator.go for the spec steps)
        let f = self.base.factory();
        let pa = node.as_property_assignment();
        let name = self.base.visitor().visit_node(node.name()).unwrap();
        let initializer = pa.initializer();
        if ast::is_assignment_expression(initializer, true /*excludeCompoundAssignment*/) {
            let assignment_element = self.visit_assignment_element(initializer);
            return f.update_property_assignment(node, None, name, None, None, assignment_element);
        }
        if ast::is_left_hand_side_expression(initializer) {
            let assignment_element = self.visit_destructuring_assignment_target(initializer);
            return f.update_property_assignment(node, None, name, None, None, assignment_element);
        }
        self.base.visitor().visit_each_child(Some(node)).unwrap()
    }

    // esdecorator.go:2237
    fn visit_shorthand_assignment_property(&self, node: P<Node>) -> P<Node> {
        // AssignmentProperty : IdentifierReference Initializer? (see esdecorator.go for the spec steps)
        let mut node = node;
        if is_named_evaluation_and(
            self.base.emit_context(),
            node,
            Some(&is_anonymous_class_needing_assigned_name),
        ) {
            node = transform_named_evaluation(
                self.base.emit_context(),
                node,
                can_ignore_empty_string_literal_in_assigned_name(
                    node.as_shorthand_property_assignment().object_assignment_initializer(),
                ),
                "",
            );
        }
        self.base.visitor().visit_each_child(Some(node)).unwrap()
    }

    // esdecorator.go:2253
    fn visit_assignment_rest_property(&self, node: P<Node>) -> P<Node> {
        let sa_expression = node.as_spread_assignment().expression;
        if ast::is_left_hand_side_expression(sa_expression) {
            let expression = self.visit_destructuring_assignment_target(sa_expression);
            return self.base.factory().update_spread_assignment(node, expression);
        }
        self.base.visitor().visit_each_child(Some(node)).unwrap()
    }

    // esdecorator.go:2263
    pub(crate) fn visit_object_assignment_element(&self, node: P<Node>) -> Option<P<Node>> {
        assert!(ast::is_object_literal_element(node));
        if ast::is_spread_assignment(node) {
            return Some(self.visit_assignment_rest_property(node));
        }
        if ast::is_shorthand_property_assignment(node) {
            return Some(self.visit_shorthand_assignment_property(node));
        }
        if ast::is_property_assignment(node) {
            return Some(self.visit_assignment_property_node(node));
        }
        self.base.visitor().visit_each_child(Some(node))
    }

    // esdecorator.go:2277
    fn visit_assignment_pattern(&self, node: P<Node>) -> P<Node> {
        let f = self.base.factory();
        if ast::is_array_literal_expression(node) {
            let ale = node.as_array_literal_expression();
            let elements = self
                .array_assignment_visitor
                .get()
                .unwrap()
                .clone()
                .visit_nodes(Some(ale.elements));
            return f.update_array_literal_expression(node, elements.unwrap(), ale.multi_line);
        }
        let ole = node.as_object_literal_expression();
        let properties =
            self.object_assignment_visitor.get().unwrap().clone().visit_nodes(Some(ole.properties));
        f.update_object_literal_expression(node, properties.unwrap(), ole.multi_line)
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:2289
    pub(crate) fn visit_export_assignment(&self, node: P<Node>) -> P<Node> {
        // 16.2.3.7 RS: Evaluation (see esdecorator.go)
        self.visit_named_evaluation_site(node, node.expression())
    }

    // esdecorator.go:2298
    pub(crate) fn visit_parenthesized_expression(&self, node: P<Node>, discarded: bool) -> P<Node> {
        // 8.4.5 RS: NamedEvaluation
        //   ParenthesizedExpression : `(` Expression `)`
        //     ...
        //     2. Return ? NamedEvaluation of |Expression| with argument _name_.
        let inner = node.as_parenthesized_expression().expression();
        let expression = if discarded {
            self.discarded_visitor.get().unwrap().clone().visit_node(Some(inner))
        } else {
            self.base.visitor().visit_node(Some(inner))
        };
        self.base.factory().update_parenthesized_expression(node, expression.unwrap())
    }

    // esdecorator.go:2315
    pub(crate) fn visit_partially_emitted_expression(
        &self,
        node: P<Node>,
        discarded: bool,
    ) -> P<Node> {
        // Emulates 8.4.5 RS: NamedEvaluation
        let inner = node.as_partially_emitted_expression().expression;
        let expression = if discarded {
            self.discarded_visitor.get().unwrap().clone().visit_node(Some(inner))
        } else {
            self.base.visitor().visit_node(Some(inner))
        };
        self.base.factory().update_partially_emitted_expression(node, expression.unwrap())
    }

    // esdecorator.go:2329
    // prependExpressions prepends a list of expressions before a target expression, preserving
    // parenthesization. If expression is nil, the pending expressions are inlined alone.
    fn prepend_expressions(
        &self,
        pending: &[P<Node>],
        expression: Option<P<Node>>,
    ) -> Option<P<Node>> {
        let f = self.base.factory();
        if pending.is_empty() {
            return expression;
        }
        let Some(expression) = expression else {
            return f.inline_expressions(pending);
        };
        if ast::is_parenthesized_expression(expression) {
            let mut exprs: Vec<P<Node>> = pending.to_vec();
            exprs.push(expression.as_parenthesized_expression().expression());
            return Some(f.update_parenthesized_expression(
                expression,
                f.inline_expressions(&exprs).unwrap(),
            ));
        }
        let mut exprs: Vec<P<Node>> = pending.to_vec();
        exprs.push(expression);
        f.inline_expressions(&exprs)
    }

    // esdecorator.go:2350
    fn inject_pending_expressions(&self, expression: P<Node>) -> P<Node> {
        let pending = self.pending_expressions.borrow().clone().unwrap_or_default();
        let result = self.prepend_expressions(&pending, Some(expression));
        assert!(result.is_some());
        if result != Some(expression) {
            *self.pending_expressions.borrow_mut() = None;
        }
        result.unwrap()
    }

    // esdecorator.go:2359
    fn inject_pending_initializers(
        &self,
        ci: P<ClassInfo>,
        is_static: bool,
        expression: Option<P<Node>>,
    ) -> Option<P<Node>> {
        let pending = if is_static {
            &ci.pending_static_initializers
        } else {
            &ci.pending_instance_initializers
        };
        let current = pending.borrow().clone();
        let result = self.prepend_expressions(&current, expression);
        if result != expression {
            pending.borrow_mut().clear();
        }
        result
    }

    // esdecorator.go:2374
    // Transforms all of the decorators for a declaration into an array of expressions.
    pub(crate) fn transform_all_decorators_of_declaration(
        &self,
        decorators: &[P<Node>],
    ) -> Vec<P<Node>> {
        let mut result: Vec<P<Node>> = Vec::with_capacity(decorators.len());
        result.extend(decorators.iter().map(|d| self.transform_decorator(*d)));
        result
    }

    // esdecorator.go:2386
    // Transforms a decorator into an expression.
    fn transform_decorator(&self, decorator: P<Node>) -> P<Node> {
        let f = self.base.factory();
        let expression =
            self.base.visitor().visit_node(Some(decorator.as_decorator().expression)).unwrap();
        self.base.emit_context().set_emit_flags(expression, EmitFlags::NoComments);

        // preserve the 'this' binding for an access expression
        let inner_expression = ast::skip_outer_expressions(expression, OuterExpressionKinds::All);
        if ast::is_access_expression(inner_expression) {
            let (target, this_arg) = self.create_call_binding(expression);
            let bind_call = f.new_function_bind_call(target, this_arg, &[]);
            return f.restore_outer_expressions(
                Some(expression),
                bind_call,
                OuterExpressionKinds::All,
            );
        }
        expression
    }

    // esdecorator.go:2400
    fn create_call_binding(&self, expression: P<Node>) -> (P<Node>, P<Node>) {
        let f = self.base.factory();
        let ec = self.base.emit_context();
        let callee = ast::skip_outer_expressions(expression, OuterExpressionKinds::All);
        if ast::is_super_property(callee) {
            return (callee, f.new_this_expression());
        }
        if callee.kind() == Kind::SuperKeyword {
            return (callee, f.new_this_expression());
        }
        if ec.emit_flags(callee).intersects(EmitFlags::HelperName) {
            return (callee, f.new_void_zero_expression());
        }
        if ast::is_property_access_expression(callee) {
            let pa = callee.as_property_access_expression();
            if Self::should_be_captured_in_temp_variable(pa.expression) {
                let this_arg = f.new_temp_variable();
                ec.add_variable_declaration(this_arg);
                let assign = f.new_assignment_expression(this_arg, pa.expression);
                assign.set_loc(pa.expression.loc());
                let target =
                    f.new_property_access_expression(assign, None, pa.name, NodeFlags::None);
                target.set_loc(callee.loc());
                return (target, this_arg);
            }
            return (callee, pa.expression);
        }
        if ast::is_element_access_expression(callee) {
            let ea = callee.as_element_access_expression();
            if Self::should_be_captured_in_temp_variable(ea.expression) {
                let this_arg = f.new_temp_variable();
                ec.add_variable_declaration(this_arg);
                let assign = f.new_assignment_expression(this_arg, ea.expression);
                assign.set_loc(ea.expression.loc());
                let target = f.new_element_access_expression(
                    assign,
                    None,
                    ea.argument_expression,
                    NodeFlags::None,
                );
                target.set_loc(callee.loc());
                return (target, this_arg);
            }
            return (callee, ea.expression);
        }
        (expression, f.new_void_zero_expression())
    }

    // esdecorator.go:2441
    fn should_be_captured_in_temp_variable(node: P<Node>) -> bool {
        // This is a simplified version of the general shouldBeCapturedInTempVariable from
        // nodeFactory with cacheIdentifiers=true, since createCallBinding in this transform
        // always caches identifiers.
        let target = ast::skip_parentheses(node);
        match target.kind() {
            Kind::ThisKeyword
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::StringLiteral => false,
            // cacheIdentifiers is always true for this transform's createCallBinding
            _ => true,
        }
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:2462
    // Creates a "value", "get", or "set" method for a pseudo-PropertyDescriptor object created for
    // a private element.
    fn create_descriptor_method(
        &self,
        original: P<Node>,
        name: P<Node>, /*PrivateIdentifier*/
        modifiers: Option<P<ModifierList>>,
        asterisk_token: Option<P<Node>>,
        kind: &'static str,
        parameters: Option<P<NodeList>>,
        body: Option<P<Node>>,
    ) -> P<Node> {
        let f = self.base.factory();
        let ec = self.base.emit_context();

        let body = body.unwrap_or_else(|| f.new_block(f.new_node_list(vec![]), false));

        let func_expr = f.new_function_expression(
            modifiers,
            asterisk_token,
            None, /*name*/
            None, /*typeParameters*/
            parameters,
            None, /*type*/
            None, /*fullSignature*/
            Some(body),
        );
        ec.set_original(func_expr, original);
        ec.set_source_map_range(func_expr, move_range_past_decorators(original));
        ec.set_emit_flags(func_expr, EmitFlags::NoComments);

        let prefix = if kind == "get" || kind == "set" { kind } else { "" };
        let function_name = f.new_string_literal_from_node(name);
        let named_function = f.new_set_function_name_helper(func_expr, function_name, prefix);

        let method =
            f.new_property_assignment(None, f.new_identifier(kind), None, None, named_function);
        ec.set_original(method, original);
        ec.set_source_map_range(method, move_range_past_decorators(original));
        ec.set_emit_flags(method, EmitFlags::NoComments);
        method
    }

    // esdecorator.go:2507
    // Creates a pseudo-PropertyDescriptor object used when decorating a private MethodDeclaration.
    fn create_method_descriptor_object(
        &self,
        member: P<Node>,
        modifiers: Option<P<ModifierList>>,
    ) -> P<Node> {
        let f = self.base.factory();
        let parameters = self.base.visitor().visit_nodes(member.parameter_list());
        let body = self.base.visitor().visit_node(member.body());
        let asterisk = member.as_method_declaration().asterisk_token();
        f.new_object_literal_expression(
            f.new_node_list(vec![self.create_descriptor_method(
                member,
                member.name().unwrap(),
                modifiers,
                asterisk,
                "value",
                parameters,
                body,
            )]),
            false,
        )
    }

    // esdecorator.go:2521
    // Creates a pseudo-PropertyDescriptor object used when decorating a private GetAccessor.
    fn create_get_accessor_descriptor_object(
        &self,
        member: P<Node>,
        modifiers: Option<P<ModifierList>>,
    ) -> P<Node> {
        let f = self.base.factory();
        let body = self.base.visitor().visit_node(member.body());
        f.new_object_literal_expression(
            f.new_node_list(vec![self.create_descriptor_method(
                member,
                member.name().unwrap(),
                modifiers,
                None,
                "get",
                Some(f.new_node_list(vec![])),
                body,
            )]),
            false,
        )
    }

    // esdecorator.go:2533
    // Creates a pseudo-PropertyDescriptor object used when decorating a private SetAccessor.
    fn create_set_accessor_descriptor_object(
        &self,
        member: P<Node>,
        modifiers: Option<P<ModifierList>>,
    ) -> P<Node> {
        let f = self.base.factory();
        let parameters = self.base.visitor().visit_nodes(member.parameter_list());
        let body = self.base.visitor().visit_node(member.body());
        f.new_object_literal_expression(
            f.new_node_list(vec![self.create_descriptor_method(
                member,
                member.name().unwrap(),
                modifiers,
                None,
                "set",
                parameters,
                body,
            )]),
            false,
        )
    }

    // esdecorator.go:2547
    // Creates a pseudo-PropertyDescriptor object used when decorating a private auto-accessor PropertyDeclaration.
    // The descriptor contains get/set methods that access the generated backing field.
    fn create_accessor_property_descriptor_object(
        &self,
        member: P<Node>,
        _modifiers: Option<P<ModifierList>>,
    ) -> P<Node> {
        //  {
        //      get() { return this.${privateName}; },
        //      set(value) { this.${privateName} = value; },
        //  }
        let f = self.base.factory();
        let name = member.name().unwrap();
        let backing_field_name = f.new_generated_private_name_for_node_ex(
            name,
            printer::AutoGenerateOptions { suffix: "_accessor_storage", ..Default::default() },
        );
        let getter = self.create_descriptor_method(
            member,
            name,
            None,
            None,
            "get",
            Some(f.new_node_list(vec![])),
            Some(f.new_block(
                f.new_node_list(vec![f.new_return_statement(Some(
                    f.new_property_access_expression(
                        f.new_this_expression(),
                        None,
                        backing_field_name,
                        NodeFlags::None,
                    ),
                ))]),
                false,
            )),
        );
        let setter = self.create_descriptor_method(
            member,
            name,
            None,
            None,
            "set",
            Some(f.new_node_list(vec![f.new_parameter_declaration(
                None,
                None,
                f.new_identifier("value"),
                None,
                None,
                None,
            )])),
            Some(f.new_block(
                f.new_node_list(vec![f.new_expression_statement(f.new_assignment_expression(
                    f.new_property_access_expression(
                        f.new_this_expression(),
                        None,
                        backing_field_name,
                        NodeFlags::None,
                    ),
                    f.new_identifier("value"),
                ))]),
                false,
            )),
        );
        f.new_object_literal_expression(f.new_node_list(vec![getter, setter]), false)
    }
}

impl EsDecoratorTransformer {
    // esdecorator.go:2585
    // Creates a MethodDeclaration that forwards its invocation to a PropertyDescriptor object.
    // (Go builds a GetAccessorDeclaration here; kept as is.)
    fn create_method_descriptor_forwarder(
        &self,
        modifiers: Option<P<ModifierList>>,
        name: P<Node>,
        descriptor_name: P<Node>,
    ) -> P<Node> {
        let f = self.base.factory();
        let static_only =
            self.static_only_modifier_visitor.get().unwrap().clone().visit_modifiers(modifiers);
        f.new_get_accessor_declaration(
            static_only,
            name,
            None, // typeParameters
            Some(f.new_node_list(vec![])),
            None, // type
            None, // fullSignature
            Some(f.new_block(
                f.new_node_list(vec![f.new_return_statement(Some(
                    f.new_property_access_expression(
                        descriptor_name,
                        None,
                        f.new_identifier("value"),
                        NodeFlags::None,
                    ),
                ))]),
                false,
            )),
        )
    }

    // esdecorator.go:2604
    // Creates a GetAccessor that forwards its invocation to a PropertyDescriptor object.
    fn create_get_accessor_descriptor_forwarder(
        &self,
        modifiers: Option<P<ModifierList>>,
        name: P<Node>,
        descriptor_name: P<Node>,
    ) -> P<Node> {
        let f = self.base.factory();
        let static_only =
            self.static_only_modifier_visitor.get().unwrap().clone().visit_modifiers(modifiers);
        f.new_get_accessor_declaration(
            static_only,
            name,
            None, // typeParameters
            Some(f.new_node_list(vec![])),
            None, // type
            None, // fullSignature
            Some(f.new_block(
                f.new_node_list(vec![f.new_return_statement(Some(f.new_function_call_call(
                    f.new_property_access_expression(
                        descriptor_name,
                        None,
                        f.new_identifier("get"),
                        NodeFlags::None,
                    ),
                    Some(f.new_this_expression()),
                    &[],
                )))]),
                false,
            )),
        )
    }

    // esdecorator.go:2627
    // Creates a SetAccessor that forwards its invocation to a PropertyDescriptor object.
    fn create_set_accessor_descriptor_forwarder(
        &self,
        modifiers: Option<P<ModifierList>>,
        name: P<Node>,
        descriptor_name: P<Node>,
    ) -> P<Node> {
        let f = self.base.factory();
        let static_only =
            self.static_only_modifier_visitor.get().unwrap().clone().visit_modifiers(modifiers);
        f.new_set_accessor_declaration(
            static_only,
            name,
            None, // typeParameters
            Some(f.new_node_list(vec![f.new_parameter_declaration(
                None,
                None,
                f.new_identifier("value"),
                None,
                None,
                None,
            )])),
            None, // type
            None, // fullSignature
            Some(f.new_block(
                f.new_node_list(vec![f.new_return_statement(Some(f.new_function_call_call(
                    f.new_property_access_expression(
                        descriptor_name,
                        None,
                        f.new_identifier("set"),
                        NodeFlags::None,
                    ),
                    Some(f.new_this_expression()),
                    &[f.new_identifier("value")],
                )))]),
                false,
            )),
        )
    }

    // esdecorator.go:2651
    pub(crate) fn create_metadata(&self, name: P<Node>, class_super: Option<P<Node>>) -> P<Node> {
        let f = self.base.factory();

        let super_metadata = match class_super {
            Some(class_super) => self.create_symbol_metadata_reference(class_super),
            None => f.new_token(Kind::NullKeyword),
        };

        let object_create = f.new_call_expression(
            f.new_property_access_expression(
                f.new_identifier("Object"),
                None,
                f.new_identifier("create"),
                NodeFlags::None,
            ),
            None,
            None,
            f.new_node_list(vec![super_metadata]),
            NodeFlags::None,
        );

        let symbol_check = f.new_logical_and_expression(
            f.new_type_check(f.new_identifier("Symbol"), "function"),
            f.new_property_access_expression(
                f.new_identifier("Symbol"),
                None,
                f.new_identifier("metadata"),
                NodeFlags::None,
            ),
        );

        let conditional = f.new_conditional_expression(
            symbol_check,
            f.new_token(Kind::QuestionToken),
            object_create,
            f.new_token(Kind::ColonToken),
            f.new_void_zero_expression(),
        );

        let var_decl = f.new_variable_declaration(name, None, None, Some(conditional));
        let var_decl_list =
            f.new_variable_declaration_list(f.new_node_list(vec![var_decl]), NodeFlags::Const);
        f.new_variable_statement(None, var_decl_list)
    }

    // esdecorator.go:2686
    pub(crate) fn create_symbol_metadata(&self, target: P<Node>, value: P<Node>) -> P<Node> {
        let f = self.base.factory();

        // Object.defineProperty(target, Symbol.metadata, { configurable: true, writable: true, enumerable: true, value })
        let symbol_metadata = f.new_property_access_expression(
            f.new_identifier("Symbol"),
            None,
            f.new_identifier("metadata"),
            NodeFlags::None,
        );

        let descriptor_props = vec![
            f.new_property_assignment(
                None,
                f.new_identifier("enumerable"),
                None,
                None,
                f.new_true_expression(),
            ),
            f.new_property_assignment(
                None,
                f.new_identifier("configurable"),
                None,
                None,
                f.new_true_expression(),
            ),
            f.new_property_assignment(
                None,
                f.new_identifier("writable"),
                None,
                None,
                f.new_true_expression(),
            ),
            f.new_property_assignment(None, f.new_identifier("value"), None, None, value),
        ];
        let descriptor = f.new_object_literal_expression(f.new_node_list(descriptor_props), false);

        let define_property = f.new_call_expression(
            f.new_property_access_expression(
                f.new_identifier("Object"),
                None,
                f.new_identifier("defineProperty"),
                NodeFlags::None,
            ),
            None,
            None,
            f.new_node_list(vec![target, symbol_metadata, descriptor]),
            NodeFlags::None,
        );

        let if_statement =
            f.new_if_statement(value, f.new_expression_statement(define_property), None);
        self.base.emit_context().set_emit_flags(if_statement, EmitFlags::SingleLine);
        if_statement
    }

    // esdecorator.go:2712
    fn create_symbol_metadata_reference(&self, class_super: P<Node>) -> P<Node> {
        let f = self.base.factory();
        let symbol_metadata = f.new_property_access_expression(
            f.new_identifier("Symbol"),
            None,
            f.new_identifier("metadata"),
            NodeFlags::None,
        );
        let element_access =
            f.new_element_access_expression(class_super, None, symbol_metadata, NodeFlags::None);
        f.new_binary_expression(
            None,
            element_access,
            None,
            f.new_token(Kind::QuestionQuestionToken),
            f.new_token(Kind::NullKeyword),
        )
    }
}

// esdecorator.go:2719
pub fn inject_class_this_assignment_if_missing(
    ec: P<EmitContext>,
    f: &printer::NodeFactory,
    node: P<Node>,
    class_this: P<Node>,
) -> P<Node> {
    if class_has_class_this_assignment(ec, node) {
        return node;
    }

    // Create: static { _classThis = this; }
    let expression = f.new_assignment_expression(class_this, f.new_this_expression());
    let statement = f.new_expression_statement(expression);
    let body = f.new_block(f.new_node_list(vec![statement]), false);
    let static_block = f.new_class_static_block_declaration(None, body);
    ec.set_class_this(static_block, class_this);

    if let Some(name) = node.name() {
        ec.set_source_map_range(statement, name.loc());
    }

    let mut new_members: Vec<P<Node>> = Vec::with_capacity(1 + node.members().len());
    new_members.push(static_block);
    new_members.extend_from_slice(node.members());
    let members_list = f.new_node_list(new_members);
    members_list.loc.set(node.member_list().unwrap().loc.get());

    let updated_node = if ast::is_class_declaration(node) {
        f.update_class_declaration(
            node,
            node.modifiers(),
            node.name(),
            None,
            node.as_class_declaration().heritage_clauses(),
            members_list,
        )
    } else {
        f.update_class_expression(
            node,
            node.modifiers(),
            node.name(),
            None,
            node.as_class_expression().heritage_clauses(),
            members_list,
        )
    };
    ec.set_class_this(updated_node, class_this);
    updated_node
}
