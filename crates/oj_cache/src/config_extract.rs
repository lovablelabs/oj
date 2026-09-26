// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::integrity;

pub struct ConfigExtractStore {
    dir: PathBuf,
    root: PathBuf,
    salt: String,
}

pub struct CachedExtract {
    pub output: String,
    pub stderr: String,
    /// The config file and everything it imported, as stamped for invalidation.
    pub deps: Vec<PathBuf>,
}

#[derive(Serialize, Deserialize)]
struct Entry {
    env_epoch: String,
    files: Vec<FileStamp>,
    output: String,
    stderr: String,
}

#[derive(Serialize, Deserialize)]
struct FileStamp {
    path: String,
    digest: String,
}

const ABSENT: &str = "-";

impl ConfigExtractStore {
    pub fn new(root: &Path, salt: &str) -> Self {
        Self {
            dir: crate::cache_root(root).join("config-extract"),
            root: root.to_path_buf(),
            salt: salt.to_string(),
        }
    }

    pub fn lookup(&self, config: &Path, command: &str, mode: &str) -> Option<CachedExtract> {
        let path = self.entry_path(config, command, mode);
        if !path.exists() {
            return None;
        }
        let payload = match integrity::read_self_verified(&path) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "oj: cache integrity: {{\"store\":\"config-extract\",\"entry\":{:?},\"error\":{:?}}}",
                    path.file_name().unwrap_or_default(),
                    e.to_string()
                );
                let _ = fs::remove_file(&path);
                return None;
            }
        };
        let entry: Entry = match serde_json::from_slice(&payload) {
            Ok(e) => e,
            Err(_) => {
                let _ = fs::remove_file(&path);
                return None;
            }
        };
        if entry.env_epoch != env_epoch(&self.root) {
            return None;
        }
        for f in &entry.files {
            if file_digest(Path::new(&f.path)) != f.digest {
                return None;
            }
        }
        Some(CachedExtract {
            deps: entry.files.iter().map(|f| PathBuf::from(&f.path)).collect(),
            output: entry.output,
            stderr: entry.stderr,
        })
    }

    pub fn store(
        &self,
        config: &Path,
        command: &str,
        mode: &str,
        deps: &[PathBuf],
        output: &str,
        stderr: &str,
    ) {
        if fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let mut files: Vec<PathBuf> = Vec::with_capacity(deps.len() + 1);
        files.push(config.to_path_buf());
        files.extend(deps.iter().cloned());
        files.sort();
        files.dedup();
        let files = files
            .into_iter()
            .map(|p| FileStamp {
                digest: file_digest(&p),
                path: p.display().to_string(),
            })
            .collect();
        let entry = Entry {
            env_epoch: env_epoch(&self.root),
            files,
            output: output.to_string(),
            stderr: stderr.to_string(),
        };
        let path = self.entry_path(config, command, mode);
        let _ =
            integrity::write_self_verified(&path, &serde_json::to_vec(&entry).unwrap_or_default());
    }

    fn entry_path(&self, config: &Path, command: &str, mode: &str) -> PathBuf {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.salt.as_bytes());
        hasher.update(&[0]);
        hasher.update(config.as_os_str().as_encoded_bytes());
        hasher.update(&[0]);
        hasher.update(command.as_bytes());
        hasher.update(&[0]);
        hasher.update(mode.as_bytes());
        self.dir
            .join(format!("{}.json", hasher.finalize().to_hex()))
    }
}

fn file_digest(path: &Path) -> String {
    match fs::read(path) {
        Ok(bytes) => blake3::hash(&bytes).to_hex().to_string(),
        Err(_) => ABSENT.to_string(),
    }
}

fn env_epoch(root: &Path) -> String {
    let mut hasher = blake3::Hasher::new();
    for name in [
        "package-lock.json",
        "yarn.lock",
        "pnpm-lock.yaml",
        "bun.lockb",
        "package.json",
        ".env",
        ".env.local",
        ".env.development",
        ".env.development.local",
        // Files a plugin's `config` hook reads outside the config-import graph:
        // @cloudflare/vite-plugin resolves the Worker config from
        // wrangler.jsonc/.json/.toml (wrangler.unstable_readConfig) while the
        // hooks run, so its verdict — runner-backed detection included — can
        // flip on a wrangler edit with no vite.config change. Absence is a
        // state too: a present file feeds name+content, an absent one feeds
        // nothing, so create/delete transitions change the epoch. (.dev.vars
        // is NOT here: the plugin reads it at preview/build-output time, not
        // in its config hook.)
        "wrangler.jsonc",
        "wrangler.json",
        "wrangler.toml",
    ] {
        if let Ok(bytes) = fs::read(root.join(name)) {
            hasher.update(name.as_bytes());
            hasher.update(&[0]);
            hasher.update(&bytes);
        }
    }
    let mut env: Vec<(String, String)> = std::env::vars()
        .filter(|(k, _)| {
            ["VITE_", "LOVABLE_", "LOVBOX_", "TSS_"]
                .iter()
                .any(|p| k.starts_with(p))
                || matches!(k.as_str(), "NODE_ENV" | "CI" | "ANALYZE")
        })
        .collect();
    env.sort();
    for (k, v) in env {
        hasher.update(b"\0e");
        hasher.update(k.as_bytes());
        hasher.update(&[0]);
        hasher.update(v.as_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oj-config-extract-test-{}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn roundtrip_hits_until_a_dep_changes() {
        let root = temp_root("roundtrip");
        let config = root.join("vite.config.ts");
        let dep = root.join("plugin.ts");
        fs::write(&config, "import './plugin'").unwrap();
        fs::write(&dep, "export const x = 1").unwrap();
        let store = ConfigExtractStore::new(&root, "s1");
        assert!(store.lookup(&config, "serve", "development").is_none());
        store.store(
            &config,
            "serve",
            "development",
            std::slice::from_ref(&dep),
            r#"{"base":"/"}"#,
            "warn\n",
        );
        let hit = store.lookup(&config, "serve", "development").unwrap();
        assert_eq!(hit.output, r#"{"base":"/"}"#);
        assert_eq!(hit.stderr, "warn\n");
        fs::write(&dep, "export const x = 2").unwrap();
        assert!(store.lookup(&config, "serve", "development").is_none());
    }

    #[test]
    fn config_edit_invalidates() {
        let root = temp_root("config-edit");
        let config = root.join("vite.config.ts");
        fs::write(&config, "a").unwrap();
        let store = ConfigExtractStore::new(&root, "s1");
        store.store(&config, "serve", "development", &[], "{}", "");
        assert!(store.lookup(&config, "serve", "development").is_some());
        fs::write(&config, "b").unwrap();
        assert!(store.lookup(&config, "serve", "development").is_none());
    }

    #[test]
    fn lockfile_and_env_file_changes_invalidate() {
        let root = temp_root("epoch");
        let config = root.join("vite.config.ts");
        fs::write(&config, "a").unwrap();
        let store = ConfigExtractStore::new(&root, "s1");
        store.store(&config, "serve", "development", &[], "{}", "");
        assert!(store.lookup(&config, "serve", "development").is_some());
        fs::write(root.join("pnpm-lock.yaml"), "lock").unwrap();
        assert!(store.lookup(&config, "serve", "development").is_none());
        store.store(&config, "serve", "development", &[], "{}", "");
        assert!(store.lookup(&config, "serve", "development").is_some());
        fs::write(root.join(".env.local"), "VITE_X=1").unwrap();
        assert!(store.lookup(&config, "serve", "development").is_none());
    }

    // A wrangler config is read by the Cloudflare plugin's config hook, not
    // imported by vite.config: without it in the key, a verdict computed under
    // a broken wrangler.jsonc survived the wrangler fix (the worker path
    // stayed off until vite.config itself was touched). Every transition —
    // create, edit, delete — must be a miss.
    #[test]
    fn wrangler_config_changes_invalidate() {
        let root = temp_root("wrangler");
        let config = root.join("vite.config.ts");
        fs::write(&config, "a").unwrap();
        let store = ConfigExtractStore::new(&root, "s1");
        store.store(&config, "serve", "development", &[], "{}", "");
        assert!(store.lookup(&config, "serve", "development").is_some());
        // Absent -> present.
        fs::write(root.join("wrangler.jsonc"), r#"{ "name": "app" }"#).unwrap();
        assert!(store.lookup(&config, "serve", "development").is_none());
        store.store(&config, "serve", "development", &[], "{}", "");
        assert!(store.lookup(&config, "serve", "development").is_some());
        // Content change (the broken-wrangler-then-fixed shape).
        fs::write(
            root.join("wrangler.jsonc"),
            r#"{ "name": "app", "main": "w.ts" }"#,
        )
        .unwrap();
        assert!(store.lookup(&config, "serve", "development").is_none());
        store.store(&config, "serve", "development", &[], "{}", "");
        // Present -> absent.
        fs::remove_file(root.join("wrangler.jsonc")).unwrap();
        assert!(store.lookup(&config, "serve", "development").is_none());
        store.store(&config, "serve", "development", &[], "{}", "");
        // The .toml spelling counts too.
        fs::write(root.join("wrangler.toml"), "name = \"app\"").unwrap();
        assert!(store.lookup(&config, "serve", "development").is_none());
    }

    #[test]
    fn dep_deleted_invalidates() {
        let root = temp_root("dep-del");
        let config = root.join("vite.config.ts");
        let dep = root.join("plugin.ts");
        fs::write(&config, "a").unwrap();
        fs::write(&dep, "b").unwrap();
        let store = ConfigExtractStore::new(&root, "s1");
        store.store(
            &config,
            "serve",
            "development",
            std::slice::from_ref(&dep),
            "{}",
            "",
        );
        assert!(store.lookup(&config, "serve", "development").is_some());
        fs::remove_file(&dep).unwrap();
        assert!(store.lookup(&config, "serve", "development").is_none());
    }

    #[test]
    fn corrupt_entry_is_dropped_not_served() {
        let root = temp_root("corrupt");
        let config = root.join("vite.config.ts");
        fs::write(&config, "a").unwrap();
        let store = ConfigExtractStore::new(&root, "s1");
        store.store(&config, "serve", "development", &[], "{}", "");
        let path = store.entry_path(&config, "serve", "development");
        fs::write(&path, b"{ not json").unwrap();
        assert!(store.lookup(&config, "serve", "development").is_none());
        assert!(!path.exists(), "corrupt entry must be removed");
    }

    #[test]
    fn salt_and_command_and_mode_separate_entries() {
        let root = temp_root("salt");
        let config = root.join("vite.config.ts");
        fs::write(&config, "a").unwrap();
        let store = ConfigExtractStore::new(&root, "s1");
        store.store(&config, "serve", "development", &[], "{}", "");
        assert!(ConfigExtractStore::new(&root, "s2")
            .lookup(&config, "serve", "development")
            .is_none());
        assert!(store.lookup(&config, "build", "development").is_none());
        assert!(store.lookup(&config, "serve", "production").is_none());
    }

    #[test]
    fn flipped_byte_is_a_miss_and_recomputes() {
        let root = temp_root("flip");
        let config = root.join("vite.config.ts");
        fs::write(&config, "a").unwrap();
        let store = ConfigExtractStore::new(&root, "s1");
        store.store(&config, "serve", "development", &[], r#"{"base":"/x"}"#, "");
        let path = store.entry_path(&config, "serve", "development");

        // Same-size, single-byte flip inside the payload.
        let mut bytes = fs::read(&path).unwrap();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        fs::write(&path, &bytes).unwrap();

        assert!(
            store.lookup(&config, "serve", "development").is_none(),
            "flipped byte must be a miss, never wrong output"
        );
        assert!(!path.exists(), "corrupt entry must be removed");

        store.store(&config, "serve", "development", &[], r#"{"base":"/x"}"#, "");
        let hit = store.lookup(&config, "serve", "development").unwrap();
        assert_eq!(hit.output, r#"{"base":"/x"}"#);
    }
}
