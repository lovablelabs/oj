// Port of estransforms/classfields.go, part 2 (Go lines 1889-3618: class declarations/expressions, members,
// constructors, private-name environments, destructuring targets and the free helpers). Part 1 is classfields_1.rs.

use super::*;
use printer::{EmitFlags, PrivateIdentifierKind};

impl ClassFieldsTransformer {
    // classfields.go:1889
    pub(crate) fn visit_class_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        self.visit_in_new_class_lexical_environment(
            node,
            Self::visit_class_declaration_in_new_class_lexical_environment,
        )
    }

    // classfields.go:1893
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    fn visit_class_declaration_in_new_class_lexical_environment(
        &self,
        node: P<Node>,
        facts: classFacts,
    ) -> Option<P<Node>> {
        let class_decl = node.as_class_declaration();
        // If a class has private static fields, or a static field has a `this` or `super` reference,
        // then we need to allocate a temp variable to hold on to that reference.
        let mut pending_class_reference_assignment: Option<P<Node>> = None;
        if facts.intersects(classFacts::NeedsClassConstructorReference) {
            // If we aren't transforming class static blocks, then we can't reuse `_classThis` since in
            // `class C { ... static { _classThis = ... } }; _classThis = C` the outer assignment would occur *after*
            // class static blocks evaluate and would overwrite the replacement constructor produced by class
            // decorators.

            // If we are transforming class static blocks, then we can reuse `_classThis` since the assignment
            // will be evaluated *before* the transformed static blocks are evaluated and thus won't overwrite
            // the replacement constructor.

            if self.should_transform_private_elements_or_class_static_blocks.get()
                && self.emit_context().class_this(node).is_some()
            {
                let class_this = self.emit_context().class_this(node).unwrap();
                self.get_class_lexical_environment().class_constructor.set(Some(class_this));
                pending_class_reference_assignment = Some(
                    self.factory()
                        .new_assignment_expression(class_this, self.factory().get_local_name(node)),
                );
            } else {
                let temp = self.factory().new_temp_variable_ex(printer::AutoGenerateOptions {
                    flags: printer::GeneratedIdentifierFlags::ReservedInNestedScopes,
                    ..Default::default()
                });
                self.emit_context().add_variable_declaration(temp);
                self.get_class_lexical_environment()
                    .class_constructor
                    .set(Some(temp.clone_node(self.factory())));
                pending_class_reference_assignment = Some(
                    self.factory()
                        .new_assignment_expression(temp, self.factory().get_local_name(node)),
                );
            }
        }

        if let Some(class_this) = self.emit_context().class_this(node) {
            self.get_class_lexical_environment().class_this.set(Some(class_this));
        }

        let is_class_with_constructor_reference = self.class_contains_constructor_reference(node);

        // Register class alias BEFORE visiting members (Strada registers after, since its
        // onSubstituteNode runs at emit time; we substitute eagerly during transformation).
        let alias = self.get_class_lexical_environment().class_constructor.get();
        if is_class_with_constructor_reference && let Some(alias) = alias {
            self.class_aliases
                .borrow_mut()
                .insert(self.emit_context().most_original(Some(node)).unwrap(), alias);
        }

        let mut modifiers = self.modifier_visitor().visit_modifiers(node.modifiers());
        let heritage_clauses = self
            .heritage_clause_visitor()
            .visit_nodes(class_decl.class_like_base.heritage_clauses.get());
        let (members, members_prologue) = self.transform_class_members(node);

        let mut statements: Vec<P<Node>> = Vec::new();

        if let Some(pending_class_reference_assignment) = pending_class_reference_assignment {
            self.pending_expressions.borrow_mut().insert(0, pending_class_reference_assignment);
        }

        // Write any pending expressions from elided or moved computed property names
        if !self.pending_expressions.borrow().is_empty() {
            let pending = self.pending_expressions.borrow().clone();
            statements
                .push(self.factory().new_expression_statement(
                    self.factory().inline_expressions(&pending).unwrap(),
                ));
        }

        // A class declaration without a name needs a generated name if it has static
        // initialized properties, since those will be moved outside the class body and
        // need to reference the class by name.
        let mut name = node.name();

        if self.should_transform_initializers_using_set.get()
            || self.should_transform_private_elements_or_class_static_blocks.get()
        {
            // Emit static property assignment. Because classDeclaration is lexically evaluated,
            // it is safe to emit static property assignment after classDeclaration
            // From ES6 specification:
            //   HasLexicalDeclaration (N) : Determines if the argument identifier has a binding in this environment record that was created using
            //                               a lexical declaration such as a LexicalDeclaration or a ClassDeclaration.
            let static_properties = Self::get_static_properties_and_class_static_block(node);
            if !static_properties.is_empty() {
                if name.is_none() {
                    name = Some(self.factory().new_generated_name_for_node(node));
                }
                statements = self.add_property_or_class_static_block_statements(
                    statements,
                    &static_properties,
                    self.factory().get_local_name(node),
                );
            }
        }

        let is_export = ast::has_syntactic_modifier(node, ModifierFlags::Export);
        let is_default = ast::has_syntactic_modifier(node, ModifierFlags::Default);

        if !statements.is_empty() && is_export && is_default {
            modifiers =
                extract_modifiers(self.emit_context(), modifiers, !ModifierFlags::ExportDefault);
            let export_assignment = self.factory().new_export_assignment(
                None,
                false, /*isExportEquals*/
                None,  /*typeNode*/
                self.factory().get_local_name(node),
            );
            statements.push(export_assignment);
        }

        let updated_class = self.factory().update_class_declaration(
            node,
            modifiers,
            name,
            None, /*typeParameters*/
            heritage_clauses,
            members,
        );

        let mut result: Vec<P<Node>> = Vec::with_capacity(1 + statements.len() + 1);
        if let Some(members_prologue) = members_prologue {
            result.push(self.factory().new_expression_statement(members_prologue));
        }
        result.push(updated_class);
        result.extend(statements);
        Some(self.factory().new_syntax_list(alloc_vec(result)))
    }

    // classfields.go:2003
    pub(crate) fn visit_class_expression(&self, node: P<Node>) -> Option<P<Node>> {
        self.visit_in_new_class_lexical_environment(
            node,
            Self::visit_class_expression_in_new_class_lexical_environment,
        )
    }

    // classfields.go:2007
    fn visit_class_expression_in_new_class_lexical_environment(
        &self,
        node: P<Node>,
        facts: classFacts,
    ) -> Option<P<Node>> {
        let class_expr = node.as_class_expression();

        // If this class expression is a transformation of a decorated class declaration,
        // then we want to output the pendingExpressions as statements, not as inlined
        // expressions with the class statement.
        //
        // In this case, we use pendingStatements to produce the same output as the
        // class declaration transformation. The VariableStatement visitor will insert
        // these statements after the class expression variable statement.
        let is_decorated_class_declaration = facts.intersects(classFacts::ClassWasDecorated);

        if let Some(class_this) = self.emit_context().class_this(node) {
            self.get_class_lexical_environment().class_this.set(Some(class_this));
        }

        let mut temp: Option<P<Node>> = None;
        if facts.intersects(classFacts::NeedsClassConstructorReference) {
            if (self.should_transform_private_elements_or_class_static_blocks.get()
                || self.node_has_transform_private_static_elements_flag(node))
                && self.emit_context().class_this(node).is_some()
            {
                let class_this = self.emit_context().class_this(node).unwrap();
                self.get_class_lexical_environment().class_constructor.set(Some(class_this));
                temp = Some(class_this);
            } else {
                let t = self.factory().new_temp_variable_ex(printer::AutoGenerateOptions {
                    flags: printer::GeneratedIdentifierFlags::ReservedInNestedScopes,
                    ..Default::default()
                });
                if self.class_expression_needs_block_scoped_temp() {
                    self.emit_context().add_lexical_declaration(t);
                } else {
                    self.emit_context().add_variable_declaration(t);
                }
                self.get_class_lexical_environment()
                    .class_constructor
                    .set(Some(t.clone_node(self.factory())));
                temp = Some(t);
            }
        }

        let static_properties_or_class_static_blocks =
            Self::get_static_properties_and_class_static_block(node);
        let has_transformable_statics = (self
            .should_transform_private_elements_or_class_static_blocks
            .get()
            || self.node_has_transform_private_static_elements_flag(node))
            && static_properties_or_class_static_blocks.iter().any(|&n| {
                ast::is_class_static_block_declaration(n)
                    || ast::is_private_identifier_class_element_declaration(n)
                    || (self.should_transform_initializers.get() && ast::is_initialized_property(n))
            });

        // Pre-compute whether the class expression will need a temp variable wrapper.
        // Strada registers class aliases AFTER transformClassMembers (since onSubstituteNode runs
        // at emit time), but we must predict this before visiting members since we substitute
        // eagerly. This requires pre-detecting willHavePrivatePendingExpressions.
        let mut is_class_with_constructor_reference = false;
        let mut defer_temp_declaration = false;
        if !is_decorated_class_declaration {
            is_class_with_constructor_reference = self.class_contains_constructor_reference(node);

            // Private instance elements (fields, methods, accessors) transformed to
            // WeakMap/WeakSet will add initialization expressions to pendingExpressions
            // during transformClassMembers. Pre-detect this so we know whether the class
            // will be wrapped with a temp variable.
            let will_have_private_pending_expressions =
                self.should_transform_private_elements_or_class_static_blocks.get()
                    && node.members().iter().any(|&n| {
                        ast::is_private_identifier_class_element_declaration(n)
                            && !ast::has_static_modifier(n)
                            && self.should_transform_class_element_to_weak_map(n)
                    });
            let will_need_temp_wrapper =
                has_transformable_statics || will_have_private_pending_expressions;

            // Register class alias BEFORE visiting members (Strada registers after, since its
            // onSubstituteNode runs at emit time). Only register when the class will be wrapped
            // with a temp, matching Strada's conditional registration.
            if is_class_with_constructor_reference
                && will_need_temp_wrapper
                && self.get_class_lexical_environment().class_constructor.get().is_none()
            {
                // Create temp early so the alias is available during member visiting, even though in the Strada
                // reference the temp would be created later in the pendingExpressions branch.
                let t = self.factory().new_temp_variable_ex(printer::AutoGenerateOptions {
                    flags: printer::GeneratedIdentifierFlags::ReservedInNestedScopes,
                    ..Default::default()
                });
                temp = Some(t);
                // Defer AddVariableDeclaration to preserve Strada's variable declaration ordering.
                defer_temp_declaration = true;
                self.get_class_lexical_environment()
                    .class_constructor
                    .set(Some(t.clone_node(self.factory())));
            }
            if let Some(alias) = self.get_class_lexical_environment().class_constructor.get()
                && is_class_with_constructor_reference
                && will_need_temp_wrapper
            {
                self.class_aliases
                    .borrow_mut()
                    .insert(self.emit_context().most_original(Some(node)).unwrap(), alias);
            }
        }

        let modifiers = self.modifier_visitor().visit_modifiers(node.modifiers());
        let heritage_clauses = self
            .heritage_clause_visitor()
            .visit_nodes(class_expr.class_like_base.heritage_clauses.get());
        let (members, members_prologue) = self.transform_class_members(node);

        if defer_temp_declaration {
            if self.class_expression_needs_block_scoped_temp() {
                self.emit_context().add_lexical_declaration(temp.unwrap());
            } else {
                self.emit_context().add_variable_declaration(temp.unwrap());
            }
        }

        let class_expression = self.factory().update_class_expression(
            node,
            modifiers,
            node.name(),
            None, /*typeParameters*/
            heritage_clauses,
            members,
        );

        let mut expressions: Vec<P<Node>> = Vec::new();
        if let Some(members_prologue) = members_prologue {
            expressions.push(members_prologue);
        }

        if !is_decorated_class_declaration {
            if has_transformable_statics || !self.pending_expressions.borrow().is_empty() {
                if temp.is_none() {
                    let t = self.factory().new_temp_variable_ex(printer::AutoGenerateOptions {
                        flags: printer::GeneratedIdentifierFlags::ReservedInNestedScopes,
                        ..Default::default()
                    });
                    temp = Some(t);
                    if self.class_expression_needs_block_scoped_temp() {
                        self.emit_context().add_lexical_declaration(t);
                    } else {
                        self.emit_context().add_variable_declaration(t);
                    }
                    self.get_class_lexical_environment()
                        .class_constructor
                        .set(Some(t.clone_node(self.factory())));
                    if is_class_with_constructor_reference {
                        self.class_aliases.borrow_mut().insert(
                            self.emit_context().most_original(Some(node)).unwrap(),
                            self.get_class_lexical_environment().class_constructor.get().unwrap(),
                        );
                    }
                }
                let temp = temp.unwrap();

                expressions.push(self.factory().new_assignment_expression(temp, class_expression));

                // Add any pending expressions leftover from elided or relocated computed property names
                expressions.extend(self.pending_expressions.borrow().iter().copied());

                expressions.extend(
                    self.generate_initialized_property_expressions_or_class_static_block(
                        &static_properties_or_class_static_blocks,
                        temp,
                    ),
                );
                expressions.push(temp.clone_node(self.factory()));
            } else {
                expressions.push(class_expression);
            }
        } else if has_transformable_statics || !self.pending_expressions.borrow().is_empty() {
            // Decorated class declaration path: emit static properties as separate statements
            // via pendingStatements, matching the class declaration output structure.

            // Write any pending expressions from elided or moved computed property names
            if !self.pending_expressions.borrow().is_empty() {
                let pending = self.pending_expressions.borrow().clone();
                for expr in pending {
                    let stmt = self.factory().new_expression_statement(expr);
                    self.pending_statements.borrow_mut().push(stmt);
                }
            }

            // Emit static properties as statements (via pendingStatements) using the class's
            // internal name as the receiver, matching the class declaration output structure.
            if !static_properties_or_class_static_blocks.is_empty() {
                let mut class_this_or_name = self.emit_context().class_this(node);
                if class_this_or_name.is_none() {
                    class_this_or_name = Some(self.factory().get_local_name(node));
                }
                let pending_statements = std::mem::take(&mut *self.pending_statements.borrow_mut());
                let pending_statements = self.add_property_or_class_static_block_statements(
                    pending_statements,
                    &static_properties_or_class_static_blocks,
                    class_this_or_name.unwrap(),
                );
                *self.pending_statements.borrow_mut() = pending_statements;
            }

            if let Some(temp) = temp {
                expressions.push(self.factory().new_assignment_expression(temp, class_expression));
            } else if self.should_transform_private_elements_or_class_static_blocks.get()
                && self.emit_context().class_this(node).is_some()
            {
                expressions.push(self.factory().new_assignment_expression(
                    self.emit_context().class_this(node).unwrap(),
                    class_expression,
                ));
            } else {
                expressions.push(class_expression);
            }
        } else {
            expressions.push(class_expression);
        }

        if expressions.len() > 1 {
            self.emit_context().add_emit_flags(class_expression, printer::EmitFlags::Indented);
            for &expr in &expressions {
                self.emit_context().add_emit_flags(expr, printer::EmitFlags::StartOnNewLine);
            }
        }
        self.factory().inline_expressions(&expressions)
    }

    // classfields.go:2181
    pub(crate) fn visit_class_static_block_declaration(&self, node: P<Node>) -> Option<P<Node>> {
        if !self.should_transform_private_elements_or_class_static_blocks.get() {
            return self.visitor().visit_each_child(Some(node));
        }
        // ClassStaticBlockDeclaration for classes are transformed in visitClassDeclaration/visitClassExpression.
        None
    }

    // classfields.go:2194
    // visitThisExpression replaces Strada's substituteThisExpression / onSubstituteNode.
    // Strada substitutes `this` at emit time; we do it eagerly during transformation.
    //
    // The Strada noSubstitution set (ensureDynamicThisIfNeeded) is not needed because
    // transformAutoAccessor() passes the receiver directly rather than emitting `this`.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    pub(crate) fn visit_this_expression(&self, node: P<Node>) -> Option<P<Node>> {
        if self.inside_computed_property_name.get()
            && self.should_transform_this_in_static_initializers.get()
            && self.lexical_environment.get().is_some_and(|l| l.data.get().is_some())
        {
            // Don't replace `this` in computed property names for ES-decorated classes.
            // The esDecorator transformer wraps them in an arrow IIFE where `this` already
            // refers to the correct outer scope.
            if (!self
                .lexical_environment
                .get()
                .unwrap()
                .data
                .get()
                .unwrap()
                .facts
                .get()
                .intersects(classFacts::ClassWasDecorated)
                || self.legacy_decorators)
                && let Some(class_this) = self.try_get_class_this_no_container()
            {
                return Some(class_this);
            }
        }
        if self.should_transform_this_in_static_initializers.get()
            && self.current_class_element.get().is_some_and(|e| {
                ast::is_class_static_block_declaration(e)
                    || (ast::is_property_declaration(e) && ast::has_static_modifier(e))
            })
            && self.lexical_environment.get().is_some_and(|l| l.data.get().is_some())
        {
            if let Some(class_this) = self.try_get_class_this_no_container() {
                return Some(class_this);
            }
            // When the class was decorated with legacy decorators and no class constructor
            // reference is available, the decorator may replace the constructor, so `this`
            // cannot reliably point to the class. Use `(void 0)` instead.
            if self
                .lexical_environment
                .get()
                .unwrap()
                .data
                .get()
                .unwrap()
                .facts
                .get()
                .intersects(classFacts::ClassWasDecorated)
                && self.legacy_decorators
            {
                return Some(
                    self.factory()
                        .new_parenthesized_expression(self.factory().new_void_zero_expression()),
                );
            }
        }
        Some(node)
    }

    // classfields.go:2223
    fn transform_class_members(&self, node: P<Node>) -> (P<NodeList>, Option<P<Node>>) {
        let mut prologue: Option<P<Node>> = None;
        let should_transform_private_static_elements_in_class = self
            .emit_context()
            .emit_flags(node)
            .intersects(printer::EmitFlags::TransformPrivateStaticElements);

        // Declare private names
        if self.should_transform_private_elements_or_class_static_blocks.get()
            || self.should_transform_private_static_elements_in_file.get()
        {
            for &member in node.members() {
                if ast::is_private_identifier_class_element_declaration(member) {
                    if self.should_transform_class_element_to_weak_map(member) {
                        self.add_private_identifier_to_environment(member);
                    } else {
                        let env = self.get_private_identifier_environment();
                        self.set_private_identifier(
                            env,
                            member.name().unwrap(),
                            new_private_identifier_info(PrivateIdentifierKind::Untransformed),
                        );
                    }
                }
            }

            if self.should_transform_private_elements_or_class_static_blocks.get()
                && !Self::get_private_instance_methods_and_accessors(node).is_empty()
            {
                self.create_brand_check_weak_set_for_private_methods();
            }

            if self.should_transform_auto_accessors_in_current_class() {
                for &member in node.members() {
                    if ast::is_auto_accessor_property_declaration(member) {
                        let storage_name = self.factory().new_generated_private_name_for_node_ex(
                            member.name().unwrap(),
                            printer::AutoGenerateOptions {
                                suffix: alloc_str("_accessor_storage"),
                                ..Default::default()
                            },
                        );
                        if self.should_transform_private_elements_or_class_static_blocks.get()
                            || should_transform_private_static_elements_in_class
                                && ast::has_static_modifier(member)
                        {
                            self.add_private_identifier_property_declaration_to_environment(
                                member,
                                storage_name,
                            );
                        } else {
                            let env = self.get_private_identifier_environment();
                            // Only register as untransformed if it hasn't already been registered
                            // by the first loop (e.g., if esDecorators expanded a private auto-accessor
                            // into a backing field with the same generated name).
                            if self.get_private_identifier(env, storage_name).is_none() {
                                self.set_private_identifier(
                                    env,
                                    storage_name,
                                    new_private_identifier_info(
                                        PrivateIdentifierKind::Untransformed,
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }

        let mut members = self.class_element_visitor().visit_nodes(node.member_list()).unwrap();

        // Create a synthetic constructor if necessary
        let mut synthetic_constructor: Option<P<Node>> = None;
        if !members.nodes().iter().any(|&n| ast::is_constructor_declaration(n)) {
            synthetic_constructor = self.transform_constructor(None, node);
        }

        // If there are pending expressions create a class static block in which to evaluate them, but only if
        // class static blocks are not also being transformed. This block will be injected at the top of the class
        // to ensure that expressions from computed property names are evaluated before any other static
        // initializers.
        let mut synthetic_static_block: Option<P<Node>> = None;
        if !self.should_transform_private_elements_or_class_static_blocks.get()
            && !self.pending_expressions.borrow().is_empty()
        {
            let pending = self.pending_expressions.borrow().clone();
            let mut statement = self
                .factory()
                .new_expression_statement(self.factory().inline_expressions(&pending).unwrap());
            if statement.subtree_facts().intersects(ast::SubtreeFacts::ContainsLexicalThisOrSuper) {
                // If there are `this` or `super` references from computed property names, shift the expression
                // into an arrow function to be evaluated in the outer scope so that `this` and `super` are
                // properly captured.
                let temp = self.factory().new_temp_variable();
                self.emit_context().add_variable_declaration(temp);
                let arrow = self.factory().new_arrow_function(
                    None,                                     /*modifiers*/
                    None,                                     /*typeParameters*/
                    Some(self.factory().new_node_list(vec![])), /*parameters*/
                    None,                                     /*returnType*/
                    None,                                     /*fullSignature*/
                    Some(self.factory().new_token(Kind::EqualsGreaterThanToken)), /*equalsGreaterThanToken*/
                    Some(self.factory().new_block(self.factory().new_node_list(vec![statement]), false /*multiline*/)),
                );
                prologue = Some(self.factory().new_assignment_expression(temp, arrow));
                statement =
                    self.factory().new_expression_statement(self.factory().new_call_expression(
                        temp,
                        None, /*questionDotToken*/
                        None, /*typeArguments*/
                        self.factory().new_node_list(vec![]),
                        NodeFlags::None,
                    ));
            }

            let block = self
                .factory()
                .new_block(self.factory().new_node_list(vec![statement]), false /*multiline*/);
            synthetic_static_block = Some(
                self.factory().new_class_static_block_declaration(None /*modifiers*/, block),
            );
            self.pending_expressions.borrow_mut().clear();
        }

        // If we created a synthetic constructor or class static block, add them to the visited members
        if synthetic_constructor.is_some() || synthetic_static_block.is_some() {
            let nodes = members.nodes();
            let mut members_array: Vec<P<Node>> = Vec::with_capacity(nodes.len() + 2);

            // Find and preserve classThis assignment block and named evaluation helper block at the top
            let class_this_idx =
                nodes.iter().position(|&n| is_class_this_assignment_block(self.emit_context(), n));
            let named_eval_idx = nodes
                .iter()
                .position(|&n| is_class_named_evaluation_helper_block(self.emit_context(), n));

            if let Some(class_this_idx) = class_this_idx {
                members_array.push(nodes[class_this_idx]);
            }
            if let Some(named_eval_idx) = named_eval_idx {
                members_array.push(nodes[named_eval_idx]);
            }
            if let Some(synthetic_constructor) = synthetic_constructor {
                members_array.push(synthetic_constructor);
            }
            if let Some(synthetic_static_block) = synthetic_static_block {
                members_array.push(synthetic_static_block);
            }

            members_array.extend(nodes.iter().enumerate().filter_map(|(i, &member)| {
                if Some(i) != class_this_idx && Some(i) != named_eval_idx {
                    Some(member)
                } else {
                    None
                }
            }));
            members = self.factory().new_node_list(members_array);
            members.loc.set(node.member_list().unwrap().loc.get());
        }

        (members, prologue)
    }

    // classfields.go:2348
    fn create_brand_check_weak_set_for_private_methods(&self) {
        let env = self.get_private_identifier_environment();
        let weak_set_name = env.data.weak_set_name.get();
        assert!(
            weak_set_name.is_some(),
            "weakSetName should be set in private identifier environment"
        );

        self.add_pending_expressions(&[self.factory().new_assignment_expression(
            weak_set_name.unwrap(),
            self.factory().new_new_expression(
                self.factory().new_identifier("WeakSet"),
                None, /*typeArguments*/
                Some(self.factory().new_node_list(vec![])),
            ),
        )]);
    }

    // classfields.go:2365
    pub(crate) fn transform_constructor(
        &self,
        constructor: Option<P<Node>>,
        container: P<Node>,
    ) -> Option<P<Node>> {
        // NOTE: The Strada reference pre-visits the constructor via `visitNode(constructor, visitor)` before
        // checking WillHoistInitializersToConstructor. This is not done here because Go's variable environment
        // (StartVariableEnvironment/EndAndMergeVariableEnvironment) is scoped inside transformConstructorBody.
        // Pre-visiting would hoist variables outside that scope, causing them to appear after field initializers
        // instead of before. Instead, we visit parameters and body separately within the correct scopes.
        if self.lexical_environment.get().is_none_or(|l| {
            l.data.get().is_none_or(|d| {
                !d.facts.get().intersects(classFacts::WillHoistInitializersToConstructor)
            })
        }) {
            if let Some(constructor) = constructor {
                return self.visitor().visit_each_child(Some(constructor));
            }
            return None;
        }

        let extends_clause_element = ast::get_class_extends_heritage_element(container);
        let is_derived_class = extends_clause_element.is_some_and(|e| {
            ast::skip_outer_expressions(e.expression().unwrap(), OuterExpressionKinds::All).kind()
                != Kind::NullKeyword
        });

        let mut parameters: Option<P<NodeList>> = None;
        if let Some(constructor) = constructor {
            parameters = self.visitor().visit_nodes(constructor.parameter_list());
        }

        let body = self.transform_constructor_body(container, constructor, is_derived_class);
        let Some(body) = body else {
            if let Some(constructor) = constructor {
                return self.visitor().visit_each_child(Some(constructor));
            }
            return None;
        };

        if let Some(constructor) = constructor {
            assert!(parameters.is_some());
            return Some(self.factory().update_constructor_declaration(
                constructor,
                None, /*modifiers*/
                None, /*typeParameters*/
                parameters,
                None, /*returnType*/
                None, /*fullSignature*/
                Some(body),
            ));
        }

        if parameters.is_none() {
            parameters = Some(self.factory().new_node_list(vec![]));
        }

        let result = self.factory().new_constructor_declaration(
            None, /*modifiers*/
            None, /*typeParameters*/
            parameters,
            None, /*returnType*/
            None, /*fullSignature*/
            Some(body),
        );
        result.set_loc(container.loc());
        Some(result)
    }

    // classfields.go:2424
    fn transform_constructor_body_worker(
        &self,
        mut statements_out: Vec<P<Node>>,
        statements_in: &'static [P<Node>],
        mut statement_offset: usize,
        super_path: &[usize],
        super_path_depth: usize,
        initializer_statements: &[P<Node>],
        constructor: Option<P<Node>>,
    ) -> Vec<P<Node>> {
        let super_statement_index = super_path[super_path_depth];
        let super_statement = statements_in[super_statement_index];

        // Visit statements before super
        let (visited, _) =
            self.visitor().visit_slice(&statements_in[statement_offset..super_statement_index]);
        statements_out.extend_from_slice(visited);
        statement_offset = super_statement_index + 1;

        if ast::is_try_statement(super_statement) {
            let try_stmt = super_statement.as_try_statement();
            let try_block_node = try_stmt.try_block;
            let try_block = try_block_node.as_block();
            let try_block_statements = self.transform_constructor_body_worker(
                Vec::new(),
                try_block.statements.nodes(),
                0, /*statementOffset*/
                super_path,
                super_path_depth + 1,
                initializer_statements,
                constructor,
            );
            let try_statement_list = self.factory().new_node_list(try_block_statements);
            try_statement_list.loc.set(try_block.statements.loc.get());

            let catch_clause = self.visitor().visit_node(try_stmt.catch_clause);
            let finally_block = self.visitor().visit_node(try_stmt.finally_block);

            let updated = self.factory().update_try_statement(
                super_statement,
                self.factory().update_block(
                    try_block_node,
                    try_statement_list,
                    try_block.multi_line,
                ),
                catch_clause,
                finally_block,
            );
            statements_out.push(updated);
        } else {
            let (visited, _) = self
                .visitor()
                .visit_slice(&statements_in[super_statement_index..=super_statement_index]);
            statements_out.extend_from_slice(visited);

            // Add the property initializers. Transforms this:
            //
            //  public x = 1;
            //
            // Into this:
            //
            //  constructor() {
            //      this.x = 1;
            //  }
            //
            // If we do useDefineForClassFields, they'll be converted elsewhere.
            // We instead *remove* them from the transformed output at this stage.

            // parameter-property assignments should occur immediately after the prologue and `super()`,
            // so only count the statements that immediately follow.
            while statement_offset < statements_in.len() {
                let stmt = statements_in[statement_offset];
                let orig = self.emit_context().most_original(Some(stmt)).unwrap();
                if ast::is_parameter_property_declaration(orig, constructor.unwrap()) {
                    statement_offset += 1;
                } else {
                    break;
                }
            }

            statements_out.extend_from_slice(initializer_statements);
        }

        // Visit remaining statements
        let (visited2, _) = self.visitor().visit_slice(&statements_in[statement_offset..]);
        statements_out.extend_from_slice(visited2);
        statements_out
    }

    // classfields.go:2503
    fn transform_constructor_body(
        &self,
        container: P<Node>,
        constructor: Option<P<Node>>,
        is_derived_class: bool,
    ) -> Option<P<Node>> {
        let instance_properties = Self::get_properties(
            container, false, /*requireInitializer*/
            false, /*isStatic*/
        );
        let mut properties = instance_properties.clone();
        if !self.compiler_options.get_use_define_for_class_fields() {
            properties.retain(|&prop| {
                prop.initializer().is_some()
                    || ast::is_private_identifier(prop.name().unwrap())
                    || ast::has_accessor_modifier(prop)
            });
        }

        let private_methods_and_accessors =
            Self::get_private_instance_methods_and_accessors(container);
        let needs_constructor_body =
            !properties.is_empty() || !private_methods_and_accessors.is_empty();

        // Only generate synthetic constructor when there are property initializers to move.
        if constructor.is_none() && !needs_constructor_body {
            return self.emit_context().visit_function_body(None, &mut self.visitor());
        }

        self.emit_context().start_variable_environment();

        let needs_synthetic_constructor = constructor.is_none() && is_derived_class;
        let mut statements: Vec<P<Node>> = Vec::new();

        // Add the property initializers. Transforms this:
        //
        //  public x = 1;
        //
        // Into this:
        //
        //  constructor() {
        //      this.x = 1;
        //  }
        //
        let mut initializer_statements: Vec<P<Node>> = Vec::new();
        let receiver = self.factory().new_this_expression();

        // private methods can be called in property initializers, they should execute first
        initializer_statements = self.add_instance_method_statements(
            initializer_statements,
            &private_methods_and_accessors,
            receiver,
        );

        if let Some(constructor) = constructor {
            let parameter_properties: Vec<P<Node>> = instance_properties
                .iter()
                .copied()
                .filter(|&prop| {
                    ast::is_parameter_property_declaration(
                        self.emit_context().most_original(Some(prop)).unwrap(),
                        constructor,
                    )
                })
                .collect();
            let non_parameter_properties: Vec<P<Node>> = properties
                .iter()
                .copied()
                .filter(|&prop| {
                    !ast::is_parameter_property_declaration(
                        self.emit_context().most_original(Some(prop)).unwrap(),
                        constructor,
                    )
                })
                .collect();
            initializer_statements = self.add_property_or_class_static_block_statements(
                initializer_statements,
                &parameter_properties,
                receiver,
            );
            initializer_statements = self.add_property_or_class_static_block_statements(
                initializer_statements,
                &non_parameter_properties,
                receiver,
            );
        } else {
            initializer_statements = self.add_property_or_class_static_block_statements(
                initializer_statements,
                &properties,
                receiver,
            );
        }

        if let Some(body_node) = constructor.and_then(|c| c.body()) {
            let body = body_node.as_block();

            // Copy prologue
            statements.extend(
                body.statements
                    .nodes()
                    .iter()
                    .copied()
                    .take_while(|&stmt| ast::is_prologue_directive(stmt)),
            );
            let mut statement_offset = statements.len();

            let super_path =
                find_super_statement_index_path(body.statements.nodes(), statement_offset);
            if super_path.is_empty() {
                // parameter-property assignments should occur immediately after the prologue and `super()`,
                // so only count the statements that immediately follow.
                while statement_offset < body.statements.nodes().len() {
                    let stmt = body.statements.nodes()[statement_offset];
                    let orig = self.emit_context().most_original(Some(stmt)).unwrap();
                    if ast::is_parameter_property_declaration(orig, constructor.unwrap()) {
                        statement_offset += 1;
                    } else {
                        break;
                    }
                }
                statements.extend_from_slice(&initializer_statements);
                let (visited, _) =
                    self.visitor().visit_slice(&body.statements.nodes()[statement_offset..]);
                statements.extend_from_slice(visited);
            } else {
                statements = self.transform_constructor_body_worker(
                    statements,
                    body.statements.nodes(),
                    statement_offset,
                    &super_path,
                    0,
                    &initializer_statements,
                    constructor,
                );
            }
        } else {
            if needs_synthetic_constructor {
                // Add a synthetic `super` call:
                //
                //  super(...arguments);
                //
                let super_call =
                    self.factory().new_expression_statement(self.factory().new_call_expression(
                        self.factory().new_keyword_expression(Kind::SuperKeyword),
                        None, /*typeArguments*/
                        None, /*questionDotToken*/
                        self.factory().new_node_list(vec![
                            self.factory()
                                .new_spread_element(self.factory().new_identifier("arguments")),
                        ]),
                        NodeFlags::None,
                    ));
                statements.push(super_call);
            }
            statements.extend_from_slice(&initializer_statements);
        }

        statements = self.emit_context().end_and_merge_variable_environment(&statements);

        if statements.is_empty() && constructor.is_none() {
            return None;
        }

        let multi_line: bool;
        if let Some(body) = constructor
            .and_then(|c| c.body())
            .filter(|b| b.as_block().statements.nodes().len() >= statements.len())
        {
            multi_line = body.as_block().multi_line;
        } else {
            multi_line = !statements.is_empty();
        }

        let statement_list = self.factory().new_node_list(statements);
        if let Some(body) = constructor.and_then(|c| c.body()) {
            statement_list.loc.set(body.as_block().statements.loc.get());
        } else {
            statement_list.loc.set(container.member_list().unwrap().loc.get());
        }

        let block = self.factory().new_block(statement_list, multi_line);
        if let Some(body) = constructor.and_then(|c| c.body()) {
            block.set_loc(body.loc());
        }
        Some(block)
    }

    // classfields.go:2637
    // addPropertyOrClassStaticBlockStatements generates assignment statements for property initializers.
    fn add_property_or_class_static_block_statements(
        &self,
        mut statements: Vec<P<Node>>,
        properties: &[P<Node>],
        receiver: P<Node>,
    ) -> Vec<P<Node>> {
        statements.extend(properties.iter().filter_map(|&property| {
            if ast::is_static(property)
                && !self.should_transform_private_elements_or_class_static_blocks.get()
            {
                return None;
            }
            self.transform_property_or_class_static_block(property, receiver)
        }));
        statements
    }

    // classfields.go:2650
    pub(crate) fn transform_property_or_class_static_block(
        &self,
        property: P<Node>,
        receiver: P<Node>,
    ) -> Option<P<Node>> {
        let expression: Option<P<Node>> = if ast::is_class_static_block_declaration(property) {
            self.set_current_class_element_and(
                Some(property),
                Self::transform_class_static_block_declaration,
                property,
            )
        } else {
            self.transform_property(property, receiver)
        };
        let expression = expression?;

        let statement = self.factory().new_expression_statement(expression);
        self.emit_context().set_original(statement, property);
        self.emit_context().add_emit_flags(
            statement,
            self.emit_context().emit_flags(property) & EmitFlags::NoComments,
        );
        self.emit_context().set_comment_range(statement, property.loc());

        let property_original_node = self.emit_context().most_original(Some(property)).unwrap();
        if ast::is_parameter_declaration(property_original_node) {
            self.emit_context().set_source_map_range(statement, property_original_node.loc());
            self.emit_context().add_emit_flags(statement, EmitFlags::NoComments);
        } else {
            self.emit_context()
                .set_source_map_range(statement, move_range_past_modifiers(property));
        }

        // `setOriginalNode` *copies* the `emitNode` from `property`, so now both
        // `statement` and `expression` have a copy of the synthesized comments.
        // Drop the comments from expression to avoid printing them twice.
        self.emit_context().set_synthetic_leading_comments(expression, Vec::new());
        self.emit_context().set_synthetic_trailing_comments(expression, Vec::new());

        // If the property was originally an auto-accessor, don't emit comments here since they will be attached to
        // the synthesized getter.
        if ast::has_accessor_modifier(property_original_node) {
            self.emit_context().add_emit_flags(statement, EmitFlags::NoComments);
        }

        Some(statement)
    }

    // classfields.go:2690
    // generateInitializedPropertyExpressionsOrClassStaticBlock generates assignment expressions for property initializers.
    fn generate_initialized_property_expressions_or_class_static_block(
        &self,
        properties_or_class_static_blocks: &[P<Node>],
        receiver: P<Node>,
    ) -> Vec<P<Node>> {
        let mut expressions: Vec<P<Node>> = Vec::new();
        expressions.extend(properties_or_class_static_blocks.iter().filter_map(|&property| {
            let expression: Option<P<Node>> = if ast::is_class_static_block_declaration(property) {
                self.set_current_class_element_and(
                    Some(property),
                    Self::transform_class_static_block_declaration,
                    property,
                )
            } else {
                self.transform_property(property, receiver)
            };
            let expression = expression?;
            self.emit_context().set_original_ex(expression, property, true /*allowOverwrite*/);
            self.emit_context().assign_comment_and_source_map_ranges(expression, property);
            Some(expression)
        }));
        expressions
    }

    // classfields.go:2713
    // transformProperty transforms a property initializer into an assignment expression.
    fn transform_property(&self, property: P<Node>, receiver: P<Node>) -> Option<P<Node>> {
        let saved_current_class_element = self.current_class_element.get();
        let transformed = self.transform_property_worker(property, receiver);
        if let Some(transformed) = transformed
            && ast::has_static_modifier(property)
        {
            self.emit_context().add_emit_flags(transformed, EmitFlags::NoLexicalThis);
        }
        if let Some(transformed) = transformed
            && ast::has_static_modifier(property)
            && self
                .lexical_environment
                .get()
                .is_some_and(|l| l.data.get().is_some_and(|d| d.facts.get() != classFacts::None))
        {
            // capture the lexical environment for the member
            self.emit_context().set_original(transformed, property);
            self.emit_context().set_source_map_range(
                transformed,
                self.emit_context().source_map_range(property.name().unwrap()),
            );
        }
        self.current_class_element.set(saved_current_class_element);
        transformed
    }

    // classfields.go:2729
    fn transform_property_worker(
        &self,
        mut property: P<Node>,
        receiver: P<Node>,
    ) -> Option<P<Node>> {
        // We generate a name here in order to reuse the value cached by the relocated computed name expression (which uses the same generated name)
        let emit_assignment = !self.compiler_options.get_use_define_for_class_fields();

        if is_named_evaluation_and(
            self.emit_context(),
            property,
            Some(&|n| self.is_anonymous_class_needing_assigned_name(n)),
        ) {
            property = transform_named_evaluation(self.emit_context(), property, false, "");
        }

        let mut property_name = property.name().unwrap();
        if ast::has_accessor_modifier(property) {
            property_name = self.factory().new_generated_private_name_for_node_ex(
                property.name().unwrap(),
                printer::AutoGenerateOptions {
                    suffix: alloc_str("_accessor_storage"),
                    ..Default::default()
                },
            );
        } else if ast::is_computed_property_name(property_name)
            && !is_simple_inlineable_expression(property_name.expression().unwrap())
        {
            property_name = self.factory().update_computed_property_name(
                property_name,
                self.factory().new_generated_name_for_node(property_name),
            );
        }

        if ast::has_static_modifier(property) {
            self.current_class_element.set(Some(property));
        }

        if ast::is_private_identifier(property_name)
            && self.should_transform_class_element_to_weak_map(property)
        {
            let info = self.access_private_identifier(property_name);
            if let Some(info) = info {
                if info.kind == PrivateIdentifierKind::Field {
                    if !info.is_static {
                        return Some(create_private_instance_field_initializer(
                            self.factory(),
                            receiver,
                            self.visitor().visit_node(property.initializer()),
                            info.brand_check_identifier.unwrap(),
                        ));
                    }
                    return Some(create_private_static_field_initializer(
                        self.factory(),
                        info.variable_name.unwrap(),
                        self.visitor().visit_node(property.initializer()),
                    ));
                }
                return None;
            }
            panic!("Undeclared private name for property declaration.");
        }

        if (ast::is_private_identifier(property_name) || ast::has_static_modifier(property))
            && property.initializer().is_none()
        {
            return None;
        }

        // TODO: can we get rid of this original checking and better coordinate with runtimesyntax?
        if ast::has_abstract_modifier(self.emit_context().most_original(Some(property)).unwrap()) {
            return None;
        }

        let mut initializer = self.visitor().visit_node(property.initializer());
        let property_original_node = self.emit_context().most_original(Some(property)).unwrap();
        if property_original_node
            .parent()
            .is_some_and(|p| ast::is_parameter_property_declaration(property_original_node, p))
            && ast::is_identifier(property_name)
        {
            // A parameter-property declaration always overrides the initializer. The only time a parameter-property
            // declaration *should* have an initializer is when decorators have added initializers that need to run before
            // any other initializer
            let local_name = property_name.clone_node(self.factory());
            if let Some(mut init) = initializer {
                // unwrap `(__runInitializers(this, _instanceExtraInitializers), void 0)`
                if ast::is_parenthesized_expression(init)
                    && ast::is_comma_expression(init.expression().unwrap())
                    && self.emit_context().is_call_to_helper(
                        init.expression().unwrap().as_binary_expression().left,
                        "__runInitializers",
                    )
                    && ast::is_void_expression(
                        init.expression().unwrap().as_binary_expression().right.get(),
                    )
                    && ast::is_numeric_literal(
                        init.expression()
                            .unwrap()
                            .as_binary_expression()
                            .right
                            .get()
                            .expression()
                            .unwrap(),
                    )
                {
                    init = init.expression().unwrap().as_binary_expression().left;
                }
                initializer = self.factory().inline_expressions(&[init, local_name]);
            } else {
                initializer = Some(local_name);
            }
            self.emit_context()
                .add_emit_flags(property_name, EmitFlags::NoComments | EmitFlags::NoSourceMap);
            self.emit_context()
                .set_source_map_range(local_name, property_original_node.name().unwrap().loc());
            self.emit_context().add_emit_flags(local_name, EmitFlags::NoComments);
        } else if initializer.is_none() {
            initializer = Some(self.factory().new_void_zero_expression());
        }
        let initializer = initializer.unwrap();

        if emit_assignment || ast::is_private_identifier(property_name) {
            let member_access = create_member_access_for_property_name(
                self.factory(),
                self.emit_context(),
                receiver,
                property_name,
                property_name,
            );
            self.emit_context().add_emit_flags(member_access, EmitFlags::NoLeadingComments);
            return Some(self.factory().new_assignment_expression(member_access, initializer));
        }

        // useDefineForClassFields: Object.defineProperty
        let name: P<Node> = if ast::is_computed_property_name(property_name) {
            property_name.expression().unwrap()
        } else if ast::is_identifier(property_name) {
            self.factory().new_string_literal(alloc_str(property_name.text()), TokenFlags::None)
        } else {
            property_name
        };
        let descriptor = self.factory().new_object_literal_expression(
            self.factory().new_node_list(vec![
                self.factory().new_property_assignment(
                    None,
                    self.factory().new_identifier("enumerable"),
                    None,
                    None,
                    self.factory().new_true_expression(),
                ),
                self.factory().new_property_assignment(
                    None,
                    self.factory().new_identifier("configurable"),
                    None,
                    None,
                    self.factory().new_true_expression(),
                ),
                self.factory().new_property_assignment(
                    None,
                    self.factory().new_identifier("writable"),
                    None,
                    None,
                    self.factory().new_true_expression(),
                ),
                self.factory().new_property_assignment(
                    None,
                    self.factory().new_identifier("value"),
                    None,
                    None,
                    initializer,
                ),
            ]),
            true,
        );
        Some(self.factory().new_object_define_property_call(receiver, name, descriptor))
    }

    // classfields.go:2836
    // addInstanceMethodStatements generates brand-check initializer for private methods.
    fn add_instance_method_statements(
        &self,
        mut statements: Vec<P<Node>>,
        methods: &[P<Node>],
        receiver: P<Node>,
    ) -> Vec<P<Node>> {
        if !self.should_transform_private_elements_or_class_static_blocks.get()
            || methods.is_empty()
        {
            return statements;
        }

        let env = self.get_private_identifier_environment();
        let weak_set_name = env.data.weak_set_name.get();
        assert!(
            weak_set_name.is_some(),
            "weakSetName should be set in private identifier environment"
        );

        statements.push(self.factory().new_expression_statement(
            create_private_instance_method_initializer(
                self.factory(),
                receiver,
                weak_set_name.unwrap(),
            ),
        ));
        statements
    }

    // classfields.go:2853
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    pub(crate) fn visit_invalid_super_property(&self, node: P<Node>) -> Option<P<Node>> {
        if ast::is_property_access_expression(node) {
            return Some(self.factory().update_property_access_expression(
                node,
                self.factory().new_void_zero_expression(),
                None,
                node.name().unwrap(),
                node.flags(),
            ));
        }
        Some(
            self.factory().update_element_access_expression(
                node,
                self.factory().new_void_zero_expression(),
                None,
                self.visitor()
                    .visit_node(Some(node.as_element_access_expression().argument_expression))
                    .unwrap(),
                node.flags(),
            ),
        )
    }

    // classfields.go:2876
    // getPropertyNameExpressionIfNeeded transforms a computed property name, then either returns an expression
    // which caches the value of the result or the expression itself if the value is either unused or safe to
    // inline into multiple locations.
    // shouldHoist indicates whether the expression needs to be reused (i.e., for an initializer or a decorator).
    pub(crate) fn get_property_name_expression_if_needed(
        &self,
        name: P<Node>,
        should_hoist: bool,
    ) -> Option<P<Node>> {
        if !ast::is_computed_property_name(name) {
            return None;
        }
        let cache_assignment =
            find_computed_property_name_cache_assignment(self.emit_context(), name);
        // Switch to outer lex env for computed property name expressions, matching
        // Strada reference's onEmitNode behavior for ComputedPropertyName.
        let saved_lexical_environment = self.lexical_environment.get();
        let saved_inside_computed_property_name = self.inside_computed_property_name.get();
        self.inside_computed_property_name.set(true);
        if let Some(previous) = self.lexical_environment.get().and_then(|l| l.previous) {
            self.lexical_environment.set(Some(previous));
        }
        let expression = self.visitor().visit_node(name.expression()).unwrap();
        self.lexical_environment.set(saved_lexical_environment);
        self.inside_computed_property_name.set(saved_inside_computed_property_name);
        let inner_expression = ast::skip_partially_emitted_expressions(expression);
        let inlinable = is_simple_inlineable_expression(inner_expression);
        let already_transformed = cache_assignment.is_some()
            || (ast::is_assignment_expression(
                inner_expression,
                true, /*excludeCompoundAssignment*/
            ) && ast::is_identifier(inner_expression.as_binary_expression().left)
                && is_generated_identifier(
                    self.emit_context(),
                    inner_expression.as_binary_expression().left,
                ));
        if !already_transformed && !inlinable && should_hoist {
            let generated_name = self.factory().new_generated_name_for_node(name);
            if self.requires_block_scoped_var() {
                self.emit_context().add_lexical_declaration(generated_name);
            } else {
                self.emit_context().add_variable_declaration(generated_name);
            }
            return Some(self.factory().new_assignment_expression(generated_name, expression));
        }
        if inlinable || ast::is_identifier(inner_expression) {
            return None;
        }
        Some(expression)
    }

    // classfields.go:2910
    pub(crate) fn start_class_lexical_environment(&self) {
        self.lexical_environment.set(Some(P::new(ClassLexicalEnv {
            previous: self.lexical_environment.get(),
            data: Cell::new(None),
            private_env: Cell::new(None),
        })));
    }

    // classfields.go:2914
    pub(crate) fn end_class_lexical_environment(&self) {
        self.lexical_environment.set(self.lexical_environment.get().unwrap().previous);
    }

    // classfields.go:2918
    pub(crate) fn get_class_lexical_environment(&self) -> P<ClassLexicalEnvironment> {
        assert!(self.lexical_environment.get().is_some());
        let lex = self.lexical_environment.get().unwrap();
        if lex.data.get().is_none() {
            lex.data.set(Some(P::new(ClassLexicalEnvironment::default())));
        }
        lex.data.get().unwrap()
    }

    // classfields.go:2926
    pub(crate) fn get_private_identifier_environment(&self) -> P<PrivateEnvironment> {
        assert!(self.lexical_environment.get().is_some());
        let lex = self.lexical_environment.get().unwrap();
        if lex.private_env.get().is_none() {
            lex.private_env.set(Some(P::new(PrivateEnvironment::default())));
        }
        lex.private_env.get().unwrap()
    }

    // classfields.go:2936
    pub(crate) fn add_pending_expressions(&self, exprs: &[P<Node>]) {
        self.pending_expressions.borrow_mut().extend_from_slice(exprs);
    }

    // classfields.go:2940
    fn add_private_identifier_property_declaration_to_environment(
        &self,
        node: P<Node>,
        name: P<Node>,
    ) {
        let lex = self.get_class_lexical_environment();
        let env = self.get_private_identifier_environment();
        let is_static = ast::has_static_modifier(node);
        let previous_info = self.get_private_identifier(env, name);
        let is_valid = !self.is_reserved_private_name(name) && previous_info.is_none();

        if is_static {
            let mut brand_check_identifier = lex.class_this.get();
            if brand_check_identifier.is_none() {
                brand_check_identifier = lex.class_constructor.get();
            }
            let variable_name = self.create_hoisted_variable_for_private_name(name, "");
            self.set_private_identifier(
                env,
                name,
                P::new(PrivateIdentifierInfo {
                    kind: PrivateIdentifierKind::Field,
                    is_static: true,
                    brand_check_identifier,
                    variable_name: Some(variable_name),
                    is_valid,
                    method_name: None,
                    getter_name: Cell::new(None),
                    setter_name: Cell::new(None),
                }),
            );
        } else {
            let weak_map_name = self.create_hoisted_variable_for_private_name(name, "");
            self.set_private_identifier(
                env,
                name,
                P::new(PrivateIdentifierInfo {
                    kind: PrivateIdentifierKind::Field,
                    is_static: false,
                    brand_check_identifier: Some(weak_map_name),
                    is_valid,
                    variable_name: None,
                    method_name: None,
                    getter_name: Cell::new(None),
                    setter_name: Cell::new(None),
                }),
            );
            self.add_pending_expressions(&[self.factory().new_assignment_expression(
                weak_map_name,
                self.factory().new_new_expression(
                    self.factory().new_identifier("WeakMap"),
                    None, /*typeArguments*/
                    Some(self.factory().new_node_list(vec![])),
                ),
            )]);
        }
    }

    // classfields.go:2981
    fn add_private_identifier_method_to_environment(
        &self,
        name: P<Node>,
        lex: P<ClassLexicalEnvironment>,
        env: P<PrivateEnvironment>,
        is_static: bool,
        is_valid: bool,
    ) {
        let method_name = self.create_hoisted_variable_for_private_name(name, "");
        let brand_check_identifier: Option<P<Node>>;
        if is_static {
            brand_check_identifier = lex.class_this.get().or(lex.class_constructor.get());
            assert!(
                brand_check_identifier.is_some(),
                "classConstructor should be set in private identifier environment"
            );
        } else {
            brand_check_identifier = env.data.weak_set_name.get();
        }
        self.set_private_identifier(
            env,
            name,
            P::new(PrivateIdentifierInfo {
                kind: PrivateIdentifierKind::Method,
                method_name: Some(method_name),
                brand_check_identifier,
                is_static,
                is_valid,
                variable_name: None,
                getter_name: Cell::new(None),
                setter_name: Cell::new(None),
            }),
        );
    }

    // classfields.go:3002
    fn add_private_identifier_get_accessor_to_environment(
        &self,
        name: P<Node>,
        lex: P<ClassLexicalEnvironment>,
        env: P<PrivateEnvironment>,
        is_static: bool,
        is_valid: bool,
        previous_info: Option<P<PrivateIdentifierInfo>>,
    ) {
        let getter_name = self.create_hoisted_variable_for_private_name(name, "_get");
        let brand_check_identifier: Option<P<Node>>;
        if is_static {
            brand_check_identifier = lex.class_this.get().or(lex.class_constructor.get());
            assert!(
                brand_check_identifier.is_some(),
                "classConstructor should be set in private identifier environment"
            );
        } else {
            brand_check_identifier = env.data.weak_set_name.get();
            assert!(
                brand_check_identifier.is_some(),
                "weakSetName should be set in private identifier environment"
            );
        }

        if let Some(previous_info) = previous_info.filter(|p| {
            p.kind == PrivateIdentifierKind::Accessor
                && p.is_static == is_static
                && p.getter_name.get().is_none()
        }) {
            previous_info.getter_name.set(Some(getter_name));
        } else {
            self.set_private_identifier(
                env,
                name,
                P::new(PrivateIdentifierInfo {
                    kind: PrivateIdentifierKind::Accessor,
                    getter_name: Cell::new(Some(getter_name)),
                    brand_check_identifier,
                    is_static,
                    is_valid,
                    variable_name: None,
                    method_name: None,
                    setter_name: Cell::new(None),
                }),
            );
        }
    }

    // classfields.go:3029
    fn add_private_identifier_set_accessor_to_environment(
        &self,
        name: P<Node>,
        lex: P<ClassLexicalEnvironment>,
        env: P<PrivateEnvironment>,
        is_static: bool,
        is_valid: bool,
        previous_info: Option<P<PrivateIdentifierInfo>>,
    ) {
        let setter_name = self.create_hoisted_variable_for_private_name(name, "_set");
        let brand_check_identifier: Option<P<Node>>;
        if is_static {
            brand_check_identifier = lex.class_this.get().or(lex.class_constructor.get());
            assert!(
                brand_check_identifier.is_some(),
                "classConstructor should be set in private identifier environment"
            );
        } else {
            brand_check_identifier = env.data.weak_set_name.get();
            assert!(
                brand_check_identifier.is_some(),
                "weakSetName should be set in private identifier environment"
            );
        }

        if let Some(previous_info) = previous_info.filter(|p| {
            p.kind == PrivateIdentifierKind::Accessor
                && p.is_static == is_static
                && p.setter_name.get().is_none()
        }) {
            previous_info.setter_name.set(Some(setter_name));
        } else {
            self.set_private_identifier(
                env,
                name,
                P::new(PrivateIdentifierInfo {
                    kind: PrivateIdentifierKind::Accessor,
                    setter_name: Cell::new(Some(setter_name)),
                    brand_check_identifier,
                    is_static,
                    is_valid,
                    variable_name: None,
                    method_name: None,
                    getter_name: Cell::new(None),
                }),
            );
        }
    }

    // classfields.go:3056
    fn add_private_identifier_auto_accessor_to_environment(
        &self,
        node: P<Node>,
        name: P<Node>,
        lex: P<ClassLexicalEnvironment>,
        env: P<PrivateEnvironment>,
        is_static: bool,
        is_valid: bool,
    ) {
        let _ = node;
        let getter_name = self.create_hoisted_variable_for_private_name(name, "_get");
        let setter_name = self.create_hoisted_variable_for_private_name(name, "_set");
        let brand_check_identifier: Option<P<Node>>;
        if is_static {
            brand_check_identifier = lex.class_this.get().or(lex.class_constructor.get());
            assert!(
                brand_check_identifier.is_some(),
                "classConstructor should be set in private identifier environment"
            );
        } else {
            brand_check_identifier = env.data.weak_set_name.get();
            assert!(
                brand_check_identifier.is_some(),
                "weakSetName should be set in private identifier environment"
            );
        }

        self.set_private_identifier(
            env,
            name,
            P::new(PrivateIdentifierInfo {
                kind: PrivateIdentifierKind::Accessor,
                getter_name: Cell::new(Some(getter_name)),
                setter_name: Cell::new(Some(setter_name)),
                brand_check_identifier,
                is_static,
                is_valid,
                variable_name: None,
                method_name: None,
            }),
        );
    }

    // classfields.go:3081
    fn add_private_identifier_to_environment(&self, node: P<Node>) {
        let lex = self.get_class_lexical_environment();
        let env = self.get_private_identifier_environment();
        let name = node.name().unwrap();
        let is_static = ast::has_static_modifier(node);
        let previous_info = self.get_private_identifier(env, name);
        let is_valid = !self.is_reserved_private_name(name) && previous_info.is_none();

        if ast::is_auto_accessor_property_declaration(node) {
            self.add_private_identifier_auto_accessor_to_environment(
                node, name, lex, env, is_static, is_valid,
            );
        } else if ast::is_property_declaration(node) {
            self.add_private_identifier_property_declaration_to_environment(node, name);
        } else if ast::is_method_declaration(node) {
            self.add_private_identifier_method_to_environment(name, lex, env, is_static, is_valid);
        } else if ast::is_get_accessor_declaration(node) {
            self.add_private_identifier_get_accessor_to_environment(
                name,
                lex,
                env,
                is_static,
                is_valid,
                previous_info,
            );
        } else if ast::is_set_accessor_declaration(node) {
            self.add_private_identifier_set_accessor_to_environment(
                name,
                lex,
                env,
                is_static,
                is_valid,
                previous_info,
            );
        }
    }

    // classfields.go:3102
    fn set_private_identifier(
        &self,
        env: P<PrivateEnvironment>,
        name: P<Node>,
        info: P<PrivateIdentifierInfo>,
    ) {
        if self.emit_context().has_auto_generate_info(Some(name)) {
            env.generated_identifiers
                .borrow_mut()
                .insert(self.emit_context().get_node_for_generated_name(name), info);
        } else {
            env.members.borrow_mut().insert(name.text().to_string(), info);
        }
    }

    // classfields.go:3113
    pub(crate) fn get_private_identifier(
        &self,
        env: P<PrivateEnvironment>,
        name: P<Node>,
    ) -> Option<P<PrivateIdentifierInfo>> {
        if self.emit_context().has_auto_generate_info(Some(name)) {
            return env
                .generated_identifiers
                .borrow()
                .get(&self.emit_context().get_node_for_generated_name(name))
                .copied();
        }
        env.members.borrow().get(name.text()).copied()
    }

    // classfields.go:3122
    pub(crate) fn create_hoisted_variable_for_class(
        &self,
        name_text: &str,
        node: P<Node>,
        suffix: &str,
    ) -> P<Node> {
        let _ = node;
        let env = self.get_private_identifier_environment();
        let identifier: P<Node>;
        if let Some(class_name) = env.data.class_name.get() {
            let prefix = format!("_{}_", class_name.text());
            identifier = self.factory().new_unique_name_ex(
                &format!("{prefix}{name_text}"),
                printer::AutoGenerateOptions {
                    flags: printer::GeneratedIdentifierFlags::Optimistic
                        | printer::GeneratedIdentifierFlags::ReservedInNestedScopes,
                    suffix: alloc_str(suffix),
                    ..Default::default()
                },
            );
        } else {
            identifier = self.factory().new_unique_name_ex(
                &format!("_{name_text}"),
                printer::AutoGenerateOptions {
                    flags: printer::GeneratedIdentifierFlags::Optimistic
                        | printer::GeneratedIdentifierFlags::ReservedInNestedScopes,
                    suffix: alloc_str(suffix),
                    ..Default::default()
                },
            );
        }
        if self.requires_block_scoped_var() {
            self.emit_context().add_lexical_declaration(identifier);
        } else {
            self.emit_context().add_variable_declaration(identifier);
        }
        identifier
    }

    // classfields.go:3145
    fn create_hoisted_variable_for_class_from_node(&self, name: P<Node>, suffix: &str) -> P<Node> {
        let env = self.get_private_identifier_environment();
        let prefix: String;
        if let Some(class_name) = env.data.class_name.get() {
            prefix = format!("_{}_", class_name.text());
        } else {
            prefix = "_".to_string();
        }
        let identifier = self.factory().new_generated_name_for_node_ex(
            name,
            printer::AutoGenerateOptions {
                flags: printer::GeneratedIdentifierFlags::Optimistic
                    | printer::GeneratedIdentifierFlags::ReservedInNestedScopes,
                prefix: alloc_str(&prefix),
                suffix: alloc_str(suffix),
            },
        );
        if self.requires_block_scoped_var() {
            self.emit_context().add_lexical_declaration(identifier);
        } else {
            self.emit_context().add_variable_declaration(identifier);
        }
        identifier
    }

    // classfields.go:3166
    fn create_hoisted_variable_for_private_name(&self, name: P<Node>, suffix: &str) -> P<Node> {
        // If the name is a generated identifier (e.g., auto-accessor backing field),
        // use node-based name generation so the emitter can resolve the name properly.
        if self.emit_context().has_auto_generate_info(Some(name)) {
            return self.create_hoisted_variable_for_class_from_node(name, suffix);
        }
        let mut text = name.text();
        if !text.is_empty() && text.as_bytes()[0] == b'#' {
            text = &text[1..]; // strip leading '#'
        }
        self.create_hoisted_variable_for_class(text, name, suffix)
    }

    // classfields.go:3181
    pub(crate) fn access_private_identifier(
        &self,
        name: P<Node>,
    ) -> Option<P<PrivateIdentifierInfo>> {
        let mut env = self.lexical_environment.get();
        while let Some(e) = env {
            if let Some(private_env) = e.private_env.get()
                && let Some(info) = self.get_private_identifier(private_env, name)
            {
                if info.kind == PrivateIdentifierKind::Untransformed {
                    return None;
                }
                return Some(info);
            }
            env = e.previous;
        }
        None
    }

    // classfields.go:3195
    pub(crate) fn wrap_private_identifier_for_destructuring_target(
        &self,
        node: P<Node>,
    ) -> Option<P<Node>> {
        let prop = node.as_property_access_expression();
        let parameter = self.factory().new_generated_name_for_node(node);
        let info = self.access_private_identifier(prop.name());
        let Some(info) = info else {
            return self.visitor().visit_each_child(Some(node));
        };
        let mut receiver = prop.expression;
        // We cannot copy `this` or `super` into the function because they will be bound
        // differently inside the function.
        let is_this_or_super_property = prop.expression.kind() == Kind::ThisKeyword
            || prop.expression.kind() == Kind::SuperKeyword;
        if is_this_or_super_property || !is_simple_copiable_expression(prop.expression) {
            receiver = self.factory().new_temp_variable_ex(printer::AutoGenerateOptions {
                flags: printer::GeneratedIdentifierFlags::ReservedInNestedScopes,
                ..Default::default()
            });
            self.emit_context().add_variable_declaration(receiver);
            let assignment = self.factory().new_assignment_expression(
                receiver,
                self.visitor().visit_node(Some(prop.expression)).unwrap(),
            );
            self.pending_expressions.borrow_mut().push(assignment);
        }
        let assign_expr =
            self.create_private_identifier_assignment(info, receiver, parameter, Kind::EqualsToken);
        Some(self.factory().new_assignment_target_wrapper(parameter, assign_expr))
    }

    // classfields.go:3220
    fn visit_assignment_element(&self, mut node: P<Node>) -> Option<P<Node>> {
        // 13.15.5.5 RS: IteratorDestructuringAssignmentEvaluation
        //   AssignmentElement : DestructuringAssignmentTarget Initializer?
        //     ...
        //     4. If |Initializer| is present and _value_ is *undefined*, then
        //        a. If IsAnonymousFunctionDefinition(|Initializer|) and IsIdentifierRef of |DestructuringAssignmentTarget| are both *true*, then
        //           i. Let _v_ be ? NamedEvaluation of |Initializer| with argument _lref_.[[ReferencedName]].
        //     ...

        if is_named_evaluation_and(
            self.emit_context(),
            node,
            Some(&|n| self.is_anonymous_class_needing_assigned_name(n)),
        ) {
            node = transform_named_evaluation(
                self.emit_context(),
                node,
                false, /*ignoreEmptyStringLiteral*/
                "",    /*assignedName*/
            );
        }
        if ast::is_assignment_expression(node, true /*excludeCompoundAssignment*/) {
            let b = node.as_binary_expression();
            let left = self.visit_destructuring_assignment_target(b.left).unwrap();
            let right = self.visitor().visit_node(Some(b.right())).unwrap();
            return Some(self.factory().update_binary_expression(
                node,
                None,
                left,
                None,
                b.operator_token,
                right,
            ));
        }
        self.visit_destructuring_assignment_target(node)
    }

    // classfields.go:3247
    fn visit_assignment_rest_element(&self, node: P<Node>) -> Option<P<Node>> {
        let spread = node.as_spread_element();
        if ast::is_left_hand_side_expression(spread.expression) {
            let expr = self.visit_destructuring_assignment_target(spread.expression).unwrap();
            return Some(self.factory().update_spread_element(node, expr));
        }
        self.visitor().visit_each_child(Some(node))
    }

    // classfields.go:3256
    pub(crate) fn visit_array_assignment_element(&self, node: P<Node>) -> Option<P<Node>> {
        if ast::is_array_binding_or_assignment_element(node) {
            if ast::is_spread_element(node) {
                return self.visit_assignment_rest_element(node);
            }
            if node.kind() != Kind::OmittedExpression {
                return self.visit_assignment_element(node);
            }
        }
        self.visitor().visit_each_child(Some(node))
    }

    // classfields.go:3268
    fn visit_assignment_property(&self, node: P<Node>) -> Option<P<Node>> {
        // AssignmentProperty : PropertyName `:` AssignmentElement
        // AssignmentElement : DestructuringAssignmentTarget Initializer?

        // 13.15.5.6 RS: KeyedDestructuringAssignmentEvaluation
        //   AssignmentElement : DestructuringAssignmentTarget Initializer?
        //     ...
        //     3. If |Initializer| is present and _v_ is *undefined*, then
        //        a. If IsAnonymousfunctionDefinition(|Initializer|) and IsIdentifierRef of |DestructuringAssignmentTarget| are both *true*, then
        //           i. Let _rhsValue_ be ? NamedEvaluation of |Initializer| with argument _lref_.[[ReferencedName]].
        //     ...

        let prop = node.as_property_assignment();
        let name = self.visitor().visit_node(node.name()).unwrap();
        let init = prop.initializer.get();
        if ast::is_assignment_expression(init, true /*excludeCompoundAssignment*/) {
            let assign_elem = self.visit_assignment_element(init).unwrap();
            return Some(self.factory().update_property_assignment(
                node,
                None,
                name,
                None,
                None,
                assign_elem,
            ));
        }
        if ast::is_left_hand_side_expression(init) {
            let target = self.visit_destructuring_assignment_target(init).unwrap();
            return Some(
                self.factory().update_property_assignment(node, None, name, None, None, target),
            );
        }
        self.visitor().visit_each_child(Some(node))
    }

    // classfields.go:3294
    fn visit_shorthand_assignment_property(&self, mut node: P<Node>) -> Option<P<Node>> {
        // AssignmentProperty : IdentifierReference Initializer?

        // 13.15.5.3 RS: PropertyDestructuringAssignmentEvaluation
        //   AssignmentProperty : IdentifierReference Initializer?
        //     ...
        //     4. If |Initializer?| is present and _v_ is *undefined*, then
        //        a. If IsAnonymousFunctionDefinition(|Initializer|) is *true*, then
        //           i. Set _v_ to ? NamedEvaluation of |Initializer| with argument _P_.
        //     ...

        if is_named_evaluation_and(
            self.emit_context(),
            node,
            Some(&|n| self.is_anonymous_class_needing_assigned_name(n)),
        ) {
            node = transform_named_evaluation(
                self.emit_context(),
                node,
                false, /*ignoreEmptyStringLiteral*/
                "",    /*assignedName*/
            );
        }
        self.visitor().visit_each_child(Some(node))
    }

    // classfields.go:3311
    fn visit_assignment_rest_property(&self, node: P<Node>) -> Option<P<Node>> {
        let spread = node.as_spread_assignment();
        if ast::is_left_hand_side_expression(spread.expression) {
            let expr = self.visit_destructuring_assignment_target(spread.expression).unwrap();
            return Some(self.factory().update_spread_assignment(node, expr));
        }
        self.visitor().visit_each_child(Some(node))
    }

    // classfields.go:3320
    pub(crate) fn visit_object_assignment_element(&self, node: P<Node>) -> Option<P<Node>> {
        assert!(ast::is_object_literal_element(node));
        if ast::is_spread_assignment(node) {
            return self.visit_assignment_rest_property(node);
        }
        if ast::is_shorthand_property_assignment(node) {
            return self.visit_shorthand_assignment_property(node);
        }
        if ast::is_property_assignment(node) {
            return self.visit_assignment_property(node);
        }
        self.visitor().visit_each_child(Some(node))
    }

    // classfields.go:3334
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Visitor branches share an optional node result so sibling branches can remove a node."
    )]
    pub(crate) fn visit_assignment_pattern(&self, node: P<Node>) -> Option<P<Node>> {
        if ast::is_array_literal_expression(node) {
            // Transforms private names in destructuring assignment array bindings.
            // Transforms SuperProperty assignments in destructuring assignment array bindings in static initializers.
            //
            // Source:
            // ([ this.#myProp ] = [ "hello" ]);
            //
            // Transformation:
            // [ { set value(x) { this.#myProp = x; } }.value ] = [ "hello" ];
            let arr = node.as_array_literal_expression();
            return Some(self.factory().update_array_literal_expression(
                node,
                self.array_assignment_element_visitor().visit_nodes(Some(arr.elements)).unwrap(),
                arr.multi_line,
            ));
        }
        // Transforms private names in destructuring assignment object bindings.
        // Transforms SuperProperty assignments in destructuring assignment object bindings in static initializers.
        //
        // Source:
        // ({ stringProperty: this.#myProp } = { stringProperty: "hello" });
        //
        // Transformation:
        // ({ stringProperty: { set value(x) { this.#myProp = x; } }.value }) = { stringProperty: "hello" };
        let obj = node.as_object_literal_expression();
        Some(self.factory().update_object_literal_expression(
            node,
            self.object_assignment_element_visitor().visit_nodes(Some(obj.properties)).unwrap(),
            obj.multi_line,
        ))
    }
}

// classfields.go:3365
pub fn create_private_static_field_initializer(
    factory: &printer::NodeFactory,
    variable_name: P<Node>,
    initializer: Option<P<Node>>,
) -> P<Node> {
    let initializer = initializer.unwrap_or_else(|| factory.new_void_zero_expression());
    factory.new_assignment_expression(
        variable_name,
        factory.new_object_literal_expression(
            factory.new_node_list(vec![factory.new_property_assignment(
                None,
                factory.new_identifier("value"),
                None,
                None,
                initializer,
            )]),
            false,
        ),
    )
}

// classfields.go:3380
pub fn create_private_instance_field_initializer(
    factory: &printer::NodeFactory,
    receiver: P<Node>,
    initializer: Option<P<Node>>,
    weak_map_name: P<Node>,
) -> P<Node> {
    let initializer = initializer.unwrap_or_else(|| factory.new_void_zero_expression());
    factory.new_method_call(
        weak_map_name,
        factory.new_identifier("set"),
        vec![receiver, initializer],
    )
}

// classfields.go:3387
pub fn create_private_instance_method_initializer(
    factory: &printer::NodeFactory,
    receiver: P<Node>,
    weak_set_name: P<Node>,
) -> P<Node> {
    factory.new_method_call(weak_set_name, factory.new_identifier("add"), vec![receiver])
}

impl ClassFieldsTransformer {
    // classfields.go:3391
    fn is_reserved_private_name(&self, node: P<Node>) -> bool {
        !(ast::is_private_identifier(node)
            && self.emit_context().has_auto_generate_info(Some(node)))
            && node.text() == "#constructor"
    }
}

// classfields.go:3395
pub fn is_static_property_declaration_or_class_static_block(node: P<Node>) -> bool {
    ast::is_class_static_block_declaration(node)
        || (ast::is_property_declaration(node) && ast::has_static_modifier(node))
}

impl ClassFieldsTransformer {
    // classfields.go:3400
    fn get_properties(node: P<Node>, require_initializer: bool, is_static: bool) -> Vec<P<Node>> {
        let mut result = Vec::new();
        result.extend(node.members().iter().filter_map(|member| {
            if ast::is_property_declaration(*member)
                && (!require_initializer || member.initializer().is_some())
                && ast::has_static_modifier(*member) == is_static
            {
                Some(*member)
            } else {
                None
            }
        }));
        result
    }

    // classfields.go:3412
    pub(crate) fn get_static_properties_and_class_static_block(node: P<Node>) -> Vec<P<Node>> {
        let mut result = Vec::new();
        result.extend(node.members().iter().filter_map(|member| {
            if ast::is_class_static_block_declaration(*member)
                || (ast::is_property_declaration(*member) && ast::has_static_modifier(*member))
            {
                Some(*member)
            } else {
                None
            }
        }));
        result
    }
}

// classfields.go:3423
// classHasClassThisAssignment checks if a class has a static block that is a class-this assignment.
pub fn class_has_class_this_assignment(emit_context: P<EmitContext>, node: P<Node>) -> bool {
    node.members().iter().any(|member| is_class_this_assignment_block(emit_context, *member))
}

// classfields.go:3432
pub fn is_non_static_method_or_accessor_with_private_name(member: P<Node>) -> bool {
    !ast::is_static(member)
        && (ast::is_method_or_accessor(member)
            || ast::is_auto_accessor_property_declaration(member))
        && ast::is_private_identifier(member.name().unwrap())
}

// classfields.go:3438
pub fn create_member_access_for_property_name(
    factory: &printer::NodeFactory,
    emit_context: P<EmitContext>,
    receiver: P<Node>,
    name: P<Node>,
    location: P<Node>,
) -> P<Node> {
    if ast::is_computed_property_name(name) {
        let expression = factory.new_element_access_expression(
            receiver,
            None,
            name.expression().unwrap(),
            NodeFlags::None,
        );
        expression.set_loc(location.loc());
        return expression;
    }

    let expression: P<Node> = if ast::is_identifier(name) || ast::is_private_identifier(name) {
        factory.new_property_access_expression(receiver, None, name, NodeFlags::None)
    } else {
        // string or numeric literal
        factory.new_element_access_expression(receiver, None, name, NodeFlags::None)
    };
    emit_context.set_comment_range(expression, name.loc());
    emit_context.set_source_map_range(expression, name.loc());
    emit_context.add_emit_flags(expression, printer::EmitFlags::NoNestedSourceMaps);
    expression
}

impl ClassFieldsTransformer {
    // classfields.go:3457
    // Returns (thisArg, target).
    pub(crate) fn create_call_binding(&self, node: P<Node>) -> (P<Node>, P<Node>) {
        if ast::is_super_property(node) {
            return (self.factory().new_this_expression(), node);
        }
        if ast::is_property_access_expression(node) {
            let expr = node.as_property_access_expression();
            if should_be_captured_in_temp_variable(expr.expression) {
                let this_arg = self.factory().new_temp_variable();
                self.emit_context().add_variable_declaration(this_arg);
                let target = self.factory().new_property_access_expression(
                    self.factory().new_parenthesized_expression(
                        // TODO: do we even need these?
                        self.factory().new_assignment_expression(this_arg, expr.expression),
                    ),
                    None,
                    expr.name(),
                    NodeFlags::None,
                );
                return (this_arg, target);
            }
            return (expr.expression, node);
        }
        let this_arg = self.factory().new_void_zero_expression();
        let target = node;
        (this_arg, target)
    }
}

// classfields.go:3483
pub fn should_be_captured_in_temp_variable(node: P<Node>) -> bool {
    let target = ast::skip_parentheses(node);
    !matches!(
        target.kind(),
        Kind::Identifier
            | Kind::ThisKeyword
            | Kind::NumericLiteral
            | Kind::BigIntLiteral
            | Kind::StringLiteral
    )
}

impl ClassFieldsTransformer {
    // classfields.go:3493
    pub(crate) fn create_accessor_property_get_redirector(
        &self,
        node: P<Node>,
        modifiers: Option<P<ModifierList>>,
        name: P<Node>,
        receiver: P<Node>,
    ) -> P<Node> {
        let f = self.factory();
        let backing_field_name = f.new_generated_private_name_for_node_ex(
            node.name().unwrap(),
            printer::AutoGenerateOptions { suffix: "_accessor_storage", ..Default::default() },
        );
        let return_expr =
            f.new_property_access_expression(receiver, None, backing_field_name, NodeFlags::None);
        let return_stmt = f.new_return_statement(Some(return_expr));
        let body = f.new_block(f.new_node_list(vec![return_stmt]), false);
        f.new_get_accessor_declaration(
            modifiers,
            name,
            None, /*typeParameters*/
            Some(f.new_node_list(vec![])),
            None, /*returnType*/
            None, /*fullSignature*/
            Some(body),
        )
    }

    // classfields.go:3514
    pub(crate) fn create_accessor_property_set_redirector(
        &self,
        node: P<Node>,
        modifiers: Option<P<ModifierList>>,
        name: P<Node>,
        receiver: P<Node>,
    ) -> P<Node> {
        let f = self.factory();
        let backing_field_name = f.new_generated_private_name_for_node_ex(
            node.name().unwrap(),
            printer::AutoGenerateOptions { suffix: "_accessor_storage", ..Default::default() },
        );
        let value_param = f.new_parameter_declaration(
            None, /*modifiers*/
            None, /*dotDotDotToken*/
            f.new_identifier("value"),
            None, /*questionToken*/
            None, /*typeNode*/
            None, /*initializer*/
        );
        let assign_expr = f.new_assignment_expression(
            f.new_property_access_expression(receiver, None, backing_field_name, NodeFlags::None),
            f.new_identifier("value"),
        );
        let expr_stmt = f.new_expression_statement(assign_expr);
        let body = f.new_block(f.new_node_list(vec![expr_stmt]), false);
        f.new_set_accessor_declaration(
            modifiers,
            name,
            None, /*typeParameters*/
            Some(f.new_node_list(vec![value_param])),
            None, /*returnType*/
            None, /*fullSignature*/
            Some(body),
        )
    }
}

// classfields.go:3547
// Go returns an `iter.Seq`; every caller ranges over all of it, so the elements are collected.
pub fn flatten_comma_list(node: P<Node>) -> Vec<P<Node>> {
    let mut result = Vec::new();
    flatten_comma_list_worker(node, &mut |e| {
        result.push(e);
        true
    });
    result
}

// classfields.go:3553
fn flatten_comma_list_worker(node: P<Node>, yield_: &mut dyn FnMut(P<Node>) -> bool) -> bool {
    if ast::is_parenthesized_expression(node) && ast::node_is_synthesized(node) {
        flatten_comma_list_worker(node.expression().unwrap(), yield_)
    } else if ast::is_comma_expression(node) {
        flatten_comma_list_worker(node.as_binary_expression().left, yield_)
            && flatten_comma_list_worker(node.as_binary_expression().right(), yield_)
    } else {
        yield_(node)
    }
}

// classfields.go:3564
// Returns the BinaryExpression node (Go returns `*ast.BinaryExpression`).
pub fn find_computed_property_name_cache_assignment(
    emit_context: P<EmitContext>,
    name: P<Node>,
) -> Option<P<Node>> {
    let _ = emit_context;
    let mut node = name.expression().unwrap();
    loop {
        node = ast::skip_outer_expressions(node, OuterExpressionKinds::empty());
        if ast::is_binary_expression(node)
            && node.as_binary_expression().operator_token.kind() == Kind::CommaToken
        {
            node = node.as_binary_expression().right();
            continue;
        }
        if ast::is_assignment_expression(node, true /*excludeCompoundAssignment*/)
            && ast::is_identifier(node.as_binary_expression().left)
        {
            return Some(node);
        }
        break;
    }
    None
}

// classfields.go:3580
pub fn expand_pre_or_postfix_increment_or_decrement_expression(
    factory: &printer::NodeFactory,
    emit_context: P<EmitContext>,
    node: P<Node>,
    expression: P<Node>,
    result_variable: Option<P<Node>>,
) -> P<Node> {
    let operator: Kind;
    let operand: P<Node>;
    if ast::is_prefix_unary_expression(node) {
        operator = node.as_prefix_unary_expression().operator;
        operand = node.as_prefix_unary_expression().operand;
    } else {
        operator = node.as_postfix_unary_expression().operator;
        operand = node.as_postfix_unary_expression().operand;
    }

    let temp = factory.new_temp_variable();
    emit_context.add_variable_declaration(temp);
    let mut expression = factory.new_assignment_expression(temp, expression);
    expression.set_loc(operand.loc());

    let mut operation: P<Node>;
    if ast::is_prefix_unary_expression(node) {
        operation = factory.new_prefix_unary_expression(operator, temp);
    } else {
        operation = factory.new_postfix_unary_expression(temp, operator);
    }
    operation.set_loc(node.loc());

    if let Some(result_variable) = result_variable {
        operation = factory.new_assignment_expression(result_variable, operation);
        operation.set_loc(node.loc());
    }

    expression = factory.new_comma_expression(expression, operation);
    expression.set_loc(node.loc());

    if ast::is_postfix_unary_expression(node) {
        expression = factory.new_comma_expression(expression, temp);
        expression.set_loc(node.loc());
    }

    expression
}

// Go `&privateIdentifierInfo{kind: ...}` with every other field at its zero value.
pub fn new_private_identifier_info(kind: PrivateIdentifierKind) -> P<PrivateIdentifierInfo> {
    P::new(PrivateIdentifierInfo {
        kind,
        brand_check_identifier: None,
        is_static: false,
        is_valid: false,
        variable_name: None,
        method_name: None,
        getter_name: Cell::new(None),
        setter_name: Cell::new(None),
    })
}
