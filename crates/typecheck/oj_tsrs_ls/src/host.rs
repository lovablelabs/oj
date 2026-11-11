use std::sync::Arc;

use crate::autoimport;
use crate::lsconv::Converters;
use crate::lsutil::UserPreferences;
use crate::sourcemap::ECMALineInfo;

// tsrs-only: a file's text shared with the host. Go's `ReadFile` returns a string, which shares its bytes; a copied
// `String` per call made every find-all-references result cost a copy of its whole file.
#[derive(Clone)]
pub struct FileText(Arc<dyn AsRef<str> + Send + Sync>);

impl FileText {
    pub fn new(text: impl AsRef<str> + Send + Sync + 'static) -> FileText {
        FileText(Arc::new(text))
    }
}

impl std::ops::Deref for FileText {
    type Target = str;

    fn deref(&self) -> &str {
        (*self.0).as_ref()
    }
}

// host.go:10
pub trait Host: Send + Sync {
    fn use_case_sensitive_file_names(&self) -> bool;
    fn read_file(&self, path: &str) -> Option<String>;
    // `read_file` without the copy, for hosts that keep the text (the default copies it once).
    fn read_file_text(&self, path: &str) -> Option<FileText> {
        self.read_file(path).map(FileText::new)
    }
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
