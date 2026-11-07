use super::{
    EncodedRootLength, get_encoded_root_length, get_normalized_absolute_path, get_root_length,
    has_trailing_directory_separator, path_is_absolute,
};

fn rooted_path_from_absolute(path: &str) -> String {
    try_rooted_path_from_absolute(path).unwrap_or_else(|| panic!("path must be absolute"))
}

fn try_rooted_path_from_absolute(path: &str) -> Option<String> {
    if has_rooted_url_suffix(path) || !path_is_absolute(path) {
        return None;
    }
    Some(ensure_rooted_path_root_separator(get_normalized_absolute_path(path, "")))
}

fn ensure_rooted_path_root_separator(mut path: String) -> String {
    if get_root_length(&path) == path.len() && !has_trailing_directory_separator(&path) {
        path.push('/');
    }
    path
}

fn has_rooted_url_suffix(path: &str) -> bool {
    if !has_url_root(path) {
        return false;
    }
    let (_, after_scheme) = path.split_once("://").unwrap();
    after_scheme.contains(['?', '#'])
}

fn has_url_root(path: &str) -> bool {
    matches!(get_encoded_root_length(path), EncodedRootLength::Url(_)) && path.contains("://")
}

pub fn rooted_directory_path_from_absolute(path: &str) -> String {
    rooted_path_from_absolute(path)
}
