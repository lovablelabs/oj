use std::fmt::Display;

use tsrs_ast::{
    self as ast, Diagnostic, FileReference, Kind, Node, SourceFile, new_compiler_diagnostic,
    new_diagnostic,
};
use tsrs_core::P;
use tsrs_core::tspath::{self, Path};
use tsrs_diagnostics::{self as diagnostics, Message};
use tsrs_module::PackageId;
use tsrs_tsoptions as tsoptions;

use crate::program::Program;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum FileIncludeKind {
    // References from file
    Import,
    ReferenceFile,
    TypeReferenceDirective,
    LibReferenceDirective,

    RootFile,
    LibFile,
    AutomaticTypeDirectiveFile,
    ContentMapperSupplemental,
}

// fileInclude.go:29
pub struct FileIncludeReason {
    pub(crate) kind: FileIncludeKind,
    pub(crate) index: usize,
    pub(crate) is_default_lib: bool,
    referenced_file: Option<ReferencedFileData>,
    pub(crate) automatic_type_directive: Option<P<AutomaticTypeDirectiveFileData>>,
    // Go `tspath.Path`, set only for a supplemental content-mapper output (an arena handle: one per supplemental file).
    pub(crate) canonical_source_file: Option<P<Path>>,
}

impl FileIncludeReason {
    pub(crate) fn new(kind: FileIncludeKind) -> FileIncludeReason {
        FileIncludeReason {
            kind,
            index: 0,
            is_default_lib: false,
            referenced_file: None,
            automatic_type_directive: None,
            canonical_source_file: None,
        }
    }

    pub(crate) fn new_referenced(
        kind: FileIncludeKind,
        file: &'static str,
        index: Option<usize>,
        synthetic: Option<P<Node>>,
    ) -> P<FileIncludeReason> {
        let mut r = FileIncludeReason::new(kind);
        r.referenced_file = Some(ReferencedFileData { file, index, synthetic });
        P::new(r)
    }
}

pub struct ReferencedFileData {
    // Go `tspath.Path`; shared by every reference from the same file (fileLoader `reason_path`).
    pub(crate) file: &'static str,
    index: Option<usize>,
    synthetic: Option<P<Node>>,
}

pub struct ReferenceFileLocation {
    file: P<SourceFile>,
    node: Option<P<Node>>,
    pub(crate) ref_: Option<P<FileReference>>,
    package_id: PackageId,
    pub(crate) is_synthetic: bool,
}

impl ReferenceFileLocation {
    fn text(&self) -> String {
        if let Some(node) = self.node {
            if ast::node_is_synthesized(node) {
                format!("\"{}\"", node.text())
            } else {
                let text = self.file.text();
                let start = tsrs_scanner::skip_trivia(
                    text,
                    node.pos().expect_source("parsed nodes have source positions"),
                ) as usize;
                let end = node.end().expect_source("parsed nodes have source positions") as usize;
                text[start..end].to_string()
            }
        } else {
            let r = self.ref_.unwrap();
            self.file.text()[r
                .text_range
                .pos()
                .expect_source("file references have source positions")
                as usize
                ..r.text_range.end().expect_source("file references have source positions")
                    as usize]
                .to_string()
        }
    }

    pub(crate) fn diagnostic_at(
        &self,
        message: &'static Message,
        args: &[&dyn Display],
    ) -> P<Diagnostic> {
        if let Some(node) = self.node {
            tsoptions::create_diagnostic_for_node_in_source_file(self.file, node, message, args)
        } else {
            new_diagnostic(Some(self.file), self.ref_.unwrap().text_range, message, args)
        }
    }
}

pub struct AutomaticTypeDirectiveFileData {
    pub(crate) type_reference: String,
    pub(crate) package_id: PackageId,
}

impl FileIncludeReason {
    fn as_index(&self) -> usize {
        self.index
    }

    fn as_lib_file_index(&self) -> Option<usize> {
        if self.is_default_lib { None } else { Some(self.index) }
    }

    pub(crate) fn is_referenced_file(&self) -> bool {
        self.kind <= FileIncludeKind::LibReferenceDirective
    }

    /// tsrs-only: a root file of the program (fileregions.rs: a file with only such reasons is referred to by none).
    pub(crate) fn is_root_file(&self) -> bool {
        self.kind == FileIncludeKind::RootFile
    }

    pub(crate) fn as_referenced_file_data(&self) -> &ReferencedFileData {
        self.referenced_file.as_ref().unwrap()
    }

    fn as_automatic_type_directive_file_data(&self) -> &AutomaticTypeDirectiveFileData {
        self.automatic_type_directive.as_deref().unwrap()
    }
}

pub fn is_referenced_file(r: Option<P<FileIncludeReason>>) -> bool {
    r.is_some_and(|r| r.is_referenced_file())
}

pub fn get_referenced_location(
    r: P<FileIncludeReason>,
    program: &Program,
) -> ReferenceFileLocation {
    let ref_ = r.as_referenced_file_data();
    let file = program.get_source_file_by_path_str(ref_.file).unwrap();
    match r.kind {
        FileIncludeKind::Import => {
            let mut specifier: Option<P<Node>> = None;
            let mut is_synthetic = false;
            let imports = file.imports();
            if let Some(synthetic) = ref_.synthetic {
                specifier = Some(synthetic);
                is_synthetic = true;
            } else if ref_.index.is_some_and(|index| index < imports.len()) {
                specifier = Some(imports[ref_.index.unwrap()]);
            } else {
                let mut aug_index = imports.len();
                for &imp in file.module_augmentations.get() {
                    if imp.kind() == Kind::StringLiteral {
                        if Some(aug_index) == ref_.index {
                            specifier = Some(imp);
                            break;
                        }
                        aug_index += 1;
                    }
                }
            }
            let specifier = specifier.unwrap();
            let package_id = program
                .get_resolved_module_from_module_specifier(file, specifier)
                .map(|r| r.package_id)
                .unwrap_or_default();
            ReferenceFileLocation {
                file,
                node: Some(specifier),
                ref_: None,
                package_id,
                is_synthetic,
            }
        }
        FileIncludeKind::ReferenceFile => ReferenceFileLocation {
            file,
            node: None,
            ref_: Some(
                file.referenced_files.get()
                    [ref_.index.expect("reference-file reasons have an index")],
            ),
            package_id: PackageId::default(),
            is_synthetic: false,
        },
        FileIncludeKind::TypeReferenceDirective => ReferenceFileLocation {
            file,
            node: None,
            ref_: Some(
                file.type_reference_directives.get()
                    [ref_.index.expect("type-reference reasons have an index")],
            ),
            package_id: PackageId::default(),
            is_synthetic: false,
        },
        FileIncludeKind::LibReferenceDirective => ReferenceFileLocation {
            file,
            node: None,
            ref_: Some(
                file.lib_reference_directives.get()
                    [ref_.index.expect("lib-reference reasons have an index")],
            ),
            package_id: PackageId::default(),
            is_synthetic: false,
        },
        _ => panic!("unknown reason: {:?}", r.kind),
    }
}

pub fn to_diagnostic(
    r: P<FileIncludeReason>,
    program: &Program,
    relative_file_name: bool,
) -> P<Diagnostic> {
    let key = (r, relative_file_name);
    if let Some(diagnostic) = program.include_processor.reason_diagnostics.lock().unwrap().get(&key)
    {
        return *diagnostic;
    }
    let diagnostic = compute_diagnostic(r, program, &|file_name: &str| {
        if relative_file_name {
            tspath::get_relative_path_from_directory(
                program.get_current_directory(),
                file_name,
                &program.compare_paths_options,
            )
        } else {
            file_name.to_string()
        }
    });
    *program.include_processor.reason_diagnostics.lock().unwrap().entry(key).or_insert(diagnostic)
}

fn compute_diagnostic(
    r: P<FileIncludeReason>,
    program: &Program,
    to_file_name: &dyn Fn(&str) -> String,
) -> P<Diagnostic> {
    if r.is_referenced_file() {
        return compute_reference_file_diagnostic(r, program, to_file_name);
    }
    match r.kind {
        FileIncludeKind::RootFile => {
            if program.opts.config.config_file.is_some() {
                let config = program.opts.config;
                let file_name = tspath::get_normalized_absolute_path(
                    &config.file_names()[r.as_index()],
                    program.get_current_directory(),
                );
                let matched_file_spec = config.get_matched_file_spec(&file_name);
                if !matched_file_spec.is_empty() {
                    return new_compiler_diagnostic(
                        &diagnostics::PART_OF_FILES_LIST_IN_TSCONFIG_JSON,
                        &[&matched_file_spec, &to_file_name(&file_name)],
                    );
                }
                let (matched_include_spec, is_default_include_spec) =
                    config.get_matched_include_spec(&file_name);
                if matched_include_spec.is_empty() {
                    new_compiler_diagnostic(&diagnostics::ROOT_FILE_SPECIFIED_FOR_COMPILATION, &[])
                } else if is_default_include_spec {
                    new_compiler_diagnostic(&diagnostics::MATCHED_BY_DEFAULT_INCLUDE_PATTERN_ASTERISK_ASTERISK_SLASH_ASTERISK, &[])
                } else {
                    new_compiler_diagnostic(
                        &diagnostics::MATCHED_BY_INCLUDE_PATTERN_0_IN_1,
                        &[&matched_include_spec, &to_file_name(config.config_name())],
                    )
                }
            } else {
                new_compiler_diagnostic(&diagnostics::ROOT_FILE_SPECIFIED_FOR_COMPILATION, &[])
            }
        }
        FileIncludeKind::AutomaticTypeDirectiveFile => {
            let data = r.as_automatic_type_directive_file_data();
            if !program.options().uses_wildcard_types() {
                if data.package_id.name.is_empty() {
                    new_compiler_diagnostic(
                        &diagnostics::ENTRY_POINT_OF_TYPE_LIBRARY_0_SPECIFIED_IN_COMPILER_OPTIONS,
                        &[&data.type_reference],
                    )
                } else {
                    new_compiler_diagnostic(
                        &diagnostics::ENTRY_POINT_OF_TYPE_LIBRARY_0_SPECIFIED_IN_COMPILER_OPTIONS_WITH_PACKAGE_ID_1,
                        &[&data.type_reference, &data.package_id.to_string()],
                    )
                }
            } else if !data.package_id.name.is_empty() {
                new_compiler_diagnostic(
                    &diagnostics::ENTRY_POINT_FOR_IMPLICIT_TYPE_LIBRARY_0_WITH_PACKAGE_ID_1,
                    &[&data.type_reference, &data.package_id.to_string()],
                )
            } else {
                new_compiler_diagnostic(
                    &diagnostics::ENTRY_POINT_FOR_IMPLICIT_TYPE_LIBRARY_0,
                    &[&data.type_reference],
                )
            }
        }
        FileIncludeKind::LibFile => {
            if let Some(index) = r.as_lib_file_index() {
                return new_compiler_diagnostic(
                    &diagnostics::LIBRARY_0_SPECIFIED_IN_COMPILER_OPTIONS,
                    &[&program.options().lib.as_ref().unwrap()[index]],
                );
            }
            let target = program.options().get_emit_script_target().string();
            if target.is_empty() {
                new_compiler_diagnostic(&diagnostics::DEFAULT_LIBRARY, &[])
            } else {
                new_compiler_diagnostic(&diagnostics::DEFAULT_LIBRARY_FOR_TARGET_0, &[&target])
            }
        }
        // fileInclude.go:209
        FileIncludeKind::ContentMapperSupplemental => {
            let canonical = program
                .get_source_file_by_path(r.canonical_source_file.as_deref().unwrap())
                .unwrap();
            new_compiler_diagnostic(
                &diagnostics::SUPPLEMENTAL_VIRTUAL_FILE_PRODUCED_BY_THE_CONTENT_MAPPER_FOR_FILE_0,
                &[&to_file_name(canonical.file_name())],
            )
        }
        _ => panic!("unknown reason: {:?}", r.kind),
    }
}

fn compute_reference_file_diagnostic(
    r: P<FileIncludeReason>,
    program: &Program,
    to_file_name: &dyn Fn(&str) -> String,
) -> P<Diagnostic> {
    let reference_location = program.include_processor.get_reference_location(r, program);
    let reference_text = reference_location.text();
    let file_name = to_file_name(reference_location.file.file_name());
    let package_id = &reference_location.package_id;
    match r.kind {
        FileIncludeKind::Import => {
            if !reference_location.is_synthetic {
                if package_id.name.is_empty() {
                    new_compiler_diagnostic(
                        &diagnostics::IMPORTED_VIA_0_FROM_FILE_1,
                        &[&reference_text, &file_name],
                    )
                } else {
                    new_compiler_diagnostic(
                        &diagnostics::IMPORTED_VIA_0_FROM_FILE_1_WITH_PACKAGE_ID_2,
                        &[&reference_text, &file_name, &package_id.to_string()],
                    )
                }
            } else if program
                .import_helpers_import_specifiers
                .get(reference_location.file.path())
                .is_some_and(|s| Some(*s) == reference_location.node)
            {
                if package_id.name.is_empty() {
                    new_compiler_diagnostic(
                        &diagnostics::IMPORTED_VIA_0_FROM_FILE_1_TO_IMPORT_IMPORT_HELPERS_AS_SPECIFIED_IN_COMPILER_OPTIONS,
                        &[&reference_text, &file_name],
                    )
                } else {
                    new_compiler_diagnostic(
                        &diagnostics::IMPORTED_VIA_0_FROM_FILE_1_WITH_PACKAGE_ID_2_TO_IMPORT_IMPORT_HELPERS_AS_SPECIFIED_IN_COMPILER_OPTIONS,
                        &[&reference_text, &file_name, &package_id.to_string()],
                    )
                }
            } else if !package_id.name.is_empty() {
                new_compiler_diagnostic(
                    &diagnostics::IMPORTED_VIA_0_FROM_FILE_1_WITH_PACKAGE_ID_2_TO_IMPORT_JSX_AND_JSXS_FACTORY_FUNCTIONS,
                    &[&reference_text, &file_name, &package_id.to_string()],
                )
            } else {
                new_compiler_diagnostic(
                    &diagnostics::IMPORTED_VIA_0_FROM_FILE_1_TO_IMPORT_JSX_AND_JSXS_FACTORY_FUNCTIONS,
                    &[&reference_text, &file_name],
                )
            }
        }
        FileIncludeKind::ReferenceFile => new_compiler_diagnostic(
            &diagnostics::REFERENCED_VIA_0_FROM_FILE_1,
            &[&reference_text, &file_name],
        ),
        FileIncludeKind::TypeReferenceDirective => {
            if package_id.name.is_empty() {
                new_compiler_diagnostic(
                    &diagnostics::TYPE_LIBRARY_REFERENCED_VIA_0_FROM_FILE_1,
                    &[&reference_text, &file_name],
                )
            } else {
                new_compiler_diagnostic(
                    &diagnostics::TYPE_LIBRARY_REFERENCED_VIA_0_FROM_FILE_1_WITH_PACKAGE_ID_2,
                    &[&reference_text, &file_name, &package_id.to_string()],
                )
            }
        }
        FileIncludeKind::LibReferenceDirective => new_compiler_diagnostic(
            &diagnostics::LIBRARY_REFERENCED_VIA_0_FROM_FILE_1,
            &[&reference_text, &file_name],
        ),
        _ => panic!("unknown reason: {:?}", r.kind),
    }
}

pub fn to_related_info(r: P<FileIncludeReason>, program: &Program) -> Option<P<Diagnostic>> {
    if r.is_referenced_file() {
        return compute_reference_file_related_info(r, program);
    }
    let config_file = program.opts.config.config_file.as_ref()?;
    let config = program.opts.config;
    let config_source_file = config_file.source_file;
    match r.kind {
        FileIncludeKind::RootFile => {
            let file_name = tspath::get_normalized_absolute_path(
                &config.file_names()[r.as_index()],
                program.get_current_directory(),
            );
            let matched_file_spec = config.get_matched_file_spec(&file_name);
            if matched_file_spec.is_empty() {
                let (matched_include_spec, is_default_include_spec) =
                    config.get_matched_include_spec(&file_name);
                if !matched_include_spec.is_empty()
                    && !is_default_include_spec
                    && let Some(include_node) = tsoptions::get_ts_config_prop_array_element_value(
                        Some(config_source_file),
                        "include",
                        &matched_include_spec,
                    )
                {
                    return Some(tsoptions::create_diagnostic_for_node_in_source_file(
                        config_source_file,
                        include_node,
                        &diagnostics::FILE_IS_MATCHED_BY_INCLUDE_PATTERN_SPECIFIED_HERE,
                        &[],
                    ));
                }
            } else if let Some(files_node) = tsoptions::get_ts_config_prop_array_element_value(
                Some(config_source_file),
                "files",
                &matched_file_spec,
            ) {
                return Some(tsoptions::create_diagnostic_for_node_in_source_file(
                    config_source_file,
                    files_node,
                    &diagnostics::FILE_IS_MATCHED_BY_FILES_LIST_SPECIFIED_HERE,
                    &[],
                ));
            }
        }
        FileIncludeKind::AutomaticTypeDirectiveFile => {
            if !program.options().uses_wildcard_types() {
                let data = r.as_automatic_type_directive_file_data();
                if let Some(types_syntax) = tsoptions::get_options_syntax_by_array_element_value(
                    program.include_processor.get_compiler_options_object_literal_syntax(program),
                    "types",
                    &data.type_reference,
                ) {
                    return Some(tsoptions::create_diagnostic_for_node_in_source_file(
                        config_source_file,
                        types_syntax,
                        &diagnostics::FILE_IS_ENTRY_POINT_OF_TYPE_LIBRARY_SPECIFIED_HERE,
                        &[],
                    ));
                }
            }
        }
        FileIncludeKind::LibFile => {
            if let Some(index) = r.as_lib_file_index() {
                if let Some(lib_syntax) = tsoptions::get_options_syntax_by_array_element_value(
                    program.include_processor.get_compiler_options_object_literal_syntax(program),
                    "lib",
                    &program.options().lib.as_ref().unwrap()[index],
                ) {
                    return Some(tsoptions::create_diagnostic_for_node_in_source_file(
                        config_source_file,
                        lib_syntax,
                        &diagnostics::FILE_IS_LIBRARY_SPECIFIED_HERE,
                        &[],
                    ));
                }
            } else {
                let target = program.options().get_emit_script_target().string();
                if !target.is_empty()
                    && let Some(target_value_syntax) = tsoptions::for_each_property_assignment(
                        program
                            .include_processor
                            .get_compiler_options_object_literal_syntax(program),
                        "target",
                        tsoptions::get_callback_for_finding_property_assignment_by_value(&target),
                        None,
                    )
                {
                    return Some(tsoptions::create_diagnostic_for_node_in_source_file(
                        config_source_file,
                        target_value_syntax,
                        &diagnostics::FILE_IS_DEFAULT_LIBRARY_FOR_TARGET_SPECIFIED_HERE,
                        &[],
                    ));
                }
            }
        }
        // fileInclude.go:293
        FileIncludeKind::ContentMapperSupplemental => return None,
        _ => panic!("unknown reason: {:?}", r.kind),
    }
    None
}

fn compute_reference_file_related_info(
    r: P<FileIncludeReason>,
    program: &Program,
) -> Option<P<Diagnostic>> {
    let reference_location = program.include_processor.get_reference_location(r, program);
    if reference_location.is_synthetic {
        return None;
    }
    Some(match r.kind {
        FileIncludeKind::Import => {
            reference_location.diagnostic_at(&diagnostics::FILE_IS_INCLUDED_VIA_IMPORT_HERE, &[])
        }
        FileIncludeKind::ReferenceFile => {
            reference_location.diagnostic_at(&diagnostics::FILE_IS_INCLUDED_VIA_REFERENCE_HERE, &[])
        }
        FileIncludeKind::TypeReferenceDirective => reference_location
            .diagnostic_at(&diagnostics::FILE_IS_INCLUDED_VIA_TYPE_LIBRARY_REFERENCE_HERE, &[]),
        FileIncludeKind::LibReferenceDirective => reference_location
            .diagnostic_at(&diagnostics::FILE_IS_INCLUDED_VIA_LIBRARY_REFERENCE_HERE, &[]),
        _ => panic!("unknown reason: {:?}", r.kind),
    })
}
