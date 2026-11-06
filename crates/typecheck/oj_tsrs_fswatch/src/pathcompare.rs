use rustc_hash::FxHashMap;
use std::sync::Mutex;
use tsrs_core::stringutil::equal_fold;

use crate::canonicalize::{NATIVE_PATH_FOLDING, fold_native_path};
use crate::watcher::{is_in_directory_or_self, join_path_suffix, rebase_path};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct BytePathComparer {
    pub ignore_case: bool,
}

// Watch roots are prepared before publication and are immutable thereafter.
// Event paths are local to one routing operation and folded only on demand.
#[derive(Clone, Default)]
pub struct ComparisonPath<'c> {
    pub(crate) path: String,
    pub(crate) folded: String,
    pub(crate) ready: bool,
    pub(crate) cache: Option<&'c Mutex<ComparisonCache>>,
}

// Shared only within a synchronous callback/termination pass, never published
// to a subscriber or stored on a watch.
pub type ComparisonCache = FxHashMap<String, String>;

impl ComparisonPath<'_> {
    pub(crate) fn new(path: &str) -> ComparisonPath<'static> {
        ComparisonPath { path: path.to_string(), ..Default::default() }
    }

    // pathcompare.go:34
    fn fold(&mut self) -> String {
        if !self.ready {
            if let Some(cache) = self.cache
                && let Some(folded) = cache.lock().unwrap().get(&self.path)
            {
                self.folded = folded.clone();
                self.ready = true;
                return self.folded.clone();
            }
            self.folded = fold_native_path(&self.path);
            self.ready = true;
            if let Some(cache) = self.cache {
                cache.lock().unwrap().insert(self.path.clone(), self.folded.clone());
            }
        }
        self.folded.clone()
    }
}

impl BytePathComparer {
    // pathcompare.go:26
    pub(crate) fn prepare(self, path: &str) -> ComparisonPath<'static> {
        let mut p = ComparisonPath::new(path);
        if self.ignore_case && NATIVE_PATH_FOLDING {
            p.fold();
        }
        p
    }

    // pathcompare.go:56
    // suffix returns the part of path below root, respecting directory boundaries.
    pub fn suffix(self, root: &str, path: &str) -> Option<String> {
        let mut p = ComparisonPath::new(path);
        self.suffix_prepared(&ComparisonPath::new(root), &mut p)
    }

    // pathcompare.go:61
    pub(crate) fn suffix_prepared(
        self,
        root: &ComparisonPath,
        path: &mut ComparisonPath,
    ) -> Option<String> {
        if is_in_directory_or_self(&root.path, &path.path) {
            return Some(path.path[root.path.len()..].to_string());
        }
        if !self.ignore_case || root.path.is_empty() {
            return None;
        }
        let (suffix, unicode) = path_suffix_ascii(&root.path, &path.path);
        if !unicode {
            return suffix;
        }
        Self::suffix_unicode(root, path)
    }

    // pathcompare.go:75
    fn suffix_unicode(root: &ComparisonPath, path: &mut ComparisonPath) -> Option<String> {
        if !NATIVE_PATH_FOLDING {
            return path_suffix_fold_unicode(&root.path, &path.path);
        }
        let mut root = root.clone();
        let (a, b) = (root.fold(), path.fold());
        if a.is_empty() || b.is_empty() {
            // CFString cannot represent invalid UTF-8. Retain the simple-fold
            // behavior for malformed paths rather than truncating or losing bytes.
            return path_suffix_fold_unicode(&root.path, &path.path);
        }
        if !is_in_directory_or_self(&a, &b) {
            return None;
        }
        if a == b {
            return Some(String::new());
        }
        // Folding and canonical normalization preserve separators, but not byte
        // lengths. Find the matching boundary in the original event, not its fold.
        let mut offset = 0;
        let mut separators = root.path.matches('/').count();
        let trailing_separator = root.path.ends_with('/');
        if !trailing_separator {
            separators += 1;
        }
        for _ in 0..separators {
            let Some(i) = path.path[offset..].find('/') else {
                panic!("fswatch: folded path lost a directory boundary");
            };
            offset += i + 1;
        }
        if trailing_separator {
            return Some(path.path[offset..].to_string());
        }
        Some(path.path[offset - 1..].to_string())
    }

    // pathcompare.go:166
    pub fn contains(self, root: &str, path: &str) -> bool {
        self.suffix(root, path).is_some()
    }

    // pathcompare.go:171
    pub fn rebase(self, path: &str, from: &str, to: &str) -> Option<String> {
        let mut p = ComparisonPath::new(path);
        self.rebase_prepared(&mut p, &ComparisonPath::new(from), to)
    }

    // pathcompare.go:176
    pub(crate) fn rebase_prepared(
        self,
        path: &mut ComparisonPath,
        from: &ComparisonPath,
        to: &str,
    ) -> Option<String> {
        if is_in_directory_or_self(&from.path, &path.path) {
            return Some(rebase_path(&path.path, &from.path, to));
        }
        if !self.ignore_case || from.path.is_empty() {
            return None;
        }
        let (mut suffix, unicode) = path_suffix_ascii(&from.path, &path.path);
        if unicode {
            suffix = Self::suffix_unicode(from, path);
        }
        let suffix = suffix?;
        Some(join_path_suffix(to, &suffix))
    }
}

// pathcompare.go:114
// The second result requests Unicode comparison; an ASCII rejection must not
// reject an expanding alias just because the other spelling is ASCII.
fn path_suffix_ascii(root: &str, path: &str) -> (Option<String>, bool) {
    let (rb, pb) = (root.as_bytes(), path.as_bytes());
    let mut i = 0;
    // Skip shared prefixes a word at a time, which is common when routing an
    // event past sibling watches. String slice comparisons do not allocate.
    while i + 8 <= rb.len() && i + 8 <= pb.len() && rb[i..i + 8] == pb[i..i + 8] {
        i += 8;
    }
    while i < rb.len() && i < pb.len() {
        let (mut a, mut b) = (rb[i], pb[i]);
        if a >= 0x80 || b >= 0x80 {
            return (None, true);
        }
        if a == b {
            i += 1;
            continue;
        }
        a |= 0x20;
        b |= 0x20;
        if a != b || !a.is_ascii_lowercase() {
            return (None, false);
        }
        i += 1;
    }
    if i == rb.len() && (i == pb.len() || pb[i] == b'/') {
        return (Some(path[i..].to_string()), false);
    }
    (None, i < rb.len() && rb[i] >= 0x80 || i < pb.len() && pb[i] >= 0x80)
}

// pathcompare.go:145
// Comparing the remaining components avoids assuming case-equivalent UTF-8
// strings have the same byte length (for example, s and long s).
fn path_suffix_fold_unicode(mut root: &str, mut path: &str) -> Option<String> {
    loop {
        let (root_part, root_rest, root_more) = cut(root, '/');
        let (path_part, path_rest, path_more) = cut(path, '/');
        if !equal_fold(root_part, path_part) {
            return None;
        }
        if !root_more {
            if path_more {
                return Some(path[path_part.len()..].to_string());
            }
            return Some(String::new());
        }
        if !path_more {
            return None;
        }
        root = root_rest;
        path = path_rest;
    }
}

fn cut(s: &str, sep: char) -> (&str, &str, bool) {
    match s.find(sep) {
        Some(i) => (&s[..i], &s[i + sep.len_utf8()..], true),
        None => (s, "", false),
    }
}
