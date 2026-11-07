// Go tsc/internal/ipc (Handler / Conn) and the error values of tsc/internal/api/proto.go.
//
// This is the transport-neutral boundary between the API session (this crate) and the wire runtime
// (crates/tsrs_api_transport). Params and JSON results are raw JSON bytes in both protocols: the sync
// MessagePack tuple protocol carries JSON payloads in `bin` fields, the async protocol is JSON-RPC.

use std::fmt;

/// Go `ErrInvalidRequest` / `ErrClientError` and friends. `Display` reproduces Go's `err.Error()`
/// text, which is what both protocols put on the wire (`jsonrpc.CodeInternalError` + message).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// Go `ErrInvalidRequest` ("api: invalid request"): malformed params or unknown method.
    InvalidRequest,
    /// Go `ErrClientError` ("api: client error"): stale/invalid handles, missing files, etc.
    ClientError,
    /// A pinned method that tsrs does not implement yet. Never reported as success.
    Unsupported,
    /// Anything else (encoder failure, callback failure, panic in a handler).
    Internal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApiError {
    pub kind: ErrorKind,
    pub(crate) message: String,
}

impl ApiError {
    pub(crate) fn invalid_request(message: impl Into<String>) -> ApiError {
        ApiError { kind: ErrorKind::InvalidRequest, message: message.into() }
    }
    pub(crate) fn client(message: impl Into<String>) -> ApiError {
        ApiError { kind: ErrorKind::ClientError, message: message.into() }
    }
    pub(crate) fn unsupported(method: &str) -> ApiError {
        ApiError {
            kind: ErrorKind::Unsupported,
            message: format!("method {method:?} is not implemented by tsrs (see docs/NODE_API.md)"),
        }
    }
    pub(crate) fn internal(message: impl Into<String>) -> ApiError {
        ApiError { kind: ErrorKind::Internal, message: message.into() }
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            ErrorKind::InvalidRequest => write!(f, "api: invalid request: {}", self.message),
            ErrorKind::ClientError => write!(f, "api: client error: {}", self.message),
            ErrorKind::Unsupported => write!(f, "api: unsupported: {}", self.message),
            ErrorKind::Internal => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ApiError {}

pub type ApiResult<T> = Result<T, ApiError>;

/// A request result. Go returns `any`: either a JSON-marshalable value or `RawBinary`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Response {
    /// Serialized JSON text (`null` for Go `nil`).
    Json(String),
    /// Go `RawBinary`: written as a msgpack `bin` payload. Only produced when the session was created with
    /// `binary_responses` (sync `MessagePack` protocol). An empty vec is Go `RawBinary(nil)`.
    Binary(Vec<u8>),
}

impl Response {
    pub(crate) fn null() -> Response {
        Response::Json("null".to_string())
    }
}

/// Go `ipc.Handler`. Implementations must be `Sync`: the async runtime may dispatch concurrent
/// requests, and filesystem callbacks re-enter the transport while a request is in flight.
pub trait Handler: Send + Sync {
    fn handle_request(&self, method: &str, params: &[u8]) -> ApiResult<Response>;
    /// A request that arrives while the server is blocked in a server->client call on the same connection
    /// (sync `MessagePack` mode: the client re-enters the API from a filesystem / resolver callback).
    /// `depth >= 1`. Nested requests are served like top-level ones (Go does the same); resources that are not
    /// reentrant are acquired with the transport's `lock_for_request`, which fails boundedly instead of
    /// deadlocking when the holder is waiting on the client (`crates/tsrs_api_transport/src/reentrancy.rs`).
    fn handle_nested_request(
        &self,
        method: &str,
        params: &[u8],
        depth: u32,
    ) -> ApiResult<Response> {
        let _ = depth;
        self.handle_request(method, params)
    }
    fn handle_notification(&self, method: &str, params: &[u8]) -> ApiResult<()>;
}

/// Server -> client calls (Go `ipc.Conn.Call`), used for filesystem callbacks and
/// `resolveModuleName` callbacks. `params` / result are JSON text.
pub trait ClientConn: Send + Sync {
    fn call(&self, method: &str, params: &str) -> ApiResult<String>;
}
