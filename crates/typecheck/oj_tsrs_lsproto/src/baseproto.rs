use std::io::{Read, Write};
use std::ops::{Deref, DerefMut};

use crate::jsonrpc;

// https://microsoft.github.io/language-server-protocol/specifications/base/0.9/specification/

// baseproto.go:12
// BaseReader wraps jsonrpc.Reader for backwards compatibility.
pub struct BaseReader<R: Read> {
    reader: jsonrpc::Reader<R>,
}

// baseproto.go:17
// NewBaseReader creates a new BaseReader.
pub fn new_base_reader<R: Read>(r: R) -> BaseReader<R> {
    BaseReader { reader: jsonrpc::new_reader(r) }
}

impl<R: Read> Deref for BaseReader<R> {
    type Target = jsonrpc::Reader<R>;

    fn deref(&self) -> &Self::Target {
        &self.reader
    }
}

impl<R: Read> DerefMut for BaseReader<R> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.reader
    }
}

// baseproto.go:24
// BaseWriter wraps jsonrpc.Writer for backwards compatibility.
pub struct BaseWriter<W: Write> {
    writer: jsonrpc::Writer<W>,
}

// baseproto.go:29
// NewBaseWriter creates a new BaseWriter.
pub fn new_base_writer<W: Write>(w: W) -> BaseWriter<W> {
    BaseWriter { writer: jsonrpc::new_writer(w) }
}

impl<W: Write> Deref for BaseWriter<W> {
    type Target = jsonrpc::Writer<W>;

    fn deref(&self) -> &Self::Target {
        &self.writer
    }
}

impl<W: Write> DerefMut for BaseWriter<W> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.writer
    }
}
