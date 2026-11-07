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

use std::collections::HashMap;
use std::hash::Hasher;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use deno_core::url::Url;
use deno_resolver::cjs::analyzer::DenoCjsAnalysis;
use deno_resolver::cjs::analyzer::NodeAnalysisCache;
use deno_resolver::cjs::analyzer::NodeAnalysisCacheSourceHash;
use deno_runtime::code_cache::CodeCache;
use deno_runtime::code_cache::CodeCacheType;

pub struct FsCodeCache {
    dir: PathBuf,
    /// False in a process another one waits on (see [`consume_only`]).
    produce: bool,
}

/// Node's compile cache, which Vite's CLI enables, keeps entries in memory
/// and writes them in batches (`module.flushCompileCache()`), not one file
/// per module during the import: written inline, a cold import of Vite and
/// the Start plugin spent a third of its time creating files. One store for
/// the whole process, so an entry one engine compiled is a hit for every
/// other engine before it reaches disk. Keyed by entry path: the newest
/// bytes for a module win.
struct Held {
    entries: HashMap<PathBuf, Arc<[u8]>>,
    /// Sum of the held entries' lengths, against [`HELD_MAX_BYTES`].
    bytes: usize,
    /// A timed flush is scheduled. Each one re-arms on the next put, so a
    /// long-lived engine writes what it compiles late as well.
    armed: bool,
    /// An early flush (over [`HELD_MAX_BYTES`]) is running.
    draining: bool,
}

static HELD: LazyLock<Mutex<Held>> = LazyLock::new(|| {
    Mutex::new(Held {
        entries: HashMap::new(),
        bytes: 0,
        armed: false,
        draining: false,
    })
});

/// Past this, a burst (a big app's first boot) is written now instead of
/// waiting out the timer in memory.
const HELD_MAX_BYTES: usize = 32 << 20;

fn held() -> std::sync::MutexGuard<'static, Held> {
    HELD.lock().unwrap_or_else(|e| e.into_inner())
}

/// Vite's `setTimeout(() => module.flushCompileCache(), 10 * 1000)`.
const FLUSH_AFTER: Duration = if cfg!(test) {
    Duration::from_millis(200)
} else {
    Duration::from_secs(10)
};

/// Writes every held entry now. An `atexit` hook runs it for every exit
/// path (`main` returning, the dev server's signal shutdown, a plugin's
/// `process.exit()`); a restart calls it before `exec`, which skips the hook.
pub fn flush_at_exit() {
    write_held();
}

extern "C" fn flush_on_exit() {
    write_held();
}

/// Registered with the first held entry, so a process that never produces
/// (a consume-only child) never runs a flush at exit.
fn arm_exit_flush() {
    static ARMED: std::sync::Once = std::sync::Once::new();
    ARMED.call_once(|| unsafe {
        libc::atexit(flush_on_exit);
    });
}

fn spawn_flush(name: &str, wait: Duration) -> bool {
    std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            std::thread::sleep(wait);
            write_held();
        })
        .is_ok()
}

/// Writes a snapshot of the held entries, then drops each one that was not
/// replaced meanwhile: until its file exists, an entry stays readable from
/// memory.
fn write_held() {
    // One flush at a time: two batches holding the same entry would write it
    // twice, the later one over whatever replaced or pruned the file between.
    static FLUSHING: Mutex<()> = Mutex::new(());
    let _flushing = FLUSHING.lock().unwrap_or_else(|e| e.into_inner());
    let batch: Vec<(PathBuf, Arc<[u8]>)> = {
        let mut held = held();
        held.armed = false;
        held.draining = false;
        held.entries
            .iter()
            .map(|(path, bytes)| (path.clone(), bytes.clone()))
            .collect()
    };
    for (path, bytes) in &batch {
        write_entry(path, bytes);
    }
    let mut held = held();
    for (path, bytes) in batch {
        if held
            .entries
            .get(&path)
            .is_some_and(|b| Arc::ptr_eq(b, &bytes))
        {
            held.entries.remove(&path);
            held.bytes -= bytes.len();
        }
    }
}

static PRODUCE: AtomicBool = AtomicBool::new(true);

/// For a one-shot child whose parent waits on its exit (an engine job, a
/// start script): its caches read entries but never create them. Producing
/// costs the cold run it happens on, which here is the parent's critical
/// path, and the long-lived engines that load the same modules write those
/// entries anyway.
pub fn consume_only() {
    PRODUCE.store(false, Ordering::Relaxed);
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
        Self::with_produce(dir, PRODUCE.load(Ordering::Relaxed))
    }

    fn with_produce(dir: PathBuf, produce: bool) -> Self {
        Self { dir, produce }
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
            return hash64(stable_specifier(specifier.as_str()).as_bytes());
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
        hash64(stable_specifier(base.as_str()).as_bytes())
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
        let path = self.entry_path(specifier, suffix);
        let held = held().entries.get(&path).cloned();
        let read;
        let bytes: &[u8] = match &held {
            Some(bytes) => bytes,
            None => {
                read = std::fs::read(&path).ok()?;
                &read
            }
        };
        let (head, data) = bytes.split_at_checked(8)?;
        if head != source_hash.to_le_bytes() {
            return None;
        }
        Some(data.to_vec())
    }

    fn put_entry(&self, specifier: &Url, suffix: &str, source_hash: u64, data: &[u8]) {
        if !self.produce {
            return;
        }
        let mut bytes = Vec::with_capacity(8 + data.len());
        bytes.extend_from_slice(&source_hash.to_le_bytes());
        bytes.extend_from_slice(data);
        let bytes: Arc<[u8]> = bytes.into();
        let mut held = held();
        held.bytes += bytes.len();
        if let Some(old) = held
            .entries
            .insert(self.entry_path(specifier, suffix), bytes)
        {
            held.bytes -= old.len();
        }
        if held.bytes > HELD_MAX_BYTES && !held.draining {
            held.draining = spawn_flush("oj-code-cache-drain", Duration::ZERO);
        }
        if held.armed {
            return;
        }
        // Like Vite's unref'd timer, it never keeps a finished process
        // waiting: the exit flush writes whatever it would have. A failed
        // spawn leaves it unarmed, so the next put tries again.
        held.armed = spawn_flush("oj-code-cache-flush", FLUSH_AFTER);
        drop(held);
        arm_exit_flush();
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

    /// Trims entries to three quarters of `max_bytes`, oldest write first,
    /// once the directory exceeds `max_bytes` (the slack keeps one boot from
    /// deleting a trickle). Nothing else ever evicts an entry, and one-shot
    /// modules (configs, renamed files) orphan theirs, so without a bound the
    /// directory only grows. Reads do not bump mtime: this evicts by write
    /// age, which is enough for a best-effort cache.
    pub fn prune_entries(&self, max_bytes: u64) {
        let Ok(read) = std::fs::read_dir(&self.dir) else {
            return;
        };
        let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = read
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".bin"))
            .filter_map(|e| {
                let meta = e.metadata().ok()?;
                let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
                meta.is_file().then(|| (modified, meta.len(), e.path()))
            })
            .collect();
        let mut total: u64 = entries.iter().map(|(_, len, _)| len).sum();
        if total <= max_bytes {
            return;
        }
        entries.sort();
        for (_, len, path) in entries {
            if total <= max_bytes / 4 * 3 {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                total -= len;
            }
        }
    }

    /// Removes sibling generation directories (other V8 versions next to this
    /// cache's dir): their bytecode can never load again, so a V8 bump
    /// otherwise leaves the whole previous generation as dead weight.
    pub fn sweep_stale_generations(&self) {
        let (Some(parent), Some(current)) = (self.dir.parent(), self.dir.file_name()) else {
            return;
        };
        let Ok(read) = std::fs::read_dir(parent) else {
            return;
        };
        for e in read.flatten() {
            if e.file_name() != current && e.path().is_dir() {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

/// Atomic publish: write-then-rename, so a concurrent reader never sees a
/// torn half-write. The tmp name carries a process-wide sequence beside the
/// pid: several caches can write the same entry, and a pid-only suffix let
/// two writers rename a torn file into place.
fn write_entry(path: &std::path::Path, bytes: &[u8]) {
    if path
        .parent()
        .is_some_and(|dir| std::fs::create_dir_all(dir).is_err())
    {
        return;
    }
    static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_extension(format!("tmp{}-{seq}", std::process::id()));
    if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// One-shot bundle names collapse to a stable key. Vite's loadConfigFromFile
/// writes `<config>.timestamp-<ms>-<hash>.mjs` and oj's fallback config
/// loader `oj-vite-config-<pid>-<rand>.tmp.mjs`, a fresh name per load:
/// keyed by raw URL, every dev-server boot wrote one permanently unreachable
/// entry. With the volatile segment dropped, the next boot overwrites in
/// place and an unchanged config hits (the embedded source hash guards
/// staleness, so a collision is only ever a miss).
fn stable_specifier(specifier: &str) -> std::borrow::Cow<'_, str> {
    if let Some(at) = specifier.rfind(".timestamp-") {
        let rest = &specifier[at + ".timestamp-".len()..];
        if let Some((stamp, tail)) = rest.split_once('.') {
            if let Some((ms, hash)) = stamp.split_once('-') {
                let digits = !ms.is_empty() && ms.bytes().all(|b| b.is_ascii_digit());
                let hex = !hash.is_empty() && hash.bytes().all(|b| b.is_ascii_hexdigit());
                if digits && hex {
                    return std::borrow::Cow::Owned(format!(
                        "{}.timestamp.{tail}",
                        &specifier[..at]
                    ));
                }
            }
        }
    }
    if let Some(at) = specifier.rfind("oj-vite-config-") {
        let rest = &specifier[at + "oj-vite-config-".len()..];
        if let Some(stamp) = rest.strip_suffix(".tmp.mjs") {
            if let Some((pid, rand)) = stamp.split_once('-') {
                let digits = !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit());
                let alnum = !rand.is_empty() && rand.bytes().all(|b| b.is_ascii_alphanumeric());
                if digits && alnum {
                    return std::borrow::Cow::Owned(format!(
                        "{}oj-vite-config.tmp.mjs",
                        &specifier[..at]
                    ));
                }
            }
        }
    }
    std::borrow::Cow::Borrowed(specifier)
}

/// Vite's MAX_TEMP_DIR_AGE_MS for its deps-cache temp dirs: 24 hours.
pub const STALE_TMP_MAX_AGE: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// The entry budget `prune_entries` trims to when `OJ_CODE_CACHE_MAX_BYTES`
/// is unset: 128 MiB, several times the largest working set observed while
/// staying negligible next to an app checkout.
pub const DEFAULT_MAX_BYTES: u64 = 128 << 20;

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

    /// A consume-only cache skips V8's serialization too, not only the write.
    fn produces(&self) -> bool {
        self.produce
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
        flush_at_exit();
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
        flush_at_exit();
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
        flush_at_exit();
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
        flush_at_exit();
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
        flush_at_exit();
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

    // Vite's loadConfigFromFile imports the bundled config under a fresh
    // `.timestamp-<ms>-<hash>.mjs` name on every load, and oj's fallback
    // loader under `oj-vite-config-<pid>-<rand>.tmp.mjs`: raw-URL keying
    // wrote one permanently unreachable entry per dev-server boot (observed
    // growing a cache by tens of GB). Each shape must collapse to one entry
    // that an unchanged config hits across boots.
    #[test]
    fn one_shot_config_bundles_share_one_entry_across_boots() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsCodeCache::new(dir.path().to_path_buf());
        for boot in 0..4u64 {
            let spec = url(&format!(
                "file:///app/vite.config.ts.timestamp-17345{boot}-ab{boot}f.mjs"
            ));
            cache.put(&spec, CodeCacheType::EsModule, 42, b"config-bytecode");
        }
        for boot in 0..4u64 {
            let spec = url(&format!(
                "file:///app/.oj-cache/v1/oj-vite-config-91{boot}-k{boot}z.tmp.mjs"
            ));
            cache.put(&spec, CodeCacheType::EsModule, 42, b"config-bytecode");
        }
        flush_at_exit();
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            2,
            "four boots per loader shape must reuse one entry each"
        );
        // The next boot's fresh name hits the unchanged config.
        let next = url("file:///app/vite.config.ts.timestamp-99999-ffff.mjs");
        assert_eq!(
            cache.get(&next, CodeCacheType::EsModule, 42).as_deref(),
            Some(b"config-bytecode".as_ref())
        );
        // An edited config (new source hash) misses, never serves stale.
        assert_eq!(cache.get(&next, CodeCacheType::EsModule, 43), None);
    }

    // Only the volatile loader shapes collapse: a module that merely contains
    // the words keeps its own entry.
    #[test]
    fn lookalike_module_names_keep_their_own_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsCodeCache::new(dir.path().to_path_buf());
        let lookalikes = [
            "file:///a/report.timestamp-draft.mjs", // no <ms>-<hash> stamp
            "file:///a/x.timestamp-12z4-abcd.mjs",  // ms not digits
            "file:///a/x.timestamp-1234-xyz.mjs",   // hash not hex
            "file:///a/oj-vite-config-notes.tmp.mjs", // no <pid>-<rand> stamp
            "file:///a/oj-vite-config-12-a_b.tmp.mjs", // rand not alphanumeric
            "file:///a/oj-vite-config-9-k.tmp.mjs.map", // wrong suffix
        ];
        for (i, s) in lookalikes.iter().enumerate() {
            cache.put(&url(s), CodeCacheType::EsModule, i as u64, b"x");
        }
        flush_at_exit();
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            lookalikes.len(),
            "lookalikes must not collide"
        );
    }

    // Nothing else evicts entries, so the boot prune must bound the
    // directory: oldest writes go first, down to three quarters of the
    // budget, and an under-budget directory is untouched.
    #[test]
    fn prune_trims_oldest_entries_to_the_budget() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsCodeCache::new(dir.path().to_path_buf());
        let payload = vec![0u8; 92]; // 100-byte entries with the hash head
        for i in 0..10u64 {
            let spec = url(&format!("file:///m{i}.js"));
            cache.put(&spec, CodeCacheType::EsModule, i, &payload);
            flush_at_exit();
            let age = std::time::SystemTime::now() - std::time::Duration::from_secs(60 * (10 - i));
            std::fs::File::options()
                .append(true)
                .open(cache.entry_path(&spec, "esm"))
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(age))
                .unwrap();
        }
        cache.prune_entries(10_000);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            10,
            "an under-budget directory stays whole"
        );
        cache.prune_entries(500);
        // 1000 bytes total, budget 500: trim to 375 = the 3 newest entries.
        for i in 0..10u64 {
            let spec = url(&format!("file:///m{i}.js"));
            let hit = cache.get(&spec, CodeCacheType::EsModule, i).is_some();
            assert_eq!(hit, i >= 7, "entry {i}: oldest writes must go first");
        }
    }

    // A V8 bump moves the cache to a new generation directory; the old one
    // can never load again and must not stay behind as dead weight.
    #[test]
    fn stale_generation_dirs_next_to_the_cache_are_removed() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("v8-14.2.1");
        let old = dir.path().join("v8-13.9.8");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("deadbeef-esm.bin"), b"old bytecode").unwrap();
        let cache = FsCodeCache::new(current.clone());
        cache.put(&url("file:///m.js"), CodeCacheType::EsModule, 1, b"new");
        flush_at_exit();
        cache.sweep_stale_generations();
        assert!(!old.exists(), "the dead generation is removed");
        assert!(
            cache
                .get(&url("file:///m.js"), CodeCacheType::EsModule, 1)
                .is_some(),
            "the current generation stays"
        );
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
        flush_at_exit();
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

    // Entries wait in memory, as in Node's compile cache; a cache dropped
    // with its engine must still land every one, by the exit flush at the
    // latest.
    #[test]
    fn dropping_the_cache_lands_every_queued_entry() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsCodeCache::new(dir.path().to_path_buf());
        for i in 0..200u64 {
            cache.put(
                &url(&format!("file:///m{i}.js")),
                CodeCacheType::EsModule,
                i,
                b"x",
            );
        }
        drop(cache);
        flush_at_exit();
        let reopened = FsCodeCache::new(dir.path().to_path_buf());
        for i in 0..200u64 {
            assert!(
                reopened
                    .get(
                        &url(&format!("file:///m{i}.js")),
                        CodeCacheType::EsModule,
                        i
                    )
                    .is_some(),
                "entry {i} never reached disk"
            );
        }
    }

    // Vite flushes the compile cache 10s after start; here every flush
    // re-arms, so a long-lived engine also persists what it compiles late (an
    // SSR route on its first request) without ever exiting cleanly.
    #[test]
    fn timed_flushes_keep_landing_later_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsCodeCache::new(dir.path().to_path_buf());
        let lands = |spec: &Url| {
            let path = cache.entry_path(spec, "esm");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            while !path.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "never flushed: {spec}"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        };
        let boot = url("file:///boot.js");
        cache.put(&boot, CodeCacheType::EsModule, 1, b"x");
        lands(&boot);
        let late = url("file:///late.js");
        cache.put(&late, CodeCacheType::EsModule, 1, b"y");
        lands(&late);
    }

    // A finished one-shot process (`oj build`, an engine job) returns from
    // main with its engines' caches still alive: the exit flush lands them.
    #[test]
    fn the_exit_flush_writes_a_live_caches_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cache = FsCodeCache::new(dir.path().to_path_buf());
        let spec = url("file:///exit.js");
        cache.put(&spec, CodeCacheType::EsModule, 1, b"x");
        flush_at_exit();
        assert!(cache.get(&spec, CodeCacheType::EsModule, 1).is_some());
    }

    // Held entries belong to the process, not to the cache that made them:
    // a second engine compiling the same module before the flush still hits.
    #[test]
    fn a_held_entry_is_a_hit_for_another_cache() {
        let dir = tempfile::tempdir().unwrap();
        let spec = url("file:///held.js");
        FsCodeCache::new(dir.path().to_path_buf()).put(&spec, CodeCacheType::EsModule, 1, b"x");
        let other = FsCodeCache::new(dir.path().to_path_buf());
        assert_eq!(
            other.get(&spec, CodeCacheType::EsModule, 1).as_deref(),
            Some(b"x".as_slice())
        );
        assert!(
            other.get(&spec, CodeCacheType::EsModule, 2).is_none(),
            "the source hash still guards"
        );
    }

    // A one-shot child's parent waits on its exit: its cache reads what the
    // long-lived engines wrote and never writes, even when dropped or flushed.
    #[test]
    fn a_consume_only_cache_reads_entries_and_never_writes() {
        let dir = tempfile::tempdir().unwrap();
        let spec = url("file:///shared.js");
        let writer = FsCodeCache::new(dir.path().to_path_buf());
        writer.put(&spec, CodeCacheType::EsModule, 1, b"bytecode");
        drop(writer);
        flush_at_exit();
        let reader = FsCodeCache::with_produce(dir.path().to_path_buf(), false);
        assert!(!reader.produces());
        assert_eq!(
            reader.get(&spec, CodeCacheType::EsModule, 1).as_deref(),
            Some(b"bytecode".as_slice())
        );
        let fresh = url("file:///fresh.js");
        reader.put(&fresh, CodeCacheType::EsModule, 1, b"x");
        flush_at_exit();
        flush_at_exit();
        drop(reader);
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            1,
            "nothing new on disk"
        );
    }
}
