// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Persistent V8 code cache for the engine: compiled-bytecode blobs for the
//! modules an engine loads from disk (an app's toolchain — a bundler's JS
//! wrapper, a config's plugin graph — is megabytes of JS re-parsed by every
//! one-shot child otherwise). One file per module under a caller-chosen
//! directory (version-keyed by the caller), `[u64 source hash][data]`, so a
//! changed source misses instead of executing stale bytecode. Reads and
//! writes are best-effort: a broken or read-only cache only costs the speedup.
//!
//! Serves all three compile paths:
//! - ES modules: the loader attaches [`deno_core::SourceCodeCacheInfo`] to
//!   each `ModuleSource` and persists through `code_cache_ready`.
//! - CJS (`require`): deno_runtime's eval-context callbacks, wired through
//!   `WorkerServiceOptions::v8_code_cache` ([`CodeCache`] below).
//! - Residual lazy ext scripts: the loader's `get_code_cache` hook.
//!
//! The same store also backs deno_resolver's [`NodeAnalysisCache`]: CJS
//! export analysis parses every CommonJS module with swc before any V8
//! compile exists to cache, so on a big toolchain the analysis is a repeat
//! cost of its own.

use std::hash::Hasher;
use std::path::PathBuf;

use deno_core::url::Url;
use deno_resolver::cjs::analyzer::DenoCjsAnalysis;
use deno_resolver::cjs::analyzer::NodeAnalysisCache;
use deno_resolver::cjs::analyzer::NodeAnalysisCacheSourceHash;
use deno_runtime::code_cache::CodeCache;
use deno_runtime::code_cache::CodeCacheType;

pub(crate) struct FsCodeCache {
    dir: PathBuf,
}

/// The compatibility key callers should partition persistent engine caches by
/// (`EngineConfig::code_cache_dir`): the V8 version, which is the bytecode
/// ABI. Keying on the embedder's own release version cold-started every
/// engine on every version bump; a release that upgrades no engine crate now
/// keeps the whole warm cache. Correctness never rests on this key: each
/// entry embeds its source hash (below), V8 itself rejects cached data from a
/// different V8 build or flag set, and the CJS-analysis entries fail
/// deserialization on a shape change -- all graceful misses. The key is
/// housekeeping, keeping incompatible generations from mixing in one
/// directory as dead weight.
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

    fn entry_path(&self, specifier: &Url, suffix: &str) -> PathBuf {
        self.dir
            .join(format!("{:016x}-{suffix}.bin", hash64(specifier.as_str().as_bytes())))
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
        // Atomic publish: a concurrent child reads either the old entry or
        // the new one, never a torn half-write.
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
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
}

impl CodeCache for FsCodeCache {
    fn get_sync(
        &self,
        specifier: &Url,
        code_cache_type: CodeCacheType,
        source_hash: u64,
    ) -> Option<Vec<u8>> {
        self.get(specifier, code_cache_type, source_hash)
    }

    fn set_sync(&self, specifier: Url, code_cache_type: CodeCacheType, source_hash: u64, data: &[u8]) {
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
