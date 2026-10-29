use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rustc_hash::FxHashMap;
use tsrs_core::json::{JsonMap, Value};

use crate::jsonrpc::{self, CODE_INTERNAL_ERROR, ID, ResponseError};
use crate::{
    Closer, Conn, ERR_CONN_CLOSED, Error, ErrorTag, Handler, Message, Protocol, ReadWriteCloser,
    lock, new_jsonrpc_protocol,
};

// timing.go:11
// Method names for the generic connection-level timing feature, handled by the connection itself rather
// than the handler.
pub const METHOD_GET_SERVER_TIMING: &str = "getServerTiming";
pub const METHOD_RESET_SERVER_TIMING: &str = "resetServerTiming";

// conn_async.go:20
// AsyncConn manages bidirectional JSON-RPC communication with async request handling.
// Each incoming request is handled in its own goroutine, allowing concurrent processing.
// This is the standard implementation for LSP-style JSON-RPC protocols.
// Go's per-request timing (SetCollectTiming, timing.go) is not ported: the connection behaves as Go's does with
// timing disabled, the only state the content mapper host uses.
pub struct AsyncConn {
    rwc: Option<Arc<dyn Closer>>,
    protocol: Box<dyn Protocol>,
    handler: Arc<dyn Handler>,

    // For server→client requests
    seq: AtomicU64,
    pending_mu: Mutex<PendingCalls>,
    write_mu: Mutex<()>,
    // Go's `handlers sync.WaitGroup`: Run starts the handler threads in a std::thread::scope, which waits for them.
}

// The state Go guards with pendingMu.
struct PendingCalls {
    pending: FxHashMap<ID, SyncSender<Message>>,
    terminal: Option<Error>,
    has_cause: bool,
}

// conn_async.go:41
// NewAsyncConn creates a new async connection with the given transport and handler.
// It uses JSONRPCProtocol (LSP-style Content-Length framing) by default.
pub fn new_async_conn(rwc: ReadWriteCloser, handler: Arc<dyn Handler>) -> Arc<AsyncConn> {
    let ReadWriteCloser { reader, writer, closer } = rwc;
    new_async_conn_with_protocol(
        Some(closer),
        Box::new(new_jsonrpc_protocol(reader, writer)),
        handler,
    )
}

// conn_async.go:46
// NewAsyncConnWithProtocol creates a new async connection with a custom protocol.
// The connection only closes its transport (when it fails to write a response, so that the read loop stops), so it
// takes the transport's closer; None is Go's nil transport.
pub fn new_async_conn_with_protocol(
    rwc: Option<Arc<dyn Closer>>,
    protocol: Box<dyn Protocol>,
    handler: Arc<dyn Handler>,
) -> Arc<AsyncConn> {
    Arc::new(AsyncConn {
        rwc,
        protocol,
        handler,
        seq: AtomicU64::new(0),
        pending_mu: Mutex::new(PendingCalls {
            pending: FxHashMap::default(),
            terminal: None,
            has_cause: false,
        }),
        write_mu: Mutex::new(()),
    })
}

impl Conn for AsyncConn {
    // conn_async.go:68
    // Run starts processing messages on the connection.
    // It blocks until an error occurs (or, in Go, the context is cancelled; the port has no context).
    fn run(&self) -> Result<(), Error> {
        let request_errors: Mutex<Option<Error>> = Mutex::new(None);
        let err = thread::scope(|s| {
            let read = catch_unwind(AssertUnwindSafe(|| {
                loop {
                    let msg = match self.protocol.read_message() {
                        Ok(msg) => msg,
                        Err(err) => {
                            if err.is(ErrorTag::EOF) {
                                break None;
                            }
                            break Some(err);
                        }
                    };

                    if msg.is_response() {
                        self.handle_response(msg);
                    } else if msg.is_request() {
                        let request_errors = &request_errors;
                        let started = thread::Builder::new()
                            .name(HANDLER_THREAD.to_string())
                            .spawn_scoped(s, move || {
                                if let Err(request_err) = self.handle_request(&msg)
                                    && self.record_request_error(request_err, request_errors)
                                    && let Some(rwc) = &self.rwc
                                {
                                    let _ = rwc.close();
                                }
                            });
                        if let Err(err) = started {
                            break Some(handler_thread_error(&err));
                        }
                    } else if msg.is_notification() {
                        let started = thread::Builder::new()
                            .name(HANDLER_THREAD.to_string())
                            .spawn_scoped(s, move || self.handle_notification(&msg));
                        if let Err(err) = started {
                            break Some(handler_thread_error(&err));
                        }
                    }
                }
            }));
            // conn_async.go:71, deferred: closePendingCalls, then (with no handler context to cancel) the end of the
            // scope waits for the handlers. Go's defer also runs when Run panics, so the waiting calls fail then too
            // instead of waiting forever.
            let err = match read {
                Ok(err) => err,
                Err(payload) => {
                    self.close_pending_calls(None);
                    std::panic::resume_unwind(payload);
                }
            };
            self.close_pending_calls(err.as_ref());
            err
        });
        match (err, request_errors.into_inner().unwrap_or_else(PoisonError::into_inner)) {
            (None, None) => Ok(()),
            (Some(err), None) | (None, Some(err)) => Err(err),
            (Some(err), Some(request_err)) => Err(Error::join(&err, &request_err)),
        }
    }

    // conn_async.go:256
    // Call sends a request to the client and waits for a response.
    fn call(&self, method: &str, params: Option<&Value>) -> Result<Option<Value>, Error> {
        self.call_until(method, params, None)
    }

    fn call_with_timeout(
        &self,
        method: &str,
        params: Option<&Value>,
        timeout: Duration,
    ) -> Result<Option<Value>, Error> {
        self.call_until(method, params, Instant::now().checked_add(timeout))
    }

    // conn_async.go:307
    // Notify sends a notification to the client (no response expected).
    fn notify(&self, method: &str, params: Option<&Value>) -> Result<(), Error> {
        {
            let p = lock(&self.pending_mu);
            if let Some(terminal) = &p.terminal {
                return Err(terminal.clone());
            }
        }
        let _write = lock(&self.write_mu);
        self.protocol.write_notification(method, params)
    }
}

// Each incoming request and notification runs on its own thread, as Go runs it on its own goroutine.
const HANDLER_THREAD: &str = "ipc-handler";

// Go starts goroutines unconditionally; a thread can fail to start (resources, or wasm32-wasip1), which ends the
// read loop like a read error.
fn handler_thread_error(err: &std::io::Error) -> Error {
    Error::new(format!("ipc: failed to start a handler thread: {err}"))
}

impl AsyncConn {
    // conn_async.go:116
    // closePendingCalls records that the read loop has exited and unblocks requests waiting for a response.
    fn close_pending_calls(&self, run_err: Option<&Error>) {
        let mut p = lock(&self.pending_mu);
        p.record_terminal_error_locked(run_err);
        p.close_pending_calls_locked();
    }

    // conn_async.go:123
    fn record_request_error(
        &self,
        request_err: Error,
        request_errors: &Mutex<Option<Error>>,
    ) -> bool {
        let mut p = lock(&self.pending_mu);
        if !p.record_terminal_error_locked(Some(&request_err)) {
            return false;
        }
        *lock(request_errors) = Some(request_err);
        p.close_pending_calls_locked();
        true
    }

    // conn_async.go:158
    // handleResponse matches a response to a pending request.
    fn handle_response(&self, msg: Message) {
        let ch = {
            let Some(id) = &msg.id else {
                return;
            };
            lock(&self.pending_mu).pending.remove(id)
        };

        if let Some(ch) = ch {
            // The channel holds one message and gets only this one, so the send never blocks. It fails when the
            // caller has stopped waiting (call_with_timeout), and the response is dropped.
            let _ = ch.send(msg);
        }
    }

    // conn_async.go:173
    // handleRequest processes an incoming request.
    fn handle_request(&self, msg: &Message) -> Result<(), Error> {
        let id = msg.id.as_ref().expect("a request has an id");

        // Intercept the meta-requests for collected server timing before dispatching
        // to the handler, so they are answered directly and not themselves recorded.
        match msg.method.as_str() {
            METHOD_GET_SERVER_TIMING => {
                let write_err = {
                    let _write = lock(&self.write_mu);
                    self.protocol.write_response(id, &disabled_server_timing_info())
                };
                return write_err.map_err(|err| {
                    Error::wrap("ipc: failed to write server timing response: ", err)
                });
            }
            METHOD_RESET_SERVER_TIMING => {
                // c.timing is nil (timing collection is not ported): there is nothing to reset.
                let write_err = {
                    let _write = lock(&self.write_mu);
                    self.protocol.write_response(id, &Value::Null)
                };
                return write_err.map_err(|err| {
                    Error::wrap("ipc: failed to write reset server timing response: ", err)
                });
            }
            _ => {}
        }

        // Recover from panics and convert to error response. Go appends the goroutine's stack to the message; the
        // stack has unwound by the time Rust catches the panic, so only the panic value is reported.
        let handled = catch_unwind(AssertUnwindSafe(|| {
            let result = self.handler.handle_request(&msg.method, msg.params.as_ref());

            let _write = lock(&self.write_mu);
            let write_err = match result {
                Err(err) => self.protocol.write_error(id, &internal_error(err.to_string())),
                Ok(result) => self.protocol.write_response(id, &result),
            };
            write_err.map_err(|err| Error::wrap("ipc: failed to write response: ", err))
        }));
        let payload = match handled {
            Ok(ret) => return ret,
            Err(payload) => payload,
        };
        let r = panic_value(payload.as_ref());
        let write_err = {
            let _write = lock(&self.write_mu);
            self.protocol.write_error(id, &internal_error(format!("panic: {r}")))
        };
        write_err.map_err(|err| {
            Error::wrap("ipc: failed to write panic error response: ", err)
                .with_suffix(&format!(" (original panic: {r})"))
        })
    }

    // conn_async.go:251
    // handleNotification processes an incoming notification.
    fn handle_notification(&self, msg: &Message) {
        let _ = self.handler.handle_notification(&msg.method, msg.params.as_ref());
    }

    // conn_async.go:256, Call with a context whose deadline is `deadline` (None: no deadline).
    fn call_until(
        &self,
        method: &str,
        params: Option<&Value>,
        deadline: Option<Instant>,
    ) -> Result<Option<Value>, Error> {
        // Create unique request ID
        let id =
            jsonrpc::new_id_string(&format!("api{}", self.seq.fetch_add(1, Ordering::SeqCst) + 1));

        // Register response channel BEFORE sending request to avoid race
        let (response_chan, responses) = mpsc::sync_channel(1);
        {
            let mut p = lock(&self.pending_mu);
            if let Some(terminal) = &p.terminal {
                return Err(terminal.clone());
            }
            p.pending.insert(id.clone(), response_chan);
        }

        let result = self.send_and_wait(&id, method, params, &responses, deadline);

        // conn_async.go:271, deferred: unregister the channel if no response or terminal error took it.
        lock(&self.pending_mu).pending.remove(&id);
        result
    }

    // conn_async.go:280, the rest of Call: send the request and wait for its response.
    fn send_and_wait(
        &self,
        id: &ID,
        method: &str,
        params: Option<&Value>,
        responses: &Receiver<Message>,
        deadline: Option<Instant>,
    ) -> Result<Option<Value>, Error> {
        // Send the request
        {
            let _write = lock(&self.write_mu);
            self.protocol.write_request(id, method, params)?;
        }

        let resp = match deadline {
            None => responses.recv().ok(),
            Some(deadline) => {
                match responses.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                    Ok(resp) => Some(resp),
                    // case <-ctx.Done(): return nil, ctx.Err()
                    Err(RecvTimeoutError::Timeout) => {
                        return Err(Error::tagged(
                            ErrorTag::DeadlineExceeded,
                            "context deadline exceeded",
                        ));
                    }
                    Err(RecvTimeoutError::Disconnected) => None,
                }
            }
        };
        let Some(resp) = resp else {
            // The channel was closed: the read loop has exited and recorded the terminal error.
            let terminal = lock(&self.pending_mu).terminal.clone();
            return Err(terminal.unwrap_or_else(conn_closed));
        };
        if let Some(err) = resp.error {
            return Err(Error::new(format!("ipc: remote error [{}]: {}", err.code, err.message)));
        }
        Ok(resp.result)
    }
}

impl PendingCalls {
    // conn_async.go:134
    fn record_terminal_error_locked(&mut self, terminal_err: Option<&Error>) -> bool {
        if self.terminal.is_none() {
            let closed = conn_closed();
            if let Some(terminal_err) = terminal_err {
                self.terminal = Some(Error::join(&closed, terminal_err));
                self.has_cause = true;
                return true;
            }
            self.terminal = Some(closed);
        } else if !self.has_cause
            && let (Some(terminal), Some(terminal_err)) = (&self.terminal, terminal_err)
        {
            self.terminal = Some(Error::join(terminal, terminal_err));
            self.has_cause = true;
            return true;
        }
        false
    }

    // conn_async.go:150
    fn close_pending_calls_locked(&mut self) {
        // Dropping a waiter's sender is Go's close(ch): its receive reports the channel closed.
        self.pending.clear();
    }
}

// ErrConnClosed (conn.go:11).
fn conn_closed() -> Error {
    Error::tagged(ErrorTag::ConnClosed, ERR_CONN_CLOSED)
}

fn internal_error(message: String) -> ResponseError {
    ResponseError { code: CODE_INTERNAL_ERROR, message, data: None }
}

// fmt's %v of a recovered panic value.
fn panic_value(payload: &(dyn Any + Send)) -> &str {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return s;
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s;
    }
    "Box<dyn Any>"
}

// timing.go:119 serverTimingSnapshot(c.timing), with the nil collector the port always has: timing.go:128
// disabledServerTimingInfo, serverTimingInfo{Enabled: false, RecentRequests: []serverRequestTiming{}}.
fn disabled_server_timing_info() -> Value {
    let mut totals = JsonMap::new();
    totals.insert("requestCount".to_string(), Value::Number(0.0));
    totals.insert("totalProcessingTimeMs".to_string(), Value::Number(0.0));
    let mut info = JsonMap::new();
    info.insert("enabled".to_string(), Value::Bool(false));
    info.insert("totals".to_string(), Value::Object(totals));
    info.insert("recentRequests".to_string(), Value::Array(Vec::new()));
    Value::Object(info)
}
