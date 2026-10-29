use std::sync::Arc;

use tsrs_core::stringutil;
// linemap.go:12
pub type LSPLineStarts = Vec<u32>;

// linemap.go:14
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct LSPLineMap {
    pub line_starts: LSPLineStarts,
    pub ascii_only: bool, // TODO(jakebailey): collect ascii-only info per line
}

// linemap.go:19
pub fn compute_lsp_line_starts(text: &str) -> Arc<LSPLineMap> {
    // This is like core.ComputeLineStarts, but only considers "\n", "\r", and "\r\n" as line breaks,
    // and reports when the text is ASCII-only.
    let bytes = text.as_bytes();
    let mut line_starts: Vec<u32> = Vec::with_capacity(memchr_count(bytes) + 1);
    let mut ascii_only = true;

    let text_len = u32::try_from(bytes.len()).expect("source text exceeds u32 positions");
    let mut pos = 0_u32;
    let mut line_start = 0_u32;
    while pos < text_len {
        let b = bytes[pos as usize];
        if b < 0x80 {
            pos += 1;
            match b {
                b'\r' | b'\n' => {
                    if b == b'\r' && pos < text_len && bytes[pos as usize] == b'\n' {
                        pos += 1;
                    }
                    line_starts.push(line_start);
                    line_start = pos;
                }
                _ => {}
            }
        } else {
            let (_, size) = stringutil::decode_rune(&bytes[pos as usize..]);
            pos += size as u32;
            ascii_only = false;
        }
    }
    line_starts.push(line_start);

    Arc::new(LSPLineMap { line_starts, ascii_only })
}

#[expect(
    clippy::naive_bytecount,
    reason = "a specialized search is a performance change that requires measurements"
)]
fn memchr_count(bytes: &[u8]) -> usize {
    bytes.iter().filter(|&&b| b == b'\n').count()
}

impl LSPLineMap {
    // linemap.go:56
    pub fn compute_index_of_line_start(&self, target_pos: u32) -> usize {
        let line_number = self.line_starts.partition_point(|&position| position < target_pos);
        if self.line_starts.get(line_number) == Some(&target_pos) {
            line_number
        } else {
            line_number.saturating_sub(1)
        }
    }
}
