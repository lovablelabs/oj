// Port of execute/incremental/referencemap.go.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use rustc_hash::FxHashMap;
use tsrs_core::collections::{Set, SyncMap};
use tsrs_core::tspath::Path;

/// A file's referenced paths (Go's `*collections.Set[tspath.Path]` in the reference map).
///
/// Every file references the files declaring its checker's ambient modules (~390 on the 38k-file codebase, ~15 M
/// entries over all files), so a set computed from the program keeps that part once per checker (`shared`) and only
/// its own paths per file (`own`, disjoint from `shared` minus `skip`). `skip` is the file itself when it declares an
/// ambient module: its own set does not reference it through that part. Sets read from a tsbuildinfo are flat.
/// Only set operations are offered (size, membership, unordered iteration, equality as sets).
#[derive(Default)]
pub struct RefSet {
    own: Set<Path>,
    shared: Option<Arc<Set<Path>>>,
    skip: Option<Path>,
}

impl RefSet {
    pub(crate) fn flat(own: Set<Path>) -> RefSet {
        RefSet { own, shared: None, skip: None }
    }

    /// `own` must not contain a path of `shared` other than `skip`; `skip`, if given, must be in `shared`.
    pub(crate) fn split(own: Set<Path>, shared: Arc<Set<Path>>, skip: Option<Path>) -> RefSet {
        RefSet { own, shared: Some(shared), skip }
    }

    pub(crate) fn len(&self) -> usize {
        self.own.len()
            + self
                .shared
                .as_ref()
                .map_or(0, |shared| shared.len() - usize::from(self.skip.is_some()))
    }

    pub(crate) fn has(&self, path: &Path) -> bool {
        self.own.has(path)
            || self
                .shared
                .as_ref()
                .is_some_and(|shared| shared.has(path) && self.skip.as_ref() != Some(path))
    }

    /// The file's own paths (all of them for a flat set).
    pub(crate) fn own(&self) -> &Set<Path> {
        &self.own
    }

    /// The shared part and the path left out of it, for a split set.
    pub(crate) fn shared(&self) -> Option<(&Arc<Set<Path>>, Option<&Path>)> {
        self.shared.as_ref().map(|shared| (shared, self.skip.as_ref()))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Path> {
        let skip = self.skip.as_ref();
        self.own.keys().iter().chain(
            self.shared
                .iter()
                .flat_map(|shared| shared.keys().iter())
                .filter(move |&path| Some(path) != skip),
        )
    }
}

impl PartialEq for RefSet {
    fn eq(&self, other: &RefSet) -> bool {
        self.len() == other.len() && self.iter().all(|path| other.has(path))
    }
}

#[derive(Default)]
pub struct ReferenceMap {
    references: SyncMap<Path, Arc<RefSet>>,
    // Go builds `referencedBy` once (`sync.Once`), from the references stored at the first query. Here that
    // snapshot is `groups`: the files grouped by the reference set they share (files read from a tsbuildinfo
    // share one set per id list). The first `SCAN_QUERIES` queries test each distinct set for the path; after
    // that the inverted map is built from the same snapshot. Either way the answer is the one Go's map gives.
    // A leaf edit asks once or twice, and inverting the ~15 M references of the 38k-file codebase costs ~1 s.
    groups: OnceLock<Vec<(Arc<RefSet>, Vec<Path>)>>,
    queries: AtomicUsize,
    // Referenced path -> indices into `groups` of the sets that contain it.
    referenced_by: OnceLock<FxHashMap<Path, Vec<u32>>>,
}

const SCAN_QUERIES: usize = 256;

impl ReferenceMap {
    // referencemap.go:18, for a set that other entries share.
    pub(crate) fn store_shared_references(&self, path: Path, refs: Arc<RefSet>) {
        self.references.store(path, refs);
    }

    // referencemap.go:22
    pub fn get_references(&self, path: &Path) -> Option<Arc<RefSet>> {
        self.references.load(path)
    }

    // referencemap.go:27
    pub(crate) fn get_paths_with_references(&self) -> Vec<Path> {
        self.references.keys()
    }

    // referencemap.go:31. Unordered, like Go's `maps.Keys`; every caller sorts it.
    pub(crate) fn get_referenced_by(&self, path: &Path) -> Vec<Path> {
        let groups = self.groups.get_or_init(|| {
            let mut index: FxHashMap<*const RefSet, usize> = FxHashMap::default();
            let mut groups: Vec<(Arc<RefSet>, Vec<Path>)> = Vec::new();
            self.references.range(|key, value| {
                let i = *index.entry(Arc::as_ptr(value)).or_insert_with(|| {
                    groups.push((Arc::clone(value), Vec::new()));
                    groups.len() - 1
                });
                groups[i].1.push(key.clone());
                true
            });
            groups
        });
        let mut result = Vec::new();
        if self.referenced_by.get().is_none()
            && self.queries.fetch_add(1, Ordering::Relaxed) < SCAN_QUERIES
        {
            result.extend(
                groups
                    .iter()
                    .filter_map(
                        |(refs, owners)| {
                            if refs.has(path) { Some(owners.iter().cloned()) } else { None }
                        },
                    )
                    .flatten(),
            );
            return result;
        }
        let referenced_by = self.referenced_by.get_or_init(|| {
            let mut referenced_by: FxHashMap<Path, Vec<u32>> = FxHashMap::default();
            for (i, (refs, _)) in groups.iter().enumerate() {
                for r in refs.iter() {
                    match referenced_by.get_mut(r) {
                        Some(sets) => sets.push(i as u32),
                        None => {
                            referenced_by.insert(r.clone(), vec![i as u32]);
                        }
                    }
                }
            }
            referenced_by
        });
        if let Some(sets) = referenced_by.get(path) {
            result.extend(sets.iter().flat_map(|&i| groups[i as usize].1.iter().cloned()));
        }
        result
    }
}
