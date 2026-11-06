// Package jsonrpc provides generic JSON-RPC 2.0 types and utilities
// that can be shared between LSP and other JSON-RPC based protocols.

use std::fmt;

use crate::json::{Json, JsonError, ObjectWriter, Value, kind};
use crate::structcodec::{field, opt, struct_members};

// jsonrpc.go:14
// JSONRPCVersion represents the JSON-RPC version field, always "2.0".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct JSONRPCVersion;

const JSON_RPC_VERSION: &str = "2.0";

const ERR_INVALID_JSONRPC_VERSION: &str = "invalid JSON-RPC version";

impl Json for JSONRPCVersion {
    const GO_TYPE: &'static str = "jsonrpc.JSONRPCVersion";

    // jsonrpc.go:18
    fn to_json(&self) -> Value {
        Value::String(JSON_RPC_VERSION.to_string())
    }

    // jsonrpc.go:24
    fn from_json(v: &Value) -> Result<Self, JsonError> {
        if !matches!(v, Value::String(s) if s == JSON_RPC_VERSION) {
            return Err(JsonError::method_v1(kind(v), Self::GO_TYPE, ERR_INVALID_JSONRPC_VERSION));
        }
        Ok(JSONRPCVersion)
    }
}

// jsonrpc.go:32
// ID represents a JSON-RPC message ID, which can be either a string or integer.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct ID {
    str: String,
    int: i32,
}

// jsonrpc.go:38
// NewID creates an ID from an IntegerOrString value.

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
}

impl fmt::Display for ID {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.string())
    }
}

impl Json for ID {
    const GO_TYPE: &'static str = "jsonrpc.ID";

    // jsonrpc.go:62
    fn to_json(&self) -> Value {
        if !self.str.is_empty() {
            return Value::String(self.str.clone());
        }
        self.int.to_json()
    }

    // jsonrpc.go:69
    fn from_json(v: &Value) -> Result<Self, JsonError> {
        if let Value::String(s) = v {
            return Ok(ID { str: s.clone(), int: 0 });
        }
        Ok(ID { str: String::new(), int: i32::from_json(v)? })
    }
}

// jsonrpc.go:92
// IntegerOrString is a helper type for creating IDs.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct IntegerOrString {
    integer: Option<i32>,
    string: Option<String>,
}

// jsonrpc.go:98
// ResponseError represents a JSON-RPC error response.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResponseError {
    pub code: i32,
    pub message: String,
    pub data: Option<Value>,
}

impl ResponseError {
    // jsonrpc.go:104
    pub fn string(&self) -> String {
        let data = tsrs_core::json::marshal(self.data.as_ref().unwrap_or(&Value::Null));
        if data.is_err() {
            return format!("[{}]: {}\n[]", self.code, self.message);
        }
        format!("[{}]: {}", self.code, self.message)
    }
}

// jsonrpc.go:115
impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.string())
    }
}

impl std::error::Error for ResponseError {}

impl Json for ResponseError {
    const GO_TYPE: &'static str = "jsonrpc.ResponseError";

    fn to_json(&self) -> Value {
        let mut w = ObjectWriter::new(3);
        w.field("code", &self.code);
        w.field("message", &self.message);
        w.opt("data", self.data.as_ref());
        w.finish()
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        let mut s = Self::default();
        let Some(members) = struct_members(v, Self::GO_TYPE, false)? else {
            return Ok(s);
        };
        for (k, v) in members {
            match k {
                "code" => s.code = field(k, v)?,
                "message" => s.message = field(k, v)?,
                "data" => s.data = opt(k, v)?,
                _ => {}
            }
        }
        Ok(s)
    }
}

// jsonrpc.go:120
// Standard JSON-RPC error codes.
pub const CODE_PARSE_ERROR: i32 = -32700;
pub const CODE_INVALID_REQUEST: i32 = -32600;
pub const CODE_METHOD_NOT_FOUND: i32 = -32601;
pub const CODE_INVALID_PARAMS: i32 = -32602;
pub const CODE_INTERNAL_ERROR: i32 = -32603;

// jsonrpc.go:129
// MessageKind indicates what type of message this is.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug, Default)]
pub enum MessageKind {
    #[default]
    Notification,
    Request,
    Response,
}

// jsonrpc.go:139
// Message represents a raw JSON-RPC message that can be a request, notification, or response.
// Unlike lsproto.Message, this keeps params/result as raw JSON for generic handling.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Message {
    id: Option<ID>,
    method: String,
    params: Option<Value>,
    result: Option<Value>,
    error: Option<ResponseError>,
}

impl Message {
    // jsonrpc.go:149
    // Kind returns the kind of message this is.

    // jsonrpc.go:160
    // IsRequest returns true if this message is a request (has ID and method).

    // jsonrpc.go:165
    // IsNotification returns true if this message is a notification (has method but no ID).

    // jsonrpc.go:170
    // IsResponse returns true if this message is a response (has ID but no method).
}

impl Json for Message {
    const GO_TYPE: &'static str = "jsonrpc.Message";

    fn to_json(&self) -> Value {
        let mut w = ObjectWriter::new(6);
        w.field("jsonrpc", &JSONRPCVersion);
        w.opt("id", self.id.as_ref());
        if !self.method.is_empty() {
            w.field("method", &self.method);
        }
        w.opt("params", self.params.as_ref());
        w.opt("result", self.result.as_ref());
        w.opt("error", self.error.as_ref());
        w.finish()
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        let mut s = Self::default();
        let Some(members) = struct_members(v, Self::GO_TYPE, false)? else {
            return Ok(s);
        };
        for (k, v) in members {
            match k {
                "jsonrpc" => {
                    field::<JSONRPCVersion>(k, v)?;
                }
                "id" => s.id = opt(k, v)?,
                "method" => s.method = field(k, v)?,
                // Raw fields are present even when their JSON value is null.
                "params" => s.params = Some(v.clone()),
                "result" => s.result = Some(v.clone()),
                "error" => s.error = opt(k, v)?,
                _ => {}
            }
        }
        Ok(s)
    }
}

// jsonrpc.go:175
// RequestMessage is a convenience type for creating request/notification messages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestMessage {
    id: Option<ID>,
    method: String,
    params: Option<Value>,
}

impl Json for RequestMessage {
    const GO_TYPE: &'static str = "jsonrpc.RequestMessage";

    fn to_json(&self) -> Value {
        let mut w = ObjectWriter::new(4);
        w.field("jsonrpc", &JSONRPCVersion);
        w.opt("id", self.id.as_ref());
        w.field("method", &self.method);
        w.opt("params", self.params.as_ref());
        w.finish()
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        let mut s = Self::default();
        let Some(members) = struct_members(v, Self::GO_TYPE, false)? else {
            return Ok(s);
        };
        for (k, v) in members {
            match k {
                "jsonrpc" => {
                    field::<JSONRPCVersion>(k, v)?;
                }
                "id" => s.id = opt(k, v)?,
                "method" => s.method = field(k, v)?,
                "params" => s.params = opt(k, v)?,
                _ => {}
            }
        }
        Ok(s)
    }
}

// jsonrpc.go:183
// ResponseMessage is a convenience type for creating response messages.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResponseMessage {
    id: Option<ID>,
    result: Option<Value>,
    error: Option<ResponseError>,
}

impl Json for ResponseMessage {
    const GO_TYPE: &'static str = "jsonrpc.ResponseMessage";

    fn to_json(&self) -> Value {
        let mut w = ObjectWriter::new(4);
        w.field("jsonrpc", &JSONRPCVersion);
        w.opt("id", self.id.as_ref());
        w.opt("result", self.result.as_ref());
        w.opt("error", self.error.as_ref());
        w.finish()
    }

    fn from_json(v: &Value) -> Result<Self, JsonError> {
        let mut s = Self::default();
        let Some(members) = struct_members(v, Self::GO_TYPE, false)? else {
            return Ok(s);
        };
        for (k, v) in members {
            match k {
                "jsonrpc" => {
                    field::<JSONRPCVersion>(k, v)?;
                }
                "id" => s.id = opt(k, v)?,
                "result" => s.result = opt(k, v)?,
                "error" => s.error = opt(k, v)?,
                _ => {}
            }
        }
        Ok(s)
    }
}
