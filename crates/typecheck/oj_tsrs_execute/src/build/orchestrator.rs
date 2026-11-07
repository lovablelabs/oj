// Port of execute/build/orchestrator.go, including watch mode (`tsc -b --watch`: `Watch` / `DoCycle`).
//
// Like Go, the build tasks run on `--builders` threads (default 4, 1 with --singleThreaded; `rangeTasks`) that take
// projects from ScheduleOrder() by an atomic index and block on their upstream tasks' `done` signals, while the
// calling thread
// (Go: a reporter goroutine) waits on each task's `built` signal in Order() and prints its buffered output. The
// signals are Mutex + Condvar stand-ins for Go's closed channels (buildtask.rs closeSignal). Each task's state is
// Mutex/atomic; the shared host caches are Mutex-guarded.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{Diagnostic, SourceFile, new_compiler_diagnostic};
use tsrs_compiler::CompilerHost;
use tsrs_core::P;
use tsrs_core::collections::Set;
use tsrs_core::tspath::{self, ComparePathsOptions, Path};
use tsrs_diagnostics as diagnostics;
use tsrs_tsoptions::ParsedBuildCommandLine;

use super::buildtask::{BuildTask, TaskResult};
use super::host::HostImpl;
use super::parsecache::ParseCache;
use crate::tsc::{
    self, CommandLineResult, CommandLineTesting, DiagnosticReporter, DiagnosticsReporter,
    ExitStatus, Statistics, System, Writer, create_builder_status_reporter,
    create_diagnostic_reporter_with_writer, create_report_error_summary,
};
use crate::watchmanager::{self, DirWatchSet, WatchManager};

pub struct Options {
    pub sys: &'static dyn System,
    pub command: P<ParsedBuildCommandLine>,
    pub testing: Option<&'static dyn CommandLineTesting>,
}

#[derive(Default)]
pub struct OrchestratorResult {
    pub status: Option<ExitStatus>,
    pub errors: Option<Vec<P<Diagnostic>>>,
    pub statistics: Statistics,
    pub files_to_delete: Option<Vec<String>>,
}

impl OrchestratorResult {
    pub(crate) fn status(&self) -> ExitStatus {
        self.status.unwrap_or(ExitStatus::Success)
    }

    // orchestrator.go:39
    fn report(&mut self, o: &Orchestrator) {
        self.report_with_files_to_delete(o, true);
    }

    // orchestrator.go:43
    fn report_with_files_to_delete(&mut self, o: &Orchestrator, report_files_to_delete: bool) {
        match &o.watch_status_reporter {
            Some(watch_status_reporter) => {
                let errors = self.errors.as_ref().map_or(0, Vec::len);
                watch_status_reporter(if errors == 1 {
                    new_compiler_diagnostic(
                        &diagnostics::FOUND_1_ERROR_WATCHING_FOR_FILE_CHANGES,
                        &[],
                    )
                } else {
                    new_compiler_diagnostic(
                        &diagnostics::FOUND_0_ERRORS_WATCHING_FOR_FILE_CHANGES,
                        &[&errors],
                    )
                });
            }
            None => (o.error_summary_reporter)(self.errors.as_deref().unwrap_or_default()),
        }
        if report_files_to_delete && let Some(files_to_delete) = &self.files_to_delete {
            let mut file_list = String::new();
            for file in files_to_delete {
                let _ = write!(file_list, "\r\n * {file}");
            }
            o.create_builder_status_reporter(None)(new_compiler_diagnostic(
                &diagnostics::A_NON_DRY_BUILD_WOULD_DELETE_THE_FOLLOWING_FILES_COLON_0,
                &[&file_list],
            ));
        }
        let options = &o.opts.command.compiler_options;
        if !options.diagnostics.is_true() && !options.extended_diagnostics.is_true() {
            return;
        }
        self.statistics.set_total_time(o.opts.sys.since_start());
        let sys = o.opts.sys;
        self.statistics.report(&|t: &str| sys.write(t), o.opts.testing);
    }
}

pub struct Orchestrator {
    pub(crate) opts: Options,
    pub(crate) compare_paths_options: ComparePathsOptions,
    host: OnceLock<&'static HostImpl>,

    // contentMapperHost transforms content-mapped files; it is created once per build session (when
    // enabled) and shared across all projects so mapper processes are consolidated. It closes itself when
    // the session context is cancelled (see contentmapper.New).
    // (Here `start_worker` closes it when the session ends.)
    pub(crate) content_mapper_host: Mutex<Option<Arc<dyn tsrs_contentmapper::Host>>>,

    // order generation result
    pub tasks: Mutex<FxHashMap<Path, P<BuildTask>>>,
    pub order: Mutex<Vec<String>>,
    errors: Mutex<Vec<P<Diagnostic>>>,
    graph_generated: AtomicBool,
    // Set when a builder thread panicked (see range_tasks).
    pub(crate) aborted: AtomicBool,
    // API builds (crates/tsrs_cli/src/api.rs): every thread that works on this orchestrator allocates in a
    // region collected here, so the whole orchestrator (configs, programs, checkers' arenas, diagnostics) can be
    // freed at once (`free_api_orchestrator`). Off for the CLI, whose single build runs to process exit.
    regions: Mutex<Vec<tsrs_core::arena::Region>>,
    api_mode: AtomicBool,
    // API clean existence answers, kept until the next build (which uses a fresh orchestrator): Go's `Clean` asks
    // the orchestrator's cachedvfs, which only `Build`'s recheck clears, so a later clean reuses them.
    api_clean_exists: Mutex<FxHashMap<String, bool>>,

    error_summary_reporter: DiagnosticsReporter<'static>,
    // Watch mode: the status lines ("Found N errors. Watching for file changes.") and the directory watches.
    watch_status_reporter: Option<DiagnosticReporter<'static>>,
    wm: Arc<WatchManager>,
    // tsrs-only, watch mode: frees the programs the tasks replace and the files nothing refers to (watchmem.rs). Other
    // CLI builds: the programs of the built tasks (`frees_built_programs`).
    pub(crate) freer: crate::watchmem::Freer,
    // tsrs-only, watch mode: the background computation of the tasks' placeholder signatures (`start_signature_job`).
    signature_job: Mutex<Option<(tsrs_core::context::CancelFunc, std::thread::JoinHandle<()>)>>,

    // order sorted by dependency depth, to reduce how often builders block on upstream projects
    schedule_order: Mutex<Vec<String>>,
    // tsrs-only (`directory_change_matters`).
    wanted_watch_dirs: Mutex<FxHashSet<Path>>,
    // tsrs-only (`cached_realpath`): the real paths of the build info's files, kept across cycles.
    realpaths: Mutex<FxHashMap<String, String>>,
}

impl Orchestrator {
    pub(crate) fn host(&self) -> &'static HostImpl {
        self.host.get().unwrap()
    }

    // orchestrator.go:94
    pub(crate) fn relative_file_name(&self, file_name: &str) -> String {
        tspath::convert_to_relative_path(file_name, &self.compare_paths_options)
    }

    // orchestrator.go:98
    pub fn to_path(&self, file_name: &str) -> Path {
        tspath::to_path(
            file_name,
            &self.compare_paths_options.current_directory,
            self.compare_paths_options.use_case_sensitive_file_names,
        )
    }

    // Only Go tests call it (graph_test.go), and those are not ported.
    pub fn order(&self) -> Vec<String> {
        self.order.lock().unwrap().clone()
    }

    // ScheduleOrder is the order in which builders pick up projects: Order() stably sorted by dependency depth.
    fn schedule_order(&self) -> Vec<String> {
        self.schedule_order.lock().unwrap().clone()
    }

    // orchestrator.go:131
    pub fn compute_schedule_order(&self, order: &[String]) -> Vec<String> {
        let mut entries: Vec<(String, usize)> = Vec::with_capacity(order.len());
        let mut depths: FxHashMap<P<BuildTask>, usize> = FxHashMap::default();
        for config in order {
            let task = self.get_task(&self.to_path(config));
            let mut depth = 0;
            depth = task.up_stream.lock().unwrap().iter().fold(depth, |mut depth, upstream| {
                depth = depth.max(depths.get(&upstream.task).copied().unwrap_or(0) + 1);

                depth
            });
            depths.insert(task, depth);
            entries.push((config.clone(), depth));
        }
        entries.sort_by_key(|e| e.1);
        entries.into_iter().map(|e| e.0).collect()
    }

    // orchestrator.go:155
    // Only Go tests call it (graph_test.go), and those are not ported.

    // orchestrator.go:171
    pub(crate) fn get_task(&self, path: &Path) -> P<BuildTask> {
        match self.tasks.lock().unwrap().get(path) {
            Some(task) => *task,
            None => panic!("No build task found for {}", path.as_str()),
        }
    }

    // orchestrator.go:179
    fn create_build_tasks(
        &self,
        old_tasks: Option<&FxHashMap<Path, P<BuildTask>>>,
        configs: &[String],
    ) {
        for config in configs {
            let path = self.to_path(config);
            let mut task = None;
            let mut build_info = None;
            if let Some(existing) = old_tasks.and_then(|old| old.get(&path)) {
                if existing.dirty.load(Ordering::SeqCst) {
                    existing.close_content_mapper_project();
                    build_info = existing.take_build_info_entry();
                } else {
                    // Reuse existing task if config is same
                    task = Some(*existing);
                }
            }
            let task = task.unwrap_or_else(|| {
                let task = P::new(BuildTask::new(config.clone(), old_tasks.is_none()));
                task.pending.store(true, Ordering::SeqCst);
                task.set_build_info_entry(build_info);
                task
            });
            if self.tasks.lock().unwrap().contains_key(&path) {
                continue;
            }
            self.tasks.lock().unwrap().insert(path.clone(), task);
            *task.resolved.lock().unwrap() =
                self.host().get_resolved_project_reference(config, path);
            task.up_stream.lock().unwrap().clear();
            if let Some(resolved) = task.resolved_opt() {
                self.create_build_tasks(old_tasks, resolved.resolved_project_reference_paths());
            }
        }
    }

    // orchestrator.go:213
    fn setup_build_task(
        &self,
        config_name: &str,
        down_stream: Option<P<BuildTask>>,
        in_circular_context: bool,
        completed: &mut Set<Path>,
        analyzing: &mut Set<Path>,
        circularity_stack: &mut Vec<String>,
    ) -> Option<P<BuildTask>> {
        let path = self.to_path(config_name);
        let task = self.get_task(&path);
        if !completed.has(&path) {
            if analyzing.has(&path) {
                if !in_circular_context {
                    self.errors.lock().unwrap().push(new_compiler_diagnostic(
                        &diagnostics::PROJECT_REFERENCES_MAY_NOT_FORM_A_CIRCULAR_GRAPH_CYCLE_DETECTED_COLON_0,
                        &[&circularity_stack.join("\n")],
                    ));
                }
                return None;
            }
            analyzing.add(path.clone());
            circularity_stack.push(config_name.to_string());
            if let Some(resolved) = task.resolved_opt() {
                for (index, sub_reference) in
                    resolved.resolved_project_reference_paths().iter().enumerate()
                {
                    let upstream = self.setup_build_task(
                        sub_reference,
                        Some(task),
                        in_circular_context || resolved.project_references()[index].circular,
                        completed,
                        analyzing,
                        circularity_stack,
                    );
                    if let Some(upstream) = upstream {
                        task.up_stream.lock().unwrap().push(super::buildtask::UpstreamTask {
                            task: upstream,
                            ref_index: index,
                        });
                    }
                }
            }
            circularity_stack.pop();
            completed.add(path);
            task.built.reset();
            task.done.reset();
            self.order.lock().unwrap().push(config_name.to_string());
        }
        if self.opts.command.compiler_options.watch.is_true()
            && let Some(down_stream) = down_stream
        {
            task.down_stream.lock().unwrap().push(down_stream);
        }
        Some(task)
    }

    // orchestrator.go:265
    fn generate_graph(&self) {
        self.generate_graph_with(None);
    }

    // orchestrator.go:252 GenerateGraphReusingOldTasks
    fn generate_graph_reusing_old_tasks(&self) {
        let old = std::mem::take(&mut *self.tasks.lock().unwrap());
        self.order.lock().unwrap().clear();
        self.errors.lock().unwrap().clear();
        self.generate_graph_with(Some(&old));
    }

    fn generate_graph_with(&self, old_tasks: Option<&FxHashMap<Path, P<BuildTask>>>) {
        let projects = self.opts.command.resolved_project_paths().to_vec();
        // Parse all config files (Go: in parallel)
        self.create_build_tasks(old_tasks, &projects);

        // Generate the graph
        let mut completed = Set::default();
        let mut analyzing = Set::default();
        let mut circularity_stack = Vec::new();
        for project in &projects {
            self.setup_build_task(
                project,
                None,
                false,
                &mut completed,
                &mut analyzing,
                &mut circularity_stack,
            );
        }
        let order = self.order.lock().unwrap().clone();
        *self.schedule_order.lock().unwrap() = self.compute_schedule_order(&order);
        if let Some(old_tasks) = old_tasks {
            // Each old task is closed on its own; the order does not matter.
            #[expect(
                clippy::iter_over_hash_type,
                reason = "closes each replaced task's content mapper project; no output depends on the order"
            )]
            for (path, old_task) in old_tasks {
                if self.tasks.lock().unwrap().get(path).is_some_and(|task| *task == *old_task) {
                    continue;
                }
                old_task.close_content_mapper_project();
            }
        }
        self.graph_generated.store(true, Ordering::SeqCst);
    }

    // orchestrator.go:337
    fn recheck_all_projects(&'static self, project: &str) {
        if !self.graph_generated.load(Ordering::SeqCst) {
            return;
        }
        let Some(order) = self.get_build_order_for(project) else {
            return;
        };
        for config in &order {
            let path = self.to_path(config);
            let task = self.get_task(&path);
            task.reset_status();
            task.reset_config(self, &path);
        }
        *self.host().m_times.write().unwrap() = Arc::default();
        self.reset_caches();
    }

    // orchestrator.go:506
    fn reset_caches(&self) {
        let h = self.host();
        h.cached_fs.clear_cache();
        *h.extended_config_cache.lock().unwrap() = Arc::new(tsc::ExtendedConfigCache::default());
        h.source_files.reset();
        h.config_times.lock().unwrap().clear();
    }

    // tsc -b entrypoint
    // orchestrator.go:295
    pub(crate) fn start(&'static self) -> CommandLineResult {
        if self.frees_built_programs() {
            // The declaration and JSON files every project shares go to regions of their own (`host::get_source_file`),
            // bound there, so that freeing a project's program does not take their binder data with it.
            tsrs_compiler::bind_in_file_regions();
        }
        let status = self.start_worker("", false /*onlyReferences*/).status();
        // Under the test harness a watch session returns after the first build; the harness drives the cycles.
        let watcher = (self.opts.command.compiler_options.watch.is_true()
            && self.opts.testing.is_some())
        .then(|| Box::new(OrchestratorWatcher(self)) as Box<dyn tsc::Watcher>);
        CommandLineResult { status, watcher }
    }

    /// Makes this orchestrator allocate in collectable regions and retain per-file API diagnostics.
    pub fn enable_api_mode(&self) {
        self.api_mode.store(true, Ordering::SeqCst);
    }

    /// tsrs-only (Go's GC): whether each project's program is freed once the project is built (`free_built_program`).
    /// A CLI build that is not a watch session; the API frees its programs its own way, a watch session keeps each as
    /// the old state of the next cycle, and the test harness reads them at report time (`Testing::on_program`).
    pub(crate) fn frees_built_programs(&self) -> bool {
        !self.opts.command.compiler_options.watch.is_true()
            && self.opts.testing.is_none()
            && !self.api_mode.load(Ordering::SeqCst)
    }

    pub(crate) fn uses_canonical_diagnostics(&self) -> bool {
        !self.api_mode.load(Ordering::SeqCst)
    }

    // A fresh region for the calling thread, registered for `free_api_orchestrator`; `None` for CLI builds.
    pub(crate) fn enter_api_region(&self) -> Option<tsrs_core::arena::RegionScope> {
        if !self.api_mode.load(Ordering::SeqCst) {
            return None;
        }
        let region = tsrs_core::arena::Region::new(1 << 20);
        let scope = region.enter();
        self.regions.lock().unwrap().push(region);
        Some(scope)
    }

    // orchestrator.go:295/301 `Build` / `BuildReferences` entrypoints for the API: one orchestrator per API
    // handle, rechecked (statuses, configs, mtimes, caches) on every call; unchanged tasks are reused.
    pub fn build_for_api(
        &'static self,
        project: &str,
        only_references: bool,
    ) -> OrchestratorResult {
        {
            let _region = self.enter_api_region();
            self.recheck_all_projects(project);
        }
        self.start_worker(project, only_references)
    }

    // orchestrator.go:354-414 `Clean` / `CleanReferences` entrypoints for the API.
    pub fn clean_for_api(
        &'static self,
        project: &str,
        only_references: bool,
    ) -> OrchestratorResult {
        let _region = self.enter_api_region();
        if !self.graph_generated.load(Ordering::SeqCst) {
            self.generate_graph();
        }
        let errors = self.errors.lock().unwrap().clone();
        if !errors.is_empty() {
            let mut result = OrchestratorResult {
                status: Some(ExitStatus::ProjectReferenceCycleOutputsSkipped),
                errors: Some(errors),
                ..Default::default()
            };
            result.report_with_files_to_delete(self, true);
            return result;
        }
        let Some(mut order) = self.get_build_order_for(project) else {
            return OrchestratorResult {
                status: Some(ExitStatus::InvalidProjectOutputsSkipped),
                ..Default::default()
            };
        };
        if only_references {
            order.pop();
        }
        let mut result = OrchestratorResult::default();
        result.statistics.projects = order.len();
        let dry = self.opts.command.build_options.dry.is_true();
        let report_diagnostic = self.create_diagnostic_reporter(None);
        let mut files_to_delete = Vec::new();
        for config in &order {
            let task = self.get_task(&self.to_path(config));
            let Some(resolved) = task.resolved_opt() else {
                let diagnostic =
                    new_compiler_diagnostic(&diagnostics::FILE_0_NOT_FOUND, &[&task.config]);
                report_diagnostic(diagnostic);
                result.errors.get_or_insert_with(Vec::new).push(diagnostic);
                continue;
            };
            let inputs: Set<Path> = tsrs_core::collections::new_set_from_items(
                resolved.file_names().iter().map(|f| self.to_path(f)),
            );
            let outputs = resolved
                .get_output_file_names()
                .into_iter()
                .chain([resolved.get_build_info_file_name()]);
            let mut deleted = false;
            for output_file in outputs {
                deleted = self.clean_project_output_for_api(
                    &output_file,
                    &inputs,
                    dry,
                    &mut files_to_delete,
                    &report_diagnostic,
                ) || deleted;
            }
            if deleted {
                task.reset_status();
                task.set_build_info_entry(None);
            }
        }
        if !files_to_delete.is_empty() {
            result.files_to_delete = Some(files_to_delete);
        }
        result.report_with_files_to_delete(self, dry);
        result
    }

    // orchestrator.go:452
    fn clean_project_output_for_api(
        &self,
        output_file: &str,
        inputs: &Set<Path>,
        dry: bool,
        files_to_delete: &mut Vec<String>,
        report: &DiagnosticReporter<'static>,
    ) -> bool {
        // Existence is checked on the uncached system filesystem: this port's up-to-date checks may cache a
        // negative `file_exists` for an output that the build then writes, and the cache is only cleared by the
        // next recheck (Go's checks go through mtimes and do not populate that entry).
        let sys_fs = self.opts.sys.fs();
        let fs: &dyn tsrs_vfs::FS = &*sys_fs;
        if output_file.is_empty() || inputs.has(&self.to_path(output_file)) {
            return false;
        }
        // Go caches the answer (cachedvfs) and `Clean` never clears it, so a file deleted by an earlier clean is
        // reported (and removed, a no-op) again by the next clean before a build, and one created in between is not.
        let exists = *self
            .api_clean_exists
            .lock()
            .unwrap()
            .entry(output_file.to_string())
            .or_insert_with(|| fs.file_exists(output_file));
        if !exists {
            return false;
        }
        files_to_delete.push(output_file.to_string());
        if dry {
            return false;
        }
        if fs.remove(output_file).is_err() {
            report(new_compiler_diagnostic(&diagnostics::FAILED_TO_DELETE_FILE_0, &[&output_file]));
            return false;
        }
        true
    }

    // orchestrator.go:311
    fn start_worker(&'static self, project: &str, only_references: bool) -> OrchestratorResult {
        let content_mapper_host =
            tsc::new_content_mapper_host(self.opts.sys, &self.opts.command.compiler_options);
        self.content_mapper_host.lock().unwrap().clone_from(&content_mapper_host);
        // Go defers the host's Close unless this is a watch session under test. Outside tests a watch session runs its
        // cycles in `watch` below, so the host closes when the session ends; the harness runs the cycles after this
        // returns, so it keeps the host open.
        let watch = self.opts.command.compiler_options.watch.is_true();
        let close_host =
            if watch && self.opts.testing.is_some() { None } else { content_mapper_host };
        let _close_content_mapper_host =
            tsc::ContentMapperCloser { host: close_host, project: None };
        if let Some(watch_status_reporter) = &self.watch_status_reporter {
            watch_status_reporter(new_compiler_diagnostic(
                &diagnostics::STARTING_COMPILATION_IN_WATCH_MODE,
                &[],
            ));
        }
        {
            let _region = self.enter_api_region();
            if self.graph_generated.load(Ordering::SeqCst) {
                self.generate_graph_reusing_old_tasks();
            } else {
                self.generate_graph();
            }
        }
        let Some(mut order) = self.get_build_order_for(project) else {
            return OrchestratorResult {
                status: Some(ExitStatus::InvalidProjectOutputsSkipped),
                ..Default::default()
            };
        };
        if only_references && self.errors.lock().unwrap().is_empty() {
            if project.is_empty() {
                return OrchestratorResult {
                    status: Some(ExitStatus::InvalidProjectOutputsSkipped),
                    ..Default::default()
                };
            }
            order.pop();
        }
        // tsrs-only (notes/mem-lazy-dts-members.md): cached declaration files are shared by all projects, so keep
        // their lists eager if any project checks them. Solution configs build no program. Forced members go to
        // the forcing thread's arena (`LazyNodeList::force`), which outlives the freed program regions.
        if self.frees_built_programs()
            && tsrs_compiler::lazy_dts_allowed()
            && order.iter().all(|config| {
                self.get_task(&self.to_path(config)).resolved_opt().is_some_and(|resolved| {
                    (resolved.file_names().is_empty() && !resolved.project_references_is_nil())
                        || crate::execute::declaration_files_unchecked(resolved)
                })
            })
        {
            tsrs_compiler::enable_lazy_dts();
        }
        // tsrs-only (`queue_files_changed_since`): when the first build started.
        let build_started = std::time::SystemTime::now();
        let result = self.build_or_clean_order(&order);
        if watch {
            self.watch(build_started);
        }
        result
    }

    // orchestrator.go:426
    fn get_build_order_for(&self, project: &str) -> Option<Vec<String>> {
        fn add_project_and_references(
            o: &Orchestrator,
            projects: &mut Set<Path>,
            task: P<BuildTask>,
        ) {
            let path = o.to_path(&task.config);
            if projects.has(&path) {
                return;
            }
            projects.add(path);
            for upstream in task.up_stream.lock().unwrap().iter() {
                add_project_and_references(o, projects, upstream.task);
            }
        }

        if project.is_empty() {
            return Some(self.order.lock().unwrap().clone());
        }

        let config = tsrs_core::resolve_config_file_name_of_project_reference(
            &tspath::resolve_path(self.opts.sys.get_current_directory(), &[project]),
        );
        let target = *self.tasks.lock().unwrap().get(&self.to_path(&config))?;

        let mut projects: Set<Path> = Set::default();
        add_project_and_references(self, &mut projects, target);

        let order: Vec<String> = self
            .order
            .lock()
            .unwrap()
            .iter()
            .filter(|config| projects.has(&self.to_path(config)))
            .cloned()
            .collect();
        Some(order)
    }

    // orchestrator.go:798
    fn build_or_clean_order(&'static self, order: &[String]) -> OrchestratorResult {
        let build_options = &self.opts.command.build_options;
        if !build_options.clean.is_true() && build_options.verbose.is_true() {
            let mut project_list = String::new();
            for project in order {
                let _ = write!(project_list, "\r\n    * {}", self.relative_file_name(project));
            }
            self.create_builder_status_reporter(None)(new_compiler_diagnostic(
                &diagnostics::PROJECTS_IN_THIS_BUILD_COLON_0,
                &[&project_list],
            ));
        }
        let mut build_result = OrchestratorResult::default();
        if self.errors.lock().unwrap().is_empty() && self.api_mode.load(Ordering::SeqCst) {
            // API builds: one builder, run on the calling thread (a builder thread per build would leave its
            // never-freed thread arena behind), then report in Order() like the reporter below. Everything outside
            // the task regions goes to a region of this call (single thread: nothing else enters it).
            let _region = self.enter_api_region();
            build_result.statistics.projects = order.len();
            for config in order {
                let path = self.to_path(config);
                let task = self.get_task(&path);
                self.build_or_clean_project(task, &path);
            }
            for config in order {
                let path = self.to_path(config);
                let task = self.get_task(&path);
                task.report(self, &path, &mut build_result);
            }
        } else if tsrs_core::NO_THREADS && self.errors.lock().unwrap().is_empty() {
            // No threads (wasm32-wasip1): the API branch's order, without its region: build every project on the
            // calling thread, then report in Order().
            build_result.statistics.projects = order.len();
            for config in order {
                let path = self.to_path(config);
                let task = self.get_task(&path);
                self.build_or_clean_project(task, &path);
            }
            for config in order {
                let path = self.to_path(config);
                let task = self.get_task(&path);
                task.report(self, &path, &mut build_result);
            }
        } else if self.errors.lock().unwrap().is_empty() {
            build_result.statistics.projects = order.len();
            let schedule_order = if order.len() == self.order.lock().unwrap().len() {
                self.schedule_order()
            } else {
                self.compute_schedule_order(order)
            };
            // Builders pick up selected projects in dependency-depth order; reporting preserves the selected build order.
            // (Go: a reporter goroutine; here the calling thread, while rangeTasks runs the builders on their own threads).
            let mut aborted_report = false;
            std::thread::scope(|scope| {
                let builders = scope.spawn(|| {
                    self.range_tasks(&schedule_order, &|path, task| {
                        self.build_or_clean_project(task, path);
                    });
                });
                for config in order {
                    let path = self.to_path(config);
                    let task = self.get_task(&path);
                    if !task.built.wait(&self.aborted) {
                        aborted_report = true;
                        break;
                    }
                    task.report(self, &path, &mut build_result);
                }
                if let Err(panic) = builders.join() {
                    std::panic::resume_unwind(panic);
                }
            });
            assert!(!aborted_report, "build aborted");
        } else {
            // Circularity errors prevent any project from being built
            build_result.status = Some(ExitStatus::ProjectReferenceCycleOutputsSkipped);
            let report_diagnostic = self.create_diagnostic_reporter(None);
            for &err in self.errors.lock().unwrap().iter() {
                report_diagnostic(err);
            }
            build_result.errors = Some(self.errors.lock().unwrap().clone());
        }
        build_result.report(self);
        build_result
    }

    // orchestrator.go:925
    pub fn range_tasks(&'static self, order: &[String], f: &(dyn Fn(&Path, P<BuildTask>) + Sync)) {
        let mut num_routines = 4;
        if self.api_mode.load(Ordering::SeqCst) {
            // API builds: one task region is entered at a time. Concurrent tasks could each wait to enter the
            // other's region (`arena::enter_owner` for lazily filled data of a shared source file), so the API
            // runs one builder.
            num_routines = 1;
        } else if self.opts.command.compiler_options.single_threaded.is_true() {
            num_routines = 1;
        } else if let Some(builders) = self.opts.command.build_options.builders {
            num_routines = builders as usize;
        }

        let current_task_index = std::sync::atomic::AtomicUsize::new(0);
        let get_next_task = || -> Option<(Path, P<BuildTask>)> {
            let index = current_task_index.fetch_add(1, Ordering::SeqCst);
            let config = order.get(index)?;
            let path = self.to_path(config);
            let task = self.get_task(&path);
            Some((path, task))
        };
        let run_task = || {
            while let Some((path, task)) = get_next_task() {
                f(&path, task);
            }
        };

        // Go's goroutines grow their stacks; checking and binding recurse deeply (main.rs runs on 512 MB too).
        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..num_routines)
                .map(|i| {
                    std::thread::Builder::new()
                        .name(format!("builder-{i}"))
                        .stack_size(512 << 20)
                        .spawn_scoped(scope, || {
                            let result =
                                std::panic::catch_unwind(std::panic::AssertUnwindSafe(run_task));
                            if result.is_err() {
                                self.abort();
                            }
                            result
                        })
                        .unwrap()
                })
                .collect();
            for handle in handles {
                if let Err(panic) | Ok(Err(panic)) = handle.join() {
                    std::panic::resume_unwind(panic);
                }
            }
        });
    }

    // A builder panicked: release every waiter so the panic surfaces instead of a hang.
    fn abort(&self) {
        self.aborted.store(true, Ordering::SeqCst);
        #[expect(
            clippy::iter_over_hash_type,
            reason = "wakes every waiter; the order does not matter"
        )]
        for task in self.tasks.lock().unwrap().values() {
            task.done.wake();
            task.built.wake();
        }
    }

    // orchestrator.go:888
    fn build_or_clean_project(&'static self, task: P<BuildTask>, path: &Path) {
        let builder: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
        *task.result.lock().unwrap() = Some(TaskResult::new(builder));
        let report_status = self.create_builder_status_reporter(Some(task));
        let diagnostic_reporter = self.create_diagnostic_reporter(Some(task));
        {
            let mut result = task.result.lock().unwrap();
            let result = result.as_mut().unwrap();
            result.report_status = Some(report_status);
            result.diagnostic_reporter = Some(diagnostic_reporter);
        }
        if self.opts.command.build_options.clean.is_true() {
            task.clean_project(self, path);
        } else {
            task.get().build_project(self, path);
            if self.opts.testing.is_none() && self.frees_built_programs() {
                // The error summary outlives the program; replace file references with lightweight copies so the
                // program's loader regions can be dropped without invalidating the retained diagnostics.
                task.detach_diagnostic_files();
            }
        }
        if self.opts.testing.is_none() {
            // The program is only needed by Testing.OnProgram at report time; drop it now so a task
            // that has finished but is not yet reported does not keep its program alive.
            let (program, regions) = {
                let mut result = task.result.lock().unwrap();
                let result = result.as_mut().unwrap();
                (result.program.take(), result.program_regions.take())
            };
            if let (true, Some(program)) = (self.frees_built_programs(), program) {
                // Keep the free thread's overlap with building, but bound the completed programs it retains when
                // builders outpace it. Watch-mode frees still run in FIFO order (`Freer::run`).
                self.freer.run_bounded(4, move || task.free_built_program(program, regions));
            } else if let (true, Some(program)) = (self.api_mode.load(Ordering::SeqCst), program) {
                // API builds free each project's program once it is built (its arenas go with the task region when
                // the orchestrator is freed; this frees the heap side: program, checkers, processed data). It is a
                // full build that shares nothing, and nothing reads it after its task (statistics and diagnostics are
                // already taken).
                // SAFETY: see above; no checker handle of it is held.
                unsafe { tsrs_compiler::free_unshared_program(program.get_program()) };
            }
            // In watch mode the task keeps the program as the old state of its next build, which drops it
            // (`BuildTask::compile_and_emit`).
        }
        task.built.close();
    }

    // orchestrator.go:101
    fn resolve_build_info_file_name(&self, file_name: &str, build_info_dir: &str) -> String {
        if tsrs_incremental::is_build_info_file_name_default_library(file_name) {
            return tspath::combine_paths(
                CompilerHost::default_library_path(self.host()),
                &[file_name],
            );
        }
        tspath::get_normalized_absolute_path(file_name, build_info_dir)
    }

    // orchestrator.go:473 Watch
    fn watch(&'static self, build_started: std::time::SystemTime) {
        {
            let mut state = self.wm.lock();
            if self.opts.testing.is_none() {
                if self
                    .opts
                    .sys
                    .get_environment_variable("TS_WATCH_DEBUG")
                    .is_some_and(|v| !v.is_empty())
                {
                    let sys = self.opts.sys;
                    self.wm.set_debug_log(Arc::new(move |t: &str| sys.write(t)));
                }
                self.wm.ensure_default_backend(&mut state);
            }
            self.update_watch();
            self.reconcile(&mut state);
            self.reset_caches();
            self.free_unreachable_watch_files();
        }
        if self.opts.testing.is_none() {
            self.queue_files_changed_since(build_started);
            self.freer.after_cycle();
        }
        self.start_signature_job();
        if self.opts.testing.is_none() {
            self.wm.run_loop(|| {
                self.do_cycle();
                self.freer.after_cycle();
            });
        }
    }

    // tsrs-only: an update event for a parsed file whose modification time is still the one it was read at changes
    // nothing (as in watcher.rs `drop_unchanged_file_events`): chmod, and the event of a save that the first cycle
    // already took from `queue_files_changed_since` (a save just after the first report: the queued cycle read it,
    // and its event, which arrives ~12 ms later, ran a second cycle). The cache is shared by every task, so its time
    // only says that some task read the file then: the event is dropped only when every task that has the file has
    // the version the cache holds (`read_by_every_task`).
    fn drop_unchanged_file_events(
        &self,
        changed_paths: &mut FxHashMap<String, tsrs_fswatch::EventKind>,
    ) {
        let Some(cache) = &self.host().watch_source_files else {
            return;
        };
        if changed_paths.is_empty() {
            return;
        }
        // A rename in case only keeps the modification time, and the cache can hold both spellings: keep its events.
        let paths: FxHashSet<Path> = changed_paths.keys().map(|p| self.to_path(p)).collect();
        if paths.len() != changed_paths.len() {
            return;
        }
        // The cache keys files by their parse options, so a file can have a version for each.
        let versions: FxHashMap<String, Vec<(P<SourceFile>, Option<std::time::SystemTime>)>> = {
            let cache = cache.lock().unwrap();
            let mut versions: FxHashMap<String, Vec<_>> = FxHashMap::default();
            #[expect(
                clippy::iter_over_hash_type,
                reason = "builds a map; the order of each file's versions does not matter"
            )]
            for (opts, &(file, mod_time)) in cache.iter() {
                if changed_paths.contains_key(&opts.file_name) {
                    versions.entry(opts.file_name.clone()).or_default().push((file, mod_time));
                }
            }
            versions
        };
        if versions.is_empty() {
            return;
        }
        let order = self.order.lock().unwrap().clone();
        let tasks: Vec<P<BuildTask>> =
            order.iter().map(|config| self.get_task(&self.to_path(config))).collect();
        let before = changed_paths.len();
        changed_paths.retain(|path, kind| {
            *kind != tsrs_fswatch::EventKind::UPDATE
                || versions
                    .get(path)
                    .is_none_or(|versions| !self.read_by_every_task(path, versions, &tasks))
        });
        if changed_paths.len() != before {
            self.wm.debug(|| {
                format!(
                    "[watch] ignoring {} event(s) for files unchanged since they were read\n",
                    before - changed_paths.len()
                )
            });
        }
    }

    // Whether every task that has `file_name` has one of `versions`, the cache's, all read at the file's current
    // modification time. A task without a program in memory is up to date with what its build info lists: it must not
    // list the file.
    fn read_by_every_task(
        &self,
        file_name: &str,
        versions: &[(P<SourceFile>, Option<std::time::SystemTime>)],
        tasks: &[P<BuildTask>],
    ) -> bool {
        let mod_time = self.opts.sys.fs().stat(file_name).and_then(|s| s.mod_time());
        if mod_time.is_none() || versions.iter().any(|&(_, read)| read != mod_time) {
            return false;
        }
        let path = self.to_path(file_name);
        tasks.iter().all(|task| match task.watch_program() {
            Some(program) => program
                .get_source_file_by_path(&path)
                .is_none_or(|file| versions.iter().any(|&(version, _)| version == file)),
            None => !self.task_may_have_input(task, &path),
        })
    }

    // Whether `path` is a root file of the task or a file its build info lists; true when the task has neither.
    fn task_may_have_input(&self, task: &BuildTask, path: &Path) -> bool {
        let Some(resolved) = task.resolved_opt() else {
            return true;
        };
        if resolved.file_names().iter().any(|file| self.to_path(file) == *path) {
            return true;
        }
        let Some((build_info_path, build_info)) = task.build_info_snapshot() else {
            return true;
        };
        let build_info_dir = tspath::get_directory_path(build_info_path.as_str());
        build_info.file_names.iter().any(|file_name| {
            self.to_path(&self.resolve_build_info_file_name(file_name, &build_info_dir)) == *path
        })
    }

    // tsrs-only: the watches start after the first build, so a file saved while that build ran (after it was read)
    // has no event; Go misses the edit until the next save (tsgo too). A program file modified since the build
    // started is queued as a change, and the first cycle checks it again. One stat per source file, once.
    fn queue_files_changed_since(&self, build_started: std::time::SystemTime) {
        // Not the host's cached file system: the first cycle must not see these stats.
        let fs = self.opts.sys.fs();
        let order = self.order.lock().unwrap().clone();
        let tasks: Vec<P<BuildTask>> =
            order.iter().map(|config| self.get_task(&self.to_path(config))).collect();
        // The build wrote the outputs a downstream project reads (a reference's declarations): not edits.
        let outputs: FxHashSet<Path> = tasks
            .iter()
            .filter_map(|task| task.resolved_opt())
            .flat_map(|resolved| resolved.get_output_file_names())
            .map(|f| self.to_path(&f))
            .collect();
        let mut changed: Vec<String> = Vec::new();
        for task in &tasks {
            let Some(program) = task.watch_program() else {
                continue;
            };
            changed.extend(
                program
                    .get_source_files()
                    .iter()
                    .filter(|file| !outputs.contains(file.path()))
                    .map(|file| file.file_name())
                    .filter(|file_name| {
                        fs.stat(file_name)
                            .and_then(|s| s.mod_time())
                            .is_some_and(|m| m > build_started)
                    })
                    .map(str::to_string),
            );
        }
        if !changed.is_empty() {
            changed.sort();
            changed.dedup();
            self.wm.debug(|| {
                format!(
                    "[watch] {} file(s) changed during the build: {}\n",
                    changed.len(),
                    changed.join(", ")
                )
            });
            self.wm.queue_changes(changed);
        }
    }

    fn reconcile(&self, state: &mut watchmanager::WatchState) {
        let desired_dirs = self.compute_desired_watches();
        if let Err(err) = self.wm.reconcile_watches(state, &desired_dirs) {
            self.opts.sys.write(&format!("{}\n", err.message()));
            // Mark overflow so the next event triggers a full rebuild
            self.wm.force_overflow();
        }
    }

    // orchestrator.go:498
    fn update_watch(&self) {
        let old_cache = std::mem::take(&mut *self.host().m_times.write().unwrap());
        let order = self.order.lock().unwrap().clone();
        for config in order {
            self.get_task(&self.to_path(&config)).update_watch(self, &old_cache);
        }
    }

    // orchestrator.go:816 DoCycle
    fn do_cycle(&'static self) {
        let mut state = self.wm.lock();
        let (mut changed_paths, overflow) = self.wm.drain_events();
        // tsrs-only: whatever the cached file system holds predates the events (`get_watch_source_file` reuses a file
        // whose modification time it finds unchanged).
        self.host().cached_fs.clear_cache();
        self.drop_unchanged_file_events(&mut changed_paths);
        if changed_paths.is_empty() && !overflow {
            self.wm.debug(|| "[watch] DoCycle: no events, skipping\n".to_string());
            return;
        }
        self.stop_signature_job();
        self.build_changes(&mut state, &changed_paths, overflow);
        self.start_signature_job();
    }

    fn build_changes(
        &'static self,
        state: &mut watchmanager::WatchState,
        changed_paths: &FxHashMap<String, tsrs_fswatch::EventKind>,
        overflow: bool,
    ) {
        self.invalidate_realpaths(changed_paths, overflow);
        let mut needs_config_update = false;
        let mut needs_update = false;
        if overflow {
            // Overflow: reset all tasks to force a full rebuild.
            let order = self.order.lock().unwrap().clone();
            for config in order {
                let path = self.to_path(&config);
                let task = self.get_task(&path);
                task.reset_config(self, &path);
                task.set_watch_changes(None);
                task.built.reset();
                task.done.reset();
            }
            needs_config_update = true;
            needs_update = true;
        } else {
            self.check_tasks_for_event_changes(
                state,
                changed_paths,
                &mut needs_config_update,
                &mut needs_update,
            );
        }
        if !needs_update {
            self.reset_caches();
            return;
        }
        if let Some(watch_status_reporter) = &self.watch_status_reporter {
            watch_status_reporter(new_compiler_diagnostic(
                &diagnostics::FILE_CHANGE_DETECTED_STARTING_INCREMENTAL_COMPILATION,
                &[],
            ));
        }
        if needs_config_update {
            self.generate_graph_reusing_old_tasks();
        }
        let order = self.order.lock().unwrap().clone();
        self.build_or_clean_order(&order);
        self.update_watch();
        self.reconcile(state);
        self.reset_caches();
        self.free_unreachable_watch_files();
    }

    // tsrs-only: `--watch`'s signature job (watcher.rs `start_signature_job`) over each task's incremental program,
    // which the task keeps for its next build. Without it, the first edit of a file rechecks everything in its project
    // that imports it. The next cycle cancels the job and waits for the chunk in progress before any task builds, so
    // the programs the job reads stay unchanged and alive while it runs.
    fn start_signature_job(&'static self) {
        let order = self.order.lock().unwrap().clone();
        let programs: Vec<P<tsrs_incremental::Program>> = order
            .iter()
            .filter_map(|config| self.get_task(&self.to_path(config)).watch_incremental_program())
            .collect();
        if let Some(testing) = self.opts.testing {
            if testing.run_signature_job() {
                for program in programs {
                    program.compute_placeholder_signatures(
                        &tsrs_compiler::Context::default(),
                        crate::watcher::SIGNATURE_JOB_CHUNK,
                    );
                }
            }
            return;
        }
        if programs.is_empty() {
            return;
        }
        let (ctx, cancel) = tsrs_compiler::Context::default().with_cancel();
        let job = std::thread::Builder::new()
            .name("tsrs-signatures".to_string())
            .spawn(move || {
                let start = std::time::Instant::now();
                let mut stored = 0;
                for program in programs {
                    stored += program
                        .compute_placeholder_signatures(&ctx, crate::watcher::SIGNATURE_JOB_CHUNK);
                }
                let state = if ctx.err().is_some() { "canceled" } else { "done" };
                self.wm.debug(|| {
                    format!(
                        "[watch] stored {stored} declaration signature(s) in {:?} ({state})\n",
                        start.elapsed()
                    )
                });
            })
            .expect("failed to spawn a thread");
        *self.signature_job.lock().unwrap() = Some((cancel, job));
    }

    fn stop_signature_job(&self) {
        let job = self.signature_job.lock().unwrap().take();
        if let Some((cancel, job)) = job {
            cancel.call();
            job.join().expect("the signature computation panicked");
        }
    }

    // tsrs-only (watchmem.rs): after a cycle's builds, frees the regions of the parsed files that neither the host's
    // cache nor a task's program refers to, once the programs the cycle replaced are freed (queued before this).
    fn free_unreachable_watch_files(&self) {
        let host = self.host();
        let Some(cache) = &host.watch_source_files else {
            return;
        };
        let mut live: rustc_hash::FxHashSet<usize> =
            cache.lock().unwrap().values().map(|(f, _)| f.addr()).collect();
        let tasks: Vec<P<BuildTask>> = self.tasks.lock().unwrap().values().copied().collect();
        for task in tasks {
            task.add_live_watch_files(&mut live);
        }
        let dead = host.watch_file_regions.take_unreachable(&live);
        self.freer.run(move || drop(dead));
    }

    // orchestrator.go:515
    fn check_tasks_for_event_changes(
        &'static self,
        state: &watchmanager::WatchState,
        changed_paths: &FxHashMap<String, tsrs_fswatch::EventKind>,
        needs_config_update: &mut bool,
        needs_update: &mut bool,
    ) {
        let normalized_paths: FxHashMap<Path, tsrs_fswatch::EventKind> =
            changed_paths.iter().map(|(p, &k)| (self.to_path(p), k)).collect();
        let order = self.order.lock().unwrap().clone();
        for config in &order {
            let path = self.to_path(config);
            let task = self.get_task(&path);
            task.built_program.store(false, std::sync::atomic::Ordering::SeqCst);
            task.set_watch_changes(None);
            if self.task_config_changed(&task, &normalized_paths) {
                task.reset_config(self, &path);
                *needs_config_update = true;
                *needs_update = true;
                continue;
            }
            let Some(resolved) = task.resolved_opt() else {
                continue;
            };
            if self.task_inputs_changed(&task, &resolved, &normalized_paths) {
                task.reset_status();
                task.set_watch_changes(Some(
                    normalized_paths.iter().map(|(p, &k)| (p.clone(), k)).collect(),
                ));
                *needs_update = true;
            }
            task.built.reset();
            task.done.reset();

            // tsrs-only: the wildcard file set changes only when a file it may match appears or goes, or a directory
            // does; an edit of a file already in it cannot change it, so the reload (a walk of the project, 60 ms on
            // vscode, before the cycle is even reported) is skipped. Go reloads every cycle.
            if !self.file_names_may_change(&resolved, changed_paths) {
                continue;
            }
            let new_config = P::new(
                resolved.reload_file_names_of_parsed_command_line(CompilerHost::fs(self.host())),
            );
            if resolved.file_names() != new_config.file_names() {
                self.host().resolved_references.store(path.clone(), Some(new_config));
                *task.resolved.lock().unwrap() = Some(new_config);
                task.set_watch_changes(None);
                task.reset_status();
                *needs_update = true;
            }
        }
        if *needs_update {
            return;
        }
        // A new or removed directory under a watched one can change wildcard matches.
        let fs = CompilerHost::fs(self.host());
        let mut keys: Vec<&String> = changed_paths.keys().collect();
        keys.sort();
        if keys.into_iter().any(|p| {
            fs.directory_exists(p)
                && WatchManager::is_path_under_watch(state, p, &self.compare_paths_options)
                && self.directory_change_matters(p)
        }) {
            for config in &order {
                let task = self.get_task(&self.to_path(config));
                task.reset_status();
                task.built.reset();
                task.done.reset();
            }
            *needs_update = true;
        }
    }

    // tsrs-only (see `check_tasks_for_event_changes`).
    fn file_names_may_change(
        &self,
        resolved: &tsrs_tsoptions::ParsedCommandLine,
        changed_paths: &FxHashMap<String, tsrs_fswatch::EventKind>,
    ) -> bool {
        if resolved.config_file.is_none()
            || resolved
                .wildcard_directories()
                .is_none_or(tsrs_core::collections::OrderedMap::is_empty)
        {
            return false;
        }
        let fs = CompilerHost::fs(self.host());
        let names = resolved.file_names_by_path();
        let cwd = self.opts.sys.get_current_directory();
        changed_paths.keys().any(|p| {
            let listed = names.get(&self.to_path(p));
            let is_name = listed.is_some();
            if fs.file_exists(p) {
                // An edit of a listed file keeps the set (unless it was renamed in case only: the path is the same,
                // the spelling is not); a new file changes it if the wildcards may match it.
                match listed {
                    Some(name) => tspath::get_normalized_absolute_path(name, cwd) != *p,
                    None => resolved.possibly_matches_file_name(p),
                }
            } else {
                // A listed file went, or a directory came or went (a directory exists, or it is gone like a file).
                is_name
                    || fs.directory_exists(p)
                    || resolved.possibly_matches_directory_name(&self.to_path(p))
                    || resolved.possibly_matches_file_name(p)
            }
        })
    }

    // The tsconfig, an extended config, or a content mapper's package manifest changed.
    fn task_config_changed(
        &self,
        task: &BuildTask,
        changed: &FxHashMap<Path, tsrs_fswatch::EventKind>,
    ) -> bool {
        if changed.contains_key(&self.to_path(&task.config)) {
            return true;
        }
        let Some(resolved) = task.resolved_opt() else {
            return false;
        };
        resolved
            .extended_source_files()
            .iter()
            .any(|file| changed.contains_key(&self.to_path(file)))
            || resolved
                .content_mappers()
                .iter()
                .filter(|m| !m.package_directory.is_empty() && m.contribution_id.is_empty())
                .any(|m| {
                    changed.contains_key(
                        &self.to_path(&tspath::combine_paths(
                            &m.package_directory,
                            &["package.json"],
                        )),
                    )
                })
    }

    // A root file, content mapper input, buildinfo-recorded dependency, or package.json lookup changed.
    fn task_inputs_changed(
        &'static self,
        task: &BuildTask,
        resolved: &tsrs_tsoptions::ParsedCommandLine,
        changed: &FxHashMap<Path, tsrs_fswatch::EventKind>,
    ) -> bool {
        if let Some(project) = task.content_mapper_project_opt() {
            match project.watched_files() {
                Err(err) => {
                    task.set_content_mapper_project_err(err);
                    return true;
                }
                Ok(watched_files) => {
                    if watched_files.iter().any(|f| changed.contains_key(&self.to_path(f))) {
                        task.refresh_content_mapper_project(self);
                        return true;
                    }
                }
            }
        }
        let mut roots: Set<Path> = Set::default();
        for file in resolved.file_names() {
            let fp = self.to_path(file);
            if changed.contains_key(&fp) {
                return true;
            }
            roots.add(fp);
        }
        if let Some((bi_path, build_info)) = task.build_info_snapshot() {
            let build_info_dir = tspath::get_directory_path(bi_path.as_str());
            let dependency_changed = build_info.file_names.iter().any(|file_name| {
                let fp =
                    self.to_path(&self.resolve_build_info_file_name(file_name, &build_info_dir));
                !roots.has(&fp) && changed.contains_key(&fp)
            });
            if dependency_changed
                || build_info
                    .get_package_jsons(&build_info_dir)
                    .iter()
                    .any(|p| self.package_json_lookup_changed(p, changed))
                || build_info
                    .get_missing_package_jsons(&build_info_dir)
                    .iter()
                    .any(|p| self.package_json_lookup_changed(p, changed))
            {
                return true;
            }
        }
        task.package_jsons().iter().any(|p| self.package_json_lookup_changed(p, changed))
    }

    // orchestrator.go:673
    fn package_json_lookup_changed(
        &self,
        package_json: &str,
        changed_paths: &FxHashMap<Path, tsrs_fswatch::EventKind>,
    ) -> bool {
        let package_json_path = self.to_path(package_json);
        if changed_paths.contains_key(&package_json_path) {
            return true;
        }
        changed_paths.iter().any(|(changed, &kind)| {
            kind == tsrs_fswatch::EventKind::DELETE
                && tspath::contains_path(
                    changed.as_str(),
                    package_json_path.as_str(),
                    &self.compare_paths_options,
                )
        })
    }

    // orchestrator.go:686
    fn compute_desired_watches(&self) -> FxHashMap<String, bool> {
        let mut desired_dirs = DirWatchSet::new(&self.compare_paths_options);
        let fs = CompilerHost::fs(self.host());
        let mut symlink_checked: FxHashSet<String> = FxHashSet::default();
        let order = self.order.lock().unwrap().clone();
        for config in order {
            let task = self.get_task(&self.to_path(&config));
            desired_dirs.set(&fs.realpath(&tspath::get_directory_path(&task.config)), false);
            let Some(resolved) = task.resolved_opt() else {
                continue;
            };
            for cfg_path in resolved.extended_source_files() {
                desired_dirs
                    .set(&tspath::get_directory_path(&fs.realpath(cfg_path.as_str())), false);
            }
            if let Some(wildcards) = resolved.wildcard_directories() {
                for (dir, &recursive) in wildcards {
                    desired_dirs.set(&fs.realpath(dir), recursive);
                }
            }
            for file_name in resolved.file_names() {
                let abs_path = tspath::get_normalized_absolute_path(
                    file_name,
                    self.opts.sys.get_current_directory(),
                );
                add_program_file_watch_dir(
                    &mut desired_dirs,
                    &tspath::get_directory_path(&abs_path),
                    fs,
                    &mut symlink_checked,
                );
                for mapper in resolved.content_mappers() {
                    if mapper.package_directory.is_empty() || !mapper.contribution_id.is_empty() {
                        continue;
                    }
                    let manifest_path =
                        tspath::combine_paths(&mapper.package_directory, &["package.json"]);
                    add_watch_dir(&mut desired_dirs, &tspath::get_directory_path(&manifest_path));
                }
            }
            if let Some(project) = task.content_mapper_project_opt() {
                match project.watched_files() {
                    Err(err) => task.set_content_mapper_project_err(err),
                    Ok(watched_files) => {
                        for file_name in watched_files {
                            add_watch_dir(
                                &mut desired_dirs,
                                &tspath::get_directory_path(&fs.realpath(&file_name)),
                            );
                        }
                    }
                }
            }
            // Non-root dependency directories from the buildinfo (e.g. node_modules .d.ts files).
            if let Some((bi_path, build_info)) = task.build_info_snapshot() {
                let build_info_dir = tspath::get_directory_path(bi_path.as_str());
                let roots: Set<Path> = tsrs_core::collections::new_set_from_items(
                    resolved.file_names().iter().map(|f| self.to_path(f)),
                );
                for file_name in &build_info.file_names {
                    let abs_path = self.cached_realpath(
                        &self.resolve_build_info_file_name(file_name, &build_info_dir),
                    );
                    if roots.has(&self.to_path(&abs_path)) {
                        continue;
                    }
                    add_program_file_watch_dir(
                        &mut desired_dirs,
                        &tspath::get_directory_path(&abs_path),
                        fs,
                        &mut symlink_checked,
                    );
                }
                for package_json in build_info.get_package_jsons(&build_info_dir) {
                    add_package_json_watch_dirs(&mut desired_dirs, &package_json);
                }
                for package_json in build_info.get_missing_package_jsons(&build_info_dir) {
                    add_package_json_watch_dirs(&mut desired_dirs, &package_json);
                }
            }
            for package_json in task.package_jsons() {
                add_package_json_watch_dirs(&mut desired_dirs, &package_json);
            }
        }
        let dirs = desired_dirs.dirs();
        // tsrs-only (`directory_change_matters`): the wanted directories and their ancestors, before a missing one is
        // replaced by its nearest existing ancestor.
        let mut wanted: FxHashSet<Path> = FxHashSet::default();
        #[expect(clippy::iter_over_hash_type, reason = "builds a set; the order does not matter")]
        for dir in dirs.keys() {
            let mut p = self.to_path(dir);
            while wanted.insert(p.clone()) {
                let parent = p.get_directory_path();
                if parent == p {
                    break;
                }
                p = parent;
            }
        }
        *self.wanted_watch_dirs.lock().unwrap() = wanted;
        self.wm.resolve_desired_dirs(&dirs)
    }

    // tsrs-only: computing the watches took one realpath per file the build info lists, every cycle (10k files, 290 ms
    // of a 400 ms vscode cycle). A real path changes only when a symlink on it does, and such a change is an event
    // for a symlink, a directory or a removed path, which clears the cache (`invalidate_realpaths`).
    fn cached_realpath(&self, path: &str) -> String {
        if let Some(real) = self.realpaths.lock().unwrap().get(path) {
            return real.clone();
        }
        let real = CompilerHost::fs(self.host()).realpath(path);
        self.realpaths.lock().unwrap().insert(path.to_string(), real.clone());
        real
    }

    fn invalidate_realpaths(
        &self,
        changed_paths: &FxHashMap<String, tsrs_fswatch::EventKind>,
        overflow: bool,
    ) {
        let fs = CompilerHost::fs(self.host());
        let structural = overflow
            || changed_paths.keys().any(|p| {
                std::fs::symlink_metadata(p).map_or(true, |m| m.file_type().is_symlink())
                    || fs.directory_exists(p)
            });
        if structural {
            self.realpaths.lock().unwrap().clear();
        }
    }

    // tsrs-only: a directory that appeared or changed under a watched one matters when a project's wildcards may
    // match it, when it is (an ancestor of) a directory the build wants watched, or when it is or lies in a
    // node_modules (an installed package: the build info does not list the module lookups that found nothing). Go counts any directory under any watch and rebuilds every project, so a directory made beside the
    // projects (whose parent is watched for a lookup) rebuilt them all.
    fn directory_change_matters(&self, dir: &str) -> bool {
        let p = self.to_path(dir);
        if watchmanager::is_in_node_modules(&p)
            || self.wanted_watch_dirs.lock().unwrap().contains(&p)
        {
            return true;
        }
        let order = self.order.lock().unwrap().clone();
        order.iter().any(|config| {
            self.get_task(&self.to_path(config))
                .resolved_opt()
                .is_some_and(|resolved| resolved.possibly_matches_directory_name(&p))
        })
    }

    // orchestrator.go:906
    fn get_writer(&self, task: Option<P<BuildTask>>) -> Writer<'static> {
        let sys = self.opts.sys;
        match task {
            None => std::sync::Arc::new(move |t: &str| sys.write(t)),
            Some(task) => {
                let builder = {
                    let result = task.result.lock().unwrap();
                    Arc::clone(&result.as_ref().unwrap().builder)
                };
                std::sync::Arc::new(move |t: &str| builder.lock().unwrap().push_str(t))
            }
        }
    }

    // orchestrator.go:913
    fn create_builder_status_reporter(
        &self,
        task: Option<P<BuildTask>>,
    ) -> DiagnosticReporter<'static> {
        create_builder_status_reporter(
            self.opts.sys,
            self.get_writer(task),
            &self.opts.command.compiler_options,
            self.opts.testing,
        )
    }

    // orchestrator.go:917
    fn create_diagnostic_reporter(
        &self,
        task: Option<P<BuildTask>>,
    ) -> DiagnosticReporter<'static> {
        create_diagnostic_reporter_with_writer(
            self.opts.sys,
            self.get_writer(task),
            Some(&self.opts.command.compiler_options),
        )
    }
}

// Go's `tsc.Watcher` for the test harness.
struct OrchestratorWatcher(&'static Orchestrator);

impl tsc::Watcher for OrchestratorWatcher {
    fn do_cycle(&mut self) {
        self.0.do_cycle();
    }
}

// orchestrator.go:774
fn add_watch_dir(desired_dirs: &mut DirWatchSet, dir: &str) {
    if !desired_dirs.covered(dir) && watchmanager::can_watch_directory(dir) {
        desired_dirs.set(dir, false);
    }
}

// orchestrator.go:780: a program file's directory is watched at any depth; add_watch_dir's guard keeps lookup
// locations from watching something as broad as / or /home.
//
// tsrs-only: a directory covered only by a recursive watch above it, but reached through a symlink, gets a watch of its
// own (`symlink_checked` holds the directories already checked); see watcher.rs `compute_desired_watches`.
fn add_program_file_watch_dir(
    desired_dirs: &mut DirWatchSet,
    dir: &str,
    fs: &dyn tsrs_vfs::FS,
    symlink_checked: &mut FxHashSet<String>,
) {
    if !desired_dirs.covered(dir)
        || (!desired_dirs.has(dir)
            && symlink_checked.insert(dir.to_string())
            && fs.realpath(dir) != dir)
    {
        desired_dirs.set(dir, false);
    }
}

// orchestrator.go:786: a package.json's directory, plus its ancestors up to the enclosing node_modules' parent.
fn add_package_json_watch_dirs(desired_dirs: &mut DirWatchSet, package_json: &str) {
    let dir = tspath::get_directory_path(package_json);
    let mut dirs = vec![dir.clone()];
    let mut current = dir.clone();
    loop {
        let parent = tspath::get_directory_path(&current);
        if parent.is_empty() || parent == current {
            add_watch_dir(desired_dirs, &dir);
            return;
        }
        dirs.push(parent.clone());
        if tspath::get_base_file_name(&parent) == "node_modules" {
            let grandparent = tspath::get_directory_path(&parent);
            if !grandparent.is_empty() && grandparent != parent {
                dirs.push(grandparent);
            }
            break;
        }
        current = parent;
    }
    for dir in &dirs {
        add_watch_dir(desired_dirs, dir);
    }
}

/// Frees an orchestrator created with `enable_api_mode` and everything its builds allocated.
///
/// # Safety
/// `o` came from `new_orchestrator`, `enable_api_mode` was called before its first build, no build or clean
/// is running, and nothing obtained from it (results, programs, diagnostics) is used afterwards.
pub unsafe fn free_api_orchestrator(o: &'static Orchestrator) {
    // The lock may be poisoned if the build unwound (`CliOrchestrator::build`); the list itself is intact.
    let regions =
        std::mem::take(&mut *o.regions.lock().unwrap_or_else(std::sync::PoisonError::into_inner));
    if std::env::var_os("TSRS_REGION_LOG").is_some() {
        eprintln!(
            "regions: api orchestrator freed ({} regions, {} KiB)",
            regions.len(),
            regions.iter().map(tsrs_core::arena::Region::allocated_bytes).sum::<usize>() >> 10
        );
    }
    let h = o.host();
    // Order: the host and orchestrator are dropped before the regions. Their destructors only free heap containers
    // (maps of `P<..>` pointers, Arcs); none dereferences arena memory, which is still alive here. Keep it that way:
    // a destructor that reads region memory must run before `drop(regions)`.
    // SAFETY: `new_orchestrator` leaked both from `Box`es, and nothing uses them afterwards (this function's contract).
    drop(unsafe { Box::from_raw(std::ptr::from_ref::<HostImpl>(h).cast_mut()) });
    // SAFETY: as above.
    drop(unsafe { Box::from_raw(std::ptr::from_ref::<Orchestrator>(o).cast_mut()) });
    drop(regions);
}

// orchestrator.go:921
pub fn new_orchestrator(opts: Options) -> &'static Orchestrator {
    // Several projects build at once; each project's pool keeps Go's checker count.
    tsrs_compiler::use_go_default_checker_count();
    let sys = opts.sys;
    let compare_paths_options = ComparePathsOptions {
        current_directory: sys.get_current_directory().to_string(),
        use_case_sensitive_file_names: sys.fs().use_case_sensitive_file_names(),
    };
    let error_summary_reporter = create_report_error_summary(sys, &opts.command.compiler_options);
    let watch = opts.command.compiler_options.watch.is_true();
    let watch_status_reporter = watch.then(|| {
        tsc::create_watch_status_reporter(sys, &opts.command.compiler_options, opts.testing)
    });
    if watch {
        tsrs_compiler::bind_in_file_regions();
    }
    let freer = crate::watchmem::Freer::new(opts.testing.is_some());
    let fs = sys.fs();
    let wm = WatchManager::new(
        Arc::new(move |t: &str| sys.write(t)),
        Box::new(move |d: &str| fs.directory_exists(d)),
    );
    if watch && let Some(backend) = opts.testing.and_then(tsc::CommandLineTesting::watch_backend) {
        WatchManager::set_backend(&mut wm.lock(), backend);
    }
    let orchestrator: &'static Orchestrator = Box::leak(Box::new(Orchestrator {
        opts,
        compare_paths_options,
        host: OnceLock::new(),
        tasks: Mutex::new(FxHashMap::default()),
        order: Mutex::new(Vec::new()),
        errors: Mutex::new(Vec::new()),
        graph_generated: AtomicBool::new(false),
        aborted: AtomicBool::new(false),
        error_summary_reporter,
        watch_status_reporter,
        wm,
        freer,
        signature_job: Mutex::new(None),
        schedule_order: Mutex::new(Vec::new()),
        wanted_watch_dirs: Mutex::new(FxHashSet::default()),
        realpaths: Mutex::new(FxHashMap::default()),
        regions: Mutex::new(Vec::new()),
        api_mode: AtomicBool::new(false),
        api_clean_exists: Mutex::new(FxHashMap::default()),
        content_mapper_host: Mutex::new(None),
    }));
    let cached_fs = Arc::new(tsrs_vfs::cachedvfs::from(sys.fs()));
    let compiler_host: Arc<dyn CompilerHost> = tsrs_compiler::new_compiler_host(
        sys.get_current_directory(),
        Arc::<tsrs_vfs::cachedvfs::FS<_>>::clone(&cached_fs),
        sys.default_library_path(),
        None,
        None,
        None,
    );
    let h: &'static HostImpl = Box::leak(Box::new(HostImpl {
        orchestrator: OnceLock::new(),
        host: compiler_host,
        cached_fs,
        extended_config_cache: Mutex::new(Arc::new(tsc::ExtendedConfigCache::default())),
        source_files: ParseCache::default(),
        config_times: Mutex::new(FxHashMap::default()),
        resolved_references: ParseCache::default(),
        m_times: std::sync::RwLock::new(Arc::default()),
        watch_source_files: watch.then(|| Mutex::new(FxHashMap::default())),
        watch_file_regions: crate::watchmem::FileRegions::default(),
    }));
    let _ = h.orchestrator.set(orchestrator);
    let _ = orchestrator.host.set(h);
    orchestrator
}
