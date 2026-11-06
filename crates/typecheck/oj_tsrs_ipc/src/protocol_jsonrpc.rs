use std::io::{Read, Write};
use std::sync::Mutex;

use tsrs_core::json::Value;

use crate::jsonrpc::{self, ID, ResponseError};
use crate::{Error, Message, Protocol, lock};

// protocol_jsonrpc.go:12
// JSONRPCProtocol implements the Protocol interface using JSON-RPC 2.0
// with the LSP base protocol framing (Content-Length headers).
// The reader is only used by the connection's read loop and the writer by one writer at a time; each has its own
// lock so that a read blocked on the stream never holds up a write.
pub struct JSONRPCProtocol<R: Read, W: Write> {
    reader: Mutex<jsonrpc::Reader<R>>,
    writer: Mutex<jsonrpc::Writer<W>>,
}

// protocol_jsonrpc.go:20
// NewJSONRPCProtocol creates a new JSON-RPC protocol handler.
// Go reads and writes one io.ReadWriter; here the two halves of a ReadWriteCloser.
pub fn new_jsonrpc_protocol<R: Read, W: Write>(r: R, w: W) -> JSONRPCProtocol<R, W> {
    JSONRPCProtocol {
        reader: Mutex::new(jsonrpc::new_reader(r)),
        writer: Mutex::new(jsonrpc::new_writer(w)),
    }
}

impl<R: Read + Send, W: Write + Send> Protocol for JSONRPCProtocol<R, W> {
    // protocol_jsonrpc.go:28
    fn read_message(&self) -> Result<Message, Error> {
        let data = lock(&self.reader).read()?;
        Message::unmarshal(&data)
    }

    // protocol_jsonrpc.go:43
    fn write_request(&self, id: &ID, method: &str, params: Option<&Value>) -> Result<(), Error> {
        let data = jsonrpc::marshal_request_message(Some(id), method, params)?;
        lock(&self.writer).write(data.as_bytes())
    }

    // protocol_jsonrpc.go:57
    fn write_notification(&self, method: &str, params: Option<&Value>) -> Result<(), Error> {
        let data = jsonrpc::marshal_request_message(None, method, params)?;
        lock(&self.writer).write(data.as_bytes())
    }

    // protocol_jsonrpc.go:70
    fn write_response(&self, id: &ID, result: &Value) -> Result<(), Error> {
        let data = jsonrpc::marshal_response_message(Some(id), Some(result), None)?;
        lock(&self.writer).write(data.as_bytes())
    }

    // protocol_jsonrpc.go:86
    fn write_error(&self, id: &ID, resp_err: &ResponseError) -> Result<(), Error> {
        let data = jsonrpc::marshal_response_message(Some(id), None, Some(resp_err))?;
        lock(&self.writer).write(data.as_bytes())
    }
}
