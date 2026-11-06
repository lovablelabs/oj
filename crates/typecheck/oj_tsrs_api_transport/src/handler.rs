// Port of the ipc.Handler / ipc.Conn interfaces (tsc/internal/ipc/conn.go) as the contract between
// this transport crate and the API session (owned by the integration lead).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::message::{ApiError, Response, TransportError};

/// Cancellation shared by every request on one connection.
///
/// It is set when the connection's Run loop exits (EOF, protocol error, or a fatal write error), mirroring
/// the cancelled handler context in ipc.AsyncConn.Run. Long-running handlers should poll it between units of
/// work.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    /// Go's parent context: cancelling it cancels this token, but not the other way round.
    parent: Option<Arc<CancellationToken>>,
}

impl CancellationToken {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// context.WithCancel(parent).
    #[must_use]
    pub(crate) fn child(self) -> Self {
        CancellationToken { cancelled: Arc::default(), parent: Some(Arc::new(self)) }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
            || self.parent.as_ref().is_some_and(|p| p.is_cancelled())
    }
}

/// Per-request context passed to the handler.
#[derive(Clone, Debug)]
pub struct RequestContext {
    pub cancel: CancellationToken,
    /// 0 for a request read by the main loop. In sync (`MessagePack`) mode a client may issue an API request
    /// from inside a filesystem callback; that nested request is handled on the thread that is blocked in
    /// `Caller::call`, with depth >= 1. A handler that holds a non-reentrant resource (e.g. an exclusive
    /// checker lease) across a callback must reject nested requests that need the same resource instead of
    /// blocking on it.
    pub depth: u32,
    /// Connection-wide callback state (how many requests are blocked on the client).
    pub(crate) callbacks: std::sync::Arc<crate::reentrancy::CallbackState>,
    /// This request's own re-entrancy state (attributed client calls, what it waits for).
    pub(crate) state: std::sync::Arc<crate::reentrancy::RequestState>,
}

/// The API session. Implementations must be thread-safe: the async connection runs each request on
/// its own thread (ipc.AsyncConn spawns a goroutine per request).
pub trait Handler: Send + Sync + 'static {
    /// `params` is the raw request payload: JSON text, or empty when the client sent no params (JSON-RPC
    /// `params` omitted / `MessagePack` empty bin). Return `Response::Binary` only for raw binary results
    /// (source-file responses when binary responses are enabled, and `echo`).
    fn handle_request(
        &self,
        cx: &RequestContext,
        method: &str,
        params: &[u8],
    ) -> Result<Response, ApiError>;

    /// Notifications are accepted and ignored by the pinned session (Session.HandleNotification).
    fn handle_notification(&self, _cx: &RequestContext, _method: &str, _params: &[u8]) {}
}

impl<T: Handler + ?Sized> Handler for Arc<T> {
    fn handle_request(
        &self,
        cx: &RequestContext,
        method: &str,
        params: &[u8],
    ) -> Result<Response, ApiError> {
        (**self).handle_request(cx, method, params)
    }

    fn handle_notification(&self, cx: &RequestContext, method: &str, params: &[u8]) {
        (**self).handle_notification(cx, method, params);
    }
}

/// Server-to-client calls (ipc.Conn.Call / Notify).
///
/// Obtained from a connection before it runs and safe to use from any thread while a request is in flight.
/// Calls never hold a lock that the connection's handler dispatch needs, so a client callback may issue
/// nested API requests.
pub trait Caller: Send + Sync {
    /// Sends `method` with JSON `params` (None = Go nil, marshalled as `null` / omitted) and blocks
    /// until the client answers. Returns the raw result payload (JSON text).
    fn call(&self, method: &str, params: Option<&[u8]>) -> Result<Vec<u8>, TransportError>;

    fn notify(&self, method: &str, params: Option<&[u8]>) -> Result<(), TransportError>;
}

/// Renders a caught panic payload the way ipc.handleRequest does (`panic: <value>\n<stack>`).
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    let value = if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "<non-string panic payload>".to_string()
    };
    // Go appends debug.Stack(); a backtrace taken here would show the catch site, not the panic
    // site, so only the value is reported (the default panic hook already wrote it to stderr).
    format!("panic: {value}")
}
