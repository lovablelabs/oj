use rustc_hash::FxHashSet;
use tsrs_core::tspath;

use crate::{FS, FileInfo, FileMode, FsError};

// The result of a WalkDir callback: Go's fs.SkipDir / fs.SkipAll sentinels or a real error.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WalkDirError {
    SkipDir,
    SkipAll,
    Err(FsError),
}

impl From<FsError> for WalkDirError {
    fn from(e: FsError) -> WalkDirError {
        WalkDirError::Err(e)
    }
}

// WalkDir calls walkFn for root and each accessible descendant in lexical order.
// Symbolic links are reported but not followed. Directory read failures are
// indistinguishable from empty directories because [FS.GetAccessibleEntries]
// does not return errors. The FileInfo returned by DirEntry.Info for a symbolic
// link contains only its name and mode because [FS] does not provide lstat.
// Using DirEntry.Info() requires the FS to implement Stat().
pub fn walk_dir(
    file_system: &dyn FS,
    root: &str,
    walk_fn: &mut dyn FnMut(&str, Option<&DirEntry>, Option<&FsError>) -> Result<(), WalkDirError>,
) -> Result<(), FsError> {
    let Some(root_info) = file_system.stat(root) else {
        return normalize_walk_dir_error(walk_fn(root, None, Some(&FsError::NotExist)));
    };

    let use_case_sensitive_file_names = file_system.use_case_sensitive_file_names();
    let root_prefix = root[..tspath::get_root_length(root)].to_string();

    let mut w =
        WalkState { use_case_sensitive_file_names, root_prefix, visited: FxHashSet::default() };

    let mut root_entry = DirEntry {
        path: root.to_string(),
        name: root_info.name.clone(),
        mode: root_info.mode,
        info: Some(root_info),
    };
    let root_realpath = file_system.realpath(root);
    if tspath::get_root_length(root) != root.len() {
        let parent = tspath::get_directory_path(root);
        let expected_realpath = tspath::combine_paths(
            &file_system.realpath(&parent),
            &[&tspath::get_base_file_name(root)],
        );
        if !w.equivalent(&root_realpath, &expected_realpath) {
            root_entry = DirEntry {
                path: root.to_string(),
                name: tspath::get_base_file_name(root),
                mode: FileMode::Symlink,
                info: None,
            };
        }
    }
    let result = w.visit(file_system, walk_fn, root, &root_entry, &root_realpath);
    normalize_walk_dir_error(result)
}

struct WalkState {
    use_case_sensitive_file_names: bool,
    root_prefix: String,
    visited: FxHashSet<String>,
}

impl WalkState {
    fn same_root(&self, path: &str) -> bool {
        let path_root_length = tspath::get_root_length(path);
        path_root_length == self.root_prefix.len()
            && tspath::compare_paths(
                &path[..path_root_length],
                &self.root_prefix,
                &tspath::ComparePathsOptions {
                    use_case_sensitive_file_names: self.use_case_sensitive_file_names,
                    ..Default::default()
                },
            ) == std::cmp::Ordering::Equal
    }

    fn equivalent(&self, left: &str, right: &str) -> bool {
        tspath::compare_paths(
            left,
            right,
            &tspath::ComparePathsOptions {
                use_case_sensitive_file_names: self.use_case_sensitive_file_names,
                ..Default::default()
            },
        ) == std::cmp::Ordering::Equal
    }

    fn canonicalize(&self, path: &str) -> String {
        tspath::get_canonical_file_name(
            &tspath::normalize_path(path),
            self.use_case_sensitive_file_names,
        )
    }

    fn visit(
        &mut self,
        file_system: &dyn FS,
        walk_fn: &mut dyn FnMut(
            &str,
            Option<&DirEntry>,
            Option<&FsError>,
        ) -> Result<(), WalkDirError>,
        path: &str,
        entry: &DirEntry,
        realpath: &str,
    ) -> Result<(), WalkDirError> {
        if entry.is_dir() {
            let canonical_realpath = self.canonicalize(realpath);
            if self.visited.contains(&canonical_realpath) {
                return Ok(());
            }
            self.visited.insert(canonical_realpath);
        }

        if let Err(err) = walk_fn(path, Some(entry), None) {
            if err == WalkDirError::SkipDir && entry.is_dir() {
                return Ok(());
            }
            return Err(err);
        }
        if !entry.is_dir() {
            return Ok(());
        }

        let entries = file_system.get_accessible_entries(path);
        let mut directories: FxHashSet<&str> = FxHashSet::default();
        directories.extend(entries.directories.iter().map(std::string::String::as_str));
        let mut names: Vec<&str> =
            entries.directories.iter().map(std::string::String::as_str).collect();
        names.extend(entries.files.iter().map(std::string::String::as_str));
        names.sort_unstable();
        for name in names {
            let child_path = tspath::combine_paths(path, &[name]);
            if !self.same_root(&child_path) {
                continue;
            }

            let mut mode = FileMode::None;
            if directories.contains(name) {
                mode = FileMode::Dir;
            }
            let mut child_realpath = String::new();
            let is_symlink;
            if let Some(symlinks) = &entries.symlinks {
                is_symlink = symlinks.contains(name);
                if !is_symlink && mode.is_dir() {
                    child_realpath = tspath::combine_paths(realpath, &[name]);
                }
            } else {
                child_realpath = file_system.realpath(&child_path);
                is_symlink =
                    !self.equivalent(&child_realpath, &tspath::combine_paths(realpath, &[name]));
            }
            if is_symlink {
                mode = FileMode::Symlink;
            }
            let child_entry =
                DirEntry { path: child_path.clone(), name: name.to_string(), mode, info: None };
            if !mode.is_dir() {
                if let Err(err) = self.visit(file_system, walk_fn, &child_path, &child_entry, "") {
                    if err == WalkDirError::SkipDir {
                        return Ok(());
                    }
                    return Err(err);
                }
                continue;
            }

            if child_realpath.is_empty() {
                child_realpath = file_system.realpath(&child_path);
            }
            if let Err(err) =
                self.visit(file_system, walk_fn, &child_path, &child_entry, &child_realpath)
            {
                if err == WalkDirError::SkipDir {
                    return Ok(());
                }
                return Err(err);
            }
        }
        Ok(())
    }
}

fn normalize_walk_dir_error(err: Result<(), WalkDirError>) -> Result<(), FsError> {
    match err {
        Ok(()) | Err(WalkDirError::SkipDir | WalkDirError::SkipAll) => Ok(()),
        Err(WalkDirError::Err(e)) => Err(e),
    }
}

// DirEntry is [fs.DirEntry] as produced by WalkDir: either the root's
// FileInfo converted with fs.FileInfoToDirEntry, or a walkDirEntry.
#[derive(Clone, Debug)]
pub struct DirEntry {
    path: String,
    name: String,
    mode: FileMode,
    info: Option<FileInfo>,
}

impl DirEntry {
    pub fn name(&self) -> &str {
        &self.name
    }

    fn is_dir(&self) -> bool {
        self.mode.is_dir()
    }

    pub fn type_(&self) -> FileMode {
        self.mode.type_()
    }

    pub fn info(&self, file_system: &dyn FS) -> Result<FileInfo, FsError> {
        if let Some(info) = &self.info {
            return Ok(info.clone());
        }
        if self.mode.intersects(FileMode::Symlink) {
            return Ok(FileInfo {
                name: self.name.clone(),
                size: 0,
                mode: self.mode,
                mod_time: None,
            });
        }
        match file_system.stat(&self.path) {
            Some(info) => Ok(info),
            None => Err(FsError::NotExist),
        }
    }
}
