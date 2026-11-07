use std::fmt::Write as _;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use rustc_hash::FxHashMap;
use tsrs_ast::Diagnostic;
use tsrs_core::collections::Set;
use tsrs_core::context::Context;
use tsrs_core::tspath::Path;
use tsrs_core::{CompilerOptions, P, ProjectReference};
use tsrs_ls::autoimport::{self, ProjectID, RegistryCloneHost};
use tsrs_ls::lsconv::{self, Converters};
use tsrs_ls::lsutil::UserPreferences;
use tsrs_ls::sourcemap::ECMALineInfo;
use tsrs_lsproto as lsproto;
use tsrs_module::{Resolver, ResolverOptions};
use tsrs_tsoptions::Mapper;
use tsrs_vfs::{FS, vfsmatch};

use crate::ata;
use crate::autoimport::new_auto_import_registry_clone_host;
use crate::client::Client;
use crate::configfileregistry::ConfigFileRegistry;
use crate::dirty::Shared;
use crate::filechange::FileChangeSummary;
use crate::logging::{LogTree, Logger, new_log_tree};
use crate::overlayfs::{FileHandle, FsRef, OverlayMap, layer_overlay_file_system};
use crate::parsecache::{parse_cache_key_for_duplicate, parse_cache_key_for_file};
use crate::project::{ID, ProgramUpdateKind, Project, SyntheticProjectID};
use crate::projectcollection::{ProjectCollection, new_project_collection, open_file_paths};
use crate::projectcollectionbuilder::new_project_collection_builder;
use crate::session::{ContentMapperContributions, UpdateReason};
use crate::snapshotfs::{
    FileHandleSource, SnapshotFS, SnapshotFSBuilder, new_snapshot_fs_builder_from_source,
    new_source_fs,
};
use crate::snapshothost::SnapshotHost;
use crate::watch::WatchedFiles;

// Shared as `Arc<Snapshot>`; Go's explicit `ref`/`Deref` counts are kept (they release parse cache entries and
// programs), the `Arc` only keeps the memory alive (docs/LSP.md "Threading model").
pub struct Snapshot {
    host: Arc<SnapshotHost>,
    pub(crate) id: u64,
    pub(crate) parent_id: u64,
    ref_count: AtomicU32,

    converters: Arc<Converters>,

    // Immutable state, cloned between snapshots
    pub(crate) fs: Arc<SnapshotFS>,
    pub project_collection: Arc<ProjectCollection>,
    pub config_file_registry: Arc<ConfigFileRegistry>,
    pub(crate) auto_imports: Option<Arc<autoimport::Registry>>,
    pub(crate) auto_imports_watch: Option<Arc<WatchedFiles<FxHashMap<Path, String>>>>,
    compiler_options_for_inferred_projects: Option<P<CompilerOptions>>,
    inferred_project_content_mappers: Vec<Mapper>,
    pub(crate) inferred_project_content_mapper_extensions: Vec<String>,
    user_preferences: UserPreferences,
    // Go: contentMapperWatchStateOnce + contentMapperExtensions + contentMapperWatchedFiles.
    content_mapper_watch_state: OnceLock<(Vec<String>, Arc<Set<Path>>)>,

    pub(crate) builder_logs: LogTree,
    pub(crate) api_error: Option<lsproto::Error>,
    // fileSystemOverride indicates that this snapshot was built from a filesystem
    // supplied by an API update rather than the session host filesystem.
    pub(crate) file_system_override: bool,

    created_programs: Vec<Option<Shared<Project>>>,
}

impl Snapshot {
    // snapshot.go:62
    pub(crate) fn content_mapper_watch_state(&self) -> (Vec<String>, Arc<Set<Path>>) {
        self.content_mapper_watch_state
            .get_or_init(|| {
                let configured = self.config_file_registry.content_mappers();
                let mut content_mapper_extensions = configured.extensions.clone();
                content_mapper_extensions
                    .extend(self.inferred_project_content_mapper_extensions.iter().cloned());
                content_mapper_extensions.sort();
                content_mapper_extensions.dedup();

                let mut content_mapper_watched_files = Set::default();
                for project in self.project_collection.projects() {
                    if let Some(watched) = &project.content_mapper_watched_files {
                        // pure set union; Go ranges the set too
                        content_mapper_watched_files
                            .m
                            .extend(watched.keys().iter().map(std::clone::Clone::clone));
                    }
                }
                (content_mapper_extensions, Arc::new(content_mapper_watched_files))
            })
            .clone()
    }
}

impl SnapshotHost {
    // snapshot.go:84
    pub(crate) fn new_snapshot(
        self: &Arc<Self>,
        id: u64,
        fs: Arc<SnapshotFS>,
        config_file_registry: Arc<ConfigFileRegistry>,
        compiler_options_for_inferred_projects: Option<P<CompilerOptions>>,
        user_preferences: UserPreferences,
        auto_imports: Option<Arc<autoimport::Registry>>,
        auto_imports_watch: Option<Arc<WatchedFiles<FxHashMap<Path, String>>>>,
    ) -> Snapshot {
        let overlays = snapshot_overlays(&fs);
        let line_map_fs = Arc::clone(&fs);
        Snapshot {
            host: Arc::clone(self),
            id,
            parent_id: 0,
            ref_count: AtomicU32::new(1),
            // Go passes the method value `s.LSPLineMap`; the closure holds the snapshot's file system rather than the
            // snapshot itself (which would form a reference cycle).
            converters: lsconv::new_converters(
                self.options.position_encoding,
                move |file_name: &str| {
                    line_map_fs.get_file(file_name).map(|file| file.lsp_line_map())
                },
            ),

            fs,
            config_file_registry,
            project_collection: Arc::new(new_project_collection(
                Arc::clone(&self.to_path),
                open_file_paths(&overlays),
            )),
            compiler_options_for_inferred_projects,
            user_preferences,
            auto_imports,
            auto_imports_watch,
            inferred_project_content_mappers: Vec::new(),
            inferred_project_content_mapper_extensions: Vec::new(),
            content_mapper_watch_state: OnceLock::new(),
            builder_logs: LogTree::nil(),
            api_error: None,
            file_system_override: false,
            created_programs: Vec::new(),
        }
    }
}

// snapshot.go:111
fn snapshot_overlays(fs: &SnapshotFS) -> OverlayMap {
    fs.fs.overlays()
}

// Go `ModuleResolverFactory` (API programs only).
pub trait ModuleResolverFactory: Send + Sync {
    fn new_resolver(
        &self,
        ctx: &Context,
        options: ResolverOptions<'static>,
    ) -> (Box<dyn Resolver>, Box<dyn FnOnce() + Send>);
}

pub struct APICreateProgramRequest {
    pub root_file_names: Vec<String>,
    pub compiler_options: Option<P<CompilerOptions>>,
    pub project_references: Vec<ProjectReference>,
    pub config_file_parsing_diagnostics: Vec<P<Diagnostic>>,
    pub module_resolver_factory: Option<Arc<dyn ModuleResolverFactory>>,
    pub module_resolver_id: u64,
}

pub struct APIReconfigureProgramRequest {
    pub program_id: SyntheticProjectID,
    pub request: APICreateProgramRequest,
}

// API snapshot request, built by tsrs_api's standalone API server (`to_api_snapshot_request`). API sessions attached
// to the language server are out of scope (docs/LSP.md).
#[derive(Default)]
pub struct APISnapshotRequest {
    pub user_preferences: Option<UserPreferences>,
    pub prepare_auto_imports: lsproto::DocumentUri,
    pub open_projects: Option<Set<String>>,
    pub close_projects: Option<Set<Path>>,
    pub open_files: Option<FxHashMap<Path, String>>,
    pub close_files: Option<Set<Path>>,
    pub create_programs: Vec<APICreateProgramRequest>,
    pub reconfigure_programs: Vec<APIReconfigureProgramRequest>,
    pub remove_programs: Option<Set<SyntheticProjectID>>,
    pub ensure_programs: Option<Set<ID>>,
    pub ensure_all_programs: bool,
    pub ensure_files: FxHashMap<Path, String>,
    pub file_system: Option<Arc<dyn FS>>,
    // A request file system that is a layered (rebasable) file system; takes precedence over `file_system`
    // (Go passes the request file system as a `vfs.FS` and type-asserts it).
    pub layered_file_system: Option<Arc<dyn crate::LayeredFileSystem>>,
    // ReplaceFileSystem indicates a total filesystem replacement. Layers use
    // per-path file changes instead of invalidating all inherited state.
    pub replace_file_system: bool,
}

#[derive(Clone, Debug, Default)]
pub struct ProjectTreeRequest {
    // If null, all project trees need to be loaded, otherwise only those that are referenced
    referenced_projects: Option<Set<Path>>,
}

impl ProjectTreeRequest {
    pub(crate) fn new(referenced_projects: Option<Set<Path>>) -> ProjectTreeRequest {
        ProjectTreeRequest { referenced_projects }
    }

    // snapshot.go:362
    pub(crate) fn is_all_projects(&self) -> bool {
        self.referenced_projects.is_none()
    }

    // snapshot.go:366
    pub(crate) fn is_project_referenced(&self, project_id: &Path) -> bool {
        self.referenced_projects.as_ref().is_some_and(|r| r.has(project_id))
    }

    // snapshot.go:370
    pub(crate) fn projects(&self) -> Vec<Path> {
        match &self.referenced_projects {
            None => Vec::new(),
            Some(r) => r.keys().iter().cloned().collect(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ResourceRequest {
    // Documents are URIs that were requested by the client.
    // The new snapshot should ensure projects for these URIs have loaded programs.
    pub(crate) documents: Vec<lsproto::DocumentUri>,
    // ConfiguredProjectDocuments are URIs for which configured projects should be loaded
    // (if disableSolutionSearching/disableReferencedProjectLoad settings allow),
    // but no inferred project should be created if no configured project is found.
    // This is used by cross-project operations like find-all-references.
    pub(crate) configured_project_documents: Vec<lsproto::DocumentUri>,
    // Update requested Projects.
    // this is used when we want to get LS and from all the Projects the file can be part of
    pub(crate) projects: Vec<ID>,
    // Update and ensure project trees that reference the projects
    // This is used to compute the solution and project tree so that
    // we can find references across all the projects in the solution irrespective of which project is open
    pub(crate) project_tree: Option<ProjectTreeRequest>,
    // AutoImports is the document URI for which auto imports should be prepared.
    pub(crate) auto_imports: lsproto::DocumentUri,
}

#[derive(Default)]
pub struct SnapshotChange {
    // Go embeds ResourceRequest.
    pub(crate) resource_request: ResourceRequest,
    pub(crate) reason: UpdateReason,
    // fs overrides the session filesystem for this snapshot. It is used by API
    // snapshots that supply their own memory or cache filesystem.
    pub(crate) fs: Option<FsRef>,
    pub(crate) file_system_override: bool,
    pub(crate) replace_file_system: bool,
    // fileChanges are the changes that have occurred since the last snapshot.
    pub(crate) file_changes: FileChangeSummary,
    // compilerOptionsForInferredProjects is the compiler options to use for inferred projects.
    // It should only be set the value in the next snapshot should be changed. If nil, the
    // value from the previous snapshot will be copied to the new snapshot.
    pub(crate) compiler_options_for_inferred_projects: Option<P<CompilerOptions>>,
    pub(crate) content_mapper_contributions: Option<ContentMapperContributions>,
    pub(crate) new_config: Option<UserPreferences>,
    // ataChanges contains ATA-related changes to apply to projects in the new snapshot.
    pub(crate) ata_changes: FxHashMap<ID, Arc<ATAStateChange>>,
    pub(crate) api_request: Option<Arc<APISnapshotRequest>>,
    // cleanFileCache triggers cleaning of cached files not referenced by any open project.
    pub(crate) clean_file_cache: bool,
}

// ATAStateChange represents a change to a project's ATA state.
pub struct ATAStateChange {
    // TypingsInfo is the new typings info for the project.
    pub(crate) typings_info: Option<Arc<ata::TypingsInfo>>,
    // TypingsFiles is the new list of typing files for the project.
    pub(crate) typings_files: Vec<String>,
    // TypingsFilesToWatch is the new list of typing files to watch for changes.
    pub(crate) typings_files_to_watch: Vec<String>,
    pub(crate) logs: LogTree,
}

// Go's `defer func() { if r := recover(); r != nil { sessionLogger.Log(logger.String()); panic(r) } }()`.
struct LogOnPanic<'a> {
    session_logger: Option<&'a dyn Logger>,
    logger: &'a LogTree,
}

impl Drop for LogOnPanic<'_> {
    fn drop(&mut self) {
        if std::thread::panicking()
            && let Some(session_logger) = self.session_logger
            && !self.logger.is_nil()
        {
            session_logger.log(&self.logger.string());
        }
    }
}

impl Snapshot {
    // snapshot.go:115
    pub(crate) fn overlays(&self) -> OverlayMap {
        snapshot_overlays(&self.fs)
    }

    // snapshot.go:119
    pub fn created_programs(&self) -> &[Option<Shared<Project>>] {
        &self.created_programs
    }

    // snapshot.go:123
    pub(crate) fn resource_request_for_document(
        &self,
        uri: &lsproto::DocumentUri,
    ) -> ResourceRequest {
        let path = uri.path(self.use_case_sensitive_file_names());
        let mut request = ResourceRequest { documents: vec![uri.clone()], ..Default::default() };
        request.projects.extend(
            self.project_collection.synthetic_projects().into_iter().filter_map(|project| {
                if project.contains_file(&path)
                    || project.host.as_ref().is_some_and(|host| {
                        host.source_fs.seen_file_or_missing_parent_directory(&path)
                    })
                {
                    Some(project.id())
                } else {
                    None
                }
            }),
        );
        request
    }

    // snapshot.go:134
    fn process_file_changes(
        &self,
        fs: &SnapshotFSBuilder,
        mut file_changes: FileChangeSummary,
        logger: &LogTree,
        content_mapper_contributions: Option<&ContentMapperContributions>,
        previous_overlays: &OverlayMap,
        overlays: &OverlayMap,
    ) -> FileChangeSummary {
        if let Some(expander) = fs.fs.as_file_change_expander() {
            file_changes = expander.expand_file_changes(file_changes);
        }
        let previous_open_files = overlay_file_handles(previous_overlays);
        let open_files = overlay_file_handles(overlays);
        if file_changes.has_excessive_watch_events() {
            let invalidate_start = Instant::now();
            if file_changes.invalidate_all {
                fs.invalidate_cache();
                logger.logf(format_args!(
                    "InvalidateAll: invalidated file cache in {:?}",
                    invalidate_start.elapsed()
                ));
            } else if !fs.watch_changes_overlap_cache(
                &file_changes,
                &previous_open_files,
                &open_files,
            ) {
                // All watch changes/deletes are files we haven't seen; should be irrelevant to us (probably an external tool's build or something)
                file_changes.changed = Set::default();
                file_changes.deleted = Set::default();
            } else if file_changes.includes_watch_change_outside_node_modules {
                fs.invalidate_cache();
                logger.logf(format_args!(
                    "Excessive watch changes detected, invalidated file cache in {:?}",
                    invalidate_start.elapsed()
                ));
            } else {
                fs.invalidate_node_modules_cache();
                logger.logf(format_args!(
                    "npm install detected, invalidated node_modules cache in {:?}",
                    invalidate_start.elapsed()
                ));
            }
        } else {
            let content_mapper_extensions: Vec<String> = match content_mapper_contributions {
                None => self.content_mapper_watch_state().0,
                Some(contributions) => {
                    let mut extensions =
                        self.config_file_registry.content_mappers().extensions.clone();
                    extensions.extend(contributions.extensions.iter().cloned());
                    extensions
                }
            };
            let (_, content_mapper_watched_files) = self.content_mapper_watch_state();
            file_changes = fs.expand_and_filter_watch_events(
                file_changes,
                &content_mapper_extensions,
                Some(&content_mapper_watched_files),
                &previous_open_files,
                &open_files,
            );
            file_changes = self.fs.expand_realpath_aliases(file_changes);
            file_changes = fs.mark_dirty_files(file_changes);
            file_changes = fs.convert_open_and_close_to_changes(
                file_changes,
                &previous_open_files,
                &open_files,
            );
        }
        #[expect(
            clippy::iter_over_hash_type,
            reason = "each open path's cache entry is only deleted; commutes; Go ranges the map too"
        )]
        for path in open_files.keys() {
            if let Some(entry) = fs.cache_files.load(path) {
                fs.delete_cache_entry(&entry);
            }
        }
        file_changes
    }

    // snapshot.go:201
    pub fn get_default_project(&self, uri: &lsproto::DocumentUri) -> Option<Shared<Project>> {
        self.project_collection.get_default_project(&uri.path(self.use_case_sensitive_file_names()))
    }

    // snapshot.go:207
    // GetLanguageServiceProjectsContainingFile does not consider synthetic projects
    // (ones created by API via createProgram).
    pub fn get_language_service_projects_containing_file(
        &self,
        uri: &lsproto::DocumentUri,
    ) -> Vec<Arc<dyn tsrs_ls::Project>> {
        let file_name = uri.file_name();
        let path = (self.host.to_path)(&file_name);
        // TODO!! sheetal may be change this to handle symlinks!!
        self.project_collection.get_language_service_projects_containing_file(&path)
    }

    // snapshot.go:214
    pub fn get_file(&self, file_name: &str) -> Option<Arc<dyn FileHandle>> {
        self.fs.get_file(file_name)
    }

    // snapshot.go:236
    pub fn user_preferences(&self) -> &UserPreferences {
        &self.user_preferences
    }

    // snapshot.go:248
    pub fn id(&self) -> u64 {
        self.id
    }

    // snapshot.go:252
    fn to_path(&self, file_name: &str) -> Path {
        (self.host.to_path)(file_name)
    }

    // snapshot.go:256
    pub(crate) fn is_open_file(&self, file_name: &str) -> bool {
        self.overlays().contains_key(&self.to_path(file_name))
    }

    // snapshot.go:261
    pub(crate) fn has_overlay_within(&self, path: &Path) -> bool {
        self.overlays().keys().any(|overlay_path| path.contains_path(overlay_path))
    }

    // snapshot.go:275
    // FileSystem returns the filesystem backing this snapshot.

    // snapshot.go:281
    // HasFileSystemOverride reports whether this snapshot uses an API-supplied
    // filesystem instead of the session host filesystem.

    // snapshot.go:309
    pub fn fs(&self) -> Arc<dyn FS> {
        Arc::new(new_source_fs(
            false,
            Arc::<SnapshotFS>::clone(&self.fs),
            Arc::clone(&self.host.to_path),
        ))
    }

    // snapshot.go:431
    pub(crate) fn clone_snapshot(
        &self,
        ctx: &Context,
        mut change: SnapshotChange,
        overlays: OverlayMap,
        session_logger: Option<&dyn Logger>,
        client: Option<Arc<dyn Client>>,
    ) -> Arc<Snapshot> {
        if let Some(api_error) = &self.api_error {
            panic!("cannot clone snapshot with API error: {}", api_error.message);
        }
        let store = &self.host;
        let mut logger = LogTree::nil();

        if store.options.logging_enabled && session_logger.is_some() {
            logger = new_log_tree(&format!("Cloning snapshot {}", self.id));
        }
        // Print in-progress logs immediately if cloning fails
        let _log_on_panic = LogOnPanic {
            session_logger: if store.options.logging_enabled { session_logger } else { None },
            logger: &logger,
        };

        if !logger.is_nil() {
            let get_details = || {
                let mut details = String::new();
                if !change.resource_request.documents.is_empty() {
                    let _ = write!(
                        details,
                        " Documents: {:?}",
                        change
                            .resource_request
                            .documents
                            .iter()
                            .map(|d| d.0.as_str())
                            .collect::<Vec<_>>()
                    );
                }
                if !change.resource_request.configured_project_documents.is_empty() {
                    let _ = write!(
                        details,
                        " ConfiguredProjectDocuments: {:?}",
                        change
                            .resource_request
                            .configured_project_documents
                            .iter()
                            .map(|d| d.0.as_str())
                            .collect::<Vec<_>>()
                    );
                }
                if !change.resource_request.projects.is_empty() {
                    let _ = write!(
                        details,
                        " Projects: {:?}",
                        change
                            .resource_request
                            .projects
                            .iter()
                            .map(|p| p.0.as_str())
                            .collect::<Vec<_>>()
                    );
                }
                if let Some(project_tree) = &change.resource_request.project_tree {
                    let _ = write!(
                        details,
                        " ProjectTree: {:?}",
                        project_tree
                            .projects()
                            .iter()
                            .map(tsrs_core::tspath::Path::as_str)
                            .collect::<Vec<_>>()
                    );
                }
                details
            };
            match change.reason {
                UpdateReason::DidOpenFile => logger
                    .logf(format_args!("Reason: DidOpenFile - {}", change.file_changes.opened.0)),
                UpdateReason::DidCloseFile => logger.logf(format_args!(
                    "Reason: DidCloseFile - {:?}",
                    change
                        .file_changes
                        .closed
                        .keys()
                        .iter()
                        .map(|u| u.0.as_str())
                        .collect::<Vec<_>>()
                )),
                UpdateReason::DidChangeCompilerOptionsForInferredProjects => {
                    logger.log("Reason: DidChangeCompilerOptionsForInferredProjects");
                }
                UpdateReason::RequestedLanguageServicePendingChanges => {
                    logger.logf(format_args!(
                        "Reason: RequestedLanguageService (pending file changes) - {}",
                        get_details()
                    ));
                }
                UpdateReason::RequestedLanguageServiceProjectNotLoaded => {
                    logger.logf(format_args!(
                        "Reason: RequestedLanguageService (project not loaded) - {}",
                        get_details()
                    ));
                }
                UpdateReason::RequestedLanguageServiceForFileNotOpen => {
                    logger.logf(format_args!(
                        "Reason: RequestedLanguageService (file not open) - {}",
                        get_details()
                    ));
                }
                UpdateReason::RequestedLanguageServiceProjectDirty => {
                    logger.logf(format_args!(
                        "Reason: RequestedLanguageService (project dirty) - {}",
                        get_details()
                    ));
                }
                UpdateReason::RequestedLoadProjectTree => logger
                    .logf(format_args!("Reason: RequestedLoadProjectTree - {}", get_details())),
                UpdateReason::IdleCleanDiskCache => logger.log("Reason: IdleCleanDiskCache"),
                UpdateReason::DidChangeConfigFile => {
                    logger.logf(format_args!("Reason: DidChangeConfigFile - {}", get_details()));
                }
                UpdateReason::DidChangeContentMapperContributions => {
                    logger.logf(format_args!(
                        "Reason: DidChangeContentMapperContributions - {}",
                        get_details()
                    ));
                }
                _ => {}
            }
        }

        let start = Instant::now();
        let mut inferred_content_mappers = self.inferred_project_content_mappers.clone();
        let mut inferred_content_mapper_extensions =
            self.inferred_project_content_mapper_extensions.clone();
        if let Some(contributions) = &change.content_mapper_contributions {
            inferred_content_mappers.clone_from(&contributions.mappers);
            inferred_content_mapper_extensions.clone_from(&contributions.extensions);
        }
        let mut base_fs = FsRef::Host(Arc::clone(&store.fs));
        if let Some(fs) = &change.fs {
            base_fs = fs.clone();
        }
        // Total replacements and returning to the session host must not retain files
        // from the previous filesystem. Layers invalidate only their per-path changes,
        // including the first layer over a host-backed snapshot.
        if change.replace_file_system || self.file_system_override && !change.file_system_override {
            change.file_changes.invalidate_all = true;
        }
        let layered_fs = layer_overlay_file_system(
            &base_fs,
            overlays,
            store.options.position_encoding,
            Arc::clone(&store.to_path),
        );
        let overlays = layered_fs.overlays();
        let fs = Arc::new(new_snapshot_fs_builder_from_source(
            layered_fs,
            Arc::clone(&self.fs.cache_files),
            Arc::clone(&self.fs.cache_directories),
            Arc::clone(&self.fs.node_modules_realpath_aliases),
            Arc::clone(&store.to_path),
        ));
        change.file_changes = self.process_file_changes(
            &fs,
            std::mem::take(&mut change.file_changes),
            &logger,
            change.content_mapper_contributions.as_ref(),
            &self.overlays(),
            &overlays,
        );

        let mut compiler_options_for_inferred_projects =
            self.compiler_options_for_inferred_projects;
        if change.compiler_options_for_inferred_projects.is_some() {
            compiler_options_for_inferred_projects = change.compiler_options_for_inferred_projects;
        }

        // Compute effective customConfigFileName from user preferences
        let mut custom_config_file_name = self.config_file_registry.custom_config_file_name.clone();
        if let Some(new_config) = &change.new_config {
            custom_config_file_name.clone_from(&new_config.custom_config_file_name);
        }

        let new_snapshot_id = store.next_snapshot_id();
        let project_collection_builder = new_project_collection_builder(
            ctx,
            new_snapshot_id,
            Arc::clone(&fs),
            Arc::clone(&overlays),
            Arc::clone(&self.project_collection),
            Arc::clone(&self.config_file_registry),
            &self.project_collection.api_state,
            compiler_options_for_inferred_projects,
            inferred_content_mappers.clone(),
            inferred_content_mapper_extensions.clone(),
            Arc::clone(&store.options),
            &custom_config_file_name,
            Arc::clone(&store.parse_cache),
            Arc::clone(&store.extended_config_cache),
            client,
        );

        // Building programs can unwind (a panic while building, e.g. the module resolver's `Unexpected
        // moduleResolution`, which the API turns into an error). The parse-cache references the clone's programs took
        // would then never reach a snapshot to release them: roll them back before unwinding further. The clone's
        // projects (and their program owners) are dropped with the builder as the unwind continues.
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            if !change.ata_changes.is_empty() {
                project_collection_builder
                    .did_update_ata_state(&change.ata_changes, &logger.fork("DidUpdateATAState"));
            }

            project_collection_builder
                .did_change_custom_config_file_name(&logger.fork("DidChangeCustomConfigFileName"));
            if let Some(options) = change.compiler_options_for_inferred_projects
                && let Some(inferred) = project_collection_builder.inferred_project_value()
            {
                let command_line = inferred.command_line.unwrap();
                project_collection_builder.update_inferred_project(
                    command_line.file_names().to_vec(),
                    Some(options),
                    command_line.project_references().to_vec(),
                    command_line.errors.clone(),
                    command_line.content_mappers().to_vec(),
                    &logger.fork("DidChangeCompilerOptionsForInferredProjects"),
                );
            }
            if change.content_mapper_contributions.is_some() {
                project_collection_builder.did_change_content_mapper_contributions(
                    &logger.fork("DidChangeContentMapperContributions"),
                );
            }
            if let Some(new_config) = &change.new_config {
                project_collection_builder.did_change_user_preferences(
                    &self.user_preferences,
                    new_config,
                    &logger.fork("DidChangeUserPreferences"),
                );
            }

            if !change.file_changes.is_empty() {
                project_collection_builder
                    .did_change_files(&change.file_changes, &logger.fork("DidChangeFiles"));
            }

            let mut api_error = None;
            if let Some(api_request) = &change.api_request {
                api_error = project_collection_builder
                    .handle_api_request(api_request, &logger.fork("HandleAPIRequest"))
                    .err();
            }

            for uri in &change.resource_request.documents {
                project_collection_builder.did_request_file(
                    uri,
                    false, /*configuredProjectsOnly*/
                    &logger.fork("DidRequestFile"),
                );
            }

            for uri in &change.resource_request.configured_project_documents {
                project_collection_builder.did_request_file(
                    uri,
                    true, /*configuredProjectsOnly*/
                    &logger.fork("DidRequestFile (optional)"),
                );
            }

            for project_id in &change.resource_request.projects {
                project_collection_builder
                    .did_request_project(project_id, &logger.fork("DidRequestProject"));
            }

            if let Some(project_tree) = &change.resource_request.project_tree {
                project_collection_builder.did_request_project_trees(
                    project_tree,
                    &logger.fork("DidRequestProjectTrees"),
                );
            }
            let (project_collection, config_file_registry) =
                project_collection_builder.finalize(&logger);
            (api_error, project_collection, config_file_registry)
        }));
        let (api_error, project_collection, config_file_registry) = match built {
            Ok(built) => built,
            Err(panic) => {
                project_collection_builder.parse_cache_journal.roll_back(&store.parse_cache);
                std::panic::resume_unwind(panic);
            }
        };

        let mut projects_with_new_program_structure: FxHashMap<ProjectID, bool> =
            FxHashMap::default();
        projects_with_new_program_structure.extend(
            project_collection.projects().into_iter().filter_map(|project| {
                if project.program_last_update == new_snapshot_id
                    && project.program_update_kind != ProgramUpdateKind::Cloned
                {
                    Some((
                        ProjectID(project.id().0),
                        project.program_update_kind == ProgramUpdateKind::NewFiles,
                    ))
                } else {
                    None
                }
            }),
        );

        // Clean cached files not touched by any open project on file open, close, delete,
        // or when explicitly requested (e.g. by an idle timer).
        let should_clean_file_cache = change.clean_file_cache
            || !change.file_changes.opened.0.is_empty()
            || !change.file_changes.reopened.0.is_empty()
            || !change.file_changes.closed.is_empty()
            || !change.file_changes.deleted.is_empty();
        if should_clean_file_cache {
            // The set of seen files can change only if a program was constructed (not cloned) during this snapshot.
            // When cleanFileCache is explicitly set, always attempt cleaning.
            if !projects_with_new_program_structure.is_empty() || change.clean_file_cache {
                let clean_files_start = Instant::now();
                let mut removed_files = 0;
                let projects = project_collection.projects();
                fs.cache_files.range(|entry| {
                    if projects.iter().any(|project| {
                        project
                            .host
                            .as_ref()
                            .is_some_and(|host| host.source_fs.seen_file(&entry.key()))
                    }) {
                        return true;
                    }
                    entry.delete();
                    removed_files += 1;
                    true
                });
                logger.logf(format_args!(
                    "Removed {} cached file(s) in {:?}",
                    removed_files,
                    clean_files_start.elapsed()
                ));
            }
        }

        let mut config = self.user_preferences.clone();
        if let Some(new_config) = &change.new_config {
            config = new_config.clone();
        }

        let project_collection = Arc::new(project_collection);
        let auto_import_host = new_auto_import_registry_clone_host(
            Arc::clone(&project_collection),
            Arc::clone(&store.parse_cache),
            Arc::clone(&fs),
            &store.options.current_directory,
            Arc::clone(&store.to_path),
        );
        let mut open_files: FxHashMap<Path, String> = FxHashMap::default();
        // pure map inserts; Go ranges the map too
        open_files.extend(
            overlays.iter().map(|(path, overlay)| (path.clone(), overlay.base.file_name.clone())),
        );
        let mut prepare_auto_imports = Path::default();
        let mut prepare_auto_imports_file_name = String::new();
        if !change.resource_request.auto_imports.0.is_empty() {
            prepare_auto_imports =
                change.resource_request.auto_imports.path(self.use_case_sensitive_file_names());
            prepare_auto_imports_file_name = change.resource_request.auto_imports.file_name();
        }
        let old_auto_imports = match &self.auto_imports {
            Some(auto_imports) => Arc::clone(auto_imports),
            None => {
                autoimport::new_registry(Arc::clone(&store.to_path), self.user_preferences.clone())
            }
        };
        let auto_imports = old_auto_imports.clone_registry(
            ctx,
            &autoimport::RegistryChange {
                requested_file: prepare_auto_imports,
                requested_file_name: prepare_auto_imports_file_name,
                open_files,
                changed: change.file_changes.changed.clone(),
                created: change.file_changes.created.clone(),
                deleted: change.file_changes.deleted.clone(),
                rebuilt_programs: projects_with_new_program_structure,
                user_preferences: change.new_config.clone(),
            },
            &auto_import_host,
            logger.fork("UpdateAutoImports"),
        );
        let auto_imports_watch = self
            .auto_imports_watch
            .as_ref()
            .map(|w| w.clone_with(auto_imports.node_modules_directories()));

        let (snapshot_fs, _) = fs.finalize();
        let snapshot_fs = Arc::new(snapshot_fs);
        let mut new_snapshot = store.new_snapshot(
            new_snapshot_id,
            Arc::clone(&snapshot_fs),
            Arc::default(),
            compiler_options_for_inferred_projects,
            config,
            Some(auto_imports),
            auto_imports_watch,
        );
        new_snapshot.parent_id = self.id;
        new_snapshot.project_collection = project_collection;
        new_snapshot.config_file_registry = config_file_registry;
        new_snapshot.inferred_project_content_mappers = inferred_content_mappers;
        new_snapshot.inferred_project_content_mapper_extensions =
            inferred_content_mapper_extensions;
        new_snapshot.builder_logs = logger.clone();
        new_snapshot.api_error = api_error;
        new_snapshot.file_system_override = change.file_system_override;
        new_snapshot.created_programs = project_collection_builder.take_created_programs();

        for project in new_snapshot.project_collection.projects() {
            if let Some(program) = project.program {
                store.program_counter.ref_(program);
                if project.program_last_update == new_snapshot_id {
                    // If the program was updated during this clone, the project and its host are new
                    // and still retain references to the builder. Freezing clears the builder reference
                    // so it's GC'd and to ensure the project can't access any data not already in the
                    // snapshot during use. This is pretty kludgy, but it's an artifact of Program design:
                    // Program has a single host, which is expected to implement a full vfs.FS, among
                    // other things. That host is *mostly* only used during program *construction*, but a
                    // few methods may get exercised during program *use*. So, our compiler host is allowed
                    // to access caches and perform mutating effects (like acquire referenced project
                    // config files) during snapshot building, and then we call `freeze` to ensure those
                    // mutations don't happen afterwards. In the future, we might improve things by
                    // separating what it takes to build a program from what it takes to use a program,
                    // and only pass the former into NewProgram instead of retaining it indefinitely.
                    project.host.as_ref().unwrap().freeze(
                        Arc::clone(&snapshot_fs),
                        Arc::clone(&new_snapshot.config_file_registry),
                    );
                }
            }
        }
        #[expect(
            clippy::iter_over_hash_type,
            reason = "only adds this snapshot as an owner (a set insert); Go ranges the map too"
        )]
        for config in new_snapshot.config_file_registry.configs.values() {
            if let Some(command_line) = config.command_line
                && let Some(config_file) = command_line.config_file
            {
                let extended_source_files = config_file.extended_source_files.lock().unwrap();
                for file in extended_source_files.iter() {
                    store.extended_config_cache.add_owner(&(store.to_path)(file), new_snapshot.id);
                }
            }
        }

        auto_import_host.dispose();

        logger.logf(format_args!(
            "Finished cloning snapshot {} into snapshot {} in {:?}",
            self.id,
            new_snapshot.id,
            start.elapsed()
        ));
        Arc::new(new_snapshot)
    }

    // snapshot.go:732
    // ref increments the snapshot's reference count, preventing it from being
    // disposed until a corresponding Deref is called. The snapshot must still
    // be alive (refCount > 0) when ref is called.
    pub(crate) fn ref_(&self) {
        if self
            .ref_count
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count > 0).then(|| count.checked_add(1)).flatten()
            })
            .is_err()
        {
            panic!("snapshot {}: ref on disposed snapshot, parentId={}", self.id, self.parent_id);
        }
    }

    // snapshot.go:741
    // tryRef attempts to increment the snapshot's reference count. If the
    // snapshot is already disposed (refCount == 0), it returns false without
    // modifying the count. On success the caller must eventually call Deref.
    pub(crate) fn try_ref(&self) -> bool {
        self.ref_count
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count > 0).then(|| count.checked_add(1)).flatten()
            })
            .is_ok()
    }

    // snapshot.go:755
    // Deref decrements the snapshot's reference count. When the count reaches
    // zero, the snapshot is disposed and its store-owned resources are released.
    pub fn deref(&self) {
        let previous = self
            .ref_count
            .try_update(Ordering::SeqCst, Ordering::SeqCst, |count| count.checked_sub(1))
            .unwrap_or_else(|_| {
                panic!("snapshot {}: ref count below zero, parentId={}", self.id, self.parent_id)
            });
        if previous == 1 {
            self.dispose();
        }
    }

    // snapshot.go:765
    fn dispose(&self) {
        let store = &self.host;
        for project in self.project_collection.projects() {
            let Some(program) = project.program else {
                continue;
            };
            if store.program_counter.deref(program) {
                // (Content mappers are not ported: no program has a content mapper project.)
                // This program is no longer referenced by any snapshot.
                // Mark its checker pool as discarded so its idle-cleanup timer stops
                // keeping the pool alive, allowing the pool and any idle checkers it
                // still references to be reclaimed when the pool is garbage-collected.
                if let Some(checker_pool) = &project.checker_pool {
                    checker_pool.discard();
                }
                for &file in program.source_files() {
                    store.parse_cache.deref(&parse_cache_key_for_file(file));
                }
                for file in program.duplicate_source_files() {
                    store.parse_cache.deref(&parse_cache_key_for_duplicate(file));
                }
            }
        }
        #[expect(
            clippy::iter_over_hash_type,
            reason = "owner releases on distinct keys commute; Go ranges the map too"
        )]
        for config in self.config_file_registry.configs.values() {
            if let Some(command_line) = config.command_line {
                for file in command_line.extended_source_files() {
                    store.extended_config_cache.release(&(store.to_path)(&file), self.id);
                }
            }
        }
    }

    pub fn ref_count(&self) -> u32 {
        self.ref_count.load(Ordering::SeqCst)
    }
}

// snapshot.go:193
fn overlay_file_handles(overlays: &OverlayMap) -> FxHashMap<Path, Arc<dyn FileHandle>> {
    let mut files: FxHashMap<Path, Arc<dyn FileHandle>> =
        FxHashMap::with_capacity_and_hasher(overlays.len(), rustc_hash::FxBuildHasher);
    // pure map inserts; Go ranges the map too
    files.extend(overlays.iter().map(|(path, overlay)| {
        (path.clone(), Arc::<crate::overlayfs::Overlay>::clone(overlay) as Arc<dyn FileHandle>)
    }));
    files
}

// ls.Host
impl tsrs_ls::Host for Snapshot {
    // snapshot.go:270
    fn use_case_sensitive_file_names(&self) -> bool {
        self.fs.fs.use_case_sensitive_file_names()
    }

    // snapshot.go:285
    fn read_file(&self, file_name: &str) -> Option<String> {
        let handle = self.get_file(file_name)?;
        Some(handle.content().to_string())
    }

    // snapshot.go:240
    fn converters(&self) -> Arc<Converters> {
        Arc::clone(&self.converters)
    }

    // snapshot.go:232
    fn get_preferences(&self, _active_file: &str) -> UserPreferences {
        self.user_preferences.clone()
    }

    // snapshot.go:225
    fn get_ecma_line_info(&self, file_name: &str) -> Option<Arc<ECMALineInfo>> {
        self.fs.get_file(file_name).map(|file| file.ecma_line_info())
    }

    // snapshot.go:244
    fn auto_import_registry(&self) -> Option<Arc<autoimport::Registry>> {
        self.auto_imports.clone()
    }

    // snapshot.go:305
    fn read_directory(
        &self,
        current_dir: &str,
        path: &str,
        extensions: &[String],
        excludes: &[String],
        includes: &[String],
        depth: usize,
    ) -> Vec<String> {
        vfsmatch::read_directory(
            &*self.fs.fs,
            current_dir,
            path,
            extensions,
            excludes,
            includes,
            depth,
        )
    }

    // snapshot.go:301
    fn get_directories(&self, path: &str) -> Vec<String> {
        self.fs.fs.get_accessible_entries(path).directories
    }

    // snapshot.go:293
    fn directory_exists(&self, path: &str) -> bool {
        self.fs.fs.directory_exists(path)
    }

    // snapshot.go:297
    fn file_exists(&self, path: &str) -> bool {
        self.fs.fs.file_exists(path)
    }
}

impl Snapshot {
    fn use_case_sensitive_file_names(&self) -> bool {
        tsrs_ls::Host::use_case_sensitive_file_names(self)
    }

    pub fn converters(&self) -> Arc<Converters> {
        Arc::clone(&self.converters)
    }

    pub fn read_file(&self, file_name: &str) -> Option<String> {
        tsrs_ls::Host::read_file(self, file_name)
    }

    pub fn auto_import_registry(&self) -> Option<Arc<autoimport::Registry>> {
        self.auto_imports.clone()
    }
}
