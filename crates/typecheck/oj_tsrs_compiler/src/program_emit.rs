// `Program.Emit` and its types (compiler/program.go:1845-2016). Kept out of program.rs so the emit code and the
// LSP work on program.rs merge cheaply (docs/EMIT.md section 11).
//
// Threading (docs/EMIT.md section 10): Go queues one task per file on a work group; each task takes the file's
// checker (`newEmitHost`) for the whole emit of that file. Here, with the compiler's own checker pool, the
// transforms run per checker group on the checker threads (files in program order within a group), the way
// `get_declaration_diagnostics` does, and each transformed file is printed and written on the worker pool (Go prints
// while it holds the checker; printing does not use it). Each file's result goes into a slot indexed by its position
// in the emit list, and `combine_emit_results` runs in input order, which is Go's observable order. With an external
// pool each file takes its checker from the pool, as in Go, and is printed after the checker is released.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use tsrs_ast::{Diagnostic, SourceFile};
use tsrs_core::P;
use tsrs_core::arena::Region;

use crate::checkerpool::Context;
use crate::emitter::{EmitOnly, Emitter};
use crate::program::Program;
use crate::programlike::{ProgramLike, get_diagnostics_of_any_program_like};
use tsrs_tsoptions::outputpaths::{self, ForceEmitPaths};

// program.go:1845
#[derive(Default)]
pub struct WriteFileData {
    pub source_map_url_pos: Option<u32>,
    // Go `BuildInfo any`: the `*incremental.BuildInfo` being written (tsrs_incremental::BuildInfo).
    pub build_info: Option<std::sync::Arc<dyn std::any::Any + Send + Sync>>,
    pub diagnostics: Vec<P<Diagnostic>>,
    pub skipped_dts_write: bool,
    pub source_file: Option<P<SourceFile>>,
}

// program.go:1853. Called from the checker threads, hence `Sync`.
pub type WriteFile<'a> = &'a (dyn Fn(&str, &str, &mut WriteFileData) -> Result<(), String> + Sync);

// program.go:1855
#[derive(Default)]
pub struct EmitOptions<'a> {
    pub target_source_files: Option<Vec<P<SourceFile>>>, // Source files to emit. If `None`, emits all files
    pub emit_only: EmitOnly,
    pub force_emit: bool,
    pub write_file: Option<WriteFile<'a>>,
}

// program.go:1862
#[derive(Default, Clone)]
pub struct EmitResult {
    pub emit_skipped: bool,
    pub diagnostics: Vec<P<Diagnostic>>, // Contains declaration emit diagnostics
    pub emitted_files: Vec<String>,      // Array of files the compiler wrote to disk
    pub source_maps: Vec<SourceMapEmitResult>, // Array of sourceMapData if compiler emitted sourcemaps
}

// program.go:1869
#[derive(Default, Clone)]
pub struct SourceMapEmitResult {
    pub input_source_file_names: Vec<String>, // Input source file (which one can use on program to get the file), 1:1 mapping with the sourceMap.sources list
    pub source_map: tsrs_sourcemap::RawSourceMap,
    pub generated_file: String,
}

// tsrs-only: where emit time goes, summed over the threads that emit (`--extendedDiagnostics` rows "Emit: ...").
// The print row includes building the source-map mappings (the printer feeds the generator as it prints); the source
// map row is serializing the map and computing its URL.
#[derive(Clone, Copy)]
pub enum EmitPhase {
    ScriptTransform,
    DeclarationTransform,
    Print,
    SourceMap,
    Write,
}

const EMIT_PHASE_NAMES: [&str; 5] = [
    "Emit: JS transform (thread sum)",
    "Emit: declaration transform (thread sum)",
    "Emit: print (thread sum)",
    "Emit: source map serialize (thread sum)",
    "Emit: write files (thread sum)",
];

// At most this many files are written at a time during emit (Go limits OS writes to 32 at a time, osvfs
// `writeSema`). Printing runs on the whole worker pool, but file creation is largely serialized by the file system
// (APFS: ~15k files/s on the development machine whether 1 or 16 threads create them), and more concurrent writers
// only add kernel lock contention that slows the transforming checker threads (notes/perf-emit.md).
const EMIT_WRITERS: usize = 4;

/// First chunk of a file's emit region (it grows by a quarter of its size at a time). Most files need less: the
/// median file on the 38k-file codebase allocates about 20 KB during emit.
const EMIT_REGION_FIRST_CHUNK: usize = 64 << 10;

/// Transformed files waiting to be printed and written, at most (`Program::emit`). Each keeps its region and its
/// emit contexts' tables. The 38k-file codebase is write-bound and had up to 7,000 files waiting (0.35 GiB); a
/// bound of 256 made emit about 8% slower on vscode (the checker threads stall), 2,048 costs no measurable time
/// and keeps 0.24 GiB of the 0.35 (notes/mem-emit-regions.md).
const EMIT_MAX_PENDING_PRINTS: usize = 2048;

#[derive(Default)]
struct PendingPrints {
    count: Mutex<usize>,
    done: Condvar,
}

impl PendingPrints {
    fn acquire(&self) {
        let mut count = self.count.lock().unwrap();
        while *count >= EMIT_MAX_PENDING_PRINTS {
            count = self.done.wait(count).unwrap();
        }
        *count += 1;
    }

    fn release(&self) {
        *self.count.lock().unwrap() -= 1;
        self.done.notify_one();
    }
}

#[derive(Default)]
pub struct EmitTimes {
    enabled: bool,
    times: [AtomicU64; 5],
    writers: Mutex<usize>,
    writer_done: Condvar,
}

pub struct WritePermit<'a>(&'a EmitTimes);

impl Drop for WritePermit<'_> {
    fn drop(&mut self) {
        *self.0.writers.lock().unwrap() -= 1;
        self.0.writer_done.notify_one();
    }
}

impl EmitTimes {
    pub(crate) fn write_permit(&self) -> WritePermit<'_> {
        let mut writers = self.writers.lock().unwrap();
        while *writers >= EMIT_WRITERS {
            writers = self.writer_done.wait(writers).unwrap();
        }
        *writers += 1;
        WritePermit(self)
    }

    pub(crate) fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    pub(crate) fn add(&self, phase: EmitPhase, start: Option<Instant>) {
        if let Some(start) = start {
            // Relaxed: only accumulates a statistic, read after the emit workers finish.
            self.times[phase as usize]
                .fetch_add(start.elapsed().as_nanos() as u64, Ordering::Relaxed);
        }
    }

    pub(crate) fn time<T>(&self, phase: EmitPhase, f: impl FnOnce() -> T) -> T {
        let start = self.start();
        let result = f();
        self.add(phase, start);
        result
    }

    fn record(&self) {
        if !self.enabled {
            return;
        }
        for (name, nanos) in EMIT_PHASE_NAMES.iter().zip(&self.times) {
            let nanos = nanos.load(Ordering::Relaxed);
            if nanos != 0 {
                tsrs_core::phases::record(name, Duration::from_nanos(nanos));
            }
        }
    }
}

impl Program {
    // program.go:1875
    pub fn emit(&'static self, ctx: &Context, options: &EmitOptions) -> EmitResult {
        if !options.force_emit && options.emit_only != EmitOnly::EmitOnlyBuilderSignature {
            let result =
                handle_no_emit_options(ctx, &self, options.target_source_files.as_deref(), None);
            if let Some(result) = result {
                return result;
            }
        }
        combine_emit_results(
            self.emit_each_file(ctx, options).into_iter().map(|(_, result)| Some(result)).collect(),
        )
    }

    /// The emit of `emit`, with each emitted file's result kept apart (in emit order), and without the `noEmit` /
    /// `noEmitOnError` handling (`handle_no_emit_options`), which the caller applies. The files run on their checkers'
    /// threads, so one call over many files is the parallel equivalent of a call per file (the incremental emit).
    pub fn emit_each_file(
        &'static self,
        ctx: &Context,
        options: &EmitOptions,
    ) -> Vec<(P<SourceFile>, EmitResult)> {
        let new_line = self.options().new_line.get_new_line_character();
        let force_dts_emit = options.emit_only == EmitOnly::EmitOnlyBuilderSignature
            || options.force_emit && options.emit_only == EmitOnly::EmitOnlyDts;
        let force_js_emit = options.force_emit && options.emit_only == EmitOnly::EmitOnlyJs;
        let source_files = crate::emitter::get_source_files_to_emit(
            self,
            options.target_source_files.as_deref(),
            force_dts_emit,
            force_js_emit,
        );

        let results: Vec<Mutex<Option<EmitResult>>> =
            source_files.iter().map(|_| Mutex::new(None)).collect();
        let times =
            EmitTimes { enabled: self.options().diagnostics_enabled(), ..Default::default() };
        let mem_log = crate::emitter::emit_mem_log();
        // Each file's emit runs in a scratch region of its own (notes/mem-emit-regions.md): the transformed trees,
        // the emit context and the node builder's per-request state are allocated there and freed once the file is
        // printed and written; the checker escapes to its own arena (`CheckerSlot::with`), and so do diagnostics
        // and the program's caches (`EmitHost`).
        let transform =
            |c: &mut tsrs_checker::Checker, source_file: P<SourceFile>| -> (Emitter, Region) {
                let outer_before = if mem_log { tsrs_core::ptr::arena_used_bytes() } else { 0 };
                let region = Region::new_scratch(EMIT_REGION_FIRST_CHUNK);
                let e = {
                    let _scratch = region.enter_scratch();
                    let emit_context = tsrs_printer::new_scratch_emit_context();
                    let emit_resolver = c.new_emit_resolver(emit_context);
                    let checker_slot = P::new(tsrs_checker::CheckerSlot::default());
                    let host = crate::emithost::new_emit_host(self, emit_resolver, checker_slot);
                    let paths = outputpaths::get_output_paths_for(
                        source_file,
                        &self.options(),
                        host,
                        ForceEmitPaths {
                            dts: force_dts_emit,
                            js: force_js_emit,
                            declaration_map: options.force_emit
                                && options.emit_only == EmitOnly::EmitOnlyDts,
                        },
                    );
                    let mut e = Emitter {
                        host,
                        emit_only: options.emit_only,
                        emitter_diagnostics: tsrs_ast::DiagnosticsCollection::default(),
                        paths,
                        source_file,
                        emit_result: EmitResult::default(),
                        force_emit: options.force_emit,
                        write_file: options.write_file,
                        times: &times,
                        pending_js: None,
                        declaration_diagnostics: Vec::new(),
                        pending_declaration: None,
                        emit_context,
                    };
                    checker_slot.lend(c, || e.transform());
                    e
                };
                c.forget_scratch_keyed_caches();
                if mem_log {
                    let outer = tsrs_core::ptr::arena_used_bytes() - outer_before;
                    eprintln!(
                        "emitmem\ttransform\t{}\t{}\t{}",
                        source_file.file_name(),
                        region.used_bytes(),
                        outer
                    );
                }
                (e, region)
            };
        let print = |(mut e, region): (Emitter, Region), index: usize| {
            let outer_before = if mem_log { tsrs_core::ptr::arena_used_bytes() } else { 0 };
            let used_before = if mem_log { region.used_bytes() } else { 0 };
            let result = {
                let _scratch = region.enter_scratch();
                // take an unused writer (Go pools them; a fresh writer prints the same text)
                let mut writer = tsrs_printer::new_text_writer(new_line, 0);
                e.print(&mut *writer);
                std::mem::take(&mut e.emit_result)
            };
            if mem_log {
                let outer = tsrs_core::ptr::arena_used_bytes() - outer_before;
                eprintln!(
                    "emitmem\tprint\t{}\t{}\t{}",
                    e.source_file.file_name(),
                    region.used_bytes() - used_before,
                    outer
                );
            }
            // The emitter's own fields are heap values; the region goes last.
            drop(e);
            drop(region);
            *results[index].lock().unwrap() = Some(result);
        };

        match self.compiler_checker_pool() {
            // Printing and writing do not need the checker (`emitter::print`), so they run on the worker pool while
            // the checker threads go on transforming; per file the steps keep Go's order. At most
            // `EMIT_MAX_PENDING_PRINTS` transformed files wait to be printed: writing is the slower stage on large
            // projects, and every waiting file keeps its region.
            Some(pool) if !self.single_threaded() => {
                let pending = PendingPrints::default();
                crate::program::worker_pool().in_place_scope(|scope| {
                    pool.for_each_checker_group_do(&source_files, false, |c, index, file| {
                        let e = transform(c, file);
                        pending.acquire();
                        let (print, pending) = (&print, &pending);
                        scope.spawn(move |_| {
                            print(e, index);
                            pending.release();
                        });
                    });
                });
            }
            Some(pool) => pool.for_each_checker_group_do(&source_files, true, |c, index, file| {
                print(transform(c, file), index);
            }),
            None => {
                for (index, &file) in source_files.iter().enumerate() {
                    let e = {
                        let mut guard = self.get_type_checker_for_file(ctx, file);
                        transform(&mut guard, file)
                    };
                    print(e, index);
                }
            }
        }

        times.record();
        if let (true, Some((in_use, high_water))) = (mem_log, tsrs_core::ptr::reserve_stats()) {
            eprintln!(
                "emitmem\treserve\tin use {} MiB\thigh water {} MiB",
                in_use >> 20,
                high_water >> 20
            );
        }

        // collect results from emit, preserving input order
        source_files
            .into_iter()
            .zip(results)
            .map(|(file, r)| (file, r.into_inner().unwrap().unwrap()))
            .collect()
    }
}

// program.go:1947
pub fn combine_emit_results(results: Vec<Option<EmitResult>>) -> EmitResult {
    let mut result = EmitResult::default();
    for emit_result in results {
        let Some(emit_result) = emit_result else {
            continue; // Skip nil results
        };
        if emit_result.emit_skipped {
            result.emit_skipped = true;
        }
        result.diagnostics.extend(emit_result.diagnostics);
        result.emitted_files.extend(emit_result.emitted_files);
        result.source_maps.extend(emit_result.source_maps);
    }
    result
}

// program.go:1984. The incremental program passes its own `emitBuildInfo`.
pub fn handle_no_emit_options(
    ctx: &Context,
    program: &dyn ProgramLike,
    files: Option<&[P<SourceFile>]>,
    emit_build_info: Option<&dyn Fn() -> Option<EmitResult>>,
) -> Option<EmitResult> {
    if !program.options().no_emit.is_true() {
        if !program.options().no_emit_on_error.is_true() {
            return None; // NoEmit is false and NoEmitOnError is also false, so we can proceed with normal emit
        }

        let diagnostics = get_diagnostics_of_any_program_like(
            ctx,
            program,
            files,
            true,
            &mut |ctx, file| program.get_bind_diagnostics(ctx, file),
            &mut |ctx, file| program.get_semantic_diagnostics(ctx, file),
        );
        if diagnostics.is_empty() {
            return None; // NoEmitOnError is enabled, but no diagnostics were found, so we can proceed with emitting
        }
        return Some(EmitResult { diagnostics, emit_skipped: true, ..Default::default() });
    }
    if files.is_some() {
        return Some(EmitResult { emit_skipped: true, ..Default::default() });
    }
    if let Some(emit_build_info) = emit_build_info {
        let result = emit_build_info();
        if result.is_some() {
            return result;
        }
    }
    Some(EmitResult::default())
}
