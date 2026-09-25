// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::integrity::{self, ExpectedFile, VerifyMode};

pub const START_BUNDLE_FORMAT: u32 = 3;
pub const DEFAULT_PRUNE_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;

const ARTIFACTS: [&str; 2] = ["client-entry.modules", "manifest.ts"];
const CLOSURE_FILE: &str = "closure.json";
const MEMO_FILE: &str = "memo.json";
const MANIFEST_FILE: &str = "manifest.json";
const CURRENT_FILE: &str = "current";
const LEGACY_POINTER_FILE: &str = "latest";
const TOUCH_FILE: &str = "last-used";
const BLOBS_DIR: &str = "blobs";
const CHUNKS_DIR: &str = "client-chunks";
const CHUNK_INDEX_FILE: &str = "client-chunks.json";
const CSS_URLS_FILE: &str = "css-urls.json";

const MEMO_FRESHNESS_SLACK_NS: u64 = 2_000_000_000;

pub struct StartBundleStore {
    dir: PathBuf,
    salt: String,
    verify: VerifyMode,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PinnedBundle {
    pub entry: String,
    chunks: HashMap<String, PinnedChunk>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PinnedChunk {
    pub path: PathBuf,
    pub size: u64,
    pub hash: Option<String>,
}

impl PinnedBundle {
    pub fn chunk(&self, name: &str) -> Option<&PinnedChunk> {
        self.chunks
            .get(name)
            .or_else(|| (name == "client-entry.js").then(|| self.chunks.get(&self.entry))?)
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn from_build_dir(start_dir: &Path) -> Option<Self> {
        let index = read_chunk_index(start_dir)?;
        let chunk_dir = start_dir.join(CHUNKS_DIR);
        let mut chunks = HashMap::with_capacity(index.files.len());
        for f in index.files {
            let path = chunk_dir.join(&f.name);
            let size = fs::metadata(&path).ok()?.len();
            chunks.insert(f.name, PinnedChunk { path, size, hash: None });
        }
        Some(Self { entry: index.entry, chunks })
    }
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GenerationManifest {
    format: u32,
    entry: String,
    css_urls: Vec<String>,
    files: BTreeMap<String, ManifestFile>,
}

#[derive(Serialize, Deserialize)]
struct ManifestFile {
    hash: String,
    size: u64,
}

#[derive(Deserialize)]
struct ChunkIndex {
    entry: String,
    files: Vec<ChunkIndexFile>,
}

#[derive(Deserialize)]
struct ChunkIndexFile {
    name: String,
    #[allow(dead_code)]
    size: u64,
}

#[derive(Debug, PartialEq)]
pub struct RestoreStats {
    pub key: String,
    pub files: usize,
    pub chunks: usize,
    pub rehashed: usize,
    pub elapsed_ms: u128,
}

#[derive(Debug, PartialEq)]
pub enum Miss {
    NoPreviousBuild,
    ClosureUnreadable,
    ClosureFileUnreadable(PathBuf),
    NoEntryForKey(String),
    EntryCorrupt(String),
    ChunkCorrupt {
        key: String,
        name: String,
        detail: String,
    },
    ArtifactWriteFailed(String),
}

impl std::fmt::Display for Miss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn short(key: &str) -> &str {
            key.get(..8).unwrap_or(key)
        }
        match self {
            Miss::NoPreviousBuild => write!(f, "no previous build"),
            Miss::ClosureUnreadable => write!(f, "previous closure unreadable"),
            Miss::ClosureFileUnreadable(p) => {
                write!(f, "closure file unreadable: {}", p.display())
            }
            Miss::NoEntryForKey(k) => write!(f, "no cached bundle for key {}…", short(k)),
            Miss::EntryCorrupt(k) => write!(f, "cached entry {}… corrupt, removed", short(k)),
            Miss::ChunkCorrupt { key, name, detail } => write!(
                f,
                "chunk {name} of entry {}… failed verification ({detail}); entry removed",
                short(key)
            ),
            Miss::ArtifactWriteFailed(name) => {
                write!(f, "could not write restored artifact {name}")
            }
        }
    }
}

#[derive(Serialize, Deserialize)]
struct Memo {
    written_at_ns: u64,
    files: HashMap<String, MemoFile>,
}

#[derive(Serialize, Deserialize)]
struct MemoFile {
    size: u64,
    mtime_ns: u64,
    digest: String,
}

impl StartBundleStore {
    pub fn new(root: &Path, tool_version: &str, verify: VerifyMode) -> Self {
        Self::for_mode(root, tool_version, verify, "development")
    }

    /// A store keyed by the dev mode too: `.env.<mode>` feeds the client bundle's
    /// import.meta.env, so `oj dev --mode staging` must not restore a bundle
    /// built for `development` (or the other way round).
    pub fn for_mode(root: &Path, tool_version: &str, verify: VerifyMode, mode: &str) -> Self {
        Self {
            dir: crate::cache_root(root).join("start-bundle"),
            salt: format!(
                "{tool_version}:{START_BUNDLE_FORMAT}:start-bundle:{mode}:{}",
                epoch(root, mode, vendored_rolldown().as_deref())
            ),
            verify,
        }
    }

    pub fn restore(&self, start_dir: &Path) -> Result<(RestoreStats, PinnedBundle), Miss> {
        let started = Instant::now();
        let current = fs::read_to_string(self.dir.join(CURRENT_FILE))
            .map_err(|_| Miss::NoPreviousBuild)?
            .trim()
            .to_string();
        let current_dir = self.dir.join(&current);
        let Some(files) = read_closure(&current_dir.join(CLOSURE_FILE)) else {
            let _ = fs::remove_dir_all(&current_dir);
            return Err(Miss::ClosureUnreadable);
        };
        let memo = read_memo(&current_dir);
        let (digests, rehashed) = verified_digests(&files, memo.as_ref());
        let key = self.key_from(&files, &digests)?;
        let entry = self.dir.join(&key);
        if !entry.is_dir() {
            return Err(Miss::NoEntryForKey(key));
        }
        let Some(manifest) = read_manifest(&entry) else {
            let _ = fs::remove_dir_all(&entry);
            return Err(Miss::EntryCorrupt(key));
        };
        if let Err((name, detail)) = self.verify_members(&manifest) {
            let _ = fs::remove_dir_all(&entry);
            return Err(Miss::ChunkCorrupt { key, name, detail });
        }
        for name in ARTIFACTS {
            let Ok(bytes) = fs::read(entry.join(name)) else {
                let _ = fs::remove_dir_all(&entry);
                return Err(Miss::EntryCorrupt(key));
            };
            if integrity::atomic_write(&start_dir.join(name), &bytes).is_err() {
                return Err(Miss::ArtifactWriteFailed(name.to_string()));
            }
        }
        let css = serde_json::to_vec(&manifest.css_urls).unwrap_or_else(|_| b"[]".to_vec());
        if integrity::atomic_write(&start_dir.join(CSS_URLS_FILE), &css).is_err() {
            return Err(Miss::ArtifactWriteFailed(CSS_URLS_FILE.to_string()));
        }
        if rehashed > 0 || !entry.join(MEMO_FILE).is_file() {
            write_memo(&entry, &build_memo(&files, &digests));
        }
        touch(&entry);
        if key != current {
            self.write_current(&key);
        }
        let stats = RestoreStats {
            key,
            files: files.len(),
            chunks: manifest.files.len(),
            rehashed,
            elapsed_ms: started.elapsed().as_millis(),
        };
        Ok((stats, self.pin(&manifest)))
    }

    pub fn persist(&self, start_dir: &Path) -> Option<(String, PinnedBundle)> {
        let files = read_closure(&start_dir.join(CLOSURE_FILE))?;
        let digests = par_map(&files, |p| hash_file(p));
        let key = self.key_from(&files, &digests).ok()?;
        let index = read_chunk_index(start_dir)?;
        let css_urls = read_css_urls(start_dir);
        let chunk_dir = start_dir.join(CHUNKS_DIR);
        let chunk_paths: Vec<PathBuf> = index.files.iter().map(|f| chunk_dir.join(&f.name)).collect();
        let chunk_hashes = par_map(&chunk_paths, |p| hash_file(p));
        let blobs = self.dir.join(BLOBS_DIR);
        fs::create_dir_all(&blobs).ok()?;
        let mut manifest_files = BTreeMap::new();
        for (f, (path, hash)) in index.files.iter().zip(chunk_paths.iter().zip(&chunk_hashes)) {
            let hex = hash.as_ref()?.to_hex().to_string();
            let size = fs::metadata(path).ok()?.len();
            let blob = blobs.join(&hex);
            if !blob.is_file() {
                let tmp = blobs.join(format!(".tmp-{}-{}", hex.get(..16)?, std::process::id()));
                if fs::copy(path, &tmp).is_err() {
                    let _ = fs::remove_file(&tmp);
                    return None;
                }
                if fs::rename(&tmp, &blob).is_err() {
                    let _ = fs::remove_file(&tmp);
                    if !blob.is_file() {
                        return None;
                    }
                }
            }
            manifest_files.insert(f.name.clone(), ManifestFile { hash: hex, size });
        }
        let manifest = GenerationManifest {
            format: START_BUNDLE_FORMAT,
            entry: index.entry,
            css_urls,
            files: manifest_files,
        };
        let entry = self.dir.join(&key);
        if !entry.is_dir() {
            let tmp = self
                .dir
                .join(format!(".tmp-{}-{}", key.get(..16)?, std::process::id()));
            let _ = fs::remove_dir_all(&tmp);
            fs::create_dir_all(&tmp).ok()?;
            for name in ARTIFACTS.into_iter().chain([CLOSURE_FILE]) {
                if fs::copy(start_dir.join(name), tmp.join(name)).is_err() {
                    let _ = fs::remove_dir_all(&tmp);
                    return None;
                }
            }
            let bytes = serde_json::to_vec(&manifest).ok()?;
            if fs::write(tmp.join(MANIFEST_FILE), bytes).is_err() {
                let _ = fs::remove_dir_all(&tmp);
                return None;
            }
            if fs::rename(&tmp, &entry).is_err() {
                let _ = fs::remove_dir_all(&tmp);
                if !entry.is_dir() {
                    return None;
                }
            }
        }
        write_memo(&entry, &build_memo(&files, &digests));
        touch(&entry);
        self.write_current(&key);
        Some((key, self.pin(&manifest)))
    }

    pub fn prune(&self, budget_bytes: u64) {
        let _ = fs::remove_file(self.dir.join(LEGACY_POINTER_FILE));
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let keep = fs::read_to_string(self.dir.join(CURRENT_FILE))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        struct Gen {
            path: PathBuf,
            stamp: SystemTime,
            own_size: u64,
            refs: Vec<String>,
            is_current: bool,
        }
        let mut gens = Vec::new();
        for e in entries.flatten() {
            let path = e.path();
            let name = e.file_name().to_string_lossy().into_owned();
            if name == BLOBS_DIR || !path.is_dir() {
                continue;
            }
            if name.starts_with(".tmp-") {
                let _ = fs::remove_dir_all(&path);
                continue;
            }
            let refs = read_manifest(&path)
                .map(|m| m.files.into_values().map(|f| f.hash).collect())
                .unwrap_or_default();
            let stamp = fs::metadata(path.join(TOUCH_FILE))
                .or_else(|_| fs::metadata(&path))
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            gens.push(Gen {
                own_size: entry_size(&path),
                is_current: name == keep,
                path,
                stamp,
                refs,
            });
        }
        let blobs_dir = self.dir.join(BLOBS_DIR);
        let mut blob_sizes: HashMap<String, u64> = HashMap::new();
        if let Ok(entries) = fs::read_dir(&blobs_dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.starts_with(".tmp-") {
                    let _ = fs::remove_file(e.path());
                    continue;
                }
                if let Ok(meta) = e.metadata() {
                    blob_sizes.insert(name, meta.len());
                }
            }
        }
        let mut refcount: HashMap<String, usize> = HashMap::new();
        for g in &gens {
            for h in &g.refs {
                *refcount.entry(h.clone()).or_default() += 1;
            }
        }
        let mut total: u64 = gens.iter().map(|g| g.own_size).sum();
        for (hash, count) in &refcount {
            if *count > 0 {
                total += blob_sizes.get(hash).copied().unwrap_or(0);
            }
        }
        gens.sort_by_key(|g| g.stamp);
        for g in &gens {
            if total <= budget_bytes {
                break;
            }
            if g.is_current {
                continue;
            }
            if fs::remove_dir_all(&g.path).is_err() {
                continue;
            }
            total = total.saturating_sub(g.own_size);
            for h in &g.refs {
                if let Some(c) = refcount.get_mut(h.as_str()) {
                    *c -= 1;
                    if *c == 0 {
                        total = total.saturating_sub(blob_sizes.get(h).copied().unwrap_or(0));
                    }
                }
            }
        }
        let mut live: HashSet<String> = refcount
            .into_iter()
            .filter(|&(_, c)| c > 0)
            .map(|(h, _)| h)
            .collect();
        if let Ok(now_current) = fs::read_to_string(self.dir.join(CURRENT_FILE)) {
            if let Some(m) = read_manifest(&self.dir.join(now_current.trim())) {
                live.extend(m.files.into_values().map(|f| f.hash));
            }
        }
        for hash in blob_sizes.keys() {
            if !live.contains(hash) {
                let _ = fs::remove_file(blobs_dir.join(hash));
            }
        }
    }

    fn pin(&self, manifest: &GenerationManifest) -> PinnedBundle {
        let blobs = self.dir.join(BLOBS_DIR);
        let chunks = manifest
            .files
            .iter()
            .map(|(name, f)| {
                (
                    name.clone(),
                    PinnedChunk {
                        path: blobs.join(&f.hash),
                        size: f.size,
                        hash: Some(f.hash.clone()),
                    },
                )
            })
            .collect();
        PinnedBundle {
            entry: manifest.entry.clone(),
            chunks,
        }
    }

    fn verify_members(&self, manifest: &GenerationManifest) -> Result<(), (String, String)> {
        let blobs = self.dir.join(BLOBS_DIR);
        let items: Vec<(String, PathBuf, ExpectedFile)> = manifest
            .files
            .iter()
            .map(|(name, f)| {
                (
                    name.clone(),
                    blobs.join(&f.hash),
                    ExpectedFile {
                        size: f.size,
                        hash: f.hash.clone(),
                    },
                )
            })
            .collect();
        let mode = self.verify;
        let results = par_map(&items, |(name, path, expected)| {
            Some(match integrity::verify_file(path, expected, mode) {
                Ok(()) => Ok(()),
                Err(e) => {
                    if !matches!(e, integrity::VerifyError::Io(_)) {
                        let _ = fs::remove_file(path);
                    }
                    Err((name.clone(), e.to_string()))
                }
            })
        });
        for r in results.into_iter().flatten() {
            r?;
        }
        Ok(())
    }

    fn key_from(
        &self,
        files: &[PathBuf],
        digests: &[Option<blake3::Hash>],
    ) -> Result<String, Miss> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.salt.as_bytes());
        for (path, digest) in files.iter().zip(digests) {
            let Some(digest) = digest else {
                return Err(Miss::ClosureFileUnreadable(path.clone()));
            };
            hasher.update(&[0]);
            hasher.update(path.as_os_str().as_encoded_bytes());
            hasher.update(&[0]);
            hasher.update(digest.as_bytes());
        }
        Ok(hasher.finalize().to_hex().to_string())
    }

    fn write_current(&self, key: &str) {
        let _ = integrity::atomic_write(&self.dir.join(CURRENT_FILE), key.as_bytes());
    }
}

/// The rolldown vendored next to the binary for oj's own Start bundles. The
/// runtime variable wins over the nix build's compile-time store path, and a
/// set-but-empty variable is an explicit opt-out (no vendor, the app's copy
/// serves). Used both to configure the bundle scripts and as a cache-key
/// input: a bundle built by one rolldown must not restore as another's.
pub fn vendored_rolldown() -> Option<String> {
    match std::env::var("OJ_VENDORED_ROLLDOWN") {
        Ok(v) => (!v.is_empty()).then_some(v),
        Err(std::env::VarError::NotUnicode(_)) => None,
        Err(std::env::VarError::NotPresent) => option_env!("OJ_VENDORED_ROLLDOWN")
            .map(str::to_string)
            .filter(|v| !v.is_empty()),
    }
}

fn epoch(root: &Path, mode: &str, vendored_rolldown: Option<&str>) -> String {
    let mut hasher = blake3::Hasher::new();
    let mode_env = [format!(".env.{mode}"), format!(".env.{mode}.local")];
    for name in [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lockb",
        "package.json",
        "vite.config.ts",
        "vite.config.js",
        "vite.config.mjs",
        "vite.config.mts",
        "vite.config.cjs",
        "vite.config.cts",
        "oj.config.ts",
        "oj.config.js",
        "oj.config.mjs",
        ".env",
        ".env.local",
        mode_env[0].as_str(),
        mode_env[1].as_str(),
    ] {
        if let Ok(bytes) = fs::read(root.join(name)) {
            hasher.update(name.as_bytes());
            hasher.update(&[0]);
            hasher.update(&bytes);
        }
    }
    // NODE_ENV too: the shell's value decides DEV/PROD and the React build the
    // client bundle compiles against, as it does under Vite.
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| k.starts_with("VITE_") || k == "TSS_SERVER_FN_BASE" || k == "NODE_ENV")
        .collect();
    env.sort();
    for (k, v) in env {
        hasher.update(b"\0e");
        hasher.update(k.as_bytes());
        hasher.update(&[0]);
        hasher.update(v.as_bytes());
    }
    // The client bundle is built by whichever rolldown the vendor preference
    // picks (resolve-pkg.mjs), so a vendor change must not restore a bundle
    // the other rolldown produced.
    if let Some(vendored) = vendored_rolldown {
        hasher.update(b"\0vendored-rolldown\0");
        hasher.update(vendored.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

fn read_closure(path: &Path) -> Option<Vec<PathBuf>> {
    let bytes = fs::read(path).ok()?;
    let files: Vec<String> = serde_json::from_slice(&bytes).ok()?;
    if files.is_empty() {
        return None;
    }
    let mut files: Vec<PathBuf> = files.into_iter().map(PathBuf::from).collect();
    files.sort();
    files.dedup();
    Some(files)
}

fn read_manifest(entry: &Path) -> Option<GenerationManifest> {
    let bytes = fs::read(entry.join(MANIFEST_FILE)).ok()?;
    let manifest: GenerationManifest = serde_json::from_slice(&bytes).ok()?;
    if manifest.format != START_BUNDLE_FORMAT || manifest.files.is_empty() {
        return None;
    }
    Some(manifest)
}

fn read_chunk_index(start_dir: &Path) -> Option<ChunkIndex> {
    let bytes = fs::read(start_dir.join(CHUNK_INDEX_FILE)).ok()?;
    let index: ChunkIndex = serde_json::from_slice(&bytes).ok()?;
    if index.files.is_empty() {
        return None;
    }
    Some(index)
}

fn read_css_urls(start_dir: &Path) -> Vec<String> {
    fs::read(start_dir.join(CSS_URLS_FILE))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn read_memo(entry: &Path) -> Option<Memo> {
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

fn write_memo(entry: &Path, memo: &Memo) {
    let Ok(bytes) = serde_json::to_vec(memo) else {
        return;
    };
    let _ = integrity::atomic_write(&entry.join(MEMO_FILE), &bytes);
}

fn build_memo(files: &[PathBuf], digests: &[Option<blake3::Hash>]) -> Memo {
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

fn verified_digests(files: &[PathBuf], memo: Option<&Memo>) -> (Vec<Option<blake3::Hash>>, usize) {
    let rehashed = AtomicUsize::new(0);
    let digests = par_map(files, |p| {
        if let Some(known) = memo.and_then(|m| p.to_str().and_then(|s| m.files.get(s))) {
            if let Ok(meta) = fs::metadata(p) {
                if meta.len() == known.size && mtime_ns(&meta) == Some(known.mtime_ns) {
                    if let Ok(digest) = blake3::Hash::from_hex(&known.digest) {
                        return Some(digest);
                    }
                }
            }
        }
        rehashed.fetch_add(1, Ordering::Relaxed);
        hash_file(p)
    });
    (digests, rehashed.into_inner())
}

fn hash_file(p: &Path) -> Option<blake3::Hash> {
    fs::read(p).ok().map(|bytes| blake3::hash(&bytes))
}

fn mtime_ns(meta: &fs::Metadata) -> Option<u64> {
    let t = meta.modified().ok()?;
    u64::try_from(t.duration_since(UNIX_EPOCH).ok()?.as_nanos()).ok()
}

fn now_ns() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .and_then(|d| u64::try_from(d.as_nanos()).ok())
        .unwrap_or(0)
}

fn par_map<I: Sync, T: Send>(items: &[I], f: impl Fn(&I) -> Option<T> + Sync) -> Vec<Option<T>> {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(items.len().max(1));
    if threads <= 1 {
        return items.iter().map(|p| f(p)).collect();
    }
    let chunk = items.len().div_ceil(threads);
    let mut out: Vec<Option<T>> = Vec::new();
    out.resize_with(items.len(), || None);
    std::thread::scope(|s| {
        for (part, slots) in items.chunks(chunk).zip(out.chunks_mut(chunk)) {
            let f = &f;
            s.spawn(move || {
                for (p, slot) in part.iter().zip(slots) {
                    *slot = f(p);
                }
            });
        }
    });
    out
}

fn touch(entry: &Path) {
    let _ = fs::write(entry.join(TOUCH_FILE), b"");
}

fn entry_size(dir: &Path) -> u64 {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .filter(|m| m.is_file())
        .map(|m| m.len())
        .sum()
}

#[cfg(test)]
mod tests {
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
            fs::write(self.start.join(CSS_URLS_FILE), br#"["/@oj-start/fs/a.css"]"#).unwrap();
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
            crate::cache_root(&self.root).join("start-bundle").join(BLOBS_DIR)
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
        assert!(
            chunk.path.starts_with(fx.blob_dir()),
            "{:?}",
            chunk.path
        );
        assert_eq!(chunk.hash.as_deref(), Some(blake3::hash(b"bundle-v1").to_hex().as_str()));
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
            crate::cache_root(&fx.root).join("start-bundle").join(CURRENT_FILE),
        )
        .unwrap();
        assert_eq!(current.trim(), key, "pointer swaps to the pinned generation");
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
    fn vendored_rolldown_is_an_epoch_input() {
        let fx = Fixture::new("vendor-epoch");
        let vendored = epoch(&fx.root, "development", Some("/nix/store/aaa-vendor"));
        assert_ne!(vendored, epoch(&fx.root, "development", None));
        assert_ne!(
            vendored,
            epoch(&fx.root, "development", Some("/nix/store/bbb-vendor")),
            "a different vendor is a different epoch"
        );
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
        assert!(!fx.entry_dir(&key).exists(), "corrupt entry must be removed");
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
        assert!(fx.store().restore(&fx.start).is_ok(), "survivor still restores");
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
}
