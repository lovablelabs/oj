use crate::*;
use rustc_hash::FxHashMap;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

// Non-function declarations in checker.go:6527-8748 (hand-ported by checker-foundation):
//   type UnusedKind (checker.go:7249)
//   const (checker.go:7251): UnusedKindLocal, UnusedKindParameter

impl Checker {
    // checker.go:6527
    pub(crate) fn get_iteration_types_of_iterable_fast(
        &mut self,
        t: P<Type>,
        r: P<IterationTypesResolver>,
    ) -> IterationTypes {
        // As an optimization, if the type is an instantiation of the following global type, then
        // just grab its related type arguments:
        // - `Iterable<T, TReturn, TNext>` or `AsyncIterable<T, TReturn, TNext>`
        // - `IteratorObject<T, TReturn, TNext>` or `AsyncIteratorObject<T, TReturn, TNext>`
        // - `IterableIterator<T, TReturn, TNext>` or `AsyncIterableIterator<T, TReturn, TNext>`
        // - `Generator<T, TReturn, TNext>` or `AsyncGenerator<T, TReturn, TNext>`
        if {
            let g = r.get_global_iterable_type(self);
            Self::is_reference_to_type(Some(t), g)
        } || {
            let g = r.get_global_iterator_object_type(self);
            Self::is_reference_to_type(Some(t), g)
        } || {
            let g = r.get_global_iterable_iterator_type(self);
            Self::is_reference_to_type(Some(t), g)
        } || {
            let g = r.get_global_generator_type(self);
            Self::is_reference_to_type(Some(t), g)
        } {
            let type_arguments = self.get_type_arguments(t);
            return r.get_resolved_iteration_types(
                self,
                type_arguments[0],
                type_arguments[1],
                type_arguments[2],
            );
        }
        // As an optimization, if the type is an instantiation of one of the following global types, then
        // just grab the related type argument:
        // - `ArrayIterator<T>`
        // - `MapIterator<T>`
        // - `SetIterator<T>`
        // - `StringIterator<T>`
        // - `ReadableStreamAsyncIterator<T>`
        let builtin = r.get_global_builtin_iterator_types(self);
        if Self::is_reference_to_some_type(Some(t), builtin) {
            let yield_type = self.get_type_arguments(t)[0];
            let return_type = self.get_builtin_iterator_return_type();
            let unknown_type = self.unknown_type;
            return r.get_resolved_iteration_types(self, yield_type, return_type, unknown_type);
        }
        IterationTypes::default()
    }
}

impl IterationTypesResolver {
    // checker.go:6554
    fn get_resolved_iteration_types(
        &self,
        c: &mut Checker,
        yield_type: P<Type>,
        return_type: P<Type>,
        next_type: P<Type>,
    ) -> IterationTypes {
        IterationTypes {
            yield_type: Some(
                self.resolve_iteration_type(c, yield_type, None /*errorNode*/)
                    .unwrap_or(yield_type),
            ),
            return_type: Some(
                self.resolve_iteration_type(c, return_type, None /*errorNode*/)
                    .unwrap_or(return_type),
            ),
            next_type: Some(next_type),
        }
    }
}

impl Checker {
    // checker.go:6562
    pub(crate) fn is_reference_to_type(t: Option<P<Type>>, target: P<Type>) -> bool {
        match t {
            Some(t) => {
                t.object_flags().intersects(ObjectFlags::Reference) && t.target() == Some(target)
            }
            None => false,
        }
    }

    // checker.go:6566
    fn is_reference_to_some_type(t: Option<P<Type>>, targets: &[P<Type>]) -> bool {
        match t {
            Some(t) => {
                t.object_flags().intersects(ObjectFlags::Reference)
                    && t.target().is_some_and(|target| targets.contains(&target))
            }
            None => false,
        }
    }

    // checker.go:6570
    pub(crate) fn get_builtin_iterator_return_type(&self) -> P<Type> {
        if self.strict_builtin_iterator_return { self.undefined_type } else { self.any_type }
    }
}

impl IterationTypes {
    // checker.go:6574
    pub(crate) fn has_types(&self) -> bool {
        self.yield_type.is_some() || self.return_type.is_some() || self.next_type.is_some()
    }

    // checker.go:6578
    pub(crate) fn get_type(&self, type_kind: IterationTypeKind) -> Option<P<Type>> {
        match type_kind {
            IterationTypeKind::Yield => self.yield_type,
            IterationTypeKind::Return => self.return_type,
            IterationTypeKind::Next => self.next_type,
        }
    }
}

impl Checker {
    // checker.go:6590
    pub(crate) fn combine_iteration_types(
        &mut self,
        iteration_types: &[IterationTypes],
    ) -> IterationTypes {
        IterationTypes {
            yield_type: self.get_iteration_type_union(iteration_types, |_, t| t.yield_type),
            return_type: self.get_iteration_type_union(iteration_types, |_, t| t.return_type),
            next_type: self.get_iteration_type_union(iteration_types, |_, t| t.next_type),
        }
    }

    // checker.go:6598
    fn get_iteration_type_union(
        &mut self,
        iteration_types: &[IterationTypes],
        mut f: impl FnMut(&mut Checker, IterationTypes) -> Option<P<Type>>,
    ) -> Option<P<Type>> {
        let mut types: Vec<P<Type>> = Vec::new();
        types.extend(iteration_types.iter().filter_map(|&it| f(self, it)));
        if types.is_empty() {
            return None;
        }
        Some(self.get_union_type(&types))
    }

    // checker.go:6606
    pub(crate) fn get_async_from_sync_iteration_types(
        &mut self,
        iteration_types: IterationTypes,
        error_node: Option<P<Node>>,
    ) -> IterationTypes {
        if !iteration_types.has_types()
            || iteration_types.yield_type == Some(self.any_type)
                && iteration_types.return_type == Some(self.any_type)
                && iteration_types.next_type == Some(self.any_type)
        {
            return iteration_types;
        }
        // if we're requesting diagnostics, report errors for a missing `Awaited<T>`.
        if error_node.is_some() {
            self.get_global_awaited_symbol();
        }
        let yield_type = self
            .get_awaited_type_ex(iteration_types.yield_type.unwrap(), error_node, None, &[])
            .unwrap_or(self.any_type);
        let return_type = self
            .get_awaited_type_ex(iteration_types.return_type.unwrap(), error_node, None, &[])
            .unwrap_or(self.any_type);
        IterationTypes {
            yield_type: Some(yield_type),
            return_type: Some(return_type),
            next_type: iteration_types.next_type,
        }
    }

    // Gets the *yield*, *return*, and *next* types of an `Iterable`-like or `AsyncIterable`-like
    // type from its members.
    //
    // If we successfully found the *yield*, *return*, and *next* types, an `IterationTypes` with non-nil
    // members is returned. Otherwise, a default `IterationTypes{}` is returned.
    //
    // NOTE: You probably don't want to call this directly and should be calling
    // `getIterationTypesOfIterable` instead.
    // checker.go:6630
    pub(crate) fn get_iteration_types_of_iterable_slow(
        &mut self,
        t: P<Type>,
        r: P<IterationTypesResolver>,
        error_node: Option<P<Node>>,
        diagnostic_output: &mut Vec<P<Diagnostic>>,
    ) -> IterationTypes {
        let name = self.get_property_name_for_known_symbol_name(r.iterator_symbol_name);
        if let Some(method) = self.get_property_of_type(t, &name)
            && !method.flags().intersects(SymbolFlags::Optional)
        {
            let method_type = self.get_type_of_symbol(method);
            if is_type_any(Some(method_type)) {
                return IterationTypes {
                    yield_type: Some(self.any_type),
                    return_type: Some(self.any_type),
                    next_type: Some(self.any_type),
                };
            }
            let all_signatures = self.get_signatures_of_type(method_type, SignatureKind::Call);
            let mut valid_signatures: Vec<P<Signature>> = Vec::new();
            valid_signatures.extend(all_signatures.iter().filter_map(|&sig| {
                if self.get_min_argument_count(sig) == 0 { Some(sig) } else { None }
            }));
            if !valid_signatures.is_empty() {
                let return_types: Vec<P<Type>> = valid_signatures
                    .iter()
                    .map(|&sig| self.get_return_type_of_signature(sig))
                    .collect();
                let iterator_type = self.get_intersection_type(&return_types);
                return self.get_iteration_types_of_iterator_worker(
                    iterator_type,
                    r,
                    error_node,
                    Some(diagnostic_output),
                );
            }
            if error_node.is_some() && !all_signatures.is_empty() {
                let target = r.get_global_iterable_type_checked(self);
                self.check_type_assignable_to_ex(t, target, error_node, None, diagnostic_output);
            }
        }
        IterationTypes::default()
    }

    // Gets the *yield*, *return*, and *next* types from an `Iterator`-like or `AsyncIterator`-like type.
    //
    // If we successfully found the *yield*, *return*, and *next* types, an `IterationTypes` with non-nil
    // members is returned. Otherwise, a default `IterationTypes{}` is returned.
    // checker.go:6655
    pub(crate) fn get_iteration_types_of_iterator(
        &mut self,
        t: P<Type>,
        r: P<IterationTypesResolver>,
        error_node: Option<P<Node>>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> IterationTypes {
        self.get_iteration_types_of_iterator_worker(t, r, error_node, diagnostic_output)
    }

    // Gets the *yield*, *return*, and *next* types from an `Iterator`-like or `AsyncIterator`-like type.
    //
    // If we successfully found the *yield*, *return*, and *next* types, an `IterationTypes` with non-nil
    // members is returned. Otherwise, a default `IterationTypes{}` is returned.
    //
    // NOTE: You probably don't want to call this directly and should be calling `getIterationTypesOfIterator` instead.
    // checker.go:6665
    fn get_iteration_types_of_iterator_worker(
        &mut self,
        t: P<Type>,
        r: P<IterationTypesResolver>,
        error_node: Option<P<Node>>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> IterationTypes {
        if is_type_any(Some(t)) {
            return IterationTypes {
                yield_type: Some(self.any_type),
                return_type: Some(self.any_type),
                next_type: Some(self.any_type),
            };
        }
        let iteration_types = self.get_iteration_types_of_iterator_fast(t, r);
        if iteration_types.has_types() {
            return iteration_types;
        }
        self.get_iteration_types_of_iterator_slow(t, r, error_node, diagnostic_output)
    }

    // checker.go:6676
    fn get_iteration_types_of_iterator_fast(
        &mut self,
        t: P<Type>,
        r: P<IterationTypesResolver>,
    ) -> IterationTypes {
        // As an optimization, if the type is an instantiation of the following global type, then
        // just grab its related type arguments:
        // - `Iterable<T, TReturn, TNext>` or `AsyncIterable<T, TReturn, TNext>`
        // - `IteratorObject<T, TReturn, TNext>` or `AsyncIteratorObject<T, TReturn, TNext>`
        // - `IterableIterator<T, TReturn, TNext>` or `AsyncIterableIterator<T, TReturn, TNext>`
        // - `Generator<T, TReturn, TNext>` or `AsyncGenerator<T, TReturn, TNext>`
        if {
            let g = r.get_global_iterator_type(self);
            Self::is_reference_to_type(Some(t), g)
        } || {
            let g = r.get_global_iterator_object_type(self);
            Self::is_reference_to_type(Some(t), g)
        } || {
            let g = r.get_global_iterable_iterator_type(self);
            Self::is_reference_to_type(Some(t), g)
        } || {
            let g = r.get_global_generator_type(self);
            Self::is_reference_to_type(Some(t), g)
        } {
            let type_arguments = self.get_type_arguments(t);
            return r.get_resolved_iteration_types(
                self,
                type_arguments[0],
                type_arguments[1],
                type_arguments[2],
            );
        }
        // As an optimization, if the type is an instantiation of one of the following global types, then
        // just grab the related type argument:
        // - `ArrayIterator<T>`
        // - `MapIterator<T>`
        // - `SetIterator<T>`
        // - `StringIterator<T>`
        // - `ReadableStreamAsyncIterator<T>`
        let builtin = r.get_global_builtin_iterator_types(self);
        if Self::is_reference_to_some_type(Some(t), builtin) {
            let yield_type = self.get_type_arguments(t)[0];
            let return_type = self.get_builtin_iterator_return_type();
            let unknown_type = self.unknown_type;
            return r.get_resolved_iteration_types(self, yield_type, return_type, unknown_type);
        }
        IterationTypes::default()
    }

    // checker.go:6703
    fn get_iteration_types_of_iterator_slow(
        &mut self,
        t: P<Type>,
        r: P<IterationTypesResolver>,
        error_node: Option<P<Node>>,
        mut diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> IterationTypes {
        let next = self.get_iteration_types_of_method(
            t,
            r,
            "next",
            error_node,
            diagnostic_output.as_deref_mut(),
        );
        let return_ = self.get_iteration_types_of_method(
            t,
            r,
            "return",
            error_node,
            diagnostic_output.as_deref_mut(),
        );
        let throw =
            self.get_iteration_types_of_method(t, r, "throw", error_node, diagnostic_output);
        self.combine_iteration_types(&[next, return_, throw])
    }

    // checker.go:6711
    fn get_iteration_types_of_method(
        &mut self,
        t: P<Type>,
        resolver: P<IterationTypesResolver>,
        method_name: &str,
        error_node: Option<P<Node>>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> IterationTypes {
        let method = self.get_property_of_type(t, method_name);
        // Ignore 'return' or 'throw' if they are missing.
        if method.is_none() && method_name != "next" {
            return IterationTypes::default();
        }
        let mut method_type: Option<P<Type>> = None;
        if let Some(method) = method
            && !(method_name == "next" && method.flags().intersects(SymbolFlags::Optional))
        {
            if method_name == "next" {
                method_type = Some(self.get_type_of_symbol(method));
            } else {
                let ty = self.get_type_of_symbol(method);
                method_type = Some(self.get_type_with_facts(ty, TypeFacts::NEUndefinedOrNull));
            }
        }
        if is_type_any(method_type) {
            return IterationTypes {
                yield_type: Some(self.any_type),
                return_type: Some(self.any_type),
                next_type: Some(self.any_type),
            };
        }
        // Both async and non-async iterators *must* have a `next` method.
        let mut method_signatures: &[P<Signature>] = &[];
        if let Some(method_type) = method_type {
            method_signatures = self.get_signatures_of_type(method_type, SignatureKind::Call);
        }
        if method_signatures.is_empty() {
            if error_node.is_some() {
                let diagnostic = if method_name == "next" {
                    resolver.must_have_a_next_method_diagnostic
                } else {
                    resolver.must_be_a_method_diagnostic
                };
                self.report_diagnostic(
                    Some(new_diagnostic_for_node(error_node, Some(diagnostic), &[&method_name])),
                    diagnostic_output,
                );
            }
            return IterationTypes::default();
        }
        let method_type = method_type.unwrap();
        // If the method signature comes exclusively from the global iterator or generator type,
        // create iteration types from its type arguments like `getIterationTypesOfIteratorFast`
        // does (so as to remove `undefined` from the next and return types). We arrive here when
        // a contextual type for a generator was not a direct reference to one of those global types,
        // but looking up `methodType` referred to one of them (and nothing else). E.g., in
        // `interface SpecialIterator extends Iterator<number> {}`, `SpecialIterator` is not a
        // reference to `Iterator`, but its `next` member derives exclusively from `Iterator`.
        if method_signatures.len() == 1 && method_type.symbol().is_some() {
            let global_generator_type = resolver.get_global_generator_type(self);
            let global_iterator_type = resolver.get_global_iterator_type(self);
            let is_generator_method = global_generator_type.symbol().is_some_and(|s| {
                s.members().and_then(|m| m.lookup(method_name)) == method_type.symbol()
            });
            let is_iterator_method = !is_generator_method
                && global_iterator_type.symbol().is_some_and(|s| {
                    s.members().and_then(|m| m.lookup(method_name)) == method_type.symbol()
                });
            if is_generator_method || is_iterator_method {
                let type_parameters =
                    if is_generator_method { global_generator_type } else { global_iterator_type }
                        .as_interface_type()
                        .type_parameters();
                let mapper = method_type.mapper().unwrap();
                let mut next_type: Option<P<Type>> = None;
                if method_name == "next" {
                    next_type = Some(self.get_mapped_type(type_parameters[2], mapper));
                }
                let yield_type = self.get_mapped_type(type_parameters[0], mapper);
                let return_type = self.get_mapped_type(type_parameters[1], mapper);
                return IterationTypes {
                    yield_type: Some(yield_type),
                    return_type: Some(return_type),
                    next_type,
                };
            }
        }
        // Extract the first parameter and return type of each signature.
        let mut method_parameter_types: Option<Vec<P<Type>>> = None;
        let mut method_return_types: Option<Vec<P<Type>>> = None;
        for &signature in method_signatures {
            if method_name != "throw" && !signature.parameters().is_empty() {
                let ty = self.get_type_at_position(signature, 0);
                method_parameter_types.get_or_insert_with(Vec::new).push(ty);
            }
            let ty = self.get_return_type_of_signature(signature);
            method_return_types.get_or_insert_with(Vec::new).push(ty);
        }
        // Resolve the *next* or *return* type from the first parameter of a `next()` or
        // `return()` method, respectively.
        let mut return_types: Vec<P<Type>> = Vec::new();
        let mut next_type: Option<P<Type>> = None;
        if method_name != "throw" {
            let method_parameter_type =
                if let Some(method_parameter_types) = &method_parameter_types {
                    self.get_union_type(method_parameter_types)
                } else {
                    self.unknown_type
                };
            if method_name == "next" {
                // The value of `next(value)` is *not* awaited by async generators
                next_type = Some(method_parameter_type);
            } else if method_name == "return" {
                // The value of `return(value)` *is* awaited by async generators
                let resolved_method_parameter_type = resolver
                    .resolve_iteration_type(self, method_parameter_type, error_node)
                    .unwrap_or(self.any_type);
                return_types.push(resolved_method_parameter_type);
            }
        }
        // Resolve the *yield* and *return* types from the return type of the method (i.e. `IteratorResult`)
        let yield_type: Option<P<Type>>;
        let method_return_type = if let Some(method_return_types) = &method_return_types {
            self.get_intersection_type(method_return_types)
        } else {
            self.never_type
        };
        let resolved_method_return_type = resolver
            .resolve_iteration_type(self, method_return_type, error_node)
            .unwrap_or(self.any_type);
        let iteration_types =
            self.get_iteration_types_of_iterator_result(resolved_method_return_type);
        if iteration_types.has_types() {
            yield_type = iteration_types.yield_type;
            // Go appends a possibly-nil returnType (the `IteratorYieldResult<T>` fast path); getUnionType of a
            // single-element slice returns that element, nil included.
            match iteration_types.return_type {
                Some(t) => return_types.push(t),
                None if return_types.is_empty() => {
                    return IterationTypes { yield_type, return_type: None, next_type };
                }
                None => panic!("nil return type in getIterationTypesOfMethod"),
            }
        } else {
            if error_node.is_some() {
                self.report_diagnostic(
                    Some(new_diagnostic_for_node(
                        error_node,
                        Some(resolver.must_have_a_value_diagnostic),
                        &[&method_name],
                    )),
                    diagnostic_output,
                );
            }
            yield_type = Some(self.any_type);
            return_types.push(self.any_type);
        }
        let return_type = self.get_union_type(&return_types);
        IterationTypes { yield_type, return_type: Some(return_type), next_type }
    }

    // Gets the *yield* and *return* types of an `IteratorResult`-like type.
    //
    // If we are unable to determine a *yield* or a *return* type, `noIterationTypes` is
    // returned to indicate to the caller that it should handle the error. Otherwise, an
    // `IterationTypes` record is returned.
    // checker.go:6819
    fn get_iteration_types_of_iterator_result(&mut self, t: P<Type>) -> IterationTypes {
        if is_type_any(Some(t)) {
            return IterationTypes {
                yield_type: Some(self.any_type),
                return_type: Some(self.any_type),
                next_type: Some(self.any_type),
            };
        }
        // As an optimization, if the type is an instantiation of one of the global `IteratorYieldResult<T>`
        // or `IteratorReturnResult<TReturn>` types, then just grab its type argument.
        let yield_result_type = self.get_global_iterator_yield_result_type();
        if Self::is_reference_to_type(Some(t), yield_result_type) {
            return IterationTypes {
                yield_type: Some(self.get_type_arguments(t)[0]),
                return_type: None,
                next_type: None,
            };
        }
        let return_result_type = self.get_global_iterator_return_result_type();
        if Self::is_reference_to_type(Some(t), return_result_type) {
            return IterationTypes {
                yield_type: None,
                return_type: Some(self.get_type_arguments(t)[0]),
                next_type: None,
            };
        }
        // Choose any constituents that can produce the requested iteration type.
        let yield_iterator_result =
            self.filter_type(t, super::checker::Checker::is_yield_iterator_result);
        let mut yield_type: Option<P<Type>> = None;
        if yield_iterator_result != self.never_type {
            yield_type = self.get_type_of_property_of_type(
                yield_iterator_result,
                "value", /* as __String */
            );
        }
        let return_iterator_result =
            self.filter_type(t, super::checker::Checker::is_return_iterator_result);
        let mut return_type: Option<P<Type>> = None;
        if return_iterator_result != self.never_type {
            return_type = self.get_type_of_property_of_type(
                return_iterator_result,
                "value", /* as __String */
            );
        }
        if yield_type.is_none() && return_type.is_none() {
            return IterationTypes::default();
        }
        // From https://tc39.github.io/ecma262/#sec-iteratorresult-interface
        // > ... If the iterator does not have a return value, `value` is `undefined`. In that case, the
        // > `value` property may be absent from the conforming object if it does not inherit an explicit
        // > `value` property.
        IterationTypes {
            yield_type,
            return_type: Some(return_type.unwrap_or(self.void_type)),
            next_type: None,
        }
    }

    // checker.go:6852
    fn is_yield_iterator_result(&mut self, t: P<Type>) -> bool {
        self.is_iterator_result(t, IterationTypeKind::Yield)
    }

    // checker.go:6856
    fn is_return_iterator_result(&mut self, t: P<Type>) -> bool {
        self.is_iterator_result(t, IterationTypeKind::Return)
    }

    // checker.go:6860
    fn is_iterator_result(&mut self, t: P<Type>, kind: IterationTypeKind) -> bool {
        // From https://tc39.github.io/ecma262/#sec-iteratorresult-interface:
        // > [done] is the result status of an iterator `next` method call. If the end of the iterator was reached `done` is `true`.
        // > If the end was not reached `done` is `false` and a value is available.
        // > If a `done` property (either own or inherited) does not exist, it is consider to have the value `false`.
        let done_type = self.get_type_of_property_of_type(t, "done").unwrap_or(self.false_type);
        let source =
            if kind == IterationTypeKind::Yield { self.false_type } else { self.true_type };
        self.is_type_assignable_to(source, done_type)
    }

    // checker.go:6869
    pub(crate) fn report_type_not_iterable_error(
        &mut self,
        error_node: P<Node>,
        t: P<Type>,
        allow_async_iterables: bool,
    ) -> P<Diagnostic> {
        let message: &'static Message = if allow_async_iterables {
            &diagnostics::TYPE_0_MUST_HAVE_A_SYMBOL_ASYNC_ITERATOR_METHOD_THAT_RETURNS_AN_ASYNC_ITERATOR
        } else {
            &diagnostics::TYPE_0_MUST_HAVE_A_SYMBOL_ITERATOR_METHOD_THAT_RETURNS_AN_ITERATOR
        };
        let suggest_await = self.get_awaited_type_of_promise(t).is_some()
            || (!allow_async_iterables
                && is_for_of_statement(error_node.parent().unwrap())
                && error_node.parent().unwrap().expression() == Some(error_node)
                && self.get_global_async_iterable_type() != self.empty_generic_type
                && {
                    let global_async_iterable_type = self.get_global_async_iterable_type();
                    let any_type = self.any_type;
                    let target = self.create_type_from_generic_global_type(
                        global_async_iterable_type,
                        &[any_type, any_type, any_type],
                    );
                    self.is_type_assignable_to(t, target)
                });
        let type_string = self.type_to_string(t, None);
        self.error_and_maybe_suggest_await(
            Some(error_node),
            suggest_await,
            message,
            &[&type_string],
        )
    }

    // checker.go:6884
    pub(crate) fn get_iteration_diagnostic_details(
        &mut self,
        use_: IterationUse,
        input_type: P<Type>,
        allows_strings: bool,
    ) -> (&'static Message, bool) {
        let yield_type = self.get_iteration_type_of_iterable(
            use_,
            IterationTypeKind::Yield,
            input_type,
            None, /*errorNode*/
        );
        if yield_type.is_some() {
            return (&diagnostics::TYPE_0_CAN_ONLY_BE_ITERATED_THROUGH_WHEN_USING_THE_DOWNLEVEL_ITERATION_FLAG_OR_WITH_A_TARGET_OF_ES2015_OR_HIGHER, false);
        }
        if let Some(symbol) = input_type.symbol()
            && is_es2015_or_later_iterable(symbol.name())
        {
            return (&diagnostics::TYPE_0_CAN_ONLY_BE_ITERATED_THROUGH_WHEN_USING_THE_DOWNLEVEL_ITERATION_FLAG_OR_WITH_A_TARGET_OF_ES2015_OR_HIGHER, true);
        }
        if allows_strings {
            return (&diagnostics::TYPE_0_IS_NOT_AN_ARRAY_TYPE_OR_A_STRING_TYPE, true);
        }
        (&diagnostics::TYPE_0_IS_NOT_AN_ARRAY_TYPE, true)
    }
}

// checker.go:6898
pub fn is_es2015_or_later_iterable(n: &str) -> bool {
    matches!(
        n,
        "Float32Array"
            | "Float64Array"
            | "Int16Array"
            | "Int32Array"
            | "Int8Array"
            | "NodeList"
            | "Uint16Array"
            | "Uint32Array"
            | "Uint8Array"
            | "Uint8ClampedArray"
    )
}

impl Checker {
    // checker.go:6906
    pub(crate) fn check_alias_symbol(&mut self, node: P<Node>) {
        let mut symbol = self.get_symbol_of_declaration(node).unwrap();
        let target = self.resolve_alias(symbol);
        if target == self.unknown_symbol {
            return;
        }
        // For external modules, `symbol` represents the local symbol for an alias.
        // This local symbol will merge any other local declarations (excluding other aliases)
        // and symbol.flags will contains combined representation for all merged declaration.
        // Based on symbol.flags we can compute a set of excluded meanings (meaning that resolved alias should not have,
        // otherwise it will conflict with some local declaration). Note that in addition to normal flags we include matching SymbolFlags.Export*
        // in order to prevent collisions with declarations that were exported from the current module (they still contribute to local names).
        symbol = self.get_merged_symbol(symbol.export_symbol().unwrap_or(symbol));
        let target_flags = self.get_symbol_flags(target);
        // A type-only import/export will already have a grammar error in a JS file, so no need to issue more errors within
        if is_in_js_file(node)
            && !target_flags.intersects(SymbolFlags::Value)
            && !is_type_only_import_or_export_declaration(node)
        {
            let error_node = node.property_name_or_name().unwrap_or(node);
            assert_ne!(node.kind(), Kind::NamespaceExport);
            if is_export_specifier(node) {
                let diag = self.error(
                    Some(error_node),
                    &diagnostics::TYPES_CANNOT_APPEAR_IN_EXPORT_DECLARATIONS_IN_JAVA_SCRIPT_FILES,
                    &[],
                );
                if let Some(source_symbol) =
                    get_source_file_of_node(node).unwrap().as_node().symbol()
                {
                    let already_exported_symbol = source_symbol
                        .exports()
                        .and_then(|e| e.lookup(node.property_name_or_name().unwrap().text()));
                    if already_exported_symbol == Some(target) {
                        let already_exported_symbol = already_exported_symbol.unwrap();
                        let exporting_declaration =
                            find(already_exported_symbol.declarations(), |&d| {
                                is_js_type_alias_declaration(d)
                            });
                        if let Some(exporting_declaration) = exporting_declaration {
                            diag.add_related_info(new_diagnostic_for_node(
                                Some(exporting_declaration),
                                Some(&diagnostics::X_0_IS_AUTOMATICALLY_EXPORTED_HERE),
                                &[&already_exported_symbol.name()],
                            ));
                        }
                    }
                }
            } else {
                let mut identifier_text: &str = symbol.name();
                if is_identifier(error_node) {
                    identifier_text = error_node.text();
                }
                let mut specifier_text: &str = "...";
                if let Some(import_declaration) = find_ancestor(node, |n| {
                    is_import_or_import_equals_declaration(n) || is_variable_declaration(n)
                }) && let Some(module_specifier) =
                    try_get_module_specifier_from_declaration(import_declaration)
                {
                    specifier_text = module_specifier.text();
                }
                let mut import_text = format!("import(\"{specifier_text}\")");
                if is_import_specifier(node) {
                    import_text = import_text + "." + identifier_text;
                }
                self.error(
                    Some(error_node),
                    &diagnostics::X_0_IS_A_TYPE_AND_CANNOT_BE_IMPORTED_IN_JAVA_SCRIPT_FILES_USE_1_IN_A_JSDOC_TYPE_ANNOTATION,
                    &[&identifier_text, &import_text],
                );
            }
            return;
        }
        let excluded_meanings =
            (if symbol.flags().intersects(SymbolFlags::Value | SymbolFlags::ExportValue) {
                SymbolFlags::Value
            } else {
                SymbolFlags::None
            }) | (if symbol.flags().intersects(SymbolFlags::Type) {
                SymbolFlags::Type
            } else {
                SymbolFlags::None
            }) | (if symbol.flags().intersects(SymbolFlags::Namespace) {
                SymbolFlags::Namespace
            } else {
                SymbolFlags::None
            });
        if target_flags.intersects(excluded_meanings) {
            let message: &'static Message = if is_export_specifier(node) {
                &diagnostics::EXPORT_DECLARATION_CONFLICTS_WITH_EXPORTED_DECLARATION_OF_0
            } else {
                &diagnostics::IMPORT_DECLARATION_CONFLICTS_WITH_LOCAL_DECLARATION_OF_0
            };
            let symbol_string = self.symbol_to_string(symbol);
            self.error(Some(node), message, &[&symbol_string]);
        } else if !is_export_specifier(node) {
            // Look at 'compilerOptions.isolatedModules' and not 'getIsolatedModules(...)' (which considers 'verbatimModuleSyntax')
            // here because 'verbatimModuleSyntax' will already have an error for importing a type without 'import type'.
            let appears_valuey_to_transpiler = self.compiler_options.isolated_modules.is_true()
                && find_ancestor(node, is_type_only_import_or_export_declaration).is_none();
            if appears_valuey_to_transpiler
                && symbol.flags().intersects(SymbolFlags::Value | SymbolFlags::ExportValue)
            {
                let symbol_string = self.symbol_to_string(symbol);
                let flag_name = self.get_isolated_modules_like_flag_name();
                self.error(
                    Some(node),
                    &diagnostics::IMPORT_0_CONFLICTS_WITH_LOCAL_VALUE_SO_MUST_BE_DECLARED_WITH_A_TYPE_ONLY_IMPORT_WHEN_ISOLATED_MODULES_IS_ENABLED,
                    &[&symbol_string, &flag_name],
                );
            }
        }
        if self.compiler_options.get_isolated_modules()
            && !is_type_only_import_or_export_declaration(node)
            && !node.flags().intersects(NodeFlags::Ambient)
        {
            let type_only_alias = self.get_type_only_alias_declaration(symbol);
            let is_type = !target_flags.intersects(SymbolFlags::Value);
            if is_type || type_only_alias.is_some() {
                match node.kind() {
                    Kind::ImportClause | Kind::ImportSpecifier | Kind::ImportEqualsDeclaration => {
                        if self.compiler_options.verbatim_module_syntax.is_true() {
                            assert!(node.name().is_some(), "An ImportClause with a symbol should have a name");
                            let message: &'static Message = if self.compiler_options.verbatim_module_syntax.is_true() && crate::is_internal_module_import_equals_declaration(node) {
                                &diagnostics::AN_IMPORT_ALIAS_CANNOT_RESOLVE_TO_A_TYPE_OR_TYPE_ONLY_DECLARATION_WHEN_VERBATIM_MODULE_SYNTAX_IS_ENABLED
                            } else if is_type {
                                &diagnostics::X_0_IS_A_TYPE_AND_MUST_BE_IMPORTED_USING_A_TYPE_ONLY_IMPORT_WHEN_VERBATIM_MODULE_SYNTAX_IS_ENABLED
                            } else {
                                &diagnostics::X_0_RESOLVES_TO_A_TYPE_ONLY_DECLARATION_AND_MUST_BE_IMPORTED_USING_A_TYPE_ONLY_IMPORT_WHEN_VERBATIM_MODULE_SYNTAX_IS_ENABLED
                            };
                            let name = node.property_name_or_name().unwrap().text();
                            let diag = self.error(Some(node), message, &[&name]);
                            Self::add_type_only_declaration_related_info(diag, if is_type { None } else { type_only_alias }, name);
                        }
                        if is_type && node.kind() == Kind::ImportEqualsDeclaration && has_modifier(node, ModifierFlags::Export) {
                            let flag_name = self.get_isolated_modules_like_flag_name();
                            self.error(Some(node), &diagnostics::CANNOT_USE_EXPORT_IMPORT_ON_A_TYPE_OR_TYPE_ONLY_NAMESPACE_WHEN_0_IS_ENABLED, &[&flag_name]);
                        }
                    }
                    Kind::ExportSpecifier
                        // Don't allow re-exporting an export that will be elided when `--isolatedModules` is set.
                        // The exception is that `import type { A } from './a'; export { A }` is allowed
                        // because single-file analysis can determine that the export should be dropped.
                        if (self.compiler_options.verbatim_module_syntax.is_true() || get_source_file_of_node(type_only_alias) != get_source_file_of_node(node)) => {
                            let name = node.property_name_or_name().unwrap().text();
                            let diagnostic = if is_type {
                                let flag_name = self.get_isolated_modules_like_flag_name();
                                self.error(Some(node), &diagnostics::RE_EXPORTING_A_TYPE_WHEN_0_IS_ENABLED_REQUIRES_USING_EXPORT_TYPE, &[&flag_name])
                            } else {
                                let flag_name = self.get_isolated_modules_like_flag_name();
                                self.error(
                                    Some(node),
                                    &diagnostics::X_0_RESOLVES_TO_A_TYPE_ONLY_DECLARATION_AND_MUST_BE_RE_EXPORTED_USING_A_TYPE_ONLY_RE_EXPORT_WHEN_1_IS_ENABLED,
                                    &[&name, &flag_name],
                                )
                            };
                            Self::add_type_only_declaration_related_info(diagnostic, if is_type { None } else { type_only_alias }, name);
                        }
                    _ => {}
                }
            }
            if self.compiler_options.verbatim_module_syntax.is_true()
                && !is_import_equals_declaration(node)
                && !is_in_js_file(node)
                && self.emit_module_format_of_file(get_source_file_of_node(node).unwrap())
                    == ModuleKind::COMMON_JS
            {
                self.error(Some(node), get_verbatim_module_syntax_error_message(node), &[]);
            } else if self.module_kind == ModuleKind::PRESERVE
                && !is_import_equals_declaration(node)
                && !is_variable_declaration(node)
                && !is_binding_element(node)
                && self.emit_module_format_of_file(get_source_file_of_node(node).unwrap())
                    == ModuleKind::COMMON_JS
            {
                // In `--module preserve`, ESM input syntax emits ESM output syntax, but there will be times
                // when we look at the `impliedNodeFormat` of this file and decide it's CommonJS (i.e., currently,
                // only if the file extension is .cjs/.cts). To avoid that inconsistency, we disallow ESM syntax
                // in files that are unambiguously CommonJS in this mode.
                self.error(Some(node), &diagnostics::ECMA_SCRIPT_MODULE_SYNTAX_IS_NOT_ALLOWED_IN_A_COMMON_JS_MODULE_WHEN_MODULE_IS_SET_TO_PRESERVE, &[]);
            }
            if self.compiler_options.verbatim_module_syntax.is_true()
                && !is_type_only_import_or_export_declaration(node)
                && !node.flags().intersects(NodeFlags::Ambient)
                && target_flags.intersects(SymbolFlags::ConstEnum)
            {
                let const_enum_declaration = target.value_declaration();
                if let Some(const_enum_declaration) = const_enum_declaration
                    && const_enum_declaration.flags().intersects(NodeFlags::Ambient)
                {
                    let redirect = self.program.get_project_reference_from_output_dts(
                        get_source_file_of_node(const_enum_declaration).unwrap().path(),
                    );
                    if redirect.is_none()
                        || !redirect
                            .unwrap()
                            .resolved
                            .compiler_options()
                            .unwrap()
                            .should_preserve_const_enums()
                    {
                        let flag_name = self.get_isolated_modules_like_flag_name();
                        self.error(
                            Some(node),
                            &diagnostics::CANNOT_ACCESS_AMBIENT_CONST_ENUMS_WHEN_0_IS_ENABLED,
                            &[&flag_name],
                        );
                    }
                }
            }
        }
        if is_import_specifier(node) {
            let target_symbol = self.resolve_alias_with_deprecation_check(symbol, node);
            if self.is_deprecated_symbol(target_symbol) && !target_symbol.declarations().is_empty()
            {
                let declarations = target_symbol.declarations();
                self.add_deprecated_suggestion(node, declarations, target_symbol.name());
            }
        }
    }

    // checker.go:7036
    pub(crate) fn are_declaration_flags_identical(left: P<Node>, right: P<Node>) -> bool {
        if is_parameter_declaration(left) && is_variable_declaration(right)
            || is_variable_declaration(left) && is_parameter_declaration(right)
        {
            // Differences in optionality between parameters and variables are allowed.
            return true;
        }
        if is_optional_declaration(left) != is_optional_declaration(right) {
            return false;
        }
        let interesting_flags = ModifierFlags::Private
            | ModifierFlags::Protected
            | ModifierFlags::Async
            | ModifierFlags::Abstract
            | ModifierFlags::Readonly
            | ModifierFlags::Static;
        get_selected_modifier_flags(left, interesting_flags)
            == get_selected_modifier_flags(right, interesting_flags)
    }

    // checker.go:7048
    pub(crate) fn check_type_alias_declaration(&mut self, node: P<Node>) {
        // Grammar checking
        self.check_grammar_modifiers(node);
        self.check_type_name_is_reserved(
            node.name().unwrap(),
            &diagnostics::TYPE_ALIAS_NAME_CANNOT_BE_0,
        );
        if !Self::container_allows_block_scoped_variable(node.parent().unwrap()) {
            self.grammar_error_on_node(
                node,
                &diagnostics::X_0_DECLARATIONS_CAN_ONLY_BE_DECLARED_INSIDE_A_BLOCK,
                &[&"type"],
            );
        }
        self.check_exports_on_merged_declarations(node);

        let type_node = node.type_node();
        let type_parameters = node.type_parameters();
        self.check_type_parameters(type_parameters);
        if let Some(type_node) = type_node
            && type_node.kind() == Kind::IntrinsicKeyword
        {
            let name = node.name().unwrap().text();
            if !(type_parameters.is_empty() && name == "BuiltinIteratorReturn"
                || type_parameters.len() == 1
                    && INTRINSIC_TYPE_KINDS
                        .get(name)
                        .copied()
                        .unwrap_or(IntrinsicTypeKind::Unknown)
                        != IntrinsicTypeKind::Unknown)
            {
                self.error(Some(type_node), &diagnostics::THE_INTRINSIC_KEYWORD_CAN_ONLY_BE_USED_TO_DECLARE_COMPILER_PROVIDED_INTRINSIC_TYPES, &[]);
            }
            // The `intrinsic` keyword is a leaf type node with no child nodes to check,
            // so skipping the checkSourceElement below visits nothing.
            return;
        }
        self.check_source_element(type_node);
        self.register_for_unused_identifiers_check(node);
    }

    // checker.go:7073
    pub(crate) fn check_type_name_is_reserved(&mut self, name: P<Node>, message: &'static Message) {
        // TS 1.0 spec (April 2014): 3.6.1
        // The predefined type keywords are reserved and cannot be used as names of user defined types.
        match name.text() {
            "any" | "unknown" | "never" | "number" | "bigint" | "boolean" | "string" | "symbol"
            | "void" | "object" | "undefined" => {
                self.error(Some(name), message, &[&name.text()]);
            }
            _ => {}
        }
    }

    // checker.go:7082
    pub(crate) fn check_exports_on_merged_declarations(&mut self, node: P<Node>) {
        // If localSymbol is defined on node then node itself is exported - check is required.
        let symbol = if let Some(symbol) = node.local_symbol() {
            symbol
        } else {
            // Local symbol is undefined => this declaration is non-exported.
            // However, symbol might contain other declarations that are exported.
            let symbol = self.get_symbol_of_declaration(node).unwrap();
            if symbol.export_symbol().is_none() {
                // This is a pure local symbol (all declarations are non-exported) - no need to check anything.
                return;
            }
            symbol
        };
        if symbol.declarations().len() < 2 || !self.merged_exports_checked.add_if_absent(symbol) {
            return;
        }
        let mut exported_declaration_spaces = DeclarationSpaces::None;
        let mut non_exported_declaration_spaces = DeclarationSpaces::None;
        let mut default_exported_declaration_spaces = DeclarationSpaces::None;
        let declarations = symbol.declarations();
        for &d in declarations {
            let declaration_spaces = self.get_declaration_spaces(d);
            let effective_declaration_flags = self
                .get_effective_declaration_flags(d, ModifierFlags::Export | ModifierFlags::Default);
            if effective_declaration_flags.intersects(ModifierFlags::Export) {
                if effective_declaration_flags.intersects(ModifierFlags::Default) {
                    default_exported_declaration_spaces |= declaration_spaces;
                } else {
                    exported_declaration_spaces |= declaration_spaces;
                }
            } else {
                non_exported_declaration_spaces |= declaration_spaces;
            }
        }
        // Spaces for anything not declared a 'default export'.
        let non_default_exported_declaration_spaces =
            exported_declaration_spaces | non_exported_declaration_spaces;
        let common_declaration_spaces_for_exports_and_locals =
            exported_declaration_spaces & non_exported_declaration_spaces;
        let common_declaration_spaces_for_default_and_non_default =
            default_exported_declaration_spaces & non_default_exported_declaration_spaces;
        if !common_declaration_spaces_for_exports_and_locals.is_empty()
            || !common_declaration_spaces_for_default_and_non_default.is_empty()
        {
            // declaration spaces for exported and non-exported declarations intersect
            let declarations = symbol.declarations();
            for &d in declarations {
                let declaration_spaces = self.get_declaration_spaces(d);
                let name = get_name_of_declaration(d);
                // Only error on the declarations that contributed to the intersecting spaces.
                if declaration_spaces
                    .intersects(common_declaration_spaces_for_default_and_non_default)
                {
                    self.error(
                        name,
                        &diagnostics::MERGED_DECLARATION_0_CANNOT_INCLUDE_A_DEFAULT_EXPORT_DECLARATION_CONSIDER_ADDING_A_SEPARATE_EXPORT_DEFAULT_0_DECLARATION_INSTEAD,
                        &[&tsrs_scanner::declaration_name_to_string(name)],
                    );
                } else if declaration_spaces
                    .intersects(common_declaration_spaces_for_exports_and_locals)
                {
                    self.error(name, &diagnostics::INDIVIDUAL_DECLARATIONS_IN_MERGED_DECLARATION_0_MUST_BE_ALL_EXPORTED_OR_ALL_LOCAL, &[&tsrs_scanner::declaration_name_to_string(name)]);
                }
            }
        }
    }

    // checker.go:7133
    fn get_declaration_spaces(&mut self, node: P<Node>) -> DeclarationSpaces {
        fn alias_spaces(c: &mut Checker, node: P<Node>) -> DeclarationSpaces {
            let mut result = DeclarationSpaces::None;
            let symbol = c.get_symbol_of_declaration(node).unwrap();
            let target = c.resolve_alias(symbol);
            let declarations = target.declarations();
            result = declarations.iter().fold(result, |mut result, &d| {
                result |= c.get_declaration_spaces(d);

                result
            });
            result
        }
        match node.kind() {
            Kind::ModuleDeclaration => {
                if is_ambient_module(node)
                    || get_module_instance_state(node) != ModuleInstanceState::NonInstantiated
                {
                    return DeclarationSpaces::ExportNamespace | DeclarationSpaces::ExportValue;
                }
                DeclarationSpaces::ExportNamespace
            }
            Kind::ClassDeclaration | Kind::EnumDeclaration | Kind::EnumMember => {
                DeclarationSpaces::ExportType | DeclarationSpaces::ExportValue
            }
            Kind::SourceFile => {
                DeclarationSpaces::ExportType
                    | DeclarationSpaces::ExportValue
                    | DeclarationSpaces::ExportNamespace
            }
            Kind::ExportAssignment | Kind::BinaryExpression => {
                let expression = if is_export_assignment(node) {
                    node.expression().unwrap()
                } else {
                    node.as_binary_expression().right()
                };
                // Export assigned entity name expressions act as aliases and should fall through, otherwise they export values.
                if !is_entity_name_expression(expression)
                    || !self
                        .get_symbol_of_declaration(node)
                        .unwrap()
                        .flags()
                        .intersects(SymbolFlags::Alias)
                {
                    return DeclarationSpaces::ExportValue;
                }
                // The below options all declare an Alias, which is allowed to merge with other values within the importing module.
                alias_spaces(self, node)
            }
            Kind::ImportEqualsDeclaration | Kind::NamespaceImport | Kind::ImportClause => {
                alias_spaces(self, node)
            }
            Kind::VariableDeclaration
            | Kind::BindingElement
            | Kind::FunctionDeclaration
            | Kind::ImportSpecifier => DeclarationSpaces::ExportValue,
            Kind::InterfaceDeclaration
            | Kind::TypeAliasDeclaration
            | Kind::JSTypeAliasDeclaration
            | Kind::JSDocTypedefTag
            | Kind::JSDocCallbackTag
            | Kind::MethodSignature
            | Kind::PropertySignature => DeclarationSpaces::ExportType,
            _ => panic!("Unhandled case in getDeclarationSpaces: {}", node.kind_string()),
        }
    }

    // checker.go:7174
    pub(crate) fn check_type_parameters(&mut self, type_parameter_declarations: &[P<Node>]) {
        let mut seen_default = false;
        for (i, &node) in type_parameter_declarations.iter().enumerate() {
            self.check_type_parameter(node);
            let default_type_node = node.as_type_parameter_declaration().default_type;
            if let Some(default_type_node) = default_type_node {
                seen_default = true;
                self.check_type_parameters_not_referenced(
                    default_type_node,
                    type_parameter_declarations,
                    i,
                );
            } else if seen_default {
                self.error(
                    Some(node),
                    &diagnostics::REQUIRED_TYPE_PARAMETERS_MAY_NOT_FOLLOW_OPTIONAL_TYPE_PARAMETERS,
                    &[],
                );
            }
            for element in &type_parameter_declarations[..i] {
                if element.symbol() == node.symbol() {
                    self.error(
                        node.name(),
                        &diagnostics::DUPLICATE_IDENTIFIER_0,
                        &[&tsrs_scanner::declaration_name_to_string(node.name())],
                    );
                }
            }
        }
    }

    // Check that type parameter defaults only reference previously declared type parameters */
    // checker.go:7194
    fn check_type_parameters_not_referenced(
        &mut self,
        root: P<Node>,
        type_parameters: &[P<Node>],
        index: usize,
    ) {
        fn visit(
            c: &mut Checker,
            node: P<Node>,
            type_parameters: &[P<Node>],
            index: usize,
        ) -> bool {
            if is_type_reference_node(node) {
                let t = c.get_type_from_type_reference(node);
                if t.flags().intersects(TypeFlags::TypeParameter) {
                    for element in type_parameters.iter().skip(index) {
                        if t.symbol() == c.get_symbol_of_declaration(*element) {
                            c.error(Some(node), &diagnostics::TYPE_PARAMETER_DEFAULTS_CAN_ONLY_REFERENCE_PREVIOUSLY_DECLARED_TYPE_PARAMETERS, &[]);
                        }
                    }
                }
            }
            node.for_each_child(&mut |child| visit(c, child, type_parameters, index))
        }
        visit(self, root, type_parameters, index);
    }

    // checker.go:7212
    pub(crate) fn register_for_unused_identifiers_check(&mut self, node: P<Node>) {
        let source_file = get_source_file_of_node(node).unwrap();
        let links = self.source_file_links.get(source_file);
        links.identifier_check_nodes.borrow_mut().push(node);
    }

    // checker.go:7218
    pub(crate) fn check_unused_identifiers(&mut self, potentially_unused_identifiers: &[P<Node>]) {
        for &node in potentially_unused_identifiers {
            match node.kind() {
                Kind::ClassDeclaration | Kind::ClassExpression => {
                    self.check_unused_class_members(node);
                    self.check_unused_type_parameters(node);
                }
                Kind::SourceFile
                | Kind::ModuleDeclaration
                | Kind::Block
                | Kind::CaseBlock
                | Kind::ForStatement
                | Kind::ForInStatement
                | Kind::ForOfStatement
                | Kind::ClassStaticBlockDeclaration => {
                    self.check_unused_locals_and_parameters(node);
                }
                Kind::Constructor
                | Kind::FunctionExpression
                | Kind::FunctionDeclaration
                | Kind::ArrowFunction
                | Kind::MethodDeclaration
                | Kind::GetAccessor
                | Kind::SetAccessor => {
                    // Only report unused parameters on the implementation, not overloads.
                    if node.body().is_some() {
                        self.check_unused_locals_and_parameters(node);
                    }
                    self.check_unused_type_parameters(node);
                }
                Kind::MethodSignature
                | Kind::CallSignature
                | Kind::ConstructSignature
                | Kind::FunctionType
                | Kind::ConstructorType
                | Kind::TypeAliasDeclaration
                | Kind::JSTypeAliasDeclaration
                | Kind::InterfaceDeclaration => {
                    self.check_unused_type_parameters(node);
                }
                Kind::InferType => {
                    self.check_unused_infer_type_parameter(node);
                }
                _ => panic!("Unhandled case in checkUnusedIdentifiers"),
            }
        }
    }

    // checker.go:7245
    fn is_referenced(&self, symbol: P<Symbol>) -> bool {
        !self.reference_kinds(symbol).is_empty()
    }

    // checker.go:7256
    fn report_unused_variable(&mut self, location: P<Node>, diagnostic: P<Diagnostic>) {
        let mut location = location;
        while is_binding_element(location) || is_binding_pattern(location) {
            location = location.parent().unwrap();
        }
        let kind = if is_parameter_declaration(location) {
            UnusedKind::Parameter
        } else {
            UnusedKind::Local
        };
        self.report_unused(location, kind, diagnostic);
    }

    // checker.go:7263
    fn report_unused(&mut self, location: P<Node>, kind: UnusedKind, diagnostic: P<Diagnostic>) {
        if !location
            .flags()
            .intersects(NodeFlags::Ambient | NodeFlags::ThisNodeOrAnySubNodesHasError)
        {
            let is_error = self.unused_is_error(kind);
            if is_error {
                self.add_diagnostic(diagnostic);
            } else {
                let suggestion = diagnostic.clone_diagnostic();
                suggestion.set_category(diagnostics::Category::Suggestion);
                self.add_suggestion_diagnostic(suggestion);
            }
        }
    }

    // checker.go:7276
    fn unused_is_error(&self, kind: UnusedKind) -> bool {
        match kind {
            UnusedKind::Local => self.compiler_options.no_unused_locals.is_true(),
            UnusedKind::Parameter => self.compiler_options.no_unused_parameters.is_true(),
        }
    }

    // checker.go:7287
    fn check_unused_class_members(&mut self, node: P<Node>) {
        for &member in node.members() {
            match member.kind() {
                Kind::MethodDeclaration
                | Kind::PropertyDeclaration
                | Kind::GetAccessor
                | Kind::SetAccessor => {
                    if is_set_accessor_declaration(member)
                        && member.symbol().unwrap().flags().intersects(SymbolFlags::GetAccessor)
                    {
                        continue; // Already would have reported an error on the getter.
                    }
                    let symbol = self.get_symbol_of_declaration(member).unwrap();
                    if !self.is_referenced(symbol)
                        && (has_modifier(member, ModifierFlags::Private)
                            || member.name().is_some_and(is_private_identifier))
                        && !member.flags().intersects(NodeFlags::Ambient)
                    {
                        let symbol_string = self.symbol_to_string(symbol);
                        self.report_unused(
                            member,
                            UnusedKind::Local,
                            new_diagnostic_for_node(
                                member.name(),
                                Some(&diagnostics::X_0_IS_DECLARED_BUT_ITS_VALUE_IS_NEVER_READ),
                                &[&symbol_string],
                            ),
                        );
                    }
                }
                Kind::Constructor => {
                    for &parameter in member.parameters() {
                        if !self.is_referenced(parameter.symbol().unwrap())
                            && has_syntactic_modifier(parameter, ModifierFlags::Private)
                        {
                            self.report_unused(
                                parameter,
                                UnusedKind::Local,
                                new_diagnostic_for_node(
                                    parameter.name(),
                                    Some(&diagnostics::PROPERTY_0_IS_DECLARED_BUT_ITS_VALUE_IS_NEVER_READ),
                                    &[&symbol_name(parameter.symbol().unwrap())],
                                ),
                            );
                        }
                    }
                }
                Kind::IndexSignature
                | Kind::SemicolonClassElement
                | Kind::ClassStaticBlockDeclaration
                | Kind::JSTypeAliasDeclaration => {
                    // Can't be private
                }
                _ => panic!("Unhandled case in checkUnusedClassMembers"),
            }
        }
    }

    // checker.go:7312
    fn check_unused_locals_and_parameters(&mut self, node: P<Node>) {
        // Go iterates a map-based set and a map here; insertion-ordered collections keep the port deterministic.
        let mut variable_parents: OrderedSet<P<Node>> = OrderedSet::default();
        let mut import_clauses: OrderedMap<P<Node>, Vec<P<Node>>> = OrderedMap::default();
        let locals = node.locals().map(|l| l.values()).unwrap_or_default();
        for local in locals {
            let reference_kinds = self.reference_kinds(local);
            if local.flags().intersects(SymbolFlags::TypeParameter)
                && (!local.flags().intersects(SymbolFlags::Variable)
                    || reference_kinds.intersects(SymbolFlags::Variable))
                || !local.flags().intersects(SymbolFlags::TypeParameter)
                    && (!reference_kinds.is_empty()
                        || local.export_symbol().is_some()
                        || local.flags().intersects(SymbolFlags::ModuleExports))
            {
                continue;
            }
            let declarations = local.declarations();
            for &declaration in declarations {
                if is_variable_declaration(declaration)
                    || is_parameter_declaration(declaration)
                    || is_binding_element(declaration)
                {
                    variable_parents.insert(get_root_declaration(declaration).parent().unwrap());
                } else if is_import_clause(declaration)
                    || is_import_specifier(declaration)
                    || is_namespace_import(declaration)
                {
                    if !is_identifier_that_starts_with_underscore(declaration.name().unwrap()) {
                        let import_clause = import_clause_from_imported(declaration).unwrap();
                        import_clauses.entry(import_clause).or_default().push(declaration);
                    }
                } else if !is_type_parameter_declaration(declaration)
                    && !is_ambient_module(declaration)
                {
                    self.report_unused_local(declaration, symbol_name(local));
                }
            }
        }
        for declaration in variable_parents {
            if is_variable_declaration_list(declaration) {
                self.report_unused_variables(declaration);
            } else {
                self.report_unused_parameters(declaration);
            }
        }
        for (declaration, unuseds) in import_clauses {
            self.report_unused_imports(declaration, &unuseds);
        }
    }

    // checker.go:7353
    fn report_unused_local(&mut self, node: P<Node>, name: &str) {
        let message: &'static Message = if is_type_declaration(node) {
            &diagnostics::X_0_IS_DECLARED_BUT_NEVER_USED
        } else {
            &diagnostics::X_0_IS_DECLARED_BUT_ITS_VALUE_IS_NEVER_READ
        };
        self.report_unused(
            node,
            UnusedKind::Local,
            new_diagnostic_for_node(Some(node.name().unwrap_or(node)), Some(message), &[&name]),
        );
    }

    // checker.go:7358
    fn report_unused_variables(&mut self, node: P<Node>) {
        let declarations = node.as_variable_declaration_list().declarations.nodes();
        if declarations.len() > 1
            && declarations.iter().all(|&d| self.is_unreferenced_variable_declaration(d))
        {
            self.report_unused_variable(
                node,
                new_diagnostic_for_node(
                    Some(node),
                    Some(&diagnostics::ALL_VARIABLES_ARE_UNUSED),
                    &[],
                ),
            );
        } else {
            self.report_unused_variable_declarations(declarations);
        }
    }

    // checker.go:7367
    fn report_unused_parameters(&mut self, node: P<Node>) {
        self.report_unused_variable_declarations(node.parameters());
    }

    // checker.go:7371
    fn report_unused_binding_elements(&mut self, node: P<Node>) {
        let declarations = node.elements();
        if declarations.len() > 1
            && declarations.iter().all(|&d| self.is_unreferenced_variable_declaration(d))
        {
            self.report_unused_variable(
                node,
                new_diagnostic_for_node(
                    Some(node),
                    Some(&diagnostics::ALL_DESTRUCTURED_ELEMENTS_ARE_UNUSED),
                    &[],
                ),
            );
        } else {
            self.report_unused_variable_declarations(declarations);
        }
    }

    // checker.go:7380
    fn report_unused_variable_declarations(&mut self, declarations: &[P<Node>]) {
        for &declaration in declarations {
            let name = declaration.name();
            if let Some(name) = name
                && !is_parameter_property_declaration(declaration, declaration.parent().unwrap())
                && !is_this_parameter(declaration)
            {
                if is_binding_pattern(name) {
                    self.report_unused_binding_elements(name);
                } else if self.is_unreferenced_variable_declaration(declaration) {
                    self.report_unused_variable(
                        declaration,
                        new_diagnostic_for_node(
                            Some(name),
                            Some(&diagnostics::X_0_IS_DECLARED_BUT_ITS_VALUE_IS_NEVER_READ),
                            &[&name.text()],
                        ),
                    );
                }
            }
        }
    }

    // checker.go:7393
    fn is_unreferenced_variable_declaration(&mut self, node: P<Node>) -> bool {
        let Some(name) = node.name() else {
            return true;
        };
        if is_binding_pattern(name) {
            return name.elements().iter().all(|&e| self.is_unreferenced_variable_declaration(e));
        }
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        if self.reference_kinds(symbol).intersects(SymbolFlags::Variable) {
            return false;
        }
        if is_binding_element(node) && is_object_binding_pattern(node.parent().unwrap()) {
            // In `{ a, ...b }, `a` is considered used since it removes a property from `b`. `b` may still be unused though.
            let last_element = last_or_nil(node.parent().unwrap().elements());
            if Some(node) != last_element && has_dot_dot_dot_token(last_element.unwrap()) {
                return false;
            }
        }
        if (is_parameter_declaration(node)
            || is_variable_declaration(node)
                && (is_for_in_or_of_statement(node.parent().unwrap().parent())
                    || self.get_combined_node_flags_cached(node).intersects(NodeFlags::Using))
            || is_binding_element(node)
                && !(is_object_binding_pattern(node.parent().unwrap())
                    && node.property_name().is_none()))
            && is_identifier_that_starts_with_underscore(name)
        {
            return false;
        }
        true
    }

    // checker.go:7420
    fn report_unused_imports(&mut self, node: P<Node>, unuseds: &[P<Node>]) {
        let mut declaration_count: usize = usize::from(node.name().is_some());
        let named_bindings = node.as_import_clause().named_bindings;
        if let Some(named_bindings) = named_bindings {
            if is_namespace_import(named_bindings) {
                declaration_count += 1;
            } else {
                declaration_count += named_bindings.elements().len();
            }
        }
        if declaration_count > 1 && declaration_count == unuseds.len() {
            self.report_unused(
                node,
                UnusedKind::Local,
                new_diagnostic_for_node(
                    node.parent(),
                    Some(&diagnostics::ALL_IMPORTS_IN_IMPORT_DECLARATION_ARE_UNUSED),
                    &[],
                ),
            );
        } else {
            for &unused in unuseds {
                self.report_unused_local(unused, unused.name().unwrap().text());
            }
        }
    }
}

// checker.go:7439
pub fn is_identifier_that_starts_with_underscore(node: P<Node>) -> bool {
    is_identifier(node) && !node.text().is_empty() && node.text().as_bytes()[0] == b'_'
}

// checker.go:7443
pub fn import_clause_from_imported(node: P<Node>) -> Option<P<Node>> {
    match node.kind() {
        Kind::ImportClause => Some(node),
        Kind::NamespaceImport => node.parent(),
        _ => node.parent().unwrap().parent(),
    }
}

impl Checker {
    // checker.go:7454
    fn check_unused_infer_type_parameter(&mut self, node: P<Node>) {
        let type_parameter = node.as_infer_type_node().type_parameter;
        if self.is_unreferenced_type_parameter(type_parameter) {
            let name = type_parameter.name().unwrap();
            self.report_unused(
                node,
                UnusedKind::Parameter,
                new_diagnostic_for_node(
                    Some(name),
                    Some(&diagnostics::X_0_IS_DECLARED_BUT_NEVER_USED),
                    &[&name.text()],
                ),
            );
        }
    }

    // checker.go:7461
    fn check_unused_type_parameters(&mut self, node: P<Node>) {
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        if !all_declarations_in_same_source_file(symbol) {
            return;
        }
        let Some(type_parameter_list) = node.type_parameter_list() else {
            return;
        };
        if type_parameter_list.nodes().len() > 1
            && type_parameter_list.nodes().iter().all(|&tp| self.is_unreferenced_type_parameter(tp))
        {
            let file = get_source_file_of_node(node).unwrap();
            let loc = range_of_type_parameters(file, type_parameter_list);
            self.report_unused(
                node,
                UnusedKind::Parameter,
                new_diagnostic(Some(file), loc, &diagnostics::ALL_TYPE_PARAMETERS_ARE_UNUSED, &[]),
            );
        } else {
            for &type_parameter in type_parameter_list.nodes() {
                if self.is_unreferenced_type_parameter(type_parameter) {
                    let name = type_parameter.name().unwrap();
                    self.report_unused(
                        node,
                        UnusedKind::Parameter,
                        new_diagnostic_for_node(
                            Some(type_parameter),
                            Some(&diagnostics::X_0_IS_DECLARED_BUT_NEVER_USED),
                            &[&name.text()],
                        ),
                    );
                }
            }
        }
    }

    // checker.go:7482
    fn is_unreferenced_type_parameter(&mut self, type_parameter: P<Node>) -> bool {
        let symbol = self.get_merged_symbol(type_parameter.symbol().unwrap());
        !self.reference_kinds(symbol).intersects(SymbolFlags::TypeParameter)
            && !is_identifier_that_starts_with_underscore(type_parameter.name().unwrap())
    }

    // checker.go:7486
    pub(crate) fn check_unused_renamed_binding_elements(&mut self) {
        let nodes = self.renamed_binding_elements_in_types.clone();
        for node in nodes {
            let symbol = self.get_symbol_of_declaration(node).unwrap();
            if self.reference_kinds(symbol).is_empty() {
                let wrapping_declaration = walk_up_binding_elements_and_patterns(node);
                assert!(
                    is_part_of_parameter_declaration(wrapping_declaration),
                    "Only parameter declaration should be checked here"
                );
                let diagnostic = new_diagnostic_for_node(
                    node.name(),
                    Some(&diagnostics::X_0_IS_AN_UNUSED_RENAMING_OF_1_DID_YOU_INTEND_TO_USE_IT_AS_A_TYPE_ANNOTATION),
                    &[&tsrs_scanner::declaration_name_to_string(node.name()), &tsrs_scanner::declaration_name_to_string(node.property_name())],
                );
                if wrapping_declaration.type_node().is_none() {
                    // entire parameter does not have type annotation, suggest adding an annotation
                    diagnostic.add_related_info(new_diagnostic(
                        get_source_file_of_node(wrapping_declaration),
                        TextRange::from_text_pos(wrapping_declaration.end(), wrapping_declaration.end()),
                        &diagnostics::WE_CAN_ONLY_WRITE_A_TYPE_FOR_0_BY_ADDING_A_TYPE_FOR_THE_ENTIRE_PARAMETER_HERE,
                        &[&tsrs_scanner::declaration_name_to_string(node.property_name())],
                    ));
                }
                self.add_diagnostic(diagnostic);
            }
        }
    }

    // checker.go:7501
    pub(crate) fn check_expression_statement(&mut self, node: P<Node>) {
        // Grammar checking
        self.check_grammar_statement_in_ambient_context(node);
        self.check_expression(node.expression().unwrap());
    }

    // Returns the type of an expression. Unlike checkExpression, this function is simply concerned
    // with computing the type and may not fully check all contained sub-expressions for errors.
    // checker.go:7509
    pub(crate) fn get_type_of_expression(&mut self, node: P<Node>) -> P<Type> {
        // Don't bother caching types that require no flow analysis and are quick to compute.
        let quick_type = self.get_quick_type_of_expression(node);
        if let Some(quick_type) = quick_type {
            return quick_type;
        }
        // If a type has been cached for the node, return it.
        if let Some(cached_type) = self.flow_type_cache.as_ref().and_then(|m| m.get(&node).copied())
        {
            // A later flow walk reads this answer only while the same cache is current (flow memo).
            self.flow_memo.add_flags(crate::flowmemo::FLAG_TYPE_CACHE);
            return cached_type;
        }
        let start_invocation_count = self.flow_invocation_count;
        let t = self.check_expression_ex(node, CheckMode::TypeOnly);
        // If control flow analysis was required to determine the type, it is worth caching.
        if self.flow_invocation_count != start_invocation_count {
            let previous =
                self.flow_type_cache.get_or_insert_with(FxHashMap::default).insert(node, t);
            if previous.is_some() {
                // A nested evaluation of this node cached it first, so a cached type changed: memo answers that read
                // this cache no longer describe it.
                self.flow_memo.type_cache_epoch = self.flow_memo.next_serial();
            }
        }
        t
    }

    // Returns the type of an expression. Unlike checkExpression, this function is simply concerned
    // with computing the type and may not fully check all contained sub-expressions for errors.
    // checker.go:7533
    pub(crate) fn get_quick_type_of_expression(&mut self, node: P<Node>) -> Option<P<Type>> {
        let expr = skip_parentheses(node);
        if is_await_expression(expr) {
            let t = self.get_quick_type_of_expression(expr.expression().unwrap());
            if let Some(t) = t {
                return self.get_awaited_type(t);
            }
            return None;
        }
        // Optimize for the common case of a call to a function with a single non-generic call
        // signature where we can just fetch the return type without checking the arguments.
        if is_call_expression(expr)
            && expr.expression().unwrap().kind() != Kind::SuperKeyword
            && !is_require_call(expr, true /*requireStringLiteralLikeArgument*/)
            && !self.is_symbol_or_symbol_for_call(expr)
            && !is_import_call(expr)
        {
            if is_call_chain(expr) {
                return self.get_return_type_of_single_non_generic_signature_of_call_chain(expr);
            }
            let func_type = self.check_non_null_expression(expr.expression().unwrap());
            return self
                .get_return_type_of_single_non_generic_signature(func_type, SignatureKind::Call);
        }
        if is_new_expression(expr) {
            let func_type = self.check_non_null_expression(expr.expression().unwrap());
            return self.get_return_type_of_single_non_generic_signature(
                func_type,
                SignatureKind::Construct,
            );
        }
        if is_assertion_expression(expr)
            && !crate::is_const_type_reference(expr.type_node().unwrap())
        {
            return Some(self.get_type_from_type_node(expr.type_node().unwrap()));
        }
        if is_literal_expression(node) || is_boolean_literal(node) {
            return Some(self.check_expression(node));
        }
        None
    }

    // checker.go:7559
    fn get_return_type_of_single_non_generic_signature(
        &mut self,
        func_type: P<Type>,
        kind: SignatureKind,
    ) -> Option<P<Type>> {
        let signature = self.get_single_signature(func_type, kind, true /*allowMembers*/);
        if let Some(signature) = signature
            && signature.type_parameters().is_empty()
        {
            return Some(self.get_return_type_of_signature(signature));
        }
        None
    }

    // checker.go:7567
    fn get_return_type_of_single_non_generic_signature_of_call_chain(
        &mut self,
        expr: P<Node>,
    ) -> Option<P<Type>> {
        let func_type = self.check_expression(expr.expression().unwrap());
        let non_optional_type =
            self.get_optional_expression_type(func_type, expr.expression().unwrap());
        let return_type =
            self.get_return_type_of_single_non_generic_signature(func_type, SignatureKind::Call);
        if let Some(return_type) = return_type {
            return Some(self.propagate_optional_type_marker(
                return_type,
                expr,
                non_optional_type != func_type,
            ));
        }
        None
    }

    // checker.go:7577
    pub(crate) fn check_non_null_expression(&mut self, node: P<Node>) -> P<Type> {
        let t = self.check_expression(node);
        self.check_non_null_type(t, node)
    }

    // checker.go:7581
    pub(crate) fn check_non_null_type(&mut self, t: P<Type>, node: P<Node>) -> P<Type> {
        self.check_non_null_type_with_reporter(
            t,
            node,
            super::checker::Checker::report_object_possibly_null_or_undefined_error,
        )
    }

    // checker.go:7585
    fn check_non_null_type_with_reporter(
        &mut self,
        t: P<Type>,
        node: P<Node>,
        mut report_error: impl FnMut(&mut Checker, P<Node>, TypeFacts),
    ) -> P<Type> {
        if self.strict_null_checks && t.flags().intersects(TypeFlags::Unknown) {
            if is_entity_name_expression(node) {
                let node_text = crate::entity_name_to_string(node);
                if node_text.len() < 100 {
                    self.error(Some(node), &diagnostics::X_0_IS_OF_TYPE_UNKNOWN, &[&node_text]);
                    return self.error_type;
                }
            }
            self.error(Some(node), &diagnostics::OBJECT_IS_OF_TYPE_UNKNOWN, &[]);
            return self.error_type;
        }
        let facts = self.get_type_facts(t, TypeFacts::IsUndefinedOrNull);
        if facts.intersects(TypeFacts::IsUndefinedOrNull) {
            report_error(self, node, facts);
            let non_nullable = self.get_non_nullable_type(t);
            if non_nullable.flags().intersects(TypeFlags::Nullable | TypeFlags::Never) {
                return self.error_type;
            }
            return non_nullable;
        }
        t
    }

    // checker.go:7609
    pub(crate) fn check_non_null_non_void_type(&mut self, t: P<Type>, node: P<Node>) -> P<Type> {
        let non_null_type = self.check_non_null_type(t, node);
        if non_null_type.flags().intersects(TypeFlags::Void) {
            if is_entity_name_expression(node) {
                let node_text = crate::entity_name_to_string(node);
                if is_identifier(node) && node_text == "undefined" {
                    self.error(
                        Some(node),
                        &diagnostics::THE_VALUE_0_CANNOT_BE_USED_HERE,
                        &[&node_text],
                    );
                    return non_null_type;
                }
                if node_text.len() < 100 {
                    self.error(Some(node), &diagnostics::X_0_IS_POSSIBLY_UNDEFINED, &[&node_text]);
                    return non_null_type;
                }
            }
            self.error(Some(node), &diagnostics::OBJECT_IS_POSSIBLY_UNDEFINED, &[]);
        }
        non_null_type
    }

    // checker.go:7628
    fn report_object_possibly_null_or_undefined_error(&mut self, node: P<Node>, facts: TypeFacts) {
        let mut node_text = String::new();
        if is_entity_name_expression(node) {
            node_text = crate::entity_name_to_string(node);
        }
        if node.kind() == Kind::NullKeyword {
            self.error(Some(node), &diagnostics::THE_VALUE_0_CANNOT_BE_USED_HERE, &[&"null"]);
            return;
        }
        if !node_text.is_empty() && node_text.len() < 100 {
            if is_identifier(node) && node_text == "undefined" {
                self.error(
                    Some(node),
                    &diagnostics::THE_VALUE_0_CANNOT_BE_USED_HERE,
                    &[&"undefined"],
                );
                return;
            }
            let message: &'static Message = if facts.intersects(TypeFacts::IsUndefined) {
                if facts.intersects(TypeFacts::IsNull) {
                    &diagnostics::X_0_IS_POSSIBLY_NULL_OR_UNDEFINED
                } else {
                    &diagnostics::X_0_IS_POSSIBLY_UNDEFINED
                }
            } else {
                &diagnostics::X_0_IS_POSSIBLY_NULL
            };
            self.error(Some(node), message, &[&node_text]);
        } else {
            let message: &'static Message = if facts.intersects(TypeFacts::IsUndefined) {
                if facts.intersects(TypeFacts::IsNull) {
                    &diagnostics::OBJECT_IS_POSSIBLY_NULL_OR_UNDEFINED
                } else {
                    &diagnostics::OBJECT_IS_POSSIBLY_UNDEFINED
                }
            } else {
                &diagnostics::OBJECT_IS_POSSIBLY_NULL
            };
            self.error(Some(node), message, &[]);
        }
    }

    // checker.go:7656
    #[cfg_attr(
        not(feature = "work-census"),
        inline(always),
        expect(
            clippy::inline_always,
            reason = "without the census the wrapper is a forwarding call; inlined, callers call the body as before (notes/perf-checker-algorithms.md)"
        )
    )]
    pub(crate) fn check_expression_with_contextual_type(
        &mut self,
        node: P<Node>,
        contextual_type: P<Type>,
        inference_context: Option<P<InferenceContext>>,
        check_mode: CheckMode,
    ) -> P<Type> {
        if self.census_on() {
            let callee = self.census.as_ref().unwrap().call_stack.last().copied().flatten();
            let span = self.census_begin(crate::workcensus::Cat::CtxCheck, || {
                crate::workcensus::CKey::OptNode(callee)
            });
            let r = self.check_expression_with_contextual_type_worker(
                node,
                contextual_type,
                inference_context,
                check_mode,
            );
            let timing = self.census_end(span).unwrap();
            self.census.as_mut().unwrap().record(
                crate::workcensus::Cat::CtxCheck,
                crate::workcensus::CKey::OptNode(callee),
                timing,
                inference_context.is_some() as u64,
                0,
                0,
            );
            return r;
        }
        self.check_expression_with_contextual_type_worker(
            node,
            contextual_type,
            inference_context,
            check_mode,
        )
    }

    fn check_expression_with_contextual_type_worker(
        &mut self,
        node: P<Node>,
        contextual_type: P<Type>,
        inference_context: Option<P<InferenceContext>>,
        check_mode: CheckMode,
    ) -> P<Type> {
        let context_node = Self::get_context_node(node).unwrap();
        self.push_contextual_type(context_node, Some(contextual_type), false /*isCache*/);
        self.push_inference_context(context_node, inference_context);
        let mut t = self.check_expression_ex(
            node,
            check_mode
                | CheckMode::Contextual
                | if inference_context.is_some() {
                    CheckMode::Inferential
                } else {
                    CheckMode::Normal
                },
        );
        // In CheckMode.Inferential we collect intra-expression inference sites to process before fixing any type
        // parameters. This information is no longer needed after the call to checkExpression.
        if let Some(inference_context) = inference_context
            && inference_context.has_intra_expression_inference_sites()
        {
            inference_context.clear_intra_expression_inference_sites();
        }
        // We strip literal freshness when an appropriate contextual type is present such that contextually typed
        // literals always preserve their literal types (otherwise they might widen during type inference). An alternative
        // here would be to not mark contextually typed literals as fresh in the first place.
        if self.maybe_type_of_kind(t, TypeFlags::Literal) && {
            let instantiated =
                self.instantiate_contextual_type(Some(contextual_type), node, ContextFlags::None);
            self.is_literal_of_contextual_type(t, instantiated)
        } {
            t = self.get_regular_type_of_literal_type(t);
        }
        self.pop_inference_context();
        self.pop_contextual_type();
        t
    }

    // checker.go:7677
    fn get_context_node(node: P<Node>) -> Option<P<Node>> {
        if is_jsx_attributes(node) && !is_jsx_self_closing_element(node.parent().unwrap()) {
            // Needs to be the root JsxElement, so it encompasses the attributes _and_ the children (which are essentially part of the attributes)
            return node.parent().unwrap().parent();
        }
        Some(node)
    }

    // checker.go:7685
    pub(crate) fn check_expression_cached(&mut self, node: P<Node>) -> P<Type> {
        self.check_expression_cached_ex(node, CheckMode::Normal)
    }

    // checker.go:7689
    pub(crate) fn check_expression_cached_ex(
        &mut self,
        node: P<Node>,
        check_mode: CheckMode,
    ) -> P<Type> {
        if check_mode != CheckMode::Normal {
            return self.check_expression_ex(node, check_mode);
        }
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            // When computing a type that we're going to cache, we need to ignore any ongoing control flow
            // analysis because variables may have transient types in indeterminable states. Moving flowLoopStart
            // to the top of the stack ensures all transient types are computed from a known point.
            let save_flow_loop_stack = std::mem::take(&mut self.flow_loop_stack);
            let save_flow_type_cache = self.take_flow_type_cache();
            let t = self.check_expression_ex(node, check_mode);
            links.resolved_type.set(Some(t));
            self.restore_flow_type_cache(save_flow_type_cache);
            self.flow_loop_stack = save_flow_loop_stack;
        }
        links.resolved_type.get().unwrap()
    }

    // Returns the type of an expression. Unlike checkExpression, this function is simply concerned
    // with computing the type and may not fully check all contained sub-expressions for errors.
    // It is intended for uses where you know there is no contextual type,
    // and requesting the contextual type might cause a circularity or other bad behaviour.
    // It sets the contextual type of the node to any before calling getTypeOfExpression.
    // checker.go:7714
    pub(crate) fn get_context_free_type_of_expression(&mut self, node: P<Node>) -> P<Type> {
        if let Some(&cached) = self.context_free_types.get(&node) {
            return cached;
        }
        let any_type = self.any_type;
        self.push_contextual_type(node, Some(any_type), false /*isCache*/);
        let t = self.check_expression_ex(node, CheckMode::SkipContextSensitive);
        self.context_free_types.insert(node, t);
        self.pop_contextual_type();
        t
    }

    // checker.go:7725
    pub(crate) fn check_expression(&mut self, node: P<Node>) -> P<Type> {
        self.check_expression_ex(node, CheckMode::Normal)
    }

    // checker.go:7729
    pub(crate) fn check_expression_ex(&mut self, node: P<Node>, check_mode: CheckMode) -> P<Type> {
        let save_current_node = self.current_node;
        self.current_node = Some(node);
        self.instantiation_count = 0;
        self.expression_checks = self.expression_checks.wrapping_add(1);
        let uninstantiated_type = self.check_expression_worker(node, check_mode);
        let t = self.instantiate_type_with_single_generic_call_signature(
            node,
            uninstantiated_type,
            check_mode,
        );
        if is_const_enum_object_type(t) {
            self.check_const_enum_access(node, t);
        }
        self.current_node = save_current_node;
        self.end_instantiation_count();
        t
    }

    /// tsrs-only. Discard a nested check's count so a declaration resolved on demand cannot exhaust its caller's
    /// budget. Do not restore the caller's earlier count: Go discards it on entry to the nested check, and restoring
    /// it can add a TS2589 across call arguments (notes/open-history-dependence.md section 5).
    /// Go-compatible history leaves the nested check's count in place, as Go does.
    #[inline]
    pub(crate) fn end_instantiation_count(&mut self) {
        if !tsrs_core::compat::go_compatible_history() {
            self.instantiation_count = 0;
        }
    }

    // checker.go:7745
    fn check_const_enum_access(&mut self, node: P<Node>, t: P<Type>) {
        // enum object type for const enums are only permitted in:
        // - 'left' in property access
        // - 'object' in indexed access
        // - target in rhs of import statement
        let parent = node.parent().unwrap();
        let ok = is_property_access_expression(parent) && parent.expression() == Some(node)
            || is_element_access_expression(parent) && parent.expression() == Some(node)
            || ((is_identifier(node) || is_qualified_name(node))
                && crate::is_in_right_side_of_import_or_export_assignment(node)
                || is_type_query_node(parent) && parent.as_type_query_node().expr_name == node)
            || is_export_specifier(parent); // We allow reexporting const enums
        if !ok {
            self.error(
                Some(node),
                &diagnostics::X_CONST_ENUMS_CAN_ONLY_BE_USED_IN_PROPERTY_OR_INDEX_ACCESS_EXPRESSIONS_OR_THE_RIGHT_HAND_SIDE_OF_AN_IMPORT_DECLARATION_OR_EXPORT_ASSIGNMENT_OR_TYPE_QUERY,
                &[],
            );
        }
        // --verbatimModuleSyntax only gets checked here when the enum usage does not
        // resolve to an import, because imports of ambient const enums get checked
        // separately in `checkAliasSymbol`.
        if self.compiler_options.isolated_modules.is_true()
            || self.compiler_options.verbatim_module_syntax.is_true()
                && ok
                && self
                    .resolve_name(
                        Some(node),
                        get_first_identifier(node).text(),
                        SymbolFlags::Alias,
                        None,
                        false,
                        true,
                    )
                    .is_none()
        {
            let symbol = t.symbol().unwrap();
            assert!(symbol.flags().intersects(SymbolFlags::ConstEnum));
            let const_enum_declaration = symbol.value_declaration().unwrap();
            let redirect = self.program.get_project_reference_from_output_dts(
                get_source_file_of_node(const_enum_declaration).unwrap().path(),
            );
            if const_enum_declaration.flags().intersects(NodeFlags::Ambient)
                && !is_valid_type_only_alias_use_site(node)
                && (redirect.is_none()
                    || !redirect
                        .unwrap()
                        .resolved
                        .compiler_options()
                        .unwrap()
                        .should_preserve_const_enums())
            {
                let flag_name = self.get_isolated_modules_like_flag_name();
                self.error(
                    Some(node),
                    &diagnostics::CANNOT_ACCESS_AMBIENT_CONST_ENUMS_WHEN_0_IS_ENABLED,
                    &[&flag_name],
                );
            }
        }
    }

    // checker.go:7771
    pub(crate) fn instantiate_type_with_single_generic_call_signature(
        &mut self,
        node: P<Node>,
        t: P<Type>,
        check_mode: CheckMode,
    ) -> P<Type> {
        if !check_mode.intersects(CheckMode::Inferential | CheckMode::SkipGenericFunctions) {
            return t;
        }
        let call_signature =
            self.get_single_signature(t, SignatureKind::Call, true /*allowMembers*/);
        let construct_signature =
            self.get_single_signature(t, SignatureKind::Construct, true /*allowMembers*/);
        let signature = call_signature.or(construct_signature);
        let Some(signature) = signature else {
            return t;
        };
        if signature.type_parameters().is_empty() {
            return t;
        }
        let Some(contextual_type) =
            self.get_apparent_type_of_contextual_type(node, ContextFlags::NoConstraints)
        else {
            return t;
        };
        let non_nullable = self.get_non_nullable_type(contextual_type);
        let contextual_signature = self.get_single_signature(
            non_nullable,
            if call_signature.is_some() { SignatureKind::Call } else { SignatureKind::Construct },
            false, /*allowMembers*/
        );
        let Some(contextual_signature) = contextual_signature else {
            return t;
        };
        if !contextual_signature.type_parameters().is_empty() {
            return t;
        }
        if check_mode.intersects(CheckMode::SkipGenericFunctions) {
            self.skipped_generic_function(node, check_mode);
            return self.any_function_type;
        }
        let context = self.get_inference_context(node).unwrap();
        // We have an expression that is an argument of a generic function for which we are performing
        // type argument inference. The expression is of a function type with a single generic call
        // signature and a contextual function type with a single non-generic call signature. Now check
        // if the outer function returns a function type with a single non-generic call signature and
        // if some of the outer function type parameters have no inferences so far. If so, we can
        // potentially add inferred type parameters to the outer function return type.
        let mut return_signature: Option<P<Signature>> = None;
        if let Some(context_signature) = context.signature.get() {
            let return_type = self.get_return_type_of_signature(context_signature);
            return_signature = self.get_single_call_or_construct_signature(return_type);
        }
        if let Some(return_signature) = return_signature
            && return_signature.type_parameters().is_empty()
            && !context.inferences.get().iter().all(|&info| has_inference_candidates(info))
        {
            // Instantiate the signature with its own type parameters as type arguments, possibly
            // renaming the type parameters to ensure they have unique names.
            let unique_type_parameters =
                self.get_unique_type_parameters(context, signature.type_parameters());
            let instantiated_signature = self
                .get_signature_instantiation_without_filling_in_type_arguments(
                    signature,
                    &unique_type_parameters,
                );
            // Infer from the parameters of the instantiated signature to the parameters of the
            // contextual signature starting with an empty set of inference candidates.
            let inferences: Vec<P<InferenceInfo>> = context
                .inferences
                .get()
                .iter()
                .map(|info| new_inference_info(info.type_parameter.get().unwrap()))
                .collect();
            self.apply_to_parameter_types(
                instantiated_signature,
                contextual_signature,
                |c, source, target| {
                    c.infer_types(
                        &inferences,
                        source,
                        target,
                        InferencePriority::NONE,
                        true, /*contravariant*/
                    );
                },
            );
            if inferences.iter().any(|&info| has_inference_candidates(info)) {
                // We have inference candidates, indicating that one or more type parameters are referenced
                // in the parameter types of the contextual signature. Now also infer from the return type.
                self.apply_to_return_types(
                    instantiated_signature,
                    contextual_signature,
                    |c, source, target| {
                        c.infer_types(&inferences, source, target, InferencePriority::NONE, false);
                    },
                );
                // If the type parameters for which we produced candidates do not have any inferences yet,
                // we adopt the new inference candidates and add the type parameters of the expression type
                // to the set of inferred type parameters for the outer function return type.
                if !has_overlapping_inferences(context.inferences.get(), &inferences) {
                    // Go merges into context.inferences in place; the slice is immutable here, so merge a copy and
                    // store it back.
                    let mut merged = context.inferences.get().to_vec();
                    Self::merge_inferences(&mut merged, &inferences);
                    context.inferences.set(alloc_vec(merged));
                    let mut inferred_type_parameters = context.inferred_type_parameters().to_vec();
                    inferred_type_parameters.extend_from_slice(&unique_type_parameters);
                    context.set_inferred_type_parameters(alloc_vec(inferred_type_parameters));
                    return self.get_or_create_type_from_signature(instantiated_signature);
                }
            }
        }
        // TODO: The signature may reference any outer inference contexts, but we map pop off and then apply new inference contexts,
        // and thus get different inferred types. That this is cached on the *first* such attempt is not currently an issue, since expression
        // types *also* get cached on the first pass. If we ever properly speculate, though, the cached "isolatedSignatureType" signature
        // field absolutely needs to be included in the list of speculative caches.
        let instantiated = self.instantiate_signature_in_context_of(
            signature,
            contextual_signature,
            Some(context),
            None,
        );
        self.get_or_create_type_from_signature(instantiated)
    }

    // checker.go:7856
    fn get_unique_type_parameters(
        &mut self,
        context: P<InferenceContext>,
        type_parameters: &[P<Type>],
    ) -> Vec<P<Type>> {
        let mut old_type_parameters: Vec<P<Type>> = Vec::new();
        let mut new_type_parameters: Vec<P<Type>> = Vec::new();
        let mut result: Vec<P<Type>> = Vec::with_capacity(type_parameters.len());
        for &tp in type_parameters {
            let name = tp.symbol().unwrap().name();
            if has_type_parameter_by_name(context.inferred_type_parameters(), name)
                || has_type_parameter_by_name(&result, name)
            {
                let mut all: Vec<P<Type>> = context.inferred_type_parameters().to_vec();
                all.extend_from_slice(&result);
                let new_name = get_unique_type_parameter_name(&all, name);
                let symbol = self.new_symbol(SymbolFlags::TypeParameter, alloc_str(&new_name));
                let new_type_parameter = self.new_type_parameter(Some(symbol));
                new_type_parameter.as_type_parameter().target.set(Some(tp));
                old_type_parameters.push(tp);
                new_type_parameters.push(new_type_parameter);
                result.push(new_type_parameter);
            } else {
                result.push(tp);
            }
        }
        if !new_type_parameters.is_empty() {
            let mapper =
                new_type_mapper(alloc_vec(old_type_parameters), alloc_slice(&new_type_parameters));
            for tp in &new_type_parameters {
                tp.as_type_parameter().mapper.set(Some(mapper));
            }
        }
        result
    }
}

// checker.go:7883
pub fn has_type_parameter_by_name(type_parameters: &[P<Type>], name: &str) -> bool {
    type_parameters.iter().any(|tp| tp.symbol().unwrap().name() == name)
}

// checker.go:7889
pub fn get_unique_type_parameter_name(type_parameters: &[P<Type>], base_name: &str) -> String {
    let mut base_name = base_name;
    while base_name.len() > 1 && base_name.as_bytes()[base_name.len() - 1].is_ascii_digit() {
        base_name = &base_name[..base_name.len() - 1];
    }
    let mut index = 1;
    loop {
        let augmented_name = format!("{base_name}{index}");
        if !has_type_parameter_by_name(type_parameters, &augmented_name) {
            return augmented_name;
        }
        index += 1;
    }
}

impl Checker {
    // checker.go:7903
    fn check_expression_worker(&mut self, node: P<Node>, check_mode: CheckMode) -> P<Type> {
        match node.kind() {
            Kind::Identifier => self.check_identifier(node, check_mode),
            Kind::PrivateIdentifier => self.check_private_identifier_expression(node),
            Kind::ThisKeyword => self.check_this_expression(node),
            Kind::SuperKeyword => self.check_super_expression(node),
            Kind::NullKeyword => self.null_widening_type,
            Kind::StringLiteral | Kind::NoSubstitutionTemplateLiteral => {
                if self.is_skip_direct_inference_node(node) {
                    return self.blocked_string_type;
                }
                let t = self.get_string_literal_type(node.text());
                self.get_fresh_type_of_literal_type(t)
            }
            Kind::NumericLiteral => {
                self.check_grammar_numeric_literal(node);
                let t = self.get_number_literal_type(jsnum::from_string(node.text()));
                self.get_fresh_type_of_literal_type(t)
            }
            Kind::BigIntLiteral => {
                self.check_grammar_big_int_literal(node);
                let t = self.get_big_int_literal_type(jsnum::new_pseudo_big_int(
                    &jsnum::parse_pseudo_big_int(node.text()),
                    false, /*negative*/
                ));
                self.get_fresh_type_of_literal_type(t)
            }
            Kind::TrueKeyword => self.true_type,
            Kind::FalseKeyword => self.false_type,
            Kind::TemplateExpression => self.check_template_expression(node),
            Kind::RegularExpressionLiteral => self.check_regular_expression_literal(node),
            Kind::ArrayLiteralExpression => self.check_array_literal(node, check_mode),
            Kind::ObjectLiteralExpression => self.check_object_literal(node, check_mode),
            Kind::PropertyAccessExpression => {
                self.check_property_access_expression(node, check_mode, false /*writeOnly*/)
            }
            Kind::QualifiedName => self.check_qualified_name(node, check_mode),
            Kind::ElementAccessExpression => self.check_indexed_access(node, check_mode),
            Kind::CallExpression => {
                if is_import_call(node) {
                    return self.check_import_call_expression(node);
                }
                self.check_call_expression(node, check_mode)
            }
            Kind::NewExpression => self.check_call_expression(node, check_mode),
            Kind::TaggedTemplateExpression => self.check_tagged_template_expression(node),
            Kind::ParenthesizedExpression => self.check_parenthesized_expression(node, check_mode),
            Kind::ClassExpression => self.check_class_expression(node),
            Kind::FunctionExpression | Kind::ArrowFunction => {
                self.check_function_expression_or_object_literal_method(node, check_mode)
            }
            Kind::TypeAssertionExpression | Kind::AsExpression => {
                self.check_assertion(node, check_mode)
            }
            Kind::TypeOfExpression => self.check_type_of_expression(node),
            Kind::NonNullExpression => self.check_non_null_assertion(node),
            Kind::ExpressionWithTypeArguments => self.check_expression_with_type_arguments(node),
            Kind::SatisfiesExpression => self.check_satisfies_expression(node),
            Kind::MetaProperty => self.check_meta_property(node),
            Kind::DeleteExpression => self.check_delete_expression(node),
            Kind::VoidExpression => self.check_void_expression(node),
            Kind::AwaitExpression => self.check_await_expression(node),
            Kind::PrefixUnaryExpression => self.check_prefix_unary_expression(node),
            Kind::PostfixUnaryExpression => self.check_postfix_unary_expression(node),
            Kind::BinaryExpression => self.check_binary_expression(node, check_mode),
            Kind::ConditionalExpression => self.check_conditional_expression(node, check_mode),
            Kind::SpreadElement => self.check_spread_expression(node, check_mode),
            Kind::OmittedExpression => self.undefined_widening_type,
            Kind::YieldExpression => self.check_yield_expression(node),
            Kind::SyntheticExpression => self.check_synthetic_expression(node),
            Kind::JsxExpression => self.check_jsx_expression(node, check_mode),
            Kind::JsxElement => self.check_jsx_element(node, check_mode),
            Kind::JsxSelfClosingElement => self.check_jsx_self_closing_element(node, check_mode),
            Kind::JsxFragment => self.check_jsx_fragment(node),
            Kind::JsxAttributes => self.check_jsx_attributes(node, check_mode),
            Kind::JsxOpeningElement => panic!("Should never directly check a JsxOpeningElement"),
            _ => self.error_type,
        }
    }

    // checker.go:8009
    fn check_private_identifier_expression(&mut self, node: P<Node>) -> P<Type> {
        self.check_grammar_private_identifier_expression(node);
        let symbol = self.get_symbol_for_private_identifier_expression(node);
        if let Some(symbol) = symbol {
            self.mark_property_as_referenced(
                symbol, None,  /*nodeForCheckWriteOnly*/
                false, /*isSelfTypeAccess*/
            );
        }
        self.any_type
    }

    // checker.go:8018
    pub(crate) fn get_symbol_for_private_identifier_expression(
        &mut self,
        node: P<Node>,
    ) -> Option<P<Symbol>> {
        let links = self.symbol_node_links.get(node);
        if links.resolved_symbol.get().is_none() {
            let symbol = Self::lookup_symbol_for_private_identifier_declaration(node.text(), node);
            links.resolved_symbol.set(symbol);
        }
        links.resolved_symbol.get()
    }

    // checker.go:8026
    pub(crate) fn check_super_expression(&mut self, node: P<Node>) -> P<Type> {
        let is_call_expression = is_call_expression(node.parent().unwrap())
            && node.parent().unwrap().expression() == Some(node);
        let immediate_container =
            crate::utilities::get_super_container(node, true /*stopOnFunctions*/);
        let mut container = immediate_container;

        // adjust the container reference in case if super is used inside arrow functions with arbitrarily deep nesting
        if !is_call_expression {
            while let Some(c) = container {
                if !is_arrow_function(c) {
                    break;
                }
                container = crate::utilities::get_super_container(c, true /*stopOnFunctions*/);
            }
        }

        let is_legal_usage_of_super_expression = |container: P<Node>| -> bool {
            if is_call_expression {
                // TS 1.0 SPEC (April 2014): 4.8.1
                // Super calls are only permitted in constructors of derived classes
                return is_constructor_declaration(container);
            }
            // TS 1.0 SPEC (April 2014)
            // 'super' property access is allowed
            // - In a constructor, instance member function, instance member accessor, or instance member variable initializer where this references a derived class instance
            // - In a static member function or static member accessor

            // topmost container must be something that is directly nested in the class declaration\object literal expression
            if is_class_like(container.parent().unwrap())
                || is_object_literal_expression(container.parent().unwrap())
            {
                if is_static(container) {
                    return node_kind_is(
                        container,
                        &[
                            Kind::MethodDeclaration,
                            Kind::MethodSignature,
                            Kind::GetAccessor,
                            Kind::SetAccessor,
                            Kind::PropertyDeclaration,
                            Kind::ClassStaticBlockDeclaration,
                        ],
                    );
                }
                return node_kind_is(
                    container,
                    &[
                        Kind::MethodDeclaration,
                        Kind::MethodSignature,
                        Kind::GetAccessor,
                        Kind::SetAccessor,
                        Kind::PropertyDeclaration,
                        Kind::PropertySignature,
                        Kind::Constructor,
                    ],
                );
            }
            false
        };

        if container.is_none() || !is_legal_usage_of_super_expression(container.unwrap()) {
            // issue more specific error if super is used in computed property name
            // class A { foo() { return "1" }}
            // class B {
            //     [super.foo()]() {}
            // }
            let current = find_ancestor_or_quit(node, |n| {
                if Some(n) == container {
                    return FindAncestorResult::Quit;
                }
                if is_computed_property_name(n) {
                    return FindAncestorResult::True;
                }
                FindAncestorResult::False
            });
            if current.is_some_and(is_computed_property_name) {
                self.error(
                    Some(node),
                    &diagnostics::X_SUPER_CANNOT_BE_REFERENCED_IN_A_COMPUTED_PROPERTY_NAME,
                    &[],
                );
            } else if is_call_expression {
                self.error(Some(node), &diagnostics::SUPER_CALLS_ARE_NOT_PERMITTED_OUTSIDE_CONSTRUCTORS_OR_IN_NESTED_FUNCTIONS_INSIDE_CONSTRUCTORS, &[]);
            } else if container.is_none()
                || container.unwrap().parent().is_none()
                || !(is_class_like(container.unwrap().parent().unwrap())
                    || is_object_literal_expression(container.unwrap().parent().unwrap()))
            {
                self.error(Some(node), &diagnostics::X_SUPER_CAN_ONLY_BE_REFERENCED_IN_MEMBERS_OF_DERIVED_CLASSES_OR_OBJECT_LITERAL_EXPRESSIONS, &[]);
            } else {
                self.error(
                    Some(node),
                    &diagnostics::X_SUPER_PROPERTY_ACCESS_IS_PERMITTED_ONLY_IN_A_CONSTRUCTOR_MEMBER_FUNCTION_OR_MEMBER_ACCESSOR_OF_A_DERIVED_CLASS,
                    &[],
                );
            }
            return self.error_type;
        }
        let container = container.unwrap();
        if !is_call_expression && immediate_container.is_some_and(is_constructor_declaration) {
            self.check_this_before_super(
                node,
                container,
                &diagnostics::X_SUPER_MUST_BE_CALLED_BEFORE_ACCESSING_A_PROPERTY_OF_SUPER_IN_THE_CONSTRUCTOR_OF_A_DERIVED_CLASS,
            );
        }
        if container.parent().unwrap().kind() == Kind::ObjectLiteralExpression {
            // for object literal assume that type of 'super' is 'any'
            return self.any_type;
        }
        // at this point the only legal case for parent is ClassLikeDeclaration
        let class_like_declaration = container.parent().unwrap();
        if get_class_extends_heritage_element(class_like_declaration).is_none() {
            self.error(
                Some(node),
                &diagnostics::X_SUPER_CAN_ONLY_BE_REFERENCED_IN_A_DERIVED_CLASS,
                &[],
            );
            return self.error_type;
        }
        if self.class_declaration_extends_null(class_like_declaration) {
            if is_call_expression {
                return self.error_type;
            }
            return self.null_widening_type;
        }
        let class_symbol = self.get_symbol_of_declaration(class_like_declaration).unwrap();
        let class_type = self.get_declared_type_of_symbol(class_symbol);
        let base_class_type = first_or_nil(self.get_base_types(class_type));
        let Some(base_class_type) = base_class_type else {
            return self.error_type;
        };
        if is_constructor_declaration(container)
            && Self::is_in_constructor_argument_initializer(node, container)
        {
            // issue custom error message for super property access in constructor arguments (to be aligned with old compiler)
            self.error(
                Some(node),
                &diagnostics::X_SUPER_CANNOT_BE_REFERENCED_IN_CONSTRUCTOR_ARGUMENTS,
                &[],
            );
            return self.error_type;
        }
        if is_static(container) || is_call_expression {
            if !is_call_expression
                && self.language_version <= ScriptTarget::ES2021
                && (is_property_declaration(container)
                    || is_class_static_block_declaration(container))
            {
                // for `super.x` or `super[x]` in a static initializer, mark all enclosing
                // block scope containers so that we can report potential collisions with
                // `Reflect`.
                let mut current = get_enclosing_block_scope_container(node.parent().unwrap());
                while let Some(cur) = current {
                    if !is_source_file(cur)
                        || is_external_or_common_js_module(cur.as_source_file_p())
                    {
                        let links = self.node_links.get(cur);
                        links.flags.set(
                            links.flags.get()
                                | NodeCheckFlags::ContainsSuperPropertyInStaticInitializer,
                        );
                    }
                    current = get_enclosing_block_scope_container(cur);
                }
            }
            return self.get_base_constructor_type_of_class(class_type);
        }
        let this_type = class_type.as_interface_type().this_type.get();
        self.get_type_with_this_argument(base_class_type, this_type, false)
    }

    // checker.go:8136
    fn is_in_constructor_argument_initializer(node: P<Node>, constructor_decl: P<Node>) -> bool {
        find_ancestor_or_quit(node, |n| {
            if is_function_like_declaration(n) {
                return FindAncestorResult::Quit;
            }
            if is_parameter_declaration(n) && n.parent() == Some(constructor_decl) {
                return FindAncestorResult::True;
            }
            FindAncestorResult::False
        })
        .is_some()
    }

    // checker.go:8148
    fn check_template_expression(&mut self, node: P<Node>) -> P<Type> {
        let expr = node.as_template_expression();
        let spans = expr.template_spans.nodes();
        let length = spans.len();
        let mut texts: Vec<&str> = vec![""; length + 1];
        let mut types: Vec<P<Type>> = Vec::with_capacity(length);
        texts[0] = expr.head.text();
        for (i, &span) in spans.iter().enumerate() {
            let t = self.check_expression(span.expression().unwrap());
            if self.maybe_type_of_kind_considering_base_constraint(t, TypeFlags::ESSymbolLike) {
                self.error(
                    span.expression(),
                    &diagnostics::IMPLICIT_CONVERSION_OF_A_SYMBOL_TO_A_STRING_WILL_FAIL_AT_RUNTIME_CONSIDER_WRAPPING_THIS_EXPRESSION_IN_STRING,
                    &[],
                );
            }
            texts[i + 1] = span.as_template_span().literal.text();
            let template_constraint_type = self.template_constraint_type;
            types.push(if self.is_type_assignable_to(t, template_constraint_type) {
                t
            } else {
                self.string_type
            });
        }
        let mut evaluated: Option<LiteralValue> = None;
        if !is_tagged_template_expression(node.parent().unwrap()) {
            evaluated = self.evaluate(node, node).value;
        }
        if let Some(evaluated) = evaluated {
            let LiteralValue::String(s) = evaluated else {
                panic!("evaluated template expression is not a string");
            };
            let t = self.get_string_literal_type(s);
            return self.get_fresh_type_of_literal_type(t);
        }
        if self.is_const_context(node) || Self::is_template_literal_context(node) || {
            let contextual_type =
                self.get_contextual_type(node, ContextFlags::None).unwrap_or(self.unknown_type);
            some_type(
                self,
                contextual_type,
                super::checker::Checker::is_template_literal_contextual_type,
            )
        } {
            return self.get_template_literal_type(&texts, &types);
        }
        self.string_type
    }

    // checker.go:8175
    fn is_template_literal_context(node: P<Node>) -> bool {
        let parent = node.parent().unwrap();
        is_parenthesized_expression(parent) && Self::is_template_literal_context(parent)
            || is_element_access_expression(parent)
                && parent.as_element_access_expression().argument_expression == node
    }

    // checker.go:8180
    fn is_template_literal_contextual_type(&mut self, t: P<Type>) -> bool {
        t.flags().intersects(TypeFlags::StringLiteral | TypeFlags::TemplateLiteral)
            || t.flags().intersects(TypeFlags::InstantiableNonPrimitive) && {
                let constraint = self.get_base_constraint_of_type(t).unwrap_or(self.unknown_type);
                self.maybe_type_of_kind(constraint, TypeFlags::StringLike)
            }
    }

    // checker.go:8184
    fn check_regular_expression_literal(&mut self, node: P<Node>) -> P<Type> {
        let node_links = self.node_links.get(node);
        if !node_links.flags.get().intersects(NodeCheckFlags::TypeChecked) {
            node_links.flags.set(node_links.flags.get() | NodeCheckFlags::TypeChecked);
            self.check_grammar_regular_expression_literal(node);
        }
        self.global_reg_exp_type
    }

    // checker.go:8193
    pub(crate) fn check_array_literal(&mut self, node: P<Node>, check_mode: CheckMode) -> P<Type> {
        let elements = node.elements();
        let mut element_types: Vec<P<Type>> = vec![self.error_type; elements.len()];
        let mut element_infos: Vec<TupleElementInfo> =
            vec![TupleElementInfo::default(); elements.len()];
        self.push_cached_contextual_type(node);
        let in_destructuring_pattern = is_assignment_target(node);
        let in_const_context = self.is_const_context(node);
        let contextual_type = self.get_apparent_type_of_contextual_type(node, ContextFlags::None);
        let in_tuple_context = is_spread_into_call_or_new(node)
            || contextual_type.is_some_and(|contextual_type| {
                some_type(self, contextual_type, |c, t| {
                    c.is_tuple_like_type(t)
                        || c.is_generic_mapped_type(t)
                            && t.as_mapped_type().name_type.get().is_none()
                            && {
                                let target = t.as_mapped_type().target.get().unwrap_or(t);
                                c.get_homomorphic_type_variable(target).is_some()
                            }
                })
            });
        let mut has_omitted_expression = false;
        for (i, &e) in elements.iter().enumerate() {
            if is_spread_element(e) {
                let spread_type = self.check_expression_ex(e.expression().unwrap(), check_mode);
                if self.is_array_like_type(spread_type) {
                    element_types[i] = spread_type;
                    element_infos[i] =
                        TupleElementInfo { flags: ElementFlags::Variadic, ..Default::default() };
                } else if in_destructuring_pattern {
                    // Given the following situation:
                    //    var c: {};
                    //    [...c] = ["", 0];
                    //
                    // c is represented in the tree as a spread element in an array literal.
                    // But c really functions as a rest element, and its purpose is to provide
                    // a contextual type for the right hand side of the assignment. Therefore,
                    // instead of calling checkExpression on "...c", which will give an error
                    // if c is not iterable/array-like, we need to act as if we are trying to
                    // get the contextual element type from it. So we do something similar to
                    // getContextualTypeForElementExpression, which will crucially not error
                    // if there is no index type / iterated type.
                    let number_type = self.number_type;
                    let mut rest_element_type =
                        self.get_index_type_of_type(spread_type, number_type);
                    if rest_element_type.is_none() {
                        let undefined_type = self.undefined_type;
                        rest_element_type = self.get_iterated_type_or_element_type(
                            IterationUse::Destructuring,
                            spread_type,
                            undefined_type,
                            None,  /*errorNode*/
                            false, /*checkAssignability*/
                        );
                        if rest_element_type.is_none() {
                            rest_element_type = Some(self.unknown_type);
                        }
                    }
                    element_types[i] = rest_element_type.unwrap();
                    element_infos[i] =
                        TupleElementInfo { flags: ElementFlags::Rest, ..Default::default() };
                } else {
                    let undefined_type = self.undefined_type;
                    element_types[i] = self.check_iterated_type_or_element_type(
                        IterationUse::Spread,
                        spread_type,
                        undefined_type,
                        e.expression(),
                    );
                    element_infos[i] =
                        TupleElementInfo { flags: ElementFlags::Rest, ..Default::default() };
                }
            } else if self.exact_optional_property_types && is_omitted_expression(e) {
                has_omitted_expression = true;
                element_types[i] = self.undefined_or_missing_type;
                element_infos[i] =
                    TupleElementInfo { flags: ElementFlags::Optional, ..Default::default() };
            } else {
                let t = self.check_expression_for_mutable_location(e, check_mode);
                element_types[i] =
                    self.add_optionality_ex(t, true /*isProperty*/, has_omitted_expression);
                element_infos[i] = TupleElementInfo {
                    flags: if has_omitted_expression {
                        ElementFlags::Optional
                    } else {
                        ElementFlags::Required
                    },
                    ..Default::default()
                };
                if in_tuple_context
                    && check_mode.intersects(CheckMode::Inferential)
                    && !check_mode.intersects(CheckMode::SkipContextSensitive)
                    && self.is_context_sensitive(e)
                {
                    let inference_context = self.get_inference_context(node);
                    // In CheckMode.Inferential we should always have an inference context
                    Self::add_intra_expression_inference_site(inference_context.unwrap(), e, t);
                }
            }
        }
        self.pop_contextual_type();
        if in_destructuring_pattern {
            return self.create_tuple_type_ex(&element_types, &element_infos, false);
        }
        if check_mode.intersects(CheckMode::ForceTuple) || in_const_context || in_tuple_context {
            let readonly = in_const_context
                && !contextual_type.is_some_and(|contextual_type| {
                    some_type(
                        self,
                        contextual_type,
                        super::checker::Checker::is_mutable_array_like_type,
                    )
                });
            let tuple = self.create_tuple_type_ex(
                &element_types,
                &element_infos,
                readonly, /*readonly*/
            );
            return self.create_array_literal_type(tuple);
        }

        let element_type: P<Type> = if !element_types.is_empty() {
            for (i, element) in element_types.iter_mut().enumerate() {
                let e = *element;
                if element_infos[i].flags.intersects(ElementFlags::Variadic) {
                    let number_type = self.number_type;
                    (*element) = self
                        .get_indexed_access_type_or_undefined(
                            e,
                            number_type,
                            AccessFlags::None,
                            None,
                            AliasArg::None,
                        )
                        .unwrap_or(self.any_type);
                }
            }
            self.get_union_type_ex(&element_types, UnionReduction::Subtype, AliasArg::None, None)
        } else if self.strict_null_checks {
            self.implicit_never_type
        } else {
            self.undefined_widening_type
        };
        let array_type = self.create_array_type_ex(element_type, in_const_context);
        self.create_array_literal_type(array_type)
    }

    // checker.go:8275
    fn create_array_literal_type(&mut self, t: P<Type>) -> P<Type> {
        if !t.object_flags().intersects(ObjectFlags::Reference) {
            return t;
        }
        let key = CachedTypeKey { kind: CachedTypeKind::ArrayLiteralType, type_id: t.id };
        if let Some(&cached) = self.cached_types.get(&key) {
            return cached;
        }
        let literal_type = self.clone_type_reference(t);
        literal_type.object_flags.set(
            literal_type.object_flags_lazy()
                | ObjectFlags::ArrayLiteral
                | ObjectFlags::ContainsObjectOrArrayLiteral,
        );
        self.cached_types.insert(key, literal_type);
        literal_type
    }
}

// checker.go:8289
pub fn is_spread_into_call_or_new(node: P<Node>) -> bool {
    let parent = walk_up_parenthesized_expressions(node.parent()).unwrap();
    is_spread_element(parent) && is_call_or_new_expression(parent.parent().unwrap())
}

impl Checker {
    // checker.go:8294
    pub(crate) fn check_qualified_name(&mut self, node: P<Node>, check_mode: CheckMode) -> P<Type> {
        let left = node.as_qualified_name().left;
        let left_type = if is_part_of_type_query(node) && is_this_identifier(left) {
            let t = self.check_this_expression(left);
            self.check_non_null_type(t, left)
        } else {
            self.check_non_null_expression(left)
        };
        self.check_property_access_expression_or_qualified_name(
            node,
            left,
            left_type,
            node.as_qualified_name().right,
            check_mode,
            false,
        )
    }

    // checker.go:8305
    fn check_indexed_access(&mut self, node: P<Node>, check_mode: CheckMode) -> P<Type> {
        if node.flags().intersects(NodeFlags::OptionalChain) {
            return self.check_element_access_chain(node, check_mode);
        }
        let expr_type = self.check_non_null_expression(node.expression().unwrap());
        self.check_element_access_expression(node, expr_type, check_mode)
    }

    // checker.go:8312
    fn check_element_access_chain(&mut self, node: P<Node>, check_mode: CheckMode) -> P<Type> {
        let expression = node.expression().unwrap();
        let expr_type = self.check_expression(expression);
        let non_optional_type = self.get_optional_expression_type(expr_type, expression);
        let non_null_type = self.check_non_null_type(non_optional_type, expression);
        let t = self.check_element_access_expression(node, non_null_type, check_mode);
        self.propagate_optional_type_marker(t, node, non_optional_type != expr_type)
    }

    // checker.go:8318
    fn check_element_access_expression(
        &mut self,
        node: P<Node>,
        expr_type: P<Type>,
        check_mode: CheckMode,
    ) -> P<Type> {
        let mut object_type = expr_type;
        if get_assignment_target_kind(node) != AssignmentKind::None
            || Self::is_method_access_for_call(node)
        {
            object_type = self.get_widened_type(object_type);
        }
        let index_expression = node.as_element_access_expression().argument_expression;
        let index_type = self.check_expression(index_expression);
        if self.is_error_type(object_type) || object_type == self.silent_never_type {
            return object_type;
        }
        if is_const_enum_object_type(object_type) && !is_string_literal_like(index_expression) {
            self.error(
                Some(index_expression),
                &diagnostics::A_CONST_ENUM_MEMBER_CAN_ONLY_BE_ACCESSED_USING_A_STRING_LITERAL,
                &[],
            );
            return self.error_type;
        }
        let mut effective_index_type = index_type;
        if self.is_for_in_variable_for_numeric_property_names(index_expression) {
            effective_index_type = self.number_type;
        }
        let assignment_target_kind = get_assignment_target_kind(node);
        let access_flags = if assignment_target_kind == AssignmentKind::None {
            AccessFlags::ExpressionPosition
        } else {
            AccessFlags::Writing
                | (if assignment_target_kind == AssignmentKind::Compound {
                    AccessFlags::ExpressionPosition
                } else {
                    AccessFlags::None
                })
                | (if self.is_generic_object_type(object_type)
                    && !is_this_type_parameter(object_type)
                {
                    AccessFlags::NoIndexSignatures
                } else {
                    AccessFlags::None
                })
        };
        let indexed_access_type = self
            .get_indexed_access_type_or_undefined(
                object_type,
                effective_index_type,
                access_flags,
                Some(node),
                AliasArg::None,
            )
            .unwrap_or(self.error_type);
        let resolved_symbol = self.get_resolved_symbol_or_nil(node);
        let flow_type = self.get_flow_type_of_access_expression(
            node,
            resolved_symbol,
            indexed_access_type,
            index_expression,
            check_mode,
        );
        self.check_indexed_access_index_type(flow_type, node)
    }

    // Return true if given node is an expression consisting of an identifier (possibly parenthesized)
    // that references a for-in variable for an object with numeric property names.
    // checker.go:8351
    fn is_for_in_variable_for_numeric_property_names(&mut self, expr: P<Node>) -> bool {
        let e = skip_parentheses(expr);
        if is_identifier(e) {
            let symbol = self.get_resolved_symbol(e);
            if symbol.flags().intersects(SymbolFlags::Variable) {
                let mut child = expr;
                let mut node = expr.parent();
                while let Some(n) = node {
                    if is_for_in_statement(n)
                        && child == n.as_for_in_or_of_statement().statement
                        && self.get_for_in_variable_symbol(n) == Some(symbol)
                        && {
                            let t = self.get_type_of_expression(n.expression().unwrap());
                            self.has_numeric_property_names(t)
                        }
                    {
                        return true;
                    }
                    child = n;
                    node = n.parent();
                }
            }
        }
        false
    }

    // Return the symbol of the for-in variable declared or referenced by the given for-in statement.
    // checker.go:8371
    fn get_for_in_variable_symbol(&mut self, node: P<Node>) -> Option<P<Symbol>> {
        let initializer = node.initializer().unwrap();
        if is_variable_declaration_list(initializer) {
            let declarations = initializer.as_variable_declaration_list().declarations.nodes();
            if !declarations.is_empty() {
                let variable = declarations[0];
                if !is_binding_pattern(variable.name().unwrap()) {
                    return self.get_symbol_of_declaration(variable);
                }
            }
        } else if is_identifier(initializer) {
            return Some(self.get_resolved_symbol(initializer));
        }
        None
    }

    // Return true if the given type is considered to have numeric property names.
    // checker.go:8388
    fn has_numeric_property_names(&mut self, t: P<Type>) -> bool {
        let number_type = self.number_type;
        self.get_index_infos_of_type(t).len() == 1
            && self.get_index_info_of_type(t, number_type).is_some()
    }

    // checker.go:8392
    pub(crate) fn check_indexed_access_index_type(
        &mut self,
        t: P<Type>,
        access_node: P<Node>,
    ) -> P<Type> {
        if !t.flags().intersects(TypeFlags::IndexedAccess) {
            return t;
        }
        // Check if the index type is assignable to 'keyof T' for the object type.
        let object_type = t.as_indexed_access_type().object_type.get().unwrap();
        let index_type = t.as_indexed_access_type().index_type.get().unwrap();
        let number_type = self.number_type;
        let has_number_index_info = self.get_index_info_of_type(object_type, number_type).is_some();
        if every_type(self, index_type, |c, t| {
            let object_index_type = c.get_index_type_ex(object_type, IndexFlags::None);
            c.is_type_assignable_to(t, object_index_type)
                || has_number_index_info && c.is_applicable_index_type(t, number_type)
        }) {
            if access_node.kind() == Kind::ElementAccessExpression
                && is_assignment_target(access_node)
                && object_type.object_flags().intersects(ObjectFlags::Mapped)
                && get_mapped_type_modifiers(object_type)
                    .intersects(MappedTypeModifiers::IncludeReadonly)
            {
                let type_string = self.type_to_string(object_type, None);
                self.error(
                    Some(access_node),
                    &diagnostics::INDEX_SIGNATURE_IN_TYPE_0_ONLY_PERMITS_READING,
                    &[&type_string],
                );
            }
            return t;
        }
        if self.is_generic_object_type(object_type) {
            let property_name = Self::get_property_name_from_index(index_type, Some(access_node));
            if property_name != INTERNAL_SYMBOL_NAME_MISSING {
                let property_symbol = self.get_constituent_property(object_type, &property_name);
                if let Some(property_symbol) = property_symbol
                    && get_declaration_modifier_flags_from_symbol(property_symbol)
                        .intersects(ModifierFlags::NonPublicAccessibilityModifier)
                {
                    self.error(Some(access_node), &diagnostics::PRIVATE_OR_PROTECTED_MEMBER_0_CANNOT_BE_ACCESSED_ON_A_TYPE_PARAMETER, &[&property_name]);
                    return self.error_type;
                }
            }
        }
        let index_string = self.type_to_string(index_type, None);
        let object_string = self.type_to_string(object_type, None);
        self.error(
            Some(access_node),
            &diagnostics::TYPE_0_CANNOT_BE_USED_TO_INDEX_TYPE_1,
            &[&index_string, &object_string],
        );
        self.error_type
    }

    // checker.go:8429
    fn get_constituent_property(
        &mut self,
        object_type: P<Type>,
        property_name: &str,
    ) -> Option<P<Symbol>> {
        let apparent_type = self.get_apparent_type(object_type);
        if let Some(result) = apparent_type.distributed_iter().into_iter().find_map(|t| {
            let prop = self.get_property_of_type(t, property_name);
            if prop.is_some() {
                return Some(prop);
            }

            None
        }) {
            return result;
        }
        None
    }

    // checker.go:8439
    fn check_import_call_expression(&mut self, node: P<Node>) -> P<Type> {
        // Check grammar of dynamic import
        self.check_grammar_import_call_expression(node);
        let args = node.arguments();
        if args.is_empty() {
            // No call arguments exist, so there are no child expressions to check.
            let any_type = self.any_type;
            return self.create_promise_return_type(node, any_type);
        }
        let specifier = args[0];
        if self.compiler_options.rewrite_relative_import_extensions.is_true()
            && !is_string_literal_like(specifier)
        {
            self.check_external_emit_helpers(
                node,
                ExternalEmitHelpers::RewriteRelativeImportExtension,
            );
        }
        let specifier_type = self.check_expression_cached(specifier);
        let mut options_type: Option<P<Type>> = None;
        if args.len() > 1 {
            options_type = Some(self.check_expression_cached(args[1]));
        }
        // Even though multiple arguments is grammatically incorrect, type-check extra arguments for completion
        for &arg in &args[2.min(args.len())..] {
            self.check_expression_cached(arg);
        }
        let string_type = self.string_type;
        if specifier_type.flags().intersects(TypeFlags::Nullable)
            || !self.is_type_assignable_to(specifier_type, string_type)
        {
            let type_string = self.type_to_string(specifier_type, None);
            self.error(
                Some(specifier),
                &diagnostics::DYNAMIC_IMPORT_S_SPECIFIER_MUST_BE_OF_TYPE_STRING_BUT_HERE_HAS_TYPE_0,
                &[&type_string],
            );
        }
        let mut import_attributes_type: Option<P<Type>> = None;
        if let Some(options_type) = options_type {
            let import_call_options_type = self.get_global_import_call_options_type_checked();
            if import_call_options_type != self.empty_object_type {
                let target = self.get_nullable_type(import_call_options_type, TypeFlags::Undefined);
                self.check_type_assignable_to(options_type, target, Some(args[1]), None);
            }
            if is_object_literal_expression(args[1])
                && let Some(prop) =
                    args[1].as_object_literal_expression().properties.nodes().iter().find_map(
                        |&prop| {
                            if is_property_assignment(prop)
                                && is_identifier(prop.name().unwrap())
                                && prop.name().unwrap().text() == "assert"
                            {
                                Some(prop)
                            } else {
                                None
                            }
                        },
                    )
            {
                self.error(prop.name(), &diagnostics::IMPORT_ASSERTIONS_HAVE_BEEN_REPLACED_BY_IMPORT_ATTRIBUTES_USE_WITH_INSTEAD_OF_ASSERT, &[]);
            }
            import_attributes_type = self.get_type_of_property_of_type(options_type, "with");
        }
        if tsrs_ast::is_source_phase_import_call(node) {
            let source_type = self.get_global_abstract_module_source_type();
            return self.create_promise_return_type(node, source_type);
        }
        // resolveExternalModuleName will return undefined if the moduleReferenceExpression is not a string literal
        let module_symbol = self.resolve_external_module_name(
            node,
            specifier,
            false, /*ignoreErrors*/
            import_attributes_type,
        );
        if let Some(module_symbol) = module_symbol {
            let es_module_symbol =
                self.resolve_external_module_symbol(module_symbol, true /*dontResolveAlias*/);
            // Go checks esModuleSymbol for nil; resolveExternalModuleSymbol never returns nil for a non-nil module symbol.
            let es_module_type = self.get_type_of_symbol(es_module_symbol);
            let mut synthetic_type = self.get_type_with_synthetic_default_only(
                Some(es_module_type),
                es_module_symbol,
                module_symbol,
                specifier,
                import_attributes_type,
            );
            if synthetic_type.is_none() {
                let es_module_type = self.get_type_of_symbol(es_module_symbol);
                synthetic_type = Some(self.get_type_with_synthetic_default_import_type(
                    es_module_type,
                    es_module_symbol,
                    module_symbol,
                    specifier,
                ));
            }
            return self.create_promise_return_type(node, synthetic_type.unwrap());
        }
        let any_type = self.any_type;
        self.create_promise_return_type(node, any_type)
    }

    /**
     * Syntactically and semantically checks a call or new expression.
     * @param node The call/new expression to be checked.
     * @returns On success, the expression's signature's return type. On failure, anyType.
     */
    // checker.go:8496
    fn check_call_expression(&mut self, node: P<Node>, check_mode: CheckMode) -> P<Type> {
        if self.compiler_options.rewrite_relative_import_extensions.is_true()
            && is_in_js_file(node)
            && is_require_call(node, false /*requireStringLiteralLikeArgument*/)
            && !is_string_literal_like(node.arguments()[0])
        {
            self.check_external_emit_helpers(
                node,
                ExternalEmitHelpers::RewriteRelativeImportExtension,
            );
        }
        self.check_grammar_type_arguments(node, node.type_argument_list());
        let signature =
            self.get_resolved_signature(node, None /*candidatesOutArray*/, check_mode);
        if signature == self.resolving_signature {
            // CheckMode.SkipGenericFunctions is enabled and this is a call to a generic function that
            // returns a function type. We defer checking and return silentNeverType.
            return self.silent_never_type;
        }
        self.check_deprecated_signature(signature, node);
        if node.expression().unwrap().kind() == Kind::SuperKeyword {
            return self.void_type;
        }
        if is_new_expression(node) {
            let declaration = signature.declaration();
            if let Some(declaration) = declaration
                && !is_constructor_declaration(declaration)
                && !is_construct_signature_declaration(declaration)
                && !is_constructor_type_node(declaration)
            {
                // When resolved signature is a call signature (and not a construct signature) the result type is any
                if self.no_implicit_any {
                    self.error(Some(node), &diagnostics::X_NEW_EXPRESSION_WHOSE_TARGET_LACKS_A_CONSTRUCT_SIGNATURE_IMPLICITLY_HAS_AN_ANY_TYPE, &[]);
                }
                return self.any_type;
            }
        }
        if is_in_js_file(node) && self.is_common_js_require(node) {
            return self.resolve_external_module_type_by_literal(node.arguments()[0]);
        }
        let return_type = self.get_return_type_of_signature(signature);
        // Treat any call to the global 'Symbol' function that is part of a const variable or readonly property
        // as a fresh unique symbol literal type.
        if return_type.flags().intersects(TypeFlags::ESSymbolLike)
            && self.is_symbol_or_symbol_for_call(node)
        {
            return self.get_es_symbol_like_type_for_node(
                walk_up_parenthesized_expressions(node.parent()).unwrap(),
            );
        }
        if is_call_expression(node)
            && node.question_dot_token().is_none()
            && is_expression_statement(node.parent().unwrap())
            && return_type.flags().intersects(TypeFlags::Void)
            && self.get_type_predicate_of_signature(signature).is_some()
        {
            let expression = node.expression().unwrap();
            if !is_dotted_name(expression) {
                self.error(Some(expression), &diagnostics::ASSERTIONS_REQUIRE_THE_CALL_TARGET_TO_BE_AN_IDENTIFIER_OR_QUALIFIED_NAME, &[]);
            } else if self.get_effects_signature(node).is_none() {
                let diagnostic = self.error(
                    Some(expression),
                    &diagnostics::ASSERTIONS_REQUIRE_EVERY_NAME_IN_THE_CALL_TARGET_TO_BE_DECLARED_WITH_AN_EXPLICIT_TYPE_ANNOTATION,
                    &[],
                );
                self.get_type_of_dotted_name(expression, Some(diagnostic));
            }
        }
        return_type
    }

    // checker.go:8538
    pub(crate) fn check_deprecated_signature(&mut self, sig: P<Signature>, node: P<Node>) {
        if sig.flags().intersects(SignatureFlags::IsSignatureCandidateForOverloadFailure) {
            return;
        }
        if let Some(declaration) = sig.declaration()
            && self.is_deprecated_declaration(declaration)
        {
            let suggestion_node = Self::get_deprecated_suggestion_node(node);
            let name =
                try_get_property_access_or_identifier_to_string(get_invoked_expression(node));
            let signature_string = self.signature_to_string(sig);
            self.add_deprecated_suggestion_with_signature(
                suggestion_node.unwrap(),
                declaration,
                &name,
                &signature_string,
            );
        }
    }

    // checker.go:8549
    fn add_deprecated_suggestion_with_signature(
        &mut self,
        location: P<Node>,
        declaration: P<Node>,
        deprecated_entity: &str,
        signature_string: &str,
    ) -> P<Diagnostic> {
        let message: &'static Message = if deprecated_entity.is_empty() {
            &diagnostics::X_0_IS_DEPRECATED
        } else {
            &diagnostics::THE_SIGNATURE_0_OF_1_IS_DEPRECATED
        };
        let diagnostic = new_diagnostic_for_node(
            Some(location),
            Some(message),
            &[&signature_string, &deprecated_entity],
        );
        self.add_deprecated_suggestion_worker(&[declaration], diagnostic)
    }

    // checker.go:8555
    fn is_symbol_or_symbol_for_call(&mut self, node: P<Node>) -> bool {
        if !is_call_expression(node) {
            return false;
        }
        let mut left = node.expression().unwrap();
        if is_property_access_expression(left) && left.name().unwrap().text() == "for" {
            left = left.expression().unwrap();
        }
        if !is_identifier(left) || left.text() != "Symbol" {
            return false;
        }
        // make sure `Symbol` is the global symbol
        let Some(global_es_symbol) = self.get_global_es_symbol_constructor_symbol_or_nil() else {
            return false;
        };
        Some(global_es_symbol)
            == self.resolve_name(
                Some(left),
                "Symbol",
                SymbolFlags::Value,
                None,  /*nameNotFoundMessage*/
                false, /*isUse*/
                false,
            )
    }

    /**
     * Resolve a signature of a given call-like expression.
     * @param node a call-like expression to try resolve a signature for
     * @param candidatesOutArray an array of signature to be filled in by the function. It is passed by signature help in the language service;
     *    the function will fill it up with appropriate candidate signatures
     * @return a signature of the call-like expression or undefined if one can't be found
     */
    // checker.go:8581
    pub(crate) fn get_resolved_signature(
        &mut self,
        node: P<Node>,
        candidates_out_array: Option<&mut Vec<P<Signature>>>,
        check_mode: CheckMode,
    ) -> P<Signature> {
        let links = self.signature_links.get(node);
        // If getResolvedSignature has already been called, we will have cached the resolvedSignature.
        // However, it is possible that either candidatesOutArray was not passed in the first time,
        // or that a different candidatesOutArray was passed in. Therefore, we need to redo the work
        // to correctly fill the candidatesOutArray.
        let cached = links.resolved_signature.get();
        if let Some(cached) = cached {
            if cached != self.resolving_signature && candidates_out_array.is_none() {
                return cached;
            }
            if cached == self.resolving_signature {
                // Re-entered while this call is resolved: the inner answer may not be the final one (flow memo).
                self.flow_memo.taint(0);
            }
        }
        let save_resolution_start = self.resolution_start;
        if cached.is_none() {
            // If we haven't already done so, temporarily reset the resolution stack. This allows us to
            // handle "inverted" situations where, for example, an API client asks for the type of a symbol
            // containined in a function call argument whose contextual type depends on the symbol itself
            // through resolution of the containing function call. By resetting the resolution stack we'll
            // retry the symbol type resolution with the resolvingSignature marker in place to suppress
            // the contextual type circularity.
            self.resolution_start = self.type_resolutions.len();
        }
        links.resolved_signature.set(Some(self.resolving_signature));
        let mut result = self.resolve_signature(node, candidates_out_array, check_mode);
        self.resolution_start = save_resolution_start;
        // When CheckMode.SkipGenericFunctions is set we use resolvingSignature to indicate that call
        // resolution should be deferred.
        if result != self.resolving_signature {
            // if the signature resolution originated on a node that itself depends on the contextual type
            // then it's possible that the resolved signature might not be the same as the one that would be computed in source order
            // since resolving such signature leads to resolving the potential outer signature, its arguments and thus the very same signature
            // it's possible that this inner resolution sets the resolvedSignature first.
            // In such a case we ignore the local result and reuse the correct one that was cached.
            if links.resolved_signature.get() != Some(self.resolving_signature) {
                result = links.resolved_signature.get().unwrap();
            }
            // If signature resolution originated in control flow type analysis (for example to compute the
            // assigned type in a flow assignment) we don't cache the result as it may be based on temporary
            // types from the control flow analysis.
            if self.flow_loop_stack.is_empty() {
                links.resolved_signature.set(Some(result));
            } else {
                links.resolved_signature.set(cached);
            }
        }
        result
    }

    // checker.go:8627
    fn resolve_signature(
        &mut self,
        node: P<Node>,
        candidates_out_array: Option<&mut Vec<P<Signature>>>,
        check_mode: CheckMode,
    ) -> P<Signature> {
        match node.kind() {
            Kind::CallExpression => {
                self.resolve_call_expression(node, candidates_out_array, check_mode)
            }
            Kind::NewExpression => {
                self.resolve_new_expression(node, candidates_out_array, check_mode)
            }
            Kind::TaggedTemplateExpression => {
                self.resolve_tagged_template_expression(node, candidates_out_array, check_mode)
            }
            Kind::Decorator => self.resolve_decorator(node, candidates_out_array, check_mode),
            Kind::JsxOpeningFragment | Kind::JsxOpeningElement | Kind::JsxSelfClosingElement => {
                self.resolve_jsx_opening_like_element(node, candidates_out_array, check_mode)
            }
            Kind::BinaryExpression => {
                self.resolve_instanceof_expression(node, candidates_out_array, check_mode)
            }
            _ => panic!("Unhandled case in resolveSignature"),
        }
    }

    // checker.go:8645
    fn resolve_call_expression(
        &mut self,
        node: P<Node>,
        candidates_out_array: Option<&mut Vec<P<Signature>>>,
        check_mode: CheckMode,
    ) -> P<Signature> {
        let expression = node.expression().unwrap();
        if expression.kind() == Kind::SuperKeyword {
            let super_type = self.check_super_expression(expression);
            if is_type_any(Some(super_type)) {
                for &arg in node.arguments() {
                    // Still visit arguments so they get marked for visibility, etc
                    self.check_expression(arg);
                }
                return self.any_signature;
            }
            if !self.is_error_type(super_type) {
                // In super call, the candidate signatures are the matching arity signatures of the base constructor function instantiated
                // with the type arguments specified in the extends clause.
                let base_type_node =
                    get_class_extends_heritage_element(get_containing_class(node).unwrap());
                if let Some(base_type_node) = base_type_node {
                    let base_constructors = self.get_instantiated_constructors_for_type_arguments(
                        super_type,
                        base_type_node.type_arguments(),
                        base_type_node,
                    );
                    return self.resolve_call(
                        node,
                        &base_constructors,
                        candidates_out_array,
                        check_mode,
                        SignatureFlags::None,
                        None,
                    );
                }
            }
            return self.resolve_untyped_call(node);
        }
        if is_import_call(node) {
            return self.resolve_untyped_call(node);
        }
        let call_chain_flags: SignatureFlags;
        let mut func_type = self.check_expression(expression);
        if is_call_chain(node) {
            let non_optional_type = self.get_optional_expression_type(func_type, expression);
            call_chain_flags = if non_optional_type == func_type {
                SignatureFlags::None
            } else if is_outermost_optional_chain(node) {
                SignatureFlags::IsOuterCallChain
            } else {
                SignatureFlags::IsInnerCallChain
            };
            func_type = non_optional_type;
        } else {
            call_chain_flags = SignatureFlags::None;
        }
        func_type = self.check_non_null_type_with_reporter(
            func_type,
            expression,
            super::checker::Checker::report_cannot_invoke_possibly_null_or_undefined_error,
        );
        if func_type == self.silent_never_type {
            return self.silent_never_signature;
        }
        let apparent_type = self.get_apparent_type(func_type);
        if self.is_error_type(apparent_type) {
            // Another error has already been reported
            return self.resolve_error_call(node);
        }
        // Technically, this signatures list may be incomplete. We are taking the apparent type,
        // but we are not including call signatures that may have been added to the Object or
        // Function interface, since they have none by default. This is a bit of a leap of faith
        // that the user will not add any.
        let call_signatures = self.get_signatures_of_type(apparent_type, SignatureKind::Call);
        let num_construct_signatures =
            self.get_signatures_of_type(apparent_type, SignatureKind::Construct).len();
        // TS 1.0 Spec: 4.12
        // In an untyped function call no TypeArgs are permitted, Args can be any argument list, no contextual
        // types are provided for the argument expressions, and the result is always of type Any.
        if self.is_untyped_function_call(
            func_type,
            apparent_type,
            call_signatures.len(),
            num_construct_signatures,
        ) {
            // The unknownType indicates that an error already occurred (and was reported).  No
            // need to report another error in this case.
            if !self.is_error_type(func_type) && node.type_argument_list().is_some() {
                self.error(
                    Some(node),
                    &diagnostics::UNTYPED_FUNCTION_CALLS_MAY_NOT_ACCEPT_TYPE_ARGUMENTS,
                    &[],
                );
            }
            return self.resolve_untyped_call(node);
        }
        // If FuncExpr's apparent type(section 3.8.1) is a function type, the call is a typed function call.
        // TypeScript employs overload resolution in typed function calls in order to support functions
        // with multiple call signatures.
        if call_signatures.is_empty() {
            if num_construct_signatures != 0 {
                let type_string = self.type_to_string(func_type, None);
                self.error(
                    Some(node),
                    &diagnostics::VALUE_OF_TYPE_0_IS_NOT_CALLABLE_DID_YOU_MEAN_TO_INCLUDE_NEW,
                    &[&type_string],
                );
            } else {
                let mut related_information: Option<P<Diagnostic>> = None;
                if node.arguments().len() == 1 {
                    let text = get_source_file_of_node(node).unwrap().text();
                    let options = tsrs_scanner::SkipTriviaOptions {
                        stop_after_line_break: true,
                        ..Default::default()
                    };
                    let pos = tsrs_scanner::skip_trivia_ex(
                        text,
                        expression.end().expect_source("parsed nodes have source positions"),
                        Some(&options),
                    );
                    if stringutil::is_line_break(u32::from(text.as_bytes()[(pos - 1) as usize])) {
                        related_information = Some(create_diagnostic_for_node(
                            Some(expression),
                            &diagnostics::ARE_YOU_MISSING_A_SEMICOLON,
                            &[],
                        ));
                    }
                }
                self.invocation_error(
                    expression,
                    apparent_type,
                    SignatureKind::Call,
                    related_information,
                );
            }
            return self.resolve_error_call(node);
        }
        // When a call to a generic function is an argument to an outer call to a generic function for which
        // inference is in process, we have a choice to make. If the inner call relies on inferences made from
        // its contextual type to its return type, deferring the inner call processing allows the best possible
        // contextual type to accumulate. But if the outer call relies on inferences made from the return type of
        // the inner call, the inner call should be processed early. There's no sure way to know which choice is
        // right (only a full unification algorithm can determine that), so we resort to the following heuristic:
        // If no type arguments are specified in the inner call and at least one call signature is generic and
        // returns a function type, we choose to defer processing. This narrowly permits function composition
        // operators to flow inferences through return types, but otherwise processes calls right away. We
        // use the resolvingSignature singleton to indicate that we deferred processing. This result will be
        // propagated out and eventually turned into silentNeverType (a type that is assignable to anything and
        // from which we never make inferences).
        if check_mode.intersects(CheckMode::SkipGenericFunctions)
            && node.type_arguments().is_empty()
            && call_signatures.iter().any(|&sig| self.is_generic_function_returning_function(sig))
        {
            self.skipped_generic_function(node, check_mode);
            return self.resolving_signature;
        }
        self.resolve_call(
            node,
            call_signatures,
            candidates_out_array,
            check_mode,
            call_chain_flags,
            None,
        )
    }
}
