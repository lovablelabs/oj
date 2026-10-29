use crate::*;
use std::borrow::Cow;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

// Go closure state of getInstantiationExpressionType.
struct InstantiationExpressionState {
    node: P<Node>,
    type_arguments: &'static [P<Node>],
    has_some_applicable_signature: bool,
    non_applicable_type: Option<P<Type>>,
}

fn get_instantiated_signatures(
    c: &mut Checker,
    st: &InstantiationExpressionState,
    signatures: &'static [P<Signature>],
) -> Cow<'static, [P<Signature>]> {
    let type_arguments = st.type_arguments;
    let applicable_signatures = tsrs_core::filter(signatures, |sig| {
        !sig.type_parameters().is_empty()
            && checker::Checker::has_correct_type_argument_arity(*sig, type_arguments)
    });
    let mapped = tsrs_core::same_map(&applicable_signatures, |sig| {
        let type_argument_types =
            c.check_type_arguments(*sig, type_arguments, true /*reportErrors*/, None);
        if !type_argument_types.is_empty() {
            return c.get_signature_instantiation(
                *sig,
                &type_argument_types,
                ast::is_in_js_file(sig.declaration.get()),
                &[],
            );
        }
        *sig
    });
    match mapped {
        Cow::Borrowed(_) => applicable_signatures,
        Cow::Owned(v) => Cow::Owned(v),
    }
}

fn get_instantiated_type(
    c: &mut Checker,
    st: &mut InstantiationExpressionState,
    t: P<Type>,
) -> P<Type> {
    let mut has_signatures = false;
    let mut has_applicable_signature = false;
    let result =
        get_instantiated_type_part(c, st, &mut has_signatures, &mut has_applicable_signature, t);
    st.has_some_applicable_signature = st.has_some_applicable_signature || has_applicable_signature;
    if has_signatures && !has_applicable_signature && st.non_applicable_type.is_none() {
        st.non_applicable_type = Some(t);
    }
    result
}

fn get_instantiated_type_part(
    c: &mut Checker,
    st: &mut InstantiationExpressionState,
    has_signatures: &mut bool,
    has_applicable_signature: &mut bool,
    t: P<Type>,
) -> P<Type> {
    if t.flags().intersects(TypeFlags::Object) {
        let resolved = c.resolve_structured_type_members(t);
        let call_signatures = get_instantiated_signatures(c, st, resolved.call_signatures());
        let construct_signatures =
            get_instantiated_signatures(c, st, resolved.construct_signatures());
        *has_signatures = *has_signatures
            || !resolved.call_signatures().is_empty()
            || !resolved.construct_signatures().is_empty();
        *has_applicable_signature = *has_applicable_signature
            || !call_signatures.is_empty()
            || !construct_signatures.is_empty();
        if !tsrs_core::same(&call_signatures, resolved.call_signatures())
            || !tsrs_core::same(&construct_signatures, resolved.construct_signatures())
        {
            let symbol =
                c.new_symbol(SymbolFlags::None, INTERNAL_SYMBOL_NAME_INSTANTIATION_EXPRESSION);
            assert!(
                t.symbol().is_some(),
                "Instantiation expression source type must have a symbol"
            );
            symbol.set_declarations_static(t.symbol().unwrap().declarations());
            let result = c.new_object_type(
                ObjectFlags::Anonymous | ObjectFlags::InstantiationExpressionType,
                Some(symbol),
            );
            c.set_structured_type_members(
                result,
                resolved.members(),
                &call_signatures,
                &construct_signatures,
                resolved.index_infos(),
            );
            result.as_instantiation_expression_type().node.set(Some(st.node));
            return result;
        }
    } else if t.flags().intersects(TypeFlags::InstantiableNonPrimitive) {
        let constraint = c.get_base_constraint_of_type(t);
        if let Some(constraint) = constraint {
            let instantiated = get_instantiated_type_part(
                c,
                st,
                has_signatures,
                has_applicable_signature,
                constraint,
            );
            if instantiated != constraint {
                return instantiated;
            }
        }
    } else if t.flags().intersects(TypeFlags::Union) {
        return c.map_type(t, |c, t| Some(get_instantiated_type(c, st, t))).unwrap();
    } else if t.flags().intersects(TypeFlags::Intersection) {
        let types = t.as_intersection_type().types.get();
        let mapped = tsrs_core::same_map(types, |t| {
            get_instantiated_type_part(c, st, has_signatures, has_applicable_signature, *t)
        });
        return c.get_intersection_type(&mapped);
    }
    t
}

impl Checker {
    // checker.go:10857
    pub(crate) fn get_instantiation_expression_type(
        &mut self,
        expr_type: P<Type>,
        node: P<Node>,
    ) -> P<Type> {
        let type_arguments = node.type_argument_list();
        if expr_type == self.silent_never_type
            || self.is_error_type(expr_type)
            || type_arguments.is_none()
        {
            return expr_type;
        }
        let type_arguments = type_arguments.unwrap();
        let key =
            InstantiationExpressionKey { node_id: ast::get_node_id(node), type_id: expr_type.id };
        if let Some(&cached) = self.instantiation_expression_types.get(&key) {
            return cached;
        }
        let mut st = InstantiationExpressionState {
            node,
            type_arguments: type_arguments.nodes(),
            has_some_applicable_signature: false,
            non_applicable_type: None,
        };
        let result = get_instantiated_type(self, &mut st, expr_type);
        self.instantiation_expression_types.insert(key, result);
        let error_type =
            if st.has_some_applicable_signature { st.non_applicable_type } else { Some(expr_type) };
        if let Some(error_type) = error_type {
            let source_file = ast::get_source_file_of_node(node).unwrap();
            let loc = TextRange::new(
                tsrs_scanner::skip_trivia(
                    source_file.text(),
                    type_arguments.pos().expect_source("parsed node lists have source positions"),
                ),
                type_arguments.end().expect_source("parsed node lists have source positions"),
            );
            let type_string = self.type_to_string_exported(error_type);
            self.add_diagnostic(ast::new_diagnostic(Some(source_file), loc, &diagnostics::TYPE_0_HAS_NO_SIGNATURES_FOR_WHICH_THE_TYPE_ARGUMENT_LIST_IS_APPLICABLE, &[&type_string]));
        }
        result
    }

    // checker.go:10941
    pub(crate) fn check_satisfies_expression(&mut self, node: P<Node>) -> P<Type> {
        let type_node = node.type_node().unwrap();
        self.check_source_element(Some(type_node));
        let expr_type = self.check_expression(node.expression().unwrap());
        let target_type = self.get_type_from_type_node(type_node);
        if self.is_error_type(target_type) {
            return target_type;
        }
        self.check_type_assignable_to_and_optionally_elaborate(
            expr_type,
            target_type,
            Some(node),
            Some(node.expression().unwrap()),
            Some(&diagnostics::TYPE_0_DOES_NOT_SATISFY_THE_EXPECTED_TYPE_1),
            None,
        );
        expr_type
    }

    // checker.go:10953
    pub(crate) fn check_meta_property(&mut self, node: P<Node>) -> P<Type> {
        self.check_grammar_meta_property(node);
        match node.as_meta_property().keyword_token {
            Kind::NewKeyword => {
                return self.check_new_target_meta_property(node);
            }
            Kind::ImportKeyword => {
                if ast::is_import_phase_meta_property(node) {
                    if ast::is_call_expression(node.parent().unwrap()) {
                        assert!(
                            node.parent().unwrap().expression() != Some(node),
                            "Trying to get the type of a phase import meta-property in its call"
                        );
                    }
                    return self.error_type;
                }
                return self.check_import_meta_property(node);
            }
            _ => {}
        }
        panic!("Unhandled case in checkMetaProperty")
    }

    // checker.go:10968
    pub(crate) fn check_new_target_meta_property(&mut self, node: P<Node>) -> P<Type> {
        let container = ast::get_new_target_container(node);
        let Some(container) = container else {
            self.error(Some(node), &diagnostics::META_PROPERTY_0_IS_ONLY_ALLOWED_IN_THE_BODY_OF_A_FUNCTION_DECLARATION_FUNCTION_EXPRESSION_OR_CONSTRUCTOR, &[&"new.target"]);
            return self.error_type;
        };
        if ast::is_constructor_declaration(container) {
            let symbol = self.get_symbol_of_declaration(container.parent().unwrap()).unwrap();
            return self.get_type_of_symbol(symbol);
        }
        let symbol = self.get_symbol_of_declaration(container).unwrap();
        self.get_type_of_symbol(symbol)
    }

    // checker.go:10982
    fn check_import_meta_property(&mut self, node: P<Node>) -> P<Type> {
        if ModuleKind::NODE16 <= self.module_kind && self.module_kind <= ModuleKind::NODE_NEXT {
            let source_file_meta_data = self
                .program
                .get_source_file_meta_data(ast::get_source_file_of_node(node).unwrap().path());
            if source_file_meta_data.implied_node_format != ModuleKind::ES_NEXT {
                self.error(Some(node), &diagnostics::THE_IMPORT_META_META_PROPERTY_IS_NOT_ALLOWED_IN_FILES_WHICH_WILL_BUILD_INTO_COMMON_JS_OUTPUT, &[]);
            }
        } else if self.module_kind < ModuleKind::ES2020 && self.module_kind != ModuleKind::SYSTEM {
            self.error(Some(node), &diagnostics::THE_IMPORT_META_META_PROPERTY_IS_ONLY_ALLOWED_WHEN_THE_MODULE_OPTION_IS_ES2020_ES2022_ESNEXT_SYSTEM_NODE16_NODE18_NODE20_OR_NODENEXT, &[]);
        }
        let file = ast::get_source_file_of_node(node).unwrap();
        assert!(
            file.as_node().flags().intersects(NodeFlags::PossiblyContainsImportMeta),
            "Containing file is missing import meta node flag."
        );
        if node.name().unwrap().text() == "meta" {
            return self.get_global_import_meta_type();
        }
        self.error_type
    }

    // checker.go:10999
    pub(crate) fn check_meta_property_keyword(&self, node: P<Node>) -> P<Type> {
        // !!! This is effectively a helper for GetSymbolAtLocation and GetTypeAtLocation
        let _ = node;
        self.error_type
    }

    // checker.go:11004
    pub(crate) fn check_delete_expression(&mut self, node: P<Node>) -> P<Type> {
        self.check_expression(node.expression().unwrap());
        let expr = ast::skip_parentheses(node.expression().unwrap());
        if !ast::is_access_expression(expr) {
            self.error(
                Some(expr),
                &diagnostics::THE_OPERAND_OF_A_DELETE_OPERATOR_MUST_BE_A_PROPERTY_REFERENCE,
                &[],
            );
            return self.boolean_type;
        }
        if ast::is_property_access_expression(expr)
            && ast::is_private_identifier(expr.name().unwrap())
        {
            self.error(
                Some(expr),
                &diagnostics::THE_OPERAND_OF_A_DELETE_OPERATOR_CANNOT_BE_A_PRIVATE_IDENTIFIER,
                &[],
            );
        }
        let resolved = self.get_resolved_symbol_or_nil(expr);
        let symbol = self.get_export_symbol_of_value_symbol_if_exported(resolved);
        if let Some(symbol) = symbol {
            if self.is_readonly_symbol(symbol) {
                self.error(
                    Some(expr),
                    &diagnostics::THE_OPERAND_OF_A_DELETE_OPERATOR_CANNOT_BE_A_READ_ONLY_PROPERTY,
                    &[],
                );
            } else {
                self.check_delete_expression_must_be_optional(expr, symbol);
            }
        }
        self.boolean_type
    }

    // checker.go:11025
    fn check_delete_expression_must_be_optional(&mut self, expr: P<Node>, symbol: P<Symbol>) {
        let t = self.get_type_of_symbol(symbol);
        if self.strict_null_checks
            && !t.flags().intersects(TypeFlags::AnyOrUnknown | TypeFlags::Never)
        {
            let is_optional = if self.exact_optional_property_types {
                symbol.flags().intersects(SymbolFlags::Optional)
            } else {
                self.has_type_facts(t, TypeFacts::IsUndefined)
            };
            if !is_optional {
                self.error(
                    Some(expr),
                    &diagnostics::THE_OPERAND_OF_A_DELETE_OPERATOR_MUST_BE_OPTIONAL,
                    &[],
                );
            }
        }
    }

    // checker.go:11040
    pub(crate) fn check_void_expression(&mut self, node: P<Node>) -> P<Type> {
        self.check_node_deferred(node);
        self.undefined_widening_type
    }

    // checker.go:11045
    pub(crate) fn check_await_expression(&mut self, node: P<Node>) -> P<Type> {
        self.check_grammar_await_or_await_using(node);
        let operand_type = self.check_expression(node.expression().unwrap());
        let awaited_type = self.check_awaited_type(operand_type, true /*withAlias*/, node, &diagnostics::TYPE_OF_AWAIT_OPERAND_MUST_EITHER_BE_A_VALID_PROMISE_OR_MUST_NOT_CONTAIN_A_CALLABLE_THEN_MEMBER);
        if awaited_type == operand_type
            && !self.is_error_type(awaited_type)
            && !operand_type.flags().intersects(TypeFlags::AnyOrUnknown)
        {
            self.add_error_or_suggestion(
                false,
                create_diagnostic_for_node(
                    Some(node),
                    &diagnostics::X_AWAIT_HAS_NO_EFFECT_ON_THE_TYPE_OF_THIS_EXPRESSION,
                    &[],
                ),
            );
        }
        awaited_type
    }

    // checker.go:11055
    pub(crate) fn check_prefix_unary_expression(&mut self, node: P<Node>) -> P<Type> {
        let expr = node.as_prefix_unary_expression();
        let operand_type = self.check_expression(expr.operand);
        if operand_type == self.silent_never_type {
            return self.silent_never_type;
        }
        match expr.operand.kind() {
            Kind::NumericLiteral => match expr.operator {
                Kind::MinusToken => {
                    let t = self.get_number_literal_type(-jsnum::from_string(expr.operand.text()));
                    return self.get_fresh_type_of_literal_type(t);
                }
                Kind::PlusToken => {
                    let t = self.get_number_literal_type(jsnum::from_string(expr.operand.text()));
                    return self.get_fresh_type_of_literal_type(t);
                }
                _ => {}
            },
            Kind::BigIntLiteral if expr.operator == Kind::MinusToken => {
                let t = self.get_big_int_literal_type(jsnum::new_pseudo_big_int(
                    &jsnum::parse_pseudo_big_int(expr.operand.text()),
                    true, /*negative*/
                ));
                return self.get_fresh_type_of_literal_type(t);
            }
            _ => {}
        }
        match expr.operator {
            Kind::PlusToken | Kind::MinusToken | Kind::TildeToken => {
                self.check_non_null_type(operand_type, expr.operand);
                if self.maybe_type_of_kind_considering_base_constraint(
                    operand_type,
                    TypeFlags::ESSymbolLike,
                ) {
                    self.error(
                        Some(expr.operand),
                        &diagnostics::THE_0_OPERATOR_CANNOT_BE_APPLIED_TO_TYPE_SYMBOL,
                        &[&tsrs_scanner::token_to_string(expr.operator)],
                    );
                }
                if expr.operator == Kind::PlusToken {
                    if self.maybe_type_of_kind_considering_base_constraint(
                        operand_type,
                        TypeFlags::BigIntLike,
                    ) {
                        let base = self.get_base_type_of_literal_type(operand_type);
                        let type_string = self.type_to_string_exported(base);
                        self.error(
                            Some(expr.operand),
                            &diagnostics::OPERATOR_0_CANNOT_BE_APPLIED_TO_TYPE_1,
                            &[&tsrs_scanner::token_to_string(expr.operator), &type_string],
                        );
                    }
                    return self.number_type;
                }
                self.get_unary_result_type(operand_type)
            }
            Kind::ExclamationToken => {
                self.check_truthiness_of_type(operand_type, expr.operand);
                let facts = self.get_type_facts(operand_type, TypeFacts::Truthy | TypeFacts::Falsy);
                if facts == TypeFacts::Truthy {
                    self.false_type
                } else if facts == TypeFacts::Falsy {
                    self.true_type
                } else {
                    self.boolean_type
                }
            }
            Kind::PlusPlusToken | Kind::MinusMinusToken => {
                let non_null = self.check_non_null_type(operand_type, expr.operand);
                let ok = self.check_arithmetic_operand_type(expr.operand, non_null, &diagnostics::AN_ARITHMETIC_OPERAND_MUST_BE_OF_TYPE_ANY_NUMBER_BIGINT_OR_AN_ENUM_TYPE, false);
                if ok {
                    // run check only if former checks succeeded to avoid reporting cascading errors
                    self.check_reference_expression(
                        expr.operand,
                        &diagnostics::THE_OPERAND_OF_AN_INCREMENT_OR_DECREMENT_OPERATOR_MUST_BE_A_VARIABLE_OR_A_PROPERTY_ACCESS,
                        &diagnostics::THE_OPERAND_OF_AN_INCREMENT_OR_DECREMENT_OPERATOR_MAY_NOT_BE_AN_OPTIONAL_PROPERTY_ACCESS,
                    );
                }
                self.get_unary_result_type(operand_type)
            }
            _ => self.error_type,
        }
    }

    // checker.go:11109
    pub(crate) fn check_postfix_unary_expression(&mut self, node: P<Node>) -> P<Type> {
        let expr = node.as_postfix_unary_expression();
        let operand_type = self.check_expression(expr.operand);
        if operand_type == self.silent_never_type {
            return self.silent_never_type;
        }
        let non_null = self.check_non_null_type(operand_type, expr.operand);
        let ok = self.check_arithmetic_operand_type(
            expr.operand,
            non_null,
            &diagnostics::AN_ARITHMETIC_OPERAND_MUST_BE_OF_TYPE_ANY_NUMBER_BIGINT_OR_AN_ENUM_TYPE,
            false,
        );
        if ok {
            // run check only if former checks succeeded to avoid reporting cascading errors
            self.check_reference_expression(
                expr.operand,
                &diagnostics::THE_OPERAND_OF_AN_INCREMENT_OR_DECREMENT_OPERATOR_MUST_BE_A_VARIABLE_OR_A_PROPERTY_ACCESS,
                &diagnostics::THE_OPERAND_OF_AN_INCREMENT_OR_DECREMENT_OPERATOR_MAY_NOT_BE_AN_OPTIONAL_PROPERTY_ACCESS,
            );
        }
        self.get_unary_result_type(operand_type)
    }

    // checker.go:11123
    fn get_unary_result_type(&mut self, operand_type: P<Type>) -> P<Type> {
        if self.maybe_type_of_kind(operand_type, TypeFlags::BigIntLike) {
            if self.is_type_assignable_to_kind(operand_type, TypeFlags::AnyOrUnknown)
                || self.maybe_type_of_kind(operand_type, TypeFlags::NumberLike)
            {
                return self.number_or_big_int_type;
            }
            return self.bigint_type;
        }
        // If it's not a bigint type, implicit coercion will result in a number
        self.number_type
    }

    // checker.go:11134
    pub(crate) fn check_conditional_expression(
        &mut self,
        node: P<Node>,
        check_mode: CheckMode,
    ) -> P<Type> {
        let cond = node.as_conditional_expression();
        let t = self.check_truthiness_expression(cond.condition, check_mode);
        self.check_testing_known_truthy_callable_or_awaitable_or_enum_member_type(
            cond.condition,
            t,
            Some(cond.when_true),
        );
        let type1 = self.check_expression_ex(cond.when_true, check_mode);
        let type2 = self.check_expression_ex(cond.when_false, check_mode);
        self.get_union_type_ex(&[type1, type2], UnionReduction::Subtype, AliasArg::None, None)
    }

    // checker.go:11143
    pub(crate) fn check_truthiness_expression(
        &mut self,
        node: P<Node>,
        check_mode: CheckMode,
    ) -> P<Type> {
        let t = self.check_expression_ex(node, check_mode);
        self.check_truthiness_of_type(t, node)
    }

    // checker.go:11147
    pub(crate) fn check_spread_expression(
        &mut self,
        node: P<Node>,
        check_mode: CheckMode,
    ) -> P<Type> {
        let array_or_iterable_type =
            self.check_expression_ex(node.expression().unwrap(), check_mode);
        let undefined_type = self.undefined_type;
        self.check_iterated_type_or_element_type(
            IterationUse::Spread,
            array_or_iterable_type,
            undefined_type,
            node.expression(),
        )
    }

    // checker.go:11152
    pub(crate) fn check_yield_expression(&mut self, node: P<Node>) -> P<Type> {
        self.check_grammar_yield_expression(node);
        // Always check the operand so its identifiers are resolved even when the yield is
        // outside a generator, keeping diagnostics stable regardless of traversal order.
        let yield_expression_type = if let Some(expression) = node.expression() {
            self.check_expression(expression)
        } else {
            self.undefined_widening_type
        };
        let Some(fn_) = ast::get_containing_function(node) else {
            return self.any_type;
        };
        let function_flags = ast::get_function_flags(Some(fn_));
        if !function_flags.intersects(FunctionFlags::Generator) {
            // If the user's code is syntactically correct, the func should always have a star. After all, we are in a yield context.
            return self.any_type;
        }
        let is_async = function_flags.intersects(FunctionFlags::Async);
        if node.as_yield_expression().asterisk_token.is_some() {
            // Async generator functions prior to ES2018 require the __await, __asyncDelegator,
            // and __asyncValues helpers
            if is_async && self.language_version < LANGUAGE_FEATURE_MINIMUM_TARGET.async_generators
            {
                self.check_external_emit_helpers(node, ExternalEmitHelpers::AsyncDelegatorIncludes);
            }
        }
        // There is no point in doing an assignability check if the function
        // has no explicit return type because the return type is directly computed
        // from the yield expressions.
        let mut return_type = self.get_return_type_from_annotation(fn_);
        if let Some(rt) = return_type
            && rt.flags().intersects(TypeFlags::Union)
        {
            return_type = Some(self.filter_type(rt, |c, t| {
                c.check_generator_instantiation_assignability_to_return_type(
                    t,
                    function_flags,
                    None, /*errorNode*/
                )
            }));
        }
        let mut iteration_types = IterationTypes::default();
        if let Some(rt) = return_type {
            iteration_types =
                self.get_iteration_types_of_generator_function_return_type(rt, is_async);
        }
        let signature_yield_type = iteration_types.yield_type.unwrap_or(self.any_type);
        let signature_next_type = iteration_types.next_type.unwrap_or(self.any_type);
        let yielded_type = self.get_yielded_type_of_yield_expression(
            node,
            yield_expression_type,
            signature_next_type,
            is_async,
        );
        if let (Some(_), Some(yielded_type)) = (return_type, yielded_type) {
            let error_node = node.expression().unwrap_or(node);
            self.check_type_assignable_to_and_optionally_elaborate(
                yielded_type,
                signature_yield_type,
                Some(error_node),
                node.expression(),
                None,
                None,
            );
        }
        if node.as_yield_expression().asterisk_token.is_some() {
            let use_ =
                if is_async { IterationUse::AsyncYieldStar } else { IterationUse::YieldStar };
            return self
                .get_iteration_type_of_iterable(
                    use_,
                    IterationTypeKind::Return,
                    yield_expression_type,
                    node.expression(),
                )
                .unwrap_or(self.any_type);
        }
        if let Some(rt) = return_type {
            return self
                .get_iteration_type_of_generator_function_return_type(
                    IterationTypeKind::Next,
                    rt,
                    is_async,
                )
                .unwrap_or(self.any_type);
        }
        let mut t = self.get_contextual_iteration_type(IterationTypeKind::Next, fn_);
        if t.is_none() {
            t = Some(self.any_type);
            if self.no_implicit_any && !expression_result_is_unused(node) {
                let contextual_type = self.get_contextual_type(node, ContextFlags::None);
                if contextual_type.is_none() || is_type_any(contextual_type) {
                    self.error(Some(node), &diagnostics::X_YIELD_EXPRESSION_IMPLICITLY_RESULTS_IN_AN_ANY_TYPE_BECAUSE_ITS_CONTAINING_GENERATOR_LACKS_A_RETURN_TYPE_ANNOTATION, &[]);
                }
            }
        }
        t.unwrap()
    }

    // checker.go:11218
    pub(crate) fn get_yielded_type_of_yield_expression(
        &mut self,
        node: P<Node>,
        expression_type: P<Type>,
        sent_type: P<Type>,
        is_async: bool,
    ) -> Option<P<Type>> {
        let error_node = node.expression().unwrap_or(node);
        let is_yield_star = node.as_yield_expression().asterisk_token.is_some();
        // A `yield*` expression effectively yields everything that its operand yields
        let mut yielded_type = expression_type;
        if is_yield_star {
            yielded_type = self.check_iterated_type_or_element_type(
                if is_async { IterationUse::AsyncYieldStar } else { IterationUse::YieldStar },
                expression_type,
                sent_type,
                Some(error_node),
            );
        }
        if !is_async {
            return Some(yielded_type);
        }
        self.get_awaited_type_ex(
            yielded_type,
            Some(error_node),
            Some(if is_yield_star {
                &diagnostics::TYPE_OF_ITERATED_ELEMENTS_OF_A_YIELD_ASTERISK_OPERAND_MUST_EITHER_BE_A_VALID_PROMISE_OR_MUST_NOT_CONTAIN_A_CALLABLE_THEN_MEMBER
            } else {
                &diagnostics::TYPE_OF_YIELD_OPERAND_IN_AN_ASYNC_GENERATOR_MUST_EITHER_BE_A_VALID_PROMISE_OR_MUST_NOT_CONTAIN_A_CALLABLE_THEN_MEMBER
            }),
            &[],
        )
    }

    // checker.go:11234
    pub(crate) fn check_synthetic_expression(&mut self, node: P<Node>) -> P<Type> {
        let t = synthetic_expression_type(node);
        if node.as_synthetic_expression().is_spread {
            let number_type = self.number_type;
            return self.get_indexed_access_type(t, number_type);
        }
        t
    }

    // checker.go:11242
    pub(crate) fn check_identifier(&mut self, node: P<Node>, check_mode: CheckMode) -> P<Type> {
        if ast::is_this_in_type_query(node) {
            return self.check_this_expression(node);
        }
        let symbol = self.get_resolved_symbol(node);
        if symbol == self.unknown_symbol {
            return self.error_type;
        }
        if symbol == self.arguments_symbol {
            if Self::is_in_property_initializer_or_class_static_block(
                node, true, /*ignoreArrowFunctions*/
            ) {
                self.error(Some(node), &diagnostics::X_ARGUMENTS_CANNOT_BE_REFERENCED_IN_PROPERTY_INITIALIZERS_OR_CLASS_STATIC_INITIALIZATION_BLOCKS, &[]);
                return self.error_type;
            }
            return self.get_type_of_symbol(symbol);
        }
        if should_mark_identifier_alias_referenced(node) {
            self.mark_linked_references(
                node,
                ReferenceHint::Identifier,
                None, /*propSymbol*/
                None, /*parentType*/
            );
        }
        let local_or_export_symbol =
            self.get_export_symbol_of_value_symbol_if_exported(Some(symbol)).unwrap();
        let target_symbol = self.resolve_alias_with_deprecation_check(local_or_export_symbol, node);
        let has_declarations = !target_symbol.declarations().is_empty();
        if has_declarations
            && self.is_deprecated_symbol(target_symbol)
            && self.is_uncalled_function_reference(node, target_symbol)
        {
            let declarations = target_symbol.declarations();
            self.add_deprecated_suggestion(node, declarations, node.text());
        }
        let mut declaration = local_or_export_symbol.value_declaration();
        let immediate_declaration = declaration;
        // If the identifier is declared in a binding pattern for which we're currently computing the implied type and the
        // reference occurs with the same binding pattern, return the non-inferrable any type. This for example occurs in
        // 'const [a, b = a + 1] = [2]' when we're computing the contextual type for the array literal '[2]'.
        if let Some(decl) = declaration
            && decl.kind() == Kind::BindingElement
            && decl.parent().is_some_and(|p| self.contextual_binding_patterns.contains(&p))
            && ast::find_ancestor(node, |parent| Some(parent) == decl.parent()).is_some()
        {
            return self.non_inferrable_any_type;
        }
        let mut t = self.get_narrowed_type_of_symbol(local_or_export_symbol, node);
        let assignment_kind = get_assignment_target_kind(node);
        if assignment_kind != AssignmentKind::None {
            let flags = local_or_export_symbol.flags();
            if !(flags.intersects(SymbolFlags::Variable)
                || ast::is_in_js_file(node) && flags.intersects(SymbolFlags::ValueModule))
            {
                let assignment_error: &'static Message = if flags.intersects(SymbolFlags::Enum) {
                    &diagnostics::CANNOT_ASSIGN_TO_0_BECAUSE_IT_IS_AN_ENUM
                } else if flags.intersects(SymbolFlags::Class) {
                    &diagnostics::CANNOT_ASSIGN_TO_0_BECAUSE_IT_IS_A_CLASS
                } else if flags.intersects(SymbolFlags::Module) {
                    &diagnostics::CANNOT_ASSIGN_TO_0_BECAUSE_IT_IS_A_NAMESPACE
                } else if flags.intersects(SymbolFlags::Function) {
                    &diagnostics::CANNOT_ASSIGN_TO_0_BECAUSE_IT_IS_A_FUNCTION
                } else if flags.intersects(SymbolFlags::Alias) {
                    &diagnostics::CANNOT_ASSIGN_TO_0_BECAUSE_IT_IS_AN_IMPORT
                } else {
                    &diagnostics::CANNOT_ASSIGN_TO_0_BECAUSE_IT_IS_NOT_A_VARIABLE
                };
                let symbol_string = self.symbol_to_string(symbol);
                self.error(Some(node), assignment_error, &[&symbol_string]);
                return self.error_type;
            }
            if self.is_readonly_symbol(local_or_export_symbol) {
                if local_or_export_symbol.flags().intersects(SymbolFlags::Variable) {
                    let symbol_string = self.symbol_to_string(symbol);
                    self.error(
                        Some(node),
                        &diagnostics::CANNOT_ASSIGN_TO_0_BECAUSE_IT_IS_A_CONSTANT,
                        &[&symbol_string],
                    );
                } else {
                    let symbol_string = self.symbol_to_string(symbol);
                    self.error(
                        Some(node),
                        &diagnostics::CANNOT_ASSIGN_TO_0_BECAUSE_IT_IS_A_READ_ONLY_PROPERTY,
                        &[&symbol_string],
                    );
                }
                return self.error_type;
            }
        }
        let is_alias = local_or_export_symbol.flags().intersects(SymbolFlags::Alias);
        // We only narrow variables and parameters occurring in a non-assignment position. For all other
        // entities we simply return the declared type.
        if local_or_export_symbol.flags().intersects(SymbolFlags::Variable) {
            if assignment_kind == AssignmentKind::Definite {
                if is_in_compound_like_assignment(node) {
                    return self.get_base_type_of_literal_type(t);
                }
                return t;
            }
        } else if is_alias {
            declaration = Self::get_declaration_of_alias_symbol(symbol);
        } else {
            return t;
        }
        let Some(declaration) = declaration else {
            return t;
        };
        t = self.get_narrowable_type_for_reference(t, node, check_mode);
        // The declaration container is the innermost function that encloses the declaration of the variable
        // or parameter. The flow container is the innermost function starting with which we analyze the control
        // flow graph to determine the control flow based type.
        let is_parameter = ast::get_root_declaration(declaration).kind() == Kind::Parameter;
        let declaration_container = Self::get_control_flow_container(declaration);
        let mut flow_container = Self::get_control_flow_container(node);
        let is_outer_variable = flow_container != declaration_container;
        let is_spread_destructuring_assignment_target = node.parent().is_some()
            && node.parent().unwrap().parent().is_some()
            && ast::is_spread_assignment(node.parent().unwrap())
            && Self::is_destructuring_assignment_target(node.parent().unwrap().parent().unwrap());
        let is_module_exports = symbol.flags().intersects(SymbolFlags::ModuleExports);
        let type_is_automatic = t == self.auto_type || t == self.auto_array_type;
        let is_automatic_type_in_non_null =
            type_is_automatic && node.parent().unwrap().kind() == Kind::NonNullExpression;
        // When the control flow originates in a function expression, arrow function, method, or accessor, and
        // we are referencing a closed-over const variable or parameter or mutable local variable past its last
        // assignment, we extend the origin of the control flow analysis to include the immediately enclosing
        // control flow container.
        while flow_container != declaration_container
            && (ast::is_function_expression_or_arrow_function(flow_container.unwrap())
                || ast::is_object_literal_or_class_expression_method_or_accessor(
                    flow_container.unwrap(),
                ))
            && (self.is_constant_variable(local_or_export_symbol) && t != self.auto_array_type
                || Self::is_parameter_or_mutable_local_variable(local_or_export_symbol)
                    && self.is_past_last_assignment(local_or_export_symbol, Some(node)))
        {
            flow_container = Self::get_control_flow_container(flow_container.unwrap());
        }
        // We only look for uninitialized variables in strict null checking mode, and only when we can analyze
        // the entire control flow graph from the variable's declaration (i.e. when the flow container and
        // declaration container are the same).
        let is_never_initialized = match immediate_declaration {
            Some(imm) => {
                ast::is_variable_declaration(imm)
                    && !ast::is_for_in_or_of_statement(imm.parent().unwrap().parent())
                    && imm.initializer().is_none()
                    && imm.as_variable_declaration().exclamation_token().is_none()
                    && Self::is_mutable_local_variable_declaration(imm)
                    && !self.is_symbol_assigned_definitely(symbol)
            }
            None => false,
        };
        let assume_initialized = is_parameter
            || is_alias
            || (is_outer_variable && !is_never_initialized)
            || is_spread_destructuring_assignment_target
            || is_module_exports
            || Self::is_same_scoped_binding_element(node, declaration)
            || t != self.auto_type
                && t != self.auto_array_type
                && (!self.strict_null_checks
                    || t.flags().intersects(TypeFlags::AnyOrUnknown | TypeFlags::Void)
                    || crate::is_in_type_query(node)
                    || Self::is_in_ambient_or_type_node(node)
                    || node.parent().unwrap().kind() == Kind::ExportSpecifier)
            || ast::is_non_null_expression(node.parent().unwrap())
            || ast::is_variable_declaration(declaration)
                && declaration.as_variable_declaration().exclamation_token().is_some()
            || declaration.flags().intersects(NodeFlags::Ambient);
        let initial_type = if is_automatic_type_in_non_null {
            self.undefined_type
        } else if assume_initialized && is_parameter {
            self.remove_optionality_from_declared_type(t, declaration)
        } else if assume_initialized {
            t
        } else if type_is_automatic {
            self.undefined_type
        } else {
            self.get_optional_type(t, false /*isProperty*/)
        };
        let flow_type = if is_automatic_type_in_non_null {
            let ft =
                self.get_flow_type_of_reference_ex(node, t, initial_type, flow_container, None);
            self.get_non_nullable_type(ft)
        } else {
            self.get_flow_type_of_reference_ex(node, t, initial_type, flow_container, None)
        };
        // A variable is considered uninitialized when it is possible to analyze the entire control flow graph
        // from declaration to use, and when the variable's declared type doesn't include undefined but the
        // control flow based type does include undefined.
        if !self.is_evolving_array_operation_target(node)
            && (t == self.auto_type || t == self.auto_array_type)
        {
            if flow_type == self.auto_type || flow_type == self.auto_array_type {
                if self.no_implicit_any {
                    let symbol_string = self.symbol_to_string(symbol);
                    let type_string = self.type_to_string_exported(flow_type);
                    self.error(ast::get_name_of_declaration(declaration), &diagnostics::VARIABLE_0_IMPLICITLY_HAS_TYPE_1_IN_SOME_LOCATIONS_WHERE_ITS_TYPE_CANNOT_BE_DETERMINED, &[&symbol_string, &type_string]);
                    let symbol_string = self.symbol_to_string(symbol);
                    let type_string = self.type_to_string_exported(flow_type);
                    self.error(
                        Some(node),
                        &diagnostics::VARIABLE_0_IMPLICITLY_HAS_AN_1_TYPE,
                        &[&symbol_string, &type_string],
                    );
                }
                return self.convert_auto_to_any(flow_type);
            }
        } else if !assume_initialized
            && !Self::contains_undefined_type(t)
            && Self::contains_undefined_type(flow_type)
        {
            let symbol_string = self.symbol_to_string(symbol);
            self.error(
                Some(node),
                &diagnostics::VARIABLE_0_IS_USED_BEFORE_BEING_ASSIGNED,
                &[&symbol_string],
            );
            // Return the declared type to reduce follow-on errors
            return t;
        }
        if assignment_kind != AssignmentKind::None {
            // Identifier is target of a compound assignment
            return self.get_base_type_of_literal_type(flow_type);
        }
        flow_type
    }

    // checker.go:11402
    fn is_same_scoped_binding_element(node: P<Node>, declaration: P<Node>) -> bool {
        if ast::is_binding_element(declaration) {
            let binding_element = ast::find_ancestor(node, ast::is_binding_element);
            return binding_element.is_some_and(|b| {
                ast::get_root_declaration(b) == ast::get_root_declaration(declaration)
            });
        }
        false
    }

    // Remove undefined from the annotated type of a parameter when there is an initializer (that doesn't include undefined)
    // checker.go:11411
    fn remove_optionality_from_declared_type(
        &mut self,
        declared_type: P<Type>,
        declaration: P<Node>,
    ) -> P<Type> {
        let remove_undefined = self.strict_null_checks
            && ast::is_parameter_declaration(declaration)
            && declaration.initializer().is_some()
            && self.has_type_facts(declared_type, TypeFacts::IsUndefined)
            && !self.parameter_initializer_contains_undefined(declaration);
        if remove_undefined {
            return self.get_type_with_facts(declared_type, TypeFacts::NEUndefined);
        }
        declared_type
    }

    // checker.go:11419
    fn parameter_initializer_contains_undefined(&mut self, declaration: P<Node>) -> bool {
        let links = self.node_links.get(declaration);
        if !links.flags.get().intersects(NodeCheckFlags::InitializerIsUndefinedComputed) {
            if !self.push_type_resolution(
                declaration.into(),
                TypeSystemPropertyName::InitializerIsUndefined,
            ) {
                self.report_circularity_error(declaration.symbol().unwrap());
                return true;
            }
            let initializer_type =
                self.check_declaration_initializer(declaration, CheckMode::Normal, None);
            let contains_undefined = self.has_type_facts(initializer_type, TypeFacts::IsUndefined);
            if !self.pop_type_resolution() {
                self.report_circularity_error(declaration.symbol().unwrap());
                return true;
            }
            if !links.flags.get().intersects(NodeCheckFlags::InitializerIsUndefinedComputed) {
                links.flags.set(
                    links.flags.get()
                        | NodeCheckFlags::InitializerIsUndefinedComputed
                        | if contains_undefined {
                            NodeCheckFlags::InitializerIsUndefined
                        } else {
                            NodeCheckFlags::empty()
                        },
                );
            }
        }
        links.flags.get().intersects(NodeCheckFlags::InitializerIsUndefined)
    }

    // checker.go:11438
    pub(crate) fn is_in_ambient_or_type_node(node: P<Node>) -> bool {
        node.flags().intersects(NodeFlags::Ambient)
            || ast::find_ancestor(node, |n| {
                ast::is_interface_declaration(n)
                    || ast::is_type_alias_declaration(n)
                    || ast::is_js_type_alias_declaration(n)
                    || ast::is_type_literal_node(n)
            })
            .is_some()
    }

    // checker.go:11444
    pub(crate) fn check_property_access_expression(
        &mut self,
        node: P<Node>,
        check_mode: CheckMode,
        write_only: bool,
    ) -> P<Type> {
        if node.flags().intersects(NodeFlags::OptionalChain) {
            return self.check_property_access_chain(node, check_mode);
        }
        let expr = node.expression().unwrap();
        let left_type = self.check_non_null_expression(expr);
        self.check_property_access_expression_or_qualified_name(
            node,
            expr,
            left_type,
            node.as_property_access_expression().name,
            check_mode,
            write_only,
        )
    }

    // checker.go:11452
    fn check_property_access_chain(&mut self, node: P<Node>, check_mode: CheckMode) -> P<Type> {
        let left_type = self.check_expression(node.expression().unwrap());
        let non_optional_type =
            self.get_optional_expression_type(left_type, node.expression().unwrap());
        let non_null_type = self.check_non_null_type(non_optional_type, node.expression().unwrap());
        let t = self.check_property_access_expression_or_qualified_name(
            node,
            node.expression().unwrap(),
            non_null_type,
            node.name().unwrap(),
            check_mode,
            false,
        );
        self.propagate_optional_type_marker(t, node, non_optional_type != left_type)
    }

    // checker.go:11458
    pub(crate) fn check_property_access_expression_or_qualified_name(
        &mut self,
        node: P<Node>,
        left: P<Node>,
        left_type: P<Type>,
        right: P<Node>,
        check_mode: CheckMode,
        write_only: bool,
    ) -> P<Type> {
        let parent_symbol = self.get_resolved_symbol_or_nil(left);
        let assignment_kind = get_assignment_target_kind(node);
        let mut widened_type = left_type;
        if assignment_kind != AssignmentKind::None || Self::is_method_access_for_call(node) {
            widened_type = self.get_widened_type(left_type);
        }
        let apparent_type = self.get_apparent_type(widened_type);
        let is_any_like =
            is_type_any(Some(apparent_type)) || apparent_type == self.silent_never_type;
        let mut prop: Option<P<Symbol>> = None;
        if ast::is_private_identifier(right) {
            if self.language_version
                < LANGUAGE_FEATURE_MINIMUM_TARGET.private_names_and_class_static_blocks
                || self.language_version
                    < LANGUAGE_FEATURE_MINIMUM_TARGET.class_and_class_element_decorators
                || !self.compiler_options.get_use_define_for_class_fields()
            {
                if assignment_kind != AssignmentKind::None {
                    self.check_external_emit_helpers(
                        node,
                        ExternalEmitHelpers::ClassPrivateFieldSet,
                    );
                }
                if assignment_kind != AssignmentKind::Definite {
                    self.check_external_emit_helpers(
                        node,
                        ExternalEmitHelpers::ClassPrivateFieldGet,
                    );
                }
            }
            let lexically_scoped_symbol =
                Self::lookup_symbol_for_private_identifier_declaration(right.text(), right);
            if assignment_kind != AssignmentKind::None
                && lexically_scoped_symbol
                    .is_some_and(|s| s.value_declaration().is_some_and(ast::is_method_declaration))
            {
                self.grammar_error_on_node(right, &diagnostics::CANNOT_ASSIGN_TO_PRIVATE_METHOD_0_PRIVATE_METHODS_ARE_NOT_WRITABLE, &[&right.text()]);
            }
            if is_any_like {
                if lexically_scoped_symbol.is_some() {
                    if self.is_error_type(apparent_type) {
                        return self.error_type;
                    }
                    return apparent_type;
                }
                if get_containing_class_excluding_class_decorators(right).is_none() {
                    self.grammar_error_on_node(
                        right,
                        &diagnostics::PRIVATE_IDENTIFIERS_ARE_NOT_ALLOWED_OUTSIDE_CLASS_BODIES,
                        &[],
                    );
                    return self.any_type;
                }
            }
            if let Some(lexically_scoped_symbol) = lexically_scoped_symbol {
                prop = self
                    .get_private_identifier_property_of_type(left_type, lexically_scoped_symbol);
            }
            match prop {
                None => {
                    // Check for private-identifier-specific shadowing and lexical-scoping errors.
                    if self.check_private_identifier_property_access(
                        left_type,
                        right,
                        lexically_scoped_symbol,
                    ) {
                        return self.error_type;
                    }
                    let containing_class = get_containing_class_excluding_class_decorators(right);
                    if let Some(containing_class) = containing_class
                        && ast::is_plain_js_file(
                            ast::get_source_file_of_node(containing_class),
                            self.compiler_options.check_js,
                        )
                    {
                        self.grammar_error_on_node(
                            right,
                            &diagnostics::PRIVATE_FIELD_0_MUST_BE_DECLARED_IN_AN_ENCLOSING_CLASS,
                            &[&right.text()],
                        );
                    }
                }
                Some(prop) => {
                    let is_setonly_accessor = prop.flags().intersects(SymbolFlags::SetAccessor)
                        && !prop.flags().intersects(SymbolFlags::GetAccessor);
                    if is_setonly_accessor && assignment_kind != AssignmentKind::Definite {
                        self.error(
                            Some(node),
                            &diagnostics::PRIVATE_ACCESSOR_WAS_DEFINED_WITHOUT_A_GETTER,
                            &[],
                        );
                    }
                }
            }
        } else {
            if is_any_like {
                if ast::is_identifier(left) && parent_symbol.is_some() {
                    self.mark_linked_references(
                        node,
                        ReferenceHint::Property,
                        None, /*propSymbol*/
                        Some(left_type),
                    );
                }
                if self.is_error_type(apparent_type) {
                    return self.error_type;
                }
                return apparent_type;
            }
            prop = self.get_property_of_type_ex(
                apparent_type,
                right.text(),
                is_const_enum_object_type(apparent_type), /*skipObjectFunctionPropertyAugment*/
                node.kind() == Kind::QualifiedName,       /*includeTypeOnlyMembers*/
            );
        }
        self.mark_linked_references(node, ReferenceHint::Property, prop, Some(left_type));

        let prop_type: P<Type> = match prop {
            None => {
                let mut index_info: Option<P<IndexInfo>> = None;
                if !ast::is_private_identifier(right)
                    && (assignment_kind == AssignmentKind::None
                        || !self.is_generic_object_type(left_type)
                        || is_this_type_parameter(left_type))
                {
                    index_info =
                        self.get_applicable_index_info_for_name(apparent_type, right.text());
                }
                let Some(index_info) = index_info else {
                    let is_unchecked_js = self.is_unchecked_js_suggestion(
                        Some(node),
                        left_type.symbol(),
                        true, /*excludeClasses*/
                    );
                    if !is_unchecked_js && self.is_js_literal_type(left_type) {
                        return self.any_type;
                    }
                    if left_type.symbol() == Some(self.global_this_symbol) {
                        let global_symbol =
                            self.global_this_symbol.exports().and_then(|e| e.lookup(right.text()));
                        if global_symbol
                            .is_some_and(|g| g.flags().intersects(SymbolFlags::BlockScoped))
                        {
                            let type_string = self.type_to_string_exported(left_type);
                            self.error(
                                Some(right),
                                &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1,
                                &[&right.text(), &type_string],
                            );
                        } else if self.no_implicit_any {
                            let type_string = self.type_to_string_exported(left_type);
                            self.error(Some(right), &diagnostics::ELEMENT_IMPLICITLY_HAS_AN_ANY_TYPE_BECAUSE_TYPE_0_HAS_NO_INDEX_SIGNATURE, &[&type_string]);
                        }
                        return self.any_type;
                    }
                    if !right.text().is_empty()
                        && !self.check_and_report_error_for_extending_interface(node)
                    {
                        self.add_deferred_diagnostic(move |c| {
                            // must be deferred because reporting this error can cause us to materialize the containing type completely (to print it), leading to erroneous circularity errors
                            c.report_nonexistent_property(
                                right,
                                if is_this_type_parameter(left_type) {
                                    apparent_type
                                } else {
                                    left_type
                                },
                                is_unchecked_js,
                            );
                        });
                    }
                    return self.error_type;
                };
                if index_info.is_readonly.get()
                    && (ast::is_assignment_target(node) || is_delete_target(node))
                {
                    let type_string = self.type_to_string_exported(apparent_type);
                    self.error(
                        Some(node),
                        &diagnostics::INDEX_SIGNATURE_IN_TYPE_0_ONLY_PERMITS_READING,
                        &[&type_string],
                    );
                }
                let mut pt = index_info.value_type.get().unwrap();
                if self.compiler_options.no_unchecked_indexed_access == Tristate::True
                    && get_assignment_target_kind(node) != AssignmentKind::Definite
                {
                    let missing_type = self.missing_type;
                    pt = self.get_union_type(&[pt, missing_type]);
                }
                if self.compiler_options.no_property_access_from_index_signature == Tristate::True
                    && ast::is_property_access_expression(node)
                {
                    self.error(Some(right), &diagnostics::PROPERTY_0_COMES_FROM_AN_INDEX_SIGNATURE_SO_IT_MUST_BE_ACCESSED_WITH_0, &[&right.text()]);
                }
                if let Some(declaration) = index_info.declaration.get()
                    && self.is_deprecated_declaration(declaration)
                {
                    self.add_deprecated_suggestion(right, &[declaration], right.text());
                }
                pt
            }
            Some(prop) => {
                let target_prop_symbol = self.resolve_alias_with_deprecation_check(prop, right);
                if self.is_deprecated_symbol(target_prop_symbol)
                    && self.is_uncalled_function_reference(node, target_prop_symbol)
                    && !target_prop_symbol.declarations().is_empty()
                {
                    let declarations = target_prop_symbol.declarations();
                    self.add_deprecated_suggestion(right, declarations, right.text());
                }
                self.check_property_not_used_before_declaration(prop, node, right);
                let is_self_type_access = self.is_self_type_access(left, parent_symbol);
                self.mark_property_as_referenced(prop, Some(node), is_self_type_access);
                self.symbol_node_links.get(node).resolved_symbol.set(Some(prop));
                self.check_property_accessibility(
                    node,
                    left.kind() == Kind::SuperKeyword,
                    ast::is_write_access(node),
                    apparent_type,
                    prop,
                );
                if self.is_assignment_to_readonly_entity(node, prop, assignment_kind) {
                    self.error(
                        Some(right),
                        &diagnostics::CANNOT_ASSIGN_TO_0_BECAUSE_IT_IS_A_READ_ONLY_PROPERTY,
                        &[&right.text()],
                    );
                    return self.error_type;
                }
                if self.is_this_property_access_in_constructor(node, prop) {
                    self.auto_type
                } else if write_only || ast::is_write_only_access(node) {
                    self.get_write_type_of_symbol(prop).unwrap()
                } else {
                    self.get_type_of_symbol(prop)
                }
            }
        };
        self.get_flow_type_of_access_expression(node, prop, prop_type, right, check_mode)
    }

    // checker.go:11592
    pub(crate) fn get_flow_type_of_access_expression(
        &mut self,
        node: P<Node>,
        prop: Option<P<Symbol>>,
        prop_type: P<Type>,
        error_node: P<Node>,
        check_mode: CheckMode,
    ) -> P<Type> {
        // Only compute control flow type if this is a property access expression that isn't an
        // assignment target, and the referenced property was declared as a variable, property,
        // accessor, or optional method.
        let assignment_kind = get_assignment_target_kind(node);
        if assignment_kind == AssignmentKind::Definite {
            return self.remove_missing_type(
                prop_type,
                prop.is_some_and(|p| p.flags().intersects(SymbolFlags::Optional)),
            );
        }
        if let Some(p) = prop
            && !(p
                .flags()
                .intersects(SymbolFlags::Variable | SymbolFlags::Property | SymbolFlags::Accessor)
                || p.flags().intersects(SymbolFlags::Method)
                    && prop_type.flags().intersects(TypeFlags::Union))
        {
            return prop_type;
        }
        if prop_type == self.auto_type {
            return self.get_flow_type_of_property(node, prop);
        }
        let prop_type = self.get_narrowable_type_for_reference(prop_type, node, check_mode);
        // If strict null checks and strict property initialization checks are enabled, if we have
        // a this.xxx property access, if the property is an instance property without an initializer,
        // and if we are in a constructor of the same class as the property declaration, assume that
        // the property is uninitialized at the top of the control flow.
        let mut assume_uninitialized = false;
        if self.strict_null_checks
            && let Some(p) = prop
            && let Some(declaration) = p.value_declaration()
        {
            if self.strict_property_initialization
                && ast::is_access_expression(node)
                && node.expression().unwrap().kind() == Kind::ThisKeyword
            {
                if Self::is_property_without_initializer(declaration)
                    && !ast::is_static(declaration)
                {
                    let flow_container = Self::get_control_flow_container(node);
                    if ast::is_constructor_declaration(flow_container.unwrap())
                        && flow_container.unwrap().parent() == declaration.parent()
                        && !declaration.flags().intersects(NodeFlags::Ambient)
                    {
                        assume_uninitialized = true;
                    }
                }
            } else if ast::is_binary_expression(declaration)
                && ast::is_property_access_expression(declaration.as_binary_expression().left)
                && Self::get_control_flow_container(node)
                    == Self::get_control_flow_container(declaration)
            {
                assume_uninitialized = true;
            }
        }
        let initial_type =
            self.add_optionality_ex(prop_type, false /*isProperty*/, assume_uninitialized);
        let flow_type =
            self.get_flow_type_of_reference_ex(node, prop_type, initial_type, None, None);
        if assume_uninitialized
            && !Self::contains_undefined_type(prop_type)
            && Self::contains_undefined_type(flow_type)
        {
            let symbol_string = self.symbol_to_string(prop.unwrap());
            self.error(
                Some(error_node),
                &diagnostics::PROPERTY_0_IS_USED_BEFORE_BEING_ASSIGNED,
                &[&symbol_string],
            );
            // Return the declared type to reduce follow-on errors
            return prop_type;
        }
        if assignment_kind != AssignmentKind::None {
            return self.get_base_type_of_literal_type(flow_type);
        }
        flow_type
    }

    // checker.go:11639
    pub(crate) fn get_control_flow_container(node: P<Node>) -> Option<P<Node>> {
        ast::find_ancestor(node.parent(), |node| {
            ast::is_function_like(node)
                && ast::get_immediately_invoked_function_expression(node).is_none()
                || ast::is_module_block(node)
                || ast::is_source_file(node)
                || ast::is_property_declaration(node)
        })
    }

    // checker.go:11645
    pub(crate) fn get_flow_type_of_property(
        &mut self,
        reference: P<Node>,
        prop: Option<P<Symbol>>,
    ) -> P<Type> {
        let mut initial_type = self.undefined_type;
        if let Some(p) = prop
            && let Some(value_declaration) = p.value_declaration()
            && (!self.is_auto_typed_property(p)
                || value_declaration.modifier_flags().intersects(ModifierFlags::Ambient))
            && let Some(base_type) = self.get_type_of_property_in_base_class(p)
        {
            initial_type = base_type;
        }
        let auto_type = self.auto_type;
        self.get_flow_type_of_reference_ex(reference, auto_type, initial_type, None, None)
    }

    // Return the inherited type of the given property or undefined if property doesn't exist in a base class.
    // checker.go:11656
    pub(crate) fn get_type_of_property_in_base_class(
        &mut self,
        property: P<Symbol>,
    ) -> Option<P<Type>> {
        let class_type = self.get_declaring_class(property);
        if let Some(class_type) = class_type {
            let base_class_types = self.get_base_types(class_type);
            if !base_class_types.is_empty() {
                return self.get_type_of_property_of_type(base_class_types[0], property.name());
            }
        }
        None
    }

    // checker.go:11667
    pub(crate) fn is_method_access_for_call(node: P<Node>) -> bool {
        let mut node = node;
        while ast::is_parenthesized_expression(node.parent().unwrap()) {
            node = node.parent().unwrap();
        }
        ast::is_call_or_new_expression(node.parent().unwrap())
            && node.parent().unwrap().expression() == Some(node)
    }

    // Lookup the private identifier lexically.
    // checker.go:11675
    pub(crate) fn lookup_symbol_for_private_identifier_declaration(
        prop_name: &str,
        location: P<Node>,
    ) -> Option<P<Symbol>> {
        let mut containing_class = get_containing_class_excluding_class_decorators(location);
        while let Some(cc) = containing_class {
            let symbol = cc.symbol().unwrap();
            let name = tsrs_binder::get_symbol_name_for_private_identifier(symbol, prop_name);
            let prop = symbol.members().and_then(|m| m.lookup(&name));
            if prop.is_some() {
                return prop;
            }
            let prop = symbol.exports().and_then(|e| e.lookup(&name));
            if prop.is_some() {
                return prop;
            }
            containing_class = ast::get_containing_class(cc);
        }
        None
    }

    // checker.go:11691
    pub(crate) fn get_private_identifier_property_of_type(
        &mut self,
        left_type: P<Type>,
        lexically_scoped_identifier: P<Symbol>,
    ) -> Option<P<Symbol>> {
        self.get_property_of_type(left_type, lexically_scoped_identifier.name())
    }

    // checker.go:11695
    fn check_private_identifier_property_access(
        &mut self,
        left_type: P<Type>,
        right: P<Node>,
        lexically_scoped_identifier: Option<P<Symbol>>,
    ) -> bool {
        // Either the identifier could not be looked up in the lexical scope OR the lexically scoped identifier did not exist on the type.
        // Find a private identifier with the same description on the type.
        let properties = self.get_properties_of_type(left_type);
        let mut property_on_type: Option<P<Symbol>> = None;
        for &symbol in properties {
            let decl = symbol.value_declaration();
            if let Some(decl) = decl
                && decl
                    .name()
                    .is_some_and(|n| ast::is_private_identifier(n) && n.text() == right.text())
            {
                property_on_type = Some(symbol);
                break;
            }
        }
        let diag_name = tsrs_scanner::declaration_name_to_string(Some(right));
        if let Some(property_on_type) = property_on_type {
            let type_value_decl = property_on_type.value_declaration().unwrap();
            let type_class = ast::get_containing_class(type_value_decl);
            // We found a private identifier property with the same description.
            // Either:
            // - There is a lexically scoped private identifier AND it shadows the one we found on the type.
            // - It is an attempt to access the private identifier outside of the class.
            if let Some(lexically_scoped_identifier) = lexically_scoped_identifier
                && let Some(lexical_value_decl) = lexically_scoped_identifier.value_declaration()
            {
                let lexical_class = ast::get_containing_class(lexical_value_decl);
                if ast::find_ancestor(lexical_class, |n| type_class == Some(n)).is_some() {
                    let type_string = self.type_to_string_exported(left_type);
                    let diagnostic = self.error(
                            Some(right),
                            &diagnostics::THE_PROPERTY_0_CANNOT_BE_ACCESSED_ON_TYPE_1_WITHIN_THIS_CLASS_BECAUSE_IT_IS_SHADOWED_BY_ANOTHER_PRIVATE_IDENTIFIER_WITH_THE_SAME_SPELLING,
                            &[&diag_name, &type_string],
                        );
                    diagnostic.add_related_info(create_diagnostic_for_node(
                        Some(lexical_value_decl),
                        &diagnostics::THE_SHADOWING_DECLARATION_OF_0_IS_DEFINED_HERE,
                        &[&diag_name],
                    ));
                    diagnostic.add_related_info(create_diagnostic_for_node(Some(type_value_decl), &diagnostics::THE_DECLARATION_OF_0_THAT_YOU_PROBABLY_INTENDED_TO_USE_IS_DEFINED_HERE, &[&diag_name]));
                    return true;
                }
            }
            let class_string =
                self.symbol_to_string_exported(type_class.unwrap().symbol().unwrap());
            self.error(Some(right), &diagnostics::PROPERTY_0_IS_NOT_ACCESSIBLE_OUTSIDE_CLASS_1_BECAUSE_IT_HAS_A_PRIVATE_IDENTIFIER, &[&diag_name, &class_string]);
            return true;
        }
        false
    }

    // checker.go:11731
    pub(crate) fn report_nonexistent_property(
        &mut self,
        prop_node: P<Node>,
        containing_type: P<Type>,
        is_unchecked_js: bool,
    ) {
        let key = NonExistentPropertyKey { prop_node, containing_type, is_unchecked_js };
        if self.non_existent_properties.has(&key) {
            return;
        }
        self.non_existent_properties.add(key);
        let links = self.node_links.get(prop_node);
        if links.flags.get().intersects(NodeCheckFlags::TypeChecked) {
            return; // error already made/in progress
        }
        links.flags.set(links.flags.get() | NodeCheckFlags::TypeChecked);
        if ast::is_jsdoc_name_reference_context(prop_node) {
            return;
        }
        let mut diagnostic: Option<P<Diagnostic>> = None;
        if !ast::is_private_identifier(prop_node)
            && containing_type.flags().intersects(TypeFlags::Union)
            && !containing_type.flags().intersects(TypeFlags::Primitive)
            && let Some(subtype) = containing_type.types().iter().find_map(|&subtype| {
                if self.get_property_of_type(subtype, prop_node.text()).is_none()
                    && self.get_applicable_index_info_for_name(subtype, prop_node.text()).is_none()
                {
                    Some(subtype)
                } else {
                    None
                }
            })
        {
            let name = tsrs_scanner::declaration_name_to_string(Some(prop_node));
            let type_string = self.type_to_string_exported(subtype);
            diagnostic = Some(new_diagnostic_chain_for_node(
                diagnostic,
                prop_node,
                Some(&diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1),
                &[&name, &type_string],
            ));
        }
        if self.type_has_static_property(prop_node.text(), containing_type) {
            let prop_name = tsrs_scanner::declaration_name_to_string(Some(prop_node));
            let type_name = self.type_to_string_exported(containing_type);
            let static_name = format!("{type_name}.{prop_name}");
            diagnostic = Some(new_diagnostic_chain_for_node(
                diagnostic,
                prop_node,
                Some(&diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1_DID_YOU_MEAN_TO_ACCESS_THE_STATIC_MEMBER_2_INSTEAD),
                &[&prop_name, &type_name, &static_name],
            ));
        } else {
            let promised_type = self.get_promised_type_of_promise(containing_type);
            if promised_type.is_some()
                && self.get_property_of_type(promised_type.unwrap(), prop_node.text()).is_some()
            {
                let name = tsrs_scanner::declaration_name_to_string(Some(prop_node));
                let type_string = self.type_to_string_exported(containing_type);
                let d = new_diagnostic_chain_for_node(
                    diagnostic,
                    prop_node,
                    Some(&diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1),
                    &[&name, &type_string],
                );
                d.add_related_info(new_diagnostic_for_node(
                    Some(prop_node),
                    Some(&diagnostics::DID_YOU_FORGET_TO_USE_AWAIT),
                    &[],
                ));
                diagnostic = Some(d);
            } else {
                let missing_property = tsrs_scanner::declaration_name_to_string(Some(prop_node));
                let container = self.type_to_string_exported(containing_type);
                let lib_suggestion = self.get_suggested_lib_for_non_existent_property(
                    &missing_property,
                    containing_type,
                );
                if lib_suggestion.is_empty() {
                    let suggestion = self
                        .get_suggested_symbol_for_nonexistent_property(prop_node, containing_type);
                    if let Some(suggestion) = suggestion {
                        let suggested_name = ast::symbol_name(suggestion);
                        let message: &'static Message = if is_unchecked_js {
                            &diagnostics::PROPERTY_0_MAY_NOT_EXIST_ON_TYPE_1_DID_YOU_MEAN_2
                        } else {
                            &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1_DID_YOU_MEAN_2
                        };
                        let d = new_diagnostic_chain_for_node(
                            diagnostic,
                            prop_node,
                            Some(message),
                            &[&missing_property, &container, &suggested_name],
                        );
                        if let Some(value_declaration) = suggestion.value_declaration() {
                            d.add_related_info(new_diagnostic_for_node(
                                Some(value_declaration),
                                Some(&diagnostics::X_0_IS_DECLARED_HERE),
                                &[&suggested_name],
                            ));
                        }
                        diagnostic = Some(d);
                    } else {
                        diagnostic = self.elaborate_never_intersection(
                            diagnostic,
                            prop_node,
                            containing_type,
                        );
                        let message: &'static Message = if self
                            .container_seems_to_be_empty_dom_element(containing_type)
                        {
                            &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1_TRY_CHANGING_THE_LIB_COMPILER_OPTION_TO_INCLUDE_DOM
                        } else {
                            &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1
                        };
                        diagnostic = Some(new_diagnostic_chain_for_node(
                            diagnostic,
                            prop_node,
                            Some(message),
                            &[&missing_property, &container],
                        ));
                    }
                } else {
                    diagnostic = Some(new_diagnostic_chain_for_node(
                        diagnostic,
                        prop_node,
                        Some(&diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1_DO_YOU_NEED_TO_CHANGE_YOUR_TARGET_LIBRARY_TRY_CHANGING_THE_LIB_COMPILER_OPTION_TO_2_OR_LATER),
                        &[&missing_property, &container, &lib_suggestion],
                    ));
                }
            }
        }
        let diagnostic = diagnostic.unwrap();
        self.add_error_or_suggestion(
            !is_unchecked_js
                || diagnostic.code()
                    != diagnostics::PROPERTY_0_MAY_NOT_EXIST_ON_TYPE_1_DID_YOU_MEAN_2.code(),
            diagnostic,
        );
    }

    // checker.go:11794
    fn get_suggested_lib_for_non_existent_property(
        &mut self,
        missing_property: &str,
        containing_type: P<Type>,
    ) -> String {
        let container = self.get_apparent_type(containing_type).symbol();
        if let Some(container) = container {
            let feature_map = get_feature_map();
            if let Some(type_features) = feature_map.get(container.name())
                && let Some(result) = type_features.iter().find_map(|entry| {
                    if entry.props.contains(&missing_property) {
                        Some(entry.lib.to_string())
                    } else {
                        None
                    }
                })
            {
                return result;
            }
        }
        String::new()
    }

    // checker.go:11809
    fn get_suggested_symbol_for_nonexistent_property(
        &mut self,
        name: P<Node>,
        containing_type: P<Type>,
    ) -> Option<P<Symbol>> {
        let mut props = self.get_properties_of_type(containing_type).to_vec();
        let parent = name.parent().unwrap();
        if ast::is_property_access_expression(parent) {
            props = tsrs_core::filter(&props, |prop| {
                self.is_valid_property_access_for_completions(parent, containing_type, *prop)
            })
            .into_owned();
        }
        self.get_spelling_suggestion_for_name(name.text(), &props, SymbolFlags::Value)
    }

    // Checks if an existing property access is valid for completions purposes.
    // @param node a property access-like node where we want to check if we can access a property.
    // This node does not need to be an access of the property we are checking.
    // e.g. in completions, this node will often be an incomplete property access node, as in `foo.`.
    // Besides providing a location (i.e. scope) used to check property accessibility, we use this node for
    // computing whether this is a `super` property access.
    // @param type the type whose property we are checking.
    // @param property the accessed property's symbol.
    // checker.go:11828
    fn is_valid_property_access_for_completions(
        &mut self,
        node: P<Node>,
        t: P<Type>,
        property: P<Symbol>,
    ) -> bool {
        self.is_property_accessible(
            node,
            ast::is_property_access_expression(node)
                && node.expression().unwrap().kind() == Kind::SuperKeyword,
            false, /*isWrite*/
            t,
            property,
        )
        // Previously we validated the 'this' type of methods but this adversely affected performance. See #31377 for more context.
    }

    // Checks if a property can be accessed in a location.
    // The location is given by the `node` parameter.
    // The node does not need to be a property access.
    // @param node location where to check property accessibility
    // @param isSuper whether to consider this a `super` property access, e.g. `super.foo`.
    // @param isWrite whether this is a write access, e.g. `++foo.x`.
    // @param containingType type where the property comes from.
    // @param property property symbol.
    // checker.go:11841
    pub(crate) fn is_property_accessible(
        &mut self,
        node: P<Node>,
        is_super: bool,
        is_write: bool,
        containing_type: P<Type>,
        property: P<Symbol>,
    ) -> bool {
        // Short-circuiting for improved performance.
        if is_type_any(Some(containing_type)) {
            return true;
        }
        // A #private property access in an optional chain is an error dealt with by the parser.
        // The checker does not check for it, so we need to do our own check here.
        if let Some(value_declaration) = property.value_declaration()
            && ast::is_private_identifier_class_element_declaration(value_declaration)
        {
            let decl_class = ast::get_containing_class(value_declaration);
            return !ast::is_optional_chain(node)
                && crate::is_node_descendant_of(Some(node), decl_class.unwrap());
        }
        self.check_property_accessibility_at_location(
            node,
            is_super,
            is_write,
            containing_type,
            property,
            None,
        )
    }

    // checker.go:11855
    fn container_seems_to_be_empty_dom_element(&mut self, containing_type: P<Type>) -> bool {
        !self
            .compiler_options
            .lib
            .as_ref()
            .is_some_and(|lib| lib.iter().any(|l| l == "lib.dom.d.ts"))
            && every_contained_type(self, containing_type, |_, t| has_common_dom_type_name(t))
            && self.is_empty_object_type(containing_type)
    }
}

// checker.go:11859
pub fn has_common_dom_type_name(t: P<Type>) -> bool {
    let Some(symbol) = t.symbol() else {
        return false;
    };
    let name = symbol.name();
    name == "EventTarget"
        || name == "Node"
        || name == "Element"
        || name.starts_with("HTML") && name.ends_with("Element")
}

impl Checker {
    // checker.go:11867
    pub(crate) fn check_and_report_error_for_extending_interface(
        &mut self,
        error_location: P<Node>,
    ) -> bool {
        let expression = Self::get_entity_name_for_extending_interface(error_location);
        if let Some(expression) = expression
            && self
                .resolve_entity_name(
                    expression,
                    SymbolFlags::Interface,
                    true, /*ignoreErrors*/
                    false,
                    None,
                )
                .is_some()
        {
            self.error(
                Some(error_location),
                &diagnostics::CANNOT_EXTEND_AN_INTERFACE_0_DID_YOU_MEAN_IMPLEMENTS,
                &[&tsrs_scanner::get_text_of_node(expression)],
            );
            return true;
        }
        false
    }

    /**
     * Climbs up parents to a heritage clause element and returns its entity name.
     */
    // checker.go:11879
    fn get_entity_name_for_extending_interface(node: P<Node>) -> Option<P<Node>> {
        match node.kind() {
            Kind::Identifier | Kind::QualifiedName | Kind::PropertyAccessExpression => {
                if let Some(parent) = node.parent() {
                    return Self::get_entity_name_for_extending_interface(parent);
                }
            }
            Kind::TypeReference => {
                return Some(node.as_type_reference_node().type_name);
            }
            Kind::ExpressionWithTypeArguments
                if ast::is_entity_name_expression(node.expression().unwrap()) =>
            {
                return node.expression();
            }
            _ => {}
        }
        None
    }

    // checker.go:11895
    pub(crate) fn is_uncalled_function_reference(
        &mut self,
        node: P<Node>,
        symbol: P<Symbol>,
    ) -> bool {
        if symbol.flags().intersects(SymbolFlags::Function | SymbolFlags::Method) {
            let mut parent = ast::find_ancestor(node.parent(), |n| !ast::is_access_expression(n));
            if parent.is_none() {
                parent = node.parent();
            }
            let parent = parent.unwrap();
            if ast::is_call_like_expression(parent) {
                return ast::is_call_or_new_expression(parent)
                    && ast::is_identifier(node)
                    && self.has_matching_argument(parent, node);
            }
            let declarations = symbol.declarations();
            return declarations
                .iter()
                .all(|&d| !ast::is_function_like(d) || self.is_deprecated_declaration(d));
        }
        true
    }

    // checker.go:11911
    fn check_property_not_used_before_declaration(
        &mut self,
        prop: P<Symbol>,
        node: P<Node>,
        right: P<Node>,
    ) {
        let value_declaration = prop.value_declaration();
        let Some(value_declaration) = value_declaration else {
            return;
        };
        if ast::get_source_file_of_node(node).unwrap().is_declaration_file() {
            return;
        }
        let mut diagnostic: Option<P<Diagnostic>> = None;
        let declaration_name = right.text();
        if !(!Self::is_in_property_initializer_or_class_static_block(
            node, false, /*ignoreArrowFunctions*/
        ) || Self::is_optional_property_declaration(value_declaration)
            || self.is_block_scoped_name_declared_before_use(value_declaration, right)
            || ast::is_access_expression(node)
                && ast::is_access_expression(node.expression().unwrap()))
            && !(ast::is_method_declaration(value_declaration)
                && self
                    .get_combined_modifier_flags_cached(value_declaration)
                    .intersects(ModifierFlags::Static))
            && (self.compiler_options.get_use_define_for_class_fields()
                || !self.is_property_declared_in_ancestor_class(prop))
        {
            diagnostic = Some(self.error(
                Some(right),
                &diagnostics::PROPERTY_0_IS_USED_BEFORE_ITS_INITIALIZATION,
                &[&declaration_name],
            ));
        } else if ast::is_class_declaration(value_declaration)
            && !ast::is_type_reference_node(node.parent().unwrap())
            && !value_declaration.flags().intersects(NodeFlags::Ambient)
            && !self.is_block_scoped_name_declared_before_use(value_declaration, right)
        {
            diagnostic = Some(self.error(
                Some(right),
                &diagnostics::CLASS_0_USED_BEFORE_ITS_DECLARATION,
                &[&declaration_name],
            ));
        }
        if let Some(diagnostic) = diagnostic {
            diagnostic.add_related_info(new_diagnostic_for_node(
                Some(value_declaration),
                Some(&diagnostics::X_0_IS_DECLARED_HERE),
                &[&declaration_name],
            ));
        }
    }

    // checker.go:11933
    fn is_optional_property_declaration(node: P<Node>) -> bool {
        ast::is_property_declaration(node)
            && !ast::has_accessor_modifier(node)
            && ast::is_question_token(node.postfix_token())
    }

    // checker.go:11937
    fn is_property_declared_in_ancestor_class(&mut self, prop: P<Symbol>) -> bool {
        let parent = prop.parent().unwrap();
        if parent.flags().intersects(SymbolFlags::Class) {
            let declared_type = self.get_declared_type_of_symbol(parent);
            let base_types = self.get_base_types(declared_type);
            if !base_types.is_empty() {
                let super_property = self.get_property_of_type(base_types[0], prop.name());
                return super_property.is_some_and(|s| s.value_declaration().is_some());
            }
        }
        false
    }

    /**
     * Check whether the requested property access is valid.
     * Returns true if node is a valid property access, and false otherwise.
     * @param node The node to be checked.
     * @param isSuper True if the access is from `super.`.
     * @param type The type of the object whose property is being accessed. (Not the type of the property.)
     * @param prop The symbol for the property being accessed.
     */
    // checker.go:11955
    pub(crate) fn check_property_accessibility(
        &mut self,
        node: P<Node>,
        is_super: bool,
        writing: bool,
        t: P<Type>,
        prop: P<Symbol>,
    ) -> bool {
        self.check_property_accessibility_ex(
            node, is_super, writing, t, prop, true, /*reportError*/
        )
    }

    // checker.go:11959
    fn check_property_accessibility_ex(
        &mut self,
        node: P<Node>,
        is_super: bool,
        writing: bool,
        t: P<Type>,
        prop: P<Symbol>,
        report_error: bool,
    ) -> bool {
        let mut error_node: Option<P<Node>> = None;
        if report_error {
            error_node = match node.kind() {
                Kind::PropertyAccessExpression => Some(node.as_property_access_expression().name),
                Kind::QualifiedName => Some(node.as_qualified_name().right),
                Kind::ImportType => Some(node),
                Kind::BindingElement => get_binding_element_property_name(node),
                _ => node.name(),
            };
        }
        self.check_property_accessibility_at_location(node, is_super, writing, t, prop, error_node)
    }

    /**
     * Check whether the requested property can be accessed at the requested location.
     * Returns true if node is a valid property access, and false otherwise.
     * @param location The location node where we want to check if the property is accessible.
     * @param isSuper True if the access is from `super.`.
     * @param writing True if this is a write property access, false if it is a read property access.
     * @param containingType The type of the object whose property is being accessed. (Not the type of the property.)
     * @param prop The symbol for the property being accessed.
     * @param errorNode The node where we should report an invalid property access error, or undefined if we should not report errors.
     */
    // checker.go:11988
    fn check_property_accessibility_at_location(
        &mut self,
        location: P<Node>,
        is_super: bool,
        writing: bool,
        containing_type: P<Type>,
        prop: P<Symbol>,
        error_node: Option<P<Node>>,
    ) -> bool {
        let flags = get_declaration_modifier_flags_from_symbol_ex(prop, writing);
        if is_super {
            // TS 1.0 spec (April 2014): 4.8.2
            // - In a constructor, instance member function, instance member accessor, or
            //   instance member variable initializer where this references a derived class instance,
            //   a super property access is permitted and must specify a public instance member function of the base class.
            // - In a static member function or static member accessor
            //   where this references the constructor function object of a derived class,
            //   a super property access is permitted and must specify a public static member function of the base class.
            if flags.intersects(ModifierFlags::Abstract) {
                // A method cannot be accessed in a super property access if the method is abstract.
                // This error could mask a private property access error. But, a member
                // cannot simultaneously be private and abstract, so this will trigger an
                // additional error elsewhere.
                if let Some(error_node) = error_node {
                    let prop_string = self.symbol_to_string(prop);
                    let declaring_class = self.get_declaring_class(prop).unwrap();
                    let class_string = self.type_to_string_exported(declaring_class);
                    self.error(Some(error_node), &diagnostics::ABSTRACT_METHOD_0_IN_CLASS_1_CANNOT_BE_ACCESSED_VIA_SUPER_EXPRESSION, &[&prop_string, &class_string]);
                }
                return false;
            }
            // A class field cannot be accessed via super.* from a derived class.
            // This is true for both [[Set]] (old) and [[Define]] (ES spec) semantics.
            if !flags.intersects(ModifierFlags::Static)
                && prop.declarations().iter().any(|&d| is_class_instance_property(d))
            {
                if let Some(error_node) = error_node {
                    let prop_string = self.symbol_to_string(prop);
                    self.error(Some(error_node), &diagnostics::CLASS_FIELD_0_DEFINED_BY_THE_PARENT_CLASS_IS_NOT_ACCESSIBLE_IN_THE_CHILD_CLASS_VIA_SUPER, &[&prop_string]);
                }
                return false;
            }
        }
        // Referencing abstract properties within their own constructors is not allowed
        if flags.intersects(ModifierFlags::Abstract)
            && self.symbol_has_non_method_declaration(prop)
            && (is_this_property(location)
                || is_this_initialized_object_binding_expression(Some(location))
                || ast::is_object_binding_pattern(location.parent().unwrap())
                    && is_this_initialized_declaration(location.parent().unwrap().parent()))
        {
            let parent_symbol = self.get_parent_of_symbol(prop);
            if let Some(parent_symbol) = parent_symbol
                && parent_symbol.flags().intersects(SymbolFlags::Class)
                && Self::is_node_used_during_class_initialization(location)
            {
                if let Some(error_node) = error_node {
                    let prop_string = self.symbol_to_string(prop);
                    let parent_string = self.symbol_to_string(parent_symbol);
                    self.error(Some(error_node), &diagnostics::ABSTRACT_PROPERTY_0_IN_CLASS_1_CANNOT_BE_ACCESSED_IN_THE_CONSTRUCTOR, &[&prop_string, &parent_string]);
                }
                return false;
            }
        }
        // Public properties are otherwise accessible.
        if !flags.intersects(ModifierFlags::NonPublicAccessibilityModifier) {
            return true;
        }
        // Property is known to be private or protected at this point
        // Private property is accessible if the property is within the declaring class
        if flags.intersects(ModifierFlags::Private) {
            let mut declaring_class_declaration: Option<P<Node>> = None;
            if let Some(parent) = self.get_parent_of_symbol(prop) {
                declaring_class_declaration = ast::get_class_like_declaration_of_symbol(parent);
            }
            if declaring_class_declaration.is_none()
                || !self.is_node_within_class(location, declaring_class_declaration.unwrap())
            {
                if let Some(error_node) = error_node {
                    let class = self.get_declaring_class(prop).unwrap_or(containing_type);
                    let prop_string = self.symbol_to_string(prop);
                    let class_string = self.type_to_string_exported(class);
                    self.error(
                        Some(error_node),
                        &diagnostics::PROPERTY_0_IS_PRIVATE_AND_ONLY_ACCESSIBLE_WITHIN_CLASS_1,
                        &[&prop_string, &class_string],
                    );
                }
                return false;
            }
            return true;
        }
        // Property is known to be protected at this point
        // All protected properties of a supertype are accessible in a super access
        if is_super {
            return true;
        }
        // Find the first enclosing class that has the declaring classes of the protected constituents
        // of the property as base classes
        let mut enclosing_class: Option<P<Type>> = None;
        let mut container = ast::get_containing_class(location);
        while let Some(cont) = container {
            let symbol = self.get_symbol_of_declaration(cont).unwrap();
            let class = self.get_declared_type_of_symbol(symbol);
            if self.is_class_derived_from_declaring_classes(class, prop, writing) {
                enclosing_class = Some(class);
                break;
            }
            container = ast::get_containing_class(cont);
        }
        // A protected property is accessible if the property is within the declaring class or classes derived from it
        if enclosing_class.is_none() {
            // allow PropertyAccessibility if context is in function with this parameter
            // static member access is disallowed
            let class = self.get_enclosing_class_from_this_parameter(location);
            if let Some(class) = class
                && self.is_class_derived_from_declaring_classes(class, prop, writing)
            {
                enclosing_class = Some(class);
            }
            if flags.intersects(ModifierFlags::Static) || enclosing_class.is_none() {
                if let Some(error_node) = error_node {
                    let class = self.get_declaring_class(prop).unwrap_or(containing_type);
                    let prop_string = self.symbol_to_string(prop);
                    let class_string = self.type_to_string_exported(class);
                    self.error(Some(error_node), &diagnostics::PROPERTY_0_IS_PROTECTED_AND_ONLY_ACCESSIBLE_WITHIN_CLASS_1_AND_ITS_SUBCLASSES, &[&prop_string, &class_string]);
                }
                return false;
            }
        }
        // No further restrictions for static properties
        if flags.intersects(ModifierFlags::Static) {
            return true;
        }
        let mut containing_type = Some(containing_type);
        let ct = containing_type.unwrap();
        if ct.flags().intersects(TypeFlags::TypeParameter) {
            // get the original type -- represented as the type constraint of the 'this' type
            if ct.as_type_parameter().is_this_type.get() {
                containing_type = self.get_constraint_of_type_parameter(ct);
            } else {
                containing_type = self.get_base_constraint_of_type(ct);
            }
        }
        let enclosing_class = enclosing_class.unwrap();
        if containing_type.is_none()
            || !self.has_base_type(containing_type.unwrap(), Some(enclosing_class))
        {
            if let (Some(error_node), Some(containing_type)) = (error_node, containing_type) {
                let prop_string = self.symbol_to_string(prop);
                let enclosing_string = self.type_to_string_exported(enclosing_class);
                let containing_string = self.type_to_string_exported(containing_type);
                self.error(
                    Some(error_node),
                    &diagnostics::PROPERTY_0_IS_PROTECTED_AND_ONLY_ACCESSIBLE_THROUGH_AN_INSTANCE_OF_CLASS_1_THIS_IS_AN_INSTANCE_OF_CLASS_2,
                    &[&prop_string, &enclosing_string, &containing_string],
                );
            }
            return false;
        }
        true
    }

    // checker.go:12103
    fn symbol_has_non_method_declaration(&mut self, symbol: P<Symbol>) -> bool {
        self.for_each_property(symbol, |_, prop| !prop.flags().intersects(SymbolFlags::Method))
    }

    // Invoke the callback for each underlying property symbol of the given symbol and return the first
    // value that isn't undefined.
    // checker.go:12109
    fn for_each_property(
        &mut self,
        prop: P<Symbol>,
        mut callback: impl FnMut(&mut Checker, P<Symbol>) -> bool,
    ) -> bool {
        self.for_each_property_worker(prop, &mut callback)
    }

    fn for_each_property_worker(
        &mut self,
        prop: P<Symbol>,
        callback: &mut dyn FnMut(&mut Checker, P<Symbol>) -> bool,
    ) -> bool {
        if !prop.check_flags().intersects(CheckFlags::Synthetic) {
            return callback(self, prop);
        }
        let types = self.value_symbol_links.get(prop).containing_type().unwrap().types();
        if types.iter().any(|&t| {
            let p = self.get_property_of_type(t, prop.name());
            if let Some(p) = p
                && self.for_each_property_worker(p, callback)
            {
                return true;
            }

            false
        }) {
            return true;
        }
        false
    }

    // Return the declaring class type of a property or undefined if property not declared in class
    // checker.go:12123
    pub(crate) fn get_declaring_class(&mut self, prop: P<Symbol>) -> Option<P<Type>> {
        if prop.parent().is_some_and(|p| p.flags().intersects(SymbolFlags::Class)) {
            let parent = self.get_parent_of_symbol(prop).unwrap();
            return Some(self.get_declared_type_of_symbol(parent));
        }
        None
    }

    // Return true if source property is a valid override of protected parts of target property.
    // checker.go:12131
    pub(crate) fn is_valid_override_of(
        &mut self,
        source_prop: P<Symbol>,
        target_prop: P<Symbol>,
    ) -> bool {
        !self.for_each_property(target_prop, |c, tp| {
            if get_declaration_modifier_flags_from_symbol(tp).intersects(ModifierFlags::Protected) {
                let declaring_class = c.get_declaring_class(tp);
                return !c.is_property_in_class_derived_from(source_prop, declaring_class);
            }
            false
        })
    }

    // Return true if some underlying source property is declared in a class that derives
    // from the given base class.
    // checker.go:12142
    fn is_property_in_class_derived_from(
        &mut self,
        prop: P<Symbol>,
        base_class: Option<P<Type>>,
    ) -> bool {
        self.for_each_property(prop, |c, sp| {
            let source_class = c.get_declaring_class(sp);
            if let Some(source_class) = source_class {
                return c.has_base_type(source_class, base_class);
            }
            false
        })
    }

    // checker.go:12152
    fn is_node_used_during_class_initialization(node: P<Node>) -> bool {
        ast::find_ancestor_or_quit(node, |element| {
            if ast::is_constructor_declaration(element) && ast::node_is_present(element.body())
                || ast::is_property_declaration(element)
            {
                FindAncestorResult::True
            } else if ast::is_class_like(element) || ast::is_function_like_declaration(element) {
                FindAncestorResult::Quit
            } else {
                FindAncestorResult::False
            }
        })
        .is_some()
    }

    // checker.go:12163
    pub(crate) fn is_node_within_class(
        &mut self,
        node: P<Node>,
        class_declaration: P<Node>,
    ) -> bool {
        self.for_each_enclosing_class(node, |_, n| n == class_declaration)
    }

    // checker.go:12167
    fn for_each_enclosing_class(
        &mut self,
        node: P<Node>,
        mut callback: impl FnMut(&mut Checker, P<Node>) -> bool,
    ) -> bool {
        let mut containing_class = ast::get_containing_class(node);
        while let Some(cc) = containing_class {
            let result = callback(self, cc);
            if result {
                return true;
            }
            containing_class = ast::get_containing_class(cc);
        }
        false
    }

    // Return true if the given class derives from each of the declaring classes of the protected
    // constituents of the given property.
    // checker.go:12181
    fn is_class_derived_from_declaring_classes(
        &mut self,
        check_class: P<Type>,
        prop: P<Symbol>,
        writing: bool,
    ) -> bool {
        !self.for_each_property(prop, |c, p| {
            if get_declaration_modifier_flags_from_symbol_ex(p, writing)
                .intersects(ModifierFlags::Protected)
            {
                let declaring_class = c.get_declaring_class(p);
                return !c.has_base_type(check_class, declaring_class);
            }
            false
        })
    }

    // checker.go:12190
    fn get_enclosing_class_from_this_parameter(&mut self, node: P<Node>) -> Option<P<Type>> {
        // 'this' type for a node comes from, in priority order...
        // 1. The type of a syntactic 'this' parameter in the enclosing function scope
        let this_parameter = get_this_parameter_from_node_context(node);
        let mut this_type: Option<P<Type>> = None;
        if let Some(this_parameter) = this_parameter
            && let Some(type_node) = this_parameter.type_node()
        {
            this_type = Some(self.get_type_from_type_node(type_node));
        }
        if let Some(tt) = this_type {
            // 2. The constraint of a type parameter used for an explicit 'this' parameter
            if tt.flags().intersects(TypeFlags::TypeParameter) {
                this_type = self.get_constraint_of_type_parameter(tt);
            }
        } else {
            // 3. The 'this' parameter of a contextual type
            let this_container = ast::get_this_container(
                node, false, /*includeArrowFunctions*/
                false, /*includeClassComputedPropertyName*/
            );
            if ast::is_function_like(this_container) {
                this_type = self.get_contextual_this_parameter_type(this_container);
            }
        }
        if let Some(tt) = this_type
            && tt.object_flags().intersects(ObjectFlags::ClassOrInterface | ObjectFlags::Reference)
        {
            return get_target_type(tt);
        }
        None
    }
}

// checker.go:12216
pub fn get_this_parameter_from_node_context(node: P<Node>) -> Option<P<Node>> {
    let this_container = ast::get_this_container(
        node, false, /*includeArrowFunctions*/
        false, /*includeClassComputedPropertyName*/
    );
    if ast::is_function_like(this_container) {
        return ast::get_this_parameter(this_container);
    }
    None
}

impl Checker {
    // checker.go:12224
    pub(crate) fn get_contextual_this_parameter_type(&mut self, fn_: P<Node>) -> Option<P<Type>> {
        if ast::is_arrow_function(fn_) {
            return None;
        }
        if self.is_context_sensitive_function_or_object_literal_method(fn_) {
            let contextual_signature = self.get_contextual_signature(fn_);
            if let Some(contextual_signature) = contextual_signature {
                let this_parameter = contextual_signature.this_parameter();
                if let Some(this_parameter) = this_parameter {
                    return Some(self.get_type_of_symbol(this_parameter));
                }
            }
        }
        let in_js = ast::is_in_js_file(fn_);
        if self.no_implicit_this || in_js {
            let containing_literal = get_containing_object_literal(fn_);
            if let Some(containing_literal) = containing_literal {
                // We have an object literal method. Check if the containing object literal has a contextual type
                // that includes a ThisType<T>. If so, T is the contextual type for 'this'. We continue looking in
                // any directly enclosing object literals.
                let contextual_type = self
                    .get_apparent_type_of_contextual_type(containing_literal, ContextFlags::None);
                let mut this_type = self.get_this_type_of_object_literal_from_contextual_type(
                    containing_literal,
                    contextual_type,
                );
                if let Some(tt) = this_type {
                    let inference_context = self.get_inference_context(containing_literal);
                    let mapper = Self::get_mapper_from_context(inference_context);
                    return Some(self.instantiate_type(tt, mapper));
                }
                // There was no contextual ThisType<T> for the containing object literal, so the contextual type
                // for 'this' is the non-null form of the contextual type for the containing object literal or
                // the type of the object literal itself.
                this_type = Some(if let Some(contextual_type) = contextual_type {
                    self.get_non_nullable_type(contextual_type)
                } else {
                    self.check_expression_cached(containing_literal)
                });
                return Some(self.get_widened_type(this_type.unwrap()));
            }
            // In an assignment of the form 'obj.xxx = function(...)' or 'obj[xxx] = function(...)', the
            // contextual type for 'this' is 'obj'.
            let parent = ast::walk_up_parenthesized_expressions(fn_.parent()).unwrap();
            if ast::is_assignment_expression(parent, false) {
                let target = parent.as_binary_expression().left;
                if ast::is_access_expression(target) {
                    let expression = target.expression().unwrap();
                    // Don't contextually type `this` as `exports` in `exports.Point = function(x, y) { this.x = x; this.y = y; }`
                    if in_js && ast::is_identifier(expression) {
                        let source_file = ast::get_source_file_of_node(parent).unwrap();
                        if source_file.common_js_module_indicator().is_some()
                            && self
                                .get_resolved_symbol(expression)
                                .flags()
                                .intersects(SymbolFlags::ModuleExports)
                        {
                            return None;
                        }
                    }
                    let t = self.check_expression_cached(expression);
                    return Some(self.get_widened_type(t));
                }
            }
        }
        None
    }

    // checker.go:12280
    pub(crate) fn check_this_expression(&mut self, node: P<Node>) -> P<Type> {
        // Stop at the first arrow function so that we can
        // tell whether 'this' needs to be captured.
        let mut container = ast::get_this_container(
            node, true, /*includeArrowFunctions*/
            true, /*includeClassComputedPropertyName*/
        );
        let mut captured_by_arrow_function = false;
        let mut this_in_computed_property_name = false;
        if ast::is_constructor_declaration(container) {
            self.check_this_before_super(node, container, &diagnostics::X_SUPER_MUST_BE_CALLED_BEFORE_ACCESSING_THIS_IN_THE_CONSTRUCTOR_OF_A_DERIVED_CLASS);
        }
        loop {
            // Now skip arrow functions to get the "real" owner of 'this'.
            if ast::is_arrow_function(container) {
                container = ast::get_this_container(
                    container,
                    false, /*includeArrowFunctions*/
                    !this_in_computed_property_name,
                );
                captured_by_arrow_function = true;
            }
            if ast::is_computed_property_name(container) {
                container = ast::get_this_container(
                    container,
                    !captured_by_arrow_function,
                    false, /*includeClassComputedPropertyName*/
                );
                this_in_computed_property_name = true;
                continue;
            }
            break;
        }
        self.check_this_in_static_class_field_initializer_in_decorated_class(node, container);
        if this_in_computed_property_name {
            self.error(
                Some(node),
                &diagnostics::X_THIS_CANNOT_BE_REFERENCED_IN_A_COMPUTED_PROPERTY_NAME,
                &[],
            );
        } else {
            match container.kind() {
                Kind::ModuleDeclaration => {
                    self.error(
                        Some(node),
                        &diagnostics::X_THIS_CANNOT_BE_REFERENCED_IN_A_MODULE_OR_NAMESPACE_BODY,
                        &[],
                    );
                    // do not return here so in case if lexical this is captured - it will be reflected in flags on NodeLinks
                }
                Kind::EnumDeclaration => {
                    self.error(
                        Some(node),
                        &diagnostics::X_THIS_CANNOT_BE_REFERENCED_IN_CURRENT_LOCATION,
                        &[],
                    );
                    // do not return here so in case if lexical this is captured - it will be reflected in flags on NodeLinks
                }
                _ => {}
            }
        }
        let t =
            self.try_get_this_type_at_ex(node, true /*includeGlobalThis*/, Some(container));
        if self.no_implicit_this {
            let global_this_type = self.get_type_of_symbol(self.global_this_symbol);
            if t == Some(global_this_type) && captured_by_arrow_function {
                self.error(
                    Some(node),
                    &diagnostics::THE_CONTAINING_ARROW_FUNCTION_CAPTURES_THE_GLOBAL_VALUE_OF_THIS,
                    &[],
                );
            } else if t.is_none() {
                // With noImplicitThis, functions may not reference 'this' if it has type 'any'
                let diag = self.error(Some(node), &diagnostics::X_THIS_IMPLICITLY_HAS_TYPE_ANY_BECAUSE_IT_DOES_NOT_HAVE_A_TYPE_ANNOTATION, &[]);
                if !ast::is_source_file(container) {
                    let outside_this = self.try_get_this_type_at(container);
                    if outside_this.is_some() && outside_this != Some(global_this_type) {
                        diag.add_related_info(create_diagnostic_for_node(
                            Some(container),
                            &diagnostics::AN_OUTER_VALUE_OF_THIS_IS_SHADOWED_BY_THIS_CONTAINER,
                            &[],
                        ));
                    }
                }
            }
        }
        match t {
            None => self.any_type,
            Some(t) => t,
        }
    }

    // checker.go:12337
    fn try_get_this_type_at(&mut self, node: P<Node>) -> Option<P<Type>> {
        self.try_get_this_type_at_ex(node, true /*includeGlobalThis*/, None /*container*/)
    }

    // checker.go:12341
    pub fn try_get_this_type_at_ex_exported(
        &mut self,
        node: P<Node>,
        include_global_this: bool,
        container: Option<P<Node>>,
    ) -> Option<P<Type>> {
        let reparsed = ast::get_reparsed_node_for_node(node).unwrap();
        if reparsed.flags().intersects(NodeFlags::JSDoc)
            && !reparsed.flags().intersects(NodeFlags::Reparsed)
        {
            return None; // Binder doesn't process non-reparsed JSDoc nodes
        }
        self.try_get_this_type_at_ex(
            reparsed,
            include_global_this,
            ast::get_reparsed_node_for_node(container),
        )
    }

    // checker.go:12349
    fn try_get_this_type_at_ex(
        &mut self,
        node: P<Node>,
        include_global_this: bool,
        container: Option<P<Node>>,
    ) -> Option<P<Type>> {
        let container = match container {
            Some(container) => container,
            None => Self::get_this_container(
                node, false, /*includeArrowFunctions*/
                false, /*includeClassComputedPropertyName*/
            )
            .unwrap(),
        };
        if ast::is_function_like(container)
            && (!Self::is_in_parameter_initializer_before_containing_function(node)
                || ast::get_this_parameter(container).is_some())
        {
            let sig = match self.get_signature_of_full_signature_type(container) {
                Some(sig) => sig,
                None => self.get_signature_from_declaration(container),
            };
            let mut this_type = self.get_this_type_of_signature(sig);
            // Note: a parameter initializer should refer to class-this unless function-this is explicitly annotated.
            // If this is a function in a JS file, it might be a class method.
            if this_type.is_none() {
                this_type = self.get_contextual_this_parameter_type(container);
            }
            if let Some(this_type) = this_type {
                return Some(self.get_flow_type_of_reference(node, this_type));
            }
        }
        if let Some(parent) = container.parent()
            && ast::is_class_like(parent)
        {
            let symbol = self.get_symbol_of_declaration(parent).unwrap();
            let t = if ast::is_static(container) {
                self.get_type_of_symbol(symbol)
            } else {
                self.get_declared_type_of_symbol(symbol)
                    .as_interface_type()
                    .this_type
                    .get()
                    .unwrap()
            };
            return Some(self.get_flow_type_of_reference(node, t));
        }
        if ast::is_source_file(container) {
            // look up in the source file's locals or exports
            if container.as_source_file().external_module_indicator().is_some() {
                // TODO: Maybe issue a better error than 'object is possibly undefined'
                return Some(self.undefined_type);
            }
            if include_global_this {
                return Some(self.get_type_of_symbol(self.global_this_symbol));
            }
        }
        None
    }

    // checker.go:12391
    pub(crate) fn get_this_container(
        node: P<Node>,
        include_arrow_functions: bool,
        include_class_computed_property_name: bool,
    ) -> Option<P<Node>> {
        let mut node = node;
        loop {
            let Some(parent) = node.parent() else {
                // If we never pass in a SourceFile, this should be unreachable, since we'll stop when we reach that.
                panic!("No parent in getThisContainer");
            };
            node = parent;
            match node.kind() {
                Kind::ComputedPropertyName => {
                    // If the grandparent node is an object literal (as opposed to a class),
                    // then the computed property is not a 'this' container.
                    // A computed property name in a class needs to be a this container
                    // so that we can error on it.
                    if include_class_computed_property_name
                        && ast::is_class_like(node.parent().unwrap().parent().unwrap())
                    {
                        return Some(node);
                    }
                    // If this is a computed property, then the parent should not
                    // make it a this container. The parent might be a property
                    // in an object literal, like a method or accessor. But in order for
                    // such a parent to be a this container, the reference must be in
                    // the *body* of the container.
                    node = node.parent().unwrap().parent().unwrap();
                }
                Kind::Decorator => {
                    // Decorators are always applied outside of the body of a class or method.
                    if node.parent().unwrap().kind() == Kind::Parameter
                        && ast::is_class_element(node.parent().unwrap().parent().unwrap())
                    {
                        // If the decorator's parent is a Parameter, we resolve the this container from
                        // the grandparent class declaration.
                        node = node.parent().unwrap().parent().unwrap();
                    } else if ast::is_class_element(node.parent().unwrap()) {
                        // If the decorator's parent is a class element, we resolve the 'this' container
                        // from the parent class declaration.
                        node = node.parent().unwrap();
                    }
                }
                Kind::ArrowFunction => {
                    if !include_arrow_functions {
                        continue;
                    }
                    return Some(node);
                }
                Kind::FunctionDeclaration
                | Kind::FunctionExpression
                | Kind::ModuleDeclaration
                | Kind::ClassStaticBlockDeclaration
                | Kind::PropertyDeclaration
                | Kind::PropertySignature
                | Kind::MethodDeclaration
                | Kind::MethodSignature
                | Kind::Constructor
                | Kind::GetAccessor
                | Kind::SetAccessor
                | Kind::CallSignature
                | Kind::ConstructSignature
                | Kind::IndexSignature
                | Kind::EnumDeclaration
                | Kind::SourceFile => {
                    return Some(node);
                }
                _ => {}
            }
        }
    }

    // checker.go:12438
    pub(crate) fn is_in_parameter_initializer_before_containing_function(node: P<Node>) -> bool {
        let mut node = node;
        let mut in_binding_initializer = false;
        while node.parent().is_some() && !ast::is_function_like(node.parent()) {
            let parent = node.parent().unwrap();
            if ast::is_parameter_declaration(parent)
                && (in_binding_initializer || parent.initializer() == Some(node))
            {
                return true;
            }

            if ast::is_binding_element(parent) && parent.initializer() == Some(node) {
                in_binding_initializer = true;
            }

            node = parent;
        }

        false
    }

    // checker.go:12457
    fn check_this_in_static_class_field_initializer_in_decorated_class(
        &mut self,
        this_expression: P<Node>,
        container: P<Node>,
    ) {
        if ast::is_property_declaration(container)
            && ast::has_static_modifier(container)
            && self.legacy_decorators
        {
            let initializer = container.initializer();
            if let Some(initializer) = initializer
                && initializer.loc().contains_inclusive(this_expression.pos())
                && ast::has_decorators(container.parent().unwrap())
            {
                self.error(Some(this_expression), &diagnostics::CANNOT_USE_THIS_IN_A_STATIC_PROPERTY_INITIALIZER_OF_A_DECORATED_CLASS, &[]);
            }
        }
    }

    // checker.go:12466
    pub(crate) fn check_this_before_super(
        &mut self,
        node: P<Node>,
        container: P<Node>,
        diagnostic_message: &'static Message,
    ) {
        let containing_class_decl = container.parent().unwrap();
        let base_type_node = ast::get_class_extends_heritage_element(containing_class_decl);
        // If a containing class does not have extends clause or the class extends null
        // skip checking whether super statement is called before "this" accessing.
        if base_type_node.is_some()
            && !self.class_declaration_extends_null(containing_class_decl)
            && node.has_flow_node_data()
            && !self.is_post_super_flow_node(node.flow_node().unwrap(), false /*noCacheCheck*/)
        {
            self.error(Some(node), diagnostic_message, &[]);
        }
    }

    /**
     * Check if the given class-declaration extends null then return true.
     * Otherwise, return false
     * @param classDecl a class declaration to check if it extends null
     */
    // checker.go:12483
    pub(crate) fn class_declaration_extends_null(&mut self, class_decl: P<Node>) -> bool {
        let class_symbol = self.get_symbol_of_declaration(class_decl).unwrap();
        let class_instance_type = self.get_declared_type_of_symbol(class_symbol);
        let base_constructor_type = self.get_base_constructor_type_of_class(class_instance_type);
        base_constructor_type == self.null_widening_type
    }

    // checker.go:12490
    pub(crate) fn check_assertion(&mut self, node: P<Node>, check_mode: CheckMode) -> P<Type> {
        if node.kind() == Kind::TypeAssertionExpression {
            let file = ast::get_source_file_of_node(node);
            if let Some(file) = file
                && tspath::file_extension_is_one_of(
                    file.file_name(),
                    &[tspath::EXTENSION_MTS, tspath::EXTENSION_CTS],
                )
            {
                self.grammar_error_on_node(node, &diagnostics::THIS_SYNTAX_IS_RESERVED_IN_FILES_WITH_THE_MTS_OR_CTS_EXTENSION_USE_AN_AS_EXPRESSION_INSTEAD, &[]);
            }
            if self.should_check_erasable_syntax(node) {
                let source_file = ast::get_source_file_of_node(node).unwrap();
                let loc = TextRange::new(
                    tsrs_scanner::skip_trivia(
                        source_file.text(),
                        node.pos().expect_source("parsed nodes have source positions"),
                    ),
                    node.expression()
                        .unwrap()
                        .pos()
                        .expect_source("parsed nodes have source positions"),
                );
                self.add_diagnostic(ast::new_diagnostic(
                    Some(source_file),
                    loc,
                    &diagnostics::THIS_SYNTAX_IS_NOT_ALLOWED_WHEN_ERASABLE_SYNTAX_ONLY_IS_ENABLED,
                    &[],
                ));
            }
        }
        let type_node = node.type_node().unwrap();
        let expr_type = self.check_expression_ex(node.expression().unwrap(), check_mode);
        // Always check the type node so its identifiers are resolved. resolveName knows not
        // to resolve (or report an error for) the `const` in a `const` assertion, so this is
        // safe even for `x as const` and keeps diagnostics stable regardless of traversal order.
        self.check_source_element(Some(type_node));
        if crate::is_const_type_reference(type_node) {
            if !self.is_valid_const_assertion_argument(node.expression().unwrap()) {
                self.error(node.expression(), &diagnostics::A_CONST_ASSERTION_CAN_ONLY_BE_APPLIED_TO_REFERENCES_TO_ENUM_MEMBERS_OR_STRING_NUMBER_BOOLEAN_ARRAY_OR_OBJECT_LITERALS, &[]);
            }
            return self.get_regular_type_of_literal_type(expr_type);
        }
        let links = self.assertion_links.get(node);
        links.expr_type.set(Some(expr_type));
        self.check_node_deferred(node);
        self.get_type_from_type_node(type_node)
    }

    // checker.go:12518
    pub(crate) fn check_assertion_deferred(&mut self, node: P<Node>) {
        let type_node = node.type_node().unwrap();
        let assertion_expr_type = self.assertion_links.get(node).expr_type.get().unwrap();
        let base_type = self.get_base_type_of_literal_type(assertion_expr_type);
        let expr_type = self.get_regular_type_of_object_literal(base_type);
        let target_type = self.get_type_from_type_node(type_node);
        if !self.is_error_type(target_type) {
            let widened_type = self.get_widened_type(expr_type);
            if !self.is_type_comparable_to(target_type, widened_type) {
                let mut err_node = node;
                if type_node.flags().intersects(NodeFlags::Reparsed) {
                    err_node = type_node;
                }
                self.check_type_comparable_to(
                    expr_type,
                    target_type,
                    err_node,
                    Some(&diagnostics::CONVERSION_OF_TYPE_0_TO_TYPE_1_MAY_BE_A_MISTAKE_BECAUSE_NEITHER_TYPE_SUFFICIENTLY_OVERLAPS_WITH_THE_OTHER_IF_THIS_WAS_INTENTIONAL_CONVERT_THE_EXPRESSION_TO_UNKNOWN_FIRST),
                );
            }
        }
    }

    // checker.go:12534
    pub(crate) fn check_binary_expression(
        &mut self,
        node: P<Node>,
        check_mode: CheckMode,
    ) -> P<Type> {
        let binary = node.as_binary_expression();
        self.check_binary_like_expression(
            binary.left,
            binary.operator_token,
            binary.right(),
            check_mode,
            Some(node),
        )
    }

    // checker.go:12539
    fn check_binary_like_expression(
        &mut self,
        left: P<Node>,
        operator_token: P<Node>,
        right: P<Node>,
        check_mode: CheckMode,
        error_node: Option<P<Node>>,
    ) -> P<Type> {
        let operator = operator_token.kind();
        if operator == Kind::EqualsToken
            && (left.kind() == Kind::ObjectLiteralExpression
                || left.kind() == Kind::ArrayLiteralExpression)
        {
            let right_type = self.check_expression_ex(right, check_mode);
            return self.check_destructuring_assignment(
                left,
                right_type,
                check_mode,
                right.kind() == Kind::ThisKeyword,
            );
        }
        let mut left_type = self.check_expression_ex(left, check_mode);
        let mut right_type = self.check_expression_ex(right, check_mode);
        if ast::is_logical_or_coalescing_binary_operator(operator) {
            let mut parent = left.parent().unwrap().parent().unwrap();
            while ast::is_parenthesized_expression(parent)
                || ast::is_logical_or_coalescing_binary_expression(parent)
            {
                parent = parent.parent().unwrap();
            }
            if operator == Kind::AmpersandAmpersandToken || ast::is_if_statement(parent) {
                let mut body: Option<P<Node>> = None;
                if ast::is_if_statement(parent) {
                    body = Some(parent.as_if_statement().then_statement);
                }
                self.check_testing_known_truthy_callable_or_awaitable_or_enum_member_type(
                    left, left_type, body,
                );
            }
            if ast::is_logical_binary_operator(operator) {
                self.check_truthiness_of_type(left_type, left);
            }
        }
        match operator {
            Kind::AsteriskToken
            | Kind::AsteriskAsteriskToken
            | Kind::AsteriskEqualsToken
            | Kind::AsteriskAsteriskEqualsToken
            | Kind::SlashToken
            | Kind::SlashEqualsToken
            | Kind::PercentToken
            | Kind::PercentEqualsToken
            | Kind::MinusToken
            | Kind::MinusEqualsToken
            | Kind::LessThanLessThanToken
            | Kind::LessThanLessThanEqualsToken
            | Kind::GreaterThanGreaterThanToken
            | Kind::GreaterThanGreaterThanEqualsToken
            | Kind::GreaterThanGreaterThanGreaterThanToken
            | Kind::GreaterThanGreaterThanGreaterThanEqualsToken
            | Kind::BarToken
            | Kind::BarEqualsToken
            | Kind::CaretToken
            | Kind::CaretEqualsToken
            | Kind::AmpersandToken
            | Kind::AmpersandEqualsToken => {
                if left_type == self.silent_never_type || right_type == self.silent_never_type {
                    return self.silent_never_type;
                }
                left_type = self.check_non_null_type(left_type, left);
                right_type = self.check_non_null_type(right_type, right);
                // if a user tries to apply a bitwise operator to 2 boolean operands
                // try and return them a helpful suggestion
                if left_type.flags().intersects(TypeFlags::BooleanLike)
                    && right_type.flags().intersects(TypeFlags::BooleanLike)
                {
                    let suggested_operator = Self::get_suggested_boolean_operator(operator);
                    if suggested_operator != Kind::Unknown {
                        self.error(
                            Some(operator_token),
                            &diagnostics::THE_0_OPERATOR_IS_NOT_ALLOWED_FOR_BOOLEAN_TYPES_CONSIDER_USING_1_INSTEAD,
                            &[&tsrs_scanner::token_to_string(operator_token.kind()), &tsrs_scanner::token_to_string(suggested_operator)],
                        );
                        return self.number_type;
                    }
                }
                // otherwise just check each operand separately and report errors as normal
                let left_ok = self.check_arithmetic_operand_type(left, left_type, &diagnostics::THE_LEFT_HAND_SIDE_OF_AN_ARITHMETIC_OPERATION_MUST_BE_OF_TYPE_ANY_NUMBER_BIGINT_OR_AN_ENUM_TYPE, true /*isAwaitValid*/);
                let right_ok = self.check_arithmetic_operand_type(right, right_type, &diagnostics::THE_RIGHT_HAND_SIDE_OF_AN_ARITHMETIC_OPERATION_MUST_BE_OF_TYPE_ANY_NUMBER_BIGINT_OR_AN_ENUM_TYPE, true /*isAwaitValid*/);

                // If both are any or unknown, allow operation; assume it will resolve to number
                let result_type: P<Type> = if self
                    .is_type_assignable_to_kind(left_type, TypeFlags::AnyOrUnknown)
                    && self.is_type_assignable_to_kind(right_type, TypeFlags::AnyOrUnknown)
                    || !self.maybe_type_of_kind(left_type, TypeFlags::BigIntLike)
                        && !self.maybe_type_of_kind(right_type, TypeFlags::BigIntLike)
                {
                    self.number_type
                } else if self.both_are_big_int_like(left_type, right_type) {
                    match operator {
                        Kind::GreaterThanGreaterThanGreaterThanToken
                        | Kind::GreaterThanGreaterThanGreaterThanEqualsToken => {
                            self.report_operator_error(
                                left_type, operator, right_type, error_node, None,
                            );
                        }
                        Kind::AsteriskAsteriskToken | Kind::AsteriskAsteriskEqualsToken
                            if self.language_version < ScriptTarget::ES2016 =>
                        {
                            self.error(error_node, &diagnostics::EXPONENTIATION_CANNOT_BE_PERFORMED_ON_BIGINT_VALUES_UNLESS_THE_TARGET_OPTION_IS_SET_TO_ES2016_OR_LATER, &[]);
                        }
                        _ => {}
                    }
                    self.bigint_type
                } else {
                    self.report_operator_error(
                        left_type,
                        operator,
                        right_type,
                        error_node,
                        Some(&mut |c: &mut Checker, l: P<Type>, r: P<Type>| {
                            c.both_are_big_int_like(l, r)
                        }),
                    );
                    self.error_type
                };
                if left_ok && right_ok {
                    self.check_assignment_operator(left, operator, right, left_type, result_type);
                    match operator {
                        Kind::LessThanLessThanToken
                        | Kind::LessThanLessThanEqualsToken
                        | Kind::GreaterThanGreaterThanToken
                        | Kind::GreaterThanGreaterThanEqualsToken
                        | Kind::GreaterThanGreaterThanGreaterThanToken
                        | Kind::GreaterThanGreaterThanGreaterThanEqualsToken => {
                            let rhs_eval = self.evaluate(right, right);
                            if let Some(LiteralValue::Number(num_value)) = rhs_eval.value
                                && num_value.abs() >= Number(32.0)
                            {
                                // Elevate from suggestion to error within an enum member
                                let is_error = ast::walk_up_parenthesized_expressions(
                                    right.parent().unwrap().parent(),
                                )
                                .is_some_and(ast::is_enum_member);
                                self.error_or_suggestion(
                                        is_error,
                                        error_node,
                                        &diagnostics::THIS_OPERATION_CAN_BE_SIMPLIFIED_THIS_SHIFT_IS_IDENTICAL_TO_0_1_2,
                                        &[&tsrs_scanner::get_text_of_node(left), &tsrs_scanner::token_to_string(operator), &num_value.remainder(Number(32.0))],
                                    );
                            }
                        }
                        _ => {}
                    }
                }
                result_type
            }
            Kind::PlusToken | Kind::PlusEqualsToken => {
                if left_type == self.silent_never_type || right_type == self.silent_never_type {
                    return self.silent_never_type;
                }
                if !self.is_type_assignable_to_kind(left_type, TypeFlags::StringLike)
                    && !self.is_type_assignable_to_kind(right_type, TypeFlags::StringLike)
                {
                    left_type = self.check_non_null_type(left_type, left);
                    right_type = self.check_non_null_type(right_type, right);
                }
                let mut result_type: Option<P<Type>> = None;
                if self.is_type_assignable_to_kind_ex(
                    left_type,
                    TypeFlags::NumberLike,
                    true, /*strict*/
                ) && self.is_type_assignable_to_kind_ex(
                    right_type,
                    TypeFlags::NumberLike,
                    true, /*strict*/
                ) {
                    // Operands of an enum type are treated as having the primitive type Number.
                    // If both operands are of the Number primitive type, the result is of the Number primitive type.
                    result_type = Some(self.number_type);
                } else if self.is_type_assignable_to_kind_ex(
                    left_type,
                    TypeFlags::BigIntLike,
                    true, /*strict*/
                ) && self.is_type_assignable_to_kind_ex(
                    right_type,
                    TypeFlags::BigIntLike,
                    true, /*strict*/
                ) {
                    // If both operands are of the BigInt primitive type, the result is of the BigInt primitive type.
                    result_type = Some(self.bigint_type);
                } else if self.is_type_assignable_to_kind_ex(
                    left_type,
                    TypeFlags::StringLike,
                    true, /*strict*/
                ) || self.is_type_assignable_to_kind_ex(
                    right_type,
                    TypeFlags::StringLike,
                    true, /*strict*/
                ) {
                    // If one or both operands are of the String primitive type, the result is of the String primitive type.
                    result_type = Some(self.string_type);
                } else if is_type_any(Some(left_type)) || is_type_any(Some(right_type)) {
                    // Otherwise, the result is of type Any.
                    // NOTE: unknown type here denotes error type. Old compiler treated this case as any type so do we.
                    if self.is_error_type(left_type) || self.is_error_type(right_type) {
                        result_type = Some(self.error_type);
                    } else {
                        result_type = Some(self.any_type);
                    }
                }
                // Symbols are not allowed at all in arithmetic expressions
                if let Some(rt) = result_type
                    && !self.check_for_disallowed_es_symbol_operand(
                        left, right, left_type, right_type, operator,
                    )
                {
                    return rt;
                }
                let Some(result_type) = result_type else {
                    // Types that have a reasonably good chance of being a valid operand type.
                    // If both types have an awaited type of one of these, we'll assume the user
                    // might be missing an await without doing an exhaustive check that inserting
                    // await(s) will actually be a completely valid binary expression.
                    let close_enough_kind = TypeFlags::NumberLike
                        | TypeFlags::BigIntLike
                        | TypeFlags::StringLike
                        | TypeFlags::AnyOrUnknown;
                    self.report_operator_error(
                        left_type,
                        operator,
                        right_type,
                        error_node,
                        Some(&mut |c: &mut Checker, left: P<Type>, right: P<Type>| {
                            c.is_type_assignable_to_kind(left, close_enough_kind)
                                && c.is_type_assignable_to_kind(right, close_enough_kind)
                        }),
                    );
                    return self.any_type;
                };
                if operator == Kind::PlusEqualsToken {
                    self.check_assignment_operator(left, operator, right, left_type, result_type);
                }
                result_type
            }
            Kind::LessThanToken
            | Kind::GreaterThanToken
            | Kind::LessThanEqualsToken
            | Kind::GreaterThanEqualsToken => {
                if self.check_for_disallowed_es_symbol_operand(
                    left, right, left_type, right_type, operator,
                ) {
                    let non_null_left = self.check_non_null_type(left_type, left);
                    left_type = self.get_base_type_of_literal_type_for_comparison(non_null_left);
                    let non_null_right = self.check_non_null_type(right_type, right);
                    right_type = self.get_base_type_of_literal_type_for_comparison(non_null_right);
                    self.report_operator_error_unless(
                        left_type,
                        operator,
                        right_type,
                        error_node,
                        |c, left, right| {
                            if is_type_any(Some(left)) || is_type_any(Some(right)) {
                                return true;
                            }
                            let number_or_big_int_type = c.number_or_big_int_type;
                            let left_assignable_to_number =
                                c.is_type_assignable_to(left, number_or_big_int_type);
                            let right_assignable_to_number =
                                c.is_type_assignable_to(right, number_or_big_int_type);
                            left_assignable_to_number && right_assignable_to_number
                                || !left_assignable_to_number
                                    && !right_assignable_to_number
                                    && c.are_types_comparable(left, right)
                        },
                    );
                }
                self.boolean_type
            }
            Kind::EqualsEqualsToken
            | Kind::ExclamationEqualsToken
            | Kind::EqualsEqualsEqualsToken
            | Kind::ExclamationEqualsEqualsToken => {
                // We suppress errors in CheckMode.TypeOnly (meaning the invocation came from getTypeOfExpression). During
                // control flow analysis it is possible for operands to temporarily have narrower types, and those narrower
                // types may cause the operands to not be comparable. We don't want such errors reported (see #46475).
                if !check_mode.intersects(CheckMode::TypeOnly) {
                    if (is_literal_expression_of_object(left) || is_literal_expression_of_object(right))
                        // only report for === and !== in JS, not == or !=
                        && (!ast::is_in_js_file(left) || (operator == Kind::EqualsEqualsEqualsToken || operator == Kind::ExclamationEqualsEqualsToken))
                    {
                        let eq_type = operator == Kind::EqualsEqualsToken
                            || operator == Kind::EqualsEqualsEqualsToken;
                        self.error(error_node, &diagnostics::THIS_CONDITION_WILL_ALWAYS_RETURN_0_SINCE_JAVA_SCRIPT_COMPARES_OBJECTS_BY_REFERENCE_NOT_VALUE, &[&if eq_type { "false" } else { "true" }]);
                    }
                    self.check_nan_equality(error_node, operator, left, right);
                    self.report_operator_error_unless(
                        left_type,
                        operator,
                        right_type,
                        error_node,
                        |c, left, right| {
                            c.is_type_equality_comparable_to(left, right)
                                || c.is_type_equality_comparable_to(right, left)
                        },
                    );
                }
                self.boolean_type
            }
            Kind::InstanceOfKeyword => {
                self.check_instance_of_expression(left, right, left_type, right_type, check_mode)
            }
            Kind::InKeyword => self.check_in_expression(left, right, left_type, right_type),
            Kind::AmpersandAmpersandToken | Kind::AmpersandAmpersandEqualsToken => {
                let mut result_type = left_type;
                if self.has_type_facts(left_type, TypeFacts::Truthy) {
                    let mut t = left_type;
                    if !self.strict_null_checks {
                        t = self.get_base_type_of_literal_type(right_type);
                    }
                    let falsy = self.extract_definitely_falsy_types(t);
                    result_type = self.get_union_type(&[falsy, right_type]);
                }
                if operator == Kind::AmpersandAmpersandEqualsToken {
                    self.check_assignment_operator(left, operator, right, left_type, right_type);
                }
                result_type
            }
            Kind::BarBarToken | Kind::BarBarEqualsToken => {
                let mut result_type = left_type;
                if self.has_type_facts(left_type, TypeFacts::Falsy) {
                    let removed = self.remove_definitely_falsy_types(left_type);
                    let non_nullable = self.get_non_nullable_type(removed);
                    result_type = self.get_union_type_ex(
                        &[non_nullable, right_type],
                        UnionReduction::Subtype,
                        AliasArg::None,
                        None,
                    );
                }
                if operator == Kind::BarBarEqualsToken {
                    self.check_assignment_operator(left, operator, right, left_type, right_type);
                }
                result_type
            }
            Kind::QuestionQuestionToken | Kind::QuestionQuestionEqualsToken => {
                if operator == Kind::QuestionQuestionToken {
                    self.check_nullish_coalesce_operands(left, right);
                }
                let mut result_type = left_type;
                if self.has_type_facts(left_type, TypeFacts::EQUndefinedOrNull) {
                    let non_nullable = self.get_non_nullable_type(left_type);
                    result_type = self.get_union_type_ex(
                        &[non_nullable, right_type],
                        UnionReduction::Subtype,
                        AliasArg::None,
                        None,
                    );
                }
                if operator == Kind::QuestionQuestionEqualsToken {
                    self.check_assignment_operator(left, operator, right, left_type, right_type);
                }
                result_type
            }
            Kind::EqualsToken => {
                self.check_assignment_operator(left, operator, right, left_type, right_type);
                right_type
            }
            Kind::CommaToken => {
                if !self.compiler_options.allow_unreachable_code.is_true()
                    && Self::is_side_effect_free(left)
                    && !Self::is_indirect_call(left.parent().unwrap())
                {
                    let sf = ast::get_source_file_of_node(left).unwrap();
                    let start = tsrs_scanner::skip_trivia(
                        sf.text(),
                        left.pos().expect_source("parsed nodes have source positions"),
                    );
                    let is_in_diag2657 = sf.diagnostics().iter().any(|d| {
                        if d.code()
                            != diagnostics::JSX_EXPRESSIONS_MUST_HAVE_ONE_PARENT_ELEMENT.code()
                        {
                            return false;
                        }
                        d.loc().is_some_and(|loc| loc.contains(start.into()))
                    });
                    if !is_in_diag2657 {
                        self.error(Some(left), &diagnostics::LEFT_SIDE_OF_COMMA_OPERATOR_IS_UNUSED_AND_HAS_NO_SIDE_EFFECTS, &[]);
                    }
                }
                right_type
            }
            _ => panic!("Unhandled case in checkBinaryLikeExpression"),
        }
    }

    // checker.go:12755
    pub(crate) fn check_destructuring_assignment(
        &mut self,
        node: P<Node>,
        source_type: P<Type>,
        check_mode: CheckMode,
        right_is_this: bool,
    ) -> P<Type> {
        let mut source_type = source_type;
        let mut target: P<Node>;
        if ast::is_shorthand_property_assignment(node) {
            let initializer =
                node.as_shorthand_property_assignment().object_assignment_initializer();
            if let Some(initializer) = initializer {
                // In strict null checking mode, if a default value of a non-undefined type is specified, remove
                // undefined from the final type.
                if self.strict_null_checks {
                    let initializer_type = self.check_expression(initializer);
                    if !self.has_type_facts(initializer_type, TypeFacts::IsUndefined) {
                        source_type = self.get_type_with_facts(source_type, TypeFacts::NEUndefined);
                    }
                }
                self.check_binary_like_expression(
                    node.name().unwrap(),
                    node.as_shorthand_property_assignment().equals_token().unwrap(),
                    initializer,
                    check_mode,
                    None,
                );
            }
            target = node.name().unwrap();
        } else {
            target = node;
        }
        if ast::is_binary_expression(target)
            && target.as_binary_expression().operator_token.kind() == Kind::EqualsToken
        {
            self.check_binary_expression(target, check_mode);
            target = target.as_binary_expression().left;
            // A default value is specified, so remove undefined from the final type.
            if self.strict_null_checks {
                source_type = self.get_type_with_facts(source_type, TypeFacts::NEUndefined);
            }
        }
        if ast::is_object_literal_expression(target) {
            return self.check_object_literal_assignment(target, source_type, right_is_this);
        }
        if ast::is_array_literal_expression(target) {
            return self.check_array_literal_assignment(target, source_type, check_mode);
        }
        self.check_reference_assignment(target, source_type, check_mode)
    }

    // checker.go:12788
    fn check_object_literal_assignment(
        &mut self,
        node: P<Node>,
        source_type: P<Type>,
        right_is_this: bool,
    ) -> P<Type> {
        let properties = node.property_list();
        if self.strict_null_checks && properties.nodes().is_empty() {
            return self.check_non_null_type(source_type, node);
        }
        for i in 0..properties.nodes().len() {
            self.check_object_literal_destructuring_property_assignment(
                node,
                source_type,
                i,
                Some(properties),
                right_is_this,
            );
        }
        source_type
    }

    // Note: If property cannot be a SpreadAssignment, then allProperties does not need to be provided
    // checker.go:12800
    pub(crate) fn check_object_literal_destructuring_property_assignment(
        &mut self,
        node: P<Node>,
        object_literal_type: P<Type>,
        property_index: usize,
        all_properties: Option<P<NodeList>>,
        right_is_this: bool,
    ) -> Option<P<Type>> {
        let properties = node.properties();
        let property = properties[property_index];
        if ast::is_property_assignment(property) || ast::is_shorthand_property_assignment(property)
        {
            let name = property.name();

            if let Some(name) = name
                && ast::is_private_identifier(name)
            {
                self.grammar_error_on_node(
                    name,
                    &diagnostics::PRIVATE_IDENTIFIERS_CANNOT_BE_USED_IN_DESTRUCTURING_PATTERNS,
                    &[],
                );
            }

            let expr_type = self.get_literal_type_from_property_name(name.unwrap());
            if is_type_usable_as_property_name(expr_type) {
                let text = get_property_name_from_type(expr_type);
                let prop = self.get_property_of_type(object_literal_type, &text);
                if let Some(prop) = prop {
                    self.mark_property_as_referenced(prop, Some(property), right_is_this);
                    self.check_property_accessibility(
                        property,
                        false, /*isSuper*/
                        true,  /*writing*/
                        object_literal_type,
                        prop,
                    );
                }
            }
            let access_flags = AccessFlags::ExpressionPosition
                | if Self::has_default_value(property) {
                    AccessFlags::AllowMissing
                } else {
                    AccessFlags::empty()
                };
            let element_type = self.get_indexed_access_type_ex(
                object_literal_type,
                expr_type,
                access_flags,
                name,
                AliasArg::None,
            );
            let t = self.get_flow_type_of_destructuring(property, element_type);
            let mut expr = property;
            if ast::is_property_assignment(property) {
                expr = property.initializer().unwrap();
            }
            return Some(self.check_destructuring_assignment(expr, t, CheckMode::Normal, false));
        }
        if ast::is_spread_assignment(property) {
            if property_index < properties.len() - 1 {
                self.error(
                    Some(property),
                    &diagnostics::A_REST_ELEMENT_MUST_BE_LAST_IN_A_DESTRUCTURING_PATTERN,
                    &[],
                );
                return None;
            }
            if self.language_version < LANGUAGE_FEATURE_MINIMUM_TARGET.object_spread_rest {
                self.check_external_emit_helpers(property, ExternalEmitHelpers::Rest);
            }
            let mut non_rest_names: Vec<P<Node>> = Vec::new();
            if let Some(all_properties) = all_properties {
                non_rest_names.extend(all_properties.nodes().iter().filter_map(
                    |&other_property| {
                        if ast::is_spread_assignment(other_property) {
                            None
                        } else {
                            Some(other_property.name().unwrap())
                        }
                    },
                ));
            }
            let t = self.get_rest_type(
                object_literal_type,
                &non_rest_names,
                object_literal_type.symbol(),
            );
            self.check_grammar_for_disallowed_trailing_comma(
                all_properties,
                &diagnostics::A_REST_PARAMETER_OR_BINDING_PATTERN_MAY_NOT_HAVE_A_TRAILING_COMMA,
            );
            return Some(self.check_destructuring_assignment(
                property.expression().unwrap(),
                t,
                CheckMode::Normal,
                false,
            ));
        }
        self.error(Some(property), &diagnostics::PROPERTY_ASSIGNMENT_EXPECTED, &[]);
        None
    }

    // checker.go:12851
    fn check_array_literal_assignment(
        &mut self,
        node: P<Node>,
        source_type: P<Type>,
        check_mode: CheckMode,
    ) -> P<Type> {
        let elements = node.elements();
        // This elementType will be used if the specific property corresponding to this index is not
        // present (aka the tuple element property). This call also checks that the parentType is in
        // fact an iterable or array (depending on target language).
        let undefined_type = self.undefined_type;
        let possibly_out_of_bounds_type = self.check_iterated_type_or_element_type(
            IterationUse::Destructuring | IterationUse::PossiblyOutOfBounds,
            source_type,
            undefined_type,
            Some(node),
        );
        let mut in_bounds_type =
            if self.compiler_options.no_unchecked_indexed_access == Tristate::True {
                None
            } else {
                Some(possibly_out_of_bounds_type)
            };
        for (i, element) in elements.iter().enumerate() {
            let mut t = possibly_out_of_bounds_type;
            if (*element).kind() == Kind::SpreadElement {
                if in_bounds_type.is_none() {
                    in_bounds_type = Some(self.check_iterated_type_or_element_type(
                        IterationUse::Destructuring,
                        source_type,
                        undefined_type,
                        Some(node),
                    ));
                }
                t = in_bounds_type.unwrap();
            }
            self.check_array_literal_destructuring_element_assignment(
                node,
                source_type,
                i,
                t,
                check_mode,
            );
        }
        source_type
    }

    // checker.go:12871
    pub(crate) fn check_array_literal_destructuring_element_assignment(
        &mut self,
        node: P<Node>,
        source_type: P<Type>,
        element_index: usize,
        element_type: P<Type>,
        check_mode: CheckMode,
    ) -> Option<P<Type>> {
        let elements = node.element_list();
        let element = elements.nodes()[element_index];
        if !ast::is_omitted_expression(element) {
            if !ast::is_spread_element(element) {
                let index_type = self.get_number_literal_type(Number(element_index as f64));
                if self.is_array_like_type(source_type) {
                    // We create a synthetic expression so that getIndexedAccessType doesn't get confused
                    // when the element is a SyntaxKind.ElementAccessExpression.
                    let access_flags = AccessFlags::ExpressionPosition
                        | if Self::has_default_value(element) {
                            AccessFlags::AllowMissing
                        } else {
                            AccessFlags::empty()
                        };
                    let synthetic =
                        self.create_synthetic_expression(element, index_type, false, None);
                    let element_type = self
                        .get_indexed_access_type_or_undefined(
                            source_type,
                            index_type,
                            access_flags,
                            Some(synthetic),
                            AliasArg::None,
                        )
                        .unwrap_or(self.error_type);
                    let mut assigned_type = element_type;
                    if Self::has_default_value(element) {
                        assigned_type =
                            self.get_type_with_facts(element_type, TypeFacts::NEUndefined);
                    }
                    let t = self.get_flow_type_of_destructuring(element, assigned_type);
                    return Some(
                        self.check_destructuring_assignment(element, t, check_mode, false),
                    );
                }
                return Some(self.check_destructuring_assignment(
                    element,
                    element_type,
                    check_mode,
                    false,
                ));
            }
            if element_index < elements.nodes().len() - 1 {
                self.error(
                    Some(element),
                    &diagnostics::A_REST_ELEMENT_MUST_BE_LAST_IN_A_DESTRUCTURING_PATTERN,
                    &[],
                );
            } else {
                let rest_expression = element.expression().unwrap();
                if ast::is_binary_expression(rest_expression)
                    && rest_expression.as_binary_expression().operator_token.kind()
                        == Kind::EqualsToken
                {
                    self.error(
                        Some(rest_expression.as_binary_expression().operator_token),
                        &diagnostics::A_REST_ELEMENT_CANNOT_HAVE_AN_INITIALIZER,
                        &[],
                    );
                } else {
                    self.check_grammar_for_disallowed_trailing_comma(Some(elements), &diagnostics::A_REST_PARAMETER_OR_BINDING_PATTERN_MAY_NOT_HAVE_A_TRAILING_COMMA);
                    let t = if every_type(self, source_type, |_, t| is_tuple_type(t)) {
                        self.map_type(source_type, |c, t| {
                            Some(c.slice_tuple_type(t, element_index, 0))
                        })
                        .unwrap()
                    } else {
                        self.create_array_type(element_type)
                    };
                    return Some(self.check_destructuring_assignment(
                        rest_expression,
                        t,
                        check_mode,
                        false,
                    ));
                }
            }
        }
        None
    }

    // checker.go:12912
    fn check_reference_assignment(
        &mut self,
        target: P<Node>,
        source_type: P<Type>,
        check_mode: CheckMode,
    ) -> P<Type> {
        let target_type = self.check_expression_ex(target, check_mode);
        let is_spread = ast::is_spread_assignment(target.parent().unwrap());
        let message: &'static Message = if is_spread {
            &diagnostics::THE_TARGET_OF_AN_OBJECT_REST_ASSIGNMENT_MUST_BE_A_VARIABLE_OR_A_PROPERTY_ACCESS
        } else {
            &diagnostics::THE_LEFT_HAND_SIDE_OF_AN_ASSIGNMENT_EXPRESSION_MUST_BE_A_VARIABLE_OR_A_PROPERTY_ACCESS
        };
        let optional_message: &'static Message = if is_spread {
            &diagnostics::THE_TARGET_OF_AN_OBJECT_REST_ASSIGNMENT_MAY_NOT_BE_AN_OPTIONAL_PROPERTY_ACCESS
        } else {
            &diagnostics::THE_LEFT_HAND_SIDE_OF_AN_ASSIGNMENT_EXPRESSION_MAY_NOT_BE_AN_OPTIONAL_PROPERTY_ACCESS
        };
        if self.check_reference_expression(target, message, optional_message) {
            self.check_type_assignable_to_and_optionally_elaborate(
                source_type,
                target_type,
                Some(target),
                Some(target),
                None,
                None,
            );
        }
        source_type
    }

    // checker.go:12926
    fn report_operator_error(
        &mut self,
        left_type: P<Type>,
        operator: Kind,
        right_type: P<Type>,
        error_node: Option<P<Node>>,
        is_related: Option<&mut dyn FnMut(&mut Checker, P<Type>, P<Type>) -> bool>,
    ) {
        let mut is_related = is_related;
        let mut would_work_with_await = false;
        if let Some(is_related) = is_related.as_deref_mut() {
            let awaited_left_type = self.get_awaited_type_no_alias(left_type);
            let awaited_right_type = self.get_awaited_type_no_alias(right_type);
            would_work_with_await = !(awaited_left_type == Some(left_type)
                && awaited_right_type == Some(right_type))
                && awaited_left_type.is_some()
                && awaited_right_type.is_some()
                && is_related(self, awaited_left_type.unwrap(), awaited_right_type.unwrap());
        }
        let mut effective_left = left_type;
        let mut effective_right = right_type;
        if !would_work_with_await && let Some(is_related) = is_related {
            (effective_left, effective_right) =
                self.get_base_types_if_unrelated(left_type, right_type, is_related);
        }
        let (left_str, right_str) =
            self.get_type_names_for_error_display(effective_left, effective_right);
        match operator {
            Kind::EqualsEqualsEqualsToken
            | Kind::EqualsEqualsToken
            | Kind::ExclamationEqualsEqualsToken
            | Kind::ExclamationEqualsToken => {
                self.error_and_maybe_suggest_await(error_node, would_work_with_await, &diagnostics::THIS_COMPARISON_APPEARS_TO_BE_UNINTENTIONAL_BECAUSE_THE_TYPES_0_AND_1_HAVE_NO_OVERLAP, &[&left_str, &right_str]);
            }
            _ => {
                self.error_and_maybe_suggest_await(
                    error_node,
                    would_work_with_await,
                    &diagnostics::OPERATOR_0_CANNOT_BE_APPLIED_TO_TYPES_1_AND_2,
                    &[&tsrs_scanner::token_to_string(operator), &left_str, &right_str],
                );
            }
        }
    }

    // checker.go:12947
    fn report_operator_error_unless(
        &mut self,
        left_type: P<Type>,
        operator: Kind,
        right_type: P<Type>,
        error_node: Option<P<Node>>,
        mut types_are_compatible: impl FnMut(&mut Checker, P<Type>, P<Type>) -> bool,
    ) {
        if !types_are_compatible(self, left_type, right_type) {
            self.report_operator_error(
                left_type,
                operator,
                right_type,
                error_node,
                Some(&mut types_are_compatible),
            );
        }
    }

    // checker.go:12953
    fn get_base_types_if_unrelated(
        &mut self,
        left_type: P<Type>,
        right_type: P<Type>,
        mut is_related: impl FnMut(&mut Checker, P<Type>, P<Type>) -> bool,
    ) -> (P<Type>, P<Type>) {
        let mut effective_left = left_type;
        let mut effective_right = right_type;
        let left_base = self.get_base_type_of_literal_type(left_type);
        let right_base = self.get_base_type_of_literal_type(right_type);
        if !is_related(self, left_base, right_base) {
            effective_left = left_base;
            effective_right = right_base;
        }
        (effective_left, effective_right)
    }

    // checker.go:12965
    fn check_assignment_operator(
        &mut self,
        left: P<Node>,
        operator: Kind,
        right: P<Node>,
        left_type: P<Type>,
        right_type: P<Type>,
    ) {
        let mut left_type = left_type;
        if ast::is_assignment_operator(operator) {
            // We ignore assignments of undefined to CommonJS exports when there are multiple assignment declarations
            let left_parent = left.parent().unwrap();
            if ast::is_declaration_node(left_parent)
                && ast::get_assignment_declaration_kind(left_parent)
                    == JSDeclarationKind::ExportsProperty
                && let Some(symbol) = self.symbol_node_links.get(left).resolved_symbol.get()
                && symbol.declarations().len() > 1
                && right_type.flags().intersects(TypeFlags::Undefined)
            {
                return;
            }
            // getters can be a subtype of setters, so to check for assignability we use the setter's type instead
            if ast::is_compound_assignment(operator) && ast::is_property_access_expression(left) {
                left_type = self.check_property_access_expression(
                    left,
                    CheckMode::Normal,
                    true, /*writeOnly*/
                );
            }
            if self.check_reference_expression(
                left,
                &diagnostics::THE_LEFT_HAND_SIDE_OF_AN_ASSIGNMENT_EXPRESSION_MUST_BE_A_VARIABLE_OR_A_PROPERTY_ACCESS,
                &diagnostics::THE_LEFT_HAND_SIDE_OF_AN_ASSIGNMENT_EXPRESSION_MAY_NOT_BE_AN_OPTIONAL_PROPERTY_ACCESS,
            ) {
                let mut head_message: Option<&'static Message> = None;
                if self.exact_optional_property_types && ast::is_property_access_expression(left) && self.maybe_type_of_kind(right_type, TypeFlags::Undefined) {
                    let expression_type = self.get_type_of_expression(left.expression().unwrap());
                    let target = self.get_type_of_property_of_type(expression_type, left.name().unwrap().text());
                    if self.is_exact_optional_property_mismatch(Some(right_type), target) {
                        head_message = Some(&diagnostics::TYPE_0_IS_NOT_ASSIGNABLE_TO_TYPE_1_WITH_EXACT_OPTIONAL_PROPERTY_TYPES_COLON_TRUE_CONSIDER_ADDING_UNDEFINED_TO_THE_TYPE_OF_THE_TARGET);
                    }
                }
                // to avoid cascading errors check assignability only if 'isReference' check succeeded and no errors were reported
                self.check_type_assignable_to_and_optionally_elaborate(right_type, left_type, Some(left), Some(right), head_message, None);
            }
        }
    }

    // checker.go:12991
    fn both_are_big_int_like(&mut self, left: P<Type>, right: P<Type>) -> bool {
        self.is_type_assignable_to_kind(left, TypeFlags::BigIntLike)
            && self.is_type_assignable_to_kind(right, TypeFlags::BigIntLike)
    }

    // checker.go:12995
    fn get_suggested_boolean_operator(operator: Kind) -> Kind {
        match operator {
            Kind::BarToken | Kind::BarEqualsToken => Kind::BarBarToken,
            Kind::CaretToken | Kind::CaretEqualsToken => Kind::ExclamationEqualsEqualsToken,
            Kind::AmpersandToken | Kind::AmpersandEqualsToken => Kind::AmpersandAmpersandToken,
            _ => Kind::Unknown,
        }
    }

    // checker.go:13007
    fn check_arithmetic_operand_type(
        &mut self,
        operand: P<Node>,
        t: P<Type>,
        diagnostic: &'static Message,
        is_await_valid: bool,
    ) -> bool {
        let number_or_big_int_type = self.number_or_big_int_type;
        if !self.is_type_assignable_to(t, number_or_big_int_type) {
            let mut awaited_type: Option<P<Type>> = None;
            if is_await_valid {
                awaited_type = self.get_awaited_type_of_promise(t);
            }
            let maybe_missing_await = awaited_type.is_some()
                && self.is_type_assignable_to(awaited_type.unwrap(), number_or_big_int_type);
            self.error_and_maybe_suggest_await(Some(operand), maybe_missing_await, diagnostic, &[]);
            return false;
        }
        true
    }

    // Return true if there was no error, false if there was an error.
    // checker.go:13020
    fn check_for_disallowed_es_symbol_operand(
        &mut self,
        left: P<Node>,
        right: P<Node>,
        left_type: P<Type>,
        right_type: P<Type>,
        operator: Kind,
    ) -> bool {
        let offending_symbol_operand = if self
            .maybe_type_of_kind_considering_base_constraint(left_type, TypeFlags::ESSymbolLike)
        {
            Some(left)
        } else if self
            .maybe_type_of_kind_considering_base_constraint(right_type, TypeFlags::ESSymbolLike)
        {
            Some(right)
        } else {
            None
        };
        if offending_symbol_operand.is_some() {
            self.error(
                offending_symbol_operand,
                &diagnostics::THE_0_OPERATOR_CANNOT_BE_APPLIED_TO_TYPE_SYMBOL,
                &[&tsrs_scanner::token_to_string(operator)],
            );
            return false;
        }
        true
    }

    // checker.go:13035
    fn check_nan_equality(
        &mut self,
        error_node: Option<P<Node>>,
        operator: Kind,
        left: P<Node>,
        right: P<Node>,
    ) {
        let is_left_nan = self.is_global_nan(ast::skip_parentheses(left));
        let is_right_nan = self.is_global_nan(ast::skip_parentheses(right));
        if is_left_nan || is_right_nan {
            let keyword = if operator == Kind::EqualsEqualsEqualsToken
                || operator == Kind::EqualsEqualsToken
            {
                Kind::FalseKeyword
            } else {
                Kind::TrueKeyword
            };
            let err = self.error(
                error_node,
                &diagnostics::THIS_CONDITION_WILL_ALWAYS_RETURN_0,
                &[&tsrs_scanner::token_to_string(keyword)],
            );
            if is_left_nan && is_right_nan {
                return;
            }
            let mut operator_string = "";
            if operator == Kind::ExclamationEqualsEqualsToken
                || operator == Kind::ExclamationEqualsToken
            {
                operator_string = tsrs_scanner::token_to_string(Kind::ExclamationToken);
            }
            let location = if is_left_nan { right } else { left };
            let expression = ast::skip_parentheses(location);
            let mut entity_name = "...".to_string();
            if ast::is_entity_name_expression(expression) {
                entity_name = crate::entity_name_to_string(expression);
            }
            let suggestion = format!("{operator_string}Number.isNaN({entity_name})");
            err.add_related_info(create_diagnostic_for_node(
                Some(location),
                &diagnostics::DID_YOU_MEAN_0,
                &[&suggestion],
            ));
        }
    }

    // checker.go:13061
    fn is_global_nan(&mut self, expr: P<Node>) -> bool {
        if ast::is_identifier(expr) && expr.text() == "NaN" {
            let global_nan_symbol = self.get_global_nan_symbol_or_nil();
            return global_nan_symbol.is_some()
                && global_nan_symbol == Some(self.get_resolved_symbol(expr));
        }
        false
    }
}
