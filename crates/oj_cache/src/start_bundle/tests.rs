// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use super::*;
use std::time::Duration;

struct Fixture {
    root: PathBuf,
    start: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let root = std::env::temp_dir().join(format!(
            "oj-start-bundle-test-{}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let start = crate::cache_root(&root).join("start");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(&start).unwrap();
        fs::write(root.join("package.json"), b"{}").unwrap();
        let fx = Self { root, start };
        fx.write_module("a.tsx", "export const a = 1;");
        fx.write_module("b.tsx", "export const b = 2;");
        fx.write_build("bundle-v1");
        fx
    }

    fn store(&self) -> StartBundleStore {
        StartBundleStore::new(&self.root, "0.0.1-test", VerifyMode::Standard)
    }

    fn full_store(&self) -> StartBundleStore {
        StartBundleStore::new(&self.root, "0.0.1-test", VerifyMode::Full)
    }

    fn module(&self, name: &str) -> PathBuf {
        self.root.join("src").join(name)
    }

    fn write_module(&self, name: &str, code: &str) {
        fs::write(self.module(name), code).unwrap();
    }

    /// Backdate module mtimes past the memo freshness window so persist
    /// records them (freshly written files are deliberately left out).
    fn settle_modules(&self) {
        let old = SystemTime::now() - Duration::from_secs(3600);
        for name in ["a.tsx", "b.tsx"] {
            set_mtime(&self.module(name), old);
        }
    }

    /// Simulate what bundle-client.mjs leaves in .oj-cache/start:
    /// the chunk dir, its index, and the small artifacts. The
    /// `shared.js` chunk keeps the same bytes across builds so blob
    /// dedupe is observable; the entry chunk carries the marker.
    fn write_build(&self, marker: &str) {
        let closure = vec![
            self.module("a.tsx").display().to_string(),
            self.module("b.tsx").display().to_string(),
        ];
        fs::write(
            self.start.join(CLOSURE_FILE),
            serde_json::to_vec(&closure).unwrap(),
        )
        .unwrap();
        let chunks = self.start.join(CHUNKS_DIR);
        let _ = fs::remove_dir_all(&chunks);
        fs::create_dir_all(&chunks).unwrap();
        fs::write(chunks.join("client-entry.js"), marker).unwrap();
        fs::write(chunks.join("shared-x.js"), "shared bytes").unwrap();
        let index = serde_json::json!({
            "entry": "client-entry.js",
            "files": [
                { "name": "client-entry.js", "size": marker.len() },
                { "name": "shared-x.js", "size": "shared bytes".len() },
            ],
        });
        fs::write(self.start.join(CHUNK_INDEX_FILE), index.to_string()).unwrap();
        fs::write(
            self.start.join(CSS_URLS_FILE),
            br#"["/@oj-start/fs/a.css"]"#,
        )
        .unwrap();
        fs::write(self.start.join("client-entry.modules"), "2").unwrap();
        fs::write(self.start.join("manifest.ts"), format!("// {marker}")).unwrap();
    }

    fn clear_start_dir(&self) {
        for name in ARTIFACTS.into_iter().chain([CSS_URLS_FILE]) {
            let _ = fs::remove_file(self.start.join(name));
        }
        let _ = fs::remove_dir_all(self.start.join(CHUNKS_DIR));
    }

    fn entry_dir(&self, key: &str) -> PathBuf {
        crate::cache_root(&self.root).join("start-bundle").join(key)
    }

    fn blob_dir(&self) -> PathBuf {
        crate::cache_root(&self.root)
            .join("start-bundle")
            .join(BLOBS_DIR)
    }

    fn blob_count(&self) -> usize {
        fs::read_dir(self.blob_dir())
            .map(|d| d.flatten().count())
            .unwrap_or(0)
    }

    fn entry_bytes(&self, pinned: &PinnedBundle) -> String {
        let chunk = pinned.chunk("client-entry.js").unwrap();
        String::from_utf8(fs::read(&chunk.path).unwrap()).unwrap()
    }
}

fn set_mtime(path: &Path, t: SystemTime) {
    let f = fs::OpenOptions::new().append(true).open(path).unwrap();
    f.set_times(fs::FileTimes::new().set_modified(t)).unwrap();
}

#[test]
fn restore_misses_without_previous_build() {
    let fx = Fixture::new("nolatest");
    assert!(matches!(
        fx.store().restore(&fx.start),
        Err(Miss::NoPreviousBuild)
    ));
}

#[test]
fn persist_then_restore_roundtrips_artifacts() {
    let fx = Fixture::new("roundtrip");
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    fx.clear_start_dir();
    let (stats, pinned) = fx.store().restore(&fx.start).unwrap();
    assert_eq!(stats.key, key);
    assert_eq!(stats.files, 2);
    assert_eq!(stats.chunks, 2);
    assert_eq!(fx.entry_bytes(&pinned), "bundle-v1");
    assert_eq!(
        fs::read_to_string(&pinned.chunk("shared-x.js").unwrap().path).unwrap(),
        "shared bytes"
    );
    assert_eq!(
        fs::read_to_string(fx.start.join("manifest.ts")).unwrap(),
        "// bundle-v1"
    );
    assert_eq!(
        fs::read_to_string(fx.start.join(CSS_URLS_FILE)).unwrap(),
        r#"["/@oj-start/fs/a.css"]"#
    );
    assert!(
        !fx.start.join(CHUNKS_DIR).exists(),
        "restore must not copy chunk bytes back into the start dir"
    );
}

#[test]
fn names_absent_from_the_manifest_do_not_resolve() {
    let fx = Fixture::new("absent");
    let (_, pinned) = fx.store().persist(&fx.start).unwrap();
    assert!(pinned.chunk("client-entry.js").is_some());
    assert!(pinned.chunk("shared-x.js").is_some());
    assert!(pinned.chunk("other-chunk.js").is_none());
    assert!(pinned.chunk("../../../etc/passwd").is_none());
}

#[test]
fn pinned_bundle_comes_from_the_blob_store_not_the_build_dir() {
    let fx = Fixture::new("blobpaths");
    let (_, pinned) = fx.store().persist(&fx.start).unwrap();
    let chunk = pinned.chunk("client-entry.js").unwrap();
    assert!(chunk.path.starts_with(fx.blob_dir()), "{:?}", chunk.path);
    assert_eq!(
        chunk.hash.as_deref(),
        Some(blake3::hash(b"bundle-v1").to_hex().as_str())
    );
}

#[test]
fn from_build_dir_pins_the_fresh_build() {
    let fx = Fixture::new("builddir");
    let pinned = PinnedBundle::from_build_dir(&fx.start).unwrap();
    assert_eq!(pinned.entry, "client-entry.js");
    assert_eq!(pinned.len(), 2);
    let chunk = pinned.chunk("shared-x.js").unwrap();
    assert!(chunk.path.starts_with(fx.start.join(CHUNKS_DIR)));
    assert_eq!(chunk.hash, None);
    assert!(pinned.chunk("missing.js").is_none());
}

#[test]
fn source_edit_changes_key_and_misses_until_repersisted() {
    let fx = Fixture::new("edit");
    fx.settle_modules();
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    fx.write_module("a.tsx", "export const a = 99;");
    match fx.store().restore(&fx.start) {
        Err(Miss::NoEntryForKey(k)) => assert_ne!(k, key),
        other => panic!("expected key miss, got {other:?}"),
    }
    fx.write_build("bundle-v2");
    let (key2, _) = fx.store().persist(&fx.start).unwrap();
    assert_ne!(key2, key);
    fx.clear_start_dir();
    assert_eq!(fx.store().restore(&fx.start).unwrap().0.key, key2);
}

#[test]
fn reverting_an_edit_pins_the_older_generation() {
    let fx = Fixture::new("revert");
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    fx.write_module("a.tsx", "export const a = 99;");
    fx.write_build("bundle-v2");
    fx.store().persist(&fx.start).unwrap();
    fx.write_module("a.tsx", "export const a = 1;");
    // current points at v2, whose closure lists the same files; the
    // recomputed key lands back on the v1 generation, and the pointer
    // swings back to it.
    let (stats, pinned) = fx.store().restore(&fx.start).unwrap();
    assert_eq!(stats.key, key);
    assert_eq!(fx.entry_bytes(&pinned), "bundle-v1");
    let current = fs::read_to_string(
        crate::cache_root(&fx.root)
            .join("start-bundle")
            .join(CURRENT_FILE),
    )
    .unwrap();
    assert_eq!(
        current.trim(),
        key,
        "pointer swaps to the pinned generation"
    );
}

#[test]
fn generations_share_identical_chunks_in_the_blob_store() {
    let fx = Fixture::new("dedupe");
    fx.store().persist(&fx.start).unwrap();
    assert_eq!(fx.blob_count(), 2, "entry + shared chunk");
    fx.write_module("a.tsx", "export const a = 99;");
    fx.write_build("bundle-v2");
    fx.store().persist(&fx.start).unwrap();
    // Second generation adds a new entry blob; shared-x.js dedupes.
    assert_eq!(fx.blob_count(), 3, "shared chunk stored once");
}

#[test]
fn deleted_closure_file_is_a_miss_not_a_panic() {
    let fx = Fixture::new("deleted");
    fx.store().persist(&fx.start).unwrap();
    fs::remove_file(fx.module("b.tsx")).unwrap();
    assert!(matches!(
        fx.store().restore(&fx.start),
        Err(Miss::ClosureFileUnreadable(_))
    ));
}

#[test]
fn epoch_input_changes_the_key() {
    let fx = Fixture::new("epoch");
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    fs::write(fx.root.join("package.json"), b"{\"name\":\"x\"}").unwrap();
    match fx.store().restore(&fx.start) {
        Err(Miss::NoEntryForKey(k)) => assert_ne!(k, key),
        other => panic!("expected key miss, got {other:?}"),
    }
    let other_version =
        StartBundleStore::new(&fx.root, "9.9.9", VerifyMode::Standard).persist(&fx.start);
    assert_ne!(other_version.unwrap().0, key, "tool version salts the key");
}

#[test]
fn vendored_rolldown_identity_is_an_epoch_input() {
    let fx = Fixture::new("vendor-epoch");
    let path = "/opt/vendor".to_string();
    let identity = |version: &str| {
        VendoredRolldown::Resolved {
            path: path.clone(),
            version: version.into(),
        }
        .epoch_input()
        .unwrap()
    };
    let v1 = epoch(&fx.root, "development", Some(&identity("1.2.1")));
    assert_ne!(v1, epoch(&fx.root, "development", None));
    assert_ne!(
        v1,
        epoch(&fx.root, "development", Some(&identity("1.3.0"))),
        "an in-place upgrade at the same path is a different epoch"
    );
    let broken = VendoredRolldown::Broken {
        path: path.clone(),
        reason: "x".into(),
    }
    .epoch_input()
    .unwrap();
    assert_ne!(
        v1,
        epoch(&fx.root, "development", Some(&broken)),
        "a broken vendor window never aliases a resolved one"
    );
    assert_eq!(VendoredRolldown::None.epoch_input(), None);
}

#[test]
fn dev_mode_and_its_env_file_change_the_key() {
    let fx = Fixture::new("mode");
    let staging = |root: &Path| {
        StartBundleStore::for_mode(root, "0.0.1-test", VerifyMode::Standard, "staging")
    };
    let (dev_key, _) = fx.store().persist(&fx.start).unwrap();
    let (staging_key, _) = staging(&fx.root).persist(&fx.start).unwrap();
    assert_ne!(staging_key, dev_key, "the mode salts the key");
    fs::write(fx.root.join(".env.staging"), b"VITE_FLAVOR=staging\n").unwrap();
    match staging(&fx.root).restore(&fx.start) {
        Err(Miss::NoEntryForKey(k)) => assert_ne!(k, staging_key, ".env.<mode> is a key input"),
        other => panic!("expected key miss, got {other:?}"),
    }
    fs::write(fx.root.join(".env.development"), b"VITE_FLAVOR=dev\n").unwrap();
    match fx.store().restore(&fx.start) {
        Err(Miss::NoEntryForKey(k)) => assert_ne!(k, dev_key),
        other => panic!("expected key miss, got {other:?}"),
    }
}

#[test]
fn missing_blob_fails_verification_and_removes_the_entry() {
    let fx = Fixture::new("missing-blob");
    let (key, pinned) = fx.store().persist(&fx.start).unwrap();
    fs::remove_file(&pinned.chunk("shared-x.js").unwrap().path).unwrap();
    match fx.store().restore(&fx.start) {
        Err(Miss::ChunkCorrupt { key: k, name, .. }) => {
            assert_eq!(k, key);
            assert_eq!(name, "shared-x.js");
        }
        other => panic!("expected chunk corruption, got {other:?}"),
    }
    assert!(
        !fx.entry_dir(&key).exists(),
        "corrupt entry must be removed"
    );
}

#[test]
fn wrong_size_blob_is_caught_in_standard_mode() {
    let fx = Fixture::new("truncated-blob");
    let (key, pinned) = fx.store().persist(&fx.start).unwrap();
    let blob = &pinned.chunk("client-entry.js").unwrap().path;
    fs::write(blob, b"bundle").unwrap();
    assert!(matches!(
        fx.store().restore(&fx.start),
        Err(Miss::ChunkCorrupt { .. })
    ));
    assert!(!fx.entry_dir(&key).exists());
    assert!(!blob.exists(), "corrupt blob must be removed");
}

#[test]
fn same_size_bitflip_is_caught_only_in_full_mode() {
    let fx = Fixture::new("bitflip");
    let (key, pinned) = fx.store().persist(&fx.start).unwrap();
    let blob = pinned.chunk("client-entry.js").unwrap().path.clone();
    fs::write(&blob, b"bundle-vX").unwrap();
    // Standard mode trusts existence + size: the flip passes restore.
    assert!(fx.store().restore(&fx.start).is_ok());
    // Full mode re-hashes, detects, removes blob + entry, and misses.
    match fx.full_store().restore(&fx.start) {
        Err(Miss::ChunkCorrupt { name, detail, .. }) => {
            assert_eq!(name, "client-entry.js");
            assert!(detail.contains("hash mismatch"), "{detail}");
        }
        other => panic!("expected hash mismatch, got {other:?}"),
    }
    assert!(!fx.entry_dir(&key).exists());
    assert!(!blob.exists());
    // The rebuild path (persist) heals the store.
    fx.write_build("bundle-v1");
    let (key2, pinned2) = fx.full_store().persist(&fx.start).unwrap();
    assert_eq!(key2, key);
    assert_eq!(fx.entry_bytes(&pinned2), "bundle-v1");
    assert!(fx.full_store().restore(&fx.start).is_ok());
}

#[test]
fn foreign_format_manifest_is_invalid_by_version() {
    let fx = Fixture::new("format");
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    let manifest_path = fx.entry_dir(&key).join(MANIFEST_FILE);
    let mut v: serde_json::Value =
        serde_json::from_slice(&fs::read(&manifest_path).unwrap()).unwrap();
    v["format"] = serde_json::json!(START_BUNDLE_FORMAT - 1);
    fs::write(&manifest_path, v.to_string()).unwrap();
    assert!(matches!(
        fx.store().restore(&fx.start),
        Err(Miss::EntryCorrupt(_))
    ));
    assert!(!fx.entry_dir(&key).exists());
}

#[test]
fn legacy_entry_without_manifest_is_a_miss() {
    let fx = Fixture::new("legacy");
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    fs::remove_file(fx.entry_dir(&key).join(MANIFEST_FILE)).unwrap();
    assert!(matches!(
        fx.store().restore(&fx.start),
        Err(Miss::EntryCorrupt(_))
    ));
    assert!(!fx.entry_dir(&key).exists(), "legacy entry removed");
}

#[test]
fn prune_evicts_lru_generations_and_sweeps_unreferenced_blobs() {
    let fx = Fixture::new("prune");
    let store = fx.store();
    let (key1, _) = store.persist(&fx.start).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    fx.write_module("a.tsx", "export const a = 2;");
    fx.write_build("bundle-v2");
    let (key2, _) = store.persist(&fx.start).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    fx.write_module("a.tsx", "export const a = 3;");
    fx.write_build("bundle-v3");
    let (key3, _) = store.persist(&fx.start).unwrap();
    assert_eq!(fx.blob_count(), 4, "3 entry blobs + 1 shared blob");

    let dir = crate::cache_root(&fx.root).join("start-bundle");
    store.prune(u64::MAX);
    assert!(dir.join(&key1).is_dir(), "under budget: nothing evicted");
    assert_eq!(fx.blob_count(), 4, "all blobs still referenced");
    store.prune(0);
    assert!(!dir.join(&key1).is_dir(), "oldest generation evicted first");
    assert!(!dir.join(&key2).is_dir());
    assert!(
        dir.join(&key3).is_dir(),
        "current generation survives zero budget"
    );
    assert_eq!(
        fx.blob_count(),
        2,
        "evicted generations' unshared blobs swept; shared blob kept"
    );
    assert!(
        fx.store().restore(&fx.start).is_ok(),
        "survivor still restores"
    );
}

#[test]
fn persist_enforces_the_generation_window() {
    // Vite keeps ONE dep-cache generation (commit replaces the old dir);
    // the store keeps a small recency window, enforced per persist, so a
    // long editing session cannot accumulate one generation per save.
    let fx = Fixture::new("window");
    let store = fx.store();
    let mut keys = Vec::new();
    for i in 0..(KEEP_GENERATIONS + 4) {
        fx.write_module("a.tsx", &format!("export const a = {i};"));
        fx.write_build(&format!("bundle-w{i}"));
        let (key, _) = store.persist(&fx.start).unwrap();
        keys.push(key);
        std::thread::sleep(Duration::from_millis(20));
    }
    let dir = crate::cache_root(&fx.root).join("start-bundle");
    let count_gens = || {
        std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                e.path().is_dir()
                    && e.file_name() != "blobs"
                    && !e.file_name().to_string_lossy().starts_with(".tmp-")
            })
            .count()
    };
    // The eviction pass is detached and each thread joins its
    // predecessor, so joining the newest joins them all: the assertions
    // below see the settled store, no polling.
    store.join_eviction();
    let swept = || {
        !std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().starts_with(".tmp-"))
    };
    let generations = count_gens();
    assert!(
        generations <= KEEP_GENERATIONS,
        "a save session must not keep a generation per save, kept {generations}"
    );
    assert!(swept(), "the background sweeper removes the renamed dirs");
    assert!(
        dir.join(keys.last().unwrap()).is_dir(),
        "the just-persisted generation survives its own prune"
    );
    assert!(
        !dir.join(&keys[0]).is_dir(),
        "the oldest generation was evicted at write time"
    );
    assert!(
        fx.store().restore(&fx.start).is_ok(),
        "current still restores after the window prune"
    );
}

#[cfg(unix)]
#[test]
fn memo_verifies_settled_files_without_reading_them() {
    use std::os::unix::fs::PermissionsExt;
    let fx = Fixture::new("memo-noread");
    fx.settle_modules();
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    for name in ["a.tsx", "b.tsx"] {
        fs::set_permissions(fx.module(name), fs::Permissions::from_mode(0o000)).unwrap();
    }
    let (stats, _) = fx.store().restore(&fx.start).unwrap();
    assert_eq!(stats.key, key);
    assert_eq!(stats.rehashed, 0, "memo hit must not read file contents");
    for name in ["a.tsx", "b.tsx"] {
        fs::set_permissions(fx.module(name), fs::Permissions::from_mode(0o644)).unwrap();
    }
}

#[test]
fn touched_but_unchanged_file_rehashes_to_the_same_key() {
    let fx = Fixture::new("memo-touch");
    fx.settle_modules();
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    fx.write_module("a.tsx", "export const a = 1;");
    let (stats, _) = fx.store().restore(&fx.start).unwrap();
    assert_eq!(stats.key, key, "same content must reach the same key");
    assert!(stats.rehashed >= 1, "stat mismatch must force a re-hash");
}

#[test]
fn racy_fresh_files_are_not_memoized() {
    let fx = Fixture::new("memo-racy");
    // Modules were written moments ago: inside the freshness window, so
    // persist must leave them out of the memo.
    fx.store().persist(&fx.start).unwrap();
    let orig_mtime = fs::metadata(fx.module("a.tsx"))
        .unwrap()
        .modified()
        .unwrap();
    // Same length, same mtime, different content: only a re-hash can see it.
    fx.write_module("a.tsx", "export const a = 9;");
    set_mtime(&fx.module("a.tsx"), orig_mtime);
    assert!(
        matches!(fx.store().restore(&fx.start), Err(Miss::NoEntryForKey(_))),
        "stat-identical edit within the racy window must still miss"
    );
}

#[test]
fn missing_memo_falls_back_to_full_hash_and_heals() {
    let fx = Fixture::new("memo-fallback");
    fx.settle_modules();
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    let memo_path = fx.entry_dir(&key).join(MEMO_FILE);
    fs::remove_file(&memo_path).unwrap();
    let (stats, _) = fx.store().restore(&fx.start).unwrap();
    assert_eq!(stats.key, key);
    assert_eq!(stats.rehashed, 2, "no memo: every file is hashed");
    assert!(memo_path.is_file(), "successful restore rewrites the memo");
    let (again, _) = fx.store().restore(&fx.start).unwrap();
    assert_eq!(again.rehashed, 0, "healed memo verifies by stat alone");
}

#[test]
fn corrupt_memo_is_dropped_and_falls_back() {
    let fx = Fixture::new("memo-corrupt");
    fx.settle_modules();
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    let memo_path = fx.entry_dir(&key).join(MEMO_FILE);
    fs::write(&memo_path, b"{ not json").unwrap();
    let (stats, _) = fx.store().restore(&fx.start).unwrap();
    assert_eq!(stats.key, key);
    assert_eq!(stats.rehashed, 2);
}
