// Go internal/vfs/wrapvfs.

use std::time::SystemTime;

use crate::{Entries, FS, FileInfo};

type Replacement<F> = Option<Box<F>>;

// wrapvfs.go:9
#[derive(Default)]
pub struct Replacements {
    pub use_case_sensitive_file_names: Replacement<dyn Fn() -> bool + Send + Sync>,
    pub file_exists: Replacement<dyn Fn(&str) -> bool + Send + Sync>,
    pub read_file: Replacement<dyn Fn(&str) -> Option<String> + Send + Sync>,
    pub write_file: Replacement<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>,
    pub append_file: Replacement<dyn Fn(&str, &str) -> Result<(), String> + Send + Sync>,
    pub remove: Replacement<dyn Fn(&str) -> Result<(), String> + Send + Sync>,
    pub chtimes:
        Replacement<dyn Fn(&str, SystemTime, SystemTime) -> Result<(), String> + Send + Sync>,
    pub directory_exists: Replacement<dyn Fn(&str) -> bool + Send + Sync>,
    pub get_accessible_entries: Replacement<dyn Fn(&str) -> Entries + Send + Sync>,
    pub stat: Replacement<dyn Fn(&str) -> Option<FileInfo> + Send + Sync>,
    pub realpath: Replacement<dyn Fn(&str) -> String + Send + Sync>,
}

// wrapvfs.go:23
pub fn wrap(fs: &'static dyn FS, replacements: Replacements) -> WrappedFS {
    WrappedFS { fs, replacements }
}

pub struct WrappedFS {
    fs: &'static dyn FS,
    replacements: Replacements,
}

impl FS for WrappedFS {
    // wrapvfs.go:36
    fn use_case_sensitive_file_names(&self) -> bool {
        if let Some(f) = &self.replacements.use_case_sensitive_file_names {
            return f();
        }
        self.fs.use_case_sensitive_file_names()
    }

    // wrapvfs.go:44
    fn file_exists(&self, path: &str) -> bool {
        if let Some(f) = &self.replacements.file_exists {
            return f(path);
        }
        self.fs.file_exists(path)
    }

    // wrapvfs.go:52
    fn read_file(&self, path: &str) -> Option<String> {
        if let Some(f) = &self.replacements.read_file {
            return f(path);
        }
        self.fs.read_file(path)
    }

    // wrapvfs.go:60
    fn write_file(&self, path: &str, data: &str) -> Result<(), String> {
        if let Some(f) = &self.replacements.write_file {
            return f(path, data);
        }
        self.fs.write_file(path, data)
    }

    // wrapvfs.go:68
    fn append_file(&self, path: &str, data: &str) -> Result<(), String> {
        if let Some(f) = &self.replacements.append_file {
            return f(path, data);
        }
        self.fs.append_file(path, data)
    }

    // wrapvfs.go:76
    fn remove(&self, path: &str) -> Result<(), String> {
        if let Some(f) = &self.replacements.remove {
            return f(path);
        }
        self.fs.remove(path)
    }

    // wrapvfs.go:84
    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), String> {
        if let Some(f) = &self.replacements.chtimes {
            return f(path, a_time, m_time);
        }
        self.fs.chtimes(path, a_time, m_time)
    }

    // wrapvfs.go:92
    fn directory_exists(&self, path: &str) -> bool {
        if let Some(f) = &self.replacements.directory_exists {
            return f(path);
        }
        self.fs.directory_exists(path)
    }

    // wrapvfs.go:100
    fn get_accessible_entries(&self, path: &str) -> Entries {
        if let Some(f) = &self.replacements.get_accessible_entries {
            return f(path);
        }
        self.fs.get_accessible_entries(path)
    }

    // wrapvfs.go:108
    fn stat(&self, path: &str) -> Option<FileInfo> {
        if let Some(f) = &self.replacements.stat {
            return f(path);
        }
        self.fs.stat(path)
    }

    // wrapvfs.go:116
    fn realpath(&self, path: &str) -> String {
        if let Some(f) = &self.replacements.realpath {
            return f(path);
        }
        self.fs.realpath(path)
    }
}
