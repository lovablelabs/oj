// Go decodes a request's whole params struct (proto.go `unmarshalPayload`) before a handler runs, so a field of
// the wrong JSON kind is an invalid request even when another field would fail a lookup first. This checks the
// top-level fields of each method's pinned params struct (`paramfields.rs`, generated from proto.go) in Go's
// struct field order, plus array element kinds and integer ranges, for every method (core and checker lanes),
// before dispatch. Unknown keys are ignored, as Go does. Integer fields are checked against the raw number
// lexeme: encoding/json/v2 accepts only plain integer syntax (`1e3`, `1.0` are `invalid syntax`).
// Error class and `failed to unmarshal *api.<T>` prefix match Go; the jsontext wording after it is approximate
// (Go reports the first invalid field in document order, this in struct order) except for DocumentIdentifier,
// whose custom decoder text is reproduced.

use std::fmt::Write as _;
use std::sync::LazyLock;

use rustc_hash::{FxHashMap, FxHashSet};

use tsrs_core::json::{JsonMap, Value};

use crate::handler::{ApiError, ApiResult};
use crate::paramfields::{Elem, FieldKind, FieldSpec, params_fields, struct_fields};

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

fn go_type_name(t: &str) -> String {
    let base = t.trim_start_matches('*');
    if base.contains('.')
        || base.starts_with("[]")
        || !base.starts_with(|c: char| c.is_ascii_uppercase())
    {
        base.to_string()
    } else {
        format!("api.{base}")
    }
}

/// proto.go `DocumentIdentifier.UnmarshalJSONFrom` (error text as probed against the pinned server by the checker
/// lane, `checker/params.rs`).
fn check_document(v: &Value, pointer: &str) -> Result<(), String> {
    let got = match v {
        Value::String(s) => {
            return if s.is_empty() {
                Err(format!(
                    "cannot unmarshal into Go api.DocumentIdentifier within \"{pointer}\": DocumentIdentifier: file name must not be empty"
                ))
            } else {
                Ok(())
            };
        }
        Value::Object(o) => {
            return match o.get("uri") {
                Some(Value::String(uri)) if !uri.is_empty() => Ok(()),
                Some(_) => Err(format!(
                    "cannot unmarshal into Go api.DocumentIdentifier within \"{pointer}\": DocumentIdentifier: uri must be a non-empty string"
                )),
                None => Err(format!(
                    "cannot unmarshal into Go api.DocumentIdentifier within \"{pointer}\": DocumentIdentifier: object must contain uri"
                )),
            };
        }
        Value::Null => "null",
        Value::Number(_) | Value::Integer(_) => "number",
        Value::Bool(true) => "true",
        Value::Bool(false) => "false",
        Value::Array(_) => "[",
    };
    Err(format!(
        "cannot unmarshal into Go api.DocumentIdentifier within \"{pointer}\": DocumentIdentifier: expected string or object, got {got}"
    ))
}

#[derive(Clone, Copy)]
enum IntRange {
    I32,
    I64,
    U32,
    U64,
}

/// A JSON number decoded into a Go integer: plain integer syntax (no fraction or exponent) within range,
/// checked on the exact literal (f64 would round `i64::MAX` / `u64::MAX` up to 2^63 / 2^64).
fn int_ok(v: &Value, lexeme: Option<&str>, range: IntRange) -> bool {
    let Value::Number(n) = v else { return false };
    let exact: Option<i128> = match lexeme {
        Some(l) if l.contains(['.', 'e', 'E']) => return false,
        Some(l) => l.parse::<i128>().ok(),
        None if n.fract() == 0.0 && n.abs() < 1e30 => Some(*n as i128),
        None => None,
    };
    let Some(n) = exact else { return false };
    match range {
        IntRange::I32 => (i32::MIN as i128..=i32::MAX as i128).contains(&n),
        IntRange::I64 => (i64::MIN as i128..=i64::MAX as i128).contains(&n),
        IntRange::U32 => (0..=u32::MAX as i128).contains(&n),
        IntRange::U64 => (0..=u64::MAX as i128).contains(&n),
    }
}

fn check_scalar(kind: Elem, v: &Value, lexeme: Option<&str>) -> bool {
    match kind {
        Elem::Any => true,
        Elem::Str => matches!(v, Value::String(_)),
        Elem::Bool => matches!(v, Value::Bool(_)),
        Elem::Object => matches!(v, Value::Object(_)),
        Elem::I32 => int_ok(v, lexeme, IntRange::I32),
        Elem::I64 => int_ok(v, lexeme, IntRange::I64),
        Elem::U32 => int_ok(v, lexeme, IntRange::U32),
        Elem::U64 => int_ok(v, lexeme, IntRange::U64),
    }
}

fn field_elem(kind: FieldKind) -> Option<Elem> {
    Some(match kind {
        FieldKind::Str => Elem::Str,
        FieldKind::I32 => Elem::I32,
        FieldKind::I64 => Elem::I64,
        FieldKind::U32 => Elem::U32,
        FieldKind::U64 => Elem::U64,
        FieldKind::Bool => Elem::Bool,
        FieldKind::Object => Elem::Object,
        FieldKind::Any => Elem::Any,
        FieldKind::Doc | FieldKind::DocList | FieldKind::Array => return None,
    })
}

/// JSON pointer token for an object member name (RFC 6901 escaping).
fn token(key: &str) -> String {
    key.replace('~', "~0").replace('/', "~1")
}

/// The JSON pointer of the value being checked, for error texts and inexact-literal lookups. A pointer that
/// is off (`Pointer::off`) stays empty: `predecode` checks with it off first and, only if that fails,
/// checks again with it on for the error text (the outcome does not depend on the pointer when there are no
/// literals to look up).
struct Pointer {
    text: String,
    on: bool,
}

impl Pointer {
    fn new(on: bool) -> Pointer {
        Pointer { text: if on { String::with_capacity(64) } else { String::new() }, on }
    }

    fn len(&self) -> usize {
        self.text.len()
    }

    fn truncate(&mut self, len: usize) {
        self.text.truncate(len);
    }

    fn push_key(&mut self, key: &str) {
        if self.on {
            push_token(&mut self.text, key);
        }
    }

    fn push_index(&mut self, i: usize) {
        if self.on {
            let _ = write!(self.text, "/{i}");
        }
    }

    fn as_str(&self) -> &str {
        &self.text
    }
}

impl std::fmt::Display for Pointer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.text)
    }
}

/// Appends `/` and the pointer token of `key` to `pointer`.
fn push_token(pointer: &mut String, key: &str) {
    pointer.push('/');
    if !key.contains(['~', '/']) {
        pointer.push_str(key);
        return;
    }
    for c in key.chars() {
        match c {
            '~' => pointer.push_str("~0"),
            '/' => pointer.push_str("~1"),
            c => pointer.push(c),
        }
    }
}

/// proto.go / project.go `SyntheticProjectID.UnmarshalJSONFrom`: a string (null decodes as "") that parses as a
/// synthetic project ID.
fn check_synthetic_id(v: &Value, pointer: &str) -> Result<(), String> {
    let text = match v {
        Value::String(s) => s.as_str(),
        Value::Null => "",
        _ => {
            return Err(format!(
                "cannot unmarshal JSON {} into Go string within \"{pointer}\"",
                json_kind(v)
            ));
        }
    };
    match tsrs_project::parse_synthetic_project_id(text) {
        Some(_) => Ok(()),
        None => Err(format!(
            "cannot unmarshal into Go project.SyntheticProjectID within \"{pointer}\": invalid synthetic project ID: {text}"
        )),
    }
}

fn base_type(go_type: &str) -> &str {
    if !go_type.starts_with(['*', '[']) {
        return go_type;
    }
    go_type.trim_start_matches('*').trim_start_matches("[]").trim_start_matches('*')
}

/// `name` as one of the field names of the params structs and the api-package structs nested in them, for
/// the request decoder to borrow.
pub(crate) fn known_member_name(name: &str) -> Option<&'static str> {
    static NAMES: LazyLock<FxHashSet<&'static str>> = LazyLock::new(|| {
        let mut names = FxHashSet::default();
        let mut seen_types = FxHashSet::default();
        let mut pending: Vec<&'static [FieldSpec]> =
            crate::methods::PARAMS_TYPES.iter().map(|(method, _)| params_fields(method)).collect();
        while let Some(fields) = pending.pop() {
            for spec in fields {
                names.insert(spec.name);
                let go_type = base_type(spec.go_type);
                if seen_types.insert(go_type) {
                    pending.push(struct_fields(go_type));
                }
            }
        }
        names
    });
    NAMES.get(name).copied()
}

/// The fields of an api-package struct value at `pointer`, recursively (Go decodes the whole params struct).
fn check_struct(
    go_type: &str,
    o: &JsonMap,
    pointer: &mut Pointer,
    lexemes: &FxHashMap<String, String>,
) -> Result<(), String> {
    let mut next = 0;
    for spec in struct_fields(base_type(go_type)) {
        if let Some(v) = o.get_from(spec.name, &mut next) {
            let len = pointer.len();
            pointer.push_key(spec.name);
            check_field(spec, v, pointer, lexemes)?;
            pointer.truncate(len);
        }
    }
    Ok(())
}

/// Integer fields of structs from other Go packages, decoded by `gojson` from the parsed value: Go's decoder
/// needs plain integer syntax within the Go type's range there too (`"target":1e1`, `"builders":1e1`, a `*int`
/// above int64). Wrong JSON kinds and int32 ranges are reported by `gojson`.
fn check_external_integers(
    go_type: &str,
    o: &JsonMap,
    pointer: &str,
    lexemes: &FxHashMap<String, String>,
) -> Result<(), String> {
    let fields: Vec<(&str, u32)> = match base_type(go_type) {
        "core.CompilerOptions" => tsrs_tsoptions::gojson::compiler_options_integer_fields(),
        "core.BuildOptions" => tsrs_tsoptions::gojson::BUILD_OPTIONS_INTEGER_FIELDS.to_vec(),
        _ => return Ok(()),
    };
    if let Some(result) = fields.into_iter().find_map(|(name, bits)| {
        if !matches!(o.get(name), Some(Value::Number(_))) {
            return None;
        }
        let p = format!("{pointer}/{}", token(name));
        let go_int = if bits == 64 { "int" } else { "int32" };
        let lexeme = lexemes.get(&p)?;
        if lexeme.contains(['.', 'e', 'E']) {
            return Some(Err(format!("cannot unmarshal JSON number {lexeme} into Go {go_int} within \"{p}\": invalid syntax")));
        }
        if bits == 64 && lexeme.parse::<i64>().is_err() {
            return Some(Err(format!("cannot unmarshal JSON number {lexeme} into Go {go_int} within \"{p}\": value out of range")));
        }

        None
    }) {
        return result;
    }
    Ok(())
}

/// Replaces the `*int` options decoded from `value` (a core.CompilerOptions object of the current request) with
/// their exact literals (f64 cannot hold every Go int).
pub(crate) fn exact_compiler_options_ints(value: &Value, options: &mut tsrs_core::CompilerOptions) {
    if let Some(n) = exact_i64(value, "maxNodeModuleJsDepth") {
        options.max_node_module_js_depth = Some(n);
    }
    if let Some(n) = exact_i64(value, "checkers") {
        options.checkers = Some(n);
    }
}

/// As `exact_compiler_options_ints`, for core.BuildOptions.
pub(crate) fn exact_build_options_ints(value: &Value, options: &mut tsrs_core::BuildOptions) {
    if let Some(n) = exact_i64(value, "builders") {
        options.builders = Some(n);
    }
}

/// requestfilesystem.RequestFileSystem (decoded by `requestfs.rs` from the parsed value): Go decodes the whole
/// struct first, so a wrong JSON kind anywhere in it is an invalid request. `null` is the zero value (a null
/// file content is "", a null path element is ""), as in Go.
fn check_request_file_system(go_type: &str, o: &JsonMap, pointer: &str) -> Result<(), String> {
    if base_type(go_type) != "requestfilesystem.RequestFileSystem" {
        return Ok(());
    }
    let err = |v: &Value, p: &str, t: &str| {
        Err(format!("cannot unmarshal JSON {} into Go {t} within \"{p}\"", json_kind(v)))
    };
    let strings = |v: &Value, p: &str| -> Result<(), String> {
        match v {
            Value::Null => Ok(()),
            Value::Array(items) => items.iter().enumerate().try_for_each(|(i, item)| {
                if matches!(item, Value::Null | Value::String(_)) {
                    Ok(())
                } else {
                    err(item, &format!("{p}/{i}"), "string")
                }
            }),
            _ => err(v, p, "[]string"),
        }
    };
    // Each member's map, checked as `map[string]<elem>` with `check` per non-null value.
    let map = |name: &str,
               go: &str,
               check: &dyn Fn(&Value, &str) -> Result<(), String>|
     -> Result<(), String> {
        let p = format!("{pointer}/{name}");
        match o.get(name) {
            None | Some(Value::Null) => Ok(()),
            Some(Value::Object(m)) => {
                for (k, v) in m {
                    if !matches!(v, Value::Null) {
                        check(v, &format!("{p}/{}", token(k)))?;
                    }
                }
                Ok(())
            }
            Some(v) => err(v, &p, go),
        }
    };
    if let Some(v) = o.get("kind").filter(|v| !matches!(v, Value::Null | Value::String(_))) {
        return err(v, &format!("{pointer}/kind"), "requestfilesystem.Kind");
    }
    map("files", "map[string]string", &|v, p| {
        if matches!(v, Value::String(_)) { Ok(()) } else { err(v, p, "string") }
    })?;
    map("directories", "map[string]requestfilesystem.RequestDirectoryEntries", &|v, p| match v {
        Value::Object(d) => {
            for name in ["files", "directories"] {
                if let Some(x) = d.get(name) {
                    strings(x, &format!("{p}/{name}"))?;
                }
            }
            Ok(())
        }
        _ => err(v, p, "requestfilesystem.RequestDirectoryEntries"),
    })?;
    map("symlinks", "map[string]requestfilesystem.RequestSymlink", &|v, p| match v {
        Value::Object(s) => {
            if let Some(x) =
                s.get("target").filter(|x| !matches!(x, Value::Null | Value::String(_)))
            {
                return err(x, &format!("{p}/target"), "string");
            }
            if let Some(x) = s.get("host").filter(|x| !matches!(x, Value::Null | Value::Bool(_))) {
                return err(x, &format!("{p}/host"), "bool");
            }
            Ok(())
        }
        _ => err(v, p, "requestfilesystem.RequestSymlink"),
    })?;
    if let Some(v) = o.get("removedPaths") {
        strings(v, &format!("{pointer}/removedPaths"))?;
    }
    Ok(())
}

/// `pointer` locates `v`; it is a scratch buffer that nested checks extend and restore.
fn check_field(
    spec: &FieldSpec,
    v: &Value,
    pointer: &mut Pointer,
    lexemes: &FxHashMap<String, String>,
) -> Result<(), String> {
    let mismatch = |v: &Value, pointer: &str, go_type: &str| {
        format!(
            "cannot unmarshal JSON {} into Go {} within \"{pointer}\"",
            json_kind(v),
            go_type_name(go_type)
        )
    };
    // project.SyntheticProjectID has its own decoder, called for null too (unlike a plain string).
    if base_type(spec.go_type) == "project.SyntheticProjectID" {
        return match v {
            Value::Array(items) if spec.go_type.starts_with("[]") => {
                for (i, item) in items.iter().enumerate() {
                    check_synthetic_id(item, &format!("{pointer}/{i}"))?;
                }
                Ok(())
            }
            Value::Null if spec.go_type.starts_with("[]") => Ok(()),
            _ if spec.go_type.starts_with("[]") => Err(mismatch(v, pointer.as_str(), spec.go_type)),
            _ => check_synthetic_id(v, pointer.as_str()),
        };
    }
    if matches!(v, Value::Null) && spec.kind != FieldKind::Doc && spec.kind != FieldKind::DocList {
        // `null` into a non-pointer field leaves the zero value; into a pointer it is nil.
        return Ok(());
    }
    if let Some(elem) = field_elem(spec.kind) {
        check_scalar(elem, v, lexemes.get(pointer.as_str()).map(String::as_str))
            .ok_or_else(|| mismatch(v, pointer.as_str(), spec.go_type))?;
        if let Value::Object(o) = v {
            check_struct(spec.go_type, o, pointer, lexemes)?;
            check_external_integers(spec.go_type, o, pointer.as_str(), lexemes)?;
            check_request_file_system(spec.go_type, o, pointer.as_str())?;
        }
        return Ok(());
    }
    match spec.kind {
        FieldKind::Doc => {
            if matches!(v, Value::Null) && spec.nullable {
                return Ok(());
            }
            check_document(v, pointer.as_str())
        }
        FieldKind::DocList => match v {
            Value::Null => Ok(()),
            Value::Array(items) => {
                for (i, item) in items.iter().enumerate() {
                    check_document(item, &format!("{pointer}/{i}"))?;
                }
                Ok(())
            }
            _ => Err(mismatch(v, pointer.as_str(), spec.go_type)),
        },
        FieldKind::Array => match v {
            Value::Array(items) => {
                let elem = spec.elem.unwrap_or(Elem::Any);
                let elem_type = spec.go_type.trim_start_matches("[]");
                let len = pointer.len();
                for (i, item) in items.iter().enumerate() {
                    pointer.truncate(len);
                    pointer.push_index(i);
                    // A null element is the element type's zero value (or nil), as for fields.
                    if !matches!(item, Value::Null)
                        && !check_scalar(
                            elem,
                            item,
                            lexemes.get(pointer.as_str()).map(String::as_str),
                        )
                    {
                        return Err(mismatch(item, pointer.as_str(), elem_type));
                    }
                    if let Value::Object(o) = item {
                        check_struct(elem_type, o, pointer, lexemes)?;
                    }
                }
                pointer.truncate(len);
                Ok(())
            }
            _ => Err(mismatch(v, pointer.as_str(), spec.go_type)),
        },
        _ => Ok(()),
    }
}

/// A strict-JSON scanner over the raw request bytes (already validated by `strictjson`).
struct Scanner<'a> {
    b: &'a [u8],
    i: usize,
}

impl Scanner<'_> {
    fn ws(&mut self) {
        self.i += (self.i..self.b.len())
            .take_while(|&index| matches!(self.b[index], b' ' | b'\t' | b'\n' | b'\r'))
            .count();
    }
    // A string token, decoded like the parsed value's object keys (escapes such as `\u0073`).
    fn string(&mut self) -> String {
        let start = self.i;
        self.i += 1;
        while self.i < self.b.len() && self.b[self.i] != b'"' {
            if self.b[self.i] == b'\\' {
                self.i += 1;
            }
            self.i += 1;
        }
        self.i += 1;
        let raw =
            tsrs_core::utf8::from_utf8_lossy(&self.b[start..self.i.min(self.b.len())]).into_owned();
        match tsrs_core::json::unmarshal(&raw) {
            Ok(Value::String(s)) => s,
            _ => raw.trim_matches('"').to_string(),
        }
    }
    fn skip_string(&mut self) {
        self.i += 1;
        while self.i < self.b.len() && self.b[self.i] != b'"' {
            if self.b[self.i] == b'\\' {
                self.i += 1;
            }
            self.i += 1;
        }
        self.i += 1;
    }
    /// Skips one value.
    fn value(&mut self) {
        self.ws();
        if self.i >= self.b.len() {
            return;
        }
        match self.b[self.i] {
            b'"' => self.skip_string(),
            b'{' => {
                self.i += 1;
                loop {
                    self.ws();
                    if self.i >= self.b.len() || self.b[self.i] == b'}' {
                        self.i += 1;
                        return;
                    }
                    if self.b[self.i] == b',' {
                        self.i += 1;
                        continue;
                    }
                    self.skip_string();
                    self.ws();
                    self.i += 1; // ':'
                    self.value();
                }
            }
            b'[' => {
                self.i += 1;
                loop {
                    self.ws();
                    if self.i >= self.b.len() || self.b[self.i] == b']' {
                        self.i += 1;
                        return;
                    }
                    if self.b[self.i] == b',' {
                        self.i += 1;
                        continue;
                    }
                    self.value();
                }
            }
            _ => {
                self.i += (self.i..self.b.len())
                    .take_while(|&index| {
                        !matches!(self.b[index], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r')
                    })
                    .count();
            }
        }
    }
}

/// The raw bytes of the value at JSON pointer `pointer` in `raw` (strict JSON), if present.
pub fn raw_value_at<'a>(raw: &'a [u8], pointer: &str) -> Option<&'a [u8]> {
    let mut s = Scanner { b: raw, i: 0 };
    let mut tokens = pointer.split('/').skip(1);
    loop {
        s.ws();
        let Some(want) = tokens.next() else {
            let start = s.i;
            s.value();
            return Some(&raw[start..s.i.min(raw.len())]);
        };
        match raw.get(s.i)? {
            b'{' => {
                s.i += 1;
                loop {
                    s.ws();
                    match raw.get(s.i)? {
                        b'}' => return None,
                        b',' => {
                            s.i += 1;
                            continue;
                        }
                        _ => {}
                    }
                    let key = s.string();
                    s.ws();
                    s.i += 1; // ':'
                    if token(&key) == want {
                        break;
                    }
                    s.value();
                }
            }
            b'[' => {
                let want: usize = want.parse().ok()?;
                s.i += 1;
                let mut index = 0;
                loop {
                    s.ws();
                    match raw.get(s.i)? {
                        b']' => return None,
                        b',' => {
                            s.i += 1;
                            continue;
                        }
                        _ => {}
                    }
                    if index == want {
                        break;
                    }
                    s.value();
                    index += 1;
                }
            }
            _ => return None,
        }
    }
}

/// Checks `params` (an object, or `{}` for `null`) against the method's pinned params struct, nested api structs
/// included. `raw` is the request payload the value was parsed from. Returns the number literals by pointer.
/// `lexemes` are the request's inexact number literals (`strictjson::Decoded::lexemes`); it returns them.
pub(crate) fn predecode(
    method: &str,
    go_type: &str,
    params: &Value,
    lexemes: FxHashMap<String, String>,
) -> ApiResult<FxHashMap<String, String>> {
    let Value::Object(o) = params else {
        return Ok(lexemes);
    };
    // Inexact literals are looked up by pointer, so with any the pointer must be on from the start.
    if lexemes.is_empty() && check_params(method, o, &mut Pointer::new(false), &lexemes).is_ok() {
        return Ok(lexemes);
    }
    check_params(method, o, &mut Pointer::new(true), &lexemes).map_err(|e| {
        ApiError::invalid_request(format!("failed to unmarshal *api.{go_type}: json: {e}"))
    })?;
    Ok(lexemes)
}

fn check_params(
    method: &str,
    o: &JsonMap,
    pointer: &mut Pointer,
    lexemes: &FxHashMap<String, String>,
) -> Result<(), String> {
    let mut next = 0;
    for spec in params_fields(method) {
        if let Some(v) = o.get_from(spec.name, &mut next) {
            pointer.truncate(0);
            pointer.push_key(spec.name);
            check_field(spec, v, pointer, lexemes)?;
        }
    }
    Ok(())
}

/// The current request on this thread: its raw payload, number literals by pointer, and the JSON pointer of each
/// object of the parsed params tree by address (so typed accessors find a value's own literal, never a same-named
/// field elsewhere). Stacked for nested dispatch (batchRequests, callback re-entry).
struct Frame {
    raw: Vec<u8>,
    lexemes: FxHashMap<String, String>,
    objects: FxHashMap<usize, String>,
}

thread_local! {
    static FRAMES: std::cell::RefCell<Vec<Frame>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn index_objects(v: &Value, pointer: &mut String, out: &mut FxHashMap<usize, String>) {
    match v {
        Value::Object(o) => {
            out.insert(std::ptr::from_ref::<Value>(v) as usize, pointer.clone());
            for (k, item) in o {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&token(k));
                index_objects(item, pointer, out);
                pointer.truncate(len);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                let len = pointer.len();
                pointer.push('/');
                pointer.push_str(&i.to_string());
                index_objects(item, pointer, out);
                pointer.truncate(len);
            }
        }
        _ => {}
    }
}

/// Makes `root` (the parsed params, alive and unmodified until the guard drops), its raw payload and number
/// literals the current request.
#[expect(clippy::implicit_hasher, reason = "Public for the integration tests.")]
pub fn enter_request(raw: &[u8], lexemes: FxHashMap<String, String>, root: &Value) -> RequestGuard {
    let mut objects = FxHashMap::default();
    if !lexemes.is_empty() {
        index_objects(root, &mut String::new(), &mut objects);
    }
    FRAMES.with(|f| {
        f.borrow_mut().push(Frame { raw: raw.to_vec(), lexemes, objects });
    });
    RequestGuard(())
}

pub struct RequestGuard(());

impl Drop for RequestGuard {
    fn drop(&mut self) {
        FRAMES.with(|f| {
            f.borrow_mut().pop();
        });
    }
}

/// The exact unsigned integer literal of member `key` of `object`, when `object` is part of the current request's
/// params tree (by address) and the member is a number literal.
pub fn exact_u64(object: &Value, key: &str) -> Option<u64> {
    FRAMES.with(|f| {
        let f = f.borrow();
        let frame = f.last()?;
        let pointer = frame.objects.get(&(std::ptr::from_ref::<Value>(object) as usize))?;
        frame.lexemes.get(&format!("{pointer}/{}", token(key))).and_then(|s| s.parse::<u64>().ok())
    })
}

/// The exact signed integer literal of member `key` of `object` (see `exact_u64`).
fn exact_i64(object: &Value, key: &str) -> Option<i64> {
    FRAMES.with(|f| {
        let f = f.borrow();
        let frame = f.last()?;
        let pointer = frame.objects.get(&(std::ptr::from_ref::<Value>(object) as usize))?;
        frame.lexemes.get(&format!("{pointer}/{}", token(key))).and_then(|s| s.parse::<i64>().ok())
    })
}

/// The raw `params` bytes of each element of the top-level `requests` array of the current request
/// (batchRequests), indexed in one pass over the payload; `None` where an element has no `params`.
pub(crate) fn current_batch_params() -> Vec<Option<Vec<u8>>> {
    FRAMES.with(|f| f.borrow().last().map(|frame| batch_params(&frame.raw)).unwrap_or_default())
}

pub fn batch_params(raw: &[u8]) -> Vec<Option<Vec<u8>>> {
    let Some(requests) = raw_value_at(raw, "/requests") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut s = Scanner { b: requests, i: 0 };
    s.ws();
    if requests.first() != Some(&b'[') {
        return out;
    }
    s.i += 1;
    loop {
        s.ws();
        match requests.get(s.i) {
            None | Some(b']') => return out,
            Some(b',') => {
                s.i += 1;
            }
            Some(b'{') => {
                let mut params = None;
                s.i += 1;
                loop {
                    s.ws();
                    match requests.get(s.i) {
                        None => return out,
                        Some(b'}') => {
                            s.i += 1;
                            break;
                        }
                        Some(b',') => {
                            s.i += 1;
                            continue;
                        }
                        _ => {}
                    }
                    let key = s.string();
                    s.ws();
                    s.i += 1; // ':'
                    s.ws();
                    let start = s.i;
                    s.value();
                    if key == "params" {
                        params = Some(requests[start..s.i.min(requests.len())].to_vec());
                    }
                }
                out.push(params);
            }
            Some(_) => {
                s.value();
                out.push(None);
            }
        }
    }
}
