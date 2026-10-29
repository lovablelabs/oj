// Decoding of the pinned request parameter shapes (tsc/internal/api/proto.go `*Params` structs).
//
// Go decodes params with encoding/json v2 into typed structs: a missing field (or `null`) leaves the
// zero value, a field of the wrong JSON type is an `ErrInvalidRequest`. The helpers below follow the
// same rules and keep every number bounded (no silent truncation of ids or positions).

use tsrs_core::json::{JsonMap, Value};
use tsrs_lsproto::DocumentUri;

use super::host::{CheckerError, CheckerResult};

static EMPTY_OBJECT: std::sync::LazyLock<JsonMap> = std::sync::LazyLock::new(Default::default);

pub fn object<'a>(params: &'a Value, method: &str) -> CheckerResult<&'a JsonMap> {
    match params {
        Value::Object(map) => Ok(map),
        // encoding/json v2 decodes `null` into a struct as its zero value.
        Value::Null => Ok(&EMPTY_OBJECT),
        _ => Err(CheckerError::invalid(format!("{method}: params must be an object"))),
    }
}

fn get<'a>(obj: &'a JsonMap, name: &str) -> Option<&'a Value> {
    match obj.get(name) {
        None | Some(Value::Null) => None,
        Some(v) => Some(v),
    }
}

fn wrong_type(name: &str, expected: &str) -> CheckerError {
    CheckerError::invalid(format!("field {name:?}: expected {expected}"))
}

fn unsigned(v: &Value, name: &str, max: f64) -> CheckerResult<u64> {
    match v {
        Value::Number(n) if n.fract() == 0.0 && *n >= 0.0 && *n <= max => Ok(*n as u64),
        _ => Err(wrong_type(name, "an unsigned integer in range")),
    }
}

fn json_kind(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) | Value::Integer(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// A Go `uint64` (`SnapshotID`, `SymbolID`, `SignatureID`) at JSON pointer `pointer` of the current
/// request. The parsed value is an f64, which rounds above 2^53 (9007199254740993 would become another
/// handle). `object` is the object holding the field, as it sits in the params tree core dispatched; core's
/// pre-decode records every literal of that tree and `predecode::exact_u64(object, key)` returns the field's
/// own exact value, at any depth (symbol references, import-adder actions). Range and integer-syntax errors
/// are reported by core's pre-decode before dispatch, nested api structs included; this only rejects what
/// cannot be a uint64 at all. A value outside the dispatched tree falls back to the parsed f64.
/// `pointer` builds the field's JSON pointer, for errors only.
fn unsigned64(
    v: &Value,
    object: &Value,
    key: &str,
    pointer: impl FnOnce() -> String,
    go_type: &str,
) -> CheckerResult<u64> {
    match v {
        Value::Number(n) if n.fract() == 0.0 && *n >= 0.0 && *n <= u64::MAX as f64 => {
            Ok(crate::predecode::exact_u64(object, key).unwrap_or(*n as u64))
        }
        Value::Number(_) => Err(CheckerError::invalid(format!(
            "cannot unmarshal JSON number into Go {go_type} within \"{}\"",
            pointer()
        ))),
        other => Err(CheckerError::invalid(format!(
            "cannot unmarshal JSON {} into Go {go_type} within \"{}\"",
            json_kind(other),
            pointer()
        ))),
    }
}

fn signed32(v: &Value, name: &str) -> CheckerResult<i32> {
    match v {
        Value::Number(n) if n.fract() == 0.0 && *n >= i32::MIN as f64 && *n <= i32::MAX as f64 => {
            Ok(*n as i32)
        }
        _ => Err(wrong_type(name, "a 32-bit integer")),
    }
}

/// A typed view over one request's params object.
pub struct Params<'a> {
    obj: &'a JsonMap,
    /// JSON pointer of this object within the request params ("" at the top level).
    pointer: String,
    /// This object's value within the params tree dispatched by core (its identity selects exact literals).
    value: &'a Value,
}

impl<'a> Params<'a> {
    pub(crate) fn new(params: &'a Value, method: &str) -> CheckerResult<Params<'a>> {
        Ok(Params { obj: object(params, method)?, pointer: String::new(), value: params })
    }

    fn nested(v: &'a Value, pointer: String) -> CheckerResult<Params<'a>> {
        Ok(Params { obj: object(v, &pointer)?, pointer, value: v })
    }

    pub(crate) fn raw(&self, name: &str) -> Option<&'a Value> {
        get(self.obj, name)
    }

    /// Go `uint64` ids (snapshot, symbol, signature handles), exact over the full uint64 range. Missing → 0.
    pub(crate) fn u64(&self, name: &str) -> CheckerResult<u64> {
        self.u64_typed(name, "uint64")
    }

    fn u64_typed(&self, name: &str, go_type: &str) -> CheckerResult<u64> {
        self.raw(name).map_or(Ok(0), |v| {
            unsigned64(v, self.value, name, || format!("{}/{name}", self.pointer), go_type)
        })
    }

    /// Go `uint32` (type ids, positions, symbol flags). Missing → 0.
    pub(crate) fn u32(&self, name: &str) -> CheckerResult<u32> {
        self.raw(name).map_or(Ok(0), |v| unsigned(v, name, u32::MAX as f64).map(|n| n as u32))
    }

    pub(crate) fn opt_u32(&self, name: &str) -> CheckerResult<Option<u32>> {
        self.raw(name).map(|v| unsigned(v, name, u32::MAX as f64).map(|n| n as u32)).transpose()
    }

    /// Go `int32` (kind, index, flags). Missing → 0.
    pub(crate) fn i32(&self, name: &str) -> CheckerResult<i32> {
        self.raw(name).map_or(Ok(0), |v| signed32(v, name))
    }

    pub(crate) fn bool(&self, name: &str) -> CheckerResult<bool> {
        match self.raw(name) {
            None => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(wrong_type(name, "a boolean")),
        }
    }

    pub(crate) fn string(&self, name: &str) -> CheckerResult<&'a str> {
        match self.raw(name) {
            None => Ok(""),
            Some(Value::String(s)) => Ok(s),
            Some(_) => Err(wrong_type(name, "a string")),
        }
    }

    pub(crate) fn array(&self, name: &str) -> CheckerResult<&'a [Value]> {
        match self.raw(name) {
            None => Ok(&[]),
            Some(Value::Array(items)) => Ok(items),
            Some(_) => Err(wrong_type(name, "an array")),
        }
    }

    pub(crate) fn u32_array(&self, name: &str) -> CheckerResult<Vec<u32>> {
        self.array(name)?
            .iter()
            .map(|v| unsigned(v, name, u32::MAX as f64).map(|n| n as u32))
            .collect()
    }

    pub(crate) fn string_array(&self, name: &str) -> CheckerResult<Vec<&'a str>> {
        self.array(name)?
            .iter()
            .map(|v| match v {
                Value::String(s) => Ok(s.as_str()),
                _ => Err(wrong_type(name, "an array of strings")),
            })
            .collect()
    }

    /// Go `project.ID` (a string; missing → "").
    pub(crate) fn project(&self) -> CheckerResult<&'a str> {
        self.string("project")
    }

    /// Go `DocumentIdentifier` field: a missing field is the zero value (empty file name), but an
    /// explicit `null` reaches the custom decoder and is an invalid request, like any non-string,
    /// non-object value.
    pub(crate) fn document(&self, name: &str) -> CheckerResult<DocumentIdentifier> {
        match self.obj.get(name) {
            None => Ok(DocumentIdentifier::default()),
            Some(v) => DocumentIdentifier::decode(v, &format!("/{name}")),
        }
    }

    /// Go `*DocumentIdentifier`: missing and `null` are both nil.
    pub(crate) fn opt_document(&self, name: &str) -> CheckerResult<Option<DocumentIdentifier>> {
        self.raw(name).map(|v| DocumentIdentifier::decode(v, &format!("/{name}"))).transpose()
    }

    /// Go `[]DocumentIdentifier`: `null` is a nil slice; a `null` element is an invalid request.
    pub(crate) fn documents(&self, name: &str) -> CheckerResult<Vec<DocumentIdentifier>> {
        self.array(name)?
            .iter()
            .enumerate()
            .map(|(i, v)| DocumentIdentifier::decode(v, &format!("/{name}/{i}")))
            .collect()
    }

    pub(crate) fn symbol_ref(&self, name: &str) -> CheckerResult<SymbolReference<'a>> {
        match self.raw(name) {
            None => Ok(SymbolReference::default()),
            Some(v) => SymbolReference::decode(v, format!("{}/{name}", self.pointer)),
        }
    }

    /// Go `[]SymbolReference`: a `null` element is the zero reference (Go structs decode `null` as zero).
    pub(crate) fn symbol_refs(&self, name: &str) -> CheckerResult<Vec<SymbolReference<'a>>> {
        self.array(name)?
            .iter()
            .enumerate()
            .map(|(i, v)| {
                if matches!(v, Value::Null) {
                    Ok(SymbolReference::default())
                } else {
                    SymbolReference::decode(v, format!("{}/{name}/{i}", self.pointer))
                }
            })
            .collect()
    }
}

/// Go `DocumentIdentifier`: `string | { uri: string }`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DocumentIdentifier {
    file_name: String,
    uri: String,
}

impl DocumentIdentifier {
    /// Go `DocumentIdentifier.UnmarshalJSONFrom`. `pointer` is the JSON pointer of the value; the error
    /// text after core's `failed to unmarshal *api.<T>: json: ` prefix is Go's.
    fn decode(v: &Value, pointer: &str) -> CheckerResult<DocumentIdentifier> {
        let got = match v {
            Value::String(s) => {
                if s.is_empty() {
                    return Err(CheckerError::invalid(format!(
                        "cannot unmarshal into Go api.DocumentIdentifier within \"{pointer}\": DocumentIdentifier: file name must not be empty"
                    )));
                }
                return Ok(DocumentIdentifier { file_name: s.clone(), uri: String::new() });
            }
            Value::Object(map) => {
                return match map.get("uri") {
                    None => Err(CheckerError::invalid(format!(
                        "cannot unmarshal into Go api.DocumentIdentifier within \"{pointer}\": DocumentIdentifier: object must contain uri"
                    ))),
                    Some(Value::String(uri)) if !uri.is_empty() => {
                        Ok(DocumentIdentifier { file_name: String::new(), uri: uri.clone() })
                    }
                    Some(_) => Err(CheckerError::invalid(format!(
                        "cannot unmarshal into Go api.DocumentIdentifier within \"{pointer}\": DocumentIdentifier: uri must be a non-empty string"
                    ))),
                };
            }
            Value::Null => "null",
            Value::Number(_) | Value::Integer(_) => "number",
            Value::Bool(true) => "true",
            Value::Bool(false) => "false",
            Value::Array(_) => "[",
        };
        Err(CheckerError::invalid(format!(
            "cannot unmarshal into Go api.DocumentIdentifier within \"{pointer}\": DocumentIdentifier: expected string or object, got {got}"
        )))
    }

    /// Go `DocumentIdentifier.ToFileName`.
    pub(crate) fn to_file_name(&self) -> String {
        if !self.uri.is_empty() {
            return DocumentUri(self.uri.clone()).file_name();
        }
        self.file_name.clone()
    }

    /// Go `DocumentIdentifier.String` (used in error messages).
    pub(crate) fn display(&self) -> &str {
        if self.uri.is_empty() { &self.file_name } else { &self.uri }
    }
}

/// Go `SymbolOwnerKind`.
pub const SYMBOL_OWNER_KIND_FILE: u32 = 0;
pub const SYMBOL_OWNER_KIND_SNAPSHOT: u32 = 1;

/// Go `SymbolReference` (`SymbolOwner` embedded + `id`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SymbolReference<'a> {
    pub(crate) kind: u32,
    pub(crate) file: Option<&'a Value>,
    pub(crate) snapshot: u64,
    pub(crate) project: &'a str,
    pub(crate) id: u64,
}

impl<'a> SymbolReference<'a> {
    /// `pointer`: JSON pointer of the reference in the request (`/symbol`, `/symbols/0`), used for exact
    /// integer literals and Go-style error locations.
    fn decode(v: &'a Value, pointer: String) -> CheckerResult<SymbolReference<'a>> {
        let Value::Object(_) = v else {
            return Err(CheckerError::invalid(format!(
                "cannot unmarshal JSON {} into Go api.SymbolReference within \"{pointer}\"",
                json_kind(v)
            )));
        };
        let p = Params::nested(v, pointer)?;
        let file = match p.raw("file") {
            None => None,
            Some(f @ Value::Object(_)) => Some(f),
            Some(_) => return Err(wrong_type("file", "a source file descriptor object")),
        };
        Ok(SymbolReference {
            kind: p.u32("kind")?,
            file,
            snapshot: p.u64_typed("snapshot", "api.SnapshotID")?,
            project: p.string("project")?,
            id: p.u64_typed("id", "api.SymbolID")?,
        })
    }

    /// The `path` of a file reference's descriptor.
    pub(crate) fn file_path(&self) -> Option<&'a str> {
        match self.file? {
            Value::Object(map) => match map.get("path") {
                Some(Value::String(s)) => Some(s),
                _ => None,
            },
            _ => None,
        }
    }
}
