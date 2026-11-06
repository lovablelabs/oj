use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;

#[cfg(not(unix))]
pub use tcp_pipe as pipe;

// Go io.ReadWriteCloser, the stream a connection runs over. A content mapper's comes from its Spawner
// (hostimpl.go:349, cmd/tsc/sys.go spawnProcess): Read is the child's stdout, Write its stdin, and Close tears the
// process down. Go hands the one value to both the protocol and the connection; Rust ownership needs the parts
// apart: the protocol takes the reader (read only by the connection's read loop) and the writer (written under the
// connection's write lock), and the closer is shared by the connection, the thread that runs it, and the host.
pub struct ReadWriteCloser {
    pub reader: Box<dyn Read + Send>,
    pub writer: Box<dyn Write + Send>,
    pub closer: Arc<dyn Closer>,
}

// Go io.Closer, plus the `ExitCode() (int, bool)` method that hostimpl.go:353 (processExitState) looks for on a
// stream with a type assertion.
pub trait Closer: Send + Sync {
    // Close tears the stream down. It may be called from any thread, more than once, and while another thread is
    // blocked reading the stream's reader, which must then return: a child process is killed, so its stdout ends;
    // an in-memory pipe is shut down.
    fn close(&self) -> std::io::Result<()>;

    // ExitCode: the process's exit code once it has exited (Go `(code, true)`). None while it runs, and for a stream
    // that is not a process (Go: a stream without the method).
    fn exit_code(&self) -> Option<i32> {
        None
    }
}

// Go net.Pipe, as the ipc tests and contentmappertest/spawner.go use it: two connected in-memory streams, each the
// other's peer. It is a Unix socket pair. `close` shuts its end down in both directions: a read blocked on either
// end returns EOF (Go's own end reports io.ErrClosedPipe instead) and later writes on this end fail. A write on the
// peer fails too on Linux; on macOS it is accepted and dropped until every handle on this end is dropped.
#[cfg(unix)]
pub fn pipe() -> std::io::Result<(ReadWriteCloser, ReadWriteCloser)> {
    let (a, b) = std::os::unix::net::UnixStream::pair()?;
    Ok((socket_read_write_closer(a)?, socket_read_write_closer(b)?))
}

#[cfg(unix)]
fn socket_read_write_closer(
    stream: std::os::unix::net::UnixStream,
) -> std::io::Result<ReadWriteCloser> {
    Ok(ReadWriteCloser {
        reader: Box::new(stream.try_clone()?),
        writer: Box::new(stream.try_clone()?),
        closer: Arc::new(SocketCloser(stream)),
    })
}

#[cfg(unix)]
struct SocketCloser(std::os::unix::net::UnixStream);

#[cfg(unix)]
impl Closer for SocketCloser {
    fn close(&self) -> std::io::Result<()> {
        match self.0.shutdown(std::net::Shutdown::Both) {
            // macOS reports a second shutdown, or one after the peer's, as ENOTCONN; Go's Close is idempotent.
            Err(err) if err.kind() == std::io::ErrorKind::NotConnected => Ok(()),
            result => result,
        }
    }
}

// Windows has no UnixStream::pair. Loopback TCP gives the test spawner the same duplex stream and lets Close
// interrupt a blocked reader, which dropping one of several cloned handles would not do.
pub fn tcp_pipe() -> std::io::Result<(ReadWriteCloser, ReadWriteCloser)> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let client = TcpStream::connect(listener.local_addr()?)?;
    let (server, _) = listener.accept()?;
    Ok((tcp_read_write_closer(client)?, tcp_read_write_closer(server)?))
}

fn tcp_read_write_closer(stream: TcpStream) -> std::io::Result<ReadWriteCloser> {
    stream.set_nodelay(true)?;
    Ok(ReadWriteCloser {
        reader: Box::new(stream.try_clone()?),
        writer: Box::new(stream.try_clone()?),
        closer: Arc::new(TcpCloser(stream)),
    })
}

struct TcpCloser(TcpStream);

impl Closer for TcpCloser {
    fn close(&self) -> std::io::Result<()> {
        match self.0.shutdown(std::net::Shutdown::Both) {
            Err(err) if err.kind() == std::io::ErrorKind::NotConnected => Ok(()),
            result => result,
        }
    }
}
