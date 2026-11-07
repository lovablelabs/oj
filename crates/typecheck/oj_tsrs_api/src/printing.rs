// session.go handlePrintNode / decodePrintNode / newPrinter and handleFormatNodeForInsertion.
//
// Decoded nodes live in the region owned by `tsrs_api_codec::DecodedNode`; printing (and the synthetic file
// and positioned clone of formatNodeForInsertion) allocate inside that region, which is freed with the
// decoded tree once the text has been copied out.

use tsrs_api_codec::{DecodeError, DecodedNode, decode_nodes};
use tsrs_core::json::Value;
use tsrs_printer::{PrintHandlers, PrinterOptions, new_printer};
use tsrs_project::ID as ProjectID;

use crate::batch::base64_decode;
use crate::handler::{ApiError, ApiResult};
use crate::program::resolve_source_file;
use crate::session::Session;
use crate::wire::{Params, s};

/// Go `decodePrintNode`. A Go decoder panic surfaces like the Go server's recovered panic.
fn decode(encoded: &str) -> ApiResult<DecodedNode> {
    let data = base64_decode(encoded)
        .map_err(|e| ApiError::client(format!("invalid base64 data: {e}")))?;
    decode_nodes(&data).map_err(|e| match e {
        DecodeError::GoDecoderPanic(m) => ApiError::internal(format!("panic: {m}")),
        e => ApiError::client(format!("failed to decode AST: {e}")),
    })
}

impl Session {
    #[expect(
        clippy::unused_self,
        reason = "API request handlers share the Session receiver used by the request dispatcher."
    )]
    pub(crate) fn handle_print_node(&self, p: Params) -> ApiResult<Value> {
        let decoded = decode(p.str("data")?)?;
        let options = PrinterOptions {
            preserve_source_newlines: p.bool("preserveSourceNewlines")?,
            never_ascii_escape: p.bool("neverAsciiEscape")?,
            terminate_unterminated_literals: p.bool("terminateUnterminatedLiterals")?,
            ..Default::default()
        };
        let text = {
            let _scope = decoded.region().enter();
            let node = decoded.root();
            let source_file = tsrs_ast::is_source_file(node).then(|| node.as_source_file_p());
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                new_printer(options, PrintHandlers::default(), None).emit(node, source_file)
            }))
        };
        drop(decoded);
        match text {
            Ok(text) => Ok(s(text)),
            Err(panic) => {
                Err(ApiError::internal(format!("panic: {}", go_kind_text(&panic_message(&*panic)))))
            }
        }
    }

    pub(crate) fn handle_format_node_for_insertion(&self, p: Params) -> ApiResult<Value> {
        let sd = self.snapshot_data(p.u64("snapshot")?)?;
        let program = sd.get_program(&ProjectID(p.str("project")?.to_string()))?;
        let target = resolve_source_file(program, &p.document("file")?)?;
        let decoded = decode(p.str("data")?)?;
        let position = u32::try_from(p.u64("position")?)
            .map_err(|_| ApiError::invalid_request("position exceeds uint32"))?;
        let pos = target.get_position_map().utf16_to_utf8(position);
        let format_options = sd.snapshot.user_preferences().format_code_settings.clone();
        let new_line = format_options.new_line_character.clone();
        let text = {
            let _scope = decoded.region().enter();
            let node = decoded.root();
            let factory = tsrs_ast::new_node_factory(tsrs_ast::NodeFactoryHooks::default());
            let (text, node_with_pos) = tsrs_printer::print_and_position_node(
                &factory,
                node,
                None,
                &new_line,
                usize::try_from(format_options.indent_size).expect("indent size is negative"),
                None,
            );
            let synthetic = tsrs_printer::create_synthetic_source_file(
                &factory,
                node_with_pos,
                &text,
                target.parse_options().clone(),
            );
            let at_line_start =
                tsrs_ls::format::get_line_start_position_for_position(pos, target) == pos;
            let initial =
                tsrs_ls::format::get_indentation(pos, target, &format_options, at_line_start);
            let delta = if format_options.indent_size != 0
                && tsrs_ls::format::should_indent_child_node(
                    &format_options,
                    node,
                    None,
                    None,
                    false,
                ) {
                format_options.indent_size
            } else {
                0
            };
            let ctx = tsrs_ls::format::with_format_code_settings(
                &tsrs_ls::format::FormatContext::default(),
                format_options,
                &new_line,
            );
            let changes = tsrs_ls::format::format_node_given_indentation(
                &ctx,
                node_with_pos,
                synthetic,
                target.language_variant(),
                initial,
                delta,
            );
            tsrs_core::apply_bulk_edits(&text, &changes)
        };
        drop(decoded);
        Ok(s(text))
    }
}

fn panic_message(p: &(dyn std::any::Any + Send)) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&str>().map(std::string::ToString::to_string))
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// The printer's panics print a node kind with `{:?}` (`TypeLiteral`); Go's `%v` of `ast.Kind` prints
/// `KindTypeLiteral`. Rewrites a trailing kind name in the recovered message only (printer output itself is
/// unchanged).
#[expect(
    clippy::too_long_first_doc_paragraph,
    reason = "Public for the integration tests; the comment keeps its upstream wording."
)]
pub fn go_kind_text(message: &str) -> String {
    if let Some((head, tail)) = message.rsplit_once(": ") {
        let is_kind_name = !tail.is_empty()
            && tail.chars().next().is_some_and(|c| c.is_ascii_uppercase())
            && tail.chars().all(|c| c.is_ascii_alphanumeric());
        if is_kind_name && !tail.starts_with("Kind") {
            return format!("{head}: Kind{tail}");
        }
    }
    message.to_string()
}
