// Port of execute/incremental/programtosnapshot.go.

use std::sync::Arc;
use tsrs_ast::{self as ast, Diagnostic, DiagnosticExt, Node, SourceFile, Symbol};
use tsrs_checker::Program as _;
use tsrs_compiler::{Checker, Context, Program as CompilerProgram};
use tsrs_core::P;
use tsrs_core::collections::{Set, SyncMap};
use tsrs_core::tspath::{self, Path};

use crate::program::Program;
use crate::referencemap::RefSet;
use crate::snapshot::{
    BuildInfoDiagnosticWithFileName, DiagnosticsCache,
    DiagnosticsOrBuildInfoDiagnosticsWithFileName, EmitSignature, FileEmitKind, FileInfo, Snapshot,
    get_file_emit_kind, get_pending_emit_kind_with_options, repopulate_diagnostic_chain,
};

// programtosnapshot.go:16
pub fn program_to_snapshot(
    program: &'static CompilerProgram,
    old_program: Option<P<Program>>,
    hash_with_text: bool,
) -> P<Snapshot> {
    if let Some(old_program) = old_program
        && old_program.program.is_some_and(|p| std::ptr::eq(p, program))
    {
        return old_program.snapshot;
    }
    // Recycled: a watch session frees the snapshot it replaces (`Program::drop_replaced`).
    let snapshot = P::new_recycled(Snapshot {
        hash_with_text,
        file_order: program.get_source_files().iter().map(|file| file.path().clone()).collect(),
        ..Default::default()
    });
    snapshot.options.set(Some(program.options()));
    snapshot.check_pending.set(program.options().no_check.is_true());
    let mut to = ToProgramSnapshot {
        program,
        old_program,
        snapshot,
        global_file_removed: false,
        has_deleted_files: false,
        global_scope_lost: false,
    };

    if to.snapshot.can_use_incremental_state() {
        to.reuse_from_old_program();
        to.compute_program_file_changes();
        to.handle_file_delete();
        to.handle_global_scope_change();
        to.handle_pending_emit();
        to.handle_pending_check();
    }
    snapshot
}

// What computeProgramFileChanges' per-file function stores into the snapshot besides the file info and references.
struct FileChange {
    add_to_change_set: bool,
    // The old state had the file affecting the global scope and the new program does not (handle_global_scope_change).
    global_scope_lost: bool,
    emit_diagnostics: Option<DiagnosticsCache>,
    semantic_diagnostics: Option<DiagnosticsCache>,
    emit_signature: Option<EmitSignature>,
}

struct ToProgramSnapshot {
    program: &'static CompilerProgram,
    old_program: Option<P<Program>>,
    snapshot: P<Snapshot>,
    global_file_removed: bool,
    // Files of the old state that the new program does not have (compute_program_file_changes).
    has_deleted_files: bool,
    // A file of the new program lost the global scope its old state had (compute_program_file_changes).
    global_scope_lost: bool,
}

fn for_each_deleted_file(
    old_file_infos: &SyncMap<Path, FileInfo>,
    new_file_infos: &SyncMap<Path, FileInfo>,
    mut on_deleted_file: impl FnMut(&Path, &FileInfo),
) {
    old_file_infos.range(|file_path, old_info| {
        if new_file_infos.load(file_path).is_none() {
            on_deleted_file(file_path, old_info);
        }
        true
    });
}

impl ToProgramSnapshot {
    // programtosnapshot.go:50
    fn reuse_from_old_program(&self) {
        if let Some(old_program) = self.old_program {
            let old = old_program.snapshot;
            if self.snapshot.options().composite.is_true() {
                self.snapshot
                    .latest_changed_dts_file
                    .borrow_mut()
                    .clone_from(&old.latest_changed_dts_file.borrow());
            }
            self.snapshot.build_info_emit_pending.set(old.build_info_emit_pending.get());
            // Copy old snapshot's changed files set
            old.changed_files_set.range(|key| {
                if self.program.get_source_file_by_path(key).is_some() {
                    self.snapshot.changed_files_set.add(key.clone());
                } else {
                    self.snapshot.build_info_emit_pending.set(true);
                }
                true
            });
            old.affected_files_pending_emit.range(|key, &emit_kind| {
                if self.program.get_source_file_by_path(key).is_some() {
                    self.snapshot.affected_files_pending_emit.store(key.clone(), emit_kind);
                } else {
                    self.snapshot.build_info_emit_pending.set(true);
                }
                true
            });
            self.snapshot.has_errors_from_old_state.set(old.has_errors.get());
            self.snapshot.has_semantic_errors_from_old_state.set(old.has_semantic_errors.get());
            self.snapshot
                .package_jsons_from_old_state
                .borrow_mut()
                .clone_from(&old.package_jsons.borrow());
            self.snapshot
                .missing_package_jsons_from_old_state
                .borrow_mut()
                .clone_from(&old.missing_package_jsons.borrow());
        } else {
            self.snapshot.build_info_emit_pending.set(self.snapshot.options().is_incremental());
        }
    }

    // programtosnapshot.go:75
    fn compute_program_file_changes(&mut self) {
        let old = self.old_program.map(|p| p.snapshot);
        let can_copy_semantic_diagnostics = old.is_some_and(|old| {
            !tsrs_tsoptions::compiler_options_affect_semantic_diagnostics(
                Some(&old.options()),
                Some(&self.program.options()),
            )
        });
        // We can only reuse emit signatures (i.e. .d.ts signatures) if the .d.ts file is unchanged,
        // which will eg be depedent on change in options like declarationDir and outDir options are unchanged.
        // We need to look in oldState.compilerOptions, rather than oldCompilerOptions (i.e.we need to disregard useOldState) because
        // oldCompilerOptions can be undefined if there was change in say module from None to some other option
        // which would make useOldState as false since we can now use reference maps that are needed to track what to emit, what to check etc
        // but that option change does not affect d.ts file name so emitSignatures should still be reused.
        let can_copy_emit_signatures = self.snapshot.options().composite.is_true()
            && old.is_some_and(|old| {
                !tsrs_tsoptions::compiler_options_affect_declaration_path(
                    Some(&old.options()),
                    Some(&self.program.options()),
                )
            });
        let copy_declaration_file_diagnostics = can_copy_semantic_diagnostics
            && self.snapshot.options().skip_lib_check.is_true()
                == old.unwrap().options().skip_lib_check.is_true();
        let copy_lib_file_diagnostics = copy_declaration_file_diagnostics
            && self.snapshot.options().skip_default_lib_check.is_true()
                == old.unwrap().options().skip_default_lib_check.is_true();

        let files = self.program.get_source_files();
        let old_options = old.map(|old| old.options());
        let ambient_module_files_by_checker = AmbientModuleFilesByChecker::default();
        // Files of the old state that the new program does not have.
        let mut deleted_files: rustc_hash::FxHashSet<Path> = rustc_hash::FxHashSet::default();
        if let Some(old) = old
            && !self.old_program.and_then(|p| p.program).is_some_and(|old_program| {
                same_file_paths(old_program, self.program, old.file_infos.size())
            })
        {
            deleted_files.extend(old.file_infos.keys().into_iter().filter_map(|path| {
                if self.program.get_source_file_by_path(&path).is_none() {
                    Some(path)
                } else {
                    None
                }
            }));
        }
        self.has_deleted_files = !deleted_files.is_empty();
        // The file of each path in the old compiler program, to reuse the versions of the files the new program shares
        // with it (`reused_version`).
        let old_compiler_program = self.old_program.and_then(|p| p.program);
        let shared_ambient_paths = SharedAmbientPaths::default();
        // tsrs-only: in a program `reuse_program` made from the old state's program, the files it did not reparse keep
        // their references. A file's references are the files declaring the module symbols of its imports and module
        // augmentations, and the files declaring ambient modules, plus the paths of its triple-slash and type
        // references. The module resolution is the old program's, and the reparsed file has the same imports, module
        // augmentation and ambient module names: every module symbol has the same declaring files. Looking up the
        // imports of every file again took 11 ms of a 50 ms one-file watch cycle on a 9.4k-file project.
        let keeps_references = |file: P<SourceFile>| {
            old_compiler_program.is_some_and(|old_program| {
                self.program.is_reuse_of(old_program)
                    && old_program.get_source_file_by_path(file.path()) == Some(file)
            })
        };
        let new_options = self.snapshot.options();
        // programtosnapshot.go:91: Go queues the per-file work on a work group and lets each function store into the
        // snapshot's sync maps. Here the per-file work runs on the program's worker pool and only reads shared state;
        // its results are stored in file order afterwards, so the maps are filled in the same order as a sequential run.
        let compute = |file: P<SourceFile>,
                       checker_references: Option<CheckerReferences>|
         -> (FileInfo, Option<std::sync::Arc<RefSet>>, FileChange) {
            // programtosnapshot.go:94
            let old_file_info = old.and_then(|old| old.file_infos.load(file.path()));
            let version = match reused_version(old_compiler_program, old_file_info.as_ref(), file) {
                Some(version) => version,
                None if !file.content_mapper().is_empty() => self.snapshot.compute_hash(&format!(
                    "{}\x00{}",
                    file.original_text(),
                    file.content_mapper_transform_identity()
                )),
                None => self.snapshot.compute_hash(file.text()),
            };
            let implied_node_format =
                self.program.get_source_file_meta_data(file.path()).implied_node_format;
            let affects_global_scope = file_affects_global_scope(file);
            let mut signature = String::new();
            let old_references = old.and_then(|old| old.referenced_map.get_references(file.path()));
            let new_references = if keeps_references(file) {
                old_references.clone()
            } else {
                let checker_references = checker_references.unwrap_or_else(|| {
                    let mut checker =
                        self.program.get_type_checker_for_file_exclusive(&Context::default(), file);
                    checker_references_of(file, &mut checker, &ambient_module_files_by_checker)
                });
                referenced_files_of(
                    self.program,
                    file,
                    checker_references,
                    old_references.as_ref(),
                    &shared_ambient_paths,
                )
            };
            let mut change = FileChange {
                add_to_change_set: false,
                global_scope_lost: false,
                emit_diagnostics: None,
                semantic_diagnostics: None,
                emit_signature: None,
            };
            if let Some(old) = old {
                if let Some(old_file_info) = old_file_info {
                    change.global_scope_lost =
                        old_file_info.affects_global_scope && !affects_global_scope;
                    if old_file_info.version != version
                        || old_file_info.affects_global_scope != affects_global_scope
                        || old_file_info.implied_node_format != implied_node_format
                    {
                        change.add_to_change_set = true;
                    } else if !new_references.as_ref().is_some_and(|new| {
                        old_references.as_ref().is_some_and(|old| std::sync::Arc::ptr_eq(new, old))
                    }) && new_references.as_deref() != old_references.as_deref()
                    {
                        // Referenced files changed
                        change.add_to_change_set = true;
                    } else if let Some(new_references) = &new_references {
                        // Go ranges over the set and asks, for each path, whether the new program lacks it while the old
                        // state had it; that is membership in deleted_files, which is usually empty.
                        if !deleted_files.is_empty()
                            && new_references
                                .iter()
                                .any(|ref_path| deleted_files.contains(ref_path))
                        {
                            // Referenced file was deleted in the new program
                            change.add_to_change_set = true;
                        }
                    }
                    signature = old_file_info.signature;
                } else {
                    change.add_to_change_set = true;
                }
                // Only this file's own entry in the change set matters, and the parallel phase does not write the set.
                if !change.add_to_change_set && !self.snapshot.changed_files_set.has(file.path()) {
                    if let Some(emit_diagnostics) = old.emit_diagnostics_per_file.load(file.path())
                    {
                        change.emit_diagnostics = Some(repopulate_diagnostics_of_file(
                            &emit_diagnostics,
                            self.program,
                            file,
                        ));
                    }
                    if can_copy_semantic_diagnostics
                        && (!file.is_declaration_file() || copy_declaration_file_diagnostics)
                        && (!self.program.is_source_file_default_library(file.path())
                            || copy_lib_file_diagnostics)
                    {
                        // Unchanged file copy diagnostics
                        if let Some(diagnostics) =
                            old.semantic_diagnostics_per_file.load(file.path())
                        {
                            change.semantic_diagnostics = Some(repopulate_diagnostics_of_file(
                                &diagnostics,
                                self.program,
                                file,
                            ));
                        }
                    }
                }
                if can_copy_emit_signatures
                    && let Some(old_emit_signature) = old.emit_signatures.load(file.path())
                {
                    change.emit_signature = Some(
                        old_emit_signature
                            .get_new_emit_signature(old_options.as_ref().unwrap(), &new_options),
                    );
                }
            } else {
                signature.clone_from(&version);
            }
            (
                FileInfo { version, signature, affects_global_scope, implied_node_format },
                new_references,
                change,
            )
        };
        let results: Vec<(FileInfo, Option<std::sync::Arc<RefSet>>, FileChange)> =
            if self.program.single_threaded() {
                files.iter().map(|&file| compute(file, None)).collect()
            } else {
                use rayon::prelude::*;
                // The checker lookups first, one task per checker that holds its checker for all of its files (in file
                // order) instead of 18 workers taking turns on 4 checker locks per file; then the rest of the per-file
                // work on the worker pool. Without a pool of its own the program locks per file.
                let checker_references: Vec<std::sync::OnceLock<CheckerReferences>> =
                    files.iter().map(|_| std::sync::OnceLock::new()).collect();
                let looked_up: Vec<usize> =
                    (0..files.len()).filter(|&i| !keeps_references(files[i])).collect();
                let looked_up_files: Vec<P<SourceFile>> =
                    looked_up.iter().map(|&i| files[i]).collect();
                let batched =
                    self.program.for_each_checker_group(&looked_up_files, |checker, j, file| {
                        let _ = checker_references[looked_up[j]].set(checker_references_of(
                            file,
                            checker,
                            &ambient_module_files_by_checker,
                        ));
                    });
                let checker_references: Vec<Option<CheckerReferences>> = checker_references
                    .into_iter()
                    .map(|r| if batched { r.into_inner() } else { None })
                    .collect();
                if !batched {
                    // Create the checker pool before the parallel loop: its lazy initialization runs on the worker pool, and a
                    // worker that steals another file's function while initializing would re-enter the pool's OnceLock.
                    if let Some(&first) = files.first() {
                        drop(
                            self.program
                                .get_type_checker_for_file_exclusive(&Context::default(), first),
                        );
                    }
                }
                tsrs_compiler::worker_pool().install(|| {
                    files
                        .par_iter()
                        .zip(checker_references)
                        .map(|(&file, r)| compute(file, r))
                        .collect()
                })
            };

        // Hash the actual virtual slots, not the deduplicated reference sets: an export alias can change edge
        // multiplicities without changing any source. The same lazily initialized table serves diagnostic reporting.
        let canonical_diagnostics = self.program.uses_canonical_diagnostics();
        if canonical_diagnostics {
            let mut hash = xxhash_rust::xxh3::Xxh3::new();
            for &file in files {
                let slot = self.program.tsgo_default_check_slot(file);
                hash.update(&slot.map_or(u32::MAX, |(checker, _)| checker).to_le_bytes());
            }
            let signature = format!("go4-v1:{:032x}", hash.digest128());
            let _ = self.snapshot.canonical_check_signature.set(signature);
        }
        let reporting_policy_changed = old.is_some_and(|old| {
            old.canonical_check_signature.get() != self.snapshot.canonical_check_signature.get()
        });
        if reporting_policy_changed {
            self.snapshot.build_info_emit_pending.set(true);
        }
        // Source/order/option changes can change a merge's declarations even when its slots remain equal.
        // tsrs-only: a changed file moves a report site only by adding or removing a declaration of a merged
        // interface, so it counts when its new or old text declares an interface. Without the old text (a program
        // loaded from build info) every changed file counts. Counting every changed file made a comment edit in
        // a watch recheck every checked file that declares an interface: 690 -> 70 ms per edit on vscode (#391).
        let declares_interface_before_or_after = |file: P<SourceFile>| {
            contains_interface_declaration(file.as_node())
                || match old_compiler_program {
                    Some(old_program) => {
                        old_program.get_source_file_by_path(file.path()).is_some_and(|old_file| {
                            old_file != file && contains_interface_declaration(old_file.as_node())
                        })
                    }
                    None => true,
                }
        };
        let refresh_interface_diagnostics = reporting_policy_changed
            || canonical_diagnostics
                && old.is_some_and(|old| {
                    old.file_order != self.snapshot.file_order
                        || old.options().skip_lib_check != new_options.skip_lib_check
                        || old.options().skip_default_lib_check
                            != new_options.skip_default_lib_check
                        || !deleted_files.is_empty()
                        || files.iter().zip(&results).any(|(&file, (_, _, change))| {
                            change.add_to_change_set && declares_interface_before_or_after(file)
                        })
                });
        for (&file, (info, new_references, mut change)) in files.iter().zip(results) {
            if refresh_interface_diagnostics
                && change.semantic_diagnostics.is_some()
                && !self.program.skip_type_checking(file, false)
                && contains_interface_declaration(file.as_node())
            {
                change.semantic_diagnostics = None;
            }
            if let Some(new_references) = new_references {
                self.snapshot
                    .referenced_map
                    .store_shared_references(file.path().clone(), new_references);
            }
            if old.is_some() {
                self.global_scope_lost |= change.global_scope_lost;
                if change.add_to_change_set {
                    self.snapshot.add_file_to_change_set(file.path().clone());
                }
                if let Some(emit_diagnostics) = change.emit_diagnostics {
                    self.snapshot
                        .emit_diagnostics_per_file
                        .store(file.path().clone(), emit_diagnostics);
                }
                if let Some(diagnostics) = change.semantic_diagnostics {
                    self.snapshot
                        .semantic_diagnostics_per_file
                        .store(file.path().clone(), diagnostics);
                }
                if let Some(emit_signature) = change.emit_signature {
                    self.snapshot.emit_signatures.store(file.path().clone(), emit_signature);
                }
            } else {
                self.snapshot.add_file_to_affected_files_pending_emit(
                    file.path(),
                    get_file_emit_kind(&new_options),
                );
            }
            self.snapshot.file_infos.store(file.path().clone(), info);
        }
    }

    // programtosnapshot.go:160
    fn handle_file_delete(&mut self) {
        // Ranging the old file infos finds nothing when no file was deleted.
        if !self.has_deleted_files {
            return;
        }
        if let Some(old_program) = self.old_program {
            let snapshot = self.snapshot;
            for_each_deleted_file(
                &old_program.snapshot.file_infos,
                &snapshot.file_infos,
                |file_path, old_info| self.handle_deleted_file(file_path, old_info),
            );
            if self.global_file_removed {
                self.invalidate_global_scope();
            }
        }
    }

    fn handle_deleted_file(&mut self, file_path: &Path, old_info: &FileInfo) {
        self.snapshot.build_info_emit_pending.set(true);
        self.snapshot.changed_files_set.delete(file_path);
        self.snapshot.affected_files_pending_emit.delete(file_path);
        self.global_file_removed |= old_info.affects_global_scope;
    }

    fn invalidate_global_scope(&self) {
        self.snapshot.build_info_emit_pending.set(true);
        for &file in self.program.get_source_files() {
            if !self.program.is_source_file_default_library(file.path()) {
                self.snapshot.add_file_to_change_set(file.path().clone());
            } else if !self.program.skip_type_checking(file, true) {
                self.snapshot.semantic_diagnostics_per_file.delete(file.path());
            }
        }
    }

    // programtosnapshot.go:180
    fn handle_global_scope_change(&self) {
        if self.old_program.is_none() || self.global_file_removed {
            return;
        }
        // Go ranges the old file infos for a file that affected the global scope and no longer does; each file's
        // compute function recorded that for its own file (fileChange::global_scope_lost).
        if self.global_scope_lost {
            self.invalidate_global_scope();
        }
    }

    // programtosnapshot.go:203
    fn handle_pending_emit(&self) {
        if let Some(old_program) = self.old_program {
            if self.global_file_removed {
                return;
            }
            // If options affect emit, then we need to do complete emit per compiler options
            // otherwise only the js or dts that needs to emitted because its different from previously emitted options
            let pending_emit_kind = if tsrs_tsoptions::compiler_options_affect_emit(
                Some(&old_program.snapshot.options()),
                Some(&self.snapshot.options()),
            ) {
                get_file_emit_kind(&self.snapshot.options())
            } else {
                get_pending_emit_kind_with_options(
                    &self.snapshot.options(),
                    &old_program.snapshot.options(),
                )
            };
            if pending_emit_kind != FileEmitKind::None {
                // Add all files to affectedFilesPendingEmit since emit changed
                for &file in self.program.get_source_files() {
                    // Add to affectedFilesPending emit only if not changed since any changed file will do full emit
                    if !self.snapshot.changed_files_set.has(file.path()) {
                        self.snapshot.add_file_to_affected_files_pending_emit(
                            file.path(),
                            pending_emit_kind,
                        );
                    }
                }
                self.snapshot.build_info_emit_pending.set(true);
            }
        }
    }

    // programtosnapshot.go:226
    fn handle_pending_check(&self) {
        if let Some(old_program) = self.old_program
            && self.snapshot.semantic_diagnostics_per_file.size()
                != self.program.get_source_files().len()
            && old_program.snapshot.check_pending.get() != self.snapshot.check_pending.get()
        {
            self.snapshot.build_info_emit_pending.set(true);
        }
    }
}

fn contains_interface_declaration(node: P<Node>) -> bool {
    ast::is_interface_declaration(node) || node.for_each_child(&mut contains_interface_declaration)
}

// programtosnapshot.go:235
pub fn file_affects_global_scope(file: P<SourceFile>) -> bool {
    tsrs_binder::bind_source_file(file);
    // if file contains anything that augments to global scope we need to build them as if
    // they are global files as well as module
    if file
        .module_augmentations()
        .iter()
        .any(|&augmentation| ast::is_global_scope_augmentation(augmentation.parent().unwrap()))
    {
        return true;
    }

    if ast::is_external_or_common_js_module(file) || ast::is_json_source_file(file) {
        return false;
    }

    // For script files that contains only ambient external modules, although they are not actually external module files,
    // they can only be consumed via importing elements from them. Regular script files cannot consume them. Therefore,
    // there are no point to rebuild all script files if these special files have changed. However, if any statement
    // in the file is not ambient external module, we treat it as a regular script file.
    file.statements().nodes().iter().any(|&stmt| !ast::is_module_with_string_literal_name(stmt))
}

// Collects the declaring files; referenced_files_of turns them into paths after releasing the checker.
// programtosnapshot.go:259
fn add_referenced_files_from_symbol(
    file: P<SourceFile>,
    referenced_files: &mut Vec<P<SourceFile>>,
    symbol: Option<P<Symbol>>,
) {
    let Some(symbol) = symbol else { return };
    for &declaration in symbol.declarations() {
        let Some(file_of_decl) = ast::get_source_file_of_node(declaration) else {
            continue;
        };
        // A symbol often has several declarations in one file; skipping the repeat saves hashing its path again.
        if file != file_of_decl && referenced_files.last() != Some(&file_of_decl) {
            referenced_files.push(file_of_decl);
        }
    }
}

// Get the module source file and all augmenting files from the import name node from file
// programtosnapshot.go:275
fn add_referenced_files_from_import_literal(
    file: P<SourceFile>,
    referenced_files: &mut Vec<P<SourceFile>>,
    checker: &mut Checker,
    import_name: P<Node>,
) {
    let symbol = checker.get_symbol_at_location_exported(import_name);
    add_referenced_files_from_symbol(file, referenced_files, symbol);
}

// Gets the path to reference file from file name, it could be resolvedPath if present otherwise path
// programtosnapshot.go:281 (addReferencedFileFromFileName, returning the path it adds)
fn referenced_file_path_from_file_name(program: &CompilerProgram, file_name: &str) -> Path {
    let redirect = program.get_parse_file_redirect(file_name);
    if redirect.is_empty() {
        tspath::to_path(
            file_name,
            program.get_current_directory(),
            program.use_case_sensitive_file_names(),
        )
    } else {
        tspath::to_path(
            &redirect,
            program.get_current_directory(),
            program.use_case_sensitive_file_names(),
        )
    }
}

// The files declaring each checker's ambient modules (in order, as a set, and the set of their paths that every
// reference set built from the program shares), keyed by the checker's address; the checkers outlive the loop.
type AmbientModuleFiles = std::sync::Arc<(
    Vec<P<SourceFile>>,
    rustc_hash::FxHashSet<P<SourceFile>>,
    std::sync::Arc<Set<Path>>,
)>;
type AmbientModuleFilesByChecker =
    std::sync::Mutex<rustc_hash::FxHashMap<usize, AmbientModuleFiles>>;

// The part of getReferencedFiles that needs the file's checker: the files declaring the symbols of the file's imports
// and module augmentations, and the files declaring the checker's ambient modules (the file itself included;
// referenced_files_of leaves it out).
struct CheckerReferences {
    import_files: Vec<P<SourceFile>>,
    augmentation_files: Vec<P<SourceFile>>,
    ambient_module_files: AmbientModuleFiles,
}

fn checker_references_of(
    file: P<SourceFile>,
    checker: &mut Checker,
    ambient_module_files_by_checker: &AmbientModuleFilesByChecker,
) -> CheckerReferences {
    let mut import_files: Vec<P<SourceFile>> = Vec::new();
    let mut augmentation_files: Vec<P<SourceFile>> = Vec::new();
    for &import_name in file.imports() {
        add_referenced_files_from_import_literal(file, &mut import_files, checker, import_name);
    }
    // Add module augmentation as references
    for &module_name in file.module_augmentations() {
        if !ast::is_string_literal(module_name) {
            continue;
        }
        add_referenced_files_from_import_literal(
            file,
            &mut augmentation_files,
            checker,
            module_name,
        );
    }
    // From ambient modules. A checker's ambient modules and their declarations are fixed once it is initialized,
    // so their declaring files are collected once per checker and only filtered per file.
    let key = std::ptr::from_ref::<Checker>(&*checker) as usize;
    let declaring_files = ambient_module_files_by_checker.lock().unwrap().get(&key).cloned();
    let ambient_module_files = declaring_files.unwrap_or_else(|| {
        let mut declaring_files = Vec::new();
        let mut seen: rustc_hash::FxHashSet<P<SourceFile>> = rustc_hash::FxHashSet::default();
        declaring_files.extend(
            checker
                .get_ambient_modules()
                .into_iter()
                .flat_map(|ambient_module| ambient_module.declarations().iter().copied())
                .filter_map(ast::get_source_file_of_node)
                .filter(|&file| seen.insert(file)),
        );
        let mut paths = Set::default();
        paths.m.reserve(declaring_files.len());
        paths.m.extend(declaring_files.iter().map(|f| f.path().clone()));
        let declaring_files =
            std::sync::Arc::new((declaring_files, seen, std::sync::Arc::new(paths)));
        ambient_module_files_by_checker.lock().unwrap().insert(key, Arc::clone(&declaring_files));
        declaring_files
    });
    CheckerReferences { import_files, augmentation_files, ambient_module_files }
}

// Gets the referenced files for a file from the program with values for the keys as referenced file's path to be true
// programtosnapshot.go:290
// Go holds the checker for the whole function. Only the symbol lookups need it (checker_references_of); the paths
// are added here without it, in Go's order: imports, triple slash references, type reference directives, module
// augmentations, ambient modules.
//
// When the file's old reference set (`old`) has exactly these paths, the old set is returned instead of a new one:
// an unchanged file then allocates nothing (on the 38k-file codebase every file references the ~390 files declaring
// ambient modules, ~15 M paths in all).
fn referenced_files_of(
    program: &'static CompilerProgram,
    file: P<SourceFile>,
    checker_references: CheckerReferences,
    old: Option<&std::sync::Arc<RefSet>>,
    shared_ambient_paths: &SharedAmbientPaths,
) -> Option<std::sync::Arc<RefSet>> {
    let CheckerReferences { import_files, augmentation_files, ambient_module_files } =
        checker_references;
    let (ambient_files, ambient_file_set, ambient_paths) = &*ambient_module_files;
    let source_file_directory = tspath::get_directory_path(file.file_name());
    // Triple slash references, then type reference directives.
    let mut file_name_paths: Vec<Path> = Vec::new();
    file_name_paths.extend(file.referenced_files().iter().map(|referenced_file| {
        let file_name = tspath::get_normalized_absolute_path(
            &referenced_file.file_name,
            &source_file_directory,
        );
        referenced_file_path_from_file_name(program, &file_name)
    }));
    if let Some(type_refs_in_file) =
        program.get_resolved_type_reference_directives().get(file.path())
    {
        // the paths go into a set, and references_equal does not depend on their order
        file_name_paths.extend(type_refs_in_file.values().filter_map(|type_ref| {
            if type_ref.resolved_file_name.is_empty() {
                None
            } else {
                Some(referenced_file_path_from_file_name(program, type_ref.resolved_file_name))
            }
        }));
    }

    // The ambient module files' paths are the checker's shared part of the set (RefSet); the file's own paths are
    // the rest. The file itself is left out of the shared part (Go filters `file != decl file` there), but stays in
    // its own part when a triple-slash reference names it.
    let file_is_ambient = ambient_file_set.contains(&file);
    let in_shared =
        |path: &Path| ambient_paths.has(path) && !(file_is_ambient && path == file.path());

    if let Some(old) = old {
        // An old set whose shared part has the same paths, minus the same file, is equal when the own parts are: both
        // own parts leave out the rest of the shared paths. Comparing the shared parts once per pair of sets instead
        // of once per file matters when every file shares several hundred ambient module files.
        let same_shared = old.shared().is_some_and(|(old_shared, old_skip)| {
            old_skip == file_is_ambient.then(|| file.path())
                && shared_ambient_paths.same(old_shared, ambient_paths)
        });
        let equal = if same_shared {
            let mut own: Vec<&Path> = import_files
                .iter()
                .chain(&augmentation_files)
                .map(|f| f.path())
                .chain(&file_name_paths)
                .filter(|p| !in_shared(p))
                .collect();
            own.sort_unstable();
            own.dedup();
            own.len() == old.own().len() && own.iter().all(|p| old.own().has(p))
        } else {
            references_equal(
                program,
                file,
                &import_files,
                &augmentation_files,
                ambient_files,
                ambient_file_set,
                &file_name_paths,
                old,
            )
        };
        if equal {
            return Some(Arc::clone(old));
        }
    }

    // We need to use a set here since the code can contain the same import twice,
    // but that will only be one dependency.
    // To avoid invernal conversion, the key of the referencedFiles map must be of type Path
    let mut own: Set<Path> = Set::default();
    own.m.reserve(import_files.len() + file_name_paths.len() + augmentation_files.len());
    own.m.extend(
        import_files
            .into_iter()
            .chain(augmentation_files)
            .filter_map(|f| if in_shared(f.path()) { None } else { Some(f.path().clone()) }),
    );
    own.m.extend(
        file_name_paths
            .into_iter()
            .filter_map(|path| if in_shared(&path) { None } else { Some(path) }),
    );
    let shared_len = ambient_paths.len() - usize::from(file_is_ambient);
    if own.len() + shared_len == 0 {
        return None;
    }
    if ambient_paths.is_empty() {
        return Some(std::sync::Arc::new(RefSet::flat(own)));
    }
    let skip = if file_is_ambient { Some(file.path().clone()) } else { None };
    Some(std::sync::Arc::new(RefSet::split(own, Arc::clone(ambient_paths), skip)))
}

// Whether two shared parts of reference sets (the paths of a checker's ambient module files) have the same paths,
// remembered per pair: the old sets share a few such parts, and each checker of the new program has one.
#[derive(Default)]
struct SharedAmbientPaths(std::sync::Mutex<rustc_hash::FxHashMap<(usize, usize), bool>>);

impl SharedAmbientPaths {
    fn same(&self, a: &Arc<Set<Path>>, b: &Arc<Set<Path>>) -> bool {
        if Arc::ptr_eq(a, b) {
            return true;
        }
        let key = (Arc::as_ptr(a) as usize, Arc::as_ptr(b) as usize);
        if let Some(&same) = self.0.lock().unwrap().get(&key) {
            return same;
        }
        let same = a.len() == b.len() && a.keys().iter().all(|path| b.has(path));
        self.0.lock().unwrap().insert(key, same);
        same
    }
}

// The version of `file` in the old state, when the old compiler program has the same file object at its path (a
// program made by `reuse_program` shares all of them but one): the same text, so the same hash. Hashing the text of
// every file again took a quarter of a one-file watch cycle's CPU on a 9.4k-file project.
// The watch frees the old compiler program only after the new program's snapshot is built (watcher.rs `install_program`).
fn reused_version(
    old_program: Option<&CompilerProgram>,
    old_file_info: Option<&FileInfo>,
    file: P<SourceFile>,
) -> Option<String> {
    let old_file = old_program?.get_source_file_by_path(file.path())?;
    (old_file == file).then(|| old_file_info.map(|info| info.version.clone())).flatten()
}

// Whether `program` has the files of `old_program`, by path and in the same order, and the old state had one file
// info per file: no file of the old state is missing from the new program. A program made by `reuse_program` does.
fn same_file_paths(
    old_program: &CompilerProgram,
    program: &CompilerProgram,
    old_file_info_count: usize,
) -> bool {
    let old_files = old_program.get_source_files();
    let files = program.get_source_files();
    old_files.len() == files.len()
        && files.len() == old_file_info_count
        && old_files.iter().zip(files).all(|(&old, &new)| old == new || old.path() == new.path())
}

// Whether `old` is exactly the set referenced_files_of would build: as large as the number of distinct paths, and
// containing each of them. Distinct program files have distinct paths, and the ambient module files are distinct.
fn references_equal(
    program: &CompilerProgram,
    file: P<SourceFile>,
    import_files: &[P<SourceFile>],
    augmentation_files: &[P<SourceFile>],
    ambient_files: &[P<SourceFile>],
    ambient_file_set: &rustc_hash::FxHashSet<P<SourceFile>>,
    file_name_paths: &[Path],
    old: &RefSet,
) -> bool {
    let file_is_ambient = ambient_file_set.contains(&file);
    let in_ambient = |f: P<SourceFile>| f != file && ambient_file_set.contains(&f);
    // Imported and augmenting files that are not also ambient module files, without repeats.
    let mut other_files: Vec<P<SourceFile>> = Vec::new();
    for &f in import_files.iter().chain(augmentation_files) {
        if !in_ambient(f) && !other_files.contains(&f) {
            other_files.push(f);
        }
    }
    let mut count = ambient_files.len() - usize::from(file_is_ambient) + other_files.len();
    for (i, path) in file_name_paths.iter().enumerate() {
        let is_program_file_path = |f: P<SourceFile>| f.path() == path;
        let names_counted_file = program.get_source_file_by_path(path).is_some_and(|f| {
            is_program_file_path(f) && (in_ambient(f) || other_files.contains(&f))
        });
        if !names_counted_file && !file_name_paths[..i].contains(path) {
            count += 1;
        }
    }
    if count != old.len() {
        return false;
    }
    other_files.iter().all(|f| old.has(f.path()))
        && file_name_paths.iter().all(|path| old.has(path))
        && ambient_files.iter().all(|&f| f == file || old.has(f.path()))
}

// repopulateDiagnosticsOfFile repopulates diagnostic chains that depend on program state.
// When diagnostics are copied from a previous build, their message chains may reference
// stale program state (e.g., resolved module alternate results, package.json scope).
// This function recomputes those chains using the current program's state.
// programtosnapshot.go:339
fn repopulate_diagnostics_of_file(
    diags: &DiagnosticsCache,
    p: &'static CompilerProgram,
    file: P<SourceFile>,
) -> DiagnosticsCache {
    if let Some(diagnostics) = diags.diagnostics() {
        let Some(repopulated) = repopulate_diagnostics_list(&diagnostics, p, file) else {
            return Arc::clone(diags);
        };
        return DiagnosticsOrBuildInfoDiagnosticsWithFileName::from_diagnostics(repopulated);
    }
    // buildInfoDiagnostics will be repopulated via toDiagnostic's repopulateInfo handling
    Arc::clone(diags)
}

// repopulateDiagnosticsList repopulates diagnostic chains in a list of diagnostics.
// Returns nil if no diagnostics needed repopulation (i.e., no changes were made).
// programtosnapshot.go:353
fn repopulate_diagnostics_list(
    diags: &[P<Diagnostic>],
    p: &'static CompilerProgram,
    file: P<SourceFile>,
) -> Option<Vec<P<Diagnostic>>> {
    let mut changed = false;
    let mut result = Vec::with_capacity(diags.len());
    result = diags.iter().fold(result, |mut result, &d| {
        match repopulate_diagnostic_message_chain(d.message_chain(), p, file) {
            Some(repopulated) => {
                let clone = d.clone_diagnostic();
                clone.set_message_chain(&repopulated);
                result.push(clone);
                changed = true;
            }
            None => result.push(d),
        }

        result
    });
    if !changed {
        return None;
    }
    Some(result)
}

// repopulateDiagnosticMessageChain repopulates chains that have repopulate info.
// Returns nil if no changes were made.
// programtosnapshot.go:375
fn repopulate_diagnostic_message_chain(
    chain: &[P<Diagnostic>],
    p: &'static CompilerProgram,
    file: P<SourceFile>,
) -> Option<Vec<P<Diagnostic>>> {
    if chain.is_empty() {
        return None;
    }
    let mut changed = false;
    let mut result = Vec::with_capacity(chain.len());
    for &c in chain {
        if let Some(info) = c.repopulate_info() {
            // Convert to buildInfoDiagnosticWithFileName and repopulate
            let mut b = BuildInfoDiagnosticWithFileName {
                pos: c.pos().unwrap_or(0),
                end: c.end().unwrap_or(0),
                code: c.code(),
                category: c.category(),
                source: c.source().to_string(),
                message_text: c.message_text().to_string(),
                message_key: c.message_key().0.to_string(),
                message_args: c.message_args().to_vec(),
                repopulate_info: Some(info.clone()),
                ..Default::default()
            };
            // Recursively handle nested chains
            b.message_chain.extend(
                c.message_chain().iter().map(|&nested| ast_diag_to_build_info_diag(nested)),
            );
            result.push(repopulate_diagnostic_chain(&b, p, Some(file)));
            changed = true;
        } else {
            // Check nested chains
            match repopulate_diagnostic_message_chain(c.message_chain(), p, file) {
                Some(nested) => {
                    let clone = c.clone_diagnostic();
                    clone.set_message_chain(&nested);
                    result.push(clone);
                    changed = true;
                }
                None => result.push(c),
            }
        }
    }
    if !changed {
        return None;
    }
    Some(result)
}

// programtosnapshot.go:420
fn ast_diag_to_build_info_diag(d: P<Diagnostic>) -> BuildInfoDiagnosticWithFileName {
    let mut b = BuildInfoDiagnosticWithFileName {
        pos: d.pos().unwrap_or(0),
        end: d.end().unwrap_or(0),
        code: d.code(),
        category: d.category(),
        source: d.source().to_string(),
        message_text: d.message_text().to_string(),
        message_key: d.message_key().0.to_string(),
        message_args: d.message_args().to_vec(),
        repopulate_info: d.repopulate_info().cloned(),
        ..Default::default()
    };
    b.message_chain
        .extend(d.message_chain().iter().map(|&nested| ast_diag_to_build_info_diag(nested)));
    b
}
