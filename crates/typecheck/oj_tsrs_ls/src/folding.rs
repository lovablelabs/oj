// Port of ls/folding.go.

use rustc_hash::FxHashSet;
use tsrs_ast::{self as ast, Kind, Node, NodeFlags, SourceFile};
use tsrs_core::context::Context;
use tsrs_core::{P, TextRange};
use tsrs_lsproto as lsproto;
use tsrs_printer as printer;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::completions_2::get_line_end_of_position;
use crate::languageservice::LanguageService;
use crate::spanmap::{Feature, Fidelity};
use crate::utilities::is_in_comment;

impl LanguageService {
    // folding.go:22
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    pub fn provide_folding_range(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
    ) -> Result<lsproto::FoldingRangeResponse, lsproto::Error> {
        let (_, source_file) = self.get_program_and_file(document_uri);
        let mut projections = vec![source_file];
        projections.extend_from_slice(source_file.supplemental_source_files());
        let mut res: Vec<lsproto::FoldingRange> = Vec::new();
        res.extend(projections.into_iter().flat_map(|projection| {
            let mut ranges = self.add_node_outlining_spans(ctx, projection);
            ranges.extend(self.add_region_outlining_spans(ctx, projection));
            if lsproto::get_client_capabilities(ctx).text_document.folding_range.line_folding_only {
                ranges = self.adjust_folding_end(ranges, projection);
            }
            ranges
        }));
        res.sort_by(|a, b| {
            let c = a.start_line.cmp(&b.start_line);
            if c != std::cmp::Ordering::Equal {
                return c;
            }
            let c = a.start_character.unwrap().cmp(&b.start_character.unwrap());
            if c != std::cmp::Ordering::Equal {
                return c;
            }
            let c = a.end_line.cmp(&b.end_line);
            if c != std::cmp::Ordering::Equal {
                return c;
            }
            a.end_character.unwrap().cmp(&b.end_character.unwrap())
        });
        let mut seen: FxHashSet<FoldingRangeKey> = FxHashSet::default();
        res.retain(|folding_range| seen.insert(key_for_folding_range(folding_range)));
        Ok(lsproto::FoldingRangesOrNull { folding_ranges: Some(res) })
    }
}

// folding.go:53
#[derive(Clone, PartialEq, Eq, Hash, Default)]
struct FoldingRangeKey {
    start_line: u32,
    start_character: u32,
    end_line: u32,
    end_character: u32,
    kind: lsproto::FoldingRangeKind,
    collapsed_text: String,
    has_start_character: bool,
    has_end_character: bool,
    has_kind: bool,
    has_collapsed_text: bool,
}

// folding.go:61
fn key_for_folding_range(folding_range: &lsproto::FoldingRange) -> FoldingRangeKey {
    let mut key = FoldingRangeKey {
        start_line: folding_range.start_line,
        end_line: folding_range.end_line,
        ..Default::default()
    };
    if let Some(start_character) = folding_range.start_character {
        key.start_character = start_character;
        key.has_start_character = true;
    }
    if let Some(end_character) = folding_range.end_character {
        key.end_character = end_character;
        key.has_end_character = true;
    }
    if let Some(kind) = folding_range.kind {
        key.kind = kind;
        key.has_kind = true;
    }
    if let Some(collapsed_text) = &folding_range.collapsed_text {
        key.collapsed_text.clone_from(collapsed_text);
        key.has_collapsed_text = true;
    }
    key
}

impl LanguageService {
    // adjustFoldingEnd adjusts the end line of folding ranges when the client signals lineFoldingOnly.
    // This mirrors the behavior of VS Code's built-in TypeScript extension (workaround for vscode#47240).
    // When lineFoldingOnly is true, we hide lines from startLine+1 to endLine. And to keep closing
    // brackets/braces visible, we subtract 1 from endLine when the range ends with a closing pair character.
    // folding.go:86
    fn adjust_folding_end(
        &self,
        ranges: Vec<lsproto::FoldingRange>,
        source_file: P<SourceFile>,
    ) -> Vec<lsproto::FoldingRange> {
        let source_text = source_file.text().as_bytes();
        let mut result = Vec::with_capacity(ranges.len());
        result.extend(ranges.into_iter().map(|mut r| {
            if let Some(end_character) = r.end_character.filter(|&c| c > 0) {
                let positions = self.converters.from_lsp_position_for_source_file(
                    source_file,
                    lsproto::Position { line: r.end_line, character: end_character },
                    Feature::FoldingRanges,
                );
                let position =
                    positions.iter().find(|p| p.script == source_file && !p.fidelity.is_none());
                if let Some(position) = position
                    && position.position > 0
                    && position.position as usize <= source_text.len()
                {
                    let end_offset = position.position;
                    let fold_end_char = source_text[end_offset as usize - 1];
                    if matches!(fold_end_char, b'}' | b']' | b')' | b'`' | b'>')
                        && r.end_line > r.start_line
                    {
                        r.end_line -= 1;
                    }
                }
            }
            r
        }));
        result
    }

    // folding.go:117
    fn add_node_outlining_spans(
        &self,
        ctx: &Context,
        source_file: P<SourceFile>,
    ) -> Vec<lsproto::FoldingRange> {
        let depth_remaining = 40;
        let mut current = 0;

        let statements = source_file.statements.nodes();
        let n = statements.len();
        let mut folding_range = Vec::with_capacity(40);
        while current < n {
            while current < n && !ast::is_any_import_syntax(statements[current]) {
                folding_range.extend(visit_node(
                    ctx,
                    statements[current],
                    depth_remaining,
                    source_file,
                    self,
                ));
                current += 1;
            }
            if current == n {
                break;
            }
            let first_import = current;
            while current < n && ast::is_any_import_syntax(statements[current]) {
                folding_range.extend(visit_node(
                    ctx,
                    statements[current],
                    depth_remaining,
                    source_file,
                    self,
                ));
                current += 1;
            }
            let last_import = current - 1;
            if last_import != first_import {
                let folding_range_kind = lsproto::FoldingRangeKind::IMPORTS;
                let imports = create_folding_range_from_bounds(
                    ctx,
                    astnav::get_start_of_node(
                        astnav::find_child_of_kind(
                            statements[first_import],
                            Kind::ImportKeyword,
                            source_file,
                        )
                        .unwrap(),
                        source_file,
                        false, /*includeJSDoc*/
                    ),
                    statements[last_import]
                        .end()
                        .expect_source("parsed imports have source positions"),
                    folding_range_kind,
                    source_file,
                    self,
                );
                if let Some(imports) = imports {
                    folding_range.push(imports);
                }
            }
        }

        // Visit the EOF Token so that comments which aren't attached to statements are included.
        folding_range.extend(visit_node(
            ctx,
            source_file.end_of_file_token,
            depth_remaining,
            source_file,
            self,
        ));
        folding_range
    }

    // folding.go:160
    fn add_region_outlining_spans(
        &self,
        ctx: &Context,
        source_file: P<SourceFile>,
    ) -> Vec<lsproto::FoldingRange> {
        struct RegionStart {
            position: u32,
            collapsed_text: Option<String>,
        }
        let mut regions: Vec<RegionStart> = Vec::with_capacity(40);
        let mut out = Vec::with_capacity(40);
        let line_starts = scanner::get_ecma_line_starts(&*source_file);
        for &current_line_start in line_starts {
            let line_end = get_line_end_of_position(source_file, current_line_start);
            let line_text = &source_file.text()[current_line_start as usize..line_end as usize];
            let result = parse_region_delimiter(line_text);
            let Some(result) = result else {
                continue;
            };
            if is_in_comment(
                source_file,
                current_line_start,
                astnav::get_token_at_position(source_file, current_line_start),
            )
            .is_some()
            {
                continue;
            }

            if result.is_start {
                let mut region = RegionStart {
                    position: current_line_start
                        + u32::try_from(
                            source_file.text()[current_line_start as usize..line_end as usize]
                                .find("//")
                                .expect("a region delimiter is a comment"),
                        )
                        .expect("region offset exceeds u32"),
                    collapsed_text: None,
                };
                if supports_collapsed_text(ctx) {
                    let mut collapsed_text = "#region".to_string();
                    if !result.name.is_empty() {
                        collapsed_text.clone_from(&result.name);
                    }
                    region.collapsed_text = Some(collapsed_text);
                }
                regions.push(region);
            } else if let Some(region) = regions.pop() {
                let (text_range, fidelity) =
                    self.create_folding_range_from_bounds(region.position, line_end, source_file);
                if fidelity.is_none() {
                    continue;
                }
                let mut folding_range =
                    create_folding_range(ctx, text_range, lsproto::FoldingRangeKind::REGION, "");
                folding_range.collapsed_text = region.collapsed_text;
                out.push(folding_range);
            }
        }
        out
    }
}

// folding.go:203
fn visit_node(
    ctx: &Context,
    n: P<Node>,
    depth_remaining: usize,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Vec<lsproto::FoldingRange> {
    let mut depth_remaining = depth_remaining;
    if n.flags().intersects(NodeFlags::Reparsed) || depth_remaining == 0 || ctx.err().is_some() {
        return Vec::new();
    }
    let mut folding_range = Vec::with_capacity(40);
    if (!ast::is_binary_expression(n) && ast::is_declaration(n))
        || ast::is_variable_statement(n)
        || ast::is_return_statement(n)
        || ast::is_call_or_new_expression(n)
        || n.kind() == Kind::EndOfFile
    {
        folding_range.extend(add_outlining_for_leading_comments_for_node(ctx, n, source_file, l));
    }
    if ast::is_function_like(n)
        && let Some(parent) = n.parent()
        && ast::is_binary_expression(parent)
        && ast::is_property_access_expression(parent.as_binary_expression().left)
    {
        folding_range.extend(add_outlining_for_leading_comments_for_node(
            ctx,
            parent.as_binary_expression().left,
            source_file,
            l,
        ));
    }
    if ast::is_block(n) {
        let statements = n.as_block().statements;
        folding_range.extend(add_outlining_for_leading_comments_for_pos(
            ctx,
            statements
                .loc
                .get()
                .end()
                .expect_source("parsed statement lists have source positions"),
            source_file,
            l,
        ));
    }
    if ast::is_module_block(n) {
        let statements = n.as_module_block().statements;
        folding_range.extend(add_outlining_for_leading_comments_for_pos(
            ctx,
            statements
                .loc
                .get()
                .end()
                .expect_source("parsed statement lists have source positions"),
            source_file,
            l,
        ));
    }
    if ast::is_class_like(n) || ast::is_interface_declaration(n) {
        let members = if ast::is_class_declaration(n) {
            n.as_class_declaration().class_like_base.members
        } else if ast::is_class_expression(n) {
            n.as_class_expression().class_like_base.members
        } else {
            n.as_interface_declaration().members
        };
        folding_range.extend(add_outlining_for_leading_comments_for_pos(
            ctx,
            members.loc.get().end().expect_source("parsed member lists have source positions"),
            source_file,
            l,
        ));
    }

    if let Some(span) = get_outlining_span_for_node(ctx, n, source_file, l) {
        folding_range.push(span);
    }

    depth_remaining -= 1;
    if ast::is_call_expression(n) {
        depth_remaining += 1;
        let expression_nodes =
            visit_node(ctx, n.expression().unwrap(), depth_remaining, source_file, l);
        folding_range.extend(expression_nodes);
        depth_remaining -= 1;
        folding_range.extend(
            n.arguments()
                .iter()
                .flat_map(|&arg| visit_node(ctx, arg, depth_remaining, source_file, l)),
        );
        folding_range.extend(
            n.type_arguments()
                .iter()
                .flat_map(|&type_arg| visit_node(ctx, type_arg, depth_remaining, source_file, l)),
        );
    } else if ast::is_if_statement(n)
        && n.as_if_statement().else_statement.is_some_and(ast::is_if_statement)
    {
        // Consider an 'else if' to be on the same depth as the 'if'.
        let if_statement = n.as_if_statement();
        folding_range.extend(visit_node(
            ctx,
            n.expression().unwrap(),
            depth_remaining,
            source_file,
            l,
        ));
        folding_range.extend(visit_node(
            ctx,
            if_statement.then_statement,
            depth_remaining,
            source_file,
            l,
        ));
        depth_remaining += 1;
        folding_range.extend(visit_node(
            ctx,
            if_statement.else_statement.unwrap(),
            depth_remaining,
            source_file,
            l,
        ));
    } else {
        n.for_each_child(&mut |node| {
            folding_range.extend(visit_node(ctx, node, depth_remaining, source_file, l));
            false
        });
    }
    folding_range
}

// folding.go:295
fn add_outlining_for_leading_comments_for_node(
    ctx: &Context,
    n: P<Node>,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Vec<lsproto::FoldingRange> {
    if ast::is_jsx_text(n) {
        return Vec::new();
    }
    add_outlining_for_leading_comments_for_pos(
        ctx,
        n.pos().expect_source("parsed nodes have source positions"),
        source_file,
        l,
    )
}

// folding.go:302
fn add_outlining_for_leading_comments_for_pos(
    ctx: &Context,
    pos: u32,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Vec<lsproto::FoldingRange> {
    let mut folding_range = Vec::with_capacity(40);
    let mut first_single_line_comment_start = None;
    let mut last_single_line_comment_end = None;
    let mut single_line_comment_count = 0_usize;
    let folding_range_kind_comment = lsproto::FoldingRangeKind::COMMENT;

    let combine_and_add_multiple_single_line_comments = |single_line_comment_count: usize,
                                                         first: Option<u32>,
                                                         last: Option<u32>|
     -> Option<lsproto::FoldingRange> {
        // Only outline spans of two or more consecutive single line comments
        if single_line_comment_count > 1 {
            return create_folding_range_from_bounds(
                ctx,
                first.expect("a comment group has a first position"),
                last.expect("a comment group has a last position"),
                folding_range_kind_comment,
                source_file,
                l,
            );
        }
        None
    };

    let source_text = source_file.text();
    for comment in scanner::get_leading_comment_ranges(source_text, pos) {
        let comment_pos =
            comment.text_range.pos().expect_source("comment ranges have source positions");
        let comment_end =
            comment.text_range.end().expect_source("comment ranges have source positions");

        if ctx.err().is_some() {
            return Vec::new();
        }
        match comment.kind {
            Kind::SingleLineCommentTrivia => {
                // never fold region delimiters into single-line comment regions
                let comment_text = &source_text[comment_pos as usize..comment_end as usize];
                if parse_region_delimiter(comment_text).is_some() {
                    if let Some(comments) = combine_and_add_multiple_single_line_comments(
                        single_line_comment_count,
                        first_single_line_comment_start,
                        last_single_line_comment_end,
                    ) {
                        folding_range.push(comments);
                    }
                    single_line_comment_count = 0;
                    continue;
                }

                // For single line comments, combine consecutive ones (2 or more) into
                // a single span from the start of the first till the end of the last
                if single_line_comment_count == 0 {
                    first_single_line_comment_start = Some(comment_pos);
                }
                last_single_line_comment_end = Some(comment_end);
                single_line_comment_count += 1;
            }
            Kind::MultiLineCommentTrivia => {
                if let Some(comments) = combine_and_add_multiple_single_line_comments(
                    single_line_comment_count,
                    first_single_line_comment_start,
                    last_single_line_comment_end,
                ) {
                    folding_range.push(comments);
                }
                if let Some(comment) = create_folding_range_from_bounds(
                    ctx,
                    comment_pos,
                    comment_end,
                    folding_range_kind_comment,
                    source_file,
                    l,
                ) {
                    folding_range.push(comment);
                }
                single_line_comment_count = 0;
            }
            k => panic!("Debug Failure. Illegal value: {k:?}"),
        }
    }
    if let Some(added_comments) = combine_and_add_multiple_single_line_comments(
        single_line_comment_count,
        first_single_line_comment_start,
        last_single_line_comment_end,
    ) {
        folding_range.push(added_comments);
    }
    folding_range
}

// folding.go:367
struct RegionDelimiterResult {
    is_start: bool,
    name: String,
}

// folding.go:372
fn parse_region_delimiter(line_text: &str) -> Option<RegionDelimiterResult> {
    // We trim the leading whitespace and // without the regex since the
    // multiple potential whitespace matches can make for some gnarly backtracking behavior
    let line_text = line_text.trim_start();
    let line_text = line_text.strip_prefix("//")?;
    let line_text = line_text.trim();
    let line_text = line_text.strip_suffix('\r').unwrap_or(line_text);
    let mut line_text = line_text.strip_prefix('#')?;
    let mut is_start = true;
    if let Some(rest) = line_text.strip_prefix("end") {
        is_start = false;
        line_text = rest;
    }
    let line_text = line_text.strip_prefix("region")?;
    Some(RegionDelimiterResult { is_start, name: line_text.trim().to_string() })
}

// folding.go:400
fn get_outlining_span_for_node(
    ctx: &Context,
    n: P<Node>,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    match n.kind() {
        Kind::Block => {
            let parent = n.parent().unwrap();
            if ast::is_function_like(parent) {
                return function_span(ctx, parent, n, source_file, l);
            }
            // Check if the block is standalone, or 'attached' to some parent statement.
            // If the latter, we want to collapse the block, but consider its hint span
            // to be the entire span of the parent.
            match parent.kind() {
                Kind::DoStatement
                | Kind::ForInStatement
                | Kind::ForOfStatement
                | Kind::ForStatement
                | Kind::IfStatement
                | Kind::WhileStatement
                | Kind::WithStatement
                | Kind::CatchClause => {
                    return span_for_node(
                        ctx,
                        n,
                        Kind::OpenBraceToken,
                        true, /*useFullStart*/
                        source_file,
                        l,
                    );
                }
                Kind::TryStatement => {
                    // Could be the try-block, or the finally-block.
                    let try_statement = parent.as_try_statement();
                    if try_statement.try_block == n {
                        return span_for_node(
                            ctx,
                            n,
                            Kind::OpenBraceToken,
                            true, /*useFullStart*/
                            source_file,
                            l,
                        );
                    } else if try_statement.finally_block == Some(n)
                        && let Some(span) = span_for_node(
                            ctx,
                            n,
                            Kind::OpenBraceToken,
                            true, /*useFullStart*/
                            source_file,
                            l,
                        )
                    {
                        return Some(span);
                    }
                    // fallthrough
                }
                _ => {}
            }
            // Block was a standalone block.  In this case we want to only collapse
            // the span of the block, independent of any parent span.
            let (text_range, fidelity) =
                l.create_lsp_range_from_node_for_feature(n, source_file, Feature::FoldingRanges);
            if fidelity.is_none() {
                return None;
            }
            Some(create_folding_range(ctx, text_range, lsproto::FoldingRangeKind(""), ""))
        }
        Kind::ModuleBlock
        | Kind::ClassDeclaration
        | Kind::ClassExpression
        | Kind::InterfaceDeclaration
        | Kind::EnumDeclaration
        | Kind::CaseBlock
        | Kind::TypeLiteral
        | Kind::ObjectBindingPattern => {
            span_for_node(ctx, n, Kind::OpenBraceToken, true /*useFullStart*/, source_file, l)
        }
        Kind::TupleType => {
            span_for_node(
                ctx,
                n,
                Kind::OpenBracketToken,
                !ast::is_tuple_type_node(n.parent().unwrap()), /*useFullStart*/
                source_file,
                l,
            )
        }
        Kind::CaseClause | Kind::DefaultClause => {
            span_for_node_array(ctx, n.as_case_or_default_clause().statements, source_file, l)
        }
        Kind::ObjectLiteralExpression => {
            let parent = n.parent().unwrap();
            span_for_node(
                ctx,
                n,
                Kind::OpenBraceToken,
                !ast::is_array_literal_expression(parent) && !ast::is_call_expression(parent), /*useFullStart*/
                source_file,
                l,
            )
        }
        Kind::ArrayLiteralExpression => {
            let parent = n.parent().unwrap();
            span_for_node(
                ctx,
                n,
                Kind::OpenBracketToken,
                !ast::is_array_literal_expression(parent) && !ast::is_call_expression(parent), /*useFullStart*/
                source_file,
                l,
            )
        }
        Kind::JsxElement | Kind::JsxFragment => span_for_jsx_element(ctx, n, source_file, l),
        Kind::JsxSelfClosingElement | Kind::JsxOpeningElement => {
            span_for_jsx_attributes(ctx, n, source_file, l)
        }
        Kind::TemplateExpression | Kind::NoSubstitutionTemplateLiteral => {
            span_for_template_literal(ctx, n, source_file, l)
        }
        Kind::ArrayBindingPattern => {
            span_for_node(
                ctx,
                n,
                Kind::OpenBracketToken,
                !ast::is_binding_element(n.parent().unwrap()), /*useFullStart*/
                source_file,
                l,
            )
        }
        Kind::ArrowFunction => span_for_arrow_function(ctx, n, source_file, l),
        Kind::CallExpression => span_for_call_expression(ctx, n, source_file, l),
        Kind::ParenthesizedExpression => span_for_parenthesized_expression(ctx, n, source_file, l),
        Kind::NamedImports | Kind::NamedExports | Kind::ImportAttributes => {
            span_for_import_export_elements(ctx, n, source_file, l)
        }
        _ => None,
    }
}

// folding.go:464
fn span_for_import_export_elements(
    ctx: &Context,
    node: P<Node>,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    let elements: Option<&'static [P<Node>]> = match node.kind() {
        Kind::NamedImports => Some(node.as_named_imports().elements.nodes()),
        Kind::NamedExports => Some(node.as_named_exports().elements.nodes()),
        Kind::ImportAttributes => Some(node.as_import_attributes().attributes.nodes()),
        _ => None,
    };
    if elements.is_none_or(<[tsrs_core::P<tsrs_ast::Node>]>::is_empty) {
        return None;
    }
    let open_token = astnav::find_child_of_kind(node, Kind::OpenBraceToken, source_file);
    let close_token = astnav::find_child_of_kind(node, Kind::CloseBraceToken, source_file);
    let (Some(open_token), Some(close_token)) = (open_token, close_token) else {
        return None;
    };
    if printer::positions_are_on_same_line(open_token.pos(), close_token.pos(), source_file) {
        return None;
    }
    range_between_tokens(ctx, open_token, close_token, source_file, false /*useFullStart*/, l)
}

// folding.go:485
fn span_for_parenthesized_expression(
    ctx: &Context,
    node: P<Node>,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    let start = astnav::get_start_of_node(node, source_file, false /*includeJSDoc*/);
    if printer::positions_are_on_same_line(start.into(), node.end(), source_file) {
        return None;
    }
    let (text_range, fidelity) = l.create_folding_range_from_bounds(
        start,
        node.end().expect_source("parsed nodes have source positions"),
        source_file,
    );
    if fidelity.is_none() {
        return None;
    }
    Some(create_folding_range(ctx, text_range, lsproto::FoldingRangeKind(""), ""))
}

// folding.go:497
fn span_for_call_expression(
    ctx: &Context,
    node: P<Node>,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    if node.arguments().is_empty() {
        return None;
    }
    let open_token = astnav::find_child_of_kind(node, Kind::OpenParenToken, source_file);
    let close_token = astnav::find_child_of_kind(node, Kind::CloseParenToken, source_file);
    let (Some(open_token), Some(close_token)) = (open_token, close_token) else {
        return None;
    };
    if printer::positions_are_on_same_line(open_token.pos(), close_token.pos(), source_file) {
        return None;
    }

    range_between_tokens(ctx, open_token, close_token, source_file, true /*useFullStart*/, l)
}

// folding.go:510
fn span_for_arrow_function(
    ctx: &Context,
    node: P<Node>,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    let body = node.body().unwrap();
    if ast::is_block(body)
        || ast::is_parenthesized_expression(body)
        || printer::positions_are_on_same_line(body.pos(), body.end(), source_file)
    {
        return None;
    }
    let (text_range, fidelity) = l.create_folding_range_from_bounds(
        body.pos().expect_source("parsed nodes have source positions"),
        body.end().expect_source("parsed nodes have source positions"),
        source_file,
    );
    if fidelity.is_none() {
        return None;
    }
    Some(create_folding_range(ctx, text_range, lsproto::FoldingRangeKind(""), ""))
}

// folding.go:522
fn span_for_template_literal(
    ctx: &Context,
    node: P<Node>,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    if node.kind() == Kind::NoSubstitutionTemplateLiteral && node.text().is_empty() {
        return None;
    }
    create_folding_range_from_bounds(
        ctx,
        astnav::get_start_of_node(node, source_file, false /*includeJSDoc*/),
        node.end().expect_source("parsed nodes have source positions"),
        lsproto::FoldingRangeKind(""),
        source_file,
        l,
    )
}

// folding.go:529
fn span_for_jsx_element(
    ctx: &Context,
    node: P<Node>,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    if node.kind() == Kind::JsxElement {
        let jsx_element = node.as_jsx_element();
        let (text_range, fidelity) = l.create_folding_range_from_bounds(
            astnav::get_start_of_node(
                jsx_element.opening_element,
                source_file,
                false, /*includeJSDoc*/
            ),
            jsx_element
                .closing_element
                .end()
                .expect_source("parsed JSX elements have source positions"),
            source_file,
        );
        if fidelity.is_none() {
            return None;
        }
        let tag_name = scanner::get_text_of_node(jsx_element.opening_element.tag_name());
        let banner_text = format!("<{tag_name}>...</{tag_name}>");
        return Some(create_folding_range(
            ctx,
            text_range,
            lsproto::FoldingRangeKind(""),
            &banner_text,
        ));
    }
    // JsxFragment
    let jsx_fragment = node.as_jsx_fragment();
    let (text_range, fidelity) = l.create_folding_range_from_bounds(
        astnav::get_start_of_node(
            jsx_fragment.opening_fragment,
            source_file,
            false, /*includeJSDoc*/
        ),
        jsx_fragment
            .closing_fragment
            .end()
            .expect_source("parsed JSX fragments have source positions"),
        source_file,
    );
    if fidelity.is_none() {
        return None;
    }
    Some(create_folding_range(ctx, text_range, lsproto::FoldingRangeKind(""), "<>...</>"))
}

// folding.go:549
fn span_for_jsx_attributes(
    ctx: &Context,
    node: P<Node>,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    let attributes = if node.kind() == Kind::JsxSelfClosingElement {
        node.as_jsx_self_closing_element().attributes
    } else {
        node.as_jsx_opening_element().attributes
    };
    if attributes.properties().is_empty() {
        return None;
    }
    create_folding_range_from_bounds(
        ctx,
        astnav::get_start_of_node(node, source_file, false /*includeJSDoc*/),
        node.end().expect_source("parsed nodes have source positions"),
        lsproto::FoldingRangeKind(""),
        source_file,
        l,
    )
}

// folding.go:562
fn span_for_node_array(
    ctx: &Context,
    statements: P<ast::NodeList>,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    if !statements.nodes().is_empty() {
        let (text_range, fidelity) = l.create_folding_range_from_bounds(
            statements
                .loc
                .get()
                .pos()
                .expect_source("parsed statement lists have source positions"),
            statements
                .loc
                .get()
                .end()
                .expect_source("parsed statement lists have source positions"),
            source_file,
        );
        if fidelity.is_none() {
            return None;
        }
        return Some(create_folding_range(ctx, text_range, lsproto::FoldingRangeKind(""), ""));
    }
    None
}

// folding.go:573
fn span_for_node(
    ctx: &Context,
    node: P<Node>,
    open: Kind,
    use_full_start: bool,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    let close_brace =
        if open == Kind::OpenBraceToken { Kind::CloseBraceToken } else { Kind::CloseBracketToken };
    let open_token = astnav::find_child_of_kind(node, open, source_file);
    let close_token = astnav::find_child_of_kind(node, close_brace, source_file);
    if let (Some(open_token), Some(close_token)) = (open_token, close_token) {
        return range_between_tokens(ctx, open_token, close_token, source_file, use_full_start, l);
    }
    None
}

// folding.go:586
fn range_between_tokens(
    ctx: &Context,
    open_token: P<Node>,
    close_token: P<Node>,
    source_file: P<SourceFile>,
    use_full_start: bool,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    let (text_range, fidelity) = if use_full_start {
        l.create_folding_range_from_bounds(
            open_token.pos().expect_source("parsed tokens have source positions"),
            close_token.end().expect_source("parsed tokens have source positions"),
            source_file,
        )
    } else {
        l.create_folding_range_from_bounds(
            astnav::get_start_of_node(open_token, source_file, false /*includeJSDoc*/),
            close_token.end().expect_source("parsed tokens have source positions"),
            source_file,
        )
    };
    if fidelity.is_none() {
        return None;
    }
    Some(create_folding_range(ctx, text_range, lsproto::FoldingRangeKind(""), ""))
}

// folding.go:600
fn supports_collapsed_text(ctx: &Context) -> bool {
    lsproto::get_client_capabilities(ctx).text_document.folding_range.folding_range.collapsed_text
}

// folding.go:604
fn create_folding_range(
    ctx: &Context,
    text_range: lsproto::Range,
    folding_range_kind: lsproto::FoldingRangeKind,
    collapsed_text: &str,
) -> lsproto::FoldingRange {
    let kind = if folding_range_kind.0.is_empty() { None } else { Some(folding_range_kind) };
    let mut result = lsproto::FoldingRange {
        start_line: text_range.start.line,
        start_character: Some(text_range.start.character),
        end_line: text_range.end.line,
        end_character: Some(text_range.end.character),
        kind,
        ..Default::default()
    };
    if !collapsed_text.is_empty() && supports_collapsed_text(ctx) {
        result.collapsed_text = Some(collapsed_text.to_string());
    }
    result
}

// folding.go:622
fn create_folding_range_from_bounds(
    ctx: &Context,
    pos: u32,
    end: u32,
    folding_range_kind: lsproto::FoldingRangeKind,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    let (text_range, fidelity) = l.create_folding_range_from_bounds(pos, end, source_file);
    if fidelity.is_none() {
        return None;
    }
    Some(create_folding_range(ctx, text_range, folding_range_kind, ""))
}

impl LanguageService {
    // folding.go:630
    fn create_folding_range_from_bounds(
        &self,
        start: u32,
        end: u32,
        source_file: P<SourceFile>,
    ) -> (lsproto::Range, Fidelity) {
        self.converters.to_lsp_range_for_feature(
            &source_file,
            TextRange::new(start, end),
            Feature::FoldingRanges,
        )
    }
}

// folding.go:634
fn function_span(
    ctx: &Context,
    node: P<Node>,
    body: P<Node>,
    source_file: P<SourceFile>,
    l: &LanguageService,
) -> Option<lsproto::FoldingRange> {
    let open_token = try_get_function_open_token(node, body, source_file);
    let close_token = astnav::find_child_of_kind(body, Kind::CloseBraceToken, source_file);
    if let (Some(open_token), Some(close_token)) = (open_token, close_token) {
        return range_between_tokens(
            ctx,
            open_token,
            close_token,
            source_file,
            true, /*useFullStart*/
            l,
        );
    }
    None
}

// folding.go:643
fn try_get_function_open_token(
    node: P<Node>,
    body: P<Node>,
    source_file: P<SourceFile>,
) -> Option<P<Node>> {
    if is_node_array_multi_line(node.parameters(), source_file)
        && let Some(open_paren_token) =
            astnav::find_child_of_kind(node, Kind::OpenParenToken, source_file)
    {
        return Some(open_paren_token);
    }
    astnav::find_child_of_kind(body, Kind::OpenBraceToken, source_file)
}

// folding.go:653
fn is_node_array_multi_line(list: &[P<Node>], source_file: P<SourceFile>) -> bool {
    if list.is_empty() {
        return false;
    }
    !printer::positions_are_on_same_line(list[0].pos(), list[list.len() - 1].end(), source_file)
}
