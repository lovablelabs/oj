use std::io::{BufRead, BufReader, BufWriter, Read, Write};

use crate::error::{Error, ErrorTag};
use crate::lsp::go_quote;

// Base protocol for JSON-RPC with Content-Length headers (as used by LSP).
// https://microsoft.github.io/language-server-protocol/specifications/base/0.9/specification/

// baseproto.go:15
const ERR_INVALID_HEADER: &str = "jsonrpc: invalid header";
const ERR_INVALID_CONTENT_LENGTH: &str = "jsonrpc: invalid content length";
const ERR_NO_CONTENT_LENGTH: &str = "jsonrpc: no content length";

// baseproto.go:22
// Reader reads JSON-RPC messages with Content-Length framing.
pub struct Reader<R: Read> {
    r: BufReader<R>,
}

// baseproto.go:27
// NewReader creates a new Reader.
pub(crate) fn new_reader<R: Read>(r: R) -> Reader<R> {
    Reader { r: BufReader::with_capacity(4096, r) }
}

fn io_error(err: &std::io::Error) -> Error {
    if err.kind() == std::io::ErrorKind::UnexpectedEof {
        return Error::tagged(ErrorTag::UnexpectedEOF, "unexpected EOF");
    }
    Error::new(err.to_string())
}

// Go strconv.ParseInt(s, 10, 64).
fn parse_int(s: &str) -> Result<i64, String> {
    tsrs_core::gocompat::parse_int64(s.as_bytes(), tsrs_core::gocompat::QuoteStyle::Lsp)
}

// Go bytes.TrimSpace: trims Unicode white space (including '\v', U+0085 and U+00A0 as UTF-8, which `trim_ascii`
// keeps); an invalid UTF-8 byte decodes as RuneError and stops the trim.
fn trim_space(mut s: &[u8]) -> &[u8] {
    let is_space = |r: u32| char::from_u32(r).is_some_and(char::is_whitespace);
    loop {
        let (r, size) = tsrs_core::stringutil::decode_rune(s);
        if size == 0 || !is_space(r) {
            break;
        }
        s = &s[size..];
    }
    loop {
        let (r, size) = tsrs_core::stringutil::decode_last_rune(s);
        if size == 0 || !is_space(r) {
            break;
        }
        s = &s[..s.len() - size];
    }
    s
}

impl<R: Read> Reader<R> {
    // baseproto.go:34
    // Read reads the next message payload.
    pub fn read(&mut self) -> Result<Vec<u8>, Error> {
        let mut content_length: i64 = 0;

        loop {
            let mut line = Vec::new();
            match self.r.read_until(b'\n', &mut line) {
                Ok(_) if line.last() == Some(&b'\n') => {}
                Ok(_) => return Err(Error::tagged(ErrorTag::EOF, "EOF")),
                Err(err) => return Err(Error::wrap("jsonrpc: read header: ", io_error(&err))),
            }

            if line == b"\r\n" {
                break;
            }

            let Some(colon) = line.iter().position(|&b| b == b':') else {
                return Err(Error::tagged(
                    ErrorTag::InvalidHeader,
                    format!("{}: {}", ERR_INVALID_HEADER, go_quote(&line)),
                ));
            };
            let (key, value) = (&line[..colon], &line[colon + 1..]);

            if key == b"Content-Length" {
                let value = tsrs_core::utf8::from_utf8_lossy(trim_space(value)).into_owned();
                content_length = match parse_int(&value) {
                    Ok(n) => n,
                    Err(err) => {
                        return Err(Error::tagged(
                            ErrorTag::InvalidContentLength,
                            format!("{ERR_INVALID_CONTENT_LENGTH}: parse error: {err}"),
                        ));
                    }
                };
                if content_length < 0 {
                    return Err(Error::tagged(
                        ErrorTag::InvalidContentLength,
                        format!("{ERR_INVALID_CONTENT_LENGTH}: negative value {content_length}"),
                    ));
                }
            }
        }

        if content_length <= 0 {
            return Err(Error::tagged(ErrorTag::NoContentLength, ERR_NO_CONTENT_LENGTH));
        }

        // io.ReadFull: EOF before any byte, ErrUnexpectedEOF after some.
        let mut data = vec![0u8; content_length as usize];
        let mut filled = 0;
        while filled < data.len() {
            match self.r.read(&mut data[filled..]) {
                Ok(0) => {
                    let err = if filled == 0 {
                        Error::tagged(ErrorTag::EOF, "EOF")
                    } else {
                        Error::tagged(ErrorTag::UnexpectedEOF, "unexpected EOF")
                    };
                    return Err(Error::wrap("jsonrpc: read content: ", err));
                }
                Ok(n) => filled += n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => return Err(Error::wrap("jsonrpc: read content: ", io_error(&err))),
            }
        }

        Ok(data)
    }
}

// baseproto.go:79
// Writer writes JSON-RPC messages with Content-Length framing.
pub struct Writer<W: Write> {
    w: BufWriter<W>,
}

// baseproto.go:84
// NewWriter creates a new Writer.
pub(crate) fn new_writer<W: Write>(w: W) -> Writer<W> {
    Writer { w: BufWriter::with_capacity(4096, w) }
}

impl<W: Write> Writer<W> {
    // baseproto.go:91
    // Write writes a message payload with Content-Length header.
    pub fn write(&mut self, data: &[u8]) -> Result<(), Error> {
        write!(self.w, "Content-Length: {}\r\n\r\n", data.len()).map_err(|err| io_error(&err))?;
        self.w.write_all(data).map_err(|err| io_error(&err))?;
        self.w.flush().map_err(|err| io_error(&err))
    }
}
