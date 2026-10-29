use std::sync::Arc;

use tsrs_core::{CompilerOptions, P, ProjectReference, TypeAcquisition};

use crate::contentmappers::Mapper;

#[derive(Clone, Debug, Default)]
pub struct ParsedOptions {
    pub compiler_options: Option<P<CompilerOptions>>,
    pub type_acquisition: Option<TypeAcquisition>,

    pub file_names: Vec<String>,
    pub project_references: Vec<ProjectReference>,
    // Go keeps `ProjectReferences` nil when the config has no "references" (build mode treats files: [] plus
    // references: [] as a solution).
    pub project_references_is_nil: bool,
    // Go `[]*Mapper`: the copies of a config (`reload_file_names_of_parsed_command_line`) share the mappers. A content
    // mapper project knows the mappers it was made with by address (hostimpl.rs `mapper_key`).
    pub content_mappers: Arc<[Mapper]>,
}

impl ParsedOptions {
    // Go ParsedOptions.Equals delegates to each option type's equality semantics. In particular,
    // TypeAcquisition treats nil and empty include/exclude lists as equivalent.
    pub fn equals(&self, other: &Self) -> bool {
        (match (self.compiler_options.as_deref(), other.compiler_options.as_deref()) {
            (None, None) => true,
            (Some(a), Some(b)) => a.equals(b),
            _ => false,
        }) && TypeAcquisition::equals(
            self.type_acquisition.as_ref(),
            other.type_acquisition.as_ref(),
        ) && self.file_names == other.file_names
            && self.project_references == other.project_references
            && self.project_references_is_nil == other.project_references_is_nil
            && self.content_mappers == other.content_mappers
    }
}
