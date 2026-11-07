use crate::*;
use rustc_hash::FxHashMap;
use std::rc::Rc;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

// checker.go:19608
pub(crate) fn get_base_type_node_of_class(t: P<Type>) -> Option<P<Node>> {
    let decl = ast::get_class_like_declaration_of_symbol(t.symbol().unwrap());
    if let Some(decl) = decl {
        return ast::get_class_extends_heritage_element(decl);
    }
    None
}

impl Checker {
    // checker.go:19616
    pub(crate) fn get_instantiated_constructors_for_type_arguments(
        &mut self,
        t: P<Type>,
        type_argument_nodes: &[P<Node>],
        location: P<Node>,
    ) -> Vec<P<Signature>> {
        let signatures = self.get_constructors_for_type_arguments(t, type_argument_nodes, location);
        let type_arguments: Vec<P<Type>> =
            type_argument_nodes.iter().map(|n| self.get_type_from_type_node(*n)).collect();
        same_map(&signatures, |sig| {
            if !sig.type_parameters().is_empty() {
                return self.get_signature_instantiation(
                    *sig,
                    &type_arguments,
                    ast::is_in_js_file(location),
                    &[],
                );
            }
            *sig
        })
        .into_owned()
    }

    // checker.go:19627
    pub(crate) fn get_constructors_for_type_arguments(
        &mut self,
        t: P<Type>,
        type_argument_nodes: &[P<Node>],
        _location: P<Node>,
    ) -> Vec<P<Signature>> {
        let type_arg_count = type_argument_nodes.len();
        let signatures = self.get_signatures_of_type(t, SignatureKind::Construct);
        let mut result = Vec::new();
        result.extend(signatures.iter().filter_map(|&sig| {
            if type_arg_count >= Self::get_min_type_argument_count(sig.type_parameters())
                && type_arg_count <= sig.type_parameters().len()
            {
                Some(sig)
            } else {
                None
            }
        }));
        result
    }

    // checker.go:19634
    pub(crate) fn get_signature_instantiation(
        &mut self,
        sig: P<Signature>,
        type_arguments: &[P<Type>],
        is_java_script: bool,
        inferred_type_parameters: &[P<Type>],
    ) -> P<Signature> {
        let min_type_argument_count = Self::get_min_type_argument_count(sig.type_parameters());
        let filled = self.fill_missing_type_arguments(
            type_arguments,
            sig.type_parameters(),
            min_type_argument_count,
            is_java_script,
        );
        let instantiated_signature =
            self.get_signature_instantiation_without_filling_in_type_arguments(sig, &filled);
        if !inferred_type_parameters.is_empty() {
            let return_type = self.get_return_type_of_signature(instantiated_signature);
            let return_signature = self.get_single_call_or_construct_signature(return_type);
            if let Some(return_signature) = return_signature {
                let new_return_signature = self.clone_signature(return_signature);
                new_return_signature
                    .set_type_parameters(Some(alloc_slice(inferred_type_parameters)));
                let new_return_type = self.get_or_create_type_from_signature(new_return_signature);
                new_return_type.as_object_type().mapper.set(instantiated_signature.mapper.get());
                let new_instantiated_signature = self.clone_signature(instantiated_signature);
                new_instantiated_signature.resolved_return_type.set(Some(new_return_type));
                return new_instantiated_signature;
            }
        }
        instantiated_signature
    }

    // checker.go:19651
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn clone_signature(&mut self, sig: P<Signature>) -> P<Signature> {
        let result = self.new_signature(
            sig.flags.get() & SignatureFlags::PropagatingFlags,
            sig.declaration.get(),
            sig.type_parameters(),
            sig.this_parameter(),
            sig.parameters.get(),
            None,
            None,
            sig.min_argument_count.get(),
        );
        if sig.type_parameters_option().is_some_and(<[_]>::is_empty) {
            result.set_type_parameters(Some(&[]));
        }
        result.target.set(sig.target.get());
        result.mapper.set(sig.mapper.get());
        result.set_composite(sig.composite());
        result
    }

    // checker.go:19659
    pub(crate) fn get_signature_instantiation_without_filling_in_type_arguments(
        &mut self,
        sig: P<Signature>,
        type_arguments: &[P<Type>],
    ) -> P<Signature> {
        let key = CachedSignatureKey { sig, key: get_type_list_key(type_arguments) };
        let mut instantiation = self.cached_signatures.get(&key);
        if instantiation.is_none() {
            let created = self.create_signature_instantiation(sig, type_arguments);
            self.cached_signatures.insert(key, created);
            instantiation = Some(created);
        }
        instantiation.unwrap()
    }

    // checker.go:19669
    pub(crate) fn create_signature_instantiation(
        &mut self,
        sig: P<Signature>,
        type_arguments: &[P<Type>],
    ) -> P<Signature> {
        let m = self.create_signature_type_mapper(sig, type_arguments);
        self.instantiate_signature_ex(sig, m, true /*eraseTypeParameters*/)
    }

    // checker.go:19673
    fn create_signature_type_mapper(
        &mut self,
        sig: P<Signature>,
        type_arguments: &[P<Type>],
    ) -> P<TypeMapper> {
        let sources = self.get_type_parameters_for_mapper(sig);
        new_type_mapper(alloc_vec(sources), alloc_slice(type_arguments))
    }

    // checker.go:19677
    fn get_type_parameters_for_mapper(&mut self, sig: P<Signature>) -> Vec<P<Type>> {
        same_map(sig.type_parameters(), |tp| self.instantiate_type(*tp, tp.mapper())).into_owned()
    }

    // If type has a single call signature and no other members, return that signature. Otherwise, return nil.
    // checker.go:19682
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn get_single_call_signature(&mut self, t: P<Type>) -> Option<P<Signature>> {
        self.get_single_signature(t, SignatureKind::Call, false /*allowMembers*/)
    }

    // checker.go:19686
    pub(crate) fn get_single_call_or_construct_signature(
        &mut self,
        t: P<Type>,
    ) -> Option<P<Signature>> {
        let call_sig =
            self.get_single_signature(t, SignatureKind::Call, false /*allowMembers*/);
        if call_sig.is_some() {
            return call_sig;
        }
        self.get_single_signature(t, SignatureKind::Construct, false /*allowMembers*/)
    }

    // checker.go:19694
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn get_single_signature(
        &mut self,
        t: P<Type>,
        kind: SignatureKind,
        allow_members: bool,
    ) -> Option<P<Signature>> {
        if t.flags().intersects(TypeFlags::Object)
            && (allow_members
                || !self.has_properties_of_structured_type(t)
                    && self.index_infos_of_structured_type(t).is_empty())
        {
            let call_signatures = self.signatures_of_structured_type(t, SignatureKind::Call);
            let construct_signatures =
                self.signatures_of_structured_type(t, SignatureKind::Construct);
            if kind == SignatureKind::Call
                && call_signatures.len() == 1
                && construct_signatures.is_empty()
            {
                return Some(call_signatures[0]);
            }
            if kind == SignatureKind::Construct
                && construct_signatures.len() == 1
                && call_signatures.is_empty()
            {
                return Some(construct_signatures[0]);
            }
        }
        None
    }

    // checker.go:19709
    pub(crate) fn get_or_create_type_from_signature(&mut self, sig: P<Signature>) -> P<Type> {
        // There are two ways to declare a construct signature, one is by declaring a class constructor
        // using the constructor keyword, and the other is declaring a bare construct signature in an
        // object type literal or interface (using the new keyword). Each way of declaring a constructor
        // will result in a different declaration kind.
        if sig.isolated_signature_type().is_none() {
            let mut kind = Kind::Unknown;
            if let Some(declaration) = sig.declaration.get() {
                kind = declaration.kind();
            }
            // If declaration is undefined, it is likely to be the signature of the default constructor.
            let is_constructor = kind == Kind::Unknown
                || kind == Kind::Constructor
                || kind == Kind::ConstructSignature
                || kind == Kind::ConstructorType;

            let mut symbol = None;
            if let Some(declaration) = sig.declaration.get() {
                symbol = declaration.symbol();
            }
            let t = self
                .new_object_type(ObjectFlags::Anonymous | ObjectFlags::SingleSignatureType, symbol);
            if is_constructor {
                self.set_structured_type_members(t, None, &[], &[sig], &[]);
            } else {
                self.set_structured_type_members(t, None, &[sig], &[], &[]);
            }
            sig.set_isolated_signature_type(Some(t));
        }
        sig.isolated_signature_type().unwrap()
    }

    // checker.go:19737
    pub(crate) fn get_erased_signature(&mut self, signature: P<Signature>) -> P<Signature> {
        if signature.type_parameters().is_empty() {
            return signature;
        }
        let key = CachedSignatureKey { sig: signature, key: SIGNATURE_KEY_ERASED };
        let mut erased = self.cached_signatures.get(&key);
        if erased.is_none() {
            let m = new_array_to_single_type_mapper(signature.type_parameters(), self.any_type);
            let created =
                self.instantiate_signature_ex(signature, m, true /*eraseTypeParameters*/);
            self.cached_signatures.insert(key, created);
            erased = Some(created);
        }
        erased.unwrap()
    }

    // checker.go:19750
    pub(crate) fn get_canonical_signature(&mut self, signature: P<Signature>) -> P<Signature> {
        if signature.type_parameters().is_empty() {
            return signature;
        }
        let key = CachedSignatureKey { sig: signature, key: SIGNATURE_KEY_CANONICAL };
        let mut canonical = self.cached_signatures.get(&key);
        if canonical.is_none() {
            let created = self.create_canonical_signature(signature);
            self.cached_signatures.insert(key, created);
            canonical = Some(created);
        }
        canonical.unwrap()
    }

    // checker.go:19763
    fn create_canonical_signature(&mut self, signature: P<Signature>) -> P<Signature> {
        // Create an instantiation of the signature where each unconstrained type parameter is replaced with
        // its original. When a generic class or interface is instantiated, each generic method in the class or
        // interface is instantiated with a fresh set of cloned type parameters (which we need to handle scenarios
        // where different generations of the same type parameter are in scope). This leads to a lot of new type
        // identities, and potentially a lot of work comparing those identities, so here we create an instantiation
        // that uses the original type identities for all unconstrained type parameters.
        let mut type_arguments = Vec::with_capacity(signature.type_parameters().len());
        type_arguments.extend(signature.type_parameters().iter().map(|&tp| {
            if let Some(target) = tp.target()
                && self.get_constraint_of_type_parameter(target).is_none()
            {
                return target;
            }
            tp
        }));
        self.get_signature_instantiation(
            signature,
            &type_arguments,
            ast::is_in_js_file(signature.declaration.get()),
            &[], /*inferredTypeParameters*/
        )
    }

    // checker.go:19779
    pub(crate) fn get_base_signature(&mut self, signature: P<Signature>) -> P<Signature> {
        let type_parameters = signature.type_parameters();
        if type_parameters.is_empty() {
            return signature;
        }
        let key = CachedSignatureKey { sig: signature, key: SIGNATURE_KEY_BASE };
        if let Some(cached) = self.cached_signatures.get(&key) {
            return cached;
        }
        let mut constraints = Vec::with_capacity(type_parameters.len());
        constraints.extend(
            type_parameters
                .iter()
                .map(|&tp| self.get_constraint_of_type_parameter(tp).unwrap_or(self.unknown_type)),
        );
        let base_constraint_mapper = new_type_mapper(type_parameters, alloc_vec(constraints));
        let mut base_constraints: Vec<P<Type>> = type_parameters
            .iter()
            .map(|tp| self.instantiate_type(*tp, Some(base_constraint_mapper)))
            .collect();
        // Run the immediate constraint mapper N-1 times so non-circular interdependent type parameters
        // resolve to their external dependencies without adding an extra expansion step for self-recursive constraints.
        base_constraints =
            (0..type_parameters.len() - 1).fold(base_constraints, |mut base_constraints, _| {
                base_constraints =
                    self.instantiate_types(&base_constraints, Some(base_constraint_mapper));

                base_constraints
            });
        // and then apply a type eraser to remove any remaining circularly dependent type parameters
        base_constraints = self.instantiate_types(
            &base_constraints,
            Some(new_array_to_single_type_mapper(type_parameters, self.any_type)),
        );
        let result = self.instantiate_signature_ex(
            signature,
            new_type_mapper(type_parameters, alloc_vec(base_constraints)),
            true, /*eraseTypeParameters*/
        );
        self.cached_signatures.insert(key, result);
        result
    }

    // Instantiate a generic signature in the context of a non-generic signature (section 3.8.5 in TypeScript spec)
    // checker.go:19807
    pub(crate) fn instantiate_signature_in_context_of(
        &mut self,
        signature: P<Signature>,
        contextual_signature: P<Signature>,
        inference_context: Option<P<InferenceContext>>,
        compare_types: Option<TypeComparer>,
    ) -> P<Signature> {
        let type_parameters = self.get_type_parameters_for_mapper(signature);
        let context = Self::new_inference_context(
            &type_parameters,
            Some(signature),
            InferenceFlags::None,
            compare_types,
        );
        // We clone the inferenceContext to avoid fixing. For example, when the source signature is <T>(x: T) => T[] and
        // the contextual signature is (...args: A) => B, we want to infer the element type of A's constraint (say 'any')
        // for T but leave it possible to later infer '[any]' back to A.
        let rest_type = self.get_effective_rest_type(contextual_signature);
        let mut mapper = None;
        if let Some(inference_context) = inference_context {
            if rest_type.is_some_and(|r| r.flags().intersects(TypeFlags::TypeParameter)) {
                mapper = inference_context.non_fixing_mapper();
            } else {
                mapper = inference_context.mapper();
            }
        }
        let source_signature = if mapper.is_some() {
            self.instantiate_signature(contextual_signature, mapper)
        } else {
            contextual_signature
        };
        self.apply_to_parameter_types(source_signature, signature, |c, source, target| {
            // Type parameters from outer context referenced by source type are fixed by instantiation of the source type
            c.infer_types(context.inferences.get(), source, target, InferencePriority::NONE, false);
        });
        if inference_context.is_none() {
            self.apply_to_return_types(contextual_signature, signature, |c, source, target| {
                c.infer_types(
                    context.inferences.get(),
                    source,
                    target,
                    InferencePriority::RETURN_TYPE,
                    false,
                );
            });
        }
        let inferred = self.get_inferred_types(context);
        self.get_signature_instantiation(
            signature,
            &inferred,
            ast::is_in_js_file(contextual_signature.declaration.get()),
            &[], /*inferredTypeParameters*/
        )
    }

    // checker.go:19839
    pub(crate) fn resolve_base_types_of_interface(&mut self, t: P<Type>) {
        let data = t.as_interface_type();
        let declarations = t.symbol().unwrap().declarations();
        for &declaration in declarations {
            if ast::is_interface_declaration(declaration) {
                for &node in ast::get_extends_heritage_clause_elements(declaration) {
                    let type_from_node = self.get_type_from_type_node(node);
                    let base_type = self.get_reduced_type(type_from_node);
                    if !self.is_error_type(base_type) {
                        if self.is_valid_base_type(base_type) {
                            if t != base_type && !self.has_base_type(base_type, Some(t)) {
                                let mut resolved = data.resolved_base_types.get().to_vec();
                                resolved.push(base_type);
                                data.resolved_base_types.set(alloc_vec(resolved));
                            } else {
                                self.report_circular_base_type(declaration, t);
                            }
                        } else {
                            self.error(Some(node), &diagnostics::AN_INTERFACE_CAN_ONLY_EXTEND_AN_OBJECT_TYPE_OR_INTERSECTION_OF_OBJECT_TYPES_WITH_STATICALLY_KNOWN_MEMBERS, &[]);
                        }
                    }
                }
            }
        }
    }

    // checker.go:19861
    pub(crate) fn are_all_outer_type_parameters_applied(&mut self, t: P<Type>) -> bool {
        // An unapplied type parameter has its symbol still the same as the matching argument symbol.
        // Since parameters are applied outer-to-inner, only the last outer parameter needs to be checked.
        let outer_type_parameters = t.as_interface_type().outer_type_parameters();
        if !outer_type_parameters.is_empty() {
            let last = outer_type_parameters.len() - 1;
            let type_arguments = self.get_type_arguments(t);
            return outer_type_parameters[last].symbol() != type_arguments[last].symbol();
        }
        true
    }

    // checker.go:19873
    pub(crate) fn report_circular_base_type(&mut self, node: P<Node>, t: P<Type>) {
        let s = self.type_to_string_ex(t, None, TypeFormatFlags::WriteArrayAsGenericType, None);
        self.error(
            Some(node),
            &diagnostics::TYPE_0_RECURSIVELY_REFERENCES_ITSELF_AS_A_BASE_TYPE,
            &[&s],
        );
    }

    // A valid base type is `any`, an object type or intersection of object types.
    // checker.go:19878
    pub(crate) fn is_valid_base_type(&mut self, t: P<Type>) -> bool {
        if t.flags().intersects(TypeFlags::TypeParameter) {
            let constraint = self.get_base_constraint_of_type(t);
            if let Some(constraint) = constraint {
                return self.is_valid_base_type(constraint);
            }
        }
        // TODO: Given that we allow type parameters here now, is this `!isGenericMappedType(type)` check really needed?
        // There's no reason a `T` should be allowed while a `Readonly<T>` should not.
        t.flags().intersects(TypeFlags::Object | TypeFlags::NonPrimitive | TypeFlags::Any)
            && !self.is_generic_mapped_type(t)
            || t.flags().intersects(TypeFlags::Intersection)
                && t.types().iter().all(|t| self.is_valid_base_type(*t))
    }

    // TODO: GH#18217 If `checkBase` is undefined, we should not call this because this will always return false.
    // checker.go:19892
    pub(crate) fn has_base_type(&mut self, t: P<Type>, check_base: Option<P<Type>>) -> bool {
        has_base_type_check(self, t, check_base)
    }
}

fn has_base_type_check(c: &mut Checker, t: P<Type>, check_base: Option<P<Type>>) -> bool {
    if t.object_flags().intersects(ObjectFlags::ClassOrInterface | ObjectFlags::Reference) {
        let target = get_target_type(t);
        if target == check_base {
            return true;
        }
        let base_types = c.get_base_types(target.unwrap());
        return base_types.iter().any(|b| has_base_type_check(c, *b, check_base));
    }
    if t.flags().intersects(TypeFlags::Intersection) {
        return t.types().iter().any(|t| has_base_type_check(c, *t, check_base));
    }
    false
}

// checker.go:19907
pub(crate) fn get_target_type(t: P<Type>) -> Option<P<Type>> {
    if t.object_flags().intersects(ObjectFlags::Reference) {
        return t.target();
    }
    Some(t)
}

impl Checker {
    // checker.go:19914
    pub(crate) fn get_type_with_this_argument(
        &mut self,
        t: P<Type>,
        this_argument: Option<P<Type>>,
        need_apparent_type: bool,
    ) -> P<Type> {
        let mut this_argument = this_argument;
        if t.object_flags().intersects(ObjectFlags::Reference) {
            let target = t.target().unwrap();
            let type_arguments = self.get_type_arguments(t);
            if target.as_interface_type().type_parameters().len() == type_arguments.len() {
                if this_argument.is_none() {
                    this_argument = target.as_interface_type().this_type.get();
                }
                let mut args = Vec::with_capacity(type_arguments.len() + 1);
                args.extend_from_slice(type_arguments);
                args.push(this_argument.unwrap());
                return self.create_type_reference(target, &args);
            }
            return t;
        } else if t.flags().intersects(TypeFlags::Intersection) {
            let types = t.types();
            let new_types = same_map(types, |t| {
                self.get_type_with_this_argument(*t, this_argument, need_apparent_type)
            });
            if matches!(new_types, std::borrow::Cow::Borrowed(_)) {
                return t;
            }
            return self.get_intersection_type(&new_types);
        }
        if need_apparent_type {
            return self.get_apparent_type(t);
        }
        t
    }

    // checker.go:19939
    pub(crate) fn add_inherited_members(
        symbols: Option<P<SymbolTable>>,
        base_symbols: &[P<Symbol>],
    ) -> Option<P<SymbolTable>> {
        let mut symbols = symbols;
        for &base in base_symbols {
            if !is_static_private_identifier_property(base) {
                let s = symbols.and_then(|table| table.lookup(base.name()));
                if s.is_none() || !s.unwrap().flags().intersects(SymbolFlags::Value) {
                    if symbols.is_none() {
                        symbols = Some(SymbolTable::new());
                    }
                    symbols.unwrap().set(base.name(), base);
                }
            }
        }
        symbols
    }

    // checker.go:19953
    pub(crate) fn resolve_declared_members(&mut self, t: P<Type>) -> &'static InterfaceType {
        let d = t.as_interface_type();
        if !d.declared_members_resolved.get() {
            let members = self.get_members_of_symbol(t.symbol().unwrap());
            d.declared_members_resolved.set(true);
            d.declared_members.set(members);
            let call_symbol =
                d.declared_members.get().and_then(|m| m.lookup(INTERNAL_SYMBOL_NAME_CALL));
            let call_signatures = self.get_signatures_of_symbol(call_symbol);
            d.declared_call_signatures.set(alloc_vec(call_signatures));
            let new_symbol =
                d.declared_members.get().and_then(|m| m.lookup(INTERNAL_SYMBOL_NAME_NEW));
            let construct_signatures = self.get_signatures_of_symbol(new_symbol);
            d.declared_construct_signatures.set(alloc_vec(construct_signatures));
            let index_infos = self.get_index_infos_of_symbol(t.symbol().unwrap());
            d.declared_index_infos.set(alloc_vec(index_infos));
        }
        d
    }

    // checker.go:19966
    fn get_index_infos_of_symbol(&mut self, symbol: P<Symbol>) -> Vec<P<IndexInfo>> {
        let index_symbol = self.get_index_symbol(symbol);
        if let Some(index_symbol) = index_symbol {
            let members = self.get_members_of_symbol(symbol);
            let sibling_symbols = members.map(|m| m.values()).unwrap_or_default();
            return self.get_index_infos_of_index_symbol(index_symbol, &sibling_symbols);
        }
        Vec::new()
    }

    // note intentional similarities to index signature building in `checkObjectLiteral` for parity
    // checker.go:19975
    pub(crate) fn get_index_infos_of_index_symbol(
        &mut self,
        index_symbol: P<Symbol>,
        sibling_symbols: &[P<Symbol>],
    ) -> Vec<P<IndexInfo>> {
        let mut index_infos: Vec<P<IndexInfo>> = Vec::new();
        let mut has_computed_string_property = false;
        let mut has_computed_number_property = false;
        let mut has_computed_symbol_property = false;
        let mut readonly_computed_string_property = true;
        let mut readonly_computed_number_property = true;
        let mut readonly_computed_symbol_property = true;
        let mut property_symbols: Vec<P<Symbol>> = Vec::new();
        let declarations = index_symbol.declarations();
        for &declaration in declarations {
            if ast::is_index_signature_declaration(declaration) {
                let parameters = declaration.parameters();
                let return_type_node = declaration.type_node();
                if parameters.len() == 1 {
                    let type_node = parameters[0].type_node();
                    if let Some(type_node) = type_node {
                        let mut value_type = self.any_type;
                        if let Some(return_type_node) = return_type_node {
                            value_type = self.get_type_from_type_node(return_type_node);
                        }
                        let key_types = self.get_type_from_type_node(type_node);
                        for_each_type(self, key_types, |c, key_type| {
                            if c.is_valid_index_key_type(key_type)
                                && find_index_info(&index_infos, key_type).is_none()
                            {
                                let index_info = checker::Checker::new_index_info(
                                    key_type,
                                    value_type,
                                    ast::has_modifier(declaration, ModifierFlags::Readonly),
                                    Some(declaration),
                                    &[],
                                );
                                index_infos.push(index_info);
                            }
                        });
                    }
                }
            } else if self.has_late_bindable_index_signature(declaration) {
                let decl_name = if ast::is_binary_expression(declaration) {
                    declaration.as_binary_expression().left()
                } else {
                    declaration.name().unwrap()
                };
                let key_type = if ast::is_element_access_expression(decl_name) {
                    self.check_expression_cached(
                        decl_name.as_element_access_expression().argument_expression,
                    )
                } else {
                    self.check_computed_property_name(decl_name)
                };
                if find_index_info(&index_infos, key_type).is_some() {
                    continue;
                    // Explicit index for key type takes priority
                }
                if self.is_type_assignable_to(key_type, self.string_number_symbol_type) {
                    if self.is_type_assignable_to(key_type, self.number_type) {
                        has_computed_number_property = true;
                        if !has_readonly_modifier(declaration) {
                            readonly_computed_number_property = false;
                        }
                    } else if self.is_type_assignable_to(key_type, self.es_symbol_type) {
                        has_computed_symbol_property = true;
                        if !has_readonly_modifier(declaration) {
                            readonly_computed_symbol_property = false;
                        }
                    } else {
                        has_computed_string_property = true;
                        if !has_readonly_modifier(declaration) {
                            readonly_computed_string_property = false;
                        }
                    }
                    property_symbols.push(declaration.symbol().unwrap());
                }
            }
        }
        if has_computed_string_property
            || has_computed_number_property
            || has_computed_symbol_property
        {
            property_symbols.extend(
                sibling_symbols
                    .iter()
                    .filter_map(|&sym| if sym == index_symbol { None } else { Some(sym) }),
            );
            // aggregate similar index infos implied to be the same key to the same combined index info
            if has_computed_string_property
                && find_index_info(&index_infos, self.string_type).is_none()
            {
                let info = self.get_object_literal_index_info(
                    readonly_computed_string_property,
                    &property_symbols,
                    self.string_type,
                );
                index_infos.push(info);
            }
            if has_computed_number_property
                && find_index_info(&index_infos, self.number_type).is_none()
            {
                let info = self.get_object_literal_index_info(
                    readonly_computed_number_property,
                    &property_symbols,
                    self.number_type,
                );
                index_infos.push(info);
            }
            if has_computed_symbol_property
                && find_index_info(&index_infos, self.es_symbol_type).is_none()
            {
                let info = self.get_object_literal_index_info(
                    readonly_computed_symbol_property,
                    &property_symbols,
                    self.es_symbol_type,
                );
                index_infos.push(info);
            }
        }
        index_infos
    }

    // NOTE: currently does not make pattern literal indexers, eg `${number}px`
    // checker.go:20062
    pub(crate) fn get_object_literal_index_info(
        &mut self,
        is_readonly: bool,
        properties: &[P<Symbol>],
        key_type: P<Type>,
    ) -> P<IndexInfo> {
        let mut prop_types: Vec<P<Type>> = Vec::new();
        let mut components: Vec<P<Node>> = Vec::new();
        for &prop in properties {
            if key_type == self.string_type && !self.is_symbol_with_symbol_name(prop)
                || key_type == self.number_type && self.is_symbol_with_numeric_name(prop)
                || key_type == self.es_symbol_type && self.is_symbol_with_symbol_name(prop)
            {
                let prop_type = self.get_type_of_symbol(prop);
                prop_types.push(prop_type);
                if Self::is_symbol_with_computed_name(prop) {
                    let first = prop.declarations()[0];
                    components.push(first);
                }
            }
        }
        let mut union_type = self.undefined_type;
        if !prop_types.is_empty() {
            union_type =
                self.get_union_type_ex(&prop_types, UnionReduction::Subtype, AliasArg::None, None);
        }
        Self::new_index_info(
            key_type,
            union_type,
            is_readonly,
            None, /*declaration*/
            &components,
        )
    }

    // checker.go:20082
    fn is_symbol_with_symbol_name(&mut self, symbol: P<Symbol>) -> bool {
        if is_known_symbol(symbol) {
            return true;
        }
        let first = symbol.declarations().first().copied();
        if let Some(first) = first {
            let name = first.name();
            return name.is_some_and(ast::is_computed_property_name) && {
                let name_type = self.check_computed_property_name(name.unwrap());
                self.is_type_assignable_to_kind(name_type, TypeFlags::ESSymbol)
            };
        }
        false
    }

    // checker.go:20093
    fn is_symbol_with_numeric_name(&mut self, symbol: P<Symbol>) -> bool {
        if is_numeric_literal_name(symbol.name()) {
            return true;
        }
        let first = symbol.declarations().first().copied();
        if let Some(first) = first {
            let name = first.name();
            return name.is_some() && self.is_numeric_name(name.unwrap());
        }
        false
    }

    // checker.go:20104
    fn is_symbol_with_computed_name(symbol: P<Symbol>) -> bool {
        let first = symbol.declarations().first().copied();
        if let Some(first) = first {
            let name = first.name();
            return name.is_some_and(ast::is_computed_property_name);
        }
        false
    }

    // checker.go:20112
    fn is_numeric_name(&mut self, name: P<Node>) -> bool {
        match name.kind() {
            Kind::ComputedPropertyName => return self.is_numeric_computed_name(name),
            Kind::Identifier | Kind::NumericLiteral | Kind::StringLiteral => {
                return is_numeric_literal_name(name.text());
            }
            _ => {}
        }
        false
    }

    // checker.go:20122
    fn is_numeric_computed_name(&mut self, name: P<Node>) -> bool {
        // It seems odd to consider an expression of type Any to result in a numeric name,
        // but this behavior is consistent with checkIndexedAccess
        let t = self.check_computed_property_name(name);
        self.is_type_assignable_to_kind(t, TypeFlags::NumberLike)
    }

    // checker.go:20128
    pub(crate) fn is_valid_index_key_type(&mut self, t: P<Type>) -> bool {
        t.flags().intersects(TypeFlags::String | TypeFlags::Number | TypeFlags::ESSymbol)
            || self.is_pattern_literal_type(t)
            || t.flags().intersects(TypeFlags::Intersection)
                && !self.is_generic_type(t)
                && t.types().iter().any(|t| self.is_valid_index_key_type(*t))
    }

    // checker.go:20143
    pub(crate) fn get_index_symbol(&mut self, symbol: P<Symbol>) -> Option<P<Symbol>> {
        self.get_members_of_symbol(symbol)?.lookup(INTERNAL_SYMBOL_NAME_INDEX)
    }

    // checker.go:20147
    pub(crate) fn get_signatures_of_symbol(
        &mut self,
        symbol: Option<P<Symbol>>,
    ) -> Vec<P<Signature>> {
        let Some(symbol) = symbol else {
            return Vec::new();
        };
        let mut result = Vec::new();
        let declarations = symbol.declarations();
        for (i, &decl) in declarations.iter().enumerate() {
            if !ast::is_function_like(decl) {
                continue;
            }
            // Don't include signature if node is the implementation of an overloaded function. A node is considered
            // an implementation node if it has a body and the previous node is of the same kind and immediately
            // precedes the implementation node (i.e. has the same parent and ends where the implementation starts).
            if i > 0 && decl.body().is_some() {
                let previous = declarations[i - 1];
                if decl.parent() == previous.parent()
                    && decl.kind() == previous.kind()
                    && (decl.pos() == previous.end()
                        || previous.flags().intersects(NodeFlags::Reparsed))
                {
                    continue;
                }
            }
            // If this is a function or method declaration, get the signature from the @type tag for the sake of optional parameters.
            // Exclude contextually-typed kinds because we already apply the @type tag to the context, plus applying it here to the initializer would suppress checks that the two are compatible.
            let mut sig = self.get_signature_of_full_signature_type(decl);
            if sig.is_none() {
                sig = Some(self.get_signature_from_declaration(decl));
            }
            result.push(sig.unwrap());
        }
        result
    }

    // checker.go:20177
    pub(crate) fn get_signature_from_declaration(&mut self, declaration: P<Node>) -> P<Signature> {
        let links = self.signature_links.get(declaration);
        if let Some(resolved_signature) = links.resolved_signature.get() {
            return resolved_signature;
        }
        let mut parameters: Vec<P<Symbol>> = Vec::new();
        let mut flags = SignatureFlags::None;
        let mut this_parameter: Option<P<Symbol>> = None;
        let mut min_argument_count = 0;
        let mut has_this_parameter = false;
        let iife = ast::get_immediately_invoked_function_expression(declaration);
        let is_untyped_signature_in_js_file = iife.is_none()
            && ast::is_in_js_file(declaration)
            && (ast::is_function_expression(declaration)
                || ast::is_arrow_function(declaration)
                || ast::is_method_or_accessor(declaration)
                || ast::is_function_declaration(declaration)
                || ast::is_constructor_declaration(declaration))
            && declaration.parameters().iter().all(|param| param.type_node().is_none())
            && self.get_contextual_type(declaration, ContextFlags::Signature).is_none();
        if is_untyped_signature_in_js_file {
            flags |= SignatureFlags::IsUntypedSignatureInJSFile;
        }
        for (i, &param) in declaration.parameters().iter().enumerate() {
            let mut param_symbol = param.symbol();
            let type_node = param.type_node();
            // Include parameter symbol instead of property symbol in the signature
            if let Some(ps) = param_symbol
                && ps.flags().intersects(SymbolFlags::Property)
                && !ast::is_binding_pattern(param.name().unwrap())
            {
                let resolved_symbol = self.resolve_name(
                    Some(param),
                    ps.name(),
                    SymbolFlags::Value,
                    None,  /*nameNotFoundMessage*/
                    false, /*isUse*/
                    false, /*excludeGlobals*/
                );
                param_symbol = resolved_symbol;
            }
            if i == 0 && param_symbol.unwrap().name() == INTERNAL_SYMBOL_NAME_THIS {
                has_this_parameter = true;
                this_parameter = param.symbol();
            } else {
                parameters.push(param_symbol.unwrap());
            }
            if type_node.is_some_and(|t| t.kind() == Kind::LiteralType) {
                flags |= SignatureFlags::HasLiteralTypes;
            }
            // Record a new minimum argument count if this is not an optional parameter
            let is_optional_parameter = is_optional_declaration(param)
                || param.initializer().is_some()
                || is_rest_parameter(param)
                || iife.is_some()
                    && parameters.len() > iife.unwrap().arguments().len()
                    && type_node.is_none();
            if !is_optional_parameter {
                min_argument_count = parameters.len();
            }
        }
        // If only one accessor includes a this-type annotation, the other behaves as if it had the same type annotation
        if (ast::is_get_accessor_declaration(declaration)
            || ast::is_set_accessor_declaration(declaration))
            && self.has_bindable_name(declaration)
            && (!has_this_parameter || this_parameter.is_none())
        {
            let other_kind = if ast::is_get_accessor_declaration(declaration) {
                Kind::SetAccessor
            } else {
                Kind::GetAccessor
            };
            let symbol = self.get_symbol_of_declaration(declaration).unwrap();
            let other = ast::get_declaration_of_kind(symbol, other_kind);
            if let Some(other) = other {
                this_parameter = Self::get_annotated_accessor_this_parameter(other);
            }
        }
        let mut class_type = None;
        if ast::is_constructor_declaration(declaration) {
            let merged = self.get_merged_symbol(declaration.parent().unwrap().symbol().unwrap());
            class_type = Some(self.get_declared_type_of_class_or_interface(merged));
        }
        let type_parameters: Vec<P<Type>> = if let Some(class_type) = class_type {
            class_type.as_interface_type().local_type_parameters().to_vec()
        } else {
            self.get_type_parameters_from_declaration(declaration)
        };
        if has_rest_parameter(declaration) {
            flags |= SignatureFlags::HasRestParameter;
        }
        if ast::is_constructor_type_node(declaration)
            || ast::is_constructor_declaration(declaration)
            || ast::is_construct_signature_declaration(declaration)
        {
            flags |= SignatureFlags::Construct;
        }
        if ast::is_constructor_type_node(declaration)
            && ast::has_syntactic_modifier(declaration, ModifierFlags::Abstract)
            || ast::is_constructor_declaration(declaration)
                && ast::has_syntactic_modifier(
                    declaration.parent().unwrap(),
                    ModifierFlags::Abstract,
                )
        {
            flags |= SignatureFlags::Abstract;
        }
        let sig = self.new_signature(
            flags,
            Some(declaration),
            &type_parameters,
            this_parameter,
            &parameters,
            None, /*resolvedReturnType*/
            None, /*resolvedTypePredicate*/
            min_argument_count,
        );
        links.resolved_signature.set(Some(sig));
        links.resolved_signature.get().unwrap()
    }

    // checker.go:20253
    pub(crate) fn get_type_parameters_from_declaration(
        &mut self,
        declaration: P<Node>,
    ) -> Vec<P<Type>> {
        if let Some(sig) = self.get_signature_of_full_signature_type(declaration) {
            return sig.type_parameters().to_vec();
        }
        let mut result = Vec::new();
        for &node in declaration.type_parameters() {
            let tp = self.get_declared_type_of_type_parameter(node.symbol().unwrap());
            append_if_unique(&mut result, tp);
        }
        result
    }

    // checker.go:20264
    fn get_annotated_accessor_this_parameter(accessor: P<Node>) -> Option<P<Symbol>> {
        let parameter = Self::get_accessor_this_parameter(accessor);
        if let Some(parameter) = parameter {
            return parameter.symbol();
        }
        None
    }

    // checker.go:20272
    pub(crate) fn get_accessor_this_parameter(accessor: P<Node>) -> Option<P<Node>> {
        if accessor.parameters().len()
            == if ast::is_get_accessor_declaration(accessor) { 1 } else { 2 }
        {
            return ast::get_this_parameter(accessor);
        }
        None
    }

    /**
     * Indicates whether a declaration has an early-bound name or a dynamic name that can be late-bound.
     */
    // checker.go:20282
    pub(crate) fn has_bindable_name(&mut self, node: P<Node>) -> bool {
        !ast::has_dynamic_name(node) || self.has_late_bindable_name(node)
    }

    /**
     * Indicates whether a declaration has a late-bindable dynamic name.
     */
    // checker.go:20289
    pub(crate) fn has_late_bindable_name(&mut self, node: P<Node>) -> bool {
        let name = ast::get_name_of_declaration(node);
        name.is_some() && self.is_late_bindable_name(name.unwrap())
    }

    /**
     * Indicates whether a declaration name is definitely late-bindable.
     * A declaration name is only late-bindable if:
     * - It is a `ComputedPropertyName`.
     * - Its expression is an `Identifier` or either a `PropertyAccessExpression` an
     *   `ElementAccessExpression` consisting only of these same three types of nodes.
     * - The type of its expression is a string or numeric literal type, or is a `unique symbol` type.
     */
    // checker.go:20302
    pub(crate) fn is_late_bindable_name(&mut self, node: P<Node>) -> bool {
        if !is_late_bindable_ast(node) {
            return false;
        }
        if ast::is_computed_property_name(node) {
            let t = self.check_computed_property_name(node);
            return is_type_usable_as_property_name(t);
        }
        let t =
            self.check_expression_cached(node.as_element_access_expression().argument_expression);
        is_type_usable_as_property_name(t)
    }

    // checker.go:20312
    pub(crate) fn has_late_bindable_index_signature(&mut self, node: P<Node>) -> bool {
        let name = ast::get_name_of_declaration(node);
        name.is_some() && self.is_late_bindable_index_signature(name.unwrap())
    }

    // checker.go:20317
    pub(crate) fn is_late_bindable_index_signature(&mut self, node: P<Node>) -> bool {
        if !is_late_bindable_ast(node) {
            return false;
        }
        if ast::is_computed_property_name(node) {
            let t = self.check_computed_property_name(node);
            return self.is_type_usable_as_index_signature_declaration(t);
        }
        let t =
            self.check_expression_cached(node.as_element_access_expression().argument_expression);
        self.is_type_usable_as_index_signature_declaration(t)
    }

    // checker.go:20327
    fn is_type_usable_as_index_signature_declaration(&mut self, t: P<Type>) -> bool {
        self.is_type_assignable_to(t, self.string_number_symbol_type)
    }
}

// checker.go:20331
fn is_late_bindable_ast(node: P<Node>) -> bool {
    let mut expr = None;
    if ast::is_computed_property_name(node) {
        expr = node.expression();
    } else if ast::is_element_access_expression(node) {
        expr = Some(node.as_element_access_expression().argument_expression);
    }
    expr.is_some_and(ast::is_entity_name_expression)
}

impl Checker {
    // checker.go:20342
    pub fn get_return_type_of_signature(&mut self, sig: P<Signature>) -> P<Type> {
        if let Some(resolved_return_type) = sig.resolved_return_type.get() {
            return resolved_return_type;
        }
        if !self.push_type_resolution(sig.into(), TypeSystemPropertyName::ResolvedReturnType) {
            return self.error_type;
        }
        let mut t;
        if let Some(target) = sig.target.get() {
            let target_return_type = self.get_return_type_of_signature(target);
            t = self.instantiate_type(target_return_type, sig.mapper.get());
        } else if let Some(composite) = sig.composite() {
            let mut return_types = Vec::with_capacity(composite.signatures.get().len());
            return_types.extend(
                composite.signatures.get().iter().map(|&s| self.get_return_type_of_signature(s)),
            );
            let combined = self.get_union_or_intersection_type(
                &return_types,
                composite.is_union.get(),
                UnionReduction::Subtype,
            );
            t = self.instantiate_type(combined, sig.mapper.get());
        } else {
            let declaration = sig.declaration.get().unwrap();
            let annotated = self.get_return_type_from_annotation(declaration);
            t = match annotated {
                Some(annotated) => annotated,
                None => {
                    if ast::node_is_missing(declaration.body()) {
                        self.any_type
                    } else {
                        self.get_return_type_from_body(declaration, CheckMode::Normal)
                    }
                }
            };
        }
        if sig.flags.get().intersects(SignatureFlags::IsInnerCallChain) {
            t = self.add_optional_type_marker(t);
        } else if sig.flags.get().intersects(SignatureFlags::IsOuterCallChain) {
            t = self.get_optional_type(t, false /*isProperty*/);
        }
        if !self.pop_type_resolution() {
            if let Some(declaration) = sig.declaration.get() {
                let type_node = declaration.type_node();
                if let Some(type_node) = type_node {
                    self.error(
                        Some(type_node),
                        &diagnostics::RETURN_TYPE_ANNOTATION_CIRCULARLY_REFERENCES_ITSELF,
                        &[],
                    );
                } else if self.no_implicit_any {
                    let name = ast::get_name_of_declaration(declaration);
                    if let Some(name) = name {
                        let name_text = tsrs_scanner::declaration_name_to_string(Some(name));
                        self.error(Some(name), &diagnostics::X_0_IMPLICITLY_HAS_RETURN_TYPE_ANY_BECAUSE_IT_DOES_NOT_HAVE_A_RETURN_TYPE_ANNOTATION_AND_IS_REFERENCED_DIRECTLY_OR_INDIRECTLY_IN_ONE_OF_ITS_RETURN_EXPRESSIONS, &[&name_text]);
                    } else {
                        self.error(Some(declaration), &diagnostics::FUNCTION_IMPLICITLY_HAS_RETURN_TYPE_ANY_BECAUSE_IT_DOES_NOT_HAVE_A_RETURN_TYPE_ANNOTATION_AND_IS_REFERENCED_DIRECTLY_OR_INDIRECTLY_IN_ONE_OF_ITS_RETURN_EXPRESSIONS, &[]);
                    }
                }
            }
            t = self.any_type;
        }
        if sig.resolved_return_type.get().is_none() {
            sig.resolved_return_type.set(Some(t));
        }
        sig.resolved_return_type.get().unwrap()
    }

    // checker.go:20392
    pub(crate) fn get_non_circular_return_type_of_signature(
        &mut self,
        sig: P<Signature>,
    ) -> P<Type> {
        if self.is_resolving_return_type_of_signature(sig) {
            return self.any_type;
        }
        self.get_return_type_of_signature(sig)
    }

    // checker.go:20399
    pub(crate) fn get_return_type_from_annotation(
        &mut self,
        declaration: P<Node>,
    ) -> Option<P<Type>> {
        if ast::is_constructor_declaration(declaration) {
            let merged = self.get_merged_symbol(declaration.parent().unwrap().symbol().unwrap());
            return Some(self.get_declared_type_of_class_or_interface(merged));
        }
        let return_type = declaration.type_node();
        if let Some(return_type) = return_type {
            return Some(self.get_type_from_type_node(return_type));
        }
        if ast::is_get_accessor_declaration(declaration) && self.has_bindable_name(declaration) {
            let symbol = self.get_symbol_of_declaration(declaration).unwrap();
            return self.get_annotated_accessor_type(ast::get_declaration_of_kind(
                symbol,
                Kind::SetAccessor,
            ));
        }
        self.get_return_type_of_full_signature(declaration)
    }

    // checker.go:20413
    pub(crate) fn get_signature_of_full_signature_type(
        &mut self,
        node: P<Node>,
    ) -> Option<P<Signature>> {
        if ast::is_in_js_file(node)
            && (ast::is_function_declaration(node)
                || ast::is_method_declaration(node)
                || ast::is_function_expression_or_arrow_function(node))
            && node.function_like_data().unwrap().full_signature().is_some()
        {
            let t = self.get_type_from_type_node(
                node.function_like_data().unwrap().full_signature().unwrap(),
            );
            return self.get_single_call_signature(t);
        }
        None
    }

    // checker.go:20420
    pub(crate) fn get_parameter_type_of_full_signature(
        &mut self,
        node: P<Node>,
        parameter: P<Node>,
    ) -> Option<P<Type>> {
        if let Some(signature) = self.get_signature_of_full_signature_type(node) {
            let pos = node
                .parameters()
                .iter()
                .position(|p| *p == parameter)
                .expect("parameter belongs to its signature");
            if parameter.as_parameter_declaration().dot_dot_dot_token().is_some() {
                return Some(self.get_rest_type_at_position(signature, pos, false /*readonly*/));
            }
            return Some(self.get_type_at_position(signature, pos));
        }
        None
    }

    // checker.go:20432
    fn get_return_type_of_full_signature(&mut self, node: P<Node>) -> Option<P<Type>> {
        if let Some(signature) = self.get_signature_of_full_signature_type(node) {
            return Some(self.get_return_type_of_signature(signature));
        }
        None
    }

    // checker.go:20439
    pub(crate) fn get_annotated_accessor_type(
        &mut self,
        accessor: Option<P<Node>>,
    ) -> Option<P<Type>> {
        let node = Self::get_annotated_accessor_type_node(accessor);
        if let Some(node) = node {
            return Some(self.get_type_from_type_node(node));
        }
        None
    }

    // checker.go:20447
    pub(crate) fn get_annotated_accessor_type_node(accessor: Option<P<Node>>) -> Option<P<Node>> {
        if let Some(accessor) = accessor {
            match accessor.kind() {
                Kind::GetAccessor | Kind::PropertyDeclaration => return accessor.type_node(),
                Kind::SetAccessor => {
                    return get_effective_set_accessor_type_annotation_node(accessor);
                }
                _ => {}
            }
        }
        None
    }
}

// checker.go:20459
fn get_effective_set_accessor_type_annotation_node(node: P<Node>) -> Option<P<Node>> {
    let param = get_set_accessor_value_parameter(node);
    if let Some(param) = param {
        return param.type_node();
    }
    None
}

impl Checker {
    // checker.go:20467
    pub(crate) fn get_return_type_from_body(
        &mut self,
        fn_: P<Node>,
        check_mode: CheckMode,
    ) -> P<Type> {
        let Some(body) = fn_.body() else {
            return self.error_type;
        };
        let function_flags = get_function_flags(Some(fn_));
        let is_async = function_flags.intersects(FunctionFlags::Async);
        let is_generator = function_flags.intersects(FunctionFlags::Generator);
        let mut return_type: Option<P<Type>> = None;
        let mut yield_type: Option<P<Type>> = None;
        let mut next_type: Option<P<Type>> = None;
        let mut fallback_return_type = self.void_type;
        if !ast::is_block(body) {
            let mut rt = self
                .check_expression_cached_ex(body, check_mode & !CheckMode::SkipGenericFunctions);
            if self.is_const_context(body) {
                rt = self.get_regular_type_of_literal_type(rt);
            }
            if is_async {
                // From within an async function you can return either a non-promise value or a promise. Any
                // Promise/A+ compatible implementation will always assimilate any foreign promise, so the
                // return type of the body should be unwrapped to its awaited type, which we will wrap in
                // the native Promise<T> type later in this function.
                let awaited = self.check_awaited_type(rt, false /*withAlias*/, fn_ /*errorNode*/, &diagnostics::THE_RETURN_TYPE_OF_AN_ASYNC_FUNCTION_MUST_EITHER_BE_A_VALID_PROMISE_OR_MUST_NOT_CONTAIN_A_CALLABLE_THEN_MEMBER);
                rt = self.unwrap_awaited_type(awaited);
            }
            return_type = Some(rt);
        } else if is_generator {
            let (return_types, is_never_returning) =
                self.check_and_aggregate_return_expression_types(fn_, check_mode);
            if is_never_returning {
                fallback_return_type = self.never_type;
            } else if !return_types.is_empty() {
                return_type = Some(self.get_union_type_ex(
                    &return_types,
                    UnionReduction::Subtype,
                    AliasArg::None,
                    None,
                ));
            }
            let (yield_types, next_types) =
                self.check_and_aggregate_yield_operand_types(fn_, check_mode);
            if !yield_types.is_empty() {
                yield_type = Some(self.get_union_type_ex(
                    &yield_types,
                    UnionReduction::Subtype,
                    AliasArg::None,
                    None,
                ));
            }
            if !next_types.is_empty() {
                next_type = Some(self.get_intersection_type(&next_types));
            }
        } else {
            let (types, is_never_returning) =
                self.check_and_aggregate_return_expression_types(fn_, check_mode);
            if is_never_returning {
                // For an async function, the return type will not be never, but rather a Promise for never.
                if function_flags.intersects(FunctionFlags::Async) {
                    return self.create_promise_return_type(fn_, self.never_type);
                }
                // Normal function
                return self.never_type;
            }
            if types.is_empty() {
                // For an async function, the return type will not be void/undefined, but rather a Promise for void/undefined.
                let contextual_return_type =
                    self.get_contextual_return_type(fn_, ContextFlags::None);
                let return_type = if contextual_return_type.is_some() && {
                    let unwrapped = self
                        .unwrap_return_type(contextual_return_type.unwrap(), function_flags)
                        .unwrap_or(self.void_type);
                    some_type(self, unwrapped, |_, t| t.flags().intersects(TypeFlags::Undefined))
                } {
                    self.undefined_type
                } else {
                    self.void_type
                };
                if function_flags.intersects(FunctionFlags::Async) {
                    return self.create_promise_return_type(fn_, return_type);
                }
                // Normal function
                return return_type;
            }
            // Return a union of the return expression types.
            return_type =
                Some(self.get_union_type_ex(&types, UnionReduction::Subtype, AliasArg::None, None));
        }
        if return_type.is_some() || yield_type.is_some() || next_type.is_some() {
            if let Some(yt) = yield_type {
                self.report_errors_from_widening(fn_, yt, WideningKind::GeneratorYield);
            }
            if let Some(rt) = return_type {
                self.report_errors_from_widening(fn_, rt, WideningKind::FunctionReturn);
            }
            if let Some(nt) = next_type {
                self.report_errors_from_widening(fn_, nt, WideningKind::GeneratorNext);
            }
            if return_type.is_some_and(is_unit_type)
                || yield_type.is_some_and(is_unit_type)
                || next_type.is_some_and(is_unit_type)
            {
                let contextual_signature =
                    self.get_contextual_signature_for_function_like_declaration(fn_);
                let mut contextual_type: Option<P<Type>> = None;
                match contextual_signature {
                    None => {
                        // No contextual type
                    }
                    Some(cs) if cs == self.get_signature_from_declaration(fn_) => {
                        if !is_generator {
                            contextual_type = return_type;
                        }
                    }
                    Some(cs) => {
                        let cs_return_type = self.get_return_type_of_signature(cs);
                        contextual_type = self.instantiate_contextual_type(
                            Some(cs_return_type),
                            fn_,
                            ContextFlags::None,
                        );
                    }
                }
                if is_generator {
                    yield_type = self
                        .get_widened_literal_like_type_for_contextual_iteration_type_if_needed(
                            yield_type,
                            contextual_type,
                            IterationTypeKind::Yield,
                            is_async,
                        );
                    return_type = self
                        .get_widened_literal_like_type_for_contextual_iteration_type_if_needed(
                            return_type,
                            contextual_type,
                            IterationTypeKind::Return,
                            is_async,
                        );
                    next_type = self
                        .get_widened_literal_like_type_for_contextual_iteration_type_if_needed(
                            next_type,
                            contextual_type,
                            IterationTypeKind::Next,
                            is_async,
                        );
                } else {
                    return_type = self
                        .get_widened_literal_like_type_for_contextual_return_type_if_needed(
                            return_type,
                            contextual_type,
                            is_async,
                        );
                }
            }
            if let Some(yt) = yield_type {
                yield_type = Some(self.get_widened_type(yt));
            }
            if let Some(rt) = return_type {
                return_type = Some(self.get_widened_type(rt));
            }
            if let Some(nt) = next_type {
                next_type = Some(self.get_widened_type(nt));
            }
        }
        let return_type = return_type.unwrap_or(fallback_return_type);
        if is_generator {
            let yield_type = yield_type.unwrap_or(self.never_type);
            let next_type = match next_type {
                Some(nt) => nt,
                None => self
                    .get_contextual_iteration_type(IterationTypeKind::Next, fn_)
                    .unwrap_or(self.unknown_type),
            };
            return self.create_generator_type(yield_type, return_type, next_type, is_async);
        }
        // From within an async function you can return either a non-promise value or a promise. Any
        // Promise/A+ compatible implementation will always assimilate any foreign promise, so the
        // return type of the body is awaited type of the body, wrapped in a native Promise<T> type.
        if is_async {
            return self.create_promise_type(return_type);
        }
        return_type
    }

    // Returns the aggregated list of return types, plus a bool indicating a never-returning function.
    // checker.go:20600
    fn check_and_aggregate_return_expression_types(
        &mut self,
        fn_: P<Node>,
        check_mode: CheckMode,
    ) -> (Vec<P<Type>>, bool) {
        let function_flags = get_function_flags(Some(fn_));
        let mut aggregated_types: Vec<P<Type>> = Vec::new();
        let mut has_return_with_no_expression = self.function_has_implicit_return(fn_);
        let mut has_return_of_type_never = false;
        ast::for_each_return_statement(fn_.body().unwrap(), |return_statement| {
            let Some(expr) = return_statement.expression() else {
                has_return_with_no_expression = true;
                return false;
            };
            let mut expr = ast::skip_parentheses(expr);
            // Bare calls to this same function don't contribute to inference
            // and `return await` is also safe to unwrap here
            if function_flags.intersects(FunctionFlags::Async) && ast::is_await_expression(expr) {
                expr = ast::skip_parentheses(expr.expression().unwrap());
            }
            if ast::is_call_expression(expr)
                && ast::is_identifier(expr.expression().unwrap())
                && {
                    let callee_type = self.check_expression_cached(expr.expression().unwrap());
                    let merged = self.get_merged_symbol(fn_.symbol().unwrap());
                    callee_type.symbol() == Some(merged)
                }
                && (!ast::is_function_expression_or_arrow_function(
                    fn_.symbol().unwrap().value_declaration().unwrap(),
                ) || self.is_constant_reference(expr.expression().unwrap()))
            {
                has_return_of_type_never = true;
                return false;
            }
            let mut t = self
                .check_expression_cached_ex(expr, check_mode & !CheckMode::SkipGenericFunctions);
            if function_flags.intersects(FunctionFlags::Async) {
                // From within an async function you can return either a non-promise value or a promise. Any
                // Promise/A+ compatible implementation will always assimilate any foreign promise, so the
                // return type of the body should be unwrapped to its awaited type, which should be wrapped in
                // the native Promise<T> type by the caller.
                let awaited = self.check_awaited_type(t, false /*withAlias*/, fn_, &diagnostics::THE_RETURN_TYPE_OF_AN_ASYNC_FUNCTION_MUST_EITHER_BE_A_VALID_PROMISE_OR_MUST_NOT_CONTAIN_A_CALLABLE_THEN_MEMBER);
                t = self.unwrap_awaited_type(awaited);
            }
            if t.flags().intersects(TypeFlags::Never) {
                has_return_of_type_never = true;
            }
            if self.is_const_context(expr) {
                t = self.get_regular_type_of_literal_type(t);
            }
            append_if_unique(&mut aggregated_types, t);
            false
        });
        if aggregated_types.is_empty()
            && !has_return_with_no_expression
            && (has_return_of_type_never || may_return_never(fn_))
        {
            return (Vec::new(), true);
        }
        if self.strict_null_checks && !aggregated_types.is_empty() && has_return_with_no_expression
        {
            append_if_unique(&mut aggregated_types, self.undefined_type);
        }
        (aggregated_types, false)
    }

    // checker.go:20648
    pub(crate) fn function_has_implicit_return(&mut self, fn_: P<Node>) -> bool {
        let end_flow_node = fn_.body_data().unwrap().end_flow_node();
        end_flow_node.is_some() && self.is_reachable_flow_node(end_flow_node.unwrap())
    }
}

// checker.go:20653
fn may_return_never(fn_: P<Node>) -> bool {
    match fn_.kind() {
        Kind::FunctionExpression | Kind::ArrowFunction => return true,
        Kind::MethodDeclaration => return ast::is_object_literal_expression(fn_.parent().unwrap()),
        _ => {}
    }
    false
}

impl Checker {
    // checker.go:20663
    fn check_and_aggregate_yield_operand_types(
        &mut self,
        fn_: P<Node>,
        check_mode: CheckMode,
    ) -> (Vec<P<Type>>, Vec<P<Type>>) {
        let mut yield_types: Vec<P<Type>> = Vec::new();
        let mut next_types: Vec<P<Type>> = Vec::new();
        let is_async = get_function_flags(Some(fn_)).intersects(FunctionFlags::Async);
        for_each_yield_expression(fn_.body().unwrap(), |yield_expr| {
            let mut yield_expr_type = self.undefined_widening_type;
            if let Some(expression) = yield_expr.expression() {
                yield_expr_type = self
                    .check_expression_ex(expression, check_mode & !CheckMode::SkipGenericFunctions);
            }
            if yield_expr.expression().is_some()
                && self.is_const_context(yield_expr.expression().unwrap())
            {
                yield_expr_type = self.get_regular_type_of_literal_type(yield_expr_type);
            }
            // Go appends a possibly-nil result here; a nil constituent would crash the union below.
            let yielded = self
                .get_yielded_type_of_yield_expression(
                    yield_expr,
                    yield_expr_type,
                    self.any_type,
                    is_async,
                )
                .unwrap();
            append_if_unique(&mut yield_types, yielded);

            let next_type = if yield_expr.as_yield_expression().asterisk_token.is_some() {
                let use_ =
                    if is_async { IterationUse::AsyncYieldStar } else { IterationUse::YieldStar };
                let iteration_types = self.get_iteration_types_of_iterable(
                    yield_expr_type,
                    use_,
                    yield_expr.expression(),
                );
                iteration_types.next_type
            } else {
                self.get_contextual_type(yield_expr, ContextFlags::None)
            };
            if let Some(next_type) = next_type {
                append_if_unique(&mut next_types, next_type);
            }
            false
        });
        (yield_types, next_types)
    }

    // checker.go:20689
    pub(crate) fn create_promise_type(&mut self, promised_type: P<Type>) -> P<Type> {
        // creates a `Promise<T>` type where `T` is the promisedType argument
        let global_promise_type = self.get_global_promise_type_checked();
        if global_promise_type != self.empty_generic_type {
            // if the promised type is itself a promise, get the underlying type; otherwise, fallback to the promised type
            // Unwrap an `Awaited<T>` to `T` to improve inference.
            let unwrapped = self.unwrap_awaited_type(promised_type);
            let promised_type =
                self.get_awaited_type_no_alias(unwrapped).unwrap_or(self.unknown_type);
            return self.create_type_reference(global_promise_type, &[promised_type]);
        }
        self.unknown_type
    }

    // checker.go:20701
    pub(crate) fn create_promise_like_type(&mut self, promised_type: P<Type>) -> P<Type> {
        // creates a `PromiseLike<T>` type where `T` is the promisedType argument
        let global_promise_like_type = self.get_global_promise_like_type();
        if global_promise_like_type != self.empty_generic_type {
            // if the promised type is itself a promise, get the underlying type; otherwise, fallback to the promised type
            // Unwrap an `Awaited<T>` to `T` to improve inference.
            let unwrapped = self.unwrap_awaited_type(promised_type);
            let promised_type =
                self.get_awaited_type_no_alias(unwrapped).unwrap_or(self.unknown_type);
            return self.create_type_reference(global_promise_like_type, &[promised_type]);
        }
        self.unknown_type
    }

    // checker.go:20713
    pub(crate) fn create_promise_return_type(
        &mut self,
        fn_: P<Node>,
        promised_type: P<Type>,
    ) -> P<Type> {
        let promise_type = self.create_promise_type(promised_type);
        if promise_type == self.unknown_type {
            let message: &'static Message = if ast::is_import_call(fn_) {
                &diagnostics::A_DYNAMIC_IMPORT_CALL_RETURNS_A_PROMISE_MAKE_SURE_YOU_HAVE_A_DECLARATION_FOR_PROMISE_OR_INCLUDE_ES2015_IN_YOUR_LIB_OPTION
            } else {
                &diagnostics::AN_ASYNC_FUNCTION_OR_METHOD_MUST_RETURN_A_PROMISE_MAKE_SURE_YOU_HAVE_A_DECLARATION_FOR_PROMISE_OR_INCLUDE_ES2015_IN_YOUR_LIB_OPTION
            };
            self.error(Some(fn_), message, &[]);
            return self.error_type;
        }
        if self.get_global_promise_constructor_symbol().is_none() {
            let message: &'static Message = if ast::is_import_call(fn_) {
                &diagnostics::A_DYNAMIC_IMPORT_CALL_IN_ES5_REQUIRES_THE_PROMISE_CONSTRUCTOR_MAKE_SURE_YOU_HAVE_A_DECLARATION_FOR_THE_PROMISE_CONSTRUCTOR_OR_INCLUDE_ES2015_IN_YOUR_LIB_OPTION
            } else {
                &diagnostics::AN_ASYNC_FUNCTION_OR_METHOD_IN_ES5_REQUIRES_THE_PROMISE_CONSTRUCTOR_MAKE_SURE_YOU_HAVE_A_DECLARATION_FOR_THE_PROMISE_CONSTRUCTOR_OR_INCLUDE_ES2015_IN_YOUR_LIB_OPTION
            };
            self.error(Some(fn_), message, &[]);
        }
        promise_type
    }

    // checker.go:20729
    pub(crate) fn unwrap_return_type(
        &mut self,
        return_type: P<Type>,
        function_flags: FunctionFlags,
    ) -> Option<P<Type>> {
        let is_generator = function_flags.intersects(FunctionFlags::Generator);
        let is_async = function_flags.intersects(FunctionFlags::Async);
        if is_generator {
            let return_iteration_type = self.get_iteration_type_of_generator_function_return_type(
                IterationTypeKind::Return,
                return_type,
                is_async,
            );
            let Some(return_iteration_type) = return_iteration_type else {
                return Some(self.error_type);
            };
            if is_async {
                let unwrapped = self.unwrap_awaited_type(return_iteration_type);
                return self.get_awaited_type_no_alias(unwrapped);
            }
            return Some(return_iteration_type);
        }
        if is_async {
            return Some(self.get_awaited_type_no_alias(return_type).unwrap_or(self.error_type));
        }
        Some(return_type)
    }

    // checker.go:20748
    fn get_widened_literal_like_type_for_contextual_return_type_if_needed(
        &mut self,
        t: Option<P<Type>>,
        contextual_signature_return_type: Option<P<Type>>,
        is_async: bool,
    ) -> Option<P<Type>> {
        let mut t = t;
        if let Some(ty) = t
            && is_unit_type(ty)
        {
            let mut contextual_type = None;
            match contextual_signature_return_type {
                None => {
                    // No contextual type
                }
                Some(csrt) if is_async => {
                    contextual_type = self.get_promised_type_of_promise(csrt);
                }
                Some(csrt) => {
                    contextual_type = Some(csrt);
                }
            }
            t = Some(self.get_widened_literal_like_type_for_contextual_type(ty, contextual_type));
        }
        t
    }

    // checker.go:20764
    fn get_widened_literal_like_type_for_contextual_iteration_type_if_needed(
        &mut self,
        t: Option<P<Type>>,
        contextual_signature_return_type: Option<P<Type>>,
        kind: IterationTypeKind,
        is_async_generator: bool,
    ) -> Option<P<Type>> {
        let mut t = t;
        if let Some(ty) = t
            && is_unit_type(ty)
        {
            let mut contextual_type = None;
            if let Some(csrt) = contextual_signature_return_type {
                contextual_type = self.get_iteration_type_of_generator_function_return_type(
                    kind,
                    csrt,
                    is_async_generator,
                );
            }
            t = Some(self.get_widened_literal_like_type_for_contextual_type(ty, contextual_type));
        }
        t
    }

    // checker.go:20775
    pub(crate) fn create_generator_type(
        &mut self,
        yield_type: P<Type>,
        return_type: P<Type>,
        next_type: P<Type>,
        is_async_generator: bool,
    ) -> P<Type> {
        let resolver = if is_async_generator {
            self.async_iteration_types_resolver
        } else {
            self.sync_iteration_types_resolver
        };
        let global_generator_type = resolver.get_global_generator_type(self);
        let yield_type = resolver
            .resolve_iteration_type(self, yield_type, None /*errorNode*/)
            .unwrap_or(self.unknown_type);
        let return_type = resolver
            .resolve_iteration_type(self, return_type, None /*errorNode*/)
            .unwrap_or(self.unknown_type);
        if global_generator_type == self.empty_generic_type {
            // Fall back to the global IterableIterator type.
            let global_iterable_iterator_type = resolver.get_global_iterable_iterator_type(self);
            if global_iterable_iterator_type != self.empty_generic_type {
                return self.create_type_from_generic_global_type(
                    global_iterable_iterator_type,
                    &[yield_type, return_type, next_type],
                );
            }
            // The global Generator type doesn't exist, so report an error
            resolver.get_global_iterable_iterator_type_checked(self);
            return self.empty_object_type;
        }
        self.create_type_from_generic_global_type(
            global_generator_type,
            &[yield_type, return_type, next_type],
        )
    }

    // checker.go:20793
    pub(crate) fn report_errors_from_widening(
        &mut self,
        declaration: P<Node>,
        t: P<Type>,
        widening_kind: WideningKind,
    ) {
        if self.no_implicit_any
            && t.object_flags().intersects(ObjectFlags::ContainsWideningType)
            && (widening_kind == WideningKind::Normal
                || ast::is_function_like_declaration(declaration)
                    && self.should_report_errors_from_widening_with_contextual_signature(
                        declaration,
                        widening_kind,
                    ))
        {
            // Report implicit any error within type if possible, otherwise report error on declaration
            if !self.report_widening_errors_in_type(t) {
                self.report_implicit_any(declaration, t, widening_kind);
            }
        }
    }

    // checker.go:20804
    fn should_report_errors_from_widening_with_contextual_signature(
        &mut self,
        declaration: P<Node>,
        widening_kind: WideningKind,
    ) -> bool {
        let Some(signature) =
            self.get_contextual_signature_for_function_like_declaration(declaration)
        else {
            return true;
        };
        let mut return_type = self.get_return_type_of_signature(signature);
        let flags = get_function_flags(Some(declaration));
        match widening_kind {
            WideningKind::FunctionReturn => {
                if flags.intersects(FunctionFlags::Generator) {
                    return_type = self
                        .get_iteration_type_of_generator_function_return_type(
                            IterationTypeKind::Return,
                            return_type,
                            flags.intersects(FunctionFlags::Async),
                        )
                        .unwrap_or(return_type);
                } else if flags.intersects(FunctionFlags::Async) {
                    return_type =
                        self.get_awaited_type_no_alias(return_type).unwrap_or(return_type);
                }
                return self.is_generic_type(return_type);
            }
            WideningKind::GeneratorYield => {
                let yield_type = self.get_iteration_type_of_generator_function_return_type(
                    IterationTypeKind::Yield,
                    return_type,
                    flags.intersects(FunctionFlags::Async),
                );
                return yield_type.is_some() && self.is_generic_type(yield_type.unwrap());
            }
            WideningKind::GeneratorNext => {
                let next_type = self.get_iteration_type_of_generator_function_return_type(
                    IterationTypeKind::Next,
                    return_type,
                    flags.intersects(FunctionFlags::Async),
                );
                return next_type.is_some() && self.is_generic_type(next_type.unwrap());
            }
            WideningKind::Normal => {}
        }
        false
    }

    // Reports implicit any errors that occur as a result of widening 'null' and 'undefined'
    // to 'any'. A call to reportWideningErrorsInType is normally accompanied by a call to
    // getWidenedType. But in some cases getWidenedType is called without reporting errors
    // (type argument inference is an example).
    //
    // The return value indicates whether an error was in fact reported. The particular circumstances
    // are on a best effort basis. Currently, if the null or undefined that causes widening is inside
    // an object literal property (arbitrarily deeply), this function reports an error. If no error is
    // reported, reportImplicitAnyError is a suitable fallback to report a general error.
    // checker.go:20838
    fn report_widening_errors_in_type(&mut self, t: P<Type>) -> bool {
        let mut error_reported = false;
        if t.object_flags().intersects(ObjectFlags::ContainsWideningType) {
            if t.flags().intersects(TypeFlags::Union) {
                if t.types().iter().any(|s| self.is_empty_object_type(*s)) {
                    error_reported = true;
                } else {
                    let types = t.types();
                    error_reported = error_reported
                        || types.iter().any(|&s| self.report_widening_errors_in_type(s));
                }
            } else if self.is_array_or_tuple_type(t) {
                let type_arguments = self.get_type_arguments(t);
                error_reported = error_reported
                    || type_arguments.iter().any(|&s| self.report_widening_errors_in_type(s));
            } else if is_object_literal_type(t) {
                for p in self.get_properties_of_object_type(t).iter().copied() {
                    let s = self.get_type_of_symbol(p);
                    if s.object_flags().intersects(ObjectFlags::ContainsWideningType) {
                        error_reported = self.report_widening_errors_in_type(s);
                        if !error_reported {
                            // we need to account for property types coming from object literal type normalization in unions
                            let value_declaration = p.declarations().iter().copied().find(|d| {
                                let value_declaration = d.symbol().unwrap().value_declaration();
                                value_declaration.is_some_and(|vd| {
                                    vd.parent() == t.symbol().unwrap().value_declaration()
                                })
                            });
                            if let Some(value_declaration) = value_declaration {
                                let p_name = self.symbol_to_string(p);
                                let widened = self.get_widened_type(s);
                                let widened_name = self.type_to_string_exported(widened);
                                self.error(Some(value_declaration), &diagnostics::OBJECT_LITERAL_S_PROPERTY_0_IMPLICITLY_HAS_AN_1_TYPE, &[&p_name, &widened_name]);
                                error_reported = true;
                            }
                        }
                    }
                }
            }
        }
        error_reported
    }

    // checker.go:20876
    pub(crate) fn get_type_predicate_from_body(
        &mut self,
        fn_: P<Node>,
    ) -> Option<P<TypePredicate>> {
        match fn_.kind() {
            Kind::Constructor | Kind::GetAccessor | Kind::SetAccessor => return None,
            _ => {}
        }
        let function_flags = get_function_flags(Some(fn_));
        if function_flags != FunctionFlags::Normal {
            return None;
        }
        // Only attempt to infer a type predicate if there's exactly one return.
        let mut single_return: Option<P<Node>> = None;
        let body = fn_.body();
        if body.is_some() && !ast::is_block(body.unwrap()) {
            // arrow function
            single_return = body;
        } else {
            let bailed_early = ast::for_each_return_statement(body.unwrap(), |return_statement| {
                if single_return.is_some() || return_statement.expression().is_none() {
                    return true;
                }
                single_return = return_statement.expression();
                false
            });
            if bailed_early || single_return.is_none() || self.function_has_implicit_return(fn_) {
                return None;
            }
        }
        self.check_if_expression_refines_any_parameter(fn_, single_return.unwrap())
    }

    // checker.go:20906
    fn check_if_expression_refines_any_parameter(
        &mut self,
        fn_: P<Node>,
        expr: P<Node>,
    ) -> Option<P<TypePredicate>> {
        let expr = ast::skip_parentheses(expr);
        let return_type = self.check_expression_cached(expr);
        if !return_type.flags().intersects(TypeFlags::Boolean) {
            return None;
        }
        if let Some(result) = fn_.parameters().iter().enumerate().find_map(|(i, &param)| {
            let init_type = self.get_type_of_symbol(param.symbol().unwrap());
            if init_type.flags().intersects(TypeFlags::Boolean)
                || !ast::is_identifier(param.name().unwrap())
                || self.is_symbol_assigned(param.symbol().unwrap())
                || is_rest_parameter(param)
            {
                // Refining "x: boolean" to "x is true" or "x is false" isn't useful.
                return None;
            }
            let true_type = self.check_if_expression_refines_parameter(fn_, expr, param, init_type);
            if let Some(true_type) = true_type {
                return Some(Some(Self::new_type_predicate(
                    TypePredicateKind::Identifier,
                    param.name().unwrap().text(),
                    Some(i),
                    Some(true_type),
                )));
            }

            None
        }) {
            return result;
        }
        None
    }

    // checker.go:20926
    fn check_if_expression_refines_parameter(
        &mut self,
        fn_: P<Node>,
        expr: P<Node>,
        param: P<Node>,
        init_type: P<Type>,
    ) -> Option<P<Type>> {
        let mut antecedent = get_flow_node_of_node(expr);
        if antecedent.is_none() && ast::is_return_statement(expr.parent().unwrap()) {
            antecedent = get_flow_node_of_node(expr.parent().unwrap());
        }
        if antecedent.is_none() {
            antecedent =
                Some(P::new(FlowNode::new(FlowFlags::Start, None, None, ast::NO_SOURCE_TEXT)));
        }
        let true_condition = P::new(FlowNode::new(
            FlowFlags::TrueCondition,
            Some(expr),
            antecedent,
            ast::NO_SOURCE_TEXT,
        ));
        let true_type = self.get_flow_type_of_reference_ex(
            param.name().unwrap(),
            init_type,
            init_type,
            Some(fn_),
            Some(true_condition),
        );
        if true_type == init_type {
            return None;
        }
        // "x is T" means that x is T if and only if it returns true. If it returns false then x is not T.
        // This means that if the function is called with an argument of type trueType, there can't be anything left in the `else` branch. It must reduce to `never`.
        let false_condition = P::new(FlowNode::new(
            FlowFlags::FalseCondition,
            Some(expr),
            antecedent,
            ast::NO_SOURCE_TEXT,
        ));
        let false_flow_type = self.get_flow_type_of_reference_ex(
            param.name().unwrap(),
            init_type,
            true_type,
            Some(fn_),
            Some(false_condition),
        );
        let false_subtype = self.get_reduced_type(false_flow_type);
        if false_subtype.flags().intersects(TypeFlags::Never) {
            return Some(true_type);
        }
        None
    }

    // checker.go:20949
    pub(crate) fn add_optional_type_marker(&mut self, t: P<Type>) -> P<Type> {
        if self.strict_null_checks {
            return self.get_union_type(&[t, self.optional_type]);
        }
        t
    }

    // checker.go:20956
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn instantiate_signature(
        &mut self,
        sig: P<Signature>,
        m: Option<P<TypeMapper>>,
    ) -> P<Signature> {
        let erase_type_parameters = m == Some(self.permissive_mapper);
        // Go passes m through unchanged; instantiateSignatureEx requires a mapper.
        self.instantiate_signature_ex(
            sig,
            m.unwrap(),
            erase_type_parameters, /*eraseTypeParameters*/
        )
    }

    // checker.go:20960
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn instantiate_signature_ex(
        &mut self,
        sig: P<Signature>,
        m: P<TypeMapper>,
        erase_type_parameters: bool,
    ) -> P<Signature> {
        let mut m = m;
        let mut fresh_type_parameters: Vec<P<Type>> = Vec::new();
        if !sig.type_parameters().is_empty() && !erase_type_parameters {
            // First create a fresh set of type parameters, then include a mapping from the old to the
            // new type parameters in the mapper function. Finally store this mapper in the new type
            // parameters such that we can use it when instantiating constraints.
            fresh_type_parameters =
                sig.type_parameters().iter().map(|tp| self.clone_type_parameter(*tp)).collect();
            m = Self::combine_type_mappers(
                Some(new_type_mapper(sig.type_parameters(), alloc_slice(&fresh_type_parameters))),
                m,
            );
            for tp in &fresh_type_parameters {
                tp.as_type_parameter().mapper.set(Some(m));
            }
        }
        // Don't compute resolvedReturnType and resolvedTypePredicate now,
        // because using `mapper` now could trigger inferences to become fixed. (See `createInferenceContext`.)
        // See GH#17600.
        let this_parameter = sig.this_parameter().map(|s| self.instantiate_symbol(s, Some(m)));
        let parameters = self.instantiate_symbols(sig.parameters.get(), m);
        let result = self.new_signature(
            sig.flags.get() & SignatureFlags::PropagatingFlags,
            sig.declaration.get(),
            &fresh_type_parameters,
            this_parameter,
            &parameters,
            None, /*resolvedReturnType*/
            None, /*resolvedTypePredicate*/
            sig.min_argument_count.get(),
        );
        result.target.set(Some(sig));
        result.mapper.set(Some(m));
        result
    }

    // checker.go:20983
    pub(crate) fn instantiate_index_info(
        &mut self,
        info: P<IndexInfo>,
        m: Option<P<TypeMapper>>,
    ) -> P<IndexInfo> {
        let new_value_type = self.instantiate_type(info.value_type(), m);
        if new_value_type == info.value_type() {
            return info;
        }
        Self::new_index_info(
            info.key_type(),
            new_value_type,
            info.is_readonly.get(),
            info.declaration.get(),
            info.components.get(),
        )
    }

    // checker.go:20991
    pub(crate) fn resolve_anonymous_type_members(&mut self, t: P<Type>) {
        let d = t.as_object_type();
        if let Some(target) = d.target.get() {
            self.set_structured_type_members(t, None, &[], &[], &[]);
            let mapper = d.mapper.get().unwrap();
            let target_properties = self.get_properties_of_object_type(target);
            let members = self.create_instantiated_symbol_table(target_properties, mapper);
            let target_call_signatures = self.get_signatures_of_type(target, SignatureKind::Call);
            let call_signatures = self.instantiate_signatures(target_call_signatures, mapper);
            let target_construct_signatures =
                self.get_signatures_of_type(target, SignatureKind::Construct);
            let construct_signatures =
                self.instantiate_signatures(target_construct_signatures, mapper);
            let target_index_infos = self.get_index_infos_of_type(target);
            let index_infos = self.instantiate_index_infos(target_index_infos, mapper);
            self.set_structured_type_members(
                t,
                members,
                &call_signatures,
                &construct_signatures,
                &index_infos,
            );
            return;
        }
        let symbol = self.get_merged_symbol(t.symbol().unwrap());
        if symbol.flags().intersects(SymbolFlags::TypeLiteral) {
            self.set_structured_type_members(t, None, &[], &[], &[]);
            let members = self.get_members_of_symbol(symbol);
            let call_signatures = self.get_signatures_of_symbol(
                members.and_then(|m| m.lookup(INTERNAL_SYMBOL_NAME_CALL)),
            );
            let construct_signatures = self
                .get_signatures_of_symbol(members.and_then(|m| m.lookup(INTERNAL_SYMBOL_NAME_NEW)));
            let index_infos = self.get_index_infos_of_symbol(symbol);
            self.set_structured_type_members(
                t,
                members,
                &call_signatures,
                &construct_signatures,
                &index_infos,
            );
            return;
        }
        // Combinations of function, class, enum and module
        let mut members = self.get_exports_of_symbol(symbol);
        let mut index_infos: Vec<P<IndexInfo>> = Vec::new();
        if symbol == self.global_this_symbol {
            let vars_only = SymbolTable::new();
            if let Some(members) = members {
                for p in members.values() {
                    if !(p.flags().intersects(SymbolFlags::BlockScoped)
                        || p.flags().intersects(SymbolFlags::ValueModule)
                            && !p.declarations().is_empty()
                            && p.declarations().iter().all(|d| ast::is_ambient_module(*d)))
                    {
                        vars_only.set(p.name(), p);
                    }
                }
            }
            members = Some(vars_only);
        }
        let mut base_constructor_index_info = None;
        self.set_structured_type_members(t, members, &[], &[], &[]);
        if symbol.flags().intersects(SymbolFlags::Class) {
            let class_type = self.get_declared_type_of_class_or_interface(symbol);
            let base_constructor_type = self.get_base_constructor_type_of_class(class_type);
            if base_constructor_type
                .flags()
                .intersects(TypeFlags::Object | TypeFlags::Intersection | TypeFlags::TypeVariable)
            {
                members = members.map(|m| m.clone_table());
                let base_properties = self.get_properties_of_type(base_constructor_type);
                Self::add_inherited_members(members, base_properties);
                self.set_structured_type_members(t, members, &[], &[], &[]);
            } else if base_constructor_type == self.any_type {
                base_constructor_index_info = Some(self.any_base_type_index_info);
            }
        }
        let index_symbol = members.and_then(|m| m.lookup(INTERNAL_SYMBOL_NAME_INDEX));
        if let Some(index_symbol) = index_symbol {
            let values = members.map(|m| m.values()).unwrap_or_default();
            index_infos = self.get_index_infos_of_index_symbol(index_symbol, &values);
        } else {
            if let Some(info) = base_constructor_index_info {
                index_infos.push(info);
            }
            if symbol.flags().intersects(SymbolFlags::Enum)
                && (self.get_declared_type_of_symbol(symbol).flags().intersects(TypeFlags::Enum)
                    || d.properties().iter().any(|prop| {
                        self.get_type_of_symbol(*prop).flags().intersects(TypeFlags::NumberLike)
                    }))
            {
                index_infos.push(self.enum_number_index_info);
            }
        }
        d.set_index_infos(alloc_vec(index_infos));
        // We resolve the members before computing the signatures because a signature may use
        // typeof with a qualified name expression that circularly references the type we are
        // in the process of resolving (see issue #6072). The temporarily empty signature list
        // will never be observed because a qualified name can't reference signatures.
        if symbol.flags().intersects(SymbolFlags::Function | SymbolFlags::Method) {
            let signatures = self.get_signatures_of_symbol(Some(symbol));
            d.set_signatures(alloc_vec(signatures));
            d.set_call_signature_count(d.signatures().len());
        }
        // And likewise for construct signatures for classes
        if symbol.flags().intersects(SymbolFlags::Class) {
            let class_type = self.get_declared_type_of_class_or_interface(symbol);
            let constructor_symbol =
                symbol.members().and_then(|m| m.lookup(INTERNAL_SYMBOL_NAME_CONSTRUCTOR));
            let mut construct_signatures = self.get_signatures_of_symbol(constructor_symbol);
            if construct_signatures.is_empty() {
                construct_signatures = self.get_default_construct_signatures(class_type);
            }
            let mut signatures = d.signatures().to_vec();
            signatures.extend(construct_signatures);
            d.set_signatures(alloc_vec(signatures));
        }
        // The members were resolved above with no signatures or index infos yet: drop what isWeakType memoized meanwhile.
        t.object_flags.set(t.object_flags_lazy() & !ObjectFlags::WeakTypeMemo);
    }

    // checker.go:21070
    fn create_instantiated_symbol_table(
        &mut self,
        symbols: &[P<Symbol>],
        m: P<TypeMapper>,
    ) -> Option<P<SymbolTable>> {
        if symbols.is_empty() {
            return None;
        }
        let result = SymbolTable::with_capacity(symbols.len());
        for &symbol in symbols {
            let instantiated = self.instantiate_symbol(symbol, Some(m));
            result.set(symbol.name(), instantiated);
        }
        Some(result)
    }

    // checker.go:21081
    pub(crate) fn instantiate_symbol_table(
        &mut self,
        symbols: Option<P<SymbolTable>>,
        m: P<TypeMapper>,
    ) -> Option<P<SymbolTable>> {
        let symbols = symbols.filter(|s| !s.is_empty())?;
        let result = SymbolTable::with_capacity(symbols.len());
        for (id, symbol) in symbols.entries() {
            if self.is_named_member(symbol, id) {
                let instantiated = self.instantiate_symbol(symbol, Some(m));
                result.set(id, instantiated);
            }
        }
        Some(result)
    }

    // checker.go:21094
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn instantiate_symbol(
        &mut self,
        symbol: P<Symbol>,
        m: Option<P<TypeMapper>>,
    ) -> P<Symbol> {
        if self.is_symbol_unaffected_by_instantiation(symbol, m) {
            return symbol;
        }
        self.new_instantiated_symbol(symbol, m)
    }

    // Can change from false to true once the type of the symbol is resolved.
    pub(crate) fn is_symbol_unaffected_by_instantiation(
        &mut self,
        symbol: P<Symbol>,
        m: Option<P<TypeMapper>>,
    ) -> bool {
        let links = self.value_symbol_links.get(symbol);
        if m.is_some_and(|m| m.maps_this_only()) && is_thisless(symbol) {
            return true;
        }
        // If the type of the symbol is already resolved, and if that type could not possibly
        // be affected by instantiation, simply return the symbol itself.
        if let Some(resolved_type) = links.resolved_type.get()
            && !self.could_contain_type_variables(resolved_type)
        {
            if !symbol.flags().intersects(SymbolFlags::SetAccessor) {
                return true;
            }
            // If we're a setter, check writeType.
            if let Some(write_type) = links.write_type()
                && !self.could_contain_type_variables(write_type)
            {
                return true;
            }
        }
        false
    }

    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn new_instantiated_symbol(
        &mut self,
        symbol: P<Symbol>,
        m: Option<P<TypeMapper>>,
    ) -> P<Symbol> {
        let mut symbol = symbol;
        let mut m = m;
        let links = self.value_symbol_links.get(symbol);
        if symbol.check_flags().intersects(CheckFlags::Instantiated) {
            // If symbol being instantiated is itself a instantiation, fetch the original target and combine the
            // type mappers. This ensures that original type identities are properly preserved and that aliases
            // always reference a non-aliases.
            symbol = links.target().unwrap();
            m = Some(Self::combine_type_mappers(links.mapper(), m.unwrap()));
        }
        // Create a transient symbol with its own flags and links, sharing the source's data.
        let result = self.new_shared_data_symbol(symbol);
        result.set_check_flags(
            CheckFlags::Instantiated
                | symbol.check_flags()
                    & (CheckFlags::Readonly
                        | CheckFlags::Late
                        | CheckFlags::OptionalParameter
                        | CheckFlags::RestParameter),
        );
        #[cfg(feature = "site-counts")]
        {
            self.inst_symbol_sites.insert(result, std::panic::Location::caller());
            crate::sitecount::hit("inst-symbol-created", "");
        }
        let result_links = self.value_symbol_links.get(result);
        result_links.set_target(Some(symbol));
        result_links.set_mapper(m);
        result_links.set_name_type(links.name_type());
        result
    }
}

// Returns true if the parameter or class/interface member given by the symbol is free of "this" references. The
// function may return false for symbols that are actually free of "this" references because it is not
// feasible to perform a complete analysis in all cases. In particular, property members with types
// inferred from their initializers and function members with inferred return types are conservatively
// assumed not to be free of "this" references.
// checker.go:21139
fn is_thisless(symbol: P<Symbol>) -> bool {
    let declarations = symbol.declarations();
    if declarations.len() == 1 {
        let declaration = declarations[0];
        match declaration.kind() {
            Kind::Parameter => return is_thisless_variable_like_declaration(declaration),
            Kind::PropertyDeclaration | Kind::PropertySignature => {
                return is_thisless_variable_like_declaration(declaration);
            }
            Kind::MethodDeclaration
            | Kind::MethodSignature
            | Kind::Constructor
            | Kind::GetAccessor
            | Kind::SetAccessor => {
                return is_thisless_function_like_declaration(declaration);
            }
            _ => {}
        }
    }
    false
}

// A variable-like declaration is free of this references if it has a type annotation
// that is thisless, or if it has no type annotation and no initializer (and is thus of type any).
// checker.go:21158
fn is_thisless_variable_like_declaration(node: P<Node>) -> bool {
    let type_node = node.type_node();
    if let Some(type_node) = type_node {
        return is_thisless_type(type_node);
    }
    node.initializer().is_none()
}

// A type is free of this references if it's the any, string, number, boolean, symbol, or void keyword, a string
// literal type, an array with an element type that is free of this references, or a type reference that is
// free of this references.
// checker.go:21169
fn is_thisless_type(node: P<Node>) -> bool {
    match node.kind() {
        Kind::AnyKeyword
        | Kind::UnknownKeyword
        | Kind::StringKeyword
        | Kind::NumberKeyword
        | Kind::BigIntKeyword
        | Kind::BooleanKeyword
        | Kind::SymbolKeyword
        | Kind::ObjectKeyword
        | Kind::VoidKeyword
        | Kind::UndefinedKeyword
        | Kind::NeverKeyword
        | Kind::LiteralType => return true,
        Kind::ArrayType => return is_thisless_type(node.as_array_type_node().element_type),
        Kind::TypeReference => return node.type_arguments().iter().all(|n| is_thisless_type(*n)),
        _ => {}
    }
    false
}

// A function-like declaration is considered free of `this` references if it has a return type
// annotation that is free of this references and if each parameter is thisless and if
// each type parameter (if present) is thisless.
// checker.go:21185
fn is_thisless_function_like_declaration(node: P<Node>) -> bool {
    let return_type = node.type_node();
    (ast::is_constructor_declaration(node) || return_type.is_some_and(is_thisless_type))
        && node.parameters().iter().all(|n| is_thisless_variable_like_declaration(*n))
        && node.type_parameters().iter().all(|n| is_thisless_type_parameter(*n))
}

// A type parameter is thisless if its constraint is thisless, or if it has no constraint. */
// checker.go:21193
fn is_thisless_type_parameter(node: P<Node>) -> bool {
    let constraint = node.as_type_parameter_declaration().constraint;
    constraint.is_none() || is_thisless_type(constraint.unwrap())
}

impl Checker {
    // checker.go:21198
    fn get_default_construct_signatures(&mut self, class_type: P<Type>) -> Vec<P<Signature>> {
        let base_constructor_type = self.get_base_constructor_type_of_class(class_type);
        let base_signatures =
            self.get_signatures_of_type(base_constructor_type, SignatureKind::Construct);
        let declaration = ast::get_class_like_declaration_of_symbol(class_type.symbol().unwrap());
        let is_abstract =
            declaration.is_some_and(|d| ast::has_syntactic_modifier(d, ModifierFlags::Abstract));
        if base_signatures.is_empty() {
            let flags = if is_abstract {
                SignatureFlags::Construct | SignatureFlags::Abstract
            } else {
                SignatureFlags::Construct
            };
            let local_type_parameters = class_type.as_interface_type().local_type_parameters();
            return vec![self.new_signature(
                flags,
                None,
                local_type_parameters,
                None,
                &[],
                Some(class_type),
                None,
                0,
            )];
        }
        let base_type_node = get_base_type_node_of_class(class_type);
        let is_java_script = declaration.is_some_and(ast::is_in_js_file);
        let type_arguments = self.get_type_arguments_from_node(base_type_node.unwrap());
        let type_arg_count = type_arguments.len();
        let mut result = Vec::new();
        result.extend(base_signatures.iter().filter_map(|&base_sig| {
            let min_type_argument_count =
                Self::get_min_type_argument_count(base_sig.type_parameters());
            let type_param_count = base_sig.type_parameters().len();
            if is_java_script
                || type_arg_count >= min_type_argument_count && type_arg_count <= type_param_count
            {
                let sig = if type_param_count != 0 {
                    let filled = self.fill_missing_type_arguments(
                        &type_arguments,
                        base_sig.type_parameters(),
                        min_type_argument_count,
                        is_java_script,
                    );
                    self.create_signature_instantiation(base_sig, &filled)
                } else {
                    self.clone_signature(base_sig)
                };
                sig.set_type_parameters(Some(
                    class_type.as_interface_type().local_type_parameters(),
                ));
                sig.resolved_return_type.set(Some(class_type));
                if is_abstract {
                    sig.flags.set(sig.flags.get() | SignatureFlags::Abstract);
                } else {
                    sig.flags.set(sig.flags.get() & !SignatureFlags::Abstract);
                }
                Some(sig)
            } else {
                None
            }
        }));
        result
    }

    // checker.go:21235
    pub(crate) fn resolve_mapped_type_members(&mut self, t: P<Type>) {
        let census_span = self.census_begin(crate::workcensus::Cat::MappedResolve, || {
            crate::workcensus::CKey::OptNode(
                t.as_mapped_type().target.get().unwrap_or(t).as_mapped_type().declaration.get(),
            )
        });
        let members = SymbolTable::new();
        // Resolve upfront such that recursive references see an empty object type.
        self.set_structured_type_members(t, None, &[], &[], &[]);
        // While the empty-members guard is in place, avoid lazy shortcuts that would resolve
        // mapped types after it has been filled (issue #649).
        self.mapped_types_resolving += 1;
        // In { [P in K]: T }, we refer to P as the type parameter type, K as the constraint type,
        // and T as the template type.
        let type_parameter = self.get_type_parameter_from_mapped_type(t);
        let constraint_type = self.get_constraint_type_from_mapped_type(t);
        let mapped_type = t.as_mapped_type().target.get().unwrap_or(t);
        let name_type = self.get_name_type_from_mapped_type(mapped_type);
        let should_link_prop_declarations =
            self.get_mapped_type_name_type_kind(mapped_type) != MappedTypeNameTypeKind::Remapping;
        let template_type = self.get_template_type_from_mapped_type(mapped_type);
        let modifiers_type_of_mapped = self.get_modifiers_type_from_mapped_type(t);
        let modifiers_type = self.get_apparent_type(modifiers_type_of_mapped);
        // The 'T' in 'keyof T'
        let template_modifiers = get_mapped_type_modifiers(t);
        let include = TypeFlags::StringOrNumberLiteralOrUnique;
        let from_seed =
            self.lazy_mapped_tables.base_get(&t).map(|frozen| Rc::new((**frozen).clone()));
        let lazy = self.lazy_mapped_tables.remove(&t).or(from_seed);
        if lazy.is_some() {
            self.lazy_member_stats.mapped_tables_resolved_in_full += 1;
        }
        let mut state = MappedTypeMembersState {
            t,
            members,
            index_infos: Vec::new(),
            type_parameter,
            name_type,
            should_link_prop_declarations,
            template_type,
            modifiers_type,
            template_modifiers,
            lazy,
        };
        if Self::is_mapped_type_with_keyof_constraint_declaration(t) {
            // We have a { [P in keyof T]: X }
            self.for_each_mapped_type_property_key_type_and_index_signature_key_type(
                modifiers_type,
                include,
                false, /*stringsOnly*/
                |c, key_type| {
                    mapped_type_add_member_for_key_type(c, &mut state, key_type);
                },
            );
        } else {
            let lower_bound = self.get_lower_bound_of_key_type(constraint_type);
            for_each_type(self, lower_bound, |c, key_type| {
                mapped_type_add_member_for_key_type(c, &mut state, key_type);
            });
        }
        let mut index_infos = std::mem::take(&mut state.index_infos);
        if let Some(lazy) = state.lazy.as_ref().filter(|lazy| lazy.index_infos_ready.get()) {
            index_infos = lazy.index_infos.get().to_vec();
        }
        self.mapped_types_resolving -= 1;
        self.set_structured_type_members(t, Some(members), &[], &[], &index_infos);
        if let Some(timing) = self.census_end(census_span) {
            let n = members.len();
            if timing.incl_ns > crate::workcensus::slow_threshold_ns() {
                let s = self.type_to_string_exported(t);
                let m = self.type_to_string_exported(modifiers_type_of_mapped);
                eprintln!(
                    "census slow mapped type: {:.1} ms, {n} keys: {} | modifiers {}",
                    timing.incl_ns as f64 / 1e6,
                    &s[..s.len().min(300)],
                    &m[..m.len().min(300)]
                );
            }
            let key = crate::workcensus::CKey::Mapped(
                mapped_type.as_mapped_type().declaration.get(),
                crate::workcensus::bucket(n),
            );
            self.census.as_mut().unwrap().record(
                crate::workcensus::Cat::MappedResolve,
                key,
                timing,
                n as u64,
                0,
                0,
            );
        }
    }

    // #64526 (behind `Checker::lazy_members`)
    fn append_mapped_type_index_info(
        &mut self,
        index_infos: Vec<P<IndexInfo>>,
        t: P<Type>,
        modifiers_type: P<Type>,
        type_parameter: P<Type>,
        template_type: P<Type>,
        template_modifiers: MappedTypeModifiers,
        key_type: P<Type>,
        prop_name_type: P<Type>,
    ) -> Vec<P<IndexInfo>> {
        let mut index_infos = index_infos;
        if !self.is_valid_index_key_type(prop_name_type)
            && !prop_name_type.flags().intersects(TypeFlags::Any | TypeFlags::Enum)
        {
            return index_infos;
        }
        let mut index_key_type = prop_name_type;
        if prop_name_type.flags().intersects(TypeFlags::Any | TypeFlags::String) {
            index_key_type = self.string_type;
        } else if prop_name_type.flags().intersects(TypeFlags::Number | TypeFlags::Enum) {
            index_key_type = self.number_type;
        }
        let mapper = append_type_mapping(t.as_mapped_type().mapper.get(), type_parameter, key_type);
        let prop_type = self.instantiate_type(template_type, Some(mapper));
        // SAFETY: made here for this one instantiation.
        unsafe { recycle_mapping(mapper, true) };
        let modifiers_index_info = self.get_applicable_index_info(modifiers_type, prop_name_type);
        let is_readonly = template_modifiers.intersects(MappedTypeModifiers::IncludeReadonly)
            || !template_modifiers.intersects(MappedTypeModifiers::ExcludeReadonly)
                && modifiers_index_info.is_some_and(|i| i.is_readonly.get());
        let index_info = Self::new_index_info(index_key_type, prop_type, is_readonly, None, &[]);
        self.append_index_info(&mut index_infos, index_info, true /*union*/)
    }

    fn new_mapped_type_member(
        &mut self,
        t: P<Type>,
        modifiers_type: P<Type>,
        template_modifiers: MappedTypeModifiers,
        should_link_prop_declarations: bool,
        key_type: P<Type>,
        prop_name_type: P<Type>,
        prop_name: &str,
    ) -> P<Symbol> {
        let mut modifiers_prop = None;
        if is_type_usable_as_property_name(key_type) {
            modifiers_prop =
                self.get_property_of_type(modifiers_type, &get_property_name_from_type(key_type));
        }
        let is_optional = template_modifiers.intersects(MappedTypeModifiers::IncludeOptional)
            || !template_modifiers.intersects(MappedTypeModifiers::ExcludeOptional)
                && modifiers_prop.is_some_and(|p| p.flags().intersects(SymbolFlags::Optional));
        let is_readonly = template_modifiers.intersects(MappedTypeModifiers::IncludeReadonly)
            || !template_modifiers.intersects(MappedTypeModifiers::ExcludeReadonly)
                && modifiers_prop.is_some()
                && self.is_readonly_symbol(modifiers_prop.unwrap());
        let strip_optional = self.strict_null_checks
            && !is_optional
            && modifiers_prop.is_some_and(|p| p.flags().intersects(SymbolFlags::Optional));
        let mut late_flag = CheckFlags::None;
        if let Some(modifiers_prop) = modifiers_prop {
            late_flag = modifiers_prop.check_flags() & CheckFlags::Late;
        }
        let prop = self.new_symbol(
            SymbolFlags::Property
                | if is_optional { SymbolFlags::Optional } else { SymbolFlags::None },
            if prop_name_type.flags().intersects(TypeFlags::StringLiteral)
                && let Some(LiteralValue::String(name)) =
                    prop_name_type.as_literal_type().value.get()
            {
                name
            } else {
                alloc_str(prop_name)
            },
        );
        prop.set_check_flags(
            late_flag
                | CheckFlags::Mapped
                | if is_readonly { CheckFlags::Readonly } else { CheckFlags::None }
                | if strip_optional { CheckFlags::StripOptional } else { CheckFlags::None },
        );
        let value_links = self.value_symbol_links.get(prop);
        value_links.set_containing_type(Some(t));
        value_links.set_name_type(Some(prop_name_type));
        let mapped_links = self.mapped_symbol_links.get(prop);
        mapped_links.key_type.set(Some(key_type));
        if let Some(modifiers_prop) = modifiers_prop {
            mapped_links.synthetic_origin.set(Some(modifiers_prop));
            if should_link_prop_declarations {
                prop.set_declarations_static(modifiers_prop.declarations());
            }
        }
        prop
    }
}

// Mapped types { [P in keyof T]: X } where T is an object type get a lazy table that creates
// members and index infos as they are asked for, the way resolveMappedTypeMembers would.
pub struct LazyMappedTable {
    type_parameter: P<Type>,
    modifiers_type: P<Type>,
    template_modifiers: MappedTypeModifiers,
    should_link_prop_declarations: bool,
    members: RefCell<FxHashMap<String, Option<P<Symbol>>>>,
    index_infos: Cell<&'static [P<IndexInfo>]>,
    index_infos_ready: Cell<bool>,
    resolving: Cell<bool>,
}

/// Shared graph: forks copy the seed's tables in parallel, so the copy must not touch the borrow flag.
impl Clone for LazyMappedTable {
    fn clone(&self) -> Self {
        LazyMappedTable {
            type_parameter: self.type_parameter,
            modifiers_type: self.modifiers_type,
            template_modifiers: self.template_modifiers,
            should_link_prop_declarations: self.should_link_prop_declarations,
            // SAFETY: the seed checker is frozen: nothing borrows its tables mutably any more.
            members: RefCell::new(unsafe { self.members.try_borrow_unguarded() }.unwrap().clone()),
            index_infos: Cell::new(self.index_infos.get()),
            index_infos_ready: Cell::new(self.index_infos_ready.get()),
            resolving: Cell::new(self.resolving.get()),
        }
    }
}

impl Checker {
    #[inline]
    pub(crate) fn get_lazy_mapped_table(
        &mut self,
        t: P<Type>,
    ) -> Option<std::rc::Rc<LazyMappedTable>> {
        if !self.lazy_members
            || t.object_flags_lazy() & (ObjectFlags::Mapped | ObjectFlags::MembersResolved)
                != ObjectFlags::Mapped
        {
            return None;
        }
        self.get_lazy_mapped_table_worker(t)
    }

    #[inline(never)]
    fn get_lazy_mapped_table_worker(&mut self, t: P<Type>) -> Option<std::rc::Rc<LazyMappedTable>> {
        if let Some(lazy) = self.lazy_mapped_tables.own.get(&t) {
            return Some(Rc::clone(lazy));
        }
        if let Some(frozen) = self.lazy_mapped_tables.base_get(&t) {
            // Shared graph: the seed's table is filled lazily, so the fork takes its own copy.
            let copy = Rc::new((**frozen).clone());
            self.lazy_mapped_tables.insert(t, Rc::clone(&copy));
            return Some(copy);
        }
        if !Self::is_mapped_type_with_keyof_constraint_declaration(t) {
            return None;
        }
        // The same steps as resolveMappedTypeMembers before it creates members.
        let type_parameter = self.get_type_parameter_from_mapped_type(t);
        self.get_constraint_type_from_mapped_type(t);
        let mapped_type = t.as_mapped_type().target.get().unwrap_or(t);
        if self.get_name_type_from_mapped_type(mapped_type).is_some() {
            crate::sitecount::hit("mapped-table-refused", "name-type");
            return None;
        }
        let should_link_prop_declarations =
            self.get_mapped_type_name_type_kind(mapped_type) != MappedTypeNameTypeKind::Remapping;
        let modifiers_type_of_mapped = self.get_modifiers_type_from_mapped_type(t);
        let modifiers_type = self.get_apparent_type(modifiers_type_of_mapped);
        if !modifiers_type.flags().intersects(TypeFlags::Object)
            || t.object_flags_lazy().intersects(ObjectFlags::MembersResolved)
        {
            #[cfg(feature = "site-counts")]
            crate::sitecount::hit(
                "mapped-table-refused",
                if t.object_flags_lazy().intersects(ObjectFlags::MembersResolved) {
                    "resolved-meanwhile"
                } else {
                    type_kind_label(modifiers_type.flags(), modifiers_type.object_flags_lazy())
                },
            );
            return None;
        }
        let lazy = std::rc::Rc::new(LazyMappedTable {
            type_parameter,
            modifiers_type,
            template_modifiers: get_mapped_type_modifiers(t),
            should_link_prop_declarations,
            members: RefCell::new(FxHashMap::default()),
            index_infos: Cell::new(&[]),
            index_infos_ready: Cell::new(false),
            resolving: Cell::new(false),
        });
        self.lazy_mapped_tables.insert(t, Rc::clone(&lazy));
        self.lazy_member_stats.mapped_tables_created += 1;
        Some(lazy)
    }

    // Go returns (member, ok); None here is !ok.
    #[expect(
        clippy::option_option,
        reason = "The outer option records whether a result is available; the inner option records an absent value."
    )]
    pub(crate) fn get_lazy_mapped_type_member(
        &mut self,
        t: P<Type>,
        lazy: &std::rc::Rc<LazyMappedTable>,
        name: &str,
    ) -> Option<Option<P<Symbol>>> {
        let existing = lazy.members.borrow().get(name).copied();
        if let Some(existing) = existing {
            self.lazy_member_stats.mapped_member_lookups += 1;
            return Some(existing);
        }
        // Recursive lookups see no member, as they would while resolveMappedTypeMembers runs.
        lazy.members.borrow_mut().insert(name.to_string(), None);
        let modifiers_prop = self.get_member_of_structured_type(lazy.modifiers_type, name);
        if t.object_flags_lazy().intersects(ObjectFlags::MembersResolved) {
            return Some(t.as_structured_type().members().and_then(|m| m.lookup(name)));
        }
        let mut member = None;
        if let Some(modifiers_prop) = modifiers_prop
            && self.is_named_member(modifiers_prop, name)
        {
            let key_type = self.get_literal_type_from_property(
                modifiers_prop,
                TypeFlags::StringOrNumberLiteralOrUnique,
                false,
            );
            if !is_type_usable_as_property_name(key_type)
                || get_property_name_from_type(key_type) != name
            {
                lazy.members.borrow_mut().remove(name);
                return None;
            }
            self.lazy_member_stats.mapped_members_created += 1;
            member = Some(self.new_mapped_type_member(
                t,
                lazy.modifiers_type,
                lazy.template_modifiers,
                lazy.should_link_prop_declarations,
                key_type,
                key_type,
                name,
            ));
        }
        lazy.members.borrow_mut().insert(name.to_string(), member);
        self.lazy_member_stats.mapped_member_lookups += 1;
        Some(member)
    }

    pub(crate) fn get_lazy_mapped_type_index_infos(
        &mut self,
        t: P<Type>,
        lazy: &std::rc::Rc<LazyMappedTable>,
    ) -> &'static [P<IndexInfo>] {
        if !lazy.index_infos_ready.get() {
            if lazy.resolving.get() {
                // Recursive requests see no index infos, as they would while resolveMappedTypeMembers runs.
                return &[];
            }
            lazy.resolving.set(true);
            // Resolving the template can reach this type again (through an alias that refers back to it).
            // resolveMappedTypeMembers does it after setting empty members, so it is done here under the guard,
            // not when the table is created, where each recursive lookup would start over.
            let template_type = self
                .get_template_type_from_mapped_type(t.as_mapped_type().target.get().unwrap_or(t));
            let mut index_infos: Vec<P<IndexInfo>> = Vec::new();
            index_infos = self.get_index_infos_of_type(lazy.modifiers_type).iter().fold(
                index_infos,
                |mut index_infos, info| {
                    index_infos = self.append_mapped_type_index_info(
                        index_infos,
                        t,
                        lazy.modifiers_type,
                        lazy.type_parameter,
                        template_type,
                        lazy.template_modifiers,
                        info.key_type(),
                        info.key_type(),
                    );

                    index_infos
                },
            );
            lazy.resolving.set(false);
            if t.object_flags_lazy().intersects(ObjectFlags::MembersResolved) {
                return t.as_structured_type().index_infos();
            }
            self.lazy_member_stats.mapped_index_info_queries += 1;
            lazy.index_infos.set(alloc_vec(index_infos));
            lazy.index_infos_ready.set(true);
        }
        lazy.index_infos.get()
    }
}

/// Locals captured by the closures `addMemberForKeyTypeWorker` / `addMemberForKeyType` in Go's `resolveMappedTypeMembers`.
struct MappedTypeMembersState {
    t: P<Type>,
    members: P<SymbolTable>,
    index_infos: Vec<P<IndexInfo>>,
    type_parameter: P<Type>,
    name_type: Option<P<Type>>,
    should_link_prop_declarations: bool,
    template_type: P<Type>,
    modifiers_type: P<Type>,
    template_modifiers: MappedTypeModifiers,
    lazy: Option<std::rc::Rc<LazyMappedTable>>,
}

fn mapped_type_add_member_for_key_type_worker(
    c: &mut Checker,
    st: &mut MappedTypeMembersState,
    key_type: P<Type>,
    prop_name_type: P<Type>,
) {
    let t = st.t;
    // If the current iteration type constituent is a string literal type, create a property.
    // Otherwise, for type string create a string index signature.
    if is_type_usable_as_property_name(prop_name_type) {
        let prop_name = get_property_name_from_type(prop_name_type);
        // String enum members from separate enums with identical values
        // are distinct types with the same property name. Make the resulting
        // property symbol's name type be the union of those enum member types.
        if let Some(existing_prop) = st.members.lookup(&prop_name) {
            let value_links = c.value_symbol_links.get(existing_prop);
            let name_type = c.get_union_type(&[value_links.name_type().unwrap(), prop_name_type]);
            value_links.set_name_type(Some(name_type));
            let mapped_links = c.mapped_symbol_links.get(existing_prop);
            let key_type_union =
                c.get_union_type(&[mapped_links.key_type.get().unwrap(), key_type]);
            mapped_links.key_type.set(Some(key_type_union));
        } else if let Some(member) = st
            .lazy
            .as_ref()
            .and_then(|lazy| lazy.members.borrow().get(&*prop_name).copied().flatten())
        {
            st.members.set(member.name(), member);
        } else {
            let prop = c.new_mapped_type_member(
                t,
                st.modifiers_type,
                st.template_modifiers,
                st.should_link_prop_declarations,
                key_type,
                prop_name_type,
                &prop_name,
            );
            st.members.set(prop.name(), prop);
        }
    } else if st.lazy.as_ref().is_none_or(|lazy| !lazy.index_infos_ready.get()) {
        let index_infos = std::mem::take(&mut st.index_infos);
        st.index_infos = c.append_mapped_type_index_info(
            index_infos,
            t,
            st.modifiers_type,
            st.type_parameter,
            st.template_type,
            st.template_modifiers,
            key_type,
            prop_name_type,
        );
    }
}

fn mapped_type_add_member_for_key_type(
    c: &mut Checker,
    st: &mut MappedTypeMembersState,
    key_type: P<Type>,
) {
    let mut prop_name_type = key_type;
    if let Some(name_type) = st.name_type {
        let mapper =
            append_type_mapping(st.t.as_mapped_type().mapper.get(), st.type_parameter, key_type);
        prop_name_type = c.instantiate_type(name_type, Some(mapper));
        // SAFETY: made here for this one instantiation.
        unsafe { recycle_mapping(mapper, true) };
    }
    for_each_type(c, prop_name_type, |c, t| {
        mapped_type_add_member_for_key_type_worker(c, st, key_type, t);
    });
}

impl Checker {
    // checker.go:21325
    #[inline(never)] // out of get_type_of_symbol, which then needs no frame for its common cases
    pub(crate) fn get_type_of_mapped_symbol(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.value_symbol_links.get(symbol);
        if links.resolved_type.get().is_none() && self.census_on() {
            let span = self.census_begin(crate::workcensus::Cat::MappedProp, || {
                let mt = links.containing_type().unwrap();
                crate::workcensus::CKey::OptNode(
                    mt.as_mapped_type()
                        .target
                        .get()
                        .unwrap_or(mt)
                        .as_mapped_type()
                        .declaration
                        .get(),
                )
            });
            let r = self.get_type_of_mapped_symbol_worker(symbol);
            let timing = self.census_end(span).unwrap();
            let mapped_type = links.containing_type().unwrap();
            let decl = mapped_type
                .as_mapped_type()
                .target
                .get()
                .unwrap_or(mapped_type)
                .as_mapped_type()
                .declaration
                .get();
            self.census.as_mut().unwrap().record(
                crate::workcensus::Cat::MappedProp,
                crate::workcensus::CKey::OptNode(decl),
                timing,
                0,
                0,
                0,
            );
            return r;
        }
        self.get_type_of_mapped_symbol_worker(symbol)
    }

    fn get_type_of_mapped_symbol_worker(&mut self, symbol: P<Symbol>) -> P<Type> {
        let links = self.value_symbol_links.get(symbol);
        if links.resolved_type.get().is_none() {
            let mapped_type = links.containing_type().unwrap();
            if !self.push_type_resolution(symbol.into(), TypeSystemPropertyName::Type) {
                mapped_type.as_mapped_type().contains_error.set(true);
                return self.error_type;
            }
            let template_type = self.get_template_type_from_mapped_type(
                mapped_type.as_mapped_type().target.get().unwrap_or(mapped_type),
            );
            let type_parameter = self.get_type_parameter_from_mapped_type(mapped_type);
            let key_type = self.mapped_symbol_links.get(symbol).key_type.get().unwrap();
            let mapper = append_type_mapping(
                mapped_type.as_mapped_type().mapper.get(),
                type_parameter,
                key_type,
            );
            let mut prop_type = self.instantiate_type(template_type, Some(mapper));
            // SAFETY: made here for this one instantiation.
            unsafe { recycle_mapping(mapper, true) };
            // When creating an optional property in strictNullChecks mode, if 'undefined' isn't assignable to the
            // type, we include 'undefined' in the type. Similarly, when creating a non-optional property in strictNullChecks
            // mode, if the underlying property is optional we remove 'undefined' from the type.
            if self.strict_null_checks
                && symbol.flags().intersects(SymbolFlags::Optional)
                && !self.maybe_type_of_kind(prop_type, TypeFlags::Undefined | TypeFlags::Void)
            {
                prop_type = self.get_optional_type(prop_type, true /*isProperty*/);
            } else if symbol.check_flags().intersects(CheckFlags::StripOptional) {
                prop_type = self.remove_missing_or_undefined_type(prop_type);
            }
            if self.pop_type_resolution() {
                if links.resolved_type.get().is_none() {
                    links.resolved_type.set(Some(prop_type));
                }
            } else {
                if links.resolved_type.get().is_none() {
                    links.resolved_type.set(Some(self.error_type));
                }
                let symbol_name = self.symbol_to_string(symbol);
                let mapped_type_name = self.type_to_string_exported(mapped_type);
                self.error(
                    self.current_node,
                    &diagnostics::TYPE_OF_PROPERTY_0_CIRCULARLY_REFERENCES_ITSELF_IN_MAPPED_TYPE_1,
                    &[&symbol_name, &mapped_type_name],
                );
            }
        }
        links.resolved_type.get().unwrap()
    }

    // Return the lower bound of the key type in a mapped type. Intuitively, the lower
    // bound includes those keys that are known to always be present, for example because
    // because of constraints on type parameters (e.g. 'keyof T' for a constrained T).
    // checker.go:21362
    pub(crate) fn get_lower_bound_of_key_type(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Index) {
            let t = self.get_apparent_type(t.as_index_type().target.get().unwrap());
            if Self::is_generic_tuple_type(t) {
                return self.get_known_keys_of_tuple_type(t);
            }
            return self.get_index_type(t);
        } else if t.flags().intersects(TypeFlags::Conditional) {
            let root = t.as_conditional_type().root.get().unwrap();
            if root.is_distributive.get() {
                let check_type = t.as_conditional_type().check_type.get().unwrap();
                let constraint = self.get_lower_bound_of_key_type(check_type);
                if constraint != check_type {
                    let mapper = prepend_type_mapping(
                        root.check_type.get().unwrap(),
                        constraint,
                        t.as_conditional_type().mapper.get(),
                    );
                    return self.get_conditional_type_instantiation(
                        t, mapper, false, /*forConstraint*/
                        None,
                    );
                }
            }
            return t;
        } else if t.flags().intersects(TypeFlags::Union) {
            return self
                .map_type_ex(
                    t,
                    |c, t| Some(c.get_lower_bound_of_key_type(t)),
                    true, /*noReductions*/
                )
                .unwrap();
        } else if t.flags().intersects(TypeFlags::Intersection) {
            // Similarly to getTypeFromIntersectionTypeNode, we preserve the special string & {}, number & {},
            // and bigint & {} intersections that are used to prevent subtype reduction in union types.
            let types = t.types();
            if types.len() == 2
                && types[0]
                    .flags()
                    .intersects(TypeFlags::String | TypeFlags::Number | TypeFlags::BigInt)
                && types[1] == self.empty_type_literal_type
            {
                return t;
            }
            let new_types = same_map(t.types(), |t| self.get_lower_bound_of_key_type(*t));
            return self.get_intersection_type(&new_types);
        }
        t
    }

    // checker.go:21393
    pub(crate) fn resolve_union_type_members(&mut self, t: P<Type>) {
        // The members and properties collections are empty for union types. To get all properties of a union
        // type use getPropertiesOfType (only the language service uses this).
        let mut call_signature_lists = Vec::with_capacity(t.types().len());
        call_signature_lists.extend(t.types().iter().map(|&t| {
            if t == self.global_function_type {
                vec![self.unknown_signature]
            } else {
                self.get_signatures_of_type(t, SignatureKind::Call).to_vec()
            }
        }));
        let mut call_signatures = self.get_union_signatures(&call_signature_lists);
        if call_signatures.is_empty() {
            call_signatures = self.get_array_member_call_signatures(t);
        }
        let mut construct_signature_lists = Vec::with_capacity(t.types().len());
        construct_signature_lists.extend(
            t.types()
                .iter()
                .map(|&t| self.get_signatures_of_type(t, SignatureKind::Construct).to_vec()),
        );
        let construct_signatures = self.get_union_signatures(&construct_signature_lists);
        let index_infos = self.get_union_index_infos(t.types());
        self.set_structured_type_members(
            t,
            None,
            &call_signatures,
            &construct_signatures,
            &index_infos,
        );
    }

    // checker.go:21412
    fn get_array_member_call_signatures(&mut self, t: P<Type>) -> Vec<P<Signature>> {
        // Check if union is exclusively instantiations of a member of the global Array or ReadonlyArray type.
        let mut member_name: &'static str = "";
        for (i, &t) in t.types().iter().enumerate() {
            if !t.object_flags().intersects(ObjectFlags::Instantiated)
                || t.symbol().is_none()
                || t.symbol().unwrap().parent().is_none()
                || !self.is_array_or_tuple_symbol(t.symbol().unwrap().parent())
            {
                return Vec::new();
            }
            if i == 0 {
                member_name = t.symbol().unwrap().name();
            } else if member_name != t.symbol().unwrap().name() {
                return Vec::new();
            }
        }
        // Transform the type from `(A[] | B[])["member"]` to `(A | B)[]["member"]` (since we pretend array is covariant anyway).
        let array_arg = self
            .map_type(t, |c, t| {
                let global = if c.is_readonly_array_symbol(t.symbol().unwrap().parent()) {
                    c.global_readonly_array_type
                } else {
                    c.global_array_type
                };
                Some(c.get_mapped_type(
                    global.as_interface_type().type_parameters()[0],
                    t.mapper().unwrap(),
                ))
            })
            .unwrap();
        let readonly =
            some_type(self, t, |c, t| c.is_readonly_array_symbol(t.symbol().unwrap().parent()));
        let array_type = self.create_array_type_ex(array_arg, readonly);
        let member_type = self.get_type_of_property_of_type(array_type, member_name).unwrap();
        self.get_signatures_of_type(member_type, SignatureKind::Call).to_vec()
    }

    // checker.go:21435
    fn is_array_or_tuple_symbol(&mut self, symbol: Option<P<Symbol>>) -> bool {
        let Some(symbol) = symbol else {
            return false;
        };
        if self.global_array_type.symbol().is_none()
            || self.global_readonly_array_type.symbol().is_none()
        {
            return false;
        }
        let global_array_symbol = self.global_array_type.symbol().unwrap();
        let global_readonly_array_symbol = self.global_readonly_array_type.symbol().unwrap();
        self.get_symbol_if_same_reference(symbol, global_array_symbol).is_some()
            || self.get_symbol_if_same_reference(symbol, global_readonly_array_symbol).is_some()
    }

    // checker.go:21442
    fn is_readonly_array_symbol(&mut self, symbol: Option<P<Symbol>>) -> bool {
        let Some(symbol) = symbol else {
            return false;
        };
        let Some(global_readonly_array_symbol) = self.global_readonly_array_type.symbol() else {
            return false;
        };
        self.get_symbol_if_same_reference(symbol, global_readonly_array_symbol).is_some()
    }

    // The signatures of a union type are those signatures that are present in each of the constituent types.
    // Generic signatures must match exactly, but non-generic signatures are allowed to have extra optional
    // parameters and may differ in return types. When signatures differ in return types, the resulting return
    // type is the union of the constituent return types.
    // checker.go:21453
    pub(crate) fn get_union_signatures(
        &mut self,
        signature_lists: &[Vec<P<Signature>>],
    ) -> Vec<P<Signature>> {
        let mut result: Vec<P<Signature>> = Vec::new();
        let mut index_with_length_over_one = 0usize;
        let mut count_length_over_one = 0;
        for (i, signatures) in signature_lists.iter().enumerate() {
            if signatures.is_empty() {
                return Vec::new();
            }
            if signatures.len() > 1 {
                index_with_length_over_one = i;
                count_length_over_one += 1;
            }
            for &signature in signatures {
                // Only process signatures with parameter lists that aren't already in the result list
                if result.is_empty()
                    || self
                        .find_matching_signature(
                            &result, signature, false, /*partialMatch*/
                            false, /*ignoreThisTypes*/
                            true,  /*ignoreReturnTypes*/
                        )
                        .is_none()
                {
                    let union_signatures =
                        self.find_matching_signatures(signature_lists, signature, i);
                    if !union_signatures.is_empty() {
                        let mut s = signature;
                        // Union the result types when more than one signature matches
                        if union_signatures.len() > 1 {
                            let mut this_parameter = signature.this_parameter();
                            let first_this_parameter_of_union_signatures =
                                first_non_nil(&union_signatures, |sig| sig.this_parameter());
                            if let Some(first_this) = first_this_parameter_of_union_signatures {
                                let mut this_types = Vec::new();
                                this_types.extend(union_signatures.iter().filter_map(|&sig| {
                                    sig.this_parameter().map(|tp| self.get_type_of_symbol(tp))
                                }));
                                let this_type = self.get_intersection_type(&this_types);
                                this_parameter =
                                    Some(self.create_symbol_with_type(first_this, Some(this_type)));
                            }
                            s = self.create_union_signature(signature, &union_signatures);
                            s.set_this_parameter(this_parameter);
                        }
                        result.push(s);
                    }
                }
            }
        }
        if result.is_empty() && count_length_over_one <= 1 {
            // No sufficiently similar signature existed to subsume all the other signatures in the union - time to see if we can make a single
            // signature that handles all of them. We only do this when there are overloads in only one constituent. (Overloads are conditional in
            // nature and having overloads in multiple constituents would necessitate making a power set of signatures from the type, whose
            // ordering would be non-obvious)
            let master_list = &signature_lists[index_with_length_over_one];
            let mut results: Option<Vec<P<Signature>>> = Some(master_list.clone());
            for (i, signatures) in signature_lists.iter().enumerate() {
                // Go compares slice identity (core.Same); the lists are distinct Vecs here, so compare by position.
                if i != index_with_length_over_one {
                    let signature = signatures[0];
                    let current = results.take().unwrap();
                    if !signature.type_parameters().is_empty()
                        && current.iter().any(|s| {
                            !s.type_parameters().is_empty()
                                && !self.compare_type_parameters_identical(
                                    signature.type_parameters(),
                                    s.type_parameters(),
                                )
                        })
                    {
                        results = None;
                    } else {
                        results = Some(
                            current
                                .iter()
                                .map(|sig| {
                                    self.combine_union_or_intersection_member_signatures(
                                        *sig, signature, true, /*isUnion*/
                                    )
                                })
                                .collect(),
                        );
                    }
                    if results.is_none() {
                        break;
                    }
                }
            }
            result = results.unwrap_or_default();
        }
        result
    }

    // checker.go:21524
    pub(crate) fn combine_union_or_intersection_member_signatures(
        &mut self,
        left: P<Signature>,
        right: P<Signature>,
        is_union: bool,
    ) -> P<Signature> {
        let mut type_params = left.type_parameters();
        if type_params.is_empty() {
            type_params = right.type_parameters();
        }
        let mut param_mapper = None;
        if !left.type_parameters().is_empty() && !right.type_parameters().is_empty() {
            // We just use the type parameter defaults from the first signature
            param_mapper = Some(new_type_mapper(right.type_parameters(), left.type_parameters()));
        }
        let mut flags = (left.flags.get() | right.flags.get())
            & (SignatureFlags::PropagatingFlags & !SignatureFlags::HasRestParameter);
        let declaration = left.declaration.get();
        let params =
            self.combine_union_or_intersection_parameters(left, right, param_mapper, is_union);
        let last_param = params.last().copied();
        if last_param.is_some_and(|p| p.check_flags().intersects(CheckFlags::RestParameter)) {
            flags |= SignatureFlags::HasRestParameter;
        }
        let this_param = self.combine_union_or_intersection_this_param(
            left.this_parameter(),
            right.this_parameter(),
            param_mapper,
            is_union,
        );
        let min_arg_count = left.min_argument_count.get().max(right.min_argument_count.get());
        let result = self.new_signature(
            flags,
            declaration,
            type_params,
            this_param,
            &params,
            None,
            None,
            min_arg_count,
        );
        let mut left_signatures: Vec<P<Signature>> =
            if let Some(composite) = left.composite().filter(|c| c.is_union.get()) {
                composite.signatures.get().to_vec()
            } else {
                vec![left]
            };
        left_signatures.push(right);
        result.set_composite(Some(P::new(CompositeSignature {
            is_union: Cell::new(is_union),
            signatures: Cell::new(alloc_vec(left_signatures)),
        })));
        if let Some(param_mapper) = param_mapper {
            if left.composite().is_some_and(|c| c.is_union.get() == is_union)
                && left.mapper.get().is_some()
            {
                result
                    .mapper
                    .set(Some(Self::combine_type_mappers(left.mapper.get(), param_mapper)));
            } else {
                result.mapper.set(Some(param_mapper));
            }
        } else if left.composite().is_some_and(|c| c.is_union.get() == is_union) {
            result.mapper.set(left.mapper.get());
        }
        result
    }

    // checker.go:21563
    fn combine_union_or_intersection_parameters(
        &mut self,
        left: P<Signature>,
        right: P<Signature>,
        mapper: Option<P<TypeMapper>>,
        is_union: bool,
    ) -> Vec<P<Symbol>> {
        let left_count = self.get_parameter_count(left);
        let right_count = self.get_parameter_count(right);
        let (longest_count, longest, shorter) = if left_count >= right_count {
            (left_count, left, right)
        } else {
            (right_count, right, left)
        };
        let either_has_effective_rest =
            self.has_effective_rest_parameter(left) || self.has_effective_rest_parameter(right);
        let needs_extra_rest_element =
            either_has_effective_rest && !self.has_effective_rest_parameter(longest);
        let mut params: Vec<P<Symbol>> =
            Vec::with_capacity(longest_count + usize::from(needs_extra_rest_element));
        for i in 0..longest_count {
            let mut longest_param_type = self.try_get_type_at_position(longest, i).unwrap();
            if longest == right {
                longest_param_type = self.instantiate_type(longest_param_type, mapper);
            }
            let mut shorter_param_type =
                self.try_get_type_at_position(shorter, i).unwrap_or(self.unknown_type);
            if shorter == right {
                shorter_param_type = self.instantiate_type(shorter_param_type, mapper);
            }
            let combined_param_type = self.get_union_or_intersection_type(
                &[longest_param_type, shorter_param_type],
                !is_union,
                UnionReduction::Literal,
            );
            let is_rest_param =
                either_has_effective_rest && !needs_extra_rest_element && i == (longest_count - 1);
            let is_optional = i >= self.get_min_argument_count(longest)
                && i >= self.get_min_argument_count(shorter);
            let mut left_name = String::new();
            let mut right_name = String::new();
            if i < left_count {
                left_name = self.get_parameter_name_at_position(left, i);
            }
            if i < right_count {
                right_name = self.get_parameter_name_at_position(right, i);
            }
            let mut param_name = String::new();
            if left_name == right_name {
                param_name = left_name;
            } else if left_name.is_empty() {
                param_name = right_name;
            } else if right_name.is_empty() {
                param_name = left_name;
            }
            if param_name.is_empty() {
                param_name = format!("arg{i}");
            }
            let param_symbol = self.new_symbol_ex(
                SymbolFlags::FunctionScopedVariable
                    | if is_optional && !is_rest_param {
                        SymbolFlags::Optional
                    } else {
                        SymbolFlags::None
                    },
                alloc_str(&param_name),
                if is_rest_param {
                    CheckFlags::RestParameter
                } else if is_optional {
                    CheckFlags::OptionalParameter
                } else {
                    CheckFlags::None
                },
            );
            let links = self.value_symbol_links.get(param_symbol);
            if is_rest_param {
                links.resolved_type.set(Some(self.create_array_type(combined_param_type)));
            } else {
                links.resolved_type.set(Some(combined_param_type));
            }
            params.push(param_symbol);
        }
        if needs_extra_rest_element {
            let rest_param_symbol = self.new_symbol_ex(
                SymbolFlags::FunctionScopedVariable,
                "args",
                CheckFlags::RestParameter,
            );
            let links = self.value_symbol_links.get(rest_param_symbol);
            let type_at_position = self.get_type_at_position(shorter, longest_count);
            links.resolved_type.set(Some(self.create_array_type(type_at_position)));
            if shorter == right {
                links
                    .resolved_type
                    .set(Some(self.instantiate_type(links.resolved_type.get().unwrap(), mapper)));
            }
            params.push(rest_param_symbol);
        }
        params
    }

    // checker.go:21629
    fn combine_union_or_intersection_this_param(
        &mut self,
        left: Option<P<Symbol>>,
        right: Option<P<Symbol>>,
        mapper: Option<P<TypeMapper>>,
        is_union: bool,
    ) -> Option<P<Symbol>> {
        let Some(left) = left else {
            return right;
        };
        let Some(right) = right else {
            return Some(left);
        };
        // A signature `this` type might be a read or a write position... It's very possible that it should be invariant
        // and we should refuse to merge signatures if there are `this` types and they do not match. However, so as to be
        // permissive when calling, for now, we'll intersect the `this` types just like we do for param types in union signatures.
        let left_type = self.get_type_of_symbol(left);
        let right_type = self.get_type_of_symbol(right);
        let right_instantiated = self.instantiate_type(right_type, mapper);
        let this_type = self.get_union_or_intersection_type(
            &[left_type, right_instantiated],
            !is_union,
            UnionReduction::Literal,
        );
        Some(self.create_symbol_with_type(left, Some(this_type)))
    }

    // checker.go:21643
    pub(crate) fn resolve_intersection_type_members(&mut self, t: P<Type>) {
        // The members and properties collections are empty for intersection types. To get all properties of an
        // intersection type use getPropertiesOfType (only the language service uses this).
        let mut call_signatures: Vec<P<Signature>> = Vec::new();
        let mut construct_signatures: Vec<P<Signature>> = Vec::new();
        let mut index_infos: Vec<P<IndexInfo>> = Vec::new();
        let types = t.types();
        let (mixin_flags, mixin_count) = self.find_mixins(types);
        for (i, &t) in types.iter().enumerate() {
            // When an intersection type contains mixin constructor types, the construct signatures from
            // those types are discarded and their return types are mixed into the return types of all
            // other construct signatures in the intersection type. For example, the intersection type
            // '{ new(...args: any[]) => A } & { new(s: string) => B }' has a single construct signature
            // 'new(s: string) => A & B'.
            if !mixin_flags[i] {
                let mut signatures =
                    self.get_signatures_of_type(t, SignatureKind::Construct).to_vec();
                if !signatures.is_empty() && mixin_count > 0 {
                    signatures = signatures
                        .iter()
                        .map(|&s| {
                            let clone = self.clone_signature(s);
                            let return_type = self.get_return_type_of_signature(s);
                            clone.resolved_return_type.set(Some(self.include_mixin_type(
                                return_type,
                                types,
                                &mixin_flags,
                                i,
                            )));
                            clone
                        })
                        .collect();
                }
                construct_signatures = self.append_signatures(&construct_signatures, &signatures);
            }
            let call = self.get_signatures_of_type(t, SignatureKind::Call);
            call_signatures = self.append_signatures(&call_signatures, call);
            index_infos = self.get_index_infos_of_type(t).iter().copied().fold(
                index_infos,
                |mut index_infos, info| {
                    index_infos =
                        self.append_index_info(&mut index_infos, info, false /*union*/);

                    index_infos
                },
            );
        }
        self.set_structured_type_members(
            t,
            None,
            &call_signatures,
            &construct_signatures,
            &index_infos,
        );
    }

    // checker.go:21676
    fn append_signatures(
        &mut self,
        signatures: &[P<Signature>],
        new_signatures: &[P<Signature>],
    ) -> Vec<P<Signature>> {
        let mut signatures = signatures.to_vec();
        for &sig in new_signatures {
            let mut all_different = true;
            if !signatures.is_empty() {
                for element in &signatures {
                    let s = *element;
                    if self.compare_signatures_identical(
                        s,
                        sig,
                        false, /*partialMatch*/
                        false, /*ignoreThisTypes*/
                        false, /*ignoreReturnTypes*/
                        super::checker::Checker::compare_types_identical,
                    ) != Ternary::FALSE
                    {
                        all_different = false;
                        break;
                    }
                }
            }
            if signatures.is_empty() || all_different {
                signatures.push(sig);
            }
        }
        signatures
    }

    // checker.go:21687
    fn append_index_info(
        &mut self,
        index_infos: &mut [P<IndexInfo>],
        new_info: P<IndexInfo>,
        union: bool,
    ) -> Vec<P<IndexInfo>> {
        for info_slot in index_infos.iter_mut() {
            let info = *info_slot;
            if info.key_type.get() == new_info.key_type.get() {
                let (value_type, is_readonly) = if union {
                    (
                        self.get_union_type(&[info.value_type(), new_info.value_type()]),
                        info.is_readonly.get() || new_info.is_readonly.get(),
                    )
                } else {
                    (
                        self.get_intersection_type(&[info.value_type(), new_info.value_type()]),
                        info.is_readonly.get() && new_info.is_readonly.get(),
                    )
                };
                *info_slot =
                    Self::new_index_info(info.key_type(), value_type, is_readonly, None, &[]);
                return index_infos.to_vec();
            }
        }
        let mut result = index_infos.to_vec();
        result.push(new_info);
        result
    }

    // checker.go:21706
    pub(crate) fn find_mixins(&mut self, types: &[P<Type>]) -> (Vec<bool>, usize) {
        let mut mixin_flags: Vec<bool> =
            types.iter().map(|t| self.is_mixin_constructor_type(*t)).collect();
        let mut constructor_type_count = 0;
        let mut mixin_count = 0;
        let mut first_mixin_index = None;
        for (i, &t) in types.iter().enumerate() {
            if !self.get_signatures_of_type(t, SignatureKind::Construct).is_empty() {
                constructor_type_count += 1;
            }
            if mixin_flags[i] {
                if first_mixin_index.is_none() {
                    first_mixin_index = Some(i);
                }
                mixin_count += 1;
            }
        }
        if constructor_type_count > 0 && constructor_type_count == mixin_count {
            mixin_flags[first_mixin_index.expect("constructor mixins are non-empty")] = false;
            mixin_count -= 1;
        }
        (mixin_flags, mixin_count)
    }

    // checker.go:21728
    fn include_mixin_type(
        &mut self,
        t: P<Type>,
        types: &[P<Type>],
        mixin_flags: &[bool],
        index: usize,
    ) -> P<Type> {
        let mut mixed_types: Vec<P<Type>> = Vec::new();
        mixed_types.extend((0..types.len()).filter_map(|i| {
            if i == index {
                Some(t)
            } else if mixin_flags[i] {
                let sig = self.get_signatures_of_type(types[i], SignatureKind::Construct)[0];
                Some(self.get_return_type_of_signature(sig))
            } else {
                None
            }
        }));
        self.get_intersection_type(&mixed_types)
    }

    /**
     * If the given type is an object type and that type has a property by the given name,
     * return the symbol for that property. Otherwise return undefined.
     */
    // checker.go:21744
    #[inline]
    pub(crate) fn get_property_of_object_type(
        &mut self,
        t: P<Type>,
        name: &str,
    ) -> Option<P<Symbol>> {
        self.get_property_of_object_type_hashed(t, HashedName::new(name))
    }

    pub(crate) fn get_property_of_object_type_hashed(
        &mut self,
        t: P<Type>,
        key: HashedName<'_>,
    ) -> Option<P<Symbol>> {
        if t.flags().intersects(TypeFlags::Object) {
            let resolved = self.resolve_structured_type_members(t);
            let symbol = resolved.members().and_then(|m| m.lookup_hashed(key));
            if let Some(symbol) = symbol
                && self.symbol_is_value(symbol)
            {
                return Some(symbol);
            }
        }
        None
    }

    // checker.go:21755
    pub(crate) fn get_property_of_union_or_intersection_type(
        &mut self,
        t: P<Type>,
        name: &str,
        skip_object_function_property_augment: bool,
    ) -> Option<P<Symbol>> {
        let prop =
            self.get_union_or_intersection_property(t, name, skip_object_function_property_augment);
        // We need to filter out partial properties in union types
        if prop.is_some_and(|p| p.check_flags().intersects(CheckFlags::ReadPartial)) {
            return None;
        }
        prop
    }
}
