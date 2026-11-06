static IGNORED_PATHS: &[&str] = &["/node_modules/.", "/.git", ".#"];

pub fn contains_ignored_path(path: &str) -> bool {
    IGNORED_PATHS.iter().any(|pattern| path.contains(pattern))
}
