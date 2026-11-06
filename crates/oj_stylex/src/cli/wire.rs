//! Result-line bytes shared by `batch`, `transform --emit json` and `serve`.
// parity: conformance/README.md § Result schema

use crate::errors::StylexError;
use crate::rules::StylexRule;

use crate::cli::classify::classify;

/// An in-progress `{"id":…,"ok":true…}` line; `finish` closes it.
pub struct OkLine(Vec<u8>);

impl OkLine {
    /// `id_json` is the already-serialized id (string ids go through
    /// [`json_str`], which matches serde_json byte for byte).
    pub fn new(id_json: &str, capacity: usize) -> Self {
        let mut buf = Vec::with_capacity(capacity + 128);
        buf.extend_from_slice(b"{\"id\":");
        buf.extend_from_slice(id_json.as_bytes());
        buf.extend_from_slice(b",\"ok\":true");
        OkLine(buf)
    }

    pub fn str(mut self, name: &str, value: &str) -> Self {
        self.key(name);
        write_json_str(&mut self.0, value);
        self
    }

    pub fn raw(mut self, name: &str, json: &str) -> Self {
        self.key(name);
        self.0.extend_from_slice(json.as_bytes());
        self
    }

    pub fn value<T: serde::Serialize>(mut self, name: &str, value: &T) -> Self {
        self.key(name);
        write_json(&mut self.0, value);
        self
    }

    pub fn rules(mut self, rules: &[StylexRule]) -> Self {
        self.key("rules");
        write_rules(&mut self.0, rules);
        self
    }

    pub fn finish(mut self) -> String {
        self.0.push(b'}');
        String::from_utf8(self.0).expect("serde_json emits valid UTF-8")
    }

    fn key(&mut self, name: &str) {
        self.0.push(b',');
        write_json_str(&mut self.0, name);
        self.0.push(b':');
    }
}

pub fn json_str(value: &str) -> String {
    let mut buf = Vec::with_capacity(value.len() + 2);
    write_json_str(&mut buf, value);
    String::from_utf8(buf).expect("serde_json emits valid UTF-8")
}

fn write_rules(buf: &mut Vec<u8>, rules: &[StylexRule]) {
    buf.push(b'[');
    for (i, rule) in rules.iter().enumerate() {
        if i > 0 {
            buf.push(b',');
        }
        buf.push(b'[');
        write_json_str(buf, &rule.class_name);
        buf.extend_from_slice(b",{\"ltr\":");
        write_json_str(buf, &rule.ltr);
        if let Some(rtl) = &rule.rtl {
            buf.extend_from_slice(b",\"rtl\":");
            write_json_str(buf, rtl);
        }
        if let Some(const_key) = &rule.const_key {
            buf.extend_from_slice(b",\"constKey\":");
            write_json_str(buf, const_key);
        }
        if let Some(const_val) = &rule.const_val {
            buf.extend_from_slice(b",\"constVal\":");
            write_json(buf, const_val);
        }
        buf.extend_from_slice(b"},");
        write_json(buf, &rule.priority);
        buf.push(b']');
    }
    buf.push(b']');
}

// A bad job line is a protocol failure, not a source-level one: it never earns
// a StyleX error class, and it must fail the differ rather than look compilable.
pub fn input_error_line(id_json: &str, detail: &str) -> String {
    format!(
        "{{\"id\":{id_json},\"ok\":false,\"errorClass\":\"unknown\",\"message\":{}}}",
        json_str(detail)
    )
}

pub fn compile_error_line(id_json: &str, root: &str, filename: &str, error: StylexError) -> String {
    // Babel prefixes every plugin error with the filename; the class regexes
    // (one is anchored) run over that prefixed message.
    let first_line = error.message.lines().next().unwrap_or_default();
    let root = root.trim_end_matches('/');
    let prefixed = format!("{root}/{filename}: {first_line}");
    let wire = StylexError::new(error.code, prefixed);
    format!(
        "{{\"id\":{id_json},\"ok\":false,\"errorClass\":{},\"message\":{}}}",
        json_str(classify(&wire)),
        json_str(&wire.message.replace(root, "<ROOT>"))
    )
}

pub fn protocol_error_line(id_json: &str, class: &str, message: &str) -> String {
    format!(
        "{{\"id\":{id_json},\"ok\":false,\"errorClass\":{},\"message\":{}}}",
        json_str(class),
        json_str(message)
    )
}

/// serde_json `serialize_str` byte-for-byte, with the clean-run skip done as
/// 8-byte SWAR checks instead of the per-byte escape table.
pub fn write_json_str(buf: &mut Vec<u8>, value: &str) {
    const LO: u64 = 0x0101_0101_0101_0101;
    const HI: u64 = 0x8080_8080_8080_8080;
    const QUOTE: u64 = LO * (b'"' as u64);
    const BACKSLASH: u64 = LO * (b'\\' as u64);
    const CTL: u64 = LO * 0x20;
    #[inline]
    fn haszero(x: u64) -> u64 {
        x.wrapping_sub(LO) & !x & HI
    }
    // Bit Twiddling hasless: bytes strictly below 0x20 (JSON control range).
    #[inline]
    fn hasless20(x: u64) -> u64 {
        x.wrapping_sub(CTL) & !x & HI
    }
    buf.push(b'"');
    let bytes = value.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i + 8 <= bytes.len() {
        let word = u64::from_le_bytes(bytes[i..i + 8].try_into().expect("8 bytes"));
        let mask = haszero(word ^ QUOTE) | haszero(word ^ BACKSLASH) | hasless20(word);
        if mask == 0 {
            i += 8;
            continue;
        }
        let hit = i + (mask.trailing_zeros() / 8) as usize;
        buf.extend_from_slice(&bytes[start..hit]);
        i = escape_one(buf, bytes, hit);
        start = i;
    }
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'"' || b == b'\\' || b < 0x20 {
            buf.extend_from_slice(&bytes[start..i]);
            i = escape_one(buf, bytes, i);
            start = i;
        } else {
            i += 1;
        }
    }
    buf.extend_from_slice(&bytes[start..]);
    buf.push(b'"');
}

/// Escapes the byte at `at` (known to need it); returns the next scan offset.
fn escape_one(buf: &mut Vec<u8>, bytes: &[u8], at: usize) -> usize {
    let b = bytes[at];
    match b {
        b'"' => buf.extend_from_slice(b"\\\""),
        b'\\' => buf.extend_from_slice(b"\\\\"),
        0x08 => buf.extend_from_slice(b"\\b"),
        0x09 => buf.extend_from_slice(b"\\t"),
        0x0a => buf.extend_from_slice(b"\\n"),
        0x0c => buf.extend_from_slice(b"\\f"),
        0x0d => buf.extend_from_slice(b"\\r"),
        _ => {
            buf.extend_from_slice(b"\\u00");
            buf.push(b"0123456789abcdef"[(b >> 4) as usize]);
            buf.push(b"0123456789abcdef"[(b & 0xf) as usize]);
        }
    }
    at + 1
}

pub fn write_json<T: serde::Serialize>(buf: &mut Vec<u8>, value: &T) {
    serde_json::to_writer(buf, value).expect("in-memory JSON write");
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn ok_line_matches_the_value_tree_bytes() {
        let rules = vec![StylexRule {
            class_name: "x1".into(),
            ltr: ".x1{color:red}".into(),
            rtl: Some(".x1{color:\"blue\"}".into()),
            const_key: None,
            const_val: Some(Box::new(json!(1.5))),
            priority: 3000.0,
        }];
        let line = OkLine::new(&json_str("a/b\n"), 0)
            .str("code", "let x = \"q\";")
            .rules(&rules)
            .finish();
        let expected = json!({
            "id": "a/b\n",
            "ok": true,
            "code": "let x = \"q\";",
            "rules": rules.iter().map(StylexRule::to_metadata_tuple).collect::<Vec<_>>(),
        })
        .to_string();
        assert_eq!(line, expected);
    }

    #[test]
    fn appended_fields_serialize_like_serde() {
        let line = OkLine::new("7", 0)
            .value("generation", &3u64)
            .value("changed", &true)
            .raw("map", "null")
            .finish();
        assert_eq!(
            line,
            r#"{"id":7,"ok":true,"generation":3,"changed":true,"map":null}"#
        );
    }

    #[test]
    fn error_lines_match_the_json_macro() {
        let detail = "no \"source\"";
        assert_eq!(
            input_error_line(&json_str("k"), detail),
            json!({ "id": "k", "ok": false, "errorClass": "unknown", "message": detail })
                .to_string()
        );
    }
}
