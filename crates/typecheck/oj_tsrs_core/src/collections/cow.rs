use rustc_hash::FxHashMap;
use std::hash::Hash;
use std::rc::Rc;

// CopyOnWriteMap is a map that defers cloning of an inherited backing map
// until the first mutation, and supports nested scopes that share the parent's
// map for reads but get their own clone on write.
//
// The zero value is an empty map ready to use.
//
// tsrs-only: the backing map is allocated at the first `set`, so an empty map (and every scope entered while it is
// still empty) costs no heap allocation. A node builder context holds four of these and most contexts never write
// to them; the contexts made while checking live in the thread's leak arena, so an eager `Rc` per map was never freed.
#[derive(Clone, Debug)]
pub struct CopyOnWriteMap<K: Hash + Eq + Clone, V: Clone> {
    m: Option<Rc<FxHashMap<K, V>>>,
}

impl<K: Hash + Eq + Clone, V: Clone> Default for CopyOnWriteMap<K, V> {
    fn default() -> Self {
        CopyOnWriteMap { m: None }
    }
}

/// Saved state returned by `enter_scope`; pass it to `exit_scope` to restore the map.
pub struct CopyOnWriteScope<K: Hash + Eq + Clone, V: Clone> {
    saved: Option<Rc<FxHashMap<K, V>>>,
}

impl<K: Hash + Eq + Clone, V: Clone> CopyOnWriteMap<K, V> {
    pub fn get(&self, k: &K) -> Option<&V> {
        self.m.as_ref()?.get(k)
    }

    pub fn has(&self, k: &K) -> bool {
        self.m.as_ref().is_some_and(|m| m.contains_key(k))
    }

    // Set assigns v to k, cloning the inherited backing map first if necessary.
    pub fn set(&mut self, k: K, v: V) {
        Rc::make_mut(self.m.get_or_insert_with(Rc::default)).insert(k, v);
    }

    // EnterScope returns a value that restores this map to its current state.
    // While the scope is active, the map shares its current backing storage with
    // the parent scope: reads see the inherited entries, and the first mutation
    // transparently clones the storage so the parent's view is not modified.
    pub fn enter_scope(&self) -> CopyOnWriteScope<K, V> {
        CopyOnWriteScope { saved: self.m.clone() }
    }

    pub fn exit_scope(&mut self, scope: CopyOnWriteScope<K, V>) {
        self.m = scope.saved;
    }
}

#[derive(Clone, Debug)]
pub struct CopyOnWriteSet<K: Hash + Eq + Clone> {
    m: CopyOnWriteMap<K, ()>,
}

impl<K: Hash + Eq + Clone> Default for CopyOnWriteSet<K> {
    fn default() -> Self {
        CopyOnWriteSet { m: CopyOnWriteMap::default() }
    }
}

impl<K: Hash + Eq + Clone> CopyOnWriteSet<K> {
    pub fn has(&self, k: &K) -> bool {
        self.m.has(k)
    }

    pub fn add(&mut self, k: K) {
        self.m.set(k, ());
    }

    pub fn enter_scope(&self) -> CopyOnWriteScope<K, ()> {
        self.m.enter_scope()
    }

    pub fn exit_scope(&mut self, scope: CopyOnWriteScope<K, ()>) {
        self.m.exit_scope(scope);
    }
}
