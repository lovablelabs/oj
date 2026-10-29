// Port of tsc/internal/api/session.go (Session, snapshotData, HandleRequest dispatch).
//
// Ownership/lifetime model (docs/NODE_API.md, "lifetimes"):
// - Every client-visible snapshot handle maps to an `Arc<SnapshotData>`. The registry entry holds one
//   reference; every in-flight request that resolves the handle holds another. The underlying
//   `tsrs_project::Snapshot` reference is dropped (`Snapshot::deref`) only when the last `Arc` goes
//   away, so a `release` racing with a query never frees regions the query is still reading.
// - Programs, source files, symbols and types are region-owned (`P<T>` / `&'static`): they may only be
//   reached through a live `SnapshotData` and are never exposed as addresses. Wire handles are the
//   pinned Go IDs (snapshot ids, project ids, symbol/type/signature ids, node handles).

use rustc_hash::FxHashMap;
use std::sync::Weak;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use tsrs_api_transport::strictjson::DecodeError;
use tsrs_compiler::Program;
use tsrs_core::context::Context;
use tsrs_core::json::{self, JsonMap, Value};
use tsrs_project::{ID as ProjectID, Snapshot, SnapshotHost};
use tsrs_vfs::FS;

use crate::checker::{self, CheckerSnapshotState};
use crate::handler::{ApiError, ApiResult, ClientConn, Handler, Response};
use crate::methods::{Owner, method_info};
use crate::program::DiagnosticKind;
use crate::wire::{Obj, Params};

pub type SnapshotID = u64;

static SESSION_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Go `StdioServerOptions` / `SessionOptions` subset that affects session behavior.
pub struct SessionOptions {
    pub cwd: String,
    pub default_library_path: String,
    /// Base filesystem (normally `bundled::wrap_fs(osvfs::fs())`, optionally wrapped by the callback FS).
    pub fs: Arc<dyn FS>,
    /// Go `UseBinaryResponses`: true for the sync `MessagePack` protocol, false for JSON-RPC.
    pub binary_responses: bool,
    /// Go `RunExternalCode` (false by default). Forwarded to the project session, but API projects get no content
    /// mapper project yet (see `Session::new`), so content-mapped files are not transformed.
    pub run_external_code: bool,
}

impl SessionOptions {
    pub fn new(
        cwd: String,
        default_library_path: String,
        fs: Arc<dyn FS>,
        binary_responses: bool,
    ) -> SessionOptions {
        SessionOptions { cwd, default_library_path, fs, binary_responses, run_external_code: false }
    }
}

/// Go `snapshotOpenState`: projects/files this client opened in a snapshot lineage.
#[derive(Clone, Default)]
pub struct OpenState {
    pub open_projects: tsrs_core::collections::Set<tsrs_core::tspath::Path>,
    pub open_files: tsrs_core::collections::Set<tsrs_core::tspath::Path>,
}

/// Go `snapshotData`: one registered snapshot plus its per-snapshot registries.
pub struct SnapshotData {
    pub snapshot: Arc<Snapshot>,
    /// Request filesystem the snapshot was created with (inherited by updates), if any.
    pub(crate) file_system: Option<Arc<crate::requestfs::RequestFileSystem>>,
    pub(crate) open_state: OpenState,
    /// Registries owned by the checker lane (symbols, types, signatures).
    pub checker_state: CheckerSnapshotState,
}

impl Drop for SnapshotData {
    fn drop(&mut self) {
        // Last reference (registry + in-flight requests) is gone: release the project snapshot.
        self.snapshot.deref();
    }
}

impl SnapshotData {
    /// Go `snapshotData.getProject` / `getProgram`.
    pub fn get_program(&self, project: &ProjectID) -> ApiResult<&'static Program> {
        let proj = self
            .snapshot
            .project_collection
            .get_project(project)
            .ok_or_else(|| ApiError::client(format!("project {} not found", project.0)))?;
        proj.get_program().ok_or_else(|| ApiError::client("project has no program"))
    }
}

/// Go `Session.openProjects` / `openFiles` / `createdPrograms`, guarded by `languageServerUpdateMu`: the canonical
/// LSP-state resources owned by this API client.
#[derive(Default)]
pub struct LanguageServerState {
    pub open_state: OpenState,
    pub(crate) created_programs: tsrs_core::collections::Set<tsrs_project::SyntheticProjectID>,
}

pub struct Session {
    id: String,
    pub(crate) snapshot_host: Arc<SnapshotHost>,
    /// Go `projectSession`: the language server's project session, for sessions created by `new_lsp_session`.
    pub(crate) project_session: Option<Arc<tsrs_project::Session>>,
    pub language_server: Mutex<LanguageServerState>,
    pub(crate) parse_config_host: crate::config::ApiParseConfigHost,
    binary_responses: bool,
    /// Registered snapshots with their API reference count (Go `snapshotData.refCount`).
    snapshots: RwLock<FxHashMap<SnapshotID, (Arc<SnapshotData>, usize)>>,
    conn: Mutex<Option<Arc<dyn ClientConn>>>,
    closed: Mutex<bool>,
    pub(crate) batch_pages: Mutex<FxHashMap<String, Vec<String>>>,
    pub(crate) module_resolvers: crate::module_resolution::ModuleResolvers,
    /// The session's base filesystem (Go `Session.FS()`), shared with resolver hosts.
    base_fs: Arc<dyn FS>,
    weak_self: std::sync::Weak<Session>,
    pub(crate) build_state: crate::build::BuildState,
    pub(crate) source_files: crate::sourcefiles::SourceFileState,
    build_backend: Mutex<Option<Arc<dyn crate::build::BuildBackend>>>,
    next_batch_page: AtomicU64,
}

impl Session {
    /// Go `NewStandaloneSession`.
    pub fn new(options: SessionOptions) -> Arc<Session> {
        let init = tsrs_project::SessionInit {
            background_ctx: Context::background(),
            options: Arc::new(tsrs_project::SessionOptions {
                current_directory: options.cwd,
                default_library_path: options.default_library_path,
                typings_location: String::new(),
                position_encoding: tsrs_lsproto::PositionEncodingKind::UTF8,
                watch_enabled: false,
                logging_enabled: false,
                telemetry_enabled: false,
                push_diagnostics_enabled: false,
                run_external_code: options.run_external_code,
                debounce_delay: std::time::Duration::ZERO,
                checker_pool_options: tsrs_project::CheckerPoolOptions::default(),
            }),
            fs: options.fs,
            client: None,
            logger: None,
            npm_executor: None,
            parse_cache: None,
            // Go also passes the server's ContentMapperSpawner here (api/server.go:99). The project system does not
            // transform content-mapped files yet (phase 2, notes/contentmappers.md): `--runExternalCode` is accepted
            // and recorded in the session options, and its projects get no content mapper project.
        };
        let base_fs = Arc::clone(&init.fs);
        Self::new_session(
            tsrs_project::new_snapshot_host(&init),
            None,
            base_fs,
            options.binary_responses,
        )
    }

    /// Go `NewLSPSession`: an API session over the language server's project session. Its snapshots are the
    /// language server's (`getCurrentLanguageServerSnapshot`), and the projects, files and programs it opens
    /// there are released when it closes.
    pub fn new_lsp_session(
        project_session: Arc<tsrs_project::Session>,
        binary_responses: bool,
    ) -> Arc<Session> {
        let snapshot_host = Arc::clone(project_session.snapshot_host());
        // Go `Session.FS()` is the project session's (overlay) file system.
        let base_fs = project_session.shared_fs();
        Self::new_session(snapshot_host, Some(project_session), base_fs, binary_responses)
    }

    /// Go `newSession`.
    fn new_session(
        snapshot_host: Arc<SnapshotHost>,
        project_session: Option<Arc<tsrs_project::Session>>,
        base_fs: Arc<dyn FS>,
        binary_responses: bool,
    ) -> Arc<Session> {
        // Go parses configs with the snapshot host as the `ParseConfigHost`.
        let parse_config_host = crate::config::ApiParseConfigHost {
            fs: snapshot_host.shared_fs(),
            cwd: snapshot_host.get_current_directory().to_string(),
        };
        let id = SESSION_ID_COUNTER.fetch_add(1, Ordering::SeqCst) + 1;
        Arc::new_cyclic(|weak_self| Session {
            id: format!("api-session-{id}"),
            snapshot_host,
            project_session,
            language_server: Mutex::new(LanguageServerState::default()),
            parse_config_host,
            binary_responses,
            snapshots: RwLock::new(FxHashMap::default()),
            conn: Mutex::new(None),
            closed: Mutex::new(false),
            batch_pages: Mutex::new(FxHashMap::default()),
            next_batch_page: AtomicU64::new(0),
            module_resolvers: crate::module_resolution::ModuleResolvers::default(),
            base_fs,
            weak_self: Weak::clone(weak_self),
            build_state: crate::build::BuildState::default(),
            source_files: crate::sourcefiles::SourceFileState::default(),
            build_backend: Mutex::new(Some(Arc::new(crate::build::ExecuteBuildBackend))),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    /// Replaces the default in-process build orchestrator implementation.
    pub fn set_build_backend(&self, backend: Arc<dyn crate::build::BuildBackend>) {
        *self.build_backend.lock().unwrap() = Some(backend);
    }

    pub(crate) fn build_backend(&self) -> Option<Arc<dyn crate::build::BuildBackend>> {
        self.build_backend.lock().unwrap().clone()
    }

    pub(crate) fn default_library_path(&self) -> &str {
        if let Some(project_session) = &self.project_session {
            return project_session.default_library_path();
        }
        self.snapshot_host.default_library_path()
    }

    pub(crate) fn weak_self(&self) -> std::sync::Weak<Session> {
        Weak::clone(&self.weak_self)
    }

    /// Go `Session.FS()`: the language server's (overlay) filesystem for an LSP-attached session, otherwise the
    /// snapshot host's.
    pub(crate) fn fs(&self) -> Arc<dyn FS> {
        Arc::clone(&self.base_fs)
    }

    /// Go `snapshotHost.FS()`: the host filesystem, without the language server's overlays.
    pub(crate) fn snapshot_host_fs(&self) -> Arc<dyn FS> {
        self.snapshot_host.shared_fs()
    }

    pub(crate) fn next_batch_page_id(&self) -> u64 {
        self.next_batch_page.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn set_connection(&self, conn: Arc<dyn ClientConn>) {
        *self.conn.lock().unwrap() = Some(conn);
    }

    pub(crate) fn connection(&self) -> Option<Arc<dyn ClientConn>> {
        self.conn.lock().unwrap().clone()
    }

    pub(crate) fn binary_responses(&self) -> bool {
        self.binary_responses
    }

    pub(crate) fn current_directory(&self) -> &str {
        self.snapshot_host.get_current_directory()
    }

    /// Go `Session.FS()` for a standalone session: the host filesystem (possibly callback-backed).
    pub(crate) fn base_fs(&self) -> &dyn FS {
        self.snapshot_host.fs()
    }

    pub(crate) fn to_path(&self, file_name: &str) -> tsrs_core::tspath::Path {
        tsrs_core::tspath::to_path(
            file_name,
            self.current_directory(),
            self.use_case_sensitive_file_names(),
        )
    }

    pub(crate) fn use_case_sensitive_file_names(&self) -> bool {
        self.snapshot_host.fs().use_case_sensitive_file_names()
    }

    /// Releases every registered snapshot (Go `Session.Close`). Idempotent; requests after close fail.
    pub fn close(&self) {
        let mut closed = self.closed.lock().unwrap();
        if *closed {
            return;
        }
        *closed = true;
        self.release_language_server_refs();
        self.source_files.release_all_leases();
        let all: Vec<_> = self.snapshots.write().unwrap().drain().collect();
        drop(all);
        self.batch_pages.lock().unwrap().clear();
    }

    /// Go `releaseLanguageServerRefs`: closes the projects and files this client opened in the language server
    /// and removes the programs it created there.
    fn release_language_server_refs(&self) {
        let Some(project_session) = &self.project_session else {
            return;
        };

        let mut state = self.language_server.lock().unwrap();
        if state.open_state.open_projects.is_empty()
            && state.open_state.open_files.is_empty()
            && state.created_programs.is_empty()
        {
            return;
        }

        let mut api_request = tsrs_project::APISnapshotRequest::default();
        if !state.open_state.open_projects.is_empty() {
            api_request.close_projects = Some(state.open_state.open_projects.clone());
        }
        if !state.open_state.open_files.is_empty() {
            api_request.close_files = Some(state.open_state.open_files.clone());
        }
        if !state.created_programs.is_empty() {
            api_request.remove_programs = Some(state.created_programs.clone());
        }
        let ctx = project_session.with_current_locale(&Context::background());
        let Ok(snapshot) = project_session.api_update(
            &ctx,
            &tsrs_project::FileChangeSummary::default(),
            Some(Arc::new(api_request)),
        ) else {
            return;
        };
        snapshot.deref();
        state.open_state.open_projects.clear();
        state.open_state.open_files.clear();
        state.created_programs.clear();
    }

    /// Go `getSnapshotData`: resolves a client snapshot handle, pinning it for the caller.
    pub fn snapshot_data(&self, handle: SnapshotID) -> ApiResult<Arc<SnapshotData>> {
        self.snapshots
            .read()
            .unwrap()
            .get(&handle)
            .map(|(sd, _)| Arc::clone(sd))
            .ok_or_else(|| ApiError::client(format!("snapshot {handle} not found")))
    }

    /// Go `registerSnapshot`: the same snapshot id returned twice (no changes) bumps the API ref count so
    /// each client-side snapshot can be disposed independently.
    pub(crate) fn register_snapshot(
        &self,
        snapshot: Arc<Snapshot>,
        open_state: OpenState,
        file_system: Option<Arc<crate::requestfs::RequestFileSystem>>,
    ) -> SnapshotID {
        let handle = snapshot.id();
        let mut snapshots = self.snapshots.write().unwrap();
        if let Some(entry) = snapshots.get_mut(&handle) {
            // The stored data already holds a project reference; drop the caller's.
            snapshot.deref();
            entry.1 += 1;
        } else {
            let sd = Arc::new(SnapshotData {
                snapshot,
                file_system,
                open_state,
                checker_state: CheckerSnapshotState::default(),
            });
            snapshots.insert(handle, (sd, 1));
        }
        handle
    }

    /// Go `releaseSnapshot`. The project snapshot is dereferenced when the last in-flight user drops its `Arc`.
    pub(crate) fn release_snapshot(&self, handle: SnapshotID) -> ApiResult<()> {
        let removed = {
            let mut snapshots = self.snapshots.write().unwrap();
            let Some(entry) = snapshots.get_mut(&handle) else {
                return Err(ApiError::client(format!("snapshot {handle} not found")));
            };
            entry.1 -= 1;
            if entry.1 == 0 { snapshots.remove(&handle) } else { None }
        };
        drop(removed);
        Ok(())
    }

    fn dispatch(&self, method: &str, params: &[u8]) -> ApiResult<Response> {
        match method {
            "echo" => {
                return Ok(if self.binary_responses {
                    Response::Binary(params.to_vec())
                } else {
                    Response::Json(tsrs_core::utf8::from_utf8_lossy(params).into_owned())
                });
            }
            "ping" => return Ok(Response::Json("\"pong\"".to_string())),
            _ => {}
        }
        let info = method_info(method)
            .ok_or_else(|| ApiError::invalid_request(format!("unknown API method {method:?}")))?;
        let raw_params = params;
        // Go `unmarshalPayload`: `noParams` methods ignore the payload; the others decode with Go's strict
        // JSON (unpaired surrogates and duplicate members are errors), reported as
        // `failed to unmarshal *api.<Type>: <jsontext error>`.
        let (params, lexemes) = match crate::methods::params_type(method) {
            None => (Value::Null, rustc_hash::FxHashMap::default()),
            Some(go_type) => {
                let decoded = tsrs_api_transport::strictjson::decode_with_names(
                    params,
                    crate::predecode::known_member_name,
                )
                .map_err(|e| match e {
                    DecodeError::Invalid(e) => ApiError::invalid_request(format!(
                        "failed to unmarshal *api.{go_type}: {e}"
                    )),
                    DecodeError::OutOfRange(e) => ApiError::invalid_request(e),
                })?;
                let value = decoded.value;
                // encoding/json/v2 into a struct: `null` leaves the zero value; any other non-object is an error.
                let kind = match &value {
                    Value::Null | Value::Object(_) => None,
                    Value::Array(_) => Some("JSON array".to_string()),
                    Value::String(_) => Some("JSON string".to_string()),
                    Value::Bool(_) => Some("JSON boolean".to_string()),
                    Value::Number(_) | Value::Integer(_) => Some("JSON number".to_string()),
                };
                if let Some(kind) = kind {
                    return Err(ApiError::invalid_request(format!(
                        "failed to unmarshal *api.{go_type}: json: cannot unmarshal {kind} into Go api.{go_type}"
                    )));
                }
                // `null` decodes to the zero-value struct: handlers see `{}`.
                let value = if matches!(value, Value::Null) {
                    Value::Object(JsonMap::new())
                } else {
                    value
                };
                // Go decodes the whole params struct before any lookup (see predecode.rs).
                let lexemes =
                    crate::predecode::predecode(method, go_type, &value, decoded.lexemes)?;
                (value, lexemes)
            }
        };
        let go_type = crate::methods::params_type(method);
        let typed = |e: ApiError| match (e.kind.clone(), go_type) {
            // Field-level decode errors are Go unmarshal errors of the params struct (the exact jsontext wording
            // for nested type mismatches is not reproduced).
            (crate::handler::ErrorKind::InvalidRequest, Some(t))
                if !e.message.starts_with("failed to unmarshal") =>
            {
                ApiError::invalid_request(format!(
                    "failed to unmarshal *api.{t}: json: {}",
                    e.message
                ))
            }
            _ => e,
        };
        self.dispatch_parsed(method, &params, info.owner, raw_params, lexemes).map_err(typed)
    }

    fn dispatch_parsed(
        &self,
        method: &str,
        params: &Value,
        owner: Owner,
        raw: &[u8],
        lexemes: rustc_hash::FxHashMap<String, String>,
    ) -> ApiResult<Response> {
        // Raw payload and exact number literals of this request, for the objects of this params tree (by address).
        let _request = crate::predecode::enter_request(raw, lexemes, params);
        if *self.closed.lock().unwrap() {
            return Err(ApiError::client("session is closed"));
        }
        if owner == Owner::Checker {
            return checker::handle(self, method, params)
                .unwrap_or_else(|| Err(ApiError::unsupported(method)));
        }
        let p = Params(params);
        if method == "batchRequests" {
            if !matches!(params, Value::Null) {
                p.object()?;
            }
            return self.handle_batch_requests(p).map(Response::Json);
        }
        match method {
            "getSourceFile" => return self.handle_get_source_file(p),
            "getConfigSourceFile" => return self.handle_get_config_source_file(p),
            "createSourceFile" => return self.handle_create_source_file(p),
            "createSourceFileFromFile" => return self.handle_create_source_file_from_file(p),
            "retainSourceFile" => return self.handle_retain_source_file(p),
            "releaseSourceFile" => return self.handle_release_source_file(p),
            "getCachedSourceFile" => return self.handle_get_cached_source_file(p),
            "getSymbolOfDeclaration" => return self.handle_get_symbol_of_declaration(p),
            _ => {}
        }
        let result = match method {
            "initialize" => Obj::new()
                .set(
                    "caseSensitivity",
                    Value::Number(f64::from(u8::from(self.use_case_sensitive_file_names()))),
                )
                .set("currentDirectory", Value::String(self.current_directory().to_string()))
                .build(),
            // Config requests allocate their parsed command lines in a scratch region freed once the response
            // value (owned JSON) is built.
            "parseCommandLine" => scratch(|| self.handle_parse_command_line(p))?,
            "readConfigFile" => scratch(|| self.handle_read_config_file(p))?,
            "parseJsonConfigFileContent" => {
                scratch(|| self.handle_parse_json_config_file_content(p))?
            }
            "parseConfigFile" => scratch(|| self.handle_parse_config_file(p))?,
            "createSnapshot" => self.handle_create_snapshot(p)?,
            "updateSnapshot" => self.handle_update_snapshot(p)?,
            "release" => self.handle_release(p)?,
            "getDefaultProjectForFile" => self.handle_get_default_project_for_file(p)?,
            "getSourceFileNames" => self.handle_get_source_file_names(p)?,
            "getModeForUsageLocation" => self.handle_get_mode_for_usage_location(p)?,
            "getModeForResolutionAtIndex" => self.handle_get_mode_for_resolution_at_index(p)?,
            "getResolvedModule" => self.handle_get_resolved_module(p)?,
            "getResolvedModuleFromModuleSpecifier" => {
                self.handle_get_resolved_module_from_module_specifier(p)?
            }
            "getResolvedTypeReferenceDirective" => {
                self.handle_get_resolved_type_reference_directive(p)?
            }
            "getResolvedTypeReferenceDirectiveFromTypeReferenceDirective" => {
                self.handle_get_resolved_type_reference_directive_from_reference(p)?
            }
            "getConfigFileNames" => self.handle_get_config_file_names(p)?,
            "getSourceFileMetadata" => self.handle_get_source_file_metadata(p)?,
            "formatNodeForInsertion" => self.handle_format_node_for_insertion(p)?,
            "getSyntacticDiagnostics" => {
                self.handle_get_diagnostics(p, DiagnosticKind::Syntactic)?
            }
            "getBindDiagnostics" => self.handle_get_diagnostics(p, DiagnosticKind::Bind)?,
            "getSemanticDiagnostics" => self.handle_get_diagnostics(p, DiagnosticKind::Semantic)?,
            "getSuggestionDiagnostics" => {
                self.handle_get_diagnostics(p, DiagnosticKind::Suggestion)?
            }
            "getDeclarationDiagnostics" => {
                self.handle_get_diagnostics(p, DiagnosticKind::Declaration)?
            }
            "getProgramDiagnostics" => self.handle_get_program_diagnostics(p)?,
            "getGlobalDiagnostics" => self.handle_get_global_diagnostics(p)?,
            "getConfigFileParsingDiagnostics" => {
                self.handle_get_config_file_parsing_diagnostics(p)?
            }
            "createModuleResolver" => self.handle_create_module_resolver(p)?,
            "releaseModuleResolver" => self.handle_release_module_resolver(p)?,
            // Standalone resolveModuleName creates a fresh resolver and resolution caches on every call.
            // Go reclaims them via GC; tsrs allocates them in the API server's leak arena unless each
            // request is routed through a scratch region that is freed once the JSON response is built.
            // The inProgressSnapshot path is excluded because resolvers cached in a ProgramResolutionContext
            // outlive the request.
            "resolveModuleName" => {
                if p.u64("inProgressSnapshot")? == 0 {
                    scratch(|| self.handle_resolve_module_name(p))?
                } else {
                    self.handle_resolve_module_name(p)?
                }
            }
            "createBuildOrchestrator" => self.handle_create_build_orchestrator(p)?,
            "disposeBuildOrchestrator" => self.handle_dispose_build_orchestrator(p)?,
            "build" => self.handle_build(p, false, false)?,
            "buildReferences" => self.handle_build(p, true, false)?,
            "cleanBuild" => self.handle_build(p, false, true)?,
            "cleanReferences" => self.handle_build(p, true, true)?,
            "getCurrentLanguageServerSnapshot" => {
                self.handle_get_current_language_server_snapshot(p)?
            }
            "printNode" => self.handle_print_node(p)?,
            "transpileModule" => self.handle_transpile(p, false)?,
            "transpileDeclaration" => self.handle_transpile(p, true)?,
            "transpileModuleFromFile" => self.handle_transpile_from_file(p, false)?,
            "transpileDeclarationFromFile" => self.handle_transpile_from_file(p, true)?,
            "emit" => self.handle_emit(p)?,
            "emitToString" => self.handle_emit_to_string(p)?,
            "getJavaScriptEmit" => {
                self.handle_selected_files_emit(p, tsrs_compiler::EmitOnly::EmitOnlyJs)?
            }
            "getDeclarationEmit" => {
                self.handle_selected_files_emit(p, tsrs_compiler::EmitOnly::EmitOnlyDts)?
            }
            _ => return Err(ApiError::unsupported(method)),
        };
        json_response(&result)
    }
}

impl Handler for Session {
    fn handle_request(&self, method: &str, params: &[u8]) -> ApiResult<Response> {
        match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.dispatch(method, params)
        })) {
            Ok(result) => result,
            Err(panic) => {
                let message = panic
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .or_else(|| panic.downcast_ref::<&str>().copied())
                    .unwrap_or("unknown panic");
                // tsrs_module's resolver, like pinned Go's (resolver.go `Unexpected moduleResolution`), panics when a
                // module is resolved with a moduleResolution number that names no kind; Go's server crashes there.
                if let Some(n) = message.strip_prefix("Unexpected moduleResolution: ") {
                    return Err(ApiError::client(format!(
                        "unsupported moduleResolution value {n} (not a ModuleResolutionKind)"
                    )));
                }
                Err(ApiError::internal(format!("panic: {message}")))
            }
        }
    }

    fn handle_notification(&self, _method: &str, _params: &[u8]) -> ApiResult<()> {
        // Go `Session.HandleNotification` ignores all notifications.
        Ok(())
    }
}

/// Runs `f` with a fresh arena region as the allocation target and frees the region afterwards. Only for
/// work whose result is owned data (JSON values) and that stores nothing arena-allocated in longer-lived state.
fn scratch<T>(f: impl FnOnce() -> T) -> T {
    let region = tsrs_core::arena::Region::new(64 << 10);
    let result = {
        let _scope = region.enter();
        f()
    };
    drop(region);
    result
}

/// Parses request params (raw JSON bytes; empty means absent, Go `UnmarshalParams` returns nil).
pub(crate) fn json_response(value: &Value) -> ApiResult<Response> {
    // Most results are small objects: start with room for one instead of growing to it.
    json::marshal_with_capacity(value, 256).map(Response::Json).map_err(ApiError::internal)
}
