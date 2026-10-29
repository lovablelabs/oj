use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::RwLock;
use std::time::SystemTime;

use rustc_hash::FxHashMap;
use tsrs_core::tspath;

use crate::internal::{self, IoDirEntry, IoFS};
use crate::iovfs::{self, IoVFS, RealpathFS, WritableFS};
use crate::{FileInfo, FileMode, FsError};

// MapFile is fstest.MapFile (without Sys, which vfstest uses for the realpath).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MapFile {
    pub data: Vec<u8>,
    pub mode: FileMode,
    pub mod_time: Option<SystemTime>,
}

impl From<&str> for MapFile {
    fn from(s: &str) -> MapFile {
        MapFile { data: s.as_bytes().to_vec(), ..Default::default() }
    }
}

impl From<String> for MapFile {
    fn from(s: String) -> MapFile {
        MapFile { data: s.into_bytes(), ..Default::default() }
    }
}

impl From<&String> for MapFile {
    fn from(s: &String) -> MapFile {
        MapFile { data: s.as_bytes().to_vec(), ..Default::default() }
    }
}

impl From<Vec<u8>> for MapFile {
    fn from(data: Vec<u8>) -> MapFile {
        MapFile { data, ..Default::default() }
    }
}

impl From<&[u8]> for MapFile {
    fn from(data: &[u8]) -> MapFile {
        MapFile { data: data.to_vec(), ..Default::default() }
    }
}

impl From<&MapFile> for MapFile {
    fn from(f: &MapFile) -> MapFile {
        f.clone()
    }
}

// An entry of the underlying map: the file plus the `sys` realpath Go stores in MapFile.Sys.
#[derive(Clone, Debug)]
struct MapEntry {
    file: MapFile,
    realpath: String,
}

pub struct MapFS {
    // keys in m are canonicalPaths
    inner: RwLock<MapFSInner>,

    use_case_sensitive_file_names: bool,

    // vfstest.go Clock: the source of modification times (time.Now when absent).
    clock: Option<Clock>,
}

// vfstest.go:38 Clock (Now; SinceStart is not used by MapFS).
pub type Clock = std::sync::Arc<dyn Fn() -> SystemTime + Send + Sync>;

#[derive(Default)]
struct MapFSInner {
    m: BTreeMap<String, MapEntry>,
    symlinks: FxHashMap<String, String>,
}

// FromMap creates a new [vfs.FS] from a map of paths to file contents.
// Those file contents may be strings, byte slices, or [MapFile]s.
//
// The paths must be normalized absolute paths according to the tspath package,
// without trailing directory separators.
// The paths must be all POSIX-style or all Windows-style, but not both.
pub fn from_map<K: AsRef<str>, F: Into<MapFile>>(
    m: impl IntoIterator<Item = (K, F)>,
    use_case_sensitive_file_names: bool,
) -> IoVFS<MapFS> {
    from_map_with_clock_option(m, use_case_sensitive_file_names, None)
}

// vfstest.go:80 FromMapWithClock
pub fn from_map_with_clock<K: AsRef<str>, F: Into<MapFile>>(
    m: impl IntoIterator<Item = (K, F)>,
    use_case_sensitive_file_names: bool,
    clock: Clock,
) -> IoVFS<MapFS> {
    from_map_with_clock_option(m, use_case_sensitive_file_names, Some(clock))
}

fn from_map_with_clock_option<K: AsRef<str>, F: Into<MapFile>>(
    m: impl IntoIterator<Item = (K, F)>,
    use_case_sensitive_file_names: bool,
    clock: Option<Clock>,
) -> IoVFS<MapFS> {
    let now = |clock: &Option<Clock>| match clock {
        Some(clock) => clock(),
        None => SystemTime::now(),
    };
    let mut posix = false;
    let mut windows = false;

    let mut check_path = |p: &str| {
        assert!(tspath::is_rooted_disk_path(p), "non-rooted path {p:?}");

        let normal =
            tspath::remove_trailing_directory_separator(&tspath::normalize_path(p)).to_string();
        assert!(normal == p, "non-normalized path {p:?}");

        if p.starts_with('/') {
            posix = true;
        } else {
            windows = true;
        }
    };

    let mut input: Vec<(String, MapFile)> =
        m.into_iter().map(|(k, f)| (k.as_ref().to_string(), f.into())).collect();
    // Sorted creation to ensure times are always guaranteed to be in order.
    input.sort_by(|a, b| compare_paths_by_parts(&a.0, &b.0));
    let mut mfs: Vec<(String, MapFile)> = Vec::with_capacity(input.len());
    mfs.extend(input.into_iter().map(|(p, f)| {
        check_path(&p);

        let mut file = f;
        file.mod_time = Some(now(&clock));

        if file.mode.intersects(FileMode::Symlink) {
            let target = tsrs_core::utf8::from_utf8_lossy(&file.data).into_owned();
            check_path(&target);

            let target = target.strip_prefix('/').unwrap_or(&target).to_string();
            file.data = target.into_bytes();
        }

        let p = p.strip_prefix('/').unwrap_or(&p).to_string();
        (p, file)
    }));

    assert!(!(posix && windows), "mixed posix and windows paths");

    let mut mapfs = convert_map_fs(mfs, use_case_sensitive_file_names);
    mapfs.clock = clock;
    iovfs::from(mapfs, use_case_sensitive_file_names)
}

pub fn convert_map_fs(input: Vec<(String, MapFile)>, use_case_sensitive_file_names: bool) -> MapFS {
    let m = MapFS {
        inner: RwLock::new(MapFSInner::default()),
        use_case_sensitive_file_names,
        clock: None,
    };

    // Verify that the input is well-formed.
    let mut canonical_paths: FxHashMap<String, String> = FxHashMap::default();
    let mut input_map: FxHashMap<String, MapFile> = FxHashMap::default();
    for (path, file) in input {
        let canonical = m.get_canonical_path(&path);
        if let Some(other) = canonical_paths.get(&canonical)
            && *other != path
        {
            // Ensure consistent panic messages
            let (path, other) = if path.as_str() < other.as_str() {
                (path.as_str(), other.as_str())
            } else {
                (other.as_str(), path.as_str())
            };
            panic!("duplicate path: {path:?} and {other:?} have the same canonical path");
        }
        canonical_paths.insert(canonical, path.clone());
        input_map.insert(path, file);
    }

    // Sort the input by depth and path so we ensure parent dirs are created
    // before their children, if explicitly specified by the input.
    let mut input_keys: Vec<String> = input_map.keys().cloned().collect();
    input_keys.sort_by(|a, b| compare_paths_by_parts(a, b));

    {
        let mut inner = m.inner.write().unwrap();
        for p in input_keys {
            let file = input_map.remove(&p).unwrap();

            // Create all missing intermediate directories so we can attach the realpath to each of them.
            // fstest.MapFS doesn't require this as it synthesizes directories on the fly, but it's a lot
            // harder to reapply a realpath onto those when we're deep in some FileInfo method.
            let dir = dir_name(&p);
            if !dir.is_empty()
                && let Err(err) =
                    m.mkdir_all_locked(&mut inner, dir, FileMode::from_bits_retain(0o777))
            {
                panic!("failed to create intermediate directories for {p:?}: {err}");
            }
            let canonical = m.get_canonical_path(&p);
            m.set_entry(&mut inner, &p, &canonical, file);
        }
    }

    m
}

fn compare_paths_by_parts(a: &str, b: &str) -> Ordering {
    let mut a = a;
    let mut b = b;
    loop {
        let (a_start, a_end, a_ok) = cut(a, '/');
        let (b_start, b_end, b_ok) = cut(b, '/');

        if !a_ok || !b_ok {
            return a.cmp(b);
        }

        let r = a_start.cmp(b_start);
        if r != Ordering::Equal {
            return r;
        }

        a = a_end;
        b = b_end;
    }
}

fn cut(s: &str, sep: char) -> (&str, &str, bool) {
    match s.find(sep) {
        Some(i) => (&s[..i], &s[i + 1..], true),
        None => (s, "", false),
    }
}

// Symlink returns a MapFile describing a symbolic link to target.
pub fn symlink(target: &str) -> MapFile {
    MapFile { data: target.as_bytes().to_vec(), mode: FileMode::Symlink, mod_time: None }
}

fn split_path(s: &str, offset: usize) -> (&str, &str) {
    match s[offset..].find('/') {
        None => (s, ""),
        Some(idx) => (&s[..idx + offset], &s[idx + 1 + offset..]),
    }
}

fn dir_name(p: &str) -> &str {
    let dir = match p.rfind('/') {
        Some(i) => &p[..=i],
        None => "",
    };
    dir.strip_suffix('/').unwrap_or(dir)
}

fn base_name(p: &str) -> &str {
    match p.rfind('/') {
        Some(i) => &p[i + 1..],
        None => p,
    }
}

const UMASK: u32 = 0o022;

// Result of Open: a regular file or a directory with its sorted children.
enum Opened {
    File { info: FileInfo, data: Vec<u8> },
    Dir { info: FileInfo, entries: Vec<IoDirEntry> },
}

impl MapFS {
    fn now(&self) -> SystemTime {
        match &self.clock {
            Some(clock) => clock(),
            None => SystemTime::now(),
        }
    }

    fn get_canonical_path(&self, p: &str) -> String {
        tspath::get_canonical_file_name(p, self.use_case_sensitive_file_names)
    }

    fn remove_locked(&self, inner: &mut MapFSInner, path: &str) {
        let canonical = self.get_canonical_path(path);
        let Some(file_info) = inner.m.remove(&canonical) else {
            // file does not exist
            return;
        };
        inner.symlinks.remove(&canonical);

        if file_info.file.mode.is_dir() {
            let prefix = format!("{canonical}/");
            inner.m.retain(|path, _| !path.starts_with(&prefix));
            inner.symlinks.retain(|path, _| !path.starts_with(&prefix));
        }
    }

    // getFollowingSymlinks returns the resolved canonical path; on success the
    // file is inner.m[cp] (Go returns it alongside).
    fn get_following_symlinks(inner: &MapFSInner, p: &str) -> (String, Result<(), FsError>) {
        Self::get_following_symlinks_worker(inner, p.to_string(), String::new(), String::new())
    }

    fn get_following_symlinks_worker(
        inner: &MapFSInner,
        p: String,
        symlink_from: String,
        symlink_to: String,
    ) -> (String, Result<(), FsError>) {
        if let Some(file) = inner.m.get(&p)
            && !file.file.mode.intersects(FileMode::Symlink)
        {
            return (p, Ok(()));
        }

        if let Some(target) = inner.symlinks.get(&p) {
            let target = target.clone();
            return Self::get_following_symlinks_worker(inner, target.clone(), p, target);
        }

        // This could be a path underneath a symlinked directory. Go takes the first match in map order; when the
        // path is under two symlinks (one inside the other) that is random, so take the outer one, which a file
        // system resolves first.
        let outer = inner
            .symlinks
            .iter()
            .filter(|(other, _)| {
                other.len() < p.len()
                    && p.starts_with(other.as_str())
                    && p.as_bytes()[other.len()] == b'/'
            })
            .min_by_key(|(other, _)| other.len());
        if let Some((other, target)) = outer {
            return Self::get_following_symlinks_worker(
                inner,
                format!("{}{}", target, &p[other.len()..]),
                other.clone(),
                target.clone(),
            );
        }

        let err = if symlink_from.is_empty() {
            FsError::NotExist
        } else {
            FsError::BrokenSymlink { from: symlink_from, to: symlink_to }
        };
        (p, Err(err))
    }

    fn set_entry(&self, inner: &mut MapFSInner, realpath: &str, canonical: &str, file: MapFile) {
        assert!(!(realpath.is_empty() || canonical.is_empty()), "empty path");

        let is_symlink = file.mode.intersects(FileMode::Symlink);
        let target = if is_symlink {
            Some(self.get_canonical_path(&tsrs_core::utf8::from_utf8_lossy(&file.data)))
        } else {
            None
        };
        inner.m.insert(canonical.to_string(), MapEntry { file, realpath: realpath.to_string() });

        if let Some(target) = target {
            inner.symlinks.insert(canonical.to_string(), target);
        }
    }

    fn mkdir_all_locked(
        &self,
        inner: &mut MapFSInner,
        p: &str,
        perm: FileMode,
    ) -> Result<(), FsError> {
        assert!(!p.is_empty(), "empty path");

        // Fast path; already exists.
        if let (other_path, Ok(())) =
            Self::get_following_symlinks(inner, &self.get_canonical_path(p))
        {
            if !inner.m[&other_path].file.mode.is_dir() {
                return Err(FsError::Other(format!(
                    "mkdir {p:?}: path exists but is not a directory"
                )));
            }
            return Ok(());
        }

        let mut p = p.to_string();
        let mut to_create: Vec<String> = Vec::new();
        let mut offset = 0;
        loop {
            let (dir, rest) = split_path(&p, offset);
            let (dir, rest) = (dir.to_string(), rest.to_string());
            let canonical = self.get_canonical_path(&dir);
            let (other_path, err) = Self::get_following_symlinks(inner, &canonical);
            if let Err(err) = err {
                if !err.is_not_exist() {
                    return Err(err);
                }
                to_create.push(dir.clone());
            } else {
                let other = &inner.m[&other_path];
                if !other.file.mode.is_dir() {
                    return Err(FsError::Other(format!(
                        "mkdir {other_path:?}: path exists but is not a directory"
                    )));
                }
                if canonical != other_path {
                    // We have a symlinked parent, reset and start again.
                    p = format!("{}/{}", other.realpath, rest);
                    to_create.clear();
                    offset = 0;
                    continue;
                }
            }
            if rest.is_empty() {
                break;
            }
            offset = dir.len() + 1;
        }

        for dir in to_create {
            let canonical = self.get_canonical_path(&dir);
            self.set_entry(
                inner,
                &dir,
                &canonical,
                MapFile {
                    data: Vec::new(),
                    mode: FileMode::Dir | FileMode::from_bits_retain(perm.bits() & !UMASK),
                    mod_time: Some(self.now()),
                },
            );
        }

        Ok(())
    }

    fn convert_info(entry: &MapEntry) -> FileInfo {
        FileInfo {
            name: base_name(&entry.realpath).to_string(),
            size: entry.file.data.len() as i64,
            mode: entry.file.mode,
            mod_time: entry.file.mod_time,
        }
    }

    // Open: vfstest's symlink resolution followed by fstest.MapFS.Open.
    fn open(&self, name: &str) -> Result<Opened, FsError> {
        let inner = self.inner.read().unwrap();

        let (cp, _) = Self::get_following_symlinks(&inner, &self.get_canonical_path(name));

        if !internal::valid_path(&cp) {
            return Err(FsError::NotExist);
        }

        let file = inner.m.get(&cp);
        if let Some(file) = file
            && !file.file.mode.is_dir()
        {
            // Ordinary file
            return Ok(Opened::File {
                info: Self::convert_info(file),
                data: file.file.data.clone(),
            });
        }

        // Directory, possibly synthesized.
        let mut entries: Vec<IoDirEntry> = Vec::new();
        let mut has_synthesized_child = false;
        let prefix = if cp == "." { String::new() } else { format!("{cp}/") };
        let range = if prefix.is_empty() {
            inner.m.range::<String, _>(..)
        } else {
            inner.m.range::<String, _>(prefix.clone()..)
        };
        for (fname, f) in range {
            if !fname.starts_with(&prefix) {
                break;
            }
            let felem = &fname[prefix.len()..];
            if cp == "." && fname == "." {
                continue;
            }
            if felem.contains('/') {
                has_synthesized_child = true;
            } else {
                entries.push(IoDirEntry {
                    name: base_name(&f.realpath).to_string(),
                    type_: f.file.mode.type_(),
                });
            }
        }
        // If the directory name is not in the map,
        // and there are no children of the name in the map,
        // then the directory is treated as not existing.
        if cp != "." && file.is_none() && entries.is_empty() && !has_synthesized_child {
            return Err(FsError::NotExist);
        }

        let info = if let Some(file) = file {
            Self::convert_info(file)
        } else {
            // This is a synthesized dir.
            assert!(name == ".", "unexpected synthesized dir: {name:?}");
            FileInfo {
                name: ".".to_string(),
                size: 0,
                mode: FileMode::Dir | FileMode::from_bits_retain(0o555),
                mod_time: None,
            }
        };

        // fs.ReadDir sorts by the (realpath-derived) entry names.
        entries.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(Opened::Dir { info, entries })
    }

    pub fn mkdir_all(&self, path: &str, perm: FileMode) -> Result<(), FsError> {
        let mut inner = self.inner.write().unwrap();
        self.mkdir_all_locked(&mut inner, path, perm)
    }

    pub fn add_symlink(&self, path: &str, target: &str) {
        let mut inner = self.inner.write().unwrap();

        let canonical = self.get_canonical_path(path);
        self.set_entry(
            &mut inner,
            path,
            &canonical,
            MapFile { data: target.as_bytes().to_vec(), mode: FileMode::Symlink, mod_time: None },
        );
    }

    fn write_file(&self, path: &str, data: &str, perm: FileMode) -> Result<(), FsError> {
        let mut inner = self.inner.write().unwrap();

        let parent = dir_name(path);
        if !parent.is_empty() {
            let canonical = self.get_canonical_path(parent);
            let (parent_path, err) = Self::get_following_symlinks(&inner, &canonical);
            if let Err(err) = err {
                return Err(FsError::Other(format!("write {path:?}: {err}")));
            }
            if !inner.m[&parent_path].file.mode.is_dir() {
                return Err(FsError::Other(format!(
                    "write {path:?}: parent path exists but is not a directory"
                )));
            }
        }

        let (cp, err) = Self::get_following_symlinks(&inner, &self.get_canonical_path(path));
        match err {
            Err(err) => {
                // No other errors are possible.
                assert!(
                    !(!err.is_not_exist() && !matches!(err, FsError::BrokenSymlink { .. })),
                    "{}",
                    err
                );
            }
            Ok(()) => {
                if !inner.m[&cp].file.mode.is_regular() {
                    return Err(FsError::Other(format!(
                        "write {path:?}: path exists but is not a regular file"
                    )));
                }
            }
        }

        self.set_entry(
            &mut inner,
            path,
            &cp,
            MapFile {
                data: data.as_bytes().to_vec(),
                mod_time: Some(self.now()),
                mode: FileMode::from_bits_retain(perm.bits() & !UMASK),
            },
        );

        Ok(())
    }

    fn append_file(&self, path: &str, data: &str, perm: FileMode) -> Result<(), FsError> {
        let mut inner = self.inner.write().unwrap();

        let parent = dir_name(path);
        if !parent.is_empty() {
            let canonical = self.get_canonical_path(parent);
            let (parent_path, err) = Self::get_following_symlinks(&inner, &canonical);
            if let Err(err) = err {
                return Err(FsError::Other(format!("append {path:?}: {err}")));
            }
            if !inner.m[&parent_path].file.mode.is_dir() {
                return Err(FsError::Other(format!(
                    "append {path:?}: parent path exists but is not a directory"
                )));
            }
        }

        let mut existing: Vec<u8> = Vec::new();
        let mut existing_mode = FileMode::None;
        let (cp, err) = Self::get_following_symlinks(&inner, &self.get_canonical_path(path));
        if let Err(err) = err {
            // No other errors are possible.
            assert!(
                !(!err.is_not_exist() && !matches!(err, FsError::BrokenSymlink { .. })),
                "{}",
                err
            );
        } else {
            let file = &inner.m[&cp];
            if !file.file.mode.is_regular() {
                return Err(FsError::Other(format!(
                    "append {path:?}: path exists but is not a regular file"
                )));
            }
            existing.clone_from(&file.file.data);
            existing_mode = file.file.mode;
        }

        let mut combined = Vec::with_capacity(existing.len() + data.len());
        combined.extend_from_slice(&existing);
        combined.extend_from_slice(data.as_bytes());

        let mut mode = existing_mode;
        if mode.is_empty() {
            mode = FileMode::from_bits_retain(perm.bits() & !UMASK);
        }

        self.set_entry(
            &mut inner,
            path,
            &cp,
            MapFile { data: combined, mod_time: Some(self.now()), mode },
        );

        Ok(())
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "The in-memory filesystem keeps the fallible Remove API shared with OS-backed file operations."
    )]
    fn remove(&self, path: &str) -> Result<(), FsError> {
        let mut inner = self.inner.write().unwrap();
        self.remove_locked(&mut inner, path);
        Ok(())
    }

    fn chtimes(&self, path: &str, _a_time: SystemTime, m_time: SystemTime) -> Result<(), FsError> {
        let mut inner = self.inner.write().unwrap();
        let canonical = self.get_canonical_path(path);
        let Some(file_info) = inner.m.get_mut(&canonical) else {
            // file does not exist
            return Err(FsError::NotExist);
        };
        file_info.file.mod_time = Some(m_time);
        Ok(())
    }

    pub fn get_target_of_symlink(&self, path: &str) -> Option<String> {
        let path = path.strip_prefix('/').unwrap_or(path);
        let inner = self.inner.read().unwrap();
        let canonical = self.get_canonical_path(path);
        if let Some(file_info) = inner.m.get(&canonical)
            && file_info.file.mode.intersects(FileMode::Symlink)
        {
            return Some(format!("/{}", tsrs_core::utf8::from_utf8_lossy(&file_info.file.data)));
        }
        None
    }

    pub fn get_mod_time(&self, path: &str) -> Option<SystemTime> {
        let path = path.strip_prefix('/').unwrap_or(path);
        let inner = self.inner.read().unwrap();
        let canonical = self.get_canonical_path(path);
        inner.m.get(&canonical)?.file.mod_time
    }

    pub fn entries(&self) -> Vec<(String, MapFile)> {
        let inner = self.inner.read().unwrap();
        let mut input_keys: Vec<&String> = inner.m.keys().collect();
        input_keys.sort_by(|a, b| compare_paths_by_parts(a, b));

        let mut result = Vec::with_capacity(input_keys.len());
        result.extend(input_keys.into_iter().map(|p| {
            let file = &inner.m[p];
            let mut path = file.realpath.clone();
            if !tspath::path_is_absolute(&path) {
                path = format!("/{path}");
            }
            (path, file.file.clone())
        }));
        result
    }

    pub fn get_file_info(&self, path: &str) -> Option<MapFile> {
        let path = path.strip_prefix('/').unwrap_or(path);
        let inner = self.inner.read().unwrap();
        let canonical = self.get_canonical_path(path);
        inner.m.get(&canonical).map(|f| f.file.clone())
    }
}

impl IoFS for MapFS {
    fn stat(&self, name: &str) -> Result<FileInfo, FsError> {
        match self.open(name)? {
            Opened::File { info, .. } | Opened::Dir { info, .. } => Ok(info),
        }
    }

    fn read_dir(&self, name: &str) -> Result<Vec<IoDirEntry>, FsError> {
        match self.open(name)? {
            Opened::File { .. } => Err(FsError::Other(format!("readdir {name}: not implemented"))),
            Opened::Dir { entries, .. } => Ok(entries),
        }
    }

    fn read_file(&self, name: &str) -> Result<Vec<u8>, FsError> {
        match self.open(name)? {
            Opened::File { data, .. } => Ok(data),
            Opened::Dir { .. } => Err(FsError::Other(format!("read {name}: is a directory"))),
        }
    }

    fn as_realpath_fs(&self) -> Option<&dyn RealpathFS> {
        Some(self)
    }

    fn as_writable_fs(&self) -> Option<&dyn WritableFS> {
        Some(self)
    }
}

impl RealpathFS for MapFS {
    fn realpath(&self, name: &str) -> Result<String, FsError> {
        let inner = self.inner.read().unwrap();

        let (cp, err) = Self::get_following_symlinks(&inner, &self.get_canonical_path(name));
        err?;
        Ok(inner.m[&cp].realpath.clone())
    }
}

impl WritableFS for MapFS {
    fn write_file(&self, path: &str, data: &str, perm: FileMode) -> Result<(), FsError> {
        MapFS::write_file(self, path, data, perm)
    }

    fn append_file(&self, path: &str, data: &str, perm: FileMode) -> Result<(), FsError> {
        MapFS::append_file(self, path, data, perm)
    }

    fn mkdir_all(&self, path: &str, perm: FileMode) -> Result<(), FsError> {
        MapFS::mkdir_all(self, path, perm)
    }

    fn remove(&self, path: &str) -> Result<(), FsError> {
        MapFS::remove(self, path)
    }

    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), FsError> {
        MapFS::chtimes(self, path, a_time, m_time)
    }
}
