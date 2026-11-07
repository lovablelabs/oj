use crate::*;
use tsrs_ast as ast;
use tsrs_ast::*;

// Non-function declarations in emitresolver.go (hand-ported in nodebuilder_types.rs):
//   var _ (emitresolver.go:17)
//   type JSXLinks (emitresolver.go:20)
//   type DeclarationLinks (emitresolver.go:26)
//   type DeclarationFileLinks (emitresolver.go:30)
//   type EmitResolver (emitresolver.go:34)

// emitresolver.go:45
pub(crate) fn new_emit_resolver(
    checker: &Checker,
    emit_context: P<EmitContext>,
) -> P<EmitResolver> {
    // Go also wires `isValueAliasDeclaration` / `aliasMarkingVisitor` (method values of the `...Worker` methods, which
    // Rust calls directly) and `checkerMu` (dropped: the caller serializes checker access, see nodebuilder_types.rs).
    P::new_in(
        emit_context.factory.is_scratch(),
        EmitResolver {
            reference_resolver: std::cell::OnceCell::new(),
            emit_context,
            request_node_builder: std::cell::OnceCell::new(),
            links: checker.emit_resolver_links,
        },
    )
}

impl EmitResolver {
    pub fn emit_context(&self) -> P<EmitContext> {
        self.emit_context
    }

    fn node_builder(&self, c: &Checker) -> P<NodeBuilder> {
        *self.request_node_builder.get_or_init(|| new_node_builder(c, self.emit_context))
    }

    // emitresolver.go:53
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn get_jsx_factory_entity(&self, c: &mut Checker, location: P<Node>) -> Option<P<Node>> {
        c.get_jsx_factory_entity(Some(location))
    }

    // emitresolver.go:59
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn get_jsx_fragment_factory_entity(
        &self,
        c: &mut Checker,
        location: P<Node>,
    ) -> Option<P<Node>> {
        c.get_jsx_fragment_factory_entity(Some(location))
    }

    // emitresolver.go:65
    pub fn is_optional_parameter_exported(&self, c: &mut Checker, node: P<Node>) -> bool {
        self.is_optional_parameter(c, node)
    }

    // emitresolver.go:71
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn is_late_bound(&self, c: &mut Checker, node: Option<P<Node>>) -> bool {
        // TODO: Require an emitContext to construct an EmitResolver, remove all emitContext arguments
        // node = r.emitContext.ParseNode(node)
        let Some(node) = node else {
            return false;
        };
        if !ast::is_parse_tree_node(node) {
            return false;
        }
        let Some(symbol) = c.get_symbol_of_declaration(node) else {
            return false;
        };
        symbol.check_flags().intersects(CheckFlags::Late)
    }

    // emitresolver.go:89
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn get_enum_member_value(&self, c: &mut Checker, node: P<Node>) -> evaluator::Result {
        // node = r.emitContext.ParseNode(node)
        if !ast::is_parse_tree_node(node) {
            return evaluator::new_result(None, false, false, false);
        }

        c.compute_enum_member_values(node.parent().unwrap());
        if !c.enum_member_links.has(node) {
            return evaluator::new_result(None, false, false, false);
        }
        c.enum_member_links.get(node).value.get()
    }

    // emitresolver.go:104
    pub fn is_declaration_visible_exported(&self, c: &mut Checker, node: P<Node>) -> bool {
        // Only lock on external API func to prevent deadlocks
        self.is_declaration_visible(c, Some(node))
    }

    // emitresolver.go:111
    fn is_declaration_visible(&self, c: &mut Checker, node: Option<P<Node>>) -> bool {
        // node = r.emitContext.ParseNode(node)
        let Some(node) = node else {
            return false;
        };
        if !ast::is_parse_tree_node(node) {
            return false;
        }

        let links = self.links.declaration_links.get(node);
        if links.is_visible.get() == Tristate::Unknown {
            if self.determine_if_declaration_is_visible(c, node) {
                links.is_visible.set(Tristate::True);
            } else {
                links.is_visible.set(Tristate::False);
            }
        }
        links.is_visible.get() == Tristate::True
    }

    // emitresolver.go:131
    fn determine_if_declaration_is_visible(&self, c: &mut Checker, node: P<Node>) -> bool {
        match node.kind() {
            Kind::JSDocCallbackTag
            // | Kind::JSDocEnumTag // !!! TODO: JSDoc @enum support?
            | Kind::JSDocTypedefTag => {
                // Top-level jsdoc type aliases are considered exported
                // First parent is comment node, second is hosting declaration or token; we only care about those tokens or declarations whose parent is a source file
                match node.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
                    Some(p) => ast::is_source_file(p),
                    None => false,
                }
            }
            Kind::BindingElement => self.is_declaration_visible(c, node.parent().unwrap().parent()),
            Kind::VariableDeclaration
            | Kind::ModuleDeclaration
            | Kind::ClassDeclaration
            | Kind::InterfaceDeclaration
            | Kind::TypeAliasDeclaration
            | Kind::JSTypeAliasDeclaration
            | Kind::FunctionDeclaration
            | Kind::EnumDeclaration
            | Kind::ImportEqualsDeclaration => {
                if ast::is_variable_declaration(node) {
                    let name = node.name().unwrap();
                    if ast::is_binding_pattern(name) && name.elements().is_empty() {
                        // If the binding pattern is empty, this variable declaration is not visible
                        return false;
                    }
                    // falls through
                }
                // External module augmentation is always visible
                // A @typedef at top-level in an external module is always visible
                if ast::is_external_module_augmentation(node) || ast::is_implicitly_exported_jsdoc_declaration(node) {
                    return true;
                }
                let parent = ast::get_declaration_container(node).unwrap();
                // If the node is not exported or it is not ambient module element (except import declaration)
                if !(c.get_combined_modifier_flags_cached(node).intersects(ModifierFlags::Export) || node.kind() != Kind::ImportEqualsDeclaration && parent.kind() != Kind::SourceFile && parent.flags().intersects(NodeFlags::Ambient))
                {
                    return ast::is_global_source_file(parent);
                }
                // Exported members/ambient module elements (exception import declaration) are visible if parent is visible
                self.is_declaration_visible(c, Some(parent))
            }

            Kind::PropertyDeclaration
            | Kind::PropertySignature
            | Kind::GetAccessor
            | Kind::SetAccessor
            | Kind::MethodDeclaration
            | Kind::MethodSignature => {
                if c.get_effective_declaration_flags(node, ModifierFlags::Private | ModifierFlags::Protected) != ModifierFlags::None {
                    // Private/protected properties/methods are not visible
                    return false;
                }
                // Public properties/methods are visible if its parents are visible, so:
                self.is_declaration_visible(c, node.parent())
            }

            Kind::Constructor
            | Kind::ConstructSignature
            | Kind::CallSignature
            | Kind::IndexSignature
            | Kind::Parameter
            | Kind::ModuleBlock
            | Kind::FunctionType
            | Kind::ConstructorType
            | Kind::TypeLiteral
            | Kind::TypeReference
            | Kind::ArrayType
            | Kind::TupleType
            | Kind::UnionType
            | Kind::IntersectionType
            | Kind::ParenthesizedType
            | Kind::NamedTupleMember => self.is_declaration_visible(c, node.parent()),

            // Type parameters, source files and namespace exports are always visible
            Kind::TypeParameter | Kind::SourceFile | Kind::NamespaceExportDeclaration => true,
            // An `export {X}` (without a module specifier) is itself a visible re-export of
            // the named binding; it contributes to the symbol's external visibility.
            Kind::ExportSpecifier => {
                let export_decl = node.parent().unwrap().parent().unwrap();
                if ast::is_export_declaration(export_decl) && export_decl.as_export_declaration().module_specifier.is_none() {
                    return self.is_declaration_visible(c, export_decl.parent());
                }
                false
            }

            // Default bindings, import specifiers and namespace imports are visible
            // only on demand, so by default they are not visible.
            // Export assignments do not create name bindings outside the module.
            _ => false,
        }
    }

    // emitresolver.go:236
    pub fn precalculate_declaration_emit_visibility(&self, c: &mut Checker, file: P<SourceFile>) {
        if self.links.declaration_file_links.get(file.as_node()).aliases_marked.get() {
            return;
        }
        self.links.declaration_file_links.get(file.as_node()).aliases_marked.set(true);
        // TODO: Does this even *have* to be an upfront walk? If it's not possible for a
        // import a = a.b.c statement to chain into exposing a statement in a sibling scope,
        // it could at least be pushed into scope entry -  then it wouldn't need to be recursive.
        file.as_node().for_each_child(&mut |n| self.alias_marking_visitor_worker(c, n));
    }
}

// emitresolver.go:249
fn is_common_js_module_exports(node: P<Node>) -> bool {
    if ast::is_binary_expression(node)
        && let Some(parent) = node.parent()
        && ast::is_expression_statement(parent)
        && let Some(grand_parent) = parent.parent()
        && ast::is_source_file(grand_parent)
        && grand_parent.as_source_file().common_js_module_indicator.get().is_some()
    {
        match ast::get_assignment_declaration_kind(node) {
            ast::JSDeclarationKind::ModuleExports | ast::JSDeclarationKind::ExportsProperty => {
                return true;
            }
            _ => {}
        }
    }
    false
}

impl EmitResolver {
    // emitresolver.go:260
    fn alias_marking_visitor_worker(&self, c: &mut Checker, node: P<Node>) -> bool {
        match node.kind() {
            Kind::BinaryExpression => {
                if is_common_js_module_exports(node)
                    && ast::is_identifier(node.as_binary_expression().right())
                {
                    self.mark_linked_aliases(c, node.as_binary_expression().right());
                }
            }
            Kind::ExportAssignment => {
                if node.expression().unwrap().kind() == Kind::Identifier {
                    self.mark_linked_aliases(c, node.expression().unwrap());
                }
            }
            Kind::ExportSpecifier => {
                self.mark_linked_aliases(c, node.property_name_or_name().unwrap());
            }
            _ => {}
        }
        node.for_each_child(&mut |n| self.alias_marking_visitor_worker(c, n))
    }

    // Sets the isVisible link on statements the Identifier or ExportName node points at
    // Follows chains of import d = a.b.c
    // emitresolver.go:278
    fn mark_linked_aliases(&self, c: &mut Checker, node: P<Node>) {
        let mut export_symbol: Option<P<Symbol>> = None;
        if node.kind() != Kind::StringLiteral
            && node
                .parent()
                .is_some_and(|p| ast::is_export_assignment(p) || is_common_js_module_exports(p))
        {
            export_symbol = c.resolve_name(
                Some(node),
                node.text(),
                SymbolFlags::Value
                    | SymbolFlags::Type
                    | SymbolFlags::Namespace
                    | SymbolFlags::Alias,
                None,  /*nameNotFoundMessage*/
                false, /*isUse*/
                false,
            );
        } else if node.parent().unwrap().kind() == Kind::ExportSpecifier {
            export_symbol = c.get_target_of_export_specifier(
                node.parent().unwrap(),
                SymbolFlags::Value
                    | SymbolFlags::Type
                    | SymbolFlags::Namespace
                    | SymbolFlags::Alias,
                false,
            );
        }

        let mut visited: FxHashSet<SymbolId> = FxHashSet::default(); // guard against circular imports
        while let Some(symbol) = export_symbol {
            if !visited.insert(ast::get_symbol_id(symbol)) {
                break;
            }

            let mut next_symbol: Option<P<Symbol>> = None;
            for &declaration in symbol.declarations() {
                self.links.declaration_links.get(declaration).is_visible.set(Tristate::True);

                if ast::is_internal_module_import_equals_declaration(declaration) {
                    // Add the referenced top container visible
                    let internal_module_reference =
                        declaration.as_import_equals_declaration().module_reference;
                    let first_identifier = ast::get_first_identifier(internal_module_reference);
                    let import_symbol = c.resolve_name(
                        Some(declaration),
                        first_identifier.text(),
                        SymbolFlags::Value
                            | SymbolFlags::Type
                            | SymbolFlags::Namespace
                            | SymbolFlags::Alias,
                        None,  /*nameNotFoundMessage*/
                        false, /*isUse*/
                        false,
                    );
                    next_symbol = import_symbol;
                }
            }

            export_symbol = next_symbol;
        }
    }
}

// emitresolver.go:311
pub(crate) fn get_meaning_of_entity_name_reference(entity_name: P<Node>) -> SymbolFlags {
    let parent = entity_name.parent().unwrap();
    // get symbol of the first identifier of the entityName
    if parent.kind() == Kind::TypeQuery
        || parent.kind() == Kind::ExpressionWithTypeArguments && !ast::is_part_of_type_node(parent)
        || parent.kind() == Kind::ComputedPropertyName
        || parent.kind() == Kind::TypePredicate
            && parent.as_type_predicate_node().parameter_name == entity_name
        || parent.kind() == Kind::BinaryExpression
    {
        // Typeof value
        return SymbolFlags::Value | SymbolFlags::ExportValue;
    }
    if entity_name.kind() == Kind::QualifiedName
        || entity_name.kind() == Kind::PropertyAccessExpression
        || parent.kind() == Kind::ImportEqualsDeclaration
        || (parent.kind() == Kind::QualifiedName && parent.as_qualified_name().left == entity_name)
        || (parent.kind() == Kind::PropertyAccessExpression
            && parent.expression() == Some(entity_name))
        || (parent.kind() == Kind::ElementAccessExpression
            && parent.expression() == Some(entity_name))
    {
        // Left identifier from type reference or TypeAlias
        // Entity name of the import declaration
        return SymbolFlags::Namespace;
    }
    // Type Reference or TypeAlias entity = Identifier
    SymbolFlags::Type
}

impl EmitResolver {
    // emitresolver.go:334
    pub fn is_entity_name_visible_exported(
        &self,
        c: &mut Checker,
        entity_name: P<Node>,
        enclosing_declaration: Option<P<Node>>,
    ) -> SymbolAccessibilityResult {
        self.is_entity_name_visible(c, entity_name, enclosing_declaration, true)
    }

    // emitresolver.go:340
    pub(crate) fn is_entity_name_visible(
        &self,
        c: &mut Checker,
        entity_name: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        should_compute_alias_to_make_visible: bool,
    ) -> SymbolAccessibilityResult {
        // node = r.emitContext.ParseNode(entityName)
        if !ast::is_parse_tree_node(entity_name) {
            return SymbolAccessibilityResult {
                accessibility: SymbolAccessibility::NotAccessible,
                ..Default::default()
            };
        }

        let meaning = get_meaning_of_entity_name_reference(entity_name);
        let first_identifier = ast::get_first_identifier(entity_name);

        let symbol = c.resolve_name(
            enclosing_declaration,
            first_identifier.text(),
            meaning,
            None,
            false,
            false,
        );

        if let Some(symbol) = symbol
            && symbol.flags().intersects(SymbolFlags::TypeParameter)
            && meaning.intersects(SymbolFlags::Type)
        {
            return SymbolAccessibilityResult {
                accessibility: SymbolAccessibility::Accessible,
                ..Default::default()
            };
        }

        if symbol.is_none() && ast::is_this_identifier(first_identifier) {
            let this_container =
                checker::Checker::get_this_container(first_identifier, false, false).unwrap();
            let sym = c.get_symbol_of_declaration(this_container);
            if self
                .is_symbol_accessible(c, sym, enclosing_declaration, meaning, false)
                .accessibility
                == SymbolAccessibility::Accessible
            {
                return SymbolAccessibilityResult {
                    accessibility: SymbolAccessibility::Accessible,
                    ..Default::default()
                };
            }
        }

        let Some(symbol) = symbol else {
            return SymbolAccessibilityResult {
                accessibility: SymbolAccessibility::NotResolved,
                error_symbol_name: first_identifier.text().to_string(),
                error_node: Some(first_identifier),
                ..Default::default()
            };
        };

        let visible =
            self.has_visible_declarations(c, symbol, should_compute_alias_to_make_visible);
        if let Some(visible) = visible {
            return (*visible).clone();
        }

        SymbolAccessibilityResult {
            accessibility: SymbolAccessibility::NotAccessible,
            error_symbol_name: first_identifier.text().to_string(),
            error_node: Some(first_identifier),
            ..Default::default()
        }
    }
}

// emitresolver.go:382
fn noop_add_visible_alias(declaration: P<Node>, aliasing_statement: P<Node>) {
    let _ = (declaration, aliasing_statement);
}

impl EmitResolver {
    // emitresolver.go:384
    pub(crate) fn has_visible_declarations(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        should_compute_alias_to_make_visible: bool,
    ) -> Option<P<SymbolAccessibilityResult>> {
        // Go collects into a map[NodeId]*Node and returns its values in (random) map order; this keeps
        // first-insertion order, overwriting the value on a repeated key like the map assignment does.
        let mut aliases_to_make_visible_set: Option<Vec<(NodeId, P<Node>)>> = None;

        {
            let mut add_visible_alias: Box<dyn FnMut(P<Node>, P<Node>) + '_> =
                if should_compute_alias_to_make_visible {
                    let aliases_to_make_visible_set = &mut aliases_to_make_visible_set;
                    Box::new(move |declaration: P<Node>, aliasing_statement: P<Node>| {
                        self.links
                            .declaration_links
                            .get(declaration)
                            .is_visible
                            .set(Tristate::True);
                        let set = aliases_to_make_visible_set.get_or_insert_with(Vec::new);
                        let id = ast::get_node_id(declaration);
                        match set.iter_mut().find(|(k, _)| *k == id) {
                            Some(entry) => entry.1 = aliasing_statement,
                            None => set.push((id, aliasing_statement)),
                        }
                    })
                } else {
                    Box::new(noop_add_visible_alias)
                };

            let declarations = symbol.declarations().to_vec();
            for declaration in declarations {
                if ast::is_identifier(declaration) {
                    continue;
                }
                if !self.is_declaration_visible(c, Some(declaration)) {
                    // Mark the unexported alias as visible if its parent is visible
                    // because these kind of aliases can be used to name types in declaration file
                    let any_import_syntax = get_any_import_syntax(declaration);
                    if let Some(any_import_syntax) = any_import_syntax
                        && !ast::has_syntactic_modifier(any_import_syntax, ModifierFlags::Export) && // import clause without export
                            self.is_declaration_visible(c, any_import_syntax.parent())
                    {
                        add_visible_alias(declaration, any_import_syntax);
                        continue;
                    }
                    if ast::is_variable_declaration(declaration) {
                        let variable_statement = declaration.parent().unwrap().parent().unwrap();
                        if ast::is_variable_statement(variable_statement)
                            && !ast::has_syntactic_modifier(variable_statement, ModifierFlags::Export) // unexported variable statement
                            && self.is_declaration_visible(c, variable_statement.parent())
                        {
                            add_visible_alias(declaration, variable_statement);
                            continue;
                        }
                    }
                    if ast::is_late_visibility_painted_statement(declaration) // unexported top-level statement
                        && !ast::has_syntactic_modifier(declaration, ModifierFlags::Export)
                        && self.is_declaration_visible(c, declaration.parent())
                    {
                        add_visible_alias(declaration, declaration);
                        continue;
                    }
                    if ast::is_binding_element(declaration) {
                        if symbol.flags().intersects(SymbolFlags::Alias)
                            && ast::is_in_js_file(declaration)
                        {
                            // exported import-like top-level JS require statement
                            let p2 = declaration.parent().and_then(|p| p.parent());
                            if let Some(p2) = p2
                                && ast::is_variable_declaration(p2)
                            {
                                let p4 = p2.parent().unwrap().parent();
                                if let Some(p4) = p4
                                    && ast::is_variable_statement(p4)
                                    && !ast::has_syntactic_modifier(p4, ModifierFlags::Export)
                                {
                                    // check if the thing containing the variable statement is visible (ie, the file)
                                    let p5 = p4.parent();
                                    if p5.is_some() && self.is_declaration_visible(c, p5) {
                                        add_visible_alias(declaration, p4);
                                        continue;
                                    }
                                }
                            }
                        }
                        if symbol.flags().intersects(SymbolFlags::BlockScopedVariable) {
                            let root_declaration =
                                ast::walk_up_binding_elements_and_patterns(declaration);
                            if ast::is_parameter_declaration(root_declaration) {
                                return None;
                            }
                            let variable_statement =
                                root_declaration.parent().unwrap().parent().unwrap();
                            if !ast::is_variable_statement(variable_statement) {
                                return None;
                            }
                            if ast::has_syntactic_modifier(
                                variable_statement,
                                ModifierFlags::Export,
                            ) {
                                continue; // no alias to add, already exported
                            }
                            if !self.is_declaration_visible(c, variable_statement.parent()) {
                                return None; // not visible
                            }
                            add_visible_alias(declaration, variable_statement);
                            continue;
                        }
                    }

                    // Declaration is not visible
                    return None;
                }
            }
        }

        Some(P::new_scratch(SymbolAccessibilityResult {
            accessibility: SymbolAccessibility::Accessible,
            aliases_to_make_visible: aliases_to_make_visible_set
                .unwrap_or_default()
                .into_iter()
                .map(|(_, n)| n)
                .collect(),
            ..Default::default()
        }))
    }

    // emitresolver.go:467
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn is_implementation_of_overload(&self, c: &mut Checker, node: P<Node>) -> bool {
        // node = r.emitContext.ParseNode(node)
        if !ast::is_parse_tree_node(node) {
            return false;
        }
        if ast::node_is_present(node.body()) {
            if ast::is_get_accessor_declaration(node) || ast::is_set_accessor_declaration(node) {
                return false; // Get or set accessors can never be overload implementations, but can have up to 2 signatures
            }
            let symbol = c.get_symbol_of_declaration(node);
            let signatures_of_symbol = c.get_signatures_of_symbol(symbol);
            // If this function body corresponds to function with multiple signature, it is implementation of overload
            // e.g.: function foo(a: string): string;
            //       function foo(a: number): number;
            //       function foo(a: any) { // This is implementation of the overloads
            //           return a;
            //       }
            if signatures_of_symbol.len() > 1 {
                return true;
            }
            // If there is single signature for the symbol, it is overload if that signature isn't coming from the node
            // e.g.: function foo(a: string): string;
            //       function foo(a: any) { // This is implementation of the overloads
            //           return a;
            //       }
            if signatures_of_symbol.len() == 1 {
                let signature = signatures_of_symbol[0];
                if Some(signature) == c.get_signature_of_full_signature_type(node) {
                    return false;
                }
                let declaration = signature.declaration();
                if declaration != Some(node)
                    && !declaration.unwrap().flags().intersects(NodeFlags::JSDoc)
                {
                    return true;
                }
            }
        }
        false
    }

    // emitresolver.go:508
    pub fn is_import_required_by_augmentation(&self, c: &mut Checker, decl: P<Node>) -> bool {
        // node = r.emitContext.ParseNode(node)
        if !ast::is_parse_tree_node(decl) {
            return false;
        }
        let file = ast::get_source_file_of_node(decl).unwrap();
        let Some(file_symbol) = file.as_node().symbol() else {
            // script file
            return false;
        };
        let Some(import_target) = self.get_external_module_file_from_declaration(c, decl) else {
            return false;
        };
        if import_target == file {
            return false;
        }
        let exports = c.get_exports_of_module(file_symbol);
        for s in exports.values() {
            let merged = c.get_merged_symbol(s);
            if merged != s
                && !merged.declarations().is_empty()
                && merged.declarations().iter().any(|&d| {
                    let decl_file = ast::get_source_file_of_node(d);
                    decl_file == Some(import_target)
                })
            {
                return true;
            }
        }
        false
    }

    // emitresolver.go:544
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn is_definitely_reference_to_global_symbol_object(
        &self,
        c: &mut Checker,
        node: P<Node>,
    ) -> bool {
        if !ast::is_property_access_expression(node)
            || !ast::is_identifier(node.name().unwrap())
            || !ast::is_property_access_expression(node.expression().unwrap())
                && !ast::is_identifier(node.expression().unwrap())
        {
            return false;
        }
        let expression = node.expression().unwrap();
        if expression.kind() == Kind::Identifier {
            if expression.text() != "Symbol" {
                return false;
            }
            // Exactly `Symbol.something` and `Symbol` either does not resolve or definitely resolves to the global Symbol
            let resolved = c.get_resolved_symbol(expression);
            return Some(resolved)
                == c.get_global_symbol(
                    "Symbol",
                    SymbolFlags::Value | SymbolFlags::ExportValue,
                    None, /*diagnostic*/
                );
        }
        let inner = expression.expression().unwrap();
        if inner.kind() != Kind::Identifier
            || inner.text() != "globalThis"
            || expression.name().unwrap().text() != "Symbol"
        {
            return false;
        }
        // Exactly `globalThis.Symbol.something` and `globalThis` resolves to the global `globalThis`
        c.get_resolved_symbol(inner) == c.global_this_symbol
    }

    // emitresolver.go:568
    pub fn requires_adding_implicit_undefined_exported(
        &self,
        c: &mut Checker,
        declaration: P<Node>,
        symbol: Option<P<Symbol>>,
        enclosing_declaration: Option<P<Node>>,
    ) -> bool {
        if !ast::is_parse_tree_node(declaration) {
            return false;
        }
        self.requires_adding_implicit_undefined(c, declaration, symbol, enclosing_declaration)
    }

    // emitresolver.go:577
    pub fn requires_adding_implicit_undefined_unsafe(
        &self,
        c: &mut Checker,
        declaration: P<Node>,
        symbol: Option<P<Symbol>>,
        enclosing_declaration: Option<P<Node>>,
    ) -> bool {
        if !ast::is_parse_tree_node(declaration) {
            return false;
        }
        // NO LOCKING - only should be called in contexts that already have a checker lock
        self.requires_adding_implicit_undefined(c, declaration, symbol, enclosing_declaration)
    }

    // emitresolver.go:585
    pub(crate) fn requires_adding_implicit_undefined(
        &self,
        c: &mut Checker,
        declaration: P<Node>,
        symbol: Option<P<Symbol>>,
        enclosing_declaration: Option<P<Node>>,
    ) -> bool {
        // node = r.emitContext.ParseNode(node)
        if !ast::is_parse_tree_node(declaration) {
            return false;
        }
        match declaration.kind() {
            Kind::PropertyDeclaration | Kind::PropertySignature | Kind::JSDocPropertyTag => {
                let symbol = match symbol {
                    Some(s) => s,
                    None => c.get_symbol_of_declaration(declaration).unwrap(),
                };
                let t = c.get_type_of_symbol(symbol);
                let _ = c.mapped_symbol_links.has(symbol);
                symbol.flags().intersects(SymbolFlags::Property)
                    && symbol.flags().intersects(SymbolFlags::Optional)
                    && is_optional_declaration(declaration)
                    && c.reverse_mapped_symbol_links.has(symbol)
                    && c.reverse_mapped_symbol_links.get(symbol).mapped_type.get().is_some()
                    && contains_non_missing_undefined_type(c, t)
            }
            Kind::Parameter | Kind::JSDocParameterTag => self
                .requires_adding_implicit_undefined_worker(c, declaration, enclosing_declaration),
            _ => panic!("Node cannot possibly require adding undefined"),
        }
    }

    // emitresolver.go:605
    fn requires_adding_implicit_undefined_worker(
        &self,
        c: &mut Checker,
        parameter: P<Node>,
        enclosing_declaration: Option<P<Node>>,
    ) -> bool {
        (self.is_required_initialized_parameter(c, parameter, enclosing_declaration)
            || self.is_optional_uninitialized_parameter_property(c, parameter))
            && !self.declared_parameter_type_contains_undefined(c, parameter)
    }

    // emitresolver.go:609
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    fn declared_parameter_type_contains_undefined(
        &self,
        c: &mut Checker,
        parameter: P<Node>,
    ) -> bool {
        // typeNode := getNonlocalEffectiveTypeAnnotationNode(parameter); // !!! JSDoc Support
        let Some(type_node) = parameter.type_node() else {
            return false;
        };
        let t = c.get_type_from_type_node(type_node);
        // allow error type here to avoid confusing errors that the annotation has to contain undefined when it does in cases like this:
        //
        // export function fn(x?: Unresolved | undefined): void {}
        c.is_error_type(t) || checker::Checker::contains_undefined_type(t)
    }

    // emitresolver.go:622
    fn is_optional_uninitialized_parameter_property(
        &self,
        c: &mut Checker,
        parameter: P<Node>,
    ) -> bool {
        c.strict_null_checks
            && self.is_optional_parameter(c, parameter)
            && ( /*isJSDocParameterTag(parameter) ||*/parameter.initializer().is_none()) // !!! TODO: JSDoc support
            && ast::has_syntactic_modifier(parameter, ModifierFlags::ParameterPropertyModifier)
    }

    // emitresolver.go:629
    fn is_required_initialized_parameter(
        &self,
        c: &mut Checker,
        parameter: P<Node>,
        enclosing_declaration: Option<P<Node>>,
    ) -> bool {
        if !c.strict_null_checks || self.is_optional_parameter(c, parameter) || /*isJSDocParameterTag(parameter) ||*/ parameter.initializer().is_none()
        {
            // !!! TODO: JSDoc Support
            return false;
        }
        if ast::has_syntactic_modifier(parameter, ModifierFlags::ParameterPropertyModifier) {
            return enclosing_declaration.is_some()
                && ast::is_function_like_declaration(enclosing_declaration);
        }
        true
    }

    // emitresolver.go:639
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    fn is_optional_parameter(&self, c: &mut Checker, node: P<Node>) -> bool {
        c.is_optional_parameter(node)
    }

    // emitresolver.go:643
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn is_literal_const_declaration(&self, c: &mut Checker, node: P<Node>) -> bool {
        // node = r.emitContext.ParseNode(node)
        if !ast::is_parse_tree_node(node) {
            return false;
        }
        if is_declaration_readonly(node)
            || ast::is_variable_declaration(node) && ast::is_var_const(node)
        {
            let Some(s) = c.get_symbol_of_declaration(node) else {
                return false;
            };
            let t = c.get_type_of_symbol(s);
            return is_fresh_literal_type(t);
        }
        false
    }

    // emitresolver.go:660
    pub fn is_expando_function_declaration_unsafe(&self, c: &mut Checker, node: P<Node>) -> bool {
        // node = r.emitContext.ParseNode(node)
        if !ast::is_parse_tree_node(node) {
            return false;
        }
        // this is substantially different from strada, but so is expando property checking
        let props = self.get_properties_of_container_function(c, Some(node));
        props.into_iter().any(|p| ast::is_expando_property_declaration(p.value_declaration()))
    }

    // emitresolver.go:675
    pub fn is_expando_function_declaration(&self, c: &mut Checker, node: P<Node>) -> bool {
        self.is_expando_function_declaration_unsafe(c, node)
    }

    // emitresolver.go:681
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn is_symbol_accessible(
        &self,
        c: &mut Checker,
        symbol: Option<P<Symbol>>,
        enclosing_declaration: Option<P<Node>>,
        meaning: SymbolFlags,
        should_compute_alias_to_mark_visible: bool,
    ) -> SymbolAccessibilityResult {
        c.is_symbol_accessible(
            symbol,
            enclosing_declaration,
            meaning,
            should_compute_alias_to_mark_visible,
        )
    }
}

// emitresolver.go:693
pub(crate) fn is_const_enum_or_const_enum_only_module(s: P<Symbol>) -> bool {
    is_const_enum_symbol(s) || s.flags().intersects(SymbolFlags::ConstEnumOnlyModule)
}

impl EmitResolver {
    // emitresolver.go:697
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn is_referenced_alias_declaration(&self, c: &mut Checker, node: P<Node>) -> bool {
        if !c.can_collect_symbol_alias_accessibility_data || !ast::is_parse_tree_node(node) {
            return true;
        }

        if ast::is_alias_symbol_declaration(node)
            && let Some(symbol) = c.get_symbol_of_declaration(node)
        {
            let alias_links = c.alias_symbol_links.get(symbol);
            if alias_links.referenced.get() {
                return true;
            }
            let target = alias_links.alias_target.get();
            if let Some(target) = target
                && node.modifier_flags().intersects(ModifierFlags::Export)
                && c.get_symbol_flags(target).intersects(SymbolFlags::Value)
                && (c.compiler_options.should_preserve_const_enums()
                    || !is_const_enum_or_const_enum_only_module(target))
            {
                return true;
            }
        }
        false
    }

    // emitresolver.go:723
    pub fn is_value_alias_declaration(&self, c: &mut Checker, node: P<Node>) -> bool {
        if !c.can_collect_symbol_alias_accessibility_data || !ast::is_parse_tree_node(node) {
            return true;
        }

        self.is_value_alias_declaration_worker(c, node)
    }

    // emitresolver.go:735
    fn is_value_alias_declaration_worker(&self, c: &mut Checker, node: P<Node>) -> bool {
        match node.kind() {
            Kind::ImportEqualsDeclaration => {
                let symbol = c.get_symbol_of_declaration(node);
                return self
                    .is_alias_resolved_to_value(c, symbol, false /*excludeTypeOnlyValues*/);
            }
            Kind::ImportClause
            | Kind::NamespaceImport
            | Kind::ImportSpecifier
            | Kind::ExportSpecifier => {
                let symbol = c.get_symbol_of_declaration(node);
                return symbol.is_some()
                    && self.is_alias_resolved_to_value(
                        c, symbol, true, /*excludeTypeOnlyValues*/
                    );
            }
            Kind::ExportDeclaration => {
                let export_clause = node.as_export_declaration().export_clause;
                // Go's `r.isValueAliasDeclaration` field is the method value `isValueAliasDeclarationWorker`.
                return match export_clause {
                    Some(export_clause) => {
                        ast::is_namespace_export(export_clause)
                            || export_clause
                                .elements()
                                .iter()
                                .any(|&e| self.is_value_alias_declaration_worker(c, e))
                    }
                    None => false,
                };
            }
            Kind::ExportAssignment => {
                if node.expression().is_some_and(|e| e.kind() == Kind::Identifier) {
                    let symbol = c.get_symbol_of_declaration(node);
                    return self.is_alias_resolved_to_value(
                        c, symbol, true, /*excludeTypeOnlyValues*/
                    );
                }
                return true;
            }
            Kind::BinaryExpression
                if is_common_js_module_exports(node)
                    && ast::is_identifier(node.as_binary_expression().right()) =>
            {
                let symbol = c.get_symbol_of_declaration(node);
                return self
                    .is_alias_resolved_to_value(c, symbol, true /*excludeTypeOnlyValues*/);
            }
            _ => {}
        }
        false
    }

    // emitresolver.go:764
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    fn is_alias_resolved_to_value(
        &self,
        c: &mut Checker,
        symbol: Option<P<Symbol>>,
        exclude_type_only_values: bool,
    ) -> bool {
        let Some(symbol) = symbol else {
            return false;
        };
        if let Some(value_declaration) = symbol.value_declaration()
            && let Some(container) = ast::get_source_file_of_node(value_declaration)
        {
            let file_symbol = c.get_symbol_of_declaration(container.as_node());
            // Ensures cjs export assignment is setup, since this symbol may point at, and merge with, the file itself.
            // If we don't, the merge may not have yet occurred, and the flags check below will be missing flags that
            // are added as a result of the merge.
            // (Go's resolveExternalModuleSymbol returns a nil module symbol unchanged.)
            if let Some(file_symbol) = file_symbol {
                c.resolve_external_module_symbol(file_symbol, false /*dontResolveAlias*/);
            }
        }
        let resolved = c.resolve_alias(symbol);
        let target = c.get_export_symbol_of_value_symbol_if_exported(Some(resolved));
        if target == Some(c.unknown_symbol) {
            return !exclude_type_only_values
                || c.get_type_only_alias_declaration(symbol).is_none();
        }
        // const enums and modules that contain only const enums are not considered values from the emit perspective
        // unless 'preserveConstEnums' option is set to true
        c.get_symbol_flags_ex(symbol, exclude_type_only_values, true /*excludeLocalMeanings*/)
            .intersects(SymbolFlags::Value)
            && (c.compiler_options.should_preserve_const_enums()
                || !is_const_enum_or_const_enum_only_module(target.unwrap()))
    }

    // emitresolver.go:789
    pub fn is_top_level_value_import_equals_with_entity_name(
        &self,
        c: &mut Checker,
        node: P<Node>,
    ) -> bool {
        if !c.can_collect_symbol_alias_accessibility_data {
            return true;
        }
        if !ast::is_parse_tree_node(node)
            || node.kind() != Kind::ImportEqualsDeclaration
            || node.parent().unwrap().kind() != Kind::SourceFile
        {
            return false;
        }
        if ast::is_import_equals_declaration(node)
            && (ast::node_is_missing(node.as_import_equals_declaration().module_reference)
                || node.as_import_equals_declaration().module_reference.kind()
                    == Kind::ExternalModuleReference)
        {
            return false;
        }

        let symbol = c.get_symbol_of_declaration(node);
        self.is_alias_resolved_to_value(c, symbol, false /*excludeTypeOnlyValues*/)
    }

    // emitresolver.go:808
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn mark_linked_references_recursively(&self, c: &mut Checker, file: Option<P<SourceFile>>) {
        if let Some(file) = file
            && !ast::is_parse_tree_node(file.as_node())
        {
            return;
        }

        if let Some(file) = file {
            file.as_node().for_each_child(&mut |n| mark_linked_references_recursively_visit(c, n));
        }
    }
}

// The `visit` closure of MarkLinkedReferencesRecursively (emitresolver.go:816).
fn mark_linked_references_recursively_visit(c: &mut Checker, n: P<Node>) -> bool {
    if ast::is_import_equals_declaration(n) && !n.modifier_flags().intersects(ModifierFlags::Export)
    {
        return false; // These are deferred and marked in a chain when referenced
    }
    if ast::is_import_declaration(n) {
        return false; // likewise, these are ultimately what get marked by calls on other nodes - we want to skip them
    }
    c.mark_linked_references(
        n,
        ReferenceHint::Unspecified,
        None, /*propSymbol*/
        None, /*parentType*/
    );
    n.for_each_child(&mut |child| mark_linked_references_recursively_visit(c, child));
    false
}

impl EmitResolver {
    // emitresolver.go:832
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn get_external_module_file_from_declaration(
        &self,
        c: &mut Checker,
        declaration: P<Node>,
    ) -> Option<P<SourceFile>> {
        if !ast::is_parse_tree_node(declaration) {
            return None;
        }

        c.get_external_module_file_from_declaration(declaration)
    }

    // emitresolver.go:842
    fn get_reference_resolver(&self, c: &Checker) -> P<ReferenceResolver<Checker>> {
        *self.reference_resolver.get_or_init(|| {
            tsrs_binder::new_reference_resolver(
                c.compiler_options,
                tsrs_binder::ReferenceResolverHooks {
                    resolve_name: Some(Checker::resolve_name),
                    get_resolved_symbol: Some(Checker::get_resolved_symbol_or_nil),
                    get_merged_symbol: Some(Checker::get_merged_symbol),
                    get_parent_of_symbol: Some(Checker::get_parent_of_symbol),
                    get_symbol_of_declaration: Some(Checker::get_symbol_of_declaration),
                    get_type_only_alias_declaration: Some(
                        Checker::get_type_only_alias_declaration_ex,
                    ),
                    get_export_symbol_of_value_symbol_if_exported: Some(
                        |c: &mut Checker, symbol: P<Symbol>| {
                            c.get_export_symbol_of_value_symbol_if_exported(Some(symbol))
                        },
                    ),
                    get_element_access_expression_name: Some(
                        Checker::try_get_element_access_expression_name,
                    ),
                },
            )
        })
    }

    // emitresolver.go:858
    pub fn get_referenced_export_container(
        &self,
        c: &mut Checker,
        node: P<Node>,
        prefix_locals: bool,
    ) -> Option<P<Node>> /*SourceFile|ModuleDeclaration|EnumDeclaration*/ {
        if !ast::is_parse_tree_node(node) {
            return None;
        }

        self.get_reference_resolver(c).get_referenced_export_container(c, node, prefix_locals)
    }

    // emitresolver.go:869
    pub fn set_referenced_import_declaration(
        &self,
        _c: &mut Checker,
        node: P<Node>,
        ref_: P<Node>,
    ) {
        self.links.jsx_links.get(node).import_ref.set(Some(ref_));
    }

    // emitresolver.go:875
    pub fn get_referenced_import_declaration(
        &self,
        c: &mut Checker,
        node: P<Node>,
    ) -> Option<P<Node>> {
        if !ast::is_parse_tree_node(node) {
            return self.links.jsx_links.get(node).import_ref.get();
        }

        let symbol = c.get_referenced_value_or_alias_symbol(node);
        if ast::is_non_local_alias(symbol, SymbolFlags::Value)
            && c.get_type_only_alias_declaration_ex(symbol.unwrap(), SymbolFlags::Value).is_none()
        {
            return checker::Checker::get_declaration_of_alias_symbol(symbol.unwrap());
        }
        None
    }

    // emitresolver.go:889
    pub fn get_referenced_value_declaration(
        &self,
        c: &mut Checker,
        node: P<Node>,
    ) -> Option<P<Node>> {
        if !ast::is_parse_tree_node(node) {
            return None;
        }

        self.get_reference_resolver(c).get_referenced_value_declaration(c, node)
    }

    // emitresolver.go:900
    pub fn get_referenced_value_declaration_unsafe(
        &self,
        c: &mut Checker,
        node: P<Node>,
    ) -> Option<P<Node>> {
        self.get_reference_resolver(c).get_referenced_value_declaration(c, node)
    }

    // emitresolver.go:904
    pub fn get_referenced_value_declarations(
        &self,
        c: &mut Checker,
        node: P<Node>,
    ) -> Vec<P<Node>> {
        if !ast::is_parse_tree_node(node) {
            return Vec::new();
        }

        self.get_reference_resolver(c).get_referenced_value_declarations(c, node)
    }

    // IsNameResolvable returns `true` if the given `name` resolves to any symbol at `location`
    // emitresolver.go:916
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn is_name_resolvable(
        &self,
        c: &mut Checker,
        location: Option<P<Node>>,
        name: &str,
    ) -> bool {
        let symbol = c.resolve_name(
            location,
            name,
            SymbolFlags::Value | SymbolFlags::Type | SymbolFlags::Namespace,
            None,  /*nameNotFoundMessage*/
            false, /*isUse*/
            false, /*excludeGlobals*/
        );
        symbol.is_some()
    }

    // emitresolver.go:924
    pub fn get_element_access_expression_name(
        &self,
        c: &mut Checker,
        expression: P<Node>,
    ) -> String {
        if !ast::is_parse_tree_node(expression) {
            return String::new();
        }

        self.get_reference_resolver(c).get_element_access_expression_name(c, Some(expression))
    }

    // emitresolver.go:935
    pub fn get_referenced_member_value_declaration(
        &self,
        c: &mut Checker,
        node: P<Node>,
    ) -> Option<P<Node>> {
        if !ast::is_parse_tree_node(node) {
            return None;
        }

        self.get_reference_resolver(c).get_referenced_member_value_declaration(c, node)
    }

    // TODO: the emit resolver being responsible for some amount of node construction is a very leaky abstraction,
    // and requires giving it access to a lot of context it's otherwise not required to have, which also further complicates the API
    // and likely reduces performance. There's probably some refactoring that could be done here to simplify this.

    // emitresolver.go:950
    pub fn create_return_type_of_signature_declaration(
        &self,
        c: &mut Checker,
        signature_declaration: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: &'static dyn SymbolTracker,
    ) -> Option<P<Node>> {
        let emit_context = self.emit_context;
        let Some(original) = emit_context.parse_node(Some(signature_declaration)) else {
            return Some(emit_context.factory.new_keyword_type_node(Kind::AnyKeyword));
        };

        let request_node_builder = self.node_builder(c);
        request_node_builder.serialize_return_type_for_signature(
            c,
            original,
            enclosing_declaration,
            flags,
            internal_flags,
            Some(tracker),
        )
    }

    // emitresolver.go:962
    pub fn create_type_parameters_of_signature_declaration(
        &self,
        c: &mut Checker,
        signature_declaration: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: &'static dyn SymbolTracker,
    ) -> Option<Vec<P<Node>>> {
        let emit_context = self.emit_context;
        let original = emit_context.parse_node(Some(signature_declaration))?;

        let request_node_builder = self.node_builder(c);
        let result = request_node_builder.serialize_type_parameters_for_signature(
            c,
            original,
            enclosing_declaration,
            flags,
            internal_flags,
            Some(tracker),
        );
        // Go's slice is nil exactly when it is empty: typeParametersToTypeParameterDeclarations only appends to a nil
        // slice (or returns nil), and exitContextSlice returns nil on error.
        if result.is_empty() {
            return None;
        }
        Some(result)
    }

    // emitresolver.go:974
    pub fn create_type_of_declaration(
        &self,
        c: &mut Checker,
        declaration: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: &'static dyn SymbolTracker,
    ) -> Option<P<Node>> {
        let emit_context = self.emit_context;
        if emit_context.parse_node(Some(declaration)).is_none() {
            return Some(emit_context.factory.new_keyword_type_node(Kind::AnyKeyword));
        }

        let request_node_builder = self.node_builder(c);
        // // Get type of the symbol if this is the valid symbol otherwise get type at location
        let symbol = c.get_symbol_of_declaration(declaration);
        request_node_builder.serialize_type_for_declaration(
            c,
            declaration,
            symbol,
            enclosing_declaration,
            flags | Flags::MultilineObjectLiterals,
            internal_flags,
            Some(tracker),
        )
    }

    // emitresolver.go:988
    pub fn create_literal_const_value(
        &self,
        c: &mut Checker,
        node: P<Node>,
        tracker: &'static dyn SymbolTracker,
    ) -> Option<P<Node>> {
        let emit_context = self.emit_context;
        let node = emit_context.parse_node(Some(node)).unwrap();
        let symbol = c.get_symbol_of_declaration(node).unwrap();
        // Go checks `t == nil` here ("TODO: How!? Maybe this should be a panic. All symbols should have a type."); the
        // Rust getTypeOfSymbol never returns nil.
        let t = c.get_type_of_symbol(symbol);

        let f = &emit_context.factory;
        let mut enum_result: Option<P<Node>> = None;
        if t.flags().intersects(TypeFlags::EnumLike) {
            let request_node_builder = self.node_builder(c);
            enum_result = request_node_builder.symbol_to_expression(
                c,
                t.symbol().unwrap(),
                SymbolFlags::Value,
                Some(node),
                Flags::None,
                InternalFlags::None,
                Some(tracker),
            );
            // What about regularTrueType/regularFalseType - since those aren't fresh, we never make initializers from them
            // TODO: handle those if this function is ever used for more than initializers in declaration emit
        } else if t == c.true_type {
            enum_result = Some(f.new_keyword_expression(Kind::TrueKeyword));
        } else if t == c.false_type {
            enum_result = Some(f.new_keyword_expression(Kind::FalseKeyword));
        }
        if enum_result.is_some() {
            return enum_result;
        }
        if !t.flags().intersects(TypeFlags::Literal) {
            return None; // non-literal type
        }
        match t.as_literal_type().value.get() {
            Some(LiteralValue::String(value)) => {
                Some(f.new_string_literal(value, TokenFlags::None))
            }
            Some(LiteralValue::Number(value)) => {
                if value.is_inf() {
                    if value > Number(0.0) {
                        return Some(f.new_identifier("Infinity"));
                    }
                    return Some(f.new_prefix_unary_expression(
                        Kind::MinusToken,
                        f.new_identifier("Infinity"),
                    ));
                }
                if value.is_nan() {
                    return Some(f.new_identifier("NaN"));
                }
                if value.abs() != value {
                    // negative
                    return Some(f.new_prefix_unary_expression(
                        Kind::MinusToken,
                        f.new_numeric_literal(f.alloc_text(&value.string()[1..]), TokenFlags::None),
                    ));
                }
                Some(f.new_numeric_literal(f.alloc_text(&value.string()), TokenFlags::None))
            }
            Some(LiteralValue::BigInt(value)) => Some(f.new_big_int_literal(
                f.alloc_text(&(pseudo_big_int_to_string(value) + "n")),
                TokenFlags::None,
            )),
            Some(LiteralValue::Boolean(value)) => {
                let kind = if value { Kind::TrueKeyword } else { Kind::FalseKeyword };
                Some(f.new_keyword_expression(kind))
            }
            None => panic!("unhandled literal const value kind"),
        }
    }

    // emitresolver.go:1049
    pub fn create_type_of_expression(
        &self,
        c: &mut Checker,
        expression: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: &'static dyn SymbolTracker,
    ) -> Option<P<Node>> {
        let emit_context = self.emit_context;
        let Some(expression) = emit_context.parse_node(Some(expression)) else {
            return Some(emit_context.factory.new_keyword_type_node(Kind::AnyKeyword));
        };

        let request_node_builder = self.node_builder(c);
        request_node_builder.serialize_type_for_expression(
            c,
            expression,
            enclosing_declaration,
            flags | Flags::MultilineObjectLiterals,
            internal_flags,
            Some(tracker),
        )
    }

    // emitresolver.go:1061
    pub fn create_late_bound_index_signatures(
        &self,
        c: &mut Checker,
        container: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: &'static dyn SymbolTracker,
    ) -> Vec<P<Node>> {
        let emit_context = self.emit_context;
        let container = emit_context.parse_node(Some(container)).unwrap();

        let sym = container.symbol().unwrap();
        let type_of_sym = c.get_type_of_symbol(sym);
        let static_infos = c.get_index_infos_of_type(type_of_sym).to_vec();
        let instance_index_symbol = c.get_index_symbol(sym);
        let mut instance_infos: Vec<P<IndexInfo>> = Vec::new();
        if let Some(instance_index_symbol) = instance_index_symbol {
            let sibling_symbols =
                c.get_members_of_symbol(sym).map(|m| m.values()).unwrap_or_default();
            instance_infos =
                c.get_index_infos_of_index_symbol(instance_index_symbol, &sibling_symbols);
        }

        let request_node_builder = self.node_builder(c);
        let f = &emit_context.factory;

        let mut result: Vec<P<Node>> = Vec::new();
        for (i, info_list) in [static_infos, instance_infos].into_iter().enumerate() {
            let is_static = i == 0;
            if info_list.is_empty() {
                continue;
            }
            for info in info_list {
                if info.declaration.get().is_some() {
                    continue;
                }
                if info == c.any_base_type_index_info {
                    continue; // inherited, but looks like a late-bound signature because it has no declarations
                }
                let components = info.components.get();
                if !components.is_empty() {
                    // !!! TODO: Complete late-bound index info support - getObjectLiteralIndexInfo does not yet add late bound components to index signatures
                    let all_component_computed_names_serializable = enclosing_declaration.is_some()
                        && components.iter().all(|&comp| {
                            comp.name().is_some_and(|name| {
                                ast::is_computed_property_name(name)
                                    && ast::is_entity_name_expression(name.expression().unwrap())
                                    && self
                                        .is_entity_name_visible(
                                            c,
                                            name.expression().unwrap(),
                                            enclosing_declaration,
                                            false,
                                        )
                                        .accessibility
                                        == SymbolAccessibility::Accessible
                            })
                        });
                    if all_component_computed_names_serializable {
                        for &comp in components {
                            if c.has_late_bindable_name(comp) {
                                // skip late bound props that contribute to the index signature - they'll be preserved via other means
                                continue;
                            }

                            let first_identifier = ast::get_first_identifier(
                                comp.name().unwrap().expression().unwrap(),
                            );
                            let name = c.resolve_name(
                                Some(first_identifier),
                                first_identifier.text(),
                                SymbolFlags::Value | SymbolFlags::ExportValue,
                                None,  /*nameNotFoundMessage*/
                                true,  /*isUse*/
                                false, /*excludeGlobals*/
                            );
                            if let Some(name) = name {
                                tracker.track_symbol(
                                    c,
                                    name,
                                    enclosing_declaration,
                                    SymbolFlags::Value,
                                );
                            }

                            // Go's core.IfElse evaluates both arms, so the static modifier is created either way.
                            let static_modifier = f.new_modifier(Kind::StaticKeyword);
                            let mut mods: Option<Vec<P<Node>>> =
                                if is_static { Some(vec![static_modifier]) } else { None };
                            if info.is_readonly.get() {
                                mods.get_or_insert_with(Vec::new)
                                    .push(f.new_modifier(Kind::ReadonlyKeyword));
                            }

                            // Likewise the modifier list is created even when `mods` is nil.
                            let modifier_list =
                                f.new_modifier_list(mods.clone().unwrap_or_default());
                            let type_of_comp = c.get_type_of_symbol(comp.symbol().unwrap());
                            let decl = f.new_property_declaration(
                                if mods.is_some() { Some(modifier_list) } else { None },
                                comp.name().unwrap(),
                                comp.question_token(),
                                request_node_builder.type_to_type_node(
                                    c,
                                    type_of_comp,
                                    enclosing_declaration,
                                    flags,
                                    internal_flags,
                                    Some(tracker),
                                ),
                                None,
                            );
                            result.push(decl);
                        }
                        continue;
                    }
                }
                let mut node = request_node_builder.index_info_to_index_signature_declaration(
                    c,
                    info,
                    enclosing_declaration,
                    flags,
                    internal_flags,
                    Some(tracker),
                );
                if let Some(n) = node
                    && is_static
                {
                    let mut mod_nodes = vec![f.new_modifier(Kind::StaticKeyword)];
                    mod_nodes.extend_from_slice(n.modifier_nodes());
                    let mods = f.new_modifier_list(mod_nodes);
                    node = Some(f.update_index_signature_declaration(
                        n,
                        Some(mods),
                        n.parameter_list(),
                        n.type_node(),
                    ));
                }
                if let Some(node) = node {
                    result.push(node);
                }
            }
        }
        result
    }

    // emitresolver.go:1151
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn get_effective_declaration_flags(
        &self,
        c: &mut Checker,
        node: P<Node>,
        flags: ModifierFlags,
    ) -> ModifierFlags {
        // node = emitContext.ParseNode(node)
        c.get_effective_declaration_flags(node, flags)
    }

    // emitresolver.go:1158
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn get_constant_value(&self, c: &mut Checker, node: P<Node>) -> Option<LiteralValue> {
        // node = emitContext.ParseNode(node)
        c.get_constant_value(node)
    }

    // emitresolver.go:1165
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn get_type_reference_serialization_kind(
        &self,
        c: &mut Checker,
        type_name: Option<P<Node>>,
        location: Option<P<Node>>,
    ) -> TypeReferenceSerializationKind {
        // typeName = emitContext.ParseNode(typeName)
        // location = emitContext.ParseNode(location)
        let (Some(type_name), Some(_)) = (type_name, location) else {
            return TypeReferenceSerializationKind::Unknown;
        };

        // Resolve the symbol as a value to ensure the type can be reached at runtime during emit.
        let mut is_type_only = false;
        if ast::is_qualified_name(type_name) {
            let root_value_symbol = c.resolve_entity_name(
                ast::get_first_identifier(type_name),
                SymbolFlags::Value,
                true,
                true,
                location,
            );

            if let Some(root_value_symbol) = root_value_symbol
                && !root_value_symbol.declarations().is_empty()
            {
                is_type_only = root_value_symbol
                    .declarations()
                    .iter()
                    .all(|&d| ast::is_type_only_import_or_export_declaration(d));
            }
        }
        let value_symbol =
            c.resolve_entity_name(type_name, SymbolFlags::Value, true, true, location);
        let mut resolved_value_symbol = value_symbol;
        if let Some(value_symbol) = value_symbol
            && value_symbol.flags().intersects(SymbolFlags::Alias)
        {
            resolved_value_symbol = Some(c.resolve_alias(value_symbol));
        }

        is_type_only = is_type_only
            || value_symbol.is_some_and(|s| {
                c.get_type_only_alias_declaration_ex(s, SymbolFlags::Value).is_some()
            });

        // Resolve the symbol as a type so that we can provide a more useful hint for the type serializer.
        let type_symbol = c.resolve_entity_name(type_name, SymbolFlags::Type, true, true, location);
        let mut resolved_type_symbol = type_symbol;
        if let Some(type_symbol) = type_symbol
            && type_symbol.flags().intersects(SymbolFlags::Alias)
        {
            resolved_type_symbol = Some(c.resolve_alias(type_symbol));
        }
        // In case the value symbol can't be resolved (e.g. because of missing declarations), use type symbol for reachability check.
        is_type_only = is_type_only
            || type_symbol.is_some_and(|s| {
                c.get_type_only_alias_declaration_ex(s, SymbolFlags::Type).is_some()
            });

        if let Some(resolved_value_symbol) = resolved_value_symbol
            && Some(resolved_value_symbol) == resolved_type_symbol
        {
            let global_promise_symbol = c.get_global_promise_constructor_symbol();
            if global_promise_symbol.is_some()
                && Some(resolved_value_symbol) == global_promise_symbol
            {
                return TypeReferenceSerializationKind::Promise;
            }

            let constructor_type = c.get_type_of_symbol(resolved_value_symbol);
            if c.is_constructor_type(constructor_type) {
                if is_type_only {
                    return TypeReferenceSerializationKind::TypeWithCallSignature;
                }
                return TypeReferenceSerializationKind::TypeWithConstructSignatureAndValue;
            }
        }

        // We might not be able to resolve type symbol so use unknown type in that case (eg error case)
        let Some(resolved_type_symbol) = resolved_type_symbol else {
            if is_type_only {
                return TypeReferenceSerializationKind::ObjectType;
            }
            return TypeReferenceSerializationKind::Unknown;
        };

        let type_ = c.get_declared_type_of_symbol(resolved_type_symbol);
        if c.is_error_type(type_) {
            if is_type_only {
                return TypeReferenceSerializationKind::ObjectType;
            }
            return TypeReferenceSerializationKind::Unknown;
        }

        if type_.flags().intersects(TypeFlags::AnyOrUnknown) {
            TypeReferenceSerializationKind::ObjectType
        } else if c.is_type_assignable_to_kind(
            type_,
            TypeFlags::Void | TypeFlags::Nullable | TypeFlags::Never,
        ) {
            TypeReferenceSerializationKind::VoidNullableOrNeverType
        } else if c.is_type_assignable_to_kind(type_, TypeFlags::BooleanLike) {
            TypeReferenceSerializationKind::BooleanType
        } else if c.is_type_assignable_to_kind(type_, TypeFlags::NumberLike) {
            TypeReferenceSerializationKind::NumberLikeType
        } else if c.is_type_assignable_to_kind(type_, TypeFlags::BigIntLike) {
            TypeReferenceSerializationKind::BigIntLikeType
        } else if c.is_type_assignable_to_kind(type_, TypeFlags::StringLike) {
            TypeReferenceSerializationKind::StringLikeType
        } else if crate::is_tuple_type(type_) {
            TypeReferenceSerializationKind::ArrayLikeType
        } else if c.is_type_assignable_to_kind(type_, TypeFlags::ESSymbolLike) {
            TypeReferenceSerializationKind::ESSymbolType
        } else if c.is_function_type(type_) {
            TypeReferenceSerializationKind::TypeWithCallSignature
        } else if c.is_array_type(type_) {
            TypeReferenceSerializationKind::ArrayLikeType
        } else {
            TypeReferenceSerializationKind::ObjectType
        }
    }

    // emitresolver.go:1257
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn get_properties_of_container_function(
        &self,
        c: &mut Checker,
        node: Option<P<Node>>,
    ) -> Vec<P<Symbol>> {
        // This is explicitly _not locked_ because it is only called via error reporters invoked via node builder calls
        // to the symbol tracker already within locked contexts.
        let Some(node) = node else {
            return Vec::new();
        };
        let Some(s) = c.get_symbol_of_declaration(node) else {
            return Vec::new();
        };
        let t = c.get_type_of_symbol(s);
        c.get_properties_of_type(t).to_vec()
    }

    // emitresolver.go:1272
    pub fn try_js_type_node_to_type_node(
        &self,
        c: &mut Checker,
        type_node: P<Node>,
        enclosing_declaration: Option<P<Node>>,
        flags: Flags,
        internal_flags: InternalFlags,
        tracker: &'static dyn SymbolTracker,
    ) -> Option<P<Node>> {
        let emit_context = self.emit_context;
        let type_node = emit_context.parse_node(Some(type_node)).unwrap();

        let request_node_builder = self.node_builder(c);
        request_node_builder.try_js_type_node_to_type_node(
            c,
            type_node,
            enclosing_declaration,
            flags,
            internal_flags,
            Some(tracker),
        )
    }

    // IsThisPropertyAssignmentDeclarationRedundant reports whether a JS `this.<name> = ...` expando
    // assignment should be omitted from declaration emit because the member it would synthesize is
    // already provided by an `extends` base type. This mirrors the skip condition in the checker's
    // serializePropertySymbol: an inherited member is redundant when it is identical to the assigned
    // one (same readonly-ness, optionality and type). Inherited accessors and methods are always
    // treated as redundant here, since accessors merge oddly with value assignments (and run via the
    // accessor at runtime), and `this`-expando props carry the ReplaceableByMethod contract, so a
    // rebind such as `this.method = this.method.bind(this)` must not override the base method.
    //
    // Only `extends` base types are considered. Members coming from `implements` clauses are not
    // inherited, so the class must redeclare them, and they are always emitted.
    // emitresolver.go:1292
    #[expect(
        clippy::unused_self,
        reason = "EmitResolver methods implement the instance resolver API used by declaration and JavaScript transforms."
    )]
    pub fn is_this_property_assignment_declaration_redundant(
        &self,
        c: &mut Checker,
        node: Option<P<Node>>,
    ) -> bool {
        let Some(node) = node else {
            return false;
        };

        let Some(s) = c.get_symbol_of_declaration(node) else {
            return false;
        };
        let Some(s_parent) = s.parent() else {
            return false;
        };
        // Go also returns false when getDeclaredTypeOfSymbol is nil; the Rust function never returns nil.
        let parent_type = c.get_declared_type_of_symbol(s_parent);
        if c.get_base_types(parent_type).iter().any(|&base| {
            let Some(base_prop) = c.get_property_of_type(base, s.name()) else {
                return false;
            };
            if base_prop
                .flags()
                .intersects(SymbolFlags::Accessor | SymbolFlags::Method | SymbolFlags::Function)
            {
                return true;
            }
            if c.is_readonly_symbol(base_prop) == c.is_readonly_symbol(s)
                && (s.flags() & SymbolFlags::Optional)
                    == (base_prop.flags() & SymbolFlags::Optional)
                && {
                    let ts = c.get_type_of_symbol(s);
                    let tb = c.get_type_of_symbol(base_prop);
                    c.is_type_identical_to(ts, tb)
                }
            {
                return true;
            }

            false
        }) {
            return true;
        }
        false
    }
}
