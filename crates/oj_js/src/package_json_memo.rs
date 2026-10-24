// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! The engines' package.json memo. Node parses each package.json once per
//! process; without a memo every resolve re-read and re-parsed the closest
//! one. A hit still checks the file's stamp, so an edit made during a long
//! dev session is picked up the way Vite's package cache drops an entry when
//! its package.json changes.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::SystemTime;

use deno_package_json::PackageJsonCache;
use deno_package_json::PackageJsonCacheResult;
use deno_package_json::PackageJsonRc;

/// What a file looked like when it was read. The mtime alone is not enough:
/// npm extracts every file with the same fixed mtime, so a same-length
/// package.json from an upgrade would match. The change time and inode move
/// on any write or replace and cannot be set back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
    #[cfg(unix)]
    changed: (i64, i64),
    #[cfg(unix)]
    inode: (u64, u64),
}

impl Stamp {
    fn of(path: &Path) -> std::io::Result<Self> {
        let meta = std::fs::metadata(path)?;
        #[cfg(unix)]
        use std::os::unix::fs::MetadataExt;
        Ok(Self {
            modified: meta.modified().ok(),
            len: meta.len(),
            #[cfg(unix)]
            changed: (meta.ctime(), meta.ctime_nsec()),
            #[cfg(unix)]
            inode: (meta.dev(), meta.ino()),
        })
    }
}

thread_local! {
    /// The stamp a `get` that missed took, for the `set` that follows its
    /// read. `load_from_path` runs both on one thread, so a concurrent load
    /// of the same file can never hand this one its stamp. Taken before the
    /// read, so a write racing it only ever makes the next lookup re-read.
    static PROBED: RefCell<HashMap<PathBuf, Stamp>> = RefCell::new(HashMap::new());
}

#[derive(Debug, Default)]
pub(crate) struct PackageJsonMemo {
    parsed: Mutex<HashMap<PathBuf, (Stamp, PackageJsonRc)>>,
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
        PROBED.with_borrow_mut(|probed| probed.insert(path.to_path_buf(), stamp));
        PackageJsonCacheResult::NotCached
    }

    fn set(&self, path: PathBuf, package_json: Option<PackageJsonRc>) {
        let stamp = PROBED.with_borrow_mut(|probed| probed.remove(&path));
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

    // npm extracts files with one fixed mtime: an upgraded dependency's
    // package.json can keep both its mtime and its length.
    #[test]
    fn a_rewrite_keeping_mtime_and_length_is_read_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("package.json");
        let fixed = std::time::UNIX_EPOCH + std::time::Duration::from_secs(499_162_500);
        let write = |text: &str| {
            std::fs::write(&path, text).unwrap();
            std::fs::File::options()
                .append(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(fixed))
                .unwrap();
        };
        let memo = PackageJsonMemo::default();
        write(r#"{"name":"dep","version":"1.2.3","main":"a.js"}"#);
        assert_eq!(load(&memo, &path).unwrap().main.as_deref(), Some("a.js"));
        write(r#"{"name":"dep","version":"1.2.4","main":"b.js"}"#);
        assert_eq!(load(&memo, &path).unwrap().main.as_deref(), Some("b.js"));
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
