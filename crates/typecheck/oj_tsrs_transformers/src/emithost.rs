// Port of printer/emithost.go. Go keeps this interface in package `printer`; it lives here because its
// `GetEmitResolver` returns `Resolver`, which is defined in this crate (tsrs_printer does not depend on the checker).

use crate::*;

// NOTE: EmitHost operations must be thread-safe
// emithost.go:11
pub trait EmitHost {
    fn options(&self) -> P<CompilerOptions>;
    fn source_files(&self) -> &'static [P<SourceFile>];
    fn content_mapper_extension_rewrites(&self) -> Vec<tsrs_core::tspath::ExtensionRewrite>;
    fn use_case_sensitive_file_names(&self) -> bool;
    fn get_current_directory(&self) -> &str;
    fn common_source_directory(&self) -> String;
    fn is_emit_blocked(&self, file: &str) -> bool;
    fn write_file(&self, file_name: &str, text: &str) -> Result<(), String>;
    fn get_emit_module_format_of_file(&self, file: P<SourceFile>) -> ModuleKind;
    fn get_emit_resolver(&self) -> Resolver;
    fn get_project_reference_from_source(
        &self,
        path: &tsrs_core::tspath::Path,
    ) -> Option<P<tsrs_tsoptions::SourceOutputAndProjectReference>>;
    fn is_source_file_from_external_library(&self, file: P<SourceFile>) -> bool;
}
