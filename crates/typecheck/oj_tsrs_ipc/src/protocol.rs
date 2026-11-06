use tsrs_core::json::Value;

use crate::Error;
use crate::jsonrpc::{self, ID, ResponseError};

// protocol.go:8
// Message is an alias for jsonrpc.Message for convenience.
pub type Message = jsonrpc::Message;

// protocol.go:11
// Protocol defines the interface for reading and writing API messages.
// The connection reads on its read loop while caller and handler threads write (under its write lock), so the
// methods take `&self` and an implementation keeps its reading and writing state apart. A None `params` is Go's nil
// `any`; Go's nil result for WriteResponse is Value::Null.
pub trait Protocol: Send + Sync {
    // protocol.go:13
    // ReadMessage reads the next message from the connection.
    fn read_message(&self) -> Result<Message, Error>;
    // protocol.go:15
    // WriteRequest writes a request message.
    fn write_request(&self, id: &ID, method: &str, params: Option<&Value>) -> Result<(), Error>;
    // protocol.go:17
    // WriteNotification writes a notification message (no ID).
    fn write_notification(&self, method: &str, params: Option<&Value>) -> Result<(), Error>;
    // protocol.go:19
    // WriteResponse writes a successful response.
    fn write_response(&self, id: &ID, result: &Value) -> Result<(), Error>;
    // protocol.go:21
    // WriteError writes an error response.
    fn write_error(&self, id: &ID, err: &ResponseError) -> Result<(), Error>;
}
