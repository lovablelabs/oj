use super::SyncMap;
use std::hash::Hash;

#[derive(Debug)]
pub struct SyncSet<T: Hash + Eq> {
    m: SyncMap<T, ()>,
}

impl<T: Hash + Eq> Default for SyncSet<T> {
    fn default() -> Self {
        SyncSet { m: SyncMap::default() }
    }
}

impl<T: Hash + Eq + Clone> SyncSet<T> {
    pub fn has(&self, key: &T) -> bool {
        self.m.load(key).is_some()
    }

    pub fn add(&self, key: T) {
        self.m.store(key, ());
    }

    // AddIfAbsent returns true if the key was added, false if it already existed.
    pub fn add_if_absent(&self, key: T) -> bool {
        let ((), loaded) = self.m.load_or_store(key, ());
        !loaded
    }

    pub fn delete(&self, key: &T) {
        self.m.delete(key);
    }

    pub fn range(&self, mut f: impl FnMut(&T) -> bool) {
        self.m.range(|k, ()| f(k));
    }

    pub fn size(&self) -> usize {
        self.m.size()
    }

    pub fn is_empty(&self) -> bool {
        self.m.size() == 0
    }

    pub fn to_slice(&self) -> Vec<T> {
        self.m.keys()
    }

    pub fn keys(&self) -> Vec<T> {
        self.m.keys()
    }
}
