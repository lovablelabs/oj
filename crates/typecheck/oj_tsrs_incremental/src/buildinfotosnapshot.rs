// Port of execute/incremental/buildinfotosnapshot.go.

use std::sync::Arc;
use tsrs_ast::RepopulateDiagnosticInfo;
use tsrs_compiler::CompilerHost;
use tsrs_core::P;
use tsrs_core::collections::new_set_with_size_hint;
use tsrs_core::tspath::{self, Path};
use tsrs_tsoptions::ParsedCommandLine;

use crate::buildinfo::*;
use crate::referencemap::RefSet;
use crate::snapshot::{
    BuildInfoDiagnosticWithFileName, DiagnosticsOrBuildInfoDiagnosticsWithFileName, EmitSignature,
    Snapshot, get_file_emit_kind,
};

// buildinfotosnapshot.go:12
pub fn build_info_to_snapshot(
    build_info: &BuildInfo,
    config: &ParsedCommandLine,
    host: &dyn CompilerHost,
) -> P<Snapshot> {
    let mut to = ToSnapshot {
        build_info,
        build_info_directory: tspath::get_directory_path(&tspath::get_normalized_absolute_path(
            &config.get_build_info_file_name(),
            config.get_current_directory(),
        )),
        snapshot: Snapshot::default(),
        file_paths: Vec::with_capacity(build_info.file_names.len()),
        file_path_set: Vec::with_capacity(build_info.file_ids_list.len()),
    };
    to.file_paths = build_info
        .file_names
        .iter()
        .map(|file_name| {
            if is_build_info_file_name_default_library(file_name) {
                return tspath::to_path(
                    &tspath::combine_paths(host.default_library_path(), &[file_name]),
                    host.get_current_directory(),
                    host.fs().use_case_sensitive_file_names(),
                );
            }
            tspath::to_path(
                file_name,
                &to.build_info_directory,
                config.use_case_sensitive_file_names(),
            )
        })
        .collect();
    to.file_path_set = build_info
        .file_ids_list
        .iter()
        .map(|file_id_list| {
            let mut file_set = new_set_with_size_hint(file_id_list.len());
            file_set.m.extend(file_id_list.iter().map(|&file_id| to.to_file_path(file_id)));
            std::sync::Arc::new(RefSet::flat(file_set))
        })
        .collect();
    to.set_compiler_options();
    to.set_file_info_and_emit_signatures();
    to.snapshot.file_order = to.file_paths[..build_info.file_infos.len()].to_vec();
    if !build_info.tsrs_canonical_check_signature.is_empty() {
        let _ = to
            .snapshot
            .canonical_check_signature
            .set(build_info.tsrs_canonical_check_signature.clone());
    }
    to.set_referenced_map();
    to.set_change_file_set();
    to.set_semantic_diagnostics();
    to.set_emit_diagnostics();
    to.set_affected_files_pending_emit();
    if !build_info.latest_changed_dts_file.is_empty() {
        *to.snapshot.latest_changed_dts_file.borrow_mut() =
            to.to_absolute_path(&build_info.latest_changed_dts_file);
    }
    to.snapshot.has_errors.set(if build_info.errors {
        tsrs_core::Tristate::True
    } else {
        tsrs_core::Tristate::False
    });
    to.snapshot.has_semantic_errors.set(build_info.semantic_errors);
    to.snapshot.check_pending.set(build_info.check_pending);
    to.set_package_jsons();
    P::new(to.snapshot)
}

struct ToSnapshot<'a> {
    build_info: &'a BuildInfo,
    build_info_directory: String,
    snapshot: Snapshot,
    file_paths: Vec<Path>,
    // Go shares one *Set per id list among the files that reference it.
    file_path_set: Vec<std::sync::Arc<RefSet>>,
}

impl ToSnapshot<'_> {
    // buildinfotosnapshot.go:62
    fn to_absolute_path(&self, path: &str) -> String {
        tspath::get_normalized_absolute_path(path, &self.build_info_directory)
    }

    // buildinfotosnapshot.go:66
    fn to_file_path(&self, file_id: BuildInfoFileId) -> Path {
        self.file_paths[file_id as usize - 1].clone()
    }

    // buildinfotosnapshot.go:70
    fn to_file_path_set(&self, file_id_list_id: BuildInfoFileIdListId) -> std::sync::Arc<RefSet> {
        Arc::clone(&self.file_path_set[file_id_list_id as usize - 1])
    }

    // buildinfotosnapshot.go:74
    fn to_build_info_diagnostics_with_file_name(
        &self,
        diagnostics: &[BuildInfoDiagnostic],
    ) -> Vec<BuildInfoDiagnosticWithFileName> {
        diagnostics
            .iter()
            .map(|d| {
                let file = if d.file != 0 { self.to_file_path(d.file) } else { Path::default() };
                BuildInfoDiagnosticWithFileName {
                    file,
                    no_file: d.no_file,
                    pos: d.pos,
                    end: d.end,
                    code: d.code,
                    category: d.category,
                    source: d.source.clone(),
                    message_text: d.message_text.clone(),
                    message_key: d.message_key.clone(),
                    message_args: d.message_args.clone(),
                    message_chain: self.to_build_info_diagnostics_with_file_name(&d.message_chain),
                    related_information: self
                        .to_build_info_diagnostics_with_file_name(&d.related_information),
                    reports_unnecessary: d.reports_unnecessary,
                    reports_deprecated: d.reports_deprecated,
                    skipped_on_no_emit: d.skipped_on_no_emit,
                    repopulate_info: from_build_info_repopulate_info(d.repopulate_info.as_ref()),
                }
            })
            .collect()
    }

    // buildinfotosnapshot.go:101
    fn to_diagnostics_or_build_info_diagnostics_with_file_name(
        &self,
        dig: &BuildInfoDiagnosticsOfFile,
    ) -> std::sync::Arc<DiagnosticsOrBuildInfoDiagnosticsWithFileName> {
        std::sync::Arc::new(DiagnosticsOrBuildInfoDiagnosticsWithFileName {
            diagnostics: std::sync::Mutex::default(),
            build_info_diagnostics: self.to_build_info_diagnostics_with_file_name(&dig.diagnostics),
        })
    }

    // buildinfotosnapshot.go:120
    fn set_compiler_options(&self) {
        self.snapshot
            .options
            .set(Some(P::new(self.build_info.get_compiler_options(&self.build_info_directory))));
    }

    // buildinfotosnapshot.go:124
    fn set_file_info_and_emit_signatures(&self) {
        let is_composite = self.snapshot.options().composite.is_true();
        for (index, build_info_file_info) in self.build_info.file_infos.iter().enumerate() {
            let path = self.to_file_path(index as BuildInfoFileId + 1);
            let info = build_info_file_info.get_file_info();
            // Add default emit signature as file's signature
            if !info.signature.is_empty() && is_composite {
                self.snapshot.emit_signatures.store(
                    path.clone(),
                    EmitSignature {
                        signature: info.signature.clone(),
                        signature_with_different_options: None,
                    },
                );
            }
            self.snapshot.file_infos.store(path, info);
        }
        // Fix up emit signatures
        for value in &self.build_info.emit_signatures {
            if value.no_emit_signature() {
                self.snapshot.emit_signatures.delete(&self.to_file_path(value.file_id));
            } else {
                let path = self.to_file_path(value.file_id);
                let emit_signature =
                    value.to_emit_signature(&path, &self.snapshot.emit_signatures.to_map());
                self.snapshot.emit_signatures.store(path, emit_signature);
            }
        }
    }

    // buildinfotosnapshot.go:145
    fn set_referenced_map(&self) {
        for entry in &self.build_info.referenced_map {
            self.snapshot.referenced_map.store_shared_references(
                self.to_file_path(entry.file_id),
                self.to_file_path_set(entry.file_id_list_id),
            );
        }
    }

    // buildinfotosnapshot.go:151
    fn set_change_file_set(&self) {
        for &file_id in &self.build_info.change_file_set {
            let file_path = self.to_file_path(file_id);
            self.snapshot.changed_files_set.add(file_path);
        }
    }

    // buildinfotosnapshot.go:158
    fn set_semantic_diagnostics(&self) {
        self.snapshot.file_infos.range(|path, _info| {
            // Initialize to have no diagnostics if its not changed file
            if !self.snapshot.changed_files_set.has(path) {
                self.snapshot
                    .semantic_diagnostics_per_file
                    .store(path.clone(), std::sync::Arc::default());
            }
            true
        });
        for diagnostic in &self.build_info.semantic_diagnostics_per_file {
            if diagnostic.file_id != 0 {
                let file_path = self.to_file_path(diagnostic.file_id);
                self.snapshot.semantic_diagnostics_per_file.delete(&file_path); // does not have cached diagnostics
            } else {
                let diagnostics = diagnostic.diagnostics.as_ref().unwrap();
                let file_path = self.to_file_path(diagnostics.file_id);
                self.snapshot.semantic_diagnostics_per_file.store(
                    file_path,
                    self.to_diagnostics_or_build_info_diagnostics_with_file_name(diagnostics),
                );
            }
        }
    }

    // buildinfotosnapshot.go:176
    fn set_emit_diagnostics(&self) {
        for diagnostic in &self.build_info.emit_diagnostics_per_file {
            let file_path = self.to_file_path(diagnostic.file_id);
            self.snapshot.emit_diagnostics_per_file.store(
                file_path,
                self.to_diagnostics_or_build_info_diagnostics_with_file_name(diagnostic),
            );
        }
    }

    // buildinfotosnapshot.go:183
    fn set_affected_files_pending_emit(&self) {
        if self.build_info.affected_files_pending_emit.is_empty() {
            return;
        }
        let own_options_emit_kind = get_file_emit_kind(&self.snapshot.options());
        for pending_emit in &self.build_info.affected_files_pending_emit {
            self.snapshot.affected_files_pending_emit.store(
                self.to_file_path(pending_emit.file_id),
                if pending_emit.emit_kind.is_empty() {
                    own_options_emit_kind
                } else {
                    pending_emit.emit_kind
                },
            );
        }
    }

    // buildinfotosnapshot.go:193
    fn set_package_jsons(&self) {
        *self.snapshot.package_jsons.borrow_mut() = Some(match &self.build_info.package_jsons {
            Some(package_jsons) => package_jsons.iter().map(|p| self.to_absolute_path(p)).collect(),
            None => Vec::new(),
        });
        *self.snapshot.missing_package_jsons.borrow_mut() =
            Some(match &self.build_info.missing_package_jsons {
                Some(missing) => missing.iter().map(|p| self.to_absolute_path(p)).collect(),
                None => Vec::new(),
            });
    }
}

// buildinfotosnapshot.go:108
fn from_build_info_repopulate_info(
    info: Option<&BuildInfoRepopulateInfo>,
) -> Option<RepopulateDiagnosticInfo> {
    let info = info?;
    Some(RepopulateDiagnosticInfo {
        kind: info.kind,
        module_reference: info.module_reference.clone(),
        mode: info.mode,
        package_name: info.package_name.clone(),
    })
}
