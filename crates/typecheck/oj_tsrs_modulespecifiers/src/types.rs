#![expect(
    clippy::redundant_pub_crate,
    reason = "Only the public API is re-exported; implementation helpers remain crate-private."
)]

//! Non-function declarations of the modulespecifiers package (types.go, plus those of preferences.go, specifiers.go
//! and util.go).

use std::sync::{LazyLock, RwLock};

use rustc_hash::FxHashMap;
use tsrs_ast::{Node, SourceFile, Symbol};
use tsrs_core::tspath::Path;
use tsrs_core::{P, ResolutionMode};
use tsrs_module::packagejson;

use crate::*;

// types.go

// Go `SourceFileForSpecifierGeneration` (Path, FileName, Imports, IsJS) is implemented by `*ast.SourceFile` in the
// type checker; parameters of that type are `P<SourceFile>`.

/// Go `CheckerShape` (implemented by the checker).
pub trait CheckerShape {
    fn get_symbol_at_location(&mut self, node: P<Node>) -> Option<P<Symbol>>;
    fn get_aliased_symbol(&mut self, symbol: P<Symbol>) -> P<Symbol>;
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum ResultKind {
    #[default]
    None,
    NodeModules,
    Paths,
    Redirect,
    Relative,
    Ambient,
}

#[derive(Clone, Debug, Default)]
pub struct ModuleSpecifiersResult {
    pub specifiers: Vec<String>,
    pub ambient_module_symbol: Option<P<Symbol>>, // used to construct an import attributes node, if one is needed
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ModulePath {
    pub file_name: String,
    pub(crate) is_in_node_modules: bool,
    pub(crate) is_redirect: bool,
}

/// Go `ModuleSpecifierGenerationHost`.
///
/// `CommonSourceDirectory`, `ContentMapperExtensions`, `GetCurrentDirectory` and `UseCaseSensitiveFileNames`
/// come from the supertrait `outputpaths.OutputPathsHost` (Go passes the host to `outputpaths` functions).
/// `ast.HasFileName` parameters are `P<SourceFile>`.
pub trait ModuleSpecifierGenerationHost: OutputPathsHost {
    // GetModuleResolutionCache() any // !!! TODO: adapt new resolution cache model
    fn get_symlink_cache(&self) -> P<KnownSymlinks>;
    // GetFileIncludeReasons() any // !!! TODO: adapt new resolution cache model
    fn get_global_typings_cache_location(&self) -> String;

    fn get_project_reference_from_source(
        &self,
        path: &Path,
    ) -> Option<P<SourceOutputAndProjectReference>>;
    fn get_redirect_targets(&self, path: &Path) -> Vec<String>;
    fn get_source_of_project_reference_if_output_included(&self, file: P<SourceFile>) -> String;

    fn file_exists(&self, path: &str) -> bool;

    fn get_nearest_ancestor_directory_with_package_json(&self, dirname: &str) -> String;
    fn get_package_json_info(&self, pkg_json_path: &str) -> Option<P<packagejson::InfoCacheEntry>>;
    fn get_default_resolution_mode_for_file(&self, file: P<SourceFile>) -> ResolutionMode;
    fn get_resolved_module_from_module_specifier(
        &self,
        file: P<SourceFile>,
        module_specifier: P<Node>,
    ) -> Option<P<ResolvedModule>>;
    fn get_mode_for_usage_location(
        &self,
        file: P<SourceFile>,
        module_specifier: P<Node>,
    ) -> ResolutionMode;

    // tsrs-only: the host's memo for `try_get_module_name_from_exports` under `options`, if it keeps one (see
    // `ExportsModuleNameCache`).
    fn exports_module_name_cache(
        &self,
        _options: &tsrs_core::CompilerOptions,
    ) -> Option<&ExportsModuleNameCache> {
        None
    }
}

/// tsrs-only: memo for `try_get_module_name_from_exports`, a pure function of its arguments and of the
/// program (its compiler options, package.json contents and output paths).
///
/// Declaration emit asks for the same (module file, package) pair from every file that prints a type from
/// that module, and each answer walks the package's `exports` map. A host returns one only if it lives as
/// long as the program it describes, and only for that program's own options object.
#[derive(Default)]
pub struct ExportsModuleNameCache(std::sync::Mutex<FxHashMap<String, String>>);

impl ExportsModuleNameCache {
    pub(crate) fn get_or_compute(&self, key: String, compute: impl FnOnce() -> String) -> String {
        if let Some(result) = self.0.lock().unwrap().get(&key) {
            return result.clone();
        }
        let result = compute();
        self.0.lock().unwrap().insert(key, result.clone());
        result
    }
}

/// Go `ImportModuleSpecifierPreference` (a string type; `as_str()` is the Go value).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum ImportModuleSpecifierPreference {
    #[default]
    None, // "" !!!
    Shortest,
    ProjectRelative,
    Relative,
    NonRelative,
}

impl ImportModuleSpecifierPreference {
    pub fn as_str(self) -> &'static str {
        match self {
            ImportModuleSpecifierPreference::None => "",
            ImportModuleSpecifierPreference::Shortest => "shortest",
            ImportModuleSpecifierPreference::ProjectRelative => "project-relative",
            ImportModuleSpecifierPreference::Relative => "relative",
            ImportModuleSpecifierPreference::NonRelative => "non-relative",
        }
    }
}

/// Go `ImportModuleSpecifierEndingPreference` (a string type; `as_str()` is the Go value).
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
pub enum ImportModuleSpecifierEndingPreference {
    #[default]
    None, // "" !!!
    Auto,
    Minimal,
    Index,
    Js,
}

impl ImportModuleSpecifierEndingPreference {
    pub fn as_str(self) -> &'static str {
        match self {
            ImportModuleSpecifierEndingPreference::None => "",
            ImportModuleSpecifierEndingPreference::Auto => "auto",
            ImportModuleSpecifierEndingPreference::Minimal => "minimal",
            ImportModuleSpecifierEndingPreference::Index => "index",
            ImportModuleSpecifierEndingPreference::Js => "js",
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct UserPreferences {
    pub import_module_specifier_preference: ImportModuleSpecifierPreference,
    pub import_module_specifier_ending: ImportModuleSpecifierEndingPreference,
    pub auto_import_specifier_exclude_regexes: Vec<String>,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ModuleSpecifierOptions {
    pub override_import_mode: ResolutionMode,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum RelativePreferenceKind {
    #[default]
    Relative,
    NonRelative,
    Shortest,
    ExternalNonRelative,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum ModuleSpecifierEnding {
    #[default]
    Minimal,
    Index,
    JsExtension,
    TsExtension,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum MatchingMode {
    #[default]
    Exact,
    Directory,
    Pattern,
}

// preferences.go

/// Carries borrowed inputs into the preferred-ending callback.
///
/// Go's `getAllowedEndingsInPreferredOrder` func field closes over `(prefs, host, compilerOptions,
/// importingSourceFile, oldImportSpecifier)`; a `'static` boxed closure cannot capture the borrowed host and
/// options, so the owned captures are stored here and `get_allowed_endings_in_preferred_order(host,
/// compiler_options, mode)` (preferences.rs) takes the other two from the caller, which always holds the same
/// ones Go captured.
pub(crate) struct ModuleSpecifierPreferences {
    pub(crate) relative_preference: RelativePreferenceKind,
    pub(crate) exclude_regexes: Vec<String>,
    pub(crate) prefs: UserPreferences,
    pub(crate) importing_source_file: P<SourceFile>,
    pub(crate) old_import_specifier: String,
}

// util.go

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct RegexPatternCacheKey {
    pub(crate) pattern: String,
    pub(crate) case_insensitive: bool,
}

/// Go `regexPatternCache` guarded by `regexPatternCacheMu` (one `RwLock`).
pub(crate) static REGEX_PATTERN_CACHE: LazyLock<
    RwLock<FxHashMap<RegexPatternCacheKey, Option<P<regex::Regex>>>>,
> = LazyLock::new(|| RwLock::new(FxHashMap::default()));

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeModulePathParts {
    pub top_level_node_modules_index: usize,
    pub top_level_package_name_index: usize,
    pub package_root_index: Option<usize>,
    pub file_name_index: usize,
}

#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum NodeModulesPathParseState {
    BeforeNodeModules,
    NodeModules,
    Scope,
    PackageContent,
}

// specifiers.go

#[derive(Clone, Debug, Default)]
pub struct AmbientModuleInfo {
    pub(crate) name: String,
    pub(crate) symbol: Option<P<Symbol>>,
}

#[derive(Clone, Debug, Default)]
pub struct Info {
    pub(crate) use_case_sensitive_file_names: bool,
    pub(crate) importing_source_file_name: String,
    pub(crate) source_directory: String,
}

#[derive(Clone, Debug, Default)]
pub struct PkgJsonDirAttemptResult {
    pub(crate) module_file_to_try: String,
    pub(crate) package_root_path: String,
    pub(crate) blocked_by_exports: bool,
    pub(crate) verbatim_from_exports: bool,
}

#[derive(Clone, Debug)]
pub struct SpecPair {
    pub(crate) ending: ModuleSpecifierEnding,
    pub(crate) value: String,
}
