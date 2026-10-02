// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The Start client-bundle store: generations keyed by the module closure,
//! chunk bytes deduplicated in a content-addressed blob store, a `current`
//! pointer, and pinning so a served bundle survives later eviction.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::integrity::{self, ExpectedFile, VerifyMode};

mod memo;
mod persist;
mod prune;
mod vendor;

use memo::{build_memo, read_memo, verified_digests, write_memo};
use vendor::epoch;
pub use vendor::{vendored_rolldown, VendoredRolldown};

pub const START_BUNDLE_FORMAT: u32 = 3;
pub const DEFAULT_PRUNE_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;
/// Generations kept per store, enforced on every persist. Vite's dep cache
/// keeps exactly one generation; this store keeps `current` plus a small
/// recency window so reverting to a just-seen closure restores warm, while a
/// long editing session cannot accumulate one generation per save.
pub const KEEP_GENERATIONS: usize = 8;

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

pub struct StartBundleStore {
    dir: PathBuf,
    salt: String,
    verify: VerifyMode,
    /// The newest detached eviction thread. Each new sweep first joins its
    /// predecessor (inside the thread, so the save path never waits), which
    /// serializes sweeps; joining this handle therefore joins them all.
    sweeper: std::sync::Mutex<Option<std::thread::JoinHandle<()>>>,
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

    /// Whether `name` is the entry chunk (also served as `client-entry.js`).
    pub fn is_entry(&self, name: &str) -> bool {
        name == self.entry || name == "client-entry.js"
    }

    /// Whether any chunk of this bundle has content hash `hash`.
    pub fn has_hash(&self, hash: &str) -> bool {
        self.chunks
            .values()
            .any(|c| c.hash.as_deref() == Some(hash))
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
            chunks.insert(
                f.name,
                PinnedChunk {
                    path,
                    size,
                    hash: None,
                },
            );
        }
        Some(Self {
            entry: index.entry,
            chunks,
        })
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
                epoch(root, mode, vendored_rolldown().epoch_input().as_deref())
            ),
            verify,
            sweeper: std::sync::Mutex::new(None),
        }
    }

    pub fn restore(&self, start_dir: &Path) -> Result<(RestoreStats, PinnedBundle), Miss> {
        let started = Instant::now();
        let current = read_current(&self.dir).ok_or(Miss::NoPreviousBuild)?;
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
        restore_artifacts(&entry, start_dir, &manifest, &key)?;
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

/// Copies the generation's artifacts and css urls into the build dir. A
/// missing artifact means the entry is corrupt and it is removed.
fn restore_artifacts(
    entry: &Path,
    start_dir: &Path,
    manifest: &GenerationManifest,
    key: &str,
) -> Result<(), Miss> {
    for name in ARTIFACTS {
        let Ok(bytes) = fs::read(entry.join(name)) else {
            let _ = fs::remove_dir_all(entry);
            return Err(Miss::EntryCorrupt(key.to_string()));
        };
        if integrity::atomic_write(&start_dir.join(name), &bytes).is_err() {
            return Err(Miss::ArtifactWriteFailed(name.to_string()));
        }
    }
    let css = serde_json::to_vec(&manifest.css_urls).unwrap_or_else(|_| b"[]".to_vec());
    if integrity::atomic_write(&start_dir.join(CSS_URLS_FILE), &css).is_err() {
        return Err(Miss::ArtifactWriteFailed(CSS_URLS_FILE.to_string()));
    }
    Ok(())
}

/// The trimmed generation key `current` points at.
fn read_current(dir: &Path) -> Option<String> {
    fs::read_to_string(dir.join(CURRENT_FILE))
        .ok()
        .map(|s| s.trim().to_string())
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
        return items.iter().map(&f).collect();
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
mod tests;
