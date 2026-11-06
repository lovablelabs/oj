use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tsrs_core::tspath;

use crate::FS;

pub(crate) fn wrap_fs<T: FS>(fs: T) -> T {
    fs
}

// bundled/noembed.go: libPath is evaluated once, including the executable realpath
// and the misplaced-executable check.
static LIB_PATH: OnceLock<String> = OnceLock::new();

/// Makes `lib_path` the source libraries, as Go's tests get them (`testing.Testing()`). Tests call it before
/// anything reads `lib_path`.
pub fn use_testing_lib_path() {
    let _ = LIB_PATH.set(super::testing_lib_path());
}

fn executable_lib_path() -> String {
    let exe = std::env::current_exe()
        .and_then(|path| executable_realpath(&path))
        .expect("bundled: failed to get executable path");
    let dir = exe.parent().expect("bundled: executable has no directory");
    let libdts = dir.join("lib.d.ts");
    assert!(
        libdts.exists(),
        "bundled: {} does not exist; this executable may be misplaced",
        libdts.display()
    );
    tspath::normalize_slashes(&dir.to_string_lossy())
}

pub fn executable_realpath(path: &Path) -> io::Result<PathBuf> {
    // nativepath.Realpath removes Windows verbatim prefixes before paths reach
    // the compiler's filesystem, which rejects device and network paths.
    #[cfg(windows)]
    {
        crate::osvfs::native_realpath(&path.to_string_lossy()).map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::fs::canonicalize(path)
    }
}

pub(crate) fn lib_path() -> String {
    LIB_PATH.get_or_init(executable_lib_path).clone()
}

// The suite runners still mount bundled paths when training a noembed compiler.
// Recognize their URI scheme without making any embedded contents reachable from the CLI.
pub fn is_bundled(path: &str) -> bool {
    super::embed::is_bundled(path)
}

pub(crate) fn embedded_file(_path: &str) -> Option<&'static str> {
    None
}
