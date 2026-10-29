// Go internal/ipc and internal/jsonrpc: the JSON-RPC connection the compiler drives a content mapper process over
// (notes/contentmappers.md). Only what contentmapper/hostimpl.go uses is ported: the JSON-RPC protocol over a
// read/write stream, the async connection (concurrent calls, incoming requests answered by a Handler, termination on
// EOF or a read error) and the jsonrpc message types. Package ipc is the crate root; package jsonrpc is `jsonrpc`.

pub mod conn;
pub mod conn_async;
pub mod error;
pub mod jsonrpc;
pub mod protocol;
pub mod protocol_jsonrpc;
pub mod stream;

pub use conn::*;
pub use conn_async::*;
pub use error::*;
pub use protocol::*;
pub use protocol_jsonrpc::*;
pub use stream::*;

use std::sync::{Mutex, MutexGuard, PoisonError};

// Go's sync.Mutex has no poisoning: a goroutine that panics with the lock held (and is recovered, as a request
// handler is) leaves the data usable. A Rust handler panic that unwinds through a guard is caught the same way.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}
