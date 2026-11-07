use std::sync::{Mutex, OnceLock};

use rustc_hash::FxHashMap;
use tsrs_ast::{self as ast, Diagnostic, DiagnosticsCollection, Node, new_compiler_diagnostic};
use tsrs_core::tspath::Path;
use tsrs_core::{ModuleKind, P};
use tsrs_diagnostics as diagnostics;
use tsrs_tsoptions as tsoptions;

use crate::file_include::{
    self, FileIncludeReason, ReferenceFileLocation, get_referenced_location, is_referenced_file,
};
use crate::processing_diagnostic::{IncludeExplainingDiagnostic, ProcessingDiagnostic};
use crate::program::Program;

#[derive(Default)]
pub struct FileIncludeData {
    pub(crate) file_include_reasons: FxHashMap<Path, Vec<P<FileIncludeReason>>>,
    pub(crate) processing_diagnostics: Vec<ProcessingDiagnostic>,
}

#[derive(Default)]
pub struct IncludeProcessor {
    pub(crate) reason_diagnostics: Mutex<FxHashMap<(P<FileIncludeReason>, bool), P<Diagnostic>>>,
    reason_to_reference_location:
        Mutex<FxHashMap<P<FileIncludeReason>, &'static ReferenceFileLocation>>,
    include_reason_to_related_info: Mutex<FxHashMap<P<FileIncludeReason>, Option<P<Diagnostic>>>>,
    redirect_and_file_format: Mutex<FxHashMap<Path, Option<Vec<P<Diagnostic>>>>>,
    computed_diagnostics: OnceLock<Mutex<DiagnosticsCollection>>,
    compiler_options_syntax: OnceLock<Option<P<Node>>>,
}

impl IncludeProcessor {
    pub(crate) fn get_diagnostics(&self, p: &Program) -> &Mutex<DiagnosticsCollection> {
        self.computed_diagnostics.get_or_init(|| {
            let mut computed = DiagnosticsCollection::default();
            let processing_diagnostics = p.processing_diagnostics();
            if !processing_diagnostics.is_empty() {
                // What this makes (diagnostics, the include processor's caches) goes to the current allocation target,
                // a checker's region when semantic diagnostics ask first: the language server's pool keeps it while
                // the program lives.
                if let Some(region) = tsrs_core::arena::current_region() {
                    region.mark_escaped();
                }
            }
            for d in processing_diagnostics.iter() {
                computed.add(d.to_diagnostic(p));
            }
            drop(processing_diagnostics);
            // Go ranges over both maps too. The collection drops a diagnostic equal to one it has (equal in
            // everything compare_diagnostics looks at) and sorts on read, and that comparator only ties on equal
            // diagnostics, so the order of the adds cannot be seen.
            #[expect(clippy::iter_over_hash_type, reason = "see the comment above")]
            for resolutions in p.resolved_modules.values() {
                for resolved_module in resolutions.values() {
                    for &diag in resolved_module.resolution_diagnostics {
                        computed.add(diag);
                    }
                }
            }
            #[expect(clippy::iter_over_hash_type, reason = "see the comment above")]
            for type_resolutions in p.type_resolutions_in_file.values() {
                for resolved_type_ref in type_resolutions.values() {
                    for &diag in resolved_type_ref.resolution_diagnostics {
                        computed.add(diag);
                    }
                }
            }
            Mutex::new(computed)
        })
    }

    pub(crate) fn get_reference_location(
        &self,
        r: P<FileIncludeReason>,
        program: &Program,
    ) -> &'static ReferenceFileLocation {
        if let Some(existing) = self.reason_to_reference_location.lock().unwrap().get(&r) {
            return existing;
        }
        let loc: &'static ReferenceFileLocation =
            Box::leak(Box::new(get_referenced_location(r, program)));
        self.reason_to_reference_location.lock().unwrap().entry(r).or_insert(loc)
    }

    pub(crate) fn get_compiler_options_object_literal_syntax(
        &self,
        program: &Program,
    ) -> Option<P<Node>> {
        *self.compiler_options_syntax.get_or_init(|| {
            let config_file = program.opts.config.config_file.as_ref()?;
            let compiler_options_property = tsoptions::for_each_ts_config_prop_array(
                Some(config_file.source_file),
                "compilerOptions",
                Some,
            )?;
            let initializer = compiler_options_property.initializer()?;
            if ast::is_object_literal_expression(initializer) { Some(initializer) } else { None }
        })
    }

    pub(crate) fn get_related_info(
        &self,
        r: P<FileIncludeReason>,
        program: &Program,
    ) -> Option<P<Diagnostic>> {
        if let Some(existing) = self.include_reason_to_related_info.lock().unwrap().get(&r) {
            return *existing;
        }
        let related_info = file_include::to_related_info(r, program);
        *self.include_reason_to_related_info.lock().unwrap().entry(r).or_insert(related_info)
    }

    pub(crate) fn explain_redirect_and_implied_format(
        &self,
        program: &Program,
        file_path: &Path,
        to_file_name: &dyn Fn(&str) -> String,
    ) -> Option<Vec<P<Diagnostic>>> {
        if let Some(existing) = self.redirect_and_file_format.lock().unwrap().get(file_path) {
            return existing.clone();
        }
        let redirects_file = program.redirect_files_by_path.get(file_path);
        let mut source_file = None;
        let (file_name, path) = if let Some(redirects_file) = redirects_file {
            (redirects_file.file_name.clone(), redirects_file.path.clone())
        } else {
            let sf = program.get_source_file_by_path(file_path)?;
            source_file = Some(sf);
            (sf.file_name().to_string(), sf.path().clone())
        };
        let mut result: Option<Vec<P<Diagnostic>>> = None;
        let source = program.get_source_of_project_reference_if_output_included(&file_name, &path);
        if source != file_name {
            result.get_or_insert_with(Vec::new).push(new_compiler_diagnostic(
                &diagnostics::FILE_IS_OUTPUT_OF_PROJECT_REFERENCE_SOURCE_0,
                &[&to_file_name(&source)],
            ));
        }

        if let Some(redirects_file) = redirects_file {
            let target_file = program.get_source_file_by_path(&redirects_file.target).unwrap();
            result.get_or_insert_with(Vec::new).push(new_compiler_diagnostic(
                &diagnostics::FILE_REDIRECTS_TO_FILE_0,
                &[&to_file_name(target_file.file_name())],
            ));
        }

        if let Some(source_file) = source_file
            && ast::is_external_or_common_js_module(source_file)
        {
            let meta_data = program.get_source_file_meta_data(&path);
            let package_json = format!("{}/package.json", meta_data.package_json_directory);
            match program.get_implied_node_format_for_emit(source_file) {
                ModuleKind::ES_NEXT => {
                    if meta_data.package_json_type == "module" {
                        result.get_or_insert_with(Vec::new).push(new_compiler_diagnostic(
                                &diagnostics::FILE_IS_ECMA_SCRIPT_MODULE_BECAUSE_0_HAS_FIELD_TYPE_WITH_VALUE_MODULE,
                                &[&to_file_name(&package_json)],
                            ));
                    }
                }
                ModuleKind::COMMON_JS => {
                    if !meta_data.package_json_type.is_empty() {
                        result.get_or_insert_with(Vec::new).push(new_compiler_diagnostic(
                                &diagnostics::FILE_IS_COMMON_JS_MODULE_BECAUSE_0_HAS_FIELD_TYPE_WHOSE_VALUE_IS_NOT_MODULE,
                                &[&to_file_name(&package_json)],
                            ));
                    } else if !meta_data.package_json_directory.is_empty() {
                        result.get_or_insert_with(Vec::new).push(new_compiler_diagnostic(
                                &diagnostics::FILE_IS_COMMON_JS_MODULE_BECAUSE_0_DOES_NOT_HAVE_FIELD_TYPE,
                                &[&to_file_name(&package_json)],
                            ));
                    } else {
                        result.get_or_insert_with(Vec::new).push(new_compiler_diagnostic(
                                &diagnostics::FILE_IS_COMMON_JS_MODULE_BECAUSE_PACKAGE_JSON_WAS_NOT_FOUND,
                                &[],
                            ));
                    }
                }
                _ => {}
            }
        }

        self.redirect_and_file_format
            .lock()
            .unwrap()
            .entry(file_path.clone())
            .or_insert(result)
            .clone()
    }
}

impl FileIncludeData {
    fn add_processing_diagnostic(&mut self, d: ProcessingDiagnostic) {
        self.processing_diagnostics.push(d);
    }

    pub(crate) fn add_processing_diagnostics_for_file_casing(
        &mut self,
        file: Path,
        existing_casing: &str,
        current_casing: &str,
        reason: Option<P<FileIncludeReason>>,
    ) {
        let already_referenced = self
            .file_include_reasons
            .get(&file)
            .is_some_and(|reasons| reasons.iter().any(|r| r.is_referenced_file()));
        if !is_referenced_file(reason) && already_referenced {
            self.add_processing_diagnostic(ProcessingDiagnostic::explaining(IncludeExplainingDiagnostic {
                file: Some(file),
                diagnostic_reason: reason,
                message: &diagnostics::ALREADY_INCLUDED_FILE_NAME_0_DIFFERS_FROM_FILE_NAME_1_ONLY_IN_CASING,
                args: vec![existing_casing.to_string(), current_casing.to_string()],
            }));
        } else {
            self.add_processing_diagnostic(ProcessingDiagnostic::explaining(IncludeExplainingDiagnostic {
                file: Some(file),
                diagnostic_reason: reason,
                message: &diagnostics::FILE_NAME_0_DIFFERS_FROM_ALREADY_INCLUDED_FILE_NAME_1_ONLY_IN_CASING,
                args: vec![current_casing.to_string(), existing_casing.to_string()],
            }));
        }
    }
}
