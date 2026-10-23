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

    /// The single-directory layout, pinned to fixture-local paths so no
    /// test reaches the developer's real shared store.
    fn store_with(&self, verify: VerifyMode, mode: &str) -> StartBundleStore {
        let local = crate::cache_root(&self.root).join("start-bundle");
        StartBundleStore::at_dirs(
            &self.root,
            local.clone(),
            local,
            KEEP_GENERATIONS,
            "0.0.1-test",
            verify,
            mode,
        )
    }

    fn store(&self) -> StartBundleStore {
        self.store_with(VerifyMode::Standard, "development")
    }

    fn full_store(&self) -> StartBundleStore {
        self.store_with(VerifyMode::Full, "development")
    }

    fn memo_path(&self) -> PathBuf {
        crate::cache_root(&self.root)
            .join("start-bundle")
            .join(MEMO_FILE)
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

    /// Simulate what the client bundle leaves in .oj-cache/start:
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
        fs::write(self.start.join(GLOB_SCOPES_FILE), "[]").unwrap();
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

fn no_globs(_: &str) -> Vec<PathBuf> {
    Vec::new()
}

// Vite expands every import.meta.glob anew on each start: a file a stored
// generation's glob expands to today but its bundle never read (added while
// no server watched) misses, and the glob's members it did read restore.
#[test]
fn a_file_new_to_a_bundled_glob_misses_the_restore() {
    let fx = Fixture::new("globgrew");
    fs::write(
        fx.start.join(GLOB_SCOPES_FILE),
        r#"[{"patterns":["src/*.tsx"]}]"#,
    )
    .unwrap();
    fx.store().persist(&fx.start).unwrap();
    fx.clear_start_dir();
    let src = fx.root.join("src");
    let members = |scopes: &str| {
        assert!(scopes.contains("src/*.tsx"));
        let mut files: Vec<PathBuf> = fs::read_dir(&src)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        files.sort();
        files
    };
    assert!(fx.store().restore(&fx.start, members).is_ok());
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(
            &fs::read_to_string(fx.start.join(GLOB_SCOPES_FILE)).unwrap()
        )
        .unwrap(),
        serde_json::json!([{"patterns":["src/*.tsx"]}]),
        "the restore brings the calls back for the dev gate"
    );
    fx.write_module("c.tsx", "export const c = 3;");
    assert_eq!(
        fx.store().restore(&fx.start, members).map(|_| ()),
        Err(Miss::GlobGrew(fx.module("c.tsx")))
    );
}

#[test]
fn restore_misses_without_previous_build() {
    let fx = Fixture::new("nolatest");
    assert!(matches!(
        fx.store().restore(&fx.start, no_globs),
        Err(Miss::NoPreviousBuild)
    ));
}

#[test]
fn persist_then_restore_roundtrips_artifacts() {
    let fx = Fixture::new("roundtrip");
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    fx.clear_start_dir();
    let (stats, pinned) = fx.store().restore(&fx.start, no_globs).unwrap();
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
    match fx.store().restore(&fx.start, no_globs) {
        Err(Miss::NoEntryForKey(k)) => assert_ne!(k, key),
        other => panic!("expected key miss, got {other:?}"),
    }
    fx.write_build("bundle-v2");
    let (key2, _) = fx.store().persist(&fx.start).unwrap();
    assert_ne!(key2, key);
    fx.clear_start_dir();
    assert_eq!(fx.store().restore(&fx.start, no_globs).unwrap().0.key, key2);
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
    let (stats, pinned) = fx.store().restore(&fx.start, no_globs).unwrap();
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
        fx.store().restore(&fx.start, no_globs),
        Err(Miss::ClosureFileUnreadable(_))
    ));
}

#[test]
fn epoch_input_changes_the_key() {
    let fx = Fixture::new("epoch");
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    fs::write(fx.root.join("package.json"), b"{\"name\":\"x\"}").unwrap();
    match fx.store().restore(&fx.start, no_globs) {
        Err(Miss::NoEntryForKey(k)) => assert_ne!(k, key),
        other => panic!("expected key miss, got {other:?}"),
    }
    let local = crate::cache_root(&fx.root).join("start-bundle");
    let other_version = StartBundleStore::at_dirs(
        &fx.root,
        local.clone(),
        local,
        KEEP_GENERATIONS,
        "9.9.9",
        VerifyMode::Standard,
        "development",
    )
    .persist(&fx.start);
    assert_ne!(other_version.unwrap().0, key, "tool version salts the key");
}

#[test]
fn dev_mode_and_its_env_file_change_the_key() {
    let fx = Fixture::new("mode");
    let staging = |_: &Path| fx.store_with(VerifyMode::Standard, "staging");
    let (dev_key, _) = fx.store().persist(&fx.start).unwrap();
    let (staging_key, _) = staging(&fx.root).persist(&fx.start).unwrap();
    assert_ne!(staging_key, dev_key, "the mode salts the key");
    fs::write(fx.root.join(".env.staging"), b"VITE_FLAVOR=staging\n").unwrap();
    match staging(&fx.root).restore(&fx.start, no_globs) {
        Err(Miss::NoEntryForKey(k)) => assert_ne!(k, staging_key, ".env.<mode> is a key input"),
        other => panic!("expected key miss, got {other:?}"),
    }
    fs::write(fx.root.join(".env.development"), b"VITE_FLAVOR=dev\n").unwrap();
    match fx.store().restore(&fx.start, no_globs) {
        Err(Miss::NoEntryForKey(k)) => assert_ne!(k, dev_key),
        other => panic!("expected key miss, got {other:?}"),
    }
}

#[test]
fn missing_blob_fails_verification_and_removes_the_entry() {
    let fx = Fixture::new("missing-blob");
    let (key, pinned) = fx.store().persist(&fx.start).unwrap();
    fs::remove_file(&pinned.chunk("shared-x.js").unwrap().path).unwrap();
    match fx.store().restore(&fx.start, no_globs) {
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
        fx.store().restore(&fx.start, no_globs),
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
    assert!(fx.store().restore(&fx.start, no_globs).is_ok());
    // Full mode re-hashes, detects, removes blob + entry, and misses.
    match fx.full_store().restore(&fx.start, no_globs) {
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
    assert!(fx.full_store().restore(&fx.start, no_globs).is_ok());
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
        fx.store().restore(&fx.start, no_globs),
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
        fx.store().restore(&fx.start, no_globs),
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
        fx.store().restore(&fx.start, no_globs).is_ok(),
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
                    && e.file_name() != "pins"
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
        fx.store().restore(&fx.start, no_globs).is_ok(),
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
    let (stats, _) = fx.store().restore(&fx.start, no_globs).unwrap();
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
    let (stats, _) = fx.store().restore(&fx.start, no_globs).unwrap();
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
        matches!(
            fx.store().restore(&fx.start, no_globs),
            Err(Miss::NoEntryForKey(_))
        ),
        "stat-identical edit within the racy window must still miss"
    );
}

#[test]
fn missing_memo_falls_back_to_full_hash_and_heals() {
    let fx = Fixture::new("memo-fallback");
    fx.settle_modules();
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    let memo_path = fx.memo_path();
    fs::remove_file(&memo_path).unwrap();
    let (stats, _) = fx.store().restore(&fx.start, no_globs).unwrap();
    assert_eq!(stats.key, key);
    assert_eq!(stats.rehashed, 2, "no memo: every file is hashed");
    assert!(memo_path.is_file(), "successful restore rewrites the memo");
    let (again, _) = fx.store().restore(&fx.start, no_globs).unwrap();
    assert_eq!(again.rehashed, 0, "healed memo verifies by stat alone");
}

#[test]
fn corrupt_memo_is_dropped_and_falls_back() {
    let fx = Fixture::new("memo-corrupt");
    fx.settle_modules();
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    let memo_path = fx.memo_path();
    fs::write(&memo_path, b"{ not json").unwrap();
    let (stats, _) = fx.store().restore(&fx.start, no_globs).unwrap();
    assert_eq!(stats.key, key);
    assert_eq!(stats.rehashed, 2);
}

#[test]
fn identical_worktrees_compute_one_bundle_key() {
    let a = Fixture::new("xwt-key-a");
    let b = Fixture::new("xwt-key-b");
    let (key_a, _) = a.store().persist(&a.start).unwrap();
    let (key_b, _) = b.store().persist(&b.start).unwrap();
    assert_eq!(key_a, key_b, "same content in two roots keys identically");
}

#[test]
fn an_edit_still_changes_the_key_across_worktrees() {
    let a = Fixture::new("xwt-edit-a");
    let b = Fixture::new("xwt-edit-b");
    b.write_module("a.tsx", "export const a = 999;");
    let (key_a, _) = a.store().persist(&a.start).unwrap();
    let (key_b, _) = b.store().persist(&b.start).unwrap();
    assert_ne!(key_a, key_b);
}

#[test]
fn the_stored_closure_is_root_relative() {
    let fx = Fixture::new("rel-closure");
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    let stored: Vec<String> =
        serde_json::from_slice(&fs::read(fx.entry_dir(&key).join(CLOSURE_FILE)).unwrap()).unwrap();
    assert!(!stored.is_empty());
    for entry in &stored {
        assert!(entry.starts_with("ws:"), "stored relative, got {entry}");
        assert!(
            !entry.contains("oj-start-bundle-test"),
            "no root leak: {entry}"
        );
    }
}

#[test]
fn memo_survives_path_spelling_differences() {
    // macOS temp_dir is /var/..., canonical is /private/var/...: the memo is
    // keyed by closure id, so either spelling hits it.
    let fx = Fixture::new("memo-spelling");
    fx.settle_modules();
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    let (stats, _) = fx.store().restore(&fx.start, no_globs).unwrap();
    assert_eq!(stats.key, key);
    assert_eq!(stats.rehashed, 0);
}

/// Generations and blobs in `shared`, pointers under each app.
fn split_store_at(root: &Path, shared: &Path, keep: usize) -> StartBundleStore {
    StartBundleStore::at_dirs(
        root,
        shared.join("start-bundle"),
        crate::cache_root(root).join("start-bundle"),
        keep,
        "0.0.1-test",
        VerifyMode::Standard,
        "development",
    )
}

fn shared_base(label: &str) -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "oj-start-bundle-shared-{}-{label}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&base);
    base
}

#[test]
fn cold_worktree_restores_through_the_probe() {
    let shared = shared_base("probe");
    let a = Fixture::new("split-probe-a");
    let b = Fixture::new("split-probe-b");
    let (key_a, _) = split_store_at(&a.root, &shared, SHARED_KEEP_GENERATIONS)
        .persist(&a.start)
        .unwrap();

    // B is cold: no current, no memo, empty start dir.
    b.clear_start_dir();
    let (stats, pinned) = split_store_at(&b.root, &shared, SHARED_KEEP_GENERATIONS)
        .restore(&b.start, no_globs)
        .unwrap();
    assert_eq!(
        stats.key, key_a,
        "the probe predicted A's closure and B's files agreed"
    );
    assert!(!pinned.is_empty());
    assert!(b.start.join("manifest.ts").is_file());
    let local = crate::cache_root(&b.root).join("start-bundle");
    assert!(local.join(CURRENT_FILE).is_file(), "current is per-root");
    assert!(local.join(MEMO_FILE).is_file(), "memo is per-root");
    let pins = fs::read_dir(shared.join("start-bundle").join(PINS_DIR))
        .unwrap()
        .count();
    assert_eq!(pins, 2, "one pin per root");
}

#[test]
fn an_edited_cold_worktree_misses_instead_of_taking_the_wrong_bundle() {
    let shared = shared_base("probe-miss");
    let a = Fixture::new("split-miss-a");
    let b = Fixture::new("split-miss-b");
    split_store_at(&a.root, &shared, SHARED_KEEP_GENERATIONS)
        .persist(&a.start)
        .unwrap();
    b.write_module("a.tsx", "export const a = 999;");
    let err = split_store_at(&b.root, &shared, SHARED_KEEP_GENERATIONS)
        .restore(&b.start, no_globs)
        .unwrap_err();
    assert!(matches!(err, Miss::NoEntryForKey(_)), "{err:?}");
}

#[test]
fn another_roots_pin_survives_eviction() {
    let shared = shared_base("pin-protect");
    let a = Fixture::new("split-pin-a");
    let b = Fixture::new("split-pin-b");
    let (key_a, _) = split_store_at(&a.root, &shared, 2)
        .persist(&a.start)
        .unwrap();

    // B churns generations through a window of 2, which without pins would
    // evict A's generation and its blobs.
    let store_b = split_store_at(&b.root, &shared, 2);
    for i in 0..4 {
        b.write_module("a.tsx", &format!("export const a = {i};"));
        b.write_build(&format!("bundle-b{i}"));
        store_b.persist(&b.start).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    store_b.join_eviction();
    store_b.prune(DEFAULT_PRUNE_BUDGET_BYTES);

    assert!(shared.join("start-bundle").join(&key_a).is_dir());
    let (stats, _) = split_store_at(&a.root, &shared, 2)
        .restore(&a.start, no_globs)
        .unwrap();
    assert_eq!(stats.key, key_a, "A still restores, blobs included");
}

#[test]
fn stale_pins_stop_protecting_and_are_removed() {
    let shared = shared_base("pin-stale");
    let a = Fixture::new("split-stale-a");
    let b = Fixture::new("split-stale-b");
    let store_a = split_store_at(&a.root, &shared, 2);
    let (key_a, _) = store_a.persist(&a.start).unwrap();
    // A's checkout is gone: its pointers with it, its pin and its
    // generation's recency aged past the freshness window.
    let _ = fs::remove_dir_all(&a.root);
    let old = SystemTime::now() - Duration::from_secs(8 * 24 * 60 * 60);
    let pin = shared
        .join("start-bundle")
        .join(PINS_DIR)
        .join(&store_a.pin_id);
    set_mtime(&pin, old);
    set_mtime(
        &shared.join("start-bundle").join(&key_a).join(TOUCH_FILE),
        old,
    );

    let store_b = split_store_at(&b.root, &shared, 2);
    for i in 0..4 {
        b.write_module("a.tsx", &format!("export const a = {i};"));
        b.write_build(&format!("bundle-b{i}"));
        store_b.persist(&b.start).unwrap();
        std::thread::sleep(Duration::from_millis(20));
    }
    store_b.join_eviction();

    assert!(
        !shared.join("start-bundle").join(&key_a).is_dir(),
        "a dead worktree's generation is evictable"
    );
    assert!(!pin.is_file(), "the stale pin file itself is cleaned up");
}

/// glob-scopes.json for one call over `patterns`.
fn glob_scopes(patterns: &[String]) -> String {
    serde_json::json!([{ "patterns": patterns, "exhaustive": true, "caseSensitive": true }])
        .to_string()
}

// The bundle's glob patterns are absolute: a shared generation stores them
// root-relative and hands each worktree its own, or B would expand A's
// checkout and its dev gate would watch A's files.
#[test]
fn in_root_globs_rehome_onto_the_restoring_worktree() {
    let shared = shared_base("glob-rehome");
    let a = Fixture::new("split-glob-a");
    let b = Fixture::new("split-glob-b");
    let pattern = |fx: &Fixture| vec![format!("{}/src/*.tsx", fx.root.display())];
    fs::write(a.start.join(GLOB_SCOPES_FILE), glob_scopes(&pattern(&a))).unwrap();
    let (key_a, _) = split_store_at(&a.root, &shared, SHARED_KEEP_GENERATIONS)
        .persist(&a.start)
        .unwrap();
    let stored = fs::read_to_string(
        shared
            .join("start-bundle")
            .join(&key_a)
            .join(GLOB_SCOPES_FILE),
    )
    .unwrap();
    assert!(
        !stored.contains("split-glob-a"),
        "stored without A's root: {stored}"
    );

    b.clear_start_dir();
    let seen = std::cell::RefCell::new(String::new());
    let (stats, _) = split_store_at(&b.root, &shared, SHARED_KEEP_GENERATIONS)
        .restore(&b.start, |scopes: &str| {
            *seen.borrow_mut() = scopes.to_string();
            Vec::new()
        })
        .unwrap();
    assert_eq!(stats.key, key_a);
    let expect: serde_json::Value = serde_json::from_str(&glob_scopes(&pattern(&b))).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&seen.borrow()).unwrap(),
        expect
    );
    let restored = fs::read_to_string(b.start.join(GLOB_SCOPES_FILE)).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&restored).unwrap(),
        expect
    );
}

// A glob reaching outside the root cannot be rehomed, so its generation is
// keyed to the root that built it: it still restores there, never elsewhere.
#[test]
fn an_out_of_root_glob_keeps_the_generation_to_its_root() {
    let shared = shared_base("glob-pin");
    let a = Fixture::new("split-globpin-a");
    let b = Fixture::new("split-globpin-b");
    let outside = vec!["/elsewhere/shared/*.ts".to_string()];
    fs::write(a.start.join(GLOB_SCOPES_FILE), glob_scopes(&outside)).unwrap();
    let store_a = split_store_at(&a.root, &shared, SHARED_KEEP_GENERATIONS);
    let (key_a, _) = store_a.persist(&a.start).unwrap();
    a.clear_start_dir();
    assert_eq!(store_a.restore(&a.start, no_globs).unwrap().0.key, key_a);

    b.clear_start_dir();
    let err = split_store_at(&b.root, &shared, SHARED_KEEP_GENERATIONS)
        .restore(&b.start, no_globs)
        .unwrap_err();
    assert!(
        matches!(&err, Miss::NoEntryForKey(k) if *k != key_a),
        "{err:?}"
    );
}

// Inputs only the caller knows (the config the app was started with) key
// the bundle: a generation persisted under one never restores under another.
#[test]
fn the_extra_salt_keys_the_bundle() {
    let fx = Fixture::new("extra-salt");
    let (key, _) = fx
        .store()
        .with_extra_salt(b"config-a")
        .persist(&fx.start)
        .unwrap();
    let (again, _) = fx
        .store()
        .with_extra_salt(b"config-a")
        .persist(&fx.start)
        .unwrap();
    assert_eq!(key, again);
    match fx
        .store()
        .with_extra_salt(b"config-b")
        .restore(&fx.start, no_globs)
    {
        Err(Miss::NoEntryForKey(k)) => assert_ne!(k, key),
        other => panic!("expected key miss, got {other:?}"),
    }
}

// A closure file reached through a symlink out of the root (a linked
// package, a symlinked node_modules) ids by its canonical path, which sorts
// apart from its in-root spelling, and may appear under both spellings. The
// key a persist computes must be the one a restore recomputes from the
// stored closure, or every boot misses.
#[cfg(unix)]
#[test]
fn a_closure_reaching_out_of_the_root_restores_under_its_own_key() {
    let fx = Fixture::new("symlinked");
    let outside = fx
        .root
        .with_file_name(format!("oj-start-bundle-outside-{}", std::process::id()));
    let _ = fs::remove_dir_all(&outside);
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("z.tsx"), "export const z = 1;").unwrap();
    std::os::unix::fs::symlink(outside.join("z.tsx"), fx.module("link.tsx")).unwrap();
    let closure = vec![
        fx.module("a.tsx").display().to_string(),
        fx.module("b.tsx").display().to_string(),
        fx.module("link.tsx").display().to_string(),
        outside.join("z.tsx").display().to_string(),
    ];
    fs::write(
        fx.start.join(CLOSURE_FILE),
        serde_json::to_vec(&closure).unwrap(),
    )
    .unwrap();
    let (key, _) = fx.store().persist(&fx.start).unwrap();
    fx.clear_start_dir();
    let (stats, _) = fx.store().restore(&fx.start, no_globs).unwrap();
    assert_eq!(stats.key, key);
    assert_eq!(
        stats.files, 3,
        "one id for both spellings of the linked file"
    );
    let _ = fs::remove_dir_all(&outside);
}

// A restore into a build dir that never built (another worktree's
// generation) leaves the chunk index a consumer reads and the closure the
// dev rebundle gate consults; the chunks stay in the blob store.
#[test]
fn a_restore_leaves_the_build_dirs_index_and_closure() {
    let shared = shared_base("build-view");
    let a = Fixture::new("view-a");
    let b = Fixture::new("view-b");
    split_store_at(&a.root, &shared, SHARED_KEEP_GENERATIONS)
        .persist(&a.start)
        .unwrap();
    b.clear_start_dir();
    let _ = fs::remove_file(b.start.join(CHUNK_INDEX_FILE));
    let _ = fs::remove_file(b.start.join(CLOSURE_FILE));
    let (_, pinned) = split_store_at(&b.root, &shared, SHARED_KEEP_GENERATIONS)
        .restore(&b.start, no_globs)
        .unwrap();
    let index: serde_json::Value =
        serde_json::from_slice(&fs::read(b.start.join(CHUNK_INDEX_FILE)).unwrap()).unwrap();
    assert_eq!(index["entry"], "client-entry.js");
    let names: Vec<&str> = index["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f["name"].as_str().unwrap())
        .collect();
    assert!(names.iter().all(|n| pinned.chunk(n).is_some()), "{names:?}");
    let closure: Vec<String> =
        serde_json::from_slice(&fs::read(b.start.join(CLOSURE_FILE)).unwrap()).unwrap();
    assert_eq!(closure.len(), 2);
    assert!(
        closure.iter().all(|f| f.contains("view-b")),
        "the closure names this worktree's files: {closure:?}"
    );
    assert!(!b.start.join(CHUNKS_DIR).exists(), "no chunk bytes copied");
}
