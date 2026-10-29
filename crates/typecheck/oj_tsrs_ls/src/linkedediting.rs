use tsrs_ast::{self as ast, Kind, NodeFlags};
use tsrs_core::TextRange;
use tsrs_core::context::Context;
use tsrs_lsproto as lsproto;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::languageservice::LanguageService;
use crate::spanmap::Feature;

// allow the client to match more than valid tag names. This allows linked editing when typing is in progress or tag name is incomplete
// linkedediting.go:16
const JSX_TAG_WORD_PATTERN: &str = "[a-zA-Z0-9:\\-\\._$]*";

impl LanguageService {
    // linkedediting.go:18
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    pub fn provide_linked_editing_range(
        &self,
        _ctx: &Context,
        params: &lsproto::LinkedEditingRangeParams,
    ) -> Result<lsproto::LinkedEditingRangeResponse, lsproto::Error> {
        let (_, mut source_file) = self.get_program_and_file(&params.text_document.uri);
        let positions = self.converters.from_lsp_position_for_source_file(
            source_file,
            params.position,
            Feature::LinkedEditing,
        );
        if positions.len() != 1 || !positions[0].fidelity.is_exact() {
            return Ok(lsproto::LinkedEditingRangeResponse::default());
        }
        source_file = positions[0].script;
        let position = positions[0].position;
        let token = astnav::find_preceding_token(source_file, position);

        let Some(token) = token.filter(|t| t.parent().unwrap().kind() != Kind::SourceFile) else {
            return Ok(lsproto::LinkedEditingRangeResponse::default());
        };

        if ast::is_jsx_fragment(token.parent().unwrap().parent().unwrap()) {
            let fragment = token.parent().unwrap().parent().unwrap().as_jsx_fragment();
            let open_fragment = fragment.opening_fragment;
            let close_fragment = fragment.closing_fragment;
            if open_fragment.flags().intersects(NodeFlags::ThisNodeOrAnySubNodesHasError)
                || close_fragment.flags().intersects(NodeFlags::ThisNodeOrAnySubNodesHasError)
            {
                return Ok(lsproto::LinkedEditingRangeResponse::default());
            }

            let open_pos = astnav::get_start_of_node(open_fragment, source_file, false)
                + u32::try_from("<".len()).expect("fragment delimiter length exceeds u32");
            let close_pos = astnav::get_start_of_node(close_fragment, source_file, false)
                + u32::try_from("</".len()).expect("fragment delimiter length exceeds u32");

            // only allows linked editing right after opening bracket: <| ></| >
            if position != open_pos && position != close_pos {
                return Ok(lsproto::LinkedEditingRangeResponse::default());
            }

            let (open_line_char, open_fidelity) = self.converters.to_lsp_position_for_feature(
                &source_file,
                open_pos.into(),
                Feature::LinkedEditing,
            );
            let (close_line_char, close_fidelity) = self.converters.to_lsp_position_for_feature(
                &source_file,
                close_pos.into(),
                Feature::LinkedEditing,
            );
            if !open_fidelity.is_exact() || !close_fidelity.is_exact() {
                return Ok(lsproto::LinkedEditingRangeResponse::default());
            }
            Ok(lsproto::LinkedEditingRangeResponse {
                linked_editing_ranges: Some(lsproto::LinkedEditingRanges {
                    ranges: vec![
                        lsproto::Range { start: open_line_char, end: open_line_char }, // only return start position for opening tag since the length of a fragment is always 3 and it is unlikely user will type in the middle of a fragment tag
                        lsproto::Range { start: close_line_char, end: close_line_char },
                    ],
                    word_pattern: Some(JSX_TAG_WORD_PATTERN.to_string()),
                }),
            })
        } else {
            // determines if the cursor is in an element tag
            let tag = ast::find_ancestor(token.parent(), |n| {
                ast::is_jsx_opening_element(n) || ast::is_jsx_closing_element(n)
            });
            let Some(tag) = tag else {
                return Ok(lsproto::LinkedEditingRangeResponse::default());
            };
            assert!(
                ast::is_jsx_opening_element(tag) || ast::is_jsx_closing_element(tag),
                "tag should be opening or closing element"
            );

            let jsx_element = tag.parent().unwrap().as_jsx_element();
            let open_tag = jsx_element.opening_element;
            let close_tag = jsx_element.closing_element;

            let open_tag_name_start =
                astnav::get_start_of_node(open_tag.tag_name(), source_file, false);
            let open_tag_name_end = open_tag
                .tag_name()
                .end()
                .expect_source("parsed JSX tag names have source positions");
            let close_tag_name_start =
                astnav::get_start_of_node(close_tag.tag_name(), source_file, false);
            let close_tag_name_end = close_tag
                .tag_name()
                .end()
                .expect_source("parsed JSX tag names have source positions");
            // do not return linked cursors if tags are not well-formed
            if open_tag_name_start == astnav::get_start_of_node(open_tag, source_file, false)
                || close_tag_name_start == astnav::get_start_of_node(close_tag, source_file, false)
                || open_tag_name_end
                    == open_tag.end().expect_source("parsed JSX tags have source positions")
                || close_tag_name_end
                    == close_tag.end().expect_source("parsed JSX tags have source positions")
            {
                return Ok(lsproto::LinkedEditingRangeResponse::default());
            }
            // only return linked cursors if the cursor is within a tag name
            if !(open_tag_name_start <= position && position <= open_tag_name_end
                || close_tag_name_start <= position && position <= close_tag_name_end)
            {
                return Ok(lsproto::LinkedEditingRangeResponse::default());
            }

            // only return linked cursors if text in both tags is identical
            let opening_tag_text = scanner::get_text_of_node(open_tag.tag_name());
            if opening_tag_text != scanner::get_text_of_node(close_tag.tag_name()) {
                return Ok(lsproto::LinkedEditingRangeResponse::default());
            }

            let (open_range, open_fidelity) = self.converters.to_lsp_range_for_feature(
                &source_file,
                TextRange::new(open_tag_name_start, open_tag_name_end),
                Feature::LinkedEditing,
            );
            let (close_range, close_fidelity) = self.converters.to_lsp_range_for_feature(
                &source_file,
                TextRange::new(close_tag_name_start, close_tag_name_end),
                Feature::LinkedEditing,
            );
            if !open_fidelity.is_exact() || !close_fidelity.is_exact() {
                return Ok(lsproto::LinkedEditingRangeResponse::default());
            }

            Ok(lsproto::LinkedEditingRangeResponse {
                linked_editing_ranges: Some(lsproto::LinkedEditingRanges {
                    ranges: vec![open_range, close_range],
                    word_pattern: Some(JSX_TAG_WORD_PATTERN.to_string()),
                }),
            })
        }
    }
}
