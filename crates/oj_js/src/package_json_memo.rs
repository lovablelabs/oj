// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The engines' package.json memo. Node parses each package.json once per
//! process; without a memo every resolve re-read and re-parsed the closest
//! one. A hit still checks the file's mtime and size, so an edit made during
//! a long dev session is picked up the way Vite's package cache drops an
//! entry when its package.json changes.

use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use deno_package_json::PackageJsonCache;
use deno_package_json::PackageJsonCacheResult;
use deno_package_json::PackageJsonRc;

/// What a file looked like when it was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

impl Stamp {
    fn of(path: &Path) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
        })
    }
}

#[derive(Debug, Default)]
pub(crate) struct PackageJsonMemo {
    parsed: Mutex<HashMap<PathBuf, (Stamp, PackageJsonRc)>>,
    /// Stamps taken by a `get` that missed, for the `set` that follows its
    /// read: taken before the read, so a write racing it only ever makes the
    /// next lookup re-read.
    probed: Mutex<HashMap<PathBuf, Stamp>>,
}

impl PackageJsonCache for PackageJsonMemo {
    fn get(&self, path: &Path) -> PackageJsonCacheResult {
        let stamp = match Stamp::of(path) {
            Ok(stamp) => stamp,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return PackageJsonCacheResult::Hit(None);
            }
            Err(_) => return PackageJsonCacheResult::NotCached,
        };
        let parsed = self.parsed.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((seen, pkg)) = parsed.get(path) {
            if *seen == stamp {
                return PackageJsonCacheResult::Hit(Some(pkg.clone()));
            }
        }
        drop(parsed);
        self.probed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(path.to_path_buf(), stamp);
        PackageJsonCacheResult::NotCached
    }

    fn set(&self, path: PathBuf, package_json: Option<PackageJsonRc>) {
        let stamp = self
            .probed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&path);
        let mut parsed = self.parsed.lock().unwrap_or_else(|e| e.into_inner());
        match (stamp, package_json) {
            (Some(stamp), Some(pkg)) => {
                parsed.insert(path, (stamp, pkg));
            }
            _ => {
                parsed.remove(&path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn load(memo: &PackageJsonMemo, path: &Path) -> Option<PackageJsonRc> {
        deno_package_json::PackageJson::load_from_path(
            &sys_traits::impls::RealSys,
            Some(memo),
            path,
        )
        .unwrap()
    }

    #[test]
    fn an_unchanged_file_is_parsed_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(&path, r#"{"name":"a","main":"a.js"}"#).unwrap();
        let memo = PackageJsonMemo::default();
        let first = load(&memo, &path).unwrap();
        let second = load(&memo, &path).unwrap();
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "the second load re-parsed"
        );
    }

    // A long-lived host must see a package.json edited during the session.
    #[test]
    fn an_edited_file_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        std::fs::write(&path, r#"{"name":"a","main":"a.js"}"#).unwrap();
        let memo = PackageJsonMemo::default();
        assert_eq!(load(&memo, &path).unwrap().main.as_deref(), Some("a.js"));
        std::fs::write(&path, r#"{"name":"a","main":"lib/b.js"}"#).unwrap();
        assert_eq!(
            load(&memo, &path).unwrap().main.as_deref(),
            Some("lib/b.js")
        );
    }

    #[test]
    fn a_missing_file_is_none_and_a_created_one_is_found() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        let memo = PackageJsonMemo::default();
        assert!(load(&memo, &path).is_none());
        std::fs::write(&path, r#"{"name":"late"}"#).unwrap();
        assert_eq!(load(&memo, &path).unwrap().name.as_deref(), Some("late"));
    }
}
