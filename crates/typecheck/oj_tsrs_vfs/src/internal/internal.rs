use std::borrow::Cow;

use rustc_hash::FxHashSet;
use tsrs_core::tspath;

use crate::{Entries, FileInfo, FileMode, FsError};

// IoFS is the subset of Go's io/fs.FS (Open, plus the fs.Stat / fs.ReadDir /
// fs.ReadFile helpers) that the vfs implementations are built on. Names are
// io/fs names: slash-separated, unrooted, validated with valid_path.
//
// The as_* methods stand in for Go's interface type assertions
// (fsys.(iovfs.RealpathFS), fsys.(iovfs.WritableFS)).
pub trait IoFS: Send + Sync {
    fn stat(&self, name: &str) -> Result<FileInfo, FsError>;

    // file_type is the subset of Stat needed by FileExists and DirectoryExists. Implementations may avoid
    // collecting the name, size and modification time; the default preserves the behavior of custom file systems.
    fn file_type(&self, name: &str) -> Result<FileMode, FsError> {
        self.stat(name).map(|info| info.mode.type_())
    }

    // ReadDir returns the entries sorted by filename, like fs.ReadDir.
    fn read_dir(&self, name: &str) -> Result<Vec<IoDirEntry>, FsError>;

    fn read_file(&self, name: &str) -> Result<Vec<u8>, FsError>;

    fn as_realpath_fs(&self) -> Option<&dyn crate::iovfs::RealpathFS> {
        None
    }

    fn as_writable_fs(&self) -> Option<&dyn crate::iovfs::WritableFS> {
        None
    }
}

// IoDirEntry is the part of fs.DirEntry that Common uses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoDirEntry {
    pub name: String,
    pub type_: FileMode,
}

// ValidPath is fs.ValidPath.
pub fn valid_path(name: &str) -> bool {
    if name == "." {
        // special case
        return true;
    }

    // No element is empty, "." or "..": the first and last are not empty (an empty name is one empty element), and
    // no element is "." or ".." or empty between two slashes (one scan instead of a search per element).
    !name.is_empty()
        && !name.starts_with('/')
        && !name.ends_with('/')
        && !tspath::has_relative_path_segment(name)
}

// RootFor lends the io/fs view of one path root for one operation: os.DirFS(root) on the OS, fs.Sub(fsys, root)
// over an IoFS, the host's root in WebAssembly. The view borrows the root instead of owning a copy in a box, so a
// probe (`file_exists`, `directory_exists`, `stat`) allocates nothing before the joined path it hands the system.
pub trait RootFor: Send + Sync {
    // Calls f with the file system serving root (a root as `split_path` returns it: "/", "c:/", "//server/share/"),
    // or returns None when no file system serves it.
    fn with_root<T>(&self, root: &str, f: impl FnOnce(&dyn IoFS) -> T) -> Option<T>;
}

pub struct Common<R: RootFor> {
    pub root_for: R,
    pub is_reparse_point: Option<fn(&str) -> bool>,
}

pub fn root_length(p: &str) -> usize {
    let l = tspath::get_encoded_root_length(p);
    assert!(l != tspath::EncodedRootLength::Relative, "vfs: path {p:?} is not absolute");
    l.len()
}

// SplitPath is a normalized absolute path split into its root and the rest without its trailing separator:
// "/a/b/" -> ("/", "a/b"), "c:/a" -> ("c:/", "a"). It borrows the path when that is already normal (the resolver
// probes combined, normalized paths), copying only one with a backslash, a "." or ".." segment or a doubled
// separator.
pub(crate) struct SplitPath<'a> {
    normalized: Cow<'a, str>,
    root_length: usize,
}

impl SplitPath<'_> {
    pub(crate) fn root(&self) -> &str {
        &self.normalized[..self.root_length]
    }

    pub(crate) fn rest(&self) -> &str {
        tspath::remove_trailing_directory_separator(&self.normalized[self.root_length..])
    }
}

pub(crate) fn split_path(p: &str) -> SplitPath<'_> {
    let normalized = tspath::normalize_path_cow(p);
    let root_length = root_length(&normalized);
    SplitPath { normalized, root_length }
}

impl<R: RootFor> Common<R> {
    // Calls f with the file system of path's root and path's io/fs name in it, or returns None when no file system
    // serves the root.
    fn with_root_and_path<T>(&self, path: &str, f: impl FnOnce(&dyn IoFS, &str) -> T) -> Option<T> {
        let split = split_path(path);
        let rest = match split.rest() {
            "" => ".",
            rest => rest,
        };
        self.root_for.with_root(split.root(), |fsys| f(fsys, rest))
    }

    pub fn stat(&self, path: &str) -> Option<FileInfo> {
        self.with_root_and_path(path, |fsys, rest| fsys.stat(rest).ok())?
    }

    pub fn file_exists(&self, path: &str) -> bool {
        self.with_root_and_path(
            path,
            |fsys, rest| matches!(fsys.file_type(rest), Ok(mode) if !mode.is_dir()),
        )
        .unwrap_or(false)
    }

    pub fn directory_exists(&self, path: &str) -> bool {
        self.with_root_and_path(
            path,
            |fsys, rest| matches!(fsys.file_type(rest), Ok(mode) if mode.is_dir()),
        )
        .unwrap_or(false)
    }

    pub fn get_accessible_entries(&self, path: &str) -> Entries {
        let mut result = Entries::default();
        let mut symlinks = FxHashSet::default();

        let add_to_result = |result: &mut Entries,
                             symlinks: &mut FxHashSet<String>,
                             name: &str,
                             mode: FileMode,
                             is_link: bool|
         -> bool {
            if mode.is_dir() {
                result.directories.push(name.to_string());
            } else if mode.is_regular() {
                result.files.push(name.to_string());
            } else {
                return false;
            }

            if is_link {
                symlinks.insert(name.to_string());
            }
            true
        };

        for entry in self.get_entries(path) {
            let entry_type = entry.type_;

            if add_to_result(&mut result, &mut symlinks, &entry.name, entry_type, false) {
                continue;
            }

            if entry_type.intersects(FileMode::Symlink) {
                // Easy case; UNIX-like system will clearly mark symlinks.
                if let Some(stat) = self.stat(&format!("{}/{}", path, entry.name)) {
                    add_to_result(&mut result, &mut symlinks, &entry.name, stat.mode, true);
                }
                continue;
            }

            if entry_type.intersects(FileMode::Irregular)
                && let Some(is_reparse_point) = self.is_reparse_point
            {
                // Could be a Windows junction or other reparse point.
                // Check using the OS-specific helper.
                let full_path = format!("{}/{}", path, entry.name);
                if is_reparse_point(&full_path)
                    && let Some(stat) = self.stat(&full_path)
                {
                    add_to_result(&mut result, &mut symlinks, &entry.name, stat.mode, true);
                }
            }
        }

        result.symlinks = Some(symlinks);
        result
    }

    fn get_entries(&self, path: &str) -> Vec<IoDirEntry> {
        self.with_root_and_path(path, |fsys, rest| fsys.read_dir(rest).unwrap_or_default())
            .unwrap_or_default()
    }

    pub fn read_file(&self, path: &str) -> Option<String> {
        let b = self.with_root_and_path(path, |fsys, rest| fsys.read_file(rest).ok()).flatten()?;

        if b.is_empty() {
            return Some(String::new());
        }

        Some(decode_bytes(b))
    }
}

pub fn decode_bytes(mut s: Vec<u8>) -> String {
    if s.len() >= 2 {
        match [s[0], s[1]] {
            [0xFF, 0xFE] => return decode_utf16(&s[2..], false),
            [0xFE, 0xFF] => return decode_utf16(&s[2..], true),
            _ => {}
        }
    }
    if s.len() >= 3 && s[0] == 0xEF && s[1] == 0xBB && s[2] == 0xBF {
        s.drain(..3);
    }

    // Go strings may hold arbitrary bytes; Rust strings must be UTF-8, so
    // invalid sequences are replaced with U+FFFD.
    if tsrs_core::utf8::basic::from_utf8(&s).is_ok() {
        // SAFETY: simdutf8 validated every byte, and s has not been modified since validation.
        unsafe { String::from_utf8_unchecked(s) }
    } else {
        String::from_utf8_lossy(&s).into_owned()
    }
}

fn decode_utf16(s: &[u8], big_endian: bool) -> String {
    let ints = s.as_chunks::<2>().0.iter().map(|c| {
        if big_endian { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) }
    });
    char::decode_utf16(ints).map(|r| r.unwrap_or(char::REPLACEMENT_CHARACTER)).collect()
}
