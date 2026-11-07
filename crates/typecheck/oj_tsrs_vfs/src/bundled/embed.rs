use std::sync::LazyLock;
use std::time::SystemTime;

use rustc_hash::FxHashMap;

use super::embed_generated::EMBEDDED_CONTENTS;
use super::libs_generated::LIB_NAMES;
use crate::{Entries, FS, FileInfo, FileMode};

const SCHEME: &str = "bundled:///";

static EMBEDDED_CONTENTS_MAP: LazyLock<FxHashMap<&'static str, &'static str>> =
    LazyLock::new(|| EMBEDDED_CONTENTS.iter().copied().collect());

fn embedded_contents(rest: &str) -> Option<&'static str> {
    EMBEDDED_CONTENTS_MAP.get(rest).copied()
}

fn split_path(path: &str) -> Option<&str> {
    path.strip_prefix(SCHEME)
}

pub fn lib_path() -> String {
    format!("{SCHEME}libs")
}

pub fn is_bundled(path: &str) -> bool {
    split_path(path).is_some()
}

// wrappedFS is implemented directly rather than going through an io/fs layer.
// Our vfs.FS works with file contents in terms of strings, and that's
// what include_str! gives us.

pub struct WrappedFS<T: FS> {
    fs: T,
}

pub fn wrap_fs<T: FS>(fs: T) -> WrappedFS<T> {
    WrappedFS { fs }
}

impl<T: FS> WrappedFS<T> {
    // ReadFile without copying the embedded contents.
}

/// The embedded text of a `bundled:///` path (the lib files `include_str!` put in the binary), without a copy; `None`
/// for any other path.
///
/// tsrs-only: the compiler host parses this text in place instead of reading a heap copy through
/// `FS::read_file` and leaking it (lib.dom.d.ts alone is 2.3 MB).
#[cfg(not(feature = "noembed"))]
pub(crate) fn embedded_file(path: &str) -> Option<&'static str> {
    split_path(path).and_then(embedded_contents)
}

impl<T: FS> FS for WrappedFS<T> {
    fn use_case_sensitive_file_names(&self) -> bool {
        self.fs.use_case_sensitive_file_names()
    }

    fn file_exists(&self, path: &str) -> bool {
        if let Some(rest) = split_path(path) {
            return embedded_contents(rest).is_some();
        }
        self.fs.file_exists(path)
    }

    fn read_file(&self, path: &str) -> Option<String> {
        if let Some(rest) = split_path(path) {
            return embedded_contents(rest).map(std::string::ToString::to_string);
        }
        self.fs.read_file(path)
    }

    fn directory_exists(&self, path: &str) -> bool {
        if let Some(rest) = split_path(path) {
            return rest == "libs";
        }
        self.fs.directory_exists(path)
    }

    fn get_accessible_entries(&self, path: &str) -> Entries {
        if let Some(rest) = split_path(path) {
            let mut result = Entries::default();
            if rest.is_empty() {
                result.directories = vec!["libs".to_string()];
            } else if rest == "libs" {
                result.files = LIB_NAMES.iter().map(std::string::ToString::to_string).collect();
            }
            return result;
        }
        self.fs.get_accessible_entries(path)
    }

    fn stat(&self, path: &str) -> Option<FileInfo> {
        if let Some(rest) = split_path(path) {
            if rest.is_empty() || rest == "libs" {
                return Some(FileInfo {
                    name: rest.to_string(),
                    size: 0,
                    mode: FileMode::Dir,
                    mod_time: None,
                });
            }
            if let Some(lib) = embedded_contents(rest) {
                let lib_name = rest.strip_prefix("libs/").unwrap_or(rest);
                return Some(FileInfo {
                    name: lib_name.to_string(),
                    size: lib.len() as i64,
                    mode: FileMode::None,
                    mod_time: None,
                });
            }
            return None;
        }
        self.fs.stat(path)
    }

    fn realpath(&self, path: &str) -> String {
        if split_path(path).is_some() {
            return path.to_string();
        }
        self.fs.realpath(path)
    }

    fn write_file(&self, path: &str, data: &str) -> Result<(), String> {
        assert!(split_path(path).is_none(), "cannot write to embedded file system");
        self.fs.write_file(path, data)
    }

    fn append_file(&self, path: &str, data: &str) -> Result<(), String> {
        assert!(split_path(path).is_none(), "cannot write to embedded file system");
        self.fs.append_file(path, data)
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        assert!(split_path(path).is_none(), "cannot remove from embedded file system");
        self.fs.remove(path)
    }

    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), String> {
        assert!(split_path(path).is_none(), "cannot change times on embedded file system");
        self.fs.chtimes(path, a_time, m_time)
    }
}
