//! JS style value → final CSS string value.
// parity: babel-plugin/src/shared/utils/transform-value.js

use crate::jsrt::{js_math_round, js_number_to_string};
use crate::shared::normalize_value::{CssValueError, normalize_value};

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
    let as_string = js_number_to_string(rounded) + get_number_suffix(key);
    if let Some(out) = transform_content_like(key, &as_string) {
        return Ok(out);
    }
    normalize_value(&as_string, key, font_size_px_to_rem)
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
    let is_keyword = matches!(
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
    );
    let has_matching_quotes = val.matches('"').count() >= 2 || val.matches('\'').count() >= 2;

    if is_css_function || is_keyword || has_matching_quotes {
        Some(val.to_string())
    } else {
        Some(format!("\"{val}\""))
    }
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
        assert_eq!(js_number_to_string(-0.0), "0");
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
}
