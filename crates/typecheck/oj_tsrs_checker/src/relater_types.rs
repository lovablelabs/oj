//! Non-function declarations of `relater.go`.

use std::fmt::Display;

use crate::direct_bitflags as bitflags;

use crate::*;

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct SignatureCheckMode: u32 {
        const None = 0;
        const BivariantCallback = 1 << 0;
        const StrictCallback = 1 << 1;
        const IgnoreReturnTypes = 1 << 2;
        const StrictArity = 1 << 3;
        const StrictTopSignature = 1 << 4;
        const Callback = Self::BivariantCallback.bits() | Self::StrictCallback.bits();
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct MinArgumentCountFlags: u32 {
        const None = 0;
        const StrongArityForUntypedJS = 1 << 0;
        const VoidIsNonOptional = 1 << 1;
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct IntersectionState: u32 {
        const None = 0;
        const Source = 1 << 0; // Source type is a constituent of an outer intersection
        const Target = 1 << 1; // Target type is a constituent of an outer intersection
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct RecursionFlags: u32 {
        const None = 0;
        const Source = 1 << 0;
        const Target = 1 << 1;
        const Both = Self::Source.bits() | Self::Target.bits();
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct ExpandingFlags: u8 {
        const None = 0;
        const Source = 1 << 0;
        const Target = 1 << 1;
        const Both = Self::Source.bits() | Self::Target.bits();
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct RelationComparisonResult: u32 {
        const None = 0;
        const Succeeded = 1 << 0;
        const Failed = 1 << 1;
        const ReportsUnmeasurable = 1 << 3;
        const ReportsUnreliable = 1 << 4;
        const ComplexityOverflow = 1 << 5;
        const ReportsMask = Self::ReportsUnmeasurable.bits() | Self::ReportsUnreliable.bits();
        const Overflow = Self::ComplexityOverflow.bits();
    }
}

/// Go `type ErrorReporter func(message *diagnostics.Message, args ...any)`, with the checker passed back.
pub(crate) type ErrorReporter<'a> =
    &'a mut dyn FnMut(&mut Checker, &'static Message, &[&dyn Display]);

/// Go `RecursionId{value any}` restricted to `*ast.Node | *ast.Symbol | *Type` (see `asRecursionId`).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RecursionId {
    Node(P<Node>),
    Symbol(P<Symbol>),
    Type(P<Type>),
}

impl From<P<Node>> for RecursionId {
    fn from(v: P<Node>) -> Self {
        RecursionId::Node(v)
    }
}
impl From<P<Symbol>> for RecursionId {
    fn from(v: P<Symbol>) -> Self {
        RecursionId::Symbol(v)
    }
}
impl From<P<Type>> for RecursionId {
    fn from(v: P<Type>) -> Self {
        RecursionId::Type(v)
    }
}

/// Go's relation cache, `map[CacheHashKey]RelationComparisonResult`, in two tables by key form (see `RelationKey`).
#[derive(Default)]
pub struct Relation {
    /// `RelationKey::Pair` keys: each slot is `key << RELATION_RESULT_BITS | result bits` (8 bytes).
    pub(crate) pairs: RefCell<hashbrown::HashTable<u64>>,
    /// `RelationKey::Hashed` keys -> result bits.
    pub(crate) hashed: RefCell<FxHashMap<PackedHashKey, u8>>,
}

/// `RelationComparisonResult` uses bits 0-5.
pub(crate) const RELATION_RESULT_BITS: u32 = 6;

/// A relation cache key.
///
/// Go hashes the key bytes of `getRelationKey` to a 128-bit `CacheHashKey`. Almost every key (99.7% on the
/// private monorepo) is the simple form `'s', source id, target id, intersection state`, which is a triple of
/// small numbers: when the ids are below 2^28 and the state below 2^2 it is packed into 58 bits as `Pair` (an
/// injective mapping, no hashing). Every other key (the `'g'` form of generic references, larger ids) is
/// `Hashed`, the same 128-bit hash Go uses. The two forms never denote the same Go key, since the simple form
/// only ever produces `Pair` keys when it fits and Go's `'s'` and `'g'` byte strings differ.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum RelationKey {
    Pair(u64),
    Hashed(CacheHashKey),
}

impl RelationKey {
    const ID_BITS: u32 = 28;

    /// The `Pair` form of the simple key, if the triple fits.
    #[inline]
    pub(crate) fn pair(
        source: TypeId,
        target: TypeId,
        intersection_state: IntersectionState,
    ) -> Option<RelationKey> {
        let (s, t, i) = (source.0 as u64, target.0 as u64, intersection_state.bits() as u64);
        (s >> Self::ID_BITS == 0 && t >> Self::ID_BITS == 0 && i >> 2 == 0)
            .then_some(RelationKey::Pair(s | (t << Self::ID_BITS) | (i << (2 * Self::ID_BITS))))
    }
}

/// A `CacheHashKey` without alignment, so a hashed-key slot (key + the result's `u8` bits) is 17 bytes instead of
/// 24. Same bits, same `Hash` input (`hi` then `lo`).
#[repr(C, packed)]
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PackedHashKey {
    hi: u64,
    lo: u64,
}

impl From<CacheHashKey> for PackedHashKey {
    #[inline]
    fn from(k: CacheHashKey) -> Self {
        PackedHashKey { hi: k.hi, lo: k.lo }
    }
}

const _: () = assert!(std::mem::size_of::<(PackedHashKey, u8)>() == 17);

/// Go `TypeDiscriminator`; the `c` field is dropped (methods take `c: &mut Checker`).
pub struct TypeDiscriminator<'a> {
    pub(crate) props: Vec<P<Symbol>>,
    pub(crate) is_related_to: &'a mut dyn FnMut(&mut Checker, P<Type>, P<Type>) -> Ternary,
}

/// Go `Discriminator` interface. Implementations delegate to the inherent (generated) methods.
pub trait Discriminator {
    fn len(&mut self, c: &mut Checker) -> usize; // Number of discriminant properties
    fn name(&mut self, c: &mut Checker, index: usize) -> &'static str; // Property name of index-th discriminator
    fn matches(&mut self, c: &mut Checker, index: usize, t: P<Type>) -> bool; // True if index-th discriminator matches the given type
}

#[derive(Default, Clone)]
pub struct ErrorState {
    pub(crate) error_chain: Option<P<ErrorChain>>,
    pub(crate) related_info: Vec<P<Diagnostic>>,
}

pub struct ErrorChain {
    pub(crate) next: Option<P<ErrorChain>>,
    pub(crate) message: &'static Message,
    pub(crate) args: Vec<String>,
}

/// An entry of a relater's source or target stack: the type, and the recursion identity of its recursion identity
/// target once a deeply-nested test has computed it (`is_deeply_nested_on_stack`; 0 before).
pub struct RelaterStackEntry {
    pub(crate) t: P<Type>,
    pub(crate) identity_key: Cell<u64>,
}

impl RelaterStackEntry {
    pub(crate) fn new(t: P<Type>) -> RelaterStackEntry {
        RelaterStackEntry { t, identity_key: Cell::new(0) }
    }
}

/// Go `Relater`. An arena handle (`P<Relater>`, pooled through `Checker::free_relater` like Go); the `c` field is
/// dropped and every method takes `c: &mut Checker`.
#[derive(Default)]
pub struct Relater {
    pub(crate) relation: Cell<Option<P<Relation>>>,
    pub(crate) error_node: Cell<Option<P<Node>>>,
    pub(crate) error_chain: Cell<Option<P<ErrorChain>>>,
    pub(crate) related_info: RefCell<Vec<P<Diagnostic>>>,
    pub(crate) maybe_keys: RefCell<Vec<RelationKey>>,
    pub(crate) maybe_keys_set: RefCell<Set<RelationKey>>,
    pub(crate) source_stack: RefCell<Vec<RelaterStackEntry>>,
    pub(crate) target_stack: RefCell<Vec<RelaterStackEntry>>,
    pub(crate) maybe_count: Cell<usize>,
    pub(crate) source_depth: Cell<usize>,
    pub(crate) target_depth: Cell<usize>,
    pub(crate) expanding_flags: Cell<ExpandingFlags>,
    pub(crate) overflow: Cell<bool>,
    // A remaining budget, not a collection length: Go keeps decrementing it
    // below zero while unwinding a relation and treats every value <= 0 as exhausted.
    pub(crate) relation_count: Cell<isize>,
    pub(crate) next: Cell<Option<P<Relater>>>,
    // The `'static` comparers that wrap this relater's methods (Go passes the method values). They capture only the
    // relater handle (and the intersection state), so they are built once per relater instead of leaking one per call.
    worker_comparer: Cell<Option<TypeComparer>>,
    pub(crate) signature_comparers: RefCell<Vec<(IntersectionState, TypeComparer)>>,
}

impl Relater {
    /// The arena handle of this relater, for closures that must be `'static` (`TypeComparer`), where Go passes
    /// `r.isRelatedToWorker` & co. as values.
    pub(crate) fn as_p(&self) -> P<Relater> {
        // SAFETY: relaters are only created with `P::new` (`Checker::get_relater`) and never freed or moved.
        unsafe { P::from_arena(&*std::ptr::from_ref::<Relater>(self)) }
    }

    /// `type_comparer` over `r.isRelatedToWorker`.
    pub(crate) fn worker_comparer(&self) -> TypeComparer {
        if let Some(f) = self.worker_comparer.get() {
            return f;
        }
        let r = self.as_p();
        let f = type_comparer(move |c, s, t, report_errors| {
            r.is_related_to_worker(c, s, t, report_errors)
        });
        self.worker_comparer.set(Some(f));
        f
    }
}

// The Go `Discriminator` implementations; the method bodies are the inherent methods in relater_1.rs / checker_15.rs.

impl Discriminator for TypeDiscriminator<'_> {
    fn len(&mut self, c: &mut Checker) -> usize {
        TypeDiscriminator::len(self, c)
    }
    fn name(&mut self, c: &mut Checker, index: usize) -> &'static str {
        TypeDiscriminator::name(self, c, index)
    }
    fn matches(&mut self, c: &mut Checker, index: usize, t: P<Type>) -> bool {
        TypeDiscriminator::matches(self, c, index, t)
    }
}

impl Discriminator for ObjectLiteralDiscriminator {
    fn len(&mut self, c: &mut Checker) -> usize {
        ObjectLiteralDiscriminator::len(self, c)
    }
    fn name(&mut self, c: &mut Checker, index: usize) -> &'static str {
        ObjectLiteralDiscriminator::name(self, c, index)
    }
    fn matches(&mut self, c: &mut Checker, index: usize, t: P<Type>) -> bool {
        ObjectLiteralDiscriminator::matches(self, c, index, t)
    }
}
