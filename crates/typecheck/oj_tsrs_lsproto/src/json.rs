// Go's lsproto types go through encoding/json/v2 (reflection, plus the generated MarshalJSONTo /
// UnmarshalJSONFrom methods and structcodec.go). The port has no reflection and no serde: every
// protocol type implements `Json`, converting to and from `tsrs_core::json::Value` (Go-compatible
// encoding, object keys in order). The impls here reproduce json/v2's default codecs for the
// primitive and container types the generated code uses, including its error texts
// (`SemanticError`), so that the same JSON in gives the same JSON (or error) out.

use std::fmt;
use std::hash::Hash;

use tsrs_core::collections::OrderedMap;
use tsrs_core::json::JsonMap;
pub use tsrs_core::json::Value;

use crate::ErrorCode;

// A decoding error. Boxed so that every `Result<T, JsonError>` the decoders pass around stays small; the
// fields are reached through `Deref`.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct JsonError(Box<SemanticError>);

// json/v2's `SemanticError` (plus plain syntactic errors). `pointer` holds the JSON pointer tokens
// innermost first; `within` appends as the error propagates outwards.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct SemanticError {
    pub(crate) go_type: String,
    json_kind: u8,
    json_value: String,
    pub(crate) pointer: Vec<String>,
    err: String,
    // ErrorCodes wrapped (Go `%w`) by the underlying error, outermost first.
    pub(crate) codes: Vec<ErrorCode>,
    plain: bool,
    // The pointer is relative to a value Go decoded with a fresh `json.Unmarshal` (a buffered union arm,
    // a replayed field); json/v2 keeps such a pointer as is instead of prefixing the outer position.
    pub(crate) sealed: bool,
}

impl std::ops::Deref for JsonError {
    type Target = SemanticError;
    fn deref(&self) -> &SemanticError {
        &self.0
    }
}

impl std::ops::DerefMut for JsonError {
    fn deref_mut(&mut self) -> &mut SemanticError {
        &mut self.0
    }
}

impl From<SemanticError> for JsonError {
    fn from(err: SemanticError) -> JsonError {
        JsonError(Box::new(err))
    }
}

impl JsonError {
    // An error returned by a type's own unmarshal method (Go `UnmarshalJSONFrom`).
    pub(crate) fn method(go_type: &str, err: impl Into<String>) -> JsonError {
        SemanticError { go_type: go_type.to_string(), err: err.into(), ..Default::default() }.into()
    }

    // An error returned by a v1-style `UnmarshalJSON` method; json/v2 reports the JSON kind too.
    pub(crate) fn method_v1(kind: u8, go_type: &str, err: impl Into<String>) -> JsonError {
        SemanticError {
            go_type: go_type.to_string(),
            json_kind: kind,
            err: err.into(),
            ..Default::default()
        }
        .into()
    }

    // json/v2: a JSON value of the wrong kind for the Go type.
    pub(crate) fn mismatch(v: &Value, go_type: &str) -> JsonError {
        SemanticError { go_type: go_type.to_string(), json_kind: kind(v), ..Default::default() }
            .into()
    }

    // json/v2: a JSON number that does not fit the Go number type.
    fn number(n: f64, go_type: &str, err: &str) -> JsonError {
        SemanticError {
            go_type: go_type.to_string(),
            json_kind: b'0',
            json_value: tsrs_core::json::marshal_f64(n).unwrap_or_default(),
            err: err.to_string(),
            ..Default::default()
        }
        .into()
    }

    pub(crate) fn plain(err: impl Into<String>) -> JsonError {
        SemanticError { err: err.into(), plain: true, ..Default::default() }.into()
    }

    #[must_use]
    pub(crate) fn within(mut self, token: &str) -> JsonError {
        if !self.plain && !self.sealed {
            self.pointer.push(token.to_string());
        }
        self
    }

    // The error of a fresh `json.Unmarshal` returned from an unmarshal method: json/v2 fills in the
    // current position only if the error has none.
    #[must_use]
    pub(crate) fn seal(mut self) -> JsonError {
        if !self.pointer.is_empty() {
            self.sealed = true;
        }
        self
    }

    #[must_use]
    fn within_index(self, index: usize) -> JsonError {
        self.within(&index.to_string())
    }

    fn json_pointer(&self) -> String {
        let mut s = String::new();
        for token in self.pointer.iter().rev() {
            s.push('/');
            s.push_str(&token.replace('~', "~0").replace('/', "~1"));
        }
        s
    }
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.plain {
            return f.write_str(&self.err);
        }
        let mut s = String::from("json: cannot unmarshal");
        match self.json_kind {
            b'n' => s.push_str(" JSON null"),
            b'f' | b't' => s.push_str(" JSON boolean"),
            b'"' => s.push_str(" JSON string"),
            b'0' => s.push_str(" JSON number"),
            b'{' | b'}' => s.push_str(" JSON object"),
            b'[' | b']' => s.push_str(" JSON array"),
            _ => {}
        }
        if !self.json_value.is_empty() && self.json_value.len() < 100 {
            s.push(' ');
            s.push_str(&self.json_value);
        }
        if !self.go_type.is_empty() {
            s.push_str(" into Go ");
            if self.go_type.len() > 100 {
                // json/v2 prints only the kind of an excessively long type.
                s.push_str(if self.go_type.starts_with("[]") {
                    "slice"
                } else if self.go_type.starts_with("map[") {
                    "map"
                } else {
                    "struct"
                });
            } else {
                s.push_str(&self.go_type);
            }
        }
        if !self.pointer.is_empty() {
            s.push_str(" within ");
            s.push_str(&tsrs_core::json::marshal_string(&self.json_pointer()));
        }
        if !self.err.is_empty() {
            s.push_str(": ");
            s.push_str(&self.err);
        }
        f.write_str(&s)
    }
}

impl std::error::Error for JsonError {}

pub(crate) fn is_null(v: &Value) -> bool {
    matches!(v, Value::Null)
}

// jsontext.Kind of a value: 'n', 't', 'f', '"', '0', '{', '['.
pub(crate) fn kind(v: &Value) -> u8 {
    match v {
        Value::Null => b'n',
        Value::Bool(true) => b't',
        Value::Bool(false) => b'f',
        Value::Number(_) | Value::Integer(_) => b'0',
        Value::String(_) => b'"',
        Value::Array(_) => b'[',
        Value::Object(_) => b'{',
    }
}

// jsontext.Kind.String().
pub(crate) fn kind_string(k: u8) -> &'static str {
    match k {
        b'n' => "null",
        b'f' => "false",
        b't' => "true",
        b'"' => "string",
        b'0' => "number",
        b'{' => "{",
        b'}' => "}",
        b'[' => "[",
        b']' => "]",
        _ => "<invalid jsontext.Kind>",
    }
}

pub trait Json: Sized {
    // The Go type name json/v2 reports in errors ("lsproto.Location", "uint32", ...).
    const GO_TYPE: &'static str;
    // Go `any(params).(NoParams)` in `UnmarshalParams`.
    const IS_NO_PARAMS: bool = false;
    // Go holds the type behind a pointer in slices and maps (`[]*T`); only used in error texts.
    const GO_POINTER: bool = false;

    // The Go type name of a composite type ("[]*lsproto.TextEdit"); only used in error texts.
    fn go_type_name() -> String {
        Self::GO_TYPE.to_string()
    }

    fn to_json(&self) -> Value;
    fn from_json(v: &Value) -> Result<Self, JsonError>;
}

// reflect.Value.IsZero for the value types that carry `omitzero` without a pointer.
pub trait IsZero {
    fn is_zero(&self) -> bool;
}

// Go `json.Marshal(v)`.
pub fn marshal<T: Json>(v: &T) -> Result<String, String> {
    tsrs_core::json::marshal(&v.to_json())
}

// Go `json.Unmarshal(data, &v)`.
pub fn unmarshal<T: Json>(data: &[u8]) -> Result<T, JsonError> {
    let text = tsrs_core::utf8::basic::from_utf8(data)
        .map_err(|_| JsonError::plain("jsontext: invalid UTF-8"))?;
    let v = tsrs_core::json::unmarshal(text).map_err(JsonError::plain)?;
    T::from_json(&v)
}

impl Json for Value {
    const GO_TYPE: &'static str = "interface {}";

    fn to_json(&self) -> Value {
        self.clone()
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        Ok(v.clone())
    }
}

impl Json for bool {
    const GO_TYPE: &'static str = "bool";

    fn to_json(&self) -> Value {
        Value::Bool(*self)
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        match v {
            Value::Bool(b) => Ok(*b),
            Value::Null => Ok(false),
            _ => Err(JsonError::mismatch(v, Self::GO_TYPE)),
        }
    }
}

impl IsZero for bool {
    fn is_zero(&self) -> bool {
        !*self
    }
}

impl Json for String {
    const GO_TYPE: &'static str = "string";

    fn to_json(&self) -> Value {
        Value::String(self.clone())
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        decode_string(v, Self::GO_TYPE).map(str::to_string)
    }
}

impl IsZero for String {
    fn is_zero(&self) -> bool {
        self.is_empty()
    }
}

// json/v2 string decoding for a Go string kind named `go_type`; null decodes as "".
pub(crate) fn decode_string<'a>(v: &'a Value, go_type: &str) -> Result<&'a str, JsonError> {
    match v {
        Value::String(s) => Ok(s),
        Value::Null => Ok(""),
        _ => Err(JsonError::mismatch(v, go_type)),
    }
}

// json/v2 signed integer decoding (`bits` wide) for a Go int kind named `go_type`; null decodes as 0.
pub(crate) fn decode_int(v: &Value, bits: u32, go_type: &str) -> Result<i64, JsonError> {
    match v {
        Value::Number(n) => {
            let n = *n;
            if n.fract() != 0.0 {
                return Err(JsonError::number(n, go_type, "invalid syntax"));
            }
            let max = (1u64 << (bits - 1)) as f64;
            if n >= max || n < -max {
                return Err(JsonError::number(n, go_type, "value out of range"));
            }
            Ok(n as i64)
        }
        Value::Null => Ok(0),
        _ => Err(JsonError::mismatch(v, go_type)),
    }
}

// json/v2 unsigned integer decoding (`bits` wide) for a Go uint kind named `go_type`; null decodes as 0.
pub(crate) fn decode_uint(v: &Value, bits: u32, go_type: &str) -> Result<u64, JsonError> {
    match v {
        Value::Number(n) => {
            let n = *n;
            if n.fract() != 0.0 || n.is_sign_negative() {
                return Err(JsonError::number(n, go_type, "invalid syntax"));
            }
            if n >= 2f64.powi(bits as i32) {
                return Err(JsonError::number(n, go_type, "value out of range"));
            }
            Ok(n as u64)
        }
        Value::Null => Ok(0),
        _ => Err(JsonError::mismatch(v, go_type)),
    }
}

impl Json for i32 {
    const GO_TYPE: &'static str = "int32";

    fn to_json(&self) -> Value {
        Value::Number(*self as f64)
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        decode_int(v, 32, Self::GO_TYPE).map(|n| n as i32)
    }
}

impl IsZero for i32 {
    fn is_zero(&self) -> bool {
        *self == 0
    }
}

impl Json for u32 {
    const GO_TYPE: &'static str = "uint32";

    fn to_json(&self) -> Value {
        Value::Number(*self as f64)
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        decode_uint(v, 32, Self::GO_TYPE).map(|n| n as u32)
    }
}

impl IsZero for u32 {
    fn is_zero(&self) -> bool {
        *self == 0
    }
}

impl Json for u64 {
    const GO_TYPE: &'static str = "uint64";

    fn to_json(&self) -> Value {
        Value::Number(*self as f64)
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        decode_uint(v, 64, Self::GO_TYPE)
    }
}

impl IsZero for u64 {
    fn is_zero(&self) -> bool {
        *self == 0
    }
}

impl Json for f64 {
    const GO_TYPE: &'static str = "float64";

    fn to_json(&self) -> Value {
        Value::Number(*self)
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        match v {
            Value::Number(n) => Ok(*n),
            Value::Null => Ok(0.0),
            _ => Err(JsonError::mismatch(v, Self::GO_TYPE)),
        }
    }
}

impl IsZero for f64 {
    fn is_zero(&self) -> bool {
        self.to_bits() == 0
    }
}

// Go pointer: null decodes as nil, nil marshals as null.
impl<T: Json> Json for Option<T> {
    const GO_TYPE: &'static str = T::GO_TYPE;
    const GO_POINTER: bool = true;

    fn go_type_name() -> String {
        T::go_type_name()
    }

    fn to_json(&self) -> Value {
        match self {
            Some(v) => v.to_json(),
            None => Value::Null,
        }
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        match v {
            Value::Null => Ok(None),
            _ => T::from_json(v).map(Some),
        }
    }
}

impl<T: Json> Json for Box<T> {
    const GO_TYPE: &'static str = T::GO_TYPE;
    const GO_POINTER: bool = T::GO_POINTER;

    fn go_type_name() -> String {
        T::go_type_name()
    }

    fn to_json(&self) -> Value {
        (**self).to_json()
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        T::from_json(v).map(Box::new)
    }
}

// Go slice: json/v2 marshals a nil slice as `[]`; null decodes as an empty (nil) slice.
impl<T: Json> Json for Vec<T> {
    const GO_TYPE: &'static str = "slice";

    fn go_type_name() -> String {
        format!("[]{}{}", if T::GO_POINTER { "*" } else { "" }, T::go_type_name())
    }

    fn to_json(&self) -> Value {
        Value::Array(self.iter().map(T::to_json).collect())
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        match v {
            Value::Array(items) => {
                let mut out = Vec::with_capacity(items.len());
                out = items.iter().enumerate().try_fold(out, |mut out, (i, item)| {
                    out.push(T::from_json(item).map_err(|e| e.within_index(i))?);
                    Ok::<_, JsonError>(out)
                })?;
                Ok(out)
            }
            Value::Null => Ok(Vec::new()),
            _ => Err(JsonError::mismatch(v, &Self::go_type_name())),
        }
    }
}

// Go `[2]uint32`.
impl Json for [u32; 2] {
    const GO_TYPE: &'static str = "[2]uint32";

    fn to_json(&self) -> Value {
        Value::Array(vec![self[0].to_json(), self[1].to_json()])
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        match v {
            Value::Array(items) => {
                let mut out = [0u32; 2];
                for (i, item) in items.iter().enumerate() {
                    if i >= 2 {
                        return Err(JsonError::method_v1(
                            b'[',
                            Self::GO_TYPE,
                            "too many array elements",
                        ));
                    }
                    out[i] = u32::from_json(item).map_err(|e| e.within_index(i))?;
                }
                if items.len() < 2 {
                    return Err(JsonError::method_v1(
                        b'[',
                        Self::GO_TYPE,
                        "too few array elements",
                    ));
                }
                Ok(out)
            }
            Value::Null => Ok([0, 0]),
            _ => Err(JsonError::mismatch(v, Self::GO_TYPE)),
        }
    }
}

// Map keys: Go string kinds.
pub trait JsonKey: Sized + Hash + Eq {
    const GO_KEY_TYPE: &'static str;
    fn key_string(&self) -> &str;
    fn from_key(s: &str) -> Self;
}

impl JsonKey for String {
    const GO_KEY_TYPE: &'static str = "string";

    fn key_string(&self) -> &str {
        self
    }

    fn from_key(s: &str) -> Self {
        s.to_string()
    }
}

// Go map. json/v2 marshals a nil map as `{}` and decodes null as a nil map. Go does not order map
// entries; an OrderedMap keeps insertion order (and decodes in document order).
impl<K: JsonKey, V: Json> Json for OrderedMap<K, V> {
    const GO_TYPE: &'static str = "map";

    fn go_type_name() -> String {
        format!(
            "map[{}]{}{}",
            K::GO_KEY_TYPE,
            if V::GO_POINTER { "*" } else { "" },
            V::go_type_name()
        )
    }

    fn to_json(&self) -> Value {
        let mut m = JsonMap::new();
        m.reserve(self.len());
        m.extend(self.into_iter().map(|(k, v)| (k.key_string().to_string(), v.to_json())));
        Value::Object(m)
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        match v {
            Value::Object(members) => {
                let mut out = OrderedMap::default();
                out.reserve(members.len());
                out = members.into_iter().try_fold(out, |mut out, (k, item)| {
                    out.insert(K::from_key(k), V::from_json(item).map_err(|e| e.within(k))?);
                    Ok::<_, JsonError>(out)
                })?;
                Ok(out)
            }
            Value::Null => Ok(OrderedMap::default()),
            _ => Err(JsonError::mismatch(v, &Self::go_type_name())),
        }
    }
}

// Builds a JSON object in Go struct field order.
pub struct ObjectWriter {
    m: JsonMap,
}

impl ObjectWriter {
    pub fn new(capacity: usize) -> ObjectWriter {
        ObjectWriter { m: JsonMap::with_capacity(capacity) }
    }

    // A field without omitzero.
    pub fn field<T: Json>(&mut self, name: &str, v: &T) {
        self.m.insert(name.to_string(), v.to_json());
    }

    // A pointer/slice/map/interface field with omitzero: omitted when nil.
    pub fn opt<T: Json>(&mut self, name: &str, v: Option<&T>) {
        if let Some(v) = v {
            self.m.insert(name.to_string(), v.to_json());
        }
    }

    // A value field with omitzero: omitted when it is the zero value.
    pub(crate) fn value<T: Json + IsZero>(&mut self, name: &str, v: &T) {
        if !v.is_zero() {
            self.m.insert(name.to_string(), v.to_json());
        }
    }

    pub(crate) fn raw(&mut self, name: &str, v: Value) {
        self.m.insert(name.to_string(), v);
    }

    pub fn finish(self) -> Value {
        Value::Object(self.m)
    }
}
