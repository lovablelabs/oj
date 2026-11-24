use crate::FileIdCache;
use file_id::{FileId, get_file_id};
use notify::{IgnoreFilter, RecursiveMode, WatchMode};
use rustc_hash::FxBuildHasher;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};
use walkdir::WalkDir;

/// A cache to hold the file system IDs of all watched files.
///
/// The file ID cache uses unique file IDs provided by the file system and is used to stitch together
/// rename events in case the notification back-end doesn't emit rename cookies.
#[derive(Debug, Clone, Default)]
pub struct FileIdMap {
    paths: HashMap<PathBuf, FileId, FxBuildHasher>,
    ignore_filter: IgnoreFilter,
}

impl FileIdMap {
    /// Construct an empty cache.
    #[must_use]
    pub fn new() -> Self {
        FileIdMap::default()
    }

    fn dir_scan_depth(is_recursive: bool) -> usize {
        if is_recursive { usize::MAX } else { 1 }
    }
}

impl FileIdCache for FileIdMap {
    fn cached_file_id(&self, path: &Path) -> Option<impl AsRef<FileId>> {
        self.paths.get(path)
    }

    fn add_path(&mut self, path: &Path, watch_mode: WatchMode) {
        // the watcher never reports ignored paths, so they are not cached
        if self.ignore_filter.is_path_ignored(path) {
            return;
        }

        let is_recursive = watch_mode.recursive_mode == RecursiveMode::Recursive;
        let ignore_filter = &self.ignore_filter;

        for (path, file_id) in WalkDir::new(path)
            .follow_links(true)
            .max_depth(Self::dir_scan_depth(is_recursive))
            .into_iter()
            .filter_entry(|entry| {
                entry.depth() == 0
                    || !ignore_filter.is_ignored(entry.path(), entry.file_type().into())
            })
            .filter_map(|entry| {
                let path = entry.ok()?.into_path();
                let file_id = get_file_id(&path).ok()?;
                Some((path, file_id))
            })
        {
            self.paths.insert(path, file_id);
        }
    }

    fn remove_path(&mut self, path: &Path) {
        self.paths.retain(|p, _| !p.starts_with(path));
    }

    fn set_ignore_filter(&mut self, ignore_filter: IgnoreFilter) {
        self.ignore_filter = ignore_filter;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn ignored_entries_are_not_cached() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("node_modules/pkg/index.js"), "").unwrap();
        std::fs::write(root.join("index.js"), "").unwrap();

        let visited = Arc::new(Mutex::new(Vec::new()));
        let mut cache = FileIdMap::new();
        cache.set_ignore_filter(IgnoreFilter::new({
            let visited = Arc::clone(&visited);
            move |path, _| {
                visited.lock().unwrap().push(path.to_path_buf());
                path.components()
                    .any(|component| component.as_os_str() == "node_modules")
            }
        }));
        cache.add_path(root, WatchMode::recursive());

        // the scan does not descend into the ignored directory
        let mut visited = std::mem::take(&mut *visited.lock().unwrap());
        visited.retain(|path| path != root && path.starts_with(root));
        visited.sort();
        assert_eq!(visited, [root.join("index.js"), root.join("node_modules")]);

        assert!(cache.cached_file_id(root).is_some());
        assert!(cache.cached_file_id(&root.join("index.js")).is_some());
        assert!(cache.cached_file_id(&root.join("node_modules")).is_none());
        assert!(
            cache
                .cached_file_id(&root.join("node_modules/pkg/index.js"))
                .is_none()
        );

        // adding an ignored path does nothing
        cache.add_path(&root.join("node_modules/pkg"), WatchMode::recursive());
        assert!(
            cache
                .cached_file_id(&root.join("node_modules/pkg/index.js"))
                .is_none()
        );
    }
}
