// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub mod config_extract;
pub mod integrity;
pub mod lockfiles;
pub mod start_bundle;

pub use lockfiles::{lockfile_digest, LockfileDigest};

use serde::{Deserialize, Serialize};

pub mod start_codegen;

pub const CACHE_FORMAT: u32 = 5;

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
    cache_base(app_root).join(format!("v{CACHE_ROOT_VERSION}"))
}

/// The directory `cache_root` versions inside. Callers that manage the cache
/// area itself -- creating it, marking it ignored, healing an older layout --
/// want this rather than the versioned path, and they have to agree with
/// `cache_root` about where it is.
pub fn cache_base(app_root: &Path) -> PathBuf {
    match std::env::var_os("OJ_CACHE_DIR") {
        Some(dir) if !dir.is_empty() => PathBuf::from(dir),
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
        if name.len() == 2
            && name.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
            && e.path().is_dir()
        {
            let _ = fs::remove_dir_all(e.path());
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CachedModule {
    pub code: String,
    pub map_data_url: Option<String>,
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
    /// `Some` when the module references `import.meta.hot`: it gets a hot
    /// context, and these are the `accept` declarations for the module graph.
    #[serde(default)]
    pub hot: Option<HotMeta>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct HotMeta {
    pub self_accept: bool,
    /// Served urls of the dependencies this module accepts updates for.
    pub deps: Vec<String>,
}

pub struct PersistentCache {
    dir: PathBuf,
    salt: String,
}

impl PersistentCache {
    pub fn new(dir: PathBuf, tool_version: &str) -> Self {
        Self {
            dir,
            salt: format!("{tool_version}:{CACHE_FORMAT}"),
        }
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
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.salt.as_bytes());
        hasher.update(&[0]);
        hasher.update(mode.as_bytes());
        hasher.update(&[0]);
        hasher.update(url.as_bytes());
        hasher.update(&[0]);
        hasher.update(source);
        hasher.finalize().to_hex().to_string()
    }

    pub fn get(&self, key: &str) -> Option<CachedModule> {
        let path = self.path_for(key)?;
        let bytes = fs::read(&path).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(module) => Some(module),
            Err(_) => {
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    pub fn put(&self, key: &str, module: &CachedModule) {
        let Some(path) = self.path_for(key) else { return };
        let Some(parent) = path.parent() else { return };
        if fs::create_dir_all(parent).is_err() {
            return;
        }
        let seq = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let tmp = path.with_extension(format!("tmp-{}-{seq}", std::process::id()));
        if fs::write(&tmp, serde_json::to_vec(module).unwrap_or_default()).is_ok() {
            if fs::rename(&tmp, &path).is_err() {
                let _ = fs::remove_file(&tmp);
            }
        } else {
            let _ = fs::remove_file(&tmp);
        }
    }

    fn path_for(&self, key: &str) -> Option<PathBuf> {
        let is_digest = key.len() == 64
            && key
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
        if !is_digest {
            return None;
        }
        Some(self.dir.join(&key[..2]).join(format!("{key}.json")))
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
            map_data_url: Some("data:application/json;base64,e30=".into()),
            imports: vec!["/node_modules/react/index.js".into()],
            is_boundary: true,
            kind: "esm".into(),
            require_map: vec![("react".into(), "/node_modules/react/index.js".into())],
            css_exports: Vec::new(),
            fs_allow: Vec::new(),
            watch_files: Vec::new(),
            hot: None,
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
        let path = cache.path_for(&key).unwrap();
        std::fs::write(&path, b"{ not json").unwrap();
        assert_eq!(cache.get(&key), None);
        assert!(!path.exists(), "corrupt entry must be removed");
    }

    #[test]
    fn shards_by_key_prefix_and_leaves_no_temp_file() {
        let cache = temp_cache("shard");
        let key = cache.key(b"s", "/u", "dev");
        cache.put(&key, &sample());
        let path = cache.path_for(&key).unwrap();
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
        assert!(leftovers.is_empty(), "temp files must be renamed away: {leftovers:?}");
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
        let _g = env_guard(); // reads the default; must not race an OJ_CACHE_DIR setter
        let root = cache_root(Path::new("/app"));
        assert_eq!(
            root,
            Path::new("/app")
                .join(".oj-cache")
                .join(format!("v{CACHE_ROOT_VERSION}"))
        );
    }

    #[test]
    fn heal_removes_legacy_layout_and_keeps_the_rest() {
        let _g = env_guard(); // uses cache_root(default); must not race an OJ_CACHE_DIR setter
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
        assert!(cache.join("user-stuff").exists(), "unknown dirs are left alone");
    }

    #[test]
    fn cache_root_is_relocatable_and_stays_versioned() {
        let app = Path::new("/srv/app");
        let versioned = format!("v{CACHE_ROOT_VERSION}");

        temp_env_var("OJ_CACHE_DIR", None, || {
            assert_eq!(cache_root(app), app.join(".oj-cache").join(&versioned));
        });
        temp_env_var("OJ_CACHE_DIR", Some("/out/oj"), || {
            assert_eq!(cache_root(app), Path::new("/out/oj").join(&versioned));
        });
        // An empty value is not a directory; it must not put the cache at "/v1".
        temp_env_var("OJ_CACHE_DIR", Some(""), || {
            assert_eq!(cache_root(app), app.join(".oj-cache").join(&versioned));
        });
        // Whoever creates and marks the cache area has to land in the same
        // place, or a relocated cache still leaves a directory in the app.
        temp_env_var("OJ_CACHE_DIR", Some("/out/oj"), || {
            assert_eq!(cache_base(app), Path::new("/out/oj"));
            assert!(cache_root(app).starts_with(cache_base(app)));
        });
    }

    // `OJ_CACHE_DIR` is process-wide, and cargo runs these tests as threads in
    // one process, so a test that sets it races a test that reads the default.
    // Every test touching `OJ_CACHE_DIR` (setters via temp_env_var, readers via
    // this guard) serializes on one lock; a setter restores the var before it
    // releases the lock, so the next reader sees the ambient (unset) value.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    fn env_guard() -> std::sync::MutexGuard<'static, ()> {
        ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn temp_env_var(key: &str, value: Option<&str>, f: impl FnOnce()) {
        let _guard = env_guard();
        let previous = std::env::var_os(key);
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
        f();
        match previous {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}
