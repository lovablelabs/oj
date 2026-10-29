use std::collections::hash_map::Entry;
use std::fmt::Display;

use rustc_hash::FxHashMap;
use tsrs_ast as ast;
use tsrs_ast::*;
use tsrs_core::*;
use tsrs_diagnostics as diagnostics;
use tsrs_diagnostics::Message;

use crate::*;

// Non-function declarations in relater.go:1-2579 live in relater_types.rs.

/// Reborrows an optional `ErrorReporter` so it can be passed on more than once.
fn reborrow_reporter<'b>(r: &'b mut Option<ErrorReporter<'_>>) -> Option<ErrorReporter<'b>> {
    match r {
        Some(f) => Some(&mut **f),
        None => None,
    }
}

/// Go `chain.args...`: stored diagnostic arguments are pre-formatted strings.
fn string_args(args: &[String]) -> Vec<&dyn Display> {
    args.iter().map(|a| a as &dyn Display).collect()
}

// relater.go:94
// This function exists to constrain the types of values that can be used as recursion IDs.
fn as_recursion_id<T: Into<RecursionId>>(value: T) -> RecursionId {
    value.into()
}

const RELATION_RESULT_MASK: u64 = (1 << RELATION_RESULT_BITS) - 1;

#[inline]
fn pair_slot_hash(key: u64) -> u64 {
    use std::hash::BuildHasher;
    rustc_hash::FxBuildHasher.hash_one(key)
}

impl Relation {
    // relater.go:102
    pub(crate) fn lookup(&self, key: RelationKey) -> RelationComparisonResult {
        let bits = match key {
            RelationKey::Pair(k) => self
                .pairs
                .borrow()
                .find(pair_slot_hash(k), |&slot| slot >> RELATION_RESULT_BITS == k)
                .map(|&slot| slot & RELATION_RESULT_MASK),
            RelationKey::Hashed(k) => {
                self.hashed.borrow().get(&PackedHashKey::from(k)).map(|&bits| bits as u64)
            }
        };
        bits.map_or(RelationComparisonResult::None, |bits| {
            RelationComparisonResult::from_bits_retain(bits as u32)
        })
    }

    // relater.go:106
    pub(crate) fn set(&self, key: RelationKey, result: RelationComparisonResult) {
        let bits = result.bits() as u64;
        assert!(bits <= RELATION_RESULT_MASK, "relation result bits above {RELATION_RESULT_BITS}");
        match key {
            RelationKey::Pair(k) => {
                let slot = (k << RELATION_RESULT_BITS) | bits;
                let mut pairs = self.pairs.borrow_mut();
                let hash = pair_slot_hash(k);
                match pairs.find_mut(hash, |&slot| slot >> RELATION_RESULT_BITS == k) {
                    Some(existing) => *existing = slot,
                    None => {
                        pairs.insert_unique(hash, slot, |&slot| {
                            pair_slot_hash(slot >> RELATION_RESULT_BITS)
                        });
                    }
                }
            }
            RelationKey::Hashed(k) => {
                self.hashed.borrow_mut().insert(PackedHashKey::from(k), bits as u8);
            }
        }
    }

    // tsrs-only (`--maxMemory`, notes/mem-recycle-checkers.md): drops every cached result and frees the tables.
    pub(crate) fn clear(&self) {
        *self.pairs.borrow_mut() = hashbrown::HashTable::new();
        *self.hashed.borrow_mut() = FxHashMap::default();
    }

    // relater.go:113
    pub(crate) fn size(&self) -> usize {
        self.pairs.borrow().len() + self.hashed.borrow().len()
    }
}

impl Checker {
    // relater.go:117
    pub(crate) fn is_type_identical_to(&mut self, source: P<Type>, target: P<Type>) -> bool {
        let relation = self.identity_relation;
        self.is_type_related_to(source, target, relation)
    }

    // relater.go:121
    pub(crate) fn compare_types_identical(&mut self, source: P<Type>, target: P<Type>) -> Ternary {
        let relation = self.identity_relation;
        if self.is_type_related_to(source, target, relation) {
            return Ternary::TRUE;
        }
        Ternary::FALSE
    }

    // relater.go:128
    fn compare_types_assignable_simple(&mut self, source: P<Type>, target: P<Type>) -> Ternary {
        let relation = self.assignable_relation;
        if self.is_type_related_to(source, target, relation) {
            return Ternary::TRUE;
        }
        Ternary::FALSE
    }

    // relater.go:135
    pub(crate) fn compare_types_assignable_worker(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        report_errors: bool,
    ) -> Ternary {
        let _ = report_errors;
        let relation = self.assignable_relation;
        if self.is_type_related_to(source, target, relation) {
            return Ternary::TRUE;
        }
        Ternary::FALSE
    }

    // relater.go:142
    fn compare_types_subtype_of(&mut self, source: P<Type>, target: P<Type>) -> Ternary {
        let relation = self.subtype_relation;
        if self.is_type_related_to(source, target, relation) {
            return Ternary::TRUE;
        }
        Ternary::FALSE
    }

    // relater.go:149
    pub(crate) fn is_type_assignable_to(&mut self, source: P<Type>, target: P<Type>) -> bool {
        let relation = self.assignable_relation;
        self.is_type_related_to(source, target, relation)
    }

    // relater.go:153
    pub(crate) fn is_type_subtype_of(&mut self, source: P<Type>, target: P<Type>) -> bool {
        let relation = self.subtype_relation;
        self.is_type_related_to(source, target, relation)
    }

    // relater.go:157
    pub(crate) fn is_type_strict_subtype_of(&mut self, source: P<Type>, target: P<Type>) -> bool {
        let relation = self.strict_subtype_relation;
        self.is_type_related_to(source, target, relation)
    }

    // relater.go:161
    pub(crate) fn is_type_comparable_to(&mut self, source: P<Type>, target: P<Type>) -> bool {
        let relation = self.comparable_relation;
        self.is_type_related_to(source, target, relation)
    }

    // relater.go:165
    pub(crate) fn are_types_comparable(&mut self, type1: P<Type>, type2: P<Type>) -> bool {
        self.is_type_comparable_to(type1, type2) || self.is_type_comparable_to(type2, type1)
    }

    // relater.go:169
    pub(crate) fn is_type_related_to(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        relation: P<Relation>,
    ) -> bool {
        let mut source = source;
        let mut target = target;
        if is_fresh_literal_type(source) {
            source = source.as_literal_type().regular_type.get().unwrap();
        }
        if is_fresh_literal_type(target) {
            target = target.as_literal_type().regular_type.get().unwrap();
        }
        if source == target {
            return true;
        }
        // Two different regular literals of one kind (not enum members) are related by no relation: narrowing a
        // union of string literals by a literal asks this of every constituent.
        let flags = source.flags();
        if flags == target.flags()
            && matches!(
                flags,
                TypeFlags::StringLiteral | TypeFlags::NumberLiteral | TypeFlags::BigIntLiteral
            )
        {
            return false;
        }
        if relation != self.identity_relation {
            if relation == self.comparable_relation
                && !target.flags().intersects(TypeFlags::Never)
                && self.is_simple_type_related_to(target, source, relation, None)
                || self.is_simple_type_related_to(source, target, relation, None)
            {
                return true;
            }
        } else if !(source.flags() | target.flags()).intersects(
            TypeFlags::UnionOrIntersection
                | TypeFlags::IndexedAccess
                | TypeFlags::Conditional
                | TypeFlags::Substitution,
        ) {
            // We have excluded types that may simplify to other forms, so types must have identical flags
            if source.flags() != target.flags() {
                return false;
            }
            if source.flags().intersects(TypeFlags::Singleton) {
                return true;
            }
        }
        if source.flags().intersects(TypeFlags::Object)
            && target.flags().intersects(TypeFlags::Object)
        {
            let is_identity = relation == self.identity_relation;
            let (id, _) =
                get_relation_key(self, source, target, IntersectionState::None, is_identity, false);
            let related = relation.lookup(id);
            crate::sitecount::hit(
                "relation cache (isTypeRelatedTo)",
                if related == RelationComparisonResult::None { "miss" } else { "hit" },
            );
            if related != RelationComparisonResult::None {
                return related.intersects(RelationComparisonResult::Succeeded);
            }
        }
        if source.flags().intersects(TypeFlags::StructuredOrInstantiable)
            || target.flags().intersects(TypeFlags::StructuredOrInstantiable)
        {
            if relation == self.identity_relation
                && (source.flags() | target.flags()).intersects(
                    TypeFlags::UnionOrIntersection
                        | TypeFlags::IndexedAccess
                        | TypeFlags::Conditional
                        | TypeFlags::Substitution,
                )
                && !(source.flags().intersects(TypeFlags::Object)
                    && target.flags().intersects(TypeFlags::Primitive))
            {
                // tsrs-only (notes/perf-checker-64.md): what is_related_to_ex decides for the identity relation before
                // it recurses, without borrowing a relater. Inference asks whether an argument type is identical to
                // each constituent of a union target (infer_from_matching_types); for a union of deferred indexed
                // accesses every answer is "no, different kinds", and this is the whole cost of that question.
                let s = self.get_normalized_type(source, false /*writing*/);
                let t = self.get_normalized_type(target, true /*writing*/);
                if s == t {
                    return true;
                }
                if s.flags() != t.flags() {
                    return false;
                }
                if s.flags().intersects(TypeFlags::Singleton) {
                    return true;
                }
            }
            return self.check_type_related_to(source, target, relation, None /*errorNode*/);
        }
        false
    }

    // relater.go:205
    /// tsrs: two enum members of one literal kind, and two structured types, answered inline before the worker's
    /// chain of flag tests. Narrowing `kind === SyntaxKind.X` asks the comparable relation about a member against
    /// every other member of the enum (three quarters of the calls on the Compiler-Unions benchmark); of the
    /// worker's tests only the enum-literal one can answer such a pair, with the values equal and the enums
    /// related, since neither type is a union, a base primitive, `any`, `unknown`, `never`, null or undefined.
    // The profiled enum-member fast path otherwise retains an out-of-line register-save frame.
    #[inline(always)]
    pub(crate) fn is_simple_type_related_to(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        relation: P<Relation>,
        error_reporter: Option<ErrorReporter<'_>>,
    ) -> bool {
        const NUMBER_MEMBER: TypeFlags = TypeFlags::NumberLiteral.union(TypeFlags::EnumLiteral);
        const STRING_MEMBER: TypeFlags = TypeFlags::StringLiteral.union(TypeFlags::EnumLiteral);

        // Two structured or instantiable types (most of the other calls, from is_type_related_to and
        // is_related_to_ex) pass none of the worker's flag tests; only the last one, a target union of undefined,
        // null and {}, can answer true.
        const STRUCTURED: TypeFlags = TypeFlags::Object
            .union(TypeFlags::Union)
            .union(TypeFlags::Intersection)
            .union(TypeFlags::TypeParameter)
            .union(TypeFlags::Index)
            .union(TypeFlags::IndexedAccess)
            .union(TypeFlags::Conditional)
            .union(TypeFlags::Substitution);

        let s = source.flags();
        let t = target.flags();
        if s == t && (s == NUMBER_MEMBER || s == STRING_MEMBER) {
            return source.as_literal_type().value_equals(target.as_literal_type())
                && self.is_enum_type_related_to(
                    source.symbol().unwrap(),
                    target.symbol().unwrap(),
                    error_reporter,
                );
        }
        if STRUCTURED.contains(s | t) {
            return t.intersects(TypeFlags::Union)
                && (relation == self.assignable_relation || relation == self.comparable_relation)
                && self.is_unknown_like_union_type(target);
        }
        self.is_simple_type_related_to_worker(source, target, relation, error_reporter)
    }

    #[inline(never)]
    fn is_simple_type_related_to_worker(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        relation: P<Relation>,
        error_reporter: Option<ErrorReporter<'_>>,
    ) -> bool {
        let mut error_reporter = error_reporter;
        let s = source.flags();
        let t = target.flags();
        if t.intersects(TypeFlags::Any)
            || s.intersects(TypeFlags::Never)
            || source == self.wildcard_type
        {
            return true;
        }
        if t.intersects(TypeFlags::Unknown)
            && !(relation == self.strict_subtype_relation && s.intersects(TypeFlags::Any))
        {
            return true;
        }
        if t.intersects(TypeFlags::Never) {
            return false;
        }
        if s.intersects(TypeFlags::StringLike) && t.intersects(TypeFlags::String) {
            return true;
        }
        if s.intersects(TypeFlags::StringLiteral)
            && s.intersects(TypeFlags::EnumLiteral)
            && t.intersects(TypeFlags::StringLiteral)
            && !t.intersects(TypeFlags::EnumLiteral)
            && source.as_literal_type().value_equals(target.as_literal_type())
        {
            return true;
        }
        if s.intersects(TypeFlags::NumberLike) && t.intersects(TypeFlags::Number) {
            return true;
        }
        if s.intersects(TypeFlags::NumberLiteral)
            && s.intersects(TypeFlags::EnumLiteral)
            && t.intersects(TypeFlags::NumberLiteral)
            && !t.intersects(TypeFlags::EnumLiteral)
            && source.as_literal_type().value_equals(target.as_literal_type())
        {
            return true;
        }
        if s.intersects(TypeFlags::BigIntLike) && t.intersects(TypeFlags::BigInt) {
            return true;
        }
        if s.intersects(TypeFlags::BooleanLike) && t.intersects(TypeFlags::Boolean) {
            return true;
        }
        if s.intersects(TypeFlags::ESSymbolLike) && t.intersects(TypeFlags::ESSymbol) {
            return true;
        }
        if s.intersects(TypeFlags::Enum)
            && t.intersects(TypeFlags::Enum)
            && source.symbol().unwrap().name() == target.symbol().unwrap().name()
            && self.is_enum_type_related_to(
                source.symbol().unwrap(),
                target.symbol().unwrap(),
                reborrow_reporter(&mut error_reporter),
            )
        {
            return true;
        }
        if s.intersects(TypeFlags::EnumLiteral) && t.intersects(TypeFlags::EnumLiteral) {
            if s.intersects(TypeFlags::Union)
                && t.intersects(TypeFlags::Union)
                && self.is_enum_type_related_to(
                    source.symbol().unwrap(),
                    target.symbol().unwrap(),
                    reborrow_reporter(&mut error_reporter),
                )
            {
                return true;
            }
            if s.intersects(TypeFlags::Literal)
                && t.intersects(TypeFlags::Literal)
                && source.as_literal_type().value_equals(target.as_literal_type())
                && self.is_enum_type_related_to(
                    source.symbol().unwrap(),
                    target.symbol().unwrap(),
                    reborrow_reporter(&mut error_reporter),
                )
            {
                return true;
            }
        }
        // In non-strictNullChecks mode, `undefined` and `null` are assignable to anything except `never`.
        // Since unions and intersections may reduce to `never`, we exclude them here.
        if s.intersects(TypeFlags::Undefined)
            && (!self.strict_null_checks && !t.intersects(TypeFlags::UnionOrIntersection)
                || t.intersects(TypeFlags::Undefined | TypeFlags::Void))
        {
            return true;
        }
        if s.intersects(TypeFlags::Null)
            && (!self.strict_null_checks && !t.intersects(TypeFlags::UnionOrIntersection)
                || t.intersects(TypeFlags::Null))
        {
            return true;
        }
        if s.intersects(TypeFlags::Object)
            && t.intersects(TypeFlags::NonPrimitive)
            && !(relation == self.strict_subtype_relation
                && self.is_empty_anonymous_object_type(source)
                && !source.object_flags().intersects(ObjectFlags::FreshLiteral))
        {
            return true;
        }
        if relation == self.assignable_relation || relation == self.comparable_relation {
            if s.intersects(TypeFlags::Any) {
                return true;
            }
            // Type number is assignable to any computed numeric enum type or any numeric enum literal type, and
            // a numeric literal type is assignable any computed numeric enum type or any numeric enum literal type
            // with a matching value. These rules exist such that enums can be used for bit-flag purposes.
            if s.intersects(TypeFlags::Number)
                && (t.intersects(TypeFlags::Enum)
                    || t.intersects(TypeFlags::NumberLiteral)
                        && t.intersects(TypeFlags::EnumLiteral))
            {
                return true;
            }
            if s.intersects(TypeFlags::NumberLiteral)
                && !s.intersects(TypeFlags::EnumLiteral)
                && (t.intersects(TypeFlags::Enum)
                    || t.intersects(TypeFlags::NumberLiteral)
                        && t.intersects(TypeFlags::EnumLiteral)
                        && source.as_literal_type().value_equals(target.as_literal_type()))
            {
                return true;
            }
            // Anything is assignable to a union containing undefined, null, and {}
            if self.is_unknown_like_union_type(target) {
                return true;
            }
        }
        false
    }

    // relater.go:281
    fn is_enum_type_related_to(
        &mut self,
        source: P<Symbol>,
        target: P<Symbol>,
        error_reporter: Option<ErrorReporter<'_>>,
    ) -> bool {
        let mut error_reporter = error_reporter;
        // core.IfElse evaluates both branches.
        let source_parent = self.get_parent_of_symbol(source);
        let source_symbol = if source.flags().intersects(SymbolFlags::EnumMember) {
            source_parent.unwrap()
        } else {
            source
        };
        let target_parent = self.get_parent_of_symbol(target);
        let target_symbol = if target.flags().intersects(SymbolFlags::EnumMember) {
            target_parent.unwrap()
        } else {
            target
        };
        if source_symbol == target_symbol {
            return true;
        }
        if source_symbol.name() != target_symbol.name()
            || !source_symbol.flags().intersects(SymbolFlags::RegularEnum)
            || !target_symbol.flags().intersects(SymbolFlags::RegularEnum)
        {
            return false;
        }
        let key = EnumRelationKey {
            source_id: get_symbol_id(source_symbol),
            target_id: get_symbol_id(target_symbol),
        };
        let entry = self.enum_relation.get(&key).copied().unwrap_or(RelationComparisonResult::None);
        if entry != RelationComparisonResult::None
            && !(entry.intersects(RelationComparisonResult::Failed) && error_reporter.is_some())
        {
            return entry.intersects(RelationComparisonResult::Succeeded);
        }
        let target_enum_type = self.get_type_of_symbol(target_symbol);
        let source_enum_type = self.get_type_of_symbol(source_symbol);
        for source_property in self.get_properties_of_type(source_enum_type).iter().copied() {
            if source_property.flags().intersects(SymbolFlags::EnumMember) {
                let target_property =
                    self.get_property_of_type(target_enum_type, source_property.name());
                if target_property.is_none_or(|p| !p.flags().intersects(SymbolFlags::EnumMember)) {
                    if let Some(reporter) = error_reporter.as_mut() {
                        let a = self.symbol_to_string(source_property);
                        let declared = self.get_declared_type_of_symbol(target_symbol);
                        let b = self.type_to_string_ex(
                            declared,
                            None, /*enclosingDeclaration*/
                            TypeFormatFlags::UseFullyQualifiedType,
                            None,
                        );
                        reporter(self, &diagnostics::PROPERTY_0_IS_MISSING_IN_TYPE_1, &[&a, &b]);
                    }
                    self.enum_relation.insert(key, RelationComparisonResult::Failed);
                    return false;
                }
                let target_property = target_property.unwrap();
                let source_value = self
                    .get_enum_member_value(
                        get_declaration_of_kind(source_property, Kind::EnumMember).unwrap(),
                    )
                    .value;
                let target_value = self
                    .get_enum_member_value(
                        get_declaration_of_kind(target_property, Kind::EnumMember).unwrap(),
                    )
                    .value;
                if source_value != target_value {
                    // If we have 2 enums with *known* values that differ, they are incompatible.
                    if let (Some(sv), Some(tv)) = (source_value, target_value) {
                        if let Some(reporter) = error_reporter.as_mut() {
                            let a = self.symbol_to_string(target_symbol);
                            let b = self.symbol_to_string(target_property);
                            let c = Self::value_to_string(tv);
                            let d = Self::value_to_string(sv);
                            reporter(
                                self,
                                &diagnostics::EACH_DECLARATION_OF_0_1_DIFFERS_IN_ITS_VALUE_WHERE_2_WAS_EXPECTED_BUT_3_WAS_GIVEN,
                                &[&a, &b, &c, &d],
                            );
                        }
                        self.enum_relation.insert(key, RelationComparisonResult::Failed);
                        return false;
                    }
                    // At this point we know that at least one of the values is 'undefined'.
                    // This may mean that we have an opaque member from an ambient enum declaration,
                    // or that we were not able to calculate it (which is basically an error).
                    //
                    // Either way, we can assume that it's numeric.
                    // If the other is a string, we have a mismatch in types.
                    let source_is_string = matches!(source_value, Some(LiteralValue::String(_)));
                    let target_is_string = matches!(target_value, Some(LiteralValue::String(_)));
                    if source_is_string || target_is_string {
                        if let Some(reporter) = error_reporter.as_mut() {
                            let known_string_value = source_value.or(target_value).unwrap();
                            let a = self.symbol_to_string(target_symbol);
                            let b = self.symbol_to_string(target_property);
                            let c = Self::value_to_string(known_string_value);
                            reporter(
                                self,
                                &diagnostics::ONE_VALUE_OF_0_1_IS_THE_STRING_2_AND_THE_OTHER_IS_ASSUMED_TO_BE_AN_UNKNOWN_NUMERIC_VALUE,
                                &[&a, &b, &c],
                            );
                        }
                        self.enum_relation.insert(key, RelationComparisonResult::Failed);
                        return false;
                    }
                }
            }
        }
        self.enum_relation.insert(key, RelationComparisonResult::Succeeded);
        true
    }

    // relater.go:339
    pub(crate) fn check_type_assignable_to(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        error_node: Option<P<Node>>,
        head_message: Option<&'static Message>,
    ) -> bool {
        let relation = self.assignable_relation;
        self.check_type_related_to_ex(source, target, relation, error_node, head_message, None)
    }

    // relater.go:343
    pub(crate) fn check_type_assignable_to_ex(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        error_node: Option<P<Node>>,
        head_message: Option<&'static Message>,
        diagnostic_output: &mut Vec<P<Diagnostic>>,
    ) -> bool {
        let relation = self.assignable_relation;
        self.check_type_related_to_ex(
            source,
            target,
            relation,
            error_node,
            head_message,
            Some(diagnostic_output),
        )
    }

    // relater.go:347
    pub(crate) fn check_type_comparable_to(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        error_node: P<Node>,
        head_message: Option<&'static Message>,
    ) -> bool {
        let relation = self.comparable_relation;
        self.check_type_related_to_ex(
            source,
            target,
            relation,
            Some(error_node),
            head_message,
            None,
        )
    }

    // relater.go:351
    pub(crate) fn check_type_related_to(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        relation: P<Relation>,
        error_node: Option<P<Node>>,
    ) -> bool {
        self.check_type_related_to_ex(source, target, relation, error_node, None, None)
    }

    // relater.go:358
    // Check that source is related to target according to the given relation. When errorNode is non-nil, errors are
    // reported to the checker's diagnostic collection or through diagnosticOutput when non-nil. Callers can assume that
    // this function only reports zero or one error to diagnosticOutput (unlike checkTypeRelatedToAndOptionallyElaborate).
    pub(crate) fn check_type_related_to_ex(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        relation: P<Relation>,
        error_node: Option<P<Node>>,
        head_message: Option<&'static Message>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> bool {
        let mut error_node = error_node;
        let r = self.get_relater();
        r.relation.set(Some(relation));
        r.error_node.set(error_node);
        r.relation_count.set(
            (16_000_000
                - isize::try_from(relation.size())
                    .expect("relation size exceeds signed budget range"))
                / 8,
        );
        let result = r.is_related_to_ex(
            self,
            source,
            target,
            RecursionFlags::Both,
            error_node.is_some(), /*reportErrors*/
            head_message,
            IntersectionState::None,
        );
        if r.overflow.get() {
            // Record this relation as having failed such that we don't attempt the overflowing operation again.
            let is_identity = relation == self.identity_relation;
            let (id, _) = get_relation_key(
                self,
                source,
                target,
                IntersectionState::None,
                is_identity,
                false, /*ignoreConstraints*/
            );
            relation.set(
                id,
                RelationComparisonResult::Failed | RelationComparisonResult::ComplexityOverflow,
            );
            if error_node.is_none() {
                error_node = self.current_node;
            }
            let s = self.type_to_string(source, None);
            let t = self.type_to_string(target, None);
            let diagnostic = new_diagnostic_for_node(
                error_node,
                Some(&diagnostics::EXCESSIVE_COMPLEXITY_COMPARING_TYPES_0_AND_1),
                &[&s, &t],
            );
            self.report_diagnostic(Some(diagnostic), diagnostic_output);
        } else if r.error_chain.get().is_some() {
            // Check if we should issue an extra diagnostic to produce a quickfix for a slightly incorrect import statement
            if head_message.is_some()
                && error_node.is_some()
                && result == Ternary::FALSE
                && source.symbol().is_some()
                && self.export_type_links.has(source.symbol().unwrap())
            {
                let links = self.export_type_links.get(source.symbol().unwrap());
                if let Some(originating_import) = links.originating_import.get()
                    && !is_import_call(originating_import)
                {
                    let t = self.get_type_of_symbol(links.target.get().unwrap());
                    let helpful_retry =
                        self.check_type_related_to(t, target, relation /*errorNode*/, None);
                    if helpful_retry {
                        // Likely an incorrect import. Issue a helpful diagnostic to produce a quickfix to change the import
                        r.related_info.borrow_mut().push(create_diagnostic_for_node(
                                Some(originating_import),
                                &diagnostics::TYPE_ORIGINATES_AT_THIS_IMPORT_A_NAMESPACE_STYLE_IMPORT_CANNOT_BE_CALLED_OR_CONSTRUCTED_AND_WILL_CAUSE_A_FAILURE_AT_RUNTIME_CONSIDER_USING_A_DEFAULT_IMPORT_OR_IMPORT_REQUIRE_HERE_INSTEAD,
                                &[],
                            ));
                    }
                }
            }
            let related_info = r.related_info.borrow().clone();
            let diagnostic = create_diagnostic_chain_from_error_chain(
                r.error_chain.get(),
                r.error_node.get().unwrap(),
                &related_info,
            );
            self.report_diagnostic(diagnostic, diagnostic_output);
        }
        self.put_relater(r);
        result != Ternary::FALSE
    }
}

// relater.go:400
fn create_diagnostic_chain_from_error_chain(
    chain: Option<P<ErrorChain>>,
    error_node: P<Node>,
    related_info: &[P<Diagnostic>],
) -> Option<P<Diagnostic>> {
    let mut chain = chain;
    while let Some(ch) = chain {
        if !ch.message.elided_in_compatibility_pyramid() {
            break;
        }
        chain = ch.next;
    }
    let chain = chain?;
    let next = create_diagnostic_chain_from_error_chain(chain.next, error_node, related_info);
    let args = string_args(&chain.args);
    match next {
        None => Some(
            new_diagnostic_for_node(Some(error_node), Some(chain.message), &args)
                .set_related_info(related_info),
        ),
        Some(next) => Some(ast::new_diagnostic_chain(next, chain.message, &args)),
    }
}

impl Checker {
    // relater.go:414
    pub(crate) fn report_diagnostic(
        &mut self,
        diagnostic: Option<P<Diagnostic>>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) {
        if let Some(diagnostic) = diagnostic {
            if let Some(diagnostic_output) = diagnostic_output {
                diagnostic_output.push(diagnostic);
            } else {
                self.add_diagnostic(diagnostic);
            }
        }
    }

    // relater.go:424
    pub(crate) fn check_type_assignable_to_and_optionally_elaborate(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        error_node: Option<P<Node>>,
        expr: Option<P<Node>>,
        head_message: Option<&'static Message>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> bool {
        let relation = self.assignable_relation;
        self.check_type_related_to_and_optionally_elaborate(
            source,
            target,
            relation,
            error_node,
            expr,
            head_message,
            diagnostic_output,
        )
    }

    // relater.go:428
    pub(crate) fn check_type_related_to_and_optionally_elaborate(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        relation: P<Relation>,
        error_node: Option<P<Node>>,
        expr: Option<P<Node>>,
        head_message: Option<&'static Message>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> bool {
        let mut diagnostic_output = diagnostic_output;
        if self.is_type_related_to(source, target, relation) {
            return true;
        }
        if error_node.is_some()
            && !self.elaborate_error(
                expr,
                source,
                target,
                relation,
                head_message,
                diagnostic_output.as_deref_mut(),
            )
        {
            return self.check_type_related_to_ex(
                source,
                target,
                relation,
                error_node,
                head_message,
                diagnostic_output,
            );
        }
        false
    }

    // relater.go:438
    pub(crate) fn elaborate_error(
        &mut self,
        node: Option<P<Node>>,
        source: P<Type>,
        target: P<Type>,
        relation: P<Relation>,
        head_message: Option<&'static Message>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> bool {
        let mut diagnostic_output = diagnostic_output;
        let Some(node) = node else {
            return false;
        };
        if Self::is_or_has_generic_conditional(target) {
            return false;
        }
        if self.compiler_options.no_check.is_true() {
            return false;
        }
        if self.elaborate_did_you_mean_to_call_or_construct(
            node,
            source,
            target,
            relation,
            SignatureKind::Construct,
            head_message,
            diagnostic_output.as_deref_mut(),
        ) || self.elaborate_did_you_mean_to_call_or_construct(
            node,
            source,
            target,
            relation,
            SignatureKind::Call,
            head_message,
            diagnostic_output.as_deref_mut(),
        ) {
            return true;
        }
        match node.kind() {
            Kind::AsExpression | Kind::JsxExpression | Kind::ParenthesizedExpression => {
                if node.kind() == Kind::AsExpression && !is_const_assertion(node) {
                    return false;
                }
                return self.elaborate_error(
                    node.expression(),
                    source,
                    target,
                    relation,
                    head_message,
                    diagnostic_output,
                );
            }
            Kind::BinaryExpression => match node.as_binary_expression().operator_token.kind() {
                Kind::EqualsToken | Kind::CommaToken => {
                    return self.elaborate_error(
                        Some(node.as_binary_expression().right()),
                        source,
                        target,
                        relation,
                        head_message,
                        diagnostic_output,
                    );
                }
                _ => {}
            },
            Kind::ObjectLiteralExpression => {
                return self.elaborate_object_literal(
                    node,
                    source,
                    target,
                    relation,
                    diagnostic_output,
                );
            }
            Kind::ArrayLiteralExpression => {
                return self.elaborate_array_literal(
                    node,
                    source,
                    target,
                    relation,
                    diagnostic_output,
                );
            }
            Kind::ArrowFunction => {
                return self.elaborate_arrow_function(
                    node,
                    source,
                    target,
                    relation,
                    diagnostic_output,
                );
            }
            Kind::JsxAttributes => {
                return self.elaborate_jsx_components(
                    node,
                    source,
                    target,
                    relation,
                    diagnostic_output,
                );
            }
            _ => {}
        }
        false
    }

    // relater.go:474
    fn is_or_has_generic_conditional(t: P<Type>) -> bool {
        t.flags().intersects(TypeFlags::Conditional)
            || (t.flags().intersects(TypeFlags::Intersection)
                && t.types().iter().any(|&t| Self::is_or_has_generic_conditional(t)))
    }

    // relater.go:478
    fn elaborate_did_you_mean_to_call_or_construct(
        &mut self,
        node: P<Node>,
        source: P<Type>,
        target: P<Type>,
        relation: P<Relation>,
        kind: SignatureKind,
        head_message: Option<&'static Message>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> bool {
        let signatures = self.get_signatures_of_type(source, kind);
        let mut some = false;
        for &s in signatures {
            let return_type = self.get_return_type_of_signature(s);
            if !return_type.flags().intersects(TypeFlags::Any | TypeFlags::Never)
                && self.check_type_related_to(
                    return_type,
                    target,
                    relation,
                    None, /*errorNode*/
                )
            {
                some = true;
                break;
            }
        }
        if some {
            let mut diags: Vec<P<Diagnostic>> = Vec::new();
            if !self.check_type_related_to_ex(
                source,
                target,
                relation,
                Some(node),
                head_message,
                Some(&mut diags),
            ) {
                let diagnostic = diags[0];
                let message = if kind == SignatureKind::Construct {
                    &diagnostics::DID_YOU_MEAN_TO_USE_NEW_WITH_THIS_EXPRESSION
                } else {
                    &diagnostics::DID_YOU_MEAN_TO_CALL_THIS_EXPRESSION
                };
                self.report_diagnostic(
                    Some(diagnostic.add_related_info(create_diagnostic_for_node(
                        Some(node),
                        message,
                        &[],
                    ))),
                    diagnostic_output,
                );
                return true;
            }
        }
        false
    }

    // relater.go:496
    fn elaborate_object_literal(
        &mut self,
        node: P<Node>,
        source: P<Type>,
        target: P<Type>,
        relation: P<Relation>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> bool {
        let mut diagnostic_output = diagnostic_output;
        if target.flags().intersects(TypeFlags::Primitive | TypeFlags::Never) {
            return false;
        }
        let mut reported_error = false;
        for &prop in node.properties() {
            if is_spread_assignment(prop) {
                continue;
            }
            let symbol = self.get_symbol_of_declaration(prop).unwrap();
            let name_type = self.get_literal_type_from_property(
                symbol,
                TypeFlags::StringOrNumberLiteralOrUnique,
                false,
            );
            if name_type.flags().intersects(TypeFlags::Never) {
                continue;
            }
            match prop.kind() {
                Kind::SetAccessor
                | Kind::GetAccessor
                | Kind::MethodDeclaration
                | Kind::ShorthandPropertyAssignment => {
                    reported_error = self.elaborate_element(
                        source,
                        target,
                        relation,
                        prop.name().unwrap(),
                        None,
                        name_type,
                        None,
                        None,
                        diagnostic_output.as_deref_mut(),
                    ) || reported_error;
                }
                Kind::PropertyAssignment => {
                    let message = if is_computed_non_literal_name(prop.name().unwrap()) {
                        Some(&diagnostics::TYPE_OF_COMPUTED_PROPERTY_S_VALUE_IS_0_WHICH_IS_NOT_ASSIGNABLE_TO_TYPE_1)
                    } else {
                        None
                    };
                    reported_error = self.elaborate_element(
                        source,
                        target,
                        relation,
                        prop.name().unwrap(),
                        prop.initializer(),
                        name_type,
                        message,
                        None,
                        diagnostic_output.as_deref_mut(),
                    ) || reported_error;
                }
                _ => {}
            }
        }
        reported_error
    }

    // relater.go:520
    fn elaborate_array_literal(
        &mut self,
        node: P<Node>,
        source: P<Type>,
        target: P<Type>,
        relation: P<Relation>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> bool {
        let mut diagnostic_output = diagnostic_output;
        let mut source = source;
        if target.flags().intersects(TypeFlags::Primitive | TypeFlags::Never) {
            return false;
        }
        if !self.is_tuple_like_type(source) {
            self.push_contextual_type(node, Some(target), false /*isCache*/);
            source = self.check_array_literal(node, CheckMode::Contextual | CheckMode::ForceTuple);
            self.pop_contextual_type();
            if !self.is_tuple_like_type(source) {
                return false;
            }
        }
        let mut reported_error = false;
        for (i, &element) in node.elements().iter().enumerate() {
            if is_omitted_expression(element)
                || self.is_tuple_like_type(target)
                    && self.get_property_of_type(target, &Number(i as f64).string()).is_none()
            {
                continue;
            }
            let name_type = self.get_number_literal_type(Number(i as f64));
            let check_node = Self::get_effective_check_node(element);
            reported_error = self.elaborate_element(
                source,
                target,
                relation,
                check_node,
                Some(check_node),
                name_type,
                None,
                None,
                diagnostic_output.as_deref_mut(),
            ) || reported_error;
        }
        reported_error
    }

    // relater.go:544
    pub(crate) fn elaborate_element(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        relation: P<Relation>,
        prop: P<Node>,
        next: Option<P<Node>>,
        name_type: P<Type>,
        error_message: Option<&'static Message>,
        diagnostic_factory: Option<&mut dyn FnMut(&mut Checker, P<Node>) -> P<Diagnostic>>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> bool {
        let mut diagnostic_output = diagnostic_output;
        let target_prop_type =
            self.get_best_match_indexed_access_type_or_undefined(source, target, name_type);
        let Some(mut target_prop_type) =
            target_prop_type.filter(|t| !t.flags().intersects(TypeFlags::IndexedAccess))
        else {
            // Don't elaborate on indexes on generic variables
            return false;
        };
        let source_prop_type = self.get_indexed_access_type_or_undefined(
            source,
            name_type,
            AccessFlags::None,
            None,
            AliasArg::None,
        );
        let Some(mut source_prop_type) = source_prop_type else {
            // Don't elaborate on indexes on generic variables or when types match
            return false;
        };
        if self.check_type_related_to(
            source_prop_type,
            target_prop_type,
            relation,
            None, /*errorNode*/
        ) {
            // Don't elaborate on indexes on generic variables or when types match
            return false;
        }
        if next.is_some()
            && self.elaborate_error(
                next,
                source_prop_type,
                target_prop_type,
                relation,
                None, /*headMessage*/
                diagnostic_output.as_deref_mut(),
            )
        {
            return true;
        }
        // Issue error on the prop itself, since the prop couldn't elaborate the error
        let mut diags: Vec<P<Diagnostic>> = Vec::new();
        // Use the expression type, if available
        let mut specific_source = source_prop_type;
        if let Some(next) = next {
            specific_source = self
                .check_expression_for_mutable_location_with_contextual_type(next, source_prop_type);
        }
        if let Some(diagnostic_factory) = diagnostic_factory {
            // Use the custom diagnostic factory if provided (e.g., for JSX text children with dynamic error messages)
            diags.push(diagnostic_factory(self, prop));
        } else if self.exact_optional_property_types
            && self
                .is_exact_optional_property_mismatch(Some(specific_source), Some(target_prop_type))
        {
            let a = self.type_to_string(specific_source, None);
            let b = self.type_to_string(target_prop_type, None);
            diags.push(create_diagnostic_for_node(
                Some(prop),
                &diagnostics::TYPE_0_IS_NOT_ASSIGNABLE_TO_TYPE_1_WITH_EXACT_OPTIONAL_PROPERTY_TYPES_COLON_TRUE_CONSIDER_ADDING_UNDEFINED_TO_THE_TYPE_OF_THE_TARGET,
                &[&a, &b],
            ));
        } else {
            let prop_name = Self::get_property_name_from_index(name_type, None /*accessNode*/);
            let target_is_optional = self
                .get_property_of_type(target, &prop_name)
                .unwrap_or(self.unknown_symbol)
                .flags()
                .intersects(SymbolFlags::Optional);
            let source_is_optional = self
                .get_property_of_type(source, &prop_name)
                .unwrap_or(self.unknown_symbol)
                .flags()
                .intersects(SymbolFlags::Optional);
            target_prop_type = self.remove_missing_type(target_prop_type, target_is_optional);
            source_prop_type = self
                .remove_missing_type(source_prop_type, target_is_optional && source_is_optional);
            let result = self.check_type_related_to_ex(
                specific_source,
                target_prop_type,
                relation,
                Some(prop),
                error_message,
                Some(&mut diags),
            );
            if result && specific_source != source_prop_type {
                // If for whatever reason the expression type doesn't yield an error, make sure we still issue an error on the sourcePropType
                self.check_type_related_to_ex(
                    source_prop_type,
                    target_prop_type,
                    relation,
                    Some(prop),
                    error_message,
                    Some(&mut diags),
                );
            }
        }
        if diags.is_empty() {
            return false;
        }
        let diagnostic = diags[0];
        let mut property_name = String::new();
        let mut target_prop: Option<P<Symbol>> = None;
        if is_type_usable_as_property_name(name_type) {
            property_name = get_property_name_from_type(name_type).into_owned();
            target_prop = self.get_property_of_type(target, &property_name);
        }
        let mut issued_elaboration = false;
        if target_prop.is_none() {
            let index_info = self.get_applicable_index_info(target, name_type);
            if let Some(index_info) = index_info
                && let Some(declaration) = index_info.declaration()
                && !self.program.is_source_file_default_library(
                    get_source_file_of_node(declaration).unwrap().path(),
                )
            {
                issued_elaboration = true;
                diagnostic.add_related_info(create_diagnostic_for_node(
                    Some(declaration),
                    &diagnostics::THE_EXPECTED_TYPE_COMES_FROM_THIS_INDEX_SIGNATURE,
                    &[],
                ));
            }
        }
        if !issued_elaboration
            && (target_prop.is_some_and(|p| !p.declarations().is_empty())
                || target.symbol().is_some_and(|s| !s.declarations().is_empty()))
        {
            let target_node = if let Some(p) = target_prop.filter(|p| !p.declarations().is_empty())
            {
                p.declarations()[0]
            } else {
                target.symbol().unwrap().declarations()[0]
            };
            if property_name.is_empty() || name_type.flags().intersects(TypeFlags::UniqueESSymbol) {
                property_name = self.type_to_string(name_type, None);
            }
            if !self.program.is_source_file_default_library(
                get_source_file_of_node(target_node).unwrap().path(),
            ) {
                let target_str = self.type_to_string(target, None);
                diagnostic.add_related_info(create_diagnostic_for_node(
                    Some(target_node),
                    &diagnostics::THE_EXPECTED_TYPE_COMES_FROM_PROPERTY_0_WHICH_IS_DECLARED_HERE_ON_TYPE_1,
                    &[&property_name, &target_str],
                ));
            }
        }
        self.report_diagnostic(Some(diagnostic), diagnostic_output);
        true
    }

    // relater.go:618
    pub(crate) fn get_best_match_indexed_access_type_or_undefined(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        name_type: P<Type>,
    ) -> Option<P<Type>> {
        let idx = self.get_indexed_access_type_or_undefined(
            target,
            name_type,
            AccessFlags::None,
            None,
            AliasArg::None,
        );
        if idx.is_some() {
            return idx;
        }
        if target.flags().intersects(TypeFlags::Union) {
            let best = self.get_best_matching_type(
                source,
                target,
                super::checker::Checker::compare_types_assignable_simple,
            );
            if let Some(best) = best {
                return self.get_indexed_access_type_or_undefined(
                    best,
                    name_type,
                    AccessFlags::None,
                    None,
                    AliasArg::None,
                );
            }
        }
        None
    }

    // relater.go:632
    pub(crate) fn check_expression_for_mutable_location_with_contextual_type(
        &mut self,
        next: P<Node>,
        source_prop_type: P<Type>,
    ) -> P<Type> {
        self.push_contextual_type(next, Some(source_prop_type), false /*isCache*/);
        let result = self.check_expression_for_mutable_location(next, CheckMode::Contextual);
        self.pop_contextual_type();
        result
    }

    // relater.go:639
    fn elaborate_arrow_function(
        &mut self,
        node: P<Node>,
        source: P<Type>,
        target: P<Type>,
        relation: P<Relation>,
        diagnostic_output: Option<&mut Vec<P<Diagnostic>>>,
    ) -> bool {
        let mut diagnostic_output = diagnostic_output;
        // Don't elaborate blocks or functions with annotated parameter types
        if node.body().is_some_and(is_block) || node.parameters().iter().any(|&p| has_type(p)) {
            return false;
        }
        let Some(source_sig) = self.get_single_call_signature(source) else {
            return false;
        };
        let target_signatures = self.get_signatures_of_type(target, SignatureKind::Call);
        if target_signatures.is_empty() {
            return false;
        }
        let return_expression = node.body();
        let source_return = self.get_return_type_of_signature(source_sig);
        let target_returns: Vec<P<Type>> =
            target_signatures.iter().map(|&s| self.get_return_type_of_signature(s)).collect();
        let target_return = self.get_union_type(&target_returns);
        if self.check_type_related_to(
            source_return,
            target_return,
            relation,
            None, /*errorNode*/
        ) {
            return false;
        }
        if return_expression.is_some()
            && self.elaborate_error(
                return_expression,
                source_return,
                target_return,
                relation,
                None, /*headMessage*/
                diagnostic_output.as_deref_mut(),
            )
        {
            return true;
        }
        let mut diags: Vec<P<Diagnostic>> = Vec::new();
        self.check_type_related_to_ex(
            source_return,
            target_return,
            relation,
            return_expression,
            None, /*headMessage*/
            Some(&mut diags),
        );
        if !diags.is_empty() {
            let diagnostic = diags[0];
            if let Some(symbol) = target.symbol().filter(|s| !s.declarations().is_empty()) {
                let decl = symbol.declarations()[0];
                diagnostic.add_related_info(create_diagnostic_for_node(
                    Some(decl),
                    &diagnostics::THE_EXPECTED_TYPE_COMES_FROM_THE_RETURN_TYPE_OF_THIS_SIGNATURE,
                    &[],
                ));
            }
            if !get_function_flags(Some(node)).intersects(FunctionFlags::Async)
                && self.get_type_of_property_of_type(source_return, "then").is_none()
            {
                let promise_type = self.create_promise_type(source_return);
                if self.check_type_related_to(
                    promise_type,
                    target_return,
                    relation,
                    None, /*errorNode*/
                ) {
                    diagnostic.add_related_info(create_diagnostic_for_node(
                        Some(node),
                        &diagnostics::DID_YOU_MEAN_TO_MARK_THIS_FUNCTION_AS_ASYNC,
                        &[],
                    ));
                }
            }
            self.report_diagnostic(Some(diagnostic), diagnostic_output);
            return true;
        }
        false
    }

    // relater.go:679
    // A type is 'weak' if it is an object type with at least one optional property
    // and no required properties, call/construct signatures or index signatures
    pub(crate) fn is_weak_type(&mut self, t: P<Type>) -> bool {
        if t.flags().intersects(TypeFlags::Object) {
            // tsrs-only: memoized once the members are resolved (the answer follows from them; a reset of the members
            // clears the memo with MembersResolved).
            let flags = t.object_flags_lazy();
            if flags.intersects(ObjectFlags::IsWeakTypeComputed) {
                return flags.intersects(ObjectFlags::IsWeakType);
            }
            let result = self.signatures_of_structured_type(t, SignatureKind::Call).is_empty()
                && self.signatures_of_structured_type(t, SignatureKind::Construct).is_empty()
                && self.index_infos_of_structured_type(t).is_empty()
                && self.has_properties_of_structured_type(t)
                && self.every_property_of_structured_type(t, &mut |_, p| {
                    p.flags().intersects(SymbolFlags::Optional)
                });
            if t.object_flags_lazy().intersects(ObjectFlags::MembersResolved) {
                t.object_flags.set(
                    t.object_flags_lazy()
                        | ObjectFlags::IsWeakTypeComputed
                        | if result { ObjectFlags::IsWeakType } else { ObjectFlags::None },
                );
            }
            return result;
        }
        if t.flags().intersects(TypeFlags::Substitution) {
            return self.is_weak_type(t.as_substitution_type().base_type.get().unwrap());
        }
        if t.flags().intersects(TypeFlags::Intersection) {
            return t.types().iter().all(|&t| self.is_weak_type(t));
        }
        false
    }

    // relater.go:695
    pub(crate) fn has_common_properties(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        is_comparing_jsx_attributes: bool,
    ) -> bool {
        for prop in self.get_properties_of_type(source) {
            if self.is_known_property(target, prop.name(), is_comparing_jsx_attributes) {
                return true;
            }
        }
        false
    }

    // relater.go:717
    /**
     * Check if a property with the given name is known anywhere in the given type. In an object type, a property
     * is considered known if
     * 1. the object type is empty and the check is for assignability, or
     * 2. if the object type has index signatures, or
     * 3. if the property is actually declared in the object type
     *    (this means that 'toString', for example, is not usually a known property).
     * 4. In a union or intersection type,
     *    a property is considered known if it is known in any constituent type.
     *    @param targetType a type to search a given name in
     *    @param name a property name to search
     *    @param isComparingJsxAttributes a boolean flag indicating whether we are searching in `JsxAttributesType`
     */
    pub(crate) fn is_known_property(
        &mut self,
        target_type: P<Type>,
        name: &str,
        is_comparing_jsx_attributes: bool,
    ) -> bool {
        if target_type.flags().intersects(TypeFlags::Object) {
            // For backwards compatibility a symbol-named property is satisfied by a string index signature. This
            // is incorrect and inconsistent with element access expressions, where it is an error, so eventually
            // we should remove this exception.
            if self.get_property_of_object_type(target_type, name).is_some()
                || self.get_applicable_index_info_for_name(target_type, name).is_some()
                || is_late_bound_name(name) && {
                    let string_type = self.string_type;
                    self.get_index_info_of_type(target_type, string_type).is_some()
                }
                || is_comparing_jsx_attributes && is_hyphenated_jsx_name(name)
            {
                // For JSXAttributes, if the attribute has a hyphenated name, consider that the attribute to be known.
                return true;
            }
        }
        if target_type.flags().intersects(TypeFlags::Substitution) {
            return self.is_known_property(
                target_type.as_substitution_type().base_type.get().unwrap(),
                name,
                is_comparing_jsx_attributes,
            );
        }
        if target_type.flags().intersects(TypeFlags::UnionOrIntersection)
            && is_excess_property_check_target(target_type)
        {
            for &t in target_type.types() {
                if self.is_known_property(t, name, is_comparing_jsx_attributes) {
                    return true;
                }
            }
        }
        false
    }
}

// relater.go:743
pub(crate) fn is_hyphenated_jsx_name(name: &str) -> bool {
    name.contains('-')
}

// relater.go:747
pub(crate) fn is_excess_property_check_target(t: P<Type>) -> bool {
    t.flags().intersects(TypeFlags::Object)
        && !t.object_flags().intersects(ObjectFlags::ObjectLiteralPatternWithComputedProperties)
        || t.flags().intersects(TypeFlags::NonPrimitive)
        || t.flags().intersects(TypeFlags::Substitution)
            && is_excess_property_check_target(t.as_substitution_type().base_type.get().unwrap())
        || t.flags().intersects(TypeFlags::Union)
            && t.types().iter().any(|&t| is_excess_property_check_target(t))
        || t.flags().intersects(TypeFlags::Intersection)
            && t.types().iter().all(|&t| is_excess_property_check_target(t))
}

impl Checker {
    // relater.go:766
    // Return true if the given type is deeply nested. We consider this to be the case when the given stack contains
    // maxDepth or more occurrences of types with the same recursion identity as the given type. The recursion identity
    // provides a shared identity for type instantiations that repeat in some (possibly infinite) pattern. For example,
    // in `type Deep<T> = { next: Deep<Deep<T>> }`, repeatedly referencing the `next` property leads to an infinite
    // sequence of ever deeper instantiations with the same recursion identity (in this case the symbol associated with
    // the object type literal).
    // A homomorphic mapped type is considered deeply nested if its target type is deeply nested, and an intersection is
    // considered deeply nested if any constituent of the intersection is deeply nested.
    // It is possible, though highly unlikely, for the deeply nested check to be true in a situation where a chain of
    // instantiations is not infinitely expanding. Effectively, we will generate a false positive when two types are
    // structurally equal to at least maxDepth levels, but unequal at some level beyond that.
    pub(crate) fn is_deeply_nested_type(
        &mut self,
        t: P<Type>,
        stack: &[P<Type>],
        max_depth: usize,
    ) -> bool {
        if stack.len() >= max_depth {
            let target = get_recursion_identity_target(self, t);
            if target.flags().intersects(TypeFlags::Intersection) {
                for &t in target.types() {
                    if self.is_deeply_nested_type(t, stack, max_depth) {
                        return true;
                    }
                }
            } else {
                let identity = get_recursion_identity_from_target(target);
                let mut count = 0;
                let mut last_type_id = TypeId(0);
                for &t in stack {
                    if has_matching_recursion_identity(self, t, identity) {
                        // We only count occurrences with a higher type id than the previous occurrence, since higher
                        // type ids are an indicator of newer instantiations caused by recursion.
                        if t.id >= last_type_id {
                            count += 1;
                            if count >= max_depth {
                                return true;
                            }
                        }
                        last_type_id = t.id;
                    }
                }
            }
        }
        false
    }
}

impl Checker {
    /// `is_deeply_nested_type` over a relater stack, keeping each entry's recursion identity once a walk has computed
    /// it: the walks repeat on every push, and an entry's identity never changes once computed (its recursion
    /// identity target is fixed by the type's structure and its mapped type's modifiers type, which is computed once).
    /// The first walk to reach an entry computes it exactly as `has_matching_recursion_identity` would, so anything
    /// that computation resolves is resolved at the same point.
    #[inline]
    pub(crate) fn is_deeply_nested_on_stack(
        &mut self,
        t: P<Type>,
        stack: &[RelaterStackEntry],
        max_depth: usize,
    ) -> bool {
        stack.len() >= max_depth && self.is_deeply_nested_on_stack_walk(t, stack, max_depth)
    }

    fn is_deeply_nested_on_stack_walk(
        &mut self,
        t: P<Type>,
        stack: &[RelaterStackEntry],
        max_depth: usize,
    ) -> bool {
        let result = self.is_deeply_nested_on_stack_walk_kept(t, stack, max_depth);
        if self.deep_nested_shadow {
            let types: Vec<P<Type>> = stack.iter().map(|e| e.t).collect();
            let fresh = self.is_deeply_nested_type(t, &types, max_depth);
            assert!(
                result == fresh,
                "TSRS_DEEP_NESTED=shadow: type {} on a stack of {}: kept identities say {result}, a fresh walk {fresh}",
                t.id.0,
                stack.len()
            );
            for e in stack {
                let kept = e.identity_key.get();
                if kept != 0 {
                    let target = get_recursion_identity_target(self, e.t);
                    let fresh = if target.flags().intersects(TypeFlags::Intersection) {
                        RECURSION_KEY_INTERSECTION | target.pack()
                    } else {
                        recursion_key(get_recursion_identity_from_target(target))
                    };
                    assert!(
                        kept == fresh,
                        "TSRS_DEEP_NESTED=shadow: the kept recursion identity of type {} changed",
                        e.t.id.0
                    );
                }
            }
        }
        result
    }

    fn is_deeply_nested_on_stack_walk_kept(
        &mut self,
        t: P<Type>,
        stack: &[RelaterStackEntry],
        max_depth: usize,
    ) -> bool {
        let target = get_recursion_identity_target(self, t);
        let top = stack.last().filter(|e| e.t == t && e.identity_key.get() == 0);
        if target.flags().intersects(TypeFlags::Intersection) {
            if let Some(top) = top {
                top.identity_key.set(RECURSION_KEY_INTERSECTION | target.pack());
            }
            for &t in target.types() {
                if self.is_deeply_nested_on_stack(t, stack, max_depth) {
                    return true;
                }
            }
            return false;
        }
        let identity = get_recursion_identity_from_target(target);
        let key = recursion_key(identity);
        if let Some(top) = top {
            top.identity_key.set(key);
        }
        let mut count = 0;
        let mut last_type_id = TypeId(0);
        for e in stack {
            let mut entry_key = e.identity_key.get();
            if entry_key == 0 {
                let target = get_recursion_identity_target(self, e.t);
                entry_key = if target.flags().intersects(TypeFlags::Intersection) {
                    RECURSION_KEY_INTERSECTION | target.pack()
                } else {
                    recursion_key(get_recursion_identity_from_target(target))
                };
                e.identity_key.set(entry_key);
            }
            let matches = if entry_key & RECURSION_KEY_TAG == RECURSION_KEY_INTERSECTION {
                // SAFETY: the low bits are `pack` of the entry's recursion identity target, a live type.
                intersection_has_matching_recursion_identity(
                    self,
                    unsafe { P::unpack(entry_key) },
                    identity,
                )
            } else {
                entry_key == key
            };
            if matches {
                // We only count occurrences with a higher type id than the previous occurrence, since higher
                // type ids are an indicator of newer instantiations caused by recursion.
                if e.t.id >= last_type_id {
                    count += 1;
                    if count >= max_depth {
                        return true;
                    }
                }
                last_type_id = e.t.id;
            }
        }
        false
    }
}

const RECURSION_KEY_TAG: u64 = 7 << 48;
/// A `RelaterStackEntry` identity key for an entry whose recursion identity target is an intersection (the target
/// packed in the low bits): such an entry matches an identity of one of its constituents.
const RECURSION_KEY_INTERSECTION: u64 = 4 << 48;

/// A nonzero word for each recursion identity, equal for equal identities.
#[inline]
fn recursion_key(identity: RecursionId) -> u64 {
    match identity {
        RecursionId::Node(n) => (1 << 48) | n.pack(),
        RecursionId::Symbol(s) => (2 << 48) | s.pack(),
        RecursionId::Type(t) => (3 << 48) | t.pack(),
    }
}

// relater.go:797
#[expect(
    clippy::inline_always,
    reason = "measured: plain #[inline] lets it be outlined (notes/perf-icount-hillclimb.md, round 3)"
)]
#[inline(always)]
fn has_matching_recursion_identity(c: &mut Checker, t: P<Type>, identity: RecursionId) -> bool {
    let target = get_recursion_identity_target(c, t);
    if target.flags().intersects(TypeFlags::Intersection) {
        return intersection_has_matching_recursion_identity(c, target, identity);
    }
    get_recursion_identity_from_target(target) == identity
}

/// `has_matching_recursion_identity` of an intersection recursion identity target.
#[inline(never)]
fn intersection_has_matching_recursion_identity(
    c: &mut Checker,
    target: P<Type>,
    identity: RecursionId,
) -> bool {
    intersection_recursion_keys(c, target).contains(&recursion_key(identity))
}

/// The `recursion_key`s of the identities of an intersection recursion identity target's constituents (nested
/// intersections' included). tsrs: kept per intersection; the deeply-nested tests ask about them again and again, and
/// they never change.
fn intersection_recursion_keys(c: &mut Checker, target: P<Type>) -> &'static [u64] {
    if let Some(&keys) = c.intersection_recursion_ids.get(&target) {
        return keys;
    }
    create_intersection_recursion_keys(c, target)
}

#[inline(never)]
fn create_intersection_recursion_keys(c: &mut Checker, target: P<Type>) -> &'static [u64] {
    let mut keys = Vec::new();
    collect_intersection_recursion_keys(c, target, &mut keys);
    let keys = alloc_slice(&keys);
    c.intersection_recursion_ids.insert(target, keys);
    keys
}

fn collect_intersection_recursion_keys(c: &mut Checker, target: P<Type>, keys: &mut Vec<u64>) {
    for &t in target.types() {
        let t = get_recursion_identity_target(c, t);
        if t.flags().intersects(TypeFlags::Intersection) {
            collect_intersection_recursion_keys(c, t, keys);
        } else {
            keys.push(recursion_key(get_recursion_identity_from_target(t)));
        }
    }
}

// relater.go:810
pub(crate) fn get_recursion_identity(c: &mut Checker, t: P<Type>) -> RecursionId {
    let target = get_recursion_identity_target(c, t);
    get_recursion_identity_from_target(target)
}

// relater.go:820
// Get the recursion identity target type from a type. Recursively (a) obtain the target object type of an
// indexed access (i.e. the T in T[K]), and (b) unwrap nested homomorphic mapped types and return the deepest
// target type that has a symbol. The unwrapping better preserves unique type identities for mapped types applied
// to explicitly written object literals. For example in `Mapped<{ x: Mapped<{ x: Mapped<{ x: string }>}>}>`,
// each of the mapped type applications will have a unique recursion identity (that of their target object type
// literal) and thus avoid appearing deeply nested.
#[inline]
fn get_recursion_identity_target(c: &mut Checker, t: P<Type>) -> P<Type> {
    // Most types are their own target; the unwrapping is out of line.
    if t.flags().intersects(TypeFlags::IndexedAccess)
        || t.object_flags().contains(ObjectFlags::InstantiatedMapped)
    {
        return get_recursion_identity_target_worker(c, t);
    }
    t
}

#[inline(never)]
fn get_recursion_identity_target_worker(c: &mut Checker, t: P<Type>) -> P<Type> {
    if t.flags().intersects(TypeFlags::IndexedAccess) {
        return get_recursion_identity_target(
            c,
            t.as_indexed_access_type().object_type.get().unwrap(),
        );
    }
    if t.object_flags().contains(ObjectFlags::InstantiatedMapped) {
        let target = c.get_modifiers_type_from_mapped_type(t);
        if target.symbol().is_some()
            || target.flags().intersects(TypeFlags::Intersection)
                && target.types().iter().any(|t| t.symbol().is_some())
        {
            return get_recursion_identity_target(c, target);
        }
    }
    t
}

// relater.go:840
// The recursion identity of a type is an object identity that is shared among multiple instantiations of the type.
// We track recursion identities in order to identify deeply nested and possibly infinite type instantiations with
// the same origin. For example, when type parameters are in scope in an object type such as { x: T }, all
// instantiations of that type have the same recursion identity. The default recursion identity is the object
// identity of the type, meaning that every type is unique. Generally, types with constituents that could circularly
// reference the type have a recursion identity that differs from the object identity.
// tsrs: inline, it is asked of every type on the relater's stack (and of each constituent of an intersection there)
// for each deeply-nested test, 474M times on the 38k-file codebase.
#[expect(
    clippy::inline_always,
    reason = "measured: #[inline] is ignored here (notes/perf-icount-hillclimb.md, round 3)"
)]
#[inline(always)]
fn get_recursion_identity_from_target(t: P<Type>) -> RecursionId {
    // Object and array literals are known not to contain recursive references and don't need a recursion identity.
    if t.flags().intersects(TypeFlags::Object) && !is_object_or_array_literal_type(t) {
        if t.object_flags().intersects(ObjectFlags::Reference)
            && let Some(node) = t.as_type_reference().node.get()
        {
            // Deferred type references are tracked through their associated AST node. This gives us finer
            // granularity than using their associated target because each manifest type reference has a
            // unique AST node.
            return as_recursion_id(node);
        }
        if let Some(symbol) = t.symbol()
            && !(t.object_flags().intersects(ObjectFlags::FromTypeNode)
                || t.object_flags().intersects(ObjectFlags::Anonymous)
                    && symbol.flags().intersects(SymbolFlags::Class))
        {
            // We track object types that have a symbol by that symbol (representing the origin of the type), but
            // exclude the static sides of classes (since they share their symbols with the instance sides) and type
            // references that originate in resolution of AST type nodes (since such type nodes cannot be the source
            // of generative recursion without first being instantiated).
            return as_recursion_id(symbol);
        }
        if is_tuple_type(t) && !t.object_flags().intersects(ObjectFlags::FromTypeNode) {
            return as_recursion_id(t.target().unwrap());
        }
    }
    if t.flags().intersects(TypeFlags::TypeParameter)
        && let Some(symbol) = t.symbol()
    {
        // We use the symbol of the type parameter such that all "fresh" instantiations of that type parameter
        // have the same recursion identity.
        return as_recursion_id(symbol);
    }
    if t.flags().intersects(TypeFlags::Conditional) {
        // The root object represents the origin of the conditional type
        return as_recursion_id(t.as_conditional_type().root.get().unwrap().node.get().unwrap());
    }
    as_recursion_id(t)
}

impl Checker {
    // relater.go:872
    pub(crate) fn get_best_matching_type(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        is_related_to: impl FnMut(&mut Checker, P<Type>, P<Type>) -> Ternary,
    ) -> Option<P<Type>> {
        if let Some(t) = self.find_matching_discriminant_type(source, target, is_related_to) {
            return Some(t);
        }
        if let Some(t) = Self::find_matching_type_reference_or_type_alias_reference(source, target)
        {
            return Some(t);
        }
        if let Some(t) = self.find_best_type_for_object_literal(source, target) {
            return Some(t);
        }
        if let Some(t) = self.find_best_type_for_invokable(source, target, SignatureKind::Call) {
            return Some(t);
        }
        if let Some(t) = self.find_best_type_for_invokable(source, target, SignatureKind::Construct)
        {
            return Some(t);
        }
        self.find_most_overlappy_type(source, target)
    }

    // relater.go:891
    fn find_matching_type_reference_or_type_alias_reference(
        source: P<Type>,
        union_target: P<Type>,
    ) -> Option<P<Type>> {
        let source_object_flags = source.object_flags();
        if source_object_flags.intersects(ObjectFlags::Reference | ObjectFlags::Anonymous)
            && union_target.flags().intersects(TypeFlags::Union)
        {
            for &target in union_target.types() {
                if target.flags().intersects(TypeFlags::Object) {
                    let overlap_obj_flags = source_object_flags & target.object_flags();
                    if overlap_obj_flags.intersects(ObjectFlags::Reference)
                        && source.target() == target.target()
                    {
                        return Some(target);
                    }
                    if overlap_obj_flags.intersects(ObjectFlags::Anonymous)
                        && source.alias().is_some()
                        && target.alias().is_some()
                        && source.alias().unwrap().symbol() == target.alias().unwrap().symbol()
                    {
                        return Some(target);
                    }
                }
            }
        }
        None
    }

    // relater.go:909
    fn find_best_type_for_invokable(
        &mut self,
        source: P<Type>,
        union_target: P<Type>,
        kind: SignatureKind,
    ) -> Option<P<Type>> {
        if !self.get_signatures_of_type(source, kind).is_empty() {
            for &t in union_target.types() {
                if !self.get_signatures_of_type(t, kind).is_empty() {
                    return Some(t);
                }
            }
            return None;
        }
        None
    }

    // relater.go:916
    fn find_most_overlappy_type(
        &mut self,
        source: P<Type>,
        union_target: P<Type>,
    ) -> Option<P<Type>> {
        let mut best_match: Option<P<Type>> = None;
        if !source.flags().intersects(TypeFlags::Primitive | TypeFlags::InstantiablePrimitive) {
            let mut matching_count = 0;
            for &target in union_target.types() {
                if !target
                    .flags()
                    .intersects(TypeFlags::Primitive | TypeFlags::InstantiablePrimitive)
                {
                    let source_index = self.get_index_type(source);
                    let target_index = self.get_index_type(target);
                    let overlap = self.get_intersection_type(&[source_index, target_index]);
                    if overlap.flags().intersects(TypeFlags::Index) {
                        // perfect overlap of keys
                        return Some(target);
                    } else if is_unit_type(overlap) || overlap.flags().intersects(TypeFlags::Union)
                    {
                        // We only want to account for literal types otherwise.
                        // If we have a union of index types, it seems likely that we
                        // needed to elaborate between two generic mapped types anyway.
                        let mut length = 1;
                        if overlap.flags().intersects(TypeFlags::Union) {
                            length = count_where(overlap.types(), |&t| is_unit_type(t));
                        }
                        if length >= matching_count {
                            best_match = Some(target);
                            matching_count = length;
                        }
                    }
                }
            }
        }
        best_match
    }

    // relater.go:945
    fn find_best_type_for_object_literal(
        &mut self,
        source: P<Type>,
        union_target: P<Type>,
    ) -> Option<P<Type>> {
        if source.object_flags().intersects(ObjectFlags::ObjectLiteral)
            && some_type(self, union_target, super::checker::Checker::is_array_like_type)
        {
            for &t in union_target.types() {
                if !self.is_array_like_type(t) {
                    return Some(t);
                }
            }
            return None;
        }
        None
    }

    // relater.go:952
    pub(crate) fn should_report_unmatched_property_error(
        &mut self,
        source: P<Type>,
        target: P<Type>,
    ) -> bool {
        let type_call_signatures =
            self.get_signatures_of_structured_type(source, SignatureKind::Call);
        let type_construct_signatures =
            self.get_signatures_of_structured_type(source, SignatureKind::Construct);
        let type_properties = self.get_properties_of_object_type(source);
        if (!type_call_signatures.is_empty() || !type_construct_signatures.is_empty())
            && type_properties.is_empty()
        {
            if (!self.get_signatures_of_type(target, SignatureKind::Call).is_empty()
                && !type_call_signatures.is_empty())
                || !self.get_signatures_of_type(target, SignatureKind::Construct).is_empty()
                    && !type_construct_signatures.is_empty()
            {
                // target has similar signature kinds to source, still focus on the unmatched property
                return true;
            }
            return false;
        }
        true
    }

    // relater.go:967
    pub(crate) fn get_unmatched_property(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        require_optional_properties: bool,
        match_discriminant_properties: bool,
    ) -> Option<P<Symbol>> {
        self.get_unmatched_properties_worker(
            source,
            target,
            require_optional_properties,
            match_discriminant_properties,
            None,
        )
    }

    /// `get_unmatched_property(source, target, require_optional_properties, false)` that also records, for each
    /// property of `get_properties_of_type(target)` by position, the source property it looked up (`Some(found)`)
    /// or `None` where it looked nothing up. Nothing is recorded when it walks a lazy member table's declared members
    /// instead (`found` is then left empty), or past an unmatched property.
    #[expect(
        clippy::option_option,
        reason = "The outer option records whether a result is available; the inner option records an absent value."
    )]
    pub(crate) fn get_unmatched_property_recording(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        require_optional_properties: bool,
        found: &mut Vec<Option<Option<P<Symbol>>>>,
    ) -> Option<P<Symbol>> {
        self.get_unmatched_properties_worker_ex(
            source,
            target,
            require_optional_properties,
            false,
            None,
            Some(found),
        )
    }

    // relater.go:971
    pub(crate) fn get_unmatched_properties(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        require_optional_properties: bool,
        match_discriminant_properties: bool,
    ) -> Vec<P<Symbol>> {
        let mut props: Vec<P<Symbol>> = Vec::new();
        self.get_unmatched_properties_worker(
            source,
            target,
            require_optional_properties,
            match_discriminant_properties,
            Some(&mut props),
        );
        props
    }

    // relater.go:977
    fn get_unmatched_properties_worker(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        require_optional_properties: bool,
        match_discriminant_properties: bool,
        props_out: Option<&mut Vec<P<Symbol>>>,
    ) -> Option<P<Symbol>> {
        self.get_unmatched_properties_worker_ex(
            source,
            target,
            require_optional_properties,
            match_discriminant_properties,
            props_out,
            None,
        )
    }

    #[expect(
        clippy::option_option,
        reason = "The outer option records whether a result is available; the inner option records an absent value."
    )]
    fn get_unmatched_properties_worker_ex(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        require_optional_properties: bool,
        match_discriminant_properties: bool,
        props_out: Option<&mut Vec<P<Symbol>>>,
        found: Option<&mut Vec<Option<Option<P<Symbol>>>>>,
    ) -> Option<P<Symbol>> {
        let mut props_out = props_out;
        let lazy_properties =
            if self.lazy_unmatched { self.get_lazy_properties_in_order(target) } else { None };
        let lazy = lazy_properties.is_some();
        let properties: std::borrow::Cow<'static, [P<Symbol>]> = match lazy_properties {
            Some(properties) => properties.into(),
            None => self.get_properties_of_type(target).into(),
        };
        // Only full lookups are worth recording (has_property_of_type does not instantiate).
        let mut found = found.filter(|_| !lazy && !self.lazy_has_prop);
        let resolved_source = self.resolved_property_source(source);
        for (i, &target_prop) in properties.iter().enumerate() {
            // tsrs: the flag test first, the declaration-reading test only for a property that is looked up (both
            // only skip it).
            // TODO: remove this when we support static private identifier fields and find other solutions to get privateNamesAndStaticFields test to pass
            if (require_optional_properties
                || !target_prop.flags().intersects(SymbolFlags::Optional)
                    && !target_prop.check_flags().intersects(CheckFlags::Partial))
                && !is_static_private_identifier_property(target_prop)
            {
                let source_prop = if (lazy || self.lazy_has_prop) && !match_discriminant_properties
                {
                    // notes/mem-lazy.md L9/L10: only whether the source has the property matters below.
                    if self.has_property_of_type(source, target_prop.name()) {
                        Some(target_prop)
                    } else {
                        None
                    }
                } else if let Some(resolved_source) = &resolved_source {
                    self.get_resolved_property(resolved_source, target_prop.name())
                } else {
                    self.get_property_of_type(source, target_prop.name())
                };
                if let Some(found) = found.as_deref_mut() {
                    // Only looked-up positions are written; the skipped ones in between (most: optional target
                    // properties) are filled in bulk.
                    found.resize(i, None);
                    found.push(Some(source_prop));
                }
                // notes/mem-lazy.md L10: a declared member stands in for the target property until the property
                // itself is returned or its type is needed.
                let mut target_prop = target_prop;
                match source_prop {
                    None => {
                        if lazy {
                            target_prop =
                                self.get_property_of_type(target, target_prop.name()).unwrap();
                        }
                        match props_out.as_deref_mut() {
                            None => return Some(target_prop),
                            Some(out) => out.push(target_prop),
                        }
                    }
                    Some(source_prop) => {
                        if match_discriminant_properties {
                            if lazy {
                                target_prop =
                                    self.get_property_of_type(target, target_prop.name()).unwrap();
                            }
                            let target_type = self.get_type_of_symbol(target_prop);
                            if target_type.flags().intersects(TypeFlags::Unit) {
                                let source_type = self.get_type_of_symbol(source_prop);
                                if !(source_type.flags().intersects(TypeFlags::Any) || {
                                    let a = self.get_regular_type_of_literal_type(source_type);
                                    let b = self.get_regular_type_of_literal_type(target_type);
                                    a == b
                                }) {
                                    match props_out.as_deref_mut() {
                                        None => return Some(target_prop),
                                        Some(out) => out.push(target_prop),
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        if let Some(found) = found {
            found.resize(properties.len(), None);
        }
        None
    }
}

impl Checker {
    // notes/mem-lazy.md L10: the properties getPropertiesOfType(t) would return, in the same order, but with declared
    // members in place of the instantiations a lazy member table has not created; None when t has no such table.
    // Mirrors resolveLazyMembers (declared named members, then addInheritedMembers over each base type's
    // properties) and getNamedMembers (members declared in the class or interface first, each part sorted with
    // compareSymbols). A declared member has its instantiation's name, flags and declarations, and names are unique
    // in a member table, so the order is the same. Kept in the table: bases share it instead of being walked again.
    fn get_lazy_properties_in_order(&mut self, t: P<Type>) -> Option<Vec<P<Symbol>>> {
        let reduced = self.get_reduced_apparent_type(t);
        if !reduced.flags().intersects(TypeFlags::Object) {
            return None;
        }
        let lm = self.get_ready_lazy_member_table(reduced)?;
        self.lazy_member_stats.unmatched_lazy_walks += 1;
        if let Some(properties) = lm.ordered_properties.get() {
            return Some(properties.get().to_vec());
        }
        let mut members: Vec<P<Symbol>> = Vec::new();
        let mut seen: FxHashSet<&'static str> = FxHashSet::default();
        if let Some(declared_members) =
            self.resolve_declared_members(reduced.target().unwrap()).declared_members.get()
        {
            for (id, symbol) in declared_members.entries() {
                if self.is_named_member(symbol, id) {
                    seen.insert(id);
                    members.push(lm.declared.lookup(id).unwrap_or(symbol));
                }
            }
        }
        for &base_type in lm.ready.get().unwrap().base_types.get() {
            let base_properties: std::borrow::Cow<'static, [P<Symbol>]> =
                match self.get_lazy_properties_in_order(base_type) {
                    Some(properties) => properties.into(),
                    None => self.get_properties_of_type(base_type).into(),
                };
            for &p in base_properties.iter() {
                if !is_static_private_identifier_property(p) && seen.insert(p.name()) {
                    members.push(p);
                }
            }
        }
        let container = reduced.symbol();
        let is_class_or_interface_container = container
            .is_some_and(|c| c.flags().intersects(SymbolFlags::Class | SymbolFlags::Interface));
        let mut contained: Vec<P<Symbol>> = Vec::new();
        let mut rest: Vec<P<Symbol>> = Vec::new();
        for p in members {
            if is_class_or_interface_container
                && Self::is_declaration_contained_by(p, container.unwrap())
            {
                contained.push(p);
            } else {
                rest.push(p);
            }
        }
        self.sort_symbols(&mut contained);
        self.sort_symbols(&mut rest);
        contained.extend(rest);
        let _ = lm.ordered_properties.set(ThinSlice::new(alloc_slice(&contained)));
        Some(contained)
    }
}

// relater.go:1008
/// tsrs: borrows `properties` when nothing is excluded (nearly always: the set is empty).
pub(crate) fn exclude_properties<'a>(
    properties: &'a [P<Symbol>],
    excluded_properties: &Set<String>,
) -> std::borrow::Cow<'a, [P<Symbol>]> {
    if excluded_properties.is_empty() || properties.is_empty() {
        return properties.into();
    }
    let mut reduced: Vec<P<Symbol>> = Vec::new();
    let mut excluded = false;
    for (i, &prop) in properties.iter().enumerate() {
        if !excluded_properties.m.contains(prop.name()) {
            if excluded {
                reduced.push(prop);
            }
        } else if !excluded {
            reduced = properties[..i].to_vec();
            excluded = true;
        }
    }
    if excluded {
        return reduced.into();
    }
    properties.into()
}

impl TypeDiscriminator<'_> {
    // relater.go:1036
    pub(crate) fn len(&self, c: &Checker) -> usize {
        let _ = c;
        self.props.len()
    }

    // relater.go:1040
    pub(crate) fn name(&self, c: &Checker, index: usize) -> &'static str {
        let _ = c;
        self.props[index].name()
    }

    // relater.go:1044
    pub(crate) fn matches(&mut self, c: &mut Checker, index: usize, t: P<Type>) -> bool {
        let prop_type = c.get_type_of_symbol(self.props[index]);
        for s in prop_type.distributed_iter() {
            if (self.is_related_to)(c, s, t) != Ternary::FALSE {
                return true;
            }
        }
        false
    }
}

impl Checker {
    // relater.go:1055
    // Keep this up-to-date with the same logic within `getApparentTypeOfContextualType`, since they should behave similarly
    pub(crate) fn find_matching_discriminant_type(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        is_related_to: impl FnMut(&mut Checker, P<Type>, P<Type>) -> Ternary,
    ) -> Option<P<Type>> {
        let mut is_related_to = is_related_to;
        if target.flags().intersects(TypeFlags::Union)
            && source.flags().intersects(TypeFlags::Intersection | TypeFlags::Object)
        {
            if let Some(m) = self.get_matching_union_constituent_for_type(target, source) {
                return Some(m);
            }
            let source_properties = self.get_properties_of_type(source);
            let discriminant_properties =
                self.find_discriminant_properties(source_properties, target);
            if !discriminant_properties.is_empty() {
                let mut discriminator = TypeDiscriminator {
                    props: discriminant_properties,
                    is_related_to: &mut is_related_to,
                };
                let discriminated =
                    self.discriminate_type_by_discriminable_items(target, &mut discriminator);
                if discriminated != target {
                    return Some(discriminated);
                }
            }
        }
        None
    }

    // relater.go:1070
    pub(crate) fn find_discriminant_properties(
        &mut self,
        source_properties: &[P<Symbol>],
        target: P<Type>,
    ) -> Vec<P<Symbol>> {
        let mut result: Vec<P<Symbol>> = Vec::new();
        for &source_property in source_properties {
            if self.is_discriminant_property(Some(target), source_property.name()) {
                result.push(source_property);
            }
        }
        result
    }

    // relater.go:1080
    pub(crate) fn is_discriminant_property(&mut self, t: Option<P<Type>>, name: &str) -> bool {
        if let Some(t) = t.filter(|t| t.flags().intersects(TypeFlags::Union)) {
            let prop = self.get_union_or_intersection_property(
                t, name, false, /*skipObjectFunctionPropertyAugment*/
            );
            if let Some(prop) =
                prop.filter(|p| p.check_flags().intersects(CheckFlags::SyntheticProperty))
            {
                if tsrs_core::sharedgraph::frozen(prop.get())
                    && !prop.check_flags().intersects(CheckFlags::IsDiscriminantComputed)
                {
                    // Shared graph: a frozen symbol's flags cannot be cached into; compute, don't cache.
                    return prop.check_flags().contains(CheckFlags::NonUniformAndLiteral) && {
                        let prop_type = self.get_type_of_symbol(prop);
                        !self.is_generic_type(prop_type)
                    };
                }
                if !prop.check_flags().intersects(CheckFlags::IsDiscriminantComputed) {
                    prop.set_check_flags(prop.check_flags() | CheckFlags::IsDiscriminantComputed);
                    if prop.check_flags().contains(CheckFlags::NonUniformAndLiteral) && {
                        let prop_type = self.get_type_of_symbol(prop);
                        !self.is_generic_type(prop_type)
                    } {
                        prop.set_check_flags(prop.check_flags() | CheckFlags::IsDiscriminant);
                    }
                }
                return prop.check_flags().intersects(CheckFlags::IsDiscriminant);
            }
        }
        false
    }

    // relater.go:1096
    pub(crate) fn get_matching_union_constituent_for_type(
        &mut self,
        union_type: P<Type>,
        t: P<Type>,
    ) -> Option<P<Type>> {
        let key_property_name = self.get_key_property_name(union_type);
        if key_property_name.is_empty() {
            return None;
        }
        let prop_type = self.get_type_of_property_of_type(t, &key_property_name)?;
        self.get_constituent_type_for_key_type(union_type, prop_type)
    }

    // relater.go:1111
    // Return the name of a discriminant property for which it was possible and feasible to construct a map of
    // constituent types keyed by the literal types of the property by that name in each constituent type. Return
    // an empty string if no such discriminant property exists.
    pub(crate) fn get_key_property_name(&mut self, t: P<Type>) -> String {
        let u = t.as_union_type();
        if u.key_property_name().is_empty() {
            let (key_property_name, constituent_map) = self.compute_key_property_name_and_map(t);
            u.set_key_property_name(alloc_str(&key_property_name));
            // An empty map stands for Go's nil map (mapTypesByKeyProperty never returns an empty non-nil map).
            if constituent_map.is_empty() {
                if let Some(m) = u.constituent_map() {
                    m.set_ref(None);
                }
            } else {
                u.constituent_map_for_write().assign(constituent_map);
            }
        }
        if u.key_property_name() == INTERNAL_SYMBOL_NAME_MISSING {
            return String::new();
        }
        u.key_property_name().to_string()
    }

    // relater.go:1124
    // Given a union type for which getKeyPropertyName returned a non-empty string, return the constituent
    // that corresponds to the given key type for that property name.
    pub(crate) fn get_constituent_type_for_key_type(
        &mut self,
        t: P<Type>,
        key_type: P<Type>,
    ) -> Option<P<Type>> {
        let key = self.get_regular_type_of_literal_type(key_type);
        let result = t.as_union_type().constituent_map().and_then(|m| m.get(&key));
        if result != Some(self.unknown_type) {
            return result;
        }
        None
    }

    // relater.go:1132
    fn compute_key_property_name_and_map(
        &mut self,
        t: P<Type>,
    ) -> (String, FxHashMap<P<Type>, P<Type>>) {
        let types = t.types();
        if types.len() < 10
            || t.object_flags().intersects(ObjectFlags::PrimitiveUnion)
            || count_where(types, |&t| is_object_or_instantiable_non_primitive(t)) < 10
        {
            return (INTERNAL_SYMBOL_NAME_MISSING.to_string(), FxHashMap::default());
        }
        let key_property_name = self.get_key_property_candidate_name(types);
        if key_property_name.is_empty() {
            return (INTERNAL_SYMBOL_NAME_MISSING.to_string(), FxHashMap::default());
        }
        let map_by_key_property = self.map_types_by_key_property(types, &key_property_name);
        // An empty map stands for Go's nil map.
        if map_by_key_property.is_empty() {
            return (INTERNAL_SYMBOL_NAME_MISSING.to_string(), FxHashMap::default());
        }
        (key_property_name, map_by_key_property)
    }
}

// relater.go:1148
fn is_object_or_instantiable_non_primitive(t: P<Type>) -> bool {
    t.flags().intersects(TypeFlags::Object | TypeFlags::InstantiableNonPrimitive)
}

impl Checker {
    // relater.go:1152
    fn get_key_property_candidate_name(&mut self, types: &[P<Type>]) -> String {
        for &t in types {
            if t.flags().intersects(TypeFlags::Object | TypeFlags::InstantiableNonPrimitive) {
                for p in self.get_properties_of_type(t).iter().copied() {
                    let prop_type = self.get_type_of_symbol(p);
                    if is_unit_type(prop_type) {
                        return p.name().to_string();
                    }
                }
            }
        }
        String::new()
    }

    // relater.go:1169
    // Given a set of constituent types and a property name, create and return a map keyed by the literal
    // types of the property by that name in each constituent type. No map is returned if some key property
    // has a non-literal type or if less than 10 or less than 50% of the constituents have a unique key.
    // Entries with duplicate keys have unknownType as the value.
    // (An empty map stands for Go's nil map.)
    fn map_types_by_key_property(
        &mut self,
        types: &[P<Type>],
        key_property_name: &str,
    ) -> FxHashMap<P<Type>, P<Type>> {
        let mut types_by_key: FxHashMap<P<Type>, P<Type>> = FxHashMap::default();
        let mut count = 0;
        for &t in types {
            if t.flags().intersects(
                TypeFlags::Object | TypeFlags::Intersection | TypeFlags::InstantiableNonPrimitive,
            ) {
                let discriminant = self.get_type_of_property_of_type(t, key_property_name);
                let Some(discriminant) = discriminant.filter(|&d| is_literal_type(d)) else {
                    return FxHashMap::default();
                };
                let mut duplicate = false;
                for d in discriminant.distributed_iter() {
                    let key = self.get_regular_type_of_literal_type(d);
                    match types_by_key.entry(key) {
                        Entry::Vacant(entry) => {
                            entry.insert(t);
                        }
                        Entry::Occupied(mut entry) => {
                            if *entry.get() != self.unknown_type {
                                entry.insert(self.unknown_type);
                                duplicate = true;
                            }
                        }
                    }
                }
                if !duplicate {
                    count += 1;
                }
            }
        }
        if count >= 10 && count * 2 >= types.len() {
            return types_by_key;
        }
        FxHashMap::default()
    }

    // relater.go:1205
    pub(crate) fn discriminate_type_by_discriminable_items(
        &mut self,
        target: P<Type>,
        discriminator: &mut dyn Discriminator,
    ) -> P<Type> {
        let types = target.types();
        let mut include = vec![Ternary::FALSE; types.len()];
        for (i, &t) in types.iter().enumerate() {
            if !t.flags().intersects(TypeFlags::Primitive)
                && !self.get_reduced_type(t).flags().intersects(TypeFlags::Never)
            {
                include[i] = Ternary::TRUE;
            }
        }
        let n_count = discriminator.len(self);
        for n in 0..n_count {
            // If the remaining target types include at least one with a matching discriminant, eliminate those that
            // have non-matching discriminants. This ensures that we ignore erroneous discriminators and gradually
            // refine the target set without eliminating every constituent (which would lead to `never`).
            let mut matched = false;
            let name = discriminator.name(self, n);
            let known = self.discriminant_property_types(target, types, name);
            for i in 0..types.len() {
                if include[i] != Ternary::FALSE {
                    let target_type = match known.as_ref().and_then(|known| known[i]) {
                        Some(target_type) => target_type,
                        None => {
                            self.get_type_of_property_or_index_signature_of_type(types[i], name)
                        }
                    };
                    if let Some(target_type) = target_type {
                        if discriminator.matches(self, n, target_type) {
                            matched = true;
                        } else {
                            include[i] = Ternary::MAYBE;
                        }
                    }
                }
            }
            // Turn each Ternary.Maybe into Ternary.False if there was a match. Otherwise, revert to Ternary.True.
            for state in &mut include {
                if *state == Ternary::MAYBE {
                    *state = if matched { Ternary::FALSE } else { Ternary::TRUE };
                }
            }
        }
        if include.contains(&Ternary::FALSE) {
            let mut filtered_types: Vec<P<Type>> = Vec::new();
            for (i, &t) in types.iter().enumerate() {
                if include[i] == Ternary::TRUE {
                    filtered_types.push(t);
                }
            }
            let filtered =
                self.get_union_type_ex(&filtered_types, UnionReduction::None, AliasArg::None, None);
            if !filtered.flags().intersects(TypeFlags::Never) {
                return filtered;
            }
        }
        target
    }

    /// tsrs-only: for a union of eight or more constituents, by constituent, what
    /// `get_type_of_property_or_index_signature_of_type(constituent, name)` returns when that is known for good and
    /// asking has no side effects (None where it must be asked). Discriminating a contextual union asks it for each
    /// discriminant of each object literal, and object literals share their contextual unions. Known: a plain object
    /// type with resolved members whose member `name` is a value symbol with its type resolved, or one with no such
    /// member, no index infos and a name that no `Object` or `Function` member has. Computed only with no member
    /// resolution in progress; dropped when any type's members are reset.
    fn discriminant_property_types(
        &mut self,
        target: P<Type>,
        types: &[P<Type>],
        name: &str,
    ) -> Option<std::rc::Rc<[Option<Option<P<Type>>>]>> {
        if types.len() < 8 {
            return None;
        }
        let key = HashedName::new(name);
        let epoch = self.members_reset_epoch;
        if let Some(e) = self
            .discriminant_property_types
            .get(&(target, key.hash()))
            .and_then(|entries| entries.iter().find(|e| e.0 == name))
            && e.1 == epoch
        {
            return Some(std::rc::Rc::clone(&e.2));
        }
        if self.members_resolving != 0 {
            return None;
        }
        let known: std::rc::Rc<[Option<Option<P<Type>>>]> =
            types.iter().map(|&t| self.known_property_or_index_type(t, key)).collect();
        let entries = self.discriminant_property_types.entry((target, key.hash())).or_default();
        entries.retain(|e| e.0 != name);
        entries.push((alloc_str(name), epoch, std::rc::Rc::clone(&known)));
        Some(known)
    }

    /// See `discriminant_property_types`.
    #[expect(
        clippy::option_option,
        reason = "The outer option records whether a result is available; the inner option records an absent value."
    )]
    fn known_property_or_index_type(
        &mut self,
        t: P<Type>,
        key: HashedName<'_>,
    ) -> Option<Option<P<Type>>> {
        if t.flags() != TypeFlags::Object
            || t.object_flags_lazy() & (ObjectFlags::MembersResolved | ObjectFlags::Mapped)
                != ObjectFlags::MembersResolved
            || is_tuple_type(t)
            || t.symbol().is_some_and(|s| s.flags().intersects(SymbolFlags::ValueModule))
        {
            return None;
        }
        match t.as_structured_type().members().and_then(|m| m.lookup_hashed(key)) {
            Some(prop) if prop.flags().intersects(SymbolFlags::Value) => {
                self.cached_type_of_value_symbol(prop).map(Some)
            }
            Some(_) => None,
            None => (self.index_infos_of_structured_type(t).is_empty()
                && !self.may_be_augment_member(key))
            .then_some(None),
        }
    }

    // relater.go:1256
    pub(crate) fn filter_primitives_if_contains_non_primitive(
        &mut self,
        union_type: P<Type>,
    ) -> P<Type> {
        if self.maybe_type_of_kind(union_type, TypeFlags::NonPrimitive) {
            let result = self.filter_type(union_type, |_, t| is_non_primitive_type(t));
            if !result.flags().intersects(TypeFlags::Never) {
                return result;
            }
        }
        union_type
    }
}

// relater.go:1266
fn is_non_primitive_type(t: P<Type>) -> bool {
    !t.flags().intersects(TypeFlags::Primitive)
}

impl Checker {
    // relater.go:1270
    pub(crate) fn get_type_names_for_error_display(
        &mut self,
        left: P<Type>,
        right: P<Type>,
    ) -> (String, String) {
        let mut left_str = if self.symbol_value_declaration_is_context_sensitive(left.symbol()) {
            self.type_to_string(left, left.symbol().unwrap().value_declaration())
        } else {
            self.type_to_string(left, None)
        };
        let mut right_str = if self.symbol_value_declaration_is_context_sensitive(right.symbol()) {
            self.type_to_string(right, right.symbol().unwrap().value_declaration())
        } else {
            self.type_to_string(right, None)
        };
        if left_str == right_str {
            left_str = self.get_type_name_for_error_display(left);
            right_str = self.get_type_name_for_error_display(right);
        }
        (left_str, right_str)
    }

    // relater.go:1290
    pub(crate) fn get_type_name_for_error_display(&mut self, t: P<Type>) -> String {
        self.type_to_string_ex(
            t,
            None, /*enclosingDeclaration*/
            TypeFormatFlags::UseFullyQualifiedType,
            None,
        )
    }

    // relater.go:1294
    fn symbol_value_declaration_is_context_sensitive(&mut self, symbol: Option<P<Symbol>>) -> bool {
        let Some(symbol) = symbol else {
            return false;
        };
        let Some(value_declaration) = symbol.value_declaration() else {
            return false;
        };
        is_expression(value_declaration) && !self.is_context_sensitive(value_declaration)
    }

    // relater.go:1298
    pub(crate) fn type_could_have_top_level_singleton_types(&mut self, t: P<Type>) -> bool {
        // Okay, yes, 'boolean' is a union of 'true | false', but that's not useful
        // in error reporting scenarios. If you need to use this function but that detail matters,
        // feel free to add a flag.
        if t.flags().intersects(TypeFlags::Boolean) {
            return false;
        }
        if t.flags().intersects(TypeFlags::UnionOrIntersection) {
            return t.types().iter().any(|&t| self.type_could_have_top_level_singleton_types(t));
        }
        if t.flags().intersects(TypeFlags::Instantiable) {
            let constraint = self.get_constraint_of_type(t);
            if let Some(constraint) = constraint
                && constraint != t
            {
                return self.type_could_have_top_level_singleton_types(constraint);
            }
        }
        is_unit_type(t)
            || t.flags().intersects(TypeFlags::TemplateLiteral)
            || t.flags().intersects(TypeFlags::StringMapping)
    }
    // relater.go:1317
    pub(crate) fn get_variances(&mut self, t: P<Type>) -> &'static [VarianceFlags] {
        // Arrays and tuples are known to be covariant, no need to spend time computing this.
        if t == self.global_array_type
            || t == self.global_readonly_array_type
            || t.object_flags().intersects(ObjectFlags::Tuple)
        {
            return self.array_variances;
        }
        self.get_variances_worker(t.symbol().unwrap(), t.as_interface_type().type_parameters())
    }

    // relater.go:1325
    pub(crate) fn get_alias_variances(&mut self, symbol: P<Symbol>) -> &'static [VarianceFlags] {
        let type_parameters = self.type_alias_links.get(symbol).type_parameters.get();
        self.get_variances_worker(symbol, type_parameters)
    }

    // relater.go:1334
    // Return an array containing the variance of each type parameter. The variance is effectively
    // a digest of the type comparisons that occur for each type argument when instantiations of the
    // generic type are structurally compared. We infer the variance information by comparing
    // instantiations of the generic type for type arguments with known relations. The function
    // returns an empty slice when invoked recursively for the given generic type.
    fn get_variances_worker(
        &mut self,
        symbol: P<Symbol>,
        type_parameters: &'static [P<Type>],
    ) -> &'static [VarianceFlags] {
        let links = self.variance_links.get(symbol);
        let variances_len = |links: P<VarianceLinks>| {
            links.variances.get().map_or(0, <[types::VarianceFlags]>::len)
        };
        if links.variances.get().is_none() {
            let stack_index = self.get_variance_stack_index(symbol);
            if stack_index.is_none() {
                let save_resolution_start = self.resolution_start;
                if self.variance_stack.is_empty() {
                    self.resolution_start = self.type_resolutions.len();
                }
                self.variance_stack.push(VarianceStackEntry { symbol, type_parameters });
                let mut variances = vec![VarianceFlags::Invariant; type_parameters.len()];
                for (i, &tp) in type_parameters.iter().enumerate() {
                    let modifiers = Self::get_type_parameter_modifiers(tp);
                    let variance: VarianceFlags;
                    if modifiers.intersects(ModifierFlags::Out) {
                        if modifiers.intersects(ModifierFlags::In) {
                            variance = VarianceFlags::Invariant;
                        } else {
                            variance = VarianceFlags::Covariant;
                        }
                    } else if modifiers.intersects(ModifierFlags::In) {
                        variance = VarianceFlags::Contravariant;
                    } else {
                        let save_reliability_flags = self.reliability_flags;
                        self.reliability_flags = RelationComparisonResult::None;
                        // We first compare instantiations where the type parameter is replaced with
                        // marker types that have a known subtype relationship. From this we can infer
                        // invariance, covariance, contravariance or bivariance.
                        let marker_super_type = self.marker_super_type;
                        let marker_sub_type = self.marker_sub_type;
                        let type_with_super =
                            self.create_marker_type(symbol, tp, marker_super_type);
                        let type_with_sub = self.create_marker_type(symbol, tp, marker_sub_type);
                        let mut v =
                            (if self.is_type_assignable_to(type_with_sub, type_with_super) {
                                VarianceFlags::Covariant
                            } else {
                                VarianceFlags::Invariant
                            }) | (if self.is_type_assignable_to(type_with_super, type_with_sub) {
                                VarianceFlags::Contravariant
                            } else {
                                VarianceFlags::Invariant
                            });
                        // If the instantiations appear to be related bivariantly it may be because the
                        // type parameter is independent (i.e. it isn't witnessed anywhere in the generic
                        // type). To determine this we compare instantiations where the type parameter is
                        // replaced with marker types that are known to be unrelated.
                        if v == VarianceFlags::Bivariant && {
                            let marker_other_type = self.marker_other_type;
                            let type_with_other =
                                self.create_marker_type(symbol, tp, marker_other_type);
                            self.is_type_assignable_to(type_with_other, type_with_super)
                        } {
                            v = VarianceFlags::Independent;
                        }
                        if self
                            .reliability_flags
                            .intersects(RelationComparisonResult::ReportsUnmeasurable)
                        {
                            v |= VarianceFlags::Unmeasurable;
                        }
                        if self
                            .reliability_flags
                            .intersects(RelationComparisonResult::ReportsUnreliable)
                        {
                            v |= VarianceFlags::Unreliable;
                        }
                        self.reliability_flags = save_reliability_flags;
                        variance = v;
                    }
                    // If variance computation was restarted due to a circularity we may have already
                    // computed variances for this generic type. If so, we exit early.
                    if variances_len(links) != 0 {
                        break;
                    }
                    variances[i] = variance;
                }
                // Store the results unless a restarted computation has already stored them.
                if variances_len(links) == 0 {
                    links.variances.set(Some(alloc_vec(variances)));
                }
                self.variance_stack.pop();
                if self.variance_stack.is_empty() {
                    self.resolution_start = save_resolution_start;
                }
            } else if let Some(stack_index) = stack_index {
                // We've detected a circularity. Since we may compute different variances depending on where
                // we enter a circularity, we find the generic type with the "smallest" symbol in the circular
                // region of the variance stack and restart the computation from there if necessary. This
                // ensures stable results for circular generic types.
                let mut min_index = stack_index;
                for i in stack_index + 1..self.variance_stack.len() {
                    let a = self.variance_stack[i].symbol;
                    let b = self.variance_stack[min_index].symbol;
                    if self.compare_symbols(Some(a), Some(b)).is_lt() {
                        min_index = i;
                    }
                }
                if min_index > stack_index {
                    let save_variance_stack = std::mem::take(&mut self.variance_stack);
                    let entry = save_variance_stack[min_index];
                    self.get_variances_worker(entry.symbol, entry.type_parameters);
                    self.variance_stack = save_variance_stack;
                }
                // Store an empty slice to mark that we can't compute variances for this type. We treat type
                // parameters as co-variant in this case.
                if variances_len(links) == 0 {
                    links.variances.set(Some(&[]));
                }
            }
        }
        // The immutable arena slice remains valid if a later resolution replaces the link descriptor.
        links.variances.get().unwrap()
    }

    // relater.go:1437
    fn get_variance_stack_index(&self, symbol: P<Symbol>) -> Option<usize> {
        for (i, entry) in self.variance_stack.iter().enumerate() {
            if entry.symbol == symbol {
                return Some(i);
            }
        }
        None
    }

    // relater.go:1446
    pub(crate) fn create_marker_type(
        &mut self,
        symbol: P<Symbol>,
        source: P<Type>,
        target: P<Type>,
    ) -> P<Type> {
        let mapper = new_simple_type_mapper(source, target);
        let t = self.get_declared_type_of_symbol(symbol);
        if self.is_error_type(t) {
            return t;
        }

        let result = if symbol.flags().intersects(SymbolFlags::TypeAlias) {
            let type_parameters = self.type_alias_links.get(symbol).type_parameters.get();
            let type_arguments = self.instantiate_types(type_parameters, Some(mapper));
            self.get_type_alias_instantiation(symbol, &type_arguments, None)
        } else {
            let type_arguments =
                self.instantiate_types(t.as_interface_type().type_parameters(), Some(mapper));
            self.create_type_reference(t, &type_arguments)
        };
        self.marker_types.add(result);
        result
    }

    // relater.go:1462
    pub(crate) fn is_marker_type(&self, t: P<Type>) -> bool {
        self.marker_types.has(&t)
    }

    // relater.go:1466
    pub(crate) fn get_type_parameter_modifiers(tp: P<Type>) -> ModifierFlags {
        let mut flags = ModifierFlags::None;
        if let Some(symbol) = tp.symbol() {
            for d in symbol.declarations() {
                flags |= d.modifier_flags();
            }
        }
        flags & (ModifierFlags::In | ModifierFlags::Out | ModifierFlags::Const)
    }

    // relater.go:1478
    // Return true if the given type reference has a 'void' type argument for a covariant type parameter.
    // See comment at call in recursiveTypeRelatedTo for when this case matters.
    pub(crate) fn has_covariant_void_argument(
        type_arguments: &[P<Type>],
        variances: &[VarianceFlags],
    ) -> bool {
        for (i, &v) in variances.iter().enumerate() {
            if v & VarianceFlags::VarianceMask == VarianceFlags::Covariant
                && type_arguments[i].flags().intersects(TypeFlags::Void)
            {
                return true;
            }
        }
        false
    }

    // relater.go:1487
    pub(crate) fn is_signature_assignable_to(
        &mut self,
        source: P<Signature>,
        target: P<Signature>,
        ignore_return_types: bool,
    ) -> bool {
        let check_mode = if ignore_return_types {
            SignatureCheckMode::IgnoreReturnTypes
        } else {
            SignatureCheckMode::None
        };
        let compare_types = Self::compare_types_assignable_comparer();
        self.compare_signatures_related(
            source,
            target,
            check_mode,
            false, /*reportErrors*/
            None,  /*errorReporter*/
            compare_types,
            None, /*reportUnreliableMarkers*/
        ) != Ternary::FALSE
    }

    // relater.go:1491
    pub(crate) fn compare_signatures_related(
        &mut self,
        source: P<Signature>,
        target: P<Signature>,
        check_mode: SignatureCheckMode,
        report_errors: bool,
        error_reporter: Option<ErrorReporter<'_>>,
        compare_types: TypeComparer,
        report_unreliable_markers: Option<P<TypeMapper>>,
    ) -> Ternary {
        let mut error_reporter = error_reporter;
        let mut source = source;
        let mut target = target;
        if source == target {
            return Ternary::TRUE;
        }
        if !(check_mode.intersects(SignatureCheckMode::StrictTopSignature)
            && self.is_top_signature(source))
            && self.is_top_signature(target)
        {
            return Ternary::TRUE;
        }
        if check_mode.intersects(SignatureCheckMode::StrictTopSignature)
            && self.is_top_signature(source)
            && !self.is_top_signature(target)
        {
            return Ternary::FALSE;
        }
        let target_count = self.get_parameter_count(target);
        let mut source_has_more_parameters = false;
        if !self.has_effective_rest_parameter(target) {
            if check_mode.intersects(SignatureCheckMode::StrictArity) {
                source_has_more_parameters = self.has_effective_rest_parameter(source)
                    || self.get_parameter_count(source) > target_count;
            } else {
                source_has_more_parameters = self.get_min_argument_count(source) > target_count;
            }
        }
        if source_has_more_parameters {
            if report_errors && !check_mode.intersects(SignatureCheckMode::StrictArity) {
                // the second condition should be redundant, because there is no error reporting when comparing signatures by strict arity
                // since it is only done for subtype reduction
                let min_argument_count = self.get_min_argument_count(source);
                (error_reporter.as_mut().unwrap())(
                    self,
                    &diagnostics::TARGET_SIGNATURE_PROVIDES_TOO_FEW_ARGUMENTS_EXPECTED_0_OR_MORE_BUT_GOT_1,
                    &[&min_argument_count, &target_count],
                );
            }
            return Ternary::FALSE;
        }
        if !source.type_parameters().is_empty()
            && !same(source.type_parameters(), target.type_parameters())
        {
            target = self.get_canonical_signature(target);
            source = self.instantiate_signature_in_context_of(
                source,
                target,
                None, /*inferenceContext*/
                Some(compare_types),
            );
        }
        let source_count = self.get_parameter_count(source);
        let source_rest_type = self.get_non_array_rest_type(source);
        let target_rest_type = self.get_non_array_rest_type(target);
        if source_rest_type.is_some() || target_rest_type.is_some() {
            self.instantiate_type(
                source_rest_type.or(target_rest_type).unwrap(),
                report_unreliable_markers,
            );
        }
        let kind = target.declaration().map_or(Kind::Unknown, |d| d.kind());
        let strict_variance = !check_mode.intersects(SignatureCheckMode::Callback)
            && self.strict_function_types
            && kind != Kind::MethodDeclaration
            && kind != Kind::MethodSignature
            && kind != Kind::Constructor;
        let mut result = Ternary::TRUE;
        let source_this_type = self.get_this_type_of_signature(source);
        if let Some(source_this_type) = source_this_type.filter(|&t| t != self.void_type) {
            let target_this_type = self.get_this_type_of_signature(target);
            if let Some(target_this_type) = target_this_type {
                // void sources are assignable to anything.
                let mut related = Ternary::FALSE;
                if !strict_variance {
                    related = compare_types(
                        self,
                        source_this_type,
                        target_this_type,
                        false, /*reportErrors*/
                    );
                }
                if related == Ternary::FALSE {
                    related =
                        compare_types(self, target_this_type, source_this_type, report_errors);
                }
                if related == Ternary::FALSE {
                    if report_errors {
                        (error_reporter.as_mut().unwrap())(
                            self,
                            &diagnostics::THE_THIS_TYPES_OF_EACH_SIGNATURE_ARE_INCOMPATIBLE,
                            &[],
                        );
                    }
                    return Ternary::FALSE;
                }
                result &= related;
            }
        }
        let param_count = if source_rest_type.is_some() || target_rest_type.is_some() {
            source_count.min(target_count)
        } else {
            source_count.max(target_count)
        };
        let rest_index = if source_rest_type.is_some() || target_rest_type.is_some() {
            param_count.checked_sub(1)
        } else {
            None
        };
        for i in 0..param_count {
            let source_type = if Some(i) == rest_index {
                Some(self.get_rest_or_any_type_at_position(source, i))
            } else {
                self.try_get_type_at_position(source, i)
            };
            let target_type = if Some(i) == rest_index {
                Some(self.get_rest_or_any_type_at_position(target, i))
            } else {
                self.try_get_type_at_position(target, i)
            };
            if let (Some(source_type), Some(target_type)) = (source_type, target_type)
                && (source_type != target_type
                    || check_mode.intersects(SignatureCheckMode::StrictArity))
            {
                // In order to ensure that any generic type Foo<T> is at least co-variant with respect to T no matter
                // how Foo uses T, we need to relate parameters bi-variantly (given that parameters are input positions,
                // they naturally relate only contra-variantly). However, if the source and target parameters both have
                // function types with a single call signature, we know we are relating two callback parameters. In
                // that case it is sufficient to only relate the parameters of the signatures co-variantly because,
                // similar to return values, callback parameters are output positions. This means that a Promise<T>,
                // where T is used only in callback parameter positions, will be co-variant (as opposed to bi-variant)
                // with respect to T.
                let mut source_sig: Option<P<Signature>> = None;
                let mut target_sig: Option<P<Signature>> = None;
                if !check_mode.intersects(SignatureCheckMode::Callback)
                    && !self.is_instantiated_generic_parameter(source, i)
                {
                    let t = self.get_non_nullable_type(source_type);
                    source_sig = self.get_single_call_signature(t);
                }
                if !check_mode.intersects(SignatureCheckMode::Callback)
                    && !self.is_instantiated_generic_parameter(target, i)
                {
                    let t = self.get_non_nullable_type(target_type);
                    target_sig = self.get_single_call_signature(t);
                }
                let callbacks = source_sig.is_some()
                    && target_sig.is_some()
                    && self.get_type_predicate_of_signature(source_sig.unwrap()).is_none()
                    && self.get_type_predicate_of_signature(target_sig.unwrap()).is_none()
                    && self.get_type_facts(source_type, TypeFacts::IsUndefinedOrNull)
                        == self.get_type_facts(target_type, TypeFacts::IsUndefinedOrNull);
                let mut related = Ternary::FALSE;
                if callbacks {
                    related = self.compare_signatures_related(
                        target_sig.unwrap(),
                        source_sig.unwrap(),
                        (check_mode & SignatureCheckMode::StrictArity)
                            | if strict_variance {
                                SignatureCheckMode::StrictCallback
                            } else {
                                SignatureCheckMode::BivariantCallback
                            },
                        report_errors,
                        reborrow_reporter(&mut error_reporter),
                        compare_types,
                        report_unreliable_markers,
                    );
                } else {
                    if !check_mode.intersects(SignatureCheckMode::Callback) && !strict_variance {
                        related = compare_types(
                            self,
                            source_type,
                            target_type,
                            false, /*reportErrors*/
                        );
                    }
                    if related == Ternary::FALSE {
                        related = compare_types(self, target_type, source_type, report_errors);
                    }
                }
                // With strict arity, (x: number | undefined) => void is a subtype of (x?: number | undefined) => void
                if related != Ternary::FALSE
                    && check_mode.intersects(SignatureCheckMode::StrictArity)
                    && i >= self.get_min_argument_count(source)
                    && i < self.get_min_argument_count(target)
                    && compare_types(self, source_type, target_type, false /*reportErrors*/)
                        != Ternary::FALSE
                {
                    related = Ternary::FALSE;
                }
                if related == Ternary::FALSE {
                    if report_errors {
                        let a = self.get_parameter_name_at_position(source, i);
                        let b = self.get_parameter_name_at_position(target, i);
                        (error_reporter.as_mut().unwrap())(
                            self,
                            &diagnostics::TYPES_OF_PARAMETERS_0_AND_1_ARE_INCOMPATIBLE,
                            &[&a, &b],
                        );
                    }
                    return Ternary::FALSE;
                }
                result &= related;
            }
        }
        if !check_mode.intersects(SignatureCheckMode::IgnoreReturnTypes) {
            // If a signature resolution is already in-flight, skip issuing a circularity error
            // here and just use the `any` type directly
            let target_return_type = self.get_non_circular_return_type_of_signature(target);
            if target_return_type == self.void_type || target_return_type == self.any_type {
                return result;
            }
            let source_return_type = self.get_non_circular_return_type_of_signature(source);
            // The following block preserves behavior forbidding boolean returning functions from being assignable to type guard returning functions
            let target_type_predicate = self.get_type_predicate_of_signature(target);
            if let Some(target_type_predicate) = target_type_predicate {
                let source_type_predicate = self.get_type_predicate_of_signature(source);
                if let Some(source_type_predicate) = source_type_predicate {
                    result &= self.compare_type_predicate_related_to(
                        source_type_predicate,
                        target_type_predicate,
                        report_errors,
                        reborrow_reporter(&mut error_reporter),
                        compare_types,
                    );
                } else if target_type_predicate.kind() == TypePredicateKind::Identifier
                    || target_type_predicate.kind() == TypePredicateKind::This
                {
                    if report_errors {
                        let a = self.signature_to_string(source);
                        (error_reporter.as_mut().unwrap())(
                            self,
                            &diagnostics::SIGNATURE_0_MUST_BE_A_TYPE_PREDICATE,
                            &[&a],
                        );
                    }
                    return Ternary::FALSE;
                }
            } else {
                // When relating callback signatures, we still need to relate return types bi-variantly as otherwise
                // the containing type wouldn't be co-variant. For example, interface Foo<T> { add(cb: () => T): void }
                // wouldn't be co-variant for T without this rule.
                let mut related = Ternary::FALSE;
                if check_mode.intersects(SignatureCheckMode::BivariantCallback) {
                    related = compare_types(
                        self,
                        target_return_type,
                        source_return_type,
                        false, /*reportErrors*/
                    );
                }
                if related == Ternary::FALSE {
                    related =
                        compare_types(self, source_return_type, target_return_type, report_errors);
                }
                result &= related;
                if result == Ternary::FALSE && report_errors {
                    // The errors reported here serve as markers that trigger error chain reduction in the (*Relater).reportError
                    // method. The markers are elided in the final diagnostic chain and never actually reported.
                    let message: &'static Message = if source.parameters().is_empty()
                        && target.parameters().is_empty()
                    {
                        if source.flags().intersects(SignatureFlags::Construct) {
                            &diagnostics::CONSTRUCT_SIGNATURES_WITH_NO_ARGUMENTS_HAVE_INCOMPATIBLE_RETURN_TYPES_0_AND_1
                        } else {
                            &diagnostics::CALL_SIGNATURES_WITH_NO_ARGUMENTS_HAVE_INCOMPATIBLE_RETURN_TYPES_0_AND_1
                        }
                    } else if source.flags().intersects(SignatureFlags::Construct) {
                        &diagnostics::CONSTRUCT_SIGNATURE_RETURN_TYPES_0_AND_1_ARE_INCOMPATIBLE
                    } else {
                        &diagnostics::CALL_SIGNATURE_RETURN_TYPES_0_AND_1_ARE_INCOMPATIBLE
                    };
                    let a = self.type_to_string(source_return_type, None);
                    let b = self.type_to_string(target_return_type, None);
                    (error_reporter.as_mut().unwrap())(self, message, &[&a, &b]);
                }
            }
        }
        result
    }

    // relater.go:1675
    fn compare_type_predicate_related_to(
        &mut self,
        source: P<TypePredicate>,
        target: P<TypePredicate>,
        report_errors: bool,
        error_reporter: Option<ErrorReporter<'_>>,
        compare_types: TypeComparer,
    ) -> Ternary {
        let mut error_reporter = error_reporter;
        if source.kind() != target.kind() {
            if report_errors {
                (error_reporter.as_mut().unwrap())(self, &diagnostics::A_THIS_BASED_TYPE_GUARD_IS_NOT_COMPATIBLE_WITH_A_PARAMETER_BASED_TYPE_GUARD, &[]);
                let a = self.type_predicate_to_string(source);
                let b = self.type_predicate_to_string(target);
                (error_reporter.as_mut().unwrap())(
                    self,
                    &diagnostics::TYPE_PREDICATE_0_IS_NOT_ASSIGNABLE_TO_1,
                    &[&a, &b],
                );
            }
            return Ternary::FALSE;
        }
        if (source.kind() == TypePredicateKind::Identifier
            || source.kind() == TypePredicateKind::AssertsIdentifier)
            && source.parameter_index() != target.parameter_index()
        {
            if report_errors {
                let a = source.parameter_name();
                let b = target.parameter_name();
                (error_reporter.as_mut().unwrap())(
                    self,
                    &diagnostics::PARAMETER_0_IS_NOT_IN_THE_SAME_POSITION_AS_PARAMETER_1,
                    &[&a, &b],
                );
                let a = self.type_predicate_to_string(source);
                let b = self.type_predicate_to_string(target);
                (error_reporter.as_mut().unwrap())(
                    self,
                    &diagnostics::TYPE_PREDICATE_0_IS_NOT_ASSIGNABLE_TO_1,
                    &[&a, &b],
                );
            }
            return Ternary::FALSE;
        }
        let related = if source.type_() == target.type_() {
            Ternary::TRUE
        } else if let (Some(st), Some(tt)) = (source.type_(), target.type_()) {
            compare_types(self, st, tt, report_errors)
        } else {
            Ternary::FALSE
        };
        if related == Ternary::FALSE && report_errors {
            let a = self.type_predicate_to_string(source);
            let b = self.type_predicate_to_string(target);
            (error_reporter.as_mut().unwrap())(
                self,
                &diagnostics::TYPE_PREDICATE_0_IS_NOT_ASSIGNABLE_TO_1,
                &[&a, &b],
            );
        }
        related
    }

    // relater.go:1708
    // Returns true if `s` is `(...args: A) => R` where `A` is `any`, `any[]`, `never`, or `never[]`, and `R` is `any` or `unknown`.
    fn is_top_signature(&mut self, s: P<Signature>) -> bool {
        if s.type_parameters().is_empty()
            && (s.this_parameter().is_none() || {
                let this_type = self.get_type_of_parameter(s.this_parameter().unwrap());
                is_type_any(Some(this_type))
            })
            && s.parameters().len() == 1
            && signature_has_rest_parameter(s)
        {
            let param_type = self.get_type_of_parameter(s.parameters()[0]);
            let rest_type = if self.is_array_type(param_type) {
                self.get_type_arguments(param_type)[0]
            } else {
                param_type
            };
            return rest_type.flags().intersects(TypeFlags::Any | TypeFlags::Never)
                && self
                    .get_return_type_of_signature(s)
                    .flags()
                    .intersects(TypeFlags::AnyOrUnknown);
        }
        false
    }

    // relater.go:1726
    // Return the number of parameters in a signature. The rest parameter, if present, counts as one
    // parameter. For example, the parameter count of (x: number, y: number, ...z: string[]) is 3 and
    // the parameter count of (x: number, ...args: [number, ...string[], boolean])) is also 3. In the
    // latter example, the effective rest type is [...string[], boolean].
    pub(crate) fn get_parameter_count(&mut self, signature: P<Signature>) -> usize {
        let length = signature.parameters().len();
        if signature_has_rest_parameter(signature) {
            let rest_type = self.get_type_of_symbol(signature.parameters()[length - 1]);
            if is_tuple_type(rest_type) {
                let target = rest_type.target_tuple_type();
                return length + target.fixed_length.get()
                    - usize::from(!target.combined_flags.get().intersects(ElementFlags::Variable));
            }
        }
        length
    }

    // relater.go:1737
    pub(crate) fn get_min_argument_count(&mut self, signature: P<Signature>) -> usize {
        self.get_min_argument_count_ex(signature, MinArgumentCountFlags::None)
    }

    // relater.go:1741
    pub(crate) fn get_min_argument_count_ex(
        &mut self,
        signature: P<Signature>,
        flags: MinArgumentCountFlags,
    ) -> usize {
        let strong_arity_for_untyped_js = flags & MinArgumentCountFlags::StrongArityForUntypedJS;
        let void_is_non_optional = flags & MinArgumentCountFlags::VoidIsNonOptional;
        if !void_is_non_optional.is_empty() || signature.resolved_min_argument_count.get().is_none()
        {
            let mut min_argument_count = None;
            if signature_has_rest_parameter(signature) {
                let rest_type = self
                    .get_type_of_symbol(signature.parameters()[signature.parameters().len() - 1]);
                if is_tuple_type(rest_type) {
                    let first_optional_index =
                        find_index(rest_type.target_tuple_type().element_infos(), |info| {
                            !info.flags.intersects(ElementFlags::Required)
                        });
                    let required_count = first_optional_index
                        .unwrap_or_else(|| rest_type.target_tuple_type().fixed_length());
                    if required_count > 0 {
                        min_argument_count =
                            Some(signature.parameters().len() - 1 + required_count);
                    }
                }
            }
            let mut min_argument_count = if let Some(min_argument_count) = min_argument_count {
                min_argument_count
            } else {
                if strong_arity_for_untyped_js.is_empty()
                    && signature.flags().intersects(SignatureFlags::IsUntypedSignatureInJSFile)
                {
                    return 0;
                }
                signature.min_argument_count()
            };
            if !void_is_non_optional.is_empty() {
                return min_argument_count;
            }
            while min_argument_count > 0 {
                let i = min_argument_count - 1;
                let t = self.get_type_at_position(signature, i);
                if !some_type(self, t, |_, t| t.flags().intersects(TypeFlags::Void)) {
                    break;
                }
                min_argument_count = i;
            }
            signature.resolved_min_argument_count.set(Some(min_argument_count));
        }
        signature.resolved_min_argument_count.get().unwrap()
    }

    // relater.go:1782
    pub(crate) fn has_effective_rest_parameter(&mut self, signature: P<Signature>) -> bool {
        if signature_has_rest_parameter(signature) {
            let rest_type =
                self.get_type_of_symbol(signature.parameters()[signature.parameters().len() - 1]);
            return !is_tuple_type(rest_type)
                || rest_type
                    .target_tuple_type()
                    .combined_flags
                    .get()
                    .intersects(ElementFlags::Variable);
        }
        false
    }

    // relater.go:1790
    pub(crate) fn get_type_at_position(&mut self, signature: P<Signature>, pos: usize) -> P<Type> {
        let t = self.try_get_type_at_position(signature, pos);
        if let Some(t) = t {
            return t;
        }
        self.any_type
    }

    // relater.go:1798
    pub(crate) fn try_get_type_at_position(
        &mut self,
        signature: P<Signature>,
        pos: usize,
    ) -> Option<P<Type>> {
        let param_count =
            signature.parameters().len() - usize::from(signature_has_rest_parameter(signature));
        if pos < param_count {
            return Some(self.get_type_of_parameter(signature.parameters()[pos]));
        }
        if signature_has_rest_parameter(signature) {
            // We want to return the value undefined for an out of bounds parameter position,
            // so we need to check bounds here before calling getIndexedAccessType (which
            // otherwise would return the type 'undefined').
            let rest_type = self.get_type_of_symbol(signature.parameters()[param_count]);
            let index = pos - param_count;
            if !is_tuple_type(rest_type)
                || rest_type
                    .target_tuple_type()
                    .combined_flags
                    .get()
                    .intersects(ElementFlags::Variable)
                || index < rest_type.target_tuple_type().fixed_length()
            {
                let index_type = self.get_number_literal_type(Number(index as f64));
                return Some(self.get_indexed_access_type(rest_type, index_type));
            }
        }
        None
    }

    // relater.go:1819
    // Return the rest type at the given position, transforming `any[]` into just `any`. We do this because
    // in signatures we want `any[]` in a rest position to be compatible with anything, but `any[]` isn't
    // assignable to tuple types with required elements.
    fn get_rest_or_any_type_at_position(&mut self, source: P<Signature>, pos: usize) -> P<Type> {
        let rest_type = self.get_rest_type_at_position(source, pos, false);
        if let Some(element_type) = self.get_element_type_of_array_type(rest_type)
            && is_type_any(Some(element_type))
        {
            return self.any_type;
        }
        rest_type
    }

    // relater.go:1829
    pub(crate) fn get_rest_type_at_position(
        &mut self,
        source: P<Signature>,
        pos: usize,
        readonly: bool,
    ) -> P<Type> {
        let parameter_count = self.get_parameter_count(source);
        let min_argument_count = self.get_min_argument_count(source);
        let rest_type = self.get_effective_rest_type(source);
        if let Some(rest_type) = rest_type
            && pos >= parameter_count - 1
        {
            if pos == parameter_count - 1 {
                return rest_type;
            }
            let number_type = self.number_type;
            let t = self.get_indexed_access_type(rest_type, number_type);
            return self.create_array_type(t);
        }
        if pos >= parameter_count {
            return self.create_tuple_type_ex(&[], &[], readonly);
        }
        let length = parameter_count - pos;
        let mut types: Vec<P<Type>> = Vec::with_capacity(length);
        let mut infos: Vec<TupleElementInfo> = Vec::with_capacity(length);
        for i in 0..length {
            let flags = if let Some(rest_type) = rest_type
                && i == length - 1
            {
                types.push(rest_type);
                ElementFlags::Variadic
            } else {
                types.push(self.get_type_at_position(source, i + pos));
                if i + pos < min_argument_count {
                    ElementFlags::Required
                } else {
                    ElementFlags::Optional
                }
            };
            let labeled_declaration = self.get_nameable_declaration_at_position(source, i + pos);
            infos.push(TupleElementInfo { flags, labeled_declaration });
        }
        self.create_tuple_type_ex(&types, &infos, readonly)
    }

    // relater.go:1860
    fn get_nameable_declaration_at_position(
        &mut self,
        signature: P<Signature>,
        pos: usize,
    ) -> Option<P<Node>> {
        let param_count =
            signature.parameters().len() - usize::from(signature_has_rest_parameter(signature));
        if pos < param_count {
            let decl = signature.parameters()[pos].value_declaration();
            if let Some(decl) = decl
                && Self::is_valid_declaration_for_tuple_label(decl)
            {
                return Some(decl);
            }
            return None;
        }
        if signature_has_rest_parameter(signature) {
            let rest_parameter = signature.parameters()[param_count];
            let rest_type = self.get_type_of_symbol(rest_parameter);
            if is_tuple_type(rest_type) {
                let element_infos = rest_type.target_tuple_type().element_infos();
                let index = pos - param_count;
                if index < element_infos.len() {
                    return element_infos[index].labeled_declaration;
                }
                return None;
            }
            if let Some(value_declaration) = rest_parameter.value_declaration()
                && Self::is_valid_declaration_for_tuple_label(value_declaration)
            {
                return Some(value_declaration);
            }
        }
        None
    }

    // relater.go:1887
    fn is_valid_declaration_for_tuple_label(d: P<Node>) -> bool {
        is_named_tuple_member(d)
            || is_parameter_declaration(d) && d.name().is_some() && is_identifier(d.name().unwrap())
    }

    // relater.go:1891
    pub(crate) fn get_non_array_rest_type(&mut self, signature: P<Signature>) -> Option<P<Type>> {
        let rest_type = self.get_effective_rest_type(signature);
        if let Some(rest_type) = rest_type
            && !self.is_array_type(rest_type)
            && !is_type_any(Some(rest_type))
        {
            return Some(rest_type);
        }
        None
    }

    // relater.go:1899
    pub(crate) fn get_effective_rest_type(&mut self, signature: P<Signature>) -> Option<P<Type>> {
        if signature_has_rest_parameter(signature) {
            let rest_type =
                self.get_type_of_symbol(signature.parameters()[signature.parameters().len() - 1]);
            if !is_tuple_type(rest_type) {
                if is_type_any(Some(rest_type)) {
                    return Some(self.any_array_type);
                }
                return Some(rest_type);
            }
            if rest_type.target_tuple_type().combined_flags.get().intersects(ElementFlags::Variable)
            {
                let fixed_length = rest_type.target_tuple_type().fixed_length();
                return Some(self.slice_tuple_type(rest_type, fixed_length, 0));
            }
        }
        None
    }

    // relater.go:1915
    pub(crate) fn slice_tuple_type(
        &mut self,
        t: P<Type>,
        index: usize,
        end_skip_count: usize,
    ) -> P<Type> {
        let target = t.target_tuple_type();
        let end_index = Self::get_type_reference_arity(t).saturating_sub(end_skip_count);
        if index > target.fixed_length() {
            if let Some(rest_array_type) = self.get_rest_array_type_of_tuple_type(t) {
                return rest_array_type;
            }
            return self.create_tuple_type(&[]);
        }
        if index >= end_index {
            return self.create_tuple_type(&[]);
        }
        let type_arguments = self.get_type_arguments(t);
        self.create_tuple_type_ex(
            &type_arguments[index..end_index],
            &target.element_infos()[index..end_index],
            false, /*readonly*/
        )
    }

    // relater.go:1930
    pub(crate) fn get_known_keys_of_tuple_type(&mut self, t: P<Type>) -> P<Type> {
        let fixed_length = t.target_tuple_type().fixed_length();
        let mut keys: Vec<P<Type>> = Vec::with_capacity(fixed_length + 1);
        for i in 0..fixed_length {
            keys.push(self.get_string_literal_type(&i.to_string()));
        }
        let array_type = if t.target_tuple_type().readonly.get() {
            self.global_readonly_array_type
        } else {
            self.global_array_type
        };
        keys.push(self.get_index_type(array_type));
        self.get_union_type(&keys)
    }

    // relater.go:1940
    fn get_rest_array_type_of_tuple_type(&mut self, t: P<Type>) -> Option<P<Type>> {
        if let Some(rest_type) = self.get_rest_type_of_tuple_type(t) {
            return Some(self.create_array_type(rest_type));
        }
        None
    }

    // relater.go:1947
    pub(crate) fn get_this_type_of_signature(
        &mut self,
        signature: P<Signature>,
    ) -> Option<P<Type>> {
        if let Some(this_parameter) = signature.this_parameter() {
            return Some(self.get_type_of_symbol(this_parameter));
        }
        None
    }

    // relater.go:1954
    fn is_instantiated_generic_parameter(&mut self, signature: P<Signature>, pos: usize) -> bool {
        let Some(target) = signature.target() else {
            return false;
        };
        let t = self.try_get_type_at_position(target, pos);
        t.is_some_and(|t| self.is_generic_type(t))
    }

    // relater.go:1962
    pub(crate) fn get_parameter_name_at_position(
        &mut self,
        signature: P<Signature>,
        pos: usize,
    ) -> String {
        let param_count =
            signature.parameters().len() - usize::from(signature_has_rest_parameter(signature));
        if pos < param_count {
            return signature.parameters()[pos].name().to_string();
        }
        let rest_parameter = signature.parameters()[param_count];
        let rest_type = self.get_type_of_symbol(rest_parameter);
        if is_tuple_type(rest_type) {
            let index = pos - param_count;
            let element_info = rest_type.target_tuple_type().element_infos()[index];
            return Self::get_tuple_element_label(element_info, Some(rest_parameter), index);
        }
        rest_parameter.name().to_string()
    }

    // relater.go:1976
    pub(crate) fn get_tuple_element_label(
        element_info: TupleElementInfo,
        rest_symbol: Option<P<Symbol>>,
        index: usize,
    ) -> String {
        if let Some(labeled_declaration) = element_info.labeled_declaration {
            return labeled_declaration.name().unwrap().text().to_string();
        }
        if let Some(rest_symbol) = rest_symbol
            && let Some(value_declaration) = rest_symbol.value_declaration()
            && is_parameter_declaration(value_declaration)
        {
            return Self::get_tuple_element_label_from_binding_element(
                value_declaration,
                index,
                element_info.flags,
            );
        }
        let root_name =
            if let Some(rest_symbol) = rest_symbol { rest_symbol.name() } else { "arg" };
        format!("{root_name}_{index}")
    }

    // relater.go:1992
    fn get_tuple_element_label_from_binding_element(
        node: P<Node>,
        index: usize,
        element_flags: ElementFlags,
    ) -> String {
        if let Some(name_node) = node.name() {
            match name_node.kind() {
                Kind::Identifier => {
                    let name = name_node.text();
                    if has_dot_dot_dot_token(node) {
                        // given
                        //   (...[x, y, ...z]: [number, number, ...number[]]) => ...
                        // this produces
                        //   (x: number, y: number, ...z: number[]) => ...
                        // which preserves rest elements of 'z'

                        // given
                        //   (...[x, y, ...z]: [number, number, ...[...number[], number]]) => ...
                        // this produces
                        //   (x: number, y: number, ...z: number[], z_1: number) => ...
                        // which preserves rest elements of z but gives distinct numbers to fixed elements of 'z'
                        if element_flags.intersects(ElementFlags::Variable) {
                            return name.to_string();
                        }
                        return format!("{name}_{index}");
                    }
                    // given
                    //   (...[x]: [number]) => ...
                    // this produces
                    //   (x: number) => ...
                    // which preserves fixed elements of 'x'

                    // given
                    //   (...[x]: ...number[]) => ...
                    // this produces
                    //   (x_0: number) => ...
                    // which which numbers fixed elements of 'x' whose tuple element type is variable
                    if element_flags.intersects(ElementFlags::Fixed) {
                        return name.to_string();
                    }
                    return format!("{name}_n");
                }
                Kind::ArrayBindingPattern if has_dot_dot_dot_token(node) => {
                    let elements = name_node.elements();
                    let last_element = last_or_nil(elements);
                    let last_element_is_binding_element_rest = last_element
                        .is_some_and(|e| is_binding_element(e) && has_dot_dot_dot_token(e));
                    let element_count =
                        elements.len() - usize::from(last_element_is_binding_element_rest);
                    if index < element_count {
                        let element = elements[index];
                        if is_binding_element(element) {
                            return Self::get_tuple_element_label_from_binding_element(
                                element,
                                index,
                                element_flags,
                            );
                        }
                    } else if last_element_is_binding_element_rest {
                        return Self::get_tuple_element_label_from_binding_element(
                            last_element.unwrap(),
                            index - element_count,
                            element_flags,
                        );
                    }
                }
                _ => {}
            }
        }
        format!("arg_{index}")
    }

    // relater.go:2049
    pub fn get_type_predicate_of_signature(
        &mut self,
        sig: P<Signature>,
    ) -> Option<P<TypePredicate>> {
        if sig.resolved_type_predicate(self.no_type_predicate).is_none() {
            if let Some(target) = sig.target() {
                let target_type_predicate = self.get_type_predicate_of_signature(target);
                if let Some(target_type_predicate) = target_type_predicate {
                    let predicate = self.instantiate_type_predicate(
                        target_type_predicate,
                        sig.mapper.get().unwrap(),
                    );
                    sig.set_resolved_type_predicate(Some(predicate), self.no_type_predicate);
                }
            } else if let Some(composite) = sig.composite() {
                let predicate = self.get_union_or_intersection_type_predicate(
                    composite.signatures.get(),
                    composite.is_union.get(),
                );
                sig.set_resolved_type_predicate(predicate, self.no_type_predicate);
            } else if let Some(declaration) = sig.declaration() {
                let type_node = declaration.type_node();
                if let Some(type_node) = type_node {
                    if is_type_predicate_node(type_node) {
                        let predicate =
                            self.create_type_predicate_from_type_predicate_node(type_node, sig);
                        sig.set_resolved_type_predicate(Some(predicate), self.no_type_predicate);
                    }
                } else if is_function_like_declaration(declaration)
                    && sig
                        .resolved_return_type
                        .get()
                        .is_none_or(|t| t.flags().intersects(TypeFlags::Boolean))
                    && self.get_parameter_count(sig) > 0
                {
                    sig.set_resolved_type_predicate(
                        Some(self.no_type_predicate),
                        self.no_type_predicate,
                    ); // avoid infinite loop
                    let predicate = self.get_type_predicate_from_body(declaration);
                    sig.set_resolved_type_predicate(predicate, self.no_type_predicate);
                }
            }
            if sig.resolved_type_predicate(self.no_type_predicate).is_none() {
                sig.set_resolved_type_predicate(
                    Some(self.no_type_predicate),
                    self.no_type_predicate,
                );
            }
        }
        if sig.resolved_type_predicate(self.no_type_predicate) == Some(self.no_type_predicate) {
            return None;
        }
        sig.resolved_type_predicate(self.no_type_predicate)
    }

    // relater.go:2083
    fn get_union_or_intersection_type_predicate(
        &mut self,
        signatures: &[P<Signature>],
        is_union: bool,
    ) -> Option<P<TypePredicate>> {
        let mut last: Option<P<TypePredicate>> = None;
        let mut types: Vec<P<Type>> = Vec::new();
        for &sig in signatures {
            let pred = self.get_type_predicate_of_signature(sig);
            if let Some(pred) = pred {
                // Constituent type predicates must all have matching kinds. We don't create composite type predicates for assertions.
                if pred.kind() != TypePredicateKind::This
                    && pred.kind() != TypePredicateKind::Identifier
                    || last.is_some() && !Self::type_predicate_kinds_match(last.unwrap(), pred)
                {
                    return None;
                }
                last = Some(pred);
                types.push(pred.type_().unwrap());
            } else {
                // In composite union signatures we permit and ignore signatures with a return type `false`.
                let mut return_type: Option<P<Type>> = None;
                if is_union {
                    return_type = Some(self.get_return_type_of_signature(sig));
                }
                if return_type != Some(self.false_type)
                    && return_type != Some(self.regular_false_type)
                {
                    return None;
                }
            }
        }
        let last = last?;
        let composite_type =
            self.get_union_or_intersection_type(&types, is_union, UnionReduction::Literal);
        Some(Self::new_type_predicate(
            last.kind(),
            last.parameter_name(),
            last.parameter_index(),
            Some(composite_type),
        ))
    }

    // relater.go:2113
    pub(crate) fn type_predicate_kinds_match(a: P<TypePredicate>, b: P<TypePredicate>) -> bool {
        a.kind() == b.kind() && a.parameter_index() == b.parameter_index()
    }

    // relater.go:2117
    fn create_type_predicate_from_type_predicate_node(
        &mut self,
        node: P<Node>,
        signature: P<Signature>,
    ) -> P<TypePredicate> {
        let predicate_node = node.as_type_predicate_node();
        let mut t: Option<P<Type>> = None;
        if let Some(type_node) = predicate_node.type_ {
            t = Some(self.get_type_from_type_node(type_node));
        }
        if is_this_type_node(predicate_node.parameter_name) {
            let kind = if predicate_node.asserts_modifier.is_some() {
                TypePredicateKind::AssertsThis
            } else {
                TypePredicateKind::This
            };
            return Self::new_type_predicate(
                kind,
                "",      /*parameterName*/
                Some(0), /*parameterIndex*/
                t,
            );
        }
        let kind = if predicate_node.asserts_modifier.is_some() {
            TypePredicateKind::AssertsIdentifier
        } else {
            TypePredicateKind::Identifier
        };
        let name = predicate_node.parameter_name.text();
        let index = find_index(signature.parameters(), |p| p.name() == name);
        Self::new_type_predicate(kind, name, index, t)
    }

    // relater.go:2133
    pub(crate) fn instantiate_type_predicate(
        &mut self,
        predicate: P<TypePredicate>,
        mapper: P<TypeMapper>,
    ) -> P<TypePredicate> {
        // Go instantiateType returns nil for a nil type.
        let t = predicate.type_().map(|t| self.instantiate_type(t, Some(mapper)));
        if t == predicate.type_() {
            return predicate;
        }
        Self::new_type_predicate(
            predicate.kind(),
            predicate.parameter_name(),
            predicate.parameter_index(),
            t,
        )
    }

    // relater.go:2141
    pub(crate) fn new_type_predicate(
        kind: TypePredicateKind,
        parameter_name: &str,
        parameter_index: Option<usize>,
        t: Option<P<Type>>,
    ) -> P<TypePredicate> {
        P::new(TypePredicate {
            kind: Cell::new(kind),
            parameter_index: Cell::new(parameter_index),
            parameter_name: Cell::new(alloc_str(parameter_name)),
            t: Cell::new(t),
        })
    }

    // relater.go:2145
    pub(crate) fn is_resolving_return_type_of_signature(
        &mut self,
        signature: P<Signature>,
    ) -> bool {
        if let Some(composite) = signature.composite()
            && composite
                .signatures
                .get()
                .iter()
                .any(|&s| self.is_resolving_return_type_of_signature(s))
        {
            return true;
        }
        signature.resolved_return_type.get().is_none()
            && self
                .find_resolution_cycle_start_index(
                    TypeSystemEntity::Signature(signature),
                    TypeSystemPropertyName::ResolvedReturnType,
                )
                .is_some()
    }

    // relater.go:2152
    pub(crate) fn find_matching_signatures(
        &mut self,
        signature_lists: &[Vec<P<Signature>>],
        signature: P<Signature>,
        list_index: usize,
    ) -> Vec<P<Signature>> {
        if !signature.type_parameters().is_empty() {
            // We require an exact match for generic signatures, so we only return signatures from the first
            // signature list and only if they have exact matches in the other signature lists.
            if list_index != 0 {
                return Vec::new();
            }
            for signature_list in signature_lists.iter().skip(1) {
                if self
                    .find_matching_signature(
                        signature_list,
                        signature,
                        false, /*partialMatch*/
                        false, /*ignoreThisTypes*/
                        false, /*ignoreReturnTypes*/
                    )
                    .is_none()
                {
                    return Vec::new();
                }
            }
            return vec![signature];
        }
        let mut result: Vec<P<Signature>> = Vec::new();
        for (i, signature_list) in signature_lists.iter().enumerate() {
            // Allow matching non-generic signatures to have excess parameters (as a fallback if exact parameter match is not found) and different return types.
            // Prefer matching this types if possible.
            let m = if i == list_index {
                Some(signature)
            } else {
                let m = self.find_matching_signature(
                    signature_list,
                    signature,
                    false, /*partialMatch*/
                    false, /*ignoreThisTypes*/
                    true,  /*ignoreReturnTypes*/
                );
                if m.is_none() {
                    self.find_matching_signature(
                        signature_list,
                        signature,
                        true,  /*partialMatch*/
                        false, /*ignoreThisTypes*/
                        true,  /*ignoreReturnTypes*/
                    )
                } else {
                    m
                }
            };
            let Some(m) = m else {
                return Vec::new();
            };
            append_if_unique(&mut result, m);
        }
        result
    }

    // relater.go:2187
    pub(crate) fn find_matching_signature(
        &mut self,
        signature_list: &[P<Signature>],
        signature: P<Signature>,
        partial_match: bool,
        ignore_this_types: bool,
        ignore_return_types: bool,
    ) -> Option<P<Signature>> {
        for &s in signature_list {
            let compare_types = |c: &mut Checker, s: P<Type>, t: P<Type>| {
                if partial_match {
                    c.compare_types_subtype_of(s, t)
                } else {
                    c.compare_types_identical(s, t)
                }
            };
            if self.compare_signatures_identical(
                s,
                signature,
                partial_match,
                ignore_this_types,
                ignore_return_types,
                compare_types,
            ) != Ternary::FALSE
            {
                return Some(s);
            }
        }
        None
    }

    // relater.go:2200
    /**
     * See signatureRelatedTo, compareSignaturesIdentical
     */
    pub(crate) fn compare_signatures_identical(
        &mut self,
        source: P<Signature>,
        target: P<Signature>,
        partial_match: bool,
        ignore_this_types: bool,
        ignore_return_types: bool,
        compare_types: impl FnMut(&mut Checker, P<Type>, P<Type>) -> Ternary,
    ) -> Ternary {
        let mut compare_types = compare_types;
        let mut source = source;
        if source == target {
            return Ternary::TRUE;
        }
        if !self.is_matching_signature(source, target, partial_match) {
            return Ternary::FALSE;
        }
        // Check that the two signatures have the same number of type parameters.
        if source.type_parameters().len() != target.type_parameters().len() {
            return Ternary::FALSE;
        }
        // Check that type parameter constraints and defaults match. If they do, instantiate the source
        // signature with the type parameters of the target signature and continue the comparison.
        if !target.type_parameters().is_empty() {
            let mapper = new_type_mapper(source.type_parameters(), target.type_parameters());
            for i in 0..target.type_parameters().len() {
                let s = source.type_parameters()[i];
                let t = target.type_parameters()[i];
                if !(s == t || {
                    let sc = self.get_constraint_or_unknown_from_type_parameter(s);
                    let sc = self.instantiate_type(sc, Some(mapper));
                    let tc = self.get_constraint_or_unknown_from_type_parameter(t);
                    compare_types(self, sc, tc) != Ternary::FALSE
                } && {
                    let sd = self.get_default_or_unknown_from_type_parameter(s);
                    let sd = self.instantiate_type(sd, Some(mapper));
                    let td = self.get_default_or_unknown_from_type_parameter(t);
                    compare_types(self, sd, td) != Ternary::FALSE
                }) {
                    return Ternary::FALSE;
                }
            }
            source =
                self.instantiate_signature_ex(source, mapper, true /*eraseTypeParameters*/);
        }
        let mut result = Ternary::TRUE;
        if !ignore_this_types {
            let source_this_type = self.get_this_type_of_signature(source);
            if let Some(source_this_type) = source_this_type {
                let target_this_type = self.get_this_type_of_signature(target);
                if let Some(target_this_type) = target_this_type {
                    let related = compare_types(self, source_this_type, target_this_type);
                    if related == Ternary::FALSE {
                        return Ternary::FALSE;
                    }
                    result &= related;
                }
            }
        }
        let target_parameter_count = self.get_parameter_count(target);
        for i in 0..target_parameter_count {
            let s = self.get_type_at_position(source, i);
            let t = self.get_type_at_position(target, i);
            let related = compare_types(self, t, s);
            if related == Ternary::FALSE {
                return Ternary::FALSE;
            }
            result &= related;
        }
        if !ignore_return_types {
            let source_type_predicate = self.get_type_predicate_of_signature(source);
            let target_type_predicate = self.get_type_predicate_of_signature(target);
            if source_type_predicate.is_some() || target_type_predicate.is_some() {
                result &= self.compare_type_predicates_identical(
                    source_type_predicate,
                    target_type_predicate,
                    &mut compare_types,
                );
            } else {
                let s = self.get_return_type_of_signature(source);
                let t = self.get_return_type_of_signature(target);
                result &= compare_types(self, s, t);
            }
        }
        result
    }

    // relater.go:2260
    fn is_matching_signature(
        &mut self,
        source: P<Signature>,
        target: P<Signature>,
        partial_match: bool,
    ) -> bool {
        let source_parameter_count = self.get_parameter_count(source);
        let target_parameter_count = self.get_parameter_count(target);
        let source_min_argument_count = self.get_min_argument_count(source);
        let target_min_argument_count = self.get_min_argument_count(target);
        let source_has_rest_parameter = self.has_effective_rest_parameter(source);
        let target_has_rest_parameter = self.has_effective_rest_parameter(target);
        // A source signature matches a target signature if the two signatures have the same number of required,
        // optional, and rest parameters.
        if source_parameter_count == target_parameter_count
            && source_min_argument_count == target_min_argument_count
            && source_has_rest_parameter == target_has_rest_parameter
        {
            return true;
        }
        // A source signature partially matches a target signature if the target signature has no fewer required
        // parameters
        if partial_match && source_min_argument_count <= target_min_argument_count {
            return true;
        }
        false
    }

    // relater.go:2280
    pub(crate) fn compare_type_parameters_identical(
        &mut self,
        source_params: &[P<Type>],
        target_params: &[P<Type>],
    ) -> bool {
        if source_params.len() != target_params.len() {
            return false;
        }
        let mapper = new_type_mapper(alloc_slice(target_params), alloc_slice(source_params));
        for i in 0..source_params.len() {
            let source = source_params[i];
            let target = target_params[i];
            if source == target {
                continue;
            }
            // We instantiate the target type parameter constraints into the source types so we can recognize `<T, U extends T>` as the same as `<A, B extends A>`
            let source_constraint =
                self.get_constraint_from_type_parameter(source).unwrap_or(self.unknown_type);
            let target_constraint =
                self.get_constraint_from_type_parameter(target).unwrap_or(self.unknown_type);
            let target_constraint = self.instantiate_type(target_constraint, Some(mapper));
            if !self.is_type_identical_to(source_constraint, target_constraint) {
                return false;
            }
            // We don't compare defaults - we just use the type parameter defaults from the first signature that seems to match.
            // It might make sense to combine these defaults in the future, but doing so intelligently requires knowing
            // if the parameter is used covariantly or contravariantly (so we intersect if it's used like a parameter or union if used like a return type)
            // and, since it's just an inference _default_, just picking one arbitrarily works OK.
        }
        true
    }

    // relater.go:2303
    fn compare_type_predicates_identical(
        &mut self,
        source: Option<P<TypePredicate>>,
        target: Option<P<TypePredicate>>,
        compare_types: impl FnMut(&mut Checker, P<Type>, P<Type>) -> Ternary,
    ) -> Ternary {
        let mut compare_types = compare_types;
        let (Some(source), Some(target)) = (source, target) else {
            return Ternary::FALSE;
        };
        if !Self::type_predicate_kinds_match(source, target) {
            return Ternary::FALSE;
        }
        if source.type_() == target.type_() {
            return Ternary::TRUE;
        }
        if let (Some(st), Some(tt)) = (source.type_(), target.type_()) {
            return compare_types(self, st, tt);
        }
        Ternary::FALSE
    }

    // relater.go:2315
    pub(crate) fn get_effective_constraint_of_intersection(
        &mut self,
        types: &[P<Type>],
        target_is_union: bool,
    ) -> Option<P<Type>> {
        let mut constraints: Vec<P<Type>> = Vec::new();
        let mut has_disjoint_domain_type = false;
        for &t in types {
            if t.flags().intersects(TypeFlags::Instantiable) {
                // We keep following constraints as long as we have an instantiable type that is known
                // not to be circular or infinite (hence we stop on index access types).
                let mut constraint = self.get_constraint_of_type(t);
                while let Some(c) = constraint.filter(|c| {
                    c.flags().intersects(
                        TypeFlags::TypeParameter | TypeFlags::Index | TypeFlags::Conditional,
                    )
                }) {
                    constraint = self.get_constraint_of_type(c);
                }
                if let Some(constraint) = constraint {
                    constraints.push(constraint);
                    if target_is_union {
                        constraints.push(t);
                    }
                }
            } else if t.flags().intersects(TypeFlags::DisjointDomains)
                || self.is_empty_anonymous_object_type(t)
            {
                has_disjoint_domain_type = true;
            }
        }
        // If the target is a union type or if we are intersecting with types belonging to one of the
        // disjoint domains, we may end up producing a constraint that hasn't been examined before.
        // (Go's non-nil constraints slice is never empty.)
        if !constraints.is_empty() && (target_is_union || has_disjoint_domain_type) {
            if has_disjoint_domain_type {
                // We add any types belong to one of the disjoint domains because they might cause the final
                // intersection operation to reduce the union constraints.
                for &t in types {
                    if t.flags().intersects(TypeFlags::DisjointDomains)
                        || self.is_empty_anonymous_object_type(t)
                    {
                        constraints.push(t);
                    }
                }
            }
            // The source types were normalized; ensure the result is normalized too.
            let intersection = self.get_intersection_type_ex(
                &constraints,
                IntersectionFlags::NoConstraintReduction,
                AliasArg::None,
            );
            return Some(self.get_normalized_type(intersection, false /*writing*/));
        }
        None
    }

    // relater.go:2354
    pub(crate) fn template_literal_types_definitely_unrelated(
        source: &'static TemplateLiteralType,
        target: &'static TemplateLiteralType,
    ) -> bool {
        // Two template literal types with differences in their starting or ending text spans are definitely unrelated.
        let source_texts = source.texts();
        let target_texts = target.texts();
        let source_start = source_texts[0].as_bytes();
        let target_start = target_texts[0].as_bytes();
        let source_end = source_texts[source_texts.len() - 1].as_bytes();
        let target_end = target_texts[target_texts.len() - 1].as_bytes();
        let start_len = source_start.len().min(target_start.len());
        let end_len = source_end.len().min(target_end.len());
        let source_suffix = &source_end[source_end.len() - end_len..];
        let target_suffix = &target_end[target_end.len() - end_len..];
        source_start[..start_len] != target_start[..start_len] || source_suffix != target_suffix
    }

    // relater.go:2365
    pub(crate) fn is_type_matched_by_template_literal_type(
        &mut self,
        source: P<Type>,
        target: &'static TemplateLiteralType,
        compare_types: TypeComparer,
    ) -> bool {
        let inferences = self.infer_types_from_template_literal_type(source, target, compare_types);
        // An empty result stands for Go's nil (a successful inference is never empty).
        if !inferences.is_empty() {
            if inferences.iter().enumerate().any(|(i, &inference)| {
                !self.is_valid_type_for_template_literal_placeholder(
                    inference,
                    target.types()[i],
                    compare_types,
                )
            }) {
                return false;
            }
            return true;
        }
        false
    }

    // relater.go:2378
    // (An empty result stands for Go's nil: a non-nil result always has one element per target placeholder.)
    pub(crate) fn infer_types_from_template_literal_type(
        &mut self,
        source: P<Type>,
        target: &'static TemplateLiteralType,
        compare_types: TypeComparer,
    ) -> Vec<P<Type>> {
        if source.flags().intersects(TypeFlags::StringLiteral) {
            let value = get_string_literal_value(source);
            return self.infer_from_literal_parts_to_template_literal(&[value], &[], target);
        }
        if source.flags().intersects(TypeFlags::TemplateLiteral) {
            let source_template = source.as_template_literal_type();
            if source_template.texts() == target.texts() {
                let mut result: Vec<P<Type>> = Vec::with_capacity(source_template.types().len());
                result.extend(source_template.types().iter().enumerate().map(|(i, &s)| {
                    let a = self.get_base_constraint_or_type(s);
                    let b = self.get_base_constraint_or_type(target.types()[i]);
                    if compare_types(self, a, b, false /*partialMatch*/) == Ternary::FALSE {
                        self.get_string_like_type_for_type(s)
                    } else {
                        s
                    }
                }));
                return result;
            }
            return self.infer_from_literal_parts_to_template_literal(
                source_template.texts(),
                source_template.types(),
                target,
            );
        }
        Vec::new()
    }

    // relater.go:2418
    // This function infers from the text parts and type parts of a source literal to a target template literal. The number
    // of text parts is always one more than the number of type parts, and a source string literal is treated as a source
    // with one text part and zero type parts. The function returns an array of inferred string or template literal types
    // corresponding to the placeholders in the target template literal, or undefined if the source doesn't match the target.
    //
    // We first check that the starting source text part matches the starting target text part, and that the ending source
    // text part ends matches the ending target text part. We then iterate through the remaining target text parts, finding
    // a match for each in the source and inferring string or template literal types created from the segments of the source
    // that occur between the matches. During this iteration, seg holds the index of the current text part in the sourceTexts
    // array and pos holds the current character position in the current text part.
    //
    // Consider inference from type `<<${string}>.<${number}-${number}>>` to type `<${string}.${string}>`, i.e.
    //
    //	sourceTexts = ['<<', '>.<', '-', '>>']
    //	sourceTypes = [string, number, number]
    //	target.texts = ['<', '.', '>']
    //
    // We first match '<' in the target to the start of '<<' in the source and '>' in the target to the end of '>>' in
    // the source. The first match for the '.' in target occurs at character 1 in the source text part at index 1, and thus
    // the first inference is the template literal type `<${string}>`. The remainder of the source makes up the second
    // inference, the template literal type `<${number}-${number}>`.
    // (An empty result stands for Go's nil.)
    fn infer_from_literal_parts_to_template_literal(
        &mut self,
        source_texts: &[&str],
        source_types: &[P<Type>],
        target: &'static TemplateLiteralType,
    ) -> Vec<P<Type>> {
        let last_source_index = source_texts.len() - 1;
        let source_start_text = source_texts[0];
        let source_end_text = source_texts[last_source_index];
        let target_texts = target.texts();
        let last_target_index = target_texts.len() - 1;
        let target_start_text = target_texts[0];
        let target_end_text = target_texts[last_target_index];
        if last_source_index == 0
            && source_start_text.len() < target_start_text.len() + target_end_text.len()
            || !source_start_text.starts_with(target_start_text)
            || !source_end_text.ends_with(target_end_text)
        {
            return Vec::new();
        }
        let remaining_end_text = &source_end_text[..source_end_text.len() - target_end_text.len()];
        let mut seg: usize = 0;
        let mut pos: usize = target_start_text.len();
        let mut matches: Vec<P<Type>> = Vec::new();
        let get_source_text = |index: usize| -> &str {
            if index < last_source_index {
                return source_texts[index];
            }
            remaining_end_text
        };
        let add_match = |c: &mut Checker,
                         s: usize,
                         p: usize,
                         seg: &mut usize,
                         pos: &mut usize,
                         matches: &mut Vec<P<Type>>| {
            let match_type = if s == *seg {
                let text = stringutil::combine_surrogate_pairs(&get_source_text(s)[*pos..p]);
                c.get_string_literal_type(&text)
            } else {
                let mut match_texts: Vec<&str> = Vec::with_capacity(s - *seg + 1);
                match_texts.push(&source_texts[*seg][*pos..]);
                match_texts.extend_from_slice(&source_texts[*seg + 1..s]);
                match_texts.push(&get_source_text(s)[..p]);
                c.get_template_literal_type(&match_texts, &source_types[*seg..s])
            };
            matches.push(match_type);
            *seg = s;
            *pos = p;
        };
        for &delim in target_texts.iter().take(last_target_index).skip(1) {
            if !delim.is_empty() {
                let mut s = seg;
                let mut p = pos;
                loop {
                    if let Some(d) = get_source_text(s)[p..].find(delim) {
                        p += d;
                        break;
                    }
                    s += 1;
                    if s == source_texts.len() {
                        return Vec::new();
                    }
                    p = 0;
                }
                add_match(self, s, p, &mut seg, &mut pos, &mut matches);
                pos += delim.len();
            } else if pos < get_source_text(seg).len() {
                let source_text = get_source_text(seg);
                // Consume one code point at a time, matching the string iterator
                // (`[x, ..._] = s`) rather than UTF-16 code-unit indexing (`s[0]`).
                // DecodeJSStringRune is required rather than utf8.DecodeRuneInString
                // because a lone surrogate is stored as an invalid-UTF-8 sentinel;
                // utf8 would treat that as an error and advance a single byte,
                // breaking the sentinel into stray bytes, whereas DecodeJSStringRune
                // pulls the whole sentinel off as one code point.
                //
                // This intentionally diverges from Strada, which advances one UTF-16
                // code unit at a time (`s[0]` semantics) and therefore splits a
                // supplementary code point such as an emoji into its surrogate
                // halves. If we ever need to match that, expand sourceTexts and
                // targetTexts into code-unit space up front with a SplitSurrogatePairs
                // helper (the inverse of CombineSurrogatePairs) and decode by code
                // unit here; the CombineSurrogatePairs call in addMatch already
                // recombines captured halves back into canonical form.
                let (_, size) = stringutil::decode_js_string_rune(&source_text[pos..]);
                add_match(self, seg, pos + size, &mut seg, &mut pos, &mut matches);
            } else if seg < last_source_index {
                add_match(self, seg + 1, 0, &mut seg, &mut pos, &mut matches);
            } else {
                return Vec::new();
            }
        }
        add_match(
            self,
            last_source_index,
            get_source_text(last_source_index).len(),
            &mut seg,
            &mut pos,
            &mut matches,
        );
        matches
    }

    // relater.go:2502
    fn get_string_like_type_for_type(&mut self, t: P<Type>) -> P<Type> {
        if t.flags().intersects(TypeFlags::Any | TypeFlags::StringLike) {
            return t;
        }
        self.get_template_literal_type(&["", ""], &[t])
    }

    // relater.go:2509
    fn is_valid_type_for_template_literal_placeholder(
        &mut self,
        source: P<Type>,
        target: P<Type>,
        compare_types: TypeComparer,
    ) -> bool {
        if target.flags().intersects(TypeFlags::Intersection) {
            return target.types().iter().all(|&t| {
                t == self.empty_type_literal_type
                    || self.is_valid_type_for_template_literal_placeholder(source, t, compare_types)
            });
        }
        if target.flags().intersects(TypeFlags::String)
            || compare_types(self, source, target, false) != Ternary::FALSE
        {
            return true;
        }
        if source.flags().intersects(TypeFlags::StringLiteral) {
            let value = get_string_literal_value(source);
            return target.flags().intersects(TypeFlags::Number)
                && is_valid_number_string(value, false /*roundTripOnly*/)
                || target.flags().intersects(TypeFlags::BigInt)
                    && is_valid_big_int_string(value, false /*roundTripOnly*/)
                || target.flags().intersects(TypeFlags::BooleanLiteral | TypeFlags::Nullable)
                    && value == target.as_intrinsic_type().intrinsic_name()
                || target.flags().intersects(TypeFlags::StringMapping)
                    && self.is_member_of_string_mapping(source, target)
                || target.flags().intersects(TypeFlags::TemplateLiteral)
                    && self.is_type_matched_by_template_literal_type(
                        source,
                        target.as_template_literal_type(),
                        compare_types,
                    );
        }
        if source.flags().intersects(TypeFlags::TemplateLiteral) {
            let texts = source.as_template_literal_type().texts();
            return texts.len() == 2
                && texts[0].is_empty()
                && texts[1].is_empty()
                && compare_types(
                    self,
                    source.as_template_literal_type().types()[0],
                    target,
                    false,
                ) != Ternary::FALSE;
        }
        false
    }

    // relater.go:2531
    pub(crate) fn is_member_of_string_mapping(&mut self, source: P<Type>, target: P<Type>) -> bool {
        if target.flags().intersects(TypeFlags::Any) {
            return true;
        }
        if target.flags().intersects(TypeFlags::String | TypeFlags::TemplateLiteral) {
            return self.is_type_assignable_to(source, target);
        }
        if target.flags().intersects(TypeFlags::StringMapping) {
            // We need to see whether applying the same mappings of the target
            // onto the source would produce an identical type *and* that
            // it's compatible with the inner-most non-string-mapped type.
            //
            // The intuition here is that if same mappings don't affect the source at all,
            // and the source is compatible with the unmapped target, then they must
            // still reside in the same domain.
            let (mapped, inner) = self.apply_target_string_mapping_to_source(source, target);
            return mapped == source && self.is_member_of_string_mapping(source, inner);
        }
        false
    }

    // relater.go:2551
    fn apply_target_string_mapping_to_source(
        &mut self,
        source: P<Type>,
        target: P<Type>,
    ) -> (P<Type>, P<Type>) {
        let mut source = source;
        let mut inner = target.as_string_mapping_type().target.get().unwrap();
        if inner.flags().intersects(TypeFlags::StringMapping) {
            (source, inner) = self.apply_target_string_mapping_to_source(source, inner);
        }
        (self.get_string_mapping_type(target.symbol().unwrap(), source), inner)
    }
}

// relater.go:2559
pub(crate) fn visibility_to_string(flags: ModifierFlags) -> String {
    if flags == ModifierFlags::Private {
        return "private".to_string();
    }
    if flags == ModifierFlags::Protected {
        return "protected".to_string();
    }
    "public".to_string()
}

/// Whether `a` and `b` are literals of one kind (string, number or bigint, not enum members) with different values:
/// no relation holds between them in either direction (`is_type_related_to`).
pub(crate) fn are_different_plain_literals(a: P<Type>, b: P<Type>) -> bool {
    let flags = a.flags();
    flags == b.flags()
        && matches!(
            flags,
            TypeFlags::StringLiteral | TypeFlags::NumberLiteral | TypeFlags::BigIntLiteral
        )
        && a.as_literal_type().regular_type.get() != b.as_literal_type().regular_type.get()
}
