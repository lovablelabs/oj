use std::sync::{Arc, Mutex};
use std::time::Instant;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::SourceFile;
use tsrs_compiler::Program;
use tsrs_core::arena::Region;
use tsrs_core::collections::{Set, SyncMap, new_set_from_items};
use tsrs_core::context::Context;
use tsrs_core::tspath::{self, ComparePathsOptions, Path};
use tsrs_core::{P, Tristate};
use tsrs_lsproto as lsproto;
use tsrs_module::packagejson::InfoCacheEntry;
use tsrs_module::symlinks::KnownSymlinks;
use tsrs_module::{
    self as module, DefaultResolver, ResolutionHost, ResolvedEntrypoint, ResolverOptions,
};
use tsrs_projectutil::dirty::{self, Cloneable, Shared, SharedMap};
use tsrs_projectutil::logging::LogTree;
use tsrs_vfs::vfsmatch::SpecMatcher;

use super::aliasresolver::{PathAndFileName, new_alias_resolver};
use super::export::Export;
use super::extract::new_export_extractor;
use super::index::Index;
use super::util::{
    PathFunc, add_package_json_dependencies, add_project_reference_output_mappings,
    create_checker_pool, get_module_resolver, get_package_names_in_node_modules,
    get_package_realpath_funcs, get_resolved_package_names,
};
use crate::lsconv;
use crate::lsutil::UserPreferences;

// registry.go:32
// Go `interface { fmt.Stringer }` used as a map key. Its only implementation is `project.ID` (a string), so the
// interface value is that string; project converts with `ProjectID(id.0)` / `ID(project_id.0)`.
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct ProjectID(pub String);

impl ProjectID {
    fn string(&self) -> String {
        self.0.clone()
    }
}

impl std::fmt::Display for ProjectID {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

pub type ToPath = Arc<dyn Fn(&str) -> Path + Send + Sync>;

// registry.go:36
fn known_recursive_search_packages() -> &'static Set<String> {
    static SET: std::sync::LazyLock<Set<String>> = std::sync::LazyLock::new(|| {
        new_set_from_items(
            [
                "@material-ui/core",
                "@material-ui/icons",
                "@sap/cds",
                "@testing-library/react-native",
                "ajv",
                "asap",
                "async",
                "aws-sdk",
                "braintree-web",
                "core-js",
                "core-js-pure",
                "crypto-js",
                "cypress-mochawesome-reporter",
                "dd-trace",
                "dumi",
                "dva",
                "egg-mock",
                "electron-log",
                "es-abstract",
                "es6-promise",
                "eslint-config-taro",
                "expo",
                "expo-router",
                "flow-remove-types",
                "gatsby",
                "glamor",
                "gluegun",
                "graphology-indices",
                "graphology-traversal",
                "graphology-utils",
                "jest-expo",
                "lodash",
                "lodash-es",
                "moment",
                "mz",
                "next",
                "pdfjs-dist",
                "protobufjs",
                "react-app-polyfill",
                "react-dev-utils",
                "react-devtools-inline",
                "recast",
                "semver",
                "stylelint-config-html",
                "umi",
                "web3-provider-engine",
                "webpack",
            ]
            .map(std::string::ToString::to_string),
        )
    });
    &SET
}

// registry.go:86
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum NewProgramStructure {
    #[default]
    False,
    SameFileNames,
    DifferentFileNames,
}

// registry.go:98
// bucketBuildPreferences holds user preferences that affect how a bucket is
// built. When any of these change between builds, the bucket must be rebuilt.
// Adding a new preference here automatically integrates it into the rebuild
// checks via Equal.
#[derive(Clone, Debug, Default)]
pub struct BucketBuildPreferences {
    file_exclude_patterns: Vec<String>,
    auto_import_entrypoint_directory_search: Tristate,
}

// registry.go:103
fn bucket_build_preferences_from_user_preferences(
    prefs: &UserPreferences,
) -> BucketBuildPreferences {
    BucketBuildPreferences {
        file_exclude_patterns: prefs.auto_import_file_exclude_patterns.clone().unwrap_or_default(),
        auto_import_entrypoint_directory_search: prefs.auto_import_entrypoint_directory_search,
    }
}

impl BucketBuildPreferences {
    // registry.go:110
    fn equal(&self, other: &BucketBuildPreferences) -> bool {
        tsrs_core::unordered_equal(&self.file_exclude_patterns, &other.file_exclude_patterns)
            && self.auto_import_entrypoint_directory_search
                == other.auto_import_entrypoint_directory_search
    }
}

// Go `*collections.Set` nil semantics (a nil set is empty; `Clone` keeps nil; `Equals` distinguishes nil).
fn set_len(s: Option<&Set<String>>) -> usize {
    s.map_or(0, tsrs_core::collections::Set::len)
}

fn set_has(s: Option<&Set<String>>, key: &str) -> bool {
    s.is_some_and(|s| s.m.contains(key))
}

fn set_equals(a: Option<&Set<String>>, b: Option<&Set<String>>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.equals(b),
        _ => false,
    }
}

fn set_is_subset_of(a: Option<&Set<String>>, b: Option<&Set<String>>) -> bool {
    let Some(a) = a else {
        return true;
    };
    a.m.iter().all(|k| set_has(b, k))
}

// registry.go:130
// BucketState represents the dirty state of a bucket.
// In general, a bucket can be used for an auto-imports request if it is clean
// or if the only edited file is the one that was requested for auto-imports.
// Most edits within a file will not change the imports available to that file.
// However, one exception causes the bucket to be rebuilt after a change to a
// single file: local files are newly added to the project by a manual import.
// This can only happen after a full (non-clone) program update. When this
// happens, the `newProgramStructure` flag is set until the next time the bucket
// is rebuilt, when this condition will be checked.
#[derive(Clone, Debug, Default)]
pub struct BucketState {
    // dirtyFile is the file that was edited last, if any. It does not necessarily
    // indicate that no other files have been edited, so it should be ignored if
    // `multipleFilesDirty` is set. It should not be used for node_modules buckets,
    // which rely on `dirtyPackages` instead.
    dirty_file: Path,
    multiple_files_dirty: bool,
    new_program_structure: NewProgramStructure,
    // buildPreferences holds the user preferences that were in effect when
    // the bucket was built. If changed, the bucket should be rebuilt.
    build_preferences: BucketBuildPreferences,
    // dirtyPackages is the set of package names that need to be re-indexed.
    // This is used for granular updates: when a file in a local workspace package
    // changes, only that package needs to be re-extracted rather than rebuilding
    // the entire node_modules bucket.
    // If nil, no granular updates are pending.
    // If set but multipleFilesDirty is true, the entire bucket needs to be rebuilt.
    dirty_packages: Option<Set<String>>,
    // recursiveSearchPackages tracks which packages were recursively directory-searched
    // when the bucket was built. nil means all non-exports packages were searched
    // (e.g. when the autoImportEntrypointDirectorySearch preference is enabled).
    // A non-nil set lists only the specific packages that were searched.
    // Used for rebuild detection: a rebuild is triggered when target packages are
    // not a subset of the currently searched packages.
    recursive_search_packages: Option<Set<String>>,
}

impl BucketState {
    // registry.go:173
    pub fn dirty(&self) -> bool {
        self.multiple_files_dirty
            || !self.dirty_file.is_empty()
            || self.new_program_structure > NewProgramStructure::False
            || set_len(self.dirty_packages.as_ref()) > 0
    }

    // registry.go:184
    pub fn dirty_packages(&self) -> Option<Set<String>> {
        if self.multiple_files_dirty {
            return None;
        }
        self.dirty_packages.clone()
    }

    // registry.go:191
    pub fn recursive_search_packages(&self) -> Option<Set<String>> {
        self.recursive_search_packages.clone()
    }

    // registry.go:195
    fn possibly_needs_rebuild_for_file(&self, file: &Path, preferences: &UserPreferences) -> bool {
        self.new_program_structure > NewProgramStructure::False
            || self.has_dirty_file_besides(file)
            || !self
                .build_preferences
                .equal(&bucket_build_preferences_from_user_preferences(preferences))
            || set_len(self.dirty_packages.as_ref()) > 0
    }

    // registry.go:202
    fn has_dirty_file_besides(&self, file: &Path) -> bool {
        self.multiple_files_dirty || !self.dirty_file.is_empty() && self.dirty_file != *file
    }
}

// registry.go:210
// recursiveSearchSubset reports whether target is a subset of current.
// nil represents "all packages" — a superset of every concrete set.
// Returns true if the current set already covers everything the target needs,
// meaning no rebuild is required for recursive search purposes.
fn recursive_search_subset(target: Option<&Set<String>>, current: Option<&Set<String>>) -> bool {
    let Some(_) = target else {
        // Target wants all packages searched — only satisfied if current is also all.
        return current.is_none();
    };
    if current.is_none() {
        // Current searched all packages, so any concrete target is satisfied.
        return true;
    }
    set_is_subset_of(target, current)
}

pub type PackageFiles = FxHashMap<String, Option<Arc<FxHashMap<Path, String>>>>;

// registry.go:223
#[derive(Clone, Default)]
pub struct RegistryBucket {
    state: BucketState,

    // Paths maps file paths to package names. For project buckets, the package name
    // is always empty string. For node_modules buckets, this enables reverse lookup
    // from path to package for granular updates. Only paths for local workspace
    // packages (symlinked and within the workspace root) have entries here, since
    // their realpaths are outside node_modules and need reverse lookup for dirty
    // detection.
    //
    // Paths is considered immutable after the bucket is finalized.
    // It should be fully replaced rather than mutated while changing a bucket.
    paths: Arc<FxHashMap<Path, String>>,
    // PackageFiles maps package names to their file paths and file names.
    // All package directory names in node_modules are keys; indexed packages have
    // non-nil maps with path→fileName entries, unindexed packages have nil maps.
    // This enables efficient removal of a package's files during granular updates
    // without iterating through all entries. Only defined for node_modules buckets.
    //
    // PackageFiles is considered immutable after the bucket is finalized.
    // It should be fully replaced rather than mutated while changing a bucket.
    pub(crate) package_files: Option<Arc<PackageFiles>>,
    // ResolvedPackageNames is only defined for project buckets. It is the set of
    // package names that were resolved from imports in the project's program files.
    // This is passed to node_modules buckets so they include packages that are
    // directly imported even if not listed in package.json dependencies.
    //
    // ResolvedPackageNames is considered immutable after the bucket is finalized.
    // It should be fully replaced rather than mutated while changing a bucket.
    pub(crate) resolved_package_names: Option<Set<String>>,
    // DependencyNames is only defined for node_modules buckets. It is the set of
    // package names that will be included in the bucket if present in the directory,
    // computed from package.json dependencies plus resolved package names from
    // active programs. If nil, all packages are included because at least one open
    // file has access to this node_modules directory without being filtered by a
    // package.json.
    //
    // DependencyNames is considered immutable after the bucket is finalized.
    // It should be fully replaced rather than mutated while changing a bucket.
    dependency_names: Option<Set<String>>,
    // AmbientModuleNames is only defined for node_modules buckets. It is the set of
    // ambient module names found while extracting exports in the bucket.
    //
    // AmbientModuleNames is considered immutable after the bucket is finalized.
    // It should be fully replaced rather than mutated while changing a bucket.
    ambient_module_names: Arc<FxHashMap<String, Vec<String>>>,
    // Index is considered immutable after the bucket is finalized.
    // It should be cloned and replaced rather than mutated while changing a bucket.
    pub(crate) index: Option<Arc<Index<Arc<Export>>>>,
}

// registry.go:277
fn new_registry_bucket() -> RegistryBucket {
    RegistryBucket {
        state: BucketState {
            multiple_files_dirty: true,
            new_program_structure: NewProgramStructure::DifferentFileNames,
            ..Default::default()
        },
        ..Default::default()
    }
}

// registry.go:286
impl Cloneable for RegistryBucket {
    fn clone_value(&self) -> RegistryBucket {
        self.clone()
    }
}

impl RegistryBucket {
    // registry.go:301
    // markProjectFileDirty should only be called within a Change call on the dirty map.
    // Buckets are considered immutable once in a finalized registry. Should only
    // be used for project buckets.
    fn mark_project_file_dirty(&mut self, file: &Path) {
        if self.state.has_dirty_file_besides(file) {
            self.state.multiple_files_dirty = true;
        } else {
            self.state.dirty_file = file.clone();
        }
    }

    // registry.go:313
    // markNodeModulesDirty should only be called within a Change call on the dirty map.
    // Buckets are considered immutable once in a finalized registry. If packageName is
    // non-empty, that package is marked for granular update. Otherwise, the entire bucket
    // is marked dirty.
    fn mark_node_modules_dirty(&mut self, package_name: &str) {
        if self.state.multiple_files_dirty {
            return;
        }
        if package_name.is_empty() {
            self.state.multiple_files_dirty = true;
            return;
        }
        // Track the package for granular updates
        self.state.dirty_packages.get_or_insert_with(Set::new).add(package_name.to_string());
    }
}

// registry.go:329
#[derive(Clone)]
pub struct Directory {
    name: String,
    pub(crate) package_json: P<InfoCacheEntry>,
    has_node_modules: bool,
}

// registry.go:335
impl Cloneable for Directory {
    fn clone_value(&self) -> Directory {
        Directory {
            name: self.name.clone(),
            package_json: self.package_json,
            has_node_modules: self.has_node_modules,
        }
    }
}

pub type SpecifierCache = Arc<SyncMap<Path, String>>;

// registry.go:343
pub struct Registry {
    to_path: ToPath,
    user_preferences: UserPreferences,

    // exports      map[tspath.Path][]*RawExport
    pub(crate) directories: SharedMap<Path, Directory>,

    pub(crate) node_modules: SharedMap<Path, RegistryBucket>,
    pub(crate) projects: SharedMap<ProjectID, RegistryBucket>,
    unique_package_count: usize,

    // entrypoints maps from file path to the resolved entrypoints for that file, shared across all node_modules buckets.
    pub(crate) entrypoints: Arc<FxHashMap<Path, Vec<Arc<ResolvedEntrypoint>>>>,

    // specifierCache maps from importing file to target file to specifier.
    pub(crate) specifier_cache: Arc<FxHashMap<Path, SpecifierCache>>,

    // Memory regions (docs/LSP.md "Memory plan"), not in Go: the regions holding the arena values this version
    // refers to (its directories' package.json entries), shared with the older and newer versions that keep them.
    #[expect(
        dead_code,
        reason = "never read: holding the regions keeps them alive as long as this version"
    )]
    regions: Vec<Region>,
}

// registry.go:360
pub fn new_registry(to_path: ToPath, preferences: UserPreferences) -> Arc<Registry> {
    Arc::new(Registry {
        to_path,
        user_preferences: preferences,
        directories: Arc::default(),
        node_modules: Arc::default(),
        projects: Arc::default(),
        unique_package_count: 0,
        entrypoints: Arc::default(),
        specifier_cache: Arc::default(),
        regions: Vec::new(),
    })
}

// Go's nil-receiver `(*Registry).IsPreparedForImportingFile` (registry.go:368): a nil registry is never prepared.
pub trait RegistryExt {
    fn is_prepared_for_importing_file(
        &self,
        file_name: &str,
        project_id: &ProjectID,
        preferences: &UserPreferences,
    ) -> bool;
}

impl RegistryExt for Option<Arc<Registry>> {
    fn is_prepared_for_importing_file(
        &self,
        file_name: &str,
        project_id: &ProjectID,
        preferences: &UserPreferences,
    ) -> bool {
        match self {
            None => false,
            Some(r) => r.is_prepared_for_importing_file(file_name, project_id, preferences),
        }
    }
}

impl Registry {
    fn to_path(&self, file_name: &str) -> Path {
        (self.to_path)(file_name)
    }

    // registry.go:368
    pub fn is_prepared_for_importing_file(
        &self,
        file_name: &str,
        project_id: &ProjectID,
        preferences: &UserPreferences,
    ) -> bool {
        let Some(project_bucket) = self.projects.get(project_id) else {
            return false;
        };
        let path = self.to_path(file_name);
        if project_bucket.state.possibly_needs_rebuild_for_file(&path, preferences) {
            return false;
        }

        let mut dir_path = path.get_directory_path();
        loop {
            if let Some(dir_bucket) = self.node_modules.get(&dir_path)
                && dir_bucket.state.possibly_needs_rebuild_for_file(&path, preferences)
            {
                return false;
            }
            let parent = dir_path.get_directory_path();
            if parent == dir_path {
                break;
            }
            dir_path = parent;
        }
        true
    }

    // registry.go:397
    pub fn node_modules_directories(&self) -> FxHashMap<Path, String> {
        let mut dirs = FxHashMap::default();
        // pure inserts into the returned map; Go ranges the map too
        dirs.extend(self.directories.iter().filter_map(|(dir_path, dir)| {
            if dir.has_node_modules {
                Some((
                    Path::new(tspath::combine_paths(dir_path.as_str(), &["node_modules"])),
                    tspath::combine_paths(&dir.name, &["node_modules"]),
                ))
            } else {
                None
            }
        }));
        dirs
    }

    // registry.go:407
    // Go's `Clone` (`clone` is taken by Rust). The host only lives for the duration of the call.
    //
    // Memory regions (not in Go, where the GC collects an update's garbage): everything the update allocates in
    // the arena (module and alias resolvers, the extraction checkers' types and symbols, resolution caches) goes
    // to a scratch region freed before this returns; the finished registry holds no reference into it. The
    // package.json entries the new version keeps in `directories` go to a region of their own (`regions`).
    pub fn clone_registry(
        &self,
        ctx: &Context,
        change: &RegistryChange,
        host: &dyn RegistryCloneHost,
        logger: LogTree,
    ) -> Arc<Registry> {
        let scratch = Region::new(SCRATCH_REGION_FIRST_CHUNK);
        let scratch_scope = scratch.enter();
        let registry = self.clone_registry_in_scratch(ctx, change, host, logger);
        drop(scratch_scope);
        drop(scratch);
        registry
    }

    fn clone_registry_in_scratch(
        &self,
        ctx: &Context,
        change: &RegistryChange,
        host: &dyn RegistryCloneHost,
        logger: LogTree,
    ) -> Arc<Registry> {
        let start = Instant::now();
        let mut logger = logger;
        if !logger.is_nil() {
            logger = logger.fork("Building autoimport registry");
        }
        let mut builder = new_registry_builder(self, assume_static(host));
        if let Some(user_preferences) = &change.user_preferences {
            builder.user_preferences = user_preferences.clone();
            if !tsrs_core::unordered_equal(
                builder
                    .user_preferences
                    .auto_import_specifier_exclude_regexes
                    .as_deref()
                    .unwrap_or(&[]),
                self.user_preferences
                    .auto_import_specifier_exclude_regexes
                    .as_deref()
                    .unwrap_or(&[]),
            ) {
                builder.specifier_cache.clear();
            }
        }
        builder.update_bucket_and_directory_existence(change, &logger);
        builder.mark_buckets_dirty(change, &logger);
        if !change.requested_file.is_empty() {
            builder.update_indexes(ctx, change, &logger);
        }
        if !logger.is_nil() {
            logger.logf(format_args!("Built autoimport registry in {:?}", start.elapsed()));
        }
        let registry = builder.build();
        Arc::new(registry)
    }

    // registry.go:452
    pub fn get_cache_stats(&self) -> CacheStats {
        let mut stats =
            CacheStats { unique_package_count: self.unique_package_count, ..Default::default() };

        // the stats Vec is sorted by unique bucket name after; Go ranges the map too
        stats.project_buckets.extend(self.projects.iter().map(|(project_id, bucket)| {
            let export_count = bucket.index.as_ref().map_or(0, |i| i.entries.len());
            BucketStats {
                name: project_id.string(),
                export_count,
                file_count: bucket.paths.len(),
                state: bucket.state.clone(),
                dependency_names: bucket.dependency_names.clone(),
                package_names: None,
            }
        }));

        #[expect(
            clippy::iter_over_hash_type,
            reason = "the stats Vec is sorted by unique bucket name after; the inner loop only adds to a set and sums; Go ranges the map too"
        )]
        for (path, bucket) in self.node_modules.iter() {
            let export_count = bucket.index.as_ref().map_or(0, |i| i.entries.len());
            // Derive PackageNames from PackageFiles keys
            let mut package_names: Option<Set<String>> = None;
            let mut file_count = 0;
            if let Some(package_files) = &bucket.package_files {
                let mut names = tsrs_core::collections::new_set_with_size_hint(package_files.len());
                for (name, paths) in package_files.iter() {
                    names.add(name.clone());
                    file_count += paths.as_ref().map_or(0, |p| p.len());
                }
                package_names = Some(names);
            }
            stats.node_modules_buckets.push(BucketStats {
                name: path.to_string(),
                export_count,
                file_count,
                state: bucket.state.clone(),
                dependency_names: bucket.dependency_names.clone(),
                package_names,
            });
        }

        stats.project_buckets.sort_by(|a, b| a.name.cmp(&b.name));
        stats.node_modules_buckets.sort_by(|a, b| a.name.cmp(&b.name));

        stats
    }
}

const SCRATCH_REGION_FIRST_CHUNK: usize = 1 << 20;

// The regions holding the package.json entries of `directories` (each entry was allocated in the package.json
// region of the update that read it).
fn regions_of_directories(directories: &SharedMap<Path, Directory>) -> Vec<Region> {
    let mut regions: Vec<Region> = Vec::new();
    for region in Region::containing_all(directories.values().map(|dir| dir.package_json.addr()))
        .into_iter()
        .flatten()
    {
        if !regions.iter().any(|r| r.ptr_eq(&region)) {
            regions.push(region);
        }
    }
    regions
}

// The registry builder keeps the clone host for the duration of `clone_registry` only (see there).
//
// Still a lifetime extension (issue #646): the module resolver now borrows its host for any lifetime, but the alias
// resolver built from this host is arena-allocated and handed to `new_checker(&'static dyn Program)`, and
// `wrapvfs::wrap` and `get_package_realpath_funcs` take `&'static dyn FS`. Removing this is blocked on the
// program/checker ownership item of #646; a lifetime here would have to reach the checker's stored program.
fn assume_static(host: &dyn RegistryCloneHost) -> &'static dyn RegistryCloneHost {
    // SAFETY: every value built from the host (resolvers, alias resolvers, checkers, extractors) lives in the
    // update's scratch region or on the stack of `clone_registry`, which frees them before it returns, before the
    // caller's host goes away; the finished registry holds no reference to any of them.
    unsafe { std::mem::transmute::<&dyn RegistryCloneHost, &'static dyn RegistryCloneHost>(host) }
}

// registry.go:433
pub struct BucketStats {
    pub name: String,
    pub export_count: usize,
    pub file_count: usize,
    pub state: BucketState,
    pub dependency_names: Option<Set<String>>,
    pub package_names: Option<Set<String>>,
}

// registry.go:442
#[derive(Default)]
pub struct CacheStats {
    pub project_buckets: Vec<BucketStats>,
    pub node_modules_buckets: Vec<BucketStats>,
    pub unique_package_count: usize,
}

// registry.go:501
#[derive(Default)]
pub struct RegistryChange {
    pub requested_file: Path,
    pub requested_file_name: String,
    pub open_files: FxHashMap<Path, String>,
    pub changed: Set<lsproto::DocumentUri>,
    pub created: Set<lsproto::DocumentUri>,
    pub deleted: Set<lsproto::DocumentUri>,
    // RebuiltPrograms maps from project ID to:
    //   - true: the program was rebuilt with a different set of file names
    //   - false: the program was rebuilt but the set of file names is unchanged
    pub rebuilt_programs: FxHashMap<ProjectID, bool>,
    pub user_preferences: Option<UserPreferences>,
}

// registry.go:514 (Go embeds module.ResolutionHost and repeats its FS(); `ResolutionHost::fs` covers both)
pub trait RegistryCloneHost: ResolutionHost {
    fn get_default_project(&self, path: &Path) -> (Option<ProjectID>, Option<&'static Program>);
    fn get_program_for_project(&self, project_id: &ProjectID) -> Option<&'static Program>;
    fn get_package_json(&self, file_name: &str) -> P<InfoCacheEntry>;
    fn get_source_file(&self, file_name: &str, path: &Path) -> Option<P<SourceFile>>;
    fn dispose(&self);
}

type SpecifierCacheBuilder = dirty::MapBuilder<Path, SpecifierCache, SpecifierCache>;
type EntrypointsBuilder =
    dirty::MapBuilder<Path, Vec<Arc<ResolvedEntrypoint>>, Vec<Arc<ResolvedEntrypoint>>>;

// registry.go:524
struct RegistryBuilder<'r> {
    host: &'static dyn RegistryCloneHost,
    base: &'r Registry,

    user_preferences: UserPreferences,
    directories: dirty::Map<Path, Directory>,
    node_modules: dirty::Map<Path, RegistryBucket>,
    projects: dirty::Map<ProjectID, RegistryBucket>,
    specifier_cache: SpecifierCacheBuilder,

    unique_package_count: usize,
    entrypoints: EntrypointsBuilder,

    // Memory regions, not in Go: where this update's new package.json entries for `directories` go (created on
    // first use; see `clone_registry`).
    package_json_region: Option<Region>,
}

// registry.go:539
fn new_registry_builder<'r>(
    registry: &'r Registry,
    host: &'static dyn RegistryCloneHost,
) -> RegistryBuilder<'r> {
    RegistryBuilder {
        host,
        base: registry,

        user_preferences: registry.user_preferences.clone(),
        directories: dirty::new_map(Arc::clone(&registry.directories)),
        node_modules: dirty::new_map(Arc::clone(&registry.node_modules)),
        projects: dirty::new_map(Arc::clone(&registry.projects)),
        specifier_cache: dirty::new_map_builder(Arc::clone(&registry.specifier_cache), |v| v),
        unique_package_count: registry.unique_package_count,
        entrypoints: dirty::new_map_builder(Arc::clone(&registry.entrypoints), |v| v),
        package_json_region: None,
    }
}

// registry.go:782 (nodeModulesBucketTask, declared inside updateIndexes)
struct NodeModulesBucketTask {
    entry: Arc<dirty::MapEntry<Path, RegistryBucket>>,
    dependency_names: Option<Set<String>>,
    dir_name: String,
    dir_path: Path,

    // For granular updates.
    is_update: bool,
    existing_bucket: Option<Shared<RegistryBucket>>,
    dirty_packages: Option<Set<String>>,

    // Filled by discovery.
    package_names: Option<Set<String>>,
    directory_package_names: Option<Set<String>>,
    discovered: Vec<Arc<DiscoveredPackage>>,
}

impl RegistryBuilder<'_> {
    // registry.go:555
    fn build(&self) -> Registry {
        let directories = self.directories.finalize().0;
        let regions = regions_of_directories(&directories);
        Registry {
            to_path: Arc::clone(&self.base.to_path),
            user_preferences: self.user_preferences.clone(),
            directories,
            node_modules: self.node_modules.finalize().0,
            projects: self.projects.finalize().0,
            specifier_cache: self.specifier_cache.build(),
            unique_package_count: self.unique_package_count,
            entrypoints: self.entrypoints.build(),
            regions,
        }
    }

    // Allocates `get_package_json`'s entry for `directories` in this update's package.json region.
    fn get_directory_package_json(&mut self, package_json_file_name: &str) -> P<InfoCacheEntry> {
        let region = self.package_json_region.get_or_insert_with(|| Region::new(0));
        let _scope = region.enter();
        self.host.get_package_json(package_json_file_name)
    }

    // registry.go:568
    fn update_bucket_and_directory_existence(&mut self, change: &RegistryChange, logger: &LogTree) {
        let start = Instant::now();
        // Go maps (random iteration order).
        let mut needed_projects: FxHashSet<ProjectID> = FxHashSet::default();
        let mut needed_directories: FxHashMap<Path, String> = FxHashMap::default();
        let mut add_needed_directories = |path: &Path, file_name: &str| {
            if tspath::is_dynamic_file_name(file_name) {
                return;
            }
            let mut dir = file_name.to_string();
            let mut dir_path = path.clone();
            loop {
                dir = tspath::get_directory_path(&dir);
                let last_dir_path = dir_path.clone();
                dir_path = dir_path.get_directory_path();
                if dir_path == last_dir_path || needed_directories.contains_key(&dir_path) {
                    break;
                }
                needed_directories.insert(dir_path.clone(), dir.clone());
            }
        };
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: fills the needed project set and directory map keyed by path; Go ranges the map too"
        )]
        for (path, file_name) in &change.open_files {
            if let (Some(project_id), _) = self.host.get_default_project(path) {
                needed_projects.insert(project_id);
            }
            if tspath::is_dynamic_file_name(file_name) {
                continue;
            }
            add_needed_directories(path, file_name);

            if !self.specifier_cache.has(path) {
                self.specifier_cache.set(path.clone(), Arc::new(SyncMap::default()));
            }
        }

        if !change.requested_file.is_empty() {
            add_needed_directories(&change.requested_file, &change.requested_file_name);
            if let (Some(project_id), _) = self.host.get_default_project(&change.requested_file) {
                needed_projects.insert(project_id);
            }
            if !self.specifier_cache.has(&change.requested_file) {
                self.specifier_cache
                    .set(change.requested_file.clone(), Arc::new(SyncMap::default()));
            }
        }

        #[expect(
            clippy::iter_over_hash_type,
            reason = "keys are only deleted from specifier_cache; Go ranges the map too"
        )]
        for path in self.base.specifier_cache.keys() {
            if !change.open_files.contains_key(path) && *path != change.requested_file {
                self.specifier_cache.delete(path.clone());
            }
        }

        let mut added_projects: Vec<ProjectID> = Vec::new();
        let mut removed_projects: Vec<ProjectID> = Vec::new();
        // core.DiffMapsFunc(base.projects, neededProjects, nil onChanged)
        // order-independent: adds missing buckets to the projects map; the order only reaches log lines (Go's DiffMapsFunc ranges a map too)
        added_projects.extend(needed_projects.iter().filter_map(|project_id| {
            if self.base.projects.contains_key(project_id) {
                None
            } else {
                // Need and don't have
                self.projects.add(project_id.clone(), Shared::new(new_registry_bucket()));
                Some(project_id.clone())
            }
        }));
        // order-independent: deletes unneeded buckets; the order only reaches log lines (Go's DiffMapsFunc ranges a map too)
        removed_projects.extend(self.base.projects.keys().filter_map(|project_id| {
            if needed_projects.contains(project_id) {
                None
            } else {
                // Have and don't need
                self.projects.delete(project_id);
                Some(project_id.clone())
            }
        }));
        if !logger.is_nil() {
            for project_id in &added_projects {
                logger.logf(format_args!("Added project: {project_id}"));
            }
            for project_id in &removed_projects {
                logger.logf(format_args!("Removed project: {project_id}"));
            }
        }

        let host = self.host;
        let update_directory = |b: &mut RegistryBuilder,
                                dir_path: &Path,
                                dir_name: &str,
                                package_json_changed: bool| {
            let package_json_file_name = tspath::combine_paths(dir_name, &["package.json"]);
            let has_node_modules =
                host.fs().directory_exists(&tspath::combine_paths(dir_name, &["node_modules"]));
            if let Some(entry) = b.directories.get(dir_path) {
                // (Go reads the package.json inside the apply function; it is read first here so that the entry is
                // allocated in this update's package.json region.)
                if entry.value().is_some_and(|dir| {
                    package_json_changed || dir.has_node_modules != has_node_modules
                }) {
                    let package_json = b.get_directory_package_json(&package_json_file_name);
                    entry.change_if(
                        |dir| package_json_changed || dir.has_node_modules != has_node_modules,
                        |dir| {
                            dir.package_json = package_json;
                            dir.has_node_modules = has_node_modules;
                        },
                    );
                }
            } else {
                let package_json = b.get_directory_package_json(&package_json_file_name);
                b.directories.add(
                    dir_path.clone(),
                    Shared::new(Directory {
                        name: dir_name.to_string(),
                        package_json,
                        has_node_modules,
                    }),
                );
            }

            if has_node_modules {
                if b.node_modules.get(dir_path).is_none() {
                    b.node_modules.add(dir_path.clone(), Shared::new(new_registry_bucket()));
                }
            } else {
                b.node_modules.try_delete(dir_path);
            }
        };

        let mut added_node_modules_dirs: Vec<Path> = Vec::new();
        let mut removed_node_modules_dirs: Vec<Path> = Vec::new();
        let package_json_changed = |dir_name: &str| -> bool {
            let uri = lsconv::file_name_to_document_uri(&tspath::combine_paths(
                dir_name,
                &["package.json"],
            ));
            change.changed.has(&uri) || change.deleted.has(&uri) || change.created.has(&uri)
        };
        // core.DiffMapsFunc(base.directories, neededDirectories, equalValues, onAdded, onRemoved, onChanged)
        let base_directories = Arc::clone(&self.base.directories);
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: updates each directory entry by its own key; the order only reaches log lines (Go's DiffMapsFunc ranges a map too)"
        )]
        for (dir_path, dir_name) in &needed_directories {
            match base_directories.get(dir_path) {
                None => {
                    // Need and don't have
                    let had_node_modules = self.base.node_modules.contains_key(dir_path);
                    update_directory(self, dir_path, dir_name, false);
                    if !logger.is_nil() {
                        logger.logf(format_args!("Added directory: {}", dir_path.as_str()));
                    }
                    if self.node_modules.get(dir_path).is_some() && !had_node_modules {
                        added_node_modules_dirs.push(dir_path.clone());
                    }
                }
                Some(dir) => {
                    let equal = !package_json_changed(dir_name)
                        && dir.has_node_modules
                            == host.fs().directory_exists(&tspath::combine_paths(
                                dir_name,
                                &["node_modules"],
                            ));
                    if !equal {
                        update_directory(self, dir_path, dir_name, package_json_changed(dir_name));
                        if !logger.is_nil() {
                            logger.logf(format_args!("Changed directory: {}", dir_path.as_str()));
                        }
                    }
                }
            }
        }
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: deletes each unneeded directory by its own key; the order only reaches log lines (Go's DiffMapsFunc ranges a map too)"
        )]
        for dir_path in base_directories.keys() {
            if !needed_directories.contains_key(dir_path) {
                // Have and don't need
                let had_node_modules = self.base.node_modules.contains_key(dir_path);
                self.directories.delete(dir_path);
                self.node_modules.try_delete(dir_path);
                if !logger.is_nil() {
                    logger.logf(format_args!("Removed directory: {}", dir_path.as_str()));
                }
                if had_node_modules {
                    removed_node_modules_dirs.push(dir_path.clone());
                }
            }
        }

        if !logger.is_nil() {
            for dir_path in &added_node_modules_dirs {
                logger.logf(format_args!("Added node_modules bucket: {}", dir_path.as_str()));
            }
            for dir_path in &removed_node_modules_dirs {
                logger.logf(format_args!("Removed node_modules bucket: {}", dir_path.as_str()));
            }
            logger.logf(format_args!("Updated buckets and directories in {:?}", start.elapsed()));
        }
    }

    // registry.go:707
    fn mark_buckets_dirty(&mut self, change: &RegistryChange, _logger: &LogTree) {
        // Mark new program structures
        #[expect(
            clippy::iter_over_hash_type,
            reason = "each project bucket is updated independently; Go ranges the map too"
        )]
        for (project_id, &new_file_names) in &change.rebuilt_programs {
            if let Some(bucket) = self.projects.get(project_id) {
                bucket.change(|bucket| {
                    bucket.state.new_program_structure = if new_file_names {
                        NewProgramStructure::DifferentFileNames
                    } else {
                        NewProgramStructure::SameFileNames
                    };
                });
            }
        }

        // Mark files dirty, bailing out if all buckets already have multiple files dirty
        let mut clean_node_modules_buckets: FxHashSet<Path> = FxHashSet::default();
        let mut clean_project_buckets: FxHashSet<ProjectID> = FxHashSet::default();
        self.node_modules.range(|entry| {
            if !entry.value().unwrap().state.multiple_files_dirty {
                clean_node_modules_buckets.insert(entry.key());
            }
            true
        });
        self.projects.range(|entry| {
            if !entry.value().unwrap().state.multiple_files_dirty {
                clean_project_buckets.insert(entry.key());
            }
            true
        });

        let mut mark_files_dirty = |b: &mut RegistryBuilder, uris: Vec<&lsproto::DocumentUri>| {
            if clean_node_modules_buckets.is_empty() && clean_project_buckets.is_empty() {
                return;
            }
            for uri in uris {
                let path = (b.base.to_path)(&uri.file_name());
                if !clean_node_modules_buckets.is_empty() {
                    // For node_modules, mark the bucket dirty if anything changes in the directory.
                    // The path could be either a symlink path (containing /node_modules/) or a realpath
                    // (for symlinked project references). Both are recorded in Paths for granular updates.
                    if let Some(node_modules_index) = path.find("/node_modules/") {
                        let dir_path = Path::from(&path[..node_modules_index]);
                        if clean_node_modules_buckets.contains(&dir_path) {
                            let entry = b.node_modules.get(&dir_path).unwrap();
                            // Look up the package name for granular updates
                            let package_name = entry
                                .value()
                                .unwrap()
                                .paths
                                .get(&path)
                                .cloned()
                                .unwrap_or_default();
                            entry.change(|bucket| bucket.mark_node_modules_dirty(&package_name));
                            if !entry.value().unwrap().state.multiple_files_dirty {
                                clean_node_modules_buckets.remove(&dir_path);
                            }
                        }
                    } else {
                        // Check if this path (possibly a realpath of a workspace package) is in any bucket's Paths.
                        // This handles local workspace packages where the realpath doesn't contain /node_modules/.
                        let bucket_dir_paths: Vec<Path> =
                            clean_node_modules_buckets.iter().cloned().collect();
                        for bucket_dir_path in bucket_dir_paths {
                            let entry = b.node_modules.get(&bucket_dir_path).unwrap();
                            let package_name = entry.value().unwrap().paths.get(&path).cloned();
                            if let Some(package_name) = package_name {
                                // Use the package name for granular updates
                                entry
                                    .change(|bucket| bucket.mark_node_modules_dirty(&package_name));
                                if !entry.value().unwrap().state.multiple_files_dirty {
                                    clean_node_modules_buckets.remove(&bucket_dir_path);
                                }
                            }
                        }
                    }
                }

                // For projects, mark the bucket dirty if the bucket contains the file directly.
                // Any other significant change, like a created failed lookup location, is
                // handled by newProgramStructure.
                let project_dir_paths: Vec<ProjectID> =
                    clean_project_buckets.iter().cloned().collect();
                for project_dir_path in project_dir_paths {
                    let entry = b.projects.get(&project_dir_path).unwrap();
                    if entry.value().unwrap().paths.contains_key(&path) {
                        // Project buckets don't use package-based granular updates
                        entry.change(|bucket| bucket.mark_project_file_dirty(&path));
                        if !entry.value().unwrap().state.multiple_files_dirty {
                            clean_project_buckets.remove(&project_dir_path);
                        }
                    }
                }
            }
        };

        mark_files_dirty(self, change.created.keys().iter().collect());
        mark_files_dirty(self, change.deleted.keys().iter().collect());
        mark_files_dirty(self, change.changed.keys().iter().collect());
    }

    // registry.go:781
    fn update_indexes(&mut self, ctx: &Context, change: &RegistryChange, logger: &LogTree) {
        let (project_id, _) = self.host.get_default_project(&change.requested_file);
        let Some(project_id) = project_id else {
            return;
        };

        // Compute resolved package names and project reference output mappings for all projects upfront.
        // Resolved package names are needed to compute node_modules dependencies so packages that are
        // directly imported by programs are included even if not listed in package.json.
        // Project reference output mappings are needed to redirect extraction from output .d.ts files
        // to source files for packages that are project references.
        // We need all projects because a node_modules directory can be used by multiple projects.
        let mut all_resolved_package_names: FxHashMap<ProjectID, Option<Set<String>>> =
            FxHashMap::default();
        let mut project_reference_outputs: FxHashMap<Path, String> = FxHashMap::default();
        // Compute which packages have implicit deep imports (subpath imports in packages
        // without exports). These packages need recursive directory search to discover
        // all auto-importable files, even when the preference is disabled.
        let mut all_deep_import_packages: Set<String> = Set::new();
        let host = self.host;
        self.projects.range(|entry| {
            let program = host.get_program_for_project(&entry.key());
            if let Some(program) = program {
                all_resolved_package_names
                    .insert(entry.key(), Some(get_resolved_package_names(ctx, program)));
                add_project_reference_output_mappings(program, &mut project_reference_outputs);
                // pure set inserts; Go ranges the set too
                all_deep_import_packages.m.extend(
                    program.deep_import_package_names().keys().iter().map(std::clone::Clone::clone),
                );
            }
            true
        });

        let file_exclude_patterns = self.user_preferences.parsed_auto_import_file_exclude_patterns(
            self.host.fs().use_case_sensitive_file_names(),
        );

        // Determine which packages need recursive directory search for this build.
        // nil means all packages (preference is enabled for all).
        let mut target_recursive_packages: Option<Set<String>> = None;
        if !self.user_preferences.auto_import_entrypoint_directory_search.is_true() {
            target_recursive_packages = Some(all_deep_import_packages);
        }

        // --- Collect node_modules tasks ---
        let mut node_modules_tasks: Vec<NodeModulesBucketTask> = Vec::new();
        change.requested_file.for_each_ancestor_directory(|dir_path| -> Option<()> {
            if let Some(node_modules_bucket) = self.node_modules.get(dir_path) {
                let dir_name =
                    self.directories.get(dir_path).unwrap().value().unwrap().name.clone();
                let dependencies = self.compute_dependencies_for_node_modules_directory(
                    change,
                    &all_resolved_package_names,
                    &dir_name,
                    dir_path,
                );
                let bucket_value = node_modules_bucket.value().unwrap();
                let bucket_state = &bucket_value.state;
                // !!! Optimization: handle different dependency set via granular updates
                let needs_full_rebuild = bucket_state.multiple_files_dirty
                    || !set_equals(bucket_value.dependency_names.as_ref(), dependencies.as_ref())
                    || !bucket_state.build_preferences.equal(
                        &bucket_build_preferences_from_user_preferences(&self.user_preferences),
                    )
                    || !recursive_search_subset(
                        target_recursive_packages.as_ref(),
                        bucket_state.recursive_search_packages.as_ref(),
                    );
                let dirty_packages = bucket_state.dirty_packages();
                let can_do_granular_update =
                    !needs_full_rebuild && set_len(dirty_packages.as_ref()) > 0;

                if needs_full_rebuild {
                    node_modules_tasks.push(NodeModulesBucketTask {
                        entry: node_modules_bucket,
                        dependency_names: dependencies,
                        dir_name,
                        dir_path: dir_path.clone(),
                        is_update: false,
                        existing_bucket: None,
                        dirty_packages: None,
                        package_names: None,
                        directory_package_names: None,
                        discovered: Vec::new(),
                    });
                } else if can_do_granular_update {
                    node_modules_tasks.push(NodeModulesBucketTask {
                        entry: node_modules_bucket,
                        dependency_names: dependencies,
                        dir_name,
                        dir_path: dir_path.clone(),
                        is_update: true,
                        existing_bucket: Some(bucket_value.clone()),
                        dirty_packages,
                        package_names: None,
                        directory_package_names: None,
                        discovered: Vec::new(),
                    });
                }
            }
            None
        });

        let mut node_modules_logger = LogTree::nil();
        if !logger.is_nil() && !node_modules_tasks.is_empty() {
            node_modules_logger = logger.fork("Building node_modules indexes");
        }

        // --- Phase 1: Discovery (parallel per bucket in Go) ---
        // Resolve package.json and realpath for each package in each bucket.
        let discovery_start = Instant::now();
        for task in &mut node_modules_tasks {
            if task.is_update {
                task.package_names = task.dirty_packages.clone();
            } else {
                task.directory_package_names = Some(get_package_names_in_node_modules(
                    &tspath::combine_paths(&task.dir_name, &["node_modules"]),
                    self.host.fs(),
                ));
                // Go's `core.Coalesce` of two set pointers.
                task.package_names =
                    task.dependency_names.clone().or_else(|| task.directory_package_names.clone());
            }
            task.discovered = self.discover_bucket_packages(
                task.package_names.as_ref(),
                &task.dir_name,
                &task.dir_path,
            );
        }
        if !node_modules_logger.is_nil() {
            node_modules_logger
                .logf(format_args!("Discovered packages: {:?}", discovery_start.elapsed()));
        }

        // --- Phase 2: Extraction (parallel per unique realpath in Go) ---
        // Extract from main packages first. If a main package has no TypeScript entrypoints,
        // we fall back to extracting from @types in a second pass. Packages with no main
        // package extract directly from @types in the primary pass.
        let extraction_start = Instant::now();
        let mut seen: FxHashMap<String, bool> = FxHashMap::default();
        let mut extraction_cache: FxHashMap<String, Arc<PerPackageExtractionResult>> =
            FxHashMap::default();
        // Collect all packages that have an @types fallback. After the primary pass, we
        // filter to only those whose main extraction failed, then deduplicate by typesRealpath.
        let mut types_fallback_candidates: Vec<Arc<DiscoveredPackage>> = Vec::new();
        for task in &node_modules_tasks {
            for pkg in &task.discovered {
                if !pkg.realpath.is_empty() {
                    if !seen.get(&pkg.realpath).copied().unwrap_or(false) {
                        seen.insert(pkg.realpath.clone(), true);
                        let enable_dir_search = target_recursive_packages.is_none()
                            || set_has(target_recursive_packages.as_ref(), &pkg.package_name)
                            || known_recursive_search_packages().has(&pkg.package_name);
                        // Record actual directory-searched packages so the stored set
                        // reflects reality for rebuild detection and stats.
                        if enable_dir_search && let Some(t) = &mut target_recursive_packages {
                            t.add(pkg.package_name.clone());
                        }
                        if ctx.err().is_none() {
                            let result = self.extract_package(
                                ctx,
                                pkg.package_json,
                                &pkg.package_name,
                                &project_reference_outputs,
                                file_exclude_patterns.as_ref(),
                                enable_dir_search,
                            );
                            if let Some(result) = result {
                                extraction_cache.insert(pkg.realpath.clone(), Arc::new(result));
                            }
                        }
                    }
                    if !pkg.types_realpath.is_empty() {
                        types_fallback_candidates.push(Arc::clone(pkg));
                    }
                } else if !pkg.types_realpath.is_empty()
                    && !seen.get(&pkg.types_realpath).copied().unwrap_or(false)
                {
                    seen.insert(pkg.types_realpath.clone(), true);
                    // @types packages always get directory search
                    if let Some(t) = &mut target_recursive_packages {
                        t.add(pkg.package_name.clone());
                    }
                    if ctx.err().is_none() {
                        let result = self.extract_package(
                            ctx,
                            pkg.types_package_json,
                            &pkg.package_name,
                            &project_reference_outputs,
                            file_exclude_patterns.as_ref(),
                            true, /*enableDirectorySearch*/
                        );
                        if let Some(result) = result {
                            extraction_cache.insert(pkg.types_realpath.clone(), Arc::new(result));
                        }
                    }
                }
            }
        }

        // For packages whose main extraction yielded nothing, fall back to @types.
        for pkg in &types_fallback_candidates {
            let main_extracted = extraction_cache.contains_key(&pkg.realpath);
            if main_extracted || seen.get(&pkg.types_realpath).copied().unwrap_or(false) {
                continue;
            }
            seen.insert(pkg.types_realpath.clone(), true);
            // @types fallback packages always get directory search
            if let Some(t) = &mut target_recursive_packages {
                t.add(pkg.package_name.clone());
            }
            if ctx.err().is_none() {
                let result = self.extract_package(
                    ctx,
                    pkg.types_package_json,
                    &pkg.package_name,
                    &project_reference_outputs,
                    file_exclude_patterns.as_ref(),
                    true, /*enableDirectorySearch*/
                );
                if let Some(result) = result {
                    extraction_cache.insert(pkg.types_realpath.clone(), Arc::new(result));
                }
            }
        }
        if !node_modules_logger.is_nil() {
            node_modules_logger.logf(format_args!(
                "Extracted exports: {:?} ({} packages)",
                extraction_start.elapsed(),
                seen.len()
            ));
        }
        self.unique_package_count = seen.len();

        // --- Phase 3: Bucket building (parallel per bucket in Go) ---
        // Each bucket installs the shared extraction results and builds its index.
        let mut all_results: Vec<BucketBuildResult> = Vec::new();

        for task in &node_modules_tasks {
            let mut br = BucketBuildResult::new(
                BucketTarget::NodeModules(Arc::clone(&task.entry)),
                task.entry.key(),
            );
            if task.is_update {
                self.update_node_modules_bucket(
                    ctx,
                    &mut br,
                    task.existing_bucket.as_ref().unwrap(),
                    task.dirty_packages.as_ref().unwrap(),
                    &task.discovered,
                    &extraction_cache,
                    target_recursive_packages.as_ref(),
                    &node_modules_logger.fork(&task.dir_name),
                );
            } else {
                self.build_node_modules_bucket(
                    ctx,
                    &mut br,
                    task.dependency_names.clone(),
                    &task.dir_path,
                    &task.discovered,
                    task.directory_package_names.as_ref(),
                    &extraction_cache,
                    target_recursive_packages.as_ref(),
                    &node_modules_logger.fork(&task.dir_name),
                );
            }
            all_results.push(br);
        }

        // Project bucket (not part of the three-phase pipeline — no cross-bucket dedup needed).
        if let Some(project) = self.projects.get(&project_id) {
            let program = self.host.get_program_for_project(&project_id).unwrap();
            let resolved_package_names =
                all_resolved_package_names.get(&project_id).cloned().flatten();
            let project_value = project.value().unwrap();
            let mut should_rebuild =
                project_value.state.has_dirty_file_besides(&change.requested_file)
                    || !project_value.state.build_preferences.equal(
                        &bucket_build_preferences_from_user_preferences(&self.user_preferences),
                    );
            if !should_rebuild
                && project_value.state.new_program_structure > NewProgramStructure::False
            {
                if !set_equals(
                    project_value.resolved_package_names.as_ref(),
                    resolved_package_names.as_ref(),
                ) || has_new_non_node_modules_files(program, &project_value)
                {
                    should_rebuild = true;
                } else {
                    project.change(|b| b.state.new_program_structure = NewProgramStructure::False);
                }
            }
            if should_rebuild {
                let mut br = BucketBuildResult::new(
                    BucketTarget::Project(project),
                    (self.base.to_path)(program.get_current_directory()),
                );
                self.build_project_bucket(
                    ctx,
                    &mut br,
                    &project_id,
                    resolved_package_names,
                    &logger.fork(&format!("Building project bucket {project_id}")),
                );
                all_results.push(br);
            }
        }

        for br in &all_results {
            if br.err.is_some() {
                continue;
            }
            for path in &br.removed_entrypoint_paths {
                self.entrypoints.delete(path.clone());
            }
            #[expect(
                clippy::iter_over_hash_type,
                reason = "distinct keys are set into entrypoints; Go ranges the map too"
            )]
            for (path, entries) in &br.entrypoints {
                self.entrypoints.set(path.clone(), entries.clone());
            }
            br.replace_bucket();
        }

        // If we failed to resolve any alias exports by ending up at a non-relative module specifier
        // that didn't resolve to another package, it's probably an ambient module declared in another package.
        // We recorded these failures, along with the name of every ambient module declared elsewhere, so we
        // can do a second pass on the failed files, this time including the ambient modules declarations that
        // were missing the first time. Example: node_modules/fs-extra/index.d.ts is simply `export * from "fs"`,
        // but when trying to resolve the `export *`, we don't know where "fs" is declared. The aliasResolver
        // tries to find packages named "fs" on the file system, but after failing, records "fs" as a failure
        // for fs-extra/index.d.ts. Meanwhile, if we also processed node_modules/@types/node/fs.d.ts, we
        // recorded that file as declaring the ambient module "fs". In the second pass, we combine those two
        // files and reprocess fs-extra/index.d.ts, this time finding "fs" declared in @types/node.
        let second_pass_start = Instant::now();
        let mut second_pass_file_count = 0;
        for br in &mut all_results {
            if br.err.is_some() {
                continue;
            }
            let Some(targets) = &br.possible_failed_ambient_module_lookup_targets else {
                continue;
            };
            let mut root_files: FxHashMap<String, Option<P<SourceFile>>> = FxHashMap::default();
            let mut root_file_order: Vec<String> = Vec::new();
            let targets: Vec<String> = targets.keys().iter().cloned().collect();
            for target in targets {
                for file_name in self.resolve_ambient_module_name(&target, &br.resolution_path) {
                    if root_files.contains_key(&file_name) {
                        continue;
                    }
                    root_files.insert(
                        file_name.clone(),
                        self.host.get_source_file(&file_name, &(self.base.to_path)(&file_name)),
                    );
                    root_file_order.push(file_name);
                    second_pass_file_count += 1;
                }
            }
            if !root_files.is_empty() {
                let module_resolver: &'static DefaultResolver =
                    tsrs_core::alloc(module::new_resolver(ResolverOptions::new(
                        self.host,
                        tsrs_core::empty_compiler_options(),
                    )));
                // Go collects `maps.Values(rootFiles)` (random order, nil files included; the checker skips nothing).
                let files: Vec<P<SourceFile>> =
                    root_file_order.iter().filter_map(|f| root_files[f]).collect();
                let alias_resolver = new_alias_resolver(
                    files,
                    FxHashMap::default(),
                    self.host,
                    module_resolver,
                    Arc::clone(&self.base.to_path),
                    Box::new(|_, _| {
                        // no-op
                    }),
                );
                let alias_resolver: &'static super::aliasresolver::AliasResolver =
                    tsrs_core::alloc(alias_resolver);
                let mut ch = tsrs_checker::new_checker(alias_resolver);
                let sources = br.possible_failed_ambient_module_lookup_sources.to_map();
                let bucket = br.bucket.as_mut().unwrap();
                let mut index = bucket.index.as_ref().map(|i| (**i).clone()).unwrap_or_default();
                #[expect(
                    clippy::iter_over_hash_type,
                    reason = "order-independent: only fills the export index, whose searches are sorted by fix preference (View.GetCompletions, sortFixInfo); Go ranges the map too"
                )]
                for source in sources.values() {
                    let (file_name, package_name) = {
                        let s = source.lock().unwrap();
                        (s.file_name.clone(), s.package_name.clone())
                    };
                    let source_file =
                        alias_resolver.get_source_file(&file_name).expect("nil source file");
                    let fs = self.host.fs();
                    let realpath: PathFunc = Arc::new(move |s: &str| fs.realpath(s));
                    let mut extractor = new_export_extractor(
                        &package_name,
                        &mut ch,
                        module_resolver,
                        Arc::clone(&self.base.to_path),
                        Some(realpath),
                    );
                    let file_exports = extractor.extract_from_file(source_file);
                    for exp in file_exports {
                        index.insert_as_words(exp);
                    }
                }
                bucket.index = Some(Arc::new(index));
                // The bucket was already installed above (Go mutates the installed bucket's index in place).
                br.replace_bucket();
            }
        }

        if !node_modules_logger.is_nil() {
            if second_pass_file_count > 0 {
                node_modules_logger.logf(format_args!(
                    "{} files required second pass, took {:?}",
                    second_pass_file_count,
                    second_pass_start.elapsed()
                ));
            }
            node_modules_logger.logf(format_args!("Total: {:?}", discovery_start.elapsed()));
        }
    }
}

// registry.go:1131
fn has_new_non_node_modules_files(program: &'static Program, bucket: &RegistryBucket) -> bool {
    if bucket.state.new_program_structure != NewProgramStructure::DifferentFileNames {
        return false;
    }
    if program.get_source_files().iter().any(|&file| {
        if file.is_content_mapper_supplemental()
            || file.file_name().contains("/node_modules/")
            || is_ignored_file(program, file)
        {
            return false;
        }
        !bucket.paths.contains_key(file.path())
    }) {
        return true;
    }
    false
}

// registry.go:1146
fn is_ignored_file(program: &'static Program, file: P<SourceFile>) -> bool {
    program.is_source_file_default_library(file.path())
        || program.is_global_typings_file(file.file_name())
}

// registry.go:1153
// hasSymlinkToNodeModules checks if a file's realpath has a symlink that points
// to a node_modules directory. This is used to skip files in the project bucket
// that would be duplicated by the node_modules bucket via their symlink.
fn has_symlink_to_node_modules(
    file_path: &Path,
    project_root_path: &Path,
    symlink_cache: Option<P<KnownSymlinks>>,
) -> bool {
    let Some(symlink_cache) = symlink_cache else {
        return false;
    };
    // Keep files inside this project indexed in project buckets even if they are
    // reachable through a node_modules symlink from elsewhere.
    if project_root_path.contains_path(file_path) {
        return false;
    }

    // First check if the file itself has a symlink to node_modules
    if let Some(symlink_paths) = symlink_cache.files_by_realpath().load(file_path) {
        let mut found = false;
        symlink_paths.range(|symlink_path: &String| {
            if symlink_path.contains("/node_modules/") {
                found = true;
                return false; // stop ranging
            }
            true
        });
        if found {
            return true;
        }
    }

    // Fall back to checking ancestor directories
    let directories_by_realpath = symlink_cache.directories_by_realpath();
    let mut found = false;
    file_path.for_each_ancestor_directory(|dir_path| -> Option<()> {
        let symlink_paths =
            directories_by_realpath.load(&dir_path.ensure_trailing_directory_separator())?;
        // Check if any of the symlinks point to a node_modules directory
        symlink_paths.range(|symlink_path: &String| {
            if symlink_path.contains("/node_modules/") {
                found = true;
                return false; // stop ranging
            }
            true
        });
        if found {
            Some(()) // stop if we found a match
        } else {
            None
        }
    });
    found
}

// registry.go:1204
pub(crate) struct FailedAmbientModuleLookupSource {
    file_name: String,
    package_name: String,
}

// Go's `replaceBucket func(*RegistryBucket)` (the dirty map entry's Replace).
enum BucketTarget {
    NodeModules(Arc<dirty::MapEntry<Path, RegistryBucket>>),
    Project(Arc<dirty::MapEntry<ProjectID, RegistryBucket>>),
}

// registry.go:1210
struct BucketBuildResult {
    replace_bucket: BucketTarget,
    resolution_path: Path,
    err: Option<String>,

    bucket: Option<RegistryBucket>,
    // entrypoints are the resolved entrypoints from this bucket's packages,
    // to be merged into the registry-level entrypoints map.
    entrypoints: FxHashMap<Path, Vec<Arc<ResolvedEntrypoint>>>,
    // removedEntrypointPaths lists paths whose entrypoints should be removed from
    // the registry-level map before merging new entrypoints. Used for granular updates.
    removed_entrypoint_paths: Vec<Path>,
    // File path to filename and package name
    possible_failed_ambient_module_lookup_sources:
        Arc<SyncMap<Path, Arc<Mutex<FailedAmbientModuleLookupSource>>>>,
    // Likely ambient module name
    possible_failed_ambient_module_lookup_targets: Option<Set<String>>,
}

impl BucketBuildResult {
    fn new(replace_bucket: BucketTarget, resolution_path: Path) -> BucketBuildResult {
        BucketBuildResult {
            replace_bucket,
            resolution_path,
            err: None,
            bucket: None,
            entrypoints: FxHashMap::default(),
            removed_entrypoint_paths: Vec::new(),
            possible_failed_ambient_module_lookup_sources: Arc::new(SyncMap::default()),
            possible_failed_ambient_module_lookup_targets: None,
        }
    }

    fn replace_bucket(&self) {
        let bucket = Shared::new(self.bucket.clone().unwrap_or_default());
        match &self.replace_bucket {
            BucketTarget::NodeModules(e) => e.replace(bucket),
            BucketTarget::Project(e) => e.replace(bucket),
        }
    }
}

impl RegistryBuilder<'_> {
    // registry.go:1229
    fn build_project_bucket(
        &self,
        ctx: &Context,
        result: &mut BucketBuildResult,
        project_id: &ProjectID,
        resolved_package_names: Option<Set<String>>,
        logger: &LogTree,
    ) {
        if let Some(err) = ctx.err() {
            result.err = Some(err.to_string());
            return;
        }

        let start = Instant::now();
        let file_exclude_patterns = self.user_preferences.parsed_auto_import_file_exclude_patterns(
            self.host.fs().use_case_sensitive_file_names(),
        );
        result.bucket = Some(RegistryBucket::default());
        let module_resolver: &'static DefaultResolver = tsrs_core::alloc(module::new_resolver(
            ResolverOptions::new(self.host, tsrs_core::empty_compiler_options()),
        ));
        let program = self.host.get_program_for_project(project_id).unwrap();
        let project_root_path = (self.base.to_path)(program.get_current_directory());
        let symlink_cache = Some(program.get_symlink_cache());
        let mut pool = create_checker_pool(program);
        // Go map (random iteration order).
        let mut exports: FxHashMap<Path, Vec<Arc<Export>>> = FxHashMap::default();
        let mut skipped_file_count = 0;
        let mut combined_exports = 0;
        let mut combined_used_checker = 0;

        for &file in program.get_source_files() {
            if file.is_content_mapper_supplemental() || is_ignored_file(program, file) {
                continue;
            }
            if file_exclude_patterns.as_ref().is_some_and(|p| p.match_string(file.file_name())) {
                skipped_file_count += 1;
                continue;
            }
            // Ordinary node_modules files are owned by node_modules buckets. Content-mapped files are not
            // discovered by those buckets, but files already transformed in the Program can be indexed here.
            if file.content_mapper().is_empty()
                && (file.file_name().contains("/node_modules/")
                    || has_symlink_to_node_modules(file.path(), &project_root_path, symlink_cache))
            {
                continue;
            }
            if ctx.err().is_none() {
                let checker = pool.get_checker();
                let mut extractor = new_export_extractor(
                    "",
                    checker,
                    module_resolver,
                    Arc::clone(&self.base.to_path),
                    None,
                );
                let file_exports = extractor.extract_from_file(file);
                exports.insert(file.path().clone(), file_exports);
                let stats = extractor.stats();
                combined_exports += stats.exports.load(std::sync::atomic::Ordering::Relaxed);
                combined_used_checker +=
                    stats.used_checker.load(std::sync::atomic::Ordering::Relaxed);
            }
        }

        let index_start = Instant::now();
        let mut idx: Index<Arc<Export>> = Index::default();
        let mut paths: FxHashMap<Path, String> =
            FxHashMap::with_capacity_and_hasher(exports.len(), rustc_hash::FxBuildHasher);
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: fills the paths map and the export index, whose searches are sorted by fix preference; Go ranges the map too"
        )]
        for (path, file_exports) in exports {
            paths.insert(path, String::new()); // Empty string for project buckets
            for exp in file_exports {
                idx.insert_as_words(exp);
            }
        }

        let bucket = result.bucket.as_mut().unwrap();
        bucket.paths = Arc::new(paths);
        bucket.index = Some(Arc::new(idx));
        bucket.resolved_package_names = resolved_package_names;
        bucket.state.build_preferences =
            bucket_build_preferences_from_user_preferences(&self.user_preferences);

        if !logger.is_nil() {
            logger.logf(format_args!(
                "Extracted exports: {:?} ({} exports, {} used checker, {} created checkers)",
                index_start.duration_since(start),
                combined_exports,
                combined_used_checker,
                pool.created_count()
            ));
            if skipped_file_count > 0 {
                logger.logf(format_args!(
                    "Skipped {skipped_file_count} files due to exclude patterns"
                ));
            }
            logger.logf(format_args!("Built index: {:?}", index_start.elapsed()));
            logger.logf(format_args!("Bucket total: {:?}", start.elapsed()));
        }
    }

    // registry.go:1318
    fn compute_dependencies_for_node_modules_directory(
        &self,
        change: &RegistryChange,
        all_resolved_package_names: &FxHashMap<ProjectID, Option<Set<String>>>,
        _dir_name: &str,
        dir_path: &Path,
    ) -> Option<Set<String>> {
        // If any open files are in scope of this directory but not in scope of any package.json,
        // we need to add all packages in this node_modules directory.
        // any match returns the same constant; no other effect; Go ranges the map too
        if let Some(result) = change.open_files.keys().into_iter().find_map(|path| {
            if dir_path.contains_path(path)
                && self.get_nearest_ancestor_directory_with_package_json(path).is_none()
            {
                Some(None)
            } else {
                None
            }
        }) {
            return result;
        }

        // Get all package.jsons that have this node_modules directory in their spine
        let mut dependencies: Set<String> = Set::new();
        self.directories.range(|entry| {
            let value = entry.value().unwrap();
            if value.package_json.exists() && dir_path.contains_path(&entry.key()) {
                add_package_json_dependencies(
                    &value.package_json.contents.unwrap(),
                    &mut dependencies,
                );
            }
            true
        });

        // Add packages that are directly imported by programs but not listed in package.json.
        // This ensures node_modules files are always in node_modules buckets.
        // Include packages from all projects that have this node_modules directory in their spine.
        for names in all_resolved_package_names.values().flatten() {
            dependencies.m.extend(names.keys().iter().map(std::clone::Clone::clone));
        }

        Some(dependencies)
    }
}

// registry.go:1351 (Go's dirPath field is never read)
// discoveredPackage represents a package found during the discovery phase.
// It holds the resolved package.json and realpath for deduplication.
// When both a real package and a corresponding @types package exist (e.g., react + @types/react),
// both are stored so extraction can fall back to the @types package if the real package has no
// TypeScript entrypoints.
struct DiscoveredPackage {
    package_name: String,
    package_json: P<InfoCacheEntry>,
    realpath: String,
    types_package_json: Option<P<InfoCacheEntry>>,
    types_realpath: String,
    is_local: bool, // true if realpath is within the workspace root
}

// registry.go:1364
// perPackageExtractionResult holds the extraction output for one physical package.
// Produced once per unique realpath during the extraction phase, then installed
// into every bucket that needs it during the bucket-building phase.
struct PerPackageExtractionResult {
    package_files: FxHashMap<Path, String>,
    entrypoints: Vec<Arc<ResolvedEntrypoint>>,
    exports: FxHashMap<Path, Vec<Arc<Export>>>,
    ambient_modules: FxHashMap<String, Vec<String>>,
    stats_exports: usize,
    stats_used_checker: usize,
    skipped_entrypoints: usize,
    is_symlinked: bool,
    failed_ambient_module_lookup_sources:
        FxHashMap<Path, Arc<Mutex<FailedAmbientModuleLookupSource>>>,
    failed_ambient_module_lookup_targets: Set<String>,
}

// registry.go:1378
// packageExtractionResult holds the results of extracting exports from a set of packages.
#[derive(Default)]
struct PackageExtractionResult {
    exports: FxHashMap<Path, Vec<Arc<Export>>>,
    package_files: FxHashMap<String, FxHashMap<Path, String>>,
    ambient_module_names: FxHashMap<String, Vec<String>>,
    entrypoints: Vec<Vec<Arc<ResolvedEntrypoint>>>,
    workspace_packages: Set<String>,
    possible_failed_ambient_module_lookup_sources:
        Arc<SyncMap<Path, Arc<Mutex<FailedAmbientModuleLookupSource>>>>,
    possible_failed_ambient_module_lookup_targets: Set<String>,
    stats_exports: usize,
    stats_used_checker: usize,
    skipped_entrypoints_count: usize,
}

impl RegistryBuilder<'_> {
    // registry.go:1392
    // discoverBucketPackages resolves the package.json and realpath for each package name
    // in a node_modules directory. This is the discovery phase of the three-phase extraction pipeline.
    fn discover_bucket_packages(
        &self,
        package_names: Option<&Set<String>>,
        dir_name: &str,
        _dir_path: &Path,
    ) -> Vec<Arc<DiscoveredPackage>> {
        let mut result =
            Vec::with_capacity(package_names.map_or(0, tsrs_core::collections::Set::len));
        let Some(package_names) = package_names else {
            return result;
        };
        // Go ranges packageNames.Keys() too, so the discovered packages come in map order there as well
        result.extend(package_names.keys().iter().map(|package_name| {
            let types_package_name = module::get_types_package_name(package_name);
            let package_json = self.host.get_package_json(&tspath::combine_paths(
                dir_name,
                &["node_modules", package_name, "package.json"],
            ));
            let mut types_package_json: Option<P<InfoCacheEntry>> = None;
            if *package_name != types_package_name {
                let types_json = self.host.get_package_json(&tspath::combine_paths(
                    dir_name,
                    &["node_modules", &types_package_name, "package.json"],
                ));
                if types_json.directory_exists {
                    types_package_json = Some(types_json);
                }
            }
            let mut realpath = String::new();
            if package_json.directory_exists {
                realpath = self.host.fs().realpath(package_json.package_directory);
            }
            let mut types_realpath = String::new();
            if let Some(t) = types_package_json {
                types_realpath = self.host.fs().realpath(t.package_directory);
            }
            let is_local = !realpath.is_empty()
                && !realpath.contains("/node_modules/")
                && tspath::contains_path(
                    self.host.get_current_directory(),
                    &realpath,
                    &ComparePathsOptions {
                        use_case_sensitive_file_names: self
                            .host
                            .fs()
                            .use_case_sensitive_file_names(),
                        ..Default::default()
                    },
                );
            Arc::new(DiscoveredPackage {
                package_name: package_name.clone(),
                package_json,
                realpath,
                types_package_json,
                types_realpath,
                is_local,
            })
        }));
        result
    }

    // registry.go:1437
    // extractPackage extracts exports from a single package.json.
    // This runs once per unique realpath during the extraction phase.
    // Returns nil if the package has no extractable entrypoints.
    fn extract_package(
        &self,
        ctx: &Context,
        package_json: impl Into<Option<P<InfoCacheEntry>>>,
        package_name: &str,
        project_reference_outputs: &FxHashMap<Path, String>,
        file_exclude_patterns: Option<&SpecMatcher>,
        enable_directory_search: bool,
    ) -> Option<PerPackageExtractionResult> {
        let package_json = package_json.into()?;
        if !package_json.directory_exists {
            return None;
        }
        let (to_realpath, to_symlink) =
            get_package_realpath_funcs(self.host.fs(), package_json.package_directory);
        let resolver: &'static DefaultResolver =
            tsrs_core::alloc(get_module_resolver(self.host, Arc::clone(&to_realpath)));
        let mut package_entrypoints = resolver.get_entrypoints_from_package_json_info(
            package_json,
            package_name,
            enable_directory_search,
        )?;

        let mut skipped_entrypoints = 0;
        if let Some(file_exclude_patterns) = file_exclude_patterns {
            let count = package_entrypoints.len();
            package_entrypoints.retain(|entrypoint| {
                !file_exclude_patterns.match_string(&entrypoint.resolved_file_name)
            });
            skipped_entrypoints = count - package_entrypoints.len();
        }
        if package_entrypoints.is_empty() {
            return None;
        }

        let package_entrypoints: Vec<Arc<ResolvedEntrypoint>> =
            package_entrypoints.into_iter().map(Arc::new).collect();
        let failed_targets: Arc<Mutex<Set<String>>> = Arc::new(Mutex::new(Set::new()));
        let failed_sources: Arc<
            Mutex<FxHashMap<Path, Arc<Mutex<FailedAmbientModuleLookupSource>>>>,
        > = Arc::new(Mutex::new(FxHashMap::default()));
        let mut result = PerPackageExtractionResult {
            package_files: FxHashMap::default(),
            entrypoints: package_entrypoints.clone(),
            exports: FxHashMap::default(),
            ambient_modules: FxHashMap::default(),
            stats_exports: 0,
            stats_used_checker: 0,
            skipped_entrypoints,
            is_symlinked: false,
            failed_ambient_module_lookup_sources: FxHashMap::default(),
            failed_ambient_module_lookup_targets: Set::new(),
        };

        // Resolve entrypoint source files and build the alias resolver.
        let mut seen_files: Set<Path> =
            tsrs_core::collections::new_set_with_size_hint(package_entrypoints.len());
        let mut root_files: Vec<Option<P<SourceFile>>> =
            Vec::with_capacity(package_entrypoints.len());
        let mut symlinks: FxHashMap<Path, PathAndFileName> = FxHashMap::default();
        for entrypoint in &package_entrypoints {
            let mut file_name = entrypoint.symlink_or_realpath().to_string();
            let mut realpath_file_name = entrypoint.resolved_file_name.clone();
            let mut realpath_path = (self.base.to_path)(&realpath_file_name);

            if let Some(input_file_name) = project_reference_outputs.get(&realpath_path) {
                file_name = to_symlink(input_file_name);
                realpath_file_name.clone_from(input_file_name);
                realpath_path = (self.base.to_path)(&realpath_file_name);
            }

            if !seen_files.add_if_absent(realpath_path.clone()) {
                continue;
            }
            if file_name != realpath_file_name {
                let symlink_path = (self.base.to_path)(&file_name);
                symlinks.insert(
                    realpath_path.clone(),
                    PathAndFileName { path: symlink_path, file_name: file_name.clone() },
                );
                result.is_symlinked = true;
            }
            let file = self.host.get_source_file(&realpath_file_name, &realpath_path);
            if let Some(file) = file {
                tsrs_binder::bind_source_file(file);
            }
            root_files.push(file);
        }
        let root_files: Vec<P<SourceFile>> = root_files.into_iter().flatten().collect();

        let on_failed_targets = Arc::clone(&failed_targets);
        let on_failed_sources = Arc::clone(&failed_sources);
        let alias_resolver = new_alias_resolver(
            root_files,
            symlinks,
            self.host,
            resolver,
            Arc::clone(&self.base.to_path),
            Box::new(move |source: P<SourceFile>, module_name: &str| {
                on_failed_targets.lock().unwrap().add(module_name.to_string());
                on_failed_sources.lock().unwrap().entry(source.path().clone()).or_insert_with(
                    || {
                        Arc::new(Mutex::new(FailedAmbientModuleLookupSource {
                            file_name: source.file_name().to_string(),
                            package_name: String::new(),
                        }))
                    },
                );
            }),
        );
        let alias_resolver: &'static super::aliasresolver::AliasResolver =
            tsrs_core::alloc(alias_resolver);

        let mut ch = tsrs_checker::new_checker(alias_resolver);
        let mut extractor = new_export_extractor(
            package_name,
            &mut ch,
            resolver,
            Arc::clone(&self.base.to_path),
            Some(to_realpath),
        );

        let mut non_module_files: Set<Path> = Set::new();
        for &entrypoint in &alias_resolver.root_files {
            if ctx.err().is_some() {
                return None;
            }
            let file_exports = extractor.extract_from_file(entrypoint);
            for &name in entrypoint.ambient_module_names() {
                result
                    .ambient_modules
                    .entry(name.to_string())
                    .or_default()
                    .push(entrypoint.file_name().to_string());
            }
            result
                .package_files
                .insert(entrypoint.path().clone(), entrypoint.file_name().to_string());
            let symlink = alias_resolver.symlinks.get(entrypoint.path());
            if let Some(symlink) = symlink {
                result.package_files.insert(symlink.path.clone(), symlink.file_name.clone());
            }

            let mut has_exports =
                !file_exports.is_empty() && entrypoint.external_module_indicator().is_some();
            let source = failed_sources.lock().unwrap().get(entrypoint.path()).cloned();
            match source {
                None => {
                    result.exports.insert(entrypoint.path().clone(), file_exports);
                }
                Some(source) => {
                    source.lock().unwrap().package_name = package_name.to_string();
                    has_exports = entrypoint.external_module_indicator().is_some();
                }
            }

            if !has_exports {
                non_module_files.add(entrypoint.path().clone());
                if let Some(symlink) = symlink {
                    non_module_files.add(symlink.path.clone());
                }
            }
        }

        // Discard entrypoints for non-module files and empty modules.
        let to_path = Arc::clone(&self.base.to_path);
        result.entrypoints.retain(|ep| !non_module_files.has(&to_path(&ep.resolved_file_name)));

        let stats = extractor.stats();
        result.stats_exports = stats.exports.load(std::sync::atomic::Ordering::Relaxed);
        result.stats_used_checker = stats.used_checker.load(std::sync::atomic::Ordering::Relaxed);
        result.failed_ambient_module_lookup_targets =
            std::mem::take(&mut *failed_targets.lock().unwrap());
        result.failed_ambient_module_lookup_sources =
            std::mem::take(&mut *failed_sources.lock().unwrap());
        Some(result)
    }
}

// registry.go:1575
// installExtractions aggregates pre-extracted per-package results into a single
// packageExtractionResult for one bucket. This is the install phase of the three-phase pipeline.
fn install_extractions(
    discovered: &[Arc<DiscoveredPackage>],
    extraction_cache: &FxHashMap<String, Arc<PerPackageExtractionResult>>,
) -> PackageExtractionResult {
    let mut result = PackageExtractionResult::default();

    for pkg in discovered {
        let mut extraction = extraction_cache.get(&pkg.realpath);
        if extraction.is_none() {
            extraction = extraction_cache.get(&pkg.types_realpath);
        }
        let Some(extraction) = extraction else {
            continue;
        };
        // copies distinct keys into a map (maps.Copy in Go)
        result.exports.extend(extraction.exports.iter().map(|(k, v)| (k.clone(), v.clone())));
        let package_files = result.package_files.entry(pkg.package_name.clone()).or_default();
        // copies distinct keys into a map (maps.Copy in Go)
        package_files.extend(extraction.package_files.iter().map(|(k, v)| (k.clone(), v.clone())));
        #[expect(
            clippy::iter_over_hash_type,
            reason = "names are distinct, so each per-key append is independent of the others; Go ranges the map too"
        )]
        for (name, file_names) in &extraction.ambient_modules {
            result
                .ambient_module_names
                .entry(name.clone())
                .or_default()
                .extend(file_names.iter().cloned());
        }
        result.entrypoints.push(extraction.entrypoints.clone());
        #[expect(
            clippy::iter_over_hash_type,
            reason = "stores distinct keys; Go ranges the map too"
        )]
        for (path, source) in &extraction.failed_ambient_module_lookup_sources {
            result
                .possible_failed_ambient_module_lookup_sources
                .load_or_store(path.clone(), Arc::clone(source));
        }
        // pure set inserts; Go ranges the set too
        result.possible_failed_ambient_module_lookup_targets.m.extend(
            extraction
                .failed_ambient_module_lookup_targets
                .keys()
                .iter()
                .map(std::clone::Clone::clone),
        );
        if extraction.is_symlinked && pkg.is_local {
            result.workspace_packages.add(pkg.package_name.clone());
        }
        result.stats_exports += extraction.stats_exports;
        result.stats_used_checker += extraction.stats_used_checker;
        result.skipped_entrypoints_count += extraction.skipped_entrypoints;
    }

    result
}

impl RegistryBuilder<'_> {
    // registry.go:1627
    fn build_node_modules_bucket(
        &self,
        ctx: &Context,
        result: &mut BucketBuildResult,
        dependencies: Option<Set<String>>,
        dir_path: &Path,
        discovered: &[Arc<DiscoveredPackage>],
        directory_package_names: Option<&Set<String>>,
        extraction_cache: &FxHashMap<String, Arc<PerPackageExtractionResult>>,
        recursive_search_packages: Option<&Set<String>>,
        logger: &LogTree,
    ) {
        if let Some(err) = ctx.err() {
            result.err = Some(err.to_string());
            return;
        }

        let extraction = install_extractions(discovered, extraction_cache);

        let index_start = Instant::now();
        // Build PackageFiles with all directory package names; indexed packages have
        // non-nil maps, unindexed packages have nil maps.
        let mut all_package_files: PackageFiles = PackageFiles::default();
        if let Some(directory_package_names) = directory_package_names {
            // pure inserts into all_package_files; Go ranges the set too
            all_package_files.extend(directory_package_names.keys().iter().map(|pkg_name| {
                (
                    pkg_name.clone(),
                    extraction.package_files.get(pkg_name).map(|m| Arc::new(m.clone())),
                )
            }));
        }

        // Build Paths as reverse mapping from path to package name.
        // Only include paths for local workspace packages (eligible for granular updates).
        let mut paths: FxHashMap<Path, String> = FxHashMap::default();
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: fills the paths map from each workspace package's files; Go ranges the set too"
        )]
        for pkg_name in extraction.workspace_packages.keys() {
            if let Some(files) = extraction.package_files.get(pkg_name) {
                // distinct keys inserted with the same value; Go ranges the map too
                paths.extend(files.keys().map(|path| (path.clone(), pkg_name.clone())));
            }
        }

        let mut index: Index<Arc<Export>> = Index::default();
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: only fills the export index, whose searches are sorted by fix preference; Go ranges the map too"
        )]
        for file_exports in extraction.exports.values() {
            for exp in file_exports {
                index.insert_as_words(Arc::clone(exp));
            }
        }
        result.bucket = Some(RegistryBucket {
            index: Some(Arc::new(index)),
            dependency_names: dependencies,
            package_files: Some(Arc::new(all_package_files)),
            ambient_module_names: Arc::new(extraction.ambient_module_names),
            paths: Arc::new(paths),
            resolved_package_names: None,
            state: BucketState {
                build_preferences: bucket_build_preferences_from_user_preferences(
                    &self.user_preferences,
                ),
                recursive_search_packages: recursive_search_packages.cloned(),
                ..Default::default()
            },
        });
        result.entrypoints = FxHashMap::with_capacity_and_hasher(
            extraction.exports.len(),
            rustc_hash::FxBuildHasher,
        );
        result.possible_failed_ambient_module_lookup_sources =
            extraction.possible_failed_ambient_module_lookup_sources;
        result.possible_failed_ambient_module_lookup_targets =
            Some(extraction.possible_failed_ambient_module_lookup_targets);
        for entrypoint_set in &extraction.entrypoints {
            for entrypoint in entrypoint_set {
                let path = (self.base.to_path)(&entrypoint.resolved_file_name);
                result.entrypoints.entry(path).or_default().push(Arc::clone(entrypoint));
            }
        }

        // Compute old entrypoint paths to remove from the registry-level map.
        // For a full rebuild, all entrypoints belonging to the old bucket's packages must be removed.
        if let Some(old_entry) = self.node_modules.get(dir_path) {
            let old_bucket = old_entry.value().unwrap();
            if let Some(package_files) = &old_bucket.package_files {
                for files in package_files.values().flatten() {
                    // the collected paths are only deleted from entrypoints; Go ranges the map too
                    result.removed_entrypoint_paths.extend(files.keys().filter_map(|path| {
                        if self.base.entrypoints.contains_key(path) {
                            Some(path.clone())
                        } else {
                            None
                        }
                    }));
                }
            }
        }

        if !logger.is_nil() {
            logger.logf(format_args!(
                "Installed {} exports ({} used checker)",
                extraction.stats_exports, extraction.stats_used_checker
            ));
            if extraction.skipped_entrypoints_count > 0 {
                logger.logf(format_args!(
                    "Skipped {} entrypoints due to exclude patterns",
                    extraction.skipped_entrypoints_count
                ));
            }
            logger.logf(format_args!("Built index: {:?}", index_start.elapsed()));
        }

        result.err = ctx.err().map(|e| e.to_string());
    }

    // registry.go:1720
    // updateNodeModulesBucket performs a granular update of the node_modules bucket,
    // re-extracting only the dirty packages and merging with the existing bucket.
    fn update_node_modules_bucket(
        &self,
        ctx: &Context,
        result: &mut BucketBuildResult,
        existing_bucket: &RegistryBucket,
        dirty_packages: &Set<String>,
        discovered: &[Arc<DiscoveredPackage>],
        extraction_cache: &FxHashMap<String, Arc<PerPackageExtractionResult>>,
        recursive_search_packages: Option<&Set<String>>,
        logger: &LogTree,
    ) {
        if let Some(err) = ctx.err() {
            result.err = Some(err.to_string());
            return;
        }

        let start = Instant::now();
        let extraction = install_extractions(discovered, extraction_cache);

        let index_start = Instant::now();

        // Clone the existing index, excluding exports from dirty packages
        let mut new_index = existing_bucket
            .index
            .as_ref()
            .map(|idx| idx.clone_filtered(|exp| !dirty_packages.has(&exp.package_name)));

        // Clone PackageFiles, removing dirty packages
        let mut new_package_files: PackageFiles =
            existing_bucket.package_files.as_ref().map(|p| (**p).clone()).unwrap_or_default();
        #[expect(
            clippy::iter_over_hash_type,
            reason = "keys are only removed; Go ranges the set too"
        )]
        for pkg_name in dirty_packages.keys() {
            new_package_files.remove(pkg_name);
        }
        // Add newly extracted package files
        // copies distinct keys into a map (maps.Copy in Go)
        new_package_files.extend(
            extraction.package_files.iter().map(|(k, v)| (k.clone(), Some(Arc::new(v.clone())))),
        );

        // Clone Paths, removing dirty package paths
        let mut new_paths: FxHashMap<Path, String> = FxHashMap::with_capacity_and_hasher(
            existing_bucket.paths.len(),
            rustc_hash::FxBuildHasher,
        );
        // filtered copy of distinct keys into a new map; Go ranges the map too
        new_paths.extend(existing_bucket.paths.iter().filter_map(|(path, pkg_name)| {
            if dirty_packages.has(pkg_name) {
                return None;
            }
            Some((path.clone(), pkg_name.clone()))
        }));
        // Add paths for newly extracted workspace packages
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: fills the paths map from each workspace package's files; Go ranges the set too"
        )]
        for pkg_name in extraction.workspace_packages.keys() {
            if let Some(files) = extraction.package_files.get(pkg_name) {
                // distinct keys inserted with the same value; Go ranges the map too
                new_paths.extend(files.keys().map(|path| (path.clone(), pkg_name.clone())));
            }
        }

        // Clone AmbientModuleNames, removing dirty package entries
        let mut new_ambient_module_names: FxHashMap<String, Vec<String>> =
            FxHashMap::with_capacity_and_hasher(
                existing_bucket.ambient_module_names.len(),
                rustc_hash::FxBuildHasher,
            );
        // filtered copy of distinct keys into a new map; Go ranges the map too
        new_ambient_module_names.extend(existing_bucket.ambient_module_names.iter().filter_map(
            |(module_name, file_names)| {
                // Filter out files from dirty packages
                let mut filtered: Vec<String> = Vec::new();
                filtered.extend(file_names.iter().filter_map(|file_name| {
                    let path = (self.base.to_path)(file_name);
                    if let Some(pkg_name) = existing_bucket.paths.get(&path)
                        && dirty_packages.has(pkg_name)
                    {
                        return None;
                    }
                    Some(file_name.clone())
                }));
                if filtered.is_empty() { None } else { Some((module_name.clone(), filtered)) }
            },
        ));
        // Add newly extracted ambient module names
        #[expect(
            clippy::iter_over_hash_type,
            reason = "names are distinct, so each per-key append is independent of the others; Go ranges the map too"
        )]
        for (module_name, file_names) in &extraction.ambient_module_names {
            new_ambient_module_names
                .entry(module_name.clone())
                .or_default()
                .extend(file_names.iter().cloned());
        }

        // Collect entrypoint paths that need to be removed from the registry-level map
        // (paths belonging to dirty packages)
        let mut removed_entrypoint_paths: Vec<Path> = Vec::new();
        // the collected paths are only deleted from entrypoints; Go ranges the map too
        removed_entrypoint_paths.extend(self.base.entrypoints.keys().filter_map(|path| {
            if let Some(pkg_name) = existing_bucket.paths.get(path) {
                if dirty_packages.has(pkg_name) { Some(path.clone()) } else { None }
            } else {
                None
            }
        }));
        // Build new entrypoints from extraction
        let mut new_entrypoints: FxHashMap<Path, Vec<Arc<ResolvedEntrypoint>>> =
            FxHashMap::default();
        for entrypoint_set in &extraction.entrypoints {
            for entrypoint in entrypoint_set {
                let path = (self.base.to_path)(&entrypoint.resolved_file_name);
                new_entrypoints.entry(path).or_default().push(Arc::clone(entrypoint));
            }
        }

        // Insert newly extracted exports into the index
        // (Go calls insertAsWords on the cloned index, which is nil when the existing bucket had none: a nil
        // pointer dereference.)
        let index = new_index.as_mut().expect("nil Index");
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: only fills the export index, whose searches are sorted by fix preference; Go ranges the map too"
        )]
        for file_exports in extraction.exports.values() {
            for exp in file_exports {
                index.insert_as_words(Arc::clone(exp));
            }
        }

        result.bucket = Some(RegistryBucket {
            index: new_index.map(Arc::new),
            dependency_names: existing_bucket.dependency_names.clone(),
            package_files: Some(Arc::new(new_package_files)),
            ambient_module_names: Arc::new(new_ambient_module_names),
            paths: Arc::new(new_paths),
            resolved_package_names: None,
            state: BucketState {
                build_preferences: bucket_build_preferences_from_user_preferences(
                    &self.user_preferences,
                ),
                recursive_search_packages: recursive_search_packages.cloned(),
                ..Default::default()
            },
        });
        result.entrypoints = new_entrypoints;
        result.removed_entrypoint_paths = removed_entrypoint_paths;
        result.possible_failed_ambient_module_lookup_sources =
            extraction.possible_failed_ambient_module_lookup_sources;
        result.possible_failed_ambient_module_lookup_targets =
            Some(extraction.possible_failed_ambient_module_lookup_targets);

        if !logger.is_nil() {
            logger.logf(format_args!(
                "Granular update of {} packages: {:?} ({} exports)",
                dirty_packages.len(),
                index_start.duration_since(start),
                extraction.stats_exports
            ));
            logger.logf(format_args!("Built index: {:?}", index_start.elapsed()));
        }

        result.err = ctx.err().map(|e| e.to_string());
    }

    // registry.go:1830
    fn get_nearest_ancestor_directory_with_package_json(
        &self,
        file_path: &Path,
    ) -> Option<Shared<Directory>> {
        file_path.get_directory_path().for_each_ancestor_directory(|dir_path| {
            if let Some(dir_entry) = self.directories.get(dir_path) {
                let value = dir_entry.value().unwrap();
                if value.package_json.exists() {
                    return Some(value);
                }
            }
            None
        })
    }

    // registry.go:1839
    fn resolve_ambient_module_name(&self, module_name: &str, from_path: &Path) -> Vec<String> {
        from_path
            .for_each_ancestor_directory(|dir_path| {
                if let Some(bucket) = self.node_modules.get(dir_path)
                    && let Some(file_names) =
                        bucket.value().unwrap().ambient_module_names.get(module_name)
                {
                    return Some(file_names.clone());
                }
                None
            })
            .unwrap_or_default()
    }
}
