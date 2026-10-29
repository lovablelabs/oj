use std::ops::Deref;

use hashbrown::{Equivalent, HashMap};
use rustc_hash::FxBuildHasher;
use tsrs_core::collections::{OrderedMap, SyncMap};
use tsrs_core::tspath::Path;
use tsrs_core::{CompilerOptions, P, ResolutionMode};

use crate::packagejson::{self, InfoCache, InfoCacheEntry};
use crate::resolver::{DefaultResolver, ParsedPatterns, ResolverOptions, try_parse_patterns};
use crate::types::{
    ModeAwareCacheKey, ResolutionHost, ResolvedModule, ResolvedProjectReference,
    ResolvedTypeReferenceDirective,
};

pub type ModeAwareCache<T> = HashMap<ModeAwareCacheKey, T, FxBuildHasher>;

#[derive(Hash)]
struct ModeAwareCacheLookup<'a> {
    name: &'a str,
    mode: ResolutionMode,
}

impl Equivalent<ModeAwareCacheKey> for ModeAwareCacheLookup<'_> {
    fn equivalent(&self, key: &ModeAwareCacheKey) -> bool {
        self.name == key.name && self.mode == key.mode
    }
}

/// Looks up a mode-aware cache without allocating or requiring a static name.
pub fn mode_aware_cache_get<'a, T>(
    cache: &'a ModeAwareCache<T>,
    name: &str,
    mode: ResolutionMode,
) -> Option<&'a T> {
    cache.get(&ModeAwareCacheLookup { name, mode })
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ModuleResolutionCacheKey {
    pub(crate) containing_directory: String,
    pub(crate) module_name: String,
    pub(crate) resolution_mode: ResolutionMode,
    pub(crate) redirect_config_name: String,
}

/// The key's fields as borrowed values: a lookup builds this from the resolver's arguments instead of an owned
/// key, so a cache hit (most resolutions in a program) allocates nothing.
///
/// The owned key and the borrowed view
/// hash and compare through the same tuple.
pub trait ModuleResolutionKey {
    fn parts(&self) -> (&str, &str, ResolutionMode, &str);
}

impl ModuleResolutionKey for ModuleResolutionCacheKey {
    fn parts(&self) -> (&str, &str, ResolutionMode, &str) {
        (
            &self.containing_directory,
            &self.module_name,
            self.resolution_mode,
            &self.redirect_config_name,
        )
    }
}

impl ModuleResolutionKey for (&str, &str, ResolutionMode, &str) {
    fn parts(&self) -> (&str, &str, ResolutionMode, &str) {
        *self
    }
}

impl std::hash::Hash for ModuleResolutionCacheKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.parts().hash(state);
    }
}

impl std::hash::Hash for dyn ModuleResolutionKey + '_ {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.parts().hash(state);
    }
}

impl PartialEq for dyn ModuleResolutionKey + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.parts() == other.parts()
    }
}

impl Eq for dyn ModuleResolutionKey + '_ {}

impl<'a> std::borrow::Borrow<dyn ModuleResolutionKey + 'a> for ModuleResolutionCacheKey {
    fn borrow(&self) -> &(dyn ModuleResolutionKey + 'a) {
        self
    }
}

#[derive(Default)]
pub struct ModuleResolutionCache {
    cache: SyncMap<ModuleResolutionCacheKey, P<ResolvedModule>>,
}

impl ModuleResolutionCache {
    pub fn size(&self) -> usize {
        self.cache.size()
    }

    /// A lookup by the key's fields, without building an owned key.
    pub(crate) fn get_parts(
        &self,
        containing_directory: &str,
        module_name: &str,
        resolution_mode: ResolutionMode,
        redirect_config_name: &str,
    ) -> Option<P<ResolvedModule>> {
        let key = (containing_directory, module_name, resolution_mode, redirect_config_name);
        self.cache.load(&key as &dyn ModuleResolutionKey)
    }

    pub(crate) fn set(&self, key: ModuleResolutionCacheKey, value: P<ResolvedModule>) {
        self.cache.load_or_store(key, value);
    }
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct TypeRefDirectiveResolutionCacheKey {
    pub(crate) containing_directory: String,
    pub(crate) type_reference_name: String,
    pub(crate) resolution_mode: ResolutionMode,
    pub(crate) redirect_config_name: String,
    pub(crate) from_inferred_types_containing_file: bool,
}

#[derive(Default)]
pub struct TypeRefDirectiveResolutionCache {
    cache: SyncMap<TypeRefDirectiveResolutionCacheKey, P<ResolvedTypeReferenceDirective>>,
}

impl TypeRefDirectiveResolutionCache {
    pub fn size(&self) -> usize {
        self.cache.size()
    }

    pub(crate) fn get(
        &self,
        key: &TypeRefDirectiveResolutionCacheKey,
    ) -> Option<P<ResolvedTypeReferenceDirective>> {
        self.cache.load(key)
    }

    pub(crate) fn set(
        &self,
        key: TypeRefDirectiveResolutionCacheKey,
        value: P<ResolvedTypeReferenceDirective>,
    ) {
        self.cache.store(key, value);
    }
}

// Keyed by the identity (address) of the path mappings, like Go's pointer key.
#[derive(Default)]
pub struct ParsedPatternsCache {
    cache: SyncMap<usize, P<ParsedPatterns>>,
}

impl ParsedPatternsCache {
    pub fn size(&self) -> usize {
        self.cache.size()
    }

    pub(crate) fn get<V>(
        &self,
        path_mappings: Option<&OrderedMap<String, V>>,
    ) -> P<ParsedPatterns> {
        let key =
            path_mappings.map_or(0, |m| std::ptr::from_ref::<OrderedMap<String, V>>(m) as usize);
        match self.cache.load(&key) {
            Some(patterns) => patterns,
            None => self.cache.load_or_store(key, P::new(try_parse_patterns(path_mappings))).0,
        }
    }
}

pub struct ResolutionData {
    pub(crate) compiler_options: P<CompilerOptions>,
    pub(crate) typings_location: String,
    pub(crate) project_name: String,
    pub(crate) extra_extensions: Vec<String>,

    pub(crate) package_json_info_cache: P<InfoCache>,
}

pub(crate) fn new_resolution_data(opts: ResolverOptions<'_>) -> P<ResolutionData> {
    let package_json_info_cache = match opts.package_json_cache {
        Some(cache) => cache,
        None => P::new(packagejson::new_info_cache(
            opts.host.get_current_directory(),
            opts.host.fs().use_case_sensitive_file_names(),
        )),
    };
    P::new(ResolutionData {
        compiler_options: opts.compiler_options,
        typings_location: opts.typings_location,
        project_name: opts.project_name,
        extra_extensions: opts.extra_extensions,
        package_json_info_cache,
    })
}

impl ResolutionData {
    // Clone copies the package-json cache table without copying its entries.
    pub fn clone_data(&self) -> P<ResolutionData> {
        P::new(ResolutionData {
            compiler_options: self.compiler_options,
            typings_location: self.typings_location.clone(),
            project_name: self.project_name.clone(),
            extra_extensions: self.extra_extensions.clone(),
            package_json_info_cache: P::new(self.package_json_info_cache.clone_cache()),
        })
    }

    pub fn package_json_cache_entries(&self, f: impl FnMut(&Path, &P<InfoCacheEntry>) -> bool) {
        self.package_json_info_cache.range(f);
    }

    pub fn new_resolver<'h>(&'static self, host: &'h dyn ResolutionHost) -> DefaultResolver<'h> {
        DefaultResolver::new_from_resolution_data(P::from_static(self), host)
    }
}

pub(crate) fn get_redirect_config_name(redirect: Option<&dyn ResolvedProjectReference>) -> String {
    redirect_config_name(redirect).to_string()
}

pub(crate) fn redirect_config_name(redirect: Option<&dyn ResolvedProjectReference>) -> &str {
    match redirect {
        None => "",
        Some(redirect) => redirect.config_name(),
    }
}

impl Deref for DefaultResolver<'_> {
    type Target = ResolutionData;
    fn deref(&self) -> &ResolutionData {
        &self.resolution_data
    }
}
