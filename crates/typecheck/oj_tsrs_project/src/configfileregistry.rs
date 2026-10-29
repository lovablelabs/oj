use std::sync::Arc;

use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_core::P;
use tsrs_core::collections::Set;
use tsrs_core::tspath::Path;
use tsrs_tsoptions::ParsedCommandLine;

use crate::dirty::{Cloneable, SharedMap};
use crate::project::{ID, PendingReload};
use crate::watch::{ALL_WATCH_KINDS, PatternsAndIgnored, WatchedFiles, new_watched_files};

#[derive(Clone, Default)]
pub struct ConfigFileRegistry {
    // configs is a map of config file paths to their entries.
    pub(crate) configs: SharedMap<Path, ConfigFileEntry>,
    // configFileNames is a map of open file paths to information
    // about their ancestor config file names. It is only used as
    // a cache during
    pub(crate) config_file_names: SharedMap<Path, ConfigFileNames>,
    // customConfigFileName is the custom config file name preference that was
    // used when building this registry's configFileNames cache.
    pub(crate) custom_config_file_name: String,
    pub(crate) all_configured_content_mappers: Option<Arc<ConfiguredContentMappers>>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConfiguredContentMappers {
    pub(crate) extensions: Vec<String>,
}

// configfileregistry.go:32
pub(crate) fn collect_configured_content_mappers(
    command_lines: &[P<ParsedCommandLine>],
) -> Arc<ConfiguredContentMappers> {
    let mut seen_extensions: Set<String> = Set::default();
    let mut extensions: Vec<String> = Vec::new();
    extensions.extend(
        command_lines
            .iter()
            .flat_map(|command_line| command_line.content_mappers())
            .flat_map(|mapper| &mapper.definition.extensions)
            .filter(|extension| seen_extensions.add_if_absent((*extension).clone()))
            .cloned(),
    );
    extensions.sort();
    Arc::new(ConfiguredContentMappers { extensions })
}

impl ConfigFileRegistry {
    // configfileregistry.go:48
    pub(crate) fn content_mappers(&self) -> Arc<ConfiguredContentMappers> {
        if let Some(mappers) = &self.all_configured_content_mappers {
            return Arc::clone(mappers);
        }
        let command_lines: Vec<P<ParsedCommandLine>> =
            self.configs.values().filter_map(|entry| entry.command_line).collect();
        collect_configured_content_mappers(&command_lines)
    }
}

#[derive(Clone)]
pub struct ConfigFileEntry {
    pub(crate) file_name: String,
    pub(crate) pending_reload: PendingReload,
    pub(crate) command_line: Option<P<ParsedCommandLine>>,
    // retainingProjects is the set of projects that have called acquireConfig
    // without releasing it. A config file entry may be acquired by a project
    // either because it is the config for that project or because it is the
    // config for a referenced project.
    pub(crate) retaining_projects: Option<FxHashSet<ID>>,
    // retainingOpenFiles is the set of open files that caused this config to
    // load during project collection building. This config file may or may not
    // end up being the config for the default project for these files, but
    // determining the default project loaded this config as a candidate, so
    // subsequent calls to `projectCollectionBuilder.findDefaultConfiguredProject`
    // will use this config as part of the search, so it must be retained.
    pub(crate) retaining_open_files: Option<FxHashSet<Path>>,
    // retainingConfigs is the set of config files that extend this one. This
    // provides a cheap reverse mapping for a project config's
    // `commandLine.ExtendedSourceFiles()` that can be used to notify the
    // extending projects when this config changes. An extended config file may
    // or may not also be used directly by a project, so it's possible that
    // when this is set, no other fields will be used.
    pub(crate) retaining_configs: Option<FxHashSet<Path>>,
    // rootFilesWatch is a watch for the root files of this config file.
    pub(crate) root_files_watch: Option<Arc<WatchedFiles<PatternsAndIgnored>>>,
}

// configfileregistry.go:88
pub(crate) fn new_config_file_entry(
    has_relative_pattern_capability: bool,
    file_name: &str,
) -> ConfigFileEntry {
    ConfigFileEntry {
        file_name: file_name.to_string(),
        pending_reload: PendingReload::Full,
        command_line: None,
        retaining_projects: None,
        retaining_open_files: None,
        retaining_configs: None,
        root_files_watch: Some(new_watched_files(
            format!("root files for {file_name}"),
            ALL_WATCH_KINDS,
            has_relative_pattern_capability,
            Arc::new(|input: &PatternsAndIgnored| input.clone()),
        )),
    }
}

// configfileregistry.go:101
pub(crate) fn new_extended_config_file_entry(
    file_name: &str,
    extending_config_path: Path,
) -> ConfigFileEntry {
    let mut retaining_configs = FxHashSet::default();
    retaining_configs.insert(extending_config_path);
    ConfigFileEntry {
        file_name: file_name.to_string(),
        pending_reload: PendingReload::Full,
        command_line: None,
        retaining_projects: None,
        retaining_open_files: None,
        retaining_configs: Some(retaining_configs),
        root_files_watch: None,
    }
}

impl Cloneable for ConfigFileEntry {
    // configfileregistry.go:109
    fn clone_value(&self) -> ConfigFileEntry {
        ConfigFileEntry {
            file_name: self.file_name.clone(),
            pending_reload: self.pending_reload,
            command_line: self.command_line,
            // !!! eagerly cloning these maps makes everything more convenient,
            // but it could be avoided if needed.
            retaining_projects: self.retaining_projects.clone(),
            retaining_open_files: self.retaining_open_files.clone(),
            retaining_configs: self.retaining_configs.clone(),
            root_files_watch: self.root_files_watch.clone(),
        }
    }
}

impl ConfigFileRegistry {
    // configfileregistry.go:123
    pub fn get_config(&self, path: &Path) -> Option<P<ParsedCommandLine>> {
        self.configs.get(path)?.command_line
    }

    // configfileregistry.go:130
    pub(crate) fn is_tracked(&self, path: &Path) -> bool {
        self.configs.contains_key(path)
    }

    // configfileregistry.go:135
    pub(crate) fn get_config_file_name(&self, path: &Path) -> String {
        match self.config_file_names.get(path) {
            Some(entry) => entry.nearest_config_file_name.clone(),
            None => String::new(),
        }
    }

    // configfileregistry.go:142
    pub(crate) fn get_ancestor_config_file_name(
        &self,
        path: &Path,
        higher_than_config: &str,
    ) -> String {
        match self.config_file_names.get(path) {
            Some(entry) => entry
                .ancestors
                .as_ref()
                .and_then(|a| a.get(higher_than_config).cloned())
                .unwrap_or_default(),
            None => String::new(),
        }
    }

    // configfileregistry.go:150
    // clone creates a shallow copy of the configFileRegistry.
    pub(crate) fn clone_registry(&self) -> ConfigFileRegistry {
        ConfigFileRegistry {
            configs: Arc::new((*self.configs).clone()),
            config_file_names: Arc::new((*self.config_file_names).clone()),
            custom_config_file_name: self.custom_config_file_name.clone(),
            all_configured_content_mappers: self.all_configured_content_mappers.clone(),
        }
    }
}

// For testing
pub struct TestConfigEntry {
    pub file_name: String,
    pub retaining_projects: Vec<ID>,
    pub retaining_open_files: Vec<Path>,
    pub retaining_configs: Vec<Path>,
}

fn test_config_entry(entry: &ConfigFileEntry) -> TestConfigEntry {
    TestConfigEntry {
        file_name: entry.file_name.clone(),
        retaining_projects: entry.retaining_projects.iter().flatten().cloned().collect(),
        retaining_open_files: entry.retaining_open_files.iter().flatten().cloned().collect(),
        retaining_configs: entry.retaining_configs.iter().flatten().cloned().collect(),
    }
}

impl ConfigFileRegistry {
    // configfileregistry.go:168
    // For testing
    pub fn for_each_test_config_entry(&self, mut cb: impl FnMut(&Path, &TestConfigEntry)) {
        #[expect(
            clippy::iter_over_hash_type,
            reason = "test-only; the caller adds rows to a table it sorts; Go ranges the map too"
        )]
        for (path, entry) in self.configs.iter() {
            cb(path, &test_config_entry(entry));
        }
    }

    // configfileregistry.go:182
    // For testing

    // configfileregistry.go:202
    // For testing
    pub fn for_each_test_config_file_names_entry(
        &self,
        mut cb: impl FnMut(&Path, &TestConfigFileNamesEntry),
    ) {
        #[expect(
            clippy::iter_over_hash_type,
            reason = "test-only; the caller adds rows to a table it sorts; Go ranges the map too"
        )]
        for (path, entry) in self.config_file_names.iter() {
            cb(
                path,
                &TestConfigFileNamesEntry {
                    nearest_config_file_name: entry.nearest_config_file_name.clone(),
                    ancestors: entry.ancestors.clone(),
                },
            );
        }
    }

    // configfileregistry.go:214
    // For testing
}

pub struct TestConfigFileNamesEntry {
    pub nearest_config_file_name: String,
    pub ancestors: Option<FxHashMap<String, String>>,
}

#[derive(Clone, Default)]
pub struct ConfigFileNames {
    // nearestConfigFileName is the file name of the nearest ancestor config file.
    pub(crate) nearest_config_file_name: String,
    // ancestors is a map from one ancestor config file path to the next.
    // For example, if `/a`, `/a/b`, and `/a/b/c` all contain config files,
    // the fully loaded map will look like:
    //      {
    //          "/a/b/c/tsconfig.json": "/a/b/tsconfig.json",
    //          "/a/b/tsconfig.json": "/a/tsconfig.json"
    //      }
    pub(crate) ancestors: Option<FxHashMap<String, String>>,
}

impl Cloneable for ConfigFileNames {
    // configfileregistry.go:239
    fn clone_value(&self) -> ConfigFileNames {
        ConfigFileNames {
            nearest_config_file_name: self.nearest_config_file_name.clone(),
            ancestors: self.ancestors.clone(),
        }
    }
}
