use crate::*;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

// Non-function declarations in checker.go:28300-30509 (hand-ported by checker-foundation):
//   const (checker.go:29618): maxTemplateLiteralTypeLength, maxTemplateLiteralTypeSpans

impl Checker {
    // checker.go:28300
    pub(crate) fn compute_is_uniform_union_type(&mut self, types: &[P<Type>]) -> bool {
        let mut enum_symbol: Option<P<Symbol>> = None;
        let mut has_string_or_number_literal = false;
        for &t in types {
            if t.flags().intersects(TypeFlags::EnumLike) {
                if has_string_or_number_literal {
                    return false;
                }
                let parent = self.get_parent_of_symbol(t.symbol().unwrap());
                if enum_symbol.is_none() {
                    enum_symbol = parent;
                } else if enum_symbol != parent {
                    return false;
                }
            } else if t.flags().intersects(TypeFlags::StringOrNumberLiteral) {
                if enum_symbol.is_some() {
                    return false;
                }
                has_string_or_number_literal = true;
            }
        }
        true
    }

    // checker.go:28324
    pub(crate) fn contains_undefined_type(t: P<Type>) -> bool {
        let mut t = t;
        if t.flags().intersects(TypeFlags::Union) {
            t = t.types()[0];
        }
        t.flags().intersects(TypeFlags::Undefined)
    }

    // checker.go:28331
    pub(crate) fn type_has_call_or_construct_signatures(&mut self, t: P<Type>) -> bool {
        t.flags().intersects(TypeFlags::StructuredType)
            && !self.resolve_structured_type_members(t).signatures().is_empty()
    }

    // checker.go:28335
    /// tsrs: the types every branch below passes through unchanged (no fresh literal, reference, union,
    /// intersection, substitution or simplifiable type) return inline; the relater normalizes both sides of every
    /// relation (0.5G calls on the 38k-file codebase).
    #[inline]
    pub(crate) fn get_normalized_type(&mut self, t: P<Type>, writing: bool) -> P<Type> {
        if !t.flags().intersects(
            TypeFlags::Freshable
                | TypeFlags::UnionOrIntersection
                | TypeFlags::Substitution
                | TypeFlags::Simplifiable,
        ) && !t.object_flags().intersects(ObjectFlags::Reference)
        {
            return t;
        }
        self.get_normalized_type_worker(t, writing)
    }

    #[inline(never)]
    fn get_normalized_type_worker(&mut self, t: P<Type>, writing: bool) -> P<Type> {
        let mut t = t;
        loop {
            let n: P<Type>;
            if is_fresh_literal_type(t) {
                n = t.as_literal_type().regular_type.get().unwrap();
            } else if Self::is_generic_tuple_type(t) {
                n = self.get_normalized_tuple_type(t, writing);
            } else if t.object_flags().intersects(ObjectFlags::Reference) {
                if t.as_type_reference().node.get().is_some() {
                    let type_arguments = self.get_type_arguments(t);
                    n = self.create_type_reference(t.target().unwrap(), type_arguments);
                } else {
                    n = match self.get_single_base_for_non_augmenting_subtype(t) {
                        Some(n) => n,
                        None => t,
                    };
                }
            } else if t.flags().intersects(TypeFlags::UnionOrIntersection) {
                n = self.get_normalized_union_or_intersection_type(t, writing);
            } else if t.flags().intersects(TypeFlags::Substitution) {
                if writing {
                    n = t.as_substitution_type().base_type.get().unwrap();
                } else {
                    n = self.get_substitution_intersection(t);
                }
            } else if t.flags().intersects(TypeFlags::Simplifiable) {
                n = self.get_simplified_type(t, writing);
            } else {
                return t;
            }
            if n == t {
                return n;
            }
            t = n;
        }
    }

    // checker.go:28372
    pub(crate) fn get_simplified_type(&mut self, t: P<Type>, writing: bool) -> P<Type> {
        if t.flags().intersects(TypeFlags::IndexedAccess) {
            return self.get_simplified_indexed_access_type(t, writing);
        } else if t.flags().intersects(TypeFlags::Conditional) {
            return self.get_simplified_conditional_type(t, writing);
        } else if t.flags().intersects(TypeFlags::Index) {
            return self.get_simplified_index_type(t);
        }
        t
    }

    // checker.go:28446
    fn get_simplified_index_type(&mut self, t: P<Type>) -> P<Type> {
        let target = t.as_index_type().target().unwrap();
        if self.is_generic_mapped_type(target)
            && self.get_name_type_from_mapped_type(target).is_some()
            && !Self::is_mapped_type_with_keyof_constraint_declaration(target)
        {
            return self.get_index_type_for_mapped_type(target, IndexFlags::None);
        }
        t
    }

    // Transform an indexed access to a simpler form, if possible. Return the simpler form, or return
    // the type itself if no transformation is possible. The writing flag indicates that the type is
    // the target of an assignment.
    // checker.go:28385
    fn get_simplified_indexed_access_type(&mut self, t: P<Type>, writing: bool) -> P<Type> {
        let key = CachedTypeKey {
            kind: if_else(
                writing,
                CachedTypeKind::IndexedAccessForWriting,
                CachedTypeKind::IndexedAccessForReading,
            ),
            type_id: t.id,
        };
        if let Some(&cached) = self.cached_types.get(&key) {
            return if_else(cached == self.circular_constraint_type, t, cached);
        }
        self.cached_types.insert(key, t);
        let mut result = self.get_simplified_indexed_access_type_worker(t, writing);
        if result != t {
            // If the simplification is a union type that includes t, remove t from the type.
            result = self.remove_type(result, t);
            self.cached_types.insert(key, result);
        }
        result
    }

    // checker.go:28400
    fn get_simplified_indexed_access_type_worker(&mut self, t: P<Type>, writing: bool) -> P<Type> {
        // We recursively simplify the object type as it may in turn be an indexed access type. For example, with
        // '{ [P in T]: { [Q in U]: number } }[T][U]' we want to first simplify the inner indexed access type.
        let object_type = self
            .get_simplified_type(t.as_indexed_access_type().object_type.get().unwrap(), writing);
        let index_type =
            self.get_simplified_type(t.as_indexed_access_type().index_type.get().unwrap(), writing);
        // T[A | B] -> T[A] | T[B] (reading)
        // T[A | B] -> T[A] & T[B] (writing)
        if let Some(distributed_over_index) =
            self.distribute_object_over_index_type(object_type, index_type, writing)
        {
            return distributed_over_index;
        }
        // Only do the inner distributions if the index can no longer be instantiated to cause index distribution again
        if !index_type.flags().intersects(TypeFlags::Instantiable) {
            // (T | U)[K] -> T[K] | U[K] (reading)
            // (T | U)[K] -> T[K] & U[K] (writing)
            // (T & U)[K] -> T[K] & U[K]
            if let Some(distributed_over_object) =
                self.distribute_index_over_object_type(object_type, index_type, writing)
            {
                return distributed_over_object;
            }
        }
        // So ultimately (reading):
        // ((A & B) | C)[K1 | K2] -> ((A & B) | C)[K1] | ((A & B) | C)[K2] -> (A & B)[K1] | C[K1] | (A & B)[K2] | C[K2] -> (A[K1] & B[K1]) | C[K1] | (A[K2] & B[K2]) | C[K2]
        // A generic tuple type indexed by a number exists only when the index type doesn't select a
        // fixed element. We simplify to either the combined type of all elements (when the index type
        // the actual number type) or to the combined type of all non-fixed elements.
        if Self::is_generic_tuple_type(object_type)
            && index_type.flags().intersects(TypeFlags::NumberLike)
        {
            let index = if_else(
                index_type.flags().intersects(TypeFlags::Number),
                0,
                object_type.target_tuple_type().fixed_length.get(),
            );
            if let Some(element_type) = self.get_element_type_of_slice_of_tuple_type(
                object_type,
                index,
                0, /*endSkipCount*/
                writing,
                false,
            ) {
                return element_type;
            }
        }
        // If the object type is a mapped type { [P in K]: E }, where K is generic, or { [P in K as N]: E }, where
        // K is generic and N is assignable to P, instantiate E using a mapper that substitutes the index type for P.
        // For example, for an index access { [P in K]: Box<T[P]> }[X], we construct the type Box<T[X]>.
        if self.is_generic_mapped_type(object_type)
            && self.get_mapped_type_name_type_kind(object_type) != MappedTypeNameTypeKind::Remapping
        {
            let substituted = self.substitute_indexed_mapped_type(
                object_type,
                t.as_indexed_access_type().index_type.get().unwrap(),
            );
            return self
                .map_type(substituted, |c, t| Some(c.get_simplified_type(t, writing)))
                .unwrap();
        }
        t
    }

    // checker.go:28445
    fn distribute_object_over_index_type(
        &mut self,
        object_type: P<Type>,
        index_type: P<Type>,
        writing: bool,
    ) -> Option<P<Type>> {
        // T[A | B] -> T[A] | T[B] (reading)
        // T[A | B] -> T[A] & T[B] (writing)
        if index_type.flags().intersects(TypeFlags::Union) {
            let types: Vec<P<Type>> = index_type
                .types()
                .iter()
                .map(|&t| {
                    let indexed = self.get_indexed_access_type(object_type, t);
                    self.get_simplified_type(indexed, writing)
                })
                .collect();
            if writing {
                return Some(self.get_intersection_type(&types));
            }
            return Some(self.get_union_type(&types));
        }
        None
    }

    // checker.go:28460
    pub(crate) fn distribute_index_over_object_type(
        &mut self,
        object_type: P<Type>,
        index_type: P<Type>,
        writing: bool,
    ) -> Option<P<Type>> {
        // (T | U)[K] -> T[K] | U[K] (reading)
        // (T | U)[K] -> T[K] & U[K] (writing)
        // (T & U)[K] -> T[K] & U[K]
        if object_type.flags().intersects(TypeFlags::Union)
            || object_type.flags().intersects(TypeFlags::Intersection)
                && !self.should_defer_index_type(object_type, IndexFlags::None)
        {
            let types: Vec<P<Type>> = object_type
                .types()
                .iter()
                .map(|&t| {
                    let indexed = self.get_indexed_access_type(t, index_type);
                    self.get_simplified_type(indexed, writing)
                })
                .collect();
            if object_type.flags().intersects(TypeFlags::Intersection) || writing {
                return Some(self.get_intersection_type(&types));
            }
            return Some(self.get_union_type(&types));
        }
        None
    }

    // checker.go:28476
    fn get_simplified_conditional_type(&mut self, t: P<Type>, writing: bool) -> P<Type> {
        let key = CachedTypeKey {
            kind: if_else(
                writing,
                CachedTypeKind::SimplifiedConditionalTypeForWriting,
                CachedTypeKind::SimplifiedConditionalTypeForReading,
            ),
            type_id: t.id,
        };
        if let Some(&cached) = self.cached_types.get(&key) {
            return cached;
        }
        let result = self.get_simplified_conditional_type_worker(t, writing);
        self.cached_types.insert(key, result);
        result
    }

    fn get_simplified_conditional_type_worker(&mut self, t: P<Type>, writing: bool) -> P<Type> {
        let check_type = t.as_conditional_type().check_type.get().unwrap();
        let extends_type = t.as_conditional_type().extends_type.get().unwrap();
        let true_type = self.get_true_type_from_conditional_type(t);
        let false_type = self.get_false_type_from_conditional_type(t);
        // Simplifications for types of the form `T extends U ? T : never` and `T extends U ? never : T`.
        if false_type.flags().intersects(TypeFlags::Never)
            && self.get_actual_type_variable(true_type) == self.get_actual_type_variable(check_type)
        {
            if check_type.flags().intersects(TypeFlags::Any) || {
                let s = self.get_restrictive_instantiation(check_type);
                let e = self.get_restrictive_instantiation(extends_type);
                self.is_type_assignable_to(s, e)
            } {
                return self.get_simplified_type(true_type, writing);
            } else if self.is_intersection_empty(check_type, extends_type) {
                return self.never_type;
            }
        } else if true_type.flags().intersects(TypeFlags::Never)
            && self.get_actual_type_variable(false_type)
                == self.get_actual_type_variable(check_type)
        {
            if !check_type.flags().intersects(TypeFlags::Any) && {
                let s = self.get_restrictive_instantiation(check_type);
                let e = self.get_restrictive_instantiation(extends_type);
                self.is_type_assignable_to(s, e)
            } {
                return self.never_type;
            } else if check_type.flags().intersects(TypeFlags::Any)
                || self.is_intersection_empty(check_type, extends_type)
            {
                return self.get_simplified_type(false_type, writing);
            }
        }
        t
    }

    // Invokes union simplification logic to determine if an intersection is considered empty as a union constituent
    // checker.go:28499
    fn is_intersection_empty(&mut self, type1: P<Type>, type2: P<Type>) -> bool {
        let intersection = self.intersect_types(Some(type1), Some(type2)).unwrap();
        let never_type = self.never_type;
        self.get_union_type(&[intersection, never_type]).flags().intersects(TypeFlags::Never)
    }

    // checker.go:28503
    pub(crate) fn get_simplified_type_or_constraint(&mut self, t: P<Type>) -> Option<P<Type>> {
        let simplified = self.get_simplified_type(t, false /*writing*/);
        if simplified != t {
            return Some(simplified);
        }
        self.get_constraint_of_type(t)
    }

    // checker.go:28510
    fn get_normalized_union_or_intersection_type(&mut self, t: P<Type>, writing: bool) -> P<Type> {
        let reduced = self.get_reduced_type(t);
        if reduced != t {
            return reduced;
        }
        if t.flags().intersects(TypeFlags::Intersection) && self.should_normalize_intersection(t) {
            // Normalization handles cases like
            // Partial<T>[K] & ({} | null) ==>
            // Partial<T>[K] & {} | Partial<T>[K} & null ==>
            // (T[K] | undefined) & {} | (T[K] | undefined) & null ==>
            // T[K] & {} | undefined & {} | T[K] & null | undefined & null ==>
            // T[K] & {} | T[K] & null
            let types = t.types();
            let normalized_types = same_map(types, |&u| self.get_normalized_type(u, writing));
            if !same(&normalized_types, types) {
                return self.get_intersection_type(&normalized_types);
            }
        }
        t
    }

    // checker.go:28530
    fn should_normalize_intersection(&mut self, t: P<Type>) -> bool {
        let flags = t.object_flags_lazy();
        if flags.intersects(ObjectFlags::ShouldNormalizeComputed) {
            return flags.intersects(ObjectFlags::ShouldNormalize);
        }
        let result = self.should_normalize_intersection_worker(t);
        t.object_flags.set(
            t.object_flags_lazy()
                | ObjectFlags::ShouldNormalizeComputed
                | if result { ObjectFlags::ShouldNormalize } else { ObjectFlags::None },
        );
        result
    }

    fn should_normalize_intersection_worker(&mut self, t: P<Type>) -> bool {
        let mut has_instantiable = false;
        let mut has_nullable_or_empty = false;
        for &t in t.types() {
            has_instantiable = has_instantiable || t.flags().intersects(TypeFlags::Instantiable);
            has_nullable_or_empty = has_nullable_or_empty
                || t.flags().intersects(TypeFlags::Nullable)
                || self.is_empty_anonymous_object_type(t);
            if has_instantiable && has_nullable_or_empty {
                return true;
            }
        }
        false
    }

    // checker.go:28543
    fn get_normalized_tuple_type(&mut self, t: P<Type>, writing: bool) -> P<Type> {
        let elements = self.get_element_types(t);
        let normalized_elements = same_map(elements, |&t| {
            if t.flags().intersects(TypeFlags::Simplifiable) {
                return self.get_simplified_type(t, writing);
            }
            t
        });
        if !same(elements, &normalized_elements) {
            let normalized_elements = normalized_elements.into_owned();
            return self.create_normalized_tuple_type(t.target().unwrap(), &normalized_elements);
        }
        t
    }

    // checker.go:28557
    pub(crate) fn get_single_base_for_non_augmenting_subtype(
        &mut self,
        t: P<Type>,
    ) -> Option<P<Type>> {
        if !t.object_flags().intersects(ObjectFlags::Reference)
            || !t.target().unwrap().object_flags().intersects(ObjectFlags::ClassOrInterface)
        {
            return None;
        }
        let key = CachedTypeKey { kind: CachedTypeKind::EquivalentBaseType, type_id: t.id };
        let object_flags = t.object_flags_lazy();
        if object_flags.intersects(ObjectFlags::IdenticalBaseTypeCalculated) {
            // tsrs-only: `IdenticalBaseTypeExists` is set together with the cache entry below, so a reference
            // without it has no entry and the map is not probed (nearly every reference the relater normalizes
            // has no single base, and this is asked of both sides of every relation between references).
            if !object_flags.intersects(ObjectFlags::IdenticalBaseTypeExists) {
                debug_assert!(
                    !self.cached_types.contains_key(&key),
                    "a cached equivalent base type without IdenticalBaseTypeExists"
                );
                return None;
            }
            return self.cached_types.get(&key).copied();
        }
        t.object_flags.set(object_flags | ObjectFlags::IdenticalBaseTypeCalculated);
        let target = t.target().unwrap();
        if target.object_flags().intersects(ObjectFlags::Class) {
            let base_type_node = get_base_type_node_of_class(target);
            // A base type expression may circularly reference the class itself (e.g. as an argument to function call), so we only
            // check for base types specified as simple qualified names.
            if let Some(base_type_node) = base_type_node {
                let expression = base_type_node.expression().unwrap();
                if !is_identifier(expression) && !is_property_access_expression(expression) {
                    return None;
                }
            }
        }
        let bases = self.get_base_types(target);
        if bases.len() != 1 {
            return None;
        }
        if self.get_members_of_symbol(t.symbol().unwrap()).map_or(0, |m| m.len()) != 0 {
            // If the interface has any members, they may subtype members in the base, so we should do a full structural comparison
            return None;
        }
        let mut instantiated_base: P<Type>;
        let type_parameters = target.as_interface_type().type_parameters();
        if type_parameters.is_empty() {
            instantiated_base = bases[0];
        } else {
            let type_arguments = self.get_type_arguments(t);
            let mapper = new_type_mapper(
                type_parameters,
                alloc_slice(&type_arguments[..type_parameters.len()]),
            );
            instantiated_base = self.instantiate_type(bases[0], Some(mapper));
        }
        let type_arguments = self.get_type_arguments(t);
        if type_arguments.len() > type_parameters.len() {
            instantiated_base = self.get_type_with_this_argument(
                instantiated_base,
                last_or_nil(type_arguments),
                false,
            );
        }
        self.cached_types.insert(key, instantiated_base);
        t.object_flags.set(t.object_flags_lazy() | ObjectFlags::IdenticalBaseTypeExists);
        Some(instantiated_base)
    }

    // checker.go:28597
    pub(crate) fn get_modifiers_type_from_mapped_type(&mut self, t: P<Type>) -> P<Type> {
        let m = t.as_mapped_type();
        if m.modifiers_type.get().is_none() {
            if Self::is_mapped_type_with_keyof_constraint_declaration(t) {
                // If the constraint declaration is a 'keyof T' node, the modifiers type is T. We check
                // AST nodes here because, when T is a non-generic type, the logic below eagerly resolves
                // 'keyof T' to a literal union type and we can't recover T from that type.
                let type_node = Self::get_constraint_declaration_for_mapped_type(t)
                    .unwrap()
                    .type_node()
                    .unwrap();
                let from_node = self.get_type_from_type_node(type_node);
                m.modifiers_type.set(Some(self.instantiate_type(from_node, m.mapper.get())));
            } else {
                // Otherwise, get the declared constraint type, and if the constraint type is a type parameter,
                // get the constraint of that type parameter. If the resulting type is an indexed type 'keyof T',
                // the modifiers type is T. Otherwise, the modifiers type is unknown.
                let declared_type =
                    self.get_type_from_mapped_type_node(m.declaration.get().unwrap());
                let constraint = self.get_constraint_type_from_mapped_type(declared_type);
                let mut extended_constraint = Some(constraint);
                if constraint.flags().intersects(TypeFlags::TypeParameter) {
                    extended_constraint = self.get_constraint_of_type_parameter(
                        get_non_distributed_type_parameter(constraint).unwrap(),
                    );
                }
                if let Some(extended_constraint) =
                    extended_constraint.filter(|e| e.flags().intersects(TypeFlags::Index))
                {
                    m.modifiers_type.set(Some(self.instantiate_type(
                        extended_constraint.as_index_type().target.get().unwrap(),
                        m.mapper.get(),
                    )));
                } else {
                    m.modifiers_type.set(Some(self.unknown_type));
                }
            }
        }
        m.modifiers_type.get().unwrap()
    }

    // checker.go:28625
    pub(crate) fn extract_types_of_kind(&mut self, t: P<Type>, kind: TypeFlags) -> P<Type> {
        self.filter_type(t, |_c, t| t.flags().intersects(kind))
    }

    // checker.go:28629
    pub(crate) fn get_regular_type_of_object_literal(&mut self, t: P<Type>) -> P<Type> {
        if !(is_object_literal_type(t) && t.object_flags().intersects(ObjectFlags::FreshLiteral)) {
            return t;
        }
        let key = CachedTypeKey { kind: CachedTypeKind::RegularObjectLiteral, type_id: t.id };
        if let Some(&cached) = self.cached_types.get(&key) {
            return cached;
        }
        let resolved = self.resolve_structured_type_members(t);
        let members = self.transform_type_of_members(
            t,
            super::checker::Checker::get_regular_type_of_object_literal,
        );
        let regular = self.new_anonymous_type(
            t.symbol(),
            Some(members),
            resolved.call_signatures(),
            resolved.construct_signatures(),
            resolved.index_infos(),
        );
        // resolved is t's own structured data, so resolved.flags/objectFlags are t's header flags
        regular.flags.set(t.flags());
        regular.object_flags.set(
            regular.object_flags_lazy()
                | (t.object_flags_lazy()
                    & !(ObjectFlags::FreshLiteral | ObjectFlags::WeakTypeMemo)),
        );
        self.cached_types.insert(key, regular);
        regular
    }

    // checker.go:28646
    fn transform_type_of_members(
        &mut self,
        t: P<Type>,
        mut f: impl FnMut(&mut Checker, P<Type>) -> P<Type>,
    ) -> P<SymbolTable> {
        let members = SymbolTable::new();
        for property in self.get_properties_of_object_type(t).iter().copied() {
            let mut property = property;
            let original = self.get_type_of_symbol(property);
            let updated = f(self, original);
            if updated != original {
                property = self.create_symbol_with_type(property, Some(updated));
            }
            members.set(property.name(), property);
        }
        members
    }

    // checker.go:28659
    pub(crate) fn mark_linked_references(
        &mut self,
        location: P<Node>,
        hint: ReferenceHint,
        prop_symbol: Option<P<Symbol>>,
        parent_type: Option<P<Type>>,
    ) {
        if !self.can_collect_symbol_alias_accessibility_data {
            return;
        }
        if location.flags().intersects(NodeFlags::Ambient)
            && !is_property_signature_declaration(location)
            && !is_property_declaration(location)
        {
            // References within types and declaration files are never going to contribute to retaining a JS import,
            // except for properties (which can be decorated).
            return;
        }
        match hint {
            ReferenceHint::Identifier => self.mark_identifier_alias_referenced(location),
            ReferenceHint::Property => {
                self.mark_property_alias_referenced(location, prop_symbol, parent_type);
            }
            ReferenceHint::ExportAssignment => {
                self.mark_export_assignment_alias_referenced(location);
            }
            ReferenceHint::Jsx => self.mark_jsx_alias_referenced(location),
            ReferenceHint::ExportImportEquals => self.mark_import_equals_alias_referenced(location),
            ReferenceHint::ExportSpecifier => self.mark_export_specifier_alias_referenced(location),
            ReferenceHint::Decorator => self.mark_decorator_alias_referenced(location),
            ReferenceHint::Unspecified => {
                if location.flags().intersects(NodeFlags::InWithStatement) {
                    // We cannot answer semantic questions within a with block, do not proceed any further
                    return;
                }
                if is_jsx_tag_name(location) && is_jsx_intrinsic_tag_name(location) {
                    return; // builtin JSX tag names aren't real type refs by most metrics, but are expressions, so must be filtered
                }
                if is_identifier(location) {
                    // A shorthand property with an object-assignment-initializer (e.g. `{ s = 5 }`) is only valid inside a
                    // destructuring assignment target. When it appears in an ordinary object literal expression, the checker
                    // checks the initializer and never resolves the property name, so resolving it here would report a spurious
                    // "No value exists in scope for the shorthand property" diagnostic. Skip such names to match checking.
                    let parent = location.parent().unwrap();
                    if is_shorthand_property_assignment(parent)
                        && parent.name() == Some(location)
                        && parent
                            .as_shorthand_property_assignment()
                            .object_assignment_initializer()
                            .is_some()
                        && !is_assignment_target(parent.parent().unwrap())
                    {
                        return;
                    }
                    // Go: ast.FindManyAncestors(location, IsMetaProperty, IsDecorator, IsForInOrOfStatement,
                    // IsComputedPropertyName, IsHeritageClause). The predicates test disjoint kinds, so one walk with a
                    // `match` finds the same nearest ancestors (this runs for every identifier emit visits).
                    let (
                        mut meta_property,
                        mut decorator,
                        mut for_node,
                        mut computed_name,
                        mut heritage_clause,
                    ) = (None, None, None, None, None);
                    let mut ancestor = Some(location);
                    while let Some(n) = ancestor {
                        let slot = match n.kind() {
                            Kind::MetaProperty => &mut meta_property,
                            Kind::Decorator => &mut decorator,
                            Kind::ForInStatement | Kind::ForOfStatement => &mut for_node,
                            Kind::ComputedPropertyName => &mut computed_name,
                            Kind::HeritageClause => &mut heritage_clause,
                            _ => {
                                ancestor = n.parent();
                                continue;
                            }
                        };
                        if slot.is_none() {
                            *slot = Some(n);
                            if meta_property.is_some()
                                && decorator.is_some()
                                && for_node.is_some()
                                && computed_name.is_some()
                                && heritage_clause.is_some()
                            {
                                break;
                            }
                        }
                        ancestor = n.parent();
                    }
                    if meta_property.is_some() {
                        return; // identifiers in meta properties shouldn't be resolved, but are expressions, so must be filtered
                    }
                    if let Some(decorator) = decorator {
                        // Decorators on nodes that cannot be decorated (e.g. class expressions, static blocks,
                        // `this` parameters) are never resolved during normal checking, so resolving them here would
                        // report spurious diagnostics. Only bail out for such invalid-position decorators; valid
                        // decorator expressions must still be resolved and marked for emit.
                        if let Some(decorated) = decorator.parent() {
                            let decorated_parent = decorated.parent();
                            if !node_can_be_decorated(
                                self.legacy_decorators,
                                decorated,
                                decorated_parent,
                                decorated_parent.unwrap().parent(),
                            ) {
                                return;
                            }
                        }
                    }
                    // The right-hand side of a 'for-in'/'for-of' statement whose initializer is an empty variable
                    // declaration list (a grammar error, e.g. `for (var of X)`) is never checked, because the RHS
                    // is only checked while inferring the type of a variable declaration and there is none here.
                    // Resolving identifiers in the RHS here would report spurious diagnostics.
                    if let Some(for_node) = for_node {
                        let data = for_node.as_for_in_or_of_statement();
                        let initializer = data.initializer;
                        if is_variable_declaration_list(initializer)
                            && initializer
                                .as_variable_declaration_list()
                                .declarations
                                .nodes()
                                .is_empty()
                            && (location == data.expression
                                || ast::is_node_descendant_of(location, data.expression))
                        {
                            return;
                        }
                    }
                    if let Some(computed_name) = computed_name
                        && is_invalid_computed_property_name(computed_name)
                    {
                        return;
                    }
                    if let Some(heritage_clause) = heritage_clause {
                        let hc_parent = heritage_clause.parent().unwrap();
                        // extends heritage clauses on interfaces are not expressions and are unchecked if they are
                        if is_interface_declaration(hc_parent) {
                            return;
                        }
                        // On a class, only the first `extends` type is resolved as a value (the base class); any
                        // additional `extends` types are grammar errors (e.g. `class C extends A extends B` or
                        // `class C extends A, B`) and are never resolved during checking.
                        if is_class_like(hc_parent)
                            && heritage_clause.as_heritage_clause().token == Kind::ExtendsKeyword
                            && let Some(first_extends) =
                                get_class_extends_heritage_element(hc_parent)
                            && location != first_extends
                            && !ast::is_node_descendant_of(location, first_extends)
                        {
                            return;
                        }
                    }
                    // Identifiers in expression contexts are emitted, so we need to follow their referenced aliases and mark them as used
                    // Some non-expression identifiers are also treated as expression identifiers for this purpose, eg, `a` in `b = {a}` or `q` in `import r = q`
                    // This is the exception, rather than the rule - most non-expression identifiers are declaration names.
                    if (is_expression_node(location)
                        || is_shorthand_property_assignment(location.parent().unwrap()))
                        && should_mark_identifier_alias_referenced(location)
                    {
                        let loc_parent = location.parent().unwrap();
                        if is_property_access_or_qualified_name(loc_parent) {
                            let left = if is_property_access_expression(loc_parent) {
                                loc_parent.expression().unwrap()
                            } else {
                                loc_parent.as_qualified_name().left
                            };
                            if left != location {
                                return; // Only mark the LHS (the RHS is a property lookup)
                            }
                        }
                        self.mark_identifier_alias_referenced(location);
                        return;
                    }
                }
                if is_property_access_or_qualified_name(location) {
                    let mut top_prop = location;
                    while is_property_access_or_qualified_name(top_prop) {
                        // Names in an import type's qualifier (`ns.y` in `typeof import("./b").ns.y`)
                        // are exports of the imported module, not references to this file's imports.
                        if is_part_of_type_node(top_prop)
                            || is_import_type_qualifier_part(top_prop).is_some()
                        {
                            return;
                        }
                        top_prop = top_prop.parent().unwrap();
                    }
                    self.mark_property_alias_referenced(
                        location, None, /*propSymbol*/
                        None, /*parentType*/
                    );
                    return;
                }
                if is_export_assignment(location) {
                    self.mark_export_assignment_alias_referenced(location);
                    return;
                }
                if is_jsx_opening_like_element(location) || is_jsx_opening_fragment(location) {
                    self.mark_jsx_alias_referenced(location);
                    return;
                }
                if is_import_equals_declaration(location) {
                    if is_internal_module_import_equals_declaration(location)
                        || self.check_external_import_or_export_declaration(location)
                    {
                        self.mark_import_equals_alias_referenced(location);
                        return;
                    }
                    return;
                }
                if is_export_specifier(location) {
                    self.mark_export_specifier_alias_referenced(location);
                    return;
                }
                if !self.compiler_options.emit_decorator_metadata.is_true() {
                    return;
                }
                if !can_have_decorators(location)
                    || !has_decorators(location)
                    || location.modifiers().is_none()
                    || !node_can_be_decorated(
                        self.legacy_decorators,
                        location,
                        location.parent(),
                        location.parent().unwrap().parent(),
                    )
                {
                    return;
                }

                self.mark_decorator_alias_referenced(location);
            }
        }
    }
}

// checker.go:28826
fn is_export_or_export_expression(location: P<Node>) -> bool {
    find_ancestor(location, |n| {
        if let Some(parent) = n.parent() {
            if is_any_export_assignment(parent) {
                return parent.expression() == Some(n) && is_entity_name_expression(n);
            }
            if is_export_specifier(parent) {
                return parent.as_export_specifier().name == n || parent.property_name() == Some(n);
            }
        }
        false
    })
    .is_some()
}

// checker.go:28841
pub(crate) fn should_mark_identifier_alias_referenced(node: P<Node>) -> bool {
    if let Some(parent) = node.parent() {
        // A property access expression LHS? checkPropertyAccessExpression will handle that.
        if is_property_access_expression(parent) && parent.expression() == Some(node) {
            return false;
        }
        // Next two check for an identifier inside a type only export.
        if is_export_specifier(parent) && parent.is_type_only() {
            return false;
        }
        if let Some(grandparent) = parent.parent()
            && let Some(great_grandparent) = grandparent.parent()
            && is_export_declaration(great_grandparent)
            && great_grandparent.is_type_only()
        {
            return false;
        }
    }
    true
}

// checker.go:28862
pub(crate) fn is_internal_module_import_equals_declaration(node: P<Node>) -> bool {
    node.kind() == Kind::ImportEqualsDeclaration
        && node.as_import_equals_declaration().module_reference.kind()
            != Kind::ExternalModuleReference
}

impl Checker {
    // checker.go:28867
    fn mark_identifier_alias_referenced(&mut self, location: P<Node>) {
        if is_this_in_type_query(location) {
            return;
        }
        let symbol = self.get_resolved_symbol(location);
        if symbol != self.arguments_symbol && symbol != self.unknown_symbol {
            self.mark_alias_referenced(symbol, location);
        }
    }

    // checker.go:28877
    fn mark_property_alias_referenced(
        &mut self,
        location: P<Node>,
        prop_symbol: Option<P<Symbol>>,
        parent_type: Option<P<Type>>,
    ) {
        if is_part_of_import_equals_module_reference(location) {
            return;
        }
        let left = if is_property_access_expression(location) {
            location.expression().unwrap()
        } else {
            location.as_qualified_name().left
        };
        if is_this_identifier(left) || !is_identifier(left) {
            return;
        }
        let parent_symbol = self.get_resolved_symbol(left);
        if parent_symbol == self.unknown_symbol {
            return;
        }
        // In `Foo.Bar.Baz`, 'Foo' is not referenced if 'Bar' is a const enum or a module containing only const enums.
        // `Foo` is also not referenced in `enum FooCopy { Bar = Foo.Bar }`, because the enum member value gets inlined
        // here even if `Foo` is not a const enum.
        //
        // The exceptions are:
        //   1. if 'isolatedModules' is enabled, because the const enum value will not be inlined, and
        //   2. if 'preserveConstEnums' is enabled and the expression is itself an export, e.g. `export = Foo.Bar.Baz`.
        //
        // The property lookup is deferred as much as possible, in as many situations as possible, to avoid alias marking
        // pulling on types/symbols it doesn't strictly need to.
        if self.compiler_options.get_isolated_modules()
            || (self.compiler_options.should_preserve_const_enums()
                && is_export_or_export_expression(location))
        {
            self.mark_alias_referenced(parent_symbol, location);
            return;
        }
        // Hereafter, this relies on type checking - but every check prior to this only used symbol information
        let left_type = match parent_type {
            Some(t) => t,
            None => self.check_expression_cached(left),
        };
        if is_type_any(Some(left_type)) || left_type == self.silent_never_type {
            self.mark_alias_referenced(parent_symbol, location);
            return;
        }
        let mut prop = prop_symbol;
        if prop.is_none() && parent_type.is_none() {
            let right = if is_property_access_expression(location) {
                location.as_property_access_expression().name
            } else {
                location.as_qualified_name().right
            };
            let mut lexically_scoped_symbol: Option<P<Symbol>> = None;
            if is_private_identifier(right) {
                lexically_scoped_symbol =
                    Self::lookup_symbol_for_private_identifier_declaration(right.text(), right);
            }
            let assignment_kind = get_assignment_target_kind(location);
            let apparent_type = if assignment_kind != AssignmentKind::None
                || Self::is_method_access_for_call(location)
            {
                let widened = self.get_widened_type(left_type);
                self.get_apparent_type(widened)
            } else {
                self.get_apparent_type(left_type)
            };
            if is_private_identifier(right) {
                if let Some(lexically_scoped_symbol) = lexically_scoped_symbol {
                    prop = self.get_private_identifier_property_of_type(
                        apparent_type,
                        lexically_scoped_symbol,
                    );
                }
            } else {
                prop = self.get_property_of_type(apparent_type, right.text());
            }
        }
        if !prop.is_some_and(|prop| {
            is_const_enum_or_const_enum_only_module(prop)
                || prop.flags().intersects(SymbolFlags::EnumMember)
                    && location.parent().unwrap().kind() == Kind::EnumMember
        }) {
            self.mark_alias_referenced(parent_symbol, location);
        }
    }
}

// checker.go:28949
fn is_part_of_import_equals_module_reference(location: P<Node>) -> bool {
    let Some(import_equals) = find_ancestor_kind(location, Kind::ImportEqualsDeclaration) else {
        return false;
    };
    let mut node = Some(location);
    while let Some(n) = node {
        if n == import_equals {
            break;
        }
        if n == import_equals.as_import_equals_declaration().module_reference {
            return true;
        }
        node = n.parent();
    }
    false
}

impl Checker {
    // checker.go:28962
    fn mark_export_assignment_alias_referenced(&mut self, location: P<Node>) {
        let id = location.expression().unwrap();
        if is_identifier(id) {
            let resolved = self.resolve_entity_name(
                id,
                SymbolFlags::All,
                true, /*ignoreErrors*/
                true, /*dontResolveAlias*/
                Some(location),
            );
            let sym = self.get_export_symbol_of_value_symbol_if_exported(resolved);
            if let Some(sym) = sym {
                self.mark_alias_referenced(sym, id);
            }
        }
    }

    // checker.go:28972
    pub(crate) fn mark_jsx_alias_referenced(&mut self, node: P<Node>) {
        if self.get_jsx_namespace_container_for_implicit_import(node).is_some() {
            return;
        }
        // The reactNamespace/jsxFactory's root symbol should be marked as 'used' so we don't incorrectly elide its import.
        // And if there is no reactNamespace/jsxFactory's symbol in scope when targeting React emit, we should issue an error.
        let jsx_factory_ref_err: Option<&'static Message> = if_else(
            self.compiler_options.jsx == JsxEmit::REACT,
            Some(&diagnostics::THIS_JSX_TAG_REQUIRES_0_TO_BE_IN_SCOPE_BUT_IT_COULD_NOT_BE_FOUND),
            None,
        );
        let jsx_factory_namespace = self.get_jsx_namespace(Some(node));
        let mut jsx_factory_location = node;
        if is_jsx_opening_like_element(node) {
            jsx_factory_location = node.tag_name();
        }
        let should_factory_ref_err = self.compiler_options.jsx != JsxEmit::PRESERVE
            && self.compiler_options.jsx != JsxEmit::REACT_NATIVE;
        // #38720/60122, allow null as jsxFragmentFactory
        let mut jsx_factory_sym: Option<P<Symbol>> = None;
        if !(is_jsx_opening_fragment(node) && jsx_factory_namespace == "null") {
            let mut flags = SymbolFlags::Value;
            if !should_factory_ref_err {
                flags &= !SymbolFlags::Enum;
            }
            jsx_factory_sym = self.resolve_name(
                Some(jsx_factory_location),
                &jsx_factory_namespace,
                flags,
                jsx_factory_ref_err,
                true,  /*isUse*/
                false, /*excludeGlobals*/
            );
        }
        if let Some(jsx_factory_sym) = jsx_factory_sym {
            // Mark local symbol as referenced here because it might not have been marked
            // if jsx emit was not jsxFactory as there wont be error being emitted
            self.symbol_referenced(jsx_factory_sym, SymbolFlags::All);
            // If react/jsxFactory symbol is alias, mark it as referenced
            if self.can_collect_symbol_alias_accessibility_data
                && jsx_factory_sym.flags().intersects(SymbolFlags::Alias)
                && self.get_type_only_alias_declaration(jsx_factory_sym).is_none()
            {
                self.mark_alias_symbol_as_referenced(jsx_factory_sym);
            }
        }
        // if JsxFragment, additionally mark jsx pragma as referenced, since `getJsxNamespace` above would have resolved to only the fragment factory if they are distinct
        if is_jsx_opening_fragment(node) {
            let file = get_source_file_of_node(node).unwrap();
            let entity = self.get_jsx_factory_entity(Some(file.as_node()));
            if let Some(entity) = entity {
                let local_jsx_namespace = get_first_identifier(entity).text();
                let mut flags = SymbolFlags::Value;
                if !should_factory_ref_err {
                    flags &= !SymbolFlags::Enum;
                }
                self.resolve_name(
                    Some(jsx_factory_location),
                    local_jsx_namespace,
                    flags,
                    jsx_factory_ref_err,
                    true,  /*isUse*/
                    false, /*excludeGlobals*/
                );
            }
        }
    }

    // checker.go:29018
    fn mark_import_equals_alias_referenced(&mut self, location: P<Node>) {
        if has_syntactic_modifier(location, ModifierFlags::Export) {
            self.mark_export_as_referenced(location);
        }
    }

    // checker.go:29024
    fn mark_export_specifier_alias_referenced(&mut self, location: P<Node>) {
        let grandparent = location.parent().unwrap().parent().unwrap();
        if grandparent.module_specifier().is_none()
            && !location.is_type_only()
            && !grandparent.is_type_only()
        {
            let exported_name = location.property_name_or_name().unwrap();
            if exported_name.kind() == Kind::StringLiteral {
                return; // Skip for invalid syntax like this: export { "x" }
            }
            let symbol = self.resolve_name(
                Some(exported_name),
                exported_name.text(),
                SymbolFlags::Value
                    | SymbolFlags::Type
                    | SymbolFlags::Namespace
                    | SymbolFlags::Alias,
                None,  /*nameNotFoundMessage*/
                true,  /*isUse*/
                false, /*excludeGlobals*/
            );
            if symbol.is_some_and(|symbol| {
                symbol == self.undefined_symbol
                    || symbol == self.global_this_symbol
                    || !symbol.declarations().is_empty()
                        && get_declaration_container(symbol.declarations()[0])
                            .is_some_and(is_global_source_file)
            }) {
                // Do nothing, non-local symbol
            } else {
                let mut target = symbol;
                if let Some(t) = target
                    && t.flags().intersects(SymbolFlags::Alias)
                {
                    target = Some(self.resolve_alias(t));
                }
                if target.is_none()
                    || self.get_symbol_flags(target.unwrap()).intersects(SymbolFlags::Value)
                {
                    self.mark_export_as_referenced(location); // marks export as used
                    self.mark_identifier_alias_referenced(exported_name); // marks target of export as used
                }
            }
        }
    }

    // checker.go:29046
    pub(crate) fn check_external_emit_helpers(
        &mut self,
        location: P<Node>,
        helpers: ExternalEmitHelpers,
    ) {
        if !self.compiler_options.import_helpers.is_true() {
            return;
        }
        let source_file = get_source_file_of_node(location).unwrap();
        if !is_effective_external_module(source_file, &self.compiler_options)
            || location.flags().intersects(NodeFlags::Ambient)
        {
            return;
        }
        let helpers_module = self.resolve_helpers_module(source_file, location);
        if helpers_module == self.unknown_symbol {
            return;
        }
        let links = self.source_file_links.get(source_file);
        if !links.requested_external_emit_helpers.get().contains(helpers) {
            let unchecked_helpers = helpers & !links.requested_external_emit_helpers.get();
            let mut helper = ExternalEmitHelpers::FirstEmitHelper;
            while helper.bits() <= ExternalEmitHelpers::LastEmitHelper.bits() {
                if unchecked_helpers.intersects(helper) {
                    for name in self.get_helper_names(helper) {
                        let exports = self.get_exports_of_module(helpers_module);
                        let symbol = self
                            .get_symbol(Some(exports), &name, SymbolFlags::Value)
                            .map(|s| self.resolve_symbol(s));
                        match symbol {
                            None => {
                                self.error(Some(location), &diagnostics::THIS_SYNTAX_REQUIRES_AN_IMPORTED_HELPER_NAMED_1_WHICH_DOES_NOT_EXIST_IN_0_CONSIDER_UPGRADING_YOUR_VERSION_OF_0, &[&EXTERNAL_HELPERS_MODULE_NAME_TEXT, &name]);
                            }
                            Some(symbol) => {
                                if helper.intersects(ExternalEmitHelpers::ClassPrivateFieldGet) {
                                    if !self.has_signature_with_arity_greater_than(symbol, 3) {
                                        self.error(Some(location), &diagnostics::THIS_SYNTAX_REQUIRES_AN_IMPORTED_HELPER_NAMED_1_WITH_2_PARAMETERS_WHICH_IS_NOT_COMPATIBLE_WITH_THE_ONE_IN_0_CONSIDER_UPGRADING_YOUR_VERSION_OF_0, &[&EXTERNAL_HELPERS_MODULE_NAME_TEXT, &name, &4]);
                                    }
                                } else if helper
                                    .intersects(ExternalEmitHelpers::ClassPrivateFieldSet)
                                {
                                    if !self.has_signature_with_arity_greater_than(symbol, 4) {
                                        self.error(Some(location), &diagnostics::THIS_SYNTAX_REQUIRES_AN_IMPORTED_HELPER_NAMED_1_WITH_2_PARAMETERS_WHICH_IS_NOT_COMPATIBLE_WITH_THE_ONE_IN_0_CONSIDER_UPGRADING_YOUR_VERSION_OF_0, &[&EXTERNAL_HELPERS_MODULE_NAME_TEXT, &name, &5]);
                                    }
                                } else if helper
                                    .intersects(ExternalEmitHelpers::RewriteRelativeImportExtension)
                                    && !self
                                        .program
                                        .as_module_specifier_generation_host()
                                        .content_mapper_extension_rewrites()
                                        .is_empty()
                                    && !self.has_signature_with_arity_greater_than(symbol, 3)
                                {
                                    self.error(Some(location), &diagnostics::THIS_SYNTAX_REQUIRES_AN_IMPORTED_HELPER_NAMED_1_WITH_2_PARAMETERS_WHICH_IS_NOT_COMPATIBLE_WITH_THE_ONE_IN_0_CONSIDER_UPGRADING_YOUR_VERSION_OF_0, &[&EXTERNAL_HELPERS_MODULE_NAME_TEXT, &name, &4]);
                                }
                            }
                        }
                    }
                }
                helper = ExternalEmitHelpers::from_bits_retain(helper.bits() << 1);
            }
        }
        links
            .requested_external_emit_helpers
            .set(links.requested_external_emit_helpers.get() | helpers);
    }

    // checker.go:29084
    fn has_signature_with_arity_greater_than(&mut self, symbol: P<Symbol>, arity: usize) -> bool {
        self.get_signatures_of_symbol(Some(symbol))
            .into_iter()
            .any(|signature| self.get_parameter_count(signature) > arity)
    }

    // checker.go:29093
    fn get_helper_names(&self, helper: ExternalEmitHelpers) -> Vec<String> {
        let names: &[&str] = if helper == ExternalEmitHelpers::Rest {
            &["__rest"]
        } else if helper == ExternalEmitHelpers::Decorate {
            if self.legacy_decorators {
                &["__decorate"]
            } else {
                &["__esDecorate", "__runInitializers"]
            }
        } else if helper == ExternalEmitHelpers::Metadata {
            &["__metadata"]
        } else if helper == ExternalEmitHelpers::Param {
            &["__param"]
        } else if helper == ExternalEmitHelpers::Awaiter {
            &["__awaiter"]
        } else if helper == ExternalEmitHelpers::Await {
            &["__await"]
        } else if helper == ExternalEmitHelpers::AsyncGenerator {
            &["__asyncGenerator"]
        } else if helper == ExternalEmitHelpers::AsyncDelegator {
            &["__asyncDelegator"]
        } else if helper == ExternalEmitHelpers::AsyncValues {
            &["__asyncValues"]
        } else if helper == ExternalEmitHelpers::ExportStar {
            &["__exportStar"]
        } else if helper == ExternalEmitHelpers::ImportStar {
            &["__importStar"]
        } else if helper == ExternalEmitHelpers::ImportDefault {
            &["__importDefault"]
        } else if helper == ExternalEmitHelpers::MakeTemplateObject {
            &["__makeTemplateObject"]
        } else if helper == ExternalEmitHelpers::ClassPrivateFieldGet {
            &["__classPrivateFieldGet"]
        } else if helper == ExternalEmitHelpers::ClassPrivateFieldSet {
            &["__classPrivateFieldSet"]
        } else if helper == ExternalEmitHelpers::ClassPrivateFieldIn {
            &["__classPrivateFieldIn"]
        } else if helper == ExternalEmitHelpers::SetFunctionName {
            &["__setFunctionName"]
        } else if helper == ExternalEmitHelpers::PropKey {
            &["__propKey"]
        } else if helper == ExternalEmitHelpers::AddDisposableResourceAndDisposeResources {
            &["__addDisposableResource", "__disposeResources"]
        } else if helper == ExternalEmitHelpers::RewriteRelativeImportExtension {
            &["__rewriteRelativeImportExtension"]
        } else {
            panic!("Unrecognized helper")
        };
        names.iter().map(std::string::ToString::to_string).collect()
    }

    // checker.go:29143
    fn resolve_helpers_module(&mut self, file: P<SourceFile>, error_node: P<Node>) -> P<Symbol> {
        let links = self.source_file_links.get(file);
        if links.external_helpers_module.get().is_none() {
            let location = self.program.get_import_helpers_import_specifier(file.path());
            let helpers_module = match self.resolve_external_module(location, EXTERNAL_HELPERS_MODULE_NAME_TEXT, Some(&diagnostics::THIS_SYNTAX_REQUIRES_AN_IMPORTED_HELPER_BUT_MODULE_0_CANNOT_BE_FOUND), Some(error_node), false /*isForAugmentation*/, None /*importAttributesType*/) {
                Some(m) => m,
                None => self.unknown_symbol,
            };
            links.external_helpers_module.set(Some(helpers_module));
        }
        links.external_helpers_module.get().unwrap()
    }

    // checker.go:29156
    fn mark_decorator_alias_referenced(&mut self, node: P<Node>) {
        if self.compiler_options.emit_decorator_metadata.is_false_or_unknown() {
            return;
        }
        let Some(first_decorator) = first_or_nil(&node.decorators()) else {
            return;
        };

        self.check_external_emit_helpers(first_decorator, ExternalEmitHelpers::Metadata);

        // we only need to perform these checks if we are emitting serialized type metadata for the target of a decorator.
        match node.kind() {
            Kind::ClassDeclaration => {
                if let Some(ctor) = get_first_constructor_with_body(node) {
                    for &p in ctor.parameters() {
                        let type_node = Self::get_parameter_type_node_for_decorator_check(p);
                        self.mark_decorator_medata_data_type_node_as_referenced(type_node);
                    }
                }
            }
            Kind::GetAccessor | Kind::SetAccessor => {
                let other_kind = if node.kind() == Kind::SetAccessor {
                    Kind::GetAccessor
                } else {
                    Kind::SetAccessor
                };
                let symbol = self.get_symbol_of_declaration(node).unwrap();
                let other_accessor = get_declaration_of_kind(symbol, other_kind);
                let mut annotation = Self::get_annotated_accessor_type_node(Some(node));
                if annotation.is_none() && other_accessor.is_some() {
                    annotation = Self::get_annotated_accessor_type_node(other_accessor);
                }
                self.mark_decorator_medata_data_type_node_as_referenced(annotation);
            }
            Kind::MethodDeclaration => {
                for &p in node.parameters() {
                    let type_node = Self::get_parameter_type_node_for_decorator_check(p);
                    self.mark_decorator_medata_data_type_node_as_referenced(type_node);
                }
                self.mark_decorator_medata_data_type_node_as_referenced(node.type_node());
            }
            Kind::PropertyDeclaration => {
                self.mark_decorator_medata_data_type_node_as_referenced(node.type_node());
            }
            Kind::Parameter => {
                let type_node = Self::get_parameter_type_node_for_decorator_check(node);
                self.mark_decorator_medata_data_type_node_as_referenced(type_node);
                let containing_signature = node.parent().unwrap();
                for &p in containing_signature.parameters() {
                    let type_node = Self::get_parameter_type_node_for_decorator_check(p);
                    self.mark_decorator_medata_data_type_node_as_referenced(type_node);
                }
                self.mark_decorator_medata_data_type_node_as_referenced(
                    containing_signature.type_node(),
                );
            }
            _ => {}
        }
    }

    // checker.go:29204
    fn get_parameter_type_node_for_decorator_check(node: P<Node>) -> Option<P<Node>> {
        let type_node = node.type_node();
        if node.as_parameter_declaration().dot_dot_dot_token().is_some() {
            return get_rest_parameter_element_type(type_node);
        }
        type_node
    }

    // node Option<P<Node>>: Go passes getParameterTypeNodeForDecoratorCheck/Type() results, which can be nil
    // checker.go:29212
    fn mark_decorator_medata_data_type_node_as_referenced(&mut self, node: Option<P<Node>>) {
        let entity_name = self.get_entity_name_for_decorator_metadata(node);
        if let Some(entity_name) = entity_name
            && is_entity_name(entity_name)
        {
            self.mark_entity_name_or_entity_expression_as_reference(Some(entity_name), true);
        }
    }

    // node Option<P<Node>>: Go checks node == nil
    // checker.go:29219
    fn get_entity_name_for_decorator_metadata(&mut self, node: Option<P<Node>>) -> Option<P<Node>> {
        let node = node?;
        match node.kind() {
            Kind::IntersectionType => self.get_entity_name_for_decorator_metadata_from_type_list(
                node.as_intersection_type_node().types().nodes(),
            ),
            Kind::UnionType => self.get_entity_name_for_decorator_metadata_from_type_list(
                node.as_union_type_node().types().nodes(),
            ),
            Kind::ConditionalType => {
                let n = node.as_conditional_type_node();
                self.get_entity_name_for_decorator_metadata_from_type_list(&[
                    n.true_type,
                    n.false_type,
                ])
            }
            Kind::ParenthesizedType => self.get_entity_name_for_decorator_metadata(Some(
                node.as_parenthesized_type_node().type_,
            )),
            Kind::NamedTupleMember => self
                .get_entity_name_for_decorator_metadata(Some(node.as_named_tuple_member().type_)),
            Kind::TypeReference => Some(node.as_type_reference_node().type_name),
            _ => None,
        }
    }

    // checker.go:29240
    fn get_entity_name_for_decorator_metadata_from_type_list(
        &mut self,
        type_nodes: &[P<Node>],
    ) -> Option<P<Node>> {
        let mut common_entity_name: Option<P<Node>> = None;
        for &type_node in type_nodes {
            if type_node.kind() == Kind::NeverKeyword {
                continue; // Always elide `never` from the union/intersection if possible
            }
            if !self.strict_null_checks
                && (type_node.kind() == Kind::LiteralType
                    && type_node.as_literal_type_node().literal.kind() == Kind::NullKeyword
                    || type_node.kind() == Kind::UndefinedKeyword)
            {
                continue; // Elide null and undefined from unions for metadata, just like what we did prior to the implementation of strict null checks
            }
            let Some(individual_entity_name) =
                self.get_entity_name_for_decorator_metadata(Some(type_node))
            else {
                // Individual is something like string number
                // So it would be serialized to either that type or object
                // Safe to return here
                return None;
            };

            match common_entity_name {
                None => common_entity_name = Some(individual_entity_name),
                Some(common) => {
                    // Note this is in sync with the transformation that happens for type node.
                    // Keep this in sync with serializeUnionOrIntersectionType
                    // Verify if they refer to same entity and is identifier
                    // return undefined if they dont match because we would emit object
                    if !is_identifier(common)
                        || !is_identifier(individual_entity_name)
                        || common.as_identifier().text()
                            != individual_entity_name.as_identifier().text()
                    {
                        return None;
                    }
                }
            }
        }
        common_entity_name
    }

    // checker.go:29272
    fn mark_alias_referenced(&mut self, symbol: P<Symbol>, location: P<Node>) {
        if !self.can_collect_symbol_alias_accessibility_data {
            return;
        }
        if is_non_local_alias(symbol, SymbolFlags::Value /*excludes*/)
            && !is_in_type_query(location)
        {
            let target = self.resolve_alias(symbol);
            if self
                .get_symbol_flags_ex(
                    symbol, true,  /*excludeTypeOnlyMeanings*/
                    false, /*excludeLocalMeanings*/
                )
                .intersects(SymbolFlags::Value | SymbolFlags::ExportValue)
            {
                // An alias resolving to a const enum cannot be elided if (1) 'isolatedModules' is enabled
                // (because the const enum value will not be inlined), or if (2) the alias is an export
                // of a const enum declaration that will be preserved.
                if self.compiler_options.get_isolated_modules()
                    || self.compiler_options.should_preserve_const_enums()
                        && is_export_or_export_expression(location)
                    || {
                        let export_symbol = self
                            .get_export_symbol_of_value_symbol_if_exported(Some(target))
                            .unwrap();
                        !is_const_enum_or_const_enum_only_module(export_symbol)
                    }
                {
                    self.mark_alias_symbol_as_referenced(symbol);
                }
            }
        }
    }

    // When an alias symbol is referenced, we need to mark the entity it references as referenced and in turn repeat that until
    // we reach a non-alias or an exported entity (which is always considered referenced). We do this by checking the target of
    // the alias as an expression (which recursively takes us back here if the target references another alias).
    // checker.go:29294
    fn mark_alias_symbol_as_referenced(&mut self, symbol: P<Symbol>) {
        let links = self.alias_symbol_links.get(symbol);
        if !links.referenced.get() {
            links.referenced.set(true);
            let Some(node) = Self::get_declaration_of_alias_symbol(symbol) else {
                panic!("Unexpected nil in markAliasSymbolAsReferenced");
            };
            // We defer checking of the reference of an `import =` until the import itself is referenced,
            // This way a chain of imports can be elided if ultimately the final input is only used in a type
            // position.
            if is_import_equals_declaration(node)
                && node.as_import_equals_declaration().module_reference.kind()
                    != Kind::ExternalModuleReference
            {
                let resolved = self.resolve_symbol(symbol);
                if self.get_symbol_flags(resolved).intersects(SymbolFlags::Value) {
                    // import foo = <symbol>
                    let left =
                        get_first_identifier(node.as_import_equals_declaration().module_reference);
                    self.mark_identifier_alias_referenced(left);
                }
            }
        }
    }

    // checker.go:29315
    fn mark_export_as_referenced(&mut self, node: P<Node>) {
        let symbol = self.get_symbol_of_declaration(node).unwrap();
        let target = self.resolve_alias(symbol);
        let mark_alias = target == self.unknown_symbol
            || (self
                .get_symbol_flags_ex(
                    symbol, true,  /*excludeTypeOnlyMeanings*/
                    false, /*excludeLocalMeanings*/
                )
                .intersects(SymbolFlags::Value)
                && !is_const_enum_or_const_enum_only_module(target));
        if mark_alias {
            self.mark_alias_symbol_as_referenced(symbol);
        }
    }

    // checker.go:29327
    fn mark_entity_name_or_entity_expression_as_reference(
        &mut self,
        type_name: Option<P<Node>>,
        for_decorator_metadata: bool,
    ) {
        let Some(type_name) = type_name else {
            return;
        };

        let root_name = get_first_identifier(type_name);
        let meaning = if_else(
            type_name.kind() == Kind::Identifier,
            SymbolFlags::Type,
            SymbolFlags::Namespace,
        ) | SymbolFlags::Alias;
        let root_symbol = self.resolve_name(
            Some(root_name),
            root_name.text(),
            meaning,
            None,  /*nameNotFoundMessage*/
            true,  /*isUse*/
            false, /*excludeGlobals*/
        );

        if let Some(root_symbol) = root_symbol.filter(|s| s.flags().intersects(SymbolFlags::Alias))
        {
            if self.can_collect_symbol_alias_accessibility_data
                && self.symbol_is_value(root_symbol)
                && {
                    let resolved = self.resolve_alias(root_symbol);
                    !is_const_enum_or_const_enum_only_module(resolved)
                }
                && self.get_type_only_alias_declaration(root_symbol).is_none()
            {
                self.mark_alias_symbol_as_referenced(root_symbol);
            } else if for_decorator_metadata
                && self.compiler_options.get_isolated_modules()
                && self.compiler_options.get_emit_module_kind() >= ModuleKind::ES2015
                && !self.symbol_is_value(root_symbol)
                && !root_symbol
                    .declarations()
                    .iter()
                    .any(|&d| is_type_only_import_or_export_declaration(d))
            {
                let diag = self.error(Some(type_name), &diagnostics::A_TYPE_REFERENCED_IN_A_DECORATED_SIGNATURE_MUST_BE_IMPORTED_WITH_IMPORT_TYPE_OR_A_NAMESPACE_IMPORT_WHEN_ISOLATED_MODULES_AND_EMIT_DECORATOR_METADATA_ARE_ENABLED, &[]);
                let alias_declaration = root_symbol
                    .declarations()
                    .iter()
                    .copied()
                    .find(|&decl| is_alias_symbol_declaration(decl));
                if let Some(alias_declaration) = alias_declaration {
                    diag.set_related_info(&[create_diagnostic_for_node(
                        Some(alias_declaration),
                        &diagnostics::X_0_WAS_IMPORTED_HERE,
                        &[&root_name.text()],
                    )]);
                }
            }
        }
    }
}

impl Checker {
    // checker.go:29389
    pub fn get_promised_type_of_promise(&mut self, t: P<Type>) -> Option<P<Type>> {
        self.get_promised_type_of_promise_ex(t, None, None)
    }

    // Gets the "promised type" of a promise.
    // @param type The type of the promise.
    // @remarks The "promised type" of a type is the type of the "value" parameter of the "onfulfilled" callback.
    // checker.go:29396
    pub(crate) fn get_promised_type_of_promise_ex(
        &mut self,
        t: P<Type>,
        error_node: Option<P<Node>>,
        this_type_for_error_out: Option<&mut Option<P<Type>>>,
    ) -> Option<P<Type>> {
        //  { // type
        //      then( // thenFunction
        //          onfulfilled: ( // onfulfilledParameterType
        //              value: T // valueParameterType
        //          ) => any
        //      ): any;
        //  }
        if is_type_any(Some(t)) {
            return None;
        }
        let key = CachedTypeKey { kind: CachedTypeKind::PromisedTypeOfPromise, type_id: t.id };
        if let Some(&cached) = self.cached_types.get(&key) {
            return Some(cached);
        }
        let global_promise_type = self.get_global_promise_type();
        if Self::is_reference_to_type(Some(t), global_promise_type) {
            let result = self.get_type_arguments(t)[0];
            self.cached_types.insert(key, result);
            return Some(result);
        }
        // primitives with a `{ then() }` won't be unwrapped/adopted.
        let base = self.get_base_constraint_or_type(t);
        if self.all_types_assignable_to_kind(base, TypeFlags::Primitive | TypeFlags::Never) {
            return None;
        }
        let then_function = self.get_type_of_property_of_type(t, "then");
        // TODO: GH#18217
        if is_type_any(then_function) {
            return None;
        }
        let mut then_signatures: &[P<Signature>] = &[];
        if let Some(then_function) = then_function {
            then_signatures = self.get_signatures_of_type(then_function, SignatureKind::Call);
        }
        if then_signatures.is_empty() {
            if error_node.is_some() {
                self.error(error_node, &diagnostics::A_PROMISE_MUST_HAVE_A_THEN_METHOD, &[]);
            }
            return None;
        }
        let mut this_type_for_error: Option<P<Type>> = None;
        let mut candidates: Vec<P<Signature>> = Vec::new();
        for &then_signature in then_signatures {
            let this_type = self.get_this_type_of_signature(then_signature);
            if this_type.is_some_and(|this_type| {
                this_type != self.void_type && {
                    let subtype_relation = self.subtype_relation;
                    !self.is_type_related_to(t, this_type, subtype_relation)
                }
            }) {
                this_type_for_error = this_type;
            } else {
                candidates.push(then_signature);
            }
        }
        if candidates.is_empty() {
            assert!(this_type_for_error.is_some());
            let this_type_for_error = this_type_for_error.unwrap();
            if let Some(out) = this_type_for_error_out {
                *out = Some(this_type_for_error);
            }
            if error_node.is_some() {
                let t_str = self.type_to_string(t, None);
                let this_str = self.type_to_string(this_type_for_error, None);
                self.error(error_node, &diagnostics::THE_THIS_CONTEXT_OF_TYPE_0_IS_NOT_ASSIGNABLE_TO_METHOD_S_THIS_OF_TYPE_1, &[&t_str, &this_str]);
            }
            return None;
        }
        let first_parameter_types: Vec<P<Type>> =
            candidates.iter().map(|&s| self.get_type_of_first_parameter_of_signature(s)).collect();
        let union = self.get_union_type(&first_parameter_types);
        let onfulfilled_parameter_type =
            self.get_type_with_facts(union, TypeFacts::NEUndefinedOrNull);
        if is_type_any(Some(onfulfilled_parameter_type)) {
            return None;
        }
        let onfulfilled_parameter_signatures =
            self.get_signatures_of_type(onfulfilled_parameter_type, SignatureKind::Call);
        if onfulfilled_parameter_signatures.is_empty() {
            if error_node.is_some() {
                self.error(error_node, &diagnostics::THE_FIRST_PARAMETER_OF_THE_THEN_METHOD_OF_A_PROMISE_MUST_BE_A_CALLBACK, &[]);
            }
            return None;
        }
        let types: Vec<P<Type>> = onfulfilled_parameter_signatures
            .iter()
            .map(|&s| self.get_type_of_first_parameter_of_signature(s))
            .collect();
        let result = self.get_union_type_ex(&types, UnionReduction::Subtype, AliasArg::None, None);
        self.cached_types.insert(key, result);
        Some(result)
    }

    // checker.go:29471
    pub(crate) fn get_type_of_first_parameter_of_signature(
        &mut self,
        signature: P<Signature>,
    ) -> P<Type> {
        let never_type = self.never_type;
        self.get_type_of_first_parameter_of_signature_with_fallback(signature, never_type)
    }

    // checker.go:29475
    pub(crate) fn get_type_of_first_parameter_of_signature_with_fallback(
        &mut self,
        signature: P<Signature>,
        fallback_type: P<Type>,
    ) -> P<Type> {
        if !signature.parameters.get().is_empty() {
            return self.get_type_at_position(signature, 0);
        }
        fallback_type
    }
}

// checker.go:29482
pub(crate) fn get_mapped_type_modifiers(t: P<Type>) -> MappedTypeModifiers {
    let declaration = t.as_mapped_type().declaration.get().unwrap().as_mapped_type_node();
    let mut modifiers = MappedTypeModifiers::empty();
    if let Some(readonly_token) = declaration.readonly_token {
        modifiers |= if_else(
            readonly_token.kind() == Kind::MinusToken,
            MappedTypeModifiers::ExcludeReadonly,
            MappedTypeModifiers::IncludeReadonly,
        );
    }
    if let Some(question_token) = declaration.question_token {
        modifiers |= if_else(
            question_token.kind() == Kind::MinusToken,
            MappedTypeModifiers::ExcludeOptional,
            MappedTypeModifiers::IncludeOptional,
        );
    }
    modifiers
}

// checker.go:29496
fn get_mapped_type_optionality(t: P<Type>) -> std::cmp::Ordering {
    let modifiers = get_mapped_type_modifiers(t);
    if modifiers.intersects(MappedTypeModifiers::ExcludeOptional) {
        return std::cmp::Ordering::Less;
    } else if modifiers.intersects(MappedTypeModifiers::IncludeOptional) {
        return std::cmp::Ordering::Greater;
    }
    std::cmp::Ordering::Equal
}

impl Checker {
    // Return -1, 0, or 1, for stripped, unchanged, or added optionality respectively. When a homomorphic mapped type doesn't
    // modify optionality, recursively consult the optionality of the type being mapped over to see if it strips or adds optionality.
    // For intersections, return -1 or 1 when all constituents strip or add optionality, otherwise return 0.
    // checker.go:29510
    pub(crate) fn get_combined_mapped_type_optionality(
        &mut self,
        t: P<Type>,
    ) -> std::cmp::Ordering {
        if t.object_flags().intersects(ObjectFlags::Mapped) {
            let optionality = get_mapped_type_optionality(t);
            if optionality != std::cmp::Ordering::Equal {
                return optionality;
            }
            let modifiers_type = self.get_modifiers_type_from_mapped_type(t);
            return self.get_combined_mapped_type_optionality(modifiers_type);
        }
        if t.flags().intersects(TypeFlags::Intersection) {
            let optionality = self.get_combined_mapped_type_optionality(t.types()[0]);
            if let Some(result) = t.types()[1..].iter().find_map(|&t| {
                if self.get_combined_mapped_type_optionality(t) == optionality {
                    None
                } else {
                    Some(std::cmp::Ordering::Equal)
                }
            }) {
                return result;
            }
            return optionality;
        }
        std::cmp::Ordering::Equal
    }
}

// checker.go:29530
pub(crate) fn is_partial_mapped_type(t: P<Type>) -> bool {
    t.object_flags().intersects(ObjectFlags::Mapped)
        && get_mapped_type_modifiers(t).intersects(MappedTypeModifiers::IncludeOptional)
}

impl Checker {
    // checker.go:29534
    pub(crate) fn get_optional_expression_type(
        &mut self,
        expr_type: P<Type>,
        expression: P<Node>,
    ) -> P<Type> {
        if is_expression_of_optional_chain_root(expression) {
            self.get_non_nullable_type(expr_type)
        } else if is_optional_chain(expression) {
            self.remove_optional_type_marker(expr_type)
        } else {
            expr_type
        }
    }

    // checker.go:29545
    pub(crate) fn remove_optional_type_marker(&mut self, t: P<Type>) -> P<Type> {
        if self.strict_null_checks {
            let optional_type = self.optional_type;
            return self.remove_type(t, optional_type);
        }
        t
    }

    // checker.go:29552
    pub(crate) fn propagate_optional_type_marker(
        &mut self,
        t: P<Type>,
        node: P<Node>,
        was_optional: bool,
    ) -> P<Type> {
        if was_optional {
            if is_outermost_optional_chain(node) {
                return self.get_optional_type(t, false);
            }
            return self.add_optional_type_marker(t);
        }
        t
    }

    // checker.go:29562
    pub(crate) fn remove_missing_type(&mut self, t: P<Type>, is_optional: bool) -> P<Type> {
        if self.exact_optional_property_types && is_optional {
            let missing_type = self.missing_type;
            return self.remove_type(t, missing_type);
        }
        t
    }

    // checker.go:29569
    pub(crate) fn remove_missing_or_undefined_type(&mut self, t: P<Type>) -> P<Type> {
        if self.exact_optional_property_types {
            let missing_type = self.missing_type;
            return self.remove_type(t, missing_type);
        }
        self.get_type_with_facts(t, TypeFacts::NEUndefined)
    }

    // checker.go:29576
    pub(crate) fn remove_definitely_falsy_types(&mut self, t: P<Type>) -> P<Type> {
        self.filter_type(t, |c, t| c.has_type_facts(t, TypeFacts::Truthy))
    }

    // checker.go:29580
    pub(crate) fn extract_definitely_falsy_types(&mut self, t: P<Type>) -> P<Type> {
        self.map_type(t, |c, t| Some(c.get_definitely_falsy_part_of_type(t))).unwrap()
    }

    // checker.go:29584
    fn get_definitely_falsy_part_of_type(&self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::String) {
            self.empty_string_type
        } else if t.flags().intersects(TypeFlags::Number) {
            self.zero_type
        } else if t.flags().intersects(TypeFlags::BigInt) {
            self.zero_big_int_type
        } else if t == self.regular_false_type
            || t == self.false_type
            || t.flags().intersects(
                TypeFlags::Void | TypeFlags::Undefined | TypeFlags::Null | TypeFlags::AnyOrUnknown,
            )
            || t.flags().intersects(TypeFlags::StringLiteral)
                && get_string_literal_value(t).is_empty()
            || t.flags().intersects(TypeFlags::NumberLiteral)
                && get_number_literal_value(t).0 == 0.0
            || t.flags().intersects(TypeFlags::BigIntLiteral) && is_zero_big_int(t)
        {
            t
        } else {
            self.never_type
        }
    }

    // checker.go:29602
    pub(crate) fn get_constraint_declaration(t: P<Type>) -> Option<P<Node>> {
        if let Some(symbol) = t.symbol()
            && let Some(result) = symbol.declarations().iter().find_map(|&d| {
                if is_type_parameter_declaration(d)
                    && let Some(constraint) = d.as_type_parameter_declaration().constraint
                {
                    return Some(Some(constraint));
                }

                None
            })
        {
            return result;
        }
        None
    }

    // checker.go:29623
    pub(crate) fn get_template_literal_type(
        &mut self,
        texts: &[&str],
        types: &[P<Type>],
    ) -> P<Type> {
        let union_index =
            find_index(types, |t| t.flags().intersects(TypeFlags::Never | TypeFlags::Union));
        if let Some(union_index) = union_index {
            if !self.check_cross_product_union(types, Self::too_complex_key(&(texts, types))) {
                return self.error_type;
            }
            // One copy of `types` for every constituent (Go copies it per constituent with `core.ReplaceElement`).
            let mut replaced = types.to_vec();
            return self
                .map_type(types[union_index], |c, t| {
                    replaced[union_index] = t;
                    Some(c.get_template_literal_type(texts, &replaced))
                })
                .unwrap();
        }
        if types.contains(&self.wildcard_type) {
            return self.wildcard_type;
        }
        let mut sb = self.free_strings.pop().unwrap_or_default();
        let result = self.get_template_literal_type_worker(texts, types, &mut sb);
        sb.clear();
        self.free_strings.push(sb);
        result
    }

    /// The rest of `get_template_literal_type`, with the text built in `sb` (an empty buffer).
    fn get_template_literal_type_worker(
        &mut self,
        texts: &[&str],
        types: &[P<Type>],
        sb: &mut String,
    ) -> P<Type> {
        let mut state = TemplateSpansState {
            new_types: Vec::new(),
            new_texts: Vec::new(),
            sb: std::mem::take(sb),
            text_length: 0,
            too_large: false,
        };
        state.sb.push_str(texts[0]);
        let spans_ok = add_template_spans(self, &mut state, texts, types);
        *sb = std::mem::take(&mut state.sb);
        let sb: &str = sb;
        if !spans_ok {
            if state.too_large {
                let current_node = self.current_node;
                self.error(
                    current_node,
                    &diagnostics::TYPE_INSTANTIATION_IS_EXCESSIVELY_DEEP_AND_POSSIBLY_INFINITE,
                    &[],
                );
                return self.error_type;
            }
            return self.string_type;
        }
        if state.new_types.is_empty() {
            return self.get_string_literal_type(&stringutil::combine_surrogate_pairs(sb));
        }
        state.new_texts.push(stringutil::combine_surrogate_pairs(sb).into_owned());
        let new_texts = state.new_texts;
        let new_types = state.new_types;
        if new_texts.iter().all(std::string::String::is_empty) {
            if new_types.iter().all(|t| t.flags().intersects(TypeFlags::String)) {
                return self.string_type;
            }
            // Normalize `${Mapping<xxx>}` into Mapping<xxx>
            if new_types.len() == 1 && self.is_pattern_literal_type(new_types[0]) {
                return new_types[0];
            }
        }
        let new_text_refs: Vec<&str> = new_texts.iter().map(std::string::String::as_str).collect();
        let key = get_template_type_key(&new_text_refs, &new_types);

        if let Some(t) = self.template_literal_types.get(&key) {
            t
        } else {
            let t = self.new_template_literal_type(&new_text_refs, &new_types);
            self.template_literal_types.insert(key, t);
            t
        }
    }

    // checker.go:29702
    /// Go `getTemplateStringForType`, appended to `sb` (without a temporary `String` for string literals).
    fn push_template_string_for_type(sb: &mut String, t: P<Type>) {
        if t.flags().intersects(
            TypeFlags::StringLiteral
                | TypeFlags::NumberLiteral
                | TypeFlags::BooleanLiteral
                | TypeFlags::BigIntLiteral,
        ) {
            match t.as_literal_type().value.get().unwrap() {
                LiteralValue::String(s) => sb.push_str(s),
                v => sb.push_str(&evaluator::any_to_string(v)),
            }
        } else if t.flags().intersects(TypeFlags::Nullable) {
            sb.push_str(t.as_intrinsic_type().intrinsic_name.get());
        }
    }

    // checker.go:29712
    pub(crate) fn get_string_mapping_type(&mut self, symbol: P<Symbol>, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Union | TypeFlags::Never) {
            self.map_type(t, |c, t| Some(c.get_string_mapping_type(symbol, t))).unwrap()
        } else if t.flags().intersects(TypeFlags::StringLiteral) {
            self.get_string_literal_type(&apply_string_mapping(symbol, get_string_literal_value(t)))
        } else if t.flags().intersects(TypeFlags::TemplateLiteral) {
            let (texts, types) = self.apply_template_string_mapping(
                symbol,
                t.as_template_literal_type().texts.get(),
                t.as_template_literal_type().types.get(),
            );
            let text_refs: Vec<&str> = texts.iter().map(std::string::String::as_str).collect();
            self.get_template_literal_type(&text_refs, &types)
        } else if t.flags().intersects(TypeFlags::StringMapping) && Some(symbol) == t.symbol() {
            t
        } else if t
            .flags()
            .intersects(TypeFlags::Any | TypeFlags::String | TypeFlags::StringMapping)
            || self.is_generic_index_type(t)
        {
            self.get_string_mapping_type_for_generic_type(symbol, t)
        } else if self.is_pattern_literal_placeholder_type(t) {
            let template = self.get_template_literal_type(&["", ""], &[t]);
            self.get_string_mapping_type_for_generic_type(symbol, template)
        } else {
            t
        }
    }
}

// Mutable state shared by the recursive `addSpans` closure of getTemplateLiteralType.
struct TemplateSpansState {
    new_types: Vec<P<Type>>,
    new_texts: Vec<String>,
    sb: String,
    text_length: usize, // combined length of the segments already moved into newTexts
    too_large: bool,
}

// checker.go:29645 (addSpans closure of getTemplateLiteralType)
fn add_template_spans(
    c: &mut Checker,
    state: &mut TemplateSpansState,
    texts: &[&str],
    types: &[P<Type>],
) -> bool {
    for (i, &t) in types.iter().enumerate() {
        if t.flags().intersects(TypeFlags::Literal | TypeFlags::Null | TypeFlags::Undefined) {
            checker::Checker::push_template_string_for_type(&mut state.sb, t);
            state.sb.push_str(texts[i + 1]);
        } else if t.flags().intersects(TypeFlags::TemplateLiteral) {
            state.sb.push_str(t.as_template_literal_type().texts.get()[0]);
            if !add_template_spans(
                c,
                state,
                t.as_template_literal_type().texts.get(),
                t.as_template_literal_type().types.get(),
            ) {
                return false;
            }
            state.sb.push_str(texts[i + 1]);
        } else if c.is_generic_index_type(t) || c.is_pattern_literal_placeholder_type(t) {
            state.new_types.push(t);
            state.new_texts.push(stringutil::combine_surrogate_pairs(&state.sb).into_owned());
            state.text_length += state.sb.len();
            state.sb.clear();
            state.sb.push_str(texts[i + 1]);
        } else {
            return false;
        }
        if state.text_length + state.sb.len() > MAX_TEMPLATE_LITERAL_TYPE_LENGTH
            || state.new_types.len() > MAX_TEMPLATE_LITERAL_TYPE_SPANS
        {
            state.too_large = true;
            return false;
        }
    }
    true
}

// checker.go:29731
pub(crate) fn apply_string_mapping(symbol: P<Symbol>, str: &str) -> String {
    match INTRINSIC_TYPE_KINDS.get(symbol.name()).copied().unwrap_or_default() {
        IntrinsicTypeKind::Uppercase => stringutil::to_upper_js(str),
        IntrinsicTypeKind::Lowercase => stringutil::to_lower_js(str),
        IntrinsicTypeKind::Capitalize => {
            let (_, size) = stringutil::decode_js_string_rune(str);
            stringutil::to_upper_js(&str[..size]) + &str[size..]
        }
        IntrinsicTypeKind::Uncapitalize => {
            let (_, size) = stringutil::decode_js_string_rune(str);
            stringutil::to_lower_js(&str[..size]) + &str[size..]
        }
        _ => str.to_string(),
    }
}

impl Checker {
    // checker.go:29747
    fn apply_template_string_mapping(
        &mut self,
        symbol: P<Symbol>,
        texts: &[&str],
        types: &[P<Type>],
    ) -> (Vec<String>, Vec<P<Type>>) {
        match INTRINSIC_TYPE_KINDS.get(symbol.name()).copied().unwrap_or_default() {
            IntrinsicTypeKind::Uppercase | IntrinsicTypeKind::Lowercase => {
                let new_texts: Vec<String> =
                    texts.iter().map(|t| apply_string_mapping(symbol, t)).collect();
                let new_types: Vec<P<Type>> =
                    types.iter().map(|&t| self.get_string_mapping_type(symbol, t)).collect();
                (new_texts, new_types)
            }
            IntrinsicTypeKind::Capitalize | IntrinsicTypeKind::Uncapitalize => {
                if !texts[0].is_empty() {
                    let mut new_texts: Vec<String> =
                        texts.iter().map(std::string::ToString::to_string).collect();
                    new_texts[0] = apply_string_mapping(symbol, &new_texts[0]);
                    return (new_texts, types.to_vec());
                }
                let mut new_types = types.to_vec();
                new_types[0] = self.get_string_mapping_type(symbol, new_types[0]);
                (texts.iter().map(std::string::ToString::to_string).collect(), new_types)
            }
            _ => (texts.iter().map(std::string::ToString::to_string).collect(), types.to_vec()),
        }
    }

    // checker.go:29765
    fn get_string_mapping_type_for_generic_type(
        &mut self,
        symbol: P<Symbol>,
        t: P<Type>,
    ) -> P<Type> {
        let key = StringMappingKey { s: symbol, t };
        if let Some(&result) = self.string_mapping_types.get(&key) {
            result
        } else {
            let result = self.new_string_mapping_type(symbol, t);
            self.string_mapping_types.insert(key, result);
            result
        }
    }

    // Given an indexed access on a mapped type of the form { [P in K]: E }[X], return an instantiation of E where P is
    // replaced with X. Since this simplification doesn't account for mapped type modifiers, add 'undefined' to the
    // resulting type if the mapped type includes a '?' modifier or if the modifiers type indicates that some properties
    // are optional. If the modifiers type is generic, conservatively estimate optionality by recursively looking for
    // mapped types that include '?' modifiers.
    // checker.go:29780
    pub(crate) fn substitute_indexed_mapped_type(
        &mut self,
        object_type: P<Type>,
        index: P<Type>,
    ) -> P<Type> {
        let type_parameter = self.get_type_parameter_from_mapped_type(object_type);
        let mapper = new_simple_type_mapper(type_parameter, index);
        let template_mapper =
            Self::combine_type_mappers(object_type.as_mapped_type().mapper.get(), mapper);
        let template_type = self.get_template_type_from_mapped_type(
            object_type.as_mapped_type().target.get().unwrap_or(object_type),
        );
        let instantiated_template_type =
            self.instantiate_type(template_type, Some(template_mapper));
        // SAFETY: both made here for this one instantiation (the composite's children escape with it).
        unsafe {
            if template_mapper != mapper {
                recycle_mapper(template_mapper);
            }
            recycle_mapper(mapper);
        }
        let mut is_optional = get_mapped_type_optionality(object_type).is_gt();
        if !is_optional {
            if self.is_generic_type(object_type) {
                let modifiers_type = self.get_modifiers_type_from_mapped_type(object_type);
                is_optional = self.get_combined_mapped_type_optionality(modifiers_type).is_gt();
            } else {
                is_optional = self.could_access_optional_property(object_type, index);
            }
        }
        self.add_optionality_ex(instantiated_template_type, true /*isProperty*/, is_optional)
    }

    // Return true if an indexed access with the given object and index types could access an optional property.
    // checker.go:29796
    fn could_access_optional_property(
        &mut self,
        object_type: P<Type>,
        index_type: P<Type>,
    ) -> bool {
        let Some(index_constraint) = self.get_base_constraint_of_type(index_type) else {
            return false;
        };
        self.get_properties_of_type(object_type).iter().copied().any(|p| {
            p.flags().intersects(SymbolFlags::Optional) && {
                let literal = self.get_literal_type_from_property(
                    p,
                    TypeFlags::StringOrNumberLiteralOrUnique,
                    false,
                );
                self.is_type_assignable_to(literal, index_constraint)
            }
        })
    }

    // checker.go:29803
    pub(crate) fn get_type_of_property_or_index_signature_of_type(
        &mut self,
        t: P<Type>,
        name: &str,
    ) -> Option<P<Type>> {
        let prop_type = self.get_type_of_property_of_type(t, name);
        if prop_type.is_some() {
            return prop_type;
        }
        let index_info = self.get_applicable_index_info_for_name(t, name);
        if let Some(index_info) = index_info {
            return Some(self.add_optionality_ex(
                index_info.value_type(),
                true, /*isProperty*/
                true, /*isOptional*/
            ));
        }
        None
    }

    /**
     * Whoa! Do you really want to use this function?
     *
     * Unless you're trying to get the *non-apparent* type for a
     * value-literal type or you're authoring relevant portions of this algorithm,
     * you probably meant to use 'getApparentTypeOfContextualType'.
     * Otherwise this may not be very useful.
     *
     * In cases where you *are* working on this function, you should understand
     * when it is appropriate to use 'getContextualType' and 'getApparentTypeOfContextualType'.
     *
     *   - Use 'getContextualType' when you are simply going to propagate the result to the expression.
     *   - Use 'getApparentTypeOfContextualType' when you're going to need the members of the type.
     *
     * @param node the expression whose contextual type will be returned.
     * @returns the contextual type of an expression.
     */
    // checker.go:29832
    pub(crate) fn get_contextual_type(
        &mut self,
        node: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        if node.flags().intersects(NodeFlags::InWithStatement) {
            // We cannot answer semantic questions within a with block, do not proceed any further
            return None;
        }
        // Cached contextual types are obtained with no ContextFlags, so we can only consult them for
        // requests with no ContextFlags.
        if let Some(index) = self
            .find_contextual_node(node, context_flags == ContextFlags::None /*includeCaches*/)
        {
            return self.contextual_infos[index].t;
        }
        let parent = node.parent().unwrap();
        match parent.kind() {
            Kind::VariableDeclaration
            | Kind::Parameter
            | Kind::PropertyDeclaration
            | Kind::PropertySignature
            | Kind::BindingElement => {
                self.get_contextual_type_for_initializer_expression(node, context_flags)
            }
            Kind::ArrowFunction | Kind::ReturnStatement => {
                self.get_contextual_type_for_return_expression(node, context_flags)
            }
            Kind::YieldExpression => {
                self.get_contextual_type_for_yield_operand(parent, context_flags)
            }
            Kind::AwaitExpression => {
                self.get_contextual_type_for_await_operand(parent, context_flags)
            }
            Kind::CallExpression | Kind::NewExpression => {
                self.get_contextual_type_for_argument(parent, node)
            }
            Kind::Decorator => self.get_contextual_type_for_decorator(parent),
            Kind::TypeAssertionExpression | Kind::AsExpression => {
                if is_const_assertion(parent) {
                    return self.get_contextual_type(parent, context_flags);
                }
                Some(self.get_type_from_type_node(parent.type_node().unwrap()))
            }
            Kind::BinaryExpression => {
                self.get_contextual_type_for_binary_operand(node, context_flags)
            }
            Kind::PropertyAssignment | Kind::ShorthandPropertyAssignment => {
                self.get_contextual_type_for_object_literal_element(parent, context_flags)
            }
            Kind::SpreadAssignment => {
                self.get_contextual_type(parent.parent().unwrap(), context_flags)
            }
            Kind::ArrayLiteralExpression => {
                let t = self.get_apparent_type_of_contextual_type(parent, context_flags);
                let element_index = index_of_node(parent.elements(), node)?;
                let (first_spread_index, last_spread_index) = self.get_spread_indices(parent);
                self.get_contextual_type_for_element_expression(
                    t,
                    element_index,
                    Some(parent.elements().len()),
                    first_spread_index,
                    last_spread_index,
                )
            }
            Kind::ConditionalExpression => {
                self.get_contextual_type_for_conditional_operand(node, context_flags)
            }
            Kind::TemplateSpan => {
                self.get_contextual_type_for_substitution_expression(parent.parent().unwrap(), node)
            }
            Kind::ParenthesizedExpression | Kind::NonNullExpression => {
                self.get_contextual_type(parent, context_flags)
            }
            Kind::SatisfiesExpression => {
                Some(self.get_type_from_type_node(parent.type_node().unwrap()))
            }
            Kind::ExportAssignment => self.try_get_type_from_type_node(parent),
            Kind::JsxExpression => {
                self.get_contextual_type_for_jsx_expression(parent, context_flags)
            }
            Kind::JsxAttribute | Kind::JsxSpreadAttribute => {
                self.get_contextual_type_for_jsx_attribute(parent, context_flags)
            }
            Kind::JsxOpeningElement | Kind::JsxSelfClosingElement => {
                self.get_contextual_jsx_element_attributes_type(parent, context_flags)
            }
            Kind::ImportAttribute => self.get_contextual_import_attribute_type(parent),
            _ => None,
        }
    }

    // In a variable, parameter or property declaration with a type annotation,
    // the contextual type of an initializer expression is the type of the variable, parameter or property.
    //
    // Otherwise, in a parameter declaration of a contextually typed function expression,
    // the contextual type of an initializer expression is the contextual type of the parameter.
    //
    // Otherwise, in a variable or parameter declaration with a binding pattern name,
    // the contextual type of an initializer expression is the type implied by the binding pattern.
    //
    // Otherwise, in a binding pattern inside a variable or parameter declaration,
    // the contextual type of an initializer expression is the type annotation of the containing declaration, if present.
    // checker.go:29912
    fn get_contextual_type_for_initializer_expression(
        &mut self,
        node: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        let declaration = node.parent().unwrap();
        let initializer = declaration.initializer();
        if Some(node) == initializer {
            let result =
                self.get_contextual_type_for_variable_like_declaration(declaration, context_flags);
            if result.is_some() {
                return result;
            }
            if !context_flags.intersects(ContextFlags::SkipBindingPatterns)
                && is_binding_pattern(declaration.name().unwrap())
                && !declaration.name().unwrap().elements().is_empty()
            {
                return Some(self.get_type_from_binding_pattern(
                    declaration.name().unwrap(),
                    true,  /*includePatternInType*/
                    false, /*reportErrors*/
                ));
            }
        }
        None
    }

    // checker.go:29927
    fn get_contextual_type_for_variable_like_declaration(
        &mut self,
        declaration: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        if let Some(type_node) = declaration.type_node() {
            return Some(self.get_type_from_type_node(type_node));
        }
        match declaration.kind() {
            Kind::Parameter => return self.get_contextually_typed_parameter_type(declaration),
            Kind::BindingElement => {
                return self.get_contextual_type_for_binding_element(declaration, context_flags);
            }
            Kind::PropertyDeclaration if is_static(declaration) => {
                return self.get_contextual_type_for_static_property_declaration(
                    declaration,
                    context_flags,
                );
            }
            _ => {}
        }
        // By default, do nothing and return nil - only the above cases have context implied by a parent
        None
    }

    // Return contextual type of parameter or undefined if no contextual type is available
    // checker.go:29947
    pub(crate) fn get_contextually_typed_parameter_type(
        &mut self,
        parameter: P<Node>,
    ) -> Option<P<Type>> {
        let fn_ = parameter.parent().unwrap();
        if !self.is_context_sensitive_function_or_object_literal_method(fn_) {
            return None;
        }
        let iife = get_immediately_invoked_function_expression(fn_);
        if let Some(iife) = iife {
            let args = self.get_effective_call_arguments(iife);
            let index_of_parameter = fn_.parameters().iter().position(|&p| p == parameter)?;
            if has_dot_dot_dot_token(parameter) {
                let any_type = self.any_type;
                return Some(self.get_spread_argument_type(
                    &args,
                    index_of_parameter,
                    args.len(),
                    any_type,
                    None, /*context*/
                    CheckMode::Normal,
                ));
            }
            let links = self.signature_links.get(iife);
            let cached = links.resolved_signature.get();
            links.resolved_signature.set(Some(self.any_signature));

            let t: Option<P<Type>> = if index_of_parameter < args.len() {
                let arg_type = self.check_expression(args[index_of_parameter]);
                Some(self.get_widened_literal_type(arg_type))
            } else if parameter.initializer().is_some() {
                None
            } else {
                Some(self.undefined_widening_type)
            };
            links.resolved_signature.set(cached);
            return t;
        }
        let contextual_signature = self.get_contextual_signature(fn_);
        if let Some(contextual_signature) = contextual_signature {
            let index = fn_
                .parameters()
                .iter()
                .position(|&p| p == parameter)?
                .checked_sub(usize::from(get_this_parameter(fn_).is_some()))?;
            if has_dot_dot_dot_token(parameter) && last_or_nil(fn_.parameters()) == Some(parameter)
            {
                return Some(self.get_rest_type_at_position(contextual_signature, index, false));
            }
            return self.try_get_type_at_position(contextual_signature, index);
        }
        None
    }

    // checker.go:29985
    pub(crate) fn is_context_sensitive_function_or_object_literal_method(
        &mut self,
        fn_: P<Node>,
    ) -> bool {
        (is_function_expression_or_arrow_function(fn_) || is_object_literal_method(fn_))
            && self.is_context_sensitive_function_like_declaration(fn_)
    }

    // checker.go:29989
    pub(crate) fn get_spread_argument_type(
        &mut self,
        args: &[P<Node>],
        index: usize,
        arg_count: usize,
        rest_type: P<Type>,
        context: Option<P<InferenceContext>>,
        check_mode: CheckMode,
    ) -> P<Type> {
        let in_const_context = self.is_const_type_variable(Some(rest_type), 0);
        if arg_count > 0 && index >= arg_count - 1 {
            let mut arg = args[arg_count - 1];
            if is_spread_argument(arg) {
                // We are inferring from a spread expression in the last argument position, i.e. both the parameter
                // and the argument are ...x forms.
                let spread_type = if is_synthetic_expression(arg) {
                    synthetic_expression_type(arg)
                } else {
                    self.check_expression_with_contextual_type(
                        arg.expression().unwrap(),
                        rest_type,
                        context,
                        check_mode,
                    )
                };
                if self.is_array_like_type(spread_type) {
                    return self.get_mutable_array_or_tuple_type(spread_type);
                }
                if is_spread_element(arg) {
                    arg = arg.expression().unwrap();
                }
                let undefined_type = self.undefined_type;
                let element_type = self.check_iterated_type_or_element_type(
                    IterationUse::Spread,
                    spread_type,
                    undefined_type,
                    Some(arg),
                );
                return self.create_array_type_ex(element_type, in_const_context);
            }
        }
        let mut types: Vec<P<Type>> = Vec::new();
        let mut infos: Vec<TupleElementInfo> = Vec::new();
        for (i, &arg) in args.iter().enumerate().take(arg_count).skip(index) {
            let t: P<Type>;
            let mut info = TupleElementInfo::default();
            if is_spread_argument(arg) {
                let spread_type = if is_synthetic_expression(arg) {
                    synthetic_expression_type(arg)
                } else {
                    self.check_expression(arg.expression().unwrap())
                };
                if self.is_array_like_type(spread_type) {
                    t = spread_type;
                    info.flags = ElementFlags::Variadic;
                } else {
                    let undefined_type = self.undefined_type;
                    if is_spread_element(arg) {
                        t = self.check_iterated_type_or_element_type(
                            IterationUse::Spread,
                            spread_type,
                            undefined_type,
                            arg.expression(),
                        );
                    } else {
                        t = self.check_iterated_type_or_element_type(
                            IterationUse::Spread,
                            spread_type,
                            undefined_type,
                            Some(arg),
                        );
                    }
                    info.flags = ElementFlags::Rest;
                }
            } else {
                let contextual_type = if is_tuple_type(rest_type) {
                    match self.get_contextual_type_for_element_expression(
                        Some(rest_type),
                        i - index,
                        Some(arg_count - index),
                        None,
                        None,
                    ) {
                        Some(t) => t,
                        None => self.unknown_type,
                    }
                } else {
                    let index_type =
                        self.get_number_literal_type(jsnum::Number((i - index) as f64));
                    self.get_indexed_access_type_ex(
                        rest_type,
                        index_type,
                        AccessFlags::Contextual,
                        None,
                        AliasArg::None,
                    )
                };
                let arg_type = self.check_expression_with_contextual_type(
                    arg,
                    contextual_type,
                    context,
                    check_mode,
                );
                let has_primitive_contextual_type = in_const_context
                    || self.maybe_type_of_kind(
                        contextual_type,
                        TypeFlags::Primitive
                            | TypeFlags::Index
                            | TypeFlags::TemplateLiteral
                            | TypeFlags::StringMapping,
                    );
                if has_primitive_contextual_type {
                    t = self.get_regular_type_of_literal_type(arg_type);
                } else {
                    t = self.get_widened_literal_type(arg_type);
                }
                info.flags = ElementFlags::Required;
            }
            if is_synthetic_expression(arg)
                && let Some(tuple_name_source) = arg.as_synthetic_expression().tuple_name_source
            {
                info.labeled_declaration = Some(tuple_name_source);
            }
            types.push(t);
            infos.push(info);
        }
        let readonly = in_const_context
            && !some_type(self, rest_type, super::checker::Checker::is_mutable_array_like_type);
        self.create_tuple_type_ex(&types, &infos, readonly)
    }

    // checker.go:30060
    fn get_mutable_array_or_tuple_type(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Union) {
            return self.map_type(t, |c, t| Some(c.get_mutable_array_or_tuple_type(t))).unwrap();
        }
        if t.flags().intersects(TypeFlags::Any) || {
            let base = self.get_base_constraint_or_type(t);
            self.is_mutable_array_or_tuple(base)
        } {
            return t;
        }
        if is_tuple_type(t) {
            let element_types = self.get_element_types(t);
            return self.create_tuple_type_ex(
                element_types,
                t.target_tuple_type().element_infos.get(),
                false, /*readonly*/
            );
        }
        self.create_tuple_type_ex(
            &[t],
            &[TupleElementInfo { flags: ElementFlags::Variadic, labeled_declaration: None }],
            false,
        )
    }

    // checker.go:30072
    fn get_contextual_type_for_binding_element(
        &mut self,
        declaration: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        let name = declaration.property_name_or_name().unwrap();
        if is_binding_pattern(name) || is_computed_non_literal_name(name) {
            return None;
        }
        let parent = declaration.parent().unwrap().parent().unwrap();
        let mut parent_type =
            self.get_contextual_type_for_variable_like_declaration(parent, context_flags);
        if parent_type.is_none() && !is_binding_element(parent) && parent.initializer().is_some() {
            parent_type = Some(self.check_declaration_initializer(
                parent,
                if_else(
                    has_dot_dot_dot_token(declaration),
                    CheckMode::RestBindingElement,
                    CheckMode::Normal,
                ),
                None,
            ));
        }
        let parent_type = parent_type?;
        if is_array_binding_pattern(parent.name().unwrap()) {
            let index =
                declaration.parent().unwrap().elements().iter().position(|&e| e == declaration);
            let index = index?;
            return self.get_contextual_type_for_element_expression(
                Some(parent_type),
                index,
                None,
                None,
                None,
            );
        }
        let name_type = self.get_literal_type_from_property_name(name);
        if is_type_usable_as_property_name(name_type) {
            return self.get_type_of_property_of_type(
                parent_type,
                &get_property_name_from_type(name_type),
            );
        }
        None
    }

    // checker.go:30101
    // Keep the static-property path out of its caller (notes/typescript-623-ports.md, entry 5).
    #[inline(never)]
    fn get_contextual_type_for_static_property_declaration(
        &mut self,
        declaration: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        let decl_parent = declaration.parent().unwrap();
        if is_expression(decl_parent) {
            // Don't contextually type a static property by its own class, its type might still be in-progress and that would cause spurious circularities
            if let Some(parent_type) = self.get_contextual_type(decl_parent, context_flags)
                && parent_type.symbol() != self.get_symbol_of_declaration(decl_parent)
            {
                let name = self.get_symbol_of_declaration(declaration).unwrap().name();
                return self.get_type_of_property_of_contextual_type(parent_type, name);
            }
        }
        None
    }

    // checker.go:30110
    fn get_contextual_type_for_return_expression(
        &mut self,
        node: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        let fn_ = get_containing_function(node);
        if let Some(fn_) = fn_ {
            let contextual_return_type = self.get_contextual_return_type(fn_, context_flags);
            if let Some(mut contextual_return_type) = contextual_return_type {
                let function_flags = get_function_flags(Some(fn_));
                if function_flags.intersects(FunctionFlags::Generator) {
                    let is_async_generator = function_flags.intersects(FunctionFlags::Async);
                    if contextual_return_type.flags().intersects(TypeFlags::Union) {
                        contextual_return_type =
                            self.filter_type(contextual_return_type, |c, t| {
                                c.get_iteration_type_of_generator_function_return_type(
                                    IterationTypeKind::Return,
                                    t,
                                    is_async_generator,
                                )
                                .is_some()
                            });
                    }
                    let iteration_return_type = self
                        .get_iteration_type_of_generator_function_return_type(
                            IterationTypeKind::Return,
                            contextual_return_type,
                            function_flags.intersects(FunctionFlags::Async),
                        );
                    let iteration_return_type = iteration_return_type?;
                    contextual_return_type = iteration_return_type;
                    // falls through to unwrap Promise for AsyncGenerators
                }
                if function_flags.intersects(FunctionFlags::Async) {
                    // Get the awaited type without the `Awaited<T>` alias
                    let contextual_awaited_type = self.map_type(
                        contextual_return_type,
                        super::checker::Checker::get_awaited_type_no_alias,
                    );
                    let contextual_awaited_type = contextual_awaited_type?;
                    let promise_like = self.create_promise_like_type(contextual_awaited_type);
                    return Some(self.get_union_type(&[contextual_awaited_type, promise_like]));
                }
                // Regular function or Generator function
                return Some(contextual_return_type);
            }
        }
        None
    }

    // checker.go:30145
    pub(crate) fn get_contextual_iteration_type(
        &mut self,
        kind: IterationTypeKind,
        function_decl: P<Node>,
    ) -> Option<P<Type>> {
        let is_async = get_function_flags(Some(function_decl)).intersects(FunctionFlags::Async);
        let contextual_return_type =
            self.get_contextual_return_type(function_decl, ContextFlags::None);
        if let Some(contextual_return_type) = contextual_return_type {
            return self.get_iteration_type_of_generator_function_return_type(
                kind,
                contextual_return_type,
                is_async,
            );
        }
        None
    }

    // checker.go:30154
    pub(crate) fn get_contextual_return_type(
        &mut self,
        function_decl: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        // If the containing function has a return type annotation, is a constructor, or is a get accessor whose
        // corresponding set accessor has a type annotation, return statements in the function are contextually typed
        let return_type = self.get_return_type_from_annotation(function_decl);
        if return_type.is_some() {
            return return_type;
        }
        // Otherwise, if the containing function is contextually typed by a function type with exactly one call signature
        // and that call signature is non-generic, return statements are contextually typed by the return type of the signature
        let signature = self.get_contextual_signature_for_function_like_declaration(function_decl);
        if let Some(signature) = signature
            && !self.is_resolving_return_type_of_signature(signature)
        {
            let return_type = self.get_return_type_of_signature(signature);
            let function_flags = get_function_flags(Some(function_decl));
            if function_flags.intersects(FunctionFlags::Generator) {
                return Some(self.filter_type(return_type, |c, t| {
                    t.flags().intersects(
                        TypeFlags::AnyOrUnknown
                            | TypeFlags::Void
                            | TypeFlags::InstantiableNonPrimitive,
                    ) || c.check_generator_instantiation_assignability_to_return_type(
                        t,
                        function_flags,
                        None, /*errorNode*/
                    )
                }));
            }
            if function_flags.intersects(FunctionFlags::Async) {
                return Some(self.filter_type(return_type, |c, t| {
                    t.flags().intersects(
                        TypeFlags::AnyOrUnknown
                            | TypeFlags::Void
                            | TypeFlags::InstantiableNonPrimitive,
                    ) || c.get_awaited_type_of_promise(t).is_some()
                }));
            }
            return Some(return_type);
        }
        let iife = get_immediately_invoked_function_expression(function_decl);
        if let Some(iife) = iife {
            return self.get_contextual_type(iife, context_flags);
        }
        None
    }

    // checker.go:30186
    pub(crate) fn check_generator_instantiation_assignability_to_return_type(
        &mut self,
        return_type: P<Type>,
        function_flags: FunctionFlags,
        error_node: Option<P<Node>>,
    ) -> bool {
        // Naively, one could check that Generator<any, any, any> is assignable to the return type annotation.
        // However, that would not catch the error in the following case.
        //
        //    interface BadGenerator extends Iterable<number>, Iterator<string> { }
        //    function* g(): BadGenerator { } // Iterable and Iterator have different types!
        //
        let is_async = function_flags.intersects(FunctionFlags::Async);
        let generator_yield_type = match self.get_iteration_type_of_generator_function_return_type(
            IterationTypeKind::Yield,
            return_type,
            is_async,
        ) {
            Some(t) => t,
            None => self.any_type,
        };
        let generator_return_type = self
            .get_iteration_type_of_generator_function_return_type(
                IterationTypeKind::Return,
                return_type,
                is_async,
            )
            .unwrap_or(generator_yield_type);
        let generator_next_type = match self.get_iteration_type_of_generator_function_return_type(
            IterationTypeKind::Next,
            return_type,
            is_async,
        ) {
            Some(t) => t,
            None => self.unknown_type,
        };
        let generator_instantiation = self.create_generator_type(
            generator_yield_type,
            generator_return_type,
            generator_next_type,
            is_async,
        );
        self.check_type_assignable_to(generator_instantiation, return_type, error_node, None)
    }

    // checker.go:30200
    pub(crate) fn get_contextual_signature_for_function_like_declaration(
        &mut self,
        node: P<Node>,
    ) -> Option<P<Signature>> {
        // Only function expressions, arrow functions, and object literal methods are contextually typed.
        if is_function_expression_or_arrow_function(node) || is_object_literal_method(node) {
            return self.get_contextual_signature(node);
        }
        None
    }

    // checker.go:30208
    fn get_contextual_type_for_yield_operand(
        &mut self,
        node: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        let fn_ = get_containing_function(node);
        if let Some(fn_) = fn_ {
            let function_flags = get_function_flags(Some(fn_));
            let contextual_return_type = self.get_contextual_return_type(fn_, context_flags);
            if let Some(mut contextual_return_type) = contextual_return_type {
                let is_async_generator = function_flags.intersects(FunctionFlags::Async);
                let is_yield_star = node.as_yield_expression().asterisk_token.is_some();
                if !is_yield_star && contextual_return_type.flags().intersects(TypeFlags::Union) {
                    contextual_return_type = self.filter_type(contextual_return_type, |c, t| {
                        c.get_iteration_type_of_generator_function_return_type(
                            IterationTypeKind::Return,
                            t,
                            is_async_generator,
                        )
                        .is_some()
                    });
                }
                if is_yield_star {
                    let iteration_types = self
                        .get_iteration_types_of_generator_function_return_type(
                            contextual_return_type,
                            is_async_generator,
                        );
                    let yield_type = iteration_types.yield_type.unwrap_or(self.silent_never_type);
                    let return_type = match self.get_contextual_type(node, context_flags) {
                        Some(t) => t,
                        None => self.silent_never_type,
                    };
                    let next_type = iteration_types.next_type.unwrap_or(self.unknown_type);
                    let generator_type = self.create_generator_type(
                        yield_type,
                        return_type,
                        next_type,
                        false, /*isAsyncGenerator*/
                    );
                    if is_async_generator {
                        let async_generator_type = self.create_generator_type(
                            yield_type,
                            return_type,
                            next_type,
                            true, /*isAsyncGenerator*/
                        );
                        return Some(self.get_union_type(&[generator_type, async_generator_type]));
                    }
                    return Some(generator_type);
                }
                return self.get_iteration_type_of_generator_function_return_type(
                    IterationTypeKind::Yield,
                    contextual_return_type,
                    is_async_generator,
                );
            }
        }
        None
    }

    // checker.go:30239
    fn get_contextual_type_for_await_operand(
        &mut self,
        node: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        let contextual_type = self.get_contextual_type(node, context_flags);
        if let Some(contextual_type) = contextual_type {
            let contextual_awaited_type = self.get_awaited_type_no_alias(contextual_type);
            if let Some(contextual_awaited_type) = contextual_awaited_type {
                let promise_like = self.create_promise_like_type(contextual_awaited_type);
                return Some(self.get_union_type(&[contextual_awaited_type, promise_like]));
            }
        }
        None
    }

    // In a typed function call, an argument or substitution expression is contextually typed by the type of the corresponding parameter.
    // checker.go:30251
    pub(crate) fn get_contextual_type_for_argument(
        &mut self,
        call_target: P<Node>,
        arg: P<Node>,
    ) -> Option<P<Type>> {
        let args = self.get_effective_call_arguments(call_target);
        let arg_index = args.iter().position(|&a| a == arg);
        // -1 for e.g. the expression of a CallExpression, or the tag of a TaggedTemplateExpression
        let arg_index = arg_index?;
        Some(self.get_contextual_type_for_argument_at_index(call_target, arg_index))
    }

    // checker.go:30261
    pub(crate) fn get_contextual_type_for_argument_at_index(
        &mut self,
        call_target: P<Node>,
        arg_index: usize,
    ) -> P<Type> {
        if is_import_call(call_target) {
            return if arg_index == 0 {
                self.string_type
            } else if arg_index == 1 {
                self.get_global_import_call_options_type()
            } else {
                self.any_type
            };
        }
        // If we're already in the process of resolving the given signature, don't resolve again as
        // that could cause infinite recursion. Instead, return anySignature.
        let signature = if self.signature_links.get(call_target).resolved_signature.get()
            == Some(self.resolving_signature)
        {
            // Not the final contextual type (flow memo).
            self.flow_memo.taint(0);
            self.resolving_signature
        } else {
            self.get_resolved_signature(call_target, None, CheckMode::Normal)
        };
        if is_jsx_opening_like_element(call_target) && arg_index == 0 {
            return self.get_effective_first_argument_for_jsx_signature(signature, call_target);
        }
        let rest_index = signature.parameters.get().len().saturating_sub(1);
        if signature_has_rest_parameter(signature) && arg_index >= rest_index {
            let rest_type = self.get_type_of_symbol(signature.parameters.get()[rest_index]);
            let index_type =
                self.get_number_literal_type(jsnum::Number((arg_index - rest_index) as f64));
            return self.get_indexed_access_type_ex(
                rest_type,
                index_type,
                AccessFlags::Contextual,
                None,
                AliasArg::None,
            );
        }
        self.get_type_at_position(signature, arg_index)
    }

    // checker.go:30290
    fn get_contextual_type_for_decorator(&mut self, decorator: P<Node>) -> Option<P<Type>> {
        let signature = self.get_decorator_call_signature(decorator);
        if let Some(signature) = signature {
            return Some(self.get_or_create_type_from_signature(signature));
        }
        None
    }

    // checker.go:30298
    fn get_contextual_type_for_binary_operand(
        &mut self,
        node: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        let binary_node = node.parent().unwrap();
        let binary = binary_node.as_binary_expression();
        if let Some(t) = binary.type_.get() {
            return Some(self.get_type_from_type_node(t));
        }
        match binary.operator_token.kind() {
            Kind::EqualsToken
            | Kind::AmpersandAmpersandEqualsToken
            | Kind::BarBarEqualsToken
            | Kind::QuestionQuestionEqualsToken => {
                // In an assignment expression, the right operand is contextually typed by the type of the left operand
                // unless it's an assignment declaration.
                if node == binary.right.get() {
                    let target = get_leftmost_expression(binary.left, false);
                    if !(is_identifier(target)
                        && self
                            .get_resolved_symbol(target)
                            .flags()
                            .intersects(SymbolFlags::ModuleExports))
                    {
                        return self.get_contextual_type_for_assignment_expression(binary_node);
                    }
                }
            }
            Kind::BarBarToken | Kind::QuestionQuestionToken => {
                // When an || expression has a contextual type, the operands are contextually typed by that type, except
                // when that type originates in a binding pattern, the right operand is contextually typed by the type of
                // the left operand. When an || expression has no contextual type, the right operand is contextually typed
                // by the type of the left operand, except for the special case of Javascript declarations of the form
                // `namespace.prop = namespace.prop || {}`.
                let t = self.get_contextual_type(binary_node, context_flags);
                if node == binary.right.get()
                    && (t.is_none() || self.pattern_for_type.contains_key(&t.unwrap()))
                {
                    return Some(self.get_type_of_expression(binary.left));
                }
                return t;
            }
            Kind::AmpersandAmpersandToken | Kind::CommaToken if node == binary.right.get() => {
                return self.get_contextual_type(binary_node, context_flags);
            }
            _ => {}
        }
        None
    }

    // checker.go:30332
    fn get_contextual_type_for_assignment_expression(
        &mut self,
        binary: P<Node>,
    ) -> Option<P<Type>> {
        let left = binary.as_binary_expression().left;
        if is_access_expression(left) {
            let expr = left.expression().unwrap();
            match expr.kind() {
                Kind::Identifier => {
                    let resolved = self.get_resolved_symbol(expr);
                    let symbol =
                        self.get_export_symbol_of_value_symbol_if_exported(Some(resolved)).unwrap();
                    if symbol.flags().intersects(SymbolFlags::ModuleExports) {
                        // No contextual type for an expression of the form 'module.exports = expr'.
                        return None;
                    }
                    if binary.symbol().is_some() {
                        // We have an assignment declaration (a binary expression with a symbol assigned by the binder) of the form
                        // 'F.id = expr' or 'F[xxx] = expr'. If 'F' is declared as a variable with a type annotation, we can obtain a
                        // contextual type from the annotated type without triggering a circularity. Otherwise, the assignment
                        // declaration has no contextual type.
                        if let Some(value_declaration) =
                            symbol.value_declaration().filter(|&d| is_variable_declaration(d))
                            && let Some(type_node) = value_declaration.type_node()
                        {
                            if is_property_access_expression(left) {
                                let t = self.get_type_from_type_node(type_node);
                                return self.get_type_of_property_of_contextual_type(
                                    t,
                                    left.name().unwrap().text(),
                                );
                            }
                            let name_type = self.check_expression_cached(
                                left.as_element_access_expression().argument_expression,
                            );
                            if is_type_usable_as_property_name(name_type) {
                                let t = self.get_type_from_type_node(type_node);
                                return self.get_type_of_property_of_contextual_type_ex(
                                    t,
                                    &get_property_name_from_type(name_type),
                                    Some(name_type),
                                );
                            }
                            return Some(self.get_type_of_expression(left));
                        }
                        return None;
                    }
                }
                Kind::PropertyAccessExpression | Kind::ElementAccessExpression => {
                    if binary.symbol().is_some() {
                        return None;
                    }
                }
                Kind::ThisKeyword => {
                    let mut symbol: Option<P<Symbol>> = None;
                    let this_type = self.get_type_of_expression(expr);
                    if is_property_access_expression(left) {
                        let name = left.name().unwrap();
                        if is_private_identifier(name) {
                            if let Some(this_symbol) = this_type.symbol() {
                                symbol = self.get_property_of_type(
                                    this_type,
                                    &tsrs_binder::get_symbol_name_for_private_identifier(
                                        this_symbol,
                                        name.text(),
                                    ),
                                );
                            }
                        } else {
                            symbol = self.get_property_of_type(this_type, name.text());
                        }
                    } else {
                        let prop_type = self.check_expression_cached(
                            left.as_element_access_expression().argument_expression,
                        );
                        if is_type_usable_as_property_name(prop_type) {
                            symbol = self.get_property_of_type(
                                this_type,
                                &get_property_name_from_type(prop_type),
                            );
                        }
                    }
                    if let Some(symbol) = symbol
                        && let Some(d) = symbol.value_declaration()
                        && (is_property_declaration(d) || is_property_signature_declaration(d))
                        && d.type_node().is_none()
                        && d.initializer().is_none()
                    {
                        // No contextual type for 'this.xxx = expr', where xxx is declared as a property with no type annotation or initializer.
                        return None;
                    }
                    if let Some(binary_symbol) = binary.symbol()
                        && binary_symbol
                            .value_declaration()
                            .is_some_and(|d| d.type_node().is_none())
                    {
                        // We have an assignment declaration 'this.xxx = expr' with no (synthetic) type annotation
                        let this_container = Self::get_this_container(expr, false, false);
                        if !is_object_literal_method(this_container) {
                            return None;
                        }
                        // and now for one single case of object literal methods
                        let name = get_element_or_property_access_name(left);
                        name?;
                        // !!! contextual typing for `this` in object literals
                        return None;
                    }
                }
                _ => {}
            }
        }
        Some(self.get_type_of_expression(left))
    }

    // checker.go:30409
    pub(crate) fn get_contextual_type_for_object_literal_element(
        &mut self,
        element: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        if let Some(t) = element.type_node()
            && !is_object_literal_method(element)
        {
            return Some(self.get_type_from_type_node(t));
        }
        let object_literal = element.parent().unwrap();
        let t = self.get_apparent_type_of_contextual_type(object_literal, context_flags);
        if let Some(t) = t {
            if self.has_bindable_name(element) {
                // For a (non-symbol) computed property, there is no reason to look up the name
                // in the type. It will just be "__computed", which does not appear in any
                // SymbolTable.
                let symbol = self.get_symbol_of_declaration(element).unwrap();
                // Read-only: no links slot for the 0.96M object literal members on the private monorepo that never get any (the
                // symbol's id is assigned the same way).
                let name_type =
                    self.value_symbol_links.try_get(symbol).and_then(|links| links.name_type());
                return self.get_type_of_property_of_contextual_type_ex(
                    t,
                    symbol.name(),
                    name_type,
                );
            }
            if has_dynamic_name(element) {
                let name = get_name_of_declaration(element);
                if let Some(name) = name.filter(|&n| is_computed_property_name(n)) {
                    let expr_type = self.check_expression(name.expression().unwrap());
                    if is_type_usable_as_property_name(expr_type) {
                        let prop_type = self.get_type_of_property_of_contextual_type(
                            t,
                            &get_property_name_from_type(expr_type),
                        );
                        if prop_type.is_some() {
                            return prop_type;
                        }
                    }
                }
            }
            if let Some(element_name) = element.name() {
                let name_type = self.get_literal_type_from_property_name(element_name);
                // We avoid calling getApplicableIndexInfo here because it performs potentially expensive intersection reduction.
                return self.map_type_ex(
                    t,
                    |c, t| {
                        let index_infos = c.get_index_infos_of_structured_type(t);
                        let index_info = c.find_applicable_index_info(index_infos, name_type);
                        index_info.map(|info| info.value_type())
                    },
                    true, /*noReductions*/
                );
            }
        }
        None
    }

    // In an object literal contextually typed by a type T, the contextual type of a property assignment is the type of
    // the matching property in T, if one exists. Otherwise, it is the type of the numeric index signature in T, if one
    // exists. Otherwise, it is the type of the string index signature in T, if one exists.
    // checker.go:30453
    pub(crate) fn get_contextual_type_for_object_literal_method(
        &mut self,
        node: P<Node>,
        context_flags: ContextFlags,
    ) -> Option<P<Type>> {
        if node.flags().intersects(NodeFlags::InWithStatement) {
            // We cannot answer semantic questions within a with block, do not proceed any further
            return None;
        }
        self.get_contextual_type_for_object_literal_element(node, context_flags)
    }

    // t Option<P<Type>>: Go checks t == nil (getContextualType passes getApparentTypeOfContextualType's result)
    // checker.go:30461
    pub(crate) fn get_contextual_type_for_element_expression(
        &mut self,
        t: Option<P<Type>>,
        index: usize,
        length: Option<usize>,
        first_spread_index: Option<usize>,
        last_spread_index: Option<usize>,
    ) -> Option<P<Type>> {
        let t = t?;
        self.map_type_ex(
            t,
            |c, t| {
                if is_tuple_type(t) {
                    let tuple = t.target_tuple_type();
                    // If index is before any spread element and within the fixed part of the contextual tuple type, return
                    // the type of the contextual tuple element.
                    if first_spread_index.is_none_or(|first| index < first)
                        && index < tuple.fixed_length.get()
                    {
                        let type_argument = c.get_type_arguments(t)[index];
                        return Some(
                            c.remove_missing_type(
                                type_argument,
                                tuple.element_infos.get()[index]
                                    .flags
                                    .intersects(ElementFlags::Optional),
                            ),
                        );
                    }
                    // When the length is known and the index is after all spread elements we compute the offset from the element
                    // to the end and the number of ending fixed elements in the contextual tuple type.
                    let offset = length
                        .filter(|_| last_spread_index.is_none_or(|last| index > last))
                        .and_then(|length| length.checked_sub(index))
                        .filter(|&offset| offset > 0);
                    let fixed_end_length = if offset.is_some()
                        && tuple.combined_flags.get().intersects(ElementFlags::Variable)
                    {
                        get_end_element_count(tuple, ElementFlags::Fixed)
                    } else {
                        0
                    };
                    // If the offset is within the ending fixed part of the contextual tuple type, return the type of the contextual
                    // tuple element.
                    if offset.is_some_and(|offset| offset <= fixed_end_length) {
                        let arity = checker::Checker::get_type_reference_arity(t);
                        return Some(c.get_type_arguments(t)[arity - offset.unwrap()]);
                    }
                    // Return a union of the possible contextual element types with no subtype reduction.
                    let mut tuple_index = tuple.fixed_length.get();
                    if let Some(first_spread_index) = first_spread_index {
                        tuple_index = tuple_index.min(first_spread_index);
                    }
                    let mut end_skip_count = fixed_end_length;
                    if let (Some(length), Some(last_spread_index)) = (length, last_spread_index) {
                        end_skip_count = fixed_end_length.min(length - last_spread_index);
                    }
                    return c.get_element_type_of_slice_of_tuple_type(
                        t,
                        tuple_index,
                        end_skip_count,
                        false, /*writing*/
                        true,  /*noReductions*/
                    );
                }
                // If element index is known and a contextual property with that name exists, return it. Otherwise return the
                // iterated or element type of the contextual type.
                if first_spread_index.is_none_or(|first| index < first) {
                    let prop_type =
                        c.get_type_of_property_of_contextual_type(t, &index.to_string());
                    if prop_type.is_some() {
                        return prop_type;
                    }
                }
                let undefined_type = c.undefined_type;
                c.get_iterated_type_or_element_type(
                    IterationUse::Element,
                    t,
                    undefined_type,
                    None,  /*errorNode*/
                    false, /*checkAssignability*/
                )
            },
            true, /*noReductions*/
        )
    }
}
