use tsrs_core::jsnum::Number;

/// Convert the scanner's unsigned, separator-free spelling. Recovery spellings
/// outside this grammar are left to `jsnum::from_string`.
pub fn parse_literal(text: &str) -> Option<Number> {
    let bytes = text.as_bytes();
    let value = match bytes {
        [b'0', b'b' | b'B', digits @ ..] => parse_radix::<1>(digits)?,
        [b'0', b'o' | b'O', digits @ ..] => parse_radix::<3>(digits)?,
        [b'0', b'x' | b'X', digits @ ..] => parse_radix::<4>(digits)?,
        [b'0'..=b'9' | b'.', ..] => parse_decimal(text)?,
        _ => return None,
    };
    Some(Number(value))
}

fn parse_decimal(text: &str) -> Option<f64> {
    // Every decimal integer of at most 19 digits fits in a u64.
    if text.len() <= 19 {
        let mut value = 0_u64;
        value = match text.bytes().try_fold(value, |mut value, byte| {
            if !byte.is_ascii_digit() {
                return Err(text.parse().ok());
            }
            value = value * 10 + u64::from(byte & 15);
            Ok(value)
        }) {
            Ok(value) => value,
            Err(value) => return value,
        };
        return Some(value as f64);
    }
    text.parse().ok()
}

fn digit<const BITS: u32>(byte: u8) -> Option<u8> {
    let value = match byte {
        b'0'..=b'9' => byte & 15,
        b'a'..=b'f' | b'A'..=b'F' if BITS == 4 => (byte & 15) + 9,
        _ => return None,
    };
    (u32::from(value) < (1 << BITS)).then_some(value)
}

fn parse_radix<const BITS: u32>(digits: &[u8]) -> Option<f64> {
    if digits.is_empty() {
        return None;
    }
    if digits.len() > (64 / BITS) as usize {
        return parse_radix_slow::<BITS>(digits);
    }
    let mut value = 0_u64;
    value = digits
        .iter()
        .try_fold(value, |value, &byte| Some((value << BITS) | u64::from(digit::<BITS>(byte)?)))?;
    Some(value as f64)
}

#[cold]
fn parse_radix_slow<const BITS: u32>(digits: &[u8]) -> Option<f64> {
    let mut significand = 0_u64;
    let mut exponent = 0_u32;
    let mut sticky = 0_u8;
    for &byte in digits {
        let value = digit::<BITS>(byte)?;
        if significand < (1 << 54) {
            significand = (significand << BITS) | u64::from(value);
        } else {
            // Keep validating after overflow, without letting an arbitrarily
            // long literal overflow the exponent counter.
            exponent = (exponent + BITS).min(1024);
            sticky |= value;
        }
    }
    if exponent > 1023 {
        return Some(f64::INFINITY);
    }
    // Oxc's conversion retains at least 55 bits: 53 significant bits, a rounding
    // bit, and a sticky bit. The cast rounds once, ties to even; scaling is exact
    // unless it overflows to infinity.
    significand |= u64::from(sticky != 0);
    Some((significand as f64) * f64::from_bits(u64::from(exponent + 1023) << 52))
}
