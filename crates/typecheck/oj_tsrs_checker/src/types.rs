use std::fmt;
use std::hash::Hash;
use std::num::NonZeroU32;

use crate::direct_bitflags as bitflags;
use tsrs_core::sharedgraph::{OvCell, OvExact, OvOptionThinSliceCell, RawWord};
use tsrs_core::{OptionThinSliceCell, TextPos, ThinSliceCell};

use crate::*;

#[derive(Default)]
pub struct CountCell(Cell<u32>);

impl CountCell {
    #[inline]
    pub(crate) fn get(&self) -> usize {
        self.0.get() as usize
    }

    #[inline]
    pub(crate) fn set(&self, value: usize) {
        self.0.set(u32::try_from(value).expect("count exceeds u32 storage"));
    }
}

pub struct OptionalCountCell(OvExact<Option<NonZeroU32>>);

impl Default for OptionalCountCell {
    fn default() -> Self {
        Self(OvExact::new(None))
    }
}

impl OptionalCountCell {
    #[inline]
    pub(crate) fn get(&self) -> Option<usize> {
        self.0.get().map(|value| value.get() as usize - 1)
    }

    #[inline]
    pub(crate) fn set(&self, value: Option<usize>) {
        let value = value.map(|value| {
            let value = u32::try_from(value).expect("count exceeds u32 storage");
            NonZeroU32::new(value.checked_add(1).expect("count exceeds optional u32 storage"))
                .unwrap()
        });
        self.0.set(value);
    }
}

// GoMap: a Go `map[K]V` field (a nil-able reference to a shared hash table), pointer-sized.

pub struct GoMap<K: 'static, V: 'static>(OvExact<Option<P<RefCell<FxHashMap<K, V>>>>>);

impl<K: 'static, V: 'static> Default for GoMap<K, V> {
    #[inline]
    fn default() -> Self {
        GoMap(OvExact::new(None))
    }
}

impl<K: Eq + Hash + Clone + 'static, V: Clone + 'static> GoMap<K, V> {
    /// Go `m = make(map[K]V)`.
    #[inline]
    pub(crate) fn make(&self) {
        // The table lives where the map field does: a field in an emit scratch region (a node builder request's
        // links, notes/mem-emit-regions.md) must not leave a table outside it that refers into it.
        let scratch = tsrs_core::arena::scratch_contains(std::ptr::from_ref::<Self>(self) as usize);
        self.0.set(Some(P::new_in(scratch, RefCell::new(FxHashMap::default()))));
    }
    /// Shared graph: the table, copied into this checker's arena first if it is frozen.
    #[inline]
    fn for_write(&self) -> Option<P<RefCell<FxHashMap<K, V>>>> {
        let m = self.0.get()?;
        if !tsrs_core::sharedgraph::frozen(m.get()) {
            return Some(m);
        }
        let copy = P::new(RefCell::new(tsrs_core::sharedgraph::with_ref(&m, Clone::clone)));
        self.0.set(Some(copy));
        Some(copy)
    }
    #[inline]
    pub(crate) fn is_nil(&self) -> bool {
        self.0.get().is_none()
    }
    /// Go `v, ok := m[k]` (reading a nil map is allowed).
    #[inline]
    pub(crate) fn get<Q: ?Sized + Hash + Eq>(&self, key: &Q) -> Option<V>
    where
        K: std::borrow::Borrow<Q>,
    {
        let m = self.0.get()?;
        tsrs_core::sharedgraph::with_ref(&m, |m| m.get(key).cloned())
    }

    /// Go `m[k] = v`. Creates the map if it is nil (Go would panic; faithful code always `make`s first).
    #[inline]
    pub(crate) fn set(&self, key: K, value: V) {
        let m = if let Some(m) = self.for_write() {
            m
        } else {
            self.make();
            self.0.get().unwrap()
        };
        m.borrow_mut().insert(key, value);
    }

    #[inline]
    pub(crate) fn set_ref(&self, m: Option<P<RefCell<FxHashMap<K, V>>>>) {
        self.0.set(m);
    }
    /// Go `a.m = someFreshlyBuiltMap`.
    #[inline]
    pub(crate) fn assign(&self, m: FxHashMap<K, V>) {
        let scratch = tsrs_core::arena::scratch_contains(std::ptr::from_ref::<Self>(self) as usize);
        self.0.set(Some(P::new_in(scratch, RefCell::new(m))));
    }
}

// ParseFlags

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct ParseFlags: u32 {
        const None = 0;
        const Yield = 1 << 0;
        const Await = 1 << 1;
        const Type = 1 << 2;
        const IgnoreMissingOpenBrace = 1 << 4;
        const JSDoc = 1 << 5;
    }
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum SignatureKind {
    #[default]
    Call,
    Construct,
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum IndexKind {
    #[default]
    String,
    Number,
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum MemberOverrideStatus {
    #[default]
    None,
    NeedsOverride,
    HasInvalidOverride,
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct ContextFlags: u32 {
        const None = 0;
        const Signature = 1 << 0; // Obtaining contextual signature
        const NoConstraints = 1 << 1; // Don't obtain type variable constraints
        const IgnoreNodeInferences = 1 << 2; // Ignore inference to current node and parent nodes out to the containing call for, for example, completions
        const SkipBindingPatterns = 1 << 3; // Ignore contextual types applied by binding patterns
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct TypeFormatFlags: u32 {
        const None = 0;
        const NoTruncation = 1 << 0;
        const WriteArrayAsGenericType = 1 << 1;
        const GenerateNamesForShadowedTypeParams = 1 << 2;
        const UseStructuralFallback = 1 << 3;
        const WriteTypeArgumentsOfSignature = 1 << 5;
        const UseFullyQualifiedType = 1 << 6;
        const SuppressAnyReturnType = 1 << 8;
        const MultilineObjectLiterals = 1 << 10;
        const WriteClassExpressionAsTypeLiteral = 1 << 11;
        const UseTypeOfFunction = 1 << 12;
        const OmitParameterModifiers = 1 << 13;
        const UseAliasDefinedOutsideCurrentScope = 1 << 14;
        const UseSingleQuotesForStringLiteralType = 1 << 28;
        const NoTypeReduction = 1 << 29;
        const UseInstantiationExpressions = 1 << 30;
        const OmitThisParameter = 1 << 25;
        const WriteCallStyleSignature = 1 << 27;
        const AllowUniqueESSymbolType = 1 << 20;
        const AddUndefined = 1 << 17;
        const WriteArrowStyleSignature = 1 << 18;
        const InArrayType = 1 << 19;
        const InElementType = 1 << 21;
        const InFirstTypeArgument = 1 << 22;
        const InTypeAlias = 1 << 23;

        const NodeBuilderFlagsMask = Self::NoTruncation.bits() | Self::WriteArrayAsGenericType.bits() | Self::GenerateNamesForShadowedTypeParams.bits() | Self::UseStructuralFallback.bits() | Self::WriteTypeArgumentsOfSignature.bits() |
            Self::UseFullyQualifiedType.bits() | Self::SuppressAnyReturnType.bits() | Self::MultilineObjectLiterals.bits() | Self::WriteClassExpressionAsTypeLiteral.bits() |
            Self::UseTypeOfFunction.bits() | Self::OmitParameterModifiers.bits() | Self::UseAliasDefinedOutsideCurrentScope.bits() | Self::AllowUniqueESSymbolType.bits() | Self::InTypeAlias.bits() |
            Self::UseInstantiationExpressions.bits() | Self::UseSingleQuotesForStringLiteralType.bits() | Self::NoTypeReduction.bits() | Self::OmitThisParameter.bits();
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct SymbolFormatFlags: u32 {
        const None = 0;
        const WriteTypeParametersOrArguments = 1 << 0;
        const UseOnlyExternalAliasing = 1 << 1;
        const AllowAnyNodeKind = 1 << 2;
        const UseAliasDefinedOutsideCurrentScope = 1 << 3;
        const WriteComputedProps = 1 << 4;
        const DoNotIncludeSymbolChain = 1 << 5;
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct ExternalEmitHelpers: u32 {
        const None = 0;
        const Rest = 1 << 0; // __rest (used by ESNext object rest transformation)
        const Decorate = 1 << 1; // __decorate (used by TypeScript decorators transformation)
        const Metadata = 1 << 2; // __metadata (used by TypeScript decorators transformation)
        const Param = 1 << 3; // __param (used by TypeScript decorators transformation)
        const Awaiter = 1 << 4; // __awaiter (used by ES2017 async functions transformation)
        const Await = 1 << 5; // __await (used by ES2017 async generator transformation)
        const AsyncGenerator = 1 << 6; // __asyncGenerator (used by ES2017 async generator transformation)
        const AsyncDelegator = 1 << 7; // __asyncDelegator (used by ES2017 async generator yield* transformation)
        const AsyncValues = 1 << 8; // __asyncValues (used by ES2017 for..await..of transformation)
        const ExportStar = 1 << 9; // __exportStar (used by CommonJS/AMD/UMD module transformation)
        const ImportStar = 1 << 10; // __importStar (used by CommonJS/AMD/UMD module transformation)
        const ImportDefault = 1 << 11; // __importDefault (used by CommonJS/AMD/UMD module transformation)
        const MakeTemplateObject = 1 << 12; // __makeTemplateObject (used for constructing template string array objects)
        const ClassPrivateFieldGet = 1 << 13; // __classPrivateFieldGet (used by the class private field transformation)
        const ClassPrivateFieldSet = 1 << 14; // __classPrivateFieldSet (used by the class private field transformation)
        const ClassPrivateFieldIn = 1 << 15; // __classPrivateFieldIn (used by the class private field transformation)
        const SetFunctionName = 1 << 16; // __setFunctionName (used by class fields and ECMAScript decorators)
        const PropKey = 1 << 17; // __propKey (used by class fields and ECMAScript decorators)
        const AddDisposableResourceAndDisposeResources = 1 << 18; // __addDisposableResource and __disposeResources (used by ESNext transformations)
        const RewriteRelativeImportExtension = 1 << 19; // __rewriteRelativeImportExtension (used by --rewriteRelativeImportExtensions)
        const ESDecorateAndRunInitializers = Self::Decorate.bits(); // __esDecorate and __runInitializers (used by ECMAScript decorators transformation)

        const FirstEmitHelper = Self::Rest.bits();
        const LastEmitHelper = Self::RewriteRelativeImportExtension.bits();

        // Helpers included by ES2017 for..await..of
        const ForAwaitOfIncludes = Self::AsyncValues.bits();

        // Helpers included by ES2017 async generators
        const AsyncGeneratorIncludes = Self::Await.bits() | Self::AsyncGenerator.bits();

        // Helpers included by yield* in ES2017 async generators
        const AsyncDelegatorIncludes = Self::Await.bits() | Self::AsyncDelegator.bits() | Self::AsyncValues.bits();
    }
}

pub(crate) const EXTERNAL_HELPERS_MODULE_NAME_TEXT: &str = "tslib";

// Ids

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct TypeId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct SignatureId(pub u32);

// Links for referenced symbols: Go's `SymbolReferenceLinks` has one field, `referenceKinds` (the meanings the symbol
// was referenced with), which `SymbolReferenceLinkStore` (links.rs) keeps in its slots.

// Links for value symbols
//
// Go's `ValueSymbolLinks` holds four pointers inline (32 bytes on 64-bit): `resolvedType`, `target`, `mapper`,
// and lazy `valueSymbolExtra` for `writeType`, `nameType`, `containingType`. Rust stores (`resolved_type`, `first`,
// `second`) in 16 bytes with compressed pointers (two 4-byte handles and a usize), 24 with plain pointers on
// 64-bit targets, 12 on 32-bit. The mode is in low two bits of `second` (every stored pointer is 8-aligned):
//
// - plain (the common case: instantiated symbols and most others): `target`, `mapper`;
// - synthetic: `containing_type`, `name_type`, for records that set those but never `target` / `mapper` (union and
//   intersection properties and mapped type members);
// - tail: a pointer to `ValueSymbolLinksTail`, which holds all five other fields for the remaining records.
//
// A record starts plain and moves to synthetic or tail mode on the first write that its mode cannot hold; it never
// moves back. Every getter returns what was last set (the zero value if never set), whatever the mode.

#[derive(Default)]
pub struct ValueSymbolLinks {
    pub(crate) resolved_type: Cell<Option<P<Type>>>, // Type of value symbol
    first: Cell<Option<P<()>>>, // plain: target (P<Symbol>); synthetic: containing_type (P<Type>); tail: P<ValueSymbolLinksTail>
    second: Cell<usize>, // `P::to_bits` of: plain: mapper (P<TypeMapper>); synthetic: name_type (P<Type>) | SYNTHETIC; tail: TAIL
}

#[derive(Default)]
struct ValueSymbolLinksTail {
    target: Cell<Option<P<Symbol>>>,
    mapper: MapperCell,
    write_type: Cell<Option<P<Type>>>,
    name_type: Cell<Option<P<Type>>>,
    containing_type: Cell<Option<P<Type>>>, // Mapped type for mapped type property, containing union or intersection type for synthetic property
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    std::mem::size_of::<ValueSymbolLinks>() == if tsrs_core::COMPRESSED_PTRS { 16 } else { 24 }
);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    std::mem::size_of::<ValueSymbolLinksTail>() == if tsrs_core::COMPRESSED_PTRS { 20 } else { 40 }
);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<ValueSymbolLinks>() == 12);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<ValueSymbolLinksTail>() == 20);

const MODE_MASK: usize = 3;
const SYNTHETIC: usize = 1;
const TAIL: usize = 2;

#[derive(Clone, Copy, PartialEq, Eq)]
enum LinksMode {
    Plain,
    Synthetic,
    Tail,
}

/// A pointer stored in a link word whose type depends on the mode.
#[inline]
#[expect(
    clippy::single_option_map,
    reason = "this helper erases the pointee type while preserving absence"
)]
fn erase<T>(p: Option<P<T>>) -> Option<P<()>> {
    // SAFETY: a `()` is zero-sized; the word only keeps the object's position.
    p.map(|p| unsafe { p.cast::<()>() })
}

/// # Safety `w` was stored by `erase` from a `P<T>` (arena values are never freed or moved).
#[inline]
#[expect(
    clippy::single_option_map,
    reason = "this helper restores the pointee type while preserving absence"
)]
unsafe fn restore<T: 'static>(w: Option<P<()>>) -> Option<P<T>> {
    // SAFETY: this function's contract: `w` was a `P<T>`.
    w.map(|w| unsafe { w.cast::<T>() })
}

/// # Safety `bits` is 0 or `P::to_bits` of a `P<T>` (mode bits cleared).
#[inline]
unsafe fn restore_bits<T: 'static>(bits: usize) -> Option<P<T>> {
    // SAFETY: this function's contract: `bits` is 0 or the bits of a `P<T>`.
    unsafe { P::from_bits_opt(bits) }
}

impl ValueSymbolLinks {
    #[inline]
    fn mode(&self) -> LinksMode {
        match self.second.get() & MODE_MASK {
            0 => LinksMode::Plain,
            SYNTHETIC => LinksMode::Synthetic,
            _ => LinksMode::Tail,
        }
    }

    #[inline]
    fn tail(&self) -> P<ValueSymbolLinksTail> {
        debug_assert!(self.mode() == LinksMode::Tail);
        // SAFETY: in tail mode `first` is an erased `P<ValueSymbolLinksTail>`.
        unsafe { restore(self.first.get()) }.unwrap()
    }

    /// The tail, moving the record to tail mode first if needed.
    #[inline]
    fn tail_for_write(&self) -> P<ValueSymbolLinksTail> {
        if self.mode() == LinksMode::Tail {
            return self.tail();
        }
        let tail = P::new(ValueSymbolLinksTail::default());
        tail.target.set(self.target());
        tail.mapper.set(self.mapper());
        tail.containing_type.set(self.containing_type());
        tail.name_type.set(self.name_type());
        self.first.set(erase(Some(tail)));
        self.second.set(TAIL);
        tail
    }

    /// Moves a plain record without target and mapper to synthetic mode, if it is one.
    #[inline]
    fn enter_synthetic_mode(&self) -> bool {
        if self.mode() == LinksMode::Plain && self.first.get().is_none() && self.second.get() == 0 {
            self.second.set(SYNTHETIC);
            return true;
        }
        self.mode() == LinksMode::Synthetic
    }

    #[inline]
    pub fn target(&self) -> Option<P<Symbol>> {
        match self.mode() {
            // SAFETY: in plain mode `first` is an erased `P<Symbol>` or nil.
            LinksMode::Plain => unsafe { restore(self.first.get()) },
            LinksMode::Synthetic => None,
            LinksMode::Tail => self.tail().target.get(),
        }
    }
    #[inline]
    pub fn set_target(&self, target: Option<P<Symbol>>) {
        match self.mode() {
            LinksMode::Plain => self.first.set(erase(target)),
            LinksMode::Synthetic if target.is_none() => {}
            _ => self.tail_for_write().target.set(target),
        }
    }
    #[inline]
    pub fn mapper(&self) -> Option<P<TypeMapper>> {
        match self.mode() {
            // SAFETY: in plain mode `second` is an erased `P<TypeMapper>` or nil (no mode bits).
            LinksMode::Plain => unsafe { restore_bits(self.second.get()) },
            LinksMode::Synthetic => None,
            LinksMode::Tail => self.tail().mapper.get(),
        }
    }
    #[inline]
    pub fn set_mapper(&self, mapper: Option<P<TypeMapper>>) {
        if let Some(m) = mapper {
            escape_mapper(m); // a symbol link outlives the call that made the mapper
        }
        match self.mode() {
            LinksMode::Plain => self.second.set(P::to_bits_opt(mapper)),
            LinksMode::Synthetic if mapper.is_none() => {}
            _ => self.tail_for_write().mapper.set(mapper),
        }
    }
    #[inline]
    pub fn containing_type(&self) -> Option<P<Type>> {
        match self.mode() {
            LinksMode::Plain => None,
            // SAFETY: in synthetic mode `first` is an erased `P<Type>` or nil.
            LinksMode::Synthetic => unsafe { restore(self.first.get()) },
            LinksMode::Tail => self.tail().containing_type.get(),
        }
    }
    #[inline]
    pub fn set_containing_type(&self, t: Option<P<Type>>) {
        if t.is_none() && self.mode() == LinksMode::Plain {
            return;
        }
        if self.enter_synthetic_mode() {
            self.first.set(erase(t));
        } else {
            self.tail_for_write().containing_type.set(t);
        }
    }
    #[inline]
    pub fn name_type(&self) -> Option<P<Type>> {
        match self.mode() {
            LinksMode::Plain => None,
            // SAFETY: in synthetic mode `second` is an erased `P<Type>` or nil plus the mode bits.
            LinksMode::Synthetic => unsafe { restore_bits(self.second.get() & !MODE_MASK) },
            LinksMode::Tail => self.tail().name_type.get(),
        }
    }
    #[inline]
    pub fn set_name_type(&self, t: Option<P<Type>>) {
        if t.is_none() && self.mode() == LinksMode::Plain {
            return;
        }
        if self.enter_synthetic_mode() {
            self.second.set(P::to_bits_opt(t) | SYNTHETIC);
        } else {
            self.tail_for_write().name_type.set(t);
        }
    }
    #[inline]
    pub fn write_type(&self) -> Option<P<Type>> {
        match self.mode() {
            LinksMode::Tail => self.tail().write_type.get(),
            _ => None,
        }
    }
    #[inline]
    pub fn set_write_type(&self, t: Option<P<Type>>) {
        if t.is_some() || self.mode() == LinksMode::Tail {
            self.tail_for_write().write_type.set(t);
        }
    }
}

// Additional links for mapped symbols

#[derive(Default)]
pub struct MappedSymbolLinks {
    pub(crate) key_type: Cell<Option<P<Type>>>, // Key type for mapped type member
    pub(crate) synthetic_origin: Cell<Option<P<Symbol>>>, // For a property on a mapped or spread type, points back to the original property
}

// Additional links for deferred type symbols

#[derive(Default)]
pub struct DeferredSymbolLinks {
    pub(crate) parent: Cell<Option<P<Type>>>, // Source union/intersection of a deferred type
    pub(crate) constituents: Cell<&'static [P<Type>]>, // Calculated list of constituents for a deferred type
    pub(crate) write_constituents: Cell<&'static [P<Type>]>, // Constituents of a deferred `writeType`
}

// Links for alias symbols

#[derive(Default)]
pub struct AliasSymbolLinks {
    pub(crate) immediate_target: Cell<Option<P<Symbol>>>, // Immediate target of an alias. May be another alias. Do not access directly, use `checker.getImmediateAliasedSymbol` instead.
    pub(crate) alias_target: Cell<Option<P<Symbol>>>,     // Resolved (non-alias) target of an alias
    pub(crate) referenced: Cell<bool>, // True if alias symbol has been referenced as a value that can be emitted
    pub(crate) type_only_declaration: Cell<Option<P<Node>>>, // First resolved alias declaration that makes the symbol only usable in type constructs
}

// Links for module symbols

#[derive(Default)]
pub struct ModuleSymbolLinks {
    pub(crate) resolved_exports: Cell<Option<P<SymbolTable>>>, // Resolved exports of module or combined early- and late-bound static members of a class.
    pub(crate) type_only_export_star_map: GoMap<String, P<Node>>, // Set on a module symbol when some of its exports were resolved through a 'export type * from "mod"' declaration
    pub(crate) exports_checked: Cell<bool>,
}

#[derive(Default)]
pub struct ReverseMappedSymbolLinks {
    pub(crate) property_type: Cell<Option<P<Type>>>,
    pub(crate) mapped_type: Cell<Option<P<Type>>>, // References a mapped type
    pub(crate) constraint_type: Cell<Option<P<Type>>>, // References an index type
}

// Links for late-bound symbols

#[derive(Default)]
pub struct LateBoundLinks {
    pub(crate) late_symbol: Cell<Option<P<Symbol>>>,
}

// Links for export type symbols

#[derive(Default)]
pub struct ExportTypeLinks {
    pub(crate) target: Cell<Option<P<Symbol>>>, // Target symbol
    pub(crate) originating_import: Cell<Option<P<Node>>>, // Import declaration which produced the symbol, present if the symbol is marked as uncallable but had call signatures in `resolveESModuleSymbol`
}

// Links for type aliases

#[derive(Default)]
pub struct TypeAliasLinks {
    pub(crate) declared_type: Cell<Option<P<Type>>>,
    pub(crate) type_parameters: Cell<&'static [P<Type>]>, // Type parameters of type alias (undefined if non-generic)
    pub(crate) instantiations: GoPackedMap<CacheHashKey, P<Type>>, // Instantiations of generic type alias (undefined if non-generic)
}

// Links for declared types (type parameters, class types, interface types, enums)

#[derive(Default)]
pub struct DeclaredTypeLinks {
    pub(crate) declared_type: Cell<Option<P<Type>>>,
    pub(crate) interface_checked: Cell<bool>,
    pub(crate) index_signatures_checked: Cell<bool>,
    pub(crate) type_parameters_checked: Cell<bool>,
    pub(crate) enum_checked: Cell<bool>,
}

// Links for switch clauses

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum ExhaustiveState {
    #[default]
    Unknown, // Exhaustive state not computed
    Computing, // Exhaustive state computation in progress
    False,     // Switch statement is not exhaustive
    True,      // Switch statement is exhaustive
}

#[derive(Default)]
pub struct SwitchStatementLinks {
    pub(crate) exhaustive_state: Cell<ExhaustiveState>, // Switch statement exhaustiveness
    pub(crate) switch_types_computed: Cell<bool>,
    pub(crate) witnesses_computed: Cell<bool>,
    pub(crate) switch_types: Cell<&'static [P<Type>]>,
    pub(crate) witnesses: Cell<Option<&'static [&'static str]>>, // Go nil (non-literal case) vs empty
}

#[derive(Default)]
pub struct ArrayLiteralLinks {
    pub(crate) indices_computed: Cell<bool>,
    pub(crate) first_spread_index: OptionalCountCell,
    pub(crate) last_spread_index: OptionalCountCell,
}

// Links for late-binding containers

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum MembersOrExportsResolutionKind {
    #[default]
    ResolvedExports = 0,
    ResolvedMembers = 1,
}

/// Go `[2]ast.SymbolTable`, indexed by `MembersOrExportsResolutionKind as usize`.
#[derive(Default)]
pub struct MembersAndExportsLinks([Cell<Option<P<SymbolTable>>>; 2]);

impl std::ops::Deref for MembersAndExportsLinks {
    type Target = [Cell<Option<P<SymbolTable>>>; 2];
    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

// Links for synthetic spread properties

#[derive(Default)]
pub struct SpreadLinks {
    pub(crate) left_spread: Cell<Option<P<Symbol>>>, // Left source for synthetic spread property
    pub(crate) right_spread: Cell<Option<P<Symbol>>>, // Right source for synthetic spread property
}

// Links for variances of type aliases and interface types

#[derive(Default)]
pub struct VarianceLinks {
    pub(crate) variances: Cell<Option<&'static [VarianceFlags]>>, // nil = not computed (Go distinguishes nil from empty)
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct VarianceFlags: u32 {
        const Invariant = 0; // Neither covariant nor contravariant
        const Covariant = 1 << 0; // Covariant
        const Contravariant = 1 << 1; // Contravariant
        const Bivariant = Self::Covariant.bits() | Self::Contravariant.bits(); // Both covariant and contravariant
        const Independent = 1 << 2; // Unwitnessed type parameter
        const VarianceMask = Self::Invariant.bits() | Self::Covariant.bits() | Self::Contravariant.bits() | Self::Independent.bits(); // Mask containing all measured variances without the unmeasurable flag
        const Unmeasurable = 1 << 3; // Variance result is unusable - relationship relies on structural comparisons which are not reflected in generic relationships
        const Unreliable = 1 << 4; // Variance result is unreliable - checking may produce false negatives, but not false positives
        const AllowsStructuralFallback = Self::Unmeasurable.bits() | Self::Unreliable.bits();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum LastAssignmentPosition {
    #[default]
    None,
    Position(TextPos),
    BeyondAll,
}

#[derive(Default)]
pub struct MarkedAssignmentSymbolLinks {
    pub(crate) last_assignment_pos: Cell<LastAssignmentPosition>,
    pub(crate) has_definite_assignment: Cell<bool>, // Symbol is definitely assigned somewhere
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct AccessibleChainCacheKey {
    pub(crate) use_only_external_aliasing: bool,
    pub(crate) location: Option<P<Node>>,
    pub(crate) meaning: SymbolFlags,
}

#[derive(Default)]
pub struct ContainingSymbolLinks {
    pub(crate) extended_containers_by_file: RefCell<FxHashMap<NodeId, &'static [P<Symbol>]>>, // Symbols of nodes which which logically contain this one, cached by file the request is made within
    pub(crate) extended_containers: Cell<Option<&'static [P<Symbol>]>>, // Containers (other than the parent) which this symbol is aliased in
    pub(crate) accessible_chain_cache:
        RefCell<FxHashMap<AccessibleChainCacheKey, &'static [P<Symbol>]>>,
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct AccessFlags: u32 {
        const None = 0;
        const IncludeUndefined = 1 << 0;
        const NoIndexSignatures = 1 << 1;
        const Writing = 1 << 2;
        const CacheSymbol = 1 << 3;
        const AllowMissing = 1 << 4;
        const ExpressionPosition = 1 << 5;
        const ReportDeprecated = 1 << 6;
        const SuppressNoImplicitAnyError = 1 << 7;
        const Contextual = 1 << 8;
        const Persistent = Self::IncludeUndefined.bits();
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct NodeCheckFlags: u32 {
        const None = 0;
        const TypeChecked = 1 << 0; // Node has been type checked
        const ContextChecked = 1 << 6; // Contextual types have been assigned
        const EnumValuesComputed = 1 << 10; // Values for enum members have been computed, and any errors have been reported for them.
        const AssignmentsMarked = 1 << 17; // Parameter assignments have been marked
        const ContainsClassWithPrivateIdentifiers = 1 << 20; // Marked on all block-scoped containers containing a class with private identifiers.
        const ContainsSuperPropertyInStaticInitializer = 1 << 21; // Marked on all block-scoped containers containing a static initializer with 'super.x' or 'super[x]'.
        const InCheckIdentifier = 1 << 22;
        const InitializerIsUndefined = 1 << 24;
        const InitializerIsUndefinedComputed = 1 << 25;
    }
}

// Common links

#[derive(Default)]
pub struct NodeLinks {
    pub(crate) flags: Cell<NodeCheckFlags>, // Set of flags specific to Node
    pub(crate) declaration_requires_scope_change: Cell<Tristate>, // Set by `useOuterVariableScopeInParameter` in checker when downlevel emit would change the name resolution scope inside of a parameter.
    pub(crate) has_reported_statement_in_ambient_context: Cell<bool>, // Cache boolean if we report statements in ambient context
}

#[derive(Default)]
pub struct SymbolNodeLinks {
    pub(crate) resolved_symbol: Cell<Option<P<Symbol>>>, // Resolved symbol associated with node
}

#[derive(Default)]
pub struct TypeNodeLinks {
    pub resolved_type: Cell<Option<P<Type>>>, // Resolved type associated with node
    pub outer_type_parameters: Cell<Option<P<ThinSliceCell<P<Type>>>>>, // Outer type parameters of anonymous object type; nil means not computed
}

impl TypeNodeLinks {
    #[inline]
    pub fn outer_type_parameters(&self) -> Option<&'static [P<Type>]> {
        self.outer_type_parameters.get().map(|types| types.get().get())
    }

    #[inline]
    pub fn set_outer_type_parameters(&self, types: Option<&'static [P<Type>]>) {
        match types {
            Some(types) => {
                if let Some(stored) = self.outer_type_parameters.get() {
                    stored.set(types);
                } else {
                    self.outer_type_parameters.set(Some(P::new(ThinSliceCell::new(types))));
                }
            }
            None => self.outer_type_parameters.set(None),
        }
    }
}

// The optional outer list stays in one handle, alongside the resolved type.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    std::mem::size_of::<TypeNodeLinks>() == if tsrs_core::COMPRESSED_PTRS { 8 } else { 16 }
);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<TypeNodeLinks>() == 8);

#[derive(Default)]
pub struct ComputedNameNodeLinks {
    pub(crate) has_name: Cell<Option<bool>>, // If the node has a computable name (Go *bool: nil = not computed)
    pub(crate) name: Cell<&'static str>,     // Resolved name associated with the type of the node
}

// Links for enum members

#[derive(Default)]
pub struct EnumMemberLinks {
    pub(crate) value: Cell<evaluator::Result>, // Constant value of enum member
}

// Links for assertion expressions

#[derive(Default)]
pub struct AssertionLinks {
    pub(crate) expr_type: Cell<Option<P<Type>>>, // Assertion expression type
}

// SourceFile links

#[derive(Default)]
pub struct SourceFileLinks {
    pub(crate) type_checked: Cell<bool>,
    pub(crate) unused_checked: Cell<bool>,
    pub(crate) external_helpers_module: Cell<Option<P<Symbol>>>,
    pub(crate) requested_external_emit_helpers: Cell<ExternalEmitHelpers>,
    pub(crate) deferred_nodes: RefCell<OrderedSet<P<Node>>>,
    pub(crate) identifier_check_nodes: RefCell<Vec<P<Node>>>,
    pub(crate) local_jsx_namespace: Cell<&'static str>,
    pub(crate) local_jsx_fragment_namespace: Cell<&'static str>,
    pub(crate) local_jsx_factory: Cell<Option<P<Node>>>,
    pub(crate) local_jsx_fragment_factory: Cell<Option<P<Node>>>,
    pub(crate) jsx_fragment_type: Cell<Option<P<Type>>>,
}

// Signature specific links

#[derive(Default)]
pub struct SignatureLinks {
    pub resolved_signature: Cell<Option<P<Signature>>>, // Cached signature of signature node or call expression
    // Go keeps these in separate fields: effects belong to call/binary expressions, while decorator signatures
    // belong to the decorated declarations. Their node kinds never overlap, so they can share one handle.
    effects_or_decorator_signature: Cell<Option<P<Signature>>>,
}

impl SignatureLinks {
    #[inline]
    pub(crate) fn effects_signature(&self, node: P<Node>) -> &Cell<Option<P<Signature>>> {
        debug_assert!(ast::is_call_expression(node) || ast::is_binary_expression(node));
        &self.effects_or_decorator_signature
    }

    #[inline]
    pub(crate) fn decorator_signature(&self, node: P<Node>) -> &Cell<Option<P<Signature>>> {
        // Invalid decorator parents still cache the `any_signature` sentinel before returning no signature.
        debug_assert!(!ast::is_call_expression(node) && !ast::is_binary_expression(node));
        &self.effects_or_decorator_signature
    }
}

// Two optional handles fit in one compressed arena slot, rather than rounding three handles up to 16 bytes.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    std::mem::size_of::<SignatureLinks>() == if tsrs_core::COMPRESSED_PTRS { 8 } else { 16 }
);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<SignatureLinks>() == 8);

// Note that for types of different kinds, the numeric values of TypeFlags determine the order
// computed by the CompareTypes function and therefore the order of constituent types in union types.
// Since union type processing often bails out early when a result is known, it is important to order
// TypeFlags in increasing order of potential type complexity. In particular, indexed access and
// conditional types should sort last as those types are potentially recursive and possibly infinite.

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct TypeFlags: u32 {
        const None = 0;
        const Any = 1 << 0;
        const Unknown = 1 << 1;
        const Undefined = 1 << 2;
        const Null = 1 << 3;
        const Void = 1 << 4;
        const String = 1 << 5;
        const Number = 1 << 6;
        const BigInt = 1 << 7;
        const Boolean = 1 << 8;
        const ESSymbol = 1 << 9; // Type of symbol primitive introduced in ES6
        const StringLiteral = 1 << 10;
        const NumberLiteral = 1 << 11;
        const BigIntLiteral = 1 << 12;
        const BooleanLiteral = 1 << 13;
        const UniqueESSymbol = 1 << 14; // unique symbol
        const EnumLiteral = 1 << 15; // Always combined with StringLiteral, NumberLiteral, or Union
        const Enum = 1 << 16; // Numeric computed enum member value (must be right after EnumLiteral, see getSortOrderFlags)
        const NonPrimitive = 1 << 17; // intrinsic object type
        const Never = 1 << 18; // Never type
        const TypeParameter = 1 << 19; // Type parameter
        const Object = 1 << 20; // Object type
        const Index = 1 << 21; // keyof T
        const TemplateLiteral = 1 << 22; // Template literal type
        const StringMapping = 1 << 23; // Uppercase/Lowercase type
        const Substitution = 1 << 24; // Type parameter substitution
        const IndexedAccess = 1 << 25; // T[K]
        const Conditional = 1 << 26; // T extends U ? X : Y
        const Union = 1 << 27; // Union (T | U)
        const Intersection = 1 << 28; // Intersection (T & U)
        const Reserved1 = 1 << 29; // Used by union/intersection type construction
        const Reserved2 = 1 << 30; // Used by union/intersection type construction
        const Reserved3 = 1 << 31;

        const AnyOrUnknown = Self::Any.bits() | Self::Unknown.bits();
        const Nullable = Self::Undefined.bits() | Self::Null.bits();
        const Literal = Self::StringLiteral.bits() | Self::NumberLiteral.bits() | Self::BigIntLiteral.bits() | Self::BooleanLiteral.bits();
        const Unit = Self::Enum.bits() | Self::Literal.bits() | Self::UniqueESSymbol.bits() | Self::Nullable.bits();
        const Freshable = Self::Enum.bits() | Self::Literal.bits();
        const StringOrNumberLiteral = Self::StringLiteral.bits() | Self::NumberLiteral.bits();
        const StringOrNumberLiteralOrUnique = Self::StringLiteral.bits() | Self::NumberLiteral.bits() | Self::UniqueESSymbol.bits();
        const DefinitelyFalsy = Self::StringLiteral.bits() | Self::NumberLiteral.bits() | Self::BigIntLiteral.bits() | Self::BooleanLiteral.bits() | Self::Void.bits() | Self::Undefined.bits() | Self::Null.bits();
        const PossiblyFalsy = Self::DefinitelyFalsy.bits() | Self::String.bits() | Self::Number.bits() | Self::BigInt.bits() | Self::Boolean.bits();
        const Intrinsic = Self::Any.bits() | Self::Unknown.bits() | Self::String.bits() | Self::Number.bits() | Self::BigInt.bits() | Self::ESSymbol.bits() | Self::Void.bits() | Self::Undefined.bits() | Self::Null.bits() | Self::Never.bits() | Self::NonPrimitive.bits();
        const StringLike = Self::String.bits() | Self::StringLiteral.bits() | Self::TemplateLiteral.bits() | Self::StringMapping.bits();
        const NumberLike = Self::Number.bits() | Self::NumberLiteral.bits() | Self::Enum.bits();
        const BigIntLike = Self::BigInt.bits() | Self::BigIntLiteral.bits();
        const BooleanLike = Self::Boolean.bits() | Self::BooleanLiteral.bits();
        const EnumLike = Self::Enum.bits() | Self::EnumLiteral.bits();
        const ESSymbolLike = Self::ESSymbol.bits() | Self::UniqueESSymbol.bits();
        const VoidLike = Self::Void.bits() | Self::Undefined.bits();
        const Primitive = Self::StringLike.bits() | Self::NumberLike.bits() | Self::BigIntLike.bits() | Self::BooleanLike.bits() | Self::EnumLike.bits() | Self::ESSymbolLike.bits() | Self::VoidLike.bits() | Self::Null.bits();
        const DefinitelyNonNullable = Self::StringLike.bits() | Self::NumberLike.bits() | Self::BigIntLike.bits() | Self::BooleanLike.bits() | Self::EnumLike.bits() | Self::ESSymbolLike.bits() | Self::Object.bits() | Self::NonPrimitive.bits();
        const DisjointDomains = Self::NonPrimitive.bits() | Self::StringLike.bits() | Self::NumberLike.bits() | Self::BigIntLike.bits() | Self::BooleanLike.bits() | Self::ESSymbolLike.bits() | Self::VoidLike.bits() | Self::Null.bits();
        const UnionOrIntersection = Self::Union.bits() | Self::Intersection.bits();
        const StructuredType = Self::Object.bits() | Self::Union.bits() | Self::Intersection.bits();
        const TypeVariable = Self::TypeParameter.bits() | Self::IndexedAccess.bits();
        const InstantiableNonPrimitive = Self::TypeVariable.bits() | Self::Conditional.bits() | Self::Substitution.bits();
        const InstantiablePrimitive = Self::Index.bits() | Self::TemplateLiteral.bits() | Self::StringMapping.bits();
        const Instantiable = Self::InstantiableNonPrimitive.bits() | Self::InstantiablePrimitive.bits();
        const StructuredOrInstantiable = Self::StructuredType.bits() | Self::Instantiable.bits();
        const ObjectFlagsType = Self::Any.bits() | Self::Nullable.bits() | Self::Never.bits() | Self::Object.bits() | Self::Union.bits() | Self::Intersection.bits();
        const Simplifiable = Self::IndexedAccess.bits() | Self::Conditional.bits() | Self::Index.bits();
        const Singleton = Self::Any.bits() | Self::Unknown.bits() | Self::String.bits() | Self::Number.bits() | Self::Boolean.bits() | Self::BigInt.bits() | Self::ESSymbol.bits() | Self::Void.bits() | Self::Undefined.bits() | Self::Null.bits() | Self::Never.bits() | Self::NonPrimitive.bits();
        // 'TypeFlagsNarrowable' types are types where narrowing actually narrows.
        // This *should* be every type other than null, undefined, void, and never
        const Narrowable = Self::Any.bits() | Self::Unknown.bits() | Self::StructuredOrInstantiable.bits() | Self::StringLike.bits() | Self::NumberLike.bits() | Self::BigIntLike.bits() | Self::BooleanLike.bits() | Self::ESSymbol.bits() | Self::UniqueESSymbol.bits() | Self::NonPrimitive.bits();
        // The following flags are aggregated during union and intersection type construction
        const IncludesMask = Self::Any.bits() | Self::Unknown.bits() | Self::Primitive.bits() | Self::Never.bits() | Self::Object.bits() | Self::Union.bits() | Self::Intersection.bits() | Self::NonPrimitive.bits() | Self::TemplateLiteral.bits() | Self::StringMapping.bits();
        // The following flags are used for different purposes during union and intersection type construction
        const IncludesMissingType = Self::TypeParameter.bits();
        const IncludesNonWideningType = Self::Index.bits();
        const IncludesWildcard = Self::IndexedAccess.bits();
        const IncludesEmptyObject = Self::Conditional.bits();
        const IncludesInstantiable = Self::Substitution.bits();
        const IncludesConstrainedTypeVariable = Self::Reserved1.bits();
        const IncludesError = Self::Reserved2.bits();
        const NotPrimitiveUnion = Self::Any.bits() | Self::Unknown.bits() | Self::Void.bits() | Self::Never.bits() | Self::Object.bits() | Self::Intersection.bits() | Self::IncludesInstantiable.bits();
    }
}

static TYPE_FLAG_NAMES: [(TypeFlags, &str); 29] = [
    (TypeFlags::Any, "Any"),
    (TypeFlags::Unknown, "Unknown"),
    (TypeFlags::Undefined, "Undefined"),
    (TypeFlags::Null, "Null"),
    (TypeFlags::Void, "Void"),
    (TypeFlags::String, "String"),
    (TypeFlags::Number, "Number"),
    (TypeFlags::BigInt, "BigInt"),
    (TypeFlags::Boolean, "Boolean"),
    (TypeFlags::ESSymbol, "ESSymbol"),
    (TypeFlags::StringLiteral, "StringLiteral"),
    (TypeFlags::NumberLiteral, "NumberLiteral"),
    (TypeFlags::BigIntLiteral, "BigIntLiteral"),
    (TypeFlags::BooleanLiteral, "BooleanLiteral"),
    (TypeFlags::UniqueESSymbol, "UniqueESSymbol"),
    (TypeFlags::EnumLiteral, "EnumLiteral"),
    (TypeFlags::Enum, "Enum"),
    (TypeFlags::NonPrimitive, "NonPrimitive"),
    (TypeFlags::Never, "Never"),
    (TypeFlags::TypeParameter, "TypeParameter"),
    (TypeFlags::Object, "Object"),
    (TypeFlags::Index, "Index"),
    (TypeFlags::TemplateLiteral, "TemplateLiteral"),
    (TypeFlags::StringMapping, "StringMapping"),
    (TypeFlags::Substitution, "Substitution"),
    (TypeFlags::IndexedAccess, "IndexedAccess"),
    (TypeFlags::Conditional, "Conditional"),
    (TypeFlags::Union, "Union"),
    (TypeFlags::Intersection, "Intersection"),
];

/// `FormatTypeFlags` returns the individual flag names as a slice of strings.
fn format_type_flags(flags: TypeFlags) -> Vec<&'static str> {
    let mut result = Vec::with_capacity(flags.bits().count_ones() as usize);
    result.extend(
        TYPE_FLAG_NAMES.iter().filter_map(
            |(flag, name)| {
                if flags.intersects(*flag) { Some(*name) } else { None }
            },
        ),
    );
    if result.is_empty() {
        result.push("None");
    }
    result
}

/// Go `TypeFlags.String()`: a pipe-separated string of flag names.
impl fmt::Display for TypeFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&format_type_flags(*self).join("|"))
    }
}

/// Go `VarianceFlags.String()`.
impl fmt::Display for VarianceFlags {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variance = *self & VarianceFlags::VarianceMask;
        let mut result = String::from(if variance == VarianceFlags::Invariant {
            "in out"
        } else if variance == VarianceFlags::Bivariant {
            "[bivariant]"
        } else if variance == VarianceFlags::Contravariant {
            "in"
        } else if variance == VarianceFlags::Covariant {
            "out"
        } else if variance == VarianceFlags::Independent {
            "[independent]"
        } else {
            ""
        });
        if self.intersects(VarianceFlags::Unmeasurable) {
            result.push_str(" (unmeasurable)");
        } else if self.intersects(VarianceFlags::Unreliable) {
            result.push_str(" (unreliable)");
        }
        f.write_str(&result)
    }
}

// Types included in TypeFlags.ObjectFlagsType have an objectFlags property. Some ObjectFlags
// are specific to certain types and reuse the same bit position. Those ObjectFlags require a check
// for a certain TypeFlags value to determine their meaning.
bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct ObjectFlags: u32 {
        const None = 0;
        const Class = 1 << 0; // Class
        const Interface = 1 << 1; // Interface
        const Reference = 1 << 2; // Generic type reference
        const Tuple = 1 << 3; // Synthesized generic tuple type
        const Anonymous = 1 << 4; // Anonymous
        const Mapped = 1 << 5; // Mapped
        const Instantiated = 1 << 6; // Instantiated anonymous or mapped type
        const ObjectLiteral = 1 << 7; // Originates in an object literal
        const EvolvingArray = 1 << 8; // Evolving array type
        const ObjectLiteralPatternWithComputedProperties = 1 << 9; // Object literal pattern with computed properties
        const ReverseMapped = 1 << 10; // Object contains a property from a reverse-mapped type
        const JsxAttributes = 1 << 11; // Jsx attributes type
        const JSLiteral = 1 << 12; // Object type declared in JS - disables errors on read/write of nonexisting members
        const FreshLiteral = 1 << 13; // Fresh object literal
        const ArrayLiteral = 1 << 14; // Originates in an array literal
        const PrimitiveUnion = 1 << 15; // Union of only primitive types
        const ContainsWideningType = 1 << 16; // Type is or contains undefined or null widening type
        const ContainsObjectOrArrayLiteral = 1 << 17; // Type is or contains object literal type
        const NonInferrableType = 1 << 18; // Type is or contains anyFunctionType or silentNeverType
        const CouldContainTypeVariablesComputed = 1 << 19; // CouldContainTypeVariables flag has been computed
        const CouldContainTypeVariables = 1 << 20; // Type could contain a type variable
        const MembersResolved = 1 << 21; // Members have been resolved

        const ClassOrInterface = Self::Class.bits() | Self::Interface.bits();
        const RequiresWidening = Self::ContainsWideningType.bits() | Self::ContainsObjectOrArrayLiteral.bits();
        const PropagatingFlags = Self::ContainsWideningType.bits() | Self::ContainsObjectOrArrayLiteral.bits() | Self::NonInferrableType.bits();
        const InstantiatedMapped = Self::Mapped.bits() | Self::Instantiated.bits();
        // Object flags that uniquely identify the kind of ObjectType
        const ObjectTypeKindMask = Self::ClassOrInterface.bits() | Self::Reference.bits() | Self::Tuple.bits() | Self::Anonymous.bits() | Self::Mapped.bits() | Self::ReverseMapped.bits() | Self::EvolvingArray.bits() | Self::InstantiationExpressionType.bits() | Self::SingleSignatureType.bits();
        // Flags that require TypeFlags.Object
        const ContainsSpread = 1 << 22; // Object literal contains spread operation
        const ObjectRestType = 1 << 23; // Originates in object rest declaration
        const InstantiationExpressionType = 1 << 24; // Originates in instantiation expression
        const SingleSignatureType = 1 << 25; // A single signature type extracted from a potentially broader type
        const IsClassInstanceClone = 1 << 26; // Type is a clone of a class instance type
        // Flags that require TypeFlags.Object and ObjectFlags.Reference
        const IdenticalBaseTypeCalculated = 1 << 27; // has had `getSingleBaseForNonAugmentingSubtype` invoked on it already
        const IdenticalBaseTypeExists = 1 << 28; // has a defined cachedEquivalentBaseType member (tsrs: the `EquivalentBaseType` entry in `cached_types`; read before the map is probed)
        const FromTypeNode = 1 << 29; // Originates in resolution of AST type node
        // Flags that require TypeFlags.UnionOrIntersection or TypeFlags.Substitution
        const IsGenericTypeComputed = 1 << 22; // IsGenericObjectType flag has been computed
        const IsGenericObjectType = 1 << 23; // Union or intersection contains generic object type
        const IsGenericIndexType = 1 << 24; // Union or intersection contains generic index type
        const IsGenericType = Self::IsGenericObjectType.bits() | Self::IsGenericIndexType.bits();
        // Flags that require TypeFlags.Union
        const ContainsIntersections = 1 << 25; // Union contains intersections
        const IsUnknownLikeUnionComputed = 1 << 26; // IsUnknownLikeUnion flag has been computed
        const IsUnknownLikeUnion = 1 << 27; // Union of null, undefined, and empty object type
        const IsUniformEnumComputed = 1 << 28; // IsUniformEnum flag has been computed
        const IsUniformEnum = 1 << 29; // Union contains uniform literal types
        // Flags that require TypeFlags.Intersection
        const IsNeverIntersectionComputed = 1 << 25; // IsNeverLike flag has been computed
        const IsNeverIntersection = 1 << 26; // Intersection reduces to never
        const IsConstrainedTypeVariable = 1 << 27; // T & C, where T's constraint and C are primitives, object, or {}
        // tsrs-only, intersections: shouldNormalizeIntersection memoized (its answer depends only on the constituents).
        const ShouldNormalizeComputed = 1 << 30;
        const ShouldNormalize = 1 << 31;
        // tsrs-only, unions: every constituent is a string, number, bigint, boolean, enum, symbol, object or
        // non-primitive kind (no nullable, void, intersection or instantiable constituent), so the union has no
        // nullability type fact and `get_type_facts_worker` answers those without the walk. Set by
        // `get_union_type_from_sorted_list` from the constituents (`new_type` drops the bit, see `WeakTypeMemo`).
        const NonNullableUnion = 1 << 30;
        // tsrs-only, object types: isWeakType memoized; cleared with MembersResolved (`WeakTypeMemo`).
        const IsWeakTypeComputed = 1 << 30;
        const IsWeakType = 1 << 31;
        const WeakTypeMemo = Self::IsWeakTypeComputed.bits() | Self::IsWeakType.bits(); // also the intersection memo and union summary bits: new_type drops them
    }
}

// TypeAlias

#[derive(Default)]
pub struct TypeAlias {
    pub(crate) symbol: Cell<Option<P<Symbol>>>,
    pub(crate) type_arguments: ThinSliceCell<P<Type>>,
}

impl TypeAlias {
    #[inline]
    pub fn symbol(&self) -> Option<P<Symbol>> {
        self.symbol.get()
    }
    #[inline]
    pub fn type_arguments(&self) -> &'static [P<Type>] {
        self.type_arguments.get()
    }
}

/// The alias argument of the type constructors that look up a cache before they create a type (union,
/// intersection, indexed access, object type instantiation; Go passes a `*TypeAlias`).
///
/// `Pending` is an instantiated alias (Go's `instantiateTypeAlias` result) that is allocated only when a type
/// is created with it: the cache key needs only its symbol and type arguments, and most calls return a cached
/// type (on the private monorepo 1.79M of the 2.34M instantiated aliases were dropped that way). A pending
/// alias is allocated at most once, so every type created with it shares one `TypeAlias`, as before.
#[derive(Clone, Copy, Default)]
pub enum AliasArg<'a> {
    #[default]
    None,
    Some(P<TypeAlias>),
    Pending(&'a PendingTypeAlias),
}

pub struct PendingTypeAlias {
    pub(crate) symbol: Option<P<Symbol>>,
    pub(crate) type_arguments: Vec<P<Type>>,
    alias: Cell<Option<P<TypeAlias>>>,
}

impl PendingTypeAlias {
    #[inline]
    pub(crate) fn new(symbol: Option<P<Symbol>>, type_arguments: Vec<P<Type>>) -> PendingTypeAlias {
        PendingTypeAlias { symbol, type_arguments, alias: Cell::new(None) }
    }
}

impl From<Option<P<TypeAlias>>> for AliasArg<'_> {
    #[inline]
    fn from(alias: Option<P<TypeAlias>>) -> Self {
        alias.map_or(AliasArg::None, AliasArg::Some)
    }
}

impl<'a> AliasArg<'a> {
    /// `alias` if given, else the pending one (Go: `if alias == nil { alias = c.instantiateTypeAlias(...) }`).
    #[inline]
    pub(crate) fn given_or_pending(
        alias: Option<P<TypeAlias>>,
        pending: Option<&'a PendingTypeAlias>,
    ) -> AliasArg<'a> {
        match (alias, pending) {
            (Some(alias), _) => AliasArg::Some(alias),
            (None, Some(pending)) => AliasArg::Pending(pending),
            (None, None) => AliasArg::None,
        }
    }

    #[inline]
    pub(crate) fn is_none(self) -> bool {
        matches!(self, AliasArg::None)
    }

    /// The alias to store in a created type: a pending alias is allocated on the first call.
    #[inline]
    pub(crate) fn alias(self) -> Option<P<TypeAlias>> {
        match self {
            AliasArg::None => None,
            AliasArg::Some(a) => Some(a),
            AliasArg::Pending(p) => Some(p.alias.get().unwrap_or_else(|| {
                let a = P::new(TypeAlias {
                    symbol: Cell::new(p.symbol),
                    type_arguments: ThinSliceCell::new(alloc_slice(&p.type_arguments)),
                });
                p.alias.set(Some(a));
                a
            })),
        }
    }
}

/// Go's nil-receiver `(*TypeAlias).Symbol()` / `TypeArguments()`: `t.alias().symbol()` works on `Option<P<TypeAlias>>`.
pub trait TypeAliasOptExt {
    fn symbol(self) -> Option<P<Symbol>>;
    fn type_arguments(self) -> &'static [P<Type>];
}

impl TypeAliasOptExt for Option<P<TypeAlias>> {
    #[inline]
    fn symbol(self) -> Option<P<Symbol>> {
        self?.symbol.get()
    }
    #[inline]
    fn type_arguments(self) -> &'static [P<Type>] {
        self.map_or(&[], |a| a.type_arguments.get())
    }
}

// Type
//
// Go's `Type` points to its type-specific data (`data TypeData`, an interface). Here the data struct is allocated
// together with the header, right after it (`TypeAlloc`), and `data_tag` says which struct it is: one allocation
// per type and a 24-byte header. `t.data()` returns the `TypeData` view; the `as_*` casts read the data in place.
// Go's `alias` is set on few types (on the private monorepo 0.6M of 9.9M), so it shares a word with `symbol`
// (`TypeSymbolWord`): the word holds the symbol until an alias is set, then a `TypeSymbolAlias` record holding both.

pub struct Type {
    pub flags: Cell<TypeFlags>,
    pub(crate) object_flags: ObjectFlagsCell,
    pub id: TypeId,
    data_tag: TypeDataTag,
    symbol_or_alias: Cell<TypeSymbolWord>,
}

/// `Type.object_flags`. Shared graph: a frozen type's flags written by a fork go to the fork's overlay,
/// kept by type id (`Overlay::id_word`); with nothing frozen this is a `Cell`.
#[repr(transparent)]
pub struct ObjectFlagsCell(Cell<ObjectFlags>);

impl ObjectFlagsCell {
    #[inline]
    const fn new(f: ObjectFlags) -> Self {
        ObjectFlagsCell(Cell::new(f))
    }
    #[inline]
    fn owner(&self) -> &Type {
        // SAFETY: an `ObjectFlagsCell` exists only as the `object_flags` field of a `Type`, which is an arena value.
        unsafe {
            tsrs_core::ptr::arena_neighbor(
                self,
                -(std::mem::offset_of!(Type, object_flags) as isize),
            )
        }
    }
    /// The flags as created plus what this checker computed lazily, except on a frozen type, where the lazily
    /// computed families (members resolved, could contain type variables, is generic, unknown-like, uniform enum,
    /// never intersection, constrained type variable, identical base type) read as the seed left them: their readers
    /// use `get_lazy`.
    #[inline]
    fn get(&self) -> ObjectFlags {
        self.0.get()
    }
    /// `get` that also sees this checker's lazily computed bits on a frozen type (shared graph).
    #[inline]
    pub(crate) fn get_lazy(&self) -> ObjectFlags {
        let v = self.0.get();
        if !tsrs_core::sharedgraph::dirty(std::ptr::from_ref(self).addr()) {
            return v;
        }
        self.get_frozen(v)
    }
    #[cold]
    #[inline(never)]
    fn get_frozen(&self, v: ObjectFlags) -> ObjectFlags {
        match tsrs_core::sharedgraph::try_current_overlay()
            .and_then(|o| o.id_word(self.owner().id.0))
        {
            Some(w) => ObjectFlags::from_bits_retain(w as u32),
            None => v,
        }
    }
    #[inline]
    pub(crate) fn set(&self, f: ObjectFlags) {
        if tsrs_core::sharedgraph::any_frozen() && tsrs_core::sharedgraph::frozen(self) {
            tsrs_core::sharedgraph::set_id_word_frozen(
                std::ptr::from_ref(self).addr(),
                self.owner().id.0,
                f.bits() as u64,
            );
            return;
        }
        self.0.set(f);
    }
}

const _: () = assert!(std::mem::size_of::<Type>() == 24);

#[derive(Default)]
struct TypeSymbolAlias {
    symbol: Cell<Option<P<Symbol>>>,
    alias: Cell<Option<P<TypeAlias>>>,
}

/// `Type.symbol`, or a `TypeSymbolAlias` record once the type has an alias: `P::pack` in the low 45 bits, bit 63 set
/// for the record. 0 = no symbol, no alias. The address stays a plain pointer to the start of its block.
#[derive(Clone, Copy, Default, PartialEq)]
struct TypeSymbolWord(u64);

impl TypeSymbolWord {
    const RECORD: u64 = 1 << 63;

    #[inline]
    fn symbol_word(symbol: Option<P<Symbol>>) -> TypeSymbolWord {
        TypeSymbolWord(P::pack_opt(symbol))
    }

    #[inline]
    fn record(self) -> Option<P<TypeSymbolAlias>> {
        // SAFETY: a tagged word was stored from a live `P<TypeSymbolAlias>` (arena objects are never moved or freed).
        (self.0 & Self::RECORD != 0).then(|| unsafe { P::unpack(self.0) })
    }

    #[inline]
    fn symbol(self) -> Option<P<Symbol>> {
        match self.record() {
            Some(r) => r.symbol.get(),
            // SAFETY: an untagged word is 0 or was stored from a live `P<Symbol>`.
            None => unsafe { P::unpack_opt(self.0) },
        }
    }
}

/// One arena allocation per type: the header, then the data struct (`repr(C)`: header at offset 0).
#[repr(C)]
pub struct TypeAlloc<T> {
    header: Type,
    pub data: T,
}

/// A type data struct, stored after the header of types tagged `TAG`.
pub trait TypePayload: Sized + 'static {
    const TAG: TypeDataTag;
}

/// Which data struct follows a type's header (one variant per `TypeData` variant).
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeDataTag {
    Intrinsic,
    Literal,
    UniqueESSymbol,
    Object,
    TypeReference,
    Interface,
    Tuple,
    InstantiationExpression,
    Mapped,
    ReverseMapped,
    EvolvingArray,
    Union,
    Intersection,
    TypeParameter,
    Index,
    IndexedAccess,
    TemplateLiteral,
    StringMapping,
    Substitution,
    Conditional,
}

impl TypePayload for IntrinsicType {
    const TAG: TypeDataTag = TypeDataTag::Intrinsic;
}
impl TypePayload for LiteralType {
    const TAG: TypeDataTag = TypeDataTag::Literal;
}
impl TypePayload for UniqueESSymbolType {
    const TAG: TypeDataTag = TypeDataTag::UniqueESSymbol;
}
impl TypePayload for ObjectType {
    const TAG: TypeDataTag = TypeDataTag::Object;
}
impl TypePayload for TypeReference {
    const TAG: TypeDataTag = TypeDataTag::TypeReference;
}
impl TypePayload for InterfaceType {
    const TAG: TypeDataTag = TypeDataTag::Interface;
}
impl TypePayload for TupleType {
    const TAG: TypeDataTag = TypeDataTag::Tuple;
}
impl TypePayload for InstantiationExpressionType {
    const TAG: TypeDataTag = TypeDataTag::InstantiationExpression;
}
impl TypePayload for MappedType {
    const TAG: TypeDataTag = TypeDataTag::Mapped;
}
impl TypePayload for ReverseMappedType {
    const TAG: TypeDataTag = TypeDataTag::ReverseMapped;
}
impl TypePayload for EvolvingArrayType {
    const TAG: TypeDataTag = TypeDataTag::EvolvingArray;
}
impl TypePayload for UnionType {
    const TAG: TypeDataTag = TypeDataTag::Union;
}
impl TypePayload for IntersectionType {
    const TAG: TypeDataTag = TypeDataTag::Intersection;
}
impl TypePayload for TypeParameter {
    const TAG: TypeDataTag = TypeDataTag::TypeParameter;
}
impl TypePayload for IndexType {
    const TAG: TypeDataTag = TypeDataTag::Index;
}
impl TypePayload for IndexedAccessType {
    const TAG: TypeDataTag = TypeDataTag::IndexedAccess;
}
impl TypePayload for TemplateLiteralType {
    const TAG: TypeDataTag = TypeDataTag::TemplateLiteral;
}
impl TypePayload for StringMappingType {
    const TAG: TypeDataTag = TypeDataTag::StringMapping;
}
impl TypePayload for SubstitutionType {
    const TAG: TypeDataTag = TypeDataTag::Substitution;
}
impl TypePayload for ConditionalType {
    const TAG: TypeDataTag = TypeDataTag::Conditional;
}

impl Type {
    /// Allocates a type whose data struct is `data` (only `Checker::new_type` and the checker's placeholder type).
    #[inline]
    pub fn alloc<T: TypePayload>(
        flags: TypeFlags,
        object_flags: ObjectFlags,
        id: TypeId,
        data: T,
    ) -> P<Type> {
        let header = Type {
            flags: Cell::new(flags),
            object_flags: ObjectFlagsCell::new(object_flags),
            id,
            data_tag: T::TAG,
            symbol_or_alias: Cell::new(TypeSymbolWord(0)),
        };
        // SAFETY: `TypeAlloc` is `repr(C)` with the header first; arena values are never moved or freed.
        unsafe { P::new(TypeAlloc { header, data }).cast::<Type>() }
    }

    /// The data struct after this type's header. Callers check `data_tag == T::TAG` first.
    #[inline]
    fn payload<T: TypePayload>(&self) -> &'static T {
        debug_assert_eq!(self.data_tag, T::TAG);
        // SAFETY: a type tagged `T::TAG` was allocated by `Type::alloc::<T>` as a `TypeAlloc<T>` whose header is
        // `self`, so its data struct lives at this offset from the header, for the rest of the process.
        unsafe {
            tsrs_core::ptr::arena_neighbor(self, std::mem::offset_of!(TypeAlloc<T>, data) as isize)
        }
    }

    /// The type-specific data (Go `t.data`).
    #[inline]
    pub fn data(&self) -> TypeData {
        match self.data_tag {
            TypeDataTag::Intrinsic => TypeData::Intrinsic(self.payload()),
            TypeDataTag::Literal => TypeData::Literal(self.payload()),
            TypeDataTag::UniqueESSymbol => TypeData::UniqueESSymbol(self.payload()),
            TypeDataTag::Object => TypeData::Object(self.payload()),
            TypeDataTag::TypeReference => TypeData::TypeReference(self.payload()),
            TypeDataTag::Interface => TypeData::Interface(self.payload()),
            TypeDataTag::Tuple => TypeData::Tuple(self.payload()),
            TypeDataTag::InstantiationExpression => {
                TypeData::InstantiationExpression(self.payload())
            }
            TypeDataTag::Mapped => TypeData::Mapped(self.payload()),
            TypeDataTag::ReverseMapped => TypeData::ReverseMapped(self.payload()),
            TypeDataTag::EvolvingArray => TypeData::EvolvingArray(self.payload()),
            TypeDataTag::Union => TypeData::Union(self.payload()),
            TypeDataTag::Intersection => TypeData::Intersection(self.payload()),
            TypeDataTag::TypeParameter => TypeData::TypeParameter(self.payload()),
            TypeDataTag::Index => TypeData::Index(self.payload()),
            TypeDataTag::IndexedAccess => TypeData::IndexedAccess(self.payload()),
            TypeDataTag::TemplateLiteral => TypeData::TemplateLiteral(self.payload()),
            TypeDataTag::StringMapping => TypeData::StringMapping(self.payload()),
            TypeDataTag::Substitution => TypeData::Substitution(self.payload()),
            TypeDataTag::Conditional => TypeData::Conditional(self.payload()),
        }
    }

    #[inline]
    pub fn data_tag(&self) -> TypeDataTag {
        self.data_tag
    }
}

impl Type {
    #[inline]
    pub fn id(&self) -> TypeId {
        self.id
    }

    #[inline]
    pub fn flags(&self) -> TypeFlags {
        self.flags.get()
    }

    #[inline]
    pub fn object_flags(&self) -> ObjectFlags {
        self.object_flags.get()
    }

    /// `object_flags` for the lazily computed bits (`ObjectFlagsCell::get_lazy`).
    #[inline]
    pub(crate) fn object_flags_lazy(&self) -> ObjectFlags {
        self.object_flags.get_lazy()
    }

    // Casts for concrete struct types

    #[inline]
    pub fn as_intrinsic_type(&self) -> &'static IntrinsicType {
        assert!(self.data_tag == TypeDataTag::Intrinsic, "as_intrinsic_type: wrong type data");
        self.payload()
    }
    #[inline]
    pub fn as_literal_type(&self) -> &'static LiteralType {
        assert!(self.data_tag == TypeDataTag::Literal, "as_literal_type: wrong type data");
        self.payload()
    }
    #[inline]
    pub fn as_unique_es_symbol_type(&self) -> &'static UniqueESSymbolType {
        assert!(
            self.data_tag == TypeDataTag::UniqueESSymbol,
            "as_unique_es_symbol_type: wrong type data"
        );
        self.payload()
    }
    #[inline]
    pub fn as_tuple_type(&self) -> &'static TupleType {
        assert!(self.data_tag == TypeDataTag::Tuple, "as_tuple_type: wrong type data");
        self.payload()
    }
    #[inline]
    pub fn as_instantiation_expression_type(&self) -> &'static InstantiationExpressionType {
        assert!(
            self.data_tag == TypeDataTag::InstantiationExpression,
            "as_instantiation_expression_type: wrong type data"
        );
        self.payload()
    }
    #[inline]
    pub fn as_mapped_type(&self) -> &'static MappedType {
        assert!(self.data_tag == TypeDataTag::Mapped, "as_mapped_type: wrong type data");
        self.payload()
    }
    #[inline]
    pub fn as_reverse_mapped_type(&self) -> &'static ReverseMappedType {
        assert!(
            self.data_tag == TypeDataTag::ReverseMapped,
            "as_reverse_mapped_type: wrong type data"
        );
        self.payload()
    }
    #[inline]
    pub fn as_evolving_array_type(&self) -> &'static EvolvingArrayType {
        assert!(
            self.data_tag == TypeDataTag::EvolvingArray,
            "as_evolving_array_type: wrong type data"
        );
        self.payload()
    }
    #[inline]
    pub fn as_type_parameter(&self) -> &'static TypeParameter {
        assert!(self.data_tag == TypeDataTag::TypeParameter, "as_type_parameter: wrong type data");
        self.payload()
    }
    #[inline]
    pub fn as_union_type(&self) -> &'static UnionType {
        assert!(self.data_tag == TypeDataTag::Union, "as_union_type: wrong type data");
        self.payload()
    }
    #[inline]
    pub fn as_intersection_type(&self) -> &'static IntersectionType {
        assert!(
            self.data_tag == TypeDataTag::Intersection,
            "as_intersection_type: wrong type data"
        );
        self.payload()
    }
    #[inline]
    pub fn as_index_type(&self) -> &'static IndexType {
        assert!(self.data_tag == TypeDataTag::Index, "as_index_type: wrong type data");
        self.payload()
    }
    #[inline]
    pub fn as_indexed_access_type(&self) -> &'static IndexedAccessType {
        assert!(
            self.data_tag == TypeDataTag::IndexedAccess,
            "as_indexed_access_type: wrong type data"
        );
        self.payload()
    }
    #[inline]
    pub fn as_template_literal_type(&self) -> &'static TemplateLiteralType {
        assert!(
            self.data_tag == TypeDataTag::TemplateLiteral,
            "as_template_literal_type: wrong type data"
        );
        self.payload()
    }
    #[inline]
    pub fn as_string_mapping_type(&self) -> &'static StringMappingType {
        assert!(
            self.data_tag == TypeDataTag::StringMapping,
            "as_string_mapping_type: wrong type data"
        );
        self.payload()
    }
    #[inline]
    pub fn as_substitution_type(&self) -> &'static SubstitutionType {
        assert!(
            self.data_tag == TypeDataTag::Substitution,
            "as_substitution_type: wrong type data"
        );
        self.payload()
    }
    #[inline]
    pub fn as_conditional_type(&self) -> &'static ConditionalType {
        assert!(self.data_tag == TypeDataTag::Conditional, "as_conditional_type: wrong type data");
        self.payload()
    }

    // Casts for embedded struct types. `as_*` panics where Go would return nil; `try_as_*` mirrors Go's nil result.

    #[inline]
    pub fn as_structured_type(&self) -> &'static StructuredType {
        self.data().as_structured_type().expect("as_structured_type: wrong type data")
    }
    #[inline]
    pub fn as_object_type(&self) -> &'static ObjectType {
        self.data().as_object_type().expect("as_object_type: wrong type data")
    }
    #[inline]
    pub fn as_type_reference(&self) -> &'static TypeReference {
        self.data().as_type_reference().expect("as_type_reference: wrong type data")
    }
    #[inline]
    pub fn as_interface_type(&self) -> &'static InterfaceType {
        self.data().as_interface_type().expect("as_interface_type: wrong type data")
    }
    #[inline]
    pub(crate) fn as_union_or_intersection_type(&self) -> &'static UnionOrIntersectionType {
        self.data()
            .as_union_or_intersection_type()
            .expect("as_union_or_intersection_type: wrong type data")
    }
    #[inline]
    pub fn try_as_constrained_type(&self) -> Option<&'static ConstrainedType> {
        self.data().as_constrained_type()
    }
    #[inline]
    pub fn try_as_structured_type(&self) -> Option<&'static StructuredType> {
        self.data().as_structured_type()
    }
    #[inline]
    pub fn try_as_object_type(&self) -> Option<&'static ObjectType> {
        self.data().as_object_type()
    }
    #[inline]
    pub fn try_as_type_reference(&self) -> Option<&'static TypeReference> {
        self.data().as_type_reference()
    }
    #[inline]
    pub fn try_as_interface_type(&self) -> Option<&'static InterfaceType> {
        self.data().as_interface_type()
    }
    #[inline]
    pub fn try_as_union_or_intersection_type(&self) -> Option<&'static UnionOrIntersectionType> {
        self.data().as_union_or_intersection_type()
    }

    // Common accessors

    #[inline]
    pub fn target(&self) -> Option<P<Type>> {
        let flags = self.flags.get();
        if flags.intersects(TypeFlags::Object) {
            return self.as_object_type().target.get();
        }
        if flags.intersects(TypeFlags::TypeParameter) {
            return self.as_type_parameter().target.get();
        }
        if flags.intersects(TypeFlags::Index) {
            return self.as_index_type().target.get();
        }
        if flags.intersects(TypeFlags::StringMapping) {
            return self.as_string_mapping_type().target.get();
        }
        panic!("Unhandled case in Type.Target")
    }

    /// `target()` of an object type, without the flag dispatch (`target()` is out of line at most call sites).
    #[inline]
    pub(crate) fn object_target(&self) -> Option<P<Type>> {
        self.as_object_type().target.get()
    }

    #[inline]
    pub(crate) fn mapper(&self) -> Option<P<TypeMapper>> {
        let flags = self.flags.get();
        if flags.intersects(TypeFlags::Object) {
            return self.as_object_type().mapper.get();
        }
        if flags.intersects(TypeFlags::TypeParameter) {
            return self.as_type_parameter().mapper.get();
        }
        if flags.intersects(TypeFlags::Conditional) {
            return self.as_conditional_type().mapper.get();
        }
        panic!("Unhandled case in Type.Mapper")
    }

    #[inline]
    pub fn types(&self) -> &'static [P<Type>] {
        let flags = self.flags.get();
        if flags.intersects(TypeFlags::UnionOrIntersection) {
            return self.as_union_or_intersection_type().types.get();
        }
        if flags.intersects(TypeFlags::TemplateLiteral) {
            return self.as_template_literal_type().types.get();
        }
        panic!("Unhandled case in Type.Types")
    }

    #[inline]
    pub(crate) fn target_interface_type(&self) -> &'static InterfaceType {
        self.as_type_reference().target.get().unwrap().as_interface_type()
    }

    #[inline]
    pub(crate) fn target_tuple_type(&self) -> &'static TupleType {
        self.as_type_reference().target.get().unwrap().as_tuple_type()
    }

    #[inline]
    pub fn symbol(&self) -> Option<P<Symbol>> {
        self.symbol_or_alias.get().symbol()
    }

    #[inline]
    pub(crate) fn set_symbol(&self, symbol: Option<P<Symbol>>) {
        let word = self.symbol_or_alias.get();
        match word.record() {
            Some(r) => r.symbol.set(symbol),
            None => self.symbol_or_alias.set(TypeSymbolWord::symbol_word(symbol)),
        }
    }

    #[inline]
    pub fn alias(&self) -> Option<P<TypeAlias>> {
        self.symbol_or_alias.get().record()?.alias.get()
    }

    #[inline]
    pub(crate) fn set_alias(&self, alias: Option<P<TypeAlias>>) {
        let word = self.symbol_or_alias.get();
        match word.record() {
            Some(r) => r.alias.set(alias),
            None if alias.is_some() => {
                let r = P::new(TypeSymbolAlias {
                    symbol: Cell::new(word.symbol()),
                    alias: Cell::new(alias),
                });
                self.symbol_or_alias.set(TypeSymbolWord(r.pack() | TypeSymbolWord::RECORD));
            }
            None => {}
        }
    }

    #[inline]
    pub fn is_union(&self) -> bool {
        self.flags.get().intersects(TypeFlags::Union)
    }

    #[inline]
    pub fn is_string(&self) -> bool {
        self.flags.get().intersects(TypeFlags::String)
    }

    #[inline]
    pub fn is_intersection(&self) -> bool {
        self.flags.get().intersects(TypeFlags::Intersection)
    }

    #[inline]
    pub fn is_string_literal(&self) -> bool {
        self.flags.get().intersects(TypeFlags::StringLiteral)
    }

    #[inline]
    pub fn is_number_literal(&self) -> bool {
        self.flags.get().intersects(TypeFlags::NumberLiteral)
    }

    #[inline]
    pub fn is_big_int_literal(&self) -> bool {
        self.flags.get().intersects(TypeFlags::BigIntLiteral)
    }

    #[inline]
    pub fn is_enum_literal(&self) -> bool {
        self.flags.get().intersects(TypeFlags::EnumLiteral)
    }

    #[inline]
    pub fn is_boolean_like(&self) -> bool {
        self.flags.get().intersects(TypeFlags::BooleanLike)
    }

    #[inline]
    pub fn is_string_like(&self) -> bool {
        self.flags.get().intersects(TypeFlags::StringLike)
    }

    #[inline]
    pub fn is_class(&self) -> bool {
        self.object_flags.get().intersects(ObjectFlags::Class)
    }

    #[inline]
    pub fn is_type_parameter(&self) -> bool {
        self.flags.get().intersects(TypeFlags::TypeParameter)
    }

    #[inline]
    pub(crate) fn is_index(&self) -> bool {
        self.flags.get().intersects(TypeFlags::Index)
    }

    /// Go `t.IsTupleType()` (= `isTupleType(t)`).
    #[inline]
    pub fn is_tuple_type(&self) -> bool {
        self.object_flags.get().intersects(ObjectFlags::Reference)
            && self
                .as_type_reference()
                .target
                .get()
                .is_some_and(|t| t.object_flags.get().intersects(ObjectFlags::Tuple))
    }
}

/// Methods Go defines on `*Type` that need the pointer itself.
pub trait TypeExt {
    /// Go `t.Distributed()`.
    fn distributed(self) -> Vec<P<Type>>;
    /// `distributed()` without the vector, for iterating over it.
    fn distributed_iter(self) -> DistributedIter;
}

/// The types of `TypeExt::distributed_iter`: a union's constituents, nothing for `never`, or the type itself.
pub struct DistributedIter {
    one: Option<P<Type>>,
    many: std::slice::Iter<'static, P<Type>>,
}

impl Iterator for DistributedIter {
    type Item = P<Type>;
    #[inline]
    fn next(&mut self) -> Option<P<Type>> {
        if let Some(t) = self.one.take() {
            return Some(t);
        }
        self.many.next().copied()
    }
}

impl TypeExt for P<Type> {
    #[inline]
    fn distributed_iter(self) -> DistributedIter {
        if self.flags.get().intersects(TypeFlags::Union) {
            return DistributedIter { one: None, many: self.as_union_type().types.get().iter() };
        }
        let one = (!self.flags.get().intersects(TypeFlags::Never)).then_some(self);
        DistributedIter { one, many: [].iter() }
    }

    #[inline]
    fn distributed(self) -> Vec<P<Type>> {
        if self.flags.get().intersects(TypeFlags::Union) {
            return self.as_union_type().types.get().to_vec();
        }
        if self.flags.get().intersects(TypeFlags::Never) {
            return Vec::new();
        }
        vec![self]
    }
}

// TypeData

/// Go's `TypeData` interface: a view of the data struct stored in the same allocation, right after the `Type`
/// header (`TypeAlloc`, `Type::alloc`).
///
/// Go's `TypeBase` (which embeds the `Type` header) has no Rust counterpart: header fields live on `Type`.
#[derive(Clone, Copy)]
pub enum TypeData {
    Intrinsic(&'static IntrinsicType),
    Literal(&'static LiteralType),
    UniqueESSymbol(&'static UniqueESSymbolType),
    Object(&'static ObjectType), // anonymous (and instantiated anonymous) object types
    TypeReference(&'static TypeReference),
    Interface(&'static InterfaceType),
    Tuple(&'static TupleType),
    InstantiationExpression(&'static InstantiationExpressionType),
    Mapped(&'static MappedType),
    ReverseMapped(&'static ReverseMappedType),
    EvolvingArray(&'static EvolvingArrayType),
    Union(&'static UnionType),
    Intersection(&'static IntersectionType),
    TypeParameter(&'static TypeParameter),
    Index(&'static IndexType),
    IndexedAccess(&'static IndexedAccessType),
    TemplateLiteral(&'static TemplateLiteralType),
    StringMapping(&'static StringMappingType),
    Substitution(&'static SubstitutionType),
    Conditional(&'static ConditionalType),
}

impl TypeData {
    fn as_constrained_type(&self) -> Option<&'static ConstrainedType> {
        Some(match *self {
            TypeData::Intrinsic(_) | TypeData::Literal(_) | TypeData::UniqueESSymbol(_) => {
                return None;
            }
            // Structured types keep their base constraint in the checker (see `StructuredType`).
            TypeData::Object(_)
            | TypeData::TypeReference(_)
            | TypeData::Interface(_)
            | TypeData::Tuple(_)
            | TypeData::InstantiationExpression(_)
            | TypeData::Mapped(_)
            | TypeData::ReverseMapped(_)
            | TypeData::EvolvingArray(_)
            | TypeData::Union(_)
            | TypeData::Intersection(_) => return None,
            TypeData::TypeParameter(d) => d,
            TypeData::Index(d) => d,
            TypeData::IndexedAccess(d) => d,
            TypeData::TemplateLiteral(d) => d,
            TypeData::StringMapping(d) => d,
            TypeData::Substitution(d) => d,
            TypeData::Conditional(d) => d,
        })
    }

    fn as_structured_type(&self) -> Option<&'static StructuredType> {
        Some(match *self {
            TypeData::Object(d) => d,
            TypeData::TypeReference(d) => d,
            TypeData::Interface(d) => d,
            TypeData::Tuple(d) => d,
            TypeData::InstantiationExpression(d) => d,
            TypeData::Mapped(d) => d,
            TypeData::ReverseMapped(d) => d,
            TypeData::EvolvingArray(d) => d,
            TypeData::Union(d) => d,
            TypeData::Intersection(d) => d,
            _ => return None,
        })
    }

    #[inline]
    pub fn as_object_type(&self) -> Option<&'static ObjectType> {
        Some(match *self {
            TypeData::Object(d) => d,
            TypeData::TypeReference(d) => d,
            TypeData::Interface(d) => d,
            TypeData::Tuple(d) => d,
            TypeData::InstantiationExpression(d) => d,
            TypeData::Mapped(d) => d,
            TypeData::ReverseMapped(d) => d,
            TypeData::EvolvingArray(d) => d,
            _ => return None,
        })
    }

    #[inline]
    fn as_type_reference(&self) -> Option<&'static TypeReference> {
        Some(match *self {
            TypeData::TypeReference(d) => d,
            TypeData::Interface(d) => d,
            TypeData::Tuple(d) => d,
            _ => return None,
        })
    }

    #[inline]
    fn as_interface_type(&self) -> Option<&'static InterfaceType> {
        Some(match *self {
            TypeData::Interface(d) => d,
            TypeData::Tuple(d) => d,
            _ => return None,
        })
    }

    #[inline]
    fn as_union_or_intersection_type(&self) -> Option<&'static UnionOrIntersectionType> {
        Some(match *self {
            TypeData::Union(d) => d,
            TypeData::Intersection(d) => d,
            _ => return None,
        })
    }
}

macro_rules! embeds {
    ($t:ty, $field:ident, $parent:ty) => {
        impl std::ops::Deref for $t {
            type Target = $parent;
            #[inline]
            fn deref(&self) -> &$parent {
                &self.$field
            }
        }
    };
}

// IntrinsicTypeData

#[derive(Default)]
pub struct IntrinsicType {
    pub(crate) intrinsic_name: Cell<&'static str>,
}

impl IntrinsicType {
    #[inline]
    pub fn intrinsic_name(&self) -> &'static str {
        self.intrinsic_name.get()
    }
}

// LiteralTypeData

/// Go `any` value of a literal type: `string | jsnum.Number | bool | PseudoBigInt`. Go's nil is `Option::None`
/// (`Option<LiteralValue>`), matching how the generated signatures map a nil-able `any`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum LiteralValue {
    String(&'static str),
    Number(jsnum::Number),
    Boolean(bool),
    BigInt(jsnum::PseudoBigInt),
}

#[derive(Default)]
pub struct LiteralType {
    pub(crate) value: Cell<Option<LiteralValue>>, // string | jsnum.Number | bool | PseudoBigInt | nil (computed enum)
    pub(crate) fresh_type: OvCell<Option<P<Type>>>, // Fresh version of type
    pub(crate) regular_type: Cell<Option<P<Type>>>, // Regular version of type
}

impl LiteralType {
    #[inline]
    pub fn value(&self) -> Option<LiteralValue> {
        self.value.get()
    }
    /// Compare stored values without materializing the whole literal enum for numeric pairs.
    #[inline]
    pub(crate) fn value_equals(&self, other: &LiteralType) -> bool {
        match self.value() {
            Some(LiteralValue::Number(left)) => {
                matches!(other.value(), Some(LiteralValue::Number(right)) if left == right)
            }
            value => value == other.value(),
        }
    }
    #[inline]
    pub fn fresh_type(&self) -> Option<P<Type>> {
        self.fresh_type.get()
    }
    #[inline]
    pub fn regular_type(&self) -> Option<P<Type>> {
        self.regular_type.get()
    }
}

// UniqueESSymbolTypeData

#[derive(Default)]
pub struct UniqueESSymbolType {
    pub(crate) name: Cell<&'static str>,
}

// ConstrainedType (type with computed base constraint)

#[derive(Default)]
pub struct ConstrainedType {
    pub(crate) resolved_base_constraint: OvCell<Option<P<Type>>>,
}

// StructuredType (base of all types with members)

// Go's StructuredType embeds ConstrainedType; here the base constraint of a structured type is kept in
// `Checker::structured_type_base_constraints` (`resolved_base_constraint_of`), and
// `try_as_constrained_type()` is None for structured types.
//
// Go now keeps members and properties inline, with optional signatures, index infos, and abstract-construct cache
// in a cold extra record. Rust keeps all resolved members in one `StructuredMembers` record allocated on first write.
// Many remain unresolved through lazy tables or unqueried union/intersection members, carrying one pointer instead of 48 bytes.
// Reads of an absent record return the zero values (nil members, empty slices, count 0), exactly like reading the
// unset fields; once allocated, every getter returns exactly what was last set (except that an empty index info
// list reads as `&[]` until a non-empty one is set, see `CountOrIndexInfos`).
#[derive(Default)]
pub struct StructuredType {
    resolved: OvCell<Option<P<StructuredMembers>>>,
    // Go's objectTypeWithoutAbstractConstructSignatures is `Checker::object_types_without_abstract_construct_signatures`.
}

#[derive(Default)]
struct StructuredMembers {
    members: Cell<Option<P<SymbolTable>>>,
    // `ThinSliceCell`s are one word each (`tsrs_core::ThinSlice`).
    properties: ThinSliceCell<P<Symbol>>,
    signatures: ThinSliceCell<P<Signature>>, // Signatures (call + construct)
    // Count of call signatures, and index infos (2% of the resolved types on the private monorepo have any).
    count_or_index_infos: CountOrIndexInfos,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    std::mem::size_of::<StructuredType>() == if tsrs_core::COMPRESSED_PTRS { 4 } else { 8 }
);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<StructuredType>() == 4);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<StructuredMembers>() == 32);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<StructuredMembers>() == 24);

/// Go's `CallSignatureCount` and `IndexInfos` in one word: `count << 1 | 1` while no non-empty index info list was
/// set (the list reads empty, `&[]`), else a pointer to an `IndexInfosTail` holding both.
struct CountOrIndexInfos(Cell<RawWord>);

struct IndexInfosTail {
    index_infos: ThinSliceCell<P<IndexInfo>>,
    call_signature_count: CountCell,
}

impl Default for CountOrIndexInfos {
    #[inline]
    fn default() -> Self {
        CountOrIndexInfos(Cell::new(RawWord(1)))
    }
}

impl CountOrIndexInfos {
    #[inline]
    fn tail(&self) -> Option<&'static IndexInfosTail> {
        let p: *const IndexInfosTail = self.0.get().ptr();
        // SAFETY: an even word is the tail allocated by `set_index_infos` (never freed).
        (p.addr() & 1 == 0).then(|| unsafe { &*p })
    }
    #[inline]
    fn call_signature_count(&self) -> usize {
        match self.tail() {
            Some(t) => t.call_signature_count.get(),
            None => self.0.get().0 >> 1,
        }
    }
    #[inline]
    fn set_call_signature_count(&self, count: usize) {
        match self.tail() {
            Some(t) => t.call_signature_count.set(count),
            None => self.0.set(RawWord((count << 1) | 1)),
        }
    }
    #[inline]
    fn index_infos(&self) -> &'static [P<IndexInfo>] {
        self.tail().map_or(&[], |t| t.index_infos.get())
    }
    #[inline]
    fn set_index_infos(&self, index_infos: &'static [P<IndexInfo>]) {
        if let Some(t) = self.tail() {
            t.index_infos.set(index_infos);
        } else if !index_infos.is_empty() {
            let count = self.call_signature_count();
            let t = P::new(IndexInfosTail {
                index_infos: ThinSliceCell::new(index_infos),
                call_signature_count: CountCell(Cell::new(count as u32)),
            });
            self.0.set(RawWord::from_ptr(std::ptr::from_ref::<IndexInfosTail>(t.get())));
        }
    }
}

impl StructuredType {
    #[inline]
    fn resolved_for_write(&self) -> P<StructuredMembers> {
        if let Some(resolved) = self.resolved.get() {
            resolved
        } else {
            let resolved = P::new(StructuredMembers::default());
            self.resolved.set(Some(resolved));
            resolved
        }
    }

    #[inline]
    pub(crate) fn members(&self) -> Option<P<SymbolTable>> {
        self.resolved.get()?.members.get()
    }
    #[inline]
    pub(crate) fn set_members(&self, members: Option<P<SymbolTable>>) {
        self.resolved_for_write().members.set(members);
    }
    #[inline]
    pub fn properties(&self) -> &'static [P<Symbol>] {
        self.resolved.get().map_or(&[], |r| r.properties.get())
    }
    #[inline]
    pub(crate) fn set_properties(&self, properties: &'static [P<Symbol>]) {
        self.resolved_for_write().properties.set(properties);
    }
    /// Call signatures followed by construct signatures.
    #[inline]
    pub(crate) fn signatures(&self) -> &'static [P<Signature>] {
        self.resolved.get().map_or(&[], |r| r.signatures.get())
    }
    #[inline]
    pub(crate) fn set_signatures(&self, signatures: &'static [P<Signature>]) {
        self.resolved_for_write().signatures.set(signatures);
    }
    #[inline]
    fn call_signature_count(&self) -> usize {
        self.resolved.get().map_or(0, |r| r.count_or_index_infos.call_signature_count())
    }
    #[inline]
    pub(crate) fn set_call_signature_count(&self, count: usize) {
        self.resolved_for_write().count_or_index_infos.set_call_signature_count(count);
    }
    #[inline]
    pub(crate) fn index_infos(&self) -> &'static [P<IndexInfo>] {
        self.resolved.get().map_or(&[], |r| r.count_or_index_infos.index_infos())
    }
    #[inline]
    pub(crate) fn set_index_infos(&self, index_infos: &'static [P<IndexInfo>]) {
        self.resolved_for_write().count_or_index_infos.set_index_infos(index_infos);
    }
    #[inline]
    pub(crate) fn call_signatures(&self) -> &'static [P<Signature>] {
        &self.signatures()[..self.call_signature_count()]
    }
    #[inline]
    pub(crate) fn construct_signatures(&self) -> &'static [P<Signature>] {
        &self.signatures()[self.call_signature_count()..]
    }
}

// Except for tuple type references and reverse mapped types, all object types have an associated symbol.
// Possible object type instances are listed in the following.

// InterfaceType:
// ObjectFlagsClass: Originating non-generic class type
// ObjectFlagsClass|ObjectFlagsReference: Originating generic class type
// ObjectFlagsInterface: Originating non-generic interface type
// ObjectFlagsInterface|ObjectFlagsReference: Originating generic interface type

// TupleType:
// ObjectFlagsReference|ObjectFlagsTuple: Originating generic tuple type (synthesized)

// TypeReference
// ObjectFlagsReference: Instantiated generic class, interface, or tuple type

// ObjectType:
// ObjectFlagsAnonymous: Originating anonymous object type
// ObjectFlagsAnonymous|ObjectFlagsInstantiated: Instantiated anonymous object type

// MappedType:
// ObjectFlagsMapped: Originating mapped type
// ObjectFlagsMapped|ObjectFlagsInstantiated: Instantiated mapped type

// InstantiationExpressionType:
// ObjectFlagsAnonymous|ObjectFlagsInstantiationExpression: Originating instantiation expression type
// ObjectFlagsAnonymous|ObjectFlagsInstantiated|ObjectFlagsInstantiationExpression: Instantiated instantiation expression type

// ReverseMappedType:
// ObjectFlagsAnonymous|ObjectFlagsReverseMapped: Reverse mapped type

// EvolvingArrayType:
// ObjectFlagsEvolvingArray: Evolving array type

/// Keeps embedded object-type headers at the allocation's start.
///
/// `repr(C)` here and on the other object, union and intersection data structs (each embedding its parent as
/// its first field) puts the `ObjectType` / `StructuredType` of every such type at one offset from the
/// header, so `TypeData::as_object_type` and `as_structured_type` compile to one range test of the data tag
/// instead of a switch over per-struct field offsets.
#[derive(Default)]
#[repr(C)]
pub struct ObjectType {
    structured_type: StructuredType,
    pub(crate) target: Cell<Option<P<Type>>>, // Target of instantiated type
    pub(crate) mapper: MapperCell,            // Type mapper for instantiated type
                                              // Go's `instantiations` map is used only by the targets of instantiations: generic interfaces and tuples keep
                                              // it in `InterfaceType`, other object types (declared anonymous and mapped types, deferred type references) in
                                              // `Checker::object_type_instantiations`, so the millions of instantiated object types and references do not
                                              // carry it.
}
embeds!(ObjectType, structured_type, StructuredType);

// TypeReference (instantiation of an InterfaceType)

#[derive(Default)]
#[repr(C)]
pub struct TypeReference {
    object_type: ObjectType,
    pub(crate) node: Cell<Option<P<Node>>>, // TypeReferenceNode | ArrayTypeNode | TupleTypeNode when deferred, else nil
    pub(crate) resolved_type_arguments: OvOptionThinSliceCell<P<Type>>, // nil = not computed (Go tests against nil)
}
embeds!(TypeReference, object_type, ObjectType);

// InterfaceType (when generic, serves as reference to instantiation of itself)

/// Go's `instantiations` map of a generic class, interface or tuple target (`map[CacheHashKey]*Type`, keyed
/// by `getTypeListKey(typeArguments)`).
///
/// Every value is a non-deferred reference whose `resolved_type_arguments` are exactly the list its key was
/// made from (the target itself for its type parameters), and they never change, so the table stores only the
/// references and compares type-argument lists: one word per slot instead of the 128-bit key plus the value,
/// and a lookup hashes the type ids instead of xxh3 over the key bytes. Exact list equality maps lists to
/// references like Go's collision-free 128-bit key does. Nil until `make()`, like the Go map.
#[derive(Default)]
pub struct ReferenceInstantiations(OvExact<Option<P<RefCell<hashbrown::HashTable<P<Type>>>>>>);

impl ReferenceInstantiations {
    #[inline]
    fn hash(type_arguments: &[P<Type>]) -> u64 {
        use std::hash::Hasher;
        let mut h = rustc_hash::FxHasher::default();
        h.write_usize(type_arguments.len());
        for t in type_arguments {
            h.write_u32(t.id.0);
        }
        h.finish()
    }

    #[inline]
    fn arguments_of(reference: P<Type>) -> &'static [P<Type>] {
        reference.as_type_reference().resolved_type_arguments.get().unwrap()
    }

    /// Go `m = make(map[CacheHashKey]*Type)`.
    #[inline]
    pub(crate) fn make(&self) {
        self.0.set(Some(P::new(RefCell::new(hashbrown::HashTable::new()))));
    }

    /// Go `m[getTypeListKey(typeArguments)]`.
    #[inline]
    pub(crate) fn get(&self, type_arguments: &[P<Type>]) -> Option<P<Type>> {
        let cell = self.0.get()?;
        tsrs_core::sharedgraph::with_ref(&cell, |table| {
            table
                .find(Self::hash(type_arguments), |&t| Self::arguments_of(t) == type_arguments)
                .copied()
        })
    }

    /// Go `m[getTypeListKey(reference's type arguments)] = reference` for a key that is not present yet.
    pub(crate) fn add(&self, reference: P<Type>) {
        if self.0.get().is_none() {
            self.make();
        }
        let mut cell = self.0.get().unwrap();
        if tsrs_core::sharedgraph::frozen(cell.get()) {
            // Shared graph: this checker's copy of a frozen table.
            cell = P::new(RefCell::new(tsrs_core::sharedgraph::with_ref(&cell, Clone::clone)));
            self.0.set(Some(cell));
        }
        let mut table = cell.borrow_mut();
        let arguments = Self::arguments_of(reference);
        debug_assert!(
            table.find(Self::hash(arguments), |&t| Self::arguments_of(t) == arguments).is_none()
        );
        table.insert_unique(Self::hash(arguments), reference, |&t| {
            Self::hash(Self::arguments_of(t))
        });
    }
}

#[derive(Default)]
#[repr(C)]
pub struct InterfaceType {
    type_reference: TypeReference,
    pub(crate) all_type_parameters: Cell<&'static [P<Type>]>, // Type parameters (outer + local + thisType)
    pub(crate) resolved_base_types: OvCell<&'static [P<Type>]>,
    pub(crate) declared_call_signatures: OvCell<&'static [P<Signature>]>, // Declared call signatures
    pub(crate) declared_construct_signatures: OvCell<&'static [P<Signature>]>, // Declared construct signatures
    pub(crate) declared_index_infos: OvCell<&'static [P<IndexInfo>]>, // Declared index signatures
    pub(crate) outer_type_parameter_count: CountCell, // Count of outer type parameters
    pub(crate) this_type: Cell<Option<P<Type>>>,      // The "this" type (nil if none)
    pub(crate) resolved_base_constructor_type: OvCell<Option<P<Type>>>,
    pub(crate) declared_members: OvCell<Option<P<SymbolTable>>>, // Declared members
    pub(crate) instantiations: ReferenceInstantiations, // Map of type instantiations (Go: in ObjectType)
    pub(crate) base_types_resolved: OvCell<bool>,
    pub(crate) declared_members_resolved: OvCell<bool>,
}
embeds!(InterfaceType, type_reference, TypeReference);

impl InterfaceType {
    #[inline]
    pub fn outer_type_parameters(&self) -> &'static [P<Type>] {
        let all = self.all_type_parameters.get();
        if all.is_empty() {
            return &[];
        }
        &all[..self.outer_type_parameter_count.get()]
    }

    #[inline]
    pub fn local_type_parameters(&self) -> &'static [P<Type>] {
        let all = self.all_type_parameters.get();
        if all.is_empty() {
            return &[];
        }
        &all[self.outer_type_parameter_count.get()..all.len() - 1]
    }

    #[inline]
    pub fn type_parameters(&self) -> &'static [P<Type>] {
        let all = self.all_type_parameters.get();
        if all.is_empty() {
            return &[];
        }
        &all[..all.len() - 1]
    }

    #[inline]
    pub fn this_type(&self) -> Option<P<Type>> {
        self.this_type.get()
    }
}

// TupleType

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct ElementFlags: u32 {
        const None = 0;
        const Required = 1 << 0; // T
        const Optional = 1 << 1; // T?
        const Rest = 1 << 2; // ...T[]
        const Variadic = 1 << 3; // ...T
        const Fixed = Self::Required.bits() | Self::Optional.bits();
        const Variable = Self::Rest.bits() | Self::Variadic.bits();
        const NonRequired = Self::Optional.bits() | Self::Rest.bits() | Self::Variadic.bits();
        const NonRest = Self::Required.bits() | Self::Optional.bits() | Self::Variadic.bits();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub struct TupleElementInfo {
    pub(crate) flags: ElementFlags,
    pub(crate) labeled_declaration: Option<P<Node>>, // NamedTupleMember | ParameterDeclaration | nil
}

impl TupleElementInfo {
    #[inline]
    pub fn labeled_declaration(self) -> Option<P<Node>> {
        self.labeled_declaration
    }
}

#[derive(Default)]
#[repr(C)]
pub struct TupleType {
    interface_type: InterfaceType,
    pub(crate) element_infos: Cell<&'static [TupleElementInfo]>,
    pub(crate) min_length: CountCell, // Number of required or variadic elements
    pub(crate) fixed_length: CountCell, // Number of initial required or optional elements
    pub(crate) combined_flags: Cell<ElementFlags>,
    pub(crate) readonly: Cell<bool>,
}
embeds!(TupleType, interface_type, InterfaceType);

impl TupleType {
    #[inline]
    pub fn fixed_length(&self) -> usize {
        self.fixed_length.get()
    }
    #[inline]
    pub fn is_readonly(&self) -> bool {
        self.readonly.get()
    }
    #[inline]
    pub fn element_flags(&self) -> Vec<ElementFlags> {
        self.element_infos.get().iter().map(|info| info.flags).collect()
    }
    #[inline]
    pub fn element_infos(&self) -> &'static [TupleElementInfo] {
        self.element_infos.get()
    }
}

// InstantiationExpressionType

#[derive(Default)]
#[repr(C)]
pub struct InstantiationExpressionType {
    object_type: ObjectType,
    pub(crate) node: Cell<Option<P<Node>>>,
}
embeds!(InstantiationExpressionType, object_type, ObjectType);

// MappedType

#[derive(Default)]
#[repr(C)]
pub struct MappedType {
    object_type: ObjectType,
    pub(crate) declaration: Cell<Option<P<Node>>>, // MappedTypeNode
    pub(crate) type_parameter: Cell<Option<P<Type>>>,
    pub(crate) constraint_type: OvCell<Option<P<Type>>>,
    pub(crate) name_type: OvCell<Option<P<Type>>>,
    pub(crate) template_type: OvCell<Option<P<Type>>>,
    pub(crate) modifiers_type: OvCell<Option<P<Type>>>,
    pub(crate) resolved_apparent_type: OvCell<Option<P<Type>>>,
    pub(crate) contains_error: Cell<bool>,
}
embeds!(MappedType, object_type, ObjectType);

impl MappedType {
    #[inline]
    pub fn type_parameter(&self) -> Option<P<Type>> {
        self.type_parameter.get()
    }
    #[inline]
    pub fn constraint_type(&self) -> Option<P<Type>> {
        self.constraint_type.get()
    }
    #[inline]
    pub fn name_type(&self) -> Option<P<Type>> {
        self.name_type.get()
    }
    #[inline]
    pub fn template_type(&self) -> Option<P<Type>> {
        self.template_type.get()
    }
    #[inline]
    pub fn resolve_components(c: &mut Checker, typ: P<Type>) {
        c.get_type_parameter_from_mapped_type(typ);
        c.get_constraint_type_from_mapped_type(typ);
        c.get_name_type_from_mapped_type(typ);
        c.get_template_type_from_mapped_type(typ);
    }
}

// ReverseMappedType

#[derive(Default)]
#[repr(C)]
pub struct ReverseMappedType {
    object_type: ObjectType,
    pub(crate) source: Cell<Option<P<Type>>>,
    pub(crate) mapped_type: Cell<Option<P<Type>>>,
    pub(crate) constraint_type: Cell<Option<P<Type>>>,
}
embeds!(ReverseMappedType, object_type, ObjectType);

// EvolvingArrayType

#[derive(Default)]
#[repr(C)]
pub struct EvolvingArrayType {
    object_type: ObjectType,
    pub(crate) element_type: Cell<Option<P<Type>>>,
    pub(crate) final_array_type: OvCell<Option<P<Type>>>,
}
embeds!(EvolvingArrayType, object_type, ObjectType);

// UnionOrIntersectionTypeData

/// Keeps uncommon union and intersection metadata out of the common fields.
///
/// Go's `UnionOrIntersectionType` / `UnionType` / `IntersectionType` fields other than `types` are set on few
/// types (on the private monorepo single: of 1.07M unions 12% get an origin, 9.5% a key property name, ~6%
/// each a property cache, resolved properties, a reduced or a regular type; of 1.17M intersections 21% a
/// non-augmented property cache, 14% an apparent type, 4.4% resolved properties), so they live in a tail
/// allocated on the first non-nil write: a `UnionRare` or an `IntersectionRare`, both starting with the
/// shared `UnionOrIntersectionRare`.
///
/// Reads of an absent tail return the zero value, like the unset Go field. The tail word keeps the kind in
/// bit 0 (set at construction) so the shared accessors allocate the right tail.
#[derive(Default)]
#[repr(C)]
pub struct UnionOrIntersectionType {
    structured_type: StructuredType,
    pub(crate) types: ThinSliceCell<P<Type>>,
    rare: UnionOrIntersectionRareWord,
}
embeds!(UnionOrIntersectionType, structured_type, StructuredType);

#[derive(Default)]
#[repr(C)]
struct UnionOrIntersectionRare {
    resolved_properties: OvOptionThinSliceCell<P<Symbol>>, // nil = not computed (Go tests against nil)
    property_cache: OvExact<Option<P<SymbolTable>>>,
    property_cache_without_function_property_augment: OvExact<Option<P<SymbolTable>>>,
}

#[derive(Default)]
#[repr(C)]
struct UnionRare {
    shared: UnionOrIntersectionRare,
    resolved_reduced_type: OvCell<Option<P<Type>>>,
    regular_type: OvCell<Option<P<Type>>>,
    origin: Cell<Option<P<Type>>>, // Denormalized union, intersection, or index type in which union originates
    key_property_name: OvCell<&'static str>, // Property with unique unit type that exists in every object/intersection in union type
    constituent_map: GoMap<P<Type>, P<Type>>, // Constituents keyed by unit type discriminants
}

#[derive(Default)]
#[repr(C)]
struct IntersectionRare {
    shared: UnionOrIntersectionRare,
    resolved_apparent_type: OvCell<Option<P<Type>>>,
    unique_literal_filled_instantiation: OvCell<Option<P<Type>>>, // Instantiation with type parameters mapped to never type
}

/// The tail pointer (8-aligned, null while absent) with bit 0 set for an intersection.
struct UnionOrIntersectionRareWord(OvExact<RawWord>);

const RARE_INTERSECTION: usize = 1;

impl Default for UnionOrIntersectionRareWord {
    #[inline]
    fn default() -> Self {
        UnionOrIntersectionRareWord(OvExact::new(RawWord(0)))
    }
}

impl UnionOrIntersectionType {
    #[inline]
    pub fn types(&self) -> &'static [P<Type>] {
        self.types.get()
    }
    #[inline]
    fn rare(&self) -> Option<&'static UnionOrIntersectionRare> {
        let p: *const UnionOrIntersectionRare =
            self.rare.0.get().ptr::<UnionOrIntersectionRare>().map_addr(|a| a & !RARE_INTERSECTION);
        // SAFETY: a non-null address is the tail allocated by `rare_for_write` (never freed).
        (!p.is_null()).then(|| unsafe { &*p })
    }
    #[inline]
    fn is_intersection_data(&self) -> bool {
        self.rare.0.get().0 & RARE_INTERSECTION != 0
    }
    fn rare_for_write(&self) -> &'static UnionOrIntersectionRare {
        if let Some(r) = self.rare() {
            return r;
        }
        let kind = self.rare.0.get().0 & RARE_INTERSECTION;
        let p: *const UnionOrIntersectionRare = if kind != 0 {
            std::ptr::from_ref::<IntersectionRare>(P::new(IntersectionRare::default()).get()).cast()
        } else {
            std::ptr::from_ref::<UnionRare>(P::new(UnionRare::default()).get()).cast()
        };
        self.rare.0.set(RawWord::from_ptr(p.map_addr(|a| a | kind)));
        // SAFETY: just allocated; `repr(C)` with the shared part first.
        unsafe { &*p }
    }
    #[inline]
    pub(crate) fn resolved_properties(&self) -> Option<&'static [P<Symbol>]> {
        self.rare()?.resolved_properties.get()
    }
    #[inline]
    pub(crate) fn set_resolved_properties(&self, properties: Option<&'static [P<Symbol>]>) {
        if properties.is_some() || self.rare().is_some() {
            self.rare_for_write().resolved_properties.set(properties);
        }
    }
    /// Go `t.propertyCacheWithoutObjectFunctionPropertyAugment` when `skip_object_function_property_augment`, else
    /// `t.propertyCache`.
    #[inline]
    pub(crate) fn property_cache(
        &self,
        skip_object_function_property_augment: bool,
    ) -> Option<P<SymbolTable>> {
        let r = self.rare()?;
        if skip_object_function_property_augment {
            r.property_cache_without_function_property_augment.get()
        } else {
            r.property_cache.get()
        }
    }
    /// `property_cache`, created empty if it is nil (Go `getSymbolTable(&cache)`).
    #[inline]
    pub(crate) fn property_cache_for_write(
        &self,
        skip_object_function_property_augment: bool,
    ) -> P<SymbolTable> {
        let r = self.rare_for_write();
        let cell = if skip_object_function_property_augment {
            &r.property_cache_without_function_property_augment
        } else {
            &r.property_cache
        };
        ov_symbol_table(cell)
    }
}

// UnionType

#[derive(Default)]
#[repr(C)]
pub struct UnionType {
    union_or_intersection_type: UnionOrIntersectionType,
}
embeds!(UnionType, union_or_intersection_type, UnionOrIntersectionType);

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<UnionType>() == 24);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<UnionType>() == 16);

impl UnionType {
    #[inline]
    fn union_rare(&self) -> Option<&'static UnionRare> {
        debug_assert!(!self.union_or_intersection_type.is_intersection_data());
        // SAFETY: a union's tail is a `UnionRare` (`rare_for_write` with the kind bit clear).
        self.union_or_intersection_type
            .rare()
            .map(|r| unsafe { tsrs_core::ptr::arena_neighbor::<_, UnionRare>(r, 0) })
    }
    #[inline]
    fn union_rare_for_write(&self) -> &'static UnionRare {
        let r = self.union_or_intersection_type.rare_for_write();
        // SAFETY: as in `union_rare`.
        unsafe { tsrs_core::ptr::arena_neighbor::<_, UnionRare>(r, 0) }
    }
    #[inline]
    pub(crate) fn resolved_reduced_type(&self) -> Option<P<Type>> {
        self.union_rare()?.resolved_reduced_type.get()
    }
    #[inline]
    pub(crate) fn set_resolved_reduced_type(&self, t: Option<P<Type>>) {
        if t.is_some() || self.union_rare().is_some() {
            self.union_rare_for_write().resolved_reduced_type.set(t);
        }
    }
    #[inline]
    pub(crate) fn regular_type(&self) -> Option<P<Type>> {
        self.union_rare()?.regular_type.get()
    }
    #[inline]
    pub(crate) fn set_regular_type(&self, t: Option<P<Type>>) {
        if t.is_some() || self.union_rare().is_some() {
            self.union_rare_for_write().regular_type.set(t);
        }
    }
    #[inline]
    pub(crate) fn origin(&self) -> Option<P<Type>> {
        self.union_rare()?.origin.get()
    }
    #[inline]
    pub(crate) fn set_origin(&self, t: Option<P<Type>>) {
        if t.is_some() || self.union_rare().is_some() {
            self.union_rare_for_write().origin.set(t);
        }
    }
    #[inline]
    pub(crate) fn key_property_name(&self) -> &'static str {
        self.union_rare().map_or("", |r| r.key_property_name.get())
    }
    #[inline]
    pub(crate) fn set_key_property_name(&self, name: &'static str) {
        if !name.is_empty() || self.union_rare().is_some() {
            self.union_rare_for_write().key_property_name.set(name);
        }
    }
    /// Go `t.constituentMap` for reading (nil while there is no tail).
    #[inline]
    pub(crate) fn constituent_map(&self) -> Option<&'static GoMap<P<Type>, P<Type>>> {
        self.union_rare().map(|r| &r.constituent_map)
    }
    /// Go `t.constituentMap` for writing.
    #[inline]
    pub(crate) fn constituent_map_for_write(&self) -> &'static GoMap<P<Type>, P<Type>> {
        &self.union_rare_for_write().constituent_map
    }
}

// IntersectionType

#[repr(C)]
pub struct IntersectionType {
    union_or_intersection_type: UnionOrIntersectionType,
}
embeds!(IntersectionType, union_or_intersection_type, UnionOrIntersectionType);

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<IntersectionType>() == 24);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<IntersectionType>() == 16);

impl Default for IntersectionType {
    #[inline]
    fn default() -> Self {
        let d = UnionOrIntersectionType::default();
        d.rare.0.set(RawWord(RARE_INTERSECTION));
        IntersectionType { union_or_intersection_type: d }
    }
}

impl IntersectionType {
    #[inline]
    fn intersection_rare(&self) -> Option<&'static IntersectionRare> {
        debug_assert!(self.union_or_intersection_type.is_intersection_data());
        // SAFETY: an intersection's tail is an `IntersectionRare` (`rare_for_write` with the kind bit set).
        self.union_or_intersection_type
            .rare()
            .map(|r| unsafe { tsrs_core::ptr::arena_neighbor::<_, IntersectionRare>(r, 0) })
    }
    #[inline]
    fn intersection_rare_for_write(&self) -> &'static IntersectionRare {
        let r = self.union_or_intersection_type.rare_for_write();
        // SAFETY: as in `intersection_rare`.
        unsafe { tsrs_core::ptr::arena_neighbor::<_, IntersectionRare>(r, 0) }
    }
    #[inline]
    pub(crate) fn resolved_apparent_type(&self) -> Option<P<Type>> {
        self.intersection_rare()?.resolved_apparent_type.get()
    }
    #[inline]
    pub(crate) fn set_resolved_apparent_type(&self, t: Option<P<Type>>) {
        if t.is_some() || self.intersection_rare().is_some() {
            self.intersection_rare_for_write().resolved_apparent_type.set(t);
        }
    }
    #[inline]
    pub(crate) fn unique_literal_filled_instantiation(&self) -> Option<P<Type>> {
        self.intersection_rare()?.unique_literal_filled_instantiation.get()
    }
    #[inline]
    pub(crate) fn set_unique_literal_filled_instantiation(&self, t: Option<P<Type>>) {
        if t.is_some() || self.intersection_rare().is_some() {
            self.intersection_rare_for_write().unique_literal_filled_instantiation.set(t);
        }
    }
}

// TypeParameter

#[derive(Default)]
pub struct TypeParameter {
    constrained_type: ConstrainedType,
    pub(crate) constraint: OvCell<Option<P<Type>>>,
    pub(crate) target: Cell<Option<P<Type>>>,
    pub(crate) mapper: MapperCell,
    pub is_this_type: Cell<bool>,
    pub(crate) is_distributed: Cell<bool>,
    pub(crate) resolved_default_type: OvCell<Option<P<Type>>>,
    pub(crate) distributed_type: OvCell<Option<P<Type>>>,
}
embeds!(TypeParameter, constrained_type, ConstrainedType);

impl TypeParameter {
    #[inline]
    pub fn is_this_type(&self) -> bool {
        self.is_this_type.get()
    }
}

// IndexFlags

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct IndexFlags: u32 {
        const None = 0;
        const StringsOnly = 1 << 0;
        const NoIndexSignatures = 1 << 1;
        const NoReducibleCheck = 1 << 2;
    }
}

// IndexType

#[derive(Default)]
pub struct IndexType {
    constrained_type: ConstrainedType,
    pub(crate) target: Cell<Option<P<Type>>>,
    pub(crate) index_flags: Cell<IndexFlags>,
}
embeds!(IndexType, constrained_type, ConstrainedType);

impl IndexType {
    #[inline]
    pub fn target(&self) -> Option<P<Type>> {
        self.target.get()
    }
}

// IndexedAccessType

#[derive(Default)]
pub struct IndexedAccessType {
    constrained_type: ConstrainedType,
    pub(crate) object_type: Cell<Option<P<Type>>>,
    pub(crate) index_type: Cell<Option<P<Type>>>,
    pub(crate) access_flags: Cell<AccessFlags>, // Only includes AccessFlags.Persistent
}
embeds!(IndexedAccessType, constrained_type, ConstrainedType);

impl IndexedAccessType {
    #[inline]
    pub fn object_type(&self) -> Option<P<Type>> {
        self.object_type.get()
    }
    #[inline]
    pub fn index_type(&self) -> Option<P<Type>> {
        self.index_type.get()
    }
}

#[derive(Default)]
pub struct TemplateLiteralType {
    constrained_type: ConstrainedType,
    pub(crate) texts: Cell<&'static [&'static str]>, // Always one element longer than types
    pub(crate) types: Cell<&'static [P<Type>]>,      // Always at least one element
}
embeds!(TemplateLiteralType, constrained_type, ConstrainedType);

impl TemplateLiteralType {
    #[inline]
    pub fn texts(&self) -> &'static [&'static str] {
        self.texts.get()
    }
    #[inline]
    pub(crate) fn types(&self) -> &'static [P<Type>] {
        self.types.get()
    }
}

#[derive(Default)]
pub struct StringMappingType {
    constrained_type: ConstrainedType,
    pub(crate) target: Cell<Option<P<Type>>>,
}
embeds!(StringMappingType, constrained_type, ConstrainedType);

impl StringMappingType {
    #[inline]
    pub fn target(&self) -> Option<P<Type>> {
        self.target.get()
    }
}

#[derive(Default)]
pub struct SubstitutionType {
    constrained_type: ConstrainedType,
    pub(crate) base_type: Cell<Option<P<Type>>>, // Target type
    pub(crate) constraint: Cell<Option<P<Type>>>, // Constraint that target type is known to satisfy
}
embeds!(SubstitutionType, constrained_type, ConstrainedType);

impl SubstitutionType {
    #[inline]
    pub fn base_type(&self) -> Option<P<Type>> {
        self.base_type.get()
    }
    #[inline]
    pub fn subst_constraint(&self) -> Option<P<Type>> {
        self.constraint.get()
    }
}

#[derive(Default)]
pub struct ConditionalRoot {
    pub(crate) node: Cell<Option<P<Node>>>, // ConditionalTypeNode
    pub(crate) check_type: Cell<Option<P<Type>>>,
    pub(crate) extends_type: Cell<Option<P<Type>>>,
    pub(crate) is_distributive: Cell<bool>,
    pub(crate) infer_type_parameters: Cell<&'static [P<Type>]>,
    pub(crate) outer_type_parameters: Cell<&'static [P<Type>]>,
    pub(crate) instantiations: GoPackedMap<CacheHashKey, P<Type>>,
    pub(crate) alias: Cell<Option<P<TypeAlias>>>,
}

#[derive(Default)]
pub struct ConditionalType {
    constrained_type: ConstrainedType,
    pub(crate) root: Cell<Option<P<ConditionalRoot>>>,
    pub(crate) check_type: Cell<Option<P<Type>>>,
    pub(crate) extends_type: Cell<Option<P<Type>>>,
    pub(crate) resolved_true_type: OvCell<Option<P<Type>>>,
    pub(crate) resolved_false_type: OvCell<Option<P<Type>>>,
    pub(crate) resolved_inferred_true_type: OvCell<Option<P<Type>>>, // The `trueType` instantiated with the `combinedMapper`, if present
    pub(crate) resolved_default_constraint: OvCell<Option<P<Type>>>,
    pub(crate) resolved_constraint_of_distributive: OvCell<Option<P<Type>>>,
    pub(crate) mapper: MapperCell,
    pub(crate) combined_mapper: MapperCell,
}
embeds!(ConditionalType, constrained_type, ConstrainedType);

impl ConditionalType {
    #[inline]
    pub fn check_type(&self) -> Option<P<Type>> {
        self.check_type.get()
    }
    #[inline]
    pub fn extends_type(&self) -> Option<P<Type>> {
        self.extends_type.get()
    }
}

// SignatureFlags

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
    pub struct SignatureFlags: u32 {
        const None = 0;
        // Propagating flags
        const HasRestParameter = 1 << 0; // Indicates last parameter is rest parameter
        const HasLiteralTypes = 1 << 1; // Indicates signature is specialized
        const Construct = 1 << 2; // Indicates signature is a construct signature
        const Abstract = 1 << 3; // Indicates signature comes from an abstract class, abstract construct signature, or abstract constructor type
        // Non-propagating flags
        const IsInnerCallChain = 1 << 4; // Indicates signature comes from a CallChain nested in an outer OptionalChain
        const IsOuterCallChain = 1 << 5; // Indicates signature comes from a CallChain that is the outermost chain of an optional expression
        const IsUntypedSignatureInJSFile = 1 << 6; // Indicates signature is from a js file and has no types
        const IsNonInferrable = 1 << 7; // Indicates signature comes from a non-inferrable type
        const IsSignatureCandidateForOverloadFailure = 1 << 8;
        // We do not propagate `IsInnerCallChain` or `IsOuterCallChain` to instantiated signatures, as that would result in us
        // attempting to add `| undefined` on each recursive call to `getReturnTypeOfSignature` when
        // instantiating the return type.
        const PropagatingFlags = Self::HasRestParameter.bits() | Self::HasLiteralTypes.bits() | Self::Construct.bits() | Self::Abstract.bits() | Self::IsUntypedSignatureInJSFile.bits() | Self::IsSignatureCandidateForOverloadFailure.bits();
        const CallChainFlags = Self::IsInnerCallChain.bits() | Self::IsOuterCallChain.bits();
    }
}

// Signature

#[derive(Default)]
pub struct Signature {
    pub id: Cell<SignatureId>,
    pub flags: Cell<SignatureFlags>,
    pub(crate) min_argument_count: CountCell,
    pub(crate) resolved_min_argument_count: OptionalCountCell,
    pub(crate) declaration: Cell<Option<P<Node>>>,
    pub(crate) parameters: ThinSliceCell<P<Symbol>>,
    pub resolved_return_type: OvCell<Option<P<Type>>>,
    pub(crate) target: Cell<Option<P<Signature>>>,
    pub(crate) mapper: MapperCell,
    // Type parameters, `thisParameter`, `isolatedSignatureType`, `composite` and a resolved type predicate other
    // than the checker's `noTypePredicate` live in a tail allocated on the first non-nil write.
    // `this_parameter()` / `set_this_parameter()` & co. read nil when it is absent. `resolvedTypePredicate ==
    // c.noTypePredicate` (every resolved signature without a predicate) is bit 0 of the same word.
    pub rare: SignatureRareWord,
}

#[cfg(target_pointer_width = "64")]
const _: () =
    assert!(std::mem::size_of::<Signature>() == if tsrs_core::COMPRESSED_PTRS { 48 } else { 64 });
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<Signature>() == 44);

#[derive(Default)]
pub struct SignatureRare {
    type_parameters: OptionThinSliceCell<P<Type>>,
    this_parameter: Cell<Option<P<Symbol>>>,
    isolated_signature_type: OvCell<Option<P<Type>>>,
    composite: Cell<Option<P<CompositeSignature>>>,
    resolved_type_predicate: OvCell<Option<P<TypePredicate>>>, // never the checker's `noTypePredicate` (that is the bit)
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    std::mem::size_of::<SignatureRare>() == if tsrs_core::COMPRESSED_PTRS { 24 } else { 40 }
);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<SignatureRare>() == 24);

/// `Signature`'s tail pointer (8-aligned, null when absent) with the "no type predicate" bit in bit 0.
pub struct SignatureRareWord(OvExact<RawWord>);

const SIGNATURE_NO_TYPE_PREDICATE: usize = 1;

impl Default for SignatureRareWord {
    #[inline]
    fn default() -> Self {
        SignatureRareWord(OvExact::new(RawWord(0)))
    }
}

impl SignatureRareWord {
    #[inline]
    pub fn tail(&self) -> Option<P<SignatureRare>> {
        let p: *const SignatureRare =
            self.0.get().ptr::<SignatureRare>().map_addr(|a| a & !SIGNATURE_NO_TYPE_PREDICATE);
        // SAFETY: a non-null address is a `P<SignatureRare>` stored by `set_tail`.
        (!p.is_null()).then(|| unsafe { P::from_arena(&*p) })
    }
    #[inline]
    fn no_type_predicate(&self) -> bool {
        self.0.get().0 & SIGNATURE_NO_TYPE_PREDICATE != 0
    }
    #[inline]
    fn set_tail(&self, tail: P<SignatureRare>) {
        let bit = self.0.get().0 & SIGNATURE_NO_TYPE_PREDICATE;
        self.0.set(RawWord::from_ptr(
            std::ptr::from_ref::<SignatureRare>(tail.get()).map_addr(|a| a | bit),
        ));
    }
    #[inline]
    fn set_no_type_predicate(&self, on: bool) {
        let w = self.0.get().0;
        self.0.set(RawWord(if on {
            w | SIGNATURE_NO_TYPE_PREDICATE
        } else {
            w & !SIGNATURE_NO_TYPE_PREDICATE
        }));
    }
}

impl Signature {
    #[inline]
    pub fn id(&self) -> SignatureId {
        self.id.get()
    }
    #[inline]
    pub fn flags(&self) -> SignatureFlags {
        self.flags.get()
    }
    #[inline]
    pub fn type_parameters(&self) -> &'static [P<Type>] {
        self.type_parameters_option().unwrap_or(&[])
    }
    #[inline]
    pub fn type_parameters_option(&self) -> Option<&'static [P<Type>]> {
        self.rare.tail()?.type_parameters.get()
    }
    #[inline]
    pub fn set_type_parameters(&self, types: Option<&'static [P<Type>]>) {
        if types.is_some() || self.rare.tail().is_some() {
            self.rare_for_write().type_parameters.set(types);
        }
    }
    #[inline]
    pub fn declaration(&self) -> Option<P<Node>> {
        self.declaration.get()
    }
    #[inline]
    pub fn target(&self) -> Option<P<Signature>> {
        self.target.get()
    }
    #[inline]
    fn rare_for_write(&self) -> P<SignatureRare> {
        if let Some(rare) = self.rare.tail() {
            rare
        } else {
            let rare = P::new(SignatureRare::default());
            self.rare.set_tail(rare);
            rare
        }
    }
    /// Go `sig.resolvedTypePredicate`; `no_type_predicate` is the checker's `noTypePredicate`.
    #[inline]
    pub fn resolved_type_predicate(
        &self,
        no_type_predicate: P<TypePredicate>,
    ) -> Option<P<TypePredicate>> {
        if self.rare.no_type_predicate() {
            return Some(no_type_predicate);
        }
        self.rare.tail()?.resolved_type_predicate.get()
    }
    /// Go `sig.resolvedTypePredicate = predicate`; `no_type_predicate` is the checker's `noTypePredicate`.
    #[inline]
    pub fn set_resolved_type_predicate(
        &self,
        predicate: Option<P<TypePredicate>>,
        no_type_predicate: P<TypePredicate>,
    ) {
        let none = predicate == Some(no_type_predicate);
        self.rare.set_no_type_predicate(none);
        let stored = if none { None } else { predicate };
        if stored.is_some() || self.rare.tail().is_some() {
            self.rare_for_write().resolved_type_predicate.set(stored);
        }
    }
    #[inline]
    pub fn this_parameter(&self) -> Option<P<Symbol>> {
        self.rare.tail()?.this_parameter.get()
    }
    #[inline]
    pub fn set_this_parameter(&self, this_parameter: Option<P<Symbol>>) {
        if this_parameter.is_some() || self.rare.tail().is_some() {
            self.rare_for_write().this_parameter.set(this_parameter);
        }
    }
    #[inline]
    pub fn isolated_signature_type(&self) -> Option<P<Type>> {
        self.rare.tail()?.isolated_signature_type.get()
    }
    #[inline]
    pub fn set_isolated_signature_type(&self, t: Option<P<Type>>) {
        if t.is_some() || self.rare.tail().is_some() {
            self.rare_for_write().isolated_signature_type.set(t);
        }
    }
    #[inline]
    pub fn composite(&self) -> Option<P<CompositeSignature>> {
        self.rare.tail()?.composite.get()
    }
    #[inline]
    pub fn set_composite(&self, composite: Option<P<CompositeSignature>>) {
        if composite.is_some() || self.rare.tail().is_some() {
            self.rare_for_write().composite.set(composite);
        }
    }
    #[inline]
    pub fn parameters(&self) -> &'static [P<Symbol>] {
        self.parameters.get()
    }
    #[inline]
    pub fn has_rest_parameter(&self) -> bool {
        self.flags.get().intersects(SignatureFlags::HasRestParameter)
    }
    #[inline]
    pub fn min_argument_count(&self) -> usize {
        self.min_argument_count.get()
    }
}

#[derive(Default)]
pub struct CompositeSignature {
    pub(crate) is_union: Cell<bool>, // True for union, false for intersection
    pub(crate) signatures: Cell<&'static [P<Signature>]>, // Individual signatures
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum TypePredicateKind {
    #[default]
    This,
    Identifier,
    AssertsThis,
    AssertsIdentifier,
}

#[derive(Default)]
pub struct TypePredicate {
    pub(crate) kind: Cell<TypePredicateKind>,
    pub(crate) parameter_index: Cell<Option<usize>>,
    pub(crate) parameter_name: Cell<&'static str>,
    pub(crate) t: Cell<Option<P<Type>>>,
}

impl TypePredicate {
    #[inline]
    pub fn type_(&self) -> Option<P<Type>> {
        self.t.get()
    }
    #[inline]
    pub fn kind(&self) -> TypePredicateKind {
        self.kind.get()
    }
    #[inline]
    pub fn parameter_index(&self) -> Option<usize> {
        self.parameter_index.get()
    }
    #[inline]
    pub fn parameter_name(&self) -> &'static str {
        self.parameter_name.get()
    }
}

// IndexInfo

#[derive(Default)]
pub struct IndexInfo {
    pub(crate) key_type: Cell<Option<P<Type>>>,
    pub(crate) value_type: Cell<Option<P<Type>>>,
    pub(crate) is_readonly: Cell<bool>,
    pub(crate) declaration: Cell<Option<P<Node>>>, // IndexSignatureDeclaration
    pub(crate) index_symbol: Cell<Option<P<Symbol>>>, // Synthetic property symbol for this index signature
    pub(crate) components: Cell<&'static [P<Node>]>,  // ElementWithComputedPropertyName
}

impl IndexInfo {
    #[inline]
    pub fn key_type(&self) -> P<Type> {
        self.key_type.get().unwrap()
    }
    #[inline]
    pub fn value_type(&self) -> P<Type> {
        self.value_type.get().unwrap()
    }
    #[inline]
    pub fn is_readonly(&self) -> bool {
        self.is_readonly.get()
    }
    #[inline]
    pub fn declaration(&self) -> Option<P<Node>> {
        self.declaration.get()
    }
}

/// Ternary values are defined such that x & y picks the lesser in the order False < Unknown < Maybe < True,
/// and x | y picks the greater in the order False < Unknown < Maybe < True.
///
/// Generally, Ternary.Maybe is used as the result of a relation that depends on itself, and Ternary.Unknown
/// is used as the result of a variance check that depends on itself. We make a distinction because we don't
/// want to cache circular variance check results.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct Ternary(i8);

impl Ternary {
    pub(crate) const FALSE: Ternary = Ternary(0);
    pub(crate) const UNKNOWN: Ternary = Ternary(1);
    pub(crate) const MAYBE: Ternary = Ternary(3);
    pub const TRUE: Ternary = Ternary(-1);
}

impl std::ops::BitAnd for Ternary {
    type Output = Ternary;
    #[inline]
    fn bitand(self, rhs: Ternary) -> Ternary {
        Ternary(self.0 & rhs.0)
    }
}
impl std::ops::BitOr for Ternary {
    type Output = Ternary;
    #[inline]
    fn bitor(self, rhs: Ternary) -> Ternary {
        Ternary(self.0 | rhs.0)
    }
}
impl std::ops::BitAndAssign for Ternary {
    #[inline]
    fn bitand_assign(&mut self, rhs: Ternary) {
        self.0 &= rhs.0;
    }
}
impl std::ops::BitOrAssign for Ternary {
    #[inline]
    fn bitor_assign(&mut self, rhs: Ternary) {
        self.0 |= rhs.0;
    }
}

/// Go `type TypeComparer func(s *Type, t *Type, reportErrors bool) Ternary`. Stored in `InferenceContext` and the
/// checker, so it is a `Copy` handle; build one with `type_comparer(|c, s, t, report_errors| ...)`.
pub type TypeComparer = &'static dyn Fn(&mut Checker, P<Type>, P<Type>, bool) -> Ternary;

pub(crate) fn type_comparer(
    f: impl Fn(&mut Checker, P<Type>, P<Type>, bool) -> Ternary + 'static,
) -> TypeComparer {
    alloc(f)
}

#[derive(Clone, Copy, Debug)]
pub struct LanguageFeatureMinimumTargetMap {
    pub(crate) async_functions: ScriptTarget,
    pub(crate) for_await_of: ScriptTarget,
    pub(crate) async_generators: ScriptTarget,
    pub(crate) object_spread_rest: ScriptTarget,
    pub(crate) private_names_and_class_static_blocks: ScriptTarget,
    pub(crate) using_and_await_using: ScriptTarget,
    pub(crate) class_and_class_element_decorators: ScriptTarget,
}

pub(crate) static LANGUAGE_FEATURE_MINIMUM_TARGET: LanguageFeatureMinimumTargetMap =
    LanguageFeatureMinimumTargetMap {
        async_functions: ScriptTarget::ES2017,
        for_await_of: ScriptTarget::ES2018,
        async_generators: ScriptTarget::ES2018,
        object_spread_rest: ScriptTarget::ES2018,
        private_names_and_class_static_blocks: ScriptTarget::ES2022,
        using_and_await_using: ScriptTarget::ES_NEXT,
        class_and_class_element_decorators: ScriptTarget::ES_NEXT,
    };

// Aliases for types

// Shared graph: how a fork copies the frozen seed's link records (links.rs `LinkCopy`).
crate::bitwise_link_copy!(
    NodeLinks,
    SymbolNodeLinks,
    TypeNodeLinks,
    SignatureLinks,
    ComputedNameNodeLinks,
    EnumMemberLinks,
    AssertionLinks,
    SwitchStatementLinks,
    ArrayLiteralLinks,
    MappedSymbolLinks,
    DeferredSymbolLinks,
    AliasSymbolLinks,
    // `type_only_export_star_map` / `instantiations` point to frozen tables, which GoMap / GoPackedMap read through.
    ModuleSymbolLinks,
    TypeAliasLinks,
    ReverseMappedSymbolLinks,
    LateBoundLinks,
    ExportTypeLinks,
    MembersAndExportsLinks,
    DeclaredTypeLinks,
    SpreadLinks,
    VarianceLinks,
    MarkedAssignmentSymbolLinks,
    crate::jsx_types::JsxElementLinks,
);

impl crate::links::LinkCopy for ValueSymbolLinks {
    fn copy_link_from(&self, frozen: &Self) {
        self.resolved_type.set(frozen.resolved_type.get());
        self.first.set(frozen.first.get());
        self.second.set(frozen.second.get());
        if frozen.mode() == LinksMode::Tail {
            // The tail is a side object of the frozen record: this record gets its own.
            let src = frozen.tail();
            let tail = P::new(ValueSymbolLinksTail::default());
            tail.target.set(src.target.get());
            tail.mapper.set(src.mapper.get());
            tail.write_type.set(src.write_type.get());
            tail.name_type.set(src.name_type.get());
            tail.containing_type.set(src.containing_type.get());
            self.first.set(erase(Some(tail)));
        }
    }
}

impl crate::links::LinkCopy for ContainingSymbolLinks {
    #[inline]
    fn copy_link_from(&self, frozen: &Self) {
        // SAFETY: the frozen record is read-only; no borrow of it is live (frozen objects are never borrowed mutably).
        unsafe {
            self.extended_containers_by_file
                .borrow_mut()
                .clone_from(frozen.extended_containers_by_file.try_borrow_unguarded().unwrap());
            self.accessible_chain_cache
                .borrow_mut()
                .clone_from(frozen.accessible_chain_cache.try_borrow_unguarded().unwrap());
        }
        self.extended_containers.set(frozen.extended_containers.get());
    }
}

impl crate::links::LinkCopy for SourceFileLinks {
    fn copy_link_from(&self, frozen: &Self) {
        self.type_checked.set(frozen.type_checked.get());
        self.unused_checked.set(frozen.unused_checked.get());
        self.external_helpers_module.set(frozen.external_helpers_module.get());
        self.requested_external_emit_helpers.set(frozen.requested_external_emit_helpers.get());
        // SAFETY: as for ContainingSymbolLinks.
        unsafe {
            self.deferred_nodes
                .borrow_mut()
                .clone_from(frozen.deferred_nodes.try_borrow_unguarded().unwrap());
            self.identifier_check_nodes
                .borrow_mut()
                .clone_from(frozen.identifier_check_nodes.try_borrow_unguarded().unwrap());
        }
        self.local_jsx_namespace.set(frozen.local_jsx_namespace.get());
        self.local_jsx_fragment_namespace.set(frozen.local_jsx_fragment_namespace.get());
        self.local_jsx_factory.set(frozen.local_jsx_factory.get());
        self.local_jsx_fragment_factory.set(frozen.local_jsx_fragment_factory.get());
        self.jsx_fragment_type.set(frozen.jsx_fragment_type.get());
    }
}

/// Go `getSymbolTable(&cell)` for an overlay cell. Shared graph: a frozen table is copied to the checker's
/// own arena before it is written (the copy replaces it in the overlay).
fn ov_symbol_table(cell: &OvExact<Option<P<SymbolTable>>>) -> P<SymbolTable> {
    if let Some(table) = cell.get() {
        if !tsrs_core::sharedgraph::frozen(table.get()) {
            return table;
        }
        let copy = table.clone_table();
        cell.set(Some(copy));
        return copy;
    }
    let table = P::new(SymbolTable::default());
    cell.set(Some(table));
    table
}
