use tsrs_ast::*;
use tsrs_core::*;

/// Externally opaque interface for printing text
///
/// Go's optional `interface{ Grow(n int) }` (probed by `Printer.Write`) is the defaulted `grow` method.
pub trait EmitTextWriter {
    fn write(&mut self, s: &str);
    fn write_trailing_semicolon(&mut self, text: &str);
    fn write_comment(&mut self, text: &str);
    fn write_keyword(&mut self, text: &str);
    fn write_operator(&mut self, text: &str);
    fn write_punctuation(&mut self, text: &str);
    fn write_space(&mut self, text: &str);
    fn write_string_literal(&mut self, text: &str);
    fn write_parameter(&mut self, text: &str);
    fn write_property(&mut self, text: &str);
    fn write_symbol(&mut self, text: &str, symbol: P<Symbol>);
    fn write_line(&mut self);
    fn write_line_force(&mut self, force: bool);
    fn increase_indent(&mut self);
    fn decrease_indent(&mut self);
    fn clear(&mut self);
    fn string(&self) -> String;
    /// Returns the current output and resets all writer state as `clear()` does.
    fn take_string(&mut self) -> String {
        let text = self.string();
        self.clear();
        text
    }
    fn raw_write(&mut self, s: &str);
    fn write_literal(&mut self, s: &str);
    fn get_text_pos(&self) -> u32;
    fn get_line(&self) -> u32;
    fn get_column(&self) -> UTF16Offset;
    fn get_indent(&self) -> u32;
    fn is_at_start_of_line(&self) -> bool;
    fn has_trailing_comment(&self) -> bool;
    fn has_trailing_whitespace(&self) -> bool;
    fn grow(&mut self, _n: usize) {}
}

// Lets a `Box<dyn EmitTextWriter>` (what `new_text_writer`/`get_single_line_string_writer` return) be passed
// wherever a `&mut dyn EmitTextWriter` is expected.
impl<T: EmitTextWriter + ?Sized> EmitTextWriter for Box<T> {
    fn write(&mut self, s: &str) {
        (**self).write(s);
    }
    fn write_trailing_semicolon(&mut self, text: &str) {
        (**self).write_trailing_semicolon(text);
    }
    fn write_comment(&mut self, text: &str) {
        (**self).write_comment(text);
    }
    fn write_keyword(&mut self, text: &str) {
        (**self).write_keyword(text);
    }
    fn write_operator(&mut self, text: &str) {
        (**self).write_operator(text);
    }
    fn write_punctuation(&mut self, text: &str) {
        (**self).write_punctuation(text);
    }
    fn write_space(&mut self, text: &str) {
        (**self).write_space(text);
    }
    fn write_string_literal(&mut self, text: &str) {
        (**self).write_string_literal(text);
    }
    fn write_parameter(&mut self, text: &str) {
        (**self).write_parameter(text);
    }
    fn write_property(&mut self, text: &str) {
        (**self).write_property(text);
    }
    fn write_symbol(&mut self, text: &str, symbol: P<Symbol>) {
        (**self).write_symbol(text, symbol);
    }
    fn write_line(&mut self) {
        (**self).write_line();
    }
    fn write_line_force(&mut self, force: bool) {
        (**self).write_line_force(force);
    }
    fn increase_indent(&mut self) {
        (**self).increase_indent();
    }
    fn decrease_indent(&mut self) {
        (**self).decrease_indent();
    }
    fn clear(&mut self) {
        (**self).clear();
    }
    fn string(&self) -> String {
        (**self).string()
    }
    fn take_string(&mut self) -> String {
        (**self).take_string()
    }
    fn raw_write(&mut self, s: &str) {
        (**self).raw_write(s);
    }
    fn write_literal(&mut self, s: &str) {
        (**self).write_literal(s);
    }
    fn get_text_pos(&self) -> u32 {
        (**self).get_text_pos()
    }
    fn get_line(&self) -> u32 {
        (**self).get_line()
    }
    fn get_column(&self) -> UTF16Offset {
        (**self).get_column()
    }
    fn get_indent(&self) -> u32 {
        (**self).get_indent()
    }
    fn is_at_start_of_line(&self) -> bool {
        (**self).is_at_start_of_line()
    }
    fn has_trailing_comment(&self) -> bool {
        (**self).has_trailing_comment()
    }
    fn has_trailing_whitespace(&self) -> bool {
        (**self).has_trailing_whitespace()
    }
    fn grow(&mut self, n: usize) {
        (**self).grow(n);
    }
}
