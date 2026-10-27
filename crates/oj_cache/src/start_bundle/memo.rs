// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Per-generation (size, mtime) -> digest memo, so a restore rehashes only
//! closure files whose stat changed.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use super::{hash_file, mtime_ns, now_ns, par_map, MEMO_FILE};
use crate::integrity;

/// Files modified this close to the memo write are not memoized: a later
/// same-size edit within mtime granularity would otherwise reuse a stale
/// digest.
const MEMO_FRESHNESS_SLACK_NS: u64 = 2_000_000_000;

#[derive(Serialize, Deserialize)]
pub(super) struct Memo {
    written_at_ns: u64,
    files: HashMap<String, MemoFile>,
}

#[derive(Serialize, Deserialize)]
struct MemoFile {
    size: u64,
    mtime_ns: u64,
    digest: String,
}

/// An unparseable memo is deleted so the next write starts clean.
pub(super) fn read_memo(entry: &Path) -> Option<Memo> {
    let path = entry.join(MEMO_FILE);
    let bytes = fs::read(&path).ok()?;
    match serde_json::from_slice(&bytes) {
        Ok(memo) => Some(memo),
        Err(_) => {
            let _ = fs::remove_file(&path);
            None
        }
    }
}

pub(super) fn write_memo(entry: &Path, memo: &Memo) {
    let Ok(bytes) = serde_json::to_vec(memo) else {
        return;
    };
    let _ = integrity::atomic_write(&entry.join(MEMO_FILE), &bytes);
}

/// The memo of a [`verified_digests`] pass: each file under the stat it had
/// before it was read. A stat taken after (once the persist's blob copies are
/// done) would pair an edit made meanwhile with the digest of the bytes
/// before it, and every later persist would take that digest as settled.
pub(super) fn build_memo(files: &[PathBuf], digests: &Digests) -> Memo {
    let written_at_ns = digests.statted_at_ns;
    let mut map = HashMap::new();
    for ((path, digest), stat) in files.iter().zip(&digests.hashes).zip(&digests.stats) {
        let (Some(digest), Some((size, mtime_ns)), Some(path)) = (digest, stat, path.to_str())
        else {
            continue;
        };
        if mtime_ns.saturating_add(MEMO_FRESHNESS_SLACK_NS) > written_at_ns {
            continue;
        }
        map.insert(
            path.to_string(),
            MemoFile {
                size: *size,
                mtime_ns: *mtime_ns,
                digest: digest.to_hex().to_string(),
            },
        );
    }
    Memo {
        written_at_ns,
        files: map,
    }
}

/// The digests of a closure's files, each with the stat taken before its
/// bytes were read, and how many had to be rehashed.
pub(super) struct Digests {
    pub(super) hashes: Vec<Option<blake3::Hash>>,
    stats: Vec<Option<(u64, u64)>>,
    /// Taken before any stat: the freshness window counts from here.
    statted_at_ns: u64,
    pub(super) rehashed: usize,
}

/// Digests for `files`, taken from the memo when size and mtime still match.
pub(super) fn verified_digests(files: &[PathBuf], memo: Option<&Memo>) -> Digests {
    let statted_at_ns = now_ns();
    let rehashed = AtomicUsize::new(0);
    let both = par_map(files, |p| {
        let stat = fs::metadata(p)
            .ok()
            .and_then(|meta| Some((meta.len(), mtime_ns(&meta)?)));
        if let Some(digest) = memo
            .zip(stat)
            .and_then(|(m, stat)| memoized_digest(m, p, stat))
        {
            return Some((stat, Some(digest)));
        }
        rehashed.fetch_add(1, Ordering::Relaxed);
        Some((stat, hash_file(p)))
    });
    let (stats, hashes) = both.into_iter().map(|b| b.unwrap_or((None, None))).unzip();
    Digests {
        hashes,
        stats,
        statted_at_ns,
        rehashed: rehashed.into_inner(),
    }
}

fn memoized_digest(memo: &Memo, p: &Path, (size, mtime_ns): (u64, u64)) -> Option<blake3::Hash> {
    let known = memo.files.get(p.to_str()?)?;
    if size != known.size || mtime_ns != known.mtime_ns {
        return None;
    }
    blake3::Hash::from_hex(&known.digest).ok()
}
