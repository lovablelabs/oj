//! Value normalization pipeline.
// parity: babel-plugin/src/shared/utils/normalize-value.js (+ normalizers/*)

use crate::jsrt::js_number_to_string;
use crate::shared::css_value::{Kind, Node, parse, stringify, unit, walk};
use crate::shared::dashify::dashify;

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
    let mut nodes = parse(value);
    detect_unclosed(&nodes, Kind::Func, CssValueError::UnclosedFunction)?;
    detect_unclosed(&nodes, Kind::Str, CssValueError::UnclosedString)?;
    normalize_whitespace(&mut nodes)?;
    normalize_timings(&mut nodes);
    normalize_zero_dimensions(&mut nodes, key);
    normalize_leading_zero(&mut nodes);
    normalize_quotes(&mut nodes);
    convert_camel_case_values(&mut nodes, key);
    if font_size_px_to_rem {
        convert_font_size_to_rem(&mut nodes, key);
    }
    Ok(stringify(&nodes))
}

fn detect_unclosed(nodes: &[Node], kind: Kind, err: CssValueError) -> Result<(), CssValueError> {
    for node in nodes {
        if node.kind == kind && node.unclosed {
            return Err(err);
        }
        if node.kind == Kind::Func {
            detect_unclosed(&node.nodes, kind, err)?;
        }
    }
    Ok(())
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
                whitespace_walk_nested(&mut nodes[i].nodes);
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
                whitespace_walk_nested(&mut node.nodes);
            }
            // Nested '!important' upstream splices the TOP-level node list (a
            // latent bug never hit by valid CSS); we deliberately skip it.
            _ => {}
        }
    }
}

// parity: normalizers/timings.js — ms >= 10 becomes seconds
fn normalize_timings(nodes: &mut [Node]) {
    walk(nodes, &mut |node| {
        if node.kind != Kind::Word {
            return;
        }
        let Some(value) = js_parse_float(&node.value) else {
            return;
        };
        let Some(dimension) = unit(&node.value) else {
            return;
        };
        if dimension.unit != "ms" || value < 10.0 {
            return;
        }
        node.value = js_number_to_string(value / 1000.0) + "s";
    });
}

// parity: normalizers/zero-dimensions.js, including its one-live-function
// endFunction bookkeeping (later sibling functions lose zero-unit protection).
fn normalize_zero_dimensions(nodes: &mut [Node], key: &str) {
    if key.starts_with("--") {
        return;
    }
    let mut end_function: usize = 0;
    walk(nodes, &mut |node| {
        if node.kind == Kind::Func && end_function == 0 {
            end_function = node.source_end_index;
        }
        if end_function > 0 && node.source_index > end_function {
            end_function = 0;
        }
        if node.kind != Kind::Word {
            return;
        }
        let Some(dimension) = unit(&node.value) else {
            return;
        };
        if dimension.number != "0" {
            return;
        }
        let unit_str = dimension.unit;
        if matches!(unit_str, "deg" | "grad" | "turn" | "rad") {
            node.value = "0deg".to_string();
        } else if matches!(unit_str, "ms" | "s") {
            node.value = "0s".to_string();
        } else if unit_str == "fr" {
            node.value = "0fr".to_string();
        } else if unit_str == "%" {
            node.value = "0%".to_string();
        } else if end_function == 0 {
            node.value = "0".to_string();
        }
    });
}

// parity: normalizers/leading-zero.js
fn normalize_leading_zero(nodes: &mut [Node]) {
    walk(nodes, &mut |node| {
        if node.kind != Kind::Word {
            return;
        }
        let Some(value) = js_parse_float(&node.value) else {
            return;
        };
        if (0.0..1.0).contains(&value) {
            let unit_str = unit(&node.value).map(|d| d.unit).unwrap_or_default();
            node.value = js_number_to_string(value).replacen("0.", ".", 1) + unit_str;
        }
    });
}

// parity: normalizers/quotes.js — empty strings get double quotes
fn normalize_quotes(nodes: &mut [Node]) {
    walk(nodes, &mut |node| {
        if node.kind == Kind::Str && node.value.is_empty() {
            node.quote = b'"';
        }
    });
}

// parity: normalizers/convert-camel-case-values.js — top-level words only
fn convert_camel_case_values(nodes: &mut [Node], key: &str) {
    if key != "transitionProperty" && key != "willChange" {
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
        if node.kind != Kind::Word {
            return;
        }
        let Some(dimension) = unit(&node.value) else {
            return;
        };
        if dimension.unit != "px" {
            return;
        }
        let number = js_parse_float(dimension.number).unwrap_or(f64::NAN);
        node.value = js_number_to_string(number / 16.0) + "rem";
    });
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
}
