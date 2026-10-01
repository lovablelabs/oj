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

pub(super) fn build_memo(files: &[PathBuf], digests: &[Option<blake3::Hash>]) -> Memo {
    let written_at_ns = now_ns();
    let stats = par_map(files, |p| {
        let meta = fs::metadata(p).ok()?;
        Some((meta.len(), mtime_ns(&meta)?))
    });
    let mut map = HashMap::new();
    for ((path, digest), stat) in files.iter().zip(digests).zip(&stats) {
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

/// Digests for `files`, taken from the memo when size and mtime still match,
/// plus how many had to be rehashed.
pub(super) fn verified_digests(
    files: &[PathBuf],
    memo: Option<&Memo>,
) -> (Vec<Option<blake3::Hash>>, usize) {
    let rehashed = AtomicUsize::new(0);
    let digests = par_map(files, |p| {
        if let Some(digest) = memo.and_then(|m| memoized_digest(m, p)) {
            return Some(digest);
        }
        rehashed.fetch_add(1, Ordering::Relaxed);
        hash_file(p)
    });
    (digests, rehashed.into_inner())
}

fn memoized_digest(memo: &Memo, p: &Path) -> Option<blake3::Hash> {
    let known = memo.files.get(p.to_str()?)?;
    let meta = fs::metadata(p).ok()?;
    if meta.len() != known.size || mtime_ns(&meta) != Some(known.mtime_ns) {
        return None;
    }
    blake3::Hash::from_hex(&known.digest).ok()
}
