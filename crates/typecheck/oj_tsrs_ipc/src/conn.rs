use std::time::Duration;

use tsrs_core::json::Value;

use crate::Error;

// conn.go:11
// The text of ErrConnClosed; an error that wraps it tests true for `is(ErrorTag::ConnClosed)`.
pub const ERR_CONN_CLOSED: &str = "ipc: connection closed";

// conn.go:16
// Handler processes incoming API requests and notifications.
// Go passes each call a context that is canceled when Run exits; the port has no handler context. A None `params`
// is a message without params (Go: an empty json.Value).
pub trait Handler: Send + Sync {
    // conn.go:18
    // HandleRequest handles an incoming request and returns a result or error.
    // Go's nil result is Value::Null.
    fn handle_request(&self, method: &str, params: Option<&Value>) -> Result<Value, Error>;
    // conn.go:20
    // HandleNotification handles an incoming notification.
    fn handle_notification(&self, method: &str, params: Option<&Value>) -> Result<(), Error>;
}

// conn.go:24
// Conn represents a bidirectional connection for API communication.
// Go's Call and Notify take a context; the port has none (the content mapper host closes its connections
// explicitly), and `call_with_timeout` stands in for a call with a `context.WithTimeout` context.
pub trait Conn: Send + Sync {
    // conn.go:27
    // Run starts processing messages on the connection.
    // It blocks until the connection ends and its handlers have returned: Ok when the stream ended (EOF) and every
    // response was written; else the read error and the first failure to write a response, as Go's errors.Join.
    fn run(&self) -> Result<(), Error>;

    // conn.go:30
    // Call sends a request to the client and waits for a response.
    // The result is None when the response has no "result" member (Go: an empty json.Value).
    fn call(&self, method: &str, params: Option<&Value>) -> Result<Option<Value>, Error>;

    // Call with a `context.WithTimeout(ctx, timeout)` context (hostimpl.go:526, the initialize request): it stops
    // waiting for the response after `timeout` with the error Go's Call returns then, ctx.Err(), which tests true
    // for `is(ErrorTag::DeadlineExceeded)` and reads "context deadline exceeded". Writing the request is not timed,
    // as in Go.
    fn call_with_timeout(
        &self,
        method: &str,
        params: Option<&Value>,
        timeout: Duration,
    ) -> Result<Option<Value>, Error>;

    // conn.go:33
    // Notify sends a notification to the client (no response expected).
    fn notify(&self, method: &str, params: Option<&Value>) -> Result<(), Error>;
}
