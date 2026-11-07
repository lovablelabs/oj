// Package stringutil Exports common rune utilities for parsing and emitting javascript

use super::rangetable::{Rune, decode_last_rune, decode_rune, push_rune};

pub fn is_white_space_like(ch: Rune) -> bool {
    is_white_space_single_line(ch) || is_line_break(ch)
}

pub fn is_white_space_single_line(ch: Rune) -> bool {
    // Note: nextLine is in the Zs space, and should be considered to be a whitespace.
    // It is explicitly not a line-break as it isn't in the exact set specified by EcmaScript.
    matches!(
        ch,
        0x20 // space
        | 0x09 // tab
        | 0x0B // verticalTab
        | 0x0C // formFeed
        | 0x0085 // nextLine
        | 0x00A0 // nonBreakingSpace
        | 0x1680 // ogham
        | 0x2000 // enQuad
        | 0x2001 // emQuad
        | 0x2002 // enSpace
        | 0x2003 // emSpace
        | 0x2004 // threePerEmSpace
        | 0x2005 // fourPerEmSpace
        | 0x2006 // sixPerEmSpace
        | 0x2007 // figureSpace
        | 0x2008 // punctuationEmSpace
        | 0x2009 // thinSpace
        | 0x200A // hairSpace
        | 0x200B // zeroWidthSpace
        | 0x202F // narrowNoBreakSpace
        | 0x205F // mathematicalSpace
        | 0x3000 // ideographicSpace
        | 0xFEFF // byteOrderMark
    )
}

pub fn is_line_break(ch: Rune) -> bool {
    // ES5 7.3:
    // The ECMAScript line terminator characters are listed in Table 3.
    //     Table 3: Line Terminator Characters
    //     Code Unit Value     Name                    Formal Name
    //     \u000A              Line Feed               <LF>
    //     \u000D              Carriage Return         <CR>
    //                    Line separator          <LS>
    //                    Paragraph separator     <PS>
    // Only the characters in Table 3 are treated as line terminators. Other new line or line
    // breaking characters are treated as white space but not as line terminators.
    matches!(
        ch,
        0x0A // lineFeed
        | 0x0D // carriageReturn
        | 0x2028 // lineSeparator
        | 0x2029 // paragraphSeparator
    )
}

pub fn is_digit(ch: Rune) -> bool {
    ch >= '0' as Rune && ch <= '9' as Rune
}

pub fn is_octal_digit(ch: Rune) -> bool {
    ch >= '0' as Rune && ch <= '7' as Rune
}

pub fn is_hex_digit(ch: Rune) -> bool {
    ch >= '0' as Rune && ch <= '9' as Rune
        || ch >= 'A' as Rune && ch <= 'F' as Rune
        || ch >= 'a' as Rune && ch <= 'f' as Rune
}

pub fn is_ascii_letter(ch: Rune) -> bool {
    ch >= 'A' as Rune && ch <= 'Z' as Rune || ch >= 'a' as Rune && ch <= 'z' as Rune
}

pub fn split_lines(text: &str) -> Vec<&str> {
    text.split_terminator('\n')
        .flat_map(|line| line.strip_suffix('\r').unwrap_or(line).split('\r'))
        .collect()
}

pub fn guess_indentation(lines: &[&str]) -> usize {
    const MAX_SMI_X86: usize = 0x3fff_ffff;
    let mut indentation = MAX_SMI_X86;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let bytes = line.as_bytes();
        let mut i = 0;
        while i < bytes.len() && i < indentation {
            let (ch, size) = decode_rune(&bytes[i..]);
            if !is_white_space_like(ch) {
                break;
            }
            i += size;
        }
        if i < indentation {
            indentation = i;
        }
        if indentation == 0 {
            return 0;
        }
    }
    if indentation == MAX_SMI_X86 {
        return 0;
    }
    indentation
}

// https://tc39.es/ecma262/multipage/global-object.html#sec-encodeuri-uri
pub fn encode_uri(s: &str) -> String {
    let mut builder = String::new();
    builder.extend(s.bytes().flat_map(|b| {
        {
            if should_escape_for_encode_uri(b) {
                [
                    Some('%'),
                    Some(UPPERHEX[(b >> 4) as usize] as char),
                    Some(UPPERHEX[(b & 0x0f) as usize] as char),
                ]
            } else {
                [Some(b as char), None, None]
            }
        }
        .into_iter()
        .flatten()
    }));
    builder
}

const UPPERHEX: &[u8] = b"0123456789ABCDEF";

fn should_escape_for_encode_uri(b: u8) -> bool {
    if b.is_ascii_uppercase() || b.is_ascii_lowercase() || b.is_ascii_digit() {
        return false;
    }
    !matches!(
        b,
        b';' | b'/'
            | b'?'
            | b':'
            | b'@'
            | b'&'
            | b'='
            | b'+'
            | b'$'
            | b','
            | b'#'
            | b'-'
            | b'_'
            | b'.'
            | b'!'
            | b'~'
            | b'*'
            | b'\''
            | b'('
            | b')'
    )
}

fn get_byte_order_mark_length(text: &[u8]) -> usize {
    if !text.is_empty() {
        let ch0 = text[0];
        if ch0 == 0xfe {
            if text.len() >= 2 && text[1] == 0xff {
                return 2; // utf16be
            }
            return 0;
        }
        if ch0 == 0xff {
            if text.len() >= 2 && text[1] == 0xfe {
                return 2; // utf16le
            }
            return 0;
        }
        if ch0 == 0xef {
            if text.len() >= 3 && text[1] == 0xbb && text[2] == 0xbf {
                return 3; // utf8
            }
            return 0;
        }
    }
    0
}

pub fn remove_byte_order_mark(text: &str) -> &str {
    let length = get_byte_order_mark_length(text.as_bytes());
    if length > 0 {
        return &text[length..];
    }
    text
}

pub fn add_utf8_byte_order_mark(text: &str) -> String {
    if get_byte_order_mark_length(text.as_bytes()) == 0 {
        return format!("\u{FEFF}{text}");
    }
    text.to_string()
}

pub fn strip_quotes(name: &str) -> &str {
    if name.len() < 2 {
        return name;
    }
    let (first_char, _) = decode_rune(name.as_bytes());
    let (last_char, _) = decode_last_rune(name.as_bytes());
    if first_char == last_char
        && (first_char == '\'' as Rune || first_char == '"' as Rune || first_char == '`' as Rune)
    {
        return &name[1..name.len() - 1];
    }
    name
}

pub fn unquote_string(str: &str) -> String {
    // strconv.Unquote is insufficient as that only handles a single character inside single quotes, as those are character literals in go
    let inner = strip_quotes(str);
    // In strada we do str.replace(/\\./g, s => s.substring(1)) - which is to say, replace all backslash-something with just something
    // That's replicated here faithfully, but it seems wrong! This should probably be an actual unquote operation?
    // (Go's regexp `.` matches any rune except '\n'.)
    let bytes = inner.as_bytes();
    let mut out = String::with_capacity(inner.len());
    let mut i = 0;
    let mut last = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] != b'\n' {
            out.push_str(&inner[last..i]);
            let (_, size) = decode_rune(&bytes[i + 1..]);
            out.push_str(&inner[i + 1..i + 1 + size]);
            i += 1 + size;
            last = i;
            continue;
        }
        i += 1;
    }
    out.push_str(&inner[last..]);
    out
}

pub fn truncate_by_runes(str: &str, max_length: usize) -> &str {
    if str.len() < max_length {
        return str;
    }
    if max_length == 0 {
        return "";
    }
    if let Some((i, _)) = str.char_indices().nth(max_length) {
        return &str[..i];
    }
    str
}

// SurrogateLowStart is the boundary between the high and low halves of the
// UTF-16 surrogate range. unicode/utf16 only exposes IsSurrogate for the
// whole range, so this split point is defined here to distinguish the two.
const SURROGATE_LOW_START: Rune = 0xDC00;

pub fn is_high_surrogate(ch: Rune) -> bool {
    is_surrogate(ch) && ch < SURROGATE_LOW_START
}

pub fn is_low_surrogate(ch: Rune) -> bool {
    is_surrogate(ch) && ch >= SURROGATE_LOW_START
}

pub(super) fn is_surrogate(ch: Rune) -> bool {
    (0xD800..0xE000).contains(&ch)
}

pub fn surrogate_pair_to_code_point(r1: Rune, r2: Rune) -> Rune {
    if (0xD800..0xDC00).contains(&r1) && (0xDC00..0xE000).contains(&r2) {
        return (((r1 - 0xD800) << 10) | (r2 - 0xDC00)) + 0x10000;
    }
    0xFFFD
}

pub fn code_point_to_surrogate_pair(r: Rune) -> (Rune, Rune) {
    if !(0x0001_0000..=0x0010_FFFF).contains(&r) {
        return (0xFFFD, 0xFFFD);
    }
    let r = r - 0x10000;
    (0xD800 + ((r >> 10) & 0x3FF), 0xDC00 + (r & 0x3FF))
}

// A lone surrogate (U+D800–U+DFFF) cannot be represented in valid UTF-8, so
// EncodeJSStringRune stores it as the 3-byte CESU-8/WTF-8 sentinel that UTF-8
// would use for that code point if surrogates were encodable.
//
// Byte layout for a code point cp in U+D000–U+DFFF (lead nibble 0xD):
//   byte0 = 0xE0 | (cp >> 12)          == 0xED
//   byte1 = 0x80 | ((cp >> 6) & 0x3F)
//   byte2 = 0x80 | (cp & 0x3F)
const SURROGATE_UTF8_LEAD: u8 = 0xED; // byte0, shared by the whole U+D000–U+DFFF block
const SURROGATE_UTF8_LEAD_BITS: Rune = 0xD000; // (surrogateUTF8Lead & 0x0F) << 12, byte0's decoded contribution
const UTF8_CONT_MARKER: u8 = 0x80; // continuation byte marker / min value (10xxxxxx)
const UTF8_CONT_MAX: u8 = 0xBF; // continuation byte max value
const UTF8_CONT_MASK: u8 = 0x3F; // data bits carried by a continuation byte

// byte1 bounds that pin the block down to the surrogate range U+D800–U+DFFF:
// 0xD800 -> 0xA0, 0xDFFF -> 0xBF.
const SURROGATE_UTF8_BYTE1_MIN: u8 = 0xA0;
const SURROGATE_UTF8_BYTE1_MAX: u8 = 0xBF;

/// Encodes a rune while preserving lone surrogates.
///
/// Returns the Go `string(ch)` encoding, except that a lone surrogate becomes the WTF-8 sentinel bytes. Such
/// a `String` is not valid UTF-8 (Go strings are byte strings): only compare, hash, concatenate, slice at
/// ASCII boundaries, or decode it with `decode_js_string_rune`; never call `chars()` on it.
pub fn encode_js_string_rune(ch: Rune) -> String {
    if is_surrogate(ch) {
        let bytes = vec![
            SURROGATE_UTF8_LEAD,
            UTF8_CONT_MARKER | (((ch >> 6) as u8) & UTF8_CONT_MASK),
            UTF8_CONT_MARKER | ((ch as u8) & UTF8_CONT_MASK),
        ];
        // SAFETY: deliberate WTF-8 byte string, mirroring Go's byte-string semantics; see doc comment.
        return unsafe { String::from_utf8_unchecked(bytes) };
    }
    let mut s = String::new();
    push_rune(&mut s, ch);
    s
}

pub fn decode_js_string_rune(s: &str) -> (Rune, usize) {
    decode_js_string_rune_bytes(s.as_bytes())
}

pub fn decode_js_string_rune_bytes(s: &[u8]) -> (Rune, usize) {
    if s.len() >= 3
        && s[0] == SURROGATE_UTF8_LEAD
        && s[1] >= SURROGATE_UTF8_BYTE1_MIN
        && s[1] <= SURROGATE_UTF8_BYTE1_MAX
        && s[2] >= UTF8_CONT_MARKER
        && s[2] <= UTF8_CONT_MAX
    {
        return (
            SURROGATE_UTF8_LEAD_BITS
                | (((s[1] & UTF8_CONT_MASK) as Rune) << 6)
                | (s[2] & UTF8_CONT_MASK) as Rune,
            3,
        );
    }
    decode_rune(s)
}

// CombineSurrogatePairs canonicalizes a JS-string value produced by
// concatenation, merging any adjacent high+low surrogate sentinel pair (as
// written by EncodeJSStringRune) into the single supplementary code point they
// represent. This mirrors how concatenating two UTF-16 code units forms a
// surrogate pair in a JavaScript string. It must be applied wherever separately
// scanned string values are joined, since each half is only a lone surrogate
// until it meets its partner. Strings without a lone-surrogate sentinel (the
// common case) are returned unchanged.
pub fn combine_surrogate_pairs(s: &str) -> std::borrow::Cow<'_, str> {
    let bytes = s.as_bytes();
    if memchr::memchr(SURROGATE_UTF8_LEAD, bytes).is_none() {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut b: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let (r, size) = decode_js_string_rune_bytes(&bytes[i..]);
        if is_high_surrogate(r) {
            let (low, low_size) = decode_js_string_rune_bytes(&bytes[i + size..]);
            if is_low_surrogate(low) {
                let mut tmp = String::new();
                push_rune(&mut tmp, surrogate_pair_to_code_point(r, low));
                b.extend_from_slice(tmp.as_bytes());
                i += size + low_size;
                continue;
            }
        }
        b.extend_from_slice(&bytes[i..i + size]);
        i += size;
    }
    // SAFETY: the output is assembled from slices of the (possibly WTF-8) input and valid UTF-8 encodings.
    std::borrow::Cow::Owned(unsafe { String::from_utf8_unchecked(b) })
}
