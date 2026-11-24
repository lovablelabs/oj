use std::path::{Path, PathBuf};

use file_id::FileId;
use notify::{IgnoreFilter, WatchMode};

/// The interface of a file ID cache.
///
/// This trait can be implemented for an existing cache, if it already holds `FileId`s.
pub trait FileIdCache {
    /// Get a `FileId` from the cache for a given `path`.
    ///
    /// If the path is not cached, `None` should be returned and there should not be any attempt to read the file ID from disk.
    fn cached_file_id(&self, path: &Path) -> Option<impl AsRef<FileId>>;

    /// Add a new path to the cache or update its value.
    ///
    /// This will be called if a new file or directory is created or if an existing file is overridden.
    fn add_path(&mut self, path: &Path, watch_mode: WatchMode);

    /// Remove a path from the cache.
    ///
    /// This will be called if a file or directory is deleted.
    fn remove_path(&mut self, path: &Path);

    /// Re-scan all `root_paths`.
    ///
    /// This will be called if the notification back-end has dropped events.
    /// The root paths are passed as argument, so the implementer doesn't have to store them.
    ///
    /// The default implementation calls `add_path` for each root path.
    fn rescan(&mut self, root_paths: &[(PathBuf, WatchMode)]) {
        for (path, watch_mode) in root_paths {
            self.add_path(path, *watch_mode);
        }
    }

    /// Sets the ignore filter of the watcher, see [`notify::Config::with_ignored`].
    ///
    /// The debouncer calls this once, before any path is added. A cache that scans directories
    /// should skip ignored entries, the watcher never reports them.
    ///
    /// The default implementation does nothing.
    fn set_ignore_filter(&mut self, _ignore_filter: IgnoreFilter) {}
}

/// An implementation of the `FileIdCache` trait that doesn't hold any data.
///
/// This pseudo cache can be used to disable the file tracking using file system IDs.
#[derive(Debug, Clone, Default)]
pub struct NoCache;

impl NoCache {
    /// Construct an empty cache.
    #[must_use]
    pub fn new() -> Self {
        NoCache
    }
}

impl FileIdCache for NoCache {
    fn cached_file_id(&self, _path: &Path) -> Option<impl AsRef<FileId>> {
        Option::<&FileId>::None
    }

    fn add_path(&mut self, _path: &Path, _watch_mode: WatchMode) {}

    fn remove_path(&mut self, _path: &Path) {}
}

/// The recommended file ID cache implementation for the current platform
#[cfg(any(target_os = "linux", target_os = "android", target_family = "wasm"))]
pub type RecommendedCache = NoCache;
/// The recommended file ID cache implementation for the current platform
#[cfg(not(any(target_os = "linux", target_os = "android", target_family = "wasm")))]
pub type RecommendedCache = crate::file_id_map::FileIdMap;
