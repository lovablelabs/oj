use std::fmt;
use std::time::SystemTime;

use bitflags::bitflags;
use rustc_hash::FxHashSet;

// FS is a file system abstraction.
pub trait FS: Send + Sync {
    // UseCaseSensitiveFileNames returns true if the file system is case-sensitive.
    fn use_case_sensitive_file_names(&self) -> bool;

    // FileExists returns true if the file exists.
    fn file_exists(&self, path: &str) -> bool;

    // ReadFile reads the file specified by path and returns the content.
    // If the file fails to be read, the result is None.
    fn read_file(&self, path: &str) -> Option<String>;

    fn write_file(&self, path: &str, data: &str) -> Result<(), String>;

    // AppendFile appends data to the file at path, creating it if it does not exist.
    fn append_file(&self, path: &str, data: &str) -> Result<(), String>;

    // Removes `path` and all its contents. Will return the first error it encounters.
    fn remove(&self, path: &str) -> Result<(), String>;

    // Chtimes changes the access and modification times of the named file.
    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), String>;

    // DirectoryExists returns true if the path is a directory.
    fn directory_exists(&self, path: &str) -> bool;

    // GetAccessibleEntries returns the files/directories in the specified directory.
    // If any entry is a symlink, it will be followed.
    fn get_accessible_entries(&self, path: &str) -> Entries;

    fn stat(&self, path: &str) -> Option<FileInfo>;

    // Realpath returns the "real path" of the specified path,
    // following symlinks and correcting filename casing.
    fn realpath(&self, path: &str) -> String;
}

macro_rules! forward_fs {
    () => {
        fn use_case_sensitive_file_names(&self) -> bool {
            (**self).use_case_sensitive_file_names()
        }
        fn file_exists(&self, path: &str) -> bool {
            (**self).file_exists(path)
        }
        fn read_file(&self, path: &str) -> Option<String> {
            (**self).read_file(path)
        }
        fn write_file(&self, path: &str, data: &str) -> Result<(), String> {
            (**self).write_file(path, data)
        }
        fn append_file(&self, path: &str, data: &str) -> Result<(), String> {
            (**self).append_file(path, data)
        }
        fn remove(&self, path: &str) -> Result<(), String> {
            (**self).remove(path)
        }
        fn chtimes(
            &self,
            path: &str,
            a_time: SystemTime,
            m_time: SystemTime,
        ) -> Result<(), String> {
            (**self).chtimes(path, a_time, m_time)
        }
        fn directory_exists(&self, path: &str) -> bool {
            (**self).directory_exists(path)
        }
        fn get_accessible_entries(&self, path: &str) -> Entries {
            (**self).get_accessible_entries(path)
        }
        fn stat(&self, path: &str) -> Option<FileInfo> {
            (**self).stat(path)
        }
        fn realpath(&self, path: &str) -> String {
            (**self).realpath(path)
        }
    };
}

impl<T: FS + ?Sized> FS for &T {
    forward_fs!();
}

impl<T: FS + ?Sized> FS for Box<T> {
    forward_fs!();
}

impl<T: FS + ?Sized> FS for std::sync::Arc<T> {
    forward_fs!();
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Entries {
    pub files: Vec<String>,
    pub directories: Vec<String>,
    // Symlinks contains the names of entries in Files or Directories that were
    // originally symbolic links (or reparse points) on disk. The names are the
    // same as those in Files/Directories (i.e., the link name, not the target).
    // None means symlink information is not available and the entries may need
    // to be re-checked for symlinks.
    pub symlinks: Option<FxHashSet<String>>,
}

bitflags! {
    // FileMode mirrors Go's io/fs.FileMode bit layout.
    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
    pub struct FileMode: u32 {
        const None = 0;
        const Dir = 1 << 31;
        const Append = 1 << 30;
        const Exclusive = 1 << 29;
        const Temporary = 1 << 28;
        const Symlink = 1 << 27;
        const Device = 1 << 26;
        const NamedPipe = 1 << 25;
        const Socket = 1 << 24;
        const Setuid = 1 << 23;
        const Setgid = 1 << 22;
        const CharDevice = 1 << 21;
        const Sticky = 1 << 20;
        const Irregular = 1 << 19;

        const Type = Self::Dir.bits() | Self::Symlink.bits() | Self::NamedPipe.bits() | Self::Socket.bits() | Self::Device.bits() | Self::CharDevice.bits() | Self::Irregular.bits();
        const Perm = 0o777;

        const _ = !0;
    }
}

impl FileMode {
    pub fn is_dir(self) -> bool {
        self.intersects(FileMode::Dir)
    }

    pub fn is_regular(self) -> bool {
        !self.intersects(FileMode::Type)
    }

    #[must_use]
    pub(crate) fn type_(self) -> FileMode {
        self & FileMode::Type
    }
}

// FileInfo is [fs.FileInfo].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileInfo {
    pub name: String,
    pub size: i64,
    pub mode: FileMode,
    pub mod_time: Option<SystemTime>,
}

impl FileInfo {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn size(&self) -> i64 {
        self.size
    }

    pub fn mode(&self) -> FileMode {
        self.mode
    }

    pub fn mod_time(&self) -> Option<SystemTime> {
        self.mod_time
    }

    pub fn is_dir(&self) -> bool {
        self.mode.is_dir()
    }
}

// The errors of Go's io/fs (ErrInvalid, ErrPermission, ErrExist, ErrNotExist, ErrClosed),
// plus the few structured errors the file systems in this crate distinguish.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FsError {
    Invalid,
    Permission,
    Exist,
    NotExist,
    Closed,
    BrokenSymlink { from: String, to: String },
    Other(String),
}

impl FsError {
    pub(crate) fn is_not_exist(&self) -> bool {
        matches!(self, FsError::NotExist)
    }
}

impl fmt::Display for FsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FsError::Invalid => f.write_str("invalid argument"),
            FsError::Permission => f.write_str("permission denied"),
            FsError::Exist => f.write_str("file already exists"),
            FsError::NotExist => f.write_str("file does not exist"),
            FsError::Closed => f.write_str("file already closed"),
            FsError::BrokenSymlink { from, to } => write!(f, "broken symlink {from:?} -> {to:?}"),
            FsError::Other(s) => f.write_str(s),
        }
    }
}

impl From<FsError> for String {
    fn from(e: FsError) -> String {
        e.to_string()
    }
}
