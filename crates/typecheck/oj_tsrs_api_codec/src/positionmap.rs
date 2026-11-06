//! Preserves Go's UTF-8 and UTF-16 position-mapping semantics.
//!
//! UTF-8 ⇄ UTF-16 offset mapping with the exact semantics of the pinned Go `ast.PositionMap` (`tsc/internal/ast/positionmap.go`), including WTF-8 lone surrogates (3 bytes, 1 UTF-16 unit) and invalid bytes (1 byte, 1 unit).
//!
//!
//! `tsrs_ast::compute_position_map` decodes with `chars()` and therefore only handles valid UTF-8; the
//! codec needs the Go behavior because API clients can send text with lone surrogates.

use tsrs_core::stringutil::decode_js_string_rune_bytes;

#[derive(Clone, Copy, Debug)]
struct Entry {
    utf8_pos: i64,
    delta: i64,
}

#[derive(Clone, Debug, Default)]
pub struct PositionMap {
    ascii_only: bool,
    entries: Vec<Entry>,
}

impl PositionMap {
    /// Go `ComputePositionMap`.
    pub(crate) fn compute(text: &str) -> PositionMap {
        let bytes = text.as_bytes();
        let mut entries = Vec::new();
        let mut delta = 0i64;
        let mut i = 0usize;
        while i < bytes.len() {
            if bytes[i] < 0x80 {
                i += 1;
                continue;
            }
            let (r, size) = decode_js_string_rune_bytes(&bytes[i..]);
            let size = size.max(1);
            let utf16_size = if r >= 0x10000 { 2 } else { 1 };
            delta += size as i64 - utf16_size;
            entries.push(Entry { utf8_pos: (i + size) as i64, delta });
            i += size;
        }
        PositionMap { ascii_only: entries.is_empty(), entries }
    }

    /// Go `UTF8ToUTF16`.
    pub(crate) fn utf8_to_utf16(&self, utf8_offset: i64) -> i64 {
        if self.ascii_only {
            return utf8_offset;
        }
        let lo = self.entries.partition_point(|e| e.utf8_pos <= utf8_offset);
        if lo == 0 {
            return utf8_offset;
        }
        utf8_offset - self.entries[lo - 1].delta
    }
}
