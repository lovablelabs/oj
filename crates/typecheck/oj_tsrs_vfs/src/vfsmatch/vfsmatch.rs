use rustc_hash::{FxHashMap, FxHashSet};
use tsrs_core::{stringutil, tspath};

use crate::{Entries, FS};

// This file implements the glob matching algorithm specified in MATCHING_ALGORITHM.md.

#[repr(i8)]
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
pub enum Usage {
    Files,
    Directories,
    Exclude,
}

// UnlimitedDepth can be passed as the depth argument to indicate there is no depth limit.
pub const UNLIMITED_DEPTH: usize = usize::MAX;

pub fn read_directory(
    host: &dyn FS,
    current_dir: &str,
    path: &str,
    extensions: &[impl AsRef<str>],
    excludes: &[impl AsRef<str>],
    includes: &[impl AsRef<str>],
    depth: usize,
) -> Vec<String> {
    match_files(
        path,
        extensions,
        excludes,
        includes,
        host.use_case_sensitive_file_names(),
        current_dir,
        depth,
        host,
    )
}

// IsImplicitGlob checks if a path component is implicitly a glob.
// An "includes" path "foo" is implicitly a glob "foo/** /*" (without the space) if its last component has no extension,
// and does not contain any glob characters itself.
pub fn is_implicit_glob(last_path_component: &str) -> bool {
    !last_path_component.contains(['.', '*', '?'])
}

fn get_include_base_path(absolute: &str) -> String {
    let Some(wildcard_offset) = absolute.find(['*', '?']) else {
        // No "*" or "?" in the path
        if !tspath::has_extension(absolute) {
            return absolute.to_string();
        }
        return tspath::remove_trailing_directory_separator(&tspath::get_directory_path(absolute))
            .to_string();
    };
    let end = absolute[..wildcard_offset].rfind(tspath::DIRECTORY_SEPARATOR as char).unwrap_or(0);
    absolute[..end].to_string()
}

// getBasePaths computes the unique non-wildcard base paths amongst the provided include patterns.
pub fn get_base_paths(
    path: &str,
    includes: &[impl AsRef<str>],
    use_case_sensitive_file_names: bool,
) -> Vec<String> {
    // Storage for our results in the form of literal paths (e.g. the paths as written by the user).
    let mut base_paths = vec![path.to_string()];

    if !includes.is_empty() {
        let compare_paths_options = tspath::ComparePathsOptions {
            current_directory: path.to_string(),
            use_case_sensitive_file_names,
        };
        let string_comparer = compare_paths_options.get_comparer();

        // Storage for literal base paths amongst the include patterns.
        let mut include_base_paths: Vec<String> = Vec::new();
        include_base_paths.extend(includes.iter().map(|include| {
            let include = include.as_ref();
            // We also need to check the relative paths by converting them to absolute and normalizing
            // in case they escape the base path (e.g "..\somedirectory")
            let absolute = if tspath::is_rooted_disk_path(include) {
                include.to_string()
            } else {
                tspath::normalize_path(&tspath::combine_paths(path, &[include]))
            };
            // Append the literal and canonical candidate base paths.
            get_include_base_path(&absolute)
        }));

        // Sort the offsets array using either the literal or canonical path representations.
        include_base_paths.sort_by(|a, b| string_comparer(a, b));

        // Iterate over each include base path and include unique base paths that are not a
        // subpath of an existing base path
        for include_base_path in include_base_paths {
            if base_paths.iter().all(|basepath| {
                !tspath::contains_path(basepath, &include_base_path, &compare_paths_options)
            }) {
                base_paths.push(include_base_path);
            }
        }
    }

    base_paths
}

// globPattern is a compiled glob pattern for matching file paths without regex.
#[derive(Clone, Debug)]
pub struct GlobPattern {
    components: Vec<Component>, // path segments to match (e.g., ["src", "**", "*.ts"])
    is_exclude: bool,           // exclude patterns have different matching rules
    case_sensitive: bool,
    exclude_min_js: bool, // for "files" patterns, exclude .min.js by default
}

// component is a single path segment in a glob pattern.
// Examples: "src" (literal), "*" (wildcard), "*.ts" (wildcard), "**" (recursive)
#[derive(Clone, Debug)]
struct Component {
    kind: ComponentKind,
    literal: String,        // for kindLiteral: the exact string to match
    segments: Vec<Segment>, // for kindWildcard: parsed wildcard pattern
    // Include patterns with wildcards skip common package folders (node_modules, etc.)
    skip_package_folders: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ComponentKind {
    Literal,        // exact match (e.g., "src")
    Wildcard,       // contains * or ? (e.g., "*.ts")
    DoubleAsterisk, // ** matches zero or more directories
}

// segment is a piece of a wildcard component.
// Example: "*.ts" becomes [segStar, segLiteral(".ts")]
#[derive(Clone, Debug, PartialEq, Eq)]
struct Segment {
    kind: SegmentKind,
    literal: String, // only for segLiteral
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum SegmentKind {
    Literal,  // exact text
    Star,     // * matches any chars except /
    Question, // ? matches single char except /
}

// compileGlobPattern compiles a glob spec (e.g., "src/**/*.ts") into a pattern.
// Returns None if the pattern would match nothing.
pub fn compile_glob_pattern(
    spec: &str,
    base_path: &str,
    usage: Usage,
    case_sensitive: bool,
) -> Option<GlobPattern> {
    let mut parts = tspath::get_normalized_path_components(spec, base_path);
    let case_sensitive = case_sensitive
        || parts.first().is_some_and(|root| tspath::is_encoded_dynamic_file_name(root));

    // "src/**" without a filename matches nothing (for include patterns)
    if usage != Usage::Exclude && parts.last().map_or("", std::string::String::as_str) == "**" {
        return None;
    }

    // Normalize root: "/home/" -> "/home"
    parts[0] = tspath::remove_trailing_directory_separator(&parts[0]).to_string();
    if tspath::is_encoded_dynamic_file_name(&parts[0]) {
        let root_parts = parts[0].split('/').map(str::to_string).collect::<Vec<_>>();
        parts.splice(0..1, root_parts);
    }

    // Directories implicitly match all files: "src" -> "src/**/*"
    if is_implicit_glob(parts.last().map_or("", std::string::String::as_str)) {
        parts.push("**".to_string());
        parts.push("*".to_string());
    }

    let mut p = GlobPattern {
        is_exclude: usage == Usage::Exclude,
        case_sensitive,
        exclude_min_js: usage == Usage::Files,
        // Avoid slice growth during compilation.
        components: Vec::with_capacity(parts.len()),
    };

    p.components.extend(parts.iter().map(|part| parse_component(part, usage != Usage::Exclude)));
    Some(p)
}

// parseComponent converts a path segment string into a component.
fn parse_component(s: &str, is_include: bool) -> Component {
    if s == "**" {
        return Component {
            kind: ComponentKind::DoubleAsterisk,
            literal: String::new(),
            segments: Vec::new(),
            skip_package_folders: false,
        };
    }
    if !s.contains(['*', '?']) {
        return Component {
            kind: ComponentKind::Literal,
            literal: s.to_string(),
            segments: Vec::new(),
            skip_package_folders: false,
        };
    }
    Component {
        kind: ComponentKind::Wildcard,
        literal: String::new(),
        segments: parse_segments(s),
        skip_package_folders: is_include,
    }
}

// parseSegments breaks "*.ts" into [segStar, segLiteral(".ts")]
fn parse_segments(s: &str) -> Vec<Segment> {
    // Preallocate based on wildcard count: each wildcard contributes 1 segment,
    // and each wildcard can split literals into at most one extra literal segment.
    let b = s.as_bytes();
    let wildcards = b.iter().filter(|&&c| c == b'*' || c == b'?').count();
    let mut result = Vec::with_capacity(2 * wildcards + 1);
    let mut start = 0;
    result = (0..b.len()).fold(result, |mut result, i| {
        match b[i] {
            b'*' | b'?' => {
                if i > start {
                    result.push(Segment {
                        kind: SegmentKind::Literal,
                        literal: s[start..i].to_string(),
                    });
                }
                if b[i] == b'*' {
                    result.push(Segment { kind: SegmentKind::Star, literal: String::new() });
                } else {
                    result.push(Segment { kind: SegmentKind::Question, literal: String::new() });
                }
                start = i + 1;
            }
            _ => {}
        }

        result
    });
    if start < b.len() {
        result.push(Segment { kind: SegmentKind::Literal, literal: s[start..].to_string() });
    }
    result
}

impl GlobPattern {
    // matches returns true if path matches this pattern.
    pub fn matches(&self, path: &str) -> bool {
        self.match_path_parts(path, "", 0, 0, false)
    }

    // matchesParts returns true if prefix+suffix matches this pattern.
    // This avoids allocating a combined string for common call sites where prefix ends with '/'.
    fn matches_parts(&self, prefix: &str, suffix: &str) -> bool {
        self.match_path_parts(prefix, suffix, 0, 0, false)
    }

    // matchesPrefixParts returns true if files under prefix+suffix could match.
    fn matches_prefix_parts(&self, prefix: &str, suffix: &str) -> bool {
        self.match_path_parts(prefix, suffix, 0, 0, true)
    }

    // matchPathParts is like matchPath, but operates on a virtual path formed by prefix+suffix.
    // Offsets are in the combined string.
    fn match_path_parts(
        &self,
        prefix: &str,
        suffix: &str,
        mut path_offset: usize,
        mut comp_idx: usize,
        prefix_only: bool,
    ) -> bool {
        loop {
            let Some((part_start, part_end, next_offset)) =
                next_path_part_parts(prefix, suffix, path_offset)
            else {
                if prefix_only {
                    return true;
                }
                return self.pattern_satisfied(comp_idx);
            };
            let path_part = if part_end <= prefix.len() {
                &prefix[part_start..part_end]
            } else {
                &suffix[part_start - prefix.len()..part_end - prefix.len()]
            };

            if comp_idx >= self.components.len() {
                return self.is_exclude && !prefix_only;
            }

            let comp = &self.components[comp_idx];
            match comp.kind {
                ComponentKind::DoubleAsterisk => {
                    if self.match_path_parts(prefix, suffix, path_offset, comp_idx + 1, prefix_only)
                    {
                        return true;
                    }
                    if !self.is_exclude
                        && (is_hidden_path(path_part) || is_package_folder(path_part))
                    {
                        return false;
                    }
                    path_offset = next_offset;
                    continue;
                }
                ComponentKind::Literal => {
                    assert!(
                        !(comp.skip_package_folders && is_package_folder(path_part)),
                        "unreachable: literal components never have skipPackageFolders"
                    );
                    if !self.strings_equal(comp.literal.as_bytes(), path_part.as_bytes()) {
                        return false;
                    }
                }
                ComponentKind::Wildcard => {
                    if comp.skip_package_folders && is_package_folder(path_part) {
                        return false;
                    }
                    if !self.match_wildcard(&comp.segments, path_part) {
                        return false;
                    }
                }
            }

            path_offset = next_offset;
            comp_idx += 1;
        }
    }

    /// tsrs-only: false when no path below the directory `dir_prefix` (ending in '/') can match, because a leading
    /// literal component differs from the directory's part at its position. `match_path_parts` on such a path reads
    /// the same parts against the same components first and fails there, so skipping the pattern for the
    /// directory's entries gives the same answers.
    fn may_match_below(&self, dir_prefix: &str) -> bool {
        let (mut offset, mut comp_idx) = (0, 0);
        loop {
            let Some((part_start, part_end, next_offset)) =
                next_path_part_single(dir_prefix, offset)
            else {
                return true;
            };
            let Some(comp) = self.components.get(comp_idx) else {
                return true;
            };
            if comp.kind != ComponentKind::Literal {
                return true;
            }
            if !self.strings_equal(
                comp.literal.as_bytes(),
                &dir_prefix.as_bytes()[part_start..part_end],
            ) {
                return false;
            }
            offset = next_offset;
            comp_idx += 1;
        }
    }

    // patternSatisfied checks if remaining pattern components can match empty input.
    fn pattern_satisfied(&self, comp_idx: usize) -> bool {
        // A pattern is satisfied when remaining components can match empty input.
        // For both include and exclude patterns, only trailing "**" components may match nothing.
        !(self.components[comp_idx..].iter().any(|c| c.kind != ComponentKind::DoubleAsterisk))
    }

    // matchWildcard matches a path component against wildcard segments.
    fn match_wildcard(&self, segs: &[Segment], s: &str) -> bool {
        // Include patterns: wildcards at start cannot match hidden files
        if !self.is_exclude
            && !segs.is_empty()
            && is_hidden_path(s)
            && (segs[0].kind == SegmentKind::Star || segs[0].kind == SegmentKind::Question)
        {
            return false;
        }

        // Fast path: single * followed by literal suffix (e.g., "*.ts")
        if segs.len() == 2
            && segs[0].kind == SegmentKind::Star
            && segs[1].kind == SegmentKind::Literal
        {
            let suffix = segs[1].literal.as_bytes();
            let sb = s.as_bytes();
            if sb.len() < suffix.len()
                || !self.strings_equal(suffix, &sb[sb.len() - suffix.len()..])
            {
                return false;
            }
            return self.should_include_min_js(s, segs);
        }

        self.match_segments(segs, s) && self.should_include_min_js(s, segs)
    }

    // matchSegments matches segments against string s using an iterative algorithm.
    // This avoids exponential backtracking by tracking only the last star position.
    // The algorithm is O(n*m) where n is the string length and m is pattern length.
    fn match_segments(&self, segs: &[Segment], s: &str) -> bool {
        let s = s.as_bytes();
        let (mut seg_idx, mut s_idx) = (0usize, 0usize);
        let (mut star_seg_idx, mut star_s_idx) = (None, 0usize);

        while s_idx < s.len() {
            if seg_idx < segs.len() {
                let seg = &segs[seg_idx];
                match seg.kind {
                    SegmentKind::Literal => {
                        let end = s_idx + seg.literal.len();
                        if end <= s.len()
                            && self.strings_equal(seg.literal.as_bytes(), &s[s_idx..end])
                        {
                            s_idx = end;
                            seg_idx += 1;
                            continue;
                        }
                    }
                    SegmentKind::Question => {
                        if s[s_idx] != b'/' {
                            let (_, size) = decode_rune(&s[s_idx..]);
                            s_idx += size;
                            seg_idx += 1;
                            continue;
                        }
                    }
                    SegmentKind::Star => {
                        // Record star position for backtracking, then try matching zero chars.
                        star_seg_idx = Some(seg_idx);
                        star_s_idx = s_idx;
                        seg_idx += 1;
                        continue;
                    }
                }
            }

            // Current segment didn't match. Backtrack to last star if possible.
            if let Some(star_seg_idx) =
                star_seg_idx.filter(|_| star_s_idx < s.len() && s[star_s_idx] != b'/')
            {
                // Star consumes one more character (rune), retry from segment after star.
                let (_, size) = decode_rune(&s[star_s_idx..]);
                star_s_idx += size;
                s_idx = star_s_idx;
                seg_idx = star_seg_idx + 1;
                continue;
            }

            return false;
        }

        // Consume any trailing stars.
        seg_idx += (seg_idx..segs.len())
            .take_while(|&index| segs[index].kind == SegmentKind::Star)
            .count();
        seg_idx >= segs.len()
    }

    fn should_include_min_js(&self, filename: &str, segs: &[Segment]) -> bool {
        if !self.exclude_min_js {
            return true;
        }

        // Preserve legacy behavior:
        // - When matching is case-sensitive, only the exact ".min.js" suffix is excluded by default.
        // - When matching is case-insensitive, any casing variant is excluded by default.
        if !self.has_min_js_suffix(filename) {
            return true;
        }
        // Allow when the user's pattern explicitly references the .min. suffix.
        if self.pattern_mentions_min_suffix(segs) {
            return true;
        }
        false
    }

    fn has_min_js_suffix(&self, filename: &str) -> bool {
        const MIN_JS: &str = ".min.js";

        if self.case_sensitive {
            return filename.ends_with(".min.js");
        }
        let f = filename.as_bytes();
        if f.len() < MIN_JS.len() {
            return false;
        }
        // Avoid allocating via strings.ToLower; compare suffix case-insensitively.
        stringutil::equal_fold_bytes(&f[f.len() - MIN_JS.len()..], MIN_JS.as_bytes())
    }

    fn pattern_mentions_min_suffix(&self, segs: &[Segment]) -> bool {
        if segs.iter().any(|seg| {
            if seg.kind != SegmentKind::Literal {
                return false;
            }
            let lit = if self.case_sensitive {
                seg.literal.clone()
            } else {
                stringutil::go_strings_to_lower(&seg.literal)
            };
            lit.contains(".min.js") || lit.contains(".min.")
        }) {
            return true;
        }
        false
    }

    // stringsEqual compares strings with appropriate case sensitivity.
    fn strings_equal(&self, a: &[u8], b: &[u8]) -> bool {
        if self.case_sensitive {
            return a == b;
        }
        stringutil::equal_fold_bytes(a, b)
    }
}

// nextPathPart extracts the next path component from path starting at offset.
// The part is returned as the byte range [start, end) of s, followed by the next offset.
fn next_path_part_single(s: &str, mut offset: usize) -> Option<(usize, usize, usize)> {
    let b = s.as_bytes();
    if offset >= b.len() {
        return None;
    }
    if offset == 0 && !b.is_empty() && b[0] == b'/' {
        return Some((0, 0, 1));
    }
    offset += (offset..b.len()).take_while(|&index| b[index] == b'/').count();
    if offset >= b.len() {
        return None;
    }
    let rest = &b[offset..];
    if let Some(idx) = rest.iter().position(|&c| c == b'/') {
        return Some((offset, offset + idx, offset + idx));
    }
    Some((offset, b.len(), b.len()))
}

// The part is returned as a byte range of the virtual string prefix+suffix
// (it never spans both).
pub fn next_path_part_parts(
    prefix: &str,
    suffix: &str,
    mut offset: usize,
) -> Option<(usize, usize, usize)> {
    // Fast paths: keep the hot single-string scan tight.
    if suffix.is_empty() {
        return next_path_part_single(prefix, offset);
    }
    if prefix.is_empty() {
        return next_path_part_single(suffix, offset);
    }

    // For matchFilesNoRegex call sites, prefix is a directory path ending in '/',
    // and suffix is a single entry name (no '/'). That makes this significantly
    // simpler than a general-purpose "virtual concatenation" scanner.

    let total_len = prefix.len() + suffix.len();
    if offset >= total_len {
        return None;
    }

    let pb = prefix.as_bytes();
    // Handle leading slash (root of absolute path)
    if offset == 0 && pb[0] == b'/' {
        return Some((0, 0, 1));
    }

    // Scan within prefix.
    if offset < pb.len() {
        offset += (offset..pb.len()).take_while(|&index| pb[index] == b'/').count();
        if offset < pb.len() {
            let rest = &pb[offset..];
            // idx is guaranteed >= 0 for the call sites we care about because prefix ends in '/'.
            let idx = rest.iter().position(|&c| c == b'/').unwrap();
            return Some((offset, offset + idx, offset + idx));
        }
        // Fall through into suffix region.
    }

    // Scan suffix: it's a single component.
    let s_off = offset - prefix.len();
    if s_off >= suffix.len() {
        return None;
    }
    Some((offset, total_len, total_len))
}

// isHiddenPath checks if a path component is hidden (starts with dot).
fn is_hidden_path(name: &str) -> bool {
    !name.is_empty() && name.as_bytes()[0] == b'.'
}

// isPackageFolder checks if name is a common package folder (node_modules, etc.)
fn is_package_folder(name: &str) -> bool {
    let b = name.as_bytes();
    match b.len() {
        12 => stringutil::equal_fold_bytes(b, b"node_modules"),
        13 => stringutil::equal_fold_bytes(b, b"jspm_packages"),
        16 => stringutil::equal_fold_bytes(b, b"bower_components"),
        _ => false,
    }
}

pub fn ensure_trailing_slash(s: &str) -> String {
    if !s.is_empty() && !s.ends_with('/') {
        return format!("{s}/");
    }
    s.to_string()
}

// utf8.DecodeRune: invalid encodings decode as (U+FFFD, 1).
fn decode_rune(b: &[u8]) -> (char, usize) {
    if b.is_empty() {
        return (char::REPLACEMENT_CHARACTER, 0);
    }
    let n = match b[0] {
        0x00..=0x7F => return (b[0] as char, 1),
        0xC2..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF4 => 4,
        _ => return (char::REPLACEMENT_CHARACTER, 1),
    };
    if b.len() < n {
        return (char::REPLACEMENT_CHARACTER, 1);
    }
    match std::str::from_utf8(&b[..n]) {
        Ok(s) => (s.chars().next().unwrap(), n),
        Err(_) => (char::REPLACEMENT_CHARACTER, 1),
    }
}

// globMatcher combines include and exclude patterns for file matching.
struct GlobMatcher {
    includes: Vec<GlobPattern>,
    excludes: Vec<GlobPattern>,
    had_includes: bool, // true if include specs were provided (even if none compiled)
}

fn new_glob_matcher(
    include_specs: &[impl AsRef<str>],
    exclude_specs: &[impl AsRef<str>],
    base_path: &str,
    case_sensitive: bool,
    usage: Usage,
) -> GlobMatcher {
    let mut m = GlobMatcher {
        had_includes: !include_specs.is_empty(),
        includes: Vec::with_capacity(include_specs.len()),
        excludes: Vec::with_capacity(exclude_specs.len()),
    };

    m.includes.extend(
        include_specs.iter().filter_map(|spec| {
            compile_glob_pattern(spec.as_ref(), base_path, usage, case_sensitive)
        }),
    );
    m.excludes.extend(exclude_specs.iter().filter_map(|spec| {
        compile_glob_pattern(spec.as_ref(), base_path, Usage::Exclude, case_sensitive)
    }));
    m
}

impl GlobMatcher {
    // matchesFileParts checks if prefix+suffix matches against the glob patterns.
    // Returns the index of the matching include pattern, or None if not matched.
    fn matches_file_parts(&self, prefix: &str, suffix: &str) -> Option<usize> {
        if let Some(result) = self
            .excludes
            .iter()
            .find_map(|e| if e.matches_parts(prefix, suffix) { Some(None) } else { None })
        {
            return result;
        }
        self.matches_includes(prefix, suffix)
    }

    fn matches_includes(&self, prefix: &str, suffix: &str) -> Option<usize> {
        if self.includes.is_empty() {
            if self.had_includes {
                return None;
            }
            return Some(0);
        }
        if let Some(i) = self.includes.iter().position(|inc| inc.matches_parts(prefix, suffix)) {
            return Some(i);
        }
        None
    }

    /// tsrs-only: the indices of the exclude patterns, of `parent` (or all), that may match below `dir_prefix`
    /// (`GlobPattern::may_match_below`); the `_in` variants below then test only those.
    fn live_excludes(&self, parent: Option<&[u32]>, dir_prefix: &str) -> Vec<u32> {
        let may = |&i: &u32| self.excludes[i as usize].may_match_below(dir_prefix);
        match parent {
            Some(parent) => parent.iter().copied().filter(may).collect(),
            None => (0..self.excludes.len() as u32).filter(may).collect(),
        }
    }

    /// `matches_file_parts` for an entry of a directory whose live excludes are `live`.
    fn matches_file_parts_in(&self, live: &[u32], prefix: &str, suffix: &str) -> Option<usize> {
        if let Some(result) = live.iter().find_map(|&e| {
            if self.excludes[e as usize].matches_parts(prefix, suffix) { Some(None) } else { None }
        }) {
            return result;
        }
        self.matches_includes(prefix, suffix)
    }

    /// `matches_directory_parts` for an entry of a directory whose live excludes are `live`.
    fn matches_directory_parts_in(&self, live: &[u32], prefix: &str, suffix: &str) -> bool {
        if live.iter().any(|&e| self.excludes[e as usize].matches_parts(prefix, suffix)) {
            return false;
        }
        self.matches_directory_includes(prefix, suffix)
    }

    // matchesDirectoryParts checks if files under the directory prefix+suffix could match any pattern.
    fn matches_directory_parts(&self, prefix: &str, suffix: &str) -> bool {
        if self.excludes.iter().any(|e| e.matches_parts(prefix, suffix)) {
            return false;
        }
        self.matches_directory_includes(prefix, suffix)
    }

    fn matches_directory_includes(&self, prefix: &str, suffix: &str) -> bool {
        if self.includes.is_empty() {
            return !self.had_includes;
        }
        self.includes.iter().any(|inc| inc.matches_prefix_parts(prefix, suffix))
    }
}

// globVisitor traverses directories matching files against glob patterns.
// (host and extensions are passed to visit rather than stored.)
struct GlobVisitor {
    file_matcher: GlobMatcher,
    directory_matcher: GlobMatcher,
    use_case_sensitive_file_names: bool,
    visited: FxHashSet<String>,
    results: Vec<Vec<String>>,
    // Directory listings read ahead in parallel (prefetch_listings), by absolute path.
    listings: FxHashMap<String, PrefetchedListing>,
}

// Directory listings `prefetch_listings` reads on the calling thread before the rest of the tree fans out on rayon.
const INLINE_PREFETCH_LISTINGS: usize = 64;

// A directory listing read ahead, with the matching that the visit of the directory does on it.
struct PrefetchedListing {
    entries: Entries,
    // (include index, index in `entries.files`) of each file that has one of the extensions and matches, in order.
    files: Vec<(usize, usize)>,
    // `directory_matcher.matches_directory_parts` of each of `entries.directories`; empty where the visit stops at
    // this depth.
    directories: Vec<bool>,
}

impl GlobVisitor {
    // tsrs-only: reads the listings of every directory the visit below will enter, in parallel, so that the
    // sequential visit (whose order and symlink-cycle handling are unchanged) finds them ready. Symlinked
    // directories are not followed here (the visit resolves them itself, with its cycle check), and neither is
    // anything below a listing without symlink information. Each listing is also matched here, against the same
    // matchers and with the same prefix as in the visit, which then only collects the results in its order.
    fn prefetch_listings(
        &mut self,
        host: &dyn FS,
        extensions: &[&str],
        base_paths: &[String],
        depth: usize,
    ) {
        struct Walk<'a> {
            host: &'a dyn FS,
            extensions: &'a [&'a str],
            file_matcher: &'a GlobMatcher,
            directory_matcher: &'a GlobMatcher,
            listings: std::sync::Mutex<FxHashMap<String, PrefetchedListing>>,
        }
        // A directory to read, with its parent's live excludes (`GlobMatcher::live_excludes`; `None` for a base path).
        struct Pending {
            path: String,
            depth: usize,
            live: Option<(Vec<u32>, Vec<u32>)>,
        }
        // Reads and matches one listing and returns the subdirectories to read next.
        fn read(w: &Walk<'_>, pending: Pending) -> Vec<Pending> {
            let Pending { path, depth, live } = pending;
            let (live_files, live_directories) = match &live {
                Some((files, directories)) => {
                    (Some(files.as_slice()), Some(directories.as_slice()))
                }
                None => (None, None),
            };
            let entries = w.host.get_accessible_entries(&path);
            let prefix = ensure_trailing_slash(&path);
            let live_files = w.file_matcher.live_excludes(live_files, &prefix);
            let live_directories = w.directory_matcher.live_excludes(live_directories, &prefix);
            let files = entries
                .files
                .iter()
                .enumerate()
                .filter(|(_, file)| {
                    w.extensions.is_empty()
                        || w.extensions.iter().any(|ext| tspath::file_extension_is(file, ext))
                })
                .filter_map(|(i, file)| {
                    w.file_matcher
                        .matches_file_parts_in(&live_files, &prefix, file)
                        .map(|idx| (idx, i))
                })
                .collect();
            let child_depth = if depth == UNLIMITED_DEPTH { UNLIMITED_DEPTH } else { depth - 1 };
            let mut directories = Vec::new();
            let mut children = Vec::new();
            if child_depth != 0 {
                directories = entries
                    .directories
                    .iter()
                    .map(|dir| {
                        w.directory_matcher.matches_directory_parts_in(
                            &live_directories,
                            &prefix,
                            dir,
                        )
                    })
                    .collect();
                if let Some(symlinks) = &entries.symlinks {
                    for (dir, &matched) in entries.directories.iter().zip(&directories) {
                        if matched && !symlinks.contains(dir) {
                            children.push(Pending {
                                path: format!("{prefix}{dir}"),
                                depth: child_depth,
                                live: Some((live_files.clone(), live_directories.clone())),
                            });
                        }
                    }
                }
            }
            w.listings
                .lock()
                .unwrap()
                .insert(path, PrefetchedListing { entries, files, directories });
            children
        }
        fn walk<'s>(s: &rayon::Scope<'s>, w: &'s Walk<'s>, pending: Pending) {
            for child in read(w, pending) {
                s.spawn(move |s| walk(s, w, child));
            }
        }
        let w = Walk {
            host,
            extensions,
            file_matcher: &self.file_matcher,
            directory_matcher: &self.directory_matcher,
            listings: std::sync::Mutex::new(FxHashMap::default()),
        };
        let mut seen: FxHashSet<&str> = FxHashSet::default();
        let mut queue: std::collections::VecDeque<Pending> = base_paths
            .iter()
            .filter(|path| seen.insert(path))
            .map(|path| Pending { path: path.clone(), depth, live: None })
            .collect();
        // `tsc -b` expands one small include per project on each builder, where a hand-off to the pool per project
        // costs more than the reads; only a larger tree fans out.
        let mut read_inline = 0;
        while read_inline < INLINE_PREFETCH_LISTINGS
            && let Some(pending) = queue.pop_front()
        {
            queue.extend(read(&w, pending));
            read_inline += 1;
        }
        if !queue.is_empty() {
            rayon::scope(|s| {
                for pending in queue {
                    let w = &w;
                    s.spawn(move |s| walk(s, w, pending));
                }
            });
        }
        self.listings = w.listings.into_inner().unwrap();
    }
}

impl GlobVisitor {
    // visit walks a directory tree, collecting files that match the glob patterns.
    // resolvedRealPath, when non-empty, is the already-resolved real path for this
    // directory (computed incrementally from the parent). When empty, Realpath is
    // called to resolve symlinks.
    fn visit(
        &mut self,
        host: &dyn FS,
        extensions: &[impl AsRef<str>],
        path: &str,
        absolute_path: &str,
        mut depth: usize,
        resolved_real_path: &str,
    ) {
        // Detect symlink cycles
        let real_path = if resolved_real_path.is_empty() {
            host.realpath(absolute_path)
        } else {
            resolved_real_path.to_string()
        };
        let canonical_path =
            tspath::get_canonical_file_name(&real_path, self.use_case_sensitive_file_names);
        if self.visited.contains(&canonical_path) {
            return;
        }
        self.visited.insert(canonical_path);

        let (entries, prefetched) = match self.listings.remove(absolute_path) {
            Some(PrefetchedListing { entries, files, directories }) => {
                (entries, Some((files, directories)))
            }
            None => (host.get_accessible_entries(absolute_path), None),
        };

        let path_prefix = ensure_trailing_slash(path);
        let abs_prefix = ensure_trailing_slash(absolute_path);

        if let Some((files, _)) = &prefetched {
            for &(idx, i) in files {
                self.results[idx].push(format!("{}{}", path_prefix, entries.files[i]));
            }
        } else {
            for file in &entries.files {
                if !extensions.is_empty()
                    && !extensions.iter().any(|ext| tspath::file_extension_is(file, ext.as_ref()))
                {
                    continue;
                }
                if let Some(idx) = self.file_matcher.matches_file_parts(&abs_prefix, file) {
                    self.results[idx].push(format!("{path_prefix}{file}"));
                }
            }
        }

        if depth != UNLIMITED_DEPTH {
            depth -= 1;
            if depth == 0 {
                return;
            }
        }

        for (i, dir) in entries.directories.iter().enumerate() {
            let matched = match prefetched.as_ref().and_then(|(_, directories)| directories.get(i))
            {
                Some(&matched) => matched,
                None => self.directory_matcher.matches_directory_parts(&abs_prefix, dir),
            };
            if !matched {
                continue;
            }
            let abs_dir = format!("{abs_prefix}{dir}");
            let mut child_real_path = String::new();
            if let Some(symlinks) = &entries.symlinks
                && !symlinks.contains(dir)
            {
                // Non-symlink directory: compute realpath incrementally.
                child_real_path = tspath::combine_paths(&real_path, &[dir]);
            }
            // else: symlink directory; leave childRealPath empty to force Realpath call.
            // If Symlinks is None, the FS doesn't track symlinks;
            // leave childRealPath empty to call Realpath (preserving old behavior).
            self.visit(
                host,
                extensions,
                &format!("{path_prefix}{dir}"),
                &abs_dir,
                depth,
                &child_real_path,
            );
        }
    }
}

pub fn match_files(
    path: &str,
    extensions: &[impl AsRef<str>],
    excludes: &[impl AsRef<str>],
    includes: &[impl AsRef<str>],
    use_case_sensitive_file_names: bool,
    current_directory: &str,
    depth: usize,
    host: &dyn FS,
) -> Vec<String> {
    let path = tspath::normalize_path(path);
    let current_directory = tspath::normalize_path(current_directory);
    let absolute_path = tspath::combine_paths(&current_directory, &[&path]);

    let file_matcher = new_glob_matcher(
        includes,
        excludes,
        &absolute_path,
        use_case_sensitive_file_names,
        Usage::Files,
    );
    let directory_matcher = new_glob_matcher(
        includes,
        excludes,
        &absolute_path,
        use_case_sensitive_file_names,
        Usage::Directories,
    );

    let results_len = file_matcher.includes.len().max(1);
    let mut v = GlobVisitor {
        file_matcher,
        directory_matcher,
        use_case_sensitive_file_names,
        visited: FxHashSet::default(),
        results: vec![Vec::new(); results_len],
        listings: FxHashMap::default(),
    };

    let base_paths = get_base_paths(&path, includes, use_case_sensitive_file_names);
    let absolute_base_paths: Vec<String> = base_paths
        .iter()
        .map(|base_path| tspath::combine_paths(&current_directory, &[base_path]))
        .collect();
    let extension_strs: Vec<&str> = extensions.iter().map(AsRef::as_ref).collect();
    v.prefetch_listings(host, &extension_strs, &absolute_base_paths, depth);
    for (base_path, abs) in base_paths.iter().zip(&absolute_base_paths) {
        v.visit(host, extensions, base_path, abs, depth, "");
    }

    // Fast path: a single include bucket (or no includes) doesn't need flattening.
    if v.results.len() == 1 {
        return v.results.pop().unwrap();
    }
    v.results.concat()
}

// SpecMatcher wraps multiple glob patterns for matching paths.
#[derive(Clone, Debug)]
pub struct SpecMatcher {
    patterns: Vec<GlobPattern>,
}

impl SpecMatcher {
    // MatchString returns true if any pattern matches the path.
    pub fn match_string(&self, path: &str) -> bool {
        self.patterns.iter().any(|p| p.matches(path))
    }

    // MatchIndex returns the index of the first matching pattern.
    pub fn match_index(&self, path: &str) -> Option<usize> {
        self.patterns.iter().position(|pattern| pattern.matches(path))
    }
}

// NewSpecMatcher creates a matcher for one or more glob specs.
// It returns a matcher that can test if paths match any of the patterns.
pub fn new_spec_matcher(
    specs: &[impl AsRef<str>],
    base_path: &str,
    usage: Usage,
    use_case_sensitive_file_names: bool,
) -> Option<SpecMatcher> {
    if specs.is_empty() {
        return None;
    }
    let mut patterns = Vec::with_capacity(specs.len());
    patterns.extend(specs.iter().filter_map(|spec| {
        compile_glob_pattern(spec.as_ref(), base_path, usage, use_case_sensitive_file_names)
    }));
    if patterns.is_empty() {
        return None;
    }
    Some(SpecMatcher { patterns })
}
