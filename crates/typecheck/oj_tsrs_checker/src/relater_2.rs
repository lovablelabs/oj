use crate::*;
use std::fmt::Display;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

// Non-function declarations in relater.go:2580-end (not generated; port by hand in this file):
//   type Relater (relater.go:2580) -- in relater_types.rs

// Go compares *diagnostics.Message by pointer.
fn is_message(message: Option<&'static Message>, target: &'static Message) -> bool {
    message.is_some_and(|m| std::ptr::eq(m, target))
}

// Go's instantiateType returns nil for a nil type.
fn instantiate_type_or_nil(
    c: &mut Checker,
    t: Option<P<Type>>,
    m: Option<P<TypeMapper>>,
) -> Option<P<Type>> {
    t.map(|t| c.instantiate_type(t, m))
}

impl Checker {
    // relater.go:2599
    pub(crate) fn get_relater(&mut self) -> P<Relater> {
        let r = match self.free_relater {
            Some(r) => r,
            None => P::new(Relater::default()),
        };
        self.free_relater = r.next.get();
        r
    }

    // relater.go:2608
    pub(crate) fn put_relater(&mut self, r: P<Relater>) {
        r.maybe_keys_set.borrow_mut().clear();
        r.relation.set(None);
        r.error_node.set(None);
        r.error_chain.set(None);
        *r.related_info.borrow_mut() = Vec::new();
        r.maybe_keys.borrow_mut().clear();
        r.source_stack.borrow_mut().clear();
        r.target_stack.borrow_mut().clear();
        r.maybe_count.set(0);
        r.source_depth.set(0);
        r.target_depth.set(0);
        r.expanding_flags.set(ExpandingFlags::None);
        r.overflow.set(false);
        r.relation_count.set(0);
        r.next.set(self.free_relater);
        self.free_relater = Some(r);
    }
}

/// tsrs-only: the length up to which the maybe stack is searched by a scan instead of through `maybe_keys_set` (Go
/// always uses the set); the set is kept only while the stack is longer.
const MAYBE_STACK_SCAN_MAX: usize = 32;

impl Relater {
    fn rel(&self) -> P<Relation> {
        self.relation.get().unwrap()
    }

    // Go `r.c.isSimpleTypeRelatedTo(source, target, r.relation, core.IfElse(reportErrors, r.reportError, nil))`
    fn is_simple_type_related_to_reporting(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
    ) -> bool {
        let relation = self.rel();
        let mut reporter = |c: &mut Checker, message: &'static Message, args: &[&dyn Display]| {
            self.report_error(c, message, args);
        };
        c.is_simple_type_related_to(
            source,
            target,
            relation,
            if report_errors { Some(&mut reporter) } else { None },
        )
    }

    // relater.go:2621
    fn is_related_to_simple(&self, c: &mut Checker, source: P<Type>, target: P<Type>) -> Ternary {
        self.is_related_to_ex(
            c,
            source,
            target,
            RecursionFlags::Both,
            false, /*reportErrors*/
            None,  /*headMessage*/
            IntersectionState::None,
        )
    }

    // relater.go:2625
    pub(crate) fn is_related_to_worker(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
    ) -> Ternary {
        self.is_related_to_ex(
            c,
            source,
            target,
            RecursionFlags::Both,
            report_errors,
            None,
            IntersectionState::None,
        )
    }

    // relater.go:2629
    fn is_related_to(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        recursion_flags: RecursionFlags,
        report_errors: bool,
    ) -> Ternary {
        self.is_related_to_ex(
            c,
            source,
            target,
            recursion_flags,
            report_errors,
            None,
            IntersectionState::None,
        )
    }

    // relater.go:2633
    pub(crate) fn is_related_to_ex(
        &self,
        c: &mut Checker,
        original_source: P<Type>,
        original_target: P<Type>,
        recursion_flags: RecursionFlags,
        report_errors: bool,
        head_message: Option<&'static Message>,
        intersection_state: IntersectionState,
    ) -> Ternary {
        if original_source == original_target {
            return Ternary::TRUE;
        }
        // Before normalization: if `source` is type an object type, and `target` is primitive,
        // skip all the checks we don't need and just return `isSimpleTypeRelatedTo` result
        if original_source.flags().intersects(TypeFlags::Object)
            && original_target.flags().intersects(TypeFlags::Primitive)
        {
            if self.rel() == c.comparable_relation
                && !original_target.flags().intersects(TypeFlags::Never)
                && c.is_simple_type_related_to(original_target, original_source, self.rel(), None)
                || self.is_simple_type_related_to_reporting(
                    c,
                    original_source,
                    original_target,
                    report_errors,
                )
            {
                return Ternary::TRUE;
            }
            if report_errors {
                self.report_error_results(
                    c,
                    original_source,
                    original_target,
                    original_source,
                    original_target,
                    head_message,
                );
            }
            return Ternary::FALSE;
        }
        // Normalize the source and target types: Turn fresh literal types into regular literal types,
        // turn deferred type references into regular type references, simplify indexed access and
        // conditional types, and resolve substitution types to either the substitution (on the source
        // side) or the type variable (on the target side).
        let source = c.get_normalized_type(original_source, false /*writing*/);
        let mut target = c.get_normalized_type(original_target, true /*writing*/);
        if source == target {
            return Ternary::TRUE;
        }
        if self.rel() == c.identity_relation {
            if source.flags() != target.flags() {
                return Ternary::FALSE;
            }
            if source.flags().intersects(TypeFlags::Singleton) {
                return Ternary::TRUE;
            }
            self.trace_unions_or_intersections_too_large(c, source, target);
            return self.recursive_type_related_to(
                c,
                source,
                target,
                false, /*reportErrors*/
                IntersectionState::None,
                recursion_flags,
            );
        }
        // We fastpath comparing a type parameter to exactly its constraint, as this is _super_ common,
        // and otherwise, for type parameters in large unions, causes us to need to compare the union to itself,
        // as we break down the _target_ union first, _then_ get the source constraint - so for every
        // member of the target, we attempt to find a match in the source. This avoids that in cases where
        // the target is exactly the constraint.
        if source.flags().intersects(TypeFlags::TypeParameter)
            && c.get_constraint_of_type(source) == Some(target)
        {
            return Ternary::TRUE;
        }
        // See if we're relating a definitely non-nullable type to a union that includes null and/or undefined
        // plus a single non-nullable type. If so, remove null and/or undefined from the target type.
        if source.flags().intersects(TypeFlags::DefinitelyNonNullable)
            && target.flags().intersects(TypeFlags::Union)
        {
            let types = target.types();
            let mut candidate: Option<P<Type>> = None;
            if types.len() == 2 && types[0].flags().intersects(TypeFlags::Nullable) {
                candidate = Some(types[1]);
            } else if types.len() == 3
                && types[0].flags().intersects(TypeFlags::Nullable)
                && types[1].flags().intersects(TypeFlags::Nullable)
            {
                candidate = Some(types[2]);
            }
            if let Some(candidate) = candidate
                && !candidate.flags().intersects(TypeFlags::Nullable)
            {
                target = c.get_normalized_type(candidate, true /*writing*/);
                if source == target {
                    return Ternary::TRUE;
                }
            }
        }
        if self.rel() == c.comparable_relation
            && !target.flags().intersects(TypeFlags::Never)
            && c.is_simple_type_related_to(target, source, self.rel(), None)
            || self.is_simple_type_related_to_reporting(c, source, target, report_errors)
        {
            return Ternary::TRUE;
        }
        if source.flags().intersects(TypeFlags::StructuredOrInstantiable)
            || target.flags().intersects(TypeFlags::StructuredOrInstantiable)
        {
            let is_performing_excess_property_checks = !intersection_state
                .intersects(IntersectionState::Target)
                && is_object_literal_type(source)
                && source.object_flags().intersects(ObjectFlags::FreshLiteral);
            if is_performing_excess_property_checks
                && self.has_excess_properties(c, source, target, report_errors)
            {
                if report_errors {
                    self.report_relation_error(
                        c,
                        head_message,
                        source,
                        if original_target.alias().is_some() { original_target } else { target },
                    );
                }
                return Ternary::FALSE;
            }
            let is_performing_common_property_checks = (self.rel() != c.comparable_relation
                || is_unit_type(source))
                && !intersection_state.intersects(IntersectionState::Target)
                && source
                    .flags()
                    .intersects(TypeFlags::Primitive | TypeFlags::Object | TypeFlags::Intersection)
                && source != c.global_object_type
                && target.flags().intersects(TypeFlags::Object | TypeFlags::Intersection)
                && c.is_weak_type(target)
                && (!c.get_properties_of_type(source).is_empty()
                    || c.type_has_call_or_construct_signatures(source));
            let is_comparing_jsx_attributes =
                source.object_flags().intersects(ObjectFlags::JsxAttributes);
            if is_performing_common_property_checks
                && !c.has_common_properties(source, target, is_comparing_jsx_attributes)
            {
                if report_errors {
                    let source_string =
                        c.type_to_string_exported(if original_source.alias().is_some() {
                            original_source
                        } else {
                            source
                        });
                    let target_string =
                        c.type_to_string_exported(if original_target.alias().is_some() {
                            original_target
                        } else {
                            target
                        });
                    let calls = c.get_signatures_of_type(source, SignatureKind::Call);
                    let constructs = c.get_signatures_of_type(source, SignatureKind::Construct);
                    if !calls.is_empty() && {
                        let return_type = c.get_return_type_of_signature(calls[0]);
                        self.is_related_to(
                            c,
                            return_type,
                            target,
                            RecursionFlags::Source,
                            false, /*reportErrors*/
                        ) != Ternary::FALSE
                    } || !constructs.is_empty() && {
                        let return_type = c.get_return_type_of_signature(constructs[0]);
                        self.is_related_to(
                            c,
                            return_type,
                            target,
                            RecursionFlags::Source,
                            false, /*reportErrors*/
                        ) != Ternary::FALSE
                    } {
                        self.report_error(c, &diagnostics::VALUE_OF_TYPE_0_HAS_NO_PROPERTIES_IN_COMMON_WITH_TYPE_1_DID_YOU_MEAN_TO_CALL_IT, &[&source_string, &target_string]);
                    } else {
                        self.report_error(
                            c,
                            &diagnostics::TYPE_0_HAS_NO_PROPERTIES_IN_COMMON_WITH_TYPE_1,
                            &[&source_string, &target_string],
                        );
                    }
                }
                return Ternary::FALSE;
            }
            self.trace_unions_or_intersections_too_large(c, source, target);
            let skip_caching = source.flags().intersects(TypeFlags::Union)
                && source.types().len() < 4
                && !target.flags().intersects(TypeFlags::Union)
                || target.flags().intersects(TypeFlags::Union)
                    && target.types().len() < 4
                    && !source.flags().intersects(TypeFlags::StructuredOrInstantiable);
            let result = if skip_caching {
                self.union_or_intersection_related_to(
                    c,
                    source,
                    target,
                    report_errors,
                    intersection_state,
                )
            } else {
                self.recursive_type_related_to(
                    c,
                    source,
                    target,
                    report_errors,
                    intersection_state,
                    recursion_flags,
                )
            };
            if result != Ternary::FALSE {
                return result;
            }
        }
        if report_errors {
            self.report_error_results(
                c,
                original_source,
                original_target,
                source,
                target,
                head_message,
            );
        }
        Ternary::FALSE
    }

    // relater.go:2747
    fn has_excess_properties(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
    ) -> bool {
        if !is_excess_property_check_target(target)
            || !c.no_implicit_any && target.object_flags().intersects(ObjectFlags::JSLiteral)
        {
            // Disable excess property checks on JS literals to simulate having an implicit "index signature" - but only outside of noImplicitAny
            return false;
        }
        let is_comparing_jsx_attributes =
            source.object_flags().intersects(ObjectFlags::JsxAttributes);
        if (self.rel() == c.assignable_relation || self.rel() == c.comparable_relation)
            && (c.is_type_subset_of(c.global_object_type, target)
                || (!is_comparing_jsx_attributes && c.is_empty_object_type(target)))
        {
            return false;
        }
        let mut reduced_target = target;
        // Go nil is the empty Vec (Distributed() only returns nil for never, never an empty slice otherwise)
        let mut check_types: Vec<P<Type>> = Vec::new();
        if target.flags().intersects(TypeFlags::Union) {
            reduced_target = match c.find_matching_discriminant_type(source, target, |c, s, t| {
                self.is_related_to_simple(c, s, t)
            }) {
                Some(t) => t,
                None => c.filter_primitives_if_contains_non_primitive(target),
            };
            check_types = reduced_target.distributed();
        }
        for prop in c.get_properties_of_type(source).iter().copied() {
            if should_check_as_excess_property(prop, source.symbol())
                && !is_ignored_jsx_property(source, prop)
            {
                if !c.is_known_property(reduced_target, prop.name(), is_comparing_jsx_attributes) {
                    if report_errors {
                        // Report error in terms of object types in the target as those are the only ones
                        // we check in isKnownProperty.
                        let error_target = c
                            .filter_type(reduced_target, |_, t| is_excess_property_check_target(t));
                        // We know *exactly* where things went wrong when comparing the types.
                        // Use this property as the error node as this will be more helpful in
                        // reasoning about what went wrong.
                        let Some(error_node) = self.error_node.get() else {
                            panic!("No errorNode in hasExcessProperties");
                        };
                        if ast::is_jsx_attributes(error_node)
                            || ast::is_jsx_opening_like_element(error_node)
                            || ast::is_jsx_opening_like_element(error_node.parent().unwrap())
                        {
                            // JsxAttributes has an object-literal flag and undergo same type-assignablity check as normal object-literal.
                            // However, using an object-literal error message will be very confusing to the users so we give different a message.
                            if let Some(value_declaration) = prop.value_declaration()
                                && ast::is_jsx_attribute(value_declaration)
                                && get_source_file_of_node(error_node)
                                    == get_source_file_of_node(value_declaration.name())
                            {
                                // Note that extraneous children (as in `<NoChild>extra</NoChild>`) don't pass this check,
                                // since `children` is a Kind.PropertySignature instead of a Kind.JsxAttribute.
                                self.error_node.set(value_declaration.name());
                            }
                            let prop_name = c.symbol_to_string(prop);
                            let suggestion_symbol = c
                                .get_suggested_symbol_for_nonexistent_jsx_attribute(
                                    &prop_name,
                                    error_target,
                                );
                            if let Some(suggestion_symbol) = suggestion_symbol {
                                let error_target_string = c.type_to_string_exported(error_target);
                                let suggestion_string = c.symbol_to_string(suggestion_symbol);
                                self.report_error(c, &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1_DID_YOU_MEAN_2, &[&prop_name, &error_target_string, &suggestion_string]);
                            } else {
                                let error_target_string = c.type_to_string_exported(error_target);
                                self.report_error(
                                    c,
                                    &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1,
                                    &[&prop_name, &error_target_string],
                                );
                            }
                        } else {
                            // use the property's value declaration if the property is assigned inside the literal itself
                            let mut object_literal_declaration: Option<P<Node>> = None;
                            if let Some(symbol) = source.symbol() {
                                object_literal_declaration = symbol.declarations().first().copied();
                            }
                            let mut suggestion = String::new();
                            if let Some(value_declaration) = prop.value_declaration()
                                && ast::is_object_literal_element(value_declaration)
                                && find_ancestor(value_declaration, |d| {
                                    Some(d) == object_literal_declaration
                                })
                                .is_some()
                                && get_source_file_of_node(object_literal_declaration)
                                    == get_source_file_of_node(self.error_node.get())
                            {
                                let name = value_declaration.name();
                                self.error_node.set(name);
                                let name = name.unwrap();
                                if ast::is_identifier(name) {
                                    suggestion = c.get_suggestion_for_nonexistent_property(
                                        name.text(),
                                        error_target,
                                    );
                                }
                            }
                            if suggestion.is_empty() {
                                let prop_string = c.symbol_to_string(prop);
                                let error_target_string = c.type_to_string_exported(error_target);
                                self.report_error(c, &diagnostics::OBJECT_LITERAL_MAY_ONLY_SPECIFY_KNOWN_PROPERTIES_AND_0_DOES_NOT_EXIST_IN_TYPE_1, &[&prop_string, &error_target_string]);
                            } else {
                                let prop_string = c.symbol_to_string(prop);
                                let error_target_string = c.type_to_string_exported(error_target);
                                self.report_error(c, &diagnostics::OBJECT_LITERAL_MAY_ONLY_SPECIFY_KNOWN_PROPERTIES_BUT_0_DOES_NOT_EXIST_IN_TYPE_1_DID_YOU_MEAN_TO_WRITE_2, &[&prop_string, &error_target_string, &suggestion]);
                            }
                        }
                    }
                    return true;
                }
                if !check_types.is_empty() && {
                    let prop_type = c.get_type_of_symbol(prop);
                    let target_prop_type =
                        c.get_type_of_property_in_types(&check_types, prop.name());
                    self.is_related_to(
                        c,
                        prop_type,
                        target_prop_type,
                        RecursionFlags::Both,
                        report_errors,
                    ) == Ternary::FALSE
                } {
                    if report_errors {
                        let prop_string = c.symbol_to_string(prop);
                        self.report_error(
                            c,
                            &diagnostics::TYPES_OF_PROPERTY_0_ARE_INCOMPATIBLE,
                            &[&prop_string],
                        );
                    }
                    return true;
                }
            }
        }
        false
    }
}

impl Checker {
    // relater.go:2829
    fn get_type_of_property_in_types(&mut self, types: &[P<Type>], name: &str) -> P<Type> {
        let mut prop_types: Vec<P<Type>> = Vec::new();
        prop_types.extend(types.iter().map(|&t| self.get_type_of_property_in_type(t, name)));
        self.get_union_type(&prop_types)
    }

    // relater.go:2837
    fn get_type_of_property_in_type(&mut self, t: P<Type>, name: &str) -> P<Type> {
        let t = self.get_apparent_type(t);
        let prop = if t.flags().intersects(TypeFlags::UnionOrIntersection) {
            self.get_property_of_union_or_intersection_type(t, name, false)
        } else {
            self.get_property_of_object_type(t, name)
        };
        if let Some(prop) = prop {
            return self.get_type_of_symbol(prop);
        }
        let index_info = self.get_applicable_index_info_for_name(t, name);
        if let Some(index_info) = index_info {
            return index_info.value_type();
        }
        self.undefined_type
    }
}

// relater.go:2855
// container is nil-able (Go only dereferences it after checking prop.ValueDeclaration).
pub fn should_check_as_excess_property(prop: P<Symbol>, container: Option<P<Symbol>>) -> bool {
    prop.value_declaration().is_some()
        && container.unwrap().value_declaration().is_some()
        && prop.value_declaration().unwrap().parent() == container.unwrap().value_declaration()
}

// relater.go:2859
pub fn is_ignored_jsx_property(source: P<Type>, source_prop: P<Symbol>) -> bool {
    source.object_flags().intersects(ObjectFlags::JsxAttributes)
        && is_hyphenated_jsx_name(source_prop.name())
}

impl Checker {
    // relater.go:2863
    pub(crate) fn is_type_subset_of(&mut self, source: P<Type>, target: P<Type>) -> bool {
        source == target
            || source.flags().intersects(TypeFlags::Never)
            || target.flags().intersects(TypeFlags::Union)
                && self.is_type_subset_of_union(source, target)
    }

    // relater.go:2867
    fn is_type_subset_of_union(&mut self, source: P<Type>, target: P<Type>) -> bool {
        if source.flags().intersects(TypeFlags::Union) {
            let (source_types, mut target_types) = (source.types(), target.types());
            if source_types.len() > target_types.len() {
                return false;
            }
            // Scan by identity unless a few source types are looked for in a large target. A CompareTypes call costs
            // roughly 8 pointer comparisons, so there a binary search is cheaper than walking the target.
            if source_types.len() * target_types.len().bit_width() as usize * 8
                >= target_types.len()
            {
                return crate::checker_13::is_sorted_subset(source_types, target_types);
            }
            for &t in source_types {
                let (i, found) =
                    tsrs_core::goslices::binary_search_func(target_types, &t, |&probe, &t| {
                        compare_types(self, Some(probe), Some(t))
                    });
                if !found {
                    return false;
                }
                // Union types are sorted by CompareTypes, so the next source type can only appear after this one.
                target_types = &target_types[i + 1..];
            }
            return true;
        }
        if source.flags().intersects(TypeFlags::EnumLike)
            && self.get_base_type_of_enum_like_type(source) == target
        {
            return true;
        }
        contains_type(self, target.types(), source)
    }
}

impl Relater {
    // relater.go:2882
    fn union_or_intersection_related_to(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        // Note that these checks are specifically ordered to produce correct results. In particular,
        // we need to deconstruct unions before intersections (because unions are always at the top),
        // and we need to handle "each" relations before "some" relations for the same kind of type.
        if source.flags().intersects(TypeFlags::Union) {
            if target.flags().intersects(TypeFlags::Union) {
                // Intersections of union types are normalized into unions of intersection types, and such normalized
                // unions can get very large and expensive to relate. The following fast path checks if the source union
                // originated in an intersection. If so, and if that intersection contains the target type, then we know
                // the result to be true (for any two types A and B, A & B is related to both A and B).
                let source_origin = source.as_union_type().origin();
                if let Some(source_origin) = source_origin
                    && source_origin.flags().intersects(TypeFlags::Intersection)
                    && target.alias().is_some()
                    && source_origin.types().contains(&target)
                {
                    return Ternary::TRUE;
                }
                // Similarly, in unions of unions the we preserve the original list of unions. This original list is often
                // much shorter than the normalized result, so we scan it in the following fast path.
                let target_origin = target.as_union_type().origin();
                if let Some(target_origin) = target_origin
                    && target_origin.flags().intersects(TypeFlags::Union)
                    && source.alias().is_some()
                    && target_origin.types().contains(&source)
                {
                    return Ternary::TRUE;
                }
            }
            if self.rel() == c.comparable_relation {
                return self.some_type_related_to_type(
                    c,
                    source,
                    target,
                    report_errors && !source.flags().intersects(TypeFlags::Primitive),
                    intersection_state,
                );
            }
            return self.each_type_related_to_type(
                c,
                source,
                target,
                report_errors && !source.flags().intersects(TypeFlags::Primitive),
                intersection_state,
            );
        }
        if target.flags().intersects(TypeFlags::Union) {
            let regular_source = c.get_regular_type_of_object_literal(source);
            return self.type_related_to_some_type(
                c,
                regular_source,
                target,
                report_errors
                    && !source.flags().intersects(TypeFlags::Primitive)
                    && !target.flags().intersects(TypeFlags::Primitive),
                intersection_state,
            );
        }
        if target.flags().intersects(TypeFlags::Intersection) {
            return self.type_related_to_each_type(
                c,
                source,
                target,
                report_errors,
                IntersectionState::Target,
            );
        }
        // Source is an intersection. For the comparable relation, if the target is a primitive type we hoist the
        // constraints of all non-primitive types in the source into a new intersection. We do this because the
        // intersection may further constrain the constraints of the non-primitive types. For example, given a type
        // parameter 'T extends 1 | 2', the intersection 'T & 1' should be reduced to '1' such that it doesn't
        // appear to be comparable to '2'.
        let mut source = source;
        if self.rel() == c.comparable_relation && target.flags().intersects(TypeFlags::Primitive) {
            let source_types = source.types();
            let mut changed = false;
            let mut constraints: Vec<P<Type>> = Vec::with_capacity(source_types.len());
            constraints.extend(source_types.iter().map(|&t| {
                let mapped = if t.flags().intersects(TypeFlags::Instantiable) {
                    let constraint = c.get_base_constraint_of_type(t);
                    match constraint {
                        Some(constraint) => constraint,
                        None => c.unknown_type,
                    }
                } else {
                    t
                };
                if mapped != t {
                    changed = true;
                }
                mapped
            }));
            if changed {
                source = c.get_intersection_type(&constraints);
                if source.flags().intersects(TypeFlags::Never) {
                    return Ternary::FALSE;
                }
                if !source.flags().intersects(TypeFlags::Intersection) {
                    let result = self.is_related_to(
                        c,
                        source,
                        target,
                        RecursionFlags::Source,
                        false, /*reportErrors*/
                    );
                    if result != Ternary::FALSE {
                        return result;
                    }
                    return self.is_related_to(
                        c,
                        target,
                        source,
                        RecursionFlags::Source,
                        false, /*reportErrors*/
                    );
                }
            }
        }
        // Check to see if any constituents of the intersection are immediately related to the target.
        // Don't report errors though. Elaborating on whether a source constituent is related to the target is
        // not actually useful and leads to some confusing error messages. Instead, we rely on the caller
        // checking whether the full intersection viewed as an object is related to the target.
        self.some_type_related_to_type(
            c,
            source,
            target,
            false, /*reportErrors*/
            IntersectionState::Source,
        )
    }

    // relater.go:2951
    fn some_type_related_to_type(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        let source_types = source.types();
        if source.flags().intersects(TypeFlags::Union) && contains_type(c, source_types, target) {
            return Ternary::TRUE;
        }
        if let Some(result) = source_types.iter().enumerate().find_map(|(i, &t)| {
            let related = self.is_related_to_ex(
                c,
                t,
                target,
                RecursionFlags::Source,
                report_errors && i == source_types.len() - 1,
                None, /*headMessage*/
                intersection_state,
            );
            if related != Ternary::FALSE {
                return Some(related);
            }

            None
        }) {
            return result;
        }
        Ternary::FALSE
    }

    // relater.go:2965
    fn each_type_related_to_type(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        let mut result = Ternary::TRUE;
        let source_types = source.types();
        // We strip `undefined` from the target if the `source` trivially doesn't contain it for our correspondence-checking fastpath
        // since `undefined` is frequently added by optionality and would otherwise spoil a potentially useful correspondence
        let stripped_target = Self::get_undefined_stripped_target_if_needed(c, source, target);
        let mut stripped_types: &[P<Type>] = &[];
        if stripped_target.flags().intersects(TypeFlags::Union) {
            stripped_types = stripped_target.types();
        }
        result = match source_types.iter().enumerate().try_fold(
            result,
            |mut result, (i, &source_type)| {
                if stripped_target.flags().intersects(TypeFlags::Union)
                    && source_types.len() >= stripped_types.len()
                    && source_types.len().is_multiple_of(stripped_types.len())
                {
                    // many unions are mappings of one another; in such cases, simply comparing members at the same index can shortcut the comparison
                    // such unions will have identical lengths, and their corresponding elements will match up. Another common scenario is where a large
                    // union has a union of objects intersected with it. In such cases, if the input was, eg `("a" | "b" | "c") & (string | boolean | {} | {whatever})`,
                    // the result will have the structure `"a" | "b" | "c" | "a" & {} | "b" & {} | "c" & {} | "a" & {whatever} | "b" & {whatever} | "c" & {whatever}`
                    // - the resulting union has a length which is a multiple of the original union, and the elements correspond modulo the length of the original union
                    let related = self.is_related_to_ex(
                        c,
                        source_type,
                        stripped_types[i % stripped_types.len()],
                        RecursionFlags::Both,
                        false, /*reportErrors*/
                        None,  /*headMessage*/
                        intersection_state,
                    );
                    if related != Ternary::FALSE {
                        result &= related;
                        return Ok(result);
                    }
                }
                let related = self.is_related_to_ex(
                    c,
                    source_type,
                    target,
                    RecursionFlags::Source,
                    report_errors,
                    None, /*headMessage*/
                    intersection_state,
                );
                if related == Ternary::FALSE {
                    return Err(Ternary::FALSE);
                }
                result &= related;
                Ok(result)
            },
        ) {
            Ok(value) => value,
            Err(value) => return value,
        };
        result
    }

    // relater.go:2997
    fn get_undefined_stripped_target_if_needed(
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
    ) -> P<Type> {
        if source.flags().intersects(TypeFlags::Union)
            && target.flags().intersects(TypeFlags::Union)
            && !source.types()[0].flags().intersects(TypeFlags::Undefined)
            && target.types()[0].flags().intersects(TypeFlags::Undefined)
        {
            return c.extract_types_of_kind(target, !TypeFlags::Undefined);
        }
        target
    }

    // relater.go:3004
    #[cfg_attr(
        not(feature = "work-census"),
        inline(always),
        expect(
            clippy::inline_always,
            reason = "without the census the wrapper is a forwarding call; inlined, callers call the body as before (notes/perf-checker-algorithms.md)"
        )
    )]
    fn type_related_to_some_type(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        if !c.census_on() {
            return self.type_related_to_some_type_worker(
                c,
                source,
                target,
                report_errors,
                intersection_state,
                &mut (0, 0, 0),
            );
        }
        let span = c.census_begin(crate::workcensus::Cat::RelUnion, || {
            let (sym, label) = crate::workcensus::type_identity(target);
            crate::workcensus::CKey::Rel(0, sym, label)
        });
        let mut outcome = (0u8, 0u8, 0u32);
        let r = self.type_related_to_some_type_worker(
            c,
            source,
            target,
            report_errors,
            intersection_state,
            &mut outcome,
        );
        let timing = c.census_end(span).unwrap();
        let k = crate::workcensus::rel_kind(c, self.rel());
        let (sym, label) = crate::workcensus::type_identity(target);
        let n = target.types().len();
        let source_kind = if is_object_literal_type(source)
            && source.object_flags().intersects(ObjectFlags::FreshLiteral)
        {
            0
        } else if is_object_literal_type(source) {
            1
        } else if source.flags().intersects(TypeFlags::Object) {
            2
        } else if source.flags().intersects(TypeFlags::Intersection) {
            3
        } else if source.flags().intersects(TypeFlags::Primitive) {
            4
        } else {
            5
        };
        let key_state = if !target.flags().intersects(TypeFlags::Union)
            || target.as_union_type().key_property_name().is_empty()
        {
            0
        } else if target.as_union_type().key_property_name() == INTERNAL_SYMBOL_NAME_MISSING {
            1
        } else {
            outcome.1
        };
        let exit = match outcome.0 {
            0 => 0,
            1 => 1,
            2 => 2,
            _ => {
                if r == Ternary::FALSE {
                    6
                } else if outcome.2 <= 1 {
                    3
                } else if (outcome.2 as usize) * 10 <= n.max(10) {
                    4
                } else {
                    5
                }
            }
        };
        let census = c.census.as_mut().unwrap();
        census.record(
            crate::workcensus::Cat::RelUnion,
            crate::workcensus::CKey::Rel(k, sym, label),
            timing,
            outcome.2 as u64,
            (outcome.0 == 2) as u64,
            (r != Ternary::FALSE) as u64,
        );
        let s = census.rel_union.entry((source_kind, key_state, exit)).or_default();
        s.count += 1;
        s.a += outcome.2 as u64;
        if timing.key_outer {
            s.key_incl_ns += timing.incl_ns;
        }
        census.charge_bookkeeping();
        r
    }

    /// `outcome`: (exit path: 0 contains, 1 primitive union, 2 key match, 3 loop; key map state; constituents tried).
    #[cfg_attr(
        not(feature = "work-census"),
        inline(always),
        expect(
            clippy::inline_always,
            reason = "its only call without the census; inlined, the outcome stores disappear"
        )
    )]
    fn type_related_to_some_type_worker(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
        intersection_state: IntersectionState,
        outcome: &mut (u8, u8, u32),
    ) -> Ternary {
        let target_types = target.types();
        if target.flags().intersects(TypeFlags::Union) {
            if contains_type(c, target_types, source) {
                outcome.0 = 0;
                return Ternary::TRUE;
            }
            if self.rel() != c.comparable_relation
                && target.object_flags().intersects(ObjectFlags::PrimitiveUnion)
                && !source.flags().intersects(TypeFlags::EnumLiteral)
                && (source.flags().intersects(
                    TypeFlags::StringLiteral | TypeFlags::BooleanLiteral | TypeFlags::BigIntLiteral,
                ) || (self.rel() == c.subtype_relation
                    || self.rel() == c.strict_subtype_relation)
                    && source.flags().intersects(TypeFlags::NumberLiteral))
            {
                // When relating a literal type to a union of primitive types, we know the relation is false unless
                // the union contains the base primitive type or the literal type in one of its fresh/regular forms.
                // We exclude numeric literals for non-subtype relations because numeric literals are assignable to
                // numeric enum literals with the same value. Similarly, we exclude enum literal types because
                // identically named enum types are related (see isEnumTypeRelatedTo). We exclude the comparable
                // relation in entirety because it needs to be checked in both directions.
                let alternate_form = if Some(source) == source.as_literal_type().regular_type.get()
                {
                    source.as_literal_type().fresh_type.get()
                } else {
                    source.as_literal_type().regular_type.get()
                };
                let mut primitive: Option<P<Type>> = None;
                if source.flags().intersects(TypeFlags::StringLiteral) {
                    primitive = Some(c.string_type);
                } else if source.flags().intersects(TypeFlags::NumberLiteral) {
                    primitive = Some(c.number_type);
                } else if source.flags().intersects(TypeFlags::BigIntLiteral) {
                    primitive = Some(c.bigint_type);
                }
                outcome.0 = 1;
                if primitive.is_some_and(|p| contains_type(c, target_types, p))
                    || alternate_form.is_some_and(|a| contains_type(c, target_types, a))
                {
                    return Ternary::TRUE;
                }
                return Ternary::FALSE;
            }
            let match_ = c.get_matching_union_constituent_for_type(target, source);
            outcome.1 = 4;
            if let Some(match_) = match_ {
                outcome.1 = 3;
                let related = self.is_related_to_ex(
                    c,
                    source,
                    match_,
                    RecursionFlags::Target,
                    false, /*reportErrors*/
                    None,  /*headMessage*/
                    intersection_state,
                );
                if related != Ternary::FALSE {
                    outcome.0 = 2;
                    outcome.1 = 2;
                    return related;
                }
            }
        }
        outcome.0 = 3;
        let census_on = c.census_on();
        let mut fail_ns = 0u64;
        let mut fail_cached = 0u64;
        for (i, &t) in target_types.iter().enumerate() {
            outcome.2 += 1;
            let (t0, cached) = if census_on {
                let (id, _) = get_relation_key(c, source, t, intersection_state, false, false);
                (
                    c.census.as_ref().unwrap().now_ns(),
                    self.rel().lookup(id) != RelationComparisonResult::None,
                )
            } else {
                (0, false)
            };
            let related = self.is_related_to_ex(
                c,
                source,
                t,
                RecursionFlags::Target,
                false, /*reportErrors*/
                None,  /*headMessage*/
                intersection_state,
            );
            if census_on && related == Ternary::FALSE {
                fail_ns += c.census.as_ref().unwrap().now_ns() - t0;
                fail_cached += cached as u64;
            }
            if related != Ternary::FALSE {
                if census_on && i > 0 {
                    // Would an index on one of the source's unit-typed properties have excluded every constituent tried
                    // before this one? (census only; the lookups below may resolve members early)
                    let mut indexable = false;
                    if source.flags().intersects(TypeFlags::Object | TypeFlags::Intersection) {
                        let props = c.get_properties_of_type(source);
                        for &p in props {
                            let pt = c.get_type_of_symbol(p);
                            if !is_unit_type(pt) {
                                continue;
                            }
                            let mut all = true;
                            for &tj in &target_types[..i] {
                                let excluded = tj
                                    .flags()
                                    .intersects(TypeFlags::Object | TypeFlags::Intersection)
                                    && c.get_property_of_type(tj, p.name()).is_some_and(|q| {
                                        let qt = c.get_type_of_symbol(q);
                                        is_unit_type(qt) && !c.is_type_assignable_to(pt, qt)
                                    });
                                if !excluded {
                                    all = false;
                                    break;
                                }
                            }
                            if all {
                                indexable = true;
                                break;
                            }
                        }
                    }
                    let n = target_types.len();
                    let census = c.census.as_mut().unwrap();
                    let s = census
                        .rel_union_late
                        .entry((crate::workcensus::bucket(n) as u8, indexable as u8))
                        .or_default();
                    s.count += 1;
                    s.a += i as u64;
                    s.b += fail_cached;
                    s.incl_ns += fail_ns;
                }
                return related;
            }
        }
        if census_on {
            let n = target_types.len();
            let census = c.census.as_mut().unwrap();
            let s =
                census.rel_union_late.entry((crate::workcensus::bucket(n) as u8, 2)).or_default();
            s.count += 1;
            s.a += target_types.len() as u64;
            s.b += fail_cached;
            s.incl_ns += fail_ns;
        }
        if report_errors {
            // Elaborate only if we can find a best matching type in the target union
            let best_matching_type = c.get_best_matching_type(source, target, |c, s, t| {
                self.is_related_to_simple(c, s, t)
            });
            if let Some(best_matching_type) = best_matching_type {
                self.is_related_to_ex(
                    c,
                    source,
                    best_matching_type,
                    RecursionFlags::Target,
                    true, /*reportErrors*/
                    None, /*headMessage*/
                    intersection_state,
                );
            }
        }
        Ternary::FALSE
    }

    // relater.go:3063
    fn type_related_to_each_type(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        let mut result = Ternary::TRUE;
        let target_types = target.types();
        result = match target_types.iter().try_fold(result, |mut result, &target_type| {
            let related = self.is_related_to_ex(
                c,
                source,
                target_type,
                RecursionFlags::Target,
                report_errors,
                None, /*headMessage*/
                intersection_state,
            );
            if related == Ternary::FALSE {
                return Err(Ternary::FALSE);
            }
            result &= related;
            Ok(result)
        }) {
            Ok(value) => value,
            Err(value) => return value,
        };
        result
    }

    // relater.go:3076
    fn each_type_related_to_some_type(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
    ) -> Ternary {
        let mut result = Ternary::TRUE;
        let source_types = source.types();
        result = match source_types.iter().try_fold(result, |mut result, &source_type| {
            let related = self.type_related_to_some_type(
                c,
                source_type,
                target,
                false, /*reportErrors*/
                IntersectionState::None,
            );
            if related == Ternary::FALSE {
                return Err(Ternary::FALSE);
            }
            result &= related;
            Ok(result)
        }) {
            Ok(value) => value,
            Err(value) => return value,
        };
        result
    }

    // Determine if possibly recursive types are related. First, check if the result is already available in the global cache.
    // Second, check if we have already started a comparison of the given two types in which case we assume the result to be true.
    // Third, check if both types are part of deeply nested chains of generic type instantiations and if so assume the types are
    // equal and infinitely expanding. Fourth, if we have reached a depth of 100 nested comparisons, assume we have runaway recursion
    // and issue an error. Otherwise, actually compare the structure of the two types.
    // relater.go:3094
    fn recursive_type_related_to(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
        intersection_state: IntersectionState,
        recursion_flags: RecursionFlags,
    ) -> Ternary {
        if self.overflow.get() {
            // Note that stack depth overflows can cause _any_ relation involving structured types to become false, so it is
            // important to have well-defined behavior even in cases that shouldn't normally occur.
            return Ternary::FALSE;
        }
        let is_identity = self.rel() == c.identity_relation;
        let (id, constrained) = get_relation_key(
            c,
            source,
            target,
            intersection_state,
            is_identity,
            false, /*ignoreConstraints*/
        );
        let entry = self.rel().lookup(id);
        crate::sitecount::hit(
            "relation cache (recursiveTypeRelatedTo)",
            if entry == RelationComparisonResult::None { "miss" } else { "hit" },
        );
        if c.census_on() {
            let k = crate::workcensus::rel_kind(c, self.rel()) as usize;
            let census = c.census.as_mut().unwrap();
            if entry != RelationComparisonResult::None {
                census.rel_cache[k][0] += 1;
            } else if self.maybe_stack_has(&id) {
                census.rel_cache[k][2] += 1;
            } else {
                census.rel_cache[k][1] += 1;
            }
        }
        if entry != RelationComparisonResult::None {
            if report_errors
                && entry.intersects(RelationComparisonResult::Failed)
                && !entry.intersects(RelationComparisonResult::Overflow)
            {
                // We are elaborating errors and the cached result is a failure not due to a comparison overflow,
                // so we will do the comparison again to generate an error message.
            } else {
                c.reliability_flags |= entry
                    & (RelationComparisonResult::ReportsUnmeasurable
                        | RelationComparisonResult::ReportsUnreliable);
                if report_errors && entry.intersects(RelationComparisonResult::Overflow) {
                    let source_string = c.type_to_string_exported(source);
                    let target_string = c.type_to_string_exported(target);
                    self.report_error(
                        c,
                        &diagnostics::EXCESSIVE_COMPLEXITY_COMPARING_TYPES_0_AND_1,
                        &[&source_string, &target_string],
                    );
                }
                if entry.intersects(RelationComparisonResult::Succeeded) {
                    return Ternary::TRUE;
                }
                return Ternary::FALSE;
            }
        }
        if self.relation_count.get() <= 0 {
            self.overflow.set(true);
            return Ternary::FALSE;
        }
        // If source and target are already being compared, consider them related with assumptions
        if self.maybe_stack_has(&id) {
            crate::sitecount::hit("relation cache (recursiveTypeRelatedTo)", "maybe stack hit");
            return Ternary::MAYBE;
        }
        // A constrained key indicates that we have type references that reference constrained
        // type parameters. For such keys we also check against the key we would have gotten if all type parameters
        // were unconstrained.
        if constrained {
            let (broadest_equivalent_id, _) = get_relation_key(
                c,
                source,
                target,
                intersection_state,
                is_identity,
                true, /*ignoreConstraints*/
            );
            if self.maybe_stack_has(&broadest_equivalent_id) {
                crate::sitecount::hit(
                    "relation cache (recursiveTypeRelatedTo)",
                    "maybe stack hit (broadest)",
                );
                return Ternary::MAYBE;
            }
        }
        if self.source_stack.borrow().len() == 100 || self.target_stack.borrow().len() == 100 {
            // We stop relating if we reach 100 levels of nesting. This is a backstop to catch infinite recursion
            // that wasn't caught by isDeeplyNestedType. It will also stop relating types that truly are over 100
            // levels deep, but those are exceedingly rare.
            return Ternary::MAYBE;
        }
        let maybe_start = self.maybe_keys.borrow().len();
        self.maybe_stack_push(id);
        let save_expanding_flags = self.expanding_flags.get();
        if recursion_flags.intersects(RecursionFlags::Source) {
            self.source_stack.borrow_mut().push(RelaterStackEntry::new(source));
            // isDeeplyNestedType never reaches this relater, so the stack borrow is not held across a re-entry.
            if !self.expanding_flags.get().intersects(ExpandingFlags::Source)
                && c.is_deeply_nested_on_stack(source, &self.source_stack.borrow(), 3)
            {
                self.expanding_flags.set(self.expanding_flags.get() | ExpandingFlags::Source);
            }
        }
        if recursion_flags.intersects(RecursionFlags::Target) {
            self.target_stack.borrow_mut().push(RelaterStackEntry::new(target));
            if !self.expanding_flags.get().intersects(ExpandingFlags::Target)
                && c.is_deeply_nested_on_stack(target, &self.target_stack.borrow(), 3)
            {
                self.expanding_flags.set(self.expanding_flags.get() | ExpandingFlags::Target);
            }
        }
        let save_reliability_flags = c.reliability_flags;
        c.reliability_flags = RelationComparisonResult::None;
        let too_complex_before = c.too_complex_reports;
        let result = if self.expanding_flags.get() == ExpandingFlags::Both {
            Ternary::MAYBE
        } else {
            self.structured_type_related_to(c, source, target, report_errors, intersection_state)
        };
        let propagating_variance_flags = c.reliability_flags;
        c.reliability_flags |= save_reliability_flags;
        if recursion_flags.intersects(RecursionFlags::Source) {
            self.source_stack.borrow_mut().pop();
        }
        if recursion_flags.intersects(RecursionFlags::Target) {
            self.target_stack.borrow_mut().pop();
        }
        self.expanding_flags.set(save_expanding_flags);
        // tsrs-only: a result computed while a TS2590 was reported (a constraint that became the error type) is not
        // recorded, so that the next comparison evaluates and reports again (`Checker::too_complex_since`).
        let record = !c.too_complex_since(too_complex_before);
        if result == Ternary::FALSE {
            // A false result goes straight into global cache (when something is false under
            // assumptions it will also be false without assumptions)
            if record {
                Relation::set(
                    &self.rel(),
                    id,
                    RelationComparisonResult::Failed | propagating_variance_flags,
                );
                self.relation_count.set(self.relation_count.get() - 1);
            }
            self.reset_maybe_stack(c, maybe_start, propagating_variance_flags, false);
        } else if result == Ternary::TRUE
            || (self.source_stack.borrow().is_empty() && self.target_stack.borrow().is_empty())
        {
            if (result == Ternary::TRUE || result == Ternary::MAYBE) && record {
                // If result is definitely true, record all maybe keys as having succeeded. Also, record Ternary.Maybe
                // results as having succeeded once we reach depth 0, but never record Ternary.Unknown results.
                self.reset_maybe_stack(c, maybe_start, propagating_variance_flags, true);
            } else {
                self.reset_maybe_stack(c, maybe_start, propagating_variance_flags, false);
            }
        }
        result
    }

    // relater.go:3201
    fn reset_maybe_stack(
        &self,
        _c: &mut Checker,
        maybe_start: usize,
        propagating_variance_flags: RelationComparisonResult,
        mark_all_as_succeeded: bool,
    ) {
        let len = self.maybe_keys.borrow().len();
        if mark_all_as_succeeded {
            for i in maybe_start..len {
                let key = self.maybe_keys.borrow()[i];
                Relation::set(
                    &self.rel(),
                    key,
                    RelationComparisonResult::Succeeded | propagating_variance_flags,
                );
                self.relation_count.set(self.relation_count.get() - 1);
            }
        }
        // `maybe_keys_set` mirrors the stack only while it is longer than MAYBE_STACK_SCAN_MAX (`maybe_stack_has`).
        if maybe_start <= MAYBE_STACK_SCAN_MAX {
            if len > MAYBE_STACK_SCAN_MAX {
                self.maybe_keys_set.borrow_mut().clear();
            }
        } else {
            let keys = self.maybe_keys.borrow();
            let mut set = self.maybe_keys_set.borrow_mut();
            for key in &keys[maybe_start..len] {
                set.delete(key);
            }
        }
        self.maybe_keys.borrow_mut().truncate(maybe_start);
    }

    /// Whether `key` is on the maybe stack: a scan while the stack is short (nearly always), the set beyond.
    #[inline]
    fn maybe_stack_has(&self, key: &RelationKey) -> bool {
        let keys = self.maybe_keys.borrow();
        if keys.len() <= MAYBE_STACK_SCAN_MAX {
            return keys.contains(key);
        }
        self.maybe_keys_set.borrow().has(key)
    }

    fn maybe_stack_push(&self, key: RelationKey) {
        let mut keys = self.maybe_keys.borrow_mut();
        keys.push(key);
        let mut set = self.maybe_keys_set.borrow_mut();
        if keys.len() == MAYBE_STACK_SCAN_MAX + 1 {
            set.m.extend(keys.iter().copied());
        } else if keys.len() > MAYBE_STACK_SCAN_MAX {
            set.add(key);
        }
    }

    // relater.go:3212
    fn get_error_state(&self, _c: &mut Checker) -> ErrorState {
        ErrorState {
            error_chain: self.error_chain.get(),
            related_info: self.related_info.borrow().clone(),
        }
    }

    // relater.go:3219
    fn restore_error_state(&self, _c: &mut Checker, e: ErrorState) {
        self.error_chain.set(e.error_chain);
        *self.related_info.borrow_mut() = e.related_info;
    }

    // relater.go:3224
    #[cfg_attr(
        not(feature = "work-census"),
        inline(always),
        expect(
            clippy::inline_always,
            reason = "without the census the wrapper is a forwarding call; inlined, callers call the body as before (notes/perf-checker-algorithms.md)"
        )
    )]
    fn structured_type_related_to(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        if c.census_on() {
            let span = c.census_begin(crate::workcensus::Cat::Rel, || {
                let (sym, label) = crate::workcensus::type_identity(target);
                crate::workcensus::CKey::Rel(0, sym, label)
            });
            let r = self.structured_type_related_to_body(
                c,
                source,
                target,
                report_errors,
                intersection_state,
            );
            let timing = c.census_end(span).unwrap();
            let k = crate::workcensus::rel_kind(c, self.rel());
            let (sym, label) = crate::workcensus::type_identity(target);
            let census = c.census.as_mut().unwrap();
            if ((((source.id.0 as u64) << 32) | target.id.0 as u64)
                .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                >> 61)
                == 0
            {
                *census.rel_pairs.entry((source.id.0, target.id.0)).or_default() |= 1 << k;
            }
            census.record(
                crate::workcensus::Cat::Rel,
                crate::workcensus::CKey::Rel(k, sym, label),
                timing,
                (r == Ternary::FALSE) as u64,
                0,
                0,
            );
            return r;
        }
        self.structured_type_related_to_body(c, source, target, report_errors, intersection_state)
    }

    fn structured_type_related_to_body(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        let save_error_state = self.get_error_state(c);
        let mut result = self.structured_type_related_to_worker(
            c,
            source,
            target,
            report_errors,
            intersection_state,
        );
        if self.rel() != c.identity_relation {
            // The combined constraint of an intersection type is the intersection of the constraints of
            // the constituents. When an intersection type contains instantiable types with union type
            // constraints, there are situations where we need to examine the combined constraint. One is
            // when the target is a union type. Another is when the intersection contains types belonging
            // to one of the disjoint domains. For example, given type variables T and U, each with the
            // constraint 'string | number', the combined constraint of 'T & U' is 'string | number' and
            // we need to check this constraint against a union on the target side. Also, given a type
            // variable V constrained to 'string | number', 'V & number' has a combined constraint of
            // 'string & number | number & number' which reduces to just 'number'.
            // This also handles type parameters, as a type parameter with a union constraint compared against a union
            // needs to have its constraint hoisted into an intersection with said type parameter, this way
            // the type param can be compared with itself in the target (with the influence of its constraint to match other parts)
            // For example, if `T extends 1 | 2` and `U extends 2 | 3` and we compare `T & U` to `T & U & (1 | 2 | 3)`
            if result == Ternary::FALSE
                && (source.flags().intersects(TypeFlags::Intersection)
                    || source.flags().intersects(TypeFlags::TypeParameter)
                        && target.flags().intersects(TypeFlags::Union))
            {
                let source_types: &[P<Type>] = if source.flags().intersects(TypeFlags::Intersection)
                {
                    source.types()
                } else {
                    std::slice::from_ref(&source)
                };
                let constraint = c.get_effective_constraint_of_intersection(
                    source_types,
                    target.flags().intersects(TypeFlags::Union),
                );
                if let Some(constraint) = constraint
                    && every_type(c, constraint, |_, t| t != source)
                {
                    // TODO: Stack errors so we get a pyramid for the "normal" comparison above, _and_ a second for this
                    result = self.is_related_to_ex(
                        c,
                        constraint,
                        target,
                        RecursionFlags::Source,
                        false, /*reportErrors*/
                        None,  /*headMessage*/
                        intersection_state,
                    );
                }
            }
            // When the target is an intersection we need an extra property check in order to detect nested excess
            // properties and nested weak types. The following are motivating examples that all should be errors, but
            // aren't without this extra property check:
            //
            //   let obj: { a: { x: string } } & { c: number } = { a: { x: 'hello', y: 2 }, c: 5 };  // Nested excess property
            //
            //   declare let wrong: { a: { y: string } };
            //   let weak: { a?: { x?: number } } & { c?: string } = wrong;  // Nested weak object type
            //
            if result != Ternary::FALSE
                && !intersection_state.intersects(IntersectionState::Target)
                && target.flags().intersects(TypeFlags::Intersection)
                && !c.is_generic_object_type(target)
                && source.flags().intersects(TypeFlags::Object | TypeFlags::Intersection)
            {
                result &= self.properties_related_to(
                    c,
                    source,
                    target,
                    report_errors,
                    &NO_EXCLUDED_PROPERTIES,
                    false, /*optionalsOnly*/
                    IntersectionState::None,
                );
                if result != Ternary::FALSE
                    && is_object_literal_type(source)
                    && source.object_flags().intersects(ObjectFlags::FreshLiteral)
                {
                    result &= self.index_signatures_related_to(
                        c,
                        source,
                        target,
                        false, /*sourceIsPrimitive*/
                        report_errors,
                        IntersectionState::None,
                    );
                }
            // When the source is an intersection we need an extra check of any optional properties in the target to
            // detect possible mismatched property types. For example:
            //
            //   function foo<T extends object>(x: { a?: string }, y: T & { a: boolean }) {
            //     x = y;  // Mismatched property in source intersection
            //   }
            //
            } else if result != Ternary::FALSE
                && c.is_non_generic_object_type(target)
                && !c.is_array_or_tuple_type(target)
                && Self::is_source_intersection_needing_extra_check(c, source, target)
            {
                result &= self.properties_related_to(
                    c,
                    source,
                    target,
                    report_errors,
                    &NO_EXCLUDED_PROPERTIES,
                    true, /*optionalsOnly*/
                    intersection_state,
                );
            }
        }
        if result != Ternary::FALSE {
            self.restore_error_state(c, save_error_state);
        }
        result
    }

    // relater.go:3286
    fn is_source_intersection_needing_extra_check(
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
    ) -> bool {
        source.flags().intersects(TypeFlags::Intersection)
            && c.get_apparent_type(source).flags().intersects(TypeFlags::StructuredType)
            && !source.types().iter().any(|&t| {
                t == target || t.object_flags().intersects(ObjectFlags::NonInferrableType)
            })
    }

    // The `relateVariances` closure of structuredTypeRelatedToWorker; the variables it captures by reference are passed
    // as `&mut` parameters.
    fn relate_variances(
        &self,
        c: &mut Checker,
        source_type_arguments: &[P<Type>],
        target_type_arguments: &[P<Type>],
        variances: &[VarianceFlags],
        intersection_state: IntersectionState,
        report_errors: bool,
        save_error_state: &ErrorState,
        result: &mut Ternary,
        variance_check_failed: &mut bool,
        original_error_chain: &mut Option<P<ErrorChain>>,
    ) -> (Ternary, bool) {
        *result = self.type_arguments_related_to(
            c,
            source_type_arguments,
            target_type_arguments,
            variances,
            report_errors,
            intersection_state,
        );
        if *result != Ternary::FALSE {
            return (*result, true);
        }
        if variances.iter().any(|v| v.intersects(VarianceFlags::AllowsStructuralFallback)) {
            // If some type parameter was `Unmeasurable` or `Unreliable`, and we couldn't pass by assuming it was identical, then we
            // have to allow a structural fallback check
            // We elide the variance-based error elaborations, since those might not be too helpful, since we'll potentially
            // be assuming identity of the type parameter.
            *original_error_chain = None;
            self.restore_error_state(c, save_error_state.clone());
            return (Ternary::FALSE, false);
        }
        let allow_structural_fallback =
            checker::Checker::has_covariant_void_argument(target_type_arguments, variances);
        *variance_check_failed = !allow_structural_fallback;
        // The type arguments did not relate appropriately, but it may be because we have no variance
        // information (in which case typeArgumentsRelatedTo defaulted to covariance for all type
        // arguments). It might also be the case that the target type has a 'void' type argument for
        // a covariant type parameter that is only used in return positions within the generic type
        // (in which case any type argument is permitted on the source side). In those cases we proceed
        // with a structural comparison. Otherwise, we know for certain the instantiations aren't
        // related and we can return here.
        if !variances.is_empty() && !allow_structural_fallback {
            // In some cases generic types that are covariant in regular type checking mode become
            // invariant in --strictFunctionTypes mode because one or more type parameters are used in
            // both co- and contravariant positions. In order to make it easier to diagnose *why* such
            // types are invariant, if any of the type parameters are invariant we reset the reported
            // errors and instead force a structural comparison (which will include elaborations that
            // reveal the reason).
            // We can switch on `reportErrors` here, since varianceCheckFailed guarantees we return `False`,
            // we can return `False` early here to skip calculating the structural error message we don't need.
            if *variance_check_failed
                && !(report_errors
                    && variances
                        .iter()
                        .any(|&v| (v & VarianceFlags::VarianceMask) == VarianceFlags::Invariant))
            {
                return (Ternary::FALSE, true);
            }
            // We remember the original error information so we can restore it in case the structural
            // comparison unexpectedly succeeds. This can happen when the structural comparison result
            // is a Ternary.Maybe for example caused by the recursion depth limiter.
            *original_error_chain = self.error_chain.get();
            self.restore_error_state(c, save_error_state.clone());
        }
        (Ternary::FALSE, false)
    }

    // relater.go:3293
    fn structured_type_related_to_worker(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        let mut source = source;
        let mut result: Ternary = Ternary::FALSE;
        let mut variance_check_failed = false;
        let mut original_error_chain: Option<P<ErrorChain>> = None;
        let save_error_state = self.get_error_state(c);
        if self.rel() == c.identity_relation {
            // We've already checked that source.flags and target.flags are identical
            if source.flags().intersects(TypeFlags::UnionOrIntersection) {
                let mut result = self.each_type_related_to_some_type(c, source, target);
                if result != Ternary::FALSE {
                    result &= self.each_type_related_to_some_type(c, target, source);
                }
                return result;
            } else if source.flags().intersects(TypeFlags::Index) {
                return self.is_related_to(
                    c,
                    source.target().unwrap(),
                    target.target().unwrap(),
                    RecursionFlags::Both,
                    false, /*reportErrors*/
                );
            } else if source.flags().intersects(TypeFlags::IndexedAccess) {
                result = self.is_related_to(
                    c,
                    source.as_indexed_access_type().object_type.get().unwrap(),
                    target.as_indexed_access_type().object_type.get().unwrap(),
                    RecursionFlags::Both,
                    false, /*reportErrors*/
                );
                if result != Ternary::FALSE {
                    result &= self.is_related_to(
                        c,
                        source.as_indexed_access_type().index_type.get().unwrap(),
                        target.as_indexed_access_type().index_type.get().unwrap(),
                        RecursionFlags::Both,
                        false, /*reportErrors*/
                    );
                    if result != Ternary::FALSE {
                        return result;
                    }
                }
            } else if source.flags().intersects(TypeFlags::Conditional) {
                let source_conditional = source.as_conditional_type();
                let target_conditional = target.as_conditional_type();
                if source_conditional.root.get().unwrap().is_distributive.get()
                    == target_conditional.root.get().unwrap().is_distributive.get()
                {
                    result = self.is_related_to(
                        c,
                        source_conditional.check_type.get().unwrap(),
                        target_conditional.check_type.get().unwrap(),
                        RecursionFlags::Both,
                        false, /*reportErrors*/
                    );
                    if result != Ternary::FALSE {
                        result &= self.is_related_to(
                            c,
                            source_conditional.extends_type.get().unwrap(),
                            target_conditional.extends_type.get().unwrap(),
                            RecursionFlags::Both,
                            false, /*reportErrors*/
                        );
                        if result != Ternary::FALSE {
                            let source_true = c.get_true_type_from_conditional_type(source);
                            let target_true = c.get_true_type_from_conditional_type(target);
                            result &= self.is_related_to(
                                c,
                                source_true,
                                target_true,
                                RecursionFlags::Both,
                                false, /*reportErrors*/
                            );
                            if result != Ternary::FALSE {
                                let source_false = c.get_false_type_from_conditional_type(source);
                                let target_false = c.get_false_type_from_conditional_type(target);
                                result &= self.is_related_to(
                                    c,
                                    source_false,
                                    target_false,
                                    RecursionFlags::Both,
                                    false, /*reportErrors*/
                                );
                                if result != Ternary::FALSE {
                                    return result;
                                }
                            }
                        }
                    }
                }
            } else if source.flags().intersects(TypeFlags::Substitution) {
                result = self.is_related_to(
                    c,
                    source.as_substitution_type().base_type.get().unwrap(),
                    target.as_substitution_type().base_type.get().unwrap(),
                    RecursionFlags::Both,
                    false, /*reportErrors*/
                );
                if result != Ternary::FALSE {
                    result &= self.is_related_to(
                        c,
                        source.as_substitution_type().constraint.get().unwrap(),
                        target.as_substitution_type().constraint.get().unwrap(),
                        RecursionFlags::Both,
                        false, /*reportErrors*/
                    );
                    if result != Ternary::FALSE {
                        return result;
                    }
                }
            } else if source.flags().intersects(TypeFlags::TemplateLiteral) {
                if source.as_template_literal_type().texts()
                    == target.as_template_literal_type().texts()
                {
                    result = Ternary::TRUE;
                    // A measured hot loop: a recursive try_fold costs more (notes/perf-iterator-regressions.md).
                    for (i, &source_type) in
                        source.as_template_literal_type().types().iter().enumerate()
                    {
                        let target_type = target.as_template_literal_type().types()[i];
                        result &= self.is_related_to(
                            c,
                            source_type,
                            target_type,
                            RecursionFlags::Both,
                            false, /*reportErrors*/
                        );
                        if result == Ternary::FALSE {
                            return result;
                        }
                    }
                    return result;
                }
            } else if source.flags().intersects(TypeFlags::StringMapping)
                && source.symbol() == target.symbol()
            {
                return self.is_related_to(
                    c,
                    source.as_string_mapping_type().target.get().unwrap(),
                    target.as_string_mapping_type().target.get().unwrap(),
                    RecursionFlags::Both,
                    false, /*reportErrors*/
                );
            }
            if !source.flags().intersects(TypeFlags::Object) {
                return Ternary::FALSE;
            }
        } else if source.flags().intersects(TypeFlags::UnionOrIntersection)
            || target.flags().intersects(TypeFlags::UnionOrIntersection)
        {
            result = self.union_or_intersection_related_to(
                c,
                source,
                target,
                report_errors,
                intersection_state,
            );
            if result != Ternary::FALSE {
                return result;
            }
            // The ordered decomposition above doesn't handle all cases. Specifically, we also need to handle:
            // Source is instantiable (e.g. source has union or intersection constraint).
            // Source is an object, target is a union (e.g. { a, b: boolean } <=> { a, b: true } | { a, b: false }).
            // Source is an intersection, target is an object (e.g. { a } & { b } <=> { a, b }).
            // Source is an intersection, target is a union (e.g. { a } & { b: boolean } <=> { a, b: true } | { a, b: false }).
            // Source is an intersection, target instantiable (e.g. string & { tag } <=> T["a"] constrained to string & { tag }).
            if !(source.flags().intersects(TypeFlags::Instantiable)
                || source.flags().intersects(TypeFlags::Object)
                    && target.flags().intersects(TypeFlags::Union)
                || source.flags().intersects(TypeFlags::Intersection)
                    && target
                        .flags()
                        .intersects(TypeFlags::Object | TypeFlags::Union | TypeFlags::Instantiable))
            {
                return Ternary::FALSE;
            }
        }
        // We limit alias variance probing to only object and conditional types since their alias behavior
        // is more predictable than other, interned types, which may or may not have an alias depending on
        // the order in which things were checked.
        if source.flags().intersects(TypeFlags::Object | TypeFlags::Conditional)
            && source.alias().is_some()
            && !source.alias().unwrap().type_arguments().is_empty()
            && target.alias().is_some()
            && source.alias().unwrap().symbol() == target.alias().unwrap().symbol()
            && !(c.is_marker_type(source) || c.is_marker_type(target))
        {
            let alias_symbol = source.alias().unwrap().symbol().unwrap();
            let variances = c.get_alias_variances(alias_symbol);
            if variances.is_empty() {
                return Ternary::UNKNOWN;
            }
            let params = c.type_alias_links.get(alias_symbol).type_parameters.get();
            let min_params = checker::Checker::get_min_type_argument_count(params);
            let node_is_in_js_file = is_in_js_file(alias_symbol.value_declaration());
            let source_types = c.fill_missing_type_arguments(
                source.alias().unwrap().type_arguments(),
                params,
                min_params,
                node_is_in_js_file,
            );
            let target_types = c.fill_missing_type_arguments(
                target.alias().unwrap().type_arguments(),
                params,
                min_params,
                node_is_in_js_file,
            );
            let (variance_result, ok) = self.relate_variances(
                c,
                &source_types,
                &target_types,
                variances,
                intersection_state,
                report_errors,
                &save_error_state,
                &mut result,
                &mut variance_check_failed,
                &mut original_error_chain,
            );
            if ok {
                return variance_result;
            }
        }
        // For a generic type T and a type U that is assignable to T, [...U] is assignable to T, U is assignable to readonly [...T],
        // and U is assignable to [...T] when U is constrained to a mutable array or tuple type.
        if is_single_element_generic_tuple_type(source)
            && !source.target_tuple_type().readonly.get()
        {
            let element = c.get_type_arguments(source)[0];
            result = self.is_related_to(
                c,
                element,
                target,
                RecursionFlags::Source,
                false, /*reportErrors*/
            );
            if result != Ternary::FALSE {
                return result;
            }
        }
        if is_single_element_generic_tuple_type(target)
            && (target.target_tuple_type().readonly.get() || {
                let base = c.get_base_constraint_or_type(source);
                c.is_mutable_array_or_tuple(base)
            })
        {
            let element = c.get_type_arguments(target)[0];
            result = self.is_related_to(
                c,
                source,
                element,
                RecursionFlags::Target,
                false, /*reportErrors*/
            );
            if result != Ternary::FALSE {
                return result;
            }
        }
        if target.flags().intersects(TypeFlags::TypeParameter) {
            // A source type { [P in Q]: X } is related to a target type T if keyof T is related to Q and X is related to T[Q].
            if source.object_flags().intersects(ObjectFlags::Mapped)
                && source
                    .as_mapped_type()
                    .declaration
                    .get()
                    .unwrap()
                    .as_mapped_type_node()
                    .name_type
                    .is_none()
                && {
                    let index_type = c.get_index_type(target);
                    let constraint_type = c.get_constraint_type_from_mapped_type(source);
                    self.is_related_to(c, index_type, constraint_type, RecursionFlags::Both, false)
                        != Ternary::FALSE
                }
                && !get_mapped_type_modifiers(source)
                    .intersects(MappedTypeModifiers::IncludeOptional)
            {
                let template_type = c.get_template_type_from_mapped_type(source);
                let type_parameter = c.get_type_parameter_from_mapped_type(source);
                let indexed_access_type = c.get_indexed_access_type(target, type_parameter);
                result = self.is_related_to(
                    c,
                    template_type,
                    indexed_access_type,
                    RecursionFlags::Both,
                    report_errors,
                );
                if result != Ternary::FALSE {
                    return result;
                }
            }
            if self.rel() == c.comparable_relation
                && source.flags().intersects(TypeFlags::TypeParameter)
            {
                // This is a carve-out in comparability to essentially forbid comparing a type parameter with another type parameter
                // unless one extends the other. (Remember: comparability is mostly bidirectional!)
                if let Some(constraint) = c.get_constraint_of_type_parameter(source)
                    && some_type(c, constraint, |_, t| {
                        t.flags().intersects(TypeFlags::TypeParameter)
                    })
                {
                    return self.is_related_to(
                        c,
                        constraint,
                        target,
                        RecursionFlags::Source,
                        false, /*reportErrors*/
                    );
                }
                return Ternary::FALSE;
            }
        } else if target.flags().intersects(TypeFlags::IndexedAccess) {
            if source.flags().intersects(TypeFlags::IndexedAccess) {
                // Relate components directly before falling back to constraint relationships
                // A type S[K] is related to a type T[J] if S is related to T and K is related to J.
                result = self.is_related_to(
                    c,
                    source.as_indexed_access_type().object_type.get().unwrap(),
                    target.as_indexed_access_type().object_type.get().unwrap(),
                    RecursionFlags::Both,
                    report_errors,
                );
                if result != Ternary::FALSE {
                    result &= self.is_related_to(
                        c,
                        source.as_indexed_access_type().index_type.get().unwrap(),
                        target.as_indexed_access_type().index_type.get().unwrap(),
                        RecursionFlags::Both,
                        report_errors,
                    );
                }
                if result != Ternary::FALSE {
                    return result;
                }
                if report_errors {
                    original_error_chain = self.error_chain.get();
                }
            }
            // A type S is related to a type T[K] if S is related to C, where C is the base
            // constraint of T[K] for writing.
            if self.rel() == c.assignable_relation || self.rel() == c.comparable_relation {
                let object_type = target.as_indexed_access_type().object_type.get().unwrap();
                let index_type = target.as_indexed_access_type().index_type.get().unwrap();
                let base_object_type = c.get_base_constraint_or_type(object_type);
                let base_index_type = c.get_base_constraint_or_type(index_type);
                if !c.is_generic_object_type(base_object_type)
                    && !c.is_generic_index_type(base_index_type)
                {
                    let access_flags = AccessFlags::Writing
                        | if base_object_type == object_type {
                            AccessFlags::None
                        } else {
                            AccessFlags::NoIndexSignatures
                        };
                    let constraint = c.get_indexed_access_type_or_undefined(
                        base_object_type,
                        base_index_type,
                        access_flags,
                        None,
                        AliasArg::None,
                    );
                    if let Some(constraint) = constraint {
                        if report_errors && original_error_chain.is_some() {
                            // create a new chain for the constraint error
                            self.restore_error_state(c, save_error_state.clone());
                        }
                        result = self.is_related_to_ex(
                            c,
                            source,
                            constraint,
                            RecursionFlags::Target,
                            report_errors,
                            None, /*headMessage*/
                            intersection_state,
                        );
                        if result != Ternary::FALSE {
                            return result;
                        }
                        // prefer the shorter chain of the constraint comparison chain, and the direct comparison chain
                        if report_errors
                            && original_error_chain.is_some()
                            && self.error_chain.get().is_some()
                            && chain_depth(original_error_chain)
                                <= chain_depth(self.error_chain.get())
                        {
                            self.error_chain.set(original_error_chain);
                        }
                    }
                }
            }
            if report_errors {
                original_error_chain = None;
            }
        } else if target.flags().intersects(TypeFlags::Index) {
            let target_type = target.as_index_type().target.get().unwrap();
            // A keyof S is related to a keyof T if T is related to S.
            if source.flags().intersects(TypeFlags::Index) {
                result = self.is_related_to(
                    c,
                    target_type,
                    source.as_index_type().target.get().unwrap(),
                    RecursionFlags::Both,
                    false, /*reportErrors*/
                );
                if result != Ternary::FALSE {
                    return result;
                }
            }
            if is_tuple_type(target_type) {
                // An index type can have a tuple type target when the tuple type contains variadic elements.
                // Check if the source is related to the known keys of the tuple type.
                let known_keys = c.get_known_keys_of_tuple_type(target_type);
                result = self.is_related_to(
                    c,
                    source,
                    known_keys,
                    RecursionFlags::Target,
                    report_errors,
                );
                if result != Ternary::FALSE {
                    return result;
                }
            } else {
                // A type S is assignable to keyof T if S is assignable to keyof C, where C is the
                // simplified form of T or, if T doesn't simplify, the constraint of T.
                let constraint = c.get_simplified_type_or_constraint(target_type);
                if let Some(constraint) = constraint {
                    // We require Ternary.True here such that circular constraints don't cause
                    // false positives. For example, given 'T extends { [K in keyof T]: string }',
                    // 'keyof T' has itself as its constraint and produces a Ternary.Maybe when
                    // related to other types.
                    let index_type = c.get_index_type_ex(
                        constraint,
                        target.as_index_type().index_flags.get() | IndexFlags::NoReducibleCheck,
                    );
                    if self.is_related_to(
                        c,
                        source,
                        index_type,
                        RecursionFlags::Target,
                        report_errors,
                    ) == Ternary::TRUE
                    {
                        return Ternary::TRUE;
                    }
                } else if c.is_generic_mapped_type(target_type) {
                    // generic mapped types that don't simplify or have a constraint still have a very simple set of keys we can compare against
                    // - their nameType or constraintType.
                    // In many ways, this comparison is a deferred version of what `getIndexTypeForMappedType` does to actually resolve the keys for _non_-generic types
                    let name_type = c.get_name_type_from_mapped_type(target_type);
                    let constraint_type = c.get_constraint_type_from_mapped_type(target_type);
                    let target_keys;
                    if let Some(name_type) = name_type
                        && checker::Checker::is_mapped_type_with_keyof_constraint_declaration(
                            target_type,
                        )
                    {
                        // we need to get the apparent mappings and union them with the generic mappings, since some properties may be
                        // missing from the `constraintType` which will otherwise be mapped in the object
                        let mapped_keys = c.get_apparent_mapped_type_keys(name_type, target_type);
                        // We still need to include the non-apparent (and thus still generic) keys in the target side of the comparison (in case they're in the source side)
                        target_keys = c.get_union_type(&[mapped_keys, name_type]);
                    } else if let Some(name_type) = name_type {
                        target_keys = name_type;
                    } else {
                        target_keys = constraint_type;
                    }
                    if self.is_related_to(
                        c,
                        source,
                        target_keys,
                        RecursionFlags::Target,
                        report_errors,
                    ) == Ternary::TRUE
                    {
                        return Ternary::TRUE;
                    }
                }
            }
        } else if target.flags().intersects(TypeFlags::Conditional) {
            // If we reach 10 levels of nesting for the same conditional type, assume it is an infinitely expanding recursive
            // conditional type and bail out with a Ternary.Maybe result.
            if c.is_deeply_nested_on_stack(target, &self.target_stack.borrow(), 10) {
                return Ternary::MAYBE;
            }
            let target_conditional = target.as_conditional_type();
            let root = target_conditional.root.get().unwrap();
            // We check for a relationship to a conditional type target only when the conditional type has no
            // 'infer' positions, is not distributive or is distributive but doesn't reference the check type
            // parameter in either of the result types, and the source isn't an instantiation of the same
            // conditional type (as happens when computing variance).
            if root.infer_type_parameters.get().is_empty()
                && !c.is_distribution_dependent(root)
                && !(source.flags().intersects(TypeFlags::Conditional)
                    && source.as_conditional_type().root.get() == Some(root))
            {
                // Check if the conditional is always true or always false but still deferred for distribution purposes.
                let skip_true = {
                    let check = c
                        .get_permissive_instantiation(target_conditional.check_type.get().unwrap());
                    let extends = c.get_permissive_instantiation(
                        target_conditional.extends_type.get().unwrap(),
                    );
                    !c.is_type_assignable_to(check, extends)
                };
                let skip_false = !skip_true && {
                    let check = c.get_restrictive_instantiation(
                        target_conditional.check_type.get().unwrap(),
                    );
                    let extends = c.get_restrictive_instantiation(
                        target_conditional.extends_type.get().unwrap(),
                    );
                    c.is_type_assignable_to(check, extends)
                };
                // TODO: Find a nice way to include potential conditional type breakdowns in error output, if they seem good (they usually don't)
                if skip_true {
                    result = Ternary::TRUE;
                } else {
                    let true_type = c.get_true_type_from_conditional_type(target);
                    result = self.is_related_to_ex(
                        c,
                        source,
                        true_type,
                        RecursionFlags::Target,
                        false, /*reportErrors*/
                        None,  /*headMessage*/
                        intersection_state,
                    );
                }
                if result != Ternary::FALSE {
                    if skip_false {
                        result &= Ternary::TRUE;
                    } else {
                        let false_type = c.get_false_type_from_conditional_type(target);
                        result &= self.is_related_to_ex(
                            c,
                            source,
                            false_type,
                            RecursionFlags::Target,
                            false, /*reportErrors*/
                            None,  /*headMessage*/
                            intersection_state,
                        );
                    }
                    if result != Ternary::FALSE {
                        return result;
                    }
                }
            }
        } else if target.flags().intersects(TypeFlags::TemplateLiteral) {
            if source.flags().intersects(TypeFlags::TemplateLiteral) {
                if self.rel() == c.comparable_relation {
                    if checker::Checker::template_literal_types_definitely_unrelated(
                        source.as_template_literal_type(),
                        target.as_template_literal_type(),
                    ) {
                        return Ternary::FALSE;
                    }
                    return Ternary::TRUE;
                }
                // Report unreliable variance for type variables referenced in template literal type placeholders.
                // For example, `foo-${number}` is related to `foo-${string}` even though number isn't related to string.
                c.instantiate_type(source, Some(c.report_unreliable_mapper));
            }
            let r = self.as_p();
            if c.is_type_matched_by_template_literal_type(
                source,
                target.as_template_literal_type(),
                r.worker_comparer(),
            ) {
                return Ternary::TRUE;
            }
        } else if target.flags().intersects(TypeFlags::StringMapping) {
            if !source.flags().intersects(TypeFlags::StringMapping)
                && c.is_member_of_string_mapping(source, target)
            {
                return Ternary::TRUE;
            }
        } else if c.is_generic_mapped_type(target) && self.rel() != c.identity_relation {
            // Check if source type `S` is related to target type `{ [P in Q]: T }` or `{ [P in Q as R]: T}`.
            let keys_remapped = target
                .as_mapped_type()
                .declaration
                .get()
                .unwrap()
                .as_mapped_type_node()
                .name_type
                .is_some();
            let template_type = c.get_template_type_from_mapped_type(target);
            let modifiers = get_mapped_type_modifiers(target);
            if !modifiers.intersects(MappedTypeModifiers::ExcludeOptional) {
                // If the mapped type has shape `{ [P in Q]: T[P] }`,
                // source `S` is related to target if `T` = `S`, i.e. `S` is related to `{ [P in Q]: S[P] }`.
                if !keys_remapped
                    && template_type.flags().intersects(TypeFlags::IndexedAccess)
                    && template_type.as_indexed_access_type().object_type.get() == Some(source)
                    && template_type.as_indexed_access_type().index_type.get()
                        == Some(c.get_type_parameter_from_mapped_type(target))
                {
                    return Ternary::TRUE;
                }
                if !c.is_generic_mapped_type(source) {
                    // If target has shape `{ [P in Q as R]: T}`, then its keys have type `R`.
                    // If target has shape `{ [P in Q]: T }`, then its keys have type `Q`.
                    let target_keys = if keys_remapped {
                        c.get_name_type_from_mapped_type(target).unwrap()
                    } else {
                        c.get_constraint_type_from_mapped_type(target)
                    };
                    // Type of the keys of source type `S`, i.e. `keyof S`.
                    let source_keys = c.get_index_type_ex(source, IndexFlags::NoIndexSignatures);
                    let include_optional =
                        modifiers.intersects(MappedTypeModifiers::IncludeOptional);
                    let mut filtered_by_applicability: Option<P<Type>> = None;
                    if include_optional {
                        filtered_by_applicability =
                            c.intersect_types(Some(target_keys), Some(source_keys));
                    }
                    // A source type `S` is related to a target type `{ [P in Q]: T }` if `Q` is related to `keyof S` and `S[Q]` is related to `T`.
                    // A source type `S` is related to a target type `{ [P in Q as R]: T }` if `R` is related to `keyof S` and `S[R]` is related to `T.
                    // A source type `S` is related to a target type `{ [P in Q]?: T }` if some constituent `Q'` of `Q` is related to `keyof S` and `S[Q']` is related to `T`.
                    // A source type `S` is related to a target type `{ [P in Q as R]?: T }` if some constituent `R'` of `R` is related to `keyof S` and `S[R']` is related to `T`.
                    if include_optional
                        && !filtered_by_applicability.unwrap().flags().intersects(TypeFlags::Never)
                        || !include_optional
                            && self.is_related_to(
                                c,
                                target_keys,
                                source_keys,
                                RecursionFlags::Both,
                                false,
                            ) != Ternary::FALSE
                    {
                        let template_type = c.get_template_type_from_mapped_type(target);
                        let type_parameter = c.get_type_parameter_from_mapped_type(target);
                        // Fastpath: When the template type has the form `Obj[P]` where `P` is the mapped type parameter, directly compare source `S` with `Obj`
                        // to avoid creating the (potentially very large) number of new intermediate types made by manufacturing `S[P]`.
                        let non_null_component =
                            c.extract_types_of_kind(template_type, !TypeFlags::Nullable);
                        if !keys_remapped
                            && non_null_component.flags().intersects(TypeFlags::IndexedAccess)
                            && non_null_component.as_indexed_access_type().index_type.get()
                                == Some(type_parameter)
                        {
                            result = self.is_related_to(
                                c,
                                source,
                                non_null_component
                                    .as_indexed_access_type()
                                    .object_type
                                    .get()
                                    .unwrap(),
                                RecursionFlags::Target,
                                report_errors,
                            );
                            if result != Ternary::FALSE {
                                return result;
                            }
                        } else {
                            // We need to compare the type of a property on the source type `S` to the type of the same property on the target type,
                            // so we need to construct an indexing type representing a property, and then use indexing type to index the source type for comparison.
                            // If the target type has shape `{ [P in Q]: T }`, then a property of the target has type `P`.
                            // If the target type has shape `{ [P in Q]?: T }`, then a property of the target has type `P`,
                            // but the property is optional, so we only want to compare properties `P` that are common between `keyof S` and `Q`.
                            // If the target type has shape `{ [P in Q as R]: T }`, then a property of the target has type `R`.
                            // If the target type has shape `{ [P in Q as R]?: T }`, then a property of the target has type `R`,
                            // but the property is optional, so we only want to compare properties `R` that are common between `keyof S` and `R`.
                            let mut indexing_type = type_parameter;
                            if keys_remapped {
                                indexing_type = filtered_by_applicability.unwrap_or(target_keys);
                            } else if let Some(filtered_by_applicability) =
                                filtered_by_applicability
                            {
                                indexing_type = c.get_intersection_type(&[
                                    filtered_by_applicability,
                                    type_parameter,
                                ]);
                            }
                            let indexed_access_type =
                                c.get_indexed_access_type(source, indexing_type);
                            // Compare `S[indexingType]` to `T`, where `T` is the type of a property of the target type.
                            result = self.is_related_to(
                                c,
                                indexed_access_type,
                                template_type,
                                RecursionFlags::Both,
                                report_errors,
                            );
                            if result != Ternary::FALSE {
                                return result;
                            }
                        }
                    }
                    original_error_chain = self.error_chain.get();
                    self.restore_error_state(c, save_error_state.clone());
                }
            }
        }
        if source.flags().intersects(TypeFlags::TypeVariable) {
            // IndexedAccess comparisons are handled above in the `target.flags&TypeFlagsIndexedAccess` branch
            if !source.flags().intersects(TypeFlags::IndexedAccess)
                || !target.flags().intersects(TypeFlags::IndexedAccess)
            {
                let constraint = match c.get_constraint_of_type(source) {
                    Some(constraint) => constraint,
                    None => c.unknown_type,
                };
                // hi-speed no-this-instantiation check (less accurate, but avoids costly `this`-instantiation when the constraint will suffice), see #28231 for report on why this is needed
                result = self.is_related_to_ex(
                    c,
                    constraint,
                    target,
                    RecursionFlags::Source,
                    false, /*reportErrors*/
                    None,  /*headMessage*/
                    intersection_state,
                );
                if result != Ternary::FALSE {
                    return result;
                }
                let constraint_with_this = c.get_type_with_this_argument(
                    constraint,
                    Some(source),
                    false, /*needApparentType*/
                );
                result = self.is_related_to_ex(
                    c,
                    constraint_with_this,
                    target,
                    RecursionFlags::Source,
                    report_errors
                        && constraint != c.unknown_type
                        && !(target.flags() & source.flags()).intersects(TypeFlags::TypeParameter),
                    None, /*headMessage*/
                    intersection_state,
                );
                if result != Ternary::FALSE {
                    return result;
                }
                if c.is_mapped_type_generic_indexed_access(source) {
                    // For an indexed access type { [P in K]: E}[X], above we have already explored an instantiation of E with X
                    // substituted for P. We also want to explore type { [P in K]: E }[C], where C is the constraint of X.
                    let index_constraint = c.get_constraint_of_type(
                        source.as_indexed_access_type().index_type.get().unwrap(),
                    );
                    if let Some(index_constraint) = index_constraint {
                        let indexed_access = c.get_indexed_access_type(
                            source.as_indexed_access_type().object_type.get().unwrap(),
                            index_constraint,
                        );
                        result = self.is_related_to(
                            c,
                            indexed_access,
                            target,
                            RecursionFlags::Source,
                            report_errors,
                        );
                        if result != Ternary::FALSE {
                            return result;
                        }
                    }
                }
            }
        } else if source.flags().intersects(TypeFlags::Index) {
            let source_index_target = source.as_index_type().target.get().unwrap();
            let is_deferred_mapped_index =
                c.should_defer_index_type(
                    source_index_target,
                    source.as_index_type().index_flags.get(),
                ) && source_index_target.object_flags().intersects(ObjectFlags::Mapped);
            result = self.is_related_to(
                c,
                c.string_number_symbol_type,
                target,
                RecursionFlags::Source,
                report_errors && !is_deferred_mapped_index,
            );
            if result != Ternary::FALSE {
                return result;
            }
            if is_deferred_mapped_index {
                let mapped_type = source_index_target;
                let name_type = c.get_name_type_from_mapped_type(mapped_type);
                // Unlike on the target side, on the source side we do *not* include the generic part of the `nameType`, since that comes from a
                // (potentially anonymous) mapped type local type parameter, so that'd never assign outside the mapped type body, but we still want to
                // allow assignments of index types of identical (or similar enough) mapped types.
                // eg, `keyof {[X in keyof A]: Obj[X]}` should be assignable to `keyof {[Y in keyof A]: Tup[Y]}` because both map over the same set of keys (`keyof A`).
                // Without this source-side breakdown, a `keyof {[X in keyof A]: Obj[X]}` style type won't be assignable to anything except itself, which is much too strict.
                let source_mapped_keys = if let Some(name_type) = name_type
                    && checker::Checker::is_mapped_type_with_keyof_constraint_declaration(
                        mapped_type,
                    ) {
                    c.get_apparent_mapped_type_keys(name_type, mapped_type)
                } else if let Some(name_type) = name_type {
                    name_type
                } else {
                    c.get_constraint_type_from_mapped_type(mapped_type)
                };
                result = self.is_related_to(
                    c,
                    source_mapped_keys,
                    target,
                    RecursionFlags::Source,
                    report_errors,
                );
                if result != Ternary::FALSE {
                    return result;
                }
            }
        } else if source.flags().intersects(TypeFlags::Conditional) {
            // If we reach 10 levels of nesting for the same conditional type, assume it is an infinitely expanding recursive
            // conditional type and bail out with a Ternary.Maybe result.
            if c.is_deeply_nested_on_stack(source, &self.source_stack.borrow(), 10) {
                return Ternary::MAYBE;
            }
            if target.flags().intersects(TypeFlags::Conditional) {
                // Two conditional types 'T1 extends U1 ? X1 : Y1' and 'T2 extends U2 ? X2 : Y2' are related if
                // one of T1 and T2 is related to the other, U1 and U2 are identical types, X1 is related to X2,
                // and Y1 is related to Y2.
                let source_conditional = source.as_conditional_type();
                let target_conditional = target.as_conditional_type();
                let source_params =
                    source_conditional.root.get().unwrap().infer_type_parameters.get();
                let mut source_extends = source_conditional.extends_type.get().unwrap();
                let mut mapper: Option<P<TypeMapper>> = None;
                if !source_params.is_empty() {
                    // If the source has infer type parameters, we instantiate them in the context of the target
                    let r = self.as_p();
                    let ctx = checker::Checker::new_inference_context(
                        source_params,
                        None, /*signature*/
                        InferenceFlags::None,
                        Some(r.worker_comparer()),
                    );
                    c.infer_types(
                        ctx.inferences.get(),
                        target_conditional.extends_type.get().unwrap(),
                        source_extends,
                        InferencePriority::NO_CONSTRAINTS | InferencePriority::ALWAYS_STRICT,
                        false,
                    );
                    source_extends = c.instantiate_type(source_extends, ctx.mapper());
                    mapper = ctx.mapper();
                }
                if c.is_type_identical_to(
                    source_extends,
                    target_conditional.extends_type.get().unwrap(),
                ) && (self.is_related_to(
                    c,
                    source_conditional.check_type.get().unwrap(),
                    target_conditional.check_type.get().unwrap(),
                    RecursionFlags::Both,
                    false,
                ) != Ternary::FALSE
                    || self.is_related_to(
                        c,
                        target_conditional.check_type.get().unwrap(),
                        source_conditional.check_type.get().unwrap(),
                        RecursionFlags::Both,
                        false,
                    ) != Ternary::FALSE)
                {
                    let source_true = c.get_true_type_from_conditional_type(source);
                    let source_true = c.instantiate_type(source_true, mapper);
                    let target_true = c.get_true_type_from_conditional_type(target);
                    result = self.is_related_to(
                        c,
                        source_true,
                        target_true,
                        RecursionFlags::Both,
                        report_errors,
                    );
                    if result != Ternary::FALSE {
                        let source_false = c.get_false_type_from_conditional_type(source);
                        let target_false = c.get_false_type_from_conditional_type(target);
                        result &= self.is_related_to(
                            c,
                            source_false,
                            target_false,
                            RecursionFlags::Both,
                            report_errors,
                        );
                    }
                    if result != Ternary::FALSE {
                        return result;
                    }
                }
            }
            // conditionals can be related to one another via normal constraint, as, eg, `A extends B ? O : never` should be assignable to `O`
            // when `O` is a conditional (`never` is trivially assignable to `O`, as is `O`!).
            let default_constraint = c.get_default_constraint_of_conditional_type(source);
            result = self.is_related_to(
                c,
                default_constraint,
                target,
                RecursionFlags::Source,
                report_errors,
            );
            if result != Ternary::FALSE {
                return result;
            }
            // conditionals aren't related to one another via distributive constraint as it is much too inaccurate and allows way
            // more assignments than are desirable (since it maps the source check type to its constraint, it loses information).
            if !target.flags().intersects(TypeFlags::Conditional)
                && c.has_non_circular_base_constraint(source)
            {
                let distributive_constraint =
                    c.get_constraint_of_distributive_conditional_type(source);
                if let Some(distributive_constraint) = distributive_constraint {
                    self.restore_error_state(c, save_error_state.clone());
                    result = self.is_related_to(
                        c,
                        distributive_constraint,
                        target,
                        RecursionFlags::Source,
                        report_errors,
                    );
                    if result != Ternary::FALSE {
                        return result;
                    }
                }
            }
        } else if source.flags().intersects(TypeFlags::TemplateLiteral)
            && !target.flags().intersects(TypeFlags::Object)
        {
            if !target.flags().intersects(TypeFlags::TemplateLiteral) {
                let constraint = c.get_base_constraint_of_type(source);
                if let Some(constraint) = constraint
                    && constraint != source
                {
                    result = self.is_related_to(
                        c,
                        constraint,
                        target,
                        RecursionFlags::Source,
                        report_errors,
                    );
                    if result != Ternary::FALSE {
                        return result;
                    }
                }
            }
        } else if source.flags().intersects(TypeFlags::StringMapping) {
            if target.flags().intersects(TypeFlags::StringMapping) {
                if source.symbol() != target.symbol() {
                    return Ternary::FALSE;
                }
                result = self.is_related_to(
                    c,
                    source.as_string_mapping_type().target.get().unwrap(),
                    target.as_string_mapping_type().target.get().unwrap(),
                    RecursionFlags::Both,
                    report_errors,
                );
                if result != Ternary::FALSE {
                    return result;
                }
            } else {
                let constraint = c.get_base_constraint_of_type(source);
                if let Some(constraint) = constraint {
                    result = self.is_related_to(
                        c,
                        constraint,
                        target,
                        RecursionFlags::Source,
                        report_errors,
                    );
                    if result != Ternary::FALSE {
                        return result;
                    }
                }
            }
        } else {
            // An empty object type is related to any mapped type that includes a '?' modifier.
            if self.rel() != c.subtype_relation
                && self.rel() != c.strict_subtype_relation
                && is_partial_mapped_type(target)
                && c.is_empty_object_type(source)
            {
                return Ternary::TRUE;
            }
            if c.is_generic_mapped_type(target) {
                if c.is_generic_mapped_type(source) {
                    result = self.mapped_type_related_to(c, source, target, report_errors);
                    if result != Ternary::FALSE {
                        return result;
                    }
                }
                return Ternary::FALSE;
            }
            let source_is_primitive = source.flags().intersects(TypeFlags::Primitive);
            if self.rel() != c.identity_relation {
                source = c.get_apparent_type(source);
            } else if c.is_generic_mapped_type(source) {
                return Ternary::FALSE;
            }
            if source.object_flags().intersects(ObjectFlags::Reference)
                && target.object_flags().intersects(ObjectFlags::Reference)
                && source.target() == target.target()
                && !is_tuple_type(source)
                && !c.is_marker_type(source)
                && !c.is_marker_type(target)
            {
                // When strictNullChecks is disabled, the element type of the empty array literal is undefinedWideningType,
                // and an empty array literal wouldn't be assignable to a `never[]` without this check.
                if c.is_empty_array_literal_type(source) {
                    return Ternary::TRUE;
                }
                // We have type references to the same generic type, and the type references are not marker
                // type references (which are intended by be compared structurally). Obtain the variance
                // information for the type parameters and relate the type arguments accordingly.
                let variances = c.get_variances(source.target().unwrap());
                // We return Ternary.Maybe for a recursive invocation of getVariances (signaled by emptyArray). This
                // effectively means we measure variance only from type parameter occurrences that aren't nested in
                // recursive instantiations of the generic type.
                if variances.is_empty() {
                    return Ternary::UNKNOWN;
                }
                let source_type_arguments = c.get_type_arguments(source);
                let target_type_arguments = c.get_type_arguments(target);
                let (variance_result, ok) = self.relate_variances(
                    c,
                    source_type_arguments,
                    target_type_arguments,
                    variances,
                    intersection_state,
                    report_errors,
                    &save_error_state,
                    &mut result,
                    &mut variance_check_failed,
                    &mut original_error_chain,
                );
                if ok {
                    return variance_result;
                }
            } else if c.is_array_type(target)
                && (c.is_readonly_array_type(target)
                    && every_type(c, source, super::checker::Checker::is_array_or_tuple_type)
                    || every_type(c, source, |_, t| is_mutable_tuple_type(t)))
            {
                if self.rel() != c.identity_relation {
                    let source_index_type =
                        c.get_index_type_of_type_ex(source, c.number_type, c.any_type);
                    let target_index_type =
                        c.get_index_type_of_type_ex(target, c.number_type, c.any_type);
                    return self.is_related_to(
                        c,
                        source_index_type,
                        target_index_type,
                        RecursionFlags::Both,
                        report_errors,
                    );
                }
                // By flags alone, we know that the `target` is a readonly array while the source is a normal array or tuple
                // or `target` is an array and source is a tuple - in both cases the types cannot be identical, by construction
                return Ternary::FALSE;
            } else if checker::Checker::is_generic_tuple_type(source)
                && is_tuple_type(target)
                && !checker::Checker::is_generic_tuple_type(target)
            {
                let constraint = c.get_base_constraint_or_type(source);
                if constraint != source {
                    return self.is_related_to(
                        c,
                        constraint,
                        target,
                        RecursionFlags::Source,
                        report_errors,
                    );
                }
            } else if (self.rel() == c.subtype_relation || self.rel() == c.strict_subtype_relation)
                && c.is_empty_object_type(target)
                && target.object_flags().intersects(ObjectFlags::FreshLiteral)
                && !c.is_empty_object_type(source)
            {
                return Ternary::FALSE;
            }
            // Even if relationship doesn't hold for unions, intersections, or generic type references,
            // it may hold in a structural comparison.
            // In a check of the form X = A & B, we will have previously checked if A relates to X or B relates
            // to X. Failing both of those we want to check if the aggregation of A and B's members structurally
            // relates to X. Thus, we include intersection types on the source side here.
            if source.flags().intersects(TypeFlags::Object | TypeFlags::Intersection)
                && target.flags().intersects(TypeFlags::Object)
            {
                // Report structural errors only if we haven't reported any errors yet
                let report_structural_errors = report_errors
                    && self.error_chain.get() == save_error_state.error_chain
                    && !source_is_primitive;
                result = self.properties_related_to(
                    c,
                    source,
                    target,
                    report_structural_errors,
                    &NO_EXCLUDED_PROPERTIES,
                    false, /*optionalsOnly*/
                    intersection_state,
                );
                if result != Ternary::FALSE {
                    result &= self.signatures_related_to(
                        c,
                        source,
                        target,
                        SignatureKind::Call,
                        report_structural_errors,
                        intersection_state,
                    );
                    if result != Ternary::FALSE {
                        result &= self.signatures_related_to(
                            c,
                            source,
                            target,
                            SignatureKind::Construct,
                            report_structural_errors,
                            intersection_state,
                        );
                        if result != Ternary::FALSE {
                            result &= self.index_signatures_related_to(
                                c,
                                source,
                                target,
                                source_is_primitive,
                                report_structural_errors,
                                intersection_state,
                            );
                        }
                    }
                }
                if result != Ternary::FALSE {
                    if !variance_check_failed {
                        return result;
                    }
                    if original_error_chain.is_some() {
                        self.error_chain.set(original_error_chain);
                    } else if self.error_chain.get().is_none() {
                        self.error_chain.set(save_error_state.error_chain);
                    }
                    // Use variance error (there is no structural one) and return false
                }
            }
            // If S is an object type and T is a discriminated union, S may be related to T if
            // there exists a constituent of T for every combination of the discriminants of S
            // with respect to T. We do not report errors here, as we will use the existing
            // error result from checking each constituent of the union.
            if source.flags().intersects(TypeFlags::Object | TypeFlags::Intersection)
                && target.flags().intersects(TypeFlags::Union)
            {
                let object_only_target = c.extract_types_of_kind(
                    target,
                    TypeFlags::Object | TypeFlags::Intersection | TypeFlags::Substitution,
                );
                if object_only_target.flags().intersects(TypeFlags::Union) {
                    let result =
                        self.type_related_to_discriminated_type(c, source, object_only_target);
                    if result != Ternary::FALSE {
                        return result;
                    }
                }
            }
        }
        Ternary::FALSE
    }

    // relater.go:3935
    fn type_arguments_related_to(
        &self,
        c: &mut Checker,
        sources: &[P<Type>],
        targets: &[P<Type>],
        variances: &[VarianceFlags],
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        if sources.len() != targets.len() && self.rel() == c.identity_relation {
            return Ternary::FALSE;
        }
        let length = sources.len().min(targets.len());
        let mut result = Ternary::TRUE;
        for i in 0..length {
            // When variance information isn't available we default to covariance. This happens
            // in the process of computing variance information for recursive types and when
            // comparing 'this' type arguments.
            let mut variance_flags = VarianceFlags::Covariant;
            if i < variances.len() {
                variance_flags = variances[i];
            }
            let variance = variance_flags & VarianceFlags::VarianceMask;
            // We ignore arguments for independent type parameters (because they're never witnessed).
            if variance != VarianceFlags::Independent {
                let s = sources[i];
                let t = targets[i];
                let mut related;
                if variance_flags.intersects(VarianceFlags::Unmeasurable) {
                    // Even an `Unmeasurable` variance works out without a structural check if the source and target are _identical_.
                    // We can't simply assume invariance, because `Unmeasurable` marks nonlinear relations, for example, a relation tainted by
                    // the `-?` modifier in a mapped type (where, no matter how the inputs are related, the outputs still might not be)
                    if self.rel() == c.identity_relation {
                        related = self.is_related_to(
                            c,
                            s,
                            t,
                            RecursionFlags::Both,
                            false, /*reportErrors*/
                        );
                    } else {
                        related = c.compare_types_identical(s, t);
                    }
                } else {
                    // Propagate unreliable variance flag in variance computations
                    if !c.variance_stack.is_empty()
                        && variance_flags.intersects(VarianceFlags::Unreliable)
                    {
                        c.instantiate_type(s, Some(c.report_unreliable_mapper));
                    }
                    if variance == VarianceFlags::Covariant {
                        related = self.is_related_to_ex(
                            c,
                            s,
                            t,
                            RecursionFlags::Both,
                            report_errors,
                            None, /*headMessage*/
                            intersection_state,
                        );
                    } else if variance == VarianceFlags::Contravariant {
                        related = self.is_related_to_ex(
                            c,
                            t,
                            s,
                            RecursionFlags::Both,
                            report_errors,
                            None, /*headMessage*/
                            intersection_state,
                        );
                    } else if variance == VarianceFlags::Bivariant {
                        // In the bivariant case we first compare contravariantly without reporting
                        // errors. Then, if that doesn't succeed, we compare covariantly with error
                        // reporting. Thus, error elaboration will be based on the covariant check,
                        // which is generally easier to reason about.
                        related = self.is_related_to(
                            c,
                            t,
                            s,
                            RecursionFlags::Both,
                            false, /*reportErrors*/
                        );
                        if related == Ternary::FALSE {
                            related = self.is_related_to_ex(
                                c,
                                s,
                                t,
                                RecursionFlags::Both,
                                report_errors,
                                None, /*headMessage*/
                                intersection_state,
                            );
                        }
                    } else {
                        // In the invariant case we first compare covariantly, and only when that
                        // succeeds do we proceed to compare contravariantly. Thus, error elaboration
                        // will typically be based on the covariant check.
                        related = self.is_related_to_ex(
                            c,
                            s,
                            t,
                            RecursionFlags::Both,
                            report_errors,
                            None, /*headMessage*/
                            intersection_state,
                        );
                        if related != Ternary::FALSE {
                            related &= self.is_related_to_ex(
                                c,
                                t,
                                s,
                                RecursionFlags::Both,
                                report_errors,
                                None, /*headMessage*/
                                intersection_state,
                            );
                        }
                    }
                }
                if related == Ternary::FALSE {
                    return Ternary::FALSE;
                }
                result &= related;
            }
        }
        result
    }

    // A type [P in S]: X is related to a type [Q in T]: Y if T is related to S and X' is
    // related to Y, where X' is an instantiation of X in which P is replaced with Q. Notice
    // that S and T are contra-variant whereas X and Y are co-variant.
    // relater.go:4004
    fn mapped_type_related_to(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
    ) -> Ternary {
        let modifiers_related = self.rel() == c.comparable_relation
            || self.rel() == c.identity_relation
                && get_mapped_type_modifiers(source) == get_mapped_type_modifiers(target)
            || self.rel() != c.identity_relation
                && c.get_combined_mapped_type_optionality(source)
                    <= c.get_combined_mapped_type_optionality(target);
        if modifiers_related {
            let target_constraint = c.get_constraint_type_from_mapped_type(target);
            let source_constraint_uninstantiated = c.get_constraint_type_from_mapped_type(source);
            let mapper = if c.get_combined_mapped_type_optionality(source).is_lt() {
                c.report_unmeasurable_mapper
            } else {
                c.report_unreliable_mapper
            };
            let source_constraint =
                c.instantiate_type(source_constraint_uninstantiated, Some(mapper));
            let result = self.is_related_to(
                c,
                target_constraint,
                source_constraint,
                RecursionFlags::Both,
                report_errors,
            );
            if result != Ternary::FALSE {
                let source_type_parameter = c.get_type_parameter_from_mapped_type(source);
                let target_type_parameter = c.get_type_parameter_from_mapped_type(target);
                let mapper = new_simple_type_mapper(source_type_parameter, target_type_parameter);
                let source_name_type = c.get_name_type_from_mapped_type(source);
                let source_name_type = instantiate_type_or_nil(c, source_name_type, Some(mapper));
                let target_name_type = c.get_name_type_from_mapped_type(target);
                let target_name_type = instantiate_type_or_nil(c, target_name_type, Some(mapper));
                if source_name_type == target_name_type {
                    let source_template = c.get_template_type_from_mapped_type(source);
                    let source_template = c.instantiate_type(source_template, Some(mapper));
                    let target_template = c.get_template_type_from_mapped_type(target);
                    return result
                        & self.is_related_to(
                            c,
                            source_template,
                            target_template,
                            RecursionFlags::Both,
                            report_errors,
                        );
                }
            }
        }
        Ternary::FALSE
    }

    // relater.go:4021
    fn type_related_to_discriminated_type(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
    ) -> Ternary {
        // 1. Generate the combinations of discriminant properties & types 'source' can satisfy.
        //    a. If the number of combinations is above a set limit, the comparison is too complex.
        // 2. Filter 'target' to the subset of types whose discriminants exist in the matrix.
        //    a. If 'target' does not satisfy all discriminants in the matrix, 'source' is not related.
        // 3. For each type in the filtered 'target', determine if all non-discriminant properties of
        //    'target' are related to a property in 'source'.
        //
        // NOTE: See ~/tests/cases/conformance/types/typeRelationships/assignmentCompatibility/assignmentCompatWithDiscriminatedUnion.ts
        //       for examples.
        let source_properties = c.get_properties_of_type(source);
        let source_properties_filtered = c.find_discriminant_properties(source_properties, target);
        if source_properties_filtered.is_empty() {
            return Ternary::FALSE;
        }
        // Though we could compute the number of combinations as we generate
        // the matrix, this would incur additional memory overhead due to
        // array allocations. To reduce this overhead, we first compute
        // the number of combinations to ensure we will not surpass our
        // fixed limit before incurring the cost of any allocations:
        let mut num_combinations = 1usize;
        for &source_property in &source_properties_filtered {
            num_combinations *= count_types(c.get_non_missing_type_of_symbol(source_property));
            if num_combinations > 25 {
                return Ternary::FALSE;
            }
            if num_combinations == 0 {
                return Ternary::FALSE;
            }
        }
        // Compute the set of types for each discriminant property.
        let mut source_discriminant_types: Vec<Vec<P<Type>>> =
            vec![Vec::new(); source_properties_filtered.len()];
        let mut excluded_properties: Set<String> = Set::new();
        for (i, &source_property) in source_properties_filtered.iter().enumerate() {
            let source_property_type = c.get_non_missing_type_of_symbol(source_property);
            source_discriminant_types[i] = source_property_type.distributed();
            excluded_properties.add(source_property.name().to_string());
        }
        // Build the cartesian product
        let mut discriminant_combinations: Vec<Vec<P<Type>>> = Vec::with_capacity(num_combinations);
        for i in 0..num_combinations {
            let mut combination: Vec<Option<P<Type>>> = vec![None; source_discriminant_types.len()];
            let mut n = i;
            for (j, element) in source_discriminant_types.iter().enumerate().rev() {
                let source_types = element;
                let length = source_types.len();
                combination[j] = Some(source_types[n % length]);
                n /= length;
            }
            discriminant_combinations.push(combination.into_iter().map(|t| t.unwrap()).collect());
        }
        // Match each combination of the cartesian product of discriminant properties to one or more
        // constituents of 'target'. If any combination does not have a match then 'source' is not relatable.
        let mut matching_types: Vec<P<Type>> = Vec::new();
        for combination in &discriminant_combinations {
            let mut has_match = false;
            'outer: for &t in target.types() {
                for (i, element) in source_properties_filtered.iter().enumerate() {
                    let source_property = *element;
                    let target_property = c.get_property_of_type(t, source_property.name());
                    let Some(target_property) = target_property else {
                        continue 'outer;
                    };
                    if source_property == target_property {
                        continue;
                    }
                    // We compare the source property to the target in the context of a single discriminant type.
                    let combination_type = combination[i];
                    let skip_optional = c.strict_null_checks || self.rel() == c.comparable_relation;
                    let related = self.property_related_to(
                        c,
                        source,
                        target,
                        source_property,
                        target_property,
                        |_, _| combination_type,
                        false, /*reportErrors*/
                        IntersectionState::None,
                        skip_optional, /*skipOptional*/
                    );
                    // If the target property could not be found, or if the properties were not related,
                    // then this constituent is not a match.
                    if related == Ternary::FALSE {
                        continue 'outer;
                    }
                }
                if !matching_types.contains(&t) {
                    matching_types.push(t);
                }
                has_match = true;
            }
            if !has_match {
                // We failed to match any type for this combination.
                return Ternary::FALSE;
            }
        }
        // Compare the remaining non-discriminant properties of each match.
        let mut result = Ternary::TRUE;
        result = match matching_types.iter().try_fold(result, |mut result, &t| {
            result &= self.properties_related_to(
                c,
                source,
                t,
                false, /*reportErrors*/
                &excluded_properties,
                false, /*optionalsOnly*/
                IntersectionState::None,
            );
            if result != Ternary::FALSE {
                result &= self.signatures_related_to(
                    c,
                    source,
                    t,
                    SignatureKind::Call,
                    false, /*reportErrors*/
                    IntersectionState::None,
                );
                if result != Ternary::FALSE {
                    result &= self.signatures_related_to(
                        c,
                        source,
                        t,
                        SignatureKind::Construct,
                        false, /*reportErrors*/
                        IntersectionState::None,
                    );
                    if result != Ternary::FALSE && !(is_tuple_type(source) && is_tuple_type(t)) {
                        // Comparing numeric index types when both `source` and `type` are tuples is unnecessary as the
                        // element types should be sufficiently covered by `propertiesRelatedTo`. It also causes problems
                        // with index type assignability as the types for the excluded discriminants are still included
                        // in the index type.
                        result &= self.index_signatures_related_to(
                            c,
                            source,
                            t,
                            false, /*sourceIsPrimitive*/
                            false, /*reportErrors*/
                            IntersectionState::None,
                        );
                    }
                }
            }
            if result == Ternary::FALSE {
                return Err(result);
            }
            Ok(result)
        }) {
            Ok(value) => value,
            Err(value) => return value,
        };
        result
    }

    // relater.go:4132
    fn properties_related_to(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
        excluded_properties: &Set<String>,
        optionals_only: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        if self.rel() == c.identity_relation {
            return self.properties_identical_to(c, source, target, excluded_properties);
        }
        let mut result = Ternary::TRUE;
        if is_tuple_type(target) {
            if c.is_array_or_tuple_type(source) {
                if !target.target_tuple_type().readonly.get()
                    && (c.is_readonly_array_type(source)
                        || is_tuple_type(source) && source.target_tuple_type().readonly.get())
                {
                    return Ternary::FALSE;
                }
                let source_arity = checker::Checker::get_type_reference_arity(source);
                let target_arity = checker::Checker::get_type_reference_arity(target);
                let source_rest = if is_tuple_type(source) {
                    source.target_tuple_type().combined_flags.get().intersects(ElementFlags::Rest)
                } else {
                    true
                };
                let target_has_rest_element =
                    target.target_tuple_type().combined_flags.get().intersects(ElementFlags::Rest);
                let target_has_variable_element = target
                    .target_tuple_type()
                    .combined_flags
                    .get()
                    .intersects(ElementFlags::Variable);
                let source_min_length = if is_tuple_type(source) {
                    source.target_tuple_type().min_length.get()
                } else {
                    0
                };
                let target_min_length = target.target_tuple_type().min_length.get();
                if !source_rest && source_arity < target_min_length {
                    if report_errors {
                        self.report_error(
                            c,
                            &diagnostics::SOURCE_HAS_0_ELEMENT_S_BUT_TARGET_REQUIRES_1,
                            &[&source_arity, &target_min_length],
                        );
                    }
                    return Ternary::FALSE;
                }
                if !target_has_variable_element && target_arity < source_min_length {
                    if report_errors {
                        self.report_error(
                            c,
                            &diagnostics::SOURCE_HAS_0_ELEMENT_S_BUT_TARGET_ALLOWS_ONLY_1,
                            &[&source_min_length, &target_arity],
                        );
                    }
                    return Ternary::FALSE;
                }
                if !target_has_variable_element && (source_rest || target_arity < source_arity) {
                    if report_errors {
                        if source_min_length < target_min_length {
                            self.report_error(
                                c,
                                &diagnostics::TARGET_REQUIRES_0_ELEMENT_S_BUT_SOURCE_MAY_HAVE_FEWER,
                                &[&target_min_length],
                            );
                        } else {
                            self.report_error(c, &diagnostics::TARGET_ALLOWS_ONLY_0_ELEMENT_S_BUT_SOURCE_MAY_HAVE_MORE, &[&target_arity]);
                        }
                    }
                    return Ternary::FALSE;
                }
                let source_type_arguments = c.get_type_arguments(source);
                let target_type_arguments = c.get_type_arguments(target);
                let target_start_count =
                    get_start_element_count(target.target_tuple_type(), ElementFlags::NonRest);
                let target_end_count =
                    get_end_element_count(target.target_tuple_type(), ElementFlags::NonRest);
                let mut can_exclude_discriminants = !excluded_properties.is_empty();
                for (source_position, &source_type_argument) in
                    source_type_arguments.iter().enumerate().take(source_arity)
                {
                    let source_flags = if is_tuple_type(source) {
                        source.target_tuple_type().element_infos.get()[source_position].flags
                    } else {
                        ElementFlags::Rest
                    };
                    let source_position_from_end = source_arity - 1 - source_position;

                    let target_position = if target_has_rest_element
                        && source_position >= target_start_count
                    {
                        target_arity - 1 - source_position_from_end.min(target_end_count)
                    } else {
                        if source_position >= target_arity {
                            if report_errors {
                                self.report_error(c, &diagnostics::TARGET_ALLOWS_ONLY_0_ELEMENT_S_BUT_SOURCE_MAY_HAVE_MORE, &[&target_arity]);
                            }
                            return Ternary::FALSE;
                        }
                        source_position
                    };
                    let target_flags =
                        target.target_tuple_type().element_infos.get()[target_position].flags;
                    if target_flags.intersects(ElementFlags::Variadic)
                        && !source_flags.intersects(ElementFlags::Variadic)
                    {
                        if report_errors {
                            self.report_error(c, &diagnostics::SOURCE_PROVIDES_NO_MATCH_FOR_VARIADIC_ELEMENT_AT_POSITION_0_IN_TARGET, &[&target_position]);
                        }
                        return Ternary::FALSE;
                    }
                    if source_flags.intersects(ElementFlags::Variadic)
                        && !target_flags.intersects(ElementFlags::Variable)
                    {
                        if report_errors {
                            self.report_error(c, &diagnostics::VARIADIC_ELEMENT_AT_POSITION_0_IN_SOURCE_DOES_NOT_MATCH_ELEMENT_AT_POSITION_1_IN_TARGET, &[&source_position, &target_position]);
                        }
                        return Ternary::FALSE;
                    }
                    if target_flags.intersects(ElementFlags::Required)
                        && !source_flags.intersects(ElementFlags::Required)
                    {
                        if report_errors {
                            self.report_error(c, &diagnostics::SOURCE_PROVIDES_NO_MATCH_FOR_REQUIRED_ELEMENT_AT_POSITION_0_IN_TARGET, &[&target_position]);
                        }
                        return Ternary::FALSE;
                    }
                    // We can only exclude discriminant properties if we have not yet encountered a variable-length element.
                    if can_exclude_discriminants {
                        if source_flags.intersects(ElementFlags::Variable)
                            || target_flags.intersects(ElementFlags::Variable)
                        {
                            can_exclude_discriminants = false;
                        }
                        if can_exclude_discriminants
                            && excluded_properties.has(&source_position.to_string())
                        {
                            continue;
                        }
                    }
                    let source_type = c.remove_missing_type(
                        source_type_argument,
                        (source_flags & target_flags).intersects(ElementFlags::Optional),
                    );
                    let target_type = target_type_arguments[target_position];
                    let target_check_type = if source_flags.intersects(ElementFlags::Variadic)
                        && target_flags.intersects(ElementFlags::Rest)
                    {
                        c.create_array_type(target_type)
                    } else {
                        c.remove_missing_type(
                            target_type,
                            target_flags.intersects(ElementFlags::Optional),
                        )
                    };
                    let related = self.is_related_to_ex(
                        c,
                        source_type,
                        target_check_type,
                        RecursionFlags::Both,
                        report_errors,
                        None, /*headMessage*/
                        intersection_state,
                    );
                    if related == Ternary::FALSE {
                        if report_errors && (target_arity > 1 || source_arity > 1) {
                            if target_has_rest_element
                                && source_position >= target_start_count
                                && source_position_from_end >= target_end_count
                                && target_start_count != source_arity - target_end_count - 1
                            {
                                self.report_error(c, &diagnostics::TYPE_AT_POSITIONS_0_THROUGH_1_IN_SOURCE_IS_NOT_COMPATIBLE_WITH_TYPE_AT_POSITION_2_IN_TARGET, &[&target_start_count, &(source_arity - target_end_count - 1), &target_position]);
                            } else {
                                self.report_error(c, &diagnostics::TYPE_AT_POSITION_0_IN_SOURCE_IS_NOT_COMPATIBLE_WITH_TYPE_AT_POSITION_1_IN_TARGET, &[&source_position, &target_position]);
                            }
                        }
                        return Ternary::FALSE;
                    }
                    result &= related;
                }
                return result;
            }
            if target.target_tuple_type().combined_flags.get().intersects(ElementFlags::Variable) {
                return Ternary::FALSE;
            }
        }
        let require_optional_properties = (self.rel() == c.subtype_relation
            || self.rel() == c.strict_subtype_relation)
            && !is_object_literal_type(source)
            && !c.is_empty_array_literal_type(source)
            && !is_tuple_type(source);
        // tsrs-only: the loop below looks up the same names in `source` again; the lookups recorded here answer it.
        let mut found = c.unmatched_found_pool.pop().unwrap_or_default();
        found.clear();
        let unmatched_property = c.get_unmatched_property_recording(
            source,
            target,
            require_optional_properties,
            &mut found,
        );
        let result = self.properties_related_to_rest(
            c,
            source,
            target,
            report_errors,
            excluded_properties,
            optionals_only,
            intersection_state,
            require_optional_properties,
            unmatched_property,
            &found,
        );
        c.unmatched_found_pool.push(found);
        result
    }

    #[expect(
        clippy::option_option,
        reason = "The outer option records whether a result is available; the inner option records an absent value."
    )]
    fn properties_related_to_rest(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
        excluded_properties: &Set<String>,
        optionals_only: bool,
        intersection_state: IntersectionState,
        require_optional_properties: bool,
        unmatched_property: Option<P<Symbol>>,
        found: &[Option<Option<P<Symbol>>>],
    ) -> Ternary {
        let mut result = Ternary::TRUE;
        if let Some(unmatched_property) = unmatched_property {
            if report_errors && c.should_report_unmatched_property_error(source, target) {
                self.report_unmatched_property(
                    c,
                    source,
                    target,
                    unmatched_property,
                    require_optional_properties,
                );
            }
            return Ternary::FALSE;
        }
        if is_object_literal_type(target) {
            let source_properties = c.get_properties_of_type(source);
            if let Some(result) = exclude_properties(source_properties, excluded_properties)
                .iter()
                .copied()
                .into_iter()
                .find_map(|source_prop| {
                    if c.get_property_of_object_type(target, source_prop.name()).is_none() {
                        if report_errors {
                            let source_prop_string = c.symbol_to_string(source_prop);
                            let target_string = c.type_to_string_exported(target);
                            self.report_error(
                                c,
                                &diagnostics::PROPERTY_0_DOES_NOT_EXIST_ON_TYPE_1,
                                &[&source_prop_string, &target_string],
                            );
                        }
                        return Some(Ternary::FALSE);
                    }

                    None
                })
            {
                return result;
            }
        }
        // We only call this for union target types when we're attempting to do excess property checking - in those cases, we want to get _all possible props_
        // from the target union, across all members
        let properties = c.get_properties_of_type(target);
        let numeric_names_only = is_tuple_type(source) && is_tuple_type(target);
        // `found` is by position in `properties` when it covers them all and nothing is excluded.
        let found = if found.len() == properties.len() && excluded_properties.is_empty() {
            found
        } else {
            &[]
        };
        let resolved_source = c.resolved_property_source(source);
        for (i, target_prop) in
            exclude_properties(properties, excluded_properties).iter().copied().enumerate()
        {
            let name = target_prop.name();
            if !target_prop.flags().intersects(SymbolFlags::Prototype)
                && (!numeric_names_only || is_numeric_literal_name(name) || name == "length")
                && (!optionals_only || target_prop.flags().intersects(SymbolFlags::Optional))
            {
                let source_prop = match found.get(i) {
                    Some(&Some(source_prop)) => source_prop,
                    _ => match &resolved_source {
                        Some(resolved_source) => c.get_resolved_property(resolved_source, name),
                        None => c.get_property_of_type(source, name),
                    },
                };
                if let Some(source_prop) = source_prop
                    && source_prop != target_prop
                {
                    let skip_optional = self.rel() == c.comparable_relation;
                    let related = self.property_related_to(
                        c,
                        source,
                        target,
                        source_prop,
                        target_prop,
                        super::checker::Checker::get_non_missing_type_of_symbol,
                        report_errors,
                        intersection_state,
                        skip_optional,
                    );
                    if related == Ternary::FALSE {
                        return Ternary::FALSE;
                    }
                    result &= related;
                }
            }
        }
        result
    }

    // relater.go:4302
    fn property_related_to(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        source_prop: P<Symbol>,
        target_prop: P<Symbol>,
        get_type_of_source_property: impl FnMut(&mut Checker, P<Symbol>) -> P<Type>,
        report_errors: bool,
        intersection_state: IntersectionState,
        skip_optional: bool,
    ) -> Ternary {
        // tsrs: only the Private and Protected bits are tested below.
        let source_prop_flags = get_declaration_non_public_flags_from_symbol(source_prop);
        let target_prop_flags = get_declaration_non_public_flags_from_symbol(target_prop);
        if source_prop_flags.intersects(ModifierFlags::Private)
            || target_prop_flags.intersects(ModifierFlags::Private)
        {
            if source_prop.value_declaration() != target_prop.value_declaration() {
                if report_errors {
                    if source_prop_flags.intersects(ModifierFlags::Private)
                        && target_prop_flags.intersects(ModifierFlags::Private)
                    {
                        let target_prop_string = c.symbol_to_string(target_prop);
                        self.report_error(
                            c,
                            &diagnostics::TYPES_HAVE_SEPARATE_DECLARATIONS_OF_A_PRIVATE_PROPERTY_0,
                            &[&target_prop_string],
                        );
                    } else {
                        let target_prop_string = c.symbol_to_string(target_prop);
                        let first = c.type_to_string_exported(
                            if source_prop_flags.intersects(ModifierFlags::Private) {
                                source
                            } else {
                                target
                            },
                        );
                        let second = c.type_to_string_exported(
                            if source_prop_flags.intersects(ModifierFlags::Private) {
                                target
                            } else {
                                source
                            },
                        );
                        self.report_error(
                            c,
                            &diagnostics::PROPERTY_0_IS_PRIVATE_IN_TYPE_1_BUT_NOT_IN_TYPE_2,
                            &[&target_prop_string, &first, &second],
                        );
                    }
                }
                return Ternary::FALSE;
            }
        } else if target_prop_flags.intersects(ModifierFlags::Protected) {
            if !c.is_valid_override_of(source_prop, target_prop) {
                if report_errors {
                    let source_type = c.get_declaring_class(source_prop).unwrap_or(source);
                    let target_type = c.get_declaring_class(target_prop).unwrap_or(target);
                    let target_prop_string = c.symbol_to_string(target_prop);
                    let source_type_string = c.type_to_string_exported(source_type);
                    let target_type_string = c.type_to_string_exported(target_type);
                    self.report_error(c, &diagnostics::PROPERTY_0_IS_PROTECTED_BUT_TYPE_1_IS_NOT_A_CLASS_DERIVED_FROM_2, &[&target_prop_string, &source_type_string, &target_type_string]);
                }
                return Ternary::FALSE;
            }
        } else if source_prop_flags.intersects(ModifierFlags::Protected) {
            if report_errors {
                let target_prop_string = c.symbol_to_string(target_prop);
                let source_string = c.type_to_string_exported(source);
                let target_string = c.type_to_string_exported(target);
                self.report_error(
                    c,
                    &diagnostics::PROPERTY_0_IS_PROTECTED_IN_TYPE_1_BUT_PUBLIC_IN_TYPE_2,
                    &[&target_prop_string, &source_string, &target_string],
                );
            }
            return Ternary::FALSE;
        }
        // Ensure {readonly a: whatever} is not a subtype of {a: whatever},
        // while {a: whatever} is a subtype of {readonly a: whatever}.
        // This ensures the subtype relationship is ordered, and preventing declaration order
        // from deciding which type "wins" in union subtype reduction.
        // They're still assignable to one another, since `readonly` doesn't affect assignability.
        // This is only applied during the strictSubtypeRelation -- currently used in subtype reduction
        if self.rel() == c.strict_subtype_relation
            && c.is_readonly_symbol(source_prop)
            && !c.is_readonly_symbol(target_prop)
        {
            return Ternary::FALSE;
        }
        // If the target comes from a partial union prop, allow `undefined` in the target type
        let related = self.is_property_symbol_type_related(
            c,
            source_prop,
            target_prop,
            get_type_of_source_property,
            report_errors,
            intersection_state,
        );
        if related == Ternary::FALSE {
            if report_errors {
                let target_prop_string = c.symbol_to_string(target_prop);
                self.report_error(
                    c,
                    &diagnostics::TYPES_OF_PROPERTY_0_ARE_INCOMPATIBLE,
                    &[&target_prop_string],
                );
            }
            return Ternary::FALSE;
        }
        // When checking for comparability, be more lenient with optional properties.
        if !skip_optional
            && source_prop.flags().intersects(SymbolFlags::Optional)
            && target_prop.flags().intersects(SymbolFlags::ClassMember)
            && !target_prop.flags().intersects(SymbolFlags::Optional)
        {
            // TypeScript 1.0 spec (April 2014): 3.8.3
            // S is a subtype of a type T, and T is a supertype of S if ...
            // S' and T are object types and, for each member M in T..
            // M is a property and S' contains a property N where
            // if M is a required property, N is also a required property
            // (M - property in T)
            // (N - property in S)
            if report_errors {
                let target_prop_string = c.symbol_to_string(target_prop);
                let source_string = c.type_to_string_exported(source);
                let target_string = c.type_to_string_exported(target);
                self.report_error(
                    c,
                    &diagnostics::PROPERTY_0_IS_OPTIONAL_IN_TYPE_1_BUT_REQUIRED_IN_TYPE_2,
                    &[&target_prop_string, &source_string, &target_string],
                );
            }
            return Ternary::FALSE;
        }
        related
    }

    // relater.go:4366
    fn is_property_symbol_type_related(
        &self,
        c: &mut Checker,
        source_prop: P<Symbol>,
        target_prop: P<Symbol>,
        mut get_type_of_source_property: impl FnMut(&mut Checker, P<Symbol>) -> P<Type>,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        let target_is_optional =
            c.strict_null_checks && target_prop.check_flags().intersects(CheckFlags::Partial);
        let non_missing_target = c.get_non_missing_type_of_symbol(target_prop);
        let effective_target =
            c.add_optionality_ex(non_missing_target, false /*isProperty*/, target_is_optional);
        // source could resolve to `any` and that's not related to `unknown` target under strict subtype relation
        if effective_target.flags().intersects(if self.rel() == c.strict_subtype_relation {
            TypeFlags::Any
        } else {
            TypeFlags::AnyOrUnknown
        }) {
            return Ternary::TRUE;
        }
        let effective_source = get_type_of_source_property(c, source_prop);
        self.is_related_to_ex(
            c,
            effective_source,
            effective_target,
            RecursionFlags::Both,
            report_errors,
            None, /*headMessage*/
            intersection_state,
        )
    }

    // relater.go:4377
    fn report_unmatched_property(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        unmatched_property: P<Symbol>,
        require_optional_properties: bool,
    ) {
        // give specific error in case where private names have the same description
        if let Some(value_declaration) = unmatched_property.value_declaration()
            && value_declaration.name().is_some_and(ast::is_private_identifier)
            && source.symbol().is_some_and(|s| s.flags().intersects(SymbolFlags::Class))
        {
            let private_identifier_description = value_declaration.name().unwrap().text();
            let symbol_table_key = tsrs_binder::get_symbol_name_for_private_identifier(
                source.symbol().unwrap(),
                private_identifier_description,
            );
            if c.get_property_of_type(source, &symbol_table_key).is_some() {
                let source_symbol_string = c.symbol_to_string_exported(source.symbol().unwrap());
                let target_symbol_string = c.symbol_to_string_exported(target.symbol().unwrap());
                self.report_error(c, &diagnostics::PROPERTY_0_IN_TYPE_1_REFERS_TO_A_DIFFERENT_MEMBER_THAT_CANNOT_BE_ACCESSED_FROM_WITHIN_TYPE_2, &[&private_identifier_description, &source_symbol_string, &target_symbol_string]);
                return;
            }
        }
        let props = c.get_unmatched_properties(
            source,
            target,
            require_optional_properties,
            false, /*matchDiscriminantProperties*/
        );
        if props.len() == 1 {
            let (source_type, target_type) = c.get_type_names_for_error_display(source, target);
            let prop_name = c.symbol_to_string(unmatched_property);
            self.report_error(
                c,
                &diagnostics::PROPERTY_0_IS_MISSING_IN_TYPE_1_BUT_REQUIRED_IN_TYPE_2,
                &[&prop_name, &source_type, &target_type],
            );
            let first_declaration = unmatched_property.declarations().first().copied();
            if let Some(first_declaration) = first_declaration {
                let diagnostic = create_diagnostic_for_node(
                    Some(first_declaration),
                    &diagnostics::X_0_IS_DECLARED_HERE,
                    &[&prop_name],
                );
                self.related_info.borrow_mut().push(diagnostic);
            }
        } else if self
            .try_elaborate_array_like_errors(c, source, target, false /*reportErrors*/)
        {
            let (source_type, target_type) = c.get_type_names_for_error_display(source, target);
            if props.len() > 5 {
                let prop_names = props[..4]
                    .iter()
                    .map(|&p| c.symbol_to_string(p))
                    .collect::<Vec<_>>()
                    .join(", ");
                self.report_error(c, &diagnostics::TYPE_0_IS_MISSING_THE_FOLLOWING_PROPERTIES_FROM_TYPE_1_COLON_2_AND_3_MORE, &[&source_type, &target_type, &prop_names, &(props.len() - 4)]);
            } else {
                let prop_names =
                    props.iter().map(|&p| c.symbol_to_string(p)).collect::<Vec<_>>().join(", ");
                self.report_error(
                    c,
                    &diagnostics::TYPE_0_IS_MISSING_THE_FOLLOWING_PROPERTIES_FROM_TYPE_1_COLON_2,
                    &[&source_type, &target_type, &prop_names],
                );
            }
        }
    }

    // relater.go:4411
    fn try_elaborate_array_like_errors(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
    ) -> bool {
        /*
         * The spec for elaboration is:
         * - If the source is a readonly tuple and the target is a mutable array or tuple, elaborate on mutability and skip property elaborations.
         * - If the source is a tuple then skip property elaborations if the target is an array or tuple.
         * - If the source is a readonly array and the target is a mutable array or tuple, elaborate on mutability and skip property elaborations.
         * - If the source an array then skip property elaborations if the target is a tuple.
         */
        if is_tuple_type(source) {
            if source.target_tuple_type().readonly.get() && c.is_mutable_array_or_tuple(target) {
                if report_errors {
                    let source_string = c.type_to_string_exported(source);
                    let target_string = c.type_to_string_exported(target);
                    self.report_error(c, &diagnostics::THE_TYPE_0_IS_READONLY_AND_CANNOT_BE_ASSIGNED_TO_THE_MUTABLE_TYPE_1, &[&source_string, &target_string]);
                }
                return false;
            }
            return c.is_array_or_tuple_type(target);
        }
        if c.is_readonly_array_type(source) && c.is_mutable_array_or_tuple(target) {
            if report_errors {
                let source_string = c.type_to_string_exported(source);
                let target_string = c.type_to_string_exported(target);
                self.report_error(c, &diagnostics::THE_TYPE_0_IS_READONLY_AND_CANNOT_BE_ASSIGNED_TO_THE_MUTABLE_TYPE_1, &[&source_string, &target_string]);
            }
            return false;
        }
        if is_tuple_type(target) {
            return c.is_array_type(source);
        }
        true
    }

    // relater.go:4440
    fn try_elaborate_errors_for_primitives_and_objects(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
    ) {
        if (source == c.global_string_type && target == c.string_type)
            || (source == c.global_number_type && target == c.number_type)
            || (source == c.global_boolean_type && target == c.boolean_type)
            || (source == c.get_global_es_symbol_type() && target == c.es_symbol_type)
        {
            let target_string = c.type_to_string_exported(target);
            let source_string = c.type_to_string_exported(source);
            self.report_error(c, &diagnostics::X_0_IS_A_PRIMITIVE_BUT_1_IS_A_WRAPPER_OBJECT_PREFER_USING_0_WHEN_POSSIBLE, &[&target_string, &source_string]);
        }
    }

    // relater.go:4449
    fn properties_identical_to(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        excluded_properties: &Set<String>,
    ) -> Ternary {
        if !source.flags().intersects(TypeFlags::Object)
            || !target.flags().intersects(TypeFlags::Object)
        {
            return Ternary::FALSE;
        }
        let source_object_properties = c.get_properties_of_object_type(source);
        let source_properties = exclude_properties(source_object_properties, excluded_properties);
        let target_object_properties = c.get_properties_of_object_type(target);
        let target_properties = exclude_properties(target_object_properties, excluded_properties);
        if source_properties.len() != target_properties.len() {
            return Ternary::FALSE;
        }
        let mut result = Ternary::TRUE;
        result = match source_properties.iter().try_fold(result, |mut result, &source_prop| {
            let target_prop = c.get_property_of_object_type(target, source_prop.name());
            let Some(target_prop) = target_prop else {
                return Err(Ternary::FALSE);
            };
            let related = c.compare_properties(source_prop, target_prop, |c, s, t| {
                self.is_related_to_simple(c, s, t)
            });
            if related == Ternary::FALSE {
                return Err(Ternary::FALSE);
            }
            result &= related;
            Ok(result)
        }) {
            Ok(value) => value,
            Err(value) => return value,
        };
        result
    }

    // relater.go:4473
    fn signatures_related_to(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        kind: SignatureKind,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        if self.rel() == c.identity_relation {
            return self.signatures_identical_to(c, source, target, kind);
        }
        // With respect to signatures, the anyFunctionType wildcard is a subtype of every other function type.
        if source == c.any_function_type {
            return Ternary::TRUE;
        }
        if target == c.any_function_type {
            return Ternary::FALSE;
        }
        let source_signatures = c.get_signatures_of_type(source, kind);
        let target_signatures = c.get_signatures_of_type(target, kind);
        if kind == SignatureKind::Construct
            && !source_signatures.is_empty()
            && !target_signatures.is_empty()
        {
            let source_is_abstract =
                source_signatures[0].flags.get().intersects(SignatureFlags::Abstract);
            let target_is_abstract =
                target_signatures[0].flags.get().intersects(SignatureFlags::Abstract);
            if source_is_abstract && !target_is_abstract {
                // An abstract constructor type is not assignable to a non-abstract constructor type
                // as it would otherwise be possible to new an abstract class. Note that the assignability
                // check we perform for an extends clause excludes construct signatures from the target,
                // so this check never proceeds.
                if report_errors {
                    self.report_error(c, &diagnostics::CANNOT_ASSIGN_AN_ABSTRACT_CONSTRUCTOR_TYPE_TO_A_NON_ABSTRACT_CONSTRUCTOR_TYPE, &[]);
                }
                return Ternary::FALSE;
            }
            if !self.constructor_visibilities_are_compatible(
                c,
                source_signatures[0],
                target_signatures[0],
                report_errors,
            ) {
                return Ternary::FALSE;
            }
        }
        let mut result = Ternary::TRUE;
        if source.object_flags().intersects(ObjectFlags::Instantiated)
            && target.object_flags().intersects(ObjectFlags::Instantiated)
            && source.symbol() == target.symbol()
            || source.object_flags().intersects(ObjectFlags::Reference)
                && target.object_flags().intersects(ObjectFlags::Reference)
                && source.target() == target.target()
        {
            // We have instantiations of the same anonymous type (which typically will be the type of a
            // method). Simply do a pairwise comparison of the signatures in the two signature lists instead
            // of the much more expensive N * M comparison matrix we explore below. We erase type parameters
            // as they are known to always be the same.
            for (i, element) in target_signatures.iter().enumerate() {
                let related = self.signature_related_to(
                    c,
                    source_signatures[i],
                    *element,
                    true, /*erase*/
                    report_errors,
                    intersection_state,
                );
                if related == Ternary::FALSE {
                    return Ternary::FALSE;
                }
                result &= related;
            }
        } else if source_signatures.len() == 1 && target_signatures.len() == 1 {
            // For simple functions (functions with a single signature) we only erase type parameters for
            // the comparable relation. Otherwise, if the source signature is generic, we instantiate it
            // in the context of the target signature before checking the relationship. Ideally we'd do
            // this regardless of the number of signatures, but the potential costs are prohibitive due
            // to the quadratic nature of the logic below.
            let erase_generics = self.rel() == c.comparable_relation;
            result = self.signature_related_to(
                c,
                source_signatures[0],
                target_signatures[0],
                erase_generics,
                report_errors,
                intersection_state,
            );
        } else {
            'outer: for &t in target_signatures {
                let save_error_state = self.get_error_state(c);
                // Only elaborate errors from the first failure
                let mut should_elaborate_errors = report_errors;
                for &s in source_signatures {
                    let related = self.signature_related_to(
                        c,
                        s,
                        t,
                        true, /*erase*/
                        should_elaborate_errors,
                        intersection_state,
                    );
                    if related != Ternary::FALSE {
                        result &= related;
                        self.restore_error_state(c, save_error_state);
                        continue 'outer;
                    }
                    should_elaborate_errors = false;
                }
                if should_elaborate_errors {
                    let source_string = c.type_to_string_exported(source);
                    let signature_string = c.signature_to_string(t);
                    self.report_error(
                        c,
                        &diagnostics::TYPE_0_PROVIDES_NO_MATCH_FOR_THE_SIGNATURE_1,
                        &[&source_string, &signature_string],
                    );
                }
                return Ternary::FALSE;
            }
        }
        result
    }

    // relater.go:4550
    fn constructor_visibilities_are_compatible(
        &self,
        c: &mut Checker,
        source_signature: P<Signature>,
        target_signature: P<Signature>,
        report_errors: bool,
    ) -> bool {
        let (Some(source_declaration), Some(target_declaration)) =
            (source_signature.declaration.get(), target_signature.declaration.get())
        else {
            return true;
        };
        let source_accessibility =
            source_declaration.modifier_flags() & ModifierFlags::NonPublicAccessibilityModifier;
        let target_accessibility =
            target_declaration.modifier_flags() & ModifierFlags::NonPublicAccessibilityModifier;
        // A public, protected and private signature is assignable to a private signature.
        if target_accessibility == ModifierFlags::Private {
            return true;
        }
        // A public and protected signature is assignable to a protected signature.
        if target_accessibility == ModifierFlags::Protected
            && source_accessibility != ModifierFlags::Private
        {
            return true;
        }
        // Only a public signature is assignable to public signature.
        if target_accessibility != ModifierFlags::Protected && source_accessibility.is_empty() {
            return true;
        }
        if report_errors {
            let source_string = visibility_to_string(source_accessibility);
            let target_string = visibility_to_string(target_accessibility);
            self.report_error(
                c,
                &diagnostics::CANNOT_ASSIGN_A_0_CONSTRUCTOR_TYPE_TO_A_1_CONSTRUCTOR_TYPE,
                &[&source_string, &target_string],
            );
        }
        false
    }

    // See signatureAssignableTo, compareSignaturesIdentical
    // relater.go:4575
    fn signature_related_to(
        &self,
        c: &mut Checker,
        source: P<Signature>,
        target: P<Signature>,
        erase: bool,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        let mut check_mode = SignatureCheckMode::None;
        if self.rel() == c.subtype_relation {
            check_mode = SignatureCheckMode::StrictTopSignature;
        } else if self.rel() == c.strict_subtype_relation {
            check_mode = SignatureCheckMode::StrictTopSignature | SignatureCheckMode::StrictArity;
        }
        let mut source = source;
        let mut target = target;
        if erase {
            source = c.get_erased_signature(source);
            target = c.get_erased_signature(target);
        }
        let cached = self
            .signature_comparers
            .borrow()
            .iter()
            .find(|e| e.0 == intersection_state)
            .map(|e| e.1);
        let is_related_to_worker = if let Some(f) = cached {
            f
        } else {
            let r = self.as_p();
            let f = type_comparer(move |c, source, target, report_errors| {
                r.is_related_to_ex(
                    c,
                    source,
                    target,
                    RecursionFlags::Both,
                    report_errors,
                    None, /*headMessage*/
                    intersection_state,
                )
            });
            self.signature_comparers.borrow_mut().push((intersection_state, f));
            f
        };
        let mut reporter = |c: &mut Checker, message: &'static Message, args: &[&dyn Display]| {
            self.report_error(c, message, args);
        };
        c.compare_signatures_related(
            source,
            target,
            check_mode,
            report_errors,
            Some(&mut reporter),
            is_related_to_worker,
            Some(c.report_unreliable_mapper),
        )
    }

    // relater.go:4593
    fn signatures_identical_to(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        kind: SignatureKind,
    ) -> Ternary {
        let source_signatures = c.get_signatures_of_type(source, kind);
        let target_signatures = c.get_signatures_of_type(target, kind);
        if source_signatures.len() != target_signatures.len() {
            return Ternary::FALSE;
        }
        let mut result = Ternary::TRUE;
        for (i, element) in source_signatures.iter().enumerate() {
            let related = c.compare_signatures_identical(
                *element,
                target_signatures[i],
                false, /*partialMatch*/
                false, /*ignoreThisTypes*/
                false, /*ignoreReturnTypes*/
                |c, s, t| self.is_related_to_simple(c, s, t),
            );
            if related == Ternary::FALSE {
                return Ternary::FALSE;
            }
            result &= related;
        }
        result
    }

    // relater.go:4610
    fn index_signatures_related_to(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
        source_is_primitive: bool,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        if self.rel() == c.identity_relation {
            return self.index_signatures_identical_to(c, source, target);
        }
        let index_infos = c.get_index_infos_of_type(target);
        let string_type = c.string_type;
        let target_has_string_index = index_infos.iter().any(|info| info.key_type() == string_type);
        let mut result = Ternary::TRUE;
        result = match index_infos.iter().try_fold(result, |mut result, &target_info| {
            let related = if self.rel() != c.strict_subtype_relation
                && !source_is_primitive
                && target_has_string_index
                && target_info.value_type().flags().intersects(TypeFlags::Any)
            {
                Ternary::TRUE
            } else if c.is_generic_mapped_type(source) && target_has_string_index {
                let template_type = c.get_template_type_from_mapped_type(source);
                self.is_related_to(
                    c,
                    template_type,
                    target_info.value_type(),
                    RecursionFlags::Both,
                    report_errors,
                )
            } else {
                self.type_related_to_index_info(
                    c,
                    source,
                    target_info,
                    report_errors,
                    intersection_state,
                )
            };
            if related == Ternary::FALSE {
                return Err(Ternary::FALSE);
            }
            result &= related;
            Ok(result)
        }) {
            Ok(value) => value,
            Err(value) => return value,
        };
        result
    }

    // relater.go:4635
    fn type_related_to_index_info(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target_info: P<IndexInfo>,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        let source_info = c.get_applicable_index_info(source, target_info.key_type());
        if let Some(source_info) = source_info {
            return self.index_info_related_to(
                c,
                source_info,
                target_info,
                report_errors,
                intersection_state,
            );
        }
        // Intersection constituents are never considered to have an inferred index signature. Also, in the strict subtype relation,
        // only fresh object literals are considered to have inferred index signatures. This ensures { [x: string]: xxx } <: {} but
        // not vice-versa. Without this rule, those types would be mutual strict subtypes.
        if !intersection_state.intersects(IntersectionState::Source)
            && (self.rel() != c.strict_subtype_relation
                || source.object_flags().intersects(ObjectFlags::FreshLiteral))
            && c.is_object_type_with_inferable_index(source)
        {
            return self.members_related_to_index_info(
                c,
                source,
                target_info,
                report_errors,
                intersection_state,
            );
        }
        if report_errors {
            let key_type_string = c.type_to_string_exported(target_info.key_type());
            let source_string = c.type_to_string_exported(source);
            self.report_error(
                c,
                &diagnostics::INDEX_SIGNATURE_FOR_TYPE_0_IS_MISSING_IN_TYPE_1,
                &[&key_type_string, &source_string],
            );
        }
        Ternary::FALSE
    }
}

impl Checker {
    // Return true if the type was inferred from
    //   - an object literal, object type literal, enum type, or a value module and has no call or construct signatures, or
    //   - a JS expando object literal or a rest type, or
    //   - a reverse mapped type with a source for which one of the above is true.
    // relater.go:4656
    pub(crate) fn is_object_type_with_inferable_index(&mut self, t: P<Type>) -> bool {
        if t.flags().intersects(TypeFlags::Intersection) {
            if t.types().iter().any(|&t| !self.is_object_type_with_inferable_index(t)) {
                return false;
            }
            return true;
        }
        t.symbol().is_some_and(|s| {
            s.flags().intersects(
                SymbolFlags::ObjectLiteral
                    | SymbolFlags::TypeLiteral
                    | SymbolFlags::Enum
                    | SymbolFlags::ValueModule,
            ) && !s.flags().intersects(SymbolFlags::Class)
        }) && !self.type_has_call_or_construct_signatures(t)
            || t.object_flags().intersects(ObjectFlags::JSLiteral | ObjectFlags::ObjectRestType)
            || t.object_flags().intersects(ObjectFlags::ReverseMapped)
                && self.is_object_type_with_inferable_index(
                    t.as_reverse_mapped_type().source.get().unwrap(),
                )
    }
}

impl Relater {
    // relater.go:4666
    fn members_related_to_index_info(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target_info: P<IndexInfo>,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        let mut result = Ternary::TRUE;
        let key_type = target_info.key_type();
        let props = if source.flags().intersects(TypeFlags::Intersection) {
            c.get_properties_of_union_or_intersection_type(source)
        } else {
            c.get_properties_of_object_type(source)
        };
        for &prop in props {
            // Skip over ignored JSX and symbol-named members
            if is_ignored_jsx_property(source, prop) {
                continue;
            }
            let literal_type = c.get_literal_type_from_property(
                prop,
                TypeFlags::StringOrNumberLiteralOrUnique,
                false,
            );
            if c.is_applicable_index_type(literal_type, key_type) {
                let prop_type = c.get_non_missing_type_of_symbol(prop);
                let t = if c.exact_optional_property_types
                    || prop_type.flags().intersects(TypeFlags::Undefined)
                    || key_type == c.number_type
                    || !prop.flags().intersects(SymbolFlags::Optional)
                {
                    prop_type
                } else {
                    c.get_type_with_facts(prop_type, TypeFacts::NEUndefined)
                };
                let related = self.is_related_to_ex(
                    c,
                    t,
                    target_info.value_type(),
                    RecursionFlags::Both,
                    report_errors,
                    None, /*headMessage*/
                    intersection_state,
                );
                if related == Ternary::FALSE {
                    if report_errors {
                        let prop_string = c.symbol_to_string(prop);
                        self.report_error(
                            c,
                            &diagnostics::PROPERTY_0_IS_INCOMPATIBLE_WITH_INDEX_SIGNATURE,
                            &[&prop_string],
                        );
                    }
                    return Ternary::FALSE;
                }
                result &= related;
            }
        }
        result = match c.get_index_infos_of_type(source).iter().copied().try_fold(
            result,
            |mut result, info| {
                if c.is_applicable_index_type(info.key_type(), key_type) {
                    let related = self.index_info_related_to(
                        c,
                        info,
                        target_info,
                        report_errors,
                        intersection_state,
                    );
                    if related == Ternary::FALSE {
                        return Err(Ternary::FALSE);
                    }
                    result &= related;
                }
                Ok(result)
            },
        ) {
            Ok(value) => value,
            Err(value) => return value,
        };
        result
    }

    // relater.go:4710
    fn index_info_related_to(
        &self,
        c: &mut Checker,
        source_info: P<IndexInfo>,
        target_info: P<IndexInfo>,
        report_errors: bool,
        intersection_state: IntersectionState,
    ) -> Ternary {
        let related = self.is_related_to_ex(
            c,
            source_info.value_type(),
            target_info.value_type(),
            RecursionFlags::Both,
            report_errors,
            None, /*headMessage*/
            intersection_state,
        );
        if related == Ternary::FALSE && report_errors {
            if source_info.key_type() == target_info.key_type() {
                let key_type_string = c.type_to_string_exported(source_info.key_type());
                self.report_error(
                    c,
                    &diagnostics::X_0_INDEX_SIGNATURES_ARE_INCOMPATIBLE,
                    &[&key_type_string],
                );
            } else {
                let source_key_string = c.type_to_string_exported(source_info.key_type());
                let target_key_string = c.type_to_string_exported(target_info.key_type());
                self.report_error(
                    c,
                    &diagnostics::X_0_AND_1_INDEX_SIGNATURES_ARE_INCOMPATIBLE,
                    &[&source_key_string, &target_key_string],
                );
            }
        }
        related
    }

    // relater.go:4722
    fn index_signatures_identical_to(
        &self,
        c: &mut Checker,
        source: P<Type>,
        target: P<Type>,
    ) -> Ternary {
        let source_infos = c.get_index_infos_of_type(source);
        let target_infos = c.get_index_infos_of_type(target);
        if source_infos.len() != target_infos.len() {
            return Ternary::FALSE;
        }
        if let Some(result) = target_infos.iter().find_map(|&target_info| {
            let source_info = c.get_index_info_of_type(source, target_info.key_type());
            if !(source_info.is_some()
                && self.is_related_to(
                    c,
                    source_info.unwrap().value_type(),
                    target_info.value_type(),
                    RecursionFlags::Both,
                    false,
                ) != Ternary::FALSE
                && source_info.unwrap().is_readonly() == target_info.is_readonly())
            {
                return Some(Ternary::FALSE);
            }

            None
        }) {
            return result;
        }
        Ternary::TRUE
    }

    // relater.go:4737
    fn report_error_results(
        &self,
        c: &mut Checker,
        original_source: P<Type>,
        original_target: P<Type>,
        source: P<Type>,
        target: P<Type>,
        head_message: Option<&'static Message>,
    ) {
        let mut source = source;
        let mut target = target;
        let source_has_base =
            c.get_single_base_for_non_augmenting_subtype(original_source).is_some();
        let target_has_base =
            c.get_single_base_for_non_augmenting_subtype(original_target).is_some();
        if original_source.alias().is_some() || source_has_base {
            source = original_source;
        }
        if original_target.alias().is_some() || target_has_base {
            target = original_target;
        }
        if source.flags().intersects(TypeFlags::Object)
            && target.flags().intersects(TypeFlags::Object)
        {
            self.try_elaborate_array_like_errors(c, source, target, true /*reportErrors*/);
        }
        if source.flags().intersects(TypeFlags::Object)
            && target.flags().intersects(TypeFlags::Primitive)
        {
            self.try_elaborate_errors_for_primitives_and_objects(c, source, target);
        } else if source.symbol().is_some()
            && source.flags().intersects(TypeFlags::Object)
            && c.global_object_type == source
        {
            self.report_error(c, &diagnostics::THE_OBJECT_TYPE_IS_ASSIGNABLE_TO_VERY_FEW_OTHER_TYPES_DID_YOU_MEAN_TO_USE_THE_ANY_TYPE_INSTEAD, &[]);
        } else if source.object_flags().intersects(ObjectFlags::JsxAttributes)
            && target.flags().intersects(TypeFlags::Intersection)
        {
            let target_types = target.types();
            let intrinsic_attributes =
                c.get_jsx_type(JSX_NAMES.intrinsic_attributes, self.error_node.get().unwrap());
            let intrinsic_class_attributes = c
                .get_jsx_type(JSX_NAMES.intrinsic_class_attributes, self.error_node.get().unwrap());
            if !c.is_error_type(intrinsic_attributes)
                && !c.is_error_type(intrinsic_class_attributes)
                && (target_types.contains(&intrinsic_attributes)
                    || target_types.contains(&intrinsic_class_attributes))
            {
                return;
            }
        } else if original_target.flags().intersects(TypeFlags::Intersection)
            && original_target.object_flags_lazy().intersects(ObjectFlags::IsNeverIntersection)
        {
            let mut message = &diagnostics::THE_INTERSECTION_0_WAS_REDUCED_TO_NEVER_BECAUSE_PROPERTY_1_HAS_CONFLICTING_TYPES_IN_SOME_CONSTITUENTS;
            let mut prop: Option<P<Symbol>> = None;
            if let Some(p) = c
                .get_properties_of_union_or_intersection_type(original_target)
                .iter()
                .copied()
                .into_iter()
                .find(|&p| c.is_discriminant_with_never_type(p))
            {
                prop = Some(p);
            }
            if prop.is_none() {
                message = &diagnostics::THE_INTERSECTION_0_WAS_REDUCED_TO_NEVER_BECAUSE_PROPERTY_1_EXISTS_IN_MULTIPLE_CONSTITUENTS_AND_IS_PRIVATE_IN_SOME;
                prop = c
                    .get_properties_of_union_or_intersection_type(original_target)
                    .iter()
                    .copied()
                    .find(|&p| is_conflicting_private_property(p));
            }
            if let Some(prop) = prop {
                let target_string = c.type_to_string_ex(
                    original_target,
                    None, /*enclosingDeclaration*/
                    TypeFormatFlags::NoTypeReduction,
                    None,
                );
                let prop_string = c.symbol_to_string(prop);
                self.report_error(c, message, &[&target_string, &prop_string]);
            }
        }
        self.report_relation_error(c, head_message, source, target);
        if source.flags().intersects(TypeFlags::TypeParameter)
            && source.symbol().is_some_and(|s| !s.declarations().is_empty())
            && c.get_constraint_of_type(source).is_none()
        {
            let synthetic_param = c.clone_type_parameter(source);
            let constraint =
                c.instantiate_type(target, Some(new_simple_type_mapper(source, synthetic_param)));
            synthetic_param.as_type_parameter().constraint.set(Some(constraint));
            if c.has_non_circular_base_constraint(synthetic_param) {
                let target_constraint_string = c.type_to_string_exported(target);
                let first_declaration = source.symbol().unwrap().declarations()[0];
                let diagnostic = new_diagnostic_for_node(
                    Some(first_declaration),
                    Some(&diagnostics::THIS_TYPE_PARAMETER_MIGHT_NEED_AN_EXTENDS_0_CONSTRAINT),
                    &[&target_constraint_string],
                );
                self.related_info.borrow_mut().push(diagnostic);
            }
        }
    }

    // relater.go:4783
    fn report_relation_error(
        &self,
        c: &mut Checker,
        message: Option<&'static Message>,
        source: P<Type>,
        target: P<Type>,
    ) {
        let (source_type, target_type) = c.get_type_names_for_error_display(source, target);
        let mut generalized_source = source;
        let mut generalized_source_type = source_type.clone();
        // Don't generalize on 'never' - we really want the original type
        // to be displayed for use-cases like 'assertNever'.
        if !target.flags().intersects(TypeFlags::Never)
            && is_literal_type(source)
            && !c.type_could_have_top_level_singleton_types(target)
        {
            generalized_source = c.get_base_type_of_literal_type(source);
            generalized_source_type = c.get_type_name_for_error_display(generalized_source);
        }
        // If `target` is of indexed access type (and `source` it is not), we use the object type of `target` for better error reporting
        let target_flags = if target.flags().intersects(TypeFlags::IndexedAccess)
            && !source.flags().intersects(TypeFlags::IndexedAccess)
        {
            target.as_indexed_access_type().object_type.get().unwrap().flags()
        } else {
            target.flags()
        };
        if target_flags.intersects(TypeFlags::TypeParameter)
            && target != c.marker_super_type_for_check
            && target != c.marker_sub_type_for_check
        {
            let constraint = c.get_base_constraint_of_type(target);
            if is_distributed_type_parameter(target)
                && c.is_type_assignable_to(
                    generalized_source,
                    target.as_type_parameter().constraint.get().unwrap(),
                )
            {
                self.report_error(c, &diagnostics::X_0_IS_ONLY_ASSIGNABLE_TO_THE_NON_DISTRIBUTED_1_BUT_1_HAS_BEEN_DISTRIBUTED_HERE, &[&generalized_source_type, &target_type]);
            } else if let Some(constraint) = constraint
                && c.is_type_assignable_to(generalized_source, constraint)
            {
                let constraint_string = c.type_to_string_exported(constraint);
                self.report_error(c, &diagnostics::X_0_IS_ASSIGNABLE_TO_THE_CONSTRAINT_OF_TYPE_1_BUT_1_COULD_BE_INSTANTIATED_WITH_A_DIFFERENT_SUBTYPE_OF_CONSTRAINT_2, &[&generalized_source_type, &target_type, &constraint_string]);
            } else if let Some(constraint) = constraint
                && c.is_type_assignable_to(source, constraint)
            {
                let constraint_string = c.type_to_string_exported(constraint);
                self.report_error(c, &diagnostics::X_0_IS_ASSIGNABLE_TO_THE_CONSTRAINT_OF_TYPE_1_BUT_1_COULD_BE_INSTANTIATED_WITH_A_DIFFERENT_SUBTYPE_OF_CONSTRAINT_2, &[&source_type, &target_type, &constraint_string]);
            } else {
                self.error_chain.set(None); // Only report this error once
                self.report_error(c, &diagnostics::X_0_COULD_BE_INSTANTIATED_WITH_AN_ARBITRARY_TYPE_WHICH_COULD_BE_UNRELATED_TO_1, &[&target_type, &generalized_source_type]);
            }
        }
        let message: &'static Message = match message {
            None => {
                if self.rel() == c.comparable_relation {
                    &diagnostics::TYPE_0_IS_NOT_COMPARABLE_TO_TYPE_1
                } else if source_type == target_type {
                    &diagnostics::TYPE_0_IS_NOT_ASSIGNABLE_TO_TYPE_1_TWO_DIFFERENT_TYPES_WITH_THIS_NAME_EXIST_BUT_THEY_ARE_UNRELATED
                } else if c.exact_optional_property_types
                    && !c.get_exact_optional_unassignable_properties(source, target).is_empty()
                {
                    &diagnostics::TYPE_0_IS_NOT_ASSIGNABLE_TO_TYPE_1_WITH_EXACT_OPTIONAL_PROPERTY_TYPES_COLON_TRUE_CONSIDER_ADDING_UNDEFINED_TO_THE_TYPES_OF_THE_TARGET_S_PROPERTIES
                } else {
                    if source.flags().intersects(TypeFlags::StringLiteral)
                        && target.flags().intersects(TypeFlags::Union)
                    {
                        let suggested_type = c
                            .get_suggested_type_for_nonexistent_string_literal_type(source, target);
                        if let Some(suggested_type) = suggested_type {
                            let suggested_type_string = c.type_to_string_exported(suggested_type);
                            self.report_error(
                                c,
                                &diagnostics::TYPE_0_IS_NOT_ASSIGNABLE_TO_TYPE_1_DID_YOU_MEAN_2,
                                &[&generalized_source_type, &target_type, &suggested_type_string],
                            );
                            return;
                        }
                    }
                    &diagnostics::TYPE_0_IS_NOT_ASSIGNABLE_TO_TYPE_1
                }
            }
            Some(message) => {
                if std::ptr::eq(message, &raw const diagnostics::ARGUMENT_OF_TYPE_0_IS_NOT_ASSIGNABLE_TO_PARAMETER_OF_TYPE_1) && c.exact_optional_property_types && !c.get_exact_optional_unassignable_properties(source, target).is_empty() {
                    &diagnostics::ARGUMENT_OF_TYPE_0_IS_NOT_ASSIGNABLE_TO_PARAMETER_OF_TYPE_1_WITH_EXACT_OPTIONAL_PROPERTY_TYPES_COLON_TRUE_CONSIDER_ADDING_UNDEFINED_TO_THE_TYPES_OF_THE_TARGET_S_PROPERTIES
                } else {
                    message
                }
            }
        };
        let chain_message = self.get_chain_message(c, 0);
        if is_message(chain_message, &diagnostics::OBJECT_LITERAL_MAY_ONLY_SPECIFY_KNOWN_PROPERTIES_AND_0_DOES_NOT_EXIST_IN_TYPE_1)
            || is_message(chain_message, &diagnostics::OBJECT_LITERAL_MAY_ONLY_SPECIFY_KNOWN_PROPERTIES_BUT_0_DOES_NOT_EXIST_IN_TYPE_1_DID_YOU_MEAN_TO_WRITE_2)
        {
            // Suppress if next message is an excess property error
            return;
        } else if is_message(chain_message, &diagnostics::EXCESSIVE_COMPLEXITY_COMPARING_TYPES_0_AND_1) || is_message(chain_message, &diagnostics::THE_TYPE_0_IS_READONLY_AND_CANNOT_BE_ASSIGNED_TO_THE_MUTABLE_TYPE_1) {
            // Suppress if next message is an excessive complexity/stack depth message for source and target or a readonly
            // vs. mutable error for source and target
            if self.chain_args_match(c, &[Some(&generalized_source_type), Some(&target_type)]) {
                return;
            }
        } else if is_message(chain_message, &diagnostics::PROPERTY_0_IS_MISSING_IN_TYPE_1_BUT_REQUIRED_IN_TYPE_2) {
            // Suppress if next message is a missing property message for source and target and we're not
            // reporting on conversion or interface implementation
            if !is_conversion_or_interface_implementation_message(message) && self.chain_args_match(c, &[None, Some(&generalized_source_type), Some(&target_type)]) {
                return;
            }
        } else if is_message(chain_message, &diagnostics::TYPE_0_IS_MISSING_THE_FOLLOWING_PROPERTIES_FROM_TYPE_1_COLON_2_AND_3_MORE)
            || is_message(chain_message, &diagnostics::TYPE_0_IS_MISSING_THE_FOLLOWING_PROPERTIES_FROM_TYPE_1_COLON_2)
        {
            // Suppress if next message is a missing property message for source and target and we're not
            // reporting on conversion or interface implementation
            if !is_conversion_or_interface_implementation_message(message) && self.chain_args_match(c, &[Some(&generalized_source_type), Some(&target_type)]) {
                return;
            }
        }
        self.report_error(c, message, &[&generalized_source_type, &target_type]);
    }

    // relater.go:4864
    fn report_error(&self, c: &mut Checker, message: &'static Message, args: &[&dyn Display]) {
        let mut message = message;
        let mut args = diagnostics::stringify_args(args);
        if std::ptr::eq(message, &raw const diagnostics::TYPES_OF_PROPERTY_0_ARE_INCOMPATIBLE) {
            // Suppress if next message is an excess property error
            let chain_message = self.get_chain_message(c, 0);
            if is_message(chain_message, &diagnostics::OBJECT_LITERAL_MAY_ONLY_SPECIFY_KNOWN_PROPERTIES_AND_0_DOES_NOT_EXIST_IN_TYPE_1)
                || is_message(chain_message, &diagnostics::OBJECT_LITERAL_MAY_ONLY_SPECIFY_KNOWN_PROPERTIES_BUT_0_DOES_NOT_EXIST_IN_TYPE_1_DID_YOU_MEAN_TO_WRITE_2)
            {
                return;
            }
            // Transform a property incompatibility message for property 'x' followed by some elaboration message
            // followed by a signature return type incompatibility message into a single return type incompatibility
            // message for 'x()' or 'x(...)'
            let mut arg = String::new();
            let chain_message = self.get_chain_message(c, 1);
            if is_message(chain_message, &diagnostics::CALL_SIGNATURES_WITH_NO_ARGUMENTS_HAVE_INCOMPATIBLE_RETURN_TYPES_0_AND_1) {
                arg = get_property_name_arg(&args[0]) + "()";
            } else if is_message(chain_message, &diagnostics::CONSTRUCT_SIGNATURES_WITH_NO_ARGUMENTS_HAVE_INCOMPATIBLE_RETURN_TYPES_0_AND_1) {
                arg = "new ".to_string() + &get_property_name_arg(&args[0]) + "()";
            } else if is_message(chain_message, &diagnostics::CALL_SIGNATURE_RETURN_TYPES_0_AND_1_ARE_INCOMPATIBLE) {
                arg = get_property_name_arg(&args[0]) + "(...)";
            } else if is_message(chain_message, &diagnostics::CONSTRUCT_SIGNATURE_RETURN_TYPES_0_AND_1_ARE_INCOMPATIBLE) {
                arg = "new ".to_string() + &get_property_name_arg(&args[0]) + "(...)";
            }
            if !arg.is_empty() {
                message =
                    &diagnostics::THE_TYPES_RETURNED_BY_0_ARE_INCOMPATIBLE_BETWEEN_THESE_TYPES;
                args[0] = arg;
                self.error_chain.set(self.error_chain.get().unwrap().next.unwrap().next);
            }
            // Transform a property incompatibility message for property 'x' followed by some elaboration message
            // followed by a property incompatibility message for property 'y' into a single property incompatibility
            // message for 'x.y'
            let chain_message = self.get_chain_message(c, 1);
            if is_message(chain_message, &diagnostics::TYPES_OF_PROPERTY_0_ARE_INCOMPATIBLE)
                || is_message(
                    chain_message,
                    &diagnostics::THE_TYPES_OF_0_ARE_INCOMPATIBLE_BETWEEN_THESE_TYPES,
                )
                || is_message(
                    chain_message,
                    &diagnostics::THE_TYPES_RETURNED_BY_0_ARE_INCOMPATIBLE_BETWEEN_THESE_TYPES,
                )
            {
                let head = get_property_name_arg(&args[0]);
                let tail =
                    get_property_name_arg(&self.error_chain.get().unwrap().next.unwrap().args[0]);
                let arg = add_to_dotted_name(&head, &tail);
                self.error_chain.set(self.error_chain.get().unwrap().next.unwrap().next);
                if std::ptr::eq(
                    message,
                    &raw const diagnostics::TYPES_OF_PROPERTY_0_ARE_INCOMPATIBLE,
                ) {
                    message = &diagnostics::THE_TYPES_OF_0_ARE_INCOMPATIBLE_BETWEEN_THESE_TYPES;
                }
                self.report_error(c, message, &[&arg]);
                return;
            }
        }
        self.error_chain.set(Some(P::new(ErrorChain {
            next: self.error_chain.get(),
            message,
            args,
        })));
    }
}

// relater.go:4912
pub fn add_to_dotted_name(head: &str, tail: &str) -> String {
    let mut head = head.to_string();
    if head.starts_with("new ") {
        head = "(".to_string() + &head + ")";
    }
    let mut pos = 0;
    loop {
        if tail[pos..].starts_with('(') {
            pos += 1;
        } else if tail[pos..].starts_with("new ") {
            pos += 4;
        } else {
            break;
        }
    }
    let prefix = &tail[..pos];
    let suffix = &tail[pos..];
    if suffix.starts_with('[') {
        return prefix.to_string() + &head + suffix;
    }
    prefix.to_string() + &head + "." + suffix
}

impl Relater {
    // relater.go:4934
    fn get_chain_message(&self, _c: &mut Checker, index: usize) -> Option<&'static Message> {
        let mut index = index;
        let mut e = self.error_chain.get();
        loop {
            let chain = e?;
            if index == 0 {
                return Some(chain.message);
            }
            e = chain.next;
            index -= 1;
        }
    }

    // Return true if the arguments of the first entry on the error chain match the
    // given arguments (where nil acts as a wildcard).
    // relater.go:4950
    // args: &[&dyn Display] -> &[Option<&str>] (Go passes nil as a wildcard; stored chain args are strings).
    fn chain_args_match(&self, _c: &mut Checker, args: &[Option<&str>]) -> bool {
        let chain = self.error_chain.get().unwrap();
        if args.iter().enumerate().any(|(i, a)| {
            if let Some(a) = a
                && *a != chain.args[i]
            {
                return true;
            }

            false
        }) {
            return false;
        }
        true
    }
}

// relater.go:4959
pub fn get_property_name_arg(arg: &str) -> String {
    let s = arg;
    if !s.is_empty()
        && (s.as_bytes()[0] == b'"' || s.as_bytes()[0] == b'\'' || s.as_bytes()[0] == b'`')
    {
        return "[".to_string() + s + "]";
    }
    s.to_string()
}

// relater.go:4967
pub fn is_conversion_or_interface_implementation_message(message: &'static Message) -> bool {
    std::ptr::eq(message, &raw const diagnostics::CLASS_0_INCORRECTLY_IMPLEMENTS_INTERFACE_1)
        || std::ptr::eq(message, &raw const diagnostics::CLASS_0_INCORRECTLY_IMPLEMENTS_CLASS_1_DID_YOU_MEAN_TO_EXTEND_1_AND_INHERIT_ITS_MEMBERS_AS_A_SUBCLASS)
        || std::ptr::eq(message, &raw const diagnostics::CONVERSION_OF_TYPE_0_TO_TYPE_1_MAY_BE_A_MISTAKE_BECAUSE_NEITHER_TYPE_SUFFICIENTLY_OVERLAPS_WITH_THE_OTHER_IF_THIS_WAS_INTENTIONAL_CONVERT_THE_EXPRESSION_TO_UNKNOWN_FIRST)
        || std::ptr::eq(message, &raw const diagnostics::ITS_INSTANCE_TYPE_0_IS_NOT_A_VALID_JSX_ELEMENT)
        || std::ptr::eq(message, &raw const diagnostics::ITS_RETURN_TYPE_0_IS_NOT_A_VALID_JSX_ELEMENT)
        || std::ptr::eq(message, &raw const diagnostics::ITS_ELEMENT_TYPE_0_IS_NOT_A_VALID_JSX_ELEMENT)
}

// relater.go:4976
pub fn chain_depth(chain: Option<P<ErrorChain>>) -> usize {
    let mut chain = chain;
    let mut depth = 0;
    while let Some(c) = chain {
        depth += 1;
        chain = c.next;
    }
    depth
}

impl Checker {
    // An object type S is considered to be derived from an object type T if
    // S is a union type and every constituent of S is derived from T,
    // T is a union type and S is derived from at least one constituent of T, or
    // S is an intersection type and some constituent of S is derived from T, or
    // S is a type variable with a base constraint that is derived from T, or
    // T is {} and S is an object-like type (ensuring {} is less derived than Object), or
    // T is one of the global types Object and Function and S is a subtype of T, or
    // T occurs directly or indirectly in an 'extends' clause of S.
    // Note that this check ignores type parameters and only considers the
    // inheritance hierarchy.
    // relater.go:4995
    pub(crate) fn is_type_derived_from(&mut self, source: P<Type>, target: P<Type>) -> bool {
        if source.flags().intersects(TypeFlags::Union) {
            !(source
                .as_union_type()
                .types
                .get()
                .iter()
                .any(|&t| !self.is_type_derived_from(t, target)))
        } else if target.flags().intersects(TypeFlags::Union) {
            target.as_union_type().types.get().iter().any(|&t| self.is_type_derived_from(source, t))
        } else if source.flags().intersects(TypeFlags::Intersection) {
            source
                .as_intersection_type()
                .types
                .get()
                .iter()
                .any(|&t| self.is_type_derived_from(t, target))
        } else if source.flags().intersects(TypeFlags::InstantiableNonPrimitive) {
            let constraint = match self.get_base_constraint_of_type(source) {
                Some(constraint) => constraint,
                None => self.unknown_type,
            };
            self.is_type_derived_from(constraint, target)
        } else if self.is_empty_anonymous_object_type(target) {
            source.flags().intersects(TypeFlags::Object | TypeFlags::NonPrimitive)
        } else if target == self.global_object_type {
            source.flags().intersects(TypeFlags::Object | TypeFlags::NonPrimitive)
                && !self.is_empty_anonymous_object_type(source)
        } else if target == self.global_function_type {
            source.flags().intersects(TypeFlags::Object) && self.is_function_object_type(source)
        } else {
            let target_type = Self::get_target_type(target).unwrap();
            self.has_base_type(source, Some(target_type))
                || (self.is_array_type(target)
                    && !self.is_readonly_array_type(target)
                    && self.is_type_derived_from(source, self.global_readonly_array_type))
        }
    }

    // relater.go:5026
    fn is_distribution_dependent(&mut self, root: P<ConditionalRoot>) -> bool {
        let check_type = root.check_type.get().unwrap();
        let node = root.node.get().unwrap().as_conditional_type_node();
        root.is_distributive.get()
            && (self.is_type_parameter_possibly_referenced(check_type, node.true_type)
                || self.is_type_parameter_possibly_referenced(check_type, node.false_type))
    }
}

impl Relater {
    // relater.go:5030
    #[expect(
        clippy::unused_self,
        reason = "The Go initialization or tracing hook is retained in the same method call sequence."
    )]
    fn trace_unions_or_intersections_too_large(
        &self,
        _c: &mut Checker,
        _source: P<Type>,
        _target: P<Type>,
    ) {
        // Only reports to the tracer, which is not ported.
    }
}

/// The empty `excludedProperties` (tsrs: shared, instead of a set made and dropped per structural comparison).
static NO_EXCLUDED_PROPERTIES: Set<String> =
    Set { m: rustc_hash::FxHashSet::with_hasher(rustc_hash::FxBuildHasher) };
