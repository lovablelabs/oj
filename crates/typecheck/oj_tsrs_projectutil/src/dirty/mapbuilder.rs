use std::hash::Hash;
use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};

pub struct MapBuilder<K, VBase, VBuilder> {
    base: Arc<FxHashMap<K, VBase>>,
    dirty: FxHashMap<K, VBuilder>,
    deleted: Option<FxHashSet<K>>,

    build: Box<dyn Fn(VBuilder) -> VBase + Send + Sync>,
}

// mapbuilder.go:14
#[expect(
    clippy::implicit_hasher,
    reason = "The API deliberately accepts only FxHashMap, as required by the workspace hashing policy."
)]
pub fn new_map_builder<K, VBase, VBuilder>(
    base: Arc<FxHashMap<K, VBase>>,
    build: impl Fn(VBuilder) -> VBase + Send + Sync + 'static,
) -> MapBuilder<K, VBase, VBuilder> {
    MapBuilder { base, dirty: FxHashMap::default(), deleted: None, build: Box::new(build) }
}

impl<K: Hash + Eq + Clone, VBase: Clone, VBuilder: Clone> MapBuilder<K, VBase, VBuilder> {
    // mapbuilder.go:27
    pub fn set(&mut self, key: K, value: VBuilder) {
        if let Some(deleted) = &mut self.deleted {
            deleted.remove(&key);
        }
        self.dirty.insert(key, value);
    }

    // mapbuilder.go:32
    pub fn delete(&mut self, key: K) {
        self.dirty.remove(&key);
        self.deleted.get_or_insert_with(FxHashSet::default).insert(key);
    }

    // mapbuilder.go:40
    // copies the keys into a set
    pub fn clear(&mut self) {
        self.dirty = FxHashMap::default();
        let mut deleted =
            FxHashSet::with_capacity_and_hasher(self.base.len(), rustc_hash::FxBuildHasher);
        deleted.extend(self.base.keys().map(std::clone::Clone::clone));
        self.deleted = Some(deleted);
    }

    // mapbuilder.go:48
    pub fn has(&self, key: &K) -> bool {
        if self.deleted.as_ref().is_some_and(|d| d.contains(key)) {
            return false;
        }
        if self.dirty.contains_key(key) {
            return true;
        }
        self.base.contains_key(key)
    }

    // mapbuilder.go:59
    #[expect(clippy::iter_over_hash_type, reason = "removes, then inserts, distinct keys")]
    pub fn build(&self) -> Arc<FxHashMap<K, VBase>> {
        if self.dirty.is_empty() && self.deleted.as_ref().is_none_or(FxHashSet::is_empty) {
            return Arc::clone(&self.base);
        }
        let mut result: FxHashMap<K, VBase> = (*self.base).clone();
        if let Some(deleted) = &self.deleted {
            for key in deleted {
                result.remove(key);
            }
        }
        result.extend(
            self.dirty.iter().map(|(key, value)| (key.clone(), (self.build)(value.clone()))),
        );
        Arc::new(result)
    }
}
