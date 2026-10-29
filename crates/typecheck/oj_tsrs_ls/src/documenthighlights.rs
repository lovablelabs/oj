use rustc_hash::FxHashMap;
use tsrs_ast::{self as ast, FindAncestorResult, Kind, ModifierFlags, Node, SourceFile};
use tsrs_compiler::Program;
use tsrs_core::collections::{Set, new_set_with_size_hint};
use tsrs_core::context::Context;
use tsrs_core::{P, stringutil};
use tsrs_lsproto as lsproto;
use tsrs_scanner as scanner;

use crate::astnav;
use crate::findallreferences::{EntryKind, RefOptions, ReferenceEntry, ReferenceUse};
use crate::languageservice::LanguageService;
use crate::lsconv;
use crate::lsutil;
use crate::spanmap::Feature;
use crate::utilities::get_children_from_non_jsdoc_node;

impl LanguageService {
    // documenthighlights.go:20
    pub fn provide_document_highlights(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
        document_position: lsproto::Position,
    ) -> Result<lsproto::DocumentHighlightResponse, lsproto::Error> {
        let result =
            self.provide_document_highlights_worker(ctx, document_uri, document_position, &[])?;
        // Extract highlights for the current file only.
        let mut document_highlights: Vec<lsproto::DocumentHighlight> = Vec::new();
        if let Some(multi_document_highlights) = result.multi_document_highlights {
            document_highlights.extend(
                multi_document_highlights
                    .into_iter()
                    .filter_map(
                        |mh| {
                            if mh.uri == *document_uri { Some(mh.highlights) } else { None }
                        },
                    )
                    .flatten(),
            );
        }
        Ok(lsproto::DocumentHighlightsOrNull { document_highlights: Some(document_highlights) })
    }

    // documenthighlights.go:37
    pub fn provide_multi_document_highlights(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
        document_position: lsproto::Position,
        files_to_search: &[lsproto::DocumentUri],
    ) -> Result<lsproto::CustomMultiDocumentHighlightResponse, lsproto::Error> {
        self.provide_document_highlights_worker(
            ctx,
            document_uri,
            document_position,
            files_to_search,
        )
    }

    // documenthighlights.go:41
    #[expect(
        clippy::unnecessary_wraps,
        reason = "Language service operations share the fallible response signature used by LSP and cross-project dispatch."
    )]
    fn provide_document_highlights_worker(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
        document_position: lsproto::Position,
        files_to_search: &[lsproto::DocumentUri],
    ) -> Result<lsproto::MultiDocumentHighlightsOrNull, lsproto::Error> {
        let (program, source_file) = self.get_program_and_file(document_uri);
        let positions = self.converters.from_lsp_position_for_source_file(
            source_file,
            document_position,
            Feature::DocumentHighlights,
        );
        let mut results: Vec<lsproto::MultiDocumentHighlightsOrNull> =
            Vec::with_capacity(positions.len());
        results.extend(positions.into_iter().filter_map(|mapped| {
            if mapped.fidelity.is_single_segment() {
                Some(self.provide_document_highlights_at_position(
                    ctx,
                    document_uri,
                    mapped.position,
                    program,
                    mapped.script,
                    files_to_search,
                ))
            } else {
                None
            }
        }));
        Ok(combine_multi_document_highlights(results))
    }

    // documenthighlights.go:53
    fn provide_document_highlights_at_position(
        &self,
        ctx: &Context,
        document_uri: &lsproto::DocumentUri,
        position: u32,
        program: &'static Program,
        source_file: P<SourceFile>,
        files_to_search: &[lsproto::DocumentUri],
    ) -> lsproto::MultiDocumentHighlightsOrNull {
        let node = astnav::get_touching_property_name(source_file, position);

        // Cheap JSX check before resolving files to search.
        if let Some(parent) = node.parent()
            && (parent.kind() == Kind::JsxClosingElement
                || (parent.kind() == Kind::JsxOpeningElement && parent.tag_name() == node))
        {
            let mut opening_element: Option<P<Node>> = None;
            let mut closing_element: Option<P<Node>> = None;
            let grandparent = parent.parent().unwrap();
            if ast::is_jsx_element(grandparent) {
                opening_element = Some(grandparent.as_jsx_element().opening_element);
                closing_element = Some(grandparent.as_jsx_element().closing_element);
            }
            let mut highlights: Vec<lsproto::DocumentHighlight> = Vec::new();
            let kind = lsproto::DocumentHighlightKind::READ;
            if let Some(opening_element) = opening_element {
                let (lsp_range, fidelity) = self.create_lsp_range_from_node_for_feature(
                    opening_element,
                    source_file,
                    Feature::DocumentHighlights,
                );
                if !fidelity.is_none() {
                    highlights
                        .push(lsproto::DocumentHighlight { range: lsp_range, kind: Some(kind) });
                }
            }
            if let Some(closing_element) = closing_element {
                let (lsp_range, fidelity) = self.create_lsp_range_from_node_for_feature(
                    closing_element,
                    source_file,
                    Feature::DocumentHighlights,
                );
                if !fidelity.is_none() {
                    highlights
                        .push(lsproto::DocumentHighlight { range: lsp_range, kind: Some(kind) });
                }
            }
            let multi_highlights =
                vec![lsproto::MultiDocumentHighlight { uri: document_uri.clone(), highlights }];
            return lsproto::MultiDocumentHighlightsOrNull {
                multi_document_highlights: Some(multi_highlights),
            };
        }

        // Resolve the source files to search, deduplicating by file name.
        let mut source_files: Vec<P<SourceFile>> = Vec::new();
        let mut seen_files: Set<String> = new_set_with_size_hint(files_to_search.len());
        source_files.extend(files_to_search.iter().filter_map(|uri| {
            let file_name = uri.file_name();
            if !seen_files.add_if_absent(file_name.clone()) {
                return None;
            }
            program.get_source_file(&file_name)
        }));
        if source_files.is_empty() {
            source_files = vec![source_file];
        }

        let mut multi_highlights =
            self.get_semantic_document_highlights(ctx, position, node, program, &source_files);
        if multi_highlights.is_empty() {
            // Fall back to syntactic highlights for the current file only.
            let syntactic_highlights = self.get_syntactic_document_highlights(node, source_file);
            if !syntactic_highlights.is_empty() {
                multi_highlights = vec![lsproto::MultiDocumentHighlight {
                    uri: document_uri.clone(),
                    highlights: syntactic_highlights,
                }];
            }
        }
        lsproto::MultiDocumentHighlightsOrNull { multi_document_highlights: Some(multi_highlights) }
    }
}

// documenthighlights.go:112
fn combine_multi_document_highlights(
    results: Vec<lsproto::MultiDocumentHighlightsOrNull>,
) -> lsproto::MultiDocumentHighlightsOrNull {
    // Go keeps `*MultiDocumentHighlight` pointers in byURI; the port keeps indexes into combinedDocuments.
    let mut by_uri: FxHashMap<lsproto::DocumentUri, usize> = FxHashMap::default();
    let mut seen: FxHashMap<lsproto::DocumentUri, Set<lsproto::Range>> = FxHashMap::default();
    let mut combined_documents: Vec<lsproto::MultiDocumentHighlight> = Vec::new();
    for result in results {
        let Some(documents) = result.multi_document_highlights else {
            continue;
        };
        for document in documents {
            let index = if let Some(&index) = by_uri.get(&document.uri) {
                index
            } else {
                combined_documents.push(lsproto::MultiDocumentHighlight {
                    uri: document.uri.clone(),
                    highlights: Vec::new(),
                });
                let index = combined_documents.len() - 1;
                by_uri.insert(document.uri.clone(), index);
                index
            };
            let ranges = seen.entry(document.uri.clone()).or_default();
            combined_documents[index].highlights.extend(
                document
                    .highlights
                    .into_iter()
                    .filter(|highlight| ranges.add_if_absent(highlight.range)),
            );
        }
    }
    lsproto::MultiDocumentHighlightsOrNull { multi_document_highlights: Some(combined_documents) }
}

impl LanguageService {
    // documenthighlights.go:139
    fn get_semantic_document_highlights(
        &self,
        ctx: &Context,
        position: u32,
        node: P<Node>,
        program: &'static Program,
        source_files: &[P<SourceFile>],
    ) -> Vec<lsproto::MultiDocumentHighlight> {
        let options = RefOptions { use_: ReferenceUse::None, ..Default::default() };
        let reference_entries = self.get_referenced_symbols_for_node(
            ctx,
            position,
            node,
            program,
            source_files,
            options,
        );
        if reference_entries.is_empty() {
            return Vec::new();
        }

        // Group highlights by file
        let mut file_highlights: FxHashMap<String, Vec<lsproto::DocumentHighlight>> =
            FxHashMap::default();
        for entry in &reference_entries {
            for r in &entry.references {
                let (file_name, highlight) = self.to_document_highlight(r);
                let Some(highlight) = highlight else {
                    continue;
                };
                file_highlights.entry(file_name).or_default().push(highlight);
            }
        }

        let mut result: Vec<lsproto::MultiDocumentHighlight> = Vec::new();
        result.extend(source_files.iter().filter_map(|sf| {
            let file_name = sf.original_file_name();
            file_highlights.remove(file_name).map(|highlights| lsproto::MultiDocumentHighlight {
                uri: lsconv::file_name_to_document_uri(file_name),
                highlights,
            })
        }));
        result
    }

    // documenthighlights.go:171
    fn to_document_highlight(
        &self,
        entry: &ReferenceEntry,
    ) -> (String, Option<lsproto::DocumentHighlight>) {
        let entry = self.resolve_entry(entry);
        let file_name = entry.source_file.get().unwrap().original_file_name().to_string();

        let mut kind = lsproto::DocumentHighlightKind::READ;
        let Some(lsp_range) =
            self.get_range_of_entry_for_feature(entry, Feature::DocumentHighlights)
        else {
            return (file_name, None);
        };
        if entry.kind == EntryKind::Range {
            return (
                file_name,
                Some(lsproto::DocumentHighlight { range: lsp_range, kind: Some(kind) }),
            );
        }

        // Determine write access for node references.
        if ast::is_write_access_for_reference(entry.node.unwrap()) {
            kind = lsproto::DocumentHighlightKind::WRITE;
        }

        let dh = lsproto::DocumentHighlight { range: lsp_range, kind: Some(kind) };

        (file_name, Some(dh))
    }

    // documenthighlights.go:200
    fn get_syntactic_document_highlights(
        &self,
        node: P<Node>,
        source_file: P<SourceFile>,
    ) -> Vec<lsproto::DocumentHighlight> {
        let parent = node.parent();
        match node.kind() {
            Kind::IfKeyword | Kind::ElseKeyword => {
                if ast::is_if_statement(parent.unwrap()) {
                    return self.get_if_else_occurrences(parent.unwrap(), source_file);
                }
                Vec::new()
            }
            Kind::ReturnKeyword => self.use_parent(
                parent.unwrap(),
                &ast::is_return_statement,
                &mut get_return_occurrences,
                source_file,
            ),
            Kind::ThrowKeyword => self.use_parent(
                parent.unwrap(),
                &ast::is_throw_statement,
                &mut get_throw_occurrences,
                source_file,
            ),
            Kind::TryKeyword | Kind::CatchKeyword | Kind::FinallyKeyword => {
                let try_statement = if node.kind() == Kind::CatchKeyword {
                    parent.unwrap().parent().unwrap()
                } else {
                    parent.unwrap()
                };
                self.use_parent(
                    try_statement,
                    &ast::is_try_statement,
                    &mut get_try_catch_finally_occurrences,
                    source_file,
                )
            }
            Kind::SwitchKeyword => self.use_parent(
                parent.unwrap(),
                &ast::is_switch_statement,
                &mut get_switch_case_default_occurrences,
                source_file,
            ),
            Kind::CaseKeyword | Kind::DefaultKeyword => {
                let parent = parent.unwrap();
                if ast::is_default_clause(parent) || ast::is_case_clause(parent) {
                    return self.use_parent(
                        parent.parent().unwrap().parent().unwrap(),
                        &ast::is_switch_statement,
                        &mut get_switch_case_default_occurrences,
                        source_file,
                    );
                }
                Vec::new()
            }
            Kind::BreakKeyword | Kind::ContinueKeyword => self.use_parent(
                parent.unwrap(),
                &ast::is_break_or_continue_statement,
                &mut get_break_or_continue_statement_occurrences,
                source_file,
            ),
            Kind::ForKeyword | Kind::WhileKeyword | Kind::DoKeyword => self.use_parent(
                parent.unwrap(),
                &|n| ast::is_iteration_statement(n, true),
                &mut get_loop_break_continue_occurrences,
                source_file,
            ),
            Kind::ConstructorKeyword => self.get_from_all_declarations(
                &ast::is_constructor_declaration,
                &[Kind::ConstructorKeyword],
                node,
                source_file,
            ),
            Kind::GetKeyword | Kind::SetKeyword => self.get_from_all_declarations(
                &ast::is_accessor,
                &[Kind::GetKeyword, Kind::SetKeyword],
                node,
                source_file,
            ),
            Kind::AwaitKeyword => self.use_parent(
                parent.unwrap(),
                &ast::is_await_expression,
                &mut get_async_and_await_occurrences,
                source_file,
            ),
            Kind::AsyncKeyword => self
                .highlight_spans(&get_async_and_await_occurrences(node, source_file), source_file),
            Kind::YieldKeyword => {
                self.highlight_spans(&get_yield_occurrences(node, source_file), source_file)
            }
            Kind::InKeyword | Kind::OutKeyword => Vec::new(),
            _ => {
                if ast::is_modifier_kind(node.kind())
                    && (ast::is_declaration(parent.unwrap())
                        || ast::is_variable_statement(parent.unwrap()))
                {
                    return self.highlight_spans(
                        &get_modifier_occurrences(node.kind(), parent.unwrap(), source_file),
                        source_file,
                    );
                }
                Vec::new()
            }
        }
    }

    // documenthighlights.go:252
    fn use_parent(
        &self,
        node: P<Node>,
        node_test: &dyn Fn(P<Node>) -> bool,
        get_nodes: &mut dyn FnMut(P<Node>, P<SourceFile>) -> Vec<P<Node>>,
        source_file: P<SourceFile>,
    ) -> Vec<lsproto::DocumentHighlight> {
        if node_test(node) {
            return self.highlight_spans(&get_nodes(node, source_file), source_file);
        }
        Vec::new()
    }

    // Go `nodes` may contain nil entries (skipped); the port's lists never do.
    // documenthighlights.go:259
    fn highlight_spans(
        &self,
        nodes: &[P<Node>],
        source_file: P<SourceFile>,
    ) -> Vec<lsproto::DocumentHighlight> {
        if nodes.is_empty() {
            return Vec::new();
        }
        let mut highlights: Vec<lsproto::DocumentHighlight> = Vec::new();
        let kind = lsproto::DocumentHighlightKind::READ;
        highlights.extend(nodes.iter().filter_map(|&node| {
            let (lsp_range, fidelity) = self.create_lsp_range_from_node_for_feature(
                node,
                source_file,
                Feature::DocumentHighlights,
            );
            if fidelity.is_none() {
                None
            } else {
                Some(lsproto::DocumentHighlight { range: lsp_range, kind: Some(kind) })
            }
        }));
        highlights
    }

    // documenthighlights.go:275
    fn get_from_all_declarations(
        &self,
        node_test: &dyn Fn(P<Node>) -> bool,
        keywords: &[Kind],
        node: P<Node>,
        source_file: P<SourceFile>,
    ) -> Vec<lsproto::DocumentHighlight> {
        self.use_parent(
            node.parent().unwrap(),
            node_test,
            &mut |decl, _sf| {
                let mut symbol_decls: Vec<P<Node>> = Vec::new();
                if ast::can_have_symbol(decl)
                    && let Some(symbol) = decl.symbol()
                {
                    symbol_decls.extend(symbol.declarations().iter().filter_map(|&d| {
                        if !node_test(d) {
                            return None;
                        }
                        get_children_from_non_jsdoc_node(d, source_file)
                            .into_iter()
                            .find(|c| keywords.contains(&c.kind()))
                    }));
                }
                symbol_decls
            },
            source_file,
        )
    }

    // documenthighlights.go:299
    fn get_if_else_occurrences(
        &self,
        if_statement: P<Node>, /*IfStatement*/
        source_file: P<SourceFile>,
    ) -> Vec<lsproto::DocumentHighlight> {
        let keywords = get_if_else_keywords(if_statement, source_file);
        let kind = lsproto::DocumentHighlightKind::READ;
        let mut highlights: Vec<lsproto::DocumentHighlight> = Vec::new();

        // We'd like to highlight else/ifs together if they are only separated by whitespace
        // (i.e. the keywords are separated by no comments, no newlines).
        let mut i = 0;
        while i < keywords.len() {
            if keywords[i].kind() == Kind::ElseKeyword && i < keywords.len() - 1 {
                let else_keyword = keywords[i];
                let if_keyword = keywords[i + 1]; // this *should* always be an 'if' keyword.
                let mut should_combine = true;

                // Avoid recalculating getStart() by iterating backwards.
                let if_token_start = scanner::get_token_pos_of_node(if_keyword, source_file, false);
                let text = source_file.text().as_bytes();
                let else_end =
                    else_keyword.end().expect_source("parsed keywords have source positions");
                let mut j = if_token_start;
                while j > else_end {
                    j -= 1;
                    if !stringutil::is_white_space_single_line(u32::from(text[j as usize])) {
                        should_combine = false;
                        break;
                    }
                }
                if should_combine {
                    let (lsp_range, fidelity) = self.create_lsp_range_from_bounds(
                        scanner::skip_trivia(
                            source_file.text(),
                            else_keyword
                                .pos()
                                .expect_source("parsed keywords have source positions"),
                        ),
                        if_keyword.end().expect_source("parsed keywords have source positions"),
                        source_file,
                    );
                    if !fidelity.is_none() {
                        highlights.push(lsproto::DocumentHighlight {
                            range: lsp_range,
                            kind: Some(kind),
                        });
                    }
                    i += 2; // skip the next keyword
                    continue;
                }
            }
            // Ordinary case: just highlight the keyword.
            let (lsp_range, fidelity) = self.create_lsp_range_from_node_for_feature(
                keywords[i],
                source_file,
                Feature::DocumentHighlights,
            );
            if !fidelity.is_none() {
                highlights.push(lsproto::DocumentHighlight { range: lsp_range, kind: Some(kind) });
            }
            i += 1;
        }
        highlights
    }
}

// documenthighlights.go:340
fn get_if_else_keywords(
    if_statement: P<Node>, /*IfStatement*/
    source_file: P<SourceFile>,
) -> Vec<P<Node>> {
    let mut if_statement = if_statement;
    // We may be at an if statement like those in the range below:
    //
    //   ```
    //   if (...) {
    //   } else [|if (...) {}|]
    //   ````
    //
    // Traverse upwards through all parent if-statements linked by their else-branches.
    while ast::is_if_statement(if_statement.parent().unwrap()) {
        // See if the parent's `else` is actually the current `if` statement.
        let parenting_if = if_statement.parent().unwrap();
        let else_statement = parenting_if.as_if_statement().else_statement;
        if else_statement != Some(if_statement) {
            break;
        }
        if_statement = parenting_if;
    }

    let mut keywords: Vec<P<Node>> = Vec::new();

    // Traverse back down through the else branches, aggregating if/else keywords of if-statements.
    loop {
        let children = get_children_from_non_jsdoc_node(if_statement, source_file);
        if !children.is_empty() && children[0].kind() == Kind::IfKeyword {
            keywords.push(children[0]);
        }
        // Generally the 'else' keyword is second-to-last, so traverse backwards.
        if let Some(c) = children
            .iter()
            .rev()
            .into_iter()
            .find_map(|&c| if c.kind() == Kind::ElseKeyword { Some(c) } else { None })
        {
            keywords.push(c);
        }
        let else_statement = if_statement.as_if_statement().else_statement;
        match else_statement {
            Some(else_statement) if ast::is_if_statement(else_statement) => {
                if_statement = else_statement;
            }
            _ => break,
        }
    }
    keywords
}

// documenthighlights.go:383
fn get_return_occurrences(node: P<Node>, source_file: P<SourceFile>) -> Vec<P<Node>> {
    let Some(func_node) = ast::find_ancestor(node.parent(), |n| ast::is_function_like(Some(n)))
    else {
        return Vec::new();
    };

    let mut keywords: Vec<P<Node>> = Vec::new();
    let body = func_node.body();
    if let Some(body) = body {
        ast::for_each_return_statement(body, |ret: P<Node>| {
            let keyword = astnav::find_child_of_kind(ret, Kind::ReturnKeyword, source_file);
            if let Some(keyword) = keyword {
                keywords.push(keyword);
            }
            false // continue traversal
        });

        // Get all throw statements not in a try block
        let throw_statements = aggregate_owned_throw_statements(body, source_file);
        keywords.extend(throw_statements.into_iter().filter_map(|throw| {
            astnav::find_child_of_kind(throw, Kind::ThrowKeyword, source_file)
        }));
    }
    keywords
}

// documenthighlights.go:412
fn aggregate_owned_throw_statements(node: P<Node>, source_file: P<SourceFile>) -> Vec<P<Node>> {
    if ast::is_throw_statement(node) {
        return vec![node];
    }
    if ast::is_try_statement(node) {
        // Exceptions thrown within a try block lacking a catch clause are "owned" in the current context.
        let statement = node.as_try_statement();
        let try_block = statement.try_block;
        let catch_clause = statement.catch_clause;
        let finally_block = statement.finally_block;

        let mut result: Vec<P<Node>>;
        if let Some(catch_clause) = catch_clause {
            result = aggregate_owned_throw_statements(catch_clause, source_file);
        } else {
            result = aggregate_owned_throw_statements(try_block, source_file);
        }
        if let Some(finally_block) = finally_block {
            result.extend(aggregate_owned_throw_statements(finally_block, source_file));
        }
        return result;
    }
    // Do not cross function boundaries.
    if ast::is_function_like(Some(node)) {
        return Vec::new();
    }
    flat_map_children(node, source_file, aggregate_owned_throw_statements)
}

// documenthighlights.go:441
fn flat_map_children<T>(
    node: P<Node>,
    source_file: P<SourceFile>,
    cb: fn(P<Node>, P<SourceFile>) -> Vec<T>,
) -> Vec<T> {
    let mut result: Vec<T> = Vec::new();

    node.for_each_child(&mut |child| {
        let value = cb(child, source_file);
        result.extend(value);
        false // continue traversal
    });
    result
}

// documenthighlights.go:454
fn get_throw_occurrences(node: P<Node>, source_file: P<SourceFile>) -> Vec<P<Node>> {
    let Some(owner) = get_throw_statement_owner(node) else {
        return Vec::new();
    };

    let mut keywords: Vec<P<Node>> = Vec::new();

    // Aggregate all throw statements "owned" by this owner.
    let throw_statements = aggregate_owned_throw_statements(owner, source_file);
    keywords.extend(
        throw_statements
            .into_iter()
            .filter_map(|throw| astnav::find_child_of_kind(throw, Kind::ThrowKeyword, source_file)),
    );

    // If the "owner" is a function, then we equate 'return' and 'throw' statements in their
    // ability to "jump out" of the function, and include occurrences for both
    if ast::is_function_block(owner) {
        ast::for_each_return_statement(owner, |ret: P<Node>| {
            let keyword = astnav::find_child_of_kind(ret, Kind::ReturnKeyword, source_file);
            if let Some(keyword) = keyword {
                keywords.push(keyword);
            }
            false // continue traversal
        });
    }

    keywords
}

// For lack of a better name, this function takes a throw statement and returns the
// nearest ancestor that is a try-block (whose try statement has a catch clause),
// function-block, or source file.
// documenthighlights.go:489
fn get_throw_statement_owner(throw_statement: P<Node>) -> Option<P<Node>> {
    let mut child = throw_statement;
    while let Some(parent) = child.parent() {
        if ast::is_function_block(parent) || parent.kind() == Kind::SourceFile {
            return Some(parent);
        }

        // A throw-statement is only owned by a try-statement if the try-statement has
        // a catch clause, and if the throw-statement occurs within the try block.
        if ast::is_try_statement(parent) {
            let try_statement = parent.as_try_statement();
            if try_statement.try_block == child && try_statement.catch_clause.is_some() {
                return Some(child);
            }
        }

        child = parent;
    }
    None
}

// documenthighlights.go:512
fn get_try_catch_finally_occurrences(node: P<Node>, source_file: P<SourceFile>) -> Vec<P<Node>> {
    let try_statement = node.as_try_statement();

    let mut keywords: Vec<P<Node>> = Vec::new();
    let token = lsutil::get_first_token(node, source_file);
    if let Some(token) = token
        && token.kind() == Kind::TryKeyword
    {
        keywords.push(token);
    }

    if try_statement.catch_clause.is_some()
        && let Some(catch_token) = astnav::find_child_of_kind(node, Kind::CatchKeyword, source_file)
    {
        keywords.push(catch_token);
    }

    if try_statement.finally_block.is_some()
        && let Some(finally_keyword) =
            astnav::find_child_of_kind(node, Kind::FinallyKeyword, source_file)
    {
        keywords.push(finally_keyword);
    }

    keywords
}

// documenthighlights.go:536
fn get_switch_case_default_occurrences(node: P<Node>, source_file: P<SourceFile>) -> Vec<P<Node>> {
    let switch_statement = node.as_switch_statement();

    let mut keywords: Vec<P<Node>> = Vec::new();
    let token = lsutil::get_first_token(node, source_file).unwrap();
    if token.kind() == Kind::SwitchKeyword {
        keywords.push(token);
    }

    let clauses = switch_statement.case_block.as_case_block().clauses;
    for &clause in clauses.nodes() {
        let clause_token = lsutil::get_first_token(clause, source_file).unwrap();
        if clause_token.kind() == Kind::CaseKeyword || clause_token.kind() == Kind::DefaultKeyword {
            keywords.push(clause_token);
        }

        let break_and_continue_statements =
            aggregate_all_break_and_continue_statements(clause, source_file);
        keywords.extend(break_and_continue_statements.into_iter().filter_map(|statement| {
            if statement.kind() == Kind::BreakStatement
                && owns_break_or_continue_statement(node, statement)
            {
                Some(lsutil::get_first_token(statement, source_file).unwrap())
            } else {
                None
            }
        }));
    }

    keywords
}

// documenthighlights.go:563
fn aggregate_all_break_and_continue_statements(
    node: P<Node>,
    source_file: P<SourceFile>,
) -> Vec<P<Node>> {
    if ast::is_break_or_continue_statement(node) {
        return vec![node];
    }
    if ast::is_function_like(Some(node)) {
        return Vec::new();
    }
    flat_map_children(node, source_file, aggregate_all_break_and_continue_statements)
}

// documenthighlights.go:573
fn owns_break_or_continue_statement(owner: P<Node>, statement: P<Node>) -> bool {
    let Some(actual_owner) = get_break_or_continue_owner(statement) else {
        return false;
    };
    actual_owner == owner
}

// documenthighlights.go:581
fn get_break_or_continue_owner(statement: P<Node>) -> Option<P<Node>> {
    ast::find_ancestor_or_quit(Some(statement), |node| {
        let labeled_or_owner = |node: P<Node>| -> FindAncestorResult {
            // If the statement is labeled, check if the node is labeled by the statement's label.
            match statement.label() {
                None => FindAncestorResult::True,
                Some(label) if is_labeled_by(node, label.text()) => FindAncestorResult::True,
                Some(_) => FindAncestorResult::False,
            }
        };
        match node.kind() {
            Kind::SwitchStatement => {
                if statement.kind() == Kind::ContinueStatement {
                    return FindAncestorResult::False;
                }
                labeled_or_owner(node)
            }
            Kind::ForStatement
            | Kind::ForInStatement
            | Kind::ForOfStatement
            | Kind::WhileStatement
            | Kind::DoStatement => labeled_or_owner(node),
            _ => {
                // Don't cross function boundaries.
                if ast::is_function_like(Some(node)) {
                    return FindAncestorResult::Quit;
                }
                FindAncestorResult::False
            }
        }
    })
}

// Whether or not a 'node' is preceded by a label of the given string.
// Note: 'node' cannot be a SourceFile.
// documenthighlights.go:611
fn is_labeled_by(node: P<Node>, label_name: &str) -> bool {
    ast::find_ancestor_or_quit(node.parent(), |owner| {
        if !ast::is_labeled_statement(owner) {
            return FindAncestorResult::Quit;
        }
        if owner.label().unwrap().text() == label_name {
            return FindAncestorResult::True;
        }
        FindAncestorResult::False
    })
    .is_some()
}

// documenthighlights.go:623
fn get_break_or_continue_statement_occurrences(
    node: P<Node>,
    source_file: P<SourceFile>,
) -> Vec<P<Node>> {
    if let Some(owner) = get_break_or_continue_owner(node) {
        match owner.kind() {
            Kind::ForStatement
            | Kind::ForInStatement
            | Kind::ForOfStatement
            | Kind::DoStatement
            | Kind::WhileStatement => {
                return get_loop_break_continue_occurrences(owner, source_file);
            }
            Kind::SwitchStatement => {
                return get_switch_case_default_occurrences(owner, source_file);
            }
            _ => {}
        }
    }
    Vec::new()
}

// documenthighlights.go:635
fn get_loop_break_continue_occurrences(node: P<Node>, source_file: P<SourceFile>) -> Vec<P<Node>> {
    let mut keywords: Vec<P<Node>> = Vec::new();

    let token = lsutil::get_first_token(node, source_file).unwrap();
    if token.kind() == Kind::ForKeyword
        || token.kind() == Kind::DoKeyword
        || token.kind() == Kind::WhileKeyword
    {
        keywords.push(token);
        if node.kind() == Kind::DoStatement {
            let loop_tokens = get_children_from_non_jsdoc_node(node, source_file);
            if let Some(loop_token) = loop_tokens.iter().rev().into_iter().find_map(|&loop_token| {
                if loop_token.kind() == Kind::WhileKeyword { Some(loop_token) } else { None }
            }) {
                keywords.push(loop_token);
            }
        }
    }

    let break_and_continue_statements =
        aggregate_all_break_and_continue_statements(node, source_file);
    keywords.extend(break_and_continue_statements.into_iter().filter_map(|statement| {
        let token = lsutil::get_first_token(statement, source_file).unwrap();
        if owns_break_or_continue_statement(node, statement)
            && (token.kind() == Kind::BreakKeyword || token.kind() == Kind::ContinueKeyword)
        {
            Some(token)
        } else {
            None
        }
    }));

    keywords
}

// documenthighlights.go:663
fn get_async_and_await_occurrences(node: P<Node>, source_file: P<SourceFile>) -> Vec<P<Node>> {
    let Some(fun) = ast::get_containing_function(node) else {
        return Vec::new();
    };

    let mut keywords: Vec<P<Node>> = Vec::new();

    keywords.extend(fun.modifier_nodes().iter().filter_map(|&modifier| {
        if modifier.kind() == Kind::AsyncKeyword { Some(modifier) } else { None }
    }));

    fun.for_each_child(&mut |child| {
        traverse_without_crossing_function(child, source_file, &mut |child| {
            if ast::is_await_expression(child) {
                let token = lsutil::get_first_token(child, source_file).unwrap();
                if token.kind() == Kind::AwaitKeyword {
                    keywords.push(token);
                }
            }
        });
        false // continue traversal
    });

    keywords
}

// documenthighlights.go:692
fn get_yield_occurrences(node: P<Node>, source_file: P<SourceFile>) -> Vec<P<Node>> {
    let Some(parent_func) = ast::find_ancestor(node.parent(), |n| ast::is_function_like(Some(n)))
    else {
        return Vec::new();
    };

    let mut keywords: Vec<P<Node>> = Vec::new();

    parent_func.for_each_child(&mut |child| {
        traverse_without_crossing_function(child, source_file, &mut |child| {
            if ast::is_yield_expression(child) {
                let token = lsutil::get_first_token(child, source_file).unwrap();
                if token.kind() == Kind::YieldKeyword {
                    keywords.push(token);
                }
            }
        });
        false // continue traversal
    });

    keywords
}

// documenthighlights.go:715
#[expect(
    clippy::only_used_in_recursion,
    reason = "The pinned Go function also carries these parameters through recursion without otherwise reading them."
)]
fn traverse_without_crossing_function(
    node: P<Node>,
    source_file: P<SourceFile>,
    cb: &mut dyn FnMut(P<Node>),
) {
    cb(node);
    if !ast::is_function_like(Some(node))
        && !ast::is_class_like(node)
        && !ast::is_interface_declaration(node)
        && !ast::is_module_declaration(node)
        && !ast::is_type_alias_declaration(node)
        && !ast::is_type_node(node)
    {
        node.for_each_child(&mut |child| {
            traverse_without_crossing_function(child, source_file, cb);
            false // continue traversal
        });
    }
}

// documenthighlights.go:725
fn get_modifier_occurrences(
    kind: Kind,
    node: P<Node>,
    _source_file: P<SourceFile>,
) -> Vec<P<Node>> {
    let mut result: Vec<P<Node>> = Vec::new();

    let nodes_to_search = get_nodes_to_search_for_modifier(node, ast::modifier_to_flag(kind));
    result.extend(nodes_to_search.into_iter().filter_map(|node| find_modifier(node, kind)));
    result
}

// documenthighlights.go:738
fn get_nodes_to_search_for_modifier(
    declaration: P<Node>,
    modifier_flag: ModifierFlags,
) -> Vec<P<Node>> {
    let mut result: Vec<P<Node>> = Vec::new();

    let Some(container) = declaration.parent() else {
        return Vec::new();
    };

    // Types of node whose children might have modifiers.
    match container.kind() {
        Kind::ModuleBlock
        | Kind::SourceFile
        | Kind::Block
        | Kind::CaseClause
        | Kind::DefaultClause => {
            // Container is either a class declaration or the declaration is a classDeclaration
            if modifier_flag.intersects(ModifierFlags::Abstract)
                && ast::is_class_declaration(declaration)
            {
                result.extend_from_slice(declaration.members());
                result.push(declaration);
                result
            } else {
                result.extend_from_slice(container.statements());
                result
            }
        }
        Kind::Constructor | Kind::MethodDeclaration | Kind::FunctionDeclaration => {
            // Parameters and, if inside a class, also class members
            result.extend_from_slice(container.parameters());
            if ast::is_class_like(container.parent().unwrap()) {
                result.extend_from_slice(container.parent().unwrap().members());
            }
            result
        }
        Kind::ClassDeclaration
        | Kind::ClassExpression
        | Kind::InterfaceDeclaration
        | Kind::TypeLiteral => {
            let nodes = container.members();
            result.extend_from_slice(nodes);
            // If we're an accessibility modifier, we're in an instance member and should search
            // the constructor's parameter list for instance members as well.
            if modifier_flag
                .intersects(ModifierFlags::AccessibilityModifier | ModifierFlags::Readonly)
            {
                let mut constructor: Option<P<Node>> = None;

                if let Some(member) = nodes.iter().find_map(|&member| {
                    if ast::is_constructor_declaration(member) { Some(member) } else { None }
                }) {
                    constructor = Some(member);
                }
                if let Some(constructor) = constructor {
                    result.extend_from_slice(constructor.parameters());
                }
            } else if modifier_flag.intersects(ModifierFlags::Abstract) {
                result.push(container);
            }
            result
        }
        _ => {
            // Syntactically invalid positions or unsupported containers
            Vec::new()
        }
    }
}

// documenthighlights.go:789
fn find_modifier(node: P<Node>, kind: Kind) -> Option<P<Node>> {
    node.modifier_nodes().iter().copied().find(|modifier| modifier.kind() == kind)
}
