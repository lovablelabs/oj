use crate::*;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;

// Non-function declarations in nodebuilderimpl.go:1851-end (hand-ported by printer-foundation in nodebuilder_types.rs):
//   type SignatureToSignatureDeclarationOptions (nodebuilderimpl.go:1858)
//   const MAX_REVERSE_MAPPED_NESTING_INSPECTION_DEPTH (nodebuilderimpl.go:2372)
//   type propertyNameNodeKind (nodebuilderimpl.go:2448)
//   const (nodebuilderimpl.go:2450): propertyNameNodeKindIdentifier, propertyNameNodeKindNumericLiteral,
//     propertyNameNodeKindStringLiteral

// Go `b.ctx.approximateLength += n`.
fn add_approximate_length(b: &NodeBuilderImpl, n: usize) {
    let ctx = b.ctx();
    ctx.approximate_length.set(ctx.approximate_length.get() + n);
}

// Go `defer`: runs the closure when dropped.
struct Defer(Box<dyn FnMut()>);

impl Drop for Defer {
    fn drop(&mut self) {
        (self.0)();
    }
}

impl NodeBuilderImpl {
    // nodebuilderimpl.go:1864
    pub(crate) fn signature_to_signature_declaration_helper(
        &self,
        c: &mut Checker,
        signature: P<Signature>,
        kind: Kind,
        options: Option<P<SignatureToSignatureDeclarationOptions>>,
    ) -> P<Node> {
        let mut type_parameters: Vec<P<Node>> = vec![];

        let (expanded_params, mut cleanup) = self.enter_signature_scope(c, signature);
        add_approximate_length(self, 3);
        // Usually a signature contributes a few more characters than this, but 3 is the minimum

        if self.ctx().flags.get().intersects(Flags::WriteTypeArgumentsOfSignature)
            && signature.target().is_some()
            && signature.mapper.get().is_some()
            && !signature.target().unwrap().type_parameters().is_empty()
        {
            type_parameters.extend(
                signature.target().unwrap().type_parameters().iter().filter_map(|&parameter| {
                    let t = c.instantiate_type(parameter, signature.mapper.get());
                    // Go appends a possibly-nil node; a nil element cannot be represented in a Rust node list.
                    self.type_to_type_node(c, Some(t))
                }),
            );
        } else {
            type_parameters.extend(
                signature
                    .type_parameters()
                    .iter()
                    .map(|&parameter| self.type_parameter_to_declaration(c, parameter)),
            );
        }

        let mut restore_flags = self.save_restore_flags(c);
        self.ctx().flags.set(self.ctx().flags.get() & !Flags::SuppressAnyReturnType);
        // If the expanded parameter list had a variadic in a non-trailing position, don't expand it
        let last_expanded_param = expanded_params.last().copied();
        let has_non_trailing_rest = expanded_params.iter().any(|&p| {
            Some(p) != last_expanded_param && p.check_flags().intersects(CheckFlags::RestParameter)
        });
        let parameter_symbols: Vec<P<Symbol>> =
            if has_non_trailing_rest { signature.parameters().to_vec() } else { expanded_params };
        let mut parameters: Vec<P<Node>> = parameter_symbols
            .iter()
            .map(|&parameter| {
                self.symbol_to_parameter_declaration(c, parameter, kind == Kind::Constructor)
            })
            .collect();
        let this_parameter = if self.ctx().flags.get().intersects(Flags::OmitThisParameter) {
            None
        } else {
            self.try_get_this_parameter_declaration(c, signature)
        };
        if let Some(this_parameter) = this_parameter {
            parameters.insert(0, this_parameter);
        }
        restore_flags(c);

        let mut return_type_node = self.serialize_return_type_for_signature(c, signature, true);

        let mut modifiers: Vec<P<Node>> = vec![];
        if let Some(options) = options {
            modifiers = options.modifiers.to_vec();
        }
        if (kind == Kind::ConstructorType) && signature.flags().intersects(SignatureFlags::Abstract)
        {
            let flags = ast::modifiers_to_flags(&modifiers);
            modifiers =
                create_modifiers_from_modifier_flags(flags | ModifierFlags::Abstract, |k| {
                    self.f.new_modifier(k)
                });
        }

        let param_list = self.f.new_node_list(parameters);
        let mut type_param_list: Option<P<NodeList>> = None;
        if !type_parameters.is_empty() {
            type_param_list = Some(self.f.new_node_list(type_parameters));
        }
        let mut modifier_list: Option<P<ModifierList>> = None;
        if !modifiers.is_empty() {
            modifier_list = Some(self.f.new_modifier_list(modifiers));
        }
        let mut name: Option<P<Node>> = None;
        if let Some(options) = options {
            name = options.name;
        }
        let name = match name {
            Some(name) => name,
            None => self.f.new_identifier(""),
        };

        let node = match kind {
            Kind::CallSignature => self.f.new_call_signature_declaration(
                type_param_list,
                Some(param_list),
                return_type_node,
            ),
            Kind::ConstructSignature => self.f.new_construct_signature_declaration(
                type_param_list,
                Some(param_list),
                return_type_node,
            ),
            Kind::MethodSignature => {
                let mut question_token: Option<P<Node>> = None;
                if let Some(options) = options {
                    question_token = options.question_token;
                }
                self.f.new_method_signature_declaration(
                    modifier_list,
                    name,
                    question_token,
                    type_param_list,
                    Some(param_list),
                    return_type_node,
                )
            }
            Kind::MethodDeclaration => self.f.new_method_declaration(
                modifier_list,
                None, /*asteriskToken*/
                name,
                None, /*questionToken*/
                type_param_list,
                Some(param_list),
                return_type_node,
                None, /*fullSignature*/
                None, /*body*/
            ),
            Kind::Constructor => self.f.new_constructor_declaration(
                modifier_list,
                None, /*typeParamList*/
                Some(param_list),
                None, /*returnTypeNode*/
                None, /*fullSignature*/
                None, /*body*/
            ),
            Kind::GetAccessor => self.f.new_get_accessor_declaration(
                modifier_list,
                name,
                None, /*typeParamList*/
                Some(param_list),
                return_type_node,
                None, /*fullSignature*/
                None, /*body*/
            ),
            Kind::SetAccessor => self.f.new_set_accessor_declaration(
                modifier_list,
                name,
                None, /*typeParamList*/
                Some(param_list),
                None, /*returnTypeNode*/
                None, /*fullSignature*/
                None, /*body*/
            ),
            Kind::IndexSignature => self.f.new_index_signature_declaration(
                modifier_list,
                Some(param_list),
                return_type_node,
            ),
            // !!! JSDoc Support
            // case kind == ast.KindJSDocFunctionType:
            // 	node = b.f.NewJSDocFunctionType(parameters, returnTypeNode)
            Kind::FunctionType => {
                if return_type_node.is_none() {
                    return_type_node =
                        Some(self.f.new_type_reference_node(self.f.new_identifier(""), None));
                }
                self.f.new_function_type_node(type_param_list, Some(param_list), return_type_node)
            }
            Kind::ConstructorType => {
                if return_type_node.is_none() {
                    return_type_node =
                        Some(self.f.new_type_reference_node(self.f.new_identifier(""), None));
                }
                self.f.new_constructor_type_node(
                    modifier_list,
                    type_param_list,
                    Some(param_list),
                    return_type_node,
                )
            }
            Kind::FunctionDeclaration => {
                // TODO: assert name is Identifier
                self.f.new_function_declaration(
                    modifier_list,
                    None, /*asteriskToken*/
                    Some(name),
                    type_param_list,
                    Some(param_list),
                    return_type_node,
                    None, /*fullSignature*/
                    None, /*body*/
                )
            }
            Kind::FunctionExpression => {
                // TODO: assert name is Identifier
                self.f.new_function_expression(
                    modifier_list,
                    None, /*asteriskToken*/
                    Some(name),
                    type_param_list,
                    Some(param_list),
                    return_type_node,
                    None, /*fullSignature*/
                    Some(self.f.new_block(self.f.new_node_list(vec![]), false)),
                )
            }
            Kind::ArrowFunction => {
                self.f.new_arrow_function(
                    modifier_list,
                    type_param_list,
                    Some(param_list),
                    return_type_node,
                    None, /*fullSignature*/
                    None, /*equalsGreaterThanToken*/
                    Some(self.f.new_block(self.f.new_node_list(vec![]), false)),
                )
            }
            _ => panic!("Unhandled kind in signatureToSignatureDeclarationHelper"),
        };

        // !!! TODO: Smuggle type arguments of signatures out for quickinfo
        // if typeArguments != nil {
        // 	node.TypeArguments = b.f.NewNodeList(typeArguments)
        // }

        cleanup(c);
        node
    }
}

// Go's `getUniqAssociatedNamesFromTupleType` closure in getExpandedParameters.
fn get_uniq_associated_names_from_tuple_type(t: P<Type>, rest_symbol: P<Symbol>) -> Vec<String> {
    let element_infos = t.target().unwrap().as_tuple_type().element_infos.get();
    let mut names: Vec<String> = element_infos
        .iter()
        .enumerate()
        .map(|(i, &info)| Checker::get_tuple_element_label(info, Some(rest_symbol), i))
        .collect();
    if !names.is_empty() {
        let mut duplicates: Vec<usize> = vec![];
        let mut unique_names: FxHashSet<String> = FxHashSet::default();
        duplicates.extend(names.iter().enumerate().filter_map(|(i, name)| {
            if unique_names.contains(name) {
                Some(i)
            } else {
                unique_names.insert(name.clone());

                None
            }
        }));
        let mut counters: FxHashMap<String, usize> = FxHashMap::default();
        for &i in &duplicates {
            let mut counter = match counters.get(&names[i]) {
                Some(&counter) => counter,
                None => 1,
            };
            let name;
            loop {
                let candidate = format!("{}_{}", names[i], counter);
                if unique_names.contains(&candidate) {
                    counter += 1;
                } else {
                    unique_names.insert(candidate.clone());
                    name = candidate;
                    break;
                }
            }
            names[i] = name;
            counters.insert(names[i].clone(), counter + 1);
        }
    }
    names
}

// Go's `expandSignatureParametersWithTupleMembers` closure in getExpandedParameters.
fn expand_signature_parameters_with_tuple_members(
    c: &mut Checker,
    sig: P<Signature>,
    rest_type: P<Type>,
    rest_index: usize,
    rest_symbol: P<Symbol>,
) -> Vec<P<Symbol>> {
    let element_types = c.get_type_arguments(rest_type);
    let associated_names = get_uniq_associated_names_from_tuple_type(rest_type, rest_symbol);
    let rest_params: Vec<P<Symbol>> = element_types
        .iter()
        .enumerate()
        .map(|(i, &t)| {
            // Lookup the label from the individual tuple passed in before falling back to the signature `rest` parameter name
            // TODO: getTupleElementLabel can no longer fail, investigate if this lack of falliability meaningfully changes output
            // var name *string
            // if associatedNames != nil && associatedNames[i] != nil {
            // 	name = associatedNames[i]
            // } else {
            // 	name = c.getParameterNameAtPosition(sig, restIndex+i, restType)
            // }
            let name = alloc_str(&associated_names[i]);
            let flags = rest_type.target().unwrap().as_tuple_type().element_infos.get()[i].flags;
            let check_flags = if flags.intersects(ElementFlags::Variable) {
                CheckFlags::RestParameter
            } else if flags.intersects(ElementFlags::Optional) {
                CheckFlags::OptionalParameter
            } else {
                CheckFlags::None
            };
            let symbol = c.new_symbol_ex(SymbolFlags::FunctionScopedVariable, name, check_flags);
            let links = c.value_symbol_links.get(symbol);
            if flags.intersects(ElementFlags::Rest) {
                let array_type = c.create_array_type(t);
                links.resolved_type.set(Some(array_type));
            } else {
                links.resolved_type.set(Some(t));
            }
            symbol
        })
        .collect();
    let mut result = sig.parameters()[0..rest_index].to_vec();
    result.extend(rest_params);
    result
}

impl Checker {
    // nodebuilderimpl.go:1984
    pub(crate) fn get_expanded_parameters(
        &mut self,
        sig: P<Signature>,
        skip_union_expanding: bool,
    ) -> Vec<Vec<P<Symbol>>> {
        if signature_has_rest_parameter(sig) {
            let rest_index = sig.parameters().len() - 1;
            let rest_symbol = sig.parameters()[rest_index];
            let rest_type = self.get_type_of_symbol(rest_symbol);
            if is_tuple_type(rest_type) {
                return vec![expand_signature_parameters_with_tuple_members(
                    self,
                    sig,
                    rest_type,
                    rest_index,
                    rest_symbol,
                )];
            } else if !skip_union_expanding
                && rest_type.flags().intersects(TypeFlags::Union)
                && rest_type.as_union_type().types.get().iter().all(|&t| is_tuple_type(t))
            {
                return rest_type
                    .as_union_type()
                    .types
                    .get()
                    .iter()
                    .map(|&t| {
                        expand_signature_parameters_with_tuple_members(
                            self,
                            sig,
                            t,
                            rest_index,
                            rest_symbol,
                        )
                    })
                    .collect();
            }
        }
        vec![sig.parameters().to_vec()]
    }
}

impl NodeBuilderImpl {
    // nodebuilderimpl.go:2072
    fn try_get_this_parameter_declaration(
        &self,
        c: &mut Checker,
        signature: P<Signature>,
    ) -> Option<P<Node>> {
        if let Some(this_parameter) = signature.this_parameter() {
            return Some(self.symbol_to_parameter_declaration(c, this_parameter, false));
        }
        if signature.declaration().is_some() && ast::is_in_js_file(signature.declaration()) {
            // !!! JSDoc Support
            // thisTag := getJSDocThisTag(signature.declaration)
            // if (thisTag && thisTag.typeExpression) {
            // 	return factory.createParameterDeclaration(
            // 		/*modifiers*/ undefined,
            // 		/*dotDotDotToken*/ undefined,
            // 		"this",
            // 		/*questionToken*/ undefined,
            // 		typeToTypeNodeHelper(getTypeFromTypeNode(context, thisTag.typeExpression), context),
            // 	);
            // }
        }
        None
    }

    /**
     * Serializes the return type of the signature by first trying to use the syntactic printer if possible and falling back to the checker type if not.
     */
    // nodebuilderimpl.go:2095
    pub(crate) fn serialize_return_type_for_signature(
        &self,
        c: &mut Checker,
        signature: P<Signature>,
        try_reuse: bool,
    ) -> Option<P<Node>> {
        let suppress_any = self.ctx().flags.get().intersects(Flags::SuppressAnyReturnType);
        let mut restore_flags = self.save_restore_flags(c);
        if suppress_any {
            self.ctx().flags.set(self.ctx().flags.get() & !Flags::SuppressAnyReturnType); // suppress only toplevel `any`s
        }
        let mut return_type_node: Option<P<Node>> = None;

        let declaration = signature.declaration();
        let return_type: P<Type> = if let Some(declaration) = declaration
            && !ast::node_is_synthesized(declaration)
        {
            let symbol = c.get_symbol_of_declaration(declaration);
            let enclosing_symbol_type = self
                .ctx()
                .enclosing_symbol_types
                .borrow()
                .get(&ast::get_symbol_id(symbol.unwrap()))
                .copied();
            if let Some(t) = enclosing_symbol_type {
                t
            } else {
                let t = c.get_return_type_of_signature(signature);
                c.instantiate_type(t, self.ctx().mapper.get())
            }
        } else {
            c.get_return_type_of_signature(signature)
        };
        if !(suppress_any && is_type_any(Some(return_type))) {
            if !self.is_actively_expanding(c)
                && try_reuse
                && self.ctx().enclosing_declaration.get().is_some()
                && let Some(declaration) = declaration
                && !ast::node_is_synthesized(declaration)
            {
                let declaration_symbol = c.get_symbol_of_declaration(declaration).unwrap();
                let mut restore =
                    self.add_symbol_type_to_context(c, declaration_symbol, return_type);
                let mut pt = self.pc.get_return_type_of_signature(declaration);
                let report_errors = !self.ctx().suppress_report_inference_fallback.get();
                if self.pseudo_type_equivalent_to_type(
                    c,
                    pt,
                    Some(return_type),
                    false,
                    report_errors,
                ) {
                    // Also verify the pseudo type captures any inferred type predicate, not just the boolean return type.
                    // The pseudochecker is unaware of inferred type predicates, so it produces boolean where
                    // the checker infers e.g. `x is string`.
                    let type_predicate = c.get_type_predicate_of_signature(signature);
                    if let Some(type_predicate) = type_predicate
                        && !Self::pseudo_return_type_matches_predicate(c, pt, type_predicate)
                    {
                        if !self.ctx().suppress_report_inference_fallback.get() {
                            self.tracker().report_inference_fallback(c, declaration);
                        }
                        pt = None;
                    }
                    if let Some(pt) = pt {
                        // !!! TODO: If annotated type node is a reference with insufficient type arguments, we should still fall back to type serialization
                        // see: canReuseTypeNodeAnnotation in strada for context
                        return_type_node =
                            self.pseudo_type_to_node_with_checker_fallback(c, pt, return_type);
                    }
                }
                restore(c);
            }
            if return_type_node.is_none() {
                return_type_node =
                    self.serialize_inferred_return_type_for_signature(c, signature, return_type);
            }
        }

        if return_type_node.is_none() && !suppress_any {
            return_type_node = Some(self.f.new_keyword_type_node(Kind::AnyKeyword));
        }
        restore_flags(c);
        return_type_node
    }

    // nodebuilderimpl.go:2150
    fn is_trivially_serializable_computed_name(&self, c: &mut Checker, e: Option<P<Node>>) -> bool {
        let shape_good = e.is_some()
            && e.unwrap().name().is_some()
            && ast::is_computed_property_name(e.unwrap().name().unwrap())
            && ast::is_entity_name_expression(e.unwrap().name().unwrap().expression().unwrap());
        if !shape_good {
            return false;
        }
        // TODO: going through emit resolver here is weird. Relayer these APIs.
        let r = c.get_emit_resolver();
        // The only caller checks that `b.ctx.enclosingDeclaration` is non-nil first.
        r.is_entity_name_visible(
            c,
            e.unwrap().name().unwrap().expression().unwrap(),
            self.ctx().enclosing_declaration.get(),
            false,
        )
        .accessibility
            == SymbolAccessibility::Accessible
    }

    // nodebuilderimpl.go:2159
    fn index_info_to_object_computed_names_or_signature_declaration(
        &self,
        c: &mut Checker,
        index_info: P<IndexInfo>,
        type_node: Option<P<Node>>,
    ) -> Vec<P<Node>> {
        let components = index_info.components.get();
        if !components.is_empty() {
            // Index info is derived from object or class computed property names (plus explicit named members) - we can clone those instead of writing out the result computed index signature
            let all_component_computed_names_serializable =
                self.ctx().enclosing_declaration.get().is_some()
                    && components
                        .iter()
                        .all(|&e| self.is_trivially_serializable_computed_name(c, Some(e)));
            if all_component_computed_names_serializable {
                // Only use computed name serialization form if all components are visible and take the `a.b.c` form
                let new_components: Vec<P<Node>> = components
                    .iter()
                    .copied()
                    .filter(|&e| {
                        // skip late bound props that contribute to the index signature - they'll be created by property creation anyway
                        !c.has_late_bindable_name(e)
                    })
                    .collect();
                let mut bailed = false;
                let results: Vec<Option<P<Node>>> = new_components
                    .iter()
                    .map(|&e| {
                        let name = self.reuse_node(c, e.name().unwrap());
                        if let Some(name) = name {
                            // Still need to track visibility even if we've already checked it to paint references as used
                            self.track_computed_name(
                                c,
                                e.name().unwrap().expression().unwrap(),
                                self.ctx().enclosing_declaration.get(),
                            );
                            let mut mods: Option<P<ModifierList>> = None;
                            if index_info.is_readonly.get() {
                                mods = Some(self.f.new_modifier_list(vec![
                                    self.f.new_modifier(Kind::ReadonlyKeyword),
                                ]));
                            }
                            let mut postfix_token: Option<P<Node>> = None;
                            if let Some(t) = e.postfix_token() {
                                postfix_token = Some(t.clone_node(&self.f));
                            }
                            let current_type_node = if type_node.is_some() {
                                self.f.deep_clone_node(type_node)
                            } else {
                                let t = c.get_type_of_symbol(e.symbol().unwrap());
                                self.type_to_type_node(c, Some(t))
                            };
                            let sig = self.f.new_property_signature_declaration(
                                mods,
                                name,
                                postfix_token,
                                current_type_node,
                                None,
                            );
                            sig.set_loc(e.loc());
                            return Some(sig);
                        }
                        bailed = true;
                        None
                    })
                    .collect();
                if !bailed {
                    return results.into_iter().map(|n| n.unwrap()).collect();
                }
            }
        }
        vec![self.index_info_to_index_signature_declaration_helper(c, index_info, type_node)]
    }

    // nodebuilderimpl.go:2210
    pub(crate) fn index_info_to_index_signature_declaration_helper(
        &self,
        c: &mut Checker,
        index_info: P<IndexInfo>,
        type_node: Option<P<Node>>,
    ) -> P<Node> {
        let name = get_name_from_index_info(index_info);
        let indexer_type_node = self.type_to_type_node(c, index_info.key_type.get());

        let indexing_parameter = self.f.new_parameter_declaration(
            None,
            None,
            self.new_identifier(c, &name, None /*symbol*/),
            None,
            indexer_type_node,
            None,
        );
        let mut type_node = type_node;
        if type_node.is_none() {
            if index_info.value_type.get().is_none() {
                type_node = Some(self.f.new_keyword_type_node(Kind::AnyKeyword));
            } else {
                type_node = self.type_to_type_node(c, index_info.value_type.get());
            }
        }
        if index_info.value_type.get().is_none()
            && !self.ctx().flags.get().intersects(Flags::AllowEmptyIndexInfoType)
        {
            self.ctx().encountered_error.set(true);
        }
        add_approximate_length(self, name.len() + 4);
        let mut modifiers: Option<P<ModifierList>> = None;
        if index_info.is_readonly.get() {
            add_approximate_length(self, 9);
            modifiers =
                Some(self.f.new_modifier_list(vec![self.f.new_modifier(Kind::ReadonlyKeyword)]));
        }
        self.f.new_index_signature_declaration(
            modifiers,
            Some(self.f.new_node_list(vec![indexing_parameter])),
            type_node,
        )
    }
}

// nodebuilderimpl.go:2234
fn has_type_annotation(declaration: Option<P<Node>>) -> bool {
    if declaration.is_none() || declaration.unwrap().type_node().is_none() {
        return false;
    }
    let declaration = declaration.unwrap();
    // Type alias declarations have a .Type() that is their type definition, not a type annotation on a value.
    // Exclude them so callers don't mistake them for annotated value declarations.
    if ast::is_type_alias_declaration(declaration) || ast::is_js_type_alias_declaration(declaration)
    {
        return false;
    }
    true
}

impl NodeBuilderImpl {
    /**
     * Unlike `typeToTypeNodeHelper`, this handles setting up the `AllowUniqueESSymbolType` flag
     * so a `unique symbol` is returned when appropriate for the input symbol, rather than `typeof sym`
     * @param declaration - The preferred declaration to pull existing type nodes from (the symbol will be used as a fallback to find any annotated declaration)
     * @param type - The type to write; an existing annotation must match this type if it's used, otherwise this is the type serialized as a new type node
     * @param symbol - The symbol is used both to find an existing annotation if declaration is not provided, and to determine if `unique symbol` should be printed
     */
    // nodebuilderimpl.go:2253
    pub(crate) fn serialize_type_for_declaration(
        &self,
        c: &mut Checker,
        declaration: Option<P<Node>>,
        t: Option<P<Type>>,
        symbol: Option<P<Symbol>>,
        try_reuse: bool,
    ) -> P<Node> {
        let mut declaration = declaration;
        let mut t = t;
        let mut symbol = symbol;
        if declaration.is_none()
            && let Some(symbol) = symbol
        {
            declaration = symbol.value_declaration();
            if declaration.is_none() {
                // TODO: prefer annotated declarations like in strada (but does this ever even matter in practice? All callers should supply a declaration!)
                declaration = symbol.declarations().first().copied();
            }
        }
        if symbol.is_none() {
            symbol = c.get_symbol_of_declaration(declaration.unwrap());
        }
        if t.is_none() {
            match symbol {
                None => {
                    if ast::is_variable_like(declaration.unwrap()) {
                        t = c.get_type_for_variable_like_declaration(
                            declaration.unwrap(),
                            false,
                            CheckMode::Normal,
                        );
                    } else {
                        t = Some(c.error_type);
                    }
                }
                Some(symbol) => {
                    t = self
                        .ctx()
                        .enclosing_symbol_types
                        .borrow()
                        .get(&ast::get_symbol_id(symbol))
                        .copied();
                    if t.is_none() {
                        if symbol.flags().intersects(SymbolFlags::Accessor)
                            && declaration.unwrap().kind() == Kind::SetAccessor
                        {
                            let write_type = c.get_write_type_of_symbol(symbol).unwrap();
                            t = Some(c.instantiate_type(write_type, self.ctx().mapper.get()));
                        } else if !symbol
                            .flags()
                            .intersects(SymbolFlags::TypeLiteral | SymbolFlags::Signature)
                        {
                            let type_of_symbol = c.get_type_of_symbol(symbol);
                            let widened = c.get_widened_literal_type(type_of_symbol);
                            t = Some(c.instantiate_type(widened, self.ctx().mapper.get()));
                        } else {
                            t = Some(c.error_type);
                        }
                    }
                }
            }
        }
        let mut t = t.unwrap();

        // !!! TODO: JSDoc, getEmitResolver call is unfortunate layering for the helper - hoist it into checker
        let requires_adding_undefined = declaration.is_some()
            && (ast::is_parameter_declaration(declaration.unwrap())
                || ast::is_property_signature_declaration(declaration.unwrap())
                || ast::is_property_declaration(declaration.unwrap()))
            && {
                let r = c.get_emit_resolver();
                r.requires_adding_implicit_undefined(
                    c,
                    declaration.unwrap(),
                    symbol,
                    self.ctx().enclosing_declaration.get(),
                )
            };
        let add_undefined_for_parameter = requires_adding_undefined
            && (ast::is_parameter_declaration(declaration.unwrap())/*|| ast.IsJSDocParameterTag(declaration)*/);
        if add_undefined_for_parameter {
            t = c.get_optional_type(t, false);
        }

        let mut restore_flags = self.save_restore_flags(c);
        if t.flags().intersects(TypeFlags::UniqueESSymbol)
            && t.symbol() == symbol
            && (self.ctx().enclosing_declaration.get().is_none() || {
                let enclosing_file = self.ctx().enclosing_file.get();
                symbol
                    .unwrap()
                    .declarations()
                    .iter()
                    .any(|&d| ast::get_source_file_of_node(d) == enclosing_file)
            })
        {
            self.ctx().flags.set(self.ctx().flags.get() | Flags::AllowUniqueESSymbolType);
        }
        let mut result: Option<P<Node>> = None;
        let mut reported_inference_fallback = false;
        // !!! expandable hover support
        if !self.is_actively_expanding(c)
            && try_reuse
            && self.ctx().enclosing_declaration.get().is_some()
            && let Some(declaration) = declaration
            && (ast::is_accessor(declaration)
                || (ast::has_inferred_type(declaration)
                    && !ast::node_is_synthesized(declaration)
                    && !t.object_flags().intersects(ObjectFlags::RequiresWidening)))
        {
            let mut remove: Option<Box<dyn FnMut(&mut Checker)>> = None;
            if let Some(symbol) = symbol {
                remove = Some(self.add_symbol_type_to_context(c, symbol, t));
            }
            let mut pt = if ast::is_accessor(declaration) {
                self.pc.get_type_of_accessor(declaration)
            } else {
                self.pc.get_type_of_declaration(declaration)
            };
            if pt.is_none_or(|pt| pt.kind == pseudochecker::PseudoTypeKind::NoResult)
                && ast::is_binary_expression(declaration)
                && let Some(symbol) = symbol
            {
                let decl =
                    symbol.declarations().iter().copied().find(|&d| has_type_annotation(Some(d)));
                if let Some(decl) = decl {
                    // Binary expressions have a first-in-wins type annotation system. The first one with an annotation supplies the type for the rest.
                    pt = self.pc.get_type_of_declaration(decl);
                }
            }
            let report_errors = !self.ctx().suppress_report_inference_fallback.get();
            let is_optional_annotated = !requires_adding_undefined
                && (ast::is_parameter_declaration(declaration)
                    || ast::is_property_signature_declaration(declaration)
                    || ast::is_property_declaration(declaration))
                && is_optional_declaration(declaration);
            if self.pseudo_type_equivalent_to_type(
                c,
                pt,
                Some(t),
                is_optional_annotated,
                report_errors,
            ) {
                // !!! TODO: If annotated type node is a reference with insufficient type arguments, we should still fall back to type serialization
                // see: canReuseTypeNodeAnnotation in strada for context
                let ptt = Self::pseudo_type_to_type(c, pt);
                if let Some(ptt) = ptt
                    && requires_adding_undefined
                    && contains_non_missing_undefined_type(c, t)
                    && !contains_non_missing_undefined_type(c, ptt)
                {
                    pt = Some(pseudochecker::new_pseudo_type_union(&[
                        pt.unwrap(),
                        *pseudochecker::PSEUDO_TYPE_UNDEFINED,
                    ]));
                }
                result = self.pseudo_type_to_node_with_checker_fallback(c, pt.unwrap(), t);
            } else {
                // Equivalence failed; if errors from inferred-with-errors pseudo types were
                // reported, note it so we can suppress nested errors during the fallback
                // typeToTypeNode serialization (mirroring the suppression that
                // pseudoTypeToNodeWithCheckerFallback provides).
                reported_inference_fallback = report_errors
                    && pt.unwrap().kind == pseudochecker::PseudoTypeKind::Inferred
                    && matches!(&pt.unwrap().data, pseudochecker::PseudoTypeData::Inferred(inferred) if !inferred.error_nodes.is_empty());
                let mut should_add_undefined = false;
                if requires_adding_undefined {
                    if let Some(ptt) = Self::pseudo_type_to_type(c, pt) {
                        should_add_undefined = !contains_non_missing_undefined_type(c, ptt);
                    } else {
                        should_add_undefined =
                            !pseudochecker::could_already_refer_to_undefined_type(pt.unwrap());
                    }
                }
                if should_add_undefined {
                    pt = Some(pseudochecker::new_pseudo_type_union(&[
                        pt.unwrap(),
                        *pseudochecker::PSEUDO_TYPE_UNDEFINED,
                    ]));
                    if self.pseudo_type_equivalent_to_type(c, pt, Some(t), false, report_errors) {
                        result = self.pseudo_type_to_node_with_checker_fallback(c, pt.unwrap(), t);
                        reported_inference_fallback = false;
                    }
                }
            }
            if let Some(mut remove) = remove {
                remove(c);
            }
        }
        if result.is_none() {
            if reported_inference_fallback {
                let old_suppress = self.ctx().suppress_report_inference_fallback.get();
                self.ctx().suppress_report_inference_fallback.set(true);
                result = self.type_to_type_node(c, Some(t));
                self.ctx().suppress_report_inference_fallback.set(old_suppress);
            } else {
                result = self.type_to_type_node(c, Some(t));
            }
        }
        restore_flags(c);
        match result {
            None => self.f.new_keyword_type_node(Kind::AnyKeyword),
            Some(result) => result,
        }
    }

    // nodebuilderimpl.go:2374
    fn should_use_placeholder_for_property(&self, c: &Checker, property_symbol: P<Symbol>) -> bool {
        // Reverse mapped type placeholders are for display, not declaration emit.
        if !self.ctx().flags.get().intersects(Flags::AllowAnonymousIdentifier) {
            return false;
        }
        // Use placeholders for reverse mapped types we've either
        // (1) already descended into, or
        // (2) are nested reverse mappings within a mapping over a non-anonymous type, or
        // (3) are deeply nested properties that originate from the same mapped type.
        // Condition (2) is a restriction mostly just to
        // reduce the blowup in printback size from doing, eg, a deep reverse mapping over `Window`.
        // Since anonymous types usually come from expressions, this allows us to preserve the output
        // for deep mappings which likely come from expressions, while truncating those parts which
        // come from mappings over library functions.
        // Condition (3) limits printing of possibly infinitely deep reverse mapped types.
        if !property_symbol.check_flags().intersects(CheckFlags::ReverseMapped) {
            return false;
        }
        let reverse_mapped_stack = self.ctx().reverse_mapped_stack.borrow().clone();
        // (1)
        if reverse_mapped_stack.contains(&property_symbol) {
            return true;
        }
        // (2)
        if !reverse_mapped_stack.is_empty() {
            let last = reverse_mapped_stack[reverse_mapped_stack.len() - 1];
            if c.reverse_mapped_symbol_links.has(last) {
                let links = c.reverse_mapped_symbol_links.try_get(last).unwrap();
                let property_type = links.property_type.get();
                if property_type.is_some()
                    && !property_type.unwrap().object_flags().intersects(ObjectFlags::Anonymous)
                {
                    return true;
                }
            }
        }
        // (3) - we only inspect the last MAX_REVERSE_MAPPED_NESTING_INSPECTION_DEPTH elements of the
        // stack for approximate matches to catch tight infinite loops
        // TODO: Why? Reasoning lost to time. this could probably stand to be improved?
        if reverse_mapped_stack.len() < MAX_REVERSE_MAPPED_NESTING_INSPECTION_DEPTH {
            return false;
        }
        if !c.reverse_mapped_symbol_links.has(property_symbol) {
            return false;
        }
        let property_links = c.reverse_mapped_symbol_links.try_get(property_symbol).unwrap();
        let prop_mapped_type = property_links.mapped_type.get();
        if prop_mapped_type.is_none() || prop_mapped_type.unwrap().symbol().is_none() {
            return false;
        }
        let prop_mapped_type = prop_mapped_type.unwrap();
        if reverse_mapped_stack
            .iter()
            .rev()
            .take(MAX_REVERSE_MAPPED_NESTING_INSPECTION_DEPTH + 1)
            .any(|&prop| {
                if c.reverse_mapped_symbol_links.has(prop) {
                    let links = c.reverse_mapped_symbol_links.try_get(prop).unwrap();
                    let mapped_type = links.mapped_type.get();
                    mapped_type.is_some()
                        && mapped_type.unwrap().symbol() == prop_mapped_type.symbol()
                } else {
                    false
                }
            })
        {
            return true;
        }
        false
    }

    // nodebuilderimpl.go:2433
    pub(crate) fn track_computed_name(
        &self,
        c: &mut Checker,
        access_expression: P<Node>,
        enclosing_declaration: Option<P<Node>>,
    ) {
        // get symbol of the first identifier of the entityName
        let first_identifier = ast::get_first_identifier(access_expression);
        let name = c.resolve_name(
            enclosing_declaration,
            first_identifier.text(),
            SymbolFlags::Value | SymbolFlags::ExportValue,
            None, /*nameNotFoundMessage*/
            true, /*isUse*/
            false,
        );
        if let Some(name) = name {
            self.tracker().track_symbol(c, name, enclosing_declaration, SymbolFlags::Value);
        } else {
            // Name does not resolve at target location, track symbol at dest location (should be inaccessible)
            let fallback = c.resolve_name(
                Some(first_identifier),
                first_identifier.text(),
                SymbolFlags::Value | SymbolFlags::ExportValue,
                None, /*nameNotFoundMessage*/
                true, /*isUse*/
                false,
            );
            if let Some(fallback) = fallback {
                self.tracker().track_symbol(c, fallback, enclosing_declaration, SymbolFlags::Value);
            }
        }
    }
}

// nodebuilderimpl.go:2456
#[expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]
pub(crate) fn classify_property_name(
    name: &str,
    string_named: bool,
    is_method: bool,
) -> PropertyNameNodeKind {
    if is_method && name == "new" {
        return PropertyNameNodeKind::StringLiteral;
    }
    if tsrs_scanner::is_identifier_text(name, LanguageVariant::Standard) {
        return PropertyNameNodeKind::Identifier;
    }
    if !string_named && is_numeric_literal_name(name) && jsnum::from_string(name).0 >= 0.0 {
        PropertyNameNodeKind::NumericLiteral
    } else {
        PropertyNameNodeKind::StringLiteral
    }
}

impl NodeBuilderImpl {
    // nodebuilderimpl.go:2466
    fn create_property_name_node_for_identifier_or_literal(
        &self,
        c: &mut Checker,
        name: &str,
        single_quote: bool,
        string_named: bool,
        is_method: bool,
        symbol: P<Symbol>,
    ) -> P<Node> {
        match classify_property_name(name, string_named, is_method) {
            PropertyNameNodeKind::Identifier => self.new_identifier(c, name, Some(symbol)),
            PropertyNameNodeKind::NumericLiteral => {
                self.f.new_numeric_literal(self.f.alloc_text(name), TokenFlags::None)
            }
            PropertyNameNodeKind::StringLiteral => self.f.new_string_literal(
                self.f.alloc_text(name),
                if single_quote { TokenFlags::SingleQuote } else { TokenFlags::None },
            ),
        }
    }

    // nodebuilderimpl.go:2477
    fn is_string_named(c: &mut Checker, d: P<Node>) -> bool {
        let name = ast::get_name_of_declaration(d);
        let Some(name) = name else {
            return false;
        };
        if ast::is_computed_property_name(name) {
            let t = c.check_expression(name.expression().unwrap());
            return t.flags().intersects(TypeFlags::StringLike);
        }
        if ast::is_element_access_expression(name) {
            let t = c.check_expression(name.as_element_access_expression().argument_expression);
            return t.flags().intersects(TypeFlags::StringLike);
        }
        ast::is_string_literal(name)
    }

    // nodebuilderimpl.go:2493
    fn is_single_quoted_string_named(_c: &mut Checker, d: P<Node>) -> bool {
        let name = ast::get_name_of_declaration(d);
        name.is_some()
            && ast::is_string_literal(name.unwrap())
            && name.unwrap().as_string_literal().token_flags().intersects(TokenFlags::SingleQuote)
    }

    // nodebuilderimpl.go:2498
    fn get_property_name_node_for_symbol(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
    ) -> Option<P<Node>> {
        // For hash-private names, clone the original private identifier from the declaration
        if let Some(value_declaration) = symbol.value_declaration() {
            let decl_name = value_declaration.name();
            if decl_name.is_some() && ast::is_private_identifier(decl_name.unwrap()) {
                return self.f.deep_clone_node(decl_name);
            }
        }
        let declarations: Vec<P<Node>> = symbol.declarations().to_vec();
        let string_named =
            !declarations.is_empty() && declarations.iter().all(|&d| Self::is_string_named(c, d));
        let single_quote = !declarations.is_empty()
            && declarations.iter().all(|&d| Self::is_single_quoted_string_named(c, d));
        let is_method = symbol.flags().intersects(SymbolFlags::Method);
        let from_name_type = self.get_property_name_node_for_symbol_from_name_type(
            c,
            symbol,
            enclosing_declaration,
            single_quote,
            string_named,
            is_method,
        );
        if from_name_type.is_some() {
            return from_name_type;
        }

        let mut name: String = symbol.name().to_string();
        let private_name_prefix = format!("{INTERNAL_SYMBOL_NAME_PREFIX}#");
        if name.starts_with(&private_name_prefix) {
            // symbol IDs are unstable - replace #nnn# with #private#
            name = name[private_name_prefix.len()..].to_string();
            name = name.trim_start_matches(|ch: char| ch.is_ascii_digit()).to_string();
            name = format!("__#private{name}");
        }

        Some(self.mark_symbol_name_literal(
            self.create_property_name_node_for_identifier_or_literal(
                c,
                &name,
                single_quote,
                string_named,
                is_method,
                symbol,
            ),
        ))
    }

    // See getNameForSymbolFromNameType for a stringy equivalent
    // nodebuilderimpl.go:2527
    fn get_property_name_node_for_symbol_from_name_type(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        enclosing_declaration: Option<P<Node>>,
        single_quote: bool,
        string_named: bool,
        is_method: bool,
    ) -> Option<P<Node>> {
        if !c.value_symbol_links.has(symbol) {
            return None;
        }
        let name_type = c.value_symbol_links.try_get(symbol).unwrap().name_type();
        let name_type = name_type?;
        let mut enum_enclosing_declaration = enclosing_declaration;
        if enum_enclosing_declaration.is_none()
            && let Some(enclosing_file) = self.ctx().enclosing_file.get()
        {
            enum_enclosing_declaration = Some(enclosing_file.as_node());
        }
        if name_type.flags().intersects(TypeFlags::EnumLiteral) {
            let name_type_symbol = name_type.symbol().unwrap();
            let enum_symbol = match name_type_symbol.parent() {
                Some(parent) => parent,
                None => name_type_symbol,
            };
            if enum_enclosing_declaration.is_some()
                && c.is_symbol_accessible_by_flags(
                    enum_symbol,
                    enum_enclosing_declaration,
                    SymbolFlags::Value,
                )
            {
                let save_enclosing_declaration = self.ctx().enclosing_declaration.get();
                self.ctx().enclosing_declaration.set(enum_enclosing_declaration);
                let expression = self.symbol_to_expression(c, name_type_symbol, SymbolFlags::Value);
                let result = self.f.new_computed_property_name(expression);
                self.ctx().enclosing_declaration.set(save_enclosing_declaration);
                return Some(result);
            }
        }
        if name_type.flags().intersects(TypeFlags::StringOrNumberLiteral) {
            let name: String = match name_type.as_literal_type().value.get() {
                Some(LiteralValue::Number(n)) => n.string(),
                Some(LiteralValue::String(s)) => s.to_string(),
                _ => String::new(),
            };
            if !tsrs_scanner::is_identifier_text(&name, LanguageVariant::Standard)
                && (string_named || !is_numeric_literal_name(&name))
            {
                let node = self.f.new_string_literal(
                    self.f.alloc_text(&name),
                    if single_quote { TokenFlags::SingleQuote } else { TokenFlags::None },
                );
                return Some(node);
            }
            if is_numeric_literal_name(&name) && name.as_bytes()[0] == b'-' {
                return Some(self.f.new_computed_property_name(
                    self.f.new_prefix_unary_expression(
                        Kind::MinusToken,
                        self.f.new_numeric_literal(self.f.alloc_text(&name[1..]), TokenFlags::None),
                    ),
                ));
            }
            return Some(self.create_property_name_node_for_identifier_or_literal(
                c,
                &name,
                single_quote,
                string_named,
                is_method,
                symbol,
            ));
        }
        if name_type.flags().intersects(TypeFlags::UniqueESSymbol) {
            // The reference was tracked in the destination scope by trackComputedName.
            // Reconstructing its spelling in the source scope must not paint that scope's declarations visible.
            let expression = self.symbol_to_expression_worker(
                c,
                name_type.symbol().unwrap(),
                SymbolFlags::Value,
            );
            return Some(self.f.new_computed_property_name(expression));
        }
        None
    }

    // nodebuilderimpl.go:2578
    pub(crate) fn add_property_to_element_list(
        &self,
        c: &mut Checker,
        property_symbol: P<Symbol>,
        type_elements: &[P<Node>],
    ) -> Vec<P<Node>> {
        let mut type_elements: Vec<P<Node>> = type_elements.to_vec();
        let property_is_reverse_mapped =
            property_symbol.check_flags().intersects(CheckFlags::ReverseMapped);
        let property_type = if self.should_use_placeholder_for_property(c, property_symbol) {
            c.any_type
        } else {
            c.get_non_missing_type_of_symbol(property_symbol)
        };
        let save_enclosing_declaration = self.ctx().enclosing_declaration.get();
        self.ctx().enclosing_declaration.set(None);
        if is_late_bound_name(property_symbol.name()) {
            let first_declaration = property_symbol.declarations().first().copied();
            if let Some(decl) = first_declaration {
                if c.has_late_bindable_name(decl) {
                    if ast::is_binary_expression(decl) {
                        let name = ast::get_name_of_declaration(decl);
                        if let Some(name) = name
                            && ast::is_element_access_expression(name)
                            && ast::is_property_access_entity_name_expression(
                                name.as_element_access_expression().argument_expression,
                                false, /*allowJs*/
                            )
                        {
                            self.track_computed_name(
                                c,
                                name.as_element_access_expression().argument_expression,
                                save_enclosing_declaration,
                            );
                        }
                    } else {
                        self.track_computed_name(
                            c,
                            decl.name().unwrap().expression().unwrap(),
                            save_enclosing_declaration,
                        );
                    }
                }
            } else {
                let property_name = c.symbol_to_string(property_symbol);
                self.tracker().report_non_serializable_property(&property_name);
            }
        }
        if let Some(value_declaration) = property_symbol.value_declaration() {
            self.ctx().enclosing_declaration.set(Some(value_declaration));
        } else if let Some(first_declaration) = property_symbol.declarations().first().copied() {
            self.ctx().enclosing_declaration.set(Some(first_declaration));
        } else {
            self.ctx().enclosing_declaration.set(save_enclosing_declaration);
        }
        let property_name =
            self.get_property_name_node_for_symbol(c, property_symbol, save_enclosing_declaration);
        self.ctx().enclosing_declaration.set(save_enclosing_declaration);
        add_approximate_length(self, ast::symbol_name(property_symbol).len() + 1);

        if property_symbol.flags().intersects(SymbolFlags::Accessor) {
            let write_type = c.get_write_type_of_symbol(property_symbol).unwrap();
            if !c.is_error_type(property_type) && !c.is_error_type(write_type) {
                let prop_declaration =
                    ast::get_declaration_of_kind(property_symbol, Kind::PropertyDeclaration);
                let parent_is_class = property_symbol.parent().is_some()
                    && property_symbol.parent().unwrap().flags().intersects(SymbolFlags::Class);
                if property_type != write_type || parent_is_class && prop_declaration.is_none() {
                    let symbol_mapper = c.value_symbol_links.get(property_symbol).mapper();
                    if let Some(getter_declaration) =
                        ast::get_declaration_of_kind(property_symbol, Kind::GetAccessor)
                    {
                        let mut getter_signature =
                            c.get_signature_from_declaration(getter_declaration);
                        if symbol_mapper.is_some() {
                            getter_signature =
                                c.instantiate_signature(getter_signature, symbol_mapper);
                        }
                        let getter = self.signature_to_signature_declaration_helper(
                            c,
                            getter_signature,
                            Kind::GetAccessor,
                            Some(P::new_scratch(SignatureToSignatureDeclarationOptions {
                                name: property_name,
                                ..Default::default()
                            })),
                        );
                        self.set_comment_range(c, getter, Some(getter_declaration));
                        type_elements.push(getter);
                    }
                    if let Some(setter_declaration) =
                        ast::get_declaration_of_kind(property_symbol, Kind::SetAccessor)
                    {
                        let mut setter_signature =
                            c.get_signature_from_declaration(setter_declaration);
                        if symbol_mapper.is_some() {
                            setter_signature =
                                c.instantiate_signature(setter_signature, symbol_mapper);
                        }
                        let setter = self.signature_to_signature_declaration_helper(
                            c,
                            setter_signature,
                            Kind::SetAccessor,
                            Some(P::new_scratch(SignatureToSignatureDeclarationOptions {
                                name: property_name,
                                ..Default::default()
                            })),
                        );
                        self.set_comment_range(c, setter, Some(setter_declaration));
                        type_elements.push(setter);
                    }
                    return type_elements;
                } else if parent_is_class
                    && prop_declaration.is_some()
                    && prop_declaration
                        .unwrap()
                        .modifier_nodes()
                        .iter()
                        .any(|m| m.kind() == Kind::AccessorKeyword)
                {
                    let prop_declaration = prop_declaration.unwrap();
                    let fake_getter_signature = c.new_signature(
                        SignatureFlags::None,
                        None,
                        &[],
                        None,
                        &[],
                        Some(property_type),
                        None,
                        0,
                    );
                    let fake_getter_declaration = self.signature_to_signature_declaration_helper(
                        c,
                        fake_getter_signature,
                        Kind::GetAccessor,
                        Some(P::new_scratch(SignatureToSignatureDeclarationOptions {
                            name: property_name,
                            ..Default::default()
                        })),
                    );
                    self.set_comment_range(c, fake_getter_declaration, Some(prop_declaration));
                    type_elements.push(fake_getter_declaration);

                    let setter_param = c.new_symbol(SymbolFlags::FunctionScopedVariable, "arg");
                    c.value_symbol_links.get(setter_param).resolved_type.set(Some(write_type));
                    let void_type = c.void_type;
                    let fake_setter_signature = c.new_signature(
                        SignatureFlags::None,
                        None,
                        &[],
                        None,
                        &[setter_param],
                        Some(void_type),
                        None,
                        0,
                    );
                    let fake_setter_declaration = self.signature_to_signature_declaration_helper(
                        c,
                        fake_setter_signature,
                        Kind::SetAccessor,
                        Some(P::new_scratch(SignatureToSignatureDeclarationOptions {
                            name: property_name,
                            ..Default::default()
                        })),
                    );
                    type_elements.push(fake_setter_declaration);
                    return type_elements;
                }
            }
        }

        let optional_token = if property_symbol.flags().intersects(SymbolFlags::Optional) {
            Some(self.f.new_token(Kind::QuestionToken))
        } else {
            None
        };
        if property_symbol.flags().intersects(SymbolFlags::Function | SymbolFlags::Method)
            && c.get_properties_of_object_type(property_type).is_empty()
            && !c.is_readonly_symbol(property_symbol)
        {
            let filtered_type =
                c.filter_type(property_type, |_c, t| !t.flags().intersects(TypeFlags::Undefined));
            let signatures = c.get_signatures_of_type(filtered_type, SignatureKind::Call);
            type_elements.extend(signatures.iter().map(|&signature| {
                let method_declaration = self.signature_to_signature_declaration_helper(
                    c,
                    signature,
                    Kind::MethodSignature,
                    Some(P::new_scratch(SignatureToSignatureDeclarationOptions {
                        name: property_name,
                        question_token: optional_token,
                        ..Default::default()
                    })),
                );
                self.set_comment_range(
                    c,
                    method_declaration,
                    signature.declaration().or(property_symbol.value_declaration()),
                );
                method_declaration
            }));
            if !signatures.is_empty() || optional_token.is_none() {
                return type_elements;
            }
        }
        let property_type_node: Option<P<Node>>;
        if self.should_use_placeholder_for_property(c, property_symbol) {
            property_type_node = Some(self.create_elided_information_placeholder(c));
        } else {
            if property_is_reverse_mapped {
                self.ctx().reverse_mapped_stack.borrow_mut().push(property_symbol);
            }
            property_type_node = Some(self.serialize_type_for_declaration(
                c,
                None, /*declaration*/
                Some(property_type),
                Some(property_symbol),
                true,
            ));
            if property_is_reverse_mapped {
                self.ctx().reverse_mapped_stack.borrow_mut().pop();
            }
        }

        let mut modifiers: Option<P<ModifierList>> = None;
        if c.is_readonly_symbol(property_symbol) {
            modifiers =
                Some(self.f.new_modifier_list(vec![self.f.new_modifier(Kind::ReadonlyKeyword)]));
            add_approximate_length(self, 9);
        }
        let property_signature = self.f.new_property_signature_declaration(
            modifiers,
            property_name.unwrap(),
            optional_token,
            property_type_node,
            None,
        );

        self.set_comment_range(c, property_signature, property_symbol.value_declaration());
        type_elements.push(property_signature);

        type_elements
    }

    // Go takes `resolvedType *StructuredType`, the result of `resolveStructuredTypeMembers(t)`, which is `t` itself;
    // the Rust port passes the type because `objectFlags` lives on the `Type` header.
    // nodebuilderimpl.go:2719
    fn create_type_nodes_from_resolved_type(
        &self,
        c: &mut Checker,
        resolved_type: P<Type>,
    ) -> Option<P<NodeList>> {
        if self.check_truncation_length(c) {
            if self.ctx().flags.get().intersects(Flags::NoTruncation) {
                let elem = self.f.new_not_emitted_type_element();
                return Some(self.f.new_node_list(vec![self.e.add_synthetic_trailing_comment(
                    elem,
                    Kind::MultiLineCommentTrivia,
                    "elided",
                    false, /*hasTrailingNewLine*/
                )]));
            }
            return Some(self.f.new_node_list(vec![self.f.new_property_signature_declaration(
                None,
                self.f.new_identifier("..."),
                None,
                None,
                None,
            )]));
        }
        let structured_type = resolved_type.as_structured_type();
        let mut type_elements: Vec<P<Node>> = vec![];
        type_elements.extend(structured_type.call_signatures().iter().map(|&signature| {
            self.signature_to_signature_declaration_helper(c, signature, Kind::CallSignature, None)
        }));
        type_elements.extend(structured_type.construct_signatures().iter().filter_map(
            |&signature| {
                if signature.flags().intersects(SignatureFlags::Abstract) {
                    return None;
                }
                Some(self.signature_to_signature_declaration_helper(
                    c,
                    signature,
                    Kind::ConstructSignature,
                    None,
                ))
            },
        ));
        type_elements.extend(structured_type.index_infos().iter().flat_map(|&info| {
            let type_node = if resolved_type.object_flags().intersects(ObjectFlags::ReverseMapped)
                && self.ctx().flags.get().intersects(Flags::AllowAnonymousIdentifier)
            {
                Some(self.create_elided_information_placeholder(c))
            } else {
                None
            };
            self.index_info_to_object_computed_names_or_signature_declaration(c, info, type_node)
        }));

        let properties = structured_type.properties();
        if properties.is_empty() {
            return Some(self.f.new_node_list(type_elements));
        }

        let mut i: usize = 0;
        for &property_symbol in properties {
            if is_expanding(self.ctx())
                && property_symbol.flags().intersects(SymbolFlags::Prototype)
            {
                continue;
            }
            i += 1;
            if self.ctx().flags.get().intersects(Flags::WriteClassExpressionAsTypeLiteral) {
                if property_symbol.flags().intersects(SymbolFlags::Prototype) {
                    continue;
                }
                if get_declaration_modifier_flags_from_symbol(property_symbol)
                    .intersects(ModifierFlags::Private | ModifierFlags::Protected)
                {
                    self.tracker()
                        .report_private_in_base_of_class_expression(property_symbol.name());
                }
                if is_private_identifier_symbol(Some(property_symbol)) {
                    self.tracker().report_private_in_base_of_class_expression(ast::symbol_name(
                        property_symbol,
                    ));
                }
            }
            if self.check_truncation_length(c) && (i + 2 < properties.len() - 1) {
                if self.ctx().flags.get().intersects(Flags::NoTruncation) {
                    let last = type_elements.len() - 1;
                    type_elements[last] = self.e.add_synthetic_trailing_comment(
                        type_elements[last],
                        Kind::MultiLineCommentTrivia,
                        &format!("... {} more elided ...", properties.len() - i),
                        false, /*hasTrailingNewLine*/
                    );
                } else {
                    let text = format!("... {} more ...", properties.len() - i);
                    type_elements.push(self.f.new_property_signature_declaration(
                        None,
                        self.f.new_identifier(self.f.alloc_text(&text)),
                        None,
                        None,
                        None,
                    ));
                }
                type_elements = self.add_property_to_element_list(
                    c,
                    properties[properties.len() - 1],
                    &type_elements,
                );
                break;
            }
            type_elements = self.add_property_to_element_list(c, property_symbol, &type_elements);
        }
        if type_elements.is_empty() { None } else { Some(self.f.new_node_list(type_elements)) }
    }

    // nodebuilderimpl.go:2782
    fn create_type_node_from_object_type(&self, c: &mut Checker, t: P<Type>) -> Option<P<Node>> {
        if c.is_generic_mapped_type(t)
            || (t.object_flags().intersects(ObjectFlags::Mapped)
                && t.as_mapped_type().contains_error.get())
        {
            return Some(self.create_mapped_type_node_from_type(c, t));
        }

        let resolved = c.resolve_structured_type_members(t);
        let call_sigs = resolved.call_signatures();
        let ctor_sigs = resolved.construct_signatures();
        if resolved.properties().is_empty() && resolved.index_infos().is_empty() {
            if call_sigs.is_empty() && ctor_sigs.is_empty() {
                add_approximate_length(self, 2);
                let result = self.f.new_type_literal_node(self.f.new_node_list(vec![]));
                self.e.set_emit_flags(result, EmitFlags::SingleLine);
                return Some(result);
            }

            if call_sigs.len() == 1 && ctor_sigs.is_empty() {
                let signature = call_sigs[0];
                let signature_node = self.signature_to_signature_declaration_helper(
                    c,
                    signature,
                    Kind::FunctionType,
                    None,
                );
                return Some(signature_node);
            }

            if ctor_sigs.len() == 1 && call_sigs.is_empty() {
                let signature = ctor_sigs[0];
                let signature_node = self.signature_to_signature_declaration_helper(
                    c,
                    signature,
                    Kind::ConstructorType,
                    None,
                );
                return Some(signature_node);
            }
        }

        let abstract_signatures: Vec<P<Signature>> = ctor_sigs
            .iter()
            .copied()
            .filter(|signature| signature.flags().intersects(SignatureFlags::Abstract))
            .collect();
        if !abstract_signatures.is_empty() {
            let mut types: Vec<P<Type>> = abstract_signatures
                .iter()
                .map(|&s| c.get_or_create_type_from_signature(s))
                .collect();
            // count the number of type elements excluding abstract constructors
            let properties = resolved.properties();
            let type_element_count = call_sigs.len()
                + (ctor_sigs.len() - abstract_signatures.len())
                + resolved.index_infos().len()
                + if self.ctx().flags.get().intersects(Flags::WriteClassExpressionAsTypeLiteral) {
                    properties
                        .iter()
                        .filter(|p| !p.flags().intersects(SymbolFlags::Prototype))
                        .count()
                } else {
                    properties.len()
                };
            // don't include an empty object literal if there were no other static-side
            // properties to write, i.e. `abstract class C { }` becomes `abstract new () => {}`
            // and not `(abstract new () => {}) & {}`
            if type_element_count != 0 {
                // create a copy of the object type without any abstract construct signatures.
                types.push(Self::get_resolved_type_without_abstract_construct_signatures(c, t));
            }
            let intersection = c.get_intersection_type(&types);
            return self.type_to_type_node(c, Some(intersection));
        }

        let mut restore_flags = self.save_restore_flags(c);
        self.ctx().flags.set(self.ctx().flags.get() | Flags::InObjectTypeLiteral);
        let members = self.create_type_nodes_from_resolved_type(c, t);
        restore_flags(c);
        // Go passes a possibly-nil member list; the printer emits a nil and an empty list identically here.
        let members = match members {
            Some(members) => members,
            None => self.f.new_node_list(vec![]),
        };
        let type_literal_node = self.f.new_type_literal_node(members);
        add_approximate_length(self, 2);
        self.e.set_emit_flags(
            type_literal_node,
            if self.ctx().flags.get().intersects(Flags::MultilineObjectLiterals) {
                EmitFlags::empty()
            } else {
                EmitFlags::SingleLine
            },
        );
        Some(type_literal_node)
    }
}

// nodebuilderimpl.go:2842
fn get_type_alias_for_type_literal(c: &mut Checker, t: P<Type>) -> Option<P<Symbol>> {
    if let Some(symbol) = t.symbol()
        && symbol.flags().intersects(SymbolFlags::TypeLiteral)
        && !symbol.declarations().is_empty()
    {
        let first_declaration = symbol.declarations()[0];
        let node = ast::walk_up_parenthesized_types(first_declaration.parent()).unwrap();
        if ast::is_type_alias_declaration(node) {
            return c.get_symbol_of_declaration(node);
        }
    }
    None
}

impl NodeBuilderImpl {
    // nodebuilderimpl.go:2852
    fn should_write_type_of_function_symbol(
        &self,
        c: &mut Checker,
        symbol: P<Symbol>,
        type_id: TypeId,
    ) -> (bool, Option<P<Symbol>>) {
        let declarations: Vec<P<Node>> = symbol.declarations().to_vec();
        // `typeof C.name` can only be written when the member name is a valid identifier
        let is_static_method_symbol = symbol.flags().intersects(SymbolFlags::Method)
            && tsrs_scanner::is_identifier_text(symbol.name(), LanguageVariant::Standard)
            && declarations.iter().any(|&declaration| {
                ast::is_static(declaration)
                    && !c.is_late_bindable_index_signature(
                        ast::get_name_of_declaration(declaration).unwrap(),
                    )
            });
        let mut is_non_local_function_symbol = false;
        let mut is_function_expression_symbol = false;
        if symbol.flags().intersects(SymbolFlags::Function) {
            if symbol.parent().is_some() {
                is_non_local_function_symbol = true;
            } else {
                for &declaration in &declarations {
                    let parent = declaration.parent().unwrap();
                    if parent.kind() == Kind::SourceFile || parent.kind() == Kind::ModuleBlock {
                        is_non_local_function_symbol = true;
                        break;
                    }
                    if ast::is_function_expression_or_arrow_function(declaration)
                        && ast::is_variable_declaration(parent)
                        && ast::is_variable_declaration_list(parent.parent().unwrap())
                        && ast::is_variable_statement(parent.parent().unwrap().parent().unwrap())
                        && parent.parent().unwrap().parent().unwrap().parent().is_some()
                        && (parent.parent().unwrap().parent().unwrap().parent().unwrap().kind()
                            == Kind::SourceFile
                            || parent.parent().unwrap().parent().unwrap().parent().unwrap().kind()
                                == Kind::ModuleBlock)
                    {
                        is_non_local_function_symbol = true;
                        is_function_expression_symbol = true;
                        break;
                    }
                }
            }
        }
        if is_static_method_symbol || is_non_local_function_symbol {
            let mut symbol = Some(symbol);
            let vd = symbol.unwrap().value_declaration();
            if is_function_expression_symbol
                && let Some(vd) = vd
                && vd.parent().is_some()
                && vd.parent() != self.ctx().enclosing_declaration.get()
            {
                // Go: `getMergedSymbol` of a nil parent symbol is nil
                symbol = vd.parent().unwrap().symbol().map(|s| c.get_merged_symbol(s));
            }
            // typeof is allowed only for static/non local functions
            let result = (self.ctx().flags.get().intersects(Flags::UseTypeOfFunction) || self.ctx().visited_types.borrow().has(&type_id)) // it is type of the symbol uses itself recursively
                && (!self.ctx().flags.get().intersects(Flags::UseStructuralFallback) || c.is_value_symbol_accessible(symbol, self.ctx().enclosing_declaration.get())); // And the build is going to succeed without visibility error or there is no structural fallback allowed
            return (result, symbol);
        }
        (false, Some(symbol))
    }

    // nodebuilderimpl.go:2890
    fn create_anonymous_type_node(&self, c: &mut Checker, t: P<Type>) -> Option<P<Node>> {
        self.create_anonymous_type_node_ex(c, t, false, false)
    }

    // nodebuilderimpl.go:2894
    fn should_emit_type_of_symbol(
        &self,
        c: &mut Checker,
        force_expansion: bool,
        force_class_expansion: bool,
        is_instance_type: SymbolFlags,
        symbol: P<Symbol>,
        type_id: TypeId,
    ) -> (bool, Option<P<Symbol>>) {
        if force_expansion {
            return (false, Some(symbol));
        }
        let non_function_result = symbol.flags().intersects(SymbolFlags::Class)
            && !force_class_expansion
            && c.get_base_type_variable_of_class(symbol).is_none()
            && !(symbol.value_declaration().is_some()
                && ast::is_class_like(symbol.value_declaration().unwrap())
                && self.ctx().flags.get().intersects(Flags::WriteClassExpressionAsTypeLiteral)
                && (!ast::is_class_declaration(symbol.value_declaration().unwrap())
                    || c.is_symbol_accessible(
                        Some(symbol),
                        self.ctx().enclosing_declaration.get(),
                        is_instance_type,
                        false, /*shouldComputeAliasesToMakeVisible*/
                    )
                    .accessibility
                        != SymbolAccessibility::Accessible))
            || symbol.flags().intersects(SymbolFlags::Enum | SymbolFlags::ValueModule);
        if non_function_result {
            return (true, Some(symbol));
        }
        self.should_write_type_of_function_symbol(c, symbol, type_id)
    }

    // nodebuilderimpl.go:2905
    fn create_anonymous_type_node_ex(
        &self,
        c: &mut Checker,
        t: P<Type>,
        force_class_expansion: bool,
        force_expansion: bool,
    ) -> Option<P<Node>> {
        let type_id = t.id;
        let symbol = t.symbol();
        if let Some(symbol) = symbol {
            let is_instantiation_expression_type =
                t.object_flags().intersects(ObjectFlags::InstantiationExpressionType);
            if is_instantiation_expression_type {
                let instantiation_expression_type = t.as_instantiation_expression_type();
                let existing = instantiation_expression_type.node.get().unwrap();
                //  instantiationExpressionType.node is unreliable for constituents of unions and intersections.
                // declare const Err: typeof ErrImpl & (<T>() => T);
                // type ErrAlias<U> = typeof Err<U>;
                // declare const e: ErrAlias<number>;
                // ErrAlias<number> = typeof Err<number> = typeof ErrImpl & (<number>() => number)
                // The problem is each constituent of the intersection will be associated with typeof Err<number>
                // And when extracting a type for typeof ErrImpl from typeof Err<number> does not make sense.
                if ast::is_type_query_node(existing)
                    && self.get_type_from_type_node(c, existing, false) == Some(t)
                {
                    // Guard against unbounded recursion when the existing typeof node fails to be reused
                    // (e.g. its entity name isn't accessible from this scope) and the recovery boundary's
                    // fallback re-enters typeToTypeNode with the very same instantiation type, which would
                    // in turn try to reuse the same node again. Mark the type as visited around the reuse
                    // attempt so the inner recursion bottoms out via the visitedTypes guard below.
                    if self.ctx().visited_types.borrow().has(&type_id) {
                        return Some(self.create_cyclic_structure_placeholder(c));
                    }
                    self.ctx().visited_types.borrow_mut().add(type_id);
                    let type_node =
                        self.try_reuse_existing_non_parameter_type_node(c, existing, t, None, None);
                    self.ctx().visited_types.borrow_mut().delete(&type_id);
                    if type_node.is_some() {
                        return type_node;
                    }
                }
                if self.ctx().visited_types.borrow().has(&type_id) {
                    return Some(self.create_cyclic_structure_placeholder(c));
                }
                return self.visit_and_transform_type(c, t, |c, b, t| {
                    b.create_type_node_from_object_type(c, t)
                });
            }
            let is_instance_type =
                if is_class_instance_side(c, t) { SymbolFlags::Type } else { SymbolFlags::Value };

            // !!! JS support
            // if c.isJSConstructor(symbol.ValueDeclaration) {
            // 	// Instance and static types share the same symbol; only add 'typeof' for the static side.
            // 	return b.symbolToTypeNode(symbol, isInstanceType, nil)
            // } else
            if let (true, symbol) = self.should_emit_type_of_symbol(
                c,
                force_expansion,
                force_class_expansion,
                is_instance_type,
                symbol,
                type_id,
            ) {
                if self.should_expand_type(c, t, false /*isAlias*/) {
                    self.ctx().depth.set(self.ctx().depth.get() + 1);
                } else {
                    return self.symbol_to_type_node(c, symbol.unwrap(), is_instance_type, None);
                }
            }
            if self.ctx().visited_types.borrow().has(&type_id) {
                // If type is an anonymous type literal in a type alias declaration, use type alias name
                let type_alias = get_type_alias_for_type_literal(c, t);
                if let Some(type_alias) = type_alias {
                    // The specified symbol flags need to be reinterpreted as type flags
                    self.symbol_to_type_node(c, type_alias, SymbolFlags::Type, None)
                } else {
                    Some(self.create_cyclic_structure_placeholder(c))
                }
            } else {
                self.visit_and_transform_type(c, t, |c, b, t| {
                    b.create_type_node_from_object_type(c, t)
                })
            }
        } else if t.object_flags().intersects(ObjectFlags::ReverseMapped)
            && !self.ctx().flags.get().intersects(Flags::AllowAnonymousIdentifier)
        {
            if self.ctx().visited_types.borrow().has(&type_id) {
                return Some(self.create_cyclic_structure_placeholder(c));
            }
            self.visit_and_transform_type(c, t, |c, b, t| b.create_type_node_from_object_type(c, t))
        } else {
            // Reverse mapped types use property and index signature placeholders for display.
            self.create_type_node_from_object_type(c, t)
        }
    }

    // nodebuilderimpl.go:2978
    pub(crate) fn get_type_from_type_node(
        &self,
        c: &mut Checker,
        node: P<Node>,
        no_mapped_types: bool,
    ) -> Option<P<Type>> {
        // !!! noMappedTypes optional param support
        if node.parent().is_none() {
            return Some(c.error_type);
        }
        let t = c.get_type_from_type_node(node);
        if self.ctx().mapper.get().is_none() {
            return Some(t);
        }

        let instantiated = c.instantiate_type(t, self.ctx().mapper.get());
        if no_mapped_types && instantiated != t {
            return None;
        }
        Some(instantiated)
    }

    // nodebuilderimpl.go:2995
    fn type_to_type_node_or_circularity_elision(
        &self,
        c: &mut Checker,
        t: P<Type>,
    ) -> Option<P<Node>> {
        if t.flags().intersects(TypeFlags::Union) {
            if self.ctx().visited_types.borrow().has(&t.id) {
                return Some(self.create_cyclic_structure_placeholder(c));
            }
            return self.visit_and_transform_type(c, t, |c, b, t| b.type_to_type_node(c, Some(t)));
        }
        self.type_to_type_node(c, Some(t))
    }

    // nodebuilderimpl.go:3005
    #[inline(never)]
    fn create_cyclic_structure_placeholder(&self, c: &mut Checker) -> P<Node> {
        if !self.ctx().flags.get().intersects(Flags::AllowAnonymousIdentifier) {
            self.ctx().encountered_error.set(true);
            self.tracker().report_cyclic_structure_error();
        }
        self.create_elided_information_placeholder(c)
    }

    // nodebuilderimpl.go:3013
    fn conditional_type_to_type_node(&self, c: &mut Checker, typ: P<Type>) -> P<Node> {
        if self.check_truncation_length(c) {
            return self.create_elided_information_placeholder(c);
        }
        let t = typ.as_conditional_type();
        let root = t.root.get().unwrap();
        let check_type_node = self.type_to_type_node(c, t.check_type.get());
        add_approximate_length(self, 15);
        if self.ctx().flags.get().intersects(Flags::GenerateNamesForShadowedTypeParams)
            && root.is_distributive.get()
            && !t.check_type.get().unwrap().flags().intersects(TypeFlags::TypeParameter)
        {
            let new_param_symbol =
                c.new_symbol(SymbolFlags::TypeParameter, "T" /* as __String */);
            let new_param = c.new_type_parameter(Some(new_param_symbol));
            let name = self.type_parameter_to_name(c, new_param);
            let new_type_variable = self.f.new_type_reference_node(name, None);
            add_approximate_length(self, 37);
            // 15 each for two added conditionals, 7 for an added infer type
            let new_mapper =
                prepend_type_mapping(root.check_type.get().unwrap(), new_param, t.mapper.get());
            let save_infer_type_parameters = self.ctx().infer_type_parameters.get();
            self.ctx().infer_type_parameters.set(root.infer_type_parameters.get());
            let extends_type =
                c.instantiate_type(root.extends_type.get().unwrap(), Some(new_mapper));
            let extends_type_node = self.type_to_type_node(c, Some(extends_type));
            self.ctx().infer_type_parameters.set(save_infer_type_parameters);
            let root_node = root.node.get().unwrap();
            let true_type_from_node = self
                .get_type_from_type_node(c, root_node.as_conditional_type_node().true_type, false)
                .unwrap();
            let true_type = c.instantiate_type(true_type_from_node, Some(new_mapper));
            let true_type_node = self.type_to_type_node_or_circularity_elision(c, true_type);
            let false_type_from_node = self
                .get_type_from_type_node(c, root_node.as_conditional_type_node().false_type, false)
                .unwrap();
            let false_type = c.instantiate_type(false_type_from_node, Some(new_mapper));
            let false_type_node = self.type_to_type_node_or_circularity_elision(c, false_type);

            // outermost conditional makes `T` a type parameter, allowing the inner conditionals to be distributive
            // second conditional makes `T` have `T & checkType` substitution, so it is correctly usable as the checkType
            // inner conditional runs the check the user provided on the check type (distributively) and returns the result
            // checkType extends infer T ? T extends checkType ? T extends extendsType<T> ? trueType<T> : falseType<T> : never : never;
            // this is potentially simplifiable to
            // checkType extends infer T ? T extends checkType & extendsType<T> ? trueType<T> : falseType<T> : never;
            // but that may confuse users who read the output more.
            // On the other hand,
            // checkType extends infer T extends checkType ? T extends extendsType<T> ? trueType<T> : falseType<T> : never;
            // may also work with `infer ... extends ...` in, but would produce declarations only compatible with the latest TS.
            let new_id = new_type_variable.as_type_reference_node().type_name.clone_node(&self.f);
            let synthetic_extends_node = self.f.new_infer_type_node(
                self.f.new_type_parameter_declaration(None, new_id, None, None, None),
            );
            let inner_check_conditional_node = self.f.new_conditional_type_node(
                new_type_variable,
                extends_type_node.unwrap(),
                true_type_node.unwrap(),
                false_type_node.unwrap(),
            );
            let synthetic_true_node = self.f.new_conditional_type_node(
                self.f.new_type_reference_node(name.clone_node(&self.f), None),
                self.f.deep_clone_node(check_type_node).unwrap(),
                inner_check_conditional_node,
                self.f.new_keyword_type_node(Kind::NeverKeyword),
            );
            return self.f.new_conditional_type_node(
                check_type_node.unwrap(),
                synthetic_extends_node,
                synthetic_true_node,
                self.f.new_keyword_type_node(Kind::NeverKeyword),
            );
        }
        let save_infer_type_parameters = self.ctx().infer_type_parameters.get();
        self.ctx().infer_type_parameters.set(root.infer_type_parameters.get());
        let extends_type_node = self.type_to_type_node(c, t.extends_type.get());
        self.ctx().infer_type_parameters.set(save_infer_type_parameters);
        let true_type = c.get_true_type_from_conditional_type(typ);
        let true_type_node = self.type_to_type_node_or_circularity_elision(c, true_type);
        let false_type = c.get_false_type_from_conditional_type(typ);
        let false_type_node = self.type_to_type_node_or_circularity_elision(c, false_type);
        self.f.new_conditional_type_node(
            check_type_node.unwrap(),
            extends_type_node.unwrap(),
            true_type_node.unwrap(),
            false_type_node.unwrap(),
        )
    }

    // Go takes `*TypeParameter`; the Rust port passes the type because `symbol` lives on the `Type` header.
    // nodebuilderimpl.go:3055
    fn get_parent_symbol_of_type_parameter(
        c: &mut Checker,
        type_parameter: P<Type>,
    ) -> Option<P<Symbol>> {
        let tp =
            ast::get_declaration_of_kind(type_parameter.symbol().unwrap(), Kind::TypeParameter);
        // !!! JSDoc support
        // if ast.IsJSDocTemplateTag(tp.Parent) {
        // 	host = getEffectiveContainerForJSDocTemplateTag(tp.Parent)
        // } else {
        let host = tp.unwrap().parent();
        // }
        let host = host?;
        c.get_symbol_of_node(host)
    }

    // nodebuilderimpl.go:3070
    fn array_or_tuple_type_to_node(&self, c: &mut Checker, t: P<Type>) -> Option<P<Node>> {
        let mut type_arguments: Vec<P<Type>> = c.get_type_arguments(t).to_vec();
        let target = t.target().unwrap();
        if target == c.global_array_type || target == c.global_readonly_array_type {
            if self.ctx().flags.get().intersects(Flags::WriteArrayAsGenericType) {
                let type_argument_node = self.type_to_type_node(c, Some(type_arguments[0]));
                let type_name = self.new_identifier(
                    c,
                    if target == c.global_array_type { "Array" } else { "ReadonlyArray" },
                    target.symbol(),
                );
                return Some(self.f.new_type_reference_node(
                    type_name,
                    Some(self.f.new_node_list(vec![type_argument_node.unwrap()])),
                ));
            }
            let element_type = self.type_to_type_node(c, Some(type_arguments[0]));
            let array_type = self.f.new_array_type_node(element_type.unwrap());
            if target == c.global_array_type {
                Some(array_type)
            } else {
                Some(self.f.new_type_operator_node(Kind::ReadonlyKeyword, array_type))
            }
        } else {
            assert!(target.object_flags().intersects(ObjectFlags::Tuple));
            let element_infos = target.as_tuple_type().element_infos.get();
            type_arguments = type_arguments
                .iter()
                .enumerate()
                .map(|(i, &arg)| {
                    let mut is_optional = false;
                    if i < element_infos.len() {
                        is_optional = element_infos[i].flags.intersects(ElementFlags::Optional);
                    }
                    c.remove_missing_type(arg, is_optional)
                })
                .collect();
            if !type_arguments.is_empty() {
                let arity = checker::Checker::get_type_reference_arity(t);
                let tuple_constituent_nodes =
                    self.map_to_type_nodes(c, &type_arguments[0..arity], false /*isBareList*/);
                if let Some(tuple_constituent_nodes) = tuple_constituent_nodes {
                    let mut nodes: Vec<P<Node>> = tuple_constituent_nodes.nodes().to_vec();
                    for (i, element) in nodes.iter_mut().enumerate() {
                        let flags = element_infos[i].flags;
                        let labeled_element_declaration = element_infos[i].labeled_declaration;

                        if labeled_element_declaration.is_some() {
                            let dot_dot_dot_token = if flags.intersects(ElementFlags::Variable) {
                                Some(self.f.new_token(Kind::DotDotDotToken))
                            } else {
                                None
                            };
                            let label = Checker::get_tuple_element_label(element_infos[i], None, i);
                            let name = self.new_identifier(c, &label, None /*symbol*/);
                            let question_token = if flags.intersects(ElementFlags::Optional) {
                                Some(self.f.new_token(Kind::QuestionToken))
                            } else {
                                None
                            };
                            let type_node = if flags.intersects(ElementFlags::Rest) {
                                self.f.new_array_type_node(*element)
                            } else {
                                *element
                            };
                            (*element) = self.f.new_named_tuple_member(
                                dot_dot_dot_token,
                                name,
                                question_token,
                                type_node,
                            );
                        } else if flags.intersects(ElementFlags::Variable) {
                            (*element) = self.f.new_rest_type_node(
                                if flags.intersects(ElementFlags::Rest) {
                                    self.f.new_array_type_node(*element)
                                } else {
                                    *element
                                },
                            );
                        } else if flags.intersects(ElementFlags::Optional) {
                            (*element) = self.f.new_optional_type_node(*element);
                        }
                    }
                    // Go assigns into `tupleConstituentNodes.Nodes` in place; Rust node lists are immutable.
                    let node_list = self.f.new_node_list(nodes);
                    node_list.loc.set(tuple_constituent_nodes.loc.get());
                    let tuple_type_node = self.f.new_tuple_type_node(node_list);
                    self.e.set_emit_flags(tuple_type_node, EmitFlags::SingleLine);
                    if target.as_tuple_type().readonly.get() {
                        return Some(
                            self.f.new_type_operator_node(Kind::ReadonlyKeyword, tuple_type_node),
                        );
                    }
                    return Some(tuple_type_node);
                }
            }
            if self.ctx().encountered_error.get()
                || self.ctx().flags.get().intersects(Flags::AllowEmptyTuple)
            {
                let tuple_type_node = self.f.new_tuple_type_node(self.f.new_node_list(vec![]));
                self.e.set_emit_flags(tuple_type_node, EmitFlags::SingleLine);
                if target.as_tuple_type().readonly.get() {
                    return Some(
                        self.f.new_type_operator_node(Kind::ReadonlyKeyword, tuple_type_node),
                    );
                }
                return Some(tuple_type_node);
            }
            self.ctx().encountered_error.set(true);
            None
            // TODO: GH#18217
        }
    }

    // nodebuilderimpl.go:3159
    fn type_reference_to_type_node(&self, c: &mut Checker, t: P<Type>) -> Option<P<Node>> {
        let type_arguments: Vec<P<Type>> = c.get_type_arguments(t).to_vec();
        let target = t.target().unwrap();
        if self.ctx().flags.get().intersects(Flags::WriteClassExpressionAsTypeLiteral)
            && t.symbol().unwrap().value_declaration().is_some()
            && ast::is_class_like(t.symbol().unwrap().value_declaration().unwrap())
            && !c.is_value_symbol_accessible(t.symbol(), self.ctx().enclosing_declaration.get())
        {
            self.create_anonymous_type_node(c, t)
        } else {
            let outer_type_parameters = target.as_interface_type().outer_type_parameters();
            let mut i: usize = 0;
            let mut result_type: Option<P<Node>> = None;
            if !outer_type_parameters.is_empty() {
                let length = outer_type_parameters.len();
                while i < length {
                    // Find group of type arguments for type parameters with the same declaring container.
                    let start = i;
                    let parent =
                        Self::get_parent_symbol_of_type_parameter(c, outer_type_parameters[i]);
                    loop {
                        // do-while loop
                        i += 1;
                        if !(i < length
                            && Self::get_parent_symbol_of_type_parameter(
                                c,
                                outer_type_parameters[i],
                            ) == parent)
                        {
                            break;
                        }
                    }
                    // When type parameters are their own type arguments for the whole group (i.e. we have
                    // the default outer type arguments), we don't show the group.

                    if outer_type_parameters[start..i] != type_arguments[start..i] {
                        let type_argument_slice = self.map_to_type_nodes(
                            c,
                            &type_arguments[start..i],
                            false, /*isBareList*/
                        );
                        let mut restore_flags = self.save_restore_flags(c);
                        self.ctx().flags.set(
                            self.ctx().flags.get() | Flags::ForbidIndexedAccessSymbolReferences,
                        );
                        let ref_ = self.symbol_to_type_node(
                            c,
                            parent.unwrap(),
                            SymbolFlags::Type,
                            type_argument_slice,
                        );
                        restore_flags(c);
                        if result_type.is_none() {
                            result_type = ref_;
                        } else {
                            result_type = Some(self.append_reference_to_type(
                                c,
                                result_type.unwrap(),
                                ref_.unwrap(),
                            ));
                        }
                    }
                }
            }
            let mut type_argument_nodes: Option<P<NodeList>> = None;
            if !type_arguments.is_empty() {
                let mut type_parameter_count: usize = 0;
                let type_params = target.as_interface_type().type_parameters();
                // Go's `TypeParameters()` is nil exactly when `allTypeParameters` is empty.
                if !target.as_interface_type().all_type_parameters.get().is_empty() {
                    type_parameter_count = type_params.len().min(type_arguments.len());

                    // Maybe we should do this for more types, but for now we only elide type arguments that are
                    // identical to their associated type parameters' defaults for `Iterable`, `IterableIterator`,
                    // `AsyncIterable`, and `AsyncIterableIterator` to provide backwards-compatible .d.ts emit due
                    // to each now having three type parameters instead of only one.
                    let global_iterable_type = c.get_global_iterable_type();
                    let mut is_iterable_reference =
                        checker::Checker::is_reference_to_type(Some(t), global_iterable_type);
                    if !is_iterable_reference {
                        let global_iterable_iterator_type = c.get_global_iterable_iterator_type();
                        is_iterable_reference = checker::Checker::is_reference_to_type(
                            Some(t),
                            global_iterable_iterator_type,
                        );
                    }
                    if !is_iterable_reference {
                        let global_async_iterable_type = c.get_global_async_iterable_type();
                        is_iterable_reference = checker::Checker::is_reference_to_type(
                            Some(t),
                            global_async_iterable_type,
                        );
                    }
                    if !is_iterable_reference {
                        let global_async_iterable_iterator_type =
                            c.get_global_async_iterable_iterator_type();
                        is_iterable_reference = checker::Checker::is_reference_to_type(
                            Some(t),
                            global_async_iterable_iterator_type,
                        );
                    }
                    if is_iterable_reference {
                        let node = t.as_type_reference().node.get();
                        if node.is_none()
                            || !ast::is_type_reference_node(node.unwrap())
                            || node.unwrap().type_arguments().len() < type_parameter_count
                        {
                            while type_parameter_count > 0 {
                                let type_argument = type_arguments[type_parameter_count - 1];
                                let type_parameter = target.as_interface_type().type_parameters()
                                    [type_parameter_count - 1];
                                let default_type =
                                    c.get_default_from_type_parameter(type_parameter);
                                if default_type.is_none()
                                    || !c.is_type_identical_to(type_argument, default_type.unwrap())
                                {
                                    break;
                                }
                                type_parameter_count -= 1;
                            }
                        }
                    }
                }

                type_argument_nodes = self.map_to_type_nodes(
                    c,
                    &type_arguments[i..type_parameter_count],
                    false, /*isBareList*/
                );
            }
            let mut restore_flags = self.save_restore_flags(c);
            self.ctx()
                .flags
                .set(self.ctx().flags.get() | Flags::ForbidIndexedAccessSymbolReferences);
            let final_ref = self.symbol_to_type_node(
                c,
                t.symbol().unwrap(),
                SymbolFlags::Type,
                type_argument_nodes,
            );
            restore_flags(c);
            if let Some(result_type) = result_type {
                Some(self.append_reference_to_type(c, result_type, final_ref.unwrap()))
            } else {
                final_ref
            }
        }
    }

    // nodebuilderimpl.go:3212
    fn visit_and_transform_type(
        &self,
        c: &mut Checker,
        t: P<Type>,
        mut transform: impl FnMut(&mut Checker, P<NodeBuilderImpl>, P<Type>) -> Option<P<Node>>,
    ) -> Option<P<Node>> {
        if self.check_truncation_length(c) {
            return Some(self.create_elided_information_placeholder(c));
        }

        let is_array_or_tuple = c.is_array_or_tuple_type(t);
        let type_id = if is_array_or_tuple {
            // Deferred and regular references share a cycle identity.
            let target = t.target().unwrap();
            let type_arguments = c.get_type_arguments(t).to_vec();
            c.create_type_reference(target, &type_arguments).id
        } else {
            t.id
        };
        if self.ctx().visited_types.borrow().has(&type_id) {
            return Some(self.create_cyclic_structure_placeholder(c));
        }

        let is_constructor_object = t.object_flags().intersects(ObjectFlags::Anonymous)
            && t.symbol().is_some()
            && t.symbol().unwrap().flags().intersects(SymbolFlags::Class);
        let id: Option<CompositeSymbolIdentity> = if is_array_or_tuple {
            // Do not bound finite container nesting by the shared Array symbol or tuple origin.
            None
        } else if t.object_flags().intersects(ObjectFlags::Reference)
            && t.as_type_reference().node.get().is_some()
        {
            Some(CompositeSymbolIdentity {
                is_constructor_node: false,
                symbol_id: SymbolId(0),
                node_id: ast::get_node_id(t.as_type_reference().node.get().unwrap()),
            })
        } else if t.flags().intersects(TypeFlags::Conditional) {
            Some(CompositeSymbolIdentity {
                is_constructor_node: false,
                symbol_id: SymbolId(0),
                node_id: ast::get_node_id(
                    t.as_conditional_type().root.get().unwrap().node.get().unwrap(),
                ),
            })
        } else {
            t.symbol().map(|symbol| CompositeSymbolIdentity {
                is_constructor_node: is_constructor_object,
                symbol_id: ast::get_symbol_id(symbol),
                node_id: NodeId(0),
            })
        };
        // Since instantiations of the same anonymous type have the same symbol, tracking symbols instead
        // of types allows us to catch circular references to instantiations of the same anonymous type

        let key = CompositeTypeCacheIdentity {
            type_id,
            flags: self.ctx().flags.get(),
            internal_flags: self.ctx().internal_flags.get(),
            infer_type_parameters: if self.ctx().infer_type_parameters.get().is_empty() {
                CacheHashKey::default()
            } else {
                get_type_list_key(self.ctx().infer_type_parameters.get())
            },
        };
        // Don't rely on type cache if we're expanding a type, because we need to compute `canIncreaseExpansionDepth`.
        let can_use_cache = self.ctx().max_expansion_depth.get().is_none();
        if can_use_cache
            && self.ctx().enclosing_declaration.get().is_some()
            && self.links.has(self.ctx().enclosing_declaration.get().unwrap())
        {
            let links = self.links.get(self.ctx().enclosing_declaration.get().unwrap());
            let cached_result = links.serialized_types.get(&key);
            if let Some(cached_result) = cached_result {
                // TODO:: check if we instead store late painted statements associated with this?
                for arg in &cached_result.tracked_symbols {
                    self.tracker().track_symbol(
                        c,
                        arg.symbol,
                        arg.enclosing_declaration,
                        arg.meaning,
                    );
                }
                if cached_result.truncating {
                    self.ctx().truncating.set(true);
                }
                add_approximate_length(self, cached_result.added_length);
                return self.f.deep_clone_node(cached_result.node);
            }
        }

        let origin_depth = if t.object_flags().intersects(ObjectFlags::ReverseMapped) {
            // Growing type arguments can prevent a reverse mapped type from repeating.
            // Bound expansion by its mapped declaration as well as its type identity.
            let origin = CompositeSymbolIdentity {
                is_constructor_node: false,
                symbol_id: SymbolId(0),
                node_id: ast::get_node_id(
                    t.as_reverse_mapped_type()
                        .mapped_type
                        .get()
                        .unwrap()
                        .as_mapped_type()
                        .declaration
                        .get()
                        .unwrap(),
                ),
            };
            let depth = self.ctx().symbol_depth.borrow().get(&origin).copied().unwrap_or(0);
            if depth >= 100 {
                self.ctx().truncating.set(true);
                return Some(self.create_elided_information_placeholder(c));
            }
            self.ctx().symbol_depth.borrow_mut().insert(origin, depth + 1);
            Some((origin, depth))
        } else {
            None
        };

        let mut depth: usize = 0;
        if let Some(id) = id {
            depth = self.ctx().symbol_depth.borrow().get(&id).copied().unwrap_or(0);
            if depth > 10 {
                self.ctx().truncating.set(true);
                if let Some((origin, origin_depth)) = origin_depth {
                    self.ctx().symbol_depth.borrow_mut().insert(origin, origin_depth);
                }
                return Some(self.create_elided_information_placeholder(c));
            }
            self.ctx().symbol_depth.borrow_mut().insert(id, depth + 1);
        }
        self.ctx().visited_types.borrow_mut().add(type_id);
        let prev_tracked_symbols = std::mem::take(&mut *self.ctx().tracked_symbols.borrow_mut());
        let start_length = self.ctx().approximate_length.get();
        let result = transform(c, self.as_p(), t);
        let added_length = self.ctx().approximate_length.get() - start_length;
        if can_use_cache
            && !self.ctx().reported_diagnostic.get()
            && !self.ctx().encountered_error.get()
        {
            // Go also caches under a nil enclosing declaration; such entries are never read back (the lookup above
            // requires a non-nil enclosing declaration), so the Rust port skips them.
            if let Some(enclosing_declaration) = self.ctx().enclosing_declaration.get() {
                let links = self.links.get(enclosing_declaration);
                let tracked_symbols = self.ctx().tracked_symbols.borrow().clone();
                links.serialized_types.set(
                    key,
                    P::new_in(
                        self.f.is_scratch(),
                        SerializedTypeEntry {
                            node: result,
                            truncating: self.ctx().truncating.get(),
                            added_length,
                            tracked_symbols,
                        },
                    ),
                );
            }
        }
        self.ctx().visited_types.borrow_mut().delete(&type_id);
        if let Some(id) = id {
            self.ctx().symbol_depth.borrow_mut().insert(id, depth);
        }
        if let Some((origin, origin_depth)) = origin_depth {
            self.ctx().symbol_depth.borrow_mut().insert(origin, origin_depth);
        }
        *self.ctx().tracked_symbols.borrow_mut() = prev_tracked_symbols;
        result

        // !!! TODO: Attempt node reuse or parse nodes to minimize copying once text range setting is set up
        // deepCloneOrReuseNode := func(node T) T {
        // 	if !nodeIsSynthesized(node) && getParseTreeNode(node) == node {
        // 		return node
        // 	}
        // 	return setTextRange(b.ctx, b.f.cloneNode(visitEachChildWorker(node, deepCloneOrReuseNode, nil /*b.ctx*/, deepCloneOrReuseNodes, deepCloneOrReuseNode)), node)
        // }

        // deepCloneOrReuseNodes := func(nodes *NodeArray[*ast.Node], visitor Visitor, test func(node *ast.Node) bool, start number, count number) *NodeArray[*ast.Node] {
        // 	if nodes != nil && nodes.length == 0 {
        // 		// Ensure we explicitly make a copy of an empty array; visitNodes will not do this unless the array has elements,
        // 		// which can lead to us reusing the same empty NodeArray more than once within the same AST during type noding.
        // 		return setTextRangeWorker(b.f.NewNodeArray(nil, nodes.hasTrailingComma), nodes)
        // 	}
        // 	return visitNodes(nodes, visitor, test, start, count)
        // }
    }

    // nodebuilderimpl.go:3303
    pub(crate) fn type_to_type_node(&self, c: &mut Checker, t: Option<P<Type>>) -> Option<P<Node>> {
        let in_type_alias = self.ctx().flags.get() & Flags::InTypeAlias;
        self.ctx().flags.set(self.ctx().flags.get() & !Flags::InTypeAlias);

        let Some(t) = t else {
            if !self.ctx().flags.get().intersects(Flags::AllowEmptyUnionOrIntersection) {
                self.ctx().encountered_error.set(true);
                return None;
                // TODO: GH#18217
            }
            add_approximate_length(self, 3);
            return Some(self.f.new_keyword_type_node(Kind::AnyKeyword));
        };

        let mut t = get_non_distributed_type_parameter(t).unwrap();

        // Push type onto typeStack for expansion depth tracking
        let _pop_type_stack = if self.ctx().max_expansion_depth.get().is_some() {
            self.ctx().type_stack.borrow_mut().push(Some(t));
            let b = self.as_p();
            Some(Defer(Box::new(move || {
                b.ctx().type_stack.borrow_mut().pop();
            })))
        } else {
            None
        };

        if !self.ctx().flags.get().intersects(Flags::NoTypeReduction) {
            t = c.get_reduced_type(t);
        }

        if t.flags().intersects(TypeFlags::Any) {
            if let Some(alias) = t.alias() {
                return Some(alias.to_type_reference_node(c, self.as_p()));
            }
            if t == c.unresolved_type {
                return Some(self.e.add_synthetic_leading_comment(
                    self.f.new_keyword_type_node(Kind::AnyKeyword),
                    Kind::MultiLineCommentTrivia,
                    "unresolved",
                    false, /*hasTrailingNewLine*/
                ));
            }
            add_approximate_length(self, 3);
            return Some(self.f.new_keyword_type_node(if t == c.intrinsic_marker_type {
                Kind::IntrinsicKeyword
            } else {
                Kind::AnyKeyword
            }));
        }
        if t.flags().intersects(TypeFlags::Unknown) {
            return Some(self.f.new_keyword_type_node(Kind::UnknownKeyword));
        }
        if t.flags().intersects(TypeFlags::String) {
            add_approximate_length(self, 6);
            return Some(self.f.new_keyword_type_node(Kind::StringKeyword));
        }
        if t.flags().intersects(TypeFlags::Number) {
            add_approximate_length(self, 6);
            return Some(self.f.new_keyword_type_node(Kind::NumberKeyword));
        }
        if t.flags().intersects(TypeFlags::BigInt) {
            add_approximate_length(self, 6);
            return Some(self.f.new_keyword_type_node(Kind::BigIntKeyword));
        }
        if t.flags().intersects(TypeFlags::Boolean) && t.alias().is_none() {
            add_approximate_length(self, 7);
            return Some(self.f.new_keyword_type_node(Kind::BooleanKeyword));
        }
        let mut expanding_enum = false;
        if t.flags().intersects(TypeFlags::EnumLike) {
            if t.symbol().unwrap().flags().intersects(SymbolFlags::EnumMember) {
                let parent_symbol = c.get_parent_of_symbol(t.symbol().unwrap()).unwrap();
                let parent_name =
                    self.symbol_to_type_node(c, parent_symbol, SymbolFlags::Type, None);
                if c.get_declared_type_of_symbol(parent_symbol) == t {
                    return parent_name;
                }
                let parent_name = parent_name.unwrap();
                let member_name = ast::symbol_name(t.symbol().unwrap());
                if tsrs_scanner::is_identifier_text(member_name, LanguageVariant::Standard) {
                    return Some(self.append_reference_to_type(
                        c,
                        parent_name, /* as TypeReference | ImportTypeNode */
                        self.f.new_type_reference_node(
                            self.f.new_identifier(member_name),
                            None, /*typeArguments*/
                        ),
                    ));
                }
                if ast::is_import_type_node(parent_name) {
                    // Go sets `parentName.AsImportTypeNode().IsTypeOf = true` in place (the node is freshly
                    // manufactured anyhow); `is_type_of` is not a `Cell` in the Rust AST, so rebuild it with the same
                    // children instead.
                    let import_type = parent_name.as_import_type_node();
                    let parent_name = self.f.new_import_type_node(
                        true,
                        import_type.argument,
                        import_type.attributes,
                        import_type.qualifier,
                        parent_name.type_argument_list(),
                    );
                    let lit =
                        self.mark_symbol_name_literal(self.new_string_literal(c, member_name));
                    return Some(self.f.new_indexed_access_type_node(
                        parent_name,
                        self.f.new_literal_type_node(lit),
                    ));
                } else if ast::is_type_reference_node(parent_name) {
                    let lit =
                        self.mark_symbol_name_literal(self.new_string_literal(c, member_name));
                    return Some(self.f.new_indexed_access_type_node(
                        self.f.new_type_query_node(
                            parent_name.as_type_reference_node().type_name,
                            None,
                        ),
                        self.f.new_literal_type_node(lit),
                    ));
                }
                panic!("Unhandled type node kind returned from `symbolToTypeNode`.");
            }
            if !t.flags().intersects(TypeFlags::Union)
                || !self.should_expand_type(c, t, false /*isAlias*/)
            {
                return self.symbol_to_type_node(c, t.symbol().unwrap(), SymbolFlags::Type, None);
            }
            expanding_enum = true;
        }
        if t.flags().intersects(TypeFlags::StringLiteral) {
            let Some(LiteralValue::String(value)) = t.as_literal_type().value.get() else {
                panic!("string literal type without a string value");
            };
            add_approximate_length(self, value.len() + 2);
            let lit = self.new_string_literal(c, value);
            self.e.add_emit_flags(lit, EmitFlags::NoAsciiEscaping);
            return Some(self.f.new_literal_type_node(lit));
        }
        if t.flags().intersects(TypeFlags::NumberLiteral) {
            let Some(LiteralValue::Number(value)) = t.as_literal_type().value.get() else {
                panic!("number literal type without a number value");
            };
            let value_text = value.string();
            add_approximate_length(self, value_text.len());
            if value.0 < 0.0 {
                return Some(self.f.new_literal_type_node(
                    self.f.new_prefix_unary_expression(
                        Kind::MinusToken,
                        self.f.new_numeric_literal(
                            self.f.alloc_text(&value_text[1..]),
                            TokenFlags::None,
                        ),
                    ),
                ));
            }
            return Some(self.f.new_literal_type_node(
                self.f.new_numeric_literal(self.f.alloc_text(&value_text), TokenFlags::None),
            ));
        }
        if t.flags().intersects(TypeFlags::BigIntLiteral) {
            add_approximate_length(
                self,
                pseudo_big_int_to_string(get_big_int_literal_value(t)).len() + 1,
            );
            return Some(self.f.new_literal_type_node(self.f.new_big_int_literal(
                self.f.alloc_text(&(pseudo_big_int_to_string(get_big_int_literal_value(t)) + "n")),
                TokenFlags::None,
            )));
        }
        if t.flags().intersects(TypeFlags::BooleanLiteral) {
            let value =
                matches!(t.as_literal_type().value.get(), Some(LiteralValue::Boolean(true)));
            if value {
                add_approximate_length(self, 4);
                return Some(
                    self.f.new_literal_type_node(self.f.new_keyword_expression(Kind::TrueKeyword)),
                );
            }
            add_approximate_length(self, 5);
            return Some(
                self.f.new_literal_type_node(self.f.new_keyword_expression(Kind::FalseKeyword)),
            );
        }
        if t.flags().intersects(TypeFlags::UniqueESSymbol) {
            if !self.ctx().flags.get().intersects(Flags::AllowUniqueESSymbolType) {
                if c.is_value_symbol_accessible(t.symbol(), self.ctx().enclosing_declaration.get())
                {
                    add_approximate_length(self, 6);
                    return self.symbol_to_type_node(
                        c,
                        t.symbol().unwrap(),
                        SymbolFlags::Value,
                        None,
                    );
                }
                self.tracker().report_inaccessible_unique_symbol_error();
            }
            add_approximate_length(self, 13);
            return Some(self.f.new_type_operator_node(
                Kind::UniqueKeyword,
                self.f.new_keyword_type_node(Kind::SymbolKeyword),
            ));
        }
        if t.flags().intersects(TypeFlags::Void) {
            add_approximate_length(self, 4);
            return Some(self.f.new_keyword_type_node(Kind::VoidKeyword));
        }
        if t.flags().intersects(TypeFlags::Undefined) {
            add_approximate_length(self, 9);
            return Some(self.f.new_keyword_type_node(Kind::UndefinedKeyword));
        }
        if t.flags().intersects(TypeFlags::Null) {
            add_approximate_length(self, 4);
            return Some(
                self.f.new_literal_type_node(self.f.new_keyword_expression(Kind::NullKeyword)),
            );
        }
        if t.flags().intersects(TypeFlags::Never) {
            add_approximate_length(self, 5);
            return Some(self.f.new_keyword_type_node(Kind::NeverKeyword));
        }
        if t.flags().intersects(TypeFlags::ESSymbol) {
            add_approximate_length(self, 6);
            return Some(self.f.new_keyword_type_node(Kind::SymbolKeyword));
        }
        if t.flags().intersects(TypeFlags::NonPrimitive) {
            add_approximate_length(self, 6);
            return Some(self.f.new_keyword_type_node(Kind::ObjectKeyword));
        }
        if is_this_type_parameter(t) {
            if self.ctx().flags.get().intersects(Flags::InObjectTypeLiteral) {
                if !self.ctx().encountered_error.get()
                    && !self.ctx().flags.get().intersects(Flags::AllowThisInObjectLiteral)
                {
                    self.ctx().encountered_error.set(true);
                }
                self.tracker().report_inaccessible_this_error();
            }
            add_approximate_length(self, 4);
            return Some(self.f.new_this_type_node());
        }

        let _decrement_depth = if in_type_alias.is_empty()
            && t.alias().is_some()
            && (self.ctx().flags.get().intersects(Flags::UseAliasDefinedOutsideCurrentScope)
                || c.is_type_symbol_accessible(
                    t.alias().symbol().unwrap(),
                    self.ctx().enclosing_declaration.get(),
                )) {
            // If we should expand this type alias, skip the alias and fall through to expand the underlying type
            if !self.should_expand_type(c, t, true /*isAlias*/) {
                let sym = t.alias().symbol().unwrap();
                let type_argument_nodes = self.map_to_type_nodes(
                    c,
                    t.alias().type_arguments(),
                    false, /*isBareList*/
                );
                if is_reserved_member_name(sym.name())
                    && !sym.flags().intersects(SymbolFlags::Class)
                {
                    return Some(
                        self.f.new_type_reference_node(
                            self.f.new_identifier(""),
                            type_argument_nodes,
                        ),
                    );
                }
                if let Some(type_argument_nodes) = type_argument_nodes
                    && type_argument_nodes.nodes().len() == 1
                    && Some(sym) == c.global_array_type.symbol()
                {
                    return Some(self.f.new_array_type_node(type_argument_nodes.nodes()[0]));
                }
                return self.symbol_to_type_node(c, sym, SymbolFlags::Type, type_argument_nodes);
            }
            // Expanding: increment depth and process the underlying type
            self.ctx().depth.set(self.ctx().depth.get() + 1);
            let b = self.as_p();
            Some(Defer(Box::new(move || {
                b.ctx().depth.set(b.ctx().depth.get() - 1);
            })))
        } else {
            None
        };

        let object_flags = t.object_flags();

        if object_flags.intersects(ObjectFlags::Reference) {
            assert!(t.flags().intersects(TypeFlags::Object));
            // When expanding, expand type references to their structural form
            if self.should_expand_type(c, t, false /*isAlias*/) {
                self.ctx().depth.set(self.ctx().depth.get() + 1);
                let result = self.create_anonymous_type_node_ex(
                    c, t, true, /*forceClassExpansion*/
                    true, /*forceExpansion*/
                );
                self.ctx().depth.set(self.ctx().depth.get() - 1);
                return result;
            }
            if c.is_array_or_tuple_type(t) {
                return self
                    .visit_and_transform_type(c, t, |c, b, t| b.array_or_tuple_type_to_node(c, t));
            } else if t.as_type_reference().node.get().is_some() {
                return self
                    .visit_and_transform_type(c, t, |c, b, t| b.type_reference_to_type_node(c, t));
            }
            return self.type_reference_to_type_node(c, t);
        }
        if t.flags().intersects(TypeFlags::TypeParameter)
            || object_flags.intersects(ObjectFlags::ClassOrInterface)
        {
            // When expanding class or interface types, show their structural form
            if object_flags.intersects(ObjectFlags::ClassOrInterface)
                && self.should_expand_type(c, t, false /*isAlias*/)
            {
                self.ctx().depth.set(self.ctx().depth.get() + 1);
                let result = self.create_anonymous_type_node_ex(
                    c, t, true, /*forceClassExpansion*/
                    true, /*forceExpansion*/
                );
                self.ctx().depth.set(self.ctx().depth.get() - 1);
                return result;
            }
            if t.flags().intersects(TypeFlags::TypeParameter)
                && self.ctx().infer_type_parameters.get().contains(&t)
            {
                add_approximate_length(self, ast::symbol_name(t.symbol().unwrap()).len() + 6);
                let mut constraint_node: Option<P<Node>> = None;
                let constraint = c.get_constraint_of_type_parameter(t);
                if let Some(constraint) = constraint {
                    // If the infer type has a constraint that is not the same as the constraint
                    // we would have normally inferred based on b, we emit the constraint
                    // using `infer T extends ?`. We omit inferred constraints from type references
                    // as they may be elided.
                    let inferred_constraint = c.get_inferred_type_parameter_constraint(
                        t, true, /*omitTypeReferences*/
                    );
                    if !(inferred_constraint.is_some()
                        && c.is_type_identical_to(constraint, inferred_constraint.unwrap()))
                    {
                        add_approximate_length(self, 9);
                        constraint_node = self.type_to_type_node(c, Some(constraint));
                    }
                }
                return Some(self.f.new_infer_type_node(
                    self.type_parameter_to_declaration_with_constraint(c, t, constraint_node),
                ));
            }
            if self.ctx().flags.get().intersects(Flags::GenerateNamesForShadowedTypeParams)
                && t.flags().intersects(TypeFlags::TypeParameter)
            {
                let name = self.type_parameter_to_name(c, t);
                add_approximate_length(self, name.text().len());
                return Some(self.f.new_type_reference_node(
                    self.new_identifier(c, name.text(), t.symbol()),
                    None, /*typeArguments*/
                ));
            }
            // Ignore constraint/default when creating a usage (as opposed to declaration) of a type parameter.
            if let Some(symbol) = t.symbol() {
                return self.symbol_to_type_node(c, symbol, SymbolFlags::Type, None);
            }
            let name: String = if (t == c.marker_super_type_for_check
                || t == c.marker_sub_type_for_check)
                && let Some(variance_type_parameter) = c.variance_type_parameter
                && let Some(symbol) = variance_type_parameter.symbol()
            {
                format!(
                    "{}{}",
                    if t == c.marker_sub_type_for_check { "sub-" } else { "super-" },
                    ast::symbol_name(symbol)
                )
            } else {
                "?".to_string()
            };
            return Some(self.f.new_type_reference_node(
                self.new_identifier(c, &name, None /*symbol*/),
                None, /*typeArguments*/
            ));
        }
        if t.flags().intersects(TypeFlags::Union) && t.as_union_type().origin().is_some() {
            t = t.as_union_type().origin().unwrap();
        }
        if t.flags().intersects(TypeFlags::Union | TypeFlags::Intersection) {
            let types: Vec<P<Type>> = if t.flags().intersects(TypeFlags::Union) {
                c.format_union_types(t.as_union_type().types.get(), expanding_enum)
            } else {
                t.as_intersection_type().types.get().to_vec()
            };
            if types.len() == 1 {
                return self.type_to_type_node(c, Some(types[0]));
            }
            let type_nodes = self.map_to_type_nodes(c, &types, true /*isBareList*/);
            if let Some(type_nodes) = type_nodes
                && !type_nodes.nodes().is_empty()
            {
                if t.flags().intersects(TypeFlags::Union) {
                    return Some(self.f.new_union_type_node(type_nodes));
                }
                return Some(self.f.new_intersection_type_node(type_nodes));
            }
            if !self.ctx().encountered_error.get()
                && !self.ctx().flags.get().intersects(Flags::AllowEmptyUnionOrIntersection)
            {
                self.ctx().encountered_error.set(true);
            }
            return None;
            // TODO: GH#18217
        }
        if object_flags.intersects(ObjectFlags::Anonymous | ObjectFlags::Mapped) {
            assert!(t.flags().intersects(TypeFlags::Object));
            // The type is an object literal type.
            return self.create_anonymous_type_node(c, t);
        }
        if t.flags().intersects(TypeFlags::Index) {
            let indexed_type = t.as_index_type().target.get();
            add_approximate_length(self, 6);
            let index_type_node = self.type_to_type_node(c, indexed_type);
            return Some(
                self.f.new_type_operator_node(Kind::KeyOfKeyword, index_type_node.unwrap()),
            );
        }
        if t.flags().intersects(TypeFlags::TemplateLiteral) {
            let texts = t.as_template_literal_type().texts.get();
            let types = t.as_template_literal_type().types.get();
            let template_head = self.f.new_template_head(texts[0], "", TokenFlags::None);
            self.e.add_emit_flags(template_head, EmitFlags::NoAsciiEscaping);
            let spans: Vec<P<Node>> = types
                .iter()
                .enumerate()
                .map(|(i, &t)| {
                    let res = if i < types.len() - 1 {
                        self.f.new_template_middle(texts[i + 1], "", TokenFlags::None)
                    } else {
                        self.f.new_template_tail(texts[i + 1], "", TokenFlags::None)
                    };
                    self.e.add_emit_flags(res, EmitFlags::NoAsciiEscaping);
                    let type_node = self.type_to_type_node(c, Some(t));
                    self.f.new_template_literal_type_span(type_node.unwrap(), res)
                })
                .collect();
            let template_spans = self.f.new_node_list(spans);
            add_approximate_length(self, 2);
            return Some(self.f.new_template_literal_type_node(template_head, template_spans));
        }
        if t.flags().intersects(TypeFlags::StringMapping) {
            let type_node = self.type_to_type_node(c, t.as_string_mapping_type().target.get());
            return self.symbol_to_type_node(
                c,
                t.symbol().unwrap(),
                SymbolFlags::Type,
                Some(self.f.new_node_list(vec![type_node.unwrap()])),
            );
        }
        if t.flags().intersects(TypeFlags::IndexedAccess) {
            let object_type_node =
                self.type_to_type_node(c, t.as_indexed_access_type().object_type.get());
            let index_type_node =
                self.type_to_type_node(c, t.as_indexed_access_type().index_type.get());
            add_approximate_length(self, 2);
            return Some(self.f.new_indexed_access_type_node(
                object_type_node.unwrap(),
                index_type_node.unwrap(),
            ));
        }
        if t.flags().intersects(TypeFlags::Conditional) {
            return self.visit_and_transform_type(c, t, |c, b, t| {
                Some(b.conditional_type_to_type_node(c, t))
            });
        }
        if t.flags().intersects(TypeFlags::Substitution) {
            let type_node = self.type_to_type_node(c, t.as_substitution_type().base_type.get());
            if !checker::Checker::is_no_infer_type(t) {
                return type_node;
            }
            let no_infer_symbol = c.get_global_type_alias_symbol("NoInfer", 1, false);
            if let Some(no_infer_symbol) = no_infer_symbol {
                return self.symbol_to_type_node(
                    c,
                    no_infer_symbol,
                    SymbolFlags::Type,
                    Some(self.f.new_node_list(vec![type_node.unwrap()])),
                );
            }
            return type_node;
        }

        panic!("Should be unreachable.");
    }

    // nodebuilderimpl.go:3624
    pub(crate) fn new_string_literal(&self, c: &mut Checker, text: &str) -> P<Node> {
        self.new_string_literal_ex(c, text, false /*isSingleQuote*/)
    }

    // nodebuilderimpl.go:3628
    pub(crate) fn new_string_literal_ex(
        &self,
        _c: &mut Checker,
        text: &str,
        is_single_quote: bool,
    ) -> P<Node> {
        let mut flags = TokenFlags::None;
        if is_single_quote
            || self.ctx().flags.get().intersects(Flags::UseSingleQuotesForStringLiteralType)
        {
            flags |= TokenFlags::SingleQuote;
        }

        self.f.new_string_literal(self.f.alloc_text(text), flags)
    }

    // Port-only: a string literal holding a symbol name. Go's internal-name prefix is the invalid byte 0xFE, which its
    // printer escapes as `\uFFFD`; the port's U+007F stand-in is user text elsewhere, so mark the literals where Go
    // would print that byte. Internal names have a non-empty rest: a lone U+007F is a user name (`"\u007f" = 127`).
    pub(crate) fn mark_symbol_name_literal(&self, node: P<Node>) -> P<Node> {
        if ast::is_string_literal(node)
            && node.text().len() > 1
            && node.text().as_bytes()[0] == INTERNAL_SYMBOL_NAME_PREFIX_BYTE
        {
            self.e.add_emit_flags(node, EmitFlags::InternalSymbolName);
        }
        node
    }
}

// Direct serialization core functions for types, type aliases, and symbols

impl TypeAlias {
    // nodebuilderimpl.go:3639
    fn to_type_reference_node(&self, c: &mut Checker, b: P<NodeBuilderImpl>) -> P<Node> {
        let type_name = b.symbol_to_entity_name_node(c, self.symbol().unwrap());
        let type_arguments =
            b.map_to_type_nodes(c, self.type_arguments(), false /*isBareList*/);
        b.f.new_type_reference_node(type_name, type_arguments)
    }
}

impl NodeBuilderImpl {
    // nodebuilderimpl.go:3643
    pub(crate) fn new_identifier(
        &self,
        _c: &mut Checker,
        text: &str,
        symbol: Option<P<Symbol>>,
    ) -> P<Node> {
        let id = self.f.new_identifier(self.f.alloc_text(text));
        if let Some(symbol) = symbol {
            self.id_to_symbol.borrow_mut().insert(id, symbol);
        }
        id
    }

    // nodebuilderimpl.go:3651
    pub(crate) fn create_access_expression(&self, node: P<Node>) -> P<Node> {
        if ast::is_qualified_name(node) {
            let qualified_name = node.as_qualified_name();
            let left = self.create_access_expression(qualified_name.left);
            self.f.new_property_access_expression(
                left,
                None, /*questionDotToken*/
                self.f.deep_clone_node(Some(qualified_name.right)).unwrap(),
                NodeFlags::None,
            )
        } else if ast::is_identifier(node)
            || ast::is_property_access_expression(node)
            || ast::is_expression_with_type_arguments(node)
        {
            self.f.deep_clone_node(Some(node)).unwrap()
        } else {
            panic!("unexpected access node kind: {}", node.kind_string());
        }
    }

    // nodebuilderimpl.go:3662
    pub(crate) fn create_expression_with_type_arguments(
        &self,
        _c: &mut Checker,
        expr: P<Node>,
        type_arguments: Option<P<NodeList>>,
    ) -> P<Node> {
        match type_arguments {
            None => return expr,
            Some(type_arguments) if type_arguments.nodes().is_empty() => return expr,
            _ => {}
        }
        self.f.new_expression_with_type_arguments(expr, type_arguments)
    }

    // nodebuilderimpl.go:3669
    pub(crate) fn lookup_instantiated_type_argument_nodes(
        &self,
        c: &mut Checker,
        chain: &[P<Symbol>],
        index: usize,
    ) -> Option<P<NodeList>> {
        if self.should_write_type_parameters_in_qualified_name(c, chain, index) {
            let symbol = chain[index];
            let next_symbol = chain[index + 1];
            if !next_symbol.check_flags().intersects(CheckFlags::Instantiated) {
                return None;
            }

            let mut target_symbol = symbol;
            if symbol.flags().intersects(SymbolFlags::Alias)
                && !checker::Checker::can_get_type_parameters_of_class_or_interface(symbol)
            {
                target_symbol = c.resolve_alias(symbol);
            }

            if !checker::Checker::can_get_type_parameters_of_class_or_interface(target_symbol) {
                return None;
            }

            let mut params = Self::get_type_parameters_of_class_or_interface(c, target_symbol);
            let target_mapper = c.value_symbol_links.get(next_symbol).mapper();
            if let Some(target_mapper) = target_mapper {
                params = params.iter().map(|&p| target_mapper.map(c, p)).collect();
            }
            return self.map_to_type_nodes(c, &params, false /*isBareList*/);
        }
        None
    }

    // nodebuilderimpl.go:3696
    pub(crate) fn lookup_expression_chain_type_argument_nodes(
        &self,
        c: &mut Checker,
        chain: &[P<Symbol>],
        index: usize,
    ) -> Option<P<NodeList>> {
        if self.should_write_type_parameters_in_qualified_name(c, chain, index) {
            let symbol = chain[index];
            let symbol_id = ast::get_symbol_id(symbol);
            if self.ctx().type_parameter_symbol_list.borrow().has(&symbol_id) {
                return None;
            }

            self.ctx().type_parameter_symbol_list.borrow_mut().add(symbol_id);
            if let Some(type_argument_nodes) =
                self.lookup_instantiated_type_argument_nodes(c, chain, index)
            {
                return Some(type_argument_nodes);
            }
            let type_parameter_nodes =
                self.type_parameters_to_type_parameter_declarations(c, symbol);
            if !type_parameter_nodes.is_empty() {
                return Some(self.f.new_node_list(type_parameter_nodes));
            }
        }
        None
    }

    // nodebuilderimpl.go:3716
    fn should_write_type_parameters_in_qualified_name(
        &self,
        _c: &mut Checker,
        chain: &[P<Symbol>],
        index: usize,
    ) -> bool {
        self.ctx().flags.get().intersects(Flags::WriteTypeParametersInQualifiedName)
            && index < chain.len() - 1
    }
}
