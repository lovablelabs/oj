//! Go `strconv.Quote` for diagnostic strings represented as valid UTF-8.

use std::fmt::Write;

#[path = "go_quote_generated.rs"]
mod generated;

pub fn quote(text: &str) -> String {
    let mut result = String::with_capacity(text.len() + 2);
    result.push('"');
    for character in text.chars() {
        match character {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\x07' => result.push_str("\\a"),
            '\x08' => result.push_str("\\b"),
            '\x0c' => result.push_str("\\f"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\x0b' => result.push_str("\\v"),
            character => {
                let code = character as u32;
                let index = generated::PRINTABLE.partition_point(|&(lo, _)| lo <= code);
                if index != 0 && code <= generated::PRINTABLE[index - 1].1 {
                    result.push(character);
                } else if code < 0x20 || code == 0x7f {
                    write!(result, "\\x{code:02x}").unwrap();
                } else if code < 0x10000 {
                    write!(result, "\\u{code:04x}").unwrap();
                } else {
                    write!(result, "\\U{code:08x}").unwrap();
                }
            }
        }
    }
    result.push('"');
    result
}
