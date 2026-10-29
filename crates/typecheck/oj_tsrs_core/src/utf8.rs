use std::borrow::Cow;
use std::io;
use std::path::Path;
use std::string::FromUtf8Error;

pub use simdutf8::{basic, compat};

/// Validates UTF-8 without copying the owned buffer, preserving the standard error on invalid input.
#[inline]
pub fn into_string(bytes: Vec<u8>) -> Result<String, FromUtf8Error> {
    if basic::from_utf8(&bytes).is_ok() {
        // SAFETY: the bytes were validated above and have not been modified.
        Ok(unsafe { String::from_utf8_unchecked(bytes) })
    } else {
        String::from_utf8(bytes)
    }
}

/// Borrows valid UTF-8 and preserves the standard replacement behavior for invalid sequences.
#[inline]
pub fn from_utf8_lossy(bytes: &[u8]) -> Cow<'_, str> {
    match basic::from_utf8(bytes) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => String::from_utf8_lossy(bytes),
    }
}

/// Reads and validates UTF-8, preserving `std::fs::read_to_string`'s invalid-data error.
pub fn read_to_string(path: impl AsRef<Path>) -> io::Result<String> {
    into_string(std::fs::read(path)?).map_err(|_| {
        io::Error::new(io::ErrorKind::InvalidData, "stream did not contain valid UTF-8")
    })
}
