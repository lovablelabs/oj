// Port of execute/incremental/affectedfileshandler.go.
//
// Go runs the per-file work on work groups and guards the handler state with sync maps; tsrs runs it on the
// calling thread (tsrs_core's WorkGroup is sequential too), in a deterministic order (sorted paths) where Go's
// order is random. The declaration signatures of the files referencing a changed file are computed a level at a
// time with one emit per level, which runs each checker's files on that checker's thread (getFilesAffectedBy).

use std::cell::{Cell, RefCell};
use std::collections::hash_map::Entry;
use std::rc::Rc;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{self as ast, SourceFile, SymbolFlags};
use tsrs_compiler::Context;
use tsrs_core::P;
use tsrs_core::tspath::{self, Path};

use crate::emit::{EmitOnly, EmitOptions, WriteFileData, compiler_program_emit};
use crate::program::{Program, SignatureUpdateKind};
use crate::snapshot::{FileEmitKind, get_file_emit_kind};

type DtsMayChange = RefCell<FxHashMap<Path, FileEmitKind>>;

fn add_file_to_affected_files_pending_emit(
    c: &DtsMayChange,
    file_path: Path,
    emit_kind: FileEmitKind,
) {
    c.borrow_mut().insert(file_path, emit_kind);
}

struct UpdatedSignature {
    signature: String,
    kind: SignatureUpdateKind,
}

struct AffectedFilesHandler<'a> {
    ctx: &'a Context,
    program: &'a Program,
    has_all_files_excluding_default_library_file: Cell<bool>,
    updated_signatures: RefCell<FxHashMap<Path, UpdatedSignature>>,
    dts_may_change: RefCell<Vec<std::rc::Rc<DtsMayChange>>>,
    files_to_remove_diagnostics: RefCell<FxHashSet<Path>>,
    cleaned_diagnostics_of_lib_files: Cell<bool>,
    seen_file_and_references: RefCell<FxHashMap<Path, bool>>,
    // Declaration signatures computed ahead of update_shape_signature by one emit for many files
    // (collect_all_affected_files, a change that affects the global scope).
    precomputed: RefCell<FxHashMap<Path, String>>,
}

impl AffectedFilesHandler<'_> {
    // affectedfileshandler.go:40
    fn get_dts_may_change(
        &self,
        affected_file_path: Path,
        affected_file_emit_kind: FileEmitKind,
    ) -> std::rc::Rc<DtsMayChange> {
        let mut m = FxHashMap::default();
        m.insert(affected_file_path, affected_file_emit_kind);
        let result = std::rc::Rc::new(RefCell::new(m));
        self.dts_may_change.borrow_mut().push(Rc::clone(&result));
        result
    }

    // affectedfileshandler.go:46
    fn is_changed_signature(&self, path: &Path) -> bool {
        // This method is called after updating signatures of that path, so signature is present in updatedSignatures
        // And is already calculated, so no need to lock and unlock mutex on the entry
        let new_signature =
            self.updated_signatures.borrow().get(path).map(|u| u.signature.clone()).unwrap();
        let old_info = self.program.snapshot.file_infos.load(path).unwrap();
        new_signature != old_info.signature
    }

    // affectedfileshandler.go:54
    fn remove_semantic_diagnostics_of(&self, path: Path) {
        self.files_to_remove_diagnostics.borrow_mut().insert(path);
    }

    // affectedfileshandler.go:58
    fn remove_diagnostics_of_library_files(&self) {
        if self.cleaned_diagnostics_of_lib_files.replace(true) {
            return;
        }
        let program = self.program.p();
        for &file in program.get_source_files() {
            if program.is_source_file_default_library(file.path())
                && !program.skip_type_checking(file, true)
            {
                self.remove_semantic_diagnostics_of(file.path().clone());
            }
        }
    }

    // affectedfileshandler.go:68
    fn compute_dts_signature(&self, file: P<SourceFile>) -> String {
        let signature = std::sync::Mutex::new(String::new());
        let done = self.program.begin_nested_emit();
        let program = self.program;
        let write_file =
            |file_name: &str, text: &str, data: &mut WriteFileData| -> Result<(), String> {
                assert!(
                    tspath::is_declaration_file_name(file_name),
                    "File extension for signature expected to be dts, got : {file_name}"
                );
                *signature.lock().unwrap() =
                    program.snapshot.compute_signature_with_diagnostics(file, text, data);
                Ok(())
            };
        compiler_program_emit(
            self.program.p(),
            self.ctx,
            &EmitOptions {
                target_source_files: Some(vec![file]),
                emit_only: EmitOnly::EmitOnlyBuilderSignature,
                write_file: Some(&write_file),
                ..Default::default()
            },
        );
        done();
        signature.into_inner().unwrap()
    }

    // computeDtsSignature for several files with one emit: the emit runs the files of each checker on that checker's
    // thread, in the order given. A file without a declaration output is missing from the result, as
    // computeDtsSignature returns "" for it.
    fn compute_dts_signatures(
        &self,
        files: Vec<P<SourceFile>>,
    ) -> FxHashMap<P<SourceFile>, String> {
        let signatures: std::sync::Mutex<FxHashMap<P<SourceFile>, String>> =
            std::sync::Mutex::new(FxHashMap::default());
        let done = self.program.begin_nested_emit();
        let program = self.program;
        let write_file =
            |file_name: &str, text: &str, data: &mut WriteFileData| -> Result<(), String> {
                assert!(
                    tspath::is_declaration_file_name(file_name),
                    "File extension for signature expected to be dts, got : {file_name}"
                );
                let file = data.source_file.expect("declaration output of a source file");
                let signature =
                    program.snapshot.compute_signature_with_diagnostics(file, text, data);
                signatures.lock().unwrap().insert(file, signature);
                Ok(())
            };
        compiler_program_emit(
            self.program.p(),
            self.ctx,
            &EmitOptions {
                target_source_files: Some(files),
                emit_only: EmitOnly::EmitOnlyBuilderSignature,
                write_file: Some(&write_file),
                ..Default::default()
            },
        );
        done();
        signatures.into_inner().unwrap()
    }

    // Whether update_shape_signature(file, false) computes the file's declaration signature.
    fn needs_dts_signature(&self, file: P<SourceFile>) -> bool {
        !self.updated_signatures.borrow().contains_key(file.path())
            && !file.is_declaration_file()
            && !ast::is_json_source_file(file)
    }

    // affectedfileshandler.go:87
    fn update_shape_signature(
        &self,
        file: P<SourceFile>,
        use_file_version_as_signature: bool,
    ) -> bool {
        self.update_shape_signature_with(file, use_file_version_as_signature, None)
    }

    // `computed`: the file's declaration signature when compute_dts_signatures already computed it.
    fn update_shape_signature_with(
        &self,
        file: P<SourceFile>,
        use_file_version_as_signature: bool,
        computed: Option<String>,
    ) -> bool {
        // If we have cached the result for this file, that means hence forth we should assume file shape is uptodate
        if self.updated_signatures.borrow().contains_key(file.path()) {
            return false;
        }
        self.updated_signatures.borrow_mut().insert(
            file.path().clone(),
            UpdatedSignature { signature: String::new(), kind: SignatureUpdateKind::ComputedDts },
        );

        let info = self.program.snapshot.file_infos.load(file.path()).unwrap();
        let prev_signature = info.signature;
        let mut signature = String::new();
        let mut kind = SignatureUpdateKind::ComputedDts;
        // JSON files have no declaration output from which to compute a shape
        // signature, so use the file version to conservatively invalidate dependents.
        if !file.is_declaration_file()
            && !ast::is_json_source_file(file)
            && !use_file_version_as_signature
        {
            signature = match computed.or_else(|| self.precomputed.borrow_mut().remove(file.path()))
            {
                Some(computed) => computed,
                None => self.compute_dts_signature(file),
            };
        }
        // Default is to use file version as signature
        if signature.is_empty() {
            signature = info.version;
            kind = SignatureUpdateKind::UsedVersion;
        }
        let changed = signature != prev_signature;
        self.updated_signatures
            .borrow_mut()
            .insert(file.path().clone(), UpdatedSignature { signature, kind });
        changed
    }

    // affectedfileshandler.go:115
    fn get_files_affected_by(&self, path: &Path) -> Vec<P<SourceFile>> {
        let Some(file) = self.program.p().get_source_file_by_path(path) else {
            return Vec::new();
        };

        if !self.update_shape_signature(file, false) {
            return vec![file];
        }

        if self.program.snapshot.file_infos.load(file.path()).unwrap().affects_global_scope {
            self.has_all_files_excluding_default_library_file.set(true);
            return self
                .program
                .snapshot
                .get_all_files_excluding_default_library_file(self.program.p(), Some(file))
                .to_vec();
        }

        if self.program.snapshot.options().isolated_modules.is_true() {
            return vec![file];
        }

        // Now we need to if each file in the referencedBy list has a shape change as well.
        // Because if so, its own referencedBy files need to be saved as well to make the
        // emitting result consistent with files on disk.
        //
        // Go walks the referencing files depth first (forEachFileReferencedBy), computing each file's signature in
        // turn. The files it visits are those reachable through files whose signature changed, and whether a file's
        // signature changed does not depend on when it is computed, so the walk here goes level by level and
        // computes each level's signatures with one emit, the checkers in parallel. The visited files and the
        // signatures are the same.
        let mut seen_file_names_map: FxHashMap<Path, Option<P<SourceFile>>> = FxHashMap::default();
        seen_file_names_map.insert(file.path().clone(), Some(file));
        let mut frontier =
            sorted(self.program.snapshot.referenced_map.get_referenced_by(file.path()));
        while !frontier.is_empty() {
            let mut level: Vec<Option<P<SourceFile>>> = Vec::new();
            level.extend(frontier.into_iter().filter_map(|current_path| {
                if let Entry::Vacant(entry) = seen_file_names_map.entry(current_path) {
                    let current_file = self.program.p().get_source_file_by_path(entry.key());
                    entry.insert(current_file);
                    Some(current_file)
                } else {
                    None
                }
            }));
            let to_compute: Vec<P<SourceFile>> =
                level.iter().flatten().copied().filter(|&f| self.needs_dts_signature(f)).collect();
            let mut computed = if to_compute.len() > 1 {
                self.compute_dts_signatures(to_compute)
            } else {
                FxHashMap::default()
            };
            let mut next: Vec<Path> = Vec::new();
            // If the current file is not nil and has a shape change, we need to queue it for processing
            next.extend(
                level
                    .into_iter()
                    .flatten()
                    .filter_map(|current_file| {
                        let signature = if computed.is_empty() {
                            None
                        } else {
                            Some(computed.remove(&current_file).unwrap_or_default())
                        };
                        if self.update_shape_signature_with(current_file, false, signature) {
                            Some(sorted(
                                self.program
                                    .snapshot
                                    .referenced_map
                                    .get_referenced_by(current_file.path()),
                            ))
                        } else {
                            None
                        }
                    })
                    .flatten(),
            );
            frontier = next;
        }
        // Return array of values that needs emit
        seen_file_names_map.into_values().flatten().collect()
    }

    // affectedfileshandler.go:153
    fn for_each_file_referenced_by(
        &self,
        file: P<SourceFile>,
        f: &mut dyn FnMut(Option<P<SourceFile>>, &Path) -> (bool, bool),
    ) -> FxHashMap<Path, Option<P<SourceFile>>> {
        // Now we need to if each file in the referencedBy list has a shape change as well.
        // Because if so, its own referencedBy files need to be saved as well to make the
        // emitting result consistent with files on disk.
        let mut seen_file_names_map: FxHashMap<Path, Option<P<SourceFile>>> = FxHashMap::default();
        // Start with the paths this file was referenced by
        seen_file_names_map.insert(file.path().clone(), Some(file));
        let mut queue = sorted(self.program.snapshot.referenced_map.get_referenced_by(file.path()));
        while let Some(current_path) = queue.pop() {
            if let Entry::Vacant(entry) = seen_file_names_map.entry(current_path) {
                let current_file = self.program.p().get_source_file_by_path(entry.key());
                let entry = entry.insert_entry(current_file);
                let (queue_for_file, fast_return) = f(current_file, entry.key());
                if fast_return {
                    return seen_file_names_map;
                }
                if queue_for_file {
                    queue.extend(sorted(
                        self.program
                            .snapshot
                            .referenced_map
                            .get_referenced_by(current_file.unwrap().path()),
                    ));
                }
            }
        }
        seen_file_names_map
    }

    // Handles semantic diagnostics and dts emit for affectedFile and files, that are referencing modules that export entities from affected file
    // This is because even though js emit doesnt change, dts emit / type used can change resulting in need for dts emit and js change
    // affectedfileshandler.go:183
    fn handle_dts_may_change_of_affected_file(
        &self,
        dts_may_change: &DtsMayChange,
        affected_file: P<SourceFile>,
    ) {
        self.remove_semantic_diagnostics_of(affected_file.path().clone());

        // If affected files is everything except default library, then nothing more to do
        if self.has_all_files_excluding_default_library_file.get() {
            self.remove_diagnostics_of_library_files();
            // When a change affects the global scope, all files are considered to be affected without updating their signature
            // That means when affected file is handled, its signature can be out of date
            // To avoid this, ensure that we update the signature for any affected file in this scenario.
            self.update_shape_signature(affected_file, false);
            return;
        }

        if self.program.snapshot.options().assume_changes_only_affect_direct_dependencies.is_true()
        {
            return;
        }

        // Iterate on referencing modules that export entities from affected file and delete diagnostics and add pending emit
        // If there was change in signature (dts output) for the changed file,
        // then only we need to handle pending file emit
        if !self.program.snapshot.changed_files_set.has(affected_file.path())
            || !self.is_changed_signature(affected_file.path())
        {
            return;
        }

        // At this point affectedFile is actually one of the changed files
        // that has some change in its .d.ts signature.

        // Since isolated modules dont change js files, files affected by change in signature is itself
        // But we need to cleanup semantic diagnostics and queue dts emit for affected files
        if self.program.snapshot.options().isolated_modules.is_true() {
            self.for_each_file_referenced_by(affected_file, &mut |_current_file, current_path| {
                if self.handle_dts_may_change_of_global_scope(
                    dts_may_change,
                    current_path,
                    false, /*invalidateJsFiles*/
                ) {
                    return (false, true);
                }
                self.handle_dts_may_change_of(
                    dts_may_change,
                    current_path,
                    false, /*invalidateJsFiles*/
                );
                if self.is_changed_signature(current_path) {
                    return (true, false);
                }
                (false, false)
            });
        }

        let mut invalidate_js_files = false;
        let mut type_checker = None;
        // If exported const enum, we need to ensure that js files are emitted as well since the const enum value changed
        if let Some(symbol) = affected_file.symbol()
            && let Some(exports) = symbol.exports()
        {
            for (_, exported) in exports.entries() {
                if exported.flags().intersects(SymbolFlags::ConstEnum) {
                    invalidate_js_files = true;
                    break;
                }
                if type_checker.is_none() {
                    type_checker = Some(
                        self.program
                            .p()
                            .get_type_checker_for_file_exclusive(self.ctx, affected_file),
                    );
                }
                let aliased = tsrs_checker::skip_alias(exported, type_checker.as_mut().unwrap());
                if aliased == exported {
                    continue;
                }
                if aliased.flags().intersects(SymbolFlags::ConstEnum)
                    && aliased
                        .declarations()
                        .iter()
                        .any(|&d| ast::get_source_file_of_node(d) == Some(affected_file))
                {
                    invalidate_js_files = true;
                    break;
                }
            }
        }
        drop(type_checker);

        // Go through files that reference affected file and handle dts emit and semantic diagnostics for them and their references
        for file_referencing_changed_file in
            sorted(self.program.snapshot.referenced_map.get_referenced_by(affected_file.path()))
        {
            if self.handle_dts_may_change_of_global_scope(
                dts_may_change,
                &file_referencing_changed_file,
                invalidate_js_files,
            ) {
                return;
            }
            // Since references of changed file = affected files - we would have already handled d.ts emit and semantic diagnostics
            // for those files. Now we need to handle files referencing those affected files to ensure correctness.
            if sorted(
                self.program
                    .snapshot
                    .referenced_map
                    .get_referenced_by(&file_referencing_changed_file),
            )
            .into_iter()
            .any(|file_referencing_affected_file| {
                self.handle_dts_may_change_of_file_and_references(
                    dts_may_change,
                    &file_referencing_affected_file,
                    invalidate_js_files,
                )
            }) {
                return;
            }
        }
    }

    // affectedfileshandler.go:277
    fn handle_dts_may_change_of_file_and_references(
        &self,
        dts_may_change: &DtsMayChange,
        file_path: &Path,
        invalidate_js_files: bool,
    ) -> bool {
        let existing = self.seen_file_and_references.borrow().get(file_path).copied();
        match existing {
            Some(existing) if existing || !invalidate_js_files => return false,
            Some(_) => {
                self.seen_file_and_references.borrow_mut().insert(file_path.clone(), true);
            }
            None => {
                self.seen_file_and_references
                    .borrow_mut()
                    .insert(file_path.clone(), invalidate_js_files);
            }
        }

        if self.handle_dts_may_change_of_global_scope(
            dts_may_change,
            file_path,
            invalidate_js_files,
        ) {
            return true;
        }
        self.handle_dts_may_change_of(dts_may_change, file_path, invalidate_js_files);

        // Remove the diagnostics of files that import this file and
        // any files that are referenced by it (directly or indirectly)
        sorted(self.program.snapshot.referenced_map.get_referenced_by(file_path)).into_iter().any(
            |referencing_file_path| {
                self.handle_dts_may_change_of_file_and_references(
                    dts_may_change,
                    &referencing_file_path,
                    invalidate_js_files,
                )
            },
        )
    }

    // affectedfileshandler.go:299
    fn handle_dts_may_change_of_global_scope(
        &self,
        dts_may_change: &DtsMayChange,
        file_path: &Path,
        invalidate_js_files: bool,
    ) -> bool {
        match self.program.snapshot.file_infos.load(file_path) {
            Some(info) if info.affects_global_scope => {}
            _ => return false,
        }
        // Every file needs to be handled
        for &file in self
            .program
            .snapshot
            .get_all_files_excluding_default_library_file(self.program.p(), None)
        {
            self.handle_dts_may_change_of(dts_may_change, file.path(), invalidate_js_files);
        }
        self.remove_diagnostics_of_library_files();
        true
    }

    // Handle the dts may change, so they need to be added to pending emit if dts emit is enabled,
    // Also we need to make sure signature is updated for these files
    // affectedfileshandler.go:314
    fn handle_dts_may_change_of(
        &self,
        dts_may_change: &DtsMayChange,
        path: &Path,
        invalidate_js_files: bool,
    ) {
        if self.program.snapshot.changed_files_set.has(path) {
            return;
        }
        let Some(file) = self.program.p().get_source_file_by_path(path) else {
            return;
        };
        self.remove_semantic_diagnostics_of(path.clone());
        // Even though the js emit doesnt change and we are already handling dts emit and semantic diagnostics
        // we need to update the signature to reflect correctness of the signature(which is output d.ts emit) of this file
        // This ensures that we dont later during incremental builds considering wrong signature.
        // Eg where this also is needed to ensure that .tsbuildinfo generated by incremental build should be same as if it was first fresh build
        // But we avoid expensive full shape computation, as using file version as shape is enough for correctness.
        self.update_shape_signature(file, true);
        // If not dts emit, nothing more to do
        let options = self.program.snapshot.options();
        if invalidate_js_files {
            add_file_to_affected_files_pending_emit(
                dts_may_change,
                path.clone(),
                get_file_emit_kind(&options),
            );
        } else if options.get_emit_declarations() {
            add_file_to_affected_files_pending_emit(
                dts_may_change,
                path.clone(),
                if options.declaration_map.is_true() {
                    FileEmitKind::AllDts
                } else {
                    FileEmitKind::Dts
                },
            );
        }
    }

    // affectedfileshandler.go:339
    #[expect(
        clippy::iter_over_hash_type,
        reason = "each loop stores, deletes or ORs one entry per key of an unordered map; the end state does not depend on the order"
    )]
    fn update_snapshot(&self) {
        if self.ctx.err().is_some() {
            return;
        }
        let snapshot = &self.program.snapshot;
        for (file_path, update) in self.updated_signatures.borrow().iter() {
            if let Some(mut info) = snapshot.file_infos.load(file_path) {
                info.signature.clone_from(&update.signature);
                snapshot.file_infos.store(file_path.clone(), info);
                if let Some(testing_data) = &self.program.testing_data {
                    testing_data
                        .lock()
                        .unwrap()
                        .updated_signature_kinds
                        .insert(file_path.clone(), update.kind);
                }
            }
        }
        for file in self.files_to_remove_diagnostics.borrow().iter() {
            snapshot.semantic_diagnostics_per_file.delete(file);
        }
        for change in self.dts_may_change.borrow().iter() {
            for (file_path, &emit_kind) in change.borrow().iter() {
                snapshot.add_file_to_affected_files_pending_emit(file_path, emit_kind);
            }
        }
        for key in snapshot.changed_files_set.keys() {
            snapshot.changed_files_set.delete(&key);
        }
        snapshot.build_info_emit_pending.set(true);
    }
}

fn sorted(mut paths: Vec<Path>) -> Vec<Path> {
    paths.sort();
    paths
}

// affectedfileshandler.go:366
pub fn collect_all_affected_files(ctx: &Context, program: &Program) {
    if program.snapshot.changed_files_set.size() == 0 {
        return;
    }

    let handler = AffectedFilesHandler {
        ctx,
        program,
        has_all_files_excluding_default_library_file: Cell::new(false),
        updated_signatures: RefCell::new(FxHashMap::default()),
        dts_may_change: RefCell::new(Vec::new()),
        files_to_remove_diagnostics: RefCell::new(FxHashSet::default()),
        cleaned_diagnostics_of_lib_files: Cell::new(false),
        seen_file_and_references: RefCell::new(FxHashMap::default()),
        precomputed: RefCell::new(FxHashMap::default()),
    };
    let mut result: Vec<P<SourceFile>> = Vec::new();
    let mut seen: FxHashSet<P<SourceFile>> = FxHashSet::default();
    result.extend(
        sorted(program.snapshot.changed_files_set.keys())
            .into_iter()
            .flat_map(|file| handler.get_files_affected_by(&file))
            .filter(|&affected_file| seen.insert(affected_file)),
    );

    if ctx.err().is_some() {
        return;
    }

    // For all the affected files, get all the files that would need to change their dts or js files,
    // update their diagnostics
    let emit_kind = get_file_emit_kind(&program.snapshot.options());
    result.sort_by(|a, b| a.path().clone().cmp(b.path()));
    // When every file is affected, handle_dts_may_change_of_affected_file computes the declaration signature of each
    // one in turn, one emit per file. Compute them with one emit instead: each checker still emits its files in
    // the same (sorted) order, so every checker goes through the same states and the signatures are the same.
    if handler.has_all_files_excluding_default_library_file.get() {
        let to_compute: Vec<P<SourceFile>> =
            result.iter().copied().filter(|&f| handler.needs_dts_signature(f)).collect();
        if to_compute.len() > 1 {
            let mut computed = handler.compute_dts_signatures(to_compute.clone());
            let mut precomputed = handler.precomputed.borrow_mut();
            precomputed.extend(
                to_compute
                    .into_iter()
                    .map(|file| (file.path().clone(), computed.remove(&file).unwrap_or_default())),
            );
        }
    }
    for file in result {
        // remove the cached semantic diagnostics and handle dts emit and js emit if needed
        let dts_may_change = handler.get_dts_may_change(file.path().clone(), emit_kind);
        handler.handle_dts_may_change_of_affected_file(&dts_may_change, file);
    }

    // Update the snapshot with the new state
    handler.update_snapshot();
}
