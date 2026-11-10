// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::{Path, PathBuf};

pub mod config_extract;
pub mod integrity;
pub mod start_bundle;

use serde::{Deserialize, Serialize};

pub mod start_codegen;

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
/// A module source's content hash, the part of a compile key that costs
/// reading the whole source: computed once per compile and handed to
/// [`module_key`] for every key derived from it.
pub type SourceHash = [u8; 32];

pub fn source_hash(source: &[u8]) -> SourceHash {
    *blake3::hash(source).as_bytes()
}

/// The dev server's in-memory module cache key: the compile mode (with the
/// import stamps and the other inputs the server folds in), the served url
/// and the source's hash.
pub fn module_key(source: &SourceHash, url: &str, mode: &str) -> String {
    hash_nul_joined(&[mode.as_bytes(), url.as_bytes(), source])
        .to_hex()
        .to_string()
}

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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_key_changes_with_every_input_and_keeps_field_boundaries() {
        let hash = source_hash(b"source");
        let base = module_key(&hash, "/src/App.tsx", "dev");
        assert_eq!(
            base,
            module_key(&source_hash(b"source"), "/src/App.tsx", "dev")
        );
        assert_ne!(
            base,
            module_key(&source_hash(b"source2"), "/src/App.tsx", "dev"),
            "content"
        );
        assert_ne!(base, module_key(&hash, "/src/Other.tsx", "dev"), "url");
        assert_ne!(base, module_key(&hash, "/src/App.tsx", "dev@5"), "mode");
        // The NUL separators keep "a","bc" apart from "ab","c".
        assert_ne!(module_key(&hash, "a", "bc"), module_key(&hash, "ab", "c"));
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
