use std::sync::Arc;

use crate::autoimport;
use crate::lsconv::Converters;
use crate::lsutil::UserPreferences;
use crate::sourcemap::ECMALineInfo;

// host.go:10
pub trait Host: Send + Sync {
    fn use_case_sensitive_file_names(&self) -> bool;
    fn read_file(&self, path: &str) -> Option<String>;
    fn converters(&self) -> Arc<Converters>;
    fn get_preferences(&self, active_file: &str) -> UserPreferences;
    fn get_ecma_line_info(&self, file_name: &str) -> Option<Arc<ECMALineInfo>>;
    fn auto_import_registry(&self) -> Option<Arc<autoimport::Registry>>;

    // Used for module specifier completions.
    // ! Do not use for anything else, as this violates the principle that
    // the host is a snapshot-in-time.
    fn read_directory(
        &self,
        current_dir: &str,
        path: &str,
        extensions: &[String],
        excludes: &[String],
        includes: &[String],
        depth: usize,
    ) -> Vec<String>;
    fn get_directories(&self, path: &str) -> Vec<String>;
    fn directory_exists(&self, path: &str) -> bool;
    fn file_exists(&self, path: &str) -> bool;
}
