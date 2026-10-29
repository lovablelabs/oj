// Package jsonrpc provides generic JSON-RPC 2.0 types and utilities
// that can be shared between LSP and other JSON-RPC based protocols.
//
// Go encodes these types with encoding/json/v2 (struct tags plus the MarshalJSON / UnmarshalJSON methods); without
// reflection or serde they are written and read by hand with tsrs_core::json, members in Go struct order, `omitzero`
// as tagged. Decoding errors carry json/v2's SemanticError text; JSON syntax errors carry tsrs_core::json's.

use std::fmt;

use tsrs_core::json::{self, Value};

use crate::Error;

// jsonrpc.go:16
const JSONRPC_VERSION: &str = "2.0";

// jsonrpc.go:18
// JSONRPCVersion.MarshalJSON: the "jsonrpc" member every message starts with.
const MESSAGE_START: &str = "{\"jsonrpc\":\"2.0\"";

// jsonrpc.go:22
const ERR_INVALID_JSONRPC_VERSION: &str = "invalid JSON-RPC version";

// jsonrpc.go:24
// JSONRPCVersion.UnmarshalJSON. Go compares the raw member text with `"2.0"`; this compares the decoded string, so
// an escaped spelling of 2.0 is accepted.
fn unmarshal_jsonrpc_version(v: &Value) -> Result<(), Error> {
    if matches!(v, Value::String(s) if s == JSONRPC_VERSION) {
        return Ok(());
    }
    Err(unmarshal_error(v, None, "jsonrpc.JSONRPCVersion", "/jsonrpc", ERR_INVALID_JSONRPC_VERSION))
}

// jsonrpc.go:32
// ID represents a JSON-RPC message ID, which can be either a string or integer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ID {
    str: String,
    int: i32,
}

// jsonrpc.go:46
// NewIDString creates a string ID.
pub fn new_id_string(str: &str) -> ID {
    ID { str: str.to_string(), int: 0 }
}

// jsonrpc.go:51
// NewIDInt creates an integer ID.
pub fn new_id_int(i: i32) -> ID {
    ID { str: String::new(), int: i }
}

impl ID {
    // jsonrpc.go:55
    pub fn string(&self) -> String {
        if !self.str.is_empty() {
            return self.str.clone();
        }
        self.int.to_string()
    }

    // jsonrpc.go:62
    fn marshal_json(&self, out: &mut String) {
        if !self.str.is_empty() {
            json::write_compact_string(out, &self.str);
            return;
        }
        json::write_compact_int(out, i64::from(self.int));
    }

    // jsonrpc.go:69
    // A JSON null never gets here: it decodes as a nil *ID.
    fn unmarshal_json(v: Value) -> Result<ID, Error> {
        if let Value::String(str) = v {
            return Ok(ID { str, int: 0 });
        }
        Ok(ID { str: String::new(), int: unmarshal_int32(&v, "/id")? })
    }
}

impl fmt::Display for ID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if !self.str.is_empty() {
            return f.write_str(&self.str);
        }
        write!(f, "{}", self.int)
    }
}

// jsonrpc.go:98
// ResponseError represents a JSON-RPC error response.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResponseError {
    pub code: i32,
    pub message: String,
    // Go `Data any`: None is a nil Data (omitted; a JSON null decodes to it).
    pub data: Option<Value>,
}

impl ResponseError {
    // jsonrpc.go:104
    fn string(&self) -> String {
        if json::marshal(self.data.as_ref().unwrap_or(&Value::Null)).is_err() {
            // Go prints the nil []byte json.Marshal returned with its error.
            return format!("[{}]: {}\n[]", self.code, self.message);
        }
        format!("[{}]: {}", self.code, self.message)
    }

    fn marshal_json(&self, out: &mut String) -> Result<(), Error> {
        out.push_str("{\"code\":");
        json::write_compact_int(out, i64::from(self.code));
        out.push_str(",\"message\":");
        json::write_compact_string(out, &self.message);
        if let Some(data) = &self.data {
            out.push_str(",\"data\":");
            json::write_compact(out, data).map_err(Error::new)?;
        }
        out.push('}');
        Ok(())
    }

    fn unmarshal_json(v: Value) -> Result<ResponseError, Error> {
        let mut r = ResponseError::default();
        let members = match v {
            Value::Object(members) => members,
            Value::Null => return Ok(r),
            v => {
                return Err(unmarshal_error(&v, None, "jsonrpc.ResponseError", "/error", ""));
            }
        };
        for (k, v) in members {
            match k.as_str() {
                "code" => r.code = unmarshal_int32(&v, "/error/code")?,
                "message" => r.message = unmarshal_string(v, "/error/message")?,
                "data" => r.data = if matches!(v, Value::Null) { None } else { Some(v) },
                _ => {}
            }
        }
        Ok(r)
    }
}

// jsonrpc.go:115
impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.string())
    }
}

impl std::error::Error for ResponseError {}

// jsonrpc.go:120
// Standard JSON-RPC error codes.
pub const CODE_PARSE_ERROR: i32 = -32700;
pub const CODE_INVALID_REQUEST: i32 = -32600;
pub const CODE_METHOD_NOT_FOUND: i32 = -32601;
pub const CODE_INVALID_PARAMS: i32 = -32602;
pub const CODE_INTERNAL_ERROR: i32 = -32603;

// jsonrpc.go:139
// Message represents a raw JSON-RPC message that can be a request, notification, or response.
// Unlike lsproto.Message, this keeps params/result as raw JSON for generic handling.
// `params` / `result` are None when the member is absent (Go: an empty json.Value, which `omitzero` drops and
// json.Unmarshal rejects) and Some(Value::Null) for an explicit null.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Message {
    pub id: Option<ID>,
    pub method: String,
    pub params: Option<Value>,
    pub result: Option<Value>,
    pub error: Option<ResponseError>,
}

impl Message {
    // jsonrpc.go:160
    // IsRequest returns true if this message is a request (has ID and method).
    pub fn is_request(&self) -> bool {
        self.id.is_some() && !self.method.is_empty()
    }

    // jsonrpc.go:165
    // IsNotification returns true if this message is a notification (has method but no ID).
    pub fn is_notification(&self) -> bool {
        self.id.is_none() && !self.method.is_empty()
    }

    // jsonrpc.go:170
    // IsResponse returns true if this message is a response (has ID but no method).
    pub fn is_response(&self) -> bool {
        self.id.is_some() && self.method.is_empty()
    }

    // Go `json.Marshal(m)`.
    pub fn marshal(&self) -> Result<String, Error> {
        let mut out = String::from(MESSAGE_START);
        if let Some(id) = &self.id {
            out.push_str(",\"id\":");
            id.marshal_json(&mut out);
        }
        if !self.method.is_empty() {
            out.push_str(",\"method\":");
            json::write_compact_string(&mut out, &self.method);
        }
        if let Some(params) = &self.params {
            out.push_str(",\"params\":");
            json::write_compact(&mut out, params).map_err(Error::new)?;
        }
        if let Some(result) = &self.result {
            out.push_str(",\"result\":");
            json::write_compact(&mut out, result).map_err(Error::new)?;
        }
        if let Some(error) = &self.error {
            out.push_str(",\"error\":");
            error.marshal_json(&mut out)?;
        }
        out.push('}');
        Ok(out)
    }

    // Go `json.Unmarshal(data, &m)`. Unknown members are ignored and a JSON null decodes as the zero Message, as
    // json/v2 does.
    pub fn unmarshal(data: &[u8]) -> Result<Message, Error> {
        let text = tsrs_core::utf8::basic::from_utf8(data)
            .map_err(|_| Error::new("jsontext: invalid UTF-8 within message"))?;
        let mut m = Message::default();
        let members = match json::unmarshal(text).map_err(Error::new)? {
            Value::Object(members) => members,
            Value::Null => return Ok(m),
            v => return Err(unmarshal_error(&v, None, "jsonrpc.Message", "", "")),
        };
        for (k, v) in members {
            match k.as_str() {
                "jsonrpc" => unmarshal_jsonrpc_version(&v)?,
                "id" => {
                    m.id =
                        if matches!(v, Value::Null) { None } else { Some(ID::unmarshal_json(v)?) }
                }
                "method" => m.method = unmarshal_string(v, "/method")?,
                "params" => m.params = Some(v),
                "result" => m.result = Some(v),
                "error" => {
                    m.error = if matches!(v, Value::Null) {
                        None
                    } else {
                        Some(ResponseError::unmarshal_json(v)?)
                    }
                }
                _ => {}
            }
        }
        Ok(m)
    }
}

// jsonrpc.go:175
// RequestMessage is a convenience type for creating request/notification messages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestMessage {
    pub id: Option<ID>,
    pub method: String,
    pub params: Option<Value>,
}

impl RequestMessage {
    // Go `json.Marshal(m)`.
    pub fn marshal(&self) -> Result<String, Error> {
        marshal_request_message(self.id.as_ref(), &self.method, self.params.as_ref())
    }
}

// jsonrpc.go:183
// ResponseMessage is a convenience type for creating response messages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResponseMessage {
    pub id: Option<ID>,
    pub result: Option<Value>,
    pub error: Option<ResponseError>,
}

impl ResponseMessage {
    // Go `json.Marshal(m)`.
    pub fn marshal(&self) -> Result<String, Error> {
        marshal_response_message(self.id.as_ref(), self.result.as_ref(), self.error.as_ref())
    }
}

// Go `json.Marshal(jsonrpc.RequestMessage{ID: id, Method: method, Params: params})` from borrowed parts, so that
// writing (or logging) a request does not copy its params. A None `params` is Go's nil `any` (omitted).
pub fn marshal_request_message(
    id: Option<&ID>,
    method: &str,
    params: Option<&Value>,
) -> Result<String, Error> {
    let mut out = String::from(MESSAGE_START);
    if let Some(id) = id {
        out.push_str(",\"id\":");
        id.marshal_json(&mut out);
    }
    out.push_str(",\"method\":");
    json::write_compact_string(&mut out, method);
    if let Some(params) = params {
        out.push_str(",\"params\":");
        json::write_compact(&mut out, params).map_err(Error::new)?;
    }
    out.push('}');
    Ok(out)
}

// Go `json.Marshal(jsonrpc.ResponseMessage{ID: id, Result: result, Error: err})` from borrowed parts.
pub fn marshal_response_message(
    id: Option<&ID>,
    result: Option<&Value>,
    err: Option<&ResponseError>,
) -> Result<String, Error> {
    let mut out = String::from(MESSAGE_START);
    if let Some(id) = id {
        out.push_str(",\"id\":");
        id.marshal_json(&mut out);
    }
    if let Some(result) = result {
        out.push_str(",\"result\":");
        json::write_compact(&mut out, result).map_err(Error::new)?;
    }
    if let Some(err) = err {
        out.push_str(",\"error\":");
        err.marshal_json(&mut out)?;
    }
    out.push('}');
    Ok(out)
}

// json/v2's decoding of a Go string; null decodes as "".
fn unmarshal_string(v: Value, pointer: &str) -> Result<String, Error> {
    match v {
        Value::String(s) => Ok(s),
        Value::Null => Ok(String::new()),
        v => Err(unmarshal_error(&v, None, "string", pointer, "")),
    }
}

// json/v2's decoding of a Go int32; null decodes as 0. Go also rejects an integral number written with a fraction
// or an exponent (1.0, 1e3); the parsed value cannot tell those apart, so they are accepted here.
fn unmarshal_int32(v: &Value, pointer: &str) -> Result<i32, Error> {
    match v {
        Value::Number(n) => {
            let n = *n;
            let text = json::marshal_f64(n).unwrap_or_default();
            if n.fract() != 0.0 {
                return Err(unmarshal_error(v, Some(&text), "int32", pointer, "invalid syntax"));
            }
            if n < f64::from(i32::MIN) || n > f64::from(i32::MAX) {
                return Err(unmarshal_error(
                    v,
                    Some(&text),
                    "int32",
                    pointer,
                    "value out of range",
                ));
            }
            Ok(n as i32)
        }
        Value::Integer(n) => i32::try_from(*n).map_err(|_| {
            unmarshal_error(v, Some(&n.to_string()), "int32", pointer, "value out of range")
        }),
        Value::Null => Ok(0),
        _ => Err(unmarshal_error(v, None, "int32", pointer, "")),
    }
}

// json/v2's SemanticError text for a value it cannot unmarshal into a Go type:
// `json: cannot unmarshal JSON <kind>[ <value>] into Go <type>[ within "<pointer>"][: <err>]`.
fn unmarshal_error(
    v: &Value,
    value: Option<&str>,
    go_type: &str,
    pointer: &str,
    err: &str,
) -> Error {
    let mut s = String::from("json: cannot unmarshal");
    s.push_str(match v {
        Value::Null => " JSON null",
        Value::Bool(_) => " JSON boolean",
        Value::String(_) => " JSON string",
        Value::Number(_) | Value::Integer(_) => " JSON number",
        Value::Object(_) => " JSON object",
        Value::Array(_) => " JSON array",
    });
    if let Some(value) = value {
        s.push(' ');
        s.push_str(value);
    }
    s.push_str(" into Go ");
    s.push_str(go_type);
    if !pointer.is_empty() {
        s.push_str(" within ");
        json::write_compact_string(&mut s, pointer);
    }
    if !err.is_empty() {
        s.push_str(": ");
        s.push_str(err);
    }
    Error::new(s)
}
