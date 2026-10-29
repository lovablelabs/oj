use std::sync::{Arc, Mutex};

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_core::collections::Set;
use tsrs_core::tspath::{self, ComparePathsOptions, Path};
use tsrs_core::{CompilerOptions, P, Tristate};
use tsrs_tsoptions::{
    self as tsoptions, ExtendedConfigCache as ExtendedConfigCacheTrait, ParseConfigHost,
    ParsedCommandLine,
};
use tsrs_vfs::FS;

use crate::configfileregistry::{
    ConfigFileEntry, ConfigFileNames, ConfigFileRegistry, ConfiguredContentMappers,
    collect_configured_content_mappers, new_config_file_entry, new_extended_config_file_entry,
};
use crate::dirty::{self, Shared, SyncMapEntry};
use crate::extendedconfigcache::{ExtendedConfigCache, ExtendedConfigParseArgs};
use crate::filechange::FileChangeSummary;
use crate::logging::LogTree;
use crate::project::{ID, PendingReload};
use crate::projectcollectionbuilder::ProjectLoadKind;
use crate::session::SessionOptions;
use crate::snapshotfs::{FileHandleSource, SnapshotFSBuilder, SourceFS, new_source_fs};
use crate::watch::{
    MIN_WATCH_LOCATION_DEPTH, PatternsAndIgnored, get_path_components_for_watching,
    get_recursive_glob_pattern,
};

// configFileRegistryBuilder tracks changes made on top of a previous
// configFileRegistry, producing a new clone with `finalize()` after
// all changes have been made.
pub struct ConfigFileRegistryBuilder {
    has_relative_pattern_capability: bool,
    fs: Arc<SourceFS>,
    is_open_file: Box<dyn Fn(&Path) -> bool + Send + Sync>,
    extended_config_cache: Arc<ExtendedConfigCache>,
    snapshot_id: u64,
    session_options: Arc<SessionOptions>,
    custom_config_file_name: String,

    base: Arc<ConfigFileRegistry>,
    configs: dirty::SyncMap<Path, ConfigFileEntry>,
    config_file_names: dirty::Map<Path, ConfigFileNames>,
    custom_config_file_name_changed: bool,
    // Go: contentMappersMu + allConfiguredContentMappers.
    all_configured_content_mappers: Mutex<Option<Arc<ConfiguredContentMappers>>>,
}

// configfileregistrybuilder.go:44
pub fn new_config_file_registry_builder(
    has_relative_pattern_capability: bool,
    fs: Arc<SnapshotFSBuilder>,
    is_open_file: Box<dyn Fn(&Path) -> bool + Send + Sync>,
    old_config_file_registry: Arc<ConfigFileRegistry>,
    extended_config_cache: Arc<ExtendedConfigCache>,
    snapshot_id: u64,
    session_options: Arc<SessionOptions>,
    custom_config_file_name: &str,
    _logger: &LogTree,
) -> ConfigFileRegistryBuilder {
    let to_path = Arc::clone(&fs.to_path);
    ConfigFileRegistryBuilder {
        has_relative_pattern_capability,
        fs: Arc::new(new_source_fs(false, fs, to_path)),
        is_open_file,
        session_options,
        extended_config_cache,
        snapshot_id,
        custom_config_file_name: custom_config_file_name.to_string(),
        custom_config_file_name_changed: custom_config_file_name
            != old_config_file_registry.custom_config_file_name,
        all_configured_content_mappers: Mutex::new(Some(
            old_config_file_registry.content_mappers(),
        )),

        configs: dirty::new_sync_map(Arc::clone(&old_config_file_registry.configs)),
        config_file_names: dirty::new_map(Arc::clone(&old_config_file_registry.config_file_names)),
        base: old_config_file_registry,
    }
}

// Go `changeFileResult`.
#[derive(Clone, Debug, Default)]
pub struct ChangeFileResult {
    pub(crate) affected_projects: Option<FxHashSet<ID>>,
    pub(crate) affected_files: Option<FxHashSet<Path>>,
}

// Go `core.CopyMapInto` over set-like maps.
fn copy_set_into<T: std::hash::Hash + Eq + Clone>(
    dst: Option<FxHashSet<T>>,
    src: Option<&FxHashSet<T>>,
) -> FxHashSet<T> {
    let mut dst = dst.unwrap_or_default();
    if let Some(src) = src {
        dst.extend(src.iter().cloned());
    }
    dst
}

impl ConfigFileRegistryBuilder {
    fn to_path(&self, file_name: &str) -> Path {
        self.fs.to_path(file_name)
    }

    // configfileregistrybuilder.go:74
    // Finalize creates a new configFileRegistry based on the changes made in the builder.
    // If no changes were made, it returns the original base registry.
    pub(crate) fn finalize(&self) -> Arc<ConfigFileRegistry> {
        let mut new_registry: Option<ConfigFileRegistry> = None;

        let (configs, changed_configs) = self.configs.finalize();
        if changed_configs {
            let registry = new_registry.get_or_insert_with(|| self.base.clone_registry());
            registry.configs = configs;
            registry.all_configured_content_mappers = Some(self.content_mappers());
        }

        let (config_file_names, changed_names) = self.config_file_names.finalize();
        if changed_names {
            let registry = new_registry.get_or_insert_with(|| self.base.clone_registry());
            registry.config_file_names = config_file_names;
        }

        if self.custom_config_file_name_changed {
            let registry = new_registry.get_or_insert_with(|| self.base.clone_registry());
            registry.custom_config_file_name.clone_from(&self.custom_config_file_name);
        }

        match new_registry {
            Some(registry) => Arc::new(registry),
            None => Arc::clone(&self.base),
        }
    }

    // configfileregistrybuilder.go:103
    fn content_mappers(&self) -> Arc<ConfiguredContentMappers> {
        let mut mappers = self.all_configured_content_mappers.lock().unwrap();
        if mappers.is_none() {
            let mut command_lines: Vec<P<ParsedCommandLine>> = Vec::new();
            self.configs.range(|entry| {
                if let Some(command_line) = entry.value().and_then(|v| v.command_line) {
                    command_lines.push(command_line);
                }
                true
            });
            *mappers = Some(collect_configured_content_mappers(&command_lines));
        }
        mappers.clone().unwrap()
    }

    // configfileregistrybuilder.go:119
    fn invalidate_content_mappers(&self) {
        *self.all_configured_content_mappers.lock().unwrap() = None;
    }

    // configfileregistrybuilder.go:125
    pub(crate) fn find_or_acquire_config_for_file(
        &self,
        config_file_name: &str,
        config_file_path: &Path,
        file_path: &Path,
        load_kind: ProjectLoadKind,
        logger: &LogTree,
    ) -> Option<P<ParsedCommandLine>> {
        match load_kind {
            ProjectLoadKind::Find => self.configs.load(config_file_path)?.value()?.command_line,
            ProjectLoadKind::Create => {
                self.acquire_config_for_file(config_file_name, config_file_path, file_path, logger)
            }
        }
    }

    // configfileregistrybuilder.go:148
    // reloadIfNeeded updates the command line of the config file entry based on its
    // pending reload state. This function should only be called from within the
    // Change() method of a dirty map entry.
    fn reload_if_needed(
        &self,
        entry: &mut ConfigFileEntry,
        file_name: &str,
        path: &Path,
        logger: &LogTree,
    ) -> bool {
        let old_command_line = entry.command_line;
        match entry.pending_reload {
            PendingReload::FileNames => {
                logger.log(&format!("Reloading file names for config: {file_name}"));
                entry.command_line = Some(P::new(
                    entry.command_line.unwrap().reload_file_names_of_parsed_command_line(&*self.fs),
                ));
            }
            PendingReload::Full => {
                logger.log(&format!("Loading config file: {file_name}"));
                // When the workspace is trusted, enable external content mappers so a config's contentMappers pass
                // the runExternalCode gate and register, as they would with the CLI flag.
                let existing_options = if self.session_options.run_external_code {
                    Some(CompilerOptions {
                        run_external_code: Tristate::True,
                        ..Default::default()
                    })
                } else {
                    None
                };
                let (command_line, _) = tsoptions::get_parsed_command_line_of_config_file_path(
                    file_name,
                    path.clone(),
                    existing_options.as_ref(),
                    None, /*optionsRaw*/
                    self,
                    Some(self as &dyn ExtendedConfigCacheTrait),
                );
                entry.command_line = command_line.map(P::new);
                self.update_extending_configs(path, entry.command_line, old_command_line);
                self.update_root_files_watch(file_name, entry);
                logger.log("Finished loading config file");
            }
            PendingReload::None => return false,
        }
        entry.pending_reload = PendingReload::None;
        old_command_line != entry.command_line
    }

    // configfileregistrybuilder.go:173
    fn update_extending_configs(
        &self,
        extending_config_path: &Path,
        new_command_line: Option<P<ParsedCommandLine>>,
        old_command_line: Option<P<ParsedCommandLine>>,
    ) {
        let mut new_extended_config_paths: Set<Path> = Set::default();
        if let Some(new_command_line) = new_command_line {
            for extended_config in new_command_line.extended_source_files() {
                let extended_config_path = self.to_path(&extended_config);
                new_extended_config_paths.add(extended_config_path.clone());
                let (entry, loaded) = self.configs.load_or_store(
                    extended_config_path,
                    Shared::new(new_extended_config_file_entry(
                        &extended_config,
                        extending_config_path.clone(),
                    )),
                );
                if loaded && let Some(entry) = entry {
                    entry.change_if(
                        |config| {
                            !config
                                .retaining_configs
                                .as_ref()
                                .is_some_and(|r| r.contains(extending_config_path))
                        },
                        |config| {
                            config
                                .retaining_configs
                                .get_or_insert_with(FxHashSet::default)
                                .insert(extending_config_path.clone());
                        },
                    );
                }
            }
        }
        if let Some(old_command_line) = old_command_line {
            for extended_config in old_command_line.extended_source_files() {
                let extended_config_path = self.to_path(&extended_config);
                if new_extended_config_paths.has(&extended_config_path) {
                    continue;
                }
                if let Some(entry) = self.configs.load(&extended_config_path) {
                    entry.change_if(
                        |config| {
                            config
                                .retaining_configs
                                .as_ref()
                                .is_some_and(|r| r.contains(extending_config_path))
                        },
                        |config| {
                            if let Some(r) = &mut config.retaining_configs {
                                r.remove(extending_config_path);
                            }
                        },
                    );
                }
            }
        }
    }

    // configfileregistrybuilder.go:217
    fn update_root_files_watch(&self, file_name: &str, entry: &mut ConfigFileEntry) {
        let Some(root_files_watch) = &entry.root_files_watch else {
            return;
        };

        let mut ignored = FxHashSet::default();
        let mut globs: Vec<String> = Vec::new();
        let mut external_directories: Vec<String> = Vec::new();
        let mut include_workspace = false;
        let mut include_tsconfig_dir = false;
        let tsconfig_dir = tspath::get_directory_path(file_name);
        // (Go calls these methods on a possibly nil *ParsedCommandLine; they return nil for it.)
        let command_line = entry.command_line;
        let compare_paths_options = ComparePathsOptions {
            current_directory: self.session_options.current_directory.clone(),
            use_case_sensitive_file_names: self.fs.use_case_sensitive_file_names(),
        };
        if let Some(wildcard_directories) =
            command_line.as_ref().and_then(|c| c.wildcard_directories())
        {
            for dir in wildcard_directories.keys() {
                if tspath::contains_path(
                    &self.session_options.current_directory,
                    dir,
                    &compare_paths_options,
                ) {
                    include_workspace = true;
                } else if tspath::contains_path(&tsconfig_dir, dir, &compare_paths_options) {
                    include_tsconfig_dir = true;
                } else {
                    external_directories.push(dir.clone());
                }
            }
        }
        for file_name in command_line.as_ref().map(|c| c.literal_file_names()).unwrap_or_default() {
            if tspath::contains_path(
                &self.session_options.current_directory,
                file_name,
                &compare_paths_options,
            ) {
                include_workspace = true;
            } else if tspath::contains_path(&tsconfig_dir, file_name, &compare_paths_options) {
                include_tsconfig_dir = true;
            } else {
                external_directories.push(tspath::get_directory_path(file_name));
            }
        }

        if include_workspace {
            globs.push(get_recursive_glob_pattern(&self.session_options.current_directory));
        }
        if include_tsconfig_dir {
            globs.push(get_recursive_glob_pattern(&tsconfig_dir));
        }
        globs.extend(
            command_line
                .as_ref()
                .map(|c| c.extended_source_files())
                .unwrap_or_default()
                .into_iter()
                .filter_map(|file_name| {
                    if include_workspace
                        && tspath::contains_path(
                            &self.session_options.current_directory,
                            &file_name,
                            &compare_paths_options,
                        )
                    {
                        return None;
                    }
                    Some(file_name)
                }),
        );
        if !external_directories.is_empty() {
            let (common_parents, ignored_external_dirs) = tspath::get_common_parents(
                &external_directories,
                MIN_WATCH_LOCATION_DEPTH,
                get_path_components_for_watching,
                &compare_paths_options,
            );
            globs.extend(common_parents.iter().map(|parent| get_recursive_glob_pattern(parent)));
            ignored = ignored_external_dirs;
        }

        globs.sort();
        entry.root_files_watch = Some(root_files_watch.clone_with(PatternsAndIgnored {
            patterns_inside_workspace: globs,
            ignored,
            ..Default::default()
        }));
    }

    // configfileregistrybuilder.go:283
    // acquireConfigForProject loads a config file entry from the cache, or parses it if not already
    // cached, then adds the project (if provided) to `retainingProjects` to keep it alive
    // in the cache. Each `acquireConfigForProject` call that passes a `project` should be accompanied
    // by an eventual `releaseConfigForProject` call with the same project.
    pub(crate) fn acquire_config_for_project(
        &self,
        file_name: &str,
        path: &Path,
        project: &ID,
        logger: &LogTree,
    ) -> Option<P<ParsedCommandLine>> {
        let (entry, _) = self.configs.load_or_store(
            path.clone(),
            Shared::new(new_config_file_entry(self.has_relative_pattern_capability, file_name)),
        );
        let entry = entry?;
        let needs_retain_project = std::cell::Cell::new(false);
        let mut content_mappers_changed = false;
        entry.change_if(
            |config| {
                let already_retaining =
                    config.retaining_projects.as_ref().is_some_and(|r| r.contains(project));
                needs_retain_project.set(!already_retaining);
                needs_retain_project.get() || config.pending_reload != PendingReload::None
            },
            |config| {
                if needs_retain_project.get() {
                    config
                        .retaining_projects
                        .get_or_insert_with(FxHashSet::default)
                        .insert(project.clone());
                }
                content_mappers_changed = self.reload_if_needed(config, file_name, path, logger);
            },
        );
        if content_mappers_changed {
            self.invalidate_content_mappers();
        }
        entry.value()?.command_line
    }

    // configfileregistrybuilder.go:313
    // acquireConfigForFile loads a config file entry from the cache, or parses it if not already
    // cached, then adds the open file to `retainingOpenFiles` to keep it alive in the cache.
    // Each `acquireConfigForFile` call that passes an `openFilePath`
    // should be accompanied by an eventual `releaseConfigForOpenFile` call with the same open file.
    fn acquire_config_for_file(
        &self,
        config_file_name: &str,
        config_file_path: &Path,
        file_path: &Path,
        logger: &LogTree,
    ) -> Option<P<ParsedCommandLine>> {
        let (entry, _) = self.configs.load_or_store(
            config_file_path.clone(),
            Shared::new(new_config_file_entry(
                self.has_relative_pattern_capability,
                config_file_name,
            )),
        );
        let entry = entry?;
        let needs_retain_open_file = std::cell::Cell::new(false);
        let mut content_mappers_changed = false;
        entry.change_if(
            |config| {
                if (self.is_open_file)(file_path) {
                    let already_retaining =
                        config.retaining_open_files.as_ref().is_some_and(|r| r.contains(file_path));
                    needs_retain_open_file.set(!already_retaining);
                }
                needs_retain_open_file.get() || config.pending_reload != PendingReload::None
            },
            |config| {
                if needs_retain_open_file.get() {
                    config
                        .retaining_open_files
                        .get_or_insert_with(FxHashSet::default)
                        .insert(file_path.clone());
                }
                content_mappers_changed =
                    self.reload_if_needed(config, config_file_name, config_file_path, logger);
            },
        );
        if content_mappers_changed {
            self.invalidate_content_mappers();
        }
        entry.value()?.command_line
    }

    // configfileregistrybuilder.go:343
    // releaseConfigForProject removes the project from the config entry. Once no projects
    // or files are associated with the config entry, it will be removed on the next call to `cleanup`.
    pub(crate) fn release_config_for_project(&self, config_file_path: &Path, project_id: &ID) {
        if let Some(entry) = self.configs.load(config_file_path) {
            entry.change_if(
                |config| config.retaining_projects.as_ref().is_some_and(|r| r.contains(project_id)),
                |config| {
                    if let Some(r) = &mut config.retaining_projects {
                        r.remove(project_id);
                    }
                },
            );
        }
    }

    // configfileregistrybuilder.go:376
    // didCloseFile removes the open file from the config entry. Once no projects
    // or files are associated with the config entry, it will be removed on the next call to `cleanup`.
    fn did_close_file(&self, path: &Path) {
        if tspath::is_dynamic_file_name(path) {
            return;
        }
        self.config_file_names.delete(path);
        self.configs.range(|entry| {
            entry.change_if(
                |config| config.retaining_open_files.as_ref().is_some_and(|r| r.contains(path)),
                |config| {
                    if let Some(r) = &mut config.retaining_open_files {
                        r.remove(path);
                    }
                },
            );
            true
        });
    }

    // configfileregistrybuilder.go:404
    pub(crate) fn did_change_custom_config_file_name(&self, _logger: &LogTree) -> bool {
        if !self.custom_config_file_name_changed {
            return false;
        }

        self.config_file_names.clear();
        true
    }

    // configfileregistrybuilder.go:413
    fn invalidate_cache(&self, logger: &LogTree) -> ChangeFileResult {
        let mut affected_projects: Option<FxHashSet<ID>> = None;
        let mut affected_files: Option<FxHashSet<Path>> = None;

        logger.log("Too many files changed; marking all configs for reload");
        self.config_file_names.range(|entry| {
            affected_files.get_or_insert_with(FxHashSet::default).insert(entry.key());
            true
        });
        self.config_file_names.clear();

        self.configs.range(|entry| {
            entry.change(|entry| {
                affected_projects = Some(copy_set_into(
                    affected_projects.take(),
                    entry.retaining_projects.as_ref(),
                ));
                if entry.pending_reload != PendingReload::Full {
                    let text = self.fs.read_file(&entry.file_name);
                    match (&text, entry.command_line) {
                        (Some(text), Some(command_line))
                            if *text
                                == command_line.config_file.unwrap().source_file.get().text() =>
                        {
                            entry.pending_reload = PendingReload::FileNames;
                        }
                        _ => entry.pending_reload = PendingReload::Full,
                    }
                }
            });
            true
        });

        ChangeFileResult { affected_projects, affected_files }
    }

    // configfileregistrybuilder.go:448
    fn is_config_base_name(&self, base_name: &str) -> bool {
        base_name == "tsconfig.json"
            || base_name == "jsconfig.json"
            || (!self.custom_config_file_name.is_empty()
                && base_name == self.custom_config_file_name)
    }

    // configfileregistrybuilder.go:453
    pub(crate) fn did_change_files(
        &self,
        summary: &FileChangeSummary,
        logger: &LogTree,
    ) -> ChangeFileResult {
        if summary.invalidate_all {
            return self.invalidate_cache(logger);
        }
        let mut affected_projects: Option<FxHashSet<ID>> = None;
        let mut affected_files: Option<FxHashSet<Path>> = None;
        let mut should_invalidate_cache = false;

        logger.log("Summarizing file changes");
        let has_excessive_changes = summary.has_excessive_watch_events()
            && summary.includes_watch_change_outside_node_modules;
        let mut created_files: FxHashMap<Path, String> = FxHashMap::default();
        let mut deleted_files: FxHashMap<Path, String> = FxHashMap::default();
        let mut created_or_deleted_config_files: FxHashSet<Path> = FxHashSet::default();
        let mut created_or_changed_or_deleted_files: Vec<Path> = Vec::new();
        let mut seen_changed: FxHashSet<Path> = FxHashSet::default();
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: fills path sets and a list (a map in Go) whose later uses commute apart from log lines; Go ranges the set too"
        )]
        for uri in summary.changed.keys() {
            if tspath::contains_ignored_path(&uri.0) {
                continue;
            }
            let file_name = uri.file_name();
            let path = self.to_path(&file_name);
            let base_name = tspath::get_base_file_name(&path);
            if self.is_config_base_name(&base_name) {
                created_or_deleted_config_files.insert(path.clone());
            }
            if seen_changed.insert(path.clone()) {
                created_or_changed_or_deleted_files.push(path);
            }
        }
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: fills path sets and maps and a list (a map in Go) whose later uses commute apart from log lines; Go ranges the set too"
        )]
        for uri in summary.deleted.keys() {
            if tspath::contains_ignored_path(&uri.0) {
                continue;
            }
            let file_name = uri.file_name();
            let path = self.to_path(&file_name);
            deleted_files.insert(path.clone(), file_name);
            let base_name = tspath::get_base_file_name(&path);
            if self.is_config_base_name(&base_name) {
                created_or_deleted_config_files.insert(path.clone());
            }
            if seen_changed.insert(path.clone()) {
                created_or_changed_or_deleted_files.push(path);
            }
        }
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: fills path sets and maps and a list (a map in Go) whose later uses commute apart from log lines; Go ranges the set too"
        )]
        for uri in summary.created.keys() {
            if tspath::contains_ignored_path(&uri.0) {
                continue;
            }
            let file_name = uri.file_name();
            let path = self.to_path(&file_name);
            created_files.insert(path.clone(), file_name);
            let base_name = tspath::get_base_file_name(&path);
            if self.is_config_base_name(&base_name) {
                created_or_deleted_config_files.insert(path.clone());
            }
            if seen_changed.insert(path.clone()) {
                created_or_changed_or_deleted_files.push(path);
            }
        }

        // Handle closed files - this ranges over config entries and could be combined
        // with the file change handling, but a separate loop is simpler and a snapshot
        // change with both closing and watch changes seems rare.
        #[expect(
            clippy::iter_over_hash_type,
            reason = "each path is only deleted from config_file_names and retaining sets; removals commute; Go ranges the set too"
        )]
        for uri in summary.closed.keys() {
            let file_name = uri.file_name();
            let path = self.to_path(&file_name);
            self.did_close_file(&path);
        }

        // Handle changes to stored config files and their content mapper package manifests.
        logger.log("Checking if any changed files are configuration files");
        for path in &created_or_changed_or_deleted_files {
            match self.configs.load(path) {
                Some(entry) => {
                    if has_excessive_changes {
                        return self.invalidate_cache(logger);
                    }

                    affected_projects = Some(copy_set_into(
                        affected_projects,
                        Self::handle_config_change(&entry, logger).as_ref(),
                    ));
                    let retaining_configs: Vec<Path> = entry
                        .value()
                        .and_then(|v| v.retaining_configs.clone())
                        .map(|r| r.into_iter().collect())
                        .unwrap_or_default();
                    affected_projects = retaining_configs.into_iter().fold(
                        affected_projects,
                        |mut affected_projects, extending_config_path| {
                            if let Some(extending_config_entry) =
                                self.configs.load(&extending_config_path)
                            {
                                affected_projects = Some(copy_set_into(
                                    affected_projects,
                                    Self::handle_config_change(&extending_config_entry, logger)
                                        .as_ref(),
                                ));
                            }

                            affected_projects
                        },
                    );
                    // This was a config file, so assume it's not also a root file
                    created_files.remove(path);
                }
                _ => {
                    if tspath::get_base_file_name(path) == "package.json" {
                        let mut manifest_changed = false;
                        self.configs.range(|entry| {
                            if content_mapper_manifest_path(
                                entry.value().and_then(|v| v.command_line),
                                |f| self.to_path(f),
                                path,
                            ) {
                                affected_projects = Some(copy_set_into(
                                    affected_projects.take(),
                                    Self::handle_config_change(entry, logger).as_ref(),
                                ));
                                manifest_changed = true;
                            }
                            true
                        });
                        if manifest_changed {
                            self.invalidate_content_mappers();
                        }
                    }
                }
            }
        }

        // Handle created/deleted files named "tsconfig.json" or "jsconfig.json"
        #[expect(
            clippy::iter_over_hash_type,
            reason = "the early return depends only on has_excessive_changes; otherwise deletes and set inserts; Go ranges the map too"
        )]
        for path in &created_or_deleted_config_files {
            if has_excessive_changes {
                return self.invalidate_cache(logger);
            }
            let directory_path = path.get_directory_path();
            self.config_file_names.range(|entry| {
                if directory_path.contains_path(&entry.key()) {
                    affected_files.get_or_insert_with(FxHashSet::default).insert(entry.key());
                    entry.delete();
                }
                true
            });
        }

        // Handle deletions of wildcard-included root files
        #[expect(
            clippy::iter_over_hash_type,
            reason = "order-independent: marks configs for a file-name reload and unions affected projects; Go ranges the map too"
        )]
        for (path, file_name) in &deleted_files {
            self.configs.range(|entry| {
                let key = entry.key();
                entry.change_if(
                    |config| {
                        if config.pending_reload != PendingReload::None {
                            return false;
                        }
                        let Some(command_line) = config.command_line else {
                            return false;
                        };
                        if command_line.file_names_by_path().contains_key(path) {
                            // If the file is included in FileNames() but not matched by literal "files", it must be
                            // included via wildcard, which means a reload of filenames will remove it from the list.
                            // (Files explicitly specified in "files" are always included in the ParsedCommandLine,
                            // triggering a missing root file error during program construction.)
                            return command_line.get_matched_file_spec(file_name).is_empty();
                        }
                        false
                    },
                    |config| {
                        config.pending_reload = PendingReload::FileNames;
                        affected_projects = Some(copy_set_into(
                            affected_projects.take(),
                            config.retaining_projects.as_ref(),
                        ));
                        logger.logf(format_args!("Root files for config {} changed", key.0));
                        should_invalidate_cache = has_excessive_changes;
                    },
                );
                !should_invalidate_cache
            });
            if should_invalidate_cache {
                return self.invalidate_cache(logger);
            }
        }

        // Handle possible root file creation
        if !created_files.is_empty() {
            self.configs.range(|entry| {
                let key = entry.key();
                entry.change_if(
                    |config| {
                        let Some(command_line) = config.command_line else {
                            return false;
                        };
                        if config.root_files_watch.is_none()
                            || config.pending_reload != PendingReload::None
                        {
                            return false;
                        }
                        logger.logf(format_args!(
                            "Checking if any of {} created files match root files for config {}",
                            created_files.len(),
                            key.0
                        ));
                        // any-match predicate; the result does not depend on order; Go ranges the map too
                        if created_files.iter().any(|(path, file_name)| {
                            if command_line.possibly_matches_file_name(file_name) {
                                return true;
                            }
                            if command_line.possibly_matches_directory_name(path)
                                && self.fs.directory_exists(file_name)
                            {
                                // If we got a creation event for a directory, it's probably a symlink. We don't need to
                                // test realpath here; this is enough confidence to trigger a filename reload.
                                return true;
                            }

                            false
                        }) {
                            return true;
                        }
                        false
                    },
                    |config| {
                        config.pending_reload = PendingReload::FileNames;
                        affected_projects = Some(copy_set_into(
                            affected_projects.take(),
                            config.retaining_projects.as_ref(),
                        ));
                        logger.logf(format_args!("Root files for config {} changed", key.0));
                        should_invalidate_cache = has_excessive_changes;
                    },
                );
                !should_invalidate_cache
            });
            if should_invalidate_cache {
                return self.invalidate_cache(logger);
            }
        }

        ChangeFileResult { affected_projects, affected_files }
    }

    // configfileregistrybuilder.go:642
    fn handle_config_change(
        entry: &Arc<SyncMapEntry<Path, ConfigFileEntry>>,
        logger: &LogTree,
    ) -> Option<FxHashSet<ID>> {
        let mut affected_projects = None;
        let changed = entry.change_if(
            |config| config.pending_reload != PendingReload::Full,
            |config| config.pending_reload = PendingReload::Full,
        );
        if changed {
            logger.logf(format_args!("Config file {} changed", entry.key().0));
            affected_projects = entry.value().and_then(|v| v.retaining_projects.clone());
        }

        affected_projects
    }

    // configfileregistrybuilder.go:669
    fn compute_config_file_name(
        &self,
        file_name: &str,
        skip_search_in_directory_of_file: bool,
        logger: &LogTree,
    ) -> String {
        let search_path = tspath::get_directory_path(file_name);
        // Prefer custom config file if provided; search ancestors with correct skip behavior.
        if !self.custom_config_file_name.is_empty() {
            let mut skip = skip_search_in_directory_of_file;
            let result = tspath::for_each_ancestor_directory(&search_path, |directory| {
                if !skip {
                    let custom_path =
                        tspath::combine_paths(directory, &[&self.custom_config_file_name]);
                    if self.fs.file_exists(&custom_path) {
                        return Some(custom_path);
                    }
                }
                if directory.ends_with("/node_modules") {
                    return Some(String::new());
                }
                skip = false;
                None
            })
            .unwrap_or_default();
            if !result.is_empty() {
                logger.logf(format_args!(
                    "computeConfigFileName:: File: {file_name}:: Result: {result}"
                ));
                return result;
            }
        }

        // When searching for ancestor of a config file, determine which config types to skip
        // in the starting directory. This matches TSServer's forEachConfigFileLocation behavior:
        // - For ancestor of tsconfig.json: skip tsconfig.json but still check jsconfig.json
        // - For ancestor of jsconfig.json: skip both tsconfig.json and jsconfig.json
        let mut skip_tsconfig = skip_search_in_directory_of_file;
        let mut skip_jsconfig =
            skip_search_in_directory_of_file && !file_name.ends_with("/tsconfig.json");
        let result = tspath::for_each_ancestor_directory(&search_path, |directory| {
            if !skip_tsconfig {
                let tsconfig_path = tspath::combine_paths(directory, &["tsconfig.json"]);
                if self.fs.file_exists(&tsconfig_path) {
                    return Some(tsconfig_path);
                }
            }
            if !skip_jsconfig {
                let jsconfig_path = tspath::combine_paths(directory, &["jsconfig.json"]);
                if self.fs.file_exists(&jsconfig_path) {
                    return Some(jsconfig_path);
                }
            }
            if directory.ends_with("/node_modules") {
                return Some(String::new());
            }
            skip_tsconfig = false;
            skip_jsconfig = false;
            None
        })
        .unwrap_or_default();
        logger.logf(format_args!("computeConfigFileName:: File: {file_name}:: Result: {result}"));
        result
    }

    // configfileregistrybuilder.go:722
    pub(crate) fn get_config_file_name_for_file(
        &self,
        file_name: &str,
        path: &Path,
        logger: &LogTree,
    ) -> String {
        if tspath::is_dynamic_file_name(file_name) {
            return String::new();
        }

        if let Some(entry) = self.config_file_names.get(path) {
            return entry.value().unwrap().nearest_config_file_name.clone();
        }

        let config_name = self.compute_config_file_name(file_name, false, logger);
        if (self.is_open_file)(path) {
            self.config_file_names.add(
                path.clone(),
                Shared::new(ConfigFileNames {
                    nearest_config_file_name: config_name.clone(),
                    ancestors: None,
                }),
            );
        }
        config_name
    }

    // configfileregistrybuilder.go:740
    pub(crate) fn for_each_config_file_name_for(&self, path: &Path, mut cb: impl FnMut(&str)) {
        if tspath::is_dynamic_file_name(path) {
            return;
        }

        if let Some(entry) = self.config_file_names.get(path) {
            let value = entry.value().unwrap();
            let mut config_file_name = value.nearest_config_file_name.clone();
            while !config_file_name.is_empty() {
                cb(&config_file_name);
                match value.ancestors.as_ref().and_then(|a| a.get(&config_file_name)) {
                    Some(ancestor_config_name) => config_file_name = ancestor_config_name.clone(),
                    None => return,
                }
            }
        }
    }

    // configfileregistrybuilder.go:758
    pub(crate) fn get_ancestor_config_file_name(
        &self,
        file_name: &str,
        path: &Path,
        config_file_name: &str,
        logger: &LogTree,
    ) -> String {
        if tspath::is_dynamic_file_name(file_name) {
            return String::new();
        }

        let Some(entry) = self.config_file_names.get(path) else {
            return String::new();
        };

        if let Some(ancestor_config_name) =
            entry.value().unwrap().ancestors.as_ref().and_then(|a| a.get(config_file_name))
        {
            return ancestor_config_name.clone();
        }

        // Look for config in parent folders of config file
        let result = self.compute_config_file_name(config_file_name, true, logger);

        if (self.is_open_file)(path) {
            let r = result.clone();
            entry.change(|value| {
                value
                    .ancestors
                    .get_or_insert_with(FxHashMap::default)
                    .insert(config_file_name.to_string(), r);
            });
        }
        result
    }

    // configfileregistrybuilder.go:814
    pub(crate) fn cleanup(&self) {
        let mut changed = false;
        self.configs.range(|entry| {
            entry.delete_if(|value| {
                let no_retaining_projects = match value.retaining_projects.as_ref() {
                    Some(projects) => projects.is_empty(),
                    None => true,
                };
                let no_retaining_open_files = match value.retaining_open_files.as_ref() {
                    Some(files) => files.is_empty(),
                    None => true,
                };
                let no_retaining_configs = match value.retaining_configs.as_ref() {
                    Some(configs) => configs.is_empty(),
                    None => true,
                };
                let should_delete =
                    no_retaining_projects && no_retaining_open_files && no_retaining_configs;
                changed = changed || should_delete;
                should_delete
            });
            true
        });
        if changed {
            self.invalidate_content_mappers();
        }
    }
}

// configfileregistrybuilder.go:656
fn content_mapper_manifest_path(
    command_line: Option<P<ParsedCommandLine>>,
    to_path: impl Fn(&str) -> Path,
    path: &Path,
) -> bool {
    let Some(command_line) = command_line else {
        return false;
    };
    command_line.content_mappers().iter().any(|mapper| {
        !mapper.definition.package.is_empty()
            && mapper.contribution_id.is_empty()
            && !mapper.package_directory.is_empty()
            && to_path(&tspath::combine_paths(&mapper.package_directory, &["package.json"]))
                == *path
    })
}

impl ParseConfigHost for ConfigFileRegistryBuilder {
    // configfileregistrybuilder.go:787
    fn fs(&self) -> &dyn FS {
        &*self.fs
    }

    // configfileregistrybuilder.go:792
    fn get_current_directory(&self) -> &str {
        &self.session_options.current_directory
    }
}

impl ExtendedConfigCacheTrait for ConfigFileRegistryBuilder {
    // configfileregistrybuilder.go:797
    fn get_extended_config(
        &self,
        file_name: &str,
        path: &Path,
        resolution_stack: &[Path],
        host: &dyn ParseConfigHost,
    ) -> P<tsoptions::ExtendedConfigCacheEntry> {
        let mut content = String::new();
        if let Some(fh) = self.fs.get_file_by_path(file_name, path) {
            content = fh.content().to_string();
        }

        self.extended_config_cache
            .load_and_acquire(
                path,
                self.snapshot_id,
                &ExtendedConfigParseArgs {
                    file_name: file_name.to_string(),
                    content,
                    fs: self.fs.source(),
                    resolution_stack: resolution_stack.to_vec(),
                    host,
                    cache: self,
                },
            )
            .extended_config_cache_entry
    }
}
