use std::io::{BufRead, BufReader, Read, Write};

use crate::Error;
use crate::error::eof;

// Base protocol for JSON-RPC with Content-Length headers (as used by LSP).
// https://microsoft.github.io/language-server-protocol/specifications/base/0.9/specification/

// baseproto.go:16
const ERR_INVALID_HEADER: &str = "jsonrpc: invalid header";
const ERR_INVALID_CONTENT_LENGTH: &str = "jsonrpc: invalid content length";
const ERR_NO_CONTENT_LENGTH: &str = "jsonrpc: no content length";

// bufio.NewReader's and bufio.NewWriter's default size.
const BUFFER_SIZE: usize = 4096;

// The most a declared Content-Length reserves before its bytes arrive. Go allocates the whole length up front
// (`make([]byte, contentLength)`); growing as the body arrives reads the same bytes without letting a bad header
// allocate gigabytes.
const MAX_INITIAL_CONTENT_CAPACITY: u64 = 1 << 20;

// baseproto.go:22
// Reader reads JSON-RPC messages with Content-Length framing.
pub struct Reader<R> {
    r: BufReader<R>,
}

// baseproto.go:27
// NewReader creates a new Reader.
pub fn new_reader<R: Read>(r: R) -> Reader<R> {
    Reader { r: BufReader::with_capacity(BUFFER_SIZE, r) }
}

impl<R: Read> Reader<R> {
    // baseproto.go:34
    // Read reads the next message payload.
    pub fn read(&mut self) -> Result<Vec<u8>, Error> {
        let mut content_length: i64 = 0;

        loop {
            // bufio.Reader.ReadBytes('\n'): an error, io.EOF included, when the line does not end in '\n'.
            let mut line = Vec::new();
            match self.r.read_until(b'\n', &mut line) {
                Ok(_) if line.last() == Some(&b'\n') => {}
                Ok(_) => return Err(eof()),
                Err(err) => return Err(Error::new(format!("jsonrpc: read header: {err}"))),
            }

            if line == b"\r\n" {
                break;
            }

            let Some(colon) = line.iter().position(|&b| b == b':') else {
                return Err(Error::new(format!("{ERR_INVALID_HEADER}: {}", go_quote(&line))));
            };
            let (key, value) = (&line[..colon], &line[colon + 1..]);

            if key == b"Content-Length" {
                content_length = match parse_int(trim_space(value)) {
                    Ok(n) => n,
                    Err(err) => {
                        return Err(Error::new(format!(
                            "{ERR_INVALID_CONTENT_LENGTH}: parse error: {err}"
                        )));
                    }
                };
                if content_length < 0 {
                    return Err(Error::new(format!(
                        "{ERR_INVALID_CONTENT_LENGTH}: negative value {content_length}"
                    )));
                }
            }
        }

        if content_length <= 0 {
            return Err(Error::new(ERR_NO_CONTENT_LENGTH));
        }

        // io.ReadFull: io.EOF when no byte arrives, io.ErrUnexpectedEOF when some do.
        let content_length = content_length as u64;
        let mut data =
            Vec::with_capacity(content_length.min(MAX_INITIAL_CONTENT_CAPACITY) as usize);
        match (&mut self.r).take(content_length).read_to_end(&mut data) {
            Ok(n) if n as u64 == content_length => Ok(data),
            Ok(0) => Err(Error::wrap("jsonrpc: read content: ", eof())),
            Ok(_) => Err(Error::new("jsonrpc: read content: unexpected EOF")),
            Err(err) => Err(Error::new(format!("jsonrpc: read content: {err}"))),
        }
    }
}

// baseproto.go:79
// Writer writes JSON-RPC messages with Content-Length framing.
pub struct Writer<W> {
    w: W,
    // Go's bufio.Writer: a frame that fits goes out in one write. Unlike a std BufWriter, nothing is left buffered
    // between frames, so no stale bytes of a failed frame are flushed later (on drop).
    buf: Vec<u8>,
    // bufio.Writer's sticky error: once a write fails, every later write returns the same error, so a frame cut
    // short is never followed by another.
    err: Option<Error>,
}

// baseproto.go:84
// NewWriter creates a new Writer.
pub fn new_writer<W: Write>(w: W) -> Writer<W> {
    Writer { w, buf: Vec::with_capacity(BUFFER_SIZE), err: None }
}

impl<W: Write> Writer<W> {
    // baseproto.go:91
    // Write writes a message payload with Content-Length header.
    pub fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        if let Some(err) = &self.err {
            return Err(err.clone());
        }
        self.buf.clear();
        // Writing to a Vec cannot fail.
        let _ = write!(self.buf, "Content-Length: {}\r\n\r\n", data.len());
        let written = if self.buf.len() + data.len() <= BUFFER_SIZE {
            self.buf.extend_from_slice(data);
            self.w.write_all(&self.buf)
        } else {
            self.w.write_all(&self.buf).and_then(|()| self.w.write_all(data))
        };
        if let Err(err) = written.and_then(|()| self.w.flush()) {
            let err = Error::new(err.to_string());
            self.err = Some(err.clone());
            return Err(err);
        }
        Ok(())
    }
}

// bytes.TrimSpace: Unicode white space at both ends.
fn trim_space(b: &[u8]) -> &[u8] {
    if let Ok(s) = tsrs_core::utf8::basic::from_utf8(b) {
        s.trim().as_bytes()
    } else {
        // Invalid UTF-8 fails strconv.ParseInt below anyway; trim the ASCII white space Go would.

        let is_space = |c: &u8| matches!(c, b' ' | b'\t' | b'\n' | b'\x0b' | b'\x0c' | b'\r');
        let start = b.iter().position(|c| !is_space(c)).unwrap_or(b.len());
        let end = b.iter().rposition(|c| !is_space(c)).map_or(start, |i| i + 1);
        &b[start..end]
    }
}

// strconv.ParseInt(string(s), 10, 64), with its error text.
fn parse_int(s: &[u8]) -> Result<i64, String> {
    tsrs_core::gocompat::parse_int64(s, tsrs_core::gocompat::QuoteStyle::Header)
}

// Go's `%q` of a byte slice (strconv.Quote): printable runes as they are, the usual backslash escapes, `\x` for
// invalid UTF-8 and ASCII controls, `\u` / `\U` for other non-printable runes. Go's unicode.IsPrint is
// approximated: controls, white space other than ' ', and the format characters a header is likely to contain.
fn go_quote(b: &[u8]) -> String {
    tsrs_core::gocompat::quote(b, tsrs_core::gocompat::QuoteStyle::Header)
}
