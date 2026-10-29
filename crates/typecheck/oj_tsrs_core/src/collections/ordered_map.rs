use rustc_hash::FxBuildHasher;
use std::hash::Hash;

pub type OrderedMap<K, V> = indexmap::IndexMap<K, V, FxBuildHasher>;

pub fn new_ordered_map_with_size_hint<K, V>(hint: usize) -> OrderedMap<K, V> {
    OrderedMap::with_capacity_and_hasher(hint, FxBuildHasher)
}

pub fn diff_ordered_maps<K: Hash + Eq, V: PartialEq>(
    m1: &OrderedMap<K, V>,
    m2: &OrderedMap<K, V>,
    on_added: impl FnMut(&K, &V),
    on_removed: impl FnMut(&K, &V),
    on_modified: impl FnMut(&K, &V, &V),
) {
    diff_ordered_maps_func(m1, m2, |a, b| a == b, on_added, on_removed, on_modified);
}

fn diff_ordered_maps_func<K: Hash + Eq, V>(
    m1: &OrderedMap<K, V>,
    m2: &OrderedMap<K, V>,
    mut equal_values: impl FnMut(&V, &V) -> bool,
    mut on_added: impl FnMut(&K, &V),
    mut on_removed: impl FnMut(&K, &V),
    mut on_modified: impl FnMut(&K, &V, &V),
) {
    for (k, v2) in m2 {
        if !m1.contains_key(k) {
            on_added(k, v2);
        }
    }
    for (k, v1) in m1 {
        if let Some(v2) = m2.get(k) {
            if !equal_values(v1, v2) {
                on_modified(k, v1, v2);
            }
        } else {
            on_removed(k, v1);
        }
    }
}
