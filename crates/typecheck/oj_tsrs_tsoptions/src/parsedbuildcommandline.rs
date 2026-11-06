use std::sync::OnceLock;

use tsrs_ast::Diagnostic;
use tsrs_core::tspath::{self, ComparePathsOptions};
use tsrs_core::{BuildOptions, CompilerOptions, P, resolve_config_file_name_of_project_reference};

use crate::commandlineoption::CompilerOptionsValue;

pub struct ParsedBuildCommandLine {
    pub build_options: BuildOptions,
    pub compiler_options: CompilerOptions,
    pub projects: Vec<String>,
    pub errors: Vec<P<Diagnostic>>,
    pub raw: CompilerOptionsValue,

    pub(crate) compare_paths_options: ComparePathsOptions,

    pub(crate) resolved_project_paths: OnceLock<Vec<String>>,
}

impl ParsedBuildCommandLine {
    pub fn resolved_project_paths(&self) -> &[String] {
        self.resolved_project_paths.get_or_init(|| {
            self.projects
                .iter()
                .map(|project| {
                    resolve_config_file_name_of_project_reference(&tspath::resolve_path(
                        &self.compare_paths_options.current_directory,
                        &[project],
                    ))
                })
                .collect()
        })
    }
}
