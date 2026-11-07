// Port of execute/incremental/snapshot.go.

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};

use tsrs_ast::{
    self as ast, Diagnostic, DiagnosticExt, RepopulateDiagnosticInfo, RepopulateDiagnosticKind,
    SourceFile,
};
use tsrs_compiler::Program as CompilerProgram;
use tsrs_core::collections::{SyncMap, SyncSet};
use tsrs_core::tspath::{self, ComparePathsOptions, Path};
use tsrs_core::{CompilerOptions, P, ResolutionMode, Tristate, alloc_str, new_text_range};
use tsrs_diagnostics::{Category, Key};

use crate::emit::WriteFileData;
use crate::referencemap::ReferenceMap;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileInfo {
    pub version: String,
    pub signature: String,
    pub affects_global_scope: bool,
    pub implied_node_format: ResolutionMode,
}

impl FileInfo {
    pub fn version(&self) -> &str {
        &self.version
    }
    pub fn signature(&self) -> &str {
        &self.signature
    }
    pub fn affects_global_scope(&self) -> bool {
        self.affects_global_scope
    }
    pub fn implied_node_format(&self) -> ResolutionMode {
        self.implied_node_format
    }
}

// snapshot.go:32
pub fn compute_hash(text: &str, hash_with_text: bool) -> String {
    // xxh3.Uint128.Bytes() is big-endian Hi then Lo.
    let hash_bytes = xxhash_rust::xxh3::xxh3_128(text.as_bytes()).to_be_bytes();
    let mut hash = String::with_capacity(32);
    for b in hash_bytes {
        let _ = write!(hash, "{b:02x}");
    }
    if hash_with_text {
        hash += "-";
        hash += text;
    }
    hash
}

bitflags::bitflags! {
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
    pub struct FileEmitKind: u32 {
        const None        = 0;
        const Js          = 1 << 0; // emit js file
        const JsMap       = 1 << 1; // emit js.map file
        const JsInlineMap = 1 << 2; // emit inline source map in js file
        const DtsErrors   = 1 << 3; // emit dts errors
        const DtsEmit     = 1 << 4; // emit d.ts file
        const DtsMap      = 1 << 5; // emit d.ts.map file

        const Dts        = Self::DtsErrors.bits() | Self::DtsEmit.bits();
        const AllJs      = Self::Js.bits() | Self::JsMap.bits() | Self::JsInlineMap.bits();
        const AllDtsEmit = Self::DtsEmit.bits() | Self::DtsMap.bits();
        const AllDts     = Self::Dts.bits() | Self::DtsMap.bits();
        const All        = Self::AllJs.bits() | Self::AllDts.bits();
    }
}

// snapshot.go:62
pub fn get_file_emit_kind(options: &CompilerOptions) -> FileEmitKind {
    let mut result = FileEmitKind::Js;
    if options.source_map.is_true() {
        result |= FileEmitKind::JsMap;
    }
    if options.inline_source_map.is_true() {
        result |= FileEmitKind::JsInlineMap;
    }
    if options.get_emit_declarations() {
        result |= FileEmitKind::Dts;
    }
    if options.declaration_map.is_true() {
        result |= FileEmitKind::DtsMap;
    }
    if options.emit_declaration_only.is_true() {
        result &= FileEmitKind::AllDts;
    }
    result
}

// snapshot.go:82
pub(crate) fn get_pending_emit_kind_with_options(
    options: &CompilerOptions,
    old_options: &CompilerOptions,
) -> FileEmitKind {
    let old_emit_kind = get_file_emit_kind(old_options);
    let new_emit_kind = get_file_emit_kind(options);
    get_pending_emit_kind(new_emit_kind, old_emit_kind)
}

// snapshot.go:88
pub(crate) fn get_pending_emit_kind(
    emit_kind: FileEmitKind,
    old_emit_kind: FileEmitKind,
) -> FileEmitKind {
    if old_emit_kind == emit_kind {
        return FileEmitKind::None;
    }
    if old_emit_kind.is_empty() || emit_kind.is_empty() {
        return emit_kind;
    }
    let diff = old_emit_kind ^ emit_kind;
    let mut result = FileEmitKind::None;
    // If there is diff in Js emit, pending emit is js emit flags
    if diff.intersects(FileEmitKind::AllJs) {
        result |= emit_kind & FileEmitKind::AllJs;
    }
    // If dts errors pending, add dts errors flag
    if diff.intersects(FileEmitKind::DtsErrors) {
        result |= emit_kind & FileEmitKind::AllDts;
    }
    // If there is diff in Dts emit, pending emit is dts emit flags
    if diff.intersects(FileEmitKind::AllDtsEmit) {
        result |= emit_kind & FileEmitKind::AllDtsEmit;
    }
    result
}

// Signature (Hash of d.ts emitted), is string if it was emitted using same d.ts.map option as what compilerOptions indicate,
// otherwise tuple of string
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EmitSignature {
    pub(crate) signature: String,
    pub(crate) signature_with_different_options: Option<Vec<String>>,
}

impl EmitSignature {
    // Covert to Emit signature based on oldOptions and EmitSignature format
    // If d.ts map options differ then swap the format, otherwise use as is
    pub(crate) fn get_new_emit_signature(
        &self,
        old_options: &CompilerOptions,
        new_options: &CompilerOptions,
    ) -> EmitSignature {
        if old_options.declaration_map.is_true() == new_options.declaration_map.is_true() {
            return self.clone();
        }
        match &self.signature_with_different_options {
            None => EmitSignature {
                signature: String::new(),
                signature_with_different_options: Some(vec![self.signature.clone()]),
            },
            Some(with_different_options) => EmitSignature {
                signature: with_different_options[0].clone(),
                signature_with_different_options: None,
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct BuildInfoDiagnosticWithFileName {
    // filename if it is for a File thats other than its stored for
    pub(crate) file: Path,
    pub(crate) no_file: bool,
    pub(crate) pos: u32,
    pub(crate) end: u32,
    pub(crate) code: i32,
    pub(crate) category: Category,
    pub(crate) source: String,
    pub(crate) message_text: String,
    pub(crate) message_key: String,
    pub(crate) message_args: Vec<String>,
    pub(crate) message_chain: Vec<BuildInfoDiagnosticWithFileName>,
    pub(crate) related_information: Vec<BuildInfoDiagnosticWithFileName>,
    pub(crate) reports_unnecessary: bool,
    pub(crate) reports_deprecated: bool,
    pub(crate) skipped_on_no_emit: bool,
    pub(crate) repopulate_info: Option<RepopulateDiagnosticInfo>,
}

impl Default for BuildInfoDiagnosticWithFileName {
    fn default() -> Self {
        BuildInfoDiagnosticWithFileName {
            file: Path::default(),
            no_file: false,
            pos: 0,
            end: 0,
            code: 0,
            category: Category::Warning,
            source: String::new(),
            message_text: String::new(),
            message_key: String::new(),
            message_args: Vec::new(),
            message_chain: Vec::new(),
            related_information: Vec::new(),
            reports_unnecessary: false,
            reports_deprecated: false,
            skipped_on_no_emit: false,
            repopulate_info: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct DiagnosticsOrBuildInfoDiagnosticsWithFileName {
    pub(crate) diagnostics: Mutex<Option<Vec<P<Diagnostic>>>>,
    pub(crate) build_info_diagnostics: Vec<BuildInfoDiagnosticWithFileName>,
}

impl DiagnosticsOrBuildInfoDiagnosticsWithFileName {
    pub(crate) fn from_diagnostics(
        diagnostics: Vec<P<Diagnostic>>,
    ) -> Arc<DiagnosticsOrBuildInfoDiagnosticsWithFileName> {
        Arc::new(DiagnosticsOrBuildInfoDiagnosticsWithFileName {
            diagnostics: Mutex::new(Some(diagnostics)),
            build_info_diagnostics: Vec::new(),
        })
    }

    pub(crate) fn has_diagnostics(&self) -> bool {
        self.diagnostics.lock().unwrap().as_ref().is_some_and(|d| !d.is_empty())
    }

    pub(crate) fn diagnostics(&self) -> Option<Vec<P<Diagnostic>>> {
        self.diagnostics.lock().unwrap().clone()
    }
}

fn key_of(key: &str) -> Key {
    Key(alloc_str(key))
}

impl BuildInfoDiagnosticWithFileName {
    // snapshot.go:150
    fn to_diagnostic(
        &self,
        p: &'static CompilerProgram,
        file: Option<P<SourceFile>>,
    ) -> P<Diagnostic> {
        let file_for_diagnostic = if !self.file.is_empty() {
            p.get_source_file_by_path(&self.file)
        } else if !self.no_file {
            file
        } else {
            None
        };

        if self.repopulate_info.is_some() {
            return repopulate_diagnostic_chain(self, p, file_for_diagnostic);
        }

        let message_chain: Vec<P<Diagnostic>> = self
            .message_chain
            .iter()
            .map(|msg| msg.to_diagnostic(p, file_for_diagnostic))
            .collect();
        let related_information: Vec<P<Diagnostic>> = self
            .related_information
            .iter()
            .map(|info| info.to_diagnostic(p, file_for_diagnostic))
            .collect();
        let diagnostic = ast::new_diagnostic_from_serialized(
            file_for_diagnostic,
            new_text_range(self.pos, self.end),
            self.code,
            self.category,
            key_of(&self.message_key),
            self.message_args.clone(),
            &message_chain,
            &related_information,
            self.reports_unnecessary,
            self.reports_deprecated,
            self.skipped_on_no_emit,
        );
        if !self.source.is_empty() || !self.message_text.is_empty() {
            return diagnostic.set_external_data(&self.source, &self.message_text);
        }
        diagnostic
    }

    // snapshot.go:203
    fn to_diagnostic_without_repopulate(
        &self,
        p: &'static CompilerProgram,
        file: Option<P<SourceFile>>,
    ) -> P<Diagnostic> {
        let message_chain: Vec<P<Diagnostic>> =
            self.message_chain.iter().map(|msg| msg.to_diagnostic(p, file)).collect();
        let related_information: Vec<P<Diagnostic>> =
            self.related_information.iter().map(|info| info.to_diagnostic(p, file)).collect();
        ast::new_diagnostic_from_serialized(
            file,
            new_text_range(self.pos, self.end),
            self.code,
            self.category,
            key_of(&self.message_key),
            self.message_args.clone(),
            &message_chain,
            &related_information,
            self.reports_unnecessary,
            self.reports_deprecated,
            self.skipped_on_no_emit,
        )
    }
}

// repopulateDiagnosticChain recomputes a diagnostic chain entry that depends on
// program state which may have changed between incremental builds.
// snapshot.go:189
pub(crate) fn repopulate_diagnostic_chain(
    b: &BuildInfoDiagnosticWithFileName,
    p: &'static CompilerProgram,
    file: Option<P<SourceFile>>,
) -> P<Diagnostic> {
    let info = b.repopulate_info.as_ref().unwrap();
    match info.kind {
        RepopulateDiagnosticKind::ModeMismatch => repopulate_mode_mismatch_chain(b, p, file),
        RepopulateDiagnosticKind::ModuleNotFound => {
            repopulate_module_not_found_chain(b, p, file, info)
        }
    }
}

// snapshot.go:225
fn repopulate_mode_mismatch_chain(
    b: &BuildInfoDiagnosticWithFileName,
    p: &'static CompilerProgram,
    file: Option<P<SourceFile>>,
) -> P<Diagnostic> {
    let Some(file) = file else {
        return b.to_diagnostic_without_repopulate(p, file);
    };

    let details =
        tsrs_checker::create_mode_mismatch_details(p as &'static dyn tsrs_checker::Program, file);

    let next_chain: Vec<P<Diagnostic>> =
        b.message_chain.iter().map(|msg| msg.to_diagnostic(p, Some(file))).collect();

    ast::new_diagnostic_from_serialized(
        Some(file),
        new_text_range(b.pos, b.end),
        details.message.code(),
        details.message.category(),
        details.message.key(),
        details.args,
        &next_chain,
        &[],
        false,
        false,
        false,
    )
}

// snapshot.go:252
fn repopulate_module_not_found_chain(
    b: &BuildInfoDiagnosticWithFileName,
    p: &'static CompilerProgram,
    file: Option<P<SourceFile>>,
    info: &RepopulateDiagnosticInfo,
) -> P<Diagnostic> {
    let Some(file) = file else {
        return b.to_diagnostic_without_repopulate(p, file);
    };

    let mut package_name = info.package_name.as_str();
    if package_name.is_empty() {
        package_name = &info.module_reference;
    }

    let details = tsrs_checker::create_module_not_found_chain(
        p as &'static dyn tsrs_checker::Program,
        file,
        &info.module_reference,
        info.mode,
        package_name,
    );

    let next_chain: Vec<P<Diagnostic>> =
        b.message_chain.iter().map(|msg| msg.to_diagnostic(p, Some(file))).collect();

    ast::new_diagnostic_from_serialized(
        Some(file),
        new_text_range(b.pos, b.end),
        details.message.code(),
        details.message.category(),
        details.message.key(),
        details.args,
        &next_chain,
        &[],
        false,
        false,
        false,
    )
}

impl DiagnosticsOrBuildInfoDiagnosticsWithFileName {
    // snapshot.go:285
    pub(crate) fn get_diagnostics(
        &self,
        p: &'static CompilerProgram,
        file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        if let Some(diagnostics) = self.diagnostics.lock().unwrap().as_ref() {
            return diagnostics.clone();
        }
        // Convert and cache the diagnostics
        let diagnostics: Vec<P<Diagnostic>> =
            self.build_info_diagnostics.iter().map(|diag| diag.to_diagnostic(p, file)).collect();
        *self.diagnostics.lock().unwrap() = Some(diagnostics.clone());
        diagnostics
    }
}

pub type DiagnosticsCache = Arc<DiagnosticsOrBuildInfoDiagnosticsWithFileName>;

#[derive(Default)]
pub struct Snapshot {
    // These are the fields that get serialized

    // Information of the file eg. its version, signature etc
    pub file_infos: SyncMap<Path, FileInfo>,
    pub(crate) options: Cell<Option<P<CompilerOptions>>>,
    //  Contains the map of ReferencedSet=Referenced files of the file if module emit is enabled
    pub referenced_map: ReferenceMap,
    // Cache of semantic diagnostics for files with their Path being the key
    pub semantic_diagnostics_per_file: SyncMap<Path, DiagnosticsCache>,
    // Cache of dts emit diagnostics for files with their Path being the key
    pub emit_diagnostics_per_file: SyncMap<Path, DiagnosticsCache>,
    // The map has key by source file's path that has been changed
    pub changed_files_set: SyncSet<Path>,
    // Files pending to be emitted
    pub affected_files_pending_emit: SyncMap<Path, FileEmitKind>,
    // Name of the file whose dts was the latest to change
    pub(crate) latest_changed_dts_file: RefCell<String>,
    // Hash of d.ts emitted for the file, use to track when emit of d.ts changes
    pub(crate) emit_signatures: SyncMap<Path, EmitSignature>,
    // Recorded if program had errors that need to be reported even with --noCheck
    pub(crate) has_errors: Cell<Tristate>,
    // Recorded if program had semantic errors only for non incremental build
    pub(crate) has_semantic_errors: Cell<bool>,
    // If semantic diagnostic check is pending
    pub(crate) check_pending: Cell<bool>,
    // Looked up package.json files from
    pub(crate) package_jsons: RefCell<Option<Vec<String>>>,
    pub(crate) missing_package_jsons: RefCell<Option<Vec<String>>>,
    // tsrs-only, versioned proof of canonical report placement; absent in Go-compatible history.
    pub(crate) canonical_check_signature: OnceLock<String>,

    // Additional fields that are not serialized but needed to track state

    // Program order, also recoverable from build info's fileNames/fileInfos, for history-dependent diagnostics.
    pub(crate) file_order: Vec<Path>,
    // true if build info emit is pending
    pub build_info_emit_pending: Cell<bool>,
    pub(crate) has_errors_from_old_state: Cell<Tristate>,
    pub(crate) has_semantic_errors_from_old_state: Cell<bool>,
    pub(crate) package_jsons_from_old_state: RefCell<Option<Vec<String>>>,
    pub(crate) missing_package_jsons_from_old_state: RefCell<Option<Vec<String>>>,
    //  Cache of all files excluding default library file for the current program
    pub(crate) all_files_excluding_default_library_file: OnceLock<Vec<P<SourceFile>>>,
    pub(crate) has_changed_dts_file: Cell<bool>,
    pub(crate) has_emit_diagnostics: Cell<bool>,

    // Used with testing to add text of hash for better comparison
    pub(crate) hash_with_text: bool,
}

impl Snapshot {
    pub(crate) fn options(&self) -> P<CompilerOptions> {
        self.options.get().unwrap()
    }

    // snapshot.go:351
    pub(crate) fn add_file_to_change_set(&self, file_path: Path) {
        self.changed_files_set.add(file_path);
        self.build_info_emit_pending.set(true);
    }

    // snapshot.go:356
    pub(crate) fn add_file_to_affected_files_pending_emit(
        &self,
        file_path: &Path,
        emit_kind: FileEmitKind,
    ) {
        let existing_kind = self.affected_files_pending_emit.load(file_path).unwrap_or_default();
        self.affected_files_pending_emit.store(file_path.clone(), existing_kind | emit_kind);
        if emit_kind.intersects(FileEmitKind::DtsErrors) {
            self.emit_diagnostics_per_file.delete(file_path);
        }
        self.build_info_emit_pending.set(true);
    }

    // snapshot.go:365
    pub(crate) fn get_all_files_excluding_default_library_file(
        &self,
        program: &'static CompilerProgram,
        first_source_file: Option<P<SourceFile>>,
    ) -> &[P<SourceFile>] {
        self.all_files_excluding_default_library_file.get_or_init(|| {
            let files = program.get_source_files();
            let mut all_files_excluding_default_library_file = Vec::with_capacity(files.len());
            let mut add_source_file = |file: P<SourceFile>| {
                if !program.is_source_file_default_library(file.path()) {
                    all_files_excluding_default_library_file.push(file);
                }
            };
            if let Some(first_source_file) = first_source_file {
                add_source_file(first_source_file);
            }
            for &file in files {
                if Some(file) != first_source_file {
                    add_source_file(file);
                }
            }
            all_files_excluding_default_library_file
        })
    }

    // snapshot.go:404
    pub(crate) fn compute_signature_with_diagnostics(
        &self,
        file: P<SourceFile>,
        text: &str,
        data: &WriteFileData,
    ) -> String {
        let mut builder = String::new();
        builder.push_str(get_text_handling_source_map_for_signature(text, data));
        for &diag in &data.diagnostics {
            diagnostic_to_string_builder(Some(diag), file, &mut builder);
        }
        self.compute_hash(&builder)
    }

    // snapshot.go:450
    pub(crate) fn compute_hash(&self, text: &str) -> String {
        compute_hash(text, self.hash_with_text)
    }

    // snapshot.go:454
    pub(crate) fn can_use_incremental_state(&self) -> bool {
        let options = self.options();
        if !options.is_incremental() && options.build.is_true() {
            // If not incremental build (with tsc -b), we don't need to track state except diagnostics per file so we can use it
            return false;
        }
        true
    }
}

// snapshot.go:397
pub(crate) fn get_text_handling_source_map_for_signature<'a>(
    text: &'a str,
    data: &WriteFileData,
) -> &'a str {
    if let Some(source_map_url_pos) = data.source_map_url_pos {
        return &text[..source_map_url_pos as usize];
    }
    text
}

// snapshot.go:413
pub fn diagnostic_to_string_builder(
    diagnostic: Option<P<Diagnostic>>,
    file: P<SourceFile>,
    builder: &mut String,
) {
    let Some(diagnostic) = diagnostic else {
        return;
    };
    builder.push('\n');
    if diagnostic.file() != Some(file) {
        // Go dereferences diagnostic.File() here; a file-less diagnostic would panic.
        let diagnostic_file = diagnostic.file().unwrap();
        builder.push_str(&tspath::ensure_path_is_non_module_name(
            &tspath::get_relative_path_from_directory(
                &tspath::get_directory_path(file.path()),
                diagnostic_file.path(),
                &ComparePathsOptions::default(),
            ),
        ));
    }
    if diagnostic.file().is_some() {
        // Go prints the raw ints: a synthetic position is -1, and its length 0.
        let _ = write!(
            builder,
            "({},{}): ",
            diagnostic.pos().map_or(-1, i64::from),
            diagnostic.len().map_or(0, i64::from)
        );
    }
    builder.push_str(diagnostic.category().name());
    let _ = write!(builder, "{}: ", diagnostic.code());
    builder.push_str(diagnostic.message_key().0);
    builder.push('\n');
    builder.extend(diagnostic.message_args().iter().flat_map(|arg| [arg.as_str(), "\n"]));
    for &chain in diagnostic.message_chain() {
        diagnostic_to_string_builder(Some(chain), file, builder);
    }
    for &info in diagnostic.related_information() {
        diagnostic_to_string_builder(Some(info), file, builder);
    }
}
