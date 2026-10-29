//! Hash maps of small `Copy` keys and values whose slots are packed to 4-byte alignment (notes/mem-checker-heap.md).
//!
//! The checker's caches keyed by a `CacheHashKey` (Go's 128-bit `xxh3` key) store a 16-byte key and a 4-byte type
//! handle; as a `(K, V)` tuple in an `FxHashMap` that slot is padded to 24 bytes. `PackedMap` stores the same pair in
//! 20 bytes, and hashes a `CacheHashKey` by its own low word (it is already a hash) instead of running Fx over both
//! words. Same contents, same lookups; only the table's layout differs. Nothing iterates these maps in an order
//! that reaches output (`values` is for counts and tests).

use crate::*;

/// A key `PackedMap` can hash.
pub trait PackedKey: Copy + Eq {
    fn packed_hash(&self) -> u64;
}

impl PackedKey for CacheHashKey {
    #[inline]
    fn packed_hash(&self) -> u64 {
        // The key is the output of xxh3-128: its low word is already uniformly distributed.
        self.lo
    }
}

impl PackedKey for CachedSignatureKey {
    #[inline]
    fn packed_hash(&self) -> u64 {
        self.key.lo ^ (self.sig.key() as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15)
    }
}

#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct PackedSlot<K, V> {
    key: K,
    value: V,
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(
    std::mem::size_of::<PackedSlot<CacheHashKey, P<Type>>>()
        == if tsrs_core::COMPRESSED_PTRS { 20 } else { 24 }
);
#[cfg(target_pointer_width = "32")]
const _: () = assert!(std::mem::size_of::<PackedSlot<CacheHashKey, P<Type>>>() == 20);

pub struct PackedMap<K, V> {
    table: hashbrown::HashTable<PackedSlot<K, V>>,
}

impl<K: Copy, V: Copy> Clone for PackedMap<K, V> {
    fn clone(&self) -> Self {
        PackedMap { table: self.table.clone() }
    }
}

impl<K, V> Default for PackedMap<K, V> {
    fn default() -> Self {
        PackedMap { table: hashbrown::HashTable::new() }
    }
}

impl<K: PackedKey, V: Copy> PackedMap<K, V> {
    /// A map holding one entry, with the capacity `FxHashMap::from_iter([(key, value)])` has.
    pub(crate) fn from_one(key: K, value: V) -> Self {
        let mut m = PackedMap { table: hashbrown::HashTable::with_capacity(1) };
        m.insert(key, value);
        m
    }

    #[inline]
    pub(crate) fn get(&self, key: &K) -> Option<V> {
        let key = *key;
        self.table.find(key.packed_hash(), |slot| ({ slot.key }) == key).map(|slot| slot.value)
    }

    /// Like `HashMap::insert`: returns the value the key had.
    #[inline]
    pub(crate) fn insert(&mut self, key: K, value: V) -> Option<V> {
        match self.table.entry(
            key.packed_hash(),
            |slot| ({ slot.key }) == key,
            |slot| ({ slot.key }).packed_hash(),
        ) {
            hashbrown::hash_table::Entry::Occupied(mut slot) => {
                let old = slot.get().value;
                slot.get_mut().value = value;
                Some(old)
            }
            hashbrown::hash_table::Entry::Vacant(slot) => {
                slot.insert(PackedSlot { key, value });
                None
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.table.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.table.is_empty()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.table.capacity()
    }

    pub(crate) fn clear(&mut self) {
        self.table.clear();
    }
}

/// `GoMap` (a nil-able shared map in the arena) over a `PackedMap`, for the `CacheHashKey`-keyed instantiation maps
/// of arena objects (conditional roots, type aliases). Same `make` / `get` / `set` behavior as `GoMap`.
pub struct GoPackedMap<K: 'static, V: 'static>(
    tsrs_core::sharedgraph::OvExact<Option<P<RefCell<PackedMap<K, V>>>>>,
);

impl<K: 'static, V: 'static> Default for GoPackedMap<K, V> {
    fn default() -> Self {
        GoPackedMap(tsrs_core::sharedgraph::OvExact::new(None))
    }
}

impl<K: PackedKey + 'static, V: Copy + 'static> GoPackedMap<K, V> {
    /// Go `m = make(map[K]V)`.
    pub(crate) fn make(&self) {
        // As `GoMap::make`: the table lives where the map field does (emit scratch regions).
        let scratch = tsrs_core::arena::scratch_contains(std::ptr::from_ref::<Self>(self) as usize);
        self.0.set(Some(P::new_in(scratch, RefCell::new(PackedMap::default()))));
    }

    /// Go `v, ok := m[k]` (reading a nil map is allowed).
    #[inline]
    pub(crate) fn get(&self, key: &K) -> Option<V> {
        let m = self.0.get()?;
        tsrs_core::sharedgraph::with_ref(&m, |m| m.get(key))
    }

    /// Go `m[k] = v`. Creates the map if it is nil, as `GoMap::set` does.
    pub(crate) fn set(&self, key: K, value: V) {
        let m = match self.0.get() {
            Some(m) if tsrs_core::sharedgraph::frozen(m.get()) => {
                // Shared graph: this checker's copy of a frozen table.
                let copy = P::new(RefCell::new(tsrs_core::sharedgraph::with_ref(&m, Clone::clone)));
                self.0.set(Some(copy));
                copy
            }
            Some(m) => m,
            None => {
                self.make();
                self.0.get().unwrap()
            }
        };
        m.borrow_mut().insert(key, value);
    }
}

/// Go `c.stringLiteralTypes` (`map[string]*Type`): the literal types by their value.
///
/// The key was a heap `String` copy of the text the literal type already holds in the arena; the table now
/// stores only the type and compares its value (one 4-byte slot instead of a 32-byte `(String, P<Type>)` slot
/// plus the copied text).
#[derive(Default, Clone)]
pub struct StringLiteralTypes {
    table: hashbrown::HashTable<P<Type>>,
    /// Shared graph: the frozen seed's table, read through.
    base: Option<&'static StringLiteralTypes>,
}

impl StringLiteralTypes {
    #[inline]
    fn hash(value: &str) -> u64 {
        use std::hash::BuildHasher;
        rustc_hash::FxBuildHasher.hash_one(value)
    }

    #[inline]
    fn value_of(t: P<Type>) -> &'static str {
        match t.as_literal_type().value() {
            Some(LiteralValue::String(s)) => s,
            _ => unreachable!("string literal cache entry without a string value"),
        }
    }

    #[inline]
    pub(crate) fn get(&self, value: &str) -> Option<P<Type>> {
        let h = Self::hash(value);
        match self.table.find(h, |&t| Self::value_of(t) == value) {
            Some(&t) => Some(t),
            None => self
                .base
                .filter(|_| tsrs_core::sharedgraph::COMPILED_IN)?
                .table
                .find(h, |&t| Self::value_of(t) == value)
                .copied(),
        }
    }

    /// Shared graph: an empty table that reads through to `self` (frozen).
    #[must_use]
    pub(crate) fn fork(&'static self) -> Self {
        StringLiteralTypes { table: hashbrown::HashTable::new(), base: Some(self) }
    }

    /// Adds a string literal type whose value is not in the table yet.
    pub(crate) fn insert_new(&mut self, t: P<Type>) {
        let value = Self::value_of(t);
        debug_assert!(self.get(value).is_none());
        self.table.insert_unique(Self::hash(value), t, |&t| Self::hash(Self::value_of(t)));
    }
}
