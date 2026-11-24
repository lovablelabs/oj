//! camelCase -> dash-case plus the dev-classname sanitizer.

use std::borrow::Cow;

/// Equivalent of `str.replace(/(^|[a-z])([A-Z])/g, '$1-$2').toLowerCase()`.
// parity: babel-plugin src/shared/utils/dashify.js
pub fn dashify(s: &str) -> String {
    if s.is_ascii() {
        // `toLowerCase()` over ASCII is the per-byte fold: one pass, one buffer.
        let mut out = String::with_capacity(s.len() + 4);
        let mut prev_is_lower = false;
        for (i, b) in s.bytes().enumerate() {
            if b.is_ascii_uppercase() && (i == 0 || prev_is_lower) {
                out.push('-');
            }
            prev_is_lower = b.is_ascii_lowercase();
            out.push(char::from(b.to_ascii_lowercase()));
        }
        return out;
    }
    dashify_unicode(s)
}

fn dashify_unicode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let mut prev_is_ascii_lower = false;
    for (i, c) in s.chars().enumerate() {
        if c.is_ascii_uppercase() && (i == 0 || prev_is_ascii_lower) {
            out.push('-');
        }
        prev_is_ascii_lower = c.is_ascii_lowercase();
        out.push(c);
    }
    out.to_lowercase()
}

/// Custom properties keep their exact spelling; everything else is dashified.
// parity: babel-plugin src/shared/utils/convert-to-className.js:40
pub fn dashed_key(key: &str) -> Cow<'_, str> {
    if key.starts_with("--") {
        return Cow::Borrowed(key);
    }
    // ASCII lower/digit/'-'/'_' strings are dashify fixed points (no dash
    // insertion, toLowerCase identity): skip the two-string rebuild.
    if key
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Cow::Borrowed(key);
    }
    Cow::Owned(dashify(key))
}

/// Equivalent of `className.replace(/[^.a-zA-Z0-9_-]/g, '')`.
// parity: babel-plugin src/utils/dev-classname.js
pub fn sanitize_dev_class_name(class_name: &str) -> String {
    class_name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dashify_basics() {
        assert_eq!(dashify("backgroundColor"), "background-color");
        // leading uppercase gets a leading dash (the `^` alternative)
        assert_eq!(dashify("WebkitTransform"), "-webkit-transform");
        // consecutive capitals only split after a lowercase
        assert_eq!(dashify("backgroundURL"), "background-url");
        assert_eq!(dashify("notARealProp"), "not-areal-prop");
    }

    #[test]
    fn ascii_branch_matches_the_unicode_one() {
        for input in [
            "",
            "a",
            "A",
            "aB",
            "aBC",
            "ABc",
            "a1B",
            "a_B",
            "a-B",
            "aB1C",
            "--aB",
            "-aB",
            "zZ",
            "Zz",
            "a B",
            "a\tB",
            "msTransform",
            "MozOsxFontSmoothing",
            "x{Y}",
            "0A",
            "aA0aA",
        ] {
            assert_eq!(dashify(input), dashify_unicode(input), "{input:?}");
        }
        assert_eq!(dashify("a1B"), "a1b");
        assert_eq!(dashify("\u{e9}aB"), "\u{e9}a-b");
        assert_eq!(dashify("a\u{c9}B"), "a\u{e9}b");
        assert_eq!(dashify("\u{130}aB"), "i\u{307}a-b");
    }

    #[test]
    fn custom_property_passthrough() {
        assert_eq!(dashed_key("--customProp"), "--customProp");
        assert_eq!(dashify("--customProp"), "--custom-prop");
        assert_eq!(dashed_key("borderTopColor"), "border-top-color");
    }

    #[test]
    fn sanitizer_strips_disallowed() {
        assert_eq!(
            sanitize_dev_class_name("My File__s.a b?c\u{2192}d"),
            "MyFile__s.abcd"
        );
        assert_eq!(sanitize_dev_class_name(""), "");
    }
}
