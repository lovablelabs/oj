//! JS style value → final CSS string value.
// parity: babel-plugin/src/shared/utils/transform-value.js

use crate::jsrt::{js_math_round, write_js_number};
use crate::shared::css_value::{Kind, parse};
use crate::shared::normalize_value::{CssValueError, normalize_owned, normalize_value};

pub fn transform_value_str(
    key: &str,
    value: &str,
    font_size_px_to_rem: bool,
) -> Result<String, CssValueError> {
    if let Some(out) = transform_content_like(key, value) {
        return Ok(out);
    }
    normalize_value(value, key, font_size_px_to_rem)
}

pub fn transform_value_num(
    key: &str,
    value: f64,
    font_size_px_to_rem: bool,
) -> Result<String, CssValueError> {
    let rounded = js_math_round(value * 10000.0) / 10000.0;
    let suffix = get_number_suffix(key);
    let mut as_string = String::with_capacity(24 + suffix.len());
    push_js_number(&mut as_string, rounded);
    as_string.push_str(suffix);
    if let Some(out) = transform_content_like(key, &as_string) {
        return Ok(out);
    }
    normalize_owned(as_string, key, font_size_px_to_rem)
}

// `String(n)`: an integer below 1e15 prints as its plain decimal digits
// (`-0` as "0"), which skips the shortest-round-trip formatter.
fn push_js_number(out: &mut String, x: f64) {
    if x.fract() != 0.0 || x.abs() >= 1e15 {
        return write_js_number(x, out);
    }
    let n = x as i64;
    let mut digits = [0u8; 16];
    let mut at = digits.len();
    let mut rest = n.unsigned_abs();
    loop {
        at -= 1;
        digits[at] = b'0' + (rest % 10) as u8;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    if n < 0 {
        out.push('-');
    }
    out.push_str(str::from_utf8(&digits[at..]).expect("ascii digits"));
}

// content/hyphenateCharacter auto-quoting; short-circuits normalization.
fn transform_content_like(key: &str, value: &str) -> Option<String> {
    if key != "content" && key != "hyphenateCharacter" && key != "hyphenate-character" {
        return None;
    }
    let val = value.trim_matches(is_js_whitespace);

    const CSS_CONTENT_FUNCTIONS: [&str; 7] = [
        "attr(",
        "counter(",
        "counters(",
        "url(",
        "linear-gradient(",
        "image-set(",
        "var(--",
    ];
    let is_css_function = CSS_CONTENT_FUNCTIONS.iter().any(|f| val.contains(f));

    if is_css_function || is_content_keyword(val) || is_quoted_content_list(val) {
        Some(val.to_string())
    } else {
        Some(quote_content(val))
    }
}

fn is_content_keyword(val: &str) -> bool {
    matches!(
        val,
        "normal"
            | "none"
            | "open-quote"
            | "close-quote"
            | "no-open-quote"
            | "no-close-quote"
            | "inherit"
            | "initial"
            | "revert"
            | "revert-layer"
            | "unset"
    )
}

// A value that already is a content list: at least one closed string, and
// nothing besides closed strings, closed functions and content keywords.
fn is_quoted_content_list(val: &str) -> bool {
    // A string node starts at a quote byte: without one the list has none.
    if memchr::memchr2(b'"', b'\'', val.as_bytes()).is_none() {
        return false;
    }
    let nodes = parse(val);
    let mut content = nodes
        .iter()
        .filter(|node| !matches!(node.kind, Kind::Space | Kind::Div));
    content
        .clone()
        .any(|node| node.kind == Kind::Str && !node.unclosed)
        && content.all(|node| match node.kind {
            Kind::Str | Kind::Func => !node.unclosed,
            Kind::Word => is_content_keyword(&node.value),
            _ => false,
        })
}

// Wraps bare text as a CSS string: an existing escape (backslash plus the
// character or CRLF after it) stays as written, a bare `"` or trailing `\`
// is escaped, and a line break becomes the `\A ` escape.
fn quote_content(val: &str) -> String {
    let mut out = String::with_capacity(val.len() + 2);
    out.push('"');
    let mut chars = val.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next() {
                Some(next) => {
                    out.push('\\');
                    out.push(next);
                    if next == '\r' && chars.peek() == Some(&'\n') {
                        out.push('\n');
                        chars.next();
                    }
                }
                None => out.push_str("\\\\"),
            },
            '"' => out.push_str("\\\""),
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push_str("\\A ");
            }
            '\n' | '\u{000C}' => out.push_str("\\A "),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

// JS String.prototype.trim character set (differs from Rust char::is_whitespace
// on U+FEFF and U+0085).
fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

pub fn get_number_suffix(key: &str) -> &'static str {
    if is_unitless_number_property(key) || key.starts_with("--") {
        return "";
    }
    match key {
        "animationDelay" | "animationDuration" | "transitionDelay" | "transitionDuration"
        | "voiceDuration" => "ms",
        _ => "px",
    }
}

fn is_unitless_number_property(key: &str) -> bool {
    matches!(
        key,
        "WebkitLineClamp"
            | "animationIterationCount"
            | "aspectRatio"
            | "borderImageOutset"
            | "borderImageSlice"
            | "borderImageWidth"
            | "counterSet"
            | "counterReset"
            | "columnCount"
            | "flex"
            | "flexGrow"
            | "flexShrink"
            | "flexOrder"
            | "gridRow"
            | "gridRowStart"
            | "gridRowEnd"
            | "gridColumn"
            | "gridColumnStart"
            | "gridColumnEnd"
            | "gridArea"
            | "fontSizeAdjust"
            | "fontWeight"
            | "hyphenateLimitChars"
            | "lineClamp"
            | "lineHeight"
            | "maskBorderOutset"
            | "maskBorderSlice"
            | "maskBorderWidth"
            | "opacity"
            | "order"
            | "orphans"
            | "tabSize"
            | "widows"
            | "zIndex"
            | "fillOpacity"
            | "floodOpacity"
            | "rotate"
            | "scale"
            | "shapeImageThreshold"
            | "stopOpacity"
            | "strokeDasharray"
            | "strokeDashoffset"
            | "strokeMiterlimit"
            | "strokeOpacity"
            | "strokeWidth"
            | "mathDepth"
            | "zoom"
    )
}

// Also exercised end-to-end by tests/values.rs against oracle pins.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_suffix_rules() {
        assert_eq!(get_number_suffix("width"), "px");
        assert_eq!(get_number_suffix("opacity"), "");
        assert_eq!(get_number_suffix("--anything"), "");
        assert_eq!(get_number_suffix("transitionDuration"), "ms");
        // dashed form is not in the ms table
        assert_eq!(get_number_suffix("transition-duration"), "px");
    }

    #[test]
    fn negative_zero_prints_unsigned() {
        // String(-0) is "0" in JS; the pipeline depends on ryu-js matching that.
        assert_eq!(crate::jsrt::js_number_to_string(-0.0), "0");
        assert_eq!(transform_value_num("width", -0.0, false).unwrap(), "0");
    }

    #[test]
    fn content_wraps_number_output() {
        assert_eq!(
            transform_value_num("content", 5.0, false).unwrap(),
            "\"5px\""
        );
        assert_eq!(
            transform_value_str("content", "hello", false).unwrap(),
            "\"hello\""
        );
        assert_eq!(
            transform_value_str("content", "attr(href)", false).unwrap(),
            "attr(href)"
        );
    }

    // Each expectation is the @stylexjs/babel-plugin 0.19.2 output pinned in
    // testdata/pins/values.json.
    #[test]
    fn content_text_is_escaped_into_one_string() {
        for (input, expected) in [
            ("a\"b\"c", r#""a\"b\"c""#),
            ("\"abc", r#""\"abc""#),
            (r#"a\"b"#, r#""a\"b""#),
            ("a\\", r#""a\\""#),
            ("line1\nline2", r#""line1\A line2""#),
            ("a\r\nb", r#""a\A b""#),
            (r#""a" foo"#, r#""\"a\" foo""#),
        ] {
            assert_eq!(
                transform_value_str("content", input, false).unwrap(),
                expected,
                "content: {input:?}"
            );
        }
    }

    #[test]
    fn quoted_content_lists_pass_through() {
        for input in [r#""a" "b""#, r#""a" none"#, "'a'", r#""a" / "alt""#] {
            assert_eq!(transform_value_str("content", input, false).unwrap(), input);
            assert_eq!(
                transform_value_str("hyphenateCharacter", input, false).unwrap(),
                input
            );
        }
    }

    #[test]
    fn font_size_adjust_is_unitless() {
        assert_eq!(get_number_suffix("fontSizeAdjust"), "");
        assert_eq!(
            transform_value_num("fontSizeAdjust", 0.5, false).unwrap(),
            ".5"
        );
    }

    #[test]
    fn push_js_number_prints_like_number_to_string() {
        for x in [
            0.0,
            -0.0,
            1.0,
            -1.0,
            9.0,
            10.0,
            16.0,
            -400.0,
            1234567890.0,
            999_999_999_999_999.0,
            -999_999_999_999_999.0,
            1e15,
            -1e15,
            1e21,
            1e300,
            0.5,
            -0.5,
            0.1 + 0.2,
            1e-7,
            5e-324,
            f64::MAX,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NAN,
        ] {
            let mut out = String::from("<");
            push_js_number(&mut out, x);
            assert_eq!(out[1..], crate::jsrt::js_number_to_string(x), "{x:?}");
        }
    }
}
