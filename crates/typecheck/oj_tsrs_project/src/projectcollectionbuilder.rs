use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Instant;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_ast::Diagnostic;
use tsrs_compiler::Program;
use tsrs_core::collections::{Set, SyncMap, SyncSet, new_set_with_size_hint};
use tsrs_core::context::Context;
use tsrs_core::tspath::{self, ComparePathsOptions, Path};
use tsrs_core::{
    BreadthFirstSearchLevel, BreadthFirstSearchOptions, breadth_first_search_parallel_ex,
};
use tsrs_core::{
    CompilerOptions, P, ProjectReference, ScriptKind, Tristate, get_script_kind_from_file_name,
};
use tsrs_diagnostics as diagnostics;
use tsrs_ls::lsutil::UserPreferences;
use tsrs_lsproto as lsproto;
use tsrs_tsoptions::{Mapper, ParsedCommandLine};

use crate::client::Client;
use crate::compilerhost::new_compiler_host;
use crate::configfileregistry::ConfigFileRegistry;
use crate::configfileregistrybuilder::{
    ChangeFileResult, ConfigFileRegistryBuilder, new_config_file_registry_builder,
};
use crate::dirty::{self, Shared, SyncMapEntry, Value};
use crate::extendedconfigcache::ExtendedConfigCache;
use crate::filechange::FileChangeSummary;
use crate::logging::LogTree;
use crate::overlayfs::{OverlayMap, ToPath};
use crate::parsecache::{ParseCache, ParseCacheJournal};
use crate::project::{
    ConfiguredProjectID, ID, Kind, ModuleResolverFactoryRef, ProgramUpdateKind, Project,
    SyntheticProjectID, inferred_project_id, new_configured_project, new_inferred_project,
    new_inferred_project_command_line, new_synthetic_project, new_synthetic_project_id,
};
use crate::projectcollection::{
    APIState, ApiOpenedFile, ProjectCollection,
    find_default_configured_project_from_program_inclusion, open_file_paths,
};
use crate::session::SessionOptions;
use crate::snapshot::{APISnapshotRequest, ATAStateChange, ProjectTreeRequest};
use crate::snapshotfs::{FileHandleSource, SnapshotFSBuilder};
use crate::watch::get_typings_locations_globs;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum ProjectLoadKind {
    // Project is not created or updated, only looked up in cache
    Find,
    // Project is created and then its graph is updated
    Create,
}

// A configured, synthetic or the inferred project entry (Go passes them all as `dirty.Value[*Project]`).
#[derive(Clone)]
pub enum ProjectEntry {
    Configured(Arc<SyncMapEntry<ConfiguredProjectID, Project>>),
    Synthetic(Arc<SyncMapEntry<SyntheticProjectID, Project>>),
    Inferred,
}

// projectcollectionbuilder.go:35 (Go's extendedConfigCache field is never read; the config file registry builder gets it)
pub struct ProjectCollectionBuilder {
    pub(crate) session_options: Arc<SessionOptions>,
    pub(crate) parse_cache: Arc<ParseCache>,
    /// The parse-cache references this clone's programs take (rolled back if the clone unwinds).
    pub(crate) parse_cache_journal: Arc<ParseCacheJournal>,
    pub(crate) to_path: ToPath,

    pub(crate) ctx: Context,
    pub(crate) fs: Arc<SnapshotFSBuilder>,
    overlays: OverlayMap,
    base: Arc<ProjectCollection>,
    compiler_options_for_inferred_projects: Option<P<CompilerOptions>>,
    inferred_content_mappers: Vec<Mapper>,
    inferred_content_mapper_extensions: Vec<String>,
    pub(crate) config_file_registry_builder: Arc<ConfigFileRegistryBuilder>,

    client: Option<Arc<dyn Client>>, // optional; used for project loading notifications

    new_snapshot_id: u64,
    program_structure_changed: Cell<bool>,
    default_projects_invalidated: Cell<bool>,
    open_files_changed: Cell<bool>,

    file_default_projects: RefCell<Option<FxHashMap<Path, ID>>>,
    configured_projects: dirty::SyncMap<ConfiguredProjectID, Project>,
    synthetic_projects: dirty::SyncMap<SyntheticProjectID, Project>,
    inferred_project: dirty::Box<Project>,
    created_programs: RefCell<Vec<Option<Shared<Project>>>>,

    api_state: RefCell<APIState>,
}

// projectcollectionbuilder.go:68
pub fn new_project_collection_builder(
    ctx: &Context,
    new_snapshot_id: u64,
    fs: Arc<SnapshotFSBuilder>,
    overlays: OverlayMap,
    old_project_collection: Arc<ProjectCollection>,
    old_config_file_registry: Arc<ConfigFileRegistry>,
    old_api_state: &APIState,
    compiler_options_for_inferred_projects: Option<P<CompilerOptions>>,
    inferred_content_mappers: Vec<Mapper>,
    inferred_content_mapper_extensions: Vec<String>,
    session_options: Arc<SessionOptions>,
    custom_config_file_name: &str,
    parse_cache: Arc<ParseCache>,
    extended_config_cache: Arc<ExtendedConfigCache>,
    client: Option<Arc<dyn Client>>,
) -> ProjectCollectionBuilder {
    let open_files = open_file_paths(&overlays);
    let is_open_overlays = Arc::clone(&overlays);
    ProjectCollectionBuilder {
        ctx: ctx.clone(),
        to_path: Arc::clone(&fs.to_path),
        compiler_options_for_inferred_projects,
        inferred_content_mappers,
        inferred_content_mapper_extensions,
        parse_cache,
        parse_cache_journal: Arc::default(),
        config_file_registry_builder: Arc::new(new_config_file_registry_builder(
            lsproto::get_client_capabilities(ctx)
                .workspace
                .did_change_watched_files
                .relative_pattern_support,
            Arc::clone(&fs),
            Box::new(move |path: &Path| is_open_overlays.contains_key(path)),
            old_config_file_registry,
            extended_config_cache,
            new_snapshot_id,
            Arc::clone(&session_options),
            custom_config_file_name,
            &LogTree::nil(),
        )),
        session_options,
        fs,
        overlays,
        new_snapshot_id,
        program_structure_changed: Cell::new(false),
        default_projects_invalidated: Cell::new(false),
        open_files_changed: Cell::new(!open_files.equals(&old_project_collection.open_files)),
        file_default_projects: RefCell::new(None),
        configured_projects: dirty::new_sync_map(Arc::clone(
            &old_project_collection.configured_projects,
        )),
        synthetic_projects: dirty::new_sync_map(Arc::clone(
            &old_project_collection.synthetic_projects,
        )),
        inferred_project: dirty::new_box(old_project_collection.inferred_project.clone()),
        created_programs: RefCell::new(Vec::new()),
        api_state: RefCell::new(old_api_state.clone_state()),
        base: old_project_collection,
        client,
    }
}

// Go's generated CompilerOptions.Equals compares every stored field and ordered paths.
fn compiler_options_deep_equal(
    a: Option<P<CompilerOptions>>,
    b: Option<P<CompilerOptions>>,
) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a == b || (*a).equals(&b),
        _ => false,
    }
}

// Go `reflect.DeepEqual` on diagnostic slices: diagnostics are compared by identity, which is what the callers
// pass (the errors of the same command line).
fn diagnostics_equal(a: &[P<Diagnostic>], b: &[P<Diagnostic>]) -> bool {
    a == b
}

impl ProjectCollectionBuilder {
    fn value_of<'a>(&'a self, entry: &'a ProjectEntry) -> &'a dyn Value<Project> {
        match entry {
            ProjectEntry::Configured(e) => e,
            ProjectEntry::Synthetic(e) => e,
            ProjectEntry::Inferred => &self.inferred_project,
        }
    }

    pub(crate) fn inferred_project_value(&self) -> Option<Shared<Project>> {
        self.inferred_project.value()
    }

    pub(crate) fn take_created_programs(&self) -> Vec<Option<Shared<Project>>> {
        std::mem::take(&mut self.created_programs.borrow_mut())
    }

    fn file_default_project(&self, path: &Path) -> Option<ID> {
        self.file_default_projects.borrow().as_ref().and_then(|m| m.get(path).cloned())
    }

    fn set_file_default_project(&self, path: Path, id: ID) {
        self.file_default_projects
            .borrow_mut()
            .get_or_insert_with(FxHashMap::default)
            .insert(path, id);
    }

    // projectcollectionbuilder.go:113
    fn is_open_file(&self, path: &Path) -> bool {
        self.overlays.contains_key(path)
    }

    // projectcollectionbuilder.go:118
    pub(crate) fn finalize(
        &self,
        _logger: &LogTree,
    ) -> (ProjectCollection, Arc<ConfigFileRegistry>) {
        let mut new_project_collection: Option<ProjectCollection> = None;
        let base = &self.base;
        macro_rules! ensure_cloned {
            () => {
                new_project_collection.get_or_insert_with(|| base.clone_collection())
            };
        }

        let (configured_projects, configured_projects_changed) =
            self.configured_projects.finalize();
        if configured_projects_changed {
            ensure_cloned!().configured_projects = configured_projects;
        }
        let (synthetic_projects, synthetic_projects_changed) = self.synthetic_projects.finalize();
        if synthetic_projects_changed {
            ensure_cloned!().synthetic_projects = synthetic_projects;
        }

        if self.open_files_changed.get() {
            ensure_cloned!().open_files = Arc::new(open_file_paths(&self.overlays));
        }

        let file_default_projects = self.file_default_projects.borrow().clone();
        let base_defaults =
            base.file_default_projects.as_ref().map(|m| (**m).clone()).unwrap_or_default();
        if file_default_projects.clone().unwrap_or_default() != base_defaults {
            ensure_cloned!().file_default_projects = file_default_projects.map(Arc::new);
        }

        let (new_inferred_project, inferred_project_changed) = self.inferred_project.finalize();
        if inferred_project_changed {
            ensure_cloned!().inferred_project = new_inferred_project;
        }

        let config_file_registry = self.config_file_registry_builder.finalize();
        if !Arc::ptr_eq(&config_file_registry, &base.config_file_registry) {
            ensure_cloned!().config_file_registry = Arc::clone(&config_file_registry);
        }

        let api_state = self.api_state.borrow().clone();
        if !api_state.equals(&base.api_state) {
            ensure_cloned!().api_state = api_state;
        }

        let new_project_collection = new_project_collection.unwrap_or_else(|| (**base).clone());
        (new_project_collection, config_file_registry)
    }

    // projectcollectionbuilder.go:166
    fn for_each_project(&self, mut f: impl FnMut(&ProjectEntry) -> bool) {
        let mut keep_going = true;
        self.configured_projects.range(|entry| {
            keep_going = f(&ProjectEntry::Configured(Arc::clone(entry)));
            keep_going
        });
        if keep_going {
            self.synthetic_projects.range(|entry| {
                keep_going = f(&ProjectEntry::Synthetic(Arc::clone(entry)));
                keep_going
            });
        }
        if !keep_going {
            return;
        }
        if self.inferred_project.value().is_some() {
            f(&ProjectEntry::Inferred);
        }
    }

    // projectcollectionbuilder.go:186
    pub(crate) fn handle_api_request(
        &self,
        api_request: &APISnapshotRequest,
        logger: &LogTree,
    ) -> Result<(), lsproto::Error> {
        let mut projects_to_close: Option<FxHashSet<Path>> = None;
        if let Some(close_projects) = &api_request.close_projects {
            let mut api_state = self.api_state.borrow_mut();
            #[expect(
                clippy::iter_over_hash_type,
                reason = "each unique key adjusts only its own counter and set membership; Go ranges the set too"
            )]
            for project_path in close_projects.keys() {
                if let Some(count) = api_state.open_projects.get_mut(project_path) {
                    if *count > 1 {
                        *count -= 1;
                    } else if *count == 1 {
                        api_state.open_projects.remove(project_path);
                        projects_to_close
                            .get_or_insert_with(FxHashSet::default)
                            .insert(project_path.clone());
                    }
                }
            }
        }

        if let Some(open_projects) = &api_request.open_projects {
            #[expect(
                clippy::iter_over_hash_type,
                reason = "Go ranges the set too; the order only decides which error is returned when several projects are missing"
            )]
            for config_file_name in open_projects.keys() {
                let config_path = (self.to_path)(config_file_name);
                match self.find_or_create_project(
                    config_file_name,
                    &config_path,
                    ProjectLoadKind::Create,
                    logger,
                ) {
                    Some(entry) => {
                        *self
                            .api_state
                            .borrow_mut()
                            .open_projects
                            .entry(config_path.clone())
                            .or_insert(0) += 1;
                        // A project re-opened in the same request shouldn't be closed.
                        if let Some(p) = &mut projects_to_close {
                            p.remove(&config_path);
                        }
                        self.update_program(&ProjectEntry::Configured(entry), logger);
                    }
                    _ => {
                        return Err(lsproto::Error::new(format!(
                            "project not found for open: {config_file_name}"
                        )));
                    }
                }
            }
        }

        if let Some(close_files) = &api_request.close_files {
            let mut api_state = self.api_state.borrow_mut();
            #[expect(
                clippy::iter_over_hash_type,
                reason = "per-key refcount update on unique keys; Go ranges the set too"
            )]
            for path in close_files.keys() {
                if let Some(entry) = api_state.open_files.get_mut(path) {
                    if entry.ref_count > 1 {
                        entry.ref_count -= 1;
                    } else {
                        api_state.open_files.remove(path);
                    }
                }
            }
        }

        if let Some(open_files) = &api_request.open_files {
            let mut api_state = self.api_state.borrow_mut();
            #[expect(
                clippy::iter_over_hash_type,
                reason = "per-key upsert on unique keys; Go ranges the map too"
            )]
            for (path, file_name) in open_files {
                let entry = api_state.open_files.entry(path.clone()).or_default();
                entry.file_name.clone_from(file_name);
                entry.ref_count += 1;
            }
        }

        #[expect(
            clippy::iter_over_hash_type,
            reason = "a cached lookup, then a delete from a set; commutes; Go ranges the map too"
        )]
        for overlay in self.overlays.values() {
            if let Some(entry) = self.find_default_configured_project(
                overlay.base.file_name.as_str(),
                &(self.to_path)(&overlay.base.file_name),
            ) && let Some(p) = &mut projects_to_close
            {
                p.remove(&entry.value().unwrap().config_file_path);
            }
        }

        if let Some(projects_to_close) = &projects_to_close {
            #[expect(
                clippy::iter_over_hash_type,
                reason = "order-independent: deletes each closed project by its own key; Go ranges the set too"
            )]
            for project_path in projects_to_close {
                if let Some(entry) =
                    self.configured_projects.load(&ConfiguredProjectID(project_path.clone()))
                {
                    self.delete_project(&ProjectEntry::Configured(entry), logger);
                }
            }
        }

        // Place newly API-opened files like LSP's textDocument/didOpen, ensuring only
        // their target projects. Existing API-opened files are retained by cleanup below
        // without implicitly updating their programs.
        if let Some(open_files) = &api_request.open_files {
            let mut retain: Set<Path> = Set::default();
            let mut ensure_inferred_project = false;
            #[expect(
                clippy::iter_over_hash_type,
                reason = "Go ranges the map too; the order only decides which error is returned when several files have no project"
            )]
            for (path, file_name) in open_files {
                if self.is_open_file(path) {
                    if self.find_default_configured_project(file_name, path).is_none()
                        && !self.is_supported_in_inferred_project(file_name)
                    {
                        return Err(lsproto::Error::new(format!(
                            "no project found for opened file: {file_name}"
                        )));
                    }
                    continue;
                }
                let result =
                    self.ensure_configured_project_and_ancestors_for_file(file_name, path, logger);
                retain.union(&result.retain);
                if result.project.is_none() {
                    if !self.is_supported_in_inferred_project(file_name) {
                        return Err(lsproto::Error::new(format!(
                            "no project found for opened file: {file_name}"
                        )));
                    }
                    ensure_inferred_project = true;
                }
            }
            self.cleanup_configured_projects(Some(&retain), logger);
            if ensure_inferred_project && self.inferred_project.value().is_some() {
                self.update_program(&ProjectEntry::Inferred, logger);
            }
        } else if api_request.close_files.is_some() {
            self.cleanup_configured_projects(None, logger);
        }
        let mut seen_reconfigured_programs: Set<SyntheticProjectID> = Set::default();
        for request in &api_request.reconfigure_programs {
            if seen_reconfigured_programs.has(&request.program_id) {
                return Err(lsproto::Error::new(format!(
                    "synthetic program reconfigured more than once: {}",
                    request.program_id.0
                )));
            }
            seen_reconfigured_programs.add(request.program_id.clone());
            if api_request.remove_programs.as_ref().is_some_and(|r| r.has(&request.program_id)) {
                return Err(lsproto::Error::new(format!(
                    "synthetic program cannot be reconfigured and removed: {}",
                    request.program_id.0
                )));
            }
            if self.synthetic_projects.load(&request.program_id).is_none() {
                return Err(lsproto::Error::new(format!(
                    "synthetic program not found for reconfiguration: {}",
                    request.program_id.0
                )));
            }
        }
        if let Some(remove_programs) = &api_request.remove_programs {
            #[expect(
                clippy::iter_over_hash_type,
                reason = "Go ranges the set too; the order only decides which error is returned when several programs are missing"
            )]
            for program_id in remove_programs.keys() {
                let Some(project) = self.synthetic_projects.load(program_id) else {
                    return Err(lsproto::Error::new(format!(
                        "synthetic program not found for removal: {}",
                        program_id.0
                    )));
                };
                self.delete_project(&ProjectEntry::Synthetic(project), logger);
            }
        }
        let mut created_entries = Vec::with_capacity(api_request.create_programs.len());
        created_entries.extend(api_request.create_programs.iter().map(|request| {
            self.update_or_create_synthetic_project(
                self.next_synthetic_project_id(),
                request.root_file_names.clone(),
                request.compiler_options,
                request.project_references.clone(),
                request.config_file_parsing_diagnostics.clone(),
                request.module_resolver_factory.clone(),
                request.module_resolver_id,
                self.inferred_content_mappers.clone(),
                logger,
            )
        }));
        let mut reconfigured_entries = Vec::with_capacity(api_request.reconfigure_programs.len());
        reconfigured_entries.extend(api_request.reconfigure_programs.iter().map(|request| {
            self.update_or_create_synthetic_project(
                request.program_id.clone(),
                request.request.root_file_names.clone(),
                request.request.compiler_options,
                request.request.project_references.clone(),
                request.request.config_file_parsing_diagnostics.clone(),
                request.request.module_resolver_factory.clone(),
                request.request.module_resolver_id,
                self.inferred_content_mappers.clone(),
                logger,
            )
        }));
        // Go updates these programs in parallel goroutines; here they run one after another.
        let mut created_programs = Vec::with_capacity(created_entries.len());
        created_programs.extend(created_entries.iter().map(|entry| {
            let entry = ProjectEntry::Synthetic(Arc::clone(entry));
            if self.value_of(&entry).value().unwrap().dirty {
                self.update_program(&entry, logger);
            }
            self.value_of(&entry).value()
        }));
        for entry in &reconfigured_entries {
            let entry = ProjectEntry::Synthetic(Arc::clone(entry));
            if self.value_of(&entry).value().unwrap().dirty {
                self.update_program(&entry, logger);
            }
        }
        *self.created_programs.borrow_mut() = created_programs;
        #[expect(
            clippy::iter_over_hash_type,
            reason = "Go ranges the map too; the order only decides which error is returned when several files have no project"
        )]
        for (path, file_name) in &api_request.ensure_files {
            self.did_request_file_path(
                file_name, path, false, /*configuredProjectsOnly*/
                logger,
            );
            if self.find_default_project(file_name, path).is_none() {
                return Err(lsproto::Error::new(format!(
                    "no project found for opened file: {file_name}"
                )));
            }
        }
        if let Some(ensure_programs) = &api_request.ensure_programs {
            #[expect(
                clippy::iter_over_hash_type,
                reason = "order-independent: loads each requested project by its own ID; Go ranges the set too"
            )]
            for project_id in ensure_programs.keys() {
                self.did_request_project(project_id, logger);
            }
        }
        if api_request.ensure_all_programs {
            self.for_each_project(|entry| {
                self.update_program(entry, logger);
                true
            });
        }
        let mut module_resolution_error: Option<String> = None;
        self.for_each_project(|entry| {
            let project = self.value_of(entry).value().unwrap();
            if let Some(program) = project.program {
                module_resolution_error =
                    program.module_resolution_error().map(std::string::ToString::to_string);
            }
            module_resolution_error.is_none()
        });
        match module_resolution_error {
            Some(err) => Err(lsproto::Error::new(err)),
            None => Ok(()),
        }
    }

    // projectcollectionbuilder.go:379
    fn next_synthetic_project_id(&self) -> SyntheticProjectID {
        let mut id = 1;
        loop {
            let project_id = new_synthetic_project_id(id);
            if self.synthetic_projects.load(&project_id).is_none() {
                return project_id;
            }
            id += 1;
        }
    }

    // projectcollectionbuilder.go:388
    pub(crate) fn did_change_files(&self, summary: &FileChangeSummary, logger: &LogTree) {
        self.open_files_changed.set(
            self.open_files_changed.get()
                || !summary.opened.0.is_empty()
                || !summary.closed.is_empty(),
        );

        let to_paths = |uris: &Set<lsproto::DocumentUri>| -> Vec<Path> {
            uris.keys().iter().map(|uri| (self.to_path)(&uri.file_name())).collect()
        };
        let changed_files = to_paths(&summary.changed);
        let deleted_files = to_paths(&summary.deleted);
        let created_files = to_paths(&summary.created);
        // (Content mappers are not ported: there is no content mapper host, so Go's refresh of content mapper
        // projects for watch changes is skipped.)

        let config_change_logger = logger.fork("Checking for changes affecting config files");
        let config_change_result =
            self.config_file_registry_builder.did_change_files(summary, &config_change_logger);
        log_change_file_result(&config_change_result, &config_change_logger);

        self.program_structure_changed
            .set(self.mark_projects_affected_by_config_changes(&config_change_result, logger));

        self.for_each_project(|entry| {
            let value = self.value_of(entry);
            // Only consider change/delete; creates are handled by the config file registry
            if summary.has_excessive_non_create_watch_events() {
                value.change(&mut |p: &mut Project| {
                    p.dirty = true;
                    p.dirty_file_path = Path::default();
                    logger.logf(format_args!("Marking project as dirty due to excessive watch changes: {}", p.id()));
                });
                return true;
            }

            // Handle closed and changed files
            self.mark_files_changed(entry, &changed_files, lsproto::FileChangeType::CHANGED, logger);
            if value.value().unwrap().kind == Kind::Inferred && !summary.closed.is_empty() {
                let command_line = value.value().unwrap().command_line.unwrap();
                let root_files_map = command_line.file_names_by_path();
                let mut new_root_files: Vec<String> = command_line.file_names().to_vec();
                #[expect(clippy::iter_over_hash_type, reason = "removes distinct names by value from new_root_files; removals commute; Go ranges the set too")]
                for uri in summary.closed.keys() {
                    let file_name = uri.file_name();
                    let path = (self.to_path)(&file_name);
                    if root_files_map.contains_key(&path) {
                        // Go: slices.Delete(newRootFiles, slices.Index(newRootFiles, fileName), ...+1)
                        let index = new_root_files.iter().position(|f| *f == file_name);
                        match index {
                            Some(index) => {
                                new_root_files.remove(index);
                            }
                            // Go: slices.Delete(s, -1, 0) panics.
                            None => panic!("slice bounds out of range [-1:]"),
                        }
                    }
                }
                self.update_inferred_project_roots(new_root_files, logger);
            }

            // Handle deleted files
            if !summary.deleted.is_empty() {
                self.mark_files_changed(entry, &deleted_files, lsproto::FileChangeType::DELETED, logger);
            }

            // Handle created files
            if !summary.created.is_empty() {
                self.mark_files_changed(entry, &created_files, lsproto::FileChangeType::CREATED, logger);
            }

            true
        });

        // Handle opened file
        if !summary.opened.0.is_empty() || !summary.reopened.0.is_empty() {
            let opened =
                if summary.opened.0.is_empty() { &summary.reopened } else { &summary.opened };
            let file_name = opened.file_name();
            let path = (self.to_path)(&file_name);
            let open_file_result =
                self.ensure_configured_project_and_ancestors_for_file(&file_name, &path, logger);
            self.cleanup_configured_projects(Some(&open_file_result.retain), logger);
        }
    }

    // projectcollectionbuilder.go:500
    // cleanupConfiguredProjects sweeps the loaded configured projects and unloads those
    // that are no longer needed. Starting from the set of all configured projects, it
    // retains any project that is the default project (along with its references and
    // ancestor configs) of an open overlay file or an API-opened file, any project
    // explicitly opened through the API, and any project in retain (e.g. the ancestor
    // solution tree built for a freshly opened overlay file). Every other configured
    // project is deleted, the inferred project roots are recomputed, and the config file
    // registry is cleaned up. This is the shared mechanism that keeps the set of loaded
    // projects minimal for both LSP file opens and API file opens/closes.
    fn cleanup_configured_projects(&self, retain: Option<&Set<Path>>, logger: &LogTree) {
        let to_remove_projects: RefCell<Set<Path>> = RefCell::new(Set::default());
        self.configured_projects.range(|entry| {
            to_remove_projects.borrow_mut().add(entry.key().0);
            true
        });

        let retain_configured_project_and_references = |project: &Project| {
            // Retain project
            to_remove_projects.borrow_mut().delete(project.config_file_path());
            if let Some(program) = project.get_program() {
                program.range_resolved_project_reference(|reference_path, _, _, _| {
                    if self
                        .configured_projects
                        .load(&ConfiguredProjectID(reference_path.clone()))
                        .is_some()
                    {
                        to_remove_projects.borrow_mut().delete(reference_path);
                    }
                    true
                });
            }
        };

        let retain_default_configured_project = |open_file_path: &Path, project: &Project| {
            // Retain project and its references
            retain_configured_project_and_references(project);

            // Retain all the ancestor projects
            self.config_file_registry_builder.for_each_config_file_name_for(
                open_file_path,
                |config_file_name| {
                    if let Some(ancestor) = self.find_or_create_project(
                        config_file_name,
                        &(self.to_path)(config_file_name),
                        ProjectLoadKind::Find,
                        logger,
                    ) {
                        retain_configured_project_and_references(&ancestor.value().unwrap());
                    }
                },
            );
        };

        let mut inferred_project_files: Vec<String> = Vec::new();
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: retains projects in a set and collects inferred roots, which update_inferred_project sorts (as Go does); Go ranges the map too"
        )]
        for overlay in self.overlays.values() {
            let open_file = overlay.base.file_name.clone();
            let open_file_path = (self.to_path)(&open_file);
            match self.find_default_configured_project(&open_file, &open_file_path) {
                Some(p) => {
                    retain_default_configured_project(&open_file_path, &p.value().unwrap());
                }
                _ => {
                    inferred_project_files.push(open_file);
                }
            }
        }
        // Treat API-opened files like open files: retain their configured project (so
        // an LSP-driven open doesn't close it), or keep them as inferred project roots.
        let api_open_files: Vec<(Path, ApiOpenedFile)> = self
            .api_state
            .borrow()
            .open_files
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        for (path, file) in &api_open_files {
            if self.is_open_file(path) {
                continue;
            }
            match self.find_default_configured_project(&file.file_name, path) {
                Some(p) => {
                    retain_default_configured_project(path, &p.value().unwrap());
                }
                _ => {
                    inferred_project_files.push(file.file_name.clone());
                }
            }
        }

        let to_remove: Vec<Path> = to_remove_projects.borrow().keys().iter().cloned().collect();
        for project_path in to_remove {
            if retain.is_some_and(|r| r.has(&project_path)) {
                continue;
            }
            if self.api_state.borrow().open_projects.contains_key(&project_path) {
                continue;
            }
            if let Some(p) = self.configured_projects.load(&ConfiguredProjectID(project_path)) {
                self.delete_project(&ProjectEntry::Configured(p), logger);
            }
        }
        self.update_inferred_project_roots(inferred_project_files, logger);
        self.config_file_registry_builder.cleanup();
    }

    // projectcollectionbuilder.go:590
    fn collect_inferred_project_roots(&self) -> Vec<String> {
        let mut inferred_project_files: Vec<String> = Vec::new();
        // order-independent: collects inferred roots, which update_inferred_project sorts (as Go does); Go ranges the map too
        inferred_project_files.extend(self.overlays.iter().filter_map(|(path, overlay)| {
            if self.find_default_configured_project(&overlay.base.file_name, path).is_none() {
                Some(overlay.base.file_name.clone())
            } else {
                None
            }
        }));
        self.append_api_opened_inferred_roots(inferred_project_files)
    }

    // projectcollectionbuilder.go:603
    // appendAPIOpenedInferredRoots appends API-opened files that aren't open in an
    // overlay and have no configured project, so they're kept as inferred project
    // roots and persist across snapshots.
    fn append_api_opened_inferred_roots(
        &self,
        mut inferred_project_files: Vec<String>,
    ) -> Vec<String> {
        let api_open_files: Vec<(Path, ApiOpenedFile)> = self
            .api_state
            .borrow()
            .open_files
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        inferred_project_files.extend(api_open_files.iter().filter_map(|(path, file)| {
            if self.is_open_file(path) {
                return None;
            }
            if self.find_default_configured_project(&file.file_name, path).is_none() {
                Some(file.file_name.clone())
            } else {
                None
            }
        }));
        inferred_project_files
    }

    // projectcollectionbuilder.go:615
    fn cleanup_inferred_project(&self, logger: &LogTree) {
        self.update_inferred_project_roots(self.collect_inferred_project_roots(), logger);
    }

    // projectcollectionbuilder.go:619
    pub(crate) fn did_change_content_mapper_contributions(&self, logger: &LogTree) {
        self.cleanup_inferred_project(logger);
        if self.inferred_project.value().is_some() {
            self.update_program(&ProjectEntry::Inferred, logger);
        }
    }

    // projectcollectionbuilder.go:626
    fn ensure_inferred_project_includes_closed_file(&self, file_name: &str, logger: &LogTree) {
        // Collect existing inferred project roots (open files not in configured projects)
        // plus this closed file.
        let mut inferred_project_files = self.collect_inferred_project_roots();
        inferred_project_files.push(file_name.to_string());
        self.update_inferred_project_roots(inferred_project_files, logger);
        if self.inferred_project.value().is_some() {
            self.update_program(&ProjectEntry::Inferred, logger);
        }
    }

    // projectcollectionbuilder.go:639
    // DidRequestFile ensures projects are loaded for the given URI.
    // If configuredProjectsOnly is true, only configured projects are loaded; no inferred project is created
    // and it is not guaranteed that there will be any project containing the file in the resulting snapshot.
    pub(crate) fn did_request_file(
        &self,
        uri: &lsproto::DocumentUri,
        configured_projects_only: bool,
        logger: &LogTree,
    ) {
        let file_name = uri.file_name();
        let path = (self.to_path)(&file_name);
        self.did_request_file_path(&file_name, &path, configured_projects_only, logger);
    }

    // projectcollectionbuilder.go:645 (Go `didRequestFile`)
    fn did_request_file_path(
        &self,
        file_name: &str,
        path: &Path,
        configured_projects_only: bool,
        logger: &LogTree,
    ) {
        let start_time = Instant::now();
        if self.default_projects_invalidated.get() {
            self.ensure_configured_project_and_ancestors_for_file(file_name, path, logger);
            if !self.is_open_file(path) {
                return;
            }
        }
        if self.is_open_file(path) {
            let mut has_changes = self.program_structure_changed.get();

            // See if we can find a default project without updating a bunch of stuff.
            if let Some(result) = self.find_default_project(file_name, path) {
                has_changes = self.update_program(&result, logger) || has_changes;
                if self.value_of(&result).value().is_some_and(|p| p.contains_file(path)) {
                    if has_changes {
                        self.cleanup_inferred_project(logger);
                        if self.inferred_project.value().is_some() {
                            self.update_program(&ProjectEntry::Inferred, logger);
                        }
                    }
                    return;
                }
            }

            // Make sure all projects we know about are up to date...
            self.configured_projects.range(|entry| {
                has_changes = self
                    .update_program(&ProjectEntry::Configured(Arc::clone(entry)), logger)
                    || has_changes;
                true
            });
            if has_changes {
                // If the structure of other projects changed, we might need to move files
                // in/out of the inferred project.
                self.cleanup_inferred_project(logger);
            }

            if self.inferred_project.value().is_some() {
                self.update_program(&ProjectEntry::Inferred, logger);
            }

            // At this point we should be able to find the default project for the file without
            // creating anything else. Initially, I verified that and panicked if nothing was found,
            // but that panic was getting triggered by fourslash infrastructure when it told us to
            // open a package.json file. This is something the VS Code client would never do, but
            // it seems possible that another client would. There's no point in panicking; we don't
            // really even have an error condition until it tries to ask us language questions about
            // a non-TS-handleable file.
        } else {
            let result =
                self.ensure_configured_project_and_ancestors_for_file(file_name, path, logger);
            if result.project.is_none() && !configured_projects_only {
                // No configured project found for this closed file.
                // Add it to the inferred project so language service requests can be served.
                self.ensure_inferred_project_includes_closed_file(file_name, logger);
            }
        }

        if !logger.is_nil() {
            logger.log(&format!(
                "Completed file request for {} in {:?}",
                file_name,
                start_time.elapsed()
            ));
        }
    }

    // projectcollectionbuilder.go:707
    pub(crate) fn did_request_project(&self, project_id: &ID, logger: &LogTree) {
        let start_time = Instant::now();
        if project_id.inferred().is_some() {
            // Update inferred project
            if self.inferred_project.value().is_some() {
                self.update_program(&ProjectEntry::Inferred, logger);
            }
        } else if let Some(synthetic_id) = project_id.synthetic() {
            if let Some(entry) = self.synthetic_projects.load(&synthetic_id) {
                self.update_program(&ProjectEntry::Synthetic(entry), logger);
            }
        } else if let Some(configured_id) = project_id.configured()
            && let Some(entry) = self.configured_projects.load(&configured_id)
        {
            self.update_program(&ProjectEntry::Configured(entry), logger);
        }

        if !logger.is_nil() {
            logger.log(&format!(
                "Completed project update request for {} in {:?}",
                project_id,
                start_time.elapsed()
            ));
        }
    }

    // projectcollectionbuilder.go:730
    pub(crate) fn did_request_project_trees(
        &self,
        project_tree_request: &ProjectTreeRequest,
        logger: &LogTree,
    ) {
        let start_time = Instant::now();

        let mut current_projects: Vec<ConfiguredProjectID> = Vec::new();
        self.configured_projects.range(|sme| {
            current_projects.push(sme.key());
            true
        });

        let seen_projects: SyncSet<ConfiguredProjectID> = SyncSet::default();
        // Go queues these on a work group; tsrs's work group runs queued functions on the calling thread, last
        // queued first, which `projectTreeWork` reproduces with an explicit stack (the queued functions queue
        // more work on the same group).
        let wg: RefCell<Vec<ProjectTreeWork>> = RefCell::new(Vec::new());
        wg.borrow_mut().extend(current_projects.into_iter().map(ProjectTreeWork::Project));
        loop {
            let work = wg.borrow_mut().pop();
            match work {
                None => break,
                Some(ProjectTreeWork::Project(project_id)) => {
                    if let Some(entry) = self.configured_projects.load(&project_id) {
                        // If this project has potential project reference for any of the project we are loading ancestor tree for
                        // load this project first
                        if let Some(project) = entry.value()
                            && (project_tree_request.is_all_projects()
                                || project.has_potential_project_reference(project_tree_request))
                        {
                            self.update_program(
                                &ProjectEntry::Configured(Arc::clone(&entry)),
                                logger,
                            );
                        }
                        Self::ensure_project_tree(
                            &wg,
                            &entry,
                            project_tree_request,
                            &seen_projects,
                            logger,
                        );
                    }
                }
                Some(ProjectTreeWork::Child(program, child_config)) => {
                    if !project_tree_request.is_all_projects()
                        && program.range_resolved_project_reference_in_child_config(
                            Some(child_config),
                            |reference_path, _config, _, _| {
                                !project_tree_request.is_project_referenced(reference_path)
                            },
                        )
                    {
                        continue;
                    }

                    // Load this child project since this is referenced
                    let child_project_entry = self
                        .find_or_create_project(
                            child_config.config_name(),
                            child_config.config_file.unwrap().source_file.get().path(),
                            ProjectLoadKind::Create,
                            logger,
                        )
                        .unwrap();
                    self.update_program(
                        &ProjectEntry::Configured(Arc::clone(&child_project_entry)),
                        logger,
                    );

                    // Ensure children for this project
                    Self::ensure_project_tree(
                        &wg,
                        &child_project_entry,
                        project_tree_request,
                        &seen_projects,
                        logger,
                    );
                }
            }
        }

        // Updated configured projects may have moved open files in or out of the inferred project.
        // Callers iterate over all language service projects, so the inferred one needs a program too.
        self.cleanup_inferred_project(logger);
        if self.inferred_project.value().is_some() {
            self.update_program(&ProjectEntry::Inferred, logger);
        }

        if !logger.is_nil() {
            logger.log(&format!(
                "Completed project tree request for {:?} in {:?}",
                project_tree_request.projects(),
                start_time.elapsed()
            ));
        }
    }

    // projectcollectionbuilder.go:761
    fn ensure_project_tree(
        wg: &RefCell<Vec<ProjectTreeWork>>,
        entry: &Arc<SyncMapEntry<ConfiguredProjectID, Project>>,
        _project_tree_request: &ProjectTreeRequest,
        seen_projects: &SyncSet<ConfiguredProjectID>,
        _logger: &LogTree,
    ) {
        if !seen_projects.add_if_absent(entry.key()) {
            return;
        }

        let Some(project) = entry.value() else {
            return;
        };

        let Some(program) = project.get_program() else {
            return;
        };

        // If this project disables child load ignore it
        if program
            .command_line()
            .compiler_options()
            .unwrap()
            .disable_referenced_project_load
            .is_true()
        {
            return;
        }

        let children = program.get_resolved_project_references();
        wg.borrow_mut().extend(
            children
                .into_iter()
                .flatten()
                .map(|child_config| ProjectTreeWork::Child(program, child_config)),
        );
    }

    // projectcollectionbuilder.go:815
    pub(crate) fn did_update_ata_state(
        &self,
        ata_changes: &FxHashMap<ID, Arc<ATAStateChange>>,
        logger: &LogTree,
    ) {
        let update_project = |project: &dyn Value<Project>, ata_change: &ATAStateChange| {
            project.change_if(
                &mut |p: &Project| {
                    // Consistency check: the ATA demands (project options, unresolved imports) of this project
                    // has not changed since the time the ATA request was dispatched; the change can still be
                    // applied to this project in its current state.
                    ata_change.typings_info.as_ref().unwrap().equals(&p.compute_typings_info())
                },
                &mut |p: &mut Project| {
                    // We checked before triggering this change (in Session.triggerATAForUpdatedProjects) that
                    // the set of typings files is actually different.
                    p.installed_typings_info.clone_from(&ata_change.typings_info);
                    p.typings_files.clone_from(&ata_change.typings_files);
                    let typings_watch_globs = get_typings_locations_globs(
                        &ata_change.typings_files_to_watch,
                        &self.session_options.typings_location,
                        &self.session_options.current_directory,
                        &p.current_directory,
                        self.fs.fs.use_case_sensitive_file_names(),
                    );
                    p.typings_watch =
                        p.typings_watch.as_ref().map(|w| w.clone_with(typings_watch_globs));
                    p.dirty = true;
                    p.dirty_file_path = Path::default();
                },
            );
        };

        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: updates each project's ATA state by its own ID; the order only reaches log lines; Go ranges the map too"
        )]
        for (project_id, ata_change) in ata_changes {
            logger.embed(&ata_change.logs);
            if project_id.inferred().is_some() {
                update_project(&self.inferred_project, ata_change);
            } else if let Some(synthetic_project_id) = project_id.synthetic() {
                if let Some(project) = self.synthetic_projects.load(&synthetic_project_id) {
                    update_project(&project, ata_change);
                }
            } else if let Some(configured_id) = project_id.configured()
                && let Some(project) = self.configured_projects.load(&configured_id)
            {
                update_project(&project, ata_change);
            }

            if !logger.is_nil() {
                logger.log(&format!("Updated ATA state for project {project_id}"));
            }
        }
    }

    // projectcollectionbuilder.go:867
    // if customConfigFileName changes, invalidate default projects.
    pub(crate) fn did_change_custom_config_file_name(&self, logger: &LogTree) {
        if !self.config_file_registry_builder.did_change_custom_config_file_name(logger) {
            return;
        }

        *self.file_default_projects.borrow_mut() = None;
        self.default_projects_invalidated.set(true);
        self.program_structure_changed.set(true);
    }

    // projectcollectionbuilder.go:877
    pub(crate) fn did_change_user_preferences(
        &self,
        old_preferences: &UserPreferences,
        new_preferences: &UserPreferences,
        logger: &LogTree,
    ) {
        if old_preferences.locale == new_preferences.locale {
            return;
        }
        self.for_each_project(|entry| {
            self.value_of(entry).change(&mut |project: &mut Project| {
                project.dirty = true;
                project.dirty_file_path = Path::default();
                logger.logf(format_args!(
                    "Marking project as dirty due to locale change: {}",
                    project.id()
                ));
            });
            true
        });
    }

    // projectcollectionbuilder.go:893
    fn mark_projects_affected_by_config_changes(
        &self,
        config_change_result: &ChangeFileResult,
        logger: &LogTree,
    ) -> bool {
        for project_id in config_change_result.affected_projects.iter().flatten() {
            let mut project: Option<ProjectEntry> = None;
            if project_id.inferred().is_some() {
                project = Some(ProjectEntry::Inferred);
            } else {
                if let Some(synthetic_project_id) = project_id.synthetic()
                    && let Some(synthetic_project) =
                        self.synthetic_projects.load(&synthetic_project_id)
                {
                    project = Some(ProjectEntry::Synthetic(synthetic_project));
                }
                if project.is_none()
                    && let Some(configured_id) = project_id.configured()
                {
                    project =
                        self.configured_projects.load(&configured_id).map(ProjectEntry::Configured);
                }
            }
            let project = match &project {
                Some(p) if self.value_of(p).value().is_some() => p,
                _ => panic!("project {project_id} affected by config change not found"),
            };
            self.value_of(project).change_if(
                &mut |p: &Project| !p.dirty || !p.dirty_file_path.0.is_empty(),
                &mut |p: &mut Project| {
                    p.dirty = true;
                    p.dirty_file_path = Path::default();
                    logger.logf(format_args!(
                        "Marking project {project_id} as dirty due to change affecting config"
                    ));
                },
            );
        }

        // Recompute default projects for open files that now have different config file presence.
        let mut has_changes = false;
        for path in config_change_result.affected_files.iter().flatten() {
            let Some(overlay) = self.overlays.get(path) else {
                continue;
            };
            let file_name = overlay.base.file_name.clone();
            let _ = self.ensure_configured_project_and_ancestors_for_file(&file_name, path, logger);
            has_changes = true;
        }

        has_changes
    }

    // projectcollectionbuilder.go:944
    fn find_default_project(&self, file_name: &str, path: &Path) -> Option<ProjectEntry> {
        if let Some(configured_project) = self.find_default_configured_project(file_name, path) {
            return Some(ProjectEntry::Configured(configured_project));
        }
        if let Some(key) = self.file_default_project(path)
            && key.inferred().is_some()
        {
            return Some(ProjectEntry::Inferred);
        }
        if let Some(inferred_project) = self.inferred_project.value()
            && inferred_project.contains_file(path)
        {
            self.set_file_default_project(path.clone(), inferred_project_id().as_id());
            return Some(ProjectEntry::Inferred);
        }
        None
    }

    // projectcollectionbuilder.go:963
    fn find_default_configured_project(
        &self,
        file_name: &str,
        path: &Path,
    ) -> Option<Arc<SyncMapEntry<ConfiguredProjectID, Project>>> {
        if let Some(key) = self.file_default_project(path)
            && let Some(configured_id) = key.configured()
            && let Some(entry) = self.configured_projects.load(&configured_id)
        {
            return Some(entry);
        }
        // Sort configured projects so we can use a deterministic "first" as a last resort.
        let mut configured_project_paths: Vec<Path> = Vec::new();
        let mut configured_projects: FxHashMap<
            Path,
            Arc<SyncMapEntry<ConfiguredProjectID, Project>>,
        > = FxHashMap::default();
        self.configured_projects.range(|entry| {
            let configured_path = entry.key().0;
            configured_project_paths.push(configured_path.clone());
            configured_projects.insert(configured_path, Arc::clone(entry));
            true
        });
        configured_project_paths.sort();

        let (project, multiple_candidates) = find_default_configured_project_from_program_inclusion(
            file_name,
            path,
            &configured_project_paths,
            |path| configured_projects[path].value().unwrap(),
        );

        if multiple_candidates
            && let Some(p) = self
                .find_or_create_default_configured_project_for_file(
                    file_name,
                    path,
                    ProjectLoadKind::Find,
                    &LogTree::nil(),
                )
                .project
        {
            return Some(p);
        }

        configured_projects.get(&project).cloned()
    }

    // projectcollectionbuilder.go:995
    fn ensure_configured_project_and_ancestors_for_file(
        &self,
        file_name: &str,
        path: &Path,
        logger: &LogTree,
    ) -> SearchResult {
        let mut result = self.find_or_create_default_configured_project_for_file(
            file_name,
            path,
            ProjectLoadKind::Create,
            logger,
        );
        if result.project.is_some() && self.is_open_file(path) {
            self.create_ancestor_tree(file_name, path, &mut result, logger);
        }
        result
    }

    // projectcollectionbuilder.go:1003
    fn create_ancestor_tree(
        &self,
        file_name: &str,
        path: &Path,
        open_result: &mut SearchResult,
        logger: &LogTree,
    ) {
        let mut project = open_result.project.as_ref().unwrap().value().unwrap();
        loop {
            // Skip if project is not composite and we are only looking for solution
            if let Some(command_line) = project.command_line {
                let options = command_line.compiler_options().unwrap();
                if !options.composite.is_true() || options.disable_solution_searching.is_true() {
                    return;
                }
            }

            // Get config file name
            let ancestor_config_name = self
                .config_file_registry_builder
                .get_ancestor_config_file_name(file_name, path, project.config_file_name(), logger);
            if ancestor_config_name.is_empty() {
                return;
            }

            // find or delay load the project
            let ancestor_path = (self.to_path)(&ancestor_config_name);
            let Some(ancestor) = self.find_or_create_project(
                &ancestor_config_name,
                &ancestor_path,
                ProjectLoadKind::Create,
                logger,
            ) else {
                return;
            };

            open_result.retain.add(ancestor_path);

            // If this ancestor is new and was not updated because we are just creating it for future loading
            // eg when invoking find all references or rename that could span multiple projects
            // we would make the current project as its potential project reference
            if ancestor.value().unwrap().command_line.is_none()
                && project.command_line.is_none_or(|command_line| {
                    command_line.compiler_options().unwrap().composite.is_true()
                })
            {
                let config_file_path = project.config_file_path.clone();
                ancestor.change(|ancestor_project| {
                    ancestor_project.set_potential_project_reference(config_file_path);
                });
            }

            project = ancestor.value().unwrap();
        }
    }

    // projectcollectionbuilder.go:1058
    fn find_or_create_default_configured_project_worker(
        &self,
        file_name: &str,
        path: &Path,
        config_file_name: &str,
        load_kind: ProjectLoadKind,
        visited: Option<Arc<SyncSet<SearchNodeKey>>>,
        mut fallback: Option<SearchResult>,
        logger: &LogTree,
    ) -> SearchResult {
        let configs: SyncMap<Path, P<ParsedCommandLine>> = SyncMap::default();
        let visited = visited.unwrap_or_default();

        let mut preprocess_level =
            |level: &mut BreadthFirstSearchLevel<'_, SearchNodeKey, SearchNode>| {
                let mut to_delete: Vec<SearchNodeKey> = Vec::new();
                level.range(|node| {
                    if node.load_kind == ProjectLoadKind::Find
                        && level.has(&SearchNodeKey {
                            config_file_name: node.config_file_name.clone(),
                            load_kind: ProjectLoadKind::Create,
                        })
                    {
                        // Remove find requests when a create request for the same project is already present.
                        to_delete.push(SearchNodeKey {
                            config_file_name: node.config_file_name.clone(),
                            load_kind: node.load_kind,
                        });
                    }
                    true
                });
                for key in &to_delete {
                    level.delete(key);
                }
            };
        let search = breadth_first_search_parallel_ex(
            SearchNode {
                config_file_name: config_file_name.to_string(),
                load_kind,
                logger: logger.clone(),
            },
            |node: &SearchNode| {
                if let Some(config) = configs.load(&(self.to_path)(&node.config_file_name))
                    && !config.project_references().is_empty()
                {
                    let mut reference_load_kind = node.load_kind;
                    if config.compiler_options().unwrap().disable_referenced_project_load.is_true()
                    {
                        reference_load_kind = ProjectLoadKind::Find;
                    }

                    let mut ref_logger = LogTree::nil();
                    let references = config.resolved_project_reference_paths();
                    if !references.is_empty() && !node.logger.is_nil() {
                        ref_logger = node.logger.fork(&format!(
                            "Searching {} project references of {}",
                            references.len(),
                            node.config_file_name
                        ));
                    }
                    return references
                        .iter()
                        .map(|config_file_name| SearchNode {
                            config_file_name: config_file_name.clone(),
                            load_kind: reference_load_kind,
                            logger: ref_logger
                                .fork(&format!("Searching project reference {config_file_name}")),
                        })
                        .collect();
                }
                Vec::new()
            },
            |node: &SearchNode| {
                let config_file_path = (self.to_path)(&node.config_file_name);
                let Some(config) =
                    self.config_file_registry_builder.find_or_acquire_config_for_file(
                        &node.config_file_name,
                        &config_file_path,
                        path,
                        node.load_kind,
                        &node.logger.fork("Acquiring config for open file"),
                    )
                else {
                    node.logger.log("Config file for project does not already exist");
                    return (false, false);
                };
                configs.store(config_file_path.clone(), config);
                if config.file_names().is_empty() {
                    // Likely a solution tsconfig.json - the search will fan out to its references.
                    node.logger.log("Project does not contain file (no root files)");
                    return (false, false);
                }

                if config.compiler_options().unwrap().composite == Tristate::True {
                    // For composite projects, we can get an early negative result.
                    // !!! what about declaration files in node_modules? wouldn't it be better to
                    //     check project inclusion if the project is already loaded?
                    if !config.file_names_by_path().contains_key(path) {
                        node.logger
                            .log("Project does not contain file (by composite config inclusion)");
                        return (false, false);
                    }
                }

                let Some(project) = self.find_or_create_project(
                    &node.config_file_name,
                    &config_file_path,
                    node.load_kind,
                    &node.logger,
                ) else {
                    node.logger.log("Project does not already exist");
                    return (false, false);
                };

                if node.load_kind == ProjectLoadKind::Create {
                    // Ensure project is up to date before checking for file inclusion
                    self.update_program(
                        &ProjectEntry::Configured(Arc::clone(&project)),
                        &node.logger,
                    );
                }

                let value = project.value().unwrap();
                if value.contains_file(path) {
                    let is_direct_inclusion = !value.is_source_from_project_reference(path);
                    if !node.logger.is_nil() {
                        node.logger.logf(format_args!(
                            "Project contains file {}",
                            if is_direct_inclusion {
                                "directly"
                            } else {
                                "as a source of a referenced project"
                            }
                        ));
                    }
                    return (true, is_direct_inclusion);
                }

                node.logger.log("Project does not contain file");
                (false, false)
            },
            BreadthFirstSearchOptions {
                visited: Some(&visited),
                preprocess_level: Some(&mut preprocess_level),
            },
            |node: &SearchNode| SearchNodeKey {
                config_file_name: node.config_file_name.clone(),
                load_kind: node.load_kind,
            },
        );

        let mut retain: Set<Path> = Set::default();
        let mut project: Option<Arc<SyncMapEntry<ConfiguredProjectID, Project>>> = None;
        if !search.path.is_empty() {
            project = self
                .configured_projects
                .load(&ConfiguredProjectID((self.to_path)(&search.path[0].config_file_name)));
            // If we found a project, we retain each project along the BFS path.
            // We don't want to retain everything we visited since BFS can terminate
            // early, and we don't want to retain nondeterministically.
            retain.m.extend(search.path.iter().map(|node| (self.to_path)(&node.config_file_name)));
        }

        if search.stopped {
            // Found a project that directly contains the file.
            return SearchResult { project, retain };
        }

        if project.is_some() {
            // If we found a project that contains the file, but it is a source from
            // a project reference, record it as a fallback.
            fallback = Some(SearchResult { project, retain: retain.clone() });
        }

        // Look for tsconfig.json files higher up the directory tree and do the same. This handles
        // the common case where a higher-level "solution" tsconfig.json contains all projects in a
        // workspace.
        if let Some(config) = configs.load(&(self.to_path)(config_file_name))
            && config.compiler_options().unwrap().disable_solution_searching.is_true()
            && let Some(fallback) = fallback
        {
            return fallback;
        }
        let ancestor_config_name = self.config_file_registry_builder.get_ancestor_config_file_name(
            file_name,
            path,
            config_file_name,
            logger,
        );
        if !ancestor_config_name.is_empty() {
            return self.find_or_create_default_configured_project_worker(
                file_name,
                path,
                &ancestor_config_name,
                load_kind,
                Some(visited),
                fallback,
                &logger.fork(&format!("Searching ancestor config file at {ancestor_config_name}")),
            );
        }
        if let Some(fallback) = fallback {
            return fallback;
        }
        // If we didn't find anything, we can retain everything we visited,
        // since the whole graph must have been traversed (i.e., the set of
        // retained projects is guaranteed to be deterministic).
        visited.range(|node| {
            retain.add((self.to_path)(&node.config_file_name));
            true
        });
        SearchResult { project: None, retain }
    }

    // projectcollectionbuilder.go:1216
    fn find_or_create_default_configured_project_for_file(
        &self,
        file_name: &str,
        path: &Path,
        load_kind: ProjectLoadKind,
        logger: &LogTree,
    ) -> SearchResult {
        if let Some(key) = self.file_default_project(path) {
            if key.inferred().is_some() {
                // The file belongs to the inferred project
                return SearchResult::default();
            }
            let configured_id = key.configured().unwrap_or_default();
            let entry = self.configured_projects.load(&configured_id);
            return SearchResult { project: entry, retain: Set::default() };
        }
        let config_file_name = self
            .config_file_registry_builder
            .get_config_file_name_for_file(file_name, path, logger);
        if !config_file_name.is_empty() {
            let start_time = Instant::now();
            let result = self.find_or_create_default_configured_project_worker(
                file_name,
                path,
                &config_file_name,
                load_kind,
                None,
                None,
                &logger.fork(&format!("Searching for default configured project for {file_name}")),
            );
            if let Some(project) = &result.project {
                self.set_file_default_project(path.clone(), project.value().unwrap().id());
            }
            if !logger.is_nil() {
                let elapsed = start_time.elapsed();
                match &result.project {
                    Some(project) => logger.log(&format!(
                        "Found default configured project for {}: {} (in {:?})",
                        file_name,
                        project.value().unwrap().config_file_name(),
                        elapsed
                    )),
                    None => logger.log(&format!("No default configured project found for {file_name} (searched in {elapsed:?})")),
                }
            }
            return result;
        }
        SearchResult::default()
    }

    // projectcollectionbuilder.go:1261
    fn find_or_create_project(
        &self,
        config_file_name: &str,
        config_file_path: &Path,
        load_kind: ProjectLoadKind,
        logger: &LogTree,
    ) -> Option<Arc<SyncMapEntry<ConfiguredProjectID, Project>>> {
        if load_kind == ProjectLoadKind::Find {
            return self.configured_projects.load(&ConfiguredProjectID(config_file_path.clone()));
        }
        let (entry, _) = self.configured_projects.load_or_store(
            ConfiguredProjectID(config_file_path.clone()),
            Shared::new(new_configured_project(config_file_name, config_file_path, self, logger)),
        );
        entry
    }

    // projectcollectionbuilder.go:1275
    fn update_inferred_project_roots(
        &self,
        root_file_names: Vec<String>,
        logger: &LogTree,
    ) -> bool {
        let root_file_names: Vec<String> = root_file_names
            .into_iter()
            .filter(|f| self.is_supported_in_inferred_project(f))
            .collect();
        let mut project_references: Vec<ProjectReference> = Vec::new();
        let mut config_file_parsing_diagnostics: Vec<P<Diagnostic>> = Vec::new();
        if let Some(project) = self.inferred_project.value() {
            let command_line = project.command_line.unwrap();
            project_references = command_line.project_references().to_vec();
            config_file_parsing_diagnostics.clone_from(&command_line.errors);
        }
        self.update_inferred_project(
            root_file_names,
            self.compiler_options_for_inferred_projects,
            project_references,
            config_file_parsing_diagnostics,
            self.inferred_content_mappers.clone(),
            logger,
        )
    }

    // projectcollectionbuilder.go:1286
    fn update_or_create_synthetic_project(
        &self,
        project_id: SyntheticProjectID,
        root_file_names: Vec<String>,
        compiler_options: Option<P<CompilerOptions>>,
        project_references: Vec<ProjectReference>,
        config_file_parsing_diagnostics: Vec<P<Diagnostic>>,
        module_resolver_factory: ModuleResolverFactoryRef,
        module_resolver_id: u64,
        content_mappers: Vec<Mapper>,
        logger: &LogTree,
    ) -> Arc<SyncMapEntry<SyntheticProjectID, Project>> {
        let Some(project) = self.synthetic_projects.load(&project_id) else {
            let mut synthetic_project = new_synthetic_project(
                &project_id,
                &self.session_options.current_directory,
                compiler_options.unwrap(),
                root_file_names,
                project_references,
                content_mappers,
                config_file_parsing_diagnostics,
                self,
                logger,
            );
            synthetic_project.module_resolver_factory = module_resolver_factory;
            synthetic_project.module_resolver_id = module_resolver_id;
            let (project, _) =
                self.synthetic_projects.load_or_store(project_id, Shared::new(synthetic_project));
            return project.unwrap();
        };

        let current_project = project.value().unwrap();
        let compiler_options = compiler_options
            .unwrap_or_else(|| current_project.command_line.unwrap().compiler_options().unwrap());
        let new_command_line = new_inferred_project_command_line(
            compiler_options,
            root_file_names.clone(),
            project_references.clone(),
            content_mappers,
            ComparePathsOptions {
                use_case_sensitive_file_names: self.fs.fs.use_case_sensitive_file_names(),
                current_directory: current_project.current_directory.clone(),
            },
            config_file_parsing_diagnostics.clone(),
        );
        project.change_if(
            |p| {
                let command_line = p.command_line.unwrap();
                command_line.file_names() != new_command_line.file_names()
                    || !compiler_options_deep_equal(
                        command_line.compiler_options(),
                        Some(compiler_options),
                    )
                    || !project_references_equal(
                        command_line.project_references(),
                        &project_references,
                    )
                    || !diagnostics_equal(&command_line.errors, &config_file_parsing_diagnostics)
                    || command_line.content_mappers() != new_command_line.content_mappers()
                    || p.module_resolver_id != module_resolver_id
            },
            |p| {
                if !logger.is_nil() {
                    logger.log(&format!(
                        "Updating synthetic project config with {} root files",
                        root_file_names.len()
                    ));
                }
                p.set_command_line(Some(new_command_line));
                p.module_resolver_factory.clone_from(&module_resolver_factory);
                p.module_resolver_id = module_resolver_id;
            },
        );
        project
    }

    // projectcollectionbuilder.go:1338
    // updateInferredProject preserves the current command line when roots/options are unchanged.
    pub(crate) fn update_inferred_project(
        &self,
        root_file_names: Vec<String>,
        compiler_options: Option<P<CompilerOptions>>,
        project_references: Vec<ProjectReference>,
        config_file_parsing_diagnostics: Vec<P<Diagnostic>>,
        content_mappers: Vec<Mapper>,
        logger: &LogTree,
    ) -> bool {
        if root_file_names.is_empty() {
            return self.delete_inferred_project(logger);
        }
        let mut root_file_names = root_file_names;
        root_file_names.sort();
        self.update_or_create_inferred_project(
            root_file_names,
            compiler_options,
            project_references,
            config_file_parsing_diagnostics,
            content_mappers,
            logger,
        )
    }

    // projectcollectionbuilder.go:1354
    fn delete_inferred_project(&self, logger: &LogTree) -> bool {
        let Some(project) = self.inferred_project.value() else {
            return false;
        };
        if !logger.is_nil() {
            logger.log("Deleting inferred project");
        }
        if let Some(program) = project.program {
            program.range_resolved_project_reference(|reference_path, _, _, _| {
                self.config_file_registry_builder
                    .release_config_for_project(reference_path, &project.id());
                true
            });
        }
        self.inferred_project.delete();
        true
    }

    // projectcollectionbuilder.go:1374
    // updateOrCreateInferredProject always retains an inferred project, including when rootFileNames is empty.
    // The caller transfers ownership of rootFileNames.
    fn update_or_create_inferred_project(
        &self,
        root_file_names: Vec<String>,
        compiler_options: Option<P<CompilerOptions>>,
        project_references: Vec<ProjectReference>,
        config_file_parsing_diagnostics: Vec<P<Diagnostic>>,
        content_mappers: Vec<Mapper>,
        logger: &LogTree,
    ) -> bool {
        let Some(project) = self.inferred_project.value() else {
            let project = new_inferred_project(
                &self.session_options.current_directory,
                compiler_options,
                root_file_names,
                project_references,
                content_mappers,
                config_file_parsing_diagnostics,
                self,
                logger,
            );
            self.inferred_project.set(Some(Shared::new(project)));
            return true;
        };

        let compiler_options = compiler_options
            .unwrap_or_else(|| project.command_line.unwrap().compiler_options().unwrap());
        let new_command_line = new_inferred_project_command_line(
            compiler_options,
            root_file_names.clone(),
            project_references.clone(),
            content_mappers,
            ComparePathsOptions {
                use_case_sensitive_file_names: self.fs.fs.use_case_sensitive_file_names(),
                current_directory: project.current_directory.clone(),
            },
            config_file_parsing_diagnostics.clone(),
        );
        let changed = self.inferred_project.change_if(
            &mut |p: &Project| {
                let command_line = p.command_line.unwrap();
                command_line.file_names() != new_command_line.file_names()
                    || !compiler_options_deep_equal(
                        command_line.compiler_options(),
                        Some(compiler_options),
                    )
                    || !project_references_equal(
                        command_line.project_references(),
                        &project_references,
                    )
                    || !diagnostics_equal(&command_line.errors, &config_file_parsing_diagnostics)
                    || command_line.content_mappers() != new_command_line.content_mappers()
            },
            &mut |p: &mut Project| {
                if !logger.is_nil() {
                    logger.log(&format!(
                        "Updating inferred project config with {} root files",
                        root_file_names.len()
                    ));
                }
                p.set_command_line(Some(new_command_line));
            },
        );
        if !changed {
            return false;
        }
        true
    }

    // projectcollectionbuilder.go:1428
    fn is_supported_in_inferred_project(&self, file_name: &str) -> bool {
        if tspath::is_dynamic_file_name(file_name)
            || get_script_kind_from_file_name(file_name) != ScriptKind::Unknown
        {
            return true;
        }
        if let Some(file) = self.fs.get_file(file_name)
            && file.is_overlay()
            && tspath::get_any_extension_from_path(file_name, &[], false).is_empty()
        {
            return true;
        }
        let extensions: Vec<&str> = self
            .inferred_content_mapper_extensions
            .iter()
            .map(std::string::String::as_str)
            .collect();
        tspath::file_extension_is_one_of(file_name, &extensions)
    }

    // projectcollectionbuilder.go:1440
    // updateProgram updates the program for the given project entry if necessary. It returns
    // a boolean indicating whether the update could have caused any structure-affecting changes.
    fn update_program(&self, entry: &ProjectEntry, logger: &LogTree) -> bool {
        let value = self.value_of(entry);
        let mut update_program = false;
        let mut delete_project = false;
        let mut files_changed = false;
        let project_id = value.value().unwrap().id();
        let start_time = Instant::now();
        let mut notified_loading = false;
        let mut display_name = String::new();
        value.locked(&mut |entry: &dyn Value<Project>| {
            let current = entry.value().unwrap();
            if current.kind == Kind::Configured {
                let command_line = self.config_file_registry_builder.acquire_config_for_project(
                    current.config_file_name(),
                    &current.config_file_path,
                    &current.id(),
                    &logger.fork("Acquiring config for project"),
                );
                let Some(command_line) = command_line else {
                    delete_project = true;
                    files_changed = true;
                    return;
                };
                if current.command_line != Some(command_line) {
                    update_program = true;
                    entry.change(&mut |p: &mut Project| {
                        p.set_command_line(Some(command_line));
                    });
                }
            }
            if !update_program {
                update_program = entry.value().unwrap().dirty;
            }
            if update_program && self.client.is_some() {
                display_name =
                    entry.value().unwrap().display_name(&self.session_options.current_directory);
                notified_loading = true;
            }
        });
        if notified_loading && let Some(client) = &self.client {
            client.progress_start(&diagnostics::PROJECT_0, &[&display_name]);
        }
        if delete_project {
            self.delete_project(entry, logger);
        }
        if update_program {
            value.locked(&mut |entry: &dyn Value<Project>| {
                entry.change(&mut |project: &mut Project| {
                    let old_host = project.host.clone();
                    let old_program = project.program;
                    // Keeps the old program alive until `release_dropped_project_references` below has read it:
                    // replacing `program_owner` drops the last reference to a program built earlier in the same
                    // snapshot build, which frees it (and its shared data). Found with AddressSanitizer.
                    let _old_program_owner = project.program_owner.clone();
                    let old_checker_pool = project.checker_pool.clone();
                    project.host = Some(new_compiler_host(
                        &project.current_directory.clone(),
                        project,
                        self,
                        logger.fork("CompilerHost"),
                    ));
                    let result = project.create_program();
                    let mut watched_files: Vec<String> = Vec::new();
                    watched_files.extend(
                        project.command_line.unwrap().content_mappers().iter().filter_map(
                            |mapper| {
                                if !mapper.definition.package.is_empty()
                                    && mapper.contribution_id.is_empty()
                                    && !mapper.package_directory.is_empty()
                                {
                                    Some(tspath::combine_paths(
                                        &mapper.package_directory,
                                        &["package.json"],
                                    ))
                                } else {
                                    None
                                }
                            },
                        ),
                    );
                    // (Content mappers are not ported: no source file is content-mapped, so there are no dynamic
                    // content mapper watched files.)
                    watched_files.sort();
                    watched_files.dedup();
                    let mut content_mapper_watched_files =
                        new_set_with_size_hint(watched_files.len());
                    content_mapper_watched_files
                        .m
                        .extend(watched_files.iter().map(|file_name| (self.to_path)(file_name)));
                    project.content_mapper_watch = project
                        .content_mapper_watch
                        .as_ref()
                        .map(|w| w.clone_with(watched_files.clone()));
                    project.content_mapper_watched_files =
                        Some(Arc::new(content_mapper_watched_files));
                    project.program = Some(result.program);
                    project.checker_pool.clone_from(&result.checker_pool);
                    project.program_owner = Some(Arc::clone(&result.owner));
                    project.program_update_kind = result.update_kind;
                    project.program_last_update = self.new_snapshot_id;
                    if result.update_kind == ProgramUpdateKind::Cloned {
                        project
                            .host
                            .as_ref()
                            .unwrap()
                            .source_fs
                            .set_seen_files(old_host.as_ref().unwrap().source_fs.seen_files());
                    }
                    if result.update_kind == ProgramUpdateKind::NewFiles {
                        files_changed = true;
                        project.program_files_watch = project.clone_watchers();
                    }
                    project.dirty = false;
                    project.dirty_file_path = Path::default();
                    self.release_dropped_project_references(
                        old_program,
                        Some(result.program),
                        &project.id(),
                    );
                    if let Some(old_checker_pool) = &old_checker_pool {
                        old_checker_pool.discard();
                    }
                });
            });
        }
        if notified_loading && let Some(client) = &self.client {
            client.progress_finish(&diagnostics::PROJECT_0, &[&display_name]);
        }
        if update_program && !logger.is_nil() {
            logger.log(&format!(
                "Program update for {} completed in {:?}",
                project_id,
                start_time.elapsed()
            ));
        }
        files_changed
    }

    // projectcollectionbuilder.go:1543
    fn mark_files_changed(
        &self,
        entry: &ProjectEntry,
        paths: &[Path],
        change_type: lsproto::FileChangeType,
        logger: &LogTree,
    ) {
        let dirty_file_path: RefCell<Path> = RefCell::new(Path::default());
        self.value_of(entry).change_if(
            &mut |p: &Project| {
                let mut dirty = false;
                if p.program.is_none() || p.dirty && p.dirty_file_path.0.is_empty() {
                    return false;
                }

                let mut dirty_file_path = dirty_file_path.borrow_mut();
                *dirty_file_path = p.dirty_file_path.clone();
                for path in paths {
                    if p.contains_file(path) {
                        dirty = true;
                        if change_type == lsproto::FileChangeType::DELETED {
                            *dirty_file_path = Path::default();
                            break;
                        }
                        // package.json changes can affect module resolution and package
                        // identity (e.g. dedup decisions), so they must always trigger
                        // a full rebuild rather than a single-file clone.
                        if tspath::get_base_file_name(path) == "package.json" {
                            *dirty_file_path = Path::default();
                            break;
                        }
                        if dirty_file_path.0.is_empty() {
                            *dirty_file_path = path.clone();
                        } else if *dirty_file_path != *path {
                            *dirty_file_path = Path::default();
                            break;
                        }
                    } else if let Some(host) = &p.host
                        && (change_type == lsproto::FileChangeType::CREATED
                            && host.source_fs.seen_file_or_missing_parent_directory(path)
                            || change_type != lsproto::FileChangeType::CREATED
                                && host.source_fs.seen_file(path))
                    {
                        dirty = true;
                        *dirty_file_path = Path::default();
                        break;
                    }
                }
                dirty || p.dirty_file_path != *dirty_file_path
            },
            &mut |p: &mut Project| {
                let dirty_file_path = dirty_file_path.borrow();
                p.dirty = true;
                p.dirty_file_path = dirty_file_path.clone();
                if !logger.is_nil() {
                    if dirty_file_path.0.is_empty() {
                        logger.logf(format_args!("Marking project {} as dirty", p.id()));
                    } else {
                        logger.logf(format_args!(
                            "Marking project {} as dirty due to changes in {}",
                            p.id(),
                            dirty_file_path.0
                        ));
                    }
                }
            },
        );
    }

    // projectcollectionbuilder.go:1597
    fn delete_project(&self, project: &ProjectEntry, logger: &LogTree) {
        let entry = self.value_of(project);
        let value = entry.value().unwrap();
        let project_id = value.id();
        if !logger.is_nil() {
            logger.logf(format_args!("Deleting {} project: {}", value.kind.string(), value.id()));
        }
        if let Some(program) = value.program {
            program.range_resolved_project_reference(|reference_path, _, _, _| {
                self.config_file_registry_builder
                    .release_config_for_project(reference_path, &project_id);
                true
            });
        }
        if value.kind == Kind::Configured {
            self.config_file_registry_builder
                .release_config_for_project(value.config_file_path(), &project_id);
        }
        entry.delete();
    }

    // projectcollectionbuilder.go:1619
    // releaseDroppedProjectReferences releases the config entries for project references
    // that were present in oldProgram but are no longer referenced by newProgram. Creating
    // newProgram already re-acquires the config for every reference it still resolves, so
    // only the dropped references need to be released here.
    fn release_dropped_project_references(
        &self,
        old_program: Option<&'static Program>,
        new_program: Option<&'static Program>,
        project_id: &ID,
    ) {
        let Some(old_program) = old_program else {
            return;
        };
        if new_program.is_some_and(|new_program| std::ptr::eq(old_program, new_program)) {
            return;
        }
        let mut new_references: Set<Path> = Set::default();
        if let Some(new_program) = new_program {
            new_program.range_resolved_project_reference(|reference_path, _, _, _| {
                new_references.add(reference_path.clone());
                true
            });
        }
        old_program.range_resolved_project_reference(|reference_path, _, _, _| {
            if !new_references.has(reference_path) {
                self.config_file_registry_builder
                    .release_config_for_project(reference_path, project_id);
            }
            true
        });
    }
}

// projectcollectionbuilder.go:581
fn log_change_file_result(result: &ChangeFileResult, logger: &LogTree) {
    if let Some(affected_projects) = &result.affected_projects
        && !affected_projects.is_empty()
    {
        logger.logf(format_args!(
            "Config file change affected projects: {:?}",
            affected_projects.iter().map(|p| p.0.as_str()).collect::<Vec<_>>()
        ));
    }
    if let Some(affected_files) = &result.affected_files
        && !affected_files.is_empty()
    {
        logger.logf(format_args!(
            "Config file change affected config file lookups for {} files",
            affected_files.len()
        ));
    }
}

// A function queued on `DidRequestProjectTrees`' work group.
enum ProjectTreeWork {
    Project(ConfiguredProjectID),
    Child(&'static Program, P<ParsedCommandLine>),
}

#[derive(Clone)]
struct SearchNode {
    config_file_name: String,
    load_kind: ProjectLoadKind,
    logger: LogTree,
}

#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct SearchNodeKey {
    config_file_name: String,
    load_kind: ProjectLoadKind,
}

#[derive(Clone, Default)]
pub struct SearchResult {
    project: Option<Arc<SyncMapEntry<ConfiguredProjectID, Project>>>,
    retain: Set<Path>,
}

// projectcollectionbuilder.go:1419
fn project_references_equal(a: &[ProjectReference], b: &[ProjectReference]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(a, b)| a.path == b.path && a.circular == b.circular)
}
