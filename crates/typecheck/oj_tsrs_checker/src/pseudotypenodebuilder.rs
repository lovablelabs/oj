use crate::*;
use pseudochecker::{PseudoObjectElementKind, PseudoTypeKind};
use tsrs_ast::*;

impl NodeBuilderImpl {
    // pseudotypenodebuilder.go:15
    // pseudoTypeToNodeWithCheckerFallback is like pseudoTypeToNode but when the top-level pseudo type
    // is PseudoTypeInferred, it reports any error nodes and then serializes from the checker's type.
    // This avoids incorrect type output when PseudoTypeInferred would derive the type from the
    // original declaration expression in an instantiated context.
    pub(crate) fn pseudo_type_to_node_with_checker_fallback(
        &self,
        c: &mut Checker,
        t: P<PseudoType>,
        checker_type: P<Type>,
    ) -> Option<P<Node>> {
        if t.kind == PseudoTypeKind::Inferred {
            let ctx = self.ctx();
            if !ctx.suppress_report_inference_fallback.get() {
                let error_nodes = t.as_pseudo_type_inferred().error_nodes;
                if error_nodes.is_empty() {
                    ctx.tracker
                        .get()
                        .unwrap()
                        .report_inference_fallback(c, t.as_pseudo_type_inferred().expression);
                } else {
                    for &n in error_nodes {
                        ctx.tracker.get().unwrap().report_inference_fallback(c, n);
                    }
                }
            }
            let old_suppress = ctx.suppress_report_inference_fallback.get();
            ctx.suppress_report_inference_fallback.set(true);
            let result = self.type_to_type_node(c, Some(checker_type));
            self.ctx().suppress_report_inference_fallback.set(old_suppress);
            return result;
        } else if t.kind == PseudoTypeKind::Direct {
            let existing = t.as_pseudo_type_direct().type_node;
            if !Self::can_reuse_existing_js_type_node(c, existing, checker_type) {
                let ctx = self.ctx();
                if !ctx.suppress_report_inference_fallback.get() {
                    ctx.tracker.get().unwrap().report_inference_fallback(c, existing);
                }
                let old_suppress = ctx.suppress_report_inference_fallback.get();
                ctx.suppress_report_inference_fallback.set(true);
                let result = self.type_to_type_node(c, Some(checker_type));
                self.ctx().suppress_report_inference_fallback.set(old_suppress);
                return result;
            }
        }
        self.pseudo_type_to_node(c, Some(t))
    }

    // pseudotypenodebuilder.go:48
    // Maps a pseudochecker's pseudotypes into ast nodes and reports any inference fallback errors the pseudotype structure implies
    fn pseudo_type_to_node(&self, c: &mut Checker, t: Option<P<PseudoType>>) -> Option<P<Node>> {
        assert!(t.is_some(), "Attempted to serialize nil pseudotype");
        let t = t.unwrap();
        match t.kind {
            PseudoTypeKind::Direct => self.reuse_type_node(c, t.as_pseudo_type_direct().type_node),
            PseudoTypeKind::Inferred => {
                let inferred = t.as_pseudo_type_inferred();
                let node = inferred.expression;
                let error_nodes = inferred.error_nodes;
                if !error_nodes.is_empty() {
                    for &n in error_nodes {
                        self.ctx().tracker.get().unwrap().report_inference_fallback(c, n);
                    }
                } else if is_entity_name_expression(node) && is_declaration(node.parent().unwrap())
                {
                    self.ctx()
                        .tracker
                        .get()
                        .unwrap()
                        .report_inference_fallback(c, node.parent().unwrap());
                } else {
                    self.ctx().tracker.get().unwrap().report_inference_fallback(c, node);
                }
                if inferred.is_signature_return {
                    let sig = c.get_signature_from_declaration(node);
                    return self.serialize_return_type_for_signature(c, sig, false);
                }
                // use symbol type from parent declaration to automatically handle expression type widening without duplicating logic
                let parent = node.parent().unwrap();
                if is_return_statement(parent) {
                    let enclosing = get_containing_function(node).unwrap();
                    if is_accessor(enclosing) {
                        return Some(self.serialize_type_for_declaration(
                            c,
                            Some(enclosing),
                            None,
                            None,
                            false,
                        ));
                    }
                    let sig = c.get_signature_from_declaration(enclosing);
                    return self.serialize_return_type_for_signature(c, sig, false);
                }
                if is_arrow_function(parent) && parent.as_arrow_function().body() == Some(node) {
                    let sig = c.get_signature_from_declaration(parent);
                    return self.serialize_return_type_for_signature(c, sig, false);
                }
                if is_declaration(parent) {
                    return Some(self.serialize_type_for_declaration(
                        c,
                        Some(parent),
                        None,
                        None,
                        false,
                    ));
                }
                // This might be effectively unreachable. If it's not, it may need more widening rules to mirror checker behavior for whatever expressions are serialized here
                let ty = c.get_type_of_expression(node);
                self.type_to_type_node(c, Some(ty))
            }
            PseudoTypeKind::NoResult => {
                let node = t.as_pseudo_type_no_result().declaration;
                self.ctx().tracker.get().unwrap().report_inference_fallback(c, node);
                if is_function_like(node) && !is_accessor(node) {
                    let sig = c.get_signature_from_declaration(node);
                    return self.serialize_return_type_for_signature(c, sig, false);
                }
                Some(self.serialize_type_for_declaration(c, Some(node), None, None, false))
            }
            PseudoTypeKind::MaybeConstLocation => {
                let d = t.as_pseudo_type_maybe_const_location();
                // see checkExpressionWithContextualType for general literal widening rules which need to be emulated here, plus
                // checkTemplateLiteralExpression for template literal widening rules if the pseudochecker ever supports literalized templates
                let mut is_in_const_context = c.is_const_context(d.node);
                if !is_in_const_context && pseudochecker::is_in_const_context(d.node) {
                    // Only consult the contextual type if the pseudochecker's syntactic check also puts us in a const context.
                    // getContextualType returns post-inference results at node-printing time which may not have existed
                    // during initial checking (e.g. when the contextual type depends on inference), causing incorrect
                    // literal type preservation.
                    let contextual_type = c.get_contextual_type(d.node, ContextFlags::None);
                    let t = Self::pseudo_type_to_type(c, Some(d.const_type));
                    if let Some(t) = t {
                        let instantiated = c.instantiate_contextual_type(
                            contextual_type,
                            d.node,
                            ContextFlags::None,
                        );
                        if c.is_literal_of_contextual_type(t, instantiated) {
                            is_in_const_context = true;
                        }
                    }
                }
                if is_in_const_context {
                    self.pseudo_type_to_node(c, Some(d.const_type))
                } else {
                    self.pseudo_type_to_node(c, Some(d.regular_type))
                }
            }
            PseudoTypeKind::Union => {
                fn append_type_node(
                    node: P<Node>,
                    res: &mut Vec<P<Node>>,
                    has_undefined: &mut bool,
                ) {
                    if is_union_type_node(node) {
                        for &node in node.as_union_type_node().types().nodes() {
                            append_type_node(node, res, has_undefined);
                        }
                        return;
                    }
                    if node.kind() == Kind::UndefinedKeyword {
                        if *has_undefined {
                            return;
                        }
                        *has_undefined = true;
                    }
                    res.push(node);
                }

                let mut res: Vec<P<Node>> = Vec::new();
                let mut has_elided_type = false;
                let mut has_undefined = false;
                let members = t.as_pseudo_type_union().types;
                for &m in members {
                    if !c.strict_null_checks
                        && (m.kind == PseudoTypeKind::Undefined || m.kind == PseudoTypeKind::Null)
                    {
                        has_elided_type = true;
                        continue;
                    }
                    let node = self.pseudo_type_to_node(c, Some(m)).unwrap();
                    append_type_node(node, &mut res, &mut has_undefined);
                }
                if res.len() == 1 {
                    return Some(res[0]);
                }
                if res.is_empty() {
                    if has_elided_type {
                        return Some(self.f.new_keyword_type_node(Kind::AnyKeyword));
                    }
                    return Some(self.f.new_keyword_type_node(Kind::NeverKeyword));
                }
                Some(self.f.new_union_type_node(self.f.new_node_list(res)))
            }
            PseudoTypeKind::Undefined => {
                if !c.strict_null_checks {
                    return Some(self.f.new_keyword_type_node(Kind::AnyKeyword));
                }
                Some(self.f.new_keyword_type_node(Kind::UndefinedKeyword))
            }
            PseudoTypeKind::Null => {
                if !c.strict_null_checks {
                    return Some(self.f.new_keyword_type_node(Kind::AnyKeyword));
                }
                Some(self.f.new_literal_type_node(self.f.new_keyword_expression(Kind::NullKeyword)))
            }
            PseudoTypeKind::Any => Some(self.f.new_keyword_type_node(Kind::AnyKeyword)),
            PseudoTypeKind::String => Some(self.f.new_keyword_type_node(Kind::StringKeyword)),
            PseudoTypeKind::Number => Some(self.f.new_keyword_type_node(Kind::NumberKeyword)),
            PseudoTypeKind::BigInt => Some(self.f.new_keyword_type_node(Kind::BigIntKeyword)),
            PseudoTypeKind::Boolean => Some(self.f.new_keyword_type_node(Kind::BooleanKeyword)),
            PseudoTypeKind::False => Some(
                self.f.new_literal_type_node(self.f.new_keyword_expression(Kind::FalseKeyword)),
            ),
            PseudoTypeKind::True => {
                Some(self.f.new_literal_type_node(self.f.new_keyword_expression(Kind::TrueKeyword)))
            }
            PseudoTypeKind::SingleCallSignature => {
                let d = t.as_pseudo_type_single_call_signature();
                let signature = c.get_signature_from_declaration(d.signature);
                let expanded_params = c
                    .get_expanded_parameters(signature, true /*skipUnionExpanding*/)
                    .swap_remove(0);
                let mut cleanup = self.enter_new_scope(
                    c,
                    Some(d.signature),
                    &expanded_params,
                    signature.type_parameters(),
                    Some(signature.parameters.get()),
                    signature.mapper.get(),
                );
                let mut type_params: Option<P<NodeList>> = None;
                if !d.type_parameters.is_empty() {
                    let mut res: Vec<P<Node>> = Vec::with_capacity(d.type_parameters.len());
                    res.extend(d.type_parameters.iter().map(|&tp| self.reuse_node(c, tp).unwrap()));
                    type_params = Some(self.f.new_node_list(res));
                }
                let params = self.pseudo_parameters_to_node_list(c, d.parameters);
                let return_type = self.pseudo_type_to_node(c, Some(d.return_type));
                let result = self.f.new_function_type_node(type_params, Some(params), return_type);
                cleanup(c);
                Some(result)
            }
            PseudoTypeKind::Tuple => {
                let mut res: Vec<P<Node>> = Vec::new();
                let elements = t.as_pseudo_type_tuple().elements;
                res.extend(elements.iter().map(|&e| self.pseudo_type_to_node(c, Some(e)).unwrap()));
                // pseudo-tuples are implicitly `readonly` since they originate from `as const` contexts
                // but strada *sometimes* fails to add the `readonly` modifier to the generated node.
                let result = self.f.new_tuple_type_node(self.f.new_node_list(res));
                self.e.add_emit_flags(result, EmitFlags::SingleLine);
                Some(self.f.new_type_operator_node(Kind::ReadonlyKeyword, result))
            }
            PseudoTypeKind::ObjectLiteral => {
                let elements = t.as_pseudo_type_object_literal().elements;
                if elements.is_empty() {
                    let result = self.f.new_type_literal_node(self.f.new_node_list(Vec::new()));
                    self.e.add_emit_flags(result, EmitFlags::SingleLine);
                    return Some(result);
                }
                // NOTE: using the checker's `isConstContext` instead of the pseudochecker's `isInConstContext`
                // results in different results here. The checker one is more "correct" but means we'll mark
                // objects in parameter positions contextually typed by const type parameters as readonly -
                // something a true syntactic ID emitter couldn't possibly know (since the signature could
                // be from across files). This can't *really* happen in any cases ID doesn't already error on, though.
                // Just something to keep in mind if the ID checker keeps growing.
                let is_const =
                    c.is_const_context(elements[0].name.parent().unwrap().parent().unwrap());
                let mut new_elements: Vec<P<Node>> = Vec::with_capacity(elements.len());

                // Member types are serialized within an object type literal, so set the
                // corresponding flag to mirror createTypeNodeFromObjectType. This ensures
                // inaccessible `this` references inside the members are reported (TS2527).
                let mut restore_object_literal_flags = self.save_restore_flags(c);
                self.ctx().flags.set(self.ctx().flags.get() | Flags::InObjectTypeLiteral);

                for &e in elements {
                    let mut modifiers: Option<P<ModifierList>> = None;
                    if is_const
                        || (e.kind == PseudoObjectElementKind::PropertyAssignment
                            && e.as_pseudo_property_assignment().readonly)
                    {
                        modifiers =
                            Some(self.f.new_modifier_list(vec![
                                self.f.new_modifier(Kind::ReadonlyKeyword),
                            ]));
                    }
                    let mut cleanup: Option<Box<dyn FnMut(&mut Checker)>> = None;
                    if e.kind != PseudoObjectElementKind::PropertyAssignment {
                        let signature = c.get_signature_from_declaration(e.signature().unwrap());
                        let expanded_params = c
                            .get_expanded_parameters(signature, true /*skipUnionExpanding*/)
                            .swap_remove(0);
                        cleanup = Some(self.enter_new_scope(
                            c,
                            e.signature(),
                            &expanded_params,
                            signature.type_parameters(),
                            Some(signature.parameters.get()),
                            signature.mapper.get(),
                        ));
                    }
                    let new_prop: P<Node> = match e.kind {
                        PseudoObjectElementKind::Method => {
                            let d = e.as_pseudo_object_method();
                            let mut type_params: Option<P<NodeList>> = None;
                            if !d.type_parameters.is_empty() {
                                let mut res: Vec<P<Node>> =
                                    Vec::with_capacity(d.type_parameters.len());
                                res.extend(
                                    d.type_parameters
                                        .iter()
                                        .map(|&tp| self.reuse_node(c, tp).unwrap()),
                                );
                                type_params = Some(self.f.new_node_list(res));
                            }
                            if is_const {
                                let name = self.reuse_name(c, e.name, false /*isMethod*/).unwrap();
                                let params = self.pseudo_parameters_to_node_list(c, d.parameters);
                                let return_type = self.pseudo_type_to_node(c, Some(d.return_type));
                                self.f.new_property_signature_declaration(
                                    modifiers,
                                    name,
                                    None,
                                    Some(self.f.new_function_type_node(
                                        type_params,
                                        Some(params),
                                        return_type,
                                    )),
                                    None,
                                )
                            } else {
                                let name = self.reuse_name(c, e.name, true /*isMethod*/).unwrap();
                                let params = self.pseudo_parameters_to_node_list(c, d.parameters);
                                let return_type = self.pseudo_type_to_node(c, Some(d.return_type));
                                self.f.new_method_signature_declaration(
                                    modifiers,
                                    name,
                                    None,
                                    type_params,
                                    Some(params),
                                    return_type,
                                )
                            }
                        }
                        PseudoObjectElementKind::PropertyAssignment => {
                            let d = e.as_pseudo_property_assignment();
                            let name = self.reuse_name(c, e.name, false /*isMethod*/).unwrap();
                            let type_node = self.pseudo_type_to_node(c, Some(d.type_));
                            self.f.new_property_signature_declaration(
                                modifiers, name, None, type_node, None,
                            )
                        }
                        PseudoObjectElementKind::SetAccessor => {
                            let d = e.as_pseudo_set_accessor();
                            let name = self.reuse_name(c, e.name, false /*isMethod*/).unwrap();
                            let param = self.pseudo_parameter_to_node(c, d.parameter);
                            self.f.new_set_accessor_declaration(
                                None,
                                name,
                                None,
                                Some(self.f.new_node_list(vec![param])),
                                None,
                                None,
                                None,
                            )
                        }
                        PseudoObjectElementKind::GetAccessor => {
                            let d = e.as_pseudo_get_accessor();
                            let name = self.reuse_name(c, e.name, false /*isMethod*/).unwrap();
                            let type_node = self.pseudo_type_to_node(c, Some(d.type_));
                            self.f.new_get_accessor_declaration(
                                None, name, None, None, type_node, None, None,
                            )
                        }
                    };
                    if self.ctx().enclosing_file.get() == get_source_file_of_node(e.name) {
                        self.e.set_comment_range(new_prop, e.name.parent().unwrap().loc());
                    }
                    new_elements.push(new_prop);
                    if let Some(mut cleanup) = cleanup {
                        cleanup(c);
                    }
                }
                restore_object_literal_flags(c);
                let result = self.f.new_type_literal_node(self.f.new_node_list(new_elements));
                if !self.ctx().flags.get().intersects(Flags::MultilineObjectLiterals) {
                    self.e.add_emit_flags(result, EmitFlags::SingleLine);
                }
                Some(result)
            }
            PseudoTypeKind::StringLiteral
            | PseudoTypeKind::NumericLiteral
            | PseudoTypeKind::BigIntLiteral => {
                let source = t.as_pseudo_type_literal().node;
                Some(self.f.new_literal_type_node(self.reuse_node(c, source).unwrap()))
            }
        }
    }

    // pseudotypenodebuilder.go:327
    fn pseudo_parameters_to_node_list(
        &self,
        c: &mut Checker,
        params: &[P<PseudoParameter>],
    ) -> P<NodeList> {
        let mut res: Vec<P<Node>> = Vec::with_capacity(params.len());
        res.extend(params.iter().map(|&p| self.pseudo_parameter_to_node(c, p)));
        self.f.new_node_list(res)
    }

    // pseudotypenodebuilder.go:335
    fn pseudo_parameter_to_node(&self, c: &mut Checker, p: P<PseudoParameter>) -> P<Node> {
        let mut dot_dot_dot: Option<P<Node>> = None;
        let mut question_mark: Option<P<Node>> = None;
        if p.rest {
            dot_dot_dot = Some(self.f.new_token(Kind::DotDotDotToken));
        }
        if p.optional {
            question_mark = Some(self.f.new_token(Kind::QuestionToken));
        }
        let name_parent = p.name.parent().unwrap();
        // matches strada behavior of always reserializing param names from scratch
        let name = self.parameter_to_parameter_declaration_name(
            c,
            name_parent.symbol().unwrap(),
            Some(name_parent),
        );
        let type_node = self.pseudo_type_to_node(c, Some(p.type_));
        let parameter = self.f.new_parameter_declaration(
            None,
            dot_dot_dot,
            name,
            question_mark,
            type_node,
            None,
        );
        let original = name_parent;
        if is_parameter_declaration(original) {
            self.set_comment_range(c, parameter, Some(original));
        }
        parameter
    }

    // pseudotypenodebuilder.go:362
    // see `typeNodeIsEquivalentToType` in strada, but applied more broadly here, so is setup to handle more equivalences - strada only used it via
    // the `canReuseTypeNodeAnnotation` host hook and not the `canReuseTypeNode` hook, which meant locations using the later were reliant on
    // over-invalidation by the ID inference engine to not emit incorrect types.
    pub(crate) fn pseudo_type_equivalent_to_type(
        &self,
        c: &mut Checker,
        t: Option<P<PseudoType>>,
        type_: Option<P<Type>>,
        is_optional_annotated: bool,
        report_errors: bool,
    ) -> bool {
        // if type_ resolves to an error, we charitably assume equality, since we might be in a single-file checking mode
        if let Some(ty) = type_
            && c.is_error_type(ty)
        {
            return true;
        }
        // If we can easily operate on just types, we should
        let type_from_pseudo = Self::pseudo_type_to_type(c, t); // note: cannot convert complex types like objects, which must be validated separately
        if type_from_pseudo == type_ {
            return true;
        }
        let mut undefined_stripped = type_;
        if is_optional_annotated {
            undefined_stripped =
                Some(c.get_type_with_facts(type_.unwrap(), TypeFacts::NEUndefined));
        }
        if let (Some(type_from_pseudo), Some(ty)) = (type_from_pseudo, type_) {
            let undefined_stripped = undefined_stripped.unwrap();
            if is_optional_annotated {
                if undefined_stripped == type_from_pseudo {
                    return true;
                }
                if type_from_pseudo.flags().intersects(TypeFlags::Union)
                    && undefined_stripped.flags().intersects(TypeFlags::Union)
                {
                    // does union comparison in general, since the unions may not be `==` identical due to aliasing and the like
                    if c.compare_types_identical(type_from_pseudo, undefined_stripped)
                        == Ternary::TRUE
                    {
                        return true;
                    }
                }
            }
            // handles freshness mismatches (e.g., fresh true vs regular true in as const)
            if c.get_regular_type_of_literal_type(type_from_pseudo)
                == c.get_regular_type_of_literal_type(ty)
            {
                return true;
            }
            if type_from_pseudo.flags().intersects(TypeFlags::Union)
                && ty.flags().intersects(TypeFlags::Union)
            {
                // handles union comparison in general, since unions may not be `==` identical due to aliasing
                if c.compare_types_identical(type_from_pseudo, ty) == Ternary::TRUE {
                    return true;
                }
            }
        }
        let t = t.unwrap();
        // otherwise, fallback to actual pseudo/type cross-comparisons
        match t.kind {
            PseudoTypeKind::Inferred => {
                // PseudoTypeInferred with error nodes identifies specific problematic children.
                // Report fine-grained errors on them, then return false so the parent falls back
                // to checker-based serialization (avoiding issues like reusing raw JSON string
                // literal property names from the pseudochecker's AST).
                let error_nodes = t.as_pseudo_type_inferred().error_nodes;
                if !error_nodes.is_empty() {
                    if report_errors {
                        for &n in error_nodes {
                            self.ctx().tracker.get().unwrap().report_inference_fallback(c, n);
                        }
                    }
                    return false;
                }
                if report_errors {
                    self.ctx()
                        .tracker
                        .get()
                        .unwrap()
                        .report_inference_fallback(c, t.as_pseudo_type_inferred().expression);
                }
                false
            }
            PseudoTypeKind::ObjectLiteral => {
                let pt = t.as_pseudo_type_object_literal();
                if type_.is_none() {
                    return false;
                }
                let undefined_stripped = undefined_stripped.unwrap();
                let target_props = c.get_properties_of_type(undefined_stripped);
                // Count total declarations across all target prop symbols to handle getter/setter pairs,
                // which are two elements in pt.Elements but only one symbol in targetProps.
                let mut target_decl_count = 0;
                target_decl_count =
                    target_props.iter().fold(target_decl_count, |mut target_decl_count, prop| {
                        target_decl_count += prop.declarations().len();

                        target_decl_count
                    });
                if pt.elements.len() != target_decl_count {
                    return false;
                }
                for &e in pt.elements {
                    let mut target_prop: Option<P<Symbol>> = None;
                    let elem_symbol = e.name.parent().unwrap().symbol();
                    if let Some(elem_symbol) = elem_symbol {
                        target_prop =
                            c.get_property_of_type(undefined_stripped, elem_symbol.name());
                    }
                    if target_prop.is_none() {
                        // Name lookup failed or returned no result; search target properties
                        // for one whose declaration name node matches the one we have
                        for &prop in target_props {
                            if let Some(value_declaration) = prop.value_declaration()
                                && value_declaration.name() == Some(e.name)
                            {
                                target_prop = Some(prop);
                                break;
                            }
                        }
                        if target_prop.is_none() {
                            if report_errors {
                                self.ctx()
                                    .tracker
                                    .get()
                                    .unwrap()
                                    .report_inference_fallback(c, e.name.parent().unwrap());
                            }
                            return false;
                        }
                    }
                    let target_prop = target_prop.unwrap();
                    let target_is_optional = target_prop.flags().intersects(SymbolFlags::Optional);
                    if e.optional != target_is_optional {
                        if report_errors {
                            self.ctx()
                                .tracker
                                .get()
                                .unwrap()
                                .report_inference_fallback(c, e.name.parent().unwrap());
                        }
                        return false;
                    }
                    let mut prop_type = c.get_type_of_symbol(target_prop);
                    prop_type = c.remove_missing_type(prop_type, target_is_optional);
                    match e.kind {
                        PseudoObjectElementKind::PropertyAssignment => {
                            let d = e.as_pseudo_property_assignment();
                            if !self.pseudo_type_equivalent_to_type(
                                c,
                                Some(d.type_),
                                Some(prop_type),
                                e.optional,
                                false,
                            ) {
                                if report_errors {
                                    if d.type_.kind == PseudoTypeKind::Inferred
                                        && !d.type_.as_pseudo_type_inferred().error_nodes.is_empty()
                                    {
                                        // Re-report the fine-grained error nodes; the recursive call used reportErrors=false
                                        for &n in d.type_.as_pseudo_type_inferred().error_nodes {
                                            self.ctx()
                                                .tracker
                                                .get()
                                                .unwrap()
                                                .report_inference_fallback(c, n);
                                        }
                                    } else if !is_structural_pseudo_type(d.type_) {
                                        self.ctx()
                                            .tracker
                                            .get()
                                            .unwrap()
                                            .report_inference_fallback(c, e.name.parent().unwrap());
                                    }
                                }
                                return false;
                            }
                        }
                        PseudoObjectElementKind::Method => {
                            let d = e.as_pseudo_object_method();
                            let target_sig = c.get_single_call_signature(prop_type);
                            let Some(target_sig) = target_sig else {
                                // Target property type doesn't have a single call signature; can't validate
                                continue;
                            };
                            let param_eq = self.pseudo_parameters_equivalent_to_parameters(
                                c,
                                d.parameters,
                                target_sig,
                                report_errors,
                                e.name.parent().unwrap(),
                            );
                            if !param_eq {
                                return false;
                            }
                            let target_predicate = c.get_type_predicate_of_signature(target_sig);
                            if let Some(target_predicate) = target_predicate {
                                if !Self::pseudo_return_type_matches_predicate(
                                    c,
                                    Some(d.return_type),
                                    target_predicate,
                                ) {
                                    if report_errors {
                                        self.ctx()
                                            .tracker
                                            .get()
                                            .unwrap()
                                            .report_inference_fallback(c, e.name.parent().unwrap());
                                    }
                                    return false;
                                }
                            } else {
                                let return_type = c.get_return_type_of_signature(target_sig);
                                if !self.pseudo_type_equivalent_to_type(
                                    c,
                                    Some(d.return_type),
                                    Some(return_type),
                                    false,
                                    false,
                                ) {
                                    if report_errors {
                                        self.ctx()
                                            .tracker
                                            .get()
                                            .unwrap()
                                            .report_inference_fallback(c, e.name.parent().unwrap());
                                    }
                                    return false;
                                }
                            }
                        }
                        PseudoObjectElementKind::GetAccessor => {
                            let d = e.as_pseudo_get_accessor();
                            if !self.pseudo_type_equivalent_to_type(
                                c,
                                Some(d.type_),
                                Some(prop_type),
                                false,
                                false,
                            ) {
                                if report_errors {
                                    self.ctx()
                                        .tracker
                                        .get()
                                        .unwrap()
                                        .report_inference_fallback(c, e.name.parent().unwrap());
                                }
                                return false;
                            }
                        }
                        PseudoObjectElementKind::SetAccessor => {
                            let d = e.as_pseudo_set_accessor();
                            let write_type = c.get_write_type_of_symbol(target_prop);
                            if !self.pseudo_type_equivalent_to_type(
                                c,
                                Some(d.parameter.type_),
                                write_type,
                                false,
                                false,
                            ) {
                                if report_errors {
                                    self.ctx()
                                        .tracker
                                        .get()
                                        .unwrap()
                                        .report_inference_fallback(c, e.name.parent().unwrap());
                                }
                                return false;
                            }
                        }
                    }
                }
                true
            }
            PseudoTypeKind::Tuple => {
                let pt = t.as_pseudo_type_tuple();
                let Some(undefined_stripped) = undefined_stripped else {
                    return false;
                };
                if !is_tuple_type(undefined_stripped) {
                    return false;
                }
                let tuple_target = undefined_stripped.target_tuple_type();
                // Pseudo-tuples come from `as const` array literals, so they only ever have required elements.
                // If the target tuple has optional, rest, or variadic elements, the structures can't match.
                if tuple_target.combined_flags.get().intersects(ElementFlags::NonRequired) {
                    return false;
                }
                let element_types = c.get_type_arguments(undefined_stripped);
                if pt.elements.len() != element_types.len() {
                    return false;
                }
                !(pt.elements.iter().enumerate().any(|(i, &elem)| {
                    !self.pseudo_type_equivalent_to_type(
                        c,
                        Some(elem),
                        Some(element_types[i]),
                        false,
                        report_errors,
                    )
                }))
            }
            PseudoTypeKind::SingleCallSignature => {
                let target_sig = c.get_single_call_signature(undefined_stripped.unwrap());
                let Some(target_sig) = target_sig else {
                    return false;
                };
                let pt = t.as_pseudo_type_single_call_signature();
                if target_sig.type_parameters().len() != pt.type_parameters.len() {
                    if report_errors {
                        self.ctx()
                            .tracker
                            .get()
                            .unwrap()
                            .report_inference_fallback(c, pt.signature);
                    }
                    return false;
                }
                let param_eq = self.pseudo_parameters_equivalent_to_parameters(
                    c,
                    pt.parameters,
                    target_sig,
                    report_errors,
                    pt.signature,
                );
                if !param_eq {
                    return false;
                }
                let target_predicate = c.get_type_predicate_of_signature(target_sig);
                if let Some(target_predicate) = target_predicate {
                    if !Self::pseudo_return_type_matches_predicate(
                        c,
                        Some(pt.return_type),
                        target_predicate,
                    ) {
                        if report_errors {
                            self.ctx()
                                .tracker
                                .get()
                                .unwrap()
                                .report_inference_fallback(c, pt.signature);
                        }
                        return false;
                    }
                } else {
                    let return_type = c.get_return_type_of_signature(target_sig);
                    if !self.pseudo_type_equivalent_to_type(
                        c,
                        Some(pt.return_type),
                        Some(return_type),
                        false,
                        report_errors,
                    ) {
                        // error reported within the return type
                        return false;
                    }
                }
                true
            }
            PseudoTypeKind::NoResult => {
                if report_errors {
                    self.ctx()
                        .tracker
                        .get()
                        .unwrap()
                        .report_inference_fallback(c, t.as_pseudo_type_no_result().declaration);
                }
                false
            }
            _ => false,
        }
    }

    // pseudotypenodebuilder.go:585
    fn pseudo_parameters_equivalent_to_parameters(
        &self,
        c: &mut Checker,
        params: &[P<PseudoParameter>],
        target_sig: P<Signature>,
        report_errors: bool,
        non_param_error_location: P<Node>,
    ) -> bool {
        let mut params = params;
        let this_parameter = target_sig.this_parameter();
        if this_parameter.is_some() && params.is_empty() {
            if report_errors {
                self.ctx()
                    .tracker
                    .get()
                    .unwrap()
                    .report_inference_fallback(c, non_param_error_location); // missing `this` param
            }
            return false;
        } else if this_parameter.is_some() && is_this_identifier(params[0].name) {
            let target_param = this_parameter.unwrap();
            let param_type = c.get_type_of_parameter(target_param);
            if !self.pseudo_type_equivalent_to_type(
                c,
                Some(params[0].type_),
                Some(param_type),
                params[0].optional,
                false,
            ) {
                if report_errors {
                    self.ctx()
                        .tracker
                        .get()
                        .unwrap()
                        .report_inference_fallback(c, params[0].name.parent().unwrap());
                }
                return false;
            }
            params = &params[1..];
        } else if this_parameter.is_some() {
            if report_errors {
                self.ctx()
                    .tracker
                    .get()
                    .unwrap()
                    .report_inference_fallback(c, non_param_error_location);
            }
            return false;
        }
        let target_params = target_sig.parameters.get();
        if target_params.len() != params.len() {
            if report_errors {
                self.ctx()
                    .tracker
                    .get()
                    .unwrap()
                    .report_inference_fallback(c, non_param_error_location);
            }
            return false; // TODO: spread tuple params may mess with this check
        }
        for (i, &p) in params.iter().enumerate() {
            let target_param = target_params[i];
            if p.optional != c.is_optional_parameter(target_param.value_declaration().unwrap()) {
                if report_errors {
                    self.ctx()
                        .tracker
                        .get()
                        .unwrap()
                        .report_inference_fallback(c, p.name.parent().unwrap());
                }
                return false;
            }
            let param_type = c.get_type_of_parameter(target_param);
            if !self.pseudo_type_equivalent_to_type(
                c,
                Some(p.type_),
                Some(param_type),
                p.optional,
                false,
            ) {
                if report_errors {
                    self.ctx()
                        .tracker
                        .get()
                        .unwrap()
                        .report_inference_fallback(c, p.name.parent().unwrap());
                }
                return false;
            }
        }
        true
    }
}

// pseudotypenodebuilder.go:632
pub fn is_structural_pseudo_type(t: P<PseudoType>) -> bool {
    match t.kind {
        PseudoTypeKind::ObjectLiteral
        | PseudoTypeKind::Tuple
        | PseudoTypeKind::SingleCallSignature => true,
        PseudoTypeKind::MaybeConstLocation => {
            let d = t.as_pseudo_type_maybe_const_location();
            is_structural_pseudo_type(d.const_type) || is_structural_pseudo_type(d.regular_type)
        }
        _ => false,
    }
}

impl NodeBuilderImpl {
    // pseudotypenodebuilder.go:645
    // pseudoReturnTypeMatchesPredicate checks if a pseudo return type (which should be a Direct type
    // wrapping a TypePredicate) matches the given type predicate from the checker.
    pub(crate) fn pseudo_return_type_matches_predicate(
        c: &mut Checker,
        rt: Option<P<PseudoType>>,
        predicate: P<TypePredicate>,
    ) -> bool {
        let rt = rt.unwrap();
        if rt.kind != PseudoTypeKind::Direct {
            return false;
        }
        let node = rt.as_pseudo_type_direct().type_node;
        if !is_type_predicate_node(node) {
            return false;
        }
        let tp = node.as_type_predicate_node();
        // Check asserts modifier matches
        let is_asserts = tp.asserts_modifier.is_some();
        let kind = predicate.kind.get();
        let predicate_is_asserts =
            kind == TypePredicateKind::AssertsThis || kind == TypePredicateKind::AssertsIdentifier;
        if is_asserts != predicate_is_asserts {
            return false;
        }
        // Check this vs identifier matches
        let is_this = is_this_type_node(tp.parameter_name);
        let predicate_is_this =
            kind == TypePredicateKind::This || kind == TypePredicateKind::AssertsThis;
        if is_this != predicate_is_this {
            return false;
        }
        // For identifier predicates, check parameter name matches
        if !is_this && tp.parameter_name.text() != predicate.parameter_name.get() {
            return false;
        }
        // Check the narrowed type, if any
        if let Some(predicate_t) = predicate.t.get() {
            let Some(tp_type) = tp.type_ else {
                return false;
            };
            let predicate_type_from_node = c.get_type_from_type_node(tp_type);
            if predicate_type_from_node != predicate_t
                && c.compare_types_identical(predicate_type_from_node, predicate_t) != Ternary::TRUE
            {
                return false;
            }
        } else if tp.type_.is_some() {
            return false;
        }
        true
    }

    // pseudotypenodebuilder.go:689
    pub(crate) fn pseudo_type_to_type(
        c: &mut Checker,
        t: Option<P<PseudoType>>,
    ) -> Option<P<Type>> {
        // !!! TODO: only literal types currently mapped because this is only used to determine if literal contextual typing need apply to the pseudotype
        // If this is used more broadly, the implementation needs to be filled out more to handle the structural pseudotypes - signatures, objects, tuples, etc
        assert!(t.is_some(), "Attempted to realize nil pseudotype");
        let t = t.unwrap();
        match t.kind {
            PseudoTypeKind::Direct => {
                Some(c.get_type_from_type_node(t.as_pseudo_type_direct().type_node))
            }
            PseudoTypeKind::Inferred => {
                let node = t.as_pseudo_type_inferred().expression;
                if t.as_pseudo_type_inferred().is_signature_return {
                    let sig = c.get_signature_from_declaration(node);
                    return Some(c.get_return_type_of_signature(sig));
                }
                let regular = c.get_regular_type_of_expression(node);
                let ty = c.get_widened_type(regular);
                Some(ty)
            }
            // TODO: extract type selection logic from `serializeTypeForDeclaration`, not needed for current usecases but needed if completeness becomes required
            PseudoTypeKind::NoResult
            // no simple mapping to a type, since these are structural types
            | PseudoTypeKind::ObjectLiteral
            | PseudoTypeKind::SingleCallSignature
            | PseudoTypeKind::Tuple => None,
            PseudoTypeKind::MaybeConstLocation => {
                let d = t.as_pseudo_type_maybe_const_location();
                if c.is_const_context(d.node) {
                    return Self::pseudo_type_to_type(c, Some(d.const_type));
                }
                Self::pseudo_type_to_type(c, Some(d.regular_type))
            }
            PseudoTypeKind::Union => {
                let mut res: Vec<P<Type>> = Vec::new();
                let mut has_elided_type = false;
                let members = t.as_pseudo_type_union().types;
                res = members.iter().try_fold(res, |mut res, &m| {
                    if !c.strict_null_checks
                        && (m.kind == PseudoTypeKind::Undefined || m.kind == PseudoTypeKind::Null)
                    {
                        has_elided_type = true;
                        return Some(res);
                    }
                    let t = Self::pseudo_type_to_type(c, Some(m));
                    let Some(t) = t else {
                        return None; // propagate failure
                    };
                    res.push(t);

                    Some(res)
                })?;
                if res.len() == 1 {
                    return Some(res[0]);
                }
                if res.is_empty() {
                    if has_elided_type {
                        return Some(c.any_type);
                    }
                    return Some(c.never_type);
                }
                Some(c.get_union_type(&res))
            }
            PseudoTypeKind::Undefined => Some(c.undefined_widening_type),
            PseudoTypeKind::Null => Some(c.null_widening_type),
            PseudoTypeKind::Any => Some(c.any_type),
            PseudoTypeKind::String => Some(c.string_type),
            PseudoTypeKind::Number => Some(c.number_type),
            PseudoTypeKind::BigInt => Some(c.bigint_type),
            PseudoTypeKind::Boolean => Some(c.boolean_type),
            PseudoTypeKind::False => Some(c.false_type),
            PseudoTypeKind::True => Some(c.true_type),
            PseudoTypeKind::StringLiteral
            | PseudoTypeKind::NumericLiteral
            | PseudoTypeKind::BigIntLiteral => {
                let source = t.as_pseudo_type_literal().node;
                Some(c.get_regular_type_of_expression(source)) // big shortcut, uses cached expression types where possible
            }
        }
    }
}
