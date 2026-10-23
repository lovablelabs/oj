// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The Start client-bundle store: generations keyed by the module closure,
//! chunk bytes deduplicated in a content-addressed blob store, a `current`
//! pointer, and pinning so a served bundle survives later eviction.
//!
//! Generations and blobs live in the shared oj store when there is one, so
//! worktree B restores the bundle worktree A built: closure keys and stored
//! closures are root-relative (`ws:<rel>`), and so are the stored globs.
//! `current` and the memo stay per-root; each root mirrors its `current`
//! into `pins/`, which every store's eviction honors.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::integrity::{self, ExpectedFile, VerifyMode};

mod epoch;
mod memo;
mod persist;
mod prune;

use epoch::epoch;
use memo::{build_memo, read_memo, verified_digests, write_memo};

pub const START_BUNDLE_FORMAT: u32 = 6;
pub const DEFAULT_PRUNE_BUDGET_BYTES: u64 = 1024 * 1024 * 1024;
/// Generations kept per store, enforced on every persist. Vite's dep cache
/// keeps exactly one generation; this store keeps `current` plus a small
/// recency window so reverting to a just-seen closure restores warm, while a
/// long editing session cannot accumulate one generation per save.
pub const KEEP_GENERATIONS: usize = 8;
/// The shared store serves every worktree and app on the machine, so its
/// generation window is wider than a single app's.
pub const SHARED_KEEP_GENERATIONS: usize = 32;
/// Cold-worktree probe: with no per-root `current`, try this many of the most
/// recently used shared generations' closures as key predictions.
const PROBE_CANDIDATES: usize = 4;

/// glob-scopes.json is the generation's `import.meta.glob` calls, which a
/// restore re-expands (see [`StartBundleStore::restore`]).
const ARTIFACTS: [&str; 3] = ["client-entry.modules", "manifest.ts", GLOB_SCOPES_FILE];
const GLOB_SCOPES_FILE: &str = "glob-scopes.json";
const CLOSURE_FILE: &str = "closure.json";
const MEMO_FILE: &str = "memo.json";
const MANIFEST_FILE: &str = "manifest.json";
const CURRENT_FILE: &str = "current";
/// Per-root pin files inside the shared dir: `pins/<hash(root)>` names the
/// generation that root currently serves, so one worktree's eviction can
/// never delete blobs another running server reads live.
const PINS_DIR: &str = "pins";
/// A pin whose file has not been refreshed for this long belongs to a
/// worktree that is gone (deleted checkout, abandoned sandbox): eviction may
/// ignore it, and the next prune removes the pin file itself.
const PIN_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// What an in-root glob pattern's root prefix is stored as.
const GLOB_ROOT_TOKEN: &str = "\0ws/";
const LEGACY_POINTER_FILE: &str = "latest";
const TOUCH_FILE: &str = "last-used";
const BLOBS_DIR: &str = "blobs";
const CHUNKS_DIR: &str = "client-chunks";
const CHUNK_INDEX_FILE: &str = "client-chunks.json";
const CSS_URLS_FILE: &str = "css-urls.json";

pub struct StartBundleStore {
    /// The canonical root: closure ids relativize against it.
    root: PathBuf,
    /// The root as the caller spells it, which the bundle's glob patterns use.
    root_spelled: PathBuf,
    /// Per-root pointer state: `current` and `memo.json`. Never shared.
    local: PathBuf,
    /// This root's pin file name in the shared dir (hash of the canonical root).
    pin_id: String,
    keep_generations: usize,
    /// Generation entries and blobs (the shared store's, when there is one).
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

    /// No chunks: what start dev serves while the client bundle has never
    /// built (its boot bundle failed), until an edit bundles it.
    pub fn empty() -> Self {
        Self {
            entry: String::new(),
            chunks: HashMap::new(),
        }
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
    GlobGrew(PathBuf),
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
            Miss::GlobGrew(p) => {
                write!(f, "{} is new to a bundled glob", p.display())
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
        let local = crate::cache_root(root).join("start-bundle");
        let (dir, keep) = match crate::store::store_root() {
            Some(store) => (store.join("start-bundle"), SHARED_KEEP_GENERATIONS),
            None => (local.clone(), KEEP_GENERATIONS),
        };
        Self::at_dirs(root, dir, local, keep, tool_version, verify, mode)
    }

    /// The explicit layout `for_mode` resolves into: generation entries and
    /// blobs at `dir` (shareable), `current` and the memo at `local`
    /// (per-root). Tests and embedders that must not touch the user's shared
    /// store build through this.
    pub fn at_dirs(
        root: &Path,
        dir: PathBuf,
        local: PathBuf,
        keep_generations: usize,
        tool_version: &str,
        verify: VerifyMode,
        mode: &str,
    ) -> Self {
        // Canonical, so rolldown's realpath ids and macOS /var symlink
        // spellings both relativize.
        let root_canon = fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
        let pin_id =
            blake3::hash(root_canon.as_os_str().as_encoded_bytes()).to_hex()[..16].to_string();
        Self {
            root: root_canon,
            root_spelled: root.to_path_buf(),
            local,
            pin_id,
            keep_generations,
            dir,
            salt: format!(
                "{tool_version}:{START_BUNDLE_FORMAT}:start-bundle:{mode}:{}",
                epoch(root, mode)
            ),
            verify,
            sweeper: std::sync::Mutex::new(None),
        }
    }

    /// Folds `extra` into the key salt: bundle inputs only the caller knows,
    /// such as the config file the app was started with and the files it
    /// imported. Hash them worktree-independently, or nothing is shared.
    pub fn with_extra_salt(mut self, extra: &[u8]) -> Self {
        self.salt.push(':');
        self.salt.push_str(&blake3::hash(extra).to_hex());
        self
    }

    /// The generation `current` names, when its closure still hashes to a
    /// stored key. The key covers every file the bundle read, but a file an
    /// `import.meta.glob` would expand to now and the bundle never read (one
    /// added while no server watched) is in no closure: `glob_members` maps
    /// the generation's glob-scopes.json (rehomed onto this root) to the
    /// files its calls expand to today, and any one outside the closure is a
    /// miss, as Vite expands every glob anew on each start.
    ///
    /// A cold worktree has no `current`, so the most recently used shared
    /// generations stand in as predictions: their closures are root-relative,
    /// so this worktree's own files decide whether one holds.
    pub fn restore(
        &self,
        start_dir: &Path,
        glob_members: impl Fn(&str) -> Vec<PathBuf>,
    ) -> Result<(RestoreStats, PinnedBundle), Miss> {
        let started = Instant::now();
        let predictions: Vec<(String, bool)> = match read_current(&self.local) {
            Some(current) => vec![(current, true)],
            None => self
                .recent_generation_keys(PROBE_CANDIDATES)
                .into_iter()
                .map(|k| (k, false))
                .collect(),
        };
        let mut last = Miss::NoPreviousBuild;
        for (candidate, own) in predictions {
            match self.restore_candidate(start_dir, &candidate, own, &glob_members) {
                Ok((key, files, rehashed, manifest)) => {
                    // Always rewritten: the pin file's freshness is what
                    // protects this generation's blobs from another
                    // worktree's eviction.
                    self.write_current(&key);
                    let stats = RestoreStats {
                        key,
                        files,
                        chunks: manifest.files.len(),
                        rehashed,
                        elapsed_ms: started.elapsed().as_millis(),
                    };
                    return Ok((stats, self.pin(&manifest)));
                }
                Err(miss) if own => return Err(miss),
                Err(miss) => last = miss,
            }
        }
        Err(last)
    }

    /// One prediction: the generation `candidate`'s closure, rehomed and
    /// hashed here, must name a stored entry.
    fn restore_candidate(
        &self,
        start_dir: &Path,
        candidate: &str,
        own: bool,
        glob_members: &impl Fn(&str) -> Vec<PathBuf>,
    ) -> Result<(String, usize, usize, GenerationManifest), Miss> {
        let candidate_dir = self.dir.join(candidate);
        let Some((ids, files)) = self.read_closure_rehomed(&candidate_dir.join(CLOSURE_FILE))
        else {
            // Only this root's own generation is known broken; a probed one
            // may just be from a store another oj writes.
            if own {
                let _ = fs::remove_dir_all(&candidate_dir);
            }
            return Err(Miss::ClosureUnreadable);
        };
        let stored_globs = fs::read_to_string(candidate_dir.join(GLOB_SCOPES_FILE))
            .unwrap_or_else(|_| "[]".into());
        let globs = self.rehome_globs(&stored_globs);
        let read: HashSet<&Path> = files.iter().map(PathBuf::as_path).collect();
        if let Some(added) = glob_members(&globs).into_iter().find(|f| {
            !read.contains(f.as_path())
                && fs::canonicalize(f).map_or(true, |real| !read.contains(real.as_path()))
        }) {
            return Err(Miss::GlobGrew(added));
        }
        let memo = read_memo(&self.local);
        let (digests, rehashed) = verified_digests(&files, &ids, memo.as_ref());
        let key = self.key_from(&files, &ids, &digests, globs_pin_root(&stored_globs))?;
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
        restore_artifacts(&entry, start_dir, &manifest, &key, &globs)?;
        restore_build_view(start_dir, &manifest, &files)
            .map_err(|_| Miss::ArtifactWriteFailed(CHUNK_INDEX_FILE.to_string()))?;
        if rehashed > 0 || !self.local.join(MEMO_FILE).is_file() {
            write_memo(&self.local, &build_memo(&files, &ids, &digests));
        }
        touch(&entry);
        Ok((key, files.len(), rehashed, manifest))
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

    /// The closure's hash under the store salt. Files key by their
    /// worktree-independent id, so two worktrees with the same content
    /// compute one key; `pin_root` (a generation whose globs reach outside
    /// the root) mixes the root in, keeping that generation to this root.
    fn key_from(
        &self,
        files: &[PathBuf],
        ids: &[String],
        digests: &[Option<blake3::Hash>],
        pin_root: bool,
    ) -> Result<String, Miss> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.salt.as_bytes());
        if pin_root {
            hasher.update(b"\0root=");
            hasher.update(self.root.as_os_str().as_encoded_bytes());
        }
        for ((path, id), digest) in files.iter().zip(ids).zip(digests) {
            let Some(digest) = digest else {
                return Err(Miss::ClosureFileUnreadable(path.clone()));
            };
            hasher.update(&[0]);
            hasher.update(id.as_bytes());
            hasher.update(&[0]);
            hasher.update(digest.as_bytes());
        }
        Ok(hasher.finalize().to_hex().to_string())
    }

    /// The worktree-independent identity of a closure file: `ws:<rel>` under
    /// the (canonical) root, the canonical absolute path otherwise. It keys
    /// the bundle hash, the stored closure and the memo alike.
    fn closure_id(&self, path: &Path) -> String {
        let canon = fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
        match crate::store::rel_key(&self.root, &canon) {
            Some(rel) => format!("ws:{rel}"),
            None => canon.to_string_lossy().into_owned(),
        }
    }

    /// A stored closure, rehomed against this root: the ids stay the
    /// identity, the paths are where this checkout reads the bytes.
    fn read_closure_rehomed(&self, path: &Path) -> Option<(Vec<String>, Vec<PathBuf>)> {
        let bytes = fs::read(path).ok()?;
        let mut ids: Vec<String> = serde_json::from_slice(&bytes).ok()?;
        if ids.is_empty() {
            return None;
        }
        ids.sort();
        ids.dedup();
        let files = ids
            .iter()
            .map(|id| match id.strip_prefix("ws:") {
                Some(rel) => self.root.join(rel),
                None => PathBuf::from(id),
            })
            .collect();
        Some((ids, files))
    }

    /// glob-scopes.json as stored: every pattern's root prefix (either
    /// spelling) becomes [`GLOB_ROOT_TOKEN`].
    fn relocate_globs(&self, json: &str) -> String {
        let roots: Vec<String> = [&self.root_spelled, &self.root]
            .iter()
            .map(|r| format!("{}/", r.display()))
            .collect();
        map_glob_strings(json, |s| {
            let (neg, body) = s.strip_prefix('!').map_or(("", s), |b| ("!", b));
            match roots.iter().find_map(|r| body.strip_prefix(r.as_str())) {
                Some(rest) => format!("{neg}{GLOB_ROOT_TOKEN}{rest}"),
                None => s.to_string(),
            }
        })
    }

    /// A stored glob-scopes.json with this root put back in.
    fn rehome_globs(&self, json: &str) -> String {
        let root = format!("{}/", self.root_spelled.display());
        map_glob_strings(json, |s| s.replacen(GLOB_ROOT_TOKEN, &root, 1))
    }

    fn write_current(&self, key: &str) {
        let _ = fs::create_dir_all(&self.local);
        let _ = integrity::atomic_write(&self.local.join(CURRENT_FILE), key.as_bytes());
        // The pin mirrors `current` into the shared dir, where every store's
        // eviction sees it; rewritten, not touched, so its mtime is its
        // freshness.
        let pins = self.dir.join(PINS_DIR);
        let _ = fs::create_dir_all(&pins);
        let _ = integrity::atomic_write(&pins.join(&self.pin_id), key.as_bytes());
    }

    /// Generation keys eviction must not remove: this root's `current` plus
    /// every fresh pin. A pin older than [`PIN_MAX_AGE`] belongs to a dead
    /// worktree and is deleted instead of honored.
    fn protected_keys(&self) -> HashSet<String> {
        let mut keys: HashSet<String> = read_current(&self.local).into_iter().collect();
        let Ok(entries) = fs::read_dir(self.dir.join(PINS_DIR)) else {
            return keys;
        };
        for e in entries.flatten() {
            let stale = e
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|m| m.elapsed().ok())
                .is_some_and(|age| age > PIN_MAX_AGE);
            if stale {
                let _ = fs::remove_file(e.path());
            } else if let Ok(key) = fs::read_to_string(e.path()) {
                keys.insert(key.trim().to_string());
            }
        }
        keys
    }

    /// The most recently used generations, newest first, for the cold probe.
    fn recent_generation_keys(&self, cap: usize) -> Vec<String> {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return Vec::new();
        };
        let mut gens: Vec<(SystemTime, String)> = entries
            .flatten()
            .filter(is_generation_dir)
            .map(|e| {
                (
                    prune::generation_stamp(&e.path()),
                    e.file_name().to_string_lossy().into_owned(),
                )
            })
            .collect();
        gens.sort_by_key(|(stamp, _)| std::cmp::Reverse(*stamp));
        gens.into_iter().take(cap).map(|(_, k)| k).collect()
    }
}

/// A generation dir: anything in the store dir but the blob store, the pins
/// and `.tmp-` staging.
fn is_generation_dir(e: &fs::DirEntry) -> bool {
    let name = e.file_name();
    let name = name.to_string_lossy();
    name != BLOBS_DIR && name != PINS_DIR && !name.starts_with(".tmp-") && e.path().is_dir()
}

/// Whether stored globs still name an absolute path (a pattern outside the
/// root), which pins the generation to the root that built it.
fn globs_pin_root(stored: &str) -> bool {
    let mut absolute = false;
    map_glob_strings(stored, |s| {
        absolute |= s.trim_start_matches('!').starts_with('/');
        s.to_string()
    });
    absolute
}

/// glob-scopes.json with every string inside `patterns` mapped. Anything
/// unparseable passes through as is (the restore treats it as no globs).
fn map_glob_strings(json: &str, mut f: impl FnMut(&str) -> String) -> String {
    let Ok(mut calls) = serde_json::from_str::<Vec<serde_json::Value>>(json) else {
        return json.to_string();
    };
    for call in &mut calls {
        if let Some(patterns) = call.get_mut("patterns").and_then(|p| p.as_array_mut()) {
            for p in patterns {
                if let Some(s) = p.as_str() {
                    *p = serde_json::Value::String(f(s));
                }
            }
        }
    }
    serde_json::to_string(&calls).unwrap_or_else(|_| json.to_string())
}

/// Copies the generation's artifacts and css urls into the build dir. A
/// missing artifact means the entry is corrupt and it is removed.
fn restore_artifacts(
    entry: &Path,
    start_dir: &Path,
    manifest: &GenerationManifest,
    key: &str,
    globs: &str,
) -> Result<(), Miss> {
    for name in ARTIFACTS {
        let bytes = match name {
            GLOB_SCOPES_FILE => Ok(globs.as_bytes().to_vec()),
            _ => fs::read(entry.join(name)),
        };
        let Ok(bytes) = bytes else {
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

/// The small files a fresh bundle leaves in the build dir besides the
/// artifacts: the chunk index and the closure, rehomed onto this root. A
/// restore from the shared store lands in a build dir that never built, so
/// without them the dev rebundle gate has no closure to consult. The chunks
/// themselves stay in the blob store, served from there (no per-boot copy
/// of the chunk set; see [`StartBundleStore::restore`]).
fn restore_build_view(
    start_dir: &Path,
    manifest: &GenerationManifest,
    files: &[PathBuf],
) -> std::io::Result<()> {
    let index: Vec<serde_json::Value> = manifest
        .files
        .iter()
        .map(|(name, f)| serde_json::json!({ "name": name, "size": f.size }))
        .collect();
    let index = serde_json::json!({ "entry": manifest.entry, "files": index });
    integrity::atomic_write(
        &start_dir.join(CHUNK_INDEX_FILE),
        index.to_string().as_bytes(),
    )?;
    let closure: Vec<String> = files
        .iter()
        .map(|f| f.to_string_lossy().into_owned())
        .collect();
    integrity::atomic_write(
        &start_dir.join(CLOSURE_FILE),
        &serde_json::to_vec(&closure).unwrap_or_default(),
    )
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
