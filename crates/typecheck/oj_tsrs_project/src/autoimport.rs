use std::sync::{Arc, Mutex};

use tsrs_ast::{SourceFile, SourceFileParseOptions};
use tsrs_compiler::Program;
use tsrs_core::collections::SyncMap;
use tsrs_core::tspath::{self, Path};
use tsrs_core::{P, alloc_str};
use tsrs_ls::autoimport::{ProjectID, RegistryCloneHost};
use tsrs_module::ResolutionHost;
use tsrs_module::packagejson::{self, InfoCacheEntry, PackageJson};
use tsrs_vfs::{Entries, FS};

use crate::overlayfs::{FileHandle, ToPath};
use crate::parsecache::{ParseCache, ParseCacheKey, new_parse_cache_key};
use crate::project::ID;
use crate::projectcollection::ProjectCollection;
use crate::snapshotfs::{FileHandleSource, FileSource, SnapshotFSBuilder, SourceFS, new_source_fs};

struct AutoImportBuilderFS {
    snapshot_fs_builder: Arc<SnapshotFSBuilder>,
    untracked_files: SyncMap<Path, Option<Arc<dyn FileHandle>>>,
}

impl FileHandleSource for AutoImportBuilderFS {
    // autoimport.go:29
    fn get_file(&self, file_name: &str) -> Option<Arc<dyn FileHandle>> {
        let path = self.snapshot_fs_builder.to_path(file_name);
        self.get_file_by_path(file_name, &path)
    }

    // autoimport.go:35
    fn get_file_by_path(&self, file_name: &str, path: &Path) -> Option<Arc<dyn FileHandle>> {
        // We want to avoid long-term caching of files referenced only by auto-imports, so we
        // override GetFileByPath to avoid collecting more files into the snapshotFSBuilder's
        // cacheFiles. (Note the reason we can't just use the finalized SnapshotFS is that changed
        // files not read during other parts of the snapshot clone will be marked as dirty, but
        // not yet refreshed from the source filesystem.)
        if let Some(cached_file) = self.snapshot_fs_builder.cache_files.load(path) {
            return self.snapshot_fs_builder.reload_entry_if_needed(&cached_file);
        }
        if let Some(fh) = self.untracked_files.load(path) {
            return fh;
        }
        let fh = self.snapshot_fs_builder.fs.get_file_by_path(file_name, path);
        let (fh, _) = self.untracked_files.load_or_store(path.clone(), fh);
        fh
    }
}

impl FileSource for AutoImportBuilderFS {
    // autoimport.go:24
    fn fs(&self) -> &dyn FS {
        &*self.snapshot_fs_builder.fs
    }

    // autoimport.go:57
    fn file_exists(&self, file_name: &str, path: &Path) -> bool {
        self.snapshot_fs_builder.file_exists(file_name, path)
    }

    // autoimport.go:52
    fn get_accessible_entries(&self, path: &str) -> Entries {
        self.snapshot_fs_builder.get_accessible_entries(path)
    }
}

pub struct AutoImportRegistryCloneHost {
    project_collection: Arc<ProjectCollection>,
    parse_cache: Arc<ParseCache>,
    fs: SourceFS,
    current_directory: String,

    // Go: filesMu + files.
    files: Mutex<Vec<ParseCacheKey>>,
}

// autoimport.go:73
pub fn new_auto_import_registry_clone_host(
    project_collection: Arc<ProjectCollection>,
    parse_cache: Arc<ParseCache>,
    snapshot_fs_builder: Arc<SnapshotFSBuilder>,
    current_directory: &str,
    to_path: ToPath,
) -> AutoImportRegistryCloneHost {
    AutoImportRegistryCloneHost {
        project_collection,
        parse_cache,
        fs: new_source_fs(
            false,
            Arc::new(AutoImportBuilderFS {
                snapshot_fs_builder,
                untracked_files: SyncMap::default(),
            }),
            to_path,
        ),
        current_directory: current_directory.to_string(),
        files: Mutex::new(Vec::new()),
    }
}

impl ResolutionHost for AutoImportRegistryCloneHost {
    // autoimport.go:89
    fn fs(&self) -> &dyn FS {
        &self.fs
    }

    // autoimport.go:94
    fn get_current_directory(&self) -> &str {
        &self.current_directory
    }
}

impl RegistryCloneHost for AutoImportRegistryCloneHost {
    // autoimport.go:99
    fn get_default_project(&self, path: &Path) -> (Option<ProjectID>, Option<&'static Program>) {
        let Some(project) = self.project_collection.get_default_project(path) else {
            return (None, None);
        };
        (Some(ProjectID(project.id().0)), project.get_program())
    }

    // autoimport.go:139
    fn get_program_for_project(&self, project_id: &ProjectID) -> Option<&'static Program> {
        let id = ID(project_id.0.clone());
        let project = self.project_collection.get_project(&id)?;
        project.get_program()
    }

    // autoimport.go:108
    fn get_package_json(&self, file_name: &str) -> P<InfoCacheEntry> {
        // !!! ref-counted shared cache
        let fh = self.fs.get_file(file_name);
        let package_directory = tspath::get_directory_path(file_name);
        let Some(fh) = fh else {
            return P::new(InfoCacheEntry {
                directory_exists: self.fs.directory_exists(&package_directory),
                package_directory: alloc_str(&package_directory),
                contents: None,
            });
        };
        match packagejson::parse(fh.content()) {
            Err(_) => P::new(InfoCacheEntry {
                directory_exists: true,
                package_directory: alloc_str(&tspath::get_directory_path(file_name)),
                contents: Some(P::new(PackageJson::new(
                    tsrs_module::packagejson::Fields::default(),
                    false,
                ))),
            }),
            Ok(fields) => P::new(InfoCacheEntry {
                directory_exists: true,
                package_directory: alloc_str(&tspath::get_directory_path(file_name)),
                contents: Some(P::new(PackageJson::new(fields, true))),
            }),
        }
    }

    // autoimport.go:152
    fn get_source_file(&self, file_name: &str, path: &Path) -> Option<P<SourceFile>> {
        let fh = self.fs.get_file(file_name)?;
        let opts = SourceFileParseOptions {
            file_name: file_name.to_string(),
            path: path.clone(),
            ..Default::default()
        };
        let key = new_parse_cache_key(opts, fh.hash(), fh.kind());
        let result = self.parse_cache.acquire(&key, fh);

        self.files.lock().unwrap().push(key);

        Some(result)
    }

    // autoimport.go:172
    fn dispose(&self) {
        let files = self.files.lock().unwrap();
        for key in files.iter() {
            self.parse_cache.deref(key);
        }
    }
}
