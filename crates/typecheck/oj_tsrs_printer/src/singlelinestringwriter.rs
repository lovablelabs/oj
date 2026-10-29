use tsrs_ast::*;
use tsrs_core::*;

use crate::EmitTextWriter;
use crate::textwriter::last_rune_is_white_space_like;

// Go pools these writers; the release function is kept for call-site parity and does nothing.
pub fn get_single_line_string_writer() -> (Box<dyn EmitTextWriter>, impl FnOnce()) {
    let mut w = SingleLineStringWriter::default();
    w.clear();
    (Box::new(w) as Box<dyn EmitTextWriter>, || {})
}

#[derive(Default)]
pub struct SingleLineStringWriter {
    builder: String,
    // Go keeps the last written string; only whether its last rune is white space is ever read.
    last_written_is_white_space_like: bool,
}

impl SingleLineStringWriter {
    fn set_last_written(&mut self, s: &str) {
        self.last_written_is_white_space_like = last_rune_is_white_space_like(s);
    }

    fn push(&mut self, s: &str) {
        self.set_last_written(s);
        self.builder.push_str(s);
    }
}

impl EmitTextWriter for SingleLineStringWriter {
    fn clear(&mut self) {
        self.last_written_is_white_space_like = false;
        self.builder.clear();
    }

    fn decrease_indent(&mut self) {
        // Do Nothing
    }

    fn get_column(&self) -> UTF16Offset {
        0
    }

    fn get_indent(&self) -> u32 {
        0
    }

    fn get_line(&self) -> u32 {
        0
    }

    fn string(&self) -> String {
        self.builder.clone()
    }

    fn get_text_pos(&self) -> u32 {
        self.builder.len() as u32
    }

    fn has_trailing_comment(&self) -> bool {
        false
    }

    fn has_trailing_whitespace(&self) -> bool {
        if self.builder.is_empty() {
            return false;
        }
        self.last_written_is_white_space_like
    }

    fn increase_indent(&mut self) {
        // Do Nothing
    }

    fn is_at_start_of_line(&self) -> bool {
        false
    }

    fn raw_write(&mut self, s: &str) {
        self.push(s);
    }

    fn write(&mut self, s: &str) {
        self.push(s);
    }

    fn write_comment(&mut self, text: &str) {
        self.push(text);
    }

    fn write_keyword(&mut self, text: &str) {
        self.push(text);
    }

    fn write_line(&mut self) {
        self.push(" ");
    }

    fn write_line_force(&mut self, _force: bool) {
        self.push(" ");
    }

    fn write_literal(&mut self, s: &str) {
        self.push(s);
    }

    fn write_operator(&mut self, text: &str) {
        self.push(text);
    }

    fn write_parameter(&mut self, text: &str) {
        self.push(text);
    }

    fn write_property(&mut self, text: &str) {
        self.push(text);
    }

    fn write_punctuation(&mut self, text: &str) {
        self.push(text);
    }

    fn write_space(&mut self, text: &str) {
        self.push(text);
    }

    fn write_string_literal(&mut self, text: &str) {
        self.push(text);
    }

    fn write_symbol(&mut self, text: &str, _symbol: P<Symbol>) {
        self.push(text);
    }

    fn write_trailing_semicolon(&mut self, text: &str) {
        self.push(text);
    }
}
