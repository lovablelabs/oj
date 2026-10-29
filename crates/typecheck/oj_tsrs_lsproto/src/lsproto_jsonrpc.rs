// Go internal/lsp/lsproto/jsonrpc.go (named lsproto_jsonrpc.rs because the crate also holds Go package
// internal/jsonrpc as module `jsonrpc`).

use crate::json::{Json, JsonError, ObjectWriter, Value, kind};
use crate::jsonrpc::{self, ID, JSONRPCVersion, MessageKind, ResponseError};
use crate::structcodec::{field, opt, struct_members};
use crate::{ErrorCode, IntegerOrString, Method};

// jsonrpc.go:12
// NewID creates an ID from an IntegerOrString value.
// This wrapper exists because lsproto has its own IntegerOrString type.
pub fn new_id(raw_value: &IntegerOrString) -> ID {
    if let Some(s) = &raw_value.string {
        return jsonrpc::new_id_string(s);
    }
    jsonrpc::new_id_int(raw_value.integer.unwrap())
}

// jsonrpc.go:19
#[derive(Clone, Debug, PartialEq)]
pub struct Message {
    pub kind: MessageKind,
    msg: MessageMsg,
}

#[derive(Clone, Debug, PartialEq)]
enum MessageMsg {
    Request(RequestMessage),
    Response(ResponseMessage),
}

impl Message {
    // jsonrpc.go:24
    pub fn as_request(&self) -> &RequestMessage {
        match &self.msg {
            MessageMsg::Request(r) => r,
            MessageMsg::Response(_) => panic!(
                "interface conversion: message is *lsproto.ResponseMessage, not *lsproto.RequestMessage"
            ),
        }
    }

    // jsonrpc.go:28
    pub fn as_response(&self) -> &ResponseMessage {
        match &self.msg {
            MessageMsg::Response(r) => r,
            MessageMsg::Request(_) => panic!(
                "interface conversion: message is *lsproto.RequestMessage, not *lsproto.ResponseMessage"
            ),
        }
    }

    pub fn into_request(self) -> RequestMessage {
        match self.msg {
            MessageMsg::Request(r) => r,
            MessageMsg::Response(_) => panic!(
                "interface conversion: message is *lsproto.ResponseMessage, not *lsproto.RequestMessage"
            ),
        }
    }

    pub fn into_response(self) -> ResponseMessage {
        match self.msg {
            MessageMsg::Response(r) => r,
            MessageMsg::Request(_) => panic!(
                "interface conversion: message is *lsproto.RequestMessage, not *lsproto.ResponseMessage"
            ),
        }
    }
}

// The anonymous struct Message.UnmarshalJSON decodes into.
#[derive(Default)]
struct RawMessage {
    method: Method,
    id: Option<ID>,
    params: Option<Value>,
    result: Option<Value>,
    error: Option<ResponseError>,
}

// `response_fields`: whether the raw struct has result and error fields (Message's does,
// RequestMessage's does not).
fn unmarshal_raw_message(v: &Value, response_fields: bool) -> Result<RawMessage, JsonError> {
    let mut raw = RawMessage::default();
    let Some(members) = struct_members(v, "struct", false)? else {
        return Ok(raw);
    };
    for (k, v) in members {
        match k {
            "jsonrpc" => {
                field::<JSONRPCVersion>(k, v)?;
            }
            "method" => raw.method = field(k, v)?,
            "id" => raw.id = opt(k, v)?,
            // json.Value fields keep the raw value, null included.
            "params" => raw.params = Some(v.clone()),
            "result" if response_fields => raw.result = Some(v.clone()),
            "error" if response_fields => raw.error = opt(k, v)?,
            _ => {}
        }
    }
    Ok(raw)
}

// Go `fmt.Errorf("%w: %w", ErrorCodeInvalidRequest, err)` returned from a v1 UnmarshalJSON method.
fn invalid_request(v: &Value, go_type: &str, err: &JsonError) -> JsonError {
    let mut wrapped =
        JsonError::method_v1(kind(v), go_type, format!("{}: {}", ErrorCode::INVALID_REQUEST, err));
    wrapped.codes = vec![ErrorCode::INVALID_REQUEST];
    wrapped.codes.extend_from_slice(&err.codes);
    wrapped
}

impl Json for Message {
    const GO_TYPE: &'static str = "lsproto.Message";

    // jsonrpc.go:76
    fn to_json(&self) -> Value {
        match &self.msg {
            MessageMsg::Request(r) => r.to_json(),
            MessageMsg::Response(r) => r.to_json(),
        }
    }

    // jsonrpc.go:32
    fn from_json(v: &Value) -> Result<Self, JsonError> {
        let raw = unmarshal_raw_message(v, true)
            .map_err(|err| invalid_request(v, Self::GO_TYPE, &err))?;
        if raw.id.is_some() && raw.method.0.is_empty() {
            return Ok(Message {
                kind: MessageKind::Response,
                msg: MessageMsg::Response(ResponseMessage {
                    id: raw.id,
                    result: raw.result,
                    error: raw.error,
                    raw_result: true,
                }),
            });
        }

        let kind = if raw.id.is_none() { MessageKind::Notification } else { MessageKind::Request };

        Ok(Message {
            kind,
            msg: MessageMsg::Request(RequestMessage {
                id: raw.id,
                method: raw.method,
                params: raw.params,
            }),
        })
    }
}

// jsonrpc.go:80
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RequestMessage {
    pub id: Option<ID>,
    pub method: Method,
    pub params: Option<Value>,
}

impl RequestMessage {
    // jsonrpc.go:87
    pub fn message(self) -> Message {
        let kind = if self.id.is_none() { MessageKind::Notification } else { MessageKind::Request };
        Message { kind, msg: MessageMsg::Request(self) }
    }
}

impl Json for RequestMessage {
    const GO_TYPE: &'static str = "lsproto.RequestMessage";

    fn to_json(&self) -> Value {
        let mut w = ObjectWriter::new(4);
        w.field("jsonrpc", &JSONRPCVersion);
        w.opt("id", self.id.as_ref());
        w.field("method", &self.method);
        w.opt("params", self.params.as_ref());
        w.finish()
    }

    // jsonrpc.go:98
    fn from_json(v: &Value) -> Result<Self, JsonError> {
        let raw = unmarshal_raw_message(v, false)
            .map_err(|err| invalid_request(v, Self::GO_TYPE, &err))?;
        Ok(RequestMessage { id: raw.id, method: raw.method, params: raw.params })
    }
}

// jsonrpc.go:118
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResponseMessage {
    pub id: Option<ID>,
    pub result: Option<Value>,
    pub error: Option<ResponseError>,
    // Set on responses read by Message's decoding, whose Go Result is always a (possibly empty) raw
    // json.Value: it marshals as null when the response had no result.
    pub raw_result: bool,
}

impl ResponseMessage {
    // jsonrpc.go:125
    pub fn message(self) -> Message {
        Message { kind: MessageKind::Response, msg: MessageMsg::Response(self) }
    }
}

impl Json for ResponseMessage {
    const GO_TYPE: &'static str = "lsproto.ResponseMessage";

    fn to_json(&self) -> Value {
        let mut w = ObjectWriter::new(4);
        w.field("jsonrpc", &JSONRPCVersion);
        w.field("id", &self.id);
        if self.result.is_none() && self.raw_result {
            w.raw("result", Value::Null);
        }
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
