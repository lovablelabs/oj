// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! One lockfile digest for every cache key that invalidates on "the installed
//! dependency tree changed". The multi-MB lockfiles used to be read and hashed
//! independently by the optimizer key, the preseed stamp, the config-extract
//! epoch, the start-bundle epoch, and the codegen salt, several times per boot
//! and per save; they now share this per-file memoized digest.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, SystemTime};

/// Every lockfile any oj cache key feeds on: the package managers' top-level
/// lockfiles plus the node_modules-internal mirrors Vite's optimizer reads
/// (its lockfileFormats). One canonical list, the union of what the call
/// sites used to key on, so no store misses a manager another store covers.
pub const LOCKFILE_NAMES: &[&str] = &[
    "node_modules/.pnpm/lock.yaml",
    "node_modules/.package-lock.json",
    "node_modules/.yarn-state.yml",
    "node_modules/.yarn-integrity",
    ".pnp.cjs",
    ".pnp.js",
    ".rush/temp/shrinkwrap-deps.json",
    "aube-lock.yaml",
    "nub.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
    "deno.lock",
];

/// A lockfile whose mtime is this recent could be rewritten again within the
/// stat's granularity; it is hashed but never memoized, so every call re-reads
/// it until it settles (the memo stores' freshness-slack rule).
const FRESHNESS_SLACK: Duration = Duration::from_secs(2);

pub struct LockfileDigest {
    pub digest: blake3::Hash,
    /// The nearest ancestor of the queried root holding any lockfile
    /// (Vite's lookupFile walks up from root), when one was found.
    pub dir: Option<PathBuf>,
    /// Which of `LOCKFILE_NAMES` were found there, in canonical order.
    pub found: Vec<&'static str>,
}

static MEMO: LazyLock<Mutex<HashMap<PathBuf, (u64, SystemTime, blake3::Hash)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The digest of one lockfile, from the process-wide memo when its size and
/// mtime are unchanged. None when the file is absent or unreadable.
fn file_digest(path: &Path) -> Option<blake3::Hash> {
    let meta = fs::metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let len = meta.len();
    let mtime = meta.modified().ok()?;
    {
        let memo = MEMO.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((known_len, known_mtime, digest)) = memo.get(path) {
            if *known_len == len && *known_mtime == mtime {
                return Some(*digest);
            }
        }
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update_reader(fs::File::open(path).ok()?).ok()?;
    let digest = hasher.finalize();
    let settled = SystemTime::now()
        .duration_since(mtime)
        .is_ok_and(|age| age >= FRESHNESS_SLACK);
    if settled {
        MEMO.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(path.to_path_buf(), (len, mtime, digest));
    }
    Some(digest)
}

/// Digest of every canonical lockfile in the nearest ancestor directory of
/// `root` that has one. Deterministic in the file set and contents; a changed
/// file re-hashes, unchanged ones come from the memo without a read.
pub fn lockfile_digest(root: &Path) -> LockfileDigest {
    let mut dir = Some(root);
    while let Some(d) = dir {
        let mut hasher = blake3::Hasher::new();
        let mut found = Vec::new();
        for name in LOCKFILE_NAMES {
            if let Some(digest) = file_digest(&d.join(name)) {
                found.push(*name);
                hasher.update(name.as_bytes());
                hasher.update(&[0]);
                hasher.update(digest.as_bytes());
                hasher.update(&[0]);
            }
        }
        if !found.is_empty() {
            return LockfileDigest {
                digest: hasher.finalize(),
                dir: Some(d.to_path_buf()),
                found,
            };
        }
        dir = d.parent();
    }
    LockfileDigest {
        digest: blake3::Hasher::new().finalize(),
        dir: None,
        found: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "oj-lockfiles-test-{}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn settle(path: &Path) {
        let old = SystemTime::now() - Duration::from_secs(3600);
        let f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(old)).unwrap();
    }

    fn set_mtime(path: &Path, t: SystemTime) {
        let f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(t)).unwrap();
    }

    #[test]
    fn digest_is_stable_across_calls() {
        let root = temp_root("stable");
        fs::write(root.join("package-lock.json"), b"{\"v\":1}").unwrap();
        fs::write(root.join("bun.lock"), b"{}").unwrap();
        let a = lockfile_digest(&root);
        let b = lockfile_digest(&root);
        assert_eq!(a.digest, b.digest);
        assert_eq!(a.dir.as_deref(), Some(root.as_path()));
        assert_eq!(a.found, vec!["package-lock.json", "bun.lock"]);
    }

    #[test]
    fn every_canonical_lockfile_feeds_the_digest() {
        let root = temp_root("names");
        fs::write(root.join("package-lock.json"), b"{}").unwrap();
        let base = lockfile_digest(&root).digest;
        for name in ["bun.lock", "deno.lock", "aube-lock.yaml", "nub.lock"] {
            fs::write(root.join(name), b"{}").unwrap();
            assert_ne!(lockfile_digest(&root).digest, base, "{name} must be keyed");
            fs::remove_file(root.join(name)).unwrap();
        }
        assert_eq!(lockfile_digest(&root).digest, base);
    }

    #[test]
    fn changed_content_and_mtime_rehash() {
        let root = temp_root("change");
        let lock = root.join("pnpm-lock.yaml");
        fs::write(&lock, b"lockfileVersion: 9").unwrap();
        settle(&lock);
        let a = lockfile_digest(&root).digest;
        assert_eq!(a, lockfile_digest(&root).digest, "memoized digest serves");
        fs::write(&lock, b"lockfileVersion: 10").unwrap();
        settle(&lock);
        // Same length, different content: the memo also keys on mtime, and a
        // settled rewrite always moves it.
        set_mtime(&lock, SystemTime::now() - Duration::from_secs(1800));
        assert_ne!(lockfile_digest(&root).digest, a);
    }

    #[cfg(unix)]
    #[test]
    fn memo_serves_settled_files_without_reading_them() {
        use std::os::unix::fs::PermissionsExt;
        let root = temp_root("noread");
        let lock = root.join("yarn.lock");
        fs::write(&lock, b"# yarn v1").unwrap();
        settle(&lock);
        let a = lockfile_digest(&root).digest;
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o000)).unwrap();
        // Unreadable content: only a memo hit (stat alone) can produce the
        // same digest again.
        assert_eq!(lockfile_digest(&root).digest, a);
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn fresh_files_are_served_but_not_memoized() {
        let root = temp_root("fresh");
        let lock = root.join("bun.lock");
        fs::write(&lock, b"{\"v\":1}").unwrap();
        let a = lockfile_digest(&root).digest;
        let orig_mtime = fs::metadata(&lock).unwrap().modified().unwrap();
        // Same length, same mtime, different content, inside the freshness
        // window: only a re-read can see it, and the memo must not hide it.
        fs::write(&lock, b"{\"v\":2}").unwrap();
        set_mtime(&lock, orig_mtime);
        assert_ne!(lockfile_digest(&root).digest, a);
    }

    #[test]
    fn lookup_walks_up_to_the_workspace_root() {
        let ws = temp_root("walkup");
        let app = ws.join("packages/app");
        fs::create_dir_all(&app).unwrap();
        fs::write(ws.join("pnpm-lock.yaml"), b"lockfileVersion: 9").unwrap();
        let d = lockfile_digest(&app);
        assert_eq!(d.dir.as_deref(), Some(ws.as_path()));
        assert_eq!(d.found, vec!["pnpm-lock.yaml"]);
        // A lockfile in the app itself shadows the workspace one.
        fs::write(app.join("package-lock.json"), b"{}").unwrap();
        let shadowed = lockfile_digest(&app);
        assert_eq!(shadowed.dir.as_deref(), Some(app.as_path()));
        assert_ne!(shadowed.digest, d.digest);
    }

    #[test]
    fn no_lockfile_anywhere_is_a_stable_empty_digest() {
        let root = temp_root("bare");
        let a = lockfile_digest(&root);
        assert!(a.dir.is_none());
        assert!(a.found.is_empty());
        assert_eq!(a.digest, lockfile_digest(&root).digest);
    }
}
