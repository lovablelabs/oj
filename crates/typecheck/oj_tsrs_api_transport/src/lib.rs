//! Transport for the tsgo-compatible programmatic API (`tsrs --api [--async]`).
//!
//! Ported from microsoft/TypeScript at the commit pinned in the workspace Cargo.toml
//! (`[workspace.metadata.typescript]`): `tsc/internal/ipc`, `tsc/internal/jsonrpc` framing, and
//! `tsc/internal/api/{protocol_msgpack.go,callbackfs.go}`. See README.md in this crate for the
//! contract with the API session and the known gaps.

pub mod base64;
pub mod callbackfs;
pub mod conn_async;
pub mod conn_sync;
pub mod handler;
pub mod jsonrpc;
pub mod message;
pub mod msgpack;
pub mod protocol;
pub mod reentrancy;
pub mod strictjson;
pub mod timing;
pub mod transport;

pub use callbackfs::{CallbackConfig, CallbackFs};
pub use conn_async::AsyncConn;
pub use conn_sync::{ConnOptions, SyncConn};
pub use handler::{Caller, CancellationToken, Handler, RequestContext};
pub use message::{ApiError, Id, Message, Response, ResponseError, TransportError};
pub use protocol::WireProtocol;
pub use reentrancy::{
    ContentionWait, Holder, RequestScope, blocking_may_deadlock, current_request, lock_for_request,
};
pub use transport::{Connection, LateCaller, PipeListener, ServeOptions, Stream, serve};
