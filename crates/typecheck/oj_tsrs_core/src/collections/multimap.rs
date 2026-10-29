use rustc_hash::FxHashMap;
use std::hash::Hash;

#[derive(Clone, Debug)]
pub struct MultiMap<K: Hash + Eq, V> {
    pub m: FxHashMap<K, Vec<V>>,
}

impl<K: Hash + Eq, V> Default for MultiMap<K, V> {
    fn default() -> Self {
        MultiMap { m: FxHashMap::default() }
    }
}

impl<K: Hash + Eq, V> MultiMap<K, V> {
    pub fn get(&self, key: &K) -> &[V] {
        self.m.get(key).map_or(&[], std::vec::Vec::as_slice)
    }

    pub fn add(&mut self, key: K, value: V) {
        self.m.entry(key).or_default().push(value);
    }

    pub fn values(&self) -> impl Iterator<Item = &Vec<V>> {
        self.m.values()
    }
}
