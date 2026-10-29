// `impl checker.Program for *compiler.Program` (the Go assertion `var _ checker.Program = (*Program)(nil)`).

use rustc_hash::FxHashMap;
use tsrs_ast::{Node, SourceFile, SourceFileMetaData};
use tsrs_core::tspath::Path;
use tsrs_core::{CompilerOptions, ModuleKind, P, ResolutionMode};
use tsrs_module::symlinks::KnownSymlinks;
use tsrs_module::{ModeAwareCache, ResolvedModule, packagejson};
use tsrs_tsoptions::{ParsedCommandLine, SourceOutputAndProjectReference};

use crate::program::Program;

impl tsrs_checker::Program for Program {
    fn options(&self) -> P<CompilerOptions> {
        Program::options(self)
    }

    fn source_files(&self) -> &'static [P<SourceFile>] {
        self.files
    }

    fn bind_source_files(&self) {
        Program::bind_source_files(self);
    }

    fn file_exists(&self, file_name: &str) -> bool {
        Program::file_exists(self, file_name)
    }

    fn get_source_file(&self, file_name: &str) -> Option<P<SourceFile>> {
        Program::get_source_file(self, file_name)
    }

    fn get_source_file_for_resolved_module(&self, file_name: &str) -> Option<P<SourceFile>> {
        Program::get_source_file_for_resolved_module(self, file_name)
    }

    fn get_emit_module_format_of_file(&self, source_file: P<SourceFile>) -> ModuleKind {
        Program::get_emit_module_format_of_file(self, source_file)
    }

    fn get_emit_syntax_for_usage_location(
        &self,
        source_file: P<SourceFile>,
        usage_location: P<Node>,
    ) -> ResolutionMode {
        Program::get_emit_syntax_for_usage_location(self, source_file, usage_location)
    }

    fn get_implied_node_format_for_emit(&self, source_file: P<SourceFile>) -> ModuleKind {
        Program::get_implied_node_format_for_emit(self, source_file)
    }

    fn get_resolved_module(
        &self,
        current_source_file: P<SourceFile>,
        module_reference: &str,
        mode: ResolutionMode,
    ) -> Option<P<ResolvedModule>> {
        Program::get_resolved_module(self, current_source_file, module_reference, mode)
    }

    fn get_resolved_modules(&self) -> &FxHashMap<Path, ModeAwareCache<P<ResolvedModule>>> {
        Program::get_resolved_modules(self)
    }

    fn get_packages_map(&self) -> &FxHashMap<String, bool> {
        Program::get_packages_map(self)
    }

    fn get_source_file_meta_data(&self, path: &Path) -> SourceFileMetaData {
        Program::get_source_file_meta_data(self, path).clone()
    }

    fn get_jsx_runtime_import_specifier(&self, path: &Path) -> (String, Option<P<Node>>) {
        Program::get_jsx_runtime_import_specifier(self, path)
    }

    fn get_import_helpers_import_specifier(&self, path: &Path) -> Option<P<Node>> {
        Program::get_import_helpers_import_specifier(self, path)
    }

    fn source_file_may_be_emitted(&self, source_file: P<SourceFile>, force_dts_emit: bool) -> bool {
        Program::source_file_may_be_emitted(self, source_file, force_dts_emit)
    }

    fn is_source_file_default_library(&self, path: &Path) -> bool {
        Program::is_source_file_default_library(self, path)
    }

    fn get_project_reference_from_output_dts(
        &self,
        path: &Path,
    ) -> Option<P<SourceOutputAndProjectReference>> {
        Program::get_project_reference_from_output_dts(self, path)
    }

    fn get_project_reference_from_source(
        &self,
        path: &Path,
    ) -> Option<P<SourceOutputAndProjectReference>> {
        Program::get_project_reference_from_source(self, path)
    }

    fn get_redirect_for_resolution(&self, file: P<SourceFile>) -> Option<P<ParsedCommandLine>> {
        Program::get_redirect_for_resolution(self, file)
    }

    fn common_source_directory(&self) -> String {
        Program::common_source_directory(self).to_string()
    }

    fn use_case_sensitive_file_names(&self) -> bool {
        Program::use_case_sensitive_file_names(self)
    }

    fn get_current_directory(&self) -> String {
        Program::get_current_directory(self).to_string()
    }

    fn get_default_resolution_mode_for_file(&self, file: P<SourceFile>) -> ResolutionMode {
        Program::get_default_resolution_mode_for_file(self, file)
    }

    fn get_mode_for_usage_location(
        &self,
        file: P<SourceFile>,
        module_specifier: P<Node>,
    ) -> ResolutionMode {
        Program::get_mode_for_usage_location(self, file, module_specifier)
    }

    fn as_module_specifier_generation_host(
        &self,
    ) -> &dyn tsrs_modulespecifiers::ModuleSpecifierGenerationHost {
        self
    }

    fn tsgo_default_check_slot(&self, file: P<SourceFile>) -> Option<(u32, u32)> {
        if !self.uses_canonical_diagnostics() {
            return None;
        }
        self.tsgo_check_slots
            .get_or_init(|| crate::checkerpool::tsgo_default_check_slots(self))
            .get(&file)
            .copied()
    }
}

// Go `*compiler.Program` satisfies `modulespecifiers.ModuleSpecifierGenerationHost` structurally (its supertrait
// `OutputPathsHost` is implemented in program.rs); the node builder reaches it through
// `checker::Program::as_module_specifier_generation_host`.
impl tsrs_modulespecifiers::ModuleSpecifierGenerationHost for Program {
    fn get_symlink_cache(&self) -> P<KnownSymlinks> {
        Program::get_symlink_cache(self)
    }

    fn get_global_typings_cache_location(&self) -> String {
        Program::get_global_typings_cache_location(self).to_string()
    }

    fn get_project_reference_from_source(
        &self,
        path: &Path,
    ) -> Option<P<tsrs_tsoptions::SourceOutputAndProjectReference>> {
        Program::get_project_reference_from_source(self, path)
    }

    fn get_redirect_targets(&self, path: &Path) -> Vec<String> {
        Program::get_redirect_targets(self, path).to_vec()
    }

    fn get_source_of_project_reference_if_output_included(&self, file: P<SourceFile>) -> String {
        Program::get_source_of_project_reference_if_output_included(
            self,
            file.file_name(),
            file.path(),
        )
    }

    fn file_exists(&self, path: &str) -> bool {
        Program::file_exists(self, path)
    }

    fn get_nearest_ancestor_directory_with_package_json(&self, dirname: &str) -> String {
        Program::get_nearest_ancestor_directory_with_package_json(self, dirname)
    }

    fn get_package_json_info(&self, pkg_json_path: &str) -> Option<P<packagejson::InfoCacheEntry>> {
        Program::get_package_json_info(self, pkg_json_path)
    }

    fn get_default_resolution_mode_for_file(&self, file: P<SourceFile>) -> ResolutionMode {
        Program::get_default_resolution_mode_for_file(self, file)
    }

    fn get_resolved_module_from_module_specifier(
        &self,
        file: P<SourceFile>,
        module_specifier: P<Node>,
    ) -> Option<P<ResolvedModule>> {
        Program::get_resolved_module_from_module_specifier(self, file, module_specifier)
    }

    fn get_mode_for_usage_location(
        &self,
        file: P<SourceFile>,
        module_specifier: P<Node>,
    ) -> ResolutionMode {
        Program::get_mode_for_usage_location(self, file, module_specifier)
    }

    fn exports_module_name_cache(
        &self,
        options: &tsrs_core::CompilerOptions,
    ) -> Option<&tsrs_modulespecifiers::ExportsModuleNameCache> {
        std::ptr::eq(options, &raw const *self.options()).then_some(&self.exports_module_name_cache)
    }
}
