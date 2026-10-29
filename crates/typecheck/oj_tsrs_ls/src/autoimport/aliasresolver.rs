use std::sync::{Arc, Mutex};

use rustc_hash::FxHashMap;
use tsrs_ast::{self as ast, Node, SourceFile};
use tsrs_checker as checker;
use tsrs_core::tspath::{self, Path};
use tsrs_core::{CompilerOptions, ModuleKind, P, ResolutionMode, Tristate, alloc_str};
use tsrs_module::packagejson::InfoCacheEntry;
use tsrs_module::symlinks::KnownSymlinks;
use tsrs_module::{DefaultResolver, ModeAwareCache, ModeAwareCacheKey, ResolvedModule};
use tsrs_modulespecifiers::ModuleSpecifierGenerationHost;
use tsrs_tsoptions::outputpaths::OutputPathsHost;
use tsrs_tsoptions::{ParsedCommandLine, SourceOutputAndProjectReference};

use super::extract::ToPathFunc;
use super::registry::RegistryCloneHost;

// aliasresolver.go:16
#[derive(Clone, Debug)]
pub struct PathAndFileName {
    pub(crate) path: Path,
    pub(crate) file_name: String,
}

pub type OnFailedAmbientModuleLookup = Box<dyn Fn(P<SourceFile>, &str) + Send + Sync>;

// aliasresolver.go:21
pub struct AliasResolver {
    to_path: ToPathFunc,
    host: &'static dyn RegistryCloneHost,
    module_resolver: &'static DefaultResolver<'static>,

    pub(crate) root_files: Vec<P<SourceFile>>,
    // symlinks maps from realpath to symlinked path and file name
    pub(crate) symlinks: FxHashMap<Path, PathAndFileName>,
    on_failed_ambient_module_lookup: OnFailedAmbientModuleLookup,
    resolved_modules: Mutex<FxHashMap<Path, Arc<Mutex<ModeAwareCache<P<ResolvedModule>>>>>>,

    options: P<CompilerOptions>,
    root_files_static: &'static [P<SourceFile>],
    empty_resolved_modules: FxHashMap<Path, ModeAwareCache<P<ResolvedModule>>>,
    empty_packages_map: FxHashMap<String, bool>,
}

// aliasresolver.go:33
#[expect(clippy::implicit_hasher, reason = "Public for the integration tests.")]
pub fn new_alias_resolver(
    root_files: Vec<P<SourceFile>>,
    symlinks: FxHashMap<Path, PathAndFileName>,
    host: &'static dyn RegistryCloneHost,
    module_resolver: &'static DefaultResolver<'static>,
    to_path: ToPathFunc,
    on_failed_ambient_module_lookup: OnFailedAmbientModuleLookup,
) -> AliasResolver {
    let root_files_static = tsrs_core::alloc_slice(&root_files);
    AliasResolver {
        to_path,
        host,
        module_resolver,
        root_files,
        symlinks,
        on_failed_ambient_module_lookup,
        resolved_modules: Mutex::new(FxHashMap::default()),
        // aliasresolver.go:61: `Options` returns a fresh `&core.CompilerOptions{NoCheck: core.TSTrue}`.
        options: P::new(CompilerOptions { no_check: Tristate::True, ..Default::default() }),
        root_files_static,
        empty_resolved_modules: FxHashMap::default(),
        empty_packages_map: FxHashMap::default(),
    }
}

impl AliasResolver {
    // aliasresolver.go:76
    pub(crate) fn get_source_file(&self, file_name: &str) -> Option<P<SourceFile>> {
        let file = self.host.get_source_file(file_name, &(self.to_path)(file_name));
        // file may be nil due to symlink/realpath mismatch; see TestAutoImportBuilderFS
        let file = file?;
        tsrs_binder::bind_source_file(file);
        Some(file)
    }
}

impl checker::Program for AliasResolver {
    // aliasresolver.go:61
    fn options(&self) -> P<CompilerOptions> {
        self.options
    }

    // aliasresolver.go:56
    fn source_files(&self) -> &'static [P<SourceFile>] {
        self.root_files_static
    }

    // Not in Go (see `checker::Program::source_files_complete`): files are loaded on demand.
    fn source_files_complete(&self) -> bool {
        false
    }

    // aliasresolver.go:51
    fn bind_source_files(&self) {
        // We will bind as we parse
    }

    // aliasresolver.go:166
    fn file_exists(&self, _file_name: &str) -> bool {
        panic!("unimplemented")
    }

    fn get_source_file(&self, file_name: &str) -> Option<P<SourceFile>> {
        AliasResolver::get_source_file(self, file_name)
    }

    // aliasresolver.go:131
    fn get_source_file_for_resolved_module(&self, file_name: &str) -> Option<P<SourceFile>> {
        AliasResolver::get_source_file(self, file_name)
    }

    // aliasresolver.go:91
    fn get_emit_module_format_of_file(&self, _source_file: P<SourceFile>) -> ModuleKind {
        ModuleKind::ES_NEXT
    }

    // aliasresolver.go:96
    fn get_emit_syntax_for_usage_location(
        &self,
        _source_file: P<SourceFile>,
        _usage_location: P<Node>,
    ) -> ResolutionMode {
        ModuleKind::ES_NEXT
    }

    // aliasresolver.go:101
    fn get_implied_node_format_for_emit(&self, _source_file: P<SourceFile>) -> ModuleKind {
        ModuleKind::ES_NEXT
    }

    // aliasresolver.go:111
    fn get_resolved_module(
        &self,
        current_source_file: P<SourceFile>,
        module_reference: &str,
        mode: ResolutionMode,
    ) -> Option<P<ResolvedModule>> {
        let cache = Arc::clone(
            self.resolved_modules
                .lock()
                .unwrap()
                .entry(current_source_file.path().clone())
                .or_default(),
        );
        if let Some(&resolved) =
            tsrs_module::mode_aware_cache_get(&cache.lock().unwrap(), module_reference, mode)
        {
            return Some(resolved);
        }
        let key = ModeAwareCacheKey { name: alloc_str(module_reference), mode };
        let (resolved, _) = self
            .module_resolver
            .resolve_module_name(module_reference, current_source_file.file_name(), mode, None)
            .unwrap();
        let resolved = *cache.lock().unwrap().entry(key).or_insert(resolved);
        if !resolved.is_resolved() && !tspath::path_is_relative(module_reference) {
            (self.on_failed_ambient_module_lookup)(current_source_file, module_reference);
        }
        Some(resolved)
    }

    // aliasresolver.go:136
    fn get_resolved_modules(&self) -> &FxHashMap<Path, ModeAwareCache<P<ResolvedModule>>> {
        // only used when producing diagnostics, which hopefully the checker won't do
        &self.empty_resolved_modules
    }

    // aliasresolver.go:236
    fn get_packages_map(&self) -> &FxHashMap<String, bool> {
        &self.empty_packages_map
    }

    // aliasresolver.go:148
    fn get_source_file_meta_data(&self, _path: &Path) -> ast::SourceFileMetaData {
        panic!("unimplemented")
    }

    // aliasresolver.go:181
    fn get_jsx_runtime_import_specifier(&self, _path: &Path) -> (String, Option<P<Node>>) {
        (String::new(), None)
    }

    // aliasresolver.go:176
    fn get_import_helpers_import_specifier(&self, _path: &Path) -> Option<P<Node>> {
        panic!("unimplemented")
    }

    // aliasresolver.go:231
    fn source_file_may_be_emitted(
        &self,
        _source_file: P<SourceFile>,
        _force_dts_emit: bool,
    ) -> bool {
        panic!("unimplemented")
    }

    // aliasresolver.go:221
    fn is_source_file_default_library(&self, _path: &Path) -> bool {
        false
    }

    // aliasresolver.go:196
    fn get_project_reference_from_output_dts(
        &self,
        _path: &Path,
    ) -> Option<P<SourceOutputAndProjectReference>> {
        panic!("unimplemented")
    }

    // aliasresolver.go:201
    fn get_project_reference_from_source(
        &self,
        _path: &Path,
    ) -> Option<P<SourceOutputAndProjectReference>> {
        panic!("unimplemented")
    }

    // aliasresolver.go:206
    fn get_redirect_for_resolution(&self, _file: P<SourceFile>) -> Option<P<ParsedCommandLine>> {
        panic!("unimplemented")
    }

    // aliasresolver.go:153
    fn common_source_directory(&self) -> String {
        panic!("unimplemented")
    }

    // aliasresolver.go:71
    fn use_case_sensitive_file_names(&self) -> bool {
        self.host.fs().use_case_sensitive_file_names()
    }

    // aliasresolver.go:66
    fn get_current_directory(&self) -> String {
        self.host.get_current_directory().to_string()
    }

    // aliasresolver.go:86
    fn get_default_resolution_mode_for_file(&self, _file: P<SourceFile>) -> ResolutionMode {
        ModuleKind::ES_NEXT
    }

    // aliasresolver.go:106
    fn get_mode_for_usage_location(
        &self,
        _file: P<SourceFile>,
        _module_specifier: P<Node>,
    ) -> ResolutionMode {
        ModuleKind::ES_NEXT
    }

    fn as_module_specifier_generation_host(&self) -> &dyn ModuleSpecifierGenerationHost {
        self
    }
}

impl OutputPathsHost for AliasResolver {
    // aliasresolver.go:153
    fn common_source_directory(&self) -> String {
        panic!("unimplemented")
    }

    // aliasresolver.go:158
    fn content_mapper_extensions(&self) -> Vec<String> {
        Vec::new()
    }

    // aliasresolver.go:66
    fn get_current_directory(&self) -> &str {
        self.host.get_current_directory()
    }

    // aliasresolver.go:71
    fn use_case_sensitive_file_names(&self) -> bool {
        self.host.fs().use_case_sensitive_file_names()
    }
}

impl ModuleSpecifierGenerationHost for AliasResolver {
    // aliasresolver.go:143
    fn get_symlink_cache(&self) -> P<KnownSymlinks> {
        panic!("unimplemented")
    }

    // aliasresolver.go:171
    fn get_global_typings_cache_location(&self) -> String {
        panic!("unimplemented")
    }

    // aliasresolver.go:201
    fn get_project_reference_from_source(
        &self,
        _path: &Path,
    ) -> Option<P<SourceOutputAndProjectReference>> {
        panic!("unimplemented")
    }

    // aliasresolver.go:211
    fn get_redirect_targets(&self, _path: &Path) -> Vec<String> {
        panic!("unimplemented")
    }

    // aliasresolver.go:221
    fn get_source_of_project_reference_if_output_included(&self, _file: P<SourceFile>) -> String {
        panic!("unimplemented")
    }

    // aliasresolver.go:166
    fn file_exists(&self, _path: &str) -> bool {
        panic!("unimplemented")
    }

    // aliasresolver.go:186
    fn get_nearest_ancestor_directory_with_package_json(&self, _dirname: &str) -> String {
        panic!("unimplemented")
    }

    // aliasresolver.go:191
    fn get_package_json_info(&self, _pkg_json_path: &str) -> Option<P<InfoCacheEntry>> {
        panic!("unimplemented")
    }

    // aliasresolver.go:86
    fn get_default_resolution_mode_for_file(&self, _file: P<SourceFile>) -> ResolutionMode {
        ModuleKind::ES_NEXT
    }

    // aliasresolver.go:216
    fn get_resolved_module_from_module_specifier(
        &self,
        _file: P<SourceFile>,
        _module_specifier: P<Node>,
    ) -> Option<P<ResolvedModule>> {
        panic!("unimplemented")
    }

    // aliasresolver.go:106
    fn get_mode_for_usage_location(
        &self,
        _file: P<SourceFile>,
        _module_specifier: P<Node>,
    ) -> ResolutionMode {
        ModuleKind::ES_NEXT
    }
}
