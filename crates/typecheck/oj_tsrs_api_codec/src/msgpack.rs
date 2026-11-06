//! Minimal msgpack writers for the structured data section (pinned encoder.go `msgpackWrite*`).

pub fn write_array_header(buf: &mut Vec<u8>, length: usize) {
    if length <= 0x0f {
        buf.push(0x90 | length as u8);
    } else if length <= 0xffff {
        buf.extend_from_slice(&[0xdc, (length >> 8) as u8, length as u8]);
    } else {
        buf.push(0xdd);
        buf.extend_from_slice(&(length as u32).to_be_bytes());
    }
}

pub fn write_uint(buf: &mut Vec<u8>, value: u32) {
    if value <= 0x7f {
        buf.push(value as u8);
    } else if value <= 0xff {
        buf.extend_from_slice(&[0xcc, value as u8]);
    } else if value <= 0xffff {
        buf.extend_from_slice(&[0xcd, (value >> 8) as u8, value as u8]);
    } else {
        buf.push(0xce);
        buf.extend_from_slice(&value.to_be_bytes());
    }
}

/// Writes the raw bytes of `s` (WTF-8 preserved), like Go's byte-string msgpack writer.
pub fn write_string(buf: &mut Vec<u8>, s: &str) {
    let n = s.len();
    if n <= 0x1f {
        buf.push(0xa0 | n as u8);
    } else if n <= 0xff {
        buf.extend_from_slice(&[0xd9, n as u8]);
    } else if n <= 0xffff {
        buf.extend_from_slice(&[0xda, (n >> 8) as u8, n as u8]);
    } else {
        buf.push(0xdb);
        buf.extend_from_slice(&(n as u32).to_be_bytes());
    }
    buf.extend_from_slice(s.as_bytes());
}

pub fn write_bool(buf: &mut Vec<u8>, value: bool) {
    buf.push(if value { 0xc3 } else { 0xc2 });
}
