use crate::*;
use smallvec::SmallVec;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;

// Non-function declarations in checker.go:26123-28299 (hand-ported by checker-foundation):
//   type IntersectionFlags (checker.go:26504)
//   const (checker.go:26506): IntersectionFlagsNone, IntersectionFlagsNoSupertypeReduction,
//     IntersectionFlagsNoConstraintReduction

/// The constituent kinds of an `ObjectFlags::NonNullableUnion`: for each, `get_type_facts_worker` returns a constant
/// without `IsUndefined`, `IsNull` or, under `strictNullChecks`, `EQUndefined`, `EQNull` and `EQUndefinedOrNull`,
/// and resolves nothing when the caller needs no object fact. Nullable and void types carry those facts; an
/// intersection or an instantiable type (a template literal or string mapping included) goes through its base
/// constraint first; `any` and `unknown` have the EQ facts.
const NON_NULLABLE_KINDS: TypeFlags =
    TypeFlags::DefinitelyNonNullable.difference(TypeFlags::Instantiable);

// Go formats an `any` diagnostic argument with `%v`.
fn literal_value_to_string(value: Option<LiteralValue>) -> String {
    match value {
        None => "<nil>".to_string(),
        Some(LiteralValue::String(s)) => s.to_string(),
        Some(LiteralValue::Number(n)) => n.to_string(),
        Some(LiteralValue::Boolean(b)) => b.to_string(),
        Some(LiteralValue::BigInt(b)) => b.to_string(),
    }
}

impl Checker {
    // checker.go:26123
    #[cfg_attr(
        not(feature = "work-census"),
        inline(always),
        expect(
            clippy::inline_always,
            reason = "without the census the wrapper is a forwarding call; inlined, callers call the body as before (notes/perf-checker-algorithms.md)"
        )
    )]
    pub(crate) fn get_union_type_worker(
        &mut self,
        types: &[P<Type>],
        union_reduction: UnionReduction,
        alias: AliasArg<'_>,
        origin: Option<P<Type>>,
    ) -> P<Type> {
        if self.census_on() {
            let span = self.census_begin(crate::workcensus::Cat::Union, || {
                crate::workcensus::CKey::Bucket(crate::workcensus::bucket(types.len()))
            });
            let r = self.get_union_type_worker_inner(types, union_reduction, alias, origin);
            let timing = self.census_end(span).unwrap();
            let key = crate::workcensus::CKey::Bucket(crate::workcensus::bucket(types.len()));
            self.census.as_mut().unwrap().record(
                crate::workcensus::Cat::Union,
                key,
                timing,
                types.len() as u64,
                (union_reduction == UnionReduction::Subtype) as u64,
                0,
            );
            return r;
        }
        self.get_union_type_worker_inner(types, union_reduction, alias, origin)
    }

    fn get_union_type_worker_inner(
        &mut self,
        types: &[P<Type>],
        union_reduction: UnionReduction,
        alias: AliasArg<'_>,
        origin: Option<P<Type>>,
    ) -> P<Type> {
        let mut origin = origin;
        let (mut type_set, includes) = self.add_types_to_union(types);
        if union_reduction != UnionReduction::None {
            if includes.intersects(TypeFlags::AnyOrUnknown) {
                if includes.intersects(TypeFlags::Any) {
                    if includes.intersects(TypeFlags::IncludesWildcard) {
                        return self.wildcard_type;
                    } else if includes.intersects(TypeFlags::IncludesError) {
                        return self.error_type;
                    }
                    return self.any_type;
                }
                return self.unknown_type;
            }
            if includes.intersects(TypeFlags::Undefined) {
                // If type set contains both undefinedType and missingType, remove missingType
                if type_set.len() >= 2
                    && type_set[0] == self.undefined_type
                    && type_set[1] == self.missing_type
                {
                    type_set.remove(1);
                }
            }
            if includes.intersects(
                TypeFlags::Enum
                    | TypeFlags::Literal
                    | TypeFlags::UniqueESSymbol
                    | TypeFlags::TemplateLiteral
                    | TypeFlags::StringMapping,
            ) || includes.intersects(TypeFlags::Void)
                && includes.intersects(TypeFlags::Undefined)
            {
                type_set = self.remove_redundant_literal_types(
                    &type_set,
                    includes,
                    union_reduction == UnionReduction::Subtype,
                );
            }
            if includes.intersects(TypeFlags::StringLiteral)
                && includes.intersects(TypeFlags::TemplateLiteral | TypeFlags::StringMapping)
            {
                // tsrs: relations and inference; never stored in the union front cache (unioncache.rs).
                self.union_front_cache.impure = self.union_front_cache.impure.wrapping_add(1);
                type_set = self.remove_string_literals_matched_by_template_literals(&type_set);
            }
            if includes.intersects(TypeFlags::IncludesConstrainedTypeVariable) {
                // tsrs: reads base constraints; never stored in the union front cache (unioncache.rs).
                self.union_front_cache.impure = self.union_front_cache.impure.wrapping_add(1);
                type_set = self.remove_constrained_type_variables(&type_set);
            }
            if union_reduction == UnionReduction::Subtype {
                match self.remove_subtypes(&type_set, includes.intersects(TypeFlags::Object)) {
                    Some(reduced) => type_set = reduced,
                    None => return self.error_type,
                }
            }
            if type_set.is_empty() {
                if includes.intersects(TypeFlags::Null) {
                    if includes.intersects(TypeFlags::IncludesNonWideningType) {
                        return self.null_type;
                    }
                    return self.null_widening_type;
                } else if includes.intersects(TypeFlags::Undefined) {
                    if includes.intersects(TypeFlags::IncludesNonWideningType) {
                        return self.undefined_type;
                    }
                    return self.undefined_widening_type;
                }
                return self.never_type;
            }
        }
        if origin.is_none() && includes.intersects(TypeFlags::Union) {
            let named_unions = Self::add_named_unions(&[], types);
            let mut reduced_types: Vec<P<Type>> = Vec::new();
            if (named_unions.len() > 1 || type_set.len() > 8)
                && self.program.source_files_complete()
            {
                // tsrs-only: under a total order `contains_type` finds exactly the identical type, so one set of the
                // named unions' constituents answers every constituent's question; the binary searches, one per
                // named union per constituent, were most of the `compare_types` calls on the 38k-file codebase.
                let mut named: FxHashSet<P<Type>> = FxHashSet::default();
                // Extend each known-length constituent slice so the set reserves before inserting.
                for u in &named_unions {
                    named.extend(u.types().iter().copied());
                }
                reduced_types.extend(type_set.iter().copied().filter(|t| !named.contains(t)));
            } else {
                reduced_types.extend(type_set.iter().filter_map(|&t| {
                    if named_unions.iter().any(|u| contains_type(self, u.types(), t)) {
                        None
                    } else {
                        Some(t)
                    }
                }));
            }
            if alias.is_none() && named_unions.len() == 1 && reduced_types.is_empty() {
                return named_unions[0];
            }
            // We create a denormalized origin type only when the union was created from one or more named unions
            // (unions with alias symbols or origins) and when there is no overlap between those named unions.
            let mut named_types_count = 0;
            named_types_count =
                named_unions.iter().fold(named_types_count, |mut named_types_count, u| {
                    named_types_count += u.types().len();

                    named_types_count
                });
            if named_types_count + reduced_types.len() == type_set.len() {
                reduced_types = named_unions.iter().fold(reduced_types, |mut reduced_types, &t| {
                    reduced_types = insert_type(self, &reduced_types, t).0;

                    reduced_types
                });
                origin = Some(self.new_union_type(ObjectFlags::None, &reduced_types));
            }
        }
        let object_flags = if_else(
            includes.intersects(TypeFlags::NotPrimitiveUnion),
            ObjectFlags::None,
            ObjectFlags::PrimitiveUnion,
        ) | if_else(
            includes.intersects(TypeFlags::Intersection),
            ObjectFlags::ContainsIntersections,
            ObjectFlags::None,
        );
        self.get_union_type_from_sorted_list(&type_set, object_flags, alias, origin)
    }

    // This function assumes the constituent type list is sorted and deduplicated.
    // checker.go:26206
    fn get_union_type_from_sorted_list(
        &mut self,
        types: &[P<Type>],
        precomputed_object_flags: ObjectFlags,
        alias: AliasArg<'_>,
        origin: Option<P<Type>>,
    ) -> P<Type> {
        if types.is_empty() {
            return self.never_type;
        }
        if types.len() == 1 {
            return types[0];
        }
        let key = get_union_key(types, origin, alias);
        if let Some(t) = self.union_types.get(&key) {
            return t;
        }
        let flags = precomputed_object_flags
            | Self::get_propagating_flags_of_types(types, TypeFlags::Nullable);
        let t = self.new_union_type(flags, types);
        // tsrs-only: the summary `get_type_facts_worker` reads for the nullability facts, from the final constituent
        // list (callers pass only part of a source union's flags) and after `new_type` (which drops the bit).
        let kinds = types.iter().fold(TypeFlags::empty(), |kinds, &t| kinds | t.flags());
        if NON_NULLABLE_KINDS.contains(kinds) {
            t.object_flags.set(t.object_flags() | ObjectFlags::NonNullableUnion);
        }
        t.as_union_type().set_origin(origin);
        t.set_alias(alias.alias());
        if types.len() == 2
            && types[0].flags().intersects(TypeFlags::BooleanLiteral)
            && types[1].flags().intersects(TypeFlags::BooleanLiteral)
        {
            t.flags.set(t.flags() | TypeFlags::Boolean);
        }
        self.union_types.insert(key, t);
        t
    }

    // checker.go:26231
    /// tsrs-only, after can1357's microsoft/TypeScript perf/checker-speedups-v3 (a3f430da0, 628f51ceb): each flattened
    /// union is already sorted and unique, and filtering keeps that order, so only the boundaries between inputs can
    /// leave the list unsorted. A sorted list skips the sort, two unions are merged directly, and otherwise the
    /// ascending runs between those boundaries are merged (`merge_ascending_runs`) instead of sorting the whole list.
    /// `compare_types` ends with the type id, so distinct types never compare equal and every correct sort of the same
    /// list gives the same order. That holds only while `compare_symbols` is a total order
    /// (`Program::source_files_complete`); otherwise this sorts as Go does.
    fn add_types_to_union(&mut self, source_types: &[P<Type>]) -> (Vec<P<Type>>, TypeFlags) {
        fn add_type(c: &Checker, types: &mut Vec<P<Type>>, includes: &mut TypeFlags, t: P<Type>) {
            let flags = t.flags();
            // We ignore 'never' types in unions
            if flags.intersects(TypeFlags::Never) {
                return;
            }
            *includes |= flags & TypeFlags::IncludesMask;
            if flags.intersects(TypeFlags::Instantiable) {
                *includes |= TypeFlags::IncludesInstantiable;
            }
            if flags.intersects(TypeFlags::Intersection)
                && t.object_flags_lazy().intersects(ObjectFlags::IsConstrainedTypeVariable)
            {
                *includes |= TypeFlags::IncludesConstrainedTypeVariable;
            }
            if t == c.wildcard_type {
                *includes |= TypeFlags::IncludesWildcard;
            }
            if c.is_error_type(t) {
                *includes |= TypeFlags::IncludesError;
            }
            if !c.strict_null_checks && flags.intersects(TypeFlags::Nullable) {
                if !t.object_flags().intersects(ObjectFlags::ContainsWideningType) {
                    *includes |= TypeFlags::IncludesNonWideningType;
                }
                return;
            }
            types.push(t);
        }

        let total_order = self.program.source_files_complete();
        let mut capacity = source_types.len();
        if source_types.len() == 2 {
            capacity = source_types.iter().fold(capacity, |mut capacity, &t| {
                if t.flags().intersects(TypeFlags::Union) {
                    capacity += t.types().len() - 1;
                }

                capacity
            });
            if source_types[0] == source_types[1] {
                capacity /= 2;
            }
        }
        let mut types: Vec<P<Type>> = Vec::with_capacity(capacity);
        let mut includes = TypeFlags::empty();
        if total_order
            && source_types.len() == 2
            && (source_types[0].flags() & source_types[1].flags()).intersects(TypeFlags::Union)
        {
            includes = source_types.iter().fold(includes, |mut includes, &t| {
                if t.alias().is_some() || t.as_union_type().origin().is_some() {
                    includes |= TypeFlags::Union;
                }

                includes
            });
            // Two sorted, unique runs: merge them (a constituent of both is added once).
            merge_sorted_types(
                self,
                source_types[0].types(),
                source_types[1].types(),
                true, /*dedup*/
                &mut |c, t| add_type(c, &mut types, &mut includes, t),
            );
            return (types, includes);
        }
        // The flattened list is a sequence of ascending runs: a new run starts at an input whose first constituent is
        // below the last one before it.
        let mut run_starts: SmallVec<[usize; 4]> = SmallVec::new();
        let mut last_type: Option<P<Type>> = None;
        for &t in source_types {
            if Some(t) != last_type {
                let start = types.len();
                if t.flags().intersects(TypeFlags::Union) {
                    let u = t.as_union_type();
                    if t.alias().is_some() || u.origin().is_some() {
                        includes |= TypeFlags::Union;
                    }
                    for &s in u.types() {
                        add_type(self, &mut types, &mut includes, s);
                    }
                } else {
                    add_type(self, &mut types, &mut includes, t);
                }
                if total_order
                    && start > 0
                    && types.len() > start
                    && compare_types(self, Some(types[start - 1]), Some(types[start])).is_gt()
                {
                    run_starts.push(start);
                }
                last_type = Some(t);
            }
        }
        if types.len() >= 2 {
            // Sort and deduplicate types
            if !total_order {
                tsrs_core::goslices::sort_stable_func(&mut types, |&a, &b| {
                    compare_types(self, Some(a), Some(b))
                });
            } else if !run_starts.is_empty() {
                merge_ascending_runs(self, &mut types, &run_starts);
            }
            let mut unique = 1;
            for i in 1..types.len() {
                let t = types[i];
                if t != types[unique - 1] {
                    types[unique] = t;
                    unique += 1;
                }
            }
            types.truncate(unique);
        }
        (types, includes)
    }

    // checker.go:26294
    fn add_named_unions(named_unions: &[P<Type>], types: &[P<Type>]) -> Vec<P<Type>> {
        let mut named_unions = named_unions.to_vec();
        for &t in types {
            if t.flags().intersects(TypeFlags::Union) {
                let u = t.as_union_type();
                let origin = u.origin();
                if t.alias().is_some()
                    || origin.is_some_and(|o| !o.flags().intersects(TypeFlags::Union))
                {
                    append_if_unique(&mut named_unions, t);
                } else if let Some(origin) =
                    origin.filter(|o| o.flags().intersects(TypeFlags::Union))
                {
                    named_unions = Self::add_named_unions(&named_unions, origin.types());
                }
            }
        }
        named_unions
    }

    // checker.go:26308
    fn remove_redundant_literal_types(
        &mut self,
        types: &[P<Type>],
        includes: TypeFlags,
        reduce_void_undefined: bool,
    ) -> Vec<P<Type>> {
        let mut types = types.to_vec();
        let mut i = types.len();
        while i > 0 {
            i -= 1;
            let t = types[i];
            let flags = t.flags();
            let remove = flags.intersects(
                TypeFlags::StringLiteral | TypeFlags::TemplateLiteral | TypeFlags::StringMapping,
            ) && includes.intersects(TypeFlags::String)
                || flags.intersects(TypeFlags::NumberLiteral)
                    && includes.intersects(TypeFlags::Number)
                || flags.intersects(TypeFlags::BigIntLiteral)
                    && includes.intersects(TypeFlags::BigInt)
                || flags.intersects(TypeFlags::UniqueESSymbol)
                    && includes.intersects(TypeFlags::ESSymbol)
                || reduce_void_undefined
                    && flags.intersects(TypeFlags::Undefined)
                    && includes.intersects(TypeFlags::Void)
                || is_fresh_literal_type(t)
                    && contains_regular_literal_type(
                        self,
                        &types,
                        i,
                        t.as_literal_type().regular_type().unwrap(),
                    );
            if remove {
                types.remove(i);
            }
        }
        types
    }

    // checker.go:26327
    fn remove_string_literals_matched_by_template_literals(
        &mut self,
        types: &[P<Type>],
    ) -> Vec<P<Type>> {
        let mut types = types.to_vec();
        let mut templates: Vec<P<Type>> = Vec::new();
        // Keep this measured hot collector as a loop; see notes/perf-iterator-regressions.md.
        for &t in &types {
            if self.is_pattern_literal_type(t) {
                templates.push(t);
            }
        }
        if !templates.is_empty() {
            let mut i = types.len();
            while i > 0 {
                i -= 1;
                let t = types[i];
                if t.flags().intersects(TypeFlags::StringLiteral)
                    && templates.iter().any(|&template| {
                        // tsrs: the text test `inferFromLiteralPartsToTemplateLiteral` starts with, done before the
                        // call; a literal that fails it is not matched, and the call would have done nothing else.
                        if template.flags().intersects(TypeFlags::TemplateLiteral) {
                            let texts = template.as_template_literal_type().texts();
                            let (start, end) = (texts[0], texts[texts.len() - 1]);
                            let value = get_string_literal_value(t);
                            if value.len() < start.len() + end.len()
                                || !value.starts_with(start)
                                || !value.ends_with(end)
                            {
                                return false;
                            }
                        }
                        self.is_type_matched_by_template_literal_or_string_mapping(t, template)
                    })
                {
                    types.remove(i);
                }
            }
        }
        types
    }

    // checker.go:26344
    fn is_type_matched_by_template_literal_or_string_mapping(
        &mut self,
        t: P<Type>,
        template: P<Type>,
    ) -> bool {
        if template.flags().intersects(TypeFlags::TemplateLiteral) {
            let comparer = Self::compare_types_assignable_comparer();
            return self.is_type_matched_by_template_literal_type(
                t,
                template.as_template_literal_type(),
                comparer,
            );
        }
        self.is_member_of_string_mapping(t, template)
    }

    // checker.go:26351
    fn remove_constrained_type_variables(&mut self, types: &[P<Type>]) -> Vec<P<Type>> {
        let mut types = types.to_vec();
        let mut type_variables: Vec<P<Type>> = Vec::new();
        // First collect a list of the type variables occurring in constraining intersections.
        for &t in &types {
            if t.flags().intersects(TypeFlags::Intersection)
                && t.object_flags_lazy().intersects(ObjectFlags::IsConstrainedTypeVariable)
            {
                let it = t.as_intersection_type().types();
                let index = usize::from(!it[0].flags().intersects(TypeFlags::TypeVariable));
                append_if_unique(&mut type_variables, it[index]);
            }
        }
        // For each type variable, check if the constraining intersections for that type variable fully
        // cover the constraint of the type variable; if so, remove the constraining intersections and
        // substitute the type variable.
        for &type_variable in &type_variables {
            let mut primitives: Vec<P<Type>> = Vec::new();
            // First collect the primitive types from the constraining intersections.
            for &t in &types {
                if t.flags().intersects(TypeFlags::Intersection)
                    && t.object_flags_lazy().intersects(ObjectFlags::IsConstrainedTypeVariable)
                {
                    let it = t.as_intersection_type().types();
                    let index = usize::from(!it[0].flags().intersects(TypeFlags::TypeVariable));
                    if it[index] == type_variable {
                        primitives = insert_type(self, &primitives, it[1 - index]).0;
                    }
                }
            }
            // If every constituent in the type variable's constraint is covered by an intersection of the type
            // variable and that constituent, remove those intersections and substitute the type variable.
            let constraint = self.get_base_constraint_of_type(type_variable).unwrap();
            if every_type(self, constraint, |c, t| contains_type(c, &primitives, t)) {
                let mut i = types.len();
                while i > 0 {
                    i -= 1;
                    let t = types[i];
                    if t.flags().intersects(TypeFlags::Intersection)
                        && t.object_flags_lazy().intersects(ObjectFlags::IsConstrainedTypeVariable)
                    {
                        let it = t.as_intersection_type().types();
                        let index = usize::from(!it[0].flags().intersects(TypeFlags::TypeVariable));
                        if it[index] == type_variable
                            && contains_type(self, &primitives, it[1 - index])
                        {
                            types.remove(i);
                        }
                    }
                }
                types = insert_type(self, &types, type_variable).0;
            }
        }
        types
    }

    // Go returns nil ("too complex") as a distinct result, hence Option.
    // checker.go:26404
    #[cfg_attr(
        not(feature = "work-census"),
        inline(always),
        expect(
            clippy::inline_always,
            reason = "without the census the wrapper is a forwarding call; inlined, callers call the body as before (notes/perf-checker-algorithms.md)"
        )
    )]
    fn remove_subtypes(
        &mut self,
        types: &[P<Type>],
        has_object_types: bool,
    ) -> Option<Vec<P<Type>>> {
        if self.census_on() {
            let span = self.census_begin(crate::workcensus::Cat::RemoveSubtypes, || {
                crate::workcensus::CKey::Bucket(crate::workcensus::bucket(types.len()))
            });
            let r = self.remove_subtypes_inner(types, has_object_types);
            let timing = self.census_end(span).unwrap();
            let removed = r.as_ref().map_or(0, |r| types.len() - r.len()) as u64;
            let key = crate::workcensus::CKey::Bucket(crate::workcensus::bucket(types.len()));
            self.census.as_mut().unwrap().record(
                crate::workcensus::Cat::RemoveSubtypes,
                key,
                timing,
                types.len() as u64,
                removed,
                r.is_none() as u64,
            );
            return r;
        }
        self.remove_subtypes_inner(types, has_object_types)
    }

    fn remove_subtypes_inner(
        &mut self,
        types: &[P<Type>],
        has_object_types: bool,
    ) -> Option<Vec<P<Type>>> {
        // [] and [T] immediately reduce to [] and [T] respectively
        if types.len() < 2 {
            return Some(types.to_vec());
        }
        let key = get_type_list_key(types);
        // tsrs-only: TSRS_TRACE_UNION_REDUCTION (uniontrace.rs). Only a union of more than 1,000 types can reach the
        // TS2590 limit below.
        let trace = types.len() > 1000 && crate::uniontrace::enabled();
        if let Some(cached) = self.subtype_reduction_cache.get(&key).copied() {
            if trace {
                crate::uniontrace::report(self, types, cached, "reduced", None, None);
            }
            return Some(cached.to_vec());
        }
        let input = types;
        let mut types = types.to_vec();
        let mut checkpoint = None;
        // We assume that redundant primitive types have already been removed from the types array and that there
        // are no any and unknown types in the array. Thus, the only possible supertypes for primitive types are empty
        // object types, and if none of those are present we can exclude primitive types from the subtype check.
        let has_empty_object = has_object_types
            && types.clone().into_iter().any(|t| {
                t.flags().intersects(TypeFlags::Object)
                    && !self.is_generic_mapped_type(t)
                    && self.is_empty_structured_type(t)
            });
        let length = types.len() as i64;
        let mut i = types.len();
        let mut count: i64 = 0;
        // tsrs: the type of the key property of each target, for the key property name of the last source that had
        // one: the sources of a discriminated union share the name, and without this each target's property is looked
        // up again for every source. Dropped when the name changes or any type's members are reset.
        let mut key_types: FxHashMap<P<Type>, Option<P<Type>>> = FxHashMap::default();
        let mut key_types_of: Option<(&'static str, u32)> = None;
        while i > 0 {
            i -= 1;
            let source = types[i];
            if has_empty_object || source.flags().intersects(TypeFlags::StructuredOrInstantiable) {
                // A type parameter with a union constraint may be a subtype of some union, but not a subtype of the
                // individual constituents of that union. For example, `T extends A | B` is a subtype of `A | B`, but not
                // a subtype of just `A` or just `B`. When we encounter such a type parameter, we therefore check if the
                // type parameter is a subtype of a union of all the other types.
                if source.flags().intersects(TypeFlags::TypeParameter)
                    && self.get_base_constraint_or_type(source).flags().intersects(TypeFlags::Union)
                {
                    let never_type = self.never_type;
                    let mapped: Vec<P<Type>> =
                        types.iter().map(|&t| if t == source { never_type } else { t }).collect();
                    let union = self.get_union_type(&mapped);
                    let relation = self.strict_subtype_relation;
                    if self.is_type_related_to(source, union, relation) {
                        types.remove(i);
                    }
                    continue;
                }
                // Find the first property with a unit type, if any. When constituents have a property by the same name
                // but of a different unit type, we can quickly disqualify them from subtype checks. This helps subtype
                // reduction of large discriminated union types.
                let mut key_property: Option<P<Symbol>> = None;
                let mut key_property_type: Option<P<Type>> = None;
                if source.flags().intersects(
                    TypeFlags::Object
                        | TypeFlags::Intersection
                        | TypeFlags::InstantiableNonPrimitive,
                ) {
                    key_property = self.get_properties_of_type(source).iter().copied().find(|&p| {
                        let t = self.get_type_of_symbol(p);
                        is_unit_type(t)
                    });
                }
                if let Some(key_property) = key_property {
                    let t = self.get_type_of_symbol(key_property);
                    key_property_type = Some(self.get_regular_type_of_literal_type(t));
                }
                let mut j = 0;
                while j < types.len() {
                    let target = types[j];
                    j += 1;
                    if source != target {
                        if count == 100_000 {
                            // After 100000 subtype checks we estimate the remaining amount of work by assuming the
                            // same ratio of checks per element. If the estimated number of remaining type checks is
                            // greater than 1M we deem the union type too complex to represent. This for example
                            // caps union types at 1000 unique object types.
                            let estimated_count = (count / (length - i as i64)) * length;
                            if trace {
                                checkpoint = Some(crate::uniontrace::Checkpoint {
                                    sources: length - i as i64,
                                    estimate: estimated_count,
                                });
                            }
                            if estimated_count > 1_000_000 {
                                if trace {
                                    crate::uniontrace::report(
                                        self,
                                        input,
                                        &types,
                                        "too complex (TS2590)",
                                        Some(count),
                                        checkpoint,
                                    );
                                }
                                self.report_too_complex(Self::too_complex_key(input));
                                return None;
                            }
                        }
                        count += 1;
                        if let Some(key_property) = key_property
                            && target.flags().intersects(
                                TypeFlags::Object
                                    | TypeFlags::Intersection
                                    | TypeFlags::InstantiableNonPrimitive,
                            )
                        {
                            let of = Some((key_property.name(), self.members_reset_epoch));
                            if key_types_of != of {
                                key_types.clear();
                                key_types_of = of;
                            }
                            let t = *key_types.entry(target).or_insert_with(|| {
                                self.get_type_of_property_of_type(target, key_property.name())
                            });
                            if let Some(t) = t
                                && is_unit_type(t)
                                && Some(self.get_regular_type_of_literal_type(t))
                                    != key_property_type
                            {
                                continue;
                            }
                        }
                        if (source == self.empty_object_type
                            || source == self.unknown_empty_object_type)
                            && target.symbol().is_some()
                            && self.is_empty_anonymous_object_type(target)
                        {
                            continue;
                        }
                        let relation = self.strict_subtype_relation;
                        if self.is_type_related_to(source, target, relation)
                            && (!Self::get_target_type(source)
                                .unwrap()
                                .object_flags()
                                .intersects(ObjectFlags::Class)
                                || !Self::get_target_type(target)
                                    .unwrap()
                                    .object_flags()
                                    .intersects(ObjectFlags::Class)
                                || self.is_type_derived_from(source, target))
                        {
                            types.remove(i);
                            break;
                        }
                    }
                }
            }
        }
        if trace {
            crate::uniontrace::report(self, input, &types, "reduced", Some(count), checkpoint);
        }
        self.subtype_reduction_cache.insert(key, alloc_slice(&types));
        Some(types)
    }

    // checker.go:26494
    pub(crate) fn intersect_types(
        &mut self,
        type1: Option<P<Type>>,
        type2: Option<P<Type>>,
    ) -> Option<P<Type>> {
        match (type1, type2) {
            (None, _) => type2,
            (_, None) => type1,
            (Some(type1), Some(type2)) => Some(self.get_intersection_type(&[type1, type2])),
        }
    }

    // We normalize combinations of intersection and union types based on the distributive property of the '&'
    // operator. Specifically, because X & (A | B) is equivalent to X & A | X & B, we can transform intersection
    // types with union type constituents into equivalent union types with intersection type constituents and
    // effectively ensure that union types are always at the top level in type representations.
    //
    // We do not perform structural deduplication on intersection types. Intersection types are created only by the &
    // type operator and we can't reduce those because we want to support recursive intersection types. For example,
    // a type alias of the form "type List<T> = T & { next: List<T> }" cannot be reduced during its declaration.
    // Also, unlike union types, the order of the constituent types is preserved in order that overload resolution
    // for intersections of types with signatures can be deterministic.
    // checker.go:26522
    pub(crate) fn get_intersection_type(&mut self, types: &[P<Type>]) -> P<Type> {
        self.get_intersection_type_ex(types, IntersectionFlags::None, AliasArg::None /*alias*/)
    }

    // checker.go:26526
    pub(crate) fn get_intersection_type_ex(
        &mut self,
        types: &[P<Type>],
        flags: IntersectionFlags,
        alias: AliasArg<'_>,
    ) -> P<Type> {
        let mut type_set = self.free_type_lists.pop().unwrap_or_default();
        let result = self.get_intersection_type_worker(types, flags, alias, &mut type_set);
        self.free_type_list(type_set);
        result
    }

    /// `get_intersection_type_ex` with the type set built in `type_set` (an empty buffer).
    fn get_intersection_type_worker(
        &mut self,
        types: &[P<Type>],
        flags: IntersectionFlags,
        alias: AliasArg<'_>,
        type_set: &mut Vec<P<Type>>,
    ) -> P<Type> {
        type_set.reserve(types.len());
        let mut ordered_types: CheckerOrderedSet<P<Type>> =
            CheckerOrderedSet { values_by_key: None, values: std::mem::take(type_set) };
        let includes =
            self.add_types_to_intersection(&mut ordered_types, TypeFlags::empty(), types);
        *type_set = ordered_types.values;
        let mut object_flags = ObjectFlags::None;
        // An intersection type is considered empty if it contains
        // the type never, or
        // more than one unit type or,
        // an object type and a nullable type (null or undefined), or
        // a string-like type and a type known to be non-string-like, or
        // a number-like type and a type known to be non-number-like, or
        // a symbol-like type and a type known to be non-symbol-like, or
        // a void-like type and a type known to be non-void-like, or
        // a non-primitive type and a type known to be primitive.
        if includes.intersects(TypeFlags::Never) {
            if type_set.contains(&self.silent_never_type) {
                return self.silent_never_type;
            }
            return self.never_type;
        }
        if self.strict_null_checks
            && includes.intersects(TypeFlags::Nullable)
            && includes.intersects(
                TypeFlags::Object | TypeFlags::NonPrimitive | TypeFlags::IncludesEmptyObject,
            )
            || includes.intersects(TypeFlags::NonPrimitive)
                && includes.intersects(TypeFlags::DisjointDomains & !TypeFlags::NonPrimitive)
            || includes.intersects(TypeFlags::StringLike)
                && includes.intersects(TypeFlags::DisjointDomains & !TypeFlags::StringLike)
            || includes.intersects(TypeFlags::NumberLike)
                && includes.intersects(TypeFlags::DisjointDomains & !TypeFlags::NumberLike)
            || includes.intersects(TypeFlags::BigIntLike)
                && includes.intersects(TypeFlags::DisjointDomains & !TypeFlags::BigIntLike)
            || includes.intersects(TypeFlags::ESSymbolLike)
                && includes.intersects(TypeFlags::DisjointDomains & !TypeFlags::ESSymbolLike)
            || includes.intersects(TypeFlags::VoidLike)
                && includes.intersects(TypeFlags::DisjointDomains & !TypeFlags::VoidLike)
        {
            return self.never_type;
        }
        if includes.intersects(TypeFlags::TemplateLiteral | TypeFlags::StringMapping)
            && includes.intersects(TypeFlags::StringLiteral)
        {
            let (ts, is_empty_set) = self.extract_redundant_template_literals(type_set);
            *type_set = ts;
            if is_empty_set {
                return self.never_type;
            }
        }
        if includes.intersects(TypeFlags::Any) {
            if includes.intersects(TypeFlags::IncludesWildcard) {
                return self.wildcard_type;
            } else if includes.intersects(TypeFlags::IncludesError) {
                return self.error_type;
            }
            return self.any_type;
        }
        if !self.strict_null_checks && includes.intersects(TypeFlags::Nullable) {
            if includes.intersects(TypeFlags::IncludesEmptyObject) {
                return self.never_type;
            } else if includes.intersects(TypeFlags::Undefined) {
                return self.undefined_type;
            }
            return self.null_type;
        }
        if (includes.intersects(TypeFlags::String)
            && includes.intersects(
                TypeFlags::StringLiteral | TypeFlags::TemplateLiteral | TypeFlags::StringMapping,
            )
            || includes.intersects(TypeFlags::Number)
                && includes.intersects(TypeFlags::NumberLiteral)
            || includes.intersects(TypeFlags::BigInt)
                && includes.intersects(TypeFlags::BigIntLiteral)
            || includes.intersects(TypeFlags::ESSymbol)
                && includes.intersects(TypeFlags::UniqueESSymbol)
            || includes.intersects(TypeFlags::Void) && includes.intersects(TypeFlags::Undefined)
            || includes.intersects(TypeFlags::IncludesEmptyObject)
                && includes.intersects(TypeFlags::DefinitelyNonNullable))
            && !flags.intersects(IntersectionFlags::NoSupertypeReduction)
        {
            self.remove_redundant_supertypes(type_set, includes);
        }
        if includes.intersects(TypeFlags::IncludesMissingType) {
            let undefined_type = self.undefined_type;
            let index = type_set.iter().position(|&t| t == undefined_type).unwrap();
            type_set[index] = self.missing_type;
        }
        if type_set.is_empty() {
            return self.unknown_type;
        }
        if type_set.len() == 1 {
            return type_set[0];
        }
        if type_set.len() == 2 && !flags.intersects(IntersectionFlags::NoConstraintReduction) {
            let type_var_index =
                usize::from(!type_set[0].flags().intersects(TypeFlags::TypeVariable));
            let type_variable = type_set[type_var_index];
            let primitive_type = type_set[1 - type_var_index];
            if type_variable.flags().intersects(TypeFlags::TypeVariable)
                && (primitive_type
                    .flags()
                    .intersects(TypeFlags::Primitive | TypeFlags::NonPrimitive)
                    && !self.is_generic_string_like_type(primitive_type)
                    || includes.intersects(TypeFlags::IncludesEmptyObject))
            {
                // We have an intersection T & P or P & T, where T is a type variable and P is a primitive type, the object type, or {}.
                let constraint = self.get_base_constraint_of_type(type_variable);
                // Check that T's constraint is similarly composed of primitive types, the object type, or {}.
                if let Some(constraint) = constraint.filter(|&constraint| {
                    every_type(
                        self,
                        constraint,
                        super::checker::Checker::is_primitive_or_object_or_empty_type,
                    )
                }) {
                    // If T's constraint is a subtype of P, simply return T. For example, given `T extends "a" | "b"`,
                    // the intersection `T & string` reduces to just T.
                    if self.is_type_strict_subtype_of(constraint, primitive_type) {
                        return type_variable;
                    }
                    if !(constraint.flags().intersects(TypeFlags::Union)
                        && some_type(self, constraint, |c, n| {
                            !crate::relater_1::are_different_plain_literals(n, primitive_type)
                                && c.is_type_strict_subtype_of(n, primitive_type)
                        }))
                    {
                        // No constituent of T's constraint is a subtype of P. If P is also not a subtype of T's constraint,
                        // then the constraint and P are unrelated, and the intersection reduces to never. For example, given
                        // `T extends "a" | "b"`, the intersection `T & number` reduces to never.
                        if !self.is_type_strict_subtype_of(primitive_type, constraint) {
                            return self.never_type;
                        }
                    }
                    // Some constituent of T's constraint is a subtype of P, or P is a subtype of T's constraint. Thus,
                    // the intersection further constrains the type variable. For example, given `T extends string | number`,
                    // the intersection `T & "a"` is marked as a constrained type variable. Likewise, given `T extends "a" | 1`,
                    // the intersection `T & number` is marked as a constrained type variable.
                    object_flags = ObjectFlags::IsConstrainedTypeVariable;
                }
            }
        }
        let key = get_intersection_key(type_set, flags, alias);
        let mut result = self.intersection_types.get(&key);
        if result.is_none() {
            let too_complex_before = self.too_complex_reports;
            let r;
            if includes.intersects(TypeFlags::Union) {
                let (ts, reduced) = self.intersect_unions_of_primitive_types(type_set);
                *type_set = ts;
                if reduced {
                    // When the intersection creates a reduced set (which might mean that *all* union types have
                    // disappeared), we restart the operation to get a new set of combined flags. Once we have
                    // reduced we'll never reduce again, so this occurs at most once.
                    r = self.get_intersection_type_ex(type_set, flags, alias);
                } else if type_set.iter().all(|&t| is_union_with_undefined(t)) {
                    let mut contained_undefined_type = self.undefined_type;
                    if type_set.iter().any(|&t| self.contains_missing_type(t)) {
                        contained_undefined_type = self.missing_type;
                    }
                    self.filter_types(type_set, |_, t| is_not_undefined_type(t));
                    let intersection = self.get_intersection_type_ex(
                        type_set,
                        flags,
                        AliasArg::None, /*alias*/
                    );
                    r = self.get_union_type_ex(
                        &[intersection, contained_undefined_type],
                        UnionReduction::Literal,
                        alias,
                        None, /*origin*/
                    );
                } else if type_set.iter().all(|&t| is_union_with_null(t)) {
                    self.filter_types(type_set, |_, t| is_not_null_type(t));
                    let intersection = self.get_intersection_type_ex(
                        type_set,
                        flags,
                        AliasArg::None, /*alias*/
                    );
                    let null_type = self.null_type;
                    r = self.get_union_type_ex(
                        &[intersection, null_type],
                        UnionReduction::Literal,
                        alias,
                        None, /*origin*/
                    );
                } else if type_set.len() >= 3 && types.len() > 2 {
                    // When we have three or more constituents, more than two inputs (to head off infinite reexpansion), some of which are unions, we employ a "divide and conquer" strategy
                    // where A & B & C & D is processed as (A & B) & (C & D). Since intersections of unions often produce far smaller
                    // unions of intersections than the full cartesian product (due to some intersections becoming `never`), this can
                    // dramatically reduce the overall work.
                    let middle = type_set.len() / 2;
                    let left = self.get_intersection_type_ex(
                        &type_set[..middle],
                        flags,
                        AliasArg::None, /*alias*/
                    );
                    let right = self.get_intersection_type_ex(
                        &type_set[middle..],
                        flags,
                        AliasArg::None, /*alias*/
                    );
                    r = self.get_intersection_type_ex(&[left, right], flags, alias);
                } else {
                    // We are attempting to construct a type of the form X & (A | B) & (C | D). Transform this into a type of
                    // the form X & A & C | X & A & D | X & B & C | X & B & D. If the estimated size of the resulting union type
                    // exceeds 100000 constituents, report an error.
                    if !self.check_cross_product_union(type_set, Self::too_complex_key(&key)) {
                        return self.error_type;
                    }
                    let constituents = self.get_cross_product_intersections(type_set, flags);
                    // We attach a denormalized origin type when at least one constituent of the cross-product union is an
                    // intersection (i.e. when the intersection didn't just reduce one or more unions to smaller unions) and
                    // the denormalized origin has fewer constituents than the union itself.
                    let mut origin = None;
                    if constituents.iter().any(|&t| is_intersection_type(t))
                        && get_constituent_count_of_types(&constituents)
                            > get_constituent_count_of_types(type_set)
                    {
                        origin = Some(self.new_intersection_type(ObjectFlags::None, type_set));
                    }
                    r = self.get_union_type_ex(
                        &constituents,
                        UnionReduction::Literal,
                        alias,
                        origin,
                    );
                }
            } else {
                let propagated = Self::get_propagating_flags_of_types(
                    types,
                    TypeFlags::Nullable, /*excludeKinds*/
                );
                r = self.new_intersection_type(object_flags | propagated, type_set);
                r.set_alias(alias.alias());
            }
            // tsrs-only: not cached when a TS2590 was reported below (a split, a restart or an inner intersection
            // returned the error type; `too_complex_since`).
            if !self.too_complex_since(too_complex_before) {
                self.intersection_types.insert(key, r);
            }
            result = Some(r);
        }
        result.unwrap()
    }
}

/// Go's `stack` of `getResolvedBaseConstraint`: the recursion identities of the enclosing calls, as a list through
/// their frames (Go appends to a slice; this needs no copy per level).
#[derive(Clone, Copy, Default)]
pub struct BaseConstraintStack<'a> {
    len: usize,
    top: Option<&'a (RecursionId, BaseConstraintStack<'a>)>,
}

impl BaseConstraintStack<'_> {
    fn contains(self, identity: RecursionId) -> bool {
        let mut at = self.top;
        while let Some((id, rest)) = at {
            if *id == identity {
                return true;
            }
            at = rest.top;
        }
        false
    }
}

// checker.go:26695
fn is_union_with_undefined(t: P<Type>) -> bool {
    t.flags().intersects(TypeFlags::Union) && t.types()[0].flags().intersects(TypeFlags::Undefined)
}

// checker.go:26699
fn is_union_with_null(t: P<Type>) -> bool {
    t.flags().intersects(TypeFlags::Union)
        && (t.types()[0].flags().intersects(TypeFlags::Null)
            || t.types()[1].flags().intersects(TypeFlags::Null))
}

// checker.go:26703
fn is_intersection_type(t: P<Type>) -> bool {
    t.flags().intersects(TypeFlags::Intersection)
}

// checker.go:26707
fn is_primitive_union(t: P<Type>) -> bool {
    t.object_flags().intersects(ObjectFlags::PrimitiveUnion)
}

// checker.go:26711
fn is_not_undefined_type(t: P<Type>) -> bool {
    !t.flags().intersects(TypeFlags::Undefined)
}

// checker.go:26715
fn is_not_null_type(t: P<Type>) -> bool {
    !t.flags().intersects(TypeFlags::Null)
}

impl Checker {
    // Add the given types to the given type set. Order is preserved, freshness is removed from literal
    // types, duplicates are removed, and nested types of the given kind are flattened into the set.
    // checker.go:26721
    fn add_types_to_intersection(
        &mut self,
        type_set: &mut CheckerOrderedSet<P<Type>>,
        includes: TypeFlags,
        types: &[P<Type>],
    ) -> TypeFlags {
        let mut includes = includes;
        for &t in types {
            let regular = self.get_regular_type_of_literal_type(t);
            includes = self.add_type_to_intersection(type_set, includes, regular);
        }
        includes
    }

    // checker.go:26728
    fn add_type_to_intersection(
        &mut self,
        type_set: &mut CheckerOrderedSet<P<Type>>,
        includes: TypeFlags,
        t: P<Type>,
    ) -> TypeFlags {
        let mut includes = includes;
        let mut t = t;
        let flags = t.flags();
        if flags.intersects(TypeFlags::Intersection) {
            return self.add_types_to_intersection(type_set, includes, t.types());
        }
        if self.is_empty_anonymous_object_type(t) {
            if !includes.intersects(TypeFlags::IncludesEmptyObject) {
                includes |= TypeFlags::IncludesEmptyObject;
                type_set.add(t);
            }
        } else {
            if flags.intersects(TypeFlags::AnyOrUnknown) {
                if t == self.wildcard_type {
                    includes |= TypeFlags::IncludesWildcard;
                }
                if self.is_error_type(t) {
                    includes |= TypeFlags::IncludesError;
                }
            } else if self.strict_null_checks || !flags.intersects(TypeFlags::Nullable) {
                if t == self.missing_type {
                    includes |= TypeFlags::IncludesMissingType;
                    t = self.undefined_type;
                }
                if !type_set.contains(t) {
                    if t.flags().intersects(TypeFlags::Unit) && includes.intersects(TypeFlags::Unit)
                    {
                        // We have seen two distinct unit types which means we should reduce to an
                        // empty intersection. Adding TypeFlags.NonPrimitive causes that to happen.
                        includes |= TypeFlags::NonPrimitive;
                    }
                    type_set.add(t);
                }
            }
            includes |= flags & TypeFlags::IncludesMask;
        }
        includes
    }

    // checker.go:26765
    fn remove_redundant_supertypes(&mut self, types: &mut Vec<P<Type>>, includes: TypeFlags) {
        let mut i = types.len();
        while i > 0 {
            i -= 1;
            let t = types[i];
            let remove = t.flags().intersects(TypeFlags::String)
                && includes.intersects(
                    TypeFlags::StringLiteral
                        | TypeFlags::TemplateLiteral
                        | TypeFlags::StringMapping,
                )
                || t.flags().intersects(TypeFlags::Number)
                    && includes.intersects(TypeFlags::NumberLiteral)
                || t.flags().intersects(TypeFlags::BigInt)
                    && includes.intersects(TypeFlags::BigIntLiteral)
                || t.flags().intersects(TypeFlags::ESSymbol)
                    && includes.intersects(TypeFlags::UniqueESSymbol)
                || t.flags().intersects(TypeFlags::Void)
                    && includes.intersects(TypeFlags::Undefined)
                || self.is_empty_anonymous_object_type(t)
                    && includes.intersects(TypeFlags::DefinitelyNonNullable);
            if remove {
                types.remove(i);
            }
        }
    }

    // Returns true if the intersection of the template literals and string literals is the empty set,
    // for example `get${string}` & "setX", and should reduce to never.
    // checker.go:26787
    fn extract_redundant_template_literals(&mut self, types: &[P<Type>]) -> (Vec<P<Type>>, bool) {
        let mut types = types.to_vec();
        let literals: Vec<P<Type>> = types
            .iter()
            .copied()
            .filter(|t| t.flags().intersects(TypeFlags::StringLiteral))
            .collect();
        let mut i = types.len();
        while i > 0 {
            i -= 1;
            let t = types[i];
            if !t.flags().intersects(TypeFlags::TemplateLiteral | TypeFlags::StringMapping) {
                continue;
            }
            for &t2 in &literals {
                if self.is_type_subtype_of(t2, t) {
                    // For example, `get${T}` & "getX" is just "getX", and Lowercase<string> & "foo" is just "foo"
                    types.remove(i);
                    break;
                }
                if self.is_pattern_literal_type(t) {
                    return (types, true);
                }
            }
        }
        (types, false)
    }

    // If the given list of types contains more than one union of primitive types, replace the
    // first with a union containing an intersection of those primitive types, then remove the
    // other unions and return true. Otherwise, do nothing and return false.
    // checker.go:26813
    fn intersect_unions_of_primitive_types(&mut self, types: &[P<Type>]) -> (Vec<P<Type>>, bool) {
        let mut types = types.to_vec();
        let Some(index) = types.iter().position(|&t| is_primitive_union(t)) else {
            return (types, false);
        };
        // Remove all but the first union of primitive types and collect them in
        // the unionTypes array.
        let mut i = index + 1;
        let mut union_types: Vec<P<Type>> = vec![types[index]];
        while i < types.len() {
            let t = types[i];
            if t.object_flags().intersects(ObjectFlags::PrimitiveUnion) {
                union_types.push(t);
                types.remove(i);
            } else {
                i += 1;
            }
        }
        // Return false if there was only one union of primitive types
        if union_types.len() == 1 {
            return (types, false);
        }
        // We have more than one union of primitive types, now intersect them. For each
        // type in each union we check if the type is matched in every union and if so
        // we include it in the result.
        let mut checked: Vec<P<Type>> = Vec::new();
        let mut result: Vec<P<Type>> = Vec::new();
        for &u in &union_types {
            for &t in u.types() {
                let inserted;
                (checked, inserted) = insert_type(self, &checked, t);
                if inserted && self.each_union_contains(&union_types, t) {
                    // undefinedType/missingType are always sorted first so we leverage that here
                    if t == self.undefined_type
                        && !result.is_empty()
                        && result[0] == self.missing_type
                    {
                        continue;
                    }
                    if t == self.missing_type
                        && !result.is_empty()
                        && result[0] == self.undefined_type
                    {
                        result[0] = self.missing_type;
                        continue;
                    }
                    result = insert_type(self, &result, t).0;
                }
            }
        }
        // Finally replace the first union with the result
        types[index] = self.get_union_type_from_sorted_list(
            &result,
            ObjectFlags::PrimitiveUnion,
            AliasArg::None, /*alias*/
            None,           /*origin*/
        );
        (types, true)
    }

    // Check that the given type has a match in every union. A given type is matched by
    // an identical type, and a literal type is additionally matched by its corresponding
    // primitive type, and missingType is matched by undefinedType (and vice versa).
    // checker.go:26866
    fn each_union_contains(&mut self, union_types: &[P<Type>], t: P<Type>) -> bool {
        for &u in union_types {
            if !self.union_contains_type(u, t, true /*matchSymbol*/) {
                return false;
            }
        }
        true
    }

    // checker.go:26875
    pub(crate) fn union_contains_type(
        &mut self,
        union: P<Type>,
        t: P<Type>,
        match_symbol: bool,
    ) -> bool {
        let types = union.types();
        if contains_type(self, types, t) {
            return true;
        }
        if t == self.missing_type {
            let undefined_type = self.undefined_type;
            return contains_type(self, types, undefined_type);
        }
        if t == self.undefined_type {
            let missing_type = self.missing_type;
            return contains_type(self, types, missing_type);
        }
        let mut primitive: Option<P<Type>> = None;
        if t.flags().intersects(TypeFlags::StringLiteral) {
            primitive = Some(self.string_type);
        } else if t.flags().intersects(TypeFlags::Enum | TypeFlags::NumberLiteral) {
            primitive = Some(self.number_type);
        } else if t.flags().intersects(TypeFlags::BigIntLiteral) {
            primitive = Some(self.bigint_type);
        } else if t.flags().intersects(TypeFlags::UniqueESSymbol) && match_symbol {
            primitive = Some(self.es_symbol_type);
        }
        match primitive {
            Some(primitive) => contains_type(self, types, primitive),
            None => false,
        }
    }

    // checker.go:26900
    fn get_cross_product_intersections(
        &mut self,
        types: &[P<Type>],
        flags: IntersectionFlags,
    ) -> Vec<P<Type>> {
        let count = Self::get_cross_product_union_size(types);
        let mut intersections: Vec<P<Type>> = Vec::new();
        // Every iteration overwrites each union position; the others keep `types[j]`.
        let mut constituents = types.to_vec();
        for i in 0..count {
            let mut n = i;
            for j in (0..types.len()).rev() {
                if types[j].flags().intersects(TypeFlags::Union) {
                    let source_types = types[j].types();
                    let length = source_types.len();
                    constituents[j] = source_types[n % length];
                    n /= length;
                }
            }
            let t =
                self.get_intersection_type_ex(&constituents, flags, AliasArg::None /*alias*/);
            if !t.flags().intersects(TypeFlags::Never) {
                intersections.push(t);
            }
        }
        intersections
    }
}

// checker.go:26922
fn get_constituent_count(t: P<Type>) -> usize {
    if !t.flags().intersects(TypeFlags::UnionOrIntersection) || t.alias().is_some() {
        return 1;
    }
    if t.flags().intersects(TypeFlags::Union)
        && let Some(origin) = t.as_union_type().origin()
    {
        return get_constituent_count(origin);
    }
    get_constituent_count_of_types(t.types())
}

// checker.go:26932
fn get_constituent_count_of_types(types: &[P<Type>]) -> usize {
    let mut n = 0;
    for &t in types {
        n += get_constituent_count(t);
    }
    n
}

impl Checker {
    // checker.go:26940
    fn filter_types(
        &mut self,
        types: &mut [P<Type>],
        mut predicate: impl FnMut(&mut Checker, P<Type>) -> bool,
    ) {
        for t in types {
            *t = self.filter_type(*t, &mut predicate);
        }
    }

    // checker.go:26946
    #[inline]
    pub fn is_empty_anonymous_object_type(&mut self, t: P<Type>) -> bool {
        t.object_flags().intersects(ObjectFlags::Anonymous)
            && self.is_empty_anonymous_object_type_worker(t)
    }

    #[inline(never)]
    fn is_empty_anonymous_object_type_worker(&mut self, t: P<Type>) -> bool {
        if t.object_flags_lazy().intersects(ObjectFlags::MembersResolved) {
            if self.is_empty_resolved_type(t.as_structured_type()) {
                return true;
            }
        } else {
            // Resolving members can change this answer on a later flow walk. Do not memoize
            // narrowing that depends on the unresolved member state.
            self.flow_memo.taint(0);
        }
        let Some(symbol) = t.symbol() else {
            return false;
        };
        if symbol.flags().intersects(SymbolFlags::TypeLiteral) {
            return self.get_members_of_symbol(symbol).map_or(0, |m| m.len()) == 0;
        }
        false
    }

    // isEmptyResolvedType(resolveStructuredTypeMembers(t)); notes/mem-lazy.md L11: a type with a lazy member table
    // (an instantiated reference, never anyFunctionType) is asked without resolving it.
    fn is_empty_structured_type(&mut self, t: P<Type>) -> bool {
        if self.lazy_empty
            && let Some(lm) = self.get_ready_lazy_member_table(t)
        {
            self.lazy_member_stats.empty_lazy_queries += 1;
            let ready = lm.ready.get().unwrap();
            return ready.call_signatures.get().is_empty()
                && ready.construct_signatures.get().is_empty()
                && ready.index_infos.get().is_empty()
                && !self.has_properties_of_structured_type(t);
        }
        let resolved = self.resolve_structured_type_members(t);
        self.is_empty_resolved_type(resolved)
    }

    // checker.go:26951
    fn is_empty_resolved_type(&self, t: &'static StructuredType) -> bool {
        !std::ptr::eq(t, self.any_function_type.as_structured_type())
            && t.properties().is_empty()
            && t.signatures().is_empty()
            && t.index_infos().is_empty()
    }

    // checker.go:26955
    pub(crate) fn is_empty_object_type(&mut self, t: P<Type>) -> bool {
        if t.flags().intersects(TypeFlags::Object) {
            if self.is_generic_mapped_type(t) {
                return false;
            }
            return self.is_empty_structured_type(t);
        } else if t.flags().intersects(TypeFlags::NonPrimitive) {
            return true;
        } else if t.flags().intersects(TypeFlags::Union) {
            return t.types().iter().any(|&t| self.is_empty_object_type(t));
        } else if t.flags().intersects(TypeFlags::Intersection) {
            return t.types().iter().all(|&t| self.is_empty_object_type(t));
        }
        false
    }

    // checker.go:26969
    pub(crate) fn is_pattern_literal_placeholder_type(&mut self, t: P<Type>) -> bool {
        if t.flags().intersects(TypeFlags::Intersection) {
            // Return true if the intersection consists of one or more placeholders and zero or
            // more object type tags.
            let mut seen_placeholder = false;
            for &s in t.types() {
                if s.flags().intersects(TypeFlags::Literal | TypeFlags::Nullable)
                    || self.is_pattern_literal_placeholder_type(s)
                {
                    seen_placeholder = true;
                } else if !s.flags().intersects(TypeFlags::Object) {
                    return false;
                }
            }
            return seen_placeholder;
        }
        t.flags()
            .intersects(TypeFlags::Any | TypeFlags::String | TypeFlags::Number | TypeFlags::BigInt)
            || self.is_pattern_literal_type(t)
    }

    // checker.go:26986
    pub(crate) fn is_pattern_literal_type(&mut self, t: P<Type>) -> bool {
        // A pattern literal type is a template literal or a string mapping type that contains only
        // non-generic pattern literal placeholders.
        t.flags().intersects(TypeFlags::TemplateLiteral)
            && t.as_template_literal_type()
                .types()
                .iter()
                .all(|&t| self.is_pattern_literal_placeholder_type(t))
            || t.flags().intersects(TypeFlags::StringMapping)
                && self.is_pattern_literal_placeholder_type(t.target().unwrap())
    }

    // checker.go:26993
    pub(crate) fn is_generic_string_like_type(&mut self, t: P<Type>) -> bool {
        t.flags().intersects(TypeFlags::TemplateLiteral | TypeFlags::StringMapping)
            && !self.is_pattern_literal_type(t)
    }
}

// checker.go:26997
// The callbacks of these Go free functions receive the checker (PORTING.md callback rule) so that callers never
// capture `self`; `c` is only passed through.
pub(crate) fn for_each_type(c: &mut Checker, t: P<Type>, mut f: impl FnMut(&mut Checker, P<Type>)) {
    if t.flags().intersects(TypeFlags::Union) {
        for &u in t.types() {
            f(c, u);
        }
    } else {
        f(c, t);
    }
}

// checker.go:27007
pub(crate) fn some_type(
    c: &mut Checker,
    t: P<Type>,
    mut f: impl FnMut(&mut Checker, P<Type>) -> bool,
) -> bool {
    if t.flags().intersects(TypeFlags::Union) {
        return t.types().iter().any(|&t| f(c, t));
    }
    f(c, t)
}

// checker.go:27014
pub(crate) fn every_type(
    c: &mut Checker,
    t: P<Type>,
    mut f: impl FnMut(&mut Checker, P<Type>) -> bool,
) -> bool {
    if t.flags().intersects(TypeFlags::Union) {
        return t.types().iter().all(|&t| f(c, t));
    }
    f(c, t)
}

// checker.go:27021
pub(crate) fn every_contained_type(
    c: &mut Checker,
    t: P<Type>,
    mut f: impl FnMut(&mut Checker, P<Type>) -> bool,
) -> bool {
    if t.flags().intersects(TypeFlags::UnionOrIntersection) {
        return t.types().iter().all(|&t| f(c, t));
    }
    f(c, t)
}

impl Checker {
    // checker.go:27028
    pub(crate) fn filter_type(
        &mut self,
        t: P<Type>,
        mut f: impl FnMut(&mut Checker, P<Type>) -> bool,
    ) -> P<Type> {
        if t.flags().intersects(TypeFlags::Union) {
            let types = t.types();
            let filtered: SmallVec<[P<Type>; 4]> =
                types.iter().copied().filter(|&u| f(self, u)).collect();
            // core.Same(types, filtered): core.Filter returns its input when nothing was removed.
            if filtered.len() == types.len() {
                return t;
            }
            let origin = t.as_union_type().origin();
            let mut new_origin: Option<P<Type>> = None;
            if let Some(origin) = origin.filter(|o| o.flags().intersects(TypeFlags::Union)) {
                // If the origin type is a (denormalized) union type, filter its non-union constituents. If that ends
                // up removing a smaller number of types than in the normalized constituent set (meaning some of the
                // filtered types are within nested unions in the origin), then we can't construct a new origin type.
                // Otherwise, if we have exactly one type left in the origin set, return that as the filtered type.
                // Otherwise, construct a new filtered origin type.
                let origin_types = origin.types();
                let origin_filtered: SmallVec<[P<Type>; 4]> = origin_types
                    .iter()
                    .copied()
                    .filter(|&u| u.flags().intersects(TypeFlags::Union) || f(self, u))
                    .collect();
                if origin_types.len() - origin_filtered.len() == types.len() - filtered.len() {
                    if origin_filtered.len() == 1 {
                        return origin_filtered[0];
                    }
                    new_origin = Some(self.new_union_type(ObjectFlags::None, &origin_filtered));
                }
            }
            // filtering could remove intersections so `ContainsIntersections` might be forwarded "incorrectly"
            // it is purely an optimization hint so there is no harm in accidentally forwarding it
            return self.get_union_type_from_sorted_list(
                &filtered,
                t.object_flags()
                    & (ObjectFlags::PrimitiveUnion | ObjectFlags::ContainsIntersections),
                AliasArg::None, /*alias*/
                new_origin,
            );
        }
        if t.flags().intersects(TypeFlags::Never) || f(self, t) {
            return t;
        }
        self.never_type
    }

    // checker.go:27064
    pub(crate) fn remove_type(&mut self, t: P<Type>, target_type: P<Type>) -> P<Type> {
        if !t.flags().intersects(TypeFlags::Union) {
            if t == target_type {
                return self.never_type;
            }
            return t;
        }
        if let Some(origin) = t.as_union_type().origin()
            && origin.flags().intersects(TypeFlags::Union)
            && contains_type(self, origin.types(), target_type)
        {
            return self.filter_type(t, |_, t| t != target_type);
        }
        let types = t.types();
        if let (i, true) =
            tsrs_core::goslices::binary_search_func(types, &target_type, |&probe, &target| {
                compare_types(self, Some(probe), Some(target))
            })
        {
            if types.len() == 2 {
                return types[1 - i];
            }
            // Remove the target type from the slice.
            let mut filtered: Vec<P<Type>> = types[..i].to_vec();
            filtered.extend_from_slice(&types[i + 1..]);
            return self.get_union_type_from_sorted_list(
                &filtered,
                t.object_flags()
                    & (ObjectFlags::PrimitiveUnion | ObjectFlags::ContainsIntersections),
                AliasArg::None, /*alias*/
                None,           /*origin*/
            );
        }
        t
    }
}

/// Sorts `types` by `compare_types` when it consists of ascending runs starting at 0 and at each of `run_starts`
/// (`add_types_to_union`): one type next to a run is inserted by binary search, other runs are merged pairwise, which
/// takes at most n comparisons per round and ceil(log2 runs) rounds. Requires `compare_types` to be a total order in
/// which only identical types compare equal; a type in several runs ends up repeated next to itself.
fn merge_ascending_runs(c: &mut Checker, types: &mut Vec<P<Type>>, run_starts: &[usize]) {
    let n = types.len();
    if let [mid] = *run_starts
        && (mid == 1 || mid == n - 1)
    {
        let t = types.remove(if mid == 1 { 0 } else { n - 1 });
        let (i, _) = tsrs_core::goslices::binary_search_func(types, &t, |&probe, &t| {
            compare_types(c, Some(probe), Some(t))
        });
        types.insert(i, t);
        return;
    }
    let mut bounds: SmallVec<[usize; 4]> = SmallVec::with_capacity(run_starts.len() + 2);
    bounds.push(0);
    bounds.extend_from_slice(run_starts);
    bounds.push(n);
    let mut src = std::mem::take(types);
    let mut dst: Vec<P<Type>> = Vec::with_capacity(n);
    while bounds.len() > 2 {
        let runs = bounds.len() - 1;
        let mut next: SmallVec<[usize; 4]> = SmallVec::with_capacity(runs / 2 + 2);
        next.push(0);
        dst.clear();
        let mut j = 0;
        while j < runs {
            if j + 1 < runs {
                merge_sorted_types(
                    c,
                    &src[bounds[j]..bounds[j + 1]],
                    &src[bounds[j + 1]..bounds[j + 2]],
                    false, /*dedup*/
                    &mut |_, t| dst.push(t),
                );
                next.push(bounds[j + 2]);
                j += 2;
            } else {
                dst.extend_from_slice(&src[bounds[j]..bounds[j + 1]]);
                next.push(bounds[j + 1]);
                j += 1;
            }
        }
        std::mem::swap(&mut src, &mut dst);
        bounds = next;
    }
    *types = src;
}

/// Merges two runs sorted by `compare_types` (a total order in which only identical types compare equal), passing
/// each type to `out` in order; a type in both runs is passed once with `dedup`, else twice. When one run is much
/// shorter, each of its types is placed by binary search in the other (m log n comparisons instead of up to n + m):
/// a small union added to a large one.
fn merge_sorted_types(
    c: &mut Checker,
    mut left: &[P<Type>],
    mut right: &[P<Type>],
    dedup: bool,
    out: &mut impl FnMut(&mut Checker, P<Type>),
) {
    let (short, long) = (left.len().min(right.len()), left.len().max(right.len()));
    if short > 0 && short * long.bit_width() as usize * 2 < long {
        let left_is_long = left.len() >= right.len();
        let (mut long_run, short_run) = if left_is_long { (left, right) } else { (right, left) };
        for &t in short_run {
            let i =
                long_run.partition_point(|&x| x != t && compare_types(c, Some(x), Some(t)).is_lt());
            for &x in &long_run[..i] {
                out(c, x);
            }
            long_run = &long_run[i..];
            out(c, t);
            if long_run.first() == Some(&t) {
                long_run = &long_run[1..];
                if !dedup {
                    out(c, t);
                }
            }
        }
        for &x in long_run {
            out(c, x);
        }
        return;
    }
    while let (Some(&l), Some(&r)) = (left.first(), right.first()) {
        if l == r {
            out(c, l);
            if !dedup {
                out(c, r);
            }
            left = &left[1..];
            right = &right[1..];
        } else if compare_types(c, Some(l), Some(r)).is_lt() {
            out(c, l);
            left = &left[1..];
        } else {
            out(c, r);
            right = &right[1..];
        }
    }
    for &t in left.iter().chain(right) {
        out(c, t);
    }
}

// Go's containsType/insertType call CompareTypes, which needs the checker (Type has no checker back pointer),
// so they take `c` like `compare_types`.
/// `maybe_type_of_kind` of any of `types`.
/// A loop rather than `any` with a closure: the recursive closure was not inlined, a call per constituent.
/// Rechecked in notes/perf-iterator-regressions.md; keep this loop when refactoring iterator use.
fn maybe_constituent_of_kind(types: &[P<Type>], kind: TypeFlags) -> bool {
    for &t in types {
        let flags = t.flags();
        if flags.intersects(kind) {
            return true;
        }
        if flags.intersects(TypeFlags::UnionOrIntersection)
            && maybe_constituent_of_kind(t.types(), kind)
        {
            return true;
        }
    }
    false
}

// checker.go:27086
pub(crate) fn contains_type(c: &mut Checker, types: &[P<Type>], t: P<Type>) -> bool {
    // tsrs-only (can1357's 628f51ceb): small lists often hold the very type asked for, found without comparing
    // structure; a miss still searches. Only while the binary search would find it too (a total order).
    if types.len() <= 8 && types.contains(&t) && c.program.source_files_complete() {
        return true;
    }
    tsrs_core::goslices::binary_search_func(types, &t, |&probe, &t| {
        compare_types(c, Some(probe), Some(t))
    })
    .1
}

/// `contains_type(types, regular)` for the regular type of the fresh literal `types[i]`. tsrs-only: the two differ
/// only in their ids, so in the sorted list the regular type, when present, is a neighbor; the binary search runs
/// only when neither neighbor is it. Same answers as `contains_type` (a neighbor that is the type is in the list).
fn contains_regular_literal_type(
    c: &mut Checker,
    types: &[P<Type>],
    i: usize,
    regular: P<Type>,
) -> bool {
    if (i > 0 && types[i - 1] == regular) || types.get(i + 1) == Some(&regular) {
        return true;
    }
    contains_type(c, types, regular)
}

/// Whether every type of the sorted, unique `source` is in the sorted, unique `target`. tsrs-only: `contains_type` for each source type binary-searches the target
/// with `compare_types`; in sorted lists each source type can only be found after the previous one's position, so a
/// forward scan by identity answers the same with no comparisons. Used when the scan is the cheaper of the two.
pub(crate) fn is_sorted_subset(source: &[P<Type>], target: &[P<Type>]) -> bool {
    let mut rest = target;
    !(source.iter().any(|&t| {
        match rest.iter().position(|&u| u == t) {
            Some(k) => rest = &rest[k + 1..],
            None => return true,
        }

        false
    }))
}

// checker.go:27091
fn insert_type(c: &mut Checker, types: &[P<Type>], t: P<Type>) -> (Vec<P<Type>>, bool) {
    if let (i, false) = tsrs_core::goslices::binary_search_func(types, &t, |&probe, &t| {
        compare_types(c, Some(probe), Some(t))
    }) {
        let mut result = types.to_vec();
        result.insert(i, t);
        return (result, true);
    }
    (types.to_vec(), false)
}

// checker.go:27098
pub(crate) fn count_types(t: P<Type>) -> usize {
    if t.flags().intersects(TypeFlags::Union) {
        return t.types().len();
    } else if t.flags().intersects(TypeFlags::Never) {
        return 0;
    }
    1
}

impl Checker {
    // checker.go:27108
    pub(crate) fn is_error_type(&self, t: P<Type>) -> bool {
        // The only 'any' types that have alias symbols are those manufactured by getTypeFromTypeAliasReference for
        // a reference to an unresolved symbol. We want those to behave like the errorType.
        t == self.error_type || t.flags().intersects(TypeFlags::Any) && t.alias().is_some()
    }
}

impl Checker {
    // checker.go:27118
    /// `key` identifies the evaluation for `report_too_complex` (tsrs-only): the caller's cache key where it has one,
    /// so that what Go's cache would have told apart (an aliased and an alias-free intersection) is told apart here.
    pub(crate) fn check_cross_product_union(&mut self, types: &[P<Type>], key: u64) -> bool {
        let size = Self::get_cross_product_union_size(types);
        // tsrs-only: TSRS_TRACE_UNION_REDUCTION (uniontrace.rs).
        if size >= 1_000 && crate::uniontrace::enabled() {
            crate::uniontrace::report_cross_product(self, types, size);
        }
        if size >= 100_000 {
            self.report_too_complex(key);
            return false;
        }
        true
    }

    /// TS2590 at `current_node` for the too-complex evaluation `key` (`too_complex_key`), counted for
    /// `too_complex_since`.
    fn report_too_complex(&mut self, key: u64) {
        self.too_complex_reports = self.too_complex_reports.wrapping_add(1);
        match self.current_node {
            // tsrs-only: each file reports a too-complex type once, at the first site in it that evaluated the type,
            // whichever file's check that evaluation happened in (resolving a declaration of file A while checking
            // file B reports at the site in A; A's later sites are then quiet). While a file is being checked the
            // report is emitted when the file is done (`flush_too_complex_reports`); outside a file check (the
            // language server asking for a type) it is emitted at once. Go reports at once, every time.
            Some(node) if !tsrs_core::compat::go_compatible_history() => {
                let reported_before = ast::get_source_file_of_node(node)
                    .is_some_and(|file| !self.too_complex_reported.insert((file, key)));
                if reported_before {
                } else if self.checking_file.is_some() {
                    self.too_complex_nodes.push(node);
                } else {
                    self.error(Some(node), &diagnostics::EXPRESSION_PRODUCES_A_UNION_TYPE_THAT_IS_TOO_COMPLEX_TO_REPRESENT, &[]);
                }
            }
            location => {
                self.error(
                    location,
                    &diagnostics::EXPRESSION_PRODUCES_A_UNION_TYPE_THAT_IS_TOO_COMPLEX_TO_REPRESENT,
                    &[],
                );
            }
        }
    }

    /// The identity of a too-complex evaluation within one checker: a hash of the caller's cache key (the alias-qualified
    /// intersection key) or of the types it combined (the union being reduced; the operands of a cross product that has
    /// no cache of its own).
    pub(crate) fn too_complex_key<K: std::hash::Hash + ?Sized>(key: &K) -> u64 {
        use std::hash::Hasher;
        let mut h = rustc_hash::FxHasher::default();
        key.hash(&mut h);
        h.finish()
    }

    /// tsrs-only: emits the TS2590 reports recorded since the last flush. Since `too_complex_since` keeps such results
    /// out of the caches, every expression that evaluates the type hits the limit again, and `report_too_complex`
    /// keeps the first site per file and type: Go, whose caches hide the later evaluations, reports the first
    /// evaluation per checker; this reports the first per file. Called at the end of `check_source_file` and
    /// `check_source_file_piece`.
    pub(crate) fn flush_too_complex_reports(&mut self) {
        for node in std::mem::take(&mut self.too_complex_nodes) {
            self.error(
                Some(node),
                &diagnostics::EXPRESSION_PRODUCES_A_UNION_TYPE_THAT_IS_TOO_COMPLEX_TO_REPRESENT,
                &[],
            );
        }
    }

    /// tsrs-only: whether a computation that began when `too_complex_reports` was `before` reported TS2590, so that
    /// its result stays out of the caches. Neither TS2590 site caches its own failure, but Go caches what is built
    /// from the error type it returns: an intersection split in halves or restarted, a union of two unions, an
    /// instantiation, a relation compared through it. A later request on the same checker then gets that result
    /// without the error, so whether a file reports the TS2590 depends on what its checker evaluated before it, and
    /// under work stealing on timing (issue #218, notes/open-history-dependence.md section 4). By default such
    /// results are not cached: every evaluation computes them again, and each file reports once per type, at the first
    /// site (`report_too_complex`, `flush_too_complex_reports`). `go_compatible_history()` keeps Go's caches.
    pub(crate) fn too_complex_since(&self, before: u32) -> bool {
        self.too_complex_reports != before && !tsrs_core::compat::go_compatible_history()
    }

    // checker.go:27130
    fn get_cross_product_union_size(types: &[P<Type>]) -> usize {
        let mut size = 1usize;
        for &t in types {
            if t.flags().intersects(TypeFlags::Union) {
                let n = t.types().len();
                // Cap the result to avoid integer overflow when computing the cross product of many large unions.
                // In TypeScript, number overflow produces Infinity which naturally exceeds the limit check;
                // in Go, we must guard against int wrapping to zero or negative.
                size = size.saturating_mul(n);
            } else if t.flags().intersects(TypeFlags::Never) {
                return 0;
            }
        }
        size
    }

    // checker.go:27150
    pub(crate) fn get_index_type(&mut self, t: P<Type>) -> P<Type> {
        self.get_index_type_ex(t, IndexFlags::None)
    }

    // checker.go:27154
    #[cfg_attr(
        not(feature = "work-census"),
        inline(always),
        expect(
            clippy::inline_always,
            reason = "without the census the wrapper is a forwarding call; inlined, callers call the body as before (notes/perf-checker-algorithms.md)"
        )
    )]
    pub(crate) fn get_index_type_ex(&mut self, t: P<Type>, index_flags: IndexFlags) -> P<Type> {
        if self.census_on() {
            let span =
                self.census_begin(crate::workcensus::Cat::KeyOf, || crate::workcensus::CKey::None);
            let r = self.get_index_type_ex_inner(t, index_flags);
            let timing = self.census_end(span).unwrap();
            let n = if t.flags().intersects(TypeFlags::Union) { t.types().len() } else { 1 };
            let key = crate::workcensus::CKey::Bucket(crate::workcensus::bucket(n));
            self.census.as_mut().unwrap().record(
                crate::workcensus::Cat::KeyOf,
                key,
                timing,
                n as u64,
                0,
                0,
            );
            return r;
        }
        self.get_index_type_ex_inner(t, index_flags)
    }

    fn get_index_type_ex_inner(&mut self, t: P<Type>, index_flags: IndexFlags) -> P<Type> {
        let t = self.get_reduced_type(t);
        if Self::is_no_infer_type(t) {
            let index_type =
                self.get_index_type_ex(t.as_substitution_type().base_type().unwrap(), index_flags);
            return self.get_no_infer_type(index_type);
        } else if self.should_defer_index_type(t, index_flags) {
            return self.get_index_type_for_generic_type(t, index_flags);
        } else if t.flags().intersects(TypeFlags::Union) {
            let types: Vec<P<Type>> =
                t.types().iter().map(|&t| self.get_index_type_ex(t, index_flags)).collect();
            return self.get_intersection_type(&types);
        } else if t.flags().intersects(TypeFlags::Intersection) {
            let types: Vec<P<Type>> =
                t.types().iter().map(|&t| self.get_index_type_ex(t, index_flags)).collect();
            return self.get_union_type(&types);
        } else if t.object_flags().intersects(ObjectFlags::Mapped) {
            return self.get_index_type_for_mapped_type(t, index_flags);
        } else if t == self.wildcard_type {
            return self.wildcard_type;
        } else if t.flags().intersects(TypeFlags::Unknown) {
            return self.never_type;
        } else if t.flags().intersects(TypeFlags::Any | TypeFlags::Never) {
            return self.string_number_symbol_type;
        }
        let include = if_else(
            index_flags.intersects(IndexFlags::NoIndexSignatures),
            TypeFlags::StringLiteral,
            TypeFlags::StringLike,
        ) | if_else(
            index_flags.intersects(IndexFlags::StringsOnly),
            TypeFlags::None,
            TypeFlags::NumberLike | TypeFlags::ESSymbolLike,
        );
        self.get_literal_type_from_properties(t, include, index_flags == IndexFlags::None)
    }

    // checker.go:27179
    pub(crate) fn get_extract_string_type(&mut self, t: P<Type>) -> P<Type> {
        let extract_type_alias = self.get_global_extract_symbol();
        if let Some(extract_type_alias) = extract_type_alias {
            let string_type = self.string_type;
            return self.get_type_alias_instantiation(extract_type_alias, &[t, string_type], None);
        }
        self.string_type
    }

    // checker.go:27187
    fn get_literal_type_from_properties(
        &mut self,
        t: P<Type>,
        include: TypeFlags,
        include_origin: bool,
    ) -> P<Type> {
        let key = PropertiesTypesKey { type_id: t.id, include, include_origin };
        if let Some(cached) = self.properties_types.get(&key).copied() {
            return cached;
        }
        let mut origin: Option<P<Type>> = None;
        if include_origin
            && t.object_flags().intersects(ObjectFlags::ClassOrInterface | ObjectFlags::Reference)
            || t.alias().is_some()
        {
            origin = Some(self.new_index_type(t, IndexFlags::None));
        }
        let props = self.get_properties_of_type(t);
        let index_infos = self.get_index_infos_of_type(t);
        let mut types: Vec<P<Type>> = Vec::with_capacity(props.len() + index_infos.len());
        types.extend(
            props.iter().map(|&prop| self.get_literal_type_from_property(prop, include, false)),
        );
        types.extend(index_infos.iter().filter_map(|&info| {
            if info != self.enum_number_index_info
                && Self::is_key_type_included(info.key_type(), include)
            {
                if info.key_type() == self.string_type && include.intersects(TypeFlags::Number) {
                    Some(self.string_or_number_type)
                } else {
                    Some(info.key_type())
                }
            } else {
                None
            }
        }));
        let result =
            self.get_union_type_ex(&types, UnionReduction::Literal, AliasArg::None, origin);
        self.properties_types.insert(key, result);
        result
    }

    // checker.go:27216
    pub(crate) fn get_literal_type_from_property(
        &mut self,
        prop: P<Symbol>,
        include: TypeFlags,
        include_non_public: bool,
    ) -> P<Type> {
        if include_non_public
            || !get_declaration_modifier_flags_from_symbol(prop)
                .intersects(ModifierFlags::NonPublicAccessibilityModifier)
        {
            let late_bound = self.get_late_bound_symbol(prop);
            let mut t = self.value_symbol_links.get(late_bound).name_type();
            if t.is_none() {
                if prop.name() == INTERNAL_SYMBOL_NAME_DEFAULT {
                    t = Some(self.get_string_literal_type("default"));
                } else {
                    let name = get_name_of_declaration(prop.value_declaration());
                    if let Some(name) = name {
                        t = Some(self.get_literal_type_from_property_name(name));
                    }
                    if t.is_none() && !is_known_symbol(prop) {
                        t = Some(self.get_string_literal_type(symbol_name(prop)));
                    }
                }
            }
            if let Some(t) = t
                && t.flags().intersects(include)
            {
                return t;
            }
        }
        self.never_type
    }

    // checker.go:27239
    pub(crate) fn get_literal_type_from_property_name(&mut self, name: P<Node>) -> P<Type> {
        if is_private_identifier(name) {
            return self.never_type;
        }
        if is_numeric_literal(name) {
            let t = self.check_expression(name);
            return self.get_regular_type_of_literal_type(t);
        }
        if is_computed_property_name(name) {
            let t = self.check_computed_property_name(name);
            return self.get_regular_type_of_literal_type(t);
        }
        let property_name = get_property_name_for_property_name_node(name);
        if property_name != INTERNAL_SYMBOL_NAME_MISSING {
            return self.get_string_literal_type(&property_name);
        }
        if is_expression(name) {
            let t = self.check_expression(name);
            return self.get_regular_type_of_literal_type(t);
        }
        self.never_type
    }

    // checker.go:27259
    fn is_key_type_included(key_type: P<Type>, include: TypeFlags) -> bool {
        key_type.flags().intersects(include)
            || key_type.flags().intersects(TypeFlags::Intersection)
                && key_type.types().iter().any(|&t| Self::is_key_type_included(t, include))
    }
}

// checker.go:27266
pub(crate) fn is_invalid_computed_property_name(node: P<Node>) -> bool {
    let grand_parent = node.parent().unwrap().parent().unwrap();
    (is_type_literal_node(grand_parent)
        || is_class_like(grand_parent)
        || is_interface_declaration(grand_parent))
        && is_binary_expression(node.expression().unwrap())
        && node.expression().unwrap().as_binary_expression().operator_token.kind()
            == Kind::InKeyword
        && !is_accessor(node.parent().unwrap())
}

impl Checker {
    // checker.go:27272
    pub(crate) fn check_computed_property_name(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            links.resolved_type.set(Some(self.circular_constraint_type));
            if is_invalid_computed_property_name(node) {
                links.resolved_type.set(Some(self.error_type));
                return links.resolved_type.get().unwrap();
            }
            let t = self.check_expression(node.expression().unwrap());
            links.resolved_type.set(Some(t));
            // This will allow types number, string, symbol or any. It will also allow enums, the unknown
            // type, and any union of these types (like string | number).
            if links.resolved_type.get().unwrap().flags().intersects(TypeFlags::Nullable)
                || !self.is_type_assignable_to_kind(
                    links.resolved_type.get().unwrap(),
                    TypeFlags::StringLike | TypeFlags::NumberLike | TypeFlags::ESSymbolLike,
                ) && !self.is_type_assignable_to(
                    links.resolved_type.get().unwrap(),
                    self.string_number_symbol_type,
                )
            {
                self.error(Some(node), &diagnostics::A_COMPUTED_PROPERTY_NAME_MUST_BE_OF_TYPE_STRING_NUMBER_SYMBOL_OR_ANY, &[]);
            }
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:27292
    pub(crate) fn is_no_infer_type(t: P<Type>) -> bool {
        // A NoInfer<T> type is represented as a substitution type with a TypeFlags.Unknown constraint.
        t.flags().intersects(TypeFlags::Substitution)
            && t.as_substitution_type()
                .constraint
                .get()
                .unwrap()
                .flags()
                .intersects(TypeFlags::Unknown)
    }

    // checker.go:27297
    pub(crate) fn get_substitution_intersection(&mut self, t: P<Type>) -> P<Type> {
        if Self::is_no_infer_type(t) {
            return t.as_substitution_type().base_type().unwrap();
        }
        let s = t.as_substitution_type();
        self.get_intersection_type(&[s.constraint.get().unwrap(), s.base_type().unwrap()])
    }

    // checker.go:27304
    pub(crate) fn should_defer_index_type(&mut self, t: P<Type>, index_flags: IndexFlags) -> bool {
        t.flags().intersects(TypeFlags::InstantiableNonPrimitive)
            || Self::is_generic_tuple_type(t)
            || self.is_generic_mapped_type(t) && self.get_name_type_from_mapped_type(t).is_some()
            || t.flags().intersects(TypeFlags::Union)
                && !index_flags.intersects(IndexFlags::NoReducibleCheck)
                && self.is_generic_reducible_type(t)
            || t.flags().intersects(TypeFlags::Intersection)
                && self.maybe_type_of_kind(t, TypeFlags::Instantiable)
                && t.types().iter().any(|&t| self.is_empty_anonymous_object_type(t))
    }

    // checker.go:27312
    pub(crate) fn get_mapped_type_name_type_kind(&mut self, t: P<Type>) -> MappedTypeNameTypeKind {
        let Some(name_type) = self.get_name_type_from_mapped_type(t) else {
            return MappedTypeNameTypeKind::None;
        };
        let type_parameter = self.get_type_parameter_from_mapped_type(t);
        if self.is_type_assignable_to(name_type, type_parameter) {
            return MappedTypeNameTypeKind::Filtering;
        }
        MappedTypeNameTypeKind::Remapping
    }

    // checker.go:27323
    fn get_index_type_for_generic_type(&mut self, t: P<Type>, index_flags: IndexFlags) -> P<Type> {
        let key = CachedTypeKey {
            kind: if_else(
                index_flags.intersects(IndexFlags::StringsOnly),
                CachedTypeKind::StringIndexType,
                CachedTypeKind::IndexType,
            ),
            type_id: t.id,
        };
        if let Some(index_type) = self.cached_types.get(&key).copied() {
            return index_type;
        }
        let index_type = self.new_index_type(t, index_flags & IndexFlags::StringsOnly);
        self.cached_types.insert(key, index_type);
        index_type
    }

    // This roughly mirrors `resolveMappedTypeMembers` in the nongeneric case, except only reports a union of the keys calculated,
    // rather than manufacturing the properties. We can't just fetch the `constraintType` since that would ignore mappings
    // and mapping the `constraintType` directly ignores how mapped types map _properties_ and not keys (thus ignoring subtype
    // reduction in the constraintType) when possible.
    // @param noIndexSignatures Indicates if _string_ index signatures should be elided. (other index signatures are always reported)
    // checker.go:27341
    pub(crate) fn get_index_type_for_mapped_type(
        &mut self,
        t: P<Type>,
        index_flags: IndexFlags,
    ) -> P<Type> {
        fn add_member_for_key_type(
            c: &mut Checker,
            key_types: &mut Vec<P<Type>>,
            t: P<Type>,
            name_type: Option<P<Type>>,
            type_parameter: P<Type>,
            key_type: P<Type>,
        ) {
            let mut prop_name_type = key_type;
            if let Some(name_type) = name_type {
                let mapper =
                    append_type_mapping(t.as_mapped_type().mapper.get(), type_parameter, key_type);
                prop_name_type = c.instantiate_type(name_type, Some(mapper));
                // SAFETY: made here for this one instantiation.
                unsafe { recycle_mapping(mapper, true) };
            }
            // `keyof` currently always returns `string | number` for concrete `string` index signatures - the below ternary keeps that behavior for mapped types
            // See `getLiteralTypeFromProperties` where there's a similar ternary to cause the same behavior.
            key_types.push(if_else(
                prop_name_type == c.string_type,
                c.string_or_number_type,
                prop_name_type,
            ));
        }

        let type_parameter = self.get_type_parameter_from_mapped_type(t);
        let constraint_type = self.get_constraint_type_from_mapped_type(t);
        let name_type =
            self.get_name_type_from_mapped_type(t.as_mapped_type().target.get().unwrap_or(t));
        if name_type.is_none() && !index_flags.intersects(IndexFlags::NoIndexSignatures) {
            // no mapping and no filtering required, just quickly bail to returning the constraint in the common case
            return constraint_type;
        }
        let mut key_types: Vec<P<Type>> = Vec::new();
        // Calling getApparentType on the `T` of a `keyof T` in the constraint type of a generic mapped type can
        // trigger a circularity. For example, `T extends { [P in keyof T & string as Captitalize<P>]: any }` is
        // a circular definition. For this reason, we only eagerly manifest the keys if the constraint is non-generic.
        if self.is_generic_index_type(constraint_type) {
            if Self::is_mapped_type_with_keyof_constraint_declaration(t) {
                // We have a generic index and a homomorphic mapping and a key remapping - we need to defer
                // the whole `keyof whatever` for later since it's not safe to resolve the shape of modifier type.
                return self.get_index_type_for_generic_type(t, index_flags);
            }
            // Include the generic component in the resulting type.
            for_each_type(self, constraint_type, |c, key_type| {
                add_member_for_key_type(c, &mut key_types, t, name_type, type_parameter, key_type);
            });
        } else if Self::is_mapped_type_with_keyof_constraint_declaration(t) {
            let modifiers = self.get_modifiers_type_from_mapped_type(t);
            let modifiers_type = self.get_apparent_type(modifiers);
            // The 'T' in 'keyof T'
            self.for_each_mapped_type_property_key_type_and_index_signature_key_type(
                modifiers_type,
                TypeFlags::StringOrNumberLiteralOrUnique,
                index_flags.intersects(IndexFlags::StringsOnly),
                |c, key_type| {
                    add_member_for_key_type(
                        c,
                        &mut key_types,
                        t,
                        name_type,
                        type_parameter,
                        key_type,
                    );
                },
            );
        } else {
            let lower_bound = self.get_lower_bound_of_key_type(constraint_type);
            for_each_type(self, lower_bound, |c, key_type| {
                add_member_for_key_type(c, &mut key_types, t, name_type, type_parameter, key_type);
            });
        }
        // We had to pick apart the constraintType to potentially map/filter it - compare the final resulting list with the
        // original constraintType, so we can return the union that preserves aliases/origin data if possible.

        let result = if index_flags.intersects(IndexFlags::NoIndexSignatures) {
            let union = self.get_union_type(&key_types);
            self.filter_type(union, |_, t| {
                !t.flags().intersects(TypeFlags::Any | TypeFlags::String)
            })
        } else {
            self.get_union_type(&key_types)
        };
        if result.flags().intersects(TypeFlags::Union)
            && constraint_type.flags().intersects(TypeFlags::Union)
            && get_type_list_key(result.types()) == get_type_list_key(constraint_type.types())
        {
            return constraint_type;
        }
        result
    }

    // checker.go:27393
    pub(crate) fn get_indexed_access_type(
        &mut self,
        object_type: P<Type>,
        index_type: P<Type>,
    ) -> P<Type> {
        self.get_indexed_access_type_ex(
            object_type,
            index_type,
            AccessFlags::None,
            None,
            AliasArg::None,
        )
    }

    // checker.go:27397
    pub(crate) fn get_indexed_access_type_ex(
        &mut self,
        object_type: P<Type>,
        index_type: P<Type>,
        access_flags: AccessFlags,
        access_node: Option<P<Node>>,
        alias: AliasArg<'_>,
    ) -> P<Type> {
        let result = self.get_indexed_access_type_or_undefined(
            object_type,
            index_type,
            access_flags,
            access_node,
            alias,
        );
        match result {
            Some(result) => result,
            None => if_else(access_node.is_some(), self.error_type, self.unknown_type),
        }
    }

    // checker.go:27405
    #[cfg_attr(
        not(feature = "work-census"),
        inline(always),
        expect(
            clippy::inline_always,
            reason = "without the census the wrapper is a forwarding call; inlined, callers call the body as before (notes/perf-checker-algorithms.md)"
        )
    )]
    pub(crate) fn get_indexed_access_type_or_undefined(
        &mut self,
        object_type: P<Type>,
        index_type: P<Type>,
        access_flags: AccessFlags,
        access_node: Option<P<Node>>,
        alias: AliasArg<'_>,
    ) -> Option<P<Type>> {
        if self.census_on() {
            let span = self.census_begin(crate::workcensus::Cat::IndexedAccess, || {
                crate::workcensus::CKey::None
            });
            let r = self.get_indexed_access_type_or_undefined_inner(
                object_type,
                index_type,
                access_flags,
                access_node,
                alias,
            );
            let timing = self.census_end(span).unwrap();
            let n = if object_type.flags().intersects(TypeFlags::Union) {
                object_type.types().len()
            } else {
                1
            };
            let literal_index =
                index_type.flags().intersects(TypeFlags::StringOrNumberLiteralOrUnique) as u64;
            let key = crate::workcensus::CKey::Bucket(crate::workcensus::bucket(n));
            self.census.as_mut().unwrap().record(
                crate::workcensus::Cat::IndexedAccess,
                key,
                timing,
                n as u64,
                literal_index,
                0,
            );
            return r;
        }
        if access_node.is_none()
            && alias.is_none()
            && index_type.flags().intersects(TypeFlags::Union)
            && !index_type.flags().intersects(TypeFlags::Boolean)
        {
            return self.get_indexed_access_type_by_union_memoized(
                object_type,
                index_type,
                access_flags,
            );
        }
        self.get_indexed_access_type_or_undefined_inner(
            object_type,
            index_type,
            access_flags,
            access_node,
            alias,
        )
    }

    /// tsrs-only: `getIndexedAccessTypeOrUndefined` for a union index without an access node or an alias, memoized
    /// per checker. TypeScript resolves such a `T[K]` again at every request (only a deferred one is interned), one
    /// property lookup per constituent of `K`; on the 38k-file codebase 92% of those lookups repeat an earlier call
    /// with the same object type, index type and flags. The store rule is the union front cache's
    /// (unioncache.rs): a call is stored only if it created no type but the one it returns, instantiated nothing,
    /// took no impure union reduction, added no diagnostic and did not return `errorType`; such a call reads only
    /// caches that grow, so a later identical call returns the same answer. `TSRS_IA_MEMO=0` turns it off, `=shadow`
    /// computes every hit again and panics on a different answer.
    fn get_indexed_access_type_by_union_memoized(
        &mut self,
        object_type: P<Type>,
        index_type: P<Type>,
        access_flags: AccessFlags,
    ) -> Option<P<Type>> {
        let mode = crate::unioncache::indexed_access_memo_mode();
        if mode == crate::unioncache::UnionCacheMode::Off {
            return self.get_indexed_access_type_or_undefined_inner(
                object_type,
                index_type,
                access_flags,
                None,
                AliasArg::None,
            );
        }
        let key = (object_type, index_type, access_flags.bits());
        if let Some(&cached) = self.indexed_access_union_memo.get(&key) {
            if mode == crate::unioncache::UnionCacheMode::Shadow {
                let fresh = self.get_indexed_access_type_or_undefined_inner(
                    object_type,
                    index_type,
                    access_flags,
                    None,
                    AliasArg::None,
                );
                assert!(
                    fresh == cached,
                    "TSRS_IA_MEMO=shadow: indexed access {} [{}]: cached {:?}, fresh {:?}",
                    object_type.id.0,
                    index_type.id.0,
                    cached.map(|t| t.id.0),
                    fresh.map(|t| t.id.0)
                );
            }
            return cached;
        }
        let type_count = self.type_count;
        let instantiation_count = self.instantiation_count;
        let total_instantiation_count = self.total_instantiation_count;
        let impure = self.union_front_cache.impure;
        let diagnostic_count = self.diagnostics.count() + self.suggestion_diagnostics.count();
        let too_complex_reports = self.too_complex_reports;
        let result = self.get_indexed_access_type_or_undefined_inner(
            object_type,
            index_type,
            access_flags,
            None,
            AliasArg::None,
        );
        let created = type_count != self.type_count
            && !(self.type_count == type_count + 1
                && result.is_some_and(|r| r.id.0 == self.type_count));
        let instantiated = instantiation_count != self.instantiation_count
            || total_instantiation_count != self.total_instantiation_count;
        let impure = impure != self.union_front_cache.impure;
        let reported = diagnostic_count
            != self.diagnostics.count() + self.suggestion_diagnostics.count()
            || too_complex_reports != self.too_complex_reports;
        if !(created || instantiated || impure || reported || result == Some(self.error_type)) {
            self.indexed_access_union_memo.insert(key, result);
        }
        result
    }

    fn get_indexed_access_type_or_undefined_inner(
        &mut self,
        object_type: P<Type>,
        index_type: P<Type>,
        access_flags: AccessFlags,
        access_node: Option<P<Node>>,
        alias: AliasArg<'_>,
    ) -> Option<P<Type>> {
        let mut index_type = index_type;
        let mut access_flags = access_flags;
        if object_type == self.wildcard_type || index_type == self.wildcard_type {
            return Some(self.wildcard_type);
        }
        let object_type = self.get_reduced_type(object_type);
        // If the object type has a string index signature and no other members we know that the result will
        // always be the type of that index signature and we can simplify accordingly.
        if self.is_string_index_signature_only_type(object_type)
            && !index_type.flags().intersects(TypeFlags::Nullable)
            && self.is_type_assignable_to_kind(index_type, TypeFlags::String | TypeFlags::Number)
        {
            index_type = self.string_type;
        }
        // In noUncheckedIndexedAccess mode, indexed access operations that occur in an expression in a read position and resolve to
        // an index signature have 'undefined' included in their type.
        if self.compiler_options.no_unchecked_indexed_access == Tristate::True
            && access_flags.intersects(AccessFlags::ExpressionPosition)
        {
            access_flags |= AccessFlags::IncludeUndefined;
        }
        // If the index type is generic, or if the object type is generic and doesn't originate in an expression and
        // the operation isn't exclusively indexing the fixed (non-variadic) portion of a tuple type, we are performing
        // a higher-order index access where we cannot meaningfully access the properties of the object type. Note that
        // for a generic T and a non-generic K, we eagerly resolve T[K] if it originates in an expression. This is to
        // preserve backwards compatibility. For example, an element access 'this["foo"]' has always been resolved
        // eagerly using the constraint type of 'this' at the given location.
        if self.should_defer_indexed_access_type(object_type, index_type, access_node) {
            if object_type.flags().intersects(TypeFlags::AnyOrUnknown) {
                return Some(object_type);
            }
            // Defer the operation by creating an indexed access type.
            let persistent_access_flags = access_flags & AccessFlags::Persistent;
            let key = get_indexed_access_key(object_type, index_type, access_flags, alias);
            if let Some(t) = self.indexed_access_types.get(&key) {
                return Some(t);
            }
            let t = self.new_indexed_access_type(object_type, index_type, persistent_access_flags);
            t.set_alias(alias.alias());
            self.indexed_access_types.insert(key, t);
            return Some(t);
        }
        // In the following we resolve T[K] to the type of the property in T selected by K.
        // We treat boolean as different from other unions to improve errors;
        // skipping straight to getPropertyTypeForIndexType gives errors with 'boolean' instead of 'true'.
        let apparent_object_type = self.get_reduced_apparent_type(object_type);
        if index_type.flags().intersects(TypeFlags::Union)
            && !index_type.flags().intersects(TypeFlags::Boolean)
        {
            let mut prop_types: Vec<P<Type>> = Vec::new();
            let mut was_missing_prop = false;
            for &t in index_type.types() {
                let prop_type = self.get_property_type_for_index_type(
                    object_type,
                    apparent_object_type,
                    t,
                    index_type,
                    access_node,
                    access_flags
                        | if_else(
                            was_missing_prop,
                            AccessFlags::SuppressNoImplicitAnyError,
                            AccessFlags::None,
                        ),
                );
                if let Some(prop_type) = prop_type {
                    prop_types.push(prop_type);
                } else if access_node.is_none() {
                    // If there's no error node, we can immediately stop, since error reporting is off
                    return None;
                } else {
                    // Otherwise we set a flag and return at the end of the loop so we still mark all errors
                    was_missing_prop = true;
                }
            }
            if was_missing_prop {
                return None;
            }
            if access_flags.intersects(AccessFlags::Writing) {
                return Some(self.get_intersection_type_ex(
                    &prop_types,
                    IntersectionFlags::None,
                    alias,
                ));
            }
            return Some(self.get_union_type_ex(&prop_types, UnionReduction::Literal, alias, None));
        }
        self.get_property_type_for_index_type(
            object_type,
            apparent_object_type,
            index_type,
            index_type,
            access_node,
            access_flags | AccessFlags::CacheSymbol | AccessFlags::ReportDeprecated,
        )
    }

    // checker.go:27471
    fn get_property_type_for_index_type(
        &mut self,
        original_object_type: P<Type>,
        object_type: P<Type>,
        index_type: P<Type>,
        full_index_type: P<Type>,
        access_node: Option<P<Node>>,
        access_flags: AccessFlags,
    ) -> Option<P<Type>> {
        let mut access_expression: Option<P<Node>> = None;
        if let Some(access_node) = access_node
            && is_element_access_expression(access_node)
        {
            access_expression = Some(access_node);
        }
        let mut prop_name = std::borrow::Cow::Borrowed("");
        let mut has_prop_name = false;
        if !access_node.is_some_and(is_private_identifier) {
            prop_name = Self::get_property_name_from_index(index_type, access_node);
            has_prop_name = prop_name != INTERNAL_SYMBOL_NAME_MISSING;
        }
        if has_prop_name {
            if access_flags.intersects(AccessFlags::Contextual) {
                let t = self.get_type_of_property_of_contextual_type(object_type, &prop_name);
                return Some(t.unwrap_or(self.any_type));
            }
            let prop = self.get_property_of_type(object_type, &prop_name);
            if let Some(prop) = prop {
                if access_flags.intersects(AccessFlags::ReportDeprecated)
                    && let Some(access_node) = access_node
                    && !prop.declarations().is_empty()
                    && self.is_deprecated_symbol(prop)
                    && self.is_uncalled_function_reference(access_node, prop)
                {
                    let deprecated_node;
                    if let Some(access_expression) = access_expression {
                        deprecated_node =
                            access_expression.as_element_access_expression().argument_expression;
                    } else if is_indexed_access_type_node(access_node) {
                        deprecated_node = access_node.as_indexed_access_type_node().index_type;
                    } else {
                        deprecated_node = access_node;
                    }
                    let declarations = prop.declarations();
                    self.add_deprecated_suggestion(deprecated_node, declarations, &prop_name);
                }
                if let Some(access_expression) = access_expression {
                    let is_self_type_access = self.is_self_type_access(
                        access_expression.expression().unwrap(),
                        object_type.symbol(),
                    );
                    self.mark_property_as_referenced(
                        prop,
                        Some(access_expression),
                        is_self_type_access,
                    );
                    if self.is_assignment_to_readonly_entity(
                        access_expression,
                        prop,
                        get_assignment_target_kind(access_expression),
                    ) {
                        let name = self.symbol_to_string(prop);
                        self.error(
                            Some(
                                access_expression
                                    .as_element_access_expression()
                                    .argument_expression,
                            ),
                            &diagnostics::CANNOT_ASSIGN_TO_0_BECAUSE_IT_IS_A_READ_ONLY_PROPERTY,
                            &[&name],
                        );
                        return None;
                    }
                    if access_flags.intersects(AccessFlags::CacheSymbol) {
                        self.symbol_node_links
                            .get(access_node.unwrap())
                            .resolved_symbol
                            .set(Some(prop));
                    }
                    if self.is_this_property_access_in_constructor(access_expression, prop) {
                        return Some(self.auto_type);
                    }
                }
                let prop_type = if access_flags.intersects(AccessFlags::Writing) {
                    self.get_write_type_of_symbol(prop).unwrap()
                } else {
                    self.get_type_of_symbol(prop)
                };
                if let Some(access_expression) = access_expression
                    .filter(|&ae| get_assignment_target_kind(ae) != AssignmentKind::Definite)
                {
                    return Some(self.get_flow_type_of_reference(access_expression, prop_type));
                } else if access_node.is_some_and(is_indexed_access_type_node)
                    && self.contains_missing_type(prop_type)
                {
                    let undefined_type = self.undefined_type;
                    return Some(self.get_union_type(&[prop_type, undefined_type]));
                }
                return Some(prop_type);
            }
            if every_type(self, object_type, |_, t| is_tuple_type(t))
                && is_numeric_literal_name(&prop_name)
            {
                let index = jsnum::from_string(&prop_name);
                if let Some(access_node) = access_node
                    && every_type(self, object_type, |_, t| {
                        !t.target_tuple_type()
                            .combined_flags
                            .get()
                            .intersects(ElementFlags::Variable)
                    })
                    && !access_flags.intersects(AccessFlags::AllowMissing)
                {
                    let index_node = get_index_node_for_access_expression(access_node);
                    if is_tuple_type(object_type) {
                        if index < jsnum::Number(0.0) {
                            self.error(
                                index_node,
                                &diagnostics::A_TUPLE_TYPE_CANNOT_BE_INDEXED_WITH_A_NEGATIVE_VALUE,
                                &[],
                            );
                            return Some(self.undefined_type);
                        }
                        let type_string = self.type_to_string_exported(object_type);
                        let arity = Self::get_type_reference_arity(object_type);
                        self.error(
                            index_node,
                            &diagnostics::TUPLE_TYPE_0_OF_LENGTH_1_HAS_NO_ELEMENT_AT_INDEX_2,
                            &[&type_string, &arity, &prop_name],
                        );
                    } else {
                        let type_string = self.type_to_string_exported(object_type);
                        self.error(
                            index_node,
                            &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1,
                            &[&prop_name, &type_string],
                        );
                    }
                }
                if index >= jsnum::Number(0.0) {
                    let number_type = self.number_type;
                    let index_info = self.get_index_info_of_type(object_type, number_type);
                    self.error_if_writing_to_readonly_index(
                        index_info,
                        object_type,
                        access_expression,
                    );
                    let undefined_like_type = if_else(
                        access_flags.intersects(AccessFlags::IncludeUndefined),
                        Some(self.missing_type),
                        None,
                    );
                    return Some(self.get_tuple_element_type_out_of_start_count(
                        object_type,
                        index,
                        undefined_like_type,
                    ));
                }
            }
        }
        if !index_type.flags().intersects(TypeFlags::Nullable)
            && self.is_type_assignable_to_kind(
                index_type,
                TypeFlags::StringLike | TypeFlags::NumberLike | TypeFlags::ESSymbolLike,
            )
        {
            if object_type.flags().intersects(TypeFlags::Any | TypeFlags::Never) {
                return Some(object_type);
            }
            // If no index signature is applicable, we default to the string index signature. In effect, this means the string
            // index signature applies even when accessing with a symbol-like type.
            let mut index_info = self.get_applicable_index_info(object_type, index_type);
            if index_info.is_none() {
                let string_type = self.string_type;
                index_info = self.get_index_info_of_type(object_type, string_type);
            }
            if let Some(index_info) = index_info {
                if access_flags.intersects(AccessFlags::NoIndexSignatures)
                    && index_info.key_type() != self.number_type
                {
                    if let Some(access_expression) = access_expression {
                        if access_flags.intersects(AccessFlags::Writing) {
                            let type_string = self.type_to_string_exported(original_object_type);
                            self.error(
                                Some(access_expression),
                                &diagnostics::TYPE_0_IS_GENERIC_AND_CAN_ONLY_BE_INDEXED_FOR_READING,
                                &[&type_string],
                            );
                        } else {
                            let index_string = self.type_to_string_exported(index_type);
                            let type_string = self.type_to_string_exported(original_object_type);
                            self.error(
                                Some(access_expression),
                                &diagnostics::TYPE_0_CANNOT_BE_USED_TO_INDEX_TYPE_1,
                                &[&index_string, &type_string],
                            );
                        }
                    }
                    return None;
                }
                if let Some(access_node) = access_node
                    && index_info.key_type() == self.string_type
                    && !self.is_type_assignable_to_kind(
                        index_type,
                        TypeFlags::String | TypeFlags::Number,
                    )
                {
                    let index_node = get_index_node_for_access_expression(access_node);
                    let index_string = self.type_to_string_exported(index_type);
                    self.error(
                        index_node,
                        &diagnostics::TYPE_0_CANNOT_BE_USED_AS_AN_INDEX_TYPE,
                        &[&index_string],
                    );
                    if access_flags.intersects(AccessFlags::IncludeUndefined) {
                        let missing_type = self.missing_type;
                        return Some(self.get_union_type(&[index_info.value_type(), missing_type]));
                    }
                    return Some(index_info.value_type());
                }
                self.error_if_writing_to_readonly_index(
                    Some(index_info),
                    object_type,
                    access_expression,
                );
                // When accessing an enum object with its own type,
                // e.g. E[E.A] for enum E { A }, undefined shouldn't
                // be included in the result type
                if access_flags.intersects(AccessFlags::IncludeUndefined)
                    && !(object_type.symbol().is_some_and(|s| {
                        s.flags().intersects(SymbolFlags::RegularEnum | SymbolFlags::ConstEnum)
                    }) && (index_type.symbol().is_some()
                        && index_type.flags().intersects(TypeFlags::EnumLiteral)
                        && self.get_parent_of_symbol(index_type.symbol().unwrap())
                            == object_type.symbol()))
                {
                    let missing_type = self.missing_type;
                    return Some(self.get_union_type(&[index_info.value_type(), missing_type]));
                }
                return Some(index_info.value_type());
            }
            if index_type.flags().intersects(TypeFlags::Never) {
                return Some(self.never_type);
            }
            if self.is_js_literal_type(object_type) {
                return Some(self.any_type);
            }
            if let Some(access_expression) =
                access_expression.filter(|_| !is_const_enum_object_type(object_type))
            {
                if is_object_literal_type(object_type) {
                    if self.no_implicit_any
                        && index_type
                            .flags()
                            .intersects(TypeFlags::StringLiteral | TypeFlags::NumberLiteral)
                    {
                        let value = literal_value_to_string(index_type.as_literal_type().value());
                        let type_string = self.type_to_string_exported(object_type);
                        self.add_diagnostic(create_diagnostic_for_node(
                            Some(access_expression),
                            &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1,
                            &[&value, &type_string],
                        ));
                        return Some(self.undefined_type);
                    } else if index_type.flags().intersects(TypeFlags::Number | TypeFlags::String) {
                        let mut types: Vec<P<Type>> = object_type
                            .as_structured_type()
                            .properties()
                            .iter()
                            .map(|&prop| self.get_type_of_symbol(prop))
                            .collect();
                        types.push(self.undefined_type);
                        return Some(self.get_union_type(&types));
                    }
                }
                let global_this_export = if has_prop_name {
                    self.global_this_symbol.exports().and_then(|e| e.lookup(&prop_name))
                } else {
                    None
                };
                if object_type.symbol() == Some(self.global_this_symbol)
                    && has_prop_name
                    && global_this_export.is_some()
                    && global_this_export.unwrap().flags().intersects(SymbolFlags::BlockScoped)
                {
                    let type_string = self.type_to_string_exported(object_type);
                    self.error(
                        Some(access_expression),
                        &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1,
                        &[&prop_name, &type_string],
                    );
                } else if self.no_implicit_any
                    && !access_flags.intersects(AccessFlags::SuppressNoImplicitAnyError)
                {
                    let number_type = self.number_type;
                    if has_prop_name && self.type_has_static_property(&prop_name, object_type) {
                        let type_name = self.type_to_string_exported(object_type);
                        let suggestion = type_name.clone()
                            + "["
                            + &tsrs_scanner::get_text_of_node(
                                access_expression
                                    .as_element_access_expression()
                                    .argument_expression,
                            )
                            + "]";
                        self.error(Some(access_expression), &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1_DID_YOU_MEAN_TO_ACCESS_THE_STATIC_MEMBER_2_INSTEAD, &[&prop_name /* as string */, &type_name, &suggestion]);
                    } else if self.get_index_type_of_type(object_type, number_type).is_some() {
                        self.error(Some(access_expression.as_element_access_expression().argument_expression), &diagnostics::ELEMENT_IMPLICITLY_HAS_AN_ANY_TYPE_BECAUSE_INDEX_EXPRESSION_IS_NOT_OF_TYPE_NUMBER, &[]);
                    } else {
                        let mut suggestion = String::new();
                        if has_prop_name {
                            suggestion = self
                                .get_suggestion_for_nonexistent_property(&prop_name, object_type);
                        }
                        if suggestion.is_empty() {
                            suggestion = self.get_suggestion_for_nonexistent_index_signature(
                                object_type,
                                access_expression,
                                index_type,
                            );
                            if suggestion.is_empty() {
                                let mut diagnostic: Option<P<Diagnostic>> = None;
                                if index_type.flags().intersects(TypeFlags::EnumLiteral) {
                                    let name = "[".to_string()
                                        + &self.type_to_string_exported(index_type)
                                        + "]";
                                    let type_string = self.type_to_string_exported(object_type);
                                    diagnostic = Some(new_diagnostic_for_node(
                                        Some(access_expression),
                                        Some(&diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1),
                                        &[&name, &type_string],
                                    ));
                                } else if index_type.flags().intersects(TypeFlags::UniqueESSymbol) {
                                    let symbol_name = self.get_fully_qualified_name(
                                        index_type.symbol().unwrap(),
                                        Some(access_expression),
                                    );
                                    let name = "[".to_string() + &symbol_name + "]";
                                    let type_string = self.type_to_string_exported(object_type);
                                    diagnostic = Some(new_diagnostic_for_node(
                                        Some(access_expression),
                                        Some(&diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1),
                                        &[&name, &type_string],
                                    ));
                                } else if (index_type.flags().intersects(TypeFlags::StringLiteral))
                                    || (index_type.flags().intersects(TypeFlags::NumberLiteral))
                                {
                                    let value = literal_value_to_string(
                                        index_type.as_literal_type().value(),
                                    );
                                    let type_string = self.type_to_string_exported(object_type);
                                    diagnostic = Some(new_diagnostic_for_node(
                                        Some(access_expression),
                                        Some(&diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1),
                                        &[&value, &type_string],
                                    ));
                                } else if index_type
                                    .flags()
                                    .intersects(TypeFlags::Number | TypeFlags::String)
                                {
                                    let index_string = self.type_to_string_exported(index_type);
                                    let type_string = self.type_to_string_exported(object_type);
                                    diagnostic = Some(new_diagnostic_for_node(Some(access_expression), Some(&diagnostics::NO_INDEX_SIGNATURE_WITH_A_PARAMETER_OF_TYPE_0_WAS_FOUND_ON_TYPE_1), &[&index_string, &type_string]));
                                }
                                let full_index_string =
                                    self.type_to_string_exported(full_index_type);
                                let type_string = self.type_to_string_exported(object_type);
                                self.add_diagnostic(new_diagnostic_chain_for_node(
                                    diagnostic,
                                    access_expression,
                                    Some(&diagnostics::ELEMENT_IMPLICITLY_HAS_AN_ANY_TYPE_BECAUSE_EXPRESSION_OF_TYPE_0_CAN_T_BE_USED_TO_INDEX_TYPE_1),
                                    &[&full_index_string, &type_string],
                                ));
                            } else {
                                let type_string = self.type_to_string_exported(object_type);
                                self.error(Some(access_expression), &diagnostics::ELEMENT_IMPLICITLY_HAS_AN_ANY_TYPE_BECAUSE_TYPE_0_HAS_NO_INDEX_SIGNATURE_DID_YOU_MEAN_TO_CALL_1, &[&type_string, &suggestion]);
                            }
                        } else {
                            let type_string = self.type_to_string_exported(object_type);
                            self.error(
                                Some(
                                    access_expression
                                        .as_element_access_expression()
                                        .argument_expression,
                                ),
                                &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1_DID_YOU_MEAN_2,
                                &[&prop_name /* as string */, &type_string, &suggestion],
                            );
                        }
                    }
                }
                return None;
            }
        }
        if access_flags.intersects(AccessFlags::AllowMissing) && is_object_literal_type(object_type)
        {
            return Some(self.undefined_type);
        }
        if self.is_js_literal_type(object_type) {
            return Some(self.any_type);
        }
        if let Some(access_node) = access_node {
            let index_node = get_index_node_for_access_expression(access_node).unwrap();
            if index_node.kind() != Kind::BigIntLiteral
                && index_type
                    .flags()
                    .intersects(TypeFlags::StringLiteral | TypeFlags::NumberLiteral)
            {
                let value = literal_value_to_string(index_type.as_literal_type().value());
                let type_string = self.type_to_string_exported(object_type);
                self.error(
                    Some(index_node),
                    &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1,
                    &[&value, &type_string],
                );
            } else if index_type.flags().intersects(TypeFlags::String | TypeFlags::Number) {
                let type_string = self.type_to_string_exported(object_type);
                let index_string = self.type_to_string_exported(index_type);
                self.error(
                    Some(index_node),
                    &diagnostics::TYPE_0_HAS_NO_MATCHING_INDEX_SIGNATURE_FOR_TYPE_1,
                    &[&type_string, &index_string],
                );
            } else {
                let type_string = if index_node.kind() == Kind::BigIntLiteral {
                    "bigint".to_string()
                } else {
                    self.type_to_string_exported(index_type)
                };
                self.error(
                    Some(index_node),
                    &diagnostics::TYPE_0_CANNOT_BE_USED_AS_AN_INDEX_TYPE,
                    &[&type_string],
                );
            }
        }
        if is_type_any(Some(index_type)) {
            return Some(index_type);
        }
        None
    }

    // checker.go:27685
    pub(crate) fn type_has_static_property(
        &mut self,
        prop_name: &str,
        containing_type: P<Type>,
    ) -> bool {
        if let Some(symbol) = containing_type.symbol() {
            let t = self.get_type_of_symbol(symbol);
            let prop = self.get_property_of_type(t, prop_name);
            return prop.is_some_and(|prop| prop.value_declaration().is_some_and(ast::is_static));
        }
        false
    }

    // checker.go:27693
    pub(crate) fn get_suggestion_for_nonexistent_property(
        &mut self,
        name: &str,
        containing_type: P<Type>,
    ) -> String {
        let props = self.get_properties_of_type(containing_type);
        let symbol = self.get_spelling_suggestion_for_name(name, props, SymbolFlags::Value);
        if let Some(symbol) = symbol {
            return symbol.name().to_string();
        }
        String::new()
    }

    // checker.go:27701
    fn get_suggestion_for_nonexistent_index_signature(
        &mut self,
        object_type: P<Type>,
        expr: P<Node>,
        keyed_type: P<Type>,
    ) -> String {
        // check if object type has setter or getter
        fn has_prop(
            c: &mut Checker,
            object_type: P<Type>,
            keyed_type: P<Type>,
            name: &str,
        ) -> bool {
            let prop = c.get_property_of_object_type(object_type, name);
            if let Some(prop) = prop {
                let t = c.get_type_of_symbol(prop);
                let s = c.get_single_call_signature(t);
                return s.is_some_and(|s| {
                    c.get_min_argument_count(s) >= 1 && {
                        let param_type = c.get_type_at_position(s, 0);
                        c.is_type_assignable_to(keyed_type, param_type)
                    }
                });
            }
            false
        }
        let suggested_method = if_else(is_assignment_target(expr), "set", "get");
        if !has_prop(self, object_type, keyed_type, suggested_method) {
            return String::new();
        }
        let suggestion =
            try_get_property_access_or_identifier_to_string(expr.expression().unwrap());
        if suggestion.is_empty() {
            return suggested_method.to_string();
        }
        suggestion + "." + suggested_method
    }

    // checker.go:27722
    pub(crate) fn get_suggested_type_for_nonexistent_string_literal_type(
        &mut self,
        source: P<Type>,
        target: P<Type>,
    ) -> Option<P<Type>> {
        let candidates = target
            .types()
            .iter()
            .copied()
            .filter(|t| t.flags().intersects(TypeFlags::StringLiteral));
        get_spelling_suggestion_with_max_candidate_count(
            get_string_literal_value(source),
            candidates,
            |t| get_string_literal_value(*t),
            |a, b| compare_types(self, Some(*a), Some(*b)),
            1000,
        )
    }
}

// checker.go:27727
fn get_index_node_for_access_expression(access_node: P<Node>) -> Option<P<Node>> {
    match access_node.kind() {
        Kind::ElementAccessExpression => {
            Some(access_node.as_element_access_expression().argument_expression)
        }
        Kind::IndexedAccessType => Some(access_node.as_indexed_access_type_node().index_type),
        Kind::ComputedPropertyName => access_node.expression(),
        _ => Some(access_node),
    }
}

impl Checker {
    // checker.go:27739
    fn error_if_writing_to_readonly_index(
        &mut self,
        index_info: Option<P<IndexInfo>>,
        object_type: P<Type>,
        access_expression: Option<P<Node>>,
    ) {
        if let (Some(index_info), Some(access_expression)) = (index_info, access_expression)
            && index_info.is_readonly()
            && (is_assignment_target(access_expression) || is_delete_target(access_expression))
        {
            let type_string = self.type_to_string_exported(object_type);
            self.error(
                Some(access_expression),
                &diagnostics::INDEX_SIGNATURE_IN_TYPE_0_ONLY_PERMITS_READING,
                &[&type_string],
            );
        }
    }

    // checker.go:27745
    pub(crate) fn is_self_type_access(&mut self, name: P<Node>, parent: Option<P<Symbol>>) -> bool {
        name.kind() == Kind::ThisKeyword
            || parent.is_some()
                && is_entity_name_expression(name)
                && parent == Some(self.get_resolved_symbol(get_first_identifier(name)))
    }

    // checker.go:27749
    pub(crate) fn is_assignment_to_readonly_entity(
        &mut self,
        expr: P<Node>,
        symbol: P<Symbol>,
        assignment_kind: AssignmentKind,
    ) -> bool {
        if assignment_kind == AssignmentKind::None {
            // no assignment means it doesn't matter whether the entity is readonly
            return false;
        }
        if is_access_expression(expr) {
            let node = skip_parentheses(expr.expression().unwrap());
            if is_identifier(node) {
                let expression_symbol = self.get_resolved_symbol(node);
                // CommonJS module.exports is never readonly
                if expression_symbol.flags().intersects(SymbolFlags::ModuleExports) {
                    return false;
                }
            }
        }
        if self.is_readonly_symbol(symbol) {
            // Allow assignments to readonly properties within constructors of the same class declaration.
            if symbol.flags().intersects(SymbolFlags::Property)
                && is_access_expression(expr)
                && expr.expression().unwrap().kind() == Kind::ThisKeyword
            {
                // Look for if this is the constructor for the class that `symbol` is a property of.
                let ctor = Self::get_control_flow_container(expr);
                let Some(ctor) = ctor.filter(|&ctor| is_constructor_declaration(ctor)) else {
                    return true;
                };
                if let Some(value_declaration) = symbol.value_declaration() {
                    let is_assignment_declaration = is_binary_expression(value_declaration);
                    let is_local_property_declaration = ctor.parent() == value_declaration.parent();
                    let is_local_parameter_property = Some(ctor) == value_declaration.parent();
                    let is_local_this_property_assignment = is_assignment_declaration
                        && symbol.parent().unwrap().value_declaration() == ctor.parent();
                    let is_local_this_property_assignment_constructor_function =
                        is_assignment_declaration
                            && symbol.parent().unwrap().value_declaration() == Some(ctor);
                    let is_writeable_symbol = is_local_property_declaration
                        || is_local_parameter_property
                        || is_local_this_property_assignment
                        || is_local_this_property_assignment_constructor_function;
                    return !is_writeable_symbol;
                }
            }
            return true;
        }
        if is_access_expression(expr) {
            // references through namespace import should be readonly
            let node = skip_parentheses(expr.expression().unwrap());
            if is_identifier(node) {
                let expression_symbol = self.get_resolved_symbol(node);
                if expression_symbol.flags().intersects(SymbolFlags::Alias) {
                    let declaration = Self::get_declaration_of_alias_symbol(expression_symbol);
                    return declaration.is_some_and(is_namespace_import);
                }
            }
        }
        false
    }

    // checker.go:27798
    pub(crate) fn is_this_property_access_in_constructor(
        &mut self,
        node: P<Node>,
        prop: P<Symbol>,
    ) -> bool {
        let mut constructor: Option<P<Node>> = None;
        let (kind, location) = self.is_constructor_declared_this_property(prop);
        if kind == ThisAssignmentDeclarationKind::Constructor {
            constructor = location;
        } else if is_this_property(node) && self.is_auto_typed_property(prop) {
            constructor = Self::get_declaring_constructor(prop);
        }
        Some(get_this_container(
            node, true,  /*includeArrowFunctions*/
            false, /*includeClassComputedPropertyName*/
        )) == constructor
    }

    // checker.go:27808
    pub(crate) fn is_auto_typed_property(&self, symbol: P<Symbol>) -> bool {
        // A property is auto-typed when its declaration has no type annotation or initializer and we're in
        // noImplicitAny mode or a .js file.
        let declaration = symbol.value_declaration();
        declaration.is_some_and(|declaration| {
            is_property_declaration(declaration)
                && declaration.type_node().is_none()
                && declaration.initializer().is_none()
        }) && self.no_implicit_any
    }

    // checker.go:27815
    pub(crate) fn get_declaring_constructor(symbol: P<Symbol>) -> Option<P<Node>> {
        let declarations = symbol.declarations();
        for &declaration in declarations {
            let container = get_this_container(
                declaration,
                false, /*includeArrowFunctions*/
                false, /*includeClassComputedPropertyName*/
            );
            if is_constructor_declaration(container) {
                return Some(container);
            }
        }
        None
    }

    // checker.go:27825
    pub(crate) fn get_property_name_from_index(
        index_type: P<Type>,
        access_node: Option<P<Node>>,
    ) -> std::borrow::Cow<'static, str> {
        if is_type_usable_as_property_name(index_type) {
            return get_property_name_from_type(index_type);
        }
        if let Some(access_node) = access_node.filter(|&n| is_property_name(n)) {
            return get_property_name_for_property_name_node(access_node);
        }
        INTERNAL_SYMBOL_NAME_MISSING.into()
    }

    // checker.go:27835
    pub(crate) fn is_string_index_signature_only_type_worker(&mut self, t: P<Type>) -> bool {
        t.flags().intersects(TypeFlags::Object)
            && !self.is_generic_mapped_type(t)
            && !self.has_properties_of_structured_type(t)
            && self.get_index_infos_of_type(t).len() == 1
            && {
                let string_type = self.string_type;
                self.get_index_info_of_type(t, string_type).is_some()
            }
            || t.flags().intersects(TypeFlags::UnionOrIntersection)
                && t.types().iter().all(|&t| self.is_string_index_signature_only_type(t))
    }

    // checker.go:27840
    fn should_defer_indexed_access_type(
        &mut self,
        object_type: P<Type>,
        index_type: P<Type>,
        access_node: Option<P<Node>>,
    ) -> bool {
        if self.is_generic_index_type(index_type) {
            return true;
        }
        if access_node.is_some_and(|n| !is_indexed_access_type_node(n)) {
            return Self::is_generic_tuple_type(object_type)
                && !index_type_less_than(
                    self,
                    index_type,
                    get_total_fixed_element_count(object_type.target_tuple_type()),
                );
        }
        self.is_generic_object_type(object_type)
            && !(is_tuple_type(object_type)
                && index_type_less_than(
                    self,
                    index_type,
                    get_total_fixed_element_count(object_type.target_tuple_type()),
                ))
            || self.is_generic_reducible_type(object_type)
    }
}

// checker.go:27851
fn index_type_less_than(c: &mut Checker, index_type: P<Type>, limit: usize) -> bool {
    every_type(c, index_type, |_, t| {
        if t.flags().intersects(TypeFlags::StringOrNumberLiteral) {
            let prop_name = get_property_name_from_type(t);
            if is_numeric_literal_name(&prop_name) {
                let index = jsnum::from_string(&prop_name);
                return index >= jsnum::Number(0.0) && index < jsnum::Number(limit as f64);
            }
        }
        false
    })
}

impl Checker {
    // checker.go:27864
    pub(crate) fn get_no_infer_type(&mut self, t: P<Type>) -> P<Type> {
        if self.is_no_infer_target_type(t) {
            let unknown_type = self.unknown_type;
            return self.get_or_create_substitution_type(t, unknown_type);
        }
        t
    }

    // checker.go:27871
    fn is_no_infer_target_type(&mut self, t: P<Type>) -> bool {
        // This is effectively a more conservative and predictable form of couldContainTypeVariables. We want to
        // preserve NoInfer<T> only for types that could contain type variables, but we don't want to exhaustively
        // examine all object type members.
        t.flags().intersects(TypeFlags::UnionOrIntersection)
            && t.as_union_or_intersection_type()
                .types()
                .iter()
                .any(|&t| self.is_no_infer_target_type(t))
            || t.flags().intersects(TypeFlags::Substitution)
                && !Self::is_no_infer_type(t)
                && self.is_no_infer_target_type(t.as_substitution_type().base_type().unwrap())
            || t.flags().intersects(TypeFlags::Object) && !self.is_empty_anonymous_object_type(t)
            || t.flags().intersects(TypeFlags::Instantiable & !TypeFlags::Substitution)
                && !self.is_pattern_literal_type(t)
    }

    // checker.go:27881
    pub(crate) fn get_substitution_type(
        &mut self,
        base_type: P<Type>,
        constraint: P<Type>,
    ) -> P<Type> {
        if constraint.flags().intersects(TypeFlags::AnyOrUnknown)
            || constraint == base_type
            || base_type.flags().intersects(TypeFlags::Any)
        {
            return base_type;
        }
        self.get_or_create_substitution_type(base_type, constraint)
    }

    // checker.go:27888
    fn get_or_create_substitution_type(
        &mut self,
        base_type: P<Type>,
        constraint: P<Type>,
    ) -> P<Type> {
        let key = SubstitutionTypeKey { base_id: base_type.id, constraint_id: constraint.id };
        if let Some(cached) = self.substitution_types.get(&key).copied() {
            return cached;
        }
        let result = self.new_substitution_type(base_type, constraint);
        self.substitution_types.insert(key, result);
        result
    }

    // checker.go:27898
    pub(crate) fn get_base_constraint_or_type(&mut self, t: P<Type>) -> P<Type> {
        let constraint = self.get_base_constraint_of_type(t);
        if let Some(constraint) = constraint {
            return constraint;
        }
        t
    }

    // checker.go:27906
    pub(crate) fn get_base_constraint_of_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        if t.flags().intersects(
            TypeFlags::InstantiableNonPrimitive
                | TypeFlags::UnionOrIntersection
                | TypeFlags::TemplateLiteral
                | TypeFlags::StringMapping
                | TypeFlags::Index,
        ) || Self::is_generic_tuple_type(t)
        {
            let constraint = self.get_resolved_base_constraint(t, BaseConstraintStack::default());
            if constraint != self.no_constraint_type && constraint != self.circular_constraint_type
            {
                return Some(constraint);
            }
            return None;
        }
        None
    }

    // checker.go:27917
    pub(crate) fn get_resolved_base_constraint(
        &mut self,
        t: P<Type>,
        stack: BaseConstraintStack<'_>,
    ) -> P<Type> {
        if t.try_as_constrained_type().is_none() && t.try_as_structured_type().is_none() {
            return t;
        }
        if let Some(resolved) = self.resolved_base_constraint_of(t) {
            return resolved;
        }
        if !self.push_type_resolution(
            TypeSystemEntity::Type(t),
            TypeSystemPropertyName::ResolvedBaseConstraint,
        ) {
            return self.circular_constraint_type;
        }
        let mut constraint: Option<P<Type>> = None;
        // We always explore at least 10 levels of nested constraints. Thereafter, we continue to explore
        // up to 50 levels of nested constraints provided there are no "deeply nested" types on the stack
        // (i.e. no types for which five instantiations have been recorded on the stack). If we reach 50
        // levels of nesting, we are presumably exploring a repeating pattern with a long cycle that hasn't
        // yet triggered the deeply nested limiter. We have no test cases that actually get to 50 levels of
        // nesting, so it is effectively just a safety stop.
        let identity = get_recursion_identity(self, t);
        if stack.len < 10 || stack.len < 50 && !stack.contains(identity) {
            let simplified = self.get_simplified_type(t, false /*writing*/);
            let frame = (identity, stack);
            constraint = self.compute_base_constraint(
                simplified,
                BaseConstraintStack { len: stack.len + 1, top: Some(&frame) },
            );
        }
        if !self.pop_type_resolution() {
            if t.flags().intersects(TypeFlags::TypeParameter) {
                let error_node = Self::get_constraint_declaration(t);
                if let Some(error_node) = error_node {
                    let type_string = self.type_to_string_exported(t);
                    let diagnostic = self.error(
                        Some(error_node),
                        &diagnostics::TYPE_PARAMETER_0_HAS_A_CIRCULAR_CONSTRAINT,
                        &[&type_string],
                    );
                    if let Some(current_node) = self.current_node
                        && !crate::is_node_descendant_of(Some(error_node), current_node)
                        && !crate::is_node_descendant_of(Some(current_node), error_node)
                    {
                        diagnostic.add_related_info(new_diagnostic_for_node(
                            Some(current_node),
                            Some(&diagnostics::CIRCULARITY_ORIGINATES_IN_TYPE_AT_THIS_LOCATION),
                            &[],
                        ));
                    }
                }
            }
            constraint = Some(self.circular_constraint_type);
        }
        let constraint = constraint.unwrap_or(self.no_constraint_type);
        if self.resolved_base_constraint_of(t).is_none() {
            match t.try_as_constrained_type() {
                Some(constrained) => constrained.resolved_base_constraint.set(Some(constraint)),
                None => {
                    self.structured_type_base_constraints.insert(t, constraint);
                }
            }
        }
        constraint
    }

    /// Go `t.AsConstrainedType().resolvedBaseConstraint` (structured types keep it in
    /// `structured_type_base_constraints`).
    #[inline]
    pub(crate) fn resolved_base_constraint_of(&self, t: P<Type>) -> Option<P<Type>> {
        match t.try_as_constrained_type() {
            Some(constrained) => constrained.resolved_base_constraint.get(),
            None => self.structured_type_base_constraints.get(&t).copied(),
        }
    }

    // checker.go:27960
    fn compute_base_constraint(
        &mut self,
        t: P<Type>,
        stack: BaseConstraintStack<'_>,
    ) -> Option<P<Type>> {
        let flags = t.flags();
        if flags.intersects(TypeFlags::TypeParameter) {
            let constraint = self.get_constraint_from_type_parameter(t);
            if t.as_type_parameter().is_this_type() {
                return constraint;
            }
            return self.get_next_base_constraint(constraint, stack);
        } else if flags.intersects(TypeFlags::UnionOrIntersection) {
            let types = t.types();
            let mut constraints: Vec<P<Type>> = Vec::with_capacity(types.len());
            let mut different = false;
            for &s in types {
                let constraint = self.get_next_base_constraint(Some(s), stack);
                if let Some(constraint) = constraint {
                    if constraint != s {
                        different = true;
                    }
                    constraints.push(constraint);
                } else {
                    different = true;
                }
            }
            if !different {
                return Some(t);
            }
            if t.flags().intersects(TypeFlags::Union) && constraints.len() == types.len() {
                return Some(self.get_union_type(&constraints));
            } else if t.flags().intersects(TypeFlags::Intersection) && !constraints.is_empty() {
                return Some(self.get_intersection_type(&constraints));
            }
            return None;
        } else if flags.intersects(TypeFlags::Index) {
            let target = t.as_index_type().target().unwrap();
            if self.is_generic_mapped_type(target) {
                let mapped_type = target;
                if self.get_name_type_from_mapped_type(mapped_type).is_some()
                    && !Self::is_mapped_type_with_keyof_constraint_declaration(mapped_type)
                {
                    let index_type =
                        self.get_index_type_for_mapped_type(mapped_type, IndexFlags::None);
                    return self.get_next_base_constraint(Some(index_type), stack);
                }
            }
            return Some(self.string_number_symbol_type);
        } else if flags.intersects(TypeFlags::TemplateLiteral) {
            let types = t.types();
            let mut constraints: Vec<P<Type>> = Vec::with_capacity(types.len());
            for &s in types {
                let constraint = self.get_next_base_constraint(Some(s), stack);
                if let Some(constraint) = constraint {
                    constraints.push(constraint);
                }
            }
            if constraints.len() == types.len() {
                return Some(self.get_template_literal_type(
                    t.as_template_literal_type().texts(),
                    &constraints,
                ));
            }
            return Some(self.string_type);
        } else if flags.intersects(TypeFlags::StringMapping) {
            let target = t.target().unwrap();
            let constraint = self.get_next_base_constraint(Some(target), stack);
            if let Some(constraint) = constraint.filter(|&c| c != target) {
                return Some(self.get_string_mapping_type(t.symbol().unwrap(), constraint));
            }
            return Some(self.string_type);
        } else if flags.intersects(TypeFlags::IndexedAccess) {
            let ia = t.as_indexed_access_type();
            if self.is_mapped_type_generic_indexed_access(t) {
                // For indexed access types of the form { [P in K]: E }[X], where K is non-generic and X is generic,
                // we substitute an instantiation of E where P is replaced with X.
                let substituted = self.substitute_indexed_mapped_type(
                    ia.object_type().unwrap(),
                    ia.index_type().unwrap(),
                );
                return self.get_next_base_constraint(Some(substituted), stack);
            }
            let base_object_type = self.get_next_base_constraint(ia.object_type(), stack);
            let base_index_type = self.get_next_base_constraint(ia.index_type(), stack);
            let (Some(base_object_type), Some(base_index_type)) =
                (base_object_type, base_index_type)
            else {
                return None;
            };
            let indexed = self.get_indexed_access_type_or_undefined(
                base_object_type,
                base_index_type,
                ia.access_flags.get(),
                None,
                AliasArg::None,
            );
            return self.get_next_base_constraint(indexed, stack);
        } else if flags.intersects(TypeFlags::Conditional) {
            if self.conditional_constraint_depth >= 100 {
                return None;
            }
            self.conditional_constraint_depth += 1;
            let constraint = self.get_constraint_from_conditional_type(t);
            self.conditional_constraint_depth -= 1;
            return self.get_next_base_constraint(Some(constraint), stack);
        } else if flags.intersects(TypeFlags::Substitution) {
            let intersection = self.get_substitution_intersection(t);
            return self.get_next_base_constraint(Some(intersection), stack);
        } else if Self::is_generic_tuple_type(t) {
            // We substitute constraints for variadic elements only when the constraints are array types or
            // non-variadic tuple types as we want to avoid further (possibly unbounded) recursion.
            let element_types = self.get_element_types(t);
            let element_infos = t.target_tuple_type().element_infos();
            let mut new_elements: Vec<P<Type>> = Vec::with_capacity(element_types.len());
            for (i, &v) in element_types.iter().enumerate() {
                let mut new_element = v;
                if v.flags().intersects(TypeFlags::TypeParameter)
                    && element_infos[i].flags.intersects(ElementFlags::Variadic)
                {
                    let constraint = self.get_next_base_constraint(Some(v), stack);
                    if let Some(constraint) = constraint
                        && constraint != v
                        && every_type(self, constraint, |c, n| {
                            c.is_array_or_tuple_type(n)
                                && !checker::Checker::is_generic_tuple_type(n)
                        })
                    {
                        new_element = constraint;
                    }
                }
                new_elements.push(new_element);
            }
            return Some(self.create_tuple_type_ex(
                &new_elements,
                element_infos,
                t.target_tuple_type().readonly.get(),
            ));
        }
        Some(t)
    }

    // Go passes nil (e.g. a missing type parameter constraint) and checks `t == nil` here.
    // checker.go:28063
    fn get_next_base_constraint(
        &mut self,
        t: Option<P<Type>>,
        stack: BaseConstraintStack<'_>,
    ) -> Option<P<Type>> {
        let t = t?;
        let constraint = self.get_resolved_base_constraint(t, stack);
        if constraint == self.no_constraint_type || constraint == self.circular_constraint_type {
            return None;
        }
        Some(constraint)
    }

    // Return true if type might be of the given kind. A union or intersection type might be of a given
    // kind if at least one constituent type is of the given kind.
    // checker.go:28076
    /// tsrs: the constituents' flags are tested in the loop, so only nested unions and intersections recurse
    /// (0.9G calls on the 38k-file codebase, most of them for constituents of large unions).
    #[inline]
    pub(crate) fn maybe_type_of_kind(&mut self, t: P<Type>, kind: TypeFlags) -> bool {
        // A union of primitives has no constituent of these kinds (`NotPrimitiveUnion` in getUnionType); `kind` is
        // nearly always a constant, so this test folds away for the other kinds.
        const NOT_IN_PRIMITIVE_UNION: TypeFlags = TypeFlags::Any
            .union(TypeFlags::Unknown)
            .union(TypeFlags::Void)
            .union(TypeFlags::Never)
            .union(TypeFlags::Object)
            .union(TypeFlags::Intersection)
            .union(TypeFlags::Instantiable);

        let flags = t.flags();
        if flags.intersects(kind) {
            return true;
        }
        if !flags.intersects(TypeFlags::UnionOrIntersection) {
            return false;
        }
        if NOT_IN_PRIMITIVE_UNION.contains(kind)
            && t.object_flags().intersects(ObjectFlags::PrimitiveUnion)
        {
            return false;
        }
        if t.types().len() >= 8 {
            return self.constituent_flags(t).intersects(kind);
        }
        maybe_constituent_of_kind(t.types(), kind)
    }

    /// The flags of the constituents of the union or intersection `t`, nested unions' and intersections' included:
    /// `maybe_type_of_kind` of a large union walks all of them for every kind asked, and they never change. Kept
    /// for 8 or more constituents only.
    #[inline(never)]
    fn constituent_flags(&mut self, t: P<Type>) -> TypeFlags {
        let large = t.types().len() >= 8;
        if large && let Some(&flags) = self.constituent_flags.get(&t) {
            return flags;
        }
        let mut flags = TypeFlags::None;
        for &c in t.types() {
            flags |= c.flags();
            if c.flags().intersects(TypeFlags::UnionOrIntersection) {
                flags |= self.constituent_flags(c);
            }
        }
        if large {
            self.constituent_flags.insert(t, flags);
        }
        flags
    }

    // checker.go:28090
    pub(crate) fn maybe_type_of_kind_considering_base_constraint(
        &mut self,
        t: P<Type>,
        kind: TypeFlags,
    ) -> bool {
        if self.maybe_type_of_kind(t, kind) {
            return true;
        }
        let base_constraint = self.get_base_constraint_or_type(t);
        self.maybe_type_of_kind(base_constraint, kind)
    }

    // checker.go:28098
    pub(crate) fn all_types_assignable_to_kind(
        &mut self,
        source: P<Type>,
        kind: TypeFlags,
    ) -> bool {
        self.all_types_assignable_to_kind_ex(source, kind, false)
    }

    // checker.go:28102
    pub(crate) fn all_types_assignable_to_kind_ex(
        &mut self,
        source: P<Type>,
        kind: TypeFlags,
        strict: bool,
    ) -> bool {
        if source.flags().intersects(TypeFlags::Union) {
            return source
                .types()
                .iter()
                .all(|&sub_type| self.all_types_assignable_to_kind_ex(sub_type, kind, strict));
        }
        self.is_type_assignable_to_kind_ex(source, kind, strict)
    }

    // checker.go:28111
    pub(crate) fn is_type_assignable_to_kind(&mut self, source: P<Type>, kind: TypeFlags) -> bool {
        self.is_type_assignable_to_kind_ex(source, kind, false)
    }

    // checker.go:28115
    pub(crate) fn is_type_assignable_to_kind_ex(
        &mut self,
        source: P<Type>,
        kind: TypeFlags,
        strict: bool,
    ) -> bool {
        if source.flags().intersects(kind) {
            return true;
        }
        if strict
            && source.flags().intersects(
                TypeFlags::AnyOrUnknown | TypeFlags::Void | TypeFlags::Undefined | TypeFlags::Null,
            )
        {
            return false;
        }
        kind.intersects(TypeFlags::NumberLike)
            && self.is_type_assignable_to(source, self.number_type)
            || kind.intersects(TypeFlags::BigIntLike)
                && self.is_type_assignable_to(source, self.bigint_type)
            || kind.intersects(TypeFlags::StringLike)
                && self.is_type_assignable_to(source, self.string_type)
            || kind.intersects(TypeFlags::BooleanLike)
                && self.is_type_assignable_to(source, self.boolean_type)
            || kind.intersects(TypeFlags::Void)
                && self.is_type_assignable_to(source, self.void_type)
            || kind.intersects(TypeFlags::Never)
                && self.is_type_assignable_to(source, self.never_type)
            || kind.intersects(TypeFlags::Null)
                && self.is_type_assignable_to(source, self.null_type)
            || kind.intersects(TypeFlags::Undefined)
                && self.is_type_assignable_to(source, self.undefined_type)
            || kind.intersects(TypeFlags::ESSymbol)
                && self.is_type_assignable_to(source, self.es_symbol_type)
            || kind.intersects(TypeFlags::NonPrimitive)
                && self.is_type_assignable_to(source, self.non_primitive_type)
    }
}

// checker.go:28134
pub(crate) fn is_const_enum_object_type(t: P<Type>) -> bool {
    t.object_flags().intersects(ObjectFlags::Anonymous)
        && t.symbol().is_some_and(is_const_enum_symbol)
}

// checker.go:28138
pub(crate) fn is_const_enum_symbol(symbol: P<Symbol>) -> bool {
    symbol.flags().intersects(SymbolFlags::ConstEnum)
}

impl Checker {
    // checker.go:28142
    pub(crate) fn compare_properties(
        &mut self,
        source_prop: P<Symbol>,
        target_prop: P<Symbol>,
        mut compare_types: impl FnMut(&mut Checker, P<Type>, P<Type>) -> Ternary,
    ) -> Ternary {
        // Two members are considered identical when
        // - they are public properties with identical names, optionality, and types,
        // - they are private or protected properties originating in the same declaration and having identical types
        if source_prop == target_prop {
            return Ternary::TRUE;
        }
        let source_prop_accessibility = get_declaration_modifier_flags_from_symbol(source_prop)
            & ModifierFlags::NonPublicAccessibilityModifier;
        let target_prop_accessibility = get_declaration_modifier_flags_from_symbol(target_prop)
            & ModifierFlags::NonPublicAccessibilityModifier;
        if source_prop_accessibility != target_prop_accessibility {
            return Ternary::FALSE;
        }
        if source_prop_accessibility != ModifierFlags::None {
            if self.get_target_symbol(source_prop) != self.get_target_symbol(target_prop) {
                return Ternary::FALSE;
            }
        } else if (source_prop.flags() & SymbolFlags::Optional)
            != (target_prop.flags() & SymbolFlags::Optional)
        {
            return Ternary::FALSE;
        }
        if self.is_readonly_symbol(source_prop) != self.is_readonly_symbol(target_prop) {
            return Ternary::FALSE;
        }
        let s = self.get_non_missing_type_of_symbol(source_prop);
        let t = self.get_non_missing_type_of_symbol(target_prop);
        compare_types(self, s, t)
    }
}

// checker.go:28169
pub(crate) fn compare_types_equal(s: P<Type>, t: P<Type>) -> Ternary {
    if s == t {
        return Ternary::TRUE;
    }
    Ternary::FALSE
}

impl Checker {
    // checker.go:28176
    pub(crate) fn mark_property_as_referenced(
        &mut self,
        prop: P<Symbol>,
        node_for_check_write_only: Option<P<Node>>,
        is_self_type_access: bool,
    ) {
        let Some(value_declaration) =
            prop.value_declaration().filter(|_| prop.flags().intersects(SymbolFlags::ClassMember))
        else {
            return;
        };
        let has_private_modifier = has_modifier(value_declaration, ModifierFlags::Private);
        let has_private_identifier = value_declaration.name().is_some_and(is_private_identifier);
        if !has_private_modifier && !has_private_identifier {
            return;
        }
        if node_for_check_write_only.is_some_and(is_write_only_access)
            && !prop.flags().intersects(SymbolFlags::SetAccessor)
        {
            return;
        }
        if is_self_type_access {
            // Find any FunctionLikeDeclaration because those create a new 'this' binding. But this should only matter for methods (or getters/setters).
            let containing_method =
                find_ancestor(node_for_check_write_only, is_function_like_declaration);
            if containing_method.is_some_and(|m| m.symbol() == Some(prop)) {
                return;
            }
        }
        let mut target = prop;
        if prop.check_flags().intersects(CheckFlags::Instantiated) {
            target = self.value_symbol_links.get(prop).target().unwrap();
        }
        self.symbol_reference_links.add_reference_kinds(target, SymbolFlags::All);
    }
}

// checker.go:28257
pub(crate) fn has_rest_parameter(signature: P<Node>) -> bool {
    let last = signature.parameters().last().copied();
    last.is_some_and(is_rest_parameter)
}

// checker.go:28262
pub(crate) fn is_rest_parameter(param: P<Node>) -> bool {
    param.as_parameter_declaration().dot_dot_dot_token().is_some()
}

// checker.go:28266
pub(crate) fn get_name_from_index_info(info: P<IndexInfo>) -> String {
    if let Some(declaration) = info.declaration() {
        return tsrs_scanner::declaration_name_to_string(declaration.parameters()[0].name());
    }
    "x".to_string()
}

impl Checker {
    // checker.go:28273
    #[expect(
        clippy::inline_always,
        reason = "measured: plain #[inline] leaves one hot call out of line (notes/perf-checker-inline-cold.md)"
    )]
    #[inline(always)]
    pub(crate) fn is_unknown_like_union_type(&mut self, t: P<Type>) -> bool {
        if !self.strict_null_checks || !t.flags().intersects(TypeFlags::Union) {
            return false;
        }
        let flags = t.object_flags_lazy();
        if flags.intersects(ObjectFlags::IsUnknownLikeUnionComputed) {
            return flags.intersects(ObjectFlags::IsUnknownLikeUnion);
        }
        self.compute_is_unknown_like_union_type(t)
    }

    /// Computes the memoized answer once; repeated relation checks take the inline flag-only path above.
    #[cold]
    #[inline(never)]
    fn compute_is_unknown_like_union_type(&mut self, t: P<Type>) -> bool {
        debug_assert!(self.strict_null_checks && t.flags().intersects(TypeFlags::Union));
        debug_assert!(!t.object_flags_lazy().intersects(ObjectFlags::IsUnknownLikeUnionComputed));
        t.object_flags.set(t.object_flags_lazy() | ObjectFlags::IsUnknownLikeUnionComputed);
        let types = t.types();
        if types.len() >= 3
            && types[0].flags().intersects(TypeFlags::Undefined)
            && types[1].flags().intersects(TypeFlags::Null)
            && types.iter().any(|&t| self.is_empty_anonymous_object_type(t))
        {
            t.object_flags.set(t.object_flags_lazy() | ObjectFlags::IsUnknownLikeUnion);
        }
        t.object_flags_lazy().intersects(ObjectFlags::IsUnknownLikeUnion)
    }

    // Return true the given type is a primitive union type where no two literal type constituents are
    // comparable. Specifically, that means (a) the union doesn't contain literals from different enum
    // types, and (b) the union doesn't contain both enum literals and string or number literals.
    // checker.go:28290
    pub(crate) fn is_uniform_union_type(&mut self, t: P<Type>) -> bool {
        if t.object_flags().intersects(ObjectFlags::PrimitiveUnion) {
            if !t.object_flags_lazy().intersects(ObjectFlags::IsUniformEnumComputed) {
                let uniform = self.compute_is_uniform_union_type(t.types());
                t.object_flags.set(
                    t.object_flags_lazy()
                        | ObjectFlags::IsUniformEnumComputed
                        | if_else(uniform, ObjectFlags::IsUniformEnum, ObjectFlags::None),
                );
            }
            return t.object_flags_lazy().intersects(ObjectFlags::IsUniformEnum);
        }
        false
    }
}
