use std::hash::Hash;
use std::sync::{Arc, Mutex, Weak};

use rustc_hash::{FxHashMap, FxHashSet};

use super::entry::EntryState;
use super::interfaces::{Cloneable, Shared, Value};

// Go `map[K]V` with pointer values, as snapshots hold it (shared between snapshots until changed).
pub type SharedMap<K, T> = Arc<FxHashMap<K, Shared<T>>>;

// Go `Map[K, V]` is not synchronized; its locks only provide interior mutability and are never held while a
// callback runs (a `Change` callback may delete the entry or look up other entries, as in Go).
pub struct MapEntry<K, T> {
    m: Weak<MapInner<K, T>>,
    key: K,
    st: Mutex<EntryState<T>>,
}

pub struct Map<K, T> {
    inner: Arc<MapInner<K, T>>,
}

struct MapInner<K, T> {
    base: Mutex<SharedMap<K, T>>,
    dirty: Mutex<FxHashMap<K, Arc<MapEntry<K, T>>>>,
}

impl<K: Hash + Eq + Clone, T: Cloneable + Clone> MapEntry<K, T> {
    pub fn key(&self) -> K {
        self.key.clone()
    }

    fn register_dirty(self: &Arc<Self>) {
        if let Some(m) = self.m.upgrade() {
            m.dirty.lock().unwrap().insert(self.key.clone(), Arc::clone(self));
        }
    }

    // map.go:10
    pub fn change(self: &Arc<Self>, apply: impl FnOnce(&mut T)) {
        let (mut value, register) = {
            let mut st = self.st.lock().unwrap();
            assert!(!st.delete, "tried to change a deleted entry");
            let mut register = false;
            if !st.dirty {
                st.value = Some(st.value.as_ref().unwrap().clone_shared());
                st.dirty = true;
                register = true;
            }
            (st.value.take().unwrap(), register)
        };
        if register {
            self.register_dirty();
        }
        apply(Shared::make_mut(&mut value));
        self.st.lock().unwrap().value = Some(value);
    }

    // map.go:22
    pub fn replace(self: &Arc<Self>, new_value: Shared<T>) {
        let register = {
            let mut st = self.st.lock().unwrap();
            assert!(!st.delete, "tried to change a deleted entry");
            let register = !st.dirty;
            st.dirty = true;
            st.value = Some(new_value);
            register
        };
        if register {
            self.register_dirty();
        }
    }

    // map.go:33
    pub fn change_if(
        self: &Arc<Self>,
        cond: impl FnOnce(&T) -> bool,
        apply: impl FnOnce(&mut T),
    ) -> bool {
        let value = self.st.lock().unwrap().value();
        if let Some(value) = value
            && cond(&value)
        {
            drop(value);
            self.change(apply);
            return true;
        }
        false
    }

    // map.go:41
    pub fn delete(self: &Arc<Self>) {
        let register = {
            let mut st = self.st.lock().unwrap();
            let register = !st.dirty;
            st.delete = true;
            register
        };
        if register {
            self.register_dirty();
        }
    }

    pub fn value(&self) -> Option<Shared<T>> {
        self.st.lock().unwrap().value()
    }

    fn original(&self) -> Option<Shared<T>> {
        self.st.lock().unwrap().original()
    }

    fn dirty(&self) -> bool {
        self.st.lock().unwrap().dirty()
    }
}

// Go passes `*MapEntry` where a `Value[V]` is expected.
impl<K: Hash + Eq + Clone, T: Cloneable + Clone> Value<T> for Arc<MapEntry<K, T>> {
    fn value(&self) -> Option<Shared<T>> {
        MapEntry::value(self)
    }

    fn original(&self) -> Option<Shared<T>> {
        MapEntry::original(self)
    }

    fn dirty(&self) -> bool {
        MapEntry::dirty(self)
    }

    fn change(&self, apply: &mut dyn FnMut(&mut T)) {
        MapEntry::change(self, |v| apply(v));
    }

    fn change_if(&self, cond: &mut dyn FnMut(&T) -> bool, apply: &mut dyn FnMut(&mut T)) -> bool {
        MapEntry::change_if(self, |v| cond(v), |v| apply(v))
    }

    fn delete(&self) {
        MapEntry::delete(self);
    }

    // map.go:48
    fn locked(&self, f: &mut dyn FnMut(&dyn Value<T>)) {
        f(self);
    }
}

// map.go:57
pub fn new_map<K, T>(base: SharedMap<K, T>) -> Map<K, T> {
    Map {
        inner: Arc::new(MapInner {
            base: Mutex::new(base),
            dirty: Mutex::new(FxHashMap::default()),
        }),
    }
}

impl<K: Hash + Eq + Clone, T: Cloneable + Clone> Map<K, T> {
    fn base_entry(&self, key: K, value: Shared<T>) -> Arc<MapEntry<K, T>> {
        Arc::new(MapEntry {
            m: Arc::downgrade(&self.inner),
            key,
            st: Mutex::new(EntryState {
                original: Some(value.clone()),
                value: Some(value),
                dirty: false,
                delete: false,
            }),
        })
    }

    // map.go:64
    pub fn get(&self, key: &K) -> Option<Arc<MapEntry<K, T>>> {
        let dirty_entry = self.inner.dirty.lock().unwrap().get(key).cloned();
        if let Some(entry) = dirty_entry {
            if entry.st.lock().unwrap().delete {
                return None;
            }
            return Some(entry);
        }
        let value = self.inner.base.lock().unwrap().get(key).cloned()?;
        Some(self.base_entry(key.clone(), value))
    }

    // map.go:89
    // Add sets a new entry in the dirty map without checking if it exists
    // in the base map. The entry added is considered dirty, so it should
    // be a fresh value, mutable until finalized (i.e., it will not be cloned
    // before changing if a change is made). If modifying an entry that may
    // exist in the base map, use `Change` instead.
    pub fn add(&self, key: K, value: Shared<T>) {
        let entry = Arc::new(MapEntry {
            m: Arc::downgrade(&self.inner),
            key: key.clone(),
            st: Mutex::new(EntryState {
                original: None,
                value: Some(value),
                dirty: true,
                delete: false,
            }),
        });
        self.inner.dirty.lock().unwrap().insert(key, entry);
    }

    // map.go:106
    pub fn try_delete(&self, key: &K) -> bool {
        if let Some(entry) = self.get(key) {
            entry.delete();
            return true;
        }
        false
    }

    // map.go:114
    pub fn delete(&self, key: &K) {
        assert!(self.try_delete(key), "tried to delete a non-existent entry");
    }

    // map.go:120
    pub fn range(&self, mut f: impl FnMut(&Arc<MapEntry<K, T>>) -> bool) {
        let mut seen_in_dirty: FxHashSet<K> = FxHashSet::default();
        let dirty: Vec<Arc<MapEntry<K, T>>> =
            self.inner.dirty.lock().unwrap().values().cloned().collect();
        for entry in &dirty {
            seen_in_dirty.insert(entry.key.clone());
            let deleted = entry.st.lock().unwrap().delete;
            if !deleted && !f(entry) {
                break;
            }
        }
        let base = self.inner.base.lock().unwrap().clone();
        #[expect(
            clippy::iter_over_hash_type,
            reason = "unspecified order, as in Go: Range ranges Go maps, so no caller can rely on an order"
        )]
        for (key, value) in base.iter() {
            if seen_in_dirty.contains(key) {
                continue; // already processed in dirty entries
            }
            if !f(&self.base_entry(key.clone(), value.clone())) {
                break;
            }
        }
    }

    // map.go:144
    pub fn clear(&self) {
        *self.inner.dirty.lock().unwrap() = FxHashMap::default();
        *self.inner.base.lock().unwrap() = Arc::default();
    }

    // map.go:149
    pub fn finalize(&self) -> (SharedMap<K, T>, bool) {
        let dirty = self.inner.dirty.lock().unwrap();
        let base = self.inner.base.lock().unwrap().clone();
        if dirty.is_empty() {
            return (base, false); // no changes, return base map
        }
        let mut result: FxHashMap<K, Shared<T>> = (*base).clone();
        #[expect(clippy::iter_over_hash_type, reason = "inserts or removes distinct keys")]
        for (key, entry) in dirty.iter() {
            let st = entry.st.lock().unwrap();
            if st.delete {
                result.remove(key);
            } else {
                result.insert(key.clone(), st.value.clone().unwrap());
            }
        }
        (Arc::new(result), true)
    }
}
