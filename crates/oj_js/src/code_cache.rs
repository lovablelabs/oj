// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Persistent V8 code cache: one file per on-disk module,
//! `[u64 source hash][data]`, so a changed source misses instead of executing
//! stale bytecode. Best-effort: a broken or read-only cache only costs the
//! speedup. Serves all three compile paths — ES modules (the loader's
//! `SourceCodeCacheInfo` + `code_cache_ready`), CJS `require`
//! (`WorkerServiceOptions::v8_code_cache`), and residual lazy ext scripts
//! (`get_code_cache`) — plus deno_resolver's [`NodeAnalysisCache`], whose swc
//! CJS-export analysis is a repeat cost of its own.

use std::hash::Hasher;
use std::path::PathBuf;

use deno_core::url::Url;
use deno_resolver::cjs::analyzer::DenoCjsAnalysis;
use deno_resolver::cjs::analyzer::NodeAnalysisCache;
use deno_resolver::cjs::analyzer::NodeAnalysisCacheSourceHash;
use deno_runtime::code_cache::CodeCache;
use deno_runtime::code_cache::CodeCacheType;

pub struct FsCodeCache {
    dir: PathBuf,
}

/// The key callers partition persistent engine caches by: the V8 version,
/// which is the bytecode ABI. Keying on the embedder's release version
/// cold-started every engine on every version bump. Correctness never rests
/// on this key — entries embed a source hash and V8 rejects foreign
/// bytecode — it only keeps incompatible generations from piling up as dead
/// weight.
pub fn engine_abi_key() -> String {
    format!("v8-{}", deno_core::v8::VERSION_STRING)
}

/// Stable across processes: `DefaultHasher::new()` is keyless SipHash, so two
/// runs of the same binary agree (a toolchain upgrade that changes it merely
/// misses; the embedded source hash keeps correctness either way).
fn hash64(bytes: &[u8]) -> u64 {
    let mut hasher = std::hash::DefaultHasher::new();
    hasher.write(bytes);
    hasher.finish()
}

impl FsCodeCache {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    /// The engine-side hash for sources it caches itself (the ESM and
    /// ext-script paths choose their own hash; the CJS path receives one).
    pub fn source_hash(source: &[u8]) -> u64 {
        hash64(source)
    }

    /// The entry key strips the volatile cache-busting params (`v`, `t`):
    /// keying on the raw URL wrote one unreachable entry per edit (tens of GB
    /// on long-lived checkouts). An edit overwrites in place — the embedded
    /// source hash guards staleness — and an unedited module hits across
    /// restarts. Intent params (`?url`, `?raw`) stay: their compiled forms
    /// differ.
    fn entry_key(specifier: &Url) -> u64 {
        if specifier.query().is_none() && specifier.fragment().is_none() {
            return hash64(specifier.as_str().as_bytes());
        }
        let kept: Vec<&str> = specifier
            .query()
            .unwrap_or("")
            .split('&')
            .filter(|p| {
                let name = p.split('=').next().unwrap_or(p);
                !p.is_empty() && name != "v" && name != "t"
            })
            .collect();
        let mut base = specifier.clone();
        base.set_fragment(None);
        if kept.is_empty() {
            base.set_query(None);
        } else {
            base.set_query(Some(&kept.join("&")));
        }
        hash64(base.as_str().as_bytes())
    }

    fn entry_path(&self, specifier: &Url, suffix: &str) -> PathBuf {
        self.dir
            .join(format!("{:016x}-{suffix}.bin", Self::entry_key(specifier)))
    }

    fn kind_suffix(kind: CodeCacheType) -> &'static str {
        match kind {
            CodeCacheType::EsModule => "esm",
            CodeCacheType::Script => "cjs",
        }
    }

    fn get_entry(&self, specifier: &Url, suffix: &str, source_hash: u64) -> Option<Vec<u8>> {
        let bytes = std::fs::read(self.entry_path(specifier, suffix)).ok()?;
        let (head, data) = bytes.split_at_checked(8)?;
        if head != source_hash.to_le_bytes() {
            return None;
        }
        Some(data.to_vec())
    }

    fn put_entry(&self, specifier: &Url, suffix: &str, source_hash: u64, data: &[u8]) {
        let path = self.entry_path(specifier, suffix);
        if std::fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        // Atomic publish: write-then-rename, so a concurrent reader never
        // sees a torn half-write. The tmp name carries a process-wide
        // sequence beside the pid: worker threads share this cache, and a
        // pid-only suffix let two threads rename a torn file into place.
        static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp{}-{seq}", std::process::id()));
        let mut bytes = Vec::with_capacity(8 + data.len());
        bytes.extend_from_slice(&source_hash.to_le_bytes());
        bytes.extend_from_slice(data);
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    pub fn get(&self, specifier: &Url, kind: CodeCacheType, source_hash: u64) -> Option<Vec<u8>> {
        self.get_entry(specifier, Self::kind_suffix(kind), source_hash)
    }

    pub fn put(&self, specifier: &Url, kind: CodeCacheType, source_hash: u64, data: &[u8]) {
        self.put_entry(specifier, Self::kind_suffix(kind), source_hash, data);
    }

    /// Removes torn-write leftovers (`.tmp*` files) older than `max_age`:
    /// Vite's deps-cache boot hygiene (cleanupDepsCacheStaleDirs, 24h). Live
    /// tmp files from a concurrent engine are younger and survive.
    pub fn sweep_stale_tmp(&self, max_age: std::time::Duration) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let now = std::time::SystemTime::now();
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if !name.contains(".tmp") {
                continue;
            }
            let stale = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age > max_age);
            if stale {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Vite's MAX_TEMP_DIR_AGE_MS for its deps-cache temp dirs: 24 hours.
pub const STALE_TMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

impl CodeCache for FsCodeCache {
    fn get_sync(
        &self,
        specifier: &Url,
        code_cache_type: CodeCacheType,
        source_hash: u64,
    ) -> Option<Vec<u8>> {
        self.get(specifier, code_cache_type, source_hash)
    }

    fn set_sync(
        &self,
        specifier: Url,
        code_cache_type: CodeCacheType,
        source_hash: u64,
        data: &[u8],
    ) {
        self.put(&specifier, code_cache_type, source_hash, data);
    }
}

impl NodeAnalysisCache for FsCodeCache {
    fn compute_source_hash(&self, source: &str) -> NodeAnalysisCacheSourceHash {
        NodeAnalysisCacheSourceHash(hash64(source.as_bytes()))
    }

    fn get_cjs_analysis(
        &self,
        specifier: &Url,
        source_hash: NodeAnalysisCacheSourceHash,
    ) -> Option<DenoCjsAnalysis> {
        let bytes = self.get_entry(specifier, "ana", source_hash.0)?;
        serde_json::from_slice(&bytes).ok()
    }

    fn set_cjs_analysis(
        &self,
        specifier: &Url,
        source_hash: NodeAnalysisCacheSourceHash,
        analysis: &DenoCjsAnalysis,
    ) {
        if let Ok(bytes) = serde_json::to_vec(analysis) {
            self.put_entry(specifier, "ana", source_hash.0, &bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_source_hash_guard() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsCodeCache::new(dir.path().join("cc"));
        let url = Url::parse("file:///app/node_modules/dep/index.js").unwrap();

        assert_eq!(cache.get(&url, CodeCacheType::EsModule, 7), None);
        cache.put(&url, CodeCacheType::EsModule, 7, b"bytecode");
        assert_eq!(
            cache.get(&url, CodeCacheType::EsModule, 7).as_deref(),
            Some(b"bytecode".as_slice())
        );
        // A different source hash (edited file) must miss, not serve stale.
        assert_eq!(cache.get(&url, CodeCacheType::EsModule, 8), None);
        // Kinds are separate entries.
        assert_eq!(cache.get(&url, CodeCacheType::Script, 7), None);
    }

    // The cache partition must follow the V8 version (the bytecode ABI), not
    // the embedder's release version: an oj version bump used to rotate the
    // directory and cold-start every engine (~+1s per one-shot child on an
    // 18k-module app), while entries stay valid across such bumps -- the
    // roundtrip test above is what invalidates on a source change.
    #[test]
    fn abi_key_is_the_v8_version_not_the_crate_version() {
        let key = engine_abi_key();
        assert_eq!(key, format!("v8-{}", deno_core::v8::VERSION_STRING));
        // Guard against reintroducing release-version keying. (Skip the
        // assert only in the pathological case where the V8 version string
        // itself embeds the crate version.)
        let crate_version = env!("CARGO_PKG_VERSION");
        if !deno_core::v8::VERSION_STRING.contains(crate_version) {
            assert!(!key.contains(crate_version));
        }
    }
}

#[cfg(test)]
mod hygiene_tests {
    use super::*;

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    // The bug that grew long-lived checkouts by tens of GB: every edit bumps
    // `?v=N`, and raw-URL keying wrote a fresh, permanently unreachable entry
    // per bump. Volatile params must collapse to one overwritten entry.
    #[test]
    fn version_bumps_overwrite_one_entry_instead_of_accumulating() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsCodeCache::new(dir.path().to_path_buf());
        for v in 1..=5u32 {
            let spec = url(&format!("oj:///src/App.tsx?v={v}"));
            cache.put(
                &spec,
                CodeCacheType::EsModule,
                u64::from(v),
                format!("bytecode-{v}").as_bytes(),
            );
        }
        let entries = std::fs::read_dir(dir.path()).unwrap().count();
        assert_eq!(entries, 1, "five version bumps must reuse one entry");
        // The latest generation is served; a stale source hash misses.
        let spec = url("oj:///src/App.tsx?v=5");
        assert_eq!(
            cache.get(&spec, CodeCacheType::EsModule, 5).as_deref(),
            Some(b"bytecode-5".as_ref())
        );
        assert_eq!(cache.get(&spec, CodeCacheType::EsModule, 4), None);
    }

    // Versions reset when the dev server restarts; an unedited module (same
    // source hash) must hit whatever version its URL carries, or warm boots
    // recompile the whole app graph.
    #[test]
    fn an_unedited_module_hits_across_a_version_reset() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsCodeCache::new(dir.path().to_path_buf());
        cache.put(
            &url("oj:///src/App.tsx?v=7"),
            CodeCacheType::EsModule,
            42,
            b"bytecode",
        );
        assert_eq!(
            cache
                .get(&url("oj:///src/App.tsx?v=1"), CodeCacheType::EsModule, 42)
                .as_deref(),
            Some(b"bytecode".as_ref())
        );
        // `t` is the other cache-busting param convention; fragments never key.
        assert_eq!(
            cache
                .get(
                    &url("oj:///src/App.tsx?t=123#frag"),
                    CodeCacheType::EsModule,
                    42
                )
                .as_deref(),
            Some(b"bytecode".as_ref())
        );
    }

    // Intent params compile differently (`?url` is a string module, `?raw`
    // the file text), so they keep entries of their own.
    #[test]
    fn intent_params_keep_their_own_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsCodeCache::new(dir.path().to_path_buf());
        cache.put(
            &url("file:///a/logo.svg?url&v=1"),
            CodeCacheType::EsModule,
            1,
            b"as-url",
        );
        cache.put(
            &url("file:///a/logo.svg?raw&v=2"),
            CodeCacheType::EsModule,
            2,
            b"as-raw",
        );
        cache.put(
            &url("file:///a/logo.svg"),
            CodeCacheType::EsModule,
            3,
            b"plain",
        );
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 3);
        assert_eq!(
            cache
                .get(
                    &url("file:///a/logo.svg?url&v=9"),
                    CodeCacheType::EsModule,
                    1
                )
                .as_deref(),
            Some(b"as-url".as_ref())
        );
    }

    // Worker threads share one cache within a process (a pool compiling the
    // same hot module races): every writer must publish a whole entry and
    // strand nothing. A pid-only tmp suffix let two threads write the same
    // tmp path and rename a torn file into place.
    #[test]
    fn concurrent_same_module_puts_from_threads_publish_whole_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = std::sync::Arc::new(FsCodeCache::new(dir.path().to_path_buf()));
        let spec = url("file:///app/node_modules/terser/main.js");
        let payload = vec![7u8; 64 * 1024];
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = std::sync::Arc::clone(&cache);
            let spec = spec.clone();
            let payload = payload.clone();
            handles.push(std::thread::spawn(move || {
                for _ in 0..50 {
                    cache.put(&spec, CodeCacheType::EsModule, 1, &payload);
                }
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(
            cache.get(&spec, CodeCacheType::EsModule, 1).as_deref(),
            Some(payload.as_slice()),
            "the published entry must be whole"
        );
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "no stranded tmp files");
    }

    // Boot hygiene mirrors Vite's deps-cache cleanup: only torn-write tmp
    // leftovers past the age threshold go; entries and fresh tmp files stay.
    #[test]
    fn sweep_removes_only_stale_tmp_leftovers() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsCodeCache::new(dir.path().to_path_buf());
        cache.put(
            &url("file:///m.js"),
            CodeCacheType::EsModule,
            1,
            b"bytecode",
        );
        let stale = dir.path().join("deadbeef-esm.bin.tmp999");
        std::fs::write(&stale, b"torn").unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 60 * 60);
        std::fs::File::options()
            .append(true)
            .open(&stale)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
        std::fs::write(dir.path().join("cafebabe-esm.bin.tmp111"), b"in flight").unwrap();
        cache.sweep_stale_tmp(STALE_TMP_MAX_AGE);
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(
            !names.iter().any(|n| n.ends_with(".tmp999")),
            "stale tmp swept: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.ends_with(".tmp111")),
            "fresh tmp kept: {names:?}"
        );
        assert_eq!(
            names.iter().filter(|n| n.ends_with(".bin")).count(),
            1,
            "entry kept: {names:?}"
        );
    }
}
