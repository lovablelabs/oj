// SPDX-License-Identifier: MIT

//! Files' content hashes, in one map. Each file has the hash last read and,
//! per consumer, the hash it last compared against (its baseline): the HMR
//! gate's is the content the pages have, Start's the content it last pushed
//! to the worker environments. One read serves every consumer.
//!
//! The current hash is always a read of the bytes, never inferred from a
//! stat: a same-size write inside one mtime tick (a class name swapped for
//! one of equal length) keeps the stat, and a stale hash would take a real
//! edit for a rewrite of the same content.
//!
//! What the pages have is only known for sure at the server: a page shows
//! bytes the server served it. So each file also keeps the contents served
//! since its gate baseline was settled. A page may hold any of them, and
//! a rewrite counts as no change only when every one of them, and the
//! baseline, is the content on disk. Serving can never make a write look
//! like no change; it can only keep a no-op from being dropped.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// A content hash of `bytes`, the one [`file_content_hash`] takes of a file.
pub fn bytes_hash(bytes: &[u8]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

/// A file's content hash (None: missing or unreadable).
pub fn file_content_hash(p: &Path) -> Option<u64> {
    std::fs::read(p).ok().map(|bytes| bytes_hash(&bytes))
}

/// What a consumer compares a file's content against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Baseline {
    /// The HMR gate: the content the pages have, as its last flush released
    /// it or a batch it let through brought it, and nothing else served
    /// since (see the module docs).
    Flushed,
    /// Start: the content last pushed to the worker environments.
    Pushed,
}

/// Distinct contents served since the gate baseline, before the set gives
/// up and counts as "anything" (a file served in this many versions
/// between two flushes is changing, not being rewritten).
const SERVED_MAX: usize = 4;

#[derive(Default, Clone)]
struct Served {
    /// Distinct contents served since the gate baseline was settled.
    since_baseline: Vec<u64>,
    /// More than [`SERVED_MAX`] of them: a page may hold any content.
    overflowed: bool,
    /// The last content served, for a cache hit that serves it again.
    last: Option<u64>,
}

impl Served {
    fn add(&mut self, hash: u64) {
        self.last = Some(hash);
        if self.overflowed || self.since_baseline.contains(&hash) {
            return;
        }
        if self.since_baseline.len() < SERVED_MAX {
            self.since_baseline.push(hash);
        } else {
            self.overflowed = true;
        }
    }

    /// Every content served since the baseline is `hash`.
    fn only(&self, hash: u64) -> bool {
        !self.overflowed && self.since_baseline.iter().all(|h| *h == hash)
    }

    fn reset(&mut self) {
        self.since_baseline.clear();
        self.overflowed = false;
    }
}

#[derive(Default, Clone)]
struct FileContent {
    current: Option<Option<u64>>,
    flushed: Option<Option<u64>>,
    pushed: Option<Option<u64>>,
    served: Served,
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
    /// Settling the gate's baseline starts its served set over: the pages
    /// are being brought to this content.
    pub fn settle(&self, path: &Path, b: Baseline) -> Option<u64> {
        let hash = file_content_hash(path);
        let mut files = self.lock();
        let file = files.entry(path.to_path_buf()).or_default();
        file.current = Some(hash);
        *file.slot(b) = Some(hash);
        if b == Baseline::Flushed {
            file.served.reset();
        }
        hash
    }

    /// The server served `path` with content `hash` (the bytes it read for
    /// the module): some page may now show them.
    pub fn served(&self, path: &Path, hash: u64) {
        self.lock()
            .entry(path.to_path_buf())
            .or_default()
            .served
            .add(hash);
    }

    /// The server served `path` again from a cache of its last serve (same
    /// stat, so the same bytes): counted as that content again. False when
    /// nothing was served before, which the caller then reads to record.
    pub fn served_again(&self, path: &Path) -> bool {
        let mut files = self.lock();
        let Some(file) = files.get_mut(path) else {
            return false;
        };
        match file.served.last {
            Some(hash) => {
                file.served.add(hash);
                true
            }
            None => false,
        }
    }

    /// Whether `path` reads as what `b` last settled on. A file `b` never
    /// settled, or settled while missing, is never unchanged. For the gate,
    /// every content served since its baseline must be that content too: a
    /// page that fetched other bytes still needs this write.
    pub fn unchanged_since(&self, path: &Path, b: Baseline) -> bool {
        let Some(Some(last)) = self.baseline(path, b) else {
            return false;
        };
        if self.read(path) != Some(last) {
            return false;
        }
        b != Baseline::Flushed || self.lock().get(path).is_some_and(|f| f.served.only(last))
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

    #[test]
    fn bytes_hash_is_the_file_hash_of_the_same_bytes() {
        let d = dir("bytes-hash");
        let f = d.join("x.js");
        std::fs::write(&f, "export const x = 1;\n").unwrap();
        assert_eq!(
            file_content_hash(&f),
            Some(bytes_hash(b"export const x = 1;\n"))
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_page_that_fetched_other_bytes_keeps_a_rewrite_of_the_baseline_a_change() {
        let d = dir("served-other");
        let f = d.join("color.js");
        std::fs::write(&f, "X").unwrap();
        let index = ContentIndex::default();
        index.settle(&f, Baseline::Flushed);
        // W landed and a page fetched it before the next flush.
        index.served(&f, bytes_hash(b"W"));
        // Back to X on disk: the page showing W still needs it.
        std::fs::write(&f, "X").unwrap();
        assert!(!index.unchanged_since(&f, Baseline::Flushed));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn serving_the_baseline_itself_keeps_a_rewrite_a_no_op() {
        let d = dir("served-same");
        let f = d.join("routeTree.gen.ts");
        std::fs::write(&f, "X").unwrap();
        let index = ContentIndex::default();
        index.settle(&f, Baseline::Flushed);
        index.served(&f, bytes_hash(b"X"));
        assert!(
            index.served_again(&f),
            "a cache hit serves the last content again"
        );
        std::fs::write(&f, "X").unwrap();
        assert!(index.unchanged_since(&f, Baseline::Flushed));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn settling_the_gate_baseline_starts_the_served_set_over() {
        let d = dir("served-reset");
        let f = d.join("color.js");
        std::fs::write(&f, "X").unwrap();
        let index = ContentIndex::default();
        index.settle(&f, Baseline::Flushed);
        index.served(&f, bytes_hash(b"W"));
        // The pages are brought to Y (a flush, or a batch let through).
        std::fs::write(&f, "Y").unwrap();
        index.settle(&f, Baseline::Flushed);
        std::fs::write(&f, "Y").unwrap();
        assert!(
            index.unchanged_since(&f, Baseline::Flushed),
            "W predates the release of Y"
        );
        // Start's baseline never resets the gate's served set.
        index.served(&f, bytes_hash(b"Z"));
        index.settle(&f, Baseline::Pushed);
        assert!(!index.unchanged_since(&f, Baseline::Flushed));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn many_served_versions_count_as_anything() {
        let d = dir("served-overflow");
        let f = d.join("color.js");
        std::fs::write(&f, "X").unwrap();
        let index = ContentIndex::default();
        index.settle(&f, Baseline::Flushed);
        for n in 0..=SERVED_MAX {
            index.served(&f, bytes_hash(format!("v{n}").as_bytes()));
        }
        // The set gave up: even serving X again cannot make X a no-op.
        index.served(&f, bytes_hash(b"X"));
        assert!(!index.unchanged_since(&f, Baseline::Flushed));
        // A release starts it over.
        index.settle(&f, Baseline::Flushed);
        assert!(index.unchanged_since(&f, Baseline::Flushed));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn served_matters_to_the_gate_only() {
        let d = dir("served-pushed");
        let f = d.join("routeTree.gen.ts");
        std::fs::write(&f, "X").unwrap();
        let index = ContentIndex::default();
        index.settle(&f, Baseline::Pushed);
        index.served(&f, bytes_hash(b"W"));
        assert!(index.unchanged_since(&f, Baseline::Pushed));
        assert!(!index.served_again(&d.join("never-served.ts")));
        let _ = std::fs::remove_dir_all(&d);
    }
}
