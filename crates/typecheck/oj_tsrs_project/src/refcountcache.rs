use std::hash::Hash;
use std::sync::{Arc, Mutex};

use tsrs_core::collections::SyncMap;

struct RefCountCacheEntry<V> {
    mu: Mutex<RefCountCacheEntryState<V>>,
}

struct RefCountCacheEntryState<V> {
    value: Option<V>,
    ref_count: usize,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct RefCountCacheOptions {
    // DisableDeletion prevents entries from being removed from the cache.
    // Used for testing.
    pub disable_deletion: bool,
}

pub struct RefCountCache<K: Hash + Eq, V, AcquireArgs> {
    options: RefCountCacheOptions,
    entries: SyncMap<K, Arc<RefCountCacheEntry<V>>>,

    parse: Box<dyn Fn(&K, AcquireArgs) -> V + Send + Sync>,

    // Not in Go (memory regions, docs/LSP.md): told when an entry's value leaves the cache (final Deref) and when a
    // deleted entry's value is stored again (`Ref` racing a final Deref).
    pub(crate) on_evict: Option<Box<dyn Fn(&V) + Send + Sync>>,
    pub(crate) on_revive: Option<Box<dyn Fn(&V) + Send + Sync>>,
}

// refcountcache.go:28
pub(crate) fn new_ref_count_cache<K: Hash + Eq, V, AcquireArgs>(
    options: RefCountCacheOptions,
    parse: impl Fn(&K, AcquireArgs) -> V + Send + Sync + 'static,
) -> RefCountCache<K, V, AcquireArgs> {
    RefCountCache {
        options,
        entries: SyncMap::default(),
        parse: Box::new(parse),
        on_evict: None,
        on_revive: None,
    }
}

impl<K: Hash + Eq + Clone, V: Clone, AcquireArgs> RefCountCache<K, V, AcquireArgs> {
    // refcountcache.go:44
    // Acquire retrieves or creates a cache entry for the given identity and hash.
    // If an entry exists with matching identity and hash, its refcount is incremented
    // and the cached value is returned. Otherwise, parse() is called to create the
    // value, which is stored and returned with refcount 1.
    //
    // The caller is responsible for calling Deref when done with the value.
    pub(crate) fn acquire(&self, identity: &K, acquire_args: AcquireArgs) -> V {
        self.load_or_store_new_locked_entry(identity, |entry, loaded| {
            if !loaded {
                // New entry - parse the value
                let value = (self.parse)(identity, acquire_args);
                entry.value = Some(value.clone());
                return value;
            }
            entry.value.clone().unwrap()
        })
    }

    // refcountcache.go:55
    pub fn has(&self, identity: &K) -> bool {
        self.entries.load(identity).is_some()
    }

    // refcountcache.go:64
    // AcquireExisting retrieves an existing entry and increments its reference count.
    // It returns false without producing a value when no live entry exists.
    //
    // The caller is responsible for calling Deref when a value is returned.
    pub(crate) fn acquire_existing(&self, identity: &K) -> Option<V> {
        let entry = self.entries.load(identity)?;
        let mut st = entry.mu.lock().unwrap();
        if st.ref_count == 0 && !self.options.disable_deletion {
            return None;
        }
        st.ref_count += 1;
        st.value.clone()
    }

    // refcountcache.go:86
    // AcquireOrError retrieves an existing entry (incrementing its refcount) or produces a new one via
    // produce. If produce returns an error, no entry is stored and the error is returned, so callers can
    // cache only successful results. produce runs while holding the new entry's lock, so concurrent
    // acquisitions of the same identity that miss serialize on it.
    //
    // The caller is responsible for calling Deref when a value is returned without error.

    // refcountcache.go:105
    // Ref increments the reference count for an existing entry.
    // Panics if the entry does not exist.
    pub(crate) fn ref_(&self, identity: &K) {
        let Some(entry) = self.entries.load(identity) else {
            panic!("cache entry not found");
        };
        let mut st = entry.mu.lock().unwrap();
        if st.ref_count == 0 && !self.options.disable_deletion {
            // Entry was deleted while we were acquiring the lock
            let value = st.value.clone();
            if let (Some(on_revive), Some(value)) = (&self.on_revive, &value) {
                on_revive(value);
            }
            self.load_or_store_new_locked_entry(identity, |new_entry, _| {
                new_entry.value = value;
            });
            return;
        }
        st.ref_count += 1;
    }

    // refcountcache.go:125
    // Deref decrements the reference count for an entry.
    // When the refcount reaches zero, the entry is removed from the cache
    // (unless DisableDeletion is set).
    pub(crate) fn deref(&self, identity: &K) {
        let Some(entry) = self.entries.load(identity) else {
            return;
        };
        let mut st = entry.mu.lock().unwrap();
        st.ref_count = st.ref_count.saturating_sub(1);
        if st.ref_count == 0 && !self.options.disable_deletion {
            self.entries.delete(identity);
            if let (Some(on_evict), Some(value)) = (&self.on_evict, &st.value) {
                on_evict(value);
            }
        }
    }

    // refcountcache.go:141
    // loadOrStoreNewLockedEntry loads an existing entry or creates a new one.
    // The returned entry's mutex is locked and its refCount is incremented
    // (or initialized to 1 in the case of a new entry).
    //
    // Go returns the entry locked and the caller unlocks it with `defer`; here the caller's code runs as `f`
    // while the lock is held.
    fn load_or_store_new_locked_entry<R>(
        &self,
        key: &K,
        f: impl FnOnce(&mut RefCountCacheEntryState<V>, bool) -> R,
    ) -> R {
        loop {
            let entry = Arc::new(RefCountCacheEntry {
                mu: Mutex::new(RefCountCacheEntryState { value: None, ref_count: 1 }),
            });
            let mut guard = entry.mu.lock().unwrap();
            let (existing, loaded) = self.entries.load_or_store(key.clone(), Arc::clone(&entry));
            if loaded {
                drop(guard);
                let mut existing_guard = existing.mu.lock().unwrap();
                if existing_guard.ref_count == 0 && !self.options.disable_deletion {
                    // Existing entry was deleted while we were acquiring the lock
                    continue;
                }
                existing_guard.ref_count += 1;
                return f(&mut existing_guard, true);
            }
            return f(&mut guard, false);
        }
    }

    #[expect(clippy::len_without_is_empty, reason = "Public for the integration tests.")]
    pub fn len(&self) -> usize {
        self.entries.size()
    }
}
