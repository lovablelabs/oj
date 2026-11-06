use std::sync::Arc;

use tsrs_core::ECMALineStarts;

// lineinfo.go:5
#[derive(Clone, Debug, Default)]
pub struct ECMALineInfo {
    pub(crate) text: String,
    pub(crate) line_starts: ECMALineStarts,
}

// lineinfo.go:10
pub fn create_ecma_line_info(text: String, line_starts: ECMALineStarts) -> Arc<ECMALineInfo> {
    Arc::new(ECMALineInfo { text, line_starts })
}

impl ECMALineInfo {
    // lineinfo.go:17
    pub(crate) fn line_count(&self) -> usize {
        self.line_starts.len()
    }

    // lineinfo.go:21
    pub(crate) fn line_text(&self, line: usize) -> &str {
        let pos = self.line_starts[line];
        let end = if line + 1 < self.line_starts.len() {
            self.line_starts[line + 1]
        } else {
            self.text.len() as u32
        };
        &self.text[pos as usize..end as usize]
    }
}
