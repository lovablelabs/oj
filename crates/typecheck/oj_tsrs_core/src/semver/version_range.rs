use std::fmt;

use super::version::*;

// Go's `\s`: [\t\n\f\r ]
fn is_go_space(c: char) -> bool {
    matches!(c, '\t' | '\n' | '\x0C' | '\r' | ' ')
}

// [a-z0-9-+.*] with (?i)
fn is_range_operand_char(c: char) -> bool {
    is_letter_fold(c) || is_digit(c) || matches!(c, '-' | '+' | '.' | '*')
}

// https://github.com/npm/node-semver#range-grammar
//
// range-set    ::= range ( logical-or range ) *
// range        ::= hyphen | simple ( ' ' simple ) * | ”
// logical-or   ::= ( ' ' ) * '||' ( ' ' ) *
// \|\|
fn logical_or_reg_exp_split(text: &str) -> Vec<&str> {
    text.split("||").collect()
}

// \s+
fn whitespace_reg_exp_split(text: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut beg = 0;
    let mut i = 0;
    let bytes = text.as_bytes();
    while i < bytes.len() {
        if is_go_space(bytes[i] as char) {
            result.push(&text[beg..i]);
            i += (i..bytes.len()).take_while(|&index| is_go_space(bytes[index] as char)).count();
            beg = i;
        } else {
            i += 1;
        }
    }
    result.push(&text[beg..]);
    result
}

// https://github.com/npm/node-semver#range-grammar
//
// partial      ::= xr ( '.' xr ( '.' xr qualifier ? )? )?
// xr           ::= 'x' | 'X' | '*' | nr
// nr           ::= '0' | ['1'-'9'] ( ['0'-'9'] ) *
// qualifier    ::= ( '-' pre )? ( '+' build )?
// pre          ::= parts
// build        ::= parts
// parts        ::= part ( '.' part ) *
// part         ::= nr | [-0-9A-Za-z]+
// (?i)^([x*0]|[1-9]\d*)(?:\.([x*0]|[1-9]\d*)(?:\.([x*0]|[1-9]\d*)(?:-([a-z0-9-.]+))?(?:\+([a-z0-9-.]+))?)?)?$
fn partial_reg_exp_find_string_submatch(text: &str) -> Option<[&str; 6]> {
    // [x*0]|[1-9]\d*
    fn component(text: &str) -> Option<usize> {
        match text.as_bytes().first() {
            Some(b'x' | b'X' | b'*' | b'0') => Some(1),
            _ => match_numeric_component(text),
        }
    }
    find_version_like_submatch(text, component)
}

// https://github.com/npm/node-semver#range-grammar
//
// hyphen       ::= partial ' - ' partial
// (?i)^\s*([a-z0-9-+.*]+)\s+-\s+([a-z0-9-+.*]+)\s*$
fn hyphen_reg_exp_find_string_submatch(text: &str) -> Option<[&str; 3]> {
    let mut pos = span(text, is_go_space);
    let left_len = span(&text[pos..], is_range_operand_char);
    if left_len == 0 {
        return None;
    }
    let left = &text[pos..pos + left_len];
    pos += left_len;
    let ws = span(&text[pos..], is_go_space);
    if ws == 0 {
        return None;
    }
    pos += ws;
    if !text[pos..].starts_with('-') {
        return None;
    }
    pos += 1;
    let ws = span(&text[pos..], is_go_space);
    if ws == 0 {
        return None;
    }
    pos += ws;
    let right_len = span(&text[pos..], is_range_operand_char);
    if right_len == 0 {
        return None;
    }
    let right = &text[pos..pos + right_len];
    pos += right_len;
    pos += span(&text[pos..], is_go_space);
    if pos != text.len() {
        return None;
    }
    Some([text, left, right])
}

// https://github.com/npm/node-semver#range-grammar
//
// simple       ::= primitive | partial | tilde | caret
// primitive    ::= ( '<' | '>' | '>=' | '<=' | '=' ) partial
// tilde        ::= '~' partial
// caret        ::= '^' partial
// (?i)^([~^<>=]|<=|>=)?\s*([a-z0-9-+.*]+)$
// The alternatives of the optional first group are tried in leftmost-first order.
fn range_reg_exp_find_string_submatch(text: &str) -> Option<[&str; 3]> {
    fn rest(text: &str) -> Option<&str> {
        let operand = &text[span(text, is_go_space)..];
        if !operand.is_empty() && span(operand, is_range_operand_char) == operand.len() {
            Some(operand)
        } else {
            None
        }
    }
    if matches!(text.as_bytes().first(), Some(b'~' | b'^' | b'<' | b'>' | b'='))
        && let Some(operand) = rest(&text[1..])
    {
        return Some([text, &text[..1], operand]);
    }
    if let Some(result) = (["<=", ">="]).into_iter().find_map(|op| {
        if let Some(after) = text.strip_prefix(op)
            && let Some(operand) = rest(after)
        {
            return Some(Some([text, &text[..2], operand]));
        }

        None
    }) {
        return result;
    }
    rest(text).map(|operand| [text, "", operand])
}

#[derive(Clone, Debug, Default)]
pub struct VersionRange {
    alternatives: Vec<Vec<VersionComparator>>,
}

#[derive(Clone, Debug)]
pub struct VersionComparator {
    operator: ComparatorOperator,
    operand: Version,
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug)]
#[expect(
    clippy::enum_variant_names,
    reason = "The variant names preserve the upstream Go enum names."
)]
pub enum ComparatorOperator {
    RangeLessThan,
    RangeLessThanEqual,
    RangeEqual,
    RangeGreaterThanEqual,
    RangeGreaterThan,
}

impl ComparatorOperator {
    fn as_str(self) -> &'static str {
        match self {
            ComparatorOperator::RangeLessThan => "<",
            ComparatorOperator::RangeLessThanEqual => "<=",
            ComparatorOperator::RangeEqual => "=",
            ComparatorOperator::RangeGreaterThanEqual => ">=",
            ComparatorOperator::RangeGreaterThan => ">",
        }
    }
}

impl VersionRange {
    pub fn string(&self) -> String {
        let mut sb = String::new();
        format_disjunction(&mut sb, &self.alternatives);
        sb
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.string())
    }
}

fn format_disjunction(sb: &mut String, alternatives: &[Vec<VersionComparator>]) {
    let orig_len = sb.len();

    for (i, alternative) in alternatives.iter().enumerate() {
        if i > 0 {
            sb.push_str(" || ");
        }
        format_alternative(sb, alternative);
    }

    if sb.len() == orig_len {
        sb.push('*');
    }
}

fn format_alternative(sb: &mut String, comparators: &[VersionComparator]) {
    for (i, comparator) in comparators.iter().enumerate() {
        if i > 0 {
            sb.push(' ');
        }
        format_comparator(sb, comparator);
    }
}

fn format_comparator(sb: &mut String, comparator: &VersionComparator) {
    sb.push_str(comparator.operator.as_str());
    sb.push_str(&comparator.operand.string());
}

impl VersionRange {
    pub fn test(&self, version: &Version) -> bool {
        test_disjunction(&self.alternatives, version)
    }
}

fn test_disjunction(alternatives: &[Vec<VersionComparator>], version: &Version) -> bool {
    // an empty disjunction is treated as "*" (all versions)
    if alternatives.is_empty() {
        return true;
    }

    alternatives.iter().any(|alternative| test_alternative(alternative, version))
}

fn test_alternative(alternative: &[VersionComparator], version: &Version) -> bool {
    !(alternative.iter().any(|comparator| !test_comparator(comparator, version)))
}

fn test_comparator(comparator: &VersionComparator, version: &Version) -> bool {
    let cmp = version.compare(&comparator.operand);
    match comparator.operator {
        ComparatorOperator::RangeLessThan => cmp == std::cmp::Ordering::Less,
        ComparatorOperator::RangeLessThanEqual => cmp != std::cmp::Ordering::Greater,
        ComparatorOperator::RangeEqual => cmp == std::cmp::Ordering::Equal,
        ComparatorOperator::RangeGreaterThanEqual => cmp != std::cmp::Ordering::Less,
        ComparatorOperator::RangeGreaterThan => cmp == std::cmp::Ordering::Greater,
    }
}

pub fn try_parse_version_range(text: &str) -> Option<VersionRange> {
    let alternatives = parse_alternatives(text)?;
    Some(VersionRange { alternatives })
}

fn parse_alternatives(text: &str) -> Option<Vec<Vec<VersionComparator>>> {
    let mut alternatives = Vec::new();

    let text = text.trim();
    let ranges = logical_or_reg_exp_split(text);
    for r in ranges {
        let r = r.trim();
        if r.is_empty() {
            continue;
        }

        let mut comparators = Vec::new();

        if let Some(hyphen_match) = hyphen_reg_exp_find_string_submatch(r) {
            {
                let parsed_comparators = parse_hyphen(hyphen_match[1], hyphen_match[2])?;
                comparators.extend(parsed_comparators);
            }
        } else {
            comparators = whitespace_reg_exp_split(r).into_iter().try_fold(
                comparators,
                |mut comparators, simple| {
                    let m = range_reg_exp_find_string_submatch(simple.trim())?;

                    {
                        let parsed_comparators = parse_comparator(m[1], m[2])?;
                        comparators.extend(parsed_comparators);
                    }

                    Some(comparators)
                },
            )?;
        }

        alternatives.push(comparators);
    }

    Some(alternatives)
}

fn parse_hyphen(left: &str, right: &str) -> Option<Vec<VersionComparator>> {
    let left_result = parse_partial(left)?;

    let right_result = parse_partial(right)?;

    let mut comparators = Vec::new();
    if !is_wildcard(&left_result.major_str) {
        // `MAJOR.*.*-...` gives us `>=MAJOR.0.0 ...`
        comparators.push(VersionComparator {
            operator: ComparatorOperator::RangeGreaterThanEqual,
            operand: left_result.version,
        });
    }

    if !is_wildcard(&right_result.major_str) {
        let operator;
        let mut operand = right_result.version;

        if is_wildcard(&right_result.minor_str) {
            // `...-MAJOR.*.*` gives us `... <(MAJOR+1).0.0`
            operand = operand.increment_major();
            operator = ComparatorOperator::RangeLessThan;
        } else if is_wildcard(&right_result.patch_str) {
            // `...-MAJOR.MINOR.*` gives us `... <MAJOR.(MINOR+1).0`
            operand = operand.increment_minor();
            operator = ComparatorOperator::RangeLessThan;
        } else {
            // `...-MAJOR.MINOR.PATCH` gives us `... <=MAJOR.MINOR.PATCH`
            operator = ComparatorOperator::RangeLessThanEqual;
        }

        comparators.push(VersionComparator { operator, operand });
    }

    Some(comparators)
}

struct PartialVersion {
    version: Version,
    major_str: String,
    minor_str: String,
    patch_str: String,
}

// Produces a "partial" version
fn parse_partial(text: &str) -> Option<PartialVersion> {
    let m = partial_reg_exp_find_string_submatch(text)?;

    let major_str = m[1];
    let mut minor_str = m[2];
    let mut patch_str = m[3];
    let prerelease_str = m[4];
    let build_str = m[5];

    if minor_str.is_empty() {
        minor_str = "*";
    }
    if patch_str.is_empty() {
        patch_str = "*";
    }

    let major_numeric;
    let minor_numeric;
    let patch_numeric;

    if is_wildcard(major_str) {
        major_numeric = 0;
        minor_numeric = 0;
        patch_numeric = 0;
    } else {
        major_numeric = get_uint_component(major_str).ok()?;

        if is_wildcard(minor_str) {
            minor_numeric = 0;
            patch_numeric = 0;
        } else {
            minor_numeric = get_uint_component(minor_str).ok()?;

            if is_wildcard(patch_str) {
                patch_numeric = 0;
            } else {
                patch_numeric = get_uint_component(patch_str).ok()?;
            }
        }
    }

    let mut prerelease = Vec::new();
    if !prerelease_str.is_empty() {
        prerelease = prerelease_str.split('.').map(str::to_string).collect();
    }

    let mut build = Vec::new();
    if !build_str.is_empty() {
        build = build_str.split('.').map(str::to_string).collect();
    }

    let result = PartialVersion {
        version: Version {
            major: major_numeric,
            minor: minor_numeric,
            patch: patch_numeric,
            prerelease,
            build,
        },
        major_str: major_str.to_string(),
        minor_str: minor_str.to_string(),
        patch_str: patch_str.to_string(),
    };

    Some(result)
}

fn parse_comparator(op: &str, text: &str) -> Option<Vec<VersionComparator>> {
    let result = parse_partial(text)?;

    let mut comparators_result = Vec::new();

    if !is_wildcard(&result.major_str) {
        match op {
            "~" => {
                let first = VersionComparator {
                    operator: ComparatorOperator::RangeGreaterThanEqual,
                    operand: result.version.clone(),
                };

                let second_version = if is_wildcard(&result.minor_str) {
                    result.version.increment_major()
                } else {
                    result.version.increment_minor()
                };

                let second = VersionComparator {
                    operator: ComparatorOperator::RangeLessThan,
                    operand: second_version,
                };
                comparators_result = vec![first, second];
            }
            "^" => {
                let first = VersionComparator {
                    operator: ComparatorOperator::RangeGreaterThanEqual,
                    operand: result.version.clone(),
                };

                let second_version = if result.version.major > 0 || is_wildcard(&result.minor_str) {
                    result.version.increment_major()
                } else if result.version.minor > 0 || is_wildcard(&result.patch_str) {
                    result.version.increment_minor()
                } else {
                    result.version.increment_patch()
                };
                let second = VersionComparator {
                    operator: ComparatorOperator::RangeLessThan,
                    operand: second_version,
                };
                comparators_result = vec![first, second];
            }
            "<" | ">=" => {
                let operator = if op == "<" {
                    ComparatorOperator::RangeLessThan
                } else {
                    ComparatorOperator::RangeGreaterThanEqual
                };
                let mut version = result.version.clone();
                if is_wildcard(&result.minor_str) || is_wildcard(&result.patch_str) {
                    version.prerelease = vec!["0".to_string()];
                }
                comparators_result = vec![VersionComparator { operator, operand: version }];
            }
            "<=" | ">" => {
                let mut operator = if op == "<=" {
                    ComparatorOperator::RangeLessThanEqual
                } else {
                    ComparatorOperator::RangeGreaterThan
                };
                let mut version = result.version.clone();
                if is_wildcard(&result.minor_str) {
                    if operator == ComparatorOperator::RangeLessThanEqual {
                        operator = ComparatorOperator::RangeLessThan;
                    } else {
                        operator = ComparatorOperator::RangeGreaterThanEqual;
                    }

                    version = version.increment_major();
                    version.prerelease = vec!["0".to_string()];
                } else if is_wildcard(&result.patch_str) {
                    if operator == ComparatorOperator::RangeLessThanEqual {
                        operator = ComparatorOperator::RangeLessThan;
                    } else {
                        operator = ComparatorOperator::RangeGreaterThanEqual;
                    }

                    version = version.increment_minor();
                    version.prerelease = vec!["0".to_string()];
                }

                comparators_result = vec![VersionComparator { operator, operand: version }];
            }
            "=" | "" => {
                // normalize empty string to `=`
                let operator = ComparatorOperator::RangeEqual;

                if is_wildcard(&result.minor_str) || is_wildcard(&result.patch_str) {
                    let original_version = &result.version;

                    let mut first_version = original_version.clone();
                    first_version.prerelease = vec!["0".to_string()];

                    let mut second_version = if is_wildcard(&result.minor_str) {
                        original_version.increment_major()
                    } else {
                        original_version.increment_minor()
                    };
                    second_version.prerelease = vec!["0".to_string()];

                    comparators_result = vec![
                        VersionComparator {
                            operator: ComparatorOperator::RangeGreaterThanEqual,
                            operand: first_version,
                        },
                        VersionComparator {
                            operator: ComparatorOperator::RangeLessThan,
                            operand: second_version,
                        },
                    ];
                } else {
                    comparators_result =
                        vec![VersionComparator { operator, operand: result.version }];
                }
            }
            _ => panic!("Unexpected operator: {op}"),
        }
    } else if op == "<" || op == ">" {
        comparators_result = vec![
            // < 0.0.0-0
            VersionComparator {
                operator: ComparatorOperator::RangeLessThan,
                operand: VERSION_ZERO.clone(),
            },
        ];
    }

    Some(comparators_result)
}

fn is_wildcard(text: &str) -> bool {
    text == "*" || text == "x" || text == "X"
}
