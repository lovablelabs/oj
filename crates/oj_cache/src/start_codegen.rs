// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::integrity::{self, ExpectedFile, VerifyMode};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

pub const START_CODEGEN_FORMAT: u32 = 2;
pub const DEFAULT_KEEP_ENTRIES: usize = 8;

const MEMO_FILE: &str = "memo.json";
const TOUCH_FILE: &str = "last-used";
const FILES_FILE: &str = ".files.json";

const MEMO_FRESHNESS_SLACK_NS: u64 = 2_000_000_000;

pub struct StartCodegenStore {
    root: PathBuf,
    dir: PathBuf,
    salt: String,
    marker: Option<String>,
}

#[derive(Debug, PartialEq)]
pub struct RestoreStats {
    pub key: String,
    pub keyed_files: usize,
    pub rehashed: usize,
    pub elapsed_ms: u128,
}

#[derive(Debug, PartialEq)]
pub enum Miss {
    InputUnreadable(PathBuf),
    NoEntryForKey(String),
    EntryCorrupt(String),
}

impl std::fmt::Display for Miss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        fn short(key: &str) -> &str {
            key.get(..8).unwrap_or(key)
        }
        match self {
            Miss::InputUnreadable(p) => write!(f, "input unreadable: {}", p.display()),
            Miss::NoEntryForKey(k) => write!(f, "no cached output for key {}…", short(k)),
            Miss::EntryCorrupt(k) => write!(f, "cached entry {}… corrupt, removed", short(k)),
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
    marked: bool,
}

#[derive(Clone)]
struct FileInfo {
    digest: blake3::Hash,
    marked: bool,
}

impl StartCodegenStore {
    pub fn new(
        root: &Path,
        kind: &str,
        tool_version: &str,
        extra_salt: &[u8],
        marker: Option<&str>,
    ) -> Self {
        Self {
            root: root.to_path_buf(),
            dir: crate::cache_root(root).join("start-codegen").join(kind),
            salt: format!(
                "{tool_version}:{START_CODEGEN_FORMAT}:{kind}:{}",
                blake3::hash(extra_salt).to_hex()
            ),
            marker: marker.map(str::to_owned),
        }
    }

    pub fn restore(
        &self,
        inputs: &[PathBuf],
        outputs: &[(&str, &Path)],
    ) -> Result<RestoreStats, Miss> {
        let started = Instant::now();
        let inputs = sorted(inputs);
        let memo = self.read_memo();
        let (infos, rehashed) = self.verified_infos(&inputs, memo.as_ref());
        let (key, keyed_files) = self.key_from(&inputs, &infos)?;
        let entry = self.dir.join(&key);
        if !entry.is_dir() {
            return Err(Miss::NoEntryForKey(key));
        }
        let Some(manifest) = read_entry_manifest(&entry) else {
            let _ = fs::remove_dir_all(&entry);
            return Err(Miss::EntryCorrupt(key));
        };
        let mut restored: Vec<(&Path, Vec<u8>)> = Vec::with_capacity(outputs.len());
        for (name, dest) in outputs {
            let bytes = manifest
                .get(*name)
                .ok_or(())
                .and_then(|exp| {
                    integrity::verified_read(&entry.join(name), exp, VerifyMode::Full)
                        .map_err(|e| {
                            eprintln!(
                                "oj: cache integrity: {{\"store\":\"start-codegen\",\"entry\":\"{}\",\"file\":{name:?},\"error\":{:?}}}",
                                key.get(..8).unwrap_or(&key),
                                e.to_string()
                            );
                        })
                });
            match bytes {
                Ok(bytes) => restored.push((dest, bytes)),
                Err(()) => {
                    let _ = fs::remove_dir_all(&entry);
                    return Err(Miss::EntryCorrupt(key));
                }
            }
        }
        for (dest, bytes) in restored {
            if write_atomic_if_changed(dest, &bytes).is_err() {
                return Err(Miss::EntryCorrupt(key));
            }
        }
        if rehashed > 0 || !self.dir.join(MEMO_FILE).is_file() {
            self.write_memo(&build_memo(&self.root, &inputs, &infos));
        }
        touch(&entry);
        Ok(RestoreStats {
            key,
            keyed_files,
            rehashed,
            elapsed_ms: started.elapsed().as_millis(),
        })
    }

    pub fn persist(&self, inputs: &[PathBuf], outputs: &[(&str, &Path)]) -> Option<String> {
        let inputs = sorted(inputs);
        let memo = self.read_memo();
        let (infos, _) = self.verified_infos(&inputs, memo.as_ref());
        let (key, _) = self.key_from(&inputs, &infos).ok()?;
        let entry = self.dir.join(&key);
        if !entry.is_dir() {
            let tmp = self
                .dir
                .join(format!(".tmp-{}-{}", key.get(..16)?, std::process::id()));
            let _ = fs::remove_dir_all(&tmp);
            fs::create_dir_all(&tmp).ok()?;
            let mut manifest: HashMap<String, ExpectedFile> = HashMap::new();
            for (name, src) in outputs {
                let Ok(bytes) = fs::read(src) else {
                    let _ = fs::remove_dir_all(&tmp);
                    return None;
                };
                if fs::write(tmp.join(name), &bytes).is_err() {
                    let _ = fs::remove_dir_all(&tmp);
                    return None;
                }
                manifest.insert(
                    (*name).to_string(),
                    ExpectedFile {
                        size: bytes.len() as u64,
                        hash: blake3::hash(&bytes).to_hex().to_string(),
                    },
                );
            }
            if write_entry_manifest(&tmp, &manifest).is_err() {
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
        self.write_memo(&build_memo(&self.root, &inputs, &infos));
        touch(&entry);
        self.prune(DEFAULT_KEEP_ENTRIES);
        Some(key)
    }

    pub fn prune(&self, keep: usize) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        let mut stamped = Vec::new();
        for e in entries.flatten() {
            let path = e.path();
            if !path.is_dir() {
                continue;
            }
            if e.file_name().to_string_lossy().starts_with(".tmp-") {
                let _ = fs::remove_dir_all(&path);
                continue;
            }
            let stamp = fs::metadata(path.join(TOUCH_FILE))
                .or_else(|_| fs::metadata(&path))
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            stamped.push((path, stamp));
        }
        stamped.sort_by_key(|&(_, stamp)| std::cmp::Reverse(stamp));
        for (path, _) in stamped.into_iter().skip(keep) {
            let _ = fs::remove_dir_all(&path);
        }
    }

    fn key_from(
        &self,
        inputs: &[PathBuf],
        infos: &[Option<FileInfo>],
    ) -> Result<(String, usize), Miss> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(self.salt.as_bytes());
        let mut keyed = 0usize;
        for (path, info) in inputs.iter().zip(infos) {
            let Some(info) = info else {
                return Err(Miss::InputUnreadable(path.clone()));
            };
            if !info.marked {
                continue;
            }
            keyed += 1;
            hasher.update(&[0]);
            hasher.update(rel_key(&self.root, path).as_bytes());
            hasher.update(&[0]);
            hasher.update(info.digest.as_bytes());
        }
        Ok((hasher.finalize().to_hex().to_string(), keyed))
    }

    fn verified_infos(
        &self,
        inputs: &[PathBuf],
        memo: Option<&Memo>,
    ) -> (Vec<Option<FileInfo>>, usize) {
        let rehashed = AtomicUsize::new(0);
        let infos = par_map(inputs, |p| {
            let rel = rel_key(&self.root, p);
            if let Some(known) = memo.and_then(|m| m.files.get(&rel)) {
                if let Ok(meta) = fs::metadata(p) {
                    if meta.len() == known.size && mtime_ns(&meta) == Some(known.mtime_ns) {
                        if let Ok(digest) = blake3::Hash::from_hex(&known.digest) {
                            return Some(FileInfo {
                                digest,
                                marked: known.marked,
                            });
                        }
                    }
                }
            }
            rehashed.fetch_add(1, Ordering::Relaxed);
            let bytes = fs::read(p).ok()?;
            Some(FileInfo {
                digest: blake3::hash(&bytes),
                marked: self
                    .marker
                    .as_ref()
                    .is_none_or(|m| contains(&bytes, m.as_bytes())),
            })
        });
        (infos, rehashed.into_inner())
    }

    fn read_memo(&self) -> Option<Memo> {
        let path = self.dir.join(MEMO_FILE);
        let bytes = fs::read(&path).ok()?;
        match serde_json::from_slice(&bytes) {
            Ok(memo) => Some(memo),
            Err(_) => {
                let _ = fs::remove_file(&path);
                None
            }
        }
    }

    fn write_memo(&self, memo: &Memo) {
        let Ok(bytes) = serde_json::to_vec(memo) else {
            return;
        };
        if fs::create_dir_all(&self.dir).is_err() {
            return;
        }
        let _ = integrity::atomic_write(&self.dir.join(MEMO_FILE), &bytes);
    }
}

fn sorted(inputs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = inputs.to_vec();
    out.sort();
    out.dedup();
    out
}

fn rel_key(root: &Path, path: &Path) -> String {
    path.strip_prefix(root)
        .unwrap_or(path)
        .to_string_lossy()
        .into_owned()
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && hay.windows(needle.len()).any(|w| w == needle)
}

fn write_atomic_if_changed(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if fs::read(dest).is_ok_and(|cur| cur == bytes) {
        return Ok(());
    }
    if let Some(dir) = dest.parent() {
        fs::create_dir_all(dir)?;
    }
    integrity::atomic_write(dest, bytes)
}

fn read_entry_manifest(entry: &Path) -> Option<HashMap<String, ExpectedFile>> {
    #[derive(Deserialize)]
    struct Rec {
        size: u64,
        hash: String,
    }
    let bytes = integrity::read_self_verified(&entry.join(FILES_FILE)).ok()?;
    let raw: HashMap<String, Rec> = serde_json::from_slice(&bytes).ok()?;
    Some(
        raw.into_iter()
            .map(|(k, r)| {
                (
                    k,
                    ExpectedFile {
                        size: r.size,
                        hash: r.hash,
                    },
                )
            })
            .collect(),
    )
}

fn write_entry_manifest(
    entry: &Path,
    manifest: &HashMap<String, ExpectedFile>,
) -> std::io::Result<()> {
    #[derive(Serialize)]
    struct Rec<'a> {
        size: u64,
        hash: &'a str,
    }
    let raw: HashMap<&str, Rec> = manifest
        .iter()
        .map(|(k, e)| {
            (
                k.as_str(),
                Rec {
                    size: e.size,
                    hash: &e.hash,
                },
            )
        })
        .collect();
    integrity::write_self_verified(
        &entry.join(FILES_FILE),
        &serde_json::to_vec(&raw).unwrap_or_default(),
    )
}

fn build_memo(root: &Path, inputs: &[PathBuf], infos: &[Option<FileInfo>]) -> Memo {
    let written_at_ns = now_ns();
    let stats = par_map(inputs, |p| {
        let meta = fs::metadata(p).ok()?;
        Some((meta.len(), mtime_ns(&meta)?))
    });
    let mut map = HashMap::new();
    for ((path, info), stat) in inputs.iter().zip(infos).zip(&stats) {
        let (Some(info), Some((size, mtime_ns))) = (info, stat) else {
            continue;
        };
        if mtime_ns.saturating_add(MEMO_FRESHNESS_SLACK_NS) > written_at_ns {
            continue;
        }
        map.insert(
            rel_key(root, path),
            MemoFile {
                size: *size,
                mtime_ns: *mtime_ns,
                digest: info.digest.to_hex().to_string(),
                marked: info.marked,
            },
        );
    }
    Memo {
        written_at_ns,
        files: map,
    }
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

fn par_map<T: Send>(files: &[PathBuf], f: impl Fn(&Path) -> Option<T> + Sync) -> Vec<Option<T>> {
    let threads = std::thread::available_parallelism()
        .map_or(1, |n| n.get())
        .min(files.len().max(1));
    if threads <= 1 {
        return files.iter().map(|p| f(p)).collect();
    }
    let chunk = files.len().div_ceil(threads);
    let mut out: Vec<Option<T>> = Vec::new();
    out.resize_with(files.len(), || None);
    std::thread::scope(|s| {
        for (paths, slots) in files.chunks(chunk).zip(out.chunks_mut(chunk)) {
            let f = &f;
            s.spawn(move || {
                for (p, slot) in paths.iter().zip(slots) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "oj-start-codegen-test-{}-{label}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(root.join("src/routes")).unwrap();
            let fx = Self { root };
            fx.write("src/routes/index.tsx", "export const Route = 1;");
            fx.write("src/routes/about.tsx", "export const Route = 2;");
            fx.write("src/routeTree.gen.ts", "// tree-v1");
            fx
        }

        fn store(&self) -> StartCodegenStore {
            StartCodegenStore::new(&self.root, "route-tree", "0.0.1-test", b"script-v1", None)
        }

        fn marked_store(&self) -> StartCodegenStore {
            StartCodegenStore::new(
                &self.root,
                "server-fn",
                "0.0.1-test",
                b"script-v1",
                Some("createServerFn"),
            )
        }

        fn path(&self, rel: &str) -> PathBuf {
            self.root.join(rel)
        }

        fn write(&self, rel: &str, content: &str) {
            let p = self.path(rel);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, content).unwrap();
        }

        fn inputs(&self) -> Vec<PathBuf> {
            vec![
                self.path("src/routes/index.tsx"),
                self.path("src/routes/about.tsx"),
            ]
        }

        /// Backdate input mtimes past the memo freshness window so persist
        /// records them (freshly written files are deliberately left out).
        fn settle(&self, rels: &[&str]) {
            let old = SystemTime::now() - Duration::from_secs(3600);
            for rel in rels {
                set_mtime(&self.path(rel), old);
            }
        }

        fn entry_dir(&self, kind: &str, key: &str) -> PathBuf {
            crate::cache_root(&self.root)
                .join("start-codegen")
                .join(kind)
                .join(key)
        }
    }

    fn set_mtime(path: &Path, t: SystemTime) {
        let f = fs::OpenOptions::new().append(true).open(path).unwrap();
        f.set_times(fs::FileTimes::new().set_modified(t)).unwrap();
    }

    fn tree_outputs(fx: &Fixture) -> (PathBuf, PathBuf) {
        (
            fx.path("src/routeTree.gen.ts"),
            fx.path("src/routeTree.gen.ts"),
        )
    }

    #[test]
    fn restore_misses_before_first_persist() {
        let fx = Fixture::new("first");
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        assert!(matches!(
            fx.store().restore(&fx.inputs(), &outputs),
            Err(Miss::NoEntryForKey(_))
        ));
    }

    #[test]
    fn persist_then_restore_roundtrips_output() {
        let fx = Fixture::new("roundtrip");
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        let key = fx.store().persist(&fx.inputs(), &outputs).unwrap();
        fs::remove_file(&out).unwrap();
        let stats = fx.store().restore(&fx.inputs(), &outputs).unwrap();
        assert_eq!(stats.key, key);
        assert_eq!(stats.keyed_files, 2);
        assert_eq!(fs::read_to_string(&out).unwrap(), "// tree-v1");
    }

    #[test]
    fn input_edit_changes_key_and_misses_until_repersisted() {
        let fx = Fixture::new("edit");
        fx.settle(&["src/routes/index.tsx", "src/routes/about.tsx"]);
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        let key = fx.store().persist(&fx.inputs(), &outputs).unwrap();
        fx.write("src/routes/index.tsx", "export const Route = 99;");
        match fx.store().restore(&fx.inputs(), &outputs) {
            Err(Miss::NoEntryForKey(k)) => assert_ne!(k, key),
            other => panic!("expected key miss, got {other:?}"),
        }
        fx.write("src/routeTree.gen.ts", "// tree-v2");
        let key2 = fx.store().persist(&fx.inputs(), &outputs).unwrap();
        assert_ne!(key2, key);
        fs::remove_file(&out).unwrap();
        let stats = fx.store().restore(&fx.inputs(), &outputs).unwrap();
        assert_eq!(stats.key, key2);
        assert_eq!(fs::read_to_string(&out).unwrap(), "// tree-v2");
    }

    #[test]
    fn added_and_removed_inputs_change_the_key() {
        let fx = Fixture::new("addrm");
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        let key = fx.store().persist(&fx.inputs(), &outputs).unwrap();
        fx.write("src/routes/new.tsx", "export const Route = 3;");
        let mut more = fx.inputs();
        more.push(fx.path("src/routes/new.tsx"));
        match fx.store().restore(&more, &outputs) {
            Err(Miss::NoEntryForKey(k)) => assert_ne!(k, key),
            other => panic!("expected key miss, got {other:?}"),
        }
        let fewer = vec![fx.path("src/routes/index.tsx")];
        match fx.store().restore(&fewer, &outputs) {
            Err(Miss::NoEntryForKey(k)) => assert_ne!(k, key),
            other => panic!("expected key miss, got {other:?}"),
        }
    }

    #[test]
    fn marker_filters_unmarked_files_out_of_the_key() {
        let fx = Fixture::new("marker");
        fx.write("src/a.ts", "export const a = createServerFn().handler(x);");
        fx.write("src/b.ts", "export const b = 1;");
        fx.write("out.mjs", "// resolver-v1");
        let out = fx.path("out.mjs");
        let outputs = [("server-fn-resolver.mjs", out.as_path())];
        let inputs = vec![fx.path("src/a.ts"), fx.path("src/b.ts")];
        let key = fx.marked_store().persist(&inputs, &outputs).unwrap();

        fx.write("src/b.ts", "export const b = 2;");
        let stats = fx.marked_store().restore(&inputs, &outputs).unwrap();
        assert_eq!(stats.key, key, "unmarked edit must still hit");
        assert_eq!(stats.keyed_files, 1);

        fx.write("src/b.ts", "export const b = createServerFn().handler(y);");
        match fx.marked_store().restore(&inputs, &outputs) {
            Err(Miss::NoEntryForKey(k)) => assert_ne!(k, key),
            other => panic!("marker gained: expected miss, got {other:?}"),
        }

        fx.write("src/a.ts", "export const a = createServerFn().handler(z);");
        fx.write("src/b.ts", "export const b = 1;");
        assert!(matches!(
            fx.marked_store().restore(&inputs, &outputs),
            Err(Miss::NoEntryForKey(_))
        ));
    }

    #[test]
    fn salt_inputs_change_the_key() {
        let fx = Fixture::new("salt");
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        let key = fx.store().persist(&fx.inputs(), &outputs).unwrap();
        let other_script =
            StartCodegenStore::new(&fx.root, "route-tree", "0.0.1-test", b"script-v2", None)
                .persist(&fx.inputs(), &outputs)
                .unwrap();
        assert_ne!(other_script, key, "script content salts the key");
        let other_version =
            StartCodegenStore::new(&fx.root, "route-tree", "9.9.9", b"script-v1", None)
                .persist(&fx.inputs(), &outputs)
                .unwrap();
        assert_ne!(other_version, key, "tool version salts the key");
    }

    #[test]
    fn deleted_input_is_a_miss_not_a_panic() {
        let fx = Fixture::new("deleted");
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        fx.store().persist(&fx.inputs(), &outputs).unwrap();
        fs::remove_file(fx.path("src/routes/about.tsx")).unwrap();
        assert!(matches!(
            fx.store().restore(&fx.inputs(), &outputs),
            Err(Miss::InputUnreadable(_))
        ));
    }

    #[test]
    fn corrupt_entry_deletes_itself() {
        let fx = Fixture::new("corrupt");
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        let key = fx.store().persist(&fx.inputs(), &outputs).unwrap();
        let entry = fx.entry_dir("route-tree", &key);
        fs::remove_file(entry.join("routeTree.gen.ts")).unwrap();
        assert_eq!(
            fx.store().restore(&fx.inputs(), &outputs),
            Err(Miss::EntryCorrupt(key))
        );
        assert!(!entry.exists(), "corrupt entry must be removed");
    }

    #[cfg(unix)]
    #[test]
    fn memo_verifies_settled_files_without_reading_them() {
        use std::os::unix::fs::PermissionsExt;
        let fx = Fixture::new("memo-noread");
        fx.settle(&["src/routes/index.tsx", "src/routes/about.tsx"]);
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        let key = fx.store().persist(&fx.inputs(), &outputs).unwrap();
        for p in fx.inputs() {
            fs::set_permissions(&p, fs::Permissions::from_mode(0o000)).unwrap();
        }
        let stats = fx.store().restore(&fx.inputs(), &outputs).unwrap();
        assert_eq!(stats.key, key);
        assert_eq!(stats.rehashed, 0, "memo hit must not read file contents");
        for p in fx.inputs() {
            fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
        }
    }

    #[test]
    fn touched_but_unchanged_file_rehashes_to_the_same_key() {
        let fx = Fixture::new("memo-touch");
        fx.settle(&["src/routes/index.tsx", "src/routes/about.tsx"]);
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        let key = fx.store().persist(&fx.inputs(), &outputs).unwrap();
        fx.write("src/routes/index.tsx", "export const Route = 1;");
        let stats = fx.store().restore(&fx.inputs(), &outputs).unwrap();
        assert_eq!(stats.key, key, "same content must reach the same key");
        assert!(stats.rehashed >= 1, "stat mismatch must force a re-hash");
    }

    #[test]
    fn racy_fresh_files_are_not_memoized() {
        let fx = Fixture::new("memo-racy");
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        fx.store().persist(&fx.inputs(), &outputs).unwrap();
        let target = fx.path("src/routes/index.tsx");
        let orig_mtime = fs::metadata(&target).unwrap().modified().unwrap();
        // Same length, same mtime, different content: only a re-hash sees it.
        fx.write("src/routes/index.tsx", "export const Route = 9;");
        set_mtime(&target, orig_mtime);
        assert!(
            matches!(
                fx.store().restore(&fx.inputs(), &outputs),
                Err(Miss::NoEntryForKey(_))
            ),
            "stat-identical edit within the racy window must still miss"
        );
    }

    #[test]
    fn prune_keeps_newest_entries() {
        let fx = Fixture::new("prune");
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        let store = fx.store();
        let mut keys = Vec::new();
        for i in 0..3 {
            fx.write(
                "src/routes/index.tsx",
                &format!("export const Route = {i};"),
            );
            keys.push(store.persist(&fx.inputs(), &outputs).unwrap());
            std::thread::sleep(Duration::from_millis(20));
        }
        store.prune(1);
        assert!(fx.entry_dir("route-tree", &keys[2]).is_dir());
        assert!(!fx.entry_dir("route-tree", &keys[0]).is_dir());
        assert!(!fx.entry_dir("route-tree", &keys[1]).is_dir());
    }

    #[test]
    fn flipped_byte_in_stored_artifact_misses_and_never_reaches_the_tree() {
        let fx = Fixture::new("flip");
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        let key = fx.store().persist(&fx.inputs(), &outputs).unwrap();

        // Same-size, single-byte corruption of the stored artifact: only a
        // content re-hash can catch it.
        let stored = fx.entry_dir("route-tree", &key).join("routeTree.gen.ts");
        let mut bytes = fs::read(&stored).unwrap();
        bytes[3] ^= 1;
        fs::write(&stored, &bytes).unwrap();

        fx.write("src/routeTree.gen.ts", "// generator-fresh");
        match fx.store().restore(&fx.inputs(), &outputs) {
            Err(Miss::EntryCorrupt(k)) => assert_eq!(k, key),
            other => panic!("expected corrupt entry, got {other:?}"),
        }
        assert!(
            !fx.entry_dir("route-tree", &key).exists(),
            "corrupt entry must be removed"
        );
        assert_eq!(
            fs::read_to_string(&out).unwrap(),
            "// generator-fresh",
            "corrupt bytes must never be restored into the tree"
        );

        // Recompute path: a fresh persist round-trips again.
        let key2 = fx.store().persist(&fx.inputs(), &outputs).unwrap();
        assert_eq!(key2, key);
        fs::remove_file(&out).unwrap();
        fx.store().restore(&fx.inputs(), &outputs).unwrap();
        assert_eq!(fs::read_to_string(&out).unwrap(), "// generator-fresh");
    }

    #[test]
    fn entry_without_manifest_is_corrupt() {
        let fx = Fixture::new("nomanifest");
        let (out, _) = tree_outputs(&fx);
        let outputs = [("routeTree.gen.ts", out.as_path())];
        let key = fx.store().persist(&fx.inputs(), &outputs).unwrap();
        fs::remove_file(fx.entry_dir("route-tree", &key).join(FILES_FILE)).unwrap();
        assert!(matches!(
            fx.store().restore(&fx.inputs(), &outputs),
            Err(Miss::EntryCorrupt(_))
        ));
        assert!(!fx.entry_dir("route-tree", &key).exists());
    }
}
