// Behavior-equivalent to stylex@0.19.0 babel-plugin/src/shared/hash.js: the JS
// murmur reads UTF-16 code units masked to their low byte, with length in units.

const M: u32 = 0x5bd1_e995;

pub fn murmur2_32_gc(input: &str, seed: u32) -> u32 {
    // ASCII fast path: UTF-16 units equal bytes (values and count), so the
    // per-call Vec<u16> re-encode is pure allocation cost there.
    if input.is_ascii() {
        return murmur2_32_units(AsciiUnits(input.as_bytes()), seed);
    }
    let units: Vec<u16> = input.encode_utf16().collect();
    murmur2_32_units(Utf16Units(&units), seed)
}

struct AsciiUnits<'a>(&'a [u8]);
struct Utf16Units<'a>(&'a [u16]);

trait Units {
    fn len(&self) -> usize;
    fn low_byte(&self, i: usize) -> u32;
}

impl Units for AsciiUnits<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }
    #[inline]
    fn low_byte(&self, i: usize) -> u32 {
        u32::from(self.0[i])
    }
}

impl Units for Utf16Units<'_> {
    fn len(&self) -> usize {
        self.0.len()
    }
    #[inline]
    fn low_byte(&self, i: usize) -> u32 {
        u32::from(self.0[i]) & 0xff
    }
}

fn murmur2_32_units(units: impl Units, seed: u32) -> u32 {
    let mut l = units.len();
    let mut h = seed ^ (l as u32);
    let mut i = 0usize;

    while l >= 4 {
        let mut k = units.low_byte(i)
            | (units.low_byte(i + 1) << 8)
            | (units.low_byte(i + 2) << 16)
            | (units.low_byte(i + 3) << 24);
        k = k.wrapping_mul(M);
        k ^= k >> 24;
        k = k.wrapping_mul(M);
        h = h.wrapping_mul(M) ^ k;
        i += 4;
        l -= 4;
    }

    if l >= 3 {
        h ^= units.low_byte(i + 2) << 16;
    }
    if l >= 2 {
        h ^= units.low_byte(i + 1) << 8;
    }
    if l >= 1 {
        h ^= units.low_byte(i);
        h = h.wrapping_mul(M);
    }

    h ^= h >> 13;
    h = h.wrapping_mul(M);
    h ^= h >> 15;
    h
}

pub fn to_base36(mut n: u32) -> String {
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut buf = [0u8; 7];
    let mut idx = buf.len();
    while n > 0 {
        idx -= 1;
        buf[idx] = DIGITS[(n % 36) as usize];
        n /= 36;
    }
    String::from_utf8_lossy(&buf[idx..]).into_owned()
}

pub fn hash(s: &str) -> String {
    to_base36(murmur2_32_gc(s, 1))
}

// Upstream's toBase62 returns "" for 0; replicated deliberately.
pub fn create_short_hash(s: &str) -> String {
    const DIGITS: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
    let mut n = murmur2_32_gc(s, 1) % 916_132_832;
    let mut out: Vec<u8> = Vec::new();
    while n > 0 {
        out.push(DIGITS[(n % 62) as usize]);
        n /= 62;
    }
    out.reverse();
    String::from_utf8(out).expect("base62 digits are ASCII")
}
