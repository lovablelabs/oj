use std::hash::Hash;

use rustc_hash::FxHashMap;

use super::interfaces::Cloneable;

// Go `CloneableMap[K, V] map[K]V`. Stored behind `Shared` in dirty maps (Go maps are reference values).
#[derive(Clone, Debug, Default)]
pub struct CloneableMap<K: Hash + Eq, V>(pub FxHashMap<K, V>);

impl<K: Hash + Eq + Clone, V: Clone> Cloneable for CloneableMap<K, V> {
    // cloneablemap.go:7
    fn clone_value(&self) -> CloneableMap<K, V> {
        CloneableMap(self.0.clone())
    }
}

impl<K: Hash + Eq, V> std::ops::Deref for CloneableMap<K, V> {
    type Target = FxHashMap<K, V>;
    fn deref(&self) -> &FxHashMap<K, V> {
        &self.0
    }
}

impl<K: Hash + Eq, V> std::ops::DerefMut for CloneableMap<K, V> {
    fn deref_mut(&mut self) -> &mut FxHashMap<K, V> {
        &mut self.0
    }
}
