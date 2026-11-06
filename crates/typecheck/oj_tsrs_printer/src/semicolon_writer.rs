use tsrs_ast::*;
use tsrs_core::*;

use crate::EmitTextWriter;

pub struct TrailingSemicolonDeferringWriter {
    inner: Box<dyn EmitTextWriter>,
    has_pending_semicolon: bool,
}

pub(crate) fn get_trailing_semicolon_deferring_writer(
    writer: Box<dyn EmitTextWriter>,
) -> Box<dyn EmitTextWriter> {
    Box::new(TrailingSemicolonDeferringWriter { inner: writer, has_pending_semicolon: false })
}

impl TrailingSemicolonDeferringWriter {
    fn commit_semicolon(&mut self) {
        if self.has_pending_semicolon {
            self.inner.write_trailing_semicolon(";");
            self.has_pending_semicolon = false;
        }
    }
}

impl EmitTextWriter for TrailingSemicolonDeferringWriter {
    fn write(&mut self, s: &str) {
        self.commit_semicolon();
        self.inner.write(s);
    }

    fn write_trailing_semicolon(&mut self, _text: &str) {
        self.has_pending_semicolon = true;
    }

    fn write_comment(&mut self, text: &str) {
        self.commit_semicolon();
        self.inner.write_comment(text);
    }

    fn write_keyword(&mut self, text: &str) {
        self.commit_semicolon();
        self.inner.write_keyword(text);
    }

    fn write_operator(&mut self, text: &str) {
        self.commit_semicolon();
        self.inner.write_operator(text);
    }

    fn write_punctuation(&mut self, text: &str) {
        self.commit_semicolon();
        self.inner.write_punctuation(text);
    }

    fn write_space(&mut self, text: &str) {
        self.commit_semicolon();
        self.inner.write_space(text);
    }

    fn write_string_literal(&mut self, text: &str) {
        self.commit_semicolon();
        self.inner.write_string_literal(text);
    }

    fn write_parameter(&mut self, text: &str) {
        self.commit_semicolon();
        self.inner.write_parameter(text);
    }

    fn write_property(&mut self, text: &str) {
        self.commit_semicolon();
        self.inner.write_property(text);
    }

    fn write_symbol(&mut self, text: &str, symbol: P<Symbol>) {
        self.commit_semicolon();
        self.inner.write_symbol(text, symbol);
    }

    fn write_line(&mut self) {
        self.commit_semicolon();
        self.inner.write_line();
    }

    fn write_line_force(&mut self, force: bool) {
        self.commit_semicolon();
        self.inner.write_line_force(force);
    }

    fn increase_indent(&mut self) {
        self.commit_semicolon();
        self.inner.increase_indent();
    }

    fn decrease_indent(&mut self) {
        self.commit_semicolon();
        self.inner.decrease_indent();
    }

    fn clear(&mut self) {
        self.has_pending_semicolon = false;
        self.inner.clear();
    }

    fn string(&self) -> String {
        self.inner.string()
    }

    fn take_string(&mut self) -> String {
        self.has_pending_semicolon = false;
        self.inner.take_string()
    }

    fn raw_write(&mut self, s: &str) {
        self.commit_semicolon();
        self.inner.raw_write(s);
    }

    fn write_literal(&mut self, s: &str) {
        self.commit_semicolon();
        self.inner.write_literal(s);
    }

    fn get_text_pos(&self) -> u32 {
        self.inner.get_text_pos()
    }

    fn get_line(&self) -> u32 {
        self.inner.get_line()
    }

    fn get_column(&self) -> UTF16Offset {
        self.inner.get_column()
    }

    fn get_indent(&self) -> u32 {
        self.inner.get_indent()
    }

    fn is_at_start_of_line(&self) -> bool {
        self.inner.is_at_start_of_line()
    }

    fn has_trailing_comment(&self) -> bool {
        self.inner.has_trailing_comment()
    }

    fn has_trailing_whitespace(&self) -> bool {
        self.inner.has_trailing_whitespace()
    }
}
