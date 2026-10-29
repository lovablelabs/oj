use crate::tspath;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProjectReference {
    // Path is a normalized path on disk.
    pub path: String,
    // OriginalPath is the path as it was originally written.
    pub original_path: String,
    // Circular indicates that this reference is intended to form a circularity.
    pub circular: bool,
}

pub fn resolve_project_reference_path(ref_: &ProjectReference) -> String {
    resolve_config_file_name_of_project_reference(&ref_.path)
}

pub fn resolve_config_file_name_of_project_reference(path: &str) -> String {
    if tspath::file_extension_is(path, tspath::EXTENSION_JSON) {
        return path.to_string();
    }
    tspath::combine_paths(path, &["tsconfig.json"])
}
