// Port of tsc/internal/ipc/transport.go and the connection setup in tsc/internal/api/server.go.
// Run on stdio or a private Unix socket directory / Windows named pipe (`--pipe <path>`),
// MessagePack + SyncConn by default, JSON-RPC + AsyncConn with `--async`.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::{Arc, OnceLock};

use crate::conn_async::AsyncConn;
use crate::conn_sync::{ConnOptions, SyncConn};
use crate::handler::{Caller, CancellationToken, Handler};
use crate::jsonrpc::{FrameReader, FrameWriter};
use crate::message::{DEFAULT_MAX_FRAME_BYTES, TransportError};
use crate::msgpack::{MessagePackReader, MessagePackWriter};
use crate::protocol::WireProtocol;

#[cfg(unix)]
#[path = "transport_unix.rs"]
pub mod pipe_os;
#[cfg(windows)]
#[path = "transport_windows.rs"]
pub mod pipe_os;
#[cfg(not(any(unix, windows)))]
mod pipe_os {
    use std::io;
    use std::path::{Path, PathBuf};

    pub(super) struct PipeListener {
        path: PathBuf,
    }

    impl PipeListener {
        pub(super) fn bind(path: &Path) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!(
                    "--pipe {}: pipe transport is not supported on this platform",
                    path.display()
                ),
            ))
        }

        pub(super) fn bind_private(name: &str) -> io::Result<Self> {
            Self::bind(Path::new(name))
        }

        pub(super) fn path(&self) -> &Path {
            &self.path
        }

        pub(super) fn accept(&self) -> io::Result<super::Stream> {
            unreachable!("PipeListener cannot be bound on this platform")
        }
    }
}

/// One accepted bidirectional stream.
pub struct Stream {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    /// Shuts the stream down so a blocked reader wakes (sockets). None for stdio.
    pub closer: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Stream {
    /// `StdioTransport`. Stdout is the wire: nothing else in the process may write to it.
    fn stdio() -> Stream {
        Stream {
            reader: Box::new(std::io::stdin()),
            writer: Box::new(std::io::stdout()),
            closer: None,
        }
    }
}

/// `PipeTransport`: listens on a private Unix socket or Windows named pipe and accepts one connection.
pub struct PipeListener {
    inner: pipe_os::PipeListener,
}

impl PipeListener {
    pub fn bind(path: &Path) -> std::io::Result<PipeListener> {
        pipe_os::PipeListener::bind(path).map(|inner| PipeListener { inner })
    }

    /// Creates a private socket path in the system temporary directory on Unix, or a named pipe
    /// under `\\.\pipe\` on Windows.
    pub fn bind_private(name: &str) -> std::io::Result<PipeListener> {
        pipe_os::PipeListener::bind_private(name).map(|inner| PipeListener { inner })
    }

    pub fn path(&self) -> &Path {
        self.inner.path()
    }

    pub fn accept(&self) -> std::io::Result<Stream> {
        self.inner.accept()
    }
}

/// A caller slot filled once the connection exists (Go: `SetConnection` after Accept), so the session and
/// callback filesystem can be built before the connection that owns them.
#[derive(Default)]
pub struct LateCaller(OnceLock<Arc<dyn Caller>>);

impl LateCaller {
    pub fn new() -> Arc<LateCaller> {
        Arc::new(LateCaller::default())
    }

    pub fn set(&self, caller: Arc<dyn Caller>) {
        let _ = self.0.set(caller);
    }
}

impl Caller for LateCaller {
    fn call(&self, method: &str, params: Option<&[u8]>) -> Result<Vec<u8>, TransportError> {
        match self.0.get() {
            Some(c) => c.call(method, params),
            None => {
                Err(TransportError::Closed(Some(format!("{method} called before connection set"))))
            }
        }
    }

    fn notify(&self, method: &str, params: Option<&[u8]>) -> Result<(), TransportError> {
        match self.0.get() {
            Some(c) => c.notify(method, params),
            None => {
                Err(TransportError::Closed(Some(format!("{method} called before connection set"))))
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ServeOptions {
    protocol: WireProtocol,
    pub collect_timing: bool,
    max_frame_bytes: u64,
    /// See `ConnOptions::reentrancy_grace`.
    reentrancy_grace: std::time::Duration,
    /// See `ConnOptions::shutdown`. The CLI cancels it on SIGINT/SIGTERM (Go signal.NotifyContext).
    pub shutdown: CancellationToken,
}

impl ServeOptions {
    /// `--api` (sync, `MessagePack`) or `--api --async` (JSON-RPC).
    pub fn new(is_async: bool) -> ServeOptions {
        ServeOptions {
            protocol: if is_async { WireProtocol::JsonRpc } else { WireProtocol::MessagePack },
            collect_timing: false,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            reentrancy_grace: crate::reentrancy::DEFAULT_ASYNC_GRACE,
            shutdown: CancellationToken::new(),
        }
    }
}

/// A connection ready to run.
pub enum Connection {
    Sync(Arc<SyncConn>),
    Async(Arc<AsyncConn>),
}

impl Connection {
    /// Builds the protocol + connection for `stream` (server.go Run), and points `late` at it.
    pub fn new(
        stream: Stream,
        handler: Arc<dyn Handler>,
        options: ServeOptions,
        late: Option<&LateCaller>,
    ) -> Connection {
        let conn_options = ConnOptions {
            collect_timing: options.collect_timing,
            reentrancy_grace: options.reentrancy_grace,
            shutdown: options.shutdown,
        };
        let conn = match options.protocol {
            WireProtocol::MessagePack => Connection::Sync(SyncConn::new(
                Box::new(MessagePackReader::with_limit(stream.reader, options.max_frame_bytes)),
                Box::new(MessagePackWriter::new(stream.writer)),
                handler,
                conn_options,
            )),
            WireProtocol::JsonRpc => Connection::Async(AsyncConn::new(
                Box::new(FrameReader::with_limit(stream.reader, options.max_frame_bytes)),
                Box::new(FrameWriter::new(stream.writer)),
                handler,
                conn_options,
                stream.closer,
            )),
        };
        if let Some(late) = late {
            late.set(conn.caller());
        }
        conn
    }

    pub fn caller(&self) -> Arc<dyn Caller> {
        match self {
            Connection::Sync(c) => c.caller(),
            Connection::Async(c) => c.caller(),
        }
    }

    /// Blocks until the client disconnects (Ok) or the connection fails.
    pub fn run(&self) -> Result<(), TransportError> {
        match self {
            Connection::Sync(c) => c.run(),
            Connection::Async(c) => c.run(),
        }
    }
}

/// Convenience for the CLI: accept on stdio or `pipe_path`, then run until the client disconnects.
/// `make_handler` receives the caller for filesystem callbacks before the connection exists.
pub fn serve(
    pipe_path: Option<&Path>,
    options: ServeOptions,
    make_handler: impl FnOnce(Arc<dyn Caller>) -> Arc<dyn Handler>,
) -> Result<(), TransportError> {
    let late = LateCaller::new();
    let handler = make_handler(Arc::<LateCaller>::clone(&late));
    let (_listener, stream) = match pipe_path {
        Some(path) => {
            let listener = PipeListener::bind(path).map_err(|e| {
                TransportError::Io(std::io::Error::new(
                    e.kind(),
                    format!("failed to create pipe transport: {e}"),
                ))
            })?;
            let stream = listener.accept().map_err(|e| {
                TransportError::Io(std::io::Error::new(
                    e.kind(),
                    format!("failed to accept connection: {e}"),
                ))
            })?;
            (Some(listener), stream)
        }
        None => (None, Stream::stdio()),
    };
    let shutdown = options.shutdown.clone();
    let conn = Connection::new(stream, handler, options, Some(&late));
    let result = conn.run();
    // serverRunError: once the server context is cancelled, how the connection ended is not an error.
    if shutdown.is_cancelled() {
        return Ok(());
    }
    result
}
