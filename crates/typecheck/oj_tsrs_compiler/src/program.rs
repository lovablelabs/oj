use std::fmt::Display;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use crate::checkerpool::{Checker, CheckerHandle, CheckerPool, Context};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{
    self as ast, CommentDirectiveKind, Diagnostic, DiagnosticExt, FileReference, Kind, Node,
    SourceFile, SourceFileMetaData, compare_diagnostics, equal_diagnostics,
    equal_diagnostics_no_related_info, new_compiler_diagnostic, new_diagnostic,
};
use tsrs_core::tspath::{self, ComparePathsOptions, Path};
use tsrs_core::{
    CompilerOptions, JsxEmit, ModuleKind, ModuleResolutionKind, P, ResolutionMode, ScriptKind,
    ScriptTarget, Tristate,
};
use tsrs_diagnostics::{self as diagnostics, Message};
use tsrs_module::{
    self as module, ModeAwareCache, ResolvedModule, ResolvedTypeReferenceDirective, Resolver,
    ResolverOptions,
};
use tsrs_tsoptions::{self as tsoptions, ParsedCommandLine, outputpaths};

use crate::checkerpool::CheckerPoolImpl;
use crate::emitter::source_file_may_be_emitted;
use crate::file_include::{self, FileIncludeReason};
use crate::fileloader::str_slice;
use crate::fileloader::{
    DuplicateSourceFile, LibFile, ProcessedFiles, get_default_resolution_mode_for_file,
    get_emit_syntax_for_usage_location_worker, get_mode_for_usage_location,
    process_all_program_files,
};
use crate::host::CompilerHost;
use crate::includeprocessor::IncludeProcessor;
use crate::processing_diagnostic::{IncludeExplainingDiagnostic, ProcessingDiagnostic};
use crate::projectreferencefilemapper::ProjectReferenceFileMapper;
use tsrs_core::collections::Set;
use tsrs_module::ResolutionHost;
use tsrs_module::symlinks::{self, KnownSymlinks};
use tsrs_tsoptions::SourceOutputAndProjectReference;

pub type CreateModuleResolver =
    Arc<dyn Fn(ResolverOptions<'static>) -> Box<dyn Resolver> + Send + Sync>;

// Go `ProgramFactories.CreateCheckerPool func(*Program) CheckerPool`.
pub type CreateCheckerPool = Arc<dyn Fn(&'static Program) -> Box<dyn CheckerPool> + Send + Sync>;

// Go `ProgramOptions` (= ProgramConfig + ProgramHosts + ProgramFactories, flattened; tracing is not ported).
pub struct ProgramOptions {
    pub(crate) config: P<ParsedCommandLine>,
    pub use_source_of_project_reference: bool,
    pub single_threaded: Tristate,
    pub typings_location: String,
    pub(crate) project_name: String,
    // SkipModuleResolution avoids all module and type reference resolution while
    // still collecting import metadata needed for emit.
    pub skip_module_resolution: bool,
    pub(crate) host: Arc<dyn CompilerHost>,
    pub create_checker_pool: Option<CreateCheckerPool>,
    pub create_module_resolver: Option<CreateModuleResolver>,
    /// tsrs-only: whether the type-check pass frees the tree of each leaf file once it is checked (fileregions.rs;
    /// the CLI's `--noEmit` check, with file regions on). Nothing may read a leaf's tree after the pass.
    pub leaf_files: crate::fileregions::LeafMode,
    /// tsrs-only: whether the type-check pass may retire a checker under `--maxMemory` and go on
    /// with a fresh one (checkerpool.rs `retire_checker`; the CLI's `--noEmit` check, where no later pass needs the
    /// checker that checked a file).
    pub checker_recycling: bool,
    /// tsrs-only: use virtual four-checker reporting sites and their incremental cache proof. API builds
    /// disable this to retain per-file diagnostics and Go-shaped build info; external pools opt out too.
    pub canonical_diagnostics: bool,
    /// tsrs-only: each checker of the built-in pool allocates in a region of its own, which `free_program` gives
    /// back (watch mode, which replaces its program every cycle, and `tsc -b`, which frees each project's program once
    /// it is built; the CLI's single build leaks its checkers).
    pub checker_regions: bool,
    /// tsrs-only, with `checker_regions`: the diagnostics each checker makes go to a region of its own, which
    /// `free_program` gives back too (checkerpool.rs `SlotChecker`): the caller copies those it keeps (watch
    /// mode's snapshot, `tsrs_incremental` `Program::copy_diagnostics`).
    pub checker_diagnostic_regions: bool,
    /// tsrs-only: what the file loader allocates goes to these regions (programregions.rs), which the caller frees
    /// once nothing uses the program's processed-file and resolution data (watch mode, `tsc -b`).
    pub regions: Option<std::sync::Arc<crate::programregions::ProgramRegions>>,
    /// tsrs-only: `reuse_program` may keep the file loader's synthetic imports (JSX runtime, import helpers) of the
    /// edited file. Their parent is this program's version of the file, so the caller must keep it alive while any
    /// program reused from this one lives (watch mode keeps the full build's files). The language server frees it
    /// with this program, so it leaves this off and gets Go's full rebuild.
    pub reuse_synthetic_imports: bool,
}

impl ProgramOptions {
    pub fn new(config: P<ParsedCommandLine>, host: Arc<dyn CompilerHost>) -> ProgramOptions {
        ProgramOptions {
            config,
            use_source_of_project_reference: false,
            single_threaded: Tristate::Unknown,
            typings_location: String::new(),
            project_name: String::new(),
            skip_module_resolution: false,
            host,
            create_checker_pool: None,
            create_module_resolver: None,
            leaf_files: crate::fileregions::LeafMode::Off,
            checker_recycling: false,
            canonical_diagnostics: true,
            checker_regions: false,
            checker_diagnostic_regions: false,
            regions: None,
            reuse_synthetic_imports: false,
        }
    }

    // Go `ProgramOptions{ProgramConfig: config, Host: host, CreateCheckerPool: ..., CreateModuleResolver: ...}`.
    fn from_config(
        config: ProgramConfig,
        host: Arc<dyn CompilerHost>,
        create_checker_pool: Option<CreateCheckerPool>,
        create_module_resolver: Option<CreateModuleResolver>,
    ) -> ProgramOptions {
        ProgramOptions {
            config: config.config,
            use_source_of_project_reference: config.use_source_of_project_reference,
            single_threaded: config.single_threaded,
            typings_location: config.typings_location,
            project_name: config.project_name,
            skip_module_resolution: config.skip_module_resolution,
            host,
            create_checker_pool,
            create_module_resolver,
            leaf_files: crate::fileregions::LeafMode::Off,
            checker_recycling: false,
            canonical_diagnostics: true,
            checker_regions: false,
            checker_diagnostic_regions: false,
            regions: None,
            reuse_synthetic_imports: false,
        }
    }

    pub(crate) fn program_config(&self) -> ProgramConfig {
        ProgramConfig {
            config: self.config,
            use_source_of_project_reference: self.use_source_of_project_reference,
            single_threaded: self.single_threaded,
            typings_location: self.typings_location.clone(),
            project_name: self.project_name.clone(),
            skip_module_resolution: self.skip_module_resolution,
            regions: self.regions.clone(),
        }
    }
}

#[derive(Clone)]
pub struct ProgramConfig {
    pub(crate) config: P<ParsedCommandLine>,
    use_source_of_project_reference: bool,
    single_threaded: Tristate,
    typings_location: String,
    project_name: String,
    pub(crate) skip_module_resolution: bool,
    pub(crate) regions: Option<std::sync::Arc<crate::programregions::ProgramRegions>>,
}

impl ProgramConfig {
    // program.go:64
    pub(crate) fn can_use_project_reference_source(&self) -> bool {
        self.use_source_of_project_reference
            && !self
                .config
                .compiler_options()
                .unwrap()
                .disable_source_of_project_reference_redirect
                .is_true()
    }
}

// program.go:91
pub struct PackageNamesInfo {
    resolved: Set<String>,
    unresolved: Set<String>,
    deep_import_packages: Set<String>,
}

// Go `checkerPool CheckerPool` + `compilerCheckerPool *checkerPool`: the built-in pool is set only when
// `CreateCheckerPool` was not provided; it enables grouped parallel iteration, non-exclusive access for emit,
// and direct global diagnostics collection.
#[expect(
    clippy::large_enum_variant,
    reason = "one per program; the built-in pool, the usual variant, is reached on every checker request and stays inline"
)]
enum ProgramCheckerPool {
    Compiler(CheckerPoolImpl),
    External(Box<dyn CheckerPool>),
}

pub struct Program {
    pub(crate) opts: ProgramConfig,
    host: Arc<dyn CompilerHost>,
    resolution_host: &'static dyn ResolutionHost,
    pub resolution_data: P<module::ResolutionData>,
    // Always set once the program is constructed (see `init_checker_pool`).
    checker_pool: OnceLock<ProgramCheckerPool>,
    pub(crate) include_processor: IncludeProcessor,
    module_resolution_error: Option<String>,

    pub(crate) compare_paths_options: ComparePathsOptions,

    // Go's embedded `processedFiles`: `files` and `filesByPath` are per program (ReuseProgram replaces one file);
    // the rest is shared by the programs ReuseProgram derives from this one, as Go's shallow struct copy shares it.
    pub(crate) files: &'static [P<SourceFile>],
    pub(crate) files_by_path: FxHashMap<Path, P<SourceFile>>,
    processed: &'static ProcessedFiles,
    project_reference_file_mapper: &'static ProjectReferenceFileMapper,
    processing_diagnostics: Mutex<Vec<ProcessingDiagnostic>>,

    uses_uri_style_node_core_modules: Tristate,

    common_source_directory: OnceLock<String>,

    declaration_diagnostic_cache: Mutex<FxHashMap<P<SourceFile>, Vec<P<Diagnostic>>>>,

    program_diagnostics: Vec<P<Diagnostic>>,
    has_emit_blocking_diagnostics: FxHashSet<Path>,
    content_mapper_option_diagnostics: Vec<P<Diagnostic>>,

    // Cached unresolved imports for ATA
    unresolved_imports: OnceLock<Arc<Set<String>>>,
    known_symlinks: OnceLock<P<KnownSymlinks>>,

    // Used by auto-imports
    package_names: OnceLock<Arc<PackageNamesInfo>>,

    // Used by workspace/symbol
    has_ts_file: OnceLock<bool>,

    // Cached map of package names to whether they bundle types
    packages_map: OnceLock<FxHashMap<String, bool>>,

    // tsrs-only (tsrs_modulespecifiers::ExportsModuleNameCache)
    pub(crate) exports_module_name_cache: tsrs_modulespecifiers::ExportsModuleNameCache,

    // tsrs-only: `ProgramOptions::leaf_files`, and whether the type-check pass that may free leaves has started.
    pub(crate) leaf_files: crate::fileregions::LeafMode,
    pub(crate) leaf_pass_started: std::sync::atomic::AtomicBool,
    // tsrs-only: the files that can be leaves and that another file refers to (`fileregions::prepare`).
    pub(crate) leaf_referred: OnceLock<rustc_hash::FxHashSet<P<SourceFile>>>,
    // tsrs-only: `checkerpool::tsgo_default_check_slots`, built on first use.
    pub(crate) tsgo_check_slots: OnceLock<FxHashMap<P<SourceFile>, (u32, u32)>>,
    // tsrs-only: the import graph those slots are computed from (`checkerpool::get_import_adjacency`), which a program
    // made by `reuse_program` takes over: it has the same files at the same indices with the same imports.
    pub tsgo_import_adjacency: OnceLock<Arc<[Vec<usize>]>>,
    // tsrs-only: `ProgramOptions::checker_recycling`.
    pub(crate) checker_recycling: bool,
    canonical_diagnostics: bool,
    // tsrs-only: `ProgramOptions::checker_regions`.
    pub(crate) checker_regions: bool,
    // tsrs-only: `ProgramOptions::checker_diagnostic_regions`.
    pub(crate) checker_diagnostic_regions: bool,
    // tsrs-only: `ProgramOptions::reuse_synthetic_imports`.
    reuse_synthetic_imports: bool,
    // tsrs-only: the address of the program `reuse_program` made this one from, 0 for a program built in full
    // (`is_reuse_of`). An address, not a reference: the watch frees that program once this one has replaced it.
    reused_from: usize,
    // tsrs-only: what `reuse_program` allocated for this program when its programs are freed (`checker_regions`):
    // the file list and the resolution data with its copy of the package.json cache table, about 40 KB per edit on a
    // 4k-file project. Last: the fields above point into it.
    own_region: Option<tsrs_core::arena::Region>,
}

impl std::ops::Deref for Program {
    type Target = ProcessedFiles;
    fn deref(&self) -> &ProcessedFiles {
        self.processed
    }
}

impl Program {
    pub fn uses_canonical_diagnostics(&self) -> bool {
        self.canonical_diagnostics
            && !tsrs_core::compat::go_compatible_history()
            && self.compiler_checker_pool().is_some()
    }

    pub(crate) fn file_exists(&self, path: &str) -> bool {
        self.host.fs().file_exists(path)
    }

    pub fn get_current_directory(&self) -> &str {
        self.host.get_current_directory()
    }

    // program.go:154
    pub fn content_mapper_project(&self) -> Option<Arc<dyn tsrs_contentmapper::Project>> {
        self.host.content_mapper_project()
    }

    pub fn get_global_typings_cache_location(&self) -> &str {
        &self.opts.typings_location
    }

    pub fn get_nearest_ancestor_directory_with_package_json(&self, dirname: &str) -> String {
        match self.new_resolver().get_package_scope_for_path(dirname) {
            Some(scoped) if scoped.exists() => scoped.package_directory.to_string(),
            _ => String::new(),
        }
    }

    pub fn get_package_json_info(
        &self,
        pkg_json_path: &str,
    ) -> Option<P<tsrs_module::packagejson::InfoCacheEntry>> {
        let directory = tspath::get_directory_path(pkg_json_path);
        match self.new_resolver().get_package_scope_for_path(&directory) {
            Some(scoped) if scoped.exists() && scoped.package_directory == directory => {
                Some(scoped)
            }
            _ => None,
        }
    }

    fn new_resolver(&self) -> module::DefaultResolver<'static> {
        self.resolution_data
            .get()
            .new_resolver(self.project_reference_file_mapper.resolution_host(self.resolution_host))
    }

    // GetRedirectTargets returns the list of file paths that redirect to the given path.
    // These are files from the same package (same name@version) installed in different locations.
    pub(crate) fn get_redirect_targets(&self, path: &Path) -> &[String] {
        self.redirect_targets_map.get(path).map_or(&[] as &[String], std::vec::Vec::as_slice)
    }

    // gets the original file that was included in program
    // this returns original source file name when including output of project reference
    // otherwise same name
    pub(crate) fn get_source_of_project_reference_if_output_included(
        &self,
        file_name: &str,
        path: &Path,
    ) -> String {
        if let Some(source) = self.output_file_to_project_reference_source.get(path) {
            return source.clone();
        }
        file_name.to_string()
    }

    // program.go:209
    pub(crate) fn get_project_reference_from_source(
        &self,
        path: &Path,
    ) -> Option<P<SourceOutputAndProjectReference>> {
        self.project_reference_file_mapper.get_project_reference_from_source(path)
    }

    // program.go:214
    pub fn is_source_from_project_reference(&self, path: &Path) -> bool {
        self.project_reference_file_mapper.is_source_from_project_reference(path)
    }

    // program.go:218
    pub(crate) fn get_project_reference_from_output_dts(
        &self,
        path: &Path,
    ) -> Option<P<SourceOutputAndProjectReference>> {
        self.project_reference_file_mapper.get_project_reference_from_output_dts(path)
    }

    // program.go:226
    pub(crate) fn get_redirect_for_resolution(
        &self,
        file: P<SourceFile>,
    ) -> Option<P<ParsedCommandLine>> {
        self.project_reference_file_mapper
            .get_redirect_parsed_command_line_for_resolution(file.file_name(), file.path())
    }

    // program.go:231
    pub fn get_parse_file_redirect(&self, file_name: &str) -> String {
        self.project_reference_file_mapper
            .get_parse_file_redirect(file_name, &self.to_path(file_name))
    }

    // program.go:235
    pub fn get_resolved_project_references(&self) -> Vec<Option<P<ParsedCommandLine>>> {
        self.project_reference_file_mapper.get_resolved_project_references()
    }

    // program.go:239
    pub fn range_resolved_project_reference(
        &self,
        f: impl FnMut(&Path, Option<P<ParsedCommandLine>>, P<ParsedCommandLine>, usize) -> bool,
    ) -> bool {
        self.project_reference_file_mapper.range_resolved_project_reference(f)
    }

    // program.go:243
    pub fn range_resolved_project_reference_in_child_config(
        &self,
        child_config: Option<P<ParsedCommandLine>>,
        f: impl FnMut(&Path, Option<P<ParsedCommandLine>>, P<ParsedCommandLine>, usize) -> bool,
    ) -> bool {
        self.project_reference_file_mapper
            .range_resolved_project_reference_in_child_config(child_config, f)
    }

    // program.go:183
    pub fn package_json_cache_entries(
        &self,
        f: impl FnMut(&Path, &P<tsrs_module::packagejson::InfoCacheEntry>) -> bool,
    ) {
        self.resolution_data.package_json_cache_entries(f);
    }

    pub fn use_case_sensitive_file_names(&self) -> bool {
        self.host.fs().use_case_sensitive_file_names()
    }

    pub fn uses_uri_style_node_core_modules(&self) -> Tristate {
        self.uses_uri_style_node_core_modules
    }

    /** This should have similar behavior to 'processSourceFile' without diagnostics or mutation. */
    pub fn get_source_file_from_reference(
        &self,
        origin: P<SourceFile>,
        ref_: P<FileReference>,
    ) -> Option<P<SourceFile>> {
        let file_name = tspath::resolve_path(
            &tspath::get_directory_path(origin.file_name()),
            &[&ref_.file_name],
        );
        let supported_extensions_base = tsoptions::get_supported_extensions(
            Some(&self.options()),
            &self.command_line().content_mapper_extensions(),
        );
        let supported_extensions =
            tsoptions::get_supported_extensions_with_json_if_resolve_json_module(
                Some(&self.options()),
                &supported_extensions_base,
            );
        let allow_non_ts_extensions = self.options().allow_non_ts_extensions.is_true();
        if tspath::has_extension(&file_name) {
            if !allow_non_ts_extensions {
                let canonical_file_name = tspath::get_canonical_file_name(
                    &file_name,
                    self.use_case_sensitive_file_names(),
                );
                let supported = supported_extensions.iter().any(|group| {
                    tspath::file_extension_is_one_of(&canonical_file_name, &str_slice(group))
                });
                if !supported {
                    return None; // unsupported extensions are forced to fail
                }
            }

            return self.get_source_file_for_resolved_module(&file_name);
        }
        if allow_non_ts_extensions
            && let Some(extensionless) = self.get_source_file_for_resolved_module(&file_name)
        {
            return Some(extensionless);
        }

        // Only try adding extensions from the first supported group (which should be .ts/.tsx/.d.ts)
        if let Some(result) = supported_extensions[0].iter().find_map(|ext| {
            if let Some(result) =
                self.get_source_file_for_resolved_module(&format!("{file_name}{ext}"))
            {
                return Some(Some(result));
            }

            None
        }) {
            return result;
        }
        None
    }
}

// Parsing and binding recurse deeply on large files; Go's goroutine stacks grow on demand, so the
// worker threads get large stacks. The pool has at most PARSE_THREAD_CAP threads: with the heap on 4 KiB pages
// (mimalloc `no_thp`, notes/mem-no-thp.md) the parallel parse is bound by page-fault contention, not threads (vscode
// on 64 vCPUs: 0.124 s at 32 threads, 0.131 s at 64), and every allocating thread keeps its own partly filled
// allocator pages and arena chunk tail (64 -> 32 threads: -0.09 GiB peak). RAYON_NUM_THREADS still sets the count
// exactly, as before (single-threaded runs with `node bench/bench.mjs count` use 1).
const PARSE_THREAD_CAP: usize = 32;

/// Runs worker jobs on the calling thread for threadless targets.
///
/// `tsrs_core::NO_THREADS` targets: a one-thread pool on the calling thread (`use_current_thread`), on which
/// `install`, `par_iter`, `broadcast` and nested scopes run inline. It must be the first rayon use on that
/// thread: once rayon's global pool has started there, building it fails with "The current thread is already
/// part of another thread pool", so an embedder calls `worker_pool()` before anything else.
pub fn worker_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        if tsrs_core::NO_THREADS {
            return rayon::ThreadPoolBuilder::new()
                .num_threads(1)
                .use_current_thread()
                .build()
                .unwrap();
        }
        let threads = std::env::var("RAYON_NUM_THREADS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|&n| n > 0)
            .unwrap_or_else(|| {
                std::thread::available_parallelism()
                    .map_or(1, std::num::NonZero::get)
                    .min(PARSE_THREAD_CAP)
            });
        rayon::ThreadPoolBuilder::new().num_threads(threads).stack_size(256 << 20).build().unwrap()
    })
}

// program.go:305
pub fn new_program(opts: ProgramOptions) -> &'static Program {
    let single_threaded = tsrs_core::NO_THREADS
        || opts
            .single_threaded
            .default_if_unknown(opts.config.compiler_options().unwrap().single_threaded)
            .is_true();
    let (mut processed, resolution_data, module_resolution_error) =
        process_all_program_files(&opts, single_threaded);
    let project_reference_file_mapper: &'static ProjectReferenceFileMapper =
        processed.project_reference_file_mapper.take().unwrap();
    let processing_diagnostics =
        std::mem::take(&mut processed.file_include_data.processing_diagnostics);
    let files = std::mem::take(&mut processed.files);
    let files_by_path = std::mem::take(&mut processed.files_by_path);
    let host = Arc::clone(&opts.host);
    let mut p = Program {
        opts: opts.program_config(),
        // Go's NewProgram never sets `comparePathsOptions`: it is the zero value (no current directory,
        // case-insensitive), which e.g. makes IsGlobalTypingsFile false when no typings location is set.
        compare_paths_options: tsrs_core::tspath::ComparePathsOptions::default(),
        resolution_host: crate::projectreferencefilemapper::resolution_host_for(Arc::clone(&host)),
        host,
        resolution_data,
        checker_pool: OnceLock::new(),
        include_processor: IncludeProcessor::default(),
        module_resolution_error,
        files,
        files_by_path,
        processed: Box::leak(Box::new(processed)),
        project_reference_file_mapper,
        processing_diagnostics: Mutex::new(processing_diagnostics),
        uses_uri_style_node_core_modules: Tristate::Unknown,
        common_source_directory: OnceLock::new(),
        declaration_diagnostic_cache: Mutex::new(FxHashMap::default()),
        program_diagnostics: Vec::new(),
        has_emit_blocking_diagnostics: FxHashSet::default(),
        content_mapper_option_diagnostics: Vec::new(),
        unresolved_imports: OnceLock::new(),
        known_symlinks: OnceLock::new(),
        package_names: OnceLock::new(),
        has_ts_file: OnceLock::new(),
        packages_map: OnceLock::new(),
        exports_module_name_cache: tsrs_modulespecifiers::ExportsModuleNameCache::default(),
        leaf_files: opts.leaf_files,
        leaf_pass_started: std::sync::atomic::AtomicBool::new(false),
        leaf_referred: OnceLock::new(),
        tsgo_check_slots: OnceLock::new(),
        tsgo_import_adjacency: OnceLock::new(),
        checker_recycling: opts.checker_recycling,
        canonical_diagnostics: opts.canonical_diagnostics,
        checker_regions: opts.checker_regions,
        checker_diagnostic_regions: opts.checker_diagnostic_regions,
        reuse_synthetic_imports: opts.reuse_synthetic_imports,
        reused_from: 0,
        own_region: None,
    };
    // Go initializes the checker pool before verifying options; the pool factory takes the program by
    // `&'static`, so here it runs after verification, once the program is leaked. Neither pool reads anything
    // verification writes (checkers are created lazily).
    tsrs_core::phases::time(p.options().diagnostics_enabled(), "Program: verify options", || {
        p.verify_compiler_options();
    });
    p.collect_content_mapper_option_diagnostics();
    let p: &'static Program = Box::leak(Box::new(p));
    p.init_checker_pool(opts.create_checker_pool, None);
    p
}

/// Releases a program after every external reference to it has ended.
///
/// Frees a program made by `new_program` or `update_program` (language server; Go's GC). The caller
/// guarantees that nothing uses it any more: no checker of its pool is held, and no snapshot or language
/// service refers to it. What it shares with other versions (`processed`, the project reference file mapper)
/// stays.
///
/// # Safety
/// `program` came from `new_program` / `update_program` and is not used afterwards.
pub unsafe fn free_program(program: &'static Program) {
    let resolution_host: *const dyn ResolutionHost = program.resolution_host;
    // SAFETY: both functions leak the program from a `Box`, and nothing uses it afterwards (this function's contract).
    drop(unsafe { Box::from_raw(std::ptr::from_ref::<Program>(program).cast_mut()) });
    // Per program (`resolution_host_for`); it keeps the compiler host alive.
    // SAFETY: `resolution_host_for` leaked it from a `Box` for this program alone, which is gone.
    drop(unsafe { Box::from_raw(resolution_host as *mut dyn ResolutionHost) });
}

/// Releases a program that never shared data with another version.
///
/// Frees a program from `new_program` that never shared data with another version (it was not the source or
/// the result of `update_program`), including its processed-file data and project reference file mapper,
/// which `free_program` keeps because language-server program versions share them. Used for one-shot programs
/// (the native API's transpileModule / transpileDeclaration).
///
/// # Safety
/// As `free_program`, and additionally: no other program refers to `program`'s processed files or mapper.
pub unsafe fn free_unshared_program(program: &'static Program) {
    let shared = shared_program_data(program);
    // SAFETY: this function's contract includes `free_program`'s.
    unsafe { free_program(program) };
    // SAFETY: the only program that shared this data was just freed (this function's contract).
    unsafe { shared.free() };
}

/// Keeps data shared between program versions alive together.
///
/// The data program versions share (`processed`, the project reference file mapper, the file loader's
/// resolution host and the mapper's dts-faking host), as an address pair that can be moved into a region's
/// `on_free` hook.
pub struct SharedProgramData {
    processed: usize,
    mapper: usize,
}

/// See `SharedProgramData`.
pub fn shared_program_data(program: &'static Program) -> SharedProgramData {
    SharedProgramData {
        processed: std::ptr::from_ref::<ProcessedFiles>(program.processed) as usize,
        mapper: std::ptr::from_ref::<ProjectReferenceFileMapper>(
            program.project_reference_file_mapper,
        ) as usize,
    }
}

impl SharedProgramData {
    /// # Safety
    /// Every program that shares this data has been freed, and nothing else refers to it.
    pub unsafe fn free(self) {
        // SAFETY: the file loader leaked the mapper from a `Box`, and nothing refers to it (this function's contract).
        unsafe {
            free_project_reference_file_mapper(self.mapper as *mut ProjectReferenceFileMapper);
        };
        // SAFETY: `new_program` leaked `processed` from a `Box`, and nothing refers to it (this function's contract).
        drop(unsafe { Box::from_raw(self.processed as *mut ProcessedFiles) });
    }
}

/// Frees a leaked project reference file mapper with its loader and dts-faking resolution hosts.
///
/// # Safety
/// `mapper` came from `Box::leak` in the file loader, and nothing refers to it or its hosts any more.
pub unsafe fn free_project_reference_file_mapper(mapper: *mut ProjectReferenceFileMapper) {
    // SAFETY: `mapper` is a live leaked `Box` (this function's contract).
    let (dts_faking_host, loader_host) =
        unsafe { ((*mapper).dts_faking_host.get().copied(), (*mapper).loader_host) };
    // SAFETY: it came from `Box::leak`, and nothing refers to it any more (this function's contract).
    drop(unsafe { Box::from_raw(mapper) });
    if let Some(h) = dts_faking_host {
        // SAFETY: `new_project_reference_dts_faking_host` leaked it from a `Box` for this mapper, which is gone.
        drop(unsafe {
            Box::from_raw(std::ptr::from_ref::<dyn ResolutionHost>(h) as *mut dyn ResolutionHost)
        });
    }
    if let Some(h) = loader_host {
        // SAFETY: `resolution_host_for` leaked it from a `Box` for this mapper's loader, which is gone.
        drop(unsafe {
            Box::from_raw(std::ptr::from_ref::<dyn ResolutionHost>(h) as *mut dyn ResolutionHost)
        });
    }
}

impl Program {
    // program.go:323
    // Return an updated program for which it is known that only the file with the given path has changed.
    // In addition to a new program, return a boolean indicating whether the data of the old program was reused.
    // The returned *ast.SourceFile is the changed file as acquired through newHost; it is nil
    // only if the host cannot locate the file (e.g. it was deleted). Callers that manage
    // host-side parse caches must release this exact pointer when the old program could not be
    // reused, since it was acquired speculatively before that decision was made.
    pub fn update_program(
        &'static self,
        changed_file_path: &Path,
        new_host: Arc<dyn CompilerHost>,
        create_checker_pool: Option<CreateCheckerPool>,
        create_module_resolver: Option<CreateModuleResolver>,
    ) -> (&'static Program, Option<P<SourceFile>>, bool) {
        let (result, new_file, reused) = self.reuse_program(
            changed_file_path,
            Arc::clone(&new_host),
            create_checker_pool.clone(),
            create_module_resolver.clone(),
        );
        if reused {
            (result.unwrap(), new_file, true)
        } else {
            let mut opts = ProgramOptions::from_config(
                self.opts.clone(),
                new_host,
                create_checker_pool,
                create_module_resolver,
            );
            opts.canonical_diagnostics = self.canonical_diagnostics;
            (new_program(opts), new_file, false)
        }
    }

    // program.go:347
    // ReuseProgram attempts to produce a new program by replacing only
    // changedFilePath in place, reusing the rest of p. It returns
    // (newProgram, newFile, true) on success, or (nil, newFile, false) when the
    // file cannot be replaced in place. Unlike UpdateProgram, it never constructs a
    // full fallback program, so callers that build their own fallback (e.g. with a
    // different host) do not pay for a discarded program build.
    pub fn reuse_program(
        &'static self,
        changed_file_path: &Path,
        new_host: Arc<dyn CompilerHost>,
        create_checker_pool: Option<CreateCheckerPool>,
        _create_module_resolver: Option<CreateModuleResolver>,
    ) -> (Option<&'static Program>, Option<P<SourceFile>>, bool) {
        let old_file = self.files_by_path[changed_file_path];
        let new_file;
        let mut old_supplemental_files: &[P<SourceFile>] = &[];
        let mut new_supplemental_files: Vec<P<SourceFile>> = Vec::new();
        if old_file.content_mapper().is_empty() {
            new_file = new_host.get_source_file(old_file.parse_options().clone());
        } else {
            // Content-mapped files are produced by running an external transform, which a plain reparse can't
            // reproduce. Re-run the transform through the host; any failure (or a missing file) falls back to
            // a full rebuild so the file loader's failure policy runs.
            // (Go passes a nil mapper on to the host, whose transform then fails; that is the same fallback.)
            let Some(mapper) =
                self.opts.config.get().get_content_mapper_for_file_name(old_file.file_name())
            else {
                return (None, None, false);
            };
            match new_host.get_content_mapped_source_files(old_file.parse_options().clone(), mapper)
            {
                Err(_) => return (None, None, false),
                Ok(files) => {
                    new_file = files.canonical;
                    old_supplemental_files = old_file.supplemental_source_files();
                    new_supplemental_files = files.supplemental;
                }
            }
        }

        // If this file is part of a package redirect group (same package installed in multiple
        // node_modules locations), we need to rebuild the program because the redirect targets
        // might need recalculation.
        let in_redirect_files = self.redirect_files_by_path.contains_key(changed_file_path);
        let is_redirect_target = self.redirect_targets_map.contains_key(changed_file_path);
        if in_redirect_files || is_redirect_target {
            return (None, new_file, false);
        }

        if self.module_resolution_error.is_some()
            || !self.can_replace_file_in_program(old_file, new_file)
        {
            return (None, new_file, false);
        }
        let new_file_some = new_file.unwrap();
        if !self.can_keep_synthetic_imports(old_file, new_file_some) {
            return (None, new_file, false);
        }
        if old_supplemental_files.len() != new_supplemental_files.len() {
            return (None, new_file, false);
        }
        if let Some(result) =
            old_supplemental_files.iter().enumerate().find_map(|(i, &old_supplemental)| {
                let new_supplemental = new_supplemental_files[i];
                if old_supplemental.path() != new_supplemental.path()
                    || !self.can_replace_file_in_program(old_supplemental, Some(new_supplemental))
                {
                    return Some((None, new_file, false));
                }
                if !self.can_keep_synthetic_imports(old_supplemental, new_supplemental) {
                    return Some((None, new_file, false));
                }

                None
            })
        {
            return result;
        }
        // TODO: reverify compiler options when config has changed?
        let own_region =
            self.checker_regions.then(|| tsrs_core::arena::Region::new_scratch(64 << 10));
        let resolution_data = {
            let _scope = own_region.as_ref().map(tsrs_core::arena::Region::enter);
            self.resolution_data.clone_data()
        };
        let mut result = Program {
            opts: self.opts.clone(),
            resolution_host: crate::projectreferencefilemapper::resolution_host_for(Arc::clone(
                &new_host,
            )),
            host: new_host,
            resolution_data,
            checker_pool: OnceLock::new(),
            include_processor: IncludeProcessor::default(),
            module_resolution_error: None,
            compare_paths_options: self.compare_paths_options.clone(),
            files: self.files,
            files_by_path: FxHashMap::default(),
            processed: self.processed,
            project_reference_file_mapper: self.project_reference_file_mapper,
            processing_diagnostics: Mutex::new(self.processing_diagnostics().clone()),
            uses_uri_style_node_core_modules: self.uses_uri_style_node_core_modules,
            common_source_directory: OnceLock::new(),
            declaration_diagnostic_cache: Mutex::new(FxHashMap::default()),
            program_diagnostics: self.program_diagnostics.clone(),
            has_emit_blocking_diagnostics: self.has_emit_blocking_diagnostics.clone(),
            content_mapper_option_diagnostics: self.content_mapper_option_diagnostics.clone(),
            unresolved_imports: OnceLock::new(),
            known_symlinks: OnceLock::new(),
            package_names: OnceLock::new(),
            has_ts_file: OnceLock::new(),
            packages_map: OnceLock::new(),
            exports_module_name_cache: tsrs_modulespecifiers::ExportsModuleNameCache::default(),
            // A reused program keeps the files of this one, so nothing of them may be freed.
            leaf_files: crate::fileregions::LeafMode::Off,
            leaf_pass_started: std::sync::atomic::AtomicBool::new(false),
            leaf_referred: OnceLock::new(),
            tsgo_check_slots: OnceLock::new(),
            tsgo_import_adjacency: OnceLock::new(),
            checker_recycling: false,
            canonical_diagnostics: self.canonical_diagnostics,
            checker_regions: self.checker_regions,
            checker_diagnostic_regions: self.checker_diagnostic_regions,
            reuse_synthetic_imports: self.reuse_synthetic_imports,
            reused_from: std::ptr::from_ref(self) as usize,
            own_region,
        };
        try_reuse(&result.unresolved_imports, &self.unresolved_imports);
        try_reuse(&result.tsgo_import_adjacency, &self.tsgo_import_adjacency);
        try_reuse(&result.known_symlinks, &self.known_symlinks);
        try_reuse(&result.package_names, &self.package_names);
        let index =
            result.files.iter().position(|file| file.path() == new_file_some.path()).unwrap();
        let mut files = result.files.to_vec();
        files[index] = new_file_some;
        result.files_by_path.clone_from(&self.files_by_path);
        result.files_by_path.insert(new_file_some.path().clone(), new_file_some);
        for (i, &old_supplemental) in old_supplemental_files.iter().enumerate() {
            let new_supplemental = new_supplemental_files[i];
            let supplemental_index =
                files.iter().position(|&file| file == old_supplemental).unwrap();
            files[supplemental_index] = new_supplemental;
            result.files_by_path.insert(new_supplemental.path().clone(), new_supplemental);
        }
        result.files = {
            let _scope = result.own_region.as_ref().map(tsrs_core::arena::Region::enter);
            tsrs_core::alloc_vec(files)
        };
        let result: &'static Program = Box::leak(Box::new(result));
        result.init_checker_pool(create_checker_pool, self.compiler_checker_pool());
        (Some(result), new_file, true)
    }

    /// tsrs-only: whether `reuse_program` made this program from `other`: the files of `other` but one (and its
    /// supplemental files), reparsed with the same imports, references, module augmentation and ambient module names,
    /// and the module resolution of `other`. Only meaningful while `other` is alive.
    pub fn is_reuse_of(&self, other: &Program) -> bool {
        self.reused_from == std::ptr::from_ref(other) as usize
    }

    // program.go:443
    // `previous`: the pool of the program this one reuses (`reuse_program`), whose file assignment it may take over.
    fn init_checker_pool(
        &'static self,
        create: Option<CreateCheckerPool>,
        previous: Option<&CheckerPoolImpl>,
    ) {
        assert!(
            self.finished_processing,
            "Program must finish processing files before initializing checker pool"
        );

        let pool = match create {
            Some(create) => ProgramCheckerPool::External(create(self)),
            None => ProgramCheckerPool::Compiler(CheckerPoolImpl::new(self, previous)),
        };
        assert!(self.checker_pool.set(pool).is_ok(), "checker pool initialized twice");
    }

    // program.go:458
    // GetCheckerPool returns the checker pool associated with this program.
    fn get_checker_pool(&self) -> &dyn CheckerPool {
        match self.checker_pool.get().unwrap() {
            ProgramCheckerPool::Compiler(pool) => pool,
            ProgramCheckerPool::External(pool) => &**pool,
        }
    }

    // Go `p.compilerCheckerPool` (nil when the program was created with `CreateCheckerPool`).
    pub fn compiler_checker_pool(&self) -> Option<&CheckerPoolImpl> {
        match self.checker_pool.get() {
            Some(ProgramCheckerPool::Compiler(pool)) => Some(pool),
            _ => None,
        }
    }

    // The built-in pool, for the CLI's statistics; panics on a program with an external pool.
    pub fn pool(&self) -> &CheckerPoolImpl {
        self.compiler_checker_pool().expect("program uses an external checker pool")
    }

    // program.go:462
    fn can_replace_file_in_program(
        &self,
        file1: P<SourceFile>,
        file2: Option<P<SourceFile>>,
    ) -> bool {
        let Some(file2) = file2 else {
            return false;
        };
        file1.parse_options() == file2.parse_options()
            && file1.script_kind.get() == file2.script_kind.get()
            && ast::is_external_or_common_js_module(file1)
                == ast::is_external_or_common_js_module(file2)
            && file1.uses_uri_style_node_core_modules() == file2.uses_uri_style_node_core_modules()
            && equal_func(file1.imports(), file2.imports(), |n1, n2| {
                equal_module_specifiers(n1, n2)
                    && self.get_mode_for_usage_location(file1, n1)
                        == self.get_mode_for_usage_location(file2, n2)
                    && n1.parent().is_some_and(ast::is_source_phase_import)
                        == n2.parent().is_some_and(ast::is_source_phase_import)
            })
            && equal_func(
                file1.module_augmentations(),
                file2.module_augmentations(),
                equal_module_augmentation_names,
            )
            && file1.ambient_module_names() == file2.ambient_module_names()
            && equal_func(file1.referenced_files(), file2.referenced_files(), equal_file_references)
            && equal_func(
                file1.type_reference_directives.get(),
                file2.type_reference_directives.get(),
                equal_file_references,
            )
            && equal_func(
                file1.lib_reference_directives.get(),
                file2.lib_reference_directives.get(),
                equal_file_references,
            )
            && equal_check_js_directives(
                file1.check_js_directive.get(),
                file2.check_js_directive.get(),
            )
    }

    // Whether the program `reuse_program` makes may keep the synthetic imports the file loader made for `old_file`,
    // the version of it in this program (the import helpers module and the JSX runtime module), with their module
    // resolution (the clone keeps the file's), as it keeps the file's other imports. Go builds the program again
    // whenever either version has one (program.go:389), so every edit of a file with JSX (`react-jsx`) or under
    // `importHelpers` is a full build. tsrs-only, with `reuse_synthetic_imports`: they are kept when `new_file` needs
    // the same ones. The synthetic nodes' parent stays the old version of the file, which only lends them its path,
    // options and implied format, the same as the new version's (`can_replace_file_in_program` compares the parse
    // options). The JSX runtime comes from the options and the file's pragmas; whether the file needs the helpers
    // module, from the options and whether it is a module, which `can_replace_file_in_program` also compares.
    fn can_keep_synthetic_imports(&self, old_file: P<SourceFile>, new_file: P<SourceFile>) -> bool {
        let old_helpers = self.import_helpers_import_specifiers.contains_key(old_file.path());
        let new_helpers = self.needs_import_helpers_import_specifier(new_file);
        let old_jsx_runtime = self
            .jsx_runtime_import_specifiers
            .get(old_file.path())
            .map_or("", |jsx| jsx.module_reference.as_str());
        let new_jsx_runtime = self.jsx_runtime_import_specifier(new_file);
        if !self.reuse_synthetic_imports {
            return !old_helpers
                && !new_helpers
                && old_jsx_runtime.is_empty()
                && new_jsx_runtime.is_empty();
        }
        old_helpers == new_helpers && old_jsx_runtime == new_jsx_runtime
    }

    // program.go:480
    fn needs_import_helpers_import_specifier(&self, file: P<SourceFile>) -> bool {
        let redirect = self
            .project_reference_file_mapper
            .get_redirect_parsed_command_line_for_resolution(file.file_name(), file.path());
        let redirect =
            redirect.map(crate::projectreferencefilemapper::as_resolved_project_reference);
        let options_for_file = module::get_compiler_options_with_redirect(
            self.opts.config.compiler_options().unwrap(),
            redirect,
        );
        if !options_for_file.import_helpers.is_true() {
            return false;
        }
        let is_java_script_file = ast::is_source_file_js(file);
        let is_external_module_file = ast::is_external_module(file);
        if !is_java_script_file
            && (file.is_declaration_file.get()
                || (!options_for_file.get_isolated_modules() && !is_external_module_file))
        {
            return false;
        }
        true
    }

    // program.go:494
    fn jsx_runtime_import_specifier(&self, file: P<SourceFile>) -> String {
        if !ast::is_source_file_js(file) && file.script_kind.get() != ScriptKind::TSX {
            return String::new();
        }
        let redirect = self
            .project_reference_file_mapper
            .get_redirect_parsed_command_line_for_resolution(file.file_name(), file.path());
        let redirect =
            redirect.map(crate::projectreferencefilemapper::as_resolved_project_reference);
        let options_for_file = module::get_compiler_options_with_redirect(
            self.opts.config.compiler_options().unwrap(),
            redirect,
        );
        ast::get_jsx_runtime_import(
            &ast::get_jsx_implicit_import_base(&options_for_file, Some(file)),
            &options_for_file,
        )
    }

    // program.go:519
    pub fn source_files(&self) -> &'static [P<SourceFile>] {
        self.files
    }

    // program.go:520
    pub fn duplicate_source_files(&self) -> &[DuplicateSourceFile] {
        &self.processed.duplicate_source_files
    }

    // program.go:521
    pub fn options(&self) -> P<CompilerOptions> {
        self.opts.config.compiler_options().unwrap()
    }

    // GetContentMapper returns the content mapper that produced the given source file, or nil if the
    // file was not produced by a content mapper.

    pub(crate) fn content_mapper_extension_rewrites(&self) -> Vec<tspath::ExtensionRewrite> {
        self.opts.config.content_mapper_extension_rewrites().to_vec()
    }

    // program.go:537
    pub fn command_line(&self) -> P<ParsedCommandLine> {
        self.opts.config
    }

    // program.go:538
    pub fn host(&self) -> &Arc<dyn CompilerHost> {
        &self.host
    }

    // program.go:540
    pub fn get_config_file_parsing_diagnostics(&self) -> Vec<P<Diagnostic>> {
        self.opts.config.get_config_file_parsing_diagnostics()
    }

    // program.go:546
    // GetUnresolvedImports returns the unresolved imports for this program.
    // The result is cached and computed only once.
    pub fn get_unresolved_imports(&self) -> &Set<String> {
        self.unresolved_imports.get_or_init(|| Arc::new(self.extract_unresolved_imports()))
    }

    // program.go:550
    fn extract_unresolved_imports(&self) -> Set<String> {
        let mut unresolved_set = Set::default();

        for &source_file in self.files {
            let unresolved_imports = self.extract_unresolved_imports_from_source_file(source_file);
            unresolved_set.m.extend(unresolved_imports);
        }

        unresolved_set
    }

    // program.go:563
    fn extract_unresolved_imports_from_source_file(&self, file: P<SourceFile>) -> Vec<String> {
        let mut unresolved_imports = Vec::new();

        if let Some(resolved_modules) = self.resolved_modules.get(file.path()) {
            // the only caller adds the names to a hash set
            unresolved_imports.extend(resolved_modules.iter().filter_map(
                |(cache_key, resolution)| {
                    let resolved = resolution.is_resolved();
                    if (!resolved
                        || !tspath::extension_is_one_of(
                            resolution.extension,
                            tspath::SUPPORTED_TS_EXTENSIONS_WITH_JSON_FLAT,
                        ))
                        && !tspath::is_external_module_name_relative(cache_key.name)
                    {
                        Some(cache_key.name.to_string())
                    } else {
                        None
                    }
                },
            ));
        }

        unresolved_imports
    }

    pub(crate) fn processing_diagnostics(
        &self,
    ) -> std::sync::MutexGuard<'_, Vec<ProcessingDiagnostic>> {
        self.processing_diagnostics.lock().unwrap()
    }

    fn add_processing_diagnostic(&self, d: ProcessingDiagnostic) {
        self.processing_diagnostics.lock().unwrap().push(d);
    }

    // program.go:578
    pub fn single_threaded(&self) -> bool {
        tsrs_core::NO_THREADS
            || self
                .opts
                .single_threaded
                .default_if_unknown(self.options().single_threaded)
                .is_true()
    }

    // program.go:582
    pub fn bind_source_files(&self) {
        let unbound: Vec<P<SourceFile>> =
            self.files.iter().copied().filter(|f| !f.is_bound()).collect();
        if self.single_threaded() {
            // Go's single-threaded work group runs queued functions last-queued first, so files bind in reverse
            // program order. The order is observable: the binder assigns symbol ids (private names).
            unbound.into_iter().rev().for_each(crate::fileregions::bind);
        } else {
            worker_pool().install(|| unbound.into_par_iter().for_each(crate::fileregions::bind));
        }
    }

    // program.go:598
    // Return the type checker associated with the program.
    pub fn get_type_checker(&self, ctx: &Context) -> CheckerHandle {
        if let Some(pool) = self.compiler_checker_pool() {
            return pool.get_checker_non_exclusive();
        }
        self.get_checker_pool().get_checker(ctx, None)
    }

    // program.go:605
    pub fn for_each_checker_parallel(&self, cb: impl Fn(usize, &mut Checker) + Sync) {
        if let Some(pool) = self.compiler_checker_pool() {
            pool.for_each_checker_parallel(cb);
        }
    }

    /// Runs `cb` for every file of `files` with the file's checker, one task per checker, each visiting its files in
    /// the order of `files` under one lock acquisition (`checkerPool::for_each_checker_group_do`). False, without
    /// calling `cb`, when the program has no checker pool of its own.
    pub fn for_each_checker_group(
        &self,
        files: &[P<SourceFile>],
        cb: impl Fn(&mut Checker, usize, P<SourceFile>) + Sync,
    ) -> bool {
        let Some(pool) = self.compiler_checker_pool() else {
            return false;
        };
        pool.for_each_checker_group_do(files, self.single_threaded(), cb);
        true
    }

    // program.go:615
    // Return a checker for the given file. We may have multiple checkers in concurrent scenarios and this
    // method returns the checker that was tasked with checking the file. Note that it isn't possible to mix
    // types obtained from different checkers, so only non-type data (such as diagnostics or string
    // representations of types) should be obtained from checkers returned by this method.
    pub fn get_type_checker_for_file(&self, ctx: &Context, file: P<SourceFile>) -> CheckerHandle {
        if let Some(pool) = self.compiler_checker_pool() {
            return pool.get_checker_for_file_non_exclusive(file);
        }
        self.get_checker_pool().get_checker(ctx, Some(file))
    }

    // program.go:624
    // Return a checker for the given file, locked to the current thread to prevent data races from multiple threads
    // accessing the same checker. The lock will be released when the `done` function is called.
    pub fn get_type_checker_for_file_exclusive(
        &self,
        ctx: &Context,
        file: P<SourceFile>,
    ) -> CheckerHandle {
        if let Some(pool) = self.compiler_checker_pool() {
            return pool.get_checker_for_file_exclusive(file);
        }
        self.get_checker_pool().get_checker(ctx, Some(file))
    }

    pub fn get_resolved_module(
        &self,
        file: P<SourceFile>,
        module_reference: &str,
        mode: ResolutionMode,
    ) -> Option<P<ResolvedModule>> {
        self.get_resolved_module_by_path(file.path(), module_reference, mode)
    }

    fn get_resolved_module_by_path(
        &self,
        path: &Path,
        module_reference: &str,
        mode: ResolutionMode,
    ) -> Option<P<ResolvedModule>> {
        let resolutions = self.resolved_modules.get(path)?;
        tsrs_module::mode_aware_cache_get(resolutions, module_reference, mode).copied()
    }

    pub fn get_resolved_module_from_module_specifier(
        &self,
        file: P<SourceFile>,
        module_specifier: P<Node>,
    ) -> Option<P<ResolvedModule>> {
        assert!(
            ast::is_string_literal_like(module_specifier),
            "moduleSpecifier must be a StringLiteralLike"
        );
        if module_specifier.parent().is_some_and(ast::is_source_phase_import) {
            return None;
        }
        let mode = self.get_mode_for_usage_location(file, module_specifier);
        self.get_resolved_module(file, module_specifier.text(), mode)
    }

    pub fn get_resolved_modules(&self) -> &FxHashMap<Path, ModeAwareCache<P<ResolvedModule>>> {
        &self.resolved_modules
    }

    pub fn module_resolution_error(&self) -> Option<&str> {
        self.module_resolution_error.as_deref()
    }

    // GetPackagesMap returns a lazily-cached map of package names to whether they bundle types.
    pub(crate) fn get_packages_map(&self) -> &FxHashMap<String, bool> {
        self.packages_map.get_or_init(|| {
            let mut packages_map: FxHashMap<String, bool> = FxHashMap::default();
            #[expect(
                clippy::iter_over_hash_type,
                reason = "ORs one flag per package name; the result does not depend on the order"
            )]
            for resolved_modules_in_file in self.resolved_modules.values() {
                for module in resolved_modules_in_file.values() {
                    if !module.package_id.name.is_empty() {
                        let entry =
                            packages_map.entry(module.package_id.name.to_string()).or_insert(false);
                        *entry = *entry || module.extension == tspath::EXTENSION_DTS;
                    }
                }
            }
            packages_map
        })
    }

    // program.go:675
    // collectDiagnostics collects diagnostics from a single file or all files.
    // If sourceFile is non-nil, returns diagnostics for just that file.
    // If sourceFile is nil, returns diagnostics for all files in the program.
    fn collect_diagnostics(
        &self,
        ctx: &Context,
        source_file: Option<P<SourceFile>>,
        concurrent: bool,
        collect: impl Fn(&Context, P<SourceFile>) -> Vec<P<Diagnostic>> + Sync,
    ) -> Vec<P<Diagnostic>> {
        let result = if let Some(file) = source_file {
            crate::fileregions::assert_not_freed(file);
            collect(ctx, file)
        } else {
            let diagnostics =
                self.collect_diagnostics_from_files(ctx, self.files, concurrent, &collect);
            diagnostics.concat()
        };
        filter_and_sort_diagnostics(&result)
    }

    // program.go:686
    fn collect_diagnostics_from_files(
        &self,
        ctx: &Context,
        source_files: &[P<SourceFile>],
        concurrent: bool,
        collect: &(impl Fn(&Context, P<SourceFile>) -> Vec<P<Diagnostic>> + Sync),
    ) -> Vec<Vec<P<Diagnostic>>> {
        if !concurrent || self.single_threaded() {
            source_files.iter().map(|&f| collect(ctx, f)).collect()
        } else {
            worker_pool().install(|| source_files.par_iter().map(|&f| collect(ctx, f)).collect())
        }
    }

    // program.go:703
    // collectCheckerDiagnostics collects diagnostics from a single file or all files,
    // using a callback that receives the checker for each file. When the checker pool
    // supports grouped iteration (compiler pool), files are grouped by checker and
    // processed in parallel with one task per checker, reducing contention and improving
    // cache locality. Otherwise, falls back to per-file concurrent collection.
    fn collect_checker_diagnostics(
        &'static self,
        ctx: &Context,
        source_file: Option<P<SourceFile>>,
        collect: impl Fn(&Context, &mut Checker, P<SourceFile>) -> Vec<P<Diagnostic>> + Sync,
    ) -> Vec<P<Diagnostic>> {
        if let Some(source_file) = source_file {
            crate::fileregions::assert_not_freed(source_file);
            if self.skip_type_checking(source_file, false) {
                return Vec::new();
            }
            let mut c = self.get_type_checker_for_file_exclusive(ctx, source_file);
            let result = collect(ctx, &mut c, source_file);
            drop(c);
            return filter_and_sort_diagnostics(&result);
        }
        filter_and_sort_diagnostics(
            &self.collect_checker_diagnostics_from_files(ctx, self.files, &collect).concat(),
        )
    }

    // program.go:728
    // collectCheckerDiagnosticsFromFiles collects checker diagnostics for a list of files.
    fn collect_checker_diagnostics_from_files(
        &'static self,
        ctx: &Context,
        source_files: &[P<SourceFile>],
        collect: &(impl Fn(&Context, &mut Checker, P<SourceFile>) -> Vec<P<Diagnostic>> + Sync),
    ) -> Vec<Vec<P<Diagnostic>>> {
        self.collect_checker_diagnostics_from_files_ex(ctx, source_files, false, collect)
    }

    // `allow_steal`: files may move between checkers while they are checked (checkerPool::for_each_checker_group_do_ex).
    fn collect_checker_diagnostics_from_files_ex(
        &'static self,
        ctx: &Context,
        source_files: &[P<SourceFile>],
        allow_steal: bool,
        collect: &(impl Fn(&Context, &mut Checker, P<SourceFile>) -> Vec<P<Diagnostic>> + Sync),
    ) -> Vec<Vec<P<Diagnostic>>> {
        let diagnostics: Vec<Mutex<Vec<P<Diagnostic>>>> =
            source_files.iter().map(|_| Mutex::new(Vec::new())).collect();
        if let Some(pool) = self.compiler_checker_pool() {
            pool.for_each_checker_group_do_ex(
                source_files,
                self.single_threaded(),
                allow_steal,
                allow_steal.then_some(ctx),
                |c, file_index, file| {
                    *diagnostics[file_index].lock().unwrap() = collect(ctx, c, file);
                },
            );
        } else {
            let run = |i: usize, file: P<SourceFile>| {
                if self.skip_type_checking(file, false) {
                    return;
                }
                let mut c = self.get_checker_pool().get_checker(ctx, Some(file));
                *diagnostics[i].lock().unwrap() = collect(ctx, &mut c, file);
                drop(c);
            };
            if self.single_threaded() {
                // Go's single-threaded work group runs queued functions last-queued first.
                source_files.iter().enumerate().rev().for_each(|(i, &file)| run(i, file));
            } else {
                worker_pool().install(|| {
                    source_files.par_iter().enumerate().for_each(|(i, &file)| run(i, file));
                });
            }
        }
        diagnostics.into_iter().map(|d| d.into_inner().unwrap()).collect()
    }

    // program.go:751
    pub fn get_syntactic_diagnostics(
        &self,
        ctx: &Context,
        source_file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        self.collect_diagnostics(ctx, source_file, false /*concurrent*/, |_, file| {
            let mut diags: Vec<P<Diagnostic>> = file.diagnostics().to_vec();
            diags.extend_from_slice(file.js_diagnostics());
            // For JS files that won't be checked by the checker (no checkJs/ts-check), we need
            // program-level syntactic checks that require compiler options. This mirrors Strada's
            // getJSSyntacticDiagnosticsForFile in program.ts.
            if ast::is_source_file_js(file)
                && !ast::is_check_js_enabled_for_file(file, &self.options())
            {
                diags.extend(get_additional_js_syntactic_diagnostics(file, &self.options()));
            }
            diags
        })
    }

    // program.go:795
    pub fn get_bind_diagnostics(
        &self,
        ctx: &Context,
        source_file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        match source_file {
            Some(file) => crate::fileregions::bind(file),
            None => self.bind_source_files(),
        }
        self.collect_diagnostics(ctx, source_file, false /*concurrent*/, |_, file| {
            file.bind_diagnostics().to_vec()
        })
    }

    // program.go:806
    pub fn get_semantic_diagnostics(
        &'static self,
        ctx: &Context,
        source_file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        let collect = |ctx: &Context, c: &mut Checker, file: P<SourceFile>| {
            self.get_semantic_diagnostics_with_checker(ctx, c, file)
        };
        match source_file {
            // All files: the type-check pass, the one that may move files between checkers.
            None => {
                // tsrs-only: the CLI's `--noEmit` check frees a leaf's tree and binder output once its diagnostics are
                // collected (fileregions.rs).
                let free_leaves = tsrs_core::phases::time(
                    self.options().diagnostics_enabled(),
                    "Checkers: leaf files",
                    || crate::fileregions::classify(self),
                );
                let collect = |ctx: &Context, c: &mut Checker, file: P<SourceFile>| {
                    let diagnostics = collect(ctx, c, file);
                    if free_leaves && file.is_check_leaf() {
                        crate::fileregions::free(file);
                    }
                    diagnostics
                };
                let diagnostics = self
                    .collect_checker_diagnostics_from_files_ex(ctx, self.files, true, &collect)
                    .concat();
                if free_leaves {
                    crate::fileregions::pass_done();
                }
                filter_and_sort_diagnostics(&diagnostics)
            }
            Some(_) => self.collect_checker_diagnostics(ctx, source_file, collect),
        }
    }

    // program.go:812
    // GetSemanticDiagnosticsForIncremental includes newly discovered globals in each
    // file's cached diagnostics and leaves noEmit filtering to the builder.
    pub fn get_semantic_diagnostics_for_incremental(
        &'static self,
        ctx: &Context,
        source_files: &[P<SourceFile>],
    ) -> FxHashMap<P<SourceFile>, Vec<P<Diagnostic>>> {
        let all_diags =
            self.collect_checker_diagnostics_from_files(ctx, source_files, &|ctx: &Context,
                                                                             c: &mut Checker,
                                                                             file: P<
                SourceFile,
            >| {
                self.get_bind_and_check_diagnostics_with_checker(
                    ctx, c, file, true, /*includeDeferredGlobals*/
                )
            });
        let mut result = FxHashMap::default();
        result.extend(
            all_diags
                .into_iter()
                .enumerate()
                .map(|(i, diags)| (source_files[i], filter_and_sort_diagnostics(&diags))),
        );
        result
    }

    // program.go:823
    pub fn get_suggestion_diagnostics(
        &'static self,
        ctx: &Context,
        source_file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        self.collect_checker_diagnostics(ctx, source_file, |ctx, c, file| {
            self.get_suggestion_diagnostics_with_checker(ctx, c, file)
        })
    }

    // program.go:827
    pub fn get_program_diagnostics(&'static self) -> Vec<P<Diagnostic>> {
        let mut all = self.program_diagnostics.clone();
        all.extend_from_slice(&self.content_mapper_diagnostics);
        all.extend_from_slice(&self.content_mapper_option_diagnostics);
        all.extend(
            self.include_processor.get_diagnostics(self).lock().unwrap().get_global_diagnostics(),
        );
        sort_and_deduplicate_diagnostics(&all)
    }

    // program.go:836
    fn collect_content_mapper_option_diagnostics(&mut self) {
        let Some(project) = self.content_mapper_project() else {
            return;
        };
        let option_diagnostics = project.diagnostics();
        let config = self.opts.config;
        self.content_mapper_option_diagnostics = option_diagnostics
            .iter()
            .map(|diagnostic| {
                let (file, loc) = tsoptions::get_content_mapper_option_diagnostic_location(
                    Some(&config),
                    diagnostic.mapper,
                    &diagnostic.path,
                );
                ast::new_external_diagnostic(
                    file,
                    loc,
                    &diagnostic.source,
                    diagnostics::Category::Error,
                    diagnostic.code,
                    &diagnostic.message_text,
                )
            })
            .collect();
    }

    pub fn get_include_processor_diagnostics(
        &'static self,
        source_file: P<SourceFile>,
    ) -> Vec<P<Diagnostic>> {
        if self.skip_type_checking(source_file, false) {
            return Vec::new();
        }
        let diags = self
            .include_processor
            .get_diagnostics(self)
            .lock()
            .unwrap()
            .get_diagnostics_for_file(source_file);
        let (filtered, _) = Self::get_diagnostics_with_preceding_directives(source_file, diags);
        filtered
    }

    pub fn skip_type_checking(&self, source_file: P<SourceFile>, ignore_no_check: bool) -> bool {
        (!ignore_no_check && self.options().no_check.is_true())
            || self.options().skip_lib_check.is_true() && source_file.is_declaration_file.get()
            || self.options().skip_default_lib_check.is_true()
                && self.is_source_file_default_library(source_file.path())
            || self.is_source_from_project_reference(source_file.path())
            || !self.can_include_bind_and_check_diagnostics(source_file)
    }

    fn can_include_bind_and_check_diagnostics(&self, source_file: P<SourceFile>) -> bool {
        if let Some(directive) = source_file.check_js_directive.get()
            && !directive.enabled
        {
            return false;
        }

        if source_file.script_kind.get() == ScriptKind::TS
            || source_file.script_kind.get() == ScriptKind::TSX
        {
            return true;
        }

        let is_js = source_file.script_kind.get() == ScriptKind::JS
            || source_file.script_kind.get() == ScriptKind::JSX;
        let is_check_js = is_js && ast::is_check_js_enabled_for_file(source_file, &self.options());
        let is_plain_js = ast::is_plain_js_file(Some(source_file), self.options().check_js);

        // By default, only type-check .ts, .tsx, plain JS, and checked JS
        // - plain JS: .js files with no // ts-check and checkJs: undefined
        // - check JS: .js files with either // ts-check or checkJs: true
        is_plain_js || is_check_js
    }

    fn verify_compiler_options(&mut self) {
        let options = self.options();

        let source_file = self.opts.config.config_file.as_ref().map(|c| c.source_file);
        let config_file_path = source_file.map(|f| f.file_name().to_string()).unwrap_or_default();

        let compiler_options_property_syntax: Option<P<Node>> =
            tsoptions::for_each_ts_config_prop_array(source_file, "compilerOptions", Some);

        let compiler_options_object_literal_syntax: Option<P<Node>> =
            compiler_options_property_syntax
                .and_then(|p| p.initializer())
                .filter(|init| ast::is_object_literal_expression(*init));

        let mut diags: Vec<P<Diagnostic>> = Vec::new();

        macro_rules! create_option_diagnostic_in_object_literal_syntax {
            ($object_literal:expr_2021, $on_key:expr_2021, $key1:expr_2021, $key2:expr_2021, $message:expr_2021, $args:expr_2021) => {{
                let key2: &str = $key2;
                let keys2: Option<&str> = if key2.is_empty() { None } else { Some(key2) };
                let diag = tsoptions::for_each_property_assignment(
                    $object_literal,
                    $key1,
                    |property: P<Node>| {
                        Some(tsoptions::create_diagnostic_for_node_in_source_file(
                            source_file.unwrap(),
                            if $on_key {
                                property.name().unwrap()
                            } else {
                                property.initializer().unwrap()
                            },
                            $message,
                            $args,
                        ))
                    },
                    keys2,
                );
                if let Some(diag) = diag {
                    diags.push(diag);
                }
                diag
            }};
        }

        let create_compiler_options_diagnostic =
            |diags: &mut Vec<P<Diagnostic>>, message: &'static Message, args: &[&dyn Display]| {
                let diag = if let Some(compiler_options_property) = compiler_options_property_syntax
                {
                    tsoptions::create_diagnostic_for_node_in_source_file(
                        source_file.unwrap(),
                        compiler_options_property.name().unwrap(),
                        message,
                        args,
                    )
                } else {
                    new_compiler_diagnostic(message, args)
                };
                diags.push(diag);
                diag
            };

        macro_rules! create_diagnostic_for_option {
            ($on_key:expr_2021, $option1:expr_2021, $option2:expr_2021, $message:expr_2021, $args:expr_2021) => {{
                let diag = create_option_diagnostic_in_object_literal_syntax!(
                    compiler_options_object_literal_syntax,
                    $on_key,
                    $option1,
                    $option2,
                    $message,
                    $args
                );
                match diag {
                    Some(d) => d,
                    None => create_compiler_options_diagnostic(&mut diags, $message, $args),
                }
            }};
        }

        macro_rules! create_diagnostic_for_option_name {
            ($message:expr_2021, $option1:expr_2021, $option2:expr_2021) => {{
                let o1: &str = $option1;
                let o2: &str = $option2;
                create_diagnostic_for_option!(true, o1, o2, $message, &[&o1, &o2]);
            }};
            ($message:expr_2021, $option1:expr_2021, $option2:expr_2021, $($arg:expr_2021),+) => {{
                let o1: &str = $option1;
                let o2: &str = $option2;
                create_diagnostic_for_option!(true, o1, o2, $message, &[&o1, &o2, $(&$arg),+]);
            }};
        }

        macro_rules! create_option_value_diagnostic {
            ($option1:expr_2021, $message:expr_2021, $args:expr_2021) => {{
                create_diagnostic_for_option!(false, $option1, "", $message, $args);
            }};
        }

        macro_rules! create_removed_option_diagnostic {
            ($name:expr_2021, $value:expr_2021, $use_instead:expr_2021) => {{
                let name: &str = $name;
                let value: &str = $value;
                let use_instead: String = $use_instead;
                let diag = if value.is_empty() {
                    create_diagnostic_for_option!(
                        true,
                        name,
                        "",
                        &diagnostics::OPTION_0_HAS_BEEN_REMOVED_PLEASE_REMOVE_IT_FROM_YOUR_CONFIGURATION,
                        &[&name]
                    )
                } else {
                    create_diagnostic_for_option!(
                        false,
                        name,
                        "",
                        &diagnostics::OPTION_0_1_HAS_BEEN_REMOVED_PLEASE_REMOVE_IT_FROM_YOUR_CONFIGURATION,
                        &[&name, &value]
                    )
                };
                if !use_instead.is_empty() {
                    diag.add_message_chain(new_compiler_diagnostic(&diagnostics::USE_0_INSTEAD, &[&use_instead]));
                }
            }};
        }

        // Removed in TS7

        if !options.base_url.is_empty() {
            // BaseUrl will have been turned absolute by this point.
            let mut use_instead = String::new();
            if !config_file_path.is_empty() {
                let mut relative = tspath::get_relative_path_from_file(
                    &config_file_path,
                    &options.base_url,
                    &self.compare_paths_options,
                );
                if !(relative.starts_with("./") || relative.starts_with("../")) {
                    relative = format!("./{relative}");
                }
                let suggestion = tspath::combine_paths(&relative, &["*"]);
                use_instead = format!(
                    "\"paths\": {{\"*\": [{}]}}",
                    tsrs_core::json::marshal_string(&suggestion)
                );
            }
            create_removed_option_diagnostic!("baseUrl", "", use_instead);
        }

        if !options.out_file.is_empty() {
            create_removed_option_diagnostic!("outFile", "", String::new());
        }

        if options.target == ScriptTarget::ES5 {
            create_removed_option_diagnostic!("target", "ES5", String::new());
        }

        if options.module == ModuleKind::AMD {
            create_removed_option_diagnostic!("module", "AMD", String::new());
        }
        if options.module == ModuleKind::SYSTEM {
            create_removed_option_diagnostic!("module", "System", String::new());
        }
        if options.module == ModuleKind::UMD {
            create_removed_option_diagnostic!("module", "UMD", String::new());
        }

        if options.module_resolution == ModuleResolutionKind::CLASSIC {
            create_removed_option_diagnostic!("moduleResolution", "Classic", String::new());
        }

        if options.always_strict.is_false() {
            create_removed_option_diagnostic!("alwaysStrict", "false", String::new());
        }

        if options.es_module_interop.is_false() {
            create_removed_option_diagnostic!("esModuleInterop", "false", String::new());
        }

        if options.allow_synthetic_default_imports.is_false() {
            create_removed_option_diagnostic!(
                "allowSyntheticDefaultImports",
                "false",
                String::new()
            );
        }

        if options.module_resolution == ModuleResolutionKind::NODE10 {
            create_removed_option_diagnostic!("moduleResolution", "node10", String::new());
        }

        if !options.downlevel_iteration.is_unknown() {
            create_removed_option_diagnostic!("downlevelIteration", "", String::new());
        }

        if options.strict_property_initialization.is_true()
            && !options.get_strict_option_value(options.strict_null_checks)
        {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITHOUT_SPECIFYING_OPTION_1,
                "strictPropertyInitialization",
                "strictNullChecks"
            );
        }
        if options.exact_optional_property_types.is_true()
            && !options.get_strict_option_value(options.strict_null_checks)
        {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITHOUT_SPECIFYING_OPTION_1,
                "exactOptionalPropertyTypes",
                "strictNullChecks"
            );
        }

        if options.isolated_declarations.is_true() {
            if options.get_allow_js() {
                create_diagnostic_for_option_name!(
                    &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITH_OPTION_1,
                    "allowJs",
                    "isolatedDeclarations"
                );
            }
            if !options.get_emit_declarations() {
                create_diagnostic_for_option_name!(
                    &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITHOUT_SPECIFYING_OPTION_1_OR_OPTION_2,
                    "isolatedDeclarations",
                    "declaration",
                    "composite"
                );
            }
        }

        if options.inline_source_map.is_true() {
            if options.source_map.is_true() {
                create_diagnostic_for_option_name!(
                    &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITH_OPTION_1,
                    "sourceMap",
                    "inlineSourceMap"
                );
            }
            if !options.map_root.is_empty() {
                create_diagnostic_for_option_name!(
                    &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITH_OPTION_1,
                    "mapRoot",
                    "inlineSourceMap"
                );
            }
        }

        if options.composite.is_true() {
            if options.declaration.is_false() {
                create_diagnostic_for_option_name!(
                    &diagnostics::COMPOSITE_PROJECTS_MAY_NOT_DISABLE_DECLARATION_EMIT,
                    "declaration",
                    ""
                );
            }
            if options.incremental.is_false() {
                create_diagnostic_for_option_name!(
                    &diagnostics::COMPOSITE_PROJECTS_MAY_NOT_DISABLE_INCREMENTAL_COMPILATION,
                    "declaration",
                    ""
                );
            }
        }

        if options.ts_build_info_file.is_empty()
            && options.incremental.is_true()
            && options.config_file_path.is_empty()
        {
            create_compiler_options_diagnostic(
                &mut diags,
                &diagnostics::OPTION_INCREMENTAL_IS_ONLY_VALID_WITH_A_KNOWN_CONFIGURATION_FILE_LIKE_TSCONFIG_JSON_OR_WHEN_TS_BUILD_INFO_FILE_IS_EXPLICITLY_PROVIDED,
                &[],
            );
        }

        self.program_diagnostics.append(&mut diags);
        self.verify_project_references();

        if options.composite.is_true() {
            let mut root_paths: FxHashSet<Path> = FxHashSet::default();
            root_paths.extend(
                self.opts.config.file_names().iter().map(|file_name| self.to_path(file_name)),
            );

            for &file in self.files {
                let mut root_path = file.path();
                if let Some(canonical) = file.canonical_source_file() {
                    root_path = canonical.get().path();
                }
                if source_file_may_be_emitted(file, self, false, false)
                    && !root_paths.contains(root_path)
                {
                    self.add_processing_diagnostic(ProcessingDiagnostic::explaining(IncludeExplainingDiagnostic {
                        file: Some(file.path().clone()),
                        diagnostic_reason: None,
                        message: &diagnostics::FILE_0_IS_NOT_LISTED_WITHIN_THE_FILE_LIST_OF_PROJECT_1_PROJECTS_MUST_LIST_ALL_FILES_OR_USE_AN_INCLUDE_PATTERN,
                        args: vec![file.file_name().to_string(), config_file_path.clone()],
                    }));
                }
            }
        }

        let paths_object_literals: Vec<P<Node>> = {
            // forEachOptionPathsSyntax visits every "paths" property; collect their object literal initializers.
            let mut result = Vec::new();
            tsoptions::for_each_property_assignment(
                compiler_options_object_literal_syntax,
                "paths",
                |path_prop: P<Node>| -> Option<P<Diagnostic>> {
                    result.push(path_prop);
                    None
                },
                None,
            );
            result
        };

        let create_diagnostic_for_option_paths =
            |diags: &mut Vec<P<Diagnostic>>,
             on_key: bool,
             key: &str,
             message: &'static Message,
             args: &[&dyn Display]| {
                let mut diag = None;
                for &path_prop in &paths_object_literals {
                    let initializer = path_prop.initializer().unwrap();
                    if ast::is_object_literal_expression(initializer) {
                        let d = tsoptions::for_each_property_assignment(
                            Some(initializer),
                            key,
                            |property: P<Node>| {
                                Some(tsoptions::create_diagnostic_for_node_in_source_file(
                                    source_file.unwrap(),
                                    if on_key {
                                        property.name().unwrap()
                                    } else {
                                        property.initializer().unwrap()
                                    },
                                    message,
                                    args,
                                ))
                            },
                            None,
                        );
                        if let Some(d) = d {
                            diags.push(d);
                            diag = Some(d);
                            break;
                        }
                    }
                }
                if diag.is_none() {
                    create_compiler_options_diagnostic(diags, message, args);
                }
            };

        let create_diagnostic_for_option_path_key_value =
            |diags: &mut Vec<P<Diagnostic>>,
             key: &str,
             value_index: usize,
             message: &'static Message,
             args: &[&dyn Display]| {
                let mut diag = None;
                for &path_prop in &paths_object_literals {
                    let initializer = path_prop.initializer().unwrap();
                    if ast::is_object_literal_expression(initializer) {
                        let d = tsoptions::for_each_property_assignment(
                            Some(initializer),
                            key,
                            |key_props: P<Node>| {
                                let initializer = key_props.initializer().unwrap();
                                if ast::is_array_literal_expression(initializer) {
                                    let elements = initializer.elements();
                                    if elements.len() > value_index {
                                        return Some(
                                            tsoptions::create_diagnostic_for_node_in_source_file(
                                                source_file.unwrap(),
                                                elements[value_index],
                                                message,
                                                args,
                                            ),
                                        );
                                    }
                                }
                                None
                            },
                            None,
                        );
                        if let Some(d) = d {
                            diags.push(d);
                            diag = Some(d);
                            break;
                        }
                    }
                }
                if diag.is_none() {
                    create_compiler_options_diagnostic(diags, message, args);
                }
            };

        if let Some(paths) = &options.paths {
            let paths_base_path = options.get_paths_base_path(self.get_current_directory());
            for (key, value) in paths.iter() {
                // !!! This code does not handle cases where where the path mappings have the wrong types,
                // as that information is mostly lost during the parsing process.
                if !has_zero_or_one_asterisk_character(key) {
                    create_diagnostic_for_option_paths(
                        &mut diags,
                        true,
                        key,
                        &diagnostics::PATTERN_0_CAN_HAVE_AT_MOST_ONE_ASTERISK_CHARACTER,
                        &[&key],
                    );
                }
                match value {
                    None => create_diagnostic_for_option_paths(
                        &mut diags,
                        false,
                        key,
                        &diagnostics::SUBSTITUTIONS_FOR_PATTERN_0_SHOULD_BE_AN_ARRAY,
                        &[&key],
                    ),
                    Some(value) if value.is_empty() => create_diagnostic_for_option_paths(
                        &mut diags,
                        false,
                        key,
                        &diagnostics::SUBSTITUTIONS_FOR_PATTERN_0_SHOULDN_T_BE_AN_EMPTY_ARRAY,
                        &[&key],
                    ),
                    Some(_) => {}
                }
                for (i, subst) in value.iter().flatten().enumerate() {
                    if !has_zero_or_one_asterisk_character(subst) {
                        create_diagnostic_for_option_path_key_value(
                            &mut diags,
                            key,
                            i,
                            &diagnostics::SUBSTITUTION_0_IN_PATTERN_1_CAN_HAVE_AT_MOST_ONE_ASTERISK_CHARACTER,
                            &[&subst, &key],
                        );
                    }
                    if !tspath::path_is_relative(subst) && !tspath::path_is_absolute(subst) {
                        create_diagnostic_for_option_path_key_value(
                            &mut diags,
                            key,
                            i,
                            &diagnostics::NON_RELATIVE_PATHS_ARE_NOT_ALLOWED_DID_YOU_FORGET_A_LEADING_SLASH,
                            &[],
                        );
                    } else if tspath::is_network_path(&tspath::get_normalized_absolute_path(
                        subst,
                        &paths_base_path,
                    )) {
                        create_diagnostic_for_option_path_key_value(
                            &mut diags,
                            key,
                            i,
                            &diagnostics::NETWORK_PATH_0_IS_NOT_SUPPORTED,
                            &[subst],
                        );
                    }
                }
            }
        }

        if options.source_map.is_false_or_unknown()
            && options.inline_source_map.is_false_or_unknown()
        {
            if options.inline_sources.is_true() {
                create_diagnostic_for_option_name!(
                    &diagnostics::OPTION_0_CAN_ONLY_BE_USED_WHEN_EITHER_OPTION_INLINE_SOURCE_MAP_OR_OPTION_SOURCE_MAP_IS_PROVIDED,
                    "inlineSources",
                    ""
                );
            }
            if !options.source_root.is_empty() {
                create_diagnostic_for_option_name!(
                    &diagnostics::OPTION_0_CAN_ONLY_BE_USED_WHEN_EITHER_OPTION_INLINE_SOURCE_MAP_OR_OPTION_SOURCE_MAP_IS_PROVIDED,
                    "sourceRoot",
                    ""
                );
            }
        }

        if !(options.map_root.is_empty()
            || options.source_map.is_true()
            || options.declaration_map.is_true())
        {
            // Error to specify --mapRoot without --sourcemap
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITHOUT_SPECIFYING_OPTION_1_OR_OPTION_2,
                "mapRoot",
                "sourceMap",
                "declarationMap"
            );
        }

        if !options.declaration_dir.is_empty() && !options.get_emit_declarations() {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITHOUT_SPECIFYING_OPTION_1_OR_OPTION_2,
                "declarationDir",
                "declaration",
                "composite"
            );
        }

        if options.declaration_map.is_true() && !options.get_emit_declarations() {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITHOUT_SPECIFYING_OPTION_1_OR_OPTION_2,
                "declarationMap",
                "declaration",
                "composite"
            );
        }

        if options.lib.is_some() && options.no_lib.is_true() {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITH_OPTION_1,
                "lib",
                "noLib"
            );
        }

        if (options.isolated_modules.is_true() || options.verbatim_module_syntax.is_true())
            && options.preserve_const_enums.is_false()
        {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_PRESERVE_CONST_ENUMS_CANNOT_BE_DISABLED_WHEN_0_IS_ENABLED,
                if options.verbatim_module_syntax.is_true() {
                    "verbatimModuleSyntax"
                } else {
                    "isolatedModules"
                },
                "preserveConstEnums"
            );
        }

        self.program_diagnostics.append(&mut diags);

        if !options.out_dir.is_empty()
            || !options.root_dir.is_empty()
            || !options.source_root.is_empty()
            || !options.map_root.is_empty()
            || (options.get_emit_declarations() && !options.declaration_dir.is_empty())
        {
            // !!! sheetal checkSourceFilesBelongToPath - for root Dir and configFile - explaining why file is in the program
            let dir = self.common_source_directory();
            if !options.out_dir.is_empty()
                && dir.is_empty()
                && self.files.iter().any(|f| tspath::get_root_length(f.file_name()) > 1)
            {
                create_diagnostic_for_option_name!(
                    &diagnostics::CANNOT_FIND_THE_COMMON_SUBDIRECTORY_PATH_FOR_THE_INPUT_FILES,
                    "outDir",
                    ""
                );
            }
        }

        if !options.no_emit.is_true()
            && !options.composite.is_true()
            && options.root_dir.is_empty()
            && !options.config_file_path.is_empty()
            && (!options.out_dir.is_empty()
                || (options.get_emit_declarations() && !options.declaration_dir.is_empty())
                || !options.out_file.is_empty())
        {
            // Check if rootDir inferred changed and issue diagnostic
            let dir = self.common_source_directory().to_string();
            let emitted_files: Vec<String> = self
                .files
                .iter()
                .filter(|f| {
                    !f.is_declaration_file.get()
                        && source_file_may_be_emitted(**f, self, false, false)
                })
                .map(|f| f.file_name().to_string())
                .collect();
            let dir59 = outputpaths::get_computed_common_source_directory(
                &emitted_files,
                self.get_current_directory(),
                self.use_case_sensitive_file_names(),
            );
            if !dir59.is_empty()
                && tspath::get_canonical_file_name(&dir, self.use_case_sensitive_file_names())
                    != tspath::get_canonical_file_name(&dir59, self.use_case_sensitive_file_names())
            {
                // change in layout
                let option1 = if !options.out_file.is_empty() {
                    "outFile"
                } else if !options.out_dir.is_empty() {
                    "outDir"
                } else {
                    "declarationDir"
                };
                let option2 = if options.out_file.is_empty() && !options.out_dir.is_empty() {
                    "declarationDir"
                } else {
                    ""
                };
                let base = tspath::get_base_file_name(&options.config_file_path);
                let relative = tspath::get_relative_path_from_file(
                    &options.config_file_path,
                    &dir59,
                    &self.compare_paths_options,
                );
                let diag = create_diagnostic_for_option!(
                    true,
                    option1,
                    option2,
                    &diagnostics::THE_COMMON_SOURCE_DIRECTORY_OF_0_IS_1_THE_ROOT_DIR_SETTING_MUST_BE_EXPLICITLY_SET_TO_THIS_OR_ANOTHER_PATH_TO_ADJUST_YOUR_OUTPUT_S_FILE_LAYOUT,
                    &[&base, &relative]
                );
                diag.add_message_chain(new_compiler_diagnostic(&diagnostics::VISIT_HTTPS_COLON_SLASH_SLASHAKA_MS_SLASHTS6_FOR_MIGRATION_INFORMATION, &[]));
            }
        }

        if options.check_js.is_true() && !options.get_allow_js() {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITHOUT_SPECIFYING_OPTION_1,
                "checkJs",
                "allowJs"
            );
        }

        if options.emit_declaration_only.is_true() && !options.get_emit_declarations() {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITHOUT_SPECIFYING_OPTION_1_OR_OPTION_2,
                "emitDeclarationOnly",
                "declaration",
                "composite"
            );
        }

        if options.emit_decorator_metadata.is_true()
            && options.experimental_decorators.is_false_or_unknown()
        {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITHOUT_SPECIFYING_OPTION_1,
                "emitDecoratorMetadata",
                "experimentalDecorators"
            );
        }

        let jsx_is_react_jsx =
            options.jsx == JsxEmit::REACT_JSX || options.jsx == JsxEmit::REACT_JSX_DEV;
        if !options.jsx_factory.is_empty() {
            if !options.react_namespace.is_empty() {
                create_diagnostic_for_option_name!(
                    &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITH_OPTION_1,
                    "reactNamespace",
                    "jsxFactory"
                );
            }
            if jsx_is_react_jsx {
                create_diagnostic_for_option_name!(
                    &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WHEN_OPTION_JSX_IS_1,
                    "jsxFactory",
                    options.jsx.string()
                );
            }
            if tsrs_parser::parse_isolated_entity_name(&options.jsx_factory).is_none() {
                create_option_value_diagnostic!(
                    "jsxFactory",
                    &diagnostics::INVALID_VALUE_FOR_JSX_FACTORY_0_IS_NOT_A_VALID_IDENTIFIER_OR_QUALIFIED_NAME,
                    &[&options.jsx_factory]
                );
            }
        } else if !options.react_namespace.is_empty()
            && !tsrs_scanner::is_identifier_text(
                &options.react_namespace,
                tsrs_core::LanguageVariant::Standard,
            )
        {
            create_option_value_diagnostic!(
                "reactNamespace",
                &diagnostics::INVALID_VALUE_FOR_REACT_NAMESPACE_0_IS_NOT_A_VALID_IDENTIFIER,
                &[&options.react_namespace]
            );
        }

        if !options.jsx_fragment_factory.is_empty() {
            if options.jsx_factory.is_empty() {
                create_diagnostic_for_option_name!(
                    &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WITHOUT_SPECIFYING_OPTION_1,
                    "jsxFragmentFactory",
                    "jsxFactory"
                );
            }
            if jsx_is_react_jsx {
                create_diagnostic_for_option_name!(
                    &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WHEN_OPTION_JSX_IS_1,
                    "jsxFragmentFactory",
                    options.jsx.string()
                );
            }
            if tsrs_parser::parse_isolated_entity_name(&options.jsx_fragment_factory).is_none() {
                create_option_value_diagnostic!(
                    "jsxFragmentFactory",
                    &diagnostics::INVALID_VALUE_FOR_JSX_FRAGMENT_FACTORY_0_IS_NOT_A_VALID_IDENTIFIER_OR_QUALIFIED_NAME,
                    &[&options.jsx_fragment_factory]
                );
            }
        }

        if !options.react_namespace.is_empty() && jsx_is_react_jsx {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WHEN_OPTION_JSX_IS_1,
                "reactNamespace",
                options.jsx.string()
            );
        }

        if !options.jsx_import_source.is_empty() && options.jsx == JsxEmit::REACT {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CANNOT_BE_SPECIFIED_WHEN_OPTION_JSX_IS_1,
                "jsxImportSource",
                options.jsx.string()
            );
        }

        let module_kind = options.get_emit_module_kind();

        if options.allow_importing_ts_extensions.is_true()
            && !(options.no_emit.is_true()
                || options.emit_declaration_only.is_true()
                || options.rewrite_relative_import_extensions.is_true())
        {
            create_option_value_diagnostic!(
                "allowImportingTsExtensions",
                &diagnostics::OPTION_ALLOW_IMPORTING_TS_EXTENSIONS_CAN_ONLY_BE_USED_WHEN_ONE_OF_NO_EMIT_EMIT_DECLARATION_ONLY_OR_REWRITE_RELATIVE_IMPORT_EXTENSIONS_IS_SET,
                &[]
            );
        }

        let module_resolution = options.get_module_resolution_kind();
        if options.resolve_package_json_exports.is_true()
            && !module_resolution_supports_package_json_exports_and_imports(module_resolution)
        {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CAN_ONLY_BE_USED_WHEN_MODULE_RESOLUTION_IS_SET_TO_NODE16_NODENEXT_OR_BUNDLER,
                "resolvePackageJsonExports",
                ""
            );
        }
        if options.resolve_package_json_imports.is_true()
            && !module_resolution_supports_package_json_exports_and_imports(module_resolution)
        {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CAN_ONLY_BE_USED_WHEN_MODULE_RESOLUTION_IS_SET_TO_NODE16_NODENEXT_OR_BUNDLER,
                "resolvePackageJsonImports",
                ""
            );
        }
        if options.custom_conditions.is_some()
            && !module_resolution_supports_package_json_exports_and_imports(module_resolution)
        {
            create_diagnostic_for_option_name!(
                &diagnostics::OPTION_0_CAN_ONLY_BE_USED_WHEN_MODULE_RESOLUTION_IS_SET_TO_NODE16_NODENEXT_OR_BUNDLER,
                "customConditions",
                ""
            );
        }

        if module_resolution == ModuleResolutionKind::BUNDLER
            && !emit_module_kind_is_non_node_esm(module_kind)
            && module_kind != ModuleKind::PRESERVE
            && module_kind != ModuleKind::COMMON_JS
        {
            create_option_value_diagnostic!(
                "moduleResolution",
                &diagnostics::OPTION_0_CAN_ONLY_BE_USED_WHEN_MODULE_IS_SET_TO_PRESERVE_COMMONJS_OR_ES2015_OR_LATER,
                &[&"bundler"]
            );
        }

        if ModuleKind::NODE16 <= module_kind
            && module_kind <= ModuleKind::NODE_NEXT
            && !(ModuleResolutionKind::NODE16 <= module_resolution
                && module_resolution <= ModuleResolutionKind::NODE_NEXT)
        {
            let module_kind_name = module_kind.string();
            let module_resolution_name =
                match tsrs_core::MODULE_KIND_TO_MODULE_RESOLUTION_KIND.get(&module_kind) {
                    Some(v) => v.string().to_string(),
                    None => "Node16".to_string(),
                };
            create_option_value_diagnostic!(
                "moduleResolution",
                &diagnostics::OPTION_MODULE_RESOLUTION_MUST_BE_SET_TO_0_OR_LEFT_UNSPECIFIED_WHEN_OPTION_MODULE_IS_SET_TO_1,
                &[&module_resolution_name, &module_kind_name]
            );
        } else if ModuleResolutionKind::NODE16 <= module_resolution
            && module_resolution <= ModuleResolutionKind::NODE_NEXT
            && !(ModuleKind::NODE16 <= module_kind && module_kind <= ModuleKind::NODE_NEXT)
        {
            let module_resolution_name = module_resolution.string();
            create_option_value_diagnostic!(
                "module",
                &diagnostics::OPTION_MODULE_MUST_BE_SET_TO_0_WHEN_OPTION_MODULE_RESOLUTION_IS_SET_TO_1,
                &[&module_resolution_name, &module_resolution_name]
            );
        }

        self.program_diagnostics.append(&mut diags);

        // !!! The below needs filesByName, which is not equivalent to p.filesByPath.

        // If the emit is enabled make sure that every output file is unique and not overwriting any of the input files
        if !options.no_emit.is_true() && !options.suppress_output_path_check.is_true() {
            let mut emit_files_seen: FxHashSet<String> = FxHashSet::default();
            let mut emit_file_names: Vec<String> = Vec::new();
            let external_output_source_files: Vec<P<SourceFile>> = self
                .source_files()
                .iter()
                .copied()
                .filter(|source_file| {
                    !source_file.content_mapper().is_empty()
                        && !self.is_source_file_from_external_library(*source_file)
                        && self.get_project_reference_from_source(source_file.path()).is_none()
                })
                .collect();
            tsoptions::outputpaths::for_each_emitted_file(
                &*self,
                &options,
                |paths, _| {
                    emit_file_names.push(paths.js_file_path().to_string());
                    emit_file_names.push(paths.source_map_file_path().to_string());
                    emit_file_names.push(paths.declaration_file_path().to_string());
                    emit_file_names.push(paths.declaration_map_path().to_string());
                    emit_file_names.push(paths.external_output_file_path().to_string());
                    false
                },
                &self.get_source_files_to_emit(None, false, false),
                &external_output_source_files,
                false,
            );
            emit_file_names.push(self.opts.config.get_build_info_file_name());

            // Verify that all the emit files are unique and don't overwrite input files
            for emit_file_name in &emit_file_names {
                if emit_file_name.is_empty() {
                    continue;
                }
                let emit_file_path = self.to_path(emit_file_name);
                // Report error if the output overwrites input file
                if self.files_by_path().contains_key(&emit_file_path) {
                    let diag = new_compiler_diagnostic(
                        &diagnostics::CANNOT_WRITE_FILE_0_BECAUSE_IT_WOULD_OVERWRITE_INPUT_FILE,
                        &[emit_file_name],
                    );
                    if config_file_path.is_empty() {
                        // The program is from either an inferred project or an external project
                        diag.add_message_chain(new_compiler_diagnostic(
                            &diagnostics::ADDING_A_TSCONFIG_JSON_FILE_WILL_HELP_ORGANIZE_PROJECTS_THAT_CONTAIN_BOTH_TYPE_SCRIPT_AND_JAVA_SCRIPT_FILES_LEARN_MORE_AT_HTTPS_COLON_SLASH_SLASHAKA_MS_SLASHTSCONFIG,
                            &[],
                        ));
                    }
                    self.block_emitting_of_file(emit_file_name, diag);
                }

                let emit_file_key = if self.host().fs().use_case_sensitive_file_names() {
                    emit_file_path.as_str().to_string()
                } else {
                    tspath::to_file_name_lower_case(&emit_file_path)
                };

                // Report error if multiple files write into same file
                if !emit_files_seen.insert(emit_file_key) {
                    // Already seen the same emit file - report error
                    self.block_emitting_of_file(
                        emit_file_name,
                        new_compiler_diagnostic(&diagnostics::CANNOT_WRITE_FILE_0_BECAUSE_IT_WOULD_BE_OVERWRITTEN_BY_MULTIPLE_INPUT_FILES, &[emit_file_name]),
                    );
                }
            }
        }
    }

    fn get_source_files_to_emit(
        &self,
        target_source_files: Option<&[P<SourceFile>]>,
        force_dts_emit: bool,
        force_js_emit: bool,
    ) -> Vec<P<SourceFile>> {
        crate::emitter::get_source_files_to_emit(
            self,
            target_source_files,
            force_dts_emit,
            force_js_emit,
        )
    }

    fn block_emitting_of_file(&mut self, emit_file_name: &str, diag: P<Diagnostic>) {
        let path = self.to_path(emit_file_name);
        self.has_emit_blocking_diagnostics.insert(path);
        self.program_diagnostics.push(diag);
    }

    pub fn is_emit_blocked(&self, emit_file_name: &str) -> bool {
        self.has_emit_blocking_diagnostics.contains(&self.to_path(emit_file_name))
    }

    // program.go:1402
    fn verify_project_references(&mut self) {
        let build_info_file_name = if self.options().suppress_output_path_check.is_true() {
            String::new()
        } else {
            self.opts.config.get_build_info_file_name()
        };
        let mut program_diagnostics = Vec::new();
        let mut emit_blocking = Vec::new();
        let mut create_diagnostic_for_reference =
            |config: P<ParsedCommandLine>,
             index: usize,
             message: &'static Message,
             args: &[&dyn Display]| {
                let diag =
                    tsoptions::create_diagnostic_at_reference_syntax(&config, index, message, args)
                        .unwrap_or_else(|| new_compiler_diagnostic(message, args));
                program_diagnostics.push(diag);
            };

        self.project_reference_file_mapper.range_resolved_project_reference(|_path, config, parent, index| {
            let ref_ = &parent.project_references()[index];
            // !!! Deprecated in 5.0 and removed since 5.5
            // verifyRemovedProjectReference(ref, parent, index);
            let Some(config) = config else {
                create_diagnostic_for_reference(parent, index, &diagnostics::FILE_0_NOT_FOUND, &[&ref_.path]);
                return true;
            };
            let ref_options = config.compiler_options().unwrap();
            if (!ref_options.composite.is_true() || ref_options.no_emit.is_true())
                && !parent.file_names().is_empty() {
                    if !ref_options.composite.is_true() {
                        create_diagnostic_for_reference(parent, index, &diagnostics::REFERENCED_PROJECT_0_MUST_HAVE_SETTING_COMPOSITE_COLON_TRUE, &[&ref_.path]);
                    }
                    if ref_options.no_emit.is_true() {
                        create_diagnostic_for_reference(parent, index, &diagnostics::REFERENCED_PROJECT_0_MAY_NOT_DISABLE_EMIT, &[&ref_.path]);
                    }
                }
            if !build_info_file_name.is_empty() && build_info_file_name == config.get_build_info_file_name() {
                create_diagnostic_for_reference(
                    parent,
                    index,
                    &diagnostics::CANNOT_WRITE_FILE_0_BECAUSE_IT_WILL_OVERWRITE_TSBUILDINFO_FILE_GENERATED_BY_REFERENCED_PROJECT_1,
                    &[&build_info_file_name, &ref_.path],
                );
                emit_blocking.push(build_info_file_name.clone());
            }
            true
        });
        self.program_diagnostics.extend(program_diagnostics);
        for file_name in emit_blocking {
            let path = self.to_path(&file_name);
            self.has_emit_blocking_diagnostics.insert(path);
        }
    }

    // program.go:1463
    pub fn get_global_diagnostics(&'static self, _ctx: &Context) -> Vec<P<Diagnostic>> {
        if self.files.is_empty() {
            return Vec::new();
        }
        if let Some(pool) = self.compiler_checker_pool() {
            return pool.get_global_diagnostics();
        }
        // For external pools (project system), global diagnostics are collected
        // incrementally as checkers are used, not via a bulk query.
        Vec::new()
    }

    // program.go:1475. Go's `collectDiagnostics(ctx, sourceFile, true /*concurrent*/, p.getDeclarationDiagnosticsForFile)`
    // runs one task per file, each taking the file's checker from the pool; with the built-in pool the files run
    // grouped by checker on the checker threads (in file order within a checker), like the other checker-backed
    // diagnostics. With an external pool each file takes its checker from the pool, as in Go.
    pub fn get_declaration_diagnostics(
        &'static self,
        ctx: &Context,
        source_file: Option<P<SourceFile>>,
    ) -> Vec<P<Diagnostic>> {
        let result = match (source_file, self.compiler_checker_pool()) {
            (Some(file), _) => self.get_declaration_diagnostics_for_file(ctx, None, file),
            (None, Some(pool)) => {
                let diagnostics: Vec<Mutex<Vec<P<Diagnostic>>>> =
                    self.files.iter().map(|_| Mutex::new(Vec::new())).collect();
                pool.for_each_checker_group_do(
                    self.files,
                    self.single_threaded(),
                    |c, file_index, file| {
                        *diagnostics[file_index].lock().unwrap() =
                            self.get_declaration_diagnostics_for_file(ctx, Some(c), file);
                    },
                );
                diagnostics.into_iter().flat_map(|d| d.into_inner().unwrap()).collect()
            }
            (None, None) => {
                self.collect_diagnostics_from_files(
                    ctx,
                    self.files,
                    true, /*concurrent*/
                    &|ctx: &Context, file: P<SourceFile>| {
                        self.get_declaration_diagnostics_for_file(ctx, None, file)
                    },
                )
                .concat()
            }
        };
        filter_and_sort_diagnostics(&result)
    }

    // program.go:1626. `c` is the file's checker when the caller already holds it (Go `newEmitHost` takes it from
    // the pool); with `None` it is taken here.
    fn get_declaration_diagnostics_for_file(
        &'static self,
        ctx: &Context,
        c: Option<&mut Checker>,
        source_file: P<SourceFile>,
    ) -> Vec<P<Diagnostic>> {
        crate::fileregions::assert_not_freed(source_file);
        if source_file.is_declaration_file.get() {
            return Vec::new();
        }

        if let Some(cached) = self.declaration_diagnostic_cache.lock().unwrap().get(&source_file) {
            return cached.clone();
        }

        let mut guard;
        let c = if let Some(c) = c {
            c
        } else {
            guard = self.get_type_checker_for_file(ctx, source_file);
            &mut *guard
        };
        // The declaration transform runs in a scratch region of its own, freed once its diagnostics are collected
        // (notes/mem-emit-regions.md; the diagnostics themselves escape it).
        let region = tsrs_core::arena::Region::new_scratch(64 << 10);
        let diagnostics = {
            let _scratch = region.enter_scratch();
            let emit_context = tsrs_printer::new_scratch_emit_context();
            let emit_resolver = c.new_emit_resolver(emit_context);
            let checker_slot = P::new(tsrs_checker::CheckerSlot::default());
            let host = crate::emithost::new_emit_host(self, emit_resolver, checker_slot);
            checker_slot
                .lend(c, || crate::emitter::get_declaration_diagnostics(host, self, source_file))
        };
        c.forget_scratch_keyed_caches();
        drop(region);
        let mut cache = self.declaration_diagnostic_cache.lock().unwrap();
        if !cache.contains_key(&source_file) {
            // The diagnostics live in the checker's region (the language server's pool keeps it while the cache does).
            for &d in &diagnostics {
                tsrs_ast::note_diagnostic_escape(d);
            }
        }
        cache.entry(source_file).or_insert(diagnostics).clone()
    }

    // program.go:1488
    fn get_semantic_diagnostics_with_checker(
        &'static self,
        ctx: &Context,
        c: &mut Checker,
        source_file: P<SourceFile>,
    ) -> Vec<P<Diagnostic>> {
        let mut result = filter_no_emit_semantic_diagnostics(
            self.get_bind_and_check_diagnostics_with_checker(
                ctx,
                c,
                source_file,
                false, /*includeDeferredGlobals*/
            ),
            &self.options(),
        );
        result.extend(self.get_include_processor_diagnostics(source_file));
        result
    }

    // getBindAndCheckDiagnosticsWithChecker gets semantic diagnostics for a single file using a
    // caller-provided checker, including bind diagnostics, checker diagnostics, and handling
    // of @ts-ignore/@ts-expect-error directives.
    // program.go:1498
    fn get_bind_and_check_diagnostics_with_checker(
        &self,
        ctx: &Context,
        file_checker: &mut Checker,
        source_file: P<SourceFile>,
        include_deferred_globals: bool,
    ) -> Vec<P<Diagnostic>> {
        let compiler_options = self.options();
        if self.skip_type_checking(source_file, false) {
            return Vec::new();
        }
        let mut previous_globals = Vec::new();
        if include_deferred_globals {
            previous_globals = file_checker.get_global_diagnostics();
        }

        // Checker creation forces binding, so bind diagnostics will be populated.
        let mut diags: Vec<P<Diagnostic>> = source_file.bind_diagnostics().to_vec();
        diags.extend(file_checker.get_diagnostics_exported(ctx, source_file));

        if include_deferred_globals {
            if file_checker.was_canceled() {
                return Vec::new();
            }
            let current_globals = file_checker.get_global_diagnostics();
            if current_globals.len() > previous_globals.len() {
                diags.extend(current_globals.into_iter().filter(|&diagnostic| {
                    previous_globals
                        .binary_search_by(|d| compare_diagnostics(*d, diagnostic))
                        .is_err()
                }));
            }
        }

        let is_plain_js = ast::is_plain_js_file(Some(source_file), compiler_options.check_js);
        if is_plain_js {
            return diags.into_iter().filter(|d| plain_js_errors().contains(&d.code())).collect();
        }

        let is_js = source_file.script_kind.get() == ScriptKind::JS
            || source_file.script_kind.get() == ScriptKind::JSX;
        let is_check_js =
            is_js && ast::is_check_js_enabled_for_file(source_file, &compiler_options);
        if is_check_js {
            diags.extend_from_slice(source_file.jsdoc_diagnostics());
        }

        let (mut filtered, directives_by_line) =
            Self::get_diagnostics_with_preceding_directives(source_file, diags);
        if let Some(directives_by_line) = directives_by_line {
            // Go iterates this map in random order; the result is sorted by the caller.
            // every caller sorts and deduplicates (filter_and_sort_diagnostics); each directive has its own location
            filtered.extend(directives_by_line.values().filter_map(|directive| {
                // Above we changed all used directive kinds to @ts-ignore, so any @ts-expect-error directives that
                // remain are unused and thus errors.
                if directive.kind == CommentDirectiveKind::ExpectError {
                    Some(new_diagnostic(
                        Some(source_file),
                        directive.loc,
                        &diagnostics::UNUSED_TS_EXPECT_ERROR_DIRECTIVE,
                        &[],
                    ))
                } else {
                    None
                }
            }));
        }
        apply_content_mapper_diagnostic_directives(source_file, filtered)
    }

    // program.go:1642
    fn get_suggestion_diagnostics_with_checker(
        &self,
        ctx: &Context,
        file_checker: &mut Checker,
        source_file: P<SourceFile>,
    ) -> Vec<P<Diagnostic>> {
        if self.skip_type_checking(source_file, false) {
            return Vec::new();
        }
        file_checker.get_suggestion_diagnostics(ctx, source_file)
    }

    fn get_diagnostics_with_preceding_directives(
        source_file: P<SourceFile>,
        diags: Vec<P<Diagnostic>>,
    ) -> (Vec<P<Diagnostic>>, Option<FxHashMap<usize, ast::CommentDirective>>) {
        let comment_directives = source_file.comment_directives.get();
        if comment_directives.is_empty() {
            return (diags, None);
        }
        // Build map of directives by line number
        let mut directives_by_line: FxHashMap<usize, ast::CommentDirective> = FxHashMap::default();
        directives_by_line.extend(comment_directives.iter().map(|directive| {
            let line = tsrs_scanner::get_ecma_line_of_position(
                &*source_file,
                directive.loc.pos().expect_source("comment directives have source positions"),
            );
            (line as usize, *directive)
        }));
        let line_starts = tsrs_scanner::get_ecma_line_starts(&*source_file);
        let text = source_file.text();
        let mut filtered = Vec::with_capacity(diags.len());
        for diagnostic in diags {
            let mut ignore_diagnostic = false;
            if diagnostic.file() != Some(source_file) {
                filtered.push(diagnostic);
                continue;
            }
            let Some(diagnostic_pos) = diagnostic.pos() else {
                filtered.push(diagnostic);
                continue;
            };
            let mut line =
                tsrs_scanner::compute_line_of_position(line_starts, diagnostic_pos).checked_sub(1);
            while let Some(current_line) = line {
                // If line contains a @ts-ignore or @ts-expect-error directive, ignore this diagnostic and change
                // the directive kind to @ts-ignore to indicate it was used.
                if let Some(directive) = directives_by_line.get_mut(&current_line) {
                    ignore_diagnostic = true;
                    directive.kind = CommentDirectiveKind::Ignore;
                    break;
                }
                // Stop searching backwards when we encounter a line that isn't blank or a comment.
                if !is_comment_or_blank_line(text, line_starts[current_line] as usize) {
                    break;
                }
                line = current_line.checked_sub(1);
            }
            if !ignore_diagnostic {
                filtered.push(diagnostic);
            }
        }
        (filtered, Some(directives_by_line))
    }

    pub fn line_count(&self) -> usize {
        // Go builds (and keeps) every file's line map here; counting the line starts gives the same number without
        // keeping 4 bytes per line of the program at the end of the run.
        if self.single_threaded() {
            return self.files.iter().map(|f| f.ecma_line_count()).sum();
        }
        worker_pool().install(|| self.files.par_iter().map(|f| f.ecma_line_count()).sum())
    }

    pub fn identifier_count(&self) -> usize {
        self.files.iter().map(|f| f.identifier_count.get()).sum()
    }

    pub fn symbol_count(&'static self) -> usize {
        let count: usize = self.files.iter().map(|f| f.symbol_count.get()).sum();
        let val =
            std::sync::atomic::AtomicUsize::new(count + self.retired_checkers(|r| r.symbol_count));
        self.for_each_checker_parallel(|_, c| {
            val.fetch_add(c.symbol_count as usize, std::sync::atomic::Ordering::Relaxed);
        });
        val.into_inner()
    }

    pub fn type_count(&'static self) -> usize {
        let val = std::sync::atomic::AtomicUsize::new(self.retired_checkers(|r| r.type_count));
        self.for_each_checker_parallel(|_, c| {
            val.fetch_add(c.type_count as usize, std::sync::atomic::Ordering::Relaxed);
        });
        val.into_inner()
    }

    pub fn instantiation_count(&'static self) -> usize {
        let val =
            std::sync::atomic::AtomicUsize::new(self.retired_checkers(|r| r.instantiation_count));
        self.for_each_checker_parallel(|_, c| {
            val.fetch_add(
                c.total_instantiation_count as usize,
                std::sync::atomic::Ordering::Relaxed,
            );
        });
        val.into_inner()
    }

    pub fn lazy_member_stats(&'static self) -> tsrs_core::lazymembers::LazyMemberStats {
        let mut retired = tsrs_core::lazymembers::LazyMemberStats::default();
        if let Some(pool) = self.compiler_checker_pool() {
            retired.add(&pool.state().retired.lock().unwrap().lazy_member_stats);
        }
        let total = std::sync::Mutex::new(retired);
        self.for_each_checker_parallel(|_, c| {
            total.lock().unwrap().add(&c.lazy_member_stats);
        });
        total.into_inner().unwrap()
    }

    // tsrs-only: a counter of the checkers the type-check pass retired (checkerpool.rs `RetiredCheckers`).
    fn retired_checkers(&self, f: impl Fn(&crate::checkerpool::RetiredCheckers) -> u64) -> usize {
        self.compiler_checker_pool()
            .map_or(0, |pool| f(&pool.state().retired.lock().unwrap()) as usize)
    }

    /// Go returns the struct by value (sharing its strings); a reference here, so the hot callers (module mode
    /// lookups during checking) do not copy two strings.
    pub fn get_source_file_meta_data(&self, path: &Path) -> &SourceFileMetaData {
        static DEFAULT: std::sync::LazyLock<SourceFileMetaData> =
            std::sync::LazyLock::new(SourceFileMetaData::default);
        self.source_file_meta_datas.get(path).unwrap_or(&DEFAULT)
    }

    pub fn get_emit_module_format_of_file(&self, source_file: P<SourceFile>) -> ModuleKind {
        ast::get_emit_module_format_of_file_worker(
            source_file.file_name(),
            &self
                .project_reference_file_mapper
                .get_compiler_options_for_file(source_file.file_name(), source_file.path()),
            self.get_source_file_meta_data(source_file.path()),
        )
    }

    pub(crate) fn get_emit_syntax_for_usage_location(
        &self,
        source_file: P<SourceFile>,
        location: P<Node>,
    ) -> ResolutionMode {
        get_emit_syntax_for_usage_location_worker(
            source_file.file_name(),
            self.get_source_file_meta_data(source_file.path()),
            location,
            &self
                .project_reference_file_mapper
                .get_compiler_options_for_file(source_file.file_name(), source_file.path()),
        )
    }

    pub fn get_implied_node_format_for_emit(&self, source_file: P<SourceFile>) -> ResolutionMode {
        ast::get_implied_node_format_for_emit_worker(
            source_file.file_name(),
            self.project_reference_file_mapper
                .get_compiler_options_for_file(source_file.file_name(), source_file.path())
                .get_emit_module_kind(),
            self.get_source_file_meta_data(source_file.path()),
        )
    }

    pub fn get_mode_for_usage_location(
        &self,
        source_file: P<SourceFile>,
        location: P<Node>,
    ) -> ResolutionMode {
        get_mode_for_usage_location(
            source_file.file_name(),
            self.get_source_file_meta_data(source_file.path()),
            location,
            Some(
                &self
                    .project_reference_file_mapper
                    .get_compiler_options_for_file(source_file.file_name(), source_file.path()),
            ),
        )
    }

    pub fn get_mode_for_resolution_at_index(
        &self,
        source_file: P<SourceFile>,
        index: usize,
    ) -> ResolutionMode {
        let imports = source_file.imports();
        if index < imports.len() {
            return self.get_mode_for_usage_location(source_file, imports[index]);
        }
        let index = index - imports.len();
        if let Some(&augmentation) = source_file
            .module_augmentations
            .get()
            .iter()
            .filter(|augmentation| augmentation.kind() == Kind::StringLiteral)
            .nth(index)
        {
            return self.get_mode_for_usage_location(source_file, augmentation);
        }
        panic!("resolution index out of range");
    }

    pub fn get_default_resolution_mode_for_file(
        &self,
        source_file: P<SourceFile>,
    ) -> ResolutionMode {
        get_default_resolution_mode_for_file(
            source_file.file_name(),
            self.get_source_file_meta_data(source_file.path()),
            &self
                .project_reference_file_mapper
                .get_compiler_options_for_file(source_file.file_name(), source_file.path()),
        )
    }

    pub fn is_source_file_default_library(&self, path: &Path) -> bool {
        self.lib_files.contains_key(path)
    }

    pub fn is_global_typings_file(&self, file_name: &str) -> bool {
        if !tspath::is_declaration_file_name(file_name) {
            return false;
        }
        tspath::contains_path(
            self.get_global_typings_cache_location(),
            file_name,
            &self.compare_paths_options,
        )
    }

    pub fn get_default_lib_file(&self, path: &Path) -> Option<P<LibFile>> {
        self.lib_files.get(path).copied()
    }

    pub fn common_source_directory(&self) -> &str {
        self.common_source_directory.get_or_init(|| {
            outputpaths::get_common_source_directory(
                &self.options(),
                || {
                    self.files
                        .iter()
                        .filter(|f| {
                            source_file_may_be_emitted(
                                **f, self, false, /*forceDtsEmit*/
                                false, /*forceJsEmit*/
                            ) && !f.is_declaration_file.get()
                        })
                        .map(|f| f.file_name().to_string())
                        .collect()
                },
                self.get_current_directory(),
                self.use_case_sensitive_file_names(),
                Some(&mut |source_files: &[String], root_directory: &str| {
                    self.check_source_files_belong_to_path(source_files, root_directory)
                }),
            )
        })
    }

    fn check_source_files_belong_to_path(
        &self,
        source_files: &[String],
        root_directory: &str,
    ) -> bool {
        let mut all_files_belong_to_path = true;
        // The path comparisons are independent (vscode: 10k files, ~1 us each); the diagnostics stay in file order.
        let outside: Vec<bool> = if self.single_threaded() {
            source_files
                .iter()
                .map(|file| {
                    !tspath::contains_path(root_directory, file, &self.compare_paths_options)
                })
                .collect()
        } else {
            worker_pool().install(|| {
                source_files
                    .par_iter()
                    .map(|file| {
                        !tspath::contains_path(root_directory, file, &self.compare_paths_options)
                    })
                    .collect()
            })
        };
        for (file, &outside) in source_files.iter().zip(&outside) {
            if outside {
                // Only the diagnostic needs the canonical absolute path (two allocations per file otherwise).
                let absolute_source_file_path = tspath::get_canonical_file_name(
                    &tspath::get_normalized_absolute_path(file, self.get_current_directory()),
                    self.use_case_sensitive_file_names(),
                );
                self.add_processing_diagnostic(ProcessingDiagnostic::explaining(IncludeExplainingDiagnostic {
                    file: Some(Path::from(absolute_source_file_path)),
                    diagnostic_reason: None,
                    message: &diagnostics::FILE_0_IS_NOT_UNDER_ROOT_DIR_1_ROOT_DIR_IS_EXPECTED_TO_CONTAIN_ALL_SOURCE_FILES,
                    args: vec![file.clone(), root_directory.to_string()],
                }));
                all_files_belong_to_path = false;
            }
        }
        all_files_belong_to_path
    }

    fn to_path(&self, filename: &str) -> Path {
        tspath::to_path(
            filename,
            self.get_current_directory(),
            self.use_case_sensitive_file_names(),
        )
    }

    pub fn get_source_file(&self, filename: &str) -> Option<P<SourceFile>> {
        let path = self.to_path(filename);
        self.get_source_file_by_path(&path)
    }

    pub(crate) fn get_source_file_for_resolved_module(
        &self,
        file_name: &str,
    ) -> Option<P<SourceFile>> {
        let file = self.get_source_file(file_name);
        if file.is_none() {
            let filename = self.get_parse_file_redirect(file_name);
            if !filename.is_empty() {
                return self.get_source_file(&filename);
            }
        }
        file
    }

    pub fn files_by_path(&self) -> &FxHashMap<Path, P<SourceFile>> {
        &self.files_by_path
    }

    pub fn get_source_file_by_path(&self, path: &Path) -> Option<P<SourceFile>> {
        self.files_by_path.get(path).copied()
    }

    pub(crate) fn get_source_file_by_path_str(&self, path: &str) -> Option<P<SourceFile>> {
        self.files_by_path.get(path).copied()
    }

    // program.go:2103
    pub fn has_same_file_names(&self, other: &Program) -> bool {
        // checks for casing differences on case-insensitive file systems
        self.files_by_path.len() == other.files_by_path.len()
            && self.files_by_path.iter().all(|(path, a)| {
                other.files_by_path.get(path).is_some_and(|b| a.file_name() == b.file_name())
            })
            && self.redirect_files_by_path.len() == other.redirect_files_by_path.len()
            && self.redirect_files_by_path.iter().all(|(path, a)| {
                other.redirect_files_by_path.get(path).is_some_and(|b| a.file_name == b.file_name)
            })
    }

    // program.go:2112
    pub fn get_source_files(&self) -> &'static [P<SourceFile>] {
        self.files
    }

    // Testing only
    pub fn get_include_reasons(&self) -> &FxHashMap<Path, Vec<P<FileIncludeReason>>> {
        &self.file_include_data.file_include_reasons
    }

    // Testing only
    pub fn is_missing_path(&self, path: &Path) -> bool {
        self.missing_files.iter().any(|missing_path| self.to_path(missing_path) == *path)
    }

    pub fn explain_files(&self, w: &mut dyn Write) {
        let to_relative_file_name = |file_name: &str| {
            tspath::get_relative_path_from_directory(
                self.get_current_directory(),
                file_name,
                &self.compare_paths_options,
            )
        };
        let mut files_explained = 0usize;
        let mut explain_file = |file_name: &str, path: &Path, files_explained: &mut usize| {
            let _ = writeln!(w, "{}", to_relative_file_name(file_name));
            for &reason in self
                .file_include_data
                .file_include_reasons
                .get(path)
                .map_or(&[] as &[P<FileIncludeReason>], std::vec::Vec::as_slice)
            {
                let _ = writeln!(
                    w,
                    "   {}",
                    file_include::to_diagnostic(reason, self, true).localize()
                );
            }
            for diag in self
                .include_processor
                .explain_redirect_and_implied_format(self, path, &to_relative_file_name)
                .unwrap_or_default()
            {
                let _ = writeln!(w, "   {}", diag.localize());
            }
            *files_explained += 1;
        };

        let mut redirect_files: Vec<&crate::fileloader::RedirectsFile> =
            self.redirect_files_by_path.values().collect();
        redirect_files.sort_by_key(|r| r.index);

        let files = self.get_source_files();
        let mut source_file_index = 0;

        for redirect_file in &redirect_files {
            // Explain all sourceFiles till we reach this redirectFile index
            while files_explained < redirect_file.index {
                let f = files[source_file_index];
                explain_file(f.file_name(), f.path(), &mut files_explained);
                source_file_index += 1;
            }
            explain_file(&redirect_file.file_name, &redirect_file.path, &mut files_explained);
        }

        // Explain any remaining sourceFiles
        while files_explained < files.len() + redirect_files.len() {
            let f = files[source_file_index];
            explain_file(f.file_name(), f.path(), &mut files_explained);
            source_file_index += 1;
        }
    }

    pub fn get_lib_file_from_reference(&self, ref_: P<FileReference>) -> Option<P<SourceFile>> {
        let path = tsoptions::get_lib_file_name(&ref_.file_name)?;
        self.files_by_path.get(&Path::from(path)).copied()
    }

    pub fn get_resolved_type_reference_directive_from_type_reference_directive(
        &self,
        type_ref: P<FileReference>,
        source_file: P<SourceFile>,
    ) -> Option<P<ResolvedTypeReferenceDirective>> {
        self.get_resolved_type_reference_directive(
            source_file,
            &type_ref.file_name,
            self.get_mode_for_type_reference_directive_in_file(type_ref, source_file),
        )
    }

    pub fn get_resolved_type_reference_directive(
        &self,
        file: P<SourceFile>,
        type_directive_name: &str,
        mode: ResolutionMode,
    ) -> Option<P<ResolvedTypeReferenceDirective>> {
        let resolutions = self.type_resolutions_in_file.get(file.path())?;
        resolutions
            .iter()
            .find(|(k, _)| k.name == type_directive_name && k.mode == mode)
            .map(|(_, v)| *v)
    }

    pub fn get_resolved_type_reference_directives(
        &self,
    ) -> &FxHashMap<Path, ModeAwareCache<P<ResolvedTypeReferenceDirective>>> {
        &self.type_resolutions_in_file
    }

    fn get_mode_for_type_reference_directive_in_file(
        &self,
        ref_: P<FileReference>,
        source_file: P<SourceFile>,
    ) -> ResolutionMode {
        if ref_.resolution_mode != ModuleKind::NONE {
            return ref_.resolution_mode;
        }
        self.get_default_resolution_mode_for_file(source_file)
    }

    pub fn is_source_file_from_external_library(&self, file: P<SourceFile>) -> bool {
        self.source_files_found_searching_node_modules.contains(file.path())
    }

    pub fn get_jsx_runtime_import_specifier(&self, path: &Path) -> (String, Option<P<Node>>) {
        match self.jsx_runtime_import_specifiers.get(path) {
            Some(result) => (result.module_reference.clone(), Some(result.specifier)),
            None => (String::new(), None),
        }
    }

    pub fn get_import_helpers_import_specifier(&self, path: &Path) -> Option<P<Node>> {
        self.import_helpers_import_specifiers.get(path).copied()
    }

    pub fn source_file_may_be_emitted(
        &self,
        source_file: P<SourceFile>,
        force_dts_emit: bool,
    ) -> bool {
        source_file_may_be_emitted(source_file, self, force_dts_emit, false)
    }

    // program.go:2222
    pub fn resolved_package_names(&self) -> &Set<String> {
        &self.collect_package_names().resolved
    }

    // program.go:2226
    pub fn unresolved_package_names(&self) -> &Set<String> {
        &self.collect_package_names().unresolved
    }

    // program.go:2230
    pub fn deep_import_package_names(&self) -> &Set<String> {
        &self.collect_package_names().deep_import_packages
    }

    // program.go:2234
    fn collect_package_names(&self) -> &PackageNamesInfo {
        self.package_names.get_or_init(|| {
            let resolver = self.new_resolver();
            let mut package_names = PackageNamesInfo {
                resolved: Set::default(),
                unresolved: Set::default(),
                deep_import_packages: Set::default(),
            };
            for &file in self.files {
                if self.is_source_file_default_library(file.path())
                    || self.is_source_file_from_external_library(file)
                    || file.file_name().contains("/node_modules/")
                {
                    // Checking for /node_modules/ is a little imprecise, but ATA treats locally installed typings
                    // as root files, which would not pass IsSourceFileFromExternalLibrary.
                    continue;
                }
                'imports: for &imp in file.imports() {
                    if imp.parent().is_some_and(ast::is_source_phase_import)
                        || tspath::is_external_module_name_relative(imp.text())
                    {
                        continue;
                    }
                    if self.resolved_modules.contains_key(file.path()) {
                        let mode = self.get_mode_for_usage_location(file, imp);
                        if let Some(resolved_module) = self
                            .get_resolved_module(file, imp.text(), mode)
                            .filter(|r| r.is_resolved())
                        {
                            if !resolved_module.is_external_library_import {
                                continue 'imports;
                            }
                            // Priority order for getting package name:
                            // 1. PackageId.Name (requires both name and version in package.json)
                            let mut name = resolved_module.package_id.name.to_string();
                            if name.is_empty() {
                                // 2. GetPackageScopeForPath - get name from package.json in the package directory
                                if let Some(package_scope) = resolver
                                    .get_package_scope_for_path(resolved_module.resolved_file_name)
                                    && package_scope.exists()
                                    && let Some(scope_name) = package_scope
                                        .contents
                                        .and_then(|c| c.get().name.get_value())
                                {
                                    name.clone_from(scope_name);
                                }
                            }
                            if name.is_empty() {
                                // 3. GetPackageNameFromDirectory - extract from node_modules path
                                name = tsrs_modulespecifiers::get_package_name_from_directory(
                                    resolved_module.resolved_file_name,
                                );
                            }
                            // 4. If all fail, don't add empty string
                            if !name.is_empty() {
                                package_names.resolved.add(name.clone());
                                // Detect deep imports: subpath imports in packages without exports.
                                // These are imports like "lodash/fp" where the package has no exports
                                // map, so auto-import can only find them via recursive directory search.
                                let (_, rest) = module::parse_package_name(imp.text());
                                if !rest.is_empty()
                                    && let Some(scope) = resolver.get_package_scope_for_path(
                                        resolved_module.resolved_file_name,
                                    )
                                    && scope.exists()
                                    && !scope.contents.is_some_and(|c| c.exports.is_present())
                                {
                                    package_names.deep_import_packages.add(
                                        module::get_package_name_from_types_package_name(&name),
                                    );
                                }
                            }
                            continue 'imports;
                        }
                    }
                    package_names.unresolved.add(imp.text().to_string());
                }
            }
            Arc::new(package_names)
        })
    }

    pub fn is_lib_file(&self, source_file: P<SourceFile>) -> bool {
        self.lib_files.contains_key(source_file.path())
    }

    // program.go:2297
    pub fn has_ts_file(&self) -> bool {
        *self.has_ts_file.get_or_init(|| {
            self.files.iter().any(|f| tspath::has_implementation_ts_file_extension(f.file_name()))
        })
    }

    pub fn get_symlink_cache(&self) -> P<KnownSymlinks> {
        if let Some(&k) = self.known_symlinks.get() {
            return k;
        }
        // `UpdateProgram` hands the cache to the next program version, which shares `processed`: in the language
        // server it lives in the region that owns `processed` (the full build's), not in this version's.
        let _region = tsrs_core::arena::enter_owner(std::ptr::from_ref::<ProcessedFiles>(
            self.processed,
        ) as usize);
        *self.known_symlinks.get_or_init(|| {
            let resolver = self.new_resolver();
            let known_symlinks = symlinks::new_known_symlink(
                self.get_current_directory(),
                self.use_case_sensitive_file_names(),
            );

            // Resolved modules store realpath information when they're resolved inside node_modules
            if !self.resolved_modules.is_empty() || !self.type_resolutions_in_file.is_empty() {
                known_symlinks.set_symlinks_from_resolutions(
                    |callback, file| self.for_each_resolved_module(callback, file),
                    |callback, file| {
                        self.for_each_resolved_type_reference_directive(callback, file);
                    },
                );
            }

            // Check other dependencies for symlinks
            let mut seen_package_jsons: tsrs_core::collections::Set<Path> =
                tsrs_core::collections::Set::default();
            // Go ranges over these maps too. Each resolution records the realpath the file system gives a symlink, so
            // the cache gets the same entries in any order; `has_directory` only skips resolving a directory again.
            #[expect(clippy::iter_over_hash_type, reason = "see the comment above")]
            for (file_path, meta) in &self.source_file_meta_datas {
                if meta.package_json_directory.is_empty()
                    || !self.source_file_may_be_emitted(
                        self.get_source_file_by_path(file_path).unwrap(),
                        false,
                    )
                    || !seen_package_jsons.add_if_absent(self.to_path(&meta.package_json_directory))
                {
                    continue;
                }
                let package_json_name =
                    tspath::combine_paths(&meta.package_json_directory, &["package.json"]);
                let info = self.get_package_json_info(&package_json_name);
                let Some(contents) = info.and_then(|info| info.get_contents()) else {
                    continue;
                };

                #[expect(
                    clippy::iter_over_hash_type,
                    reason = "see the comment above the outer loop"
                )]
                for dep in contents.get_runtime_dependency_names().keys() {
                    // Skip work in common case: we already saved a symlink for this package directory
                    // in the node_modules adjacent to this package.json
                    let possible_directory_path = self.to_path(&tspath::combine_paths(
                        &meta.package_json_directory,
                        &["node_modules", dep],
                    ));
                    if known_symlinks.has_directory(&possible_directory_path) {
                        continue;
                    }
                    if !dep.starts_with("@types") {
                        let possible_types_directory_path = self.to_path(&tspath::combine_paths(
                            &meta.package_json_directory,
                            &["node_modules", &module::get_types_package_name(dep)],
                        ));
                        if known_symlinks.has_directory(&possible_types_directory_path) {
                            continue;
                        }
                    }

                    if let Some(package_resolution) = resolver
                        .resolve_package_directory(
                            dep,
                            &package_json_name,
                            ModuleKind::COMMON_JS,
                            None,
                        )
                        .filter(|r| r.is_resolved() && !r.original_path.is_empty())
                    {
                        known_symlinks.process_resolution(
                            &tspath::combine_paths(
                                package_resolution.original_path,
                                &["package.json"],
                            ),
                            &tspath::combine_paths(
                                package_resolution.resolved_file_name,
                                &["package.json"],
                            ),
                        );
                    }
                }
            }
            P::new(known_symlinks)
        })
    }

    fn for_each_resolved_module(
        &self,
        callback: &mut dyn FnMut(&ResolvedModule, &str, ResolutionMode, &Path),
        file: Option<P<SourceFile>>,
    ) {
        for_each_resolution(
            &self.resolved_modules,
            |resolution, module_name, mode, file_path| {
                callback(resolution, module_name, mode, file_path);
            },
            file,
        );
    }

    fn for_each_resolved_type_reference_directive(
        &self,
        callback: &mut dyn FnMut(&ResolvedTypeReferenceDirective, &str, ResolutionMode, &Path),
        file: Option<P<SourceFile>>,
    ) {
        for_each_resolution(
            &self.type_resolutions_in_file,
            |resolution, module_name, mode, file_path| {
                callback(resolution, module_name, mode, file_path);
            },
            file,
        );
    }
}

#[expect(
    clippy::iter_over_hash_type,
    reason = "the one caller (KnownSymlinks::set_symlinks_from_resolutions) records file-system symlinks, which come out the same in any order"
)]
fn for_each_resolution<T>(
    resolution_cache: &FxHashMap<Path, ModeAwareCache<P<T>>>,
    mut callback: impl FnMut(&T, &str, ResolutionMode, &Path),
    file: Option<P<SourceFile>>,
) {
    if let Some(file) = file {
        if let Some(resolutions) = resolution_cache.get(file.path()) {
            for (key, resolution) in resolutions {
                callback(resolution, key.name, key.mode, file.path());
            }
        }
    } else {
        for (file_path, resolutions) in resolution_cache {
            for (key, resolution) in resolutions {
                callback(resolution, key.name, key.mode, file_path);
            }
        }
    }
}

// program.go:84 `lazyValue.tryReuse`.
fn try_reuse<T: Clone>(to: &OnceLock<T>, from: &OnceLock<T>) {
    if let Some(value) = from.get() {
        let _ = to.set(value.clone());
    }
}

// Go `slices.EqualFunc`.
fn equal_func<T: Copy>(a: &[T], b: &[T], mut eq: impl FnMut(T, T) -> bool) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(&x, &y)| eq(x, y))
}

// program.go:503
fn equal_module_specifiers(n1: P<Node>, n2: P<Node>) -> bool {
    n1.kind() == n2.kind() && (!ast::is_string_literal(n1) || n1.text() == n2.text())
}

// program.go:507
fn equal_module_augmentation_names(n1: P<Node>, n2: P<Node>) -> bool {
    n1.kind() == n2.kind() && n1.text() == n2.text()
}

// program.go:511
fn equal_file_references(f1: P<FileReference>, f2: P<FileReference>) -> bool {
    f1.file_name == f2.file_name
        && f1.resolution_mode == f2.resolution_mode
        && f1.preserve == f2.preserve
}

// program.go:515
fn equal_check_js_directives(
    d1: Option<P<ast::CheckJsDirective>>,
    d2: Option<P<ast::CheckJsDirective>>,
) -> bool {
    match (d1, d2) {
        (None, None) => true,
        (Some(d1), Some(d2)) => d1.enabled == d2.enabled,
        _ => false,
    }
}

// program.go:716
fn filter_and_sort_diagnostics(diags: &[P<Diagnostic>]) -> Vec<P<Diagnostic>> {
    let filtered: Vec<P<Diagnostic>> = diags
        .iter()
        .copied()
        .filter(|diag| {
            let file = diag.file();
            let Some(span_map) = file.and_then(|file| file.span_map()) else {
                return true;
            };
            if !diag.reports_unnecessary() || !diag.source().is_empty() {
                return true;
            }
            let Some(loc) = diag.loc() else {
                return true;
            };
            let (_, fidelity) = span_map.virtual_to_original_span(loc);
            fidelity != tsrs_spanmap::Fidelity::None
        })
        .collect();
    sort_and_deduplicate_diagnostics(&filtered)
}

// program.go:1551
fn apply_content_mapper_diagnostic_directives(
    source_file: P<SourceFile>,
    diags: Vec<P<Diagnostic>>,
) -> Vec<P<Diagnostic>> {
    let directives = source_file.diagnostic_directives();
    if directives.is_empty() {
        return diags;
    }
    let mut used = vec![false; directives.len()];
    let mut mark_used = |diag: P<Diagnostic>| -> bool {
        if diag.file() != Some(source_file) || !diag.source().is_empty() {
            return false;
        }
        let Some(pos) = diag.pos() else {
            return false;
        };
        directives.iter().enumerate().any(|(i, directive)| {
            if Some(pos) >= directive.virtual_range.source_pos()
                && Some(pos) < directive.virtual_range.source_end()
            {
                used[i] = true;
                return true;
            }

            false
        })
    };
    let mut filtered: Vec<P<Diagnostic>> =
        diags.into_iter().filter(|&diag| !mark_used(diag)).collect();
    filtered.extend(directives.iter().enumerate().filter_map(|(i, directive)| {
        if directive.policy == ast::MappedDiagnosticDirectivePolicy::Expect && !used[i] {
            Some(ast::new_external_diagnostic(
                Some(source_file),
                directive.original_range,
                directive.source,
                diagnostics::Category::Error,
                directive.unused_code,
                directive.unused_message_text,
            ))
        } else {
            None
        }
    }));
    filtered
}

// getAdditionalJSSyntacticDiagnostics produces option-dependent syntactic diagnostics for JS files
// that aren't covered by the parser or the checker. The checker handles these for checked files,
// but doesn't run on unchecked JS files (no checkJs/ts-check).
fn get_additional_js_syntactic_diagnostics(
    file: P<SourceFile>,
    options: &CompilerOptions,
) -> Vec<P<Diagnostic>> {
    // Parameter decorators are only valid with experimentalDecorators. Without it,
    // the checker would report this, but the checker doesn't run on unchecked JS files.
    fn walk(node: P<Node>, file: P<SourceFile>, diags: &mut Vec<P<Diagnostic>>) -> bool {
        if !node.subtree_facts().intersects(ast::SubtreeFacts::ContainsDecorators) {
            return false;
        }
        if node.kind() == Kind::Parameter
            && ast::has_decorators(node)
            && let Some(decorator) =
                node.modifier_nodes().iter().copied().find(|n| ast::is_decorator(*n))
        {
            diags.push(new_diagnostic(
                Some(file),
                decorator.loc(),
                &diagnostics::DECORATORS_ARE_NOT_VALID_HERE,
                &[],
            ));
        }
        node.for_each_child(&mut |child| walk(child, file, diags));
        false
    }

    if options.experimental_decorators.is_true() {
        return Vec::new();
    }
    let mut diags = Vec::new();
    file.as_node().for_each_child(&mut |child| walk(child, file, &mut diags));
    diags
}

pub fn filter_no_emit_semantic_diagnostics(
    diagnostics: Vec<P<Diagnostic>>,
    options: &CompilerOptions,
) -> Vec<P<Diagnostic>> {
    if !options.no_emit.is_true() {
        return diagnostics;
    }
    diagnostics.into_iter().filter(|d| !d.skipped_on_no_emit()).collect()
}

fn has_zero_or_one_asterisk_character(str: &str) -> bool {
    str.bytes().filter(|&c| c == b'*').count() <= 1
}

fn module_resolution_supports_package_json_exports_and_imports(
    module_resolution: ModuleResolutionKind,
) -> bool {
    module_resolution >= ModuleResolutionKind::NODE16
        && module_resolution <= ModuleResolutionKind::NODE_NEXT
        || module_resolution == ModuleResolutionKind::BUNDLER
}

fn emit_module_kind_is_non_node_esm(module_kind: ModuleKind) -> bool {
    module_kind >= ModuleKind::ES2015 && module_kind <= ModuleKind::ES_NEXT
}

fn is_comment_or_blank_line(text: &str, pos: usize) -> bool {
    let text = text.as_bytes();
    let mut pos = pos;
    pos +=
        (pos..text.len()).take_while(|&index| text[index] == b' ' || text[index] == b'\t').count();
    pos == text.len()
        || pos < text.len() && (text[pos] == b'\r' || text[pos] == b'\n')
        || pos + 1 < text.len() && text[pos] == b'/' && text[pos + 1] == b'/'
}

pub fn sort_and_deduplicate_diagnostics(diagnostics: &[P<Diagnostic>]) -> Vec<P<Diagnostic>> {
    let mut diagnostics = diagnostics.to_vec();
    // Go's slices.SortFunc is not stable; equal diagnostics are merged below, so stability does not matter.
    diagnostics.sort_by(|a, b| compare_diagnostics(*a, *b));
    compact_and_merge_related_infos(diagnostics)
}

// Remove duplicate diagnostics and, for sequences of diagnostics that differ only by related information,
// create a single diagnostic with sorted and deduplicated related information.
fn compact_and_merge_related_infos(diagnostics: Vec<P<Diagnostic>>) -> Vec<P<Diagnostic>> {
    if diagnostics.len() < 2 {
        return diagnostics;
    }
    let mut result = Vec::with_capacity(diagnostics.len());
    let mut i = 0;
    while i < diagnostics.len() {
        let mut d = diagnostics[i];
        let mut n = 1;
        n += (i + n..diagnostics.len())
            .take_while(|&index| equal_diagnostics_no_related_info(d, diagnostics[index]))
            .count();
        if n > 1 {
            let mut related_infos: Vec<P<Diagnostic>> = Vec::new();
            let mut any = false;
            for k in 0..n {
                let r = diagnostics[i + k].related_information();
                if !r.is_empty() {
                    any = true;
                }
                related_infos.extend_from_slice(r);
            }
            if any {
                related_infos.sort_by(|a, b| compare_diagnostics(*a, *b));
                related_infos.dedup_by(|a, b| equal_diagnostics(*a, *b));
                d = d.clone_diagnostic().set_related_info(&related_infos);
            }
        }
        result.push(d);
        i += n;
    }
    result
}

// GetDiagnosticsOfAnyProgram collects config, syntactic, program, global and semantic diagnostics
// in tsc's order, stopping at the first phase that produced diagnostics.
// program.go:2018
pub fn get_diagnostics_of_any_program(
    ctx: &Context,
    program: &'static Program,
    files: Option<&[P<SourceFile>]>,
    skip_no_emit_check_for_dts_diagnostics: bool,
    get_bind_diagnostics: &mut dyn FnMut(&Context, Option<P<SourceFile>>) -> Vec<P<Diagnostic>>,
    get_semantic_diagnostics: &mut dyn FnMut(&Context, Option<P<SourceFile>>) -> Vec<P<Diagnostic>>,
) -> Vec<P<Diagnostic>> {
    let mut all_diagnostics = program.get_config_file_parsing_diagnostics();
    let config_file_parsing_diagnostics_length = all_diagnostics.len();

    let append_diagnostics_for_all_files = |diagnostics: &mut Vec<P<Diagnostic>>,
                                            get_diagnostics: &mut dyn FnMut(
        &Context,
        Option<P<SourceFile>>,
    ) -> Vec<
        P<Diagnostic>,
    >| match files {
        None => diagnostics.extend(get_diagnostics(ctx, None)),
        Some(files) => {
            diagnostics.extend(files.iter().flat_map(|&file| get_diagnostics(ctx, Some(file))));
        }
    };

    let mut syntactic_diagnostics = Vec::new();
    let start = tsrs_core::phases::Timer::new(program.options().diagnostics_enabled());
    append_diagnostics_for_all_files(&mut syntactic_diagnostics, &mut |ctx, f| {
        program.get_syntactic_diagnostics(ctx, f)
    });
    start.record("Diagnostics: syntactic");
    if !syntactic_diagnostics.is_empty() {
        // Per-file content mapper failures are syntactic diagnostics, but the locationless diagnostic
        // that disables a repeatedly failing mapper must still be reported.
        all_diagnostics.extend_from_slice(&program.content_mapper_diagnostics);
    }
    all_diagnostics.extend(syntactic_diagnostics);

    // If we didn't have any syntactic errors, then also try getting the program (options),
    // global and semantic errors.
    if all_diagnostics.len() == config_file_parsing_diagnostics_length {
        all_diagnostics.extend(tsrs_core::phases::time(
            program.options().diagnostics_enabled(),
            "Diagnostics: program",
            || program.get_program_diagnostics(),
        ));

        // Do binding early so we can track the time.
        append_diagnostics_for_all_files(&mut Vec::new(), get_bind_diagnostics);

        if program.options().list_files_only.is_false_or_unknown() {
            all_diagnostics.extend(tsrs_core::phases::time(
                program.options().diagnostics_enabled(),
                "Diagnostics: global (first)",
                || program.get_global_diagnostics(ctx),
            ));

            if all_diagnostics.len() == config_file_parsing_diagnostics_length {
                append_diagnostics_for_all_files(&mut all_diagnostics, get_semantic_diagnostics);
                // Incremental programs cache checking globals with file diagnostics;
                // a late sweep would also collect incidental signature-generation globals.
                all_diagnostics.extend(tsrs_core::phases::time(
                    program.options().diagnostics_enabled(),
                    "Diagnostics: global (after)",
                    || program.get_global_diagnostics(ctx),
                ));
                crate::checkerpool::write_file_times(program);
            }

            if (skip_no_emit_check_for_dts_diagnostics || program.options().no_emit.is_true())
                && program.options().get_emit_declarations()
                && all_diagnostics.len() == config_file_parsing_diagnostics_length
            {
                append_diagnostics_for_all_files(&mut all_diagnostics, &mut |ctx, f| {
                    program.get_declaration_diagnostics(ctx, f)
                });
            }
        }
    }
    crate::checkerpool::write_cost_cache(program);
    all_diagnostics
}

fn plain_js_errors() -> &'static FxHashSet<i32> {
    static SET: OnceLock<FxHashSet<i32>> = OnceLock::new();
    SET.get_or_init(|| {
        [
            // binder errors
            &diagnostics::CANNOT_REDECLARE_BLOCK_SCOPED_VARIABLE_0,
            &diagnostics::A_MODULE_CANNOT_HAVE_MULTIPLE_DEFAULT_EXPORTS,
            &diagnostics::ANOTHER_EXPORT_DEFAULT_IS_HERE,
            &diagnostics::THE_FIRST_EXPORT_DEFAULT_IS_HERE,
            &diagnostics::IDENTIFIER_EXPECTED_0_IS_A_RESERVED_WORD_AT_THE_TOP_LEVEL_OF_A_MODULE,
            &diagnostics::IDENTIFIER_EXPECTED_0_IS_A_RESERVED_WORD_IN_STRICT_MODE_MODULES_ARE_AUTOMATICALLY_IN_STRICT_MODE,
            &diagnostics::IDENTIFIER_EXPECTED_0_IS_A_RESERVED_WORD_THAT_CANNOT_BE_USED_HERE,
            &diagnostics::X_CONSTRUCTOR_IS_A_RESERVED_WORD,
            &diagnostics::X_DELETE_CANNOT_BE_CALLED_ON_AN_IDENTIFIER_IN_STRICT_MODE,
            &diagnostics::CODE_CONTAINED_IN_A_CLASS_IS_EVALUATED_IN_JAVA_SCRIPT_S_STRICT_MODE_WHICH_DOES_NOT_ALLOW_THIS_USE_OF_0_FOR_MORE_INFORMATION_SEE_HTTPS_COLON_SLASH_SLASHDEVELOPER_MOZILLA_ORG_SLASHEN_US_SLASHDOCS_SLASH_WEB_SLASH_JAVA_SCRIPT_SLASH_REFERENCE_SLASH_STRICT_MODE,
            &diagnostics::INVALID_USE_OF_0_MODULES_ARE_AUTOMATICALLY_IN_STRICT_MODE,
            &diagnostics::INVALID_USE_OF_0_IN_STRICT_MODE,
            &diagnostics::A_LABEL_IS_NOT_ALLOWED_HERE,
            &diagnostics::X_WITH_STATEMENTS_ARE_NOT_ALLOWED_IN_STRICT_MODE,
            // grammar errors
            &diagnostics::A_BREAK_STATEMENT_CAN_ONLY_BE_USED_WITHIN_AN_ENCLOSING_ITERATION_OR_SWITCH_STATEMENT,
            &diagnostics::A_BREAK_STATEMENT_CAN_ONLY_JUMP_TO_A_LABEL_OF_AN_ENCLOSING_STATEMENT,
            &diagnostics::A_CLASS_DECLARATION_WITHOUT_THE_DEFAULT_MODIFIER_MUST_HAVE_A_NAME,
            &diagnostics::A_CLASS_MEMBER_CANNOT_HAVE_THE_0_KEYWORD,
            &diagnostics::A_COMMA_EXPRESSION_IS_NOT_ALLOWED_IN_A_COMPUTED_PROPERTY_NAME,
            &diagnostics::A_CONTINUE_STATEMENT_CAN_ONLY_BE_USED_WITHIN_AN_ENCLOSING_ITERATION_STATEMENT,
            &diagnostics::A_CONTINUE_STATEMENT_CAN_ONLY_JUMP_TO_A_LABEL_OF_AN_ENCLOSING_ITERATION_STATEMENT,
            &diagnostics::A_DEFAULT_CLAUSE_CANNOT_APPEAR_MORE_THAN_ONCE_IN_A_SWITCH_STATEMENT,
            &diagnostics::A_DEFAULT_EXPORT_MUST_BE_AT_THE_TOP_LEVEL_OF_A_FILE_OR_MODULE_DECLARATION,
            &diagnostics::A_DEFERRED_IMPORT_MUST_SPECIFY_A_NAMESPACE_BINDING,
            &diagnostics::A_DEFINITE_ASSIGNMENT_ASSERTION_IS_NOT_PERMITTED_IN_THIS_CONTEXT,
            &diagnostics::A_DESTRUCTURING_DECLARATION_MUST_HAVE_AN_INITIALIZER,
            &diagnostics::A_GET_ACCESSOR_CANNOT_HAVE_PARAMETERS,
            &diagnostics::A_REST_ELEMENT_CANNOT_CONTAIN_A_BINDING_PATTERN,
            &diagnostics::A_REST_ELEMENT_CANNOT_HAVE_A_PROPERTY_NAME,
            &diagnostics::A_REST_ELEMENT_CANNOT_HAVE_AN_INITIALIZER,
            &diagnostics::A_REST_ELEMENT_MUST_BE_LAST_IN_A_DESTRUCTURING_PATTERN,
            &diagnostics::A_REST_PARAMETER_CANNOT_HAVE_AN_INITIALIZER,
            &diagnostics::A_REST_PARAMETER_MUST_BE_LAST_IN_A_PARAMETER_LIST,
            &diagnostics::A_REST_PARAMETER_OR_BINDING_PATTERN_MAY_NOT_HAVE_A_TRAILING_COMMA,
            &diagnostics::A_RETURN_STATEMENT_CANNOT_BE_USED_INSIDE_A_CLASS_STATIC_BLOCK,
            &diagnostics::A_SET_ACCESSOR_CANNOT_HAVE_REST_PARAMETER,
            &diagnostics::A_SET_ACCESSOR_MUST_HAVE_EXACTLY_ONE_PARAMETER,
            &diagnostics::A_SOURCE_PHASE_IMPORT_MUST_SPECIFY_A_LOCAL_BINDING,
            &diagnostics::AN_EXPORT_DECLARATION_CAN_ONLY_BE_USED_AT_THE_TOP_LEVEL_OF_A_MODULE,
            &diagnostics::AN_EXPORT_DECLARATION_CANNOT_HAVE_MODIFIERS,
            &diagnostics::AN_IMPORT_DECLARATION_CAN_ONLY_BE_USED_AT_THE_TOP_LEVEL_OF_A_MODULE,
            &diagnostics::AN_IMPORT_DECLARATION_CANNOT_HAVE_MODIFIERS,
            &diagnostics::AN_OBJECT_MEMBER_CANNOT_BE_DECLARED_OPTIONAL,
            &diagnostics::ARGUMENT_OF_DYNAMIC_IMPORT_CANNOT_BE_SPREAD_ELEMENT,
            &diagnostics::CANNOT_ASSIGN_TO_PRIVATE_METHOD_0_PRIVATE_METHODS_ARE_NOT_WRITABLE,
            &diagnostics::CANNOT_REDECLARE_IDENTIFIER_0_IN_CATCH_CLAUSE,
            &diagnostics::CATCH_CLAUSE_VARIABLE_CANNOT_HAVE_AN_INITIALIZER,
            &diagnostics::CLASS_DECORATORS_CAN_T_BE_USED_WITH_STATIC_PRIVATE_IDENTIFIER_CONSIDER_REMOVING_THE_EXPERIMENTAL_DECORATOR,
            &diagnostics::CLASSES_CAN_ONLY_EXTEND_A_SINGLE_CLASS,
            &diagnostics::CLASSES_MAY_NOT_HAVE_A_FIELD_NAMED_CONSTRUCTOR,
            &diagnostics::DID_YOU_MEAN_TO_USE_A_COLON_AN_CAN_ONLY_FOLLOW_A_PROPERTY_NAME_WHEN_THE_CONTAINING_OBJECT_LITERAL_IS_PART_OF_A_DESTRUCTURING_PATTERN,
            &diagnostics::DUPLICATE_LABEL_0,
            &diagnostics::DYNAMIC_IMPORTS_CAN_ONLY_ACCEPT_A_MODULE_SPECIFIER_AND_AN_OPTIONAL_SET_OF_ATTRIBUTES_AS_ARGUMENTS,
            &diagnostics::X_FOR_AWAIT_LOOPS_CANNOT_BE_USED_INSIDE_A_CLASS_STATIC_BLOCK,
            &diagnostics::JSX_ATTRIBUTES_MUST_ONLY_BE_ASSIGNED_A_NON_EMPTY_EXPRESSION,
            &diagnostics::JSX_ELEMENTS_CANNOT_HAVE_MULTIPLE_ATTRIBUTES_WITH_THE_SAME_NAME,
            &diagnostics::JSX_EXPRESSIONS_MAY_NOT_USE_THE_COMMA_OPERATOR_DID_YOU_MEAN_TO_WRITE_AN_ARRAY,
            &diagnostics::JSX_PROPERTY_ACCESS_EXPRESSIONS_CANNOT_INCLUDE_JSX_NAMESPACE_NAMES,
            &diagnostics::JUMP_TARGET_CANNOT_CROSS_FUNCTION_BOUNDARY,
            &diagnostics::LINE_TERMINATOR_NOT_PERMITTED_BEFORE_ARROW,
            &diagnostics::MODIFIERS_CANNOT_APPEAR_HERE,
            &diagnostics::NAMED_AND_NAMESPACE_IMPORTS_ARE_NOT_ALLOWED_IN_A_SOURCE_PHASE_IMPORT,
            &diagnostics::ONLY_A_SINGLE_VARIABLE_DECLARATION_IS_ALLOWED_IN_A_FOR_IN_STATEMENT,
            &diagnostics::ONLY_A_SINGLE_VARIABLE_DECLARATION_IS_ALLOWED_IN_A_FOR_OF_STATEMENT,
            &diagnostics::OPTIONAL_CHAINING_CANNOT_BE_USED_WITH_IMPORT_SOURCE,
            &diagnostics::PRIVATE_IDENTIFIERS_ARE_NOT_ALLOWED_OUTSIDE_CLASS_BODIES,
            &diagnostics::PRIVATE_IDENTIFIERS_ARE_ONLY_ALLOWED_IN_CLASS_BODIES_AND_MAY_ONLY_BE_USED_AS_PART_OF_A_CLASS_MEMBER_DECLARATION_PROPERTY_ACCESS_OR_ON_THE_LEFT_HAND_SIDE_OF_AN_IN_EXPRESSION,
            &diagnostics::PROPERTY_0_IS_NOT_ACCESSIBLE_OUTSIDE_CLASS_1_BECAUSE_IT_HAS_A_PRIVATE_IDENTIFIER,
            &diagnostics::SOURCE_PHASE_IMPORTS_ARE_NOT_ALLOWED_ON_STATEMENTS_THAT_COMPILE_TO_COMMON_JS_REQUIRE_CALLS,
            &diagnostics::SOURCE_PHASE_IMPORTS_ARE_ONLY_SUPPORTED_WHEN_THE_MODULE_OPTION_IS_SET_TO_ESNEXT_NODENEXT_OR_PRESERVE,
            &diagnostics::TAGGED_TEMPLATE_EXPRESSIONS_ARE_NOT_PERMITTED_IN_AN_OPTIONAL_CHAIN,
            &diagnostics::THE_LEFT_HAND_SIDE_OF_A_FOR_OF_STATEMENT_MAY_NOT_BE_ASYNC,
            &diagnostics::THE_VARIABLE_DECLARATION_OF_A_FOR_IN_STATEMENT_CANNOT_HAVE_AN_INITIALIZER,
            &diagnostics::THE_VARIABLE_DECLARATION_OF_A_FOR_OF_STATEMENT_CANNOT_HAVE_AN_INITIALIZER,
            &diagnostics::TRAILING_COMMA_NOT_ALLOWED,
            &diagnostics::VARIABLE_DECLARATION_LIST_CANNOT_BE_EMPTY,
            &diagnostics::X_0_AND_1_OPERATIONS_CANNOT_BE_MIXED_WITHOUT_PARENTHESES,
            &diagnostics::X_0_EXPECTED,
            &diagnostics::X_0_IS_NOT_A_VALID_META_PROPERTY_FOR_KEYWORD_IMPORT_DID_YOU_MEAN_META_DEFER_OR_SOURCE,
            &diagnostics::X_0_IS_NOT_A_VALID_META_PROPERTY_FOR_KEYWORD_1_DID_YOU_MEAN_2,
            &diagnostics::X_0_LIST_CANNOT_BE_EMPTY,
            &diagnostics::X_0_MODIFIER_ALREADY_SEEN,
            &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_CONSTRUCTOR_DECLARATION,
            &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_MODULE_OR_NAMESPACE_ELEMENT,
            &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_A_PARAMETER,
            &diagnostics::X_0_MODIFIER_CANNOT_APPEAR_ON_CLASS_ELEMENTS_OF_THIS_KIND,
            &diagnostics::X_0_MODIFIER_CANNOT_BE_USED_HERE,
            &diagnostics::X_0_MODIFIER_MUST_PRECEDE_1_MODIFIER,
            &diagnostics::X_0_DECLARATIONS_CAN_ONLY_BE_DECLARED_INSIDE_A_BLOCK,
            &diagnostics::X_0_DECLARATIONS_MUST_BE_INITIALIZED,
            &diagnostics::X_EXTENDS_CLAUSE_ALREADY_SEEN,
            &diagnostics::X_LET_IS_NOT_ALLOWED_TO_BE_USED_AS_A_NAME_IN_LET_OR_CONST_DECLARATIONS,
            &diagnostics::CLASS_CONSTRUCTOR_MAY_NOT_BE_A_GENERATOR,
            &diagnostics::CLASS_CONSTRUCTOR_MAY_NOT_BE_AN_ACCESSOR,
            &diagnostics::X_AWAIT_EXPRESSIONS_ARE_ONLY_ALLOWED_WITHIN_ASYNC_FUNCTIONS_AND_AT_THE_TOP_LEVELS_OF_MODULES,
            &diagnostics::X_AWAIT_USING_STATEMENTS_ARE_ONLY_ALLOWED_WITHIN_ASYNC_FUNCTIONS_AND_AT_THE_TOP_LEVELS_OF_MODULES,
            &diagnostics::PRIVATE_FIELD_0_MUST_BE_DECLARED_IN_AN_ENCLOSING_CLASS,
            // Type errors
            &diagnostics::THIS_CONDITION_WILL_ALWAYS_RETURN_0_SINCE_JAVA_SCRIPT_COMPARES_OBJECTS_BY_REFERENCE_NOT_VALUE,
        ]
        .iter()
        .map(|m| m.code())
        .collect()
    })
}

impl tsoptions::outputpaths::OutputPathsHost for Program {
    fn common_source_directory(&self) -> String {
        Program::common_source_directory(self).to_string()
    }

    fn content_mapper_extensions(&self) -> Vec<String> {
        self.opts.config.content_mapper_extensions()
    }

    fn content_mapper_extension_rewrites(&self) -> Vec<tspath::ExtensionRewrite> {
        Program::content_mapper_extension_rewrites(self)
    }

    fn get_current_directory(&self) -> &str {
        Program::get_current_directory(self)
    }

    fn use_case_sensitive_file_names(&self) -> bool {
        Program::use_case_sensitive_file_names(self)
    }
}
