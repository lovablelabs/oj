use std::sync::{Arc, OnceLock};

use rustc_hash::FxHashMap;
use tsrs_core::collections::{OrderedMap, Set, SyncSet, new_set_with_size_hint};
use tsrs_core::tspath::Path;
use tsrs_core::{BreadthFirstSearchOptions, breadth_first_search_parallel_ex};
use tsrs_ls as ls;

use crate::configfileregistry::ConfigFileRegistry;
use crate::dirty::{Shared, SharedMap};
use crate::overlayfs::{OverlayMap, ToPath};
use crate::project::{ConfiguredProjectID, ID, Project, SyntheticProjectID};

#[derive(Clone)]
pub struct ProjectCollection {
    to_path: ToPath,
    pub(crate) config_file_registry: Arc<ConfigFileRegistry>,
    // fileDefaultProjects is a map of file paths to the ID of the default project
    // for that file. This map
    // contains quick lookups for only the associations discovered during the latest
    // snapshot update.
    pub(crate) file_default_projects: Option<Arc<FxHashMap<Path, ID>>>,
    // configuredProjects is the set of loaded projects associated with a tsconfig
    // file, keyed by the config file path.
    pub(crate) configured_projects: SharedMap<ConfiguredProjectID, Project>,
    // syntheticProjects contains synthetic projects created explicitly through the API.
    pub(crate) synthetic_projects: SharedMap<SyntheticProjectID, Project>,
    // openFiles is the set of open file paths associated with the snapshot that owns
    // this project collection.
    pub(crate) open_files: Arc<Set<Path>>,
    // inferredProject is a fallback project that is used when no configured
    // project can be found for an open file.
    pub(crate) inferred_project: Option<Shared<Project>>,
    // apiState tracks the projects and files that API clients have explicitly
    // opened so they are kept loaded across snapshots.
    pub(crate) api_state: APIState,

    // Go: openConfiguredProjectsOnce + openConfiguredProjects (not copied by clone()).
    open_configured_projects: Arc<OnceLock<Arc<Set<ConfiguredProjectID>>>>,
}

// APIState tracks the projects and files that API clients have explicitly opened.
// Opens and closes are ref-counted so multiple API clients don't clobber each
// other, and it is carried across snapshots so API-opened resources stay loaded.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct APIState {
    // openProjects is the ref-counted set of projects to keep open for API
    // clients, keyed by config file path.
    pub(crate) open_projects: FxHashMap<Path, usize>,
    // openFiles is the ref-counted set of files to keep open for API clients,
    // keyed by file path. Files with no configured project are loaded into the
    // inferred project.
    pub(crate) open_files: FxHashMap<Path, ApiOpenedFile>,
}

impl APIState {
    // projectcollection.go:55
    pub(crate) fn clone_state(&self) -> APIState {
        self.clone()
    }

    // projectcollection.go:62
    pub(crate) fn equals(&self, other: &APIState) -> bool {
        self == other
    }
}

// apiOpenedFile tracks a file kept open by API clients along with its ref count.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApiOpenedFile {
    pub(crate) file_name: String,
    pub(crate) ref_count: usize,
}

pub(crate) fn new_project_collection(to_path: ToPath, open_files: Set<Path>) -> ProjectCollection {
    ProjectCollection {
        to_path,
        config_file_registry: Arc::default(),
        file_default_projects: None,
        configured_projects: Arc::default(),
        synthetic_projects: Arc::default(),
        open_files: Arc::new(open_files),
        inferred_project: None,
        api_state: APIState::default(),
        open_configured_projects: Arc::new(OnceLock::new()),
    }
}

impl ProjectCollection {
    // projectcollection.go:72
    pub fn config_file_registry(&self) -> &Arc<ConfigFileRegistry> {
        &self.config_file_registry
    }

    // projectcollection.go:74
    pub fn configured_project(&self, path: &Path) -> Option<Shared<Project>> {
        self.configured_projects.get(&ConfiguredProjectID(path.clone())).cloned()
    }

    // projectcollection.go:78
    pub fn get_project(&self, id: &ID) -> Option<Shared<Project>> {
        if id.inferred().is_some() {
            return self.inferred_project.clone();
        }
        if let Some(synthetic_id) = id.synthetic() {
            return self.synthetic_projects.get(&synthetic_id).cloned();
        }
        if let Some(configured_id) = id.configured() {
            return self.configured_projects.get(&configured_id).cloned();
        }
        None
    }

    // projectcollection.go:92
    // ConfiguredProjects returns all configured projects in a stable order.
    fn configured_projects(&self) -> Vec<Shared<Project>> {
        let mut projects = Vec::with_capacity(self.configured_projects.len());
        self.fill_configured_projects(&mut projects);
        projects
    }

    // projectcollection.go:98
    fn fill_configured_projects(&self, projects: &mut Vec<Shared<Project>>) {
        let start = projects.len();
        // collected, then sorted by unique project id as in Go
        projects.extend(self.configured_projects.values().map(std::clone::Clone::clone));
        projects[start..].sort_by_key(|a| a.id());
    }

    // projectcollection.go:108
    // SyntheticProjects returns all synthetic projects in a stable order.
    pub fn synthetic_projects(&self) -> Vec<Shared<Project>> {
        let mut projects: Vec<Shared<Project>> =
            self.synthetic_projects.values().cloned().collect();
        projects.sort_by_key(|a| a.id());
        projects
    }

    // projectcollection.go:120
    // ProjectsByID returns all projects keyed by project ID in stable order.
    pub fn projects_by_id(&self) -> OrderedMap<ID, Shared<Project>> {
        let mut projects: OrderedMap<ID, Shared<Project>> = OrderedMap::default();
        projects
            .extend(self.configured_projects().into_iter().map(|project| (project.id(), project)));
        projects
            .extend(self.synthetic_projects().into_iter().map(|project| (project.id(), project)));
        if let Some(inferred_project) = &self.inferred_project {
            projects.insert(inferred_project.id(), inferred_project.clone());
        }
        projects
    }

    // projectcollection.go:137
    // Projects returns all configured, synthetic, and inferred projects in a stable order.
    pub fn projects(&self) -> Vec<Shared<Project>> {
        let mut projects =
            Vec::with_capacity(self.configured_projects.len() + self.synthetic_projects.len() + 1);
        self.fill_configured_projects(&mut projects);
        projects.extend(self.synthetic_projects());
        if let Some(inferred_project) = &self.inferred_project {
            projects.push(inferred_project.clone());
        }
        projects
    }

    // projectcollection.go:150
    // LanguageServiceProjects returns configured and inferred projects in stable order.
    // Synthetic projects are accessed explicitly through the API and do not participate
    // in cross-project language service operations.
    pub fn language_service_projects(&self) -> Vec<Shared<Project>> {
        let mut projects = Vec::with_capacity(self.configured_projects.len() + 1);
        self.fill_configured_projects(&mut projects);
        if let Some(inferred_project) = &self.inferred_project {
            projects.push(inferred_project.clone());
        }
        projects
    }

    // projectcollection.go:159
    pub fn inferred_project(&self) -> Option<Shared<Project>> {
        self.inferred_project.clone()
    }

    // projectcollection.go:165
    // GetLanguageServiceProjectsContainingFile does not consider synthetic projects
    // (ones created by API via createProgram)
    pub(crate) fn get_language_service_projects_containing_file(
        &self,
        path: &Path,
    ) -> Vec<Arc<dyn ls::Project>> {
        let mut projects: Vec<Arc<dyn ls::Project>> = Vec::new();
        projects.extend(self.configured_projects().into_iter().filter_map(
            |project| -> Option<Arc<dyn ls::Project>> {
                if project.contains_file(path) {
                    Some(Arc::<Project>::clone(project.arc()))
                } else {
                    None
                }
            },
        ));
        if let Some(inferred_project) = &self.inferred_project
            && inferred_project.contains_file(path)
        {
            projects.push(Arc::<Project>::clone(inferred_project.arc()));
        }
        projects
    }

    // projectcollection.go:179
    // GetOpenConfiguredProjects returns configured projects containing at least one open file.
    pub(crate) fn get_open_configured_projects(&self) -> Arc<Set<ConfiguredProjectID>> {
        Arc::clone(self.open_configured_projects.get_or_init(|| {
            let mut open_projects = new_set_with_size_hint(self.configured_projects.len());
            #[expect(
                clippy::iter_over_hash_type,
                reason = "only inserts into a set; Go ranges the set too"
            )]
            for path in self.open_files.keys() {
                if let Some(project_id) =
                    self.file_default_projects.as_ref().and_then(|m| m.get(path))
                    && let Some(configured_id) = project_id.configured()
                    && self.configured_projects.contains_key(&configured_id)
                {
                    open_projects.add(configured_id);
                    continue;
                }

                // only inserts into a set; Go ranges the map too
                open_projects.m.extend(self.configured_projects.values().filter_map(|project| {
                    if project.contains_file(path) {
                        let configured_id = project.id().configured().unwrap();
                        Some(configured_id)
                    } else {
                        None
                    }
                }));
            }
            Arc::new(open_projects)
        }))
    }

    // projectcollection.go:211
    // !!! result could be cached
    pub fn get_default_project(&self, path: &Path) -> Option<Shared<Project>> {
        if let Some(result) = self.file_default_projects.as_ref().and_then(|m| m.get(path)) {
            if result.inferred().is_some() {
                return self.inferred_project.clone();
            }
            let configured_id = result.configured().unwrap_or_default();
            return self.configured_projects.get(&configured_id).cloned();
        }

        let mut containing_projects: Vec<Shared<Project>> = Vec::new();
        let mut first_configured_project: Option<Shared<Project>> = None;
        let mut first_non_source_of_project_reference_redirect: Option<Shared<Project>> = None;
        let mut multiple_direct_inclusions = false;
        for p in self.configured_projects() {
            if p.contains_file(path) {
                containing_projects.push(p.clone());
                if !multiple_direct_inclusions && !p.is_source_from_project_reference(path) {
                    if first_non_source_of_project_reference_redirect.is_none() {
                        first_non_source_of_project_reference_redirect = Some(p.clone());
                    } else {
                        multiple_direct_inclusions = true;
                    }
                }
                if first_configured_project.is_none() {
                    first_configured_project = Some(p.clone());
                }
            }
        }
        if containing_projects.len() == 1 {
            return Some(containing_projects[0].clone());
        }
        if containing_projects.is_empty() {
            if let Some(inferred_project) = &self.inferred_project
                && inferred_project.contains_file(path)
            {
                return Some(inferred_project.clone());
            }
            return None;
        }
        if !multiple_direct_inclusions {
            if first_non_source_of_project_reference_redirect.is_some() {
                // Multiple projects include the file, but only one is a direct inclusion.
                return first_non_source_of_project_reference_redirect;
            }
            // Multiple projects include the file, and none are direct inclusions.
            return first_configured_project;
        }
        // Multiple projects include the file directly.
        if let Some(default_project) = self.find_default_configured_project(path) {
            return Some(default_project);
        }
        first_configured_project
    }

    // projectcollection.go:265
    fn find_default_configured_project(&self, path: &Path) -> Option<Shared<Project>> {
        let config_file_name = self.config_file_registry.get_config_file_name(path);
        if !config_file_name.is_empty() {
            return self.find_default_configured_project_worker(
                path,
                &config_file_name,
                None,
                None,
            );
        }
        None
    }

    // projectcollection.go:272
    fn find_default_configured_project_worker(
        &self,
        path: &Path,
        config_file_name: &str,
        visited: Option<Arc<SyncSet<Shared<Project>>>>,
        mut fallback: Option<Shared<Project>>,
    ) -> Option<Shared<Project>> {
        let config_file_path = (self.to_path)(config_file_name);
        let project = self.configured_projects.get(&ConfiguredProjectID(config_file_path))?.clone();
        let visited = visited.unwrap_or_default();

        // Look in the config's project and its references recursively.
        let search = breadth_first_search_parallel_ex(
            project,
            |project: &Shared<Project>| {
                let Some(command_line) = project.command_line else {
                    return Vec::new();
                };
                // A referenced project may not be loaded if `disableReferencedProjectLoad` is true.
                command_line
                    .resolved_project_reference_paths()
                    .iter()
                    .filter_map(|config_file_name| {
                        self.configured_projects
                            .get(&ConfiguredProjectID((self.to_path)(config_file_name)))
                            .cloned()
                    })
                    .collect()
            },
            |project: &Shared<Project>| {
                if project.contains_file(path) {
                    return (true, !project.is_source_from_project_reference(path));
                }
                (false, false)
            },
            BreadthFirstSearchOptions { visited: Some(&visited), preprocess_level: None },
            |p: &Shared<Project>| p.clone(),
        );

        if search.stopped {
            // If we found a project that directly contains the file, return it.
            return Some(search.path[0].clone());
        }
        if !search.path.is_empty() && fallback.is_none() {
            // If we found a project that contains the file, but it is a source from
            // a project reference, record it as a fallback.
            fallback = Some(search.path[0].clone());
        }

        // Look for tsconfig.json files higher up the directory tree and do the same. This handles
        // the common case where a higher-level "solution" tsconfig.json contains all projects in a
        // workspace.
        if let Some(config) = self.config_file_registry.get_config(path)
            && config.compiler_options().unwrap().disable_solution_searching.is_true()
        {
            return fallback;
        }
        let ancestor_config_name =
            self.config_file_registry.get_ancestor_config_file_name(path, config_file_name);
        if !ancestor_config_name.is_empty() {
            return self.find_default_configured_project_worker(
                path,
                &ancestor_config_name,
                Some(visited),
                fallback,
            );
        }
        fallback
    }

    // projectcollection.go:329
    // clone creates a shallow copy of the project collection.
    pub(crate) fn clone_collection(&self) -> ProjectCollection {
        ProjectCollection {
            to_path: Arc::clone(&self.to_path),
            config_file_registry: Arc::clone(&self.config_file_registry),
            configured_projects: Arc::clone(&self.configured_projects),
            synthetic_projects: Arc::clone(&self.synthetic_projects),
            open_files: Arc::clone(&self.open_files),
            inferred_project: self.inferred_project.clone(),
            file_default_projects: self.file_default_projects.clone(),
            api_state: self.api_state.clone(),
            open_configured_projects: Arc::new(OnceLock::new()),
        }
    }
}

// projectcollection.go:202
pub(crate) fn open_file_paths(overlays: &OverlayMap) -> Set<Path> {
    let mut open_files = new_set_with_size_hint(overlays.len());
    // pure set insert; Go ranges the map too
    open_files.m.extend(overlays.keys().map(std::clone::Clone::clone));
    open_files
}

// projectcollection.go:348
// findDefaultConfiguredProjectFromProgramInclusion finds the default configured project for a file
// based on the file's inclusion in existing projects. The projects should be sorted, as ties will
// be broken by slice order. `getProject` should return a project with an up-to-date program.
// Along with the resulting project path, a boolean is returned indicating whether there were multiple
// direct inclusions of the file in different projects, indicating that the caller may want to perform
// additional logic to determine the best project.
pub(crate) fn find_default_configured_project_from_program_inclusion(
    _file_name: &str,
    path: &Path,
    project_paths: &[Path],
    get_project: impl Fn(&Path) -> Shared<Project>,
) -> (Path, bool) {
    let mut containing_projects: Vec<Path> = Vec::new();
    let mut first_configured_project = Path::default();
    let mut first_non_source_of_project_reference_redirect = Path::default();
    let mut multiple_direct_inclusions = false;

    for project_path in project_paths {
        let p = get_project(project_path);
        if p.contains_file(path) {
            containing_projects.push(project_path.clone());
            if !multiple_direct_inclusions && !p.is_source_from_project_reference(path) {
                if first_non_source_of_project_reference_redirect.0.is_empty() {
                    first_non_source_of_project_reference_redirect = project_path.clone();
                } else {
                    multiple_direct_inclusions = true;
                }
            }
            if first_configured_project.0.is_empty() {
                first_configured_project = project_path.clone();
            }
        }
    }

    if containing_projects.len() == 1 {
        return (containing_projects[0].clone(), false);
    }
    if !multiple_direct_inclusions {
        if !first_non_source_of_project_reference_redirect.0.is_empty() {
            // Multiple projects include the file, but only one is a direct inclusion.
            return (first_non_source_of_project_reference_redirect, false);
        }
        // Multiple projects include the file, and none are direct inclusions.
        return (first_configured_project, false);
    }
    // Multiple projects include the file directly.
    (first_configured_project, true)
}
