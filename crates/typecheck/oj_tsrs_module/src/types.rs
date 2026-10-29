use std::fmt;

use bitflags::bitflags;
use tsrs_ast::Diagnostic;
use tsrs_core::tspath;
use tsrs_core::{CompilerOptions, P, ResolutionMode};
use tsrs_vfs::FS;

use crate::cache::ResolutionData;
use crate::resolver::DiagAndArgs;

pub trait ResolutionHost: Send + Sync {
    fn fs(&self) -> &dyn FS;
    fn get_current_directory(&self) -> &str;
}

pub trait Resolver: Send + Sync {
    fn resolve_module_name(
        &self,
        module_name: &str,
        containing_file: &str,
        resolution_mode: ResolutionMode,
        redirected_reference: Option<&dyn ResolvedProjectReference>,
    ) -> Result<(P<ResolvedModule>, Vec<DiagAndArgs>), String>;
    fn resolve_module_name_from_directory(
        &self,
        module_name: &str,
        containing_directory: &str,
        resolution_mode: ResolutionMode,
    ) -> Result<(P<ResolvedModule>, Vec<DiagAndArgs>), String>;
    fn resolve_type_reference_directive(
        &self,
        type_reference_directive_name: &str,
        containing_file: &str,
        resolution_mode: ResolutionMode,
        redirected_reference: Option<&dyn ResolvedProjectReference>,
    ) -> (P<ResolvedTypeReferenceDirective>, Vec<DiagAndArgs>);
    fn get_resolution_data(&self) -> P<ResolutionData>;
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct ModeAwareCacheKey {
    pub name: &'static str,
    pub mode: ResolutionMode,
}

pub trait ResolvedProjectReference: Send + Sync {
    fn config_name(&self) -> &str;
    fn compiler_options(&self) -> Option<P<CompilerOptions>>;
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub struct NodeResolutionFeatures: u32 {
        const Imports = 1 << 0;
        const SelfName = 1 << 1;
        const Exports = 1 << 2;
        const ExportsPatternTrailers = 1 << 3;
        // allowing `#/` root imports in package.json imports field
        // not supported until mass adoption - https://github.com/nodejs/node/pull/60864
        const ImportsPatternRoot = 1 << 4;

        const None = 0;
        const All = Self::Imports.bits() | Self::SelfName.bits() | Self::Exports.bits() | Self::ExportsPatternTrailers.bits() | Self::ImportsPatternRoot.bits();
        const Node16Default = Self::Imports.bits() | Self::SelfName.bits() | Self::Exports.bits() | Self::ExportsPatternTrailers.bits();
        const NodeNextDefault = Self::All.bits();
        const BundlerDefault = Self::Imports.bits() | Self::SelfName.bits() | Self::Exports.bits() | Self::ExportsPatternTrailers.bits() | Self::ImportsPatternRoot.bits();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct PackageId {
    pub name: &'static str,
    pub sub_module_name: &'static str,
    pub version: &'static str,
    pub peer_dependencies: &'static str,
}

impl fmt::Display for PackageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}{}", self.package_name(), self.version, self.peer_dependencies)
    }
}

impl PackageId {
    fn package_name(&self) -> String {
        if !self.sub_module_name.is_empty() {
            return format!("{}/{}", self.name, self.sub_module_name);
        }
        self.name.to_string()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ResolvedModule {
    pub resolution_diagnostics: &'static [P<Diagnostic>],
    pub resolved_file_name: &'static str,
    pub original_path: &'static str,
    pub extension: &'static str,
    pub resolved_using_ts_extension: bool,
    pub resolved_using_extra_extensions: bool,
    pub package_id: PackageId,
    pub is_external_library_import: bool,
    pub is_custom_resolution: bool,
    pub alternate_result: &'static str,
}

impl ResolvedModule {
    pub fn is_resolved(&self) -> bool {
        !self.resolved_file_name.is_empty()
    }
}

#[derive(Clone, Debug, Default)]
pub struct ResolvedTypeReferenceDirective {
    pub resolution_diagnostics: &'static [P<Diagnostic>],
    pub primary: bool,
    pub resolved_file_name: &'static str,
    pub original_path: &'static str,
    pub package_id: PackageId,
    pub is_external_library_import: bool,
}

impl ResolvedTypeReferenceDirective {
    pub fn is_resolved(&self) -> bool {
        !self.resolved_file_name.is_empty()
    }
}

bitflags! {
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, Default)]
    pub(crate) struct Extensions: u32 {
        const TypeScript = 1 << 0;
        const JavaScript = 1 << 1;
        const Declaration = 1 << 2;
        const Json = 1 << 3;

        const ImplementationFiles = Self::TypeScript.bits() | Self::JavaScript.bits();
    }
}

impl fmt::Display for Extensions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut result: Vec<&str> = Vec::with_capacity(self.bits().count_ones() as usize);
        if self.intersects(Extensions::TypeScript) {
            result.push("TypeScript");
        }
        if self.intersects(Extensions::JavaScript) {
            result.push("JavaScript");
        }
        if self.intersects(Extensions::Declaration) {
            result.push("Declaration");
        }
        if self.intersects(Extensions::Json) {
            result.push("JSON");
        }
        f.write_str(&result.join(", "))
    }
}

impl Extensions {
    pub(crate) fn array(self) -> Vec<&'static str> {
        let mut result = Vec::new();
        if self.intersects(Extensions::TypeScript) {
            result.extend_from_slice(tspath::SUPPORTED_TS_IMPLEMENTATION_EXTENSIONS);
        }
        if self.intersects(Extensions::JavaScript) {
            result.extend_from_slice(tspath::SUPPORTED_JS_EXTENSIONS_FLAT);
        }
        if self.intersects(Extensions::Declaration) {
            result.extend_from_slice(tspath::SUPPORTED_DECLARATION_EXTENSIONS);
        }
        if self.intersects(Extensions::Json) {
            result.push(tspath::EXTENSION_JSON);
        }
        result
    }
}
