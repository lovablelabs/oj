use std::borrow::Borrow;
use std::cmp::Ordering;
use std::collections::hash_map::Entry;
use std::fmt;
use std::ops::Deref;

use rustc_hash::{FxHashMap, FxHashSet};

use crate::stringutil;
use crate::stringutil::{decode_rune, push_rune, unicode_to_lower};

#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Path(pub std::sync::Arc<str>);

impl Path {
    #[inline]
    pub fn new(s: impl Into<String>) -> Path {
        Path(std::sync::Arc::from(s.into()))
    }

    #[inline]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Deref for Path {
    type Target = str;
    #[inline]
    fn deref(&self) -> &str {
        &self.0
    }
}

impl Borrow<str> for Path {
    fn borrow(&self) -> &str {
        &self.0
    }
}

impl From<String> for Path {
    fn from(s: String) -> Path {
        Path(std::sync::Arc::from(s))
    }
}

impl From<&str> for Path {
    fn from(s: &str) -> Path {
        Path(std::sync::Arc::from(s))
    }
}

impl fmt::Debug for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

// Internally, we represent paths as strings with '/' as the directory separator.
// When we make system calls (eg: LanguageServiceHost.getDirectory()),
// we expect the host to correctly handle paths in our specified format.
pub const DIRECTORY_SEPARATOR: u8 = b'/';
const URL_SCHEME_SEPARATOR: &str = "://";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncodedRootLength {
    Relative,
    Disk(usize),
    Url(usize),
}

impl EncodedRootLength {
    #[expect(
        clippy::len_without_is_empty,
        reason = "This accessor reports a position or optional diagnostic length, not a container size."
    )]
    pub fn len(self) -> usize {
        match self {
            Self::Relative => 0,
            Self::Disk(length) | Self::Url(length) => length,
        }
    }
}

// Path Tests

// Determines whether a byte corresponds to `/` or `\`.
fn is_any_directory_separator(char: u8) -> bool {
    char == b'/' || char == b'\\'
}

// Reports whether Windows may resolve path over the network. This includes
// UNC paths, Win32 device paths, and NT object paths, with either separator.
// Check the original bytes before normalizing: normalization can erase a
// network prefix on POSIX hosts, where these paths are also refused.
pub fn is_network_path(path: &str) -> bool {
    let bytes = path.as_bytes();
    if bytes.len() < 2 || !is_any_directory_separator(bytes[0]) {
        return false;
    }
    if is_any_directory_separator(bytes[1]) {
        return true;
    }
    bytes.len() >= 3
        && bytes[1] == b'?'
        && bytes[2] == b'?'
        && (bytes.len() == 3 || is_any_directory_separator(bytes[3]))
}

// Determines whether a path starts with a URL scheme (e.g. starts with `http://`, `ftp://`, `file://`, etc.).
pub fn is_url(path: &str) -> bool {
    matches!(get_encoded_root_length(path), EncodedRootLength::Url(_))
}

// Determines whether a path is an absolute disk path (e.g. starts with `/`, or a dos path
// like `c:`, `c:\` or `c:/`).
pub fn is_rooted_disk_path(path: &str) -> bool {
    matches!(get_encoded_root_length(path), EncodedRootLength::Disk(_))
}

// Determines whether a path consists only of a path root.
pub fn is_disk_path_root(path: &str) -> bool {
    matches!(get_encoded_root_length(path), EncodedRootLength::Disk(root_length) if root_length == path.len())
}

// IsDynamicFileName returns true if the file name represents a dynamic/virtual file
// that doesn't exist on disk (e.g., untitled files with paths like "^/untitled/...").
pub fn is_dynamic_file_name(file_name: &str) -> bool {
    file_name.starts_with("^/")
}

// Determines whether a path starts with an absolute path component (i.e. `/`, `c:/`, `file://`, etc.).
pub fn path_is_absolute(path: &str) -> bool {
    get_encoded_root_length(path) != EncodedRootLength::Relative
}

pub fn has_trailing_directory_separator(path: &str) -> bool {
    !path.is_empty() && is_any_directory_separator(path.as_bytes()[path.len() - 1])
}

// Combines paths. If a path is absolute, it replaces any previous path. Relative paths are not simplified.
//
//	// Non-rooted
//	CombinePaths("path", "to", "file.ext") === "path/to/file.ext"
//	CombinePaths("path", "dir", "..", "to", "file.ext") === "path/dir/../to/file.ext"
//	// POSIX
//	CombinePaths("/path", "to", "file.ext") === "/path/to/file.ext"
//	CombinePaths("/path", "/to", "file.ext") === "/to/file.ext"
pub fn combine_paths(first_path: &str, paths: &[&str]) -> String {
    let mut result = String::new();
    combine_paths_into(&mut result, first_path, paths);
    result
}

/// `combine_paths` appended to `result` (cleared first), for a caller that keeps a scratch string across calls.
pub fn combine_paths_into(result: &mut String, first_path: &str, paths: &[&str]) {
    // Each absolute path replaces everything before it, so start from the last one without normalizing what it
    // replaces (`first_path` is often the current directory).
    let is_absolute = |p: &str| {
        if p.as_bytes().contains(&b'\\') {
            get_root_length(&normalize_slashes(p)) != 0
        } else {
            get_root_length(p) != 0
        }
    };
    let (first, paths) = match paths.iter().rposition(|p| is_absolute(p)) {
        Some(last_absolute) => (paths[last_absolute], &paths[last_absolute + 1..]),
        None => (first_path, paths),
    };
    // One allocation for the result (a profile of the front end had this function reallocating on every
    // `push_str`): it holds `first`, and a separator and each trailing path at most.
    let first = normalize_slashes_cow(first);
    result.clear();
    result.reserve(first.len() + paths.iter().map(|p| p.len() + 1).sum::<usize>());
    result.push_str(&first);
    for &trailing_path in paths {
        if trailing_path.is_empty() {
            continue;
        }
        let trailing_path = normalize_slashes_cow(trailing_path);
        if result.is_empty() || get_root_length(&trailing_path) != 0 {
            // `trailingPath` is absolute.
            result.clear();
            result.push_str(&trailing_path);
        } else {
            if !has_trailing_directory_separator(result) {
                result.push('/');
            }
            result.push_str(&trailing_path);
        }
    }
}

pub fn get_path_components(path: &str, current_directory: &str) -> Vec<String> {
    let path = combine_paths(current_directory, &[path]);
    path_components(&path, get_root_length(&path))
}

fn path_components(path: &str, root_length: usize) -> Vec<String> {
    let root = &path[..root_length];
    let mut rest: Vec<&str> = path[root_length..].split('/').collect();
    if !rest.is_empty() && rest[rest.len() - 1].is_empty() {
        rest.pop();
    }
    let mut result = Vec::with_capacity(rest.len() + 1);
    result.push(root.to_string());
    result.extend(rest.into_iter().map(std::string::ToString::to_string));
    result
}

pub fn is_volume_character(char: u8) -> bool {
    char.is_ascii_lowercase() || char.is_ascii_uppercase()
}

fn get_file_url_volume_separator_end(url: &str, start: usize) -> Option<usize> {
    let url = url.as_bytes();
    if url.len() <= start {
        return None;
    }
    let ch0 = url[start];
    if ch0 == b':' {
        return Some(start + 1);
    }
    if ch0 == b'%' && url.len() > start + 2 && url[start + 1] == b'3' {
        let ch2 = url[start + 2];
        if ch2 == b'a' || ch2 == b'A' {
            return Some(start + 3);
        }
    }
    None
}

pub fn get_encoded_root_length(path: &str) -> EncodedRootLength {
    let bytes = path.as_bytes();
    let ln = bytes.len();
    if ln == 0 {
        return EncodedRootLength::Relative;
    }
    let ch0 = bytes[0];

    // POSIX or UNC
    if ch0 == b'/' || ch0 == b'\\' {
        if ln == 1 || bytes[1] != ch0 {
            return EncodedRootLength::Disk(1); // POSIX: "/" (or non-normalized "\")
        }

        let offset = 2;
        let Some(p1) = memchr::memchr(ch0, &bytes[offset..]) else {
            return EncodedRootLength::Disk(ln); // UNC: "//server" or "\\server"
        };

        return EncodedRootLength::Disk(p1 + offset + 1); // UNC: "//server/" or "\\server\"
    }

    // Dynamic/virtual compiler file name.
    if ch0 == b'^' && ln > 1 && bytes[1] == b'/' {
        if path.starts_with(super::DYNAMIC_URI_FILE_NAME_PREFIX) {
            let prefix_len = super::DYNAMIC_URI_FILE_NAME_PREFIX.len();
            if let Some(scheme_end) = path[prefix_len..].find('/') {
                let scheme_end = prefix_len + scheme_end;
                if let Some(authority_end) = path[scheme_end + 1..].find('/') {
                    return EncodedRootLength::Disk(scheme_end + authority_end + 2);
                }
                return EncodedRootLength::Url(ln);
            }
        }
        return EncodedRootLength::Disk(2);
    }

    // DOS
    if is_volume_character(ch0) && ln > 1 && bytes[1] == b':' {
        if ln == 2 {
            return EncodedRootLength::Disk(2); // DOS: "c:" (but not "c:d")
        }
        let ch2 = bytes[2];
        if ch2 == b'/' || ch2 == b'\\' {
            return EncodedRootLength::Disk(3); // DOS: "c:/" or "c:\"
        }
    }

    // URL
    // `path.find("://")` by its colons: the substring searcher's setup cost more than the search (most paths here are
    // relative module names without a colon).
    if let Some(scheme_end) =
        memchr::memchr_iter(b':', bytes).find(|&i| bytes[i + 1..].starts_with(b"//"))
    {
        let authority_start = scheme_end + URL_SCHEME_SEPARATOR.len();
        if let Some(authority_length) = path[authority_start..].find('/') {
            // URL: "file:///", "file://server/", "file://server/path"
            let authority_end = authority_start + authority_length;

            // For local "file" URLs, include the leading DOS volume (if present).
            // Per https://www.ietf.org/rfc/rfc1738.txt, a host of "" or "localhost" is a
            // special case interpreted as "the machine from which the URL is being interpreted".
            let scheme = &path[..scheme_end];
            let authority = &path[authority_start..authority_end];
            if scheme.eq_ignore_ascii_case("file")
                && (authority.is_empty() || authority.eq_ignore_ascii_case("localhost"))
                && (ln > authority_end + 2)
                && is_volume_character(bytes[authority_end + 1])
            {
                let volume_separator_end =
                    get_file_url_volume_separator_end(path, authority_end + 2);
                if let Some(volume_separator_end) = volume_separator_end {
                    if volume_separator_end == ln {
                        // URL: "file:///c:", "file://localhost/c:", "file:///c$3a", "file://localhost/c%3a"
                        // but not "file:///c:d" or "file:///c%3ad"
                        return EncodedRootLength::Url(volume_separator_end);
                    }
                    if bytes[volume_separator_end] == b'/' {
                        // URL: "file:///c:/", "file://localhost/c:/", "file:///c%3a/", "file://localhost/c%3a/"
                        return EncodedRootLength::Url(volume_separator_end + 1);
                    }
                }
            }
            return EncodedRootLength::Url(authority_end + 1); // URL: "file://server/", "http://server/"
        }
        return EncodedRootLength::Url(ln); // URL: "file://server", "http://server"
    }

    // relative
    EncodedRootLength::Relative
}

pub fn get_root_length(path: &str) -> usize {
    get_encoded_root_length(path).len()
}

fn last_index_byte(s: &str, b: u8) -> Option<usize> {
    memchr::memrchr(b, s.as_bytes())
}

pub fn get_directory_path(path: &str) -> String {
    get_directory_path_cow(path).into_owned()
}

/// `get_directory_path` borrowing from `path` when it has no backslash (the common case): the directory is a
/// prefix of the path.
pub fn get_directory_path_cow(path: &str) -> std::borrow::Cow<'_, str> {
    let path = normalize_slashes_cow(path);

    // If the path provided is itself a root, then return it.
    let root_length = get_root_length(&path);
    if root_length == path.len() {
        return path;
    }

    // return the leading portion of the path up to the last (non-terminal) directory separator
    // but not including any trailing directory separator.
    let directory_end = {
        let trimmed = remove_trailing_directory_separator(&path);
        last_index_byte(trimmed, b'/').map_or(root_length, |index| root_length.max(index))
    };
    match path {
        std::borrow::Cow::Borrowed(p) => std::borrow::Cow::Borrowed(&p[..directory_end]),
        std::borrow::Cow::Owned(mut p) => {
            p.truncate(directory_end);
            std::borrow::Cow::Owned(p)
        }
    }
}

impl Path {
    #[must_use]
    pub fn get_directory_path(&self) -> Path {
        Path::new(get_directory_path(&self.0))
    }
}

pub fn get_path_from_path_components<S: AsRef<str>>(path_components: &[S]) -> String {
    if path_components.is_empty() {
        return String::new();
    }

    let root = path_components[0].as_ref();
    let mut result =
        if root.is_empty() { String::new() } else { ensure_trailing_directory_separator(root) };
    result = path_components[1..].iter().enumerate().fold(result, |mut result, (i, c)| {
        if i > 0 {
            result.push('/');
        }
        result.push_str(c.as_ref());

        result
    });
    result
}

pub fn normalize_slashes(path: &str) -> String {
    if !path.as_bytes().contains(&b'\\') {
        return path.to_string();
    }
    path.replace('\\', "/")
}

/// `normalize_slashes` without a copy when the path has no backslash (every path on POSIX).
fn normalize_slashes_cow(path: &str) -> std::borrow::Cow<'_, str> {
    if !path.as_bytes().contains(&b'\\') {
        return std::borrow::Cow::Borrowed(path);
    }
    std::borrow::Cow::Owned(path.replace('\\', "/"))
}

/// `reduce_path_components(&path_components(path, get_root_length(path)))` as slices of `path`, which must have
/// forward slashes only (the output of `combine_paths`).
fn reduced_path_components_of_combined(path: &str) -> Vec<&str> {
    let root_length = get_root_length(path);
    let mut reduced: Vec<&str> = Vec::with_capacity(8);
    reduced.push(&path[..root_length]);
    for component in path[root_length..].split('/') {
        // An empty component is the trailing separator `path_components` drops, or a repeated separator
        // `reduce_path_components` skips; either way it goes.
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            if reduced.len() > 1 {
                if reduced[reduced.len() - 1] != ".." {
                    reduced.pop();
                    continue;
                }
            } else if !reduced[0].is_empty() {
                continue;
            }
        }
        reduced.push(component);
    }
    reduced
}

pub fn reduce_path_components(components: &[String]) -> Vec<String> {
    if components.is_empty() {
        return Vec::new();
    }
    let mut reduced = vec![components[0].clone()];
    for component in &components[1..] {
        if component.is_empty() {
            continue;
        }
        if component == "." {
            continue;
        }
        if component == ".." {
            if reduced.len() > 1 {
                if reduced[reduced.len() - 1] != ".." {
                    reduced.pop();
                    continue;
                }
            } else if !reduced[0].is_empty() {
                continue;
            }
        }
        reduced.push(component.clone());
    }
    reduced
}

// Combines and resolves paths. If a path is absolute, it replaces any previous path. Any
// `.` and `..` path components are resolved. Trailing directory separators are preserved.
//
// resolvePath("/path", "to", "file.ext") == "path/to/file.ext"
// resolvePath("/path", "to", "file.ext/") == "path/to/file.ext/"
// resolvePath("/path", "dir", "..", "to", "file.ext") == "path/to/file.ext"
pub fn resolve_path(path: &str, paths: &[&str]) -> String {
    let combined_path =
        if paths.is_empty() { normalize_slashes(path) } else { combine_paths(path, paths) };
    normalize_path(&combined_path)
}

pub fn get_normalized_path_components(path: &str, current_directory: &str) -> Vec<String> {
    let combined = combine_paths(current_directory, &[path]);
    get_normalized_path_components_from_combined(&combined)
}

fn get_normalized_path_components_from_combined(path: &str) -> Vec<String> {
    let bytes = path.as_bytes();
    let root_length = get_root_length(path);
    // Always include the root component (empty string for relative paths).
    let mut components: Vec<String> = Vec::with_capacity(8);
    components.push(path[..root_length].to_string());

    let mut i = root_length;
    while i < bytes.len() {
        // Skip directory separators (handles consecutive separators and trailing '/').
        i += (i..bytes.len()).take_while(|&index| bytes[index] == b'/').count();
        if i >= bytes.len() {
            break;
        }

        let start = i;
        i += (i..bytes.len()).take_while(|&index| bytes[index] != b'/').count();
        let component = &path[start..i];

        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            if components.len() > 1 {
                if components[components.len() - 1] != ".." {
                    components.pop();
                    continue;
                }
            } else if !components[0].is_empty() {
                // If this is an absolute path, we can't go above the root.
                continue;
            }
        }

        components.push(component.to_string());
    }

    components
}

pub fn get_normalized_absolute_path_without_root(
    file_name: &str,
    current_directory: &str,
) -> String {
    let absolute_path = get_normalized_absolute_path(file_name, current_directory);
    let root_length = get_root_length(&absolute_path);
    absolute_path[root_length..].to_string()
}

pub fn get_normalized_absolute_path(file_name: &str, current_directory: &str) -> String {
    let root_length = get_root_length(file_name);
    let file_name = if root_length == 0 && !current_directory.is_empty() {
        combine_paths(current_directory, &[file_name])
    } else {
        // CombinePaths normalizes slashes, so not necessary in other branch
        normalize_slashes(file_name)
    };
    let root_length = get_root_length(&file_name);

    if let Some(simple_normalized) = simple_normalize_path(&file_name) {
        let length = simple_normalized.len();
        if length > root_length {
            return remove_trailing_directory_separator(&simple_normalized).to_string();
        }
        if length == root_length && root_length != 0 {
            return ensure_trailing_directory_separator(&simple_normalized);
        }
        return simple_normalized.to_string();
    }

    let fb = file_name.as_bytes();
    let length = fb.len();
    let root = &file_name[..root_length];
    // `normalized` is only initialized once `fileName` is determined to be non-normalized.
    // `changed` is set at the same time.
    let mut changed = false;
    let mut normalized = String::new();
    let mut segment_start;
    let mut index = root_length;
    let mut normalized_up_to = index;
    let mut seen_non_dot_dot_segment = root_length != 0;
    let seen_non_dot_dot = |normalized: &str| -> bool {
        (normalized.len() != root_length || root_length != 0)
            && normalized != ".."
            && !normalized.ends_with("/..")
    };
    while index < length {
        // At beginning of segment
        segment_start = index;
        let mut ch = fb[index];
        while ch == b'/' {
            index += 1;
            if index < length {
                ch = fb[index];
            } else {
                break;
            }
        }
        if index > segment_start {
            // Seen superfluous separator
            if !changed {
                normalized = file_name[..root_length.max(segment_start - 1)].to_string();
                changed = true;
            }
            if index == length {
                break;
            }
            segment_start = index;
        }
        // Past any superfluous separators
        let segment_end = match memchr::memchr(b'/', &fb[index + 1..]) {
            None => length,
            Some(e) => e + index + 1,
        };
        let segment_length = segment_end - segment_start;
        if segment_length == 1 && fb[index] == b'.' {
            // "." segment (skip)
            if !changed {
                normalized = file_name[..normalized_up_to].to_string();
                changed = true;
            }
        } else if segment_length == 2 && fb[index] == b'.' && fb[index + 1] == b'.' {
            // ".." segment
            if !seen_non_dot_dot_segment {
                if changed {
                    if normalized.len() == root_length {
                        normalized.push_str("..");
                    } else {
                        normalized.push_str("/..");
                    }
                } else {
                    normalized_up_to = index + 2;
                }
            } else if !changed {
                if normalized_up_to >= 1 {
                    let last = memchr::memrchr(b'/', &fb[..normalized_up_to - 1]);
                    normalized = file_name
                        [..last.map_or(root_length, |index| root_length.max(index))]
                        .to_string();
                } else {
                    normalized = file_name[..normalized_up_to].to_string();
                }
                changed = true;
                seen_non_dot_dot_segment = seen_non_dot_dot(&normalized);
            } else {
                let last_slash = last_index_byte(&normalized, b'/');
                if let Some(last_slash) = last_slash {
                    normalized.truncate(root_length.max(last_slash));
                } else {
                    normalized = root.to_string();
                }
                seen_non_dot_dot_segment = seen_non_dot_dot(&normalized);
            }
        } else if changed {
            if normalized.len() != root_length {
                normalized.push('/');
            }
            seen_non_dot_dot_segment = true;
            normalized.push_str(&file_name[segment_start..segment_end]);
        } else {
            seen_non_dot_dot_segment = true;
            normalized_up_to = segment_end;
        }
        index = segment_end + 1;
    }
    if changed {
        return normalized;
    }
    if length > root_length {
        return remove_trailing_directory_separators(&file_name).to_string();
    }
    if length == root_length {
        return ensure_trailing_directory_separator(&file_name);
    }
    file_name
}

fn simple_normalize_path(path: &str) -> Option<std::borrow::Cow<'_, str>> {
    use std::borrow::Cow;
    // Most paths don't require normalization
    if !has_relative_path_segment(path) {
        return Some(Cow::Borrowed(path));
    }
    // Some paths only require cleanup of `/./` or leading `./`
    let simplified = path.replace("/./", "/");
    let trimmed = simplified.strip_prefix("./").unwrap_or(&simplified);
    if trimmed != path
        && !has_relative_path_segment(trimmed)
        && !(trimmed != simplified && trimmed.starts_with('/'))
    {
        // If we trimmed a leading "./" and the path now starts with "/", we changed the meaning
        return Some(Cow::Owned(trimmed.to_string()));
    }
    None
}

// hasRelativePathSegment reports whether p contains ".", "..", "./", "../", "/.", "/..", "//", "/./", or "/../".
pub fn has_relative_path_segment(p: &str) -> bool {
    // A segment that is "." or "..", or an empty segment between two slashes. Every segment but the first starts
    // after a slash, so only a slash followed by '/' or '.' can start one. Module resolution normalizes paths of
    // ~100 bytes and many short segments, so this looks for such pairs eight bytes at a time (seven slash
    // positions per word) instead of calling memchr per segment, and checks each candidate exactly.
    const ONES: u64 = u64::from_ne_bytes([0x01; 8]);
    const HIGHS: u64 = u64::from_ne_bytes([0x80; 8]);
    // The high bit of each zero byte of `x`, and possibly of a 0x01 byte just above one (checked exactly below).
    let zero_bytes = |x: u64| x.wrapping_sub(ONES) & !x & HIGHS;
    let p = p.as_bytes();
    let n = p.len();
    if p.first() == Some(&b'.') && dot_segment_at(p, 0) {
        return true;
    }
    let mut i = 0;
    while i + 8 <= n {
        let word = u64::from_le_bytes(p[i..i + 8].try_into().unwrap());
        let slashes = zero_bytes(word ^ u64::from_ne_bytes([b'/'; 8]));
        let dots = zero_bytes(word ^ u64::from_ne_bytes([b'.'; 8]));
        // Byte k: a slash at i + k and a slash or dot at i + k + 1 (k < 7).
        let mut pairs = slashes & ((slashes | dots) >> 8);
        while pairs != 0 {
            if relative_segment_after(p, i + (pairs.trailing_zeros() / 8) as usize) {
                return true;
            }
            pairs &= pairs - 1;
        }
        i += 7;
    }
    (i..n.saturating_sub(1)).any(|i| relative_segment_after(p, i))
}

// Whether p[slash] is a slash that ends an empty segment or starts a "." or ".." segment (`has_relative_path_segment`).
#[inline]
fn relative_segment_after(p: &[u8], slash: usize) -> bool {
    p[slash] == b'/'
        && p.get(slash + 1).is_some_and(|&c| c == b'/' || c == b'.' && dot_segment_at(p, slash + 1))
}

// Whether the segment at p[start] (a dot) is "." or "..".
#[inline]
fn dot_segment_at(p: &[u8], start: usize) -> bool {
    match p.get(start + 1) {
        None | Some(b'/') => true,
        Some(b'.') => matches!(p.get(start + 2), None | Some(b'/')),
        Some(_) => false,
    }
}

/// `normalize_path` borrowing from `path` when it is already normal (no backslash, no `.` or `..` segment, no
/// repeated separator): a file name from the file system usually is.
pub fn normalize_path_cow(path: &str) -> std::borrow::Cow<'_, str> {
    if !path.as_bytes().contains(&b'\\')
        && let Some(normalized) = simple_normalize_path(path)
    {
        return normalized;
    }
    std::borrow::Cow::Owned(normalize_path(path))
}

pub fn normalize_path(path: &str) -> String {
    // normalize_slashes without copying a path that has no backslash (the common case; the result is copied once)
    let slashed;
    let path: &str = if path.as_bytes().contains(&b'\\') {
        slashed = path.replace('\\', "/");
        &slashed
    } else {
        path
    };
    if let Some(normalized) = simple_normalize_path(path) {
        return normalized.into_owned();
    }
    let mut normalized = get_normalized_absolute_path(path, "");
    if !normalized.is_empty() && has_trailing_directory_separator(path) {
        normalized = ensure_trailing_directory_separator(&normalized);
    }
    normalized
}

pub fn get_canonical_file_name(file_name: &str, use_case_sensitive_file_names: bool) -> String {
    if use_case_sensitive_file_names {
        return file_name.to_string();
    }
    to_file_name_lower_case(file_name)
}

// TrimFilePathPrefix removes prefix from the start of path, honoring
// useCaseSensitiveFileNames the same way GetCanonicalFileName does. It returns
// the remainder of path if path starts with prefix.
//
// This must not slice path using len(prefix): case-folding (as performed by
// GetCanonicalFileName) can change a string's UTF-8 byte length without
// changing its rune count (e.g. the Kelvin sign '\u212A' case-folds to the
// single-byte 'k'), so path and prefix can disagree in byte length even when
// one is (a case-insensitive match for) a prefix of the other.
pub fn trim_file_path_prefix<'a>(
    path: &'a str,
    prefix: &str,
    use_case_sensitive_file_names: bool,
) -> Option<&'a str> {
    if use_case_sensitive_file_names {
        return path.strip_prefix(prefix);
    }
    let canonical_prefix =
        get_canonical_file_name(prefix, false /*useCaseSensitiveFileNames*/);
    if !get_canonical_file_name(path, false /*useCaseSensitiveFileNames*/)
        .starts_with(&canonical_prefix)
    {
        return None;
    }
    Some(trim_rune_count(path, rune_count_in_string(&canonical_prefix)))
}

fn rune_count_in_string(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    let mut n = 0;
    while i < b.len() {
        let (_, size) = decode_rune(&b[i..]);
        i += size;
        n += 1;
    }
    n
}

// trimRuneCount returns the suffix of s after skipping up to runeCount runes,
// clamping to the end of s if it has fewer runes than runeCount.
fn trim_rune_count(s: &str, rune_count: usize) -> &str {
    let b = s.as_bytes();
    let mut i = 0;
    for _ in 0..rune_count {
        if i >= b.len() {
            break;
        }
        let (_, size) = decode_rune(&b[i..]);
        i += size;
    }
    &s[i..]
}

// We convert the file names to lower case as key for file name on case insensitive file system
// While doing so we need to handle special characters (eg \u0130) to ensure that we dont convert
// it to lower case, fileName with its lowercase form can exist along side it.
// Handle special characters and make those case sensitive instead
//
// |-#--|-Unicode--|-Char code-|-Desc-------------------------------------------------------------------|
// | 1. | i        | 105       | Ascii i                                                                |
// | 2. | I        | 73        | Ascii I                                                                |
// |-------- Special characters ------------------------------------------------------------------------|
// | 3. | \u0130   | 304       | Upper case I with dot above                                            |
// | 4. | i,\u0307 | 105,775   | i, followed by 775: Lower case of (3rd item)                           |
// | 5. | I,\u0307 | 73,775    | I, followed by 775: Upper case of (4th item), lower case is (4th item) |
// | 6. | \u0131   | 305       | Lower case i without dot, upper case is I (2nd item)                   |
// | 7. | \u00DF   | 223       | Lower case sharp s                                                     |
//
// Because item 3 is special where in its lowercase character has its own
// upper case form we cant convert its case.
// Rest special characters are either already in lower case format or
// they have corresponding upper case character so they dont need special handling
pub fn to_file_name_lower_case(file_name: &str) -> String {
    const I_WITH_DOT: stringutil::Rune = 0x130;

    if file_name.is_ascii() {
        return file_name.to_ascii_lowercase();
    }

    let b = file_name.as_bytes();
    let mut out = String::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let (r, size) = decode_rune(&b[i..]);
        i += size;
        if r == I_WITH_DOT {
            push_rune(&mut out, r);
        } else {
            push_rune(&mut out, unicode_to_lower(r));
        }
    }
    out
}

pub fn to_path(file_name: &str, base_path: &str, use_case_sensitive_file_names: bool) -> Path {
    // The common case on Linux: an absolute, already normal file name becomes a `Path` with one allocation (the
    // `Arc`), not a normalized `String` copied into one.
    Path::from(&*to_path_cow(file_name, base_path, use_case_sensitive_file_names))
}

/// The text of `to_path`, borrowed from `file_name` when it is the key as it is (absolute and already normal on a
/// case-sensitive host): a cache lookup by `Path` can hash and compare this without making a `Path`.
pub fn to_path_cow<'a>(
    file_name: &'a str,
    base_path: &str,
    use_case_sensitive_file_names: bool,
) -> std::borrow::Cow<'a, str> {
    use std::borrow::Cow;
    if use_case_sensitive_file_names && is_rooted_disk_path(file_name) {
        return normalize_path_cow(file_name);
    }
    let mut non_canonicalized_path = if is_rooted_disk_path(file_name) {
        normalize_path(file_name)
    } else {
        get_normalized_absolute_path(file_name, base_path)
    };
    if super::is_encoded_dynamic_file_name(&non_canonicalized_path) {
        return Cow::Owned(super::canonical_dynamic_uri_path(&non_canonicalized_path));
    }
    // get_canonical_file_name, reusing the owned string
    if use_case_sensitive_file_names {
        return Cow::Owned(non_canonicalized_path);
    }
    if non_canonicalized_path.is_ascii() {
        non_canonicalized_path.make_ascii_lowercase();
        return Cow::Owned(non_canonicalized_path);
    }
    Cow::Owned(to_file_name_lower_case(&non_canonicalized_path))
}

pub fn remove_trailing_directory_separator(path: &str) -> &str {
    if has_trailing_directory_separator(path) {
        return &path[..path.len() - 1];
    }
    path
}

impl Path {
    #[must_use]
    pub fn remove_trailing_directory_separator(&self) -> Path {
        Path::from(remove_trailing_directory_separator(&self.0))
    }
}

fn remove_trailing_directory_separators(path: &str) -> &str {
    let mut path = path;
    while has_trailing_directory_separator(path) {
        path = remove_trailing_directory_separator(path);
    }
    path
}

pub fn ensure_trailing_directory_separator(path: &str) -> String {
    if !has_trailing_directory_separator(path) {
        let mut s = String::with_capacity(path.len() + 1);
        s.push_str(path);
        s.push('/');
        return s;
    }

    path.to_string()
}

impl Path {
    #[must_use]
    pub fn ensure_trailing_directory_separator(&self) -> Path {
        Path::new(ensure_trailing_directory_separator(&self.0))
    }
}

// Relative Paths

pub fn get_path_components_relative_to(
    from: &str,
    to: &str,
    options: &ComparePathsOptions,
) -> Vec<String> {
    let from_components =
        reduce_path_components(&get_path_components(from, &options.current_directory));
    let to_components =
        reduce_path_components(&get_path_components(to, &options.current_directory));

    let mut start = 0;
    let max_common_components = from_components.len().min(to_components.len());
    let string_equaler = options.get_equality_comparer();
    while start < max_common_components {
        let from_component = &from_components[start];
        let to_component = &to_components[start];
        if start == 0 {
            if !stringutil::equate_string_case_insensitive(from_component, to_component) {
                break;
            }
        } else if !string_equaler(from_component, to_component) {
            break;
        }
        start += 1;
    }

    if start == 0 {
        return to_components;
    }

    let num_dot_dot_slashes = from_components.len() - start;
    let mut result = Vec::with_capacity(1 + num_dot_dot_slashes + to_components.len() - start);

    result.push(String::new());
    // Add all the relative components until we hit a common directory.
    result.extend(std::iter::repeat_with(|| "..".to_string()).take(num_dot_dot_slashes));
    // Now add all the remaining components of the "to" path.
    result.extend(to_components[start..].iter().cloned());

    result
}

pub fn get_relative_path_from_directory(
    from_directory: &str,
    to: &str,
    options: &ComparePathsOptions,
) -> String {
    assert!(
        (get_root_length(from_directory) > 0) == (get_root_length(to) > 0),
        "paths must either both be absolute or both be relative"
    );
    let path_components = get_path_components_relative_to(from_directory, to, options);
    get_path_from_path_components(&path_components)
}

pub fn get_relative_path_from_file(from: &str, to: &str, options: &ComparePathsOptions) -> String {
    ensure_path_is_non_module_name(&get_relative_path_from_directory(
        &get_directory_path(from),
        to,
        options,
    ))
}

pub fn convert_to_relative_path(
    absolute_or_relative_path: &str,
    options: &ComparePathsOptions,
) -> String {
    if !is_rooted_disk_path(absolute_or_relative_path) {
        return absolute_or_relative_path.to_string();
    }

    get_relative_path_to_directory_or_url(
        &options.current_directory,
        absolute_or_relative_path,
        false, /*isAbsolutePathAnUrl*/
        options,
    )
}

pub fn get_relative_path_to_directory_or_url(
    directory_path_or_url: &str,
    relative_or_absolute_path: &str,
    is_absolute_path_an_url: bool,
    options: &ComparePathsOptions,
) -> String {
    let mut path_components =
        get_path_components_relative_to(directory_path_or_url, relative_or_absolute_path, options);

    let first_component = path_components[0].clone();
    if is_absolute_path_an_url && is_rooted_disk_path(&first_component) {
        let prefix = if first_component.as_bytes()[0] == DIRECTORY_SEPARATOR {
            "file://"
        } else {
            "file:///"
        };
        path_components[0] = format!("{prefix}{first_component}");
    }

    get_path_from_path_components(&path_components)
}

// Gets the portion of a path following the last (non-terminal) separator (`/`).
// Semantics align with NodeJS's `path.basename` except that we support URL's as well.
//
//	// POSIX
//	GetBaseFileName("/path/to/file.ext") == "file.ext"
//	GetBaseFileName("/path/to/") == "to"
//	GetBaseFileName("/") == ""
//	// DOS
//	GetBaseFileName("c:/path/to/file.ext") == "file.ext"
//	GetBaseFileName("c:/path/to/") == "to"
//	GetBaseFileName("c:/") == ""
//	GetBaseFileName("c:") == ""
//	// URL
//	GetBaseFileName("http://typescriptlang.org/path/to/file.ext") == "file.ext"
//	GetBaseFileName("http://typescriptlang.org/path/to/") == "to"
//	GetBaseFileName("http://typescriptlang.org/") == ""
//	GetBaseFileName("http://typescriptlang.org") == ""
pub fn get_base_file_name(path: &str) -> String {
    get_base_file_name_cow(path).into_owned()
}

/// `get_base_file_name` borrowing from `path` when it has no backslash (the common case): the base name is a
/// slice of the path.
pub fn get_base_file_name_cow(path: &str) -> std::borrow::Cow<'_, str> {
    let path = normalize_slashes_cow(path);

    // if the path provided is itself the root, then it has no file name.
    let root_length = get_root_length(&path);
    if root_length == path.len() {
        return std::borrow::Cow::Borrowed("");
    }

    // return the trailing portion of the path starting after the last (non-terminal) directory
    // separator but not including any trailing directory separator.
    let (base_start, base_end) = {
        let trimmed = remove_trailing_directory_separator(&path);
        let root_length = get_root_length(trimmed);
        let base_start = last_index_byte(trimmed, DIRECTORY_SEPARATOR)
            .map_or(root_length, |index| root_length.max(index + 1));
        (base_start, trimmed.len())
    };
    match path {
        std::borrow::Cow::Borrowed(p) => std::borrow::Cow::Borrowed(&p[base_start..base_end]),
        std::borrow::Cow::Owned(p) => std::borrow::Cow::Owned(p[base_start..base_end].to_string()),
    }
}

// Gets the file extension for a path.
// If extensions are provided, gets the file extension for a path, provided it is one of the provided extensions.
//
//	GetAnyExtensionFromPath("/path/to/file.ext", nil, false) == ".ext"
//	GetAnyExtensionFromPath("/path/to/file.ext/", nil, false) == ".ext"
//	GetAnyExtensionFromPath("/path/to/file", nil, false) == ""
//	GetAnyExtensionFromPath("/path/to.ext/file", nil, false) == ""
//	GetAnyExtensionFromPath("/path/to/file.ext", ".ext", true) === ".ext"
//	GetAnyExtensionFromPath("/path/to/file.js", ".ext", true) === ""
//	GetAnyExtensionFromPath("/path/to/file.js", [".ext", ".js"], true) === ".js"
//	GetAnyExtensionFromPath("/path/to/file.ext", ".EXT", false) === ""
pub fn get_any_extension_from_path(path: &str, extensions: &[&str], ignore_case: bool) -> String {
    // Retrieves any string from the final "." onwards from a base file name.
    // Unlike extensionFromPath, which throws an exception on unrecognized extensions.
    if !extensions.is_empty() {
        return get_any_extension_from_path_worker(
            remove_trailing_directory_separator(path),
            extensions,
            stringutil::get_string_equality_comparer(ignore_case),
        );
    }

    let base_file_name = get_base_file_name(path);
    if let Some(extension_index) = base_file_name.rfind('.') {
        return base_file_name[extension_index..].to_string();
    }
    String::new()
}

pub fn get_longest_extension_from_path(
    path: &str,
    extensions: &[&str],
    ignore_case: bool,
) -> String {
    let path = remove_trailing_directory_separator(path);
    let comparer = stringutil::get_string_equality_comparer(ignore_case);
    let mut longest = String::new();
    for extension in extensions {
        if extension.len() > longest.len() {
            let matched = try_get_extension_from_path_with(path, extension, comparer);
            if !matched.is_empty() {
                longest = matched;
            }
        }
    }
    longest
}

fn get_any_extension_from_path_worker(
    path: &str,
    extensions: &[&str],
    string_equality_comparer: fn(&str, &str) -> bool,
) -> String {
    if let Some(result) = extensions.iter().find_map(|extension| {
        let result = try_get_extension_from_path_with(path, extension, string_equality_comparer);
        if !result.is_empty() {
            return Some(result);
        }

        None
    }) {
        return result;
    }
    String::new()
}

// Go's unexported tryGetExtensionFromPath; renamed because it collides with TryGetExtensionFromPath after snake-casing.
fn try_get_extension_from_path_with(
    path: &str,
    extension: &str,
    string_equality_comparer: fn(&str, &str) -> bool,
) -> String {
    // Go prefixes a missing "." to the extension; both comparers match the "." alike, so the rest is compared
    // without building the prefixed string (this runs once per candidate extension of every changeExtension).
    let without_dot = extension.strip_prefix('.').unwrap_or(extension);
    let len = without_dot.len() + 1;
    let pb = path.as_bytes();
    if pb.len() >= len && pb[pb.len() - len] == b'.' {
        let path_extension = &path[path.len() - len..];
        if string_equality_comparer(&path_extension[1..], without_dot) {
            return path_extension.to_string();
        }
    }
    String::new()
}

pub fn path_is_relative(path: &str) -> bool {
    // True if path is ".", "..", or starts with "./", "../", ".\\", or "..\\".
    let p = path.as_bytes();

    if p == b"." || p == b".." {
        return true;
    }

    if p.len() >= 2 && p[0] == b'.' && (p[1] == b'/' || p[1] == b'\\') {
        return true;
    }

    if p.len() >= 3 && p[0] == b'.' && p[1] == b'.' && (p[2] == b'/' || p[2] == b'\\') {
        return true;
    }

    false
}

// EnsurePathIsNonModuleName ensures a path is either absolute (prefixed with `/` or `c:`) or dot-relative (prefixed
// with `./` or `../`) so as not to be confused with an unprefixed module name.
pub fn ensure_path_is_non_module_name(path: &str) -> String {
    if !path_is_absolute(path) && !path_is_relative(path) {
        return format!("./{path}");
    }
    path.to_string()
}

pub fn is_external_module_name_relative(module_name: &str) -> bool {
    // TypeScript 1.0 spec (April 2014): 11.2.1
    // An external module name is "relative" if the first term is "." or "..".
    // Update: We also consider a path like `C:\foo.ts` "relative" because we do not search for it in `node_modules` or treat it as an ambient module.
    path_is_relative(module_name) || is_rooted_disk_path(module_name)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ComparePathsOptions {
    pub use_case_sensitive_file_names: bool,
    pub current_directory: String,
}

impl ComparePathsOptions {
    pub fn get_comparer(&self) -> fn(&str, &str) -> Ordering {
        stringutil::get_string_comparer(!self.use_case_sensitive_file_names)
    }

    fn get_equality_comparer(&self) -> fn(&str, &str) -> bool {
        stringutil::get_string_equality_comparer(!self.use_case_sensitive_file_names)
    }
}

pub fn compare_paths(a: &str, b: &str, options: &ComparePathsOptions) -> Ordering {
    let a = combine_paths(&options.current_directory, &[a]);
    let b = combine_paths(&options.current_directory, &[b]);

    if a == b {
        return Ordering::Equal;
    }
    if a.is_empty() {
        return Ordering::Less;
    }
    if b.is_empty() {
        return Ordering::Greater;
    }

    // NOTE: Performance optimization - shortcut if the root segments differ as there would be no
    //       need to perform path reduction.
    let a_root = &a[..get_root_length(&a)];
    let b_root = &b[..get_root_length(&b)];
    let result = stringutil::compare_strings_case_insensitive(a_root, b_root);
    if result != Ordering::Equal {
        return result;
    }

    // NOTE: Performance optimization - shortcut if there are no relative path segments in
    //       the non-root portion of the path
    let a_rest = &a[a_root.len()..];
    let b_rest = &b[b_root.len()..];
    if !has_relative_path_segment(a_rest) && !has_relative_path_segment(b_rest) {
        return options.get_comparer()(a_rest, b_rest);
    }

    // The path contains a relative path segment. Normalize the paths and perform a slower component
    // by component comparison.
    let a_components = reduce_path_components(&get_path_components(&a, ""));
    let b_components = reduce_path_components(&get_path_components(&b, ""));
    let shared_length = a_components.len().min(b_components.len());
    if let Some(result) = (1..shared_length)
        .map(|i| options.get_comparer()(&a_components[i], &b_components[i]))
        .find(|&result| result != Ordering::Equal)
    {
        return result;
    }
    a_components.len().cmp(&b_components.len())
}

pub fn contains_path(parent: &str, child: &str, options: &ComparePathsOptions) -> bool {
    let parent = combine_paths(&options.current_directory, &[parent]);
    let child = combine_paths(&options.current_directory, &[child]);
    if parent.is_empty() || child.is_empty() {
        return false;
    }
    if parent == child {
        return true;
    }
    // `reduce_path_components(&get_path_components(path, ""))` over slices of the combined (slash-normalized)
    // paths: one allocation per path instead of one per component (a program checks every file against its root).
    let parent_components = reduced_path_components_of_combined(&parent);
    let child_components = reduced_path_components_of_combined(&child);
    if child_components.len() < parent_components.len() {
        return false;
    }

    let component_comparer = options.get_equality_comparer();
    if parent_components.iter().enumerate().any(|(i, parent_component)| {
        let comparer: fn(&str, &str) -> bool =
            if i == 0 { stringutil::equate_string_case_insensitive } else { component_comparer };
        !comparer(parent_component, child_components[i])
    }) {
        return false;
    }

    true
}

impl Path {
    // ContainsPath checks whether child is contained within or equal to p.
    // Since Path values are already rooted, reduced, and case-canonicalized,
    // this is a simple string prefix check.
    pub fn contains_path(&self, child: &Path) -> bool {
        let p = self.0.as_bytes();
        let c = child.0.as_bytes();
        if p.is_empty() {
            return false;
        }
        p == c
            || c.len() > p.len()
                && c.starts_with(p)
                && (p[p.len() - 1] == b'/' || c[p.len()] == b'/')
    }
}

pub fn file_extension_is(path: &str, extension: &str) -> bool {
    path.len() > extension.len() && path.ends_with(extension)
}

// Calls `callback` on `directory` and every ancestor directory it has, returning the first defined result.
// Stops at global cache location
pub fn for_each_ancestor_directory_stopping_at_global_cache<T: Default>(
    global_cache_location: &str,
    directory: &str,
    mut callback: impl FnMut(&str) -> Option<T>,
) -> T {
    for_each_ancestor_directory(directory, |ancestor_directory| {
        let result = callback(ancestor_directory);
        if result.is_some() {
            return result;
        }
        if ancestor_directory == global_cache_location {
            return Some(T::default());
        }
        None
    })
    .unwrap_or_default()
}

/// Go callback `(result T, stop bool)` is `Option<T>` here: `Some(result)` stops the walk.
pub fn for_each_ancestor_directory<T>(
    directory: &str,
    mut callback: impl FnMut(&str) -> Option<T>,
) -> Option<T> {
    // Every ancestor of a slash-normalized path is a prefix of it, so the walk borrows (a resolution walks the
    // ancestors of its containing directory on every node_modules lookup).
    let slashed = normalize_slashes_cow(directory);
    let mut directory: &str = &slashed;
    loop {
        if let Some(result) = callback(directory) {
            return Some(result);
        }

        let parent_path = match get_directory_path_cow(directory) {
            std::borrow::Cow::Borrowed(parent) => parent,
            // Unreachable: `directory` has no backslash.
            std::borrow::Cow::Owned(_) => return None,
        };
        if parent_path == directory {
            return None;
        }

        directory = parent_path;
    }
}

impl Path {
    pub fn for_each_ancestor_directory<T>(
        &self,
        mut callback: impl FnMut(&Path) -> Option<T>,
    ) -> Option<T> {
        for_each_ancestor_directory(&self.0, |directory| callback(&Path::from(directory)))
    }
}

pub fn has_extension(file_name: &str) -> bool {
    get_base_file_name(file_name).contains('.')
}

/// Go `(volume, rest, ok)` -> `Some((volume, rest))`; on failure the rest is the whole path.
pub fn split_volume_path(path: &str) -> Option<(String, &str)> {
    let b = path.as_bytes();
    if b.len() >= 2 && is_volume_character(b[0]) && b[1] == b':' {
        return Some((path[0..2].to_ascii_lowercase(), &path[2..]));
    }
    None
}

// GetCommonParents returns the smallest set of directories that are parents of all paths with
// at least `minComponents` directory components. Any path that has fewer than `minComponents` directory components
// will be returned in the second return value. Examples:
//
//	/a/b/c/d, /a/b/c/e, /a/b/f/g  =>  /a/b
//	/a/b/c/d, /a/b/c/e, /a/b/f/g, /x/y  =>  /
//	/a/b/c/d, /a/b/c/e, /a/b/f/g, /x/y  (minComponents: 2)	=>  /a/b, /x/y
//	c:/a/b/c/d, d:/a/b/c/d =>	c:/a/b/c/d, d:/a/b/c/d
pub fn get_common_parents(
    paths: &[String],
    min_components: usize,
    get_path_components: fn(&str, &str) -> Vec<String>,
    options: &ComparePathsOptions,
) -> (Vec<String>, FxHashSet<String>) {
    assert!(min_components >= 1, "minComponents must be at least 1");
    if paths.is_empty() {
        return (Vec::new(), FxHashSet::default());
    }
    if paths.len() == 1 {
        if reduce_path_components(&get_path_components(&paths[0], &options.current_directory)).len()
            < min_components
        {
            let mut ignored = FxHashSet::default();
            ignored.insert(paths[0].clone());
            return (Vec::new(), ignored);
        }
        return (paths.to_vec(), FxHashSet::default());
    }

    let mut ignored = FxHashSet::default();
    let mut path_components: Vec<Vec<String>> = Vec::with_capacity(paths.len());
    for path in paths {
        let components =
            reduce_path_components(&get_path_components(path, &options.current_directory));
        if components.len() < min_components {
            ignored.insert(path.clone());
        } else {
            path_components.push(components);
        }
    }

    let results = get_common_parents_worker(&path_components, min_components, options);
    let result_paths = results.iter().map(|comps| get_path_from_path_components(comps)).collect();

    (result_paths, ignored)
}

fn get_common_parents_worker(
    component_groups: &[Vec<String>],
    min_components: usize,
    options: &ComparePathsOptions,
) -> Vec<Vec<String>> {
    if component_groups.is_empty() {
        return Vec::new();
    }
    // Determine the maximum depth we can consider
    let mut max_depth = component_groups[0].len();
    max_depth = component_groups[1..].iter().fold(max_depth, |mut max_depth, comps| {
        if comps.len() < max_depth {
            max_depth = comps.len();
        }

        max_depth
    });

    let equality = options.get_equality_comparer();
    for last_common_index in 0..max_depth {
        let candidate = &component_groups[0][last_common_index];
        for comps in &component_groups[1..] {
            if !equality(candidate, &comps[last_common_index]) {
                // divergence
                if last_common_index < min_components {
                    // Not enough components, we need to fan out
                    let mut ordered_groups: Vec<Path> = Vec::new();
                    let mut new_groups: FxHashMap<Path, (Vec<String>, Vec<Vec<String>>)> =
                        FxHashMap::default();
                    for g in component_groups {
                        let key = Path::from(get_canonical_file_name(
                            &g[last_common_index],
                            options.use_case_sensitive_file_names,
                        ));
                        let entry = match new_groups.entry(key) {
                            Entry::Occupied(entry) => entry.into_mut(),
                            Entry::Vacant(entry) => {
                                ordered_groups.push(entry.key().clone());
                                entry.insert(Default::default())
                            }
                        };
                        entry.0 = g[..=last_common_index].to_vec();
                        entry.1.push(g[last_common_index + 1..].to_vec());
                    }
                    ordered_groups.sort();
                    let mut result = Vec::with_capacity(new_groups.len());
                    result.extend(ordered_groups.iter().flat_map(|key| {
                        let group = &new_groups[key];
                        get_common_parents_worker(
                            &group.1,
                            min_components - (last_common_index + 1),
                            options,
                        )
                        .into_iter()
                        .map(move |sr| {
                            let mut v = group.0.clone();
                            v.extend(sr);
                            v
                        })
                    }));
                    return result;
                }
                return vec![component_groups[0][..last_common_index].to_vec()];
            }
        }
    }

    vec![component_groups[0][..max_depth].to_vec()]
}

pub fn starts_with_directory(
    file_name: &str,
    directory_name: &str,
    use_case_sensitive_file_names: bool,
) -> bool {
    if directory_name.is_empty() {
        return false;
    }

    let canonical_file_name = get_canonical_file_name(file_name, use_case_sensitive_file_names);
    let canonical_directory_name =
        get_canonical_file_name(directory_name, use_case_sensitive_file_names);
    let canonical_directory_name =
        canonical_directory_name.strip_suffix('/').unwrap_or(&canonical_directory_name);
    let canonical_directory_name =
        canonical_directory_name.strip_suffix('\\').unwrap_or(canonical_directory_name);

    canonical_file_name.starts_with(&format!("{canonical_directory_name}/"))
        || canonical_file_name.starts_with(&format!("{canonical_directory_name}\\"))
}

pub fn compare_number_of_directory_separators(path1: &str, path2: &str) -> Ordering {
    memchr::memchr_iter(b'/', path1.as_bytes())
        .count()
        .cmp(&memchr::memchr_iter(b'/', path2.as_bytes()).count())
}
