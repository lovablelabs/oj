// Port of execute/incremental/emitfileshandler.go.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{Diagnostic, SourceFile};
use tsrs_compiler::Context;
use tsrs_core::P;
use tsrs_core::tspath::{self, Path};

use crate::affectedfileshandler::collect_all_affected_files;
use crate::emit::{
    EmitOnly, EmitOptions, EmitResult, WriteFileData, combine_emit_results, compiler_program_emit,
};
use crate::program::{Program, SignatureUpdateKind};
use crate::snapshot::{
    DiagnosticsOrBuildInfoDiagnosticsWithFileName, EmitSignature, FileEmitKind,
    get_pending_emit_kind, get_text_handling_source_map_for_signature,
};

struct EmitUpdate {
    pending_kind: FileEmitKind,
    result: Option<EmitResult>,
    dts_errors_from_cache: bool,
}

struct EmitFilesHandler<'a> {
    ctx: &'a Context,
    program: P<Program>,
    is_for_dts_errors: bool,
    signatures: Mutex<FxHashMap<Path, String>>,
    emit_signatures: Mutex<FxHashMap<Path, EmitSignature>>,
    latest_changed_dts_files: Mutex<FxHashMap<Path, String>>,
    deleted_pending_kinds: Mutex<FxHashSet<Path>>,
    emit_updates: Mutex<FxHashMap<Path, EmitUpdate>>,
    has_emit_diagnostics: AtomicBool,
}

impl<'a> EmitFilesHandler<'a> {
    // Determining what all is pending to be emitted based on previous options or previous file emit flags
    // emitfileshandler.go:33
    fn get_pending_emit_kind_for_emit_options(
        &self,
        emit_kind: FileEmitKind,
        options: &EmitOptions,
    ) -> FileEmitKind {
        let mut pending_kind = get_pending_emit_kind(emit_kind, FileEmitKind::None);
        if options.emit_only == EmitOnly::EmitOnlyDts {
            pending_kind &= FileEmitKind::AllDts;
        }
        if self.is_for_dts_errors {
            pending_kind &= FileEmitKind::DtsErrors;
        }
        pending_kind
    }

    // Emits the next affected file's emit result (EmitResult and sourceFiles emitted) or returns undefined if iteration is complete
    // The first of writeFile if provided, writeFile of BuilderProgramHost if provided, writeFile of compiler host
    // in that order would be used to write the files
    // emitfileshandler.go:47
    #[expect(
        clippy::unnecessary_wraps,
        reason = "The incremental emit interface shares an optional result with paths that have no pending emit."
    )]
    fn emit_all_affected_files(&self, options: &EmitOptions) -> Option<EmitResult> {
        let program = self.program.p();
        // Emit all affected files
        if self.program.snapshot.can_use_incremental_state() {
            let results = self.emit_files_incremental(options);
            if self.is_for_dts_errors {
                if let Some(target_source_files) = &options.target_source_files {
                    // Result from cache
                    let result = EmitResult {
                        emit_skipped: true,
                        diagnostics: target_source_files
                            .iter()
                            .flat_map(|&target_file| {
                                // Go calls getDiagnostics on a nil entry, which returns its nil diagnostics.
                                match self
                                    .program
                                    .snapshot
                                    .emit_diagnostics_per_file
                                    .load(target_file.path())
                                {
                                    Some(diagnostics) => {
                                        diagnostics.get_diagnostics(program, Some(target_file))
                                    }
                                    None => Vec::new(),
                                }
                            })
                            .collect(),
                        ..Default::default()
                    };
                    self.update_has_emit_diagnostics(Some(&result));
                    return Some(result);
                }
                for result in &results {
                    self.update_has_emit_diagnostics(Some(result));
                }
                Some(combine_emit_results(results.into_iter().map(Some).collect()))
            } else {
                // Combine results and update buildInfo
                let mut result = combine_emit_results(results.into_iter().map(Some).collect());
                self.update_has_emit_diagnostics(Some(&result));
                self.emit_build_info(options, &mut result);
                Some(result)
            }
        } else if !self.is_for_dts_errors {
            let write_file = self.get_emit_write_file(options);
            let mut result = compiler_program_emit(
                program,
                self.ctx,
                &self.get_emit_options(options, write_file.as_deref()),
            )
            .unwrap_or_default();
            self.update_has_emit_diagnostics(Some(&result));
            self.update_snapshot();
            self.emit_build_info(options, &mut result);
            Some(result)
        } else {
            let diagnostics: Vec<P<Diagnostic>> = match &options.target_source_files {
                None => program.get_declaration_diagnostics(self.ctx, None),
                Some(files) => files
                    .iter()
                    .flat_map(|&target_source_file| {
                        program.get_declaration_diagnostics(self.ctx, Some(target_source_file))
                    })
                    .collect(),
            };
            let result = EmitResult { emit_skipped: true, diagnostics, ..Default::default() };
            if !result.diagnostics.is_empty() {
                self.update_has_emit_diagnostics(Some(&result));
                self.program.snapshot.has_emit_diagnostics.set(true);
            }
            Some(result)
        }
    }

    // emitfileshandler.go:105
    fn update_has_emit_diagnostics(&self, result: Option<&EmitResult>) {
        if let Some(result) = result
            && !result.diagnostics.is_empty()
        {
            self.has_emit_diagnostics.store(true, Ordering::Relaxed);
        }
    }

    // emitfileshandler.go:111
    fn emit_build_info(&self, options: &EmitOptions, result: &mut EmitResult) {
        if let Some(build_info_result) = self.program.emit_build_info(self.ctx, options) {
            result.diagnostics.extend(build_info_result.diagnostics);
            result.emitted_files.extend(build_info_result.emitted_files);
        }
    }

    // emitfileshandler.go:119
    fn emit_files_incremental(&self, options: &EmitOptions) -> Vec<EmitResult> {
        let program = self.program.p();
        // Get all affected files
        collect_all_affected_files(self.ctx, &self.program);
        if self.ctx.err().is_some() {
            return Vec::new();
        }

        let mut pending: Vec<(Path, FileEmitKind)> = Vec::new();
        self.program.snapshot.affected_files_pending_emit.range(|path, &emit_kind| {
            pending.push((path.clone(), emit_kind));
            true
        });
        pending.sort_by(|a, b| a.0.cmp(&b.0));
        let mut to_emit: Vec<(Path, FileEmitKind, FileEmitKind, P<SourceFile>, EmitOnly)> =
            Vec::new();
        for (path, emit_kind) in pending {
            let affected_file = program.get_source_file_by_path(&path);
            let Some(affected_file) =
                affected_file.filter(|&f| program.source_file_may_be_emitted(f, false))
            else {
                self.deleted_pending_kinds.lock().unwrap().insert(path);
                continue;
            };
            let pending_kind = self.get_pending_emit_kind_for_emit_options(emit_kind, options);
            if !pending_kind.is_empty() {
                // Determine if we can do partial emit
                let mut emit_only = EmitOnly::EmitAll;
                if pending_kind.intersects(FileEmitKind::AllJs) {
                    emit_only = EmitOnly::EmitOnlyJs;
                }
                if pending_kind.intersects(FileEmitKind::AllDts) {
                    if emit_only == EmitOnly::EmitOnlyJs {
                        emit_only = EmitOnly::EmitAll;
                    } else {
                        emit_only = EmitOnly::EmitOnlyDts;
                    }
                }
                to_emit.push((path, emit_kind, pending_kind, affected_file, emit_only));
            }
        }
        let mut results = self.emit_pending_files(&to_emit, options);
        for (path, emit_kind, pending_kind, _, _) in to_emit {
            let result = results.remove(&path);
            self.update_has_emit_diagnostics(result.as_ref());
            // Update the pendingEmit for the file
            self.emit_updates.lock().unwrap().insert(
                path,
                EmitUpdate {
                    pending_kind: get_pending_emit_kind(emit_kind, pending_kind),
                    result,
                    dts_errors_from_cache: false,
                },
            );
        }
        if self.ctx.err().is_some() {
            return Vec::new();
        }

        // Get updated errors that were not included in affected files emit
        let mut cached: Vec<(Path, std::sync::Arc<DiagnosticsOrBuildInfoDiagnosticsWithFileName>)> =
            Vec::new();
        self.program.snapshot.emit_diagnostics_per_file.range(|path, diagnostics| {
            cached.push((path.clone(), Arc::clone(diagnostics)));
            true
        });
        cached.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, diagnostics) in cached {
            if self.emit_updates.lock().unwrap().contains_key(&path) {
                continue;
            }
            let affected_file = program.get_source_file_by_path(&path);
            let Some(affected_file) =
                affected_file.filter(|&f| program.source_file_may_be_emitted(f, false))
            else {
                self.deleted_pending_kinds.lock().unwrap().insert(path);
                continue;
            };
            let pending_kind =
                self.program.snapshot.affected_files_pending_emit.load(&path).unwrap_or_default();
            self.emit_updates.lock().unwrap().insert(
                path,
                EmitUpdate {
                    pending_kind,
                    result: Some(EmitResult {
                        emit_skipped: true,
                        diagnostics: diagnostics.get_diagnostics(program, Some(affected_file)),
                        ..Default::default()
                    }),
                    dts_errors_from_cache: true,
                },
            );
        }

        self.update_snapshot()
    }

    // Go emits each affected file as its own work group task. tsrs emits the files that share an `emit_only` with one
    // `emit_each_file`, which runs them on their checkers' threads; a call per file would run them one at a time.
    // With noEmit or noEmitOnError the per-file emit decides per file whether to emit (`handle_no_emit_options`), so
    // those keep a call per file.
    fn emit_pending_files(
        &self,
        to_emit: &[(Path, FileEmitKind, FileEmitKind, P<SourceFile>, EmitOnly)],
        options: &EmitOptions<'a>,
    ) -> FxHashMap<Path, EmitResult> {
        let program = self.program.p();
        let mut results = FxHashMap::default();
        if self.is_for_dts_errors {
            for (path, _, _, file, _) in to_emit {
                let result = EmitResult {
                    emit_skipped: true,
                    diagnostics: program.get_declaration_diagnostics(self.ctx, Some(*file)),
                    ..Default::default()
                };
                results.insert(path.clone(), result);
            }
            return results;
        }
        let compiler_options = program.options();
        if compiler_options.no_emit.is_true() || compiler_options.no_emit_on_error.is_true() {
            for (path, _, _, file, emit_only) in to_emit {
                let file_options = EmitOptions {
                    target_source_files: Some(vec![*file]),
                    emit_only: *emit_only,
                    write_file: options.write_file,
                    ..Default::default()
                };
                let write_file = self.get_emit_write_file(&file_options);
                if let Some(result) = compiler_program_emit(
                    program,
                    self.ctx,
                    &self.get_emit_options(&file_options, write_file.as_deref()),
                ) {
                    results.insert(path.clone(), result);
                }
            }
            return results;
        }
        for emit_only in [EmitOnly::EmitAll, EmitOnly::EmitOnlyJs, EmitOnly::EmitOnlyDts] {
            let files: Vec<P<SourceFile>> =
                to_emit.iter().filter(|e| e.4 == emit_only).map(|e| e.3).collect();
            if files.is_empty() {
                continue;
            }
            let file_options = EmitOptions {
                target_source_files: Some(files),
                emit_only,
                write_file: options.write_file,
                ..Default::default()
            };
            let write_file = self.get_emit_write_file(&file_options);
            for (file, result) in program.emit_each_file(
                self.ctx,
                &self.get_emit_options(&file_options, write_file.as_deref()),
            ) {
                results.insert(file.path().clone(), result);
            }
        }
        // A file the emit filtered out (`source_file_may_be_emitted` with forced emits off) gets the empty result of its
        // own call.
        for (path, ..) in to_emit {
            results.entry(path.clone()).or_default();
        }
        results
    }

    // Go's getEmitOptions returns options whose WriteFile closes over the handler; Rust builds the closure
    // separately (it must outlive the returned options).
    fn get_emit_write_file(
        &self,
        options: &EmitOptions<'a>,
    ) -> Option<Box<dyn Fn(&str, &str, &mut WriteFileData) -> Result<(), String> + Sync + '_>> {
        if !self.program.snapshot.options().get_emit_declarations() {
            return None;
        }
        let can_use_incremental_state = self.program.snapshot.can_use_incremental_state();
        let outer_write_file = options.write_file;
        Some(Box::new(
            move |file_name: &str, text: &str, data: &mut WriteFileData| -> Result<(), String> {
                let mut differs_only_in_map = false;
                if tspath::is_declaration_file_name(file_name) && can_use_incremental_state {
                    let source_file = data.source_file.unwrap();
                    let mut emit_signature = String::new();
                    let info = self.program.snapshot.file_infos.load(source_file.path()).unwrap();
                    if info.signature == info.version {
                        let signature = self.program.snapshot.compute_signature_with_diagnostics(
                            source_file,
                            text,
                            data,
                        );
                        // With d.ts diagnostics they are also part of the signature so emitSignature will be different from it since its just hash of d.ts
                        if data.diagnostics.is_empty() {
                            emit_signature.clone_from(&signature);
                        }
                        if signature != info.version {
                            // Update it
                            self.signatures
                                .lock()
                                .unwrap()
                                .insert(source_file.path().clone(), signature);
                        }
                    }

                    // Store d.ts emit hash so later can be compared to check if d.ts has changed.
                    // Currently we do this only for composite projects since these are the only projects that can be referenced by other projects
                    // and would need their d.ts change time in --build mode
                    if self.skip_dts_output_of_composite(
                        source_file,
                        file_name,
                        text,
                        data,
                        emit_signature,
                        &mut differs_only_in_map,
                    ) {
                        return Ok(());
                    }
                }

                let host = self.program.host.as_ref().unwrap();
                let a_time = if differs_only_in_map { host.get_m_time(file_name) } else { None };
                let mut err = match outer_write_file {
                    Some(write_file) => write_file(file_name, text, data),
                    None => self.program.p().host().fs().write_file(file_name, text),
                };
                if err.is_ok() && differs_only_in_map {
                    // Revert the time to original one
                    err = host.set_m_time(file_name, a_time);
                }
                err
            },
        ))
    }

    // emitfileshandler.go:209
    fn get_emit_options<'b>(
        &self,
        options: &EmitOptions<'b>,
        write_file: Option<
            &'b (dyn Fn(&str, &str, &mut WriteFileData) -> Result<(), String> + Sync + 'b),
        >,
    ) -> EmitOptions<'b> {
        if !self.program.snapshot.options().get_emit_declarations() {
            return EmitOptions {
                target_source_files: options.target_source_files.clone(),
                emit_only: options.emit_only,
                force_emit: options.force_emit,
                write_file: options.write_file,
            };
        }
        EmitOptions {
            target_source_files: options.target_source_files.clone(),
            emit_only: options.emit_only,
            force_emit: options.force_emit,
            write_file,
        }
    }

    // Compare to existing computed signature and store it or handle the changes in d.ts map option from before
    // returning undefined means that, we dont need to emit this d.ts file since its contents didnt change
    // emitfileshandler.go:264
    fn skip_dts_output_of_composite(
        &self,
        file: P<SourceFile>,
        output_file_name: &str,
        text: &str,
        data: &mut WriteFileData,
        mut new_signature: String,
        differs_only_in_map: &mut bool,
    ) -> bool {
        if !self.program.snapshot.options().composite.is_true() {
            return false;
        }
        let mut old_signature = String::new();
        let old_signature_format = self.program.snapshot.emit_signatures.load(file.path());
        if let Some(old_signature_format) = &old_signature_format {
            if old_signature_format.signature.is_empty() {
                old_signature.clone_from(
                    &old_signature_format.signature_with_different_options.as_ref().unwrap()[0],
                );
            } else {
                old_signature.clone_from(&old_signature_format.signature);
            }
        }
        if new_signature.is_empty() {
            new_signature = self
                .program
                .snapshot
                .compute_hash(get_text_handling_source_map_for_signature(text, data));
        }
        // Dont write dts files if they didn't change
        if new_signature == old_signature {
            // If the signature was encoded as string the dts map options match so nothing to do
            if old_signature_format.as_ref().is_some_and(|f| f.signature == old_signature) {
                data.skipped_dts_write = true;
                return true;
            }
            // Mark as differsOnlyInMap so that we can reverse the timestamp with --build so that
            // the downstream projects dont detect this as change in d.ts file
            *differs_only_in_map = self.program.snapshot.options().build.is_true();
        } else {
            self.latest_changed_dts_files
                .lock()
                .unwrap()
                .insert(file.path().clone(), output_file_name.to_string());
        }
        self.emit_signatures.lock().unwrap().insert(
            file.path().clone(),
            EmitSignature { signature: new_signature, signature_with_different_options: None },
        );
        false
    }

    // emitfileshandler.go:300
    #[expect(
        clippy::iter_over_hash_type,
        reason = "the three loops store or delete one entry per key of an unordered map; the results below follow the program's file order"
    )]
    fn update_snapshot(&self) -> Vec<EmitResult> {
        let snapshot = &self.program.snapshot;
        if snapshot.can_use_incremental_state() {
            for (file, signature) in self.signatures.lock().unwrap().iter() {
                let mut info = snapshot.file_infos.load(file).unwrap();
                info.signature.clone_from(signature);
                snapshot.file_infos.store(file.clone(), info);
                if let Some(testing_data) = &self.program.testing_data {
                    testing_data
                        .lock()
                        .unwrap()
                        .updated_signature_kinds
                        .insert(file.clone(), SignatureUpdateKind::StoredAtEmit);
                }
                snapshot.build_info_emit_pending.set(true);
            }
            for (file, signature) in self.emit_signatures.lock().unwrap().iter() {
                snapshot.emit_signatures.store(file.clone(), signature.clone());
                snapshot.build_info_emit_pending.set(true);
            }
            for file in self.deleted_pending_kinds.lock().unwrap().iter() {
                snapshot.affected_files_pending_emit.delete(file);
                snapshot.build_info_emit_pending.set(true);
            }
            // Always use correct order when to collect the result
            let mut results = Vec::new();
            let mut emit_updates = self.emit_updates.lock().unwrap();
            for &file in self.program.p().get_source_files() {
                if let Some(latest_changed_dts_file) =
                    self.latest_changed_dts_files.lock().unwrap().get(file.path())
                {
                    snapshot
                        .latest_changed_dts_file
                        .borrow_mut()
                        .clone_from(latest_changed_dts_file);
                    snapshot.build_info_emit_pending.set(true);
                    snapshot.has_changed_dts_file.set(true);
                }
                if let Some(update) = emit_updates.remove(file.path()) {
                    if !update.dts_errors_from_cache {
                        if update.pending_kind.is_empty() {
                            snapshot.affected_files_pending_emit.delete(file.path());
                        } else {
                            snapshot
                                .affected_files_pending_emit
                                .store(file.path().clone(), update.pending_kind);
                        }
                        snapshot.build_info_emit_pending.set(true);
                    }
                    if let Some(result) = update.result {
                        if !result.diagnostics.is_empty() {
                            snapshot.emit_diagnostics_per_file.store(
                                file.path().clone(),
                                DiagnosticsOrBuildInfoDiagnosticsWithFileName::from_diagnostics(
                                    result.diagnostics.clone(),
                                ),
                            );
                        }
                        results.push(result);
                    }
                }
            }
            return results;
        } else if self.has_emit_diagnostics.load(Ordering::Relaxed) {
            snapshot.has_emit_diagnostics.set(true);
        }
        Vec::new()
    }
}

// emitfileshandler.go:341
pub fn emit_files(
    ctx: &Context,
    program: P<Program>,
    options: &EmitOptions,
    is_for_dts_errors: bool,
) -> Option<EmitResult> {
    let emit_handler = EmitFilesHandler {
        ctx,
        program,
        is_for_dts_errors,
        signatures: std::sync::Mutex::default(),
        emit_signatures: std::sync::Mutex::default(),
        latest_changed_dts_files: std::sync::Mutex::default(),
        deleted_pending_kinds: std::sync::Mutex::default(),
        emit_updates: std::sync::Mutex::default(),
        has_emit_diagnostics: AtomicBool::new(false),
    };

    // Single file emit - do direct from program
    if !is_for_dts_errors && options.target_source_files.is_some() {
        let write_file = emit_handler.get_emit_write_file(options);
        let result = compiler_program_emit(
            program.p(),
            ctx,
            &emit_handler.get_emit_options(options, write_file.as_deref()),
        );
        emit_handler.update_has_emit_diagnostics(result.as_ref());
        if ctx.err().is_some() {
            return None;
        }
        emit_handler.update_snapshot();
        return result;
    }

    // Emit only affected files if using builder for emit
    emit_handler.emit_all_affected_files(options)
}
