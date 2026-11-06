use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;
use tsrs_ast::{SourceFile, SourceFileParseOptions};
use tsrs_core::context::Context;
use tsrs_core::tspath::{self, Path};
use tsrs_core::{P, ScriptKind};
use tsrs_ls::lsutil;
use tsrs_lsproto as lsproto;
use tsrs_vfs::FS;

use crate::extendedconfigcache::{ExtendedConfigCache, new_extended_config_cache};
use crate::filechange::FileChangeSummary;
use crate::logging::Logger;
use crate::overlayfs::{FsRef, ToPath, new_cached_file_handle, new_overlay_fs};
use crate::parsecache::{ParseCache, ParseCacheKey, new_parse_cache, new_parse_cache_key};
use crate::programcounter::ProgramCounter;
use crate::refcountcache::RefCountCacheOptions;
use crate::session::{SessionInit, SessionOptions, UpdateReason};
use crate::snapshot::{APISnapshotRequest, Snapshot, SnapshotChange};
use crate::snapshotfs::new_snapshot_fs;
use crate::watch::{
    ALL_WATCH_KINDS, PatternsAndIgnored, get_recursive_glob_pattern, new_watched_files,
};

// SnapshotHost owns the services shared by a collection of immutable snapshots.
pub struct SnapshotHost {
    pub(crate) options: Arc<SessionOptions>,
    pub(crate) to_path: ToPath,
    pub(crate) fs: Arc<dyn FS>,

    pub parse_cache: Arc<ParseCache>,
    pub(crate) extended_config_cache: Arc<ExtendedConfigCache>,
    pub program_counter: ProgramCounter,
    // (Content mappers are not ported: Go's `contentMapperHost` is always nil here.)
    snapshot_id: AtomicU64,
}

pub struct SourceFileLease {
    cache: Arc<ParseCache>,
    key: ParseCacheKey,
    source_file: P<SourceFile>,
    released: Mutex<bool>,
}

impl SourceFileLease {
    // snapshothost.go:41
    pub fn source_file(&self) -> P<SourceFile> {
        self.source_file
    }

    // snapshothost.go:45
    pub fn release(&self) {
        let mut released = self.released.lock().unwrap();
        if !*released {
            *released = true;
            self.cache.deref(&self.key);
        }
    }
}

impl SnapshotHost {
    // snapshothost.go:51
    pub(crate) fn next_snapshot_id(&self) -> u64 {
        self.snapshot_id.fetch_add(1, Ordering::SeqCst) + 1
    }

    // snapshothost.go:55
    pub fn acquire_source_file(
        &self,
        options: SourceFileParseOptions,
        text: &str,
        script_kind: ScriptKind,
    ) -> SourceFileLease {
        let file_handle = new_cached_file_handle(&options.file_name, text);
        let key = new_parse_cache_key(options, file_handle.hash(), script_kind);
        SourceFileLease {
            cache: Arc::clone(&self.parse_cache),
            key: key.clone(),
            source_file: self.parse_cache.acquire(&key, file_handle),
            released: Mutex::new(false),
        }
    }

    // snapshothost.go:65
    pub fn acquire_existing_source_file(&self, key: ParseCacheKey) -> Option<SourceFileLease> {
        let source_file = self.parse_cache.acquire_existing(&key)?;
        Some(SourceFileLease {
            cache: Arc::clone(&self.parse_cache),
            key,
            source_file,
            released: Mutex::new(false),
        })
    }
}

// snapshothost.go:77
pub fn new_snapshot_host(init: &SessionInit) -> Arc<SnapshotHost> {
    let current_directory = init.options.current_directory.clone();
    let use_case_sensitive_file_names = init.fs.use_case_sensitive_file_names();
    let to_path: ToPath = Arc::new(move |file_name: &str| {
        tspath::to_path(file_name, &current_directory, use_case_sensitive_file_names)
    });
    let parse_cache = init
        .parse_cache
        .clone()
        .unwrap_or_else(|| Arc::new(new_parse_cache(RefCountCacheOptions::default())));

    Arc::new(SnapshotHost {
        options: Arc::clone(&init.options),
        to_path,
        fs: Arc::clone(&init.fs),
        parse_cache,
        extended_config_cache: Arc::new(new_extended_config_cache()),
        program_counter: ProgramCounter::default(),
        snapshot_id: AtomicU64::new(0),
    })
}

impl SnapshotHost {
    // snapshothost.go:105
    // NewRootSnapshot creates an independent root snapshot.
    pub fn new_root_snapshot(self: &Arc<Self>) -> Arc<Snapshot> {
        self.new_root_snapshot_with(0, false)
    }

    // snapshothost.go:110
    // RetainSnapshot adds a reference to a snapshot owned by this host.
    #[expect(
        clippy::unused_self,
        reason = "SnapshotHost lifecycle callbacks keep their instance receiver alongside the stateful host operations."
    )]
    pub fn retain_snapshot(&self, snapshot: &Snapshot) {
        snapshot.ref_();
    }

    // snapshothost.go:116
    // CloneSnapshot derives a snapshot from baseSnapshot without adopting it as any
    // canonical session state or performing session side effects.
    pub fn clone_snapshot(
        &self,
        ctx: &Context,
        base_snapshot: &Snapshot,
        file_changes: FileChangeSummary,
        api_request: Option<Arc<APISnapshotRequest>>,
    ) -> Result<Arc<Snapshot>, (Arc<Snapshot>, lsproto::Error)> {
        let mut change = SnapshotChange { file_changes, ..Default::default() };
        if let Some(api_request) = &api_request {
            change.fs = match &api_request.layered_file_system {
                Some(layered) => Some(FsRef::Layered(Arc::clone(layered))),
                None => api_request.file_system.clone().map(FsRef::Host),
            };
            change.file_system_override = change.fs.is_some();
            change.replace_file_system = api_request.replace_file_system;
            change.new_config.clone_from(&api_request.user_preferences);
            if !api_request.prepare_auto_imports.0.is_empty() {
                change.resource_request =
                    base_snapshot.resource_request_for_document(&api_request.prepare_auto_imports);
                change.resource_request.auto_imports = api_request.prepare_auto_imports.clone();
            }
        }
        change.api_request = api_request;
        let snapshot = self.update(ctx, base_snapshot, change);
        match snapshot.api_error.clone() {
            Some(err) => Err((snapshot, err)),
            None => Ok(snapshot),
        }
    }

    // snapshothost.go:137
    // update derives a snapshot from baseSnapshot without adopting it as any
    // canonical session state or performing session side effects.
    #[expect(
        clippy::unused_self,
        reason = "SnapshotHost lifecycle callbacks keep their instance receiver alongside the stateful host operations."
    )]
    fn update(
        &self,
        ctx: &Context,
        base_snapshot: &Snapshot,
        change: SnapshotChange,
    ) -> Arc<Snapshot> {
        base_snapshot.clone_snapshot(ctx, change, base_snapshot.overlays(), None, None)
    }

    // snapshothost.go:143
    // CloneSnapshotWithAutoImports derives a snapshot with auto-import preparation without
    // adopting the clone in the background.
    #[expect(
        clippy::unused_self,
        reason = "SnapshotHost lifecycle callbacks keep their instance receiver alongside the stateful host operations."
    )]
    pub fn clone_snapshot_with_auto_imports(
        &self,
        ctx: &Context,
        base_snapshot: &Snapshot,
        uri: &lsproto::DocumentUri,
        logger: Option<&dyn Logger>,
    ) -> Arc<Snapshot> {
        let mut change = SnapshotChange {
            reason: UpdateReason::RequestedLanguageServiceWithAutoImports,
            fs: Some(FsRef::Layered(Arc::clone(&base_snapshot.fs.fs))),
            file_system_override: base_snapshot.file_system_override,
            resource_request: base_snapshot.resource_request_for_document(uri),
            ..Default::default()
        };
        change.resource_request.auto_imports = uri.clone();
        base_snapshot.clone_snapshot(ctx, change, base_snapshot.overlays(), logger, None)
    }

    // snapshothost.go:154
    pub(crate) fn new_root_snapshot_with(
        self: &Arc<Self>,
        id: u64,
        relative_pattern_support: bool,
    ) -> Arc<Snapshot> {
        let file_system = new_overlay_fs(
            FsRef::Host(Arc::clone(&self.fs)),
            Arc::default(),
            self.options.position_encoding,
            Arc::clone(&self.to_path),
        );
        Arc::new(self.new_snapshot(
            id,
            Arc::new(new_snapshot_fs(Arc::clone(&self.to_path), Arc::new(file_system))),
            Arc::default(),
            None,
            lsutil::new_default_user_preferences(),
            None,
            Some(new_watched_files(
                "auto-import".to_string(),
                ALL_WATCH_KINDS,
                relative_pattern_support,
                Arc::new(|node_modules_dirs: &FxHashMap<Path, String>| {
                    let mut patterns: Vec<String> = node_modules_dirs
                        .values()
                        .map(|dir| get_recursive_glob_pattern(dir))
                        .collect();
                    patterns.sort();
                    PatternsAndIgnored { patterns_inside_workspace: patterns, ..Default::default() }
                }),
            )),
        ))
    }

    /// `fs` as a shared handle (a Go `vfs.FS` is a reference), for an LSP-attached API session.
    pub fn shared_fs(&self) -> Arc<dyn FS> {
        Arc::clone(&self.fs)
    }

    // snapshothost.go:184
    pub fn fs(&self) -> &dyn FS {
        &*self.fs
    }

    // snapshothost.go:188
    pub fn get_current_directory(&self) -> &str {
        &self.options.current_directory
    }

    // snapshothost.go:192
    pub fn default_library_path(&self) -> &str {
        &self.options.default_library_path
    }

    // snapshothost.go:196
    #[expect(
        clippy::unused_self,
        reason = "SnapshotHost lifecycle callbacks keep their instance receiver alongside the stateful host operations."
    )]
    pub(crate) fn close(&self) {
        // (Content mappers are not ported: there is no content mapper host to close.)
    }
}
