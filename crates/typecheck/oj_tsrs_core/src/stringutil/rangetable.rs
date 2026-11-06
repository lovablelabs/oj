// Equivalent of Go's unicode.RangeTable / unicode.Is, used by the generated tables.

pub type Rune = u32;

pub(crate) struct Range16 {
    pub(crate) lo: u16,
    pub(crate) hi: u16,
    pub(crate) stride: u16,
}

pub(crate) struct Range32 {
    pub(crate) lo: u32,
    pub(crate) hi: u32,
    pub(crate) stride: u32,
}

pub(crate) struct RangeTable {
    pub(crate) r16: &'static [Range16],
    pub(crate) r32: &'static [Range32],
}

const LINEAR_MAX: usize = 18;

fn is16(ranges: &[Range16], r: u16) -> bool {
    if ranges.len() <= LINEAR_MAX || r <= 0xFF {
        if let Some(result) = ranges.iter().find_map(|range| {
            if r < range.lo {
                Some(false)
            } else if r <= range.hi {
                Some(range.stride == 1 || (r - range.lo).is_multiple_of(range.stride))
            } else {
                None
            }
        }) {
            return result;
        }
        return false;
    }
    let (mut lo, mut hi) = (0usize, ranges.len());
    while lo < hi {
        let m = lo + (hi - lo) / 2;
        let range = &ranges[m];
        if range.lo <= r && r <= range.hi {
            return range.stride == 1 || (r - range.lo).is_multiple_of(range.stride);
        }
        if r < range.lo {
            hi = m;
        } else {
            lo = m + 1;
        }
    }
    false
}

fn is32(ranges: &[Range32], r: u32) -> bool {
    if ranges.len() <= LINEAR_MAX {
        if let Some(result) = ranges.iter().find_map(|range| {
            if r < range.lo {
                Some(false)
            } else if r <= range.hi {
                Some(range.stride == 1 || (r - range.lo).is_multiple_of(range.stride))
            } else {
                None
            }
        }) {
            return result;
        }
        return false;
    }
    let (mut lo, mut hi) = (0usize, ranges.len());
    while lo < hi {
        let m = lo + (hi - lo) / 2;
        let range = &ranges[m];
        if range.lo <= r && r <= range.hi {
            return range.stride == 1 || (r - range.lo).is_multiple_of(range.stride);
        }
        if r < range.lo {
            hi = m;
        } else {
            lo = m + 1;
        }
    }
    false
}

/// Go `unicode.Is`.
pub(crate) fn unicode_is(table: &RangeTable, r: Rune) -> bool {
    let r16 = table.r16;
    if !r16.is_empty() && r <= u32::from(r16[r16.len() - 1].hi) {
        return is16(r16, r as u16);
    }
    let r32 = table.r32;
    if !r32.is_empty() && r >= r32[0].lo {
        return is32(r32, r);
    }
    false
}

/// Go `unicode.ToLower` via Rust's full lowercase mapping; U+0130, the only rune with a multi-char full lowercase, is
/// special-cased to 'i'.
pub fn unicode_to_lower(r: Rune) -> Rune {
    if r < 0x80 {
        if (b'A' as Rune..=b'Z' as Rune).contains(&r) {
            return r + 32;
        }
        return r;
    }
    let Some(c) = char::from_u32(r) else {
        return r;
    };
    if c == '\u{130}' {
        return 'i' as Rune;
    }
    let mut it = c.to_lowercase();
    match (it.next(), it.next()) {
        (Some(l), None) => l as Rune,
        _ => r,
    }
}

/// Go `unicode.ToUpper` (simple uppercase mapping) via Rust's full uppercase mapping, keeping the rune when
/// that mapping is not a single char.
///
/// The only runes whose full uppercase is multi-char but whose simple uppercase is another rune are the 27
/// Greek letters with ypogegrammeni, which map to their titlecase form (U+1F80 -> U+1F88, U+1FB3 -> U+1FBC);
/// they are special-cased. Checked exhaustively against Go 1.27 (Unicode 17.0.0, the same version as the
/// pinned Rust toolchain's `char::UNICODE_VERSION`).
pub fn unicode_to_upper(r: Rune) -> Rune {
    if r < 0x80 {
        if (b'a' as Rune..=b'z' as Rune).contains(&r) {
            return r - 32;
        }
        return r;
    }
    let Some(c) = char::from_u32(r) else {
        return r;
    };
    match r {
        0x1F80..=0x1F87 | 0x1F90..=0x1F97 | 0x1FA0..=0x1FA7 => return r + 8,
        0x1FB3 | 0x1FC3 | 0x1FF3 => return r + 9,
        _ => {}
    }
    let mut it = c.to_uppercase();
    match (it.next(), it.next()) {
        (Some(u), None) => u as Rune,
        _ => r,
    }
}

/// Go `unicode.SimpleFold` (Go 1.27 unicode/letter.go:354): the next rune after `r` in its simple case
/// folding orbit, wrapping around to the smallest.
///
/// Go first consults its `asciiFold` table for ASCII; that table agrees with `CASE_ORBIT` (K, S, k, s) and
/// the ToLower/ToUpper fallback, so it is not copied.
pub fn unicode_simple_fold(r: Rune) -> Rune {
    if r > 0x0010_FFFF {
        return r;
    }
    if let Ok(i) = CASE_ORBIT.binary_search_by_key(&r, |&(from, _)| from as Rune) {
        return CASE_ORBIT[i].1 as Rune;
    }
    // No folding specified. This is a one- or two-element equivalence class containing rune and ToLower(rune) and
    // ToUpper(rune) if they are different from rune.
    let l = unicode_to_lower(r);
    if l != r {
        return l;
    }
    unicode_to_upper(r)
}

/// Go `unicode.caseOrbit` (unicode/tables.go:9816, Unicode 17.0.0): the orbits of more than two runes, plus U+0130 and
/// U+0131, which fold only to themselves.
#[rustfmt::skip]
static CASE_ORBIT: [(u16, u16); 94] = [
    (0x004B, 0x006B), (0x0053, 0x0073), (0x006B, 0x212A), (0x0073, 0x017F), (0x00B5, 0x039C), (0x00C5, 0x00E5),
    (0x00DF, 0x1E9E), (0x00E5, 0x212B), (0x0130, 0x0130), (0x0131, 0x0131), (0x017F, 0x0053), (0x01C4, 0x01C5),
    (0x01C5, 0x01C6), (0x01C6, 0x01C4), (0x01C7, 0x01C8), (0x01C8, 0x01C9), (0x01C9, 0x01C7), (0x01CA, 0x01CB),
    (0x01CB, 0x01CC), (0x01CC, 0x01CA), (0x01F1, 0x01F2), (0x01F2, 0x01F3), (0x01F3, 0x01F1), (0x0345, 0x0399),
    (0x0390, 0x1FD3), (0x0392, 0x03B2), (0x0395, 0x03B5), (0x0398, 0x03B8), (0x0399, 0x03B9), (0x039A, 0x03BA),
    (0x039C, 0x03BC), (0x03A0, 0x03C0), (0x03A1, 0x03C1), (0x03A3, 0x03C2), (0x03A6, 0x03C6), (0x03A9, 0x03C9),
    (0x03B0, 0x1FE3), (0x03B2, 0x03D0), (0x03B5, 0x03F5), (0x03B8, 0x03D1), (0x03B9, 0x1FBE), (0x03BA, 0x03F0),
    (0x03BC, 0x00B5), (0x03C0, 0x03D6), (0x03C1, 0x03F1), (0x03C2, 0x03C3), (0x03C3, 0x03A3), (0x03C6, 0x03D5),
    (0x03C9, 0x2126), (0x03D0, 0x0392), (0x03D1, 0x03F4), (0x03D5, 0x03A6), (0x03D6, 0x03A0), (0x03F0, 0x039A),
    (0x03F1, 0x03A1), (0x03F4, 0x0398), (0x03F5, 0x0395), (0x0412, 0x0432), (0x0414, 0x0434), (0x041E, 0x043E),
    (0x0421, 0x0441), (0x0422, 0x0442), (0x042A, 0x044A), (0x0432, 0x1C80), (0x0434, 0x1C81), (0x043E, 0x1C82),
    (0x0441, 0x1C83), (0x0442, 0x1C84), (0x044A, 0x1C86), (0x0462, 0x0463), (0x0463, 0x1C87), (0x1C80, 0x0412),
    (0x1C81, 0x0414), (0x1C82, 0x041E), (0x1C83, 0x0421), (0x1C84, 0x1C85), (0x1C85, 0x0422), (0x1C86, 0x042A),
    (0x1C87, 0x0462), (0x1C88, 0xA64A), (0x1E60, 0x1E61), (0x1E61, 0x1E9B), (0x1E9B, 0x1E60), (0x1E9E, 0x00DF),
    (0x1FBE, 0x0345), (0x1FD3, 0x0390), (0x1FE3, 0x03B0), (0x2126, 0x03A9), (0x212A, 0x004B), (0x212B, 0x00C5),
    (0xA64A, 0xA64B), (0xA64B, 0x1C88), (0xFB05, 0xFB06), (0xFB06, 0xFB05),
];

/// Go `utf8.DecodeRuneInString` over raw bytes: returns (`RuneError`, 1) for invalid encodings (including
/// surrogate encodings) and (`RuneError`, 0) for empty input.
pub fn decode_rune(s: &[u8]) -> (Rune, usize) {
    const RUNE_ERROR: Rune = 0xFFFD;
    if s.is_empty() {
        return (RUNE_ERROR, 0);
    }
    let b0 = s[0];
    if b0 < 0x80 {
        return (b0 as Rune, 1);
    }
    let cont = |i: usize, lo: u8, hi: u8| -> Option<u32> {
        let b = *s.get(i)?;
        if b < lo || b > hi { None } else { Some((b & 0x3F) as u32) }
    };
    if (0xC2..=0xDF).contains(&b0) {
        if let Some(c1) = cont(1, 0x80, 0xBF) {
            return ((((b0 & 0x1F) as u32) << 6) | c1, 2);
        }
        return (RUNE_ERROR, 1);
    }
    if (0xE0..=0xEF).contains(&b0) {
        let (lo, hi) = match b0 {
            0xE0 => (0xA0, 0xBF),
            0xED => (0x80, 0x9F),
            _ => (0x80, 0xBF),
        };
        if let (Some(c1), Some(c2)) = (cont(1, lo, hi), cont(2, 0x80, 0xBF)) {
            return ((((b0 & 0x0F) as u32) << 12) | (c1 << 6) | c2, 3);
        }
        return (RUNE_ERROR, 1);
    }
    if (0xF0..=0xF4).contains(&b0) {
        let (lo, hi) = match b0 {
            0xF0 => (0x90, 0xBF),
            0xF4 => (0x80, 0x8F),
            _ => (0x80, 0xBF),
        };
        if let (Some(c1), Some(c2), Some(c3)) =
            (cont(1, lo, hi), cont(2, 0x80, 0xBF), cont(3, 0x80, 0xBF))
        {
            return ((((b0 & 0x07) as u32) << 18) | (c1 << 12) | (c2 << 6) | c3, 4);
        }
        return (RUNE_ERROR, 1);
    }
    (RUNE_ERROR, 1)
}

/// Go `utf8.DecodeLastRuneInString` over raw bytes.
pub fn decode_last_rune(s: &[u8]) -> (Rune, usize) {
    if s.is_empty() {
        return (0xFFFD, 0);
    }
    let end = s.len();
    let lim = end.saturating_sub(4);
    let mut start = end - 1;
    if s[start] < 0x80 {
        return (s[start] as Rune, 1);
    }
    start -= (lim + 1..=start).rev().take_while(|&i| (s[i] & 0xC0) == 0x80).count();
    let (r, size) = decode_rune(&s[start..end]);
    if start + size != end {
        return (0xFFFD, 1);
    }
    (r, size)
}

/// Appends the UTF-8 encoding of `r` (Go `strings.Builder.WriteRune`; invalid runes become U+FFFD).
pub fn push_rune(b: &mut String, r: Rune) {
    b.push(char::from_u32(r).unwrap_or('\u{FFFD}'));
}
