// The subset of Go's math/big used by this package, keeping its parsing and
// intermediate rounding semantics around Rust's arbitrary-precision integers.

use num_bigint::{BigInt, Sign};
use num_traits::{Pow, ToPrimitive};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Int(BigInt);

impl Int {
    pub fn new(x: i64) -> Int {
        Int(BigInt::from(x))
    }

    // Go's (*big.Int).SetString: base 0 detects prefixes and allows underscores
    // between digits or immediately after a prefix; explicit bases do neither.
    pub fn set_string(s: &str, base: u32) -> Option<Int> {
        let (negative, body) = match s.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, s.strip_prefix('+').unwrap_or(s)),
        };
        let (radix, digits, prefixed) = if base == 0 && body.len() > 1 && body.starts_with('0') {
            match body.as_bytes()[1] {
                b'b' | b'B' => (2, &body[2..], true),
                b'o' | b'O' => (8, &body[2..], true),
                b'x' | b'X' => (16, &body[2..], true),
                _ => (8, &body[1..], true),
            }
        } else {
            (if base == 0 { 10 } else { base }, body, false)
        };
        let mut previous_digit = prefixed;
        for byte in digits.bytes() {
            if byte == b'_' && base == 0 {
                if !previous_digit {
                    return None;
                }
                previous_digit = false;
            } else {
                let digit = match byte {
                    b'0'..=b'9' => u32::from(byte - b'0'),
                    b'a'..=b'z' => u32::from(byte - b'a') + 10,
                    b'A'..=b'Z' => u32::from(byte - b'A') + 10,
                    _ => return None,
                };
                if digit >= radix {
                    return None;
                }
                previous_digit = true;
            }
        }
        if !previous_digit {
            return None;
        }
        // num-bigint rejects a leading underscore even when it follows a Go prefix.
        let digits = digits.strip_prefix('_').unwrap_or(digits);
        let value = BigInt::parse_bytes(digits.as_bytes(), radix)?;
        Some(Int(if negative { -value } else { value }))
    }

    // Go's (*big.Int).Exp(x, y, nil) returns one for a non-positive exponent.
    #[must_use]
    pub fn exp(&self, y: &Int) -> Int {
        if y.0.sign() != Sign::Plus {
            return Int::new(1);
        }
        Int(Pow::pow(&self.0, y.0.magnitude()))
    }

    // Round the magnitude before restoring its sign, as big.Float.SetInt does.
    #[must_use]
    pub fn round_to_prec(&self, prec: usize) -> Int {
        assert!(prec > 0);
        let magnitude = self.0.magnitude();
        let bits = usize::try_from(magnitude.bits()).unwrap();
        if bits <= prec {
            return self.clone();
        }
        let shift = bits - prec;
        let mut rounded = magnitude >> shift;
        let half = magnitude.bit((shift - 1) as u64);
        let sticky = magnitude.trailing_zeros().is_some_and(|zeros| zeros < (shift - 1) as u64);
        if half && (sticky || rounded.bit(0)) {
            rounded += 1u8;
        }
        Int(BigInt::from_biguint(self.0.sign(), rounded << shift))
    }

    pub fn float64(&self) -> f64 {
        self.0.to_f64().unwrap()
    }

    pub fn string(&self) -> String {
        self.0.to_string()
    }
}
