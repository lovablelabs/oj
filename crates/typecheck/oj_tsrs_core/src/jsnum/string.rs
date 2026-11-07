use std::fmt;

use super::big;
use super::jsnum::*;

impl Number {
    // https://tc39.es/ecma262/2024/multipage/ecmascript-data-types-and-values.html#sec-numeric-types-number-tostring
    #[expect(
        clippy::float_cmp,
        reason = "Exact equality preserves JavaScript numeric identity; an epsilon comparison would merge distinct values."
    )]
    pub fn string(self) -> String {
        if self.is_nan() {
            return "NaN".to_string();
        } else if self.is_inf() {
            if self < Number(0.0) {
                return "-Infinity".to_string();
            }
            return "Infinity".to_string();
        }

        // Fast path: for safe integers, directly convert to string.
        if MIN_SAFE_INTEGER <= self && self <= MAX_SAFE_INTEGER {
            let i = self.0 as i64;
            if i as f64 == self.0 {
                return i.to_string();
            }
        }

        dragonbox_ecma::Buffer::new().format_finite(self.0).to_owned()
    }
}

impl fmt::Display for Number {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.string())
    }
}

// https://tc39.es/ecma262/2024/multipage/abstract-operations.html#sec-stringtonumber
pub fn from_string(s: &str) -> Number {
    // Keep JavaScript's whitespace, Infinity spelling and radix rules around
    // Rust's decimal float parser. Its accepted decimal forms already include
    // leading zeros, an omitted integer/fractional part and signed exponents.

    let s = s.trim_matches(is_str_white_space);

    match s {
        "" => return Number(0.0),
        "Infinity" | "+Infinity" => return inf(1),
        "-Infinity" => return inf(-1),
        _ => {}
    }

    if let Some(result) =
        s.bytes().into_iter().find_map(|b| if is_number_byte(b) { None } else { Some(nan()) })
    {
        return result;
    }

    if let Some(n) = try_parse_int(s) {
        return n;
    }

    // Cut this off first so we can ensure -0 is returned as -0.
    let (mut s, negative) = cut_prefix(s, "-");

    if !negative {
        (s, _) = cut_prefix(s, "+");
    }

    if !s.as_bytes().first().is_some_and(|b| b.is_ascii_digit() || *b == b'.') {
        return nan();
    }

    let f = s.parse::<f64>().unwrap_or(f64::NAN);
    if f.is_nan() {
        return nan();
    }

    let sign = if negative { -1.0 } else { 1.0 };
    Number(f.copysign(sign))
}

fn cut_prefix<'a>(s: &'a str, prefix: &str) -> (&'a str, bool) {
    match s.strip_prefix(prefix) {
        Some(rest) => (rest, true),
        None => (s, false),
    }
}

fn is_str_white_space(r: char) -> bool {
    // This is different than stringutil.IsWhiteSpaceLike.

    // https://tc39.es/ecma262/2024/multipage/ecmascript-language-lexical-grammar.html#prod-LineTerminator
    // https://tc39.es/ecma262/2024/multipage/ecmascript-language-lexical-grammar.html#prod-WhiteSpace

    match r {
        // LineTerminator
        '\n' | '\r' | '\u{2028}' | '\u{2029}'
        // WhiteSpace
        | '\t' | '\u{000B}' | '\u{000C}' | '\u{FEFF}' => return true,
        _ => {}
    }

    // WhiteSpace (unicode.Zs)
    matches!(
        r,
        '\u{0020}' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}'
    )
}

fn try_parse_int(s: &str) -> Option<Number> {
    let mut s = s;
    let mut i: i64 = 0;
    let mut err = false;
    let mut has_int_result = false;

    if s.len() > 2 {
        let (prefix, rest) = (&s[..2], &s[2..]);
        match prefix {
            "0b" | "0B" => {
                if !is_all_binary_digits(rest) {
                    return Some(nan());
                }
                (i, err) = parse_int(rest, 2);
                has_int_result = true;
            }
            "0o" | "0O" => {
                if !is_all_octal_digits(rest) {
                    return Some(nan());
                }
                (i, err) = parse_int(rest, 8);
                has_int_result = true;
            }
            "0x" | "0X" => {
                if !is_all_hex_digits(rest) {
                    return Some(nan());
                }
                (i, err) = parse_int(rest, 16);
                has_int_result = true;
            }
            _ => {}
        }
    }

    if !has_int_result {
        // StringToNumber does not parse leading zeros as octal.
        s = trim_leading_zeros(s);
        if !is_all_digits(s) {
            return None;
        }
        (i, err) = parse_int(s, 10);
        has_int_result = true;
    }

    if has_int_result && !err {
        return Some(Number(i as f64));
    }

    // Using this to parse large integers.
    let Some(bi) = big::Int::set_string(s, 0) else {
        return Some(nan());
    };

    let f = bi.float64();
    Some(Number(f))
}

// Go's strconv.ParseInt(s, base, 64), reporting only whether it failed.
fn parse_int(s: &str, base: u32) -> (i64, bool) {
    match i64::from_str_radix(s, base) {
        Ok(i) => (i, false),
        Err(_) => (0, true),
    }
}

fn trim_leading_zeros(s: &str) -> &str {
    let mut s = s;
    if s.starts_with('0') {
        s = s.trim_start_matches('0');
        if s.is_empty() {
            return "0";
        }
    }
    s
}

fn is_all_digits(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_digit())
}

fn is_all_binary_digits(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b'0' | b'1'))
}

fn is_all_octal_digits(s: &str) -> bool {
    s.bytes().all(|b| matches!(b, b'0'..=b'7'))
}

fn is_all_hex_digits(s: &str) -> bool {
    s.bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_number_byte(b: u8) -> bool {
    if b.is_ascii_digit() {
        return true;
    }

    if (b'a'..=b'f').contains(&b) {
        return true;
    }

    if (b'A'..=b'F').contains(&b) {
        return true;
    }

    matches!(b, b'.' | b'-' | b'+' | b'x' | b'X' | b'o' | b'O')
}
