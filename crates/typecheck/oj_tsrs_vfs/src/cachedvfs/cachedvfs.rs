use std::sync::atomic::{AtomicBool, Ordering};
use std::time::SystemTime;

use crate::FS as VFS;
use crate::{Entries, FileInfo};

// SyncMap is collections.SyncMap: a concurrent map with Load/Store/Clear, keyed by path strings and probed with
// `&str` (the sharded `tsrs_core::collections::SyncMap`: program construction probes these caches from every
// worker, vscode: 250k acquisitions in the 64-thread parse phase, notes/perf-frontend-64.md).
struct SyncMap<V> {
    m: tsrs_core::collections::SyncMap<String, V>,
}

impl<V: Clone> SyncMap<V> {
    fn new() -> Self {
        SyncMap { m: tsrs_core::collections::SyncMap::default() }
    }

    fn load(&self, key: &str) -> Option<V> {
        self.m.load(key)
    }

    fn store(&self, key: &str, value: V) {
        self.m.store(key.to_string(), value);
    }

    fn clear(&self) {
        self.m.clear();
    }
}

pub struct FS<T: VFS> {
    fs: T,
    enabled: AtomicBool,

    directory_exists_cache: SyncMap<bool>,
    file_exists_cache: SyncMap<bool>,
    get_accessible_entries_cache: SyncMap<Entries>,
    realpath_cache: SyncMap<String>,
    stat_cache: SyncMap<Option<FileInfo>>,
}

pub fn from<T: VFS>(fs: T) -> FS<T> {
    FS {
        fs,
        enabled: AtomicBool::new(true),
        directory_exists_cache: SyncMap::new(),
        file_exists_cache: SyncMap::new(),
        get_accessible_entries_cache: SyncMap::new(),
        realpath_cache: SyncMap::new(),
        stat_cache: SyncMap::new(),
    }
}

impl<T: VFS> FS<T> {
    pub fn disable_and_clear_cache(&self) {
        if self.enabled.compare_exchange(true, false, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            self.clear_cache();
        }
    }

    pub fn enable(&self) {
        self.enabled.store(true, Ordering::SeqCst);
    }

    pub fn clear_cache(&self) {
        self.directory_exists_cache.clear();
        self.file_exists_cache.clear();
        self.get_accessible_entries_cache.clear();
        self.realpath_cache.clear();
        self.stat_cache.clear();
    }

    fn enabled(&self) -> bool {
        self.enabled.load(Ordering::SeqCst)
    }
}

impl<T: VFS> VFS for FS<T> {
    fn directory_exists(&self, path: &str) -> bool {
        if self.enabled()
            && let Some(ret) = self.directory_exists_cache.load(path)
        {
            return ret;
        }

        let ret = self.fs.directory_exists(path);

        if self.enabled() {
            self.directory_exists_cache.store(path, ret);
        }

        ret
    }

    fn file_exists(&self, path: &str) -> bool {
        if self.enabled()
            && let Some(ret) = self.file_exists_cache.load(path)
        {
            return ret;
        }

        let ret = self.fs.file_exists(path);

        if self.enabled() {
            self.file_exists_cache.store(path, ret);
        }

        ret
    }

    fn get_accessible_entries(&self, path: &str) -> Entries {
        if self.enabled()
            && let Some(ret) = self.get_accessible_entries_cache.load(path)
        {
            return ret;
        }

        let ret = self.fs.get_accessible_entries(path);

        if self.enabled() {
            self.get_accessible_entries_cache.store(path, ret.clone());
        }

        ret
    }

    fn read_file(&self, path: &str) -> Option<String> {
        self.fs.read_file(path)
    }

    fn realpath(&self, path: &str) -> String {
        if self.enabled()
            && let Some(ret) = self.realpath_cache.load(path)
        {
            return ret;
        }

        let ret = self.fs.realpath(path);

        if self.enabled() {
            self.realpath_cache.store(path, ret.clone());
        }

        ret
    }

    fn remove(&self, path: &str) -> Result<(), String> {
        self.fs.remove(path)
    }

    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), String> {
        self.fs.chtimes(path, a_time, m_time)
    }

    fn stat(&self, path: &str) -> Option<FileInfo> {
        if self.enabled()
            && let Some(ret) = self.stat_cache.load(path)
        {
            return ret;
        }

        let ret = self.fs.stat(path);

        if self.enabled() {
            self.stat_cache.store(path, ret.clone());
        }

        ret
    }

    fn use_case_sensitive_file_names(&self) -> bool {
        self.fs.use_case_sensitive_file_names()
    }

    fn write_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.fs.write_file(path, data)
    }

    fn append_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.fs.append_file(path, data)
    }
}
