use std::cmp::Ordering;

fn non_finite_text(x: f64) -> Option<&'static str> {
    if x.is_nan() {
        Some("NaN")
    } else if x.is_infinite() {
        Some(if x > 0.0 { "Infinity" } else { "-Infinity" })
    } else {
        None
    }
}

pub fn js_number_to_string(x: f64) -> String {
    if let Some(text) = non_finite_text(x) {
        return text.to_string();
    }
    let mut buf = ryu_js::Buffer::new();
    buf.format(x).to_string()
}

pub fn write_js_number(x: f64, out: &mut String) {
    if let Some(text) = non_finite_text(x) {
        out.push_str(text);
        return;
    }
    let mut buf = ryu_js::Buffer::new();
    out.push_str(buf.format(x));
}

// ES Math.round: nearest integer, ties toward +Infinity (not half-away-from-zero).
// Rounding into zero from x in [-0.5, 0) yields -0, matching JS.
pub fn js_math_round(x: f64) -> f64 {
    if !x.is_finite() || x == 0.0 || x.fract() == 0.0 {
        return x;
    }
    let f = x.floor();
    let r = if x - f >= 0.5 { f + 1.0 } else { f };
    if r == 0.0 && x < 0.0 { -0.0 } else { r }
}

#[cfg(test)]
mod round_tests {
    use super::js_math_round;

    #[test]
    fn negative_zero_and_ties() {
        assert!(js_math_round(-0.1).is_sign_negative());
        assert!(js_math_round(-0.5).is_sign_negative());
        assert_eq!(js_math_round(-0.5), 0.0);
        assert_eq!(js_math_round(0.5), 1.0);
        assert_eq!(js_math_round(2.5), 3.0);
        assert_eq!(js_math_round(-2.5), -2.0);
        assert_eq!(js_math_round(0.49999999999999994), 0.0);
    }
}

// JS default string comparison (`<`, Array.prototype.sort) is UTF-16 code-unit
// order, which diverges from Rust's `str` Ord for astral-plane characters.
pub fn utf16_cmp(a: &str, b: &str) -> Ordering {
    a.encode_utf16().cmp(b.encode_utf16())
}

// JS String.prototype.slice indexes UTF-16 code units; byte slicing panics on
// multibyte boundaries. Negative end counts from the end, as in JS.
pub fn js_slice_utf16(s: &str, start: usize, end: isize) -> String {
    let units: Vec<u16> = s.encode_utf16().collect();
    let len = units.len() as isize;
    let end = if end < 0 {
        (len + end).max(0)
    } else {
        end.min(len)
    } as usize;
    if start >= end {
        return String::new();
    }
    String::from_utf16_lossy(&units[start..end])
}

/// A slice boundary split a surrogate pair: the doctrine is loud over lossy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LoneSurrogate;

/// `js_slice_utf16` that refuses to manufacture lone surrogates.
pub fn js_slice_utf16_checked(s: &str, start: usize, end: isize) -> Result<String, LoneSurrogate> {
    let units: Vec<u16> = s.encode_utf16().collect();
    let len = units.len() as isize;
    let end = if end < 0 {
        (len + end).max(0)
    } else {
        end.min(len)
    } as usize;
    if start >= end {
        return Ok(String::new());
    }
    String::from_utf16(&units[start..end]).map_err(|_| LoneSurrogate)
}

/// JS Number.prototype.toFixed: the spec strips the sign first, then rounds
/// the exact decimal expansion half-up (ties pick the larger n).
pub fn js_to_fixed(x: f64, digits: usize) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.abs() >= 1e21 || x.is_infinite() {
        return js_number_to_string(x);
    }
    let negative = x.is_sign_negative() && x != 0.0;
    // 1100 fractional digits covers the exact expansion of any finite f64.
    let exact = format!("{:.1100}", x.abs());
    let (int_part, frac_part) = exact.split_once('.').unwrap_or((exact.as_str(), ""));
    let mut int_digits: Vec<u8> = int_part.bytes().map(|b| b - b'0').collect();
    let mut frac_digits: Vec<u8> = frac_part.bytes().map(|b| b - b'0').collect();
    frac_digits.resize(1101.max(digits + 1), 0);
    let kept = frac_digits[..digits].to_vec();
    let rest = &frac_digits[digits..];
    let first = rest.first().copied().unwrap_or(0);
    let round_up = first >= 5;
    let mut frac = kept;
    if round_up {
        let mut carry = 1u8;
        for digit in frac.iter_mut().rev() {
            let sum = *digit + carry;
            *digit = sum % 10;
            carry = sum / 10;
            if carry == 0 {
                break;
            }
        }
        if carry > 0 {
            for digit in int_digits.iter_mut().rev() {
                let sum = *digit + carry;
                *digit = sum % 10;
                carry = sum / 10;
                if carry == 0 {
                    break;
                }
            }
            if carry > 0 {
                int_digits.insert(0, carry);
            }
        }
    }
    let int_str: String = int_digits.iter().map(|d| (d + b'0') as char).collect();
    let int_str = int_str.trim_start_matches('0');
    let int_str = if int_str.is_empty() { "0" } else { int_str };
    // JS keeps the sign of tiny negatives ("-0.00"); literal -0 stays "0.00".
    let sign = if negative { "-" } else { "" };
    if digits == 0 {
        format!("{sign}{int_str}")
    } else {
        let frac_str: String = frac.iter().map(|d| (d + b'0') as char).collect();
        format!("{sign}{int_str}.{frac_str}")
    }
}

#[cfg(test)]
mod to_fixed_tests {
    use super::js_to_fixed;

    #[test]
    fn ties_round_half_up_on_the_magnitude() {
        // 0.125 and ±2.5 are exactly representable: true ties (node-pinned).
        assert_eq!(js_to_fixed(0.125, 2), "0.13");
        assert_eq!(js_to_fixed(-0.125, 2), "-0.13");
        // 1.005 is stored below the tie, so it rounds down (JS "1.00").
        assert_eq!(js_to_fixed(1.005, 2), "1.00");
        assert_eq!(js_to_fixed(1.5, 0), "2");
        assert_eq!(js_to_fixed(2.5, 0), "3");
        assert_eq!(js_to_fixed(-2.5, 0), "-3");
        assert_eq!(js_to_fixed(1.0, 2), "1.00");
        assert_eq!(js_to_fixed(0.0, 2), "0.00");
        assert_eq!(js_to_fixed(-0.0, 2), "0.00");
        assert_eq!(js_to_fixed(-0.004, 2), "-0.00");
        assert_eq!(js_to_fixed(f64::NAN, 2), "NaN");
        assert_eq!(js_to_fixed(1.45, 1), "1.4");
        assert_eq!(js_to_fixed(9.995, 2), "9.99");
        assert_eq!(js_to_fixed(0.999, 2), "1.00");
    }
}

/// StyleX's stylesheet comparator: UTF-16 code-unit order.
pub use utf16_cmp as default_locale_cmp;

pub(crate) fn default_first_cmp(a: &str, b: &str) -> Ordering {
    (a != "default")
        .cmp(&(b != "default"))
        .then_with(|| utf16_cmp(a, b))
}

// ES TrimString WhiteSpace + LineTerminator: WhiteSpace is TAB VT FF SP NBSP
// ZWNBSP(FEFF) + Zs; LineTerminator is LF CR LS PS. NEL (U+0085) is excluded.
pub fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{2000}'
            ..='\u{200A}'
                | '\u{0009}'
                | '\u{000A}'
                | '\u{000B}'
                | '\u{000C}'
                | '\u{000D}'
                | '\u{0020}'
                | '\u{00A0}'
                | '\u{1680}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

#[cfg(test)]
mod js_trim_tests {
    use super::js_trim;

    #[test]
    fn trims_es_whitespace_rust_trim_misses() {
        assert_eq!(js_trim("\u{FEFF}--real\u{FEFF}"), "--real");
        assert_eq!(js_trim(" \u{FEFF}\u{00A0}x\u{2028}\u{2029} "), "x");
        assert_eq!("\u{FEFF}x".trim(), "\u{FEFF}x");
    }

    #[test]
    fn keeps_nel_rust_trim_removes() {
        assert_eq!(js_trim("\u{0085}x\u{0085}"), "\u{0085}x\u{0085}");
        assert_eq!("\u{0085}x".trim(), "x");
    }
}

/// A string literal's value as text. Valid UTF-8 is borrowed; a lone
/// surrogate keeps the form oxc 0.154 stored it in (`\u{FFFD}` then its four
/// hex digits, and a literal U+FFFD as `\u{FFFD}fffd`), which these readers
/// compared and keyed by before oxc 0.155 made the value WTF-8.
pub fn lit_text(value: oxc_str::JSStr<'_>) -> std::borrow::Cow<'_, str> {
    use std::fmt::Write as _;
    if let Some(text) = value.as_str() {
        return std::borrow::Cow::Borrowed(text);
    }
    let mut out = String::with_capacity(value.as_bytes().len());
    for c in value.chars() {
        match c.to_char() {
            Some('\u{FFFD}') => out.push_str("\u{FFFD}fffd"),
            Some(ch) => out.push(ch),
            None => {
                out.push('\u{FFFD}');
                let _ = write!(out, "{:04x}", c.to_u32());
            }
        }
    }
    std::borrow::Cow::Owned(out)
}

#[cfg(test)]
mod lit_text_tests {
    use super::lit_text;

    /// The first string literal's value in `source`.
    fn first_string(source: &str, check: impl FnOnce(oxc_str::JSStr<'_>)) {
        let allocator = oxc_allocator::Allocator::default();
        let parsed =
            oxc_parser::Parser::new(&allocator, source, oxc_span::SourceType::mjs()).parse();
        let oxc_ast::ast::Statement::ExpressionStatement(stmt) = &parsed.program.body[0] else {
            panic!("an expression statement");
        };
        let oxc_ast::ast::Expression::ParenthesizedExpression(paren) = &stmt.expression else {
            panic!("a parenthesized literal");
        };
        let oxc_ast::ast::Expression::StringLiteral(lit) = &paren.expression else {
            panic!("a string literal");
        };
        check(lit.value);
    }

    #[test]
    fn valid_text_is_borrowed_and_lone_surrogates_keep_the_lossy_form() {
        first_string(r#"("a\u00e9b");"#, |v| {
            assert!(matches!(
                lit_text(v),
                std::borrow::Cow::Borrowed("a\u{e9}b")
            ));
        });
        // oxc 0.154's form: U+FFFD then the surrogate's hex, and a literal
        // U+FFFD in the same string escaped as `\u{FFFD}fffd`.
        first_string(r#"("x\uD800\uFFFDy");"#, |v| {
            assert_eq!(lit_text(v), "x\u{FFFD}d800\u{FFFD}fffdy");
        });
    }
}
