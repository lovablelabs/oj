// SPDX-License-Identifier: MIT

//! Files' content hashes, in one map. Each file has the hash last read and,
//! per consumer, the hash it last compared against (its baseline): the HMR
//! gate's is the content its last flush released, Start's the content it
//! last pushed to the worker environments. One read serves every consumer.
//!
//! The current hash is always a read of the bytes, never inferred from a
//! stat: a same-size write inside one mtime tick (a class name swapped for
//! one of equal length) keeps the stat, and a stale hash would take a real
//! edit for a rewrite of the same content.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A file's content hash (None: missing or unreadable).
pub fn file_content_hash(p: &Path) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    let bytes = std::fs::read(p).ok()?;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    Some(h.finish())
}

/// What a consumer compares a file's content against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Baseline {
    /// The HMR gate: the content the pages have, as its last flush released
    /// it or a batch it let through brought it.
    Flushed,
    /// Start: the content last pushed to the worker environments.
    Pushed,
}

#[derive(Default, Clone, Copy)]
struct FileContent {
    current: Option<Option<u64>>,
    flushed: Option<Option<u64>>,
    pushed: Option<Option<u64>>,
}

impl FileContent {
    fn slot(&mut self, b: Baseline) -> &mut Option<Option<u64>> {
        match b {
            Baseline::Flushed => &mut self.flushed,
            Baseline::Pushed => &mut self.pushed,
        }
    }
}

#[derive(Default)]
pub struct ContentIndex {
    files: Mutex<HashMap<PathBuf, FileContent>>,
}

impl ContentIndex {
    /// Read `path` now: its hash is the current content from here on.
    pub fn read(&self, path: &Path) -> Option<u64> {
        let hash = file_content_hash(path);
        self.lock().entry(path.to_path_buf()).or_default().current = Some(hash);
        hash
    }

    /// `b`'s baseline for `path`: None when never settled, `Some(None)` when
    /// settled while the file was missing.
    pub fn baseline(&self, path: &Path, b: Baseline) -> Option<Option<u64>> {
        self.lock().get_mut(path).and_then(|f| *f.slot(b))
    }

    /// `b`'s baseline takes `path`'s content as read now (also the current
    /// content), returned. Always a fresh read: a write landing after the
    /// last read must not leave the baseline older than what `b`'s consumer
    /// now has, or a later rewrite of the older bytes would read as no change.
    pub fn settle(&self, path: &Path, b: Baseline) -> Option<u64> {
        let hash = file_content_hash(path);
        let mut files = self.lock();
        let file = files.entry(path.to_path_buf()).or_default();
        file.current = Some(hash);
        *file.slot(b) = Some(hash);
        hash
    }

    /// Whether `path` reads as what `b` last settled on. A file `b` never
    /// settled, or settled while missing, is never unchanged.
    pub fn unchanged_since(&self, path: &Path, b: Baseline) -> bool {
        match self.baseline(path, b) {
            Some(Some(last)) => self.read(path) == Some(last),
            _ => false,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<PathBuf, FileContent>> {
        self.files
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(label: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("oj-content-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn each_consumer_compares_against_its_own_baseline_from_one_read() {
        let d = dir("baselines");
        let f = d.join("routeTree.gen.ts");
        std::fs::write(&f, "v1").unwrap();
        let index = ContentIndex::default();
        index.settle(&f, Baseline::Flushed);
        index.settle(&f, Baseline::Pushed);
        assert!(index.unchanged_since(&f, Baseline::Flushed));

        // One consumer moves on; the other keeps its baseline.
        std::fs::write(&f, "v2").unwrap();
        index.read(&f);
        index.settle(&f, Baseline::Pushed);
        assert!(!index.unchanged_since(&f, Baseline::Flushed));
        assert!(index.unchanged_since(&f, Baseline::Pushed));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_same_size_rewrite_is_read_not_inferred_from_the_stat() {
        let d = dir("same-size");
        let f = d.join("App.tsx");
        std::fs::write(&f, "bg-red-500").unwrap();
        let index = ContentIndex::default();
        index.settle(&f, Baseline::Flushed);
        // Same length, likely the same mtime tick: still a change.
        std::fs::write(&f, "bg-tan-500").unwrap();
        assert!(!index.unchanged_since(&f, Baseline::Flushed));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn settling_reads_the_file_now_not_the_last_read() {
        let d = dir("settle-fresh");
        let f = d.join("App.tsx");
        std::fs::write(&f, "A").unwrap();
        let index = ContentIndex::default();
        index.read(&f);
        // Written after the last read: the baseline is what is on disk now.
        std::fs::write(&f, "B").unwrap();
        index.settle(&f, Baseline::Flushed);
        assert!(index.unchanged_since(&f, Baseline::Flushed));
        std::fs::write(&f, "A").unwrap();
        assert!(
            !index.unchanged_since(&f, Baseline::Flushed),
            "A is older than what was settled"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn never_settled_or_missing_is_never_unchanged() {
        let d = dir("missing");
        let f = d.join("gone.ts");
        let index = ContentIndex::default();
        assert_eq!(index.baseline(&f, Baseline::Flushed), None);
        assert!(!index.unchanged_since(&f, Baseline::Flushed));
        // Settled while missing: recorded as such, and a missing file is no
        // rewrite of anything.
        index.settle(&f, Baseline::Flushed);
        assert_eq!(index.baseline(&f, Baseline::Flushed), Some(None));
        assert!(!index.unchanged_since(&f, Baseline::Flushed));
        let _ = std::fs::remove_dir_all(&d);
    }
}
