use crate::*;
use tsrs_core::{PKey, PSlot};

/// Go `core.LinkStore` keyed by node or symbol identity.
///
/// Most of the checker's link stores use this type; `SymbolReferenceLinkStore`, `NodeLinkStore`,
/// `NodeValueLinkStore` (both Go `nodeLinkStore`) and `SymbolArenaLinkStore` (Go `symbolLinkStore`)
/// below cover the rest. (The node builder's link stores use `tsrs_core::LinkStore`.) Values live in the arena, so `get`
/// hands out a `Copy` pointer whose `Cell` fields are mutated in place.
///
/// Keyed by the key's identity like Go's `map[K]*V`. A slot is the key (`P::key`: its handle with compressed
/// pointers, else its address) and the value's index (8 or 12 bytes, 4-aligned) instead of two pointers, and
/// the values live in fixed-size arena chunks in first-access order (stable addresses, as before). 5.1M links
/// in 26 stores on the private monorepo single.
///
/// Stable addresses: a chunk is never reallocated, freed or recycled, so a `P<V>` handed out stays valid for
/// the store's lifetime. Callers rely on it: they keep links across accesses that create others, and
/// `run_without_resolved_signature_caching` keeps them in a vector across a nested check.
pub struct LinkStore<K: 'static, V: 'static> {
    slots: hashbrown::HashTable<LinkSlot>,
    chunks: Vec<P<PSlot<V>>>, // first slot of each chunk
    len: u32,
    key: std::marker::PhantomData<P<K>>,
    /// Shared graph: the frozen seed's store, read through; a record is copied on first `get`.
    parent: Option<&'static LinkStore<K, V>>,
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct LinkSlot {
    key: PKey,
    index: u32, // the value's position in `chunks`
}

#[cfg(target_pointer_width = "64")]
const _: () =
    assert!(std::mem::size_of::<LinkSlot>() == if tsrs_core::COMPRESSED_PTRS { 8 } else { 12 });
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<LinkSlot>() == 8);

// Bound unused tails in short stores while keeping every allocated value at a stable address.
// Measurements: notes/data-structure-redesigns.md.
const LINK_CHUNK_SHIFT: u32 = 6;
const LINK_CHUNK: usize = 1 << LINK_CHUNK_SHIFT;

impl<K: 'static, V: 'static> Default for LinkStore<K, V> {
    fn default() -> Self {
        LinkStore {
            slots: hashbrown::HashTable::new(),
            chunks: Vec::new(),
            len: 0,
            key: std::marker::PhantomData,
            parent: None,
        }
    }
}

impl<K: 'static, V: 'static> LinkStore<K, V> {
    #[inline]
    fn hash(key: PKey) -> u64 {
        use std::hash::BuildHasher;
        rustc_hash::FxBuildHasher.hash_one(key)
    }

    #[inline]
    fn address(key: P<K>) -> PKey {
        key.key()
    }

    #[inline]
    fn at(&self, index: u32) -> P<V> {
        debug_assert!(index < self.len);
        #[expect(
            clippy::disallowed_methods,
            reason = "bounds checks on the three link-store lookups: +0.6% instructions, four checkers (notes/lint-paydown-compiler.md)"
        )]
        // SAFETY: every stored index is below `len`, and `chunks` holds `LINK_CHUNK` values per started chunk.
        let chunk = unsafe { *self.chunks.get_unchecked((index >> LINK_CHUNK_SHIFT) as usize) };
        // SAFETY: `chunk` is the first slot of a `LINK_CHUNK`-slot array, and the offset is below `LINK_CHUNK`.
        unsafe { PSlot::nth(chunk, index as usize & (LINK_CHUNK - 1)) }
    }

    #[inline]
    fn index(&self, key: P<K>) -> Option<u32> {
        self.index_by_pkey(Self::address(key))
    }

    #[inline]
    fn index_by_pkey(&self, key: PKey) -> Option<u32> {
        self.slots.find(Self::hash(key), |slot| ({ slot.key }) == key).map(|slot| slot.index)
    }

    #[inline]
    pub(crate) fn try_get(&self, key: P<K>) -> Option<P<V>> {
        match self.index(key) {
            Some(index) => Some(self.at(index)),
            None => self.parent.filter(|_| tsrs_core::sharedgraph::COMPILED_IN)?.try_get(key),
        }
    }

    #[inline]
    pub(crate) fn has(&self, key: P<K>) -> bool {
        self.index(key).is_some()
            || self
                .parent
                .filter(|_| tsrs_core::sharedgraph::COMPILED_IN)
                .is_some_and(|p| p.has(key))
    }

    /// Shared graph: an empty store that reads through to `self` (frozen).
    #[must_use]
    pub(crate) fn fork(&'static self) -> Self {
        LinkStore {
            slots: hashbrown::HashTable::new(),
            chunks: Vec::new(),
            len: 0,
            key: std::marker::PhantomData,
            parent: Some(self),
        }
    }
}

impl<K: 'static, V: Default + LinkCopy + 'static> LinkStore<K, V> {
    /// Returns the links for `key`, creating them on first use. A hit is a probe and no call (so no saved
    /// registers where this is inlined); the first access probes again in `create`.
    #[inline]
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub fn get(&mut self, key: impl Into<Option<P<K>>>) -> P<V> {
        // Go pointer keys may be nil; zero is never an arena handle or address.
        let key = key.into().map_or(0, Self::address);
        match self.index_by_pkey(key) {
            Some(index) => self.at(index),
            None => self.create(key),
        }
    }

    /// `get` of a key without links.
    #[cfg_attr(feature = "site-counts", track_caller)]
    #[inline(never)]
    fn create(&mut self, key: PKey) -> P<V> {
        crate::sitecount::hit("links", std::any::type_name::<V>());
        let index = self.len;
        self.slots
            .insert_unique(Self::hash(key), LinkSlot { key, index }, |slot| Self::hash(slot.key));
        if (index as usize).is_multiple_of(LINK_CHUNK) {
            self.chunks.push(PSlot::first(alloc_vec(
                std::iter::repeat_with(|| PSlot(V::default())).take(LINK_CHUNK).collect(),
            )));
        }
        self.len += 1;
        if let Some(frozen) = self
            .parent
            .filter(|_| tsrs_core::sharedgraph::COMPILED_IN)
            .and_then(|p| p.index_by_pkey(key).map(|i| p.at(i)))
        {
            self.at(index).copy_link_from(&frozen);
        }
        self.at(index)
    }
}

/// Shared graph: how a fork copies a link record of the frozen seed into its own store on first access.
///
/// Records made only of `Cell`s copy bitwise (`bitwise_link_copy!`); records that own or point to mutable
/// side objects copy those too.
pub trait LinkCopy {
    fn copy_link_from(&self, frozen: &Self);
}

/// Bitwise copy of a record of `Cell`s.
///
/// # Safety
/// `V` holds only `Cell`s of `Copy` values with no pointer to a mutable side object of its own.
pub unsafe fn copy_cells_bitwise<V>(dst: &V, src: &V) {
    const { assert!(!std::mem::needs_drop::<V>()) };
    // SAFETY: the caller's contract: every byte of `V` is inside a `Cell`, so writing through `dst` is a `Cell` write;
    // `src` and `dst` are distinct records (the frozen seed's and this checker's).
    unsafe {
        std::ptr::copy_nonoverlapping(
            std::ptr::from_ref(src).cast::<u8>(),
            std::ptr::from_ref(dst).cast::<u8>().cast_mut(),
            std::mem::size_of::<V>(),
        );
    };
}

#[macro_export]
macro_rules! bitwise_link_copy {
    ($($t:ty),* $(,)?) => {
        $(impl $crate::links::LinkCopy for $t {
            #[inline]
            fn copy_link_from(&self, frozen: &Self) {
                // SAFETY: a record of `Cell`s of `Copy` values (listed by hand in links.rs).
                unsafe { $crate::links::copy_cells_bitwise(self, frozen) }
            }
        })*
    };
}

bitwise_link_copy!(Cell<u64>, Cell<u32>);

/// A link value that holds its own key, for `KeyedLinkStore`.
pub trait KeyedLinks {
    /// The key the store filed the value under (`P::key`); 0 until then. Only the store reads or writes it.
    fn link_key(&self) -> &Cell<PKey>;
}

/// `LinkStore` for values with room for their key: a bucket holds only the value's index (4 bytes plus
/// hashbrown's control byte, against 8 + 1), and a probe compares the key the value holds.
///
/// Before upstream switched their Go stores to paged node indexes, `signature_links` and
/// `type_node_links` used this layout with their keys in each record's padding; that saved 4/9 of
/// the table space (notes/mem-dense-link-tables.md). They now use `NodeValueLinkStore`.
///
/// The key is written before the index is inserted, so a probe never sees a value without its key (and a
/// fresh value's key 0 is never a handle: the reservation never hands out offset 0). A hit loads the value
/// the caller reads next; a false positive of hashbrown's 7-bit tag costs one value load; a rehash reads each
/// value's key once.
///
/// Stable addresses: as `LinkStore`, the values sit in arena chunks that are never reallocated, freed or
/// recycled, so a `P<V>` stays valid for the store's lifetime while the table of indexes grows and rehashes
/// around it.
pub struct KeyedLinkStore<K: 'static, V: 'static> {
    slots: hashbrown::HashTable<u32>, // the value's position in `chunks`
    chunks: Vec<P<PSlot<V>>>,         // first slot of each chunk
    len: u32,
    key: std::marker::PhantomData<P<K>>,
    /// Shared graph: as `LinkStore::parent`.
    parent: Option<&'static KeyedLinkStore<K, V>>,
}

impl<K: 'static, V: 'static> Default for KeyedLinkStore<K, V> {
    fn default() -> Self {
        KeyedLinkStore {
            slots: hashbrown::HashTable::new(),
            chunks: Vec::new(),
            len: 0,
            key: std::marker::PhantomData,
            parent: None,
        }
    }
}

/// The value at `index` of a store's chunks (`LinkStore::at`, as a function so that a probe closure can borrow the
/// chunks while the table is borrowed mutably).
#[inline]
fn keyed_at<V>(chunks: &[P<PSlot<V>>], index: u32) -> P<V> {
    #[expect(
        clippy::disallowed_methods,
        reason = "measured with the other link-store lookups (see LinkStore::at)"
    )]
    // SAFETY: every stored index is below the store's `len`, and `chunks` holds `LINK_CHUNK` values per started chunk.
    let chunk = unsafe { *chunks.get_unchecked((index >> LINK_CHUNK_SHIFT) as usize) };
    // SAFETY: `chunk` is the first slot of a `LINK_CHUNK`-slot array, and the offset is below `LINK_CHUNK`.
    unsafe { PSlot::nth(chunk, index as usize & (LINK_CHUNK - 1)) }
}

impl<K: 'static, V: KeyedLinks + 'static> KeyedLinkStore<K, V> {
    #[inline]
    fn hash(key: PKey) -> u64 {
        use std::hash::BuildHasher;
        rustc_hash::FxBuildHasher.hash_one(key)
    }

    #[inline]
    pub fn try_get(&self, key: P<K>) -> Option<P<V>> {
        self.try_get_pkey(key.key())
    }

    #[inline]
    fn try_get_pkey(&self, key: PKey) -> Option<P<V>> {
        let chunks = &self.chunks;
        match self
            .slots
            .find(Self::hash(key), |&i| keyed_at(chunks, i).link_key().get() == key)
            .map(|&i| keyed_at(chunks, i))
        {
            Some(v) => Some(v),
            None => self.parent.filter(|_| tsrs_core::sharedgraph::COMPILED_IN)?.try_get_pkey(key),
        }
    }

    #[inline]
    pub fn has(&self, key: P<K>) -> bool {
        self.try_get(key).is_some()
    }
}

impl<K: 'static, V: KeyedLinks + Default + LinkCopy + 'static> KeyedLinkStore<K, V> {
    /// Returns the links for `key`, creating them on first use.
    #[inline]
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub fn get(&mut self, key: P<K>) -> P<V> {
        let key = key.key();
        let chunks = &self.chunks;
        match self.slots.find(Self::hash(key), |&i| keyed_at(chunks, i).link_key().get() == key) {
            Some(&index) => keyed_at(chunks, index),
            None => self.create(key),
        }
    }

    /// `get` of a key without links (see `LinkStore::create`).
    #[cfg_attr(feature = "site-counts", track_caller)]
    #[inline(never)]
    fn create(&mut self, key: PKey) -> P<V> {
        crate::sitecount::hit("links", std::any::type_name::<V>());
        let index = self.len;
        if (index as usize).is_multiple_of(LINK_CHUNK) {
            self.chunks.push(PSlot::first(alloc_vec(
                std::iter::repeat_with(|| PSlot(V::default())).take(LINK_CHUNK).collect(),
            )));
        }
        self.len += 1;
        let value = keyed_at(&self.chunks, index);
        if let Some(frozen) = self
            .parent
            .filter(|_| tsrs_core::sharedgraph::COMPILED_IN)
            .and_then(|p| p.try_get_pkey(key))
        {
            value.copy_link_from(&frozen);
        }
        value.link_key().set(key);
        let chunks = &self.chunks;
        self.slots.insert_unique(Self::hash(key), index, |&i| {
            Self::hash(keyed_at(chunks, i).link_key().get())
        });
        value
    }
}

/// Go `symbolReferenceLinks` (`core.LinkStore[*ast.Symbol, SymbolReferenceLinks]`).
///
/// The record has one field, `referenceKinds`, a 4-byte `SymbolFlags`, kept in the slot next to the key (8
/// bytes with compressed pointers, the size of `LinkStore`'s key-and-index slot) instead of in an 8-byte
/// arena slot of its own: -6.5 MB on vscode at one checker (notes/mem-dense-link-tables.md).
///
/// No stable address: the flags move when the table grows, so the store hands out no reference. Go's callers
/// never keep the record: `symbolReferenced` ORs meanings into it and the unused-locals checks read it, each
/// on the spot.
#[derive(Default)]
pub struct SymbolReferenceLinkStore {
    pub slots: hashbrown::HashTable<ReferenceKindsSlot>,
}

impl SymbolReferenceLinkStore {
    /// Shared graph: a copy (the slots are heap data of the frozen seed checker).
    #[must_use]
    pub(crate) fn fork(&'static self) -> Self {
        SymbolReferenceLinkStore { slots: self.slots.clone() }
    }
}

#[derive(Clone, Copy)]
pub struct ReferenceKindsSlot {
    key: PKey,
    kinds: SymbolFlags,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    std::mem::size_of::<ReferenceKindsSlot>() == if tsrs_core::COMPRESSED_PTRS { 8 } else { 16 }
);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<ReferenceKindsSlot>() == 8);

impl SymbolReferenceLinkStore {
    #[inline]
    fn hash(key: PKey) -> u64 {
        use std::hash::BuildHasher;
        rustc_hash::FxBuildHasher.hash_one(key)
    }

    /// Go `c.symbolReferenceLinks.Get(symbol).referenceKinds |= meaning`: creates the record on first use.
    #[inline]
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub fn add_reference_kinds(&mut self, symbol: P<Symbol>, meaning: SymbolFlags) {
        let key = symbol.key();
        let slot = match self.slots.entry(Self::hash(key), |s| s.key == key, |s| Self::hash(s.key))
        {
            hashbrown::hash_table::Entry::Occupied(slot) => slot.into_mut(),
            hashbrown::hash_table::Entry::Vacant(slot) => {
                crate::sitecount::hit("links", "SymbolReferenceLinks");
                slot.insert(ReferenceKindsSlot { key, kinds: SymbolFlags::None }).into_mut()
            }
        };
        slot.kinds |= meaning;
    }

    /// Go `c.symbolReferenceLinks.Get(symbol).referenceKinds`, without creating a record for a symbol never
    /// referenced (`SymbolFlags::None` either way).
    #[inline]
    pub fn reference_kinds(&self, symbol: P<Symbol>) -> SymbolFlags {
        let key = symbol.key();
        self.slots.find(Self::hash(key), |s| s.key == key).map_or(SymbolFlags::None, |s| s.kinds)
    }
}

/// Links keyed by a node/symbol id (Go `PagedLinkStore`-backed stores).
///
/// Like Go, the id is looked up by indexing, not hashing: a group of `ID_GROUP` consecutive ids holds 2 bytes
/// per id (the slot's offset from the group's first slot, 0 = no links), found through a vector indexed by
/// group number (one `Option<P<IdGroup>>` per group of the id space below the highest id seen). The groups
/// and the values live in the arena (the values in fixed-size chunks, in first-access order). Stable
/// addresses: chunks are never reallocated, freed or recycled, so a `P<V>` stays valid for the store's
/// lifetime (`value_symbol_links` callers keep links across other accesses).
///
/// Node and symbol ids come from process-wide counters in per-thread blocks of 1,024, so with several
/// checkers one checker's links are dense in its own blocks and spread thinly over the other checkers' (every
/// checker touches most lib symbols, whichever checker numbered them). Groups are small (128 ids, 264 bytes)
/// so that a thinly used block costs little: the 1,024-id pages they replace were 14% full on vscode at 64
/// checkers, and the ids-to-slots tables took 3.3 MB per checker (notes/mem-64.md). A group whose offsets
/// outgrow 16 bits (a link made more than 65,535 links after the group's first) gets a 4-byte-per-id dense
/// form, as the narrow pages did.
pub struct IdLinkStore<V: 'static> {
    pub index: Vec<Option<P<IdGroup>>>,
    wide_slots: FxHashMap<u64, u32>, // ids >= 2^32 (long-running processes such as the test runner)
    chunks: Vec<P<PSlot<V>>>,        // first slot of each chunk
    len: u32,
    /// Shared graph: as `LinkStore::parent`.
    parent: Option<&'static IdLinkStore<V>>,
}

/// Slots as `slot - before` (0 = no links); `before` is one less than the first slot the group got (wrapping; slots
/// only grow). When an offset does not fit, `dense` points at the 4-byte form (`slot + 1`, 0 = no links), which is
/// read instead of `slots` from then on.
#[repr(C)]
#[expect(
    clippy::too_long_first_doc_paragraph,
    reason = "Public for the integration tests; the comment keeps its upstream wording."
)]
pub struct IdGroup {
    before: u32,
    pub dense: Cell<Option<P<[Cell<u32>; ID_GROUP]>>>,
    slots: [Cell<u16>; ID_GROUP],
}

#[cfg(target_pointer_width = "64")]
const _: () =
    assert!(std::mem::size_of::<IdGroup>() == if tsrs_core::COMPRESSED_PTRS { 264 } else { 272 });
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<IdGroup>() == 264);

/// Preserves the configuration entry point used by embedders.
///
/// Kept for embedders (tsrslint) and the pool, which chose sparse id pages for multi-checker runs: the groups
/// above made the sparse form unnecessary, so these are inert and `TSRS_SPARSE_ID_PAGES` is ignored.
pub fn set_multiple_checkers(_multiple: bool) {}

/// See `set_multiple_checkers`.
pub fn set_sparse_id_pages(_on: bool) {}

const ID_LINK_CHUNK_SHIFT: u32 = 12;
const ID_LINK_CHUNK: usize = 1 << ID_LINK_CHUNK_SHIFT;
const ID_GROUP_SHIFT: u32 = 7;
const ID_GROUP: usize = 1 << ID_GROUP_SHIFT;

impl<V: 'static> Default for IdLinkStore<V> {
    fn default() -> Self {
        IdLinkStore {
            index: Vec::new(),
            wide_slots: FxHashMap::default(),
            chunks: Vec::new(),
            len: 0,
            parent: None,
        }
    }
}

impl<V: 'static> IdLinkStore<V> {
    #[inline]
    fn at(&self, slot: u32) -> P<V> {
        debug_assert!(slot < self.len);
        #[expect(
            clippy::disallowed_methods,
            reason = "measured with the other link-store lookups (see LinkStore::at)"
        )]
        // SAFETY: every stored slot is below `len`; chunks hold `ID_LINK_CHUNK` values per started chunk.
        unsafe {
            PSlot::nth(
                *self.chunks.get_unchecked((slot >> ID_LINK_CHUNK_SHIFT) as usize),
                slot as usize & (ID_LINK_CHUNK - 1),
            )
        }
    }

    #[inline]
    fn slot(&self, id: u64) -> Option<u32> {
        match u32::try_from(id) {
            Ok(id) => self.narrow_slot(id),
            Err(_) => self.wide_slot(id),
        }
    }

    #[inline]
    fn narrow_slot(&self, id: u32) -> Option<u32> {
        let group = (*self.index.get((id >> ID_GROUP_SHIFT) as usize)?)?;
        let i = id as usize & (ID_GROUP - 1);
        // Narrow groups first: nearly all groups are narrow.
        if let Some(dense) = group.dense.get() {
            return Self::dense_slot(dense, i);
        }
        match group.slots[i].get() {
            0 => None,
            offset => Some(group.before.wrapping_add(offset as u32)),
        }
    }

    /// Inline: a call here would make every caller that inlines `narrow_slot` save registers on its hit path
    /// (`get_type_of_symbol`'s cached-type return).
    #[inline]
    fn dense_slot(dense: P<[Cell<u32>; ID_GROUP]>, i: usize) -> Option<u32> {
        dense[i].get().checked_sub(1)
    }

    // Out of line: keeps the hash lookup out of every inlined `get`.
    #[cold]
    #[inline(never)]
    fn wide_slot(&self, id: u64) -> Option<u32> {
        self.wide_slots.get(&id).copied()
    }

    #[inline]
    pub fn try_get(&self, id: u64) -> Option<P<V>> {
        match self.slot(id) {
            Some(slot) => Some(self.at(slot)),
            None => self.parent.filter(|_| tsrs_core::sharedgraph::COMPILED_IN)?.try_get(id),
        }
    }

    #[inline]
    fn has(&self, id: u64) -> bool {
        self.slot(id).is_some()
            || self
                .parent
                .filter(|_| tsrs_core::sharedgraph::COMPILED_IN)
                .is_some_and(|p| p.has(id))
    }

    /// Shared graph: an empty store that reads through to `self` (frozen).
    #[must_use]
    fn fork(&'static self) -> Self {
        IdLinkStore {
            index: Vec::new(),
            wide_slots: FxHashMap::default(),
            chunks: Vec::new(),
            len: 0,
            parent: Some(self),
        }
    }
}

impl<V: Default + LinkCopy + 'static> IdLinkStore<V> {
    #[inline]
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub fn get(&mut self, id: u64) -> P<V> {
        if let Some(slot) = self.slot(id) {
            return self.at(slot);
        }
        self.create(id)
    }

    #[inline(never)]
    #[cfg_attr(feature = "site-counts", track_caller)]
    fn create(&mut self, id: u64) -> P<V> {
        crate::sitecount::hit("links", std::any::type_name::<V>());
        let slot = self.len;
        if (slot as usize).is_multiple_of(ID_LINK_CHUNK) {
            self.chunks.push(PSlot::first(alloc_vec(
                std::iter::repeat_with(|| PSlot(V::default())).take(ID_LINK_CHUNK).collect(),
            )));
        }
        self.len += 1;
        if u32::try_from(id).is_ok() {
            let group_index = (id >> ID_GROUP_SHIFT) as usize;
            if group_index >= self.index.len() {
                // Grow by a quarter, not double: ids keep arriving in every checker's blocks for the whole run, so the
                // index ends near the top of the id space in every checker (a checker's index is a few hundred KB).
                let need = group_index + 1;
                if need > self.index.capacity() {
                    self.index.reserve_exact((need - self.index.len()).max(self.index.len() / 4));
                }
                self.index.resize(need, None);
            }
            let i = id as usize & (ID_GROUP - 1);
            let group = *self.index[group_index].get_or_insert_with(|| {
                P::new(IdGroup {
                    before: slot.wrapping_sub(1),
                    dense: Cell::new(None),
                    slots: [const { Cell::new(0) }; ID_GROUP],
                })
            });
            if let Some(dense) = group.dense.get() {
                dense[i].set(slot + 1);
            } else {
                match u16::try_from(slot.wrapping_sub(group.before)) {
                    Ok(offset) if offset != 0 => group.slots[i].set(offset),
                    _ => {
                        let dense: [Cell<u32>; ID_GROUP] = std::array::from_fn(|j| {
                            Cell::new(match group.slots[j].get() {
                                0 => 0,
                                offset => group.before.wrapping_add(offset as u32) + 1,
                            })
                        });
                        dense[i].set(slot + 1);
                        group.dense.set(Some(P::new(dense)));
                    }
                }
            }
        } else {
            self.wide_slots.insert(id, slot);
        }
        if let Some(frozen) =
            self.parent.filter(|_| tsrs_core::sharedgraph::COMPILED_IN).and_then(|p| p.try_get(id))
        {
            self.at(slot).copy_link_from(&frozen);
        }
        self.at(slot)
    }
}

/// Links keyed by a node or symbol id whose value is small (a 4-byte handle), stored in the table itself: no
/// slot, no offset, no separate value chunk.
///
/// Ids map through two levels that follow the per-thread id blocks of 1,024 (`ast::use_id_blocks`):
/// `blocks[id >> 10]` points at an arena table of 32 group pointers, and a group is an arena array of 32
/// values (`[V; 32]`, 128 bytes for a 4-byte `V`), allocated on the first access to one of its ids and
/// default (unset) until written. A checker's own id blocks fill their groups; the blocks other checkers
/// numbered get a 128-byte table and only the groups they touch (notes/mem-dense-link-tables.md: 0.98 / 0.67
/// of a group's ids have links at 1 / 32 checkers on vscode, against 0.98 / 0.41 for 128-id groups).
///
/// Stable addresses: groups and tables are arena objects that are never freed, moved or recycled (like
/// `IdLinkStore`'s groups and chunks), so a value handed out stays where it is for the store's lifetime.
/// Callers keep the reference across later accesses that create other links (`get_resolved_symbol` holds it
/// across `resolve_name`). Values are handed out as `&'static V`, not `P<V>`: a cell of a 4-byte value is
/// 4-aligned, and a `P` handle names 8-byte units.
///
/// Unlike `IdLinkStore`, the store cannot tell an id whose links were never created from one whose links are
/// unset: both read as `V::default()`. That is the same answer for every reader of a value whose fields are
/// all unset by default (`SymbolNodeLinks`).
pub struct InlineIdStore<V: 'static> {
    blocks: Vec<Option<P<InlineBlock<V>>>>,
    wide: FxHashMap<u64, P<V>>, // ids >= 2^32 (long-running processes such as the test runner)
    /// Shared graph: the frozen seed's store; a new group starts as a copy of the parent's group.
    parent: Option<&'static InlineIdStore<V>>,
}

const INLINE_GROUP_SHIFT: u32 = 5;
const INLINE_GROUP: usize = 1 << INLINE_GROUP_SHIFT;
/// One second-level table per id block (`ast::use_id_blocks` takes 1,024 ids at a time).
const INLINE_BLOCK_SHIFT: u32 = 10;
const INLINE_BLOCK_GROUPS: usize = 1 << (INLINE_BLOCK_SHIFT - INLINE_GROUP_SHIFT);

type InlineGroup<V> = [V; INLINE_GROUP];
type InlineBlock<V> = [Cell<Option<P<InlineGroup<V>>>>; INLINE_BLOCK_GROUPS];

impl<V: 'static> Default for InlineIdStore<V> {
    fn default() -> Self {
        InlineIdStore { blocks: Vec::new(), wide: FxHashMap::default(), parent: None }
    }
}

impl<V: 'static> InlineIdStore<V> {
    #[inline]
    fn narrow(&self, id: u32) -> Option<&'static V> {
        let block = (*self.blocks.get((id >> INLINE_BLOCK_SHIFT) as usize)?)?;
        let group =
            block.get()[(id >> INLINE_GROUP_SHIFT) as usize & (INLINE_BLOCK_GROUPS - 1)].get()?;
        Some(&group.get()[id as usize & (INLINE_GROUP - 1)])
    }

    #[inline]
    pub fn try_get(&self, id: u64) -> Option<&'static V> {
        let own = match u32::try_from(id) {
            Ok(id) => self.narrow(id),
            Err(_) => self.wide_get(id),
        };
        match own {
            Some(v) => Some(v),
            None => self.parent.filter(|_| tsrs_core::sharedgraph::COMPILED_IN)?.try_get(id),
        }
    }

    /// Shared graph: an empty store that reads through to `self` (frozen).
    #[must_use]
    fn fork(&'static self) -> Self {
        InlineIdStore { blocks: Vec::new(), wide: FxHashMap::default(), parent: Some(self) }
    }

    // Out of line: keeps the hash lookup out of every inlined `try_get`.
    #[cold]
    #[inline(never)]
    fn wide_get(&self, id: u64) -> Option<&'static V> {
        self.wide.get(&id).map(|v| v.get())
    }
}

impl<V: Default + LinkCopy + 'static> InlineIdStore<V> {
    #[inline]
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub fn get(&mut self, id: u64) -> &'static V {
        if let Ok(narrow) = u32::try_from(id)
            && let Some(value) = self.narrow(narrow)
        {
            return value;
        }
        self.create(id)
    }

    /// The value of an id whose group does not exist yet (or a wide id): allocates the block table and the group.
    #[inline(never)]
    #[cfg_attr(feature = "site-counts", track_caller)]
    fn create(&mut self, id: u64) -> &'static V {
        crate::sitecount::hit("links", std::any::type_name::<V>());
        let Ok(id) = u32::try_from(id) else {
            let parent = self.parent;
            return self
                .wide
                .entry(id)
                .or_insert_with(|| {
                    let v = P::new(V::default());
                    if let Some(frozen) = parent
                        .filter(|_| tsrs_core::sharedgraph::COMPILED_IN)
                        .and_then(|p| p.wide_get(id))
                    {
                        v.copy_link_from(frozen);
                    }
                    v
                })
                .get();
        };
        let b = (id >> INLINE_BLOCK_SHIFT) as usize;
        if b >= self.blocks.len() {
            // Grow by a quarter, not double: ids keep arriving in every checker's blocks for the whole run, so the
            // index ends near the top of the id space in every checker (4 bytes per 1,024 ids).
            let need = b + 1;
            if need > self.blocks.capacity() {
                self.blocks.reserve_exact((need - self.blocks.len()).max(self.blocks.len() / 4));
            }
            self.blocks.resize(need, None);
        }
        let block =
            *self.blocks[b].get_or_insert_with(|| P::new(std::array::from_fn(|_| Cell::new(None))));
        let cell = &block.get()[(id >> INLINE_GROUP_SHIFT) as usize & (INLINE_BLOCK_GROUPS - 1)];
        let group = if let Some(group) = cell.get() {
            group
        } else {
            let group: P<InlineGroup<V>> = P::new(std::array::from_fn(|_| V::default()));
            if let Some(p) = self.parent.filter(|_| tsrs_core::sharedgraph::COMPILED_IN) {
                let first = id & !(INLINE_GROUP as u32 - 1);
                if let Some(frozen) = p.narrow(first) {
                    let frozen_group: &'static [V] = std::slice::from_ref(frozen);
                    // SAFETY: `frozen` is the first value of the parent's group array of INLINE_GROUP values.
                    let frozen_group =
                        unsafe { std::slice::from_raw_parts(frozen_group.as_ptr(), INLINE_GROUP) };
                    for (dst, src) in group.get().iter().zip(frozen_group) {
                        dst.copy_link_from(src);
                    }
                }
            }
            cell.set(Some(group));
            group
        };
        &group.get()[id as usize & (INLINE_GROUP - 1)]
    }
}

pub const MAX_PAGE_LINK_COUNT: u64 = 0x100_0000;

#[inline]
pub fn dense_link_key(id: u64) -> Option<u64> {
    // Ordinary ids wrap to a large key and fail the bound check; the hot path needs one comparison.
    let key = id.wrapping_sub(ast::BLOCK_ID_OFFSET);
    (key < MAX_PAGE_LINK_COUNT).then_some(key)
}

/// Go `nodeLinkStore` with a checker-local 256-id generator.
///
/// Go uses a reserved high id range for this store; the AST decodes Rust's tagged 32-bit representation to that
/// range.
///
/// High-range ids are normalized before indexing the dense `InlineIdStore`; an ordinary id assigned elsewhere
/// uses a separate store so the two ranges cannot alias. Its one user, `symbol_node_links`, holds a 4-byte
/// symbol handle per node.
pub struct NodeLinkStore<V: 'static> {
    generator: ast::NodeIdGenerator,
    block_store: InlineIdStore<V>,
    ordinary_store: InlineIdStore<V>,
}

impl<V: 'static> Default for NodeLinkStore<V> {
    fn default() -> Self {
        NodeLinkStore {
            generator: ast::NodeIdGenerator::default(),
            block_store: InlineIdStore::default(),
            ordinary_store: InlineIdStore::default(),
        }
    }
}

impl<V: 'static> NodeLinkStore<V> {
    /// Shared graph: see `InlineIdStore::fork`.
    #[must_use]
    pub(crate) fn fork(&'static self) -> Self {
        NodeLinkStore {
            generator: ast::NodeIdGenerator::default(),
            block_store: self.block_store.fork(),
            ordinary_store: self.ordinary_store.fork(),
        }
    }
}

impl<V: Default + LinkCopy + 'static> NodeLinkStore<V> {
    /// The node's links, created on first use. The reference stays valid for the store's lifetime (`InlineIdStore`).
    #[inline]
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub(crate) fn get(&mut self, node: P<Node>) -> &'static V {
        let id = self.generator.get_node_id(node).0;
        if let Some(key) = dense_link_key(id) {
            self.block_store.get(key)
        } else {
            self.ordinary_store.get(id)
        }
    }
}

impl<V: 'static> NodeLinkStore<V> {
    /// `None` if no id of the node's group of 32 has links yet; otherwise the node's value, unset if the node itself
    /// never had links (`InlineIdStore`). Assigns the node an id, as Go's `TryGet` does.
    #[inline]
    pub(crate) fn try_get(&self, node: P<Node>) -> Option<&'static V> {
        let id = self.generator.get_node_id(node).0;
        if let Some(key) = dense_link_key(id) {
            self.block_store.try_get(key)
        } else {
            self.ordinary_store.try_get(id)
        }
    }

    /// `try_get` that returns `None` for a node without an id instead of assigning one (no side effect).
    #[inline]
    pub(crate) fn try_get_if_id_assigned(&self, node: P<Node>) -> Option<&'static V> {
        let id = ast::get_assigned_node_id(node)?.0;
        let (store, key) = match dense_link_key(id) {
            Some(key) => (&self.block_store, key),
            None if id >= ast::BLOCK_ID_OFFSET => return self.ordinary_store.try_get(id),
            None => (&self.ordinary_store, id),
        };
        let key = u32::try_from(key).ok()?;
        match store.narrow(key) {
            Some(v) => Some(v),
            None => store.parent.filter(|_| tsrs_core::sharedgraph::COMPILED_IN)?.narrow(key),
        }
    }
}

/// Go `nodeLinkStore` with per-id presence in each page.
///
/// Values are separate arena objects, so their addresses stay stable as pages grow. The ordinary-id
/// map covers nodes assigned an id before this checker first sees them. Rust uses 32-id groups
/// behind a second-level index: Go's 256-id pages increased multi-checker RSS on the public
/// VS Code case because each checker sparsely touches blocks assigned by other checkers.
pub struct NodeValueLinkStore<V: 'static> {
    generator: ast::NodeIdGenerator,
    blocks: Vec<Option<P<NodeValueBlock<V>>>>,
    ordinary: FxHashMap<u64, P<V>>,
    parent: Option<&'static NodeValueLinkStore<V>>,
}

type NodeValueGroup<V> = [Cell<Option<P<V>>>; INLINE_GROUP];
type NodeValueBlock<V> = [Cell<Option<P<NodeValueGroup<V>>>>; INLINE_BLOCK_GROUPS];

impl<V: 'static> Default for NodeValueLinkStore<V> {
    fn default() -> Self {
        Self {
            generator: ast::NodeIdGenerator::default(),
            blocks: Vec::new(),
            ordinary: FxHashMap::default(),
            parent: None,
        }
    }
}

impl<V: 'static> NodeValueLinkStore<V> {
    #[inline]
    fn local(&self, id: u64) -> Option<P<V>> {
        if let Some(key) = dense_link_key(id) {
            let block = (*self.blocks.get((key >> INLINE_BLOCK_SHIFT) as usize)?)?;
            let group =
                block[(key >> INLINE_GROUP_SHIFT) as usize & (INLINE_BLOCK_GROUPS - 1)].get()?;
            group[(key as usize) & (INLINE_GROUP - 1)].get()
        } else {
            self.ordinary.get(&id).copied()
        }
    }

    #[inline]
    fn try_get_id(&self, id: u64) -> Option<P<V>> {
        self.local(id)
            .or_else(|| self.parent.filter(|_| tsrs_core::sharedgraph::COMPILED_IN)?.try_get_id(id))
    }

    #[inline]
    pub fn try_get(&self, node: P<Node>) -> Option<P<V>> {
        self.try_get_id(self.generator.get_node_id(node).0)
    }

    #[inline]
    pub fn has(&self, node: P<Node>) -> bool {
        self.try_get(node).is_some()
    }

    /// Shared graph: the fork reads the frozen store and copies a link on first mutation.
    #[must_use]
    pub fn fork(&'static self) -> Self {
        Self { parent: Some(self), ..Self::default() }
    }
}

impl<V: Default + LinkCopy + 'static> NodeValueLinkStore<V> {
    #[inline]
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub fn get(&mut self, node: P<Node>) -> P<V> {
        let id = self.generator.get_node_id(node).0;
        self.local(id).unwrap_or_else(|| self.create(id))
    }

    #[inline(never)]
    #[cfg_attr(feature = "site-counts", track_caller)]
    fn create(&mut self, id: u64) -> P<V> {
        crate::sitecount::hit("links", std::any::type_name::<V>());
        let value = P::new(V::default());
        if let Some(frozen) = self
            .parent
            .filter(|_| tsrs_core::sharedgraph::COMPILED_IN)
            .and_then(|p| p.try_get_id(id))
        {
            value.copy_link_from(&frozen);
        }
        if let Some(key) = dense_link_key(id) {
            let block_index = (key >> INLINE_BLOCK_SHIFT) as usize;
            if block_index >= self.blocks.len() {
                self.blocks.resize(block_index + 1, None);
            }
            let block = *self.blocks[block_index]
                .get_or_insert_with(|| P::new(std::array::from_fn(|_| Cell::new(None))));
            let cell = &block[(key >> INLINE_GROUP_SHIFT) as usize & (INLINE_BLOCK_GROUPS - 1)];
            let group = if let Some(group) = cell.get() {
                group
            } else {
                let group = P::new(std::array::from_fn(|_| Cell::new(None)));
                cell.set(Some(group));
                group
            };
            group[(key as usize) & (INLINE_GROUP - 1)].set(Some(value));
        } else {
            self.ordinary.insert(id, value);
        }
        value
    }
}

/// Go `symbolLinkStore`: assigns an id from a checker-local 256-id block when the symbol has no id.
///
/// Its reserved high ids are observable: unique-symbol internal names embed them, and the node builder counts
/// their length toward truncation. Rust keeps the packed `IdLinkStore` layout for high and ordinary ids in
/// separate stores to prevent collisions.
pub struct SymbolArenaLinkStore<V: 'static> {
    generator: ast::SymbolIdGenerator,
    block_store: IdLinkStore<V>,
    ordinary_store: IdLinkStore<V>,
}

impl<V: 'static> Default for SymbolArenaLinkStore<V> {
    fn default() -> Self {
        SymbolArenaLinkStore {
            generator: ast::SymbolIdGenerator::default(),
            block_store: IdLinkStore::default(),
            ordinary_store: IdLinkStore::default(),
        }
    }
}

impl<V: 'static> SymbolArenaLinkStore<V> {
    /// Shared graph: see `IdLinkStore::fork`.
    #[must_use]
    pub(crate) fn fork(&'static self) -> Self {
        SymbolArenaLinkStore {
            generator: ast::SymbolIdGenerator::default(),
            block_store: self.block_store.fork(),
            ordinary_store: self.ordinary_store.fork(),
        }
    }
}

impl<V: Default + LinkCopy + 'static> SymbolArenaLinkStore<V> {
    #[inline]
    #[cfg_attr(feature = "site-counts", track_caller)]
    pub fn get(&mut self, symbol: P<Symbol>) -> P<V> {
        let id = self.generator.get_symbol_id(symbol).0;
        if let Some(key) = dense_link_key(id) {
            self.block_store.get(key)
        } else {
            self.ordinary_store.get(id)
        }
    }
}

impl<V: 'static> SymbolArenaLinkStore<V> {
    #[inline]
    pub(crate) fn try_get(&self, symbol: P<Symbol>) -> Option<P<V>> {
        let id = self.generator.get_symbol_id(symbol).0;
        if let Some(key) = dense_link_key(id) {
            self.block_store.try_get(key)
        } else {
            self.ordinary_store.try_get(id)
        }
    }

    /// `try_get` that returns `None` for a symbol without an id instead of assigning one (no side effect, no call:
    /// for fast paths whose fallback does the `get`).
    #[inline]
    pub fn try_get_if_id_assigned(&self, symbol: P<Symbol>) -> Option<P<V>> {
        let id = ast::get_assigned_symbol_id(symbol)?.0;
        let (store, key) = match dense_link_key(id) {
            Some(key) => (&self.block_store, key),
            None if id >= ast::BLOCK_ID_OFFSET => return self.ordinary_store.try_get(id),
            None => (&self.ordinary_store, id),
        };
        let key = u32::try_from(key).ok()?;
        match store.narrow_slot(key) {
            Some(slot) => Some(store.at(slot)),
            None => store
                .parent
                .filter(|_| tsrs_core::sharedgraph::COMPILED_IN)?
                .try_get(u64::from(key)),
        }
    }

    #[inline]
    pub(crate) fn has(&self, symbol: P<Symbol>) -> bool {
        let id = self.generator.get_symbol_id(symbol).0;
        if let Some(key) = dense_link_key(id) {
            self.block_store.has(key)
        } else {
            self.ordinary_store.has(id)
        }
    }
}
