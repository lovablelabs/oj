#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

use crate::*;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;

// Non-function declarations in checker.go:23954-26122 (hand-ported by checker-foundation):
//   type UnionReduction (checker.go:26072)
//   const (checker.go:26074): UnionReductionNone, UnionReductionLiteral, UnionReductionSubtype

// checker.go:23954
pub(crate) fn is_mutable_tuple_type(t: P<Type>) -> bool {
    is_tuple_type(t) && !t.target_tuple_type().readonly.get()
}

// checker.go:23958
fn is_generic_tuple_type(t: P<Type>) -> bool {
    is_tuple_type(t)
        && t.target_tuple_type().combined_flags.get().intersects(ElementFlags::Variadic)
}

// checker.go:23962
pub(crate) fn is_single_element_generic_tuple_type(t: P<Type>) -> bool {
    is_generic_tuple_type(t) && t.target_tuple_type().element_infos.get().len() == 1
}

impl Checker {
    // checker.go:23966
    // Public for lint rules (tsgolint's shim exposes Checker_isArrayOrTupleType).
    pub(crate) fn is_array_or_tuple_type(&mut self, t: P<Type>) -> bool {
        self.is_array_type(t) || is_tuple_type(t)
    }

    // checker.go:23970
    pub(crate) fn is_mutable_array_or_tuple(&self, t: P<Type>) -> bool {
        self.is_array_type(t) && !self.is_readonly_array_type(t)
            || is_tuple_type(t) && !t.target_tuple_type().readonly.get()
    }

    // checker.go:23974
    pub(crate) fn get_element_type_of_array_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        if self.is_array_type(t) {
            return Some(self.get_type_arguments(t)[0]);
        }
        None
    }

    // checker.go:23981
    pub(crate) fn is_array_like_type(&mut self, t: P<Type>) -> bool {
        // A type is array-like if it is a reference to the global Array or global ReadonlyArray type,
        // or if it is not the undefined or null type and if it is assignable to ReadonlyArray<any>
        self.is_array_type(t)
            || !t.flags().intersects(TypeFlags::Nullable)
                && self.is_type_assignable_to(t, self.any_readonly_array_type)
    }

    // checker.go:23987
    pub(crate) fn is_mutable_array_like_type(&mut self, t: P<Type>) -> bool {
        // A type is mutable-array-like if it is a reference to the global Array type, or if it is not the
        // any, undefined, null or never type and if it is assignable to Array<any>
        self.is_mutable_array_or_tuple(t)
            || !t.flags().intersects(TypeFlags::Any | TypeFlags::Nullable | TypeFlags::Never)
                && self.is_type_assignable_to(t, self.any_array_type)
    }

    // checker.go:23993
    pub(crate) fn is_empty_array_literal_type(&mut self, t: P<Type>) -> bool {
        let element_type = self.get_element_type_of_array_type(t);
        element_type.is_some_and(|e| self.is_empty_literal_type(e))
    }

    // checker.go:23998
    pub(crate) fn is_empty_literal_type(&self, t: P<Type>) -> bool {
        if self.strict_null_checks {
            return t == self.implicit_never_type;
        }
        t == self.undefined_widening_type
    }

    // checker.go:24005
    pub(crate) fn is_tuple_like_type(&mut self, t: P<Type>) -> bool {
        if is_tuple_type(t) || self.get_property_of_type(t, "0").is_some() {
            return true;
        }
        if self.is_array_like_type(t)
            && let Some(length_type) = self.get_type_of_property_of_type(t, "length")
        {
            return every_type(self, length_type, |_, t| {
                t.flags().intersects(TypeFlags::NumberLiteral)
            });
        }
        false
    }

    // checker.go:24017
    pub(crate) fn is_array_or_tuple_like_type(&mut self, t: P<Type>) -> bool {
        self.is_array_like_type(t) || self.is_tuple_like_type(t)
    }

    // checker.go:24021
    pub(crate) fn is_array_or_tuple_or_intersection(&mut self, t: P<Type>) -> bool {
        t.flags().intersects(TypeFlags::Intersection)
            && t.types().iter().all(|&t| self.is_array_or_tuple_type(t))
    }

    // checker.go:24025
    pub(crate) fn get_tuple_element_type(&mut self, t: P<Type>, index: usize) -> Option<P<Type>> {
        let prop_type = self.get_type_of_property_of_type(t, &index.to_string());
        if prop_type.is_some() {
            return prop_type;
        }
        if every_type(self, t, |_, t| is_tuple_type(t)) {
            let undefined_like_type =
                if self.compiler_options.no_unchecked_indexed_access == Tristate::True {
                    Some(self.undefined_type)
                } else {
                    None
                };
            return Some(self.get_tuple_element_type_out_of_start_count(
                t,
                Number(index as f64),
                undefined_like_type,
            ));
        }
        None
    }

    /**
     * Get type from reference to type alias. When a type alias is generic, the declared type of the type alias may include
     * references to the type parameters of the alias. We replace those with the actual type arguments by instantiating the
     * declared type. Instantiations are cached using the type identities of the type arguments as the key.
     */
    // checker.go:24041
    pub(crate) fn get_type_from_type_alias_reference(
        &mut self,
        node: P<Node>,
        symbol: P<Symbol>,
    ) -> P<Type> {
        let type_arguments = node.type_arguments();
        if symbol.check_flags().intersects(CheckFlags::Unresolved) {
            let alias_type_arguments: Vec<P<Type>> =
                type_arguments.iter().map(|&n| self.get_type_from_type_node(n)).collect();
            let alias = P::new(TypeAlias {
                symbol: Cell::new(Some(symbol)),
                type_arguments: ThinSliceCell::new(alloc_vec(alias_type_arguments)),
            });
            let key = get_alias_key(Some(alias).into());
            let mut error_type = self.error_types.get(&key);
            if error_type.is_none() {
                let t = self.new_intrinsic_type(TypeFlags::Any, "error");
                t.set_alias(Some(alias));
                self.error_types.insert(key, t);
                error_type = Some(t);
            }
            return error_type.unwrap();
        }
        let t = self.get_declared_type_of_symbol(symbol);
        let type_parameters = self.type_alias_links.get(symbol).type_parameters.get();
        if !type_parameters.is_empty() {
            let num_type_arguments = type_arguments.len();
            let min_type_argument_count = Self::get_min_type_argument_count(type_parameters);
            if num_type_arguments < min_type_argument_count
                || num_type_arguments > type_parameters.len()
            {
                let message = if min_type_argument_count == type_parameters.len() {
                    &diagnostics::GENERIC_TYPE_0_REQUIRES_1_TYPE_ARGUMENT_S
                } else {
                    &diagnostics::GENERIC_TYPE_0_REQUIRES_BETWEEN_1_AND_2_TYPE_ARGUMENTS
                };
                let name = self.symbol_to_string(symbol);
                self.error(
                    Some(node),
                    message,
                    &[&name, &min_type_argument_count, &type_parameters.len()],
                );
                return self.error_type;
            }
            // We refrain from associating a local type alias with an instantiation of a top-level type alias
            // because the local alias may end up being referenced in an inferred return type where it is not
            // accessible--which in turn may lead to a large structural expansion of the type when generating
            // a .d.ts file. See #43622 for an example.
            let alias_symbol = self.get_alias_symbol_for_type_node(node);
            let mut new_alias_symbol = None;
            if let Some(alias_symbol) = alias_symbol
                && (is_local_type_alias(symbol) || !is_local_type_alias(alias_symbol))
            {
                new_alias_symbol = Some(alias_symbol);
            }
            let mut alias_type_arguments: Vec<P<Type>> = Vec::new();
            if new_alias_symbol.is_some() {
                alias_type_arguments = self.get_type_arguments_for_alias_symbol(new_alias_symbol);
            } else if ast::is_type_reference_type(node) {
                let alias_symbol = self.resolve_type_reference_name(
                    node,
                    SymbolFlags::Alias,
                    true, /*ignoreErrors*/
                );
                // refers to an alias import/export/reexport - by making sure we use the target as an aliasSymbol,
                // we ensure the exported symbol is used to refer to the type when it is reserialized later
                if alias_symbol != self.unknown_symbol {
                    let resolved = self.resolve_alias(alias_symbol);
                    if resolved.flags().intersects(SymbolFlags::TypeAlias) {
                        new_alias_symbol = Some(resolved);
                        alias_type_arguments = self.get_type_arguments_from_node(node);
                    }
                }
            }
            let mut new_alias = None;
            if new_alias_symbol.is_some() {
                new_alias = Some(P::new(TypeAlias {
                    symbol: Cell::new(new_alias_symbol),
                    type_arguments: ThinSliceCell::new(alloc_vec(alias_type_arguments)),
                }));
            }
            let type_arguments_from_node = self.get_type_arguments_from_node(node);
            return self.get_type_alias_instantiation(symbol, &type_arguments_from_node, new_alias);
        }
        if self.check_no_type_arguments(node, Some(symbol)) {
            return t;
        }
        self.error_type
    }

    // checker.go:24102
    pub(crate) fn get_type_alias_instantiation(
        &mut self,
        symbol: P<Symbol>,
        type_arguments: &[P<Type>],
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        let t = self.get_declared_type_of_symbol(symbol);
        if t == self.intrinsic_marker_type
            && let Some(&type_kind) = INTRINSIC_TYPE_KINDS.get(symbol.name())
            && type_arguments.len() == 1
        {
            match type_kind {
                IntrinsicTypeKind::NoInfer => {
                    return self.get_no_infer_type(type_arguments[0]);
                }
                _ => return self.get_string_mapping_type(symbol, type_arguments[0]),
            }
        }
        let links = self.type_alias_links.get(symbol);
        let type_parameters = links.type_parameters.get();
        let key = get_type_alias_instantiation_key(type_arguments, alias);
        let mut instantiation = links.instantiations.get(&key);
        if instantiation.is_none() {
            let too_complex_before = self.too_complex_reports;
            let min_type_argument_count = Self::get_min_type_argument_count(type_parameters);
            let filled = self.fill_missing_type_arguments(
                type_arguments,
                type_parameters,
                min_type_argument_count,
                ast::is_in_js_file(symbol.value_declaration()),
            );
            let filled = match filled {
                std::borrow::Cow::Borrowed(args) => alloc_slice(args),
                std::borrow::Cow::Owned(args) => alloc_vec(args),
            };
            let mapper = new_type_mapper(type_parameters, filled);
            let result = self.instantiate_type_with_alias(t, Some(mapper), alias);
            // tsrs-only: not cached when the instantiation reported TS2590 (`too_complex_since`).
            if !self.too_complex_since(too_complex_before) {
                links.instantiations.set(key, result);
            }
            instantiation = Some(result);
        }
        instantiation.unwrap()
    }
}

// checker.go:24126
fn is_local_type_alias(symbol: P<Symbol>) -> bool {
    let declaration = symbol.declarations().iter().copied().find(|&d| is_type_alias(d));
    declaration.is_some_and(|d| ast::get_containing_function(d).is_some())
}

impl Checker {
    // checker.go:24131
    pub(crate) fn get_declared_type_of_symbol(&mut self, symbol: P<Symbol>) -> P<Type> {
        let mut result = self.try_get_declared_type_of_symbol(symbol);
        if result.is_none() {
            result = Some(self.error_type);
        }
        result.unwrap()
    }

    // checker.go:24139
    pub(crate) fn try_get_declared_type_of_symbol(&mut self, symbol: P<Symbol>) -> Option<P<Type>> {
        let flags = symbol.flags();
        if flags.intersects(SymbolFlags::Class | SymbolFlags::Interface) {
            return Some(self.get_declared_type_of_class_or_interface(symbol));
        } else if flags.intersects(SymbolFlags::TypeParameter) {
            return Some(self.get_declared_type_of_type_parameter(symbol));
        } else if flags.intersects(SymbolFlags::TypeAlias) {
            return Some(self.get_declared_type_of_type_alias(symbol));
        } else if flags.intersects(SymbolFlags::Enum) {
            return Some(self.get_declared_type_of_enum(symbol));
        } else if flags.intersects(SymbolFlags::EnumMember) {
            return Some(self.get_declared_type_of_enum_member(symbol));
        } else if flags.intersects(SymbolFlags::Alias) {
            return Some(self.get_declared_type_of_alias(symbol));
        }
        None
    }
}

// checker.go:24157
pub(crate) fn get_type_reference_name(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::TypeReference => {
            return Some(node.as_type_reference_node().type_name);
        }
        Kind::ExpressionWithTypeArguments => {
            // We only support expressions that are simple qualified names. For other
            // expressions this produces nil
            let expr = node.expression().unwrap();
            if ast::is_entity_name_expression(expr) {
                return Some(expr);
            }
        }
        _ => {}
    }
    None
}

impl Checker {
    // checker.go:24172
    pub(crate) fn get_alias_for_type_node(&mut self, node: P<Node>) -> Option<P<TypeAlias>> {
        let symbol = self.get_alias_symbol_for_type_node(node);
        if symbol.is_some() {
            let type_arguments = self.get_type_arguments_for_alias_symbol(symbol);
            return Some(P::new(TypeAlias {
                symbol: Cell::new(symbol),
                type_arguments: ThinSliceCell::new(alloc_vec(type_arguments)),
            }));
        }
        None
    }

    // checker.go:24180
    pub(crate) fn get_alias_symbol_for_type_node(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        let mut host = node.parent().unwrap();
        while ast::is_parenthesized_type_node(host)
            || ast::is_type_operator_node(host)
                && host.as_type_operator_node().operator == Kind::ReadonlyKeyword
        {
            host = host.parent().unwrap();
        }
        if is_type_alias(host) {
            return self.get_symbol_of_declaration(host);
        }
        None
    }

    // checker.go:24191
    fn get_type_arguments_for_alias_symbol(&mut self, symbol: Option<P<Symbol>>) -> Vec<P<Type>> {
        if let Some(symbol) = symbol {
            return self.get_local_type_parameters_of_class_or_interface_or_type_alias(symbol);
        }
        Vec::new()
    }

    // checker.go:24198
    pub(crate) fn get_outer_type_parameters_of_class_or_interface(
        &mut self,
        symbol: P<Symbol>,
    ) -> Vec<P<Type>> {
        let declaration = Self::get_class_or_interface_like_declaration(symbol);
        assert!(
            declaration.is_some(),
            "Class was missing valueDeclaration -OR- non-class had no interface declarations"
        );
        self.get_outer_type_parameters(declaration.unwrap(), false /*includeThisTypes*/)
    }

    // Returns the declaration used to obtain a class, interface, or function symbol's outer type parameters.
    // checker.go:24205
    fn get_class_or_interface_like_declaration(symbol: P<Symbol>) -> Option<P<Node>> {
        if symbol.flags().intersects(SymbolFlags::Class | SymbolFlags::Function) {
            return symbol.value_declaration();
        }
        symbol.declarations().iter().copied().find(|&d| {
            if ast::is_interface_declaration(d) {
                return true;
            }
            if !ast::is_variable_declaration(d) {
                return false;
            }
            let initializer = d.initializer();
            initializer.is_some_and(ast::is_function_expression_or_arrow_function)
        })
    }

    // checker.go:24221
    pub(crate) fn can_get_type_parameters_of_class_or_interface(symbol: P<Symbol>) -> bool {
        Self::get_class_or_interface_like_declaration(symbol).is_some()
    }

    // Return the outer type parameters of a node or undefined if the node has no outer type parameters.
    // checker.go:24226
    pub(crate) fn get_outer_type_parameters(
        &mut self,
        node: P<Node>,
        include_this_types: bool,
    ) -> Vec<P<Type>> {
        let mut node = node;
        loop {
            node = match node.parent() {
                Some(parent) => parent,
                None => return Vec::new(),
            };
            let kind = node.kind();
            match kind {
                Kind::ClassDeclaration
                | Kind::ClassExpression
                | Kind::InterfaceDeclaration
                | Kind::CallSignature
                | Kind::ConstructSignature
                | Kind::MethodSignature
                | Kind::FunctionType
                | Kind::ConstructorType
                | Kind::FunctionDeclaration
                | Kind::MethodDeclaration
                | Kind::FunctionExpression
                | Kind::ArrowFunction
                | Kind::TypeAliasDeclaration
                | Kind::JSTypeAliasDeclaration
                | Kind::MappedType
                | Kind::ConditionalType => {
                    let mut outer_type_parameters =
                        self.get_outer_type_parameters(node, include_this_types);
                    if (kind == Kind::FunctionExpression
                        || kind == Kind::ArrowFunction
                        || ast::is_object_literal_method(node))
                        && self.is_context_sensitive(node)
                    {
                        let symbol = self.get_symbol_of_declaration(node).unwrap();
                        let t = self.get_type_of_symbol(symbol);
                        let signature =
                            self.get_signatures_of_type(t, SignatureKind::Call).first().copied();
                        if let Some(signature) = signature
                            && !signature.type_parameters().is_empty()
                        {
                            outer_type_parameters.extend_from_slice(signature.type_parameters());
                            return outer_type_parameters;
                        }
                    }
                    if kind == Kind::MappedType {
                        let symbol = self
                            .get_symbol_of_declaration(node.as_mapped_type_node().type_parameter)
                            .unwrap();
                        let tp = self.get_declared_type_of_type_parameter(symbol);
                        outer_type_parameters.push(tp);
                        return outer_type_parameters;
                    }
                    if kind == Kind::ConditionalType {
                        let infer_type_parameters = self.get_infer_type_parameters(node);
                        outer_type_parameters.extend(infer_type_parameters);
                        return outer_type_parameters;
                    }
                    let mut outer_and_own_type_parameters =
                        self.append_type_parameters(&outer_type_parameters, node.type_parameters());
                    let mut this_type = None;
                    if include_this_types
                        && (kind == Kind::ClassDeclaration
                            || kind == Kind::ClassExpression
                            || kind == Kind::InterfaceDeclaration)
                    {
                        let symbol = self.get_symbol_of_declaration(node).unwrap();
                        this_type = self
                            .get_declared_type_of_class_or_interface(symbol)
                            .as_interface_type()
                            .this_type
                            .get();
                    }
                    if let Some(this_type) = this_type {
                        outer_and_own_type_parameters.push(this_type);
                        return outer_and_own_type_parameters;
                    }
                    return outer_and_own_type_parameters;
                }
                _ => {}
            }
        }
    }

    // checker.go:24264
    pub(crate) fn get_infer_type_parameters(&mut self, node: P<Node>) -> Vec<P<Type>> {
        let mut result = Vec::new();
        let symbols = node.locals().map_or_else(Vec::new, |locals| locals.values());
        result.extend(symbols.into_iter().filter_map(|symbol| {
            if symbol.flags().intersects(SymbolFlags::TypeParameter) {
                Some(self.get_declared_type_of_symbol(symbol))
            } else {
                None
            }
        }));
        tsrs_core::goslices::sort_func(&mut result, |&a, &b| compare_types(self, Some(a), Some(b)));
        result
    }

    // The local type parameters are the combined set of type parameters from all declarations of the class,
    // interface, or type alias.
    // checker.go:24276
    pub(crate) fn get_local_type_parameters_of_class_or_interface_or_type_alias(
        &mut self,
        symbol: P<Symbol>,
    ) -> Vec<P<Type>> {
        self.append_local_type_parameters_of_class_or_interface_or_type_alias(&[], symbol)
    }

    // checker.go:24280
    pub(crate) fn append_local_type_parameters_of_class_or_interface_or_type_alias(
        &mut self,
        types: &[P<Type>],
        symbol: P<Symbol>,
    ) -> Vec<P<Type>> {
        let mut types = types.to_vec();
        let declarations = symbol.declarations();
        types = declarations.iter().fold(types, |mut types, &node| {
            if ast::node_kind_is(
                node,
                &[Kind::InterfaceDeclaration, Kind::ClassDeclaration, Kind::ClassExpression],
            ) || is_type_alias(node)
            {
                types = self.append_type_parameters(&types, node.type_parameters());
            }

            types
        });
        types
    }

    // Appends the type parameters given by a list of declarations to a set of type parameters and returns the resulting set.
    // The function allocates a new array if the input type parameter set is undefined, but otherwise it modifies the set
    // in-place and returns the same array.
    // checker.go:24292
    fn append_type_parameters(
        &mut self,
        type_parameters: &[P<Type>],
        declarations: &[P<Node>],
    ) -> Vec<P<Type>> {
        let mut type_parameters = type_parameters.to_vec();
        for &declaration in declarations {
            let symbol = self.get_symbol_of_declaration(declaration).unwrap();
            let tp = self.get_declared_type_of_type_parameter(symbol);
            append_if_unique(&mut type_parameters, tp);
        }
        type_parameters
    }

    // checker.go:24299
    pub(crate) fn get_declared_type_of_type_parameter(
        &mut self,
        symbol: impl Into<Option<P<Symbol>>>,
    ) -> P<Type> {
        let symbol = symbol.into();
        let links = self.declared_type_links.get(symbol);
        if links.declared_type.get().is_none() {
            links.declared_type.set(Some(self.new_type_parameter(symbol)));
        }
        links.declared_type.get().unwrap()
    }

    // checker.go:24307
    pub(crate) fn get_declared_type_of_type_alias(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.type_alias_links.get(symbol);
        if links.declared_type.get().is_none() {
            // Note that we use the links object as the target here because the symbol object is used as the unique
            // identity for resolution of the 'type' property in SymbolLinks.
            if !self.push_type_resolution(
                TypeSystemEntity::Symbol(symbol),
                TypeSystemPropertyName::DeclaredType,
            ) {
                return self.error_type;
            }
            let declaration = symbol
                .declarations()
                .iter()
                .copied()
                .find(|&d| ast::is_type_or_js_type_alias_declaration(d))
                .unwrap();
            let type_node = declaration.type_node().unwrap();
            let mut t = self.get_type_from_type_node(type_node);
            if self.pop_type_resolution() {
                let type_parameters =
                    self.get_local_type_parameters_of_class_or_interface_or_type_alias(symbol);
                if !type_parameters.is_empty() {
                    // Initialize the instantiation cache for generic type aliases. The declared type corresponds to
                    // an instantiation of the type alias with the type parameters supplied as type arguments.
                    let type_parameters = alloc_vec(type_parameters);
                    links.type_parameters.set(type_parameters);
                    links.instantiations.make();
                    links.instantiations.set(get_type_list_key(type_parameters), t);
                }
                if t == self.intrinsic_marker_type && symbol.name() == "BuiltinIteratorReturn" {
                    t = self.get_builtin_iterator_return_type();
                }
            } else {
                let error_node = declaration.name().unwrap_or(declaration);
                let name = self.symbol_to_string(symbol);
                self.error(
                    Some(error_node),
                    &diagnostics::TYPE_ALIAS_0_CIRCULARLY_REFERENCES_ITSELF,
                    &[&name],
                );
                t = self.error_type;
            }
            if links.declared_type.get().is_none() {
                links.declared_type.set(Some(t));
            }
        }
        links.declared_type.get().unwrap()
    }

    // checker.go:24345
    fn get_declared_type_of_enum(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.declared_type_links.get(symbol);
        if links.declared_type.get().is_none() {
            let mut member_type_list: Vec<P<Type>> = Vec::new();
            let declarations = symbol.declarations();
            for declaration in declarations {
                if declaration.kind() == Kind::EnumDeclaration {
                    member_type_list.extend(declaration.members().iter().filter_map(|&member| {
                        if ast::has_dynamic_name(member) {
                            None
                        } else {
                            let member_symbol = self.get_symbol_of_declaration(member).unwrap();
                            let value = self.get_enum_member_value(member).value;
                            let member_type = if let Some(value) = value {
                                self.get_enum_literal_type(value, symbol, member_symbol)
                            } else {
                                self.create_computed_enum_type(member_symbol)
                            };
                            let member_links = self.declared_type_links.get(member_symbol);
                            member_links
                                .declared_type
                                .set(Some(self.get_fresh_type_of_literal_type(member_type)));
                            Some(member_type)
                        }
                    }));
                }
            }
            let enum_type = if member_type_list.is_empty() {
                self.create_computed_enum_type(symbol)
            } else {
                let alias =
                    P::new(TypeAlias { symbol: Cell::new(Some(symbol)), ..Default::default() });
                self.get_union_type_ex(
                    &member_type_list,
                    UnionReduction::Literal,
                    Some(alias).into(),
                    None, /*origin*/
                )
            };
            if enum_type.flags().intersects(TypeFlags::Union) {
                enum_type.flags.set(enum_type.flags() | TypeFlags::EnumLiteral);
                enum_type.set_symbol(Some(symbol));
            }
            links.declared_type.set(Some(enum_type));
        }
        links.declared_type.get().unwrap()
    }

    // checker.go:24382
    pub(crate) fn get_enum_member_value(&mut self, node: P<Node>) -> evaluator::Result {
        self.compute_enum_member_values(node.parent().unwrap());
        self.enum_member_links.get(node).value.get()
    }

    // checker.go:24387
    fn create_computed_enum_type(&mut self, symbol: P<Symbol>) -> P<Type> {
        let regular_type = self.new_literal_type(TypeFlags::Enum, None, None);
        regular_type.set_symbol(Some(symbol));
        let fresh_type = self.new_literal_type(TypeFlags::Enum, None, Some(regular_type));
        fresh_type.set_symbol(Some(symbol));
        regular_type.as_literal_type().fresh_type.set(Some(fresh_type));
        fresh_type.as_literal_type().fresh_type.set(Some(fresh_type));
        regular_type
    }

    // checker.go:24397
    pub(crate) fn get_declared_type_of_enum_member(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.declared_type_links.get(symbol);
        if links.declared_type.get().is_none() {
            let parent = self.get_parent_of_symbol(symbol).unwrap();
            let enum_type = self.get_declared_type_of_enum(parent);
            if links.declared_type.get().is_none() {
                links.declared_type.set(Some(enum_type));
            }
        }
        links.declared_type.get().unwrap()
    }

    // checker.go:24408
    pub(crate) fn compute_enum_member_values(&mut self, node: P<Node>) {
        let node_links = self.node_links.get(node);
        if !node_links.flags.get().intersects(NodeCheckFlags::EnumValuesComputed) {
            node_links.flags.set(node_links.flags.get() | NodeCheckFlags::EnumValuesComputed);
            let mut auto_value: Option<Number> = Some(Number(0.0));
            let mut previous: Option<P<Node>> = None;
            for &member in node.members() {
                let result = self.compute_enum_member_value(member, auto_value.as_mut(), previous);
                self.enum_member_links.get(member).value.set(result);
                if let Some(LiteralValue::Number(value)) = result.value {
                    let next_value = value + Number(1.0);
                    auto_value = Some(next_value);
                } else {
                    auto_value = None;
                }
                previous = Some(member);
            }
        }
    }

    // checker.go:24428
    fn compute_enum_member_value(
        &mut self,
        member: P<Node>,
        auto_value: Option<&mut Number>,
        previous: Option<P<Node>>,
    ) -> evaluator::Result {
        let name = member.name().unwrap();
        if ast::is_computed_non_literal_name(name) {
            self.error(
                Some(name),
                &diagnostics::COMPUTED_PROPERTY_NAMES_ARE_NOT_ALLOWED_IN_ENUMS,
                &[],
            );
        } else if ast::is_big_int_literal(name) {
            self.error(Some(name), &diagnostics::AN_ENUM_MEMBER_CANNOT_HAVE_A_NUMERIC_NAME, &[]);
        } else {
            let text = ast::get_text_of_property_name(name);
            if is_numeric_literal_name(&text) && !ast::is_infinity_or_nan_string(&text) {
                self.error(
                    Some(name),
                    &diagnostics::AN_ENUM_MEMBER_CANNOT_HAVE_A_NUMERIC_NAME,
                    &[],
                );
            }
        }
        if member.initializer().is_some() {
            return self.compute_constant_enum_member_value(member);
        }
        // In ambient non-const numeric enum declarations, enum members without initializers are
        // considered computed members (as opposed to having auto-incremented values).
        let parent = member.parent().unwrap();
        if parent.flags().intersects(NodeFlags::Ambient) && !ast::is_enum_const(parent) {
            return evaluator::new_result(None, false, false, false);
        }
        // If the member declaration specifies no value, the member is considered a constant enum member.
        // If the member is the first member in the enum declaration, it is assigned the value zero.
        // Otherwise, it is assigned the value of the immediately preceding member plus one, and an error
        // occurs if the immediately preceding member is not a constant enum member.
        let Some(auto_value) = auto_value else {
            self.error(Some(name), &diagnostics::ENUM_MEMBER_MUST_HAVE_INITIALIZER, &[]);
            return evaluator::new_result(None, false, false, false);
        };
        if self.compiler_options.get_isolated_modules()
            && let Some(previous) = previous
            && previous.initializer().is_some()
        {
            let prev_value = self.get_enum_member_value(previous);
            let prev_is_num = matches!(prev_value.value, Some(LiteralValue::Number(_)));
            if !prev_is_num || prev_value.resolved_other_files {
                self.error(Some(name), &diagnostics::ENUM_MEMBER_FOLLOWING_A_NON_LITERAL_NUMERIC_MEMBER_MUST_HAVE_AN_INITIALIZER_WHEN_ISOLATED_MODULES_IS_ENABLED, &[]);
            }
        }
        evaluator::new_result(Some(LiteralValue::Number(*auto_value)), false, false, false)
    }

    // checker.go:24465
    fn compute_constant_enum_member_value(&mut self, member: P<Node>) -> evaluator::Result {
        let parent = member.parent().unwrap();
        let is_const_enum = ast::is_enum_const(parent);
        let initializer = member.initializer().unwrap();
        let result = self.evaluate(initializer, member);
        if result.value.is_some() {
            if is_const_enum
                && let Some(LiteralValue::Number(num_value)) = result.value
                && (num_value.is_inf() || num_value.is_nan())
            {
                let message = if num_value.is_nan() {
                    &diagnostics::X_CONST_ENUM_MEMBER_INITIALIZER_WAS_EVALUATED_TO_DISALLOWED_VALUE_NA_N
                } else {
                    &diagnostics::X_CONST_ENUM_MEMBER_INITIALIZER_WAS_EVALUATED_TO_A_NON_FINITE_VALUE
                };
                self.error(Some(initializer), message, &[]);
            }
            if self.compiler_options.get_isolated_modules()
                && let Some(LiteralValue::String(_)) = result.value
                && !result.is_syntactically_string
            {
                let member_name =
                    format!("{}.{}", parent.name().unwrap().text(), member.name().unwrap().text());
                self.error(
                            Some(initializer),
                            &diagnostics::X_0_HAS_A_STRING_TYPE_BUT_MUST_HAVE_SYNTACTICALLY_RECOGNIZABLE_STRING_SYNTAX_WHEN_ISOLATED_MODULES_IS_ENABLED,
                            &[&member_name],
                        );
            }
        } else if is_const_enum {
            self.error(
                Some(initializer),
                &diagnostics::X_CONST_ENUM_MEMBER_INITIALIZERS_MUST_BE_CONSTANT_EXPRESSIONS,
                &[],
            );
        } else if parent.flags().intersects(NodeFlags::Ambient) {
            self.error(Some(initializer), &diagnostics::IN_AMBIENT_ENUM_DECLARATIONS_MEMBER_INITIALIZER_MUST_BE_CONSTANT_EXPRESSION, &[]);
        } else {
            let t = self.check_expression(initializer);
            self.check_type_assignable_to(
                t,
                self.number_type,
                Some(initializer),
                Some(&diagnostics::TYPE_0_IS_NOT_ASSIGNABLE_TO_TYPE_1_AS_REQUIRED_FOR_COMPUTED_ENUM_MEMBER_VALUES),
            );
        }
        result
    }

    // checker.go:24494
    pub(crate) fn evaluate_entity(
        &mut self,
        expr: P<Node>,
        location: Option<P<Node>>,
    ) -> evaluator::Result {
        match expr.kind() {
            Kind::Identifier | Kind::PropertyAccessExpression => {
                let symbol = self.resolve_entity_name(
                    expr,
                    SymbolFlags::Value,
                    true, /*ignoreErrors*/
                    false,
                    None,
                );
                let Some(symbol) = symbol else {
                    return evaluator::new_result(None, false, false, false);
                };
                if expr.kind() == Kind::Identifier
                    && ast::is_infinity_or_nan_string(expr.text())
                    && (Some(symbol)
                        == self.get_global_symbol(
                            expr.text(),
                            SymbolFlags::Value,
                            None, /*diagnostic*/
                        ))
                {
                    // Technically we resolved a global lib file here, but the decision to treat this as numeric
                    // is more predicated on the fact that the single-file resolution *didn't* resolve to a
                    // different meaning of `Infinity` or `NaN`. Transpilers handle this no problem.
                    return evaluator::new_result(
                        Some(LiteralValue::Number(jsnum::from_string(expr.text()))),
                        false,
                        false,
                        false,
                    );
                }
                if symbol.flags().intersects(SymbolFlags::EnumMember) {
                    if let Some(location) = location {
                        return self.evaluate_enum_member(expr, symbol, location);
                    }
                    return self.get_enum_member_value(symbol.value_declaration().unwrap());
                }
                if self.is_constant_variable(symbol)
                    && let Some(declaration) = symbol.value_declaration()
                    && ast::is_variable_declaration(declaration)
                    && declaration.type_node().is_none()
                    && declaration.initializer().is_some()
                    && (location.is_none()
                        || Some(declaration) != location
                            && self.is_block_scoped_name_declared_before_use(
                                declaration,
                                location.unwrap(),
                            ))
                {
                    let result = self.evaluate(declaration.initializer().unwrap(), declaration);
                    if location.is_some()
                        && ast::get_source_file_of_node(location)
                            != ast::get_source_file_of_node(declaration)
                    {
                        return evaluator::new_result(result.value, false, true, true);
                    }
                    return evaluator::new_result(
                        result.value,
                        result.is_syntactically_string,
                        result.resolved_other_files,
                        true, /*hasExternalReferences*/
                    );
                }
                evaluator::new_result(None, false, false, false)
            }
            Kind::ElementAccessExpression => {
                let root = expr.expression().unwrap();
                let argument_expression = expr.as_element_access_expression().argument_expression;
                if ast::is_entity_name_expression(root)
                    && ast::is_string_literal_like(argument_expression)
                {
                    let root_symbol = self.resolve_entity_name(
                        root,
                        SymbolFlags::Value,
                        true, /*ignoreErrors*/
                        false,
                        None,
                    );
                    if let Some(root_symbol) = root_symbol
                        && root_symbol.flags().intersects(SymbolFlags::Enum)
                    {
                        let name = argument_expression.text();
                        let member = root_symbol.exports().and_then(|exports| exports.lookup(name));
                        if let Some(member) = member {
                            if let Some(location) = location {
                                return self.evaluate_enum_member(expr, member, location);
                            }
                            return self.get_enum_member_value(member.value_declaration().unwrap());
                        }
                    }
                }
                evaluator::new_result(None, false, false, false)
            }
            _ => panic!("Unhandled case in evaluateEntity"),
        }
    }

    // checker.go:24547
    fn evaluate_enum_member(
        &mut self,
        expr: P<Node>,
        symbol: P<Symbol>,
        location: P<Node>,
    ) -> evaluator::Result {
        let declaration = symbol.value_declaration();
        if declaration.is_none() || declaration == Some(location) {
            let name = self.symbol_to_string(symbol);
            self.error(
                Some(expr),
                &diagnostics::PROPERTY_0_IS_USED_BEFORE_BEING_ASSIGNED,
                &[&name],
            );
            return evaluator::new_result(None, false, false, false);
        }
        let declaration = declaration.unwrap();
        if !self.is_block_scoped_name_declared_before_use(declaration, location) {
            self.error(
                Some(expr),
                &diagnostics::A_MEMBER_INITIALIZER_IN_A_ENUM_DECLARATION_CANNOT_REFERENCE_MEMBERS_DECLARED_AFTER_IT_INCLUDING_MEMBERS_DEFINED_IN_OTHER_ENUMS,
                &[],
            );
            return evaluator::new_result(
                Some(LiteralValue::Number(Number(0.0))),
                false,
                false,
                false,
            );
        }
        let value = self.get_enum_member_value(declaration);
        if location.parent() != declaration.parent() {
            return evaluator::new_result(
                value.value,
                value.is_syntactically_string,
                value.resolved_other_files,
                true, /*hasExternalReferences*/
            );
        }
        value
    }

    // checker.go:24564
    fn get_declared_type_of_alias(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.declared_type_links.get(symbol);
        if links.declared_type.get().is_none() {
            let resolved = self.resolve_alias(symbol);
            links.declared_type.set(Some(self.get_declared_type_of_symbol(resolved)));
        }
        links.declared_type.get().unwrap()
    }

    // checker.go:24572
    pub(crate) fn get_type_from_type_query_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            // TypeScript 1.0 spec (April 2014): 3.6.3
            // The expression is processed as an identifier expression (section 4.3)
            // or property access expression(section 4.10),
            // the widened type(section 3.9) of which becomes the result.
            let t = self.check_expression_with_type_arguments(node);
            let widened = self.get_widened_type(t);
            links.resolved_type.set(Some(self.get_regular_type_of_literal_type(widened)));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:24585
    pub(crate) fn get_type_from_array_or_tuple_type_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let target = self.get_array_or_tuple_target_type(node);
            if target == self.empty_generic_type {
                links.resolved_type.set(Some(self.empty_object_type));
            } else if !(node.kind() == Kind::TupleType
                && node.elements().iter().any(|&e| Self::is_variadic_tuple_element(e)))
                && self.is_deferred_type_reference_node(node, false)
            {
                if node.kind() == Kind::TupleType && node.elements().is_empty() {
                    links.resolved_type.set(Some(target));
                } else {
                    links.resolved_type.set(Some(self.create_deferred_type_reference(
                        target, node, None, /*mapper*/
                        None, /*alias*/
                    )));
                }
            } else {
                let element_types: Vec<P<Type>> = if node.kind() == Kind::ArrayType {
                    vec![self.get_type_from_type_node(node.as_array_type_node().element_type)]
                } else {
                    node.elements().iter().map(|&e| self.get_type_from_type_node(e)).collect()
                };
                if target.object_flags().intersects(ObjectFlags::Tuple) {
                    links.resolved_type.set(Some(self.create_normalized_tuple_type_ex(
                        target,
                        &element_types,
                        ObjectFlags::FromTypeNode,
                    )));
                } else {
                    links.resolved_type.set(Some(self.create_type_reference_ex(
                        target,
                        &element_types,
                        ObjectFlags::FromTypeNode,
                    )));
                }
            }
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:24614
    fn is_variadic_tuple_element(node: P<Node>) -> bool {
        Self::get_tuple_element_flags(node).intersects(ElementFlags::Variadic)
    }

    // checker.go:24618
    fn get_array_or_tuple_target_type(&mut self, node: P<Node>) -> P<Type> {
        let readonly = Self::is_readonly_type_operator(node.parent().unwrap());
        let element_type = Self::get_array_element_type_node(node);
        if element_type.is_some() {
            if readonly {
                return self.global_readonly_array_type;
            }
            return self.global_array_type;
        }
        let element_infos: Vec<TupleElementInfo> =
            node.elements().iter().map(|&e| Self::get_tuple_element_info(e)).collect();
        self.get_tuple_target_type(&element_infos, readonly)
    }

    // checker.go:24630
    fn is_readonly_type_operator(node: P<Node>) -> bool {
        ast::is_type_operator_node(node)
            && node.as_type_operator_node().operator == Kind::ReadonlyKeyword
    }

    // checker.go:24634
    pub(crate) fn get_type_from_named_tuple_type_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            if node.as_named_tuple_member().dot_dot_dot_token.is_some() {
                links.resolved_type.set(Some(self.get_type_from_rest_type_node(node)));
            } else {
                let t = self.get_type_from_type_node(node.type_node().unwrap());
                links.resolved_type.set(Some(self.add_optionality_ex(
                    t,
                    true, /*isProperty*/
                    node.question_token().is_some(),
                )));
            }
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:24646
    pub(crate) fn get_type_from_rest_type_node(&mut self, node: P<Node>) -> P<Type> {
        let mut type_node = node.type_node().unwrap();
        let element_type_node = Self::get_array_element_type_node(type_node);
        if let Some(element_type_node) = element_type_node {
            type_node = element_type_node;
        }
        self.get_type_from_type_node(type_node)
    }

    // checker.go:24655
    fn get_array_element_type_node(node: P<Node>) -> Option<P<Node>> {
        match node.kind() {
            Kind::ParenthesizedType => {
                return Self::get_array_element_type_node(node.type_node().unwrap());
            }
            Kind::TupleType => {
                if node.elements().len() == 1 {
                    let node = node.elements()[0];
                    if node.kind() == Kind::RestType {
                        return Self::get_array_element_type_node(node.type_node().unwrap());
                    }
                    if node.kind() == Kind::NamedTupleMember
                        && node.as_named_tuple_member().dot_dot_dot_token.is_some()
                    {
                        return Self::get_array_element_type_node(node.type_node().unwrap());
                    }
                }
            }
            Kind::ArrayType => {
                return Some(node.as_array_type_node().element_type);
            }
            _ => {}
        }
        None
    }

    // checker.go:24675
    pub(crate) fn get_type_from_optional_type_node(&mut self, node: P<Node>) -> P<Type> {
        let t = self.get_type_from_type_node(node.type_node().unwrap());
        self.add_optionality_ex(t, true /*isProperty*/, true /*isOptional*/)
    }

    // checker.go:24679
    pub(crate) fn get_type_from_union_type_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let alias = self.get_alias_for_type_node(node);
            let types: Vec<P<Type>> = node
                .as_union_type_node()
                .union_or_intersection_type_node_base
                .types
                .nodes()
                .iter()
                .map(|&n| self.get_type_from_type_node(n))
                .collect();
            links.resolved_type.set(Some(self.get_union_type_ex(
                &types,
                UnionReduction::Literal,
                alias.into(),
                None, /*origin*/
            )));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:24688
    pub(crate) fn get_type_from_intersection_type_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let alias = self.get_alias_for_type_node(node);
            let types: Vec<P<Type>> = node
                .as_intersection_type_node()
                .union_or_intersection_type_node_base
                .types
                .nodes()
                .iter()
                .map(|&n| self.get_type_from_type_node(n))
                .collect();
            // We perform no supertype reduction for X & {} or {} & X, where X is one of string, number, bigint,
            // or a pattern literal template type. This enables union types like "a" | "b" | string & {} or
            // "aa" | "ab" | `a${string}` which preserve the literal types for purposes of statement completion.
            let mut no_supertype_reduction = false;
            if types.len() == 2
                && let Some(empty_index) =
                    types.iter().position(|&t| t == self.empty_type_literal_type)
            {
                let t = types[1 - empty_index];
                no_supertype_reduction =
                    t.flags().intersects(TypeFlags::String | TypeFlags::Number | TypeFlags::BigInt)
                        || t.flags().intersects(TypeFlags::TemplateLiteral)
                            && self.is_pattern_literal_type(t);
            }
            let flags = if no_supertype_reduction {
                IntersectionFlags::NoSupertypeReduction
            } else {
                IntersectionFlags::None
            };
            links.resolved_type.set(Some(self.get_intersection_type_ex(
                &types,
                flags,
                alias.into(),
            )));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:24709
    pub(crate) fn get_type_from_template_type_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let spans = node.as_template_literal_type_node().template_spans;
            let mut texts: Vec<&str> = vec![""; spans.nodes().len() + 1];
            let mut types: Vec<P<Type>> = Vec::with_capacity(spans.nodes().len());
            texts[0] = node.as_template_literal_type_node().head.text();
            for (i, &span) in spans.nodes().iter().enumerate() {
                texts[i + 1] = span.as_template_literal_type_span().literal.text();
                types.push(self.get_type_from_type_node(span.type_node().unwrap()));
            }
            links.resolved_type.set(Some(self.get_template_literal_type(&texts, &types)));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:24725
    pub(crate) fn get_type_from_mapped_type_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let t = self.new_object_type(ObjectFlags::Mapped, node.symbol());
            t.as_mapped_type().declaration.set(Some(node));
            t.set_alias(self.get_alias_for_type_node(node));
            links.resolved_type.set(Some(t));
            // Eagerly resolve the constraint type which forces an error if the constraint type circularly
            // references itself through one or more type aliases.
            self.get_constraint_type_from_mapped_type(t);
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:24739
    pub(crate) fn get_type_from_conditional_type_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let check_type =
                self.get_type_from_type_node(node.as_conditional_type_node().check_type);
            let alias = self.get_alias_for_type_node(node);
            let all_outer_type_parameters =
                self.get_outer_type_parameters(node, true /*includeThisTypes*/);
            let outer_type_parameters: Vec<P<Type>> =
                if alias.is_some_and(|a| !a.type_arguments.get().is_empty()) {
                    all_outer_type_parameters
                } else {
                    all_outer_type_parameters
                        .into_iter()
                        .filter(|&tp| self.is_type_parameter_possibly_referenced(tp, node))
                        .collect()
                };
            let extends_type =
                self.get_type_from_type_node(node.as_conditional_type_node().extends_type);
            let infer_type_parameters = self.get_infer_type_parameters(node);
            let outer_type_parameters = alloc_vec(outer_type_parameters);
            let root = P::new(ConditionalRoot {
                node: Cell::new(Some(node)),
                check_type: Cell::new(Some(check_type)),
                extends_type: Cell::new(Some(extends_type)),
                is_distributive: Cell::new(check_type.flags().intersects(TypeFlags::TypeParameter)),
                infer_type_parameters: Cell::new(alloc_vec(infer_type_parameters)),
                outer_type_parameters: Cell::new(outer_type_parameters),
                instantiations: GoPackedMap::default(),
                alias: Cell::new(alias),
            });
            links.resolved_type.set(Some(self.get_conditional_type(
                root, None,  /*mapper*/
                false, /*forConstraint*/
                None,
            )));
            if !outer_type_parameters.is_empty() {
                root.instantiations.make();
                root.instantiations.set(
                    get_conditional_type_key(
                        outer_type_parameters,
                        None,  /*alias*/
                        false, /*forConstraint*/
                    ),
                    links.resolved_type.get().unwrap(),
                );
            }
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:24770
    pub(crate) fn get_conditional_type(
        &mut self,
        root: P<ConditionalRoot>,
        mapper: Option<P<TypeMapper>>,
        for_constraint: bool,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        let (mappers, contexts, lists) = (
            self.scratch_mappers.len(),
            self.scratch_contexts.len(),
            self.scratch_mapper_lists.len(),
        );
        let census_span =
            self.census_begin(crate::workcensus::Cat::Cond, || crate::workcensus::CKey::Root(root));
        let result = self.get_conditional_type_worker(root, mapper, for_constraint, alias);
        if let Some(t) = self.census_end(census_span) {
            let never = (result == self.never_type) as u64;
            self.census.as_mut().unwrap().record(
                crate::workcensus::Cat::Cond,
                crate::workcensus::CKey::Root(root),
                t,
                never,
                0,
                0,
            );
        }
        // The `infer` contexts and the composite mappers made for them are garbage unless one of their mappers was
        // stored (74% / 61-88% are, notes/mem-census.md). Contexts first: recycling reads their mapper fields.
        while self.scratch_contexts.len() > contexts {
            let ctx = self.scratch_contexts.pop().unwrap();
            InferenceContext::recycle(ctx);
        }
        while self.scratch_mappers.len() > mappers {
            let m = self.scratch_mappers.pop().unwrap();
            // SAFETY: made by this call (below), which is done; escaped mappers are kept.
            unsafe { recycle_mapper(m) };
        }
        while self.scratch_mapper_lists.len() > lists {
            let (m, targets) = self.scratch_mapper_lists.pop().unwrap();
            // SAFETY: made by this call's `getTailRecursionRoot` with its list; escaped mappers keep theirs.
            unsafe { recycle_mapper_with_targets(m, targets) };
        }
        result
    }

    fn get_conditional_type_worker(
        &mut self,
        root: P<ConditionalRoot>,
        mapper: Option<P<TypeMapper>>,
        for_constraint: bool,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        let mut root = root;
        let mut mapper = mapper;
        let mut alias = alias;
        let result: P<Type>;
        let mut extra_types: Option<Vec<P<Type>>> = None;
        let mut tail_count = 0;
        // We loop here for an immediately nested conditional type in the false position, effectively treating
        // types of the form 'A extends B ? X : C extends D ? Y : E extends F ? Z : ...' as a single construct for
        // purposes of resolution. We also loop here when resolution of a conditional type ends in resolution of
        // another (or, through recursion, possibly the same) conditional type. In the potentially tail-recursive
        // cases we increment the tail recursion counter and stop after 1000 iterations.
        loop {
            if tail_count == 1000 {
                self.error(
                    self.current_node,
                    &diagnostics::TYPE_INSTANTIATION_IS_EXCESSIVELY_DEEP_AND_POSSIBLY_INFINITE,
                    &[],
                );
                return self.error_type;
            }
            let actual_check_type =
                self.get_actual_type_variable(root.check_type.get().unwrap()).unwrap();
            let check_type = self.instantiate_type(actual_check_type, mapper);
            let extends_type = self.instantiate_type(root.extends_type.get().unwrap(), mapper);
            if check_type == self.error_type || extends_type == self.error_type {
                return self.error_type;
            }
            if check_type == self.wildcard_type || extends_type == self.wildcard_type {
                return self.wildcard_type;
            }
            let root_node = root.node.get().unwrap();
            let check_type_node =
                ast::skip_type_parentheses(root_node.as_conditional_type_node().check_type);
            let extends_type_node =
                ast::skip_type_parentheses(root_node.as_conditional_type_node().extends_type);
            // When the check and extends types are simple tuple types of the same arity, we defer resolution of the
            // conditional type when any tuple elements are generic. This is such that non-distributable conditional
            // types can be written `[X] extends [Y] ? ...` and be deferred similarly to `X extends Y ? ...`.
            let check_tuples = Self::is_simple_tuple_type(check_type_node)
                && Self::is_simple_tuple_type(extends_type_node)
                && check_type_node.elements().len() == extends_type_node.elements().len();
            let check_type_deferred = self.is_deferred_type(check_type, check_tuples);
            let mut combined_mapper: Option<P<TypeMapper>> = None;
            if !root.infer_type_parameters.get().is_empty() {
                // When we're looking at making an inference for an infer type, when we get its constraint, it'll automagically be
                // instantiated with the context, so it doesn't need the mapper for the inference context - however the constraint
                // may refer to another _root_, _uncloned_ `infer` type parameter [1], or to something mapped by `mapper` [2].
                // [1] Eg, if we have `Foo<T, U extends T>` and `Foo<number, infer B>` - `B` is constrained to `T`, which, in turn, has been instantiated
                // as `number`
                // Conversely, if we have `Foo<infer A, infer B>`, `B` is still constrained to `T` and `T` is instantiated as `A`
                // [2] Eg, if we have `Foo<T, U extends T>` and `Foo<Q, infer B>` where `Q` is mapped by `mapper` into `number` - `B` is constrained to `T`
                // which is in turn instantiated as `Q`, which is in turn instantiated as `number`.
                // So we need to:
                //    * combine `context.nonFixingMapper` with `mapper` so their constraints can be instantiated in the context of `mapper` (otherwise they'd only get inference context information)
                //    * incorporate all of the component mappers into the combined mapper for the true and false members
                // This means we have two mappers that need applying:
                //    * The original `mapper` used to create this conditional
                //    * The mapper that maps the infer type parameter to its inference result (`context.mapper`)
                let context = Self::new_inference_context(
                    root.infer_type_parameters.get(),
                    None, /*signature*/
                    InferenceFlags::None,
                    None,
                );
                self.scratch_contexts.push(context);
                if let Some(mapper) = mapper {
                    let own = context.non_fixing_mapper().unwrap();
                    let non_fixing_mapper = Self::combine_type_mappers(Some(own), mapper);
                    context.set_non_fixing_mapper(non_fixing_mapper);
                    // The context no longer holds its own non-fixing mapper, so it is recycled from here.
                    self.scratch_mappers.push(own);
                    self.scratch_mappers.push(non_fixing_mapper);
                }
                if !check_type_deferred {
                    // We don't want inferences from constraints as they may cause us to eagerly resolve the
                    // conditional type instead of deferring resolution. Also, we always want strict function
                    // types rules (i.e. proper contravariance) for inferences.
                    self.infer_types(
                        context.inferences.get(),
                        check_type,
                        extends_type,
                        InferencePriority::NO_CONSTRAINTS | InferencePriority::ALWAYS_STRICT,
                        false,
                    );
                }
                // It's possible for 'infer T' type parameters to be given uninstantiated constraints when the
                // those type parameters are used in type references (see getInferredTypeParameterConstraint). For
                // that reason we need context.mapper to be first in the combined mapper. See #42636 for examples.
                if let Some(mapper) = mapper {
                    let combined = Self::combine_type_mappers(context.mapper(), mapper);
                    self.scratch_mappers.push(combined);
                    combined_mapper = Some(combined);
                } else {
                    combined_mapper = context.mapper();
                }
            }
            // Instantiate the extends type including inferences for 'infer T' type parameters
            let inferred_extends_type = if combined_mapper.is_some() {
                self.instantiate_type(root.extends_type.get().unwrap(), combined_mapper)
            } else {
                extends_type
            };
            // We attempt to resolve the conditional type only when the check and extends types are non-generic
            if !check_type_deferred && !self.is_deferred_type(inferred_extends_type, check_tuples) {
                // Return falseType for a definitely false extends check. We check an instantiations of the two
                // types with type parameters mapped to the wildcard type, the most permissive instantiations
                // possible (the wildcard type is assignable to and from all types). If those are not related,
                // then no instantiations will be and we can just return the false branch type.
                if !inferred_extends_type.flags().intersects(TypeFlags::AnyOrUnknown)
                    && (check_type.flags().intersects(TypeFlags::Any) || {
                        let permissive_check_type = self.get_permissive_instantiation(check_type);
                        let permissive_extends_type =
                            self.get_permissive_instantiation(inferred_extends_type);
                        !self.is_type_assignable_to(permissive_check_type, permissive_extends_type)
                    })
                {
                    // Return union of trueType and falseType for 'any' since it matches anything. Furthermore, for a
                    // distributive conditional type applied to the constraint of a type variable, include trueType if
                    // there are possible values of the check type that are also possible values of the extends type.
                    // We use a reverse assignability check as it is less expensive than the comparable relationship
                    // and avoids false positives of a non-empty intersection check.
                    if check_type.flags().intersects(TypeFlags::Any)
                        || for_constraint
                            && !inferred_extends_type.flags().intersects(TypeFlags::Never)
                            && {
                                let permissive_extends_type =
                                    self.get_permissive_instantiation(inferred_extends_type);
                                some_type(self, permissive_extends_type, |c, t| {
                                    let permissive_check_type =
                                        c.get_permissive_instantiation(check_type);
                                    c.is_type_assignable_to(t, permissive_check_type)
                                })
                            }
                    {
                        let true_type = self.get_type_from_type_node(
                            root_node.as_conditional_type_node().true_type,
                        );
                        let instantiated =
                            self.instantiate_type(true_type, combined_mapper.or(mapper));
                        extra_types.get_or_insert_with(Vec::new).push(instantiated);
                    }
                    // If falseType is an immediately nested conditional type that isn't distributive or has an
                    // identical checkType, switch to that type and loop.
                    let false_type = self
                        .get_type_from_type_node(root_node.as_conditional_type_node().false_type);
                    if false_type.flags().intersects(TypeFlags::Conditional) {
                        let new_root = false_type.as_conditional_type().root.get().unwrap();
                        if new_root.node.get().unwrap().parent() == Some(root_node)
                            && (!new_root.is_distributive.get()
                                || new_root.check_type.get() == root.check_type.get())
                        {
                            root = new_root;
                            continue;
                        }
                        if let (Some(new_root), new_root_mapper) =
                            self.get_tail_recursion_root(false_type, mapper)
                        {
                            root = new_root;
                            mapper = new_root_mapper;
                            alias = None;
                            if new_root.alias.get().is_some() {
                                tail_count += 1;
                            }
                            continue;
                        }
                    }
                    result = self.instantiate_type(false_type, mapper);
                    break;
                }
                // Return trueType for a definitely true extends check. We check instantiations of the two
                // types with type parameters mapped to their restrictive form, i.e. a form of the type parameter
                // that has no constraint. This ensures that, for example, the type
                //   type Foo<T extends { x: any }> = T extends { x: string } ? string : number
                // doesn't immediately resolve to 'string' instead of being deferred.
                if inferred_extends_type.flags().intersects(TypeFlags::AnyOrUnknown) || {
                    let restrictive_check_type = self.get_restrictive_instantiation(check_type);
                    let restrictive_extends_type =
                        self.get_restrictive_instantiation(inferred_extends_type);
                    self.is_type_assignable_to(restrictive_check_type, restrictive_extends_type)
                } {
                    let true_type = self
                        .get_type_from_type_node(root_node.as_conditional_type_node().true_type);
                    let true_mapper = combined_mapper.or(mapper);
                    if let (Some(new_root), new_root_mapper) =
                        self.get_tail_recursion_root(true_type, true_mapper)
                    {
                        root = new_root;
                        mapper = new_root_mapper;
                        alias = None;
                        if new_root.alias.get().is_some() {
                            tail_count += 1;
                        }
                        continue;
                    }
                    result = self.instantiate_type(true_type, true_mapper);
                    break;
                }
            }
            // Return a deferred type for a check that is neither definitely true nor definitely false
            result = self.new_conditional_type(root, mapper, combined_mapper);
            if alias.is_some() {
                result.set_alias(alias);
            } else {
                let instantiated_alias = self.instantiate_type_alias(root.alias.get(), mapper);
                result.set_alias(instantiated_alias);
            }
            break;
        }
        if let Some(mut extra_types) = extra_types {
            extra_types.push(result);
            return self.get_union_type(&extra_types);
        }
        result
    }

    // We tail-recurse for generic conditional types that (a) have not already been evaluated and cached, and
    // (b) are non distributive, have a check type that is unaffected by instantiation, or have a non-union check
    // type. Note that recursion is possible only through aliased conditional types, so we only increment the tail
    // recursion counter for those.
    // checker.go:24920
    fn get_tail_recursion_root(
        &mut self,
        new_type: P<Type>,
        new_mapper: Option<P<TypeMapper>>,
    ) -> (Option<P<ConditionalRoot>>, Option<P<TypeMapper>>) {
        if new_type.flags().intersects(TypeFlags::Conditional)
            && let Some(new_mapper) = new_mapper
        {
            let new_root = new_type.as_conditional_type().root.get().unwrap();
            if !new_root.outer_type_parameters.get().is_empty() {
                let conditional_mapper = new_type.as_conditional_type().mapper.get();
                let type_param_mapper = Self::combine_type_mappers(conditional_mapper, new_mapper);
                let type_arguments: Vec<P<Type>> = new_root
                    .outer_type_parameters
                    .get()
                    .iter()
                    .map(|&t| type_param_mapper.map(self, t))
                    .collect();
                if conditional_mapper.is_some() {
                    // SAFETY: the composite made above, used only by those `map` calls.
                    unsafe { recycle_mapper(type_param_mapper) };
                }
                let type_argument_list = tsrs_core::alloc_slice_recycled(&type_arguments);
                let new_root_mapper =
                    new_type_mapper(new_root.outer_type_parameters.get(), type_argument_list);
                let mut new_check_type = None;
                if new_root.is_distributive.get() {
                    new_check_type = Some(
                        self.get_mapped_type(new_root.check_type.get().unwrap(), new_root_mapper),
                    );
                }
                if new_check_type.is_none()
                    || new_check_type == new_root.check_type.get()
                    || !new_check_type
                        .unwrap()
                        .flags()
                        .intersects(TypeFlags::Union | TypeFlags::Never)
                {
                    // The caller's `getConditionalType` loops with this mapper and recycles it on return unless
                    // the result kept it (99% do not, notes/mem-scoped-arenas.md).
                    self.scratch_mapper_lists.push((new_root_mapper, type_argument_list));
                    return (Some(new_root), Some(new_root_mapper));
                }
                // SAFETY: made above; mapping a type through it stores nothing.
                unsafe { recycle_mapper_with_targets(new_root_mapper, type_argument_list) };
            }
        }
        (None, None)
    }

    // checker.go:24939
    fn is_simple_tuple_type(node: P<Node>) -> bool {
        ast::is_tuple_type_node(node)
            && !node.elements().is_empty()
            && !node.elements().iter().any(|&e| {
                ast::is_optional_type_node(e)
                    || ast::is_rest_type_node(e)
                    || ast::is_named_tuple_member(e)
                        && (e.question_token().is_some()
                            || e.as_named_tuple_member().dot_dot_dot_token.is_some())
            })
    }

    // checker.go:24945
    fn is_deferred_type(&mut self, t: P<Type>, check_tuples: bool) -> bool {
        self.is_generic_type(t)
            || check_tuples && is_tuple_type(t) && {
                let element_types = self.get_element_types(t);
                element_types.iter().any(|&e| self.is_generic_type(e))
            }
    }

    // checker.go:24949
    pub(crate) fn get_permissive_instantiation(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Primitive | TypeFlags::AnyOrUnknown | TypeFlags::Never) {
            return t;
        }
        let key = CachedTypeKey { kind: CachedTypeKind::PermissiveInstantiation, type_id: t.id };
        if let Some(&cached) = self.cached_types.get(&key) {
            return cached;
        }
        let result = self.instantiate_type(t, Some(self.permissive_mapper));
        self.cached_types.insert(key, result);
        result
    }

    // checker.go:24962
    pub(crate) fn get_restrictive_instantiation(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Primitive | TypeFlags::AnyOrUnknown | TypeFlags::Never) {
            return t;
        }
        let key = CachedTypeKey { kind: CachedTypeKind::RestrictiveInstantiation, type_id: t.id };
        if let Some(&cached) = self.cached_types.get(&key) {
            return cached;
        }
        let result = self.instantiate_type(t, Some(self.restrictive_mapper));
        self.cached_types.insert(key, result);
        // We set the following so we don't attempt to set the restrictive instance of a restrictive instance
        // which is redundant - we'll produce new type identities, but all type params have already been mapped.
        // This also gives us a way to detect restrictive instances upon comparisons and _disable_ the "distributeive constraint"
        // assignability check for them, which is distinctly unsafe, as once you have a restrctive instance, all the type parameters
        // are constrained to `unknown` and produce tons of false positives/negatives!
        self.cached_types.insert(
            CachedTypeKey { kind: CachedTypeKind::RestrictiveInstantiation, type_id: result.id },
            result,
        );
        result
    }

    // checker.go:24981
    fn get_restrictive_type_parameter(&mut self, t: P<Type>) -> P<Type> {
        if t.as_type_parameter().constraint.get().is_none()
            && Self::get_constraint_declaration(t).is_none()
            || t.as_type_parameter().constraint.get() == Some(self.no_constraint_type)
        {
            return t;
        }
        let key = CachedTypeKey { kind: CachedTypeKind::RestrictiveTypeParameter, type_id: t.id };
        if let Some(&cached) = self.cached_types.get(&key) {
            return cached;
        }
        let result = self.new_type_parameter(t.symbol());
        result.as_type_parameter().constraint.set(Some(self.no_constraint_type));
        self.cached_types.insert(key, result);
        result
    }

    // checker.go:24995
    pub(crate) fn restrictive_mapper_worker(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::TypeParameter) {
            return self.get_restrictive_type_parameter(t);
        }
        t
    }

    // checker.go:25002
    pub(crate) fn permissive_mapper_worker(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::TypeParameter) {
            return self.wildcard_type;
        }
        t
    }

    // checker.go:25009
    pub(crate) fn get_true_type_from_conditional_type(&mut self, t: P<Type>) -> P<Type> {
        let d = t.as_conditional_type();
        if d.resolved_true_type.get().is_none() {
            let true_type_node =
                d.root.get().unwrap().node.get().unwrap().as_conditional_type_node().true_type;
            let true_type = self.get_type_from_type_node(true_type_node);
            d.resolved_true_type.set(Some(self.instantiate_type(true_type, d.mapper.get())));
        }
        d.resolved_true_type.get().unwrap()
    }

    // checker.go:25017
    pub(crate) fn get_false_type_from_conditional_type(&mut self, t: P<Type>) -> P<Type> {
        let d = t.as_conditional_type();
        if d.resolved_false_type.get().is_none() {
            let false_type_node =
                d.root.get().unwrap().node.get().unwrap().as_conditional_type_node().false_type;
            let false_type = self.get_type_from_type_node(false_type_node);
            d.resolved_false_type.set(Some(self.instantiate_type(false_type, d.mapper.get())));
        }
        d.resolved_false_type.get().unwrap()
    }

    // checker.go:25025
    pub(crate) fn get_inferred_true_type_from_conditional_type(&mut self, t: P<Type>) -> P<Type> {
        let d = t.as_conditional_type();
        if d.resolved_inferred_true_type.get().is_none() {
            if d.combined_mapper.get().is_some() {
                let true_type_node =
                    d.root.get().unwrap().node.get().unwrap().as_conditional_type_node().true_type;
                let true_type = self.get_type_from_type_node(true_type_node);
                d.resolved_inferred_true_type
                    .set(Some(self.instantiate_type(true_type, d.combined_mapper.get())));
            } else {
                d.resolved_inferred_true_type
                    .set(Some(self.get_true_type_from_conditional_type(t)));
            }
        }
        d.resolved_inferred_true_type.get().unwrap()
    }

    // checker.go:25037
    pub(crate) fn get_type_from_infer_type_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let symbol =
                self.get_symbol_of_declaration(node.as_infer_type_node().type_parameter).unwrap();
            links.resolved_type.set(Some(self.get_declared_type_of_type_parameter(symbol)));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:25045
    pub(crate) fn get_type_from_import_type_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let n = node.as_import_type_node();
            if !ast::is_literal_import_type_node(node) {
                self.error(Some(n.argument), &diagnostics::STRING_LITERAL_EXPECTED, &[]);
                self.symbol_node_links.get(node).resolved_symbol.set(Some(self.unknown_symbol));
                links.resolved_type.set(Some(self.error_type));
                return links.resolved_type.get().unwrap();
            }
            let target_meaning = if n.is_type_of { SymbolFlags::Value } else { SymbolFlags::Type };
            // TODO: Future work: support unions/generics/whatever via a deferred import-type
            let import_attributes_type =
                self.get_type_from_import_attributes(ast::get_import_attributes(node));
            let inner_module_symbol = self.resolve_external_module_name(
                node,
                n.argument.as_literal_type_node().literal,
                false, /*ignoreErrors*/
                import_attributes_type,
            );
            let Some(inner_module_symbol) = inner_module_symbol else {
                self.symbol_node_links.get(node).resolved_symbol.set(Some(self.unknown_symbol));
                links.resolved_type.set(Some(self.error_type));
                return links.resolved_type.get().unwrap();
            };
            let module_symbol = self.resolve_external_module_symbol(
                inner_module_symbol,
                false, /*dontResolveAlias*/
            );
            if !ast::node_is_missing(n.qualifier) {
                let name_chain = Self::get_identifier_chain(n.qualifier.unwrap());
                let mut current_namespace = module_symbol;
                for (i, &current) in name_chain.iter().enumerate() {
                    let mut meaning = SymbolFlags::Namespace;
                    if i == name_chain.len() - 1 {
                        meaning = target_meaning;
                    }
                    // typeof a.b.c is normally resolved using `checkExpression` which in turn defers to `checkQualifiedName`
                    // That, in turn, ultimately uses `getPropertyOfType` on the type of the symbol, which differs slightly from
                    // the `exports` lookup process that only looks up namespace members which is used for most type references
                    let resolved = self.resolve_symbol(current_namespace);
                    let merged_resolved_symbol = self.get_merged_symbol(resolved);
                    let mut symbol_from_variable = None;
                    let mut symbol_from_module = None;
                    if n.is_type_of {
                        let t = self.get_type_of_symbol(merged_resolved_symbol);
                        symbol_from_variable = self.get_property_of_type_ex(
                            t,
                            current.text(),
                            false, /*skipObjectFunctionPropertyAugment*/
                            true,  /*includeTypeOnlyMembers*/
                        );
                    } else {
                        let exports = self.get_exports_of_symbol(merged_resolved_symbol);
                        symbol_from_module = self.get_symbol(exports, current.text(), meaning);
                        if symbol_from_module.is_none() {
                            // a CommonJS module might have typedefs exported alongside an export=
                            // !!!
                            let immediate_module_symbol = self.resolve_external_module_symbol(
                                inner_module_symbol,
                                true, /*dontResolveAlias*/
                            );
                            let is_module_exports =
                                immediate_module_symbol.declarations().iter().any(|&d| {
                                    ast::get_assignment_declaration_kind(d)
                                        == JSDeclarationKind::ModuleExports
                                });
                            if is_module_exports {
                                let exports = self.get_exports_of_symbol(
                                    immediate_module_symbol.parent().unwrap(),
                                );
                                symbol_from_module =
                                    self.get_symbol(exports, current.text(), meaning);
                            }
                        }
                    }
                    let next = symbol_from_module.or(symbol_from_variable);
                    let Some(next) = next else {
                        let namespace_name = self.get_fully_qualified_name(current_namespace, None);
                        let member_name = tsrs_scanner::declaration_name_to_string(Some(current));
                        self.error(
                            Some(current),
                            &diagnostics::NAMESPACE_0_HAS_NO_EXPORTED_MEMBER_1,
                            &[&namespace_name, &member_name],
                        );
                        links.resolved_type.set(Some(self.error_type));
                        return links.resolved_type.get().unwrap();
                    };
                    self.symbol_node_links.get(current).resolved_symbol.set(Some(next));
                    self.symbol_node_links
                        .get(current.parent().unwrap())
                        .resolved_symbol
                        .set(Some(next));
                    current_namespace = next;
                }
                links.resolved_type.set(Some(self.resolve_import_symbol_type(
                    node,
                    current_namespace,
                    target_meaning,
                )));
            } else if self.get_symbol_flags(module_symbol).intersects(target_meaning) {
                links.resolved_type.set(Some(self.resolve_import_symbol_type(
                    node,
                    module_symbol,
                    target_meaning,
                )));
            } else {
                let message = if target_meaning == SymbolFlags::Value {
                    &diagnostics::MODULE_0_DOES_NOT_REFER_TO_A_VALUE_BUT_IS_USED_AS_A_VALUE_HERE
                } else {
                    &diagnostics::MODULE_0_DOES_NOT_REFER_TO_A_TYPE_BUT_IS_USED_AS_A_TYPE_HERE_DID_YOU_MEAN_TYPEOF_IMPORT_0
                };
                let text = n.argument.as_literal_type_node().literal.text();
                self.error(Some(node), message, &[&text]);
                self.symbol_node_links.get(node).resolved_symbol.set(Some(self.unknown_symbol));
                links.resolved_type.set(Some(self.error_type));
            }
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:25120
    fn get_identifier_chain(node: P<Node>) -> Vec<P<Node>> {
        if ast::is_identifier(node) {
            return vec![node];
        }
        let mut chain = Self::get_identifier_chain(node.as_qualified_name().left);
        chain.push(node.as_qualified_name().right);
        chain
    }

    // checker.go:25127
    fn resolve_import_symbol_type(
        &mut self,
        node: P<Node>,
        symbol: P<Symbol>,
        meaning: SymbolFlags,
    ) -> P<Type> {
        let resolved_symbol = self.resolve_symbol(symbol);
        self.symbol_node_links.get(node).resolved_symbol.set(Some(resolved_symbol));
        if meaning == SymbolFlags::Value {
            // intentionally doesn't use resolved symbol so type is cached as expected on the alias
            let t = self.get_type_of_symbol(symbol);
            return self.get_instantiation_expression_type(t, node);
        }
        // getTypeReferenceType doesn't handle aliases - it must get the resolved symbol
        self.get_type_reference_type(node, resolved_symbol)
    }

    // checker.go:25138
    pub(crate) fn create_type_from_generic_global_type(
        &mut self,
        generic_global_type: P<Type>,
        type_arguments: &[P<Type>],
    ) -> P<Type> {
        if generic_global_type != self.empty_generic_type {
            return self.create_type_reference(generic_global_type, type_arguments);
        }
        self.empty_object_type
    }

    // checker.go:25145
    pub(crate) fn get_global_strict_function_type(&mut self, name: &str) -> P<Type> {
        if self.strict_bind_call_apply {
            return self.get_global_type(name, 0 /*arity*/, true /*reportErrors*/);
        }
        self.global_function_type
    }

    // checker.go:25152
    pub(crate) fn get_global_import_meta_expression_type(&mut self) -> P<Type> {
        if self.deferred_global_import_meta_expression_type.is_none() {
            // Create a synthetic type `ImportMetaExpression { meta: MetaProperty }`
            let symbol = self.new_symbol(SymbolFlags::None, "ImportMetaExpression");
            let import_meta_type = self.get_global_import_meta_type();
            let meta_property_symbol =
                self.new_symbol_ex(SymbolFlags::Property, "meta", CheckFlags::Readonly);
            meta_property_symbol.set_parent(Some(symbol));
            self.value_symbol_links
                .get(meta_property_symbol)
                .resolved_type
                .set(Some(import_meta_type));
            let members = create_symbol_table(&[meta_property_symbol]);
            symbol.set_members(members);
            self.deferred_global_import_meta_expression_type =
                Some(self.new_anonymous_type(Some(symbol), members, &[], &[], &[]));
        }
        self.deferred_global_import_meta_expression_type.unwrap()
    }

    // checker.go:25167
    pub(crate) fn create_iterable_type(&mut self, iterated_type: P<Type>) -> P<Type> {
        let generic_global_type = self.get_global_iterable_type_checked();
        self.create_type_from_generic_global_type(
            generic_global_type,
            &[iterated_type, self.void_type, self.undefined_type],
        )
    }

    // checker.go:25171
    pub(crate) fn create_array_type(&mut self, element_type: P<Type>) -> P<Type> {
        self.create_array_type_ex(element_type, false /*readonly*/)
    }

    // checker.go:25175
    pub(crate) fn create_array_type_ex(
        &mut self,
        element_type: P<Type>,
        readonly: bool,
    ) -> P<Type> {
        let generic_global_type =
            if readonly { self.global_readonly_array_type } else { self.global_array_type };
        self.create_type_from_generic_global_type(generic_global_type, &[element_type])
    }

    // checker.go:25179
    pub(crate) fn get_tuple_element_flags(node: P<Node>) -> ElementFlags {
        match node.kind() {
            Kind::OptionalType => {
                return ElementFlags::Optional;
            }
            Kind::RestType => {
                return if Self::get_array_element_type_node(node.type_node().unwrap()).is_some() {
                    ElementFlags::Rest
                } else {
                    ElementFlags::Variadic
                };
            }
            Kind::NamedTupleMember => {
                let named = node.as_named_tuple_member();
                if named.question_token.is_some() {
                    return ElementFlags::Optional;
                } else if named.dot_dot_dot_token.is_some() {
                    return if Self::get_array_element_type_node(named.type_).is_some() {
                        ElementFlags::Rest
                    } else {
                        ElementFlags::Variadic
                    };
                }
                return ElementFlags::Required;
            }
            _ => {}
        }
        ElementFlags::Required
    }

    // checker.go:25198
    fn get_tuple_element_info(node: P<Node>) -> TupleElementInfo {
        TupleElementInfo {
            flags: Self::get_tuple_element_flags(node),
            labeled_declaration: if ast::is_named_tuple_member(node)
                || ast::is_parameter_declaration(node)
            {
                Some(node)
            } else {
                None
            },
        }
    }

    // checker.go:25205
    pub(crate) fn create_tuple_type(&mut self, element_types: &[P<Type>]) -> P<Type> {
        let element_infos: Vec<TupleElementInfo> = element_types
            .iter()
            .map(|_| TupleElementInfo { flags: ElementFlags::Required, labeled_declaration: None })
            .collect();
        self.create_tuple_type_ex(element_types, &element_infos, false /*readonly*/)
    }

    // checker.go:25210
    pub(crate) fn create_tuple_type_ex(
        &mut self,
        element_types: &[P<Type>],
        element_infos: &[TupleElementInfo],
        readonly: bool,
    ) -> P<Type> {
        let tuple_target = self.get_tuple_target_type(element_infos, readonly);
        if tuple_target == self.empty_generic_type {
            return self.empty_object_type;
        } else if !element_types.is_empty() {
            return self.create_normalized_type_reference(tuple_target, element_types);
        }
        tuple_target
    }

    // checker.go:25221
    pub(crate) fn get_tuple_target_type(
        &mut self,
        element_infos: &[TupleElementInfo],
        readonly: bool,
    ) -> P<Type> {
        if element_infos.len() == 1 && element_infos[0].flags.intersects(ElementFlags::Rest) {
            // [...X[]] is equivalent to just X[]
            if readonly {
                return self.global_readonly_array_type;
            }
            return self.global_array_type;
        }
        let key = get_tuple_key(element_infos, readonly);
        let mut t = self.tuple_types.get(&key);
        if t.is_none() {
            let target = self.create_tuple_target_type(element_infos, readonly);
            self.tuple_types.insert(key, target);
            t = Some(target);
        }
        t.unwrap()
    }

    // We represent tuple types as type references to synthesized generic interface types created by
    // this function. The types are of the form:
    //
    //	interface Tuple<T0, T1, T2, ...> extends Array<T0 | T1 | T2 | ...> { 0: T0, 1: T1, 2: T2, ... }
    //
    // Note that the generic type created by this function has no symbol associated with it. The same
    // is true for each of the synthesized type parameters.
    // checker.go:25245
    fn create_tuple_target_type(
        &mut self,
        element_infos: &[TupleElementInfo],
        readonly: bool,
    ) -> P<Type> {
        let arity = element_infos.len();
        let min_length = count_where(element_infos, |e| {
            e.flags.intersects(ElementFlags::Required | ElementFlags::Variadic)
        });
        let mut type_parameters: Vec<P<Type>> = Vec::new();
        let members = SymbolTable::new();
        let mut combined_flags = ElementFlags::None;
        if arity != 0 {
            // tsrs-only: Go makes new element type parameters for every target. Only the mappers of the target's
            // references observe them, each through the target's own list (notes/perf-shared-tuple-elements.md), so
            // all targets share one per index, and the index names with them: type-fest made 4.6M parameters and
            // 3.8M names at one checker. The element symbols stay per target: a union or intersection property
            // treats symbols with one target symbol as instantiations of one declaration
            // (`create_union_or_intersection_property`).
            while self.tuple_elements.len() < arity {
                let type_parameter = self.new_type_parameter(None);
                let name = alloc_str(&self.tuple_elements.len().to_string());
                self.tuple_elements.push((type_parameter, name));
            }
            type_parameters = Vec::with_capacity(arity + 1);
            for (i, element_info) in element_infos.iter().enumerate().take(arity) {
                let (type_parameter, name) = self.tuple_elements[i];
                type_parameters.push(type_parameter);
                let flags = element_info.flags;
                combined_flags |= flags;
                if !combined_flags.intersects(ElementFlags::Variable) {
                    let property = self.new_symbol_ex(
                        SymbolFlags::Property
                            | if flags.intersects(ElementFlags::Optional) {
                                SymbolFlags::Optional
                            } else {
                                SymbolFlags::None
                            },
                        name,
                        if readonly { CheckFlags::Readonly } else { CheckFlags::None },
                    );
                    self.value_symbol_links.get(property).resolved_type.set(Some(type_parameter));
                    // c.valueSymbolLinks.get(property).tupleLabelDeclaration = elementInfos[i].labeledDeclaration
                    members.set(property.name(), property);
                }
            }
        }
        let fixed_length = members.len();
        let length_symbol = self.new_symbol_ex(
            SymbolFlags::Property,
            "length",
            if readonly { CheckFlags::Readonly } else { CheckFlags::None },
        );
        if combined_flags.intersects(ElementFlags::Variable) {
            self.value_symbol_links.get(length_symbol).resolved_type.set(Some(self.number_type));
        } else {
            let mut literal_types = Vec::new();
            literal_types.extend(
                (min_length..=arity).map(|i| self.get_number_literal_type(Number(i as f64))),
            );
            let length_type = self.get_union_type(&literal_types);
            self.value_symbol_links.get(length_symbol).resolved_type.set(Some(length_type));
        }
        members.set(length_symbol.name(), length_symbol);
        let t = self.new_object_type(ObjectFlags::Tuple | ObjectFlags::Reference, None);
        let d = t.as_tuple_type();
        let this_type = self.new_type_parameter(None);
        d.this_type.set(Some(this_type));
        this_type.as_type_parameter().is_this_type.set(true);
        this_type.as_type_parameter().constraint.set(Some(t));
        type_parameters.push(this_type);
        d.all_type_parameters.set(alloc_vec(type_parameters));
        d.target.set(Some(t));
        d.resolved_type_arguments.set(Some(d.type_parameters()));
        d.instantiations.make();
        d.instantiations.add(t);
        d.declared_members_resolved.set(true);
        d.declared_members.set(Some(members));
        d.element_infos.set(alloc_slice(element_infos));
        d.min_length.set(min_length);
        d.fixed_length.set(fixed_length);
        d.combined_flags.set(combined_flags);
        d.readonly.set(readonly);
        t
    }

    // checker.go:25300
    pub(crate) fn get_element_type_of_slice_of_tuple_type(
        &mut self,
        t: P<Type>,
        index: usize,
        end_skip_count: usize,
        writing: bool,
        no_reductions: bool,
    ) -> Option<P<Type>> {
        let length = Self::get_type_reference_arity(t).saturating_sub(end_skip_count);
        let element_infos = t.target_tuple_type().element_infos.get();
        if index < length {
            let type_arguments = self.get_type_arguments(t);
            let mut element_types = Vec::new();
            element_types.extend((index..length).map(|i| {
                let mut e = type_arguments[i];
                if element_infos[i].flags.intersects(ElementFlags::Variadic) {
                    e = self.get_indexed_access_type(e, self.number_type);
                }
                e
            }));
            if writing {
                return Some(self.get_intersection_type(&element_types));
            }
            return Some(self.get_union_type_ex(
                &element_types,
                if no_reductions { UnionReduction::None } else { UnionReduction::Literal },
                AliasArg::None,
                None,
            ));
        }
        None
    }

    // checker.go:25321
    pub(crate) fn get_rest_type_of_tuple_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        let fixed_length = t.target_tuple_type().fixed_length.get();
        self.get_element_type_of_slice_of_tuple_type(t, fixed_length, 0, false, false)
    }

    // checker.go:25325
    pub(crate) fn get_tuple_element_type_out_of_start_count(
        &mut self,
        t: P<Type>,
        index: Number,
        undefined_like_type: Option<P<Type>>,
    ) -> P<Type> {
        self.map_type(t, |c, t| {
            let Some(rest_type) = c.get_rest_type_of_tuple_type(t) else {
                return Some(c.undefined_type);
            };
            if let Some(undefined_like_type) = undefined_like_type
                && index >= Number(get_total_fixed_element_count(t.target_tuple_type()) as f64)
            {
                return Some(c.get_union_type(&[rest_type, undefined_like_type]));
            }
            Some(rest_type)
        })
        .unwrap()
    }

    // checker.go:25338
    pub(crate) fn is_generic_type(&mut self, t: P<Type>) -> bool {
        !self.get_generic_object_flags(t).is_empty()
    }

    // checker.go:25342
    pub(crate) fn is_generic_object_type(&mut self, t: P<Type>) -> bool {
        self.get_generic_object_flags(t).intersects(ObjectFlags::IsGenericObjectType)
    }

    // checker.go:25346
    pub(crate) fn is_generic_index_type(&mut self, t: P<Type>) -> bool {
        self.get_generic_object_flags(t).intersects(ObjectFlags::IsGenericIndexType)
    }

    // checker.go:25350
    fn get_generic_object_flags(&mut self, t: P<Type>) -> ObjectFlags {
        let mut combined_flags = ObjectFlags::None;
        if t.flags().intersects(TypeFlags::UnionOrIntersection | TypeFlags::Substitution) {
            if !t.object_flags_lazy().intersects(ObjectFlags::IsGenericTypeComputed) {
                if t.flags().intersects(TypeFlags::UnionOrIntersection) {
                    combined_flags =
                        t.types().iter().fold(combined_flags, |mut combined_flags, &u| {
                            combined_flags |= self.get_generic_object_flags(u);

                            combined_flags
                        });
                } else {
                    let d = t.as_substitution_type();
                    combined_flags = self.get_generic_object_flags(d.base_type.get().unwrap())
                        | self.get_generic_object_flags(d.constraint.get().unwrap());
                }
                t.object_flags.set(
                    t.object_flags_lazy() | ObjectFlags::IsGenericTypeComputed | combined_flags,
                );
            }
            return t.object_flags_lazy() & ObjectFlags::IsGenericType;
        }
        if t.flags().intersects(TypeFlags::InstantiableNonPrimitive)
            || self.is_generic_mapped_type(t)
            || Self::is_generic_tuple_type(t)
        {
            combined_flags |= ObjectFlags::IsGenericObjectType;
        }
        if t.flags().intersects(TypeFlags::InstantiableNonPrimitive | TypeFlags::Index)
            || self.is_generic_string_like_type(t)
        {
            combined_flags |= ObjectFlags::IsGenericIndexType;
        }
        combined_flags
    }

    // checker.go:25374
    #[inline]
    pub(crate) fn is_generic_tuple_type(t: P<Type>) -> bool {
        is_tuple_type(t)
            && t.target_tuple_type().combined_flags.get().intersects(ElementFlags::Variadic)
    }

    // checker.go:25378
    #[inline]
    pub(crate) fn is_generic_mapped_type(&mut self, t: P<Type>) -> bool {
        t.object_flags().intersects(ObjectFlags::Mapped) && self.is_generic_mapped_type_worker(t)
    }

    #[inline(never)]
    fn is_generic_mapped_type_worker(&mut self, t: P<Type>) -> bool {
        let constraint = self.get_constraint_type_from_mapped_type(t);
        if self.is_generic_index_type(constraint) {
            return true;
        }
        // A mapped type is generic if the 'as' clause references generic types other than the iteration type.
        // To determine this, we substitute the constraint type (that we now know isn't generic) for the iteration
        // type and check whether the resulting type is generic.
        let name_type = self.get_name_type_from_mapped_type(t);
        if let Some(name_type) = name_type {
            let type_parameter = self.get_type_parameter_from_mapped_type(t);
            let mapper = new_simple_type_mapper(type_parameter, constraint);
            let instantiated = self.instantiate_type(name_type, Some(mapper));
            // SAFETY: made here for this one instantiation; kept if the result stored it.
            unsafe { recycle_mapper(mapper) };
            if self.is_generic_index_type(instantiated) {
                return true;
            }
        }
        false
    }

    /**
     * A union type which is reducible upon instantiation (meaning some members are removed under certain instantiations)
     * must be kept generic, as that instantiation information needs to flow through the type system. By replacing all
     * type parameters in the union with a special never type that is treated as a literal in `getReducedType`, we can cause
     * the `getReducedType` logic to reduce the resulting type if possible (since only intersections with conflicting
     * literal-typed properties are reducible).
     */
    // checker.go:25402
    pub(crate) fn is_generic_reducible_type(&mut self, t: P<Type>) -> bool {
        t.flags().intersects(TypeFlags::Union)
            && t.object_flags().intersects(ObjectFlags::ContainsIntersections)
            && t.types().iter().any(|&t| self.is_generic_reducible_type(t))
            || t.flags().intersects(TypeFlags::Intersection) && self.is_reducible_intersection(t)
    }

    // checker.go:25407
    fn is_reducible_intersection(&mut self, t: P<Type>) -> bool {
        let d = t.as_intersection_type();
        if d.unique_literal_filled_instantiation().is_none() {
            d.set_unique_literal_filled_instantiation(Some(
                self.instantiate_type(t, Some(self.unique_literal_mapper)),
            ));
        }
        let instantiation = d.unique_literal_filled_instantiation().unwrap();
        self.get_reduced_type(instantiation) != instantiation
    }

    // checker.go:25415
    pub(crate) fn get_unique_literal_type_for_type_parameter(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::TypeParameter) {
            return self.unique_literal_type;
        }
        t
    }

    // checker.go:25422
    pub(crate) fn get_conditional_flow_type_of_type(
        &mut self,
        t: P<Type>,
        node: Option<P<Node>>,
    ) -> P<Type> {
        let mut constraints: Vec<P<Type>> = Vec::new();
        let mut covariant = true;
        let mut node = node;
        while let Some(n) = node {
            if ast::is_statement(n) || n.kind() == Kind::JSDoc {
                break;
            }
            let parent = n.parent().unwrap();
            // only consider variance flipped by parameter locations - `keyof` types would usually be considered variance inverting, but
            // often get used in indexed accesses where they behave sortof invariantly, but our checking is lax
            if ast::is_parameter_declaration(parent) {
                covariant = !covariant;
            }
            // Always substitute on type parameters, regardless of variance, since even
            // in contravariant positions, they may rely on substituted constraints to be valid
            if (covariant || t.flags().intersects(TypeFlags::TypeVariable))
                && ast::is_conditional_type_node(parent)
                && n == parent.as_conditional_type_node().true_type
            {
                let constraint = self.get_implied_constraint(
                    t,
                    parent.as_conditional_type_node().check_type,
                    parent.as_conditional_type_node().extends_type,
                );
                if let Some(constraint) = constraint {
                    constraints.push(constraint);
                }
            } else if t.flags().intersects(TypeFlags::TypeParameter)
                && ast::is_mapped_type_node(parent)
                && parent.as_mapped_type_node().name_type.is_none()
                && Some(n) == parent.type_node()
            {
                let mapped_type = self.get_type_from_type_node(parent);
                let type_parameter_from_mapped_type =
                    self.get_type_parameter_from_mapped_type(mapped_type);
                if Some(type_parameter_from_mapped_type) == self.get_actual_type_variable(t) {
                    let type_parameter = self.get_homomorphic_type_variable(mapped_type);
                    if let Some(type_parameter) = type_parameter {
                        let constraint = self.get_constraint_of_type_parameter(type_parameter);
                        if let Some(constraint) = constraint
                            && every_type(
                                self,
                                constraint,
                                super::checker::Checker::is_array_or_tuple_type,
                            )
                        {
                            constraints.push(
                                self.get_union_type(&[self.number_type, self.numeric_string_type]),
                            );
                        }
                    }
                }
            }
            node = Some(parent);
        }
        if !constraints.is_empty() {
            let constraint = self.get_intersection_type(&constraints);
            return self.get_substitution_type(t, constraint);
        }
        t
    }

    // checker.go:25459
    fn get_implied_constraint(
        &mut self,
        t: P<Type>,
        check_node: P<Node>,
        extends_node: P<Node>,
    ) -> Option<P<Type>> {
        if is_unary_tuple_type_node(check_node) && is_unary_tuple_type_node(extends_node) {
            return self.get_implied_constraint(
                t,
                check_node.elements()[0],
                extends_node.elements()[0],
            );
        }
        let check_type = self.get_type_from_type_node(check_node);
        if self.get_actual_type_variable(check_type) == self.get_actual_type_variable(t) {
            return Some(self.get_type_from_type_node(extends_node));
        }
        None
    }
}

// checker.go:25469
fn is_unary_tuple_type_node(node: P<Node>) -> bool {
    ast::is_tuple_type_node(node) && node.elements().len() == 1
}

impl Checker {
    // checker.go:25473
    #[cfg_attr(feature = "site-counts", track_caller)]
    fn new_type<T: TypePayload>(
        &mut self,
        flags: TypeFlags,
        object_flags: ObjectFlags,
        data: T,
    ) -> P<Type> {
        self.type_count += 1;
        crate::sitecount::hit("type", type_kind_label(flags, object_flags));
        Type::alloc(
            flags,
            object_flags
                & !(ObjectFlags::CouldContainTypeVariablesComputed
                    | ObjectFlags::CouldContainTypeVariables
                    | ObjectFlags::MembersResolved
                    | ObjectFlags::WeakTypeMemo),
            TypeId(self.type_count),
            data,
        )
    }

    // checker.go:25487
    pub(crate) fn new_intrinsic_type(&mut self, flags: TypeFlags, intrinsic_name: &str) -> P<Type> {
        self.new_intrinsic_type_ex(flags, intrinsic_name, ObjectFlags::None)
    }

    // checker.go:25491
    pub(crate) fn new_intrinsic_type_ex(
        &mut self,
        flags: TypeFlags,
        intrinsic_name: &str,
        object_flags: ObjectFlags,
    ) -> P<Type> {
        let data = IntrinsicType::default();
        data.intrinsic_name.set(alloc_str(intrinsic_name));
        self.new_type(flags, object_flags, data)
    }

    // checker.go:25497
    pub(crate) fn create_widening_type(&mut self, non_widening_type: P<Type>) -> P<Type> {
        if self.strict_null_checks {
            return non_widening_type;
        }
        let t = self.new_intrinsic_type(
            non_widening_type.flags(),
            non_widening_type.as_intrinsic_type().intrinsic_name(),
        );
        t.object_flags.set(t.object_flags_lazy() | ObjectFlags::ContainsWideningType);
        t
    }

    // checker.go:25506
    pub(crate) fn create_unknown_union_type(&mut self) -> P<Type> {
        if self.strict_null_checks {
            return self.get_union_type(&[
                self.undefined_type,
                self.null_type,
                self.unknown_empty_object_type,
            ]);
        }
        self.unknown_type
    }

    // checker.go:25513
    pub(crate) fn new_literal_type(
        &mut self,
        flags: TypeFlags,
        value: Option<LiteralValue>,
        regular_type: Option<P<Type>>,
    ) -> P<Type> {
        let data = LiteralType::default();
        data.value.set(value);
        let t = self.new_type(flags, ObjectFlags::None, data);
        let data = t.as_literal_type();
        if regular_type.is_some() {
            data.regular_type.set(regular_type);
        } else {
            data.regular_type.set(Some(t));
        }
        t
    }

    // checker.go:25525
    pub(crate) fn new_unique_es_symbol_type(&mut self, symbol: P<Symbol>, name: &str) -> P<Type> {
        let data = UniqueESSymbolType::default();
        data.name.set(alloc_str(name));
        let t = self.new_type(TypeFlags::UniqueESSymbol, ObjectFlags::None, data);
        t.set_symbol(Some(symbol));
        t
    }

    // checker.go:25533
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn new_object_type(
        &mut self,
        object_flags: ObjectFlags,
        symbol: Option<P<Symbol>>,
    ) -> P<Type> {
        let t = if object_flags.intersects(ObjectFlags::ClassOrInterface) {
            self.new_type(TypeFlags::Object, object_flags, InterfaceType::default())
        } else if object_flags.intersects(ObjectFlags::Tuple) {
            self.new_type(TypeFlags::Object, object_flags, TupleType::default())
        } else if object_flags.intersects(ObjectFlags::Reference) {
            self.new_type(TypeFlags::Object, object_flags, TypeReference::default())
        } else if object_flags.intersects(ObjectFlags::Mapped) {
            self.new_type(TypeFlags::Object, object_flags, MappedType::default())
        } else if object_flags.intersects(ObjectFlags::ReverseMapped) {
            self.new_type(TypeFlags::Object, object_flags, ReverseMappedType::default())
        } else if object_flags.intersects(ObjectFlags::EvolvingArray) {
            self.new_type(TypeFlags::Object, object_flags, EvolvingArrayType::default())
        } else if object_flags.intersects(ObjectFlags::InstantiationExpressionType) {
            self.new_type(TypeFlags::Object, object_flags, InstantiationExpressionType::default())
        } else if object_flags.intersects(ObjectFlags::Anonymous) {
            self.new_type(TypeFlags::Object, object_flags, ObjectType::default())
        } else {
            panic!("Unhandled case in newObjectType")
        };
        t.set_symbol(symbol);
        t
    }

    // checker.go:25560
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn new_anonymous_type(
        &mut self,
        symbol: Option<P<Symbol>>,
        members: Option<P<SymbolTable>>,
        call_signatures: &[P<Signature>],
        construct_signatures: &[P<Signature>],
        index_infos: &[P<IndexInfo>],
    ) -> P<Type> {
        let t = self.new_object_type(ObjectFlags::Anonymous, symbol);
        self.set_structured_type_members(
            t,
            members,
            call_signatures,
            construct_signatures,
            index_infos,
        );
        t
    }

    // checker.go:25566
    pub(crate) fn try_create_type_reference(
        &mut self,
        target: P<Type>,
        type_arguments: &[P<Type>],
    ) -> P<Type> {
        if !type_arguments.is_empty() && target == self.empty_generic_type {
            return self.unknown_type;
        }
        self.create_type_reference(target, type_arguments)
    }

    // checker.go:25573
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn create_type_reference(
        &mut self,
        target: P<Type>,
        type_arguments: &[P<Type>],
    ) -> P<Type> {
        self.create_type_reference_ex(target, type_arguments, ObjectFlags::None)
    }

    // checker.go:25577
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn create_type_reference_ex(
        &mut self,
        target: P<Type>,
        type_arguments: &[P<Type>],
        object_flags: ObjectFlags,
    ) -> P<Type> {
        let intf = target.as_interface_type();
        if let Some(t) = intf.instantiations.get(type_arguments) {
            return t;
        }
        let propagating_flags =
            Self::get_propagating_flags_of_types(type_arguments, TypeFlags::None);
        let t = self.new_object_type(
            ObjectFlags::Reference | object_flags | propagating_flags,
            target.symbol(),
        );
        let d = t.as_type_reference();
        d.target.set(Some(target));
        d.resolved_type_arguments.set(Some(alloc_slice(type_arguments)));
        intf.instantiations.add(t);
        t
    }

    // checker.go:25591
    pub(crate) fn create_deferred_type_reference(
        &mut self,
        target: P<Type>,
        node: P<Node>,
        mapper: Option<P<TypeMapper>>,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        let mut alias = alias;
        if alias.is_none() {
            alias = self.get_alias_for_type_node(node);
            if let Some(a) = alias
                && mapper.is_some()
            {
                let type_arguments = self.instantiate_types(a.type_arguments.get(), mapper);
                a.type_arguments.set(alloc_vec(type_arguments));
            }
        }
        let t = self.new_object_type(ObjectFlags::Reference, target.symbol());
        t.set_alias(alias);
        let d = t.as_type_reference();
        d.target.set(Some(target));
        d.mapper.set(mapper);
        d.node.set(Some(node));
        t
    }

    // checker.go:25607
    pub(crate) fn clone_type_reference(&mut self, source: P<Type>) -> P<Type> {
        let t = self.new_object_type(ObjectFlags::Reference, source.symbol());
        t.object_flags.set(
            source.object_flags_lazy()
                & !(ObjectFlags::MembersResolved | ObjectFlags::WeakTypeMemo),
        );
        t.as_type_reference().target.set(source.as_type_reference().target.get());
        t.as_type_reference()
            .resolved_type_arguments
            .set(source.as_type_reference().resolved_type_arguments.get());
        t
    }

    // checker.go:25615
    pub(crate) fn set_structured_type_members(
        &mut self,
        t: P<Type>,
        members: Option<P<SymbolTable>>,
        call_signatures: &[P<Signature>],
        construct_signatures: &[P<Signature>],
        index_infos: &[P<IndexInfo>],
    ) {
        // A class or interface sets its own members, then again with the inherited ones: drop what isWeakType
        // memoized in between. The memo lives on object types only; a union keeps `NonNullableUnion` and an
        // intersection its `ShouldNormalize` memo in the same bits.
        let mut object_flags = t.object_flags_lazy() | ObjectFlags::MembersResolved;
        if t.flags().intersects(TypeFlags::Object) {
            object_flags &= !ObjectFlags::WeakTypeMemo;
        }
        t.object_flags.set(object_flags);
        let data = t.as_structured_type();
        data.set_members(members);
        let properties = self.get_named_members(members, t.symbol());
        data.set_properties(alloc_vec(properties));
        if call_signatures.is_empty() {
            if construct_signatures.is_empty() {
                data.set_signatures(&[]);
            } else {
                data.set_signatures(alloc_slice(construct_signatures));
            }
            data.set_call_signature_count(0);
        } else {
            if construct_signatures.is_empty() {
                data.set_signatures(alloc_slice(call_signatures));
            } else {
                data.set_signatures(alloc_vec([call_signatures, construct_signatures].concat()));
            }
            data.set_call_signature_count(call_signatures.len());
        }
        data.set_index_infos(alloc_slice(index_infos));
    }

    // checker.go:25638
    pub(crate) fn new_type_parameter(&mut self, symbol: Option<P<Symbol>>) -> P<Type> {
        let t =
            self.new_type(TypeFlags::TypeParameter, ObjectFlags::None, TypeParameter::default());
        t.set_symbol(symbol);
        t
    }

    // This function is used to propagate certain flags when creating new object type references and union types.
    // It is only necessary to do so if a constituent type might be the undefined type, the null type, the type
    // of an object literal or a non-inferrable type. This is because there are operations in the type checker
    // that care about the presence of such types at arbitrary depth in a containing type.
    // checker.go:25648
    pub(crate) fn get_propagating_flags_of_types(
        types: &[P<Type>],
        exclude_kinds: TypeFlags,
    ) -> ObjectFlags {
        let mut result = ObjectFlags::None;
        result = types.iter().fold(result, |mut result, &t| {
            if !t.flags().intersects(exclude_kinds) {
                result |= t.object_flags();
            }

            result
        });
        result & ObjectFlags::PropagatingFlags
    }

    // checker.go:25658
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn new_union_type(
        &mut self,
        object_flags: ObjectFlags,
        types: &[P<Type>],
    ) -> P<Type> {
        let data = UnionType::default();
        data.types.set(alloc_slice(types));
        self.new_type(TypeFlags::Union, object_flags, data)
    }

    // checker.go:25664
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn new_intersection_type(
        &mut self,
        object_flags: ObjectFlags,
        types: &[P<Type>],
    ) -> P<Type> {
        let data = IntersectionType::default();
        data.types.set(alloc_slice(types));
        self.new_type(TypeFlags::Intersection, object_flags, data)
    }

    // checker.go:25670
    pub(crate) fn new_indexed_access_type(
        &mut self,
        object_type: P<Type>,
        index_type: P<Type>,
        access_flags: AccessFlags,
    ) -> P<Type> {
        let data = IndexedAccessType::default();
        data.object_type.set(Some(object_type));
        data.index_type.set(Some(index_type));
        data.access_flags.set(access_flags);
        self.new_type(TypeFlags::IndexedAccess, ObjectFlags::None, data)
    }

    // checker.go:25678
    pub(crate) fn new_index_type(&mut self, target: P<Type>, index_flags: IndexFlags) -> P<Type> {
        let data = IndexType::default();
        data.target.set(Some(target));
        data.index_flags.set(index_flags);
        self.new_type(TypeFlags::Index, ObjectFlags::None, data)
    }

    // checker.go:25685
    pub(crate) fn new_template_literal_type(
        &mut self,
        texts: &[&str],
        types: &[P<Type>],
    ) -> P<Type> {
        let data = TemplateLiteralType::default();
        data.texts.set(alloc_vec(texts.iter().map(|s| alloc_str(s)).collect()));
        data.types.set(alloc_slice(types));
        self.new_type(TypeFlags::TemplateLiteral, ObjectFlags::None, data)
    }

    // checker.go:25692
    pub(crate) fn new_string_mapping_type(
        &mut self,
        symbol: P<Symbol>,
        target: P<Type>,
    ) -> P<Type> {
        let data = StringMappingType::default();
        data.target.set(Some(target));
        let t = self.new_type(TypeFlags::StringMapping, ObjectFlags::None, data);
        t.set_symbol(Some(symbol));
        t
    }

    // checker.go:25700
    fn new_conditional_type(
        &mut self,
        root: P<ConditionalRoot>,
        mapper: Option<P<TypeMapper>>,
        combined_mapper: Option<P<TypeMapper>>,
    ) -> P<Type> {
        let data = ConditionalType::default();
        data.root.set(Some(root));
        data.check_type.set(Some(self.instantiate_type(root.check_type.get().unwrap(), mapper)));
        data.extends_type
            .set(Some(self.instantiate_type(root.extends_type.get().unwrap(), mapper)));
        data.mapper.set(mapper);
        data.combined_mapper.set(combined_mapper);
        self.new_type(TypeFlags::Conditional, ObjectFlags::None, data)
    }

    // checker.go:25710
    pub(crate) fn new_substitution_type(
        &mut self,
        base_type: P<Type>,
        constraint: P<Type>,
    ) -> P<Type> {
        let data = SubstitutionType::default();
        data.base_type.set(Some(base_type));
        data.constraint.set(Some(constraint));
        self.new_type(TypeFlags::Substitution, ObjectFlags::None, data)
    }

    // checker.go:25717
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn new_signature(
        &mut self,
        flags: SignatureFlags,
        declaration: Option<P<Node>>,
        type_parameters: &[P<Type>],
        this_parameter: Option<P<Symbol>>,
        parameters: &[P<Symbol>],
        resolved_return_type: Option<P<Type>>,
        resolved_type_predicate: Option<P<TypePredicate>>,
        min_argument_count: usize,
    ) -> P<Signature> {
        self.signature_count += 1;
        crate::sitecount::hit("signature", "");
        let sig = P::new(Signature::default());
        sig.id.set(SignatureId(self.signature_count));
        sig.flags.set(flags);
        sig.declaration.set(declaration);
        sig.set_type_parameters(
            (!type_parameters.is_empty()).then(|| alloc_slice(type_parameters)),
        );
        sig.parameters.set(alloc_slice(parameters));
        sig.set_this_parameter(this_parameter);
        sig.resolved_return_type.set(resolved_return_type);
        sig.set_resolved_type_predicate(resolved_type_predicate, self.no_type_predicate);
        sig.min_argument_count.set(min_argument_count);
        sig.resolved_min_argument_count.set(None);
        sig
    }

    // checker.go:25733
    pub(crate) fn new_index_info(
        key_type: P<Type>,
        value_type: P<Type>,
        is_readonly: bool,
        declaration: Option<P<Node>>,
        components: &[P<Node>],
    ) -> P<IndexInfo> {
        let info = P::new(IndexInfo::default());
        info.key_type.set(Some(key_type));
        info.value_type.set(Some(value_type));
        info.is_readonly.set(is_readonly);
        info.declaration.set(declaration);
        info.components.set(alloc_slice(components));
        info
    }

    // checker.go:25743
    pub(crate) fn get_regular_type_of_literal_type(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Freshable) {
            return t.as_literal_type().regular_type.get().unwrap();
        }
        if t.flags().intersects(TypeFlags::Union) {
            let u = t.as_union_type();
            if u.regular_type().is_none() {
                u.set_regular_type(
                    self.map_type(t, |c, t| Some(c.get_regular_type_of_literal_type(t))),
                );
            }
            return u.regular_type().unwrap();
        }
        t
    }

    // checker.go:25757
    pub(crate) fn get_fresh_type_of_literal_type(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Freshable) {
            let d = t.as_literal_type();
            if d.fresh_type.get().is_none() {
                let f = self.new_literal_type(t.flags(), d.value.get(), Some(t));
                f.set_symbol(t.symbol());
                f.as_literal_type().fresh_type.set(Some(f));
                d.fresh_type.set(Some(f));
            }
            return d.fresh_type.get().unwrap();
        }
        t
    }
}

// checker.go:25771
pub(crate) fn is_fresh_literal_type(t: P<Type>) -> bool {
    t.flags().intersects(TypeFlags::Freshable) && t.as_literal_type().fresh_type.get() == Some(t)
}

impl Checker {
    // checker.go:25775
    pub(crate) fn get_string_literal_type(&mut self, value: &str) -> P<Type> {
        let mut t = self.string_literal_types.get(value);
        if t.is_none() {
            let literal = self.new_literal_type(
                TypeFlags::StringLiteral,
                Some(LiteralValue::String(alloc_str(value))),
                None,
            );
            self.string_literal_types.insert_new(literal);
            t = Some(literal);
        }
        t.unwrap()
    }

    // checker.go:25784
    pub(crate) fn get_number_literal_type(&mut self, value: Number) -> P<Type> {
        // NaN cannot be used as a Go map key because NaN != NaN in IEEE 754,
        // so Go map lookups for NaN always miss. Cache NaN type separately.
        if value.is_nan() {
            if self.nan_type.is_none() {
                self.nan_type = Some(self.new_literal_type(
                    TypeFlags::NumberLiteral,
                    Some(LiteralValue::Number(value)),
                    None,
                ));
            }
            return self.nan_type.unwrap();
        }
        let mut t = self.number_literal_types.get(&value).copied();
        if t.is_none() {
            let literal = self.new_literal_type(
                TypeFlags::NumberLiteral,
                Some(LiteralValue::Number(value)),
                None,
            );
            self.number_literal_types.insert(value, literal);
            t = Some(literal);
        }
        t.unwrap()
    }

    // checker.go:25801
    pub(crate) fn get_big_int_literal_type(&mut self, value: PseudoBigInt) -> P<Type> {
        let mut t = self.bigint_literal_types.get(&value).copied();
        if t.is_none() {
            let literal = self.new_literal_type(
                TypeFlags::BigIntLiteral,
                Some(LiteralValue::BigInt(value)),
                None,
            );
            self.bigint_literal_types.insert(value, literal);
            t = Some(literal);
        }
        t.unwrap()
    }

    // text is a valid bigint string excluding a trailing `n`, but including a possible prefix `-`.
    // Use `isValidBigIntString(text, roundTripOnly)` before calling this function.
    // checker.go:25812
    pub(crate) fn parse_big_int_literal_type(&mut self, text: &str) -> P<Type> {
        self.get_big_int_literal_type(jsnum::parse_valid_big_int(text))
    }
}

// checker.go:25816
pub(crate) fn get_string_literal_value(t: P<Type>) -> &'static str {
    match t.as_literal_type().value.get() {
        Some(LiteralValue::String(s)) => s,
        _ => panic!("getStringLiteralValue: not a string literal"),
    }
}

// checker.go:25820
pub(crate) fn get_number_literal_value(t: P<Type>) -> Number {
    match t.as_literal_type().value.get() {
        Some(LiteralValue::Number(n)) => n,
        _ => panic!("getNumberLiteralValue: not a number literal"),
    }
}

// checker.go:25824
pub(crate) fn get_big_int_literal_value(t: P<Type>) -> PseudoBigInt {
    match t.as_literal_type().value.get() {
        Some(LiteralValue::BigInt(v)) => v,
        _ => panic!("getBigIntLiteralValue: not a bigint literal"),
    }
}

// checker.go:25828
pub(crate) fn get_boolean_literal_value(t: P<Type>) -> bool {
    match t.as_literal_type().value.get() {
        Some(LiteralValue::Boolean(b)) => b,
        _ => panic!("getBooleanLiteralValue: not a boolean literal"),
    }
}

impl Checker {
    // checker.go:25832
    fn get_enum_literal_type(
        &mut self,
        value: LiteralValue,
        enum_symbol: P<Symbol>,
        symbol: P<Symbol>,
    ) -> P<Type> {
        let flags = match value {
            LiteralValue::String(_) => TypeFlags::EnumLiteral | TypeFlags::StringLiteral,
            LiteralValue::Number(v) => {
                let flags = TypeFlags::EnumLiteral | TypeFlags::NumberLiteral;
                // NaN cannot be used as a Go map key because NaN != NaN in IEEE 754,
                // so Go map lookups for NaN always miss. Cache NaN enum types separately by enum symbol.
                if v.is_nan() {
                    let mut t = self.enum_nan_literal_types.get(&enum_symbol).copied();
                    if t.is_none() {
                        let literal = self.new_literal_type(flags, Some(value), None);
                        literal.set_symbol(Some(symbol));
                        self.enum_nan_literal_types.insert(enum_symbol, literal);
                        t = Some(literal);
                    }
                    return t.unwrap();
                }
                flags
            }
            _ => panic!("Unhandled case in getEnumLiteralType"),
        };
        let key = EnumLiteralKey { enum_symbol, value };
        let mut t = self.enum_literal_types.get(&key).copied();
        if t.is_none() {
            let literal = self.new_literal_type(flags, Some(value), None);
            literal.set_symbol(Some(symbol));
            self.enum_literal_types.insert(key, literal);
            t = Some(literal);
        }
        t.unwrap()
    }
}

// checker.go:25863
pub(crate) fn is_literal_type(t: P<Type>) -> bool {
    if t.flags().intersects(TypeFlags::Boolean) {
        return true;
    }
    if t.flags().intersects(TypeFlags::Union) {
        if t.flags().intersects(TypeFlags::EnumLiteral) {
            return true;
        }
        return t.types().iter().all(|&t| is_unit_type(t));
    }
    is_unit_type(t)
}

// checker.go:25876
pub(crate) fn is_neither_unit_type_nor_never(t: P<Type>) -> bool {
    !t.flags().intersects(TypeFlags::Unit | TypeFlags::Never)
}

// checker.go:25880
pub(crate) fn is_unit_type(t: P<Type>) -> bool {
    t.flags().intersects(TypeFlags::Unit)
}

impl Checker {
    // checker.go:25884
    pub(crate) fn is_unit_like_type(&mut self, t: P<Type>) -> bool {
        // Intersections that reduce to 'never' (e.g. 'T & null' where 'T extends {}') are not unit types.
        let t = self.get_base_constraint_or_type(t);
        // Scan intersections such that tagged literal types are considered unit types.
        if t.flags().intersects(TypeFlags::Intersection) {
            return t.as_intersection_type().types.get().iter().any(|&t| is_unit_type(t));
        }
        is_unit_type(t)
    }

    // checker.go:25894
    pub(crate) fn extract_unit_type(t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Intersection) {
            let u = t.as_intersection_type().types.get().iter().copied().find(|&t| is_unit_type(t));
            if let Some(u) = u {
                return u;
            }
        }
        t
    }

    // checker.go:25904
    pub(crate) fn get_base_type_of_literal_type(&mut self, t: P<Type>) -> P<Type> {
        let flags = t.flags();
        if flags.intersects(TypeFlags::EnumLike) {
            return self.get_base_type_of_enum_like_type(t);
        } else if flags.intersects(
            TypeFlags::StringLiteral | TypeFlags::TemplateLiteral | TypeFlags::StringMapping,
        ) {
            return self.string_type;
        } else if flags.intersects(TypeFlags::NumberLiteral) {
            return self.number_type;
        } else if flags.intersects(TypeFlags::BigIntLiteral) {
            return self.bigint_type;
        } else if flags.intersects(TypeFlags::BooleanLiteral) {
            return self.boolean_type;
        } else if flags.intersects(TypeFlags::Union) {
            return self.get_base_type_of_literal_type_union(t);
        }
        t
    }

    // This like getBaseTypeOfLiteralType, but instead treats enum literals as strings/numbers instead
    // of returning their enum base type (which depends on the types of other literals in the enum).
    // checker.go:25924
    pub(crate) fn get_base_type_of_literal_type_for_comparison(&mut self, t: P<Type>) -> P<Type> {
        let flags = t.flags();
        if flags.intersects(
            TypeFlags::StringLiteral | TypeFlags::TemplateLiteral | TypeFlags::StringMapping,
        ) {
            return self.string_type;
        } else if flags.intersects(TypeFlags::NumberLiteral | TypeFlags::Enum) {
            return self.number_type;
        } else if flags.intersects(TypeFlags::BigIntLiteral) {
            return self.bigint_type;
        } else if flags.intersects(TypeFlags::BooleanLiteral) {
            return self.boolean_type;
        } else if flags.intersects(TypeFlags::Union) {
            return self
                .map_type(t, |c, t| Some(c.get_base_type_of_literal_type_for_comparison(t)))
                .unwrap();
        }
        t
    }

    // checker.go:25940
    pub(crate) fn get_base_type_of_enum_like_type(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::EnumLike)
            && t.symbol().unwrap().flags().intersects(SymbolFlags::EnumMember)
        {
            let parent = self.get_parent_of_symbol(t.symbol().unwrap()).unwrap();
            return self.get_declared_type_of_symbol(parent);
        }
        t
    }

    // checker.go:25947
    fn get_base_type_of_literal_type_union(&mut self, t: P<Type>) -> P<Type> {
        let key = CachedTypeKey { kind: CachedTypeKind::LiteralUnionBaseType, type_id: t.id };
        if let Some(&cached) = self.cached_types.get(&key) {
            return cached;
        }
        let result = self.map_type(t, |c, t| Some(c.get_base_type_of_literal_type(t))).unwrap();
        self.cached_types.insert(key, result);
        result
    }

    // checker.go:25957
    pub(crate) fn get_widened_literal_type(&mut self, t: P<Type>) -> P<Type> {
        let flags = t.flags();
        if flags.intersects(TypeFlags::EnumLike) && is_fresh_literal_type(t) {
            return self.get_base_type_of_enum_like_type(t);
        } else if flags.intersects(TypeFlags::StringLiteral) && is_fresh_literal_type(t) {
            return self.string_type;
        } else if flags.intersects(TypeFlags::NumberLiteral) && is_fresh_literal_type(t) {
            return self.number_type;
        } else if flags.intersects(TypeFlags::BigIntLiteral) && is_fresh_literal_type(t) {
            return self.bigint_type;
        } else if flags.intersects(TypeFlags::BooleanLiteral) && is_fresh_literal_type(t) {
            return self.boolean_type;
        } else if flags.intersects(TypeFlags::Union) {
            return self.map_type(t, |c, t| Some(c.get_widened_literal_type(t))).unwrap();
        }
        t
    }

    // checker.go:25975
    fn get_widened_unique_es_symbol_type(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::UniqueESSymbol) {
            return self.es_symbol_type;
        } else if t.flags().intersects(TypeFlags::Union) {
            return self.map_type(t, |c, t| Some(c.get_widened_unique_es_symbol_type(t))).unwrap();
        }
        t
    }

    // checker.go:25985
    pub(crate) fn get_widened_literal_like_type_for_contextual_type(
        &mut self,
        t: P<Type>,
        contextual_type: Option<P<Type>>,
    ) -> P<Type> {
        let mut t = t;
        if !self.is_literal_of_contextual_type(t, contextual_type) {
            let widened = self.get_widened_literal_type(t);
            t = self.get_widened_unique_es_symbol_type(widened);
        }
        self.get_regular_type_of_literal_type(t)
    }

    // checker.go:25992
    pub(crate) fn is_literal_of_contextual_type(
        &mut self,
        candidate_type: P<Type>,
        contextual_type: Option<P<Type>>,
    ) -> bool {
        if let Some(contextual_type) = contextual_type {
            if contextual_type.flags().intersects(TypeFlags::UnionOrIntersection) {
                return contextual_type
                    .types()
                    .iter()
                    .any(|&t| self.is_literal_of_contextual_type(candidate_type, Some(t)));
            }
            if contextual_type.flags().intersects(TypeFlags::InstantiableNonPrimitive) {
                // If the contextual type is a type variable constrained to a primitive type, consider
                // this a literal context for literals of that primitive type. For example, given a
                // type parameter 'T extends string', infer string literal types for T.
                let constraint =
                    self.get_base_constraint_of_type(contextual_type).unwrap_or(self.unknown_type);
                return self.maybe_type_of_kind(constraint, TypeFlags::String)
                    && self.maybe_type_of_kind(candidate_type, TypeFlags::StringLiteral)
                    || self.maybe_type_of_kind(constraint, TypeFlags::Number)
                        && self.maybe_type_of_kind(candidate_type, TypeFlags::NumberLiteral)
                    || self.maybe_type_of_kind(constraint, TypeFlags::BigInt)
                        && self.maybe_type_of_kind(candidate_type, TypeFlags::BigIntLiteral)
                    || self.maybe_type_of_kind(constraint, TypeFlags::ESSymbol)
                        && self.maybe_type_of_kind(candidate_type, TypeFlags::UniqueESSymbol)
                    || self.is_literal_of_contextual_type(candidate_type, Some(constraint));
            }
            // If the contextual type is a literal of a particular primitive type, we consider this a
            // literal context for all literals of that primitive type.
            let flags = contextual_type.flags();
            return flags.intersects(
                TypeFlags::StringLiteral
                    | TypeFlags::Index
                    | TypeFlags::TemplateLiteral
                    | TypeFlags::StringMapping,
            ) && self.maybe_type_of_kind(candidate_type, TypeFlags::StringLiteral)
                || flags.intersects(TypeFlags::NumberLiteral)
                    && self.maybe_type_of_kind(candidate_type, TypeFlags::NumberLiteral)
                || flags.intersects(TypeFlags::BigIntLiteral)
                    && self.maybe_type_of_kind(candidate_type, TypeFlags::BigIntLiteral)
                || flags.intersects(TypeFlags::BooleanLiteral)
                    && self.maybe_type_of_kind(candidate_type, TypeFlags::BooleanLiteral)
                || flags.intersects(TypeFlags::UniqueESSymbol)
                    && self.maybe_type_of_kind(candidate_type, TypeFlags::UniqueESSymbol);
        }
        false
    }

    // checker.go:26024
    pub(crate) fn map_type_with_alias(
        &mut self,
        t: P<Type>,
        f: impl FnMut(&mut Checker, P<Type>) -> P<Type>,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        let mut f = f;
        if t.flags().intersects(TypeFlags::Union) && alias.is_some() {
            let types: Vec<P<Type>> = t.types().iter().map(|&t| f(self, t)).collect();
            return self.get_union_type_ex(&types, UnionReduction::Literal, alias.into(), None);
        }
        self.map_type(t, |c, t| Some(f(c, t))).unwrap()
    }

    // checker.go:26031
    pub(crate) fn map_type(
        &mut self,
        t: P<Type>,
        f: impl FnMut(&mut Checker, P<Type>) -> Option<P<Type>>,
    ) -> Option<P<Type>> {
        self.map_type_ex(t, f, false /*noReductions*/)
    }

    // checker.go:26035
    pub(crate) fn map_type_ex(
        &mut self,
        t: P<Type>,
        f: impl FnMut(&mut Checker, P<Type>) -> Option<P<Type>>,
        no_reductions: bool,
    ) -> Option<P<Type>> {
        let mut f = f;
        self.map_type_ex_worker(t, &mut f, no_reductions)
    }

    // Body of mapTypeEx; recursion goes through a `dyn` callback to avoid unbounded generic instantiation.
    fn map_type_ex_worker(
        &mut self,
        t: P<Type>,
        f: &mut dyn FnMut(&mut Checker, P<Type>) -> Option<P<Type>>,
        no_reductions: bool,
    ) -> Option<P<Type>> {
        if t.flags().intersects(TypeFlags::Never) {
            return Some(t);
        }
        if !t.flags().intersects(TypeFlags::Union) {
            return f(self, t);
        }
        let u = t.as_union_type();
        let mut types = u.types.get();
        if let Some(origin) = u.origin()
            && origin.flags().intersects(TypeFlags::Union)
        {
            types = origin.types();
        }
        let mut mapped_types = self.free_type_lists.pop().unwrap_or_default();
        let mut changed = false;
        mapped_types.extend(types.iter().filter_map(|&s| {
            let mapped = if s.flags().intersects(TypeFlags::Union) {
                self.map_type_ex_worker(s, f, no_reductions)
            } else {
                f(self, s)
            };
            if mapped != Some(s) {
                changed = true;
            }
            mapped
        }));
        let result = if !changed {
            Some(t)
        } else if mapped_types.is_empty() {
            None
        } else {
            Some(self.get_union_type_ex(
                &mapped_types,
                if no_reductions { UnionReduction::None } else { UnionReduction::Literal },
                AliasArg::None, /*alias*/
                None,           /*origin*/
            ))
        };
        self.free_type_list(mapped_types);
        result
    }

    // checker.go:26080
    pub(crate) fn get_union_or_intersection_type(
        &mut self,
        types: &[P<Type>],
        is_union: bool,
        union_reduction: UnionReduction,
    ) -> P<Type> {
        if is_union {
            return self.get_union_type_ex(types, union_reduction, AliasArg::None, None);
        }
        self.get_intersection_type(types)
    }

    // checker.go:26087
    pub(crate) fn get_union_type(&mut self, types: &[P<Type>]) -> P<Type> {
        self.get_union_type_ex(
            types,
            UnionReduction::Literal,
            AliasArg::None, /*alias*/
            None,           /*origin*/
        )
    }

    // We sort and deduplicate the constituent types based on object identity. If the subtypeReduction
    // flag is specified we also reduce the constituent type set to only include types that aren't subtypes
    // of other types. Subtype reduction is expensive for large union types and is possible only when union
    // types are known not to circularly reference themselves (as is the case with union types created by
    // expression constructs such as array literals and the || and ?: operators). Named types can
    // circularly reference themselves and therefore cannot be subtype reduced during their declaration.
    // For example, "type Item = string | (() => Item" is a named type that circularly references itself.
    // checker.go:26098
    pub(crate) fn get_union_type_ex(
        &mut self,
        types: &[P<Type>],
        union_reduction: UnionReduction,
        alias: AliasArg<'_>,
        origin: Option<P<Type>>,
    ) -> P<Type> {
        if types.is_empty() {
            return self.never_type;
        }
        if types.len() == 1 {
            return types[0];
        }
        // tsrs: a front cache for small unions without an origin (unioncache.rs).
        if self.union_front_cache.mode != crate::unioncache::UnionCacheMode::Off {
            if origin.is_none() {
                return self.get_union_type_front_cached(types, union_reduction, alias);
            }
            crate::unioncache::count_bypass_origin(&self.union_front_cache);
        }
        self.get_union_type_ex_uncached(types, union_reduction, alias, origin)
    }

    /// `getUnionType` for two or more types, without the front cache.
    pub(crate) fn get_union_type_ex_uncached(
        &mut self,
        types: &[P<Type>],
        union_reduction: UnionReduction,
        alias: AliasArg<'_>,
        origin: Option<P<Type>>,
    ) -> P<Type> {
        // We optimize for the common case of unioning a union type with some other type (such as `undefined`).
        if types.len() == 2
            && origin.is_none()
            && (types[0].flags().intersects(TypeFlags::Union)
                || types[1].flags().intersects(TypeFlags::Union))
        {
            let mut id1 = types[0].id;
            let mut id2 = types[1].id;
            if id1 > id2 {
                std::mem::swap(&mut id1, &mut id2);
            }
            let key = UnionOfUnionKey { id1, id2, r: union_reduction, a: get_alias_key(alias) };
            let mut t = self.union_of_union_types.get(&key).copied();
            if t.is_none() {
                let too_complex_before = self.too_complex_reports;
                let result =
                    self.get_union_type_worker(types, union_reduction, alias, None /*origin*/);
                // tsrs-only: not cached when the reduction reported TS2590 (`too_complex_since`).
                if !self.too_complex_since(too_complex_before) {
                    self.union_of_union_types.insert(key, result);
                }
                t = Some(result);
            }
            return t.unwrap();
        }
        self.get_union_type_worker(types, union_reduction, alias, origin)
    }
}

/// Kind of a type for the creation-site counters (`crate::sitecount`).
pub(crate) fn type_kind_label(flags: TypeFlags, object_flags: ObjectFlags) -> &'static str {
    if flags.intersects(TypeFlags::Object) {
        if object_flags.intersects(ObjectFlags::ClassOrInterface) {
            "object:class/interface"
        } else if object_flags.intersects(ObjectFlags::Tuple) {
            "object:tuple"
        } else if object_flags.intersects(ObjectFlags::Reference) {
            "object:reference"
        } else if object_flags.intersects(ObjectFlags::Mapped) {
            "object:mapped"
        } else if object_flags.intersects(ObjectFlags::ReverseMapped) {
            "object:reverse-mapped"
        } else if object_flags.intersects(ObjectFlags::Instantiated) {
            "object:anonymous-instantiated"
        } else {
            "object:anonymous"
        }
    } else if flags.intersects(TypeFlags::Union) {
        "union"
    } else if flags.intersects(TypeFlags::Intersection) {
        "intersection"
    } else if flags.intersects(TypeFlags::Literal) {
        "literal"
    } else if flags.intersects(TypeFlags::TypeParameter) {
        "type-parameter"
    } else if flags.intersects(TypeFlags::IndexedAccess) {
        "indexed-access"
    } else if flags.intersects(TypeFlags::Conditional) {
        "conditional"
    } else if flags.intersects(TypeFlags::Substitution) {
        "substitution"
    } else if flags.intersects(TypeFlags::Index) {
        "index"
    } else if flags.intersects(TypeFlags::TemplateLiteral) {
        "template-literal"
    } else if flags.intersects(TypeFlags::StringMapping) {
        "string-mapping"
    } else {
        "other"
    }
}
