// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Minimal html attribute scanning shared by the build's asset walker and the
//! wasm playground, so the two never disagree on how a tag is read.

/// The value of one attribute in `tag` (the text between `<` and `>`), or None.
/// Attribute names are tokenized (whitespace-delimited, `=`-terminated) and
/// matched ASCII case-insensitively, so a `data-src` can never answer for
/// `src` and a value containing ` src=` is never misread as the attribute.
/// Values may be single-quoted, double-quoted, or bare.
pub fn html_attr<'a>(tag: &'a str, name: &str) -> Option<&'a str> {
    let bytes = tag.as_bytes();
    let mut cursor = bytes.iter().position(u8::is_ascii_whitespace)?;

    while cursor < bytes.len() {
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        let start = cursor;
        while cursor < bytes.len()
            && !bytes[cursor].is_ascii_whitespace()
            && bytes[cursor] != b'='
            && bytes[cursor] != b'/'
        {
            cursor += 1;
        }
        if cursor == start {
            cursor += 1;
            continue;
        }
        let attribute = &tag[start..cursor];
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() || bytes[cursor] != b'=' {
            continue;
        }
        cursor += 1;
        while cursor < bytes.len() && bytes[cursor].is_ascii_whitespace() {
            cursor += 1;
        }
        if cursor >= bytes.len() {
            return None;
        }
        let (value_start, value_end) = if matches!(bytes[cursor], b'\'' | b'"') {
            let quote = bytes[cursor];
            let start = cursor + 1;
            cursor = start;
            while cursor < bytes.len() && bytes[cursor] != quote {
                cursor += 1;
            }
            let end = cursor;
            cursor += usize::from(cursor < bytes.len());
            (start, end)
        } else {
            let start = cursor;
            while cursor < bytes.len() && !bytes[cursor].is_ascii_whitespace() {
                cursor += 1;
            }
            (start, cursor)
        };
        if attribute.eq_ignore_ascii_case(name) {
            return Some(&tag[value_start..value_end]);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::html_attr;

    #[test]
    fn tokenizes_names_and_never_matches_inside_values_or_data_attrs() {
        assert_eq!(html_attr(r#"<script data-src="/x" src="/y">"#, "src"), Some("/y"));
        assert_eq!(html_attr(r#"<script data-type="module" src="/y">"#, "type"), None);
        assert_eq!(html_attr(r#"<script data-cfg="a type=module b" src="/y">"#, "type"), None);
        assert_eq!(html_attr(r#"<SCRIPT TYPE="module" SRC=/y>"#, "type"), Some("module"));
        // The tag is the text between `<` and `>`: callers strip the closing
        // bracket, or a bare value at the end of the tag would swallow it.
        assert_eq!(html_attr("<link rel='stylesheet' href=app.css", "href"), Some("app.css"));
    }
}
