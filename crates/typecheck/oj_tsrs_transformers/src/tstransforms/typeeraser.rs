use super::*;

struct TypeEraserTransformer {
    base: Transformer,
    compiler_options: P<CompilerOptions>,
    parent_node: Cell<Option<P<Node>>>,
    current_node: Cell<Option<P<Node>>>,
}

// typeeraser.go:18
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_type_eraser_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let compiler_options = opt.compiler_options;
    let emit_context = opt.context;
    let tx = P::new(TypeEraserTransformer {
        base: Transformer::default(),
        compiler_options,
        parent_node: Cell::new(None),
        current_node: Cell::new(None),
    });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| tx.visit(n)),
        Some(emit_context),
    ))
}

impl TypeEraserTransformer {
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
    // typeeraser.go:26
    fn push_node(&self, node: P<Node>) -> Option<P<Node>> {
        let grandparent_node = self.parent_node.get();
        self.parent_node.set(self.current_node.get());
        self.current_node.set(Some(node));
        grandparent_node
    }

    // Pops the last child node off the ancestor tracking stack, restoring the grandparent node.
    // typeeraser.go:34
    fn pop_node(&self, grandparent_node: Option<P<Node>>) {
        self.current_node.set(self.parent_node.get());
        self.parent_node.set(grandparent_node);
    }

    // typeeraser.go:39
    fn elide(&self, node: P<Node>) -> P<Node> {
        self.emit_context().new_not_emitted_statement(node)
    }

    // typeeraser.go:43
    fn visit(&self, node: P<Node>) -> Option<P<Node>> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsTypeScript) {
            return Some(node);
        }

        if ast::is_statement(node) && ast::has_syntactic_modifier(node, ModifierFlags::Ambient) {
            return Some(self.elide(node));
        }

        let grandparent_node = self.push_node(node);
        let result = self.visit_worker(node);
        self.pop_node(grandparent_node);
        result
    }

    fn visit_worker(&self, node: P<Node>) -> Option<P<Node>> {
        let f = self.factory();
        let mut v = self.visitor();
        match node.kind() {
            // TypeScript accessibility and readonly modifiers are elided
            Kind::PublicKeyword
            | Kind::PrivateKeyword
            | Kind::ProtectedKeyword
            | Kind::AbstractKeyword
            | Kind::OverrideKeyword
            | Kind::ConstKeyword
            | Kind::DeclareKeyword
            | Kind::ReadonlyKeyword
            // TypeScript type nodes are elided.
            | Kind::ArrayType
            | Kind::TupleType
            | Kind::OptionalType
            | Kind::RestType
            | Kind::TypeLiteral
            | Kind::TypePredicate
            | Kind::TypeParameter
            | Kind::AnyKeyword
            | Kind::UnknownKeyword
            | Kind::BooleanKeyword
            | Kind::StringKeyword
            | Kind::NumberKeyword
            | Kind::NeverKeyword
            | Kind::VoidKeyword
            | Kind::SymbolKeyword
            | Kind::ConstructorType
            | Kind::FunctionType
            | Kind::TypeQuery
            | Kind::TypeReference
            | Kind::UnionType
            | Kind::IntersectionType
            | Kind::ConditionalType
            | Kind::ParenthesizedType
            | Kind::ThisType
            | Kind::TypeOperator
            | Kind::IndexedAccessType
            | Kind::MappedType
            | Kind::LiteralType
            // TypeScript index signatures are elided.
            | Kind::IndexSignature => None,

            Kind::InKeyword | Kind::OutKeyword => {
                // TypeScript `in`/`out` variance modifiers are elided. These keywords are only
                // meaningful as modifiers on type parameters (which are themselves elided), but they may
                // appear as a grammar error on other declarations and must not leak into the emitted JS.
                // The `in` binary operator shares this token kind, so only elide when used as a modifier.
                match self.parent_node.get() {
                    Some(parent) if ast::is_binary_expression(parent) => v.visit_each_child(Some(node)),
                    _ => None,
                }
            }

            Kind::JSImportDeclaration => {
                // reparsed commonjs are elided
                None
            }
            Kind::TypeAliasDeclaration | Kind::JSTypeAliasDeclaration | Kind::InterfaceDeclaration => {
                // TypeScript type-only declarations are elided.
                Some(self.elide(node))
            }

            Kind::NamespaceExportDeclaration => {
                // TypeScript namespace export declarations are elided.
                None
            }

            Kind::ModuleDeclaration => {
                if !ast::is_identifier(node.name().unwrap())
                    || !ast::is_instantiated_module(node, self.compiler_options.should_preserve_const_enums())
                    || get_innermost_module_declaration_from_dotted_module(node).body().is_none()
                {
                    // TypeScript module declarations are elided if they are not instantiated or have no body
                    return Some(self.elide(node));
                }
                v.visit_each_child(Some(node))
            }

            Kind::ExpressionWithTypeArguments => {
                let n = node.as_expression_with_type_arguments();
                Some(f.update_expression_with_type_arguments(node, v.visit_node(Some(n.expression)).unwrap(), None))
            }

            Kind::PropertyDeclaration => {
                if self.compiler_options.experimental_decorators.is_true() && ast::has_syntactic_modifier(node, ModifierFlags::Ambient | ModifierFlags::Abstract) && ast::has_decorators(node) {
                    // declare/abstract props with decorators must be preserved until the decorator transform can process them and remove them
                    return Some(f.update_property_declaration(node, v.visit_modifiers(node.modifiers()), v.visit_node(node.name()).unwrap(), None, None, v.visit_node(node.initializer())));
                }
                if ast::has_syntactic_modifier(node, ModifierFlags::Ambient | ModifierFlags::Abstract) {
                    // TypeScript `declare` fields are elided
                    return None;
                }
                Some(f.update_property_declaration(node, v.visit_modifiers(node.modifiers()), v.visit_node(node.name()).unwrap(), None, None, v.visit_node(node.initializer())))
            }

            Kind::Constructor => {
                if ast::node_is_missing(node.body()) {
                    // TypeScript overloads are elided
                    return None;
                }
                Some(f.update_constructor_declaration(node, None, None, v.visit_nodes(node.parameter_list()), None, None, v.visit_node(node.body())))
            }

            Kind::MethodDeclaration => {
                if ast::node_is_missing(node.body()) {
                    // TypeScript overloads are elided
                    return None;
                }
                let n = node.as_method_declaration();
                Some(f.update_method_declaration(
                    node,
                    v.visit_modifiers(node.modifiers()),
                    n.asterisk_token(),
                    v.visit_node(node.name()).unwrap(),
                    None,
                    None,
                    v.visit_nodes(node.parameter_list()),
                    None,
                    None,
                    v.visit_node(node.body()),
                ))
            }

            Kind::GetAccessor => {
                if ast::node_is_missing(node.body()) && ast::has_syntactic_modifier(node, ModifierFlags::Abstract) {
                    // Abstract accessors are elided
                    return None;
                }
                let body = v.visit_node(node.body()).unwrap_or_else(|| f.new_block(f.new_node_list(Vec::new()), false));
                Some(f.update_get_accessor_declaration(node, v.visit_modifiers(node.modifiers()), v.visit_node(node.name()).unwrap(), None, v.visit_nodes(node.parameter_list()), None, None, Some(body)))
            }

            Kind::SetAccessor => {
                if ast::node_is_missing(node.body()) && ast::has_syntactic_modifier(node, ModifierFlags::Abstract) {
                    // Abstract accessors are elided
                    return None;
                }
                let body = v.visit_node(node.body()).unwrap_or_else(|| f.new_block(f.new_node_list(Vec::new()), false));
                Some(f.update_set_accessor_declaration(node, v.visit_modifiers(node.modifiers()), v.visit_node(node.name()).unwrap(), None, v.visit_nodes(node.parameter_list()), None, None, Some(body)))
            }

            Kind::VariableDeclaration => {
                let n = node.as_variable_declaration();
                let updated = f.update_variable_declaration(node, v.visit_node(node.name()).unwrap(), None, None, v.visit_node(node.initializer()));
                if let Some(type_) = n.type_() {
                    self.emit_context().set_type_node(updated.name().unwrap(), type_);
                }
                Some(updated)
            }

            Kind::HeritageClause => {
                let n = node.as_heritage_clause();
                if n.token == Kind::ImplementsKeyword {
                    // TypeScript `implements` clauses are elided
                    return None;
                }
                Some(f.update_heritage_clause(node, n.token, v.visit_nodes(Some(n.types())).unwrap()))
            }

            Kind::ClassDeclaration => {
                let n = node.as_class_declaration();
                Some(f.update_class_declaration(node, v.visit_modifiers(node.modifiers()), v.visit_node(node.name()), None, v.visit_nodes(n.heritage_clauses()), v.visit_nodes(node.member_list()).unwrap()))
            }

            Kind::ClassExpression => {
                let n = node.as_class_expression();
                Some(f.update_class_expression(node, v.visit_modifiers(node.modifiers()), v.visit_node(node.name()), None, v.visit_nodes(n.heritage_clauses()), v.visit_nodes(node.member_list()).unwrap()))
            }

            Kind::FunctionDeclaration => {
                if ast::node_is_missing(node.body()) {
                    // TypeScript overloads are elided
                    return Some(self.elide(node));
                }
                let n = node.as_function_declaration();
                Some(f.update_function_declaration(node, v.visit_modifiers(node.modifiers()), n.asterisk_token(), v.visit_node(node.name()), None, v.visit_nodes(node.parameter_list()), None, None, v.visit_node(node.body())))
            }

            Kind::FunctionExpression => {
                let n = node.as_function_expression();
                Some(f.update_function_expression(node, v.visit_modifiers(node.modifiers()), n.asterisk_token(), v.visit_node(node.name()), None, v.visit_nodes(node.parameter_list()), None, None, v.visit_node(node.body())))
            }

            Kind::ArrowFunction => {
                let n = node.as_arrow_function();
                Some(f.update_arrow_function(node, v.visit_modifiers(node.modifiers()), None, v.visit_nodes(node.parameter_list()), None, None, n.equals_greater_than_token, v.visit_node(node.body())))
            }

            Kind::Parameter => {
                if ast::is_this_parameter(node) {
                    // TypeScript `this` parameters are elided
                    return None;
                }
                let n = node.as_parameter_declaration();
                // preserve parameter property modifiers to be handled by the runtime transformer
                let mut modifiers: Option<P<ModifierList>> = None;
                if ast::is_parameter_property_declaration(node, self.parent_node.get().unwrap()) {
                    modifiers = extract_modifiers(self.emit_context(), node.modifiers(), ModifierFlags::ParameterPropertyModifier);
                }
                // preserve decorators for the decorator transforms
                if ast::has_decorators(node) {
                    let decorators = node.decorators();
                    let (visited, _) = v.visit_slice(alloc_vec(decorators));
                    modifiers = match modifiers {
                        None => Some(f.new_modifier_list(visited.to_vec())),
                        Some(modifiers) => {
                            let mut nodes = modifiers.nodes().to_vec();
                            nodes.extend_from_slice(visited);
                            Some(f.new_modifier_list(nodes))
                        }
                    };
                }
                Some(f.update_parameter_declaration(node, modifiers, n.dot_dot_dot_token(), v.visit_node(node.name()).unwrap(), None, None, v.visit_node(node.initializer())))
            }

            Kind::CallExpression => {
                let n = node.as_call_expression();
                Some(f.update_call_expression(node, v.visit_node(node.expression()).unwrap(), n.question_dot_token(), None, v.visit_nodes(Some(n.arguments)).unwrap(), node.flags()))
            }

            Kind::NewExpression => {
                let n = node.as_new_expression();
                Some(f.update_new_expression(node, v.visit_node(node.expression()).unwrap(), None, v.visit_nodes(n.arguments)))
            }

            Kind::TaggedTemplateExpression => {
                let n = node.as_tagged_template_expression();
                Some(f.update_tagged_template_expression(node, v.visit_node(Some(n.tag)).unwrap(), n.question_dot_token, None, v.visit_node(Some(n.template)).unwrap(), node.flags()))
            }

            Kind::NonNullExpression | Kind::TypeAssertionExpression | Kind::AsExpression | Kind::SatisfiesExpression => {
                let partial = f.new_partially_emitted_expression(v.visit_node(node.expression()).unwrap());
                self.emit_context().set_original(partial, node);
                partial.set_loc(node.loc());
                Some(partial)
            }

            Kind::ParenthesizedExpression => {
                if !ast::is_jsdoc_type_assertion(node) {
                    let n = node.as_parenthesized_expression();
                    let expression = ast::skip_outer_expressions(n.expression(), OuterExpressionKinds::AllExceptAssertionsOrExpressionsWithTypeArguments);
                    if ast::is_assertion_expression(expression) || ast::is_satisfies_expression(expression) {
                        let partial = f.new_partially_emitted_expression(v.visit_node(Some(n.expression())).unwrap());
                        self.emit_context().set_original(partial, node);
                        partial.set_loc(node.loc());
                        return Some(partial);
                    }
                }
                v.visit_each_child(Some(node))
            }

            Kind::JsxSelfClosingElement => {
                let n = node.as_jsx_self_closing_element();
                Some(f.update_jsx_self_closing_element(node, v.visit_node(Some(n.tag_name)).unwrap(), None, v.visit_node(Some(n.attributes)).unwrap()))
            }

            Kind::JsxOpeningElement => {
                let n = node.as_jsx_opening_element();
                Some(f.update_jsx_opening_element(node, v.visit_node(Some(n.tag_name)).unwrap(), None, v.visit_node(Some(n.attributes)).unwrap()))
            }

            Kind::ImportEqualsDeclaration => {
                let n = node.as_import_equals_declaration();
                if n.is_type_only {
                    // elide type-only imports
                    return None;
                }
                v.visit_each_child(Some(node))
            }

            Kind::ImportDeclaration => {
                let n = node.as_import_declaration();
                let Some(import_clause) = n.import_clause else {
                    // Do not elide a side-effect only import declaration.
                    //  import "foo";
                    return Some(node);
                };
                let import_clause = v.visit_node(Some(import_clause))?;
                Some(f.update_import_declaration(node, node.modifiers(), Some(import_clause), n.module_specifier, n.attributes()))
            }

            Kind::ImportClause => {
                let n = node.as_import_clause();
                if node.is_type_only() {
                    // Always elide type-only imports
                    return None;
                }
                let name = node.name();
                let mut named_bindings = v.visit_node(n.named_bindings);
                // Empty {} due to type-only import erasure can be skipped if there is also a default import
                if name.is_some()
                    && named_bindings.is_some_and(|bindings| {
                        ast::is_named_imports(bindings) && bindings.as_named_imports().elements.nodes().is_empty()
                    })
                    && n.named_bindings
                        .is_some_and(|bindings| !bindings.as_named_imports().elements.nodes().is_empty())
                {
                    // the default binding keeps the import; a source-written {} is left as is
                    named_bindings = None;
                }
                if name.is_none() && named_bindings.is_none() {
                    // all import bindings were elided
                    return None;
                }
                Some(f.update_import_clause(node, n.phase_modifier(), name, named_bindings))
            }

            Kind::NamedImports => {
                let n = node.as_named_imports();
                if n.elements.nodes().is_empty() {
                    // Do not elide a side-effect only import declaration.
                    return Some(node);
                }
                let elements = v.visit_nodes(Some(n.elements)).unwrap();
                if !self.compiler_options.verbatim_module_syntax.is_true() && elements.nodes().is_empty() {
                    // all import specifiers were elided
                    return None;
                }
                Some(f.update_named_imports(node, elements))
            }

            Kind::ImportSpecifier => {
                let n = node.as_import_specifier();
                if n.is_type_only {
                    // elide type-only or unused imports
                    return None;
                }
                Some(node)
            }

            Kind::ExportDeclaration => {
                let n = node.as_export_declaration();
                if n.is_type_only {
                    // elide type-only exports
                    return None;
                }
                let mut export_clause: Option<P<Node>> = None;
                if let Some(c) = n.export_clause {
                    export_clause = v.visit_node(Some(c));
                    export_clause?;
                }
                Some(f.update_export_declaration(node, None /*modifiers*/, false /*isTypeOnly*/, export_clause, v.visit_node(n.module_specifier), v.visit_node(n.attributes())))
            }

            Kind::NamedExports => {
                let n = node.as_named_exports();
                if n.elements.nodes().is_empty() {
                    // Do not elide an empty export declaration.
                    return Some(node);
                }

                let elements = v.visit_nodes(Some(n.elements)).unwrap();
                if !self.compiler_options.verbatim_module_syntax.is_true() && elements.nodes().is_empty() {
                    // all export specifiers were elided
                    return None;
                }
                Some(f.update_named_exports(node, elements))
            }

            Kind::ExportSpecifier => {
                let n = node.as_export_specifier();
                if n.is_type_only {
                    // elide unused export
                    return None;
                }
                Some(node)
            }

            Kind::EnumDeclaration => {
                if ast::is_enum_const(node) {
                    return Some(node);
                }
                v.visit_each_child(Some(node))
            }

            _ => v.visit_each_child(Some(node)),
        }
    }
}
