use crate::*;
use rustc_hash::FxHashMap;
use tsrs_ast::*;
use tsrs_core::collections::Set;
use tsrs_core::*;

// nodebuilder_hover.go:16
// isExpanding returns whether the node builder context is operating in hover-expansion mode.
#[expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]
pub(crate) fn is_expanding(ctx: P<NodeBuilderContext>) -> bool {
    ctx.max_expansion_depth.get().is_some()
}

impl NodeBuilderImpl {
    // nodebuilder_hover.go:25
    // expandSymbolForHover produces declaration nodes (class, interface, enum, module) for a symbol
    // for expandable hover. This is a focused alternative to the full symbolTableToDeclarationStatements
    // machinery used by declaration emit — it directly builds the declaration nodes hover needs
    // without the declaration-emit scaffolding (deferred privates, symbol name remapping, export
    // modifier computation, alias resolution, visited symbols tracking).
    pub(crate) fn expand_symbol_for_hover(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
    ) -> Vec<P<Node>> {
        let mut results: Vec<P<Node>> = Vec::new();
        if symbol.flags().intersects(SymbolFlags::Enum) {
            let node = self.expand_enum_decl(c, symbol);
            results.push(node);
        }
        if symbol.flags().intersects(SymbolFlags::Class) {
            let node = self.expand_class_decl(c, symbol);
            results.push(node);
        }
        // Module/namespace before interface (matching Strada ordering for merged declarations)
        if symbol.flags().intersects(SymbolFlags::ValueModule | SymbolFlags::NamespaceModule) {
            let node = self.expand_module_decl(c, symbol);
            results.push(node);
        }
        if symbol.flags().intersects(SymbolFlags::Interface)
            && !symbol.flags().intersects(SymbolFlags::Class)
        {
            let node = self.expand_interface_decl(c, symbol);
            results.push(node);
        }
        results
    }

    // nodebuilder_hover.go:52
    // expandEnumDecl produces an EnumDeclaration node with all members.
    fn expand_enum_decl(&self, c: &mut Checker, symbol: P<Symbol>) -> P<Node> {
        let name = symbol_name(symbol);
        let ctx = self.ctx();
        ctx.approximate_length.set(ctx.approximate_length.get() + 9 + name.len());
        let type_of_symbol = c.get_type_of_symbol(symbol);
        let member_props: Vec<P<Symbol>> = c
            .get_properties_of_type(type_of_symbol)
            .iter()
            .copied()
            .filter(|p| p.flags().intersects(SymbolFlags::EnumMember))
            .collect();
        let mut members: Vec<P<Node>> = Vec::new();
        for (i, &p) in member_props.iter().enumerate() {
            if self.check_truncation_length_if_expanding(c) && i + 3 < member_props.len() - 1 {
                self.ctx().expansion_truncated.set(true);
                members.push(self.f.new_enum_member(
                    self.f.new_string_literal(
                        alloc_str(&format!(" ... {} more ... ", member_props.len() - i - 1)),
                        TokenFlags::None,
                    ),
                    None,
                ));
                let last = member_props[member_props.len() - 1];
                let last_name = self.f.new_identifier(last.name());
                let last_initializer = self.enum_member_initializer(c, last);
                members.push(self.f.new_enum_member(last_name, last_initializer));
                break;
            }
            let member_decl = p.declarations().iter().copied().find(|d| is_enum_member(*d));
            let initializer: Option<P<Node>>;
            if let Some(init) = member_decl.and_then(|d| d.as_enum_member().initializer) {
                initializer = self.f.deep_clone_node(Some(init));
            } else {
                initializer = self.enum_member_initializer(c, p);
            }
            let ctx = self.ctx();
            ctx.approximate_length.set(ctx.approximate_length.get() + 4 + p.name().len());
            if initializer.is_some() {
                ctx.approximate_length.set(ctx.approximate_length.get() + 5); // " = " + value estimate
            }
            members.push(self.f.new_enum_member(self.f.new_identifier(p.name()), initializer));
        }

        let mut const_modifier = ModifierFlags::None;
        if is_const_enum_symbol(symbol) {
            const_modifier = ModifierFlags::Const;
        }
        let mut mods: Option<P<ModifierList>> = None;
        if !const_modifier.is_empty() {
            mods =
                Some(self.f.new_modifier_list(create_modifiers_from_modifier_flags(
                    const_modifier,
                    |k| self.f.new_modifier(k),
                )));
        }
        self.f.new_enum_declaration(
            mods,
            self.f.new_identifier(name),
            self.f.new_node_list(members),
        )
    }

    // nodebuilder_hover.go:92
    fn enum_member_initializer(&self, c: &mut Checker, p: P<Symbol>) -> Option<P<Node>> {
        let member_decl = p.declarations().iter().copied().find(|d| is_enum_member(*d));
        let member_decl = member_decl?;
        let val = c.get_constant_value(member_decl);
        let val = val?;
        match val {
            LiteralValue::String(v) => Some(self.f.new_string_literal(v, TokenFlags::None)),
            LiteralValue::Number(v) => {
                Some(self.f.new_numeric_literal(alloc_str(&v.string()), TokenFlags::None))
            }
            _ => None,
        }
    }

    // nodebuilder_hover.go:111
    // expandClassDecl produces a ClassDeclaration node with heritage clauses and members.
    fn expand_class_decl(&self, c: &mut Checker, symbol: P<Symbol>) -> P<Node> {
        let name = symbol_name(symbol);
        let ctx = self.ctx();
        ctx.approximate_length.set(ctx.approximate_length.get() + 9 + name.len());

        let class_like_declarations: Vec<P<Node>> =
            symbol.declarations().iter().copied().filter(|d| is_class_like(*d)).collect();
        let original_decl = class_like_declarations.first().copied();
        let old_enclosing = ctx.enclosing_declaration.get();
        if let Some(original_decl) = original_decl {
            ctx.enclosing_declaration.set(Some(original_decl));
        }

        let local_params = c.get_local_type_parameters_of_class_or_interface_or_type_alias(symbol);
        let type_param_decls: Vec<P<Node>> =
            local_params.iter().map(|&p| self.type_parameter_to_declaration(c, p)).collect();

        let declared_type = c.get_declared_type_of_class_or_interface(symbol);
        let class_type = c.get_type_with_this_argument(declared_type, None, false);
        let target_type = checker::Checker::get_target_type(class_type).unwrap();
        let base_types = c.get_base_types(target_type);
        let static_type = c.get_type_of_symbol(symbol);
        let is_class =
            static_type.symbol().is_some_and(|s| s.value_declaration().is_some_and(is_class_like));
        let static_base_type =
            if is_class { c.get_base_constructor_type_of_class(declared_type) } else { c.any_type };

        // Heritage clauses
        let heritage_clauses = self.hover_heritage_clauses(c, &class_like_declarations);

        // Instance members via addPropertyToElementList (reusing existing serialization),
        // then convert TypeElements to ClassElements and add class-specific modifiers
        let all_props = c.get_properties_of_type(class_type);
        let symbol_props = Self::filter_inherited_properties(c, class_type, base_types, all_props);
        let public_props: Vec<P<Symbol>> =
            symbol_props.iter().copied().filter(|&s| !is_hash_private(s)).collect();
        let has_private = symbol_props.iter().any(|&s| is_hash_private(s));

        let mut instance_members = self.serialize_properties_with_truncation(c, &public_props, &[]);
        let mut instance_members = type_elements_to_class_elements(&self.f, &mut instance_members);
        let instance_members = self.add_class_modifiers(c, &mut instance_members, false);

        // Static members
        let mut static_props: Vec<P<Symbol>> = Vec::new();
        static_props.extend(c.get_properties_of_type(static_type).iter().copied().filter(|&p| {
            !p.flags().intersects(SymbolFlags::Prototype)
                && p.name() != "prototype"
                && !Self::is_namespace_member(c, p)
        }));
        let mut static_members = self.serialize_properties_with_truncation(c, &static_props, &[]);
        let mut static_members = type_elements_to_class_elements(&self.f, &mut static_members);
        let static_members = self.add_class_modifiers(c, &mut static_members, true);

        // Hash-private members
        let mut private_members: Vec<P<Node>> = Vec::new();
        if has_private {
            let private_props: Vec<P<Symbol>> =
                symbol_props.iter().copied().filter(|&s| is_hash_private(s)).collect();
            private_members =
                self.serialize_properties_with_truncation(c, &private_props, &private_members);
            private_members = type_elements_to_class_elements(&self.f, &mut private_members);
        }

        // Constructors
        let constructors =
            self.serialize_constructors(c, static_type, Some(static_base_type), is_class, symbol);

        // Index signatures
        let index_sigs =
            self.serialize_index_signatures_of_type(c, class_type, base_types.first().copied());

        let mut all_members: Vec<P<Node>> = Vec::with_capacity(
            index_sigs.len()
                + static_members.len()
                + constructors.len()
                + instance_members.len()
                + private_members.len(),
        );
        all_members.extend_from_slice(&index_sigs);
        all_members.extend_from_slice(&static_members);
        all_members.extend_from_slice(&constructors);
        all_members.extend_from_slice(&instance_members);
        all_members.extend_from_slice(&private_members);

        let result = self.f.new_class_declaration(
            None,
            Some(self.f.new_identifier(name)),
            Some(self.f.new_node_list(type_param_decls)),
            Some(self.f.new_node_list(heritage_clauses)),
            self.f.new_node_list(all_members),
        );
        self.ctx().enclosing_declaration.set(old_enclosing);
        result
    }

    // nodebuilder_hover.go:187
    // addClassModifiers post-processes class member nodes to add class-specific modifiers
    // (private, protected, public, abstract, static) based on the original symbol declarations.
    fn add_class_modifiers(
        &self,
        _c: &mut Checker,
        members: &mut [P<Node>],
        is_static: bool,
    ) -> Vec<P<Node>> {
        for element in members.iter_mut() {
            let m = *element;
            // Find the symbol for this member by matching the property name
            let mut member_symbol: Option<P<Symbol>> = None;
            let member_name = m.name();
            if let Some(member_name) = member_name
                && let Some(sym) = self.id_to_symbol.borrow().get(&member_name).copied()
            {
                member_symbol = Some(sym);
            }
            let Some(member_symbol) = member_symbol else {
                continue;
            };
            let mut mod_flags =
                get_declaration_modifier_flags_from_symbol(member_symbol) & !ModifierFlags::Async;
            if is_static {
                mod_flags |= ModifierFlags::Static;
            }
            if !mod_flags.is_empty() && can_have_modifiers(m) {
                let existing = m.modifier_flags();
                if mod_flags != existing {
                    (*element) = replace_modifiers(
                        &self.f,
                        m,
                        Some(self.f.new_modifier_list(create_modifiers_from_modifier_flags(
                            mod_flags | existing,
                            |k| self.f.new_modifier(k),
                        ))),
                    );
                }
            }
        }
        members.to_vec()
    }
}

// nodebuilder_hover.go:217
// typeElementsToClassElements converts TypeElement nodes (PropertySignature, MethodSignature)
// to their ClassElement equivalents (PropertyDeclaration, MethodDeclaration) so they can be
// used as members of a ClassDeclaration. Nodes that are already ClassElements pass through unchanged.
fn type_elements_to_class_elements(f: &NodeFactory, members: &mut [P<Node>]) -> Vec<P<Node>> {
    for element in members.iter_mut() {
        let m = *element;
        match m.kind() {
            Kind::PropertySignature => {
                (*element) = f.new_property_declaration(
                    m.modifiers(),
                    m.name().unwrap(),
                    m.question_token(),
                    m.type_node(),
                    None,
                );
            }
            Kind::MethodSignature => {
                (*element) = f.new_method_declaration(
                    m.modifiers(),
                    None,
                    m.name().unwrap(),
                    m.question_token(),
                    m.type_parameter_list(),
                    m.parameter_list(),
                    m.type_node(),
                    None,
                    None,
                );
            }
            _ => {}
        }
    }
    members.to_vec()
}

impl NodeBuilderImpl {
    // nodebuilder_hover.go:234
    // expandInterfaceDecl produces an InterfaceDeclaration with members.
    // Reuses addPropertyToElementList for property serialization and
    // signatureToSignatureDeclarationHelper for signatures.
    fn expand_interface_decl(&self, c: &mut Checker, symbol: P<Symbol>) -> P<Node> {
        let name = symbol_name(symbol);
        let ctx = self.ctx();
        ctx.approximate_length.set(ctx.approximate_length.get() + 14 + name.len());

        let interface_type = c.get_declared_type_of_class_or_interface(symbol);
        let interface_declarations: Vec<P<Node>> = symbol
            .declarations()
            .iter()
            .copied()
            .filter(|d| is_interface_declaration(*d))
            .collect();
        let local_params = c.get_local_type_parameters_of_class_or_interface_or_type_alias(symbol);
        let type_param_decls: Vec<P<Node>> =
            local_params.iter().map(|&p| self.type_parameter_to_declaration(c, p)).collect();
        let base_types = c.get_base_types(interface_type);
        let mut base_type: Option<P<Type>> = None;
        if !base_types.is_empty() {
            base_type = Some(c.get_intersection_type(base_types));
        }

        // Members: reuse existing serialization functions
        let resolved = c.resolve_structured_type_members(interface_type);
        let mut members: Vec<P<Node>> = Vec::new();

        // Index signatures, filtering those identical to base
        members.extend(self.serialize_index_signatures_of_type(c, interface_type, base_type));
        // Construct signatures (skip abstract)
        members.extend(resolved.construct_signatures().iter().filter_map(|&sig| {
            if sig.flags.get().intersects(SignatureFlags::Abstract) {
                return None;
            }
            Some(self.signature_to_signature_declaration_helper(
                c,
                sig,
                Kind::ConstructSignature,
                None,
            ))
        }));
        // Call signatures
        members.extend(resolved.call_signatures().iter().map(|&sig| {
            self.signature_to_signature_declaration_helper(c, sig, Kind::CallSignature, None)
        }));
        // Properties, filtering inherited
        let filtered_props =
            Self::filter_inherited_properties(c, interface_type, base_types, resolved.properties());
        members = self.serialize_properties_with_truncation(c, &filtered_props, &members);

        // Heritage clauses
        let heritage_clauses = self.hover_heritage_clauses(c, &interface_declarations);

        self.f.new_interface_declaration(
            None,
            self.f.new_identifier(name),
            Some(self.f.new_node_list(type_param_decls)),
            Some(self.f.new_node_list(heritage_clauses)),
            self.f.new_node_list(members),
        )
    }

    // nodebuilder_hover.go:275
    fn hover_heritage_clauses(&self, _c: &mut Checker, declarations: &[P<Node>]) -> Vec<P<Node>> {
        let mut extends_types: Vec<P<Node>> = Vec::new();
        let mut implements_types: Vec<P<Node>> = Vec::new();
        for &declaration in declarations {
            extends_types.extend(
                get_extends_heritage_clause_elements(declaration).iter().map(|&heritage_element| {
                    self.f.deep_clone_node(Some(heritage_element)).unwrap()
                }),
            );
            implements_types.extend(
                get_implements_heritage_clause_elements(declaration).iter().map(
                    |&heritage_element| self.f.deep_clone_node(Some(heritage_element)).unwrap(),
                ),
            );
        }

        let mut heritage_clauses: Vec<P<Node>> = Vec::new();
        if !extends_types.is_empty() {
            heritage_clauses.push(
                self.f
                    .new_heritage_clause(Kind::ExtendsKeyword, self.f.new_node_list(extends_types)),
            );
        }
        if !implements_types.is_empty() {
            heritage_clauses.push(self.f.new_heritage_clause(
                Kind::ImplementsKeyword,
                self.f.new_node_list(implements_types),
            ));
        }
        heritage_clauses
    }

    // nodebuilder_hover.go:299
    // serializePropertiesWithTruncation iterates properties using addPropertyToElementList,
    // with truncation checks matching Strada's createTypeNodesFromResolvedType behavior.
    fn serialize_properties_with_truncation(
        &self,
        c: &mut Checker,
        properties: &[P<Symbol>],
        elements: &[P<Node>],
    ) -> Vec<P<Node>> {
        let properties: Vec<P<Symbol>> = properties
            .iter()
            .copied()
            .filter(|p| !p.flags().intersects(SymbolFlags::Prototype))
            .collect();
        let mut elements: Vec<P<Node>> = elements.to_vec();
        for (i, &p) in properties.iter().enumerate() {
            if self.check_truncation_length_if_expanding(c) && i + 3 < properties.len() - 1 {
                self.ctx().expansion_truncated.set(true);
                let text = format!("... {} more ...", properties.len() - i - 1);
                elements.push(self.f.new_property_signature_declaration(
                    None,
                    self.f.new_identifier(alloc_str(&text)),
                    None,
                    None,
                    None,
                ));
                elements = self.add_property_to_element_list(
                    c,
                    properties[properties.len() - 1],
                    &elements,
                );
                break;
            }
            elements = self.add_property_to_element_list(c, p, &elements);
        }
        elements
    }

    // nodebuilder_hover.go:317
    // serializeConstructors builds constructor signature(s) for a class, with base type filtering.
    fn serialize_constructors(
        &self,
        c: &mut Checker,
        static_type: P<Type>,
        static_base_type: Option<P<Type>>,
        is_class: bool,
        symbol: P<Symbol>,
    ) -> Vec<P<Node>> {
        let is_non_constructable = !is_class
            && symbol.value_declaration().is_some_and(is_in_js_file)
            && c.get_signatures_of_type(static_type, SignatureKind::Construct).is_empty();
        if is_non_constructable {
            let ctx = self.ctx();
            ctx.approximate_length.set(ctx.approximate_length.get() + 21);
            let modifiers = create_modifiers_from_modifier_flags(ModifierFlags::Private, |k| {
                self.f.new_modifier(k)
            });
            return vec![self.f.new_constructor_declaration(
                Some(self.f.new_modifier_list(modifiers)),
                None,
                Some(self.f.new_node_list(Vec::new())),
                None,
                None,
                None,
            )];
        }
        let signatures = c.get_signatures_of_type(static_type, SignatureKind::Construct);
        if let Some(static_base_type) = static_base_type {
            let base_sigs = c.get_signatures_of_type(static_base_type, SignatureKind::Construct);
            if base_sigs.is_empty() && signatures.iter().all(|sig| sig.parameters.get().is_empty())
            {
                return Vec::new();
            }
            if base_sigs.len() == signatures.len() {
                let mut all_match = true;
                for (i, element) in base_sigs.iter().enumerate() {
                    if c.compare_signatures_identical(
                        signatures[i],
                        *element,
                        false,
                        false,
                        true,
                        super::checker::Checker::compare_types_identical,
                    ) != Ternary::TRUE
                    {
                        all_match = false;
                        break;
                    }
                }
                if all_match {
                    return Vec::new();
                }
            }
            let mut private_protected = ModifierFlags::None;
            private_protected =
                signatures.iter().fold(private_protected, |mut private_protected, sig| {
                    if let Some(declaration) = sig.declaration.get() {
                        private_protected |= declaration.modifier_flags()
                            & (ModifierFlags::Private | ModifierFlags::Protected);
                    }

                    private_protected
                });
            if !private_protected.is_empty() {
                return vec![self.f.new_constructor_declaration(
                    Some(self.f.new_modifier_list(create_modifiers_from_modifier_flags(
                        private_protected,
                        |k| self.f.new_modifier(k),
                    ))),
                    None,
                    Some(self.f.new_node_list(Vec::new())),
                    None,
                    None,
                    None,
                )];
            }
        } else if signatures.iter().all(|sig| sig.parameters.get().is_empty()) {
            return Vec::new();
        }
        let mut result: Vec<P<Node>> = Vec::new();
        result.extend(signatures.iter().map(|&sig| {
            let ctx = self.ctx();
            ctx.approximate_length.set(ctx.approximate_length.get() + 1);
            self.signature_to_signature_declaration_helper(c, sig, Kind::Constructor, None)
        }));
        result
    }

    // nodebuilder_hover.go:369
    // serializeIndexSignaturesOfType builds index signature declarations, filtering those identical to baseType.
    fn serialize_index_signatures_of_type(
        &self,
        c: &mut Checker,
        input: P<Type>,
        base_type: Option<P<Type>>,
    ) -> Vec<P<Node>> {
        let mut result: Vec<P<Node>> = Vec::new();
        result.extend(c.get_index_infos_of_type(input).iter().copied().filter_map(|info| {
            if let Some(base_type) = base_type {
                let base_info = c.get_index_info_of_type(base_type, info.key_type.get().unwrap());
                if let Some(base_info) = base_info
                    && c.is_type_identical_to(
                        info.value_type.get().unwrap(),
                        base_info.value_type.get().unwrap(),
                    )
                {
                    return None;
                }
            }
            Some(self.index_info_to_index_signature_declaration_helper(c, info, None))
        }));
        result
    }

    // nodebuilder_hover.go:385
    // serializeNamespaceMember produces the appropriate declaration node for a namespace member
    // based on its symbol flags (type alias, enum, class, interface, nested namespace, or variable).
    fn serialize_namespace_member(
        &self,
        c: &mut Checker,
        resolved: P<Symbol>,
        name: &str,
    ) -> P<Node> {
        let flags = resolved.flags();
        if flags.intersects(SymbolFlags::TypeAlias) {
            self.serialize_type_alias_for_namespace(c, resolved, name)
        } else if flags.intersects(SymbolFlags::Enum) {
            self.expand_enum_decl(c, resolved)
        } else if flags.intersects(SymbolFlags::Class) {
            self.expand_class_decl(c, resolved)
        } else if flags.intersects(SymbolFlags::Interface) {
            self.expand_interface_decl(c, resolved)
        } else if flags.intersects(SymbolFlags::ValueModule | SymbolFlags::NamespaceModule) {
            self.expand_module_decl(c, resolved)
        } else {
            let type_of_symbol = c.get_type_of_symbol(resolved);
            let t = c.get_widened_type(type_of_symbol);
            let ctx = self.ctx();
            ctx.approximate_length.set(ctx.approximate_length.get() + name.len() + 5);
            let ident = self.f.new_identifier(alloc_str(name));
            let type_node =
                self.serialize_type_for_declaration(c, None, Some(t), Some(resolved), true);
            self.f.new_variable_statement(
                None,
                self.f.new_variable_declaration_list(
                    self.f.new_node_list(vec![self.f.new_variable_declaration(
                        ident,
                        None,
                        Some(type_node),
                        None,
                    )]),
                    NodeFlags::Let,
                ),
            )
        }
    }

    // nodebuilder_hover.go:413
    // expandModuleDecl produces a ModuleDeclaration with exported members.
    fn expand_module_decl(&self, c: &mut Checker, symbol: P<Symbol>) -> P<Node> {
        struct HoverStatement {
            node: P<Node>,
            is_local: bool, // local declarations (e.g. alias targets) should not get export modifier
        }

        let exports = c.get_exports_of_symbol(symbol);
        let mut members: Vec<P<Symbol>> = Vec::new();
        if let Some(exports) = exports {
            members.extend(exports.values().into_iter().filter(|&sym| {
                // Filter to namespace-relevant members
                if !Self::is_namespace_member(c, sym) {
                    return false;
                }
                if !tsrs_scanner::is_identifier_text(sym.name(), LanguageVariant::Standard) {
                    return false;
                }
                true
            }));
        }
        c.sort_symbols(&mut members);
        let ctx = self.ctx();
        ctx.approximate_length.set(ctx.approximate_length.get() + 14);

        // Use the same name as symbol display.
        let old_flags = ctx.flags.get();
        ctx.flags.set(
            ctx.flags.get()
                | Flags::WriteTypeParametersInQualifiedName
                | Flags::from_bits_retain(SymbolFormatFlags::UseOnlyExternalAliasing.bits()),
        );
        let local_name = self.symbol_to_node(c, symbol, SymbolFlags::All);
        self.ctx().flags.set(old_flags);

        let mut body_stmts: Vec<HoverStatement> = Vec::new();
        let mut emitted_locals: Set<P<Symbol>> = Set::new();
        let mut next = 0usize;
        while next < members.len() {
            let i = next;
            next += 1;
            let m = members[i];
            if self.check_truncation_length_if_expanding(c) && i + 3 < members.len() - 1 {
                self.ctx().expansion_truncated.set(true);
                let text = format!("... ({} more) ...", members.len() - i - 1);
                body_stmts.push(HoverStatement {
                    node: self.f.new_expression_statement(self.f.new_identifier(alloc_str(&text))),
                    is_local: false,
                });
                next = members.len() - 1; // skip to last member
                continue;
            }

            // Handle alias/re-export symbols
            if m.flags().intersects(SymbolFlags::Alias) {
                let alias_decl = checker::Checker::get_declaration_of_alias_symbol(m);
                let target =
                    c.get_target_of_alias_declaration(alias_decl).map(|t| c.get_merged_symbol(t));
                if let Some(target) = target {
                    // If the alias target is a local symbol (not itself an export), emit its declaration first
                    if target.flags().intersects(
                        SymbolFlags::BlockScopedVariable
                            | SymbolFlags::FunctionScopedVariable
                            | SymbolFlags::Property,
                    ) && emitted_locals.add_if_absent(target)
                    {
                        let type_of_target = c.get_type_of_symbol(target);
                        let local_type = c.get_widened_type(type_of_target);
                        let ctx = self.ctx();
                        ctx.approximate_length
                            .set(ctx.approximate_length.get() + target.name().len() + 5);
                        let ident = self.f.new_identifier(target.name());
                        let type_node = self.serialize_type_for_declaration(
                            c,
                            None,
                            Some(local_type),
                            Some(target),
                            true,
                        );
                        let local_stmt = self.f.new_variable_statement(
                            None,
                            self.f.new_variable_declaration_list(
                                self.f.new_node_list(vec![self.f.new_variable_declaration(
                                    ident,
                                    None,
                                    Some(type_node),
                                    None,
                                )]),
                                NodeFlags::Let,
                            ),
                        );
                        body_stmts.push(HoverStatement { node: local_stmt, is_local: true });
                    }
                    let target_name = target.name();
                    let ctx = self.ctx();
                    ctx.approximate_length.set(ctx.approximate_length.get() + 16 + m.name().len());
                    let mut property_name: Option<P<Node>> = None;
                    if m.name() != target_name {
                        property_name = Some(self.f.new_identifier(target_name));
                    }
                    let stmt = self.f.new_export_declaration(
                        None,
                        false,
                        Some(self.f.new_named_exports(self.f.new_node_list(vec![
                            self.f.new_export_specifier(
                                false,
                                property_name,
                                self.f.new_identifier(m.name()),
                            ),
                        ]))),
                        None,
                        None,
                    );
                    body_stmts.push(HoverStatement { node: stmt, is_local: false });
                    continue;
                }
            }

            let resolved = c.resolve_symbol(m);

            // Handle functions as function declarations
            if resolved.flags().intersects(SymbolFlags::Function | SymbolFlags::Method) {
                let t = c.get_type_of_symbol(resolved);
                let sigs = c.get_signatures_of_type(t, SignatureKind::Call);
                body_stmts.extend(sigs.iter().map(|&sig| {
                    let ctx = self.ctx();
                    ctx.approximate_length.set(ctx.approximate_length.get() + 1);
                    let options = P::new(SignatureToSignatureDeclarationOptions {
                        modifiers: &[],
                        name: Some(self.f.new_identifier(m.name())),
                        question_token: None,
                    });
                    let decl = self.signature_to_signature_declaration_helper(
                        c,
                        sig,
                        Kind::FunctionDeclaration,
                        Some(options),
                    );
                    HoverStatement { node: decl, is_local: false }
                }));
                // If the function also has namespace characteristics, emit an empty namespace.
                let merged = c.get_merged_symbol(resolved);
                let has_module_exports = merged
                    .flags()
                    .intersects(SymbolFlags::ValueModule | SymbolFlags::NamespaceModule)
                    && merged.exports().is_some_and(|e| !e.is_empty());
                if !has_module_exports {
                    body_stmts.push(HoverStatement {
                        node: self.f.new_module_declaration(
                            None,
                            Kind::NamespaceKeyword,
                            self.f.new_identifier(m.name()),
                            None, /*attributes*/
                            Some(self.f.new_module_block(self.f.new_node_list(Vec::new()))),
                        ),
                        is_local: false,
                    });
                }
                continue;
            }

            // Handle remaining member kinds (type alias, enum, class, interface, namespace, variable)
            let node = self.serialize_namespace_member(c, resolved, m.name());
            body_stmts.push(HoverStatement { node, is_local: false });
        }

        // Add export modifier to exported statements (skip local declarations and ExportDeclarations).
        for s in &mut body_stmts {
            if s.is_local || is_export_declaration(s.node) {
                continue;
            }
            if can_have_modifiers(s.node) {
                let mf = s.node.modifier_flags() | ModifierFlags::Export;
                s.node = replace_modifiers(
                    &self.f,
                    s.node,
                    Some(self.f.new_modifier_list(create_modifiers_from_modifier_flags(mf, |k| {
                        self.f.new_modifier(k)
                    }))),
                );
            }
        }

        // Collect nodes, stripping export if all statements are exported.
        let mut body_statements: Vec<P<Node>> = body_stmts.iter().map(|s| s.node).collect();
        let all_exported = !body_statements.is_empty()
            && body_statements.iter().all(|&d| has_syntactic_modifier(d, ModifierFlags::Export));
        if all_exported {
            for element in &mut body_statements {
                let stmt = *element;
                if can_have_modifiers(stmt) {
                    let mf = stmt.modifier_flags() & !ModifierFlags::Export;
                    (*element) =
                        replace_modifiers(
                            &self.f,
                            stmt,
                            Some(self.f.new_modifier_list(create_modifiers_from_modifier_flags(
                                mf,
                                |k| self.f.new_modifier(k),
                            ))),
                        );
                }
            }
        }

        let mut keyword = Kind::NamespaceKeyword;
        if !is_identifier(local_name) {
            keyword = Kind::ModuleKeyword;
        }
        let mut attributes: Option<P<Node>> = None;
        let declaration = symbol.declarations().iter().copied().find(|&declaration| {
            is_module_declaration(declaration)
                && declaration.as_module_declaration().attributes.is_some()
        });
        if let Some(declaration) = declaration {
            attributes = self.f.deep_clone_node(declaration.as_module_declaration().attributes);
            self.e.set_emit_flags(attributes.unwrap(), EmitFlags::SingleLine);
        }
        let result = self.f.new_module_declaration(
            None,
            keyword,
            local_name,
            attributes,
            Some(self.f.new_module_block(self.f.new_node_list(body_statements))),
        );
        self.ctx().flags.set(old_flags);
        result
    }

    // nodebuilder_hover.go:558
    // serializeTypeAliasForNamespace produces a TypeAliasDeclaration for a type alias inside a namespace body.
    fn serialize_type_alias_for_namespace(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        name: &str,
    ) -> P<Node> {
        let alias_type = c.get_declared_type_of_type_alias(symbol);
        let type_params = c.get_local_type_parameters_of_class_or_interface_or_type_alias(symbol);
        let type_param_decls: Vec<P<Node>> =
            type_params.iter().map(|&p| self.type_parameter_to_declaration(c, p)).collect();
        let mut restore_flags = self.save_restore_flags(c);
        self.ctx().flags.set(self.ctx().flags.get() | Flags::InTypeAlias);
        let type_node = self.type_to_type_node(c, Some(alias_type));
        restore_flags(c);
        let ctx = self.ctx();
        ctx.approximate_length.set(ctx.approximate_length.get() + 8 + name.len());
        self.f.new_type_alias_declaration(
            None,
            self.f.new_identifier(alloc_str(name)),
            Some(self.f.new_node_list(type_param_decls)),
            type_node,
        )
    }

    // nodebuilder_hover.go:571
    // filterInheritedProperties removes properties already present in base types.
    fn filter_inherited_properties(
        c: &mut Checker,
        t: P<Type>,
        base_types: &[P<Type>],
        properties: &[P<Symbol>],
    ) -> Vec<P<Symbol>> {
        if base_types.is_empty() {
            return properties.to_vec();
        }
        // Build a lookup from property name to symbol for parent-identity comparison.
        let mut props_by_name: FxHashMap<&'static str, P<Symbol>> = FxHashMap::default();
        props_by_name.extend(properties.iter().map(|&p| (p.name(), p)));
        // Collect names of properties inherited unchanged from base types.
        let mut inherited: Set<&'static str> = Set::new();
        for &base in base_types {
            let this_type =
                checker::Checker::get_target_type(t).unwrap().as_interface_type().this_type.get();
            let base_with_this = c.get_type_with_this_argument(base, this_type, false);
            inherited.m.extend(c.get_properties_of_type(base_with_this).iter().filter_map(
                |prop| {
                    if let Some(existing) = props_by_name.get(prop.name()) {
                        if prop.parent() == existing.parent() { Some(prop.name()) } else { None }
                    } else {
                        None
                    }
                },
            ));
        }
        if inherited.is_empty() {
            return properties.to_vec();
        }
        properties.iter().copied().filter(|p| !inherited.has(&p.name())).collect()
    }

    // nodebuilder_hover.go:598
    fn is_namespace_member(_c: &mut Checker, p: P<Symbol>) -> bool {
        p.flags().intersects(SymbolFlags::Type | SymbolFlags::Namespace | SymbolFlags::Alias)
            || !(p.flags().intersects(SymbolFlags::Prototype)
                || p.name() == "prototype"
                || p.value_declaration()
                    .is_some_and(|d| has_static_modifier(d) && is_class_like(d.parent().unwrap())))
    }
}

// nodebuilder_hover.go:603
fn is_hash_private(s: P<Symbol>) -> bool {
    s.value_declaration().is_some_and(|d| d.name().is_some_and(is_private_identifier))
}
