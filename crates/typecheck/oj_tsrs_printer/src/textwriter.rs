use tsrs_ast::*;
use tsrs_core::stringutil;
use tsrs_core::*;

use crate::EmitTextWriter;

/// Go `utf8.DecodeLastRuneInString(s)` reduced to "is the last rune white-space-like", the only use Go makes of
/// `lastWritten` (`RuneError` counts as not white space).
pub(crate) fn last_rune_is_white_space_like(s: &str) -> bool {
    let (ch, _) = stringutil::decode_last_rune(s.as_bytes());
    if ch == 0xFFFD {
        return false;
    }
    stringutil::is_white_space_like(ch)
}

#[derive(Default)]
pub struct TextWriter {
    new_line: String,
    indent_size: usize,
    builder: String,
    // Go keeps the last written string; only whether its last rune is white space is ever read.
    last_written_is_white_space_like: bool,
    indent: u32,
    line_start: bool,
    line_count: u32,
    line_pos: u32,
    has_trailing_comment_state: bool,
}

impl TextWriter {
    // Go builds `textWriter{newLine: ..., indentSize: ...}` literals inside the package (ChangeTrackerWriter).
    pub(crate) fn new_with(new_line: &str, indent_size: usize) -> TextWriter {
        TextWriter { new_line: new_line.to_string(), indent_size, ..Default::default() }
    }

    fn set_last_written(&mut self, s: &str) {
        self.last_written_is_white_space_like = last_rune_is_white_space_like(s);
    }

    fn update_line_count_and_pos_for(&mut self, s: &str) {
        let mut count = 0;
        let mut last_line_start = 0;

        compute_ecma_line_starts_seq(s, |line_start| {
            count += 1;
            last_line_start = line_start;
            true
        });

        if count > 1 {
            self.line_count += count - 1;
            let cur_len = self.builder.len() as u32;
            self.line_pos = cur_len - s.len() as u32 + last_line_start;
            self.line_start = self.line_pos == cur_len;
            return;
        }
        self.line_start = false;
    }

    fn write_text(&mut self, s: &str) {
        if !s.is_empty() {
            if self.line_start {
                self.builder
                    .extend(std::iter::repeat_n(' ', self.indent as usize * self.indent_size));
                self.line_start = false;
            }
            self.builder.push_str(s);
            self.set_last_written(s);
            self.update_line_count_and_pos_for(s);
        }
    }

    fn write_line_raw(&mut self) {
        self.builder.push_str(&self.new_line);
        let nl = std::mem::take(&mut self.new_line);
        self.set_last_written(&nl);
        self.new_line = nl;
        self.line_count += 1;
        self.line_pos = self.builder.len() as u32;
        self.line_start = true;
        self.has_trailing_comment_state = false;
    }
}

impl EmitTextWriter for TextWriter {
    fn clear(&mut self) {
        *self = TextWriter {
            new_line: std::mem::take(&mut self.new_line),
            indent_size: self.indent_size,
            line_start: true,
            ..Default::default()
        };
    }

    fn grow(&mut self, n: usize) {
        self.builder.reserve(n);
    }

    fn decrease_indent(&mut self) {
        self.indent = self.indent.checked_sub(1).expect("indentation underflow");
    }

    // GetColumn returns the column position measured in UTF-16 code units
    // for source map compatibility.
    fn get_column(&self) -> UTF16Offset {
        if self.line_start {
            return self.indent * self.indent_size as u32;
        }
        // Count UTF-16 code units from the last line start.
        // For ASCII-only output (the common case), this equals the byte count.
        utf16_len(&self.builder[self.line_pos as usize..])
    }

    fn get_indent(&self) -> u32 {
        self.indent
    }

    fn get_line(&self) -> u32 {
        self.line_count
    }

    fn string(&self) -> String {
        self.builder.clone()
    }

    fn take_string(&mut self) -> String {
        let text = std::mem::take(&mut self.builder);
        self.clear();
        text
    }

    fn get_text_pos(&self) -> u32 {
        self.builder.len() as u32
    }

    fn has_trailing_comment(&self) -> bool {
        self.has_trailing_comment_state
    }

    fn has_trailing_whitespace(&self) -> bool {
        if self.builder.is_empty() {
            return false;
        }
        self.last_written_is_white_space_like
    }

    fn increase_indent(&mut self) {
        self.indent += 1;
    }

    fn is_at_start_of_line(&self) -> bool {
        self.line_start
    }

    fn raw_write(&mut self, s: &str) {
        if !s.is_empty() {
            self.builder.push_str(s);
            self.set_last_written(s);
            self.has_trailing_comment_state = false;
        }
        self.update_line_count_and_pos_for(s);
    }

    fn write(&mut self, s: &str) {
        if !s.is_empty() {
            self.has_trailing_comment_state = false;
        }
        self.write_text(s);
    }

    fn write_comment(&mut self, text: &str) {
        if !text.is_empty() {
            self.has_trailing_comment_state = true;
        }
        self.write_text(text);
    }

    fn write_keyword(&mut self, text: &str) {
        self.write(text);
    }

    fn write_line(&mut self) {
        if !self.line_start {
            self.write_line_raw();
        }
    }

    fn write_line_force(&mut self, force: bool) {
        if !self.line_start || force {
            self.write_line_raw();
        }
    }

    fn write_literal(&mut self, s: &str) {
        self.write(s);
    }

    fn write_operator(&mut self, text: &str) {
        self.write(text);
    }

    fn write_parameter(&mut self, text: &str) {
        self.write(text);
    }

    fn write_property(&mut self, text: &str) {
        self.write(text);
    }

    fn write_punctuation(&mut self, text: &str) {
        self.write(text);
    }

    fn write_space(&mut self, text: &str) {
        self.write(text);
    }

    fn write_string_literal(&mut self, text: &str) {
        self.write(text);
    }

    fn write_symbol(&mut self, text: &str, _symbol: P<Symbol>) {
        self.write(text);
    }

    fn write_trailing_semicolon(&mut self, text: &str) {
        self.write(text);
    }
}

const DEFAULT_INDENT_SIZE: usize = 4;

// GetDefaultIndentSize returns the default indent size (4 spaces) used when no specific indent size is configured.
pub fn get_default_indent_size() -> usize {
    DEFAULT_INDENT_SIZE
}

pub(crate) fn get_indent_string(indent: u32, indent_size: usize) -> String {
    if indent == 0 {
        return String::new();
    }
    // TODO: This is cached in tsc - should it be cached here?
    " ".repeat(indent as usize * indent_size)
}

pub fn new_text_writer(new_line: &str, indent_size: usize) -> Box<dyn EmitTextWriter> {
    let mut indent_size = indent_size;
    if indent_size == 0 {
        indent_size = 4;
    }
    let mut w = TextWriter { new_line: new_line.to_string(), indent_size, ..Default::default() };
    w.clear();
    Box::new(w)
}
