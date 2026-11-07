//! camelCase -> dash-case plus the dev-classname sanitizer.

use std::borrow::Cow;

/// Equivalent of `str.replace(/(^|[a-z])([A-Z])/g, '$1-$2').toLowerCase()`.
// parity: babel-plugin src/shared/utils/dashify.js
pub fn dashify(s: &str) -> String {
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
