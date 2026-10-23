// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub mod config_extract;
pub mod integrity;
pub mod start_bundle;
pub mod store;

use serde::{Deserialize, Serialize};

pub mod start_codegen;

// Bump whenever CachedModule/HotMeta meaning changes, so older entries are
// never deserialized with the new meaning.
pub const CACHE_FORMAT: u32 = 7;

pub const CACHE_ROOT_VERSION: u32 = 1;

/// Where everything oj caches for an app goes.
///
/// `<app>/.oj-cache` by default, and `OJ_CACHE_DIR` when the caller has
/// somewhere else for it. A build system serving a source tree it does not own
/// -- read-only, shared between concurrent builds, or checked for cleanliness
/// afterwards -- needs the writes to land outside it, and `OJ_SSR_BRIDGE_DIR`
/// already works this way for the SSR bridge.
///
/// The version segment is appended either way: it is what makes an older
/// layout inert rather than misread, and a caller pointing at one directory for
/// several apps still gets that.
pub fn cache_root(app_root: &Path) -> PathBuf {
    cache_root_in(app_root, oj_env::get().knobs.cache_dir.as_deref())
}

/// [`cache_root`] with `OJ_CACHE_DIR` given as `dir`.
pub fn cache_root_in(app_root: &Path, dir: Option<&Path>) -> PathBuf {
    cache_base_in(app_root, dir).join(format!("v{CACHE_ROOT_VERSION}"))
}

/// The directory `cache_root` versions inside. Callers that manage the cache
/// area itself -- creating it, marking it ignored, healing an older layout --
/// want this rather than the versioned path, and they have to agree with
/// `cache_root` about where it is.
pub fn cache_base(app_root: &Path) -> PathBuf {
    cache_base_in(app_root, oj_env::get().knobs.cache_dir.as_deref())
}

/// [`cache_base`] with `OJ_CACHE_DIR` given as `dir` (an empty one is unset).
pub fn cache_base_in(app_root: &Path, dir: Option<&Path>) -> PathBuf {
    match dir {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => app_root.join(".oj-cache"),
    }
}

const LEGACY_TOP_LEVEL: &[&str] = &[
    "start",
    "start-codegen",
    "start-bundle",
    "config-extract",
    "ssr",
    "deps",
    "v8",
    "graph-snapshot.json",
    "css-sidecar.mjs",
    "svelte-compile.mjs",
    "css-preprocess.mjs",
    "tailwind-sidecar.mjs",
    "oj-vite-config.mjs",
    "oj-vite-extract.mjs",
    "plugin-host.mjs",
    "optimize-deps.mjs",
    "ssr-env.json",
    "ssr-load-pack.jsonl",
    "ssr-resolve.jsonl",
    "ssr-bridge-pack.jsonl",
];

/// Removes what pre-versioned layouts left at the top of the cache base:
/// the names in `LEGACY_TOP_LEVEL` and the old two-hex-digit shard dirs.
/// Anything else (including the versioned root) is left alone.
pub fn heal_legacy_layout(app_root: &Path) {
    let root = cache_base(app_root);
    for name in LEGACY_TOP_LEVEL {
        let path = root.join(name);
        if path.is_dir() {
            let _ = fs::remove_dir_all(&path);
        } else {
            let _ = fs::remove_file(&path);
        }
    }
    let Ok(entries) = fs::read_dir(&root) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.len() == 2 && is_lower_hex(name) && e.path().is_dir() {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

fn is_lower_hex(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// blake3 over `fields` joined by NUL bytes. The separators keep field
/// boundaries unambiguous ("a","bc" vs "ab","c").
pub(crate) fn hash_nul_joined(fields: &[&[u8]]) -> blake3::Hash {
    let mut hasher = blake3::Hasher::new();
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            hasher.update(&[0]);
        }
        hasher.update(field);
    }
    hasher.finalize()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct CachedModule {
    pub code: String,
    /// Raw sourcemap JSON; encoded to a data URL at serve time (Vite's
    /// genSourceMapUrl shape), so caches retain 25% fewer map bytes.
    pub map_json: Option<String>,
    pub imports: Vec<String>,
    pub is_boundary: bool,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub require_map: Vec<(String, String)>,
    #[serde(default)]
    pub css_exports: Vec<(String, String)>,
    #[serde(default)]
    pub fs_allow: Vec<String>,
    #[serde(default)]
    pub watch_files: Vec<String>,
    /// Per import (served url), the binding names this module uses from it
    /// (Vite's `importedBindings`), for partial-accept (`acceptExports`)
    /// propagation decisions.
    #[serde(default)]
    pub import_bindings: Vec<(String, Vec<String>)>,
    /// `Some` when the module references `import.meta.hot`: it gets a hot
    /// context, and these are the `accept` declarations for the module graph.
    #[serde(default)]
    pub hot: Option<HotMeta>,
    /// The StyleX rules the module compiled to, so a cache hit can restore
    /// its part of the stylesheet without recompiling.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stylex_rules: Vec<oj_stylex::rules::StylexRule>,
    /// Constructive trace (pulp P2): files a plugin hook actually READ while
    /// producing this entry, beyond the module source the key already covers.
    /// A warm hit re-verifies every row; any mismatch (edited, deleted) is a
    /// miss, never a stale serve. Empty for the vast majority of modules, so
    /// verification costs nothing where plugins did nothing.
    #[serde(default)]
    pub trace: Vec<TraceRow>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TraceRow {
    /// The file's identity: absolute in memory, `ws:`/`mono:`-relative in the
    /// stored form (see `relocated_for_store`).
    pub id: String,
    /// blake3 of the file's content at build time.
    pub digest: String,
}

impl CachedModule {
    /// The on-disk form: absolute paths under `root` become `ws:<rel>` so the
    /// stored artifact means the same thing in every worktree. Fields it
    /// covers are the metadata paths oj itself consumes (`fs_allow`,
    /// `watch_files`); anything not under the root stays absolute, which
    /// keeps the entry correct and merely non-shareable. `rehome` is the
    /// inverse, applied on read against the reading worktree's root.
    pub fn relocated_for_store(&self, root: &Path, workspace: &Path) -> CachedModule {
        let mut out = self.clone();
        let relocate = |entry: &mut String| {
            // Glob rows carry their pattern behind a `glob:` tag; the inner
            // pattern relocates like a path.
            let (prefix, inner) = match entry.strip_prefix("glob:") {
                Some(inner) => ("glob:", inner.to_string()),
                None => ("", entry.clone()),
            };
            if let Some(rel) = crate::store::rel_key(root, Path::new(&inner)) {
                *entry = format!("{prefix}ws:{rel}");
            } else if let Some(rel) = crate::store::rel_key(workspace, Path::new(&inner)) {
                *entry = format!("{prefix}mono:{rel}");
            }
        };
        for list in [&mut out.fs_allow, &mut out.watch_files] {
            for entry in list.iter_mut() {
                relocate(entry);
            }
        }
        for row in &mut out.trace {
            relocate(&mut row.id);
        }
        out
    }

    /// Expand `ws:<rel>` metadata paths against this worktree's root: the
    /// read-side inverse of `relocated_for_store`.
    pub fn rehomed(&self, root: &Path, workspace: &Path) -> CachedModule {
        let mut out = self.clone();
        let rehome = |entry: &mut String| {
            let (prefix, inner) = match entry.strip_prefix("glob:") {
                Some(inner) => ("glob:", inner.to_string()),
                None => ("", entry.clone()),
            };
            if let Some(rel) = inner.strip_prefix("ws:") {
                *entry = format!("{prefix}{}", root.join(rel).to_string_lossy());
            } else if let Some(rel) = inner.strip_prefix("mono:") {
                *entry = format!("{prefix}{}", workspace.join(rel).to_string_lossy());
            }
        };
        for list in [&mut out.fs_allow, &mut out.watch_files] {
            for entry in list.iter_mut() {
                rehome(entry);
            }
        }
        for row in &mut out.trace {
            rehome(&mut row.id);
        }
        out
    }

    /// True when nothing in the stored form still points into a specific
    /// worktree: the admission test for a shared store. Checks the metadata
    /// paths and the rewritten import URLs (`/@fs/<abs>` pins an artifact to
    /// one checkout; `importer=` hex params are covered by their own `ws:`
    /// encoding at generation time).
    pub fn is_relocatable(&self) -> bool {
        let path_free = |s: &str| {
            let s = s.strip_prefix("glob:").unwrap_or(s);
            s.starts_with("ws:") || s.starts_with("mono:") || !Path::new(s).is_absolute()
        };
        // Served-URL fields: /@fs/<abs> and /@oj-pkg/<hex abs> pin an entry
        // to one checkout (the latter hex-encodes an absolute entry path).
        // /@fs/@mono/<rel> is the workspace-relative form: the serving
        // worktree resolves it against its OWN workspace root, so it does
        // not pin.
        let pinned_url = |s: &str| {
            (s.starts_with("/@fs/") && !s.starts_with("/@fs/@mono/")) || s.starts_with("/@oj-pkg/")
        };
        self.fs_allow.iter().all(|p| path_free(p))
            && self.watch_files.iter().all(|p| path_free(p))
            && self.trace.iter().all(|row| path_free(&row.id))
            && !self.imports.iter().any(|i| pinned_url(i))
            && !self
                .require_map
                .iter()
                .any(|(_, target)| pinned_url(target))
            && !code_pins(&self.code)
            && !self.code.contains("/@oj-pkg/")
    }
}

/// True when `code` contains a worktree-pinning `/@fs/` reference: any
/// occurrence NOT in the workspace-relative `/@fs/@mono/` form.
fn code_pins(code: &str) -> bool {
    let mut rest = code;
    while let Some(i) = rest.find("/@fs/") {
        let after = &rest[i + "/@fs/".len()..];
        if !after.starts_with("@mono/") {
            return true;
        }
        rest = after;
    }
    false
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HotMeta {
    pub self_accept: bool,
    /// Served urls of the dependencies this module accepts updates for.
    pub deps: Vec<String>,
    /// `acceptExports(names)`: the module is a boundary only for changes its
    /// importers reach through these exports (Vite's `acceptedHmrExports`).
    #[serde(default)]
    pub accepted_exports: Option<Vec<String>>,
}

pub struct PersistentCache {
    /// The per-app home: entries whose stored form still pins a worktree
    /// (`/@fs/` imports, absolute metadata) live here and nowhere else.
    dir: PathBuf,
    /// The shared per-user modules directory. Entries that pass
    /// [`CachedModule::is_relocatable`] are written here, where every
    /// worktree (and every app whose salted key matches) can hit them.
    shared: Option<PathBuf>,
    salt: String,
}

impl PersistentCache {
    pub fn new(dir: PathBuf, tool_version: &str) -> Self {
        Self {
            dir,
            shared: None,
            salt: format!("{tool_version}:{CACHE_FORMAT}"),
        }
    }

    /// Attach the shared modules directory (pulp P1 capstone). Callers hand
    /// entries to `put` in their stored (`ws:`-relative) form; `put` admits
    /// them to the shared directory only when nothing in that form still
    /// points into a specific worktree.
    pub fn with_shared(mut self, shared: Option<PathBuf>) -> Self {
        self.shared = shared;
        self
    }

    /// Compiled modules embed inlined `import.meta.env` values, so anything
    /// that changes the defines (dotenv edits, process env, plugin config()
    /// mutations) must land in the salt or warm restarts serve stale env.
    pub fn with_salt_extra(mut self, extra: &str) -> Self {
        self.salt.push(':');
        self.salt.push_str(extra);
        self
    }

    pub fn key(&self, source: &[u8], url: &str, mode: &str) -> String {
        hash_nul_joined(&[
            self.salt.as_bytes(),
            mode.as_bytes(),
            url.as_bytes(),
            source,
        ])
        .to_hex()
        .to_string()
    }

    pub fn get(&self, key: &str) -> Option<CachedModule> {
        // Shared first: relocatable entries (most of a typical app) are
        // written there. A corrupt entry is removed from whichever directory
        // held it and reads fall through.
        if let Some(module) = self
            .shared
            .as_deref()
            .and_then(|shared| integrity::read_json_or_remove(&Self::path_in(shared, key)?))
        {
            return Some(module);
        }
        integrity::read_json_or_remove(&Self::path_in(&self.dir, key)?)
    }

    pub fn put(&self, key: &str, module: &CachedModule) {
        // Admission control for the shared store: only an entry whose stored
        // form carries no worktree-specific path may leave this app's cache.
        // Everything else stays correct and local.
        let dir = match &self.shared {
            Some(shared) if module.is_relocatable() => shared,
            _ => &self.dir,
        };
        let Some(path) = Self::path_in(dir, key) else {
            return;
        };
        let Some(parent) = path.parent() else { return };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        // Per-write sequence: threads of one process may put the same key
        // concurrently and must not share a temp file.
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp-{}-{seq}", std::process::id()));
        let bytes = serde_json::to_vec(module).unwrap_or_default();
        let _ = integrity::write_via_tmp(&tmp, &path, &bytes);
    }

    /// `<dir>/<key[..2]>/<key>.json`; `None` for anything but a lowercase
    /// blake3 hex digest, so a key can never escape the cache dir.
    fn path_in(dir: &Path, key: &str) -> Option<PathBuf> {
        if key.len() != 64 || !is_lower_hex(key) {
            return None;
        }
        Some(dir.join(&key[..2]).join(format!("{key}.json")))
    }
}

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_cache(label: &str) -> PersistentCache {
        let dir =
            std::env::temp_dir().join(format!("oj-cache-test-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        PersistentCache::new(dir, "0.0.1-test")
    }

    fn sample() -> CachedModule {
        CachedModule {
            code: "export const x = 1;".into(),
            map_json: Some("data:application/json;base64,e30=".into()),
            imports: vec!["/node_modules/react/index.js".into()],
            is_boundary: true,
            kind: "esm".into(),
            require_map: vec![("react".into(), "/node_modules/react/index.js".into())],
            css_exports: Vec::new(),
            fs_allow: Vec::new(),
            watch_files: Vec::new(),
            import_bindings: Vec::new(),
            hot: None,
            stylex_rules: Vec::new(),
            trace: Vec::new(),
        }
    }

    #[test]
    fn roundtrip() {
        let cache = temp_cache("roundtrip");
        let key = cache.key(b"source", "/src/App.tsx", "dev");
        assert_eq!(cache.get(&key), None);
        cache.put(&key, &sample());
        assert_eq!(cache.get(&key), Some(sample()));
    }

    #[test]
    fn every_input_changes_the_key() {
        let cache = temp_cache("keys");
        let base = cache.key(b"source", "/src/App.tsx", "dev");
        assert_ne!(
            base,
            cache.key(b"source2", "/src/App.tsx", "dev"),
            "content"
        );
        assert_ne!(base, cache.key(b"source", "/src/Other.tsx", "dev"), "url");
        assert_ne!(base, cache.key(b"source", "/src/App.tsx", "prod"), "mode");
        let other_version = PersistentCache::new(std::env::temp_dir(), "9.9.9");
        assert_ne!(
            base,
            other_version.key(b"source", "/src/App.tsx", "dev"),
            "version"
        );
        let salted = PersistentCache::new(std::env::temp_dir(), "9.9.9").with_salt_extra("env-a");
        let resalted = PersistentCache::new(std::env::temp_dir(), "9.9.9").with_salt_extra("env-b");
        assert_ne!(
            salted.key(b"source", "/src/App.tsx", "dev"),
            resalted.key(b"source", "/src/App.tsx", "dev"),
            "env defines salt"
        );
    }

    #[test]
    fn corrupt_entries_are_dropped_not_served() {
        let cache = temp_cache("corrupt");
        let key = cache.key(b"s", "/u", "dev");
        cache.put(&key, &sample());
        let path = PersistentCache::path_in(&cache.dir, &key).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(cache.get(&key), None);
        assert!(!path.exists(), "corrupt entry must be removed");
    }

    #[test]
    fn shards_by_key_prefix_and_leaves_no_temp_file() {
        let cache = temp_cache("shard");
        let key = cache.key(b"s", "/u", "dev");
        cache.put(&key, &sample());
        let path = PersistentCache::path_in(&cache.dir, &key).unwrap();
        assert_eq!(
            path.parent()
                .unwrap()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            &key[..2]
        );
        assert!(path.exists());
        let shard = path.parent().unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(shard)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files must be renamed away: {leftovers:?}"
        );
    }

    #[test]
    fn field_separators_prevent_key_collisions() {
        let cache = temp_cache("sep");
        let a = cache.key(b"bc", "a", "dev");
        let b = cache.key(b"c", "ab", "dev");
        assert_ne!(a, b, "url/source boundary must be unambiguous");
    }

    #[test]
    fn cache_root_is_versioned() {
        let root = cache_root_in(Path::new("/app"), None);
        assert_eq!(
            root,
            Path::new("/app")
                .join(".oj-cache")
                .join(format!("v{CACHE_ROOT_VERSION}"))
        );
    }

    #[test]
    fn heal_removes_legacy_layout_and_keeps_the_rest() {
        let app = std::env::temp_dir().join(format!("oj-heal-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&app);
        let cache = app.join(".oj-cache");
        for dir in ["start/ssr-bridge", "start-bundle", "deps", "ab", "3f"] {
            fs::create_dir_all(cache.join(dir)).unwrap();
        }
        for file in ["ssr-resolve.jsonl", "graph-snapshot.json"] {
            fs::write(cache.join(file), b"x").unwrap();
        }
        fs::write(cache.join(".gitignore"), b"*\n").unwrap();
        fs::create_dir_all(cache_root(&app).join("start")).unwrap();
        fs::create_dir_all(cache.join("user-stuff")).unwrap();

        heal_legacy_layout(&app);

        for legacy in [
            "start",
            "start-bundle",
            "deps",
            "ab",
            "3f",
            "ssr-resolve.jsonl",
            "graph-snapshot.json",
        ] {
            assert!(!cache.join(legacy).exists(), "{legacy} must be removed");
        }
        assert!(cache.join(".gitignore").exists());
        assert!(cache_root(&app).join("start").exists());
        assert!(
            cache.join("user-stuff").exists(),
            "unknown dirs are left alone"
        );
    }

    fn shared_pair(label: &str) -> (PersistentCache, PersistentCache, PathBuf) {
        let base =
            std::env::temp_dir().join(format!("oj-shared-cache-{}-{label}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let shared = base.join("store/modules");
        let a = PersistentCache::new(base.join("wt-a/.oj-cache/v1"), "0.0.1-test")
            .with_shared(Some(shared.clone()));
        let b = PersistentCache::new(base.join("wt-b/.oj-cache/v1"), "0.0.1-test")
            .with_shared(Some(shared.clone()));
        (a, b, base)
    }

    #[test]
    fn relocatable_entries_are_shared_across_worktrees() {
        let (a, b, base) = shared_pair("hit");
        let module = CachedModule {
            code: "export const x = 1;".into(),
            fs_allow: vec!["ws:node_modules/dep".into()],
            ..Default::default()
        };
        let key = a.key(b"src", "/src/App.tsx", "dev");
        a.put(&key, &module);
        assert_eq!(
            b.get(&key).as_ref(),
            Some(&module),
            "worktree B reads what worktree A compiled"
        );
        // It lives in the shared directory, not in either app's cache.
        assert!(base
            .join("store/modules")
            .join(&key[..2])
            .join(format!("{key}.json"))
            .is_file());
        assert!(!base.join("wt-a/.oj-cache/v1").join(&key[..2]).exists());
    }

    #[test]
    fn pinned_entries_stay_in_their_own_worktree() {
        let (a, b, base) = shared_pair("pin");
        let pinned = CachedModule {
            code: "import u from \"/@fs//work/wt-a/pkg/u.ts\";".into(),
            imports: vec!["/@fs//work/wt-a/pkg/u.ts".into()],
            ..Default::default()
        };
        let key = a.key(b"src", "/src/uses-sibling.ts", "dev");
        a.put(&key, &pinned);
        assert_eq!(
            a.get(&key).as_ref(),
            Some(&pinned),
            "A still hits its own entry"
        );
        assert_eq!(
            b.get(&key),
            None,
            "B must never receive an entry that points into A's checkout"
        );
        assert!(!base.join("store/modules").join(&key[..2]).exists());
    }

    #[test]
    fn without_a_shared_dir_everything_stays_local() {
        let cache = temp_cache("no-shared");
        let module = sample();
        let key = cache.key(b"src", "/src/a.ts", "dev");
        cache.put(&key, &module);
        assert_eq!(cache.get(&key).as_ref(), Some(&module));
    }

    #[test]
    fn corrupt_shared_entry_is_removed_and_misses() {
        let (a, b, base) = shared_pair("corrupt");
        let key = a.key(b"src", "/src/App.tsx", "dev");
        let shard = base.join("store/modules").join(&key[..2]);
        std::fs::create_dir_all(&shard).unwrap();
        let path = shard.join(format!("{key}.json"));
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(b.get(&key), None);
        assert!(!path.exists(), "corrupt shared entry deleted on read");
        let _ = a;
    }

    #[test]
    fn relocatability_covers_require_map_and_pkg_bundle_urls() {
        let clean = CachedModule {
            code: "require(\"react\");".into(),
            require_map: vec![("react".into(), "/node_modules/react/index.js".into())],
            ..Default::default()
        };
        assert!(clean.is_relocatable());
        let fs_require = CachedModule {
            require_map: vec![("sib".into(), "/@fs//work/wt-a/sib/index.js".into())],
            ..clean.clone()
        };
        assert!(!fs_require.is_relocatable(), "/@fs require target pins");
        let pkg_import = CachedModule {
            imports: vec!["/@oj-pkg/6162".into()],
            ..clean.clone()
        };
        assert!(
            !pkg_import.is_relocatable(),
            "/@oj-pkg hex-encodes an absolute path"
        );
    }

    #[test]
    fn cached_module_relocates_metadata_and_rehomes_in_another_worktree() {
        let root_a = Path::new("/work/wt-a/mono/app");
        let ws_a = Path::new("/work/wt-a/mono");
        let root_b = Path::new("/elsewhere/wt-b/mono/app");
        let ws_b = Path::new("/elsewhere/wt-b/mono");
        let module = CachedModule {
            code: "export {}".into(),
            fs_allow: vec![
                "/work/wt-a/mono/app/node_modules/dep".into(),
                "/work/wt-a/mono/packages/common".into(),
                "/outside/sibling".into(),
            ],
            watch_files: vec!["/work/wt-a/mono/app/tailwind.config.js".into()],
            ..Default::default()
        };
        let stored = module.relocated_for_store(root_a, ws_a);
        assert_eq!(
            stored.fs_allow,
            vec![
                "ws:node_modules/dep".to_string(),
                "mono:packages/common".to_string(),
                "/outside/sibling".to_string()
            ],
            "root-relative, workspace-relative, and external forms"
        );
        assert_eq!(
            stored.watch_files,
            vec!["ws:tailwind.config.js".to_string()]
        );
        // Reading worktree B expands against ITS root and workspace.
        let rehomed = stored.rehomed(root_b, ws_b);
        assert_eq!(
            rehomed.fs_allow,
            vec![
                "/elsewhere/wt-b/mono/app/node_modules/dep".to_string(),
                "/elsewhere/wt-b/mono/packages/common".to_string(),
                "/outside/sibling".to_string()
            ]
        );
        assert_eq!(
            rehomed.watch_files,
            vec!["/elsewhere/wt-b/mono/app/tailwind.config.js".to_string()]
        );
        // The roundtrip in one worktree is the identity.
        assert_eq!(
            module
                .relocated_for_store(root_a, ws_a)
                .rehomed(root_a, ws_a),
            module
        );
    }

    #[test]
    fn trace_rows_relocate_rehome_and_gate_admission() {
        let root_a = Path::new("/wt-a/mono/app");
        let ws_a = Path::new("/wt-a/mono");
        let module = CachedModule {
            code: "export {}".into(),
            trace: vec![
                TraceRow {
                    id: "/wt-a/mono/app/extra.txt".into(),
                    digest: "d1".into(),
                },
                TraceRow {
                    id: "/wt-a/mono/packages/tokens.json".into(),
                    digest: "d2".into(),
                },
                TraceRow {
                    id: "/etc/hosts".into(),
                    digest: "d3".into(),
                },
            ],
            ..Default::default()
        };
        let stored = module.relocated_for_store(root_a, ws_a);
        assert_eq!(stored.trace[0].id, "ws:extra.txt");
        assert_eq!(stored.trace[1].id, "mono:packages/tokens.json");
        assert_eq!(stored.trace[2].id, "/etc/hosts");
        assert!(
            !stored.is_relocatable(),
            "a trace row outside the workspace pins the entry"
        );
        let in_ws = CachedModule {
            trace: stored.trace[..2].to_vec(),
            ..stored.clone()
        };
        assert!(in_ws.is_relocatable(), "ws:/mono: trace rows do not pin");
        // B rehomes against its own roots; digests travel untouched.
        let rehomed = in_ws.rehomed(Path::new("/b/mono/app"), Path::new("/b/mono"));
        assert_eq!(rehomed.trace[0].id, "/b/mono/app/extra.txt");
        assert_eq!(rehomed.trace[1].id, "/b/mono/packages/tokens.json");
        assert_eq!(rehomed.trace[1].digest, "d2");
    }

    #[test]
    fn glob_trace_rows_relocate_behind_their_tag() {
        let module = CachedModule {
            trace: vec![TraceRow {
                id: "glob:/wt-a/mono/app/globbed/*.ts".into(),
                digest: "d".into(),
            }],
            ..Default::default()
        };
        let stored =
            module.relocated_for_store(Path::new("/wt-a/mono/app"), Path::new("/wt-a/mono"));
        assert_eq!(stored.trace[0].id, "glob:ws:globbed/*.ts");
        assert!(stored.is_relocatable());
        let rehomed = stored.rehomed(Path::new("/b/app"), Path::new("/b"));
        assert_eq!(rehomed.trace[0].id, "glob:/b/app/globbed/*.ts");
        // Out-of-workspace glob pins.
        let pinned = CachedModule {
            trace: vec![TraceRow {
                id: "glob:/elsewhere/*.ts".into(),
                digest: "d".into(),
            }],
            ..Default::default()
        };
        assert!(!pinned
            .relocated_for_store(Path::new("/wt-a/app"), Path::new("/wt-a"))
            .is_relocatable());
    }

    #[test]
    fn relocatability_is_judged_on_the_stored_form() {
        let clean = CachedModule {
            code: "import x from \"/src/a.js\";".into(),
            imports: vec!["/src/a.js".into()],
            fs_allow: vec!["ws:node_modules/dep".into()],
            watch_files: vec![],
            ..Default::default()
        };
        assert!(clean.is_relocatable());

        let abs_allow = CachedModule {
            fs_allow: vec!["/work/wt-a/node_modules/dep".into()],
            ..clean.clone()
        };
        assert!(
            !abs_allow.is_relocatable(),
            "absolute fs_allow pins the entry"
        );

        let fs_import = CachedModule {
            imports: vec!["/@fs//work/wt-a/pkg/util.ts".into()],
            ..clean.clone()
        };
        assert!(!fs_import.is_relocatable(), "/@fs import pins the entry");

        let fs_in_code = CachedModule {
            code: "import u from \"/@fs//work/wt-a/pkg/util.ts\";".into(),
            ..clean.clone()
        };
        assert!(!fs_in_code.is_relocatable());

        // The workspace-relative /@fs/@mono form does NOT pin: the serving
        // worktree resolves it against its own workspace root.
        let mono_import = CachedModule {
            code: "import r from \"/@fs/@mono/node_modules/react/index.js?v=1\";".into(),
            imports: vec!["/@fs/@mono/node_modules/react/index.js?v=1".into()],
            fs_allow: vec!["mono:node_modules/react".into()],
            ..clean.clone()
        };
        assert!(
            mono_import.is_relocatable(),
            "@mono urls are worktree-independent"
        );
        // But one pinning occurrence among mono ones still pins.
        let mixed = CachedModule {
            code: "import a from \"/@fs/@mono/x.js\"; import b from \"/@fs//work/wt-a/y.js\";"
                .into(),
            ..clean.clone()
        };
        assert!(!mixed.is_relocatable(), "one absolute /@fs still pins");
    }

    #[test]
    fn cache_root_is_relocatable_and_stays_versioned() {
        let app = Path::new("/srv/app");
        let versioned = format!("v{CACHE_ROOT_VERSION}");

        let out = Some(Path::new("/out/oj"));
        assert_eq!(
            cache_root_in(app, None),
            app.join(".oj-cache").join(&versioned)
        );
        assert_eq!(
            cache_root_in(app, out),
            Path::new("/out/oj").join(&versioned)
        );
        // An empty value is not a directory; it must not put the cache at "/v1".
        assert_eq!(
            cache_root_in(app, Some(Path::new(""))),
            app.join(".oj-cache").join(&versioned)
        );
        // Whoever creates and marks the cache area has to land in the same
        // place, or a relocated cache still leaves a directory in the app.
        assert_eq!(cache_base_in(app, out), Path::new("/out/oj"));
        assert!(cache_root_in(app, out).starts_with(cache_base_in(app, out)));
    }
}
