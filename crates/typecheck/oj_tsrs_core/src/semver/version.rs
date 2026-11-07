use std::cmp::Ordering;
use std::fmt;
use std::sync::LazyLock;

// Go's `(?i)[a-z]` also matches U+017F (folds to 's') and U+212A (folds to 'k').
pub(super) fn is_letter_fold(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '\u{17F}' || c == '\u{212A}'
}

pub(super) fn is_digit(c: char) -> bool {
    c.is_ascii_digit()
}

// [a-z0-9-.] with (?i)
fn is_identifier_or_dot_char(c: char) -> bool {
    is_letter_fold(c) || is_digit(c) || c == '-' || c == '.'
}

// Length in bytes of the longest prefix of `text` whose chars all satisfy `f`.
pub(super) fn span(text: &str, f: impl Fn(char) -> bool) -> usize {
    text.char_indices().find(|&(_, c)| !f(c)).map_or(text.len(), |(i, _)| i)
}

// 0|[1-9]\d*
pub(super) fn match_numeric_component(text: &str) -> Option<usize> {
    let b = text.as_bytes();
    match b.first() {
        Some(b'0') => Some(1),
        Some(b'1'..=b'9') => Some(1 + span(&text[1..], is_digit)),
        _ => None,
    }
}

// Shared shape of versionRegexp and partialRegExp:
// ^(C)(?:\.(C)(?:\.(C)(?:-([a-z0-9-.]+))?(?:\+([a-z0-9-.]+))?)?)?$
// Every group is deterministic and anything a skipped optional group would have consumed
// must then be matched by `$`, so a greedy parse followed by an end check is equivalent.
// Non-participating groups are "", as in Go's FindStringSubmatch.
pub(super) fn find_version_like_submatch(
    text: &str,
    component: fn(&str) -> Option<usize>,
) -> Option<[&str; 6]> {
    let mut m: [&str; 6] = [""; 6];
    let n = component(text)?;
    m[1] = &text[..n];
    let mut pos = n;
    if let Some(minor_len) = text[pos..].strip_prefix('.').and_then(component) {
        m[2] = &text[pos + 1..pos + 1 + minor_len];
        pos += 1 + minor_len;
        if let Some(patch_len) = text[pos..].strip_prefix('.').and_then(component) {
            m[3] = &text[pos + 1..pos + 1 + patch_len];
            pos += 1 + patch_len;
            if let Some(rest) = text[pos..].strip_prefix('-') {
                let l = span(rest, is_identifier_or_dot_char);
                if l > 0 {
                    m[4] = &rest[..l];
                    pos += 1 + l;
                }
            }
            if let Some(rest) = text[pos..].strip_prefix('+') {
                let l = span(rest, is_identifier_or_dot_char);
                if l > 0 {
                    m[5] = &rest[..l];
                    pos += 1 + l;
                }
            }
        }
    }
    if pos != text.len() {
        return None;
    }
    m[0] = text;
    Some(m)
}

// https://semver.org/#spec-item-2
// > A normal version number MUST take the form X.Y.Z where X, Y, and Z are non-negative
// > integers, and MUST NOT contain leading zeroes. X is the major version, Y is the minor
// > version, and Z is the patch version. Each element MUST increase numerically.
//
// NOTE: We differ here in that we allow X and X.Y, with missing parts having the default
// value of `0`.
// (?i)^(0|[1-9]\d*)(?:\.(0|[1-9]\d*)(?:\.(0|[1-9]\d*)(?:-([a-z0-9-.]+))?(?:\+([a-z0-9-.]+))?)?)?$
fn version_regexp_find_string_submatch(text: &str) -> Option<[&str; 6]> {
    find_version_like_submatch(text, match_numeric_component)
}

// (?i)^(?:0|[1-9]\d*|[a-z-][a-z0-9-]*)$
fn is_prerelease_part(part: &str) -> bool {
    if part == "0" {
        return true;
    }
    let mut chars = part.chars();
    match chars.next() {
        Some('1'..='9') => chars.all(is_digit),
        Some(c) if is_letter_fold(c) || c == '-' => {
            chars.all(|c| is_letter_fold(c) || is_digit(c) || c == '-')
        }
        _ => false,
    }
}

// https://semver.org/#spec-item-9
// > A pre-release version MAY be denoted by appending a hyphen and a series of dot separated
// > identifiers immediately following the patch version. Identifiers MUST comprise only ASCII
// > alphanumerics and hyphen [0-9A-Za-z-]. Identifiers MUST NOT be empty. Numeric identifiers
// > MUST NOT include leading zeroes.
// (?i)^(?:0|[1-9]\d*|[a-z-][a-z0-9-]*)(?:\.(?:0|[1-9]\d*|[a-zA-Z-][a-zA-Z0-9-]*))*$
fn prerelease_regexp_match_string(text: &str) -> bool {
    text.split('.').all(is_prerelease_part)
}

// (?i)^[a-z0-9-]+$
fn is_build_part(part: &str) -> bool {
    !part.is_empty() && part.chars().all(|c| is_letter_fold(c) || is_digit(c) || c == '-')
}

// https://semver.org/#spec-item-10
// > Build metadata MAY be denoted by appending a plus sign and a series of dot separated
// > identifiers immediately following the patch or pre-release version. Identifiers MUST
// > comprise only ASCII alphanumerics and hyphen [0-9A-Za-z-]. Identifiers MUST NOT be empty.
// (?i)^[a-z0-9-]+(?:\.[a-z0-9-]+)*$
fn build_reg_exp_match_string(text: &str) -> bool {
    text.split('.').all(is_build_part)
}

// https://semver.org/#spec-item-9
// > Numeric identifiers MUST NOT include leading zeroes.
// ^(?:0|[1-9]\d*)$
fn numeric_identifier_reg_exp_match_string(text: &str) -> bool {
    match_numeric_component(text) == Some(text.len())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Version {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub prerelease: Vec<String>,
    pub build: Vec<String>,
}

pub(super) static VERSION_ZERO: LazyLock<Version> =
    LazyLock::new(|| Version { prerelease: vec!["0".to_string()], ..Default::default() });

impl Version {
    pub(crate) fn increment_major(&self) -> Version {
        Version { major: self.major + 1, ..Default::default() }
    }

    pub(crate) fn increment_minor(&self) -> Version {
        Version { major: self.major, minor: self.minor + 1, ..Default::default() }
    }

    pub(crate) fn increment_patch(&self) -> Version {
        Version {
            major: self.major,
            minor: self.minor,
            patch: self.patch + 1,
            ..Default::default()
        }
    }
}

pub const COMPARISON_LESS_THAN: Ordering = Ordering::Less;
pub const COMPARISON_EQUAL_TO: Ordering = Ordering::Equal;
pub const COMPARISON_GREATER_THAN: Ordering = Ordering::Greater;

fn cmp_compare<T: Ord + Copy>(a: T, b: T) -> Ordering {
    a.cmp(&b)
}

impl Version {
    pub fn compare(&self, b: &Version) -> Ordering {
        // https://semver.org/#spec-item-11
        // > Precedence is determined by the first difference when comparing each of these
        // > identifiers from left to right as follows: Major, minor, and patch versions are
        // > always compared numerically.
        //
        // https://semver.org/#spec-item-11
        // > Precedence for two pre-release versions with the same major, minor, and patch version
        // > MUST be determined by comparing each dot separated identifier from left to right until
        // > a difference is found [...]
        //
        // https://semver.org/#spec-item-11
        // > Build metadata does not figure into precedence
        let a = self;
        if std::ptr::eq(a, b) {
            return COMPARISON_EQUAL_TO;
        }

        let r = cmp_compare(a.major, b.major);
        if r != Ordering::Equal {
            return r;
        }

        let r = cmp_compare(a.minor, b.minor);
        if r != Ordering::Equal {
            return r;
        }

        let r = cmp_compare(a.patch, b.patch);
        if r != Ordering::Equal {
            return r;
        }

        compare_pre_release_identifiers(&a.prerelease, &b.prerelease)
    }
}

fn compare_pre_release_identifiers(left: &[String], right: &[String]) -> Ordering {
    // https://semver.org/#spec-item-11
    // > When major, minor, and patch are equal, a pre-release version has lower precedence
    // > than a normal version.
    if left.is_empty() {
        if right.is_empty() {
            return COMPARISON_EQUAL_TO;
        }
        return COMPARISON_GREATER_THAN;
    } else if right.is_empty() {
        return COMPARISON_LESS_THAN;
    }

    // https://semver.org/#spec-item-11
    // > Precedence for two pre-release versions with the same major, minor, and patch version
    // > MUST be determined by comparing each dot separated identifier from left to right until
    // > a difference is found [...]
    if let Some(result) = left.iter().zip(right.iter()).into_iter().find_map(|(l, r)| {
        let c = compare_pre_release_identifier(l, r);
        if c != Ordering::Equal {
            return Some(c);
        }

        None
    }) {
        return result;
    }
    cmp_compare(left.len(), right.len())
}

fn compare_pre_release_identifier(left: &str, right: &str) -> Ordering {
    // https://semver.org/#spec-item-11
    // > Precedence for two pre-release versions with the same major, minor, and patch version
    // > MUST be determined by comparing each dot separated identifier from left to right until
    // > a difference is found [...]
    let compare_result = cmp_compare(left, right);
    if compare_result == Ordering::Equal {
        return compare_result;
    }

    let left_is_numeric = numeric_identifier_reg_exp_match_string(left);
    let right_is_numeric = numeric_identifier_reg_exp_match_string(right);

    if left_is_numeric || right_is_numeric {
        // https://semver.org/#spec-item-11
        // > Numeric identifiers always have lower precedence than non-numeric identifiers.
        if !right_is_numeric {
            return COMPARISON_LESS_THAN;
        }
        if !left_is_numeric {
            return COMPARISON_GREATER_THAN;
        }

        // https://semver.org/#spec-item-11
        // > identifiers consisting of only digits are compared numerically
        let left_as_number = get_uint_component(left);
        let right_as_number = get_uint_component(right);
        if let (Ok(l), Ok(r)) = (left_as_number, right_as_number) {
            return cmp_compare(l, r);
        }
        // This should only happen in the event of an overflow.
        // If so, use the lengths or fall back to string comparison.
        let left_len = left.len();
        let right_len = right.len();
        let len_compare = cmp_compare(left_len, right_len);
        if len_compare == Ordering::Equal {
            return compare_result;
        }
        return len_compare;
    }

    // https://semver.org/#spec-item-11
    // > identifiers with letters or hyphens are compared lexically in ASCII sort order.
    compare_result
}

impl Version {
    pub fn string(&self) -> String {
        let mut sb = format!("{}.{}.{}", self.major, self.minor, self.patch);
        if !self.prerelease.is_empty() {
            sb.push('-');
            sb.push_str(&self.prerelease.join("."));
        }
        if !self.build.is_empty() {
            sb.push('+');
            sb.push_str(&self.build.join("."));
        }
        sb
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.string())
    }
}

fn semver_parse_error(orig_input: &str) -> String {
    format!("Could not parse version string from {orig_input:?}")
}

pub fn try_parse_version(text: &str) -> Result<Version, String> {
    let mut result = Version::default();

    let Some(m) = version_regexp_find_string_submatch(text) else {
        return Err(semver_parse_error(text));
    };

    let major_str = m[1];
    let minor_str = m[2];
    let patch_str = m[3];
    let prerelease_str = m[4];
    let build_str = m[5];

    result.major = get_uint_component(major_str)?;

    if !minor_str.is_empty() {
        result.minor = get_uint_component(minor_str)?;
    }

    if !patch_str.is_empty() {
        result.patch = get_uint_component(patch_str)?;
    }

    if !prerelease_str.is_empty() {
        if !prerelease_regexp_match_string(prerelease_str) {
            return Err(semver_parse_error(text));
        }

        result.prerelease = prerelease_str.split('.').map(str::to_string).collect();
    }
    if !build_str.is_empty() {
        if !build_reg_exp_match_string(build_str) {
            return Err(semver_parse_error(text));
        }

        result.build = build_str.split('.').map(str::to_string).collect();
    }

    Ok(result)
}

pub fn must_parse(text: &str) -> Version {
    match try_parse_version(text) {
        Ok(v) => v,
        Err(err) => panic!("{err}"),
    }
}

// strconv.ParseUint(text, 10, 32)
pub fn get_uint_component(text: &str) -> Result<u32, String> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("strconv.ParseUint: parsing {text:?}: invalid syntax"));
    }
    text.parse::<u32>()
        .map_err(|_| format!("strconv.ParseUint: parsing {text:?}: value out of range"))
}
