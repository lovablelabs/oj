use tsrs_ast::{self as ast, Kind, Node, NodeFlags};
use tsrs_core::P;
use tsrs_core::context::Context;
use tsrs_lsproto as lsproto;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::completions::escape_snippet_text;
use crate::languageservice::LanguageService;
use crate::spanmap::Feature;

impl LanguageService {
    // autoinsert.go:13
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    pub fn provide_on_auto_insert(
        &self,
        _ctx: &Context,
        params: &lsproto::VSOnAutoInsertParams,
    ) -> Result<lsproto::VSOnAutoInsertResponse, lsproto::Error> {
        if self.user_preferences().enable_auto_closing_tags.is_false() {
            return Ok(lsproto::VSOnAutoInsertResponse::default());
        }
        if params.vs_ch != ">" {
            return Ok(lsproto::VSOnAutoInsertResponse::default());
        }

        let (_, mut source_file) = self.get_program_and_file(&params.vs_text_document.uri);
        let positions = self.converters.from_lsp_position_for_source_file(
            source_file,
            params.vs_position,
            Feature::AutoInsert,
        );
        if positions.len() != 1 || !positions[0].fidelity.is_exact() {
            return Ok(lsproto::VSOnAutoInsertResponse::default());
        }
        source_file = positions[0].script;
        let position = positions[0].position;

        let Some(token) = astnav::find_preceding_token(source_file, position) else {
            return Ok(lsproto::VSOnAutoInsertResponse::default());
        };

        let mut closing_text = String::new();
        let mut element: Option<P<Node>> = None;
        if token.kind() == Kind::GreaterThanToken
            && ast::is_jsx_opening_element(token.parent().unwrap())
        {
            element = token.parent().unwrap().parent();
        } else if ast::is_jsx_text(token) && ast::is_jsx_element(token.parent().unwrap()) {
            element = token.parent();
        }

        if let Some(element) = element
            && is_unclosed_tag(element)
        {
            let tag_name_node = element.as_jsx_element().opening_element.tag_name();
            // Slight divergence from Strada - we don't use the verbatim text from the opening tag.
            closing_text = format!(
                "</{}>",
                ast::entity_name_to_string(tag_name_node, Some(&scanner::get_text_of_node))
            );
        } else {
            let mut fragment: Option<P<Node>> = None;
            if token.kind() == Kind::GreaterThanToken
                && ast::is_jsx_opening_fragment(token.parent().unwrap())
            {
                fragment = token.parent().unwrap().parent();
            } else if ast::is_jsx_text(token) && ast::is_jsx_fragment(token.parent().unwrap()) {
                fragment = token.parent();
            }

            if fragment.is_some() && is_unclosed_fragment(fragment.unwrap()) {
                closing_text = "</>".to_string();
            }
        }

        if closing_text.is_empty() {
            return Ok(lsproto::VSOnAutoInsertResponse::default());
        }

        Ok(lsproto::VSOnAutoInsertResponse {
            vs_on_auto_insert_response_item: Some(lsproto::VSOnAutoInsertResponseItem {
                vs_text_edit_format: lsproto::InsertTextFormat::SNIPPET,
                vs_text_edit: lsproto::TextEdit {
                    range: lsproto::Range { start: params.vs_position, end: params.vs_position },
                    // Tag names can contain `$` (valid JSX identifier characters), so
                    // escape the closing text to avoid being interpreted as a snippet
                    // placeholder/variable.
                    new_text: "$0".to_string() + &escape_snippet_text(&closing_text),
                },
            }),
        })
    }
}

// autoinsert.go:77
fn is_unclosed_tag(node: P<Node>) -> bool {
    let opening_element = node.as_jsx_element().opening_element;
    let closing_element = node.as_jsx_element().closing_element;
    if !ast::tag_names_are_equivalent(opening_element.tag_name(), closing_element.tag_name()) {
        return true;
    }

    let parent = node.parent().unwrap();
    if ast::is_jsx_element(parent) {
        return ast::tag_names_are_equivalent(
            opening_element.tag_name(),
            parent.as_jsx_element().opening_element.tag_name(),
        ) && is_unclosed_tag(parent);
    }

    false
}

// autoinsert.go:93
fn is_unclosed_fragment(node: P<Node>) -> bool {
    let closing_fragment = node.as_jsx_fragment().closing_fragment;
    if closing_fragment.flags().intersects(NodeFlags::ThisNodeHasError) {
        return true;
    }

    let parent = node.parent().unwrap();
    if ast::is_jsx_fragment(parent) && is_unclosed_fragment(parent) {
        return true;
    }

    false
}
