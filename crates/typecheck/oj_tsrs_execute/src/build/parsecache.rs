// Port of execute/build/parseCache.go.

use std::hash::Hash;
use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;

struct ParseCacheEntry<V> {
    value: Mutex<Option<V>>,
}

pub struct ParseCache<K, V> {
    entries: Mutex<FxHashMap<K, Arc<ParseCacheEntry<V>>>>,
}

impl<K, V> Default for ParseCache<K, V> {
    fn default() -> Self {
        ParseCache { entries: Mutex::new(FxHashMap::default()) }
    }
}

// Go compares the value with its zero value (`allowZero`); every V here is an Option.
pub trait IsZeroValue {
    fn is_zero_value(&self) -> bool;
}

impl<T> IsZeroValue for Option<T> {
    fn is_zero_value(&self) -> bool {
        self.is_none()
    }
}

impl<K: Hash + Eq + Clone, V: Clone + IsZeroValue> ParseCache<K, V> {
    // parseCache.go:18
    pub(crate) fn load_or_store(
        &self,
        key: &K,
        parse: impl FnOnce(&K) -> V,
        allow_zero: bool,
    ) -> V {
        let new_entry = Arc::new(ParseCacheEntry { value: Mutex::new(None) });
        let mut new_guard = new_entry.value.lock().unwrap();
        let existing = {
            let mut entries = self.entries.lock().unwrap();
            if let Some(entry) = entries.get(key) {
                Some(Arc::clone(entry))
            } else {
                entries.insert(key.clone(), Arc::clone(&new_entry));
                None
            }
        };
        if let Some(entry) = existing {
            drop(new_guard);
            let mut guard = entry.value.lock().unwrap();
            if let Some(value) = guard.as_ref()
                && (allow_zero || !value.is_zero_value())
            {
                return value.clone();
            }
            let value = parse(key);
            *guard = Some(value.clone());
            return value;
        }
        let value = parse(key);
        *new_guard = Some(value.clone());
        value
    }

    // parseCache.go:34
    pub(crate) fn store(&self, key: K, value: V) {
        self.entries
            .lock()
            .unwrap()
            .insert(key, Arc::new(ParseCacheEntry { value: Mutex::new(Some(value)) }));
    }

    // parseCache.go:38
    pub(crate) fn delete(&self, key: &K) {
        self.entries.lock().unwrap().remove(key);
    }

    // parseCache.go:42
    pub(crate) fn reset(&self) {
        self.entries.lock().unwrap().clear();
    }
}
