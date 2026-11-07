use std::cell::RefCell;
use std::hash::Hash;
use std::sync::{Arc, Mutex, Weak};

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_core::collections::SyncMap as LockedMap;

use super::entry::EntryState;
use super::interfaces::{Cloneable, Shared, Value};
use super::map::SharedMap;

struct LockedEntry<'a, K: Hash + Eq, T> {
    e: &'a Arc<SyncMapEntry<K, T>>,
    st: RefCell<&'a mut SyncEntryState<K, T>>,
}

impl<K: Hash + Eq + Clone + Send + Sync, T: Cloneable + Clone> Value<T> for LockedEntry<'_, K, T> {
    // syncmap.go:14
    fn value(&self) -> Option<Shared<T>> {
        self.st.borrow().e.value()
    }

    // syncmap.go:18
    fn original(&self) -> Option<Shared<T>> {
        self.st.borrow().e.original()
    }

    // syncmap.go:22
    fn dirty(&self) -> bool {
        self.st.borrow().e.dirty
    }

    // syncmap.go:26
    fn change(&self, apply: &mut dyn FnMut(&mut T)) {
        self.e.change_locked(&mut self.st.borrow_mut(), apply);
    }

    // syncmap.go:30
    fn change_if(&self, cond: &mut dyn FnMut(&T) -> bool, apply: &mut dyn FnMut(&mut T)) -> bool {
        let value = self.st.borrow().e.value();
        if let Some(value) = value
            && cond(&value)
        {
            drop(value);
            self.e.change_locked(&mut self.st.borrow_mut(), apply);
            return true;
        }
        false
    }

    // syncmap.go:38
    fn delete(&self) {
        self.e.delete_locked(&mut self.st.borrow_mut());
    }

    // syncmap.go:42
    fn locked(&self, f: &mut dyn FnMut(&dyn Value<T>)) {
        f(self);
    }
}

pub struct SyncMapEntry<K: Hash + Eq, T> {
    m: Weak<SyncMapInner<K, T>>,
    key: K,
    mu: Mutex<SyncEntryState<K, T>>,
}

struct SyncEntryState<K: Hash + Eq, T> {
    e: EntryState<T>,
    // proxyFor is set when this entry loses a race to become the dirty entry
    // for a value. Since two goroutines hold a reference to two entries that
    // may try to mutate the same underlying value, all mutations are routed
    // through the one that actually exists in the dirty map.
    proxy_for: Option<Arc<SyncMapEntry<K, T>>>,
}

impl<K: Hash + Eq + Clone + Send + Sync, T: Cloneable + Clone> SyncMapEntry<K, T> {
    pub fn key(&self) -> K {
        self.key.clone()
    }

    pub fn proxy_for(&self) -> Option<Arc<SyncMapEntry<K, T>>> {
        self.mu.lock().unwrap().proxy_for.clone()
    }

    // syncmap.go:57
    pub fn value(self: &Arc<Self>) -> Option<Shared<T>> {
        let st = self.mu.lock().unwrap();
        if let Some(proxy_for) = &st.proxy_for {
            return proxy_for.value();
        }
        st.e.value()
    }

    fn original(&self) -> Option<Shared<T>> {
        self.mu.lock().unwrap().e.original()
    }

    // syncmap.go:74
    fn dirty(self: &Arc<Self>) -> bool {
        let st = self.mu.lock().unwrap();
        if let Some(proxy_for) = &st.proxy_for {
            return proxy_for.dirty();
        }
        st.e.dirty
    }

    // syncmap.go:83
    pub fn locked(self: &Arc<Self>, f: &mut dyn FnMut(&dyn Value<T>)) {
        let mut st = self.mu.lock().unwrap();
        if let Some(proxy_for) = st.proxy_for.clone() {
            proxy_for.locked(f);
            return;
        }
        f(&LockedEntry { e: self, st: RefCell::new(&mut st) });
    }

    // syncmap.go:93
    pub fn change(self: &Arc<Self>, apply: impl FnOnce(&mut T)) {
        let mut apply = Some(apply);
        self.change_dyn(&mut |v| (apply.take().unwrap())(v));
    }

    fn change_dyn(self: &Arc<Self>, apply: &mut dyn FnMut(&mut T)) {
        let mut st = self.mu.lock().unwrap();
        if let Some(proxy_for) = st.proxy_for.clone() {
            proxy_for.change_dyn(apply);
            return;
        }
        self.change_locked(&mut st, apply);
    }

    // syncmap.go:103
    fn change_locked(
        self: &Arc<Self>,
        st: &mut SyncEntryState<K, T>,
        apply: &mut dyn FnMut(&mut T),
    ) {
        if st.e.dirty {
            apply(Shared::make_mut(st.e.value.as_mut().unwrap()));
            return;
        }

        let m = self.m.upgrade().expect("dirty.SyncMap dropped while an entry is in use");
        let (entry, loaded) = m.dirty.load_or_store(self.key.clone(), Arc::clone(self));
        if loaded {
            let mut es = entry.mu.lock().unwrap();
            if !es.e.dirty {
                es.e.value = Some(es.e.value.as_ref().unwrap().clone_shared());
                es.e.dirty = true;
            }
            // Go also copies `entry.value` into this entry; every access is routed through proxyFor, so the copy
            // is not kept (it would only force copy-on-write on later changes).
            st.proxy_for = Some(Arc::clone(&entry));
            st.e.dirty = true;
            st.e.delete = es.e.delete;
            apply(Shared::make_mut(es.e.value.as_mut().unwrap()));
            return;
        }
        st.e.value = Some(st.e.value.as_ref().unwrap().clone_shared());
        st.e.dirty = true;
        apply(Shared::make_mut(st.e.value.as_mut().unwrap()));
    }

    // syncmap.go:127
    pub fn change_if(
        self: &Arc<Self>,
        cond: impl FnOnce(&T) -> bool,
        apply: impl FnOnce(&mut T),
    ) -> bool {
        let mut cond = Some(cond);
        let mut apply = Some(apply);
        self.change_if_dyn(&mut |v| (cond.take().unwrap())(v), &mut |v| {
            (apply.take().unwrap())(v);
        })
    }

    fn change_if_dyn(
        self: &Arc<Self>,
        cond: &mut dyn FnMut(&T) -> bool,
        apply: &mut dyn FnMut(&mut T),
    ) -> bool {
        let mut st = self.mu.lock().unwrap();
        if let Some(proxy_for) = st.proxy_for.clone() {
            return proxy_for.change_if_dyn(cond, apply);
        }

        let value = st.e.value.clone();
        if let Some(value) = value
            && cond(&value)
        {
            drop(value);
            self.change_locked(&mut st, apply);
            return true;
        }
        false
    }

    // syncmap.go:141
    pub fn delete(self: &Arc<Self>) {
        let mut st = self.mu.lock().unwrap();
        if let Some(proxy_for) = st.proxy_for.clone() {
            proxy_for.delete();
            return;
        }

        if st.e.dirty {
            st.e.delete = true;
            return;
        }
        let m = self.m.upgrade().expect("dirty.SyncMap dropped while an entry is in use");
        let (entry, loaded) = m.dirty.load_or_store(self.key.clone(), Arc::clone(self));
        if loaded {
            let _es = entry.mu.lock().unwrap();
            st.e.delete = true;
        } else {
            st.e.delete = true;
        }
    }

    // syncmap.go:163
    fn delete_locked(self: &Arc<Self>, st: &mut SyncEntryState<K, T>) {
        if st.e.dirty {
            st.e.delete = true;
            return;
        }
        let m = self.m.upgrade().expect("dirty.SyncMap dropped while an entry is in use");
        let (entry, loaded) = m.dirty.load_or_store(self.key.clone(), Arc::clone(self));
        if loaded {
            let mut es = entry.mu.lock().unwrap();
            st.proxy_for = Some(Arc::clone(&entry));
            st.e.value.clone_from(&es.e.value);
            st.e.delete = true;
            st.e.dirty = es.e.dirty;
            es.e.delete = true;
            return;
        }
        st.e.delete = true;
    }

    // syncmap.go:180
    pub fn delete_if(self: &Arc<Self>, cond: impl FnOnce(&T) -> bool) {
        let mut cond = Some(cond);
        self.delete_if_dyn(&mut |v| (cond.take().unwrap())(v));
    }

    fn delete_if_dyn(self: &Arc<Self>, cond: &mut dyn FnMut(&T) -> bool) {
        let mut st = self.mu.lock().unwrap();
        if let Some(proxy_for) = st.proxy_for.clone() {
            proxy_for.delete_if_dyn(cond);
            return;
        }
        let value = st.e.value.clone();
        if let Some(value) = value
            && cond(&value)
        {
            drop(value);
            self.delete_locked(&mut st);
        }
    }
}

// Go passes `*SyncMapEntry` where a `Value[V]` is expected.
impl<K: Hash + Eq + Clone + Send + Sync, T: Cloneable + Clone> Value<T>
    for Arc<SyncMapEntry<K, T>>
{
    fn value(&self) -> Option<Shared<T>> {
        SyncMapEntry::value(self)
    }

    fn original(&self) -> Option<Shared<T>> {
        SyncMapEntry::original(self)
    }

    fn dirty(&self) -> bool {
        SyncMapEntry::dirty(self)
    }

    fn change(&self, apply: &mut dyn FnMut(&mut T)) {
        self.change_dyn(apply);
    }

    fn change_if(&self, cond: &mut dyn FnMut(&T) -> bool, apply: &mut dyn FnMut(&mut T)) -> bool {
        self.change_if_dyn(cond, apply)
    }

    fn delete(&self) {
        SyncMapEntry::delete(self);
    }

    fn locked(&self, f: &mut dyn FnMut(&dyn Value<T>)) {
        SyncMapEntry::locked(self, f);
    }
}

pub struct SyncMap<K: Hash + Eq, T> {
    inner: Arc<SyncMapInner<K, T>>,
}

struct SyncMapInner<K: Hash + Eq, T> {
    base: SharedMap<K, T>,
    dirty: LockedMap<K, Arc<SyncMapEntry<K, T>>>,
}

// syncmap.go:197
pub fn new_sync_map<K: Hash + Eq, T>(base: SharedMap<K, T>) -> SyncMap<K, T> {
    SyncMap { inner: Arc::new(SyncMapInner { base, dirty: LockedMap::default() }) }
}

pub struct FinalizationHooks<'a, K, T> {
    pub on_delete: Option<&'a mut dyn FnMut(&K, Option<&Shared<T>>)>,
    pub on_change: Option<&'a mut dyn FnMut(&K, Option<&Shared<T>>, Option<&Shared<T>>)>,
    pub on_add: Option<&'a mut dyn FnMut(&K, Option<&Shared<T>>)>,
}

impl<K, T> Default for FinalizationHooks<'_, K, T> {
    fn default() -> Self {
        FinalizationHooks { on_delete: None, on_change: None, on_add: None }
    }
}

impl<K: Hash + Eq + Clone + Send + Sync, T: Cloneable + Clone> SyncMap<K, T> {
    fn new_entry(
        &self,
        key: K,
        original: Option<Shared<T>>,
        value: Option<Shared<T>>,
        dirty: bool,
        delete: bool,
    ) -> Arc<SyncMapEntry<K, T>> {
        Arc::new(SyncMapEntry {
            m: Arc::downgrade(&self.inner),
            key,
            mu: Mutex::new(SyncEntryState {
                e: EntryState { original, value, dirty, delete },
                proxy_for: None,
            }),
        })
    }

    // syncmap.go:204
    pub fn load(&self, key: &K) -> Option<Arc<SyncMapEntry<K, T>>> {
        if let Some(entry) = self.inner.dirty.load(key) {
            if entry.mu.lock().unwrap().e.delete {
                return None;
            }
            return Some(entry);
        }
        if let Some(val) = self.inner.base.get(key) {
            return Some(self.new_entry(
                key.clone(),
                Some(val.clone()),
                Some(val.clone()),
                false,
                false,
            ));
        }
        None
    }

    // syncmap.go:226
    pub fn load_or_store(
        &self,
        key: K,
        value: Shared<T>,
    ) -> (Option<Arc<SyncMapEntry<K, T>>>, bool) {
        // Check for existence in the base map first so the sync map access is atomic.
        if let Some(base_value) = self.inner.base.get(&key) {
            if let Some(dirty) = self.inner.dirty.load(&key) {
                if dirty.mu.lock().unwrap().e.delete {
                    return (None, false);
                }
                return (Some(dirty), true);
            }
            return (
                Some(self.new_entry(
                    key,
                    Some(base_value.clone()),
                    Some(base_value.clone()),
                    false,
                    false,
                )),
                true,
            );
        }
        let new_entry = self.new_entry(key.clone(), None, Some(value), true, false);
        let (entry, loaded) = self.inner.dirty.load_or_store(key, new_entry);
        if loaded && entry.mu.lock().unwrap().e.delete {
            return (None, false);
        }
        (Some(entry), loaded)
    }

    // syncmap.go:274
    pub fn range(&self, mut f: impl FnMut(&Arc<SyncMapEntry<K, T>>) -> bool) {
        let mut seen_in_dirty: FxHashSet<K> = FxHashSet::default();
        self.inner.dirty.range(|key, entry| {
            seen_in_dirty.insert(key.clone());
            let deleted = entry.mu.lock().unwrap().e.delete;
            if !deleted && !f(entry) {
                return false;
            }
            true
        });
        // Go continues with the base map even when the callback stopped the dirty map's Range.
        #[expect(
            clippy::iter_over_hash_type,
            reason = "unspecified order, as in Go: Range ranges Go maps, so no caller can rely on an order"
        )]
        for (key, value) in self.inner.base.iter() {
            if seen_in_dirty.contains(key) {
                continue; // already processed in dirty entries
            }
            if !f(&self.new_entry(
                key.clone(),
                Some(value.clone()),
                Some(value.clone()),
                false,
                false,
            )) {
                break;
            }
        }
    }

    // syncmap.go:308
    fn finalize_with_hooks(
        &self,
        mut hooks: FinalizationHooks<'_, K, T>,
    ) -> (SharedMap<K, T>, bool) {
        let mut changed = false;
        let mut result: Option<FxHashMap<K, Shared<T>>> = None;
        let base = &self.inner.base;

        self.inner.dirty.range(|key, entry| {
            let st = entry.mu.lock().unwrap();
            if st.e.delete {
                let result = result.get_or_insert_with(|| (**base).clone());
                changed = true;
                if let Some(on_delete) = hooks.on_delete.as_mut() {
                    on_delete(key, st.e.value.as_ref());
                }
                result.remove(key);
            } else if st.e.dirty {
                let result = result.get_or_insert_with(|| (**base).clone());
                changed = true;
                if hooks.on_change.is_some() || hooks.on_add.is_some() {
                    if base.contains_key(key) {
                        if let Some(on_change) = hooks.on_change.as_mut() {
                            on_change(key, st.e.original.as_ref(), st.e.value.as_ref());
                        }
                    } else if let Some(on_add) = hooks.on_add.as_mut() {
                        on_add(key, st.e.value.as_ref());
                    }
                }
                result.insert(key.clone(), st.e.value.clone().unwrap());
            }
            true
        });
        match result {
            Some(result) => (Arc::new(result), changed),
            None => (Arc::clone(base), changed),
        }
    }

    // syncmap.go:349
    pub fn finalize(&self) -> (SharedMap<K, T>, bool) {
        self.finalize_with_hooks(FinalizationHooks::default())
    }

    // syncmap.go:353
    pub fn finalize_with(&self, hooks: FinalizationHooks<'_, K, T>) -> (SharedMap<K, T>, bool) {
        self.finalize_with_hooks(hooks)
    }
}
