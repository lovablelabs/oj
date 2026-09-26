// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! oj exists to make many builds cheap, so several of them share one
//! `.oj-cache` at the same time — threads within a build, and separate
//! processes in CI or an agent fleet. The rule under contention is the same as
//! everywhere else in the cache: a read returns the exact entry or nothing.
//! Never a torn entry, never a leftover temp file, never a panic.

mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use common::*;
use oj_cache::PersistentCache;

/// Big enough that a write is several syscalls, so a torn read is reachable if
/// publication is not atomic.
fn big_module(tag: &str) -> oj_cache::CachedModule {
    let mut m = module(&format!(
        "/*{tag}*/ export const big = \"{}\";",
        "x".repeat(400_000)
    ));
    m.imports = (0..2_000).map(|i| format!("/src/dep{i}.tsx")).collect();
    m
}

#[test]
fn concurrent_writers_of_one_key_publish_a_complete_entry() {
    let f = fixture();
    let dir = f.dir.path().to_path_buf();
    let key = f.cache.key(b"source", "/src/App.tsx", "dev");
    let expected = big_module("same");

    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for _ in 0..8 {
        let (dir, key, barrier) = (dir.clone(), key.clone(), Arc::clone(&barrier));
        let expected = expected.clone();
        handles.push(std::thread::spawn(move || {
            let cache = PersistentCache::new(dir, VERSION);
            barrier.wait();
            for _ in 0..20 {
                cache.put(&key, &expected);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(f.cache.get(&key), Some(expected));
    assert_eq!(
        walk(f.dir.path()),
        vec![format!("{}/{key}.json", &key[..2])],
        "no temp files may survive"
    );
}

#[test]
fn a_reader_never_observes_a_partial_entry() {
    let f = fixture();
    let dir = f.dir.path().to_path_buf();
    let key = f.cache.key(b"source", "/src/App.tsx", "dev");
    let expected = big_module("torn");

    let stop = Arc::new(AtomicBool::new(false));
    let hits = Arc::new(AtomicUsize::new(0));
    let misses = Arc::new(AtomicUsize::new(0));

    let readers: Vec<_> = (0..4)
        .map(|_| {
            let (dir, key, stop) = (dir.clone(), key.clone(), Arc::clone(&stop));
            let (hits, misses, expected) =
                (Arc::clone(&hits), Arc::clone(&misses), expected.clone());
            std::thread::spawn(move || {
                let cache = PersistentCache::new(dir, VERSION);
                while !stop.load(Ordering::Relaxed) {
                    match cache.get(&key) {
                        Some(entry) => {
                            assert_eq!(entry, expected, "torn entry served");
                            hits.fetch_add(1, Ordering::Relaxed);
                        }
                        None => {
                            misses.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            })
        })
        .collect();

    let writers: Vec<_> = (0..4)
        .map(|_| {
            let (dir, key, expected) = (dir.clone(), key.clone(), expected.clone());
            std::thread::spawn(move || {
                let cache = PersistentCache::new(dir, VERSION);
                for _ in 0..30 {
                    cache.put(&key, &expected);
                }
            })
        })
        .collect();

    for w in writers {
        w.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    for r in readers {
        r.join().unwrap();
    }

    assert!(
        hits.load(Ordering::Relaxed) > 0,
        "the entry must become readable at some point ({} misses)",
        misses.load(Ordering::Relaxed)
    );
    assert_eq!(f.cache.get(&key), Some(expected));
}

#[test]
fn concurrent_writers_of_distinct_keys_all_land() {
    let f = fixture();
    let dir = f.dir.path().to_path_buf();
    let total = 400;

    let barrier = Arc::new(Barrier::new(8));
    let mut handles = Vec::new();
    for thread in 0..8 {
        let (dir, barrier) = (dir.clone(), Arc::clone(&barrier));
        handles.push(std::thread::spawn(move || {
            let cache = PersistentCache::new(dir, VERSION);
            barrier.wait();
            for i in (thread..total).step_by(8) {
                let url = format!("/src/m{i}.tsx");
                let key = cache.key(format!("source {i}").as_bytes(), &url, "dev");
                cache.put(&key, &module(&format!("export const n = {i};")));
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    for i in 0..total {
        let url = format!("/src/m{i}.tsx");
        let key = f.cache.key(format!("source {i}").as_bytes(), &url, "dev");
        assert_eq!(
            f.cache.get(&key).map(|m| m.code),
            Some(format!("export const n = {i};")),
            "entry {i} is missing"
        );
    }
    assert_eq!(walk(f.dir.path()).len(), total, "one file per entry");
}

#[test]
fn a_reader_racing_a_first_write_sees_a_miss_or_the_entry() {
    // The interesting window is the very first `put` for a key: the shard
    // directory does not exist yet.
    for round in 0..40 {
        let f = fixture();
        let dir = f.dir.path().to_path_buf();
        let key = f
            .cache
            .key(format!("round {round}").as_bytes(), "/src/App.tsx", "dev");
        let expected = module(&format!("export const round = {round};"));

        let barrier = Arc::new(Barrier::new(2));
        let writer = {
            let (dir, key, expected, barrier) = (
                dir.clone(),
                key.clone(),
                expected.clone(),
                Arc::clone(&barrier),
            );
            std::thread::spawn(move || {
                let cache = PersistentCache::new(dir, VERSION);
                barrier.wait();
                cache.put(&key, &expected);
            })
        };
        let reader = {
            let (dir, key, expected, barrier) = (dir, key, expected.clone(), barrier);
            std::thread::spawn(move || {
                let cache = PersistentCache::new(dir, VERSION);
                barrier.wait();
                for _ in 0..50 {
                    if let Some(entry) = cache.get(&key) {
                        assert_eq!(entry, expected, "torn first entry");
                    }
                }
            })
        };
        writer.join().unwrap();
        reader.join().unwrap();
    }
}

#[test]
fn eviction_of_a_corrupt_entry_races_safely_with_a_rewrite() {
    let f = fixture();
    let dir = f.dir.path().to_path_buf();
    let key = f.cache.key(b"source", "/src/App.tsx", "dev");
    let expected = module("export const x = 1;");
    let path = entry_path(f.dir.path(), &key);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();

    // One thread keeps corrupting the entry (a crashed peer), the others keep
    // reading and rewriting it. Nothing here may panic, and the value that
    // survives must be the right one.
    let stop = Arc::new(AtomicBool::new(false));
    let corrupter = {
        let (path, stop) = (path.clone(), Arc::clone(&stop));
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = std::fs::write(&path, b"{ truncated");
            }
        })
    };
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let (dir, key, expected) = (dir.clone(), key.clone(), expected.clone());
            std::thread::spawn(move || {
                let cache = PersistentCache::new(dir, VERSION);
                for _ in 0..200 {
                    if let Some(entry) = cache.get(&key) {
                        assert_eq!(entry.kind, expected.kind);
                    }
                    cache.put(&key, &expected);
                }
            })
        })
        .collect();
    for w in workers {
        w.join().unwrap();
    }
    stop.store(true, Ordering::Relaxed);
    corrupter.join().unwrap();

    f.cache.put(&key, &expected);
    assert_eq!(f.cache.get(&key), Some(expected));
}
