use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::SystemTime;

use rustc_hash::FxHashMap;
use tsrs_core::collections::{Set, SyncMap, SyncSet};
use tsrs_core::new_work_group;
use tsrs_core::tspath::{self, Path};
use tsrs_ls::lsconv;
use tsrs_lsproto as lsproto;
use tsrs_vfs::{Entries, FS, FileInfo, cachedvfs};

use crate::dirty::{
    self, Cloneable, CloneableMap, FinalizationHooks, Shared, SharedMap, SyncMapEntry, Value,
};
use crate::filechange::{FileChangeExpander, FileChangeSummary};
use crate::overlayfs::{
    CachedFile, FileHandle, LayeredFileSystem, OverlayMap, ToPath, hash_string_128, new_cached_file,
};

pub trait FileHandleSource {
    fn get_file(&self, file_name: &str) -> Option<Arc<dyn FileHandle>>;
    fn get_file_by_path(&self, file_name: &str, path: &Path) -> Option<Arc<dyn FileHandle>>;
}

pub trait FileSource: FileHandleSource + Send + Sync {
    fn fs(&self) -> &dyn FS;
    fn file_exists(&self, file_name: &str, path: &Path) -> bool;
    fn get_accessible_entries(&self, path: &str) -> Entries;
}

pub struct CachedLayeredFileSystem {
    fs: cachedvfs::FS<Arc<dyn LayeredFileSystem>>,
    layered: Arc<dyn LayeredFileSystem>,
}

// snapshotfs.go:38
fn new_cached_layered_file_system(
    file_system: Arc<dyn LayeredFileSystem>,
) -> Arc<dyn LayeredFileSystem> {
    Arc::new(CachedLayeredFileSystem {
        fs: cachedvfs::from(Arc::clone(&file_system)),
        layered: file_system,
    })
}

impl FileHandleSource for CachedLayeredFileSystem {
    // snapshotfs.go:45
    fn get_file(&self, file_name: &str) -> Option<Arc<dyn FileHandle>> {
        self.layered.get_file(file_name)
    }

    // snapshotfs.go:49
    fn get_file_by_path(&self, file_name: &str, path: &Path) -> Option<Arc<dyn FileHandle>> {
        self.layered.get_file_by_path(file_name, path)
    }
}

impl LayeredFileSystem for CachedLayeredFileSystem {
    // snapshotfs.go:53
    fn overlays(&self) -> OverlayMap {
        self.layered.overlays()
    }

    fn as_file_change_expander(&self) -> Option<&dyn FileChangeExpander> {
        Some(self)
    }
}

impl FileChangeExpander for CachedLayeredFileSystem {
    // snapshotfs.go:57
    fn expand_file_changes(&self, change: FileChangeSummary) -> FileChangeSummary {
        if let Some(expander) = self.layered.as_file_change_expander() {
            return expander.expand_file_changes(change);
        }
        change
    }
}

// Go embeds `*cachedvfs.FS`.
impl FS for CachedLayeredFileSystem {
    fn use_case_sensitive_file_names(&self) -> bool {
        self.fs.use_case_sensitive_file_names()
    }
    fn file_exists(&self, path: &str) -> bool {
        self.fs.file_exists(path)
    }
    fn read_file(&self, path: &str) -> Option<String> {
        self.fs.read_file(path)
    }
    fn write_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.fs.write_file(path, data)
    }
    fn append_file(&self, path: &str, data: &str) -> Result<(), String> {
        self.fs.append_file(path, data)
    }
    fn remove(&self, path: &str) -> Result<(), String> {
        self.fs.remove(path)
    }
    fn chtimes(&self, path: &str, a_time: SystemTime, m_time: SystemTime) -> Result<(), String> {
        self.fs.chtimes(path, a_time, m_time)
    }
    fn directory_exists(&self, path: &str) -> bool {
        self.fs.directory_exists(path)
    }
    fn get_accessible_entries(&self, path: &str) -> Entries {
        self.fs.get_accessible_entries(path)
    }
    fn stat(&self, path: &str) -> Option<FileInfo> {
        self.fs.stat(path)
    }
    fn realpath(&self, path: &str) -> String {
        self.fs.realpath(path)
    }
}

// realpathAliasSet is a thread-safe set of symlink paths that alias a single realpath.
// It implements dirty.Cloneable so it can be used as a value in dirty.SyncMap.
// (Go guards `paths` with a mutex; here it is only mutated through `&mut` inside a dirty-map change.)
#[derive(Clone, Default)]
pub struct RealpathAliasSet {
    paths: Set<Path>,
}

impl RealpathAliasSet {
    // snapshotfs.go:76
    fn add(&mut self, path: Path) {
        self.paths.add(path);
    }
}

impl Cloneable for RealpathAliasSet {
    // snapshotfs.go:82
    fn clone_value(&self) -> RealpathAliasSet {
        let mut clone = RealpathAliasSet::default();
        if !self.paths.is_empty() {
            clone.paths = self.paths.clone();
        }
        clone
    }
}

pub type CacheDirectories = SharedMap<Path, CloneableMap<Path, String>>;

pub struct SnapshotFS {
    to_path: ToPath,
    pub(crate) fs: Arc<dyn LayeredFileSystem>,
    pub(crate) cache_files: SharedMap<Path, CachedFile>,
    pub(crate) cache_directories: CacheDirectories,
    read_files: SyncMap<Path, MemoizedCachedFile>,
    // nodeModulesRealpathAliases maps realpath-based keys to sets of symlink-based keys,
    // for files inside node_modules that are accessed through directory symlinks.
    // This allows watch events (which use realpaths) to invalidate files cached under symlink paths.
    pub(crate) node_modules_realpath_aliases: SharedMap<Path, RealpathAliasSet>,
}

// Go `func() FileHandle` built with `sync.OnceValue`.
type MemoizedCachedFile = Arc<OnceLock<Option<Arc<dyn FileHandle>>>>;

pub(crate) fn new_snapshot_fs(to_path: ToPath, fs: Arc<dyn LayeredFileSystem>) -> SnapshotFS {
    SnapshotFS {
        to_path,
        fs,
        cache_files: Arc::default(),
        cache_directories: Arc::default(),
        read_files: SyncMap::default(),
        node_modules_realpath_aliases: Arc::default(),
    }
}

impl FileHandleSource for SnapshotFS {
    // snapshotfs.go:110
    fn get_file(&self, file_name: &str) -> Option<Arc<dyn FileHandle>> {
        self.get_file_by_path(file_name, &(self.to_path)(file_name))
    }

    // snapshotfs.go:121
    fn get_file_by_path(&self, file_name: &str, path: &Path) -> Option<Arc<dyn FileHandle>> {
        if let Some(file) = self.cache_files.get(path) {
            return Some(Arc::<CachedFile>::clone(file.arc()));
        }
        let new_entry: MemoizedCachedFile = Arc::new(OnceLock::new());
        let (entry, _) = self.read_files.load_or_store(path.clone(), new_entry);
        entry.get_or_init(|| self.fs.get_file_by_path(file_name, path)).clone()
    }
}

impl FileSource for SnapshotFS {
    // snapshotfs.go:106
    fn fs(&self) -> &dyn FS {
        &*self.fs
    }

    // snapshotfs.go:114
    fn file_exists(&self, file_name: &str, path: &Path) -> bool {
        if self.cache_files.contains_key(path) {
            return true;
        }
        self.fs.file_exists(file_name)
    }

    // snapshotfs.go:132
    fn get_accessible_entries(&self, directory_name: &str) -> Entries {
        let lower_entries = self.fs.get_accessible_entries(directory_name);
        let Some(directory) = self.cache_directories.get(&(self.to_path)(directory_name)) else {
            return lower_entries;
        };
        merge_cached_directory_entries(
            &lower_entries,
            directory,
            |path| self.cache_files.contains_key(path) || self.fs.file_exists(path),
            self.fs.use_case_sensitive_file_names(),
        )
    }
}

// snapshotfs.go:144
fn merge_cached_directory_entries(
    directory_entries: &Entries,
    cached_entries: &CloneableMap<Path, String>,
    is_cached_file: impl Fn(&Path) -> bool,
    use_case_sensitive_file_names: bool,
) -> Entries {
    let mut entries =
        Entries { symlinks: directory_entries.symlinks.clone(), ..Default::default() };
    let equal_name = |left: &str, right: &str| {
        tspath::get_canonical_file_name(left, use_case_sensitive_file_names)
            == tspath::get_canonical_file_name(right, use_case_sensitive_file_names)
    };
    let has_name =
        |names: &[String], name: &str| names.iter().any(|candidate| equal_name(candidate, name));
    #[expect(
        clippy::iter_over_hash_type,
        reason = "Go ranges the cached entry map too, so cached children are appended in map order there as well"
    )]
    for (child_path, child_name) in cached_entries.iter() {
        if let Some(symlinks) = &mut entries.symlinks {
            symlinks.retain(|name| !equal_name(name, child_name));
        }
        if is_cached_file(child_path) {
            entries.files.push(child_name.clone());
        } else {
            entries.directories.push(child_name.clone());
        }
    }
    for file_name in &directory_entries.files {
        if !has_name(&entries.files, file_name) && !has_name(&entries.directories, file_name) {
            entries.files.push(file_name.clone());
        }
    }
    for directory_name in &directory_entries.directories {
        if !has_name(&entries.files, directory_name)
            && !has_name(&entries.directories, directory_name)
        {
            entries.directories.push(directory_name.clone());
        }
    }
    entries
}

pub struct SnapshotFSBuilder {
    pub(crate) fs: Arc<dyn LayeredFileSystem>,
    pub(crate) cache_files: dirty::SyncMap<Path, CachedFile>,
    cache_directories: dirty::Map<Path, CloneableMap<Path, String>>,
    source_backed_replacements: Mutex<Set<Path>>,
    node_modules_realpath_aliases: dirty::SyncMap<Path, RealpathAliasSet>,
    pub(crate) to_path: ToPath,
}

// snapshotfs.go:186
pub(crate) fn new_snapshot_fs_builder_from_source(
    fs: Arc<dyn LayeredFileSystem>,
    cache_files: SharedMap<Path, CachedFile>,
    cache_directories: CacheDirectories,
    node_modules_realpath_aliases: SharedMap<Path, RealpathAliasSet>,
    to_path: ToPath,
) -> SnapshotFSBuilder {
    let fs = new_cached_layered_file_system(fs);

    SnapshotFSBuilder {
        fs,
        cache_files: dirty::new_sync_map(cache_files),
        cache_directories: dirty::new_map(cache_directories),
        source_backed_replacements: Mutex::new(Set::default()),
        node_modules_realpath_aliases: dirty::new_sync_map(node_modules_realpath_aliases),
        to_path,
    }
}

impl SnapshotFSBuilder {
    pub(crate) fn to_path(&self, file_name: &str) -> Path {
        (self.to_path)(file_name)
    }

    fn on_added_file(&self, path: &Path, file_name: &str) {
        let mut child_path = path.clone();
        let mut child = file_name.to_string();
        loop {
            let parent_path = child_path.get_directory_path();
            let parent = tspath::get_directory_path(&child);
            if child_path == parent_path {
                break; // reached root
            }
            let base_name = tspath::get_base_file_name(&child);
            if let Some(dir_entry) = self.cache_directories.get(&parent_path) {
                let cp = child_path;
                dir_entry.change(|dir| {
                    dir.insert(cp, base_name);
                });
                break;
            }
            let mut dir = CloneableMap::default();
            dir.insert(child_path.clone(), base_name);
            self.cache_directories.add(parent_path.clone(), Shared::new(dir));
            child_path = parent_path;
            child = parent;
        }
    }

    fn on_deleted_file_or_directory(&self, path: &Path) {
        let Some(dir_entry) = self.cache_directories.get(&path.get_directory_path()) else {
            return;
        };
        let mut now_empty = false;
        dir_entry.change(|dir| {
            dir.remove(path);
            now_empty = dir.is_empty();
        });
        // Go deletes the entry and recurses from inside the Change callback.
        if now_empty {
            dir_entry.delete();
            self.on_deleted_file_or_directory(&dir_entry.key());
        }
    }

    // snapshotfs.go:208
    pub(crate) fn finalize(&self) -> (SnapshotFS, bool) {
        // Synchronize directory structure based on added and deleted cache entries.
        let mut deleted: Option<FxHashMap<Path, Option<Shared<CachedFile>>>> = None;
        let mut added: Vec<(Path, String)> = Vec::new();

        let source_backed_replacements = self.source_backed_replacements.lock().unwrap().clone();
        let mut on_delete = |key: &Path, value: Option<&Shared<CachedFile>>| {
            if source_backed_replacements.has(key) {
                return;
            }
            deleted.get_or_insert_with(FxHashMap::default).insert(key.clone(), value.cloned());
        };
        let mut on_add = |key: &Path, value: Option<&Shared<CachedFile>>| {
            added.push((key.clone(), value.unwrap().base.file_name.clone()));
        };
        let (cache_files, changed) = self.cache_files.finalize_with(FinalizationHooks {
            on_delete: Some(&mut on_delete),
            on_add: Some(&mut on_add),
            on_change: None,
        });
        // Go updates the directory map from inside the OnAdd hook.
        for (key, file_name) in &added {
            self.on_added_file(key, file_name);
        }

        if let Some(deleted) = &deleted {
            #[expect(
                clippy::iter_over_hash_type,
                reason = "removes each path from its parent dir and prunes emptied dirs; the end state is order-free; Go ranges the map too"
            )]
            for path in deleted.keys() {
                self.on_deleted_file_or_directory(path);
            }
        }

        // Prune deleted symlink paths from realpath alias sets before finalizing,
        // so that empty sets are dropped during finalization.
        if let Some(deleted) = &deleted {
            #[expect(
                clippy::iter_over_hash_type,
                reason = "per-path removal from alias sets; commutes; Go ranges the map too"
            )]
            for (deleted_path, deleted_file) in deleted {
                let Some(deleted_file) = deleted_file else {
                    continue;
                };
                if deleted_file.realpath_path.0.is_empty() {
                    continue;
                }
                if let Some(entry) =
                    self.node_modules_realpath_aliases.load(&deleted_file.realpath_path)
                {
                    entry.locked(&mut |e: &dyn Value<RealpathAliasSet>| {
                        e.change(&mut |alias_set: &mut RealpathAliasSet| {
                            alias_set.paths.delete(deleted_path);
                        });
                        if e.value().unwrap().paths.is_empty() {
                            e.delete();
                        }
                    });
                }
            }
        }

        let (node_modules_realpath_aliases, aliases_changed) =
            self.node_modules_realpath_aliases.finalize();

        (
            SnapshotFS {
                fs: Arc::clone(&self.fs),
                cache_files,
                cache_directories: self.cache_directories.finalize().0,
                read_files: SyncMap::default(),
                node_modules_realpath_aliases,
                to_path: Arc::clone(&self.to_path),
            },
            changed || aliases_changed,
        )
    }

    // snapshotfs.go:305
    pub(crate) fn delete_cache_entry(&self, entry: &Arc<SyncMapEntry<Path, CachedFile>>) {
        if let Some(file) = entry.value()
            && self.fs.file_exists(&file.base.file_name)
        {
            self.source_backed_replacements.lock().unwrap().add(entry.key());
        }
        entry.delete();
    }

    // snapshotfs.go:348
    fn cache_source_file(
        &self,
        file_name: &str,
        path: &Path,
        source: &dyn FileHandle,
    ) -> Option<Arc<dyn FileHandle>> {
        let mut file = new_cached_file(file_name.to_string(), source.content().to_string());
        file.base.hash = source.hash();
        let (entry, loaded) = self.cache_files.load_or_store(path.clone(), Shared::new(file));
        let entry = entry?;
        if !loaded && path.contains("/node_modules/") {
            self.record_realpath_alias(&entry, file_name, path);
        }
        self.reload_entry_if_needed(&entry)
    }

    // snapshotfs.go:361
    fn get_cached_file(
        &self,
        file_name: &str,
        path: &Path,
        force_reload: bool,
    ) -> Option<Arc<dyn FileHandle>> {
        let placeholder = CachedFile {
            base: crate::overlayfs::FileBase::new(file_name.to_string(), String::new(), 0),
            needs_reload: true,
            realpath_path: Path::default(),
        };
        let (entry, loaded) =
            self.cache_files.load_or_store(path.clone(), Shared::new(placeholder));
        if let Some(entry) = entry {
            if !loaded && path.contains("/node_modules/") {
                self.record_realpath_alias(&entry, file_name, path);
            }
            if force_reload {
                return self.reload_entry(&entry);
            }
            return self.reload_entry_if_needed(&entry);
        }
        None
    }

    // snapshotfs.go:378
    // recordRealpathAlias checks if fileName is accessed through a symlink and, if so,
    // records a mapping from the realpath-based key to the symlink-based key.
    // This is only called for files inside node_modules where symlinks are common.
    fn record_realpath_alias(
        &self,
        cached_file_entry: &Arc<SyncMapEntry<Path, CachedFile>>,
        symlink_file_name: &str,
        symlink_path: &Path,
    ) {
        let realpath = self.fs.realpath(symlink_file_name);
        let realpath_path = (self.to_path)(&realpath);
        if realpath_path != *symlink_path {
            let rp = realpath_path.clone();
            cached_file_entry.change(|file| {
                file.realpath_path = rp;
            });
            let (entry, _) = self
                .node_modules_realpath_aliases
                .load_or_store(realpath_path, Shared::new(RealpathAliasSet::default()));
            if let Some(entry) = entry {
                let sp = symlink_path.clone();
                entry.change(|alias_set| {
                    alias_set.add(sp);
                });
            }
        }
    }

    // snapshotfs.go:392
    fn reload_entry(
        &self,
        entry: &Arc<SyncMapEntry<Path, CachedFile>>,
    ) -> Option<Arc<dyn FileHandle>> {
        let mut file_name = String::new();
        entry.locked(&mut |e: &dyn Value<CachedFile>| {
            if let Some(v) = e.value() {
                file_name.clone_from(&v.base.file_name);
            }
        });
        if file_name.is_empty() {
            return None;
        }
        // Read file outside the lock to avoid blocking other goroutines.
        let content = self.fs.read_file(&file_name);
        entry.locked(&mut |e: &dyn Value<CachedFile>| {
            if e.value().is_none() {
                return;
            }
            if let Some(content) = &content {
                e.change(&mut |file: &mut CachedFile| {
                    file.base = crate::overlayfs::FileBase::new(
                        file.base.file_name.clone(),
                        content.clone(),
                        hash_string_128(content),
                    );
                    file.needs_reload = false;
                });
            } else {
                e.delete();
            }
        });
        entry.value().map(|v| Arc::clone(v.arc()) as Arc<dyn FileHandle>)
    }

    // snapshotfs.go:424
    pub(crate) fn reload_entry_if_needed(
        &self,
        entry: &Arc<SyncMapEntry<Path, CachedFile>>,
    ) -> Option<Arc<dyn FileHandle>> {
        let mut file_name = String::new();
        entry.locked(&mut |e: &dyn Value<CachedFile>| {
            if let Some(v) = e.value()
                && !v.matches_disk_text()
            {
                file_name.clone_from(&v.base.file_name);
            }
        });
        if !file_name.is_empty() {
            // Read file outside the lock to avoid blocking other goroutines.
            let content = self.fs.read_file(&file_name);
            entry.locked(&mut |e: &dyn Value<CachedFile>| {
                match e.value() {
                    None => return,
                    Some(v) if v.matches_disk_text() => return, // another goroutine already reloaded it
                    Some(_) => {}
                }
                if let Some(content) = &content {
                    e.change(&mut |file: &mut CachedFile| {
                        file.base = crate::overlayfs::FileBase::new(
                            file.base.file_name.clone(),
                            content.clone(),
                            hash_string_128(content),
                        );
                        file.needs_reload = false;
                    });
                } else {
                    e.delete();
                }
            });
        }
        entry.value().map(|v| Arc::clone(v.arc()) as Arc<dyn FileHandle>)
    }

    // snapshotfs.go:455
    pub(crate) fn watch_changes_overlap_cache(
        &self,
        change: &FileChangeSummary,
        previous_open_files: &FxHashMap<Path, Arc<dyn FileHandle>>,
        open_files: &FxHashMap<Path, Arc<dyn FileHandle>>,
    ) -> bool {
        // pure lookup any(); returns a bool only; Go ranges the set too
        if change.changed.keys().iter().any(|uri| {
            let path = (self.to_path)(&uri.file_name());
            if previous_open_files.contains_key(&path) || open_files.contains_key(&path) {
                return true;
            }
            if self.cache_files.load(&path).is_some() {
                return true;
            }
            self.node_modules_realpath_aliases.load(&path).is_some()
        }) {
            return true;
        }
        // pure lookup any(); returns a bool only; Go ranges the set too
        if change.deleted.keys().iter().any(|uri| {
            let path = (self.to_path)(&uri.file_name());
            if previous_open_files.contains_key(&path) || open_files.contains_key(&path) {
                return true;
            }
            if self.cache_files.load(&path).is_some() {
                return true;
            }
            self.node_modules_realpath_aliases.load(&path).is_some()
        }) {
            return true;
        }
        false
    }

    // snapshotfs.go:483
    pub(crate) fn invalidate_cache(&self) {
        self.cache_files.range(|entry| {
            entry.change(|file| {
                file.needs_reload = true;
            });
            true
        });
    }

    // snapshotfs.go:492
    pub(crate) fn invalidate_node_modules_cache(&self) {
        self.cache_files.range(|entry| {
            if entry.key().contains("/node_modules/") {
                entry.change(|file| {
                    file.needs_reload = true;
                });
            }
            true
        });
    }

    // snapshotfs.go:503
    pub(crate) fn mark_dirty_files(&self, mut change: FileChangeSummary) -> FileChangeSummary {
        if !change.changed.is_empty() {
            let filtered_changed: SyncSet<lsproto::DocumentUri> = SyncSet::default();
            let wg = new_work_group(false);
            #[expect(
                clippy::iter_over_hash_type,
                reason = "per-entry reload plus a set insert; Go runs these in parallel"
            )]
            for uri in change.changed.keys() {
                let path = (self.to_path)(&uri.file_name());
                if let Some(file) = self.fs.get_file_by_path(&uri.file_name(), &path)
                    && file.is_overlay()
                {
                    filtered_changed.add(uri.clone());
                    continue;
                }
                let Some(entry) = self.cache_files.load(&path) else {
                    filtered_changed.add(uri.clone());
                    continue;
                };
                let filtered_changed = &filtered_changed;
                let uri = uri.clone();
                wg.queue(move || {
                    if self.reload_entry_if_content_changed(&entry) {
                        filtered_changed.add(uri);
                    }
                });
            }
            wg.run_and_wait();
            drop(wg);
            let mut new_changed =
                tsrs_core::collections::new_set_with_size_hint(filtered_changed.size());
            new_changed.m.extend(filtered_changed.keys());
            change.changed = new_changed;
        }
        #[expect(
            clippy::iter_over_hash_type,
            reason = "only deletes cache entries; Go ranges the set too"
        )]
        for uri in change.deleted.keys() {
            let path = (self.to_path)(&uri.file_name());
            if let Some(entry) = self.cache_files.load(&path) {
                self.delete_cache_entry(&entry);
            }
        }
        change
    }

    // snapshotfs.go:540
    fn reload_entry_if_content_changed(&self, entry: &Arc<SyncMapEntry<Path, CachedFile>>) -> bool {
        let Some(file) = entry.value() else {
            return true;
        };
        let content = self.fs.read_file(&file.base.file_name);
        let mut changed = true;
        entry.locked(&mut |e: &dyn Value<CachedFile>| {
            let Some(cur) = e.value() else {
                return;
            };
            let Some(content) = &content else {
                e.delete();
                return;
            };
            if *content == cur.base.content {
                changed = false;
                if !cur.matches_disk_text() {
                    e.change(&mut |file: &mut CachedFile| {
                        file.needs_reload = false;
                    });
                }
                return;
            }
            e.change(&mut |file: &mut CachedFile| {
                file.base = crate::overlayfs::FileBase::new(
                    file.base.file_name.clone(),
                    content.clone(),
                    hash_string_128(content),
                );
                file.needs_reload = false;
            });
        });
        changed
    }

    // snapshotfs.go:615
    // isRelevantFileName returns true if the given URI refers to a file that
    // could affect the project: it has a TypeScript-relevant or configured content-mapper extension,
    // is dynamic (e.g. untitled), or is present in the supplied open-file state.
    fn is_relevant_file_name(
        &self,
        uri: &lsproto::DocumentUri,
        content_mapper_extensions: &[String],
        content_mapper_watched_files: Option<&Set<Path>>,
        open_files: &FxHashMap<Path, Arc<dyn FileHandle>>,
    ) -> bool {
        let file_name = uri.file_name();
        if content_mapper_watched_files.is_some_and(|w| w.has(&(self.to_path)(&file_name))) {
            return true;
        }
        let extensions: Vec<&str> =
            content_mapper_extensions.iter().map(std::string::String::as_str).collect();
        if tspath::file_extension_is_one_of(&file_name, &extensions) {
            return true;
        }
        if tspath::is_dynamic_file_name(&file_name) {
            return true;
        }
        let path = (self.to_path)(&file_name);
        if open_files.contains_key(&path) {
            return true;
        }
        let Some(i) = path.rfind('.') else {
            return false;
        };
        is_relevant_extension(&path[i..])
    }

    // snapshotfs.go:651
    // expandAndFilterWatchEvents expands directory deletion URIs into individual
    // file deletion URIs using the cached directory structure, and filters out
    // watch events for paths that are neither known directories nor have relevant
    // file extensions.
    pub(crate) fn expand_and_filter_watch_events(
        &self,
        mut change: FileChangeSummary,
        content_mapper_extensions: &[String],
        content_mapper_watched_files: Option<&Set<Path>>,
        previous_open_files: &FxHashMap<Path, Arc<dyn FileHandle>>,
        open_files: &FxHashMap<Path, Arc<dyn FileHandle>>,
    ) -> FileChangeSummary {
        if !change.deleted.is_empty() {
            let mut filtered_deleted: Set<lsproto::DocumentUri> = Set::default();
            #[expect(
                clippy::iter_over_hash_type,
                reason = "filters into a new set; pure inserts; Go ranges the set too"
            )]
            for uri in change.deleted.keys() {
                let path = (self.to_path)(&uri.file_name());
                if self.cache_directories.get(&path).is_some()
                    || has_open_file_within(&path, previous_open_files, open_files)
                {
                    self.collect_files_recursive(
                        &path,
                        &mut filtered_deleted,
                        previous_open_files,
                        open_files,
                    );
                } else if self.is_relevant_file_name(
                    uri,
                    content_mapper_extensions,
                    content_mapper_watched_files,
                    open_files,
                ) || is_node_modules_path(&path)
                {
                    // node_modules deletions must always be preserved for auto-import registry change handlers.
                    // They won't be in cacheDirectories since the registry doesn't use the snapshotFSBuilder for
                    // its file system, since we don't want to retain files read there.
                    filtered_deleted.add(uri.clone());
                }
            }
            change.deleted = filtered_deleted;
        }

        if !change.changed.is_empty() {
            let mut filtered_changed: Set<lsproto::DocumentUri> = Set::default();
            // filters into a new set; pure inserts; Go ranges the set too
            filtered_changed.m.extend(change.changed.keys().iter().filter_map(|uri| {
                if self.is_relevant_file_name(
                    uri,
                    content_mapper_extensions,
                    content_mapper_watched_files,
                    open_files,
                ) {
                    Some(uri.clone())
                } else {
                    None
                }
            }));
            change.changed = filtered_changed;
        }

        // We can't filter created events because any created path could be a directory symlink
        // that includes relevant files. configFileRegistryBuilder will do check if these paths
        // are directories if they fall within a config's wildcard directories.

        change
    }

    // snapshotfs.go:709
    // collectFilesRecursive recursively collects all cached file URIs under the
    // given directory path using the cacheDirectories and cacheFiles maps.
    fn collect_files_recursive(
        &self,
        dir_path: &Path,
        files: &mut Set<lsproto::DocumentUri>,
        previous_open_files: &FxHashMap<Path, Arc<dyn FileHandle>>,
        open_files: &FxHashMap<Path, Arc<dyn FileHandle>>,
    ) {
        // pure set inserts; Go ranges the map too
        files.m.extend(open_files.iter().filter_map(|(path, file)| {
            if dir_path.contains_path(path) {
                Some(lsconv::file_name_to_document_uri(file.file_name()))
            } else {
                None
            }
        }));
        // pure set inserts; Go ranges the map too
        files.m.extend(previous_open_files.iter().filter_map(|(path, file)| {
            if dir_path.contains_path(path) {
                Some(lsconv::file_name_to_document_uri(file.file_name()))
            } else {
                None
            }
        }));
        let Some(dir_entry) = self.cache_directories.get(dir_path) else {
            return;
        };
        let children: Vec<Path> =
            dir_entry.value().map(|v| v.keys().cloned().collect()).unwrap_or_default();
        for child_path in children {
            if let Some(entry) = self.cache_files.load(&child_path)
                && let Some(file) = entry.value()
            {
                files.add(lsconv::file_name_to_document_uri(&file.base.file_name));
            }
            self.collect_files_recursive(&child_path, files, previous_open_files, open_files);
        }
    }

    // snapshotfs.go:734
    pub(crate) fn convert_open_and_close_to_changes(
        &self,
        mut change: FileChangeSummary,
        previous_open_files: &FxHashMap<Path, Arc<dyn FileHandle>>,
        open_files: &FxHashMap<Path, Arc<dyn FileHandle>>,
    ) -> FileChangeSummary {
        if !change.opened.0.is_empty() && !tspath::is_dynamic_file_name(&change.opened.file_name())
        {
            let path = (self.to_path)(&change.opened.file_name());
            let entry = self.cache_files.load(&path);
            match &entry {
                Some(entry) if entry.original().is_some() => {
                    if let Some(open_file) = open_files.get(&path) {
                        // The file already exists in the program, but the open-file content from
                        // didOpen may differ from what was originally read from the source (e.g. the
                        // editor normalizes line endings, or the source file changed since the
                        // project was loaded). Mark it as Changed so the project rebuilds.
                        if let Some(cached_file) = entry.original()
                            && open_file.hash() != cached_file.base.hash
                        {
                            change.changed.add(change.opened.clone());
                        }
                        self.delete_cache_entry(entry);
                    }
                }
                _ => {
                    change.created.add(change.opened.clone());
                }
            }
        }
        let closed: Vec<lsproto::DocumentUri> = change.closed.keys().iter().cloned().collect();
        for uri in closed {
            let file_name = uri.file_name();
            if tspath::is_dynamic_file_name(&file_name) {
                continue;
            }
            let path = (self.to_path)(&file_name);
            // We may have ignored watcher events while the file was open, so force a reload.
            if let Some(fh) = self.get_cached_file(&file_name, &path, true /*forceReload*/) {
                if let Some(previous_open_file) = previous_open_files.get(&path)
                    && fh.hash() != previous_open_file.hash()
                {
                    change.changed.add(uri.clone());
                }
                continue;
            }
            change.deleted.add(uri);
        }
        change
    }
}

impl FileHandleSource for SnapshotFSBuilder {
    // snapshotfs.go:300
    fn get_file(&self, file_name: &str) -> Option<Arc<dyn FileHandle>> {
        let path = (self.to_path)(file_name);
        self.get_file_by_path(file_name, &path)
    }

    // snapshotfs.go:325
    fn get_file_by_path(&self, file_name: &str, path: &Path) -> Option<Arc<dyn FileHandle>> {
        if let Some(entry) = self.cache_files.load(path) {
            return self.reload_entry_if_needed(&entry);
        }
        let file = self.fs.get_file_by_path(file_name, path)?;
        if file.is_overlay() {
            return Some(file);
        }
        self.cache_source_file(file_name, path, &*file)
    }
}

impl FileSource for SnapshotFSBuilder {
    // snapshotfs.go:204
    fn fs(&self) -> &dyn FS {
        &*self.fs
    }

    // snapshotfs.go:312
    fn file_exists(&self, file_name: &str, path: &Path) -> bool {
        if let Some(entry) = self.cache_files.load(path) {
            if entry.value().is_none() {
                return false;
            }
            // Entry may be dirty - reload to check current state in the source filesystem.
            return self.reload_entry_if_needed(&entry).is_some();
        }
        // Path never loaded into cacheFiles - use cached stat (no file read).
        self.fs.file_exists(file_name)
    }

    // snapshotfs.go:336
    fn get_accessible_entries(&self, path: &str) -> Entries {
        let lower_entries = self.fs.get_accessible_entries(path);
        let Some(directory) = self.cache_directories.get(&(self.to_path)(path)) else {
            return lower_entries;
        };
        let directory = directory.value().unwrap();
        merge_cached_directory_entries(
            &lower_entries,
            &directory,
            |path| {
                self.cache_files.load(path).is_some_and(|entry| entry.value().is_some())
                    || self.fs.file_exists(path)
            },
            self.fs.use_case_sensitive_file_names(),
        )
    }
}

impl SnapshotFS {
    // snapshotfs.go:578
    // expandRealpathAliases adds synthetic URIs to the Changed and Deleted sets for
    // files that were accessed through node_modules symlinks. When a watch event arrives
    // using a realpath, this expands it to include the symlink-based path so that
    // downstream consumers (markDirtyFiles, markFilesChanged) can find cached entries.
    pub(crate) fn expand_realpath_aliases(
        &self,
        mut change: FileChangeSummary,
    ) -> FileChangeSummary {
        if self.node_modules_realpath_aliases.is_empty() {
            return change;
        }

        let mut additional_changed: Set<lsproto::DocumentUri> = Set::default();
        #[expect(clippy::iter_over_hash_type, reason = "pure set unions; Go ranges the set too")]
        for uri in change.changed.keys() {
            let path = (self.to_path)(&uri.file_name());
            if let Some(aliases) = self.node_modules_realpath_aliases.get(&path) {
                additional_changed.m.extend(
                    aliases
                        .paths
                        .keys()
                        .iter()
                        .map(|alias_path| lsconv::file_name_to_document_uri(alias_path)),
                );
            }
        }
        // pure set unions; Go ranges the set too
        change.changed.m.extend(additional_changed.keys().iter().map(std::clone::Clone::clone));

        let mut additional_deleted: Set<lsproto::DocumentUri> = Set::default();
        #[expect(clippy::iter_over_hash_type, reason = "pure set unions; Go ranges the set too")]
        for uri in change.deleted.keys() {
            let path = (self.to_path)(&uri.file_name());
            if let Some(aliases) = self.node_modules_realpath_aliases.get(&path) {
                additional_deleted.m.extend(
                    aliases
                        .paths
                        .keys()
                        .iter()
                        .map(|alias_path| lsconv::file_name_to_document_uri(alias_path)),
                );
            }
        }
        // pure set unions; Go ranges the set too
        change.deleted.m.extend(additional_deleted.keys().iter().map(std::clone::Clone::clone));

        change
    }
}

// snapshotfs.go:639
// isRelevantExtension returns true if the given extension is a known TypeScript
// or JavaScript extension that can affect the project.
pub(crate) fn is_relevant_extension(ext: &str) -> bool {
    matches!(ext, ".js" | ".jsx" | ".mjs" | ".cjs" | ".ts" | ".tsx" | ".mts" | ".cts" | ".json")
}

// snapshotfs.go:688
// isNodeModulesPath reports whether path is a node_modules directory itself or
// lives inside one. Used to preserve node_modules watch deletions, whose package
// files are read transiently and therefore never tracked in cacheDirectories.
pub(crate) fn is_node_modules_path(path: &Path) -> bool {
    path.ends_with("/node_modules") || path.contains("/node_modules/")
}

// snapshotfs.go:693
fn has_open_file_within(
    path: &Path,
    previous_open_files: &FxHashMap<Path, Arc<dyn FileHandle>>,
    open_files: &FxHashMap<Path, Arc<dyn FileHandle>>,
) -> bool {
    // pure any() predicate; Go ranges the map too
    if open_files.keys().into_iter().any(|open_file_path| path.contains_path(open_file_path)) {
        return true;
    }
    // pure any() predicate; Go ranges the map too
    previous_open_files.keys().into_iter().any(|open_file_path| path.contains_path(open_file_path))
}

// sourceFS is a vfs.FS that sources files from a FileSource and tracks seen files.
// (`tracking`, `source` and `seenFiles` are reassigned after construction by `compilerHost.freeze` and
// `updateProgram`, hence the locks.)
pub struct SourceFS {
    tracking: RwLock<bool>,
    to_path: ToPath,
    missing_directories: Option<Arc<SyncSet<Path>>>,
    seen_files: RwLock<Option<Arc<SyncMap<Path, String>>>>,
    source: RwLock<Arc<dyn FileSource>>,
}

// snapshotfs.go:777
pub(crate) fn new_source_fs(
    tracking: bool,
    source: Arc<dyn FileSource>,
    to_path: ToPath,
) -> SourceFS {
    let (seen_files, missing_directories) = if tracking {
        (Some(Arc::new(SyncMap::default())), Some(Arc::new(SyncSet::default())))
    } else {
        (None, None)
    };
    SourceFS {
        tracking: RwLock::new(tracking),
        to_path,
        missing_directories,
        seen_files: RwLock::new(seen_files),
        source: RwLock::new(source),
    }
}

impl SourceFS {
    pub(crate) fn to_path(&self, file_name: &str) -> Path {
        (self.to_path)(file_name)
    }

    pub(crate) fn source(&self) -> Arc<dyn FileSource> {
        self.source.read().unwrap().clone()
    }

    pub(crate) fn set_source(&self, source: Arc<dyn FileSource>) {
        *self.source.write().unwrap() = source;
    }

    pub(crate) fn seen_files(&self) -> Option<Arc<SyncMap<Path, String>>> {
        self.seen_files.read().unwrap().clone()
    }

    pub(crate) fn set_seen_files(&self, seen_files: Option<Arc<SyncMap<Path, String>>>) {
        *self.seen_files.write().unwrap() = seen_files;
    }

    // snapshotfs.go:792
    pub(crate) fn disable_tracking(&self) {
        *self.tracking.write().unwrap() = false;
    }

    // snapshotfs.go:796
    pub(crate) fn track(&self, file_name: &str) {
        if !*self.tracking.read().unwrap() {
            return;
        }
        if let Some(seen_files) = self.seen_files() {
            seen_files.store((self.to_path)(file_name), file_name.to_string());
        }
    }

    // snapshotfs.go:803
    pub(crate) fn seen_file(&self, path: &Path) -> bool {
        match self.seen_files() {
            None => false,
            Some(seen_files) => seen_files.load(path).is_some(),
        }
    }

    // snapshotfs.go:810
    pub(crate) fn seen_file_or_missing_parent_directory(&self, path: &Path) -> bool {
        if self.seen_files().is_some_and(|s| s.load(path).is_some()) {
            return true;
        }
        if let Some(missing_directories) = &self.missing_directories
            && !missing_directories.is_empty()
        {
            let mut path = path.clone();
            loop {
                if missing_directories.has(&path) {
                    return true;
                }

                let parent = path.get_directory_path();
                if parent == path {
                    break;
                }
                path = parent;
            }
        }
        false
    }
}

impl FileHandleSource for SourceFS {
    // snapshotfs.go:830
    fn get_file(&self, file_name: &str) -> Option<Arc<dyn FileHandle>> {
        self.track(file_name);
        self.source().get_file(file_name)
    }

    // snapshotfs.go:835
    fn get_file_by_path(&self, file_name: &str, path: &Path) -> Option<Arc<dyn FileHandle>> {
        self.track(file_name);
        self.source().get_file_by_path(file_name, path)
    }
}

impl FS for SourceFS {
    // snapshotfs.go:879
    fn use_case_sensitive_file_names(&self) -> bool {
        self.source().fs().use_case_sensitive_file_names()
    }

    // snapshotfs.go:850
    fn file_exists(&self, path: &str) -> bool {
        self.track(path);
        self.source().file_exists(path, &(self.to_path)(path))
    }

    // snapshotfs.go:861
    fn read_file(&self, path: &str) -> Option<String> {
        self.get_file(path).map(|fh| fh.content().to_string())
    }

    // snapshotfs.go:884
    fn write_file(&self, _path: &str, _data: &str) -> Result<(), String> {
        panic!("unimplemented")
    }

    // snapshotfs.go:889
    fn append_file(&self, _path: &str, _data: &str) -> Result<(), String> {
        panic!("unimplemented")
    }

    // snapshotfs.go:894
    fn remove(&self, _path: &str) -> Result<(), String> {
        panic!("unimplemented")
    }

    // snapshotfs.go:899
    fn chtimes(&self, _path: &str, _a_time: SystemTime, _m_time: SystemTime) -> Result<(), String> {
        panic!("unimplemented")
    }

    // snapshotfs.go:841
    fn directory_exists(&self, path: &str) -> bool {
        let exists = self.source().fs().directory_exists(path);
        if !exists
            && *self.tracking.read().unwrap()
            && let Some(missing_directories) = &self.missing_directories
        {
            missing_directories.add((self.to_path)(path));
        }
        exists
    }

    // snapshotfs.go:856
    fn get_accessible_entries(&self, path: &str) -> Entries {
        self.source().get_accessible_entries(path)
    }

    // snapshotfs.go:874
    fn stat(&self, path: &str) -> Option<FileInfo> {
        self.source().fs().stat(path)
    }

    // snapshotfs.go:869
    fn realpath(&self, path: &str) -> String {
        self.source().fs().realpath(path)
    }
}
