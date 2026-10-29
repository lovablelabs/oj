// Port of tsc/internal/ipc/conn_async.go: a reader loop dispatches each incoming request to its own
// thread (Go: goroutine), routes responses to pending server-to-client calls by ID, and writes are
// serialized by a write lock. A thread per request (rather than a bounded pool) is deliberate: a
// handler blocked in a client callback must never prevent a nested client request from running.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::Instant;

use rustc_hash::FxHashMap;

use crate::conn_sync::ConnOptions;
use crate::handler::{Caller, CancellationToken, Handler, RequestContext, panic_message};
use crate::message::{Id, Message, Response, ResponseError, TransportError};
use crate::protocol::{ProtocolReader, ProtocolWriter};
use crate::timing::{
    METHOD_GET_SERVER_TIMING, METHOD_RESET_SERVER_TIMING, TimingCollector, server_timing_snapshot,
};

/// Stack size for request handler threads. The checker recurses deeply; the reservation is virtual.
const DEFAULT_HANDLER_STACK_SIZE: usize = 256 << 20;

#[derive(Default)]
struct Pending {
    calls: FxHashMap<Id, mpsc::SyncSender<Message>>,
    /// Some once the connection is terminal; the inner value is the recorded cause, if any.
    #[expect(
        clippy::option_option,
        reason = "Distinguishes an open connection, a normal close, and a close with a recorded error."
    )]
    terminal: Option<Option<String>>,
}

impl Pending {
    /// recordTerminalErrorLocked: returns true when `cause` became the recorded cause.
    fn record_terminal(&mut self, cause: Option<String>) -> bool {
        match &mut self.terminal {
            None => {
                let has = cause.is_some();
                self.terminal = Some(cause);
                has
            }
            Some(existing @ None) if cause.is_some() => {
                *existing = cause;
                true
            }
            Some(_) => false,
        }
    }

    fn terminal_error(&self) -> TransportError {
        TransportError::Closed(self.terminal.clone().flatten())
    }

    fn close_calls(&mut self) {
        // Dropping the senders wakes every waiter with a disconnect.
        self.calls.clear();
    }
}

pub struct AsyncConn {
    reader: Mutex<Option<Box<dyn ProtocolReader>>>,
    writer: Mutex<Box<dyn ProtocolWriter>>,
    handler: Arc<dyn Handler>,
    timing: Option<TimingCollector>,
    cancel: CancellationToken,
    seq: AtomicU64,
    pending: Mutex<Pending>,
    request_error: Mutex<Option<TransportError>>,
    /// Closes the underlying stream (Go: rwc.Close) after a fatal write error so the reader unblocks.
    closer: Option<Box<dyn Fn() + Send + Sync>>,
    handler_stack_size: usize,
    callbacks: Arc<crate::reentrancy::CallbackState>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl AsyncConn {
    pub fn new(
        reader: Box<dyn ProtocolReader>,
        writer: Box<dyn ProtocolWriter>,
        handler: Arc<dyn Handler>,
        options: ConnOptions,
        closer: Option<Box<dyn Fn() + Send + Sync>>,
    ) -> Arc<AsyncConn> {
        Arc::new(AsyncConn {
            reader: Mutex::new(Some(reader)),
            writer: Mutex::new(writer),
            handler,
            timing: options.collect_timing.then(TimingCollector::default),
            cancel: options.shutdown.child(),
            seq: AtomicU64::new(0),
            pending: Mutex::new(Pending::default()),
            request_error: Mutex::new(None),
            closer,
            handler_stack_size: DEFAULT_HANDLER_STACK_SIZE,
            callbacks: Arc::new(crate::reentrancy::CallbackState::new(
                false,
                options.reentrancy_grace,
            )),
        })
    }

    pub fn caller(self: &Arc<Self>) -> Arc<dyn Caller> {
        Arc::new(AsyncCaller(Arc::downgrade(self)))
    }

    /// Run: blocks until EOF (Ok) or a fatal error. On exit, pending calls fail with the terminal
    /// error, the cancellation token is set, and every in-flight handler thread is joined before
    /// returning, so no handler outlives the connection.
    pub fn run(self: &Arc<Self>) -> Result<(), TransportError> {
        let Some(mut reader) = lock(&self.reader).take() else {
            return Err(TransportError::Unexpected(
                "ipc: connection is already running".to_string(),
            ));
        };
        let mut handles: Vec<JoinHandle<()>> = Vec::new();
        let result = loop {
            if self.cancel.is_cancelled() {
                break Ok(());
            }
            if lock(&self.pending).terminal.is_some() {
                // A handler hit a fatal write error.
                break Ok(());
            }
            let msg = match reader.read_message() {
                Ok(msg) => msg,
                Err(TransportError::Eof) => break Ok(()),
                Err(e) => break Err(e),
            };
            handles.retain(|h| !h.is_finished());
            if msg.is_response() {
                self.handle_response(msg);
            } else if msg.is_request() || msg.is_notification() {
                let this = Arc::clone(self);
                let spawned = std::thread::Builder::new()
                    .name(format!("tsrs-api-{}", msg.method))
                    .stack_size(self.handler_stack_size)
                    .spawn(move || this.dispatch(&msg));
                match spawned {
                    Ok(handle) => handles.push(handle),
                    Err(e) => break Err(TransportError::Io(e)),
                }
            }
            // Messages that are none of the three kinds (no id and no method) are ignored, as in Go.
        };
        let cause = result.as_ref().err().map(std::string::ToString::to_string);
        {
            let mut pending = lock(&self.pending);
            pending.record_terminal(cause);
            pending.close_calls();
        }
        self.cancel.cancel();
        for handle in handles {
            let _ = handle.join();
        }
        drop(reader);
        let request_error = lock(&self.request_error).take();
        match (result, request_error) {
            (Ok(()), None) => Ok(()),
            (Err(e), None) | (Ok(()), Some(e)) => Err(e),
            (Err(e), Some(req)) => Err(TransportError::Unexpected(format!("{e}\n{req}"))),
        }
    }

    fn dispatch(&self, msg: &Message) {
        if msg.is_notification() {
            let cx = RequestContext {
                cancel: self.cancel.clone(),
                depth: 0,
                callbacks: Arc::clone(&self.callbacks),
                state: std::sync::Arc::default(),
            };
            let _ = catch_unwind(AssertUnwindSafe(|| {
                self.handler.handle_notification(&cx, &msg.method, msg.params_bytes());
            }));
            return;
        }
        if let Err(e) = self.handle_request(msg) {
            let recorded = {
                let mut pending = lock(&self.pending);
                let recorded = pending.record_terminal(Some(e.to_string()));
                if recorded {
                    pending.close_calls();
                }
                recorded
            };
            if recorded {
                *lock(&self.request_error) = Some(e);
                if let Some(close) = &self.closer {
                    close();
                }
            }
        }
    }

    fn handle_response(&self, msg: Message) {
        let id = msg.id.clone().expect("responses have an id");
        let sender = lock(&self.pending).calls.remove(&id);
        if let Some(sender) = sender {
            let _ = sender.try_send(msg);
        }
    }

    fn write_result(
        &self,
        id: &Id,
        result: &Result<Response, ResponseError>,
    ) -> Result<(), TransportError> {
        let mut w = lock(&self.writer);
        let written = match result {
            Ok(response) => match w.write_response(id, response) {
                Err(TransportError::Protocol(e)) => w.write_error(id, &ResponseError::internal(e)),
                other => other,
            },
            Err(err) => w.write_error(id, err),
        };
        written.map_err(|e| {
            TransportError::Io(std::io::Error::other(format!("ipc: failed to write response: {e}")))
        })
    }

    fn handle_request(&self, msg: &Message) -> Result<(), TransportError> {
        let id = msg.id.as_ref().expect("requests have an id");
        match msg.method.as_str() {
            METHOD_GET_SERVER_TIMING => {
                let json = serde_json::to_vec(&server_timing_snapshot(self.timing.as_ref()))
                    .expect("timing serializes");
                return self.write_result(id, &Ok(Response::Json(json)));
            }
            METHOD_RESET_SERVER_TIMING => {
                if let Some(t) = &self.timing {
                    t.reset();
                }
                return self.write_result(id, &Ok(Response::null()));
            }
            _ => {}
        }
        let start = self.timing.as_ref().map(|_| Instant::now());
        let cx = RequestContext {
            cancel: self.cancel.clone(),
            depth: 0,
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
        self.write_result(id, &result)
    }

    /// Call: unique "api<N>" string IDs; the response channel is registered before the request is
    /// written so a fast response cannot be lost.
    fn call(&self, method: &str, params: Option<&[u8]>) -> Result<Vec<u8>, TransportError> {
        struct Unregister<'a>(&'a AsyncConn, Id);
        impl Drop for Unregister<'_> {
            fn drop(&mut self) {
                lock(&self.0.pending).calls.remove(&self.1);
            }
        }

        let id = Id::string(format!("api{}", self.seq.fetch_add(1, Ordering::SeqCst) + 1));
        let (tx, rx) = mpsc::sync_channel::<Message>(1);
        {
            let mut pending = lock(&self.pending);
            if pending.terminal.is_some() {
                return Err(pending.terminal_error());
            }
            pending.calls.insert(id.clone(), tx);
        }
        let _guard = Unregister(self, id.clone());
        let _waiting = self.callbacks.enter();
        lock(&self.writer).write_request(&id, method, params)?;
        match rx.recv() {
            Ok(resp) => {
                if let Some(err) = resp.error {
                    return Err(TransportError::Remote(err));
                }
                Ok(resp.result.unwrap_or_default())
            }
            Err(_) => Err(lock(&self.pending).terminal_error()),
        }
    }

    fn notify(&self, method: &str, params: Option<&[u8]>) -> Result<(), TransportError> {
        {
            let pending = lock(&self.pending);
            if pending.terminal.is_some() {
                return Err(pending.terminal_error());
            }
        }
        lock(&self.writer).write_notification(method, params)
    }
}

struct AsyncCaller(Weak<AsyncConn>);

impl Caller for AsyncCaller {
    fn call(&self, method: &str, params: Option<&[u8]>) -> Result<Vec<u8>, TransportError> {
        self.0.upgrade().ok_or(TransportError::Closed(None))?.call(method, params)
    }

    fn notify(&self, method: &str, params: Option<&[u8]>) -> Result<(), TransportError> {
        self.0.upgrade().ok_or(TransportError::Closed(None))?.notify(method, params)
    }
}
