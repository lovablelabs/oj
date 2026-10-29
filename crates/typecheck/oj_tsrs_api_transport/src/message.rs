// Port of tsc/internal/jsonrpc/jsonrpc.go (Message, ID, ResponseError) as used by tsc/internal/ipc.
//
// Payload fields stay raw bytes: JSON text for JSON-RPC, and the untouched bin payload for the
// MessagePack tuple protocol (tsc/internal/api/protocol_msgpack.go stores it in Params/Result).

use std::fmt;
use std::io::Read as _;

// jsonrpc.go: Code* constants.
pub const CODE_PARSE_ERROR: i32 = -32700;
pub const CODE_INVALID_REQUEST: i32 = -32600;
pub const CODE_METHOD_NOT_FOUND: i32 = -32601;
pub const CODE_INVALID_PARAMS: i32 = -32602;
const CODE_INTERNAL_ERROR: i32 = -32603;

/// jsonrpc.ID: either a string or an int32. Like Go, an empty string is indistinguishable from the
/// integer form (`ID{str: ""}` marshals as its int field).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Id {
    Str(String),
    Int(i32),
}

impl Id {
    pub fn string(s: impl Into<String>) -> Id {
        Id::Str(s.into())
    }

    /// jsonrpc.go ID.MarshalJSON.
    pub fn to_json(&self) -> String {
        match self {
            Id::Str(s) if !s.is_empty() => serde_json::to_string(s).expect("string serializes"),
            Id::Str(_) => "0".to_string(),
            Id::Int(i) => i.to_string(),
        }
    }
}

impl Id {
    /// The `Display` text, borrowed when the id is a non-empty string (the msgpack protocol's ids are method
    /// names, written on every response).
    pub(crate) fn text(&self) -> std::borrow::Cow<'_, str> {
        match self {
            Id::Str(s) if !s.is_empty() => std::borrow::Cow::Borrowed(s),
            _ => std::borrow::Cow::Owned(self.to_string()),
        }
    }
}

impl fmt::Display for Id {
    // jsonrpc.go ID.String.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Id::Str(s) if !s.is_empty() => f.write_str(s),
            Id::Str(_) => f.write_str("0"),
            Id::Int(i) => write!(f, "{i}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseError {
    pub code: i32,
    pub message: String,
}

impl ResponseError {
    pub fn internal(message: impl Into<String>) -> ResponseError {
        ResponseError { code: CODE_INTERNAL_ERROR, message: message.into() }
    }
}

impl fmt::Display for ResponseError {
    // jsonrpc.go ResponseError.String (Data is never set by the API server).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}]: {}", self.code, self.message)
    }
}

/// jsonrpc.Message. `params`/`result` are `None` when absent (Go `omitzero` on an empty json.Value).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Message {
    pub id: Option<Id>,
    pub method: String,
    pub params: Option<Vec<u8>>,
    pub result: Option<Vec<u8>>,
    pub error: Option<ResponseError>,
}

impl Message {
    pub fn is_request(&self) -> bool {
        self.id.is_some() && !self.method.is_empty()
    }

    pub fn is_notification(&self) -> bool {
        self.id.is_none() && !self.method.is_empty()
    }

    pub fn is_response(&self) -> bool {
        self.id.is_some() && self.method.is_empty()
    }

    pub(crate) fn params_bytes(&self) -> &[u8] {
        self.params.as_deref().unwrap_or(&[])
    }
}

/// A handler result.
///
/// `Json` must hold one complete JSON value (validated before it is written, as Go's json.Marshal of a
/// json.Value does). `ValidJson` is JSON its producer guarantees valid (a JSON encoder's output), written
/// without that check. `Binary` is api.RawBinary: written verbatim as the `MessagePack` tuple payload, and
/// (like Go's json.Marshal of a byte slice) as a base64 JSON string under JSON-RPC.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    Json(Vec<u8>),
    ValidJson(Vec<u8>),
    Binary(Vec<u8>),
}

impl Response {
    /// The `nil` result: Go writes `null` for both protocols.
    pub fn null() -> Response {
        Response::Json(b"null".to_vec())
    }

    pub fn json<T: serde::Serialize + ?Sized>(value: &T) -> Result<Response, ApiError> {
        serde_json::to_vec(value)
            .map(Response::Json)
            .map_err(|e| ApiError::internal(format!("failed to marshal result: {e}")))
    }
}

/// A structured handler error.
///
/// All API session errors are reported with `CodeInternalError` by ipc.handleRequest; the message is the only
/// data the client sees (msgpack Error payload / JSON-RPC error.message).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub(crate) code: i32,
    pub message: String,
}

impl ApiError {
    pub fn internal(message: impl Into<String>) -> ApiError {
        ApiError { code: CODE_INTERNAL_ERROR, message: message.into() }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

/// Transport-level failure. Every variant ends the connection's Run loop (as in Go, where any `ReadMessage`
/// error other than io.EOF is returned from Run).
#[derive(Debug)]
pub enum TransportError {
    /// Clean EOF before the first byte of a frame.
    Eof,
    /// EOF inside a frame.
    UnexpectedEof(&'static str),
    Io(std::io::Error),
    /// Malformed frame or message (api.ErrInvalidRequest / jsonrpc header errors / JSON errors).
    Protocol(String),
    /// A declared frame length above the configured limit.
    FrameTooLarge {
        declared: u64,
        limit: u64,
    },
    /// The connection is closed (ipc.ErrConnClosed), optionally with its terminal cause.
    Closed(Option<String>),
    /// The remote side answered a call with an error (ipc: remote error [code]: message).
    Remote(ResponseError),
    /// The connection received a message it cannot accept in its current state.
    Unexpected(String),
}

impl TransportError {
    pub fn is_eof(&self) -> bool {
        matches!(self, TransportError::Eof)
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::Eof => f.write_str("EOF"),
            TransportError::UnexpectedEof(what) => write!(f, "{what}: unexpected EOF"),
            TransportError::Io(e) => write!(f, "{e}"),
            TransportError::Protocol(msg) | TransportError::Unexpected(msg) => f.write_str(msg),
            TransportError::FrameTooLarge { declared, limit } => {
                write!(f, "ipc: frame of {declared} bytes exceeds the {limit} byte limit")
            }
            TransportError::Closed(None) => f.write_str("ipc: connection closed"),
            TransportError::Closed(Some(cause)) => write!(f, "ipc: connection closed\n{cause}"),
            TransportError::Remote(e) => write!(f, "ipc: remote error [{}]: {}", e.code, e.message),
        }
    }
}

impl std::error::Error for TransportError {}

impl From<std::io::Error> for TransportError {
    fn from(e: std::io::Error) -> Self {
        TransportError::Io(e)
    }
}

/// Default bound for a single incoming frame (Content-Length body or msgpack bin field).
pub(crate) const DEFAULT_MAX_FRAME_BYTES: u64 = 1 << 30;

/// Reads exactly `len` bytes without trusting `len` for the up-front allocation: the buffer grows
/// only as bytes actually arrive, so a hostile length cannot force a huge allocation.
pub(crate) fn read_exact_bounded<R: std::io::Read>(
    r: &mut R,
    len: u64,
    limit: u64,
    what: &'static str,
) -> Result<Vec<u8>, TransportError> {
    const INITIAL: u64 = 64 * 1024;

    if len > limit {
        return Err(TransportError::FrameTooLarge { declared: len, limit });
    }
    let mut out = Vec::with_capacity(len.min(INITIAL) as usize);
    let n = (&mut *r).take(len).read_to_end(&mut out)?;
    if (n as u64) < len {
        return Err(TransportError::UnexpectedEof(what));
    }
    Ok(out)
}
