use std::collections::hash_map::Entry;
use std::sync::Arc;

use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::{self as ast, Node, SourceFile, SourceFileMetaData};
use tsrs_core::tspath::{self, Path};
use tsrs_core::{ModuleKind, P, Tristate};
use tsrs_diagnostics as diagnostics;
use tsrs_module::{
    DiagAndArgs, ModeAwareCache, PackageId, ResolvedModule, ResolvedTypeReferenceDirective,
};
use tsrs_tsoptions as tsoptions;

use crate::file_include::{FileIncludeKind, FileIncludeReason};
use crate::fileloader::{
    DuplicateSourceFile, FileLoader, JsxRuntimeImportSpecifier, LibFile, PrefetchedResolutions,
    ProcessedFiles, RedirectsFile, parse_options_for, prefetch_resolutions, source_file_meta_data,
};
use crate::includeprocessor::FileIncludeData;
use crate::processing_diagnostic::{IncludeExplainingDiagnostic, ProcessingDiagnostic};

pub type TaskId = usize;
pub type DataId = usize;

pub struct ParseTask {
    // Shared (like `path`) by the tasks of every reference to the same file name (`add_sub_task_normalized`).
    pub(crate) normalized_file_path: std::sync::Arc<str>,
    pub(crate) path: Path,
    pub(crate) file: Option<P<SourceFile>>,
    pub(crate) lib_file: Option<P<LibFile>>,
    redirected_parse_task: Option<TaskId>,
    pub(crate) sub_tasks: Vec<TaskId>,
    loaded: bool,
    started_sub_tasks: bool,
    pub(crate) is_for_automatic_type_directive: bool,
    is_content_mapper_supplemental: bool,
    pub(crate) failed_lookup: bool,
    // Set when the metadata was computed ahead of time for a parallel parse.
    metadata_loaded: bool,
    pub(crate) increase_depth: bool,
    pub(crate) elide_on_depth: bool,
    pub(crate) include_reason: Option<P<FileIncludeReason>>,
    pub(crate) package_id: PackageId,
    loaded_task: Option<TaskId>,
    // Everything only a loaded (or metadata-prefetched) task fills, allocated on the first write: most tasks are
    // one per import edge and only point at the task that loads their file (`loaded_task`, filesParser::start).
    data: Option<Box<ParseTaskLoaded>>,
    // The index in `filesParser::datas` of this task's path, `NO_DATA` until known: set by `add_sub_task_normalized`
    // when the path has a task already and by `start` always (a data is never removed), so neither `start` nor
    // `get_processed_files` hashes the path again (one hash of a 100-byte path per import edge was a tenth of the
    // sequential load).
    pub(crate) data_id: u32,
}

pub const NO_DATA: u32 = u32::MAX;

#[derive(Default)]
pub struct ParseTaskLoaded {
    metadata: SourceFileMetaData,
    pub(crate) prefetched_resolutions: Option<Box<PrefetchedResolutions>>,
    pub(crate) resolutions_in_file: ModeAwareCache<P<ResolvedModule>>,
    pub(crate) resolutions_trace: Vec<DiagAndArgs>,
    pub(crate) type_resolutions_in_file: ModeAwareCache<P<ResolvedTypeReferenceDirective>>,
    pub(crate) type_resolutions_trace: Vec<DiagAndArgs>,
    pub(crate) processing_diagnostics: Vec<ProcessingDiagnostic>,
    pub(crate) import_helpers_import_specifier: Option<P<Node>>,
    pub(crate) jsx_runtime_import_specifier: Option<P<JsxRuntimeImportSpecifier>>,
    // The task's path as the `FileIncludeReason`s of its references store it: one copy per file, not per reference.
    reason_path: Option<&'static str>,
}

impl ParseTask {
    pub(crate) fn new(normalized_file_path: impl Into<std::sync::Arc<str>>) -> ParseTask {
        ParseTask::with_path(normalized_file_path, Path::default())
    }

    /// A task whose path is known when it is created. The sub tasks (one per import edge, vscode: 110k in the
    /// sequential load) are made with it: an empty `Path` placeholder is a clone of the one static empty `Arc<str>`,
    /// and that refcount update and the placeholder's drop waited for the loop's stores (notes/perf-serial-load.md).
    pub(crate) fn with_path(
        normalized_file_path: impl Into<std::sync::Arc<str>>,
        path: Path,
    ) -> ParseTask {
        ParseTask {
            normalized_file_path: normalized_file_path.into(),
            path,
            file: None,
            lib_file: None,
            redirected_parse_task: None,
            sub_tasks: Vec::new(),
            loaded: false,
            started_sub_tasks: false,
            is_for_automatic_type_directive: false,
            is_content_mapper_supplemental: false,
            failed_lookup: false,
            metadata_loaded: false,
            increase_depth: false,
            elide_on_depth: false,
            include_reason: None,
            package_id: PackageId::default(),
            loaded_task: None,
            data: None,
            data_id: NO_DATA,
        }
    }

    /// The task's data index when known (`data_id`).
    #[inline]
    fn data_hint(&self) -> Option<DataId> {
        (self.data_id != NO_DATA).then_some(self.data_id as DataId)
    }

    /// The loaded-task data, created on first use.
    pub(crate) fn data(&mut self) -> &mut ParseTaskLoaded {
        self.data.get_or_insert_with(Default::default)
    }

    fn data_ref(&self) -> Option<&ParseTaskLoaded> {
        self.data.as_deref()
    }

    /// The file's metadata (the default before `load_metadata` or the prefetch set it).
    pub(crate) fn metadata(&self) -> SourceFileMetaData {
        self.data_ref().map(|d| d.metadata.clone()).unwrap_or_default()
    }

    fn take_processing_diagnostics(&mut self) -> Vec<ProcessingDiagnostic> {
        self.data
            .as_mut()
            .map(|d| std::mem::take(&mut d.processing_diagnostics))
            .unwrap_or_default()
    }

    /// `self.path` as an arena string shared by every include reason this task creates.
    pub(crate) fn reason_path(&mut self) -> &'static str {
        let path = &self.path;
        let data = self.data.get_or_insert_with(Default::default);
        data.reason_path.get_or_insert_with(|| tsrs_core::alloc_str(path.as_str()))
    }
}

// Whether a task's file would go through the parser when loaded (as opposed to being skipped
// because of an unsupported extension, a failed lookup, or being the automatic type directive task).
fn task_needs_parse(loader: &FileLoader, t: TaskId) -> bool {
    let task = &loader.tasks[t];
    if task.loaded
        || task.is_for_automatic_type_directive
        || task.failed_lookup
        || task.file.is_some()
    {
        return false;
    }
    if !loader
        .project_references
        .get_parse_file_redirect(&task.normalized_file_path, &task.path)
        .is_empty()
    {
        return false;
    }
    if tspath::has_extension(&task.normalized_file_path) {
        let allow_non_ts_extensions =
            loader.opts.config.compiler_options().unwrap().allow_non_ts_extensions.is_true();
        if !allow_non_ts_extensions {
            let canonical_file_name = tspath::get_canonical_file_name(
                &task.normalized_file_path,
                loader.host.fs().use_case_sensitive_file_names(),
            );
            if !loader.is_supported_extension(&canonical_file_name) {
                return false;
            }
        }
    }
    true
}

fn load(t: TaskId, loader: &mut FileLoader) {
    loader.tasks[t].loaded = true;
    if loader.tasks[t].is_for_automatic_type_directive {
        load_automatic_type_directives(t, loader);
        return;
    }
    if loader.tasks[t].failed_lookup {
        // The root file name did not resolve to a supported extension; the task
        // exists only to carry its processing diagnostic, so nothing is parsed.
        return;
    }
    let redirect = {
        let task = &loader.tasks[t];
        loader.project_references.get_parse_file_redirect(&task.normalized_file_path, &task.path)
    };
    if !redirect.is_empty() {
        redirect_task(t, loader, &redirect);
        return;
    }

    // A prefetched file passed these checks in task_needs_parse (the same conditions); only an unparsed task repeats
    // them here, where a failure becomes the task's diagnostic.
    if !loader.tasks[t].is_content_mapper_supplemental
        && loader.tasks[t].file.is_none()
        && tspath::has_extension(&loader.tasks[t].normalized_file_path)
    {
        let compiler_options = loader.opts.config.compiler_options().unwrap();
        let allow_non_ts_extensions = compiler_options.allow_non_ts_extensions.is_true();
        if !allow_non_ts_extensions {
            let normalized_file_path = loader.tasks[t].normalized_file_path.to_string();
            let canonical_file_name = tspath::get_canonical_file_name(
                &normalized_file_path,
                loader.host.fs().use_case_sensitive_file_names(),
            );
            if !loader.is_supported_extension(&canonical_file_name) {
                let include_reason = loader.tasks[t].include_reason;
                if tspath::has_js_file_extension(&canonical_file_name) {
                    loader.tasks[t].data().processing_diagnostics.push(ProcessingDiagnostic::explaining(IncludeExplainingDiagnostic {
                        file: None,
                        diagnostic_reason: include_reason,
                        message: &diagnostics::FILE_0_IS_A_JAVA_SCRIPT_FILE_DID_YOU_MEAN_TO_ENABLE_THE_ALLOW_JS_OPTION,
                        args: vec![normalized_file_path],
                    }));
                } else {
                    let flat: Vec<&str> = loader
                        .supported_extensions
                        .iter()
                        .flatten()
                        .map(std::string::String::as_str)
                        .collect();
                    loader.tasks[t].data().processing_diagnostics.push(ProcessingDiagnostic::explaining(IncludeExplainingDiagnostic {
                        file: None,
                        diagnostic_reason: include_reason,
                        message: &diagnostics::FILE_0_HAS_AN_UNSUPPORTED_EXTENSION_THE_ONLY_SUPPORTED_EXTENSIONS_ARE_1,
                        args: vec![normalized_file_path, format!("'{}'", flat.join("', '"))],
                    }));
                }
                return;
            }
        }
    }

    loader.total_file_count += 1;
    load_metadata(t, loader);

    let mut file = loader.tasks[t].file;
    if file.is_none() {
        file = loader.parse_source_file(t);
    }
    let Some(file) = file else {
        return;
    };

    loader.tasks[t].file = Some(file);
    apply_virtual_file_name(&mut loader.tasks[t].data().metadata, file);
    loader.tasks[t].sub_tasks = Vec::with_capacity(
        file.referenced_files.get().len()
            + file.imports().len()
            + file.module_augmentations.get().len(),
    );
    // The prefetch's include reasons name the task by this string; every reason of the task shares it.
    if let Some(reason_path) = loader.tasks[t]
        .data_ref()
        .and_then(|d| d.prefetched_resolutions.as_ref())
        .and_then(|p| p.reason_path)
    {
        loader.tasks[t].data().reason_path.get_or_insert(reason_path);
    }

    let compiler_options = loader.opts.config.compiler_options().unwrap();
    if !compiler_options.no_resolve.is_true() && !loader.opts.skip_module_resolution {
        let mut prefetched = loader.tasks[t]
            .data()
            .prefetched_resolutions
            .as_mut()
            .map(|p| std::mem::take(&mut p.referenced_files).into_iter());
        for (index, ref_) in file.referenced_files.get().iter().enumerate() {
            let prefetched_lookup = prefetched
                .as_mut()
                .map(|p| p.next().expect("prefetched triple-slash reference lookup"));
            match loader.resolve_tripleslash_path_reference(
                &ref_.file_name,
                file.file_name(),
                index,
                prefetched_lookup,
            ) {
                Err(processing_diagnostic) => {
                    loader.tasks[t].data().processing_diagnostics.push(processing_diagnostic);
                }
                Ok(resolved_ref) => loader.add_sub_task(t, &resolved_ref, None),
            }
        }

        loader.resolve_type_reference_directives(t);
    }

    if compiler_options.no_lib != Tristate::True && !loader.opts.skip_module_resolution {
        for (index, lib) in file.lib_reference_directives.get().iter().enumerate() {
            let include_reason = FileIncludeReason::new_referenced(
                FileIncludeKind::LibReferenceDirective,
                loader.tasks[t].reason_path(),
                Some(index),
                None,
            );
            if let Some(name) = tsoptions::get_lib_file_name(&lib.file_name) {
                let lib_file = loader.path_for_lib_file(&name);
                loader.add_sub_task(
                    t,
                    &ResolvedRef {
                        file_name: std::borrow::Cow::Owned(lib_file.path.clone()),
                        increase_depth: false,
                        elide_on_depth: false,
                        include_reason,
                        package_id: PackageId::default(),
                    },
                    Some(lib_file),
                );
            } else {
                loader.tasks[t]
                    .data()
                    .processing_diagnostics
                    .push(ProcessingDiagnostic::unknown_reference(include_reason));
            }
        }
    }

    loader.resolve_imports_and_module_augmentations(t);
    // filesparser.go:172
    for &supplemental in file.supplemental_source_files() {
        let mut sub_task = ParseTask::new(supplemental.file_name());
        sub_task.file = Some(supplemental);
        sub_task.is_content_mapper_supplemental = true;
        let mut include_reason = FileIncludeReason::new(FileIncludeKind::ContentMapperSupplemental);
        include_reason.canonical_source_file = Some(P::new(loader.tasks[t].path.clone()));
        sub_task.include_reason = Some(P::new(include_reason));
        loader.tasks.push(sub_task);
        let id = loader.tasks.len() - 1;
        loader.tasks[t].sub_tasks.push(id);
    }
}

// filesparser.go:126: a content-mapped file's module format follows its virtual file name. The parse ahead
// (`prefetch`) applies it before resolving the file's imports, as Go's load does; applying it again is a no-op.
fn apply_virtual_file_name(metadata: &mut SourceFileMetaData, file: P<SourceFile>) {
    let virtual_file_name = file.virtual_file_name();
    if !virtual_file_name.is_empty() {
        metadata.implied_node_format =
            ast::get_implied_node_format_for_file(virtual_file_name, &metadata.package_json_type);
    }
}

fn load_metadata(t: TaskId, loader: &mut FileLoader) {
    if loader.tasks[t].metadata_loaded {
        return;
    }
    loader.tasks[t].metadata_loaded = true;
    if loader.tasks[t].lib_file.is_some() {
        loader.lib_file_count += 1;
        // Default lib files are all scripts; we can safely skip looking up their package.json
        // to avoid adding spurious lookups to file watcher tracking.
        loader.tasks[t].data().metadata =
            SourceFileMetaData { implied_node_format: ModuleKind::COMMON_JS, ..Default::default() };
    } else {
        let metadata = loader.load_source_file_meta_data(&loader.tasks[t].normalized_file_path);
        loader.tasks[t].data().metadata = metadata;
    }
}

fn redirect_task(t: TaskId, loader: &mut FileLoader, file_name: &str) {
    let mut redirected = ParseTask::new(tspath::normalize_path(file_name));
    redirected.lib_file = loader.tasks[t].lib_file;
    redirected.include_reason = loader.tasks[t].include_reason;
    loader.tasks.push(redirected);
    let id = loader.tasks.len() - 1;
    loader.tasks[t].redirected_parse_task = Some(id);
    // increaseDepth and elideOnDepth are not copied to redirects, otherwise their depth would be double counted.
    loader.tasks[t].sub_tasks = vec![id];
}

fn load_automatic_type_directives(t: TaskId, loader: &mut FileLoader) {
    let normalized_file_path = loader.tasks[t].normalized_file_path.to_string();
    let (to_parse_type_refs, type_resolutions_in_file, type_resolutions_trace, p_diagnostics) =
        loader.resolve_automatic_type_directives(&normalized_file_path);
    let data = loader.tasks[t].data();
    data.type_resolutions_in_file = type_resolutions_in_file;
    data.type_resolutions_trace = type_resolutions_trace;
    data.processing_diagnostics.extend(p_diagnostics);
    for type_resolution in to_parse_type_refs {
        loader.add_sub_task(t, &type_resolution, None);
    }
}

pub struct ResolvedRef {
    // Borrowed for a resolved module (an arena string); owned for libs and triple-slash references.
    pub(crate) file_name: std::borrow::Cow<'static, str>,
    pub(crate) increase_depth: bool,
    pub(crate) elide_on_depth: bool,
    pub(crate) include_reason: P<FileIncludeReason>,
    pub(crate) package_id: PackageId,
}

pub struct ParseTaskData {
    // map of tasks by file casing
    pub(crate) tasks: TasksByCasing,
    lowest_depth: Option<u32>,
    started_sub_tasks: bool,
    package_id: PackageId,
}

/// Go's `map[string]*parseTask` of a file's casings, in insertion order (nearly always one entry; a map per
/// file cost more than the tasks it indexes).
#[derive(Default)]
pub struct TasksByCasing(Vec<(std::sync::Arc<str>, TaskId)>);

impl TasksByCasing {
    pub(crate) fn get_key_value(&self, casing: &str) -> Option<(&std::sync::Arc<str>, TaskId)> {
        // The tasks of a file name share one string (`add_sub_task_normalized`): the address answers without reading it.
        let same = |name: &str| {
            (name.as_ptr() == casing.as_ptr() && name.len() == casing.len()) || name == casing
        };
        self.0.iter().find(|(name, _)| same(name)).map(|(name, task)| (name, *task))
    }
    fn get(&self, casing: &str) -> Option<TaskId> {
        self.get_key_value(casing).map(|(_, task)| task)
    }
    fn contains_key(&self, casing: &str) -> bool {
        self.get_key_value(casing).is_some()
    }
    /// Callers insert only a casing that is not present yet.
    fn insert(&mut self, casing: std::sync::Arc<str>, task: TaskId) {
        debug_assert!(!self.contains_key(&casing));
        self.0.push((casing, task));
    }
    fn values(&self) -> impl Iterator<Item = &TaskId> {
        self.0.iter().map(|(_, task)| task)
    }
}

#[derive(Clone, Copy)]
struct QueuedTask {
    task: TaskId,
    loaded: bool,
    data: DataId,
    depth: u32,
}

#[derive(Clone, Copy)]
enum NodeModulesDepthLimit {
    BelowRoot,
    Depth(u32),
}

impl NodeModulesDepthLimit {
    fn from_option(value: i64) -> Self {
        if value < 0 {
            Self::BelowRoot
        } else {
            Self::Depth(u32::try_from(value).unwrap_or(u32::MAX))
        }
    }

    fn includes(self, depth: u32) -> bool {
        matches!(self, Self::Depth(limit) if depth <= limit)
    }
}

// Go queues one closure per task on a work group and runs them concurrently. Here the queue is
// drained in rounds: every file that a round will load is first parsed in parallel, then the
// round's closures run sequentially. The resulting task graph (and therefore the file order
// computed by getProcessedFiles) is the same fixed point Go reaches.
pub struct FilesParser {
    pub(crate) task_data_by_path: FxHashMap<Path, DataId>,
    pub(crate) datas: Vec<ParseTaskData>,
    queue: Vec<QueuedTask>,
    max_depth: NodeModulesDepthLimit,
    single_threaded: bool,
    // Files parsed ahead of the round that loads them, by path (see `prefetch`).
    speculative: FxHashMap<Path, SpeculativeParse>,
    speculated: bool,
}

// A file parsed, and its references resolved, before a task for it exists: what `prefetch` would compute for the
// task once a later round queues it.
struct SpeculativeParse {
    file_name: String,
    metadata: SourceFileMetaData,
    file: Option<P<SourceFile>>,
    resolutions: Option<Box<PrefetchedResolutions>>,
}

// The shared state of one speculative walk (`prefetch`).
struct Speculation<'a> {
    ctx: &'a crate::fileloader::PrefetchContext<'a>,
    // The files a speculative job claimed, beyond those with a task already (`ctx.task_data_by_path`, read-only during
    // the walk). Sharded: one mutex for all of them serialized the workers on vscode's 110k import edges at 64 threads.
    claimed: tsrs_core::collections::SyncSet<Path>,
    results: std::sync::Mutex<Vec<(Path, SpeculativeParse)>>,
}

impl Speculation<'_> {
    // The files that loading `file` adds as sub tasks which are certainly loaded and parsed in turn: triple-slash
    // references and type reference directives that resolved, and imports that `resolved_import_sub_task` adds and
    // does not elide by depth. Lib reference directives and the synthetic helper/JSX imports are left to the loader.
    fn sub_tasks(
        &self,
        file: P<SourceFile>,
        resolutions: &PrefetchedResolutions,
    ) -> Vec<(String, Path)> {
        let ctx = self.ctx;
        let host = ctx.host;
        let to_path = |name: &str| {
            tspath::to_path(
                name,
                host.get_current_directory(),
                host.fs().use_case_sensitive_file_names(),
            )
        };
        let compiler_options = ctx.opts.config.compiler_options().unwrap();
        let mut out = Vec::new();
        if !compiler_options.no_resolve.is_true() && !ctx.opts.skip_module_resolution {
            out.extend(resolutions.referenced_files.iter().filter_map(|(name, diagnostic)| {
                if diagnostic.is_none() {
                    let name = tspath::normalize_path(name);
                    let path = to_path(&name);
                    Some((name, path))
                } else {
                    None
                }
            }));
            out.extend(resolutions.type_references.iter().filter_map(|(resolved, _)| {
                if resolved.is_resolved() {
                    let name = tspath::normalize_path(resolved.resolved_file_name);
                    let path = to_path(&name);
                    Some((name, path))
                } else {
                    None
                }
            }));
        }
        let imports = file.imports();
        out.extend(resolutions.imports.iter().take(imports.len()).filter_map(|entry| {
            let prepared = entry.as_ref().and_then(|entry| entry.prepared.as_ref())?;
            if prepared.elide_on_depth {
                None
            } else {
                Some((prepared.normalized_file_path.to_string(), prepared.path.clone()))
            }
        }));
        out
    }

    // Whether a sub task's file goes through the parser when loaded (`task_needs_parse`), and no other task or
    // speculative job has it.
    fn claim(&self, file_name: &str, path: &Path) -> bool {
        let ctx = self.ctx;
        if ctx.task_data_by_path.contains_key(path) {
            return false;
        }
        if !tsrs_core::festats::timed_counted(
            tsrs_core::festats::Cat::LockClaim,
            tsrs_core::festats::Cat::ClaimOps,
            || self.claimed.add_if_absent(path.clone()),
        ) {
            return false;
        }
        // A bundled lib file can also be loaded as a lib (with lib metadata) through a lib reference.
        if tspath::starts_with_directory(
            file_name,
            ctx.host.default_library_path(),
            ctx.host.fs().use_case_sensitive_file_names(),
        ) {
            return false;
        }
        if !ctx.project_references.get_parse_file_redirect(file_name, path).is_empty() {
            return false;
        }
        // A content-mapped file is transformed by an external mapper, and a failed transform counts towards the
        // mapper's failure budget: it is parsed only by the round that loads it, never ahead of it.
        if tspath::file_extension_is_one_of(
            file_name,
            &crate::fileloader::str_slice(ctx.content_mapper_extensions),
        ) {
            return false;
        }
        if tspath::has_extension(file_name)
            && !ctx.opts.config.compiler_options().unwrap().allow_non_ts_extensions.is_true()
        {
            let canonical_file_name = tspath::get_canonical_file_name(
                file_name,
                ctx.host.fs().use_case_sensitive_file_names(),
            );
            if !crate::fileloader::is_supported_extension(
                ctx.supported_extensions_with_json,
                &canonical_file_name,
            ) {
                return false;
            }
        }
        true
    }

    fn spawn_sub_tasks<'s>(
        &'s self,
        scope: &rayon::Scope<'s>,
        file: P<SourceFile>,
        resolutions: &PrefetchedResolutions,
    ) {
        for (file_name, path) in self.sub_tasks(file, resolutions) {
            if self.claim(&file_name, &path) {
                scope.spawn(move |scope| self.run(scope, file_name, path));
            }
        }
    }

    fn run<'s>(&'s self, scope: &rayon::Scope<'s>, file_name: String, path: Path) {
        use tsrs_core::festats::{self, Cat};

        let ctx = self.ctx;
        let _regions = crate::programregions::enter(ctx.opts.regions.as_deref());
        let job_start = festats::enabled().then(std::time::Instant::now);
        let metadata = festats::timed(Cat::Meta, || {
            source_file_meta_data(ctx.opts, ctx.resolver, ctx.project_references, &file_name)
        });
        let file = ctx.source_file_parser().parse_source_file(parse_options_for(
            ctx.host,
            ctx.project_references,
            &file_name,
            &path,
            &metadata,
        ));
        festats::timed(Cat::Bind, || file.map(crate::fileregions::bind));
        let resolutions = festats::timed(Cat::Resolve, || {
            file.map(|file| Box::new(prefetch_resolutions(ctx, file, &metadata)))
        });
        if let (Some(file), Some(resolutions)) = (file, &resolutions) {
            self.spawn_sub_tasks(scope, file, resolutions);
        }
        self.results
            .lock()
            .unwrap()
            .push((path, SpeculativeParse { file_name, metadata, file, resolutions }));
        if let Some(start) = job_start {
            let wall = start.elapsed().as_nanos() as u64;
            festats::add(Cat::JobWall, wall);
            festats::add_max(Cat::JobMax, wall);
            festats::add(Cat::Jobs, 1);
        }
    }
}

impl FilesParser {
    pub(crate) fn new(single_threaded: bool, max_depth: i64) -> FilesParser {
        FilesParser {
            task_data_by_path: FxHashMap::default(),
            datas: Vec::new(),
            queue: Vec::new(),
            max_depth: NodeModulesDepthLimit::from_option(max_depth),
            single_threaded,
            speculative: FxHashMap::default(),
            speculated: false,
        }
    }

    pub(crate) fn parse(loader: &mut FileLoader, tasks: &[TaskId]) {
        // One entry per root file (vscode: 10.4k); the map is only probed, never iterated.
        loader.files_parser.task_data_by_path.reserve(tasks.len());
        loader.files_parser.datas.reserve(tasks.len());
        Self::start(loader, tasks, 0);
        // Go's single-threaded work group runs the queued closures last-queued first (core/workgroup.go), a depth-first
        // order that the rounds below do not keep. The program's files do not depend on it, but two outputs do, and
        // for them the load runs Go's order:
        // - which files a failing content mapper reports before the failure budget disables it (fileloader.go:681,
        //   the tsc contentMapperFailures scenarios), single-threaded;
        // - `--traceResolution`, at any checker count: a lookup traces as served "according to earlier cached
        //   lookups" when an earlier resolution made it. Go's parallel order is a race; its single-threaded order is
        //   the one deterministic reference. Traced resolutions run sequentially anyway (`prefetch`).
        let trace_resolution =
            loader.opts.config.compiler_options().unwrap().trace_resolution.is_true();
        if trace_resolution
            || (loader.files_parser.single_threaded && !loader.content_mapper_extensions.is_empty())
        {
            while let Some(item) = loader.files_parser.queue.pop() {
                Self::run_queued(loader, item);
            }
            return;
        }
        loop {
            let round = std::mem::take(&mut loader.files_parser.queue);
            if round.is_empty() {
                break;
            }
            tsrs_core::phases::count(
                loader.opts.config.compiler_options().unwrap().diagnostics_enabled(),
                "Program:   loader rounds",
                1,
            );
            Self::prefetch(loader, &round);
            let start = tsrs_core::phases::Timer::new(
                loader.opts.config.compiler_options().unwrap().diagnostics_enabled(),
            );
            for item in round {
                Self::run_queued(loader, item);
            }
            start.record("Program:   sequential load");
        }
        if loader.files_parser.speculated && !loader.files_parser.speculative.is_empty() {
            tsrs_core::phases::count(
                loader.opts.config.compiler_options().unwrap().diagnostics_enabled(),
                "Program:   parsed ahead, unused",
                loader.files_parser.speculative.len() as u64,
            );
        }
    }

    fn start(loader: &mut FileLoader, tasks: &[TaskId], depth: u32) {
        for &task in tasks {
            // A sub task created from a prefetched resolution already carries this path.
            if loader.tasks[task].path.as_str().is_empty() {
                loader.tasks[task].path = loader.to_path(&loader.tasks[task].normalized_file_path);
            }
            let w = &mut loader.files_parser;
            let known = match loader.tasks[task].data_hint() {
                Some(data) => Some(data),
                None => w.task_data_by_path.get(&loader.tasks[task].path).copied(),
            };
            let (data, loaded) = if let Some(data) = known {
                (data, true)
            } else {
                let mut tasks = TasksByCasing::default();
                tasks.insert(Arc::clone(&loader.tasks[task].normalized_file_path), task);
                w.datas.push(ParseTaskData {
                    tasks,
                    lowest_depth: None,
                    started_sub_tasks: false,
                    package_id: PackageId::default(),
                });
                let id = w.datas.len() - 1;
                w.task_data_by_path.insert(loader.tasks[task].path.clone(), id);
                (id, false)
            };
            loader.tasks[task].data_id = data as u32;
            w.queue.push(QueuedTask { task, loaded, data, depth });
        }
    }

    fn run_queued(loader: &mut FileLoader, item: QueuedTask) {
        let QueuedTask { task, loaded, data, depth } = item;
        let mut start_subtasks = false;
        if loaded {
            // The name is cloned only when it is new: nearly every queued task (vscode: 110k) names a known one.
            if let Some(existing_task) =
                loader.files_parser.datas[data].tasks.get(&loader.tasks[task].normalized_file_path)
            {
                loader.tasks[task].loaded_task = Some(existing_task);
            } else {
                let casing = Arc::clone(&loader.tasks[task].normalized_file_path);
                loader.files_parser.datas[data].tasks.insert(casing, task);
                // This is new task for file name - so load subtasks if there was loading for any other casing
                start_subtasks = loader.files_parser.datas[data].started_sub_tasks;
            }
        }

        // Propagate packageId to data if we have one and data doesn't yet
        if loader.files_parser.datas[data].package_id.name.is_empty()
            && !loader.tasks[task].package_id.name.is_empty()
        {
            loader.files_parser.datas[data].package_id = loader.tasks[task].package_id;
        }

        let current_depth = if loader.tasks[task].increase_depth { depth + 1 } else { depth };
        let lowered = loader.files_parser.datas[data]
            .lowest_depth
            .is_none_or(|lowest_depth| current_depth < lowest_depth);
        if lowered {
            // If we're seeing this task at a lower depth than before,
            // reprocess its subtasks to ensure they are loaded.
            loader.files_parser.datas[data].lowest_depth = Some(current_depth);
            start_subtasks = true;
            loader.files_parser.datas[data].started_sub_tasks = true;
        }

        if loader.tasks[task].elide_on_depth
            && !loader.files_parser.max_depth.includes(current_depth)
        {
            return;
        }

        // The data's casings, by index: loading and starting sub tasks add tasks and datas but no casing to this data
        // (only a later `run_queued` of one of its tasks does), so the list stays as it is (one entry, nearly always).
        for index in 0..loader.files_parser.datas[data].tasks.0.len() {
            let task_by_file_name = loader.files_parser.datas[data].tasks.0[index].1;
            let mut load_sub_tasks = start_subtasks;
            if !loader.tasks[task_by_file_name].loaded {
                load(task_by_file_name, loader);
                if loader.tasks[task_by_file_name].redirected_parse_task.is_some() {
                    // Always load redirected task
                    load_sub_tasks = true;
                    loader.files_parser.datas[data].started_sub_tasks = true;
                }
            }
            if load_sub_tasks && (lowered || !loader.tasks[task_by_file_name].started_sub_tasks) {
                loader.tasks[task_by_file_name].started_sub_tasks = true;
                // `start` reads the sub tasks themselves, never this task's list; it is lent out and put back.
                let sub_tasks = std::mem::take(&mut loader.tasks[task_by_file_name].sub_tasks);
                let lowest_depth = loader.files_parser.datas[data]
                    .lowest_depth
                    .expect("started task data has a depth");
                Self::start(loader, &sub_tasks, lowest_depth);
                loader.tasks[task_by_file_name].sub_tasks = sub_tasks;
            }
        }
    }

    // Before a round's sequential load: adopts what an earlier round's speculative walk parsed, then, when at least
    // two files remain and not --singleThreaded, in parallel computes the metadata of every remaining file (lib files
    // get the default), parses and binds it and (unless --traceResolution) resolves its references, type reference
    // directives and imports. The first parallel round also walks ahead, parsing, binding and resolving the files those
    // certainly load (not with --traceResolution, project references or libReplacement). `load` falls back to doing
    // the work on the spot for anything not prefetched.
    fn prefetch(loader: &mut FileLoader, round: &[QueuedTask]) {
        let mut planned: FxHashSet<(DataId, std::sync::Arc<str>)> = FxHashSet::default();
        let mut to_parse: Vec<TaskId> = Vec::new();
        for item in round {
            let task = &loader.tasks[item.task];
            let current_depth = if task.increase_depth { item.depth + 1 } else { item.depth };
            if task.elide_on_depth && !loader.files_parser.max_depth.includes(current_depth) {
                continue;
            }
            let data = &loader.files_parser.datas[item.data];
            let unlisted =
                (!data.tasks.contains_key(&task.normalized_file_path)).then_some(item.task);
            // `task_needs_parse` before the key: it is false for a loaded task, and most of a later round's queued
            // sub tasks name files that an earlier round loaded (vscode's round 2: ~110k of them for 16 files).
            to_parse.extend(data.tasks.values().copied().chain(unlisted).filter(|&candidate| {
                task_needs_parse(loader, candidate)
                    && planned.insert((
                        item.data,
                        Arc::clone(&loader.tasks[candidate].normalized_file_path),
                    ))
            }));
        }
        // The sub tasks the round's loads will append to `loader.tasks` (at most), reserved at once: on vscode the first
        // sequential load appends ~110k tasks, and growing the vector by doubling copied and faulted it repeatedly.
        let mut sub_tasks = 0;
        // Files that an earlier round's speculative walk parsed. The walk only takes files that are certainly loaded
        // as non-lib tasks under this name, so its metadata, file and resolutions are what this round would compute.
        if !loader.files_parser.speculative.is_empty() {
            let mut rest = Vec::with_capacity(to_parse.len());
            for t in to_parse {
                let task = &loader.tasks[t];
                let hit = task.lib_file.is_none()
                    && loader
                        .files_parser
                        .speculative
                        .get(&task.path)
                        .is_some_and(|s| *s.file_name == *task.normalized_file_path);
                if !hit {
                    rest.push(t);
                    continue;
                }
                let speculative =
                    loader.files_parser.speculative.remove(&task.path.clone()).unwrap();
                let task = &mut loader.tasks[t];
                task.data().metadata = speculative.metadata;
                task.metadata_loaded = true;
                task.file = speculative.file;
                if let Some(resolutions) = speculative.resolutions {
                    sub_tasks += resolutions.sub_task_bound();
                    task.data().prefetched_resolutions = Some(resolutions);
                }
            }
            to_parse = rest;
        }
        if to_parse.len() < 2 || loader.files_parser.single_threaded {
            loader.tasks.reserve(sub_tasks);
            return;
        }
        // Each job computes the file's metadata, parses it and, unless resolution traces are requested, resolves
        // its imports and type reference directives (Go does all of this per task in parallel). Traces stay
        // sequential: they say whether a lookup was served from a cache, which depends on the resolution order.
        let resolve_ahead =
            !loader.opts.config.compiler_options().unwrap().trace_resolution.is_true();
        // The first parallel round also walks ahead: each parsed file's sub tasks that will certainly be loaded are
        // parsed and resolved on the spot, recursively, instead of one dependency level per round (Go has no
        // rounds; it queues each sub task as soon as it is found). Only files the program loads are touched, so the
        // resolver and package.json caches end up with the same entries. Not with project references (their
        // redirect lookups record symlink state) or lib replacement (a lib file can live in node_modules).
        let speculate = resolve_ahead
            && !loader.files_parser.speculated
            && loader.opts.config.resolved_project_reference_paths().is_empty()
            && !loader.opts.config.compiler_options().unwrap().lib_replacement.is_true();
        loader.files_parser.speculated = true;
        // The last flag: a root file of the program (fileregions.rs `parse_task`).
        let jobs: Vec<(TaskId, Arc<str>, Path, bool, bool)> = to_parse
            .into_iter()
            .map(|t| {
                let task = &loader.tasks[t];
                (
                    t,
                    Arc::clone(&task.normalized_file_path),
                    task.path.clone(),
                    task.lib_file.is_some(),
                    task.include_reason.is_some_and(|r| r.is_root_file()),
                )
            })
            .collect();
        let ctx = loader.prefetch_context();
        let spec = Speculation {
            ctx: &ctx,
            claimed: tsrs_core::collections::SyncSet::default(),
            results: std::sync::Mutex::new(Vec::new()),
        };
        let (opts, host, resolver, project_references) =
            (ctx.opts, ctx.host, ctx.resolver, ctx.project_references);
        let file_parser = ctx.source_file_parser();
        let stats = tsrs_core::festats::enabled();
        let timings = loader.opts.config.compiler_options().unwrap().diagnostics_enabled() || stats;
        let parse_start = tsrs_core::phases::Timer::new(timings);
        let cpu_before: Vec<f64> = if stats {
            crate::program::worker_pool().broadcast(|_| crate::checkerpool::thread_cpu_seconds())
        } else {
            Vec::new()
        };
        let prefetched: Vec<(
            TaskId,
            SourceFileMetaData,
            Option<P<SourceFile>>,
            Option<Box<PrefetchedResolutions>>,
        )> = crate::program::worker_pool().install(|| {
            rayon::scope(|scope| {
                let spec = &spec;
                jobs.into_par_iter()
                    .map(|(t, file_name, path, is_lib, is_root)| {
                        use tsrs_core::festats::{self, Cat};

                        let _regions = crate::programregions::enter(opts.regions.as_deref());
                        let job_start = festats::enabled().then(std::time::Instant::now);
                        let file_name: &str = &file_name;
                        let mut metadata = if is_lib {
                            SourceFileMetaData {
                                implied_node_format: ModuleKind::COMMON_JS,
                                ..Default::default()
                            }
                        } else {
                            festats::timed(Cat::Meta, || {
                                source_file_meta_data(opts, resolver, project_references, file_name)
                            })
                        };
                        let file = crate::fileregions::parse_task(is_root, || {
                            file_parser.parse_source_file(parse_options_for(
                                host,
                                project_references,
                                file_name,
                                &path,
                                &metadata,
                            ))
                        });
                        // Bind here too: the round is bound by file system calls, and the checkers would bind every
                        // file on the same pool later (binding depends only on the file).
                        festats::timed(Cat::Bind, || file.map(crate::fileregions::bind));
                        if let Some(file) = file {
                            apply_virtual_file_name(&mut metadata, file);
                        }
                        let resolutions = festats::timed(Cat::Resolve, || match file {
                            Some(file) if resolve_ahead => {
                                Some(Box::new(prefetch_resolutions(&ctx, file, &metadata)))
                            }
                            _ => None,
                        });
                        if speculate
                            && !is_lib
                            && let (Some(file), Some(resolutions)) = (file, &resolutions)
                        {
                            spec.spawn_sub_tasks(scope, file, resolutions);
                        }
                        if let Some(start) = job_start {
                            let wall = start.elapsed().as_nanos() as u64;
                            festats::add(Cat::JobWall, wall);
                            festats::add_max(Cat::JobMax, wall);
                            festats::add(Cat::Jobs, 1);
                        }
                        (t, metadata, file, resolutions)
                    })
                    .collect()
            })
        });
        let parse_elapsed = parse_start.elapsed();
        if timings {
            tsrs_core::phases::record("Program:   parallel parse + resolve", parse_elapsed);
        }
        if stats {
            const ROWS: [&str; tsrs_core::festats::CATS] = [
                "Program:     stats: job wall",
                "Program:     stats: metadata",
                "Program:     stats: read",
                "Program:     stats: parse",
                "Program:     stats: bind",
                "Program:     stats: resolve",
                "Program:     stats: lock claimed",
                "Program:     stats: lock syncmap",
                "Program:     stats: lock vfs",
                "Program:     stats: claim ops",
                "Program:     stats: syncmap ops",
                "Program:     stats: vfs ops",
                "Program:     stats: jobs",
                "Program:     stats: longest job",
            ];

            let pool = crate::program::worker_pool();
            let cpu_after: Vec<f64> = pool.broadcast(|_| crate::checkerpool::thread_cpu_seconds());
            let per_thread: Vec<[u64; tsrs_core::festats::CATS]> =
                pool.broadcast(|_| tsrs_core::festats::take_thread());
            let mut sum = [0u64; tsrs_core::festats::CATS];
            for t in &per_thread {
                for (i, (s, v)) in sum.iter_mut().zip(t).enumerate() {
                    *s = if tsrs_core::festats::is_max(i) { (*s).max(*v) } else { *s + v };
                }
            }
            let threads = per_thread.len() as u64;
            let cpu: f64 = cpu_after.iter().zip(&cpu_before).map(|(a, b)| a - b).sum();
            tsrs_core::phases::record(
                "Program:     stats: thread-seconds (wall x threads)",
                parse_elapsed * threads as u32,
            );
            tsrs_core::phases::record(
                "Program:     stats: thread cpu",
                std::time::Duration::from_secs_f64(cpu),
            );
            for (i, (row, v)) in ROWS.iter().zip(sum).enumerate() {
                if tsrs_core::festats::is_count(i) {
                    tsrs_core::phases::count(true, row, v);
                } else {
                    tsrs_core::phases::record(row, std::time::Duration::from_nanos(v));
                }
            }
        }
        let speculative = spec.results.into_inner().unwrap();
        if speculate {
            tsrs_core::phases::count(
                loader.opts.config.compiler_options().unwrap().diagnostics_enabled(),
                "Program:   parsed ahead",
                speculative.len() as u64,
            );
        }
        loader.files_parser.speculative.extend(speculative);
        for (t, metadata, file, resolutions) in prefetched {
            let task = &mut loader.tasks[t];
            task.data().metadata = metadata;
            // Lib files keep metadata_loaded unset so that load_metadata still counts them.
            task.metadata_loaded = task.lib_file.is_none();
            // A missing file stays None; load() asks the host again and records it as missing.
            task.file = file;
            if let Some(resolutions) = resolutions {
                sub_tasks += resolutions.sub_task_bound();
                task.data().prefetched_resolutions = Some(resolutions);
            }
        }
        loader.tasks.reserve(sub_tasks);
    }

    pub(crate) fn get_processed_files(loader: &mut FileLoader) -> ProcessedFiles {
        struct Collector<'a> {
            loader: &'a mut FileLoader,
            // By data: the task whose casing the data was first walked under, and the address of that casing (a map
            // keyed by data hashed once per visit). The address answers the common case, the same shared name, as
            // `Arc<str>`'s `==` does, without reading the task; it is never dereferenced. Not an `Arc` clone: its
            // refcount update waited for the walk's earlier stores, which made it a tenth of the walk (notes/perf-serial-collect.md).
            seen: Vec<Option<(TaskId, *const str)>>,
        }

        // An explicit stack replaces Go's recursive collectFiles (the import graph of a large
        // project is deeper than the default thread stack allows). Each frame is a task list and
        // the index of the next task to visit; the post-subtask work of a task runs when its
        // child frame is popped.
        // A list frame names the task whose sub tasks it walks (None: the root tasks) instead of copying the list.
        enum Frame {
            List { owner: Option<TaskId>, next: usize },
            After { task: TaskId, data: DataId },
        }

        let total_file_count = loader.total_file_count;
        let lib_file_count = loader.lib_file_count;

        let mut missing_files: Vec<String> = Vec::new();
        let mut duplicate_source_files: Vec<DuplicateSourceFile> = Vec::new();
        let mut files: Vec<P<SourceFile>> =
            Vec::with_capacity(total_file_count.saturating_sub(lib_file_count));
        let mut lib_files: Vec<P<SourceFile>> = Vec::with_capacity(total_file_count);

        // The per-file maps get one entry per file: sized once (vscode: 10.4k entries; a map's iteration order is never
        // relied on, see the `iter_over_hash_type` expectations of their readers).
        let mut files_by_path: FxHashMap<Path, P<SourceFile>> =
            FxHashMap::with_capacity_and_hasher(total_file_count, rustc_hash::FxBuildHasher);
        // stores 'filename -> file association' ignoring case
        // used to track cases when two file names differ only in casing
        let mut tasks_seen_by_name_ignore_case: Option<FxHashMap<String, TaskId>> =
            if loader.compare_paths_options.use_case_sensitive_file_names {
                Some(FxHashMap::default())
            } else {
                None
            };

        let mut include_data = FileIncludeData::default();
        let can_use_project_reference_source = loader.opts.can_use_project_reference_source();
        let mut output_file_to_project_reference_source: FxHashMap<Path, String> =
            FxHashMap::default();
        let mut resolved_modules: FxHashMap<Path, ModeAwareCache<P<ResolvedModule>>> =
            FxHashMap::with_capacity_and_hasher(total_file_count, rustc_hash::FxBuildHasher);
        let mut type_resolutions_in_file: FxHashMap<
            Path,
            ModeAwareCache<P<ResolvedTypeReferenceDirective>>,
        > = FxHashMap::with_capacity_and_hasher(total_file_count, rustc_hash::FxBuildHasher);
        let mut source_file_meta_datas: FxHashMap<Path, SourceFileMetaData> =
            FxHashMap::with_capacity_and_hasher(total_file_count, rustc_hash::FxBuildHasher);
        let mut jsx_runtime_import_specifiers: FxHashMap<Path, P<JsxRuntimeImportSpecifier>> =
            FxHashMap::default();
        let mut import_helpers_import_specifiers: FxHashMap<Path, P<Node>> = FxHashMap::default();
        let mut source_files_found_searching_node_modules: FxHashSet<Path> = FxHashSet::default();
        let mut lib_files_map: FxHashMap<Path, P<LibFile>> = FxHashMap::default();

        let dedupe =
            !loader.opts.config.compiler_options().unwrap().deduplicate_packages.is_false();
        let mut redirect_targets_map: FxHashMap<Path, Vec<String>> = FxHashMap::default();
        let mut redirect_files_by_path: FxHashMap<Path, RedirectsFile> = FxHashMap::default();
        let mut package_id_to_source_file: FxHashMap<PackageId, P<SourceFile>> =
            FxHashMap::default();
        let force_consistent_casing = !loader
            .opts
            .config
            .compiler_options()
            .unwrap()
            .force_consistent_casing_in_file_names
            .is_false();

        // recordedDuplicates tracks, per task data, the set of file-name casings that
        // have already been recorded in duplicateSourceFiles. A file that is reached
        // from multiple import sites is walked once per site, but each distinct casing
        // is only parsed and acquired in the parse cache once. Recording the same casing
        // as a duplicate more than once would cause it to be released more times than it
        // was acquired when the snapshot is disposed, leaving a dangling cache entry that
        // panics the next time it is referenced.
        let mut recorded_duplicates: FxHashMap<DataId, FxHashSet<String>> = FxHashMap::default();
        // Include reasons by data (one per path), moved into the path-keyed map at the end in first-seen order:
        // keying by path here would hash each file's path once per import edge (vscode: 110k edges, 10k files).
        let mut reasons_by_data: Vec<Vec<P<FileIncludeReason>>> = Vec::new();
        reasons_by_data.resize_with(loader.files_parser.datas.len(), Vec::new);
        // The task whose path keys the data's reasons; the path is cloned after the walk (see `seen`).
        let mut reasons_order: Vec<(DataId, TaskId)> = Vec::new();
        let mut c = Collector { loader, seen: vec![None; reasons_by_data.len()] };
        let roots = c.loader.root_tasks.clone();
        let mut stack: Vec<Frame> = vec![Frame::List { owner: None, next: 0 }];

        while let Some(frame) = stack.pop() {
            match frame {
                Frame::After { task, data } => {
                    let loader = &mut *c.loader;
                    // Exclude automatic type directive tasks from include reason processing,
                    // as these are internal implementation details and should not contribute
                    // to the reasons for including files.
                    if let Some(redirected) = loader.tasks[task].redirected_parse_task {
                        if !can_use_project_reference_source {
                            output_file_to_project_reference_source.insert(
                                loader.tasks[redirected].path.clone(),
                                loader.tasks[task].normalized_file_path.to_string(),
                            );
                        }
                        continue;
                    }

                    if loader.tasks[task].is_for_automatic_type_directive {
                        let path = loader.tasks[task].path.clone();
                        type_resolutions_in_file.insert(
                            path,
                            std::mem::take(&mut loader.tasks[task].data().type_resolutions_in_file),
                        );
                        let diags = loader.tasks[task].take_processing_diagnostics();
                        include_data.processing_diagnostics.extend(diags);
                        continue;
                    }

                    let diags = loader.tasks[task].take_processing_diagnostics();
                    include_data.processing_diagnostics.extend(diags);

                    let Some(file) = loader.tasks[task].file else {
                        missing_files.push(loader.tasks[task].normalized_file_path.to_string());
                        continue;
                    };

                    // The keys are cloned before the inserts (see `seen`); the metadata is taken, as the resolutions
                    // are: this is the last read of the task's data.
                    let path = loader.tasks[task].path.clone();
                    let (files_key, resolutions_key, type_resolutions_key, metadata_key) =
                        (path.clone(), path.clone(), path.clone(), path.clone());
                    if let Some(lib_file) = loader.tasks[task].lib_file {
                        lib_files.push(file);
                        lib_files_map.insert(path.clone(), lib_file);
                    } else {
                        files.push(file);
                    }
                    let loaded = loader.tasks[task].data();
                    files_by_path.insert(files_key, file);
                    resolved_modules
                        .insert(resolutions_key, std::mem::take(&mut loaded.resolutions_in_file));
                    type_resolutions_in_file.insert(
                        type_resolutions_key,
                        std::mem::take(&mut loaded.type_resolutions_in_file),
                    );
                    source_file_meta_datas
                        .insert(metadata_key, std::mem::take(&mut loaded.metadata));

                    if let Some(jsx) = loaded.jsx_runtime_import_specifier {
                        jsx_runtime_import_specifiers.insert(path.clone(), jsx);
                    }
                    if let Some(specifier) = loaded.import_helpers_import_specifier {
                        import_helpers_import_specifiers.insert(path.clone(), specifier);
                    }
                    if loader.files_parser.datas[data].lowest_depth.is_some_and(|depth| depth > 0) {
                        source_files_found_searching_node_modules.insert(path);
                    }
                }
                Frame::List { owner, next } => {
                    let tasks: &[TaskId] = match owner {
                        Some(owner) => &c.loader.tasks[owner].sub_tasks,
                        None => &roots,
                    };
                    if next >= tasks.len() {
                        continue;
                    }
                    let mut task = tasks[next];
                    stack.push(Frame::List { owner, next: next + 1 });

                    let loader = &mut *c.loader;
                    let include_reason = loader.tasks[task].include_reason;
                    // Exclude automatic type directive tasks from include reason processing,
                    // as these are internal implementation details and should not contribute
                    // to the reasons for including files.
                    if loader.tasks[task].redirected_parse_task.is_none()
                        && !loader.tasks[task].is_for_automatic_type_directive
                    {
                        if let Some(loaded_task) = loader.tasks[task].loaded_task {
                            task = loaded_task;
                        }
                        add_include_reason(
                            loader,
                            &mut reasons_by_data,
                            &mut reasons_order,
                            task,
                            include_reason,
                        );
                    }
                    let data = task_data(loader, task);
                    if !loader.tasks[task].loaded {
                        continue;
                    }

                    // ensure we only walk each task once
                    if let Some((checked_task, checked_ptr)) = c.seen[data] {
                        let name = &loader.tasks[task].normalized_file_path;
                        let differs = !std::ptr::eq(checked_ptr, Arc::as_ptr(name))
                            && *loader.tasks[checked_task].normalized_file_path != **name;
                        if let Some(file) = loader.tasks[task].file
                            && differs
                            && recorded_duplicates
                                .entry(data)
                                .or_default()
                                .insert(loader.tasks[task].normalized_file_path.to_string())
                        {
                            duplicate_source_files.push(duplicate_source_file(file));
                        }
                        // Identical names normalize identically; only differing ones need the comparison.
                        if force_consistent_casing && differs {
                            let checked_name = &loader.tasks[checked_task].normalized_file_path;
                            // Check if it differs only in drive letters its ok to ignore that error:
                            let checked_absolute_path =
                                tspath::get_normalized_absolute_path_without_root(
                                    checked_name,
                                    &loader.compare_paths_options.current_directory,
                                );
                            let input_absolute_path =
                                tspath::get_normalized_absolute_path_without_root(
                                    &loader.tasks[task].normalized_file_path,
                                    &loader.compare_paths_options.current_directory,
                                );
                            if checked_absolute_path != input_absolute_path {
                                let checked_name = Arc::clone(checked_name);
                                include_data.add_processing_diagnostics_for_file_casing(
                                    loader.tasks[task].path.clone(),
                                    &checked_name,
                                    loader.tasks[task].normalized_file_path.as_ref(),
                                    include_reason,
                                );
                            }
                        }
                        continue;
                    }
                    c.seen[data] =
                        Some((task, Arc::as_ptr(&loader.tasks[task].normalized_file_path)));

                    if let Some(seen_ignore_case) = &mut tasks_seen_by_name_ignore_case {
                        let path_lower_case =
                            tspath::to_file_name_lower_case(loader.tasks[task].path.as_str());
                        match seen_ignore_case.entry(path_lower_case) {
                            Entry::Occupied(entry) => {
                                let task_by_ignore_case = *entry.get();
                                let (p, n) = (
                                    loader.tasks[task_by_ignore_case].path.clone(),
                                    loader.tasks[task_by_ignore_case]
                                        .normalized_file_path
                                        .to_string(),
                                );
                                include_data.add_processing_diagnostics_for_file_casing(
                                    p,
                                    &n,
                                    loader.tasks[task].normalized_file_path.as_ref(),
                                    include_reason,
                                );
                            }
                            Entry::Vacant(entry) => {
                                entry.insert(task);
                            }
                        }
                    }

                    if let Some(data) = loader.tasks[task].data_ref() {
                        for trace in &data.type_resolutions_trace {
                            loader.host.trace(trace.message, &trace_args(trace));
                        }
                        for trace in &data.resolutions_trace {
                            loader.host.trace(trace.message, &trace_args(trace));
                        }
                    }

                    let file = loader.tasks[task].file;
                    let data_package_id = loader.files_parser.datas[data].package_id;
                    if dedupe && !data_package_id.name.is_empty() {
                        if let Some(&package_id_file) =
                            package_id_to_source_file.get(&data_package_id)
                        {
                            if let Some(file) = file {
                                // Package deduplication keeps the first package instance in the
                                // program, but we still parsed this file and acquired it through
                                // the host, so snapshot disposal must release that extra owner.
                                duplicate_source_files.push(duplicate_source_file(file));
                            }
                            redirect_targets_map
                                .entry(package_id_file.path().clone())
                                .or_default()
                                .push(loader.tasks[task].normalized_file_path.to_string());
                            let index = files.len() + redirect_files_by_path.len();
                            redirect_files_by_path.insert(
                                loader.tasks[task].path.clone(),
                                RedirectsFile {
                                    index,
                                    file_name: loader.tasks[task].normalized_file_path.to_string(),
                                    path: loader.tasks[task].path.clone(),
                                    target: package_id_file.path().clone(),
                                },
                            );
                            files_by_path.insert(loader.tasks[task].path.clone(), package_id_file);
                            if loader.files_parser.datas[data]
                                .lowest_depth
                                .is_some_and(|depth| depth > 0)
                            {
                                source_files_found_searching_node_modules
                                    .insert(loader.tasks[task].path.clone());
                            }
                            continue;
                        } else if let Some(file) = file {
                            package_id_to_source_file.insert(data_package_id, file);
                        }
                    }

                    stack.push(Frame::After { task, data });
                    if !loader.tasks[task].sub_tasks.is_empty() {
                        stack.push(Frame::List { owner: Some(task), next: 0 });
                    }
                }
            }
        }

        let loader = c.loader;
        let reason_keys: Vec<Path> =
            reasons_order.iter().map(|&(_, task)| loader.tasks[task].path.clone()).collect();
        include_data.file_include_reasons.extend(
            reasons_order
                .into_iter()
                .zip(reason_keys)
                .map(|((data, _), path)| (path, std::mem::take(&mut reasons_by_data[data]))),
        );
        loader.sort_libs(&mut lib_files);

        let lib_len = lib_files.len();
        let mut all_files = lib_files;
        all_files.extend(files);
        #[expect(clippy::iter_over_hash_type, reason = "shifts each index on its own")]
        for redirect_file in redirect_files_by_path.values_mut() {
            redirect_file.index += lib_len;
        }

        let mut keys: Vec<Path> = loader.path_for_lib_file_resolutions.keys().cloned().collect();
        keys.sort();
        for key in keys {
            let value = &loader.path_for_lib_file_resolutions[&key];
            let mut cache = ModeAwareCache::default();
            if let Some(resolution) = value.resolution {
                cache.insert(
                    tsrs_module::ModeAwareCacheKey {
                        name: tsrs_core::alloc_str(&value.library_name),
                        mode: ModuleKind::COMMON_JS,
                    },
                    resolution,
                );
            }
            resolved_modules.insert(key.clone(), cache);
            for trace in &value.trace {
                loader.host.trace(trace.message, &trace_args(trace));
            }
        }

        ProcessedFiles {
            finished_processing: true,
            files: tsrs_core::alloc_vec(all_files),
            duplicate_source_files,
            files_by_path,
            project_reference_file_mapper: Some(loader.project_references.take_mapper()),
            resolved_modules,
            type_resolutions_in_file,
            source_file_meta_datas,
            jsx_runtime_import_specifiers,
            import_helpers_import_specifiers,
            source_files_found_searching_node_modules,
            lib_files: lib_files_map,
            missing_files,
            file_include_data: include_data,
            output_file_to_project_reference_source,
            redirect_targets_map,
            redirect_files_by_path,
            content_mapper_diagnostics: std::mem::take(
                &mut loader.content_mapper_mu.lock().unwrap().content_mapper_diagnostics,
            ),
        }
    }
}

// filesparser.go:408, 454
fn duplicate_source_file(file: P<SourceFile>) -> DuplicateSourceFile {
    DuplicateSourceFile {
        parse_options: file.parse_options().clone(),
        hash: file.hash.get(),
        script_kind: file.script_kind.get(),
    }
}

fn add_include_reason(
    loader: &FileLoader,
    reasons_by_data: &mut [Vec<P<FileIncludeReason>>],
    reasons_order: &mut Vec<(DataId, TaskId)>,
    task: TaskId,
    reason: Option<P<FileIncludeReason>>,
) {
    if let Some(redirected) = loader.tasks[task].redirected_parse_task {
        add_include_reason(loader, reasons_by_data, reasons_order, redirected, reason);
    } else if loader.tasks[task].loaded
        && let Some(reason) = reason
    {
        let data = task_data(loader, task);
        if reasons_by_data[data].is_empty() {
            reasons_order.push((data, task));
        }
        reasons_by_data[data].push(reason);
    }
}

/// The data of a task's path (`data_id` when `start` set it, else the map).
fn task_data(loader: &FileLoader, task: TaskId) -> DataId {
    match loader.tasks[task].data_hint() {
        Some(data) => data,
        None => loader.files_parser.task_data_by_path[&loader.tasks[task].path],
    }
}

fn trace_args(trace: &DiagAndArgs) -> Vec<&dyn std::fmt::Display> {
    trace.args.iter().map(|a| a as &dyn std::fmt::Display).collect()
}
