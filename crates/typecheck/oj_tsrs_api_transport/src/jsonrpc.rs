// Port of tsc/internal/jsonrpc/baseproto.go (Content-Length framing) and
// tsc/internal/ipc/protocol_jsonrpc.go (JSON-RPC 2.0 messages) for the `--api --async` server.

use std::io::{BufRead, BufReader, BufWriter, Read, Write};

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::message::{
    DEFAULT_MAX_FRAME_BYTES, Id, Message, Response, ResponseError, TransportError,
    read_exact_bounded,
};
use crate::protocol::{ProtocolReader, ProtocolWriter};

const ERR_INVALID_HEADER: &str = "jsonrpc: invalid header";
const ERR_INVALID_CONTENT_LENGTH: &str = "jsonrpc: invalid content length";
const ERR_NO_CONTENT_LENGTH: &str = "jsonrpc: no content length";

/// Upper bound on one header line. Go's bufio `ReadBytes` is unbounded; a header line is never legitimately
/// this long, so longer lines are rejected instead of buffered forever.
const MAX_HEADER_LINE_BYTES: usize = 8 * 1024;

/// baseproto.go Reader.
pub struct FrameReader<R: Read> {
    r: BufReader<R>,
    max_frame_bytes: u64,
}

impl<R: Read> FrameReader<R> {
    pub fn new(r: R) -> Self {
        Self::with_limit(r, DEFAULT_MAX_FRAME_BYTES)
    }

    pub fn with_limit(r: R, max_frame_bytes: u64) -> Self {
        FrameReader { r: BufReader::with_capacity(64 * 1024, r), max_frame_bytes }
    }

    fn read_line(&mut self) -> Result<Option<Vec<u8>>, TransportError> {
        let mut line = Vec::new();
        let limit = (MAX_HEADER_LINE_BYTES + 1) as u64;
        let n = (&mut self.r)
            .take(limit)
            .read_until(b'\n', &mut line)
            .map_err(|e| TransportError::Protocol(format!("jsonrpc: read header: {e}")))?;
        if line.last() == Some(&b'\n') {
            return Ok(Some(line));
        }
        if n as u64 >= limit {
            return Err(TransportError::Protocol(format!(
                "{ERR_INVALID_HEADER}: header line exceeds {MAX_HEADER_LINE_BYTES} bytes"
            )));
        }
        // Go returns io.EOF for an EOF anywhere in the header block.
        Ok(None)
    }

    /// baseproto.go Read.
    pub fn read_frame(&mut self) -> Result<Vec<u8>, TransportError> {
        let mut content_length: i64 = 0;
        loop {
            let Some(line) = self.read_line()? else {
                return Err(TransportError::Eof);
            };
            if line == b"\r\n" {
                break;
            }
            let Some(colon) = line.iter().position(|&b| b == b':') else {
                return Err(TransportError::Protocol(format!(
                    "{ERR_INVALID_HEADER}: {}",
                    go_quote(&line)
                )));
            };
            let (key, value) = (&line[..colon], &line[colon + 1..]);
            if key == b"Content-Length" {
                let text = tsrs_core::utf8::from_utf8_lossy(value.trim_ascii()).into_owned();
                content_length = parse_go_int64(&text).map_err(|e| {
                    TransportError::Protocol(format!(
                        "{ERR_INVALID_CONTENT_LENGTH}: parse error: {e}"
                    ))
                })?;
                if content_length < 0 {
                    return Err(TransportError::Protocol(format!(
                        "{ERR_INVALID_CONTENT_LENGTH}: negative value {content_length}"
                    )));
                }
            }
        }
        if content_length <= 0 {
            return Err(TransportError::Protocol(ERR_NO_CONTENT_LENGTH.to_string()));
        }
        // Deliberate divergence: Go reports a body truncated before its first byte as io.EOF (clean
        // shutdown); any truncation inside a declared frame is reported here as an explicit error.
        read_exact_bounded(
            &mut self.r,
            content_length as u64,
            self.max_frame_bytes,
            "jsonrpc: read content",
        )
    }
}

/// strconv.ParseInt(s, 10, 64) error text.
fn parse_go_int64(s: &str) -> Result<i64, String> {
    tsrs_core::gocompat::parse_int64(s.as_bytes(), tsrs_core::gocompat::QuoteStyle::Api)
}

/// A close approximation of Go's %q for error messages.
fn go_quote(bytes: &[u8]) -> String {
    tsrs_core::gocompat::quote(bytes, tsrs_core::gocompat::QuoteStyle::Api)
}

#[derive(Deserialize)]
struct WireError {
    code: i32,
    message: String,
}

// Unknown fields are ignored (encoding/json/v2 default); duplicate names are rejected by both.
#[derive(Deserialize)]
struct WireMessage<'a> {
    #[serde(borrow, default)]
    jsonrpc: Option<&'a RawValue>,
    #[serde(borrow, default)]
    id: Option<&'a RawValue>,
    #[serde(default)]
    method: Option<String>,
    #[serde(borrow, default)]
    params: Option<&'a RawValue>,
    #[serde(borrow, default)]
    result: Option<&'a RawValue>,
    #[serde(default)]
    error: Option<WireError>,
}

/// Decodes one JSON-RPC message body (jsonrpc.Message unmarshal).
pub fn decode_message(data: &[u8]) -> Result<Message, TransportError> {
    // jsontext validates the whole message, including params/result it does not interpret.
    crate::strictjson::validate(data).map_err(TransportError::Protocol)?;
    let wire: WireMessage<'_> = serde_json::from_slice(data)
        .map_err(|e| TransportError::Protocol(format!("jsonrpc: {e}")))?;
    if let Some(version) = wire.jsonrpc
        && version.get() != "\"2.0\""
    {
        return Err(TransportError::Protocol("invalid JSON-RPC version".to_string()));
    }
    let id = match wire.id {
        None => None,
        Some(raw) if raw.get() == "null" => None,
        Some(raw) => Some(decode_id(raw.get())?),
    };
    Ok(Message {
        id,
        method: wire.method.unwrap_or_default(),
        params: wire.params.map(|p| p.get().as_bytes().to_vec()),
        result: wire.result.map(|r| r.get().as_bytes().to_vec()),
        error: wire.error.map(|e| ResponseError { code: e.code, message: e.message }),
    })
}

fn decode_id(raw: &str) -> Result<Id, TransportError> {
    if raw.starts_with('"') {
        let s: String = serde_json::from_str(raw)
            .map_err(|e| TransportError::Protocol(format!("jsonrpc: invalid id: {e}")))?;
        return Ok(Id::Str(s));
    }
    let i: i32 = serde_json::from_str(raw)
        .map_err(|e| TransportError::Protocol(format!("jsonrpc: invalid id {raw}: {e}")))?;
    Ok(Id::Int(i))
}

impl<R: Read + Send> ProtocolReader for FrameReader<R> {
    fn read_message(&mut self) -> Result<Message, TransportError> {
        let data = self.read_frame()?;
        decode_message(&data)
    }
}

/// baseproto.go Writer + `protocol_jsonrpc.go` Write*.
pub struct FrameWriter<W: Write> {
    w: BufWriter<W>,
}

impl<W: Write> FrameWriter<W> {
    pub fn new(w: W) -> Self {
        FrameWriter { w: BufWriter::with_capacity(64 * 1024, w) }
    }

    pub fn write_frame(&mut self, data: &[u8]) -> Result<(), TransportError> {
        write!(self.w, "Content-Length: {}\r\n\r\n", data.len())?;
        self.w.write_all(data)?;
        self.w.flush()?;
        Ok(())
    }
}

/// Validates that `bytes` is exactly one JSON value, as json.Marshal of a json.Value does.
pub(crate) fn validate_json(bytes: &[u8]) -> Result<&str, String> {
    let s = tsrs_core::utf8::compat::from_utf8(bytes)
        .map_err(|e| format!("invalid UTF-8 in JSON result: {e}"))?;
    serde_json::from_str::<&RawValue>(s).map_err(|e| format!("invalid JSON result: {e}"))?;
    Ok(s.trim_matches(|c: char| c == ' ' || c == '\t' || c == '\n' || c == '\r'))
}

fn encode_request(
    id: Option<&Id>,
    method: &str,
    params: Option<&[u8]>,
) -> Result<Vec<u8>, TransportError> {
    let mut out = String::from("{\"jsonrpc\":\"2.0\"");
    if let Some(id) = id {
        out.push_str(",\"id\":");
        out.push_str(&id.to_json());
    }
    out.push_str(",\"method\":");
    out.push_str(&serde_json::to_string(method).expect("string"));
    if let Some(params) = params {
        let params = validate_json(params).map_err(TransportError::Protocol)?;
        out.push_str(",\"params\":");
        out.push_str(params);
    }
    out.push('}');
    Ok(out.into_bytes())
}

fn encode_response(id: &Id, result: &Response) -> Result<Vec<u8>, TransportError> {
    let mut out = String::from("{\"jsonrpc\":\"2.0\",\"id\":");
    out.push_str(&id.to_json());
    out.push_str(",\"result\":");
    match result {
        Response::Json(json) => {
            out.push_str(validate_json(json).map_err(TransportError::Protocol)?);
        }
        Response::ValidJson(json) => out.push_str(
            std::str::from_utf8(json).map_err(|e| TransportError::Protocol(e.to_string()))?,
        ),
        // json.Marshal([]byte) is a base64 string; a nil RawBinary marshals as null.
        Response::Binary(raw) => {
            out.push('"');
            out.push_str(&crate::base64::encode(raw));
            out.push('"');
        }
    }
    out.push('}');
    Ok(out.into_bytes())
}

fn encode_error(id: &Id, err: &ResponseError) -> Vec<u8> {
    let mut out = String::from("{\"jsonrpc\":\"2.0\",\"id\":");
    out.push_str(&id.to_json());
    out.push_str(",\"error\":{\"code\":");
    out.push_str(&err.code.to_string());
    out.push_str(",\"message\":");
    out.push_str(&serde_json::to_string(&err.message).expect("string"));
    out.push_str("}}");
    out.into_bytes()
}

impl<W: Write + Send> ProtocolWriter for FrameWriter<W> {
    fn write_request(
        &mut self,
        id: &Id,
        method: &str,
        params: Option<&[u8]>,
    ) -> Result<(), TransportError> {
        let data = encode_request(Some(id), method, params)?;
        self.write_frame(&data)
    }

    fn write_notification(
        &mut self,
        method: &str,
        params: Option<&[u8]>,
    ) -> Result<(), TransportError> {
        let data = encode_request(None, method, params)?;
        self.write_frame(&data)
    }

    fn write_response(&mut self, id: &Id, result: &Response) -> Result<(), TransportError> {
        let data = encode_response(id, result)?;
        self.write_frame(&data)
    }

    fn write_error(&mut self, id: &Id, err: &ResponseError) -> Result<(), TransportError> {
        self.write_frame(&encode_error(id, err))
    }
}
