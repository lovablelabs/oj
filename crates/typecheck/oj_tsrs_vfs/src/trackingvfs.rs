// Port of Go `vfs/trackingvfs`: a wrapper that records every path read during a compilation, so watch mode knows
// which files and directories the program depended on, including paths that did not exist (failed module
// resolution lookups). Writes (outputs) are not recorded.

use std::time::SystemTime;

use tsrs_core::collections::SyncMap;

use crate::{Entries, FS as VFS, FileInfo};

pub struct FS<T: VFS> {
    inner: T,
    seen_files: SyncMap<String, ()>,
}

impl<T: VFS> FS<T> {
    pub fn new(inner: T) -> FS<T> {
        FS { inner, seen_files: SyncMap::default() }
    }

    /// Go `SeenFiles.Add`, for paths the caller depends on without reading them through this file system.
    pub fn add_seen_file(&self, path: &str) {
        self.see(path);
    }

    /// Go `SeenFiles.ToSlice()` (unordered).
    pub fn seen_files(&self) -> Vec<String> {
        self.seen_files.keys()
    }

    // Program construction asks the same paths many times from every parse worker: probe under the shard's read
    // lock before allocating the key.
    fn see(&self, path: &str) {
        if self.seen_files.load(path).is_none() {
            self.seen_files.store(path.to_string(), ());
        }
    }
}

impl<T: VFS> VFS for FS<T> {
    fn use_case_sensitive_file_names(&self) -> bool {
        self.inner.use_case_sensitive_file_names()
    }

    fn file_exists(&self, path: &str) -> bool {
        self.see(path);
        self.inner.file_exists(path)
    }

    fn read_file(&self, path: &str) -> Option<String> {
        self.see(path);
        self.inner.read_file(path)
    }

    fn write_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.inner.write_file(path, data)
    }

    fn append_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.inner.append_file(path, data)
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        self.inner.remove(path)
    }

    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), String> {
        self.inner.chtimes(path, a_time, m_time)
    }

    fn directory_exists(&self, path: &str) -> bool {
        self.see(path);
        self.inner.directory_exists(path)
    }

    fn get_accessible_entries(&self, path: &str) -> Entries {
        self.see(path);
        self.inner.get_accessible_entries(path)
    }

    fn stat(&self, path: &str) -> Option<FileInfo> {
        self.see(path);
        self.inner.stat(path)
    }

    fn realpath(&self, path: &str) -> String {
        self.see(path);
        self.inner.realpath(path)
    }
}
