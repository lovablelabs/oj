use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, RwLock, Weak};
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;
use tsrs_ast::Diagnostic;
use tsrs_core::collections::{OrderedMap, Set, SyncSet, diff_ordered_maps};
use tsrs_core::context::{CancelFunc, Context, has_locale, with_locale};
use tsrs_core::tspath::{self, Path};
use tsrs_core::{CompilerOptions, P, ScriptKind, diff_maps_func};
use tsrs_ls::autoimport::{ProjectID, RegistryExt};
use tsrs_ls::lsconv::{self, Converters};
use tsrs_ls::lsutil::{self, UserPreferences};
use tsrs_ls::{LanguageService, new_language_service};
use tsrs_lsproto as lsproto;
use tsrs_tsoptions::Mapper;
use tsrs_vfs::FS;

use crate::ata;
use crate::background::{Queue, Timer, after_func, new_queue};
use crate::checkerpool::CheckerPoolOptions;
use crate::client::Client;
use crate::configfileregistry::ConfigFileEntry;
use crate::dirty::Shared;
use crate::filechange::{FileChange, FileChangeKind, FileChangeSummary, merge_file_change_summary};
use crate::logging::{Logger, new_nop_logger};
use crate::overlayfs::{FsRef, OverlayFS, OverlayMap, ToPath, new_overlay_fs};
use crate::parsecache::ParseCache;
use crate::project::{HR, ID, Kind, ProgramUpdateKind, Project};
use crate::snapshot::{
    APISnapshotRequest, ATAStateChange, ProjectTreeRequest, ResourceRequest, Snapshot,
    SnapshotChange,
};
use crate::snapshotfs::{is_node_modules_path, is_relevant_extension};
use crate::snapshothost::{SnapshotHost, new_snapshot_host};
use crate::watch::{
    WatchRegistry, WatchedFiles, WatchedFilesExt, WatcherID, file_system_watcher_glob_string,
    new_watch_registry,
};

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum UpdateReason {
    #[default]
    Unknown = 0,
    DidOpenFile,
    DidCloseFile,
    DidChangeCompilerOptionsForInferredProjects,
    RequestedLanguageServicePendingChanges,
    RequestedLanguageServiceProjectNotLoaded,
    RequestedLanguageServiceForFileNotOpen,
    RequestedLanguageServiceProjectDirty,
    RequestedLoadProjectTree,
    RequestedLanguageServiceWithAutoImports,
    IdleCleanDiskCache,
    DidChangeConfigFile,
    DidChangeContentMapperContributions,
}

// ErrNoProjectForUnknownScriptKind identifies requests for otherwise unsupported files.
// (Go `errors.New`; errors wrapping it carry this message as their prefix, see `is_err_no_project_for_unknown_script_kind`.)
const ERR_NO_PROJECT_FOR_UNKNOWN_SCRIPT_KIND: &str = "no project for unknown script kind";

// Go `errors.Is(err, ErrNoProjectForUnknownScriptKind)`.
pub fn is_err_no_project_for_unknown_script_kind(err: &lsproto::Error) -> bool {
    err.message.starts_with(ERR_NO_PROJECT_FOR_UNKNOWN_SCRIPT_KIND)
}

#[derive(Clone, Debug, Default)]
pub struct ContentMapperContributions {
    pub mappers: Vec<Mapper>,
    pub extensions: Vec<String>,
}

// watchRequestTimeout is the maximum time to wait for the client to respond to
// a WatchFiles or UnwatchFiles request while holding the watches mutex.
const WATCH_REQUEST_TIMEOUT: Duration = Duration::from_secs(1);

// SessionOptions are the immutable initialization options for a session.
// Snapshots may reference them as a pointer since they never change.
#[derive(Clone, Debug, Default)]
pub struct SessionOptions {
    pub current_directory: String,
    pub default_library_path: String,
    pub typings_location: String,
    pub position_encoding: lsproto::PositionEncodingKind,
    pub watch_enabled: bool,
    pub logging_enabled: bool,
    pub telemetry_enabled: bool,
    pub push_diagnostics_enabled: bool,
    // RunExternalCode allows configured content mappers to run their (external) processes,
    // gated on workspace trust by the client. It corresponds to the --runExternalCode CLI flag.
    pub run_external_code: bool,
    pub debounce_delay: Duration,
    pub checker_pool_options: CheckerPoolOptions,
}

// (Content mappers and ATA are not ported: Go's `Spawner` and `ContentMapperLogger` are omitted, and an
// `NpmExecutor` never starts a typings installer.)
pub struct SessionInit {
    pub background_ctx: Context,
    pub options: Arc<SessionOptions>,
    pub fs: Arc<dyn FS>,
    pub client: Option<Arc<dyn Client>>,
    pub logger: Option<Arc<dyn Logger>>,
    pub npm_executor: Option<Arc<dyn ata::NpmExecutor>>,
    pub parse_cache: Option<Arc<ParseCache>>,
}

struct UserConfigState {
    // current preferences
    workspace_user_preferences: UserPreferences,
    pending_user_config_changes: bool,
}

#[derive(Default)]
struct CancelState {
    cancel: Option<CancelFunc>,
    generation: u64,
}

#[derive(Default)]
struct IdleCacheCleanState {
    timer: Option<Timer>,
    generation: u64,
    pending: usize,
    closed: bool,
}

struct IdleCacheCleanDone(Arc<Session>);

impl Drop for IdleCacheCleanDone {
    fn drop(&mut self) {
        let mut state = self.0.idle_cache_clean.lock().unwrap();
        state.pending -= 1;
        if state.pending == 0 {
            self.0.idle_cache_clean_done.notify_all();
        }
    }
}

// Session manages the state of an LSP session. It receives textDocument
// events and requests for LanguageService objects from the LPS server
// and processes them into immutable snapshots as the data source for
// LanguageServices. When Session transitions from one snapshot to the
// next, it diffs them and updates file watchers and Automatic Type
// Acquisition (ATA) state accordingly.
//
// Shared as `Arc<Session>` (`new_session`); background tasks reach it through `self_ref`.
pub struct Session {
    // Go embeds *SnapshotHost.
    snapshot_host: Arc<SnapshotHost>,
    options: Arc<SessionOptions>,
    logger: Arc<dyn Logger>,
    background_ctx: Context,
    background_cancel: CancelFunc,
    to_path: ToPath,
    client: Option<Arc<dyn Client>>,
    #[expect(
        dead_code,
        reason = "Go's reader sendPerformanceTelemetry (session.go:737) is not ported yet"
    )]
    start_time: Instant,
    npm_executor: Option<Arc<dyn ata::NpmExecutor>>,
    fs: Arc<OverlayFS>,

    // registeredContentMapperSnapshotID is the ID of the newest snapshot whose registration has been
    // applied. Registration runs from background tasks that may finish out of order, so
    // contentMapperRegistrationMu serializes updates and the snapshot ID keeps a stale task from
    // overwriting a newer snapshot's registration.
    // Go: contentMapperRegistrationMu + registeredContentMapperExtensions + registeredContentMapperSnapshotID.
    content_mapper_registration: Mutex<(Option<Vec<String>>, u64)>,

    // read-only after initialization
    initial_user_preferences: Mutex<UserPreferences>,
    // Go: userConfigRWMu + workspaceUserPreferences + pendingUserConfigChanges.
    user_config: Mutex<UserConfigState>,
    compiler_options_for_inferred_projects: Mutex<Option<P<CompilerOptions>>>,
    background_queue: Queue,

    // snapshot is the current immutable state of all projects.
    snapshot: RwLock<Option<Arc<Snapshot>>>,
    snapshot_update_mu: Mutex<()>,

    // scheduledSnapshotUpdateCancel is the cancelation function for a scheduled
    // snapshot update. Snapshot updates are scheduled and debounced after file closes.
    scheduled_snapshot_update: Mutex<CancelState>,

    configure_mu: Mutex<()>,

    // pendingFileChanges are accumulated from textDocument/* events delivered
    // by the LSP server through DidOpenFile(), DidChangeFile(), etc. They are
    // applied to the next snapshot update.
    pub pending_file_changes: Mutex<Vec<FileChange>>,

    // pendingATAChanges are produced by Automatic Type Acquisition (ATA)
    // installations and applied to the next snapshot update.
    pending_ata_changes: Mutex<FxHashMap<ID, Arc<ATAStateChange>>>,

    // diagnosticsRefreshCancel is the cancelation function for a scheduled
    // diagnostics refresh. Diagnostics refreshes are scheduled and debounced
    // after file watch changes and ATA updates.
    diagnostics_refresh: Mutex<CancelState>,

    // warmAutoImportCancel is the cancelation function for a running
    // auto-import cache warming task. It is cancelled on file opens,
    // closes, changes, watched-file changes, new auto-import warming
    // requests, and when the session closes.
    warm_auto_import_cancel: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,

    // idleCacheCleanTimer is a resettable timer for scheduling idle disk
    // cache cleans. The timer resets on any file event (open, close,
    // change, save, watch) and fires after 30 seconds of inactivity.
    idle_cache_clean: Mutex<IdleCacheCleanState>,
    idle_cache_clean_done: Condvar,

    // performanceTelemetryCancel cancels the periodic performance telemetry ticker.
    performance_telemetry_cancel: Mutex<Option<CancelFunc>>,

    // seenProjects tracks projects that have already had telemetry sent.
    #[expect(
        dead_code,
        reason = "Go's reader sendProjectInfoTelemetry (session.go:875) is not ported yet"
    )]
    seen_projects: SyncSet<ID>,

    // watches tracks the current watch globs and how many individual WatchedFiles
    // are using each glob.
    watches: WatchRegistry,

    // globalDiagPublishPending is set to true when a global diagnostics publish
    // task should be enqueued. It is reset when the task runs, coalescing multiple
    // requests into a single background task.
    global_diag_publish_pending: AtomicBool,

    self_ref: Weak<Session>,
}

// session.go:210
pub fn new_session(init: &SessionInit) -> Arc<Session> {
    let snapshot_host = new_snapshot_host(init);
    let session_logger = init.logger.clone().unwrap_or_else(new_nop_logger);
    let relative_pattern_support = lsproto::get_client_capabilities(&init.background_ctx)
        .workspace
        .did_change_watched_files
        .relative_pattern_support;
    let snapshot = snapshot_host.new_root_snapshot_with(0, relative_pattern_support);
    let (background_ctx, background_cancel) = init.background_ctx.with_cancel();
    // (ATA is not ported: Go creates a typings installer here when TypingsLocation and NpmExecutor are set.)
    Arc::new_cyclic(|self_ref| Session {
        options: Arc::clone(&init.options),
        logger: session_logger,
        background_ctx,
        background_cancel,
        to_path: Arc::clone(&snapshot_host.to_path),
        client: init.client.clone(),
        npm_executor: init.npm_executor.clone(),
        fs: Arc::new(new_overlay_fs(
            FsRef::Host(Arc::clone(&snapshot_host.fs)),
            Arc::default(),
            init.options.position_encoding,
            Arc::clone(&snapshot_host.to_path),
        )),
        background_queue: new_queue(),
        start_time: Instant::now(),
        snapshot: RwLock::new(Some(snapshot)),
        initial_user_preferences: Mutex::new(lsutil::new_default_user_preferences()),
        user_config: Mutex::new(UserConfigState {
            workspace_user_preferences: lsutil::new_default_user_preferences(),
            pending_user_config_changes: false,
        }),
        pending_ata_changes: Mutex::new(FxHashMap::default()),
        watches: new_watch_registry(),
        snapshot_host,
        content_mapper_registration: Mutex::new((None, 0)),
        compiler_options_for_inferred_projects: Mutex::new(None),
        snapshot_update_mu: Mutex::new(()),
        scheduled_snapshot_update: Mutex::new(CancelState::default()),
        configure_mu: Mutex::new(()),
        pending_file_changes: Mutex::new(Vec::new()),
        diagnostics_refresh: Mutex::new(CancelState::default()),
        warm_auto_import_cancel: Mutex::new(None),
        idle_cache_clean: Mutex::new(IdleCacheCleanState::default()),
        idle_cache_clean_done: Condvar::new(),
        performance_telemetry_cancel: Mutex::new(None),
        seen_projects: SyncSet::default(),
        global_diag_publish_pending: AtomicBool::new(false),
        self_ref: Weak::clone(self_ref),
    })
}

impl Session {
    fn arc(&self) -> Arc<Session> {
        self.self_ref.upgrade().expect("session used after it was dropped")
    }

    pub fn snapshot_host(&self) -> &Arc<SnapshotHost> {
        &self.snapshot_host
    }

    // session.go:251
    // FS implements module.ResolutionHost

    /// `fs` as a shared handle (a Go `vfs.FS` is a reference), for an LSP-attached API session.
    pub fn shared_fs(&self) -> Arc<dyn FS> {
        Arc::clone(&self.fs) as Arc<dyn FS>
    }

    // session.go:256
    // GetCurrentDirectory implements module.ResolutionHost

    // session.go:260
    pub fn default_library_path(&self) -> &str {
        &self.options.default_library_path
    }

    // session.go:265
    // Gets copy of current configuration
    pub fn config(&self) -> UserPreferences {
        self.user_config.lock().unwrap().workspace_user_preferences.clone()
    }

    // session.go:271
    fn background_context(&self) -> Context {
        self.with_current_locale(&self.background_ctx)
    }

    // session.go:275
    pub fn with_current_locale(&self, ctx: &Context) -> Context {
        let Some(client) = &self.client else {
            return ctx.clone();
        };
        with_locale(ctx, client.get_locale())
    }

    // session.go:287
    pub fn configure(&self, config: &UserPreferences) {
        let _configure = self.configure_mu.lock().unwrap();
        let old_config = {
            let mut user_config = self.user_config.lock().unwrap();
            user_config.pending_user_config_changes = true;
            std::mem::replace(&mut user_config.workspace_user_preferences, config.clone())
        };

        if !config.locale.is_empty()
            && let Some(client) = &self.client
        {
            // (Content mappers are not ported: Go forwards a changed locale to the content mapper host.)
            client.set_locale(&config.locale);
        }

        // Tell the client to re-request certain commands depending on user preference changes.
        self.refresh_inlay_hints_if_needed(&old_config, config);
        self.refresh_code_lens_if_needed(&old_config, config);
        self.refresh_diagnostics_if_needed(&old_config, config);
        self.refresh_ata_if_needed(&old_config, config);
    }

    // session.go:314
    // (Go stores initialUserPreferences before calling Configure; Configure does not read it.)
    pub fn initialize_with_user_config(&self, config: UserPreferences) {
        self.configure(&config);
        *self.initial_user_preferences.lock().unwrap() = config;
    }

    // session.go:319
    pub fn did_open_file(
        &self,
        ctx: &Context,
        uri: lsproto::DocumentUri,
        version: i32,
        content: String,
        language_kind: lsproto::LanguageKind,
    ) {
        self.cancel_warm_auto_import_cache();
        self.schedule_idle_cache_clean();
        self.cancel_scheduled_snapshot_update();
        let _update = self.snapshot_update_mu.lock().unwrap();
        let mut pending = self.pending_file_changes.lock().unwrap();
        pending.push(FileChange {
            kind: FileChangeKind::Open,
            uri: uri.clone(),
            version,
            content,
            language_kind,
            changes: Vec::new(),
        });
        let (changes, overlays) = self.flush_changes_locked(ctx, &mut pending);
        drop(pending);
        self.update_snapshot(
            ctx,
            overlays,
            SnapshotChange {
                reason: UpdateReason::DidOpenFile,
                file_changes: changes,
                resource_request: ResourceRequest { documents: vec![uri], ..Default::default() },
                ..Default::default()
            },
        );
    }

    // session.go:344
    // SetContentMapperContributions atomically replaces extension-provided inferred-project mappers and
    // discovers configured projects for matching open documents. Configured projects never consume these mappers.
    pub fn set_content_mapper_contributions(
        &self,
        ctx: &Context,
        contributions: ContentMapperContributions,
        document_uris: Vec<lsproto::DocumentUri>,
    ) {
        if !self.options.run_external_code {
            return;
        }
        self.cancel_scheduled_snapshot_update();
        let _update = self.snapshot_update_mu.lock().unwrap();
        let mut pending = self.pending_file_changes.lock().unwrap();
        let (changes, overlays) = self.flush_changes_locked(ctx, &mut pending);
        drop(pending);
        self.update_snapshot(
            ctx,
            overlays,
            SnapshotChange {
                reason: UpdateReason::DidChangeContentMapperContributions,
                file_changes: changes,
                content_mapper_contributions: Some(contributions),
                resource_request: ResourceRequest {
                    configured_project_documents: document_uris,
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        let _ = self.update_content_mapper_registrations(ctx, &self.snapshot());
    }

    // session.go:363
    pub fn did_close_file(&self, _ctx: &Context, uri: lsproto::DocumentUri) {
        self.cancel_warm_auto_import_cache();
        self.schedule_idle_cache_clean();
        self.pending_file_changes.lock().unwrap().push(FileChange {
            kind: FileChangeKind::Close,
            uri,
            ..Default::default()
        });
        self.schedule_snapshot_update(UpdateReason::DidCloseFile);
    }

    // session.go:375
    pub fn did_change_file(
        &self,
        _ctx: &Context,
        uri: lsproto::DocumentUri,
        version: i32,
        changes: Vec<lsproto::TextDocumentContentChangePartialOrWholeDocument>,
    ) {
        self.cancel_warm_auto_import_cache();
        self.schedule_idle_cache_clean();
        let is_content_mapper_file = self.is_content_mapper_file(&uri);
        self.pending_file_changes.lock().unwrap().push(FileChange {
            kind: FileChangeKind::Change,
            uri,
            version,
            changes,
            ..Default::default()
        });

        // Editing a content-mapped file changes the program like any source edit, but the client's
        // pull-diagnostics machinery won't re-request diagnostics for dependent files: the content-mapped file is not
        // in the diagnostic provider's document selector, so a change to it never triggers the client's
        // inter-file re-pull. Prompt a workspace refresh so dependents update. We skip the debounce here so the
        // edit doesn't feel sluggish (normal source edits are pulled per-keystroke client-side); the refresh is
        // still coalesced. Ordinary source files are handled entirely client-side, so we cancel any pending
        // refresh for them as before.
        if is_content_mapper_file {
            self.schedule_diagnostics_refresh_with_delay(Duration::ZERO);
        } else {
            self.cancel_diagnostics_refresh();
        }
    }

    // session.go:403
    // isContentMapperFile reports whether uri is a content-mapped file handled by a configured content mapper, based
    // on the extensions currently registered with the client for text document synchronization.
    fn is_content_mapper_file(&self, uri: &lsproto::DocumentUri) -> bool {
        let snapshot = self.snapshot();
        let configured = snapshot.config_file_registry.content_mappers();
        let mut extensions = configured.extensions.clone();
        extensions.extend(snapshot.inferred_project_content_mapper_extensions.iter().cloned());
        let extensions: Vec<&str> = extensions.iter().map(std::string::String::as_str).collect();
        tspath::file_extension_is_one_of(&uri.file_name(), &extensions)
    }

    // session.go:410
    pub fn did_save_file(&self, _ctx: &Context, uri: lsproto::DocumentUri) {
        self.schedule_idle_cache_clean();
        self.pending_file_changes.lock().unwrap().push(FileChange {
            kind: FileChangeKind::Save,
            uri,
            ..Default::default()
        });
    }

    // session.go:420
    pub fn did_change_watched_files(&self, _ctx: &Context, changes: &[lsproto::FileEvent]) {
        let mut file_changes: Vec<FileChange> = Vec::with_capacity(changes.len());
        let mut has_relevant_change = false;
        let mut has_config_change = false;
        let snapshot = self.snapshot();
        let config_file_registry = Arc::clone(&snapshot.config_file_registry);
        let (content_mapper_extensions, content_mapper_watched_files) =
            snapshot.content_mapper_watch_state();
        for change in changes {
            let kind = match change.type_ {
                lsproto::FileChangeType::CREATED => FileChangeKind::WatchCreate,
                lsproto::FileChangeType::CHANGED => FileChangeKind::WatchChange,
                lsproto::FileChangeType::DELETED => FileChangeKind::WatchDelete,
                _ => continue, // Ignore unknown change types.
            };
            file_changes.push(FileChange { kind, uri: change.uri.clone(), ..Default::default() });

            if !has_config_change
                && config_file_registry.is_tracked(&(self.to_path)(&change.uri.file_name()))
            {
                has_config_change = true;
            }

            if !has_relevant_change {
                let file_name = change.uri.file_name();
                let path = (self.to_path)(&file_name).remove_trailing_directory_separator();
                let path_str: &str = &path;
                if content_mapper_watched_files.has(&path) {
                    has_relevant_change = true;
                    continue;
                }
                let i = path_str.rfind('.');
                if i.is_none() || path_str.rfind('/') > i {
                    // Extensionless paths might be directories.
                    // For creations/changes, we can check the file system.
                    // For deletions, consult the current snapshot cache to avoid treating extensionless file deletions as relevant.
                    if kind == FileChangeKind::WatchDelete {
                        let snapshot = self.snapshot();
                        if snapshot.fs.cache_directories.contains_key(&path)
                            || snapshot.has_overlay_within(&path)
                            || is_node_modules_path(&path)
                        {
                            has_relevant_change = true;
                        }
                    } else {
                        has_relevant_change = self.fs.directory_exists(&file_name);
                    }
                } else if let Some(i) = i {
                    let extensions: Vec<&str> =
                        content_mapper_extensions.iter().map(std::string::String::as_str).collect();
                    if is_relevant_extension(&path_str[i..])
                        || tspath::file_extension_is_one_of(path_str, &extensions)
                    {
                        has_relevant_change = true;
                    }
                }
            }
        }

        self.pending_file_changes.lock().unwrap().extend(file_changes);

        if has_relevant_change {
            // Schedule a debounced diagnostics refresh only for paths
            // that can affect the TypeScript program (relevant extensions or directories).
            self.schedule_diagnostics_refresh();
        }
        if has_config_change {
            // Config file diagnostics are pushed on snapshot updates rather than pulled,
            // so they must not depend on the client re-pulling diagnostics in response to
            // the refresh request above.
            self.schedule_snapshot_update(UpdateReason::DidChangeConfigFile);
        }
        self.cancel_warm_auto_import_cache();
        self.schedule_idle_cache_clean();
    }

    // session.go:499
    pub fn did_change_compiler_options_for_inferred_projects(
        &self,
        ctx: &Context,
        options: Option<P<CompilerOptions>>,
    ) {
        *self.compiler_options_for_inferred_projects.lock().unwrap() = options;
        self.update_snapshot(
            ctx,
            tsrs_overlays(&self.fs),
            SnapshotChange {
                reason: UpdateReason::DidChangeCompilerOptionsForInferredProjects,
                compiler_options_for_inferred_projects: options,
                ..Default::default()
            },
        );
    }

    // session.go:507
    fn schedule_diagnostics_refresh(&self) {
        self.schedule_diagnostics_refresh_with_delay(self.options.debounce_delay);
    }

    // session.go:514
    // scheduleDiagnosticsRefresh schedules a coalesced workspace diagnostics refresh after delay. A delay of
    // 0 refreshes as soon as the background queue runs the task; it is used for interactive edits (e.g. a
    // content-mapped file) where the debounce would make dependent-file diagnostics feel sluggish.
    fn schedule_diagnostics_refresh_with_delay(&self, delay: Duration) {
        let mut st = self.diagnostics_refresh.lock().unwrap();

        // Cancel any existing scheduled diagnostics refresh
        if let Some(cancel) = &st.cancel {
            cancel.call();
            self.logger.log("Delaying scheduled diagnostics refresh...");
        } else {
            self.logger.log("Scheduling new diagnostics refresh...");
        }

        // Create a new cancellable context for the debounce task
        let (debounce_ctx, cancel) = self.background_context().with_cancel();
        st.generation += 1;
        let generation = st.generation;
        st.cancel = Some(cancel.clone());

        // Enqueue the (optionally debounced) diagnostics refresh
        let s = self.arc();
        self.background_queue.enqueue(&debounce_ctx, move |ctx| {
            let _cancel = CancelOnDrop(cancel);
            if !delay.is_zero() {
                // Wait out the debounce window; a newer event cancels this one.
                if ctx.wait_timeout(delay) {
                    // Context was cancelled, newer events arrived
                    return;
                }
                // Delay completed, proceed with refresh
            } else if ctx.err().is_some() {
                return;
            }

            // Clear the cancel function since we're about to execute the refresh
            {
                let mut st = s.diagnostics_refresh.lock().unwrap();
                if st.generation != generation {
                    return;
                }
                st.cancel = None;
            }

            if s.options.logging_enabled {
                s.logger.log("Running scheduled diagnostics refresh");
            }
            if let Some(client) = &s.client
                && let Err(err) = client.refresh_diagnostics(&s.background_context())
                && s.options.logging_enabled
            {
                s.logger.logf(format_args!("Error refreshing diagnostics: {err}"));
            }
        });
    }

    // session.go:566
    fn cancel_diagnostics_refresh(&self) {
        let mut st = self.diagnostics_refresh.lock().unwrap();
        if let Some(cancel) = st.cancel.take() {
            cancel.call();
            self.logger.log("Canceled scheduled diagnostics refresh");
            st.generation += 1;
        }
    }

    // session.go:577
    fn schedule_snapshot_update(&self, reason: UpdateReason) {
        let mut st = self.scheduled_snapshot_update.lock().unwrap();

        // Cancel any existing scheduled snapshot update
        if let Some(cancel) = &st.cancel {
            cancel.call();
            if self.options.logging_enabled {
                self.logger.log("Delaying scheduled snapshot update...");
            }
        } else if self.options.logging_enabled {
            self.logger.log("Scheduling new snapshot update...");
        }

        // Create a new cancellable context for the debounce task
        let (debounce_ctx, cancel) = self.background_context().with_cancel();
        st.generation += 1;
        let generation = st.generation;
        st.cancel = Some(cancel.clone());

        // Enqueue the debounced snapshot update
        let s = self.arc();
        self.background_queue.enqueue(&debounce_ctx, move |ctx| {
            let _cancel = CancelOnDrop(cancel);
            // Sleep for the debounce delay
            if ctx.wait_timeout(s.options.debounce_delay) {
                // Context was cancelled, newer events arrived or another snapshot update ran
                return;
            }

            // Clear the cancel function since we're about to execute the update
            {
                let mut st = s.scheduled_snapshot_update.lock().unwrap();
                if st.generation != generation {
                    return;
                }
                st.cancel = None;
            }

            if s.options.logging_enabled {
                s.logger.log("Running scheduled snapshot update");
            }

            let _update = s.snapshot_update_mu.lock().unwrap();

            let (file_changes, overlays, ata_changes, new_config) = s.flush_changes(ctx);
            if file_changes.is_empty() && ata_changes.is_empty() && new_config.is_none() {
                return;
            }

            s.update_snapshot(
                ctx,
                overlays,
                SnapshotChange {
                    reason,
                    file_changes,
                    new_config,
                    ata_changes,
                    ..Default::default()
                },
            );
        });
    }

    // session.go:639
    fn cancel_scheduled_snapshot_update(&self) {
        let mut st = self.scheduled_snapshot_update.lock().unwrap();
        if let Some(cancel) = st.cancel.take() {
            cancel.call();
            if self.options.logging_enabled {
                self.logger.log("Canceled scheduled snapshot update");
            }
            st.generation += 1;
        }
    }

    // session.go:652
    #[expect(
        clippy::significant_drop_in_scrutinee,
        reason = "Go holds warmAutoImportMu while it cancels"
    )]
    fn cancel_warm_auto_import_cache(&self) {
        if let Some(cancel) = self.warm_auto_import_cancel.lock().unwrap().take() {
            cancel();
        }
    }

    // session.go:663
    fn schedule_idle_cache_clean(&self) {
        let mut state = self.idle_cache_clean.lock().unwrap();
        if state.closed {
            return;
        }
        if let Some(timer) = state.timer.take()
            && timer.stop()
        {
            state.pending -= 1;
        }

        state.generation += 1;
        state.pending += 1;
        let generation = state.generation;

        let s = Weak::clone(&self.self_ref);
        state.timer = Some(after_func(IDLE_CACHE_CLEAN_DELAY, move || {
            let Some(s) = s.upgrade() else {
                return;
            };
            let _done = IdleCacheCleanDone(Arc::clone(&s));
            {
                let mut state = s.idle_cache_clean.lock().unwrap();
                if state.generation == generation {
                    state.timer = None;
                }
            }

            let _update = s.snapshot_update_mu.lock().unwrap();
            if s.background_ctx.is_canceled() {
                return;
            }
            s.cancel_scheduled_snapshot_update();

            let ctx = s.background_context();
            let (file_changes, overlays, ata_changes, new_config) = s.flush_changes(&ctx);
            s.update_snapshot(
                &ctx,
                overlays,
                SnapshotChange {
                    reason: UpdateReason::IdleCleanDiskCache,
                    file_changes,
                    ata_changes,
                    new_config,
                    clean_file_cache: true,
                    ..Default::default()
                },
            );
            // (Go also starts a GC here.)
        }));
    }

    // session.go:694
    fn cancel_idle_cache_clean(&self) {
        let mut state = self.idle_cache_clean.lock().unwrap();
        if let Some(timer) = state.timer.take()
            && timer.stop()
        {
            state.pending -= 1;
            if state.pending == 0 {
                self.idle_cache_clean_done.notify_all();
            }
        }
    }

    // session.go:720
    fn close_idle_cache_clean(&self) {
        let mut state = self.idle_cache_clean.lock().unwrap();
        state.closed = true;
        if let Some(timer) = state.timer.take()
            && timer.stop()
        {
            state.pending -= 1;
        }
        while state.pending != 0 {
            state = self.idle_cache_clean_done.wait(state).unwrap();
        }
    }

    // session.go:707
    // StartPerformanceTelemetry begins periodic collection and sending of performance
    // telemetry. It should be called once after the session is initialized.
    // (Telemetry is not ported: the hook is kept and nothing is sent.)
    pub fn start_performance_telemetry(&self) {
        if !self.options.telemetry_enabled {}
    }

    // session.go:730
    fn stop_performance_telemetry(&self) {
        let cancel = self.performance_telemetry_cancel.lock().unwrap().take();
        if let Some(cancel) = cancel {
            cancel.call();
        }
    }

    // session.go:859
    // (Telemetry is not ported: the hook is kept and nothing is sent.)
    fn send_project_info_telemetry_for_new_projects(
        &self,
        _old_snapshot: &Snapshot,
        _new_snapshot: &Snapshot,
    ) {
        if !self.options.telemetry_enabled {}
    }

    // session.go:1018
    pub fn snapshot(&self) -> Arc<Snapshot> {
        Arc::clone(self.snapshot.read().unwrap().as_ref().expect("session is closed"))
    }

    // session.go:1028
    // getSnapshot flushes pending changes and updates the session's snapshot
    // if needed for the given request. When callerRef is true, the returned
    // snapshot has an extra reference for the caller (taken atomically under
    // snapshotMu), guaranteeing it stays alive until the caller calls Deref.
    fn get_snapshot(
        &self,
        ctx: &Context,
        request: ResourceRequest,
        caller_ref: bool,
    ) -> Arc<Snapshot> {
        let _update = self.snapshot_update_mu.lock().unwrap();
        self.cancel_scheduled_snapshot_update();

        let (file_changes, overlays, ata_changes, new_config) = self.flush_changes(ctx);
        let update_snapshot =
            !file_changes.is_empty() || !ata_changes.is_empty() || new_config.is_some();
        if update_snapshot {
            // If there are pending file changes, we need to update the snapshot.
            // Sending the requested URI ensures that the project for this URI is loaded.
            return self
                .update_snapshot_with(
                    ctx,
                    overlays,
                    SnapshotChange {
                        reason: UpdateReason::RequestedLanguageServicePendingChanges,
                        file_changes,
                        ata_changes,
                        new_config,
                        resource_request: request,
                        ..Default::default()
                    },
                    caller_ref,
                )
                .unwrap();
        }
        // If there are no pending file changes, we can try to use the current snapshot.
        let guard = self.snapshot.read().unwrap();
        let snapshot = Arc::clone(guard.as_ref().expect("session is closed"));
        let mut update_reason = UpdateReason::Unknown;
        if !request.projects.is_empty() {
            update_reason = UpdateReason::RequestedLanguageServiceProjectDirty;
        } else if request.project_tree.is_some() {
            update_reason = UpdateReason::RequestedLoadProjectTree;
        } else if !request.auto_imports.0.is_empty() {
            update_reason = UpdateReason::RequestedLanguageServiceWithAutoImports;
        } else {
            for document in &request.documents {
                match snapshot.get_default_project(document) {
                    None => update_reason = UpdateReason::RequestedLanguageServiceProjectNotLoaded,
                    Some(project) if project.dirty => {
                        update_reason = UpdateReason::RequestedLanguageServiceProjectDirty;
                    }
                    Some(_) => {}
                }
            }
            if update_reason == UpdateReason::Unknown {
                for document in &request.configured_project_documents {
                    if snapshot.is_open_file(&document.file_name()) {
                        match snapshot.get_default_project(document) {
                            None => {
                                update_reason =
                                    UpdateReason::RequestedLanguageServiceProjectNotLoaded;
                            }
                            Some(project) if project.dirty => {
                                update_reason = UpdateReason::RequestedLanguageServiceProjectDirty;
                            }
                            Some(_) => {}
                        }
                    } else {
                        update_reason = UpdateReason::RequestedLanguageServiceForFileNotOpen;
                    }
                }
            }
        }
        if update_reason == UpdateReason::Unknown {
            if caller_ref {
                snapshot.ref_();
            }
            drop(guard);
            return snapshot;
        }

        drop(guard);
        self.update_snapshot_with(
            ctx,
            overlays,
            SnapshotChange {
                reason: update_reason,
                resource_request: request,
                ..Default::default()
            },
            caller_ref,
        )
        .unwrap()
    }

    // session.go:1099
    fn get_snapshot_and_default_project(
        &self,
        ctx: &Context,
        uri: &lsproto::DocumentUri,
        caller_ref: bool,
    ) -> Result<(Arc<Snapshot>, Shared<Project>, Arc<LanguageService>), lsproto::Error> {
        let snapshot = self.get_snapshot(
            ctx,
            ResourceRequest { documents: vec![uri.clone()], ..Default::default() },
            caller_ref,
        );
        let Some(project) = snapshot.get_default_project(uri) else {
            if caller_ref {
                snapshot.deref();
            }
            if let Some(file) = snapshot.get_file(&uri.file_name())
                && file.kind() == ScriptKind::Unknown
            {
                return Err(lsproto::Error::new(format!(
                    "{ERR_NO_PROJECT_FOR_UNKNOWN_SCRIPT_KIND}: no project found for URI {uri}"
                )));
            }
            return Err(lsproto::Error::new(format!("no project found for URI {uri}")));
        };
        let language_service = Arc::new(new_language_service(
            ProjectID(project.id().0),
            project.get_program().unwrap(),
            Arc::<Snapshot>::clone(&snapshot),
            &uri.file_name(),
        ));
        Ok((snapshot, project, language_service))
    }

    // session.go:1118
    pub fn get_language_service(
        &self,
        ctx: &Context,
        uri: &lsproto::DocumentUri,
    ) -> Result<Arc<LanguageService>, lsproto::Error> {
        let (_, _, language_service) =
            self.get_snapshot_and_default_project(ctx, uri, false /*callerRef*/)?;
        Ok(language_service)
    }

    // session.go:1126
    pub fn get_language_service_and_projects_for_file(
        &self,
        ctx: &Context,
        uri: &lsproto::DocumentUri,
    ) -> Result<
        (Shared<Project>, Arc<LanguageService>, Vec<Arc<dyn tsrs_ls::Project>>),
        lsproto::Error,
    > {
        let (snapshot, project, default_ls) =
            self.get_snapshot_and_default_project(ctx, uri, false /*callerRef*/)?;
        // !!! TODO: sheetal:  Get other projects that contain the file with symlink
        let all_projects = snapshot.get_language_service_projects_containing_file(uri);
        Ok((project, default_ls, all_projects))
    }

    // session.go:1136
    pub fn get_projects_for_file(
        &self,
        ctx: &Context,
        uri: &lsproto::DocumentUri,
    ) -> Vec<Arc<dyn tsrs_ls::Project>> {
        let snapshot = self.get_snapshot(
            ctx,
            ResourceRequest {
                configured_project_documents: vec![uri.clone()],
                ..Default::default()
            },
            false, /*callerRef*/
        );

        // !!! TODO: sheetal:  Get other projects that contain the file with symlink

        snapshot.get_language_service_projects_containing_file(uri)
    }

    // session.go:1153
    // GetLanguageServicesForDocumentsLoadingProjectTree returns language services for
    // every project in the snapshot, loading all project trees first so that projects
    // that were never opened but reference the given documents are included. Loading the
    // trees is expensive, so this should only be used by operations that need to touch
    // every project in a solution, like file rename.
    pub fn get_language_services_for_documents_loading_project_tree(
        &self,
        ctx: &Context,
        uris: &[lsproto::DocumentUri],
    ) -> Vec<Arc<LanguageService>> {
        let snapshot = self.get_snapshot(
            ctx,
            ResourceRequest {
                documents: uris.to_vec(),
                project_tree: Some(ProjectTreeRequest::default()),
                ..Default::default()
            },
            false, /*callerRef*/
        );

        let mut active_file = String::new();
        if !uris.is_empty() {
            active_file = uris[0].file_name();
        }

        let projects = snapshot.project_collection.language_service_projects();
        let mut services = Vec::with_capacity(projects.len());
        services.extend(projects.into_iter().filter_map(|project| {
            let program = project.get_program()?;

            Some(Arc::new(new_language_service(
                ProjectID(project.id().0),
                program,
                Arc::<Snapshot>::clone(&snapshot),
                &active_file,
            )))
        }));
        services
    }

    // session.go:1181
    pub fn get_language_service_for_project_with_file(
        &self,
        ctx: &Context,
        project: &Project,
        uri: &lsproto::DocumentUri,
    ) -> Option<Arc<LanguageService>> {
        let snapshot = self.get_snapshot(
            ctx,
            ResourceRequest { projects: vec![project.id()], ..Default::default() },
            false, /*callerRef*/
        );
        // Ensure we have updated project
        let project = snapshot.project_collection.get_project(&project.id())?;
        // if program doesnt contain this file any more ignore it
        if !project.has_file(&uri.file_name()) {
            return None;
        }
        Some(Arc::new(new_language_service(
            ProjectID(project.id().0),
            project.get_program().unwrap(),
            snapshot,
            &uri.file_name(),
        )))
    }

    // session.go:1202
    // WithSnapshotLoadingProjectTree acquires a ref'd snapshot with the
    // requested project trees loaded, then calls fn. The snapshot stays alive
    // for the duration of fn.
    pub fn with_snapshot_loading_project_tree(
        &self,
        ctx: &Context,
        requested_project_trees: Option<Set<Path>>,
        f: impl FnOnce(&Arc<Snapshot>),
    ) {
        let snapshot = self.get_snapshot(
            ctx,
            ResourceRequest {
                project_tree: Some(ProjectTreeRequest::new(requested_project_trees)),
                ..Default::default()
            },
            true, /*callerRef*/
        );
        let _deref = DerefOnDrop(&snapshot);
        f(&snapshot);
    }

    // session.go:1216
    pub fn with_snapshot_for_document(
        &self,
        ctx: &Context,
        uri: &lsproto::DocumentUri,
        f: impl FnOnce(&Arc<Snapshot>),
    ) {
        let snapshot = self.get_snapshot(
            ctx,
            ResourceRequest { documents: vec![uri.clone()], ..Default::default() },
            true, /*callerRef*/
        );
        let _deref = DerefOnDrop(&snapshot);
        f(&snapshot);
    }

    // session.go:1235
    // GetCurrentLanguageServiceWithAutoImports flushes pending file changes, clones the
    // current snapshot with auto-import preparation for the given URI, then returns a
    // LanguageService for the default project. Use this only outside of request handling
    // (e.g. cache warming). For request handlers, use GetLanguageServiceWithAutoImports
    // with the request-level snapshot instead.

    // session.go:1257
    // WithLanguageServiceAndSnapshot synchronously acquires a ref'd snapshot and
    // creates a language service for the given URI. fn receives both the language
    // service and the backing snapshot so it can clone the snapshot (e.g. to
    // enable auto-imports). The snapshot is kept alive until the async work
    // completes.
    //
    // Only use this method when the callback needs direct access to the snapshot.
    // For handlers that only need a LanguageService, use GetLanguageService
    // directly—language services continue to work even after their backing
    // snapshot has been disposed.
    pub fn with_language_service_and_snapshot(
        &self,
        ctx: &Context,
        uri: &lsproto::DocumentUri,
        f: impl FnOnce(Arc<LanguageService>, Arc<Snapshot>) -> Result<Option<AsyncWork>, lsproto::Error>,
    ) -> Result<Option<AsyncWork>, lsproto::Error> {
        let (snapshot, _, language_service) =
            self.get_snapshot_and_default_project(ctx, uri, true /*callerRef*/)?;
        let async_work = f(language_service, Arc::clone(&snapshot));
        match async_work {
            Err(err) => {
                snapshot.deref();
                Err(err)
            }
            Ok(None) => {
                snapshot.deref();
                Ok(None)
            }
            Ok(Some(async_work)) => Ok(Some(Box::new(move || {
                let _deref = DerefOnDrop(&snapshot);
                async_work()
            }))),
        }
    }

    // session.go:1281
    // GetLanguageServiceWithAutoImports clones the given snapshot with auto-import
    // preparation for the given URI, without flushing pending file changes.
    // The cloned snapshot will be adopted as the session's current snapshot in the background
    // if other changes haven't been adopted in the meantime.
    pub fn get_language_service_with_auto_imports(
        &self,
        ctx: &Context,
        base_snapshot: &Arc<Snapshot>,
        uri: &lsproto::DocumentUri,
    ) -> Result<Arc<LanguageService>, lsproto::Error> {
        let new_snapshot = self.snapshot_host.clone_snapshot_with_auto_imports(
            ctx,
            base_snapshot,
            uri,
            Some(&*self.logger),
        );
        let Some(project) = new_snapshot.get_default_project(uri) else {
            // Clone's initial ref (1) is released since we won't use this snapshot.
            new_snapshot.deref();
            return Err(lsproto::Error::new(format!("no project found for URI {uri}")));
        };

        self.try_adopt_snapshot_change_in_background(
            Arc::clone(base_snapshot),
            Arc::clone(&new_snapshot),
        );

        Ok(Arc::new(new_language_service(
            ProjectID(project.id().0),
            project.get_program().unwrap(),
            new_snapshot,
            &uri.file_name(),
        )))
    }

    // session.go:1295
    fn try_adopt_snapshot_change_in_background(
        &self,
        base_snapshot: Arc<Snapshot>,
        new_snapshot: Arc<Snapshot>,
    ) {
        // The clone's initial ref (1) is transferred to adoptSnapshotChange,
        // which will either promote it as the session's current snapshot or
        // release it if the session has moved on.
        let s = self.arc();
        self.background_queue.enqueue(&self.background_context(), move |_ctx| {
            s.adopt_snapshot_change(&base_snapshot, &new_snapshot);
        });
    }

    // api.go:18
    // APIUpdate creates a new snapshot incorporating the given file changes and the
    // supplied API open/close request. The apiRequest may open or close projects and
    // files; opens are tracked in the snapshot (ref-counted) so they persist across
    // future updates, and closes release a previously taken ref. Programs are updated
    // only when explicitly requested by an open or ensure operation.
    // On success, returns a ref'd snapshot which the caller must Deref when done.
    // On failure, releases the rejected snapshot and returns nil and the error.
    // A snapshot with an API error is never adopted as canonical session state;
    // host changes flushed alongside it are adopted separately.
    pub fn api_update(
        &self,
        ctx: &Context,
        api_file_changes: &FileChangeSummary,
        api_request: Option<Arc<APISnapshotRequest>>,
    ) -> Result<Arc<Snapshot>, lsproto::Error> {
        let _update = self.snapshot_update_mu.lock().unwrap();
        self.cancel_scheduled_snapshot_update();

        let (host_file_changes, overlays, ata_changes, _) = self.flush_changes(ctx);
        let mut file_changes = host_file_changes.clone_summary();
        merge_file_change_summary(&mut file_changes, api_file_changes);
        let mut fs = None;
        let mut replace_file_system = false;
        if let Some(api_request) = &api_request {
            // Go passes the request file system as a `vfs.FS`; a layered one takes precedence, as in
            // `SnapshotHost::clone_snapshot`.
            fs = match &api_request.layered_file_system {
                Some(layered) => Some(FsRef::Layered(Arc::clone(layered))),
                None => api_request.file_system.clone().map(FsRef::Host),
            };
            replace_file_system = api_request.replace_file_system;
        }

        let file_system_override = fs.is_some();
        let new_snapshot = self.update_snapshot_ref(
            ctx,
            Arc::clone(&overlays),
            SnapshotChange {
                api_request,
                fs,
                file_system_override,
                replace_file_system,
                file_changes,
                ata_changes: ata_changes.clone(),
                ..Default::default()
            },
        );
        if let Some(api_error) = new_snapshot.api_error.clone() {
            new_snapshot.deref();
            if !host_file_changes.is_empty() || !ata_changes.is_empty() {
                // The API request is rejected as a unit, but host changes were already
                // flushed and must still advance the canonical session snapshot.
                self.update_snapshot(
                    ctx,
                    overlays,
                    SnapshotChange {
                        file_changes: host_file_changes,
                        ata_changes,
                        ..Default::default()
                    },
                );
            }
            return Err(api_error);
        }
        Ok(new_snapshot)
    }

    // api.go:59
    // TryAdoptSnapshotInBackground retains a derived snapshot and attempts to adopt it
    // as the session's current snapshot without blocking the caller.
    pub fn try_adopt_snapshot_in_background(
        &self,
        base_snapshot: Arc<Snapshot>,
        new_snapshot: Arc<Snapshot>,
    ) {
        self.snapshot_host.retain_snapshot(&new_snapshot);
        self.try_adopt_snapshot_change_in_background(base_snapshot, new_snapshot);
    }

    // session.go:1308
    // adoptSnapshotChange promotes a cloned snapshot as the session's current
    // snapshot so future requests benefit from the work already done. If the
    // session has moved on, the snapshot is discarded; the next request needing
    // auto-imports will redo the work on the latest snapshot.
    fn adopt_snapshot_change(&self, base_snapshot: &Arc<Snapshot>, new_snapshot: &Arc<Snapshot>) {
        let mut guard = self.snapshot.write().unwrap();
        let old_snapshot = Arc::clone(guard.as_ref().expect("session is closed"));
        if Arc::ptr_eq(&old_snapshot, base_snapshot) {
            // Session hasn't moved on; adopt the new snapshot. The clone's initial
            // ref is transferred to become the session's ref for its current snapshot.
            *guard = Some(Arc::clone(new_snapshot));
            old_snapshot.deref();
            drop(guard);
            if self.options.logging_enabled {
                self.logger.logf(format_args!(
                    "Adopted snapshot {} (parent {}) as current session snapshot (replacing {})",
                    new_snapshot.id, new_snapshot.parent_id, old_snapshot.id
                ));
                if !new_snapshot.builder_logs.is_nil() {
                    self.logger.log(&new_snapshot.builder_logs.string());
                }
            }
        } else {
            // Session has moved on to a newer snapshot; discard this one.
            // Release the clone's initial ref. If a handler is still using
            // the snapshot, its own ref keeps it alive.
            drop(guard);
            if self.options.logging_enabled {
                self.logger.logf(format_args!(
                    "Discarded snapshot {} (parent {}); session has moved on to snapshot {}",
                    new_snapshot.id, new_snapshot.parent_id, old_snapshot.id
                ));
                if !new_snapshot.builder_logs.is_nil() {
                    let logs = new_snapshot.builder_logs.string();
                    if !logs.is_empty() {
                        self.logger.logf(format_args!(
                            "--- Discarded snapshot {} builder logs (NOT adopted) ---",
                            new_snapshot.id
                        ));
                        self.logger.log(&logs);
                        self.logger.logf(format_args!(
                            "--- End discarded snapshot {} builder logs ---",
                            new_snapshot.id
                        ));
                    }
                }
            }
            new_snapshot.deref();
        }
    }

    // session.go:1344
    fn update_snapshot(&self, ctx: &Context, overlays: OverlayMap, change: SnapshotChange) {
        self.update_snapshot_with(ctx, overlays, change, false);
    }

    // session.go:1352
    // updateSnapshotRef is like UpdateSnapshot but returns the created snapshot
    // with an extra reference for the caller. The ref is taken atomically with
    // the snapshot assignment under snapshotMu, so the snapshot is guaranteed
    // to be alive when returned. The caller must call snapshot.Deref() when done.
    fn update_snapshot_ref(
        &self,
        ctx: &Context,
        overlays: OverlayMap,
        change: SnapshotChange,
    ) -> Arc<Snapshot> {
        self.update_snapshot_with(ctx, overlays, change, true).unwrap()
    }

    // session.go:1356 (Go `updateSnapshot`; returns nil when an API error rejects the snapshot and no caller ref
    // was requested)
    fn update_snapshot_with(
        &self,
        ctx: &Context,
        overlays: OverlayMap,
        change: SnapshotChange,
        caller_ref: bool,
    ) -> Option<Arc<Snapshot>> {
        let mut guard = self.snapshot.write().unwrap();
        let old_snapshot = Arc::clone(guard.as_ref().expect("session is closed"));
        let ctx = if has_locale(ctx) { ctx.clone() } else { self.with_current_locale(ctx) };
        let warm_change = WarmChange::of(&change);
        let new_snapshot = old_snapshot.clone_snapshot(
            &ctx,
            change,
            overlays,
            Some(&*self.logger),
            self.client.clone(),
        );
        // A failed API request may have mutated only a prefix of its clone. Such a
        // snapshot is returned to the caller for inspection and cleanup, but must
        // never become canonical session state or trigger adoption side effects.
        if new_snapshot.api_error.is_some() {
            drop(guard);
            if caller_ref {
                return Some(new_snapshot);
            }
            new_snapshot.deref();
            return None;
        }
        *guard = Some(Arc::clone(&new_snapshot));
        if caller_ref {
            new_snapshot.ref_();
        }
        if !Arc::ptr_eq(&new_snapshot, &old_snapshot) {
            // Release the session's reference to the old snapshot. The new snapshot's
            // clone ref (1) is transferred to become the session's ref for its current
            // snapshot. Other holders (e.g. active handlers) keep the old snapshot alive
            // via their own refs until they complete.
            old_snapshot.deref();
        }
        drop(guard);

        // Enqueue ATA updates if needed
        // (ATA is not ported: there is never a typings installer.)

        // Enqueue logging, watch updates, and diagnostic refresh tasks
        // !!! userPreferences/configuration updates
        let s = self.arc();
        let task_snapshot = Arc::clone(&new_snapshot);
        self.background_queue.enqueue(&self.background_context(), move |ctx| {
            let new_snapshot = task_snapshot;
            if s.options.logging_enabled {
                s.logger.logf(format_args!(
                    "Adopted snapshot {} (parent {}) as current session snapshot (replacing {})",
                    new_snapshot.id, new_snapshot.parent_id, old_snapshot.id
                ));
                if !new_snapshot.builder_logs.is_nil() {
                    s.logger.log(&new_snapshot.builder_logs.string());
                }
                s.log_project_changes(&old_snapshot, &new_snapshot);
                s.logger.log("");
            }
            if s.options.watch_enabled
                && let Err(err) = s.update_watches(&old_snapshot, &new_snapshot)
                && s.options.logging_enabled
            {
                s.logger.log(&err);
            }
            let _ = s.update_content_mapper_registrations(ctx, &new_snapshot);
            s.publish_program_diagnostics(&old_snapshot, &new_snapshot);
            s.send_project_info_telemetry_for_new_projects(&old_snapshot, &new_snapshot);
            s.warm_auto_import_cache(ctx, &warm_change, &old_snapshot, &new_snapshot);
        });

        Some(new_snapshot)
    }

    // session.go:1476
    // WaitForBackgroundTasks waits for all background tasks to complete.
    // This is intended to be used only for testing purposes.
    pub fn wait_for_background_tasks(&self) {
        self.cancel_idle_cache_clean();
        self.background_queue.wait();
    }

    // session.go:1481
    fn update_watch<T>(
        &self,
        ctx: &Context,
        old_watcher: Option<&Arc<WatchedFiles<T>>>,
        new_watcher: Option<&Arc<WatchedFiles<T>>>,
    ) -> Vec<lsproto::Error> {
        let mut errors = Vec::new();
        let Some(client) = &self.client else {
            return errors;
        };
        if let Some(new_watcher) = new_watcher {
            let w = new_watcher.watchers();
            let mut watchers = w.workspace_watchers.clone();
            watchers.extend(w.outside_workspace_watchers.iter().cloned());
            if !watchers.is_empty() {
                let mut new_watchers: OrderedMap<WatcherID, lsproto::FileSystemWatcher> =
                    OrderedMap::default();
                new_watchers.extend(watchers.iter().enumerate().filter_map(|(i, watcher)| {
                    let glob_id = WatcherID(format!("{}.{}", w.watcher_id, i));
                    if self.watches.acquire(watcher, glob_id.clone()) {
                        Some((glob_id, watcher.clone()))
                    } else {
                        None
                    }
                }));
                let mut watch_errors = Vec::new();
                for (id, watcher) in &new_watchers {
                    // Create a fresh timeout per client call so earlier calls
                    // don't consume the deadline for later ones.
                    let (call_ctx, call_cancel) = ctx.with_timeout(WATCH_REQUEST_TIMEOUT);
                    let result = client.watch_files(&call_ctx, id.clone(), vec![watcher.clone()]);
                    call_cancel.call();
                    if let Err(err) = result {
                        watch_errors.push(err);
                    } else {
                        if old_watcher.is_none() {
                            self.logger.log(&format!("Added new watch: {id}"));
                        } else {
                            self.logger.log(&format!("Updated watch: {id}"));
                        }
                        self.logger.log(&format!("\t{}", file_system_watcher_glob_string(watcher)));
                        self.logger.log("");
                    }
                }
                if watch_errors.is_empty() {
                    self.watches.clear_pending(&w.watcher_id);
                } else {
                    // Roll back ALL newly-acquired watchers on any failure to keep
                    // refcounts clean. On retry, Acquire will see them as new again.
                    // Re-registering an already-registered watcher with the client
                    // is harmless (registerCapability with the same ID replaces it).
                    for watcher in new_watchers.values() {
                        self.watches.release(watcher);
                    }
                    self.watches.mark_pending(w.watcher_id.clone());
                    errors.extend(watch_errors);
                }
                if !w.ignored_paths.is_empty() {
                    self.logger.logf(format_args!(
                        "{} paths ineligible for watching",
                        w.ignored_paths.len()
                    ));
                    if self.logger.is_verbose() {
                        #[expect(
                            clippy::iter_over_hash_type,
                            reason = "verbose log lines only; Go ranges the map too"
                        )]
                        for path in &w.ignored_paths {
                            self.logger.log(&format!("\t{path}"));
                        }
                    }
                }
            }
        }
        if let Some(old_watcher) = old_watcher {
            let w = old_watcher.watchers();
            let mut watchers = w.workspace_watchers.clone();
            watchers.extend(w.outside_workspace_watchers.iter().cloned());
            if !watchers.is_empty() {
                let mut removed_ids = Vec::new();
                removed_ids.extend(watchers.iter().filter_map(|watcher| {
                    let (id, removed) = self.watches.release(watcher);
                    if removed { Some(id) } else { None }
                }));
                for id in removed_ids {
                    let (call_ctx, call_cancel) = ctx.with_timeout(WATCH_REQUEST_TIMEOUT);
                    let result = client.unwatch_files(&call_ctx, id.clone());
                    call_cancel.call();
                    match result {
                        Err(err) => errors.push(err),
                        Ok(()) => {
                            if new_watcher.is_none() {
                                self.logger.log(&format!("Removed watch: {id}"));
                            }
                        }
                    }
                }
            }
        }
        errors
    }

    // session.go:1565
    // updateContentMapperRegistrations computes the union of content mapper extensions across all loaded
    // configs in the new snapshot and, when the set changes, asks the client to synchronize text documents
    // with those extensions. This is how an otherwise unsupported file (e.g. a `.vue`) begins flowing to the server once a
    // config that maps it is discovered.
    fn update_content_mapper_registrations(
        &self,
        ctx: &Context,
        snapshot: &Snapshot,
    ) -> Result<(), lsproto::Error> {
        let Some(client) = &self.client else {
            return Ok(());
        };
        let content_mappers = snapshot.config_file_registry.content_mappers();
        let mut extensions = content_mappers.extensions.clone();
        extensions.extend(snapshot.inferred_project_content_mapper_extensions.iter().cloned());
        extensions.sort();
        extensions.dedup();

        let mut registration = self.content_mapper_registration.lock().unwrap();
        // Background tasks may finish out of order; never let an older snapshot's task overwrite the
        // registration derived from a newer one.
        if snapshot.id() <= registration.1 {
            return Ok(());
        }
        if extensions == registration.0.clone().unwrap_or_default() {
            registration.1 = snapshot.id();
            return Ok(());
        }
        // RegisterContentMapperExtensions replaces the prior registration wholesale (unregistering extensions
        // that are no longer mapped and registering the current set), so an empty set removes the registration
        // once the last mapping config unloads. On failure we leave the state unadvanced so the next snapshot
        // update retries.
        if let Err(err) = client.register_content_mapper_extensions(ctx, extensions.clone()) {
            if self.options.logging_enabled {
                self.logger.log(&err.message);
            }
            return Err(err);
        }
        registration.0 = Some(extensions);
        registration.1 = snapshot.id();
        Ok(())
    }

    // session.go:1600
    fn update_watches(
        &self,
        old_snapshot: &Snapshot,
        new_snapshot: &Snapshot,
    ) -> Result<(), String> {
        let mut errors: Vec<lsproto::Error> = Vec::new();
        let start = Instant::now();
        let ctx = self.background_context();
        let old_configs = &old_snapshot.config_file_registry.configs;
        let new_configs = &new_snapshot.config_file_registry.configs;
        let mut added: Vec<Shared<ConfigFileEntry>> = Vec::new();
        let mut removed: Vec<Shared<ConfigFileEntry>> = Vec::new();
        let mut changed: Vec<(Shared<ConfigFileEntry>, Shared<ConfigFileEntry>)> = Vec::new();
        diff_maps_func(
            old_configs,
            new_configs,
            |a: &Shared<ConfigFileEntry>, b: &Shared<ConfigFileEntry>| {
                a.root_files_watch.id() == b.root_files_watch.id()
            },
            Some(|_: &Path, added_entry: &Shared<ConfigFileEntry>| added.push(added_entry.clone())),
            Some(|_: &Path, removed_entry: &Shared<ConfigFileEntry>| {
                removed.push(removed_entry.clone());
            }),
            Some(
                |_: &Path,
                 old_entry: &Shared<ConfigFileEntry>,
                 new_entry: &Shared<ConfigFileEntry>| {
                    changed.push((old_entry.clone(), new_entry.clone()));
                },
            ),
        );
        errors.extend(added.iter().flat_map(|added_entry| {
            self.update_watch(&ctx, None, added_entry.root_files_watch.as_ref())
        }));
        errors.extend(removed.iter().flat_map(|removed_entry| {
            self.update_watch(&ctx, removed_entry.root_files_watch.as_ref(), None)
        }));
        errors.extend(changed.iter().flat_map(|(old_entry, new_entry)| {
            self.update_watch(
                &ctx,
                old_entry.root_files_watch.as_ref(),
                new_entry.root_files_watch.as_ref(),
            )
        }));
        // Retry config watchers whose IDs didn't change but whose previous registration failed.
        // order-independent: retries each pending config watcher by its own ID; Go ranges the map too, so the registrations go out in map order there as well
        errors.extend(
            new_configs
                .iter()
                .filter_map(|(path, new_entry)| {
                    if let Some(old_entry) = old_configs.get(path) {
                        if old_entry.root_files_watch.id() == new_entry.root_files_watch.id()
                            && self.watches.is_pending(&new_entry.root_files_watch.id())
                        {
                            Some(self.update_watch(&ctx, None, new_entry.root_files_watch.as_ref()))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                })
                .flatten(),
        );

        let old_projects = old_snapshot.project_collection.projects_by_id();
        let new_projects = new_snapshot.project_collection.projects_by_id();
        let mut added_projects: Vec<Shared<Project>> = Vec::new();
        let mut removed_projects: Vec<Shared<Project>> = Vec::new();
        let mut modified_projects: Vec<(Shared<Project>, Shared<Project>)> = Vec::new();
        // (Go updates watches from inside the diff callbacks; they are collected first and applied in order.)
        diff_ordered_maps(
            &old_projects,
            &new_projects,
            |_, added_project| added_projects.push(added_project.clone()),
            |_, removed_project| removed_projects.push(removed_project.clone()),
            |_, old_project, new_project| {
                modified_projects.push((old_project.clone(), new_project.clone()));
            },
        );
        for added_project in &added_projects {
            errors.extend(self.update_watch(
                &ctx,
                None,
                added_project.program_files_watch.as_ref(),
            ));
            errors.extend(self.update_watch(&ctx, None, added_project.typings_watch.as_ref()));
            errors.extend(self.update_watch(
                &ctx,
                None,
                added_project.content_mapper_watch.as_ref(),
            ));
        }
        for removed_project in &removed_projects {
            errors.extend(self.update_watch(
                &ctx,
                removed_project.program_files_watch.as_ref(),
                None,
            ));
            errors.extend(self.update_watch(&ctx, removed_project.typings_watch.as_ref(), None));
            errors.extend(self.update_watch(
                &ctx,
                removed_project.content_mapper_watch.as_ref(),
                None,
            ));
        }
        for (old_project, new_project) in &modified_projects {
            if old_project.program_files_watch.id() != new_project.program_files_watch.id() {
                errors.extend(self.update_watch(
                    &ctx,
                    old_project.program_files_watch.as_ref(),
                    new_project.program_files_watch.as_ref(),
                ));
            } else if self.watches.is_pending(&new_project.program_files_watch.id()) {
                errors.extend(self.update_watch(
                    &ctx,
                    None,
                    new_project.program_files_watch.as_ref(),
                ));
            }
            if old_project.typings_watch.id() != new_project.typings_watch.id() {
                errors.extend(self.update_watch(
                    &ctx,
                    old_project.typings_watch.as_ref(),
                    new_project.typings_watch.as_ref(),
                ));
            } else if self.watches.is_pending(&new_project.typings_watch.id()) {
                errors.extend(self.update_watch(&ctx, None, new_project.typings_watch.as_ref()));
            }
            if old_project.content_mapper_watch.id() != new_project.content_mapper_watch.id() {
                errors.extend(self.update_watch(
                    &ctx,
                    old_project.content_mapper_watch.as_ref(),
                    new_project.content_mapper_watch.as_ref(),
                ));
            } else if self.watches.is_pending(&new_project.content_mapper_watch.id()) {
                errors.extend(self.update_watch(
                    &ctx,
                    None,
                    new_project.content_mapper_watch.as_ref(),
                ));
            }
        }

        if old_snapshot.auto_imports_watch.id() != new_snapshot.auto_imports_watch.id() {
            errors.extend(self.update_watch(
                &ctx,
                old_snapshot.auto_imports_watch.as_ref(),
                new_snapshot.auto_imports_watch.as_ref(),
            ));
        } else if self.watches.is_pending(&new_snapshot.auto_imports_watch.id()) {
            errors.extend(self.update_watch(&ctx, None, new_snapshot.auto_imports_watch.as_ref()));
        }

        if !errors.is_empty() {
            return Err(format!(
                "errors updating watches: [{}]",
                errors.iter().map(|e| e.message.as_str()).collect::<Vec<_>>().join(" ")
            ));
        } else if self.options.logging_enabled {
            self.logger.log(&format!("Updated watches in {:?}", start.elapsed()));
        }
        Ok(())
    }

    // session.go:1683
    pub fn close(&self) {
        // Cancel any pending scheduled snapshot update
        self.cancel_scheduled_snapshot_update();
        // Cancel any pending diagnostics refresh
        self.cancel_diagnostics_refresh();
        // Cancel any pending auto-import cache warming
        self.cancel_warm_auto_import_cache();
        // Stop and drain any pending idle cache clean
        self.close_idle_cache_clean();
        // Cancel periodic performance telemetry
        self.stop_performance_telemetry();
        self.background_cancel.call();
        self.background_queue.close();

        let _update = self.snapshot_update_mu.lock().unwrap();
        let snapshot = self.snapshot.write().unwrap().take();
        if let Some(snapshot) = snapshot {
            snapshot.deref();
        }
        self.snapshot_host.close();
    }

    // session.go:1698
    fn flush_changes(
        &self,
        ctx: &Context,
    ) -> (FileChangeSummary, OverlayMap, FxHashMap<ID, Arc<ATAStateChange>>, Option<UserPreferences>)
    {
        let mut pending = self.pending_file_changes.lock().unwrap();
        let mut pending_ata = self.pending_ata_changes.lock().unwrap();
        let pending_ata_changes = std::mem::take(&mut *pending_ata);
        let (file_changes, overlays) = self.flush_changes_locked(ctx, &mut pending);
        let mut user_config = self.user_config.lock().unwrap();
        let mut new_prefs = None;
        if user_config.pending_user_config_changes {
            new_prefs = Some(user_config.workspace_user_preferences.clone());
        }
        user_config.pending_user_config_changes = false;
        (file_changes, overlays, pending_ata_changes, new_prefs)
    }

    // session.go:1718
    // flushChangesLocked should only be called with s.pendingFileChangesMu held.
    fn flush_changes_locked(
        &self,
        _ctx: &Context,
        pending: &mut MutexGuard<'_, Vec<FileChange>>,
    ) -> (FileChangeSummary, OverlayMap) {
        if pending.is_empty() {
            return (FileChangeSummary::default(), tsrs_overlays(&self.fs));
        }

        let start = Instant::now();
        let (changes, overlays) = self.fs.process_changes(&pending[..]);
        if self.options.logging_enabled {
            self.logger.log(&format!(
                "Processed {} file changes in {:?}",
                pending.len(),
                start.elapsed()
            ));
        }
        pending.clear();
        (changes, overlays)
    }

    // session.go:1733
    // logProjectChanges logs information about projects that have changed between snapshots
    fn log_project_changes(&self, old_snapshot: &Snapshot, new_snapshot: &Snapshot) {
        let mut logged_project_changes = false;
        let verbose = self.logger.is_verbose();
        let mut log_project = |project: &Project| {
            let mut builder = String::new();
            project.print(
                verbose, /*writeFileNames*/
                verbose, /*writeFileExplanation*/
                &mut builder,
            );
            self.logger.log(&builder);
            logged_project_changes = true;
        };
        let old_projects = old_snapshot.project_collection.projects_by_id();
        let new_projects = new_snapshot.project_collection.projects_by_id();
        let events: std::cell::RefCell<Vec<(u8, Shared<Project>)>> =
            std::cell::RefCell::new(Vec::new());
        diff_ordered_maps(
            &old_projects,
            &new_projects,
            |_, added_project| events.borrow_mut().push((0, added_project.clone())),
            |_, removed_project| events.borrow_mut().push((1, removed_project.clone())),
            |_, _old_project, new_project| events.borrow_mut().push((2, new_project.clone())),
        );
        for (kind, project) in events.into_inner() {
            match kind {
                // New project added
                0 => log_project(&project),
                // Project removed
                1 => self.logger.logf(format_args!("\nProject '{}' removed\n{}", project.id(), HR)),
                // Project updated
                _ => {
                    if project.program_update_kind == ProgramUpdateKind::NewFiles {
                        log_project(&project);
                    }
                }
            }
        }

        if logged_project_changes || self.logger.is_verbose() {
            self.log_cache_stats(new_snapshot);
        }
    }

    // session.go:1765
    fn log_cache_stats(&self, snapshot: &Snapshot) {
        let mut parse_cache_size = 0;
        let mut extended_config_count = 0;
        if self.logger.is_verbose() {
            parse_cache_size = self.snapshot_host.parse_cache.len();
            extended_config_count = self.snapshot_host.extended_config_cache.len();
        }
        self.logger.log("\n======== Cache Statistics ========");
        self.logger.logf(format_args!("Open file count:   {:6}", snapshot.overlays().len()));
        self.logger.logf(format_args!("Cached disk files: {:6}", snapshot.fs.cache_files.len()));
        self.logger.logf(format_args!(
            "Realpath aliases:  {:6}",
            snapshot.fs.node_modules_realpath_aliases.len()
        ));
        self.logger.logf(format_args!(
            "Project count:     {:6}",
            snapshot.project_collection.projects().len()
        ));
        self.logger.logf(format_args!(
            "Config count:      {:6}",
            snapshot.config_file_registry.configs.len()
        ));
        if self.logger.is_verbose() {
            self.logger.logf(format_args!("Parse cache size:           {parse_cache_size:6}"));
            self.logger.logf(format_args!(
                "Program count:              {:6}",
                self.snapshot_host.program_counter.len()
            ));
            self.logger.logf(format_args!("Extended config cache size: {extended_config_count:6}"));

            self.logger.log("Auto Imports:");
            // Go calls GetCacheStats on the (never nil after an update) registry pointer.
            let auto_import_stats = snapshot
                .auto_import_registry()
                .expect("nil auto-import registry")
                .get_cache_stats();
            self.logger.logf(format_args!(
                "\tUnique packages (by realpath): {}",
                auto_import_stats.unique_package_count
            ));
            if !auto_import_stats.project_buckets.is_empty() {
                self.logger.log("\tProject buckets:");
                for bucket in &auto_import_stats.project_buckets {
                    self.logger.logf(format_args!(
                        "\t\t{}{}:",
                        bucket.name,
                        if bucket.state.dirty() { " (dirty)" } else { "" }
                    ));
                    self.logger.logf(format_args!("\t\t\tFiles: {}", bucket.file_count));
                    self.logger.logf(format_args!("\t\t\tExports: {}", bucket.export_count));
                }
            }
            if !auto_import_stats.node_modules_buckets.is_empty() {
                self.logger.log("\tnode_modules buckets:");
                for bucket in &auto_import_stats.node_modules_buckets {
                    self.logger.logf(format_args!(
                        "\t\t{}{}:",
                        bucket.name,
                        if bucket.state.dirty() { " (dirty)" } else { "" }
                    ));
                    if let Some(dirty_packages) = bucket.state.dirty_packages() {
                        #[expect(
                            clippy::iter_over_hash_type,
                            reason = "log lines only; Go ranges the set too"
                        )]
                        for package_name in dirty_packages.keys() {
                            self.logger
                                .logf(format_args!("\t\t\tNeeds granular update: {package_name}"));
                        }
                    }
                    match &bucket.dependency_names {
                        Some(dependency_names) => self.logger.logf(format_args!(
                            "\t\t\tCollected packages: {}",
                            dependency_names.len()
                        )),
                        None => self.logger.logf(format_args!(
                            "\t\t\tCollected packages: all, due to no package.json!"
                        )),
                    }
                    self.logger.logf(format_args!(
                        "\t\t\tTotal packages: {}",
                        bucket.package_names.as_ref().map_or(0, tsrs_core::collections::Set::len)
                    ));
                    self.logger.logf(format_args!("\t\t\tFiles: {}", bucket.file_count));
                    self.logger.logf(format_args!("\t\t\tExports: {}", bucket.export_count));
                    match bucket.state.recursive_search_packages() {
                        None => self.logger.log("\t\t\tRecursive search: all"),
                        Some(packages) if !packages.is_empty() => {
                            self.logger.logf(format_args!(
                                "\t\t\tRecursive search: {} packages",
                                packages.len()
                            ));
                        }
                        Some(_) => self.logger.log("\t\t\tRecursive search: none"),
                    }
                }
            }
        }
    }

    // Preserve the SessionInit npm executor hook for embedding hosts.
    pub fn npm_install(&self, cwd: &str, npm_install_args: &[String]) -> Result<Vec<u8>, String> {
        self.npm_executor.as_ref().expect("no npm executor").npm_install(cwd, npm_install_args)
    }

    // session.go:1831
    fn refresh_inlay_hints_if_needed(
        &self,
        old_prefs: &UserPreferences,
        new_prefs: &UserPreferences,
    ) {
        if old_prefs.inlay_hints != new_prefs.inlay_hints
            && let Some(client) = &self.client
            && let Err(err) = client.refresh_inlay_hints(&self.background_context())
            && self.options.logging_enabled
        {
            self.logger.logf(format_args!("Error refreshing inlay hints: {err}"));
        }
    }

    // session.go:1839
    fn refresh_code_lens_if_needed(
        &self,
        old_prefs: &UserPreferences,
        new_prefs: &UserPreferences,
    ) {
        if old_prefs.code_lens != new_prefs.code_lens
            && let Some(client) = &self.client
            && let Err(err) = client.refresh_code_lens(&self.background_context())
            && self.options.logging_enabled
        {
            self.logger.logf(format_args!("Error refreshing code lens: {err}"));
        }
    }

    // session.go:1847
    fn refresh_diagnostics_if_needed(
        &self,
        old_prefs: &UserPreferences,
        new_prefs: &UserPreferences,
    ) {
        if old_prefs.custom_config_file_name != new_prefs.custom_config_file_name
            || old_prefs.report_style_checks_as_warnings
                != new_prefs.report_style_checks_as_warnings
            || old_prefs.enable_validation != new_prefs.enable_validation
        {
            self.schedule_diagnostics_refresh();
        }
    }

    // session.go:1855
    fn refresh_ata_if_needed(&self, old_prefs: &UserPreferences, new_prefs: &UserPreferences) {
        if old_prefs.is_ata_disabled() && !new_prefs.is_ata_disabled() {
            // ATA was re-enabled; schedule a diagnostics refresh so the next snapshot update
            // re-triggers ATA for existing projects with the new setting.
            self.schedule_diagnostics_refresh();
        }
    }

    // session.go:1863
    fn publish_program_diagnostics(&self, old_snapshot: &Snapshot, new_snapshot: &Snapshot) {
        if !self.options.push_diagnostics_enabled {
            return;
        }
        if new_snapshot.user_preferences().enable_validation.is_false() {
            if old_snapshot.user_preferences().enable_validation.is_false() {
                return;
            }
            for old_project in old_snapshot.project_collection.projects_by_id().values() {
                if let Some(configured_id) = old_project.id().configured()
                    && old_snapshot
                        .project_collection
                        .get_open_configured_projects()
                        .has(&configured_id)
                {
                    let config_file_path = old_project.config_file_path().clone();
                    self.publish_project_diagnostics(
                        &self.background_context(),
                        &config_file_path,
                        &[],
                        &old_snapshot.converters(),
                    );
                }
            }
            return;
        }

        let ctx = self.background_context();
        let old_projects = old_snapshot.project_collection.projects_by_id();
        let new_projects = new_snapshot.project_collection.projects_by_id();
        let old_open_projects = old_snapshot.project_collection.get_open_configured_projects();
        let new_open_projects = new_snapshot.project_collection.get_open_configured_projects();
        let events: std::cell::RefCell<Vec<(u8, Shared<Project>)>> =
            std::cell::RefCell::new(Vec::new());
        diff_ordered_maps(
            &old_projects,
            &new_projects,
            |_, added_project| events.borrow_mut().push((0, added_project.clone())),
            |_, removed_project| events.borrow_mut().push((1, removed_project.clone())),
            |_, _old_project, new_project| events.borrow_mut().push((2, new_project.clone())),
        );
        for (kind, project) in events.into_inner() {
            match kind {
                0 | 2 => {
                    let configured_id = project.id().configured();
                    if !should_publish_program_diagnostics(&project, new_snapshot.id())
                        || configured_id.is_none()
                        || !new_open_projects.has(configured_id.as_ref().unwrap())
                    {
                        continue;
                    }
                    let config_file_path = project.config_file_path().clone();
                    self.publish_project_diagnostics(
                        &ctx,
                        &config_file_path,
                        &project.get_project_diagnostics(&ctx),
                        &new_snapshot.converters(),
                    );
                }
                _ => {
                    if project.kind != Kind::Configured {
                        continue;
                    }
                    let config_file_path = project.config_file_path().clone();
                    self.publish_project_diagnostics(
                        &ctx,
                        &config_file_path,
                        &[],
                        &old_snapshot.converters(),
                    );
                }
            }
        }
        // Sync diagnostics for projects whose open-file state changed without a program update.
        for (project_id, new_project) in &new_projects {
            if new_project.kind != Kind::Configured {
                continue;
            }
            if !old_projects.contains_key(project_id) {
                continue; // Handled by added project case above
            }
            let configured_id = new_project.id().configured().unwrap();
            let config_file_path = new_project.config_file_path().clone();
            let old_project = old_projects.get(project_id).unwrap();
            let new_has_open_files = new_open_projects.has(&configured_id);
            let old_has_open_files = old_open_projects.has(&configured_id);
            if new_has_open_files
                && !old_has_open_files
                && (new_project == old_project
                    || !should_publish_program_diagnostics(new_project, new_snapshot.id()))
            {
                // Project reopened without a program update
                self.publish_project_diagnostics(
                    &ctx,
                    &config_file_path,
                    &new_project.get_project_diagnostics(&ctx),
                    &new_snapshot.converters(),
                );
            } else if !new_has_open_files && old_has_open_files {
                // Project closed
                self.publish_project_diagnostics(
                    &ctx,
                    &config_file_path,
                    &[],
                    &new_snapshot.converters(),
                );
            }
        }
    }

    // session.go:1944
    fn publish_project_diagnostics(
        &self,
        ctx: &Context,
        config_file_path: &str,
        diagnostics: &[P<Diagnostic>],
        converters: &Converters,
    ) {
        let diagnostics =
            if self.config().enable_validation.is_false() { &[] } else { diagnostics };
        let ctx = self.with_current_locale(ctx);
        let mut lsp_diagnostics = Vec::with_capacity(diagnostics.len());
        lsp_diagnostics.extend(
            diagnostics.iter().map(|&diag| lsconv::diagnostic_to_lsp_push(&ctx, converters, diag)),
        );

        if let Some(client) = &self.client
            && let Err(err) = client.publish_diagnostics(
                &ctx,
                lsproto::PublishDiagnosticsParams {
                    uri: lsconv::file_name_to_document_uri(config_file_path),
                    version: None,
                    diagnostics: lsp_diagnostics,
                },
            )
            && self.options.logging_enabled
        {
            self.logger.logf(format_args!("Error publishing diagnostics: {err}"));
        }
    }

    // session.go:1965
    // EnqueuePublishGlobalDiagnostics schedules a background check for new accumulated
    // global diagnostics from checker pools, re-publishing tsconfig diagnostics if changed.
    // Multiple calls are coalesced into a single background task.
    pub fn enqueue_publish_global_diagnostics(&self) {
        if !self.options.push_diagnostics_enabled || self.config().enable_validation.is_false() {
            return;
        }
        if self
            .global_diag_publish_pending
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            let s = self.arc();
            self.background_queue.enqueue(&self.background_context(), move |ctx| {
                s.publish_global_diagnostics(ctx);
            });
        }
    }

    // session.go:1974
    fn publish_global_diagnostics(&self, ctx: &Context) {
        struct ResetPending<'a>(&'a AtomicBool);
        impl Drop for ResetPending<'_> {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _reset = ResetPending(&self.global_diag_publish_pending);

        let snapshot = {
            let guard = self.snapshot.read().unwrap();
            let snapshot = guard.as_ref().expect("session is closed");
            snapshot.ref_();
            Arc::clone(snapshot)
        };
        let _deref = DerefOnDrop(&snapshot);

        for project in snapshot.project_collection.projects() {
            if project.kind != Kind::Configured {
                continue;
            }
            let Some(checker_pool) = &project.checker_pool else {
                continue;
            };
            if checker_pool.take_new_global_diagnostics() {
                self.publish_project_diagnostics(
                    ctx,
                    &project.config_file_path,
                    &project.get_project_diagnostics(ctx),
                    &snapshot.converters(),
                );
            }
        }
    }

    // session.go:2046
    fn warm_auto_import_cache(
        &self,
        ctx: &Context,
        change: &WarmChange,
        _old_snapshot: &Snapshot,
        new_snapshot: &Arc<Snapshot>,
    ) {
        if change.changed.len() == 1 {
            let changed_file = change.changed[0].clone();
            if !new_snapshot.is_open_file(&changed_file.file_name()) {
                return;
            }
            let prefs = new_snapshot.user_preferences();
            if prefs.include_completions_for_module_exports.is_false() {
                return;
            }
            let Some(project) = new_snapshot.get_default_project(&changed_file) else {
                return;
            };
            if new_snapshot.auto_imports.is_prepared_for_importing_file(
                &changed_file.file_name(),
                &ProjectID(project.id().0),
                prefs,
            ) {
                return;
            }

            // Cancel any previous auto-import warming and create a new cancellable context.
            // Only publish the new cancel func if the derived context is still active,
            // and make the stored cancel func a no-op once that warming task is done.
            let (warm_ctx, cancel) = {
                let mut warm = self.warm_auto_import_cancel.lock().unwrap();
                if let Some(previous) = warm.take() {
                    previous();
                }
                let (warm_ctx, cancel) = ctx.with_cancel();
                if warm_ctx.err().is_none() {
                    let (c, l, f, cancel) = (
                        warm_ctx.clone(),
                        Arc::clone(&self.logger),
                        changed_file.file_name(),
                        cancel.clone(),
                    );
                    *warm = Some(Box::new(move || {
                        if c.err().is_some() {
                            return;
                        }
                        l.logf(format_args!("Cancelling auto-import warming for file {f}"));
                        cancel.call();
                    }));
                }
                (warm_ctx, cancel)
            };

            if warm_ctx.err().is_some() {
                cancel.call();
                return;
            }
            let _cancel = CancelOnDrop(cancel);

            // Clone the snapshot with auto-imports using warmCtx so the expensive
            // extraction work is cancelled if a file change arrives.
            if !new_snapshot.try_ref() {
                return;
            }
            let _deref = DerefOnDrop(new_snapshot);

            let warm_change = SnapshotChange {
                reason: UpdateReason::RequestedLanguageServiceWithAutoImports,
                resource_request: ResourceRequest {
                    documents: vec![changed_file.clone()],
                    auto_imports: changed_file,
                    ..Default::default()
                },
                ..Default::default()
            };
            let cloned_snapshot = new_snapshot.clone_snapshot(
                &warm_ctx,
                warm_change,
                new_snapshot.overlays(),
                Some(&*self.logger),
                self.client.clone(),
            );

            // If cancelled during clone, discard the incomplete result.
            if warm_ctx.err().is_some() {
                cloned_snapshot.deref();
                return;
            }

            // Conditionally adopt: if the session hasn't moved past newSnapshot,
            // promote the clone so future requests benefit from the warmed cache.
            self.adopt_snapshot_change(new_snapshot, &cloned_snapshot);
        }
    }
}

// The part of a `SnapshotChange` the background tasks of `updateSnapshot` read after the change has been
// consumed by `Clone` (Go keeps using the `change` value).
struct WarmChange {
    changed: Vec<lsproto::DocumentUri>,
}

impl WarmChange {
    fn of(change: &SnapshotChange) -> WarmChange {
        WarmChange { changed: change.file_changes.changed.keys().iter().cloned().collect() }
    }
}

// session.go:1937
fn should_publish_program_diagnostics(p: &Project, snapshot_id: u64) -> bool {
    if p.kind != Kind::Configured || p.program.is_none() || p.program_last_update != snapshot_id {
        return false;
    }
    p.program_update_kind > ProgramUpdateKind::Cloned
}

const IDLE_CACHE_CLEAN_DELAY: Duration = Duration::from_secs(30);

// The deferred work a `WithLanguageServiceAndSnapshot` callback hands back (Go `func() error`).
pub type AsyncWork = Box<dyn FnOnce() -> Result<(), lsproto::Error> + Send>;

// Go `defer cancel()`.
struct CancelOnDrop(CancelFunc);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.call();
    }
}

// Go `defer snapshot.Deref()`.
struct DerefOnDrop<'a>(&'a Arc<Snapshot>);

impl Drop for DerefOnDrop<'_> {
    fn drop(&mut self) {
        self.0.deref();
    }
}

fn tsrs_overlays(fs: &OverlayFS) -> OverlayMap {
    crate::overlayfs::LayeredFileSystem::overlays(fs)
}
