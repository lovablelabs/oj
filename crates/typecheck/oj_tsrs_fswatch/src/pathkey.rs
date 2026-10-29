use crate::canonicalize::{NATIVE_PATH_FOLDING, fold_native_path};
use crate::pathcompare::BytePathComparer;

// PathComparer describes the filename equivalence of a watched volume. Its zero
// value compares bytes exactly. It is immutable and safe to share.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PathComparer {
    pub(crate) comparer: BytePathComparer,
}

impl PathComparer {
    // pathkey.go:21
    // Key returns a watch-only comparison key, not a filesystem path or a compiler
    // identity. Native Darwin comparers use the same CoreFoundation folding as the
    // watcher. Other comparers preserve bytes, including malformed UTF-8.
    pub fn key(self, path: &str) -> String {
        if !self.comparer.ignore_case || !NATIVE_PATH_FOLDING {
            return path.to_string();
        }
        if path.contains('\0') {
            return path.to_string();
        }
        let folded = fold_native_path(path);
        if !folded.is_empty() {
            return folded;
        }
        // Invalid UTF-8 and NUL-containing names are opaque, not Unicode aliases.
        path.to_string()
    }

    // pathkey.go:36
    // Rebase replaces a matching directory prefix while preserving the spelling and
    // byte boundaries of the remaining event path. (Rust strings are valid UTF-8, so Go's invalid-UTF-8 branch
    // reduces to the NUL check.)
    pub fn rebase(self, path: &str, from: &str, to: &str) -> Option<String> {
        if path.contains('\0') || from.contains('\0') {
            return BytePathComparer::default().rebase(path, from, to);
        }
        self.comparer.rebase(path, from, to)
    }
}
