// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Eviction: the save-path generation window (background) and the boot-time
//! budget prune. Both evict oldest-first by last-use stamp, never touch a
//! protected generation (this root's `current`, any root's fresh pin), and
//! only delete blobs no surviving generation references.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{
    entry_size, is_generation_dir, read_manifest, StartBundleStore, BLOBS_DIR, LEGACY_POINTER_FILE,
    PINS_DIR, TOUCH_FILE,
};

/// A `.tmp-` entry younger than this is presumed live (a persist mid-copy,
/// an eviction thread's aside dir) and the boot sweep leaves it alone, as
/// Vite age-gates its `_temp_` cleanup. Older is a stranded crash leftover.
const TMP_SWEEP_MIN_AGE: Duration = Duration::from_secs(600);

fn tmp_is_stale(path: &Path) -> bool {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| m.elapsed().ok())
        .is_none_or(|age| age >= TMP_SWEEP_MIN_AGE)
}

/// Last-use time of a generation: its touch file, else the dir's own mtime.
pub(super) fn generation_stamp(path: &Path) -> SystemTime {
    fs::metadata(path.join(TOUCH_FILE))
        .or_else(|_| fs::metadata(path))
        .and_then(|m| m.modified())
        .unwrap_or(UNIX_EPOCH)
}

/// Blob hashes a generation's manifest references (empty when unreadable).
fn manifest_hashes(entry: &Path) -> Vec<String> {
    read_manifest(entry)
        .map(|m| m.files.into_values().map(|f| f.hash).collect())
        .unwrap_or_default()
}

impl StartBundleStore {
    pub fn prune(&self, budget_bytes: u64) {
        self.prune_with(budget_bytes, self.keep_generations)
    }

    /// The save-path half of eviction. Like Vite's dep-cache commit (rename +
    /// background rm), but the whole pass (listing, renames-aside, blob
    /// sweep) runs on a detached thread so the save path pays one spawn.
    /// Safe because: it never touches a protected generation, a mid-sweep crash strands
    /// only `.tmp-` dirs the boot prune age-gates away, and a sweep racing the
    /// next persist is covered by persist's post-commit blob re-check. Each
    /// thread joins its predecessor first, so sweeps never stack and
    /// [`Self::join_eviction`] joins them all.
    pub(super) fn enforce_window(&self, max_generations: usize) {
        let prev = self.sweeper.lock().ok().and_then(|mut s| s.take());
        let dir = self.dir.clone();
        let protected = self.protected_keys();
        let handle = std::thread::spawn(move || {
            if let Some(prev) = prev {
                let _ = prev.join();
            }
            enforce_window_blocking(&dir, max_generations, &protected);
        });
        if let Ok(mut slot) = self.sweeper.lock() {
            *slot = Some(handle);
        }
    }

    /// Blocks until every eviction thread this store spawned has finished.
    /// Tests use it for determinism; production callers never need to.
    pub fn join_eviction(&self) {
        if let Some(handle) = self.sweeper.lock().ok().and_then(|mut s| s.take()) {
            let _ = handle.join();
        }
    }

    /// Boot-time prune: sweeps stale `.tmp-` leftovers, evicts the oldest
    /// unprotected generations until both the byte budget and the window
    /// hold, then deletes every blob no remaining generation references.
    fn prune_with(&self, budget_bytes: u64, max_generations: usize) {
        let _ = fs::remove_file(self.dir.join(LEGACY_POINTER_FILE));
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let protected = self.protected_keys();
        let mut gens = scan_generations(entries, &protected);
        let blobs_dir = self.dir.join(BLOBS_DIR);
        let blob_sizes = scan_blobs(&blobs_dir);
        let mut refcount: HashMap<String, usize> = HashMap::new();
        for g in &gens {
            for h in &g.refs {
                *refcount.entry(h.clone()).or_default() += 1;
            }
        }
        evict_oldest(
            &mut gens,
            &mut refcount,
            &blob_sizes,
            budget_bytes,
            max_generations,
        );
        let mut live: HashSet<String> = refcount
            .into_iter()
            .filter(|&(_, c)| c > 0)
            .map(|(h, _)| h)
            .collect();
        // Protection may have moved since the scan (a concurrent persist or
        // another root's restore).
        for key in self.protected_keys() {
            live.extend(manifest_hashes(&self.dir.join(key)));
        }
        for hash in blob_sizes.keys() {
            if !live.contains(hash) {
                let _ = fs::remove_file(blobs_dir.join(hash));
            }
        }
    }
}

struct Generation {
    path: PathBuf,
    stamp: SystemTime,
    own_size: u64,
    refs: Vec<String>,
    protected: bool,
}

/// Lists generation dirs, deleting stale `.tmp-` dirs along the way.
fn scan_generations(entries: fs::ReadDir, protected: &HashSet<String>) -> Vec<Generation> {
    let mut gens = Vec::new();
    for e in entries.flatten() {
        let path = e.path();
        let name = e.file_name().to_string_lossy().into_owned();
        if name == BLOBS_DIR || name == PINS_DIR || !path.is_dir() {
            continue;
        }
        if name.starts_with(".tmp-") {
            if tmp_is_stale(&path) {
                let _ = fs::remove_dir_all(&path);
            }
            continue;
        }
        let refs = manifest_hashes(&path);
        let stamp = generation_stamp(&path);
        gens.push(Generation {
            own_size: entry_size(&path),
            protected: protected.contains(&name),
            path,
            stamp,
            refs,
        });
    }
    gens
}

/// Blob hash -> size, deleting stale `.tmp-` blob copies along the way.
fn scan_blobs(blobs_dir: &Path) -> HashMap<String, u64> {
    let mut blob_sizes = HashMap::new();
    if let Ok(entries) = fs::read_dir(blobs_dir) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(".tmp-") {
                if tmp_is_stale(&e.path()) {
                    let _ = fs::remove_file(e.path());
                }
                continue;
            }
            if let Ok(meta) = e.metadata() {
                blob_sizes.insert(name, meta.len());
            }
        }
    }
    blob_sizes
}

/// Removes generations oldest-first (skipping protected ones) until the total size
/// (generation dirs plus referenced blobs) fits the budget and at most
/// `max_generations` remain. Decrements `refcount` for each eviction.
fn evict_oldest(
    gens: &mut [Generation],
    refcount: &mut HashMap<String, usize>,
    blob_sizes: &HashMap<String, u64>,
    budget_bytes: u64,
    max_generations: usize,
) {
    let mut total: u64 = gens.iter().map(|g| g.own_size).sum();
    for (hash, count) in refcount.iter() {
        if *count > 0 {
            total += blob_sizes.get(hash).copied().unwrap_or(0);
        }
    }
    gens.sort_by_key(|g| g.stamp);
    let mut kept = gens.len();
    for g in gens.iter() {
        if total <= budget_bytes && kept <= max_generations {
            break;
        }
        if g.protected {
            continue;
        }
        // A dir already gone (another process's prune won the race) is an
        // eviction all the same; only a real failure keeps the entry.
        if let Err(e) = fs::remove_dir_all(&g.path) {
            if e.kind() != std::io::ErrorKind::NotFound {
                continue;
            }
        }
        kept -= 1;
        total = total.saturating_sub(g.own_size);
        for h in &g.refs {
            if let Some(c) = refcount.get_mut(h.as_str()) {
                *c -= 1;
                if *c == 0 {
                    total = total.saturating_sub(blob_sizes.get(h).copied().unwrap_or(0));
                }
            }
        }
    }
}

fn enforce_window_blocking(dir: &Path, max_generations: usize, protected: &HashSet<String>) {
    let evicted = rename_aside_overflow(dir, protected, max_generations);
    if evicted.is_empty() {
        return;
    }
    // Blobs still referenced by a surviving generation stay;
    // everything the evicted generations uniquely owned goes.
    let live = live_blob_hashes(dir);
    let blobs = dir.join(BLOBS_DIR);
    for aside in evicted {
        for hash in manifest_hashes(&aside) {
            if !live.contains(&hash) {
                let _ = fs::remove_file(blobs.join(&hash));
            }
        }
        let _ = fs::remove_dir_all(&aside);
    }
}

/// Renames the oldest generations beyond the window to `.tmp-evict-` dirs
/// and returns them. A protected generation is never a candidate.
fn rename_aside_overflow(
    dir: &Path,
    protected: &HashSet<String>,
    max_generations: usize,
) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut gens: Vec<(SystemTime, PathBuf)> = Vec::new();
    let mut kept = 0;
    for e in entries.flatten() {
        if !is_generation_dir(&e) {
            continue;
        }
        if protected.contains(e.file_name().to_string_lossy().as_ref()) {
            kept += 1;
            continue;
        }
        gens.push((generation_stamp(&e.path()), e.path()));
    }
    // Protected generations count toward the window (at least one: the
    // generation this persist just made current).
    let overflow = (gens.len() + kept.max(1)).saturating_sub(max_generations);
    if overflow == 0 {
        return Vec::new();
    }
    gens.sort_by_key(|(stamp, _)| *stamp);
    let mut evicted = Vec::new();
    for (_, path) in gens.into_iter().take(overflow) {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            continue;
        };
        let aside = dir.join(format!(
            ".tmp-evict-{}-{}",
            name.get(..16).unwrap_or(&name),
            std::process::id()
        ));
        if fs::rename(&path, &aside).is_ok() {
            evicted.push(aside);
        }
    }
    evicted
}

/// Every blob hash referenced by a generation dir (protected ones included).
fn live_blob_hashes(dir: &Path) -> HashSet<String> {
    let mut live = HashSet::new();
    if let Ok(entries) = fs::read_dir(dir) {
        for e in entries.flatten().filter(is_generation_dir) {
            live.extend(manifest_hashes(&e.path()));
        }
    }
    live
}
