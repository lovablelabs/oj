use std::time::SystemTime;

use tsrs_core::tspath;

use crate::internal::{self, Common, IoDirEntry, IoFS, RootFor};
use crate::{Entries, FS, FileInfo, FileMode, FsError};

pub trait RealpathFS: IoFS {
    fn realpath(&self, path: &str) -> Result<String, FsError>;
}

pub trait WritableFS: IoFS {
    fn write_file(&self, path: &str, data: &str, perm: FileMode) -> Result<(), FsError>;
    fn append_file(&self, path: &str, data: &str, perm: FileMode) -> Result<(), FsError>;
    fn mkdir_all(&self, path: &str, perm: FileMode) -> Result<(), FsError>;
    // Removes `path` and all its contents. Will return the first error it encounters.
    fn remove(&self, path: &str) -> Result<(), FsError>;
    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), FsError>;
}

// From creates a new FS from an [fs.FS].
//
// For paths like `c:/foo/bar`, fsys will be used as though it's rooted at `/` and the path is `/c:/foo/bar`.
//
// If the provided [fs.FS] implements [RealpathFS], it will be used to implement the Realpath method.
// If the provided [fs.FS] implements [WritableFS], it will be used to implement the WriteFile method.
//
// From does not actually handle case-insensitivity; ensure the passed in [fs.FS]
// respects case-insensitive file names if needed. Consider using [vfstest.FromMap] for testing.
#[expect(clippy::redundant_pub_crate, reason = "Re-exported within the crate for vfstest.")]
pub(crate) fn from<T: IoFS>(fsys: T, use_case_sensitive_file_names: bool) -> IoVFS<T> {
    IoVFS {
        common: Common { root_for: SubRoots { fsys }, is_reparse_point: None },
        use_case_sensitive_file_names,
    }
}

// fs.Sub(fsys, root) for each root; "/" and "." are fsys itself.
struct SubRoots<T: IoFS> {
    fsys: T,
}

impl<T: IoFS> RootFor for SubRoots<T> {
    fn with_root<R>(&self, root: &str, f: impl FnOnce(&dyn IoFS) -> R) -> Option<R> {
        if root == "/" {
            return Some(f(&self.fsys));
        }

        let p = tspath::remove_trailing_directory_separator(root);
        if !internal::valid_path(p) {
            if tspath::is_url(root) {
                return None;
            }
            panic!("vfs: failed to create sub file system for {p:?}: sub {p}: invalid argument");
        }
        if p == "." {
            return Some(f(&self.fsys));
        }
        Some(f(&SubFS { fsys: &self.fsys, dir: p }))
    }
}

// fs.Sub, lent by SubRoots for one operation.
struct SubFS<'a, T: IoFS> {
    fsys: &'a T,
    dir: &'a str,
}

impl<T: IoFS> SubFS<'_, T> {
    fn full_name(&self, name: &str) -> Result<String, FsError> {
        if !internal::valid_path(name) {
            return Err(FsError::Invalid);
        }
        if name == "." {
            return Ok(self.dir.to_string());
        }
        Ok(format!("{}/{}", self.dir, name))
    }
}

impl<T: IoFS> IoFS for SubFS<'_, T> {
    fn stat(&self, name: &str) -> Result<FileInfo, FsError> {
        let full = self.full_name(name)?;
        self.fsys.stat(&full)
    }

    fn read_dir(&self, name: &str) -> Result<Vec<IoDirEntry>, FsError> {
        let full = self.full_name(name)?;
        self.fsys.read_dir(&full)
    }

    fn read_file(&self, name: &str) -> Result<Vec<u8>, FsError> {
        let full = self.full_name(name)?;
        self.fsys.read_file(&full)
    }
}

pub struct IoVFS<T: IoFS> {
    common: Common<SubRoots<T>>,

    use_case_sensitive_file_names: bool,
}

fn cut_slash(path: &str) -> (&str, bool) {
    match path.strip_prefix('/') {
        Some(rest) => (rest, true),
        None => (path, false),
    }
}

impl<T: IoFS> IoVFS<T> {
    pub fn fsys(&self) -> &T {
        &self.common.root_for.fsys
    }

    fn realpath_impl(&self, path: &str) -> Result<String, FsError> {
        match self.fsys().as_realpath_fs() {
            Some(fsys) => {
                let (rest, had_slash) = cut_slash(path);
                let rp = fsys.realpath(rest)?;
                if had_slash {
                    return Ok(format!("/{rp}"));
                }
                Ok(rp)
            }
            None => Ok(path.to_string()),
        }
    }

    fn writable(&self, what: &str) -> &dyn WritableFS {
        match self.fsys().as_writable_fs() {
            Some(fsys) => fsys,
            None => panic!("{what} not supported"),
        }
    }

    fn write_file_impl(&self, path: &str, content: &str) -> Result<(), FsError> {
        let (rest, _) = cut_slash(path);
        self.writable("writeFile").write_file(rest, content, FileMode::from_bits_retain(0o666))
    }

    fn append_file_impl(&self, path: &str, content: &str) -> Result<(), FsError> {
        let (rest, _) = cut_slash(path);
        self.writable("appendFile").append_file(rest, content, FileMode::from_bits_retain(0o666))
    }

    fn mkdir_all_impl(&self, path: &str) -> Result<(), FsError> {
        let (rest, _) = cut_slash(path);
        self.writable("mkdirAll").mkdir_all(rest, FileMode::from_bits_retain(0o777))
    }

    fn write_file_ensuring_dir(
        &self,
        path: &str,
        content: &str,
        write: fn(&Self, &str, &str) -> Result<(), FsError>,
    ) -> Result<(), String> {
        let _ = internal::root_length(path); // Assert path is rooted
        if write(self, path, content).is_ok() {
            return Ok(());
        }
        self.mkdir_all_impl(&tspath::get_directory_path(&tspath::normalize_path(path)))?;
        write(self, path, content).map_err(String::from)
    }
}

impl<T: IoFS> FS for IoVFS<T> {
    fn use_case_sensitive_file_names(&self) -> bool {
        self.use_case_sensitive_file_names
    }

    fn directory_exists(&self, path: &str) -> bool {
        self.common.directory_exists(path)
    }

    fn file_exists(&self, path: &str) -> bool {
        self.common.file_exists(path)
    }

    fn get_accessible_entries(&self, path: &str) -> Entries {
        self.common.get_accessible_entries(path)
    }

    fn stat(&self, path: &str) -> Option<FileInfo> {
        let _ = internal::root_length(path); // Assert path is rooted
        self.common.stat(path)
    }

    fn read_file(&self, path: &str) -> Option<String> {
        self.common.read_file(path)
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        let _ = internal::root_length(path); // Assert path is rooted
        let (rest, _) = cut_slash(path);
        self.writable("remove").remove(rest).map_err(String::from)
    }

    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), String> {
        let _ = internal::root_length(path); // Assert path is rooted
        let (rest, _) = cut_slash(path);
        self.writable("chtimes").chtimes(rest, a_time, m_time).map_err(String::from)
    }

    fn realpath(&self, path: &str) -> String {
        let split = internal::split_path(path);
        // splitPath normalizes the path into parts (e.g. "c:/foo/bar" -> "c:/", "foo/bar")
        // Put them back together to call realpath.
        match self.realpath_impl(&format!("{}{}", split.root(), split.rest())) {
            Ok(realpath) => realpath,
            Err(_) => path.to_string(),
        }
    }

    fn write_file(&self, path: &str, content: &str) -> Result<(), String> {
        self.write_file_ensuring_dir(path, content, Self::write_file_impl)
    }

    fn append_file(&self, path: &str, content: &str) -> Result<(), String> {
        self.write_file_ensuring_dir(path, content, Self::append_file_impl)
    }
}
