// session.go handleCreateBuildOrchestrator / handleDisposeBuildOrchestrator / handleBuild /
// handleBuildReferences / handleCleanBuild / handleCleanReferences.
//
// Every API session installs the in-process tsrs_execute build backend, including sessions attached to the
// language server. `Session::set_build_backend` can replace it for an embedding host.

use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tsrs_ast::Diagnostic;
use tsrs_core::json::Value;
use tsrs_core::{BuildOptions, CompilerOptions, P};
use tsrs_execute::build::{Options, new_orchestrator};
use tsrs_execute::tsc::System;
use tsrs_tsoptions::{ParseConfigHost, gojson};
use tsrs_vfs::FS;

use crate::diagnostics::diagnostic_responses;
use crate::handler::{ApiError, ApiResult};
use crate::session::Session;
use crate::wire::{Obj, Params, strings};

/// Go `build.OrchestratorResult` fields the API reports.
#[derive(Default)]
pub struct BuildOutcome {
    status: i32,
    diagnostics: Vec<P<Diagnostic>>,
    projects: usize,
    projects_built: usize,
    timestamp_updates: usize,
    files_deleted: Vec<String>,
}

/// Inputs of Go `handleCreateBuildOrchestrator` (`apiBuildSystem` + `ParseBuildCommandLine`).
pub struct BuildRequest {
    fs: Arc<dyn FS>,
    default_library_path: String,
    current_directory: String,
    root_names: Vec<String>,
    build_options: Option<BuildOptions>,
    compiler_options: Option<CompilerOptions>,
}

pub trait BuildOrchestrator: Send {
    /// Go `Orchestrator.Build` (`only_references = false`) / `BuildReferences`.
    fn build(&mut self, project: &str, only_references: bool) -> BuildOutcome;
    /// Go `Orchestrator.Clean` / `CleanReferences`.
    fn clean(&mut self, project: &str, only_references: bool) -> BuildOutcome;
}

pub trait BuildBackend: Send + Sync {
    fn create(&self, request: BuildRequest) -> Box<dyn BuildOrchestrator>;
}

#[derive(Default)]
pub(crate) struct BuildState {
    next_id: AtomicU64,
    orchestrators: Mutex<FxHashMap<u64, Orchestrator>>,
}

struct Orchestrator {
    backend: Box<dyn BuildOrchestrator>,
}

fn outcome_response(o: BuildOutcome, clean: bool) -> Value {
    let stats = Obj::new()
        .set("Projects", Value::Number(o.projects as f64))
        .set("ProjectsBuilt", Value::Number(o.projects_built as f64))
        .set("TimestampUpdates", Value::Number(o.timestamp_updates as f64))
        .build();
    let mut r = Obj::new()
        .set("status", Value::Number(o.status as f64))
        .set_opt(
            "diagnostics",
            (!o.diagnostics.is_empty()).then(|| diagnostic_responses(&o.diagnostics)),
        )
        .set("statistics", stats);
    if clean {
        r = r.set_opt(
            "filesDeleted",
            (!o.files_deleted.is_empty()).then(|| strings(o.files_deleted)),
        );
    }
    r.build()
}

impl Session {
    pub(crate) fn handle_create_build_orchestrator(&self, p: Params) -> ApiResult<Value> {
        let backend = self.build_backend().ok_or_else(|| {
            ApiError::unsupported("createBuildOrchestrator (no build backend in this session)")
        })?;
        let root_names = p.strings("rootNames")?;
        let cwd = match p.str("cwd")? {
            "" => self.current_directory().to_string(),
            c => c.to_string(),
        };
        let mut build_options = if p.has("buildOptions") {
            Some(
                gojson::build_options_from_go_json(p.get("buildOptions"))
                    .map_err(ApiError::invalid_request)?,
            )
        } else {
            None
        };
        if let Some(options) = &mut build_options {
            crate::predecode::exact_build_options_ints(p.get("buildOptions"), options);
        }
        let mut compiler_options = if p.has("compilerOptions") {
            Some(
                gojson::compiler_options_from_go_json(p.get("compilerOptions"))
                    .map_err(ApiError::invalid_request)?,
            )
        } else {
            None
        };
        if let Some(options) = &mut compiler_options {
            crate::predecode::exact_compiler_options_ints(p.get("compilerOptions"), options);
        }
        let orchestrator = backend.create(BuildRequest {
            fs: self.snapshot_host_fs(),
            default_library_path: self.default_library_path().to_string(),
            current_directory: cwd,
            root_names,
            build_options,
            compiler_options,
        });
        let id = self.build_state.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        // A build that failed by unwinding (the moduleResolution boundary above) leaves the map intact; its lock
        // is only poisoned.
        self.build_state
            .orchestrators
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, Orchestrator { backend: orchestrator });
        Ok(Obj::new().set("buildOrchestratorID", Value::Number(id as f64)).build())
    }

    pub(crate) fn handle_dispose_build_orchestrator(&self, p: Params) -> ApiResult<Value> {
        let id = p.u64("buildOrchestratorID")?;
        let removed = self
            .build_state
            .orchestrators
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
        match removed {
            Some(_) => Ok(Value::Bool(true)),
            None => Err(ApiError::internal("build orchestrator not found while disposing")),
        }
    }

    /// Go holds `buildMu` for the whole build; builds on one session are serialized.
    pub(crate) fn handle_build(
        &self,
        p: Params,
        only_references: bool,
        clean: bool,
    ) -> ApiResult<Value> {
        let id = p.u64("buildOrchestratorID")?;
        let project = p.str("project")?;
        // Go holds `buildMu` for the whole build. A request re-entering from a client callback while a build
        // holds it gets a bounded error instead of a deadlock.
        let mut orchestrators = tsrs_api_transport::reentrancy::lock_for_request(
            &self.build_state.orchestrators,
            "the build orchestrator",
        )
        .map_err(|e| ApiError::internal(e.message))?;
        let Some(o) = orchestrators.get_mut(&id) else {
            let what = match (clean, only_references) {
                (false, false) => format!("build orchestrator not found while building {project}"),
                (false, true) => {
                    format!("build orchestrator not found for building references for {project}")
                }
                (true, false) => format!("build orchestrator not found while cleaning {project}"),
                (true, true) => {
                    format!("build orchestrator not found while cleaning references for {project}")
                }
            };
            return Err(ApiError::internal(what));
        };
        // A moduleResolution number with no named kind fails (stable client error, see `Session::handle_request`)
        // only where a module is actually resolved, where pinned Go panics; import-free builds succeed as in Go.
        let outcome = if clean {
            o.backend.clean(project, only_references)
        } else {
            o.backend.build(project, only_references)
        };
        Ok(outcome_response(outcome, clean))
    }
}

/// Go `apiBuildSystem`: output is discarded, the filesystem is the session's host filesystem.
struct ApiBuildSystem {
    fs: Arc<dyn FS>,
    default_library_path: String,
    current_directory: String,
    start: Instant,
}

impl System for ApiBuildSystem {
    fn fs(&self) -> Arc<dyn FS> {
        Arc::clone(&self.fs)
    }
    fn default_library_path(&self) -> &str {
        &self.default_library_path
    }
    fn get_current_directory(&self) -> &str {
        &self.current_directory
    }
    fn write(&self, _text: &str) {}
    fn flush(&self) {}
    fn write_output_is_tty(&self) -> bool {
        false
    }
    fn get_environment_variable(&self, _name: &str) -> Option<String> {
        None
    }
    fn now(&self) -> Instant {
        Instant::now()
    }
    fn since_start(&self) -> Duration {
        self.start.elapsed()
    }
    // session.go:1919
    fn spawn(
        &self,
        _command: &[String],
        _dir: &str,
        _stderr: Option<Box<dyn std::io::Write + Send>>,
    ) -> Result<tsrs_execute::tsc::ReadWriteCloser, String> {
        Err("spawning processes is not supported by the API build orchestrator".to_string())
    }
}

impl ParseConfigHost for ApiBuildSystem {
    fn fs(&self) -> &dyn FS {
        &*self.fs
    }
    fn get_current_directory(&self) -> &str {
        &self.current_directory
    }
}

pub(crate) struct ExecuteBuildBackend;

/// One API build orchestrator handle. Go keeps one `build.Orchestrator`: `Build` rechecks every project in the
/// build order (statuses, configs, mtimes, caches) and regenerates the graph; `Clean` uses the graph of the last
/// build ("cleans the last built configuration"). Here each build gets a fresh CLI orchestrator (the state Go's
/// recheck leaves) whose allocations live in collectable regions; it is kept for later cleans and freed when the
/// next build replaces it or the handle is disposed, so memory stays bounded by one build.
struct ExecuteOrchestrator {
    sys: &'static ApiBuildSystem,
    command: P<tsrs_tsoptions::ParsedBuildCommandLine>,
    orchestrator: Option<ApiOrchestratorOwner>,
}

// Owns one API-mode orchestrator and its regions. A successful build keeps this owner until the next successful
// build or disposal, so its diagnostics remain alive while the API serializes the response and cleans use its graph.
struct ApiOrchestratorOwner(&'static tsrs_execute::build::Orchestrator);

impl Drop for ApiOrchestratorOwner {
    fn drop(&mut self) {
        // SAFETY: `fresh` creates and enables this orchestrator exclusively for one serialized API handle.
        // Its build/clean call has returned or unwound (joining scoped workers), and no result outlives the owner.
        unsafe { tsrs_execute::build::free_api_orchestrator(self.0) };
    }
}

impl ExecuteOrchestrator {
    fn fresh(&self) -> ApiOrchestratorOwner {
        let o = new_orchestrator(Options { sys: self.sys, command: self.command, testing: None });
        o.enable_api_mode();
        ApiOrchestratorOwner(o)
    }
}

impl BuildBackend for ExecuteBuildBackend {
    fn create(&self, request: BuildRequest) -> Box<dyn BuildOrchestrator> {
        // `&'static` system and command line: the CLI build module requires them (one small leak per orchestrator).
        let sys: &'static ApiBuildSystem = Box::leak(Box::new(ApiBuildSystem {
            fs: request.fs,
            default_library_path: request.default_library_path,
            current_directory: request.current_directory,
            start: Instant::now(),
        }));
        let mut command = tsrs_tsoptions::parse_build_command_line(&request.root_names, sys);
        if let Some(options) = request.compiler_options {
            command.compiler_options = options;
        }
        if let Some(options) = request.build_options {
            command.build_options = options;
        }
        // API builds allocate in per-task regions (ExecuteOrchestrator); program construction and checking must stay on
        // the task's thread so their allocations land there, not in the compiler worker pool's thread arenas.
        command.compiler_options.single_threaded = tsrs_core::Tristate::True;
        Box::new(ExecuteOrchestrator { sys, command: P::new(command), orchestrator: None })
    }
}

fn outcome(result: tsrs_execute::build::OrchestratorResult) -> BuildOutcome {
    BuildOutcome {
        status: result.status.unwrap_or(tsrs_execute::tsc::ExitStatus::Success) as i32,
        diagnostics: result.errors.unwrap_or_default(),
        projects: result.statistics.projects,
        projects_built: result.statistics.projects_built,
        timestamp_updates: result.statistics.timestamp_updates,
        files_deleted: result.files_to_delete.unwrap_or_default(),
    }
}

impl BuildOrchestrator for ExecuteOrchestrator {
    fn build(&mut self, project: &str, only_references: bool) -> BuildOutcome {
        let o = self.fresh();
        let result = outcome(o.0.build_for_api(project, only_references));
        self.orchestrator = Some(o);
        result
    }
    fn clean(&mut self, project: &str, only_references: bool) -> BuildOutcome {
        if self.orchestrator.is_none() {
            self.orchestrator = Some(self.fresh());
        }
        let o = self.orchestrator.as_ref().unwrap();
        outcome(o.0.clean_for_api(project, only_references))
    }
}
