// !!! SourceMaps and Comments need to be validated

use super::*;
use tsrs_checker::LiteralValue;
use tsrs_core::TextPos;

/// Go `map[string]*ast.Node`: a reference type that `pushScope` saves and `popScope` restores by pointer, so a saved
/// map and the current one may alias (mutations through one are visible through the other).
type DeclarationsOfName = Option<Rc<RefCell<FxHashMap<&'static str, P<Node>>>>>;

// Transforms TypeScript-specific runtime syntax into JavaScript-compatible syntax.
struct RuntimeSyntaxTransformer {
    base: Transformer,
    compiler_options: P<CompilerOptions>,
    parent_node: Cell<Option<P<Node>>>,
    current_node: Cell<Option<P<Node>>>,
    current_source_file: Cell<Option<P<Node>>>,
    current_scope: Cell<Option<P<Node>>>, // SourceFile | Block | ModuleBlock | CaseBlock
    current_scope_first_declarations_of_name: RefCell<DeclarationsOfName>,
    current_enum: Cell<Option<P<Node>>>,
    current_namespace: Cell<Option<P<Node>>>,
    resolver: ReferenceResolverRef,
    emit_resolver: Option<Resolver>,
}

// runtimesyntax.go:31
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_runtime_syntax_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let compiler_options = opt.compiler_options;
    let emit_context = opt.context;
    let tx = P::new(RuntimeSyntaxTransformer {
        base: Transformer::default(),
        compiler_options,
        parent_node: Cell::new(None),
        current_node: Cell::new(None),
        current_source_file: Cell::new(None),
        current_scope: Cell::new(None),
        current_scope_first_declarations_of_name: RefCell::new(None),
        current_enum: Cell::new(None),
        current_namespace: Cell::new(None),
        resolver: opt.resolver,
        emit_resolver: opt.emit_resolver,
    });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(emit_context),
    ))
}

impl RuntimeSyntaxTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    // Pushes a new child node onto the ancestor tracking stack, returning the grandparent node to be restored later via `popNode`.
    // runtimesyntax.go:39
    fn push_node(&self, node: P<Node>) -> Option<P<Node>> {
        let grandparent_node = self.parent_node.get();
        self.parent_node.set(self.current_node.get());
        self.current_node.set(Some(node));
        grandparent_node
    }

    // Pops the last child node off the ancestor tracking stack, restoring the grandparent node.
    // runtimesyntax.go:47
    fn pop_node(&self, grandparent_node: Option<P<Node>>) {
        self.current_node.set(self.parent_node.get());
        self.parent_node.set(grandparent_node);
    }

    // runtimesyntax.go:52
    fn push_scope(&self, node: P<Node>) -> (Option<P<Node>>, DeclarationsOfName) {
        let saved_current_scope = self.current_scope.get();
        let saved_current_scope_first_declarations_of_name =
            self.current_scope_first_declarations_of_name.borrow().clone();
        match node.kind() {
            Kind::SourceFile => {
                self.current_scope.set(Some(node));
                self.current_source_file.set(Some(node));
                *self.current_scope_first_declarations_of_name.borrow_mut() = None;
            }
            Kind::CaseBlock | Kind::ModuleBlock | Kind::Block => {
                self.current_scope.set(Some(node));
                *self.current_scope_first_declarations_of_name.borrow_mut() = None;
            }
            Kind::FunctionDeclaration | Kind::ClassDeclaration | Kind::VariableStatement => {
                self.record_declaration_in_scope(node);
            }
            _ => {}
        }
        (saved_current_scope, saved_current_scope_first_declarations_of_name)
    }

    // runtimesyntax.go:69
    fn pop_scope(
        &self,
        saved_current_scope: Option<P<Node>>,
        saved_current_scope_first_declarations_of_name: DeclarationsOfName,
    ) {
        if self.current_scope.get() != saved_current_scope {
            // only reset the first declaration for a name if we are exiting the scope in which it was declared
            *self.current_scope_first_declarations_of_name.borrow_mut() =
                saved_current_scope_first_declarations_of_name;
        }

        self.current_scope.set(saved_current_scope);
    }

    // Visits each node in the AST
    // runtimesyntax.go:79
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        let grandparent_node = self.push_node(node);
        let (saved_current_scope, saved_current_scope_first_declarations_of_name) =
            self.push_scope(node);
        let result = self.visit_worker(node);
        self.pop_scope(saved_current_scope, saved_current_scope_first_declarations_of_name);
        self.pop_node(grandparent_node);
        result
    }

    fn visit_worker(&self, node: P<Node>) -> Option<P<Node>> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsTypeScript)
            && (self.current_namespace.get().is_none() && self.current_enum.get().is_none()
                || !node.subtree_facts().intersects(SubtreeFacts::ContainsIdentifier))
        {
            return Some(node);
        }

        match node.kind() {
            // TypeScript parameter property modifiers are elided
            Kind::PublicKeyword
            | Kind::PrivateKeyword
            | Kind::ProtectedKeyword
            | Kind::ReadonlyKeyword
            | Kind::OverrideKeyword => None,
            Kind::EnumDeclaration => Some(self.visit_enum_declaration(node)),
            Kind::ModuleDeclaration => Some(self.visit_module_declaration(node)),
            Kind::ClassDeclaration => Some(self.visit_class_declaration(node)),
            Kind::ClassExpression => Some(self.visit_class_expression(node)),
            Kind::Constructor => Some(self.visit_constructor_declaration(node)),
            Kind::FunctionDeclaration => Some(self.visit_function_declaration(node)),
            Kind::VariableStatement => self.visit_variable_statement(node),
            Kind::ExportDeclaration | Kind::ImportDeclaration | Kind::ImportClause => {
                if self.current_namespace.get().is_some()
                    && matches!(self.current_scope.get(), Some(scope) if scope.kind() != Kind::Block)
                {
                    // do not emit ES6 imports and exports since they are illegal inside a namespace
                    None
                } else {
                    self.visitor().visit_each_child(Some(node))
                }
            }
            Kind::ImportEqualsDeclaration => {
                let module_reference_kind =
                    node.as_import_equals_declaration().module_reference.kind();
                if self.current_namespace.get().is_some()
                    && matches!(self.current_scope.get(), Some(scope) if scope.kind() != Kind::Block)
                    && module_reference_kind == Kind::ExternalModuleReference
                {
                    // do not emit ES6 imports and exports since they are illegal inside a namespace
                    None
                } else if self.current_namespace.get().is_some()
                    && matches!(self.current_scope.get(), Some(scope) if scope.kind() == Kind::Block)
                    && module_reference_kind != Kind::ExternalModuleReference
                {
                    // inside a block within a namespace, elide internal import aliases
                    None
                } else {
                    Some(self.visit_import_equals_declaration(node))
                }
            }
            Kind::Identifier => Some(self.visit_identifier(node)),
            Kind::ShorthandPropertyAssignment => {
                Some(self.visit_shorthand_property_assignment(node))
            }
            _ => self.visitor().visit_each_child(Some(node)),
        }
    }

    // Records that a declaration was emitted in the current scope, if it was the first declaration for the provided symbol.
    // runtimesyntax.go:139
    fn record_declaration_in_scope(&self, node: P<Node>) {
        match node.kind() {
            Kind::VariableStatement => {
                self.record_declaration_in_scope(node.as_variable_statement().declaration_list);
                return;
            }
            Kind::VariableDeclarationList => {
                for &decl in node.as_variable_declaration_list().declarations.nodes() {
                    self.record_declaration_in_scope(decl);
                }
                return;
            }
            Kind::ArrayBindingPattern | Kind::ObjectBindingPattern => {
                for &element in node.elements() {
                    self.record_declaration_in_scope(element);
                }
                return;
            }
            _ => {}
        }
        if let Some(name) = node.name() {
            if ast::is_identifier(name) {
                let map = {
                    let mut current = self.current_scope_first_declarations_of_name.borrow_mut();
                    Rc::clone(
                        current.get_or_insert_with(|| Rc::new(RefCell::new(FxHashMap::default()))),
                    )
                };
                let text = name.text();
                map.borrow_mut().entry(text).or_insert(node);
            } else if ast::is_binding_pattern(name) {
                self.record_declaration_in_scope(name);
            }
        }
    }

    // Determines whether a declaration is the first declaration with the same name emitted in the current scope.
    // runtimesyntax.go:174
    fn is_first_declaration_in_scope(&self, node: P<Node>) -> bool {
        if let Some(name) = node.name()
            && ast::is_identifier(name)
        {
            let text = name.text();
            if let Some(map) = self.current_scope_first_declarations_of_name.borrow().as_ref()
                && let Some(&first_declaration) = map.borrow().get(text)
            {
                return first_declaration == node;
            }
        }
        false
    }

    // runtimesyntax.go:186
    fn is_export_of_namespace(&self, node: P<Node>) -> bool {
        self.current_namespace.get().is_some()
            && self.current_scope.get().is_none_or(|scope| scope.kind() != Kind::Block)
            && node.modifier_flags().intersects(ModifierFlags::Export)
    }

    // Gets an expression that represents a property name, such as `"foo"` for the identifier `foo`.
    // runtimesyntax.go:191
    fn get_expression_for_property_name(&self, member: P<Node>) -> P<Node> {
        let f = self.factory();
        let name = member.name().unwrap();
        match name.kind() {
            Kind::PrivateIdentifier => f.new_identifier(""),
            Kind::ComputedPropertyName => {
                // enums don't support computed properties so we always generate the 'expression' part of the name as-is.
                self.visitor().visit_node(name.expression()).unwrap()
            }
            Kind::Identifier => f.new_string_literal(name.text(), TokenFlags::None),
            Kind::StringLiteral => {
                // !!! propagate token flags (will produce new diffs)
                f.new_string_literal(name.text(), TokenFlags::None)
            }
            Kind::NumericLiteral => f.new_numeric_literal(name.text(), TokenFlags::None),
            _ => name,
        }
    }

    // Gets an expression like `E["A"]` that references an enum member.
    // runtimesyntax.go:212
    fn get_enum_qualified_element(&self, enum_: P<Node>, member: P<Node>) -> P<Node> {
        let prop = self.get_namespace_qualified_element(
            self.get_namespace_container_name(enum_),
            self.get_expression_for_property_name(member),
        );
        self.emit_context().add_emit_flags(
            prop,
            EmitFlags::NoComments
                | EmitFlags::NoNestedComments
                | EmitFlags::NoSourceMap
                | EmitFlags::NoNestedSourceMaps,
        );
        prop
    }

    // Gets an expression used to refer to a namespace or enum from within the body of its declaration.
    // runtimesyntax.go:219
    fn get_namespace_container_name(&self, node: P<Node>) -> P<Node> {
        self.factory().new_generated_name_for_node(node)
    }

    // Gets an expression used to refer to an export of a namespace or a member of an enum by property name.
    // runtimesyntax.go:224
    fn get_namespace_qualified_property(&self, ns: P<Node>, name: P<Node>) -> P<Node> {
        self.factory().get_namespace_member_name(
            ns,
            name,
            NameOptions { allow_source_maps: true, ..Default::default() },
        )
    }

    // Gets an expression used to refer to an export of a namespace or a member of an enum by indexed access.
    // runtimesyntax.go:229
    fn get_namespace_qualified_element(&self, ns: P<Node>, expression: P<Node>) -> P<Node> {
        let qualified_name = self.emit_context().factory.new_element_access_expression(
            ns,
            None, /*questionDotToken*/
            expression,
            NodeFlags::None,
        );
        self.emit_context().assign_comment_and_source_map_ranges(qualified_name, expression);
        qualified_name
    }

    // Gets an expression used within the provided node's container for any exported references.
    // runtimesyntax.go:236
    fn get_export_qualified_reference_to_declaration(&self, node: P<Node>) -> P<Node> {
        if self.is_export_of_namespace(node) {
            return self.factory().get_external_module_or_namespace_export_name(
                Some(self.get_namespace_container_name(self.current_namespace.get().unwrap())),
                node,
                false, /*allowComments*/
                true,  /*allowSourceMaps*/
            );
        }
        self.factory().get_declaration_name_ex(
            node,
            NameOptions { allow_source_maps: true, ..Default::default() },
        )
    }

    // runtimesyntax.go:243
    fn add_var_for_declaration(&self, statements: &mut Vec<P<Node>>, node: P<Node>) -> bool {
        self.record_declaration_in_scope(node);
        if !self.is_first_declaration_in_scope(node) {
            return false;
        }

        let f = self.factory();
        let emit_context = self.emit_context();
        // var name;
        let name = f.get_local_name_ex(
            node,
            AssignedNameOptions { allow_source_maps: true, ..Default::default() },
        );
        let var_decl = f.new_variable_declaration(name, None, None, None);
        let var_flags = if self.current_scope.get() == self.current_source_file.get() {
            NodeFlags::None
        } else {
            NodeFlags::Let
        };
        let var_decls = f.new_variable_declaration_list(f.new_node_list(vec![var_decl]), var_flags);
        // Replicate modifierVisitor: strip decorators, TypeScript modifiers, and export when in namespace.
        let mut modifier_mask = !(ModifierFlags::TypeScriptModifier | ModifierFlags::Decorator);
        if self.current_namespace.get().is_some() {
            modifier_mask &= !ModifierFlags::Export;
        }
        let modifiers = extract_modifiers(emit_context, node.modifiers(), modifier_mask);
        let var_statement = f.new_variable_statement(modifiers, var_decls);

        emit_context.set_original(var_decl, node);
        // !!! synthetic comments
        emit_context.set_original(var_statement, node);

        // Adjust the source map emit to match the old emitter.
        if ast::is_enum_declaration(node) {
            emit_context.set_source_map_range(var_decls, node.loc());
        } else {
            emit_context.set_source_map_range(var_statement, node.loc());
        }

        // Trailing comments for enum declaration should be emitted after the function closure
        // instead of the variable statement:
        //
        //     /** Leading comment*/
        //     enum E {
        //         A
        //     } // trailing comment
        //
        // Should emit:
        //
        //     /** Leading comment*/
        //     var E;
        //     (function (E) {
        //         E[E["A"] = 0] = "A";
        //     })(E || (E = {})); // trailing comment
        //
        emit_context.set_comment_range(var_statement, node.loc());
        emit_context.add_emit_flags(var_statement, EmitFlags::NoTrailingComments);
        statements.push(var_statement);

        true
    }

    // runtimesyntax.go:300
    fn visit_enum_declaration(&self, node: P<Node>) -> P<Node> {
        if !self.should_emit_enum_declaration(node) {
            return self.emit_context().new_not_emitted_statement(node);
        }

        let f = self.factory();
        let emit_context = self.emit_context();
        let mut statements: Vec<P<Node>> = Vec::new();

        // If needed, we should emit a variable declaration for the enum:
        //  var name;
        let var_added = self.add_var_for_declaration(&mut statements, node);

        // If we emit a leading variable declaration, we should not emit leading comments for the enum body, but we should
        // still emit the comments if we are emitting to a System module.
        let mut emit_flags = EmitFlags::None;
        if var_added
            && (self.compiler_options.get_emit_module_kind() != ModuleKind::SYSTEM
                || self.current_scope.get() != self.current_source_file.get())
        {
            emit_flags |= EmitFlags::NoLeadingComments;
        }

        //  x || (x = {})
        //  exports.x || (exports.x = {})
        let mut enum_arg = f.new_logical_or_expression(
            self.get_export_qualified_reference_to_declaration(node),
            f.new_assignment_expression(
                self.get_export_qualified_reference_to_declaration(node),
                f.new_object_literal_expression(f.new_node_list(Vec::new()), false),
            ),
        );

        if self.is_export_of_namespace(node) {
            // `localName` is the expression used within this node's containing scope for any local references.
            let local_name = f.get_local_name_ex(
                node,
                AssignedNameOptions { allow_source_maps: true, ..Default::default() },
            );

            //  x = (exports.x || (exports.x = {}))
            enum_arg = f.new_assignment_expression(local_name, enum_arg);
        }

        // (function (name) { ... })(name || (name = {}))
        let enum_param_name = f.new_generated_name_for_node(node);
        emit_context.set_source_map_range(enum_param_name, node.name().unwrap().loc());

        let enum_param = f.new_parameter_declaration(None, None, enum_param_name, None, None, None);
        let enum_body = self.transform_enum_body(node);
        let enum_func = f.new_function_expression(
            None,
            None,
            None,
            None,
            Some(f.new_node_list(vec![enum_param])),
            None,
            None,
            Some(enum_body),
        );
        let enum_call = f.new_call_expression(
            f.new_parenthesized_expression(enum_func),
            None,
            None,
            f.new_node_list(vec![enum_arg]),
            NodeFlags::None,
        );
        let enum_statement = f.new_expression_statement(enum_call);
        emit_context.set_original(enum_statement, node);
        emit_context.assign_comment_and_source_map_ranges(enum_statement, node);
        emit_context.add_emit_flags(enum_statement, emit_flags);
        statements.push(enum_statement);
        f.new_syntax_list(alloc_vec(statements))
    }

    // Transforms the body of an enum declaration.
    // runtimesyntax.go:358
    fn transform_enum_body(&self, node: P<Node>) -> P<Node> {
        let saved_current_enum = self.current_enum.get();
        self.current_enum.set(Some(node));

        // visit the children of `node` in advance to capture any references to enum members
        let node = self.visitor().visit_each_child(Some(node)).unwrap();

        let members = node.as_enum_declaration().members;
        let mut statements: Vec<P<Node>> = Vec::new();
        for i in 0..members.nodes().len() {
            //  E[E["A"] = 0] = "A";
            self.transform_enum_member(&mut statements, node, i);
        }

        let f = self.factory();
        let statement_list = f.new_node_list(statements);
        statement_list.loc.set(members.loc.get());

        self.current_enum.set(saved_current_enum);
        f.new_block(statement_list, true /*multiline*/)
    }

    // Transforms an enum member into a statement. It is expected that `enum` has already been visited.
    // runtimesyntax.go:384
    fn transform_enum_member(&self, statements: &mut Vec<P<Node>>, enum_: P<Node>, index: usize) {
        let member_node = enum_.as_enum_declaration().members.nodes()[index];
        let member = member_node.as_enum_member();
        let f = self.factory();
        let emit_context = self.emit_context();

        let saved_parent = self.parent_node.get();
        self.parent_node.set(self.current_node.get());
        self.current_node.set(Some(member_node));

        //  E[E["A"] = x] = "A";
        //             ^
        let mut expression = member.initializer(); // NOTE: already visited

        let use_explicit_reverse_mapping: bool;

        let parse_node = emit_context.parse_node(Some(member_node));
        let result = self.emit_resolver.unwrap().get_enum_member_value(parse_node.unwrap());
        match result.value {
            Some(LiteralValue::Number(value)) => {
                expression = Some(constant_expression(ConstantValue::Number(value), f));
                use_explicit_reverse_mapping = true;
            }
            Some(LiteralValue::String(value)) => {
                expression = Some(constant_expression(ConstantValue::String(value), f));
                use_explicit_reverse_mapping = false;
            }
            _ => {
                if expression.is_none() {
                    expression = Some(f.new_void_zero_expression());
                }
                use_explicit_reverse_mapping = !result.is_syntactically_string;
            }
        }

        // Define the enum member property:
        //  E[E["A"] = 0] = "A";
        //    ^^^^^^^^--_____
        let mut expression = f.new_assignment_expression(
            self.get_enum_qualified_element(enum_, member_node),
            expression.unwrap(),
        );

        if use_explicit_reverse_mapping {
            //  E[E["A"] = 0] = "A";
            //  ^^--------------^^^^^
            expression = f.new_assignment_expression(
                f.new_element_access_expression(
                    self.get_namespace_container_name(enum_),
                    None, /*questionDotToken*/
                    expression,
                    NodeFlags::None,
                ),
                self.get_expression_for_property_name(member_node),
            );
        }

        let member_statement = f.new_expression_statement(expression);
        emit_context.assign_comment_and_source_map_ranges(expression, member_node);
        emit_context.assign_comment_and_source_map_ranges(member_statement, member_node);
        statements.push(member_statement);

        self.current_node.set(self.parent_node.get());
        self.parent_node.set(saved_parent);
    }

    // runtimesyntax.go:451
    fn visit_module_declaration(&self, node: P<Node>) -> P<Node> {
        if !self.should_emit_module_declaration(node) {
            return self.emit_context().new_not_emitted_statement(node);
        }

        let f = self.factory();
        let emit_context = self.emit_context();
        let mut statements: Vec<P<Node>> = Vec::new();

        // If needed, we should emit a variable declaration for the module:
        //  var name;
        let var_added = self.add_var_for_declaration(&mut statements, node);

        // If we emit a leading variable declaration, we should not emit leading comments for the module body, but we should
        // still emit the comments if we are emitting to a System module.
        let mut emit_flags = EmitFlags::None;
        if var_added
            && (self.compiler_options.get_emit_module_kind() != ModuleKind::SYSTEM
                || self.current_scope.get() != self.current_source_file.get())
        {
            emit_flags |= EmitFlags::NoLeadingComments;
        }

        //  x || (x = {})
        //  exports.x || (exports.x = {})
        let mut module_arg = f.new_logical_or_expression(
            self.get_export_qualified_reference_to_declaration(node),
            f.new_assignment_expression(
                self.get_export_qualified_reference_to_declaration(node),
                f.new_object_literal_expression(f.new_node_list(Vec::new()), false),
            ),
        );

        if self.is_export_of_namespace(node) {
            // `localName` is the expression used within this node's containing scope for any local references.
            let local_name = f.get_local_name_ex(
                node,
                AssignedNameOptions { allow_source_maps: true, ..Default::default() },
            );

            //  x = (exports.x || (exports.x = {}))
            module_arg = f.new_assignment_expression(local_name, module_arg);
        }

        // (function (name) { ... })(name || (name = {}))
        let module_param_name = f.new_generated_name_for_node(node);
        emit_context.set_source_map_range(module_param_name, node.name().unwrap().loc());

        let module_param =
            f.new_parameter_declaration(None, None, module_param_name, None, None, None);
        let module_body = self.transform_module_body(node, self.get_namespace_container_name(node));
        let module_func = f.new_function_expression(
            None,
            None,
            None,
            None,
            Some(f.new_node_list(vec![module_param])),
            None,
            None,
            Some(module_body),
        );
        let module_call = f.new_call_expression(
            f.new_parenthesized_expression(module_func),
            None,
            None,
            f.new_node_list(vec![module_arg]),
            NodeFlags::None,
        );
        let module_statement = f.new_expression_statement(module_call);
        emit_context.set_original(module_statement, node);
        emit_context.assign_comment_and_source_map_ranges(module_statement, node);
        emit_context.add_emit_flags(module_statement, emit_flags);
        statements.push(module_statement);
        f.new_syntax_list(alloc_vec(statements))
    }

    // runtimesyntax.go:509
    fn transform_module_body(&self, node: P<Node>, _namespace_local_name: P<Node>) -> P<Node> {
        let saved_current_namespace = self.current_namespace.get();
        let saved_current_scope = self.current_scope.get();
        let saved_current_scope_first_declarations_of_name =
            self.current_scope_first_declarations_of_name.borrow().clone();

        self.current_namespace.set(Some(node));
        *self.current_scope_first_declarations_of_name.borrow_mut() = None;

        let emit_context = self.emit_context();
        let f = self.factory();
        let mut node = node;
        let mut statements: Vec<P<Node>> = Vec::new();
        emit_context.start_variable_environment();

        let mut statements_location = TextRange::default();
        let mut block_location = TextRange::default();
        if let Some(body) = node.body() {
            if body.kind() == Kind::ModuleBlock {
                // visit the children of `node` in advance to capture any references to namespace members
                node = self.visitor().visit_each_child(Some(node)).unwrap();
                let body = node.body().unwrap();
                let block = body.as_module_block();
                statements = block.statements.nodes().to_vec();
                statements_location = block.statements.loc.get();
                block_location = body.loc();
            } else {
                // node.Body.Kind == ast.KindModuleDeclaration
                // !!! Strada didn't do this; why?
                // tx.currentScope = node.AsNode()
                let (visited, _) = self.visitor().visit_slice(alloc_vec(vec![body]));
                statements = visited.to_vec();
                let module_block =
                    get_innermost_module_declaration_from_dotted_module(node).body().unwrap();
                statements_location = module_block
                    .as_module_block()
                    .statements
                    .loc
                    .get()
                    .with_pos(TextPos::Synthetic);
            }
        }

        self.current_namespace.set(saved_current_namespace);
        self.current_scope.set(saved_current_scope);
        *self.current_scope_first_declarations_of_name.borrow_mut() =
            saved_current_scope_first_declarations_of_name;

        let statements = emit_context.end_and_merge_variable_environment(&statements);
        let statement_list = f.new_node_list(statements);
        statement_list.loc.set(statements_location);
        let block = f.new_block(statement_list, true /*multiline*/);
        block.set_loc(block_location);

        //  namespace hello.hi.world {
        //       function foo() {}
        //
        //       // TODO, blah
        //  }
        //
        // should be emitted as
        //
        //  var hello;
        //  (function (hello) {
        //      var hi;
        //      (function (hi) {
        //          var world;
        //          (function (world) {
        //              function foo() { }
        //              // TODO, blah
        //          })(world = hi.world || (hi.world = {}));
        //      })(hi = hello.hi || (hello.hi = {}));
        //  })(hello || (hello = {}));
        //
        // We only want to emit comment on the namespace which contains block body itself, not the containing namespaces.
        if node.body().is_none_or(|body| body.kind() != Kind::ModuleBlock) {
            emit_context.add_emit_flags(block, EmitFlags::NoComments);
        }
        block
    }

    // runtimesyntax.go:585
    fn visit_import_equals_declaration(&self, node: P<Node>) -> P<Node> {
        let n = node.as_import_equals_declaration();
        if n.module_reference.kind() == Kind::ExternalModuleReference {
            return self.visitor().visit_each_child(Some(node)).unwrap();
        }

        let f = self.factory();
        let emit_context = self.emit_context();
        let module_reference = f.create_expression_from_entity_name(n.module_reference);
        emit_context
            .set_emit_flags(module_reference, EmitFlags::NoComments | EmitFlags::NoNestedComments);
        if self.is_export_of_namespace(node) {
            // exports.${name} = ${moduleReference};
            let statement = self.create_export_statement(
                node.name().unwrap(),
                module_reference,
                node.loc(),
                node.loc(),
                node,
            );
            statement.set_loc(node.loc());
            statement
        } else {
            //  export var ${name} = ${moduleReference};
            //  var ${name} = ${moduleReference};
            let var_decl = f.new_variable_declaration(
                node.name().unwrap(),
                None, /*exclamationToken*/
                None, /*type*/
                Some(module_reference),
            );
            emit_context.set_original(var_decl, node);
            let var_list =
                f.new_variable_declaration_list(f.new_node_list(vec![var_decl]), NodeFlags::None);
            let var_modifiers =
                extract_modifiers(emit_context, node.modifiers(), ModifierFlags::Export);
            let var_statement = f.new_variable_statement(var_modifiers, var_list);
            emit_context.set_original(var_statement, node);
            emit_context.assign_comment_and_source_map_ranges(var_statement, node);
            var_statement
        }
    }

    // runtimesyntax.go:613
    fn visit_variable_statement(&self, node: P<Node>) -> Option<P<Node>> {
        if self.is_export_of_namespace(node) {
            let mut expressions: Vec<P<Node>> = Vec::new();
            expressions.extend(
                node.as_variable_statement()
                    .declaration_list
                    .as_variable_declaration_list()
                    .declarations
                    .nodes()
                    .iter()
                    .filter_map(|&declaration| {
                        let v = declaration.as_variable_declaration();
                        v.initializer()?;
                        if ast::is_binding_pattern(declaration.name().unwrap()) {
                            let visited = self.visitor().visit_node(Some(declaration)).unwrap();
                            let callback =
                                |export_name: P<Node>,
                                 export_value: P<Node>,
                                 location: Option<&TextRange>| {
                                    self.create_namespace_export_expression(
                                        export_name,
                                        export_value,
                                        location,
                                    )
                                };
                            let expression = flatten_destructuring_assignment(
                                &self.base,
                                visited,
                                false, /*needsValue*/
                                FlattenLevel::All,
                                Some(&callback),
                            );
                            Some(expression)
                        } else {
                            convert_variable_declaration_to_assignment_expression(
                                self.emit_context(),
                                declaration,
                            )
                        }
                    }),
            );
            if expressions.is_empty() {
                return None;
            }
            let f = self.factory();
            let expression = f.inline_expressions(&expressions).unwrap();
            let statement = f.new_expression_statement(expression);
            self.emit_context().set_original(statement, node);
            self.emit_context().assign_comment_and_source_map_ranges(statement, node);

            // re-visit as the new node
            let saved_current = self.current_node.get();
            self.current_node.set(Some(statement));
            let statement = self.visitor().visit_each_child(Some(statement));
            self.current_node.set(saved_current);
            return statement;
        }
        self.visitor().visit_each_child(Some(node))
    }

    // createNamespaceExportExpression creates an assignment to a namespace member for use as a
    // callback during destructuring flattening.
    // runtimesyntax.go:660
    fn create_namespace_export_expression(
        &self,
        export_name: P<Node>,
        export_value: P<Node>,
        location: Option<&TextRange>,
    ) -> P<Node> {
        let member_name = self.get_namespace_qualified_property(
            self.get_namespace_container_name(self.current_namespace.get().unwrap()),
            export_name,
        );
        let expression = self.factory().new_assignment_expression(member_name, export_value);
        if let Some(location) = location {
            expression.set_loc(*location);
        }
        expression
    }

    // runtimesyntax.go:669
    fn visit_function_declaration(&self, node: P<Node>) -> P<Node> {
        if self.is_export_of_namespace(node) {
            let f = self.factory();
            let mut v = self.visitor();
            let updated = f.update_function_declaration(
                node,
                v.visit_modifiers(extract_modifiers(
                    self.emit_context(),
                    node.modifiers(),
                    !ModifierFlags::Export,
                )),
                node.as_function_declaration().asterisk_token(),
                v.visit_node(node.name()),
                None, /*typeParameters*/
                v.visit_nodes(node.parameter_list()),
                None, /*returnType*/
                None, /*fullSignature*/
                v.visit_node(node.body()),
            );
            let export = self.create_export_statement_for_declaration(node);
            return f.new_syntax_list(alloc_vec(vec![updated, export]));
        }
        self.visitor().visit_each_child(Some(node)).unwrap()
    }

    // runtimesyntax.go:690
    fn get_parameter_properties(constructor: Option<P<Node>>) -> Vec<P<Node>> {
        let mut parameter_properties: Vec<P<Node>> = Vec::new();
        if let Some(constructor) = constructor {
            parameter_properties.extend(constructor.parameters().iter().filter_map(|&parameter| {
                if ast::is_parameter_property_declaration(parameter, constructor) {
                    Some(parameter)
                } else {
                    None
                }
            }));
        }
        parameter_properties
    }

    // runtimesyntax.go:702
    fn visit_class_declaration(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let emit_context = self.emit_context();
        let mut v = self.visitor();
        let exported = self.is_export_of_namespace(node);
        let modifiers = if exported {
            v.visit_modifiers(extract_modifiers(
                emit_context,
                node.modifiers(),
                !ModifierFlags::ExportDefault,
            ))
        } else {
            v.visit_modifiers(node.modifiers())
        };

        let mut name = v.visit_node(node.name());
        if name.is_none()
            && (exported
                || ast::child_is_decorated(
                    self.compiler_options.experimental_decorators.is_true(),
                    node,
                    None,
                )
                || !node.modifier_flags().contains(ModifierFlags::ExportDefault)
                    && ast::node_is_decorated(
                        self.compiler_options.experimental_decorators.is_true(),
                        node,
                        None,
                        None,
                    ))
        {
            name = Some(f.new_generated_name_for_node(node));
        }
        let heritage_clauses = v.visit_nodes(node.as_class_declaration().heritage_clauses());
        let member_list = node.member_list().unwrap();
        let mut members = v.visit_nodes(Some(member_list)).unwrap();
        let parameter_properties = Self::get_parameter_properties(
            member_list.nodes().iter().copied().find(|&m| ast::is_constructor_declaration(m)),
        );

        if !parameter_properties.is_empty() {
            let mut new_members: Vec<P<Node>> = Vec::new();
            new_members.extend(parameter_properties.into_iter().filter_map(|parameter| {
                if ast::is_identifier(parameter.name().unwrap()) {
                    let parameter_property = f.new_property_declaration(
                        None, /*modifiers*/
                        parameter.name().unwrap().clone_node(f),
                        None, /*questionOrExclamationToken*/
                        None, /*type*/
                        None, /*initializer*/
                    );
                    emit_context.set_original(parameter_property, parameter);
                    Some(parameter_property)
                } else {
                    None
                }
            }));
            if !new_members.is_empty() {
                new_members.extend_from_slice(members.nodes());
                members = f.new_node_list(new_members);
                members.loc.set(member_list.loc.get());
            }
        }

        let updated = f.update_class_declaration(
            node,
            modifiers,
            name,
            None, /*typeParameters*/
            heritage_clauses,
            members,
        );
        if exported {
            let export = self.create_export_statement_for_declaration(node);
            return f.new_syntax_list(alloc_vec(vec![updated, export]));
        }
        updated
    }

    // runtimesyntax.go:752
    fn visit_class_expression(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let emit_context = self.emit_context();
        let mut v = self.visitor();
        let modifiers = v.visit_modifiers(extract_modifiers(
            emit_context,
            node.modifiers(),
            !ModifierFlags::ExportDefault,
        ));
        let name = v.visit_node(node.name());
        let heritage_clauses = v.visit_nodes(node.as_class_expression().heritage_clauses());
        let member_list = node.member_list().unwrap();
        let mut members = v.visit_nodes(Some(member_list)).unwrap();
        let parameter_properties = Self::get_parameter_properties(
            member_list.nodes().iter().copied().find(|&m| ast::is_constructor_declaration(m)),
        );

        if !parameter_properties.is_empty() {
            let mut new_members: Vec<P<Node>> = Vec::new();
            new_members.extend(parameter_properties.into_iter().filter_map(|parameter| {
                if ast::is_identifier(parameter.name().unwrap()) {
                    let parameter_property = f.new_property_declaration(
                        None, /*modifiers*/
                        parameter.name().unwrap().clone_node(f),
                        None, /*questionOrExclamationToken*/
                        None, /*type*/
                        None, /*initializer*/
                    );
                    emit_context.set_original(parameter_property, parameter);
                    Some(parameter_property)
                } else {
                    None
                }
            }));
            if !new_members.is_empty() {
                new_members.extend_from_slice(members.nodes());
                members = f.new_node_list(new_members);
                members.loc.set(member_list.loc.get());
            }
        }

        f.update_class_expression(
            node,
            modifiers,
            name,
            None, /*typeParameters*/
            heritage_clauses,
            members,
        )
    }

    // runtimesyntax.go:787
    fn visit_constructor_declaration(&self, node: P<Node>) -> P<Node> {
        let mut v = self.visitor();
        let modifiers = v.visit_modifiers(node.modifiers());
        let parameters = self.emit_context().visit_parameters(node.parameter_list(), &mut v);
        let body = self.visit_constructor_body(node.body().unwrap(), node);
        self.factory().update_constructor_declaration(
            node, modifiers, None, /*typeParameters*/
            parameters, None, /*returnType*/
            None, /*fullSignature*/
            body,
        )
    }

    // runtimesyntax.go:794
    fn visit_constructor_body(&self, body: P<Node>, constructor: P<Node>) -> Option<P<Node>> {
        let emit_context = self.emit_context();
        let parameter_properties = Self::get_parameter_properties(Some(constructor));
        if parameter_properties.is_empty() {
            return emit_context.visit_function_body(Some(body), &mut self.visitor());
        }

        let f = self.factory();
        let grandparent_of_body = self.push_node(body);
        let (saved_current_scope, saved_current_scope_first_declarations_of_name) =
            self.push_scope(body);

        emit_context.start_variable_environment();
        let block = body.as_block();
        let (prologue, rest) = f.split_standard_prologue(block.statements.nodes());
        let mut statements: Vec<P<Node>> = prologue.to_vec();

        // Transform parameters into property assignments. Transforms this:
        //
        //  constructor (public x, public y) {
        //  }
        //
        // Into this:
        //
        //  constructor (x, y) {
        //      this.x = x;
        //      this.y = y;
        //  }
        //

        let mut parameter_property_assignments: Vec<P<Node>> = Vec::new();
        parameter_property_assignments.extend(parameter_properties.into_iter().filter_map(
            |parameter| {
                let parameter_name = parameter.name().unwrap();
                if ast::is_identifier(parameter_name) {
                    let property_name = parameter_name.clone_node(f);
                    property_name.set_parent(parameter_name.parent()); // .Parent set to get node to printback using text from original file instead of processed text; TODO: this should be achievable via EmitFlags instead
                    emit_context.add_emit_flags(
                        property_name,
                        EmitFlags::NoComments | EmitFlags::NoSourceMap,
                    );

                    let local_name = parameter_name.clone_node(f);
                    local_name.set_parent(parameter_name.parent()); // .Parent set to get node to printback using text from original file instead of processed text; TODO: this should be achievable via EmitFlags instead
                    emit_context.add_emit_flags(local_name, EmitFlags::NoComments);

                    let parameter_property =
                        f.new_expression_statement(f.new_assignment_expression(
                            f.new_property_access_expression(
                                f.new_this_expression(),
                                None, /*questionDotToken*/
                                property_name,
                                NodeFlags::None,
                            ),
                            local_name,
                        ));
                    emit_context.set_original(parameter_property, parameter);
                    emit_context.add_emit_flags(parameter_property, EmitFlags::StartOnNewLine);
                    Some(parameter_property)
                } else {
                    None
                }
            },
        ));

        let super_path = find_super_statement_index_path(rest, 0);

        if super_path.is_empty() {
            statements.extend(parameter_property_assignments);
            statements.extend_from_slice(self.visitor().visit_slice(rest).0);
        } else {
            statements.extend(self.transform_constructor_body_worker(
                rest,
                &super_path,
                &parameter_property_assignments,
            ));
        }

        let statements = emit_context.end_and_merge_variable_environment(&statements);
        let statement_list = f.new_node_list(statements);
        statement_list.loc.set(block.statements.loc.get());

        self.pop_scope(saved_current_scope, saved_current_scope_first_declarations_of_name);
        self.pop_node(grandparent_of_body);
        let updated = f.new_block(statement_list /*multiline*/, true);
        emit_context.set_original(updated, body);
        updated.set_loc(body.loc());
        Some(updated)
    }

    // runtimesyntax.go:871
    fn transform_constructor_body_worker(
        &self,
        statements_in: &'static [P<Node>],
        super_path: &[usize],
        initializer_statements: &[P<Node>],
    ) -> Vec<P<Node>> {
        let f = self.factory();
        let mut statements_out: Vec<P<Node>> = Vec::new();
        let super_statement_index = super_path[0];
        let super_statement = statements_in[super_statement_index];

        // visit up to the statement containing `super`
        statements_out.extend_from_slice(
            self.visitor().visit_slice(&statements_in[..super_statement_index]).0,
        );

        // if the statement containing `super` is a `try` statement, transform the body of the `try` block
        if ast::is_try_statement(super_statement) {
            let try_statement = super_statement.as_try_statement();
            let try_block_node = try_statement.try_block;
            let try_block = try_block_node.as_block();

            // keep track of hierarchy as we descend
            let grandparent_of_try_statement = self.push_node(super_statement);
            let grandparent_of_try_block = self.push_node(try_block_node);
            let (saved_current_scope, saved_current_scope_first_declarations_of_name) =
                self.push_scope(try_block_node);

            // visit the `try` block
            let try_block_statements = self.transform_constructor_body_worker(
                try_block.statements.nodes(),
                &super_path[1..],
                initializer_statements,
            );

            // restore hierarchy as we ascend to the `try` statement
            self.pop_scope(saved_current_scope, saved_current_scope_first_declarations_of_name);
            self.pop_node(grandparent_of_try_block);

            let try_block_statement_list = f.new_node_list(try_block_statements);
            try_block_statement_list.loc.set(try_block.statements.loc.get());
            let mut v = self.visitor();
            statements_out.push(f.update_try_statement(
                super_statement,
                f.update_block(try_block_node, try_block_statement_list, try_block.multi_line),
                v.visit_node(try_statement.catch_clause),
                v.visit_node(try_statement.finally_block),
            ));

            // restore hierarchy as we ascend to the parent of the `try` statement
            self.pop_node(grandparent_of_try_statement);
        } else {
            // visit the statement containing `super`
            statements_out.extend_from_slice(
                self.visitor()
                    .visit_slice(&statements_in[super_statement_index..=super_statement_index])
                    .0,
            );

            // insert the initializer statements
            statements_out.extend_from_slice(initializer_statements);
        }

        // visit the statements after `super`
        statements_out.extend_from_slice(
            self.visitor().visit_slice(&statements_in[super_statement_index + 1..]).0,
        );
        statements_out
    }

    // runtimesyntax.go:924
    fn visit_shorthand_property_assignment(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let n = node.as_shorthand_property_assignment();
        let name = node.name().unwrap();
        let exported_or_imported_name = self.visit_expression_identifier(name);
        if exported_or_imported_name != name {
            let mut expression = exported_or_imported_name;
            if let Some(object_assignment_initializer) = n.object_assignment_initializer() {
                let equals_token = n.equals_token.unwrap_or_else(|| f.new_token(Kind::EqualsToken));
                expression = f.new_binary_expression(
                    None, /*modifiers*/
                    expression,
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
                expression,
            );
            updated.set_loc(node.loc());
            self.emit_context().set_original(updated, node);
            self.emit_context().assign_comment_and_source_map_ranges(updated, node);
            return updated;
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

    // runtimesyntax.go:959
    fn visit_identifier(&self, node: P<Node>) -> P<Node> {
        if is_identifier_reference(node, self.parent_node.get().unwrap()) {
            return self.visit_expression_identifier(node);
        }
        node
    }

    // runtimesyntax.go:966
    fn visit_expression_identifier(&self, node: P<Node>) -> P<Node> {
        let emit_context = self.emit_context();
        if (self.current_enum.get().is_some() || self.current_namespace.get().is_some())
            && !is_generated_identifier(emit_context, node)
            && !is_local_name(emit_context, node)
        {
            let location = emit_context.most_original(Some(node)).unwrap();
            let container = self
                .resolver
                .get_referenced_export_container(location, false /*prefixLocals*/);
            if let Some(container) = container
                && (ast::is_enum_declaration(container) || ast::is_module_declaration(container))
            {
                let f = self.factory();
                let container_name = self.get_namespace_container_name(container);

                let member_name = node.clone_node(f);
                emit_context
                    .set_emit_flags(member_name, EmitFlags::NoComments | EmitFlags::NoSourceMap);

                let expression = f.get_namespace_member_name(
                    container_name,
                    member_name,
                    NameOptions { allow_source_maps: true, ..Default::default() },
                );
                emit_context.assign_comment_and_source_map_ranges(expression, node);
                return expression;
            }
        }
        node
    }

    // runtimesyntax.go:985
    fn create_export_statement_for_declaration(&self, node: P<Node>) -> P<Node> {
        let f = self.factory();
        let emit_context = self.emit_context();
        let export_name = f.get_external_module_or_namespace_export_name(
            Some(self.get_namespace_container_name(self.current_namespace.get().unwrap())),
            node,
            false, /*allowComments*/
            true,  /*allowSourceMaps*/
        );
        let local_name = f.get_local_name(node);
        let expression = f.new_assignment_expression(export_name, local_name);
        let mut export_assignment_source_map_range = node.loc();
        if let Some(name) = node.name() {
            export_assignment_source_map_range =
                export_assignment_source_map_range.with_pos(name.pos());
        }
        emit_context.set_source_map_range(expression, export_assignment_source_map_range);

        let statement = f.new_expression_statement(expression);
        let export_statement_source_map_range = node.loc().with_pos(TextPos::Synthetic);
        emit_context.set_source_map_range(statement, export_statement_source_map_range);
        statement
    }

    // runtimesyntax.go:1002
    fn create_export_assignment(
        &self,
        name: P<Node>,
        expression: P<Node>,
        export_assignment_source_map_range: TextRange,
        original: P<Node>,
    ) -> P<Node> {
        let export_name = self.get_namespace_qualified_property(
            self.get_namespace_container_name(self.current_namespace.get().unwrap()),
            name,
        );
        let export_assignment = self.factory().new_assignment_expression(export_name, expression);
        self.emit_context().set_original(export_assignment, original);
        self.emit_context()
            .set_source_map_range(export_assignment, export_assignment_source_map_range);
        export_assignment
    }

    // runtimesyntax.go:1010
    fn create_export_statement(
        &self,
        name: P<Node>,
        expression: P<Node>,
        export_assignment_source_map_range: TextRange,
        export_statement_source_map_range: TextRange,
        original: P<Node>,
    ) -> P<Node> {
        let export_statement =
            self.factory().new_expression_statement(self.create_export_assignment(
                name,
                expression,
                export_assignment_source_map_range,
                original,
            ));
        self.emit_context().set_original(export_statement, original);
        self.emit_context()
            .set_source_map_range(export_statement, export_statement_source_map_range);
        export_statement
    }

    // runtimesyntax.go:1017
    fn should_emit_enum_declaration(&self, node: P<Node>) -> bool {
        !ast::is_enum_const(node) || self.compiler_options.should_preserve_const_enums()
    }

    // runtimesyntax.go:1021
    fn should_emit_module_declaration(&self, node: P<Node>) -> bool {
        let Some(pn) = self.emit_context().parse_node(Some(node)) else {
            // If we can't find a parse tree node, assume the node is instantiated.
            return true;
        };
        ast::is_instantiated_module(pn, self.compiler_options.should_preserve_const_enums())
    }
}

// runtimesyntax.go:1030
#[expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]
pub(crate) fn get_innermost_module_declaration_from_dotted_module(
    module_declaration: P<Node>,
) -> P<Node> {
    let mut module_declaration = module_declaration;
    while let Some(body) = module_declaration.body() {
        if body.kind() != Kind::ModuleDeclaration {
            break;
        }
        module_declaration = body;
    }
    module_declaration
}
