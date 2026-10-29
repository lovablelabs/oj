use hashbrown::HashTable;
use hashbrown::hash_table::Entry;
use rustc_hash::{FxHashMap, FxHasher};
use std::borrow::Borrow;
use std::hash::{Hash, Hasher};
use std::sync::{OnceLock, RwLock};

/// Shards per map. Program construction reads and writes the resolver's and package.json caches from every worker
/// (vscode: 390k acquisitions in a 0.14 s phase at 64 threads); one lock serialized them on its cache line and in
/// the kernel's futex queues (notes/perf-frontend-64.md). Keys spread over the shards by their hash, so a shard
/// sees 1/64 of the traffic; the same hash probes the shard's table, so a lookup hashes once.
const SHARDS: usize = 64;
const GROUPS: usize = 8;
const GROUP_SIZE: usize = SHARDS / GROUPS;
const _: () = assert!(SHARDS.is_power_of_two() && GROUPS.is_power_of_two() && GROUPS <= SHARDS);

#[derive(Debug)]
pub struct SyncMap<K: Hash + Eq, V> {
    // Read-write locks: the caches are read from every worker and rarely written. (No caller re-enters the map while
    // holding a shard, so a waiting writer cannot deadlock a nested read.)
    // Most resolver caches are empty or small. Allocate locks in groups of eight on the first write,
    // retaining the same 64 logical shards and hash distribution (notes/data-structure-redesigns.md).
    shards: [OnceLock<Box<[RwLock<HashTable<(K, V)>>; GROUP_SIZE]>>; GROUPS],
}

impl<K: Hash + Eq, V> Default for SyncMap<K, V> {
    fn default() -> Self {
        SyncMap { shards: std::array::from_fn(|_| OnceLock::new()) }
    }
}

#[inline]
fn hash_of<Q: Hash + ?Sized>(key: &Q) -> u64 {
    let mut h = FxHasher::default();
    key.hash(&mut h);
    h.finish()
}

#[inline]
fn read<K, V>(
    shard: &RwLock<HashTable<(K, V)>>,
) -> std::sync::RwLockReadGuard<'_, HashTable<(K, V)>> {
    crate::festats::timed_counted(
        crate::festats::Cat::LockSyncMap,
        crate::festats::Cat::SyncMapOps,
        || shard.read().unwrap(),
    )
}

#[inline]
fn write<K, V>(
    shard: &RwLock<HashTable<(K, V)>>,
) -> std::sync::RwLockWriteGuard<'_, HashTable<(K, V)>> {
    crate::festats::timed_counted(
        crate::festats::Cat::LockSyncMap,
        crate::festats::Cat::SyncMapOps,
        || shard.write().unwrap(),
    )
}

impl<K: Hash + Eq, V> SyncMap<K, V> {
    fn shard_index(hash: u64) -> usize {
        (hash >> (u64::BITS - SHARDS.trailing_zeros())) as usize
    }
    #[inline]
    fn shard(&self, hash: u64) -> Option<&RwLock<HashTable<(K, V)>>> {
        let i = Self::shard_index(hash);
        self.shards[i / GROUP_SIZE].get().map(|group| &group[i % GROUP_SIZE])
    }
    #[inline]
    fn shard_or_init(&self, hash: u64) -> &RwLock<HashTable<(K, V)>> {
        let i = Self::shard_index(hash);
        &self.shards[i / GROUP_SIZE]
            .get_or_init(|| Box::new(std::array::from_fn(|_| RwLock::new(HashTable::new()))))
            [i % GROUP_SIZE]
    }
    fn initialized_shards(&self) -> impl Iterator<Item = &RwLock<HashTable<(K, V)>>> {
        self.shards.iter().filter_map(OnceLock::get).flat_map(|group| group.iter())
    }
}

impl<K: Hash + Eq + Clone, V: Clone> SyncMap<K, V> {
    /// The value under `key`; `key` may be a borrowed form of `K` (`str` for `String`), hashing the same.
    pub fn load<Q>(&self, key: &Q) -> Option<V>
    where
        K: Borrow<Q>,
        Q: Hash + Eq + ?Sized,
    {
        let hash = hash_of(key);
        let m = read(self.shard(hash)?);
        m.find(hash, |(k, _)| k.borrow() == key).map(|(_, v)| v.clone())
    }

    pub fn store(&self, key: K, value: V) {
        let hash = hash_of(&key);
        let mut m = write(self.shard_or_init(hash));
        match m.entry(hash, |(k, _)| *k == key, |(k, _)| hash_of(k)) {
            Entry::Occupied(mut e) => e.get_mut().1 = value,
            Entry::Vacant(e) => {
                e.insert((key, value));
            }
        }
    }

    pub fn load_or_store(&self, key: K, value: V) -> (V, bool) {
        let hash = hash_of(&key);
        let shard = self.shard_or_init(hash);
        if let Some((_, v)) = read(shard).find(hash, |(k, _)| *k == key) {
            return (v.clone(), true);
        }
        let mut m = write(shard);
        match m.entry(hash, |(k, _)| *k == key, |(k, _)| hash_of(k)) {
            Entry::Occupied(e) => (e.get().1.clone(), true),
            Entry::Vacant(e) => {
                e.insert((key, value.clone()));
                (value, false)
            }
        }
    }

    pub fn delete(&self, key: &K) {
        let hash = hash_of(key);
        let Some(shard) = self.shard(hash) else {
            return;
        };
        let mut m = shard.write().unwrap();
        if let Ok(e) = m.find_entry(hash, |(k, _)| k == key) {
            e.remove();
        }
    }

    pub fn clear(&self) {
        for shard in self.initialized_shards() {
            shard.write().unwrap().clear();
        }
    }

    pub fn range(&self, mut f: impl FnMut(&K, &V) -> bool) {
        // Copy shards in index order before invoking callbacks, so callbacks may reenter or mutate the map.
        let entries: Vec<(K, V)> = self
            .initialized_shards()
            .flat_map(|s| s.read().unwrap().iter().cloned().collect::<Vec<_>>())
            .collect();
        entries.iter().all(|(k, v)| f(k, v));
    }

    // Size returns the approximate number of items in the map.
    pub fn size(&self) -> usize {
        self.initialized_shards().map(|s| s.read().unwrap().len()).sum()
    }

    pub fn to_map(&self) -> FxHashMap<K, V> {
        let mut out = FxHashMap::with_capacity_and_hasher(self.size(), rustc_hash::FxBuildHasher);
        for shard in self.initialized_shards() {
            out.extend(shard.read().unwrap().iter().cloned());
        }
        out
    }

    pub fn keys(&self) -> Vec<K> {
        self.initialized_shards()
            .flat_map(|s| s.read().unwrap().iter().map(|(k, _)| k.clone()).collect::<Vec<_>>())
            .collect()
    }

    #[must_use]
    pub fn clone_map(&self) -> SyncMap<K, V> {
        let clone = SyncMap::default();
        for (i, group) in self.shards.iter().enumerate() {
            if let Some(group) = group.get() {
                let copy = Box::new(std::array::from_fn(|index| {
                    RwLock::new(group[index].read().unwrap().clone())
                }));
                // The clone is not published until every initialized group has been copied.
                assert!(clone.shards[i].set(copy).is_ok());
            }
        }
        clone
    }
}
