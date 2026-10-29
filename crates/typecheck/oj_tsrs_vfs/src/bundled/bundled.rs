use tsrs_core::tspath;

use super::implementation;
use crate::FS;

// Embedded is true if the bundled files are implemented through an embedded FS.
pub const EMBEDDED: bool = !cfg!(feature = "noembed");

// WrapFS returns an FS which redirects embedded paths to the embedded file system.
// If embedding is disabled, it returns the original FS.
pub fn wrap_fs<T: FS>(fs: T) -> impl FS {
    implementation::wrap_fs(fs)
}

// LibPath returns the path to the directory containing the bundled lib.d.ts files.
pub fn lib_path() -> String {
    implementation::lib_path()
}

/// The default library directory, with the tsrs-only `TSRS_LIB_PATH` override.
///
/// Otherwise follows Go's build-selected `bundled.LibPath`: embedded libraries for
/// standalone builds, or the executable's directory for npm (`noembed`) builds.
pub fn default_lib_path() -> String {
    default_lib_path_from(std::env::var("TSRS_LIB_PATH").ok())
}

pub fn default_lib_path_from(override_dir: Option<String>) -> String {
    match override_dir.filter(|dir| !dir.is_empty()) {
        Some(dir) => tspath::normalize_path(&dir),
        None => lib_path(),
    }
}

// The embedded text of a bundled lib path without a copy (tsrs-only, see implementation::embedded_file).
pub fn embedded_file(path: &str) -> Option<&'static str> {
    implementation::embedded_file(path)
}

// TestingLibPath returns the path to the source bundled libs directory.
// It's only valid to use where the source code is available.
pub fn testing_lib_path() -> String {
    tspath::normalize_slashes(concat!(env!("CARGO_MANIFEST_DIR"), "/libs"))
}
