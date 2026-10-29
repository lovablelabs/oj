use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};

use tsrs_ast::Diagnostic;
use tsrs_compiler::{
    CheckerPool, CreateCheckerPool, CreateModuleResolver, Program, ProgramOptions, new_program,
    sort_and_deduplicate_diagnostics,
};
use tsrs_core::collections::{Set, SyncMap};
use tsrs_core::context::Context;
use tsrs_core::tspath::{self, ComparePathsOptions, Path};
use tsrs_core::{
    CompilerOptions, JsxEmit, ModuleKind, ModuleResolutionKind, P, ProjectReference, ScriptTarget,
    Tristate, TypeAcquisition,
};
use tsrs_ls as ls;
use tsrs_lsproto as lsproto;
use tsrs_module::{self as module, Resolver, ResolverOptions};
use tsrs_tsoptions::{self as tsoptions, Mapper, ParsedCommandLine};

use crate::ata;
use crate::checkerpool::{CheckerPoolHandle, CheckerPoolImpl, new_checker_pool};
use crate::compilerhost::CompilerHostImpl;
use crate::dirty::Cloneable;
use crate::logging::LogTree;
use crate::memregions::ProgramOwner;
use crate::parsecache::{parse_cache_key_for_duplicate, parse_cache_key_for_file};
use crate::projectcollectionbuilder::ProjectCollectionBuilder;
use crate::snapshot::{ModuleResolverFactory, ProjectTreeRequest};
use crate::watch::{
    ALL_WATCH_KINDS, PatternsAndIgnored, WatchedFiles, create_resolution_lookup_glob_mapper,
    new_watched_files, new_watched_files_for_paths,
};
use tsrs_core::arena::Region;

const INFERRED_PROJECT_NAME: &str = "/dev/null/inferred"; // lowercase so toPath is a no-op regardless of settings
const SYNTHETIC_PROJECT_PREFIX: &str = "/dev/null/synthetic/";
pub(crate) const HR: &str = "-----------------------------------------------";

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct ID(pub String);

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct ConfiguredProjectID(pub(crate) Path);

impl ConfiguredProjectID {
    // project.go:54
    pub fn as_id(&self) -> ID {
        ID(self.0.to_string())
    }
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct InferredProjectID(String);

pub(crate) type ModuleResolverFactoryRef = Option<Arc<dyn ModuleResolverFactory>>;

pub(crate) fn inferred_project_id() -> InferredProjectID {
    InferredProjectID(INFERRED_PROJECT_NAME.to_string())
}

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub struct SyntheticProjectID(pub String);

// project.go:46
pub fn new_synthetic_project_id(id: i64) -> SyntheticProjectID {
    assert!(id > 0, "invalid synthetic project ID: {id}");
    SyntheticProjectID(format!("{SYNTHETIC_PROJECT_PREFIX}{id}"))
}

impl std::fmt::Display for ID {
    // project.go:53
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl InferredProjectID {
    // project.go:55
    pub(crate) fn as_id(&self) -> ID {
        ID(self.0.clone())
    }
}

impl SyntheticProjectID {
    // project.go:56
    fn as_id(&self) -> ID {
        ID(self.0.clone())
    }
}

impl ID {
    // project.go:58
    pub(crate) fn configured(&self) -> Option<ConfiguredProjectID> {
        parse_configured_project_id(&Path::new(self.0.clone()))
    }

    // project.go:76
    pub(crate) fn inferred(&self) -> Option<InferredProjectID> {
        if self.0 == INFERRED_PROJECT_NAME { Some(inferred_project_id()) } else { None }
    }

    // project.go:80
    pub fn synthetic(&self) -> Option<SyntheticProjectID> {
        parse_synthetic_project_id(&self.0)
    }
}

// project.go:62
pub fn parse_configured_project_id(value: &Path) -> Option<ConfiguredProjectID> {
    let id: &str = value.as_str();
    if id.is_empty() || id == INFERRED_PROJECT_NAME || parse_synthetic_project_id(id).is_some() {
        return None;
    }
    Some(ConfiguredProjectID(value.clone()))
}

// project.go:97
pub fn parse_synthetic_project_id(value: &str) -> Option<SyntheticProjectID> {
    let suffix = value.strip_prefix(SYNTHETIC_PROJECT_PREFIX)?;
    // Go strconv.Atoi: optional sign, decimal digits.
    let id: i64 = suffix.parse().ok()?;
    if id <= 0 {
        return None;
    }
    Some(new_synthetic_project_id(id))
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum Kind {
    #[default]
    Inferred = 0,
    Configured = 1,
    Synthetic = 2,
}

impl Kind {
    // project_stringer_generated.go:19
    pub(crate) fn string(self) -> &'static str {
        match self {
            Kind::Inferred => "Inferred",
            Kind::Configured => "Configured",
            Kind::Synthetic => "Synthetic",
        }
    }
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum ProgramUpdateKind {
    #[default]
    None = 0,
    Cloned = 1,
    SameFileNames = 2,
    NewFiles = 3,
}

#[repr(u32)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum PendingReload {
    #[default]
    None = 0,
    FileNames = 1,
    Full = 2,
}

// Project represents a TypeScript project.
// If changing struct fields, also update the Clone method.
//
// Snapshots share projects as `Shared<Project>` (Go `*Project`). `#[derive(Clone)]` is a field-by-field copy
// used for copy-on-write; Go's `Clone` is `Cloneable::clone_value`.
#[derive(Clone)]
pub struct Project {
    pub kind: Kind,
    id: ID,
    pub(crate) current_directory: String,
    config_file_name: String,
    pub(crate) config_file_path: Path,

    pub(crate) dirty: bool,
    pub(crate) dirty_file_path: Path,

    pub(crate) host: Option<Arc<CompilerHostImpl>>,
    pub command_line: Option<P<ParsedCommandLine>>,
    // Go: `commandLineWithTypingsFiles` + `commandLineWithTypingsFilesOnce`.
    command_line_with_typings_files: Arc<OnceLock<P<ParsedCommandLine>>>,
    pub program: Option<&'static Program>,
    // The kind of update that was performed on the program last time it was updated.
    pub(crate) program_update_kind: ProgramUpdateKind,
    // The ID of the snapshot that created the program stored in this project.
    pub(crate) program_last_update: u64,
    // Set of projects that this project could be referencing.
    // Only set before actually loading config file to get actual project references
    potential_project_references: Option<Arc<Set<Path>>>,

    pub(crate) program_files_watch: Option<Arc<WatchedFiles<Option<Arc<SyncMap<Path, String>>>>>>,
    pub(crate) typings_watch: Option<Arc<WatchedFiles<PatternsAndIgnored>>>,
    pub(crate) content_mapper_watch: Option<Arc<WatchedFiles<Vec<String>>>>,
    pub(crate) content_mapper_watched_files: Option<Arc<Set<Path>>>,

    pub checker_pool: Option<Arc<CheckerPoolImpl>>,
    // Memory regions (not in Go): owns the program and what is freed with it (memregions.rs).
    pub(crate) program_owner: Option<Arc<ProgramOwner>>,

    pub(crate) module_resolver_factory: Option<Arc<dyn ModuleResolverFactory>>,
    pub(crate) module_resolver_id: u64,

    // installedTypingsInfo is the value of `project.ComputeTypingsInfo()` that was
    // used during the most recently completed typings installation.
    pub(crate) installed_typings_info: Option<Arc<ata::TypingsInfo>>,
    // typingsFiles are the root files added by the typings installer.
    pub(crate) typings_files: Vec<String>,
}

// project.go:180
pub(crate) fn new_configured_project(
    config_file_name: &str,
    config_file_path: &Path,
    builder: &ProjectCollectionBuilder,
    logger: &LogTree,
) -> Project {
    let Some(configured_project_id) = parse_configured_project_id(config_file_path) else {
        panic!("invalid configured project ID: {}", config_file_path.0);
    };
    let mut project = new_project(
        configured_project_id.as_id(),
        Kind::Configured,
        &tspath::get_directory_path(config_file_name),
        builder,
        logger,
    );
    project.config_file_name = config_file_name.to_string();
    project.config_file_path = config_file_path.clone();
    project
}

// project.go:196
pub(crate) fn new_inferred_project(
    current_directory: &str,
    compiler_options: Option<P<CompilerOptions>>,
    root_file_names: Vec<String>,
    project_references: Vec<ProjectReference>,
    content_mappers: Vec<Mapper>,
    errors: Vec<P<Diagnostic>>,
    builder: &ProjectCollectionBuilder,
    logger: &LogTree,
) -> Project {
    let mut p = new_project(
        inferred_project_id().as_id(),
        Kind::Inferred,
        current_directory,
        builder,
        logger,
    );
    let compiler_options = compiler_options.unwrap_or_else(|| {
        P::new(CompilerOptions {
            allow_js: Tristate::True,
            module: ModuleKind::ES_NEXT,
            module_resolution: ModuleResolutionKind::BUNDLER,
            target: ScriptTarget::LATEST_STANDARD,
            jsx: JsxEmit::REACT_JSX,
            allow_importing_ts_extensions: Tristate::True,
            strict_null_checks: Tristate::True,
            strict_function_types: Tristate::True,
            source_map: Tristate::True,
            allow_non_ts_extensions: Tristate::True,
            resolve_json_module: Tristate::True,
            ..Default::default()
        })
    });
    p.command_line = Some(new_inferred_project_command_line(
        compiler_options,
        root_file_names,
        project_references,
        content_mappers,
        ComparePathsOptions {
            use_case_sensitive_file_names: builder.fs.fs.use_case_sensitive_file_names(),
            current_directory: current_directory.to_string(),
        },
        errors,
    ));
    p
}

// project.go:234
pub(crate) fn new_synthetic_project(
    id: &SyntheticProjectID,
    current_directory: &str,
    compiler_options: P<CompilerOptions>,
    root_file_names: Vec<String>,
    project_references: Vec<ProjectReference>,
    content_mappers: Vec<Mapper>,
    errors: Vec<P<Diagnostic>>,
    builder: &ProjectCollectionBuilder,
    logger: &LogTree,
) -> Project {
    let mut project = new_project(id.as_id(), Kind::Synthetic, current_directory, builder, logger);
    project.command_line = Some(new_inferred_project_command_line(
        compiler_options,
        root_file_names,
        project_references,
        content_mappers,
        ComparePathsOptions {
            use_case_sensitive_file_names: builder.fs.fs.use_case_sensitive_file_names(),
            current_directory: current_directory.to_string(),
        },
        errors,
    ));
    project
}

// project.go:258
// `errors` is the `Errors` slice Go assigns on the returned command line right after creating it (Rust's
// command line is immutable once allocated).
pub(crate) fn new_inferred_project_command_line(
    compiler_options: P<CompilerOptions>,
    root_file_names: Vec<String>,
    project_references: Vec<ProjectReference>,
    content_mappers: Vec<Mapper>,
    compare_paths_options: ComparePathsOptions,
    errors: Vec<P<Diagnostic>>,
) -> P<ParsedCommandLine> {
    let mut command_line = tsoptions::new_parsed_command_line(
        compiler_options,
        root_file_names,
        project_references,
        compare_paths_options,
    );
    command_line.parsed_config.content_mappers = content_mappers.into();
    command_line.errors = errors;
    P::new(command_line)
}

// project.go:270
fn new_project(
    id: ID,
    kind: Kind,
    current_directory: &str,
    builder: &ProjectCollectionBuilder,
    logger: &LogTree,
) -> Project {
    if !logger.is_nil() {
        logger.log(&format!(
            "Creating {}Project: {}, currentDirectory: {}",
            kind.string(),
            id,
            current_directory
        ));
    }
    let relative_pattern_support = lsproto::get_client_capabilities(&builder.ctx)
        .workspace
        .did_change_watched_files
        .relative_pattern_support;
    let use_case_sensitive_file_names = builder.fs.fs.use_case_sensitive_file_names();
    let program_files_watch = new_watched_files(
        format!("program files for {}", id.0),
        ALL_WATCH_KINDS,
        relative_pattern_support,
        create_resolution_lookup_glob_mapper(
            &builder.session_options.current_directory,
            &builder.session_options.default_library_path,
            current_directory,
            use_case_sensitive_file_names,
        ),
    );
    let typings_watch = if builder.session_options.typings_location.is_empty() {
        None
    } else {
        Some(new_watched_files(
            "typings installer files".to_string(),
            ALL_WATCH_KINDS,
            relative_pattern_support,
            Arc::new(|input: &PatternsAndIgnored| input.clone()),
        ))
    };
    let content_mapper_watch = new_watched_files_for_paths(
        format!("content mapper configuration files for {}", id.0),
        ALL_WATCH_KINDS,
        relative_pattern_support,
        &builder.session_options.current_directory,
        &builder.session_options.current_directory,
        use_case_sensitive_file_names,
    );
    Project {
        kind,
        id,
        current_directory: current_directory.to_string(),
        config_file_name: String::new(),
        config_file_path: Path::default(),
        dirty: true,
        dirty_file_path: Path::default(),
        host: None,
        command_line: None,
        command_line_with_typings_files: Arc::new(OnceLock::new()),
        program: None,
        program_update_kind: ProgramUpdateKind::None,
        program_last_update: 0,
        potential_project_references: None,
        program_files_watch: Some(program_files_watch),
        typings_watch,
        content_mapper_watch: Some(content_mapper_watch),
        content_mapper_watched_files: None,
        checker_pool: None,
        program_owner: None,
        module_resolver_factory: None,
        module_resolver_id: 0,
        installed_typings_info: None,
        typings_files: Vec::new(),
    }
}

// The result of `CreateProgram`.
pub(crate) struct CreateProgramResult {
    pub(crate) program: &'static Program,
    pub(crate) update_kind: ProgramUpdateKind,
    // Go reads the pool back with `program.GetCheckerPool().(*checkerPool)`; the program keeps the pool behind
    // `dyn CheckerPool`, so the factory hands the concrete pool out here.
    pub(crate) checker_pool: Option<Arc<CheckerPoolImpl>>,
    pub(crate) owner: Arc<ProgramOwner>,
}

impl Project {
    // project.go:312
    pub fn current_directory(&self) -> &str {
        &self.current_directory
    }

    // project.go:320
    // DisplayName returns a short, human-readable name for the project,
    // relative to the given workspace root directory.
    // For configured projects, this is the config file path made relative.
    // For inferred projects, this is the last component of the current directory.
    pub(crate) fn display_name(&self, cwd: &str) -> String {
        if self.kind == Kind::Inferred {
            return tspath::get_base_file_name(&self.current_directory);
        }
        let mut name = self.id().0;
        if self.kind == Kind::Configured {
            name = self.config_file_name().to_string();
        }
        tspath::convert_to_relative_path(
            &name,
            &ComparePathsOptions {
                current_directory: cwd.to_string(),
                use_case_sensitive_file_names: false,
            },
        )
    }

    // project.go:333
    pub fn id(&self) -> ID {
        self.id.clone()
    }

    // project.go:338
    // ConfigFileName panics if Kind() is not KindConfigured.
    pub fn config_file_name(&self) -> &str {
        assert!(self.kind == Kind::Configured, "ConfigFileName called on non-configured project");
        &self.config_file_name
    }

    // project.go:346
    // ConfigFilePath panics if Kind() is not KindConfigured.
    pub(crate) fn config_file_path(&self) -> &Path {
        assert!(self.kind == Kind::Configured, "ConfigFilePath called on non-configured project");
        &self.config_file_path
    }

    // project.go:353
    fn id_string(&self) -> String {
        self.id.0.clone()
    }

    // project.go:357
    pub fn get_program(&self) -> Option<&'static Program> {
        self.program
    }

    // project.go:361
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    // project.go:368
    // GetProjectDiagnostics returns program diagnostics combined with any global
    // diagnostics discovered during checking. These are the diagnostics reported on
    // the tsconfig.json file.
    pub fn get_project_diagnostics(&self, _ctx: &Context) -> Vec<P<Diagnostic>> {
        let mut global_diags = Vec::new();
        if let Some(checker_pool) = &self.checker_pool {
            global_diags = checker_pool.get_global_diagnostics();
        }
        let program = self.program.unwrap();
        let mut all = program.get_config_file_parsing_diagnostics();
        all.extend(program.get_program_diagnostics());
        all.extend(global_diags);
        sort_and_deduplicate_diagnostics(&all)
    }

    // project.go:380
    pub(crate) fn has_file(&self, file_name: &str) -> bool {
        self.contains_file(&self.to_path(file_name))
    }

    // project.go:384
    pub(crate) fn contains_file(&self, path: &Path) -> bool {
        self.program.is_some_and(|program| program.get_source_file_by_path(path).is_some())
    }

    // project.go:388
    pub(crate) fn is_source_from_project_reference(&self, path: &Path) -> bool {
        self.program.is_some_and(|program| program.is_source_from_project_reference(path))
    }

    // project.go:433
    // SetCommandLine reassigns the project's command line and resets all state derived
    // from it. Changing the command line always requires a full program rebuild, so the
    // project is marked fully dirty. It also resets:
    //   - the memoized command line augmented with typings files (and its sync.Once, so
    //     the augmented command line is rebuilt from the new command line on next access);
    //   - potentialProjectReferences, the pre-load placeholder derived from the old
    //     command line (always nil for inferred projects, which have no project references).
    pub(crate) fn set_command_line(&mut self, command_line: Option<P<ParsedCommandLine>>) {
        self.command_line = command_line;
        self.command_line_with_typings_files = Arc::new(OnceLock::new());
        self.potential_project_references = None;
        self.dirty = true;
        self.dirty_file_path = Path::default();
    }

    // project.go:443
    // getCommandLineWithTypingsFiles returns the command line augmented with typing files if ATA is enabled.
    fn get_command_line_with_typings_files(&self) -> Option<P<ParsedCommandLine>> {
        if self.typings_files.is_empty() {
            return self.command_line;
        }

        // Check if ATA is enabled for this project
        let type_acquisition = self.get_type_acquisition();
        if !type_acquisition.as_ref().is_some_and(|ta| ta.enable.is_true()) {
            return self.command_line;
        }

        let command_line = self.command_line.unwrap();
        Some(*self.command_line_with_typings_files.get_or_init(|| {
            // Create an augmented command line that includes typing files
            let original_root_names = command_line.file_names();
            let mut new_root_names =
                Vec::with_capacity(original_root_names.len() + self.typings_files.len());
            new_root_names.extend(original_root_names.iter().cloned());
            new_root_names.extend(self.typings_files.iter().cloned());

            P::new(command_line.with_file_names(new_root_names))
        }))
    }

    // project.go:468
    pub(crate) fn set_potential_project_reference(&mut self, config_file_path: Path) {
        let mut references = match &self.potential_project_references {
            None => Set::default(),
            Some(existing) => (**existing).clone(),
        };
        references.add(config_file_path);
        self.potential_project_references = Some(Arc::new(references));
    }

    // project.go:477
    pub(crate) fn has_potential_project_reference(
        &self,
        project_tree_request: &ProjectTreeRequest,
    ) -> bool {
        if let Some(command_line) = self.command_line {
            if command_line
                .resolved_project_reference_paths()
                .iter()
                .any(|path| project_tree_request.is_project_referenced(&self.to_path(path)))
            {
                return true;
            }
        } else if let Some(potential_project_references) = &self.potential_project_references {
            // pure membership any(); Go ranges the set too
            if potential_project_references
                .keys()
                .iter()
                .any(|path| project_tree_request.is_project_referenced(path))
            {
                return true;
            }
        }
        false
    }

    // project.go:499
    pub(crate) fn create_program(&self) -> CreateProgramResult {
        let host = self.host.clone().unwrap();
        let mut update_kind = ProgramUpdateKind::NewFiles;
        let mut program_cloned = false;
        let new_program_result: &'static Program;

        let pool_slot: Arc<Mutex<Option<Arc<CheckerPoolImpl>>>> = Arc::new(Mutex::new(None));
        let create_checker_pool: CreateCheckerPool = {
            let options = host.session_options.checker_pool_options;
            let pool_slot = Arc::clone(&pool_slot);
            Arc::new(move |program: &'static Program| -> Box<dyn CheckerPool> {
                let pool = new_checker_pool(options, program, None);
                *pool_slot.lock().unwrap() = Some(Arc::clone(&pool));
                Box::new(CheckerPoolHandle(pool))
            })
        };
        let cleanup_module_resolver: Arc<Mutex<Option<Box<dyn FnOnce() + Send>>>> =
            Arc::new(Mutex::new(None));
        let create_module_resolver: CreateModuleResolver = {
            let factory = self.module_resolver_factory.clone();
            let cleanup_module_resolver = Arc::clone(&cleanup_module_resolver);
            let ctx = host.builder_ctx();
            Arc::new(move |options: ResolverOptions<'static>| -> Box<dyn Resolver> {
                let Some(factory) = &factory else {
                    return Box::new(module::new_resolver(options));
                };
                let (resolver, cleanup) = factory.new_resolver(&ctx, options);
                *cleanup_module_resolver.lock().unwrap() = Some(cleanup);
                resolver
            })
        };

        // Create the command line, potentially augmented with typing files
        let command_line = self.get_command_line_with_typings_files();
        // Memory regions: what the new program version allocates lives in its own region (memregions.rs). The
        // command line above is memoized in the project and outlives the program, so it is made before.
        let region = Region::new(1 << 20);
        let region_scope = region.enter();
        let reuse = !self.dirty_file_path.0.is_empty()
            && self.program.is_some_and(|program| Some(program.command_line()) == command_line);
        if reuse {
            let old_program = self.program.unwrap();
            let (program, dirty_file, cloned) = old_program.update_program(
                &self.dirty_file_path,
                Arc::<CompilerHostImpl>::clone(&host),
                Some(create_checker_pool),
                Some(create_module_resolver),
            );
            new_program_result = program;
            program_cloned = cloned;
            let (parse_cache, journal) = host.builder_parse_cache_journal();
            if program_cloned {
                update_kind = ProgramUpdateKind::Cloned;
                for &file in new_program_result.source_files() {
                    // Use pointer identity: dirtyFile is the exact instance UpdateProgram acquired,
                    // and it is the only file whose refcount is already accounted for.
                    // (Content mappers are not ported: no file is a failure stub, supplemental or content-mapped.)
                    if Some(file) != dirty_file {
                        // UpdateProgram acquired the changed file only, so we need to ref everything else
                        journal.ref_(&parse_cache, parse_cache_key_for_file(file));
                    }
                }
                for file in new_program_result.duplicate_source_files() {
                    journal.ref_(&parse_cache, parse_cache_key_for_duplicate(file));
                }
            } else if let Some(dirty_file) = dirty_file {
                // UpdateProgram always acquires the dirty file before deciding whether it can
                // reuse the old program. If it falls back to a full rebuild, release that
                // speculative acquire so the rebuilt program is the only remaining owner.
                journal.deref(&parse_cache, &parse_cache_key_for_file(dirty_file));
            }
        } else {
            let mut typings_location = String::new();
            if self.get_type_acquisition().is_some_and(|ta| ta.enable.is_true()) {
                typings_location.clone_from(&host.session_options.typings_location);
            }
            let mut opts = ProgramOptions::new(command_line.unwrap(), host);
            opts.use_source_of_project_reference = true;
            opts.typings_location = typings_location;
            opts.create_checker_pool = Some(create_checker_pool);
            opts.create_module_resolver = Some(create_module_resolver);
            new_program_result = new_program(opts);
        }
        let cleanup = cleanup_module_resolver.lock().unwrap().take();
        if let Some(cleanup) = cleanup {
            cleanup();
        }

        if !program_cloned
            && self.program.is_some_and(|program| program.has_same_file_names(new_program_result))
        {
            update_kind = ProgramUpdateKind::SameFileNames;
        }

        new_program_result.bind_source_files();
        drop(region_scope);

        let checker_pool = pool_slot.lock().unwrap().take();
        let base = match (&self.program_owner, program_cloned) {
            (Some(old), true) => old.base.clone(),
            _ => region.clone(),
        };
        let owner = Arc::new(ProgramOwner::new(
            new_program_result,
            checker_pool.clone(),
            region,
            base,
            !program_cloned,
        ));
        if let Some(pool) = &checker_pool {
            pool.set_owner(Arc::downgrade(&owner));
        }
        CreateProgramResult { program: new_program_result, update_kind, checker_pool, owner }
    }

    // project.go:589
    pub(crate) fn clone_watchers(
        &self,
    ) -> Option<Arc<WatchedFiles<Option<Arc<SyncMap<Path, String>>>>>> {
        let seen_files = self.host.as_ref().unwrap().source_fs.seen_files();
        self.program_files_watch.as_ref().map(|w| w.clone_with(seen_files))
    }

    // project.go:593
    #[expect(
        dead_code,
        reason = "Go passes p.log to newCheckerPool (project.go:505); with this no-op body the Rust passes None"
    )]
    #[expect(
        clippy::unused_self,
        reason = "The Go project logging callback retains its receiver when logging is disabled."
    )]
    fn log(&self, _msg: &str) {
        // !!!
    }

    // project.go:597
    fn to_path(&self, file_name: &str) -> Path {
        tspath::to_path(
            file_name,
            &self.current_directory,
            tsrs_compiler::CompilerHost::fs(&**self.host.as_ref().unwrap())
                .use_case_sensitive_file_names(),
        )
    }

    // project.go:601
    pub(crate) fn print(
        &self,
        write_file_names: bool,
        _write_file_explanation: bool,
        builder: &mut String,
    ) -> String {
        let _ = write!(builder, "\nProject '{}'\n", self.id());
        match self.program {
            None => builder.push_str("\tFiles (0) NoProgram\n"),
            Some(program) => {
                let source_files = program.get_source_files();
                let _ = writeln!(builder, "\tFiles ({})", source_files.len());
                if write_file_names {
                    builder.extend(
                        source_files
                            .iter()
                            .flat_map(|source_file| ["\t\t", source_file.file_name(), "\n"]),
                    );
                    // !!!
                    // if writeFileExplanation {}
                }
            }
        }
        builder.push_str(HR);
        builder.clone()
    }

    // project.go:623
    // GetTypeAcquisition returns the type acquisition settings for this project.
    fn get_type_acquisition(&self) -> Option<TypeAcquisition> {
        if self.kind == Kind::Inferred || self.kind == Kind::Synthetic {
            // For inferred and synthetic projects, use default settings.
            return Some(TypeAcquisition {
                enable: Tristate::True,
                include: None,
                exclude: None,
                disable_filename_based_type_acquisition: Tristate::False,
            });
        }

        if let Some(command_line) = self.command_line {
            return command_line.type_acquisition().cloned();
        }

        None
    }

    // project.go:642
    // GetUnresolvedImports extracts unresolved imports from this project's program.
    fn get_unresolved_imports(&self) -> Option<Arc<Set<String>>> {
        let program = self.program?;
        Some(Arc::new(program.get_unresolved_imports().clone()))
    }

    // project.go:651
    // ShouldTriggerATA determines if ATA should be triggered for this project.

    // project.go:668
    pub(crate) fn compute_typings_info(&self) -> ata::TypingsInfo {
        ata::TypingsInfo {
            compiler_options: self.command_line.unwrap().compiler_options(),
            type_acquisition: self.get_type_acquisition(),
            unresolved_imports: self.get_unresolved_imports(),
        }
    }
}

impl Cloneable for Project {
    // project.go:392
    fn clone_value(&self) -> Project {
        Project {
            kind: self.kind,
            id: self.id.clone(),
            current_directory: self.current_directory.clone(),
            config_file_name: self.config_file_name.clone(),
            config_file_path: self.config_file_path.clone(),

            dirty: self.dirty,
            dirty_file_path: self.dirty_file_path.clone(),

            host: self.host.clone(),
            command_line: self.command_line,
            command_line_with_typings_files: {
                // Go copies the memoized value and starts a fresh sync.Once.
                let once = OnceLock::new();
                if let Some(v) = self.command_line_with_typings_files.get() {
                    let _ = once.set(*v);
                }
                Arc::new(once)
            },
            program: self.program,
            program_update_kind: ProgramUpdateKind::None,
            program_last_update: self.program_last_update,
            potential_project_references: self.potential_project_references.clone(),

            program_files_watch: self.program_files_watch.clone(),
            typings_watch: self.typings_watch.clone(),
            content_mapper_watch: self.content_mapper_watch.clone(),
            content_mapper_watched_files: self.content_mapper_watched_files.clone(),

            checker_pool: self.checker_pool.clone(),
            program_owner: self.program_owner.clone(),

            module_resolver_factory: self.module_resolver_factory.clone(),
            module_resolver_id: self.module_resolver_id,

            installed_typings_info: self.installed_typings_info.clone(),
            typings_files: self.typings_files.clone(),
        }
    }
}

// project.go:178 `var _ ls.Project = (*Project)(nil)`
impl ls::Project for Project {
    fn id(&self) -> String {
        self.id_string()
    }

    fn get_program(&self) -> Option<&'static Program> {
        self.program
    }

    fn has_file(&self, file_name: &str) -> bool {
        Project::has_file(self, file_name)
    }
}
