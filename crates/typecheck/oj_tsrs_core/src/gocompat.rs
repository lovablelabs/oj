// Compatibility for Go-style strings used in errors and baselines. The legacy profiles retain
// existing callers' escaping differences; Go uses strconv.Quote's printable tables.

use std::fmt::Write;

mod isprint_generated;
use isprint_generated::{IS_NOT_PRINT16, IS_NOT_PRINT32, IS_PRINT16, IS_PRINT32};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QuoteStyle {
    /// Go `strconv.Quote`, using its pinned Unicode printable tables.
    Go,
    /// IPC headers and content-mapper errors: escape Unicode whitespace and selected format characters.
    Header,
    /// API transport errors: hex escapes for ASCII controls other than tab and line endings.
    Api,
    /// LSP errors: replace invalid UTF-8, retaining other Unicode characters.
    Lsp,
    /// Mapper harness errors: escape Unicode controls and whitespace.
    MapperTest,
    /// Fourslash baselines: escape Unicode controls, retaining Unicode whitespace.
    Fourslash,
}
fn escaped(c: char, style: QuoteStyle) -> bool {
    match style {
        QuoteStyle::Go => !is_print(c as u32),
        QuoteStyle::Header => {
            c != ' '
                && (c.is_control()
                    || c.is_whitespace()
                    || matches!(c,'\u{ad}'|'\u{200b}'..='\u{200f}'|'\u{202a}'..='\u{202e}'|'\u{2060}'..='\u{2064}'|'\u{feff}'))
        }
        QuoteStyle::MapperTest => c.is_control() || (c.is_whitespace() && c != ' '),
        QuoteStyle::Fourslash => c.is_control(),
        _ => false,
    }
}

// Go strconv/quote.go:518.
fn is_print(r: u32) -> bool {
    if r <= 0xff {
        if (0x20..=0x7e).contains(&r) {
            return true;
        }
        if (0xa1..=0xff).contains(&r) {
            return r != 0xad;
        }
        return false;
    }
    if r < 1 << 16 {
        let rr = r as u16;
        let i = IS_PRINT16.partition_point(|&value| value < rr);
        if i >= IS_PRINT16.len() || rr < IS_PRINT16[i & !1] || IS_PRINT16[i | 1] < rr {
            return false;
        }
        return IS_NOT_PRINT16.binary_search(&rr).is_err();
    }
    let i = IS_PRINT32.partition_point(|&value| value < r);
    if i >= IS_PRINT32.len() || r < IS_PRINT32[i & !1] || IS_PRINT32[i | 1] < r {
        return false;
    }
    if r >= 0x20000 {
        return true;
    }
    IS_NOT_PRINT32.binary_search(&((r - 0x10000) as u16)).is_err()
}
fn push_char(out: &mut String, c: char, style: QuoteStyle) {
    match c {
        '"' => out.push_str("\\\""),
        '\\' => out.push_str("\\\\"),
        '\n' => out.push_str("\\n"),
        '\r' => out.push_str("\\r"),
        '\t' => out.push_str("\\t"),
        '\x07' if style != QuoteStyle::Api => out.push_str("\\a"),
        '\x08' if style != QuoteStyle::Api => out.push_str("\\b"),
        '\x0c' if style != QuoteStyle::Api => out.push_str("\\f"),
        '\x0b' if style != QuoteStyle::Api => out.push_str("\\v"),
        c if (c as u32) < 0x20 || c == '\x7f' => {
            let _ = write!(out, "\\x{:02x}", c as u32);
        }
        c if escaped(c, style) => {
            if style == QuoteStyle::MapperTest || (c as u32) < 0x10000 {
                let _ = write!(out, "\\u{:04x}", c as u32);
            } else {
                let _ = write!(out, "\\U{:08x}", c as u32);
            }
        }
        c => out.push(c),
    }
}
pub fn quote(b: &[u8], style: QuoteStyle) -> String {
    let mut out = String::with_capacity(b.len() + 2);
    out.push('"');
    if style == QuoteStyle::Lsp {
        for c in crate::utf8::from_utf8_lossy(b).chars() {
            push_char(&mut out, c, style);
        }
    } else {
        for chunk in b.utf8_chunks() {
            for c in chunk.valid().chars() {
                push_char(&mut out, c, style);
            }
            for b in chunk.invalid() {
                let _ = write!(out, "\\x{b:02x}");
            }
        }
    }
    out.push('"');
    out
}
pub fn parse_int64(b: &[u8], style: QuoteStyle) -> Result<i64, String> {
    let digits = b.strip_prefix(b"+").or_else(|| b.strip_prefix(b"-")).unwrap_or(b);
    let error = if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        "invalid syntax"
    } else {
        if let Ok(n) =
            std::str::from_utf8(b).expect("validated sign and digits are ASCII").parse::<i64>()
        {
            return Ok(n);
        }
        "value out of range"
    };
    Err(format!("strconv.ParseInt: parsing {}: {error}", quote(b, style)))
}
