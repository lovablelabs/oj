use tsrs_ast::*;
use tsrs_core::*;

use crate::{EmitContext, EmitTextWriter, PrinterOptions, new_change_tracker_writer, new_printer};

// PrintAndPositionNode prints a synthesized node to text using the standard
// change-tracker printer options, trims the trailing newline, and assigns
// positions to the resulting node tree.
// sourceFile may be nil; when non-nil it is passed to the printer for comment
// preservation.
// The returned text is the printed source, and positioned is the node with
// concrete source positions assigned to it and all descendants.
// syntheticfile.go:17
pub fn print_and_position_node(
    factory: &NodeFactory,
    node: P<Node>,
    source_file: Option<P<SourceFile>>,
    new_line: &str,
    indent_size: usize,
    emit_context: Option<P<EmitContext>>,
) -> (String, P<Node>) {
    let mut writer = new_change_tracker_writer(new_line, Some(indent_size));
    new_printer(
        PrinterOptions {
            new_line: get_new_line_kind(new_line),
            never_ascii_escape: true,
            preserve_source_newlines: true,
            terminate_unterminated_literals: true,
            ..Default::default()
        },
        writer.get_print_handlers(),
        emit_context,
    )
    .write(node, source_file, &mut writer, None);

    let mut text = writer.string();
    if let Some(stripped) = text.strip_suffix(new_line) {
        text = stripped.to_string();
    }
    let positioned = writer.assign_positions_to_node(node, factory);
    (text, positioned)
}

// CreateSyntheticSourceFile wraps a positioned node in a synthetic source file
// suitable for use with the formatter. The node must already have valid source
// positions assigned (e.g. via PrintAndPositionNode or AssignPositionsToNode).
// syntheticfile.go:39
pub fn create_synthetic_source_file(
    factory: &NodeFactory,
    node: P<Node>,
    text: &str,
    parse_options: SourceFileParseOptions,
) -> P<SourceFile> {
    let eof = factory.new_token(Kind::EndOfFile);
    let text_len =
        u32::try_from(text.len()).expect("synthetic source file exceeds supported source length");
    eof.set_loc(TextRange::new(text_len, text_len));
    let statements = factory.new_node_list(vec![node]);
    statements.loc.set(TextRange::from_text_pos(node.pos(), node.end()));
    let synthetic_file = factory.new_source_file(parse_options, alloc_str(text), statements, eof);
    synthetic_file.set_loc(TextRange::new(0, text_len));
    set_parent_in_children(synthetic_file);
    synthetic_file.as_source_file_p()
}
