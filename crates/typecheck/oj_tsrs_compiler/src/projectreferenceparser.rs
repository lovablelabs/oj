use std::collections::hash_map::Entry;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_core::P;
use tsrs_core::tspath::{self, Path};
use tsrs_tsoptions::ParsedCommandLine;

use crate::host::CompilerHost;
use crate::projectreferencefilemapper::ProjectReferenceFileMapper;

// Go's tasks are shared pointers (a deduplicated task appears in several subTasks lists); here they live in
// `projectReferenceParser.tasks` and are referenced by index.
type TaskId = usize;

struct ProjectReferenceParseTask {
    config_name: String,
    resolved: Option<P<ParsedCommandLine>>,
    sub_tasks: Vec<TaskId>,
}

// projectreferenceparser.go:13
pub struct ProjectReferenceParser<'a> {
    host: &'a dyn CompilerHost,
    tasks: Vec<ProjectReferenceParseTask>,
    tasks_by_file_name: FxHashMap<Path, TaskId>,
}

impl<'a> ProjectReferenceParser<'a> {
    pub(crate) fn new(host: &'a dyn CompilerHost) -> ProjectReferenceParser<'a> {
        ProjectReferenceParser { host, tasks: Vec::new(), tasks_by_file_name: FxHashMap::default() }
    }

    fn to_path(&self, file_name: &str) -> Path {
        tspath::to_path(
            file_name,
            self.host.get_current_directory(),
            self.host.fs().use_case_sensitive_file_names(),
        )
    }

    // projectreferenceparser.go:19
    fn parse_task(&mut self, task: TaskId) {
        let config_name = self.tasks[task].config_name.clone();
        let path = self.to_path(&config_name);
        let resolved = self.host.get_resolved_project_reference(&config_name, path);
        self.tasks[task].resolved = resolved;
        let Some(resolved) = resolved else {
            return;
        };
        ParsedCommandLine::parse_input_output_names(resolved);
        let sub_references = resolved.resolved_project_reference_paths();
        if !sub_references.is_empty() {
            self.tasks[task].sub_tasks = self.create_project_reference_parse_tasks(sub_references);
        }
    }

    // projectreferenceparser.go:34
    pub(crate) fn create_project_reference_parse_tasks(
        &mut self,
        project_references: &[String],
    ) -> Vec<TaskId> {
        project_references
            .iter()
            .map(|config_name| {
                self.tasks.push(ProjectReferenceParseTask {
                    config_name: config_name.clone(),
                    resolved: None,
                    sub_tasks: Vec::new(),
                });
                self.tasks.len() - 1
            })
            .collect()
    }

    // projectreferenceparser.go:48 (Go parses the tasks on a work group; here they are parsed in queue order on the
    // calling thread, which gives the same deduplication and mapper contents)
    pub(crate) fn parse(&mut self, tasks: &mut [TaskId], mapper: &mut ProjectReferenceFileMapper) {
        self.start(tasks);
        self.init_mapper(tasks, mapper);
    }

    // projectreferenceparser.go:54
    fn start(&mut self, tasks: &mut [TaskId]) {
        for task_slot in tasks.iter_mut() {
            let task = *task_slot;
            let path = self.to_path(&self.tasks[task].config_name);
            match self.tasks_by_file_name.entry(path) {
                Entry::Occupied(entry) => {
                    // dedup tasks to ensure correct file order, regardless of which task would be started first
                    *task_slot = *entry.get();
                    continue;
                }
                Entry::Vacant(entry) => {
                    entry.insert(task);
                }
            }
            self.parse_task(task);
            let mut sub_tasks = std::mem::take(&mut self.tasks[task].sub_tasks);
            self.start(&mut sub_tasks);
            self.tasks[task].sub_tasks = sub_tasks;
        }
    }

    // projectreferenceparser.go:68
    fn init_mapper(&self, tasks: &[TaskId], mapper: &mut ProjectReferenceFileMapper) {
        let total_references = self.tasks_by_file_name.len() + 1;
        mapper.config_to_project_reference =
            FxHashMap::with_capacity_and_hasher(total_references, rustc_hash::FxBuildHasher);
        mapper.references_in_config_file =
            FxHashMap::with_capacity_and_hasher(total_references, rustc_hash::FxBuildHasher);
        mapper.source_to_project_reference = FxHashMap::default();
        mapper.output_dts_to_project_reference = FxHashMap::default();
        let root = self.init_mapper_worker(tasks, &mut FxHashSet::default(), mapper);
        mapper.references_in_config_file.insert(mapper.root_config_path(), root);
    }

    // projectreferenceparser.go:78
    fn init_mapper_worker(
        &self,
        tasks: &[TaskId],
        seen: &mut FxHashSet<TaskId>,
        mapper: &mut ProjectReferenceFileMapper,
    ) -> Vec<Path> {
        if tasks.is_empty() {
            return Vec::new();
        }
        let mut results = Vec::with_capacity(tasks.len());
        for &task in tasks {
            let t = &self.tasks[task];
            let path = self.to_path(&t.config_name);
            results.push(path.clone());
            // ensure we only walk each task once
            if !seen.insert(task) {
                continue;
            }
            mapper.config_to_project_reference.insert(path.clone(), t.resolved);
            if let Some(resolved) = t.resolved
                && mapper.config.config_file != resolved.config_file
            {
                // Map current task's files first, before recursing into subtasks.
                // This matches TypeScript's behavior where child project references
                // overwrite parent entries when a file belongs to multiple projects.
                if let Some(m) = resolved.source_to_project_reference() {
                    mapper
                        .source_to_project_reference
                        .extend(m.iter().map(|(k, v)| (k.clone(), *v)));
                }
                if let Some(m) = resolved.output_dts_to_project_reference() {
                    mapper
                        .output_dts_to_project_reference
                        .extend(m.iter().map(|(k, v)| (k.clone(), *v)));
                }
                if mapper.use_source_of_project_reference {
                    let options = resolved.compiler_options().unwrap();
                    let mut decl_dir = options.declaration_dir.clone();
                    if decl_dir.is_empty() {
                        decl_dir.clone_from(&options.out_dir);
                    }
                    if !decl_dir.is_empty() {
                        mapper.dts_directories.insert(self.to_path(&decl_dir));
                    }
                }
            }
            let references_in_config = self.init_mapper_worker(&t.sub_tasks, seen, mapper);
            mapper.references_in_config_file.insert(path, references_in_config);
        }
        results
    }
}
