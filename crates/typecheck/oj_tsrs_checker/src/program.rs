use tsrs_core::tspath::Path;
use tsrs_core::{CompilerOptions, ModuleKind, ResolutionMode};
use tsrs_module::{ModeAwareCache, ResolvedModule};

use crate::*;

pub use tsrs_tsoptions::{ParsedCommandLine, SourceOutputAndProjectReference};

/// Go `checker.Program` (plus the `modulespecifiers.ModuleSpecifierGenerationHost` methods the checker calls).
/// `ast.HasFileName` parameters are `P<SourceFile>`.
pub trait Program: Send + Sync {
    fn options(&self) -> P<CompilerOptions>;
    fn source_files(&self) -> &'static [P<SourceFile>];
    fn bind_source_files(&self);
    fn file_exists(&self, file_name: &str) -> bool;
    fn get_source_file(&self, file_name: &str) -> Option<P<SourceFile>>;
    fn get_source_file_for_resolved_module(&self, file_name: &str) -> Option<P<SourceFile>>;
    fn get_emit_module_format_of_file(&self, source_file: P<SourceFile>) -> ModuleKind;
    fn get_emit_syntax_for_usage_location(
        &self,
        source_file: P<SourceFile>,
        usage_location: P<Node>,
    ) -> ResolutionMode;
    fn get_implied_node_format_for_emit(&self, source_file: P<SourceFile>) -> ModuleKind;
    fn get_resolved_module(
        &self,
        current_source_file: P<SourceFile>,
        module_reference: &str,
        mode: ResolutionMode,
    ) -> Option<P<ResolvedModule>>;
    fn get_resolved_modules(&self) -> &FxHashMap<Path, ModeAwareCache<P<ResolvedModule>>>;
    fn get_packages_map(&self) -> &FxHashMap<String, bool>;
    fn get_source_file_meta_data(&self, path: &Path) -> ast::SourceFileMetaData;
    /// Go `(moduleReference string, specifier *ast.Node)`.
    fn get_jsx_runtime_import_specifier(&self, path: &Path) -> (String, Option<P<Node>>);
    fn get_import_helpers_import_specifier(&self, path: &Path) -> Option<P<Node>>;
    fn source_file_may_be_emitted(&self, source_file: P<SourceFile>, force_dts_emit: bool) -> bool;
    fn is_source_file_default_library(&self, path: &Path) -> bool;
    fn get_project_reference_from_output_dts(
        &self,
        path: &Path,
    ) -> Option<P<SourceOutputAndProjectReference>>;
    /// Go `modulespecifiers.ModuleSpecifierGenerationHost.GetProjectReferenceFromSource`.
    fn get_project_reference_from_source(
        &self,
        path: &Path,
    ) -> Option<P<SourceOutputAndProjectReference>>;
    fn get_redirect_for_resolution(&self, file: P<SourceFile>) -> Option<P<ParsedCommandLine>>;
    fn common_source_directory(&self) -> String;

    // Host (modulespecifiers.ModuleSpecifierGenerationHost) methods used by checker code
    fn use_case_sensitive_file_names(&self) -> bool;
    fn get_current_directory(&self) -> String;
    fn get_default_resolution_mode_for_file(&self, file: P<SourceFile>) -> ResolutionMode;
    fn get_mode_for_usage_location(
        &self,
        file: P<SourceFile>,
        module_specifier: P<Node>,
    ) -> ResolutionMode;

    /// Go's implicit conversion of the program to the checker's `Host` (= `modulespecifiers.ModuleSpecifierGenerationHost`),
    /// done by `NewNodeBuilderEx` (`host: ch.program`). The program type implements that trait separately.
    fn as_module_specifier_generation_host(&self) -> &dyn ModuleSpecifierGenerationHost;

    /// Not in Go: whether every source file the checker can reach is in `source_files` (true for a program). When it
    /// is not (the auto-import alias resolver loads files on demand), `compareNodes` maps the missing files to index
    /// 0 like Go's map lookup, so `compareSymbols` is not a total order (`sort_symbols`).
    fn source_files_complete(&self) -> bool {
        true
    }

    /// Not in Go: where tsgo's compiler, at its default checker count, type checks `file`: the checker's index and the
    /// file's index in the program (each checker visits its files in program order). None when it does not type check
    /// the file, or for an external project/API pool with no fixed assignment (then every declaring file counts as
    /// visited first).
    fn tsgo_default_check_slot(&self, _file: P<SourceFile>) -> Option<(u32, u32)> {
        None
    }
}
