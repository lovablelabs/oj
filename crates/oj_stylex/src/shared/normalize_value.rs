//! Value normalization pipeline.
// parity: babel-plugin/src/shared/utils/normalize-value.js (+ normalizers/*)

use crate::jsrt::js_number_to_string;
use crate::shared::css_value::{
    Kind, Node, is_unicode_range, like_number, parse, stringify_with_capacity, unit, walk,
    walk_pruned,
};
use crate::shared::dashify::dashify;
use std::borrow::Cow;

// Local until errors.rs lands a shared error surface; Display strings for the
// two unclosed variants byte-match upstream messages.js LINT_* constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CssValueError {
    UnclosedFunction,
    UnclosedString,
    // Upstream crashes with the same TypeError on empty/whitespace-only values
    // and on mid-value '!important'; we surface one structured error for both.
    EmptyValue,
}

impl std::fmt::Display for CssValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CssValueError::UnclosedFunction => f.write_str("Rule contains an unclosed function"),
            CssValueError::UnclosedString => f.write_str("Rule contains an unclosed string"),
            CssValueError::EmptyValue => f.write_str("Cannot normalize an empty value"),
        }
    }
}

impl std::error::Error for CssValueError {}

pub fn normalize_value(
    value: &str,
    key: &str,
    font_size_px_to_rem: bool,
) -> Result<String, CssValueError> {
    match normalize_plain(value, key, font_size_px_to_rem) {
        Some(plain) => Ok(plain.into_owned()),
        None => normalize_parsed(value, key, font_size_px_to_rem),
    }
}

/// `normalize_value` over a string the caller no longer needs: a value that
/// normalizes to itself is handed back without a copy.
pub(crate) fn normalize_owned(
    value: String,
    key: &str,
    font_size_px_to_rem: bool,
) -> Result<String, CssValueError> {
    let plain = normalize_plain(&value, key, font_size_px_to_rem).map(|plain| match plain {
        Cow::Borrowed(_) => None,
        Cow::Owned(changed) => Some(changed),
    });
    match plain {
        Some(None) => Ok(value),
        Some(Some(changed)) => Ok(changed),
        None => normalize_parsed(&value, key, font_size_px_to_rem),
    }
}

// The full pipeline over the parsed node tree.
fn normalize_parsed(
    value: &str,
    key: &str,
    font_size_px_to_rem: bool,
) -> Result<String, CssValueError> {
    let mut nodes = parse(value);
    detect_unclosed(&nodes)?;
    normalize_whitespace(&mut nodes)?;
    normalize_timings(&mut nodes);
    normalize_zero_dimensions(&mut nodes, key);
    normalize_leading_zero(&mut nodes);
    normalize_quotes(&mut nodes);
    convert_camel_case_values(&mut nodes, key);
    if font_size_px_to_rem {
        convert_font_size_to_rem(&mut nodes, key);
    }
    Ok(stringify_with_capacity(&nodes, value.len()))
}

const PLAIN_SPACE: u8 = 1;
const PLAIN_STOP: u8 = 2;

// Byte classes for `normalize_plain`: whitespace (every byte the tokenizer
// treats as such, `<= 32`), and the bytes that start anything but a word —
// quotes, dividers, `(`, comments (`/`), escapes — plus `!`, since a
// mid-value `!important` word reshapes its neighbours, and every non-ASCII
// byte, so the word rules there only ever reason about ASCII.
const PLAIN_CLASS: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut b = 0;
    while b <= 32 {
        table[b] = PLAIN_SPACE;
        b += 1;
    }
    let mut b = 128;
    while b < 256 {
        table[b] = PLAIN_STOP;
        b += 1;
    }
    let stops = b"'\"/,:(\\!";
    let mut i = 0;
    while i < stops.len() {
        table[stops[i] as usize] = PLAIN_STOP;
        i += 1;
    }
    table
};

#[derive(Clone, Copy)]
struct WordRules {
    zero_dimensions: bool,
    camel_case: bool,
    font_size_rem: bool,
}

/// The pipeline's result for an ASCII value made of plain words and whitespace
/// only, computed without building the node tree. Such a value tokenizes into
/// top-level word (or unicode-range) nodes separated by space nodes, so every
/// normalizer acts on one word at a time: leading/trailing whitespace drops,
/// inner runs collapse to one space, and each word takes the per-word rewrites
/// in pipeline order. `None` sends everything else — functions, strings,
/// dividers, comments, escapes, `!important`, non-ASCII text, empty values —
/// to the parser.
fn normalize_plain<'v>(
    value: &'v str,
    key: &str,
    font_size_px_to_rem: bool,
) -> Option<Cow<'v, str>> {
    let bytes = value.as_bytes();
    let mut classes = 0u8;
    for &b in bytes {
        classes |= PLAIN_CLASS[b as usize];
    }
    if classes & PLAIN_STOP != 0 || bytes.is_empty() {
        return None;
    }
    let rules = WordRules {
        zero_dimensions: !key.starts_with("--"),
        camel_case: is_camel_case_key(key),
        font_size_rem: font_size_px_to_rem && key == "fontSize",
    };
    if classes & PLAIN_SPACE == 0 {
        return Some(match normalize_word(value, rules) {
            Some(changed) => Cow::Owned(changed),
            None => Cow::Borrowed(value),
        });
    }
    let mut out = String::with_capacity(value.len());
    let mut first = true;
    let mut pos = 0;
    while pos < bytes.len() {
        if bytes[pos] <= b' ' {
            pos += 1;
            continue;
        }
        let start = pos;
        while pos < bytes.len() && bytes[pos] > b' ' {
            pos += 1;
        }
        if !first {
            out.push(' ');
        }
        first = false;
        // Word edges sit on ASCII whitespace or the value's ends.
        let word = &value[start..pos];
        match normalize_word(word, rules) {
            Some(changed) => out.push_str(&changed),
            None => out.push_str(word),
        }
    }
    // Whitespace only: the parser path reports the empty value.
    (!first).then_some(Cow::Owned(out))
}

/// One top-level word through the per-word normalizers, in pipeline order;
/// `None` when it comes out unchanged.
fn normalize_word(word: &str, rules: WordRules) -> Option<String> {
    let bytes = word.as_bytes();
    if !like_number(bytes) {
        // The numeric normalizers need `unit()` or a `parseFloat` in [0, 1),
        // and both need the word to start a number.
        if rules.camel_case && !word.starts_with("--") && !is_unicode_range(bytes) {
            return Some(dashify(word));
        }
        return None;
    }
    let mut current: Option<String> = None;
    if let Some(next) = timing_word(word) {
        current = Some(next);
    }
    if rules.zero_dimensions
        && let Some(next) = zero_dimension_word(current.as_deref().unwrap_or(word), false)
    {
        current = Some(next.to_string());
    }
    if let Some(next) = leading_zero_word(current.as_deref().unwrap_or(word)) {
        current = Some(next);
    }
    if rules.camel_case {
        let value = current.as_deref().unwrap_or(word);
        if !value.starts_with("--") {
            current = Some(dashify(value));
        }
    }
    if rules.font_size_rem
        && let Some(next) = font_size_rem_word(current.as_deref().unwrap_or(word))
    {
        current = Some(next);
    }
    current
}

// The top level decides: input that ends inside a nested node leaves every
// enclosing function unclosed too. An unclosed function is reported ahead of
// an unclosed string wherever each sits.
fn detect_unclosed(nodes: &[Node]) -> Result<(), CssValueError> {
    let mut unclosed_string = false;
    for node in nodes {
        match node.kind {
            Kind::Func if node.unclosed => return Err(CssValueError::UnclosedFunction),
            Kind::Str => unclosed_string |= node.unclosed,
            _ => {}
        }
    }
    if unclosed_string {
        Err(CssValueError::UnclosedString)
    } else {
        Ok(())
    }
}

// parity: normalizers/whitespace.js
fn normalize_whitespace(nodes: &mut Vec<Node>) -> Result<(), CssValueError> {
    // Upstream indexes nodes[0]/nodes[last] unguarded; empty input crashes there.
    if nodes.first().ok_or(CssValueError::EmptyValue)?.kind == Kind::Space {
        nodes.remove(0);
    }
    if nodes.last().ok_or(CssValueError::EmptyValue)?.kind == Kind::Space {
        nodes.pop();
    }
    let max = nodes.len();
    for i in 0..max {
        if i >= nodes.len() {
            // Upstream's stale-length walk reads undefined here and throws
            // (mid-value '!important'); reproduce the compile failure.
            return Err(CssValueError::EmptyValue);
        }
        match nodes[i].kind {
            Kind::Space => set_single_space(&mut nodes[i].value),
            Kind::Div => normalize_div_spacing(&mut nodes[i]),
            Kind::Func => {
                nodes[i].before.clear();
                nodes[i].after.clear();
                if !is_url(&nodes[i]) {
                    whitespace_walk_nested(&mut nodes[i].nodes);
                }
            }
            Kind::Word
                if nodes[i].value == "!important" && i > 0 && nodes[i - 1].kind == Kind::Space =>
            {
                nodes.remove(i - 1);
            }
            _ => {}
        }
    }
    Ok(())
}

fn normalize_div_spacing(node: &mut Node) {
    if node.value == "," {
        node.before.clear();
        node.after.clear();
    } else {
        set_single_space(&mut node.before);
        set_single_space(&mut node.after);
    }
}

fn set_single_space(s: &mut String) {
    if s != " " {
        s.clear();
        s.push(' ');
    }
}

fn whitespace_walk_nested(nodes: &mut [Node]) {
    for node in nodes.iter_mut() {
        match node.kind {
            Kind::Space => set_single_space(&mut node.value),
            Kind::Div => normalize_div_spacing(node),
            Kind::Func => {
                node.before.clear();
                node.after.clear();
                if !is_url(node) {
                    whitespace_walk_nested(&mut node.nodes);
                }
            }
            // Nested '!important' upstream splices the TOP-level node list (a
            // latent bug never hit by valid CSS); we deliberately skip it.
            _ => {}
        }
    }
}

// The normalizers leave a `url()`'s contents as the author wrote them. No
// non-ASCII letter lowercases into "url", so the ASCII fold is exact.
fn is_url(node: &Node) -> bool {
    node.kind == Kind::Func && node.value.eq_ignore_ascii_case("url")
}

// parity: normalizers/timings.js — ms >= 10 becomes seconds
fn normalize_timings(nodes: &mut [Node]) {
    walk_pruned(nodes, &mut |node| {
        if is_url(node) {
            return false;
        }
        if node.kind == Kind::Word
            && let Some(seconds) = timing_word(&node.value)
        {
            node.value = seconds;
        }
        true
    });
}

fn timing_word(value: &str) -> Option<String> {
    let dimension = unit(value)?;
    if dimension.unit != "ms" {
        return None;
    }
    let number = js_parse_float(value)?;
    if number < 10.0 {
        return None;
    }
    Some(js_number_to_string(number / 1000.0) + "s")
}

// parity: normalizers/zero-dimensions.js. `end_function` is the furthest end
// of any function seen so far, cleared once a node starts past it; a zero
// inside a function keeps its unit.
fn normalize_zero_dimensions(nodes: &mut [Node], key: &str) {
    if key.starts_with("--") {
        return;
    }
    let mut end_function: usize = 0;
    walk_pruned(nodes, &mut |node| {
        if node.kind == Kind::Func {
            end_function = end_function.max(node.source_end_index);
        }
        if end_function > 0 && node.source_index > end_function {
            end_function = 0;
        }
        if is_url(node) {
            return false;
        }
        if node.kind == Kind::Word
            && let Some(zero) = zero_dimension_word(&node.value, end_function != 0)
            && node.value != zero
        {
            node.value.clear();
            node.value.push_str(zero);
        }
        true
    });
}

fn zero_dimension_word(value: &str, in_function: bool) -> Option<&'static str> {
    // The number must be the exact text "0", which the word then starts with.
    if value.as_bytes().first() != Some(&b'0') {
        return None;
    }
    let dimension = unit(value)?;
    if dimension.number != "0" {
        return None;
    }
    // Units compare case-insensitively; none of them is the lowercase of a
    // non-ASCII string, so the ASCII fold matches `toLowerCase()`.
    let is = |name: &str| dimension.unit.eq_ignore_ascii_case(name);
    if is("deg") || is("grad") || is("turn") || is("rad") {
        Some("0deg")
    } else if is("ms") || is("s") {
        Some("0s")
    } else if is("fr") {
        Some("0fr")
    } else if dimension.unit == "%" {
        Some("0%")
    } else if !in_function {
        Some("0")
    } else {
        None
    }
}

// parity: normalizers/leading-zero.js
fn normalize_leading_zero(nodes: &mut [Node]) {
    walk_pruned(nodes, &mut |node| {
        if is_url(node) {
            return false;
        }
        if node.kind == Kind::Word
            && let Some(stripped) = leading_zero_word(&node.value)
        {
            node.value = stripped;
        }
        true
    });
}

fn leading_zero_word(value: &str) -> Option<String> {
    // A bare "0" prints back as itself, and a number with a nonzero integer
    // part and no exponent is at least 1: neither needs the float parse.
    if value == "0" || at_least_one(value.as_bytes()) {
        return None;
    }
    let number = js_parse_float(value)?;
    if !(0.0..1.0).contains(&number) {
        return None;
    }
    let unit_str = unit(value).map(|d| d.unit).unwrap_or_default();
    Some(js_number_to_string(number).replacen("0.", ".", 1) + unit_str)
}

// `[1-9][0-9]*(\.[0-9]*)?` with no exponent after it: the `parseFloat` prefix
// is that literal, so the value is >= 1.
fn at_least_one(bytes: &[u8]) -> bool {
    if !matches!(bytes.first(), Some(b'1'..=b'9')) {
        return false;
    }
    let digits = |from: usize| {
        bytes[from..]
            .iter()
            .position(|b| !b.is_ascii_digit())
            .map_or(bytes.len(), |n| from + n)
    };
    let mut end = digits(1);
    if bytes.get(end) == Some(&b'.') {
        end = digits(end + 1);
    }
    if matches!(bytes.get(end), Some(b'e' | b'E')) {
        let mut exponent = end + 1;
        if matches!(bytes.get(exponent), Some(b'+' | b'-')) {
            exponent += 1;
        }
        if bytes.get(exponent).is_some_and(u8::is_ascii_digit) {
            return false;
        }
    }
    true
}

// parity: normalizers/quotes.js — empty strings get double quotes
fn normalize_quotes(nodes: &mut [Node]) {
    walk(nodes, &mut |node| {
        if node.kind == Kind::Str && node.value.is_empty() {
            node.quote = b'"';
        }
    });
}

fn is_camel_case_key(key: &str) -> bool {
    key == "transitionProperty" || key == "willChange"
}

// parity: normalizers/convert-camel-case-values.js — top-level words only
fn convert_camel_case_values(nodes: &mut [Node], key: &str) {
    if !is_camel_case_key(key) {
        return;
    }
    for node in nodes.iter_mut() {
        if node.kind == Kind::Word && !node.value.starts_with("--") {
            node.value = dashify(&node.value);
        }
    }
}

// parity: normalizers/font-size-px-to-rem.js — appended last, so leading zeros
// are already stripped ('.5px' → '.03125rem') and '0px' already collapsed to '0'.
fn convert_font_size_to_rem(nodes: &mut [Node], key: &str) {
    if key != "fontSize" {
        return;
    }
    walk(nodes, &mut |node| {
        if node.kind == Kind::Word
            && let Some(rem) = font_size_rem_word(&node.value)
        {
            node.value = rem;
        }
    });
}

fn font_size_rem_word(value: &str) -> Option<String> {
    let dimension = unit(value)?;
    if dimension.unit != "px" {
        return None;
    }
    let number = js_parse_float(dimension.number).unwrap_or(f64::NAN);
    Some(js_number_to_string(number / 16.0) + "rem")
}

// JS Number.parseFloat over a word token: longest valid decimal-literal
// prefix, or None for what JS reports as NaN.
pub(crate) fn js_parse_float(s: &str) -> Option<f64> {
    let b = s.as_bytes();
    let mut end = 0;
    if matches!(b.first(), Some(b'+') | Some(b'-')) {
        end = 1;
    }
    if s[end..].starts_with("Infinity") {
        let inf = if b[0] == b'-' {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
        return Some(inf);
    }
    let int_start = end;
    while b.get(end).is_some_and(|c| c.is_ascii_digit()) {
        end += 1;
    }
    if int_start == end {
        // '.' must be followed by at least one digit when there is no int part
        if b.get(end) != Some(&b'.') || !b.get(end + 1).is_some_and(|c| c.is_ascii_digit()) {
            return None;
        }
        end += 1;
        while b.get(end).is_some_and(|c| c.is_ascii_digit()) {
            end += 1;
        }
    } else if b.get(end) == Some(&b'.') {
        end += 1;
        while b.get(end).is_some_and(|c| c.is_ascii_digit()) {
            end += 1;
        }
    }
    if matches!(b.get(end), Some(b'e') | Some(b'E')) {
        let mut k = end + 1;
        if matches!(b.get(k), Some(b'+') | Some(b'-')) {
            k += 1;
        }
        let exp_start = k;
        while b.get(k).is_some_and(|c| c.is_ascii_digit()) {
            k += 1;
        }
        if k > exp_start {
            end = k;
        }
    }
    s[..end].parse::<f64>().ok()
}

// Also exercised end-to-end by tests/values.rs against oracle pins.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_messages_match_upstream() {
        assert_eq!(
            normalize_value("calc(", "width", false),
            Err(CssValueError::UnclosedFunction)
        );
        assert_eq!(
            normalize_value("\"abc", "width", false),
            Err(CssValueError::UnclosedString)
        );
        assert_eq!(
            CssValueError::UnclosedFunction.to_string(),
            "Rule contains an unclosed function"
        );
        assert_eq!(
            CssValueError::UnclosedString.to_string(),
            "Rule contains an unclosed string"
        );
    }

    // Each expectation is the @stylexjs/babel-plugin 0.19.2 output pinned in
    // testdata/pins/values.json.
    #[test]
    fn url_contents_are_left_alone() {
        for (key, input, expected) in [
            ("backgroundImage", "url( a  b.png )", "url(a  b.png)"),
            ("backgroundImage", "url(0.5px)", "url(0.5px)"),
            ("backgroundImage", "url(1000ms)", "url(1000ms)"),
            (
                "backgroundImage",
                "URL( \"a b\" 0.50px )",
                "URL(\"a b\" 0.50px)",
            ),
            ("background", "url(a) 0px 0.5px", "url(a) 0 .5px"),
        ] {
            assert_eq!(
                normalize_value(input, key, false).unwrap(),
                expected,
                "{key}: {input}"
            );
        }
    }

    #[test]
    fn zero_keeps_its_unit_only_inside_a_function() {
        for (key, input, expected) in [
            (
                "margin",
                "calc(calc(0px) + 0px) 0px",
                "calc(calc(0px) + 0px) 0",
            ),
            (
                "margin",
                "a(b(0px) 0px) 0px c(0px) 0px",
                "a(b(0px) 0px) 0 c(0px) 0",
            ),
            ("width", "0PX", "0"),
            ("rotate", "0DEG", "0deg"),
            ("transitionDuration", "0Ms", "0s"),
            ("gridTemplateColumns", "0FR 1fr", "0fr 1fr"),
        ] {
            assert_eq!(
                normalize_value(input, key, false).unwrap(),
                expected,
                "{key}: {input}"
            );
        }
    }

    #[test]
    fn mid_value_important_reproduces_upstream_throw() {
        assert_eq!(
            normalize_value("a !important b", "width", false),
            Err(CssValueError::EmptyValue)
        );
        assert_eq!(
            normalize_value("1px !important 2px", "margin", false),
            Err(CssValueError::EmptyValue)
        );
        assert_eq!(
            normalize_value("1px !important !important", "width", false),
            Err(CssValueError::EmptyValue)
        );
        // trailing '!important' stays legal, space removed
        assert_eq!(
            normalize_value("1px !important", "margin", false).unwrap(),
            "1px!important"
        );
        assert_eq!(
            normalize_value("!important 1px", "width", false).unwrap(),
            "!important 1px"
        );
    }

    #[test]
    fn empty_and_whitespace_only_error() {
        assert_eq!(
            normalize_value("", "width", false),
            Err(CssValueError::EmptyValue)
        );
        assert_eq!(
            normalize_value(" ", "width", false),
            Err(CssValueError::EmptyValue)
        );
    }

    #[test]
    fn js_parse_float_prefix_semantics() {
        assert_eq!(js_parse_float("500ms"), Some(500.0));
        assert_eq!(js_parse_float(".5em"), Some(0.5));
        assert_eq!(js_parse_float("+.5"), Some(0.5));
        assert_eq!(js_parse_float("-0px"), Some(-0.0));
        assert!(js_parse_float("-0px").unwrap().is_sign_negative());
        assert_eq!(js_parse_float("5.e3"), Some(5000.0));
        assert_eq!(js_parse_float("5e"), Some(5.0));
        assert_eq!(js_parse_float("5e+"), Some(5.0));
        assert_eq!(js_parse_float("1e-7px"), Some(1e-7));
        assert_eq!(js_parse_float("Infinityms"), Some(f64::INFINITY));
        assert_eq!(js_parse_float("-Infinity"), Some(f64::NEG_INFINITY));
        assert_eq!(js_parse_float("solid"), None);
        assert_eq!(js_parse_float(".e3"), None);
        assert_eq!(js_parse_float("0x10"), Some(0.0));
    }

    fn rem(value: &str) -> String {
        normalize_value(value, "fontSize", true).unwrap()
    }

    #[test]
    fn font_size_px_to_rem_divides_by_sixteen() {
        assert_eq!(rem("24px"), "1.5rem");
        assert_eq!(rem("+24px"), "1.5rem");
        assert_eq!(rem("-24px"), "-1.5rem");
        assert_eq!(rem("24px 12px"), "1.5rem 0.75rem");
        assert_eq!(rem("1.25rem"), "1.25rem");
        assert_eq!(rem("inherit"), "inherit");
        // Off by default, and gated on the exact camelCase key.
        assert_eq!(normalize_value("24px", "fontSize", false).unwrap(), "24px");
        for key in ["font-size", "fontSizeAdjust", "--fontSize", "font"] {
            assert_eq!(normalize_value("24px", key, true).unwrap(), "24px", "{key}");
        }
    }

    #[test]
    fn font_size_px_to_rem_runs_after_the_other_normalizers() {
        // leading-zero already stripped: the rem result keeps the '0.' form.
        assert_eq!(rem("0.5px"), "0.03125rem");
        // zero-dimensions already collapsed '0px' to a unitless word.
        assert_eq!(rem("0px"), "0");
        assert_eq!(normalize_value("0", "fontSize", true).unwrap(), "0");
        assert_eq!(rem("0x10px"), "0");
    }

    #[test]
    fn font_size_px_to_rem_number_formatting_matches_js() {
        assert_eq!(rem("-0px"), "0rem");
        assert_eq!(rem("5e-324px"), "0rem");
        assert_eq!(rem("1e-7px"), "6.25e-9rem");
        assert_eq!(rem("1.6e-6px"), "1e-7rem");
        assert_eq!(rem("1e-6px"), "6.25e-8rem");
        assert_eq!(rem("0.000001px"), "6.25e-8rem");
        assert_eq!(rem("0.0000001px"), "6.25e-9rem");
        assert_eq!(rem("1e21px"), "62500000000000000000rem");
        assert_eq!(rem("1e300px"), "6.25e+298rem");
        assert_eq!(
            rem("1.7976931348623157e308px"),
            "1.1235582092889473e+307rem"
        );
        assert_eq!(
            rem("123456789012345678901234567890px"),
            "7.716049313271605e+27rem"
        );
        assert_eq!(rem("0.30000000000000004px"), "0.018750000000000003rem");
    }

    #[test]
    fn font_size_px_to_rem_walks_functions_and_skips_non_px_words() {
        assert_eq!(rem("calc(100% - 24px)"), "calc(100% - 1.5rem)");
        assert_eq!(rem("clamp(12px, 2vw, 24px)"), "clamp(0.75rem,2vw,1.5rem)");
        assert_eq!(rem("url(24px)"), "url(1.5rem)");
        assert_eq!(rem("attr(24px)"), "attr(1.5rem)");
        assert_eq!(rem("'24px'"), "'24px'");
        // Unit match is exact and lowercase; malformed words have no unit().
        for value in [
            "1PX",
            "1Px",
            "px",
            "-px",
            ".px",
            "1.px",
            "--24px",
            "24pxx",
            "24 px",
            "Infinitypx",
            "NaNpx",
            "24px!important",
        ] {
            assert_eq!(rem(value), value, "{value}");
        }
    }

    #[test]
    fn camel_case_values_dashify_for_gated_keys_only() {
        assert_eq!(
            normalize_value("marginTop, opacity", "transitionProperty", false).unwrap(),
            "margin-top,opacity"
        );
        assert_eq!(
            normalize_value("WebkitFilter", "willChange", false).unwrap(),
            "-webkit-filter"
        );
        assert_eq!(
            normalize_value("--customProp", "willChange", false).unwrap(),
            "--customProp"
        );
        assert_eq!(
            normalize_value("marginTop", "color", false).unwrap(),
            "marginTop"
        );
    }

    // The fast path must be indistinguishable from the node-tree pipeline it
    // skips, so every value it accepts is replayed through that pipeline.
    #[test]
    fn plain_fast_path_agrees_with_the_parsed_pipeline() {
        #[rustfmt::skip]
        const WORDS: &[&str] = &[
            "0", "00", "0.0", "-0", "+0", "0px", "0PX", "0Px", "-0px", "+0em", "0%", "0deg",
            "0DEG", "0rad", "0turn", "0grad", "0s", "0ms", "0MS", "0fr", "0x", "0x10", "0e3",
            "0e3px", "0.0px", ".0px", "0.px", "1", "1.0", "1.", "01", "0.5", ".5", "-.5",
            "+.5", "-0.5", "+0.5", "00.5", "0.50", "0.5px", "0.5PX", "0.999", "1e-7", "1e-7px",
            "1e3", "1E3", "5e", "5e+", "5e-", "0.5e1", "0.05e3", "1e-3rem", "9ms", "10ms",
            "10MS", "10Ms", "500ms", "-500ms", "+500ms", "1500ms", "0.5ms", "1e3ms", "1e1ms",
            "9.99ms", "500msx", "10s", "0.5s", "16px", "16PX", "14px", "1px", "1.5px", "-16px",
            ".5px", "1e1px", "16pxx", "1rem", "100%", "100", "1/2", "+", "-", ".", "+.", "-.",
            "--", "--x", "--0", "--aB", "-a", "a", "aB", "ABC", "WebkitFilter", "marginTop",
            "red", "solid", "auto", "inherit", "currentColor", "ease-in-out", "Infinity",
            "-Infinity", "NaN", "Infinityms", "e", "e3", "E3", ".e3", "px", "-px", ".px", "u",
            "U", "u+", "U+0025-00FF", "u+4??", "U+26", "u+x", "#fff", "#FFF", "#0", "%", "$x",
            "@a", "a;b", "a{b}", "[x]", "a=b", "a|b", "~", "a.b", "a+b", "a-b", "a--b", "x1",
            "1x", ")", "x)", "*", "a*b", "1.5.5px", "0.0.5px", "-+1px", "+-1px", "1e309",
            "1e309px", "12345678901234567890", "0.30000000000000004", "0.000001", "0.0000001",
            "5e-324",
        ];
        const SEPARATORS: &[&str] = &[
            " ", "  ", "\t", "\n", "\r\n", "\u{c}", "\u{b}", "\0", "\u{1f}",
        ];
        const KEYS: &[&str] = &[
            "width",
            "fontSize",
            "font-size",
            "--x",
            "transitionProperty",
            "willChange",
            "transitionDuration",
            "lineHeight",
            "unicodeRange",
        ];

        let mut accepted = 0usize;
        let mut check = |value: &str| {
            for key in KEYS {
                for px_to_rem in [false, true] {
                    let Some(plain) = normalize_plain(value, key, px_to_rem) else {
                        continue;
                    };
                    accepted += 1;
                    assert_eq!(
                        Ok(plain.as_ref()),
                        normalize_parsed(value, key, px_to_rem).as_deref(),
                        "{key}: {value:?} (px_to_rem: {px_to_rem})"
                    );
                    assert_eq!(
                        normalize_owned(value.to_string(), key, px_to_rem).as_deref(),
                        Ok(plain.as_ref()),
                    );
                }
            }
        };

        let mut value = String::new();
        for (i, first) in WORDS.iter().enumerate() {
            check(first);
            for (j, second) in WORDS.iter().enumerate() {
                let separator = SEPARATORS[(i + j) % SEPARATORS.len()];
                value.clear();
                value.push_str(first);
                value.push_str(separator);
                value.push_str(second);
                check(&value);
            }
            let lead = SEPARATORS[i % SEPARATORS.len()];
            let trail = SEPARATORS[(i + 3) % SEPARATORS.len()];
            let third = WORDS[(i * 7 + 3) % WORDS.len()];
            check(&format!("{lead}{first}{trail}"));
            check(&format!("{first}{lead}{third}{trail}{first}"));
        }
        // The corpus is only a check if the fast path takes nearly all of it.
        assert!(accepted > 300_000, "fast path accepted {accepted} values");
    }

    #[test]
    fn plain_fast_path_declines_what_needs_the_parser() {
        for value in [
            "",
            " ",
            "\t\n",
            "a, b",
            "a/b",
            "a:b",
            "rgb(0 0 0)",
            "'a'",
            "\"a\"",
            "a\\ b",
            "red !important",
            "0\u{a0}0.5px",
            "caf\u{e9}",
        ] {
            assert!(
                normalize_plain(value, "width", false).is_none(),
                "{value:?}"
            );
        }
    }
}
