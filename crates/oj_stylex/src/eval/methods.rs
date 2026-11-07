//! The VALID_CALLEES global surface and prototype-method dispatch.
// parity: evaluate-path.js:945-1053, transcribed for every reachable method.

use std::rc::Rc;

use crate::errors::{ErrorCode, StylexError};
use crate::eval::value::array_index;
use crate::eval::{JsObj, JsValue, is_nullish, js_to_number, js_to_string, truthy};
use crate::jsrt::{js_number_to_string, js_slice_utf16_checked, js_to_fixed, js_trim, utf16_cmp};

pub(crate) const VALID_CALLEES: [&str; 5] = ["String", "Number", "Math", "Object", "Array"];
pub(crate) const INVALID_METHODS: [&str; 7] = [
    "random",
    "assign",
    "defineProperties",
    "defineProperty",
    "freeze",
    "seal",
    "splice",
];

pub(crate) fn is_valid_callee(name: &str) -> bool {
    VALID_CALLEES.contains(&name)
}

pub(crate) fn is_invalid_method(name: &str) -> bool {
    INVALID_METHODS.contains(&name)
}

/// What `global[object][property]` holds upstream.
pub(crate) enum StaticMember {
    /// A function we implement: called with evaluated args.
    Fn(&'static str),
    /// A data property: upstream throws `func.apply is not a function`.
    NonCallable,
    /// Callable upstream but not modelled: structured loud error.
    Unsupported,
    /// Absent upstream: falls through to the value-callee branch.
    Unknown,
}

fn func_apply_error() -> StylexError {
    StylexError::new(ErrorCode::NonStaticValue, "func.apply is not a function")
}

fn unsupported_global(name: &str) -> StylexError {
    StylexError::unsupported_api(&format!("compile-time call to {name}"))
}

const MATH_FNS: [&str; 34] = [
    "abs", "acos", "acosh", "asin", "asinh", "atan", "atan2", "atanh", "cbrt", "ceil", "clz32",
    "cos", "cosh", "exp", "expm1", "floor", "fround", "hypot", "imul", "log", "log1p", "log2",
    "log10", "max", "min", "pow", "round", "sign", "sin", "sinh", "sqrt", "tan", "tanh", "trunc",
];
const MATH_CONSTS: [&str; 8] = [
    "E", "LN10", "LN2", "LOG10E", "LOG2E", "PI", "SQRT1_2", "SQRT2",
];

pub(crate) fn lookup_global_static(object: &str, property: &str) -> StaticMember {
    match object {
        "Math" => {
            if MATH_FNS.contains(&property) {
                StaticMember::Fn(property_static("Math", property))
            } else if MATH_CONSTS.contains(&property) {
                StaticMember::NonCallable
            } else if property == "f16round" || property == "sumPrecise" {
                StaticMember::Unsupported
            } else {
                StaticMember::Unknown
            }
        }
        "Object" => match property {
            "keys"
            | "values"
            | "entries"
            | "fromEntries"
            | "is"
            | "hasOwn"
            | "getOwnPropertyNames"
            | "getOwnPropertySymbols"
            | "isExtensible"
            | "isFrozen"
            | "isSealed" => StaticMember::Fn(property_static("Object", property)),
            "create"
            | "getOwnPropertyDescriptor"
            | "getOwnPropertyDescriptors"
            | "getPrototypeOf"
            | "groupBy"
            | "preventExtensions"
            | "setPrototypeOf" => StaticMember::Unsupported,
            "length" | "name" | "prototype" => StaticMember::NonCallable,
            _ => StaticMember::Unknown,
        },
        "Array" => match property {
            "isArray" | "of" | "from" => StaticMember::Fn(property_static("Array", property)),
            "fromAsync" => StaticMember::Unsupported,
            "length" | "name" | "prototype" => StaticMember::NonCallable,
            _ => StaticMember::Unknown,
        },
        "String" => match property {
            "fromCharCode" | "fromCodePoint" => {
                StaticMember::Fn(property_static("String", property))
            }
            "raw" => StaticMember::Unsupported,
            "length" | "name" | "prototype" => StaticMember::NonCallable,
            _ => StaticMember::Unknown,
        },
        "Number" => match property {
            "isFinite" | "isInteger" | "isNaN" | "isSafeInteger" | "parseFloat" | "parseInt" => {
                StaticMember::Fn(property_static("Number", property))
            }
            "EPSILON" | "MAX_SAFE_INTEGER" | "MAX_VALUE" | "MIN_SAFE_INTEGER" | "MIN_VALUE"
            | "NaN" | "NEGATIVE_INFINITY" | "POSITIVE_INFINITY" | "length" | "name"
            | "prototype" => StaticMember::NonCallable,
            _ => StaticMember::Unknown,
        },
        _ => StaticMember::Unknown,
    }
}

// Canonical "&'static str" for the (object, property) pair so `Fn` can carry it.
fn property_static(object: &'static str, property: &str) -> &'static str {
    let joined: &[(&str, &[&str])] = &[
        ("Math", &MATH_FNS),
        (
            "Object",
            &[
                "keys",
                "values",
                "entries",
                "fromEntries",
                "is",
                "hasOwn",
                "getOwnPropertyNames",
                "getOwnPropertySymbols",
                "isExtensible",
                "isFrozen",
                "isSealed",
            ],
        ),
        ("Array", &["isArray", "of", "from"]),
        ("String", &["fromCharCode", "fromCodePoint"]),
        (
            "Number",
            &[
                "isFinite",
                "isInteger",
                "isNaN",
                "isSafeInteger",
                "parseFloat",
                "parseInt",
            ],
        ),
    ];
    joined
        .iter()
        .find(|(o, _)| *o == object)
        .and_then(|(_, list)| list.iter().find(|p| **p == property))
        .copied()
        .expect("property listed in the matching table")
}

/// The arrow-closure hook: methods with callbacks call back into the evaluator.
pub(crate) trait ArrowCaller {
    fn call_arrow(&mut self, key: u32, args: Vec<JsValue>) -> Result<JsValue, StylexError>;
}

fn call_callback(
    ev: &mut dyn ArrowCaller,
    callback: &JsValue,
    args: Vec<JsValue>,
    method: &str,
) -> Result<JsValue, StylexError> {
    match callback {
        JsValue::Callable(crate::eval::Callable::Arrow(key)) => ev.call_arrow(*key, args),
        JsValue::Callable(_) => Err(unsupported_global(&format!(
            "a non-arrow callback in {method}()"
        ))),
        _ => Err(StylexError::new(
            ErrorCode::NonStaticValue,
            format!(
                "{} is not a function",
                js_to_string(callback).unwrap_or_default()
            ),
        )),
    }
}

fn lone_surrogate_slice(context: &'static str) -> StylexError {
    StylexError::lone_surrogate(context)
}

fn utf16_len(s: &str) -> usize {
    s.encode_utf16().count()
}

// ES ToIntegerOrInfinity.
fn to_integer_or_infinity(n: f64) -> f64 {
    if n.is_nan() { 0.0 } else { n.trunc() }
}

fn relative_index(n: f64, len: usize) -> isize {
    let len = len as f64;
    let k = to_integer_or_infinity(n);
    let clamped = if k < 0.0 {
        (len + k).max(0.0)
    } else {
        k.min(len)
    };
    clamped as isize
}

fn arg(args: &[JsValue], i: usize) -> JsValue {
    args.get(i).cloned().unwrap_or(JsValue::Undefined)
}

fn num_arg(args: &[JsValue], i: usize) -> f64 {
    js_to_number(&arg(args, i))
}

fn str_arg(args: &[JsValue], i: usize, method: &str) -> Result<String, StylexError> {
    js_to_string(&arg(args, i))
        .ok_or_else(|| unsupported_global(&format!("a function argument to {method}()")))
}

// parity: ES SameValue.
fn same_value(a: &JsValue, b: &JsValue) -> bool {
    match (a, b) {
        (JsValue::Num(x), JsValue::Num(y)) => {
            if x.is_nan() && y.is_nan() {
                true
            } else {
                x == y && x.is_sign_positive() == y.is_sign_positive()
            }
        }
        _ => crate::eval::js_strict_eq(a, b),
    }
}

/// ES-ordered own enumerable string keys (index keys ascending, then insertion).
fn es_own_keys(obj: &JsObj) -> Vec<String> {
    let mut index_keys: Vec<(u32, &str)> = Vec::new();
    let mut named: Vec<&str> = Vec::new();
    for (key, _) in obj.entries() {
        match array_index(key) {
            Some(n) => index_keys.push((n, key)),
            None => named.push(key),
        }
    }
    index_keys.sort_by_key(|(n, _)| *n);
    index_keys
        .into_iter()
        .map(|(_, k)| k.to_string())
        .chain(named.into_iter().map(str::to_string))
        .collect()
}

fn keyable(value: &JsValue, method: &str) -> Result<Option<KeySource>, StylexError> {
    Ok(match value {
        JsValue::Null | JsValue::Undefined => {
            return Err(StylexError::new(
                ErrorCode::NonStaticValue,
                "Cannot convert undefined or null to object",
            ));
        }
        JsValue::Obj(obj) => Some(KeySource::Obj(Rc::clone(obj))),
        JsValue::Arr(items) => Some(KeySource::Arr(items.len())),
        JsValue::Str(s) => Some(KeySource::Str(utf16_len(s))),
        JsValue::Num(_) | JsValue::Bool(_) | JsValue::Proxy(_) => None,
        JsValue::Callable(_) => {
            return Err(unsupported_global(&format!("{method} of a function")));
        }
    })
}

enum KeySource {
    Obj(Rc<JsObj>),
    Arr(usize),
    Str(usize),
}

fn own_keys_of(value: &JsValue, method: &str) -> Result<Vec<String>, StylexError> {
    Ok(match keyable(value, method)? {
        None => Vec::new(),
        Some(KeySource::Obj(obj)) => es_own_keys(&obj),
        Some(KeySource::Arr(len)) | Some(KeySource::Str(len)) => {
            (0..len).map(|i| i.to_string()).collect()
        }
    })
}

fn own_value_of(value: &JsValue, key: &str) -> Result<JsValue, StylexError> {
    Ok(match value {
        JsValue::Obj(obj) => obj.get(key).cloned().unwrap_or(JsValue::Undefined),
        JsValue::Arr(items) => array_index(key)
            .and_then(|i| items.get(i as usize))
            .cloned()
            .unwrap_or(JsValue::Undefined),
        JsValue::Str(s) => match array_index(key) {
            Some(i) if (i as usize) < utf16_len(s) => JsValue::Str(
                js_slice_utf16_checked(s, i as usize, i as isize + 1)
                    .map_err(|_| lone_surrogate_slice("string indexing"))?,
            ),
            _ => JsValue::Undefined,
        },
        _ => JsValue::Undefined,
    })
}

pub(crate) fn call_global_static(
    ev: &mut dyn ArrowCaller,
    object: &str,
    method: &'static str,
    args: &[JsValue],
) -> Result<JsValue, StylexError> {
    match object {
        "Math" => call_math(method, args),
        "Object" => call_object_static(method, args),
        "Array" => call_array_static(ev, method, args),
        "String" => call_string_static(method, args),
        "Number" => call_number_static(method, args),
        _ => Err(unsupported_global(object)),
    }
}

fn call_math(method: &str, args: &[JsValue]) -> Result<JsValue, StylexError> {
    let nums: Vec<f64> = args.iter().map(js_to_number).collect();
    let x = nums.first().copied().unwrap_or(f64::NAN);
    let y = nums.get(1).copied().unwrap_or(f64::NAN);
    let result = match method {
        // parity: JS Math.min/max — any NaN operand wins (Rust min/max drop it).
        "min" => nums.iter().copied().fold(f64::INFINITY, |a, b| {
            if a.is_nan() || b.is_nan() {
                f64::NAN
            } else {
                a.min(b)
            }
        }),
        "max" => nums.iter().copied().fold(f64::NEG_INFINITY, |a, b| {
            if a.is_nan() || b.is_nan() {
                f64::NAN
            } else {
                a.max(b)
            }
        }),
        "abs" => x.abs(),
        "acos" => x.acos(),
        "acosh" => x.acosh(),
        "asin" => x.asin(),
        "asinh" => x.asinh(),
        "atan" => x.atan(),
        "atan2" => x.atan2(y),
        "atanh" => x.atanh(),
        "cbrt" => x.cbrt(),
        "ceil" => x.ceil(),
        "clz32" => f64::from(crate::eval::to_uint32(x).leading_zeros()),
        "cos" => x.cos(),
        "cosh" => x.cosh(),
        "exp" => x.exp(),
        "expm1" => x.exp_m1(),
        "floor" => x.floor(),
        "fround" => x as f32 as f64,
        "hypot" => nums.iter().copied().fold(0.0f64, f64::hypot),
        "imul" => f64::from(crate::eval::to_int32(x).wrapping_mul(crate::eval::to_int32(y))),
        "log" => x.ln(),
        "log1p" => x.ln_1p(),
        "log2" => x.log2(),
        "log10" => x.log10(),
        "pow" => crate::eval::js_pow(x, y),
        "round" => crate::jsrt::js_math_round(x),
        "sign" => {
            if x.is_nan() || x == 0.0 {
                x
            } else if x > 0.0 {
                1.0
            } else {
                -1.0
            }
        }
        "sin" => x.sin(),
        "sinh" => x.sinh(),
        "sqrt" => x.sqrt(),
        "tan" => x.tan(),
        "tanh" => x.tanh(),
        "trunc" => x.trunc(),
        _ => return Err(unsupported_global(&format!("Math.{method}"))),
    };
    Ok(JsValue::Num(result))
}

fn call_object_static(method: &str, args: &[JsValue]) -> Result<JsValue, StylexError> {
    let target = arg(args, 0);
    match method {
        "keys" => Ok(JsValue::array(
            own_keys_of(&target, "Object.keys")?
                .into_iter()
                .map(JsValue::Str)
                .collect(),
        )),
        "values" => {
            let keys = own_keys_of(&target, "Object.values")?;
            let values = keys
                .iter()
                .map(|k| own_value_of(&target, k))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(JsValue::array(values))
        }
        "entries" => {
            let keys = own_keys_of(&target, "Object.entries")?;
            let entries = keys
                .into_iter()
                .map(|k| {
                    let value = own_value_of(&target, &k)?;
                    Ok(JsValue::array(vec![JsValue::Str(k), value]))
                })
                .collect::<Result<Vec<_>, StylexError>>()?;
            Ok(JsValue::array(entries))
        }
        "fromEntries" => {
            let JsValue::Arr(entries) = &target else {
                return Err(unsupported_global(
                    "Object.fromEntries of a non-array iterable",
                ));
            };
            let mut obj = JsObj::default();
            for entry in entries.iter() {
                let JsValue::Arr(pair) = entry else {
                    return Err(unsupported_global(
                        "Object.fromEntries of non-array entries",
                    ));
                };
                let key = js_to_string(&pair.first().cloned().unwrap_or(JsValue::Undefined))
                    .ok_or_else(|| unsupported_global("a function key in Object.fromEntries"))?;
                obj.insert(key, pair.get(1).cloned().unwrap_or(JsValue::Undefined));
            }
            Ok(JsValue::object(obj))
        }
        "is" => Ok(JsValue::Bool(same_value(&target, &arg(args, 1)))),
        "hasOwn" => {
            let key = js_to_string(&arg(args, 1))
                .ok_or_else(|| unsupported_global("a function key in Object.hasOwn"))?;
            let has = match keyable(&target, "Object.hasOwn")? {
                None => false,
                Some(KeySource::Obj(obj)) => obj.get(&key).is_some(),
                Some(KeySource::Arr(len)) | Some(KeySource::Str(len)) => {
                    key == "length" || array_index(&key).is_some_and(|i| (i as usize) < len)
                }
            };
            Ok(JsValue::Bool(has))
        }
        "getOwnPropertyNames" => {
            let mut keys = own_keys_of(&target, "Object.getOwnPropertyNames")?;
            if matches!(target, JsValue::Arr(_) | JsValue::Str(_)) {
                keys.push("length".to_string());
            }
            Ok(JsValue::array(keys.into_iter().map(JsValue::Str).collect()))
        }
        "getOwnPropertySymbols" => match target {
            JsValue::Null | JsValue::Undefined => Err(StylexError::new(
                ErrorCode::NonStaticValue,
                "Cannot convert undefined or null to object",
            )),
            _ => Ok(JsValue::array(Vec::new())),
        },
        "isExtensible" => Ok(JsValue::Bool(matches!(
            target,
            JsValue::Obj(_) | JsValue::Arr(_) | JsValue::Proxy(_) | JsValue::Callable(_)
        ))),
        "isFrozen" | "isSealed" => Ok(JsValue::Bool(!matches!(
            target,
            JsValue::Obj(_) | JsValue::Arr(_) | JsValue::Proxy(_) | JsValue::Callable(_)
        ))),
        _ => Err(unsupported_global(&format!("Object.{method}"))),
    }
}

fn call_array_static(
    ev: &mut dyn ArrowCaller,
    method: &str,
    args: &[JsValue],
) -> Result<JsValue, StylexError> {
    match method {
        "isArray" => Ok(JsValue::Bool(matches!(arg(args, 0), JsValue::Arr(_)))),
        "of" => Ok(JsValue::array(args.to_vec())),
        "from" => {
            let source = arg(args, 0);
            let items: Vec<JsValue> = match &source {
                JsValue::Str(s) => s.chars().map(|c| JsValue::Str(c.to_string())).collect(),
                JsValue::Arr(items) => items.as_ref().clone(),
                JsValue::Obj(obj) => {
                    let len =
                        js_to_number(&obj.get("length").cloned().unwrap_or(JsValue::Undefined));
                    let len = to_integer_or_infinity(len).max(0.0);
                    if !(0.0..=65535.0).contains(&len) {
                        return Err(unsupported_global("Array.from of a huge array-like"));
                    }
                    (0..len as usize)
                        .map(|i| {
                            obj.get(&i.to_string())
                                .cloned()
                                .unwrap_or(JsValue::Undefined)
                        })
                        .collect()
                }
                JsValue::Num(_) | JsValue::Bool(_) => Vec::new(),
                _ => return Err(unsupported_global("Array.from of this source")),
            };
            let mapper = arg(args, 1);
            if matches!(mapper, JsValue::Undefined) {
                return Ok(JsValue::array(items));
            }
            let mut out = Vec::with_capacity(items.len());
            for (i, item) in items.into_iter().enumerate() {
                out.push(call_callback(
                    ev,
                    &mapper,
                    vec![item, JsValue::Num(i as f64)],
                    "Array.from",
                )?);
            }
            Ok(JsValue::array(out))
        }
        _ => Err(unsupported_global(&format!("Array.{method}"))),
    }
}

// ES ToUint16.
fn to_uint16(n: f64) -> u16 {
    (crate::eval::to_uint32(n) & 0xFFFF) as u16
}

fn utf16_units_to_string(units: &[u16], context: &'static str) -> Result<String, StylexError> {
    String::from_utf16(units).map_err(|_| lone_surrogate_slice(context))
}

fn call_string_static(method: &str, args: &[JsValue]) -> Result<JsValue, StylexError> {
    match method {
        "fromCharCode" => {
            let units: Vec<u16> = args.iter().map(|a| to_uint16(js_to_number(a))).collect();
            Ok(JsValue::Str(utf16_units_to_string(
                &units,
                "String.fromCharCode",
            )?))
        }
        "fromCodePoint" => {
            let mut out = String::new();
            for value in args {
                let n = js_to_number(value);
                if n.fract() != 0.0 || !(0.0..=1_114_111.0).contains(&n) {
                    return Err(StylexError::new(
                        ErrorCode::NonStaticValue,
                        format!("Invalid code point {}", js_number_to_string(n)),
                    ));
                }
                let code = n as u32;
                match char::from_u32(code) {
                    Some(c) => out.push(c),
                    None => return Err(lone_surrogate_slice("String.fromCodePoint")),
                }
            }
            Ok(JsValue::Str(out))
        }
        _ => Err(unsupported_global(&format!("String.{method}"))),
    }
}

fn call_number_static(method: &str, args: &[JsValue]) -> Result<JsValue, StylexError> {
    let value = arg(args, 0);
    match method {
        "isFinite" => Ok(JsValue::Bool(
            matches!(value, JsValue::Num(n) if n.is_finite()),
        )),
        "isNaN" => Ok(JsValue::Bool(
            matches!(value, JsValue::Num(n) if n.is_nan()),
        )),
        "isInteger" => Ok(JsValue::Bool(
            matches!(value, JsValue::Num(n) if n.is_finite() && n.fract() == 0.0),
        )),
        "isSafeInteger" => Ok(JsValue::Bool(
            matches!(value, JsValue::Num(n) if n.is_finite() && n.fract() == 0.0 && n.abs() <= 9_007_199_254_740_991.0),
        )),
        "parseInt" => {
            let input = js_to_string(&value)
                .ok_or_else(|| unsupported_global("a function argument to parseInt()"))?;
            Ok(JsValue::Num(js_parse_int(&input, num_arg(args, 1))))
        }
        "parseFloat" => {
            let input = js_to_string(&value)
                .ok_or_else(|| unsupported_global("a function argument to parseFloat()"))?;
            Ok(JsValue::Num(js_parse_float(&input)))
        }
        _ => Err(unsupported_global(&format!("Number.{method}"))),
    }
}

// parity: ES parseInt (trim, sign, radix with 0x handling, maximal prefix).
pub(crate) fn js_parse_int(input: &str, radix: f64) -> f64 {
    let s = js_trim(input);
    let (sign, s) = match s.strip_prefix('-') {
        Some(rest) => (-1.0, rest),
        None => (1.0, s.strip_prefix('+').unwrap_or(s)),
    };
    let mut radix = crate::eval::to_int32(radix);
    let mut s = s;
    if radix == 16 || radix == 0 {
        if let Some(rest) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            s = rest;
            radix = 16;
        } else if radix == 0 {
            radix = 10;
        }
    }
    if !(2..=36).contains(&radix) {
        return f64::NAN;
    }
    let digits: Vec<u32> = s
        .chars()
        .map_while(|c| c.to_digit(36).filter(|d| *d < radix as u32))
        .collect();
    if digits.is_empty() {
        return f64::NAN;
    }
    let mut out = 0.0f64;
    for d in digits {
        out = out * f64::from(radix) + f64::from(d);
    }
    sign * out
}

// parity: ES parseFloat (maximal StrDecimalLiteral prefix).
pub(crate) fn js_parse_float(input: &str) -> f64 {
    let s = js_trim(input);
    let bytes = s.as_bytes();
    let mut i = 0;
    if i < bytes.len() && (bytes[i] == b'+' || bytes[i] == b'-') {
        i += 1;
    }
    if s[i..].starts_with("Infinity") {
        return if bytes.first() == Some(&b'-') {
            f64::NEG_INFINITY
        } else {
            f64::INFINITY
        };
    }
    let digits_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
    }
    if i == digits_start || (i == digits_start + 1 && bytes[digits_start] == b'.') {
        return f64::NAN;
    }
    let mantissa_end = i;
    if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
        let mut j = i + 1;
        if j < bytes.len() && (bytes[j] == b'+' || bytes[j] == b'-') {
            j += 1;
        }
        let exp_digits = j;
        while j < bytes.len() && bytes[j].is_ascii_digit() {
            j += 1;
        }
        if j > exp_digits {
            i = j;
        }
    }
    let _ = mantissa_end;
    s[..i].parse::<f64>().unwrap_or(f64::NAN)
}

/// Prototype-method dispatch for evaluated receivers (the `func == null`
/// fallback in evaluate-path.js, where real JS methods run).
pub(crate) enum ProtoLookup {
    Fn(&'static str),
    Unsupported(&'static str),
    NotFound,
}

const STR_METHODS: [&str; 26] = [
    "at",
    "charAt",
    "charCodeAt",
    "codePointAt",
    "concat",
    "endsWith",
    "includes",
    "indexOf",
    "lastIndexOf",
    "padEnd",
    "padStart",
    "repeat",
    "replace",
    "replaceAll",
    "slice",
    "split",
    "startsWith",
    "substr",
    "substring",
    "toLowerCase",
    "toString",
    "toUpperCase",
    "trim",
    "trimEnd",
    "trimStart",
    "valueOf",
];
const STR_UNSUPPORTED: [&str; 22] = [
    "anchor",
    "big",
    "blink",
    "bold",
    "fixed",
    "fontcolor",
    "fontsize",
    "isWellFormed",
    "italics",
    "link",
    "localeCompare",
    "match",
    "matchAll",
    "normalize",
    "search",
    "small",
    "strike",
    "sub",
    "sup",
    "toLocaleLowerCase",
    "toLocaleUpperCase",
    "toWellFormed",
];

const ARR_METHODS: [&str; 25] = [
    "at",
    "concat",
    "every",
    "filter",
    "find",
    "findIndex",
    "findLast",
    "findLastIndex",
    "flat",
    "flatMap",
    "forEach",
    "includes",
    "indexOf",
    "join",
    "lastIndexOf",
    "map",
    "reduce",
    "reduceRight",
    "reverse",
    "slice",
    "some",
    "sort",
    "toReversed",
    "toSorted",
    "toString",
];
const ARR_UNSUPPORTED: [&str; 13] = [
    "copyWithin",
    "entries",
    "fill",
    "keys",
    "pop",
    "push",
    "shift",
    "splice",
    "toLocaleString",
    "toSpliced",
    "unshift",
    "values",
    "with",
];

pub(crate) const NUM_METHODS: [&str; 3] = ["toFixed", "toString", "valueOf"];
pub(crate) const NUM_UNSUPPORTED: [&str; 3] = ["toExponential", "toLocaleString", "toPrecision"];

const OBJ_METHODS: [&str; 6] = [
    "hasOwnProperty",
    "isPrototypeOf",
    "propertyIsEnumerable",
    "toLocaleString",
    "toString",
    "valueOf",
];
const OBJ_UNSUPPORTED: [&str; 4] = [
    "__defineGetter__",
    "__defineSetter__",
    "__lookupGetter__",
    "__lookupSetter__",
];

fn find_static(table: &'static [&'static str], name: &str) -> Option<&'static str> {
    table.iter().find(|m| **m == name).copied()
}

pub(crate) fn lookup_proto_method(receiver: &JsValue, name: &str) -> ProtoLookup {
    let (methods, unsupported): (&'static [&'static str], &'static [&'static str]) = match receiver
    {
        JsValue::Str(_) => (&STR_METHODS, &STR_UNSUPPORTED),
        JsValue::Arr(_) => (&ARR_METHODS, &ARR_UNSUPPORTED),
        JsValue::Num(_) => (&NUM_METHODS, &NUM_UNSUPPORTED),
        JsValue::Bool(_) => (&["toString", "valueOf"][..], &[][..]),
        JsValue::Obj(_) => (&OBJ_METHODS, &OBJ_UNSUPPORTED),
        _ => (&[][..], &[][..]),
    };
    if let Some(found) = find_static(methods, name) {
        ProtoLookup::Fn(found)
    } else if let Some(found) = find_static(unsupported, name) {
        ProtoLookup::Unsupported(found)
    } else {
        ProtoLookup::NotFound
    }
}

pub(crate) fn call_proto_method(
    ev: &mut dyn ArrowCaller,
    receiver: &JsValue,
    method: &'static str,
    args: &[JsValue],
) -> Result<JsValue, StylexError> {
    match receiver {
        JsValue::Str(s) => call_string_method(s, method, args),
        JsValue::Arr(items) => call_array_method(ev, receiver, items, method, args),
        JsValue::Num(n) => call_number_method(*n, method, args),
        JsValue::Bool(b) => match method {
            "toString" => Ok(JsValue::Str(b.to_string())),
            "valueOf" => Ok(JsValue::Bool(*b)),
            _ => Err(unsupported_global(method)),
        },
        JsValue::Obj(obj) => call_object_method(receiver, obj, method, args),
        _ => Err(unsupported_global(method)),
    }
}

fn checked_slice(
    s: &str,
    start: isize,
    end: isize,
    context: &'static str,
) -> Result<String, StylexError> {
    let start = start.max(0) as usize;
    js_slice_utf16_checked(s, start, end).map_err(|_| lone_surrogate_slice(context))
}

fn call_string_method(
    s: &str,
    method: &'static str,
    args: &[JsValue],
) -> Result<JsValue, StylexError> {
    let len = utf16_len(s) as isize;
    let units = || s.encode_utf16().collect::<Vec<u16>>();
    match method {
        "toString" | "valueOf" => Ok(JsValue::Str(s.to_string())),
        "toUpperCase" => Ok(JsValue::Str(s.to_uppercase())),
        "toLowerCase" => Ok(JsValue::Str(s.to_lowercase())),
        "trim" => Ok(JsValue::Str(js_trim(s).to_string())),
        "trimStart" => Ok(JsValue::Str(
            s.trim_start_matches(crate::jsrt::is_js_whitespace)
                .to_string(),
        )),
        "trimEnd" => Ok(JsValue::Str(
            s.trim_end_matches(crate::jsrt::is_js_whitespace)
                .to_string(),
        )),
        "charAt" => {
            let i = to_integer_or_infinity(num_arg(args, 0));
            if i < 0.0 || i >= len as f64 {
                return Ok(JsValue::Str(String::new()));
            }
            Ok(JsValue::Str(checked_slice(
                s,
                i as isize,
                i as isize + 1,
                "String.prototype.charAt",
            )?))
        }
        "at" => {
            let mut i = to_integer_or_infinity(num_arg(args, 0));
            if i < 0.0 {
                i += len as f64;
            }
            if i < 0.0 || i >= len as f64 {
                return Ok(JsValue::Undefined);
            }
            Ok(JsValue::Str(checked_slice(
                s,
                i as isize,
                i as isize + 1,
                "String.prototype.at",
            )?))
        }
        "charCodeAt" => {
            let i = to_integer_or_infinity(num_arg(args, 0));
            if i < 0.0 || i >= len as f64 {
                return Ok(JsValue::Num(f64::NAN));
            }
            Ok(JsValue::Num(f64::from(units()[i as usize])))
        }
        "codePointAt" => {
            let i = to_integer_or_infinity(num_arg(args, 0));
            if i < 0.0 || i >= len as f64 {
                return Ok(JsValue::Undefined);
            }
            let us = units();
            let first = us[i as usize];
            let code = if (0xD800..=0xDBFF).contains(&first)
                && let Some(second) = us.get(i as usize + 1)
                && (0xDC00..=0xDFFF).contains(second)
            {
                0x10000 + (u32::from(first) - 0xD800) * 0x400 + (u32::from(*second) - 0xDC00)
            } else {
                u32::from(first)
            };
            Ok(JsValue::Num(f64::from(code)))
        }
        "indexOf" | "lastIndexOf" | "includes" | "startsWith" | "endsWith" => {
            let needle = str_arg(args, 0, method)?;
            let position = args.get(1).map(js_to_number);
            string_search(s, &needle, position, method)
        }
        "slice" => {
            let start = relative_index(num_arg(args, 0), len as usize);
            let end = match args.get(1) {
                None | Some(JsValue::Undefined) => len,
                Some(v) => relative_index(js_to_number(v), len as usize),
            };
            if start >= end {
                return Ok(JsValue::Str(String::new()));
            }
            Ok(JsValue::Str(checked_slice(
                s,
                start,
                end,
                "String.prototype.slice",
            )?))
        }
        "substring" => {
            let a = to_integer_or_infinity(num_arg(args, 0)).clamp(0.0, len as f64) as isize;
            let b = match args.get(1) {
                None | Some(JsValue::Undefined) => len,
                Some(v) => to_integer_or_infinity(js_to_number(v)).clamp(0.0, len as f64) as isize,
            };
            let (start, end) = if a <= b { (a, b) } else { (b, a) };
            Ok(JsValue::Str(checked_slice(
                s,
                start,
                end,
                "String.prototype.substring",
            )?))
        }
        "substr" => {
            let mut start = to_integer_or_infinity(num_arg(args, 0)) as isize;
            if start < 0 {
                start = (len + start).max(0);
            }
            let count = match args.get(1) {
                None | Some(JsValue::Undefined) => len - start,
                Some(v) => to_integer_or_infinity(js_to_number(v)) as isize,
            };
            // parity: JS adds as floats, so Infinity clamps instead of wrapping.
            let end = start.saturating_add(count.max(0)).min(len);
            if start >= end {
                return Ok(JsValue::Str(String::new()));
            }
            Ok(JsValue::Str(checked_slice(
                s,
                start,
                end,
                "String.prototype.substr",
            )?))
        }
        "concat" => {
            let mut out = s.to_string();
            for value in args {
                out.push_str(&js_to_string(value).ok_or_else(|| {
                    unsupported_global("a function argument to String.prototype.concat()")
                })?);
            }
            Ok(JsValue::Str(out))
        }
        "repeat" => {
            let n = to_integer_or_infinity(num_arg(args, 0));
            if n < 0.0 || n.is_infinite() {
                return Err(StylexError::new(
                    ErrorCode::NonStaticValue,
                    format!(
                        "Invalid count value: {}",
                        js_number_to_string(num_arg(args, 0))
                    ),
                ));
            }
            if n * len as f64 > 65535.0 {
                return Err(unsupported_global("String.prototype.repeat beyond 64KiB"));
            }
            Ok(JsValue::Str(s.repeat(n as usize)))
        }
        "padStart" | "padEnd" => {
            let target = to_integer_or_infinity(num_arg(args, 0));
            let filler = match args.get(1) {
                None | Some(JsValue::Undefined) => " ".to_string(),
                Some(v) => js_to_string(v).ok_or_else(|| {
                    unsupported_global("a function argument to String.prototype.pad()")
                })?,
            };
            if target > 65535.0 {
                return Err(unsupported_global("String.prototype.pad beyond 64KiB"));
            }
            if target <= len as f64 || filler.is_empty() {
                return Ok(JsValue::Str(s.to_string()));
            }
            let missing = target as usize - len as usize;
            let filler_units: Vec<u16> = filler.encode_utf16().collect();
            let pad_units: Vec<u16> = filler_units.iter().copied().cycle().take(missing).collect();
            let pad = utf16_units_to_string(&pad_units, "String.prototype.pad")?;
            Ok(JsValue::Str(if method == "padStart" {
                format!("{pad}{s}")
            } else {
                format!("{s}{pad}")
            }))
        }
        "split" => {
            let limit = match args.get(1) {
                None | Some(JsValue::Undefined) => u32::MAX,
                Some(v) => crate::eval::to_uint32(js_to_number(v)),
            };
            let separator = match args.first() {
                None | Some(JsValue::Undefined) => {
                    if limit == 0 {
                        return Ok(JsValue::array(Vec::new()));
                    }
                    return Ok(JsValue::array(vec![JsValue::Str(s.to_string())]));
                }
                Some(v) => js_to_string(v).ok_or_else(|| {
                    unsupported_global("a function separator in String.prototype.split()")
                })?,
            };
            let mut parts: Vec<JsValue> = Vec::new();
            if separator.is_empty() {
                let us = units();
                for i in 0..us.len().min(limit as usize) {
                    parts.push(JsValue::Str(checked_slice(
                        s,
                        i as isize,
                        i as isize + 1,
                        "String.prototype.split",
                    )?));
                }
            } else {
                for piece in s.split(&separator) {
                    if parts.len() >= limit as usize {
                        break;
                    }
                    parts.push(JsValue::Str(piece.to_string()));
                }
            }
            Ok(JsValue::array(parts))
        }
        "replace" | "replaceAll" => Err(unsupported_global("String.prototype.replace")),
        _ => Err(unsupported_global(method)),
    }
}

fn string_search(
    s: &str,
    needle: &str,
    position: Option<f64>,
    method: &str,
) -> Result<JsValue, StylexError> {
    let hay: Vec<u16> = s.encode_utf16().collect();
    let pat: Vec<u16> = needle.encode_utf16().collect();
    let len = hay.len();
    let find_from = |from: usize| -> Option<usize> {
        if pat.is_empty() {
            return Some(from.min(len));
        }
        if pat.len() > len {
            return None;
        }
        (from..=len.saturating_sub(pat.len())).find(|&i| hay[i..i + pat.len()] == pat[..])
    };
    match method {
        "indexOf" => {
            let from = position
                .map_or(0.0, to_integer_or_infinity)
                .clamp(0.0, len as f64) as usize;
            Ok(JsValue::Num(find_from(from).map_or(-1.0, |i| i as f64)))
        }
        "includes" => {
            let from = position
                .map_or(0.0, to_integer_or_infinity)
                .clamp(0.0, len as f64) as usize;
            Ok(JsValue::Bool(find_from(from).is_some()))
        }
        "lastIndexOf" => {
            let from = match position {
                None => len as f64,
                Some(p) => {
                    let p = to_integer_or_infinity(p);
                    if p.is_nan() { len as f64 } else { p }
                }
            }
            .clamp(0.0, len as f64) as usize;
            if pat.is_empty() {
                return Ok(JsValue::Num(from.min(len) as f64));
            }
            let mut best: f64 = -1.0;
            let mut i = 0usize;
            while let Some(found) = find_from(i) {
                if found > from {
                    break;
                }
                best = found as f64;
                i = found + 1;
            }
            Ok(JsValue::Num(best))
        }
        "startsWith" => {
            let from = position
                .map_or(0.0, to_integer_or_infinity)
                .clamp(0.0, len as f64) as usize;
            Ok(JsValue::Bool(
                from + pat.len() <= len && hay[from..from + pat.len()] == pat[..],
            ))
        }
        "endsWith" => {
            let end = match position {
                None => len,
                Some(p) => to_integer_or_infinity(p).clamp(0.0, len as f64) as usize,
            };
            Ok(JsValue::Bool(
                pat.len() <= end && hay[end - pat.len()..end] == pat[..],
            ))
        }
        _ => Err(unsupported_global(method)),
    }
}

fn element_string(value: &JsValue, method: &str) -> Result<String, StylexError> {
    if is_nullish(value) {
        return Ok(String::new());
    }
    js_to_string(value)
        .ok_or_else(|| unsupported_global(&format!("a function element in {method}()")))
}

fn call_array_method(
    ev: &mut dyn ArrowCaller,
    receiver: &JsValue,
    items: &Rc<Vec<JsValue>>,
    method: &'static str,
    args: &[JsValue],
) -> Result<JsValue, StylexError> {
    let len = items.len();
    let callback_args =
        |item: &JsValue, i: usize| vec![item.clone(), JsValue::Num(i as f64), receiver.clone()];
    match method {
        "toString" | "join" => {
            let separator = match (method, args.first()) {
                ("join", Some(JsValue::Undefined)) | ("join", None) | ("toString", _) => {
                    ",".to_string()
                }
                ("join", Some(v)) => js_to_string(v).ok_or_else(|| {
                    unsupported_global("a function separator in Array.prototype.join()")
                })?,
                _ => ",".to_string(),
            };
            let parts = items
                .iter()
                .map(|item| element_string(item, "Array.prototype.join"))
                .collect::<Result<Vec<_>, _>>()?;
            Ok(JsValue::Str(parts.join(&separator)))
        }
        "at" => {
            let mut i = to_integer_or_infinity(num_arg(args, 0));
            if i < 0.0 {
                i += len as f64;
            }
            if i < 0.0 || i >= len as f64 {
                return Ok(JsValue::Undefined);
            }
            Ok(items[i as usize].clone())
        }
        "includes" => Ok(JsValue::Bool(
            items
                .iter()
                .any(|item| same_value_zero(item, &arg(args, 0))),
        )),
        "indexOf" => Ok(JsValue::Num(
            items
                .iter()
                .position(|item| crate::eval::js_strict_eq(item, &arg(args, 0)))
                .map_or(-1.0, |i| i as f64),
        )),
        "lastIndexOf" => Ok(JsValue::Num(
            items
                .iter()
                .rposition(|item| crate::eval::js_strict_eq(item, &arg(args, 0)))
                .map_or(-1.0, |i| i as f64),
        )),
        "slice" => {
            let start = relative_index(num_arg(args, 0), len);
            let end = match args.get(1) {
                None | Some(JsValue::Undefined) => len as isize,
                Some(v) => relative_index(js_to_number(v), len),
            };
            if start >= end {
                return Ok(JsValue::array(Vec::new()));
            }
            Ok(JsValue::array(items[start as usize..end as usize].to_vec()))
        }
        "concat" => {
            let mut out = items.as_ref().clone();
            for value in args {
                match value {
                    JsValue::Arr(more) => out.extend(more.iter().cloned()),
                    other => out.push(other.clone()),
                }
            }
            Ok(JsValue::array(out))
        }
        "flat" => {
            let depth = match args.first() {
                None | Some(JsValue::Undefined) => 1.0,
                Some(v) => to_integer_or_infinity(js_to_number(v)),
            };
            let mut out = Vec::new();
            flatten_into(&mut out, items, depth);
            Ok(JsValue::array(out))
        }
        "reverse" | "toReversed" => Ok(JsValue::array(
            items.iter().rev().cloned().collect::<Vec<_>>(),
        )),
        "sort" | "toSorted" => {
            let comparator = arg(args, 0);
            let mut sorted: Vec<JsValue> = items.as_ref().clone();
            // ES SortCompare: undefined elements sort last, before holes.
            let mut error: Option<StylexError> = None;
            if matches!(comparator, JsValue::Undefined) {
                let mut keyed: Vec<(Option<String>, JsValue)> = Vec::with_capacity(sorted.len());
                for item in sorted {
                    let key = if matches!(item, JsValue::Undefined) {
                        None
                    } else {
                        match js_to_string(&item) {
                            Some(k) => Some(k),
                            None => {
                                return Err(unsupported_global(
                                    "a function element in Array.prototype.sort()",
                                ));
                            }
                        }
                    };
                    keyed.push((key, item));
                }
                keyed.sort_by(|(a, _), (b, _)| match (a, b) {
                    (None, None) => std::cmp::Ordering::Equal,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (Some(a), Some(b)) => utf16_cmp(a, b),
                });
                sorted = keyed.into_iter().map(|(_, v)| v).collect();
            } else {
                sorted.sort_by(|a, b| {
                    if error.is_some() {
                        return std::cmp::Ordering::Equal;
                    }
                    if matches!(a, JsValue::Undefined) || matches!(b, JsValue::Undefined) {
                        return match (
                            matches!(a, JsValue::Undefined),
                            matches!(b, JsValue::Undefined),
                        ) {
                            (true, true) => std::cmp::Ordering::Equal,
                            (true, false) => std::cmp::Ordering::Greater,
                            (false, true) => std::cmp::Ordering::Less,
                            _ => unreachable!(),
                        };
                    }
                    match call_callback(
                        ev,
                        &comparator,
                        vec![a.clone(), b.clone()],
                        "Array.prototype.sort",
                    ) {
                        Ok(v) => {
                            let n = js_to_number(&v);
                            if n < 0.0 {
                                std::cmp::Ordering::Less
                            } else if n > 0.0 {
                                std::cmp::Ordering::Greater
                            } else {
                                std::cmp::Ordering::Equal
                            }
                        }
                        Err(e) => {
                            error = Some(e);
                            std::cmp::Ordering::Equal
                        }
                    }
                });
                if let Some(e) = error {
                    return Err(e);
                }
            }
            Ok(JsValue::array(sorted))
        }
        "map" | "flatMap" => {
            let callback = arg(args, 0);
            let mut out = Vec::with_capacity(len);
            for (i, item) in items.iter().enumerate() {
                let mapped =
                    call_callback(ev, &callback, callback_args(item, i), "Array.prototype.map")?;
                if method == "flatMap" {
                    match mapped {
                        JsValue::Arr(inner) => out.extend(inner.iter().cloned()),
                        other => out.push(other),
                    }
                } else {
                    out.push(mapped);
                }
            }
            Ok(JsValue::array(out))
        }
        "filter" => {
            let callback = arg(args, 0);
            let mut out = Vec::new();
            for (i, item) in items.iter().enumerate() {
                if truthy(&call_callback(
                    ev,
                    &callback,
                    callback_args(item, i),
                    "Array.prototype.filter",
                )?) {
                    out.push(item.clone());
                }
            }
            Ok(JsValue::array(out))
        }
        "forEach" => {
            let callback = arg(args, 0);
            for (i, item) in items.iter().enumerate() {
                call_callback(
                    ev,
                    &callback,
                    callback_args(item, i),
                    "Array.prototype.forEach",
                )?;
            }
            Ok(JsValue::Undefined)
        }
        "some" | "every" => {
            let callback = arg(args, 0);
            for (i, item) in items.iter().enumerate() {
                let passed = truthy(&call_callback(
                    ev,
                    &callback,
                    callback_args(item, i),
                    "Array.prototype.some",
                )?);
                if method == "some" && passed {
                    return Ok(JsValue::Bool(true));
                }
                if method == "every" && !passed {
                    return Ok(JsValue::Bool(false));
                }
            }
            Ok(JsValue::Bool(method == "every"))
        }
        "find" | "findIndex" | "findLast" | "findLastIndex" => {
            let callback = arg(args, 0);
            let indices: Vec<usize> = if method.contains("Last") {
                (0..len).rev().collect()
            } else {
                (0..len).collect()
            };
            for i in indices {
                let item = &items[i];
                if truthy(&call_callback(
                    ev,
                    &callback,
                    callback_args(item, i),
                    "Array.prototype.find",
                )?) {
                    return Ok(if method.ends_with("Index") {
                        JsValue::Num(i as f64)
                    } else {
                        item.clone()
                    });
                }
            }
            Ok(if method.ends_with("Index") {
                JsValue::Num(-1.0)
            } else {
                JsValue::Undefined
            })
        }
        "reduce" | "reduceRight" => {
            let callback = arg(args, 0);
            let mut order: Vec<usize> = (0..len).collect();
            if method == "reduceRight" {
                order.reverse();
            }
            let mut iter = order.into_iter();
            let mut acc = match args.get(1) {
                Some(initial) => initial.clone(),
                None => match iter.next() {
                    Some(i) => items[i].clone(),
                    None => {
                        return Err(StylexError::new(
                            ErrorCode::NonStaticValue,
                            "Reduce of empty array with no initial value",
                        ));
                    }
                },
            };
            for i in iter {
                acc = call_callback(
                    ev,
                    &callback,
                    vec![
                        acc,
                        items[i].clone(),
                        JsValue::Num(i as f64),
                        receiver.clone(),
                    ],
                    "Array.prototype.reduce",
                )?;
            }
            Ok(acc)
        }
        _ => Err(unsupported_global(method)),
    }
}

// ES SameValueZero (Array.prototype.includes).
fn same_value_zero(a: &JsValue, b: &JsValue) -> bool {
    match (a, b) {
        (JsValue::Num(x), JsValue::Num(y)) => (x.is_nan() && y.is_nan()) || x == y,
        _ => crate::eval::js_strict_eq(a, b),
    }
}

fn flatten_into(out: &mut Vec<JsValue>, items: &[JsValue], depth: f64) {
    for item in items {
        match item {
            JsValue::Arr(inner) if depth >= 1.0 => flatten_into(out, inner, depth - 1.0),
            other => out.push(other.clone()),
        }
    }
}

fn call_number_method(
    n: f64,
    method: &'static str,
    args: &[JsValue],
) -> Result<JsValue, StylexError> {
    match method {
        "valueOf" => Ok(JsValue::Num(n)),
        "toString" => {
            let radix = match args.first() {
                None | Some(JsValue::Undefined) => 10.0,
                Some(v) => to_integer_or_infinity(js_to_number(v)),
            };
            if radix == 10.0 {
                return Ok(JsValue::Str(js_number_to_string(n)));
            }
            if !(2.0..=36.0).contains(&radix) {
                return Err(StylexError::new(
                    ErrorCode::NonStaticValue,
                    "toString() radix must be between 2 and 36",
                ));
            }
            if !n.is_finite() {
                return Ok(JsValue::Str(js_number_to_string(n)));
            }
            if n.fract() != 0.0 || n.abs() >= 9_007_199_254_740_992.0 {
                return Err(unsupported_global(
                    "Number.prototype.toString with a non-integral radix argument",
                ));
            }
            let radix = radix as u32;
            let negative = n < 0.0;
            let mut magnitude = n.abs() as u64;
            let mut digits = Vec::new();
            loop {
                let digit = (magnitude % u64::from(radix)) as u32;
                digits.push(char::from_digit(digit, radix).expect("digit < radix"));
                magnitude /= u64::from(radix);
                if magnitude == 0 {
                    break;
                }
            }
            if negative {
                digits.push('-');
            }
            Ok(JsValue::Str(digits.into_iter().rev().collect()))
        }
        "toFixed" => {
            let digits = to_integer_or_infinity(num_arg(args, 0));
            if !(0.0..=100.0).contains(&digits) {
                return Err(StylexError::new(
                    ErrorCode::NonStaticValue,
                    "toFixed() digits argument must be between 0 and 100",
                ));
            }
            Ok(JsValue::Str(js_to_fixed(n, digits as usize)))
        }
        _ => Err(unsupported_global(method)),
    }
}

fn call_object_method(
    receiver: &JsValue,
    obj: &Rc<JsObj>,
    method: &'static str,
    args: &[JsValue],
) -> Result<JsValue, StylexError> {
    match method {
        "toString" | "toLocaleString" => Ok(JsValue::Str("[object Object]".to_string())),
        "valueOf" => Ok(receiver.clone()),
        "hasOwnProperty" | "propertyIsEnumerable" => {
            let key = js_to_string(&arg(args, 0)).ok_or_else(|| {
                unsupported_global("a function key in Object.prototype.hasOwnProperty()")
            })?;
            Ok(JsValue::Bool(obj.get(&key).is_some()))
        }
        "isPrototypeOf" => Ok(JsValue::Bool(false)),
        _ => Err(unsupported_global(method)),
    }
}

/// The global identifier-callee surface (`String(x)`, `Array(…)`, …).
pub(crate) fn call_global_identifier(
    name: &str,
    args: &[JsValue],
) -> Result<Option<JsValue>, StylexError> {
    match name {
        // parity: global.Math is not callable — the TypeError escapes.
        "Math" => Err(func_apply_error()),
        "String" => Ok(match args.first() {
            None => Some(JsValue::Str(String::new())),
            Some(v) => js_to_string(v).map(JsValue::Str),
        }),
        "Number" => Ok(Some(JsValue::Num(args.first().map_or(0.0, js_to_number)))),
        "Array" => {
            if args.len() == 1
                && let JsValue::Num(n) = args[0]
            {
                if n.fract() != 0.0 || !(0.0..=4_294_967_295.0).contains(&n) {
                    return Err(StylexError::new(
                        ErrorCode::NonStaticValue,
                        "Invalid array length",
                    ));
                }
                if n > 65535.0 {
                    return Err(unsupported_global("Array() beyond 65535 elements"));
                }
                // Holes approximated as undefined (visible only via own-keys).
                return Ok(Some(JsValue::array(vec![JsValue::Undefined; n as usize])));
            }
            Ok(Some(JsValue::array(args.to_vec())))
        }
        "Object" => match args.first() {
            None | Some(JsValue::Null | JsValue::Undefined) => {
                Ok(Some(JsValue::object(JsObj::default())))
            }
            Some(v @ (JsValue::Obj(_) | JsValue::Arr(_) | JsValue::Proxy(_))) => {
                Ok(Some(v.clone()))
            }
            Some(_) => Err(unsupported_global("Object() wrapper objects")),
        },
        _ => Ok(None),
    }
}
