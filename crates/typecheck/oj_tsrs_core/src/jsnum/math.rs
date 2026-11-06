// The Go "math" functions used by this package, ported from Go's darwin/arm64
// implementation so that results match bit for bit. Go's arm64 compiler fuses
// x*y+z into a single fused multiply-add, and its Exp is written in assembly
// using fused multiply-adds; mul_add reproduces both.

const SHIFT: u32 = 64 - 11 - 1;
const MASK: u64 = 0x7FF;
const BIAS: i64 = 1023;

pub fn modf(f: f64) -> (f64, f64) {
    let integer = f.trunc();
    let fractional = (f - integer).copysign(f);
    (integer, fractional)
}

// Go's archExp for arm64 (exp_arm64.s).
pub fn exp(x: f64) -> f64 {
    const LN2_HI: f64 = 6.931_471_803_691_238e-1;
    const LN2_LO: f64 = 1.908_214_929_270_587_7e-10;
    #[expect(clippy::approx_constant, reason = "the literal from Go's math.Exp, kept verbatim")]
    const LOG2E: f64 = 1.442_695_040_888_963_4;
    const OVERFLOW: f64 = 7.097_827_128_933_84e2;
    const UNDERFLOW: f64 = -7.451_332_191_019_411e2;
    const NEAR_ZERO: f64 = 1.0 / (1u64 << 28) as f64;
    const FRAC_MASK: u64 = 0x000f_ffff_ffff_ffff;
    const P1: f64 = 1.666_666_666_666_666_6e-1;
    const P2: f64 = -2.777_777_777_701_559_3e-3;
    const P3: f64 = 6.613_756_321_437_934e-5;
    const P4: f64 = -1.653_390_220_546_525_2e-6;
    const P5: f64 = 4.138_136_797_057_238_5e-8;

    if x.is_nan() {
        return x;
    }
    if x > OVERFLOW {
        return f64::INFINITY;
    }
    if x < UNDERFLOW {
        return 0.0;
    }
    if x.abs() < NEAR_ZERO {
        return 1.0 + x;
    }

    // argument reduction, x = k*ln2 + r,  |r| <= 0.5*ln2
    // computed as r = hi - lo for extra precision.
    let k = if x < 0.0 { LOG2E.mul_add(x, -0.5) } else { LOG2E.mul_add(x, 0.5) };
    let ki = k as i64;
    let k = ki as f64;
    let hi = (-k).mul_add(LN2_HI, x);
    let lo = k * LN2_LO;
    let r = hi - lo;
    let t = r * r;
    let mut c = t.mul_add(P5, P4);
    c = t.mul_add(c, P3);
    c = t.mul_add(c, P2);
    c = t.mul_add(c, P1);
    c = (-t).mul_add(c, r);
    let y = 1.0 - ((lo - (r * c) / (2.0 - c)) - hi);

    // inline Ldexp(y, k)
    let bits = y.to_bits();
    let frac = bits & FRAC_MASK;
    let mut e = ((bits >> 52) as i64).wrapping_add(ki);
    let mut m: f64 = 1.0;
    if e < 1 {
        e += 52;
        m = f64::from_bits(0x3cb0_0000_0000_0000); // 2**-52
    }
    m * f64::from_bits(((e as u64) << 52) | frac)
}

pub fn log(x: f64) -> f64 {
    const LN2_HI: f64 = 6.931_471_803_691_238e-1; /* 3fe62e42 fee00000 */
    const LN2_LO: f64 = 1.908_214_929_270_587_7e-10; /* 3dea39ef 35793c76 */
    const L1: f64 = 6.666_666_666_666_735e-1; /* 3FE55555 55555593 */
    const L2: f64 = 3.999_999_999_940_942e-1; /* 3FD99999 9997FA04 */
    const L3: f64 = 2.857_142_874_366_239e-1; /* 3FD24924 94229359 */
    const L4: f64 = 2.222_219_843_214_978_4e-1; /* 3FCC71C5 1D8E78AF */
    const L5: f64 = 1.818_357_216_161_805e-1; /* 3FC74664 96CB03DE */
    const L6: f64 = 1.531_383_769_920_937_3e-1; /* 3FC39A09 D078C69F */
    const L7: f64 = 1.479_819_860_511_658_6e-1; /* 3FC2F112 DF3E5244 */

    // special cases
    if x.is_nan() || x == f64::INFINITY {
        return x;
    } else if x < 0.0 {
        return f64::NAN;
    } else if x == 0.0 {
        return f64::NEG_INFINITY;
    }

    // reduce
    let (mut f1, mut ki) = frexp(x);
    if f1 < std::f64::consts::SQRT_2 / 2.0 {
        f1 *= 2.0;
        ki -= 1;
    }
    let f = f1 - 1.0;
    let k = ki as f64;

    // compute
    // t1 := s2 * (L1 + s4*(L3+s4*(L5+s4*L7)))
    // t2 := s4 * (L2 + s4*(L4+s4*L6))
    // R := t1 + t2
    // hfsq := 0.5 * f * f
    // return k*Ln2Hi - ((hfsq - (s*(hfsq+R) + k*Ln2Lo)) - f)
    // with the multiply-adds fused exactly as Go's arm64 compiler fuses them.
    let s = f / (2.0 + f);
    let s2 = s * s;
    let s4 = s2 * s2;
    let t2 = s4 * s4.mul_add(s4.mul_add(L6, L4), L2);
    let r = s2.mul_add(s4.mul_add(s4.mul_add(s4.mul_add(L7, L5), L3), L1), t2);
    let hfsq_f = 0.5 * f;
    let a = hfsq_f.mul_add(f, r);
    let b = s.mul_add(a, k * LN2_LO);
    let d = hfsq_f.mul_add(f, -b) - f;
    k.mul_add(LN2_HI, -d)
}

fn normalize(x: f64) -> (f64, i64) {
    const SMALLEST_NORMAL: f64 = 2.225_073_858_507_201_4e-308;
    if x.abs() < SMALLEST_NORMAL {
        return (x * (1u64 << 52) as f64, -52);
    }
    (x, 0)
}

pub fn frexp(f: f64) -> (f64, i64) {
    if f == 0.0 {
        return (f, 0);
    }
    if f.is_infinite() || f.is_nan() {
        return (f, 0);
    }
    let (f, mut exp) = normalize(f);
    let mut x = f.to_bits();
    exp += ((x >> SHIFT) & MASK) as i64 - BIAS + 1;
    x &= !(MASK << SHIFT);
    x |= ((-1 + BIAS) as u64) << SHIFT;
    (f64::from_bits(x), exp)
}

pub fn ldexp(frac: f64, exp: i64) -> f64 {
    if frac == 0.0 {
        return frac;
    }
    if frac.is_infinite() || frac.is_nan() {
        return frac;
    }
    let (frac, e) = normalize(frac);
    let mut exp = exp + e;
    let mut x = frac.to_bits();
    exp += ((x >> SHIFT) & MASK) as i64 - BIAS;
    if exp < -1075 {
        return 0f64.copysign(frac);
    }
    if exp > 1023 {
        if frac < 0.0 {
            return f64::NEG_INFINITY;
        }
        return f64::INFINITY;
    }
    let mut m: f64 = 1.0;
    if exp < -1022 {
        exp += 53;
        m = 1.0 / (1u64 << 53) as f64;
    }
    x &= !(MASK << SHIFT);
    x |= ((exp + BIAS) as u64) << SHIFT;
    m * f64::from_bits(x)
}

#[expect(
    clippy::float_cmp,
    reason = "Exact equality preserves JavaScript numeric identity; an epsilon comparison would merge distinct values."
)]
pub fn log2(x: f64) -> f64 {
    let (frac, exp) = frexp(x);
    // Make sure exact powers of two give an exact answer.
    // Don't depend on Log(0.5)*(1/Ln2)+exp being exactly exp-1.
    if frac == 0.5 {
        return (exp - 1) as f64;
    }
    // Go fuses this multiply-add on arm64.
    log(frac).mul_add(1.0 / std::f64::consts::LN_2, exp as f64)
}

fn is_odd_int(x: f64) -> bool {
    if x.abs() >= (1u64 << 53) as f64 {
        // 1 << 53 is the largest exact integer in the float64 format.
        // Any number outside this range will be truncated before the decimal point and therefore will always be
        // an even integer.
        return false;
    }

    let (xi, xf) = modf(x);
    xf == 0.0 && (xi as i64) & 1 == 1
}

#[expect(
    clippy::float_cmp,
    reason = "Exact equality preserves JavaScript numeric identity; an epsilon comparison would merge distinct values."
)]
pub fn pow(x: f64, y: f64) -> f64 {
    if y == 0.0 || x == 1.0 {
        return 1.0;
    } else if y == 1.0 {
        return x;
    } else if x.is_nan() || y.is_nan() {
        return f64::NAN;
    } else if x == 0.0 {
        if y < 0.0 {
            if x.is_sign_negative() && is_odd_int(y) {
                return f64::NEG_INFINITY;
            }
            return f64::INFINITY;
        } else if y > 0.0 {
            if x.is_sign_negative() && is_odd_int(y) {
                return x;
            }
            return 0.0;
        }
    } else if y.is_infinite() {
        if x == -1.0 {
            return 1.0;
        } else if (x.abs() < 1.0) == (y == f64::INFINITY) {
            return 0.0;
        }
        return f64::INFINITY;
    } else if x.is_infinite() {
        if x == f64::NEG_INFINITY {
            return pow(1.0 / x, -y); // Pow(-0, -y)
        }
        if y < 0.0 {
            return 0.0;
        } else if y > 0.0 {
            return f64::INFINITY;
        }
    } else if y == 0.5 {
        return x.sqrt();
    } else if y == -0.5 {
        return 1.0 / x.sqrt();
    }

    let (mut yi, mut yf) = modf(y.abs());
    if yf != 0.0 && x < 0.0 {
        return f64::NAN;
    }
    if yi >= (1u64 << 63) as f64 {
        // yi is a large even int that will lead to overflow (or underflow to 0)
        // for all x except -1 (x == 1 was handled earlier)
        if x == -1.0 {
            return 1.0;
        } else if (x.abs() < 1.0) == (y > 0.0) {
            return 0.0;
        }
        return f64::INFINITY;
    }

    // ans = a1 * 2**ae (= 1 for now).
    let mut a1: f64 = 1.0;
    let mut ae: i64 = 0;

    // ans *= x**yf
    if yf != 0.0 {
        if yf > 0.5 {
            yf -= 1.0;
            yi += 1.0;
        }
        a1 = exp(yf * log(x));
    }

    // ans *= x**yi
    // by multiplying in successive squarings
    // of x according to bits of yi.
    // accumulate powers of two into exp.
    let (mut x1, mut xe) = frexp(x);
    let mut i = yi as i64;
    while i != 0 {
        if !(-(1 << 12)..=(1 << 12)).contains(&xe) {
            // catch xe before it overflows the left shift below
            // Since i !=0 it has at least one bit still set, so ae will accumulate xe
            // on at least one more iteration, ae += xe is a lower bound on ae
            // the lower bound on ae exceeds the size of a float64 exp
            // so the final call to Ldexp will produce under/overflow (0/Inf)
            ae += xe;
            break;
        }
        if i & 1 == 1 {
            a1 *= x1;
            ae += xe;
        }
        x1 *= x1;
        xe <<= 1;
        if x1 < 0.5 {
            x1 += x1;
            xe -= 1;
        }
        i >>= 1;
    }

    // ans = a1*2**ae
    // if y < 0 { ans = 1 / ans }
    // but in the opposite order
    if y < 0.0 {
        a1 = 1.0 / a1;
        ae = -ae;
    }
    ldexp(a1, ae)
}
