// Port of tsc/internal/ipc/conn_sync.go: requests are handled one at a time on the Run thread;
// server-to-client calls are serialized and read their response inline. While a call waits, a
// nested client request (issued from inside a client callback) is handled with the protocol lock
// released, so callbacks can re-enter the API without deadlocking.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::time::Instant;

use crate::handler::{Caller, CancellationToken, Handler, RequestContext, panic_message};
use crate::message::{Message, Response, ResponseError, TransportError};
use crate::protocol::{ProtocolReader, ProtocolWriter};
use crate::timing::{
    METHOD_GET_SERVER_TIMING, METHOD_RESET_SERVER_TIMING, TimingCollector, server_timing_snapshot,
};

struct SyncIo {
    reader: Box<dyn ProtocolReader>,
    writer: Box<dyn ProtocolWriter>,
    calls: u32,
    reading: bool,
}

#[derive(Clone, Debug)]
pub struct ConnOptions {
    /// `SetCollectTiming`: answer getServerTiming/resetServerTiming with collected data.
    pub collect_timing: bool,
    /// Async connections only: how long a contended re-entrancy-aware acquisition waits while some
    /// request on the connection is blocked on the client before it fails (see `reentrancy`). Sync
    /// connections fail such acquisitions immediately (the conflict is provably a re-entry there).
    pub reentrancy_grace: std::time::Duration,
    /// The context passed to Run: once it is cancelled, the run loop stops before reading the next
    /// message. The connection's own token is a child of it.
    pub shutdown: CancellationToken,
}

impl Default for ConnOptions {
    fn default() -> Self {
        ConnOptions {
            collect_timing: false,
            reentrancy_grace: crate::reentrancy::DEFAULT_ASYNC_GRACE,
            shutdown: CancellationToken::new(),
        }
    }
}

pub struct SyncConn {
    io: Mutex<SyncIo>,
    turn: Condvar,
    handler: Arc<dyn Handler>,
    timing: Option<TimingCollector>,
    cancel: CancellationToken,
    callbacks: Arc<crate::reentrancy::CallbackState>,
}

impl SyncConn {
    pub fn new(
        reader: Box<dyn ProtocolReader>,
        writer: Box<dyn ProtocolWriter>,
        handler: Arc<dyn Handler>,
        options: ConnOptions,
    ) -> Arc<SyncConn> {
        Arc::new(SyncConn {
            io: Mutex::new(SyncIo { reader, writer, calls: 0, reading: false }),
            turn: Condvar::new(),
            handler,
            timing: options.collect_timing.then(TimingCollector::default),
            cancel: options.shutdown.child(),
            callbacks: Arc::new(crate::reentrancy::CallbackState::new(
                true,
                options.reentrancy_grace,
            )),
        })
    }

    /// A caller for server-to-client calls (filesystem callbacks). Holds only a weak reference, so
    /// handlers that keep it do not keep the connection alive.
    pub fn caller(self: &Arc<Self>) -> Arc<dyn Caller> {
        Arc::new(SyncCaller(Arc::downgrade(self)))
    }

    fn lock(&self) -> MutexGuard<'_, SyncIo> {
        self.io.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_turn(&self, depth: u32) -> MutexGuard<'_, SyncIo> {
        let mut io = self.lock();
        while io.calls != depth {
            io = self.turn.wait(io).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        io.reading = depth > 0;
        io
    }

    /// Run: processes messages until EOF (Ok) or a fatal error. Always cancels the token on exit.
    pub fn run(&self) -> Result<(), TransportError> {
        let result = self.run_loop();
        self.cancel.cancel();
        result
    }

    fn run_loop(&self) -> Result<(), TransportError> {
        loop {
            if self.cancel.is_cancelled() {
                return Ok(());
            }
            let msg = {
                let mut io = self.lock();
                io.reader.read_message()
            };
            let msg = match msg {
                Ok(msg) => msg,
                Err(TransportError::Eof) => return Ok(()),
                Err(e) => return Err(e),
            };
            if msg.is_request() {
                self.handle_request(&msg, 0)?;
            } else if msg.is_notification() {
                self.handle_notification(&msg, 0);
            } else {
                return Err(TransportError::Unexpected(
                    "ipc: unexpected response message in sync connection".to_string(),
                ));
            }
        }
    }

    fn write_response(
        &self,
        msg: &Message,
        result: &Result<Response, ResponseError>,
        depth: u32,
    ) -> Result<(), TransportError> {
        let id = msg.id.as_ref().expect("requests have an id");
        let mut io = self.lock_turn(depth);
        let written = match result {
            Ok(response) => match io.writer.write_response(id, response) {
                // An unencodable result (invalid JSON from the handler) becomes an error response
                // instead of corrupting the stream.
                Err(TransportError::Protocol(e)) => {
                    io.writer.write_error(id, &ResponseError::internal(e))
                }
                other => other,
            },
            Err(err) => io.writer.write_error(id, err),
        };
        written.map_err(|e| {
            TransportError::Io(std::io::Error::other(format!("ipc: failed to write response: {e}")))
        })
    }

    fn handle_request(&self, msg: &Message, depth: u32) -> Result<(), TransportError> {
        match msg.method.as_str() {
            METHOD_GET_SERVER_TIMING => {
                let snapshot = server_timing_snapshot(self.timing.as_ref());
                let json = serde_json::to_vec(&snapshot).expect("timing serializes");
                return self.write_response(msg, &Ok(Response::Json(json)), depth);
            }
            METHOD_RESET_SERVER_TIMING => {
                if let Some(t) = &self.timing {
                    t.reset();
                }
                return self.write_response(msg, &Ok(Response::null()), depth);
            }
            _ => {}
        }
        let start = self.timing.as_ref().map(|_| Instant::now());
        let cx = RequestContext {
            cancel: self.cancel.clone(),
            depth,
            callbacks: Arc::clone(&self.callbacks),
            state: std::sync::Arc::default(),
        };
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            crate::reentrancy::with_request(&cx, || {
                self.handler.handle_request(&cx, &msg.method, msg.params_bytes())
            })
        }));
        if let (Some(t), Some(start)) = (&self.timing, start) {
            t.record(&msg.method, start.elapsed());
        }
        let result = match outcome {
            Ok(Ok(response)) => Ok(response),
            Ok(Err(err)) => Err(ResponseError { code: err.code, message: err.message }),
            Err(payload) => Err(ResponseError::internal(panic_message(&*payload))),
        };
        self.write_response(msg, &result, depth)
    }

    fn handle_notification(&self, msg: &Message, depth: u32) {
        let cx = RequestContext {
            cancel: self.cancel.clone(),
            depth,
            callbacks: Arc::clone(&self.callbacks),
            state: std::sync::Arc::default(),
        };
        let _ = catch_unwind(AssertUnwindSafe(|| {
            self.handler.handle_notification(&cx, &msg.method, msg.params_bytes());
        }));
    }

    /// Call: serialized; the msgpack protocol uses the method name as the response ID.
    pub fn call(&self, method: &str, params: Option<&[u8]>) -> Result<Vec<u8>, TransportError> {
        let mut io = self.lock();
        while io.reading {
            io = self.turn.wait(io).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        if self.cancel.is_cancelled() {
            return Err(TransportError::Closed(None));
        }
        io.calls += 1;
        io.reading = true;
        let depth = io.calls;
        let _active = ActiveCall(self);
        (|| {
            let id = crate::message::Id::string(method);
            io.writer.write_request(&id, method, params)?;
            let _waiting = self.callbacks.enter();
            loop {
                let msg = match io.reader.read_message() {
                    Ok(msg) => msg,
                    Err(TransportError::Eof) => {
                        return Err(TransportError::Closed(Some(format!(
                            "EOF while waiting for {method:?} response"
                        ))));
                    }
                    Err(e) => return Err(e),
                };
                if msg.is_response() && msg.id.as_ref().is_some_and(|id| id.text() == method) {
                    if let Some(err) = msg.error {
                        return Err(TransportError::Remote(err));
                    }
                    return Ok(msg.result.unwrap_or_default());
                }
                if msg.is_request() {
                    io.reading = false;
                    self.turn.notify_all();
                    drop(io);
                    let handled = self.handle_request(&msg, depth);
                    io = self.lock();
                    handled?;
                    continue;
                }
                if msg.is_notification() {
                    io.reading = false;
                    self.turn.notify_all();
                    drop(io);
                    self.handle_notification(&msg, depth);
                    io = self.lock_turn(depth);
                    continue;
                }
                return Err(TransportError::Unexpected(format!(
                    "ipc: unexpected message while waiting for {method:?} response"
                )));
            }
        })()
    }

    fn notify(&self, method: &str, params: Option<&[u8]>) -> Result<(), TransportError> {
        let mut io = self.lock();
        io.writer.write_notification(method, params)
    }
}

struct ActiveCall<'a>(&'a SyncConn);

impl Drop for ActiveCall<'_> {
    fn drop(&mut self) {
        let mut io = self.0.lock();
        io.calls -= 1;
        io.reading = false;
        self.0.turn.notify_all();
    }
}

struct SyncCaller(Weak<SyncConn>);

impl Caller for SyncCaller {
    fn call(&self, method: &str, params: Option<&[u8]>) -> Result<Vec<u8>, TransportError> {
        self.0.upgrade().ok_or(TransportError::Closed(None))?.call(method, params)
    }

    fn notify(&self, method: &str, params: Option<&[u8]>) -> Result<(), TransportError> {
        self.0.upgrade().ok_or(TransportError::Closed(None))?.notify(method, params)
    }
}
