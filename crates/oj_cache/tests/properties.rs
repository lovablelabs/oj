// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Properties of the cache key and of the entry round trip. The key is the
//! whole correctness story: if two different compilations can share a key, the
//! cache serves the wrong code, and no test downstream of it will notice.

mod common;

use common::*;
use oj_cache::{CachedModule, PersistentCache};
use proptest::prelude::*;

fn cached_module() -> impl Strategy<Value = CachedModule> {
    (
        ".{0,64}",
        proptest::option::of(".{0,32}"),
        proptest::collection::vec(".{0,24}", 0..4),
        any::<bool>(),
        "(esm|cjs|css|)",
        proptest::collection::vec((".{0,8}", ".{0,8}"), 0..3),
    )
        .prop_map(
            |(code, map_json, imports, is_boundary, kind, pairs)| CachedModule {
                code,
                map_json,
                imports,
                is_boundary,
                kind: kind.to_string(),
                require_map: pairs.clone(),
                css_exports: pairs.clone(),
                fs_allow: pairs.iter().map(|(a, _)| a.clone()).collect(),
                watch_files: pairs.iter().map(|(_, b)| b.clone()).collect(),
                import_bindings: pairs
                    .iter()
                    .map(|(a, b)| (a.clone(), vec![b.clone()]))
                    .collect(),
                hot: None,
            },
        )
}

proptest! {
    /// A key is always a blake3 digest in lowercase hex: the on-disk layout
    /// (`<first two chars>/<key>.json`) depends on it.
    #[test]
    fn keys_are_always_hex_digests(source in proptest::collection::vec(any::<u8>(), 0..64), url in ".{0,40}", mode in ".{0,8}") {
        let cache = PersistentCache::new(std::env::temp_dir(), "v");
        let key = cache.key(&source, &url, &mode);
        prop_assert_eq!(key.len(), 64);
        prop_assert!(key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)), "{}", key);
    }

    /// Same inputs, same key: a warm cache has to hit.
    #[test]
    fn keys_are_deterministic(source in proptest::collection::vec(any::<u8>(), 0..64), url in ".{0,40}", mode in ".{0,8}") {
        let a = PersistentCache::new(std::env::temp_dir(), "v");
        let b = PersistentCache::new(std::path::PathBuf::from("/elsewhere"), "v");
        prop_assert_eq!(a.key(&source, &url, &mode), a.key(&source, &url, &mode));
        prop_assert_eq!(a.key(&source, &url, &mode), b.key(&source, &url, &mode), "the directory is not part of the key");
    }

    /// The field separators do their job: no way to shift bytes between the
    /// mode, the url and the source and land on the same key.
    #[test]
    fn no_two_distinct_inputs_share_a_key(
        source_a in proptest::collection::vec(any::<u8>(), 0..24),
        url_a in ".{0,16}",
        mode_a in ".{0,6}",
        source_b in proptest::collection::vec(any::<u8>(), 0..24),
        url_b in ".{0,16}",
        mode_b in ".{0,6}",
    ) {
        prop_assume!((&source_a, &url_a, &mode_a) != (&source_b, &url_b, &mode_b));
        let cache = PersistentCache::new(std::env::temp_dir(), "v");
        prop_assert_ne!(
            cache.key(&source_a, &url_a, &mode_a),
            cache.key(&source_b, &url_b, &mode_b),
            "({:?}, {:?}, {:?}) collided with ({:?}, {:?}, {:?})",
            source_a, url_a, mode_a, source_b, url_b, mode_b
        );
    }

    /// A tool-version change is always a cache miss, never a silent hit.
    #[test]
    fn the_tool_version_is_part_of_every_key(a in ".{0,12}", b in ".{0,12}") {
        prop_assume!(a != b);
        let one = PersistentCache::new(std::env::temp_dir(), &a);
        let two = PersistentCache::new(std::env::temp_dir(), &b);
        prop_assert_ne!(one.key(b"source", "/u", "dev"), two.key(b"source", "/u", "dev"));
    }

    /// Whatever a compile produced, the cache gives it back unchanged.
    #[test]
    fn entries_round_trip_exactly(entry in cached_module()) {
        let f = fixture();
        let key = f.cache.key(entry.code.as_bytes(), "/src/App.tsx", "dev");
        prop_assert_eq!(f.cache.get(&key), None);
        f.cache.put(&key, &entry);
        prop_assert_eq!(f.cache.get(&key), Some(entry.clone()));
        // Overwriting with a new value replaces it wholesale.
        let mut replaced = entry.clone();
        replaced.code.push_str("/* v2 */");
        f.cache.put(&key, &replaced);
        prop_assert_eq!(f.cache.get(&key), Some(replaced));
    }

    /// Anything that is not a digest is refused rather than resolved against
    /// the filesystem.
    #[test]
    fn non_digest_keys_are_never_written(key in ".{0,80}") {
        prop_assume!(
            key.len() != 64
                || !key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        );
        let f = fixture();
        f.cache.put(&key, &module("side effect"));
        prop_assert!(walk(f.dir.path()).is_empty(), "wrote for key {:?}", key);
        prop_assert_eq!(f.cache.get(&key), None);
    }

    /// Every entry lands in the shard named by its own first two characters.
    #[test]
    fn entries_are_sharded_by_key_prefix(source in ".{0,32}", url in ".{0,32}") {
        let f = fixture();
        let key = f.cache.key(source.as_bytes(), &url, "dev");
        f.cache.put(&key, &module("export const x = 1;"));
        prop_assert_eq!(walk(f.dir.path()), vec![format!("{}/{key}.json", &key[..2])]);
    }
}
