use crate::*;

// TypeMapperKind

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum TypeMapperKind {
    #[default]
    Unknown,
    Simple,
    Array,
    Merged,
}

// TypeMapper

/// Go's `TypeMapper` + `TypeMapperData` implementations. Mappers that Go implements with a stored checker or captured
/// closures receive the checker as a parameter of `map` instead.
///
/// Representation (16 bytes; there are tens of millions of mappers): two words. The low three bits of `first`
/// hold the kind (`TAG_*`); its other bits hold the first payload: an arena object's `P::to_bits` (8-aligned), or a
/// list's address shifted left by one (lists of `P<Type>` are 4-aligned). The array kinds keep their list lengths in
/// the top 16 bits of the two list words (when the addresses are below 2^47 and the lists in `u16`; otherwise the
/// mapper is out of line). The rare kinds (functions, deferred mappers, long lists) point
/// to a `RareTypeMapper`. `data()` decodes a mapper into the `TypeMapperData` view that the code matches on.
pub struct TypeMapper {
    first: MapperWord,
    // Also holds the escaped bit (`ESCAPED`, notes/mem-recycle.md) in bit 2, which every payload leaves free.
    second: std::cell::Cell<MapperWord>,
}

#[cfg(target_pointer_width = "64")]
type MapperWord = usize;
/// 32-bit targets (wasm32): the words are `u64` (the mapper stays 16 bytes) and the kind tags and `ESCAPED` live
/// above the address bits, since arena objects there are only 4-aligned. Addresses are below 2^32, so a list's
/// address shifted left by one uses bits 0-32, the tags bits 40-43 and the list lengths bits 48-63.
#[cfg(target_pointer_width = "32")]
type MapperWord = u64;

#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::size_of::<TypeMapper>() == 16);
// The payload pointers leave the tag bits free.
#[cfg(target_pointer_width = "64")]
const _: () = assert!(std::mem::align_of::<Type>() >= 8 && std::mem::align_of::<TypeMapper>() >= 8);
#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    std::mem::align_of::<InferenceContext>() >= 8 && std::mem::align_of::<RareTypeMapper>() >= 8
);
#[cfg(target_pointer_width = "32")]
const _: () = {
    assert!(std::mem::size_of::<TypeMapper>() == 16);
    // A 32-bit address shifted left by one, the tags (with `ESCAPED`) and the list length do not overlap.
    assert!(ADDR_MASK == (((u32::MAX as u64) << 1) | 1));
    assert!(ADDR_MASK & TAG_MASK == 0 && ESCAPED & TAG_MASK == ESCAPED);
    assert!(
        TAG_MASK >> LEN_SHIFT == 0
            && (u16::MAX as u64) << LEN_SHIFT >> LEN_SHIFT == u16::MAX as u64
    );
    assert!(TAG_RARE & !TAG_MASK == 0 && TAG_RARE & ESCAPED == 0);
};

#[cfg(target_pointer_width = "64")]
const TAG_MASK: usize = 0b111;
#[cfg(target_pointer_width = "64")]
const TAG_SIMPLE: usize = 0;
#[cfg(target_pointer_width = "64")]
const TAG_MERGED: usize = 1;
#[cfg(target_pointer_width = "64")]
const TAG_COMPOSITE: usize = 2;
#[cfg(target_pointer_width = "64")]
const TAG_INFERENCE: usize = 3;
#[cfg(target_pointer_width = "64")]
const TAG_ARRAY: usize = 4;
#[cfg(target_pointer_width = "64")]
const TAG_ARRAY_TO_SINGLE: usize = 5;
#[cfg(target_pointer_width = "64")]
const TAG_RARE: usize = 6;
const LEN_SHIFT: u32 = 48;
/// Set on a mapper that may be used after the call that created it returns: one stored in a type, signature, symbol
/// link, lazy member table, node builder context or inference context, or reachable from such a mapper (children of
/// merged / composite mappers, the context of an inference mapper). Mappers without it are dead once their creator
/// is done with them and may be recycled there.
#[cfg(target_pointer_width = "64")]
const ESCAPED: usize = 0b100;
#[cfg(target_pointer_width = "64")]
const ADDR_MASK: usize = ((1 << LEN_SHIFT) - 1) & !TAG_MASK;

#[cfg(target_pointer_width = "32")]
const TAG_SHIFT: u32 = 40;
/// Includes `ESCAPED`, as the 64-bit mask does, so `untagged` drops it.
#[cfg(target_pointer_width = "32")]
const TAG_MASK: u64 = 0b1111 << TAG_SHIFT;
#[cfg(target_pointer_width = "32")]
const TAG_SIMPLE: u64 = 0;
#[cfg(target_pointer_width = "32")]
const TAG_MERGED: u64 = 1 << TAG_SHIFT;
#[cfg(target_pointer_width = "32")]
const TAG_COMPOSITE: u64 = 2 << TAG_SHIFT;
#[cfg(target_pointer_width = "32")]
const TAG_INFERENCE: u64 = 3 << TAG_SHIFT;
#[cfg(target_pointer_width = "32")]
const TAG_ARRAY: u64 = 4 << TAG_SHIFT;
#[cfg(target_pointer_width = "32")]
const TAG_ARRAY_TO_SINGLE: u64 = 5 << TAG_SHIFT;
#[cfg(target_pointer_width = "32")]
const TAG_RARE: u64 = 6 << TAG_SHIFT;
#[cfg(target_pointer_width = "32")]
const ESCAPED: u64 = 0b1000 << TAG_SHIFT;
#[cfg(target_pointer_width = "32")]
const ADDR_MASK: u64 = (1 << 33) - 1;

/// A decoded mapper (the Go mapper data structs).
#[derive(Clone, Copy)]
pub enum TypeMapperData {
    Simple { source: P<Type>, target: P<Type> },
    Array { sources: &'static [P<Type>], targets: &'static [P<Type>] },
    ArrayToSingle { sources: &'static [P<Type>], target: P<Type> },
    Deferred { data: &'static DeferredTypeMapper },
    Function { f: fn(&mut Checker, P<Type>) -> P<Type> },
    Merged { m1: P<TypeMapper>, m2: P<TypeMapper> },
    Composite { m1: P<TypeMapper>, m2: P<TypeMapper> },
    Inference { n: P<InferenceContext>, fixing: bool },
}

/// The mapper kinds that are rare enough to live out of line.
enum RareTypeMapper {
    Array { sources: &'static [P<Type>], targets: &'static [P<Type>] },
    ArrayToSingle { sources: &'static [P<Type>], target: P<Type> },
    Deferred(DeferredTypeMapper),
    Function(fn(&mut Checker, P<Type>) -> P<Type>),
}

pub struct DeferredTypeMapper {
    sources: &'static [P<Type>],
    targets: Vec<Box<dyn Fn(&mut Checker) -> P<Type>>>,
}

impl TypeMapperData {
    /// The sources and targets of an array mapper (Go `ArrayMapper`).
    #[inline]
    pub(crate) fn array_sources_targets(&self) -> Option<(&'static [P<Type>], &'static [P<Type>])> {
        match *self {
            TypeMapperData::Array { sources, targets } => Some((sources, targets)),
            _ => None,
        }
    }
}

/// A list's address shifted left by one with its length in the top 16 bits, or `None` when either does not fit.
#[cfg(target_pointer_width = "64")]
#[inline]
#[expect(
    clippy::verbose_bit_mask,
    reason = "the mask directly documents the two tag bits reserved in the packed address"
)]
fn pack_slice(s: &'static [P<Type>]) -> Option<usize> {
    let a = s.as_ptr().expose_provenance();
    (a & 3 == 0 && a >> (LEN_SHIFT - 1) == 0 && u16::try_from(s.len()).is_ok())
        .then_some((a << 1) | (s.len() << LEN_SHIFT))
}

/// # Safety
/// `w` must come from `pack_slice` (tag bits may have been added).
#[cfg(target_pointer_width = "64")]
#[inline]
unsafe fn unpack_slice(w: usize) -> &'static [P<Type>] {
    // SAFETY: `w` came from `pack_slice` of a `&'static [P<Type>]` (this function's contract): the exposed data
    // address and the length, which tag bits do not overlap.
    unsafe {
        std::slice::from_raw_parts(
            std::ptr::with_exposed_provenance::<P<Type>>((w & ADDR_MASK) >> 1),
            w >> LEN_SHIFT,
        )
    }
}

#[cfg(target_pointer_width = "64")]
#[inline]
fn tagged<T>(p: P<T>, tag: usize) -> usize {
    p.to_bits() | tag
}

/// # Safety
/// `w` must hold a `P<T>`'s bits (plus tag bits).
#[cfg(target_pointer_width = "64")]
#[inline]
unsafe fn untagged<T>(w: usize) -> P<T> {
    // SAFETY: `w` holds a `P<T>`'s bits plus tag bits (this function's contract); the tags are masked off.
    unsafe { P::from_bits(w & !TAG_MASK) }
}

/// 32-bit twin of `pack_slice`: every 4-aligned address fits (see `MapperWord`).
#[cfg(target_pointer_width = "32")]
#[inline]
#[expect(
    clippy::verbose_bit_mask,
    reason = "the mask directly documents the two tag bits reserved in the packed address"
)]
fn pack_slice(s: &'static [P<Type>]) -> Option<u64> {
    let a = s.as_ptr().expose_provenance();
    (a & 3 == 0 && u16::try_from(s.len()).is_ok())
        .then(|| (u64::from(a as u32) << 1) | ((s.len() as u64) << LEN_SHIFT))
}

/// # Safety
/// `w` must come from `pack_slice` (tag bits may have been added).
#[cfg(target_pointer_width = "32")]
#[inline]
unsafe fn unpack_slice(w: u64) -> &'static [P<Type>] {
    let addr = ((w & ADDR_MASK) >> 1) as usize;
    let len = (w >> LEN_SHIFT) as usize;
    // SAFETY: `w` came from `pack_slice` of a `&'static [P<Type>]` (this function's contract): the exposed data
    // address and the length, which tag bits do not overlap.
    unsafe { std::slice::from_raw_parts(std::ptr::with_exposed_provenance::<P<Type>>(addr), len) }
}

#[cfg(target_pointer_width = "32")]
#[inline]
fn tagged<T>(p: P<T>, tag: u64) -> u64 {
    p.to_bits() as u64 | tag
}

/// # Safety
/// `w` must hold a `P<T>`'s bits (plus tag bits).
#[cfg(target_pointer_width = "32")]
#[inline]
unsafe fn untagged<T>(w: u64) -> P<T> {
    // SAFETY: `w` holds a `P<T>`'s bits (a 32-bit address) plus tag bits (this function's contract); the tags are
    // masked off.
    unsafe { P::from_bits((w & !TAG_MASK) as usize) }
}

impl TypeMapper {
    fn alloc_rare(rare: RareTypeMapper) -> P<TypeMapper> {
        P::new_recycled(TypeMapper {
            first: tagged(P::new(rare), TAG_RARE),
            second: std::cell::Cell::new(0),
        })
    }

    /// The mapper's kind and payload.
    #[inline]
    pub fn data(&self) -> TypeMapperData {
        let second = self.second.get();
        // SAFETY: the words were encoded for this tag by the constructors below.
        unsafe {
            match self.first & TAG_MASK {
                TAG_SIMPLE => TypeMapperData::Simple {
                    source: untagged(self.first),
                    target: untagged(second),
                },
                TAG_MERGED => {
                    TypeMapperData::Merged { m1: untagged(self.first), m2: untagged(second) }
                }
                TAG_COMPOSITE => {
                    TypeMapperData::Composite { m1: untagged(self.first), m2: untagged(second) }
                }
                TAG_INFERENCE => {
                    TypeMapperData::Inference { n: untagged(self.first), fixing: second & 1 != 0 }
                }
                TAG_ARRAY => TypeMapperData::Array {
                    sources: unpack_slice(self.first),
                    targets: unpack_slice(second),
                },
                TAG_ARRAY_TO_SINGLE => TypeMapperData::ArrayToSingle {
                    sources: unpack_slice(self.first),
                    target: untagged(second),
                },
                _ => match untagged::<RareTypeMapper>(self.first).get() {
                    RareTypeMapper::Array { sources, targets } => {
                        TypeMapperData::Array { sources, targets }
                    }
                    RareTypeMapper::ArrayToSingle { sources, target } => {
                        TypeMapperData::ArrayToSingle { sources, target: *target }
                    }
                    RareTypeMapper::Deferred(data) => TypeMapperData::Deferred { data },
                    RareTypeMapper::Function(f) => TypeMapperData::Function { f: *f },
                },
            }
        }
    }
}

impl TypeMapper {
    #[inline]
    pub fn escaped(&self) -> bool {
        self.second.get() & ESCAPED != 0
    }

    #[inline]
    fn set_escaped(&self) {
        self.second.set(self.second.get() | ESCAPED);
    }

    pub(crate) fn map(&self, c: &mut Checker, t: P<Type>) -> P<Type> {
        match self.data() {
            TypeMapperData::Simple { source, target } => {
                if t == source {
                    target
                } else {
                    t
                }
            }
            TypeMapperData::Array { sources, targets } => {
                if let Some(result) = sources
                    .iter()
                    .enumerate()
                    .find_map(|(i, s)| if t == *s { Some(targets[i]) } else { None })
                {
                    return result;
                }
                t
            }
            TypeMapperData::ArrayToSingle { sources, target } => {
                if sources.contains(&t) {
                    target
                } else {
                    t
                }
            }
            TypeMapperData::Deferred { data } => {
                if let Some(result) = data
                    .sources
                    .iter()
                    .enumerate()
                    .find_map(|(i, s)| if t == *s { Some(data.targets[i](c)) } else { None })
                {
                    return result;
                }
                t
            }
            TypeMapperData::Function { f } => f(c, t),
            TypeMapperData::Merged { m1, m2 } => {
                let t1 = m1.map(c, t);
                m2.map(c, t1)
            }
            TypeMapperData::Composite { m1, m2 } => {
                let t1 = m1.map(c, t);
                if t1 != t {
                    return c.instantiate_type(t1, Some(m2));
                }
                m2.map(c, t)
            }
            TypeMapperData::Inference { n, fixing } => {
                let inferences = n.inferences.get();
                if let Some((i, inference)) = inferences
                    .iter()
                    .enumerate()
                    .find(|(_, inference)| Some(t) == inference.type_parameter.get())
                {
                    if fixing && !inference.is_fixed.get() {
                        // Before committing to an inference, infer from the collected intra-expression sites.
                        c.infer_from_intra_expression_sites(n);
                        clear_cached_inferences(n.inferences.get());
                        inference.is_fixed.set(true);
                    }
                    return c.get_inferred_type(n, i);
                }
                t
            }
        }
    }

    pub fn kind(&self) -> TypeMapperKind {
        match self.data() {
            TypeMapperData::Simple { .. } => TypeMapperKind::Simple,
            TypeMapperData::Array { .. } => TypeMapperKind::Array,
            TypeMapperData::Merged { .. } => TypeMapperKind::Merged,
            _ => TypeMapperKind::Unknown,
        }
    }

    pub fn maps_this_only(&self) -> bool {
        match self.data() {
            TypeMapperData::Simple { source, .. } => is_this_type_parameter(source),
            TypeMapperData::Array { sources, .. }
            | TypeMapperData::ArrayToSingle { sources, .. } => {
                sources.len() == 1 && is_this_type_parameter(sources[0])
            }
            TypeMapperData::Deferred { data: DeferredTypeMapper { sources, .. } } => {
                sources.len() == 1 && is_this_type_parameter(sources[0])
            }
            _ => false,
        }
    }
}

/// Marks `m` escaped, with everything it references that could be recycled (see `ESCAPED`). Invariant: an escaped
/// mapper's children are escaped, so the walk stops at the first escaped mapper.
pub fn escape_mapper(m: P<TypeMapper>) {
    // A mapper of the frozen shared-graph seed is never recycled by a fork (it is not the fork's to free), so it counts
    // as escaped; its escape bit cannot be written.
    if m.escaped() || tsrs_core::sharedgraph::frozen(&*m) {
        return;
    }
    m.set_escaped();
    match m.data() {
        TypeMapperData::Merged { m1, m2 } | TypeMapperData::Composite { m1, m2 } => {
            escape_mapper(m1);
            escape_mapper(m2);
        }
        TypeMapperData::Inference { n, .. } => n.escape(),
        _ => {}
    }
}

/// Gives a mapper its creator made back to the arena unless it escaped (with its rare payload's lists left alone).
///
/// # Safety
/// The caller created `m` and is done with it; mappers that did not escape are referenced only by locals of finished
/// calls, the active-mapper stack (popped by then) and other mappers that did not escape.
pub(crate) unsafe fn recycle_mapper(m: P<TypeMapper>) {
    if !m.escaped() && !tsrs_core::sharedgraph::frozen(&*m) {
        tsrs_core::free!(m);
    }
}

/// Recycles the one or two mappers `append_type_mapping` (`appended`) / `prepend_type_mapping` made: the simple
/// mapper and, when a mapper was extended, the merged one (not the extended mapper itself).
///
/// # Safety
/// As for `recycle_mapper`: the caller made `m` with that function and is done with it.
pub(crate) unsafe fn recycle_mapping(m: P<TypeMapper>, appended: bool) {
    if m.escaped() {
        return; // its children are escaped too
    }
    if let TypeMapperData::Merged { m1, m2 } = m.data() {
        let simple = if appended { m2 } else { m1 };
        tsrs_core::free!(m);
        // SAFETY: the caller made the simple mapper too (this function's contract), and the merged mapper that held
        // it was just freed.
        unsafe { recycle_mapper(simple) };
    } else {
        tsrs_core::free!(m);
    }
}

/// Recycles a mapper made by `new_type_mapper(sources, targets)` whose creator also made `targets` (with
/// `alloc_slice_recycled`), with that list: an array mapper is its list's only holder, a simple mapper does not
/// refer to it (`getConditionalTypeInstantiation` does the same inline).
///
/// `targets` is a raw slice, not a `&[P<Type>]`: a reference parameter is a protected borrow for the whole call, and
/// freeing the memory it points to inside the call let LLVM drop the free-list link (`tsrs_core::free_raw`).
///
/// # Safety
/// As for `recycle_mapper`; `targets` is the list the caller made for `m`.
pub(crate) unsafe fn recycle_mapper_with_targets(m: P<TypeMapper>, targets: *const [P<Type>]) {
    if !m.escaped() {
        tsrs_core::free!(m);
        // SAFETY: the list the caller made for `m` (this function's contract); `m`, its only holder, is gone.
        unsafe { tsrs_core::free_slice_ptr(targets) };
    } else if targets.len() == 1 {
        // SAFETY: a one-type list is not kept by the simple mapper made from it.
        unsafe { tsrs_core::free_slice_ptr(targets) };
    }
}

/// A stored mapper: `Cell<Option<P<TypeMapper>>>` whose `new` / `set` mark the mapper escaped (`escape_mapper`).
#[derive(Default)]
#[repr(transparent)]
pub struct MapperCell(Cell<Option<P<TypeMapper>>>);

impl MapperCell {
    #[inline]
    pub(crate) fn new(m: Option<P<TypeMapper>>) -> MapperCell {
        if let Some(m) = m {
            escape_mapper(m);
        }
        MapperCell(Cell::new(m))
    }
    #[inline]
    pub(crate) fn get(&self) -> Option<P<TypeMapper>> {
        self.0.get()
    }
    #[inline]
    pub(crate) fn set(&self, m: Option<P<TypeMapper>>) {
        if let Some(m) = m {
            escape_mapper(m);
        }
        self.0.set(m);
    }
}

// Factory functions

impl Checker {
    /// Go free function `getMappedType(t, mapper)`; a method because mapping may need the checker.
    pub(crate) fn get_mapped_type(&mut self, t: P<Type>, mapper: P<TypeMapper>) -> P<Type> {
        mapper.map(self, get_non_distributed_type_parameter(t).unwrap())
    }

    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn combine_type_mappers(
        m1: Option<P<TypeMapper>>,
        m2: P<TypeMapper>,
    ) -> P<TypeMapper> {
        if let Some(m1) = m1 {
            return new_composite_type_mapper(m1, m2);
        }
        m2
    }

    pub(crate) fn map_type_with_composite_mapper(
        &mut self,
        t: P<Type>,
        m1: Option<P<TypeMapper>>,
        m2: P<TypeMapper>,
    ) -> P<Type> {
        let Some(m1) = m1 else {
            return self.get_mapped_type(t, m2);
        };
        let t1 = self.get_mapped_type(t, m1);
        if t1 != t {
            return self.instantiate_type(t1, Some(m2));
        }
        self.get_mapped_type(t, m2)
    }

    /// Maps forward-references to later types parameters to the empty object type.
    /// This is used during inference when instantiating type parameter defaults.
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn new_backreference_mapper(
        &self,
        context: P<InferenceContext>,
        index: usize,
    ) -> P<TypeMapper> {
        let forward_inferences = &context.inferences.get()[index..];
        let type_parameters: Vec<P<Type>> =
            forward_inferences.iter().map(|i| i.type_parameter.get().unwrap()).collect();
        new_array_to_single_type_mapper(
            tsrs_core::alloc_slice_recycled(&type_parameters),
            self.unknown_type,
        )
    }
}

#[cfg_attr(feature = "site-counts", track_caller)]
pub(crate) fn new_inference_type_mapper(n: P<InferenceContext>, fixing: bool) -> P<TypeMapper> {
    crate::sitecount::hit("mapper", "inference");
    P::new_recycled(TypeMapper {
        first: tagged(n, TAG_INFERENCE),
        second: std::cell::Cell::new(MapperWord::from(fixing)),
    })
}

#[cfg_attr(feature = "site-counts", track_caller)]
pub(crate) fn new_type_mapper(
    sources: &'static [P<Type>],
    targets: &'static [P<Type>],
) -> P<TypeMapper> {
    if sources.len() == 1 {
        return new_simple_type_mapper(sources[0], targets[0]);
    }
    new_array_type_mapper(sources, targets)
}

#[cfg_attr(feature = "site-counts", track_caller)]
pub(crate) fn merge_type_mappers(m1: Option<P<TypeMapper>>, m2: P<TypeMapper>) -> P<TypeMapper> {
    if let Some(m1) = m1 {
        return new_merged_type_mapper(m1, m2);
    }
    m2
}

#[cfg_attr(feature = "site-counts", track_caller)]
pub(crate) fn prepend_type_mapping(
    source: P<Type>,
    target: P<Type>,
    mapper: Option<P<TypeMapper>>,
) -> P<TypeMapper> {
    let Some(mapper) = mapper else {
        return new_simple_type_mapper(get_non_distributed_type_parameter(source).unwrap(), target);
    };
    new_merged_type_mapper(
        new_simple_type_mapper(get_non_distributed_type_parameter(source).unwrap(), target),
        mapper,
    )
}

#[cfg_attr(feature = "site-counts", track_caller)]
pub(crate) fn append_type_mapping(
    mapper: Option<P<TypeMapper>>,
    source: P<Type>,
    target: P<Type>,
) -> P<TypeMapper> {
    let Some(mapper) = mapper else {
        return new_simple_type_mapper(get_non_distributed_type_parameter(source).unwrap(), target);
    };
    new_merged_type_mapper(
        mapper,
        new_simple_type_mapper(get_non_distributed_type_parameter(source).unwrap(), target),
    )
}

#[cfg_attr(feature = "site-counts", track_caller)]
pub fn new_simple_type_mapper(source: P<Type>, target: P<Type>) -> P<TypeMapper> {
    crate::sitecount::hit("mapper", "simple");
    P::new_recycled(TypeMapper {
        first: tagged(source, TAG_SIMPLE),
        second: std::cell::Cell::new(tagged(target, 0)),
    })
}

#[cfg_attr(feature = "site-counts", track_caller)]
pub fn new_array_type_mapper(
    sources: &'static [P<Type>],
    targets: &'static [P<Type>],
) -> P<TypeMapper> {
    crate::sitecount::hit("mapper", "array");
    match (pack_slice(sources), pack_slice(targets)) {
        (Some(first), Some(second)) => P::new_recycled(TypeMapper {
            first: first | TAG_ARRAY,
            second: std::cell::Cell::new(second),
        }),
        _ => TypeMapper::alloc_rare(RareTypeMapper::Array { sources, targets }),
    }
}

#[cfg_attr(feature = "site-counts", track_caller)]
pub fn new_array_to_single_type_mapper(
    sources: &'static [P<Type>],
    target: P<Type>,
) -> P<TypeMapper> {
    crate::sitecount::hit("mapper", "array_to_single");
    match pack_slice(sources) {
        Some(first) => P::new_recycled(TypeMapper {
            first: first | TAG_ARRAY_TO_SINGLE,
            second: std::cell::Cell::new(tagged(target, 0)),
        }),
        None => TypeMapper::alloc_rare(RareTypeMapper::ArrayToSingle { sources, target }),
    }
}

#[cfg_attr(feature = "site-counts", track_caller)]
pub fn new_deferred_type_mapper(
    sources: &'static [P<Type>],
    targets: Vec<Box<dyn Fn(&mut Checker) -> P<Type>>>,
) -> P<TypeMapper> {
    crate::sitecount::hit("mapper", "deferred");
    TypeMapper::alloc_rare(RareTypeMapper::Deferred(DeferredTypeMapper { sources, targets }))
}

#[cfg_attr(feature = "site-counts", track_caller)]
pub fn new_function_type_mapper(f: fn(&mut Checker, P<Type>) -> P<Type>) -> P<TypeMapper> {
    crate::sitecount::hit("mapper", "function");
    TypeMapper::alloc_rare(RareTypeMapper::Function(f))
}

#[cfg_attr(feature = "site-counts", track_caller)]
pub fn new_merged_type_mapper(m1: P<TypeMapper>, m2: P<TypeMapper>) -> P<TypeMapper> {
    crate::sitecount::hit("mapper", "merged");
    P::new_recycled(TypeMapper {
        first: tagged(m1, TAG_MERGED),
        second: std::cell::Cell::new(tagged(m2, 0)),
    })
}

#[cfg_attr(feature = "site-counts", track_caller)]
pub fn new_composite_type_mapper(m1: P<TypeMapper>, m2: P<TypeMapper>) -> P<TypeMapper> {
    crate::sitecount::hit("mapper", "composite");
    P::new_recycled(TypeMapper {
        first: tagged(m1, TAG_COMPOSITE),
        second: std::cell::Cell::new(tagged(m2, 0)),
    })
}
