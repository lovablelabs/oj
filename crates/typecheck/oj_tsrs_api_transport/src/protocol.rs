// Port of tsc/internal/ipc/protocol.go, split into read and write halves so the async connection
// can block in a read on one thread while handler threads write responses.

use crate::message::{Id, Message, Response, ResponseError, TransportError};

pub trait ProtocolReader: Send {
    /// Reads the next message. A clean end of stream before a frame starts is
    /// `TransportError::Eof`; anything else is an error that terminates the connection.
    fn read_message(&mut self) -> Result<Message, TransportError>;
}

pub trait ProtocolWriter: Send {
    /// Writes a server-to-client request. `params` is JSON text (None = omitted / Go nil).
    fn write_request(
        &mut self,
        id: &Id,
        method: &str,
        params: Option<&[u8]>,
    ) -> Result<(), TransportError>;
    fn write_notification(
        &mut self,
        method: &str,
        params: Option<&[u8]>,
    ) -> Result<(), TransportError>;
    fn write_response(&mut self, id: &Id, result: &Response) -> Result<(), TransportError>;
    fn write_error(&mut self, id: &Id, err: &ResponseError) -> Result<(), TransportError>;
}

/// Which wire protocol a connection speaks. `--api` alone is `MessagePack` + sync connection; `--api --async`
/// is JSON-RPC + async connection (server.go Run).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WireProtocol {
    MessagePack,
    JsonRpc,
}
