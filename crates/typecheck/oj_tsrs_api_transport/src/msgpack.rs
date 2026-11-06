// Port of tsc/internal/api/protocol_msgpack.go: the custom MessagePack tuple framing
// [MessageType, method (bin), payload (bin)] used by the synchronous `--api` server.

use std::io::{BufReader, BufWriter, Read, Write};

use crate::message::{
    DEFAULT_MAX_FRAME_BYTES, Id, Message, Response, ResponseError, TransportError,
    read_exact_bounded,
};
use crate::protocol::{ProtocolReader, ProtocolWriter};

/// `protocol_msgpack.go` `MessageType`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum MessageType {
    Unknown = 0,
    Request = 1,
    CallResponse = 2,
    CallError = 3,
    Response = 4,
    Error = 5,
    Call = 6,
}

impl MessageType {
    pub fn from_u8(v: u8) -> Option<MessageType> {
        Some(match v {
            1 => MessageType::Request,
            2 => MessageType::CallResponse,
            3 => MessageType::CallError,
            4 => MessageType::Response,
            5 => MessageType::Error,
            6 => MessageType::Call,
            _ => return None,
        })
    }
}

const MSGPACK_FIXED_ARRAY3: u8 = 0x93;
const MSGPACK_BIN8: u8 = 0xC4;
const MSGPACK_BIN16: u8 = 0xC5;
const MSGPACK_BIN32: u8 = 0xC6;
const MSGPACK_U8: u8 = 0xCC;

/// api.ErrInvalidRequest's text.
const ERR_INVALID_REQUEST: &str = "api: invalid request";

/// A decoded tuple.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tuple {
    pub msg_type: MessageType,
    pub method: Vec<u8>,
    pub payload: Vec<u8>,
}

pub struct MessagePackReader<R: Read> {
    r: BufReader<R>,
    max_frame_bytes: u64,
}

impl<R: Read> MessagePackReader<R> {
    pub fn new(r: R) -> Self {
        Self::with_limit(r, DEFAULT_MAX_FRAME_BYTES)
    }

    pub fn with_limit(r: R, max_frame_bytes: u64) -> Self {
        MessagePackReader { r: BufReader::with_capacity(64 * 1024, r), max_frame_bytes }
    }

    /// Reads one byte; `first` distinguishes a clean EOF between frames from a truncated frame.
    fn read_byte(&mut self, first: bool) -> Result<u8, TransportError> {
        let mut b = [0u8; 1];
        loop {
            match self.r.read(&mut b) {
                Ok(0) if first => return Err(TransportError::Eof),
                Ok(0) => return Err(TransportError::UnexpectedEof("msgpack: read tuple")),
                Ok(_) => return Ok(b[0]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(TransportError::Io(e)),
            }
        }
    }

    fn read_n<const N: usize>(&mut self) -> Result<[u8; N], TransportError> {
        let mut buf = [0u8; N];
        self.r.read_exact(&mut buf).map_err(|e| match e.kind() {
            std::io::ErrorKind::UnexpectedEof => {
                TransportError::UnexpectedEof("msgpack: read bin length")
            }
            _ => TransportError::Io(e),
        })?;
        Ok(buf)
    }

    /// `protocol_msgpack.go` readTuple.
    pub fn read_tuple(&mut self) -> Result<Tuple, TransportError> {
        let t = self.read_byte(true)?;
        if t != MSGPACK_FIXED_ARRAY3 {
            return Err(TransportError::Protocol(format!(
                "{ERR_INVALID_REQUEST}: expected fixed 3-element array (0x93), received: 0x{t:02x}"
            )));
        }
        let t = self.read_byte(false)?;
        let raw_type = if t <= 0x7F {
            t
        } else if t == MSGPACK_U8 {
            self.read_byte(false)?
        } else {
            return Err(TransportError::Protocol(format!(
                "{ERR_INVALID_REQUEST}: expected positive fixint or uint8 marker, received: 0x{t:02x}"
            )));
        };
        let Some(msg_type) = MessageType::from_u8(raw_type) else {
            return Err(TransportError::Protocol(format!(
                "{ERR_INVALID_REQUEST}: unknown message type: {raw_type}"
            )));
        };
        let method = self.read_bin()?;
        let payload = self.read_bin()?;
        Ok(Tuple { msg_type, method, payload })
    }

    fn read_bin(&mut self) -> Result<Vec<u8>, TransportError> {
        let t = self.read_byte(false)?;
        let size: u64 = match t {
            MSGPACK_BIN8 => self.read_byte(false)? as u64,
            MSGPACK_BIN16 => u16::from_be_bytes(self.read_n::<2>()?) as u64,
            MSGPACK_BIN32 => u32::from_be_bytes(self.read_n::<4>()?) as u64,
            _ => {
                return Err(TransportError::Protocol(format!(
                    "{ERR_INVALID_REQUEST}: expected binary data (0xc4-0xc6), received: 0x{t:02x}"
                )));
            }
        };
        read_exact_bounded(&mut self.r, size, self.max_frame_bytes, "msgpack: read bin")
    }
}

impl<R: Read + Send> ProtocolReader for MessagePackReader<R> {
    /// `protocol_msgpack.go` `ReadMessage`. The protocol has no request IDs: the method name is the ID.
    fn read_message(&mut self) -> Result<Message, TransportError> {
        let tuple = self.read_tuple()?;
        // Method names are UTF-8, so the bin's buffer usually becomes the string as it is.
        let method = String::from_utf8(tuple.method)
            .unwrap_or_else(|e| tsrs_core::utf8::from_utf8_lossy(e.as_bytes()).into_owned());
        let mut msg = Message::default();
        match tuple.msg_type {
            MessageType::Request => {
                msg.id = Some(Id::Str(method.clone()));
                msg.method = method;
                msg.params = Some(tuple.payload);
            }
            MessageType::CallResponse => {
                msg.id = Some(Id::Str(method));
                msg.result = Some(tuple.payload);
            }
            MessageType::CallError => {
                msg.id = Some(Id::Str(method));
                msg.error = Some(ResponseError::internal(
                    tsrs_core::utf8::from_utf8_lossy(&tuple.payload).into_owned(),
                ));
            }
            other => {
                return Err(TransportError::Protocol(format!(
                    "unexpected message type: {}",
                    other as u8
                )));
            }
        }
        Ok(msg)
    }
}

pub struct MessagePackWriter<W: Write> {
    w: BufWriter<W>,
}

impl<W: Write> MessagePackWriter<W> {
    pub fn new(w: W) -> Self {
        MessagePackWriter { w: BufWriter::with_capacity(64 * 1024, w) }
    }

    /// `protocol_msgpack.go` writeTuple: message type as a positive fixint, then two bin fields, then a
    /// flush.
    pub fn write_tuple(
        &mut self,
        msg_type: MessageType,
        method: &[u8],
        payload: &[u8],
    ) -> Result<(), TransportError> {
        self.w.write_all(&[MSGPACK_FIXED_ARRAY3, msg_type as u8])?;
        self.write_bin(method)?;
        self.write_bin(payload)?;
        self.w.flush()?;
        Ok(())
    }

    fn write_bin(&mut self, data: &[u8]) -> Result<(), TransportError> {
        let len = data.len();
        if len < 256 {
            self.w.write_all(&[MSGPACK_BIN8, len as u8])?;
        } else if len < 1 << 16 {
            self.w.write_all(&[MSGPACK_BIN16])?;
            self.w.write_all(&(len as u16).to_be_bytes())?;
        } else {
            let Ok(len32) = u32::try_from(len) else {
                return Err(TransportError::Protocol(format!(
                    "msgpack: payload of {len} bytes exceeds bin32"
                )));
            };
            self.w.write_all(&[MSGPACK_BIN32])?;
            self.w.write_all(&len32.to_be_bytes())?;
        }
        self.w.write_all(data)?;
        Ok(())
    }
}

impl<W: Write + Send> ProtocolWriter for MessagePackWriter<W> {
    /// Requests from the server are Call tuples; params nil marshals to `null`.
    fn write_request(
        &mut self,
        _id: &Id,
        method: &str,
        params: Option<&[u8]>,
    ) -> Result<(), TransportError> {
        self.write_tuple(MessageType::Call, method.as_bytes(), params.unwrap_or(b"null"))
    }

    /// The msgpack protocol does not distinguish notifications from calls.
    fn write_notification(
        &mut self,
        method: &str,
        params: Option<&[u8]>,
    ) -> Result<(), TransportError> {
        self.write_tuple(MessageType::Call, method.as_bytes(), params.unwrap_or(b"null"))
    }

    fn write_response(&mut self, id: &Id, result: &Response) -> Result<(), TransportError> {
        let method = id.text();
        let payload: &[u8] = match result {
            // json.Marshal of a json.Value validates it.
            Response::Json(json) => {
                crate::jsonrpc::validate_json(json).map_err(TransportError::Protocol)?.as_bytes()
            }
            Response::ValidJson(json) | Response::Binary(json) => json,
        };
        self.write_tuple(MessageType::Response, method.as_bytes(), payload)
    }

    fn write_error(&mut self, id: &Id, err: &ResponseError) -> Result<(), TransportError> {
        let method = id.text();
        self.write_tuple(MessageType::Error, method.as_bytes(), err.message.as_bytes())
    }
}
