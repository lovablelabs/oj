// Port of compiler/emitHost.go: the host the declaration transformer and the emitter run against (Go's
// `compiler.EmitHost` = `printer.EmitHost` (here `tsrs_transformers::EmitHost`) + `declarations.DeclarationEmitHost`).
//
// Go `newEmitHost` acquires the file's checker from the pool and keeps it locked until `done()`; here the caller
// holds the checker (`CheckerHandle`/`&mut Checker`) and lends it to `checker_slot` for the duration of the
// transform, and the host's `Resolver` borrows it from there (see tsrs_declarations::Resolver).

use tsrs_ast::{FileReference, Node, SourceFile};
use tsrs_checker::CheckerSlot;
use tsrs_core::tspath::Path;
use tsrs_core::{P, ResolutionMode};
use tsrs_declarations::{DeclarationEmitHost, Resolver};
use tsrs_module::symlinks::KnownSymlinks;
use tsrs_module::{ResolvedModule, packagejson};
use tsrs_tsoptions::outputpaths::{self, OutputPaths, OutputPathsHost};

use crate::emitter;
use crate::program::Program;

// Every method that reaches the program escapes a scratch region (one file's emit, notes/mem-emit-regions.md): the
// program's lazily filled caches (package.json entries, symlinks, module-name memos) outlive the file.

pub struct EmitHost {
    program: &'static Program,
    emit_resolver: Resolver,
}

// emitHost.go:38 (the checker acquisition is the caller's; see above)
pub fn new_emit_host(
    program: &'static Program,
    emit_resolver: P<tsrs_checker::EmitResolver>,
    checker_slot: P<CheckerSlot>,
) -> &'static EmitHost {
    P::new(EmitHost { program, emit_resolver: Resolver::new(emit_resolver, checker_slot) }).get()
}

impl OutputPathsHost for EmitHost {
    fn common_source_directory(&self) -> String {
        let _outer = tsrs_core::arena::escape_scratch();
        OutputPathsHost::common_source_directory(self.program)
    }

    fn content_mapper_extensions(&self) -> Vec<String> {
        let _outer = tsrs_core::arena::escape_scratch();
        OutputPathsHost::content_mapper_extensions(self.program)
    }

    fn content_mapper_extension_rewrites(&self) -> Vec<tsrs_core::tspath::ExtensionRewrite> {
        let _outer = tsrs_core::arena::escape_scratch();
        OutputPathsHost::content_mapper_extension_rewrites(self.program)
    }

    fn get_current_directory(&self) -> &str {
        let _outer = tsrs_core::arena::escape_scratch();
        OutputPathsHost::get_current_directory(self.program)
    }

    fn use_case_sensitive_file_names(&self) -> bool {
        let _outer = tsrs_core::arena::escape_scratch();
        OutputPathsHost::use_case_sensitive_file_names(self.program)
    }
}

impl tsrs_modulespecifiers::ModuleSpecifierGenerationHost for EmitHost {
    fn get_symlink_cache(&self) -> P<KnownSymlinks> {
        let _outer = tsrs_core::arena::escape_scratch();
        tsrs_modulespecifiers::ModuleSpecifierGenerationHost::get_symlink_cache(self.program)
    }

    fn get_global_typings_cache_location(&self) -> String {
        let _outer = tsrs_core::arena::escape_scratch();
        tsrs_modulespecifiers::ModuleSpecifierGenerationHost::get_global_typings_cache_location(
            self.program,
        )
    }

    fn get_project_reference_from_source(
        &self,
        path: &Path,
    ) -> Option<P<tsrs_tsoptions::SourceOutputAndProjectReference>> {
        let _outer = tsrs_core::arena::escape_scratch();
        tsrs_modulespecifiers::ModuleSpecifierGenerationHost::get_project_reference_from_source(
            self.program,
            path,
        )
    }

    fn get_redirect_targets(&self, path: &Path) -> Vec<String> {
        let _outer = tsrs_core::arena::escape_scratch();
        tsrs_modulespecifiers::ModuleSpecifierGenerationHost::get_redirect_targets(
            self.program,
            path,
        )
    }

    fn get_source_of_project_reference_if_output_included(&self, file: P<SourceFile>) -> String {
        let _outer = tsrs_core::arena::escape_scratch();
        tsrs_modulespecifiers::ModuleSpecifierGenerationHost::get_source_of_project_reference_if_output_included(self.program, file)
    }

    fn file_exists(&self, path: &str) -> bool {
        let _outer = tsrs_core::arena::escape_scratch();
        tsrs_modulespecifiers::ModuleSpecifierGenerationHost::file_exists(self.program, path)
    }

    fn get_nearest_ancestor_directory_with_package_json(&self, dirname: &str) -> String {
        let _outer = tsrs_core::arena::escape_scratch();
        tsrs_modulespecifiers::ModuleSpecifierGenerationHost::get_nearest_ancestor_directory_with_package_json(self.program, dirname)
    }

    fn get_package_json_info(&self, pkg_json_path: &str) -> Option<P<packagejson::InfoCacheEntry>> {
        let _outer = tsrs_core::arena::escape_scratch();
        tsrs_modulespecifiers::ModuleSpecifierGenerationHost::get_package_json_info(
            self.program,
            pkg_json_path,
        )
    }

    fn exports_module_name_cache(
        &self,
        options: &tsrs_core::CompilerOptions,
    ) -> Option<&tsrs_modulespecifiers::ExportsModuleNameCache> {
        let _outer = tsrs_core::arena::escape_scratch();
        tsrs_modulespecifiers::ModuleSpecifierGenerationHost::exports_module_name_cache(
            self.program,
            options,
        )
    }

    fn get_default_resolution_mode_for_file(&self, file: P<SourceFile>) -> ResolutionMode {
        let _outer = tsrs_core::arena::escape_scratch();
        tsrs_modulespecifiers::ModuleSpecifierGenerationHost::get_default_resolution_mode_for_file(
            self.program,
            file,
        )
    }

    fn get_resolved_module_from_module_specifier(
        &self,
        file: P<SourceFile>,
        module_specifier: P<Node>,
    ) -> Option<P<ResolvedModule>> {
        let _outer = tsrs_core::arena::escape_scratch();
        tsrs_modulespecifiers::ModuleSpecifierGenerationHost::get_resolved_module_from_module_specifier(self.program, file, module_specifier)
    }

    fn get_mode_for_usage_location(
        &self,
        file: P<SourceFile>,
        module_specifier: P<Node>,
    ) -> ResolutionMode {
        let _outer = tsrs_core::arena::escape_scratch();
        tsrs_modulespecifiers::ModuleSpecifierGenerationHost::get_mode_for_usage_location(
            self.program,
            file,
            module_specifier,
        )
    }
}

impl DeclarationEmitHost for EmitHost {
    // emitHost.go:103
    fn get_source_file_from_reference(
        &self,
        origin: P<SourceFile>,
        ref_: P<FileReference>,
    ) -> Option<P<SourceFile>> {
        let _outer = tsrs_core::arena::escape_scratch();
        self.program.get_source_file_from_reference(origin, ref_)
    }

    // emitHost.go:94
    fn get_output_paths_for(&self, file: P<SourceFile>, force_dts_paths: bool) -> OutputPaths {
        let _outer = tsrs_core::arena::escape_scratch();
        // TODO: cache
        outputpaths::get_output_paths_for(
            file,
            &self.program.options(),
            self,
            outputpaths::ForceEmitPaths { dts: force_dts_paths, ..Default::default() },
        )
    }

    // emitHost.go:99
    fn source_file_may_be_emitted(&self, file: P<SourceFile>, force_dts_emit: bool) -> bool {
        let _outer = tsrs_core::arena::escape_scratch();
        emitter::source_file_may_be_emitted(file, self.program, force_dts_emit, false)
    }
}

impl tsrs_transformers::EmitHost for EmitHost {
    // emitHost.go:113
    fn options(&self) -> P<tsrs_core::CompilerOptions> {
        let _outer = tsrs_core::arena::escape_scratch();
        self.program.options()
    }

    // emitHost.go:114
    fn source_files(&self) -> &'static [P<SourceFile>] {
        let _outer = tsrs_core::arena::escape_scratch();
        self.program.source_files()
    }

    fn content_mapper_extension_rewrites(&self) -> Vec<tsrs_core::tspath::ExtensionRewrite> {
        let _outer = tsrs_core::arena::escape_scratch();
        self.program.content_mapper_extension_rewrites()
    }

    // emitHost.go:122
    fn use_case_sensitive_file_names(&self) -> bool {
        let _outer = tsrs_core::arena::escape_scratch();
        self.program.use_case_sensitive_file_names()
    }

    // emitHost.go:115
    fn get_current_directory(&self) -> &str {
        let _outer = tsrs_core::arena::escape_scratch();
        self.program.get_current_directory()
    }

    // emitHost.go:116
    fn common_source_directory(&self) -> String {
        let _outer = tsrs_core::arena::escape_scratch();
        self.program.common_source_directory().to_string()
    }

    // emitHost.go:126
    fn is_emit_blocked(&self, file: &str) -> bool {
        let _outer = tsrs_core::arena::escape_scratch();
        self.program.is_emit_blocked(file)
    }

    // emitHost.go:130. The program host's file system (the CLI: the cached FS over the OS FS, which creates missing
    // directories). Reached through `Program::emit`.
    fn write_file(&self, file_name: &str, text: &str) -> Result<(), String> {
        let _outer = tsrs_core::arena::escape_scratch();
        self.program.host().fs().write_file(file_name, text)
    }

    // emitHost.go:59
    fn get_emit_module_format_of_file(&self, file: P<SourceFile>) -> tsrs_core::ModuleKind {
        let _outer = tsrs_core::arena::escape_scratch();
        self.program.get_emit_module_format_of_file(file)
    }

    // emitHost.go:134
    fn get_emit_resolver(&self) -> Resolver {
        self.emit_resolver
    }

    // emitHost.go:83
    fn get_project_reference_from_source(
        &self,
        path: &Path,
    ) -> Option<P<tsrs_tsoptions::SourceOutputAndProjectReference>> {
        let _outer = tsrs_core::arena::escape_scratch();
        self.program.get_project_reference_from_source(path)
    }

    // emitHost.go:138
    fn is_source_file_from_external_library(&self, file: P<SourceFile>) -> bool {
        let _outer = tsrs_core::arena::escape_scratch();
        self.program.is_source_file_from_external_library(file)
    }
}
