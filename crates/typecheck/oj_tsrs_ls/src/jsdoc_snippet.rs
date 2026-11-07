use std::fmt::Write as _;
use tsrs_ast::{self as ast, Kind, Node, SourceFile};
use tsrs_core::context::Context;
use tsrs_core::{P, stringutil};
use tsrs_diagnostics as diagnostics;
use tsrs_lsproto as lsproto;
use tsrs_parser as parser;

use crate::astnav;
use crate::completions::*;
use crate::format;
use crate::languageservice::LanguageService;

// jsdoc_snippet.go:19
struct DocCommentTemplate {
    new_text: String,
}

// jsdoc_snippet.go:23
struct CommentOwnerInfo {
    comment_owner: P<Node>,
    parameters: &'static [P<Node>],
    has_return: bool,
}

impl LanguageService {
    // jsdoc_snippet.go:29
    pub(crate) fn get_jsdoc_snippet_completion(
        &self,
        ctx: &Context,
        file: P<SourceFile>,
        position: u32,
    ) -> Option<CompletionList> {
        if self.user_preferences().enable_jsdoc_completions.is_false() {
            return None;
        }
        if !is_potentially_valid_jsdoc_snippet_completion_position(file, position) {
            return None;
        }
        let mut new_line = self.format_options().new_line_character.clone();
        if new_line.is_empty() {
            new_line = "\n".to_string();
        }
        let template = get_doc_comment_template_at_position(
            file,
            position,
            self.user_preferences().generate_return_in_doc_template.is_true(),
            &new_line,
        )?;

        let mut insert_text = template.new_text;
        let mut insert_text_format: Option<lsproto::InsertTextFormat> = None;
        if client_supports_item_snippet(ctx) {
            insert_text = template_to_snippet(&insert_text, &new_line);
            insert_text_format = Some(lsproto::InsertTextFormat::SNIPPET);
        }

        let edit_range = self.get_jsdoc_snippet_completion_range(ctx, file, position, &insert_text);
        let mut commit_characters: Option<Vec<String>> = None;
        if client_supports_item_commit_characters(ctx) {
            commit_characters = Some(Vec::new());
        }
        let item = CompletionItem::new(lsproto::CompletionItem {
            label: "/** */".to_string(),
            kind: Some(lsproto::CompletionItemKind::TEXT),
            // English only (docs/LSP.md): Go localizes with locale.FromContext(ctx).
            detail: Some(diagnostics::JSDOC_COMMENT.localize(&[])),
            sort_text: Some("\x00".to_string()),
            insert_text_format,
            text_edit: edit_range,
            commit_characters,
            ..Default::default()
        });
        Some(CompletionList { is_incomplete: false, items: vec![item], ..Default::default() })
    }
}

// jsdoc_snippet.go:74
pub fn is_potentially_valid_jsdoc_snippet_completion_position(
    file: P<SourceFile>,
    position: u32,
) -> bool {
    let text = file.text();
    let line_start = format::get_line_start_position_for_position(position, file);
    let prefix = &text[line_start as usize..position as usize];
    if !is_jsdoc_snippet_prefix(prefix) {
        return false;
    }

    let line_end = get_line_end_of_position(file, position);
    let suffix = &text[position as usize..line_end as usize];
    is_jsdoc_snippet_suffix(suffix)
}

impl LanguageService {
    // jsdoc_snippet.go:87
    fn get_jsdoc_snippet_completion_range(
        &self,
        ctx: &Context,
        file: P<SourceFile>,
        position: u32,
        new_text: &str,
    ) -> Option<lsproto::TextEditOrInsertReplaceEdit> {
        let text = file.text();
        let line_start = format::get_line_start_position_for_position(position, file);
        let prefix = &text[line_start as usize..position as usize];
        let mut start = position;
        if let Some(prefix_start) = get_jsdoc_snippet_prefix_start(prefix) {
            start =
                line_start + u32::try_from(prefix_start).expect("JSDoc prefix length exceeds u32");
        }

        let line_end = get_line_end_of_position(file, position);
        let suffix = &text[position as usize..line_end as usize];
        let mut end = position;
        if let (suffix_end, true) = get_jsdoc_snippet_suffix_end(suffix) {
            end += u32::try_from(suffix_end).expect("JSDoc suffix length exceeds u32");
        }

        let (replacement_range, fidelity) = self.create_lsp_range_from_bounds(start, end, file);
        if !fidelity.is_exact() {
            return None;
        }
        if client_supports_item_insert_replace(ctx) {
            return Some(lsproto::TextEditOrInsertReplaceEdit {
                insert_replace_edit: Some(lsproto::InsertReplaceEdit {
                    new_text: new_text.to_string(),
                    insert: replacement_range,
                    replace: replacement_range,
                }),
                ..Default::default()
            });
        }
        Some(lsproto::TextEditOrInsertReplaceEdit {
            text_edit: Some(lsproto::TextEdit {
                new_text: new_text.to_string(),
                range: replacement_range,
            }),
            ..Default::default()
        })
    }
}

// jsdoc_snippet.go:124
fn get_doc_comment_template_at_position(
    source_file: P<SourceFile>,
    position: u32,
    generate_return_in_doc_template: bool,
    new_line: &str,
) -> Option<DocCommentTemplate> {
    // Go checks the token for nil; GetTokenAtPosition never returns nil.
    let mut token_at_pos = astnav::get_token_at_position(source_file, position);

    let existing_doc_comment = ast::find_ancestor(token_at_pos, ast::is_jsdoc);
    let (doc_comment_end, has_doc_comment_at_position, has_closing_doc_comment_at_position) =
        get_doc_comment_end_at_position(source_file, position);
    let is_in_empty_doc_comment = existing_doc_comment.is_some() || has_doc_comment_at_position;
    if is_non_empty_jsdoc(existing_doc_comment)
        && has_doc_comment_at_position
        && !has_closing_doc_comment_at_position
    {
        let reparse_text = source_file.text()[..position as usize].to_string()
            + " */"
            + &source_file.text()[position as usize..];
        let reparse = parser::parse_source_file(
            source_file.parse_options().clone(),
            &reparse_text,
            source_file.script_kind(),
        );
        return get_doc_comment_template_at_position(
            reparse,
            position,
            generate_return_in_doc_template,
            new_line,
        );
    }
    if is_non_empty_jsdoc(existing_doc_comment) {
        return None;
    }
    if existing_doc_comment.is_none() && has_doc_comment_at_position {
        token_at_pos = astnav::get_token_at_position(
            source_file,
            skip_whitespace(source_file.text(), doc_comment_end),
        );
    }
    let token_start =
        astnav::get_start_of_node(token_at_pos, source_file, false /*includeJSDoc*/);
    if !is_in_empty_doc_comment && token_start < position {
        return None;
    }

    let comment_owner_info = get_comment_owner_info(token_at_pos, generate_return_in_doc_template)?;

    let comment_owner = comment_owner_info.comment_owner;
    let last_js_doc = comment_owner.jsdoc(Some(source_file.get())).last().copied();
    let comment_owner_start =
        astnav::get_start_of_node(comment_owner, source_file, false /*includeJSDoc*/);
    if comment_owner_start < position
        || last_js_doc.is_some()
            && existing_doc_comment.is_some()
            && last_js_doc != existing_doc_comment
    {
        return None;
    }

    let indentation = get_indentation_string_at_position(source_file, position);
    let mut tags = parameter_doc_comments(
        comment_owner_info.parameters,
        ast::is_source_file_js(source_file),
        &indentation,
        new_line,
    );
    if comment_owner_info.has_return {
        tags += &returns_doc_comment(&indentation, new_line);
    }

    if !tags.is_empty() && !has_jsdoc_tags(comment_owner, source_file) {
        let preamble = format!("/**{new_line}{indentation} * ");
        let mut end_line = String::new();
        if token_start == position {
            end_line = format!("{new_line}{indentation}");
        }
        return Some(DocCommentTemplate {
            new_text: format!("{preamble}{new_line}{tags}{indentation} */{end_line}"),
        });
    }
    Some(DocCommentTemplate { new_text: "/** */".to_string() })
}

// jsdoc_snippet.go:181
fn get_doc_comment_end_at_position(file: P<SourceFile>, position: u32) -> (u32, bool, bool) {
    let text = file.text();
    let line_start = format::get_line_start_position_for_position(position, file);
    let line_end = get_line_end_of_position(file, position);
    let prefix = &text[line_start as usize..position as usize];
    let suffix = &text[position as usize..line_end as usize];
    if !trim_right_single_line_whitespace(prefix).ends_with("/**") {
        return (0, false, false);
    }
    let (suffix_end, has_closing) = get_jsdoc_snippet_suffix_end(suffix);
    (
        position + u32::try_from(suffix_end).expect("JSDoc suffix length exceeds u32"),
        true,
        has_closing,
    )
}

// jsdoc_snippet.go:194
fn skip_whitespace(text: &str, position: u32) -> u32 {
    let mut position = position as usize;
    while position < text.len() {
        let (ch, size) = stringutil::decode_js_string_rune(&text[position..]);
        if size == 0 {
            break;
        }
        if !stringutil::is_white_space_like(ch) {
            break;
        }
        position += size;
    }
    u32::try_from(position).expect("source position exceeds u32")
}

// jsdoc_snippet.go:208
fn get_comment_owner_info(
    token_at_pos: P<Node>,
    generate_return_in_doc_template: bool,
) -> Option<CommentOwnerInfo> {
    let mut node = Some(token_at_pos);
    while let Some(n) = node {
        let (info, quit) = get_comment_owner_info_worker(Some(n), generate_return_in_doc_template);
        if info.is_some() || quit {
            return info;
        }
        node = n.parent();
    }
    None
}

// jsdoc_snippet.go:218
fn get_comment_owner_info_worker(
    comment_owner: Option<P<Node>>,
    generate_return_in_doc_template: bool,
) -> (Option<CommentOwnerInfo>, bool) {
    let Some(comment_owner) = comment_owner else {
        return (None, false);
    };
    let plain = |comment_owner: P<Node>| CommentOwnerInfo {
        comment_owner,
        parameters: &[],
        has_return: false,
    };
    match comment_owner.kind() {
        Kind::FunctionDeclaration
        | Kind::FunctionExpression
        | Kind::MethodDeclaration
        | Kind::Constructor
        | Kind::MethodSignature
        | Kind::ArrowFunction => (
            Some(CommentOwnerInfo {
                comment_owner,
                parameters: comment_owner.parameters(),
                has_return: has_return(comment_owner, generate_return_in_doc_template),
            }),
            false,
        ),
        Kind::PropertyAssignment => get_comment_owner_info_worker(
            Some(comment_owner.as_property_assignment().initializer()),
            generate_return_in_doc_template,
        ),
        Kind::ClassDeclaration
        | Kind::InterfaceDeclaration
        | Kind::EnumDeclaration
        | Kind::EnumMember
        | Kind::TypeAliasDeclaration => (Some(plain(comment_owner)), false),
        Kind::PropertySignature => {
            if let Some(type_node) = comment_owner.as_property_signature_declaration().type_()
                && ast::is_function_type_node(type_node)
            {
                return (
                    Some(CommentOwnerInfo {
                        comment_owner,
                        parameters: type_node.parameters(),
                        has_return: has_return(type_node, generate_return_in_doc_template),
                    }),
                    false,
                );
            }
            (Some(plain(comment_owner)), false)
        }
        Kind::VariableStatement => {
            let declarations = comment_owner
                .as_variable_statement()
                .declaration_list
                .as_variable_declaration_list()
                .declarations
                .nodes();
            if declarations.len() == 1
                && let Some(initializer) = declarations[0].as_variable_declaration().initializer()
                && let Some(host) = get_right_hand_side_of_assignment(Some(initializer))
            {
                return (
                    Some(CommentOwnerInfo {
                        comment_owner,
                        parameters: host.parameters(),
                        has_return: has_return(host, generate_return_in_doc_template),
                    }),
                    false,
                );
            }
            (Some(plain(comment_owner)), false)
        }
        Kind::SourceFile => (None, true),
        Kind::ModuleDeclaration => {
            if comment_owner.parent().unwrap().kind() == Kind::ModuleDeclaration {
                return (None, false);
            }
            (Some(plain(comment_owner)), false)
        }
        Kind::ExpressionStatement => get_comment_owner_info_worker(
            Some(comment_owner.as_expression_statement().expression),
            generate_return_in_doc_template,
        ),
        Kind::BinaryExpression => {
            let binary_expression = comment_owner.as_binary_expression();
            if ast::get_assignment_declaration_kind(comment_owner) == ast::JSDeclarationKind::None {
                return (None, true);
            }
            let right = binary_expression.right();
            if ast::is_function_like(right) {
                return (
                    Some(CommentOwnerInfo {
                        comment_owner,
                        parameters: right.parameters(),
                        has_return: has_return(right, generate_return_in_doc_template),
                    }),
                    false,
                );
            }
            (Some(plain(comment_owner)), false)
        }
        Kind::PropertyDeclaration => {
            if let Some(initializer) = comment_owner.as_property_declaration().initializer()
                && ast::is_function_expression_or_arrow_function(initializer)
            {
                return (
                    Some(CommentOwnerInfo {
                        comment_owner,
                        parameters: initializer.parameters(),
                        has_return: has_return(initializer, generate_return_in_doc_template),
                    }),
                    false,
                );
            }
            (None, false)
        }
        _ => (None, false),
    }
}

// jsdoc_snippet.go:270
fn has_return(node: P<Node>, generate_return_in_doc_template: bool) -> bool {
    if !generate_return_in_doc_template {
        return false;
    }
    if ast::is_function_type_node(node) {
        return true;
    }
    if ast::is_arrow_function(node)
        && let Some(body) = node.body()
        && ast::is_expression(body)
    {
        return true;
    }
    ast::is_function_like_declaration(node)
        && node.body().is_some()
        && ast::is_block(node.body().unwrap())
        && ast::for_each_return_statement(node.body().unwrap(), |_| true)
}

// jsdoc_snippet.go:287
fn get_right_hand_side_of_assignment(right_hand_side: Option<P<Node>>) -> Option<P<Node>> {
    let mut right_hand_side = right_hand_side?;
    while right_hand_side.kind() == Kind::ParenthesizedExpression {
        right_hand_side = right_hand_side.as_parenthesized_expression().expression();
    }
    match right_hand_side.kind() {
        Kind::FunctionExpression | Kind::ArrowFunction => Some(right_hand_side),
        Kind::ClassExpression => {
            right_hand_side.members().iter().copied().find(|&m| ast::is_constructor_declaration(m))
        }
        _ => None,
    }
}

// jsdoc_snippet.go:304
fn parameter_doc_comments(
    parameters: &[P<Node>],
    is_java_script_file: bool,
    indentation: &str,
    new_line: &str,
) -> String {
    let mut b = String::new();
    for (i, &parameter) in parameters.iter().enumerate() {
        let mut param_name = format!("param{i}");
        if ast::is_identifier(parameter.name().unwrap()) {
            param_name = parameter.name().unwrap().text().to_string();
        }
        let mut param_type = "";
        if is_java_script_file {
            if parameter.as_parameter_declaration().dot_dot_dot_token().is_some() {
                param_type = "{...any} ";
            } else {
                param_type = "{any} ";
            }
        }
        b.push_str(indentation);
        b.push_str(" * @param ");
        b.push_str(param_type);
        b.push_str(&param_name);
        b.push_str(new_line);
    }
    b
}

// jsdoc_snippet.go:328
fn returns_doc_comment(indentation: &str, new_line: &str) -> String {
    format!("{indentation} * @returns{new_line}")
}

// jsdoc_snippet.go:332
fn get_indentation_string_at_position(source_file: P<SourceFile>, position: u32) -> String {
    let text = source_file.text();
    let line_start = format::get_line_start_position_for_position(position, source_file) as usize;
    let mut pos = line_start;
    while pos < position as usize {
        let (ch, size) = stringutil::decode_js_string_rune(&text[pos..]);
        if size == 0 {
            break;
        }
        if !stringutil::is_white_space_single_line(ch) {
            break;
        }
        pos += size;
    }
    text[line_start..pos].to_string()
}

// jsdoc_snippet.go:349
fn is_non_empty_jsdoc(jsdoc: Option<P<Node>>) -> bool {
    let Some(jsdoc) = jsdoc else {
        return false;
    };
    let data = jsdoc.as_jsdoc();
    !data.comment.nodes().is_empty() || data.tags.is_some_and(|t| !t.nodes().is_empty())
}

// jsdoc_snippet.go:357
fn has_jsdoc_tags(node: P<Node>, file: P<SourceFile>) -> bool {
    let jsdocs = node.jsdoc(Some(file.get()));
    if jsdocs.is_empty() {
        return false;
    }
    let tags = jsdocs[jsdocs.len() - 1].as_jsdoc().tags;
    tags.is_some_and(|t| !t.nodes().is_empty())
}

// jsdoc_snippet.go:366
fn template_to_snippet(template: &str, new_line: &str) -> String {
    if template == "/** */" {
        return format!("/**{new_line} * $0{new_line} */");
    }

    let mut snippet_index = 1;
    let template = escape_snippet_text(template);
    let template = strip_jsdoc_template_indentation(&template, new_line);
    transform_jsdoc_template_lines(&template, new_line, &mut snippet_index)
}

// jsdoc_snippet.go:377
fn strip_jsdoc_template_indentation(template: &str, new_line: &str) -> String {
    let mut lines: Vec<String> =
        template.split(new_line).map(std::string::ToString::to_string).collect();
    for line in &mut lines {
        let trimmed = line.trim_start_matches([' ', '\t']).to_string();
        if trimmed.starts_with('/') {
            *line = trimmed;
        } else if trimmed.starts_with('*') {
            *line = format!(" {trimmed}");
        }
    }
    lines.join(new_line)
}

// jsdoc_snippet.go:390
fn transform_jsdoc_template_lines(
    template: &str,
    new_line: &str,
    snippet_index: &mut usize,
) -> String {
    let mut lines: Vec<String> =
        template.split(new_line).map(std::string::ToString::to_string).collect();
    for i in 0..lines.len() {
        if i > 0 && lines[i - 1].starts_with("/**") && line_has_only_jsdoc_asterisk(&lines[i]) {
            lines[i] = format!("{}$0", lines[i]);
            continue;
        }
        if let Some(transformed) = transform_jsdoc_param_line(&lines[i], snippet_index) {
            lines[i] = transformed;
            continue;
        }
        if let Some(transformed) = transform_jsdoc_returns_line(&lines[i], snippet_index) {
            lines[i] = transformed;
        }
    }
    lines.join(new_line)
}

// jsdoc_snippet.go:408
fn line_has_only_jsdoc_asterisk(line: &str) -> bool {
    let line = line.trim_start_matches([' ', '\t']);
    line.starts_with('*') && is_only_spaces_or_tabs(&line[1..])
}

// jsdoc_snippet.go:413
fn transform_jsdoc_param_line(line: &str, snippet_index: &mut usize) -> Option<String> {
    let mut prefix = "";
    let mut rest = line;
    if rest.starts_with(' ') {
        prefix = " ";
        rest = &rest[1..];
    }
    if !rest.starts_with("* @param") {
        return None;
    }
    rest = &rest["* @param".len()..];
    if !starts_with_single_line_whitespace(rest) {
        return None;
    }
    rest = rest.trim_start_matches([' ', '\t']);

    let mut type_text = String::new();
    if rest.starts_with('{') {
        let close_brace = rest.find('}')?;
        type_text = format!(" {}", &rest[..=close_brace]);
        rest = &rest[close_brace + 1..];
        if !starts_with_single_line_whitespace(rest) {
            return None;
        }
        rest = rest.trim_start_matches([' ', '\t']);
    }

    let (param_name, rest) = scan_non_whitespace(rest)?;
    if !is_only_spaces_or_tabs(rest) {
        return None;
    }

    let mut out = format!("{prefix}* @param ");
    if type_text == " {any}" || type_text == " {*}" {
        let _ = write!(out, "{{${{{}:*}}}} ", *snippet_index);
        *snippet_index += 1;
    } else if !type_text.is_empty() {
        let _ = write!(out, "{type_text} ");
    }
    let _ = write!(out, "{} ${{{}}}", param_name, *snippet_index);
    *snippet_index += 1;
    Some(out)
}

// jsdoc_snippet.go:460
fn transform_jsdoc_returns_line(line: &str, snippet_index: &mut usize) -> Option<String> {
    let mut prefix = "";
    let mut rest = line;
    if rest.starts_with(' ') {
        prefix = " ";
        rest = &rest[1..];
    }
    if !rest.starts_with("* @returns") || !is_only_spaces_or_tabs(&rest["* @returns".len()..]) {
        return None;
    }
    let text = format!("{}* @returns ${{{}}}", prefix, *snippet_index);
    *snippet_index += 1;
    Some(text)
}

// jsdoc_snippet.go:475
fn scan_non_whitespace(text: &str) -> Option<(&str, &str)> {
    if text.is_empty() {
        return None;
    }
    let mut i = 0;
    while i < text.len() {
        let (ch, size) = stringutil::decode_js_string_rune(&text[i..]);
        if size == 0 || stringutil::is_white_space_like(ch) {
            if i == 0 {
                return None;
            }
            return Some((&text[..i], &text[i..]));
        }
        i += size;
    }
    Some((text, ""))
}

// jsdoc_snippet.go:492
fn is_jsdoc_snippet_prefix(prefix: &str) -> bool {
    let trimmed = trim_right_single_line_whitespace(prefix);
    if trimmed.ends_with("/**") {
        return true;
    }
    let start = skip_single_line_whitespace(prefix, 0);
    let trimmed = trimmed.as_bytes();
    if start >= trimmed.len() || trimmed[start] != b'/' {
        return false;
    }
    if start + 3 > trimmed.len() {
        return false;
    }
    if trimmed[start + 1..].iter().any(|&b| b != b'*') {
        return false;
    }
    trimmed.len() - start >= 3
}

// jsdoc_snippet.go:512
fn get_jsdoc_snippet_prefix_start(prefix: &str) -> Option<usize> {
    let trimmed = trim_right_single_line_whitespace(prefix).as_bytes();
    for i in (0..trimmed.len()).rev() {
        if trimmed[i] != b'*' {
            break;
        }
        if i > 0 && trimmed[i - 1] == b'/' {
            return Some(i - 1);
        }
    }
    if trimmed.ends_with(b"/") {
        return Some(trimmed.len() - 1);
    }
    None
}

// jsdoc_snippet.go:525
fn is_jsdoc_snippet_suffix(suffix: &str) -> bool {
    let trimmed =
        trim_right_single_line_whitespace(&suffix[skip_single_line_whitespace(suffix, 0)..]);
    if trimmed.is_empty() {
        return true;
    }
    if !trimmed.ends_with('/') {
        return false;
    }
    let bytes = trimmed.as_bytes();
    !(bytes[..bytes.len() - 1].iter().any(|&b| b != b'*'))
}

// jsdoc_snippet.go:541 (Go `(int, bool)`)
fn get_jsdoc_snippet_suffix_end(suffix: &str) -> (usize, bool) {
    let bytes = suffix.as_bytes();
    let mut pos = skip_single_line_whitespace(suffix, 0);
    pos += (pos..bytes.len()).take_while(|&index| bytes[index] == b'*').count();
    if pos < bytes.len() && bytes[pos] == b'/' {
        return (pos + 1, true);
    }
    (0, false)
}

// jsdoc_snippet.go:552
fn trim_right_single_line_whitespace(text: &str) -> &str {
    let mut end = 0;
    let mut pos = 0;
    while pos < text.len() {
        let (ch, size) = stringutil::decode_js_string_rune(&text[pos..]);
        if size == 0 {
            break;
        }
        pos += size;
        if !stringutil::is_white_space_single_line(ch) {
            end = pos;
        }
    }
    &text[..end]
}

// jsdoc_snippet.go:567
fn skip_single_line_whitespace(text: &str, pos: usize) -> usize {
    let mut pos = pos;
    while pos < text.len() {
        let (ch, size) = stringutil::decode_js_string_rune(&text[pos..]);
        if size == 0 || !stringutil::is_white_space_single_line(ch) {
            break;
        }
        pos += size;
    }
    pos
}

// jsdoc_snippet.go:582
fn starts_with_single_line_whitespace(text: &str) -> bool {
    if text.is_empty() {
        return false;
    }
    let (ch, size) = stringutil::decode_js_string_rune(text);
    size != 0 && stringutil::is_white_space_single_line(ch)
}

// jsdoc_snippet.go:590
fn is_only_spaces_or_tabs(text: &str) -> bool {
    text.bytes().all(|b| b == b' ' || b == b'\t')
}
