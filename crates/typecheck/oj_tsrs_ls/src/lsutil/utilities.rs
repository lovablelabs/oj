use tsrs_ast::{self as ast, Kind, Node, NodeFlags, SourceFile, Symbol, TokenFlags};
use tsrs_compiler::Program;
use tsrs_core::stringutil::{push_rune, unicode_to_upper};
use tsrs_core::{P, Tristate, tspath};
use tsrs_scanner as scanner;

use super::*;
use crate::astnav;

// utilities.go:15
pub fn probably_uses_semicolons(file: P<SourceFile>) -> bool {
    fn visit(
        node: P<Node>,
        file: P<SourceFile>,
        with_semicolon: &mut usize,
        without_semicolon: &mut usize,
        n_statements_to_observe: usize,
    ) -> bool {
        if node.flags().intersects(NodeFlags::Reparsed) {
            return false;
        }
        if syntax_requires_trailing_semicolon_or_asi(node.kind()) {
            let last_token = get_last_token(Some(node), file);
            if last_token.is_some_and(|t| t.kind() == Kind::SemicolonToken) {
                *with_semicolon += 1;
            } else {
                *without_semicolon += 1;
            }
        } else if syntax_requires_trailing_comma_or_semicolon_or_asi(node.kind()) {
            let last_token = get_last_token(Some(node), file);
            if last_token.is_some_and(|t| t.kind() == Kind::SemicolonToken) {
                *with_semicolon += 1;
            } else if let Some(last_token) = last_token.filter(|t| t.kind() != Kind::CommaToken) {
                let last_token_line = scanner::get_ecma_line_of_position(
                    file.get(),
                    astnav::get_start_of_node(last_token, file, false /*includeJSDoc*/),
                );
                let next_token_line = scanner::get_ecma_line_of_position(
                    file.get(),
                    scanner::skip_trivia(
                        file.text(),
                        last_token.end().expect_source("parsed tokens have source positions"),
                    ),
                );
                // Avoid counting missing semicolon in single-line objects:
                // `function f(p: { x: string /*no semicolon here is insignificant*/ }) {`
                if last_token_line != next_token_line {
                    *without_semicolon += 1;
                }
            }
        }

        if *with_semicolon + *without_semicolon >= n_statements_to_observe {
            return true;
        }

        node.for_each_child(&mut |n| {
            visit(n, file, with_semicolon, without_semicolon, n_statements_to_observe)
        })
    }

    let mut with_semicolon = 0;
    let mut without_semicolon = 0;
    let n_statements_to_observe = 5;

    file.as_node().for_each_child(&mut |n| {
        visit(n, file, &mut with_semicolon, &mut without_semicolon, n_statements_to_observe)
    });

    // One statement missing a semicolon isn't sufficient evidence to say the user
    // doesn't want semicolons, because they may not even be done writing that statement.
    if with_semicolon == 0 && without_semicolon <= 1 {
        return true;
    }

    // When both kinds of observation exist, treat the file as using semicolons when the
    // ratio withSemicolon/withoutSemicolon exceeds 1/nStatementsToObserve (real arithmetic),
    // implemented as an integer inequality to avoid truncation.
    if without_semicolon == 0 {
        return true;
    }
    with_semicolon * n_statements_to_observe > without_semicolon
}

// utilities.go:77
pub(crate) fn should_use_uri_style_node_core_modules(
    file: P<SourceFile>,
    program: &Program,
) -> Tristate {
    if let Some(result) = file.imports().iter().find_map(|&node| {
        if tsrs_core::node_core_modules().contains(node.text())
            && !tsrs_core::is_exclusively_prefixed_node_core_module(node.text())
        {
            if node.text().starts_with("node:") {
                return Some(Tristate::True);
            }
            return Some(Tristate::False);
        }

        None
    }) {
        return result;
    }

    program.uses_uri_style_node_core_modules()
}

// utilities.go:91
fn quote_preference_from_string(str: P<Node>) -> QuotePreference {
    if str.as_string_literal().token_flags().intersects(TokenFlags::SingleQuote) {
        return QuotePreference::Single;
    }
    QuotePreference::Double
}

// utilities.go:98
pub(crate) fn get_quote_preference(
    source_file: P<SourceFile>,
    preferences: &UserPreferences,
) -> QuotePreference {
    if preferences.quote_preference != QuotePreference::Unknown
        && preferences.quote_preference != QuotePreference::Auto
    {
        if preferences.quote_preference == QuotePreference::Single {
            return QuotePreference::Single;
        }
        return QuotePreference::Double;
    }
    // ignore synthetic import added when importHelpers: true
    let first_module_specifier = source_file
        .imports()
        .iter()
        .copied()
        .find(|&n| ast::is_string_literal(n) && !ast::node_is_synthesized(n.parent().unwrap()));
    if let Some(first_module_specifier) = first_module_specifier {
        return quote_preference_from_string(first_module_specifier);
    }
    QuotePreference::Double
}

// utilities.go:115
pub(crate) fn module_symbol_to_valid_identifier(
    module_symbol: P<Symbol>,
    force_capitalize: bool,
) -> String {
    let mut module_name = module_symbol.name();
    if let Some(ambient_module_name) =
        ast::try_get_ambient_module_name_from_symbol_name(module_name)
    {
        module_name = ambient_module_name;
    }
    module_specifier_to_valid_identifier(module_name, force_capitalize)
}

// utilities.go:123
pub fn module_specifier_to_valid_identifier(
    module_specifier: &str,
    force_capitalize: bool,
) -> String {
    let without_ext = tspath::remove_any_file_extension(module_specifier);
    let base_name =
        tspath::get_base_file_name(without_ext.strip_suffix("/index").unwrap_or(without_ext));
    let mut res = String::new();
    let mut last_char_was_valid = true;
    let base_name_runes: Vec<u32> = base_name.chars().map(|c| c as u32).collect();
    if !base_name_runes.is_empty() && scanner::is_identifier_start(base_name_runes[0]) {
        if force_capitalize {
            push_rune(&mut res, unicode_to_upper(base_name_runes[0]));
        } else {
            push_rune(&mut res, base_name_runes[0]);
        }
    } else {
        last_char_was_valid = false;
    }

    for &r in base_name_runes.iter().skip(1) {
        let is_valid = scanner::is_identifier_part(r);
        if is_valid {
            if last_char_was_valid {
                push_rune(&mut res, r);
            } else {
                push_rune(&mut res, unicode_to_upper(r));
            }
        }
        last_char_was_valid = is_valid;
    }

    // Need `"_"` to ensure result isn't empty.
    if !res.is_empty() && !is_non_contextual_keyword(scanner::string_to_token(&res)) {
        return res;
    }
    format!("_{res}")
}

// utilities.go:158
pub(crate) fn is_non_contextual_keyword(token: Kind) -> bool {
    ast::is_keyword_kind(token) && !ast::is_contextual_keyword(token)
}
