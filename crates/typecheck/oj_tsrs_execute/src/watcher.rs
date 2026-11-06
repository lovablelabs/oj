// Port of Go `execute/watcher.go`: `tsc --watch` for one project. Each cycle drains the file system events, decides
// whether they matter to the program (the paths the last build read, wildcard matches, watched directories), and
// rebuilds: in place when exactly one already-parsed file changed (`Program::reuse_program`), otherwise with a full
// `new_program` over a source file cache keyed by modification time. Either way the incremental program carries the
// old one's diagnostics and signatures, so only affected files are checked again.

use std::sync::Arc;
use std::time::SystemTime;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{SourceFile, SourceFileParseOptions, new_compiler_diagnostic};
use tsrs_compiler::{CompilerHost, Program, ProgramOptions, new_compiler_host, new_program};
use tsrs_contentmapper as contentmapper;
use tsrs_core::arena::Region;
use tsrs_core::collections::SyncMap;
use tsrs_core::tspath::{self, ComparePathsOptions, Path};
use tsrs_core::{CompilerOptions, P};
use tsrs_diagnostics as diagnostics;
use tsrs_fswatch::EventKind;
use tsrs_tsoptions::{self as tsoptions, CompilerOptionsValue, Mapper, ParsedCommandLine};
use tsrs_vfs::{FS, cachedvfs, trackingvfs};

use crate::tsc::{
    self, CommandLineTesting, DiagnosticReporter, DiagnosticsReporter, EmitInput,
    ExtendedConfigCache, System,
};
use crate::watchmanager::{self, DirWatchSet, WatchManager, WatchState};
use crate::watchmem::{FileRegions, Freer};

#[derive(Clone, Copy)]
struct CachedSourceFile {
    file: P<SourceFile>,
    mod_time: Option<SystemTime>,
}

type SourceFileCache = Arc<SyncMap<Path, CachedSourceFile>>;

/// Go `watchCompilerHost`: the compiler host, with parsed files reused while their modification time is unchanged.
struct WatchCompilerHost {
    inner: Arc<dyn CompilerHost>,
    cache: SourceFileCache,
    // tsrs-only: each parsed file gets a region of its own (watchmem.rs).
    regions: Arc<FileRegions>,
}

impl CompilerHost for WatchCompilerHost {
    fn fs(&self) -> &dyn FS {
        self.inner.fs()
    }
    fn default_library_path(&self) -> &str {
        self.inner.default_library_path()
    }
    fn get_current_directory(&self) -> &str {
        self.inner.get_current_directory()
    }
    fn trace(&self, msg: &'static tsrs_diagnostics::Message, args: &[&dyn std::fmt::Display]) {
        self.inner.trace(msg, args);
    }
    fn get_source_file(&self, opts: SourceFileParseOptions) -> Option<P<SourceFile>> {
        let info = self.inner.fs().stat(&opts.file_name);
        if let Some(cached) = self.cache.load(&opts.path)
            && let Some(info) = &info
            && info.mod_time() == cached.mod_time
        {
            return Some(cached.file);
        }
        let path = opts.path.clone();
        let size = info.as_ref().map_or(0, |info| usize::try_from(info.size()).unwrap_or(0));
        let file = self.regions.parse(size, || self.inner.get_source_file(opts));
        match file {
            Some(file) => {
                if let Some(info) = info {
                    self.cache.store(path, CachedSourceFile { file, mod_time: info.mod_time() });
                }
            }
            None => self.cache.delete(&path),
        }
        file
    }
    fn get_content_mapped_source_files(
        &self,
        parse_options: SourceFileParseOptions,
        mapper: &Mapper,
    ) -> Result<contentmapper::SourceFiles, contentmapper::Error> {
        // Not cached: each full build transforms the file again. Its region lives while a snapshot's kept diagnostics
        // refer to a version of it, as for the other files (`live_files`).
        let size = self
            .inner
            .fs()
            .stat(&parse_options.file_name)
            .map_or(0, |info| usize::try_from(info.size()).unwrap_or(0));
        self.regions.parse_content_mapped(size, || {
            self.inner.get_content_mapped_source_files(parse_options, mapper)
        })
    }
    fn content_mapper_project(&self) -> Option<Arc<dyn contentmapper::Project>> {
        self.inner.content_mapper_project()
    }
    fn get_resolved_project_reference(
        &self,
        file_name: &str,
        path: Path,
    ) -> Option<P<ParsedCommandLine>> {
        self.inner.get_resolved_project_reference(file_name, path)
    }
}

// Files per declaration emit of `start_signature_job`: what a new cycle may wait for once it cancels the job.
pub(crate) const SIGNATURE_JOB_CHUNK: usize = 256;

pub struct Watcher {
    sys: &'static dyn System,
    // tsrs-only: what the programs since the last full build share (`install_program`).
    shared_program_data: Option<tsrs_compiler::SharedProgramData>,
    // Frees what the last `install_program` replaced, once the cycle has reported (`free_replaced_program`).
    free_replaced: Option<Box<dyn FnOnce() + Send>>,
    // The background computation of the program's placeholder signatures (`start_signature_job`).
    signature_job: Option<(tsrs_core::context::CancelFunc, std::thread::JoinHandle<()>)>,
    config_file_name: String,
    config: P<ParsedCommandLine>,
    compiler_options_from_command_line: P<CompilerOptions>,
    command_line_raw: Option<CompilerOptionsValue>,
    report_diagnostic: DiagnosticReporter<'static>,
    report_error_summary: DiagnosticsReporter<'static>,
    report_watch_status: DiagnosticReporter<'static>,
    testing: Option<&'static dyn CommandLineTesting>,

    // Created once per watch session (when enabled) and reused across cycles.
    content_mapper_host: Option<Arc<dyn contentmapper::Host>>,
    content_mapper_project: Option<Arc<dyn contentmapper::Project>>,

    program: Option<P<tsrs_incremental::Program>>,
    extended_config_cache: Arc<ExtendedConfigCache>,
    config_modified: bool,
    config_has_errors: bool,
    config_file_paths: Vec<String>,

    source_file_cache: SourceFileCache,
    // tsrs-only (watchmem.rs): the regions of the parsed files, the files of the last full build (the programs
    // updated from it share its processed-file data, which refers to them), and the thread that frees what the
    // session no longer uses.
    file_regions: Arc<FileRegions>,
    full_build_files: Vec<P<SourceFile>>,
    // What the file loader allocated for the last full build (resolutions, include reasons, processed-file data),
    // freed after the data the programs updated from it share (`install_program`).
    full_build_regions: Option<Arc<tsrs_compiler::ProgramRegions>>,
    // The diagnostics the current snapshot keeps from the cycles before (`install_program`).
    snapshot_diagnostics: Option<Region>,
    freer: Freer,

    wm: Arc<WatchManager>,
    seen_files: FxHashSet<Path>, // all build dependencies (for event filtering)
    // tsrs-only (`directory_change_matters`): the directories that contain a seen file, at any depth.
    seen_dirs: FxHashSet<Path>,
    // tsrs-only (`drop_own_output_events`): what the config's builds write.
    own_outputs: FxHashSet<Path>,
    config_mtimes: FxHashMap<String, Option<SystemTime>>,
    watch_set_dirty: bool,
    // A reason that requires a full `new_program` rebuild (an overflow, a mid-cycle watch failure, a newly appeared
    // project file, a changed non-source dependency). Unlike `watch_set_dirty` it is kept until a full rebuild runs, so
    // the single-file fast path cannot reuse a stale program.
    force_full_rebuild: bool,
    program_ready: bool,

    // Test-only observability of which build path was taken.
    fast_path_builds: usize,
    full_builds: usize,
}

impl tsc::Watcher for Watcher {
    fn do_cycle(&mut self) {
        Watcher::do_cycle(self);
    }
}

/// Go `createWatcher` + `start`. Without the test harness this blocks, running cycles until the process ends.
pub(crate) fn run_watcher(
    sys: &'static dyn System,
    config: P<ParsedCommandLine>,
    compiler_options_from_command_line: P<CompilerOptions>,
    command_line_raw: Option<CompilerOptionsValue>,
    report_diagnostic: DiagnosticReporter<'static>,
    report_error_summary: DiagnosticsReporter<'static>,
    testing: Option<&'static dyn CommandLineTesting>,
    backend: Option<Arc<dyn watchmanager::WatchBackend>>,
) -> Option<Box<Watcher>> {
    let writer: tsc::SyncWriter = Arc::new(move |t: &str| sys.write(t));
    let fs = sys.fs();
    let wm =
        WatchManager::new(Arc::clone(&writer), Box::new(move |d: &str| fs.directory_exists(d)));
    if testing.is_none()
        && sys.get_environment_variable("TS_WATCH_DEBUG").is_some_and(|v| !v.is_empty())
    {
        wm.set_debug_log(Arc::clone(&writer));
    }
    if let Some(backend) = backend {
        WatchManager::set_backend(&mut wm.lock(), backend);
    }
    let options = config.compiler_options().unwrap();
    tsrs_compiler::bind_in_file_regions();
    let mut w = Box::new(Watcher {
        sys,
        shared_program_data: None,
        free_replaced: None,
        signature_job: None,
        config_file_name: config
            .config_file
            .map(|f| f.source_file.file_name().to_string())
            .unwrap_or_default(),
        config,
        compiler_options_from_command_line,
        command_line_raw,
        report_diagnostic,
        report_error_summary,
        report_watch_status: tsc::create_watch_status_reporter(
            sys,
            &options,
            testing.map(|t| t as &dyn CommandLineTesting),
        ),
        testing,
        content_mapper_host: None,
        content_mapper_project: None,
        program: None,
        extended_config_cache: Arc::new(ExtendedConfigCache::default()),
        config_modified: false,
        config_has_errors: false,
        config_file_paths: Vec::new(),
        source_file_cache: Arc::default(),
        file_regions: Arc::default(),
        full_build_files: Vec::new(),
        full_build_regions: None,
        snapshot_diagnostics: None,
        freer: Freer::new(testing.is_some()),
        wm,
        seen_files: FxHashSet::default(),
        seen_dirs: FxHashSet::default(),
        own_outputs: FxHashSet::default(),
        config_mtimes: FxHashMap::default(),
        watch_set_dirty: false,
        force_full_rebuild: false,
        program_ready: false,
        fast_path_builds: 0,
        full_builds: 0,
    });
    w.start();
    if testing.is_some() {
        return Some(w);
    }
    let wm = Arc::clone(&w.wm);
    w.freer.after_cycle();
    wm.run_loop(|| {
        w.do_cycle();
        w.freer.after_cycle();
    });
    None
}

impl Watcher {
    fn start(&mut self) {
        self.content_mapper_host =
            tsc::new_content_mapper_host(self.sys, &self.config.compiler_options().unwrap());
        self.replace_content_mapper_project(self.config);
        let wm = Arc::clone(&self.wm);
        let mut state = wm.lock();
        let host = self.new_compiler_host(self.sys.fs());
        self.program = tsrs_incremental::read_build_info_program(
            self.config,
            &*tsrs_incremental::new_build_info_reader(Arc::clone(&host)),
            &*host,
        );

        if !self.config_file_name.is_empty() {
            self.config_file_paths = std::iter::once(self.config_file_name.clone())
                .chain(self.config.extended_source_files())
                .collect();
        }
        if self.testing.is_none() {
            wm.ensure_default_backend(&mut state);
        }
        (self.report_watch_status)(new_compiler_diagnostic(
            &diagnostics::STARTING_COMPILATION_IN_WATCH_MODE,
            &[],
        ));
        self.watch_set_dirty = true;
        if self.do_build(&mut state).is_err() {
            wm.force_overflow();
        }
        if self.testing.is_none() {
            self.queue_files_changed_during_build();
        }
    }

    // tsrs-only: the watches start after the first build, so a file saved while that build ran (after it was read)
    // has no event; Go misses the edit until the next save (tsgo too). A file whose modification time is no longer
    // the one it was parsed at is queued as a change, and the first cycle picks it up. One stat per source file
    // (about 15 ms for 9k files), once.
    fn queue_files_changed_during_build(&self) {
        let fs = self.sys.fs();
        let mut changed: Vec<String> = Vec::new();
        for path in self.source_file_cache.keys() {
            let Some(cached) = self.source_file_cache.load(&path) else {
                continue;
            };
            let file_name = cached.file.file_name();
            if fs.stat(file_name).map(|s| s.mod_time()) != Some(cached.mod_time) {
                changed.push(file_name.to_string());
            }
        }
        if !changed.is_empty() {
            changed.sort();
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

    fn new_compiler_host(&self, fs: Arc<dyn FS>) -> Arc<dyn CompilerHost> {
        new_compiler_host(
            self.sys.get_current_directory(),
            fs,
            self.sys.default_library_path(),
            Some(Arc::clone(&self.extended_config_cache)
                as Arc<dyn tsoptions::ExtendedConfigCache + Send>),
            Some(crate::execute::get_trace_from_sys(self.sys, self.testing)),
            self.content_mapper_project.clone(),
        )
    }

    fn replace_content_mapper_project(&mut self, config: P<ParsedCommandLine>) {
        if self.content_mapper_host.is_none() {
            return;
        }
        let project =
            crate::execute::get_content_mapper_project(self.content_mapper_host.as_ref(), config);
        if let Some(old) = self.content_mapper_project.take() {
            let _ = old.close();
        }
        self.content_mapper_project = project;
    }

    fn content_mapper_watched_files(&self) -> Vec<String> {
        let mut files: Vec<String> = self
            .config
            .content_mappers()
            .iter()
            .filter(|m| !m.package_directory.is_empty() && m.contribution_id.is_empty())
            .map(|m| tspath::combine_paths(&m.package_directory, &["package.json"]))
            .collect();
        if let Some(project) = &self.content_mapper_project {
            match project.watched_files() {
                Ok(dynamic) => files.extend(dynamic),
                Err(err) => {
                    (self.report_diagnostic)(tsrs_compiler::content_mapper_project_diagnostic(
                        &err,
                    ));
                    return files;
                }
            }
        }
        files.sort();
        files.dedup();
        files
    }

    fn compare_paths_options(&self) -> ComparePathsOptions {
        ComparePathsOptions {
            use_case_sensitive_file_names: self.sys.fs().use_case_sensitive_file_names(),
            current_directory: self.sys.get_current_directory().to_string(),
        }
    }

    fn to_path(&self, file_name: &str) -> Path {
        tspath::to_path(
            file_name,
            self.sys.get_current_directory(),
            self.sys.fs().use_case_sensitive_file_names(),
        )
    }

    fn compiler_program(&self) -> &'static Program {
        self.program.expect("built").get_program()
    }

    fn compute_desired_watches(&self, seen_file_paths: &[String]) -> FxHashMap<String, bool> {
        let fs = self.sys.fs();
        let mut desired_dirs: FxHashMap<String, bool> = FxHashMap::default(); // dir → recursive

        // Wildcard directories from tsconfig (recursive or non-recursive)
        if self.config.config_file.is_some()
            && let Some(wildcards) = self.config.wildcard_directories()
        {
            for (dir, &recursive) in wildcards {
                desired_dirs.insert(fs.realpath(dir), recursive);
            }
        }
        // For no-config CLI mode, ensure CWD is watched
        if self.config.config_file.is_none() && desired_dirs.is_empty() {
            desired_dirs.insert(fs.realpath(self.sys.get_current_directory()), false);
        }
        // Config file parent directories as non-recursive watches
        for cfg_path in &self.config_file_paths {
            let dir = tspath::get_directory_path(&fs.realpath(cfg_path));
            desired_dirs.entry(dir).or_insert(false);
        }
        // For no-config CLI mode, also watch the CLI-specified files' directories
        if self.config.config_file.is_none() {
            for file_name in self.config.file_names() {
                let abs = tspath::get_normalized_absolute_path(
                    file_name,
                    self.sys.get_current_directory(),
                );
                desired_dirs.entry(tspath::get_directory_path(&fs.realpath(&abs))).or_insert(false);
            }
        }

        // Add parent directories for seen files not covered by existing dir watches. Resolve ancestor fallbacks first
        // so coverage checks use final dirs.
        let resolved_dirs = self.wm.resolve_desired_dirs(&desired_dirs);
        let mut coverage = DirWatchSet::new(&self.compare_paths_options());
        let mut resolved_sorted: Vec<(&String, &bool)> = resolved_dirs.iter().collect();
        resolved_sorted.sort();
        for (dir, &recursive) in resolved_sorted {
            coverage.set(dir, recursive);
        }
        let program_files = self.compiler_program().files_by_path();
        let root_files: FxHashSet<Path> =
            self.config.file_names().iter().map(|f| self.to_path(f)).collect();
        // tsrs-only: directories under a recursive watch that are reached through a symlink, checked once each.
        let mut symlink_checked: FxHashSet<String> = FxHashSet::default();
        for file_path in seen_file_paths {
            let dir = tspath::get_directory_path(file_path);
            if coverage.covered(&dir) {
                // tsrs-only: a recursive watch does not follow a symlinked directory below its root (FSEvents and
                // inotify both report the target's own path, which no watch covers), so a file read through one
                // would never be seen to change. Its directory gets a watch of its own, under the name the program
                // uses; the backend watches the target and reports the events under that name. Go misses these edits
                // (tsgo too).
                if !coverage.has(&dir)
                    && symlink_checked.insert(dir.clone())
                    && fs.realpath(&dir) != dir
                {
                    coverage.set(&dir, false);
                }
                continue;
            }
            // Seen files mix program files with lookup locations. Only lookups keep the depth check, so an imported
            // file outside the tsconfig directory (say /shared next to /app) is still watched. A root file is not in
            // the program while it is missing, but its directory stays watched so that recreating it rebuilds.
            let p = self.to_path(file_path);
            if program_files.contains_key(&p)
                || root_files.contains(&p)
                || watchmanager::can_watch_directory(&dir)
            {
                coverage.set(&dir, false);
            }
        }
        // Re-resolve in case newly added dirs don't exist
        self.wm.resolve_desired_dirs(&coverage.dirs())
    }

    fn reconcile_watches(
        &self,
        state: &mut WatchState,
        seen_file_paths: &[String],
    ) -> Result<(), tsrs_fswatch::Error> {
        let desired_dirs = self.compute_desired_watches(seen_file_paths);
        self.wm.reconcile_watches(state, &desired_dirs)
    }

    /// Go `DoCycle`.
    fn do_cycle(&mut self) {
        let wm = Arc::clone(&self.wm);
        let mut state = wm.lock();

        let (mut changed_paths, overflow) = wm.drain_events();
        self.drop_own_output_events(&mut changed_paths);
        self.drop_unchanged_file_events(&mut changed_paths);
        let has_events = !changed_paths.is_empty() || overflow;

        let manifest_changed = self.content_mapper_manifest_changed(&changed_paths);
        if self.recheck_ts_config(manifest_changed) {
            return;
        }

        if has_events && !overflow && !self.config_modified {
            // Filter fswatch events against known dependencies
            if !self.is_relevant_change(&state, &changed_paths) {
                if let Some(log) = wm.debug_log() {
                    log(&format!(
                        "[watch] DoCycle: {} event(s) not relevant to compilation, skipping rebuild\n",
                        changed_paths.len()
                    ));
                }
                if let Some(testing) = self.testing {
                    testing.on_program(self.program.unwrap());
                }
                return;
            }
            self.evict_changed_source_files(&changed_paths);
            let fs = self.sys.fs();
            let program_files = self.compiler_program().files_by_path();
            let content_mapper_watched_files: FxHashSet<Path> =
                self.content_mapper_watched_files().iter().map(|f| self.to_path(f)).collect();
            let mut content_mapper_config_changed = false;
            for event_path in sorted_keys(&changed_paths) {
                if fs.directory_exists(event_path) {
                    // A watched directory changed: the wildcard file set may have changed, so reload file names on
                    // the next build.
                    if self.directory_change_matters(&self.to_path(event_path)) {
                        self.watch_set_dirty = true;
                    }
                    continue;
                }
                let p = self.to_path(event_path);
                if content_mapper_watched_files.contains(&p) {
                    content_mapper_config_changed = true;
                    self.force_full_rebuild = true;
                }
                if self.config.config_file.is_some()
                    && self.config.possibly_matches_file_name(event_path)
                    && !self.seen_files.contains(&p)
                {
                    // A file that matches the project but was not previously seen appeared: a structural change that
                    // requires a full rebuild, not the single-file fast path.
                    self.watch_set_dirty = true;
                    self.force_full_rebuild = true;
                    continue;
                }
                match program_files.get(&p) {
                    // Canonical mapped files must be transformed again, and supplemental paths are failed physical
                    // lookups reserved for virtual files. Neither can use single-file AST reuse.
                    Some(source_file) if !source_file.content_mapper().is_empty() => {
                        self.force_full_rebuild = true;
                    }
                    // tsrs-only: on a case-insensitive file system a rename that changes only the case keeps the
                    // file's path, so the single-file fast path would reuse the program with the old spelling (and
                    // miss TS1149 for imports that spell it the old way). An event spelled unlike the program's
                    // file reloads the file names and rebuilds. Go (at the pinned commit) has the same fast path;
                    // the tsgo nightly of 2026-07-07 rebuilds here.
                    Some(source_file)
                        if !fs.use_case_sensitive_file_names()
                            && source_file.file_name() != event_path.as_str() =>
                    {
                        self.watch_set_dirty = true;
                        self.force_full_rebuild = true;
                    }
                    // A non-source build dependency changed (a package.json or a previously-missing module path):
                    // module resolution may now differ, so the single-file fast path is unsafe.
                    None if self.seen_files.contains(&p) => self.force_full_rebuild = true,
                    Some(_) | None => {}
                }
            }
            if content_mapper_config_changed
                && let Some(project) = &self.content_mapper_project
                && project.refresh().is_err()
            {
                (self.report_diagnostic)(new_compiler_diagnostic(
                    &diagnostics::THE_CONTENT_MAPPER_PROCESS_COULD_NOT_BE_STARTED_OR_INITIALIZED,
                    &[],
                ));
                return;
            }
        } else if overflow {
            // Overflow: evict the entire source file cache and force a full rebuild. The fast path must not run here:
            // after clearing the cache a one-file program would present exactly one cache miss and be misread as a
            // single-file content edit, silently reusing a stale program instead of rediscovering the file graph.
            self.source_file_cache = Arc::default();
            self.watch_set_dirty = true;
            self.force_full_rebuild = true;
        } else if !has_events && !self.config_modified {
            if let Some(log) = wm.debug_log() {
                log("[watch] DoCycle: no events, skipping\n");
            }
            if let Some(testing) = self.testing {
                testing.on_program(self.program.unwrap());
            }
            return;
        }

        // tsrs-only: only a cycle that builds stops the signature job (as orchestrator.rs `do_cycle`); one that returns
        // above leaves it running. Most such cycles are the events of the last build's own outputs, which come while
        // the job runs, and stopping it there left the placeholders until the next build.
        self.stop_signature_job();
        self.report_status(|| {
            new_compiler_diagnostic(
                &diagnostics::FILE_CHANGE_DETECTED_STARTING_INCREMENTAL_COMPILATION,
                &[],
            )
        });
        if self.do_build(&mut state).is_err() {
            // Mid-cycle watch failure; force a full rebuild on the next event
            wm.force_overflow();
        }
    }

    // tsrs-only: the build's own writes (its outputs and build info) come back as events. When they land under a
    // watched directory that the config's wildcards cover (an outDir inside `include: ["**/*"]`, or the build info
    // beside the config), each would look like a new project file and start a full rebuild after every cycle; the
    // TypeScript (JavaScript) watch ignores the program's emitted files the same way (isIgnoredFileFromWildCardWatching).
    // Go does not (tsgo runs that second cycle). An output that is also a source of the program is kept.
    fn drop_own_output_events(&self, changed_paths: &mut FxHashMap<String, EventKind>) {
        if self.own_outputs.is_empty() || self.program.is_none() {
            return;
        }
        let program_files = self.compiler_program().files_by_path();
        let before = changed_paths.len();
        let mut dropped: Vec<Path> = Vec::new();
        changed_paths.retain(|event_path, _| {
            let p = self.to_path(event_path);
            let keep = !self.own_outputs.contains(&p) || program_files.contains_key(&p);
            if !keep {
                dropped.push(p);
            }
            keep
        });
        // A backend that reports the ancestors of a changed file (the test harness's, FSEvents for a created file)
        // reports those of the outputs too.
        if !dropped.is_empty() {
            let options = self.compare_paths_options();
            changed_paths.retain(|event_path, _| {
                let p = self.to_path(event_path);
                !dropped
                    .iter()
                    .any(|output| tspath::contains_path(p.as_str(), output.as_str(), &options))
            });
        }
        if changed_paths.len() != before
            && let Some(log) = self.wm.debug_log()
        {
            log(&format!(
                "[watch] ignoring {} event(s) for the build's own outputs\n",
                before - changed_paths.len()
            ));
        }
    }

    // tsrs-only: an event for a source file whose modification time is still the one it was parsed at changes
    // nothing: the cycle would reuse the parsed file (`watchCompilerHost`), so it is dropped. Such events come from
    // metadata changes (chmod, extended attributes) and from writes made just before the watch started, which
    // FSEvents delivers after (about 12 ms late). Go runs a cycle for them.
    fn drop_unchanged_file_events(&self, changed_paths: &mut FxHashMap<String, EventKind>) {
        let fs = self.sys.fs();
        let before = changed_paths.len();
        changed_paths.retain(|event_path, kind| {
            if *kind != EventKind::UPDATE {
                return true;
            }
            let Some(cached) = self.source_file_cache.load(&self.to_path(event_path)) else {
                return true;
            };
            // The program's spelling: a case-only rename must still be seen.
            cached.file.file_name() != event_path.as_str()
                || fs.stat(event_path).is_none_or(|s| s.mod_time() != cached.mod_time)
        });
        if changed_paths.len() != before
            && let Some(log) = self.wm.debug_log()
        {
            log(&format!(
                "[watch] ignoring {} event(s) for files unchanged since they were read\n",
                before - changed_paths.len()
            ));
        }
    }

    fn compute_own_outputs(&self) -> FxHashSet<Path> {
        let options = self.config.compiler_options().unwrap();
        if options.no_emit.is_true() {
            return FxHashSet::default();
        }
        let mut outputs: FxHashSet<Path> =
            self.config.get_output_file_names().iter().map(|f| self.to_path(f)).collect();
        let build_info = self.config.get_build_info_file_name();
        if !build_info.is_empty() {
            outputs.insert(self.to_path(&build_info));
        }
        outputs
    }

    fn is_relevant_change(
        &self,
        state: &WatchState,
        changed_paths: &FxHashMap<String, EventKind>,
    ) -> bool {
        let fs = self.sys.fs();
        let opts = self.compare_paths_options();
        let content_mapper_watched_files: FxHashSet<Path> =
            self.content_mapper_watched_files().iter().map(|f| self.to_path(f)).collect();
        sorted_keys(changed_paths).into_iter().any(|event_path| {
            let p = self.to_path(event_path);
            // tsrs-only: Go also asks `PossiblyMatchesDirectoryName` of a changed file, which is true of any file under a
            // recursive wildcard directory (an editor's swap file, a log, a .md), so each write to one rebuilt the
            // program. Only a directory (or a deleted path, which may have been one) can bring in files that match.
            let maybe_directory = || {
                changed_paths[event_path] == EventKind::DELETE || fs.directory_exists(event_path)
            };
            content_mapper_watched_files.contains(&p)
                || self.seen_files.contains(&p)
                || (self.config.config_file.is_some()
                    && self.config.possibly_matches_file_name(event_path))
                || (self.config.config_file.is_some()
                    && self.config.possibly_matches_directory_name(&p)
                    && maybe_directory())
                || (fs.directory_exists(event_path)
                    && WatchManager::is_path_under_watch(state, event_path, &opts)
                    && self.directory_change_matters(&p))
        })
    }

    // tsrs-only: a directory that appeared or changed under a watched one matters when the config's wildcards may
    // match it, when a file the build read or looked for lies below it, or when it is or lies in a node_modules (an
    // installed package). Go
    // counts any directory under any watch, so a new directory beside the project (the watch of its parent is for a
    // node_modules lookup) ran a full cycle.
    fn directory_change_matters(&self, p: &Path) -> bool {
        (self.config.config_file.is_some() && self.config.possibly_matches_directory_name(p))
            || self.seen_dirs.contains(p)
            || watchmanager::is_in_node_modules(p)
    }

    fn do_build(&mut self, state: &mut WatchState) -> Result<(), tsrs_fswatch::Error> {
        if self.config_modified {
            self.source_file_cache = Arc::default();
            self.watch_set_dirty = true;
        }

        let mut reloaded_file_names = false;
        if self.watch_set_dirty {
            if self.config.config_file.is_some()
                && self.config.wildcard_directories().is_some_and(|w| !w.is_empty())
            {
                let new_config =
                    P::new(self.config.reload_file_names_of_parsed_command_line(&*self.sys.fs()));
                reloaded_file_names = true;
                if self.config.file_names() == new_config.file_names() {
                    self.watch_set_dirty = false;
                }
                self.config = new_config;
            } else if !self.config_modified {
                self.watch_set_dirty = false;
            }
        }

        if self.program.is_some()
            && self.program_ready
            && !self.config_modified
            && !self.watch_set_dirty
            && !self.force_full_rebuild
        {
            let cached = Arc::new(cachedvfs::from(self.sys.fs()));
            let inner_host = self.new_compiler_host(Arc::clone(&cached) as Arc<dyn FS>);
            let host: Arc<dyn CompilerHost> = Arc::new(WatchCompilerHost {
                inner: inner_host,
                cache: Arc::clone(&self.source_file_cache),
                regions: Arc::clone(&self.file_regions),
            });
            if self.try_update_program(host) {
                self.fast_path_builds += 1;
                let result = self.compile_and_emit();
                cached.disable_and_clear_cache();
                self.record_config_mtimes();
                self.config_modified = false;
                self.report_error_count(result.diagnostics.len());
                self.free_replaced_program();
                self.start_signature_job();
                if let Some(testing) = self.testing {
                    testing.on_program(self.program.unwrap());
                }
                return Ok(());
            }
            cached.disable_and_clear_cache();
        }

        let cached = Arc::new(cachedvfs::from(self.sys.fs()));
        let tfs = Arc::new(trackingvfs::FS::new(Arc::clone(&cached)));
        let inner_host = self.new_compiler_host(Arc::clone(&tfs) as Arc<dyn FS>);
        let host: Arc<dyn CompilerHost> = Arc::new(WatchCompilerHost {
            inner: inner_host,
            cache: Arc::clone(&self.source_file_cache),
            regions: Arc::clone(&self.file_regions),
        });

        if self.config.config_file.is_some() {
            if let Some(wildcards) = self.config.wildcard_directories() {
                for dir in wildcards.keys() {
                    tfs.add_seen_file(dir);
                }
            }
            if !reloaded_file_names
                && !self.watch_set_dirty
                && self.config.wildcard_directories().is_some_and(|w| !w.is_empty())
            {
                self.config =
                    P::new(self.config.reload_file_names_of_parsed_command_line(&*self.sys.fs()));
            }
        }
        for path in &self.config_file_paths {
            tfs.add_seen_file(path);
        }
        for path in self.content_mapper_watched_files() {
            tfs.add_seen_file(&path);
        }

        let mut program_options = ProgramOptions::new(self.config, Arc::clone(&host));
        // Each cycle replaces the program; its checkers' regions are freed with it (`install_program`), and what the
        // file loader allocates with the data the programs updated from it share.
        program_options.checker_regions = true;
        // `full_build_files` keeps the parents of the synthetic imports the reused programs keep.
        program_options.reuse_synthetic_imports = true;
        // The harness reads diagnostics of programs that are freed (`install_program`).
        program_options.checker_diagnostic_regions = self.testing.is_none();
        let regions = Arc::new(tsrs_compiler::ProgramRegions::default());
        program_options.regions = Some(Arc::clone(&regions));
        let program = new_program(program_options);
        self.install_program(program, host, Some(regions));
        self.program_ready = true;
        self.full_builds += 1;

        let result = self.compile_and_emit();
        cached.disable_and_clear_cache();

        let seen = tfs.seen_files();
        self.seen_files = seen.iter().map(|p| self.to_path(p)).collect();
        self.seen_dirs = seen_directories(&self.seen_files);
        self.own_outputs = self.compute_own_outputs();
        self.record_config_mtimes();

        if let Err(err) = self.reconcile_watches(state, &seen) {
            self.sys.write(&format!("{}\n", err.message()));
            // tsrs-only: the build's diagnostics and status line still go out (Go returns here and prints neither);
            // the status line also flushes the output, which would otherwise hold the error until the next cycle.
            self.report_error_count(result.diagnostics.len());
            return Err(err);
        }
        self.watch_set_dirty = false;
        self.config_modified = false;
        self.force_full_rebuild = false;

        let program_files = self.compiler_program().files_by_path();
        for path in self.source_file_cache.keys() {
            if !program_files.contains_key(&path) {
                self.source_file_cache.delete(&path);
            }
        }

        self.report_error_count(result.diagnostics.len());
        self.free_replaced_program();
        self.start_signature_job();
        if let Some(testing) = self.testing {
            testing.on_program(self.program.unwrap());
        }
        Ok(())
    }

    fn record_config_mtimes(&mut self) {
        let fs = self.sys.fs();
        self.config_mtimes = self
            .config_file_paths
            .iter()
            .filter_map(|cfg_path| fs.stat(cfg_path).map(|s| (cfg_path.clone(), s.mod_time())))
            .collect();
    }

    fn report_error_count(&self, error_count: usize) {
        self.report_status(|| {
            if error_count == 1 {
                new_compiler_diagnostic(&diagnostics::FOUND_1_ERROR_WATCHING_FOR_FILE_CHANGES, &[])
            } else {
                new_compiler_diagnostic(
                    &diagnostics::FOUND_0_ERRORS_WATCHING_FOR_FILE_CHANGES,
                    &[&error_count],
                )
            }
        });
    }

    // tsrs-only: reports a status message made in a region freed right after (the reporter writes it out and keeps
    // nothing); the two of each cycle would otherwise stay in the thread arena.
    fn report_status(&self, make: impl FnOnce() -> P<tsrs_ast::Diagnostic>) {
        let region = Region::new_scratch(4 << 10);
        let diagnostic = {
            let _route = region.route_diagnostics();
            make()
        };
        (self.report_watch_status)(diagnostic);
    }

    fn try_update_program(&mut self, host: Arc<dyn CompilerHost>) -> bool {
        let old_program = self.compiler_program();
        let mut changed_path: Option<&Path> = None;
        let mut changed_count = 0;
        for file in old_program.get_source_files() {
            if !file.content_mapper().is_empty() {
                continue;
            }
            if self.source_file_cache.load(file.path()).is_none() {
                changed_path = Some(file.path());
                changed_count += 1;
                if changed_count > 1 {
                    return false;
                }
            }
        }
        let Some(changed_path) = changed_path else {
            return false;
        };
        let (new_program, _, reused) =
            old_program.reuse_program(changed_path, Arc::clone(&host), None, None);
        if reused {
            self.install_program(new_program.unwrap(), host, None);
        }
        reused
    }

    // tsrs-only (Go's GC): makes `program` the current one and frees the compiler program it replaces, with its
    // checkers, once the new incremental program has taken what it keeps of the old snapshot (plain data, and
    // diagnostics, which it copies to a region of its own: the old program's checkers made some in theirs). A full
    // build also frees the data the programs since the previous full build shared (`update_program` hands it on).
    // Then the regions of the files nothing refers to any more (watchmem.rs).
    // `full_build_regions`: the file loader's regions of a full build (`None` for a program updated from the current one).
    fn install_program(
        &mut self,
        program: &'static Program,
        host: Arc<dyn CompilerHost>,
        full_build_regions: Option<Arc<tsrs_compiler::ProgramRegions>>,
    ) {
        let full_build = full_build_regions.is_some();
        // Bind before any checker exists: single-threaded, the first checker would bind the files in its region, and
        // later programs reuse the cached files.
        program.bind_source_files();
        // The harness reads the old snapshot's diagnostics, which the new one shares (`copy_diagnostics`).
        let snapshot_diagnostics = self.testing.is_none().then(|| Region::new_scratch(64 << 10));
        let new = {
            // The diagnostics the snapshot recomputes for the new program (`repopulate_diagnostics_of_file`).
            let _route = snapshot_diagnostics.as_ref().map(Region::route_diagnostics);
            tsrs_incremental::new_program(
                program,
                self.program,
                tsrs_incremental::create_host(host),
                Some(std::time::Instant::now),
                self.testing.is_some(),
            )
        };
        if let Some(region) = &snapshot_diagnostics {
            let _scope = region.enter();
            new.copy_diagnostics();
        }
        let old_snapshot_diagnostics =
            std::mem::replace(&mut self.snapshot_diagnostics, snapshot_diagnostics);
        let old_incremental = self.program.replace(new);
        let old = old_incremental.and_then(|old| old.compiler_program());
        if self.testing.is_none()
            && let Some(old_incremental) = old_incremental
        {
            // SAFETY: `new` was created from it, and nothing else holds it: the watcher's only reference was
            // `self.program`, and the signature job, which reads the current program, is stopped during a cycle. Both
            // incremental programs are made on this thread (`install_program`).
            unsafe { tsrs_incremental::Program::free_replaced(old_incremental) };
        }
        let (shared, old_regions) = if full_build {
            self.full_build_files = program.get_source_files().to_vec();
            (
                self.shared_program_data.replace(tsrs_compiler::shared_program_data(program)),
                std::mem::replace(&mut self.full_build_regions, full_build_regions),
            )
        } else {
            (None, None)
        };
        let dead_files = self.file_regions.take_unreachable(&self.live_files(new));
        let free = move || {
            if let Some(old) = old {
                // SAFETY: nothing refers to the old compiler program any more: the watcher's only reference was
                // `self.program`, its checkers are not held (the cycle that used them is over), and the new
                // incremental program read the old snapshot without keeping the program.
                unsafe { tsrs_compiler::free_program(old) };
            }
            // The snapshot that kept them is gone, and the new one keeps copies.
            drop(old_snapshot_diagnostics);
            if let Some(shared) = shared {
                // SAFETY: every program since the previous full build shared this data, and each was freed when the
                // next one replaced it (above, for the last one, after the earlier ones: the frees run in order);
                // the new full build has its own.
                unsafe { shared.free() };
            }
            // The programs that referred to these files are freed (above, or by earlier jobs), and nothing that is
            // read again does (`live_files`).
            drop(dead_files);
            // Last: the programs and the shared data above point into them (the programs hold the set too).
            drop(old_regions);
        };
        // The test harness frees in place, for deterministic runs.
        if self.testing.is_some() {
            free();
        } else {
            // A cycle that ended early (a watch error) left its free pending.
            self.free_replaced_program();
            self.free_replaced = Some(Box::new(free));
        }
    }

    // tsrs-only: while the watch waits for the next change, replaces the placeholder signatures of the program's
    // snapshot with declaration signatures (tsrs_incremental `Program::compute_placeholder_signatures`), so that
    // the next edit of a file whose declarations do not change rechecks that file instead of everything importing
    // it. The next cycle cancels the computation and waits for the chunk in progress (`stop_signature_job`). The
    // test harness leaves the placeholders, as Go does, unless the scenario asks for the job (`run_signature_job`).
    fn start_signature_job(&mut self) {
        let Some(program) = self.program else { return };
        if let Some(testing) = self.testing {
            if testing.run_signature_job() {
                program.compute_placeholder_signatures(
                    &tsrs_compiler::Context::default(),
                    SIGNATURE_JOB_CHUNK,
                );
            }
            return;
        }
        let (ctx, cancel) = tsrs_compiler::Context::default().with_cancel();
        let wm = Arc::clone(&self.wm);
        let job = std::thread::Builder::new()
            .name("tsrs-signatures".to_string())
            .spawn(move || {
                let start = std::time::Instant::now();
                let stored = program.compute_placeholder_signatures(&ctx, SIGNATURE_JOB_CHUNK);
                if let Some(log) = wm.debug_log() {
                    let state = if ctx.err().is_some() { "canceled" } else { "done" };
                    log(&format!(
                        "[watch] stored {stored} declaration signature(s) in {:?} ({state})\n",
                        start.elapsed()
                    ));
                }
            })
            .expect("failed to spawn a thread");
        self.signature_job = Some((cancel, job));
    }

    fn stop_signature_job(&mut self) {
        if let Some((cancel, job)) = self.signature_job.take() {
            cancel.call();
            job.join().expect("the signature computation panicked");
        }
    }

    // Freeing a large program's checker regions takes a while (~150 ms on a 9k-file project) and nothing waits for it,
    // so it runs on the free thread (watchmem.rs `Freer`), queued once the cycle has reported instead of beside the
    // rest of the cycle.
    fn free_replaced_program(&mut self) {
        if let Some(free) = self.free_replaced.take() {
            self.freer.run(free);
        }
    }

    // The files whose regions must live on: those the cache can hand out again, the current program's, the last full
    // build's (its processed-file data, which the current program shares, refers to them), and those the
    // diagnostics the new snapshot copied from the old one refer to. The old incremental program also refers to
    // older files, but nothing reads it again.
    fn live_files(&self, program: P<tsrs_incremental::Program>) -> FxHashSet<usize> {
        let mut live = FxHashSet::default();
        self.source_file_cache.range(|_, cached| {
            live.insert(cached.file.addr());
            true
        });
        live.extend(self.full_build_files.iter().map(|f| f.addr()));
        if let Some(compiler_program) = program.compiler_program() {
            live.extend(compiler_program.get_source_files().iter().map(|f| f.addr()));
        }
        program.for_each_diagnostic_file(&mut |f| {
            live.insert(f.addr());
        });
        live
    }

    fn evict_changed_source_files(&self, changed_paths: &FxHashMap<String, EventKind>) {
        for event_path in sorted_keys(changed_paths) {
            let p = self.to_path(event_path);
            if self.source_file_cache.load(&p).is_some() {
                if let Some(log) = self.wm.debug_log() {
                    log(&format!("[watch] evicting cached source file: {p}\n"));
                }
                self.source_file_cache.delete(&p);
            }
        }
    }

    fn compile_and_emit(&self) -> tsc::CompileAndEmitResult {
        let program = self.program.unwrap();
        tsc::emit_files_and_report_errors(&EmitInput {
            sys: self.sys,
            program: program.get_program(),
            config: self.config,
            report_diagnostic: &self.report_diagnostic,
            format_diagnostic: None,
            report_error_summary: &self.report_error_summary,
            compile_times: tsc::CompileTimes::default(),
            incremental: Some(program),
            writer: None,
            write_file: None,
            testing: self.testing,
            testing_m_times_cache: None,
        })
    }

    fn content_mapper_manifest_changed(
        &self,
        changed_paths: &FxHashMap<String, EventKind>,
    ) -> bool {
        let options = self.compare_paths_options();
        let mut changed_path_keys: Option<FxHashSet<Path>> = None;
        self.config
            .content_mappers()
            .iter()
            .filter(|m| !m.package_directory.is_empty() && m.contribution_id.is_empty())
            .any(|m| {
                let keys = changed_path_keys.get_or_insert_with(|| {
                    changed_paths
                        .keys()
                        .map(|path| {
                            tspath::to_path(
                                path,
                                &options.current_directory,
                                options.use_case_sensitive_file_names,
                            )
                        })
                        .collect()
                });
                let manifest = tspath::combine_paths(&m.package_directory, &["package.json"]);
                let manifest_key = tspath::to_path(
                    &manifest,
                    &options.current_directory,
                    options.use_case_sensitive_file_names,
                );
                keys.contains(&manifest_key)
            })
    }

    // Returns true when the config could not be parsed (its errors were reported and the cycle ends).
    fn recheck_ts_config(&mut self, force: bool) -> bool {
        if self.config_file_name.is_empty() {
            return false;
        }
        if !force && !self.config_has_errors && !self.config_file_paths.is_empty() {
            let fs = self.sys.fs();
            let changed = self.config_file_paths.iter().any(|path| {
                let stat = fs.stat(path);
                match self.config_mtimes.get(path) {
                    None => stat.is_some(),
                    Some(old_mtime) => stat.is_none_or(|s| s.mod_time() != *old_mtime),
                }
            });
            if !changed {
                return false;
            }
        }

        let Some(config_parse_result) = self.parse_config_file() else {
            return true;
        };
        if self.config_has_errors {
            self.config_modified = true;
        }
        self.config_has_errors = false;
        self.config_file_paths = std::iter::once(self.config_file_name.clone())
            .chain(config_parse_result.extended_source_files())
            .collect();
        if !parsed_configs_equal(&self.config, &config_parse_result) {
            self.config_modified = true;
        }
        self.replace_content_mapper_project(config_parse_result);
        self.config = config_parse_result;
        false
    }

    fn parse_config_file(&mut self) -> Option<P<ParsedCommandLine>> {
        let extended_config_cache = Arc::new(ExtendedConfigCache::default());
        let host = crate::execute::parse_config_host(self.sys);
        let (config_parse_result, errors) = tsrs_compiler::worker_pool().install(|| {
            tsoptions::get_parsed_command_line_of_config_file(
                &self.config_file_name,
                Some(&self.compiler_options_from_command_line),
                self.command_line_raw.as_ref(),
                host,
                Some(&*extended_config_cache),
            )
        });
        if !errors.is_empty() {
            let error_count = errors.len();
            for e in errors {
                (self.report_diagnostic)(e);
            }
            self.config_has_errors = true;
            self.report_error_count(error_count);
            return None;
        }
        self.extended_config_cache = extended_config_cache;
        config_parse_result.map(P::new)
    }
}

// tsrs-only: every directory that contains one of `files`, at any depth.
fn seen_directories(files: &FxHashSet<Path>) -> FxHashSet<Path> {
    let mut dirs: FxHashSet<Path> = FxHashSet::default();
    #[expect(clippy::iter_over_hash_type, reason = "builds a set; the order does not matter")]
    for file in files {
        let mut dir = file.get_directory_path();
        // Stop at a directory already added: its ancestors are too.
        while dirs.insert(dir.clone()) {
            let parent = dir.get_directory_path();
            if parent == dir {
                break;
            }
            dir = parent;
        }
    }
    dirs
}

// Go ranges over the event map; the order only matters for debug logs and the first matching reason.
fn sorted_keys(changed_paths: &FxHashMap<String, EventKind>) -> Vec<&String> {
    let mut keys: Vec<&String> = changed_paths.keys().collect();
    keys.sort();
    keys
}

// Go `w.config.ParsedConfig.Equals(configParseResult.ParsedConfig)`.
fn parsed_configs_equal(a: &ParsedCommandLine, b: &ParsedCommandLine) -> bool {
    a.parsed_config.equals(&b.parsed_config)
}
