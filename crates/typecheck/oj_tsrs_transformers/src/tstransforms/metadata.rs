use crate::*;

use super::legacydecorators::get_decorators_of_parameters;
use super::typeserializer::{
    MetadataSerializer, MetadataSerializerContext, new_metadata_serializer,
};

const USE_NEW_TYPE_METADATA_FORMAT: bool = false;

struct MetadataTransformer {
    base: Transformer,
    legacy_decorators: bool,
    resolver: Resolver,

    serializer: Cell<Option<P<MetadataSerializer>>>,
    language_version: ScriptTarget,
    strict_null_checks: bool,
    parent: Cell<Option<P<Node>>>,
    current_lexical_scope: Cell<Option<P<Node>>>,
}

// metadata.go:24
#[expect(
    clippy::unnecessary_wraps,
    reason = "Transformer factories share an optional result so disabled transforms can return None."
)]
pub fn new_metadata_transformer(opt: &TransformOptions) -> Option<P<Transformer>> {
    let tx = P::new(MetadataTransformer {
        base: Transformer::default(),
        legacy_decorators: opt.compiler_options.experimental_decorators.is_true(),
        resolver: opt.emit_resolver.unwrap(),
        serializer: Cell::new(None),
        language_version: opt.compiler_options.get_emit_script_target(),
        strict_null_checks: opt
            .compiler_options
            .get_strict_option_value(opt.compiler_options.strict_null_checks),
        parent: Cell::new(None),
        current_lexical_scope: Cell::new(None),
    });
    Some(tx.get().base.new_transformer(
        Rc::new(move |_: &mut NodeVisitor, n: P<Node>| Some(tx.visit(n))),
        Some(opt.context),
    ))
}

impl MetadataTransformer {
    fn emit_context(&self) -> P<EmitContext> {
        self.base.emit_context()
    }

    fn factory(&self) -> &'static printer::NodeFactory {
        self.base.factory()
    }

    fn visitor(&self) -> NodeVisitor {
        self.base.visitor()
    }

    fn serializer(&self) -> P<MetadataSerializer> {
        self.serializer.get().unwrap()
    }

    // metadata.go:34
    fn visit(&self, node: P<Node>) -> P<Node> {
        if !node.subtree_facts().intersects(SubtreeFacts::ContainsDecorators) {
            return node;
        }

        match node.kind() {
            Kind::ClassDeclaration => self.visit_class_declaration(node),
            Kind::ClassExpression => self.visit_class_expression(node),
            Kind::ObjectLiteralExpression => self.visit_object_literal_expression(node),
            Kind::PropertyDeclaration => self.visit_property_declaration(node),
            Kind::MethodDeclaration => self.visit_method_declaration(node),
            Kind::SetAccessor => self.visit_set_accessor(node),
            Kind::GetAccessor => self.visit_get_accessor(node),
            Kind::SourceFile => {
                self.parent.set(None);
                self.current_lexical_scope.set(Some(node));
                self.serializer.set(Some(new_metadata_serializer(
                    self.resolver,
                    self.factory(),
                    self.emit_context(),
                    self.language_version,
                    self.strict_null_checks,
                )));
                let updated = self.visitor().visit_each_child(Some(node)).unwrap();
                self.emit_context()
                    .add_emit_helper(updated, &self.emit_context().read_emit_helpers());
                // Go: defer tx.setCurrentLexicalScope(nil); defer tx.setParent(nil)
                self.set_current_lexical_scope(None);
                self.set_parent(None);
                updated
            }
            Kind::ModuleBlock | Kind::Block | Kind::CaseBlock => {
                let old_scope = self.current_lexical_scope.get();
                self.current_lexical_scope.set(Some(node));
                let result = self.visitor().visit_each_child(Some(node)).unwrap();
                self.set_current_lexical_scope(old_scope);
                result
            }
            _ => self.visitor().visit_each_child(Some(node)).unwrap(),
        }
    }

    // metadata.go:73
    fn set_parent(&self, node: Option<P<Node>>) {
        self.parent.set(node);
    }

    // metadata.go:77
    fn set_current_lexical_scope(&self, node: Option<P<Node>>) {
        self.current_lexical_scope.set(node);
    }

    // metadata.go:81
    fn visit_object_literal_expression(&self, node: P<Node>) -> P<Node> {
        let old_parent = self.parent.get();
        self.parent.set(Some(node));
        let result = self.visitor().visit_each_child(Some(node)).unwrap();
        self.set_parent(old_parent);
        result
    }

    // metadata.go:89
    fn visit_class_expression(&self, node: P<Node>) -> P<Node> {
        let old_parent = self.parent.get();
        self.parent.set(Some(node));

        let result =
            if ast::class_or_constructor_parameter_is_decorated(self.legacy_decorators, node) {
                let c = node.as_class_expression();
                let modifiers = self.inject_class_type_metadata(
                    self.visitor().visit_modifiers(node.modifiers()),
                    node,
                );
                self.factory().update_class_expression(
                    node,
                    modifiers,
                    self.visitor().visit_node(node.name()),
                    self.visitor().visit_nodes(c.type_parameters()),
                    self.visitor().visit_nodes(c.heritage_clauses()),
                    self.visitor().visit_nodes(Some(c.members())).unwrap(),
                )
            } else {
                self.visitor().visit_each_child(Some(node)).unwrap()
            };
        self.set_parent(old_parent);
        result
    }

    // metadata.go:101
    fn visit_class_declaration(&self, node: P<Node>) -> P<Node> {
        let old_parent = self.parent.get();
        self.parent.set(Some(node));

        let result =
            if ast::class_or_constructor_parameter_is_decorated(self.legacy_decorators, node) {
                let c = node.as_class_declaration();
                let modifiers = self.inject_class_type_metadata(
                    self.visitor().visit_modifiers(node.modifiers()),
                    node,
                );
                self.factory().update_class_declaration(
                    node,
                    modifiers,
                    self.visitor().visit_node(node.name()),
                    self.visitor().visit_nodes(c.type_parameters()),
                    self.visitor().visit_nodes(c.heritage_clauses()),
                    self.visitor().visit_nodes(Some(c.members())).unwrap(),
                )
            } else {
                self.visitor().visit_each_child(Some(node)).unwrap()
            };
        self.set_parent(old_parent);
        result
    }

    // metadata.go:121
    fn visit_property_declaration(&self, node: P<Node>) -> P<Node> {
        if !ast::has_decorators(node) {
            return self.visitor().visit_each_child(Some(node)).unwrap();
        }

        let modifiers = self.inject_class_element_type_metadata(
            self.visitor().visit_modifiers(node.modifiers()),
            node,
            self.parent.get(),
        );
        let p = node.as_property_declaration();
        self.factory().update_property_declaration(
            node,
            modifiers,
            self.visitor().visit_node(node.name()).unwrap(),
            self.visitor().visit_node(p.postfix_token()),
            self.visitor().visit_node(p.type_()),
            self.visitor().visit_node(p.initializer()),
        )
    }

    // metadata.go:137
    fn visit_method_declaration(&self, node: P<Node>) -> P<Node> {
        if !ast::has_decorators(node) && get_decorators_of_parameters(Some(node)).is_empty() {
            return self.visitor().visit_each_child(Some(node)).unwrap();
        }

        let modifiers = self.inject_class_element_type_metadata(
            self.visitor().visit_modifiers(node.modifiers()),
            node,
            self.parent.get(),
        );
        let m = node.as_method_declaration();
        self.factory().update_method_declaration(
            node,
            modifiers,
            self.visitor().visit_node(m.asterisk_token()),
            self.visitor().visit_node(node.name()).unwrap(),
            self.visitor().visit_node(m.postfix_token()),
            self.visitor().visit_nodes(m.type_parameters()),
            self.visitor().visit_nodes(node.parameter_list()),
            self.visitor().visit_node(m.type_()),
            self.visitor().visit_node(m.full_signature()),
            self.visitor().visit_node(node.body()),
        )
    }

    // metadata.go:157
    fn visit_set_accessor(&self, node: P<Node>) -> P<Node> {
        if !ast::has_decorators(node) && get_decorators_of_parameters(Some(node)).is_empty() {
            return self.visitor().visit_each_child(Some(node)).unwrap();
        }

        let modifiers = self.inject_class_element_type_metadata(
            self.visitor().visit_modifiers(node.modifiers()),
            node,
            self.parent.get(),
        );
        let a = node.as_set_accessor_declaration();
        self.factory().update_set_accessor_declaration(
            node,
            modifiers,
            self.visitor().visit_node(node.name()).unwrap(),
            self.visitor().visit_nodes(a.type_parameters()),
            self.visitor().visit_nodes(node.parameter_list()),
            self.visitor().visit_node(a.type_()),
            self.visitor().visit_node(a.full_signature()),
            self.visitor().visit_node(node.body()),
        )
    }

    // metadata.go:175
    fn visit_get_accessor(&self, node: P<Node>) -> P<Node> {
        if !ast::has_decorators(node) {
            return self.visitor().visit_each_child(Some(node)).unwrap();
        }

        let modifiers = self.inject_class_element_type_metadata(
            self.visitor().visit_modifiers(node.modifiers()),
            node,
            self.parent.get(),
        );
        let a = node.as_get_accessor_declaration();
        self.factory().update_get_accessor_declaration(
            node,
            modifiers,
            self.visitor().visit_node(node.name()).unwrap(),
            self.visitor().visit_nodes(a.type_parameters()),
            self.visitor().visit_nodes(node.parameter_list()),
            self.visitor().visit_node(a.type_()),
            self.visitor().visit_node(a.full_signature()),
            self.visitor().visit_node(node.body()),
        )
    }

    // metadata.go:193
    fn inject_class_type_metadata(
        &self,
        list: Option<P<ModifierList>>,
        node: P<Node>,
    ) -> Option<P<ModifierList>> {
        let metadata = self.get_type_metadata(node, Some(node));
        if !metadata.is_empty() {
            let original_nodes: &[P<Node>] = match list {
                Some(list) => list.nodes(),
                None => &[],
            };
            if original_nodes.is_empty() {
                let res = self.factory().new_modifier_list(metadata);
                if let Some(list) = list {
                    res.list.loc.set(list.list.loc.get());
                }
                return Some(res);
            }
            let mut modifiers_array: Vec<P<Node>> = Vec::new();
            if ast::is_modifier(original_nodes[0])
                && (original_nodes[0].kind() == Kind::DefaultKeyword
                    || original_nodes[0].kind() == Kind::ExportKeyword)
            {
                modifiers_array.push(original_nodes[0]);
                if original_nodes.len() > 1
                    && (original_nodes[1].kind() == Kind::DefaultKeyword
                        || original_nodes[1].kind() == Kind::ExportKeyword)
                {
                    modifiers_array.push(original_nodes[1]);
                }
            }
            let rest_start = modifiers_array.len();
            modifiers_array
                .extend(original_nodes.iter().copied().filter(|n| ast::is_decorator(*n)));
            modifiers_array.extend(metadata);
            modifiers_array.extend(
                original_nodes[rest_start..].iter().copied().filter(|n| ast::is_modifier(*n)),
            );
            let res = self.factory().new_modifier_list(modifiers_array);
            res.list.loc.set(list.unwrap().list.loc.get());
            return Some(res);
        }
        list
    }

    // metadata.go:225
    fn inject_class_element_type_metadata(
        &self,
        list: Option<P<ModifierList>>,
        node: P<Node>,
        container: Option<P<Node>>,
    ) -> Option<P<ModifierList>> {
        // Go: ast.IsClassLike(nil) is false
        let Some(container) = container.filter(|c| ast::is_class_like(*c)) else {
            return list;
        };
        if !ast::class_element_or_class_element_parameter_is_decorated(
            self.legacy_decorators,
            node,
            container,
        ) {
            return list;
        }
        let metadata = self.get_type_metadata(node, Some(container));
        if !metadata.is_empty() {
            let original_nodes: &[P<Node>] = match list {
                Some(list) => list.nodes(),
                None => &[],
            };
            if original_nodes.is_empty() {
                let res = self.factory().new_modifier_list(metadata);
                if let Some(list) = list {
                    res.list.loc.set(list.list.loc.get());
                }
                return Some(res);
            }
            let mut modifiers_array: Vec<P<Node>> = Vec::new();
            modifiers_array
                .extend(original_nodes.iter().copied().filter(|n| ast::is_decorator(*n)));
            modifiers_array.extend(metadata);
            modifiers_array.extend(original_nodes.iter().copied().filter(|n| ast::is_modifier(*n)));
            let res = self.factory().new_modifier_list(modifiers_array);
            res.list.loc.set(list.unwrap().list.loc.get());
            return Some(res);
        }
        list
    }

    // metadata.go:262
    /*
     * Gets optional type metadata for a declaration.
     *
     * @param node The declaration node.
     */
    fn get_type_metadata(&self, node: P<Node>, container: Option<P<Node>>) -> Vec<P<Node>> {
        // Decorator metadata is not yet supported for ES decorators.
        if !self.legacy_decorators {
            return Vec::new();
        }
        if USE_NEW_TYPE_METADATA_FORMAT {
            return self.get_new_type_metadata(node, container);
        }
        self.get_old_type_metadata(node, container)
    }

    fn serializer_context(&self, container: Option<P<Node>>) -> MetadataSerializerContext {
        MetadataSerializerContext {
            current_lexical_scope: self.current_lexical_scope.get(),
            current_name_scope: container,
            serializing_conditional_type_branch: false,
        }
    }

    // metadata.go:273
    fn get_old_type_metadata(&self, node: P<Node>, container: Option<P<Node>>) -> Vec<P<Node>> {
        let f = self.factory();
        let mut decorators: Vec<P<Node>> = Vec::new();
        if Self::should_add_type_metadata(node) {
            let type_metadata = f.new_metadata_helper(
                "design:type",
                self.serializer().serialize_type_of_node(
                    self.serializer_context(container),
                    node,
                    container,
                ),
            );
            decorators.push(f.new_decorator(type_metadata));
        }
        if Self::should_add_param_types_metadata(node) {
            let param_types_metadata = f.new_metadata_helper(
                "design:paramtypes",
                self.serializer().serialize_parameter_types_of_node(
                    self.serializer_context(container),
                    node,
                    container,
                ),
            );
            decorators.push(f.new_decorator(param_types_metadata));
        }
        if Self::should_add_return_type_metadata(node) {
            let return_type_metadata = f.new_metadata_helper(
                "design:returntype",
                self.serializer()
                    .serialize_return_type_of_node(self.serializer_context(container), node),
            );
            decorators.push(f.new_decorator(return_type_metadata));
        }
        decorators
    }

    // metadata.go:291
    fn get_new_type_metadata(&self, node: P<Node>, container: Option<P<Node>>) -> Vec<P<Node>> {
        let f = self.factory();
        let mut properties: Vec<P<Node>> = Vec::new();
        if Self::should_add_type_metadata(node) {
            properties.push(f.new_property_assignment(
                None,
                f.new_identifier("type"),
                None,
                None,
                f.new_arrow_function(
                    None,
                    None,
                    Some(f.new_node_list(vec![])),
                    None,
                    None,
                    Some(f.new_token(Kind::EqualsGreaterThanToken)),
                    Some(self.serializer().serialize_type_of_node(
                        self.serializer_context(container),
                        node,
                        container,
                    )),
                ),
            ));
        }
        if Self::should_add_param_types_metadata(node) {
            properties.push(f.new_property_assignment(
                None,
                f.new_identifier("paramTypes"),
                None,
                None,
                f.new_arrow_function(
                    None,
                    None,
                    Some(f.new_node_list(vec![])),
                    None,
                    None,
                    Some(f.new_token(Kind::EqualsGreaterThanToken)),
                    Some(self.serializer().serialize_parameter_types_of_node(
                        self.serializer_context(container),
                        node,
                        container,
                    )),
                ),
            ));
        }
        if Self::should_add_return_type_metadata(node) {
            properties.push(f.new_property_assignment(
                None,
                f.new_identifier("returnType"),
                None,
                None,
                f.new_arrow_function(
                    None,
                    None,
                    Some(f.new_node_list(vec![])),
                    None,
                    None,
                    Some(f.new_token(Kind::EqualsGreaterThanToken)),
                    Some(
                        self.serializer().serialize_return_type_of_node(
                            self.serializer_context(container),
                            node,
                        ),
                    ),
                ),
            ));
        }
        if !properties.is_empty() {
            let type_info_metadata = f.new_metadata_helper(
                "design:typeinfo",
                f.new_object_literal_expression(f.new_node_list(properties), true),
            );
            return vec![f.new_decorator(type_info_metadata)];
        }
        Vec::new()
    }

    // metadata.go:349
    /*
     * Determines whether to emit the "design:type" metadata based on the node's kind.
     * The caller should have already tested whether the node has decorators and whether the
     * emitDecoratorMetadata compiler option is set.
     *
     * @param node The node to test.
     */
    fn should_add_type_metadata(node: P<Node>) -> bool {
        matches!(
            node.kind(),
            Kind::MethodDeclaration
                | Kind::GetAccessor
                | Kind::SetAccessor
                | Kind::PropertyDeclaration
        )
    }

    // metadata.go:365
    /*
     * Determines whether to emit the "design:returntype" metadata based on the node's kind.
     * The caller should have already tested whether the node has decorators and whether the
     * emitDecoratorMetadata compiler option is set.
     *
     * @param node The node to test.
     */
    fn should_add_return_type_metadata(node: P<Node>) -> bool {
        node.kind() == Kind::MethodDeclaration
    }

    // metadata.go:377
    /*
     * Determines whether to emit the "design:paramtypes" metadata based on the node's kind.
     * The caller should have already tested whether the node has decorators and whether the
     * emitDecoratorMetadata compiler option is set.
     *
     * @param node The node to test.
     */
    fn should_add_param_types_metadata(node: P<Node>) -> bool {
        match node.kind() {
            Kind::ClassDeclaration | Kind::ClassExpression => {
                ast::get_first_constructor_with_body(node).is_some()
            }
            Kind::MethodDeclaration | Kind::GetAccessor | Kind::SetAccessor => true,
            _ => false,
        }
    }
}
