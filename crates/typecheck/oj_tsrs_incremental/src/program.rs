// Port of execute/incremental/program.go.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;
use tsrs_ast::{Diagnostic, DiagnosticExt, SourceFile, new_compiler_diagnostic};
use tsrs_compiler::{Context, Program as CompilerProgram, filter_no_emit_semantic_diagnostics};
use tsrs_core::tspath::{self, ComparePathsOptions, Path};
use tsrs_core::{CompilerOptions, P, Tristate};
use tsrs_diagnostics as diagnostics;

use crate::affectedfileshandler::collect_all_affected_files;
use crate::emit::{
    EmitOnly, EmitOptions, EmitResult, ProgramLike, WriteFileData, handle_no_emit_options,
};
use crate::emitfileshandler::emit_files;
use crate::host::Host;
use crate::programtosnapshot::program_to_snapshot;
use crate::snapshot::{DiagnosticsOrBuildInfoDiagnosticsWithFileName, Snapshot};
use crate::snapshottobuildinfo::snapshot_to_build_info;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SignatureUpdateKind {
    ComputedDts,
    StoredAtEmit,
    UsedVersion,
}

#[derive(Default)]
struct NestedEmitState {
    depth: usize,
    start: Option<Instant>,
    time: Duration,
}

pub struct Program {
    pub snapshot: P<Snapshot>,
    pub program: Option<&'static CompilerProgram>,
    pub(crate) host: Option<Box<dyn Host + Send + Sync>>,

    // Testing data
    pub(crate) testing_data: Option<Mutex<TestingData>>,

    nested_emit_now: Option<fn() -> Instant>,
    nested_emit: Mutex<NestedEmitState>,
}

// program.go:46
pub fn new_program(
    program: &'static CompilerProgram,
    old_program: Option<P<Program>>,
    host: Box<dyn Host + Send + Sync>,
    nested_emit_now: Option<fn() -> Instant>,
    testing: bool,
) -> P<Program> {
    let snapshot = program_to_snapshot(program, old_program, testing);
    let testing_data = if testing {
        let testing_data = TestingData {
            old_snapshot: old_program.map(|p| p.snapshot),
            updated_signature_kinds: FxHashMap::default(),
        };
        Some(Mutex::new(testing_data))
    } else {
        None
    };
    // Recycled: see `drop_replaced`.
    P::new_recycled(Program {
        snapshot,
        program: Some(program),
        host: Some(host),
        testing_data,
        nested_emit_now: nested_emit_now.filter(|_| program.options().diagnostics_enabled()),
        nested_emit: Mutex::new(NestedEmitState::default()),
    })
}

// Go's TestingData holds pointers to the program's and the old program's semantic diagnostics maps; Rust keeps the
// old snapshot and answers the questions the harness asks (Program::testing_semantic_diagnostics_state).
pub struct TestingData {
    old_snapshot: Option<P<Snapshot>>,
    pub(crate) updated_signature_kinds: FxHashMap<Path, SignatureUpdateKind>,
}

// The tsctests "SemanticDiagnostics::" line of a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticDiagnosticsState {
    NotCached,
    Refreshed,
    Reused,
}

impl Program {
    pub(crate) fn new_from_snapshot(snapshot: P<Snapshot>) -> P<Program> {
        P::new(Program {
            snapshot,
            program: None,
            host: None,
            testing_data: None,
            nested_emit_now: None,
            nested_emit: Mutex::new(NestedEmitState::default()),
        })
    }

    // sys.go OnProgram: `*refresh*` when the cached entry is not the one the old program had (by pointer).
    pub fn testing_semantic_diagnostics_state(&self, path: &Path) -> SemanticDiagnosticsState {
        let Some(diagnostics) = self.snapshot.semantic_diagnostics_per_file.load(path) else {
            return SemanticDiagnosticsState::NotCached;
        };
        let testing_data = self.testing_data.as_ref().unwrap().lock().unwrap();
        let old =
            testing_data.old_snapshot.and_then(|s| s.semantic_diagnostics_per_file.load(path));
        match old {
            Some(old) if std::sync::Arc::ptr_eq(&old, &diagnostics) => {
                SemanticDiagnosticsState::Reused
            }
            _ => SemanticDiagnosticsState::Refreshed,
        }
    }

    pub fn testing_updated_signature_kind(&self, path: &Path) -> Option<SignatureUpdateKind> {
        self.testing_data
            .as_ref()
            .unwrap()
            .lock()
            .unwrap()
            .updated_signature_kinds
            .get(path)
            .copied()
    }

    // program.go:82
    pub(crate) fn begin_nested_emit(&self) -> impl FnOnce() + '_ {
        let now = self.nested_emit_now;
        if let Some(now) = now {
            let mut state = self.nested_emit.lock().unwrap();
            if state.depth == 0 {
                state.start = Some(now());
            }
            state.depth += 1;
        }
        move || {
            if let Some(now) = now {
                let mut state = self.nested_emit.lock().unwrap();
                state.depth -= 1;
                if state.depth == 0 {
                    let elapsed = now() - state.start.unwrap();
                    state.time += elapsed;
                }
            }
        }
    }

    // program.go:105
    pub fn take_nested_emit_time(&self) -> Duration {
        if self.nested_emit_now.is_none() {
            return Duration::ZERO;
        }
        let mut state = self.nested_emit.lock().unwrap();
        std::mem::take(&mut state.time)
    }

    // program.go:113
    fn panic_if_no_program(&self, method: &str) {
        assert!(self.program.is_some(), "{method}: should not be called without program");
    }

    // program.go:119
    /// The compiler program, `None` for one read from a build info.
    pub fn compiler_program(&self) -> Option<&'static CompilerProgram> {
        self.program
    }

    /// tsrs-only: replaces the signatures that stand in for a file's declaration shape with the signature of its
    /// declaration output. The stand-in is the file's version: the initial build stores one for every file, and
    /// `handle_dts_may_change_of` stores one for each file it invalidates without computing its declarations. The
    /// next edit of such a file compares its new declaration signature with the version, which always differs, and
    /// so invalidates every file importing it, transitively: about 5 s of rechecking on a 9.4k-file project after an
    /// edit that changed no declaration. With the real signature only a changed declaration shape does that.
    ///
    /// Does nothing while a change is pending (a non-empty changed file set). Otherwise every file has been checked
    /// against this program or is queued to be, so the files that depend on a file saw its current declaration
    /// shape, which is what a builder that always computed the signatures would hold. Emits `chunk` files at a time
    /// and returns between chunks once `ctx` is cancelled; the signatures stored until then stand. Must not run
    /// concurrently with anything else that uses this program or its snapshot. Returns the number of signatures
    /// stored.
    pub fn compute_placeholder_signatures(&self, ctx: &Context, chunk: usize) -> usize {
        self.oj_compute_placeholder_signatures(ctx, chunk, &|_| false, &|_, _| {})
    }

    /// oj (tools/tsrs-patches): `compute_placeholder_signatures` without the files `skip` names (by file name), and
    /// with each emitted file's own declaration transform time passed to `timed`. A cancel stops the emit before
    /// its next file, not at the end of the chunk (`EmitOptions::oj_interruptible`).
    pub fn oj_compute_placeholder_signatures(
        &self,
        ctx: &Context,
        chunk: usize,
        skip: &dyn Fn(&str) -> bool,
        timed: &(dyn Fn(P<SourceFile>, Duration) + Sync),
    ) -> usize {
        let snapshot = self.snapshot;
        if !snapshot.can_use_incremental_state()
            || snapshot.changed_files_set.size() != 0
            || ctx.err().is_some()
        {
            return 0;
        }
        let program = self.p();
        let files: Vec<P<SourceFile>> = program
            .get_source_files()
            .iter()
            .copied()
            .filter(|&file| {
                !file.is_declaration_file()
                    && !tsrs_ast::is_json_source_file(file)
                    && !skip(file.file_name())
                    && snapshot
                        .file_infos
                        .load(file.path())
                        .is_some_and(|info| info.signature == info.version)
            })
            .collect();
        let stored = std::sync::atomic::AtomicUsize::new(0);
        let write_file =
            |file_name: &str, text: &str, data: &mut WriteFileData| -> Result<(), String> {
                assert!(
                    tspath::is_declaration_file_name(file_name),
                    "File extension for signature expected to be dts, got : {file_name}"
                );
                let file = data.source_file.expect("declaration output of a source file");
                let signature = snapshot.compute_signature_with_diagnostics(file, text, data);
                if let Some(mut info) = snapshot.file_infos.load(file.path())
                    && !signature.is_empty()
                    && info.signature == info.version
                {
                    info.signature = signature;
                    snapshot.file_infos.store(file.path().clone(), info);
                    // Relaxed: a count, read once the emit has joined the threads that run this.
                    stored.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(())
            };
        for files in files.chunks(chunk.max(1)) {
            if ctx.err().is_some() {
                break;
            }
            program.emit(
                ctx,
                &EmitOptions {
                    target_source_files: Some(files.to_vec()),
                    emit_only: EmitOnly::EmitOnlyBuilderSignature,
                    write_file: Some(&write_file),
                    oj_interruptible: Some(timed),
                    ..Default::default()
                },
            );
        }
        let stored = stored.into_inner();
        if stored > 0 {
            snapshot.build_info_emit_pending.set(true);
        }
        stored
    }

    /// tsrs-only (watch mode, Go's GC): runs the destructors of a program nothing reads any more and of its
    /// snapshot, giving back the heap they own (the snapshot's maps, the host and its file system caches, about 13 MB
    /// per full build on a 9.4k-file project). Their arena blocks stay (`free_replaced` gives them back too).
    ///
    /// # Safety
    /// Nothing reads `program` or its snapshot afterwards: the program made from it (if any) has been created, and it
    /// was not created for the test harness (whose next program keeps the snapshot, `TestingData`).
    pub unsafe fn drop_replaced(program: P<Program>) {
        let snapshot = program.snapshot;
        // SAFETY: both are arena values the thread arenas never drop, and nothing uses them any more (this function's
        // contract), so each is dropped once.
        unsafe {
            std::ptr::drop_in_place(std::ptr::from_ref::<Program>(program.get()).cast_mut());
            std::ptr::drop_in_place(std::ptr::from_ref::<Snapshot>(snapshot.get()).cast_mut());
        }
    }

    /// `drop_replaced`, and gives their arena blocks to the thread's free list, for the next ones (`new_program`).
    ///
    /// # Safety
    /// As `drop_replaced`; both were allocated on this thread in its own arena.
    pub unsafe fn free_replaced(program: P<Program>) {
        let snapshot = program.snapshot;
        // SAFETY: this function's contract; the blocks are dead once dropped.
        unsafe {
            Program::drop_replaced(program);
            tsrs_core::free!(program);
            tsrs_core::free!(snapshot);
        }
    }

    /// tsrs-only (a watch session's file regions, `tsrs_execute` watchmem.rs): calls `f` with each source
    /// file that the diagnostics the snapshot keeps refer to, related information and message chains
    /// included. Diagnostics copied from an older snapshot refer to the file versions they were reported for,
    /// which need not be the program's.
    pub fn for_each_diagnostic_file(&self, f: &mut dyn FnMut(P<SourceFile>)) {
        fn visit(diagnostics: &[P<Diagnostic>], f: &mut dyn FnMut(P<SourceFile>)) {
            for d in diagnostics {
                if let Some(file) = d.file() {
                    f(file);
                }
                visit(d.message_chain(), f);
                visit(d.related_information(), f);
            }
        }
        let mut snapshots = vec![self.snapshot];
        // The test harness reads the previous snapshot's diagnostics too.
        if let Some(testing_data) = &self.testing_data
            && let Some(old) = testing_data.lock().unwrap().old_snapshot
        {
            snapshots.push(old);
        }
        for snapshot in snapshots {
            for map in
                [&snapshot.semantic_diagnostics_per_file, &snapshot.emit_diagnostics_per_file]
            {
                map.range(|_, cache| {
                    if let Some(diagnostics) = cache.diagnostics() {
                        visit(&diagnostics, f);
                    }
                    true
                });
            }
        }
    }

    /// tsrs-only (watch mode): replaces every diagnostic the snapshot keeps with a copy in the current
    /// allocation target (`DiagnosticExt::copy_deep`), so that the regions of the diagnostics it copied from
    /// the old snapshot and of those the replaced program's checkers made (`tsrs_compiler` checkerpool.rs
    /// `SlotChecker`) can be freed. Not for the test harness, which reads the old snapshot's diagnostics too:
    /// some are shared with this one.
    pub fn copy_diagnostics(&self) {
        for map in
            [&self.snapshot.semantic_diagnostics_per_file, &self.snapshot.emit_diagnostics_per_file]
        {
            map.range(|_, cache| {
                if let Some(diagnostics) = cache.diagnostics.lock().unwrap().as_mut() {
                    for d in diagnostics.iter_mut() {
                        *d = d.copy_deep();
                    }
                }
                true
            });
        }
    }

    pub fn get_program(&self) -> &'static CompilerProgram {
        self.panic_if_no_program("GetProgram");
        self.program.unwrap()
    }

    // program.go:124
    pub fn has_changed_dts_file(&self) -> bool {
        self.snapshot.has_changed_dts_file.get()
    }

    pub(crate) fn p(&self) -> &'static CompilerProgram {
        self.program.unwrap()
    }

    // program.go:160
    fn get_semantic_diagnostics_impl(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        self.panic_if_no_program("GetSemanticDiagnostics");
        if self.snapshot.options().no_check.is_true() {
            return Vec::new();
        }

        // Ensure all the diagnsotics are cached
        self.collect_semantic_diagnostics_of_affected_files(ctx, file);
        if ctx.err().is_some() {
            return Vec::new();
        }

        // Return result from cache
        if let Some(file) = file {
            return self.get_semantic_diagnostics_of_file(file);
        }

        let mut diagnostics = Vec::new();
        diagnostics.extend(
            self.p()
                .get_source_files()
                .iter()
                .flat_map(|&file| self.get_semantic_diagnostics_of_file(file)),
        );
        diagnostics
    }

    // program.go:186
    fn get_semantic_diagnostics_of_file(&self, file: P<SourceFile>) -> Vec<P<Diagnostic>> {
        let Some(cached_diagnostics) =
            self.snapshot.semantic_diagnostics_per_file.load(file.path())
        else {
            panic!("After handling all the affected files, there shouldnt be more changes");
        };
        let mut result = filter_no_emit_semantic_diagnostics(
            cached_diagnostics.get_diagnostics(self.p(), Some(file)),
            &self.snapshot.options(),
        );
        result.extend(self.p().get_include_processor_diagnostics(file));
        result
    }

    // Handle affected files and cache the semantic diagnostics for all of them or the file asked for
    // program.go:256
    fn collect_semantic_diagnostics_of_affected_files(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) {
        if self.snapshot.can_use_incremental_state() {
            // Get all affected files
            collect_all_affected_files(ctx, self);
            if ctx.err().is_some() {
                return;
            }

            if self.snapshot.semantic_diagnostics_per_file.size()
                == self.p().get_source_files().len()
            {
                // If we have all the files,
                return;
            }
        }

        let affected_files: Vec<P<SourceFile>> = match file {
            Some(file) => {
                if self.snapshot.semantic_diagnostics_per_file.load(file.path()).is_some() {
                    return;
                }
                vec![file]
            }
            None => self
                .p()
                .get_source_files()
                .iter()
                .copied()
                .filter(|file| {
                    self.snapshot.semantic_diagnostics_per_file.load(file.path()).is_none()
                })
                .collect(),
        };

        // Get their diagnostics and cache them
        let diagnostics_per_file =
            self.p().get_semantic_diagnostics_for_incremental(ctx, &affected_files);
        // commit changes if no err
        if ctx.err().is_some() {
            return;
        }

        // Commit changes to snapshot
        #[expect(
            clippy::iter_over_hash_type,
            reason = "stores one entry per file in an unordered map"
        )]
        for (file, diagnostics) in diagnostics_per_file {
            self.snapshot.semantic_diagnostics_per_file.store(
                file.path().clone(),
                DiagnosticsOrBuildInfoDiagnosticsWithFileName::from_diagnostics(diagnostics),
            );
        }
        if self.snapshot.semantic_diagnostics_per_file.size() == self.p().get_source_files().len()
            && self.snapshot.check_pending.get()
            && !self.snapshot.options().no_check.is_true()
        {
            self.snapshot.check_pending.set(false);
        }
        self.snapshot.build_info_emit_pending.set(true);
    }

    // program.go:304
    pub(crate) fn emit_build_info(
        &self,
        ctx: &Context,
        options: &EmitOptions,
    ) -> Option<EmitResult> {
        let program = self.p();
        let build_info_file_name = tsrs_tsoptions::outputpaths::get_build_info_file_name(
            &self.snapshot.options(),
            &ComparePathsOptions {
                current_directory: program.get_current_directory().to_string(),
                use_case_sensitive_file_names: program.use_case_sensitive_file_names(),
            },
        );
        if build_info_file_name.is_empty() || program.is_emit_blocked(&build_info_file_name) {
            return None;
        }
        if self.snapshot.has_errors.get() == Tristate::Unknown {
            tsrs_core::phases::time(
                program.options().diagnostics_enabled(),
                "BuildInfo: has errors",
                || {
                    self.ensure_has_errors_for_state(ctx, program);
                },
            );
            if self.snapshot.has_errors.get() != self.snapshot.has_errors_from_old_state.get()
                || self.snapshot.has_semantic_errors.get()
                    != self.snapshot.has_semantic_errors_from_old_state.get()
            {
                self.snapshot.build_info_emit_pending.set(true);
            }
        }
        if self.snapshot.package_jsons.borrow().is_none() {
            tsrs_core::phases::time(
                program.options().diagnostics_enabled(),
                "BuildInfo: package.jsons",
                || {
                    self.ensure_package_jsons_for_state();
                },
            );
            if self.snapshot.package_jsons.borrow().as_deref().unwrap_or_default()
                != self
                    .snapshot
                    .package_jsons_from_old_state
                    .borrow()
                    .as_deref()
                    .unwrap_or_default()
                || self.snapshot.missing_package_jsons.borrow().as_deref().unwrap_or_default()
                    != self
                        .snapshot
                        .missing_package_jsons_from_old_state
                        .borrow()
                        .as_deref()
                        .unwrap_or_default()
            {
                self.snapshot.build_info_emit_pending.set(true);
            }
        }
        if !self.snapshot.build_info_emit_pending.get() {
            return None;
        }
        if ctx.err().is_some() {
            return None;
        }
        let build_info = match tsrs_core::phases::time(
            program.options().diagnostics_enabled(),
            "BuildInfo: from snapshot",
            || snapshot_to_build_info(&self.snapshot, program, &build_info_file_name),
        ) {
            Ok(build_info) => build_info,
            Err(err) => {
                return Some(EmitResult {
                    emit_skipped: true,
                    diagnostics: vec![tsrs_compiler::content_mapper_project_diagnostic(&err)],
                    ..Default::default()
                });
            }
        };
        let text = tsrs_core::phases::time(
            program.options().diagnostics_enabled(),
            "BuildInfo: marshal",
            || build_info.marshal(),
        );
        let write_start = tsrs_core::phases::Timer::new(program.options().diagnostics_enabled());
        let err = match options.write_file {
            Some(write_file) => {
                let mut data = WriteFileData {
                    build_info: Some(std::sync::Arc::new(build_info)),
                    ..Default::default()
                };
                write_file(&build_info_file_name, &text, &mut data)
            }
            None => program.host().fs().write_file(&build_info_file_name, &text),
        };
        write_start.record("BuildInfo: write");
        if let Err(err) = err {
            return Some(EmitResult {
                emit_skipped: true,
                diagnostics: vec![new_compiler_diagnostic(
                    &diagnostics::COULD_NOT_WRITE_FILE_0_COLON_1,
                    &[&build_info_file_name, &err],
                )],
                ..Default::default()
            });
        }
        self.snapshot.build_info_emit_pending.set(false);
        Some(EmitResult {
            emit_skipped: false,
            emitted_files: vec![build_info_file_name],
            ..Default::default()
        })
    }

    // program.go:368
    fn ensure_has_errors_for_state(&self, ctx: &Context, program: &'static CompilerProgram) {
        let mut has_include_processing_diagnostics: Option<bool> = None;
        let mut has_emit_diagnostics = false;
        let snapshot = &self.snapshot;
        let compute_has_include_processing_diagnostics: Box<dyn Fn() -> bool> =
            if snapshot.can_use_incremental_state() {
                if program.get_source_files().iter().any(|&file| {
                    if snapshot.emit_diagnostics_per_file.load(file.path()).is_some() {
                        // emit diagnostics will be encoded in buildInfo;
                        return true;
                    }
                    if has_include_processing_diagnostics.is_none()
                        && !program.get_include_processor_diagnostics(file).is_empty()
                    {
                        has_include_processing_diagnostics = Some(true);
                    }
                    false
                }) {
                    has_emit_diagnostics = true;
                }
                let value = has_include_processing_diagnostics.unwrap_or(false);
                Box::new(move || value)
            } else {
                has_emit_diagnostics = snapshot.has_emit_diagnostics.get();
                Box::new(move || {
                    program
                        .get_source_files()
                        .iter()
                        .any(|&file| !program.get_include_processor_diagnostics(file).is_empty())
                })
            };

        if has_emit_diagnostics {
            // Record this for only non incremental build info
            snapshot.has_errors.set(if snapshot.options().is_incremental() {
                Tristate::False
            } else {
                Tristate::True
            });
            // Dont need to encode semantic errors state since the emit diagnostics are encoded
            snapshot.has_semantic_errors.set(false);
            return;
        }

        if compute_has_include_processing_diagnostics()
            || !program.get_config_file_parsing_diagnostics().is_empty()
            || !program.get_syntactic_diagnostics(ctx, None).is_empty()
            || !program.get_program_diagnostics().is_empty()
            || !program.get_global_diagnostics(ctx).is_empty()
        {
            snapshot.has_errors.set(Tristate::True);
            // Dont need to encode semantic errors state since the syntax and program diagnostics are encoded as present
            snapshot.has_semantic_errors.set(false);
            return;
        }

        snapshot.has_errors.set(Tristate::False);
        // Check semantic and emit diagnostics first as we dont need to ask program about it
        if program.get_source_files().iter().any(|&file| {
            let Some(semantic_diagnostics) =
                snapshot.semantic_diagnostics_per_file.load(file.path())
            else {
                // Missing semantic diagnostics in cache will be encoded in incremental buildInfo
                return snapshot.options().is_incremental();
            };
            // cached semantic diagnostics will be encoded in buildInfo
            semantic_diagnostics.has_diagnostics()
                || !semantic_diagnostics.build_info_diagnostics.is_empty()
        }) {
            // Because semantic diagnostics are recorded in buildInfo, we dont need to encode hasErrors in incremental buildInfo
            // But encode as errors in non incremental buildInfo
            snapshot.has_semantic_errors.set(!snapshot.options().is_incremental());
        }
    }

    // program.go:443
    fn ensure_package_jsons_for_state(&self) {
        let program = self.p();
        let mut package_jsons: Vec<String> = Vec::new();
        let mut missing_package_jsons: Vec<String> = Vec::new();
        let config = tspath::get_directory_path(program.command_line().config_name());
        if !config.is_empty() {
            let mut entries = Vec::new();
            program.package_json_cache_entries(|_key, value| {
                entries.push(*value);
                true
            });
            // The realpath calls are independent file system lookups, and both lists are sorted below, so they run on
            // the worker pool.
            let resolve = |value: &P<tsrs_module::packagejson::InfoCacheEntry>| {
                let mut package_json =
                    tspath::combine_paths(value.package_directory, &["package.json"]);
                if value.exists() || value.directory_exists {
                    package_json = program.host().fs().realpath(&package_json);
                }
                (value.exists(), package_json)
            };
            let resolved: Vec<(bool, String)> = if program.single_threaded() {
                entries.iter().map(resolve).collect()
            } else {
                use rayon::prelude::*;
                tsrs_compiler::worker_pool().install(|| entries.par_iter().map(resolve).collect())
            };
            (missing_package_jsons, package_jsons) = resolved.into_iter().fold(
                (missing_package_jsons, package_jsons),
                |(mut missing_package_jsons, mut package_jsons), (exists, package_json)| {
                    if exists {
                        package_jsons.push(package_json);
                    } else if package_json.contains("/node_modules/") {
                        missing_package_jsons.push(package_json);
                    }

                    (missing_package_jsons, package_jsons)
                },
            );
        }
        *self.snapshot.package_jsons.borrow_mut() = Some(normalize_package_jsons(package_jsons));
        *self.snapshot.missing_package_jsons.borrow_mut() =
            Some(normalize_package_jsons(missing_package_jsons));
    }

    // program.go:478
    pub fn package_json_lookup_paths(&self) -> Vec<String> {
        let program = self.p();
        let config = tspath::get_directory_path(program.command_line().config_name());
        if config.is_empty() {
            return Vec::new();
        }

        let mut package_jsons = Vec::new();
        program.package_json_cache_entries(|_key, value| {
            let mut package_json =
                tspath::combine_paths(value.package_directory, &["package.json"]);
            if value.exists() || value.directory_exists {
                package_json = program.host().fs().realpath(&package_json);
            }
            package_jsons.push(package_json);
            true
        });
        package_jsons.sort();
        package_jsons.dedup();
        package_jsons
    }
}

// program.go:470
fn normalize_package_jsons(mut package_jsons: Vec<String>) -> Vec<String> {
    package_jsons.sort();
    package_jsons.dedup();
    package_jsons
}

// `&'static Program` (not `P<Program>`: the orphan rule forbids a foreign trait on the foreign `P`).
impl ProgramLike for &'static Program {
    // Options implements compiler.AnyProgram interface.
    fn options(&self) -> P<CompilerOptions> {
        self.snapshot.options()
    }

    // GetSourceFile implements compiler.AnyProgram interface.
    fn get_source_file(&self, path: &str) -> Option<P<SourceFile>> {
        self.panic_if_no_program("GetSourceFile");
        self.p().get_source_file(path)
    }

    // GetSourceFiles implements compiler.AnyProgram interface.
    fn get_source_files(&self) -> &'static [P<SourceFile>] {
        self.panic_if_no_program("GetSourceFiles");
        self.p().get_source_files()
    }

    // GetConfigFileParsingDiagnostics implements compiler.AnyProgram interface.
    fn get_config_file_parsing_diagnostics(&self) -> Vec<P<Diagnostic>> {
        self.panic_if_no_program("GetConfigFileParsingDiagnostics");
        self.p().get_config_file_parsing_diagnostics()
    }

    // GetSyntacticDiagnostics implements compiler.AnyProgram interface.
    fn get_syntactic_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        self.panic_if_no_program("GetSyntacticDiagnostics");
        self.p().get_syntactic_diagnostics(ctx, file)
    }

    // GetBindDiagnostics implements compiler.AnyProgram interface.
    fn get_bind_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        self.panic_if_no_program("GetBindDiagnostics");
        self.p().get_bind_diagnostics(ctx, file)
    }

    fn get_program_diagnostics(&self) -> Vec<P<Diagnostic>> {
        self.panic_if_no_program("GetProgramDiagnostics");
        self.p().get_program_diagnostics()
    }

    fn get_global_diagnostics(&self, ctx: &Context) -> Vec<P<Diagnostic>> {
        self.panic_if_no_program("GetGlobalDiagnostics");
        self.p().get_global_diagnostics(ctx)
    }

    // GetSemanticDiagnostics implements compiler.AnyProgram interface.
    fn get_semantic_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        self.get_semantic_diagnostics_impl(ctx, file)
    }

    // GetDeclarationDiagnostics implements compiler.AnyProgram interface.
    fn get_declaration_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        self.panic_if_no_program("GetDeclarationDiagnostics");
        let result = emit_files(
            ctx,
            P::from_static(*self),
            &EmitOptions { target_source_files: file.map(|f| vec![f]), ..Default::default() },
            true,
        );
        match result {
            Some(result) => result.diagnostics,
            None => Vec::new(),
        }
    }

    // GetSuggestionDiagnostics implements compiler.AnyProgram interface.
    fn get_suggestion_diagnostics(
        &self,
        ctx: &Context,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        self.panic_if_no_program("GetSuggestionDiagnostics");
        self.p().get_suggestion_diagnostics(ctx, file) // TODO: incremental suggestion diagnostics (only relevant in editor incremental builder?)
    }

    // program.go:221
    fn emit(&self, ctx: &Context, options: EmitOptions) -> Option<EmitResult> {
        self.panic_if_no_program("Emit");

        let mut result = None;
        if !options.force_emit && options.emit_only != EmitOnly::EmitOnlyBuilderSignature {
            let emit_build_info = || self.emit_build_info(ctx, &options);
            let emit_build_info: Option<&dyn Fn() -> Option<EmitResult>> =
                if self.options().no_emit.is_true() { Some(&emit_build_info) } else { None };
            result = handle_no_emit_options(
                ctx,
                self,
                options.target_source_files.as_deref(),
                emit_build_info,
            );
            if ctx.err().is_some() {
                return None;
            }
        }
        if let Some(mut result) = result {
            if options.target_source_files.is_some() || self.options().no_emit.is_true() {
                return Some(result);
            }

            // Emit buildInfo and combine result
            if let Some(build_info_result) = self.emit_build_info(ctx, &options) {
                result.diagnostics.extend(build_info_result.diagnostics);
                result.emitted_files.extend(build_info_result.emitted_files);
            }
            return Some(result);
        }
        emit_files(ctx, P::from_static(*self), &options, false)
    }

    // CommonSourceDirectory implements compiler.AnyProgram interface.
    fn common_source_directory(&self) -> String {
        self.panic_if_no_program("CommonSourceDirectory");
        self.p().common_source_directory().to_string()
    }

    // IsSourceFileDefaultLibrary implements compiler.AnyProgram interface.
    fn is_source_file_default_library(&self, path: &Path) -> bool {
        self.panic_if_no_program("IsSourceFileDefaultLibrary");
        self.p().is_source_file_default_library(path)
    }

    // Program implements compiler.AnyProgram interface.
    fn program(&self) -> &'static CompilerProgram {
        self.panic_if_no_program("Program");
        self.p()
    }

    fn is_compiler_program(&self) -> bool {
        false
    }
}
