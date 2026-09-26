// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The cache is an optimization, so every failure mode has exactly one correct
//! outcome: a miss. A crashed build, a half-written file, a read-only checkout
//! or an entry from another oj version must all cost a recompile and nothing
//! else — never a panic, and never a wrong module served from disk.

mod common;

use common::*;
use oj_cache::{CachedModule, PersistentCache};

fn corrupt_entry_is_a_miss(label: &str, contents: &[u8]) {
    let f = fixture();
    let key = f.cache.key(b"source", "/src/App.tsx", "dev");
    f.cache.put(&key, &module("export const ok = 1;"));
    let path = entry_path(f.dir.path(), &key);
    std::fs::write(&path, contents).unwrap();

    assert_eq!(f.cache.get(&key), None, "{label} must be a miss");
    assert!(!path.exists(), "{label} must be evicted, not left to rot");
    // And the cache is usable again afterwards.
    f.cache.put(&key, &module("export const ok = 2;"));
    assert_eq!(
        f.cache.get(&key).map(|m| m.code),
        Some("export const ok = 2;".to_string())
    );
}

#[test]
fn garbage_and_truncated_entries_are_misses() {
    let full = serde_json::to_vec(&module("export const x = 1;")).unwrap();
    corrupt_entry_is_a_miss("empty file", b"");
    corrupt_entry_is_a_miss("not json", b"{ not json");
    corrupt_entry_is_a_miss("truncated json", &full[..full.len() / 2]);
    corrupt_entry_is_a_miss("json of the wrong type", b"[]");
    corrupt_entry_is_a_miss("json null", b"null");
    corrupt_entry_is_a_miss(
        "missing required field",
        br#"{"imports":[],"is_boundary":false}"#,
    );
    corrupt_entry_is_a_miss(
        "wrong field type",
        br#"{"code":42,"imports":[],"is_boundary":false}"#,
    );
    corrupt_entry_is_a_miss("nul bytes", b"\0\0\0\0");
    corrupt_entry_is_a_miss("utf16 bom", b"\xff\xfe{\0}\0");
}

#[test]
fn an_entry_from_an_older_schema_still_loads() {
    // Fields added since a `.oj-cache` was written must default, not fail: the
    // cache is shared across oj versions with the same CACHE_FORMAT.
    let f = fixture();
    let key = f.cache.key(b"source", "/src/App.tsx", "dev");
    let path = entry_path(f.dir.path(), &key);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        br#"{"code":"export const a = 1;","map_data_url":null,"imports":["./b.ts"],"is_boundary":true}"#,
    )
    .unwrap();

    let hit = f.cache.get(&key).expect("older entries still load");
    assert_eq!(hit.code, "export const a = 1;");
    assert_eq!(hit.imports, vec!["./b.ts".to_string()]);
    assert_eq!(hit.kind, "", "absent fields take their default");
    assert!(hit.require_map.is_empty());
    assert!(hit.css_exports.is_empty());
    assert!(hit.fs_allow.is_empty());
    assert!(hit.watch_files.is_empty());
}

#[test]
fn unknown_fields_from_a_newer_oj_are_ignored() {
    let f = fixture();
    let key = f.cache.key(b"source", "/src/App.tsx", "dev");
    let path = entry_path(f.dir.path(), &key);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &path,
        br#"{"code":"x","imports":[],"is_boundary":false,"future_field":{"deep":[1,2]}}"#,
    )
    .unwrap();
    assert_eq!(f.cache.get(&key).map(|m| m.code), Some("x".to_string()));
}

#[test]
fn a_directory_where_an_entry_should_be_is_a_miss() {
    let f = fixture();
    let key = f.cache.key(b"source", "/src/App.tsx", "dev");
    let path = entry_path(f.dir.path(), &key);
    std::fs::create_dir_all(&path).unwrap();
    assert_eq!(f.cache.get(&key), None);
}

#[test]
fn a_leftover_temp_file_is_never_served_and_never_blocks_a_write() {
    let f = fixture();
    let key = f.cache.key(b"source", "/src/App.tsx", "dev");
    let path = entry_path(f.dir.path(), &key);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    // A crash mid-`put` leaves a partial temp file behind, named after the
    // writer that died.
    std::fs::write(path.with_extension("tmp"), b"{ half written").unwrap();
    std::fs::write(path.with_extension("tmp-999999-7"), b"{ half written").unwrap();

    assert_eq!(f.cache.get(&key), None, "temp files are not entries");
    f.cache.put(&key, &module("export const recovered = 1;"));
    assert_eq!(
        f.cache.get(&key).map(|m| m.code),
        Some("export const recovered = 1;".to_string())
    );
}

#[cfg(unix)]
#[test]
fn a_read_only_cache_directory_degrades_to_no_caching() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().unwrap();
    let cache_dir = dir.path().join("cache");
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o555)).unwrap();

    let cache = PersistentCache::new(cache_dir.clone(), VERSION);
    let key = cache.key(b"source", "/src/App.tsx", "dev");
    // Must not panic, must not hang, must simply not cache.
    cache.put(&key, &module("export const x = 1;"));
    assert_eq!(cache.get(&key), None);

    std::fs::set_permissions(&cache_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[cfg(unix)]
#[test]
fn a_read_only_shard_directory_degrades_to_no_caching() {
    use std::os::unix::fs::PermissionsExt;

    let f = fixture();
    let key = f.cache.key(b"source", "/src/App.tsx", "dev");
    f.cache.put(&key, &module("first"));
    let shard = entry_path(f.dir.path(), &key)
        .parent()
        .unwrap()
        .to_path_buf();
    std::fs::set_permissions(&shard, std::fs::Permissions::from_mode(0o555)).unwrap();

    f.cache.put(&key, &module("second"));
    assert_eq!(
        f.cache.get(&key).map(|m| m.code),
        Some("first".to_string()),
        "a failed write must leave the previous entry intact"
    );

    std::fs::set_permissions(&shard, std::fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn a_cache_dir_that_is_a_file_is_inert() {
    let file = tempfile::NamedTempFile::new().unwrap();
    let cache = PersistentCache::new(file.path().to_path_buf(), VERSION);
    let key = cache.key(b"source", "/src/App.tsx", "dev");
    cache.put(&key, &module("export const x = 1;"));
    assert_eq!(cache.get(&key), None);
}

#[test]
fn a_missing_cache_dir_is_created_on_demand() {
    let dir = tempfile::tempdir().unwrap();
    let nested = dir.path().join("a/b/c/.oj-cache");
    let cache = PersistentCache::new(nested.clone(), VERSION);
    let key = cache.key(b"source", "/src/App.tsx", "dev");
    cache.put(&key, &module("export const x = 1;"));
    assert!(cache.get(&key).is_some());
    assert!(nested.is_dir());
}

#[test]
fn a_different_tool_version_never_reads_the_other_ones_entries() {
    let dir = tempfile::tempdir().unwrap();
    let old = PersistentCache::new(dir.path().to_path_buf(), "0.0.1");
    let new = PersistentCache::new(dir.path().to_path_buf(), "0.0.2");

    let old_key = old.key(b"source", "/src/App.tsx", "dev");
    old.put(&old_key, &module("compiled by 0.0.1"));

    let new_key = new.key(b"source", "/src/App.tsx", "dev");
    assert_ne!(old_key, new_key, "the version is part of the key");
    assert_eq!(new.get(&new_key), None, "a version bump is a cold cache");
    // ...and the old entry is still there for the old binary.
    assert!(old.get(&old_key).is_some());
}

#[test]
fn keys_that_are_not_valid_paths_do_not_escape_the_cache_dir() {
    // `get`/`put` take a key as a string; a caller passing something that is
    // not a cache key must not be able to write outside the cache dir.
    let f = fixture();
    for hostile in [
        "../../../../../../tmp/oj-escape",
        "..",
        "/etc/oj-escape",
        "",
        "a",
    ] {
        f.cache.put(hostile, &module("nope"));
        let _ = f.cache.get(hostile);
    }
    assert!(
        walk(f.dir.path()).is_empty(),
        "a malformed key must not become an entry: {:?}",
        walk(f.dir.path())
    );
    for escape in ["/tmp/oj-escape", "/tmp/oj-escape.json"] {
        assert!(
            !std::path::Path::new(escape).exists(),
            "wrote outside the cache directory: {escape}"
        );
    }
}

#[test]
fn an_entry_is_only_ever_published_whole() {
    // A hit must carry every field exactly as written: no lossy round trip.
    let f = fixture();
    let key = f.cache.key(b"source", "/src/App.tsx", "dev");
    let written = CachedModule {
        code: "const s = \"\u{2028}\u{2029}\\n\t\0é🚀\";".into(),
        map_data_url: None,
        imports: vec![String::new(), "…/ünïcødé.tsx".into()],
        is_boundary: false,
        kind: "cjs".into(),
        require_map: vec![("".into(), "".into())],
        css_exports: vec![("a\nb".into(), "c\"d".into())],
        fs_allow: vec!["/tmp/a b/c".into()],
        watch_files: vec!["\\\\?\\C:\\x".into()],
        hot: None,
    };
    f.cache.put(&key, &written);
    assert_eq!(f.cache.get(&key), Some(written));
}
