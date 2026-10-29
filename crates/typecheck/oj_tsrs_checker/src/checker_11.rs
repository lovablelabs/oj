use crate::*;
use rustc_hash::FxHashMap;
use std::borrow::Cow;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::collections::OrderedSet;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

// Non-function declarations in checker.go:21764-23953 (hand-ported by checker-foundation):
//   type TupleNormalizer (checker.go:23816)

impl Checker {
    // checker.go:21769
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn get_union_or_intersection_property(
        &mut self,
        t: P<Type>,
        name: &str,
        skip_object_function_property_augment: bool,
    ) -> Option<P<Symbol>> {
        if self.lazy_prop_cache {
            return self.get_union_or_intersection_property_lazy_cache(
                t,
                name,
                skip_object_function_property_augment,
            );
        }
        let cache = t
            .as_union_or_intersection_type()
            .property_cache_for_write(skip_object_function_property_augment);
        if let Some(prop) = cache.lookup(name) {
            return Some(prop);
        }
        let prop = self.create_union_or_intersection_property(
            t,
            name,
            skip_object_function_property_augment,
        );
        if let Some(prop) = prop {
            // The caches are only looked up by name, so the key can be the property's own name when it is the same
            // text (it is for every property this creates), and both caches can share one copy otherwise.
            let key = if prop.name() == name { prop.name() } else { alloc_str(name) };
            cache.set(key, prop);
            // Propagate an entry from the non-augmented cache to the augmented cache unless the property is partial.
            if skip_object_function_property_augment
                && !prop.check_flags().intersects(CheckFlags::Partial)
            {
                let augmented_cache =
                    t.as_union_or_intersection_type().property_cache_for_write(false);
                if augmented_cache.lookup(name).is_none() {
                    augmented_cache.set(key, prop);
                }
            }
        }
        prop
    }

    // notes/mem-lazy.md L6: the same lookups without allocating a cache before there is something to store, and
    // without copying non-partial entries of the non-augmented cache into the augmented one: an augmented lookup
    // that misses reads them from the non-augmented cache, which is what the copy would have given it.
    #[cfg_attr(feature = "site-counts", track_caller)]
    fn get_union_or_intersection_property_lazy_cache(
        &mut self,
        t: P<Type>,
        name: &str,
        skip_object_function_property_augment: bool,
    ) -> Option<P<Symbol>> {
        let d = t.as_union_or_intersection_type();
        if let Some(prop) =
            d.property_cache(skip_object_function_property_augment).and_then(|c| c.lookup(name))
        {
            return Some(prop);
        }
        if !skip_object_function_property_augment
            && let Some(prop) = d.property_cache(true).and_then(|c| c.lookup(name))
            && !prop.check_flags().intersects(CheckFlags::Partial)
        {
            self.lazy_member_stats.prop_cache_shared_hits += 1;
            return Some(prop);
        }
        let prop = self.create_union_or_intersection_property(
            t,
            name,
            skip_object_function_property_augment,
        );
        if let Some(prop) = prop {
            let key = if prop.name() == name { prop.name() } else { alloc_str(name) };
            d.property_cache_for_write(skip_object_function_property_augment).set(key, prop);
            if skip_object_function_property_augment
                && !prop.check_flags().intersects(CheckFlags::Partial)
                && d.property_cache(false).is_none_or(|c| c.lookup(name).is_none())
            {
                self.lazy_member_stats.prop_cache_copies_avoided += 1;
            }
        }
        prop
    }

    // checker.go:21793
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn create_union_or_intersection_property(
        &mut self,
        containing_type: P<Type>,
        name: &str,
        skip_object_function_property_augment: bool,
    ) -> Option<P<Symbol>> {
        let mut prop_flags = SymbolFlags::None;
        let mut single_prop: Option<P<Symbol>> = None;
        let mut prop_set: CheckerOrderedSet<P<Symbol>> = CheckerOrderedSet::default();
        let mut index_types: Vec<P<Type>> = Vec::new();
        let is_union = containing_type.flags().intersects(TypeFlags::Union);
        // Flags we want to propagate to the result if they exist in all source symbols
        let mut check_flags = CheckFlags::None;
        let mut optional_flag = SymbolFlags::None;
        if !is_union {
            check_flags = CheckFlags::Readonly;
            optional_flag = SymbolFlags::Optional;
        }
        let mut synthetic_flag = CheckFlags::SyntheticMethod;
        let mut merged_instantiations = false;
        let key = HashedName::new(name);
        for &current in containing_type.types() {
            let t = self.get_apparent_type(current);
            if !self.is_error_type(t) && !t.flags().intersects(TypeFlags::Never) {
                let prop =
                    match self.try_resolved_property(t, key, skip_object_function_property_augment)
                    {
                        Some(prop) => prop,
                        None => self.get_property_of_type_ex(
                            t,
                            name,
                            skip_object_function_property_augment,
                            false,
                        ),
                    };
                if let Some(prop) = prop {
                    let modifiers = get_declaration_modifier_flags_from_symbol(prop);
                    // tsrs: a declared symbol without a set accessor has its read modifiers for writes.
                    let write_modifiers = if prop.check_flags().intersects(CheckFlags::Synthetic)
                        || prop.flags().intersects(SymbolFlags::SetAccessor)
                    {
                        get_declaration_modifier_flags_from_symbol_ex(prop, true /*isWrite*/)
                    } else {
                        modifiers
                    };
                    if prop.flags().intersects(SymbolFlags::ClassMember) {
                        if is_union {
                            optional_flag |= prop.flags() & SymbolFlags::Optional;
                        } else {
                            optional_flag &= prop.flags();
                        }
                    }
                    if single_prop.is_none() {
                        single_prop = Some(prop);
                        prop_flags = tsrs_core::or_else(
                            prop.flags() & SymbolFlags::Accessor,
                            SymbolFlags::Property,
                        );
                    } else if let Some(sp) = single_prop
                        && prop != sp
                    {
                        let is_instantiation =
                            self.get_target_symbol(prop) == self.get_target_symbol(sp);
                        // If the symbols are instances of one another with identical types - consider the symbols
                        // equivalent and just use the first one, which thus allows us to avoid eliding private
                        // members when intersecting a (this-)instantiations of a class with its raw base or another instance
                        if is_instantiation
                            && self
                                .compare_properties(sp, prop, |_, s, t| compare_types_equal(s, t))
                                == Ternary::TRUE
                        {
                            // If we merged instantiations of a generic type, we replicate the symbol parent resetting behavior we used
                            // to do when we recorded multiple distinct symbols so that we still get, eg, `Array<T>.length` printed
                            // back and not `Array<string>.length` when we're looking at a `.length` access on a `string[] | number[]`
                            merged_instantiations = match sp.parent() {
                                Some(parent) => !self
                                    .get_local_type_parameters_of_class_or_interface_or_type_alias(
                                        parent,
                                    )
                                    .is_empty(),
                                None => false,
                            };
                        } else {
                            if prop_set.values.is_empty() {
                                prop_set.add(sp);
                            }
                            if !prop_set.contains(prop) {
                                prop_set.add(prop);
                            }
                        }
                        // classes created by mixins are represented as intersections
                        // and overriding a property in a derived class redefines it completely at runtime
                        // so a get accessor can't be merged with a set accessor in a base class,
                        // for that reason the accessor flags are only used when they are the same in all constituents
                        if prop_flags.intersects(SymbolFlags::Accessor)
                            && (prop.flags() & SymbolFlags::Accessor)
                                != (prop_flags & SymbolFlags::Accessor)
                        {
                            prop_flags =
                                (prop_flags & !SymbolFlags::Accessor) | SymbolFlags::Property;
                        }
                    }
                    if is_union && self.is_readonly_symbol_with_modifiers(prop, || modifiers) {
                        check_flags |= CheckFlags::Readonly;
                    } else if !is_union
                        && !self.is_readonly_symbol_with_modifiers(prop, || modifiers)
                    {
                        check_flags &= !CheckFlags::Readonly;
                    }
                    if modifiers.intersects(ModifierFlags::Protected)
                        && !modifiers.intersects(ModifierFlags::Public)
                    {
                        check_flags |= CheckFlags::ContainsProtected;
                    } else if modifiers.intersects(ModifierFlags::Private)
                        && !modifiers.intersects(ModifierFlags::Public)
                    {
                        check_flags |= CheckFlags::ContainsPrivate;
                    } else {
                        check_flags |= CheckFlags::ContainsPublic;
                    }
                    if write_modifiers.intersects(ModifierFlags::Protected)
                        && !write_modifiers.intersects(ModifierFlags::Public)
                    {
                        check_flags |= CheckFlags::ContainsWriteProtected;
                    } else if write_modifiers.intersects(ModifierFlags::Private)
                        && !write_modifiers.intersects(ModifierFlags::Public)
                    {
                        check_flags |= CheckFlags::ContainsWritePrivate;
                    } else {
                        check_flags |= CheckFlags::ContainsWritePublic;
                    }
                    if modifiers.intersects(ModifierFlags::Static) {
                        check_flags |= CheckFlags::ContainsStatic;
                    }
                    if !is_prototype_property(prop) {
                        synthetic_flag = CheckFlags::SyntheticProperty;
                    }
                } else if is_union {
                    let mut index_info: Option<P<IndexInfo>> = None;
                    if !is_late_bound_name(name) {
                        index_info = self.get_applicable_index_info_for_name(t, name);
                    }
                    if let Some(index_info) = index_info {
                        prop_flags = (prop_flags & !SymbolFlags::Accessor) | SymbolFlags::Property;
                        check_flags |= CheckFlags::WritePartial
                            | tsrs_core::if_else(
                                index_info.is_readonly(),
                                CheckFlags::Readonly,
                                CheckFlags::None,
                            );
                        if is_tuple_type(t) {
                            let index_type =
                                self.get_rest_type_of_tuple_type(t).unwrap_or(self.undefined_type);
                            index_types.push(index_type);
                        } else {
                            index_types.push(index_info.value_type());
                        }
                    } else if is_object_literal_type(t)
                        && !t.object_flags().intersects(ObjectFlags::ContainsSpread)
                    {
                        check_flags |= CheckFlags::WritePartial;
                        index_types.push(self.undefined_type);
                    } else {
                        check_flags |= CheckFlags::ReadPartial;
                    }
                }
            }
        }
        let Some(single_prop) = single_prop else {
            // No property was found
            return None;
        };
        if (!Self::has_common_declaration(&prop_set.values) || prop_set.values.is_empty())
            && (check_flags.intersects(CheckFlags::Partial) || !prop_set.values.is_empty())
            && check_flags.intersects(
                CheckFlags::ContainsPrivate
                    | CheckFlags::ContainsProtected
                    | CheckFlags::ContainsWritePrivate
                    | CheckFlags::ContainsWriteProtected,
            )
            && is_union
        {
            // A property in a union has a private or protected declaration in one constituent, but is missing
            // or has a different declaration in another constituent. If the private or protected declaration is
            // for reading, we don't create a property.
            if check_flags.intersects(CheckFlags::ContainsPrivate | CheckFlags::ContainsProtected) {
                return None;
            }
            // Otherwise, if the private or protected declaration is for writing, reduce accessibility to that of
            // the most restricted constituent.
            if check_flags.intersects(CheckFlags::ContainsWritePrivate) {
                check_flags &=
                    !(CheckFlags::ContainsWritePublic | CheckFlags::ContainsWriteProtected);
            } else if check_flags.intersects(CheckFlags::ContainsWriteProtected) {
                check_flags &= !CheckFlags::ContainsWritePublic;
            }
        }
        if prop_set.values.is_empty()
            && !check_flags.intersects(CheckFlags::ReadPartial)
            && index_types.is_empty()
        {
            if !merged_instantiations {
                return Some(single_prop);
            }
            // No symbol from a union/intersection should have a `.parent` set (since unions/intersections don't act as symbol parents)
            // Unless that parent is "reconstituted" from the "first value declaration" on the symbol (which is likely different than its instantiated parent!)
            // They also have a `.containingType` set, which affects some services endpoints behavior, like `getRootSymbol`
            let mut single_prop_type: Option<P<Type>> = None;
            let mut single_prop_mapper: Option<P<TypeMapper>> = None;
            if single_prop.flags().intersects(SymbolFlags::Transient) {
                let links = self.value_symbol_links.get(single_prop);
                single_prop_type = links.resolved_type.get();
                single_prop_mapper = links.mapper();
            }
            let clone = self.create_symbol_with_type(single_prop, single_prop_type);
            if let Some(value_declaration) = single_prop.value_declaration() {
                clone.set_parent(value_declaration.symbol().unwrap().parent());
            }
            let links = self.value_symbol_links.get(clone);
            links.set_containing_type(Some(containing_type));
            links.set_mapper(single_prop_mapper);
            let write_type = self.get_write_type_of_symbol(single_prop);
            links.set_write_type(write_type);
            return Some(clone);
        }
        if prop_set.values.is_empty() {
            prop_set.add(single_prop);
        }
        let props = prop_set.values;
        let mut declarations: Vec<P<Node>> = Vec::with_capacity(props.len());
        let mut first_type: Option<P<Type>> = None;
        let mut name_type: Option<P<Type>> = None;
        let mut prop_types = self.free_type_lists.pop().unwrap_or_default();
        prop_types.reserve(props.len() + index_types.len());
        let mut write_types: Option<Vec<P<Type>>> = None;
        let mut first_value_declaration: Option<P<Node>> = None;
        let mut has_non_uniform_value_declaration = false;
        for prop in props {
            if first_value_declaration.is_none() {
                first_value_declaration = prop.value_declaration();
            } else if prop.value_declaration().is_some()
                && prop.value_declaration() != first_value_declaration
            {
                has_non_uniform_value_declaration = true;
            }
            for &declaration in prop.declarations() {
                tsrs_core::append_if_unique(&mut declarations, declaration);
            }
            let t = self.get_type_of_symbol(prop);
            if first_type.is_none() {
                first_type = Some(t);
                name_type = self.value_symbol_links.get(prop).name_type();
            }
            let write_type = self.get_write_type_of_symbol(prop);
            if write_types.is_some() || write_type != Some(t) {
                let wt = write_types.get_or_insert_with(|| prop_types.clone());
                wt.push(write_type.unwrap());
            }
            if Some(t) != first_type {
                check_flags |= CheckFlags::HasNonUniformType;
            }
            if is_literal_type(t) || self.is_pattern_literal_type(t) {
                check_flags |= CheckFlags::HasLiteralType;
            }
            if t.flags().intersects(TypeFlags::Never) && t != self.unique_literal_type {
                check_flags |= CheckFlags::HasNeverType;
            }
            prop_types.push(t);
        }
        prop_types.extend_from_slice(&index_types);
        let result = self.new_symbol_ex(
            prop_flags | optional_flag,
            alloc_str(name),
            check_flags | synthetic_flag,
        );
        result.set_declarations(&declarations);
        if !has_non_uniform_value_declaration
            && let Some(first_value_declaration) = first_value_declaration
        {
            result.set_value_declaration(Some(first_value_declaration));
            // Inherit information about parent type.
            result.set_parent(first_value_declaration.symbol().unwrap().parent());
        }
        let links = self.value_symbol_links.get(result);
        links.set_containing_type(Some(containing_type));
        links.set_name_type(name_type);
        if prop_types.len() > 2 {
            // When `propTypes` has the potential to explode in size when normalized, defer normalization until absolutely needed
            result.set_check_flags(result.check_flags() | CheckFlags::DeferredType);
            let deferred = self.deferred_symbol_links.get(result);
            deferred.parent.set(Some(containing_type));
            deferred.constituents.set(alloc_slice(&prop_types));
            self.free_type_list(prop_types);
            deferred.write_constituents.set(write_types.map_or(&[][..], alloc_vec));
            return Some(result);
        }
        if is_union {
            links.resolved_type.set(Some(self.get_union_type(&prop_types)));
        } else {
            links.resolved_type.set(Some(self.get_intersection_type(&prop_types)));
        }
        self.free_type_list(prop_types);
        if let Some(write_types) = write_types {
            if is_union {
                links.set_write_type(Some(self.get_union_type(&write_types)));
            } else {
                links.set_write_type(Some(self.get_intersection_type(&write_types)));
            }
        }
        Some(result)
    }

    // checker.go:22024
    pub(crate) fn get_target_symbol(&mut self, s: P<Symbol>) -> Option<P<Symbol>> {
        // if symbol is instantiated its flags are not copied from the 'target'
        // so we'll need to get back original 'target' symbol to work with correct set of flags
        if s.check_flags().intersects(CheckFlags::Instantiated) {
            return self.value_symbol_links.get(s).target();
        }
        Some(s)
    }
}

/// The occurrence count of each property name over a list of property lists, in first-occurrence order (Go's
/// `collections.OrderedMap[string, int]` in `somePropertyReducesToNever`).
#[derive(Default)]
pub struct NameCounts {
    pub lists: Vec<&'static [P<Symbol>]>,
    /// (name, name hash, count), in first-occurrence order.
    pub entries: Vec<(&'static str, u32, i32)>,
    /// Open addressing over `entries`: 0 is empty, otherwise an entry index plus one.
    pub table: Vec<u32>,
}

impl NameCounts {
    pub fn count_lists(&mut self, total: usize) {
        self.entries.clear();
        let size = (total * 2).max(16).next_power_of_two();
        self.table.clear();
        self.table.resize(size, 0);
        let mask = size - 1;
        for &list in &self.lists {
            for prop in list {
                let name = prop.name();
                let hash = HashedName::new(name).hash();
                let mut i = hash as usize & mask;
                loop {
                    let slot = self.table[i];
                    if slot == 0 {
                        self.entries.push((name, hash, 1));
                        self.table[i] = self.entries.len() as u32;
                        break;
                    }
                    let entry = &mut self.entries[slot as usize - 1];
                    if entry.1 == hash && (std::ptr::eq(entry.0, name) || entry.0 == name) {
                        entry.2 += 1;
                        break;
                    }
                    i = (i + 1) & mask;
                }
            }
        }
    }
}

/**
 * Return whether this symbol is a member of a prototype somewhere
 * Note that this is not tracked well within the compiler, so the answer may be incorrect.
 */
// checker.go:22037
pub(crate) fn is_prototype_property(symbol: P<Symbol>) -> bool {
    symbol.flags().intersects(SymbolFlags::Method)
        || symbol.check_flags().intersects(CheckFlags::SyntheticMethod)
}

impl Checker {
    // checker.go:22041
    fn has_common_declaration(symbols: &[P<Symbol>]) -> bool {
        let mut common_declarations: OrderedSet<P<Node>> = OrderedSet::default();
        for symbol in symbols {
            let symbol_declarations = symbol.declarations();
            if symbol_declarations.is_empty() {
                return false;
            }
            if common_declarations.is_empty() {
                common_declarations.extend(symbol_declarations.iter().copied());
                continue;
            }
            common_declarations.retain(|d| symbol_declarations.contains(d));
            if common_declarations.is_empty() {
                return false;
            }
        }
        !common_declarations.is_empty()
    }

    // checker.go:22065
    pub(crate) fn create_symbol_with_type(
        &mut self,
        source: P<Symbol>,
        t: Option<P<Type>>,
    ) -> P<Symbol> {
        let symbol = self.new_symbol_ex(
            source.flags(),
            source.name(),
            source.check_flags() & CheckFlags::Readonly,
        );
        symbol.set_declarations_static(source.declarations());
        symbol.set_parent(source.parent());
        symbol.set_value_declaration(source.value_declaration());
        let links = self.value_symbol_links.get(symbol);
        links.resolved_type.set(t);
        links.set_target(Some(source));
        links.set_name_type(self.value_symbol_links.get(source).name_type());
        symbol
    }

    // checker.go:22077
    pub(crate) fn is_mapped_type_generic_indexed_access(&mut self, t: P<Type>) -> bool {
        if t.flags().intersects(TypeFlags::IndexedAccess) {
            let object_type = t.as_indexed_access_type().object_type().unwrap();
            return object_type.object_flags().intersects(ObjectFlags::Mapped)
                && !self.is_generic_mapped_type(object_type)
                && self.is_generic_index_type(t.as_indexed_access_type().index_type().unwrap())
                && !get_mapped_type_modifiers(object_type)
                    .intersects(MappedTypeModifiers::ExcludeOptional)
                && object_type
                    .as_mapped_type()
                    .declaration
                    .get()
                    .unwrap()
                    .as_mapped_type_node()
                    .name_type
                    .is_none();
        }
        false
    }

    /**
     * For a type parameter, return the base constraint of the type parameter. For the string, number,
     * boolean, and symbol primitive types, return the corresponding object types. Otherwise return the
     * type itself.
     */
    // checker.go:22091
    /// tsrs: the case that returns `t` itself (no flag below and not mapped: most object types) inline; called about
    /// 1.8G times on the 38k-file codebase, where the out-of-line body spent most of its instructions on its frame.
    #[inline]
    pub(crate) fn get_apparent_type(&mut self, t: P<Type>) -> P<Type> {
        const SLOW: TypeFlags = TypeFlags::Instantiable
            .union(TypeFlags::Intersection)
            .union(TypeFlags::StringLike)
            .union(TypeFlags::NumberLike)
            .union(TypeFlags::BigIntLike)
            .union(TypeFlags::BooleanLike)
            .union(TypeFlags::ESSymbolLike)
            .union(TypeFlags::NonPrimitive)
            .union(TypeFlags::Index)
            .union(TypeFlags::Unknown);
        if !t.flags().intersects(SLOW) && !t.object_flags().intersects(ObjectFlags::Mapped) {
            return t;
        }
        self.get_apparent_type_worker(t)
    }

    #[inline(never)]
    fn get_apparent_type_worker(&mut self, t: P<Type>) -> P<Type> {
        let original_type = t;
        let mut t = t;
        if t.flags().intersects(TypeFlags::Instantiable) {
            t = self.get_base_constraint_of_type(t).unwrap_or(self.unknown_type);
        }
        if t.object_flags().intersects(ObjectFlags::Mapped) {
            return self.get_apparent_type_of_mapped_type(t);
        } else if t.object_flags().intersects(ObjectFlags::Reference) && t != original_type {
            return self.get_type_with_this_argument(
                t,
                Some(original_type),
                false, /*needsApparentType*/
            );
        } else if t.flags().intersects(TypeFlags::Intersection) {
            return self.get_apparent_type_of_intersection_type(t, original_type);
        } else if t.flags().intersects(TypeFlags::StringLike) {
            return self.global_string_type;
        } else if t.flags().intersects(TypeFlags::NumberLike) {
            return self.global_number_type;
        } else if t.flags().intersects(TypeFlags::BigIntLike) {
            return self.get_global_big_int_type();
        } else if t.flags().intersects(TypeFlags::BooleanLike) {
            return self.global_boolean_type;
        } else if t.flags().intersects(TypeFlags::ESSymbolLike) {
            return self.get_global_es_symbol_type();
        } else if t.flags().intersects(TypeFlags::NonPrimitive) {
            return self.empty_object_type;
        } else if t.flags().intersects(TypeFlags::Index) {
            return self.string_number_symbol_type;
        } else if t.flags().intersects(TypeFlags::Unknown) && !self.strict_null_checks {
            return self.empty_object_type;
        }
        t
    }

    // checker.go:22126
    fn get_apparent_type_of_mapped_type(&mut self, t: P<Type>) -> P<Type> {
        let m = t.as_mapped_type();
        if m.resolved_apparent_type.get().is_none() {
            let resolved = self.get_resolved_apparent_type_of_mapped_type(t);
            m.resolved_apparent_type.set(Some(resolved));
        }
        m.resolved_apparent_type.get().unwrap()
    }

    // checker.go:22134
    fn get_resolved_apparent_type_of_mapped_type(&mut self, t: P<Type>) -> P<Type> {
        let target = t.as_mapped_type().target.get().unwrap_or(t);
        let type_variable = self.get_homomorphic_type_variable(target);
        if let Some(type_variable) = type_variable
            && target
                .as_mapped_type()
                .declaration
                .get()
                .unwrap()
                .as_mapped_type_node()
                .name_type
                .is_none()
        {
            // We have a homomorphic mapped type or an instantiation of a homomorphic mapped type, i.e. a type
            // of the form { [P in keyof T]: X }. Obtain the modifiers type (the T of the keyof T), and if it is
            // another generic mapped type, recursively obtain its apparent type. Otherwise, obtain its base
            // constraint. Then, if every constituent of the base constraint is an array or tuple type, apply
            // this mapped type to the base constraint. It is safe to recurse when the modifiers type is a
            // mapped type because we protect again circular constraints in getTypeFromMappedTypeNode.
            let modifiers_type = self.get_modifiers_type_from_mapped_type(t);
            let base_constraint = if self.is_generic_mapped_type(modifiers_type) {
                Some(self.get_apparent_type_of_mapped_type(modifiers_type))
            } else {
                self.get_base_constraint_of_type(modifiers_type)
            };
            if let Some(base_constraint) = base_constraint
                && every_type(self, base_constraint, |c, t| {
                    c.is_array_or_tuple_type(t) || c.is_array_or_tuple_or_intersection(t)
                })
            {
                let mapper = prepend_type_mapping(
                    type_variable,
                    base_constraint,
                    t.as_mapped_type().mapper.get(),
                );
                return self.instantiate_type(target, Some(mapper));
            }
        }
        t
    }

    // checker.go:22158
    fn get_apparent_type_of_intersection_type(
        &mut self,
        t: P<Type>,
        this_argument: P<Type>,
    ) -> P<Type> {
        if t == this_argument {
            let d = t.as_intersection_type();
            if d.resolved_apparent_type().is_none() {
                let resolved = self.get_type_with_this_argument(
                    t,
                    Some(this_argument),
                    true, /*needApparentType*/
                );
                d.set_resolved_apparent_type(Some(resolved));
            }
            return d.resolved_apparent_type().unwrap();
        }
        let key = CachedTypeKey { kind: CachedTypeKind::ApparentType, type_id: this_argument.id };

        if let Some(result) = self.cached_types.get(&key).copied() {
            result
        } else {
            let result = self.get_type_with_this_argument(
                t,
                Some(this_argument),
                true, /*needApparentType*/
            );
            self.cached_types.insert(key, result);
            result
        }
    }

    /**
     * Return the reduced form of the given type. For a union type, it is a union of the normalized constituent types.
     * For an intersection of types containing one or more mututally exclusive discriminant properties, it is 'never'.
     * For all other types, it is simply the type itself. Discriminant properties are considered mutually exclusive when
     * no constituent property has type 'never', but the intersection of the constituent property types is 'never'.
     */
    // checker.go:22181
    /// The common no-op cases inline; the rest (a large frame) out of line.
    #[inline]
    pub(crate) fn get_reduced_type(&mut self, t: P<Type>) -> P<Type> {
        let flags = t.flags();
        if !flags.intersects(TypeFlags::UnionOrIntersection)
            || flags.intersects(TypeFlags::Union)
                && !t.object_flags().intersects(ObjectFlags::ContainsIntersections)
        {
            return t;
        }
        // An intersection whose never-reduction is already known (the worker's answer without computing anything).
        if flags.intersects(TypeFlags::Intersection)
            && t.object_flags().intersects(ObjectFlags::IsNeverIntersectionComputed)
        {
            return if t.object_flags().intersects(ObjectFlags::IsNeverIntersection) {
                self.never_type
            } else {
                t
            };
        }
        self.get_reduced_type_worker(t)
    }

    #[inline(never)]
    fn get_reduced_type_worker(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Union) {
            if t.object_flags().intersects(ObjectFlags::ContainsIntersections) {
                if let Some(reduced_type) = t.as_union_type().resolved_reduced_type() {
                    return reduced_type;
                }
                let reduced_type = self.get_reduced_union_type(t);
                t.as_union_type().set_resolved_reduced_type(Some(reduced_type));
                return reduced_type;
            }
        } else if t.flags().intersects(TypeFlags::Intersection) {
            if !t.object_flags_lazy().intersects(ObjectFlags::IsNeverIntersectionComputed) {
                t.object_flags
                    .set(t.object_flags_lazy() | ObjectFlags::IsNeverIntersectionComputed);
                if !self.is_mapping_of_same_object_type(t.types())
                    && self.some_property_reduces_to_never(t)
                {
                    t.object_flags.set(t.object_flags_lazy() | ObjectFlags::IsNeverIntersection);
                }
            }
            if t.object_flags_lazy().intersects(ObjectFlags::IsNeverIntersection) {
                return self.never_type;
            }
        }
        t
    }

    // checker.go:22206
    fn is_mapping_of_same_object_type(&mut self, types: &[P<Type>]) -> bool {
        if !types.is_empty() && types[0].object_flags().intersects(ObjectFlags::Mapped) {
            let first_type = self.get_modifiers_type_from_mapped_type(types[0]);
            if first_type.flags().intersects(TypeFlags::Object) {
                if types[1..].iter().any(|&t| {
                    !t.object_flags().intersects(ObjectFlags::Mapped)
                        || self.get_modifiers_type_from_mapped_type(t) != first_type
                }) {
                    return false;
                }
                return true;
            }
        }
        false
    }

    // checker.go:22220
    fn some_property_reduces_to_never(&mut self, t: P<Type>) -> bool {
        // Collect declaration counts for each property across all constituent types of the intersection.
        // A name that occurs in more than one constituent occurs in one other than skipped, so skipped
        // only needs its members looked up, which may not resolve them.
        let types = t.types();
        let skipped = if self.lazy_members {
            types.iter().position(|&t| {
                if t.object_flags_lazy() & (ObjectFlags::Mapped | ObjectFlags::MembersResolved)
                    == ObjectFlags::Mapped
                {
                    // Mapped types with an `as` clause are not given lazy tables and are resolved
                    // in full on lookup; skipping them only reorders resolution (issue #649).
                    let mapped_type = t.as_mapped_type().target.get().unwrap_or(t);
                    self.get_name_type_from_mapped_type(mapped_type).is_none()
                } else {
                    may_have_lazy_members(t) && t.flags().intersects(TypeFlags::Object)
                }
            })
        } else {
            None
        };
        // tsrs: the counts are kept in a pooled `NameCounts` (Go's ordered map, without its per-name allocation and
        // hashing); every constituent's property list is fetched first, in order, as before.
        let mut counts = self.name_counts_pool.pop().unwrap_or_default();
        counts.lists.clear();
        let mut total = 0;
        for (i, &t) in types.iter().enumerate() {
            if Some(i) != skipped {
                let props = self.get_properties_of_type(t);
                total += props.len();
                counts.lists.push(props);
            }
        }
        counts.count_lists(total);
        if let Some(skipped) = skipped {
            self.lazy_member_stats.some_property_skipped_constituents += 1;
            for i in 0..counts.entries.len() {
                let prop_name = counts.entries[i].0;
                if self
                    .get_property_of_type_ex(
                        types[skipped],
                        prop_name,
                        true,  /*skipObjectFunctionPropertyAugment*/
                        false, /*includeTypeOnlyMembers*/
                    )
                    .is_some()
                {
                    counts.entries[i].2 += 1;
                }
            }
        }
        // Check if any property appears in more than one constituent type and reduces to 'never'.
        // Go in the order the properties were found so the combined properties are created in the same order every time.
        let mut result = false;
        for i in 0..counts.entries.len() {
            let (prop_name, _, count) = counts.entries[i];
            if count > 1
                && let Some(prop) = self.get_property_of_union_or_intersection_type(
                    t, prop_name, true, /*skipObjectFunctionPropertyAugment*/
                )
                && self.is_never_reduced_property(prop)
            {
                result = true;
                break;
            }
        }
        self.name_counts_pool.push(counts);
        result
    }

    // checker.go:22240
    fn get_reduced_union_type(&mut self, union_type: P<Type>) -> P<Type> {
        let types = union_type.types();
        let mut reduced_types: Option<Vec<P<Type>>> = None;
        for (i, &t) in types.iter().enumerate() {
            let mapped = self.get_reduced_type(t);
            match &mut reduced_types {
                Some(v) => v.push(mapped),
                None => {
                    if mapped != t {
                        let mut v = types[..i].to_vec();
                        v.push(mapped);
                        reduced_types = Some(v);
                    }
                }
            }
        }
        let Some(reduced_types) = reduced_types else {
            return union_type;
        };
        let reduced = self.get_union_type(&reduced_types);
        if reduced.flags().intersects(TypeFlags::Union) {
            reduced.as_union_type().set_resolved_reduced_type(Some(reduced));
        }
        reduced
    }

    // checker.go:22252
    fn is_never_reduced_property(&mut self, prop: P<Symbol>) -> bool {
        self.is_discriminant_with_never_type(prop) || is_conflicting_private_property(prop)
    }

    // checker.go:22256
    #[inline]
    pub(crate) fn get_reduced_apparent_type(&mut self, t: P<Type>) -> P<Type> {
        // An object type other than a mapped type is its own reduced and apparent type (most property lookups).
        if t.flags() == TypeFlags::Object && !t.object_flags().intersects(ObjectFlags::Mapped) {
            return t;
        }
        self.get_reduced_apparent_type_worker(t)
    }

    #[inline(never)]
    fn get_reduced_apparent_type_worker(&mut self, t: P<Type>) -> P<Type> {
        // Since getApparentType may return a non-reduced union or intersection type, we need to perform
        // type reduction both before and after obtaining the apparent type. For example, given a type parameter
        // 'T extends A | B', the type 'T & X' becomes 'A & X | B & X' after obtaining the apparent type, and
        // that type may need further reduction to remove empty intersections.
        let reduced = self.get_reduced_type(t);
        let apparent = self.get_apparent_type(reduced);
        self.get_reduced_type(apparent)
    }

    // checker.go:22264
    pub(crate) fn elaborate_never_intersection(
        &mut self,
        chain: Option<P<Diagnostic>>,
        node: P<Node>,
        t: P<Type>,
    ) -> Option<P<Diagnostic>> {
        if t.flags().intersects(TypeFlags::Intersection)
            && t.object_flags_lazy().intersects(ObjectFlags::IsNeverIntersection)
        {
            let mut never_prop: Option<P<Symbol>> = None;
            if let Some(prop) = self
                .get_properties_of_union_or_intersection_type(t)
                .iter()
                .copied()
                .into_iter()
                .find(|&prop| self.is_discriminant_with_never_type(prop))
            {
                never_prop = Some(prop);
            }
            if let Some(never_prop) = never_prop {
                let type_string =
                    self.type_to_string_ex(t, None, TypeFormatFlags::NoTypeReduction, None);
                let prop_string = self.symbol_to_string(never_prop);
                return Some(new_diagnostic_chain_for_node(chain, node, Some(&diagnostics::THE_INTERSECTION_0_WAS_REDUCED_TO_NEVER_BECAUSE_PROPERTY_1_HAS_CONFLICTING_TYPES_IN_SOME_CONSTITUENTS), &[&type_string, &prop_string]));
            }
            let private_prop = self
                .get_properties_of_union_or_intersection_type(t)
                .iter()
                .copied()
                .find(|&p| is_conflicting_private_property(p));
            if let Some(private_prop) = private_prop {
                let type_string =
                    self.type_to_string_ex(t, None, TypeFormatFlags::NoTypeReduction, None);
                let prop_string = self.symbol_to_string(private_prop);
                return Some(new_diagnostic_chain_for_node(chain, node, Some(&diagnostics::THE_INTERSECTION_0_WAS_REDUCED_TO_NEVER_BECAUSE_PROPERTY_1_EXISTS_IN_MULTIPLE_CONSTITUENTS_AND_IS_PRIVATE_IN_SOME), &[&type_string, &prop_string]));
            }
        }
        chain
    }

    // checker.go:22278
    pub(crate) fn is_discriminant_with_never_type(&mut self, prop: P<Symbol>) -> bool {
        // Return true for a synthetic non-optional property with non-uniform types, where at least one is
        // a literal type and none is never, that reduces to never.
        !prop.flags().intersects(SymbolFlags::Optional)
            && (prop.check_flags() & (CheckFlags::NonUniformAndLiteral | CheckFlags::HasNeverType))
                == CheckFlags::NonUniformAndLiteral
            && self.get_type_of_symbol(prop).flags().intersects(TypeFlags::Never)
    }
}

// checker.go:22284
pub(crate) fn is_conflicting_private_property(prop: P<Symbol>) -> bool {
    // Return true for a synthetic property with multiple declarations, at least one of which is private.
    prop.value_declaration().is_none() && prop.check_flags().intersects(CheckFlags::ContainsPrivate)
}

impl Checker {
    // checker.go:22289
    pub(crate) fn get_type_arguments(&mut self, t: P<Type>) -> &'static [P<Type>] {
        let d = t.as_type_reference();
        if d.resolved_type_arguments.get().is_none() {
            let n = d.target.get().unwrap().as_interface_type();
            if !self.push_type_resolution(t.into(), TypeSystemPropertyName::ResolvedTypeArguments) {
                return alloc_vec(vec![self.error_type; n.type_parameters().len()]);
            }
            let mut type_arguments: Vec<P<Type>> = Vec::new();
            let node = t.as_type_reference().node.get();
            if let Some(node) = node {
                match node.kind() {
                    Kind::TypeReference => {
                        type_arguments = n.outer_type_parameters().to_vec();
                        let effective =
                            self.get_effective_type_arguments(node, n.local_type_parameters());
                        type_arguments.extend(effective);
                    }
                    Kind::ArrayType => {
                        type_arguments = vec![
                            self.get_type_from_type_node(node.as_array_type_node().element_type),
                        ];
                    }
                    Kind::TupleType => {
                        type_arguments = node
                            .elements()
                            .iter()
                            .map(|&e| self.get_type_from_type_node(e))
                            .collect();
                    }
                    _ => panic!("Unhandled case in getTypeArguments"),
                }
            }
            if self.pop_type_resolution() {
                if d.resolved_type_arguments.get().is_none() {
                    let instantiated = self.instantiate_types(&type_arguments, d.mapper.get());
                    d.resolved_type_arguments.set(Some(alloc_vec(instantiated)));
                }
            } else {
                if d.resolved_type_arguments.get().is_none() {
                    d.resolved_type_arguments
                        .set(Some(alloc_vec(vec![self.error_type; n.type_parameters().len()])));
                }
                let error_node = if node.is_some() { node } else { self.current_node };
                if let Some(target_symbol) = d.target.get().unwrap().symbol() {
                    let s = self.symbol_to_string(target_symbol);
                    self.error(
                        error_node,
                        &diagnostics::TYPE_ARGUMENTS_FOR_0_CIRCULARLY_REFERENCE_THEMSELVES,
                        &[&s],
                    );
                } else {
                    self.error(
                        error_node,
                        &diagnostics::TUPLE_TYPE_ARGUMENTS_CIRCULARLY_REFERENCE_THEMSELVES,
                        &[],
                    );
                }
            }
        }
        d.resolved_type_arguments.get().unwrap()
    }

    // checker.go:22329
    pub(crate) fn get_effective_type_arguments(
        &mut self,
        node: P<Node>,
        type_parameters: &[P<Type>],
    ) -> Vec<P<Type>> {
        let type_arguments: Vec<P<Type>> =
            node.type_arguments().iter().map(|&n| self.get_type_from_type_node(n)).collect();
        let min_type_argument_count = Self::get_min_type_argument_count(type_parameters);
        self.fill_missing_type_arguments(
            &type_arguments,
            type_parameters,
            min_type_argument_count,
            ast::is_in_js_file(node),
        )
        .into_owned()
    }

    // Gets the minimum number of type arguments needed to satisfy all non-optional type parameters.
    // checker.go:22334
    pub(crate) fn get_min_type_argument_count(type_parameters: &[P<Type>]) -> usize {
        let mut min_type_argument_count = 0;
        min_type_argument_count = type_parameters.iter().enumerate().fold(
            min_type_argument_count,
            |mut min_type_argument_count, (i, &type_parameter)| {
                if !Self::has_type_parameter_default(type_parameter) {
                    min_type_argument_count = i + 1;
                }

                min_type_argument_count
            },
        );
        min_type_argument_count
    }

    // checker.go:22344
    fn has_type_parameter_default(t: P<Type>) -> bool {
        match t.symbol() {
            Some(symbol) => symbol.declarations().iter().any(|&d| {
                is_type_parameter_declaration(d)
                    && d.as_type_parameter_declaration().default_type.is_some()
            }),
            None => false,
        }
    }

    // checker.go:22350
    pub(crate) fn fill_missing_type_arguments<'a>(
        &mut self,
        type_arguments: &'a [P<Type>],
        type_parameters: &[P<Type>],
        min_type_argument_count: usize,
        is_java_script_implicit_any: bool,
    ) -> Cow<'a, [P<Type>]> {
        let _ = min_type_argument_count;
        let num_type_parameters = type_parameters.len();
        if num_type_parameters == 0 {
            return Cow::Borrowed(&[]);
        }
        let num_type_arguments = type_arguments.len();
        if is_java_script_implicit_any || num_type_arguments < num_type_parameters {
            let mut result: Vec<P<Type>> = vec![self.error_type; num_type_parameters];
            // Go copy(result, typeArguments) copies min(len) elements
            let n = num_type_arguments.min(num_type_parameters);
            result[..n].copy_from_slice(&type_arguments[..n]);
            // Map invalid forward references in default types to the error type

            let base_default_type =
                self.get_default_type_argument_type(is_java_script_implicit_any);
            for i in num_type_arguments..num_type_parameters {
                let mut default_type = self.get_default_from_type_parameter(type_parameters[i]);

                if is_java_script_implicit_any
                    && let Some(dt) = default_type
                    && (self.is_type_identical_to(dt, self.unknown_type)
                        || self.is_type_identical_to(dt, self.empty_object_type))
                {
                    default_type = Some(self.any_type);
                }

                if let Some(default_type) = default_type {
                    // Go aliases `result` into the mapper (later writes to result are visible through it); the
                    // arena mapper holds a snapshot instead. See notes/checker-11.md.
                    let mapper =
                        new_type_mapper(alloc_slice(type_parameters), alloc_slice(&result));
                    result[i] = self.instantiate_type(default_type, Some(mapper));
                } else {
                    result[i] = base_default_type;
                }
            }
            return Cow::Owned(result);
        }
        Cow::Borrowed(type_arguments)
    }

    // checker.go:22382
    fn get_default_type_argument_type(&self, is_in_java_script_file: bool) -> P<Type> {
        if is_in_java_script_file {
            return self.any_type;
        }
        self.unknown_type
    }

    // Gets the default type for a type parameter. If the type parameter is the result of an instantiation,
    // this gets the instantiated default type of its target. If the type parameter has no default type or
    // the default is circular, `undefined` is returned.
    // checker.go:22392
    pub(crate) fn get_default_from_type_parameter(&mut self, t: P<Type>) -> Option<P<Type>> {
        if !t.flags().intersects(TypeFlags::TypeParameter) {
            return None;
        }
        let default_type = self.get_resolved_type_parameter_default(t);
        if default_type != self.no_constraint_type && default_type != self.circular_constraint_type
        {
            return Some(default_type);
        }
        None
    }

    // checker.go:22403
    pub(crate) fn get_resolved_type_parameter_default(&mut self, t: P<Type>) -> P<Type> {
        let d = t.as_type_parameter();
        if d.resolved_default_type.get().is_none() {
            if let Some(target) = d.target.get() {
                let target_default = self.get_resolved_type_parameter_default(target);
                let resolved = self.instantiate_type(target_default, d.mapper.get());
                d.resolved_default_type.set(Some(resolved));
            } else {
                // To block recursion, set the initial value to the resolvingDefaultType.
                d.resolved_default_type.set(Some(self.resolving_default_type));
                let mut default_type = self.no_constraint_type;
                if let Some(symbol) = t.symbol() {
                    let default_declaration = symbol.declarations().iter().find_map(|&decl| {
                        if is_type_parameter_declaration(decl) {
                            return decl.as_type_parameter_declaration().default_type;
                        }
                        None
                    });
                    if let Some(default_declaration) = default_declaration {
                        default_type = self.get_type_from_type_node(default_declaration);
                    }
                }
                if d.resolved_default_type.get() == Some(self.resolving_default_type) {
                    // If we have not been called recursively, set the correct default type.
                    d.resolved_default_type.set(Some(default_type));
                }
            }
        } else if d.resolved_default_type.get() == Some(self.resolving_default_type) {
            // If we are called recursively for this type parameter, mark the default as circular.
            d.resolved_default_type.set(Some(self.circular_constraint_type));
        }
        d.resolved_default_type.get().unwrap()
    }

    // checker.go:22440
    pub(crate) fn get_default_or_unknown_from_type_parameter(&mut self, t: P<Type>) -> P<Type> {
        let result = self.get_default_from_type_parameter(t);
        result.unwrap_or(self.unknown_type)
    }

    // checker.go:22445
    pub(crate) fn get_named_members(
        &mut self,
        members: Option<P<SymbolTable>>,
        container: Option<P<Symbol>>,
    ) -> Vec<P<Symbol>> {
        let Some(members) = members.filter(|m| !m.is_empty()) else {
            return Vec::new();
        };
        // For classes and interfaces, we store explicitly declared members ahead of inherited members. This ensures we process
        // explicitly declared members first in type relations, which is beneficial because explicitly declared members are more
        // likely to contain discriminating differences. See for example https://github.com/microsoft/TypeScript/tsc/issues/1968.
        let n = members.len();
        let mut result: Vec<P<Symbol>> = Vec::with_capacity(n);
        let mut contained_count = 0;
        let is_class_or_interface_container = container
            .is_some_and(|c| c.flags().intersects(SymbolFlags::Class | SymbolFlags::Interface));
        if is_class_or_interface_container {
            let container = container.unwrap();
            result.extend((0..n).filter_map(|i| {
                let (id, symbol) = members.entry(i);
                if self.is_named_member(symbol, id)
                    && Self::is_declaration_contained_by(symbol, container)
                {
                    Some(symbol)
                } else {
                    None
                }
            }));
            contained_count = result.len();
        }
        result.extend((0..n).filter_map(|i| {
            let (id, symbol) = members.entry(i);
            if self.is_named_member(symbol, id)
                && (!is_class_or_interface_container
                    || !Self::is_declaration_contained_by(symbol, container.unwrap()))
            {
                Some(symbol)
            } else {
                None
            }
        }));
        let (contained, rest) = result.split_at_mut(contained_count);
        self.sort_symbols(contained);
        self.sort_symbols(rest);
        result
    }

    // checker.go:22472
    pub(crate) fn is_declaration_contained_by(symbol: P<Symbol>, container: P<Symbol>) -> bool {
        if let Some(declaration) = symbol.value_declaration()
            && container.declarations().iter().any(|d| declaration.loc().contained_by(d.loc()))
        {
            return true;
        }
        false
    }

    // checker.go:22483
    pub(crate) fn is_named_member(&mut self, symbol: P<Symbol>, id: &str) -> bool {
        !is_reserved_member_name(id) && self.symbol_is_value(symbol)
    }

    // checker.go:22487
    pub(crate) fn symbol_is_value(&mut self, symbol: P<Symbol>) -> bool {
        self.symbol_is_value_ex(symbol, false /*includeTypeOnlyMembers*/)
    }

    // checker.go:22491
    pub(crate) fn symbol_is_value_ex(
        &mut self,
        symbol: P<Symbol>,
        include_type_only_members: bool,
    ) -> bool {
        symbol.flags().intersects(SymbolFlags::Value)
            || symbol.flags().intersects(SymbolFlags::Alias)
                && self
                    .get_symbol_flags_ex(
                        symbol,
                        !include_type_only_members,
                        false, /*excludeLocalMeanings*/
                    )
                    .intersects(SymbolFlags::Value)
    }

    // checker.go:22496
    pub(crate) fn instantiate_type(&mut self, t: P<Type>, m: Option<P<TypeMapper>>) -> P<Type> {
        self.instantiate_type_with_alias(t, m, None /*alias*/)
    }

    // checker.go:22500
    #[inline]
    pub(crate) fn instantiate_type_with_alias(
        &mut self,
        t: P<Type>,
        m: Option<P<TypeMapper>>,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        // Check for type variables in the alias, so things like `type Brand<T> = number & {}` can potentially be copied with new alias type args, despite them being unreferenced.
        // This is the behavior most people using aliases expect, and prevents the cache from leaking type parameters outside their scope of validity.
        // tests/cases/compiler/declarationEmitArrowFunctionNoRenaming.ts contains an example of this, which previously only worked in strada via some input node reuse logic instead.
        let Some(m) = m else {
            return t;
        };
        // `could_contain_type_variables(t)`'s cached answers, so that every path here returns or tail-calls (the
        // checks that call out are in `instantiate_type_with_alias_slow`, which repeats these).
        if t.flags().intersects(TypeFlags::StructuredOrInstantiable) {
            let object_flags = t.object_flags_lazy();
            if !object_flags.intersects(ObjectFlags::CouldContainTypeVariablesComputed) {
                return self.instantiate_type_with_alias_slow(t, m, alias);
            }
            if object_flags.intersects(ObjectFlags::CouldContainTypeVariables) {
                return self.instantiate_type_with_alias_worker(t, m, alias);
            }
        }
        if t.alias().is_none() {
            return t;
        }
        self.instantiate_type_with_alias_slow(t, m, alias)
    }

    #[inline(never)]
    fn instantiate_type_with_alias_slow(
        &mut self,
        t: P<Type>,
        m: P<TypeMapper>,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        if !(self.could_contain_type_variables(t)
            || (t.alias().is_some()
                && !t.alias().type_arguments().is_empty()
                && t.alias()
                    .type_arguments()
                    .iter()
                    .any(|&a| self.could_contain_type_variables(a))))
        {
            return t;
        }
        self.instantiate_type_with_alias_worker(t, m, alias)
    }

    #[inline(never)]
    fn instantiate_type_with_alias_worker(
        &mut self,
        t: P<Type>,
        m: P<TypeMapper>,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        if self.instantiation_stack.len() == 100 || self.instantiation_count >= 5_000_000 {
            // We have reached 100 recursive type instantiations, or 5M type instantiations caused by the same statement
            // or expression. There is a very high likelihood we're dealing with a combination of infinite generic types
            // that perpetually generate new type identities, so we stop the recursion here by yielding the error type.
            let circular_type_names = self.get_circular_type_names();
            match circular_type_names.len().cmp(&1) {
                std::cmp::Ordering::Equal => {
                    self.error(
                        self.current_node,
                        &diagnostics::INSTANTIATIONS_OF_TYPE_0_APPEAR_INFINITELY_CIRCULAR,
                        &[&circular_type_names[0]],
                    );
                }
                std::cmp::Ordering::Greater => {
                    let names: Vec<&str> =
                        circular_type_names.iter().map(std::string::String::as_str).collect();
                    let quoted = quoted_and_comma_separated(&names);
                    self.error(self.current_node, &diagnostics::INSTANTIATIONS_OF_THE_FOLLOWING_TYPES_APPEAR_INFINITELY_CIRCULAR_COLON_0, &[&quoted]);
                }
                std::cmp::Ordering::Less => {
                    self.error(
                        self.current_node,
                        &diagnostics::TYPE_INSTANTIATION_IS_EXCESSIVELY_DEEP_AND_POSSIBLY_INFINITE,
                        &[],
                    );
                }
            }
            return self.error_type;
        }
        let index = self.find_active_mapper(m);
        // A mapper that was not active gets a cleared cache, and nothing is stored in it for this call (the result is
        // stored only when the mapper was already active), so its lookup can only miss: no key is built for it.
        let key = if let Some(index) = index {
            let key = active_mapper_cache_key(t, alias);
            if let Some(cached_type) = self.active_type_mappers_caches[index].get(&key) {
                crate::sitecount::hit("active mapper cache (instantiateTypeWithAlias)", "hit");
                return cached_type;
            }
            Some(key)
        } else {
            self.push_active_mapper(m);
            debug_assert!(
                self.active_type_mappers_caches
                    .last()
                    .is_some_and(super::packedmap::PackedMap::is_empty)
            );
            None
        };
        crate::sitecount::hit(
            "active mapper cache (instantiateTypeWithAlias)",
            if index.is_none() { "miss (new mapper)" } else { "miss (active mapper)" },
        );
        self.total_instantiation_count += 1;
        self.instantiation_count += 1;
        crate::sitecount::hit("instantiation", type_kind_label(t.flags(), t.object_flags()));
        self.instantiation_stack.push(t);
        let result = self.instantiate_type_worker(t, m, alias);
        match key {
            None => self.pop_active_mapper(),
            Some(key) => {
                self.active_type_mappers_caches[index.unwrap()].insert(key, result);
            }
        }
        self.instantiation_stack.pop();
        result
    }
}

/// The high word of an active mapper cache key without an alias (any constant; see `active_mapper_cache_key`).
const ACTIVE_MAPPER_TYPE_KEY: u64 = 0x7f4a_7c15_9e37_79b9;

/// The key of `t` (instantiated with `alias`) in an active mapper's cache (Go `instantiateTypeWithAlias`).
///
/// Go hashes `write_type` + `write_alias` with xxh3-128 and only compares keys for equality, and these caches are
/// private to this function. Without an alias the key is the type id itself, spread by an odd multiplier (a
/// bijection, so two ids never share a key) under a fixed high word; an xxh3 key of an aliased instantiation
/// matches one of these with the same 2^-128 odds as two xxh3 keys match each other.
#[inline]
fn active_mapper_cache_key(t: P<Type>, alias: Option<P<TypeAlias>>) -> CacheHashKey {
    if alias.is_none() {
        CacheHashKey {
            hi: ACTIVE_MAPPER_TYPE_KEY,
            lo: u64::from(t.id.0).wrapping_mul(0x9e37_79b9_7f4a_7c15),
        }
    } else {
        let mut b = KeyBuilder::default();
        b.write_type(t);
        b.write_alias(alias);
        b.hash()
    }
}

impl Checker {
    // checker.go:22551
    fn get_circular_type_names(&mut self) -> Vec<String> {
        let mut type_counts: FxHashMap<P<Type>, usize> = FxHashMap::default();
        let mut circular_type_names: Vec<String> = Vec::new();
        let stack = self.instantiation_stack.clone();
        for t in stack {
            let count = type_counts.entry(t).or_insert(0);
            *count += 1;
            if *count == 3 {
                let mut symbol = t.symbol();
                if let Some(alias) = t.alias() {
                    symbol = alias.symbol();
                }
                if let Some(symbol) = symbol
                    && !symbol.name().is_empty()
                    && symbol.name().as_bytes()[0] != INTERNAL_SYMBOL_NAME_PREFIX_BYTE
                {
                    let name = self.symbol_to_string_exported(symbol);
                    if !circular_type_names.contains(&name) {
                        circular_type_names.push(name);
                    }
                }
            }
        }
        circular_type_names
    }

    // checker.go:22571
    fn push_active_mapper(&mut self, mapper: P<TypeMapper>) {
        self.active_mappers.push(mapper);
        // Go reuses a cleared map left in the slice capacity by popActiveMapper (here: a pool of cleared maps).
        let cache = self.free_type_mapper_caches.pop().unwrap_or_default();
        self.active_type_mappers_caches.push(cache);
    }

    // checker.go:22586
    fn pop_active_mapper(&mut self) {
        self.active_mappers.pop();
        // Go clears the map and leaves it in the list for later reuse.
        let mut cache = self.active_type_mappers_caches.pop().unwrap();
        if cache.capacity() > 64 && cache.capacity() > 4 * cache.len() {
            // A table one large instantiation grew is not worth clearing (clearing touches every bucket).
            cache = PackedMap::default();
        } else {
            cache.clear();
        }
        self.free_type_mapper_caches.push(cache);
    }

    // checker.go:22596
    fn find_active_mapper(&self, mapper: P<TypeMapper>) -> Option<usize> {
        find_last_index(&self.active_mappers, |&m| m == mapper)
    }

    // checker.go:22600
    pub(crate) fn clear_active_mapper_caches(&mut self) {
        for cache in &mut self.active_type_mappers_caches {
            cache.clear();
        }
    }

    // Return true if the given type could possibly reference a type parameter for which
    // we perform type inference (i.e. a type parameter of a generic function). We cache
    // results for union and intersection types for performance reasons.
    // checker.go:22609
    pub(crate) fn could_contain_type_variables_worker(&mut self, t: P<Type>) -> bool {
        if !t.flags().intersects(TypeFlags::StructuredOrInstantiable) {
            return false;
        }
        let object_flags = t.object_flags_lazy();
        if object_flags.intersects(ObjectFlags::CouldContainTypeVariablesComputed) {
            return object_flags.intersects(ObjectFlags::CouldContainTypeVariables);
        }
        let result = t.flags().intersects(TypeFlags::Instantiable)
            || t.flags().intersects(TypeFlags::Object)
                && !Self::is_non_generic_top_level_type(t)
                && (object_flags.intersects(ObjectFlags::Reference)
                    && (t.as_type_reference().node.get().is_some() || {
                        let type_arguments = self.get_type_arguments(t);
                        type_arguments.iter().any(|&a| self.could_contain_type_variables(a))
                    })
                    || object_flags.intersects(ObjectFlags::Anonymous)
                        && t.symbol().is_some_and(|s| {
                            s.flags().intersects(
                                SymbolFlags::Function
                                    | SymbolFlags::Method
                                    | SymbolFlags::Class
                                    | SymbolFlags::TypeLiteral
                                    | SymbolFlags::ObjectLiteral,
                            ) && !s.declarations().is_empty()
                        })
                    || object_flags.intersects(
                        ObjectFlags::Mapped
                            | ObjectFlags::ReverseMapped
                            | ObjectFlags::ObjectRestType
                            | ObjectFlags::InstantiationExpressionType,
                    ))
            || t.flags().intersects(TypeFlags::UnionOrIntersection)
                && !t.flags().intersects(TypeFlags::EnumLiteral)
                && !Self::is_non_generic_top_level_type(t)
                && t.types().iter().any(|&u| self.could_contain_type_variables(u));
        t.object_flags.set(
            t.object_flags_lazy()
                | ObjectFlags::CouldContainTypeVariablesComputed
                | tsrs_core::if_else(
                    result,
                    ObjectFlags::CouldContainTypeVariables,
                    ObjectFlags::None,
                ),
        );
        result
    }

    // checker.go:22626
    fn is_non_generic_top_level_type(t: P<Type>) -> bool {
        if let Some(alias) = t.alias()
            && alias.type_arguments().is_empty()
        {
            let alias_symbol = alias.symbol().unwrap();
            let mut declaration = get_declaration_of_kind(alias_symbol, Kind::TypeAliasDeclaration);
            if declaration.is_none() {
                declaration = get_declaration_of_kind(alias_symbol, Kind::JSTypeAliasDeclaration);
            }
            return match declaration {
                Some(declaration) => {
                    find_ancestor_or_quit(declaration.parent(), |n| match n.kind() {
                        Kind::SourceFile => FindAncestorResult::True,
                        Kind::ModuleDeclaration => FindAncestorResult::False,
                        _ => FindAncestorResult::Quit,
                    })
                    .is_some()
                }
                None => false,
            };
        }
        false
    }

    // checker.go:22645
    fn instantiate_type_worker(
        &mut self,
        t: P<Type>,
        m: P<TypeMapper>,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        let flags = t.flags();
        if flags.intersects(TypeFlags::TypeParameter) {
            return self.get_mapped_type(t, m);
        } else if flags.intersects(TypeFlags::Object) {
            let object_flags = t.object_flags();
            if object_flags
                .intersects(ObjectFlags::Reference | ObjectFlags::Anonymous | ObjectFlags::Mapped)
            {
                if object_flags.intersects(ObjectFlags::Reference)
                    && t.as_type_reference().node.get().is_none()
                {
                    let resolved_type_arguments =
                        t.as_type_reference().resolved_type_arguments.get().unwrap_or(&[]);
                    // Go core.Same: instantiateList returns the input slice iff no element changed
                    let Some(new_type_arguments) =
                        self.instantiate_types_changed(resolved_type_arguments, m)
                    else {
                        return t;
                    };
                    let result = self.create_normalized_type_reference(
                        t.object_target().unwrap(),
                        &new_type_arguments,
                    );
                    self.free_type_list(new_type_arguments);
                    return result;
                }
                if object_flags.intersects(ObjectFlags::ReverseMapped) {
                    return self.instantiate_reverse_mapped_type(t, m);
                }
                return self.get_object_type_instantiation(t, Some(m), alias);
            }
            return t;
        } else if flags.intersects(TypeFlags::UnionOrIntersection) {
            let mut source = t;
            if t.flags().intersects(TypeFlags::Union)
                && let Some(origin) = t.as_union_type().origin()
                && origin.flags().intersects(TypeFlags::UnionOrIntersection)
            {
                source = origin;
            }
            let types = source.types();
            // Go core.Same: instantiateList returns the input slice iff no element changed
            let changed = self.instantiate_types_changed(types, m);
            if changed.is_none() && alias.symbol() == t.alias().symbol() {
                return t;
            }
            let new_types = changed.as_deref().unwrap_or(types);
            let pending = if alias.is_none() {
                self.instantiate_type_alias_pending(t.alias(), Some(m))
            } else {
                None
            };
            let alias = AliasArg::given_or_pending(alias, pending.as_ref());
            let result = if source.flags().intersects(TypeFlags::Intersection) {
                self.get_intersection_type_ex(new_types, IntersectionFlags::None, alias)
            } else {
                self.get_union_type_ex(
                    new_types,
                    UnionReduction::Literal,
                    alias,
                    None, /*origin*/
                )
            };
            if let Some(changed) = changed {
                self.free_type_list(changed);
            }
            return result;
        } else if flags.intersects(TypeFlags::Index) {
            let target = self.instantiate_type(t.target().unwrap(), Some(m));
            return self.get_index_type(target);
        } else if flags.intersects(TypeFlags::IndexedAccess) {
            let pending = if alias.is_none() {
                self.instantiate_type_alias_pending(t.alias(), Some(m))
            } else {
                None
            };
            let alias = AliasArg::given_or_pending(alias, pending.as_ref());
            let d = t.as_indexed_access_type();
            let object_type = self.instantiate_type(d.object_type().unwrap(), Some(m));
            let index_type = self.instantiate_type(d.index_type().unwrap(), Some(m));
            return self.get_indexed_access_type_ex(
                object_type,
                index_type,
                d.access_flags.get(),
                None, /*accessNode*/
                alias,
            );
        } else if flags.intersects(TypeFlags::TemplateLiteral) {
            let types = self.instantiate_types(t.as_template_literal_type().types(), Some(m));
            return self.get_template_literal_type(t.as_template_literal_type().texts(), &types);
        } else if flags.intersects(TypeFlags::StringMapping) {
            let target =
                self.instantiate_type(t.as_string_mapping_type().target().unwrap(), Some(m));
            return self.get_string_mapping_type(t.symbol().unwrap(), target);
        } else if flags.intersects(TypeFlags::Conditional) {
            if self.lazy_cond_mapper {
                if t.as_conditional_type().mapper.get().is_some() {
                    self.lazy_member_stats.cond_mappers_avoided += 1;
                }
                return self.get_conditional_type_instantiation_ex(
                    t,
                    t.as_conditional_type().mapper.get(),
                    m,
                    false, /*forConstraint*/
                    alias,
                );
            }
            let mapper = Self::combine_type_mappers(t.as_conditional_type().mapper.get(), m);
            return self.get_conditional_type_instantiation(
                t, mapper, false, /*forConstraint*/
                alias,
            );
        } else if flags.intersects(TypeFlags::Substitution) {
            let new_base_type =
                self.instantiate_type(t.as_substitution_type().base_type().unwrap(), Some(m));
            if Self::is_no_infer_type(t) {
                return self.get_no_infer_type(new_base_type);
            }
            let new_constraint = self
                .instantiate_type(t.as_substitution_type().subst_constraint().unwrap(), Some(m));
            // A substitution type originates in the true branch of a conditional type and can be resolved
            // to just the base type in the same cases as the conditional type resolves to its true branch
            // (because the base type is then known to satisfy the constraint).
            if new_base_type.flags().intersects(TypeFlags::TypeVariable)
                && self.is_generic_type(new_constraint)
            {
                return self.get_substitution_type(new_base_type, new_constraint);
            }
            if new_constraint.flags().intersects(TypeFlags::AnyOrUnknown) || {
                let restrictive_base = self.get_restrictive_instantiation(new_base_type);
                let restrictive_constraint = self.get_restrictive_instantiation(new_constraint);
                self.is_type_assignable_to(restrictive_base, restrictive_constraint)
            } {
                return new_base_type;
            }
            if new_base_type.flags().intersects(TypeFlags::TypeVariable) {
                return self.get_substitution_type(new_base_type, new_constraint);
            }
            return self.get_intersection_type(&[new_constraint, new_base_type]);
        }
        t
    }

    // Handles instantiation of the following object types:
    // AnonymousType (ObjectFlagsAnonymous|ObjectFlagsSingleSignatureType)
    // TypeReference with node != nil (ObjectFlagsReference)
    // InstantiationExpressionType (ObjectFlagsInstantiationExpressionType)
    // MappedType (ObjectFlagsMapped)
    // checker.go:22729
    fn get_object_type_instantiation(
        &mut self,
        t: P<Type>,
        m: Option<P<TypeMapper>>,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        let declaration: P<Node> = if t.object_flags().intersects(ObjectFlags::Reference) {
            // Deferred type reference
            t.as_type_reference().node.get().unwrap()
        } else if t.object_flags().intersects(ObjectFlags::InstantiationExpressionType) {
            t.as_instantiation_expression_type().node.get().unwrap()
        } else {
            t.symbol().unwrap().declarations()[0]
        };
        let links = self.type_node_links.get(declaration);
        let target: P<Type> = if t.object_flags().intersects(ObjectFlags::Reference) {
            // Deferred type reference
            links.resolved_type.get().unwrap()
        } else if t.object_flags().intersects(ObjectFlags::Instantiated) {
            t.object_target().unwrap()
        } else {
            t
        };
        let type_parameters: &'static [P<Type>] =
            if let Some(type_parameters) = links.outer_type_parameters() {
                type_parameters
            } else {
                // The first time an anonymous type is instantiated we compute and store a list of the type
                // parameters that are in scope (and therefore potentially referenced). For type literals that
                // aren't the right hand side of a generic type alias declaration we optimize by reducing the
                // set of type parameters to those that are possibly referenced in the literal.
                let mut type_parameters =
                    self.get_outer_type_parameters(declaration, true /*includeThisTypes*/);
                if target.alias().type_arguments().is_empty() {
                    if t.object_flags().intersects(
                        ObjectFlags::Reference | ObjectFlags::InstantiationExpressionType,
                    ) {
                        let mut filtered = Vec::new();
                        filtered.extend(type_parameters.into_iter().filter(|&tp| {
                            self.is_type_parameter_possibly_referenced(tp, declaration)
                        }));
                        type_parameters = filtered;
                    } else if target
                        .symbol()
                        .unwrap()
                        .flags()
                        .intersects(SymbolFlags::Method | SymbolFlags::TypeLiteral)
                    {
                        let mut filtered = Vec::new();
                        let declarations = t.symbol().unwrap().declarations();
                        filtered.extend(type_parameters.into_iter().filter(|&tp| {
                            declarations
                                .iter()
                                .any(|&d| self.is_type_parameter_possibly_referenced(tp, d))
                        }));
                        type_parameters = filtered;
                    }
                }
                let type_parameters = alloc_vec(type_parameters);
                links.set_outer_type_parameters(Some(type_parameters));
                type_parameters
            };
        if type_parameters.is_empty() {
            return t;
        }
        // We are instantiating an anonymous type that has one or more type parameters in scope. Apply the
        // mapper to the type parameters to produce the effective list of type arguments, and compute the
        // instantiation cache key from the type IDs of the type arguments.
        let mut type_arguments = self.free_type_lists.pop().unwrap_or_default();
        type_arguments.extend(
            type_parameters
                .iter()
                .map(|&tp| self.map_type_with_composite_mapper(tp, t.mapper(), m.unwrap())),
        );
        let pending =
            if alias.is_none() { self.instantiate_type_alias_pending(t.alias(), m) } else { None };
        let new_alias = AliasArg::given_or_pending(alias, pending.as_ref());
        // Go `data := target.AsObjectType()`; `data.instantiations` is `self.object_type_instantiations[target]`
        // (the target is a declared anonymous or mapped type or a deferred reference, never an interface or tuple).
        assert!(
            target.try_as_interface_type().is_none(),
            "object type instantiation of an interface target"
        );
        let key = get_type_instantiation_key(
            &type_arguments,
            new_alias,
            t.object_flags().intersects(ObjectFlags::SingleSignatureType),
        );
        // Shared graph: a fork's table for a frozen target holds only what the fork added; the seed's
        // entries are read through.
        let from_seed = self.object_type_instantiations.base_get(&target).and_then(|m| m.get(&key));
        let instantiations =
            self.object_type_instantiations.own.entry(target).or_insert_with(|| {
                let initial_key =
                    get_type_instantiation_key(type_parameters, target.alias().into(), false);
                PackedMap::from_one(initial_key, target)
            });
        let mut result = instantiations.get(&key).or(from_seed);
        if result.is_none() {
            let too_complex_before = self.too_complex_reports;
            let new_alias = new_alias.alias();
            let mut new_mapper = new_type_mapper(type_parameters, alloc_slice(&type_arguments));
            if target.object_flags().intersects(ObjectFlags::SingleSignatureType)
                && let Some(m) = m
            {
                new_mapper = Self::combine_type_mappers(Some(new_mapper), m);
            }
            let r = if target.object_flags().intersects(ObjectFlags::Reference) {
                self.create_deferred_type_reference(
                    t.target().unwrap(),
                    t.as_type_reference().node.get().unwrap(),
                    Some(new_mapper),
                    new_alias,
                )
            } else if target.object_flags().intersects(ObjectFlags::Mapped) {
                self.instantiate_mapped_type(target, new_mapper, new_alias)
            } else {
                self.instantiate_anonymous_type(target, new_mapper, new_alias)
            };
            // tsrs-only: not cached when the instantiation reported TS2590 (`too_complex_since`).
            if !self.too_complex_since(too_complex_before) {
                self.object_type_instantiations.own.get_mut(&target).unwrap().insert(key, r);
            }
            if r.flags().intersects(TypeFlags::ObjectFlagsType)
                && !r.object_flags_lazy().intersects(ObjectFlags::CouldContainTypeVariablesComputed)
            {
                // if `result` is one of the object types we tried to make (it may not be, due to how `instantiateMappedType` works), we can carry forward the type variable containment check from the input type arguments
                let result_could_contain_object_flags =
                    type_arguments.iter().any(|&a| self.could_contain_type_variables(a));
                if !r.object_flags_lazy().intersects(ObjectFlags::CouldContainTypeVariablesComputed)
                {
                    if r.object_flags().intersects(
                        ObjectFlags::Mapped | ObjectFlags::Anonymous | ObjectFlags::Reference,
                    ) {
                        r.object_flags.set(
                            r.object_flags_lazy()
                                | ObjectFlags::CouldContainTypeVariablesComputed
                                | tsrs_core::if_else(
                                    result_could_contain_object_flags,
                                    ObjectFlags::CouldContainTypeVariables,
                                    ObjectFlags::None,
                                ),
                        );
                    } else {
                        // If none of the type arguments for the outer type parameters contain type variables, it follows
                        // that the instantiated type doesn't reference type variables.
                        // Intrinsics have `CouldContainTypeVariablesComputed` pre-set, so this should only cover unions and intersections resulting from `instantiateMappedType`
                        r.object_flags.set(
                            r.object_flags_lazy()
                                | tsrs_core::if_else(
                                    !result_could_contain_object_flags,
                                    ObjectFlags::CouldContainTypeVariablesComputed,
                                    ObjectFlags::None,
                                ),
                        );
                    }
                }
            }
            result = Some(r);
        }
        self.free_type_list(type_arguments);
        result.unwrap()
    }

    // checker.go:22828
    pub(crate) fn is_type_parameter_possibly_referenced(
        &mut self,
        tp: P<Type>,
        node: P<Node>,
    ) -> bool {
        // If the type parameter doesn't have exactly one declaration, if there are intervening statement blocks
        // between the node and the type parameter declaration, if the node contains actual references to the
        // type parameter, or if the node contains type queries that we can't prove couldn't contain references to the type parameter,
        // we consider the type parameter possibly referenced.
        if let Some(tp_symbol) = tp.symbol()
            && tp_symbol.declarations().len() == 1
        {
            let container = tp_symbol.declarations()[0].parent();
            let mut n = Some(node);
            while n != container {
                let Some(nn) = n else {
                    return true;
                };
                if is_block(nn)
                    || is_conditional_type_node(nn)
                        && self.type_parameter_contains_reference(
                            tp,
                            nn.as_conditional_type_node().extends_type,
                        )
                {
                    return true;
                }
                n = nn.parent();
            }
            return self.type_parameter_contains_reference(tp, node);
        }
        true
    }

    // The `containsReference` closure of isTypeParameterPossiblyReferenced (checker.go:22830).
    fn type_parameter_contains_reference(&mut self, tp: P<Type>, node: P<Node>) -> bool {
        match node.kind() {
            Kind::ThisType => {
                return tp.as_type_parameter().is_this_type();
            }
            Kind::TypeReference => {
                // use worker because we're looking for === equality
                if !tp.as_type_parameter().is_this_type()
                    && node.type_arguments().is_empty()
                    && Some(self.get_symbol_from_type_reference(node)) == tp.symbol()
                {
                    return true;
                }
            }
            Kind::TypeQuery => {
                let entity_name = node.as_type_query_node().expr_name;
                let first_identifier = get_first_identifier(entity_name);
                if !is_this_identifier(first_identifier) {
                    let first_identifier_symbol = self.get_resolved_symbol(first_identifier);
                    let tp_declaration = tp.symbol().unwrap().declarations()[0]; // There is exactly one declaration, otherwise `containsReference` is not called
                    let mut tp_scope: Option<P<Node>> = None;
                    if is_type_parameter_declaration(tp_declaration) {
                        tp_scope = tp_declaration.parent(); // Type parameter is a regular type parameter, e.g. foo<T>
                    } else if tp.as_type_parameter().is_this_type() {
                        tp_scope = Some(tp_declaration); // Type parameter is the this type, and its declaration is the class declaration.
                    }
                    if let Some(tp_scope) = tp_scope {
                        return first_identifier_symbol
                            .declarations()
                            .iter()
                            .any(|&d| crate::is_node_descendant_of(Some(d), tp_scope))
                            || node
                                .type_arguments()
                                .iter()
                                .any(|&a| self.type_parameter_contains_reference(tp, a));
                    }
                }
                return true;
            }
            Kind::MethodDeclaration | Kind::MethodSignature => {
                let return_type = node.type_node();
                return return_type.is_none() && node.body().is_some()
                    || node
                        .type_parameters()
                        .iter()
                        .any(|&p| self.type_parameter_contains_reference(tp, p))
                    || node
                        .parameters()
                        .iter()
                        .any(|&p| self.type_parameter_contains_reference(tp, p))
                    || return_type.is_some_and(|r| self.type_parameter_contains_reference(tp, r));
            }
            _ => {}
        }
        node.for_each_child(&mut |child| self.type_parameter_contains_reference(tp, child))
    }

    // checker.go:22883
    fn instantiate_anonymous_type(
        &mut self,
        t: P<Type>,
        m: P<TypeMapper>,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        let mut m = m;
        let mut alias = alias;
        let result = self.new_object_type(
            (t.object_flags_lazy()
                & !(ObjectFlags::CouldContainTypeVariablesComputed
                    | ObjectFlags::CouldContainTypeVariables))
                | ObjectFlags::Instantiated,
            t.symbol(),
        );
        if t.object_flags().intersects(ObjectFlags::Mapped) {
            result.as_mapped_type().declaration.set(t.as_mapped_type().declaration.get());
            // C.f. instantiateSignature
            let orig_type_parameter = self.get_type_parameter_from_mapped_type(t);
            let fresh_type_parameter = self.clone_type_parameter(orig_type_parameter);
            result.as_mapped_type().type_parameter.set(Some(fresh_type_parameter));
            m = Self::combine_type_mappers(
                Some(new_simple_type_mapper(orig_type_parameter, fresh_type_parameter)),
                m,
            );
            fresh_type_parameter.as_type_parameter().mapper.set(Some(m));
        } else if t.object_flags().intersects(ObjectFlags::InstantiationExpressionType) {
            result
                .as_instantiation_expression_type()
                .node
                .set(t.as_instantiation_expression_type().node.get());
        }
        if alias.is_none() {
            alias = self.instantiate_type_alias(t.alias(), Some(m));
        }
        result.set_alias(alias);
        if let Some(alias) = alias
            && !alias.type_arguments().is_empty()
        {
            let propagating = Self::get_propagating_flags_of_types(
                result.alias().unwrap().type_arguments(),
                TypeFlags::None,
            );
            result.object_flags.set(result.object_flags_lazy() | propagating);
        }
        let d = result.as_object_type();
        d.target.set(Some(t));
        d.mapper.set(Some(m));
        result
    }

    // checker.go:22910
    pub(crate) fn get_conditional_type_instantiation(
        &mut self,
        t: P<Type>,
        mapper: P<TypeMapper>,
        for_constraint: bool,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        self.get_conditional_type_instantiation_ex(t, None, mapper, for_constraint, alias)
    }

    // notes/mem-lazy.md L5: the effective mapper is combineTypeMappers(m1, mapper), which is only used to compute the
    // type arguments, so it is applied in place instead of being allocated.
    fn get_conditional_type_instantiation_ex(
        &mut self,
        t: P<Type>,
        m1: Option<P<TypeMapper>>,
        mapper: P<TypeMapper>,
        for_constraint: bool,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        let root = t.as_conditional_type().root.get().unwrap();
        if !root.outer_type_parameters.get().is_empty() {
            // We are instantiating a conditional type that has one or more type parameters in scope. Apply the
            // mapper to the type parameters to produce the effective list of type arguments, and compute the
            // instantiation cache key from the type IDs of the type arguments.
            let outer_type_parameters = root.outer_type_parameters.get();
            let mut type_arguments = self.free_type_lists.pop().unwrap_or_default();
            type_arguments.reserve(outer_type_parameters.len());
            for &tp in outer_type_parameters {
                // With m1, this is what the composite mapper's Map does.
                let mapped = match m1 {
                    Some(m1) => {
                        let t1 = m1.map(self, tp);
                        if t1 == tp {
                            mapper.map(self, tp)
                        } else {
                            self.instantiate_type(t1, Some(mapper))
                        }
                    }
                    None => mapper.map(self, tp),
                };
                type_arguments.push(mapped);
            }
            let key = get_conditional_type_key(&type_arguments, alias, for_constraint);
            let mut result = root.instantiations.get(&key);
            if result.is_none() {
                let too_complex_before = self.too_complex_reports;
                let census_span = self.census_begin(crate::workcensus::Cat::CondInst, || {
                    crate::workcensus::CKey::Root(root)
                });
                let mut census_fan_out: Option<usize> = None;
                let mut census_nevers = 0u64;
                let type_argument_list = tsrs_core::alloc_slice_recycled(&type_arguments);
                let new_mapper = new_type_mapper(outer_type_parameters, type_argument_list);
                let check_type = root.check_type.get().unwrap();
                let mut distribution_type: Option<P<Type>> = None;
                if root.is_distributive.get() {
                    let mapped = self.get_mapped_type(check_type, new_mapper);
                    distribution_type = Some(self.get_reduced_type(mapped));
                }
                // Distributive conditional types are distributed over union types. For example, when the
                // distributive conditional type T extends U ? X : Y is instantiated with A | B for T, the
                // result is (A extends U ? X : Y) | (B extends U ? X : Y).
                let r = match distribution_type {
                    Some(distribution_type)
                        if check_type != distribution_type
                            && distribution_type
                                .flags()
                                .intersects(TypeFlags::Union | TypeFlags::Never) =>
                    {
                        if census_span.is_some() {
                            census_fan_out =
                                Some(if distribution_type.flags().intersects(TypeFlags::Union) {
                                    distribution_type.types().len()
                                } else {
                                    0
                                });
                        }
                        let never_type = self.never_type;
                        self.map_type_with_alias(
                            distribution_type,
                            |c, t| {
                                let m = prepend_type_mapping(check_type, t, Some(new_mapper));
                                let r = c.get_conditional_type(root, Some(m), for_constraint, None);
                                // SAFETY: made here for this one call.
                                unsafe { recycle_mapping(m, false) };
                                if r == never_type {
                                    census_nevers += 1;
                                }
                                r
                            },
                            alias,
                        )
                    }
                    _ => self.get_conditional_type(root, Some(new_mapper), for_constraint, alias),
                };
                if let Some(t) = self.census_end(census_span) {
                    let census = self.census.as_mut().unwrap();
                    let n = census_fan_out.unwrap_or(0);
                    census.record(
                        crate::workcensus::Cat::CondInst,
                        crate::workcensus::CKey::Root(root),
                        t,
                        n as u64,
                        census_nevers,
                        census_fan_out.is_some() as u64,
                    );
                    if census_fan_out.is_some() {
                        let s =
                            census.fanout.entry((root, crate::workcensus::bucket(n))).or_default();
                        s.count += 1;
                        s.a += n as u64;
                        s.b += census_nevers;
                        if t.outer {
                            s.incl_ns += t.incl_ns;
                        }
                        if t.key_outer {
                            s.key_incl_ns += t.incl_ns;
                        }
                        s.self_ns += t.self_ns;
                        census.charge_bookkeeping();
                    }
                }
                // tsrs-only: not cached when the instantiation reported TS2590 (`too_complex_since`).
                if !self.too_complex_since(too_complex_before) {
                    root.instantiations.set(key, r);
                }
                result = Some(r);
                // The mapper and its type list are garbage unless the result kept the mapper (79%, notes/mem-census.md).
                // SAFETY: made above; a single-type mapper does not keep the list, an array mapper keeps it only if
                // it escaped.
                unsafe {
                    if !new_mapper.escaped() {
                        recycle_mapper(new_mapper);
                        tsrs_core::free_slice!(type_argument_list);
                    } else if type_argument_list.len() == 1 {
                        tsrs_core::free_slice!(type_argument_list);
                    }
                }
            }
            self.free_type_list(type_arguments);
            return result.unwrap();
        }
        t
    }

    // checker.go:22943
    pub(crate) fn clone_type_parameter(&mut self, tp: P<Type>) -> P<Type> {
        let result = self.new_type_parameter(tp.symbol());
        result.as_type_parameter().target.set(Some(tp));
        result
    }

    // checker.go:22949
    pub(crate) fn get_homomorphic_type_variable(&mut self, t: P<Type>) -> Option<P<Type>> {
        let constraint_type = self.get_constraint_type_from_mapped_type(t);
        if constraint_type.flags().intersects(TypeFlags::Index) {
            let type_variable = self
                .get_actual_type_variable(constraint_type.as_index_type().target().unwrap())
                .unwrap();
            if type_variable.flags().intersects(TypeFlags::TypeParameter) {
                return Some(type_variable);
            }
        }
        None
    }

    // checker.go:22960
    fn instantiate_mapped_type(
        &mut self,
        t: P<Type>,
        m: P<TypeMapper>,
        alias: Option<P<TypeAlias>>,
    ) -> P<Type> {
        // For a homomorphic mapped type { [P in keyof T]: X }, where T is some type variable, the mapping
        // operation depends on T as follows:
        // * If T is a primitive type no mapping is performed and the result is simply T.
        // * If T is a union type we distribute the mapped type over the union.
        // * If T is an array we map to an array where the element type has been transformed.
        // * If T is a tuple we map to a tuple where the element types have been transformed.
        // * If T is an intersection of array or tuple types we map to an intersection of transformed array or tuple types.
        // * Otherwise we map to an object type where the type of each property has been transformed.
        // For example, when T is instantiated to a union type A | B, we produce { [P in keyof A]: X } |
        // { [P in keyof B]: X }, and when when T is instantiated to a union type A | undefined, we produce
        // { [P in keyof A]: X } | undefined.
        let type_variable = self.get_homomorphic_type_variable(t);
        if let Some(type_variable) = type_variable {
            let mapped_type_variable = self.instantiate_type(type_variable, Some(m));
            if type_variable != mapped_type_variable {
                let reduced = self.get_reduced_type(mapped_type_variable);
                return self.map_type_with_alias(
                    reduced,
                    |c, s| c.instantiate_mapped_type_constituent(t, type_variable, m, s),
                    alias,
                );
            }
        }
        // If the constraint type of the instantiation is the wildcard type, return the wildcard type.
        let constraint_type = self.get_constraint_type_from_mapped_type(t);
        if self.instantiate_type(constraint_type, Some(m)) == self.wildcard_type {
            return self.wildcard_type;
        }
        self.instantiate_anonymous_type(t, m, alias)
    }

    // The `instantiateConstituent` closure of instantiateMappedType (checker.go:22975).
    fn instantiate_mapped_type_constituent(
        &mut self,
        t: P<Type>,
        type_variable: P<Type>,
        m: P<TypeMapper>,
        s: P<Type>,
    ) -> P<Type> {
        let d = t.as_mapped_type();
        if !s.flags().intersects(
            TypeFlags::AnyOrUnknown
                | TypeFlags::InstantiableNonPrimitive
                | TypeFlags::Object
                | TypeFlags::Intersection,
        ) || s == self.wildcard_type
            || self.is_error_type(s)
        {
            return s;
        }
        if d.declaration.get().unwrap().as_mapped_type_node().name_type.is_none() {
            if self.is_array_type(s)
                || s.flags().intersects(TypeFlags::Any)
                    && self
                        .find_resolution_cycle_start_index(
                            type_variable.into(),
                            TypeSystemPropertyName::ResolvedBaseConstraint,
                        )
                        .is_none()
                    && self.has_array_or_type_type_constraint(type_variable)
            {
                return self.instantiate_mapped_array_type(
                    s,
                    t,
                    prepend_type_mapping(type_variable, s, Some(m)),
                );
            }
            if is_tuple_type(s) {
                return self.instantiate_mapped_tuple_type(s, t, type_variable, m);
            }
            if self.is_array_or_tuple_or_intersection(s) {
                let types: Vec<P<Type>> = s
                    .types()
                    .iter()
                    .map(|&u| self.instantiate_mapped_type_constituent(t, type_variable, m, u))
                    .collect();
                return self.get_intersection_type(&types);
            }
        }
        self.instantiate_anonymous_type(t, prepend_type_mapping(type_variable, s, Some(m)), None)
    }

    // checker.go:23005
    fn has_array_or_type_type_constraint(&mut self, type_variable: P<Type>) -> bool {
        let constraint = self.get_constraint_of_type_parameter(type_variable);
        match constraint {
            Some(constraint) => {
                every_type(self, constraint, super::checker::Checker::is_array_or_tuple_type)
            }
            None => false,
        }
    }

    // checker.go:23010
    fn instantiate_mapped_array_type(
        &mut self,
        array_type: P<Type>,
        mapped_type: P<Type>,
        m: P<TypeMapper>,
    ) -> P<Type> {
        let element_type = self.instantiate_mapped_type_template(
            mapped_type,
            self.number_type,
            true, /*isOptional*/
            m,
        );
        if self.is_error_type(element_type) {
            return self.error_type;
        }
        let readonly = get_modified_readonly_state(
            self.is_readonly_array_type(array_type),
            get_mapped_type_modifiers(mapped_type),
        );
        self.create_array_type_ex(element_type, readonly)
    }

    // checker.go:23018
    fn instantiate_mapped_tuple_type(
        &mut self,
        tuple_type: P<Type>,
        mapped_type: P<Type>,
        type_variable: P<Type>,
        m: P<TypeMapper>,
    ) -> P<Type> {
        // We apply the mapped type's template type to each of the fixed part elements. For variadic elements, we
        // apply the mapped type itself to the variadic element type. For other elements in the variable part of the
        // tuple, we surround the element type with an array type and apply the mapped type to that. This ensures
        // that we get sequential property key types for the fixed part of the tuple, and property key type number
        // for the remaining elements. For example
        //
        //   type Keys<T> = { [K in keyof T]: K };
        //   type Foo<T extends any[]> = Keys<[string, string, ...T, string]>; // ["0", "1", ...Keys<T>, number]
        //
        let element_infos = tuple_type.target_tuple_type().element_infos();
        let fixed_length = tuple_type.target_tuple_type().fixed_length();
        let mut fixed_mapper = m;
        if fixed_length != 0 {
            fixed_mapper = prepend_type_mapping(type_variable, tuple_type, Some(m));
        }
        let modifiers = get_mapped_type_modifiers(mapped_type);
        let element_types = self.get_element_types(tuple_type);
        let mut new_element_types: Vec<P<Type>> = Vec::with_capacity(element_types.len());
        let mut new_element_infos: Vec<TupleElementInfo> = element_infos.to_vec();
        for (i, &e) in element_types.iter().enumerate() {
            let flags = element_infos[i].flags;
            let mapped = if i < fixed_length {
                let key = self.get_string_literal_type(&i.to_string());
                self.instantiate_mapped_type_template(
                    mapped_type,
                    key,
                    flags.intersects(ElementFlags::Optional),
                    fixed_mapper,
                )
            } else if flags.intersects(ElementFlags::Variadic) {
                self.instantiate_type(
                    mapped_type,
                    Some(prepend_type_mapping(type_variable, e, Some(m))),
                )
            } else {
                let array_type = self.create_array_type(e);
                let instantiated = self.instantiate_type(
                    mapped_type,
                    Some(prepend_type_mapping(type_variable, array_type, Some(m))),
                );
                self.get_element_type_of_array_type(instantiated).unwrap_or(self.unknown_type)
            };
            if modifiers.intersects(MappedTypeModifiers::IncludeOptional) {
                if flags.intersects(ElementFlags::Required) {
                    new_element_infos[i].flags = ElementFlags::Optional;
                }
            } else if modifiers.intersects(MappedTypeModifiers::ExcludeOptional)
                && flags.intersects(ElementFlags::Optional)
            {
                new_element_infos[i].flags = ElementFlags::Required;
            }
            new_element_types.push(mapped);
        }
        let new_readonly = get_modified_readonly_state(
            tuple_type.target_tuple_type().is_readonly(),
            get_mapped_type_modifiers(mapped_type),
        );
        if new_element_types.contains(&self.error_type) {
            return self.error_type;
        }
        self.create_tuple_type_ex(&new_element_types, &new_element_infos, new_readonly)
    }

    // checker.go:23071
    fn instantiate_mapped_type_template(
        &mut self,
        t: P<Type>,
        key: P<Type>,
        is_optional: bool,
        m: P<TypeMapper>,
    ) -> P<Type> {
        let type_parameter = self.get_type_parameter_from_mapped_type(t);
        let template_mapper = append_type_mapping(Some(m), type_parameter, key);
        let template_type =
            self.get_template_type_from_mapped_type(t.as_mapped_type().target.get().unwrap_or(t));
        let prop_type = self.instantiate_type(template_type, Some(template_mapper));
        let modifiers = get_mapped_type_modifiers(t);
        if self.strict_null_checks
            && modifiers.intersects(MappedTypeModifiers::IncludeOptional)
            && !self.maybe_type_of_kind(prop_type, TypeFlags::Undefined | TypeFlags::Void)
        {
            self.get_optional_type(prop_type, true /*isProperty*/)
        } else if self.strict_null_checks
            && modifiers.intersects(MappedTypeModifiers::ExcludeOptional)
            && is_optional
        {
            self.remove_missing_or_undefined_type(prop_type)
        } else {
            prop_type
        }
    }
}

// checker.go:23085
fn get_modified_readonly_state(state: bool, modifiers: MappedTypeModifiers) -> bool {
    if modifiers.intersects(MappedTypeModifiers::IncludeReadonly) {
        return true;
    } else if modifiers.intersects(MappedTypeModifiers::ExcludeReadonly) {
        return false;
    }
    state
}

impl Checker {
    // checker.go:23095
    pub(crate) fn get_type_parameter_from_mapped_type(&mut self, t: P<Type>) -> P<Type> {
        let m = t.as_mapped_type();
        if m.type_parameter.get().is_none() {
            let symbol = self.get_symbol_of_declaration(
                m.declaration.get().unwrap().as_mapped_type_node().type_parameter,
            );
            let type_parameter = self.get_declared_type_of_type_parameter(symbol);
            m.type_parameter.set(Some(type_parameter));
        }
        m.type_parameter.get().unwrap()
    }

    // checker.go:23103
    pub(crate) fn get_constraint_type_from_mapped_type(&mut self, t: P<Type>) -> P<Type> {
        let m = t.as_mapped_type();
        if m.constraint_type.get().is_none() {
            let type_parameter = self.get_type_parameter_from_mapped_type(t);
            let constraint_type =
                self.get_constraint_of_type_parameter(type_parameter).unwrap_or(self.error_type);
            m.constraint_type.set(Some(constraint_type));
        }
        m.constraint_type.get().unwrap()
    }

    // checker.go:23111
    pub(crate) fn get_name_type_from_mapped_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        let m = t.as_mapped_type();
        let name_type_node = m.declaration.get().unwrap().as_mapped_type_node().name_type?;
        if m.name_type.get().is_none() {
            let name_type = self.get_type_from_type_node(name_type_node);
            let instantiated = self.instantiate_type(name_type, m.mapper.get());
            m.name_type.set(Some(instantiated));
        }
        m.name_type.get()
    }

    // checker.go:23122
    pub(crate) fn get_template_type_from_mapped_type(&mut self, t: P<Type>) -> P<Type> {
        let m = t.as_mapped_type();
        if m.template_type.get().is_none() {
            if let Some(type_node) = m.declaration.get().unwrap().as_mapped_type_node().type_ {
                let declared = self.get_type_from_type_node(type_node);
                let with_optionality = self.add_optionality_ex(
                    declared,
                    true, /*isProperty*/
                    get_mapped_type_modifiers(t).intersects(MappedTypeModifiers::IncludeOptional),
                );
                let template_type = self.instantiate_type(with_optionality, m.mapper.get());
                m.template_type.set(Some(template_type));
            } else {
                m.template_type.set(Some(self.error_type));
            }
        }
        m.template_type.get().unwrap()
    }

    // checker.go:23134
    pub(crate) fn is_mapped_type_with_keyof_constraint_declaration(t: P<Type>) -> bool {
        let constraint_declaration = Self::get_constraint_declaration_for_mapped_type(t).unwrap();
        is_type_operator_node(constraint_declaration)
            && constraint_declaration.as_type_operator_node().operator == Kind::KeyOfKeyword
    }

    // checker.go:23139
    pub(crate) fn get_constraint_declaration_for_mapped_type(t: P<Type>) -> Option<P<Node>> {
        t.as_mapped_type()
            .declaration
            .get()
            .unwrap()
            .as_mapped_type_node()
            .type_parameter
            .as_type_parameter_declaration()
            .constraint
    }

    // checker.go:23143
    pub(crate) fn get_apparent_mapped_type_keys(
        &mut self,
        name_type: P<Type>,
        target_type: P<Type>,
    ) -> P<Type> {
        let modifiers = self.get_modifiers_type_from_mapped_type(target_type);
        let modifiers_type = self.get_apparent_type(modifiers);
        let mut mapped_keys: Vec<P<Type>> = Vec::new();
        self.for_each_mapped_type_property_key_type_and_index_signature_key_type(
            modifiers_type,
            TypeFlags::StringOrNumberLiteralOrUnique,
            false,
            |c, t| {
                let target_mapper = target_type.mapper();
                let type_parameter = c.get_type_parameter_from_mapped_type(target_type);
                let mapper = append_type_mapping(target_mapper, type_parameter, t);
                mapped_keys.push(c.instantiate_type(name_type, Some(mapper)));
            },
        );
        self.get_union_type(&mapped_keys)
    }

    // checker.go:23152
    pub(crate) fn for_each_mapped_type_property_key_type_and_index_signature_key_type(
        &mut self,
        t: P<Type>,
        include: TypeFlags,
        strings_only: bool,
        cb: impl FnMut(&mut Checker, P<Type>),
    ) {
        let mut cb = cb;
        for prop in self.get_properties_of_type(t).iter().copied() {
            let key_type = self.get_literal_type_from_property(prop, include, false);
            cb(self, key_type);
        }
        if t.flags().intersects(TypeFlags::Any) {
            cb(self, self.string_type);
        } else {
            for info in self.get_index_infos_of_type(t) {
                if !strings_only
                    || info
                        .key_type()
                        .flags()
                        .intersects(TypeFlags::String | TypeFlags::TemplateLiteral)
                {
                    cb(self, info.key_type());
                }
            }
        }
    }

    // checker.go:23167
    fn instantiate_reverse_mapped_type(&mut self, t: P<Type>, m: P<TypeMapper>) -> P<Type> {
        let r = t.as_reverse_mapped_type();
        let inner_mapped_type = self.instantiate_type(r.mapped_type.get().unwrap(), Some(m));
        if !inner_mapped_type.object_flags().intersects(ObjectFlags::Mapped) {
            return t;
        }
        let inner_index_type = self.instantiate_type(r.constraint_type.get().unwrap(), Some(m));
        if !inner_index_type.flags().intersects(TypeFlags::Index) {
            return t;
        }
        let source = self.instantiate_type(r.source.get().unwrap(), Some(m));
        let instantiated = self.infer_type_for_homomorphic_mapped_type(
            source,
            inner_mapped_type,
            inner_index_type,
        );
        if let Some(instantiated) = instantiated {
            return instantiated;
        }
        t
        // Nested invocation of `inferTypeForHomomorphicMappedType` or the `source` instantiated into something unmappable
    }

    // checker.go:23185
    pub(crate) fn instantiate_type_alias(
        &mut self,
        alias: Option<P<TypeAlias>>,
        m: Option<P<TypeMapper>>,
    ) -> Option<P<TypeAlias>> {
        let alias = alias?;
        let type_arguments = self.instantiate_types(alias.type_arguments(), m);
        Some(P::new(TypeAlias {
            symbol: Cell::new(alias.symbol()),
            type_arguments: ThinSliceCell::new(alloc_vec(type_arguments)),
        }))
    }

    /// `instantiate_type_alias` for the alias argument of a cached type constructor (`AliasArg::Pending`): the type
    /// arguments are instantiated here, like Go, the `TypeAlias` is allocated only if a type is created with it.
    fn instantiate_type_alias_pending(
        &mut self,
        alias: Option<P<TypeAlias>>,
        m: Option<P<TypeMapper>>,
    ) -> Option<PendingTypeAlias> {
        let alias = alias?;
        let type_arguments = self.instantiate_types(alias.type_arguments(), m);
        Some(PendingTypeAlias::new(alias.symbol(), type_arguments))
    }

    // checker.go:23192
    pub(crate) fn instantiate_types(
        &mut self,
        types: &[P<Type>],
        m: Option<P<TypeMapper>>,
    ) -> Vec<P<Type>> {
        self.instantiate_list(types, m, super::checker::Checker::instantiate_type)
    }

    /// `instantiate_types` for callers that only read the result: `None` when no element changed (where Go's
    /// `instantiateList` returns its input), else the new list in a buffer to hand back with `free_type_list`.
    /// Instantiates the same elements in the same order.
    fn instantiate_types_changed(
        &mut self,
        types: &[P<Type>],
        m: P<TypeMapper>,
    ) -> Option<Vec<P<Type>>> {
        for (i, &t) in types.iter().enumerate() {
            let mapped = self.instantiate_type(t, Some(m));
            if mapped != t {
                let mut result = self.free_type_lists.pop().unwrap_or_default();
                result.reserve(types.len());
                result.extend_from_slice(&types[..i]);
                result.push(mapped);
                result.extend(types[i + 1..].iter().map(|&t| self.instantiate_type(t, Some(m))));
                return Some(result);
            }
        }
        None
    }

    pub(crate) fn free_type_list(&mut self, mut list: Vec<P<Type>>) {
        if list.capacity() <= 256 {
            list.clear();
            self.free_type_lists.push(list);
        }
    }

    // checker.go:23196
    pub(crate) fn instantiate_symbols(
        &mut self,
        symbols: &[P<Symbol>],
        m: P<TypeMapper>,
    ) -> Vec<P<Symbol>> {
        self.instantiate_list(symbols, Some(m), super::checker::Checker::instantiate_symbol)
    }

    // checker.go:23200
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn instantiate_signatures(
        &mut self,
        signatures: &[P<Signature>],
        m: P<TypeMapper>,
    ) -> Vec<P<Signature>> {
        self.instantiate_list(signatures, Some(m), super::checker::Checker::instantiate_signature)
    }

    // checker.go:23204
    pub(crate) fn instantiate_index_infos(
        &mut self,
        index_infos: &[P<IndexInfo>],
        m: P<TypeMapper>,
    ) -> Vec<P<IndexInfo>> {
        self.instantiate_list(index_infos, Some(m), super::checker::Checker::instantiate_index_info)
    }

    // Returns a copy of `values` when nothing changed (Go returns `values` itself); callers that need Go's
    // `core.Same(result, values)` compare the elements, which is equivalent because a new slice is only built
    // when some element changed.
    // checker.go:23208
    fn instantiate_list<T: Copy + Eq + std::hash::Hash>(
        &mut self,
        values: &[T],
        m: Option<P<TypeMapper>>,
        instantiator: impl FnMut(&mut Checker, T, Option<P<TypeMapper>>) -> T,
    ) -> Vec<T> {
        let mut instantiator = instantiator;
        for (i, &value) in values.iter().enumerate() {
            let mapped = instantiator(self, value, m);
            if mapped != value {
                let mut result: Vec<T> = Vec::with_capacity(values.len());
                result.extend_from_slice(&values[..i]);
                result.push(mapped);
                result.extend(values[i + 1..].iter().map(|&v| instantiator(self, v, m)));
                return result;
            }
        }
        values.to_vec()
    }

    // checker.go:23224
    pub(crate) fn try_get_type_from_type_node(&mut self, node: P<Node>) -> Option<P<Type>> {
        let type_node = node.type_node();
        if let Some(type_node) = type_node {
            return Some(self.get_type_from_type_node(type_node));
        }
        None
    }

    // checker.go:23232
    pub(crate) fn get_type_from_type_node(&mut self, node: P<Node>) -> P<Type> {
        let t = self.get_type_from_type_node_worker(node);
        self.get_conditional_flow_type_of_type(t, Some(node))
    }

    // checker.go:23236
    fn get_type_from_type_node_worker(&mut self, node: P<Node>) -> P<Type> {
        match node.kind() {
            Kind::AnyKeyword | Kind::JSDocAllType => self.any_type,
            Kind::JSDocNonNullableType | Kind::ParenthesizedType => {
                self.get_type_from_type_node(node.type_node().unwrap())
            }
            Kind::JSDocNullableType => {
                let t = self.get_type_from_type_node(node.type_node().unwrap());
                if self.strict_null_checks { self.get_nullable_type(t, TypeFlags::Null) } else { t }
            }
            Kind::JSDocVariadicType => {
                let t = self.get_type_from_type_node(node.as_jsdoc_variadic_type().type_);
                self.create_array_type(t)
            }
            Kind::JSDocOptionalType => {
                let t = self.get_type_from_type_node(node.type_node().unwrap());
                self.add_optionality(t)
            }
            Kind::UnknownKeyword => self.unknown_type,
            Kind::StringKeyword => self.string_type,
            Kind::NumberKeyword => self.number_type,
            Kind::BigIntKeyword => self.bigint_type,
            Kind::BooleanKeyword => self.boolean_type,
            Kind::SymbolKeyword => self.es_symbol_type,
            Kind::VoidKeyword => self.void_type,
            Kind::UndefinedKeyword => self.undefined_type,
            Kind::NullKeyword => self.null_type,
            Kind::NeverKeyword => self.never_type,
            Kind::ObjectKeyword => self.non_primitive_type,
            Kind::IntrinsicKeyword => self.intrinsic_marker_type,
            Kind::ThisType | Kind::ThisKeyword => self.get_type_from_this_type_node(node),
            Kind::LiteralType => self.get_type_from_literal_type_node(node),
            Kind::TypeReference | Kind::ExpressionWithTypeArguments => {
                self.get_type_from_type_reference(node)
            }
            Kind::TypePredicate => {
                if node.as_type_predicate_node().asserts_modifier.is_some() {
                    return self.void_type;
                }
                self.boolean_type
            }
            Kind::TypeQuery => self.get_type_from_type_query_node(node),
            Kind::ArrayType | Kind::TupleType => self.get_type_from_array_or_tuple_type_node(node),
            Kind::OptionalType => self.get_type_from_optional_type_node(node),
            Kind::UnionType => self.get_type_from_union_type_node(node),
            Kind::IntersectionType => self.get_type_from_intersection_type_node(node),
            Kind::NamedTupleMember => self.get_type_from_named_tuple_type_node(node),
            Kind::RestType => self.get_type_from_rest_type_node(node),
            Kind::FunctionType | Kind::ConstructorType | Kind::TypeLiteral => {
                self.get_type_from_type_literal_or_function_or_constructor_type_node(node)
            }
            Kind::TypeOperator => self.get_type_from_type_operator_node(node),
            Kind::IndexedAccessType => self.get_type_from_indexed_access_type_node(node),
            Kind::TemplateLiteralType => self.get_type_from_template_type_node(node),
            Kind::MappedType => self.get_type_from_mapped_type_node(node),
            Kind::ConditionalType => self.get_type_from_conditional_type_node(node),
            Kind::InferType => self.get_type_from_infer_type_node(node),
            Kind::ImportType => self.get_type_from_import_type_node(node),
            _ => self.error_type,
        }
    }

    // checker.go:23325
    pub(crate) fn get_type_from_this_type_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let t = self.get_this_type(node);
            links.resolved_type.set(Some(t));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:23333
    fn get_this_type(&mut self, node: P<Node>) -> P<Type> {
        let container = ast::get_this_container(
            node, false, /*includeArrowFunctions*/
            false, /*includeClassComputedPropertyName*/
        );
        if let Some(parent) = container.parent()
            && (is_class_like(parent) || is_interface_declaration(parent))
            && !ast::is_static(container)
            && (!is_constructor_declaration(container)
                || container
                    .body()
                    .is_some_and(|body| crate::is_node_descendant_of(Some(node), body)))
        {
            let symbol = self.get_symbol_of_declaration(parent).unwrap();
            let declared = self.get_declared_type_of_class_or_interface(symbol);
            return declared.as_interface_type().this_type().unwrap_or(self.error_type);
        }
        self.error(Some(node), &diagnostics::A_THIS_TYPE_IS_AVAILABLE_ONLY_IN_A_NON_STATIC_MEMBER_OF_A_CLASS_OR_INTERFACE, &[]);
        self.error_type
    }

    // checker.go:23347
    fn get_type_from_literal_type_node(&mut self, node: P<Node>) -> P<Type> {
        if node.as_literal_type_node().literal.kind() == Kind::NullKeyword {
            return self.null_type;
        }
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let t = self.check_expression(node.as_literal_type_node().literal);
            let regular = self.get_regular_type_of_literal_type(t);
            links.resolved_type.set(Some(regular));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:23358
    pub(crate) fn get_type_from_type_literal_or_function_or_constructor_type_node(
        &mut self,
        node: P<Node>,
    ) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            // Deferred resolution of members is handled by resolveObjectTypeMembers
            let alias = self.get_alias_for_type_node(node);
            let is_empty = match node.symbol() {
                None => true,
                Some(sym) => {
                    self.get_members_of_symbol(sym).map_or(0, |m| m.len()) == 0 && alias.is_none()
                }
            };
            if is_empty {
                links.resolved_type.set(Some(self.empty_type_literal_type));
            } else {
                let t = self.new_object_type(ObjectFlags::Anonymous, node.symbol());
                t.set_alias(alias);
                links.resolved_type.set(Some(t));
            }
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:23374
    pub(crate) fn get_type_from_indexed_access_type_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let object_type =
                self.get_type_from_type_node(node.as_indexed_access_type_node().object_type);
            let index_type =
                self.get_type_from_type_node(node.as_indexed_access_type_node().index_type);
            let potential_alias = self.get_alias_for_type_node(node);
            let t = self.get_indexed_access_type_ex(
                object_type,
                index_type,
                AccessFlags::None,
                Some(node),
                potential_alias.into(),
            );
            links.resolved_type.set(Some(t));
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:23385
    fn get_type_from_type_operator_node(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            let arg_type = node.type_node().unwrap();
            match node.as_type_operator_node().operator {
                Kind::KeyOfKeyword => {
                    let t = self.get_type_from_type_node(arg_type);
                    let index_type = self.get_index_type(t);
                    links.resolved_type.set(Some(index_type));
                }
                Kind::UniqueKeyword => {
                    if arg_type.kind() == Kind::SymbolKeyword {
                        let t = self.get_es_symbol_like_type_for_node(
                            walk_up_parenthesized_types(node.parent()).unwrap(),
                        );
                        links.resolved_type.set(Some(t));
                    } else {
                        links.resolved_type.set(Some(self.error_type));
                    }
                }
                Kind::ReadonlyKeyword => {
                    let t = self.get_type_from_type_node(arg_type);
                    links.resolved_type.set(Some(t));
                }
                _ => panic!("Unhandled case in getTypeFromTypeOperatorNode"),
            }
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:23407
    pub(crate) fn get_es_symbol_like_type_for_node(&mut self, node: P<Node>) -> P<Type> {
        if is_valid_es_symbol_declaration(node) {
            let symbol = self.get_symbol_of_node(node);
            if let Some(symbol) = symbol {
                let unique_type =
                    if let Some(unique_type) = self.unique_es_symbol_types.get(&symbol).copied() {
                        unique_type
                    } else {
                        let mut b = String::new();
                        b.push_str(INTERNAL_SYMBOL_NAME_PREFIX);
                        b.push('@');
                        b.push_str(symbol.name());
                        b.push('@');
                        b.push_str(&get_symbol_id(symbol).0.to_string());
                        let unique_type = self.new_unique_es_symbol_type(symbol, &b);
                        self.unique_es_symbol_types.insert(symbol, unique_type);
                        unique_type
                    };
                return unique_type;
            }
        }
        self.es_symbol_type
    }

    // checker.go:23428
    pub(crate) fn get_type_from_type_reference(&mut self, node: P<Node>) -> P<Type> {
        let links = self.type_node_links.get(node);
        if links.resolved_type.get().is_none() {
            // Cache both the resolved symbol and the resolved type. The resolved symbol is needed when we check the
            // type reference in checkTypeReferenceNode.
            // handle LS queries on the `const` in `x as const` by resolving to the type of `x`
            if crate::is_const_type_reference(node)
                && is_assertion_expression(node.parent().unwrap())
            {
                let t = self.check_expression_cached(node.parent().unwrap().expression().unwrap());
                links.resolved_type.set(Some(t));
            } else if let Some(t) = self.get_intended_type_from_jsdoc_type_reference(node) {
                links.resolved_type.set(Some(t));
            } else {
                let symbol = self.get_symbol_from_type_reference(node);
                let t = self.get_type_reference_type(node, symbol);
                let t = self.get_distributed_type_parameter(node, t);
                links.resolved_type.set(Some(t));
            }
        }
        links.resolved_type.get().unwrap()
    }

    // checker.go:23445
    fn get_distributed_type_parameter(&mut self, node: P<Node>, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::TypeParameter)
            && !t.as_type_parameter().is_distributed.get()
        {
            let mut n = node.parent();
            while let Some(nn) = n {
                if is_statement(nn) {
                    break;
                }
                if is_conditional_type_node(nn) {
                    let check_type_node = nn.as_conditional_type_node().check_type;
                    if is_simple_identifier_type_reference(check_type_node)
                        && Some(self.get_symbol_from_type_reference(check_type_node)) == t.symbol()
                    {
                        // If node is contained in a distributive conditional type for the given type parameter,
                        // return the distributed form of the type parameter.
                        return self.get_distributed_type_from_type_parameter(t);
                    }
                }
                n = nn.parent();
            }
        }
        t
    }

    // checker.go:23460
    fn get_distributed_type_from_type_parameter(&mut self, t: P<Type>) -> P<Type> {
        let tp = t.as_type_parameter();
        if tp.distributed_type.get().is_none() {
            let distributed_type = self.new_type_parameter(t.symbol());
            tp.distributed_type.set(Some(distributed_type));
            distributed_type.as_type_parameter().is_distributed.set(true);
            distributed_type.as_type_parameter().constraint.set(Some(t));
        }
        tp.distributed_type.get().unwrap()
    }
}

// checker.go:23470
pub(crate) fn get_non_distributed_type_parameter(t: P<Type>) -> Option<P<Type>> {
    if t.flags().intersects(TypeFlags::TypeParameter) && t.as_type_parameter().is_distributed.get()
    {
        return t.as_type_parameter().constraint.get();
    }
    Some(t)
}

// checker.go:23477
fn is_simple_identifier_type_reference(node: P<Node>) -> bool {
    is_type_reference_node(node)
        && is_identifier(node.as_type_reference_node().type_name)
        && node.type_argument_list().is_none()
}

impl Checker {
    // checker.go:23481
    pub(crate) fn get_intended_type_from_jsdoc_type_reference(
        &mut self,
        node: P<Node>,
    ) -> Option<P<Type>> {
        if node.flags().intersects(NodeFlags::JSDoc) && is_type_reference_node(node) {
            let type_name = node.as_type_reference_node().type_name;
            if is_identifier(type_name) {
                let type_args = node.type_arguments();
                match type_name.text() {
                    "String" => {
                        self.check_no_type_arguments(node, None);
                        return Some(self.string_type);
                    }
                    "Number" => {
                        self.check_no_type_arguments(node, None);
                        return Some(self.number_type);
                    }
                    "BigInt" => {
                        self.check_no_type_arguments(node, None);
                        return Some(self.bigint_type);
                    }
                    "Boolean" => {
                        self.check_no_type_arguments(node, None);
                        return Some(self.boolean_type);
                    }
                    "Void" => {
                        self.check_no_type_arguments(node, None);
                        return Some(self.void_type);
                    }
                    "Undefined" => {
                        self.check_no_type_arguments(node, None);
                        return Some(self.undefined_type);
                    }
                    "Null" => {
                        self.check_no_type_arguments(node, None);
                        return Some(self.null_type);
                    }
                    "Function" | "function" => {
                        self.check_no_type_arguments(node, None);
                        return Some(self.global_function_type);
                    }
                    "array" => {
                        if type_args.is_empty() && !self.no_implicit_any {
                            return Some(self.any_array_type);
                        }
                    }
                    "promise" => {
                        if type_args.is_empty() && !self.no_implicit_any {
                            return Some(self.create_promise_type(self.any_type));
                        }
                    }
                    "Object" => {
                        if type_args.len() == 2 {
                            if let Some(record_symbol) = self.get_global_record_symbol() {
                                let index_type = self.get_type_from_type_node(type_args[0]);
                                if self.is_valid_index_key_type(index_type) {
                                    let value_type = self.get_type_from_type_node(type_args[1]);
                                    return Some(self.get_type_alias_instantiation(
                                        record_symbol,
                                        &[index_type, value_type],
                                        None,
                                    ));
                                }
                            }
                            return Some(self.any_type);
                        }
                        if !self.no_implicit_any {
                            self.check_no_type_arguments(node, None);
                            return Some(self.any_type);
                        }
                    }
                    _ => {}
                }
            }
        }
        None
    }

    // checker.go:23538
    fn get_symbol_from_type_reference(&mut self, node: P<Node>) -> P<Symbol> {
        let links = self.symbol_node_links.get(node);
        if links.resolved_symbol.get().is_none() {
            // The `const` in a `const` assertion resolves to nothing; resolveName knows not to
            // report an error for it, so no special-casing is needed here.
            let symbol = self.resolve_type_reference_name(
                node,
                SymbolFlags::Type,
                false, /*ignoreErrors*/
            );
            links.resolved_symbol.set(Some(symbol));
        }
        links.resolved_symbol.get().unwrap()
    }

    // checker.go:23548
    pub(crate) fn resolve_type_reference_name(
        &mut self,
        type_reference: P<Node>,
        meaning: SymbolFlags,
        ignore_errors: bool,
    ) -> P<Symbol> {
        let Some(name) = get_type_reference_name(type_reference) else {
            return self.unknown_symbol;
        };
        let symbol = self.resolve_entity_name(
            name,
            meaning,
            ignore_errors,
            false, /*dontResolveAlias*/
            None,  /*location*/
        );
        if let Some(symbol) = symbol
            && symbol != self.unknown_symbol
        {
            return symbol;
        }
        if ignore_errors {
            return self.unknown_symbol;
        }
        self.get_unresolved_symbol_for_entity_name(name)
    }

    // checker.go:23563
    pub(crate) fn get_unresolved_symbol_for_entity_name(&mut self, name: P<Node>) -> P<Symbol> {
        let identifier = match name.kind() {
            Kind::QualifiedName => name.as_qualified_name().right,
            Kind::PropertyAccessExpression => name.name().unwrap(),
            _ => name,
        };
        let text = identifier.text();
        if !text.is_empty() {
            let parent_symbol: Option<P<Symbol>> = match name.kind() {
                Kind::QualifiedName => {
                    Some(self.get_unresolved_symbol_for_entity_name(name.as_qualified_name().left))
                }
                Kind::PropertyAccessExpression => {
                    Some(self.get_unresolved_symbol_for_entity_name(name.expression().unwrap()))
                }
                _ => None,
            };
            let path = match parent_symbol {
                Some(parent_symbol) => get_symbol_path(parent_symbol) + "." + text,
                None => text.to_string(),
            };
            let result = if let Some(result) = self.unresolved_symbols.get(&path).copied() {
                result
            } else {
                let result =
                    self.new_symbol_ex(SymbolFlags::TypeAlias, text, CheckFlags::Unresolved);
                self.unresolved_symbols.insert(path, result);
                result.set_parent(parent_symbol);
                self.type_alias_links.get(result).declared_type.set(Some(self.unresolved_type));
                result
            };
            return result;
        }
        self.unknown_symbol
    }
}

// checker.go:23600
fn get_symbol_path(symbol: P<Symbol>) -> String {
    if let Some(parent) = symbol.parent() {
        return get_symbol_path(parent) + "." + symbol.name();
    }
    symbol.name().to_string()
}

impl Checker {
    // checker.go:23607
    pub(crate) fn get_type_reference_type(&mut self, node: P<Node>, symbol: P<Symbol>) -> P<Type> {
        if symbol == self.unknown_symbol {
            return self.error_type;
        }
        if symbol.flags().intersects(SymbolFlags::Class | SymbolFlags::Interface) {
            return self.get_type_from_class_or_interface_reference(node, symbol);
        }
        if symbol.flags().intersects(SymbolFlags::TypeAlias) {
            return self.get_type_from_type_alias_reference(node, symbol);
        }
        // Get type from reference to named type that cannot be generic (enum or type parameter)
        let res = self.try_get_declared_type_of_symbol(symbol);
        if let Some(res) = res
            && self.check_no_type_arguments(node, Some(symbol))
        {
            return self.get_regular_type_of_literal_type(res);
        }

        // !!! Resolving values as types for JS
        self.error_type
    }

    /**
     * Get type from type-reference that reference to class or interface
     */
    // checker.go:23630
    pub(crate) fn get_type_from_class_or_interface_reference(
        &mut self,
        node: P<Node>,
        symbol: P<Symbol>,
    ) -> P<Type> {
        let merged = self.get_merged_symbol(symbol);
        let t = self.get_declared_type_of_class_or_interface(merged);
        let d = t.as_interface_type();
        let type_parameters = d.local_type_parameters();
        if !type_parameters.is_empty() {
            let num_type_arguments = node.type_arguments().len();
            let min_type_argument_count = Self::get_min_type_argument_count(type_parameters);
            let is_js = ast::is_in_js_file(node);
            let is_js_implicit_any = !self.no_implicit_any && is_js;
            if !is_js_implicit_any
                && (num_type_arguments < min_type_argument_count
                    || num_type_arguments > type_parameters.len())
            {
                let missing_augments_tag = is_js
                    && is_expression_with_type_arguments(node)
                    && !is_jsdoc_augments_tag(node.parent().unwrap());
                let message: &'static Message = if missing_augments_tag {
                    if min_type_argument_count < type_parameters.len() {
                        &diagnostics::EXPECTED_0_1_TYPE_ARGUMENTS_PROVIDE_THESE_WITH_AN_EXTENDS_TAG
                    } else {
                        &diagnostics::EXPECTED_0_TYPE_ARGUMENTS_PROVIDE_THESE_WITH_AN_EXTENDS_TAG
                    }
                } else {
                    if min_type_argument_count < type_parameters.len() {
                        &diagnostics::GENERIC_TYPE_0_REQUIRES_BETWEEN_1_AND_2_TYPE_ARGUMENTS
                    } else {
                        &diagnostics::GENERIC_TYPE_0_REQUIRES_1_TYPE_ARGUMENT_S
                    }
                };
                let type_str = self.type_to_string_ex(
                    t,
                    None, /*enclosingDeclaration*/
                    TypeFormatFlags::WriteArrayAsGenericType,
                    None,
                );
                let num_type_parameters = type_parameters.len();
                self.error(
                    Some(node),
                    message,
                    &[&type_str, &min_type_argument_count, &num_type_parameters],
                );
                if !is_js {
                    // TODO: Adopt same permissive behavior in TS as in JS to reduce follow-on editing experience failures (requires editing fillMissingTypeArguments)
                    return self.error_type;
                }
            }
            if node.kind() == Kind::TypeReference
                && self.is_deferred_type_reference_node(
                    node,
                    num_type_arguments != type_parameters.len(),
                )
            {
                return self.create_deferred_type_reference(
                    t, node, None, /*mapper*/
                    None, /*alias*/
                );
            }
            // In a type reference, the outer type parameters of the referenced class or interface are automatically
            // supplied as type arguments and the type reference only specifies arguments for the local type parameters
            // of the class or interface.
            let node_type_arguments = self.get_type_arguments_from_node(node);
            let local_type_arguments = self.fill_missing_type_arguments(
                &node_type_arguments,
                type_parameters,
                min_type_argument_count,
                is_js,
            );
            let mut type_arguments = d.outer_type_parameters().to_vec();
            type_arguments.extend_from_slice(&local_type_arguments);
            drop(local_type_arguments);
            return self.create_type_reference_ex(t, &type_arguments, ObjectFlags::FromTypeNode);
        }
        if self.check_no_type_arguments(node, Some(symbol)) {
            return t;
        }
        self.error_type
    }

    // checker.go:23677
    pub(crate) fn get_type_arguments_from_node(&mut self, node: P<Node>) -> Vec<P<Type>> {
        node.type_arguments().iter().map(|&n| self.get_type_from_type_node(n)).collect()
    }

    // checker.go:23681
    pub(crate) fn check_no_type_arguments(
        &mut self,
        node: P<Node>,
        symbol: Option<P<Symbol>>,
    ) -> bool {
        if !node.type_arguments().is_empty() {
            let type_name = match symbol {
                Some(symbol) => self.symbol_to_string(symbol),
                None => tsrs_scanner::declaration_name_to_string(Some(
                    node.as_type_reference_node().type_name,
                )),
            };
            self.error(Some(node), &diagnostics::TYPE_0_IS_NOT_GENERIC, &[&type_name]);
            return false;
        }
        true
    }

    // Return true if the given type reference node is directly aliased or if it needs to be deferred
    // because it is possibly contained in a circular chain of eagerly resolved types.
    // checker.go:23697
    pub(crate) fn is_deferred_type_reference_node(
        &mut self,
        node: P<Node>,
        has_default_type_arguments: bool,
    ) -> bool {
        if self.get_alias_symbol_for_type_node(node).is_some() {
            return true;
        }
        if Self::is_resolved_by_type_alias(node) {
            match node.kind() {
                Kind::ArrayType => {
                    return self.may_resolve_type_alias(node.as_array_type_node().element_type);
                }
                Kind::TupleType => {
                    return node.elements().iter().any(|&e| self.may_resolve_type_alias(e));
                }
                Kind::TypeReference => {
                    return has_default_type_arguments
                        || node.type_arguments().iter().any(|&a| self.may_resolve_type_alias(a));
                }
                _ => {}
            }
            panic!("Unhandled case in isDeferredTypeReferenceNode");
        }
        false
    }

    // Return true when the given node is transitively contained in type constructs that eagerly
    // resolve their constituent types. We include SyntaxKind.TypeReference because type arguments
    // of type aliases are eagerly resolved.
    // checker.go:23718
    fn is_resolved_by_type_alias(node: P<Node>) -> bool {
        let parent = node.parent().unwrap();
        match parent.kind() {
            Kind::ParenthesizedType
            | Kind::NamedTupleMember
            | Kind::TypeReference
            | Kind::UnionType
            | Kind::IntersectionType
            | Kind::IndexedAccessType
            | Kind::ConditionalType
            | Kind::TypeOperator
            | Kind::ArrayType
            | Kind::TupleType => Self::is_resolved_by_type_alias(parent),
            Kind::TypeAliasDeclaration | Kind::JSTypeAliasDeclaration => true,
            _ => false,
        }
    }

    // Return true if resolving the given node (i.e. getTypeFromTypeNode) possibly causes resolution
    // of a type alias.
    // checker.go:23732
    fn may_resolve_type_alias(&mut self, node: P<Node>) -> bool {
        match node.kind() {
            Kind::TypeReference => self
                .resolve_type_reference_name(node, SymbolFlags::Type, false)
                .flags()
                .intersects(SymbolFlags::TypeAlias),
            Kind::TypeQuery => true,
            Kind::TypeOperator => {
                node.as_type_operator_node().operator != Kind::UniqueKeyword
                    && self.may_resolve_type_alias(node.type_node().unwrap())
            }
            Kind::ParenthesizedType | Kind::OptionalType | Kind::NamedTupleMember => {
                self.may_resolve_type_alias(node.type_node().unwrap())
            }
            Kind::RestType => {
                let type_node = node.type_node().unwrap();
                type_node.kind() != Kind::ArrayType
                    || self.may_resolve_type_alias(type_node.as_array_type_node().element_type)
            }
            Kind::UnionType => node
                .as_union_type_node()
                .types()
                .nodes()
                .iter()
                .any(|&t| self.may_resolve_type_alias(t)),
            Kind::IntersectionType => node
                .as_intersection_type_node()
                .types()
                .nodes()
                .iter()
                .any(|&t| self.may_resolve_type_alias(t)),
            Kind::IndexedAccessType => {
                self.may_resolve_type_alias(node.as_indexed_access_type_node().object_type)
                    || self.may_resolve_type_alias(node.as_indexed_access_type_node().index_type)
            }
            Kind::ConditionalType => {
                let n = node.as_conditional_type_node();
                self.may_resolve_type_alias(n.check_type)
                    || self.may_resolve_type_alias(n.extends_type)
                    || self.may_resolve_type_alias(n.true_type)
                    || self.may_resolve_type_alias(n.false_type)
            }
            _ => false,
        }
    }

    // checker.go:23757
    pub(crate) fn create_normalized_type_reference(
        &mut self,
        target: P<Type>,
        type_arguments: &[P<Type>],
    ) -> P<Type> {
        if target.object_flags().intersects(ObjectFlags::Tuple) {
            return self.create_normalized_tuple_type(target, type_arguments);
        }
        self.create_type_reference(target, type_arguments)
    }

    // checker.go:23764
    pub(crate) fn create_normalized_tuple_type_ex(
        &mut self,
        target: P<Type>,
        element_types: &[P<Type>],
        object_flags: ObjectFlags,
    ) -> P<Type> {
        let d = target.as_tuple_type();
        if !d.combined_flags.get().intersects(ElementFlags::NonRequired) {
            // No need to normalize when we only have regular required elements
            return self.create_type_reference_ex(target, element_types, object_flags);
        }
        let element_infos = d.element_infos();
        if d.combined_flags.get().intersects(ElementFlags::Variadic)
            && let Some(result) = element_types.iter().enumerate().find_map(|(i, &e)| {
                if i < element_infos.len()
                    && element_infos[i].flags.intersects(ElementFlags::Variadic)
                    && e.flags().intersects(TypeFlags::Never | TypeFlags::Union)
                {
                    // Transform [A, ...(X | Y | Z)] into [A, ...X] | [A, ...Y] | [A, ...Z]
                    let check_types: Vec<P<Type>> = element_types
                        .iter()
                        .enumerate()
                        .map(|(i, &t)| {
                            if i < element_infos.len()
                                && element_infos[i].flags.intersects(ElementFlags::Variadic)
                            {
                                t
                            } else {
                                self.unknown_type
                            }
                        })
                        .collect();
                    if self.check_cross_product_union(
                        &check_types,
                        Self::too_complex_key(&check_types),
                    ) {
                        return Some(
                            self.map_type(e, |c, t| {
                                Some(c.create_normalized_tuple_type_ex(
                                    target,
                                    &replace_element(element_types, i, t),
                                    object_flags,
                                ))
                            })
                            .unwrap(),
                        );
                    }
                }

                None
            })
        {
            return result;
        }
        // We have optional, rest, or variadic elements that may need normalizing. Normalization ensures that all variadic
        // elements are generic and that the tuple type has one of the following layouts, disregarding variadic elements:
        // (1) Zero or more required elements, followed by zero or more optional elements, followed by zero or one rest element.
        // (2) Zero or more required elements, followed by a rest element, followed by zero or more required elements.
        // In either layout, zero or more generic variadic elements may be present at any location.
        // Note that the element types may contain an extra 'this' type argument that we want to ignore during normalization
        // and then just append to the normalized element types.
        let mut n = TupleNormalizer::default();
        if !n.normalize(self, &element_types[..element_infos.len()], element_infos) {
            return self.error_type;
        }
        if element_types.len() > element_infos.len() {
            n.types.push(element_types[element_infos.len()]);
        }
        let tuple_target = self.get_tuple_target_type(&n.infos, d.readonly.get());
        if tuple_target == self.empty_generic_type {
            return self.empty_object_type;
        } else if !n.types.is_empty() {
            return self.create_type_reference_ex(tuple_target, &n.types, object_flags);
        }
        tuple_target
    }

    // checker.go:23812
    pub(crate) fn create_normalized_tuple_type(
        &mut self,
        target: P<Type>,
        element_types: &[P<Type>],
    ) -> P<Type> {
        self.create_normalized_tuple_type_ex(target, element_types, ObjectFlags::None)
    }
}

impl TupleNormalizer {
    // checker.go:23825
    fn normalize(
        &mut self,
        c: &mut Checker,
        element_types: &[P<Type>],
        element_infos: &[TupleElementInfo],
    ) -> bool {
        self.last_required_index = None;
        self.first_rest_index = None;
        self.last_optional_or_rest_index = None;
        for (i, &t) in element_types.iter().enumerate() {
            let info = element_infos[i];
            if info.flags.intersects(ElementFlags::Variadic) {
                if t.flags().intersects(TypeFlags::Any) {
                    self.add(
                        c,
                        t,
                        TupleElementInfo {
                            flags: ElementFlags::Rest,
                            labeled_declaration: info.labeled_declaration,
                        },
                    );
                } else if t.flags().intersects(TypeFlags::InstantiableNonPrimitive)
                    || c.is_generic_mapped_type(t)
                {
                    // Generic variadic elements stay as they are.
                    self.add(c, t, info);
                } else if is_tuple_type(t) {
                    let spread_types = c.get_element_types(t);
                    if spread_types.len() + self.types.len() >= 10_000 {
                        let message = if c.current_node.is_some_and(is_part_of_type_node) {
                            &diagnostics::TYPE_PRODUCES_A_TUPLE_TYPE_THAT_IS_TOO_LARGE_TO_REPRESENT
                        } else {
                            &diagnostics::EXPRESSION_PRODUCES_A_TUPLE_TYPE_THAT_IS_TOO_LARGE_TO_REPRESENT
                        };
                        c.error(c.current_node, message, &[]);
                        return false;
                    }
                    // Spread variadic elements with tuple types into the resulting tuple.
                    let spread_infos = t.target_tuple_type().element_infos();
                    for (j, &s) in spread_types.iter().enumerate() {
                        self.add(c, s, spread_infos[j]);
                    }
                } else {
                    // Treat everything else as an array type and create a rest element.
                    let mut s: Option<P<Type>> = None;
                    if c.is_array_like_type(t) {
                        s = c.get_index_type_of_type(t, c.number_type);
                    }
                    let s = s.unwrap_or(c.error_type);
                    self.add(
                        c,
                        s,
                        TupleElementInfo {
                            flags: ElementFlags::Rest,
                            labeled_declaration: info.labeled_declaration,
                        },
                    );
                }
            } else {
                // Copy other element kinds with no change.
                self.add(c, t, info);
            }
        }
        // Turn optional elements preceding the last required element into required elements
        for i in 0..self.last_required_index.unwrap_or(0) {
            if self.infos[i].flags.intersects(ElementFlags::Optional) {
                self.infos[i].flags = ElementFlags::Required;
            }
        }
        if self
            .first_rest_index
            .zip(self.last_optional_or_rest_index)
            .is_some_and(|(first, last)| first < last)
        {
            let first = self.first_rest_index.unwrap();
            let last = self.last_optional_or_rest_index.unwrap();
            // Turn elements between first rest and last optional/rest into a single rest element
            let mut types: Vec<P<Type>> = Vec::new();
            types = (first..=last).fold(types, |mut types, i| {
                let mut t = self.types[i];
                if self.infos[i].flags.intersects(ElementFlags::Variadic) {
                    t = c.get_indexed_access_type(t, c.number_type);
                }
                types.push(t);

                types
            });
            self.types[first] = c.get_union_type(&types);
            self.types.drain((first + 1)..=last);
            self.infos.drain((first + 1)..=last);
        }
        true
    }

    // checker.go:23891
    fn add(&mut self, c: &mut Checker, t: P<Type>, info: TupleElementInfo) {
        if info.flags.intersects(ElementFlags::Required) {
            self.last_required_index = Some(self.types.len());
        }
        if info.flags.intersects(ElementFlags::Rest) && self.first_rest_index.is_none() {
            self.first_rest_index = Some(self.types.len());
        }
        if info.flags.intersects(ElementFlags::Optional | ElementFlags::Rest) {
            self.last_optional_or_rest_index = Some(self.types.len());
        }
        let t = c.add_optionality_ex(
            t,
            true, /*isProperty*/
            info.flags.intersects(ElementFlags::Optional),
        );
        self.types.push(t);
        self.infos.push(info);
    }
}

// Return count of starting consecutive tuple elements of the given kind(s)
// checker.go:23906
pub(crate) fn get_start_element_count(t: &'static TupleType, flags: ElementFlags) -> usize {
    if let Some(result) = t
        .element_infos()
        .iter()
        .enumerate()
        .find_map(|(i, info)| if info.flags.intersects(flags) { None } else { Some(i) })
    {
        return result;
    }
    t.element_infos().len()
}

// Return count of ending consecutive tuple elements of the given kind(s)
// checker.go:23916
pub(crate) fn get_end_element_count(t: &'static TupleType, flags: ElementFlags) -> usize {
    let element_infos = t.element_infos();

    element_infos.iter().rev().take_while(|info| info.flags.intersects(flags)).count()
}

// checker.go:23925
pub(crate) fn get_total_fixed_element_count(t: &'static TupleType) -> usize {
    t.fixed_length() + get_end_element_count(t, ElementFlags::Fixed)
}

impl Checker {
    // checker.go:23929
    pub(crate) fn get_element_types(&mut self, t: P<Type>) -> &'static [P<Type>] {
        let type_arguments = self.get_type_arguments(t);
        let arity = Self::get_type_reference_arity(t);
        if type_arguments.len() == arity {
            return type_arguments;
        }
        &type_arguments[..arity.min(type_arguments.len())]
    }

    // checker.go:23938
    pub(crate) fn get_type_reference_arity(t: P<Type>) -> usize {
        t.target_interface_type().type_parameters().len()
    }

    // checker.go:23942
    pub(crate) fn is_array_type(&self, t: P<Type>) -> bool {
        t.object_flags().intersects(ObjectFlags::Reference)
            && (t.target() == Some(self.global_array_type)
                || t.target() == Some(self.global_readonly_array_type))
    }

    // checker.go:23946
    pub(crate) fn is_readonly_array_type(&self, t: P<Type>) -> bool {
        t.object_flags().intersects(ObjectFlags::Reference)
            && t.target() == Some(self.global_readonly_array_type)
    }
}

// checker.go:23950
pub(crate) fn is_tuple_type(t: P<Type>) -> bool {
    t.object_flags().intersects(ObjectFlags::Reference)
        && t.target().unwrap().object_flags().intersects(ObjectFlags::Tuple)
}
