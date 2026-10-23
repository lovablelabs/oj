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

/// A character outside the collation alphabet verified against Node's ICU
/// (testdata/pins/collation.json); `locale_cmp` refuses to guess its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnverifiedChar(pub char);

// (primary, secondary, tertiary) per char; ranks transcribed from the pinned
// `groups` in collation.json (gen-pins-collation.mjs, node en-US ICU).
pub(crate) const fn locale_key(c: char) -> Result<(u8, u8, u8), UnverifiedChar> {
    let key = match c {
        ' ' => (0, 0, 0),
        '_' => (1, 0, 0),
        '-' => (2, 0, 0),
        '–' => (3, 0, 0),
        '—' => (4, 0, 0),
        ',' => (5, 0, 0),
        ';' => (6, 0, 0),
        ':' => (7, 0, 0),
        '!' => (8, 0, 0),
        '¡' => (9, 0, 0),
        '?' => (10, 0, 0),
        '¿' => (11, 0, 0),
        '.' => (12, 0, 0),
        '·' => (13, 0, 0),
        '\'' => (14, 0, 0),
        '‘' => (14, 1, 0),
        '’' => (14, 2, 0),
        '‹' => (15, 0, 0),
        '›' => (16, 0, 0),
        '"' => (17, 0, 0),
        '“' => (17, 1, 0),
        '”' => (17, 2, 0),
        '«' => (18, 0, 0),
        '»' => (19, 0, 0),
        '(' => (20, 0, 0),
        ')' => (21, 0, 0),
        '[' => (22, 0, 0),
        ']' => (23, 0, 0),
        '{' => (24, 0, 0),
        '}' => (25, 0, 0),
        '§' => (26, 0, 0),
        '¶' => (27, 0, 0),
        '@' => (28, 0, 0),
        '*' => (29, 0, 0),
        '/' => (30, 0, 0),
        '\\' => (31, 0, 0),
        '&' => (32, 0, 0),
        '#' => (33, 0, 0),
        '%' => (34, 0, 0),
        '‰' => (35, 0, 0),
        '†' => (36, 0, 0),
        '‡' => (37, 0, 0),
        '•' => (38, 0, 0),
        '″' => (39, 0, 0),
        '`' => (40, 0, 0),
        '´' => (41, 0, 0),
        '^' => (42, 0, 0),
        '¯' => (43, 0, 0),
        '¨' => (44, 0, 0),
        '¸' => (45, 0, 0),
        '°' => (46, 0, 0),
        '©' => (47, 0, 0),
        '®' => (48, 0, 0),
        '←' => (49, 0, 0),
        '→' => (50, 0, 0),
        '↔' => (51, 0, 0),
        '⇒' => (52, 0, 0),
        '+' => (53, 0, 0),
        '±' => (54, 0, 0),
        '÷' => (55, 0, 0),
        '×' => (56, 0, 0),
        '<' => (57, 0, 0),
        '=' => (58, 0, 0),
        '≠' => (58, 1, 0),
        '>' => (59, 0, 0),
        '¬' => (60, 0, 0),
        '|' => (61, 0, 0),
        '¦' => (62, 0, 0),
        '~' => (63, 0, 0),
        '∞' => (64, 0, 0),
        '≈' => (65, 0, 0),
        '≤' => (66, 0, 0),
        '≥' => (67, 0, 0),
        '★' => (68, 0, 0),
        '☆' => (69, 0, 0),
        '♠' => (70, 0, 0),
        '♣' => (71, 0, 0),
        '♥' => (72, 0, 0),
        '♦' => (73, 0, 0),
        '✓' => (74, 0, 0),
        '✗' => (75, 0, 0),
        '✨' => (76, 0, 0),
        '❤' => (77, 0, 0),
        '⭐' => (78, 0, 0),
        '🎉' => (79, 0, 0),
        '👍' => (80, 0, 0),
        '💯' => (81, 0, 0),
        '🔥' => (82, 0, 0),
        '🦄' => (83, 0, 0),
        '😀' => (84, 0, 0),
        '🚀' => (85, 0, 0),
        '¤' => (86, 0, 0),
        '¢' => (87, 0, 0),
        '$' => (88, 0, 0),
        '£' => (89, 0, 0),
        '¥' => (90, 0, 0),
        '€' => (91, 0, 0),
        '0' => (92, 0, 0),
        '1' => (93, 0, 0),
        '2' => (94, 0, 0),
        '²' => (94, 0, 1),
        '3' => (95, 0, 0),
        '³' => (95, 0, 1),
        '4' => (96, 0, 0),
        '5' => (97, 0, 0),
        '6' => (98, 0, 0),
        '7' => (99, 0, 0),
        '8' => (100, 0, 0),
        '9' => (101, 0, 0),
        'a' => (102, 0, 0),
        'A' => (102, 0, 1),
        'ª' => (102, 0, 2),
        'á' => (102, 1, 0),
        'Á' => (102, 1, 1),
        'à' => (102, 2, 0),
        'À' => (102, 2, 1),
        'â' => (102, 3, 0),
        'Â' => (102, 3, 1),
        'å' => (102, 4, 0),
        'Å' => (102, 4, 1),
        'ä' => (102, 5, 0),
        'Ä' => (102, 5, 1),
        'ã' => (102, 6, 0),
        'Ã' => (102, 6, 1),
        'b' => (103, 0, 0),
        'B' => (103, 0, 1),
        'c' => (104, 0, 0),
        'C' => (104, 0, 1),
        'ç' => (104, 1, 0),
        'Ç' => (104, 1, 1),
        'd' => (105, 0, 0),
        'D' => (105, 0, 1),
        'ð' => (105, 1, 0),
        'Ð' => (105, 1, 1),
        'e' => (106, 0, 0),
        'E' => (106, 0, 1),
        'é' => (106, 1, 0),
        'É' => (106, 1, 1),
        'è' => (106, 2, 0),
        'È' => (106, 2, 1),
        'ê' => (106, 3, 0),
        'Ê' => (106, 3, 1),
        'ë' => (106, 4, 0),
        'Ë' => (106, 4, 1),
        'f' => (107, 0, 0),
        'F' => (107, 0, 1),
        'g' => (108, 0, 0),
        'G' => (108, 0, 1),
        'h' => (109, 0, 0),
        'H' => (109, 0, 1),
        'i' => (110, 0, 0),
        'I' => (110, 0, 1),
        'í' => (110, 1, 0),
        'Í' => (110, 1, 1),
        'ì' => (110, 2, 0),
        'Ì' => (110, 2, 1),
        'î' => (110, 3, 0),
        'Î' => (110, 3, 1),
        'ï' => (110, 4, 0),
        'Ï' => (110, 4, 1),
        'j' => (111, 0, 0),
        'J' => (111, 0, 1),
        'k' => (112, 0, 0),
        'K' => (112, 0, 1),
        'l' => (113, 0, 0),
        'L' => (113, 0, 1),
        'm' => (114, 0, 0),
        'M' => (114, 0, 1),
        'n' => (115, 0, 0),
        'N' => (115, 0, 1),
        'ñ' => (115, 1, 0),
        'Ñ' => (115, 1, 1),
        'o' => (116, 0, 0),
        'O' => (116, 0, 1),
        'º' => (116, 0, 2),
        'ó' => (116, 1, 0),
        'Ó' => (116, 1, 1),
        'ò' => (116, 2, 0),
        'Ò' => (116, 2, 1),
        'ô' => (116, 3, 0),
        'Ô' => (116, 3, 1),
        'ö' => (116, 4, 0),
        'Ö' => (116, 4, 1),
        'õ' => (116, 5, 0),
        'Õ' => (116, 5, 1),
        'ø' => (116, 6, 0),
        'Ø' => (116, 6, 1),
        'p' => (117, 0, 0),
        'P' => (117, 0, 1),
        'q' => (118, 0, 0),
        'Q' => (118, 0, 1),
        'r' => (119, 0, 0),
        'R' => (119, 0, 1),
        's' => (120, 0, 0),
        'S' => (120, 0, 1),
        't' => (121, 0, 0),
        'T' => (121, 0, 1),
        'u' => (122, 0, 0),
        'U' => (122, 0, 1),
        'ú' => (122, 1, 0),
        'Ú' => (122, 1, 1),
        'ù' => (122, 2, 0),
        'Ù' => (122, 2, 1),
        'û' => (122, 3, 0),
        'Û' => (122, 3, 1),
        'ü' => (122, 4, 0),
        'Ü' => (122, 4, 1),
        'v' => (123, 0, 0),
        'V' => (123, 0, 1),
        'w' => (124, 0, 0),
        'W' => (124, 0, 1),
        'x' => (125, 0, 0),
        'X' => (125, 0, 1),
        'y' => (126, 0, 0),
        'Y' => (126, 0, 1),
        'ý' => (126, 1, 0),
        'Ý' => (126, 1, 1),
        'ÿ' => (126, 2, 0),
        'z' => (127, 0, 0),
        'Z' => (127, 0, 1),
        'þ' => (128, 0, 0),
        'Þ' => (128, 0, 1),
        'α' => (129, 0, 0),
        'β' => (130, 0, 0),
        'γ' => (131, 0, 0),
        'δ' => (132, 0, 0),
        'µ' => (133, 0, 0),
        'π' => (134, 0, 0),
        'Ω' => (135, 0, 0),
        'а' => (136, 0, 0),
        'б' => (137, 0, 0),
        'в' => (138, 0, 0),
        '中' => (139, 0, 0),
        '日' => (140, 0, 0),
        _ => return Err(UnverifiedChar(c)),
    };
    Ok(key)
}

// 256 wide so a byte index needs no bounds check; only ASCII strings read it.
pub(crate) static ASCII_LOCALE_KEYS: [Option<(u8, u8, u8)>; 256] = {
    let mut table = [None; 256];
    let mut b = 0usize;
    while b < 128 {
        table[b] = match locale_key(b as u8 as char) {
            Ok(key) => Some(key),
            Err(_) => None,
        };
        b += 1;
    }
    table
};

#[cfg(test)]
mod ascii_locale_key_tests {
    use super::{ASCII_LOCALE_KEYS, locale_key};

    #[test]
    fn table_matches_locale_key_for_every_byte() {
        for b in 0..=255u8 {
            let want = (b < 128).then(|| locale_key(b as char).ok()).flatten();
            assert_eq!(ASCII_LOCALE_KEYS[usize::from(b)], want, "byte {b:#x}");
        }
        assert!(ASCII_LOCALE_KEYS[usize::from(b'a')].is_some());
        assert!(ASCII_LOCALE_KEYS[usize::from(b'{')].is_some());
    }
}

/// JS localeCompare as the oracle's node runs it: whole-string primary,
/// secondary (accents), then tertiary (case, quote shape) passes.
pub fn locale_cmp(a: &str, b: &str) -> Result<Ordering, UnverifiedChar> {
    let ka = a.chars().map(locale_key).collect::<Result<Vec<_>, _>>()?;
    let kb = b.chars().map(locale_key).collect::<Result<Vec<_>, _>>()?;
    let primary = ka.iter().map(|k| k.0).cmp(kb.iter().map(|k| k.0));
    if primary != Ordering::Equal {
        return Ok(primary);
    }
    let secondary = ka.iter().map(|k| k.1).cmp(kb.iter().map(|k| k.1));
    if secondary != Ordering::Equal {
        return Ok(secondary);
    }
    Ok(ka.iter().map(|k| k.2).cmp(kb.iter().map(|k| k.2)))
}

/// The one localeCompare stand-in for rule and pseudo sorting; outside the
/// verified alphabet it falls back to UTF-16 order (divergence risk vs ICU).
pub fn default_locale_cmp(a: &str, b: &str) -> Ordering {
    locale_cmp(a, b).unwrap_or_else(|UnverifiedChar(_)| utf16_cmp(a, b))
}

/// First char the pinned collation cannot order; hash-feeding sorts hard-error
/// on it instead of guessing (r4#4 policy), non-hash sorts keep the fallback.
pub fn unverified_collation_char(s: &str) -> Option<char> {
    s.chars().find(|&c| locale_key(c).is_err())
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
